use std::collections::HashMap;

use crate::api::schema::AgentStatus;
use crate::detect::AgentState;
use crate::layout::PaneId;
use crate::pane::AgentStatusProjection;
use crate::terminal::{TerminalId, TerminalState};

use super::{Tab, Workspace};

/// Detail info for a single pane, used by the agent detail panel.
pub struct PaneDetail {
    pub pane_id: PaneId,
    pub tab_idx: usize,
    pub agent_kind_label: Option<String>,
    pub state: AgentState,
    pub seen: bool,
    pub last_agent_state_change_seq: Option<u64>,
    pub tokens: HashMap<String, String>,
}

impl Tab {
    /// Aggregate the selected status fact; desktop pane details below stay live.
    pub fn agent_status_from(
        &self,
        terminals: &HashMap<TerminalId, TerminalState>,
        projection: AgentStatusProjection,
    ) -> AgentStatus {
        aggregate_status(self.panes.values(), terminals, projection)
    }

    fn pane_details(
        &self,
        terminals: &HashMap<TerminalId, TerminalState>,
        tab_idx: usize,
    ) -> Vec<PaneDetail> {
        self.layout
            .pane_ids()
            .iter()
            .filter_map(|id| {
                let pane = self.panes.get(id)?;
                let terminal = terminals.get(&pane.attached_terminal_id)?;
                let agent_kind_label = terminal.effective_agent_label().map(str::to_string);
                if terminal.agent_name.is_none() && agent_kind_label.is_none() {
                    return None;
                }
                Some(PaneDetail {
                    pane_id: *id,
                    tab_idx,
                    agent_kind_label,
                    state: terminal.state,
                    seen: pane.seen,
                    last_agent_state_change_seq: terminal.last_agent_state_change_seq,
                    tokens: terminal.metadata_tokens.values(),
                })
            })
            .collect()
    }
}

/// Aggregate using the same projection as the individual panes.
fn aggregate_status<'a>(
    panes: impl Iterator<Item = &'a crate::pane::PaneState>,
    terminals: &HashMap<TerminalId, TerminalState>,
    projection: AgentStatusProjection,
) -> AgentStatus {
    let now = crate::pane::unix_now_secs();
    panes
        .filter_map(|pane| {
            let terminal = terminals.get(&pane.attached_terminal_id)?;
            let live = match (terminal.state, pane.seen) {
                (AgentState::Working, _) => AgentStatus::Working,
                (AgentState::Blocked, _) => AgentStatus::Blocked,
                (AgentState::Idle, false) => AgentStatus::Done,
                (AgentState::Idle, true) => AgentStatus::Idle,
                (AgentState::Unknown, _) => AgentStatus::Unknown,
            };
            Some(projection.pair(pane, live, now).0)
        })
        .max_by_key(|status| match status {
            AgentStatus::Blocked => 4,
            AgentStatus::Done => 3,
            AgentStatus::Working => 2,
            AgentStatus::Idle => 1,
            AgentStatus::Unknown => 0,
        })
        .unwrap_or(AgentStatus::Unknown)
}

impl Workspace {
    pub fn agent_status_from(
        &self,
        terminals: &HashMap<TerminalId, TerminalState>,
        projection: AgentStatusProjection,
    ) -> AgentStatus {
        aggregate_status(
            self.tabs.iter().flat_map(|tab| tab.panes.values()),
            terminals,
            projection,
        )
    }

