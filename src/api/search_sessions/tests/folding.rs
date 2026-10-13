use super::*;

fn session_key(agent: &str, id: &str) -> Value {
    json!({"agent": agent, "kind": "id", "value": id})
}

pub(super) fn reported(agent: &str, id: &str, pane: &str, cwd: &str, at: u64) -> Value {
    json!({"type": "session_reported", "key": session_key(agent, id),
        "pane": pane, "terminal": format!("terminal-{pane}"), "cwd": cwd, "at": at})
}

#[test]
fn logbook_labels_survive_pane_close_and_record_removal() {
    let home = FixtureHome::new();
    let cwd = home.dir("project");
    let record = home.write(
        ".claude/projects/project/labelled.jsonl",
        &[claude_prompt(
            "labelled",
            &cwd,
            "Find the original work",
            "2026-10-03T00:00:00Z",
        )],
    );
    home.write(".claude/history.jsonl", &[
        json!({"sessionId": "labelled", "project": cwd, "display": "Find the original work", "timestamp": 1790985600000_u64}),
    ]);
    let key = session_key("claude", "labelled");
    home.write("logbook.jsonl", &[
        reported("claude", "labelled", "w1:p1", &cwd, 1790985600000),
        json!({"type": "label_worn", "key": key, "pane": "w1:p1", "label": "Personal name", "source": "person", "at": 1790985601000_u64}),
        json!({"type": "label_worn", "key": key, "pane": "w1:p1", "label": "Vanished auto-name", "source": "editor", "at": 1790985602000_u64}),
        json!({"type": "label_worn", "key": key, "pane": "w1:p1", "label": null, "source": "person", "at": 1790985603000_u64}),
        json!({"type": "label_worn", "key": key, "pane": "w1:p1", "label": "Last closed name", "source": "person", "at": 1790985604000_u64}),
        json!({"type": "pane_closed", "pane": "w1:p1", "at": 1790985605000_u64}),
        reported("claude", "labelled", "w2:p1", &cwd, 1790985606000),
        json!({"type": "label_worn", "key": key, "pane": "w2:p1", "label": "Current name", "source": "editor", "at": 1790985607000_u64}),
    ]);
    home.catalog.refresh();

    let expected = json!([
        {"label": "Personal name", "source": "person", "pane_id": "w1:p1", "from_ms": 1790985601000_u64, "to_ms": 1790985602000_u64},
        {"label": "Vanished auto-name", "source": "editor", "pane_id": "w1:p1", "from_ms": 1790985602000_u64, "to_ms": 1790985603000_u64},
        {"label": "Last closed name", "source": "person", "pane_id": "w1:p1", "from_ms": 1790985604000_u64, "to_ms": 1790985605000_u64},
        {"label": "Current name", "source": "editor", "pane_id": "w2:p1", "from_ms": 1790985607000_u64},
    ]);
    let ended = home.search(json!({"terms": ["VANISHED"]}), vec![]);
    let row = by_key(&ended, "labelled");
    assert_eq!(row["labels"], expected);
    assert_eq!(row["state"], "ended");
    assert_eq!(
        row["why"],
        json!({"field": "label", "line": "Vanished auto-name", "terms": ["VANISHED"]})
    );

    let live = home.search(json!({}), vec![live_pane("claude", "id", "labelled")]);
    assert_eq!(by_key(&live, "labelled")["state"], "live");
    assert_eq!(by_key(&live, "labelled")["labels"], expected);

    std::fs::remove_file(record).unwrap();
    home.catalog.refresh();
    let gone = home.search(json!({"terms": ["vanished"]}), vec![]);
    assert_eq!(by_key(&gone, "labelled")["state"], "record_gone");
    assert_eq!(by_key(&gone, "labelled")["labels"], expected);
}

