use crate::api::schema::PaneStartedBy;
use crate::api::{PeerInPane, RequestPeer};
use crate::app::started_by::StarterLink;
use crate::app::App;

impl App {
    pub(super) fn set_workspace_root_starter(&mut self, ws_idx: usize, link: Option<StarterLink>) {
        let ws = &self.state.workspaces[ws_idx];
        let root = ws.tabs[ws.active_tab].root_pane;
        if let Some(pane) = ws.pane_state(root) {
            if let Some(terminal) = self.state.terminals.get_mut(&pane.attached_terminal_id) {
                StarterLink::attach(link, terminal);
            }
        }
    }

    /// A caller's environment is a claim, not authority. Only positively
    /// placed local peers in an agent pane can attribute a new terminal.
    pub(super) fn verified_pane_starter(
        &self,
        caller_pane_id: Option<&str>,
        peer: Option<&RequestPeer>,
    ) -> Option<StarterLink> {
        let caller_pane_id = caller_pane_id?;
        let peer = peer?;
        let (ws_idx, pane_id) = self
            .parse_pane_id(caller_pane_id)
            .filter(|&(ws_idx, pane_id)| self.pane_contains_peer(ws_idx, pane_id, peer))
            .or_else(|| self.pane_containing_peer(peer))?;
        let pane = self.state.workspaces.get(ws_idx)?.pane_state(pane_id)?;
        let terminal = self.state.terminals.get(&pane.attached_terminal_id)?;
        terminal.effective_agent_label()?;
        Some(StarterLink {
            started_by: PaneStartedBy::Pane {
                pane_id: self.public_pane_id(ws_idx, pane_id)?,
            },
            label: self.state.starter_label(ws_idx, pane_id),
        })
    }

    fn pane_containing_peer(&self, peer: &RequestPeer) -> Option<(usize, crate::layout::PaneId)> {
        self.state
            .workspaces
            .iter()
            .enumerate()
            .flat_map(|(ws_idx, ws)| {
                ws.tabs
                    .iter()
                    .flat_map(|tab| tab.layout.pane_ids())
                    .map(move |pane_id| (ws_idx, pane_id))
            })
            .find(|&(ws_idx, pane_id)| self.pane_contains_peer(ws_idx, pane_id, peer))
    }

    fn pane_contains_peer(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
        peer: &RequestPeer,
    ) -> bool {
        let Some((runtime, _)) = self.lookup_runtime(ws_idx, pane_id) else {
            return false;
        };
        let Some(shell_pid) = runtime.child_pid() else {
            return false;
        };
        // Agent command runners can allocate a nested PTY. For creation,
        // verified descent still identifies the starter despite a different
        // controlling terminal; agent reports retain their stricter check.
        if peer
            .placement
            .ancestry
            .as_ref()
            .is_some_and(|parents| parents.contains(&shell_pid))
        {
            return true;
        }
        let pane_tty = peer
            .placement
            .tty
            .and_then(|_| crate::platform::process_tty(shell_pid).flatten());
        peer.in_pane(shell_pid, pane_tty) == PeerInPane::Inside
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use crate::api::schema::PaneStartedBy;
    use crate::api::RequestPeer;
    use crate::app::started_by::StarterLink;
    use crate::app::{App, AppPolicy};
    use crate::config::Config;
    use crate::detect::Agent;
    use crate::layout::PaneId;
    use crate::platform::ProcessPlacement;
    use crate::terminal::TerminalRuntime;
    use crate::workspace::Workspace;

    fn attach_runtime(app: &mut App, ws_idx: usize, pane_id: PaneId, shell_pid: u32) {
        let terminal_id = app.state.workspaces[ws_idx]
            .pane_state(pane_id)
            .unwrap()
            .attached_terminal_id
            .clone();
        let runtime = TerminalRuntime::test_with_screen_bytes(80, 24, b"");
        runtime
            .child_pid_handle()
            .store(shell_pid, Ordering::Relaxed);
        app.terminal_runtimes.insert(terminal_id, runtime);
    }

    fn peer_descending_from(shell_pid: u32) -> RequestPeer {
        RequestPeer {
            pid: 9000,
            placement: ProcessPlacement {
                tty: None,
                ancestry: Some(vec![9000, 8000, shell_pid, 1]),
            },
        }
    }

    #[tokio::test]
    async fn verified_pane_starter_derives_agent_pane_from_peer_when_claimed_pane_fails() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        let mut ws = Workspace::test_new("starters");
        ws.test_add_tab(Some("caller"));
        ws.test_add_tab(Some("plain"));
        let pane_b = ws.tabs[0].root_pane;
        let pane_a = ws.tabs[1].root_pane;
        let pane_plain = ws.tabs[2].root_pane;
        app.state.workspaces = vec![ws];
        app.state.ensure_test_terminals();

        attach_runtime(&mut app, 0, pane_b, 100);
        attach_runtime(&mut app, 0, pane_a, 200);
        attach_runtime(&mut app, 0, pane_plain, 300);

        let term_b = app.state.workspaces[0]
            .terminal_id(pane_b)
            .cloned()
            .unwrap();
        let term_a = app.state.workspaces[0]
            .terminal_id(pane_a)
            .cloned()
            .unwrap();
        app.state.terminals.get_mut(&term_b).unwrap().detected_agent = Some(Agent::Pi);
        app.state.terminals.get_mut(&term_a).unwrap().detected_agent = Some(Agent::Codex);

        let public_b = app.public_pane_id(0, pane_b).unwrap();
        let public_a = app.public_pane_id(0, pane_a).unwrap();
        let expected_a = Some(StarterLink {
            started_by: PaneStartedBy::Pane {
                pane_id: public_a.clone(),
            },
            label: Some("caller".into()),
        });

        // Claim matches peer inside agent pane A.
        assert_eq!(
            app.verified_pane_starter(Some(&public_a), Some(&peer_descending_from(200))),
            expected_a
        );
        // Stale claim names another live agent pane B while peer runs in agent pane A.
        assert_eq!(
            app.verified_pane_starter(Some(&public_b), Some(&peer_descending_from(200))),
            expected_a
        );
        // Stale claim names a closed pane while peer runs in agent pane A.
        assert_eq!(
            app.verified_pane_starter(Some("w99:p1"), Some(&peer_descending_from(200))),
            expected_a
        );
        // Stale claim names live agent pane B while peer runs in a plain-shell pane.
        assert_eq!(
            app.verified_pane_starter(Some(&public_b), Some(&peer_descending_from(300))),
            None
        );
        // Forged claim from outside every pane.
        assert_eq!(
            app.verified_pane_starter(Some(&public_b), Some(&peer_descending_from(999))),
            None
        );
        // Unclaimed caller_pane_id (such as --machine routing) never attributes a local pane.
        assert_eq!(
            app.verified_pane_starter(None, Some(&peer_descending_from(200))),
            None
        );
    }
}
