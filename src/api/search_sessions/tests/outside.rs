use super::*;
use crate::platform::SessionProcess;

fn running_claude() -> FixtureHome {
    let home = FixtureHome::new();
    let cwd = home.dir("project");
    home.write(
        ".claude/projects/project/session-a.jsonl",
        &[claude_prompt(
            "session-a",
            &cwd,
            "Continue the work",
            "2026-10-12T11:39:11Z",
        )],
    );
    home.catalog.refresh();
    // The registry can change without a record-cache refresh.
    home.write(
        ".claude/sessions/241.json",
        &[json!({"pid": 241, "sessionId": "session-a", "cwd": cwd,
            "procStart": "Mon Oct 12 11:39:11 2026", "kind": "interactive", "status": "idle"})],
    );
    home.processes.lock().unwrap().push(SessionProcess {
        pid: 241,
        started_at: "Mon Oct 12 11:39:11 2026".into(),
        argv: vec!["claude".into()],
    });
    home
}

#[test]
fn a_running_claude_registry_entry_blocks_resume_outside_the_server() {
    let home = running_claude();
    let response = home.search(json!({}), vec![]);
    let row = by_key(&response, "session-a");
    assert_eq!(row["state"], "outside");
    assert_eq!(
        row["outside"],
        json!({"pid": 241, "detail": "claude pid 241"})
    );
    assert_eq!(row["resume"], Value::Null);
    assert_eq!(row["resume_blocked"], "outside");
    assert!(row.get("live_pane_id").is_none());
}

#[test]
fn a_dead_claude_pid_leaves_a_resumable_ended_session() {
    let home = running_claude();
    assert_eq!(
        by_key(&home.search(json!({}), vec![]), "session-a")["state"],
        "outside"
    );
    home.processes.lock().unwrap().clear();

    let response = home.search(json!({}), vec![]);
    let row = by_key(&response, "session-a");
    assert_eq!(row["state"], "ended");
    assert_eq!(row["resume"]["command"], "claude --resume session-a");
    assert!(row.get("outside").is_none());
    assert!(row.get("resume_blocked").is_none());
}

#[test]
fn a_reused_claude_pid_does_not_claim_the_old_session() {
    let home = running_claude();
    assert_eq!(
        by_key(&home.search(json!({}), vec![]), "session-a")["state"],
        "outside"
    );
    for started_at in ["Mon Oct 12 11:39:12 2026", ""] {
        home.processes.lock().unwrap()[0].started_at = started_at.into();
        let response = home.search(json!({}), vec![]);
        let row = by_key(&response, "session-a");
        assert_eq!(row["state"], "ended", "{started_at:?}");
        assert_eq!(row["resume"]["command"], "claude --resume session-a");
        assert!(row.get("outside").is_none());
    }
}

#[test]
fn this_servers_live_pane_wins_over_a_running_registry_entry() {
    let home = running_claude();
    let unrelated = home.search(json!({}), vec![live_pane("codex", "id", "session-a")]);
    assert_eq!(by_key(&unrelated, "session-a")["state"], "outside");

    let response = home.search(json!({}), vec![live_pane("claude", "id", "session-a")]);
    let row = by_key(&response, "session-a");
    assert_eq!(row["state"], "live");
    assert_eq!(row["live_pane_id"], "w1:p1");
    assert!(row.get("outside").is_none());
    assert_eq!(row["resume"], Value::Null);
    assert_eq!(row["resume_blocked"], "live");
}

#[test]
fn a_codex_resume_process_marks_its_saved_session_outside() {
    let home = FixtureHome::new();
    let cwd = home.dir("project");
    home.write(
        ".codex/sessions/2026/10/12/rollout-session-a.jsonl",
        &[json!({"type": "session_meta", "payload": {"id": "session-a", "cwd": cwd, "source": "cli"}})],
    );
    home.catalog.refresh();
    home.processes.lock().unwrap().push(SessionProcess {
        pid: 242,
        started_at: "Mon Oct 12 11:39:11 2026".into(),
        argv: vec!["/usr/bin/codex".into(), "resume".into(), "session-a".into()],
    });

    let response = home.search(json!({}), vec![]);
    let row = by_key(&response, "session-a");
    assert_eq!(row["state"], "outside");
    assert_eq!(
        row["outside"],
        json!({"pid": 242, "detail": "codex pid 242"})
    );
    assert_eq!(row["resume"], Value::Null);
    assert_eq!(row["resume_blocked"], "outside");
}

