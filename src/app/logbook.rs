use super::{App, AppState};
use crate::layout::PaneId;
use crate::persist::logbook::{self, Event, LabelSource, SessionKey};

impl AppState {
    pub(crate) fn logbook_key(&self, pane_id: PaneId) -> Option<SessionKey> {
        self.logbook_events.as_ref()?;
        let terminal_id = self
            .workspaces
            .iter()
            .find_map(|ws| ws.terminal_id(pane_id))?;
        self.terminals
            .get(terminal_id)?
            .persisted_agent_session
            .as_ref()
            .map(SessionKey::from)
    }

    pub(crate) fn record_logbook_parent(&mut self, pane_id: PaneId, parent: SessionKey) {
        if let Some(child) = self.logbook_key(pane_id) {
            if child != parent && child.agent == parent.agent {
                self.push_logbook_event(Event::Parent { child, parent });
            }
        }
    }

    fn logbook_pane(
        &self,
        ws_idx: usize,
        pane_id: PaneId,
    ) -> Option<(String, &crate::terminal::TerminalState)> {
        let ws = self.workspaces.get(ws_idx)?;
        let pane =
            crate::workspace::public_pane_id_for_number(&ws.id, ws.public_pane_number(pane_id)?);
        let terminal = self.terminals.get(ws.terminal_id(pane_id)?)?;
        Some((pane, terminal))
    }

    pub(crate) fn record_logbook_session(&mut self, ws_idx: usize, pane_id: PaneId) {
        if self.logbook_events.is_none() {
            return;
        }
        let events = self.logbook_session_events(ws_idx, pane_id);
        if let Some(pending) = &mut self.logbook_events {
            pending.extend(events);
        }
    }

    fn logbook_session_events(&self, ws_idx: usize, pane_id: PaneId) -> Vec<Event> {
        let Some((pane, terminal)) = self.logbook_pane(ws_idx, pane_id) else {
            return Vec::new();
        };
        let Some(session) = &terminal.persisted_agent_session else {
            return Vec::new();
        };
        let event = Event::SessionReported {
            pane,
            terminal: terminal.id.clone(),
            key: SessionKey::from(session),
            cwd: terminal.cwd.clone(),
            at: logbook::now_ms(),
        };
        let mut events = vec![event];
        if terminal.manual_label.is_some() {
            events.extend(self.logbook_label_event(ws_idx, pane_id));
        }
        events
    }

    pub(crate) fn record_logbook_label(&mut self, ws_idx: usize, pane_id: PaneId) {
        if self.logbook_events.is_none() {
            return;
        }
        if let Some(event) = self.logbook_label_event(ws_idx, pane_id) {
            self.push_logbook_event(event);
        }
    }

    fn logbook_label_event(&self, ws_idx: usize, pane_id: PaneId) -> Option<Event> {
        let (pane, terminal) = self.logbook_pane(ws_idx, pane_id)?;
        let session = terminal.persisted_agent_session.as_ref()?;
        Some(Event::LabelWorn {
            key: SessionKey::from(session),
            pane,
            label: terminal.manual_label.clone(),
            source: match terminal.label_source {
                Some(crate::api::schema::LabelSource::Editor) => LabelSource::Editor,
                None => LabelSource::Person,
            },
            at: logbook::now_ms(),
        })
    }

    pub(crate) fn record_logbook_close(&mut self, ws_idx: usize, pane_id: PaneId) {
        if let Some(event) = self.logbook_close_event(ws_idx, pane_id) {
            self.push_logbook_event(event);
        }
    }

    fn logbook_close_event(&self, ws_idx: usize, pane_id: PaneId) -> Option<Event> {
        self.logbook_events.as_ref()?;
        let ws = self.workspaces.get(ws_idx)?;
        let number = ws.public_pane_number(pane_id)?;
        Some(Event::PaneClosed {
            pane: crate::workspace::public_pane_id_for_number(&ws.id, number),
            at: logbook::now_ms(),
        })
    }

    pub(crate) fn logbook_tab_close_events(&self, ws_idx: usize, tab_idx: usize) -> Vec<Event> {
        if self.logbook_events.is_none() {
            return Vec::new();
        }
        self.pane_ids_for_tab(ws_idx, tab_idx)
            .into_iter()
            .filter_map(|pane_id| self.logbook_close_event(ws_idx, pane_id))
            .collect()
    }

