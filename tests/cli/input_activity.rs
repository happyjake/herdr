use super::harness::*;
use serde_json::{json, Value};

struct Session {
    base: PathBuf,
    socket: PathBuf,
    server: Option<SpawnedHerdr>,
    pane_id: String,
}

impl Session {
    fn new() -> Self {
        let base = unique_test_dir();
        let socket = base.join("runtime/herdr.sock");
        let server =
            spawn_herdr_with_pane_history(&base.join("config"), &base.join("runtime"), &socket);
        wait_for_socket(&socket, Duration::from_secs(10));
        let created = run_cli_json(
            &socket,
            &["workspace", "create", "--cwd", base.to_str().unwrap()],
        );
        Self {
            pane_id: created["result"]["root_pane"]["pane_id"]
                .as_str()
                .unwrap()
                .to_owned(),
            base,
            socket,
            server: Some(server),
        }
    }

    fn pane(&self) -> Value {
        run_cli_json(&self.socket, &["pane", "get", &self.pane_id])["result"]["pane"].clone()
    }

    fn stamp(&self) -> u64 {
        let pane = self.pane();
        pane["input_or_command_at"]
            .as_u64()
            .unwrap_or_else(|| panic!("missing activity stamp: {pane}"))
    }

    fn request(&self, method: &str, mut params: Value) -> Value {
        params["pane_id"] = self.pane_id.clone().into();
        let response = send_request(
            &self.socket,
            &json!({"id":"activity", "method":method, "params":params}).to_string(),
        );
        assert!(response.get("error").is_none(), "{response}");
        response
    }

    fn wait_for_stamp_after(&self, previous: u64) -> u64 {
        assert!(wait_until(
            Duration::from_secs(10),
            Duration::from_millis(50),
            || self.stamp() > previous
        ));
        self.stamp()
    }

    fn stop(&mut self) {
        assert!(run_cli(&self.socket, &["server", "stop"]).status.success());
        if let Some(mut server) = self.server.take() {
            let pid = server.child.process_id();
            assert!(server.child.wait().unwrap().success());
            unregister_spawned_herdr_pid(pid);
        }
    }

    fn start(&mut self) {
        self.server = Some(spawn_herdr_with_pane_history(
            &self.base.join("config"),
            &self.base.join("runtime"),
            &self.socket,
        ));
        wait_for_socket(&self.socket, Duration::from_secs(10));
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.server.is_none() && self.socket.exists() {
            let _ = run_cli(&self.socket, &["server", "stop"]);
        }
        drop(self.server.take());
        cleanup_test_base(&self.base);
    }
}

#[test]
fn input_activity_typing_moves_the_stamp() {
    let session = Session::new();
    let before = session.stamp();
    thread::sleep(Duration::from_millis(1100));
    session.request("pane.send_text", json!({"text":"typed without enter"}));
    session.wait_for_stamp_after(before);
    assert!(wait_until(
        Duration::from_secs(5),
        Duration::from_millis(50),
        || { pane_read_recent_contains(&session.socket, &session.pane_id, "typed without enter") }
    ));
}

#[test]
fn input_activity_foreground_change_moves_the_stamp_without_more_input() {
    let session = Session::new();
    let fifo = session.base.join("command-gate");
    assert!(Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap()
        .success());
    let command = format!(
        "printf 'WAIT%s\\n' ING; read token < '{}'; sleep 30",
        fifo.display()
    );
    session.request("pane.send_input", json!({"text":command,"keys":["Enter"]}));
    assert!(wait_until(
        Duration::from_secs(5),
        Duration::from_millis(50),
        || { pane_read_recent_contains(&session.socket, &session.pane_id, "WAITING") }
    ));
    thread::sleep(Duration::from_millis(1500));
    let waiting = session.stamp();
    thread::sleep(Duration::from_millis(1100));
    // Release a shell builtin through a file, without sending terminal input.
    fs::write(fifo, "go\n").unwrap();
    session.wait_for_stamp_after(waiting);
}

