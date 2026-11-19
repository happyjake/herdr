use std::io::{BufRead, BufReader, Cursor};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{json, Value};

use super::*;

struct PictureServer {
    server: Option<WsTestServer>,
    pane: String,
}

impl PictureServer {
    fn start() -> Self {
        let server = start_ws_test_server();
        let created = unix_request(
            &server.socket_path,
            &wire_request("workspace.create", json!({"cwd":server.base,"focus":true})),
        );
        let pane = created["result"]["root_pane"]["pane_id"]
            .as_str()
            .unwrap()
            .to_owned();
        Self {
            server: Some(server),
            pane,
        }
    }

    fn request(&self, method: &str, params: Value) -> Value {
        unix_request(
            &self.server.as_ref().unwrap().socket_path,
            &wire_request(method, params),
        )
    }

    fn read(&self, source: &str, extra: Value) -> Value {
        let mut params = json!({"pane_id":self.pane,"format":"ansi","source":source});
        params
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let response = self.request("pane.read", params);
        assert_eq!(response["result"]["type"], "pane_read", "{response}");
        response["result"]["read"].clone()
    }

    fn fetch(&self, fingerprint: &str, width_px: u32) -> Value {
        self.request("pane.picture", self.fetch_params(fingerprint, width_px))
    }

    fn fetch_params(&self, fingerprint: &str, width_px: u32) -> Value {
        json!({"pane_id":self.pane,"fingerprint":fingerprint,"width_px":width_px})
    }

    fn run_stages(&self, stages: &[String]) {
        let mut command = Vec::new();
        for (index, bytes) in stages.iter().enumerate() {
            let name = format!("picture-stage-{index}.pty");
            fs::write(self.server.as_ref().unwrap().base.join(&name), bytes).unwrap();
            command.push(format!("cat ./{name}; read ignored"));
        }
        let sent = self.request(
            "pane.send_text",
            json!({"pane_id":self.pane,"text":command.join("; ")}),
        );
        assert_eq!(sent["result"]["type"], "ok", "{sent}");
        self.advance();
    }

    fn advance(&self) {
        let sent = self.request(
            "pane.send_keys",
            json!({"pane_id":self.pane,"keys":["Enter"]}),
        );
        assert_eq!(sent["result"]["type"], "ok", "{sent}");
    }

    fn wait(&self, marker: &str) {
        let waited = self.request(
            "pane.wait_for_output",
            json!({
                "pane_id":self.pane,"source":"recent","lines":40,
                "match":{"type":"substring","value":marker},"timeout_ms":4000
            }),
        );
        assert_eq!(waited["result"]["type"], "output_matched", "{waited}");
    }
}

impl Drop for PictureServer {
    fn drop(&mut self) {
        if let Some(mut server) = self.server.take() {
            support::stop_spawned_herdr(&mut *server.child.child);
            cleanup_spawned_herdr(server.child, server.base);
        }
    }
}

fn wire_request(method: &str, params: Value) -> String {
    json!({"id":"picture-test","method":method,"params":params}).to_string()
}

fn kitty_data(controls: &str, data: &[u8]) -> String {
    let encoded = STANDARD.encode(data);
    let chunks = encoded.as_bytes().chunks(4096);
    let count = chunks.len();
    let mut stream = String::new();
    for (index, chunk) in chunks.enumerate() {
        let controls = if index == 0 { controls } else { "" };
        let more = u8::from(index + 1 != count);
        let chunk = std::str::from_utf8(chunk).unwrap();
        let comma = if controls.is_empty() { "" } else { "," };
        stream.push_str(&format!("\x1b_G{controls}{comma}m={more};{chunk}\x1b\\"));
    }
    stream
}

fn opaque_png() -> Vec<u8> {
    let mut png = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png, 100, 50);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&vec![127; 100 * 50 * 3])
            .unwrap();
    }
    png
}

fn image_bytes(response: &Value) -> Vec<u8> {
    STANDARD
        .decode(
            response["result"]["data_b64"]
                .as_str()
                .expect(&response.to_string()),
        )
        .unwrap()
}

