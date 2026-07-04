//! Protocol equivalence between the Unix socket API and the WebSocket API.
//!
//! These tests run a real `herdr server` with the WebSocket listener bound on
//! localhost and drive the same requests over both transports, asserting the
//! payloads are identical modulo framing (JSON lines vs one JSON message per
//! text frame). Responses are compared as parsed JSON values so the assertion
//! is framing-independent. Handshake rejection and the off-by-default
//! behavior are covered here too; token validation unit tests live in
//! `src/api/websocket.rs`.

mod support;

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use support::{
    cleanup_test_base, register_runtime_dir, register_spawned_herdr_pid,
    unregister_spawned_herdr_pid, wait_for_socket,
};
use tungstenite::client::IntoClientRequest;
use tungstenite::{Message, WebSocket};

const TEST_TOKEN: &str = "ws-api-test-token";
const TEST_SERVER_NAME: &str = "ws-test-server";

fn test_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn unique_test_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    PathBuf::from(format!("/tmp/hws-{}-{nanos}", std::process::id()))
}

/// Reserve a port by binding to an ephemeral one and releasing it. The tiny
/// window between release and the server's bind is an accepted test-only
/// race; nothing else in the suite touches these ports.
fn pick_free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = listener.local_addr().expect("ephemeral port addr").port();
    drop(listener);
    port
}

struct SpawnedHerdr {
    _master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
}

impl SpawnedHerdr {
    #[cfg(target_os = "linux")]
    fn pid(&self) -> Option<u32> {
        self.child.process_id()
    }
}

impl Drop for SpawnedHerdr {
    fn drop(&mut self) {
        let pid = self.child.process_id();
        let _ = self.child.kill();

        if let Some(pid) = pid {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                let mut status = 0;
                let result =
                    unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
                if result == pid as libc::pid_t || result == -1 {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }

            unregister_spawned_herdr_pid(Some(pid));
        }
    }
}

fn cleanup_spawned_herdr(spawned: SpawnedHerdr, base: PathBuf) {
    drop(spawned);
    cleanup_test_base(&base);
}

/// Spawn a real herdr server. `websocket` is the config section body to
/// append after `onboarding = false`, empty for the default (no listener).
fn spawn_herdr_with_config(
    config_home: &Path,
    runtime_dir: &Path,
    socket_path: &Path,
    websocket_section: &str,
) -> SpawnedHerdr {
    // Debug builds read the herdr-dev config dir; write the release dir too
    // so the fixture does not depend on the build profile.
    for dir in ["herdr", "herdr-dev"] {
        fs::create_dir_all(config_home.join(dir)).unwrap();
        fs::write(
            config_home.join(dir).join("config.toml"),
            format!("onboarding = false\n{websocket_section}"),
        )
        .unwrap();
    }
    fs::create_dir_all(runtime_dir).unwrap();
    register_runtime_dir(runtime_dir);

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_herdr"));
    cmd.arg("server");
    cmd.env("XDG_CONFIG_HOME", config_home);
    cmd.env("XDG_RUNTIME_DIR", runtime_dir);
    cmd.env("HERDR_SOCKET_PATH", socket_path);
    // An inherited config override (tests running inside a herdr pane) would
    // make the spawned server read the real config instead of the fixture.
    cmd.env_remove("HERDR_CONFIG_PATH");
    cmd.env_remove("HERDR_CLIENT_SOCKET_PATH");
    cmd.env("SHELL", "/bin/sh");
    cmd.env_remove("HERDR_ENV");

    let child = pair.slave.spawn_command(cmd).unwrap();
    register_spawned_herdr_pid(child.process_id());

    SpawnedHerdr {
        _master: pair.master,
        child,
    }
}

fn websocket_section(port: u16) -> String {
    format!(
        "[websocket_api]\nbind = \"127.0.0.1:{port}\"\ntoken = \"{TEST_TOKEN}\"\nname = \"{TEST_SERVER_NAME}\"\n"
    )
}