#[test]
fn pi_process_arguments_match_session_paths_and_record_ids() {
    let home = FixtureHome::new();
    let cwd = home.dir("project");
    let path = home.write(
        ".pi/agent/sessions/project/record with spaces.jsonl",
        &[json!({"type": "session", "id": "pi-a", "cwd": cwd,
            "timestamp": "2026-10-12T11:39:11Z"})],
    );
    home.catalog.refresh();
    let key = path.to_str().unwrap();
    for argv in [
        vec!["pi", "--session", key],
        vec![
            "node",
            "/usr/lib/node_modules/@earendil-works/pi-coding-agent/dist/cli.js",
            "--session",
            key,
        ],
        vec!["pi", "--session", "pi-a"],
    ] {
        *home.processes.lock().unwrap() = vec![SessionProcess {
            pid: 243,
            started_at: "Mon Oct 12 11:39:11 2026".into(),
            argv: argv.iter().map(|arg| (*arg).into()).collect(),
        }];
        let response = home.search(json!({}), vec![]);
        let row = by_key(&response, key);
        assert_eq!(row["state"], "outside", "{argv:?}");
        assert_eq!(row["outside"], json!({"pid": 243, "detail": "pi pid 243"}));
        assert_eq!(row["resume"], Value::Null);
        assert_eq!(row["resume_blocked"], "outside");
    }
}

#[test]
fn codex_process_matches_use_complete_arguments_and_the_correct_agent() {
    let home = FixtureHome::new();
    let cwd = home.dir("project");
    let path = home.write(
        ".codex/sessions/2026/10/12/rollout-session-a.jsonl",
        &[json!({"type": "session_meta", "payload": {"id": "session-a", "cwd": cwd, "source": "cli"}})],
    );
    home.catalog.refresh();
    let path = path.to_str().unwrap();
    for (argv, outside) in [
        (vec!["codex", "resume", "session-a"], true),
        (vec!["codex", "resume", path], true),
        (vec!["codex", "resume", "session-a-extra"], false),
        (vec!["codex", "inspect session-a please"], false),
        (vec!["cat", path], false),
        (vec!["printf", "codex", "resume", "session-a"], false),
        (vec!["pi", "--session", "session-a"], false),
    ] {
        *home.processes.lock().unwrap() = vec![SessionProcess {
            pid: 242,
            started_at: "Mon Oct 12 11:39:11 2026".into(),
            argv: argv.iter().map(|arg| (*arg).into()).collect(),
        }];
        let response = home.search(json!({}), vec![]);
        let row = by_key(&response, "session-a");
        assert_eq!(
            row["state"],
            if outside { "outside" } else { "ended" },
            "{argv:?}"
        );
        assert_eq!(row["resume"].is_null(), outside);
        assert_eq!(row.get("outside").is_some(), outside);
    }
}

#[test]
fn this_servers_live_panes_win_over_codex_and_pi_process_matches() {
    let home = FixtureHome::new();
    let cwd = home.dir("project");
    home.write(
        ".codex/sessions/2026/10/12/rollout-session-a.jsonl",
        &[json!({"type": "session_meta", "payload": {"id": "session-a", "cwd": cwd, "source": "cli"}})],
    );
    let path = home.write(
        ".pi/agent/sessions/project/pi-a.jsonl",
        &[json!({"type": "session", "id": "pi-a", "cwd": cwd,
            "timestamp": "2026-10-12T11:39:11Z"})],
    );
    home.catalog.refresh();
    let path = path.to_str().unwrap();
    *home.processes.lock().unwrap() = vec![
        SessionProcess {
            pid: 242,
            started_at: String::new(),
            argv: vec!["codex".into(), "resume".into(), "session-a".into()],
        },
        SessionProcess {
            pid: 243,
            started_at: String::new(),
            argv: vec!["pi".into(), "--session".into(), path.into()],
        },
    ];
    let outside = home.search(json!({}), vec![]);
    for key in ["session-a", path] {
        assert_eq!(by_key(&outside, key)["state"], "outside");
    }
    let response = home.search(
        json!({}),
        vec![
            live_pane("codex", "id", "session-a"),
            live_pane("pi", "path", path),
        ],
    );
    for key in ["session-a", path] {
        let row = by_key(&response, key);
        assert_eq!(row["state"], "live");
        assert_eq!(row["live_pane_id"], "w1:p1");
        assert!(row.get("outside").is_none());
        assert_eq!(row["resume"], Value::Null);
        assert_eq!(row["resume_blocked"], "live");
    }
}

