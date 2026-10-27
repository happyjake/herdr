//! Claude's record, read into turns.
//!
//! What starts a turn: a prompt the prompt trail would count — so never a
//! line the harness wrote in the person's name, a command with no words, a
//! command's own output, shell input or output, an interruption note or a
//! compaction summary — or a prompt the person typed while the agent was
//! working, delivered into the run; a background task's notification, which
//! starts an answer-only `task` turn; and a scheduled task firing, which
//! starts an answer-only `wake` turn.
//!
//! What ends one: the message that stopped with `end_turn`, whose text
//! blocks — written one line each under one message id — are the final
//! answer; an interruption note; or an API error. Anything the agent does
//! after an answer reopens the turn, so the answer is always the last one.
//!
//! Sub-agent traffic lives in files of its own and is never read here;
//! a sidechain line that did reach this file is stepped over. Compaction
//! boundaries say nothing about a turn and are stepped over too.

use serde_json::Value;

use super::{RecordTurns, Turn};
use crate::api::schema::{PromptTrailAgent, TurnEnd, TurnStart};

/// The note a user line carries when the person stopped a run.
const INTERRUPTED: &str = "[Request interrupted";

/// What a background task's finishing reads as.
const TASK_NOTIFICATION: &str = "<task-notification>";

/// What the harness draws at column 0 ahead of a prompt's first row.
const PROMPT_MARKER: &str = "\u{276f}";

/// The origin the harness gives what the person typed.
const HUMAN_ORIGIN: &str = "human";

#[derive(Default)]
pub(crate) struct ClaudeReader {
    session_id: Option<String>,
    turns: Vec<Turn>,
    /// The id of the assistant message being written, and its text so far.
    message_id: Option<String>,
    message_text: Vec<String>,
}

impl super::Reader for ClaudeReader {
    fn accept(&mut self, line: &Value) {
        if flag(line, "isSidechain") {
            return;
        }
        if self.session_id.is_none() {
            if let Some(id) = line.get("sessionId").and_then(Value::as_str) {
                self.session_id = Some(id.to_string());
            }
        }
        match line.get("type").and_then(Value::as_str) {
            Some("user") => self.user(line),
            Some("assistant") => self.assistant(line),
            Some("attachment") => self.attachment(line),
            _ => {}
        }
    }

    fn finish(self: Box<Self>) -> RecordTurns {
        RecordTurns {
            session_id: self.session_id,
            turns: self.turns,
        }
    }

    fn prompt_markers(&self) -> &'static [&'static str] {
        &[PROMPT_MARKER]
    }

    fn editor_box(&self) -> bool {
        true
    }
}

impl ClaudeReader {
    fn user(&mut self, line: &Value) {
        let content = line
            .get("message")
            .and_then(|message| message.get("content"));
        let raw = content.map(raw_text).unwrap_or_default();
        let raw = raw.trim_start();

        if flag(line, "isMeta") {
            // A scheduled task firing speaks in the harness's voice, but it
            // is what started the run that follows.
            if line.get("scheduledTaskId").is_some() {
                self.start(line, TurnStart::Wake, None);
            }
            return;
        }
        if raw.starts_with(TASK_NOTIFICATION) {
            self.start(line, TurnStart::Task, None);
            return;
        }
        if raw.starts_with(INTERRUPTED) && !flag(line, "isCompactSummary") {
            if let Some(turn) = self.turns.last_mut() {
                turn.end_without_answer(TurnEnd::Aborted);
            }
            return;
        }
        // A line that says where it came from is a prompt only when that is
        // the person; one that says nothing predates the harness saying it.
        if origin(line).is_some_and(|kind| kind != HUMAN_ORIGIN) {
            return;
        }
        if crate::prompt_trail::prompt_text(PromptTrailAgent::Claude, line).is_some() {
            self.start(line, TurnStart::Prompt, Some(prompt_body(raw)));
        }
    }

