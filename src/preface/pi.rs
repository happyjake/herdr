//! pi's record, read into turns.
//!
//! The turns are those on the record's last-recorded branch, walked exactly
//! as the turn index walks it and started and ended by the turn index's own
//! rules, so a preface and a turn index never disagree about which turns a
//! pi conversation holds: a person's message starts a turn, and so does a
//! wake the harness displayed, as an answer-only `wake` turn. A turn's last
//! assistant message decides how it ended — only a `stop` with no tool call
//! answers it — and a tool result after it reopens it. Thinking, tool
//! traffic, hidden messages, error text and compaction summaries are never
//! quoted.
//!
//! A record whose entries do not form a branch the turn index can walk has
//! no turns here: a preface placed on a guessed branch would quote turns the
//! conversation does not hold.
//!
//! pi sets a prompt apart on its screen by a background colour alone, with
//! no glyph a screen's text can show, so this reader declares that colour
//! instead of a marker: the dark theme's prompt background, the one the
//! phone's pi shapes key a prompt on (`isPiPrompt`, `#213b49`). Under any
//! other theme no row carries it, and the seam is not found rather than
//! guessed. A screen that holds only wake blocks has no prompt row to place
//! the seam on either, and answers `seam_not_found` too.

use serde_json::Value;

use super::{body, RecordTurns, Turn};
use crate::api::schema::{PromptTrailReason, TurnEnd, TurnStart};
use crate::turn_index::{assistant_end, start_at, turn_start, BranchWalk};

enum Step {
    Start(Turn),
    Assistant {
        end: TurnEnd,
        answer: Option<String>,
    },
    ToolResult,
    Other,
}

#[derive(Default)]
pub(crate) struct PiReader {
    walk: BranchWalk<Step>,
    broken: bool,
}

/// pi's dark-theme prompt background, `#213b49`; the phone's pi shapes
/// key a prompt on the same colour.
const PROMPT_BACKGROUND: [u8; 3] = [0x21, 0x3b, 0x49];

impl super::Reader for PiReader {
    fn prompt_background(&self) -> Option<[u8; 3]> {
        Some(PROMPT_BACKGROUND)
    }

    fn accept(&mut self, line: &Value) {
        if !self.broken && self.walk.accept(line, step).is_err() {
            self.broken = true;
        }
    }

    fn finish(self: Box<Self>) -> RecordTurns {
        if self.broken {
            return RecordTurns::default();
        }
        // A half-written last line never reaches a reader, so the branch
        // ends at the last whole entry.
        let Ok(branch) = self.walk.finish(false) else {
            return RecordTurns::default();
        };
        let mut turns: Vec<Turn> = Vec::new();
        for entry in branch.entries {
            match entry {
                Step::Start(turn) => turns.push(turn),
                Step::Assistant { end, answer } => {
                    if let Some(turn) = turns.last_mut() {
                        match end {
                            TurnEnd::Answered => turn.answer(answer.as_deref().unwrap_or("")),
                            end => turn.end_without_answer(end),
                        }
                    }
                }
                Step::ToolResult => {
                    if let Some(turn) = turns.last_mut() {
                        turn.end_without_answer(TurnEnd::Open);
                    }
                }
                Step::Other => {}
            }
        }
        RecordTurns {
            session_id: Some(branch.session_id),
            turns,
        }
    }
}

