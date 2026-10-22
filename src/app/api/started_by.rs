use crate::api::schema::PaneStartedBy;
use crate::api::{PeerInPane, RequestPeer};
use crate::app::App;

impl App {
    pub(super) fn set_workspace_root_starter(
        &mut self,
        ws_idx: usize,
        started_by: Option<PaneStartedBy>,
    ) {
        let ws = &self.state.workspaces[ws_idx];
        let root = ws.tabs[ws.active_tab].root_pane;
        if let Some(pane) = ws.pane_state(root) {
            if let Some(terminal) = self.state.terminals.get_mut(&pane.attached_terminal_id) {
                terminal.started_by = started_by;
            }
        }
    }

    /// A caller's environment is a claim, not authority. Only positively
    /// placed local peers in an agent pane can attribute a new terminal.
    pub(super) fn verified_pane_starter(
        &self,
        caller_pane_id: Option<&str>,
        peer: Option<&RequestPeer>,
    ) -> Option<PaneStartedBy> {
        let peer = peer?;
        let (ws_idx, pane_id) = self.parse_pane_id(caller_pane_id?)?;
        let pane = self.state.workspaces.get(ws_idx)?.pane_state(pane_id)?;
        let terminal = self.state.terminals.get(&pane.attached_terminal_id)?;
        terminal.effective_agent_label()?;
        let (runtime, _) = self.lookup_runtime(ws_idx, pane_id)?;
        let shell_pid = runtime.child_pid()?;
        let pane_tty = crate::platform::process_tty(shell_pid).flatten();
        // Agent command runners can allocate a nested PTY. For creation,
        // verified descent still identifies the starter despite a different
        // controlling terminal; agent reports retain their stricter check.
        let descends_from_pane = peer
            .placement
            .ancestry
            .as_ref()
            .is_some_and(|parents| parents.contains(&shell_pid));
        if !descends_from_pane && peer.in_pane(shell_pid, pane_tty) != PeerInPane::Inside {
            return None;
        }
        Some(PaneStartedBy::Pane {
            pane_id: self.public_pane_id(ws_idx, pane_id)?,
        })
    }
}
