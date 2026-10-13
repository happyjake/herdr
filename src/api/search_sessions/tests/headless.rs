use super::*;

#[test]
fn browse_returns_pre_logbook_interactive_sessions() {
    let home = FixtureHome::new();
    let cwd = home.dir("project");
    let mut keys = Vec::new();
    for (id, entrypoint) in [
        ("claude-legacy", None),
        ("claude-cli", Some("cli")),
        ("claude-unknown", Some("unknown")),
    ] {
        let mut prompt = claude_prompt(id, &cwd, "Interactive work", "2026-10-03T00:00:00Z");
        if let Some(entrypoint) = entrypoint {
            prompt["entrypoint"] = json!(entrypoint);
        }
        home.write(&format!(".claude/projects/project/{id}.jsonl"), &[prompt]);
        keys.push(id.to_string());
    }
    for (id, source) in [
        ("codex-legacy", None),
        ("codex-cli", Some(json!("cli"))),
        ("codex-vscode", Some(json!("vscode"))),
        ("codex-unknown", Some(json!({"other": "exec"}))),
    ] {
        let mut meta = json!({"type": "session_meta", "timestamp": "2026-10-03T00:00:00Z",
            "payload": {"id": id, "cwd": cwd}});
        if let Some(source) = source {
            meta["payload"]["source"] = source;
        }
        home.write(
            &format!(".codex/sessions/2026/10/03/{id}.jsonl"),
            &[
                meta,
                codex_prompt("Interactive work", "2026-10-03T00:01:00Z"),
            ],
        );
        keys.push(id.to_string());
    }
    let pi = home.write(
        ".pi/agent/sessions/project/session.jsonl",
        &[
            json!({"type": "session", "id": "pi-session", "cwd": cwd,
                "timestamp": "2026-10-03T00:00:00Z"}),
            json!({"type": "message", "timestamp": "2026-10-03T00:01:00Z",
                "message": {"role": "user", "content": "Interactive work"}}),
        ],
    );
    keys.push(pi.to_str().unwrap().to_string());
    home.catalog.refresh();

    for params in [
        json!({}),
        json!({"include_headless": false}),
        json!({"include_headless": true}),
        json!({"terms": ["Interactive work"]}),
    ] {
        let found = home.search(params, vec![]);
        assert_eq!(found["result"]["partial"], false);
        assert_eq!(found["result"]["scanned"], keys.len());
        assert_eq!(
            found["result"]["sessions"].as_array().unwrap().len(),
            keys.len()
        );
        for key in &keys {
            let row = by_key(&found, key);
            assert_eq!(row["headless"], false);
            assert!(row.get("parent").is_none());
            assert_eq!(row["subagent_count"], 0);
        }
    }
}

