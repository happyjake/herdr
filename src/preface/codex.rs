//! codex's rollout, read into turns.
//!
//! One turn per task the harness started, from its `task_started` to its
//! `task_complete` or `turn_aborted`. A turn's prompt is what the person
//! typed into it — the user messages left once the context codex injects is
//! filtered out, by the same rules the prompt trail reads with, and once the
//! person's replies to the agent's own questions are set aside. Its answer
//! is the assistant message in the `final_answer` phase, or failing that
//! the last agent message `task_complete` carries.
//!
//! A turn nobody prompted is kept only for what it answered: a sub-agent's
//! report or a goal's continuation is an answer-only `task` turn, and one
//! with neither a prompt nor an answer — a review pass — says nothing worth
//! quoting. A turn the next one began before it ended is left open.
//!
//! A compaction rewrites what the model sees, not what was said, so it is
//! stepped over; a rollback removes the turns it names, as codex itself
//! forgets them. `codex resume` keeps appending to the conversation's own
//! rollout, so one record holds every turn from before a resume and after.

use serde_json::Value;

use super::{body, RecordTurns, Turn};
use crate::api::schema::{TurnEnd, TurnStart};
use crate::prompt_trail::{
    clean, codex_injected, join_text_blocks, replace_image_tags, unix_seconds_from_rfc3339,
};

/// What codex draws at column 0 ahead of a prompt on its screen.
const PROMPT_MARKER: &str = "\u{203a}";

/// The person's reply to a question the agent asked mid-turn. It answers
/// the agent rather than starting anything, so it is no part of a prompt.
const QUESTION_REPLY: &str = "<send_user_message_question_reply";

#[derive(Default)]
pub(crate) struct CodexReader {
    session_id: Option<String>,
    turns: Vec<Turn>,
    open: Option<Building>,
    /// Lines seen, so a turn with no id of its own still has a stable one.
    lines: u64,
}

/// A turn while its lines are still being read.
struct Building {
    /// codex's own id for the turn, which its ending names too.
    turn_id: Option<String>,
    id: String,
    at: u64,
    prompts: Vec<String>,
    final_answer: Option<String>,
}

impl super::Reader for CodexReader {
    fn accept(&mut self, line: &Value) {
        self.lines += 1;
        let payload = &line["payload"];
        match (line["type"].as_str(), payload["type"].as_str()) {
            (Some("session_meta"), _) => {
                if self.session_id.is_none() {
                    self.session_id = payload["id"].as_str().map(str::to_owned);
                }
            }
            (Some("event_msg"), Some("task_started")) => {
                // A turn that never ended before the next began was cut off.
                if let Some(building) = self.open.take() {
                    self.close(building, TurnEnd::Open, None);
                }
                let turn_id = payload["turn_id"].as_str().map(str::to_owned);
                self.open = Some(Building {
                    id: turn_id
                        .clone()
                        .unwrap_or_else(|| format!("line-{}", self.lines)),
                    turn_id,
                    at: line_at(line)
                        .or_else(|| payload["started_at"].as_u64())
                        .unwrap_or(0),
                    prompts: Vec::new(),
                    final_answer: None,
                });
            }
            (Some("response_item"), Some("message")) => match payload["role"].as_str() {
                Some("user") => {
                    let Some(prompt) = typed_prompt(payload) else {
                        return;
                    };
                    let lines = self.lines;
                    self.open
                        .get_or_insert_with(|| Building {
                            turn_id: None,
                            id: format!("line-{lines}"),
                            at: line_at(line).unwrap_or(0),
                            prompts: Vec::new(),
                            final_answer: None,
                        })
                        .prompts
                        .push(prompt);
                }
                Some("assistant") if payload["phase"] == "final_answer" => {
                    if let Some(building) = self.open.as_mut() {
                        let blocks = payload["content"].as_array().map(Vec::as_slice);
                        building.final_answer =
                            Some(join_text_blocks(blocks.unwrap_or_default(), "output_text"));
                    }
                }
                _ => {}
            },
            (Some("event_msg"), Some("task_complete")) => {
                if let Some(building) = self.take_matching(payload) {
                    let answer = building
                        .final_answer
                        .clone()
                        .filter(|answer| !body(answer).is_empty())
                        .or_else(|| payload["last_agent_message"].as_str().map(str::to_owned));
                    self.close(building, TurnEnd::Answered, answer);
                }
            }
            (Some("event_msg"), Some("turn_aborted")) => {
                if let Some(building) = self.take_matching(payload) {
                    self.close(building, TurnEnd::Aborted, None);
                }
            }
            (Some("event_msg"), Some("thread_rolled_back")) => {
                self.roll_back(payload["num_turns"].as_u64().unwrap_or(0));
            }
            // The summary a compaction leaves, and the history it replays,
            // are the model's working copy; the turns they stand for are
            // already in this record as they were said.
            (Some("compacted"), _) => {}
            _ => {}
        }
    }

