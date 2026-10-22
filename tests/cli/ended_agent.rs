use super::harness::*;
use crate::support::InPaneReporter;
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
        let pane_id = created["result"]["root_pane"]["pane_id"]
            .as_str()
            .unwrap()
            .to_owned();
        Self {
            base,
            socket,
            server: Some(server),
            pane_id,
        }
    }

    fn pane(&self) -> Value {
        run_cli_json(&self.socket, &["pane", "get", &self.pane_id])["result"]["pane"].clone()
    }

    fn agent(&self, kind: &str, run: &str) -> InPaneReporter {
        let dir = self.base.join(run);
        fs::create_dir_all(&dir).unwrap();
        let agent = InPaneReporter::write(&dir, kind, &self.socket);
        let result = run_cli(
            &self.socket,
            &["pane", "run", &self.pane_id, &agent.command()],
        );
        assert!(result.status.success(), "{result:?}");
        agent.wait_until_started(Duration::from_secs(10));
        let detected = self.wait_for(|pane| pane["agent"] == kind);
        assert_eq!(detected.get("ended_agent"), Some(&Value::Null));
        // Exercise the verified in-pane CLI report lane as well as process detection.
        agent.ok(&[
            "pane",
            "report-agent",
            &self.pane_id,
            "--source",
            &format!("herdr:{kind}"),
            "--agent",
            kind,
            "--state",
            "idle",
        ]);
        agent
    }

    fn wait_for(&self, matches: impl Fn(&Value) -> bool) -> Value {
        let mut pane = Value::Null;
        assert!(
            wait_until(Duration::from_secs(10), Duration::from_millis(50), || {
                pane = self.pane();
                matches(&pane)
            }),
            "unexpected pane: {pane}"
        );
        pane
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

    fn data_dir(&self) -> PathBuf {
        self.base.join("config").join(app_dir_name())
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
fn ended_agent_remembers_exit_and_clears_for_the_next_agent() {
    let session = Session::new();
    let first = session.agent("pi", "first");
    let active = session.pane();
    first.stop();
    let ended =
        session.wait_for(|pane| pane["agent"].is_null() && pane["agent_status"] == "unknown");
    assert_eq!(ended.get("ended_agent"), Some(&active["agent"]));
    assert!(
        ended["agent_status_changed_at"].as_u64().unwrap()
            > active["agent_status_changed_at"].as_u64().unwrap()
    );

    let next = session.agent("codex", "next");
    assert_eq!(session.pane().get("ended_agent"), Some(&Value::Null));
    next.stop();
    let ended =
        session.wait_for(|pane| pane["agent"].is_null() && pane["agent_status"] == "unknown");
    assert_eq!(ended["ended_agent"], "codex");
}

#[test]
fn ended_agent_survives_snapshot_and_cold_restore() {
    let mut session = Session::new();
    let agent = session.agent("pi", "first");
    agent.stop();
    let ended =
        session.wait_for(|pane| pane["agent"].is_null() && pane["agent_status"] == "unknown");
    assert_eq!(ended["ended_agent"], "pi");
    session.stop();

    let snapshot: Value =
        serde_json::from_slice(&fs::read(session.data_dir().join("session.json")).unwrap())
            .unwrap();
    let saved = snapshot["workspaces"][0]["tabs"][0]["panes"]
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap();
    assert_eq!(saved["ended_agent"], "pi");

    session.start();
    let restored = session.pane();
    assert_ne!(restored["terminal_id"], ended["terminal_id"]);
    assert_eq!(restored["ended_agent"], "pi");
    assert_eq!(
        restored["agent_status_changed_at"],
        ended["agent_status_changed_at"]
    );
    let next = session.agent("pi", "next");
    assert_eq!(session.pane().get("ended_agent"), Some(&Value::Null));
    next.stop();
}

#[test]
fn ended_agent_absent_from_legacy_snapshot_preserves_history() {
    use sha2::{Digest, Sha256};

    let mut session = Session::new();
    let marker = "RESTORED_PANE_HISTORY";
    let output = run_cli(
        &session.socket,
        &[
            "pane",
            "run",
            &session.pane_id,
            "printf '%s%s\\n' RESTORED_ PANE_HISTORY",
        ],
    );
    assert!(output.status.success(), "{output:?}");
    assert!(wait_until(
        Duration::from_secs(10),
        Duration::from_millis(50),
        || { pane_read_recent_contains(&session.socket, &session.pane_id, marker) }
    ));
    session.stop();

    let snapshot_path = session.data_dir().join("session.json");
    let history_path = session.data_dir().join("session-history.json");
    let mut snapshot: Value = serde_json::from_slice(&fs::read(&snapshot_path).unwrap()).unwrap();
    for workspace in snapshot["workspaces"].as_array_mut().unwrap() {
        for tab in workspace["tabs"].as_array_mut().unwrap() {
            for pane in tab["panes"].as_object_mut().unwrap().values_mut() {
                pane.as_object_mut().unwrap().remove("ended_agent");
            }
        }
    }
    // Compute the old writer's fingerprint from its JSON, before any current
    // Rust defaults can add a field the old writer never had.
    let bytes = serde_json::to_vec(&snapshot).unwrap();
    let mut history: Value = serde_json::from_slice(&fs::read(&history_path).unwrap()).unwrap();
    history["layout_fingerprint"] = json!(format!("{:x}", Sha256::digest(&bytes)));
    fs::write(snapshot_path, bytes).unwrap();
    fs::write(history_path, serde_json::to_vec(&history).unwrap()).unwrap();

    session.start();
    assert_eq!(session.pane().get("ended_agent"), Some(&Value::Null));
    assert!(
        pane_read_recent_contains(&session.socket, &session.pane_id, marker),
        "a snapshot without ended_agent must retain its saved history"
    );
}

#[test]
fn ended_agent_survives_live_handoff() {
    let mut session = Session::new();
    let agent = session.agent("pi", "first");
    agent.stop();
    let ended =
        session.wait_for(|pane| pane["agent"].is_null() && pane["agent_status"] == "unknown");
    assert_eq!(ended["ended_agent"], "pi");
    let handoff = run_cli(&session.socket, &["server", "live-handoff"]);
    assert!(handoff.status.success(), "{handoff:?}");
    drop(session.server.take());
    assert!(wait_until(
        Duration::from_secs(10),
        Duration::from_millis(50),
        || {
            run_cli(&session.socket, &["pane", "get", &session.pane_id])
                .status
                .success()
        }
    ));
    let restored = session.pane();
    assert_ne!(restored["terminal_id"], ended["terminal_id"]);
    assert_eq!(restored["ended_agent"], "pi");
    assert_eq!(
        restored["agent_status_changed_at"],
        ended["agent_status_changed_at"]
    );
    let next = session.agent("pi", "next");
    assert_eq!(session.pane().get("ended_agent"), Some(&Value::Null));
    next.stop();
    session.stop();
}

#[test]
fn ended_agent_keeps_a_self_reported_kind_when_its_process_exits() {
    let session = Session::new();
    let reporter = InPaneReporter::write(&session.base, "custom-runner", &session.socket);
    let output = run_cli(
        &session.socket,
        &["pane", "run", &session.pane_id, &reporter.command()],
    );
    assert!(output.status.success(), "{output:?}");
    reporter.wait_until_started(Duration::from_secs(10));
    reporter.ok(&[
        "pane",
        "report-agent",
        &session.pane_id,
        "--source",
        "custom:runner",
        "--agent",
        "custom-kind",
        "--state",
        "idle",
    ]);
    let active = session.wait_for(|pane| pane["agent"] == "custom-kind");
    assert_eq!(active.get("ended_agent"), Some(&Value::Null));
    reporter.stop();
    let ended =
        session.wait_for(|pane| pane["agent"].is_null() && pane["agent_status"] == "unknown");
    assert_eq!(ended.get("ended_agent"), Some(&active["agent"]));
}

async fn next_pane_event(
    events: &mut BufReader<UnixStream>,
    event: &str,
    id: &str,
    matches: impl Fn(&Value) -> bool,
) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let value = read_event(events).await;
            let pane = &value["data"]["pane"];
            if value["event"] == event && pane["pane_id"] == id && matches(pane) {
                return pane.clone();
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("missing {event} for {id}"))
}

async fn subscribe_to_updates(session: &Session) -> BufReader<UnixStream> {
    let mut stream = UnixStream::connect(&session.socket).unwrap();
    writeln!(stream, "{}", json!({"id":"updates","method":"events.subscribe","params":{"subscriptions":[{"type":"pane.updated"}]}})).unwrap();
    stream.set_nonblocking(true).unwrap();
    let mut events = BufReader::new(stream);
    let ack = tokio::time::timeout(Duration::from_secs(10), read_event(&mut events))
        .await
        .unwrap();
    assert_eq!(ack["result"]["type"], "subscription_started");
    events
}

async fn last_update_before_barrier(
    session: &Session,
    events: &mut BufReader<UnixStream>,
) -> Value {
    // A different pane's update bounds the stream without changing the pane
    // under test or relying on a quiet interval to infer that delivery finished.
    let created = run_cli_json(&session.socket, &["tab", "create"]);
    let barrier = created["result"]["root_pane"]["pane_id"].as_str().unwrap();
    let titled = run_cli(
        &session.socket,
        &[
            "pane",
            "run",
            barrier,
            "printf '\\033]0;event-barrier\\007'",
        ],
    );
    assert!(titled.status.success(), "{titled:?}");
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut last = Value::Null;
        loop {
            let event = read_event(events).await;
            assert_eq!(event["event"], "pane_updated");
            let pane = &event["data"]["pane"];
            if pane["pane_id"] == barrier && pane["terminal_title"] == "event-barrier" {
                return last;
            }
            if pane["pane_id"] == session.pane_id {
                last = pane.clone();
            }
        }
    })
    .await
    .expect("missing barrier pane update")
}

