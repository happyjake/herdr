use super::harness::*;
use crate::support::InPaneReporter;
use serde_json::{json, Value};

struct Session {
    base: PathBuf,
    socket: PathBuf,
    server: Option<SpawnedHerdr>,
    agent: InPaneReporter,
    starter: String,
    path: Option<String>,
}

impl Session {
    fn new() -> Self {
        Self::with_config("onboarding = false\n")
    }

    fn with_config(config: &str) -> Self {
        Self::spawn(config, |_| None)
    }

    fn with_agent_setup(config: &str, setup: impl FnOnce(&Path)) -> Self {
        Self::build(config, setup, |_| None)
    }

    /// A session whose server runs with the PATH `path` makes in its base dir.
    fn spawn(config: &str, path: impl FnOnce(&Path) -> Option<String>) -> Self {
        Self::build(config, |_| {}, path)
    }

    fn build(
        config: &str,
        setup: impl FnOnce(&Path),
        path: impl FnOnce(&Path) -> Option<String>,
    ) -> Self {
        // The clock alone can name two parallel sessions alike.
        static SESSIONS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = SESSIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let base = PathBuf::from(format!("{}-{n}", unique_test_dir().display()));
        fs::create_dir_all(&base).unwrap();
        create_committed_repo(&base.join("repo"));
        let socket = base.join("runtime/herdr.sock");
        let agent = InPaneReporter::write(&base, "pi", &socket);
        setup(&base);
        let path = path(&base);
        let server = spawn_herdr_with_config(
            &base.join("config"),
            &base.join("runtime"),
            &socket,
            path.as_deref().map(Path::new),
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
            path,
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
fn started_by_nested_pty_command_runner_keeps_the_agent_starter() {
    let session = Session::with_agent_setup("onboarding = false\n", |base| {
        let runner = base.join("nested-runner.sh");
        fs::write(&runner, "tty > \"$1.inner-tty\"\nexec /bin/sh \"$1\"\n").unwrap();
        let command = format!(
            "/bin/sh {} \"$inbox/$n.cmd\"",
            shell_quote(runner.to_str().unwrap())
        );
        #[cfg(target_os = "linux")]
        let command = format!("script -q -c {} /dev/null", shell_quote(&command));
        #[cfg(not(target_os = "linux"))]
        let command = format!("script -q /dev/null {command}");
        let script = base.join("pi");
        let body = fs::read_to_string(&script).unwrap();
        let direct = "/bin/sh \"$inbox/$n.cmd\"";
        assert!(body.contains(direct));
        fs::write(
            &script,
            body.replace(
                direct,
                &format!("export inbox n\n    tty > \"$inbox/$n.cmd.outer-tty\"\n    {command}"),
            ),
        )
        .unwrap();
    });
    let child: Value = serde_json::from_str(&session.agent.ok(&["tab", "create"])).unwrap();
    let outer = fs::read_to_string(session.base.join("pi-inbox/0.cmd.outer-tty")).unwrap();
    let inner = fs::read_to_string(session.base.join("pi-inbox/0.cmd.inner-tty")).unwrap();
    assert!(outer.trim().starts_with("/dev/"), "{outer}");
    assert!(inner.trim().starts_with("/dev/"), "{inner}");
    assert_ne!(
        inner, outer,
        "the command runner must actually allocate another PTY"
    );
    assert_eq!(
        child["result"]["root_pane"]["started_by"],
        json!({"pane_id":session.starter}),
        "{child}"
    );
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

impl Session {
    fn subscribe(&self, kinds: &[&str]) -> BufReader<UnixStream> {
        let mut stream = UnixStream::connect(&self.socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let subscriptions: Vec<Value> = kinds.iter().map(|kind| json!({"type": kind})).collect();
        writeln!(
            stream,
            "{}",
            json!({"id":"sub","method":"events.subscribe","params":{"subscriptions":subscriptions}})
        )
        .unwrap();
        let mut reader = BufReader::new(stream);
        let mut ack = String::new();
        reader.read_line(&mut ack).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&ack).unwrap()["result"]["type"],
            "subscription_started"
        );
        reader
    }

    /// Starts a stand-in agent in `pane`, so the pane can start panes of its own.
    fn agent_in(&self, pane: &str, name: &str) -> InPaneReporter {
        self.agent_kind_in(pane, name, "pi")
    }

    fn agent_kind_in(&self, pane: &str, name: &str, kind: &str) -> InPaneReporter {
        let dir = self.base.join(name);
        fs::create_dir_all(&dir).unwrap();
        let agent = InPaneReporter::write(&dir, kind, &self.socket);
        let run = run_cli(&self.socket, &["pane", "run", pane, &agent.command()]);
        assert!(run.status.success(), "{run:?}");
        agent.wait_until_started(Duration::from_secs(5));
        assert!(wait_until(
            Duration::from_secs(5),
            Duration::from_millis(50),
            || self.pane(pane)["agent"] == kind
        ));
        agent
    }

    fn tab_of(&self, pane: &str) -> String {
        self.pane(pane)["tab_id"].as_str().unwrap().to_owned()
    }

    fn restart(&mut self) {
        self.stop();
        self.server = Some(spawn_herdr_with_config(
            &self.base.join("config"),
            &self.base.join("runtime"),
            &self.socket,
            self.path.as_deref().map(Path::new),
            "onboarding = false\n",
        ));
        wait_for_socket(&self.socket, Duration::from_secs(5));
    }

    fn live_handoff(&mut self, pane: &str) {
        let handoff = run_cli(&self.socket, &["server", "live-handoff"]);
        assert!(
            handoff.status.success(),
            "{}",
            String::from_utf8_lossy(&handoff.stderr)
        );
        drop(self.server.take());
        assert!(wait_until(
            Duration::from_secs(10),
            Duration::from_millis(50),
            || run_cli(&self.socket, &["pane", "get", pane])
                .status
                .success()
        ));
    }
}

/// Reads events until `pane`'s update carries `started_by`, returning every
/// event seen on the way so callers can check what preceded the rewrite.
fn until_started_by(
    reader: &mut BufReader<UnixStream>,
    pane: &str,
    started_by: &Value,
) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut seen = Vec::new();
    loop {
        assert!(
            Instant::now() < deadline,
            "no update of {pane} to {started_by}; received {seen:?}"
        );
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .unwrap_or_else(|e| panic!("waiting for {pane}: {e}; received {seen:?}"));
        let value: Value = serde_json::from_str(&line).unwrap();
        let done = value["event"] == "pane_updated"
            && value["data"]["pane"]["pane_id"] == pane
            && &value["data"]["pane"]["started_by"] == started_by;
        seen.push(value);
        if done {
            return seen;
        }
    }
}

fn saw(events: &[Value], event: &str, pane: &str) -> bool {
    events.iter().any(|value| {
        value["event"] == event
            && (value["data"]["pane_id"] == pane || value["data"]["previous_pane_id"] == pane)
    })
}

/// Reports a pi session as its integration does, sequenced: the server
/// applies no unsequenced pi session report.
fn report_session(agent: &InPaneReporter, pane: &str, base: &Path, source: &str, seq: &str) {
    let name = format!("{}-{source}-{seq}.jsonl", pane.replace(':', "-"));
    let session = base.join(name);
    fs::write(&session, "").unwrap();
    let args = [
        "pane",
        "report-agent-session",
        pane,
        "--source",
        "herdr:pi",
        "--agent",
        "pi",
        "--agent-session-path",
        session.to_str().unwrap(),
        "--session-start-source",
        source,
        "--seq",
        seq,
    ];
    agent.ok(&args);
}

#[test]
fn started_by_follows_a_starter_moved_to_another_workspace() {
    let mut session = Session::new();
    let child = session.create(&["tab", "create"], "root_pane");
    let child = child["pane_id"].as_str().unwrap().to_owned();
    let mut events = session.subscribe(&["pane.moved", "pane.updated"]);
    let moved = run_cli_json(
        &session.socket,
        &["pane", "move", &session.starter, "--new-workspace"],
    );
    let moved_to = moved["result"]["move_result"]["pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(moved_to, session.starter, "{moved}");
    let expected = json!({"pane_id": moved_to});
    let seen = until_started_by(&mut events, &child, &expected);
    assert!(saw(&seen, "pane_moved", &session.starter), "{seen:?}");
    assert_eq!(session.pane(&child)["started_by"], expected);

    session.live_handoff(&child);
    let handed = session.pane(&child);
    session.stop();
    assert_eq!(handed["started_by"], expected);
}

impl Session {
    fn rename(&self, kind: &str, id: &str, label: &str) {
        assert!(run_cli(&self.socket, &[kind, "rename", id, label])
            .status
            .success());
    }
}

#[test]
fn started_by_closed_starter_leaves_a_marker_that_keeps_the_chain() {
    let mut session = Session::new();
    // A marker names its starter as it was when the link was made: the pane
    // label, else the tab label. Names given later do not reach it.
    let starter_tab = session.tab_of(&session.starter);
    session.rename("tab", &starter_tab, "supervisor");
    let middle = session.create(&["tab", "create"], "root_pane");
    let middle = middle["pane_id"].as_str().unwrap().to_owned();
    let middle_agent = session.agent_in(&middle, "middle");
    session.rename("pane", &middle, "reviewer");
    let leaf: Value = serde_json::from_str(&middle_agent.ok(&["tab", "create"])).unwrap();
    let leaf = leaf["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        session.pane(&leaf)["started_by"],
        json!({"pane_id": middle})
    );
    session.rename("pane", &middle, "renamed reviewer");
    session.rename("tab", &starter_tab, "renamed supervisor");
    let mut events = session.subscribe(&["pane.closed", "tab.closed", "pane.updated"]);

    assert!(run_cli(&session.socket, &["pane", "close", &middle])
        .status
        .success());
    let after_middle =
        json!({"closed": {"label": "reviewer", "started_by": {"pane_id": session.starter}}});
    let seen = until_started_by(&mut events, &leaf, &after_middle);
    assert!(saw(&seen, "pane_closed", &middle), "{seen:?}");

    assert!(run_cli(&session.socket, &["tab", "close", &starter_tab])
        .status
        .success());
    let after_starter =
        json!({"closed": {"label": "reviewer", "started_by": {"closed": {"label": "supervisor"}}}});
    let seen = until_started_by(&mut events, &leaf, &after_starter);
    assert!(
        seen.iter().any(|value| value["event"] == "tab_closed"),
        "{seen:?}"
    );
    assert_eq!(session.pane(&leaf)["started_by"], after_starter);

    session.restart();
    assert_eq!(session.pane(&leaf)["started_by"], after_starter);
    session.live_handoff(&leaf);
    let handed = session.pane(&leaf);
    session.stop();
    assert_eq!(handed["started_by"], after_starter);
}

#[test]
fn started_by_label_at_start_survives_restore_and_handoff_before_close() {
    let mut session = Session::new();
    let starter_tab = session.tab_of(&session.starter);
    session.rename("tab", &starter_tab, "supervisor");
    let child = session.create(&["tab", "create"], "root_pane");
    let child = child["pane_id"].as_str().unwrap().to_owned();
    session.rename("tab", &starter_tab, "renamed supervisor");
    session.restart();
    session.live_handoff(&child);
    assert!(run_cli(&session.socket, &["tab", "close", &starter_tab])
        .status
        .success());
    let closed = session.pane(&child);
    session.stop();
    assert_eq!(
        closed["started_by"],
        json!({"closed": {"label": "supervisor"}})
    );
}

#[test]
fn started_by_starter_shell_exit_leaves_a_closed_marker() {
    let session = Session::new();
    let starter_tab = session.tab_of(&session.starter);
    session.rename("tab", &starter_tab, "supervisor");
    let child = session.create(&["tab", "create"], "root_pane");
    let child = child["pane_id"].as_str().unwrap().to_owned();
    session.rename("tab", &starter_tab, "renamed supervisor");
    let mut events = session.subscribe(&["pane.updated"]);
    session.agent.stop();
    assert!(
        run_cli(&session.socket, &["pane", "run", &session.starter, "exit"])
            .status
            .success()
    );
    let expected = json!({"closed": {"label": "supervisor"}});
    until_started_by(&mut events, &child, &expected);
    assert!(
        !run_cli(&session.socket, &["pane", "get", &session.starter])
            .status
            .success()
    );
    assert_eq!(session.pane(&child)["started_by"], expected);
}

#[test]
fn started_by_first_resume_report_starts_a_new_family() {
    let session = Session::new();
    let child = session.create(&["tab", "create"], "root_pane");
    let child = child["pane_id"].as_str().unwrap().to_owned();
    let agent = session.agent_in(&child, "resumed");
    let mut events = session.subscribe(&["pane.updated"]);
    report_session(&agent, &child, &session.base, "resume", "1");
    until_started_by(&mut events, &child, &Value::Null);
    assert_eq!(session.pane(&child)["started_by"], Value::Null);
}

#[test]
fn started_by_later_resume_report_keeps_the_link_across_restore() {
    // The restored server relaunches the pane's agent to resume its session;
    // a stand-in on its PATH plays that agent.
    let mut session = Session::spawn("onboarding = false\n", |base| {
        Some(format!(
            "{}:{}",
            base.join("restored").display(),
            std::env::var("PATH").unwrap()
        ))
    });
    let child = session.create(&["tab", "create"], "root_pane");
    let child = child["pane_id"].as_str().unwrap().to_owned();
    let expected = json!({"pane_id": session.starter});
    let agent = session.agent_in(&child, "fresh");
    report_session(&agent, &child, &session.base, "startup", "1");
    report_session(&agent, &child, &session.base, "resume", "2");
    assert_eq!(session.pane(&child)["started_by"], expected);

    // A restored agent resuming its own conversation is not a new family.
    let restored_dir = session.base.join("restored");
    fs::create_dir_all(&restored_dir).unwrap();
    let restored_agent = InPaneReporter::write(&restored_dir, "pi", &session.socket);
    session.restart();
    restored_agent.wait_until_started(Duration::from_secs(10));
    assert!(wait_until(
        Duration::from_secs(5),
        Duration::from_millis(50),
        || session.pane(&child)["agent"] == "pi"
    ));
    report_session(&restored_agent, &child, &session.base, "resume", "3");
    let restored = session.pane(&child);
    session.stop();
    assert_eq!(restored["started_by"], expected);
}

fn report_claude(agent: &InPaneReporter, pane: &str, command: &str, extra: &[&str]) {
    let mut args = vec![
        "pane",
        command,
        pane,
        "--source",
        "herdr:claude",
        "--agent",
        "claude",
    ];
    args.extend_from_slice(extra);
    agent.ok(&args);
}

#[test]
fn started_by_stale_resume_report_does_not_decide_the_family() {
    let session = Session::new();
    let child = session.create(&["tab", "create"], "root_pane");
    let child = child["pane_id"].as_str().unwrap().to_owned();
    let agent = session.agent_kind_in(&child, "stale", "claude");
    // A state report sets the pane's sequence and session without being a
    // session report.
    report_claude(
        &agent,
        &child,
        "report-agent",
        &[
            "--state",
            "working",
            "--seq",
            "5",
            "--agent-session-id",
            "earlier",
        ],
    );
    // Older than the pane's last report: refused, so it neither clears the
    // link nor uses up the first-report decision.
    let resume = |seq: &str| {
        report_claude(
            &agent,
            &child,
            "report-agent-session",
            &[
                "--seq",
                seq,
                "--agent-session-id",
                &format!("resumed-{seq}"),
                "--session-start-source",
                "resume",
            ],
        )
    };
    resume("3");
    assert_eq!(
        session.pane(&child)["started_by"],
        json!({"pane_id": session.starter})
    );
    let mut events = session.subscribe(&["pane.updated"]);
    resume("6");
    until_started_by(&mut events, &child, &Value::Null);
    assert_eq!(session.pane(&child)["started_by"], Value::Null);
}

/// A session whose `git worktree add` waits for `release` to appear, after
/// leaving `entered` behind.
fn gated_worktree_session() -> Session {
    use std::os::unix::fs::PermissionsExt;
    Session::spawn("onboarding = false\n", |base| {
        let bin = base.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let real_git = String::from_utf8(
            Command::new("/bin/sh")
                .args(["-c", "command -v git"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        let script = format!(
            "#!/bin/sh\ncase \" $* \" in *' worktree add '*)\n  : > {entered}\n  while [ ! -f {release} ]; do sleep 0.05; done ;;\nesac\nexec {git} \"$@\"\n",
            entered = shell_quote(base.join("entered").to_str().unwrap()),
            release = shell_quote(base.join("release").to_str().unwrap()),
            git = shell_quote(real_git.trim()),
        );
        fs::write(bin.join("git"), script).unwrap();
        fs::set_permissions(bin.join("git"), fs::Permissions::from_mode(0o755)).unwrap();
        Some(format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap()
        ))
    })
}

/// Starts a worktree create from `builder`'s agent, holds it in git while
/// `between` runs, then returns the created workspace's root pane.
fn worktree_created_around(
    session: &Session,
    builder: &InPaneReporter,
    between: impl FnOnce(),
) -> Value {
    let checkout = session.base.join("slow-checkout");
    builder.start(&[
        "worktree",
        "create",
        "--branch",
        "slow",
        "--path",
        checkout.to_str().unwrap(),
    ]);
    crate::support::wait_for_file(&session.base.join("entered"), Duration::from_secs(10));
    between();
    fs::write(session.base.join("release"), "").unwrap();
    let checkout = checkout.to_str().unwrap().to_owned();
    let mut found = Value::Null;
    assert!(wait_until(
        Duration::from_secs(10),
        Duration::from_millis(50),
        || {
            let panes = run_cli_json(&session.socket, &["pane", "list"]);
            found = panes["result"]["panes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|pane| {
                    pane["cwd"].as_str().is_some_and(|cwd| {
                        fs::canonicalize(cwd).ok() == fs::canonicalize(&checkout).ok()
                    })
                })
                .cloned()
                .unwrap_or(Value::Null);
            !found.is_null()
        }
    ));
    found
}

fn gated_builder(session: &Session) -> (String, InPaneReporter) {
    let builder = session.create(&["tab", "create"], "root_pane");
    let builder = builder["pane_id"].as_str().unwrap().to_owned();
    let agent = session.agent_in(&builder, "builder");
    session.rename("pane", &builder, "builder");
    (builder, agent)
}

#[test]
fn started_by_worktree_in_flight_follows_a_moved_starter() {
    let session = gated_worktree_session();
    let (builder, agent) = gated_builder(&session);
    let mut moved_to = String::new();
    let created = worktree_created_around(&session, &agent, || {
        let moved = run_cli_json(
            &session.socket,
            &["pane", "move", &builder, "--new-workspace"],
        );
        moved_to = moved["result"]["move_result"]["pane"]["pane_id"]
            .as_str()
            .unwrap()
            .to_owned();
    });
    assert_ne!(moved_to, builder);
    assert_eq!(
        created["started_by"],
        json!({"pane_id": moved_to}),
        "{created}"
    );
}

#[test]
fn started_by_worktree_in_flight_keeps_the_chain_of_a_closed_starter() {
    let session = gated_worktree_session();
    let (builder, agent) = gated_builder(&session);
    let created = worktree_created_around(&session, &agent, || {
        assert!(run_cli(&session.socket, &["pane", "close", &builder])
            .status
            .success());
    });
    assert_eq!(
        created["started_by"],
        json!({"closed": {"label": "builder", "started_by": {"pane_id": session.starter}}}),
        "{created}"
    );
}
