//! An Antigravity CLI (agy) conversation transcript, read into turns.
//!
//! agy writes each conversation step as one JSON line to
//! `transcript_full.jsonl`; this reads that file alone, never agy's
//! SQLite or protobuf stores, and never the truncated `transcript.jsonl`.
//!
//! What starts a turn: a prompt the person typed — a `USER_INPUT` step
//! whose source is `USER_EXPLICIT` — or, while the agent is idle, a system
//! message announcing that a background task finished, which starts an
//! answer-only `task` turn.
//!
//! What ends one: a planner response with content and no tool calls, which
//! is the final answer; or an error message after the last response. Any
//! later tool step reopens the turn, so the answer is always the last one.
//! Other inputs, compaction checkpoints, thinking and tool traffic never
//! become part of a turn.
//!
//! The same steps feed two readings: the preface's [`AgyReader`], and the
//! turn index's [`read_transcript`], which also says whether the record
//! ended in a half-written line.

use std::io::BufRead;
use std::path::Path;

use serde_json::Value;

use super::{RecordTurns, Turn};
use crate::api::schema::{PromptTrailReason, TurnEnd, TurnStart};
use crate::prompt_trail::{
    read_line_bounded, unix_seconds_from_rfc3339, LineRead, MAX_LINE_BYTES, MAX_RECORD_BYTES,
};

/// What agy draws at column 0 ahead of the first row of a person's prompt;
/// its own messages start with a bullet, and its empty input box is the
/// marker alone.
const PROMPT_MARKER: &str = ">";

const REQUEST_OPEN: &str = "<USER_REQUEST>\n";
const REQUEST_CLOSE: &str = "\n</USER_REQUEST>";
const REQUEST_END: &str = "\n</USER_REQUEST>\n<ADDITIONAL_METADATA>";

/// One turn as the transcript's steps leave it, its texts whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgyTurn {
    /// The step index of the step that started the turn.
    pub id: String,
    /// Start time in unix seconds.
    pub at: u64,
    pub start: TurnStart,
    /// What the person typed. None for a task turn.
    pub prompt: Option<String>,
    pub end: TurnEnd,
    /// The final answer. None unless the turn ended answered.
    pub answer: Option<String>,
    /// The step index of the final answer.
    pub answer_id: Option<String>,
}

/// The turns of a transcript, a step at a time.
#[derive(Debug, Default)]
pub(crate) struct AgySteps {
    turns: Vec<AgyTurn>,
    /// The step index of the last step taken.
    last_step: Option<String>,
}