#[tokio::test]
async fn ended_agent_exit_update_carries_the_final_shell_timestamp() {
    let session = Session::new();
    let agent = session.agent("pi", "idle-agent");
    let active = session.wait_for(|pane| pane["agent_status"] == "idle");
    assert!(active["label"].is_null());
    let mut events = subscribe_to_updates(&session).await;
    agent.stop();
    let ended =
        session.wait_for(|pane| pane["agent"].is_null() && pane["agent_status"] == "unknown");
    assert_eq!(ended["ended_agent"], "pi");
    assert!(
        ended["agent_status_changed_at"].as_u64().unwrap()
            > active["agent_status_changed_at"].as_u64().unwrap()
    );
    let last = last_update_before_barrier(&session, &mut events).await;
    assert_eq!(last["ended_agent"], "pi");
    assert_eq!(
        last["agent_status_changed_at"],
        session.pane()["agent_status_changed_at"]
    );
    assert_eq!(last["agent_status"], "unknown");
}

#[tokio::test]
async fn ended_agent_command_respawn_publishes_identity_without_a_status_change() {
    let mut session = Session::new();
    let reporter = InPaneReporter::write(&session.base, "custom-command", &session.socket);
    let mut stream = UnixStream::connect(&session.socket).unwrap();
    writeln!(
        stream,
        "{}",
        json!({
            "id":"command-pane", "method":"layout.apply",
            "params":{"root":{"type":"pane", "command":["/bin/sh", reporter.command()]}}
        })
    )
    .unwrap();
    stream.set_nonblocking(true).unwrap();
    let response = tokio::time::timeout(
        Duration::from_secs(10),
        read_event(&mut BufReader::new(stream)),
    )
    .await
    .unwrap();
    assert!(response.get("error").is_none(), "{response}");
    session.pane_id = response["result"]["layout"]["root"]["pane_id"]
        .as_str()
        .unwrap()
        .to_owned();
    reporter.wait_until_started(Duration::from_secs(10));

    let handoff = run_cli(&session.socket, &["server", "live-handoff"]);
    assert!(handoff.status.success(), "{handoff:?}");
    drop(session.server.take());
    assert!(wait_until(
        Duration::from_secs(10),
        Duration::from_millis(50),
        || {
            run_cli(&session.socket, &["pane", "get", &session.pane_id])
                .status
                .success()
        }
    ));
    reporter.ok(&[
        "pane",
        "report-agent",
        &session.pane_id,
        "--source",
        "custom:command",
        "--agent",
        "custom-kind",
        "--state",
        "unknown",
    ]);
    let active = session.wait_for(|pane| pane["agent"] == "custom-kind");
    assert_eq!(active["agent_status"], "unknown");
    assert_eq!(active.get("ended_agent"), Some(&Value::Null));
    let mut events = subscribe_to_updates(&session).await;
    reporter.stop();
    let ended =
        session.wait_for(|pane| pane["agent"].is_null() && pane["ended_agent"] == "custom-kind");
    assert_eq!(ended["agent_status"], "unknown");
    assert_eq!(
        ended["agent_status_changed_at"],
        active["agent_status_changed_at"]
    );

    let output = run_cli(
        &session.socket,
        &[
            "pane",
            "run",
            &session.pane_id,
            "printf '%s%s\\n' RESPAWNED_ SHELL",
        ],
    );
    assert!(output.status.success(), "{output:?}");
    assert!(wait_until(
        Duration::from_secs(10),
        Duration::from_millis(50),
        || { pane_read_recent_contains(&session.socket, &session.pane_id, "RESPAWNED_SHELL") }
    ));
    let last = last_update_before_barrier(&session, &mut events).await;
    assert_eq!(last["ended_agent"], "custom-kind");
    assert!(last["agent"].is_null());
    assert_eq!(last["agent_status"], "unknown");
    assert_eq!(
        last["agent_status_changed_at"],
        session.pane()["agent_status_changed_at"]
    );

    let next = session.agent("pi", "next");
    let cleared = last_update_before_barrier(&session, &mut events).await;
    assert_eq!(cleared.get("ended_agent"), Some(&Value::Null));
    assert_eq!(cleared["agent"], "pi");
    next.stop();
    session.stop();
}