fn wait_for_ws_listener(addr: SocketAddr, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if TcpStream::connect(addr).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("websocket listener did not appear at {addr}");
}

// ---- Unix socket client (same shape as tests/api_ping.rs) ----

struct JsonLineReader {
    stream: UnixStream,
    buf: Vec<u8>,
}

impl JsonLineReader {
    fn connect(socket_path: &Path) -> Self {
        Self {
            stream: UnixStream::connect(socket_path).unwrap(),
            buf: Vec::new(),
        }
    }

    fn send_line(&mut self, json: &str) {
        self.stream.write_all(json.as_bytes()).unwrap();
        self.stream.write_all(b"\n").unwrap();
        self.stream.flush().unwrap();
    }

    /// One response line with the newline framing stripped — the raw payload.
    fn read_raw_line(&mut self, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        self.stream.set_nonblocking(true).unwrap();

        loop {
            if Instant::now() >= deadline {
                panic!("timed out waiting for json line");
            }

            if let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                let mut line = String::from_utf8(self.buf.drain(..=pos).collect()).unwrap();
                line.truncate(line.len() - 1);
                self.stream.set_nonblocking(false).unwrap();
                return line;
            }

            let mut bytes = [0u8; 256];
            match self.stream.read(&mut bytes) {
                Ok(0) => panic!("stream closed while waiting for json line"),
                Ok(n) => self.buf.extend_from_slice(&bytes[..n]),
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(err) => panic!("failed to read json line: {err}"),
            }
        }
    }

    fn read_json_line(&mut self, timeout: Duration) -> serde_json::Value {
        serde_json::from_str(&self.read_raw_line(timeout)).unwrap()
    }
}

fn unix_request(socket_path: &Path, json: &str) -> serde_json::Value {
    let mut reader = JsonLineReader::connect(socket_path);
    reader.send_line(json);
    reader.read_json_line(Duration::from_secs(5))
}

fn open_unix_subscription(socket_path: &Path, json: &str) -> JsonLineReader {
    let mut reader = JsonLineReader::connect(socket_path);
    reader.send_line(json);
    reader
}

// ---- WebSocket client ----

struct WsClient {
    websocket: WebSocket<TcpStream>,
}

impl WsClient {
    fn connect(addr: SocketAddr, token: &str) -> Self {
        let stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut request = format!("ws://{addr}").into_client_request().unwrap();
        request.headers_mut().insert(
            tungstenite::http::header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        let websocket = complete_client_handshake(request, stream);
        Self { websocket }
    }

    fn send(&mut self, json: &str) {
        self.websocket.send(Message::text(json)).unwrap();
    }

    /// One text frame's payload — WS framing carries no newline.
    fn read_raw(&mut self, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        loop {
            match self.websocket.read() {
                Ok(Message::Text(text)) => return text.as_str().to_string(),
                Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => continue,
                Ok(other) => panic!("unexpected websocket message: {other:?}"),
                Err(tungstenite::Error::Io(err))
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    if Instant::now() >= deadline {
                        panic!("timed out waiting for websocket json message");
                    }
                }
                Err(err) => panic!("failed to read websocket message: {err}"),
            }
        }
    }

    fn read_json(&mut self, timeout: Duration) -> serde_json::Value {
        serde_json::from_str(&self.read_raw(timeout)).unwrap()
    }

    fn request(&mut self, json: &str) -> serde_json::Value {
        self.send(json);
        self.read_json(Duration::from_secs(5))
    }
}

