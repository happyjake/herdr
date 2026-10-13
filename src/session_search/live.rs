//! Active ownership from server data only. This accessor has no runtime access.

use crate::agent_resume::AgentSessionRefKind;
use crate::api::schema::{SearchSessionKey, SearchSessionKeyKind};
use crate::app::AppState;

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct ActiveSession {
    pub pane_id: String,
    pub key: SearchSessionKey,
}

pub(crate) fn active_sessions(state: &AppState) -> Vec<ActiveSession> {
    let mut sessions = Vec::new();
    for workspace in &state.workspaces {
        for tab in &workspace.tabs {
            for (pane_id, pane) in &tab.panes {
                let Some(terminal) = state.terminals.get(&pane.attached_terminal_id) else {
                    continue;
                };
                let Some(agent) = terminal.effective_agent_label() else {
                    continue;
                };
                let reference = terminal
                    .hook_authority
                    .as_ref()
                    .filter(|authority| authority.agent_label == agent)
                    .and_then(|authority| authority.session_ref.as_ref())
                    .or_else(|| {
                        terminal
                            .persisted_agent_session
                            .as_ref()
                            .filter(|session| session.agent == agent)
                            .map(|session| &session.session_ref)
                    });
                let (Some(reference), Some(number)) =
                    (reference, workspace.public_pane_number(*pane_id))
                else {
                    continue;
                };
                sessions.push(ActiveSession {
                    pane_id: crate::workspace::public_pane_id_for_number(&workspace.id, number),
                    key: SearchSessionKey {
                        agent: agent.into(),
                        kind: match reference.kind {
                            AgentSessionRefKind::Id => SearchSessionKeyKind::Id,
                            AgentSessionRefKind::Path => SearchSessionKeyKind::Path,
                        },
                        value: reference.value.clone(),
                    },
                });
            }
        }
    }
    sessions
}
