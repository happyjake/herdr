//! codex rollouts: one turn per task the harness started, from its
//! `task_started` to its `task_complete` or `turn_aborted`.
//!
//! A turn's prompt is what the person typed into it — the user messages left
//! once the context codex injects is filtered out, by the same rules the
//! prompt trail reads with. Its answer is the assistant message in the
//! `final_answer` phase, or failing that the last agent message
//! `task_complete` carries. A compaction rewrites what the model sees, not
//! what was said, so it is stepped over; a rollback removes the turns it
//! names, as codex itself forgets them.
//!
//! `codex resume` keeps appending to the conversation's own rollout, so one
//! record holds every turn from before a resume and after it.

use std::io::BufRead;

use serde_json::Value;

use super::{quoted, RecordStart, RecordTurn};
use crate::api::schema::{PromptTrailReason, TurnEnd};
use crate::prompt_trail::{
    clean, codex_injected, join_text_blocks, read_line_bounded, replace_image_tags,
    unix_seconds_from_rfc3339, LineRead, MAX_LINE_BYTES, MAX_RECORD_BYTES,
};

/// The person's reply to a question the agent asked mid-turn. It answers
/// the agent rather than starting anything, so it is no part of a prompt.
const QUESTION_REPLY: &str = "<send_user_message_question_reply";

/// A turn while its lines are still being read.
struct Building {
    /// codex's own id for the turn, which its ending names too.
    turn_id: Option<String>,
    id: String,
    at: u64,
    prompts: Vec<String>,
    final_answer: Option<String>,
}

/// Every turn the rollout holds, oldest first.
pub(crate) fn turns(reader: impl BufRead) -> Result<Vec<RecordTurn>, PromptTrailReason> {
    read(reader, MAX_RECORD_BYTES)
}

