//! Bounded turn indexes from the last-recorded branch of a pi session.

use std::collections::HashMap;
use std::io::BufRead;

use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::api::schema::{
    IndexedTurn, PromptTrailReason, TurnEnd, TurnIndex, TurnIndexAgent, TurnStart,
};
use crate::prompt_trail::{
    clean, cut, read_line_bounded, unix_seconds_from_rfc3339, LineRead, MAX_LINE_BYTES,
    MAX_RECORD_BYTES,
};

const MAX_TURNS: usize = 64;
const HEAD_CHARS: usize = 160;

/// Only heads and branch links survive parsing a line. Tool, system,
/// thinking and extension payloads never enter the index or its cache.
struct Entry {
    parent: Option<String>,
    kind: EntryKind,
}

enum EntryKind {
    Start(IndexedTurn),
    Assistant {
        id: String,
        end: TurnEnd,
        answer: Option<String>,
    },
    ToolResult,
    Other,
}

fn parse(mut reader: impl BufRead, revision: String) -> Result<TurnIndex, PromptTrailReason> {
    let mut entries = HashMap::new();
    let mut session_id = None;
    let mut leaf_id = None;
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
        let parsed: Value = match serde_json::from_slice(&line) {
            Ok(parsed) => parsed,
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
        if session_id.is_none() {
            if parsed["type"] != "session" {
                return Err(PromptTrailReason::Unreadable);
            }
            session_id = Some(required_string(&parsed, "id")?);
            continue;
        }
        let id = required_string(&parsed, "id")?;
        let parent = match parsed.get("parentId") {
            Some(Value::Null) => None,
            Some(Value::String(parent)) => Some(parent.clone()),
            _ => return Err(PromptTrailReason::Unreadable),
        };
        let kind = entry_kind(&parsed, &id)?;
        if entries.insert(id.clone(), Entry { parent, kind }).is_some() {
            return Err(PromptTrailReason::Unreadable);
        }
        leaf_id = Some(id);
    }
    let mut branch = Vec::new();
    let mut next = leaf_id.clone();
    while let Some(id) = next {
        // Removing each visited entry also rejects cycles.
        let entry = entries.remove(&id).ok_or(PromptTrailReason::Unreadable)?;
        next = entry.parent;
        branch.push(entry.kind);
    }
    let mut turns: std::collections::VecDeque<IndexedTurn> = std::collections::VecDeque::new();
    for entry in branch.into_iter().rev() {
        match entry {
            EntryKind::Start(turn) => {
                if turns.len() == MAX_TURNS {
                    turns.pop_front();
                }
                turns.push_back(turn);
            }
            EntryKind::Assistant { id, end, answer } => {
                if let Some(turn) = turns.back_mut() {
                    turn.end = end;
                    turn.answer_id = answer.as_ref().map(|_| id);
                    turn.answer = answer;
                }
            }
            EntryKind::ToolResult => {
                if let Some(turn) = turns.back_mut() {
                    open(turn);
                }
            }
            EntryKind::Other => {}
        }
    }
    if partial {
        if let Some(turn) = turns.back_mut() {
            open(turn);
        }
    }
    Ok(TurnIndex {
        agent: TurnIndexAgent::Pi,
        session_id: session_id.ok_or(PromptTrailReason::Unreadable)?,
        leaf_id,
        revision,
        partial,
        turns: turns.into(),
    })
}

fn required_string(value: &Value, field: &str) -> Result<String, PromptTrailReason> {
    value[field]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or(PromptTrailReason::Unreadable)
}

