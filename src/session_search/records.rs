//! Decode harness records into prompt material and session metadata only.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};

use crate::agent_resume::AgentSessionRef;
use crate::api::schema::{PromptTrailAgent, SearchSessionKey, SearchSessionKeyKind, SessionLabel};
use crate::prompt_trail::{self, LineRead};

#[derive(Clone, Copy)]
pub(super) enum FileKind {
    Claude,
    ClaudeSubagent,
    Codex,
    Pi,
    ClaudeHistory,
    CodexHistory,
    CodexIndex,
}

impl FileKind {
    fn agent(self) -> PromptTrailAgent {
        match self {
            Self::Claude | Self::ClaudeSubagent | Self::ClaudeHistory => PromptTrailAgent::Claude,
            Self::Codex | Self::CodexHistory | Self::CodexIndex => PromptTrailAgent::Codex,
            Self::Pi => PromptTrailAgent::Pi,
        }
    }

    fn name(self) -> &'static str {
        match self.agent() {
            PromptTrailAgent::Claude => "claude",
            PromptTrailAgent::Codex => "codex",
            PromptTrailAgent::Pi => "pi",
        }
    }
}

#[derive(Clone)]
pub(super) struct Prompt {
    pub text: Arc<str>,
    pub folded: Arc<str>,
    pub at: u64,
}

#[derive(Clone)]
pub(super) struct Session {
    pub key: SearchSessionKey,
    /// A path-keyed record may also be named by its harness id in process argv.
    pub record_id: Option<String>,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    pub title: Option<String>,
    pub prompts: Vec<Prompt>,
    pub started_at_ms: u64,
    pub last_active_at_ms: u64,
    pub record_path: Option<PathBuf>,
    pub labels: Vec<SessionLabel>,
    pub parent: Option<SearchSessionKey>,
    pub reported: bool,
    pub non_interactive: bool,
    pub claude_subagent: bool,
}

impl Session {
    pub(super) fn new(key: SearchSessionKey, record_path: Option<PathBuf>) -> Self {
        Self {
            key,
            record_id: None,
            cwd: None,
            branch: None,
            title: None,
            prompts: Vec::new(),
            started_at_ms: u64::MAX,
            last_active_at_ms: 0,
            record_path,
            labels: Vec::new(),
            parent: None,
            reported: false,
            non_interactive: false,
            claude_subagent: false,
        }
    }

    pub(super) fn at(&mut self, at: u64) {
        self.started_at_ms = self.started_at_ms.min(at);
        self.last_active_at_ms = self.last_active_at_ms.max(at);
    }
}

#[derive(Default)]
pub(super) struct FileData {
    pub sessions: Vec<Session>,
    pub names: BTreeMap<String, String>,
}

pub(super) fn read(
    path: &Path,
    kind: FileKind,
    size: u64,
    modified_ms: u64,
) -> std::io::Result<FileData> {
    let file = std::fs::File::open(path)?;
    let input = BufReader::new(file.take(size));
    let mut reader: Box<dyn BufRead> = if path.extension().is_some_and(|ext| ext == "zst") {
        Box::new(BufReader::new(super::zstd::Decoder::new(input)))
    } else {
        Box::new(input)
    };
    let mut data = FileData::default();
    let mut history = BTreeMap::new();
    let mut session = match kind {
        FileKind::Claude | FileKind::ClaudeSubagent => path
            .file_stem()
            .and_then(|id| id.to_str())
            .and_then(AgentSessionRef::id)
            .map(|id| {
                let mut session = Session::new(
                    key(kind, SearchSessionKeyKind::Id, id.value),
                    Some(path.into()),
                );
                if matches!(kind, FileKind::ClaudeSubagent) {
                    session.claude_subagent = true;
                    session.parent = path
                        .parent()
                        .and_then(Path::parent)
                        .and_then(Path::file_name)
                        .and_then(|name| name.to_str())
                        .and_then(AgentSessionRef::id)
                        .map(|id| key(kind, SearchSessionKeyKind::Id, id.value));
                }
                session
            }),
        FileKind::Pi => path
            .to_str()
            .and_then(AgentSessionRef::path)
            .map(|reference| {
                Session::new(
                    key(kind, SearchSessionKeyKind::Path, reference.value),
                    Some(path.into()),
                )
            }),
        _ => None,
    };
    let mut line = Vec::new();
    let mut consumed = 0;
    loop {
        match prompt_trail::read_line_bounded(
            &mut reader,
            &mut line,
            &mut consumed,
            prompt_trail::MAX_LINE_BYTES,
        )? {
            LineRead::End => break,
            LineRead::TooLong => {
                return Err(std::io::Error::other("session line exceeds read bound"))
            }
            LineRead::Line => {}
        }
        let Ok(line) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        match kind {
            FileKind::ClaudeHistory | FileKind::CodexHistory => {
                read_history(kind, &line, &mut history);
                continue;
            }
            FileKind::CodexIndex => {
                if let (Some(id), Some(name)) = (
                    line.get("id").and_then(Value::as_str),
                    line.get("thread_name").and_then(Value::as_str),
                ) {
                    data.names.insert(id.into(), name.into());
                }
                continue;
            }
            _ => {}
        }
        if matches!(kind, FileKind::Codex) && line["type"] == "session_meta" {
            if let Some(id) = line["payload"]["id"].as_str().and_then(AgentSessionRef::id) {
                let session = session.get_or_insert_with(|| {
                    Session::new(
                        key(kind, SearchSessionKeyKind::Id, id.value),
                        Some(path.into()),
                    )
                });
                if let Some(parent) = line["payload"]["source"]["subagent"]["thread_spawn"]
                    ["parent_thread_id"]
                    .as_str()
                    .and_then(AgentSessionRef::id)
                {
                    session.parent = Some(key(kind, SearchSessionKeyKind::Id, parent.value));
                }
            }
        }
        let Some(session) = session.as_mut() else {
            continue;
        };
        read_metadata(session, kind, &line);
        let at = line
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(timestamp_ms);
        if let Some(at) = at {
            session.at(at);
        }
        if let Some(title) = prompt_trail::claude_title(kind.agent(), &line) {
            session.title = Some(title);
        }
        if let Some(text) = prompt_trail::prompt_text(kind.agent(), &line) {
            session.prompts.push(Prompt {
                folded: text.to_lowercase().into(),
                text: text.into(),
                at: at.unwrap_or(modified_ms),
            });
        }
    }
    data.sessions.extend(history.into_values());
    if let Some(mut session) = session {
        if session.started_at_ms == u64::MAX {
            session.at(modified_ms);
        }
        session.prompts.sort_by_key(|prompt| prompt.at);
        data.sessions.push(session);
    }
    Ok(data)
}

