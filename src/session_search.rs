//! Read-only session catalog, independent of panes and presentation.

mod catalog;
mod matching;
mod records;
mod zstd;

pub(crate) use catalog::{Catalog, Desk};

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::agent_resume::{self, AgentSessionRef, AgentSessionRefKind};
use crate::api::schema::{
    PaneInfo, SearchSessionKey, SearchSessionKeyKind, ServerSearchSessionsParams, SessionHit,
    SessionPlaceState, SessionResume, SessionResumeBlocked, SessionState,
};
use crate::prompt_trail;

#[derive(Clone, Default)]
pub(crate) struct SearchResult {
    pub sessions: Vec<SessionHit>,
    pub scanned: u64,
    pub partial: bool,
}

impl Catalog {
    pub(crate) fn search(
        &self,
        params: &ServerSearchSessionsParams,
        panes: &[PaneInfo],
        deadline: Instant,
        now: &dyn Fn() -> Instant,
        progress: &Mutex<SearchResult>,
    ) {
        let live: HashMap<_, _> = panes
            .iter()
            .filter_map(|pane| {
                let session = pane.agent_session.as_ref()?;
                Some((
                    SearchSessionKey {
                        agent: session.agent.clone(),
                        kind: match session.kind {
                            AgentSessionRefKind::Id => SearchSessionKeyKind::Id,
                            AgentSessionRefKind::Path => SearchSessionKeyKind::Path,
                        },
                        value: session.value.clone(),
                    },
                    pane.pane_id.clone(),
                ))
            })
            .collect();
        // Leave time to search the published rows even while a cold index is
        // still filling. No record I/O is performed by the query itself.
        let snapshot = self.snapshot_until(
            deadline
                .checked_sub(Duration::from_millis(100))
                .unwrap_or(deadline),
        );
        let query = matching::Query::new(params);
        let limit = params.limit.unwrap_or(50).min(200) as usize;
        for session in &snapshot.sessions {
            if now() >= deadline {
                break;
            }
            if let Ok(mut result) = progress.lock() {
                result.scanned += 1;
            }
            let Some((score, why)) = query.matches(session, &|| now() < deadline) else {
                continue;
            };
            if limit == 0 {
                continue;
            }
            let worth_returning = progress.lock().is_ok_and(|result| {
                result.sessions.len() < limit
                    || result.sessions.last().is_some_and(|last| {
                        score
                            .total_cmp(&last.score)
                            .reverse()
                            .then_with(|| last.last_active_at_ms.cmp(&session.last_active_at_ms))
                            .then_with(|| session.key.cmp(&last.key))
                            .is_lt()
                    })
            });
            if !worth_returning {
                continue;
            }
            // Existence checks and command rendering happen only for candidates
            // that can enter the bounded result set, outside the progress lock.
            let mut row = hit(session, live.get(&session.key));
            row.score = score;
            row.why = why;
            if let Ok(mut result) = progress.lock() {
                let index = result
                    .sessions
                    .partition_point(|previous| rank(previous, &row).is_le());
                result.sessions.insert(index, row);
                result.sessions.truncate(limit);
            }
        }
        if let Ok(mut result) = progress.lock() {
            result.partial = !snapshot.complete || now() >= deadline;
        }
    }
}

fn rank(a: &SessionHit, b: &SessionHit) -> std::cmp::Ordering {
    b.score
        .total_cmp(&a.score)
        .then_with(|| b.last_active_at_ms.cmp(&a.last_active_at_ms))
        .then_with(|| a.key.cmp(&b.key))
}

fn hit(session: &records::Session, live_pane_id: Option<&String>) -> SessionHit {
    let first = session
        .prompts
        .first()
        .map_or("", |prompt| prompt.text.as_ref());
    let present = session
        .cwd
        .as_ref()
        .is_some_and(|cwd| Path::new(cwd).is_dir());
    let nearest_existing = if present {
        None
    } else {
        session.cwd.as_deref().and_then(|cwd| {
            Path::new(cwd)
                .ancestors()
                .skip(1)
                .find(|ancestor| ancestor.is_dir())
                .and_then(|ancestor| ancestor.to_str())
                .map(str::to_string)
        })
    };
    let state = if live_pane_id.is_some() {
        SessionState::Live
    } else if session.record_path.is_some() {
        SessionState::Ended
    } else {
        SessionState::RecordGone
    };
    // Claude and Codex find ids across working directories; pi uses its record
    // path. The command itself always comes from the existing resume planner.
    let resume = (state == SessionState::Ended)
        .then(|| {
            let cwd = if present {
                session.cwd.as_ref()
            } else {
                nearest_existing.as_ref()
            }?;
            let reference = match session.key.kind {
                SearchSessionKeyKind::Id => AgentSessionRef::id(&session.key.value)?,
                SearchSessionKeyKind::Path => AgentSessionRef::path(&session.key.value)?,
                SearchSessionKeyKind::Day => return None,
            };
            let plan = agent_resume::plan(
                &format!("herdr:{}", session.key.agent),
                &session.key.agent,
                &reference,
            )?;
            Some(SessionResume {
                command: crate::platform::interactive_shell_command(&plan.argv, "sh")?,
                cwd: cwd.clone(),
            })
        })
        .flatten();
    let resume_blocked = if resume.is_some() {
        None
    } else {
        Some(match state {
            SessionState::Live => SessionResumeBlocked::Live,
            SessionState::RecordGone => SessionResumeBlocked::RecordGone,
            _ => SessionResumeBlocked::FolderGone,
        })
    };
    SessionHit {
        key: session.key.clone(),
        agent: session.key.agent.clone(),
        cwd: session.cwd.clone(),
        place_state: if present {
            SessionPlaceState::Present
        } else {
            SessionPlaceState::Gone
        },
        nearest_existing,
        branch: session.branch.clone(),
        title: session.title.clone(),
        first_prompt: prompt_trail::cut(first, 400),
        recent_prompts: session
            .prompts
            .iter()
            .rev()
            .take(3)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|prompt| prompt_trail::cut(&prompt.text, 240))
            .collect(),
        started_at_ms: session.started_at_ms,
        last_active_at_ms: session.last_active_at_ms,
        labels: Vec::new(),
        state,
        live_pane_id: live_pane_id.cloned(),
        outside: None,
        resume,
        resume_blocked,
        headless: false,
        parent: None,
        subagent_count: 0,
        why: None,
        score: 0.0,
    }
}
