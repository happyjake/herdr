use super::harness::*;
use crate::support::InPaneReporter;
use serde_json::{json, Value};

struct ServerProcess {
    child: std::process::Child,
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let pid = self.child.id();
        let _ = self.child.kill();
        let _ = self.child.wait();
        unregister_spawned_herdr_pid(Some(pid));
    }
}

fn spawn_herdr_with_home(
    home: &Path,
    config_home: &Path,
    runtime_dir: &Path,
    socket_path: &Path,
) -> ServerProcess {
    fs::create_dir_all(home).unwrap();
    fs::create_dir_all(config_home.join(app_dir_name())).unwrap();
    fs::create_dir_all(runtime_dir).unwrap();
    register_runtime_dir(runtime_dir);
    fs::write(
        config_home.join(app_dir_name()).join("config.toml"),
        "onboarding = false\n",
    )
    .unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_herdr"));
    command
        .arg("server")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", config_home)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("HERDR_SOCKET_PATH", socket_path)
        .env_remove("HERDR_CLIENT_SOCKET_PATH")
        .env("SHELL", "/bin/sh")
        .env_remove("HERDR_ENV")
        .env_remove("HERDR_STARTUP_CWD")
        .env_remove("HERDR_SESSION")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let child = command.spawn().unwrap();
    register_spawned_herdr_pid(Some(child.id()));
    ServerProcess { child }
}

#[test]
fn pane_close_records_by_and_reason_in_logbook_and_surfaces_in_search_sessions() {
    let base = unique_test_dir();
    let home = base.join("home");
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket = runtime_dir.join("herdr.sock");
    let project = base.join("project");
    fs::create_dir_all(&project).unwrap();
    let project_str = project.to_str().unwrap();

    let claude_record = home.join(".claude/projects/project/session-phone.jsonl");
    fs::create_dir_all(claude_record.parent().unwrap()).unwrap();
    fs::write(
        &claude_record,
        format!(
            "{}\n",
            json!({
                "type": "user",
                "sessionId": "session-phone",
                "cwd": project_str,
                "gitBranch": "main",
                "timestamp": "2026-10-03T00:00:00Z",
                "message": {"role": "user", "content": "Inspect dormant close attribution"}
            })
        ),
    )
    .unwrap();

    let server = spawn_herdr_with_home(&home, &config_home, &runtime_dir, &socket);
    wait_for_socket(&socket, Duration::from_secs(10));

    let created = run_cli_json(&socket, &["workspace", "create", "--cwd", project_str]);
    let p1 = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    let p2 = run_cli_json(
        &socket,
        &["pane", "split", &p1, "--direction", "right", "--no-focus"],
    )["result"]["pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    let p3 = run_cli_json(
        &socket,
        &["pane", "split", &p1, "--direction", "down", "--no-focus"],
    )["result"]["pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();

    let reporter = InPaneReporter::write(&base, "claude", &socket);
    let run = run_cli(&socket, &["pane", "run", &p1, &reporter.command()]);
    assert!(run.status.success(), "{run:?}");
    reporter.wait_until_started(Duration::from_secs(10));
    assert!(wait_until(
        Duration::from_secs(10),
        Duration::from_millis(50),
        || run_cli_json(&socket, &["pane", "get", &p1])["result"]["pane"]["agent"] == "claude"
    ));
    reporter.ok(&[
        "pane",
        "report-agent-session",
        &p1,
        "--source=herdr:claude",
        "--agent=claude",
        "--seq=1",
        "--agent-session-id=session-phone",
        "--session-start-source=startup",
    ]);
    assert!(wait_until(
        Duration::from_secs(5),
        Duration::from_millis(25),
        || {
            run_cli_json(&socket, &["pane", "get", &p1])["result"]["pane"]["agent_session"]["value"]
                == "session-phone"
        }
    ));

    let phone_close = send_request(
        &socket,
        &json!({
            "id": "phone-close",
            "method": "pane.close",
            "params": {
                "pane_id": p1,
                "by": "phone",
                "reason": "dormant"
            }
        })
        .to_string(),
    );
    assert_eq!(phone_close["result"]["type"], "ok", "{phone_close}");

    let cli_close = run_cli(&socket, &["pane", "close", &p2]);
    assert!(
        cli_close.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&cli_close.stderr)
    );

    let api_close = send_request(
        &socket,
        &json!({
            "id": "endpoint:example",
            "method": "pane.close",
            "params": {
                "pane_id": p3
            }
        })
        .to_string(),
    );
    assert_eq!(api_close["result"]["type"], "ok", "{api_close}");

    let logbook_path = config_home.join(app_dir_name()).join("logbook.jsonl");
    assert!(
        wait_until(Duration::from_secs(5), Duration::from_millis(25), || {
            fs::read_to_string(&logbook_path).is_ok_and(|text| {
                text.lines()
                    .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                    .filter(|row| row["type"] == "pane_closed")
                    .count()
                    == 3
            })
        }),
        "expected 3 pane_closed lines in {}",
        logbook_path.display()
    );

    let closed_events: Vec<Value> = fs::read_to_string(&logbook_path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["type"] == "pane_closed")
        .collect();
    assert_eq!(closed_events.len(), 3);
    assert_eq!(closed_events[0]["pane"], p1);
    assert_eq!(closed_events[0]["by"], "phone");
    assert_eq!(closed_events[0]["reason"], "dormant");
    assert_eq!(
        closed_events[0]["session"],
        json!({"agent": "claude", "id": "session-phone"})
    );
    let closed_at = closed_events[0]["at"].as_u64().unwrap();

    assert_eq!(closed_events[1]["pane"], p2);
    assert_eq!(closed_events[1]["by"], "cli");
    assert!(closed_events[1].get("reason").is_none());
    assert!(closed_events[1].get("session").is_none());

    assert_eq!(closed_events[2]["pane"], p3);
    assert_eq!(closed_events[2]["by"], "api");
    assert!(closed_events[2].get("reason").is_none());
    assert!(closed_events[2].get("session").is_none());

    let search = send_request(
        &socket,
        &json!({
            "id": "search-closed",
            "method": "server.search_sessions",
            "params": {
                "terms": ["dormant"]
            }
        })
        .to_string(),
    );
    let sessions = search["result"]["sessions"]
        .as_array()
        .unwrap_or_else(|| panic!("expected sessions array: {search}"));
    assert_eq!(sessions.len(), 1, "{search}");
    let hit = &sessions[0];
    assert_eq!(hit["key"]["value"], "session-phone");
    assert_eq!(hit["state"], "ended");
    assert_eq!(
        hit["closed"],
        json!({
            "by": "phone",
            "reason": "dormant",
            "at_ms": closed_at
        })
    );
    assert_eq!(
        hit["resume"],
        json!({
            "command": "claude --resume session-phone",
            "cwd": project_str
        })
    );

    let _ = run_cli(&socket, &["server", "stop"]);
    drop(server);
    cleanup_test_base(&base);
}