fn jpeg_dimensions(data: &[u8]) -> [u32; 2] {
    assert!(data.starts_with(&[0xff, 0xd8]) && data.ends_with(&[0xff, 0xd9]));
    let mut index = 2;
    while let Some([0xff, marker, high, low]) = data
        .get(index..index + 4)
        .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
    {
        let len = usize::from(u16::from_be_bytes([high, low]));
        if marker == 0xc0 {
            let frame = &data[index + 5..index + 9];
            return [
                u32::from(u16::from_be_bytes([frame[2], frame[3]])),
                u32::from(u16::from_be_bytes([frame[0], frame[1]])),
            ];
        }
        index += 2 + len;
    }
    panic!("missing baseline JPEG frame header");
}

#[test]
fn pane_read_lists_direct_kitty_pictures() {
    let _lock = test_lock();
    let server = PictureServer::start();
    let empty = server.read("visible", json!({}));
    assert!(empty.get("pictures").is_none() && empty.get("cell_px").is_none());

    let picture = kitty_data("a=T,f=100,q=2,C=1,i=12,c=10,r=4", &opaque_png());
    let contents = "t".repeat(75);
    let draw = format!(
        "\x1b[2J\x1b[Hfirst\x1b[2;4H{picture}\x1b[6;1H{contents}\r\n{contents}\r\n{contents}\r\n{contents}\r\npicture-ready"
    );
    let mut scroll = "\r\n".to_owned();
    for index in 0..125 {
        scroll.push_str(&format!("history {index}\r\n"));
    }
    scroll.push_str("history-ready");
    server.run_stages(&[draw, scroll]);
    server.wait("picture-ready");

    let visible = server.read("visible", json!({}));
    let fingerprint = visible["pictures"][0]["fingerprint"]
        .as_str()
        .expect(&visible.to_string());
    assert_eq!(fingerprint.len(), 16);
    assert!(fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit()));
    let expected = json!({
        "fingerprint":fingerprint,"image_px":[100,50],"line":1,"rows_above":0,"col":3,
        "cols":10,"rows":4,"crop":[0,0,100,50],"z":0,"opaque":true,"kind":"direct"
    });
    for source in ["visible", "recent", "recent_unwrapped"] {
        let read = server.read(source, json!({}));
        assert_eq!(read["pictures"], json!([expected]), "{source}: {read}");
        assert_eq!(read["cell_px"], json!([8, 16]));
    }
    assert_eq!(
        server.request("ping", json!({}))["result"]["capabilities"]["pane_pictures"],
        true
    );
    let clipped = server.read("visible", json!({"lines":7}));
    let mut clip = expected.clone();
    clip["line"] = json!(0);
    clip["rows_above"] = json!(2);
    assert_eq!(clipped["pictures"], json!([clip]), "{clipped}");

    let full = server.read("recent_unwrapped", json!({"lines":10,"continuity":true}));
    let token = full["continuity"]["token"]
        .as_str()
        .expect(&full.to_string());
    let continued = server.read("recent_unwrapped", json!({"lines":9,"continue_from":token}));
    assert_eq!(continued["text"], "");
    assert_eq!(continued["continued"]["before"], "");
    assert_eq!(continued["continued"]["after"], "");
    let mut at_top = expected.clone();
    at_top["line"] = json!(0);
    assert_eq!(continued["pictures"], json!([at_top]));
    assert_eq!(continued["cell_px"], full["cell_px"]);

    let fetch = server.fetch(fingerprint, 33);
    assert_eq!(fetch["result"]["media_type"], "image/jpeg", "{fetch}");
    assert_eq!(fetch["result"]["px"], json!([64, 32]));
    assert_eq!(jpeg_dimensions(&image_bytes(&fetch)), [64, 32]);
    let native = server.fetch(fingerprint, u32::MAX);
    assert_eq!(native["result"]["px"], json!([100, 50]));

    server.advance();
    server.wait("history-ready");
    assert!(server.read("visible", json!({})).get("pictures").is_none());
    for source in ["recent", "recent_unwrapped"] {
        let paged = server.read(source, json!({"lines":8,"offset_from_bottom":10000}));
        assert!(paged["effective_offset"].as_u64().unwrap() > 100, "{paged}");
        assert_eq!(paged["pictures"], json!([expected]), "{source}: {paged}");
    }
}