fn step(value: &Value, id: &str) -> Result<Step, PromptTrailReason> {
    let message = &value["message"];
    if let Some(start) = turn_start(value) {
        let at = start_at(value)?;
        // A wake's own text is the harness's reminder, not a prompt.
        let prompt = match start {
            TurnStart::Prompt => text(&message["content"]).map(|text| typed(&text)),
            _ => None,
        };
        return Ok(Step::Start(Turn::started(id.into(), at, start, prompt)));
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

/// The text parts of a message, and nothing else of it: never its thinking,
/// its tool calls or its images. Nothing when there are no words.
fn text(content: &Value) -> Option<String> {
    let text = match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|part| part["type"] == "text")
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    (!body(&text).is_empty()).then_some(text)
}

/// What the person typed into a message. Invoking a skill makes pi write
/// the skill's whole body into the message ahead of the person's request;
/// that body is the harness's instructions, so only the invocation and the
/// request are kept, as `/skill:name request`.
pub(crate) fn typed(text: &str) -> String {
    match skill_block(text) {
        Some((name, Some(request))) => format!("/skill:{name} {request}"),
        Some((name, None)) => format!("/skill:{name}"),
        None => text.to_string(),
    }
}

/// A skill block's name and the request after it, read as pi's own
/// `parseSkillBlock` reads one: the block is the whole message, or the
/// whole message up to a blank line and a request. Its body ends at the
/// first closing tag that leaves exactly that.
fn skill_block(text: &str) -> Option<(&str, Option<&str>)> {
    const CLOSE: &str = "\n</skill>";
    let (name, rest) = text.strip_prefix("<skill name=\"")?.split_once('"')?;
    let (location, rest) = rest.strip_prefix(" location=\"")?.split_once('"')?;
    if name.is_empty() || location.is_empty() {
        return None;
    }
    let body = rest.strip_prefix(">\n")?;
    let mut from = 0;
    loop {
        let close = from + body[from..].find(CLOSE)?;
        let after = &body[close + CLOSE.len()..];
        if after.is_empty() {
            return Some((name, None));
        }
        if let Some(request) = after
            .strip_prefix("\n\n")
            .filter(|request| !request.is_empty())
        {
            let request = request.trim();
            return Some((name, (!request.is_empty()).then_some(request)));
        }
        from = close + 1;
    }
}

#[cfg(test)]
#[cfg(test)]
mod tests {
    use super::super::read_turns;
    use super::*;
    use crate::api::schema::PromptTrailAgent;
    use crate::prompt_trail::cut;

    fn turns(record: &[u8]) -> Result<Vec<Turn>, PromptTrailReason> {
        read_turns(record, PromptTrailAgent::Pi).map(|read| read.turns)
    }

    const FIXTURE: &str = include_str!("fixtures/pi-record.jsonl");

    type Shape = (String, TurnStart, Option<String>, TurnEnd, Option<String>);

    fn shape(turn: &Turn) -> Shape {
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
        start: TurnStart,
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
                    TurnStart::Prompt,
                    Some("Explain   the relay\n\nin two lines."),
                    TurnEnd::Answered,
                    Some("The relay forwards frames.\nIt keeps one socket."),
                ),
                expect(
                    "u2",
                    TurnStart::Prompt,
                    Some("Check the build."),
                    TurnEnd::Answered,
                    Some("The build passes."),
                ),
                expect(
                    "w3",
                    TurnStart::Wake,
                    None,
                    TurnEnd::Answered,
                    Some("Still waiting on the job."),
                ),
                expect(
                    "u4",
                    TurnStart::Prompt,
                    Some("Rewrite the parser."),
                    TurnEnd::Aborted,
                    None,
                ),
                expect(
                    "u5",
                    TurnStart::Prompt,
                    Some("Try again."),
                    TurnEnd::Error,
                    None,
                ),
                expect(
                    "u6",
                    TurnStart::Prompt,
                    Some("Add a test."),
                    TurnEnd::Answered,
                    Some("Test added."),
                ),
                expect(
                    "u7",
                    TurnStart::Prompt,
                    Some("Ship it."),
                    TurnEnd::Answered,
                    Some("Shipped."),
                ),
                expect(
                    "u9",
                    TurnStart::Prompt,
                    Some("/skill:neutral-skill Draw the module map."),
                    TurnEnd::Answered,
                    Some("Map drawn."),
                ),
                expect(
                    "u8",
                    TurnStart::Prompt,
                    Some("Watch the deploy."),
                    TurnEnd::Open,
                    None,
                ),
            ]
        );
        assert_eq!(turns[0].head.as_deref(), Some("Explain the relay"));
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
            "Injected skill body",
            "References are relative",
            "SKILL.md",
            "<skill",
        ] {
            assert!(
                quoted.iter().all(|text| !text.contains(leaked)),
                "{leaked} leaked into {quoted:?}"
            );
        }
    }

    /// The turn index is what the phone places pi turns by, so the turns
    /// quoted here must be the same turns, ended the same way, at every
    /// whole line the record can be caught at. A half-written last line is
    /// no line a reader sees: the turns are those of the lines before it.
    #[test]
    fn pi_record_turns_agree_with_the_turn_index_at_every_byte_cut() {
        let header = FIXTURE.find('\n').unwrap() + 1;
        let index_skill_head = Some(
            r#"<skill name="neutral-skill" location="/skills/neutral-skill/SKILL.md">"#.to_string(),
        );
        let mut whole_lines = Vec::new();
        let mut start = 0;
        for line in FIXTURE.split_inclusive('\n') {
            start += line.len();
            if start >= header {
                whole_lines.push(start);
            }
        }
        let mut previous: Option<Vec<Turn>> = None;
        let mut line_start = header;
        for end in whole_lines {
            // Short of its newline a line is still whole, and is read as
            // the whole-line cut below is.
            for cut_at in [line_start + 1, (line_start + end) / 2] {
                if cut_at <= line_start || cut_at >= end - 1 {
                    continue;
                }
                let turns = turns(&FIXTURE.as_bytes()[..cut_at])
                    .unwrap_or_else(|error| panic!("cut {cut_at}: {error:?}"));
                if let Some(previous) = &previous {
                    assert_eq!(&turns, previous, "cut {cut_at}");
                }
            }
            line_start = end;
            let source = &FIXTURE.as_bytes()[..end];
            let whole = turns(source).unwrap_or_else(|error| panic!("cut {end}: {error:?}"));
            assert_eq!(
                turns(&FIXTURE.as_bytes()[..end - 1]).as_ref(),
                Ok(&whole),
                "cut {}",
                end - 1
            );
            let index = crate::turn_index::parse(source, "r".into()).unwrap();
            let indexed: Vec<_> = index
                .turns
                .iter()
                .map(|turn| {
                    (
                        turn.id.clone(),
                        turn.at,
                        turn.start,
                        turn.end,
                        turn.prompt.clone(),
                        turn.answer.clone(),
                    )
                })
                .collect();
            let read: Vec<_> = whole
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
                    // The index heads a skill turn by its raw block; this
                    // reader quotes the invocation instead.
                    let prompt = if turn.id == "u9" {
                        index_skill_head.clone()
                    } else {
                        head(&turn.prompt)
                    };
                    (
                        turn.id.clone(),
                        turn.at,
                        turn.start,
                        turn.end,
                        prompt,
                        head(&turn.answer),
                    )
                })
                .collect();
            assert_eq!(read, indexed, "cut {end}");
            for turn in &whole {
                assert!(
                    turn.end == TurnEnd::Answered || turn.answer.is_none(),
                    "cut {end}: an unanswered turn carries an answer"
                );
            }
            previous = Some(whole);
        }
    }

    #[test]
    fn pi_record_turns_read_skill_blocks_as_pi_parses_them() {
        let block = "<skill name=\"s\" location=\"/x\">\nBody\n</skill>";
        assert_eq!(typed(block), "/skill:s");
        assert_eq!(
            typed(&format!("{block}\n\n  Ask this\nand that \n")),
            "/skill:s Ask this\nand that"
        );
        assert_eq!(typed(&format!("{block}\n\n   ")), "/skill:s");
        // The body runs to the first closing tag that leaves the rest of the
        // message a request; a closing tag inside the body is body.
        assert_eq!(
            typed("<skill name=\"s\" location=\"/x\">\nA\n</skill>\nB\n</skill>\n\nAsk"),
            "/skill:s Ask"
        );
        // Anything else is the person's own text, left as typed.
        for own in [
            format!("Please {block}"),
            format!("{block}\nno blank line"),
            "<skill name=\"\" location=\"/x\">\nBody\n</skill>".to_string(),
            "<skill name=\"s\">\nBody\n</skill>".to_string(),
            "<skill name=\"s\" location=\"/x\">\nunclosed".to_string(),
        ] {
            assert_eq!(typed(&own), own);
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
    fn a_pi_record_the_turn_index_refuses_has_no_turns() {
        let undated = FIXTURE.replacen(
            "\"timestamp\":\"2026-03-04T09:00:01.000Z\"",
            "\"timestamp\":\"later\"",
            1,
        );
        let no_header = FIXTURE.split_once('\n').unwrap().1.to_string();
        let orphaned = FIXTURE.replacen("\"parentId\":\"a7\"", "\"parentId\":\"nowhere\"", 1);
        for record in [undated, no_header, orphaned] {
            assert!(crate::turn_index::parse(record.as_bytes(), "r".into()).is_err());
            assert_eq!(turns(record.as_bytes()), Ok(Vec::new()));
        }
        assert_eq!(turns(b""), Ok(Vec::new()));
    }
}
