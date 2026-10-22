use super::harness::*;
use crate::support::InPaneReporter;
use serde_json::{json, Value};

struct Session {
    base: PathBuf,
    socket: PathBuf,
    server: Option<SpawnedHerdr>,
    agent: InPaneReporter,
    starter: String,
}

impl Session {
    fn new() -> Self {
        Self::with_config("onboarding = false\n")
    }

    fn with_config(config: &str) -> Self {
        let base = unique_test_dir();
        fs::create_dir_all(&base).unwrap();
        create_committed_repo(&base.join("repo"));
        let socket = base.join("runtime/herdr.sock");
        let agent = InPaneReporter::write(&base, "pi", &socket);
        let server = spawn_herdr_with_config(
            &base.join("config"),
            &base.join("runtime"),
            &socket,
            None,
            config,
        );
        wait_for_socket(&socket, Duration::from_secs(5));
        let created = run_cli_json(
            &socket,
            &[
                "workspace",
                "create",
                "--cwd",
                base.join("repo").to_str().unwrap(),
            ],
        );
        let starter = created["result"]["root_pane"]["pane_id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            created["result"]["root_pane"].get("started_by"),
            Some(&Value::Null)
        );
        assert!(
            run_cli(&socket, &["pane", "run", &starter, &agent.command()])
                .status
                .success()
        );
        agent.wait_until_started(Duration::from_secs(5));
        assert!(wait_until(
            Duration::from_secs(5),
            Duration::from_millis(50),
            || {
                run_cli_json(&socket, &["pane", "get", &starter])["result"]["pane"]["agent"] == "pi"
            }
        ));
        Self {
            base,
            socket,
            server: Some(server),
            agent,
            starter,
        }
    }

    fn create(&self, args: &[&str], pane_key: &str) -> Value {
        let checkout = self.base.join("checkout");
        let mut args = args.to_vec();
        if args[0] == "worktree" {
            args.extend(["--path", checkout.to_str().unwrap()]);
        }
        let child: Value = serde_json::from_str(&self.agent.ok(&args)).unwrap();
        let pane = child["result"][pane_key].clone();
        assert_eq!(
            pane["started_by"],
            json!({"pane_id": self.starter}),
            "{child}"
        );
        pane
    }

    fn pane(&self, id: &str) -> Value {
        run_cli_json(&self.socket, &["pane", "get", id])["result"]["pane"].clone()
    }

    fn stop(&mut self) {
        assert!(run_cli(&self.socket, &["server", "stop"]).status.success());
        if let Some(mut server) = self.server.take() {
            let pid = server.child.process_id();
            assert!(server.child.wait().unwrap().success());
            unregister_spawned_herdr_pid(pid);
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        drop(self.server.take());
        cleanup_test_base(&self.base);
    }
}

#[test]
fn started_by_tab_create_from_agent_pane() {
    Session::new().create(&["tab", "create"], "root_pane");
}

#[test]
fn started_by_split_from_agent_pane() {
    Session::new().create(&["pane", "split", "--direction", "right"], "pane");
}

#[test]
fn started_by_workspace_create_from_agent_pane() {
    Session::new().create(&["workspace", "create"], "root_pane");
}

#[test]
fn started_by_worktree_create_from_agent_pane() {
    Session::new().create(&["worktree", "create", "--branch", "child"], "root_pane");
}

#[test]
fn started_by_survives_cold_restore() {
    let mut session = Session::new();
    let before = session.create(&["tab", "create"], "root_pane");
    session.stop();
    session.server = Some(spawn_herdr(
        &session.base.join("config"),
        &session.base.join("runtime"),
        &session.socket,
    ));
    wait_for_socket(&session.socket, Duration::from_secs(5));
    let restored = session.pane(before["pane_id"].as_str().unwrap());
    assert_ne!(restored["terminal_id"], before["terminal_id"]);
    assert_eq!(restored["started_by"], before["started_by"]);
}

#[test]
fn started_by_survives_live_handoff() {
    let mut session = Session::new();
    let before = session.create(&["tab", "create"], "root_pane");
    let handoff = run_cli(&session.socket, &["server", "live-handoff"]);
    assert!(
        handoff.status.success(),
        "{}",
        String::from_utf8_lossy(&handoff.stderr)
    );
    drop(session.server.take());
    assert!(wait_until(
        Duration::from_secs(10),
        Duration::from_millis(50),
        || {
            run_cli(
                &session.socket,
                &["pane", "get", before["pane_id"].as_str().unwrap()],
            )
            .status
            .success()
        }
    ));
    let restored = session.pane(before["pane_id"].as_str().unwrap());
    // Stop the imported server as well as the original, even if an assertion fails.
    session.stop();
    assert_ne!(restored["terminal_id"], before["terminal_id"]);
    assert_eq!(restored["started_by"], before["started_by"]);
}

fn create_lanes<'a>(
    starter: &'a str,
    workspace: &'a str,
    checkout: &'a str,
) -> Vec<(&'a str, Vec<&'a str>)> {
    vec![
        ("root_pane", vec!["tab", "create", "--workspace", workspace]),
        (
            "pane",
            vec!["pane", "split", starter, "--direction", "right"],
        ),
        ("root_pane", vec!["workspace", "create"]),
        (
            "root_pane",
            vec![
                "worktree",
                "create",
                "--workspace",
                workspace,
                "--branch",
                "negative",
                "--path",
                checkout,
            ],
        ),
    ]
}

