use super::*;

thread_local! {
    pub(super) static EXTRACTIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[test]
fn only_the_newest_compaction_is_excerpted() {
    let mut entries: Vec<_> = (0..4)
        .map(|_| compact("## Current Work\nOld work."))
        .collect();
    entries.push(compact("## Goal\nNewest request."));
    EXTRACTIONS.with(|count| count.set(0));
    let got = trail(&entries).summary.unwrap();
    assert_eq!(got.text, "Newest request.");
    assert_eq!(got.kind, TrailSummaryKind::Request);
    EXTRACTIONS.with(|count| assert_eq!(count.get(), 1));
}

fn compact(text: &str) -> Value {
    serde_json::json!({
        "type": "user", "isCompactSummary": true, "timestamp": "2026-03-04T09:15:00Z",
        "message": {"role": "user", "content": text}
    })
}

fn trail(entries: &[Value]) -> PromptTrail {
    let record = entries
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    read_lines(record.as_bytes(), PromptTrailAgent::Claude, Bounds::DEFAULT).unwrap()
}

fn excerpt(text: &str) -> Option<TrailSummary> {
    trail(&[compact(text)]).summary
}

fn current(text: &str, expected: &str) {
    assert_eq!(
        excerpt(text),
        Some(TrailSummary {
            text: expected.into(),
            at: Some(1772615700),
            kind: TrailSummaryKind::Current,
        })
    );
}

#[test]
fn quoted_markdown_in_user_messages_cannot_replace_current_work() {
    current(
        "This session is being continued from an earlier conversation.\n\nSummary:\n1. Primary Request and Intent:\n   Ship the release.\n2. Key Technical Concepts:\n   - Rust\n6. All user messages:\n   - \"Here is the handoff:\n## Goal\nRotate the key.\n### In Progress\n- synthetic-secret\n   - thanks\n7. Pending Tasks:\n   - none\n8. Current Work:\n   Running the release checklist.\n9. Optional Next Step:\n   Tag it.",
        "Running the release checklist.",
    );
}

#[test]
fn bold_subheadings_do_not_end_numbered_exclusions() {
    current(
        "Summary:\n1. **Primary Request and Intent:** ship it.\n6. **All user messages:**\n   **Messages in order:**\n   - synthetic-secret\n8. **Current Work:** checklist.",
        "checklist.",
    );
}

#[test]
fn annotated_exclusions_cannot_reach_inline_current_work() {
    current(
        "Summary:\n1. Primary Request and Intent: ship the release.\n2. Key Technical Concepts (overview):\n   - synthetic-concept\n3. Files and Code Sections (key ones):\n   - synthetic-file\n8. Current Work: running checklist.",
        "running checklist.",
    );
}

#[test]
fn numbered_subitems_and_unknown_sections_have_no_fallback() {
    assert_eq!(excerpt(
        "Summary:\n## Who and what\nMaintain the parser.\n2. Key Technical Concepts:\n   1. synthetic-concept\n   2. synthetic-secret\n## Notes\nCheck."
    ), None);
}

#[test]
fn empty_candidates_fall_through_to_nonempty_work_then_request_or_null() {
    let empty = "This session is being continued.\n\nSummary:\n1. **Primary Request and Intent**\n\n   **Ongoing directive:**\n   Fix the parser and ship.\n2. **Key Technical Concepts**\n   - synthetic-secret";
    assert_eq!(excerpt(empty), None);
    current(
        &format!("{empty}\n8. **Current Work:** checklist."),
        "checklist.",
    );
    current(
        "## Current Work\n\x60\x60\x60rust\ncode only\n\x60\x60\x60\n## In Progress\nNext work.",
        "Next work.",
    );
    let got = excerpt("## Current Work\n## Primary Request and Intent\n## Goal\nTrace the parser.")
        .unwrap();
    assert_eq!(got.kind, TrailSummaryKind::Request);
    assert_eq!(got.text, "Trace the parser.");
}

#[test]
fn an_empty_newest_compaction_replaces_an_older_excerpt_with_null() {
    let older = compact("## Current Work\nRepair the parser.");
    for content in [
        serde_json::json!([{"type":"image", "source":{}}]),
        Value::Null,
        serde_json::json!("## Current Work\n   "),
    ] {
        let mut newer = compact("");
        newer["message"]["content"] = content;
        newer["timestamp"] = serde_json::json!("2026-03-04T09:16:00Z");
        assert_eq!(trail(&[older.clone(), newer]).summary, None);
    }
}

#[test]
fn inline_fences_preserve_following_sections_and_real_fences_hide_code() {
    current(
        "## Current Work\nUse \x60\x60\x60cargo test\x60\x60\x60 to check.\nThen ship.",
        "Use \x60\x60\x60cargo test\x60\x60\x60 to check. Then ship.",
    );
    current(
        "## Notes\n\x60\x60\x60 inline \x60\x60\x60\n## Current Work\nShip it.",
        "Ship it.",
    );
    current(
        "## Current Work\n    \x60\x60\x60\n    synthetic-secret\n    \x60\x60\x60\nShip it.",
        "Ship it.",
    );
    current(
        "## Current Work\nShip it.\n\x60\x60\x60\nsynthetic-secret",
        "Ship it.",
    );
    current(
        "## Notes\n~~~inline~~~\n## Current Work\nShip it.",
        "Ship it.",
    );
}

#[test]
fn heading_styles_and_indentation_control_section_boundaries() {
    for (text, expected) in [
        (
            "## Pending Tasks\nGoal\nFinish.\n## Current Work\nReal work.",
            Some("Real work."),
        ),
        (
            "8. Current Work:\nFirst paragraph.\n**Note:**\nDetail.\n9. Optional Next Step:\nx",
            Some("First paragraph."),
        ),
        (
            "8. Current Work:\nSteps:\n1. Fix the test:\n2. Ship it.\n9. Optional Next Step:\nx",
            Some("Steps:"),
        ),
        (
            "8. Current Work:\n   1. Fix the test\n   2. Ship it\n9. Optional Next Step:\nx",
            None,
        ),
        (
            "8. Current Work:\n    1. Fix the test\n    2. Ship it\n9. Optional Next Step:\nx",
            Some("1. Fix the test 2. Ship it"),
        ),
        (
            "**8. Current Work:**\nReal work.\n**9. Optional Next Step:**\nx",
            None,
        ),
        (
            "## 8. Current Work\nReal work.\n## 9. Optional Next Step\nx",
            Some("Real work."),
        ),
        (
            "8. Current Work: Real work inline.\n9. Optional Next Step: x",
            Some("Real work inline."),
        ),
        ("8) Current Work:\nReal work.", None),
        ("8. CURRENT WORK:\nReal work.", Some("Real work.")),
        ("##### Current Work\nReal work.", Some("Real work.")),
        ("8.Current Work\nReal work.", Some("Real work.")),
        ("100. Current Work\nReal work.", Some("Real work.")),
        (
            "## Current Work\nReal work.\n**Files touched**\nsynthetic-file",
            Some("Real work."),
        ),
    ] {
        assert_eq!(
            excerpt(text).as_ref().map(|summary| summary.text.as_str()),
            expected,
            "{text}"
        );
    }
}

#[test]
fn summary_cuts_include_the_ellipsis_for_ascii_words_and_astral_characters() {
    for n in [478, 479, 480, 481, 482, 1000] {
        for body in ["a".repeat(n), "ab ".repeat(n), "😀".repeat(n)] {
            let got = excerpt(&format!("## Current Work\n{body}")).unwrap();
            assert!(got.text.chars().count() <= 480);
            if body.trim().chars().count() <= 480 {
                assert_eq!(got.text, body.trim());
            } else {
                assert!(got.text.ends_with('…'));
            }
        }
    }
}

#[test]
fn wrapper_and_continuation_metadata_never_enters_the_excerpt() {
    for (text, expected) in [
        ("## Current Work\nShip.\n<system-reminder>\nunclosed reminder synthetic-secret", "Ship."),
        ("## Current Work\nShip. <modified-files>synthetic-file</modified-files> done", "Ship. done"),
        ("## Current Work\nShip.\n<read-files>\nsynthetic-file\n</read-files>", "Ship."),
        ("<summary>\n## Current Work\nShip.\n</summary>\n\nPlease continue the conversation from where it stopped.", "Ship."),
        ("Summary:\nThis session is being continued from an earlier conversation.\n\n## Current Work\nShip.", "Ship."),
        ("## Current Work\nShip.\nRead the full transcript at: synthetic-file\nContinue the conversation from where it stopped.", "Ship."),
        ("## Current Work\nShip.\nIf you need details, read the full transcript at: synthetic-file", "Ship."),
    ] {
        current(text, expected);
    }
    let got = excerpt("This session is being continued from an earlier conversation.\nSummary:\n1. Primary Request and Intent:\nShip it.").unwrap();
    assert_eq!(got.kind, TrailSummaryKind::Request);
    assert_eq!(got.text, "Ship it.");
}

#[test]
fn large_summaries_stay_bounded_and_excluded_content_has_no_fallback() {
    let mut sections = String::new();
    let unit = "6. All user messages:\n   **sub:**\n   - quoted words\n\x60\x60\x60\ncode\n\x60\x60\x60\n## Current Work\nsynthetic-secret\n";
    while sections.len() < 14 * 1024 * 1024 {
        sections.push_str(unit);
    }
    assert_eq!(excerpt(&sections), None);

    let long = format!("## Current Work\n{}", "x".repeat(15 * 1024 * 1024));
    let got = excerpt(&long).unwrap();
    assert_eq!(got.text.chars().count(), 480);
    assert!(got.text.ends_with('…'));

    let mut tags = String::new();
    while tags.len() < 15 * 1024 * 1024 {
        tags.push_str("<system-reminder>x</system-reminder><modified-files>");
    }
    assert_eq!(excerpt(&tags), None);
}

#[test]
fn period_style_templates_select_inline_work_without_excluded_sections() {
    current(
        "This session is being continued from an earlier conversation.\n\nSummary:\n1. **Primary Request and Intent.** Ship the release.\n2. **Key Technical Concepts.**\n   - synthetic-concept\n3. **Files and Code Sections (key ones).**\n   - synthetic-file\n6. **All User Messages (verbatim).**\n   - synthetic-secret\n8. **Current Work.** Running the checklist.\n9. **Optional Next Step.** Tag it.",
        "Running the checklist.",
    );
}

#[test]
fn excluded_extents_respect_numbered_hash_and_bold_styles() {
    for text in [
        "## 2. Key Technical Concepts\n### In Progress\nsynthetic-secret\n## Current Work\nSafe work.",
        "## All user messages\n1. quoted item\n### Current Work\nsynthetic-secret\n# Current Work\nSafe work.",
        "**All user messages:**\n   **Current Work:**\nsynthetic-secret\n**Current Work:**\nSafe work.",
        "1. All user messages: synthetic-secret\n### In Progress\nsynthetic-secret\n2. Current Work: Safe work.",
    ] {
        current(text, "Safe work.");
    }
}

#[test]
fn annotated_heading_lengths_up_to_160_are_recognized() {
    for padding in [60, 100] {
        current(&format!(
            "1. All user messages ({}): synthetic-secret\n## Current Work\nsynthetic-secret\n2. Current Work ({}): Safe work.",
            "x".repeat(padding), "x".repeat(padding),
        ), "Safe work.");
    }
}