#[test]
fn pane_picture_fetches_alpha_and_checks_current_storage() {
    let _lock = test_lock();
    let server = PictureServer::start();
    let alternating = [240u8, 40, 10, 255, 0, 80, 240, 0].repeat(64 * 64);
    let image = kitty_data(
        "a=T,f=32,s=128,v=64,q=2,C=1,i=13,c=4,r=3,z=-2",
        &alternating,
    );
    server.run_stages(&[
        format!("\x1b[2J\x1b[Halpha\x1b[2;2H{image}\x1b[7;1Halpha-ready"),
        "\x1b[?1049halternate-ready".into(),
        "\x1b[?1049l\x1b_Ga=d,d=I,i=13,q=2;\x1b\\\x1b[12;1Hdeleted-ready".into(),
    ]);
    server.wait("alpha-ready");
    let read = server.read("recent", json!({}));
    let picture = &read["pictures"][0];
    let fingerprint = picture["fingerprint"].as_str().expect(&read.to_string());
    assert_eq!(picture["opaque"], false);
    assert_eq!(picture["z"], -2);
    assert_eq!(picture["line"], 1);
    assert_eq!(picture["col"], 1);

    let fetch = server.fetch(fingerprint, 1);
    assert_eq!(fetch["result"]["media_type"], "image/png", "{fetch}");
    assert_eq!(fetch["result"]["px"], json!([64, 32]));
    let bytes = image_bytes(&fetch);
    let mut png = png::Decoder::new(Cursor::new(bytes)).read_info().unwrap();
    let mut pixels = vec![0; png.output_buffer_size()];
    let info = png.next_frame(&mut pixels).unwrap();
    assert_eq!([info.width, info.height], [64, 32]);
    assert!(pixels.chunks_exact(4).all(|p| p == [240, 40, 10, 128]));
    assert_eq!(server.fetch(fingerprint, 64), fetch);
    assert_eq!(
        server.fetch("0000000000000000", 64)["error"]["code"],
        "picture_not_held"
    );

    let another = server.request(
        "workspace.create",
        json!({"cwd":server.server.as_ref().unwrap().base}),
    );
    let another_pane = another["result"]["root_pane"]["pane_id"].as_str().unwrap();
    let missing = server.request(
        "pane.picture",
        json!({"pane_id":another_pane,"fingerprint":fingerprint,"width_px":64}),
    );
    assert_eq!(missing["error"]["code"], "picture_not_held");

    server.advance();
    server.wait("alternate-ready");
    let alternate = server.read("visible", json!({}));
    assert_eq!(alternate["alternate_screen"], true);
    assert!(alternate.get("pictures").is_none());
    assert_eq!(server.fetch(fingerprint, 64), fetch);

    server.advance();
    server.wait("deleted-ready");
    assert!(server.read("visible", json!({})).get("pictures").is_none());
    assert_eq!(
        server.fetch(fingerprint, 64)["error"]["code"],
        "picture_not_held"
    );
}

