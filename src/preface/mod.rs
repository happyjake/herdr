//! Prefaces — a pane's earlier turns, quoted from its session record.
//!
//! A pane's screen forgets: a byte cap drops its oldest rows, a handoff cuts
//! it to its tail, an agent clears its own scrollback. Its session record
//! does not. A preface is the turns the record holds from before the oldest
//! prompt a client still holds on screen, each one the prompt and its final
//! answer, quoted rather than summarized.
//!
//! Three parts, each reused by every harness:
//!
//! - **A reader per harness** turns one record into a list of turns in a
//!   common shape ([`Turn`]): how the turn started, its prompt, how it
//!   ended, and its final answer. Each reader owns its harness's rules for
//!   what is not a prompt and what the final answer is; nothing else here
//!   knows a harness.
//! - **The seam** ([`seam`]) places the client's screen against the record
//!   and picks the turns the screen no longer holds, within the bounds a
//!   request allows.
//! - **The read** walks the record under the prompt trail's bounds and
//!   keeps the turns it found by record fingerprint, so a reopened pane
//!   whose record has not moved is not read again.
//!
//! Bodies live only in the answer and in this process's memory. Nothing
//! here logs a prompt or an answer.

pub(crate) mod agy;
pub(crate) mod claude;
pub(crate) mod codex;
pub(crate) mod pi;
pub(crate) mod seam;

use std::collections::VecDeque;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::Value;

use crate::api::schema::{PromptTrailAgent, PromptTrailReason, TurnEnd, TurnStart};
use crate::prompt_trail::{read_line_bounded, LineRead, MAX_LINE_BYTES, MAX_RECORD_BYTES};

/// Bytes a prompt is cut to. Past it the client still has the prompt's
/// opening, which is all it shows of one.
pub(crate) const PROMPT_BYTES: usize = 4 * 1024;

/// Bytes an answer is cut to.
pub(crate) const ANSWER_BYTES: usize = 16 * 1024;

/// One turn as a reader finds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Turn {
    /// The record's own id for the line that started the turn.
    pub id: String,
    /// Unix seconds the turn started at.
    pub at: u64,
    pub start: TurnStart,
    /// The prompt, cut to [`PROMPT_BYTES`]; none for a wake or task turn.
    pub prompt: Option<String>,
    pub prompt_cut: bool,
    /// The prompt's first line as one row of text, taken before any cut:
    /// what the seam looks for on the screen.
    pub head: Option<String>,
    pub end: TurnEnd,
    /// The final answer, cut to [`ANSWER_BYTES`]; none unless answered.
    pub answer: Option<String>,
    pub answer_cut: bool,
}

impl Turn {
    /// A turn that has only started.
    pub(crate) fn started(id: String, at: u64, start: TurnStart, prompt: Option<String>) -> Self {
        let prompt = prompt.map(|prompt| body(&prompt));
        let head = prompt.as_deref().and_then(first_line);
        let (prompt, prompt_cut) = match prompt {
            Some(prompt) => {
                let (prompt, cut) = capped(prompt, PROMPT_BYTES);
                (Some(prompt), cut)
            }
            None => (None, false),
        };
        Self {
            id,
            at,
            start,
            prompt,
            prompt_cut,
            head,
            end: TurnEnd::Open,
            answer: None,
            answer_cut: false,
        }
    }

    /// End the turn with a final answer. An answer with no words in it is
    /// still an answered turn, just one with nothing to quote.
    pub(crate) fn answer(&mut self, text: &str) {
        let text = body(text);
        self.end = TurnEnd::Answered;
        if text.is_empty() {
            self.answer = None;
            self.answer_cut = false;
        } else {
            let (answer, cut) = capped(text, ANSWER_BYTES);
            self.answer = Some(answer);
            self.answer_cut = cut;
        }
    }

    /// End the turn some other way, or reopen it.
    pub(crate) fn end_without_answer(&mut self, end: TurnEnd) {
        self.end = end;
        self.answer = None;
        self.answer_cut = false;
    }

    fn held_bytes(&self) -> usize {
        self.id.len()
            + self.prompt.as_ref().map_or(0, String::len)
            + self.head.as_ref().map_or(0, String::len)
            + self.answer.as_ref().map_or(0, String::len)
    }
}

/// Every turn one record holds, oldest first.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct RecordTurns {
    pub session_id: Option<String>,
    pub turns: Vec<Turn>,
}

