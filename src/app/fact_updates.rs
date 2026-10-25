use std::path::Path;
use std::time::Instant;

use crate::api::schema::{EventData, EventEnvelope, EventKind};
use crate::layout::PaneId;

use super::{App, PendingCwdUpdate, PANE_CWD_EVENT_DEBOUNCE, PANE_CWD_EVENT_MAX_DELAY};

impl App {
    pub(crate) fn seed_emitted_pane_and_workspace_facts(&mut self) {
        self.pending_pane_cwd_updates.clear();
        self.dirty_workspace_labels.clear();
        self.last_emitted_pane_cwds.clear();
        self.last_emitted_workspace_labels.clear();
        // Initialization samples the same live shell fact used by snapshots. Seed
        // terminal state too: restored cwd can predate the replacement runtime.
        for ws in &self.state.workspaces {
            for tab in &ws.tabs {
                for &pane_id in tab.panes.keys() {
                    if let Some(cwd) =
                        tab.cwd_for_pane(pane_id, &self.state.terminals, &self.terminal_runtimes)
                    {
                        if let Some(terminal) = tab
                            .terminal_id(pane_id)
                            .and_then(|id| self.state.terminals.get_mut(id))
                        {
                            terminal.cwd = cwd.clone();
                        }
                        self.last_emitted_pane_cwds.insert(pane_id, cwd);
                    }
                }
            }
            self.last_emitted_workspace_labels.insert(
                ws.id.clone(),
                ws.display_name_from_terminals(&self.state.terminals),
            );
        }
    }

    /// Every event publisher passes here, including presentation-only updates.
    pub(super) fn record_published_facts(&mut self, data: &EventData) {
        match data {
            EventData::PaneCreated { pane } | EventData::PaneUpdated { pane } => {
                self.record_published_pane(pane);
            }
            EventData::PaneMoved {
                pane,
                created_workspace,
                ..
            } => {
                self.record_published_pane(pane);
                if let Some(workspace) = created_workspace {
                    self.last_emitted_workspace_labels
                        .insert(workspace.workspace_id.clone(), workspace.label.clone());
                }
            }
            EventData::WorkspaceCreated { workspace }
            | EventData::WorkspaceUpdated { workspace }
            | EventData::WorkspaceMetadataUpdated { workspace }
            | EventData::WorktreeCreated { workspace, .. }
            | EventData::WorktreeOpened { workspace, .. } => {
                self.last_emitted_workspace_labels
                    .insert(workspace.workspace_id.clone(), workspace.label.clone());
            }
            EventData::WorkspaceMoved { workspaces, .. }
            | EventData::WorkspaceReordered { workspaces, .. } => {
                for workspace in workspaces {
                    self.last_emitted_workspace_labels
                        .insert(workspace.workspace_id.clone(), workspace.label.clone());
                }
            }
            EventData::WorktreeRemoved {
                workspace: Some(workspace),
                ..
            } => {
                self.last_emitted_workspace_labels
                    .insert(workspace.workspace_id.clone(), workspace.label.clone());
            }
            EventData::WorkspaceRenamed {
                workspace_id,
                label,
            } => {
                self.last_emitted_workspace_labels
                    .insert(workspace_id.clone(), label.clone());
            }
            _ => {}
        }
    }

