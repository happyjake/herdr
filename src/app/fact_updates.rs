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
                        if let Some(runtime) = tab
                            .terminal_id(pane_id)
                            .and_then(|id| self.terminal_runtimes.get(id))
                        {
                            runtime.record_published_cwd(Some(&cwd));
                        }
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
            if let Some((_, pane_state)) = self.find_pane(pane_id) {
                if let Some(runtime) = self.terminal_runtimes.get(&pane_state.attached_terminal_id)
                {
                    runtime.record_published_cwd(pane.cwd.as_deref().map(Path::new));
                }
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use bytes::Bytes;
    use tokio::sync::{mpsc, Notify};

    use crate::config::Config;
    use crate::events::AppEvent;
    use crate::pane::{AgentDetection, PaneLaunchEnv};
    use crate::render_signal::RenderSignal;
    use crate::terminal::{TerminalId, TerminalRuntime};
    use crate::workspace::Workspace;

    struct NestedShell {
        app: App,
        hub: crate::api::EventHub,
        events: mpsc::Sender<AppEvent>,
        reports: mpsc::Receiver<AppEvent>,
        terminal_id: TerminalId,
        pane_id: PaneId,
        base: PathBuf,
    }

    impl NestedShell {
        async fn new() -> Self {
            let base = std::env::temp_dir().join(format!(
                "herdr-cwd-publication-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(base.join("alpha")).unwrap();
            std::fs::create_dir(base.join("beta")).unwrap();
            let base = base.canonicalize().unwrap();
            let hub = crate::api::EventHub::default();
            let (_, api_rx) = mpsc::unbounded_channel();
            let mut app = App::new(
                &Config::default(),
                super::super::AppPolicy::TEST,
                None,
                api_rx,
                hub.clone(),
            );
            let mut workspace = Workspace::test_new("alpha");
            workspace.custom_name = None;
            workspace.identity_cwd = base.join("alpha");
            workspace.cached_identity_cwd = workspace.identity_cwd.clone();
            workspace.cached_auto_label = "alpha".into();
            let pane_id = workspace.tabs[0].root_pane;
            let terminal_id = workspace.tabs[0].terminal_id(pane_id).unwrap().clone();
            app.state.workspaces = vec![workspace];
            app.state.active = Some(0);
            app.state.ensure_test_terminals();
            let (events, reports) = mpsc::channel(64);
            // Drive sampling explicitly so B can be published between two samples.
            // The shell and all cwd resolution remain real; no OSC 7 is emitted.
            let runtime = TerminalRuntime::spawn_argv_command(
                pane_id,
                24,
                80,
                base.clone(),
                &["/bin/sh".into(), "-i".into()],
                &PaneLaunchEnv::default(),
                AgentDetection::Disabled,
                0,
                crate::terminal_theme::TerminalTheme::default(),
                None,
                events.clone(),
                Arc::new(Notify::new()),
                Arc::new(RenderSignal::new()),
            )
            .unwrap();
            runtime
                .try_send_bytes(Bytes::from_static(b"/bin/sh -i\ncd alpha\n"))
                .unwrap();
            app.terminal_runtimes.insert(terminal_id.clone(), runtime);
            let mut shell = Self {
                app,
                hub,
                events,
                reports,
                terminal_id,
                pane_id,
                base,
            };
            shell.wait_for_cwd("alpha").await;
            shell.app.seed_emitted_pane_and_workspace_facts();
            shell
        }

        fn runtime(&self) -> &TerminalRuntime {
            self.app.terminal_runtimes.get(&self.terminal_id).unwrap()
        }

        async fn wait_for_cwd(&self, name: &str) {
            let expected = self.base.join(name);
            tokio::time::timeout(Duration::from_secs(5), async {
                while self.runtime().cwd().as_ref() != Some(&expected) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("nested shell reaches requested cwd");
        }

        async fn change_cwd(&self, name: &str) {
            self.runtime()
                .try_send_bytes(Bytes::from(format!("cd ../{name}\n")))
                .unwrap();
            self.wait_for_cwd(name).await;
        }

        fn drain_reports(&mut self, expected: &Path) {
            while let Ok(event) = self.reports.try_recv() {
                if let AppEvent::TerminalCwdReported { cwd, .. } = &event {
                    assert_eq!(
                        cwd, expected,
                        "no intermediate cwd report may enter the App"
                    );
                }
                self.app.handle_internal_event(event);
            }
        }
    }

    impl Drop for NestedShell {
        fn drop(&mut self) {
            for (_, runtime) in self.app.terminal_runtimes.drain() {
                runtime.shutdown();
            }
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    #[tokio::test]
    async fn pane_cwd_sample_corrects_published_unsampled_cwd_and_workspace_label() {
        let mut shell = NestedShell::new().await;
        let alpha = shell.base.join("alpha");
        let beta = shell.base.join("beta");
        let sampled_at = Instant::now();
        shell.runtime().test_sample_cwd(sampled_at, &shell.events);
        shell.drain_reports(&alpha);
        assert!(shell.hub.events_after(0).is_empty());

        shell.change_cwd("beta").await;
        // An unrelated presentation emitter samples B live, without a B report.
        shell.app.emit_pane_updated(0, shell.pane_id);
        shell.app.emit_due_workspace_label_updates(Instant::now());
        assert_eq!(shell.app.last_emitted_pane_cwds[&shell.pane_id], beta);
        assert!(shell.app.pending_pane_cwd_updates.is_empty());
        shell.drain_reports(&alpha);

        shell.change_cwd("alpha").await;
        shell
            .runtime()
            .test_sample_cwd(sampled_at + Duration::from_secs(1), &shell.events);
        shell.drain_reports(&alpha);
        shell
            .app
            .flush_due_pane_cwd_updates(Instant::now() + Duration::from_secs(2));
        let events = shell.hub.events_after(0);
        let cwds: Vec<_> = events
            .iter()
            .filter_map(|(_, event)| match &event.data {
                EventData::PaneUpdated { pane } => pane.cwd.clone(),
                _ => None,
            })
            .collect();
        let labels: Vec<_> = events
            .iter()
            .filter_map(|(_, event)| match &event.data {
                EventData::WorkspaceUpdated { workspace } => Some(workspace.label.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            cwds,
            [beta.display().to_string(), alpha.display().to_string()]
        );
        assert_eq!(labels, ["beta", "alpha"]);
        // Synchronization must retain suppression of genuinely unchanged samples.
        shell
            .runtime()
            .test_sample_cwd(sampled_at + Duration::from_secs(2), &shell.events);
        while let Ok(event) = shell.reports.try_recv() {
            assert!(!matches!(event, AppEvent::TerminalCwdReported { .. }));
        }
    }

    #[tokio::test]
    async fn nested_shell_cwd_walk_cost() {
        let shell = NestedShell::new().await;
        let pid = shell.runtime().child_pid().unwrap();
        let foreground = crate::platform::foreground_process_group_id(pid);
        assert_ne!(foreground, Some(pid), "fixture must drive a nested shell");
        let expected = shell.base.join("alpha");
        assert_ne!(crate::platform::process_cwd(pid), Some(expected.clone()));
        let mut durations = Vec::with_capacity(100);
        for _ in 0..100 {
            let start = Instant::now();
            let observed = crate::platform::pane_shell_cwd(pid, foreground);
            durations.push(start.elapsed());
            assert_eq!(observed.as_ref(), Some(&expected));
        }
        durations.sort_unstable();
        let median_us = (durations[49].as_secs_f64() + durations[50].as_secs_f64()) * 500_000.0;
        eprintln!("nested-shell cwd walk: 100 samples, median {median_us:.3} us; at 1 Hz: 1 pane {median_us:.3} us/s, 15 panes {:.3} us/s", median_us * 15.0);
    }
}