#[test]
fn started_by_refuses_forged_environment_from_outside() {
    let session = Session::new();
    session.create(&["tab", "create"], "root_pane");
    let workspace = session.starter.split(':').next().unwrap();
    let checkout = session.base.join("outside-checkout");
    for (key, args) in create_lanes(&session.starter, workspace, checkout.to_str().unwrap()) {
        let output = Command::new(env!("CARGO_BIN_EXE_herdr"))
            .args(&args)
            .env("HERDR_SOCKET_PATH", &session.socket)
            .env("HERDR_PANE_ID", &session.starter)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let response = parse_cli_json_output(&args, output);
        assert_eq!(
            response["result"][key].get("started_by"),
            Some(&Value::Null),
            "{response}"
        );
    }
}

#[test]
fn started_by_plain_shell_caller_has_no_starter() {
    let session = Session::new();
    session.create(&["tab", "create"], "root_pane");
    let plain = InPaneReporter::write(&session.base, "plain-shell", &session.socket);
    let created = run_cli_json(&session.socket, &["tab", "create"]);
    let caller = created["result"]["root_pane"]["pane_id"].as_str().unwrap();
    assert!(
        run_cli(&session.socket, &["pane", "run", caller, &plain.command()])
            .status
            .success()
    );
    plain.wait_until_started(Duration::from_secs(5));
    assert!(session.pane(caller)["agent"].is_null());
    let checkout = session.base.join("shell-checkout");
    for (key, args) in create_lanes(caller, "w1", checkout.to_str().unwrap()) {
        let response: Value = serde_json::from_str(&plain.ok(&args)).unwrap();
        assert_eq!(
            response["result"][key].get("started_by"),
            Some(&Value::Null),
            "{response}"
        );
    }
}

#[test]
fn started_by_plugin_panes_are_roots_even_when_opened_by_an_agent() {
    let session = Session::new();
    session.create(&["tab", "create"], "root_pane");
    let plugin = session.base.join("plugin");
    fs::create_dir_all(&plugin).unwrap();
    fs::write(
        plugin.join("herdr-plugin.toml"),
        r#"id = "example.starter"
name = "Starter test"
version = "0.1.0"
min_herdr_version = "0.6.10"
[[panes]]
id = "board"
title = "Board"
placement = "tab"
command = ["sh", "-c", "sleep 60"]
"#,
    )
    .unwrap();
    run_cli_json(
        &session.socket,
        &["plugin", "link", plugin.to_str().unwrap()],
    );
    let opened: Value = serde_json::from_str(&session.agent.ok(&[
        "plugin",
        "pane",
        "open",
        "--plugin",
        "example.starter",
        "--entrypoint",
        "board",
    ]))
    .unwrap();
    let pane_id = opened["result"]["plugin_pane"]["pane"]["pane_id"]
        .as_str()
        .unwrap();
    assert_eq!(session.pane(pane_id).get("started_by"), Some(&Value::Null));
}

fn next_event(reader: &mut BufReader<UnixStream>, event: &str, pane_id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut seen = Vec::new();
    loop {
        assert!(Instant::now() < deadline, "missing {event} for {pane_id}");
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .unwrap_or_else(|e| panic!("waiting for {event}: {e}; received {seen:?}"));
        let value: Value = serde_json::from_str(&line).unwrap();
        if value["event"] == event && value["data"]["pane"]["pane_id"] == pane_id {
            return value["data"]["pane"].clone();
        }
        seen.push(value);
    }
}