#[test]
fn structural_subagents_keep_distinct_records_and_fold_under_their_parents() {
    let home = FixtureHome::new();
    let cwd = home.dir("project");
    home.write(
        ".claude/projects/project/claude-parent.jsonl",
        &[claude_prompt(
            "claude-parent",
            &cwd,
            "Parent question",
            "2026-10-03T00:00:00Z",
        )],
    );
    // A nested record can repeat the parent's session id. Its filename identifies the child.
    home.write(
        ".claude/projects/project/claude-parent/subagents/agent-child.jsonl",
        &[claude_prompt(
            "claude-parent",
            &cwd,
            "Child question",
            "2026-10-03T00:01:00Z",
        )],
    );
    for (id, source) in [
        ("codex-parent", json!("cli")),
        (
            "codex-child",
            json!({"subagent": {"thread_spawn": {"parent_thread_id": "codex-parent"}}}),
        ),
    ] {
        home.write(
            &format!(".codex/sessions/2026/10/03/{id}.jsonl"),
            &[
                json!({"type": "session_meta", "timestamp": "2026-10-03T00:00:00Z",
                "payload": {"id": id, "cwd": cwd, "source": source}}),
                codex_prompt("Codex question", "2026-10-03T00:01:00Z"),
            ],
        );
    }
    home.write(".claude/history.jsonl", &[
        json!({"sessionId": "logged-child", "project": cwd, "display": "Retained question", "timestamp": 1790985600000_u64}),
    ]);
    home.write("logbook.jsonl", &[
        reported("claude", "claude-parent", "w1:p1", &cwd, 1790985600000),
        reported("codex", "codex-parent", "w1:p2", &cwd, 1790985600000),
        reported("claude", "logged-child", "w1:p3", &cwd, 1790985600000),
        json!({"type": "parent", "child": session_key("claude", "logged-child"), "parent": session_key("claude", "claude-parent")}),
    ]);
    home.catalog.refresh();

    let visible = home.search(json!({}), vec![]);
    assert_eq!(visible["result"]["sessions"].as_array().unwrap().len(), 2);
    assert_eq!(by_key(&visible, "claude-parent")["subagent_count"], 2);
    assert_eq!(by_key(&visible, "codex-parent")["subagent_count"], 1);
    let all = home.search(json!({"include_headless": true}), vec![]);
    assert_eq!(all["result"]["scanned"], 5);
    assert_eq!(all["result"]["sessions"].as_array().unwrap().len(), 5);
    for (child, agent, parent, headless) in [
        ("agent-child", "claude", "claude-parent", true),
        ("codex-child", "codex", "codex-parent", true),
        ("logged-child", "claude", "claude-parent", false),
    ] {
        assert_eq!(by_key(&all, child)["parent"], session_key(agent, parent));
        assert_eq!(by_key(&all, child)["subagent_count"], 0);
        assert_eq!(by_key(&all, child)["headless"], headless);
    }
    assert_eq!(
        by_key(&all, "claude-parent")["first_prompt"],
        "Parent question"
    );
    assert_eq!(
        by_key(&all, "agent-child")["first_prompt"],
        "Child question"
    );
    let limited = home.search(json!({"terms": ["Parent question"], "limit": 1}), vec![]);
    assert_eq!(by_key(&limited, "claude-parent")["subagent_count"], 2);
}

#[test]
fn headless_records_fold_only_into_pane_hosted_windows_at_the_same_place() {
    let home = FixtureHome::new();
    let cwd = home.dir("project");
    let elsewhere = home.dir("elsewhere");
    for (id, hour) in [("live-host", 0), ("logged-host", 1), ("unhosted", 2)] {
        home.write(
            &format!(".claude/projects/project/{id}.jsonl"),
            &[
                claude_prompt(
                    id,
                    &cwd,
                    "Parent work",
                    &format!("2026-10-03T0{hour}:00:00Z"),
                ),
                claude_prompt(
                    id,
                    &cwd,
                    "Latest work",
                    &format!("2026-10-03T0{hour}:10:00Z"),
                ),
            ],
        );
    }
    for (id, place, at) in [
        ("review-live", &cwd, "2026-10-03T00:05:00Z"),
        ("review-logged", &cwd, "2026-10-03T01:05:00Z"),
        ("alone", &cwd, "2026-10-03T02:05:00Z"),
        ("other-place", &elsewhere, "2026-10-03T00:05:00Z"),
        ("before", &cwd, "2026-10-02T23:59:59Z"),
        ("after", &cwd, "2026-10-03T01:10:00.001Z"),
    ] {
        home.write(
            &format!(".codex/sessions/2026/10/03/{id}.jsonl"),
            &[
                json!({"type": "session_meta", "timestamp": at,
                "payload": {"id": id, "cwd": place, "source": {"subagent": "review"}}}),
                codex_prompt("Review work", at),
            ],
        );
    }
    home.write(
        ".claude/projects/project/hosted-child.jsonl",
        &[claude_prompt(
            "hosted-child",
            &cwd,
            "Independent pane",
            "2026-10-03T00:06:00Z",
        )],
    );
    home.write(
        "logbook.jsonl",
        &[
            reported("claude", "logged-host", "w2:p1", &cwd, 1790989200000),
            json!({"type": "pane_closed", "pane": "w2:p1", "at": 1790989800000_u64}),
            reported("claude", "hosted-child", "w2:p2", &cwd, 1790985960000),
            json!({"type": "pane_closed", "pane": "w2:p2", "at": 1790985960000_u64}),
        ],
    );
    home.catalog.refresh();
    let panes = vec![live_pane("claude", "id", "live-host")];
    let all = home.search(json!({"include_headless": true}), panes.clone());
    for (child, parent) in [
        ("review-live", "live-host"),
        ("review-logged", "logged-host"),
    ] {
        assert_eq!(by_key(&all, child)["headless"], true);
        assert_eq!(by_key(&all, child)["parent"], session_key("claude", parent));
        assert_eq!(by_key(&all, parent)["headless"], false);
        assert_eq!(by_key(&all, parent)["subagent_count"], 1);
    }
    for id in ["alone", "unhosted", "other-place", "before", "after"] {
        assert_eq!(by_key(&all, id)["headless"], true);
        assert!(
            by_key(&all, id).get("parent").is_none(),
            "{id} must stand alone"
        );
    }
    assert_eq!(by_key(&all, "hosted-child")["headless"], false);
    assert!(by_key(&all, "hosted-child").get("parent").is_none());
    for params in [json!({}), json!({"include_headless": false})] {
        let visible = home.search(params, panes.clone());
        assert_eq!(visible["result"]["sessions"].as_array().unwrap().len(), 3);
        assert_eq!(visible["result"]["scanned"], 10);
        assert_eq!(by_key(&visible, "live-host")["subagent_count"], 1);
        assert_eq!(by_key(&visible, "logged-host")["subagent_count"], 1);
    }
    // Only the current server's matching agent/key grants live pane ownership.
    let other_server = home.search(
        json!({"include_headless": true}),
        vec![live_pane("codex", "id", "live-host")],
    );
    assert_eq!(by_key(&other_server, "live-host")["headless"], true);
    assert!(by_key(&other_server, "review-live").get("parent").is_none());
}

