use super::*;
use serde_json::{json, Value};
use std::path::PathBuf;

struct FixtureHome {
    root: PathBuf,
    catalog: Arc<Catalog>,
}

impl FixtureHome {
    fn new() -> Self {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "herdr-session-search-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let catalog = Arc::new(Catalog::new(Desk {
            home: Some(root.clone()),
        }));
        Self { root, catalog }
    }

    fn dir(&self, relative: &str) -> String {
        let path = self.root.join(relative);
        std::fs::create_dir_all(&path).unwrap();
        path.to_str().unwrap().to_string()
    }

    fn write(&self, relative: &str, lines: &[Value]) -> PathBuf {
        let path = self.root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
        std::fs::write(&path, text).unwrap();
        path
    }

    fn search(&self, params: Value, panes: Vec<Value>) -> Value {
        let live =
            json!({"id": "panes", "result": {"type": "pane_list", "panes": panes}}).to_string();
        serde_json::from_str::<Value>(&respond(
            "search".into(),
            &serde_json::from_value(params).unwrap(),
            Sources {
                live: Box::new(move || live),
                catalog: Arc::clone(&self.catalog),
                now: Arc::new(Instant::now),
            },
        ))
        .unwrap()
    }
}

impl Drop for FixtureHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn claude_prompt(id: &str, cwd: &str, text: &str, timestamp: &str) -> Value {
    json!({"type": "user", "sessionId": id, "cwd": cwd, "gitBranch": "feature/search",
        "timestamp": timestamp, "message": {"role": "user", "content": text}})
}

#[test]
fn browse_reads_a_claude_session_and_offers_resume() {
    let home = FixtureHome::new();
    let cwd = home.dir("project");
    home.write(
        ".claude/projects/-home-user-project/session-a.jsonl",
        &[
            claude_prompt(
                "session-a",
                &cwd,
                "Fix the keyboard",
                "2026-10-03T00:00:00.123Z",
            ),
            json!({"type": "ai-title", "aiTitle": "Keyboard focus", "sessionId": "session-a"}),
            claude_prompt("session-a", &cwd, "Check focus", "2026-10-03T01:00:00.456Z"),
        ],
    );
    home.catalog.refresh();

    let response = home.search(json!({}), vec![]);
    assert_eq!(
        response["result"],
        json!({
            "type": "server_search_sessions", "scanned": 1, "partial": false,
            "sessions": [{
                "key": {"agent": "claude", "kind": "id", "value": "session-a"},
                "agent": "claude", "cwd": cwd, "place_state": "present",
                "branch": "feature/search", "title": "Keyboard focus",
                "first_prompt": "Fix the keyboard", "recent_prompts": ["Fix the keyboard", "Check focus"],
                "started_at_ms": 1790985600123_u64, "last_active_at_ms": 1790989200456_u64,
                "labels": [], "state": "ended", "resume": {"command": "claude --resume session-a", "cwd": cwd},
                "headless": false, "subagent_count": 0, "why": null, "score": 0.0
            }]
        })
    );
}

fn codex_prompt(text: &str, timestamp: &str) -> Value {
    json!({"type": "response_item", "timestamp": timestamp, "payload": {
        "type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]
    }})
}

fn by_key<'a>(response: &'a Value, key: &str) -> &'a Value {
    response["result"]["sessions"]
        .as_array()
        .expect("session rows")
        .iter()
        .find(|row| row["key"]["value"] == key)
        .unwrap_or_else(|| panic!("missing {key}: {response}"))
}