    /// A prompt typed while the agent was working reaches the record as an
    /// attachment to the run rather than a line of its own.
    ///
    /// The harness queues its own words the same way — a continuation it
    /// injects toward a goal, another session's message — so only a queued
    /// prompt that names the person as its origin is one.
    fn attachment(&mut self, line: &Value) {
        let Some(attachment) = line.get("attachment") else {
            return;
        };
        if attachment.get("type").and_then(Value::as_str) != Some("queued_command")
            || attachment.get("commandMode").and_then(Value::as_str) != Some("prompt")
            || flag(attachment, "isMeta")
            || origin(attachment) != Some(HUMAN_ORIGIN)
        {
            return;
        }
        let Some(prompt) = attachment.get("prompt") else {
            return;
        };
        // Judged by the same rules as a prompt on a line of its own.
        let as_line = serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": prompt },
        });
        if crate::prompt_trail::prompt_text(PromptTrailAgent::Claude, &as_line).is_none() {
            return;
        }
        let raw = raw_text(prompt);
        self.start(line, TurnStart::Prompt, Some(prompt_body(&raw)));
    }

    fn assistant(&mut self, line: &Value) {
        let Some(turn) = self.turns.last_mut() else {
            return;
        };
        if flag(line, "isApiErrorMessage") {
            turn.end_without_answer(TurnEnd::Error);
            self.message_id = None;
            self.message_text.clear();
            return;
        }
        let Some(message) = line.get("message") else {
            return;
        };
        let id = message.get("id").and_then(Value::as_str);
        if id.is_none() || id != self.message_id.as_deref() {
            self.message_id = id.map(str::to_string);
            self.message_text.clear();
        }
        if let Some(Value::Array(blocks)) = message.get("content") {
            self.message_text.extend(
                blocks
                    .iter()
                    .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|block| block.get("text").and_then(Value::as_str))
                    .map(str::to_string),
            );
        }
        match message.get("stop_reason").and_then(Value::as_str) {
            Some("end_turn" | "stop_sequence") => turn.answer(&self.message_text.join("\n\n")),
            Some("max_tokens" | "refusal") => turn.end_without_answer(TurnEnd::Error),
            _ => turn.end_without_answer(TurnEnd::Open),
        }
    }

    fn start(&mut self, line: &Value, start: TurnStart, prompt: Option<String>) {
        let id = line
            .get("uuid")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("line-{}", self.turns.len()));
        let at = line
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(crate::prompt_trail::unix_seconds_from_rfc3339)
            .unwrap_or(0);
        self.message_id = None;
        self.message_text.clear();
        self.turns.push(Turn::started(id, at, start, prompt));
    }
}

/// The origin the harness gives a line it queued or recorded.
fn origin(value: &Value) -> Option<&str> {
    value
        .get("origin")
        .and_then(|origin| origin.get("kind"))
        .and_then(Value::as_str)
}

