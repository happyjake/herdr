use super::harness::*;

#[test]
fn agent_start_waits_through_unknown_then_rejects_blocked() {
    let base = unique_test_dir();
    fs::create_dir_all(&base).unwrap();
    let socket_path = base.join("herdr.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();

    let server = thread::spawn(move || {
        let (mut pane_stream, pane_line) = accept_fake_cli_operation(&listener);
        let pane: serde_json::Value = serde_json::from_str(&pane_line).unwrap();
        assert_eq!(pane["method"], "pane.get");
        assert_eq!(pane["params"]["pane_id"], "w1:p1");
        writeln!(
            pane_stream,
            "{}",
            serde_json::json!({
                "id": pane["id"],
                "result": {
                    "type": "pane_info",
                    "pane": { "terminal_id": "term_1" }
                }
            })
        )
        .unwrap();
        pane_stream.flush().unwrap();

        let (mut start_stream, start_line) = accept_fake_cli_operation(&listener);
        let start: serde_json::Value = serde_json::from_str(&start_line).unwrap();
        assert_eq!(start["method"], "agent.start");
        writeln!(
            start_stream,
            "{}",
            serde_json::json!({
                "id": start["id"],
                "result": {
                    "type": "agent_started",
                    "agent": {
                        "pane_id": "w1:p1",
                        "terminal_id": "term_1",
                        "name": "reviewer"
                    },
                    "argv": ["opencode"]
                }
            })
        )
        .unwrap();
        start_stream.flush().unwrap();

        let (mut get_stream, get_line) = accept_fake_cli_operation(&listener);
        let get: serde_json::Value = serde_json::from_str(&get_line).unwrap();
        assert_eq!(get["method"], "agent.get");
        assert_eq!(get["params"]["target"], "reviewer");
        writeln!(
            get_stream,
            "{}",
            serde_json::json!({
                "id": get["id"],
                "result": {
                    "type": "agent_info",
                    "agent": {
                        "agent": null,
                        "agent_status": "unknown",
                        "interactive_ready": true,
                        "launch_pending": false,
                        "name": "reviewer",
                        "pane_id": "w1:p1",
                        "terminal_id": "term_1"
                    }
                }
            })
        )
        .unwrap();
        get_stream.flush().unwrap();

        let (mut get_stream, get_line) = accept_fake_cli_operation(&listener);
        let get: serde_json::Value = serde_json::from_str(&get_line).unwrap();
        assert_eq!(get["method"], "agent.get");
        assert_eq!(get["params"]["target"], "reviewer");
        writeln!(
            get_stream,
            "{}",
            serde_json::json!({
                "id": get["id"],
                "result": {
                    "type": "agent_info",
                    "agent": {
                        "agent": "opencode",
                        "agent_status": "blocked",
                        "interactive_ready": true,
                        "launch_pending": false,
                        "name": "reviewer",
                        "pane_id": "w1:p1",
                        "terminal_id": "term_1"
                    }
                }
            })
        )
        .unwrap();
        get_stream.flush().unwrap();
    });

    let started = run_cli(
        &socket_path,
        &[
            "agent", "start", "reviewer", "--kind", "opencode", "--pane", "w1:p1",
        ],
    );
    assert_eq!(started.status.code(), Some(1));
    assert!(started.stdout.is_empty());
    let error: serde_json::Value = serde_json::from_slice(&started.stderr).unwrap();
    assert_eq!(error["error"]["code"], "agent_not_ready");

    server.join().unwrap();
    cleanup_test_base(&base);
}

#[test]
fn agent_start_does_not_retry_after_the_target_terminal_changes() {
    let base = unique_test_dir();
    fs::create_dir_all(&base).unwrap();
    let socket_path = base.join("herdr.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();

    let server = thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, line) = accept_fake_cli_operation(&listener);
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["method"], "pane.get");
            writeln!(
                stream,
                "{}",
                serde_json::json!({
                    "id": request["id"],
                    "result": {
                        "type": "pane_info",
                        "pane": { "terminal_id": "term_1" }
                    }
                })
            )
            .unwrap();
            stream.flush().unwrap();

            let (mut stream, line) = accept_fake_cli_operation(&listener);
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            if request["method"] == "agent.start" {
                writeln!(
                    stream,
                    "{}",
                    serde_json::json!({
                        "id": request["id"],
                        "error": {
                            "code": "agent_pane_busy",
                            "message": "agent target pane w1:p1 is not an available shell"
                        }
                    })
                )
                .unwrap();
                stream.flush().unwrap();
            } else {
                assert_eq!(request["method"], "pane.process_info");
                writeln!(
                    stream,
                    "{}",
                    serde_json::json!({
                        "id": request["id"],
                        "result": {
                            "type": "pane_process_info",
                            "process_info": {
                                "pane_id": "w1:p1",
                                "shell_pid": 10,
                                "foreground_process_group_id": 10,
                                "foreground_processes": [
                                    { "pid": 10, "name": "bash" },
                                    { "pid": 11, "name": "startup-helper" }
                                ]
                            }
                        }
                    })
                )
                .unwrap();
                stream.flush().unwrap();
            }
        }

        let (mut stream, line) = accept_fake_cli_operation(&listener);
        let request: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["method"], "pane.get");
        writeln!(
            stream,
            "{}",
            serde_json::json!({
                "id": request["id"],
                "result": {
                    "type": "pane_info",
                    "pane": { "terminal_id": "term_2" }
                }
            })
        )
        .unwrap();
        stream.flush().unwrap();
    });

    let started = run_cli(
        &socket_path,
        &[
            "agent", "start", "reviewer", "--kind", "pi", "--pane", "w1:p1",
        ],
    );
    assert_eq!(started.status.code(), Some(1));
    let error: serde_json::Value = serde_json::from_slice(&started.stderr).unwrap();
    assert_eq!(error["error"]["code"], "agent_pane_busy");

    server.join().unwrap();
    cleanup_test_base(&base);
}