impl AgySteps {
    /// Take one step. A step without the fields every step carries is
    /// refused and leaves the turns as they were.
    pub(crate) fn step(&mut self, step: &Value) -> Result<(), PromptTrailReason> {
        let index = step["step_index"]
            .as_u64()
            .ok_or(PromptTrailReason::Unreadable)?
            .to_string();
        let kind = step["type"].as_str().ok_or(PromptTrailReason::Unreadable)?;
        let turns = &mut self.turns;
        match kind {
            "USER_INPUT" if step["source"] == "USER_EXPLICIT" => {
                let prompt = request_text(step["content"].as_str().unwrap_or_default());
                turns.push(started(
                    step,
                    &index,
                    TurnStart::Prompt,
                    Some(prompt.into()),
                )?);
            }
            // A finished task wakes an idle agent into a turn of its own;
            // while the agent is still working, the notice is part of the
            // turn.
            "SYSTEM_MESSAGE" if turns.last().is_none_or(|turn| turn.end != TurnEnd::Open) => {
                turns.push(started(step, &index, TurnStart::Task, None)?);
            }
            "PLANNER_RESPONSE" => {
                if let Some(turn) = turns.last_mut() {
                    let calls_tools = step["tool_calls"]
                        .as_array()
                        .is_some_and(|calls| !calls.is_empty());
                    match step["content"]
                        .as_str()
                        .filter(|text| !text.trim().is_empty())
                    {
                        Some(text) if !calls_tools => {
                            turn.end = TurnEnd::Answered;
                            turn.answer = Some(text.into());
                            turn.answer_id = Some(index.clone());
                        }
                        _ => reopen(turn),
                    }
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
        self.last_step = Some(index);
        Ok(())
    }
}

fn started(
    step: &Value,
    index: &str,
    start: TurnStart,
    prompt: Option<String>,
) -> Result<AgyTurn, PromptTrailReason> {
    Ok(AgyTurn {
        id: index.into(),
        at: step["created_at"]
            .as_str()
            .and_then(unix_seconds_from_rfc3339)
            .ok_or(PromptTrailReason::Unreadable)?,
        start,
        prompt,
        end: TurnEnd::Open,
        answer: None,
        answer_id: None,
    })
}

fn reopen(turn: &mut AgyTurn) {
    turn.end = TurnEnd::Open;
    turn.answer = None;
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

/// The preface's reading of an agy transcript.
#[derive(Default)]
pub(crate) struct AgyReader {
    steps: AgySteps,
}

impl super::Reader for AgyReader {
    fn accept(&mut self, line: &Value) {
        // A step without its index or kind is stepped over, the way the
        // seam steps over a line that is no JSON.
        let _ = self.steps.step(line);
    }

    fn finish(self: Box<Self>) -> RecordTurns {
        RecordTurns {
            // The transcript never names its conversation; the directory
            // it is filed in does.
            session_id: None,
            turns: self.steps.turns.into_iter().map(preface_turn).collect(),
        }
    }

    fn prompt_markers(&self) -> &'static [&'static str] {
        &[PROMPT_MARKER]
    }
}

fn preface_turn(turn: AgyTurn) -> Turn {
    let mut preface = Turn::started(turn.id, turn.at, turn.start, turn.prompt);
    match (turn.end, turn.answer) {
        (TurnEnd::Answered, Some(answer)) => preface.answer(&answer),
        (end, _) => preface.end_without_answer(end),
    }
    preface
}

/// The conversation a transcript belongs to: agy files it as
/// `brain/<conversation id>/.system_generated/logs/transcript_full.jsonl`.
pub(crate) fn session_id(path: &Path) -> Option<String> {
    path.ancestors()
        .nth(3)
        .and_then(Path::file_name)
        .and_then(std::ffi::OsStr::to_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

/// A whole transcript as the turn index reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgyRecord {
    pub turns: Vec<AgyTurn>,
    /// The step index of the last whole step.
    pub last_step: Option<String>,
    /// True when the record ended in a half-written line.
    pub partial: bool,
}

/// Read every turn of one transcript, oldest first, refusing it whole when
/// any line but a half-written last one is not a step.
pub(crate) fn read_transcript(mut reader: impl BufRead) -> Result<AgyRecord, PromptTrailReason> {
    let mut steps = AgySteps::default();
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
        steps.step(&step)?;
    }
    let AgySteps {
        mut turns,
        last_step,
    } = steps;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::PromptTrailAgent;
    use crate::preface::{read_turns, ANSWER_BYTES, PROMPT_BYTES};
    use serde_json::json;

    /// A trimmed agy transcript in the harness's real line shapes — every
    /// key set taken from a real one, every text neutral and invented.
    const FIXTURE: &str =
        include_str!("../../tests/fixtures/session-record/agy/transcript_full.jsonl");

    fn fixture() -> AgyRecord {
        read_transcript(FIXTURE.as_bytes()).expect("a readable transcript")
    }

    fn ids(turns: &[AgyTurn]) -> Vec<&str> {
        turns.iter().map(|turn| turn.id.as_str()).collect()
    }

    #[test]
    fn agy_reader_reads_prompts_from_the_request_block_only() {
        let record = fixture();
        let prompts: Vec<_> = record
            .turns
            .iter()
            .map(|turn| (turn.start, turn.prompt.as_deref()))
            .collect();
        assert_eq!(
            prompts,
            [
                (
                    TurnStart::Prompt,
                    Some("  Explain\t the relay\nand why it waits")
                ),
                (TurnStart::Task, None),
                (TurnStart::Prompt, Some("Rename the beacon")),
                (
                    TurnStart::Prompt,
                    Some("Rename the beacon to lighthouse instead")
                ),
                (TurnStart::Prompt, Some("Ship the lighthouse")),
                (TurnStart::Prompt, Some("Try shipping again")),
            ]
        );
        assert_eq!(record.turns[0].at, 1772615700);
    }

    #[test]
    fn agy_reader_filters_non_explicit_inputs_and_compaction() {
        let record = fixture();
        assert_eq!(ids(&record.turns), ["0", "7", "11", "12", "20", "25"]);
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
            .map(|turn| (turn.end, turn.answer_id.as_deref()))
            .collect();
        assert_eq!(
            answers,
            [
                (TurnEnd::Answered, Some("6")),
                (TurnEnd::Answered, Some("10")),
                (TurnEnd::Open, None),
                (TurnEnd::Answered, Some("19")),
                (TurnEnd::Error, None),
                (TurnEnd::Open, None),
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
        assert!(!ids(&record.turns).contains(&"16"));
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
            let record = read_transcript(source.as_bytes()).unwrap();
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
    fn agy_reader_half_written_tail_opens_only_the_newest_turn() {
        let whole =
            prompt(0, "first") + &answer(1, "one") + &prompt(2, "second") + &answer(3, "two");
        let half = answer(4, "three");
        let source = whole.clone() + &half[..half.len() / 2];
        let record = read_transcript(source.as_bytes()).unwrap();
        assert!(record.partial);
        assert_eq!(record.last_step.as_deref(), Some("3"));
        assert_eq!(record.turns[0].end, TurnEnd::Answered);
        assert_eq!(record.turns[1].end, TurnEnd::Open);
        assert_eq!(record.turns[1].answer, None);

        // A cut inside a multi-byte character is still a step being written.
        let accented = answer(4, "é");
        let cut_at = accented.find('é').unwrap() + 1;
        let source = [whole.as_bytes(), &accented.as_bytes()[..cut_at]].concat();
        assert!(read_transcript(source.as_slice()).unwrap().partial);

        // A malformed whole line, or a cut line followed by another, is not.
        for corrupt in [
            whole.clone() + "{\"step_index\":4,\"type\":\n",
            whole.clone() + &half[..half.len() / 2] + "\n" + &answer(5, "five"),
            whole.clone() + "{\"type\":\"GENERIC\"}\n",
        ] {
            assert_eq!(
                read_transcript(corrupt.as_bytes()),
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
        let record = read_transcript(source.as_bytes()).unwrap();
        assert_eq!(ids(&record.turns), ["1", "3"]);
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
            (
                "<USER_REQUEST>\nclosed at the end\n</USER_REQUEST>",
                "closed at the end",
            ),
            (
                "<USER_REQUEST>\nbefore\n</USER_REQUEST>\n<USER_SETTINGS_CHANGE>\nx\n</USER_SETTINGS_CHANGE>",
                "before",
            ),
        ] {
            let source = step(0, "USER_INPUT", "USER_EXPLICIT", json!({"content": content}));
            let record = read_transcript(source.as_bytes()).unwrap();
            assert_eq!(record.turns[0].prompt.as_deref(), Some(expected));
        }
    }

    #[test]
    fn agy_reader_refuses_lines_past_the_line_bound() {
        let source = prompt(0, &"x".repeat(MAX_LINE_BYTES + 1));
        assert_eq!(
            read_transcript(source.as_bytes()),
            Err(PromptTrailReason::Unreadable)
        );
    }

    fn preface_turns(record: &str) -> RecordTurns {
        read_turns(record.as_bytes(), PromptTrailAgent::Agy).expect("a readable transcript")
    }

    #[test]
    fn an_agy_preface_reading_quotes_prompts_and_final_answers_whole() {
        let read = preface_turns(FIXTURE);
        assert_eq!(read.session_id, None);
        let turns: Vec<_> = read
            .turns
            .iter()
            .map(|turn| {
                (
                    turn.id.as_str(),
                    turn.start,
                    turn.head.as_deref(),
                    turn.end,
                    turn.answer.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            turns,
            [
                (
                    "0",
                    TurnStart::Prompt,
                    Some("Explain the relay"),
                    TurnEnd::Answered,
                    Some("The relay waits for the build.\nIt resumes when the build finishes."),
                ),
                (
                    "7",
                    TurnStart::Task,
                    None,
                    TurnEnd::Answered,
                    Some("```\n\n## The build passed\n\n| step | result |\n| --- | --- |\n| relay | ok |"),
                ),
                (
                    "11",
                    TurnStart::Prompt,
                    Some("Rename the beacon"),
                    TurnEnd::Open,
                    None
                ),
                (
                    "12",
                    TurnStart::Prompt,
                    Some("Rename the beacon to lighthouse instead"),
                    TurnEnd::Answered,
                    Some("Renamed the beacon to lighthouse."),
                ),
                (
                    "20",
                    TurnStart::Prompt,
                    Some("Ship the lighthouse"),
                    TurnEnd::Error,
                    None
                ),
                (
                    "25",
                    TurnStart::Prompt,
                    Some("Try shipping again"),
                    TurnEnd::Open,
                    None
                ),
            ]
        );
        assert_eq!(
            read.turns[0].prompt.as_deref(),
            Some("Explain\t the relay\nand why it waits")
        );
        assert_eq!(read.turns[0].at, 1772615700);
    }

    #[test]
    fn an_agy_preface_reading_cuts_long_prompts_and_answers_and_says_so() {
        let long_prompt = "a long prompt line\n".repeat(PROMPT_BYTES / 10);
        let long_answer = "a long answer line\n".repeat(ANSWER_BYTES / 10);
        let source = prompt(0, &long_prompt) + &answer(1, &long_answer);
        let read = preface_turns(&source);
        let turn = &read.turns[0];
        assert!(turn.prompt_cut);
        assert!(turn
            .prompt
            .as_deref()
            .is_some_and(|kept| kept.len() <= PROMPT_BYTES && long_prompt.starts_with(kept)));
        assert_eq!(turn.head.as_deref(), Some("a long prompt line"));
        assert!(turn.answer_cut);
        assert!(turn
            .answer
            .as_deref()
            .is_some_and(|kept| kept.len() <= ANSWER_BYTES && long_answer.starts_with(kept)));

        let short = preface_turns(&(prompt(0, "short") + &answer(1, "brief")));
        assert!(!short.turns[0].prompt_cut && !short.turns[0].answer_cut);
    }

    #[test]
    fn an_agy_preface_reading_steps_over_lines_that_are_not_steps() {
        let source = prompt(0, "go")
            + "{\"type\":\"GENERIC\"}\n"
            + "not json\n"
            + &answer(1, "done")
            + &answer(2, "half")[..10];
        let read = preface_turns(&source);
        assert_eq!(read.turns.len(), 1);
        assert_eq!(read.turns[0].answer.as_deref(), Some("done"));
    }

    #[test]
    fn an_agy_session_is_named_by_the_directory_its_transcript_is_filed_in() {
        assert_eq!(
            session_id(Path::new(
                "/home/brain/conv-1/.system_generated/logs/transcript_full.jsonl"
            ))
            .as_deref(),
            Some("conv-1")
        );
        assert_eq!(session_id(Path::new("transcript_full.jsonl")), None);
    }
}
