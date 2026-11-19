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
        Self::start_at(unique_test_dir(), &[])
    }

    fn start_at(base: PathBuf, extra_env: &[(&str, &str)]) -> Self {
        // Use system utilities only. The production startup can look for
        // optional agent executables, including its model catalog lister.
        let mut env = vec![("PATH", "/usr/bin:/bin")];
        env.extend_from_slice(extra_env);
        let server = start_ws_test_server_at(Path::new(env!("CARGO_BIN_EXE_herdr")), base, &env);
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

    fn send_fetch(&self, fingerprint: &str, width_px: u32) -> BufReader<UnixStream> {
        let path = &self.server.as_ref().unwrap().socket_path;
        let mut stream = UnixStream::connect(path).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(8)))
            .unwrap();
        writeln!(
            stream,
            "{}",
            wire_request("pane.picture", self.fetch_params(fingerprint, width_px))
        )
        .unwrap();
        BufReader::new(stream)
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
                .unwrap_or_else(|| panic!("{response}")),
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

fn read_fetch(mut reader: BufReader<UnixStream>) -> Value {
    let mut response = String::new();
    reader.read_line(&mut response).unwrap();
    serde_json::from_str(&response).unwrap()
}

#[cfg(debug_assertions)]
#[test]
fn pane_picture_bounds_admission_and_leaves_other_sockets_responsive() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let barrier = base.join("encode-barrier");
    fs::create_dir_all(&barrier).unwrap();
    let server = PictureServer::start_at(
        base,
        &[(
            "HERDR_TEST_PICTURE_ENCODE_BARRIER",
            barrier.to_str().unwrap(),
        )],
    );
    let image = kitty_data("a=T,f=100,q=2,C=1,i=24,c=4,r=2", &opaque_png());
    server.run_stages(&[format!("\x1b[2J\x1b[H{image}encode-ready")]);
    server.wait("encode-ready");
    let read = server.read("visible", json!({}));
    let fingerprint = read["pictures"][0]["fingerprint"].as_str().unwrap();
    let first = server.send_fetch(fingerprint, 64);
    let second = server.send_fetch(fingerprint, 128);

    let deadline = Instant::now() + Duration::from_secs(5);
    for entry in ["entered-0", "entered-1"] {
        while !barrier.join(entry).is_file() {
            assert!(Instant::now() < deadline, "encoder did not reach {entry}");
            thread::sleep(Duration::from_millis(5));
        }
    }
    let small = server.read("visible", json!({}));
    assert_eq!(small["pictures"], read["pictures"]);
    assert_eq!(
        server.fetch(fingerprint, 192)["error"]["code"],
        "picture_busy"
    );
    fs::write(barrier.join("release"), []).unwrap();
    assert_eq!(read_fetch(first)["result"]["px"], json!([64, 32]));
    let native = read_fetch(second);
    assert_eq!(native["result"]["px"], json!([100, 50]));
    assert_eq!(server.fetch(fingerprint, 2048), native);
    assert!(!barrier.join("entered-2").exists());
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
        .unwrap_or_else(|| panic!("{visible}"));
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
        .unwrap_or_else(|| panic!("{full}"));
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
    let fingerprint = picture["fingerprint"]
        .as_str()
        .unwrap_or_else(|| panic!("{read}"));
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
    let pictures = read["pictures"]
        .as_array()
        .unwrap_or_else(|| panic!("{read}"));
    assert_eq!(pictures.len(), 2);
    let noisy_fp = pictures[0]["fingerprint"].as_str().unwrap();
    let tall_fp = pictures[1]["fingerprint"].as_str().unwrap();

    let noisy = read_fetch(server.send_fetch(noisy_fp, 1024));
    assert_eq!(noisy["result"]["media_type"], "image/png");
    assert_eq!(noisy["result"]["px"], json!([256, 256]));
    assert!(image_bytes(&noisy).len() <= 768 * 1024);

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
    let pictures = wrapped["pictures"]
        .as_array()
        .unwrap_or_else(|| panic!("{wrapped}"));
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
            let clamped = server.read("recent_unwrapped", json!({"offset_from_bottom":1}));
            assert_eq!(clamped["effective_offset"], 0);
            assert_eq!(clamped["text"], read["text"]);
            assert_eq!(clamped["pictures"], read["pictures"]);
            break;
        }
        assert!(Instant::now() < deadline, "{read}");
    }
}
#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn pane_picture_keeps_headless_cell_geometry_across_handoff() {
    let _lock = test_lock();
    let server = PictureServer::start();

    // Before any resize/handoff, the root pane uses the 8x16 headless cell size:
    // a 100x50 PNG placed with c=10 (pi's column-sized a=T,f=100 style) is
    // 80px wide -> 40px tall -> ceil(40 / 16) = 3 rows.
    let before_pic = kitty_data("a=T,f=100,q=2,C=1,i=40,c=10", &opaque_png());
    let after_pic = kitty_data("a=T,f=100,q=2,C=1,i=41,c=10", &opaque_png());
    server.run_stages(&[
        format!("\x1b[2J\x1b[H{before_pic}\x1b[6;1Hbefore-handoff"),
        format!("\x1b[2J\x1b[H{after_pic}\x1b[12;1Hafter-handoff"),
    ]);
    server.wait("before-handoff");
    let before = server.read("visible", json!({}));
    assert_eq!(before["cell_px"], json!([8, 16]));
    assert_eq!(before["pictures"][0]["rows"], json!(3));

    // Perform a live handoff (e.g. `herdr update`) on the headless server.
    let handoff = server.request("server.live_handoff", json!({}));
    assert_eq!(handoff["result"]["type"], "ok", "{handoff}");

    server.advance();
    server.wait("after-handoff");
    let after = server.read("visible", json!({}));
    let _ = server.request("server.stop", json!({}));

    assert_eq!(after["cell_px"], json!([8, 16]), "after handoff: {after}");
    assert_eq!(
        after["pictures"][0]["rows"],
        json!(3),
        "after handoff: {after}"
    );
}

