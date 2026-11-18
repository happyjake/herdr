//! Incremental disk refresh. Queries never walk record directories or open records.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

use super::records::{self, FileData, FileKind, Prompt, Session};

pub(crate) struct Desk {
    pub home: Option<PathBuf>,
    pub processes:
        Box<dyn Fn() -> std::io::Result<Vec<crate::platform::SessionProcess>> + Send + Sync>,
    pub registry_retry_sleep: Box<dyn Fn(Duration) + Send + Sync>,
    pub logbook: PathBuf,
}

pub(crate) struct Catalog {
    desk: Desk,
    files: Mutex<HashMap<PathBuf, CachedFile>>,
    logbook: Mutex<CachedLogbook>,
    snapshot: Mutex<Arc<Snapshot>>,
    ready: Condvar,
}

#[derive(Default)]
pub(super) struct Snapshot {
    pub sessions: Vec<Arc<Session>>,
    pub complete: bool,
    /// A refresh attempt ended, even if individual sources could not be read.
    refreshed: bool,
}

#[derive(Clone, PartialEq, Eq)]
struct Stamp {
    modified: SystemTime,
    size: u64,
}

struct CachedFile {
    stamp: Stamp,
    data: FileData,
}

#[derive(Default)]
struct CachedLogbook {
    stamp: Option<Stamp>,
    sessions: BTreeMap<crate::api::schema::SearchSessionKey, Session>,
}

impl Catalog {
    pub(crate) fn new(desk: Desk) -> Self {
        Self {
            desk,
            files: Mutex::new(HashMap::new()),
            logbook: Mutex::new(CachedLogbook::default()),
            snapshot: Mutex::new(Arc::new(Snapshot::default())),
            ready: Condvar::new(),
        }
    }

    pub(super) fn outside_sessions(&self) -> std::io::Result<super::outside::OutsideSessions> {
        super::outside::read(
            self.desk.home.as_deref(),
            &(self.desk.processes)()?,
            self.desk.registry_retry_sleep.as_ref(),
        )
    }