fn flag(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// The text of a user turn, with its lines kept.
fn raw_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// A prompt the trail counted, as the person wrote it: paste wrapper tags
/// removed, a slash command read back as it was typed, everything else whole.
fn prompt_body(raw: &str) -> String {
    let stripped = crate::prompt_trail::strip_pasted_content_tags(raw);
    let raw = stripped.trim_start();
    if raw.starts_with("<command-message>") || raw.starts_with("<command-name>") {
        let name = crate::prompt_trail::tag_contents(raw, "command-name")
            .unwrap_or_default()
            .trim()
            .trim_start_matches('/')
            .trim();
        let args = crate::prompt_trail::tag_contents(raw, "command-args")
            .unwrap_or_default()
            .trim();
        return format!("/{name} {args}");
    }
    raw.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preface::{read_turns, ANSWER_BYTES, PROMPT_BYTES};

    /// A trimmed Claude record in the harness's real line shapes — every
    /// key set taken from a real one, every text neutral and invented.
    const RECORD: &str = include_str!("fixtures/claude-record.jsonl");

    fn turns_of(record: &str) -> Vec<Turn> {
        read_turns(record.as_bytes(), PromptTrailAgent::Claude)
            .expect("a readable record")
            .turns
    }

    fn summary(turns: &[Turn]) -> Vec<(TurnStart, Option<&str>, TurnEnd)> {
        turns
            .iter()
            .map(|turn| (turn.start, turn.prompt.as_deref(), turn.end))
            .collect()
    }

    #[test]
    fn a_claude_record_reads_into_its_prompted_task_and_wake_turns() {
        let read = read_turns(RECORD.as_bytes(), PromptTrailAgent::Claude).unwrap();
        assert_eq!(
            read.session_id.as_deref(),
            Some("0f6e0c2a-5b1d-4c8e-9a7f-3e2d1c0b9a88")
        );
        // Every line the harness wrote in the person's name — the caveat,
        // the bare command and its output, shell input and output, a
        // reminder, a skill expansion, the image note, the compaction
        // summary — is no turn, and neither is the compaction boundary.
        assert_eq!(
            summary(&read.turns),
            vec![
                (
                    TurnStart::Prompt,
                    Some("trace why the ferry timetable importer drops the last sailing of the day\nkeep the fix small"),
                    TurnEnd::Answered
                ),
                (
                    TurnStart::Prompt,
                    Some("/review the harbour schedule parser\nand its tests"),
                    TurnEnd::Aborted
                ),
                (
                    TurnStart::Prompt,
                    Some("summarise the tide tables for the north pier"),
                    TurnEnd::Error
                ),
                (TurnStart::Task, None, TurnEnd::Answered),
                (
                    TurnStart::Prompt,
                    Some("now chunk the timetable by pier"),
                    TurnEnd::Open
                ),
                (
                    TurnStart::Prompt,
                    Some("also keep the old pier names"),
                    TurnEnd::Answered
                ),
                (TurnStart::Wake, None, TurnEnd::Answered),
                (
                    TurnStart::Prompt,
                    Some("ship the pier map\nwith the legend"),
                    TurnEnd::Answered
                ),
                (
                    TurnStart::Prompt,
                    Some("and print it for the harbour office"),
                    TurnEnd::Open
                ),
            ]
        );
        let ids: std::collections::HashSet<_> = read.turns.iter().map(|turn| &turn.id).collect();
        assert_eq!(ids.len(), read.turns.len(), "every turn has its own id");
        assert!(read.turns.windows(2).all(|pair| pair[0].at < pair[1].at));
        assert_eq!(read.turns[0].at, 1772614807);
    }

    #[test]
    fn a_claude_answer_is_the_end_turn_message_text_and_nothing_said_before_it() {
        let turns = turns_of(RECORD);
        // The thinking block sharing the message's id and the commentary
        // between tool calls are not the answer; both text blocks of the
        // final message are.
        assert_eq!(
            turns[0].answer.as_deref(),
            Some(
                "The importer drops the last sailing because the window ends at midnight.\n\n\
                 | sailing | time |\n|---|---|\n| last | 23:40 |\n\n\
                 Fixed by making the window inclusive: https://example.invalid/pull/7"
            )
        );
        assert_eq!(
            turns[3].answer.as_deref(),
            Some("The background export finished cleanly.")
        );
        assert_eq!(
            turns[5].answer.as_deref(),
            Some("Chunked by pier, and the old pier names are kept.")
        );
        assert_eq!(
            turns[6].answer.as_deref(),
            Some("The export queue is empty.")
        );
        // Turns that did not end in an answer carry none, the API's error
        // text included.
        for index in [1, 2, 4, 8] {
            assert_eq!(turns[index].answer, None, "turn {index}");
        }
        assert!(!format!("{turns:?}").contains("API Error"));
        assert!(!format!("{turns:?}").contains("Checking the importer"));
    }

    #[test]
    fn a_claude_head_is_the_prompts_first_line_as_one_row() {
        let turns = turns_of(RECORD);
        assert_eq!(
            turns[0].head.as_deref(),
            Some("trace why the ferry timetable importer drops the last sailing of the day")
        );
        assert_eq!(
            turns[1].head.as_deref(),
            Some("/review the harbour schedule parser")
        );
        assert_eq!(turns[3].head, None);
    }

    #[test]
    fn an_answer_past_the_byte_cap_is_cut_and_says_so() {
        let turns = turns_of(RECORD);
        let answer = turns[7].answer.as_deref().expect("an answer");
        assert!(turns[7].answer_cut);
        assert!(answer.len() <= ANSWER_BYTES);
        assert!(answer.len() > ANSWER_BYTES / 2);
        assert!(answer.starts_with("Section 1. The pier map ships"));
        assert!(!turns[7].prompt_cut);
        assert!(turns.iter().filter(|turn| turn.answer_cut).count() == 1);
    }

    fn line(value: serde_json::Value) -> String {
        value.to_string() + "\n"
    }

    fn typed(text: &str, at: &str) -> String {
        line(serde_json::json!({
            "type": "user", "uuid": format!("u-{at}"), "timestamp": at, "isSidechain": false,
            "message": { "role": "user", "content": text },
        }))
    }

    fn said(id: &str, kind: serde_json::Value, stop: &str) -> String {
        line(serde_json::json!({
            "type": "assistant", "timestamp": "2026-03-04T09:00:30Z", "isSidechain": false,
            "message": { "id": id, "role": "assistant", "content": [kind], "stop_reason": stop },
        }))
    }

    fn text(words: &str) -> serde_json::Value {
        serde_json::json!({ "type": "text", "text": words })
    }

    #[test]
    fn a_prompt_past_the_byte_cap_is_cut_and_keeps_its_whole_head() {
        let long = format!(
            "{}\n{}",
            "chart the harbour ".repeat(20),
            "x".repeat(PROMPT_BYTES)
        );
        let turns = turns_of(&typed(&long, "2026-03-04T09:00:00Z"));
        let prompt = turns[0].prompt.as_deref().unwrap();
        assert!(turns[0].prompt_cut);
        assert!(prompt.len() <= PROMPT_BYTES);
        assert!(long.starts_with(prompt));
        assert_eq!(
            turns[0].head.as_deref(),
            Some("chart the harbour ".repeat(20).trim())
        );
    }

    #[test]
    fn work_after_an_answer_reopens_the_turn_and_the_last_answer_counts() {
        let record = [
            typed("map the piers", "2026-03-04T09:00:00Z"),
            said("m1", text("First answer."), "end_turn"),
            // A stop hook sent the agent back to work.
            said(
                "m2",
                serde_json::json!({ "type": "tool_use", "id": "t", "name": "Bash", "input": {} }),
                "tool_use",
            ),
        ]
        .concat();
        assert_eq!(turns_of(&record)[0].end, TurnEnd::Open);
        let record = record + &said("m3", text("Second answer."), "end_turn");
        let turns = turns_of(&record);
        assert_eq!(turns[0].end, TurnEnd::Answered);
        assert_eq!(turns[0].answer.as_deref(), Some("Second answer."));
    }

    #[test]
    fn an_answer_gathers_every_text_block_written_under_its_message_id() {
        let record = [
            typed("map the piers", "2026-03-04T09:00:00Z"),
            said("m1", text("Opening paragraph."), "tool_use"),
            said("m2", text("Not this one."), "tool_use"),
            said("m3", text("Final, first part."), "end_turn"),
            said("m3", text("Final, second part."), "end_turn"),
        ]
        .concat();
        assert_eq!(
            turns_of(&record)[0].answer.as_deref(),
            Some("Final, first part.\n\nFinal, second part.")
        );
    }

    #[test]
    fn a_sidechain_line_that_reached_the_record_is_no_turn() {
        let sidechain = line(serde_json::json!({
            "type": "user", "uuid": "side", "timestamp": "2026-03-04T09:00:10Z", "isSidechain": true,
            "message": { "role": "user", "content": "a sub-agent's own brief" },
        }));
        let record = [typed("map the piers", "2026-03-04T09:00:00Z"), sidechain].concat();
        let turns = turns_of(&record);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].prompt.as_deref(), Some("map the piers"));
    }

    #[test]
    fn an_answer_with_no_words_is_answered_with_nothing_to_quote() {
        let record = [
            typed("map the piers", "2026-03-04T09:00:00Z"),
            said(
                "m1",
                serde_json::json!({ "type": "thinking", "thinking": "" }),
                "end_turn",
            ),
        ]
        .concat();
        let turns = turns_of(&record);
        assert_eq!(turns[0].end, TurnEnd::Answered);
        assert_eq!(turns[0].answer, None);
    }

    #[test]
    fn a_line_the_harness_says_someone_else_wrote_is_no_prompt() {
        let from = |kind: &str| {
            line(serde_json::json!({
                "type": "user", "uuid": format!("u-{kind}"), "timestamp": "2026-03-04T09:00:10Z",
                "isSidechain": false, "origin": { "kind": kind },
                "message": { "role": "user", "content": format!("words from {kind}") },
            }))
        };
        let record = [
            typed("map the piers", "2026-03-04T09:00:00Z"),
            from("auto-continuation"),
            from("peer"),
            from("human"),
        ]
        .concat();
        let prompts: Vec<_> = turns_of(&record)
            .into_iter()
            .map(|turn| turn.prompt)
            .collect();
        // A line with no origin at all predates the harness writing one.
        assert_eq!(
            prompts,
            vec![
                Some("map the piers".to_string()),
                Some("words from human".to_string())
            ]
        );
    }

    #[test]
    fn a_pasted_claude_prompt_reads_as_its_text_and_its_first_line_is_its_head() {
        let turns = turns_of(&typed(
            "\n<pasted_content id=\"ec3c\">\nchart the north breakwater\nand mark the shoals\n</pasted_content id=\"ec3c\">",
            "2026-03-04T09:00:00Z",
        ));
        assert_eq!(turns.len(), 1);
        assert_eq!(
            turns[0].prompt.as_deref(),
            Some("chart the north breakwater\nand mark the shoals")
        );
        assert_eq!(turns[0].head.as_deref(), Some("chart the north breakwater"));
    }

    #[test]
    fn a_queued_claude_paste_prompt_reads_as_its_text_and_its_first_line_is_its_head() {
        let queued = line(serde_json::json!({
            "type": "attachment",
            "uuid": "u-queued",
            "timestamp": "2026-03-04T09:00:10Z",
            "isSidechain": false,
            "attachment": {
                "type": "queued_command",
                "commandMode": "prompt",
                "origin": { "kind": "human" },
                "prompt": "<pasted_content id=\"b4a1\">\nkeep the south jetty beacon lit\nuntil dawn\n</pasted_content id=\"b4a1\">",
            },
        }));
        let turns = turns_of(&queued);
        assert_eq!(turns.len(), 1);
        assert_eq!(
            turns[0].prompt.as_deref(),
            Some("keep the south jetty beacon lit\nuntil dawn")
        );
        assert_eq!(
            turns[0].head.as_deref(),
            Some("keep the south jetty beacon lit")
        );
    }
}
