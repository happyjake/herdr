//! Turns from an Antigravity CLI (agy) conversation transcript.
//!
//! agy writes each conversation step as one JSON line to
//! `transcript_full.jsonl`; this reads that file alone, never agy's
//! SQLite or protobuf stores, and never the truncated `transcript.jsonl`.
//!
//! A turn starts at a prompt the person typed — a `USER_INPUT` step whose
//! source is `USER_EXPLICIT` — or, while the agent is idle, at a system
//! message announcing that a background task finished. Its final answer is
//! the last planner response with content and no tool calls; any later
//! tool step reopens it, and an error message after the last response ends
//! it failed. Other inputs, compaction checkpoints, thinking and tool
//! traffic never become part of a turn.

use std::io::BufRead;

use serde_json::Value;

use crate::api::schema::{PromptTrailReason, TurnEnd, TurnStart};
use crate::prompt_trail::{
    read_line_bounded, unix_seconds_from_rfc3339, LineRead, MAX_LINE_BYTES, MAX_RECORD_BYTES,
};

/// The most of a prompt one turn carries.
pub(crate) const PROMPT_BYTES: usize = 4 * 1024;
/// The most of a final answer one turn carries.
pub(crate) const ANSWER_BYTES: usize = 16 * 1024;

const REQUEST_OPEN: &str = "<USER_REQUEST>\n";
const REQUEST_CLOSE: &str = "\n</USER_REQUEST>";
const REQUEST_END: &str = "\n</USER_REQUEST>\n<ADDITIONAL_METADATA>";

/// One turn, oldest first in [`AgyRecord::turns`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgyTurn {
    /// The step index of the step that started the turn.
    pub id: String,
    /// Start time in unix seconds.
    pub at: u64,
    pub start: TurnStart,
    /// The prompt's text, at most [`PROMPT_BYTES`]. None for a task turn.
    pub prompt: Option<String>,
    pub prompt_cut: bool,
    /// The prompt's first non-blank line, cleaned and cut.
    pub prompt_head: Option<String>,
    pub end: TurnEnd,
    /// The final answer's text, at most [`ANSWER_BYTES`]. None unless the
    /// turn ended answered.
    pub answer: Option<String>,
    pub answer_cut: bool,
    /// The final answer's first non-blank, non-fence line, cleaned and cut.
    pub answer_head: Option<String>,
    /// The step index of the final answer.
    pub answer_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgyRecord {
    pub turns: Vec<AgyTurn>,
    /// The step index of the last whole step.
    pub last_step: Option<String>,
    /// True when the record ended in a half-written line.
    pub partial: bool,
}