#[tokio::test]
async fn ended_agent_is_explicit_in_created_list_snapshot_and_updated_payloads() {
    let session = Session::new();
    assert_eq!(session.pane().get("ended_agent"), Some(&Value::Null));
    let mut stream = UnixStream::connect(&session.socket).unwrap();
    let request = json!({"id":"sub","method":"events.subscribe","params":{"subscriptions":[{"type":"pane.created"},{"type":"pane.updated"}]}});
    writeln!(stream, "{request}").unwrap();
    stream.set_nonblocking(true).unwrap();
    let mut events = BufReader::new(stream);
    let ack = tokio::time::timeout(Duration::from_secs(10), read_event(&mut events))
        .await
        .unwrap();
    assert_eq!(ack["result"]["type"], "subscription_started");

    let created = run_cli_json(&session.socket, &["tab", "create"]);
    let created_pane = &created["result"]["root_pane"];
    assert_eq!(created_pane.get("ended_agent"), Some(&Value::Null));
    let event = next_pane_event(
        &mut events,
        "pane_created",
        created_pane["pane_id"].as_str().unwrap(),
        |_| true,
    )
    .await;
    assert_eq!(event.get("ended_agent"), Some(&Value::Null));

    let agent = session.agent("pi", "first");
    let active = next_pane_event(&mut events, "pane_updated", &session.pane_id, |pane| {
        pane["agent"] == "pi"
    })
    .await;
    assert_eq!(active.get("ended_agent"), Some(&Value::Null));
    agent.stop();
    let ended = next_pane_event(&mut events, "pane_updated", &session.pane_id, |pane| {
        pane["ended_agent"] == "pi"
    })
    .await;
    assert!(ended["agent"].is_null());

    for args in [&["pane", "list"][..], &["api", "snapshot"][..]] {
        let response = run_cli_json(&session.socket, args);
        let panes = if args[0] == "pane" {
            &response["result"]["panes"]
        } else {
            &response["result"]["snapshot"]["panes"]
        };
        let pane = panes
            .as_array()
            .unwrap()
            .iter()
            .find(|pane| pane["pane_id"] == session.pane_id)
            .unwrap();
        assert_eq!(pane["ended_agent"], "pi");
        assert!(panes
            .as_array()
            .unwrap()
            .iter()
            .all(|pane| pane.get("ended_agent").is_some()));
    }

    let next = session.agent("pi", "next");
    let active = next_pane_event(&mut events, "pane_updated", &session.pane_id, |pane| {
        pane["agent"] == "pi"
    })
    .await;
    assert_eq!(active.get("ended_agent"), Some(&Value::Null));
    next.stop();
}

// Read without blocking the async test; retain partial JSON across WouldBlock.
async fn read_event(events: &mut BufReader<UnixStream>) -> Value {
    let mut line = Vec::new();
    loop {
        match events.read_until(b'\n', &mut line) {
            Ok(0) => panic!("event stream closed"),
            Ok(_) => return serde_json::from_slice(&line).unwrap(),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(error) => panic!("event stream failed: {error}"),
        }
    }
}