#[test]
fn pane_read_reports_resolved_picture_source_crop() {
    let _lock = test_lock();
    let server = PictureServer::start();

    // Place a 50x25 crop starting at (20, 10) of a 100x50 image.
    let cropped = kitty_data(
        "a=T,f=100,q=2,C=1,i=42,x=20,y=10,w=50,h=25,c=10,r=4",
        &opaque_png(),
    );
    server.run_stages(&[format!("\x1b[2J\x1b[H{cropped}\x1b[6;1Hcrop-ready")]);
    server.wait("crop-ready");

    let read = server.read("visible", json!({}));
    assert_eq!(
        read["pictures"][0]["crop"],
        json!([20, 10, 50, 25]),
        "expected placement source crop, got whole image: {read}"
    );
}

#[test]
fn pane_read_unwrapped_uses_the_formatters_break_before_a_wide_character() {
    let _lock = test_lock();
    let server = PictureServer::start();
    let cols = server.read("visible", json!({}))["cols"].as_u64().unwrap();
    let image = kitty_data("a=T,f=100,q=2,C=1,i=25,c=2,r=1", &opaque_png());
    server.run_stages(&[format!(
        "\x1b[2J\x1b[1;{cols}H界\x1b[3;1H{image}wide-character-ready"
    )]);
    server.wait("wide-character-ready");
    let read = server.read("recent_unwrapped", json!({}));
    assert_eq!(read["text"], "\r\n界\r\nwide-character-ready");
    assert_eq!(read["pictures"][0]["line"], 2, "{read}");
}

#[test]
fn pane_read_visible_line_limit_keeps_a_picture_in_the_trailing_blank_rows() {
    let _lock = test_lock();
    let server = PictureServer::start();
    let image = kitty_data("a=T,f=100,q=2,C=1,i=26,c=3,r=2", &opaque_png());
    server.run_stages(&[format!(
        "\x1b[2J\x1b[Hfirst\r\nlast-visible-line\x1b[5;2H{image}"
    )]);
    server.wait("last-visible-line");
    let read = server.read("visible", json!({"lines":1}));
    assert_eq!(read["text"], "last-visible-line");
    assert_eq!(read["pictures"][0]["line"], 3, "{read}");
    assert_eq!(read["pictures"][0]["rows"], 2);
}

#[test]
fn pane_picture_fingerprint_is_stable_across_file_and_inline_uploads() {
    let _lock = test_lock();
    let server = PictureServer::start();
    let root = server.server.as_ref().unwrap().base.clone();
    let bytes = [19u8, 42, 73, 255].repeat(1024);
    let mut stages = Vec::new();
    for (index, name) in ["first.rgba", "second.rgba"].into_iter().enumerate() {
        let path = root.join(name);
        fs::write(&path, &bytes).unwrap();
        let image = kitty_data(
            "a=T,t=f,f=32,s=32,v=32,q=2,C=1,i=27,c=3,r=2",
            path.to_str().unwrap().as_bytes(),
        );
        stages.push(format!("\x1b[2J\x1b[H{image}source-{index}"));
    }
    let inline = kitty_data("a=T,f=32,s=32,v=32,q=2,C=1,i=27,c=3,r=2", &bytes);
    stages.push(format!("\x1b[2J\x1b[H{inline}source-2"));
    server.run_stages(&stages);

    server.wait("source-0");
    let first = server.read("visible", json!({}));
    let fingerprint = first["pictures"][0]["fingerprint"].as_str().unwrap();
    assert_eq!(
        server.fetch(fingerprint, 64)["result"]["media_type"],
        "image/jpeg"
    );
    for index in 1..=2 {
        server.advance();
        server.wait(&format!("source-{index}"));
        let resent = server.read("visible", json!({}));
        assert_eq!(resent["pictures"], first["pictures"]);
        assert_eq!(
            server.fetch(fingerprint, 64)["result"]["px"],
            json!([32, 32])
        );
    }
}

#[test]
fn pane_read_reports_picture_crop_after_scrolling_inside_margins() {
    let _lock = test_lock();
    let server = PictureServer::start();
    let picture = kitty_data("a=T,f=100,q=2,C=1,i=28,c=10,r=5", &opaque_png());
    server.run_stages(&[format!(
        "\x1b[2J\x1b[2;9r\x1b[2;1H{picture}\x1b[9;1H\n\x1b[r\x1b[11;1Hmargin-clipped"
    )]);
    server.wait("margin-clipped");
    let read = server.read("visible", json!({}));
    assert_eq!(read["pictures"][0]["line"], 1, "{read}");
    assert_eq!(read["pictures"][0]["rows"], 4, "{read}");
    assert_eq!(
        read["pictures"][0]["crop"],
        json!([0, 10, 100, 40]),
        "{read}"
    );
}
