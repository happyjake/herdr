use super::*;
use crate::platform::SessionProcess;

#[test]
fn logbook_merge_retains_pi_id_for_outside_detection() {
    let home = FixtureHome::new();
    let cwd = home.dir("project");
    let path = home.write(
        ".pi/agent/sessions/project/session-a.jsonl",
        &[json!({"type": "session", "id": "pi-a", "cwd": cwd,
            "timestamp": "2026-10-12T11:39:11Z"})],
    );
    home.processes.lock().unwrap().push(SessionProcess {
        pid: 243,
        started_at: "Mon Oct 12 11:39:11 2026".into(),
        argv: vec!["pi".into(), "--session-id".into(), "pi-a".into()],
        cwd: None,
    });
    home.catalog.refresh();
    let key = path.to_str().unwrap();
    assert_eq!(
        by_key(&home.search_all(json!({}), vec![]), key)["state"],
        "outside"
    );

    let session_key = json!({"agent": "pi", "kind": "path", "value": key});
    home.write(
        "logbook.jsonl",
        &[
            json!({"type": "session_reported", "key": session_key, "pane": "w1:p1",
            "terminal": "terminal-a", "cwd": cwd, "at": 1791805151000_u64}),
            json!({"type": "label_worn", "key": session_key, "pane": "w1:p1",
            "label": "Saved task", "source": "person", "at": 1791805151000_u64}),
            json!({"type": "pane_closed", "pane": "w1:p1", "at": 1791805152000_u64}),
        ],
    );
    home.catalog.refresh();
    let response = home.search(json!({}), vec![]);
    let row = by_key(&response, key);
    assert_eq!(row["state"], "outside");
    assert_eq!(row["outside"], json!({"pid": 243, "detail": "pi pid 243"}));
    assert_eq!(row["resume"], Value::Null);
    assert_eq!(row["resume_blocked"], "outside");
    assert_eq!(row["headless"], false);
    assert_eq!(row["labels"][0]["label"], "Saved task");
}

#[test]
fn a_torn_live_registry_entry_is_retried_before_returning_a_resume() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let mut home = running_claude();
    let path = home.root.join(".claude/sessions/241.json");
    let complete = std::fs::read(&path).unwrap();
    std::fs::write(&path, []).unwrap();
    let sleeps = Arc::new(AtomicUsize::new(0));
    let observed_sleeps = Arc::clone(&sleeps);
    let processes = Arc::clone(&home.processes);
    home.catalog = Arc::new(Catalog::new(Desk {
        home: Some(home.root.clone()),
        logbook: home.root.join("logbook.jsonl"),
        processes: Box::new(move || Ok(processes.lock().unwrap().clone())),
        registry_retry_sleep: Box::new(move |delay| {
            assert_eq!(delay, Duration::from_millis(20));
            // Keep the first two reads torn; only the third can identify the session.
            if observed_sleeps.fetch_add(1, Ordering::SeqCst) == 1 {
                std::fs::write(&path, &complete).unwrap();
            }
        }),
    }));
    home.catalog.refresh();

    let response = home.search_all(json!({}), vec![]);
    let row = by_key(&response, "session-a");
    assert_eq!(row["state"], "outside");
    assert_eq!(row["resume"], Value::Null);
    assert_eq!(row["resume_blocked"], "outside");
    assert_eq!(sleeps.load(Ordering::SeqCst), 2);
}

#[test]
fn a_persistently_torn_registry_entry_uses_the_live_claude_resume_operand() {
    let home = running_claude();
    std::fs::write(home.root.join(".claude/sessions/241.json"), []).unwrap();
    for flag in ["--resume", "-r", "--session-id"] {
        home.processes.lock().unwrap()[0].argv =
            vec!["claude".into(), flag.into(), "session-a".into()];
        let response = home.search_all(json!({}), vec![]);
        let row = by_key(&response, "session-a");
        assert_eq!(row["state"], "outside", "{flag}");
        assert_eq!(
            row["outside"],
            json!({"pid": 241, "detail": "claude pid 241"})
        );
        assert_eq!(row["resume"], Value::Null);
        assert_eq!(row["resume_blocked"], "outside");
    }
}

