use std::time::Instant;

#[cfg(test)]
use std::time::Duration;

use super::{
    background_update_check_enabled, App, AUTO_UPDATE_CHECK_INTERVAL, MIN_RENDER_INTERVAL,
};
fn retain_detached_process_after_wait(
    pid: u32,
    result: std::io::Result<Option<std::process::ExitStatus>>,
) -> bool {
    match result {
        Ok(None) => true,
        Ok(Some(_)) => false,
        Err(err) if err.kind() == std::io::ErrorKind::Interrupted => true,
        Err(err) => {
            tracing::warn!(pid, err = %err, "failed to reap detached process");
            false
        }
    }
}

impl App {
    pub(crate) fn reap_finished_detached_processes(&mut self) {
        self.detached_process_children
            .retain_mut(|child| retain_detached_process_after_wait(child.id(), child.try_wait()));
    }

    pub(crate) fn shutdown_terminal_runtime(&mut self, terminal_id: crate::terminal::TerminalId) {
        if let Some(runtime) = self.terminal_runtimes.remove(&terminal_id) {
            runtime.shutdown();
        }
    }

    pub(crate) fn shutdown_detached_terminal_runtimes(&mut self) {
        // Internal events and explicit layout mutations converge here after
        // applying state. Persist their queued history before detaching runtimes.
        self.flush_logbook();
        let terminal_ids = std::mem::take(&mut self.state.terminal_runtime_shutdowns);
        let removed_terminals = !terminal_ids.is_empty();
        for terminal_id in terminal_ids {
            self.shutdown_terminal_runtime(terminal_id);
        }
        if removed_terminals {
            self.sync_flag_deadline();
            self.prune_emitted_pane_and_workspace_facts();
        }
    }

    pub(crate) fn seed_emitted_pane_and_workspace_facts(&mut self) {
        self.pending_pane_cwd_updates.clear();
        self.last_emitted_pane_cwds.clear();
        self.last_emitted_workspace_labels.clear();
        for ws in &self.state.workspaces {
            let label = ws.display_name_from(&self.state.terminals, &self.terminal_runtimes);
            self.last_emitted_workspace_labels
                .insert(ws.id.clone(), label);
            for tab in &ws.tabs {
                for &pane_id in tab.panes.keys() {
                    if let Some(cwd) =
                        tab.cwd_for_pane(pane_id, &self.state.terminals, &self.terminal_runtimes)
                    {
                        self.last_emitted_pane_cwds.insert(pane_id, cwd);
                    }
                }
            }
        }
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
        self.last_emitted_workspace_labels.retain(|ws_id, _| {
            self.state.workspaces.iter().any(|ws| &ws.id == ws_id)
        });
    }

    pub(crate) fn next_pane_cwd_update_deadline(&self) -> Option<Instant> {
        self.pending_pane_cwd_updates.values().copied().min()
    }

    pub(crate) fn flush_due_pane_cwd_updates(&mut self, now: Instant) -> bool {
        let mut due_panes: Vec<crate::layout::PaneId> = self
            .pending_pane_cwd_updates
            .iter()
            .filter_map(|(&pane_id, &deadline)| (now >= deadline).then_some(pane_id))
            .collect();
        if due_panes.is_empty() {
            return false;
        }
        due_panes.sort_unstable_by_key(|pane_id| pane_id.raw());
        for pane_id in &due_panes {
            self.pending_pane_cwd_updates.remove(pane_id);
        }

        let mut emitted = false;
        for pane_id in due_panes {
            let Some((ws_idx, _)) = self.find_pane(pane_id) else {
                self.last_emitted_pane_cwds.remove(&pane_id);
                continue;
            };
            let Some(tab_idx) = self.state.workspaces[ws_idx].find_tab_index_for_pane(pane_id)
            else {
                continue;
            };
            let Some(current_cwd) = self.state.workspaces[ws_idx].tabs[tab_idx].cwd_for_pane(
                pane_id,
                &self.state.terminals,
                &self.terminal_runtimes,
            ) else {
                continue;
            };
            if self.last_emitted_pane_cwds.get(&pane_id) != Some(&current_cwd) {
                self.last_emitted_pane_cwds.insert(pane_id, current_cwd);
                self.emit_pane_updated(ws_idx, pane_id);
                emitted = true;
            }
        }
        emitted |= self.emit_due_workspace_label_updates();
        emitted
    }