/// One harness's reading of its own record, a line at a time.
pub(crate) trait Reader {
    fn accept(&mut self, line: &Value);
    fn finish(self: Box<Self>) -> RecordTurns;

    /// What the harness draws at column 0 ahead of the first row of a
    /// prompt on its screen — the evidence that a screen row is a prompt
    /// rather than an answer or tool output repeating its words.
    ///
    /// A harness that declares none cannot have its screen lined up with
    /// its record, and its prefaces answer `seam_not_found`.
    fn prompt_markers(&self) -> &'static [&'static str] {
        &[]
    }

    /// The background colour, as RGB, a harness lays under every visible
    /// cell of a prompt on its screen, for a harness that sets a prompt
    /// apart by colour rather than by a marker. A screen is then read with
    /// its styling, and a prompt row is the first text row of a block in
    /// that colour.
    fn prompt_background(&self) -> Option<[u8; 3]> {
        None
    }
}

/// The reader for a harness, when this server has one.
pub(crate) fn reader_for(agent: PromptTrailAgent) -> Option<Box<dyn Reader>> {
    match agent {
        PromptTrailAgent::Claude => Some(Box::new(claude::ClaudeReader::default())),
        PromptTrailAgent::Codex => Some(Box::new(codex::CodexReader::default())),
        PromptTrailAgent::Pi => Some(Box::new(pi::PiReader::default())),
        PromptTrailAgent::Agy => Some(Box::new(agy::AgyReader::default())),
    }
}

/// The name a harness goes by on the wire.
pub(crate) fn agent_name(agent: PromptTrailAgent) -> &'static str {
    match agent {
        PromptTrailAgent::Claude => "claude",
        PromptTrailAgent::Codex => "codex",
        PromptTrailAgent::Pi => "pi",
        PromptTrailAgent::Agy => "agy",
    }
}

/// Walk one record through its harness's reader, under the prompt trail's
/// bounds: a line past its cap or a record past its size refuses the whole
/// record, a line that is no JSON is stepped over the way a half-written
/// last line has to be, and a file with nothing but such lines is no record
/// this can read.
pub(crate) fn read_turns(
    reader: impl BufRead,
    agent: PromptTrailAgent,
) -> Result<RecordTurns, PromptTrailReason> {
    read_turns_bounded(reader, agent, MAX_RECORD_BYTES, MAX_LINE_BYTES)
}