#[test]
fn browse_covers_codex_pi_and_both_prompt_histories() {
    let home = FixtureHome::new();
    let cwd = home.dir("project");
    home.write(".codex/sessions/2026/10/03/rollout-2026-10-03-codex-a.jsonl", &[
        json!({"type": "session_meta", "timestamp": "2026-10-03T00:00:00Z",
            "payload": {"id": "codex-a", "cwd": cwd, "source": "exec", "git": {"branch": "feature/codex"}}}),
        codex_prompt("Review the keyboard", "2026-10-03T00:01:00Z"),
    ]);
    home.write(
        ".codex/session_index.jsonl",
        &[
            json!({"id": "codex-a", "thread_name": "Old name"}),
            json!({"id": "codex-a", "thread_name": "Keyboard review"}),
        ],
    );
    let pi_path = home.write(".pi/agent/sessions/--home-user-project--/2026-10-03_pi-a.jsonl", &[
        json!({"type": "session", "id": "pi-a", "cwd": cwd, "timestamp": "2026-10-03T00:00:00Z"}),
        json!({"type": "message", "timestamp": "2026-10-03T00:02:00Z",
            "message": {"role": "user", "content": [{"type": "text", "text": "Check layout"}]}}),
    ]);
    home.write(".claude/history.jsonl", &[
        json!({"display": "Recover the old prompt", "project": cwd, "sessionId": "claude-gone", "timestamp": 1790985600000_u64}),
        json!({"display": "First legacy prompt", "project": cwd, "timestamp": 1790985600000_u64}),
        json!({"display": "Second legacy prompt", "project": cwd, "timestamp": 1790989200000_u64}),
    ]);
    home.write(
        ".codex/history.jsonl",
        &[
            json!({"session_id": "codex-gone", "ts": 1790985600, "text": "Recover another prompt"}),
            json!({"session_id": "codex-a", "ts": 1790985660, "text": "Review the keyboard"}),
        ],
    );
    home.catalog.refresh();

    let response = home.search(json!({}), vec![]);
    assert_eq!(response["result"]["scanned"], 5);
    assert_eq!(response["result"]["partial"], false);
    let codex = by_key(&response, "codex-a");
    assert_eq!(codex["title"], "Keyboard review");
    assert_eq!(codex["branch"], "feature/codex");
    assert_eq!(codex["recent_prompts"], json!(["Review the keyboard"]));
    assert_eq!(
        codex["resume"],
        json!({"command": "codex resume codex-a", "cwd": cwd})
    );
    let pi = by_key(&response, pi_path.to_str().unwrap());
    assert_eq!(pi["key"]["kind"], "path");
    assert_eq!(pi["first_prompt"], "Check layout");
    assert!(pi["resume"]["command"]
        .as_str()
        .unwrap()
        .contains(pi_path.to_str().unwrap()));
    assert!(pi.get("title").is_none());
    for key in ["claude-gone", "codex-gone"] {
        let row = by_key(&response, key);
        assert_eq!(row["state"], "record_gone");
        assert_eq!(row["resume_blocked"], "record_gone");
        assert_eq!(row["resume"], Value::Null);
    }
    assert_eq!(by_key(&response, "claude-gone")["cwd"], cwd);
    assert_eq!(by_key(&response, "codex-gone")["cwd"], Value::Null);
    let day = by_key(&response, &format!("{cwd}|2026-10-03"));
    assert_eq!(day["key"]["kind"], "day");
    assert_eq!(
        day["recent_prompts"],
        json!(["First legacy prompt", "Second legacy prompt"])
    );
    assert_eq!(day["resume"], Value::Null);
}

fn live_pane(agent: &str, kind: &str, value: &str) -> Value {
    json!({
        "pane_id": "w1:p1", "terminal_id": "terminal-a", "workspace_id": "w1", "tab_id": "w1:t1",
        "focused": false, "agent_status": "idle", "revision": 0,
        "label": "A pane label is not folded into search",
        "agent_session": {"agent": agent, "source": format!("herdr:{agent}"), "kind": kind, "value": value}
    })
}