/// Read every turn of one transcript, oldest first.
pub(crate) fn read_turns(mut reader: impl BufRead) -> Result<AgyRecord, PromptTrailReason> {
    let mut turns: Vec<AgyTurn> = Vec::new();
    let mut last_step = None;
    let mut partial = false;
    let mut consumed = 0;
    let mut line = Vec::new();
    loop {
        let before = consumed;
        match read_line_bounded(&mut reader, &mut line, &mut consumed, MAX_LINE_BYTES) {
            Ok(LineRead::End) => break,
            Ok(LineRead::Line) if consumed <= MAX_RECORD_BYTES => {}
            _ => return Err(PromptTrailReason::Unreadable),
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let step: Value = match serde_json::from_slice(&line) {
            Ok(step) => step,
            // A last line with no newline that stops mid-value is a step
            // still being written; anything else malformed is corruption.
            Err(error)
                if consumed - before == line.len() as u64
                    && match std::str::from_utf8(&line) {
                        Ok(_) => error.is_eof(),
                        Err(error) => error.error_len().is_none(),
                    } =>
            {
                partial = true;
                break;
            }
            Err(_) => return Err(PromptTrailReason::Unreadable),
        };
        let index = step["step_index"]
            .as_u64()
            .ok_or(PromptTrailReason::Unreadable)?
            .to_string();
        apply(&mut turns, &step, &index)?;
        last_step = Some(index);
    }
    if partial {
        if let Some(turn) = turns.last_mut() {
            reopen(turn);
        }
    }
    Ok(AgyRecord {
        turns,
        last_step,
        partial,
    })
}

fn apply(turns: &mut Vec<AgyTurn>, step: &Value, index: &str) -> Result<(), PromptTrailReason> {
    let kind = step["type"].as_str().ok_or(PromptTrailReason::Unreadable)?;
    match kind {
        "USER_INPUT" if step["source"] == "USER_EXPLICIT" => {
            let text = request_text(step["content"].as_str().unwrap_or_default());
            let (prompt, prompt_cut) = bounded(text, PROMPT_BYTES);
            turns.push(AgyTurn {
                prompt_head: crate::turn_index::text_head(text, false),
                prompt: Some(prompt),
                prompt_cut,
                ..started(step, index, TurnStart::Prompt)?
            });
        }
        // A finished task wakes an idle agent into a turn of its own; while
        // the agent is still working, the notice is part of the turn.
        "SYSTEM_MESSAGE" if turns.last().is_none_or(|turn| turn.end != TurnEnd::Open) => {
            turns.push(started(step, index, TurnStart::Task)?);
        }
        "PLANNER_RESPONSE" => {
            let Some(turn) = turns.last_mut() else {
                return Ok(());
            };
            let calls_tools = step["tool_calls"]
                .as_array()
                .is_some_and(|calls| !calls.is_empty());
            match step["content"]
                .as_str()
                .filter(|text| !text.trim().is_empty())
            {
                Some(text) if !calls_tools => {
                    let (answer, answer_cut) = bounded(text, ANSWER_BYTES);
                    turn.end = TurnEnd::Answered;
                    turn.answer_head = crate::turn_index::text_head(text, true);
                    turn.answer = Some(answer);
                    turn.answer_cut = answer_cut;
                    turn.answer_id = Some(index.into());
                }
                _ => reopen(turn),
            }
        }
        "GENERIC" => {
            if let Some(turn) = turns.last_mut() {
                reopen(turn);
            }
        }
        "ERROR_MESSAGE" => {
            if let Some(turn) = turns.last_mut() {
                reopen(turn);
                turn.end = TurnEnd::Error;
            }
        }
        // Other inputs, compaction checkpoints and steps of kinds this
        // does not know leave the turns as they are.
        _ => {}
    }
    Ok(())
}

fn started(step: &Value, index: &str, start: TurnStart) -> Result<AgyTurn, PromptTrailReason> {
    Ok(AgyTurn {
        id: index.into(),
        at: step["created_at"]
            .as_str()
            .and_then(unix_seconds_from_rfc3339)
            .ok_or(PromptTrailReason::Unreadable)?,
        start,
        prompt: None,
        prompt_cut: false,
        prompt_head: None,
        end: TurnEnd::Open,
        answer: None,
        answer_cut: false,
        answer_head: None,
        answer_id: None,
    })
}

fn reopen(turn: &mut AgyTurn) {
    turn.end = TurnEnd::Open;
    turn.answer = None;
    turn.answer_cut = false;
    turn.answer_head = None;
    turn.answer_id = None;
}

/// What the person typed: agy wraps it in a request block followed by
/// metadata blocks it adds itself. The metadata can quote anything, so the
/// request ends where the first metadata block begins.
fn request_text(content: &str) -> &str {
    let Some(body) = content.strip_prefix(REQUEST_OPEN) else {
        return content;
    };
    let end = body
        .find(REQUEST_END)
        .or_else(|| body.strip_suffix(REQUEST_CLOSE).map(str::len))
        .or_else(|| body.find(REQUEST_CLOSE))
        .unwrap_or(body.len());
    &body[..end]
}

/// Text held to a byte bound at a character boundary, and whether it was
/// cut.
fn bounded(text: &str, limit: usize) -> (String, bool) {
    if text.len() <= limit {
        return (text.to_string(), false);
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const FIXTURE: &str =
        include_str!("../tests/fixtures/session-record/agy/transcript_full.jsonl");

    fn fixture() -> AgyRecord {
        read_turns(FIXTURE.as_bytes()).expect("a readable transcript")
    }

    fn ids(record: &AgyRecord) -> Vec<&str> {
        record.turns.iter().map(|turn| turn.id.as_str()).collect()
    }

    #[test]
    fn agy_reader_reads_prompts_from_the_request_block_only() {
        let record = fixture();
        let prompts: Vec<_> = record
            .turns
            .iter()
            .map(|turn| {
                (
                    turn.start,
                    turn.prompt.as_deref(),
                    turn.prompt_head.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            prompts,
            [
                (
                    TurnStart::Prompt,
                    Some("  Explain\t the relay\nand why it waits"),
                    Some("Explain the relay")
                ),
                (TurnStart::Task, None, None),
                (
                    TurnStart::Prompt,
                    Some("Rename the beacon"),
                    Some("Rename the beacon")
                ),
                (
                    TurnStart::Prompt,
                    Some("Rename the beacon to lighthouse instead"),
                    Some("Rename the beacon to lighthouse instead")
                ),
                (
                    TurnStart::Prompt,
                    Some("Ship the lighthouse"),
                    Some("Ship the lighthouse")
                ),
                (
                    TurnStart::Prompt,
                    Some("Try shipping again"),
                    Some("Try shipping again")
                ),
            ]
        );
        assert_eq!(record.turns[0].at, 1772615700);
        assert!(record.turns.iter().all(|turn| !turn.prompt_cut));
    }

    #[test]
    fn agy_reader_filters_non_explicit_inputs_and_compaction() {
        let record = fixture();
        assert_eq!(ids(&record), ["0", "7", "11", "12", "20", "25"]);
        let text = format!("{:?}", record);
        for injected in [
            "did not type",
            "compaction",
            "ADDITIONAL_METADATA",
            "skill text",
            "settings text",
            "Neutral planning text",
            "Neutral tool output",
            "commentary",
            "closing thought",
            "neutral error text",
        ] {
            assert!(!text.contains(injected), "{injected} leaked into a turn");
        }
    }

    #[test]
    fn agy_reader_takes_each_turns_last_planner_response_without_tool_calls() {
        let record = fixture();
        let answers: Vec<_> = record
            .turns
            .iter()
            .map(|turn| {
                (
                    turn.end,
                    turn.answer_id.as_deref(),
                    turn.answer_head.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            answers,
            [
                (
                    TurnEnd::Answered,
                    Some("6"),
                    Some("The relay waits for the build.")
                ),
                (TurnEnd::Answered, Some("10"), Some("## The build passed")),
                (TurnEnd::Open, None, None),
                (
                    TurnEnd::Answered,
                    Some("19"),
                    Some("Renamed the beacon to lighthouse.")
                ),
                (TurnEnd::Error, None, None),
                (TurnEnd::Open, None, None),
            ]
        );
        assert_eq!(
            record.turns[0].answer.as_deref(),
            Some("The relay waits for the build.\nIt resumes when the build finishes.")
        );
        assert_eq!(
            record.turns[1].answer.as_deref(),
            Some(
                "```\n\n## The build passed\n\n| step | result |\n| --- | --- |\n| relay | ok |\n"
            )
        );
        assert_eq!(record.last_step.as_deref(), Some("27"));
        assert!(!record.partial);
    }

    #[test]
    fn agy_reader_folds_a_task_notice_into_a_turn_still_working() {
        let record = fixture();
        // Step 16 announces a task while turn 12 is still calling tools.
        assert!(!ids(&record).contains(&"16"));
        assert_eq!(record.turns[3].answer_id.as_deref(), Some("19"));
    }

    fn step(index: u64, kind: &str, source: &str, extra: Value) -> String {
        let mut step = json!({"step_index": index, "source": source, "type": kind,
            "status": "DONE", "created_at": "2026-03-04T09:15:00Z"});
        if let (Some(step), Some(extra)) = (step.as_object_mut(), extra.as_object()) {
            step.extend(extra.clone());
        }
        step.to_string() + "\n"
    }

    fn prompt(index: u64, text: &str) -> String {
        step(
            index,
            "USER_INPUT",
            "USER_EXPLICIT",
            json!({"content": format!("<USER_REQUEST>\n{text}\n</USER_REQUEST>")}),
        )
    }

    fn answer(index: u64, text: &str) -> String {
        step(index, "PLANNER_RESPONSE", "MODEL", json!({"content": text}))
    }

    #[test]
    fn agy_reader_later_steps_reopen_or_fail_an_answered_turn() {
        let tool = step(
            2,
            "PLANNER_RESPONSE",
            "MODEL",
            json!({"content": "more", "tool_calls": [{"name": "read_file", "args": {}}]}),
        );
        let result = step(3, "GENERIC", "MODEL", json!({"content": "out"}));
        let error = step(4, "ERROR_MESSAGE", "SYSTEM", json!({"error": "failed"}));
        let blank = answer(5, " \n");
        for (tail, end) in [
            (String::new(), TurnEnd::Answered),
            (tool.clone(), TurnEnd::Open),
            (result.clone(), TurnEnd::Open),
            (error.clone(), TurnEnd::Error),
            (error.clone() + &answer(6, "recovered"), TurnEnd::Answered),
            (blank, TurnEnd::Open),
        ] {
            let source = prompt(0, "go") + &answer(1, "done") + &tail;
            let record = read_turns(source.as_bytes()).unwrap();
            assert_eq!(record.turns.len(), 1);
            assert_eq!(record.turns[0].end, end, "after {tail:?}");
            assert_eq!(
                record.turns[0].answer.is_some(),
                end == TurnEnd::Answered,
                "after {tail:?}"
            );
        }
    }

    #[test]
    fn agy_reader_cuts_long_prompts_and_answers_at_a_character_boundary() {
        let long_prompt = "é".repeat(PROMPT_BYTES);
        let long_answer = format!("a{}", "é".repeat(ANSWER_BYTES));
        let source = prompt(0, &long_prompt) + &answer(1, &long_answer);
        let turn = &read_turns(source.as_bytes()).unwrap().turns[0];
        let kept = turn.prompt.as_deref().unwrap();
        assert!(turn.prompt_cut);
        assert_eq!(kept.len(), PROMPT_BYTES);
        assert!(long_prompt.starts_with(kept));
        let kept = turn.answer.as_deref().unwrap();
        assert!(turn.answer_cut);
        assert_eq!(kept.len(), ANSWER_BYTES - 1);
        assert!(long_answer.starts_with(kept));
        assert_eq!(
            turn.answer_head.as_deref().map(|head| head.ends_with('…')),
            Some(true)
        );

        let exact = "x".repeat(PROMPT_BYTES);
        let source = prompt(0, &exact);
        let turn = &read_turns(source.as_bytes()).unwrap().turns[0];
        assert_eq!(turn.prompt.as_deref(), Some(exact.as_str()));
        assert!(!turn.prompt_cut);
    }

    #[test]
    fn agy_reader_half_written_tail_opens_only_the_newest_turn() {
        let whole =
            prompt(0, "first") + &answer(1, "one") + &prompt(2, "second") + &answer(3, "two");
        let half = answer(4, "three");
        let source = whole.clone() + &half[..half.len() / 2];
        let record = read_turns(source.as_bytes()).unwrap();
        assert!(record.partial);
        assert_eq!(record.last_step.as_deref(), Some("3"));
        assert_eq!(record.turns[0].end, TurnEnd::Answered);
        assert_eq!(record.turns[1].end, TurnEnd::Open);
        assert_eq!(record.turns[1].answer, None);

        // A cut inside a multi-byte character is still a step being written.
        let accented = answer(4, "é");
        let cut_at = accented.find('é').unwrap() + 1;
        let source = [whole.as_bytes(), &accented.as_bytes()[..cut_at]].concat();
        assert!(read_turns(source.as_slice()).unwrap().partial);

        // A malformed whole line, or a cut line followed by another, is not.
        for corrupt in [
            whole.clone() + "{\"step_index\":4,\"type\":\n",
            whole.clone() + &half[..half.len() / 2] + "\n" + &answer(5, "five"),
            whole.clone() + "{\"type\":\"GENERIC\"}\n",
        ] {
            assert_eq!(
                read_turns(corrupt.as_bytes()),
                Err(PromptTrailReason::Unreadable)
            );
        }
    }

    #[test]
    fn agy_reader_starts_task_turns_only_while_idle_and_ignores_steps_before_a_turn() {
        let notice = |index| {
            step(
                index,
                "SYSTEM_MESSAGE",
                "SYSTEM",
                json!({"content": "task done"}),
            )
        };
        let source = answer(0, "orphan")
            + &notice(1)
            + &answer(2, "after the task")
            + &prompt(3, "go")
            + &notice(4)
            + &answer(5, "still the prompt's");
        let record = read_turns(source.as_bytes()).unwrap();
        assert_eq!(ids(&record), ["1", "3"]);
        assert_eq!(record.turns[0].start, TurnStart::Task);
        assert_eq!(record.turns[0].prompt, None);
        assert_eq!(record.turns[0].answer.as_deref(), Some("after the task"));
        assert_eq!(
            record.turns[1].answer.as_deref(),
            Some("still the prompt's")
        );
    }

    #[test]
    fn agy_reader_takes_unwrapped_or_unclosed_requests_whole() {
        for (content, expected) in [
            ("plain text", "plain text"),
            ("<USER_REQUEST>\nunclosed", "unclosed"),
            ("<USER_REQUEST>\nclosed at the end\n</USER_REQUEST>", "closed at the end"),
            (
                "<USER_REQUEST>\nbefore\n</USER_REQUEST>\n<USER_SETTINGS_CHANGE>\nx\n</USER_SETTINGS_CHANGE>",
                "before",
            ),
        ] {
            let source = step(0, "USER_INPUT", "USER_EXPLICIT", json!({"content": content}));
            let record = read_turns(source.as_bytes()).unwrap();
            assert_eq!(record.turns[0].prompt.as_deref(), Some(expected));
        }
    }

    #[test]
    fn agy_reader_refuses_lines_past_the_line_bound() {
        let source = prompt(0, &"x".repeat(MAX_LINE_BYTES + 1));
        assert_eq!(
            read_turns(source.as_bytes()),
            Err(PromptTrailReason::Unreadable)
        );
    }
}