#[test]
fn input_activity_survives_cold_restart() {
    let mut session = Session::new();
    session.request("pane.send_text", json!({"text":"retained input"}));
    thread::sleep(Duration::from_millis(1500));
    let before = session.stamp();
    session.stop();
    thread::sleep(Duration::from_millis(1100));
    session.start();
    thread::sleep(Duration::from_millis(1500));
    assert_eq!(session.stamp(), before);
    session.request("pane.send_text", json!({"text":"after restart"}));
    session.wait_for_stamp_after(before);
}

#[test]
fn input_activity_survives_live_handoff() {
    let mut session = Session::new();
    session.request(
        "pane.send_input",
        json!({"text":"sleep 60", "keys":["Enter"]}),
    );
    thread::sleep(Duration::from_secs(2));
    let before = session.stamp();
    let terminal = session.pane()["terminal_id"].clone();
    let handoff = run_cli(&session.socket, &["server", "live-handoff"]);
    assert!(handoff.status.success(), "{handoff:?}");
    drop(session.server.take());
    assert!(wait_until(
        Duration::from_secs(10),
        Duration::from_millis(50),
        || { session.pane()["terminal_id"] != terminal }
    ));
    thread::sleep(Duration::from_millis(1500));
    assert_eq!(session.stamp(), before);
    session.request("pane.send_text", json!({"text":"after handoff"}));
    session.wait_for_stamp_after(before);
}

#[test]
fn input_activity_output_and_resize_leave_the_stamp_unchanged() {
    let session = Session::new();
    session.request("pane.send_input", json!({
        "text":"sh -c 'n=0; while :; do n=$((n + 1)); printf \"OUTPUT-%s\\n\" \"$n\"; sleep 0.1; done'",
        "keys":["Enter"]
    }));
    assert!(wait_until(
        Duration::from_secs(10),
        Duration::from_millis(50),
        || { pane_read_recent_contains(&session.socket, &session.pane_id, "OUTPUT-10") }
    ));
    let before = session.stamp();
    let before_cols =
        session.request("pane.read", json!({"source":"recent"}))["result"]["read"]["cols"].clone();
    let client_socket = session.socket.with_file_name("herdr-client.sock");
    wait_for_socket(&client_socket, Duration::from_secs(5));
    let mut client = UnixStream::connect(client_socket).unwrap();
    let (_, error) = crate::support::client_shell_handshake(
        &mut client,
        crate::support::CURRENT_ENDPOINT_PROTOCOL_GENERATION,
        80,
        24,
    )
    .unwrap();
    assert!(error.is_none(), "{error:?}");
    crate::support::wait_for_client_shell_bootstrap(&mut client, Duration::from_secs(5)).unwrap();
    assert!(
        wait_until(Duration::from_secs(5), Duration::from_millis(50), || {
            session.request("pane.read", json!({"source":"recent"}))["result"]["read"]["cols"]
                != before_cols
        }),
        "client geometry must reach the terminal"
    );
    assert!(wait_until(
        Duration::from_secs(10),
        Duration::from_millis(50),
        || { pane_read_recent_contains(&session.socket, &session.pane_id, "OUTPUT-30") }
    ));
    assert_eq!(session.stamp(), before);
    session.request(
        "pane.send_text",
        json!({"text":"input while output continues"}),
    );
    session.wait_for_stamp_after(before);
}