fn read(mut reader: impl BufRead, record_bytes: u64) -> Result<Vec<RecordTurn>, PromptTrailReason> {
    let mut turns = Vec::new();
    let mut open: Option<Building> = None;
    let mut lines_read: u64 = 0;
    let mut lines_parsed: u64 = 0;
    let mut consumed: u64 = 0;
    let mut line = Vec::new();
    loop {
        let before = consumed;
        match read_line_bounded(&mut reader, &mut line, &mut consumed, MAX_LINE_BYTES) {
            Ok(LineRead::End) => break,
            Ok(LineRead::Line) if consumed <= record_bytes => {}
            _ => return Err(PromptTrailReason::Unreadable),
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let Ok(parsed) = serde_json::from_slice::<Value>(&line) else {
            // A last line with no end yet is one still being written, and
            // says nothing about what the record is.
            if consumed - before != line.len() as u64 {
                lines_read += 1;
            }
            continue;
        };
        lines_read += 1;
        lines_parsed += 1;
        take_line(&parsed, lines_read, &mut open, &mut turns)?;
    }
    // Something was in this file and none of it was JSON: not a rollout.
    if lines_read > 0 && lines_parsed == 0 {
        return Err(PromptTrailReason::Unreadable);
    }
    if let Some(building) = open {
        close(building, TurnEnd::Open, None, &mut turns);
    }
    Ok(turns)
}

fn take_line(
    line: &Value,
    number: u64,
    open: &mut Option<Building>,
    turns: &mut Vec<RecordTurn>,
) -> Result<(), PromptTrailReason> {
    let payload = &line["payload"];
    match (line["type"].as_str(), payload["type"].as_str()) {
        (Some("event_msg"), Some("task_started")) => {
            // A turn that never ended before the next began was cut off.
            if let Some(building) = open.take() {
                close(building, TurnEnd::Open, None, turns);
            }
            let at = line_at(line)
                .or_else(|| payload["started_at"].as_u64())
                .ok_or(PromptTrailReason::Unreadable)?;
            let turn_id = payload["turn_id"].as_str().map(str::to_owned);
            *open = Some(Building {
                id: turn_id.clone().unwrap_or_else(|| format!("line-{number}")),
                turn_id,
                at,
                prompts: Vec::new(),
                final_answer: None,
            });
        }
        (Some("response_item"), Some("message")) => match payload["role"].as_str() {
            Some("user") => {
                let Some(prompt) = typed_prompt(payload) else {
                    return Ok(());
                };
                if open.is_none() {
                    *open = Some(Building {
                        turn_id: None,
                        id: format!("line-{number}"),
                        at: line_at(line).ok_or(PromptTrailReason::Unreadable)?,
                        prompts: Vec::new(),
                        final_answer: None,
                    });
                }
                if let Some(building) = open.as_mut() {
                    building.prompts.push(prompt);
                }
            }
            Some("assistant") if payload["phase"] == "final_answer" => {
                if let Some(building) = open.as_mut() {
                    let blocks = payload["content"].as_array().map(Vec::as_slice);
                    building.final_answer =
                        quoted(&join_text_blocks(blocks.unwrap_or_default(), "output_text"));
                }
            }
            _ => {}
        },
        (Some("event_msg"), Some("task_complete")) => {
            if let Some(building) = take_matching(open, payload) {
                let answer = building
                    .final_answer
                    .clone()
                    .or_else(|| payload["last_agent_message"].as_str().and_then(quoted));
                close(building, TurnEnd::Answered, answer, turns);
            }
        }
        (Some("event_msg"), Some("turn_aborted")) => {
            if let Some(building) = take_matching(open, payload) {
                close(building, TurnEnd::Aborted, None, turns);
            }
        }
        (Some("event_msg"), Some("thread_rolled_back")) => {
            let count = payload["num_turns"].as_u64().unwrap_or(0);
            roll_back(turns, count);
        }
        // The summary a compaction leaves, and the history it replays, are
        // the model's working copy; the turns they stand for are already
        // in this record as they were said.
        (Some("compacted"), _) => {}
        _ => {}
    }
    Ok(())
}

/// The open turn, when this ending is its own. An ending naming some other
/// turn leaves it running.
fn take_matching(open: &mut Option<Building>, payload: &Value) -> Option<Building> {
    let ends = payload["turn_id"].as_str();
    let matches = match (open.as_ref()?.turn_id.as_deref(), ends) {
        (Some(open_id), Some(ends)) => open_id == ends,
        _ => true,
    };
    if matches {
        open.take()
    } else {
        None
    }
}

/// Finish one turn. A turn nobody prompted is kept only for what it
/// answered: a sub-agent's report or a goal's continuation is a task, and a
/// turn with neither a prompt nor an answer — a review pass, an aborted
/// notification — says nothing worth quoting.
fn close(building: Building, end: TurnEnd, answer: Option<String>, turns: &mut Vec<RecordTurn>) {
    let answer = if end == TurnEnd::Answered {
        answer
    } else {
        None
    };
    let (start, prompt) = if building.prompts.is_empty() {
        if answer.is_none() {
            return;
        }
        (RecordStart::Task, None)
    } else {
        (RecordStart::Prompt, Some(building.prompts.join("\n\n")))
    };
    let mut turn = RecordTurn::new(building.id, building.at, start, prompt);
    turn.end = end;
    turn.answer = answer;
    turns.push(turn);
}

/// Forget the newest `count` prompted turns, and whatever followed them.
fn roll_back(turns: &mut Vec<RecordTurn>, mut count: u64) {
    while count > 0 {
        let Some(turn) = turns.pop() else {
            return;
        };
        if turn.start == RecordStart::Prompt {
            count -= 1;
        }
    }
}

/// What a person typed into one user message, when they typed it.
fn typed_prompt(payload: &Value) -> Option<String> {
    let raw = join_text_blocks(payload["content"].as_array()?, "input_text");
    let cleaned = clean(&raw);
    if codex_injected(&cleaned) || cleaned.starts_with(QUESTION_REPLY) {
        return None;
    }
    quoted(&replace_image_tags(&raw))
}

fn line_at(line: &Value) -> Option<u64> {
    line["timestamp"]
        .as_str()
        .and_then(unix_seconds_from_rfc3339)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../tests/fixtures/session-record/codex.jsonl");

    fn turn_id(n: u32) -> String {
        format!("00000000-0000-7000-8000-00000000000{n}")
    }

    type Shape = (String, RecordStart, Option<String>, TurnEnd, Option<String>);

    fn shape(turn: &RecordTurn) -> Shape {
        (
            turn.id.clone(),
            turn.start,
            turn.prompt.clone(),
            turn.end,
            turn.answer.clone(),
        )
    }

    fn expect(
        n: u32,
        start: RecordStart,
        prompt: Option<&str>,
        end: TurnEnd,
        answer: Option<&str>,
    ) -> Shape {
        (
            turn_id(n),
            start,
            prompt.map(str::to_owned),
            end,
            answer.map(str::to_owned),
        )
    }

    #[test]
    fn codex_record_turns_quote_what_was_typed_and_what_was_concluded() {
        let turns = turns(FIXTURE.as_bytes()).unwrap();
        assert_eq!(
            turns.iter().map(shape).collect::<Vec<_>>(),
            vec![
                expect(
                    1,
                    RecordStart::Prompt,
                    Some("Summarize   the layout module\n\nand list its public functions."),
                    TurnEnd::Answered,
                    Some(
                        "The layout module has two parts.\n\n| name | role |\n| --- | --- |\n| split | divides space |"
                    ),
                ),
                expect(
                    2,
                    RecordStart::Prompt,
                    Some("Compare these:\n[image]\nwhich one is wider?"),
                    TurnEnd::Answered,
                    Some("The first one is wider."),
                ),
                expect(
                    3,
                    RecordStart::Prompt,
                    Some("Rename every helper."),
                    TurnEnd::Aborted,
                    None,
                ),
                expect(
                    4,
                    RecordStart::Prompt,
                    Some("Only rename the private ones."),
                    TurnEnd::Answered,
                    Some("Renamed the private helpers."),
                ),
                expect(
                    6,
                    RecordStart::Task,
                    None,
                    TurnEnd::Answered,
                    Some("Goal step finished."),
                ),
                expect(
                    8,
                    RecordStart::Prompt,
                    Some("Continue with the tests."),
                    TurnEnd::Answered,
                    Some("Tests added."),
                ),
                expect(
                    9,
                    RecordStart::Prompt,
                    Some("Run the full check."),
                    TurnEnd::Open,
                    None,
                ),
            ]
        );
        assert_eq!(
            turns[0].prompt_head.as_deref(),
            Some("Summarize the layout module")
        );
        // 2026-03-04T09:00:01Z, the turn's start, not its first prompt's.
        assert_eq!(turns[0].at, 1_772_614_801);
        // Resumed the next day into the same rollout.
        assert_eq!(turns[5].at, 1_772_704_801);
    }

    #[test]
    fn codex_record_turns_never_quote_injected_context_or_compaction() {
        let turns = turns(FIXTURE.as_bytes()).unwrap();
        let quoted: Vec<&str> = turns
            .iter()
            .flat_map(|turn| [turn.prompt.as_deref(), turn.answer.as_deref()])
            .flatten()
            .collect();
        for leaked in [
            "AGENTS.md",
            "environment_context",
            "skill",
            "send_user_message_question_reply",
            "turn_aborted",
            "user_action",
            "codex_internal_context",
            "Replayed history",
            "Reading the module first",
            "A different closing message",
            "A prompt taken back",
            "neutral tool",
        ] {
            assert!(
                quoted.iter().all(|text| !text.contains(leaked)),
                "{leaked} leaked into {quoted:?}"
            );
        }
    }

    #[test]
    fn codex_record_turns_survive_every_byte_cut() {
        let full = turns(FIXTURE.as_bytes()).unwrap();
        let rolled_back = turn_id(7);
        let mut cuts = vec![0, FIXTURE.len()];
        let mut start = 0;
        for line in FIXTURE.split_inclusive('\n') {
            let end = start + line.len();
            cuts.extend([start + 1, start + line.len() / 2, end - 1, end]);
            start = end;
        }
        for cut in cuts {
            let source = &FIXTURE.as_bytes()[..cut.min(FIXTURE.len())];
            let turns = turns(source).unwrap_or_else(|error| panic!("cut {cut}: {error:?}"));
            let Some((last, settled)) = turns.split_last() else {
                continue;
            };
            for turn in settled {
                if turn.id == rolled_back {
                    continue;
                }
                let whole = full.iter().find(|whole| whole.id == turn.id);
                assert_eq!(Some(turn), whole, "cut {cut}");
            }
            assert!(
                last.end == TurnEnd::Answered || last.answer.is_none(),
                "cut {cut}: an unanswered turn carries an answer"
            );
        }
    }

    #[test]
    fn codex_record_turns_end_only_the_turn_an_ending_names() {
        let source = [
            r#"{"timestamp":"2026-03-04T09:00:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":"mine"}}"#,
            r#"{"timestamp":"2026-03-04T09:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Still going"}]}}"#,
            r#"{"timestamp":"2026-03-04T09:00:02Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"another","last_agent_message":"Not mine"}}"#,
        ]
        .join("\n");
        let turns = turns(source.as_bytes()).unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].end, TurnEnd::Open);
        assert_eq!(turns[0].answer, None);
    }

    #[test]
    fn codex_record_turns_keep_a_turn_cut_off_by_the_next_as_unfinished() {
        let source = [
            r#"{"timestamp":"2026-03-04T09:00:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":"first"}}"#,
            r#"{"timestamp":"2026-03-04T09:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Left running"}]}}"#,
            r#"{"timestamp":"2026-03-04T09:00:02Z","type":"event_msg","payload":{"type":"task_started","turn_id":"second"}}"#,
            r#"{"timestamp":"2026-03-04T09:00:03Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Next"}]}}"#,
            r#"{"timestamp":"2026-03-04T09:00:04Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"second","last_agent_message":"Done"}}"#,
        ]
        .join("\n");
        let turns = turns(source.as_bytes()).unwrap();
        assert_eq!(
            turns
                .iter()
                .map(|turn| (turn.id.as_str(), turn.end))
                .collect::<Vec<_>>(),
            vec![("first", TurnEnd::Open), ("second", TurnEnd::Answered)]
        );
    }

    #[test]
    fn codex_record_turns_refuse_records_past_their_bounds() {
        assert_eq!(
            read(FIXTURE.as_bytes(), 1024),
            Err(PromptTrailReason::Unreadable)
        );
        assert_eq!(
            turns("not json\nstill not json\n".as_bytes()),
            Err(PromptTrailReason::Unreadable)
        );
        assert_eq!(turns("".as_bytes()), Ok(Vec::new()));
    }
}