#[test]
fn moved_claude_session_keeps_its_key_and_live_state_blocks_resume() {
    let home = FixtureHome::new();
    let cwd = home.dir("projects/original");
    let original = home.write(
        ".claude/projects/original/session-a.jsonl",
        &[claude_prompt(
            "session-a",
            &cwd,
            "Original prompt",
            "2026-10-03T00:00:00Z",
        )],
    );
    home.write(".claude/history.jsonl", &[
        json!({"display": "Original prompt", "project": cwd, "sessionId": "session-a", "timestamp": 1790985600000_u64}),
    ]);
    home.catalog.refresh();
    let before = home.search(json!({}), vec![]);
    assert_eq!(by_key(&before, "session-a")["state"], "ended");

    let ancestor = home.dir("projects/destination");
    let gone = format!("{ancestor}/removed/worktree");
    let moved = home
        .root
        .join(".claude/projects/destination/session-a.jsonl");
    std::fs::create_dir_all(moved.parent().unwrap()).unwrap();
    std::fs::rename(original, &moved).unwrap();
    use std::io::Write;
    writeln!(
        std::fs::OpenOptions::new()
            .append(true)
            .open(moved)
            .unwrap(),
        "{}",
        claude_prompt("session-a", &gone, "Resumed prompt", "2026-10-03T01:00:00Z")
    )
    .unwrap();
    home.catalog.refresh();

    let after = home.search(json!({}), vec![live_pane("codex", "id", "session-a")]);
    assert_eq!(after["result"]["sessions"].as_array().unwrap().len(), 1);
    let row = by_key(&after, "session-a");
    assert_eq!(row["state"], "ended");
    assert_eq!(row["cwd"], gone);
    assert_eq!(row["place_state"], "gone");
    assert_eq!(row["nearest_existing"], ancestor);
    assert_eq!(
        row["resume"],
        json!({"command": "claude --resume session-a", "cwd": ancestor})
    );
    assert!(row.get("resume_blocked").is_none());

    let live = home.search(json!({}), vec![live_pane("claude", "id", "session-a")]);
    let row = by_key(&live, "session-a");
    assert_eq!(row["state"], "live");
    assert_eq!(row["live_pane_id"], "w1:p1");
    assert_eq!(row["resume"], Value::Null);
    assert_eq!(row["resume_blocked"], "live");
    assert_eq!(row["labels"], json!([]));
}