fn assert_headless_children(home: &FixtureHome, agent: &str) {
    home.catalog.refresh();
    let all = home.search(json!({"include_headless": true}), vec![]);
    assert_eq!(all["result"]["partial"], false);
    assert_eq!(all["result"]["scanned"], 3);
    assert_eq!(all["result"]["sessions"].as_array().unwrap().len(), 3);
    assert_eq!(by_key(&all, "child")["headless"], true);
    assert_eq!(by_key(&all, "alone")["headless"], true);
    assert!(by_key(&all, "alone").get("parent").is_none());
    assert_eq!(
        by_key(&all, "child")["parent"],
        json!({"agent": agent, "kind": "id", "value": "parent"})
    );
    assert_eq!(by_key(&all, "parent")["headless"], false);
    assert_eq!(by_key(&all, "parent")["subagent_count"], 1);

    for params in [json!({}), json!({"include_headless": false})] {
        let visible = home.search(params, vec![]);
        assert_eq!(visible["result"]["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(by_key(&visible, "parent")["subagent_count"], 1);
    }
    let found = home.search(json!({"terms": ["Child work"]}), vec![]);
    assert_eq!(found["result"]["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(
        by_key(&found, "parent")["why"]["line"],
        "Subagent: Child work"
    );
    assert_eq!(
        home.search(json!({"terms": ["Standalone work"]}), vec![])["result"]["sessions"],
        json!([])
    );
}

#[test]
fn claude_sdk_cli_records_are_headless_after_history_merge() {
    let home = FixtureHome::new();
    let cwd = home.dir("project");
    home.write(
        ".claude/projects/project/parent.jsonl",
        &[
            claude_prompt("parent", &cwd, "Parent work", "2026-10-03T00:00:00Z"),
            claude_prompt("parent", &cwd, "Latest work", "2026-10-03T00:10:00Z"),
        ],
    );
    for (id, text, at) in [
        ("child", "Child work", "2026-10-03T00:05:00Z"),
        ("alone", "Standalone work", "2026-10-03T01:00:00Z"),
    ] {
        let mut later = claude_prompt(id, &cwd, text, at);
        later["entrypoint"] = json!("cli");
        home.write(
            &format!(".claude/projects/project/{id}.jsonl"),
            &[
                claude_prompt(id, &cwd, text, at),
                json!({"type": "assistant", "entrypoint": "sdk-cli", "timestamp": at}),
                later,
            ],
        );
    }
    home.write(
        ".claude/history.jsonl",
        &[
            json!({"sessionId": "child", "project": cwd, "display": "Child work", "timestamp": 1790985900000_u64}),
            json!({"sessionId": "alone", "project": cwd, "display": "Standalone work", "timestamp": 1790989200000_u64}),
        ],
    );
    assert_headless_children(&home, "claude");
}

fn codex_headless_home(source: impl Fn(&str) -> Value) -> FixtureHome {
    let home = FixtureHome::new();
    let cwd = home.dir("project");
    home.write(
        ".codex/sessions/2026/10/03/parent.jsonl",
        &[
            json!({"type": "session_meta", "timestamp": "2026-10-03T00:00:00Z",
                "payload": {"id": "parent", "cwd": cwd, "source": "cli"}}),
            codex_prompt("Parent work", "2026-10-03T00:10:00Z"),
        ],
    );
    for (id, parent, text, at) in [
        ("child", "parent", "Child work", "2026-10-03T00:05:00Z"),
        ("alone", "absent", "Standalone work", "2026-10-03T01:00:00Z"),
    ] {
        home.write(
            &format!(".codex/sessions/2026/10/03/{id}.jsonl"),
            &[
                json!({"type": "session_meta", "timestamp": at,
                    "payload": {"id": id, "cwd": cwd, "source": source(parent)}}),
                codex_prompt(text, at),
            ],
        );
    }
    home.write(
        ".codex/history.jsonl",
        &[
            json!({"session_id": "child", "text": "Child work", "ts": 1790985900}),
            json!({"session_id": "alone", "text": "Standalone work", "ts": 1790989200}),
        ],
    );
    home
}

#[test]
fn codex_exec_records_are_headless_after_history_merge() {
    let home = codex_headless_home(|_| json!("exec"));
    assert_headless_children(&home, "codex");
}

#[test]
fn codex_review_records_fold_into_interactive_windows_or_stay_hidden() {
    let home = codex_headless_home(|_| json!({"subagent": "review"}));
    assert_headless_children(&home, "codex");
}

#[test]
fn codex_spawn_records_keep_structural_parents_or_stay_hidden() {
    let home = codex_headless_home(
        |parent| json!({"subagent": {"thread_spawn": {"parent_thread_id": parent}}}),
    );
    let cwd = home.dir("worker");
    home.write(
        ".codex/sessions/2026/10/03/child.jsonl",
        &[
            json!({"type": "session_meta", "timestamp": "2026-10-03T00:05:00Z",
                "payload": {"id": "child", "cwd": cwd, "parent_thread_id": "parent",
                    "source": {"subagent": {"thread_spawn": {"parent_thread_id": "parent"}}}}}),
            codex_prompt("Child work", "2026-10-03T00:05:00Z"),
        ],
    );
    assert_headless_children(&home, "codex");
}

#[test]
fn codex_other_subagent_objects_are_headless() {
    let home = codex_headless_home(|_| json!({"subagent": {"other": {}}}));
    assert_headless_children(&home, "codex");
}

fn marked_records_home() -> FixtureHome {
    let home = codex_headless_home(|_| json!("exec"));
    let cwd = home.dir("project");
    let mut prompt = claude_prompt("sdk", &cwd, "SDK work", "2026-10-03T00:05:00Z");
    prompt["entrypoint"] = json!("sdk-cli");
    home.write(".claude/projects/project/sdk.jsonl", &[prompt]);
    home.catalog.refresh();
    let before = home.search(json!({}), vec![]);
    assert_eq!(before["result"]["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(by_key(&before, "parent")["subagent_count"], 2);
    home
}

#[test]
fn logbook_reports_override_non_interactive_markers() {
    let home = marked_records_home();
    let cwd = home.dir("project");
    home.write(
        "logbook.jsonl",
        &[
            folding::reported("claude", "sdk", "w1:p1", &cwd, 1790985900000),
            folding::reported("codex", "child", "w2:p1", &cwd, 1790985900000),
            json!({"type": "pane_closed", "pane": "w1:p1", "at": 1790985960000_u64}),
            json!({"type": "pane_closed", "pane": "w2:p1", "at": 1790985960000_u64}),
        ],
    );
    home.catalog.refresh();
    let found = home.search(json!({}), vec![]);
    assert_eq!(found["result"]["sessions"].as_array().unwrap().len(), 3);
    for id in ["parent", "sdk", "child"] {
        let row = by_key(&found, id);
        assert_eq!(row["headless"], false);
        assert!(row.get("parent").is_none());
        assert_eq!(row["subagent_count"], 0);
        assert_eq!(row["state"], "ended");
    }
    let all = home.search(json!({"include_headless": true}), vec![]);
    assert_eq!(by_key(&all, "alone")["headless"], true);
}

#[test]
fn live_panes_override_non_interactive_markers_without_logbook_history() {
    let home = marked_records_home();
    for (agent, id) in [("claude", "sdk"), ("codex", "child")] {
        let found = home.search(json!({}), vec![live_pane(agent, "id", id)]);
        assert_eq!(found["result"]["sessions"].as_array().unwrap().len(), 2);
        let live = by_key(&found, id);
        assert_eq!(live["headless"], false);
        assert!(live.get("parent").is_none());
        assert_eq!(live["state"], "live");
        assert_eq!(live["resume_blocked"], "live");
        assert_eq!(by_key(&found, "parent")["headless"], false);
    }
    let detached = home.search(json!({}), vec![]);
    assert_eq!(detached["result"]["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(by_key(&detached, "parent")["subagent_count"], 2);
}

#[test]
fn codex_sources_outside_session_meta_do_not_hide_interactive_records() {
    let home = FixtureHome::new();
    let cwd = home.dir("project");
    let mut prompt = codex_prompt("Interactive work", "2026-10-03T00:01:00Z");
    prompt["payload"]["source"] = json!("exec");
    home.write(
        ".codex/sessions/2026/10/03/session.jsonl",
        &[
            json!({"type": "session_meta", "timestamp": "2026-10-03T00:00:00Z",
                "payload": {"id": "interactive", "cwd": cwd, "source": "cli"}}),
            prompt,
            json!({"type": "event_msg", "payload": {"source": {"subagent": "review"}}}),
        ],
    );
    home.catalog.refresh();
    let found = home.search(json!({}), vec![]);
    assert_eq!(by_key(&found, "interactive")["headless"], false);
}