fn entry_kind(value: &Value, id: &str) -> Result<EntryKind, PromptTrailReason> {
    let message = &value["message"];
    let prompt = value["type"] == "message" && message["role"] == "user";
    let wake = value["type"] == "custom_message"
        && value["customType"] == "wake"
        && value["display"] == true;
    if prompt || wake {
        let at = value["timestamp"]
            .as_str()
            .and_then(unix_seconds_from_rfc3339)
            .ok_or(PromptTrailReason::Unreadable)?;
        return Ok(EntryKind::Start(IndexedTurn {
            id: id.into(),
            at,
            start: if prompt {
                TurnStart::Prompt
            } else {
                TurnStart::Wake
            },
            prompt: if prompt {
                head(&message["content"], false)
            } else {
                None
            },
            end: TurnEnd::Open,
            answer: None,
            answer_id: None,
        }));
    }
    if value["type"] != "message" {
        return Ok(EntryKind::Other);
    }
    if message["role"] == "toolResult" {
        return Ok(EntryKind::ToolResult);
    }
    if message["role"] != "assistant" {
        return Ok(EntryKind::Other);
    }
    let has_tool_call = message["content"]
        .as_array()
        .is_some_and(|parts| parts.iter().any(|part| part["type"] == "toolCall"));
    let end = match message["stopReason"].as_str() {
        Some("stop") if !has_tool_call => TurnEnd::Answered,
        Some("aborted") => TurnEnd::Aborted,
        Some("error" | "length") => TurnEnd::Error,
        _ => TurnEnd::Open,
    };
    Ok(EntryKind::Assistant {
        id: id.into(),
        end,
        answer: if end == TurnEnd::Answered {
            head(&message["content"], true)
        } else {
            None
        },
    })
}

fn open(turn: &mut IndexedTurn) {
    turn.end = TurnEnd::Open;
    turn.answer = None;
    turn.answer_id = None;
}

fn head(content: &Value, skip_fence: bool) -> Option<String> {
    let first_line = |text: &str| {
        text.lines()
            .map(clean)
            .find(|line| !(line.is_empty() || skip_fence && bare_fence(line)))
            .map(|line| cut(&line, HEAD_CHARS))
    };
    match content {
        Value::String(text) => first_line(text),
        Value::Array(parts) => parts
            .iter()
            .filter(|part| part["type"] == "text")
            .filter_map(|part| part["text"].as_str())
            .find_map(first_line),
        _ => None,
    }
}

fn bare_fence(line: &str) -> bool {
    ['`', '~'].into_iter().any(|marker| {
        let count = line
            .chars()
            .take_while(|character| *character == marker)
            .count();
        count >= 3 && !line[count..].contains(marker)
    })
}

const CACHE_RECORDS: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Fingerprint {
    identity: crate::platform::ExecutableFileIdentity,
    bytes: u64,
    modified: std::time::SystemTime,
    changed_ns: i128,
}

impl Fingerprint {
    fn at(path: &std::path::Path) -> Result<Self, PromptTrailReason> {
        let metadata = std::fs::metadata(path).map_err(|_| PromptTrailReason::NoRecord)?;
        if !metadata.is_file() {
            return Err(PromptTrailReason::NoRecord);
        }
        if metadata.len() > MAX_RECORD_BYTES {
            return Err(PromptTrailReason::Unreadable);
        }
        Ok(Self {
            // The platform helper identifies any regular file; it does not
            // require executable permissions or inspect file contents.
            identity: crate::platform::executable_file_identity(path)
                .map_err(|_| PromptTrailReason::Unreadable)?,
            bytes: metadata.len(),
            changed_ns: crate::platform::file_change_time(path)
                .map_err(|_| PromptTrailReason::Unreadable)?,
            modified: metadata
                .modified()
                .map_err(|_| PromptTrailReason::Unreadable)?,
        })
    }
}

struct Cached {
    path: std::path::PathBuf,
    fingerprint: Fingerprint,
    index: TurnIndex,
}

#[derive(Default)]
struct CacheState {
    entries: std::collections::VecDeque<Cached>,
    // Only active flights live here. Existing waiters retain the outcome
    // after removal, while later callers start a fresh flight.
    flights: HashMap<std::path::PathBuf, std::sync::Arc<Flight>>,
}

#[derive(Default)]
struct Flight {
    result: std::sync::Mutex<Option<Result<TurnIndex, PromptTrailReason>>>,
    ready: std::sync::Condvar,
}

impl Flight {
    fn wait(&self) -> Result<TurnIndex, PromptTrailReason> {
        let mut result = self
            .result
            .lock()
            .map_err(|_| PromptTrailReason::Unreadable)?;
        loop {
            if let Some(result) = result.as_ref() {
                return result.clone();
            }
            result = self
                .ready
                .wait(result)
                .map_err(|_| PromptTrailReason::Unreadable)?;
        }
    }
}