#[test]
fn a_persistently_torn_registry_without_an_argv_identity_leaves_resume_available() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let mut home = running_claude();
    std::fs::write(home.root.join(".claude/sessions/241.json"), []).unwrap();
    let sleeps = Arc::new(AtomicUsize::new(0));
    let observed_sleeps = Arc::clone(&sleeps);
    let processes = Arc::clone(&home.processes);
    home.catalog = Arc::new(Catalog::new(Desk {
        home: Some(home.root.clone()),
        logbook: home.root.join("logbook.jsonl"),
        processes: Box::new(move || Ok(processes.lock().unwrap().clone())),
        registry_retry_sleep: Box::new(move |_| {
            observed_sleeps.fetch_add(1, Ordering::SeqCst);
        }),
    }));
    home.catalog.refresh();
    let response = home.search_all(json!({}), vec![]);
    let row = by_key(&response, "session-a");
    assert_eq!(row["state"], "ended");
    assert_eq!(row["resume"]["command"], "claude --resume session-a");
    assert!(row.get("outside").is_none());
    assert_eq!(sleeps.load(Ordering::SeqCst), 2);
}

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
        cwd: None,
        pid: 241,
        started_at: "Mon Oct 12 11:39:11 2026".into(),
        argv: vec!["claude".into()],
    });
    home
}

#[test]
fn a_running_claude_registry_entry_blocks_resume_outside_the_server() {
    let home = running_claude();
    let response = home.search_all(json!({}), vec![]);
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
        by_key(&home.search_all(json!({}), vec![]), "session-a")["state"],
        "outside"
    );
    home.processes.lock().unwrap().clear();

    let response = home.search_all(json!({}), vec![]);
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
        by_key(&home.search_all(json!({}), vec![]), "session-a")["state"],
        "outside"
    );
    for started_at in ["Mon Oct 12 11:39:12 2026", ""] {
        home.processes.lock().unwrap()[0].started_at = started_at.into();
        let response = home.search_all(json!({}), vec![]);
        let row = by_key(&response, "session-a");
        assert_eq!(row["state"], "ended", "{started_at:?}");
        assert_eq!(row["resume"]["command"], "claude --resume session-a");
        assert!(row.get("outside").is_none());
    }
}

#[test]
fn this_servers_live_pane_wins_over_a_running_registry_entry() {
    let home = running_claude();
    let unrelated = home.search_all(json!({}), vec![live_pane("codex", "id", "session-a")]);
    assert_eq!(by_key(&unrelated, "session-a")["state"], "outside");

    let response = home.search_all(json!({}), vec![live_pane("claude", "id", "session-a")]);
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
        cwd: None,
        pid: 242,
        started_at: "Mon Oct 12 11:39:11 2026".into(),
        argv: vec!["/usr/bin/codex".into(), "resume".into(), "session-a".into()],
    });

    let response = home.search_all(json!({}), vec![]);
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
            cwd: None,
            pid: 243,
            started_at: "Mon Oct 12 11:39:11 2026".into(),
            argv: argv.iter().map(|arg| (*arg).into()).collect(),
        }];
        let response = home.search_all(json!({}), vec![]);
        let row = by_key(&response, key);
        assert_eq!(row["state"], "outside", "{argv:?}");
        assert_eq!(row["outside"], json!({"pid": 243, "detail": "pi pid 243"}));
        assert_eq!(row["resume"], Value::Null);
        assert_eq!(row["resume_blocked"], "outside");
    }
}

