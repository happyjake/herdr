//! Fold the typed logbook on the catalog worker, retaining metadata rather than events.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use super::records::Session;
use crate::agent_resume::{AgentSessionRef, AgentSessionRefKind};
use crate::api::schema::{
    CloseDebrief, SearchSessionKey, SearchSessionKeyKind, SessionClosed, SessionLabel,
    SessionLabelSource,
};
use crate::persist::logbook::{self, CloseSession, Event, LabelSource, SessionKey};

pub(super) fn read(path: &Path) -> std::io::Result<BTreeMap<SearchSessionKey, Session>> {
    let mut sessions = BTreeMap::new();
    let mut panes = HashMap::new();
    let mut labels = HashMap::new();
    logbook::for_each_event(path, |event| match event {
        Event::SessionReported {
            pane, key, cwd, at, ..
        } => {
            let key = search_key(key);
            if panes.get(&pane) != Some(&key) {
                close_label(&pane, at, &mut labels, &mut sessions);
                if let Some(previous) = panes.get(&pane).and_then(|key| sessions.get_mut(key)) {
                    previous.at(at);
                }
            }
            panes.insert(pane, key.clone());
            let session = ensure_session(&mut sessions, key);
            session.cwd = cwd.to_str().map(str::to_owned);
            session.reported = true;
            session.at(at);
        }
        Event::LabelWorn {
            key,
            pane,
            label,
            source,
            at,
        } => {
            close_label(&pane, at, &mut labels, &mut sessions);
            let key = search_key(key);
            let session = ensure_session(&mut sessions, key.clone());
            session.at(at);
            if let Some(label) = label {
                labels.insert(pane.clone(), (key, session.labels.len()));
                session.labels.push(SessionLabel {
                    label,
                    source: match source {
                        LabelSource::Editor => SessionLabelSource::Editor,
                        LabelSource::Person => SessionLabelSource::Person,
                    },
                    pane_id: pane,
                    from_ms: at,
                    to_ms: None,
                });
            }
        }
        Event::PaneClosed {
            pane,
            by,
            reason,
            age_secs,
            debrief,
            session,
            at,
        } => {
            close_label(&pane, at, &mut labels, &mut sessions);
            let pane_key = panes.remove(&pane);
            if let Some(close_session) = session {
                let key = close_search_key(&sessions, close_session);
                if pane_key.is_none() {
                    panes.retain(|_, current| current != &key);
                }
                let target = sessions
                    .entry(key.clone())
                    .or_insert_with(|| Session::new(key, None));
                apply_close(target, by, reason, age_secs, debrief, at);
            } else if let Some(key) = pane_key {
                if let Some(target) = sessions.get_mut(&key) {
                    apply_close(target, by, reason, age_secs, debrief, at);
                }
            }
        }
        Event::Parent { child, parent } => {
            let key = search_key(child);
            ensure_session(&mut sessions, key).parent = Some(search_key(parent));
        }
    })?;
    for session in sessions.values_mut() {
        session.labels.sort_by_key(|label| label.from_ms);
    }
    Ok(sessions)
}

fn ensure_session<'a>(
    sessions: &'a mut BTreeMap<SearchSessionKey, Session>,
    key: SearchSessionKey,
) -> &'a mut Session {
    if !sessions.contains_key(&key) {
        let alternate_kind = match key.kind {
            SearchSessionKeyKind::Id => Some(SearchSessionKeyKind::Path),
            SearchSessionKeyKind::Path => Some(SearchSessionKeyKind::Id),
            SearchSessionKeyKind::Day => None,
        };
        if let Some(alternate_kind) = alternate_kind {
            let alternate = SearchSessionKey {
                agent: key.agent.clone(),
                kind: alternate_kind,
                value: key.value.clone(),
            };
            if sessions
                .get(&alternate)
                .is_some_and(|session| !session.reported && session.labels.is_empty())
            {
                if let Some(mut previous) = sessions.remove(&alternate) {
                    previous.key = key.clone();
                    sessions.insert(key.clone(), previous);
                }
            }
        }
    }
    sessions
        .entry(key.clone())
        .or_insert_with(|| Session::new(key, None))
}

fn close_search_key(
    sessions: &BTreeMap<SearchSessionKey, Session>,
    close: CloseSession,
) -> SearchSessionKey {
    for kind in [SearchSessionKeyKind::Id, SearchSessionKeyKind::Path] {
        let candidate = SearchSessionKey {
            agent: close.agent.clone(),
            kind,
            value: close.id.clone(),
        };
        if sessions.contains_key(&candidate) {
            return candidate;
        }
    }
    let kind = if matches!(close.agent.as_str(), "pi" | "omp")
        && AgentSessionRef::path(&close.id).is_some()
    {
        SearchSessionKeyKind::Path
    } else {
        SearchSessionKeyKind::Id
    };
    SearchSessionKey {
        agent: close.agent,
        kind,
        value: close.id,
    }
}

fn apply_close(
    session: &mut Session,
    by: Option<logbook::CloseBy>,
    reason: Option<String>,
    age_secs: Option<u64>,
    debrief: Option<logbook::CloseDebrief>,
    at: u64,
) {
    session.at(at);
    if session.closed_at_ms.is_none_or(|previous| at >= previous) {
        session.closed_at_ms = Some(at);
        session.closed = by.map(|by| SessionClosed {
            by,
            reason,
            age_secs,
            at_ms: at,
            debrief: debrief.map(|debrief| CloseDebrief {
                outcome: debrief.outcome,
                memories: debrief.memories,
            }),
        });
    }
}

fn close_label(
    pane: &str,
    at: u64,
    labels: &mut HashMap<String, (SearchSessionKey, usize)>,
    sessions: &mut BTreeMap<SearchSessionKey, Session>,
) {
    if let Some((key, index)) = labels.remove(pane) {
        if let Some(session) = sessions.get_mut(&key) {
            session.labels[index].to_ms = Some(at);
            session.at(at);
        }
    }
}

fn search_key(key: SessionKey) -> SearchSessionKey {
    SearchSessionKey {
        agent: key.agent,
        kind: match key.kind {
            AgentSessionRefKind::Id => SearchSessionKeyKind::Id,
            AgentSessionRefKind::Path => SearchSessionKeyKind::Path,
        },
        value: key.value,
    }
}