/// Drive a client handshake to completion over a read-timeout stream, where
/// a slow server read surfaces as `HandshakeError::Interrupted`.
fn complete_client_handshake(
    request: tungstenite::handshake::client::Request,
    stream: TcpStream,
) -> WebSocket<TcpStream> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut pending = match tungstenite::client::client(request, stream) {
        Ok((websocket, _response)) => return websocket,
        Err(tungstenite::HandshakeError::Interrupted(pending)) => pending,
        Err(tungstenite::HandshakeError::Failure(err)) => panic!("ws handshake failed: {err}"),
    };
    loop {
        assert!(Instant::now() < deadline, "ws handshake timed out");
        match pending.handshake() {
            Ok((websocket, _response)) => return websocket,
            Err(tungstenite::HandshakeError::Interrupted(next)) => pending = next,
            Err(tungstenite::HandshakeError::Failure(err)) => panic!("ws handshake failed: {err}"),
        }
    }
}

fn wait_for_event_matching<F>(
    read: &mut dyn FnMut(Duration) -> serde_json::Value,
    expected: &str,
    timeout: Duration,
    mut matches: F,
) -> serde_json::Value
where
    F: FnMut(&serde_json::Value) -> bool,
{
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let value = read(remaining.max(Duration::from_millis(1)));
        if value["event"] == expected && matches(&value) {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for event {expected}"
        );
    }
}

/// Assert two fetches converge on identical values. Retries because live
/// server state (a shell pane painting its prompt) may legitimately change
/// between the two reads; equivalence only requires that the same request
/// against the same state produces the same payload.
fn assert_eventually_identical(
    label: &str,
    mut unix_fetch: impl FnMut() -> serde_json::Value,
    mut ws_fetch: impl FnMut() -> serde_json::Value,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let (mut unix_value, mut ws_value) = (unix_fetch(), ws_fetch());
    loop {
        if unix_value == ws_value {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{label} diverged between transports:\nunix: {unix_value}\nws:   {ws_value}"
        );
        thread::sleep(Duration::from_millis(100));
        unix_value = unix_fetch();
        ws_value = ws_fetch();
    }
}

struct WsTestServer {
    base: PathBuf,
    socket_path: PathBuf,
    ws_addr: SocketAddr,
    child: SpawnedHerdr,
}

fn start_ws_test_server() -> WsTestServer {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");
    let ws_port = pick_free_port();
    let ws_addr: SocketAddr = format!("127.0.0.1:{ws_port}").parse().unwrap();

    let child = spawn_herdr_with_config(
        &config_home,
        &runtime_dir,
        &socket_path,
        &websocket_section(ws_port),
    );
    wait_for_socket(&socket_path, Duration::from_secs(5));
    wait_for_ws_listener(ws_addr, Duration::from_secs(5));

    WsTestServer {
        base,
        socket_path,
        ws_addr,
        child,
    }
}

#[test]
fn ping_response_is_identical_over_unix_socket_and_websocket() {
    let _lock = test_lock();
    let server = start_ws_test_server();

    let request = r#"{"id":"req_eq_ping","method":"ping","params":{}}"#;

    // Compare the raw payloads: the unix line minus its newline and the WS
    // text frame must match byte for byte — identical modulo framing.
    let mut unix_reader = JsonLineReader::connect(&server.socket_path);
    unix_reader.send_line(request);
    let unix_raw = unix_reader.read_raw_line(Duration::from_secs(5));

    let mut ws = WsClient::connect(server.ws_addr, TEST_TOKEN);
    ws.send(request);
    let ws_raw = ws.read_raw(Duration::from_secs(5));

    assert_eq!(unix_raw, ws_raw, "raw ping payloads must be identical");

    let ws_response: serde_json::Value = serde_json::from_str(&ws_raw).unwrap();
    assert_eq!(ws_response["result"]["type"], "pong");
    assert_eq!(ws_response["result"]["version"], env!("CARGO_PKG_VERSION"));
    // The declared server name rides the same pong on both transports (the
    // raw equality above already proves they match).
    assert_eq!(ws_response["result"]["name"], TEST_SERVER_NAME);

    cleanup_spawned_herdr(server.child, server.base);
}