#[test]
fn input_activity_marked_agent_prompt_is_delivered_without_restamping() {
    use std::os::unix::fs::PermissionsExt;
    let session = Session::new();
    let pong = send_request(
        &session.socket,
        r#"{"id":"ping","method":"ping","params":{}}"#,
    );
    assert_eq!(pong["result"]["capabilities"]["input_activity"], true);
    let script = session.base.join("pi");
    fs::write(
        &script,
        "#!/bin/sh\nwhile IFS= read -r line; do printf 'PICKED:%s\\n' \"$line\"; done\n",
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    session.request(
        "pane.send_input",
        json!({"text":script.display().to_string(),"keys":["Enter"]}),
    );
    assert!(wait_until(
        Duration::from_secs(10),
        Duration::from_millis(50),
        || session.pane()["agent"] == "pi"
    ));
    session.request(
        "pane.report_agent",
        json!({"source":"custom:test","agent":"pi","state":"idle"}),
    );
    let before = session.stamp();
    thread::sleep(Duration::from_millis(1100));
    for (text, preserve) in [("ordinary prompt", false), ("maintenance prompt", true)] {
        let stamp = session.stamp();
        thread::sleep(Duration::from_millis(1100));
        let response = send_request(
            &session.socket,
            &json!({
                "id":"prompt", "method":"agent.prompt", "params":{
                    "target":session.pane_id, "text":text, "preserve_activity":preserve
                }
            })
            .to_string(),
        );
        assert!(response.get("error").is_none(), "{response}");
        assert!(wait_until(
            Duration::from_secs(5),
            Duration::from_millis(50),
            || {
                pane_read_recent_contains(
                    &session.socket,
                    &session.pane_id,
                    &format!("PICKED:{text}"),
                )
            }
        ));
        if preserve {
            assert_eq!(session.stamp(), stamp);
        } else {
            session.wait_for_stamp_after(before);
        }
    }
    let preserved = session.stamp();
    session.request("pane.send_text", json!({"text":"typed by a person"}));
    session.wait_for_stamp_after(preserved);
}

#[test]
fn input_activity_marked_text_and_command_input_are_delivered_without_restamping() {
    let session = Session::new();
    thread::sleep(Duration::from_millis(1500));
    let before = session.stamp();
    session.request(
        "pane.send_text",
        json!({"text":"printf 'KEPT%s\\n' _CLOCK", "preserve_activity":true}),
    );
    session.request(
        "pane.send_input",
        json!({"text":"", "keys":["Enter"], "preserve_activity":true}),
    );
    assert!(wait_until(
        Duration::from_secs(5),
        Duration::from_millis(50),
        || { pane_read_recent_contains(&session.socket, &session.pane_id, "KEPT_CLOCK") }
    ));
    thread::sleep(Duration::from_millis(1100));
    assert_eq!(session.stamp(), before);
    session.request(
        "pane.send_input",
        json!({"text":"printf 'STILL%s\\n' _KEPT", "keys":["Enter"], "preserve_activity":true}),
    );
    assert!(wait_until(
        Duration::from_secs(5),
        Duration::from_millis(50),
        || { pane_read_recent_contains(&session.socket, &session.pane_id, "STILL_KEPT") }
    ));
    assert_eq!(session.stamp(), before);
}

#[test]
fn input_activity_legacy_snapshot_starts_a_clock_without_losing_history() {
    use sha2::{Digest, Sha256};
    let mut session = Session::new();
    session.request(
        "pane.send_input",
        json!({"text":"printf 'LEGACY%s\\n' _HISTORY", "keys":["Enter"]}),
    );
    assert!(wait_until(
        Duration::from_secs(5),
        Duration::from_millis(50),
        || { pane_read_recent_contains(&session.socket, &session.pane_id, "LEGACY_HISTORY") }
    ));
    session.stop();
    let data = session.base.join("config").join(app_dir_name());
    let snapshot_path = data.join("session.json");
    let history_path = data.join("session-history.json");
    let mut snapshot: Value = serde_json::from_slice(&fs::read(&snapshot_path).unwrap()).unwrap();
    for workspace in snapshot["workspaces"].as_array_mut().unwrap() {
        for tab in workspace["tabs"].as_array_mut().unwrap() {
            for pane in tab["panes"].as_object_mut().unwrap().values_mut() {
                assert!(pane
                    .as_object_mut()
                    .unwrap()
                    .remove("input_or_command_at")
                    .is_some());
            }
        }
    }
    let bytes = serde_json::to_vec(&snapshot).unwrap();
    let mut history: Value = serde_json::from_slice(&fs::read(&history_path).unwrap()).unwrap();
    history["layout_fingerprint"] = json!(format!("{:x}", Sha256::digest(&bytes)));
    fs::write(snapshot_path, bytes).unwrap();
    fs::write(history_path, serde_json::to_vec(&history).unwrap()).unwrap();
    let started = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    session.start();
    assert!(session.stamp() >= started);
    assert!(pane_read_recent_contains(
        &session.socket,
        &session.pane_id,
        "LEGACY_HISTORY"
    ));
}