    /// One maintenance pass. The server runs this on its single catalog worker;
    /// callers may keep querying the previous snapshot throughout the pass.
    pub(crate) fn refresh(&self) {
        let Ok(mut cache) = self.files.try_lock() else {
            return;
        };
        let mut files = Vec::new();
        let mut complete = true;
        let mut logbook_complete = true;
        let Ok(mut logbook) = self.logbook.lock() else {
            return;
        };
        let history = std::fs::metadata(&self.desk.logbook)
            .map(|metadata| Some(stamp(&metadata)))
            .or_else(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    Ok(None)
                } else {
                    Err(error)
                }
            });
        match history {
            Ok(stamp) if stamp != logbook.stamp => match super::logbook::read(&self.desk.logbook) {
                Ok(sessions) => *logbook = CachedLogbook { stamp, sessions },
                Err(error) => {
                    tracing::warn!(%error, "could not refresh session logbook");
                    logbook_complete = false;
                }
            },
            Err(error) => {
                tracing::warn!(%error, "could not stat session logbook");
                logbook_complete = false;
            }
            _ => {}
        }
        if let Some(home) = &self.desk.home {
            for (root, depth, kind) in [
                (".claude/projects", 1, FileKind::Claude),
                (".codex/sessions", 3, FileKind::Codex),
                (".pi/agent/sessions", 1, FileKind::Pi),
            ] {
                walk(&home.join(root), depth, kind, &mut files, &mut complete);
            }
            let gemini_dir = home.join(".gemini");
            let agy_dir = gemini_dir.join("antigravity-cli");
            if is_real_dir(&gemini_dir, &mut complete) && is_real_dir(&agy_dir, &mut complete) {
                walk_agy_brain(&agy_dir.join("brain"), &mut files, &mut complete);
                let history_path = agy_dir.join("history.jsonl");
                match std::fs::symlink_metadata(&history_path) {
                    Ok(metadata) if metadata.is_file() => {
                        files.push((history_path, stamp(&metadata), FileKind::AgyHistory));
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => complete = false,
                }
            }
            for (path, kind) in [
                (".claude/history.jsonl", FileKind::ClaudeHistory),
                (".codex/history.jsonl", FileKind::CodexHistory),
                (".codex/session_index.jsonl", FileKind::CodexIndex),
            ] {
                let path = home.join(path);
                if let Ok(metadata) = std::fs::symlink_metadata(&path) {
                    if metadata.is_file() {
                        files.push((path, stamp(&metadata), kind));
                    }
                }
            }
        }
        files.sort_by(|a, b| b.1.modified.cmp(&a.1.modified).then_with(|| b.0.cmp(&a.0)));
        let present: HashSet<_> = files.iter().map(|(path, _, _)| path.clone()).collect();
        if complete {
            cache.retain(|path, _| present.contains(path));
        }
        let discovery_complete = complete;
        for (index, (path, stamp, kind)) in files.into_iter().enumerate() {
            if cache.get(&path).is_some_and(|cached| cached.stamp == stamp) {
                continue;
            }
            let modified_ms = stamp
                .modified
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            match records::read(&path, kind, stamp.size, modified_ms) {
                Ok(data) => {
                    cache.insert(path, CachedFile { stamp, data });
                }
                Err(_) => {
                    cache.remove(&path);
                    complete = false;
                }
            }
            if index % 32 == 0 {
                self.publish(&cache, &logbook.sessions, false, false, false);
            }
        }
        self.publish(
            &cache,
            &logbook.sessions,
            discovery_complete,
            complete && logbook_complete,
            true,
        );
    }

    pub(super) fn snapshot_until(&self, deadline: Instant) -> Arc<Snapshot> {
        let Ok(snapshot) = self.snapshot.lock() else {
            return Arc::default();
        };
        self.ready
            .wait_timeout_while(
                snapshot,
                deadline.saturating_duration_since(Instant::now()),
                |snapshot| !snapshot.refreshed,
            )
            .map(|(snapshot, _)| Arc::clone(&snapshot))
            .unwrap_or_default()
    }

    fn publish(
        &self,
        cache: &HashMap<PathBuf, CachedFile>,
        logbook: &BTreeMap<crate::api::schema::SearchSessionKey, Session>,
        discovery_complete: bool,
        complete: bool,
        refreshed: bool,
    ) {
        let mut sessions = merge(cache, logbook, discovery_complete);
        sessions.sort_by(|a, b| {
            b.last_active_at_ms
                .cmp(&a.last_active_at_ms)
                .then_with(|| a.key.cmp(&b.key))
        });
        if let Ok(mut snapshot) = self.snapshot.lock() {
            *snapshot = Arc::new(Snapshot {
                sessions,
                complete,
                refreshed,
            });
            self.ready.notify_all();
        }
    }
}

/// Fixed harness layout, newest buckets first. No symlink traversal or unrelated
/// directory search. Claude subagents have one explicitly recognized nested layout.
fn walk(
    dir: &Path,
    depth: usize,
    kind: FileKind,
    found: &mut Vec<(PathBuf, Stamp, FileKind)>,
    complete: &mut bool,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                *complete = false;
            }
            return;
        }
    };
    let mut children = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            *complete = false;
            continue;
        };
        let Ok(metadata) = entry.metadata() else {
            *complete = false;
            continue;
        };
        let Ok(kind) = entry.file_type() else {
            *complete = false;
            continue;
        };
        if kind.is_symlink() {
            continue;
        }
        children.push((entry.path(), metadata));
    }
    children.sort_by(|a, b| {
        b.1.modified()
            .ok()
            .cmp(&a.1.modified().ok())
            .then_with(|| b.0.cmp(&a.0))
    });
    for (path, metadata) in children {
        if depth > 0 && metadata.is_dir() {
            walk(&path, depth - 1, kind, found, complete);
        } else if depth == 0 && metadata.is_dir() && matches!(kind, FileKind::Claude) {
            let subagents = path.join("subagents");
            if std::fs::symlink_metadata(&subagents).is_ok_and(|metadata| metadata.is_dir()) {
                walk(&subagents, 0, FileKind::ClaudeSubagent, found, complete);
            }
        } else if depth == 0
            && metadata.is_file()
            && (!matches!(kind, FileKind::ClaudeSubagent)
                || path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("agent-")))
            && (path.extension().is_some_and(|ext| ext == "jsonl")
                || (matches!(kind, FileKind::Codex)
                    && path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.ends_with(".jsonl.zst"))))
        {
            found.push((path, stamp(&metadata), kind));
        }
    }
}