#[derive(Default)]
struct Cache {
    state: std::sync::Mutex<CacheState>,
    #[cfg(test)]
    parses: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    joined: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    before_parse: Option<Box<dyn Fn() + Send + Sync>>,
}

impl Cache {
    fn read(&self, path: &std::path::Path) -> Result<TurnIndex, PromptTrailReason> {
        let path = path
            .canonicalize()
            .map_err(|_| PromptTrailReason::NoRecord)?;
        let (flight, leader) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| PromptTrailReason::Unreadable)?;
            match state.flights.get(&path) {
                Some(flight) => (std::sync::Arc::clone(flight), false),
                None => {
                    let flight = std::sync::Arc::new(Flight::default());
                    state
                        .flights
                        .insert(path.clone(), std::sync::Arc::clone(&flight));
                    (flight, true)
                }
            }
        };
        #[cfg(test)]
        self.joined
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if !leader {
            return flight.wait();
        }

        let outcome = self.read_record(&path);
        {
            // Publishing and removal are atomic with respect to admission.
            // Joined callers receive this outcome, including failures; a
            // caller arriving after completion never reuses a failed flight.
            let mut state = self
                .state
                .lock()
                .map_err(|_| PromptTrailReason::Unreadable)?;
            *flight
                .result
                .lock()
                .map_err(|_| PromptTrailReason::Unreadable)? = Some(outcome.clone());
            state.flights.remove(&path);
        }
        flight.ready.notify_all();
        outcome
    }

    fn read_record(&self, path: &std::path::Path) -> Result<TurnIndex, PromptTrailReason> {
        let fingerprint = Fingerprint::at(path)?;
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| PromptTrailReason::Unreadable)?;
            if let Some(position) = state.entries.iter().position(|entry| entry.path == path) {
                if let Some(cached) = state.entries.remove(position) {
                    if cached.fingerprint == fingerprint {
                        let index = cached.index.clone();
                        state.entries.push_back(cached);
                        return Ok(index);
                    }
                }
            }
        }
        let file = std::fs::File::open(path).map_err(|_| PromptTrailReason::Unreadable)?;
        // Hash metadata rather than exposing the path or file identity.
        // A revision still names the same record version after eviction
        // or a server restart.
        let revision = format!(
            "{:x}",
            Sha256::digest(format!("{path:?}:{fingerprint:?}").as_bytes())
        );
        #[cfg(test)]
        self.parses
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        #[cfg(test)]
        if let Some(before_parse) = &self.before_parse {
            before_parse();
        }
        let index = parse(std::io::BufReader::new(file), revision)?;
        // An append or replacement during the read must not be cached
        // under the old fingerprint. The next request will read it whole.
        if Fingerprint::at(path).as_ref() == Ok(&fingerprint) {
            let mut state = self
                .state
                .lock()
                .map_err(|_| PromptTrailReason::Unreadable)?;
            if state.entries.len() == CACHE_RECORDS {
                state.entries.pop_front();
            }
            state.entries.push_back(Cached {
                path: path.to_path_buf(),
                fingerprint,
                index: index.clone(),
            });
        }
        Ok(index)
    }
}