#[test]
fn logbook_cache_keeps_label_only_sessions_and_refreshes_changed_history() {
    let home = FixtureHome::new();
    let cwd = home.dir("project");
    let key = session_key("codex", "missing-record");
    let path = home.write("logbook.jsonl", &[
        reported("codex", "missing-record", "w1:p1", &cwd, 1790985600000),
        json!({"type": "label_worn", "key": key, "pane": "w1:p1", "label": "alpha", "source": "person", "at": 1790985601000_u64}),
        json!({"type": "pane_closed", "pane": "w1:p1", "at": 1790985602000_u64}),
    ]);
    home.catalog.refresh();
    let initial = home.search(json!({"terms": ["alpha"]}), vec![]);
    let row = by_key(&initial, "missing-record");
    assert_eq!(row["state"], "record_gone");
    assert_eq!(row["cwd"], cwd);
    assert_eq!(row["headless"], false);
    assert_eq!(row["resume_blocked"], "record_gone");
    assert_eq!(row["why"]["field"], "label");

    let stamp = std::fs::metadata(&path).unwrap().modified().unwrap();
    let original = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, original.replace("alpha", "bravo")).unwrap();
    let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.set_modified(stamp).unwrap();
    assert_eq!(home.search(json!({"terms": ["alpha"]}), vec![]), initial);
    home.catalog.refresh();
    assert_eq!(home.search(json!({"terms": ["alpha"]}), vec![]), initial);
    assert_eq!(
        home.search(json!({"terms": ["bravo"]}), vec![])["result"]["sessions"],
        json!([])
    );

    file.set_modified(stamp + Duration::from_secs(1)).unwrap();
    home.catalog.refresh();
    let changed = home.search(json!({"terms": ["bravo"]}), vec![]);
    assert_eq!(
        by_key(&changed, "missing-record")["labels"][0]["label"],
        "bravo"
    );
    assert_eq!(
        home.search(json!({"terms": ["alpha"]}), vec![])["result"]["sessions"],
        json!([])
    );

    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(file, "{{\"type\":\"label_worn\",\"label\":").unwrap();
    for event in [
        reported("codex", "missing-record", "w1:p1", &cwd, 1790985603000),
        json!({"type": "label_worn", "key": key, "pane": "w1:p1", "label": "resumed", "source": "editor", "at": 1790985604000_u64}),
    ] {
        writeln!(file, "{event}").unwrap();
    }
    file.set_modified(stamp + Duration::from_secs(1)).unwrap();
    assert_eq!(
        home.search(json!({"terms": ["resumed"]}), vec![])["result"]["sessions"],
        json!([])
    );
    home.catalog.refresh();
    let resumed = home.search(json!({"terms": ["resumed"]}), vec![]);
    assert_eq!(resumed["result"]["partial"], false);
    assert_eq!(
        by_key(&resumed, "missing-record")["labels"],
        json!([
            {"label": "bravo", "source": "person", "pane_id": "w1:p1", "from_ms": 1790985601000_u64, "to_ms": 1790985602000_u64},
            {"label": "resumed", "source": "editor", "pane_id": "w1:p1", "from_ms": 1790985604000_u64},
        ])
    );
}