    /// codex marks the first row of a prompt with its prompt glyph at
    /// column 0, its own messages with a bullet.
    fn prompt_markers(&self) -> &'static [&'static str] {
        &[PROMPT_MARKER]
    }

    fn finish(mut self: Box<Self>) -> RecordTurns {
        if let Some(building) = self.open.take() {
            self.close(building, TurnEnd::Open, None);
        }
        RecordTurns {
            session_id: self.session_id,
            turns: self.turns,
        }
    }
}

impl CodexReader {
    /// The open turn, when this ending is its own. An ending naming some
    /// other turn leaves it running.
    fn take_matching(&mut self, payload: &Value) -> Option<Building> {
        let open_id = self.open.as_ref()?.turn_id.as_deref();
        match (open_id, payload["turn_id"].as_str()) {
            (Some(open_id), Some(ends)) if open_id != ends => None,
            _ => self.open.take(),
        }
    }

    fn close(&mut self, building: Building, end: TurnEnd, answer: Option<String>) {
        let answer = answer.filter(|answer| end == TurnEnd::Answered && !body(answer).is_empty());
        let (start, prompt) = if building.prompts.is_empty() {
            if answer.is_none() {
                return;
            }
            (TurnStart::Task, None)
        } else {
            (TurnStart::Prompt, Some(building.prompts.join("\n\n")))
        };
        let mut turn = Turn::started(building.id, building.at, start, prompt);
        match (end, answer) {
            (TurnEnd::Answered, Some(answer)) => turn.answer(&answer),
            (TurnEnd::Answered, None) => turn.answer(""),
            (end, _) => turn.end_without_answer(end),
        }
        self.turns.push(turn);
    }

