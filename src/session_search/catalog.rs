//! Incremental disk refresh. Queries never walk record directories or open records.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Instant, SystemTime};

use super::records::{self, FileData, FileKind, Session};

pub(crate) struct Desk {
    pub home: Option<PathBuf>,
}

pub(crate) struct Catalog {
    desk: Desk,
    files: Mutex<HashMap<PathBuf, CachedFile>>,
    snapshot: Mutex<Arc<Snapshot>>,
    ready: Condvar,
}

#[derive(Default)]
pub(super) struct Snapshot {
    pub sessions: Vec<Arc<Session>>,
    pub complete: bool,
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

impl Catalog {
    pub(crate) fn new(desk: Desk) -> Self {
        Self {
            desk,
            files: Mutex::new(HashMap::new()),
            snapshot: Mutex::new(Arc::new(Snapshot::default())),
            ready: Condvar::new(),
        }
    }

    /// One maintenance pass. The server runs this on its single catalog worker;
    /// callers may keep querying the previous snapshot throughout the pass.
    pub(crate) fn refresh(&self) {
        let Ok(mut cache) = self.files.try_lock() else {
            return;
        };
        let mut files = Vec::new();
        let mut complete = true;
        if let Some(home) = &self.desk.home {
            for (root, depth, kind) in [
                (".claude/projects", 1, FileKind::Claude),
                (".codex/sessions", 3, FileKind::Codex),
                (".pi/agent/sessions", 1, FileKind::Pi),
            ] {
                walk(&home.join(root), depth, kind, &mut files, &mut complete);
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
                    complete = false;
                }
            }
            if index % 32 == 0 {
                self.publish(&cache, false);
            }
        }
        self.publish(&cache, complete);
    }

    pub(super) fn snapshot_until(&self, deadline: Instant) -> Arc<Snapshot> {
        let Ok(snapshot) = self.snapshot.lock() else {
            return Arc::default();
        };
        self.ready
            .wait_timeout_while(
                snapshot,
                deadline.saturating_duration_since(Instant::now()),
                |snapshot| !snapshot.complete,
            )
            .map(|(snapshot, _)| Arc::clone(&snapshot))
            .unwrap_or_default()
    }

    fn publish(&self, cache: &HashMap<PathBuf, CachedFile>, complete: bool) {
        let mut sessions = merge(cache, complete);
        sessions.sort_by(|a, b| {
            b.last_active_at_ms
                .cmp(&a.last_active_at_ms)
                .then_with(|| a.key.cmp(&b.key))
        });
        if let Ok(mut snapshot) = self.snapshot.lock() {
            *snapshot = Arc::new(Snapshot { sessions, complete });
            self.ready.notify_all();
        }
    }
}

/// Fixed harness layout, newest buckets first. No symlink traversal or unrelated
/// directory search; a Claude session's nested subagents are below this depth.
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
        } else if depth == 0
            && metadata.is_file()
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

fn stamp(metadata: &std::fs::Metadata) -> Stamp {
    Stamp {
        modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        size: metadata.len(),
    }
}

/// History contributes prompts, while a record supplies the current place and
/// resume identity. Claude's id deliberately survives moves between projects.
fn merge(cache: &HashMap<PathBuf, CachedFile>, complete: bool) -> Vec<Arc<Session>> {
    let excluded: HashSet<_> = cache
        .values()
        .flat_map(|entry| entry.data.excluded.iter())
        .collect();
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
    let mut sessions: BTreeMap<_, Session> = BTreeMap::new();
    for (_, source) in sources {
        if let Some(session) = sessions.get_mut(&source.key) {
            session.cwd = source.cwd.clone().or(session.cwd.take());
            session.branch = source.branch.clone().or(session.branch.take());
            session.title = source.title.clone().or(session.title.take());
            session.record_path = source.record_path.clone().or(session.record_path.take());
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
            !excluded.contains(&session.key) && (complete || session.record_path.is_some())
        })
        .map(|mut session| {
            session.prompts.sort_by_key(|prompt| prompt.at);
            let mut seen = HashSet::new();
            session
                .prompts
                .retain(|prompt| seen.insert((prompt.at / 1000, Arc::clone(&prompt.text))));
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