fn read_turns_bounded(
    mut reader: impl BufRead,
    agent: PromptTrailAgent,
    record_bytes: u64,
    line_bytes: usize,
) -> Result<RecordTurns, PromptTrailReason> {
    let mut turns = reader_for(agent).ok_or(PromptTrailReason::UnsupportedAgent)?;
    let mut consumed = 0;
    let mut line = Vec::new();
    let mut lines_read = 0u64;
    let mut lines_parsed = 0u64;
    loop {
        match read_line_bounded(&mut reader, &mut line, &mut consumed, line_bytes) {
            Ok(LineRead::End) => break,
            Ok(LineRead::Line) if consumed <= record_bytes => {}
            _ => return Err(PromptTrailReason::Unreadable),
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        lines_read += 1;
        let Ok(parsed) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        lines_parsed += 1;
        turns.accept(&parsed);
    }
    if lines_read > 0 && lines_parsed == 0 {
        return Err(PromptTrailReason::Unreadable);
    }
    Ok(turns.finish())
}

/// A body as text a client can lay out: line endings made one kind,
/// control characters other than line breaks and tabs dropped, and the
/// blank edges trimmed.
pub(crate) fn body(text: &str) -> String {
    let text = text.replace("\r\n", "\n");
    text.chars()
        .filter(|character| matches!(character, '\n' | '\t') || !character.is_control())
        .collect::<String>()
        .trim()
        .to_string()
}

/// The first line with anything on it, as one row of text.
fn first_line(text: &str) -> Option<String> {
    text.lines()
        .map(crate::prompt_trail::clean)
        .find(|line| !line.is_empty())
}

/// Cut a body to a number of bytes, at a line break when one falls in the
/// back half of what is kept and at a character boundary otherwise.
pub(crate) fn capped(text: String, limit: usize) -> (String, bool) {
    if text.len() <= limit {
        return (text, false);
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let end = match text[..end].rfind('\n') {
        Some(index) if index * 2 >= limit => index,
        _ => end,
    };
    (text[..end].trim_end().to_string(), true)
}

/// Records whose turns are kept at once.
const CACHE_RECORDS: usize = 4;

/// Bytes of kept turns across every cached record. A record whose turns
/// alone pass it is read every time rather than crowding out the rest.
const CACHE_BYTES: usize = 32 * 1024 * 1024;

struct Cached {
    path: PathBuf,
    fingerprint: crate::turn_index::Fingerprint,
    revision: String,
    turns: std::sync::Arc<RecordTurns>,
    bytes: usize,
}

/// The turns one record holds and the revision they were read at.
#[derive(Debug, Clone)]
pub(crate) struct ReadTurns {
    pub revision: String,
    pub turns: std::sync::Arc<RecordTurns>,
}

#[derive(Default)]
pub(crate) struct Cache {
    entries: Mutex<VecDeque<Cached>>,
    #[cfg(test)]
    pub(crate) parses: std::sync::atomic::AtomicUsize,
}

impl Cache {
    /// The turns of one record, read again only when its fingerprint moved.
    pub(crate) fn read(
        &self,
        path: &Path,
        agent: PromptTrailAgent,
    ) -> Result<ReadTurns, PromptTrailReason> {
        let path = path
            .canonicalize()
            .map_err(|_| PromptTrailReason::NoRecord)?;
        let fingerprint = crate::turn_index::Fingerprint::at(&path)?;
        if let Ok(mut entries) = self.entries.lock() {
            if let Some(position) = entries.iter().position(|entry| entry.path == path) {
                if let Some(cached) = entries.remove(position) {
                    if cached.fingerprint == fingerprint {
                        let found = ReadTurns {
                            revision: cached.revision.clone(),
                            turns: std::sync::Arc::clone(&cached.turns),
                        };
                        entries.push_back(cached);
                        return Ok(found);
                    }
                }
            }
        }

        let file = std::fs::File::open(&path).map_err(|_| PromptTrailReason::Unreadable)?;
        #[cfg(test)]
        self.parses
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let turns = std::sync::Arc::new(read_turns(std::io::BufReader::new(file), agent)?);
        let revision = crate::turn_index::revision(&path, &fingerprint);
        let bytes = turns.turns.iter().map(Turn::held_bytes).sum::<usize>();
        // A record that moved while it was read is answered, but not kept
        // under a fingerprint it no longer has.
        if bytes <= CACHE_BYTES / 2
            && crate::turn_index::Fingerprint::at(&path).as_ref() == Ok(&fingerprint)
        {
            if let Ok(mut entries) = self.entries.lock() {
                entries.push_back(Cached {
                    path,
                    fingerprint,
                    revision: revision.clone(),
                    turns: std::sync::Arc::clone(&turns),
                    bytes,
                });
                while entries.len() > CACHE_RECORDS
                    || entries.iter().map(|entry| entry.bytes).sum::<usize>() > CACHE_BYTES
                {
                    entries.pop_front();
                }
            }
        }
        Ok(ReadTurns { revision, turns })
    }
}

/// The turns of one record, through the process's one cache.
pub(crate) fn read(path: &Path, agent: PromptTrailAgent) -> Result<ReadTurns, PromptTrailReason> {
    static CACHE: std::sync::OnceLock<Cache> = std::sync::OnceLock::new();
    CACHE.get_or_init(Cache::default).read(path, agent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_body_keeps_its_lines_and_loses_its_control_characters() {
        assert_eq!(
            body("\r\n  first line\r\n\tsecond\u{7} line\u{1b}[0m\n\n"),
            "first line\n\tsecond line[0m"
        );
    }

    #[test]
    fn a_long_body_is_cut_at_a_line_break_in_its_back_half_or_a_character() {
        let lines = "one line of words\n".repeat(10);
        let (cut, was_cut) = capped(lines.clone(), 100);
        assert!(was_cut);
        assert!(cut.len() <= 100);
        assert!(lines.starts_with(&cut));
        assert!(cut.ends_with("words"), "cut at a line break: {cut:?}");

        // One unbroken run is cut where the cap falls, never inside a
        // character.
        let (cut, was_cut) = capped("é".repeat(60), 101);
        assert!(was_cut);
        assert_eq!(cut, "é".repeat(50));

        let (whole, was_cut) = capped("short".into(), 100);
        assert_eq!((whole.as_str(), was_cut), ("short", false));
    }

    #[test]
    fn every_harness_with_a_trail_has_a_reader() {
        for agent in [
            PromptTrailAgent::Claude,
            PromptTrailAgent::Codex,
            PromptTrailAgent::Pi,
        ] {
            assert!(reader_for(agent).is_some(), "{agent:?}");
        }
    }

    #[test]
    fn the_record_bounds_refuse_an_oversized_line_or_record() {
        let line = "{\"type\":\"user\"}\n";
        assert!(read_turns_bounded(line.as_bytes(), PromptTrailAgent::Claude, 1024, 1024).is_ok());
        assert_eq!(
            read_turns_bounded(line.as_bytes(), PromptTrailAgent::Claude, 1024, 4),
            Err(PromptTrailReason::Unreadable)
        );
        assert_eq!(
            read_turns_bounded(
                line.repeat(4).as_bytes(),
                PromptTrailAgent::Claude,
                32,
                1024
            ),
            Err(PromptTrailReason::Unreadable)
        );
        assert_eq!(
            read_turns(&b"not json\nnor this\n"[..], PromptTrailAgent::Claude),
            Err(PromptTrailReason::Unreadable)
        );
    }

    #[test]
    fn a_record_whose_fingerprint_has_not_moved_is_not_read_again() {
        use std::io::Write as _;
        use std::sync::atomic::Ordering;
        let home = crate::prompt_trail::tests::FixtureHome::new();
        let line = |text: &str, at: &str| {
            serde_json::json!({
                "type": "user", "uuid": at, "timestamp": at,
                "message": { "role": "user", "content": text },
            })
            .to_string()
                + "\n"
        };
        let path = home.write(
            "records/claude.jsonl",
            &line("map the piers", "2026-03-04T09:00:00Z"),
        );
        let cache = Cache::default();
        let first = cache.read(&path, PromptTrailAgent::Claude).unwrap();
        let again = cache.read(&path, PromptTrailAgent::Claude).unwrap();
        assert_eq!(cache.parses.load(Ordering::SeqCst), 1);
        assert_eq!(first.revision, again.revision);
        assert!(std::sync::Arc::ptr_eq(&first.turns, &again.turns));

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(line("and the harbour", "2026-03-04T09:01:00Z").as_bytes())
            .unwrap();
        drop(file);
        let grown = cache.read(&path, PromptTrailAgent::Claude).unwrap();
        assert_eq!(cache.parses.load(Ordering::SeqCst), 2);
        assert_ne!(grown.revision, first.revision);
        assert_eq!(grown.turns.turns.len(), 2);

        assert_eq!(
            cache
                .read(
                    &home.path().join("records/gone.jsonl"),
                    PromptTrailAgent::Claude
                )
                .map(|read| read.revision),
            Err(PromptTrailReason::NoRecord)
        );
    }

    /// The cold and warm cost of one real record, named by
    /// `HERDR_PREFACE_BENCH_RECORD`. Run by hand, in a release build:
    /// `HERDR_PREFACE_BENCH_RECORD=<record> cargo nextest run --release --run-ignored only preface_record_read_cost`.
    #[test]
    #[ignore = "reads a real record named by the environment"]
    fn preface_record_read_cost() {
        let Some(path) = std::env::var_os("HERDR_PREFACE_BENCH_RECORD") else {
            panic!("set HERDR_PREFACE_BENCH_RECORD to a Claude record");
        };
        let path = PathBuf::from(path);
        let bytes = std::fs::metadata(&path).unwrap().len();
        for round in 0..3 {
            let cache = Cache::default();
            let began = std::time::Instant::now();
            let cold = cache.read(&path, PromptTrailAgent::Claude).unwrap();
            let cold_took = began.elapsed();
            let began = std::time::Instant::now();
            let warm = cache.read(&path, PromptTrailAgent::Claude).unwrap();
            let warm_took = began.elapsed();
            assert!(std::sync::Arc::ptr_eq(&cold.turns, &warm.turns));
            assert_eq!(cache.parses.load(std::sync::atomic::Ordering::SeqCst), 1);
            let held = cold.turns.turns.iter().map(Turn::held_bytes).sum::<usize>();
            eprintln!(
                "round {round}: {bytes} bytes, {} turns, {held} bytes held: cold {cold_took:?}, warm {warm_took:?}",
                cold.turns.turns.len()
            );
        }
    }
}
