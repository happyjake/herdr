use super::harness::*;

struct Session {
    base: PathBuf,
    socket: PathBuf,
    server: Option<SpawnedHerdr>,
}

impl Drop for Session {
    fn drop(&mut self) {
        // The replacement is no longer owned by SpawnedHerdr after handoff.
        if self.socket.exists() {
            let _ = run_cli(&self.socket, &["server", "stop"]);
        }
        drop(self.server.take());
        cleanup_test_base(&self.base);
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn run_in_pane_herdr(session: &Session, pane_id: &str, name: &str, args: &[&str]) -> String {
    let out_path = session.base.join(format!("{name}.out"));
    let status_path = session.base.join(format!("{name}.status"));
    let status_tmp_path = session.base.join(format!("{name}.status.tmp"));
    let _ = fs::remove_file(&out_path);
    let _ = fs::remove_file(&status_path);
    let mut quoted_args = vec![shell_quote(env!("CARGO_BIN_EXE_herdr"))];
    quoted_args.extend(args.iter().map(|arg| shell_quote(arg)));
    let command = format!(
        "{} > {} 2>&1; printf '%s\\n' \"$?\" > {} && mv {} {}",
        quoted_args.join(" "),
        shell_quote(out_path.to_str().unwrap()),
        shell_quote(status_tmp_path.to_str().unwrap()),
        shell_quote(status_tmp_path.to_str().unwrap()),
        shell_quote(status_path.to_str().unwrap()),
    );
    let run = run_cli(&session.socket, &["pane", "run", pane_id, &command]);
    assert!(run.status.success(), "{run:?}");
    crate::support::wait_for_file(&status_path, Duration::from_secs(10));
    let output = fs::read_to_string(&out_path).unwrap_or_default();
    assert_eq!(
        fs::read_to_string(&status_path).unwrap().trim(),
        "0",
        "{output}"
    );
    output
}

fn perform_live_handoff(session: &mut Session, pane_id: &str) {
    let handoff = run_cli(&session.socket, &["server", "live-handoff"]);
    assert!(handoff.status.success(), "{handoff:?}");
    drop(session.server.take());
    assert!(wait_until(
        Duration::from_secs(10),
        Duration::from_millis(50),
        || run_cli(&session.socket, &["pane", "get", pane_id])
            .status
            .success()
    ));
}

#[test]
fn reported_agent_on_a_plain_shell_survives_live_handoff() {
    let base = unique_test_dir();
    let socket = base.join("runtime/herdr.sock");
    let server = spawn_herdr(&base.join("config"), &base.join("runtime"), &socket);
    let mut session = Session {
        base,
        socket,
        server: Some(server),
    };
    wait_for_socket(&session.socket, Duration::from_secs(10));
    let created = run_cli_json(
        &session.socket,
        &[
            "workspace",
            "create",
            "--cwd",
            session.base.to_str().unwrap(),
        ],
    );
    let pane_id = created["result"]["root_pane"]["pane_id"].as_str().unwrap();
    assert!(created["result"]["root_pane"]["agent"].is_null());

    // Report from inside the pane using a custom lifecycle source, then return
    // to the plain shell: no agent process can rediscover the label. The built-in
    // pi source requires process or session evidence this shell lacks.
    let report_path = session.base.join("report.json");
    let status_path = session.base.join("report.status");
    let before_pid_path = session.base.join("before.pid");
    let command = format!(
        "{} pane report-agent {} --source custom:test --agent pi --state working > {} 2>&1; report_status=$?; printf '%s\\n' \"$$\" > {}; printf '%s\\n' \"$report_status\" > {}",
        shell_quote(env!("CARGO_BIN_EXE_herdr")),
        shell_quote(pane_id),
        shell_quote(report_path.to_str().unwrap()),
        shell_quote(before_pid_path.to_str().unwrap()),
        shell_quote(status_path.to_str().unwrap()),
    );
    let run = run_cli(&session.socket, &["pane", "run", pane_id, &command]);
    assert!(run.status.success(), "{run:?}");
    crate::support::wait_for_file(&status_path, Duration::from_secs(10));
    let report = fs::read_to_string(&report_path).unwrap();
    assert_eq!(
        fs::read_to_string(&status_path).unwrap().trim(),
        "0",
        "{report}"
    );
    let before = run_cli_json(&session.socket, &["pane", "get", pane_id])["result"]["pane"].clone();
    assert_eq!(before["agent"], "pi", "{before}");
    assert_eq!(before["agent_status"], "working", "{before}");
    assert!(before["agent_session"].is_null(), "{before}");

    let handoff = run_cli(&session.socket, &["server", "live-handoff"]);
    assert!(handoff.status.success(), "{handoff:?}");
    drop(session.server.take());
    assert!(wait_until(
        Duration::from_secs(10),
        Duration::from_millis(50),
        || run_cli(&session.socket, &["pane", "get", pane_id])
            .status
            .success()
    ));

    // Prove that handoff retained the shell, rather than cold-respawning it.
    let after_pid_path = session.base.join("after.pid");
    let command = format!(
        "printf '%s\\n' \"$$\" > {}",
        shell_quote(after_pid_path.to_str().unwrap())
    );
    let run = run_cli(&session.socket, &["pane", "run", pane_id, &command]);
    assert!(run.status.success(), "{run:?}");
    crate::support::wait_for_file(&after_pid_path, Duration::from_secs(10));
    assert_eq!(
        fs::read_to_string(&after_pid_path).unwrap(),
        fs::read_to_string(&before_pid_path).unwrap(),
        "live handoff must retain the original shell process"
    );
    let after = run_cli_json(&session.socket, &["pane", "get", pane_id])["result"]["pane"].clone();
    assert_ne!(after["terminal_id"], before["terminal_id"]);
    assert_eq!(after["agent"], "pi", "{after}");
    assert_eq!(after["agent_status"], "working", "{after}");
    assert!(after["ended_agent"].is_null(), "{after}");
}

#[test]
fn reported_agent_explain_authority_and_repeated_handoff_survive_live_handoff() {
    let base = unique_test_dir();
    let socket = base.join("runtime/herdr.sock");
    let server = spawn_herdr(&base.join("config"), &base.join("runtime"), &socket);
    let mut session = Session {
        base,
        socket,
        server: Some(server),
    };
    wait_for_socket(&session.socket, Duration::from_secs(10));
    let created = run_cli_json(
        &session.socket,
        &[
            "workspace",
            "create",
            "--cwd",
            session.base.to_str().unwrap(),
        ],
    );
    let pane_id = created["result"]["root_pane"]["pane_id"].as_str().unwrap();

    let reported = send_request(
        &session.socket,
        &serde_json::json!({
            "id": "report-initial",
            "method": "pane.report_agent",
            "params": {
                "pane_id": pane_id,
                "source": "custom:test",
                "agent": "pi",
                "state": "working",
                "seq": 10,
            }
        })
        .to_string(),
    );
    assert_eq!(reported["result"]["type"], "ok", "{reported}");

    let explain_before = run_cli_json(&session.socket, &["agent", "explain", pane_id, "--json"]);
    assert_eq!(explain_before["agent"], "pi", "{explain_before}");
    assert!(
        explain_before["manifest_version"].is_string(),
        "{explain_before}"
    );
    assert_eq!(
        explain_before["manifest_source"], "bundled",
        "{explain_before}"
    );
    assert_eq!(
        explain_before["fallback_reason"], "default_known_agent_idle_fallback",
        "{explain_before}"
    );
    assert!(
        explain_before["evaluated_rules"].is_array(),
        "{explain_before}"
    );

    perform_live_handoff(&mut session, pane_id);

    let explain_after = run_cli_json(&session.socket, &["agent", "explain", pane_id, "--json"]);
    assert_eq!(explain_after["agent"], "pi", "{explain_after}");
    assert_eq!(
        explain_after["manifest_version"], explain_before["manifest_version"],
        "{explain_after}"
    );
    assert_eq!(
        explain_after["manifest_source"], explain_before["manifest_source"],
        "{explain_after}"
    );
    assert_eq!(
        explain_after["fallback_reason"], explain_before["fallback_reason"],
        "{explain_after}"
    );
    assert_eq!(
        explain_after["evaluated_rules"].as_array().map(Vec::len),
        explain_before["evaluated_rules"].as_array().map(Vec::len),
        "{explain_after}"
    );

    // A stale sequence or unsequenced report from the same source must be ignored after handoff.
    for seq in [Some(10_u64), None] {
        let mut params = serde_json::json!({
            "pane_id": pane_id,
            "source": "custom:test",
            "agent": "pi",
            "state": "idle",
        });
        if let Some(seq) = seq {
            params["seq"] = serde_json::json!(seq);
        }
        let stale = send_request(
            &session.socket,
            &serde_json::json!({
                "id": "report-stale",
                "method": "pane.report_agent",
                "params": params,
            })
            .to_string(),
        );
        assert_eq!(stale["result"]["type"], "ok", "{stale}");
        let pane =
            run_cli_json(&session.socket, &["pane", "get", pane_id])["result"]["pane"].clone();
        assert_eq!(pane["agent"], "pi", "{pane}");
        assert_eq!(pane["agent_status"], "working", "{pane}");
    }

    // A foreign source cannot clear or release the carried custom report authority.
    let foreign_clear = send_request(
        &session.socket,
        &serde_json::json!({
            "id": "clear-foreign",
            "method": "pane.clear_agent_authority",
            "params": {
                "pane_id": pane_id,
                "source": "custom:other",
            }
        })
        .to_string(),
    );
    assert_eq!(foreign_clear["result"]["type"], "ok", "{foreign_clear}");
    run_in_pane_herdr(
        &session,
        pane_id,
        "release-foreign",
        &[
            "pane",
            "release-agent",
            pane_id,
            "--source",
            "custom:other",
            "--agent",
            "pi",
        ],
    );
    let pane = run_cli_json(&session.socket, &["pane", "get", pane_id])["result"]["pane"].clone();
    assert_eq!(pane["agent"], "pi", "{pane}");
    assert_eq!(pane["agent_status"], "working", "{pane}");

    // The reporting source can update the pane with a fresh sequence after handoff.
    let updated = send_request(
        &session.socket,
        &serde_json::json!({
            "id": "report-update",
            "method": "pane.report_agent",
            "params": {
                "pane_id": pane_id,
                "source": "custom:test",
                "agent": "pi",
                "state": "idle",
                "seq": 11,
            }
        })
        .to_string(),
    );
    assert_eq!(updated["result"]["type"], "ok", "{updated}");
    let pane = run_cli_json(&session.socket, &["pane", "get", pane_id])["result"]["pane"].clone();
    assert_eq!(pane["agent"], "pi", "{pane}");
    assert_eq!(pane["agent_status"], "idle", "{pane}");
    assert!(pane["ended_agent"].is_null(), "{pane}");

    // A second live handoff in a row still carries the reported agent and explain fields.
    perform_live_handoff(&mut session, pane_id);

    let after_second =
        run_cli_json(&session.socket, &["pane", "get", pane_id])["result"]["pane"].clone();
    assert_eq!(after_second["agent"], "pi", "{after_second}");
    assert_eq!(after_second["agent_status"], "idle", "{after_second}");
    assert!(after_second["ended_agent"].is_null(), "{after_second}");

    let explain_second = run_cli_json(&session.socket, &["agent", "explain", pane_id, "--json"]);
    assert_eq!(explain_second["agent"], "pi", "{explain_second}");
    assert_eq!(
        explain_second["manifest_version"], explain_before["manifest_version"],
        "{explain_second}"
    );

    // After the second handoff, the reporting source can clear or release its agent.
    let cleared = send_request(
        &session.socket,
        &serde_json::json!({
            "id": "clear-owner",
            "method": "pane.clear_agent_authority",
            "params": {
                "pane_id": pane_id,
                "source": "custom:test",
                "seq": 12,
            }
        })
        .to_string(),
    );
    assert_eq!(cleared["result"]["type"], "ok", "{cleared}");
    let after_clear =
        run_cli_json(&session.socket, &["pane", "get", pane_id])["result"]["pane"].clone();
    assert!(after_clear["agent"].is_null(), "{after_clear}");
    assert_eq!(after_clear["agent_status"], "unknown", "{after_clear}");
    assert_eq!(after_clear["ended_agent"], "pi", "{after_clear}");

    run_in_pane_herdr(
        &session,
        pane_id,
        "report-again",
        &[
            "pane",
            "report-agent",
            pane_id,
            "--source",
            "custom:next",
            "--agent",
            "pi",
            "--state",
            "working",
        ],
    );
    let reacquired =
        run_cli_json(&session.socket, &["pane", "get", pane_id])["result"]["pane"].clone();
    assert_eq!(reacquired["agent"], "pi", "{reacquired}");
    assert!(reacquired["ended_agent"].is_null(), "{reacquired}");
    run_in_pane_herdr(
        &session,
        pane_id,
        "release-owner",
        &[
            "pane",
            "release-agent",
            pane_id,
            "--source",
            "custom:next",
            "--agent",
            "pi",
        ],
    );
    let released =
        run_cli_json(&session.socket, &["pane", "get", pane_id])["result"]["pane"].clone();
    assert!(released["agent"].is_null(), "{released}");
    assert_eq!(released["agent_status"], "unknown", "{released}");
    assert_eq!(released["ended_agent"], "pi", "{released}");
}

#[test]
fn reported_agent_pane_exiting_after_live_handoff_still_ends() {
    let base = unique_test_dir();
    let socket = base.join("runtime/herdr.sock");
    let server = spawn_herdr(&base.join("config"), &base.join("runtime"), &socket);
    let mut session = Session {
        base,
        socket,
        server: Some(server),
    };
    wait_for_socket(&session.socket, Duration::from_secs(10));
    let created = run_cli_json(
        &session.socket,
        &[
            "workspace",
            "create",
            "--cwd",
            session.base.to_str().unwrap(),
        ],
    );
    let tab_id = created["result"]["tab"]["tab_id"].as_str().unwrap();

    // Apply a layout with an explicit command so the pane respawns an interactive
    // shell when the command shell exits (`respawn_shell_on_exit`).
    let applied = send_request(
        &session.socket,
        &serde_json::json!({
            "id": "apply-shell-command",
            "method": "layout.apply",
            "params": {
                "tab_id": tab_id,
                "root": {
                    "type": "pane",
                    "cwd": session.base.to_str().unwrap(),
                    "command": ["/bin/sh"],
                }
            }
        })
        .to_string(),
    );
    assert!(applied.get("error").is_none(), "{applied}");
    let pane_id = applied["result"]["layout"]["root"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();

    run_in_pane_herdr(
        &session,
        &pane_id,
        "report-before-exit",
        &[
            "pane",
            "report-agent",
            &pane_id,
            "--source",
            "custom:test",
            "--agent",
            "pi",
            "--state",
            "working",
        ],
    );
    let before =
        run_cli_json(&session.socket, &["pane", "get", &pane_id])["result"]["pane"].clone();
    assert_eq!(before["agent"], "pi", "{before}");
    assert_eq!(before["agent_status"], "working", "{before}");

    perform_live_handoff(&mut session, &pane_id);

    let after_handoff =
        run_cli_json(&session.socket, &["pane", "get", &pane_id])["result"]["pane"].clone();
    assert_eq!(after_handoff["agent"], "pi", "{after_handoff}");
    assert_eq!(after_handoff["agent_status"], "working", "{after_handoff}");
    assert!(after_handoff["ended_agent"].is_null(), "{after_handoff}");

    // Exit the shell after handoff; the pane respawns its fallback shell and
    // records the reported agent as ended.
    let exit_run = run_cli(&session.socket, &["pane", "run", &pane_id, "exit"]);
    assert!(exit_run.status.success(), "{exit_run:?}");

    assert!(
        wait_until(Duration::from_secs(10), Duration::from_millis(50), || {
            let pane = &run_cli_json(&session.socket, &["pane", "get", &pane_id])["result"]["pane"];
            pane["agent"].is_null() && pane["ended_agent"] == "pi"
        }),
        "pane after shell exit: {}",
        run_cli_json(&session.socket, &["pane", "get", &pane_id])
    );
    let ended = run_cli_json(&session.socket, &["pane", "get", &pane_id])["result"]["pane"].clone();
    assert!(ended["agent"].is_null(), "{ended}");
    assert_eq!(ended["ended_agent"], "pi", "{ended}");
    assert_eq!(ended["agent_status"], "unknown", "{ended}");
}