    /// Forget the newest `count` prompted turns, and whatever followed them.
    fn roll_back(&mut self, mut count: u64) {
        while count > 0 {
            let Some(turn) = self.turns.pop() else {
                return;
            };
            if turn.start == TurnStart::Prompt {
                count -= 1;
            }
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
    let prompt = replace_image_tags(&raw);
    (!body(&prompt).is_empty()).then_some(prompt)
}

fn line_at(line: &Value) -> Option<u64> {
    line["timestamp"]
        .as_str()
        .and_then(unix_seconds_from_rfc3339)
}

#[cfg(test)]
mod tests {
    use super::super::{read_turns, read_turns_bounded};
    use super::*;
    use crate::api::schema::{PromptTrailAgent, PromptTrailReason};

    const FIXTURE: &str = include_str!("fixtures/codex-record.jsonl");

    fn turns_of(record: &str) -> Vec<Turn> {
        read_turns(record.as_bytes(), PromptTrailAgent::Codex)
            .unwrap()
            .turns
    }

    fn turn_id(n: u32) -> String {
        format!("00000000-0000-7000-8000-00000000000{n}")
    }

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
        n: u32,
        start: TurnStart,
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
    fn a_codex_rollout_reads_into_what_was_typed_and_what_was_concluded() {
        let read = read_turns(FIXTURE.as_bytes(), PromptTrailAgent::Codex).unwrap();
        assert_eq!(read.session_id.as_deref(), Some(turn_id(0).as_str()));
        let turns = read.turns;
        assert_eq!(
            turns.iter().map(shape).collect::<Vec<_>>(),
            vec![
                expect(
                    1,
                    TurnStart::Prompt,
                    Some("Summarize   the layout module\n\nand list its public functions."),
                    TurnEnd::Answered,
                    Some(
                        "The layout module has two parts.\n\n| name | role |\n| --- | --- |\n| split | divides space |"
                    ),
                ),
                expect(
                    2,
                    TurnStart::Prompt,
                    Some("Compare these:\n[image]\nwhich one is wider?"),
                    TurnEnd::Answered,
                    Some("The first one is wider."),
                ),
                expect(
                    3,
                    TurnStart::Prompt,
                    Some("Rename every helper."),
                    TurnEnd::Aborted,
                    None,
                ),
                expect(
                    4,
                    TurnStart::Prompt,
                    Some("Only rename the private ones."),
                    TurnEnd::Answered,
                    Some("Renamed the private helpers."),
                ),
                expect(
                    6,
                    TurnStart::Task,
                    None,
                    TurnEnd::Answered,
                    Some("Goal step finished."),
                ),
                expect(
                    8,
                    TurnStart::Prompt,
                    Some("Continue with the tests."),
                    TurnEnd::Answered,
                    Some("Tests added."),
                ),
                expect(
                    9,
                    TurnStart::Prompt,
                    Some("Run the full check."),
                    TurnEnd::Open,
                    None,
                ),
            ]
        );
        assert_eq!(
            turns[0].head.as_deref(),
            Some("Summarize the layout module")
        );
        // 2026-03-04T09:00:01Z, the turn's start, not its first prompt's.
        assert_eq!(turns[0].at, 1_772_614_801);
        // Resumed the next day into the same rollout.
        assert_eq!(turns[5].at, 1_772_704_801);
    }

    #[test]
    fn a_codex_preface_never_quotes_injected_context_or_compaction() {
        let turns = turns_of(FIXTURE);
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
    fn a_codex_rollout_caught_mid_write_reads_into_the_turns_written_so_far() {
        let full = turns_of(FIXTURE);
        let rolled_back = turn_id(7);
        let first_line = FIXTURE.find('\n').unwrap() + 1;
        let mut cuts = vec![FIXTURE.len()];
        let mut start = 0;
        for line in FIXTURE.split_inclusive('\n') {
            let end = start + line.len();
            cuts.extend([start + 1, start + line.len() / 2, end - 1, end]);
            start = end;
        }
        // A record with nothing but a half-written first line is no record
        // yet, as for every harness.
        for cut in cuts.into_iter().filter(|cut| *cut >= first_line) {
            let turns = read_turns(&FIXTURE.as_bytes()[..cut], PromptTrailAgent::Codex)
                .unwrap_or_else(|error| panic!("cut {cut}: {error:?}"))
                .turns;
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
    fn a_codex_ending_ends_only_the_turn_it_names() {
        let record = [
            r#"{"timestamp":"2026-03-04T09:00:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":"mine"}}"#,
            r#"{"timestamp":"2026-03-04T09:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Still going"}]}}"#,
            r#"{"timestamp":"2026-03-04T09:00:02Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"another","last_agent_message":"Not mine"}}"#,
        ]
        .join("\n");
        let turns = turns_of(&record);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].end, TurnEnd::Open);
        assert_eq!(turns[0].answer, None);
    }

    #[test]
    fn a_codex_turn_the_next_one_cut_off_is_kept_unfinished() {
        let record = [
            r#"{"timestamp":"2026-03-04T09:00:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":"first"}}"#,
            r#"{"timestamp":"2026-03-04T09:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Left running"}]}}"#,
            r#"{"timestamp":"2026-03-04T09:00:02Z","type":"event_msg","payload":{"type":"task_started","turn_id":"second"}}"#,
            r#"{"timestamp":"2026-03-04T09:00:03Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Next"}]}}"#,
            r#"{"timestamp":"2026-03-04T09:00:04Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"second","last_agent_message":"Done"}}"#,
        ]
        .join("\n");
        assert_eq!(
            turns_of(&record)
                .iter()
                .map(|turn| (turn.id.as_str(), turn.end))
                .collect::<Vec<_>>(),
            vec![("first", TurnEnd::Open), ("second", TurnEnd::Answered)]
        );
    }

    #[test]
    fn a_codex_rollout_past_its_bounds_is_refused() {
        assert_eq!(
            read_turns_bounded(FIXTURE.as_bytes(), PromptTrailAgent::Codex, 1024, 1 << 20),
            Err(PromptTrailReason::Unreadable)
        );
        assert_eq!(
            read_turns(&b"not json\nstill not json\n"[..], PromptTrailAgent::Codex),
            Err(PromptTrailReason::Unreadable)
        );
        assert_eq!(turns_of(""), Vec::new());
    }
}