pub(crate) fn read(path: &std::path::Path) -> Result<TurnIndex, PromptTrailReason> {
    static CACHE: std::sync::OnceLock<Cache> = std::sync::OnceLock::new();
    CACHE.get_or_init(Cache::default).read(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn record(entries: &[Value]) -> String {
        std::iter::once(json!({"type":"session", "version":3, "id":"session"}))
            .chain(entries.iter().cloned())
            .map(|entry| entry.to_string() + "\n")
            .collect()
    }

    fn message(id: &str, parent: Option<&str>, role: &str, text: &str, stop: &str) -> Value {
        json!({"type":"message", "id":id, "parentId":parent,
        "timestamp":"2026-03-04T09:15:00Z", "message": {
            "role":role, "content":[{"type":"text", "text":text}], "stopReason":stop
        }})
    }

    #[test]
    fn turn_index_reads_prompt_and_final_answer_heads() {
        let source = record(&[
            message(
                "u",
                None,
                "user",
                "\n  Explain\t the relay\nMore details",
                "",
            ),
            message(
                "a",
                Some("u"),
                "assistant",
                "\nThe relay is ready.\nExtra details",
                "stop",
            ),
        ]);
        let index = parse(source.as_bytes(), "revision".into()).unwrap();
        assert_eq!(index.session_id, "session");
        assert_eq!(index.leaf_id.as_deref(), Some("a"));
        assert!(!index.partial);
        assert_eq!(index.turns.len(), 1);
        assert_eq!(index.turns[0].id, "u");
        assert_eq!(index.turns[0].at, 1772615700);
        assert_eq!(index.turns[0].prompt.as_deref(), Some("Explain the relay"));
        assert_eq!(index.turns[0].end, TurnEnd::Answered);
        assert_eq!(
            index.turns[0].answer.as_deref(),
            Some("The relay is ready.")
        );
        assert_eq!(index.turns[0].answer_id.as_deref(), Some("a"));
    }
    struct Fixture {
        root: std::path::PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "herdr-turn-index-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }
        fn write(&self, name: &str, source: &str) -> std::path::PathBuf {
            let path = self.root.join(name);
            std::fs::write(&path, source).unwrap();
            path
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.root).unwrap();
        }
    }

    #[test]
    fn turn_index_cache_reuses_unchanged_records_and_reparses_appends() {
        use std::io::Write;
        let fixture = Fixture::new();
        let path = fixture.write(
            "record.jsonl",
            &record(&[message("u", None, "user", "Explain", "")]),
        );
        let cache = Cache::default();
        let first = cache.read(&path).unwrap();
        assert_eq!(cache.read(&path).unwrap(), first);
        assert_eq!(cache.parses.load(std::sync::atomic::Ordering::SeqCst), 1);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(
            file,
            "{}",
            message("a", Some("u"), "assistant", "Ready", "stop")
        )
        .unwrap();
        drop(file);
        let changed = cache.read(&path).unwrap();
        assert_eq!(cache.parses.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_ne!(changed.revision, first.revision);
        assert_eq!(changed.turns[0].answer.as_deref(), Some("Ready"));
    }
    #[test]
    fn turn_index_last_assistant_decides_every_end_without_leaking_other_text() {
        for (stop, end) in [
            ("stop", TurnEnd::Answered),
            ("aborted", TurnEnd::Aborted),
            ("error", TurnEnd::Error),
            ("length", TurnEnd::Error),
            ("toolUse", TurnEnd::Open),
            ("pending", TurnEnd::Open),
            ("deferred", TurnEnd::Open),
        ] {
            let mut last = message("last", Some("prior"), "assistant", "Final head", stop);
            last["message"]["errorMessage"] = json!("private error detail");
            last["message"]["content"]
                .as_array_mut()
                .unwrap()
                .insert(0, json!({"type":"thinking", "thinking":"private thought"}));
            let source = record(&[
                message("u", None, "user", "Explain", ""),
                message("prior", Some("u"), "assistant", "Earlier answer", "stop"),
                last,
            ]);
            let index = parse(source.as_bytes(), "r".into()).unwrap();
            let turn = &index.turns[0];
            assert_eq!(turn.end, end, "{stop}");
            assert_eq!(
                turn.answer.as_deref(),
                if end == TurnEnd::Answered {
                    Some("Final head")
                } else {
                    None
                }
            );
            assert_eq!(
                turn.answer_id.as_deref(),
                if end == TurnEnd::Answered {
                    Some("last")
                } else {
                    None
                }
            );
            let wire = serde_json::to_string(&index).unwrap();
            assert!(!wire.contains("private"));
            assert!(!wire.contains("Earlier answer"));
        }
    }

    #[test]
    fn turn_index_tool_calls_and_unanswered_tool_results_are_open() {
        let mut call = message("a", Some("u"), "assistant", "Not final", "stop");
        call["message"]["content"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"toolCall", "id":"call", "name":"read", "arguments":{}}));
        let source = record(&[message("u", None, "user", "Explain", ""), call]);
        let turn = &parse(source.as_bytes(), "r".into()).unwrap().turns[0];
        assert_eq!(turn.end, TurnEnd::Open);
        assert_eq!(turn.answer, None);
        let source = record(&[
            message("u", None, "user", "Explain", ""),
            message("a", Some("u"), "assistant", "Earlier answer", "stop"),
            message("t", Some("a"), "toolResult", "private result", ""),
        ]);
        let turn = &parse(source.as_bytes(), "r".into()).unwrap().turns[0];
        assert_eq!(turn.end, TurnEnd::Open);
        assert_eq!(turn.answer, None);
        assert_eq!(turn.answer_id, None);
    }

    fn custom(id: &str, parent: &str, kind: &str, display: bool) -> Value {
        json!({"type":"custom_message", "id":id, "parentId":parent, "timestamp":"2026-03-04T09:16:00Z",
            "customType":kind, "display":display, "content":"private extension message"})
    }

    #[test]
    fn turn_index_steers_and_displayed_wakes_start_turns_but_hidden_continuations_do_not() {
        let source = record(&[
            message("u", None, "user", "Initial prompt", ""),
            message("tool", Some("u"), "assistant", "Starting", "toolUse"),
            message("steer", Some("tool"), "user", "Change direction", ""),
            message("early", Some("steer"), "assistant", "Early stop", "stop"),
            custom("hidden", "early", "wake", false),
            custom("other", "hidden", "notice", true),
            message("late", Some("other"), "assistant", "Final stop", "stop"),
            custom("wake", "late", "wake", true),
            message("wake-answer", Some("wake"), "assistant", "Awake", "stop"),
        ]);
        let index = parse(source.as_bytes(), "r".into()).unwrap();
        assert_eq!(
            index
                .turns
                .iter()
                .map(|t| t.id.as_str())
                .collect::<Vec<_>>(),
            ["u", "steer", "wake"]
        );
        assert_eq!(index.turns[0].end, TurnEnd::Open);
        assert_eq!(index.turns[1].answer.as_deref(), Some("Final stop"));
        assert_eq!(index.turns[1].answer_id.as_deref(), Some("late"));
        assert_eq!(index.turns[2].start, TurnStart::Wake);
        assert_eq!(index.turns[2].prompt, None);
        assert_eq!(index.turns[2].at, 1772615760);
        assert!(!serde_json::to_string(&index).unwrap().contains("private"));
    }

    #[test]
    fn turn_index_uses_only_the_last_recorded_branch_including_metadata_links() {
        let source = record(&[
            message("u", None, "user", "Root prompt", ""),
            message("old", Some("u"), "assistant", "Abandoned answer", "stop"),
            message("discarded", Some("old"), "user", "Abandoned prompt", ""),
            json!({"type":"branch_summary", "id":"branch", "parentId":"u", "fromId":"discarded", "summary":"private"}),
            message("new", Some("branch"), "assistant", "Chosen answer", "stop"),
            json!({"type":"session_info", "id":"leaf", "parentId":"new", "name":"private"}),
        ]);
        let index = parse(source.as_bytes(), "r".into()).unwrap();
        assert_eq!(index.leaf_id.as_deref(), Some("leaf"));
        assert_eq!(index.turns.len(), 1);
        assert_eq!(index.turns[0].answer.as_deref(), Some("Chosen answer"));
        let wire = serde_json::to_string(&index).unwrap();
        assert!(!wire.contains("Abandoned"));
        assert!(!wire.contains("private"));
        let source = record(&[
            message("old", None, "user", "Old root", ""),
            message("new", None, "user", "New root", ""),
        ]);
        let index = parse(source.as_bytes(), "r".into()).unwrap();
        assert_eq!(index.turns.len(), 1);
        assert_eq!(index.turns[0].id, "new");
    }

    #[test]
    fn turn_index_missing_parents_cycles_and_malformed_whole_lines_are_unreadable() {
        for source in [
            record(&[message("u", Some("missing"), "user", "Explain", "")]),
            record(&[message("u", Some("u"), "user", "Explain", "")]),
            record(&[
                message("u", Some("a"), "user", "Explain", ""),
                message("a", Some("u"), "assistant", "Answer", "stop"),
            ]),
            record(&[]) + "{broken}\n",
        ] {
            assert_eq!(
                parse(source.as_bytes(), "r".into()),
                Err(PromptTrailReason::Unreadable)
            );
        }
    }

    #[test]
    fn turn_index_half_written_tail_opens_only_the_newest_turn() {
        let whole = record(&[
            message("u1", None, "user", "First", ""),
            message("a1", Some("u1"), "assistant", "First answer", "stop"),
            message("u2", Some("a1"), "user", "Second", ""),
            message("a2", Some("u2"), "assistant", "Second answer", "stop"),
        ]);
        let partial = whole.clone() + "{\"type\":\"message\",\"id\":\"unfinished";
        let index = parse(partial.as_bytes(), "r".into()).unwrap();
        assert!(index.partial);
        assert_eq!(index.leaf_id.as_deref(), Some("a2"));
        assert_eq!(index.turns[0].end, TurnEnd::Answered);
        assert_eq!(index.turns[1].end, TurnEnd::Open);
        assert_eq!(index.turns[1].answer, None);
        assert_eq!(index.turns[1].answer_id, None);
        assert!(
            !parse(whole.trim_end().as_bytes(), "r".into())
                .unwrap()
                .partial
        );
    }

    #[test]
    fn turn_index_heads_skip_fences_and_blank_text_and_cut_at_a_word() {
        for (text, expected) in [
            (
                "\n```rust\nlet ready = true;\n```",
                Some("let ready = true;"),
            ),
            ("~~~\n\t\nReady\n~~~", Some("Ready")),
            ("```inline```", Some("```inline```")),
            ("\t\n\u{0001}", None),
        ] {
            let source = record(&[
                message("u", None, "user", "Explain", ""),
                message("a", Some("u"), "assistant", text, "stop"),
            ]);
            let index = parse(source.as_bytes(), "r".into()).unwrap();
            assert_eq!(index.turns[0].end, TurnEnd::Answered);
            assert_eq!(index.turns[0].answer.as_deref(), expected);
            assert_eq!(index.turns[0].answer_id.as_deref(), expected.map(|_| "a"));
        }
        let long = "word ".repeat(40);
        let source = record(&[
            message("u", None, "user", &long, ""),
            message("a", Some("u"), "assistant", &long, "stop"),
        ]);
        let index = parse(source.as_bytes(), "r".into()).unwrap();
        let expected = "word ".repeat(31) + "word…";
        assert_eq!(expected.chars().count(), 160);
        assert_eq!(index.turns[0].prompt.as_deref(), Some(expected.as_str()));
        assert_eq!(index.turns[0].answer.as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn turn_index_keeps_the_latest_sixty_four_turns_oldest_first() {
        let mut entries = Vec::new();
        let mut parent = None;
        for number in 0..70 {
            let id = format!("u{number}");
            entries.push(message(&id, parent.as_deref(), "user", "Explain", ""));
            parent = Some(id);
        }
        let source = record(&entries);
        let index = parse(source.as_bytes(), "r".into()).unwrap();
        assert_eq!(index.turns.len(), 64);
        assert_eq!(index.turns[0].id, "u6");
        assert_eq!(index.turns[63].id, "u69");
        assert!(index.turns.iter().all(|t| t.end == TurnEnd::Open));
    }
    #[test]
    fn turn_index_cache_single_flight_and_lru_eviction() {
        use std::sync::{atomic::Ordering, Arc, Barrier};
        let fixture = Fixture::new();
        let cache = Arc::new(Cache::default());
        let path = fixture.write(
            "shared.jsonl",
            &record(&[message("u", None, "user", "Explain", "")]),
        );
        let start = Arc::new(Barrier::new(8));
        let readers: Vec<_> = (0..8)
            .map(|_| {
                let cache = Arc::clone(&cache);
                let start = Arc::clone(&start);
                let path = path.clone();
                std::thread::spawn(move || {
                    start.wait();
                    cache.read(&path).unwrap()
                })
            })
            .collect();
        let indexes: Vec<_> = readers
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert!(indexes.iter().all(|index| index == &indexes[0]));
        assert_eq!(cache.parses.load(Ordering::SeqCst), 1);
        let mut paths = vec![path];
        for n in 1..32 {
            paths.push(fixture.write(&format!("record{n}.jsonl"), &record(&[])));
            cache.read(&paths[n]).unwrap();
        }
        // Touch the oldest; the next insertion must evict the second oldest.
        cache.read(&paths[0]).unwrap();
        let extra = fixture.write("extra.jsonl", &record(&[]));
        cache.read(&extra).unwrap();
        assert_eq!(cache.parses.load(Ordering::SeqCst), 33);
        cache.read(&paths[0]).unwrap();
        assert_eq!(cache.parses.load(Ordering::SeqCst), 33);
        cache.read(&paths[1]).unwrap();
        assert_eq!(cache.parses.load(Ordering::SeqCst), 34);
        assert_eq!(cache.state.lock().unwrap().entries.len(), 32);
    }

    #[test]
    fn turn_index_cache_detects_replacement_and_same_size_rewrites() {
        use std::sync::atomic::Ordering;
        let fixture = Fixture::new();
        let first = record(&[message("u", None, "user", "First", "")]);
        let second = record(&[message("u", None, "user", "Other", "")]);
        assert_eq!(first.len(), second.len());
        let path = fixture.write("record.jsonl", &first);
        let cache = Cache::default();
        let old = cache.read(&path).unwrap();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let replacement = fixture.write("replacement.jsonl", &second);
        std::fs::File::options()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        let replaced = cache.read(&path).unwrap();
        assert_eq!(replaced.turns[0].prompt.as_deref(), Some("Other"));
        assert_ne!(replaced.revision, old.revision);
        assert_eq!(cache.parses.load(Ordering::SeqCst), 2);
        std::fs::write(&path, first).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(modified + std::time::Duration::from_secs(1)),
            )
            .unwrap();
        let rewritten = cache.read(&path).unwrap();
        assert_eq!(rewritten.turns[0].prompt.as_deref(), Some("First"));
        assert_ne!(rewritten.revision, replaced.revision);
        assert_eq!(cache.parses.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn turn_index_record_and_line_caps_refuse_oversized_input() {
        let fixture = Fixture::new();
        let path = fixture.write("large.jsonl", "");
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_RECORD_BYTES + 1)
            .unwrap();
        assert_eq!(
            Cache::default().read(&path),
            Err(PromptTrailReason::Unreadable)
        );
        let long = record(&[]) + &" ".repeat(MAX_LINE_BYTES + 1) + "\n";
        assert_eq!(
            parse(long.as_bytes(), "r".into()),
            Err(PromptTrailReason::Unreadable)
        );
    }
    #[test]
    fn turn_index_revision_names_the_record_version_across_caches() {
        let fixture = Fixture::new();
        let path = fixture.write("record.jsonl", &record(&[]));
        let first = Cache::default().read(&path).unwrap();
        let second = Cache::default().read(&path).unwrap();
        assert_eq!(first.revision, second.revision);
        fixture.write(
            "record.jsonl",
            &record(&[message("u", None, "user", "Explain", "")]),
        );
        let changed = Cache::default().read(&path).unwrap();
        assert_ne!(first.revision, changed.revision);
    }
    #[test]
    fn turn_index_cache_detects_same_size_rewrite_with_restored_mtime() {
        let fixture = Fixture::new();
        let first = record(&[message("u", None, "user", "First", "")]);
        let second = record(&[message("u", None, "user", "Other", "")]);
        assert_eq!(first.len(), second.len());
        let path = fixture.write("record.jsonl", &first);
        let cache = Cache::default();
        let old = cache.read(&path).unwrap();
        let before = Fingerprint::at(&path).unwrap();
        std::fs::write(&path, second).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(before.modified))
            .unwrap();
        let after = Fingerprint::at(&path).unwrap();
        assert_eq!(before.identity, after.identity);
        assert_eq!(before.bytes, after.bytes);
        assert_eq!(before.modified, after.modified);
        assert_ne!(before.changed_ns, after.changed_ns);
        let rewritten = cache.read(&path).unwrap();
        assert_eq!(rewritten.turns[0].prompt.as_deref(), Some("Other"));
        assert_ne!(rewritten.revision, old.revision);
        assert_eq!(cache.parses.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn turn_index_eof_distinguishes_corruption_from_incomplete_json_and_utf8() {
        let whole = record(&[
            message("u", None, "user", "Explain", ""),
            message("a", Some("u"), "assistant", "Ready", "stop"),
        ]);
        for tail in [
            b"{broken}".as_slice(),
            b"{broken}\n",
            b"{\"text\":\"\xff",
            b"{\"id\":\"unfinished\n",
        ] {
            let mut source = whole.as_bytes().to_vec();
            source.extend_from_slice(tail);
            assert_eq!(
                parse(source.as_slice(), "r".into()),
                Err(PromptTrailReason::Unreadable)
            );
        }
        for tail in [b"{\"id\":\"unfinished".as_slice(), b"{\"text\":\"\xe2\x82"] {
            let mut source = whole.as_bytes().to_vec();
            source.extend_from_slice(tail);
            let index = parse(source.as_slice(), "r".into()).unwrap();
            assert!(index.partial);
            assert_eq!(index.leaf_id.as_deref(), Some("a"));
            assert_eq!(index.turns[0].end, TurnEnd::Open);
            assert_eq!(index.turns[0].answer, None);
            assert_eq!(index.turns[0].answer_id, None);
        }
        let complete = parse(whole.trim_end().as_bytes(), "r".into()).unwrap();
        assert!(!complete.partial);
        assert_eq!(complete.turns[0].answer.as_deref(), Some("Ready"));
    }
    #[test]
    fn turn_index_overlapping_failed_reads_share_the_flight_but_later_reads_retry() {
        use std::sync::{atomic::Ordering, mpsc, Arc};
        use std::time::{Duration, Instant};
        let fixture = Fixture::new();
        let path = fixture.write("malformed.jsonl", "{broken}\n");
        let (started, observed) = mpsc::channel();
        let (release, held) = mpsc::channel();
        let held = std::sync::Mutex::new(Some(held));
        let cache = Arc::new(Cache {
            before_parse: Some(Box::new(move || {
                if let Some(held) = held.lock().unwrap().take() {
                    started.send(()).unwrap();
                    held.recv_timeout(Duration::from_secs(5)).unwrap();
                }
            })),
            ..Cache::default()
        });
        let leader = {
            let cache = Arc::clone(&cache);
            let path = path.clone();
            std::thread::spawn(move || cache.read(&path))
        };
        observed.recv_timeout(Duration::from_secs(5)).unwrap();
        let waiter = {
            let cache = Arc::clone(&cache);
            let path = path.clone();
            std::thread::spawn(move || cache.read(&path))
        };
        // Observe admission to the flight, not elapsed time or thread start.
        let deadline = Instant::now() + Duration::from_secs(3);
        while cache.joined.load(Ordering::SeqCst) < 2 && Instant::now() < deadline {
            std::thread::yield_now();
        }
        let joined = cache.joined.load(Ordering::SeqCst);
        release.send(()).unwrap();
        let leader_result = leader.join().unwrap();
        let waiter_result = waiter.join().unwrap();
        assert_eq!(joined, 2, "the waiter must overlap the held leader");
        assert_eq!(leader_result, Err(PromptTrailReason::Unreadable));
        assert_eq!(waiter_result, Err(PromptTrailReason::Unreadable));
        assert_eq!(cache.parses.load(Ordering::SeqCst), 1);
        assert_eq!(cache.read(&path), Err(PromptTrailReason::Unreadable));
        assert_eq!(cache.parses.load(Ordering::SeqCst), 2);
        fixture.write(
            "malformed.jsonl",
            &record(&[message("u", None, "user", "Repaired", "")]),
        );
        assert_eq!(
            cache.read(&path).unwrap().turns[0].prompt.as_deref(),
            Some("Repaired")
        );
        assert_eq!(cache.parses.load(Ordering::SeqCst), 3);
    }
}
