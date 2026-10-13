//! Combine structural parentage with this server's historical and current pane ownership.

use std::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap, HashMap, HashSet};
use std::sync::Arc;

use super::records::Session;
use crate::api::schema::SearchSessionKey;

#[derive(Default)]
pub(super) struct Folding<'a> {
    pub headless: HashSet<&'a SearchSessionKey>,
    pub parents: HashMap<&'a SearchSessionKey, &'a SearchSessionKey>,
    pub children: HashMap<&'a SearchSessionKey, u32>,
}

impl<'a> Folding<'a> {
    pub fn new(
        sessions: &'a [Arc<Session>],
        live: &HashMap<SearchSessionKey, String>,
        has_time: &dyn Fn() -> bool,
    ) -> Option<Self> {
        let mut folded = Self::default();
        let mut places: HashMap<&str, Vec<&Session>> = HashMap::new();
        for session in sessions {
            if !has_time() {
                return None;
            }
            if !session.reported && !live.contains_key(&session.key) {
                folded.headless.insert(&session.key);
            }
            if let Some(parent) = session
                .parent
                .as_ref()
                .filter(|parent| **parent != session.key)
            {
                folded.parents.insert(&session.key, parent);
            }
            if let Some(cwd) = session.cwd.as_deref().filter(|cwd| !cwd.is_empty()) {
                places.entry(cwd).or_default().push(session);
            }
        }
        for mut sessions in places.into_values() {
            sessions.sort_by(|a, b| {
                a.started_at_ms
                    .cmp(&b.started_at_ms)
                    .then_with(|| a.key.cmp(&b.key))
            });
            let hosts: Vec<_> = sessions
                .iter()
                .copied()
                .filter(|session| !folded.headless.contains(&session.key))
                .collect();
            let mut hosts = hosts.into_iter().peekable();
            let mut active = BTreeSet::new();
            let mut endings = BinaryHeap::new();
            for session in sessions {
                if !has_time() {
                    return None;
                }
                if !folded.headless.contains(&session.key)
                    || folded.parents.contains_key(&session.key)
                {
                    continue;
                }
                // Sweep inclusive windows once per place, rather than comparing
                // every headless record with every pane-hosted session.
                while hosts
                    .peek()
                    .is_some_and(|host| host.started_at_ms <= session.started_at_ms)
                {
                    let host = hosts.next()?;
                    active.insert((host.started_at_ms, &host.key));
                    endings.push(Reverse((
                        host.last_active_at_ms,
                        host.started_at_ms,
                        &host.key,
                    )));
                }
                while endings
                    .peek()
                    .is_some_and(|Reverse((end, _, _))| *end < session.started_at_ms)
                {
                    let Reverse((_, start, key)) = endings.pop()?;
                    active.remove(&(start, key));
                }
                // Prefer the most recently started eligible host; key order
                // breaks simultaneous starts deterministically.
                if let Some((_, parent)) = active.last() {
                    folded.parents.insert(&session.key, parent);
                }
            }
        }
        for parent in folded.parents.values() {
            if !has_time() {
                return None;
            }
            let count = folded.children.entry(parent).or_default();
            *count = count.saturating_add(1);
        }
        Some(folded)
    }
}