#[test]
fn prompt_wait_is_sent_as_one_agent_request() {
    let base = unique_test_dir();
    fs::create_dir_all(&base).unwrap();
    let socket_path = base.join("herdr.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();

    let server = thread::spawn(move || {
        let (mut prompt_stream, prompt_line) = accept_fake_cli_operation(&listener);
        let prompt: serde_json::Value = serde_json::from_str(&prompt_line).unwrap();
        assert_eq!(prompt["method"], "agent.prompt");
        assert_eq!(prompt["params"]["target"], "w1:p1");
        assert_eq!(
            prompt["params"]["wait"]["until"],
            serde_json::json!(["idle"])
        );
        assert!(prompt["params"]["wait"].get("timeout_ms").is_none());
        writeln!(
            prompt_stream,
            "{}",
            serde_json::json!({
                "id": prompt["id"],
                "result": {
                    "type": "agent_prompted",
                    "agent": {
                        "pane_id": "w1:p1",
                        "terminal_id": "term_1",
                        "name": "reviewer",
                        "agent": "pi",
                        "agent_status": "idle",
                        "workspace_id": "w1",
                        "tab_id": "w1:t1",
                        "focused": true,
                        "revision": 0
                    }
                }
            })
        )
        .unwrap();
        prompt_stream.flush().unwrap();
    });

    let prompted = run_cli(
        &socket_path,
        &[
            "agent",
            "prompt",
            "w1:p1",
            "review this",
            "--wait",
            "--until",
            "idle",
        ],
    );
    assert!(
        prompted.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&prompted.stderr)
    );
    let prompted: serde_json::Value = serde_json::from_slice(&prompted.stdout).unwrap();
    assert_eq!(prompted["result"]["agent"]["name"], "reviewer");

    server.join().unwrap();
    cleanup_test_base(&base);
}