    fn record_published_pane(&mut self, pane: &crate::api::schema::PaneInfo) {
        if let Some((ws_idx, pane_id)) = self.parse_pane_id(&pane.pane_id) {
            if let Some(cwd) = pane.cwd.as_deref() {
                self.last_emitted_pane_cwds.insert(pane_id, cwd.into());
                if let Some((_, pane)) = self.find_pane(pane_id) {
                    let terminal_id = pane.attached_terminal_id.clone();
                    if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
                        terminal.cwd = cwd.into();
                    }
                }
            } else {
                self.last_emitted_pane_cwds.remove(&pane_id);
            }
            self.mark_pane_label_source_dirty(ws_idx, pane_id, Instant::now());
        }
    }

    pub(super) fn mark_label_sources_changed(&mut self, data: &EventData) {
        let now = Instant::now();
        match data {
            EventData::LayoutUpdated { layout } => {
                self.mark_workspace_label_dirty(&layout.workspace_id, now);
            }
            EventData::TabCreated { tab } => {
                self.mark_workspace_label_dirty(&tab.workspace_id, now);
            }
            EventData::TabClosed { workspace_id, .. }
            | EventData::TabMoved { workspace_id, .. }
            | EventData::PaneExited { workspace_id, .. }
            | EventData::PaneClosed { workspace_id, .. } => {
                self.mark_workspace_label_dirty(workspace_id, now);
            }
            EventData::PaneMoved {
                previous_workspace_id,
                pane,
                ..
            } => {
                self.mark_workspace_label_dirty(previous_workspace_id, now);
                self.mark_workspace_label_dirty(&pane.workspace_id, now);
            }
            EventData::WorkspaceClosed { workspace_id, .. } => {
                self.dirty_workspace_labels.remove(workspace_id);
                self.last_emitted_workspace_labels.remove(workspace_id);
            }
            _ => {}
        }
    }

    pub(super) fn mark_workspace_label_dirty(&mut self, workspace_id: &str, now: Instant) {
        if self
            .state
            .workspaces
            .iter()
            .any(|ws| ws.id == workspace_id && ws.custom_name.is_none())
        {
            self.dirty_workspace_labels
                .entry(workspace_id.into())
                .or_insert(now);
        }
    }

    fn mark_pane_label_source_dirty(&mut self, ws_idx: usize, pane_id: PaneId, now: Instant) {
        let ws = &self.state.workspaces[ws_idx];
        if ws.tabs.first().is_some_and(|tab| tab.root_pane == pane_id) {
            let workspace_id = ws.id.clone();
            self.mark_workspace_label_dirty(&workspace_id, now);
        }
    }

    pub(super) fn reconcile_reported_pane_cwd(
        &mut self,
        pane_id: PaneId,
        cwd: &Path,
        now: Instant,
    ) {
        let Some((ws_idx, _)) = self.find_pane(pane_id) else {
            return;
        };
        if self
            .last_emitted_pane_cwds
            .get(&pane_id)
            .map(|cwd| cwd.as_path())
            != Some(cwd)
        {
            let pending =
                self.pending_pane_cwd_updates
                    .entry(pane_id)
                    .or_insert(PendingCwdUpdate {
                        first_seen: now,
                        deadline: now + PANE_CWD_EVENT_DEBOUNCE,
                    });
            pending.deadline =
                (now + PANE_CWD_EVENT_DEBOUNCE).min(pending.first_seen + PANE_CWD_EVENT_MAX_DELAY);
        }
        self.mark_pane_label_source_dirty(ws_idx, pane_id, now);
    }

    pub(crate) fn prune_emitted_pane_and_workspace_facts(&mut self) {
        self.pending_pane_cwd_updates.retain(|pane_id, _| {
            self.state
                .workspaces
                .iter()
                .any(|ws| ws.pane_state(*pane_id).is_some())
        });
        self.last_emitted_pane_cwds.retain(|pane_id, _| {
            self.state
                .workspaces
                .iter()
                .any(|ws| ws.pane_state(*pane_id).is_some())
        });
        self.last_emitted_workspace_labels
            .retain(|id, _| self.state.workspaces.iter().any(|ws| &ws.id == id));
        self.dirty_workspace_labels
            .retain(|id, _| self.state.workspaces.iter().any(|ws| &ws.id == id));
    }

    fn workspace_label_deadline(&self, id: &str, dirty_at: Instant) -> Instant {
        self.state
            .workspaces
            .iter()
            .find(|ws| ws.id == id)
            .and_then(|ws| ws.tabs.first())
            .and_then(|tab| self.pending_pane_cwd_updates.get(&tab.root_pane))
            .map_or(dirty_at, |pending| dirty_at.max(pending.deadline))
    }

    pub(crate) fn next_pane_cwd_update_deadline(&self) -> Option<Instant> {
        self.pending_pane_cwd_updates
            .values()
            .map(|pending| pending.deadline)
            .chain(
                self.dirty_workspace_labels
                    .iter()
                    .map(|(id, &dirty_at)| self.workspace_label_deadline(id, dirty_at)),
            )
            .min()
    }

    pub(crate) fn flush_due_pane_cwd_updates(&mut self, now: Instant) -> bool {
        let mut due: Vec<_> = self
            .pending_pane_cwd_updates
            .iter()
            .filter_map(|(&id, pending)| (now >= pending.deadline).then_some(id))
            .collect();
        due.sort_unstable_by_key(|id| id.raw());
        let mut emitted = false;
        for pane_id in due {
            self.pending_pane_cwd_updates.remove(&pane_id);
            let Some((ws_idx, pane)) = self.find_pane(pane_id) else {
                continue;
            };
            let cwd = self
                .state
                .terminals
                .get(&pane.attached_terminal_id)
                .map(|t| &t.cwd);
            if cwd != self.last_emitted_pane_cwds.get(&pane_id) {
                self.emit_pane_updated(ws_idx, pane_id);
                emitted = true;
            }
        }
        emitted | self.emit_due_workspace_label_updates(now)
    }

    pub(crate) fn emit_due_workspace_label_updates(&mut self, now: Instant) -> bool {
        let mut due: Vec<_> = self
            .dirty_workspace_labels
            .iter()
            .filter_map(|(id, &dirty_at)| {
                (self.workspace_label_deadline(id, dirty_at) <= now).then_some(id.clone())
            })
            .collect();
        due.sort();
        let mut emitted = false;
        for id in due {
            self.dirty_workspace_labels.remove(&id);
            let Some(index) = self.state.workspaces.iter().position(|ws| ws.id == id) else {
                continue;
            };
            let label =
                self.state.workspaces[index].display_name_from_terminals(&self.state.terminals);
            if self.last_emitted_workspace_labels.get(&id) != Some(&label) {
                let workspace = self.workspace_info_with_label(
                    index,
                    crate::pane::AgentStatusProjection::Reported,
                    label,
                );
                self.emit_event(EventEnvelope {
                    event: EventKind::WorkspaceUpdated,
                    data: EventData::WorkspaceUpdated { workspace },
                });
                emitted = true;
            }
        }
        emitted
    }
}
