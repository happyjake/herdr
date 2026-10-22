//! pi records: the last-recorded branch, walked exactly as the turn index
//! walks it, keeping each turn's whole prompt and whole final answer.

use std::io::BufRead;

use serde_json::Value;

use super::{quoted, RecordStart, RecordTurn};
use crate::api::schema::{PromptTrailReason, TurnEnd, TurnStart};
use crate::turn_index::{assistant_end, last_branch, start_at, turn_start};

enum Step {
    Start(RecordTurn),
    Assistant {
        end: TurnEnd,
        answer: Option<String>,
    },
    ToolResult,
    Other,
}

/// Every turn on the record's last-recorded branch, oldest first.
///
/// A turn's last assistant message decides how it ended; a tool result after
/// it means the agent is still at work, and so does a record whose last line
/// was cut mid-write.
pub(crate) fn turns(reader: impl BufRead) -> Result<Vec<RecordTurn>, PromptTrailReason> {
    let branch = last_branch(reader, step)?;
    let mut turns: Vec<RecordTurn> = Vec::new();
    for entry in branch.entries {
        match entry {
            Step::Start(turn) => turns.push(turn),
            Step::Assistant { end, answer } => {
                if let Some(turn) = turns.last_mut() {
                    turn.end = end;
                    turn.answer = answer;
                }
            }
            Step::ToolResult => {
                if let Some(turn) = turns.last_mut() {
                    reopen(turn);
                }
            }
            Step::Other => {}
        }
    }
    if branch.partial {
        if let Some(turn) = turns.last_mut() {
            reopen(turn);
        }
    }
    Ok(turns)
}

fn step(value: &Value, id: &str) -> Result<Step, PromptTrailReason> {
    let message = &value["message"];
    if let Some(start) = turn_start(value) {
        let at = start_at(value)?;
        let turn = match start {
            TurnStart::Prompt => RecordTurn::new(
                id.into(),
                at,
                RecordStart::Prompt,
                text(&message["content"]),
            ),
            // A wake's own text is the harness's reminder, not a prompt.
            TurnStart::Wake => RecordTurn::new(id.into(), at, RecordStart::Wake, None),
        };
        return Ok(Step::Start(turn));
    }
    if value["type"] != "message" {
        return Ok(Step::Other);
    }
    if message["role"] == "toolResult" {
        return Ok(Step::ToolResult);
    }
    if message["role"] != "assistant" {
        return Ok(Step::Other);
    }
    let end = assistant_end(message);
    Ok(Step::Assistant {
        end,
        answer: if end == TurnEnd::Answered {
            text(&message["content"])
        } else {
            None
        },
    })
}

fn reopen(turn: &mut RecordTurn) {
    turn.end = TurnEnd::Open;
    turn.answer = None;
}