#[test]
fn pi_session_paths_resolve_dot_components_and_relative_paths_from_process_cwd() {
    let home = FixtureHome::new();
    let cwd = home.dir("project");
    let path = home.write(
        ".pi/agent/sessions/project/record with spaces.jsonl",
        &[json!({"type": "session", "id": "pi-a", "cwd": cwd})],
    );
    home.catalog.refresh();
    let parent = path.parent().unwrap();
    let key = path.to_str().unwrap();
    let absolute_dot = format!("{}/./record with spaces.jsonl", parent.display());
    let absolute_parent = format!("{}/unused/../record with spaces.jsonl", parent.display());
    for (operand, process_cwd, outside) in [
        (absolute_dot.as_str(), None, true),
        (absolute_parent.as_str(), None, true),
        (
            "./record with spaces.jsonl",
            Some(parent.to_path_buf()),
            true,
        ),
        (
            "unused/../record with spaces.jsonl",
            Some(parent.to_path_buf()),
            true,
        ),
        ("record with spaces.jsonl", Some(parent.to_path_buf()), true),
        ("record with spaces.jsonl", None, false),
        ("record with spaces.jsonl", Some(home.root.clone()), false),
        ("pi-a", None, true),
        ("./pi-a", Some(parent.to_path_buf()), false),
    ] {
        *home.processes.lock().unwrap() = vec![SessionProcess {
            pid: 243,
            started_at: "Mon Oct 12 11:39:11 2026".into(),
            argv: vec!["pi".into(), "--session".into(), operand.into()],
            cwd: process_cwd,
        }];
        let response = home.search_all(json!({}), vec![]);
        let row = by_key(&response, key);
        assert_eq!(
            row["state"],
            if outside { "outside" } else { "ended" },
            "{operand}"
        );
        assert_eq!(row["resume"].is_null(), outside, "{operand}");
    }

    for identity in ["pi-a", "PI-A", "./pi-a", key, "record with spaces.jsonl"] {
        *home.processes.lock().unwrap() = vec![SessionProcess {
            pid: 243,
            started_at: "Mon Oct 12 11:39:11 2026".into(),
            argv: vec!["pi".into(), "--session-id".into(), identity.into()],
            cwd: Some(parent.to_path_buf()),
        }];
        let response = home.search_all(json!({}), vec![]);
        assert_eq!(
            by_key(&response, key)["state"],
            if identity == "pi-a" {
                "outside"
            } else {
                "ended"
            },
            "{identity}"
        );
    }
}

#[test]
fn pi_fork_sources_and_other_operands_do_not_own_the_saved_session() {
    let home = FixtureHome::new();
    let cwd = home.dir("project");
    let path = home.write(
        ".pi/agent/sessions/project/session-a.jsonl",
        &[json!({"type": "session", "id": "pi-a", "cwd": cwd})],
    );
    home.catalog.refresh();
    let key = path.to_str().unwrap();
    for (args, outside) in [
        (vec!["--session", key], true),
        (vec!["--resume", "--session", key], true),
        (vec!["--session-id", "pi-a"], true),
        (vec!["--fork", key], false),
        (vec!["--session", key, "--fork", key], false),
        (vec![key], false),
        (vec!["--system-prompt", key], false),
        (vec!["--session", "another-session", key], false),
        (vec!["--resume", key], false),
        (vec!["-r", key], false),
        (vec!["--", "--session", key], false),
    ] {
        *home.processes.lock().unwrap() = vec![SessionProcess {
            cwd: None,
            pid: 243,
            started_at: "Mon Oct 12 11:39:11 2026".into(),
            argv: std::iter::once("pi")
                .chain(args.iter().copied())
                .map(str::to_string)
                .collect(),
        }];
        let response = home.search_all(json!({}), vec![]);
        let row = by_key(&response, key);
        assert_eq!(
            row["state"],
            if outside { "outside" } else { "ended" },
            "{args:?}"
        );
        assert_eq!(row["resume"].is_null(), outside, "{args:?}");
        assert_eq!(row.get("outside").is_some(), outside, "{args:?}");
    }
}