/// Rewrite the spawned server's config with a different `[websocket_api]`
/// section body, matching the layout `spawn_herdr_with_config` wrote.
fn rewrite_herdr_config(config_home: &Path, websocket_section: &str) {
    for dir in ["herdr", "herdr-dev"] {
        fs::write(
            config_home.join(dir).join("config.toml"),
            format!("onboarding = false\n{websocket_section}"),
        )
        .unwrap();
    }
}

#[test]
fn pong_name_defaults_to_the_hostname_and_follows_config_reload() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");
    let ws_port = pick_free_port();
    let ws_addr: SocketAddr = format!("127.0.0.1:{ws_port}").parse().unwrap();

    // No `name` in the config: the server declares the machine hostname.
    let nameless_section =
        format!("[websocket_api]\nbind = \"127.0.0.1:{ws_port}\"\ntoken = \"{TEST_TOKEN}\"\n");
    let child =
        spawn_herdr_with_config(&config_home, &runtime_dir, &socket_path, &nameless_section);
    wait_for_socket(&socket_path, Duration::from_secs(5));
    wait_for_ws_listener(ws_addr, Duration::from_secs(5));

    let ping = r#"{"id":"req_name","method":"ping","params":{}}"#;
    let default_pong = unix_request(&socket_path, ping);
    let default_name = default_pong["result"]["name"]
        .as_str()
        .expect("pong must declare a name even without one configured")
        .to_string();
    assert!(!default_name.is_empty());

    let mut ws = WsClient::connect(ws_addr, TEST_TOKEN);
    assert_eq!(ws.request(ping)["result"]["name"], default_name.as_str());

    // Rename via config reload — the same path token rotation uses. No
    // restart: the websocket connection opened above keeps working and the
    // very next pong carries the new name on both transports.
    rewrite_herdr_config(
        &config_home,
        &format!("{nameless_section}name = \"renamed-server\"\n"),
    );
    let reloaded = unix_request(
        &socket_path,
        r#"{"id":"req_name_reload","method":"server.reload_config","params":{}}"#,
    );
    assert_eq!(reloaded["result"]["type"], "config_reload");

    let renamed_pong = unix_request(&socket_path, ping);
    assert_eq!(renamed_pong["result"]["name"], "renamed-server");
    assert_eq!(ws.request(ping)["result"]["name"], "renamed-server");

    cleanup_spawned_herdr(child, base);
}

