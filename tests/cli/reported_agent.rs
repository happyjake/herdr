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

// Pre-existing: handoff carries full-lifecycle hook authority, but a custom
// report without session identity loses its label while retaining its status.
#[test]
#[ignore = "live handoff drops a custom-reported agent without session identity"]
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
