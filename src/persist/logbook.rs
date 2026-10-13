//! Append-only session history, owned by the server and kept beside session.json.
//!
//! `logbook.jsonl` is never rotated or cleared by layout persistence. Each line
//! is an [`Event`]. [`read`] supplies the typed file interface; no API method or
//! harness record is involved. Repeated reports are deduplicated by pane and
//! session key, while changes to label text or provenance remain separate events.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

mod background;
pub(crate) use background::BackgroundWriter;
#[cfg(unix)]
pub(crate) use background::DRAIN_TIMEOUT;

/// Resume identity without the reporting hook's source or record-file hint.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionKey {
    pub agent: String,
    pub kind: crate::agent_resume::AgentSessionRefKind,
    pub value: String,
}

impl From<&crate::agent_resume::PersistedAgentSession> for SessionKey {
    fn from(session: &crate::agent_resume::PersistedAgentSession) -> Self {
        Self {
            agent: session.agent.clone(),
            kind: session.session_ref.kind,
            value: session.session_ref.value.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LabelSource {
    Editor,
    Person,
}

/// `at` is Unix time in milliseconds. A null label ends the previous label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    SessionReported {
        pane: String,
        terminal: crate::terminal::TerminalId,
        key: SessionKey,
        cwd: PathBuf,
        at: u64,
    },
    LabelWorn {
        key: SessionKey,
        pane: String,
        label: Option<String>,
        source: LabelSource,
        at: u64,
    },
    PaneClosed {
        pane: String,
        at: u64,
    },
    Parent {
        child: SessionKey,
        parent: SessionKey,
    },
}

pub fn path() -> PathBuf {
    crate::session::data_dir().join("logbook.jsonl")
}

/// Read complete events in file order. Missing files are empty histories.
/// Invalid lines are skipped, including a torn tail and tails subsequently
/// delimited by an append. Filesystem errors are returned to the caller.
// Kept as the typed consumer interface; the writer streams replay without collecting.
#[cfg_attr(not(test), allow(dead_code))]
pub fn read(path: &Path) -> io::Result<Vec<Event>> {
    let mut events = Vec::new();
    for_each_event(path, |event| events.push(event))?;
    Ok(events)
}

pub(crate) fn for_each_event(path: &Path, mut visit: impl FnMut(Event)) -> io::Result<()> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    for (index, line) in BufReader::new(file).split(b'\n').enumerate() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        match serde_json::from_slice(&line) {
            Ok(event) => visit(event),
            Err(err) => {
                tracing::warn!(line = index + 1, %err, "skipping incomplete or invalid logbook event")
            }
        }
    }
    Ok(())
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

pub(crate) struct Writer {
    path: PathBuf,
    sessions: HashSet<(String, SessionKey)>,
    labels: HashMap<(String, SessionKey), (Option<String>, LabelSource)>,
    parents: HashSet<(SessionKey, SessionKey)>,
}

impl Writer {
    pub(crate) fn open(
        path: &Path,
        seed: &[Event],
        live_panes: &HashSet<String>,
    ) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => return Err(io::Error::other("logbook is not a regular file")),
            Err(err) if err.kind() == io::ErrorKind::NotFound => seed_once(path, seed)?,
            Err(err) => return Err(err),
        }
        let mut writer = Self {
            path: path.to_owned(),
            sessions: HashSet::new(),
            labels: HashMap::new(),
            parents: HashSet::new(),
        };
        for_each_event(path, |event| writer.remember(&event))?;
        // A predecessor may still be writing a close after a handoff timeout.
        // The imported layout, not that delayed event, determines which pane
        // identities can retain deduplication entries in the new writer.
        writer
            .sessions
            .retain(|(pane, _)| live_panes.contains(pane));
        writer
            .labels
            .retain(|(pane, _), _| live_panes.contains(pane));
        Ok(writer)
    }

    pub(crate) fn append(&mut self, event: &Event) -> io::Result<()> {
        match event {
            Event::Parent { child, parent }
                if self.parents.contains(&(child.clone(), parent.clone())) =>
            {
                return Ok(())
            }
            Event::SessionReported { pane, key, .. }
                if self.sessions.contains(&(pane.clone(), key.clone())) =>
            {
                return Ok(())
            }
            Event::LabelWorn {
                pane,
                key,
                label,
                source,
                ..
            } if self.labels.get(&(pane.clone(), key.clone()))
                == Some(&(label.clone(), *source)) =>
            {
                return Ok(())
            }
            _ => {}
        }
        let mut line = serde_json::to_vec(event)?;
        line.push(b'\n');
        if !std::fs::symlink_metadata(&self.path)?.is_file() {
            return Err(io::Error::other("logbook is not a regular file"));
        }
        let mut file = File::options().read(true).append(true).open(&self.path)?;
        if file.metadata()?.len() > 0 {
            file.seek(SeekFrom::End(-1))?;
            let mut last = [0];
            file.read_exact(&mut last)?;
            if last[0] != b'\n' {
                file.write_all(b"\n")?;
            }
        }
        file.write_all(&line)?;
        self.remember(event);
        Ok(())
    }

    fn remember(&mut self, event: &Event) {
        match event {
            Event::SessionReported { pane, key, .. } => {
                self.sessions.insert((pane.clone(), key.clone()));
            }
            Event::LabelWorn {
                pane,
                key,
                label,
                source,
                ..
            } => {
                self.labels
                    .insert((pane.clone(), key.clone()), (label.clone(), *source));
            }
            Event::PaneClosed { pane, .. } => {
                self.sessions
                    .retain(|(recorded_pane, _)| recorded_pane != pane);
                self.labels
                    .retain(|(recorded_pane, _), _| recorded_pane != pane);
            }
            Event::Parent { child, parent } => {
                self.parents.insert((child.clone(), parent.clone()));
            }
        }
    }
}

/// Publish a fully written seed, even an empty one, as the initialization marker.
/// A crash before publication leaves only a temporary file; a competing opener
/// cannot replace a logbook that has already been published.
fn seed_once(path: &Path, seed: &[Event]) -> io::Result<()> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pending = path.with_extension(format!("{}-{stamp}.pending", std::process::id()));
    let mut file = crate::platform::create_config_temporary(&pending, true)?;
    let result = (|| {
        for event in seed {
            serde_json::to_writer(&mut file, event)?;
            file.write_all(b"\n")?;
        }
        file.sync_all()?;
        drop(file);
        match std::fs::hard_link(&pending, path) {
            Ok(()) => {
                if let Some(parent) = path.parent() {
                    crate::platform::sync_parent_directory(parent)?;
                }
                Ok(())
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(err) => Err(err),
        }
    })();
    let _ = std::fs::remove_file(pending);
    result
}