#[test]
fn pane_picture_caps_reply_size_and_longest_edge() {
    let _lock = test_lock();
    let server = PictureServer::start();
    let mut random = 0x1234_5678u32;
    let rgba: Vec<u8> = (0..1024 * 1024)
        .flat_map(|_| {
            random ^= random << 13;
            random ^= random >> 17;
            random ^= random << 5;
            random.to_le_bytes()
        })
        .collect();
    let noisy = kitty_data("a=T,f=32,s=1024,v=1024,q=2,C=1,i=14,c=4,r=2", &rgba);
    let tall = kitty_data(
        "a=T,f=24,s=1,v=8200,q=2,C=1,i=15,c=4,r=2",
        &vec![80; 8200 * 3],
    );
    server.run_stages(&[format!(
        "\x1b[2J\x1b[Hnoisy{noisy}\x1b[4;1H{tall}\x1b[8;1Hsizes-ready"
    )]);
    server.wait("sizes-ready");

    let read = server.read("visible", json!({}));
    let pictures = read["pictures"].as_array().expect(&read.to_string());
    assert_eq!(pictures.len(), 2);
    let noisy_fp = pictures[0]["fingerprint"].as_str().unwrap();
    let tall_fp = pictures[1]["fingerprint"].as_str().unwrap();

    // A separate socket can read the pane while the large cache miss is in flight.
    let path = &server.server.as_ref().unwrap().socket_path;
    let heavy = UnixStream::connect(path).unwrap();
    heavy
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut heavy = BufReader::new(heavy);
    writeln!(
        heavy.get_mut(),
        "{}",
        wire_request("pane.picture", server.fetch_params(noisy_fp, 1024))
    )
    .unwrap();
    let (completed, first) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let large_done = completed.clone();
        let large = scope.spawn(move || {
            let mut reply = String::new();
            heavy.read_line(&mut reply).unwrap();
            large_done.send("picture").unwrap();
            serde_json::from_str::<Value>(&reply).unwrap()
        });
        let small = server.read("visible", json!({}));
        completed.send("read").unwrap();
        assert_eq!(first.recv_timeout(Duration::from_secs(5)).unwrap(), "read");
        assert_eq!(small["pictures"], read["pictures"]);
        let noisy = large.join().unwrap();
        assert_eq!(noisy["result"]["media_type"], "image/png");
        assert_eq!(noisy["result"]["px"], json!([256, 256]));
        assert!(image_bytes(&noisy).len() <= 768 * 1024);
    });

    let tall = server.fetch(tall_fp, 2048);
    assert_eq!(tall["result"]["px"], json!([1, 2050]));
    assert_eq!(jpeg_dimensions(&image_bytes(&tall)), [1, 2050]);
}

#[test]
fn pane_read_unwrapped_omits_wrapped_placements_and_remaps_later_lines() {
    let _lock = test_lock();
    let server = PictureServer::start();
    let cols = server.read("visible", json!({}))["cols"].as_u64().unwrap() as usize;
    let png = opaque_png();
    let wrapped = kitty_data("a=T,f=100,q=2,C=1,i=21,c=2,r=1", &png);
    let plain = kitty_data("a=T,f=100,q=2,C=1,i=22,c=2,r=1", &png);
    let picture_only = kitty_data("a=T,f=100,q=2,C=1,i=23,c=2,r=2", &png);
    server.run_stages(&[
        format!(
            "\x1b[2J\x1b[H{}\x1b[2;2H{wrapped}\x1b[4;2H{plain}\x1b[5;1Hwrapped-ready",
            "x".repeat(cols + 12)
        ),
        format!("\x1b[2J\x1b[5;2H{picture_only}\x1b[H"),
    ]);
    server.wait("wrapped-ready");

    let wrapped = server.read("recent", json!({}));
    let pictures = wrapped["pictures"].as_array().expect(&wrapped.to_string());
    assert_eq!(pictures.len(), 2);
    assert_eq!(pictures[0]["line"], 1);
    assert_eq!(pictures[1]["line"], 3);

    let unwrapped = server.read("recent_unwrapped", json!({}));
    let mut expected = pictures[1].clone();
    expected["line"] = json!(2);
    assert_eq!(unwrapped["pictures"], json!([expected]), "{unwrapped}");

    server.advance();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let read = server.read("recent_unwrapped", json!({"offset_from_bottom":0}));
        if read["text"] == "" {
            assert_eq!(read["pictures"][0]["line"], 4, "{read}");
            assert_eq!(read["pictures"][0]["rows"], 2, "{read}");
            break;
        }
        assert!(Instant::now() < deadline, "{read}");
    }
}