fn accept_fake_cli_operation_with_prompt_expectations(
    listener: &UnixListener,
) -> (UnixStream, String) {
    loop {
        let (mut stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        reader.read_line(&mut line).unwrap();
        let request: serde_json::Value = serde_json::from_str(&line).unwrap();
        if request["method"] != "ping" {
            return (stream, line);
        }

        writeln!(
            stream,
            "{}",
            serde_json::json!({
                "id": request["id"],
                "result": {
                    "type": "pong",
                    "version": "different-build-same-protocol",
                    "protocol": CURRENT_PROTOCOL,
                    "capabilities": {
                        "live_handoff": true,
                        "detached_server_daemon": true,
                        "agent_prompt_expectations": true
                    }
                }
            })
        )
        .unwrap();
        stream.flush().unwrap();
    }
}

#[test]
fn prompt_refuses_expectations_when_server_lacks_capability() {
    let base = unique_test_dir();
    fs::create_dir_all(&base).unwrap();
    let socket_path = base.join("herdr.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();

    let server = thread::spawn(move || {
        // First command: conditional prompt against a server whose pong omits
        // `agent_prompt_expectations`. The CLI must fail closed after the ping
        // without sending `agent.prompt`.
        let (mut stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        reader.read_line(&mut line).unwrap();
        let ping: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(ping["method"], "ping");
        write_fake_pong(
            &mut stream,
            &ping,
            "base-branch-same-protocol",
            CURRENT_PROTOCOL,
        );

        // Second command: legacy `--wait` literal text + `--wait` option
        // without expectations still succeeds against the same older server.
        let (mut wait_stream, wait_line) = accept_fake_cli_operation(&listener);
        let wait_req: serde_json::Value = serde_json::from_str(&wait_line).unwrap();
        assert_eq!(wait_req["method"], "agent.prompt");
        assert_eq!(wait_req["params"]["target"], "reviewer");
        assert_eq!(wait_req["params"]["text"], "--wait");
        assert_eq!(wait_req["params"]["wait"], serde_json::json!({}));
        writeln!(
            wait_stream,
            "{}",
            serde_json::json!({
                "id": wait_req["id"],
                "result": {
                    "type": "agent",
                    "agent": sample_agent_json("idle")
                }
            })
        )
        .unwrap();
        wait_stream.flush().unwrap();

        // Third command: legacy `--` literal text + `--wait` option submits
        // literal `--` and waits.
        let (mut dash_stream, dash_line) = accept_fake_cli_operation(&listener);
        let dash_req: serde_json::Value = serde_json::from_str(&dash_line).unwrap();
        assert_eq!(dash_req["method"], "agent.prompt");
        assert_eq!(dash_req["params"]["target"], "reviewer");
        assert_eq!(dash_req["params"]["text"], "--");
        assert_eq!(dash_req["params"]["wait"], serde_json::json!({}));
        writeln!(
            dash_stream,
            "{}",
            serde_json::json!({
                "id": dash_req["id"],
                "result": {
                    "type": "agent",
                    "agent": sample_agent_json("idle")
                }
            })
        )
        .unwrap();
        dash_stream.flush().unwrap();
    });

    let refused = run_cli(
        &socket_path,
        &[
            "agent",
            "prompt",
            "reviewer",
            "hello",
            "--expect-terminal",
            "term_wrong",
        ],
    );
    assert_eq!(refused.status.code(), Some(1));
    assert!(refused.stdout.is_empty());
    let refused_err: serde_json::Value = serde_json::from_slice(&refused.stderr).unwrap();
    assert_eq!(refused_err["id"], "cli:agent:prompt");
    assert_eq!(refused_err["error"]["code"], "protocol_mismatch");
    assert!(refused_err["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .contains("agent_prompt_expectations"));

    let literal_wait = run_cli(
        &socket_path,
        &["agent", "prompt", "reviewer", "--wait", "--wait"],
    );
    assert!(
        literal_wait.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&literal_wait.stderr)
    );

    let literal_dash = run_cli(
        &socket_path,
        &["agent", "prompt", "reviewer", "--", "--wait"],
    );
    assert!(
        literal_dash.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&literal_dash.stderr)
    );

    server.join().unwrap();
    cleanup_test_base(&base);
}

#[test]
fn prompt_sends_conditional_expectations_and_reports_refusal() {
    let base = unique_test_dir();
    fs::create_dir_all(&base).unwrap();
    let socket_path = base.join("herdr.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();

    for invalid_status in ["unknown", "bogus", "", "idle,"] {
        let rejected = run_cli(
            &socket_path,
            &[
                "agent",
                "prompt",
                "w1:p1",
                "review this",
                "--if-status",
                invalid_status,
            ],
        );
        assert_eq!(
            rejected.status.code(),
            Some(2),
            "expected exit 2 for --if-status {invalid_status:?}"
        );
    }

    let server = thread::spawn(move || {
        // First call: flags after <text>, server returns agent_mismatch.
        let (mut mismatch_stream, mismatch_line) =
            accept_fake_cli_operation_with_prompt_expectations(&listener);
        let mismatch: serde_json::Value = serde_json::from_str(&mismatch_line).unwrap();
        assert_eq!(mismatch["method"], "agent.prompt");
        assert_eq!(mismatch["params"]["target"], "w1:p1");
        assert_eq!(mismatch["params"]["text"], "review this");
        assert_eq!(mismatch["params"]["expect_terminal"], "term_1");
        assert_eq!(mismatch["params"]["expect_agent"], "claude");
        assert_eq!(mismatch["params"]["expect_session"], "sess-abc");
        assert_eq!(
            mismatch["params"]["if_status"],
            serde_json::json!(["idle", "done"])
        );
        writeln!(
            mismatch_stream,
            "{}",
            serde_json::json!({
                "id": mismatch["id"],
                "error": {
                    "code": "agent_mismatch",
                    "message": "agent target w1:p1 has terminal term_2, expected term_1"
                }
            })
        )
        .unwrap();
        mismatch_stream.flush().unwrap();

        // Second call: flags before <text>, server returns agent_busy.
        let (mut busy_stream, busy_line) =
            accept_fake_cli_operation_with_prompt_expectations(&listener);
        let busy: serde_json::Value = serde_json::from_str(&busy_line).unwrap();
        assert_eq!(busy["method"], "agent.prompt");
        assert_eq!(busy["params"]["target"], "w1:p1");
        assert_eq!(busy["params"]["text"], "follow up");
        assert_eq!(busy["params"]["expect_terminal"], "term_1");
        assert_eq!(
            busy["params"]["if_status"],
            serde_json::json!(["idle", "blocked"])
        );
        writeln!(
            busy_stream,
            "{}",
            serde_json::json!({
                "id": busy["id"],
                "error": {
                    "code": "agent_busy",
                    "message": "agent w1:p1 has status Working, which is not in expected if_status"
                }
            })
        )
        .unwrap();
        busy_stream.flush().unwrap();
    });

    let mismatch = run_cli(
        &socket_path,
        &[
            "agent",
            "prompt",
            "w1:p1",
            "review this",
            "--expect-terminal",
            "term_1",
            "--expect-agent",
            "claude",
            "--expect-session",
            "sess-abc",
            "--if-status",
            "idle,done",
        ],
    );
    assert_eq!(mismatch.status.code(), Some(1));
    assert!(mismatch.stdout.is_empty());
    let mismatch_err: serde_json::Value = serde_json::from_slice(&mismatch.stderr).unwrap();
    assert_eq!(mismatch_err["error"]["code"], "agent_mismatch");

    let busy = run_cli(
        &socket_path,
        &[
            "agent",
            "prompt",
            "w1:p1",
            "--expect-terminal",
            "term_1",
            "--if-status",
            "idle,blocked",
            "follow up",
        ],
    );
    assert_eq!(busy.status.code(), Some(1));
    assert!(busy.stdout.is_empty());
    let busy_err: serde_json::Value = serde_json::from_slice(&busy.stderr).unwrap();
    assert_eq!(busy_err["error"]["code"], "agent_busy");

    server.join().unwrap();
    cleanup_test_base(&base);
}