#[test]
fn session_replacement_ends_the_previous_label_and_host_window() {
    let home = FixtureHome::new();
    let cwd = home.dir("project");
    home.write("logbook.jsonl", &[
        reported("claude", "unlabelled", "w1:p1", &cwd, 1790985600000),
        reported("claude", "labelled", "w1:p1", &cwd, 1790986200000),
        json!({"type": "label_worn", "key": session_key("claude", "labelled"), "pane": "w1:p1", "label": "Shared name", "source": "person", "at": 1790986260000_u64}),
        reported("claude", "replacement", "w1:p1", &cwd, 1790986800000),
        json!({"type": "label_worn", "key": session_key("claude", "replacement"), "pane": "w1:p1", "label": "Shared name", "source": "editor", "at": 1790986860000_u64}),
    ]);
    home.write(
        ".codex/sessions/2026/10/03/review.jsonl",
        &[
            json!({"type": "session_meta", "timestamp": "2026-10-03T00:05:00Z",
            "payload": {"id": "review", "cwd": cwd, "source": {"subagent": "review"}}}),
        ],
    );
    home.catalog.refresh();
    let all = home.search(json!({"include_headless": true}), vec![]);
    assert_eq!(
        by_key(&all, "review")["parent"],
        session_key("claude", "unlabelled")
    );
    assert_eq!(by_key(&all, "unlabelled")["subagent_count"], 1);
    assert_eq!(
        by_key(&all, "labelled")["labels"],
        json!([
            {"label": "Shared name", "source": "person", "pane_id": "w1:p1", "from_ms": 1790986260000_u64, "to_ms": 1790986800000_u64},
        ])
    );
    assert_eq!(
        by_key(&all, "replacement")["labels"],
        json!([
            {"label": "Shared name", "source": "editor", "pane_id": "w1:p1", "from_ms": 1790986860000_u64},
        ])
    );
    assert!(by_key(&all, "labelled").get("parent").is_none());
    assert!(by_key(&all, "replacement").get("parent").is_none());
}

#[test]
fn overlapping_host_windows_include_boundaries_and_expire_independently() {
    let home = FixtureHome::new();
    let cwd = home.dir("project");
    home.write(
        "logbook.jsonl",
        &[
            reported("claude", "older-host", "w1:p1", &cwd, 1790985600000),
            reported("claude", "newer-host", "w1:p2", &cwd, 1790986200000),
            json!({"type": "pane_closed", "pane": "w1:p2", "at": 1790986800000_u64}),
            json!({"type": "pane_closed", "pane": "w1:p1", "at": 1790988000000_u64}),
        ],
    );
    for (id, at, place) in [
        ("at-start", "2026-10-03T00:10:00Z", Some(&cwd)),
        ("at-end", "2026-10-03T00:20:00Z", Some(&cwd)),
        ("after-newer", "2026-10-03T00:20:00.001Z", Some(&cwd)),
        ("after-all", "2026-10-03T00:40:00.001Z", Some(&cwd)),
        ("unknown-place", "2026-10-03T00:10:00Z", None),
    ] {
        home.write(
            &format!(".codex/sessions/2026/10/03/{id}.jsonl"),
            &[json!({"type": "session_meta", "timestamp": at,
                "payload": {"id": id, "cwd": place, "source": "exec"}})],
        );
    }
    home.catalog.refresh();
    let all = home.search(json!({"include_headless": true}), vec![]);
    for id in ["at-start", "at-end"] {
        assert_eq!(
            by_key(&all, id)["parent"],
            session_key("claude", "newer-host")
        );
    }
    assert_eq!(
        by_key(&all, "after-newer")["parent"],
        session_key("claude", "older-host")
    );
    assert_eq!(by_key(&all, "newer-host")["subagent_count"], 2);
    assert_eq!(by_key(&all, "older-host")["subagent_count"], 1);
    for id in ["after-all", "unknown-place"] {
        assert!(by_key(&all, id).get("parent").is_none());
        assert_eq!(by_key(&all, id)["headless"], true);
    }
}