    pub(crate) fn emit_due_workspace_label_updates(&mut self) -> bool {
        let mut emitted = false;
        for ws_idx in 0..self.state.workspaces.len() {
            if self.state.workspaces[ws_idx]
                .tabs
                .first()
                .is_some_and(|tab| self.pending_pane_cwd_updates.contains_key(&tab.root_pane))
            {
                continue;
            }
            let ws_id = self.state.workspaces[ws_idx].id.clone();
            let current_label = self.state.workspaces[ws_idx]
                .display_name_from(&self.state.terminals, &self.terminal_runtimes);
            match self.last_emitted_workspace_labels.get(&ws_id) {
                Some(prev_label) if prev_label != &current_label => {
                    self.last_emitted_workspace_labels
                        .insert(ws_id, current_label);
                    self.emit_workspace_updated(ws_idx);
                    emitted = true;
                }
                None => {
                    self.last_emitted_workspace_labels
                        .insert(ws_id, current_label);
                }
                Some(_) => {}
            }
        }
        emitted
    }

    /// Classify every pane whose restored status claim has run out, dating
    /// each at its own deadline. Returns whether any pane changed.
    pub(crate) fn resolve_due_agent_status_windows(&mut self, now: Instant) -> bool {
        let pending_changed = self.flush_agent_status_resolutions();
        if self
            .state
            .next_agent_status_resolution_deadline(now)
            .is_none_or(|deadline| now < deadline)
        {
            return pending_changed;
        }
        let resolved = self
            .state
            .resolve_due_agent_status_windows_at(crate::pane::unix_now_secs());
        let changed = pending_changed || !resolved.is_empty();
        self.emit_agent_status_resolutions(resolved);
        changed
    }

    pub(crate) fn sync_agent_metadata_deadline(&mut self) {
        self.agent_metadata_deadline = self.state.next_agent_metadata_expiry();
    }

    pub(crate) fn expire_due_metadata(&mut self, now: Instant) -> bool {
        let Some(deadline) = self
            .agent_metadata_deadline
            .filter(|deadline| now >= *deadline)
        else {
            return false;
        };
        self.expire_metadata_at(deadline, now);
        true
    }

    pub(crate) fn expire_metadata_at(&mut self, deadline: Instant, now: Instant) {
        let previous_toast = self.state.toast.clone();
        for update in self.state.expire_agent_metadata_at(deadline, now) {
            self.refresh_new_herdr_toast_context_for_update(&update, &previous_toast);
            self.emit_pane_state_update(&update);
        }
        let (panes, workspaces) = self.state.expire_metadata_tokens(now);
        for (ws_idx, pane_id) in panes {
            self.emit_pane_updated(ws_idx, pane_id);
        }
        for ws_idx in workspaces {
            self.emit_workspace_token_updated(ws_idx);
        }
        self.sync_agent_metadata_deadline();
    }

    pub(crate) fn can_render_now(&self, now: Instant) -> bool {
        match self.last_render_at {
            Some(last_render_at) => now.duration_since(last_render_at) >= MIN_RENDER_INTERVAL,
            None => true,
        }
    }

    pub(crate) fn can_present_now(&self, now: Instant) -> bool {
        match self.last_presentation_at {
            Some(last_presentation_at) => {
                now.duration_since(last_presentation_at) >= MIN_RENDER_INTERVAL
            }
            None => true,
        }
    }

    pub(crate) fn record_render_attempt(&mut self, now: Instant, presentation: bool) {
        self.last_render_at = Some(now);
        if presentation {
            self.last_presentation_at = Some(now);
        }
    }

    pub(crate) fn run_auto_update_check(&mut self) {
        if !background_update_check_enabled(
            self.policy.background_updates,
            self.update_version_check_enabled,
        ) {
            self.next_auto_update_check = None;
            return;
        }

        self.next_auto_update_check = self
            .state
            .update_available
            .is_none()
            .then_some(Instant::now() + AUTO_UPDATE_CHECK_INTERVAL);

        if self.state.update_available.is_some() {
            return;
        }

        let update_tx = self.event_tx.clone();
        if let Err(err) = crate::thread_spawn::spawn_named("herdr-update-check", move || {
            crate::update::auto_update(update_tx)
        }) {
            tracing::warn!(err = %err, "failed to spawn update check thread");
        }
    }