/// The text parts of a message, and nothing else of it: never its thinking,
/// its tool calls or its images.
fn text(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => quoted(text),
        Value::Array(parts) => quoted(
            &parts
                .iter()
                .filter(|part| part["type"] == "text")
                .filter_map(|part| part["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt_trail::cut;

    const FIXTURE: &str = include_str!("../../tests/fixtures/session-record/pi.jsonl");

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
        id: &str,
        start: RecordStart,
        prompt: Option<&str>,
        end: TurnEnd,
        answer: Option<&str>,
    ) -> Shape {
        (
            id.into(),
            start,
            prompt.map(str::to_owned),
            end,
            answer.map(str::to_owned),
        )
    }

    #[test]
    fn pi_record_turns_quote_the_last_recorded_branch() {
        let turns = turns(FIXTURE.as_bytes()).unwrap();
        assert_eq!(
            turns.iter().map(shape).collect::<Vec<_>>(),
            vec![
                expect(
                    "u1",
                    RecordStart::Prompt,
                    Some("Explain   the relay\n\nin two lines."),
                    TurnEnd::Answered,
                    Some("The relay forwards frames.\nIt keeps one socket."),
                ),
                expect(
                    "u2",
                    RecordStart::Prompt,
                    Some("Check the build."),
                    TurnEnd::Answered,
                    Some("The build passes."),
                ),
                expect(
                    "w3",
                    RecordStart::Wake,
                    None,
                    TurnEnd::Answered,
                    Some("Still waiting on the job."),
                ),
                expect(
                    "u4",
                    RecordStart::Prompt,
                    Some("Rewrite the parser."),
                    TurnEnd::Aborted,
                    None,
                ),
                expect(
                    "u5",
                    RecordStart::Prompt,
                    Some("Try again."),
                    TurnEnd::Error,
                    None,
                ),
                expect(
                    "u6",
                    RecordStart::Prompt,
                    Some("Add a test."),
                    TurnEnd::Answered,
                    Some("Test added."),
                ),
                expect(
                    "u7",
                    RecordStart::Prompt,
                    Some("Ship it."),
                    TurnEnd::Answered,
                    Some("Shipped."),
                ),
                expect(
                    "u8",
                    RecordStart::Prompt,
                    Some("Watch the deploy."),
                    TurnEnd::Open,
                    None,
                ),
            ]
        );
        assert_eq!(turns[0].prompt_head.as_deref(), Some("Explain the relay"));
        assert_eq!(turns[0].at, 1_772_614_801);
    }

    #[test]
    fn pi_record_turns_never_quote_thinking_tools_errors_or_summaries() {
        let turns = turns(FIXTURE.as_bytes()).unwrap();
        let quoted: Vec<&str> = turns
            .iter()
            .flat_map(|turn| [turn.prompt.as_deref(), turn.answer.as_deref()])
            .flatten()
            .collect();
        for leaked in [
            "Neutral thought",
            "Interim note",
            "neutral tool output",
            "Neutral wake reminder",
            "Neutral hidden note",
            "Neutral abort detail",
            "Neutral error detail",
            "Partial words",
            "Neutral compaction summary",
            "abandoned",
            "Neutral system text",
        ] {
            assert!(
                quoted.iter().all(|text| !text.contains(leaked)),
                "{leaked} leaked into {quoted:?}"
            );
        }
    }

    /// The turn index is what the phone places pi turns by, so the turns
    /// quoted here must be the same turns, ended the same way, at every
    /// point the record can be caught mid-write.
    #[test]
    fn pi_record_turns_agree_with_the_turn_index_at_every_byte_cut() {
        let header = FIXTURE.find('\n').unwrap() + 1;
        let mut cuts = vec![FIXTURE.len()];
        let mut start = 0;
        for line in FIXTURE.split_inclusive('\n') {
            let end = start + line.len();
            cuts.extend([start + 1, start + line.len() / 2, end - 1, end]);
            start = end;
        }
        for cut_at in cuts.into_iter().filter(|cut_at| *cut_at >= header) {
            let source = &FIXTURE.as_bytes()[..cut_at];
            let turns = turns(source).unwrap_or_else(|error| panic!("cut {cut_at}: {error:?}"));
            let index = crate::turn_index::parse(source, "r".into()).unwrap();
            let indexed: Vec<_> = index
                .turns
                .iter()
                .map(|turn| {
                    (
                        turn.id.clone(),
                        turn.at,
                        turn.start == TurnStart::Wake,
                        turn.end,
                        turn.prompt.clone(),
                        turn.answer.clone(),
                    )
                })
                .collect();
            let read: Vec<_> = turns
                .iter()
                .map(|turn| {
                    let head = |text: &Option<String>| {
                        text.as_deref()
                            .and_then(|text| {
                                text.lines()
                                    .map(crate::prompt_trail::clean)
                                    .find(|line| !line.is_empty())
                            })
                            .map(|line| cut(&line, 160))
                    };
                    (
                        turn.id.clone(),
                        turn.at,
                        turn.start == RecordStart::Wake,
                        turn.end,
                        head(&turn.prompt),
                        head(&turn.answer),
                    )
                })
                .collect();
            assert_eq!(read, indexed, "cut {cut_at}");
            for turn in &turns {
                assert!(
                    turn.end == TurnEnd::Answered || turn.answer.is_none(),
                    "cut {cut_at}: an unanswered turn carries an answer"
                );
            }
        }
    }

    #[test]
    fn pi_record_turns_reopen_an_answer_a_tool_result_follows() {
        let source = [
            r#"{"type":"session","version":3,"id":"s"}"#,
            r#"{"type":"message","id":"u","parentId":null,"timestamp":"2026-03-04T09:00:00Z","message":{"role":"user","content":"Go"}}"#,
            r#"{"type":"message","id":"a","parentId":"u","timestamp":"2026-03-04T09:00:01Z","message":{"role":"assistant","content":[{"type":"text","text":"Early words"}],"stopReason":"stop"}}"#,
            r#"{"type":"message","id":"r","parentId":"a","timestamp":"2026-03-04T09:00:02Z","message":{"role":"toolResult","content":[{"type":"text","text":"late output"}]}}"#,
        ]
        .join("\n");
        let turns = turns(source.as_bytes()).unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].prompt.as_deref(), Some("Go"));
        assert_eq!(turns[0].end, TurnEnd::Open);
        assert_eq!(turns[0].answer, None);
    }

    #[test]
    fn pi_record_turns_refuse_what_the_turn_index_refuses() {
        assert_eq!(turns("".as_bytes()), Err(PromptTrailReason::Unreadable));
        assert_eq!(
            turns("{\"type\":\"message\",\"id\":\"x\"}\n".as_bytes()),
            Err(PromptTrailReason::Unreadable)
        );
        let undated = FIXTURE.replacen(
            "\"timestamp\":\"2026-03-04T09:00:01.000Z\"",
            "\"timestamp\":\"later\"",
            1,
        );
        assert_eq!(
            turns(undated.as_bytes()),
            Err(PromptTrailReason::Unreadable)
        );
    }
}