#[test]
fn codex_only_the_resume_operand_owns_the_saved_session() {
    let home = FixtureHome::new();
    let cwd = home.dir("project");
    home.write(
        ".codex/sessions/2026/10/12/rollout-session-a.jsonl",
        &[json!({"type": "session_meta", "payload": {"id": "session-a", "cwd": cwd, "source": "cli"}})],
    );
    home.catalog.refresh();
    for (args, outside) in [
        (vec!["resume", "session-a"], true),
        (vec!["fork", "session-a"], false),
        (vec!["exec", "session-a"], false),
        (vec!["resume", "another-session", "session-a"], false),
        (vec!["resume", "--last", "session-a"], false),
        (vec!["--", "resume", "session-a"], false),
    ] {
        *home.processes.lock().unwrap() = vec![SessionProcess {
            cwd: None,
            pid: 242,
            started_at: "Mon Oct 12 11:39:11 2026".into(),
            argv: std::iter::once("codex")
                .chain(args.iter().copied())
                .map(str::to_string)
                .collect(),
        }];
        let response = home.search_all(json!({}), vec![]);
        let row = by_key(&response, "session-a");
        assert_eq!(
            row["state"],
            if outside { "outside" } else { "ended" },
            "{args:?}"
        );
        assert_eq!(row["resume"].is_null(), outside, "{args:?}");
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
            cwd: None,
            pid: 242,
            started_at: "Mon Oct 12 11:39:11 2026".into(),
            argv: argv.iter().map(|arg| (*arg).into()).collect(),
        }];
        let response = home.search_all(json!({}), vec![]);
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
            cwd: None,
            pid: 242,
            started_at: String::new(),
            argv: vec!["codex".into(), "resume".into(), "session-a".into()],
        },
        SessionProcess {
            cwd: None,
            pid: 243,
            started_at: String::new(),
            argv: vec!["pi".into(), "--session".into(), path.into()],
        },
    ];
    let outside = home.search_all(json!({}), vec![]);
    for key in ["session-a", path] {
        assert_eq!(by_key(&outside, key)["state"], "outside");
    }
    let response = home.search_all(
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
        by_key(&home.search_all(json!({}), vec![]), "session-a")["state"],
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
        by_key(&home.search_all(json!({}), vec![]), "session-a")["state"],
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
        let response = home.search_all(json!({}), vec![]);
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
    let response = home.search_all(json!({}), vec![]);
    assert_eq!(by_key(&response, "session-a")["state"], "outside");
}

#[test]
fn a_running_session_blocks_resume_even_when_its_folder_or_record_disappears() {
    let home = running_claude();
    std::fs::remove_dir(home.root.join("project")).unwrap();
    let response = home.search_all(json!({}), vec![]);
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
        by_key(&home.search_all(json!({}), vec![]), "session-a")["state"],
        "outside"
    );
    home.processes.lock().unwrap().clear();
    let response = home.search_all(json!({}), vec![]);
    let row = by_key(&response, "session-a");
    assert_eq!(row["state"], "record_gone");
    assert_eq!(row["resume_blocked"], "record_gone");
    assert!(row.get("outside").is_none());
}

#[test]
fn an_unavailable_process_view_does_not_offer_an_optimistic_resume() {
    let mut home = running_claude();
    assert_eq!(
        by_key(&home.search_all(json!({}), vec![]), "session-a")["state"],
        "outside"
    );
    home.catalog = Arc::new(Catalog::new(Desk {
        home: Some(home.root.clone()),
        logbook: home.root.join("logbook.jsonl"),
        processes: Box::new(|| Err(std::io::Error::other("process view unavailable"))),
        registry_retry_sleep: Box::new(|_| {}),
    }));
    home.catalog.refresh();
    let response = home.search_all(json!({}), vec![]);
    assert_eq!(response["error"]["code"], "server_unavailable");
    assert!(response.get("result").is_none());
}

#[test]
fn an_unreadable_running_registry_does_not_offer_an_optimistic_resume() {
    let home = running_claude();
    assert_eq!(
        by_key(&home.search_all(json!({}), vec![]), "session-a")["state"],
        "outside"
    );
    std::fs::remove_dir_all(home.root.join(".claude/sessions")).unwrap();
    home.write(".claude/sessions", &[]);
    let response = home.search_all(json!({}), vec![]);
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
        logbook: home.root.join("logbook.jsonl"),
        processes: Box::new(move || {
            let _ = held.lock().unwrap().recv_timeout(Duration::from_secs(2));
            Ok(Vec::new())
        }),
        registry_retry_sleep: Box::new(|_| {}),
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