    pub(crate) fn enqueue_logbook_events(&mut self, events: impl IntoIterator<Item = Event>) {
        if let Some(pending) = &mut self.logbook_events {
            pending.extend(events);
        }
    }

    fn push_logbook_event(&mut self, event: Event) {
        if let Some(events) = &mut self.logbook_events {
            events.push(event);
        }
    }
}

impl App {
    pub(super) fn initialize_logbook(&mut self) {
        if !self.policy.persist_session {
            return;
        }
        self.state.logbook_events.get_or_insert_with(Vec::new);
        let mut seed = Vec::new();
        for ws_idx in 0..self.state.workspaces.len() {
            for pane_id in self.state.pane_ids_for_workspace(ws_idx) {
                seed.extend(self.state.logbook_session_events(ws_idx, pane_id));
            }
        }
        match logbook::Writer::open(&logbook::path(), &seed) {
            Ok(writer) => self.logbook_writer = Some(writer),
            Err(err) => tracing::warn!(%err, "failed to open session logbook"),
        }
    }

    pub(crate) fn flush_logbook(&mut self) {
        let Some(events) = &mut self.state.logbook_events else {
            return;
        };
        if events.is_empty() {
            return;
        }
        let events = std::mem::take(events);
        if self.logbook_writer.is_none() {
            self.initialize_logbook();
        }
        if let Some(writer) = &mut self.logbook_writer {
            for event in events {
                if let Err(err) = writer.append(&event) {
                    tracing::warn!(%err, "failed to append session logbook event");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::api::schema::{LabelSource, Method, PaneRenameParams, PaneTarget, Request};
    use crate::app::{App, AppPolicy};
    use crate::events::AppEvent;
    use crate::workspace::Workspace;
    use serde_json::{json, Value};

    struct ConfigRoot {
        root: std::path::PathBuf,
        previous_config: Option<std::ffi::OsString>,
        previous_session: Option<std::ffi::OsString>,
    }

    impl ConfigRoot {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "herdr-logbook-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let previous_config = std::env::var_os("XDG_CONFIG_HOME");
            let previous_session = std::env::var_os(crate::session::SESSION_ENV_VAR);
            std::env::set_var("XDG_CONFIG_HOME", &root);
            std::env::set_var(crate::session::SESSION_ENV_VAR, "logbook-test");
            Self {
                root,
                previous_config,
                previous_session,
            }
        }

        fn path(&self) -> std::path::PathBuf {
            self.root
                .join(crate::config::app_dir_name())
                .join("sessions/logbook-test/logbook.jsonl")
        }
    }

    impl Drop for ConfigRoot {
        fn drop(&mut self) {
            for (key, value) in [
                ("XDG_CONFIG_HOME", &self.previous_config),
                (crate::session::SESSION_ENV_VAR, &self.previous_session),
            ] {
                if let Some(value) = value {
                    std::env::set_var(key, value);
                } else {
                    std::env::remove_var(key);
                }
            }
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn app_with_policy(policy: AppPolicy) -> App {
        let (_, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            policy,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("workspace")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app
    }

    fn app() -> App {
        app_with_policy(AppPolicy {
            persist_session: true,
            ..AppPolicy::TEST
        })
    }

    #[test]
    fn logbook_seeds_live_layout_once_even_after_restart_and_resume() {
        let _guard = crate::config::test_config_env_lock().lock().unwrap();
        let root = ConfigRoot::new();
        let before = crate::pane::unix_now_secs() * 1000;
        let mut app = app_with_policy(AppPolicy::TEST);
        app.state.workspaces[0].id = "wseed".into();
        app.state.workspaces[0].test_add_tab(None);
        app.state.workspaces[0].test_add_tab(None);
        app.state.workspaces[0].test_add_tab(None);
        app.state.ensure_test_terminals();
        let pane_ids = app.state.pane_ids_for_workspace(0);
        let mut expected = Vec::new();
        for (index, source) in [Some(LabelSource::Editor), None].into_iter().enumerate() {
            let pane_id = pane_ids[index];
            let pane = app.public_pane_id(0, pane_id).unwrap();
            let terminal_id = app.state.terminal_id_for_pane(0, pane_id).unwrap();
            let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
            terminal.cwd = "project".into();
            terminal.set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
                source: "herdr:claude".into(),
                agent: "claude".into(),
                session_ref: crate::agent_resume::AgentSessionRef::id(format!("session-{index}"))
                    .unwrap(),
            });
            terminal.restore_manual_label(Some(format!("label-{index}")), source, Some(1700000000));
            let key = json!({"agent":"claude","kind":"id","value":format!("session-{index}")});
            expected.push(json!({"type":"session_reported","pane":pane,"terminal":terminal_id,"key":key,"cwd":"project"}));
            expected.push(json!({"type":"label_worn","key":key,"pane":pane,"label":format!("label-{index}"),"source":if index == 0 {"editor"} else {"person"}}));
        }
        let unlabelled = pane_ids[2];
        let pane = app.public_pane_id(0, unlabelled).unwrap();
        let terminal_id = app.state.terminal_id_for_pane(0, unlabelled).unwrap();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.cwd = "project".into();
        terminal.set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
            source: "herdr:claude".into(),
            agent: "claude".into(),
            session_ref: crate::agent_resume::AgentSessionRef::id("unlabelled").unwrap(),
        });
        expected.push(json!({"type":"session_reported","pane":pane,"terminal":terminal_id,"key":{"agent":"claude","kind":"id","value":"unlabelled"},"cwd":"project"}));
        app.policy.persist_session = true;
        app.initialize_logbook();
        assert_eq!(rows(&root.path(), before), expected);
        let seeded = std::fs::read(root.path()).unwrap();
        drop(app);

        let mut restarted = app_with_policy(AppPolicy::TEST);
        restarted.state.workspaces[0].id = "wseed".into();
        restarted.policy.persist_session = true;
        restarted.initialize_logbook();
        assert_eq!(std::fs::read(root.path()).unwrap(), seeded);
        let pane_id = restarted.state.workspaces[0].tabs[0].root_pane;
        let pane = restarted.public_pane_id(0, pane_id).unwrap();
        rename(
            &mut restarted,
            &pane,
            Some("label-0"),
            Some(LabelSource::Editor),
        );
        report(&mut restarted, pane_id, "session-0", 1, "resume");
        assert_eq!(std::fs::read(root.path()).unwrap(), seeded);
        report(&mut restarted, pane_id, "new-session", 2, "clear");
        let appended = std::fs::read(root.path()).unwrap();
        assert!(appended.starts_with(&seeded));
        assert_eq!(rows(&root.path(), before).len(), expected.len() + 2);
    }

    fn rename(app: &mut App, pane: &str, label: Option<&str>, source: Option<LabelSource>) {
        let response = app.handle_api_request(Request {
            id: "rename".into(),
            method: Method::PaneRename(PaneRenameParams {
                pane_id: pane.into(),
                label: label.map(str::to_owned),
                source,
            }),
        });
        assert!(
            serde_json::from_str::<Value>(&response)
                .unwrap()
                .get("error")
                .is_none(),
            "{response}"
        );
    }

    fn report(
        app: &mut App,
        pane_id: crate::layout::PaneId,
        session: &str,
        seq: u64,
        reason: &str,
    ) {
        app.handle_internal_event(AppEvent::AgentSessionReported {
            pane_id,
            source: "herdr:claude".into(),
            agent_label: "claude".into(),
            seq: Some(seq),
            session_ref: crate::agent_resume::AgentSessionRef::id(session),
            session_start_source: Some(reason.into()),
        });
    }

    fn rows(path: &std::path::Path, before: u64) -> Vec<Value> {
        let text = std::fs::read_to_string(path).expect("logbook must be written");
        assert!(text.ends_with('\n'));
        text.lines()
            .map(|line| {
                let mut row: Value = serde_json::from_str(line).unwrap();
                if row["type"] == "parent" {
                    return row;
                }
                let at = row
                    .as_object_mut()
                    .unwrap()
                    .remove("at")
                    .unwrap()
                    .as_u64()
                    .unwrap();
                assert!((before..=crate::pane::unix_now_secs() * 1000 + 999).contains(&at));
                row
            })
            .collect()
    }

    #[test]
    fn logbook_records_known_parent_and_round_trips_after_a_torn_tail() {
        use std::io::Write;
        let _guard = crate::config::test_config_env_lock().lock().unwrap();
        let root = ConfigRoot::new();
        let before = crate::pane::unix_now_secs() * 1000;
        let mut app = app();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let pane = app.public_pane_id(0, pane_id).unwrap();
        let terminal = app.state.terminal_id_for_pane(0, pane_id).unwrap();
        app.state.terminals.get_mut(&terminal).unwrap().cwd = "project".into();
        rename(&mut app, &pane, Some("original"), None);
        app.handle_internal_event(AppEvent::AgentProcessDetected {
            pane_id,
            agent: crate::detect::Agent::Pi,
            observed_at: std::time::Instant::now(),
            reading: crate::events::StatusReading::Provisional,
        });
        let parent_path = root.root.join("parent.jsonl").display().to_string();
        let child_path = root.root.join("child.jsonl").display().to_string();
        for (session, seq, reason) in [
            (&parent_path, 1, "startup"),
            (&child_path, 2, "fork"),
            (&parent_path, 1, "fork"),
            (&child_path, 3, "resume"),
        ] {
            app.handle_internal_event(AppEvent::AgentSessionReported {
                pane_id,
                source: "herdr:pi".into(),
                agent_label: "pi".into(),
                seq: Some(seq),
                session_ref: crate::agent_resume::AgentSessionRef::path(session),
                session_start_source: Some(reason.into()),
            });
        }
        rename(&mut app, &pane, None, None);
        let parent = json!({"agent":"pi","kind":"path","value":parent_path});
        let child = json!({"agent":"pi","kind":"path","value":child_path});
        assert_eq!(
            rows(&root.path(), before),
            vec![
                json!({"type":"session_reported","pane":pane,"terminal":terminal,"key":parent,"cwd":"project"}),
                json!({"type":"label_worn","key":parent,"pane":pane,"label":"original","source":"person"}),
                json!({"type":"session_reported","pane":pane,"terminal":terminal,"key":child,"cwd":"project"}),
                json!({"type":"label_worn","key":child,"pane":pane,"label":"original","source":"person"}),
                json!({"type":"parent","child":child,"parent":parent}),
                json!({"type":"label_worn","key":child,"pane":pane,"label":null,"source":"person"}),
            ]
        );
        let typed = crate::persist::logbook::read(&root.path()).unwrap();
        let complete = std::fs::read_to_string(root.path()).unwrap();
        let raw: Vec<Value> = complete
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(serde_json::to_value(&typed).unwrap(), json!(raw));
        std::fs::File::options()
            .append(true)
            .open(root.path())
            .unwrap()
            .write_all(b"{\"type\":\"label_worn\",\"label\":\"\xf0\x9f")
            .unwrap();
        let torn = std::fs::read(root.path()).unwrap();
        assert_eq!(crate::persist::logbook::read(&root.path()).unwrap(), typed);
        // Reopening also rebuilds deduplication from the complete prefix.
        app.initialize_logbook();
        rename(
            &mut app,
            &pane,
            Some("recovered"),
            Some(LabelSource::Editor),
        );
        app.handle_internal_event(AppEvent::PaneDied {
            pane_id,
            exit_reason: crate::platform::ChildExitReason::Exited,
        });
        let recovered = crate::persist::logbook::read(&root.path()).unwrap();
        assert_eq!(&recovered[..typed.len()], typed);
        assert_eq!(recovered.len(), typed.len() + 2);
        assert_eq!(
            serde_json::to_value(&recovered[typed.len()]).unwrap()["label"],
            "recovered"
        );
        assert_eq!(
            serde_json::to_value(recovered.last().unwrap()).unwrap()["type"],
            "pane_closed"
        );
        assert!(std::fs::read(root.path()).unwrap().starts_with(&torn));
        assert!(!std::path::Path::new(&parent_path).exists());
        assert!(!std::path::Path::new(&child_path).exists());
    }

    #[test]
    fn logbook_records_session_labels_resume_and_close_in_order() {
        let _guard = crate::config::test_config_env_lock().lock().unwrap();
        let root = ConfigRoot::new();
        let before = crate::pane::unix_now_secs() * 1000;
        let mut app = app();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let pane = app.public_pane_id(0, pane_id).unwrap();
        let terminal = app.state.workspaces[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        app.state.terminals.get_mut(&terminal).unwrap().cwd = "project".into();
        rename(
            &mut app,
            &pane,
            Some("first label"),
            Some(LabelSource::Editor),
        );
        report(&mut app, pane_id, "first-session", 1, "startup");
        report(&mut app, pane_id, "first-session", 2, "resume");
        rename(&mut app, &pane, Some("first label"), None);
        rename(&mut app, &pane, Some("personal label"), None);
        rename(
            &mut app,
            &pane,
            Some("automatic label"),
            Some(LabelSource::Editor),
        );
        report(&mut app, pane_id, "next-session", 3, "clear");
        let response = app.handle_api_request(Request {
            id: "close".into(),
            method: Method::PaneClose(PaneTarget {
                pane_id: pane.clone(),
            }),
        });
        assert!(serde_json::from_str::<Value>(&response)
            .unwrap()
            .get("error")
            .is_none());
        assert!(app.state.workspaces.is_empty());
        let first = json!({"agent":"claude","kind":"id","value":"first-session"});
        let next = json!({"agent":"claude","kind":"id","value":"next-session"});
        assert_eq!(
            rows(&root.path(), before),
            vec![
                json!({"type":"session_reported","pane":pane,"terminal":terminal,"key":first,"cwd":"project"}),
                json!({"type":"label_worn","key":first,"pane":pane,"label":"first label","source":"editor"}),
                json!({"type":"label_worn","key":first,"pane":pane,"label":"first label","source":"person"}),
                json!({"type":"label_worn","key":first,"pane":pane,"label":"personal label","source":"person"}),
                json!({"type":"label_worn","key":first,"pane":pane,"label":"automatic label","source":"editor"}),
                json!({"type":"session_reported","pane":pane,"terminal":terminal,"key":next,"cwd":"project"}),
                json!({"type":"label_worn","key":next,"pane":pane,"label":"automatic label","source":"editor"}),
                json!({"type":"pane_closed","pane":pane}),
            ]
        );
        let history = std::fs::read(root.path()).unwrap();
        app.save_session_now();
        assert_eq!(std::fs::read(root.path()).unwrap(), history);
    }

    #[test]
    fn logbook_records_all_panes_closed_by_tab_and_workspace_actions() {
        let _guard = crate::config::test_config_env_lock().lock().unwrap();
        let root = ConfigRoot::new();
        let before = crate::pane::unix_now_secs() * 1000;
        let mut app = app();
        let first = app.state.workspaces[0].tabs[0].root_pane;
        let first_public = app.public_pane_id(0, first).unwrap();
        let tab_idx = app.state.workspaces[0].test_add_tab(None);
        app.state.workspaces[0].active_tab = tab_idx;
        app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
        app.state.ensure_test_terminals();
        let closing = app
            .state
            .pane_ids_for_tab(0, tab_idx)
            .into_iter()
            .map(|pane_id| app.public_pane_id(0, pane_id).unwrap())
            .collect::<Vec<_>>();
        let response = app.handle_api_request(Request {
            id: "close-tab".into(),
            method: Method::TabClose(crate::api::schema::TabTarget {
                tab_id: app.public_tab_id(0, tab_idx).unwrap(),
            }),
        });
        assert!(
            serde_json::from_str::<Value>(&response)
                .unwrap()
                .get("error")
                .is_none(),
            "{response}"
        );
        let mut expected: Vec<_> = closing
            .into_iter()
            .map(|pane| json!({"type":"pane_closed","pane":pane}))
            .collect();
        assert_eq!(rows(&root.path(), before), expected);
        let response = app.handle_api_request(Request {
            id: "close-workspace".into(),
            method: Method::WorkspaceClose(crate::api::schema::WorkspaceCloseParams {
                workspace_id: app.public_workspace_id(0),
                close_group: false,
            }),
        });
        assert!(
            serde_json::from_str::<Value>(&response)
                .unwrap()
                .get("error")
                .is_none(),
            "{response}"
        );
        expected.push(json!({"type":"pane_closed","pane":first_public}));
        app.handle_internal_event(AppEvent::PaneDied {
            pane_id: first,
            exit_reason: crate::platform::ChildExitReason::Exited,
        });
        assert_eq!(rows(&root.path(), before), expected);
        assert!(app.state.workspaces.is_empty());
    }

    #[test]
    fn logbook_storage_failures_do_not_interrupt_reports_renames_or_close() {
        let _guard = crate::config::test_config_env_lock().lock().unwrap();
        for fail_at_start in [true, false] {
            let root = ConfigRoot::new();
            if fail_at_start {
                std::fs::create_dir_all(root.path()).unwrap();
            }
            let mut app = app();
            let saved = root.path().with_extension("saved");
            if !fail_at_start {
                std::fs::rename(root.path(), &saved).unwrap();
                std::fs::create_dir(root.path()).unwrap();
            }
            let pane_id = app.state.workspaces[0].tabs[0].root_pane;
            let pane = app.public_pane_id(0, pane_id).unwrap();
            let terminal = app.state.terminal_id_for_pane(0, pane_id).unwrap();
            report(&mut app, pane_id, "session", 1, "startup");
            rename(
                &mut app,
                &pane,
                Some("label survives"),
                Some(LabelSource::Editor),
            );
            let state = &app.state.terminals[&terminal];
            assert_eq!(
                state
                    .persisted_agent_session
                    .as_ref()
                    .unwrap()
                    .session_ref
                    .value,
                "session"
            );
            assert_eq!(state.manual_label.as_deref(), Some("label survives"));
            assert_eq!(state.label_source, Some(LabelSource::Editor));
            let response = app.handle_api_request(Request {
                id: "close".into(),
                method: Method::PaneClose(PaneTarget { pane_id: pane }),
            });
            assert!(
                serde_json::from_str::<Value>(&response)
                    .unwrap()
                    .get("error")
                    .is_none(),
                "{response}"
            );
            assert!(app.state.workspaces.is_empty());
            assert!(root.path().is_dir());
            drop(app);
            std::fs::remove_dir(root.path()).unwrap();
            if !fail_at_start {
                std::fs::rename(&saved, root.path()).unwrap();
            }

            let mut recovered = self::app();
            let pane_id = recovered.state.workspaces[0].tabs[0].root_pane;
            report(&mut recovered, pane_id, "recovered", 1, "startup");
            let events = crate::persist::logbook::read(&root.path()).unwrap();
            assert_eq!(events.len(), 1);
            assert_eq!(
                serde_json::to_value(&events[0]).unwrap()["key"]["value"],
                "recovered"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn logbook_failed_seed_is_retried_without_publishing_a_partial_layout() {
        use std::os::unix::ffi::OsStringExt;
        let _guard = crate::config::test_config_env_lock().lock().unwrap();
        let root = ConfigRoot::new();
        let mut app = app_with_policy(AppPolicy::TEST);
        let first = app.state.workspaces[0].tabs[0].root_pane;
        let second = app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
        app.state.ensure_test_terminals();
        for (pane_id, value) in [(first, "first"), (second, "second")] {
            let pane = app.public_pane_id(0, pane_id).unwrap();
            rename(&mut app, &pane, Some(value), None);
            report(&mut app, pane_id, value, 1, "startup");
        }
        let terminal = app.state.terminal_id_for_pane(0, second).unwrap();
        app.state.terminals.get_mut(&terminal).unwrap().cwd =
            std::ffi::OsString::from_vec(vec![0xff]).into();
        app.policy.persist_session = true;
        app.initialize_logbook();
        assert!(
            !root.path().exists(),
            "a failed seed must not mark initialization complete"
        );
        app.state.terminals.get_mut(&terminal).unwrap().cwd = "project".into();
        app.initialize_logbook();
        let events = crate::persist::logbook::read(&root.path()).unwrap();
        assert_eq!(events.len(), 4);
        assert_eq!(
            serde_json::to_value(&events[0]).unwrap()["key"]["value"],
            "first"
        );
        assert_eq!(
            serde_json::to_value(&events[2]).unwrap()["key"]["value"],
            "second"
        );
        app.initialize_logbook();
        assert_eq!(crate::persist::logbook::read(&root.path()).unwrap(), events);
    }

    #[test]
    fn logbook_repeated_accepted_report_recovers_a_failed_append() {
        let _guard = crate::config::test_config_env_lock().lock().unwrap();
        let root = ConfigRoot::new();
        let mut app = app();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let saved = root.path().with_extension("saved");
        std::fs::rename(root.path(), &saved).unwrap();
        std::fs::create_dir(root.path()).unwrap();
        report(&mut app, pane_id, "session", 1, "startup");
        assert!(std::fs::read(&saved).unwrap().is_empty());
        std::fs::remove_dir(root.path()).unwrap();
        std::fs::rename(saved, root.path()).unwrap();
        report(&mut app, pane_id, "session", 2, "resume");
        let events = crate::persist::logbook::read(&root.path()).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            serde_json::to_value(&events[0]).unwrap()["key"]["value"],
            "session"
        );
        report(&mut app, pane_id, "session", 3, "resume");
        assert_eq!(crate::persist::logbook::read(&root.path()).unwrap(), events);
    }
}