#[test]
fn started_by_reaches_created_list_snapshot_and_updated_payloads() {
    let session = Session::new();
    let mut stream = UnixStream::connect(&session.socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    writeln!(stream, "{}", json!({"id":"sub","method":"events.subscribe","params":{"subscriptions":[{"type":"pane.created"},{"type":"pane.updated"}]}})).unwrap();
    let mut reader = BufReader::new(stream);
    let mut ack = String::new();
    reader.read_line(&mut ack).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&ack).unwrap()["result"]["type"],
        "subscription_started"
    );
    let pane = session.create(&["tab", "create"], "root_pane");
    let id = pane["pane_id"].as_str().unwrap();
    assert_eq!(
        next_event(&mut reader, "pane_created", id)["started_by"],
        pane["started_by"]
    );
    for args in [&["pane", "list"][..], &["api", "snapshot"][..]] {
        let response = run_cli_json(&session.socket, args);
        let panes = if args[0] == "pane" {
            &response["result"]["panes"]
        } else {
            &response["result"]["snapshot"]["panes"]
        };
        let found = panes
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["pane_id"] == id)
            .unwrap();
        assert_eq!(found["started_by"], pane["started_by"], "{response}");
    }
    assert!(run_cli(
        &session.socket,
        &["pane", "run", id, "printf '\\033]0;child-title\\007'"]
    )
    .status
    .success());
    assert_eq!(
        next_event(&mut reader, "pane_updated", id)["started_by"],
        pane["started_by"]
    );
}

fn create_requests(session: &Session) -> Vec<(&'static str, Value)> {
    let caller = &session.starter;
    vec![
        (
            "root_pane",
            json!({"id":"tab","method":"tab.create","params":{"caller_pane_id":caller}}),
        ),
        (
            "pane",
            json!({"id":"split","method":"pane.split","params":{"caller_pane_id":caller,"target_pane_id":caller,"direction":"right"}}),
        ),
        (
            "root_pane",
            json!({"id":"workspace","method":"workspace.create","params":{"caller_pane_id":caller}}),
        ),
        (
            "root_pane",
            json!({"id":"worktree","method":"worktree.create","params":{"caller_pane_id":caller,"workspace_id":"w1","branch":"transport","path":session.base.join("transport-checkout")}}),
        ),
    ]
}

#[test]
fn started_by_websocket_cannot_claim_a_local_agent_pane() {
    use std::net::TcpListener;
    use tungstenite::client::IntoClientRequest;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let session = Session::with_config(&format!(
        "onboarding = false\n[websocket_api]\nbind = \"{addr}\"\ntoken = \"test-starter-token\"\n"
    ));
    session.create(&["tab", "create"], "root_pane");
    let mut request = format!("ws://{addr}/").into_client_request().unwrap();
    request.headers_mut().insert(
        "Authorization",
        "Bearer test-starter-token".parse().unwrap(),
    );
    let (mut ws, _) = tungstenite::connect(request).unwrap();
    if let tungstenite::stream::MaybeTlsStream::Plain(stream) = ws.get_mut() {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
    }
    for (key, mut request) in create_requests(&session) {
        // Transport provenance is server-owned, even if JSON claims otherwise.
        request["params"]["peer"] = json!({"pid":1,"placement":{"tty":0,"ancestry":[1]}});
        ws.send(tungstenite::Message::text(request.to_string()))
            .unwrap();
        let response: Value = serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(
            response["result"][key].get("started_by"),
            Some(&Value::Null),
            "{response}"
        );
    }
    ws.close(None).unwrap();
}

fn endpoint_control(stream: &mut UnixStream, expected_kind: &str) -> Value {
    loop {
        let (tag, payload) = crate::support::read_server_message(stream).unwrap();
        if tag == crate::support::SERVER_MESSAGE_ENDPOINT_CONTROL {
            let ((kind, data), _): ((String, String), usize) =
                bincode::decode_from_slice(&payload, bincode::config::standard()).unwrap();
            if kind == expected_kind {
                return serde_json::from_str(&data).unwrap();
            }
        }
    }
}

#[test]
fn started_by_client_shell_requests_have_no_starter() {
    let session = Session::new();
    session.create(&["tab", "create"], "root_pane");
    let mut client = UnixStream::connect(session.base.join("runtime/herdr-client.sock")).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let (_, error) = crate::support::client_shell_handshake(
        &mut client,
        crate::support::CURRENT_ENDPOINT_PROTOCOL_GENERATION,
        80,
        24,
    )
    .unwrap();
    assert!(error.is_none(), "{error:?}");
    let snapshot = endpoint_control(&mut client, "shell.snapshot.v1");
    let boot_id = snapshot["boot_id"].as_str().unwrap();
    for (key, request) in create_requests(&session) {
        // Generation-1 ClientShellEndpointRequest: frozen tag followed by strings.
        let payload = bincode::encode_to_vec(
            (15u32, boot_id, request.to_string()),
            bincode::config::standard(),
        )
        .unwrap();
        client
            .write_all(&(payload.len() as u32).to_le_bytes())
            .unwrap();
        client.write_all(&payload).unwrap();
        let mut response = Vec::new();
        loop {
            let (tag, payload) = crate::support::read_server_message(&mut client).unwrap();
            if tag == 18 {
                let ((_, id, final_chunk, data), _): ((String, String, bool, Vec<u8>), usize) =
                    bincode::decode_from_slice(&payload, bincode::config::standard()).unwrap();
                assert_eq!(id, request["id"].as_str().unwrap());
                response.extend(data);
                if final_chunk {
                    break;
                }
            }
        }
        let response: Value = serde_json::from_slice(&response).unwrap();
        assert_eq!(
            response["result"][key].get("started_by"),
            Some(&Value::Null),
            "{response}"
        );
    }
}