    pub(crate) fn run_agent_manifest_update_check(&mut self) {
        if !background_update_check_enabled(
            self.policy.background_updates,
            self.update_manifest_check_enabled,
        ) {
            self.next_agent_manifest_update_check = None;
            return;
        }

        self.next_agent_manifest_update_check = Some(Instant::now() + AUTO_UPDATE_CHECK_INTERVAL);

        let manifest_update_tx = self.event_tx.clone();
        if let Err(err) = crate::thread_spawn::spawn_named("herdr-manifest-check", move || {
            crate::detect::manifest_update::auto_update(manifest_update_tx)
        }) {
            tracing::warn!(err = %err, "failed to spawn agent manifest check thread");
        }
    }

    pub(crate) fn next_headless_loop_deadline_with_git_refresh(
        &self,
        now: Instant,
        needs_render: bool,
        include_git_refresh: bool,
    ) -> Option<Instant> {
        let render_deadline = if needs_render {
            self.last_render_at
                .map(|last_render_at| last_render_at + MIN_RENDER_INTERVAL)
                .filter(|deadline| *deadline > now)
        } else {
            None
        };

        [
            self.config_diagnostic_deadline,
            self.toast_deadline,
            self.flag_deadline,
            self.state.next_pending_agent_notification_deadline(),
            self.state.next_managed_agent_deadline(),
            self.state.next_agent_status_resolution_deadline(now),
            include_git_refresh
                .then(|| self.git_refresh_deadline())
                .flatten(),
            self.restored_worktree_validation_retry_at,
            self.next_auto_update_check,
            self.next_agent_manifest_update_check,
            self.agent_metadata_deadline,
            self.pending_agent_resume_deadline,
            self.session_save_deadline,
            self.next_tab_bar_status_deadline(),
            self.next_pane_cwd_update_deadline(),
            render_deadline,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    #[cfg(test)]
    pub(crate) fn drain_internal_events(&mut self) -> bool {
        self.drain_internal_events_up_to(super::APP_EVENT_DRAIN_LIMIT)
            .1
    }

    #[cfg(test)]
    pub(crate) fn drain_all_internal_events(&mut self) -> bool {
        let mut changed = false;
        loop {
            let (had_event, batch_changed) =
                self.drain_internal_events_up_to(super::APP_EVENT_DRAIN_LIMIT);
            changed |= batch_changed;
            if !had_event {
                break;
            }
        }
        changed
    }

    #[cfg(test)]
    fn drain_internal_events_up_to(&mut self, limit: usize) -> (bool, bool) {
        let mut had_event = false;
        let mut changed = false;
        for _ in 0..limit {
            let Ok(ev) = self.event_rx.try_recv() else {
                break;
            };
            had_event = true;
            changed |= self.handle_internal_event_with_render_impact(ev);
        }
        (had_event, changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::Workspace;

    #[test]
    fn hidden_render_attempt_keeps_presentation_cadence_available() {
        let (mut app, _) = test_app_with_pane();
        let initial_presentation = Instant::now();
        app.record_render_attempt(initial_presentation, true);

        let hidden_attempt = initial_presentation + MIN_RENDER_INTERVAL;
        app.record_render_attempt(hidden_attempt, false);
        let foreground_echo = hidden_attempt + Duration::from_millis(1);

        assert!(!app.can_render_now(foreground_echo));
        assert!(app.can_present_now(foreground_echo));
    }

    #[test]
    fn interrupted_detached_process_wait_keeps_child_for_retry() {
        let interrupted = std::io::Error::new(std::io::ErrorKind::Interrupted, "test interrupt");

        assert!(retain_detached_process_after_wait(42, Err(interrupted)));
    }

    fn test_app_with_pane() -> (super::super::App, crate::layout::PaneId) {
        let mut app = super::super::App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        );
        let ws = Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        app.state.workspaces.push(ws);
        app.state.active = Some(0);
        app.state.view.pane_infos.push(crate::layout::PaneInfo {
            id: pane_id,
            rect: ratatui::layout::Rect::new(0, 0, 80, 24),
            inner_rect: ratatui::layout::Rect::new(0, 0, 80, 24),
            scrollbar_rect: None,
            borders: ratatui::widgets::Borders::NONE,
            is_focused: true,
        });
        (app, pane_id)
    }
}
