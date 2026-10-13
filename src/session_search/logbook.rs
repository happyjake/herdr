//! Fold the typed logbook on the catalog worker, retaining metadata rather than events.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use super::records::Session;
use crate::agent_resume::AgentSessionRefKind;
use crate::api::schema::{
    SearchSessionKey, SearchSessionKeyKind, SessionLabel, SessionLabelSource,
};
use crate::persist::logbook::{self, Event, LabelSource, SessionKey};

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
            let session = sessions
                .entry(key.clone())
                .or_insert_with(|| Session::new(key, None));
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
            let session = sessions
                .entry(key.clone())
                .or_insert_with(|| Session::new(key.clone(), None));
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
        Event::PaneClosed { pane, at } => {
            close_label(&pane, at, &mut labels, &mut sessions);
            if let Some(key) = panes.remove(&pane) {
                if let Some(session) = sessions.get_mut(&key) {
                    session.at(at);
                }
            }
        }
        Event::Parent { child, parent } => {
            let key = search_key(child);
            sessions
                .entry(key.clone())
                .or_insert_with(|| Session::new(key, None))
                .parent = Some(search_key(parent));
        }
    })?;
    for session in sessions.values_mut() {
        session.labels.sort_by_key(|label| label.from_ms);
    }
    Ok(sessions)
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