    pub fn pane_details(&self, terminals: &HashMap<TerminalId, TerminalState>) -> Vec<PaneDetail> {
        self.tabs
            .iter()
            .enumerate()
            .flat_map(|(tab_idx, tab)| tab.pane_details(terminals, tab_idx))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use ratatui::layout::Direction;

    use super::*;
    use crate::detect::Agent;

    fn terminal_for_pane(ws: &Workspace, pane_id: PaneId) -> TerminalState {
        TerminalState::new(ws.terminal_id(pane_id).unwrap().clone(), "/tmp".into())
    }

    #[test]
    fn aggregate_state_all_unknown() {
        let ws = Workspace::test_new("test");
        let mut terminals = HashMap::new();
        let root = ws.tabs[0].root_pane;
        let terminal = terminal_for_pane(&ws, root);
        terminals.insert(terminal.id.clone(), terminal);
        let status = ws.agent_status_from(&terminals, AgentStatusProjection::Reported);
        assert_eq!(status, AgentStatus::Unknown);
    }

    #[test]
    fn aggregate_state_priority() {
        let mut ws = Workspace::test_new("test");
        let id2 = ws.test_split(Direction::Horizontal);
        let root_id = ws.tabs[0]
            .panes
            .keys()
            .find(|id| **id != id2)
            .copied()
            .unwrap();
        let mut terminals = HashMap::new();
        let mut root_terminal = terminal_for_pane(&ws, root_id);
        root_terminal.state = AgentState::Idle;
        terminals.insert(root_terminal.id.clone(), root_terminal);
        let mut second_terminal = terminal_for_pane(&ws, id2);
        second_terminal.state = AgentState::Working;
        terminals.insert(second_terminal.id.clone(), second_terminal);

        let status = ws.agent_status_from(&terminals, AgentStatusProjection::Reported);

        assert_eq!(status, AgentStatus::Working);
    }

    #[test]
    fn aggregate_state_done_unseen_beats_working() {
        let mut ws = Workspace::test_new("test");
        let id2 = ws.test_split(Direction::Horizontal);
        let root_id = ws.tabs[0]
            .panes
            .keys()
            .find(|id| **id != id2)
            .copied()
            .unwrap();
        let mut terminals = HashMap::new();
        let mut root_terminal = terminal_for_pane(&ws, root_id);
        root_terminal.state = AgentState::Idle;
        terminals.insert(root_terminal.id.clone(), root_terminal);
        let mut second_terminal = terminal_for_pane(&ws, id2);
        second_terminal.state = AgentState::Working;
        terminals.insert(second_terminal.id.clone(), second_terminal);
        let root = ws.tabs[0].panes.get_mut(&root_id).unwrap();
        root.seen = false;

        let status = ws.agent_status_from(&terminals, AgentStatusProjection::Reported);

        assert_eq!(status, AgentStatus::Done);
    }

    #[test]
    fn reported_aggregates_keep_captured_attention_while_desktop_details_stay_live() {
        let mut ws = Workspace::test_new("restored");
        let other = ws.test_split(Direction::Horizontal);
        let root = ws.tabs[0].root_pane;
        let mut terminals = HashMap::new();
        for (pane_id, status) in [(root, AgentStatus::Done), (other, AgentStatus::Working)] {
            let mut terminal = terminal_for_pane(&ws, pane_id);
            terminal.agent_name = Some("restored-agent".into());
            let pane = crate::pane::PaneState::restored(
                terminal.id.clone(),
                status,
                1_000,
                crate::pane::unix_now_secs(),
            );
            ws.tabs[0].panes.insert(pane_id, pane);
            terminals.insert(terminal.id.clone(), terminal);
        }
        assert_eq!(
            ws.agent_status_from(&terminals, AgentStatusProjection::Reported),
            AgentStatus::Done
        );
        assert_eq!(
            ws.tabs[0].agent_status_from(&terminals, AgentStatusProjection::Reported),
            AgentStatus::Done
        );
        assert!(ws
            .pane_details(&terminals)
            .iter()
            .all(|pane| pane.state == AgentState::Unknown));
        ws.pane_state_mut(other)
            .unwrap()
            .resolve_agent_status_at(AgentStatus::Blocked, crate::pane::unix_now_secs());
        terminals
            .get_mut(ws.terminal_id(other).unwrap())
            .unwrap()
            .state = AgentState::Blocked;
        assert_eq!(
            ws.agent_status_from(&terminals, AgentStatusProjection::Reported),
            AgentStatus::Blocked
        );
    }

    #[test]
    fn pane_details_lists_codebuddy_detected_agent_label() {
        let mut ws = Workspace::test_new("test");
        let root_pane = ws.tabs[0].root_pane;
        ws.tabs[0].panes.get_mut(&root_pane).unwrap().seen = false;
        let mut terminals = HashMap::new();
        let mut terminal = terminal_for_pane(&ws, root_pane);
        terminal.set_detected_state(Some(Agent::Codebuddy), AgentState::Idle);
        terminals.insert(terminal.id.clone(), terminal);

        let details = ws.pane_details(&terminals);
        assert_eq!(details.len(), 1);
        assert_eq!(details[0].agent_kind_label.as_deref(), Some("codebuddy"));
        assert_eq!(details[0].state, AgentState::Idle);
        assert!(!details[0].seen);
    }

    #[test]
    fn pane_details_use_tab_vector_index_not_stable_public_tab_number() {
        let mut ws = Workspace::test_new("test");
        let removed_tab = ws.test_add_tab(Some("removed"));
        let survivor_tab = ws.test_add_tab(Some("survivor"));
        let survivor_pane = ws.tabs[survivor_tab].root_pane;
        assert!(ws.close_tab(removed_tab));

        let mut terminals = HashMap::new();
        let mut terminal = terminal_for_pane(&ws, survivor_pane);
        terminal.detected_agent = Some(Agent::Codex);
        terminals.insert(terminal.id.clone(), terminal);

        let details = ws.pane_details(&terminals);
        let survivor = details
            .iter()
            .find(|detail| detail.pane_id == survivor_pane)
            .expect("surviving tab agent should be listed");

        assert_eq!(ws.tabs[1].number, 3);
        assert_eq!(survivor.tab_idx, 1);
    }
}