/// Discover `brain/<id>/.system_generated/logs/transcript_full.jsonl` and
/// `brain/<id>/.system_generated/subagents/*.json` without following symlinks
/// or picking up truncated `transcript.jsonl` files.
fn walk_agy_brain(
    brain_dir: &Path,
    found: &mut Vec<(PathBuf, Stamp, FileKind)>,
    complete: &mut bool,
) {
    if !is_real_dir(brain_dir, complete) {
        return;
    }
    let entries = match std::fs::read_dir(brain_dir) {
        Ok(entries) => entries,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                *complete = false;
            }
            return;
        }
    };
    for entry in entries {
        let Ok(entry) = entry else {
            *complete = false;
            continue;
        };
        let Ok(file_type) = entry.file_type() else {
            *complete = false;
            continue;
        };
        if file_type.is_symlink() || !file_type.is_dir() {
            continue;
        }
        let Some(conv_id) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !crate::agent_resume::valid_agy_conversation_id(&conv_id) {
            continue;
        }
        let system_generated = entry.path().join(".system_generated");
        if !is_real_dir(&system_generated, complete) {
            continue;
        }
        let logs_dir = system_generated.join("logs");
        if is_real_dir(&logs_dir, complete) {
            let transcript = logs_dir.join("transcript_full.jsonl");
            match std::fs::symlink_metadata(&transcript) {
                Ok(metadata) if metadata.is_file() => {
                    found.push((transcript, stamp(&metadata), FileKind::Agy));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => *complete = false,
            }
        }
        let subagents_dir = system_generated.join("subagents");
        if is_real_dir(&subagents_dir, complete) {
            walk_agy_subagents(&subagents_dir, found, complete);
        }
    }
}

fn walk_agy_subagents(
    subagents_dir: &Path,
    found: &mut Vec<(PathBuf, Stamp, FileKind)>,
    complete: &mut bool,
) {
    let entries = match std::fs::read_dir(subagents_dir) {
        Ok(entries) => entries,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                *complete = false;
            }
            return;
        }
    };
    for entry in entries {
        let Ok(entry) = entry else {
            *complete = false;
            continue;
        };
        let Ok(file_type) = entry.file_type() else {
            *complete = false;
            continue;
        };
        if file_type.is_symlink() || !file_type.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() => {
                found.push((path, stamp(&metadata), FileKind::AgySubagent));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => *complete = false,
        }
    }
}

fn is_real_dir(path: &Path, complete: &mut bool) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata.is_dir(),
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                *complete = false;
            }
            false
        }
    }
}

fn stamp(metadata: &std::fs::Metadata) -> Stamp {
    Stamp {
        modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        size: metadata.len(),
    }
}