fn key(kind: FileKind, key_kind: SearchSessionKeyKind, value: String) -> SearchSessionKey {
    SearchSessionKey {
        agent: kind.name().into(),
        kind: key_kind,
        value,
    }
}

fn read_metadata(session: &mut Session, kind: FileKind, line: &Value) {
    let metadata = match kind {
        FileKind::Codex => &line["payload"],
        _ => line,
    };
    if matches!(kind, FileKind::Claude | FileKind::ClaudeSubagent) {
        session.non_interactive |= line["entrypoint"] == "sdk-cli";
    }
    if matches!(kind, FileKind::Codex) && line["type"] == "session_meta" {
        let source = &metadata["source"];
        session.non_interactive |= source == "exec" || source.get("subagent").is_some();
    }
    // Nested Claude records can repeat the parent's sessionId; keep their file identity.
    if matches!(kind, FileKind::Claude) {
        if let Some(id) = line
            .get("sessionId")
            .and_then(Value::as_str)
            .and_then(AgentSessionRef::id)
        {
            session.key.value = id.value;
        }
    }
    if matches!(kind, FileKind::Pi) && line["type"] == "session" {
        session.record_id = line
            .get("id")
            .and_then(Value::as_str)
            .and_then(AgentSessionRef::id)
            .map(|reference| reference.value);
    }
    if let Some(cwd) = metadata
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|cwd| !cwd.is_empty())
    {
        session.cwd = Some(cwd.into());
    }
    let branch = match kind {
        FileKind::Codex => metadata.get("git").and_then(|git| git.get("branch")),
        _ => metadata.get("gitBranch"),
    };
    if let Some(branch) = branch.and_then(Value::as_str) {
        session.branch = Some(branch.into());
    }
}

fn read_history(kind: FileKind, line: &Value, sessions: &mut BTreeMap<SearchSessionKey, Session>) {
    let (id, cwd, text, at) = match kind {
        FileKind::ClaudeHistory => (
            line.get("sessionId").and_then(Value::as_str),
            line.get("project").and_then(Value::as_str),
            line.get("display").and_then(Value::as_str),
            line.get("timestamp").and_then(Value::as_u64),
        ),
        FileKind::CodexHistory => (
            line.get("session_id").and_then(Value::as_str),
            None,
            line.get("text").and_then(Value::as_str),
            line.get("ts")
                .and_then(Value::as_u64)
                .and_then(|seconds| seconds.checked_mul(1000)),
        ),
        _ => return,
    };
    let (Some(text), Some(at)) = (text, at) else {
        return;
    };
    let prompt = match kind {
        FileKind::ClaudeHistory => json!({"type": "user", "isMeta": line.get("isMeta"),
            "isCompactSummary": line.get("isCompactSummary"), "message": {"role": "user", "content": text}}),
        _ => json!({"type": "response_item", "payload": {"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": text}]}}),
    };
    let Some(text) = prompt_trail::prompt_text(kind.agent(), &prompt) else {
        return;
    };
    let reference = if let Some(id) = id.and_then(AgentSessionRef::id) {
        key(kind, SearchSessionKeyKind::Id, id.value)
    } else if matches!(kind, FileKind::ClaudeHistory) {
        let Some(cwd) = cwd else { return };
        let Ok(date) = time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(at) * 1_000_000)
        else {
            return;
        };
        key(
            kind,
            SearchSessionKeyKind::Day,
            format!("{cwd}|{}", date.date()),
        )
    } else {
        return;
    };
    let session = sessions
        .entry(reference.clone())
        .or_insert_with(|| Session::new(reference, None));
    session.at(at);
    if let Some(cwd) = cwd {
        session.cwd = Some(cwd.into());
    }
    session.prompts.push(Prompt {
        folded: text.to_lowercase().into(),
        text: text.into(),
        at,
    });
}

fn timestamp_ms(text: &str) -> Option<u64> {
    let time =
        time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339).ok()?;
    u64::try_from(time.unix_timestamp_nanos() / 1_000_000).ok()
}