#[test]
fn literal_or_scoring_explains_hits_and_hard_filters_use_session_windows() {
    let home = FixtureHome::new();
    let cwd = home.dir("Project");
    let prompts = [
        "Start here",
        "KEYBOARD sim [focus] breadcrumbs",
        "third",
        "fourth",
        "fifth",
        "sixth",
    ];
    let lines: Vec<_> = prompts
        .iter()
        .enumerate()
        .map(|(index, text)| {
            claude_prompt(
                "all-terms",
                &cwd,
                text,
                &format!("2026-10-03T0{index}:00:00Z"),
            )
        })
        .collect();
    home.write(".claude/projects/project/all-terms.jsonl", &lines);
    home.write(
        ".claude/projects/project/some-terms.jsonl",
        &[claude_prompt(
            "some-terms",
            &cwd,
            "keyboard only",
            "2026-10-04T00:00:00Z",
        )],
    );
    home.write(
        ".claude/projects/project/unrelated.jsonl",
        &[claude_prompt(
            "unrelated",
            &cwd,
            "unrelated newest prompt",
            "2026-10-05T00:00:00Z",
        )],
    );
    home.catalog.refresh();

    let response = home.search(json!({"terms": ["keyboard", "sim"]}), vec![]);
    let rows = response["result"]["sessions"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["key"]["value"], "all-terms");
    assert_eq!(rows[1]["key"]["value"], "some-terms");
    assert!(rows[0]["score"].as_f64().unwrap() > rows[1]["score"].as_f64().unwrap());
    assert_eq!(
        rows[0]["why"],
        json!({"field": "prompt", "line": "KEYBOARD sim [focus] breadcrumbs", "terms": ["keyboard", "sim"]})
    );
    assert_eq!(
        rows[0]["recent_prompts"],
        json!(["fourth", "fifth", "sixth"])
    );
    assert_eq!(response["result"]["scanned"], 3);

    for (terms, field) in [
        (json!(["[focus]"]), "prompt"),
        (json!(["Start here"]), "first_prompt"),
        (json!(["feature/search"]), "branch"),
        (json!(["PROJECT"]), "cwd"),
    ] {
        let response = home.search(json!({"terms": terms}), vec![]);
        assert_eq!(by_key(&response, "all-terms")["why"]["field"], field);
    }
    let filtered = home.search(
        json!({"terms": ["keyboard", "sim"], "agent": "claude",
        "place": "pRoJeCt", "since_ms": 1790991000000_u64, "until_ms": 1790992800000_u64}),
        vec![],
    );
    assert_eq!(filtered["result"]["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(
        filtered["result"]["sessions"][0]["key"]["value"],
        "all-terms"
    );
    for params in [
        json!({"place": "missing-place"}),
        json!({"agent": "Claude"}),
        json!({"since_ms": 1791200000000_u64}),
        json!({"until_ms": 1790900000000_u64}),
    ] {
        assert_eq!(home.search(params, vec![])["result"]["sessions"], json!([]));
    }
    let limited = home.search(json!({"terms": ["keyboard", "sim"], "limit": 1}), vec![]);
    assert_eq!(limited["result"]["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(
        limited["result"]["sessions"][0]["key"]["value"],
        "all-terms"
    );
    let browse = home.search(
        json!({"terms": [], "include_headless": true, "embedding": [0.1]}),
        vec![],
    );
    assert_eq!(browse["result"]["sessions"][0]["key"]["value"], "unrelated");
    assert_eq!(browse["result"]["sessions"][0]["why"], Value::Null);
    assert_eq!(browse, home.search(json!({}), vec![]));
}

#[test]
fn search_skips_subagents_and_injected_material_and_judges_both_harness_titles() {
    let home = FixtureHome::new();
    let cwd = home.dir("project");
    let mut meta = claude_prompt("claude-a", &cwd, "excluded-meta", "2026-10-03T00:01:00Z");
    meta["isMeta"] = json!(true);
    let mut compact = claude_prompt("claude-a", &cwd, "excluded-summary", "2026-10-03T00:02:00Z");
    compact["isCompactSummary"] = json!(true);
    home.write(".claude/projects/project/claude-a.jsonl", &[
        claude_prompt("claude-a", &cwd, "A genuine question", "2026-10-03T00:00:00Z"), meta, compact,
        json!({"type": "ai-title", "aiTitle": "Claude Code"}),
        json!({"type": "assistant", "message": {"role": "assistant", "content": "excluded-assistant"}}),
        json!({"type": "user", "message": {"role": "user", "content": [
            {"type": "tool_result", "content": "excluded-tool"}, {"type": "text", "text": "excluded-companion"}
        ]}}),
        claude_prompt("claude-a", &cwd, "<command-message>clear</command-message><command-name>/clear</command-name>", "2026-10-03T00:03:00Z"),
    ]);
    home.write(
        ".claude/projects/project/claude-a/subagents/agent-child.jsonl",
        &[claude_prompt(
            "claude-child",
            &cwd,
            "excluded-child",
            "2026-10-03T00:00:00Z",
        )],
    );
    home.write(".codex/sessions/2026/10/03/rollout-codex-a.jsonl", &[
        json!({"type": "session_meta", "payload": {"id": "codex-a", "cwd": cwd, "source": "cli"}}),
        codex_prompt("# AGENTS.md instructions excluded-preamble", "2026-10-03T00:00:00Z"),
        codex_prompt("<environment_context>excluded-environment</environment_context>", "2026-10-03T00:00:00Z"),
        codex_prompt("Another genuine question", "2026-10-03T00:01:00Z"),
        json!({"type": "event_msg", "payload": {"type": "user_message", "message": "excluded-duplicate-event"}}),
        json!({"type": "response_item", "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "excluded-output"}]}}),
    ]);
    for (id, source) in [
        ("review-child", json!({"subagent": "review"})),
        (
            "spawn-child",
            json!({"subagent": {"thread_spawn": {"parent_thread_id": "codex-a"}}}),
        ),
    ] {
        home.write(&format!(".codex/sessions/2026/10/03/rollout-{id}.jsonl"), &[
            json!({"type": "session_meta", "payload": {"id": id, "cwd": cwd, "source": source, "parent_thread_id": "codex-a"}}),
            codex_prompt("excluded-subagent", "2026-10-03T00:00:00Z"),
        ]);
    }
    home.write(
        ".codex/session_index.jsonl",
        &[
            json!({"id": "codex-a", "thread_name": "Useful old title"}),
            json!({"id": "codex-a", "thread_name": "ANOTHER genuine question"}),
        ],
    );
    home.write(".codex/history.jsonl", &[
        json!({"session_id": "review-child", "ts": 1790985600, "text": "excluded-child-history"}),
        json!({"session_id": "codex-a", "ts": 1790985600, "text": "<user_instructions>excluded-history</user_instructions>"}),
    ]);
    home.write(".claude/history.jsonl", &[
        json!({"sessionId": "claude-a", "project": cwd, "timestamp": 1790985600000_u64, "display": "<system-reminder>excluded-history</system-reminder>"}),
    ]);
    home.catalog.refresh();

    let browse = home.search(json!({"include_headless": true}), vec![]);
    assert_eq!(browse["result"]["sessions"].as_array().unwrap().len(), 2);
    for id in ["claude-a", "codex-a"] {
        let row = by_key(&browse, id);
        assert!(row.get("title").is_none());
        assert_eq!(row["headless"], false);
        assert_eq!(row["subagent_count"], 0);
        assert!(row.get("parent").is_none());
        assert!(row.get("outside").is_none());
    }
    assert_eq!(
        home.search(json!({"terms": ["genuine"]}), vec![])["result"]["sessions"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        home.search(json!({"terms": ["excluded", "command-name"]}), vec![])["result"]["sessions"],
        json!([])
    );

    home.write(
        ".codex/session_index.jsonl",
        &[json!({"id": "codex-a", "thread_name": "Project synthesis"})],
    );
    home.catalog.refresh();
    let renamed = home.search(json!({"terms": ["synthesis"]}), vec![]);
    assert_eq!(
        by_key(&renamed, "codex-a")["why"],
        json!({"field": "title", "line": "Project synthesis", "terms": ["synthesis"]})
    );
}

#[test]
fn older_zstd_rollouts_are_searchable_and_resumable_by_id() {
    let home = FixtureHome::new();
    let path = home.write(
        ".codex/sessions/2025/01/01/rollout-compressed-a.jsonl.zst",
        &[],
    );
    // Two concatenated frames; codex.jsonl holds the uncompressed source.
    std::fs::write(path, include_bytes!("fixtures/codex.jsonl.zst")).unwrap();
    home.catalog.refresh();
    let response = home.search(json!({"terms": ["archived"]}), vec![]);
    let row = by_key(&response, "compressed-a");
    assert_eq!(response["result"]["partial"], false);
    assert_eq!(row["first_prompt"], "Find the archived keyboard work");
    assert_eq!(row["branch"], "archive");
    assert_eq!(row["started_at_ms"], 1735689600000_u64);
    assert_eq!(row["last_active_at_ms"], 1735689720000_u64);
    assert_eq!(
        row["recent_prompts"],
        json!([
            "Find the archived keyboard work",
            "Read the second frame too"
        ])
    );
    assert_eq!(row["resume"]["command"], "codex resume compressed-a");
}

#[test]
fn the_request_deadline_includes_a_stalled_app() {
    let home = FixtureHome::new();
    home.catalog.refresh();
    let (release, held) = std::sync::mpsc::channel::<()>();
    let began = std::time::Instant::now();
    let response = respond_with_budget(
        "search".into(),
        &ServerSearchSessionsParams::default(),
        Sources {
            live: Box::new(move || {
                let _ = held.recv_timeout(Duration::from_secs(2));
                json!({"id": "panes", "result": {"type": "pane_list", "panes": []}}).to_string()
            }),
            catalog: Arc::clone(&home.catalog),
            now: Arc::new(Instant::now),
        },
        Duration::from_millis(100),
    );
    drop(release);
    assert!(
        began.elapsed() < Duration::from_secs(1),
        "request waited out the app"
    );
    let response: Value = serde_json::from_str(&response).unwrap();
    assert_eq!(response["result"]["partial"], true);
    assert_eq!(response["result"]["sessions"], json!([]));
}

#[test]
fn a_scan_deadline_preserves_the_rows_already_found() {
    let home = FixtureHome::new();
    let cwd = home.dir("project");
    for (id, at) in [
        ("older", "2026-10-03T00:00:00Z"),
        ("newer", "2026-10-04T00:00:00Z"),
    ] {
        home.write(
            &format!(".claude/projects/project/{id}.jsonl"),
            &[claude_prompt(id, &cwd, "Find me", at)],
        );
    }
    home.catalog.refresh();
    assert_eq!(
        home.search(json!({}), vec![])["result"]["sessions"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let ticks = std::sync::atomic::AtomicUsize::new(0);
    let start = Instant::now();
    let response = respond_with_budget(
        "search".into(),
        &ServerSearchSessionsParams::default(),
        Sources {
            live: Box::new(|| {
                json!({"id": "panes", "result": {"type": "pane_list", "panes": []}}).to_string()
            }),
            catalog: Arc::clone(&home.catalog),
            now: Arc::new(move || {
                // The first call starts the budget, the next admits one row,
                // and the following row encounters an expired scan deadline.
                if ticks.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 2 {
                    start
                } else {
                    start + Duration::from_secs(10)
                }
            }),
        },
        Duration::from_secs(2),
    );
    let response: Value = serde_json::from_str(&response).unwrap();
    assert_eq!(response["result"]["partial"], true);
    assert_eq!(response["result"]["scanned"], 1);
    assert_eq!(response["result"]["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(response["result"]["sessions"][0]["key"]["value"], "newer");
}

#[test]
fn timed_out_searches_keep_their_permits_until_the_work_finishes() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let home = FixtureHome::new();
    home.catalog.refresh();
    let started = Arc::new(AtomicUsize::new(0));
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let release_rx = Arc::new(Mutex::new(release_rx));
    let mut callers = Vec::new();
    for _ in 0..2 {
        let started = Arc::clone(&started);
        let entered = entered_tx.clone();
        let held = Arc::clone(&release_rx);
        let catalog = Arc::clone(&home.catalog);
        callers.push(std::thread::spawn(move || {
            respond_with_budget(
                "search".into(),
                &ServerSearchSessionsParams::default(),
                Sources {
                    live: Box::new(move || {
                        started.fetch_add(1, Ordering::SeqCst);
                        entered.send(()).unwrap();
                        let _ = held.lock().unwrap().recv_timeout(Duration::from_secs(5));
                        json!({"id": "panes", "result": {"type": "pane_list", "panes": []}})
                            .to_string()
                    }),
                    catalog,
                    now: Arc::new(Instant::now),
                },
                Duration::from_millis(200),
            )
        }));
    }
    for _ in 0..2 {
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    }
    for caller in callers {
        let response: Value = serde_json::from_str(&caller.join().unwrap()).unwrap();
        assert_eq!(response["result"]["partial"], true);
    }
    assert_eq!(permits().available_permits(), 0);
    let counted = Arc::clone(&started);
    let third = respond_with_budget(
        "search".into(),
        &ServerSearchSessionsParams::default(),
        Sources {
            live: Box::new(move || {
                counted.fetch_add(1, Ordering::SeqCst);
                String::new()
            }),
            catalog: Arc::clone(&home.catalog),
            now: Arc::new(Instant::now),
        },
        Duration::from_millis(100),
    );
    assert_eq!(
        serde_json::from_str::<Value>(&third).unwrap()["result"]["partial"],
        true
    );
    assert_eq!(started.load(Ordering::SeqCst), 2);
    for _ in 0..2 {
        release_tx.send(()).unwrap();
    }
    let until = Instant::now() + Duration::from_secs(2);
    while permits().available_permits() != 2 {
        assert!(
            Instant::now() < until,
            "finished searches did not release permits"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(home.search(json!({}), vec![])["result"]["partial"], false);
    assert_eq!(
        started.load(Ordering::SeqCst),
        2,
        "an expired queued search ran later"
    );
}

#[test]
fn cached_records_refresh_by_mtime_and_size_and_deleted_records_use_history() {
    let home = FixtureHome::new();
    let cwd = home.dir("project");
    let path = home.write(
        ".claude/projects/project/cache-a.jsonl",
        &[claude_prompt(
            "cache-a",
            &cwd,
            "alpha",
            "2026-10-03T00:00:00Z",
        )],
    );
    home.write(".claude/history.jsonl", &[
        json!({"sessionId": "cache-a", "project": cwd, "display": "history survives", "timestamp": 1790900000000_u64}),
    ]);
    home.catalog.refresh();
    assert_eq!(
        by_key(&home.search(json!({"terms": ["alpha"]}), vec![]), "cache-a")["state"],
        "ended"
    );
    let stamp = std::fs::metadata(&path).unwrap().modified().unwrap();
    let original = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, original.replace("alpha", "bravo")).unwrap();
    let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.set_modified(stamp).unwrap();
    home.catalog.refresh();
    // With the same path, mtime and size, maintenance and queries reuse the
    // parsed record. A reader that opens every file again would return bravo.
    assert_eq!(
        home.search(json!({"terms": ["bravo"]}), vec![])["result"]["sessions"],
        json!([])
    );
    file.set_modified(stamp + Duration::from_secs(1)).unwrap();
    home.catalog.refresh();
    assert_eq!(
        by_key(&home.search(json!({"terms": ["bravo"]}), vec![]), "cache-a")["state"],
        "ended"
    );
    assert_eq!(
        home.search(json!({"terms": ["alpha"]}), vec![])["result"]["sessions"],
        json!([])
    );
    use std::io::Write;
    writeln!(
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap(),
        "{}",
        claude_prompt("cache-a", &cwd, "grown prompt", "2026-10-04T00:00:00Z")
    )
    .unwrap();
    file.set_modified(stamp + Duration::from_secs(1)).unwrap();
    assert_eq!(
        home.search(json!({"terms": ["grown"]}), vec![])["result"]["sessions"],
        json!([])
    );
    home.catalog.refresh();
    assert_eq!(
        by_key(&home.search(json!({"terms": ["grown"]}), vec![]), "cache-a")["last_active_at_ms"],
        1791072000000_u64
    );
    drop(file);
    std::fs::remove_file(&path).unwrap();
    home.catalog.refresh();
    let gone = home.search(json!({}), vec![]);
    let row = by_key(&gone, "cache-a");
    assert_eq!(row["state"], "record_gone");
    assert_eq!(row["resume_blocked"], "record_gone");
    assert_eq!(row["first_prompt"], "history survives");
    assert_eq!(row["recent_prompts"], json!(["history survives"]));
    let live = home.search(json!({}), vec![live_pane("claude", "id", "cache-a")]);
    assert_eq!(by_key(&live, "cache-a")["state"], "live");
}

#[test]
fn resume_uses_existing_ancestors_for_codex_and_pi_and_blocks_unknown_places() {
    let home = FixtureHome::new();
    let ancestor = home.dir("projects");
    let gone = format!("{ancestor}/removed/worktree");
    home.write(".codex/sessions/2026/10/03/rollout-codex-a.jsonl", &[
        json!({"type": "session_meta", "payload": {"id": "codex-a", "cwd": gone, "source": "cli"}}),
    ]);
    home.write(
        ".codex/sessions/2026/10/03/rollout-unknown.jsonl",
        &[json!({"type": "session_meta", "payload": {"id": "unknown", "source": "cli"}})],
    );
    let pi = home.write(".pi/agent/sessions/project/with space.jsonl", &[
        json!({"type": "session", "id": "pi-a", "cwd": gone, "timestamp": "2026-10-03T00:00:00Z"})
    ]);
    home.catalog.refresh();
    let response = home.search(json!({}), vec![]);
    for key in ["codex-a", pi.to_str().unwrap()] {
        let row = by_key(&response, key);
        assert_eq!(row["state"], "ended");
        assert_eq!(row["place_state"], "gone");
        assert_eq!(row["nearest_existing"], ancestor);
        assert_eq!(row["resume"]["cwd"], ancestor);
        assert!(row.get("resume_blocked").is_none());
    }
    let unknown = by_key(&response, "unknown");
    assert_eq!(unknown["cwd"], Value::Null);
    assert_eq!(unknown["resume"], Value::Null);
    assert_eq!(unknown["resume_blocked"], "folder_gone");
    let live = home.search(
        json!({}),
        vec![live_pane("pi", "path", pi.to_str().unwrap())],
    );
    assert_eq!(by_key(&live, pi.to_str().unwrap())["live_pane_id"], "w1:p1");
}

#[test]
fn browse_defaults_to_fifty_and_caps_the_requested_limit_at_two_hundred() {
    let home = FixtureHome::new();
    let cwd = home.dir("project");
    let history: Vec<_> = (0..201)
        .map(|index| {
            json!({
                "sessionId": format!("session-{index:03}"), "project": cwd,
                "display": format!("Prompt {index}"), "timestamp": 1790985600000_u64 + index * 1000
            })
        })
        .collect();
    home.write(".claude/history.jsonl", &history);
    home.catalog.refresh();
    for (params, count) in [
        (json!({}), 50),
        (json!({"limit": 999}), 200),
        (json!({"limit": 0}), 0),
    ] {
        let response = home.search(params, vec![]);
        assert_eq!(response["result"]["scanned"], 201);
        assert_eq!(
            response["result"]["sessions"].as_array().unwrap().len(),
            count
        );
        if count > 0 {
            assert_eq!(
                response["result"]["sessions"][0]["key"]["value"],
                "session-200"
            );
        }
    }
}

#[test]
fn a_cold_catalog_is_partial_and_an_indexed_empty_catalog_is_complete() {
    let home = FixtureHome::new();
    let response = respond_with_budget(
        "search".into(),
        &ServerSearchSessionsParams::default(),
        Sources {
            live: Box::new(|| {
                json!({"id": "panes", "result": {"type": "pane_list", "panes": []}}).to_string()
            }),
            catalog: Arc::clone(&home.catalog),
            now: Arc::new(Instant::now),
        },
        Duration::from_millis(100),
    );
    assert_eq!(
        serde_json::from_str::<Value>(&response).unwrap()["result"]["partial"],
        true
    );
    home.catalog.refresh();
    assert_eq!(home.search(json!({}), vec![])["result"]["partial"], false);
}

#[test]
fn an_unavailable_app_cannot_turn_a_live_session_into_a_resume_offer() {
    let home = FixtureHome::new();
    let cwd = home.dir("project");
    home.write(
        ".claude/projects/project/session-a.jsonl",
        &[claude_prompt(
            "session-a",
            &cwd,
            "Find me",
            "2026-10-03T00:00:00Z",
        )],
    );
    home.catalog.refresh();
    let response = respond(
        "search".into(),
        &ServerSearchSessionsParams::default(),
        Sources {
            live: Box::new(|| {
                json!({"id": "panes", "error": {"code": "server_unavailable", "message": "app unavailable"}}).to_string()
            }),
            catalog: Arc::clone(&home.catalog),
            now: Arc::new(Instant::now),
        },
    );
    let response: Value = serde_json::from_str(&response).unwrap();
    assert_eq!(response["error"]["code"], "server_unavailable");
    assert!(response.get("result").is_none());
}

#[test]
fn long_prompts_are_searchable_beyond_previews_and_why_keeps_the_matching_text() {
    let home = FixtureHome::new();
    let cwd = home.dir("project");
    let text = format!("{}終点 keyboard", "İ fox 🦊 ".repeat(200));
    home.write(
        ".claude/projects/project/session-a.jsonl",
        &[claude_prompt(
            "session-a",
            &cwd,
            &text,
            "2026-10-03T00:00:00Z",
        )],
    );
    home.catalog.refresh();
    let response = home.search(json!({"terms": ["終点"]}), vec![]);
    let row = by_key(&response, "session-a");
    assert!(row["first_prompt"].as_str().unwrap().chars().count() <= 401);
    assert!(row["recent_prompts"][0].as_str().unwrap().chars().count() <= 241);
    assert!(!row["first_prompt"].as_str().unwrap().contains("終点"));
    assert!(row["why"]["line"].as_str().unwrap().contains("終点"));
    assert_eq!(row["why"]["field"], "prompt");
    assert_eq!(row["why"]["terms"], json!(["終点"]));
    assert_eq!(
        home.search(
            json!({"terms": ["pane label"]}),
            vec![live_pane("claude", "id", "session-a")]
        )["result"]["sessions"],
        json!([])
    );
}

#[test]
fn appended_codex_metadata_keeps_the_original_prompt_and_session_window() {
    let home = FixtureHome::new();
    let cwd = home.dir("project");
    let path = home.write(
        ".codex/sessions/2026/10/03/rollout-codex-a.jsonl",
        &[
            json!({"type": "session_meta", "timestamp": "2026-10-03T00:00:00Z",
            "payload": {"id": "codex-a", "cwd": cwd, "source": "cli"}}),
            codex_prompt("Original question", "2026-10-03T00:01:00Z"),
        ],
    );
    home.catalog.refresh();
    let before = home.search(json!({}), vec![]);
    assert_eq!(
        by_key(&before, "codex-a")["first_prompt"],
        "Original question"
    );
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(
        file,
        "{}",
        json!({"type": "session_meta", "timestamp": "2026-10-04T00:00:00Z",
        "payload": {"id": "codex-a", "cwd": cwd, "source": "cli"}})
    )
    .unwrap();
    writeln!(
        file,
        "{}",
        codex_prompt("Continued question", "2026-10-04T00:01:00Z")
    )
    .unwrap();
    drop(file);
    home.catalog.refresh();
    let after = home.search(json!({}), vec![]);
    let row = by_key(&after, "codex-a");
    assert_eq!(row["first_prompt"], "Original question");
    assert_eq!(
        row["recent_prompts"],
        json!(["Original question", "Continued question"])
    );
    assert_eq!(row["started_at_ms"], 1790985600000_u64);
    assert_eq!(row["last_active_at_ms"], 1791072060000_u64);
}