/// History contributes prompts, while a record supplies the current place and
/// resume identity. Claude's id deliberately survives moves between projects.
fn merge(
    cache: &HashMap<PathBuf, CachedFile>,
    logbook: &BTreeMap<crate::api::schema::SearchSessionKey, Session>,
    complete: bool,
) -> Vec<Arc<Session>> {
    let mut sources: Vec<_> = cache
        .values()
        .flat_map(|entry| {
            entry
                .data
                .sessions
                .iter()
                .map(move |session| (entry.stamp.modified, session))
        })
        .collect();
    sources.sort_by(|a, b| {
        a.1.record_path
            .is_some()
            .cmp(&b.1.record_path.is_some())
            .then_with(|| a.0.cmp(&b.0))
            .then_with(|| a.1.record_path.cmp(&b.1.record_path))
    });
    let mut sessions = logbook.clone();
    for (_, source) in sources {
        if let Some(session) = sessions.get_mut(&source.key) {
            session.record_id = source.record_id.clone().or(session.record_id.take());
            session.cwd = source.cwd.clone().or(session.cwd.take());
            session.branch = source.branch.clone().or(session.branch.take());
            session.title = source.title.clone().or(session.title.take());
            session.record_path = source.record_path.clone().or(session.record_path.take());
            session.parent = source.parent.clone().or(session.parent.take());
            session.from_history |= source.from_history;
            if session.key.agent == "agy" {
                session.non_interactive = if session.parent.is_some() {
                    true
                } else {
                    !session.from_history
                };
            } else {
                session.non_interactive |= source.non_interactive;
            }
            session.claude_subagent |= source.claude_subagent;
            session.started_at_ms = session.started_at_ms.min(source.started_at_ms);
            session.last_active_at_ms = session.last_active_at_ms.max(source.last_active_at_ms);
            session.prompts.extend(source.prompts.iter().cloned());
        } else {
            sessions.insert(source.key.clone(), source.clone());
        }
    }
    for entry in cache.values() {
        for (id, name) in &entry.data.names {
            let key = crate::api::schema::SearchSessionKey {
                agent: "codex".into(),
                kind: crate::api::schema::SearchSessionKeyKind::Id,
                value: id.clone(),
            };
            if let Some(session) = sessions.get_mut(&key) {
                session.title = Some(name.clone());
            }
        }
    }
    sessions
        .into_values()
        .filter(|session| {
            session.started_at_ms != u64::MAX && (complete || session.record_path.is_some())
        })
        .map(|mut session| {
            if session.key.agent == "agy" {
                reconcile_agy_prompts(&mut session.prompts);
            } else {
                session.prompts.sort_by_key(|prompt| prompt.at);
                let mut seen = HashSet::new();
                session
                    .prompts
                    .retain(|prompt| seen.insert((prompt.at / 1000, Arc::clone(&prompt.text))));
            }
            session.title = crate::prompt_trail::judged_title(
                session.title.take(),
                session
                    .prompts
                    .first()
                    .map_or("", |prompt| prompt.text.as_ref()),
            );
            Arc::new(session)
        })
        .collect()
}

/// Pair history and transcript copies of the same submission 1-to-1 within a
/// short window so cross-second writes collapse while repeated asks survive.
fn reconcile_agy_prompts(prompts: &mut Vec<Prompt>) {
    const CROSS_SOURCE_DEDUP_WINDOW_MS: u64 = 2_000;
    let mut by_text: BTreeMap<Arc<str>, (Vec<Prompt>, Vec<Prompt>)> = BTreeMap::new();
    for prompt in prompts.drain(..) {
        let entry = by_text.entry(Arc::clone(&prompt.text)).or_default();
        if prompt.from_history {
            entry.0.push(prompt);
        } else {
            entry.1.push(prompt);
        }
    }
    for (_, (mut history, mut transcript)) in by_text {
        history.sort_by_key(|prompt| prompt.at);
        transcript.sort_by_key(|prompt| prompt.at);
        let mut history = VecDeque::from(history);
        let mut transcript = VecDeque::from(transcript);
        while let (Some(h), Some(t)) = (history.front(), transcript.front()) {
            if h.at.abs_diff(t.at) <= CROSS_SOURCE_DEDUP_WINDOW_MS {
                if let (Some(h), Some(t)) = (history.pop_front(), transcript.pop_front()) {
                    prompts.push(if h.at <= t.at { h } else { t });
                }
            } else if h.at < t.at {
                if let Some(h) = history.pop_front() {
                    prompts.push(h);
                }
            } else if let Some(t) = transcript.pop_front() {
                prompts.push(t);
            }
        }
        prompts.extend(history);
        prompts.extend(transcript);
    }
    prompts.sort_by_key(|prompt| prompt.at);
}