#[test]
fn websocket_handshake_tolerates_unknown_query_parameters() {
    // A client that reads only the token from the pairing URL's query — or
    // replays the whole query including the name and any future parameters —
    // must keep connecting. Pins the QR-compatibility contract.
    let _lock = test_lock();
    let server = start_ws_test_server();

    let url = format!(
        "ws://{}/?token={TEST_TOKEN}&name=some%20server&future=1",
        server.ws_addr
    );
    let stream = TcpStream::connect(server.ws_addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let request = url.into_client_request().unwrap();
    let mut ws = WsClient {
        websocket: complete_client_handshake(request, stream),
    };

    let pong = ws.request(r#"{"id":"req_query_extra","method":"ping","params":{}}"#);
    assert_eq!(pong["result"]["type"], "pong");
    assert_eq!(pong["result"]["name"], TEST_SERVER_NAME);

    cleanup_spawned_herdr(server.child, server.base);
}

#[test]
fn pane_list_and_pane_read_are_identical_over_unix_socket_and_websocket() {
    let _lock = test_lock();
    let server = start_ws_test_server();

    let created = unix_request(
        &server.socket_path,
        &format!(
            r#"{{"id":"req_eq_ws1","method":"workspace.create","params":{{"cwd":"{}","focus":true}}}}"#,
            server.base.display()
        ),
    );
    assert_eq!(created["result"]["type"], "workspace_created");
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();

    let mut ws = WsClient::connect(server.ws_addr, TEST_TOKEN);

    let list_request = r#"{"id":"req_eq_list","method":"pane.list","params":{}}"#;
    assert_eventually_identical(
        "pane.list",
        || unix_request(&server.socket_path, list_request),
        || ws.request(list_request),
    );

    let read_request = format!(
        r#"{{"id":"req_eq_read","method":"pane.read","params":{{"pane_id":"{pane_id}","source":"visible"}}}}"#
    );
    let mut ws = WsClient::connect(server.ws_addr, TEST_TOKEN);
    assert_eventually_identical(
        "pane.read",
        || unix_request(&server.socket_path, &read_request),
        || ws.request(&read_request),
    );

    cleanup_spawned_herdr(server.child, server.base);
}

#[test]
fn send_text_and_wait_for_output_work_over_one_websocket_connection() {
    let _lock = test_lock();
    let server = start_ws_test_server();

    let created = unix_request(
        &server.socket_path,
        &format!(
            r#"{{"id":"req_ws_flow1","method":"workspace.create","params":{{"cwd":"{}","focus":true}}}}"#,
            server.base.display()
        ),
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();

    // One connection, sequential requests: send text, press enter, wait for
    // the echoed output — the whole phone "respond" path over WS only.
    let mut ws = WsClient::connect(server.ws_addr, TEST_TOKEN);

    let sent = ws.request(&format!(
        r#"{{"id":"ws_send","method":"pane.send_text","params":{{"pane_id":"{pane_id}","text":"echo ws-roundtrip-ok"}}}}"#
    ));
    assert_eq!(sent["result"]["type"], "ok");

    let entered = ws.request(&format!(
        r#"{{"id":"ws_enter","method":"pane.send_keys","params":{{"pane_id":"{pane_id}","keys":["Enter"]}}}}"#
    ));
    assert_eq!(entered["result"]["type"], "ok");

    ws.send(&format!(
        r#"{{"id":"ws_wait","method":"pane.wait_for_output","params":{{"pane_id":"{pane_id}","source":"recent","lines":40,"match":{{"type":"substring","value":"ws-roundtrip-ok"}},"timeout_ms":5000}}}}"#
    ));
    let waited = ws.read_json(Duration::from_secs(6));
    assert_eq!(waited["id"], "ws_wait");
    assert_eq!(waited["result"]["type"], "output_matched");
    assert!(waited["result"]["matched_line"]
        .as_str()
        .unwrap()
        .contains("ws-roundtrip-ok"));

    cleanup_spawned_herdr(server.child, server.base);
}

#[test]
fn events_subscription_payloads_are_identical_over_unix_socket_and_websocket() {
    let _lock = test_lock();
    let server = start_ws_test_server();

    let subscribe = r#"{"id":"sub_eq","method":"events.subscribe","params":{"subscriptions":[{"type":"workspace.created"},{"type":"tab.created"},{"type":"pane.created"}]}}"#;

    let mut unix_reader = open_unix_subscription(&server.socket_path, subscribe);
    let mut ws = WsClient::connect(server.ws_addr, TEST_TOKEN);
    ws.send(subscribe);

    let unix_ack = unix_reader.read_json_line(Duration::from_secs(2));
    let ws_ack = ws.read_json(Duration::from_secs(2));
    assert_eq!(unix_ack, ws_ack);
    assert_eq!(ws_ack["result"]["type"], "subscription_started");

    let created = unix_request(
        &server.socket_path,
        &format!(
            r#"{{"id":"req_eq_evt","method":"workspace.create","params":{{"cwd":"{}","focus":true}}}}"#,
            server.base.display()
        ),
    );
    let workspace_id = created["result"]["workspace"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_string();

    let mut read_unix = |timeout: Duration| unix_reader.read_json_line(timeout);
    let mut read_ws = |timeout: Duration| ws.read_json(timeout);

    // The same event stream must deliver the same payloads on both
    // transports. Streaming order within one connection follows the hub
    // sequence, so match each expected kind and compare whole envelopes.
    for kind in ["workspace_created", "tab_created", "pane_created"] {
        let unix_event =
            wait_for_event_matching(&mut read_unix, kind, Duration::from_secs(3), |_| true);
        let ws_event =
            wait_for_event_matching(&mut read_ws, kind, Duration::from_secs(3), |_| true);
        assert_eq!(unix_event, ws_event, "event {kind} diverged");
    }

    // Liveness: a later event still flows to both.
    let renamed = unix_request(
        &server.socket_path,
        &format!(
            r#"{{"id":"req_eq_evt2","method":"tab.create","params":{{"workspace_id":"{workspace_id}","focus":true}}}}"#
        ),
    );
    assert_eq!(renamed["result"]["type"], "tab_created");
    let unix_event = wait_for_event_matching(
        &mut read_unix,
        "tab_created",
        Duration::from_secs(3),
        |_| true,
    );
    let ws_event =
        wait_for_event_matching(&mut read_ws, "tab_created", Duration::from_secs(3), |_| {
            true
        });
    assert_eq!(unix_event, ws_event, "live tab_created diverged");

    cleanup_spawned_herdr(server.child, server.base);
}

#[test]
fn websocket_handshake_is_rejected_without_a_valid_token() {
    let _lock = test_lock();
    let server = start_ws_test_server();

    // Missing token.
    let err = tungstenite::connect(format!("ws://{}", server.ws_addr)).unwrap_err();
    match err {
        tungstenite::Error::Http(response) => assert_eq!(response.status().as_u16(), 401),
        other => panic!("expected http 401, got: {other:?}"),
    }

    // Wrong token.
    let mut request = format!("ws://{}", server.ws_addr)
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        tungstenite::http::header::AUTHORIZATION,
        "Bearer definitely-wrong".parse().unwrap(),
    );
    let err = tungstenite::connect(request).unwrap_err();
    match err {
        tungstenite::Error::Http(response) => assert_eq!(response.status().as_u16(), 401),
        other => panic!("expected http 401, got: {other:?}"),
    }

    // The server keeps serving authorized clients afterwards.
    let mut ws = WsClient::connect(server.ws_addr, TEST_TOKEN);
    let pong = ws.request(r#"{"id":"req_after_401","method":"ping","params":{}}"#);
    assert_eq!(pong["result"]["type"], "pong");

    cleanup_spawned_herdr(server.child, server.base);
}

// ---- Pairing CLI ----

struct PairOutcome {
    exit_code: i32,
    stdout: String,
    stderr: String,
}

/// Run `herdr pair` against the same config dir and API socket a spawned
/// test server uses, with inherited herdr overrides cleared.
fn run_pair_cli(config_home: &Path, runtime_dir: &Path, socket_path: &Path) -> PairOutcome {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_herdr"))
        .arg("pair")
        .env("XDG_CONFIG_HOME", config_home)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("HERDR_SOCKET_PATH", socket_path)
        .env_remove("HERDR_CONFIG_PATH")
        .env_remove("HERDR_CLIENT_SOCKET_PATH")
        .env_remove("HERDR_ENV")
        .output()
        .expect("run herdr pair");
    PairOutcome {
        exit_code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    }
}

fn minted_token_from_pair_stdout(stdout: &str) -> String {
    stdout
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("token ")
                .map(|token| token.trim().to_string())
        })
        .unwrap_or_else(|| panic!("pair output must contain a token line:\n{stdout}"))
}

/// The stored token survives for the next server start. The pair CLI writes
/// the config dir matching its build profile, so accept either variant.
fn stored_config_contains(config_home: &Path, needle: &str) -> bool {
    ["herdr", "herdr-dev"].iter().any(|dir| {
        fs::read_to_string(config_home.join(dir).join("config.toml"))
            .is_ok_and(|content| content.contains(needle))
    })
}

fn expect_handshake_rejected(addr: SocketAddr, token: &str) {
    let mut request = format!("ws://{addr}").into_client_request().unwrap();
    request.headers_mut().insert(
        tungstenite::http::header::AUTHORIZATION,
        format!("Bearer {token}").parse().unwrap(),
    );
    match tungstenite::connect(request).unwrap_err() {
        tungstenite::Error::Http(response) => assert_eq!(response.status().as_u16(), 401),
        other => panic!("expected http 401 for token {token:?}, got: {other:?}"),
    }
}

#[test]
fn pair_cli_rotates_the_token_and_the_live_listener_enforces_it() {
    let _lock = test_lock();
    let server = start_ws_test_server();
    let config_home = server.base.join("config");
    let runtime_dir = server.base.join("runtime");

    // Baseline: the initially configured token authenticates.
    let mut ws = WsClient::connect(server.ws_addr, TEST_TOKEN);
    let pong = ws.request(r#"{"id":"req_pair_base","method":"ping","params":{}}"#);
    assert_eq!(pong["result"]["type"], "pong");

    let pair = run_pair_cli(&config_home, &runtime_dir, &server.socket_path);
    assert_eq!(
        pair.exit_code, 0,
        "stdout:\n{}\nstderr:\n{}",
        pair.stdout, pair.stderr
    );
    let minted = minted_token_from_pair_stdout(&pair.stdout);

    // The payload carries the endpoint and token in connectable form, plus a
    // terminal QR rendering of the same URL.
    assert!(
        pair.stdout
            .contains(&format!("ws://{}/?token={minted}", server.ws_addr)),
        "payload url missing:\n{}",
        pair.stdout
    );
    assert!(
        pair.stdout
            .contains(&format!("endpoint  ws://{}", server.ws_addr)),
        "plaintext endpoint missing:\n{}",
        pair.stdout
    );
    assert!(
        pair.stdout.contains('█') || pair.stdout.contains('▀') || pair.stdout.contains('▄'),
        "terminal qr missing:\n{}",
        pair.stdout
    );

    // The freshly minted token authenticates against the live listener; the
    // previous token is rejected without a server restart.
    expect_handshake_rejected(server.ws_addr, TEST_TOKEN);
    let mut ws = WsClient::connect(server.ws_addr, &minted);
    let pong = ws.request(r#"{"id":"req_pair_new","method":"ping","params":{}}"#);
    assert_eq!(pong["result"]["type"], "pong");

    // Re-running rotates again: the earlier mint stops authenticating.
    let second = run_pair_cli(&config_home, &runtime_dir, &server.socket_path);
    assert_eq!(second.exit_code, 0, "stderr:\n{}", second.stderr);
    let minted_again = minted_token_from_pair_stdout(&second.stdout);
    assert_ne!(minted, minted_again);

    expect_handshake_rejected(server.ws_addr, &minted);
    let mut ws = WsClient::connect(server.ws_addr, &minted_again);
    let pong = ws.request(r#"{"id":"req_pair_second","method":"ping","params":{}}"#);
    assert_eq!(pong["result"]["type"], "pong");

    // The rotation is persisted: only the latest token is stored.
    assert!(stored_config_contains(&config_home, &minted_again));
    assert!(!stored_config_contains(&config_home, &minted));

    cleanup_spawned_herdr(server.child, server.base);
}

#[test]
fn pair_cli_explains_required_config_when_listener_is_not_configured() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    for dir in ["herdr", "herdr-dev"] {
        fs::create_dir_all(config_home.join(dir)).unwrap();
        fs::write(
            config_home.join(dir).join("config.toml"),
            "onboarding = false\n",
        )
        .unwrap();
    }
    fs::create_dir_all(&runtime_dir).unwrap();

    let pair = run_pair_cli(&config_home, &runtime_dir, &runtime_dir.join("herdr.sock"));

    assert_ne!(pair.exit_code, 0, "stdout:\n{}", pair.stdout);
    assert!(
        pair.stderr.contains("[websocket_api]") && pair.stderr.contains("bind"),
        "explanation must name the required config:\n{}",
        pair.stderr
    );
    assert!(
        !pair.stdout.contains("ws://") && !pair.stdout.contains("token"),
        "no payload may be printed without a configured listener:\n{}",
        pair.stdout
    );

    cleanup_test_base(&base);
}

#[test]
fn pair_cli_provisions_the_token_before_the_first_server_start() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let port = pick_free_port();
    for dir in ["herdr", "herdr-dev"] {
        fs::create_dir_all(config_home.join(dir)).unwrap();
        fs::write(
            config_home.join(dir).join("config.toml"),
            format!("onboarding = false\n[websocket_api]\nbind = \"127.0.0.1:{port}\"\n"),
        )
        .unwrap();
    }
    fs::create_dir_all(&runtime_dir).unwrap();

    let pair = run_pair_cli(&config_home, &runtime_dir, &runtime_dir.join("herdr.sock"));

    assert_eq!(pair.exit_code, 0, "stderr:\n{}", pair.stderr);
    let minted = minted_token_from_pair_stdout(&pair.stdout);
    assert!(pair
        .stdout
        .contains(&format!("ws://127.0.0.1:{port}/?token={minted}")));
    assert!(
        pair.stdout.contains("No running herdr server"),
        "must say the token applies at next start:\n{}",
        pair.stdout
    );
    assert!(stored_config_contains(&config_home, &minted));

    cleanup_test_base(&base);
}

#[test]
fn no_websocket_config_opens_no_port_and_keeps_unix_socket_working() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");
    let probe_port = pick_free_port();

    let child = spawn_herdr_with_config(&config_home, &runtime_dir, &socket_path, "");
    wait_for_socket(&socket_path, Duration::from_secs(5));

    let pong = unix_request(
        &socket_path,
        r#"{"id":"req_default_ping","method":"ping","params":{}}"#,
    );
    assert_eq!(pong["result"]["type"], "pong");

    let refused = TcpStream::connect(format!("127.0.0.1:{probe_port}"));
    assert!(
        refused.is_err(),
        "no listener may appear on an unconfigured port"
    );

    #[cfg(target_os = "linux")]
    {
        let pid = child.pid().expect("spawned server pid");
        assert_eq!(
            listening_tcp_local_ports(pid),
            Vec::<u16>::new(),
            "an unconfigured server must own no listening tcp sockets"
        );
    }

    cleanup_spawned_herdr(child, base);
}

#[cfg(target_os = "linux")]
#[test]
fn websocket_listener_owns_exactly_the_configured_port() {
    let _lock = test_lock();
    let server = start_ws_test_server();

    let pid = server.child.pid().expect("spawned server pid");
    assert_eq!(
        listening_tcp_local_ports(pid),
        vec![server.ws_addr.port()],
        "the configured websocket port must be the only listening tcp socket"
    );

    cleanup_spawned_herdr(server.child, server.base);
}

/// Ports of TCP sockets in LISTEN state owned by `pid`, from /proc.
#[cfg(target_os = "linux")]
fn listening_tcp_local_ports(pid: u32) -> Vec<u16> {
    use std::collections::HashSet;

    let mut socket_inodes = HashSet::new();
    for entry in fs::read_dir(format!("/proc/{pid}/fd")).expect("read process fds") {
        let Ok(entry) = entry else { continue };
        let Ok(target) = fs::read_link(entry.path()) else {
            continue;
        };
        let target = target.to_string_lossy().to_string();
        if let Some(inode) = target
            .strip_prefix("socket:[")
            .and_then(|rest| rest.strip_suffix(']'))
        {
            socket_inodes.insert(inode.to_string());
        }
    }

    let mut ports = Vec::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(content) = fs::read_to_string(table) else {
            continue;
        };
        for line in content.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 10 {
                continue;
            }
            let listen_state = fields[3] == "0A";
            if !listen_state || !socket_inodes.contains(fields[9]) {
                continue;
            }
            if let Some((_, port_hex)) = fields[1].rsplit_once(':') {
                if let Ok(port) = u16::from_str_radix(port_hex, 16) {
                    ports.push(port);
                }
            }
        }
    }
    ports.sort_unstable();
    ports.dedup();
    ports
}