#[test]
fn started_by_machine_routing_drops_the_local_caller() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let session = Session::new();
    let bin = session.base.join("bin");
    fs::create_dir_all(&bin).unwrap();
    // Replace only the SSH transport. Discovery, CLI routing, stdio forwarding,
    // socket peer placement and pane creation all run their production code.
    fs::write(
        bin.join("ssh"),
        r#"#!/bin/sh
for arg do
    last=$arg
    if [ "$arg" = -G ]; then printf 'controlpath none\n'; exit 0; fi
done
case "$last" in
    exit) exit 0 ;;
    '/bin/sh -s')
        script=$(cat)
        case "$script" in
            *'uname -s'*) printf 'herdr-remote-output-ready:1\n'; uname -s; uname -m ;;
            *) exec /bin/sh -c "$script" ;;
        esac ;;
    '/bin/sh -c '*) exec /bin/sh -c "$last" ;;
    *) printf 'unexpected transport invocation\n' >&2; exit 2 ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(bin.join("ssh"), fs::Permissions::from_mode(0o755)).unwrap();
    symlink(env!("CARGO_BIN_EXE_herdr"), bin.join("herdr")).unwrap();
    let state = session.base.join("state");
    let catalog = state.join(app_dir_name()).join("client");
    fs::create_dir_all(&catalog).unwrap();
    fs::write(catalog.join("endpoints.json"), json!({"version":1,"ssh":[{
        "id":"0123456789abcdef0123456789abcdef", "label":"loopback", "target":"test-transport.invalid", "session":"isolated", "enabled":true
    }]}).to_string()).unwrap();
    let remote_session = session
        .base
        .join("config")
        .join(app_dir_name())
        .join("sessions/isolated");
    fs::create_dir_all(&remote_session).unwrap();
    symlink(&session.socket, remote_session.join("herdr.sock")).unwrap();
    let agent_dir = session.base.join("remote-agent");
    fs::create_dir_all(&agent_dir).unwrap();
    let agent = InPaneReporter::write(&agent_dir, "pi", &session.socket);
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let script = agent_dir.join("pi");
    let body = fs::read_to_string(&script).unwrap();
    fs::write(
        &script,
        body.replacen(
            "#!/bin/sh\n",
            &format!(
                "#!/bin/sh\nexport PATH={}\nexport XDG_STATE_HOME={}\n",
                shell_quote(&path),
                shell_quote(state.to_str().unwrap())
            ),
            1,
        ),
    )
    .unwrap();
    let created = run_cli_json(
        &session.socket,
        &[
            "tab",
            "create",
            "--env",
            &format!("PATH={path}"),
            "--env",
            &format!("XDG_STATE_HOME={}", state.display()),
        ],
    );
    let caller = created["result"]["root_pane"]["pane_id"].as_str().unwrap();
    assert!(
        run_cli(&session.socket, &["pane", "run", caller, &agent.command()])
            .status
            .success()
    );
    agent.wait_until_started(Duration::from_secs(5));
    assert!(wait_until(
        Duration::from_secs(5),
        Duration::from_millis(50),
        || session.pane(caller)["agent"] == "pi"
    ));
    let direct: Value = serde_json::from_str(&agent.ok(&["tab", "create"])).unwrap();
    assert_eq!(
        direct["result"]["root_pane"]["started_by"],
        json!({"pane_id":caller})
    );
    let checkout = session.base.join("machine-checkout");
    for (key, args) in create_lanes(caller, "w1", checkout.to_str().unwrap()) {
        let mut routed = vec!["--machine", "loopback"];
        routed.extend(args);
        let response: Value = serde_json::from_str(&agent.ok(&routed)).unwrap();
        assert_eq!(
            response["result"][key].get("started_by"),
            Some(&Value::Null),
            "{response}"
        );
    }
}

#[test]
fn started_by_stdio_forward_cannot_claim_a_local_agent_pane() {
    let session = Session::new();
    session.create(&["tab", "create"], "root_pane");
    for (key, request) in create_requests(&session) {
        let mut bridge = Command::new(env!("CARGO_BIN_EXE_herdr"))
            .arg("remote-api-bridge")
            .env("HERDR_SOCKET_PATH", &session.socket)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        writeln!(bridge.stdin.take().unwrap(), "{request}").unwrap();
        let output = bridge.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        let response: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            response["result"][key].get("started_by"),
            Some(&Value::Null),
            "{response}"
        );
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