#[test]
fn claude_requires_a_valid_registry_entry_and_ignores_key_files() {
    let home = running_claude();
    assert_eq!(
        by_key(&home.search(json!({}), vec![]), "session-a")["state"],
        "outside"
    );
    home.processes.lock().unwrap()[0].argv =
        vec!["claude".into(), "--resume".into(), "session-a".into()];
    std::fs::rename(
        home.root.join(".claude/sessions/241.json"),
        home.root.join(".claude/sessions/241.key"),
    )
    .unwrap();
    assert_eq!(
        by_key(&home.search(json!({}), vec![]), "session-a")["state"],
        "ended"
    );

    for entry in [
        json!("not an entry"),
        json!({"pid": 242, "sessionId": "session-a", "procStart": "Mon Oct 12 11:39:11 2026"}),
        json!({"pid": 241, "sessionId": "session-a"}),
        json!({"pid": 241, "sessionId": "session-a", "procStart": ""}),
        json!({"pid": 241, "sessionId": "another-session", "procStart": "Mon Oct 12 11:39:11 2026"}),
    ] {
        home.write(".claude/sessions/241.json", std::slice::from_ref(&entry));
        let response = home.search(json!({}), vec![]);
        let row = by_key(&response, "session-a");
        assert_eq!(row["state"], "ended", "{entry}");
        assert!(row.get("outside").is_none());
        assert!(!row["resume"].is_null());
    }
}

#[test]
fn registry_start_times_allow_space_padded_days() {
    let home = running_claude();
    home.write(
        ".claude/sessions/241.json",
        &[json!({"pid": 241, "sessionId": "session-a", "procStart": "Fri Oct  9 07:46:31 2026"})],
    );
    home.processes.lock().unwrap()[0].started_at = "Fri Oct 9 07:46:31 2026".into();
    let response = home.search(json!({}), vec![]);
    assert_eq!(by_key(&response, "session-a")["state"], "outside");
}

#[test]
fn a_running_session_blocks_resume_even_when_its_folder_or_record_disappears() {
    let home = running_claude();
    std::fs::remove_dir(home.root.join("project")).unwrap();
    let response = home.search(json!({}), vec![]);
    let row = by_key(&response, "session-a");
    assert_eq!(row["state"], "outside");
    assert_eq!(row["place_state"], "gone");
    assert_eq!(row["resume"], Value::Null);
    assert_eq!(row["resume_blocked"], "outside");

    home.write(
        ".claude/history.jsonl",
        &[
            json!({"sessionId": "session-a", "project": home.root.join("project"),
            "display": "Continue the work", "timestamp": 1791805151000_u64}),
        ],
    );
    std::fs::remove_file(home.root.join(".claude/projects/project/session-a.jsonl")).unwrap();
    home.catalog.refresh();
    assert_eq!(
        by_key(&home.search(json!({}), vec![]), "session-a")["state"],
        "outside"
    );
    home.processes.lock().unwrap().clear();
    let response = home.search(json!({}), vec![]);
    let row = by_key(&response, "session-a");
    assert_eq!(row["state"], "record_gone");
    assert_eq!(row["resume_blocked"], "record_gone");
    assert!(row.get("outside").is_none());
}

#[test]
fn an_unavailable_process_view_does_not_offer_an_optimistic_resume() {
    let mut home = running_claude();
    assert_eq!(
        by_key(&home.search(json!({}), vec![]), "session-a")["state"],
        "outside"
    );
    home.catalog = Arc::new(Catalog::new(Desk {
        home: Some(home.root.clone()),
        processes: Box::new(|| Err(std::io::Error::other("process view unavailable"))),
    }));
    home.catalog.refresh();
    let response = home.search(json!({}), vec![]);
    assert_eq!(response["error"]["code"], "server_unavailable");
    assert!(response.get("result").is_none());
}

#[test]
fn an_unreadable_running_registry_does_not_offer_an_optimistic_resume() {
    let home = running_claude();
    assert_eq!(
        by_key(&home.search(json!({}), vec![]), "session-a")["state"],
        "outside"
    );
    std::fs::remove_dir_all(home.root.join(".claude/sessions")).unwrap();
    home.write(".claude/sessions", &[]);
    let response = home.search(json!({}), vec![]);
    assert_eq!(response["error"]["code"], "server_unavailable");
    assert!(response.get("result").is_none());
}

#[test]
fn a_stalled_process_view_obeys_the_request_deadline() {
    let mut home = running_claude();
    let (release, held) = std::sync::mpsc::channel::<()>();
    let held = Mutex::new(held);
    home.catalog = Arc::new(Catalog::new(Desk {
        home: Some(home.root.clone()),
        processes: Box::new(move || {
            let _ = held.lock().unwrap().recv_timeout(Duration::from_secs(2));
            Ok(Vec::new())
        }),
    }));
    home.catalog.refresh();
    let began = Instant::now();
    let response = respond_with_budget(
        "search".into(),
        &ServerSearchSessionsParams::default(),
        Sources {
            live: Box::new(|| json!([]).to_string()),
            catalog: Arc::clone(&home.catalog),
            now: Arc::new(Instant::now),
        },
        Duration::from_millis(100),
    );
    drop(release);
    assert!(began.elapsed() < Duration::from_secs(1));
    let response: Value = serde_json::from_str(&response).unwrap();
    assert_eq!(response["result"]["partial"], true);
    assert_eq!(response["result"]["sessions"], json!([]));
}
