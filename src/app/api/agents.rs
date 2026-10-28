use std::time::Duration;

use bytes::Bytes;

use crate::api::schema::{
    AgentPromptParams, AgentRenameParams, AgentSendKeysParams, AgentStartParams, AgentTarget,
    PaneReadResult, ResponseResult,
};
use crate::app::App;

use super::responses::{encode_error, encode_error_body, encode_success};

const AGENT_PROMPT_SUBMIT_DELAY: Duration = Duration::from_millis(300);

// Codex's Windows input reader does not surface bracketed paste. It detects the prompt as a
// "paste burst" and, while that burst is buffered, rewrites a following Enter into a newline
// instead of submitting. The burst only flushes after an idle timeout, so any size-based delay is
// a timing guess that fails when ConPTY delivery lags it. Codex flushes a buffered burst
// synchronously when it receives a non-character key, so appending one after the paste gives the
// submission a deterministic paste boundary regardless of prompt size or delivery speed.
#[cfg(windows)]
fn append_codex_paste_boundary(runtime: &crate::terminal::TerminalRuntime, text: &mut Vec<u8>) {
    let keys = match crate::app::api_helpers::encode_api_keys(runtime, &["right".to_string()]) {
        Ok(keys) => keys,
        Err(key) => {
            tracing::warn!(key = %key, "failed to encode Codex paste boundary key");
            return;
        }
    };
    if let Some(key) = keys.into_iter().find(|bytes| !bytes.is_empty()) {
        text.extend_from_slice(&key);
    }
}

impl App {
    pub(super) fn handle_agent_list(&mut self, id: String) -> String {
        encode_success(
            id,
            ResponseResult::AgentList {
                agents: self.collect_agent_infos(),
            },
        )
    }

    pub(super) fn handle_agent_get(&mut self, id: String, target: AgentTarget) -> String {
        self.reconcile_managed_agent_target(&target.target);
        let agent = match self.agent_info_for_target(&target.target) {
            Ok(agent) => agent,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };

        encode_success(id, ResponseResult::AgentInfo { agent })
    }

    pub(super) fn handle_agent_focus(&mut self, id: String, target: AgentTarget) -> String {
        let agent = match self.focus_agent_target(&target.target) {
            Ok(agent) => agent,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };

        encode_success(id, ResponseResult::AgentInfo { agent })
    }

    pub(super) fn handle_agent_rename(&mut self, id: String, params: AgentRenameParams) -> String {
        let agent = match self.rename_agent_target(&params.target, params.name) {
            Ok(agent) => agent,
            Err(err) => return encode_error_body(id, self.agent_rename_error_body(err)),
        };

        encode_success(id, ResponseResult::AgentInfo { agent })
    }

    pub(super) fn handle_agent_start(&mut self, id: String, params: AgentStartParams) -> String {
        let (agent, argv) = match self.start_agent(params) {
            Ok(started) => started,
            Err(err) => return encode_error_body(id, self.agent_start_error_body(err)),
        };

        encode_success(id, ResponseResult::AgentStarted { agent, argv })
    }

    pub(crate) fn handle_deferred_agent_api_request(
        &mut self,
        request: crate::api::schema::Request,
        respond_to: std::sync::mpsc::Sender<String>,
    ) -> bool {
        let crate::api::schema::Method::AgentPrompt(params) = request.method else {
            return false;
        };
        let validated = match self.validate_agent_prompt(request.id, &params) {
            Ok(validated) => validated,
            Err(response) => {
                let _ = respond_to.send(response);
                return true;
            }
        };
        // Start the completion waiter before submitting, so a refused thread
        // fails the request while nothing has been sent yet.
        let (handoff_tx, handoff_rx) = std::sync::mpsc::channel::<(
            String,
            crate::api::schema::AgentInfo,
            std::sync::mpsc::Receiver<std::io::Result<()>>,
        )>();
        let waiter_respond_to = respond_to.clone();
        let spawned = crate::thread_spawn::spawn_named("herdr-agent-prompt", move || {
            let Ok((id, agent, completion)) = handoff_rx.recv() else {
                return;
            };
            let response = match completion.recv() {
                Ok(Ok(())) => encode_success(id, ResponseResult::AgentPrompted { agent }),
                Ok(Err(err)) => {
                    if let Some(rejected) = crate::pty::actor::submission_rejected_response(&err) {
                        rejected.to_owned()
                    } else if err.kind() == std::io::ErrorKind::TimedOut {
                        encode_error(id, "timeout", err.to_string())
                    } else {
                        encode_error(id, "agent_prompt_failed", err.to_string())
                    }
                }
                Err(_) => encode_error(id, "agent_prompt_failed", "pty actor closed"),
            };
            let _ = waiter_respond_to.send(response);
        });
        if let Err(err) = spawned {
            tracing::warn!(err = %err, "failed to spawn agent prompt thread");
            let _ = respond_to.send(encode_error(
                validated.id,
                "agent_prompt_failed",
                format!("could not start prompt completion waiter: {err}"),
            ));
            return true;
        }
        match self.submit_agent_prompt(validated, &params) {
            Ok(submission) => {
                let _ = handoff_tx.send(submission);
            }
            Err(response) => {
                let _ = respond_to.send(response);
            }
        }
        true
    }

    /// Runs every check that can reject a prompt without touching the pane.
    fn validate_agent_prompt(
        &self,
        id: String,
        params: &AgentPromptParams,
    ) -> Result<ValidatedAgentPrompt, String> {
        if params.text.is_empty() {
            return Err(encode_error(
                id,
                "empty_agent_prompt",
                "agent prompt must not be empty",
            ));
        }
        if params
            .if_status
            .contains(&crate::api::schema::AgentStatus::Unknown)
        {
            return Err(encode_error(
                id,
                "invalid_request",
                "if_status only accepts idle, done, blocked, or working",
            ));
        }
        let target_is_pane_id = self.parse_current_public_pane_id(&params.target).is_some();
        let resolved = match self
            .resolve_agent_prompt_target(&params.target, params.expect_terminal.as_deref())
        {
            Ok(resolved) => resolved,
            Err(err) => return Err(encode_error_body(id, self.agent_target_error_body(err))),
        };
        let Some(workspace) = self.state.workspaces.get(resolved.ws_idx) else {
            return Err(agent_not_found(id, &params.target));
        };
        let Some(pane_state) = workspace.pane_state(resolved.pane_id) else {
            return Err(agent_not_found(id, &params.target));
        };
        let terminal_id = pane_state.attached_terminal_id.clone();
        let Some(terminal) = self.state.terminals.get(&terminal_id) else {
            return Err(agent_not_found(id, &params.target));
        };
        pane_state.sync_delivery_snapshot();
        terminal.sync_delivery_snapshot();
        let pane_snapshot_handle = pane_state.delivery_snapshot_handle();
        let terminal_snapshot_handle = terminal.delivery_snapshot_handle();
        let Some(runtime) = self.lookup_runtime_sender(resolved.ws_idx, resolved.pane_id) else {
            if params.expect_terminal.is_some()
                || params.expect_agent.is_some()
                || params.expect_session.is_some()
            {
                let pane_snapshot = pane_snapshot_handle
                    .upgrade()
                    .and_then(|arc| arc.lock().ok().map(|guard| guard.clone()));
                let terminal_snapshot = terminal_snapshot_handle
                    .upgrade()
                    .and_then(|arc| arc.lock().ok().map(|guard| guard.clone()));
                let dummy_pid = std::sync::atomic::AtomicU32::new(0);
                validate_agent_prompt_delivery(
                    &id,
                    params,
                    target_is_pane_id,
                    pane_snapshot.as_ref(),
                    terminal_snapshot.as_ref(),
                    &dummy_pid,
                    #[cfg(test)]
                    None,
                )?;
            }
            return Err(agent_not_found(id, &params.target));
        };
        let child_pid_handle = runtime.child_pid_handle();
        #[cfg(test)]
        let test_foreground_agent = runtime.test_foreground_agent_handle();
        let expected_agent = {
            let pane_snapshot = pane_snapshot_handle
                .upgrade()
                .and_then(|arc| arc.lock().ok().map(|guard| guard.clone()));
            let terminal_snapshot = terminal_snapshot_handle
                .upgrade()
                .and_then(|arc| arc.lock().ok().map(|guard| guard.clone()));
            validate_agent_prompt_delivery(
                &id,
                params,
                target_is_pane_id,
                pane_snapshot.as_ref(),
                terminal_snapshot.as_ref(),
                &child_pid_handle,
                #[cfg(test)]
                test_foreground_agent.as_ref(),
            )?
        };
        let Some(agent) = self.agent_info(resolved.ws_idx, resolved.pane_id) else {
            return Err(agent_not_found(id, &params.target));
        };
        let guard_id = id.clone();
        let guard_params: AgentPromptParams = params.clone();
        let guard: crate::pty::actor::PtySubmissionGuard = Box::new(move || {
            let pane_snapshot = match pane_snapshot_handle.upgrade() {
                Some(arc) => Some(
                    arc.lock()
                        .map_err(|_| std::io::Error::other("pane delivery snapshot poisoned"))?
                        .clone(),
                ),
                None => None,
            };
            let terminal_snapshot = match terminal_snapshot_handle.upgrade() {
                Some(arc) => Some(
                    arc.lock()
                        .map_err(|_| std::io::Error::other("terminal delivery snapshot poisoned"))?
                        .clone(),
                ),
                None => None,
            };
            validate_agent_prompt_delivery(
                &guard_id,
                &guard_params,
                target_is_pane_id,
                pane_snapshot.as_ref(),
                terminal_snapshot.as_ref(),
                &child_pid_handle,
                #[cfg(test)]
                test_foreground_agent.as_ref(),
            )
            .map(|_| ())
            .map_err(crate::pty::actor::submission_rejected_error)
        });
        Ok(ValidatedAgentPrompt {
            id,
            ws_idx: resolved.ws_idx,
            pane_id: resolved.pane_id,
            expected_agent,
            agent,
            guard,
        })
    }

    fn submit_agent_prompt(
        &mut self,
        validated: ValidatedAgentPrompt,
        params: &AgentPromptParams,
    ) -> Result<
        (
            String,
            crate::api::schema::AgentInfo,
            std::sync::mpsc::Receiver<std::io::Result<()>>,
        ),
        String,
    > {
        let ValidatedAgentPrompt {
            id,
            ws_idx,
            pane_id,
            expected_agent,
            agent,
            guard,
        } = validated;
        let Some(runtime) = self.lookup_runtime_sender(ws_idx, pane_id) else {
            return Err(agent_not_found(id, &params.target));
        };
        #[cfg(windows)]
        let submit_deadline = params
            .wait
            .as_ref()
            .and_then(|wait| wait.submission_deadline);
        #[cfg(not(windows))]
        let submit_deadline = None;
        let prefix = if expected_agent == crate::detect::Agent::GithubCopilot {
            // Copilot ignores synthetic Enter after focus loss until it receives focus gained.
            let focus = match crate::ghostty::encode_focus(crate::ghostty::FocusEvent::Gained) {
                Ok(focus) => focus,
                Err(err) => {
                    return Err(encode_error(id, "agent_prompt_failed", err.to_string()));
                }
            };
            Bytes::from(focus)
        } else {
            Bytes::new()
        };
        let (text, enter) =
            crate::app::api_helpers::encode_api_submission_parts(runtime, &params.text);
        #[cfg(windows)]
        let text = if expected_agent == crate::detect::Agent::Codex {
            let mut text = text;
            append_codex_paste_boundary(runtime, &mut text);
            text
        } else {
            text
        };
        let completion = runtime
            .queue_user_input_submission(
                prefix,
                Bytes::from(text),
                Bytes::from(enter),
                AGENT_PROMPT_SUBMIT_DELAY,
                submit_deadline,
                Some(guard),
            )
            .map_err(|err| encode_error(id.clone(), "agent_prompt_failed", err.to_string()))?;
        Ok((id, agent, completion))
    }

    pub(super) fn handle_agent_read(
        &mut self,
        id: String,
        params: crate::api::schema::AgentReadParams,
    ) -> String {
        let resolved = match self.resolve_agent_target(&params.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        let Some((pane, workspace_id)) = self.lookup_runtime(resolved.ws_idx, resolved.pane_id)
        else {
            return agent_not_found(id, &params.target);
        };
        let (text, truncated, effective_offset, has_more) = match super::pane_read_window(
            pane,
            params.source,
            params.format,
            params.lines,
            params.offset_from_bottom,
            None,
        ) {
            Ok(window) => window,
            Err(message) => return encode_error(id, "invalid_request", message),
        };

        encode_success(
            id,
            ResponseResult::PaneRead {
                read: PaneReadResult {
                    pane_id: self
                        .public_pane_id(resolved.ws_idx, resolved.pane_id)
                        .unwrap_or_else(|| params.target.clone()),
                    workspace_id,
                    tab_id: self
                        .public_tab_id(resolved.ws_idx, resolved.tab_idx)
                        .unwrap(),
                    source: params.source,
                    cols: Some(pane.current_size().1),
                    format: params.format,
                    text,
                    revision: 0,
                    truncated,
                    effective_offset,
                    has_more,
                },
            },
        )
    }

    pub(super) fn handle_agent_explain(&mut self, id: String, target: AgentTarget) -> String {
        let resolved = match self.resolve_agent_target(&target.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        let Some((pane, _workspace_id)) = self.lookup_runtime(resolved.ws_idx, resolved.pane_id)
        else {
            return agent_not_found(id, &target.target);
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|workspace| workspace.terminal_id(resolved.pane_id))
        else {
            return agent_not_found(id, &target.target);
        };
        let Some(terminal) = self.state.terminals.get(terminal_id) else {
            return agent_not_found(id, &target.target);
        };
        if terminal.full_lifecycle_hook_authority_active() {
            let explain = serde_json::json!({
                "agent": terminal.effective_agent_label().unwrap_or("unknown"),
                "state": crate::detect::manifest::agent_state_label(terminal.state),
                "manifest_source": null,
                "manifest_version": null,
                "cached_remote_version": null,
                "local_override_shadowing_remote": false,
                "remote_update_status": null,
                "remote_update_error": null,
                "matched_rule": null,
                "visible_idle": false,
                "visible_blocker": false,
                "visible_working": false,
                "screen_detection_skipped": true,
                "screen_detection_skip_reason": "full_lifecycle_hook_authority",
                "skip_state_update": false,
                "skipped_update_reason": null,
                "fallback_reason": null,
                "warning": null,
                "evaluated_rules": [],
            });
            return encode_success(id, ResponseResult::AgentExplain { explain });
        }
        let Some(agent) = terminal.effective_known_agent().or(terminal.detected_agent) else {
            return encode_error(
                id,
                "agent_explain_unavailable",
                format!(
                    "agent target {} does not have a detected agent label",
                    target.target
                ),
            );
        };

        let screen = pane.detection_text();
        let osc_title = pane.agent_osc_title();
        let osc_progress = pane.agent_osc_progress();
        let explain = crate::detect::manifest::explain_with_input(
            agent,
            crate::detect::manifest::DetectionInput {
                screen: &screen,
                osc_title: &osc_title,
                osc_progress: &osc_progress,
            },
        );
        let value = crate::detect::manifest::explain_to_json_value(&explain);

        encode_success(id, ResponseResult::AgentExplain { explain: value })
    }

    pub(super) fn handle_agent_send_keys(
        &mut self,
        id: String,
        params: AgentSendKeysParams,
    ) -> String {
        let resolved = match self.resolve_agent_target(&params.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|workspace| workspace.terminal_id(resolved.pane_id))
        else {
            return agent_not_found(id, &params.target);
        };
        let Some(expected_agent) = self
            .state
            .terminals
            .get(terminal_id)
            .and_then(|terminal| terminal.effective_known_agent())
        else {
            return agent_not_ready(id, &params.target);
        };
        let Some(runtime) = self.lookup_runtime_sender(resolved.ws_idx, resolved.pane_id) else {
            return agent_not_found(id, &params.target);
        };
        if !super::super::agents::runtime_hosts_agent(runtime, expected_agent) {
            return agent_not_ready(id, &params.target);
        }
        let encoded = match super::super::api_helpers::encode_api_keys(runtime, &params.keys) {
            Ok(encoded) => encoded,
            Err(key) => {
                return encode_error(id, "invalid_key", format!("unsupported key {key}"));
            }
        };
        let bytes: Vec<u8> = encoded.into_iter().flatten().collect();
        if let Err(err) = runtime.try_send_bytes(Bytes::from(bytes)) {
            return encode_error(id, "agent_send_keys_failed", err.to_string());
        }

        encode_success(id, ResponseResult::Ok {})
    }
}

struct ValidatedAgentPrompt {
    id: String,
    ws_idx: usize,
    pane_id: crate::layout::PaneId,
    expected_agent: crate::detect::Agent,
    agent: crate::api::schema::AgentInfo,
    guard: crate::pty::actor::PtySubmissionGuard,
}

#[allow(clippy::too_many_arguments)]
fn validate_agent_prompt_delivery(
    id: &str,
    params: &AgentPromptParams,
    target_is_pane_id: bool,
    pane_snapshot: Option<&crate::pane::PaneDeliverySnapshot>,
    terminal_snapshot: Option<&crate::terminal::state::TerminalDeliverySnapshot>,
    child_pid_handle: &std::sync::atomic::AtomicU32,
    #[cfg(test)] test_foreground_agent: Option<
        &std::sync::Arc<std::sync::Mutex<Option<Option<crate::detect::Agent>>>>,
    >,
) -> Result<crate::detect::Agent, String> {
    let has_identity_expectations = params.expect_terminal.is_some()
        || params.expect_agent.is_some()
        || params.expect_session.is_some();
    let (Some(pane_snapshot), Some(terminal_snapshot)) = (pane_snapshot, terminal_snapshot) else {
        if has_identity_expectations {
            return Err(encode_error(
                id.to_string(),
                "agent_mismatch",
                format!(
                    "agent target {} is no longer hosted by the expected terminal",
                    params.target
                ),
            ));
        }
        return Err(agent_not_found(id.to_string(), &params.target));
    };
    if pane_snapshot.attached_terminal_id != terminal_snapshot.id {
        if has_identity_expectations {
            return Err(encode_error(
                id.to_string(),
                "agent_mismatch",
                format!(
                    "agent target {} is no longer hosted by terminal {}",
                    params.target, terminal_snapshot.id
                ),
            ));
        }
        return Err(agent_not_found(id.to_string(), &params.target));
    }
    if let Some(expected_terminal) = params.expect_terminal.as_deref() {
        let actual_terminal = terminal_snapshot.id.to_string();
        if actual_terminal != expected_terminal {
            return Err(encode_error(
                id.to_string(),
                "agent_mismatch",
                format!(
                    "agent target {} has terminal {actual_terminal}, expected {expected_terminal}",
                    params.target
                ),
            ));
        }
    }
    if !target_is_pane_id && terminal_snapshot.agent_name.as_deref() != Some(params.target.as_str())
    {
        return Err(encode_error(
            id.to_string(),
            "agent_mismatch",
            format!(
                "agent target {} is no longer hosted by terminal {}",
                params.target, terminal_snapshot.id
            ),
        ));
    }
    if let Some(expected_agent) = params.expect_agent.as_deref() {
        let actual_agent = terminal_snapshot
            .is_agent_terminal
            .then_some(terminal_snapshot.effective_agent_label.as_deref())
            .flatten();
        if actual_agent != Some(expected_agent) {
            return Err(encode_error(
                id.to_string(),
                "agent_mismatch",
                format!(
                    "agent target {} has agent {}, expected {expected_agent}",
                    params.target,
                    actual_agent.unwrap_or("none")
                ),
            ));
        }
    }
    if let Some(expected_session) = params.expect_session.as_deref() {
        let actual_session = terminal_snapshot
            .is_agent_terminal
            .then_some(terminal_snapshot.session_value.as_deref())
            .flatten();
        if actual_session != Some(expected_session) {
            return Err(encode_error(
                id.to_string(),
                "agent_mismatch",
                format!(
                    "agent target {} has session {}, expected {expected_session}",
                    params.target,
                    actual_session.unwrap_or("none")
                ),
            ));
        }
    }
    if !terminal_snapshot.is_agent_terminal {
        if has_identity_expectations {
            return Err(encode_error(
                id.to_string(),
                "agent_mismatch",
                format!(
                    "agent target {} does not currently host an agent",
                    params.target
                ),
            ));
        }
        return Err(agent_not_found(id.to_string(), &params.target));
    }
    let Some(expected_agent) = terminal_snapshot.effective_known_agent else {
        if has_identity_expectations {
            return Err(encode_error(
                id.to_string(),
                "agent_mismatch",
                format!(
                    "agent target {} does not currently host an interactive agent",
                    params.target
                ),
            ));
        }
        return Err(agent_not_ready(id.to_string(), &params.target));
    };
    if terminal_snapshot.managed_agent_launch_pending {
        return Err(agent_not_ready(id.to_string(), &params.target));
    }
    if !super::super::agents::runtime_handles_host_agent(
        child_pid_handle,
        #[cfg(test)]
        test_foreground_agent,
        expected_agent,
    ) {
        let code = if has_identity_expectations {
            "agent_mismatch"
        } else {
            "agent_not_ready"
        };
        return Err(encode_error(
            id.to_string(),
            code,
            format!(
                "agent {} is no longer the pane foreground process",
                params.target
            ),
        ));
    }
    if !params.if_status.is_empty() {
        let current_status = pane_snapshot.reported_agent_status(
            crate::app::api_helpers::pane_agent_status(terminal_snapshot.state, pane_snapshot.seen),
        );
        if !params.if_status.contains(&current_status) {
            return Err(encode_error(
                id.to_string(),
                "agent_busy",
                format!(
                    "agent {} has status {:?}, which is not in expected if_status",
                    params.target, current_status
                ),
            ));
        }
    } else if terminal_snapshot.state == crate::detect::AgentState::Blocked {
        return Err(encode_error(
            id.to_string(),
            "agent_blocked",
            format!(
                "agent {} is blocked and requires interactive input",
                params.target
            ),
        ));
    }
    Ok(expected_agent)
}

fn agent_not_ready(id: String, target: &str) -> String {
    encode_error(
        id,
        "agent_not_ready",
        format!("agent {target} is not an active named agent"),
    )
}

fn agent_not_found(id: String, target: &str) -> String {
    encode_error(
        id,
        "agent_not_found",
        format!("agent target {target} not found"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::schema::{AgentStatus, SuccessResponse},
        app::Mode,
        config::Config,
        detect::{Agent, AgentState},
        workspace::Workspace,
    };

    fn app_with_agent() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("agent")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = Mode::Terminal;
        app
    }

    fn start_deferred_agent_prompt(
        app: &mut App,
        id: &str,
        params: AgentPromptParams,
    ) -> std::sync::mpsc::Receiver<String> {
        let (respond_to, response_rx) = std::sync::mpsc::channel();
        assert!(app.handle_deferred_agent_api_request(
            crate::api::schema::Request {
                id: id.into(),
                method: crate::api::schema::Method::AgentPrompt(params),
            },
            respond_to,
        ));
        response_rx
    }

    fn run_deferred_agent_prompt(app: &mut App, id: &str, params: AgentPromptParams) -> String {
        start_deferred_agent_prompt(app, id, params)
            .recv_timeout(Duration::from_secs(1))
            .expect("agent prompt responds after submission")
    }

    fn prompt_params(target: impl Into<String>, text: impl Into<String>) -> AgentPromptParams {
        AgentPromptParams {
            target: target.into(),
            text: text.into(),
            expect_terminal: None,
            expect_agent: None,
            expect_session: None,
            if_status: Vec::new(),
            wait: None,
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_codex_prompt_flushes_paste_burst_before_enter() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::Codex), AgentState::Idle);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);

        let response =
            run_deferred_agent_prompt(&mut app, "req", prompt_params("reviewer", "A != B"));
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            success.result,
            ResponseResult::AgentPrompted { .. }
        ));
        // The non-character key must precede Enter so Codex commits the paste burst first.
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"A != B\x1b[C"));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\r"));
    }

    #[tokio::test]
    async fn a_false_process_exit_makes_a_named_live_agent_unreachable_by_name() {
        // Reproduces the registration loss reported on #3225 by rszrszrsz:
        // a live agent pane with an assigned name stops resolving by that name
        // while its process keeps running, and renaming is the only recovery.
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let observed_at = std::time::Instant::now();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Working);
        terminal.set_agent_name("reviewer".into());

        let found = app.handle_agent_get(
            "req:before".into(),
            AgentTarget {
                target: "reviewer".into(),
            },
        );
        assert!(
            serde_json::from_str::<SuccessResponse>(&found).is_ok(),
            "the assigned name must resolve while the agent is running: {found}"
        );

        // One process-exit observation, then the same agent is observed alive
        // again on the next probe - the process never actually went away.
        app.handle_internal_event(crate::events::AppEvent::StateChanged {
            pane_id,
            agent: Some(Agent::Pi),
            state: AgentState::Idle,
            visible_blocker: false,
            visible_working: false,
            process_exited: true,
            observed_at,
            reading: crate::events::StatusReading::Verdict,
        });
        app.handle_internal_event(crate::events::AppEvent::AgentProcessDetected {
            pane_id,
            agent: Agent::Pi,
            observed_at: observed_at + std::time::Duration::from_secs(1),
            reading: crate::events::StatusReading::Provisional,
        });

        let terminal = &app.state.terminals[&terminal_id];
        assert_eq!(
            terminal.detected_agent,
            Some(Agent::Pi),
            "the agent process is still there"
        );

        let after = app.handle_agent_get(
            "req:after".into(),
            AgentTarget {
                target: "reviewer".into(),
            },
        );
        assert!(
            serde_json::from_str::<SuccessResponse>(&after).is_ok(),
            "a live agent must stay reachable by its assigned name: {after}"
        );
    }

    #[tokio::test]
    async fn agent_prompt_sends_text_then_delays_enter() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::OpenCode), AgentState::Working);
        let (runtime, mut rx) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                80, 24, 0, b"", 2,
            );
        runtime.test_process_pty_bytes(b"\x1b[?2004h");
        app.state.insert_test_runtime(pane_id, runtime);

        let public_pane_id = app.public_pane_id(0, pane_id).unwrap();
        let bracketed_started = std::time::Instant::now();
        let response_rx =
            start_deferred_agent_prompt(&mut app, "req", prompt_params(public_pane_id, "A != B"));
        assert!(response_rx.try_recv().is_err());
        let response = response_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("agent prompt responds after submission");
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::AgentPrompted { agent, .. } = success.result else {
            panic!("expected prompted response");
        };
        assert_eq!(agent.name.as_deref(), Some("reviewer"));
        assert_eq!(
            rx.try_recv().unwrap(),
            Bytes::from_static(b"\x1b[200~A != B\x1b[201~")
        );
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\r"));
        assert!(bracketed_started.elapsed() >= AGENT_PROMPT_SUBMIT_DELAY);

        app.lookup_runtime_sender(0, pane_id)
            .unwrap()
            .test_process_pty_bytes(b"\x1b[?2004l");
        let raw_started = std::time::Instant::now();
        let raw =
            run_deferred_agent_prompt(&mut app, "req-raw", prompt_params("reviewer", "A != B"));
        let raw: SuccessResponse = serde_json::from_str(&raw).unwrap();
        assert!(matches!(raw.result, ResponseResult::AgentPrompted { .. }));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"A != B"));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\r"));
        assert!(raw_started.elapsed() >= AGENT_PROMPT_SUBMIT_DELAY);

        let rejected = run_deferred_agent_prompt(
            &mut app,
            "req-label",
            prompt_params("opencode", "wrong target"),
        );
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&rejected).unwrap();
        assert_eq!(error.error.code, "agent_not_found");
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn agent_prompt_waiter_spawn_failure_fails_before_submitting() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::OpenCode), AgentState::Idle);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);

        crate::thread_spawn::test_hook::fail_next_spawns(1);
        let response = run_deferred_agent_prompt(
            &mut app,
            "req",
            AgentPromptParams {
                target: "reviewer".into(),
                text: "hello".into(),
                wait: None,
                expect_terminal: None,
                expect_agent: None,
                expect_session: None,
                if_status: Vec::new(),
            },
        );

        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "agent_prompt_failed");
        std::thread::sleep(AGENT_PROMPT_SUBMIT_DELAY * 2);
        assert!(rx.try_recv().is_err(), "a failed prompt must not be sent");
    }

    #[tokio::test]
    async fn agent_prompt_validation_errors_win_over_waiter_spawn_failure() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::OpenCode), AgentState::Idle);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);

        for (target, text, code) in [
            ("reviewer", "", "empty_agent_prompt"),
            ("missing", "hello", "agent_not_found"),
        ] {
            crate::thread_spawn::test_hook::fail_next_spawns(1);
            let response = run_deferred_agent_prompt(
                &mut app,
                "req",
                AgentPromptParams {
                    target: target.into(),
                    text: text.into(),
                    wait: None,
                    expect_terminal: None,
                    expect_agent: None,
                    expect_session: None,
                    if_status: Vec::new(),
                },
            );

            let error: crate::api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
            assert_eq!(error.error.code, code);
            assert!(
                crate::thread_spawn::spawn_named("probe", || {}).is_err(),
                "a rejected prompt must not start a waiter thread"
            );
        }
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn agent_prompt_rejects_blocked_agent_without_writing() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::GithubCopilot), AgentState::Blocked);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);

        let response = run_deferred_agent_prompt(
            &mut app,
            "req",
            prompt_params("reviewer", "unrelated prompt"),
        );

        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "agent_blocked");
        assert!(
            tokio::time::timeout(
                AGENT_PROMPT_SUBMIT_DELAY + Duration::from_millis(100),
                rx.recv()
            )
            .await
            .is_err(),
            "blocked prompt wrote or scheduled terminal input"
        );
    }

    #[tokio::test]
    async fn agent_prompt_focuses_copilot_before_submitting() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::GithubCopilot), AgentState::Idle);
        let (runtime, mut rx) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                80, 24, 0, b"", 3,
            );
        runtime.test_process_pty_bytes(b"\x1b[?2004h");
        app.state.insert_test_runtime(pane_id, runtime);

        let response =
            run_deferred_agent_prompt(&mut app, "req", prompt_params("reviewer", "A != B"));
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            success.result,
            ResponseResult::AgentPrompted { .. }
        ));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\x1b[I"));
        assert_eq!(
            rx.try_recv().unwrap(),
            Bytes::from_static(b"\x1b[200~A != B\x1b[201~")
        );
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\r"));
    }

    #[tokio::test]
    async fn agent_send_keys_validates_every_key_before_writing() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);

        let rejected = app.handle_agent_send_keys(
            "req-invalid".into(),
            AgentSendKeysParams {
                target: "reviewer".into(),
                keys: vec!["enter".into(), "not-a-key".into()],
            },
        );
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&rejected).unwrap();
        assert_eq!(error.error.code, "invalid_key");
        assert!(rx.try_recv().is_err());

        let sent = app.handle_agent_send_keys(
            "req-valid".into(),
            AgentSendKeysParams {
                target: "reviewer".into(),
                keys: vec!["up".into(), "enter".into()],
            },
        );
        let success: SuccessResponse = serde_json::from_str(&sent).unwrap();
        assert!(matches!(success.result, ResponseResult::Ok {}));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\x1b[A\r"));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn agent_prompt_rejects_managed_agent_while_startup_is_pending() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        let now = std::time::Instant::now();
        terminal.begin_managed_agent(
            "reviewer".into(),
            Agent::OpenCode,
            now,
            std::time::Duration::from_secs(3),
            std::time::Duration::from_secs(10),
        );
        terminal.set_detected_state(Some(Agent::OpenCode), AgentState::Idle);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);

        let response =
            run_deferred_agent_prompt(&mut app, "req-pending", prompt_params("reviewer", "A != B"));
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "agent_not_ready");
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn agent_read_preserves_osc8_hyperlinks() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_detected_state(Some(Agent::OpenCode), AgentState::Working);
        let runtime = crate::terminal::TerminalRuntime::test_with_scrollback_bytes(
            40,
            5,
            100_000,
            b"\x1b]8;;https://example.com\x1b\\docs\x1b]8;;\x1b\\\r\nplain",
        );
        app.state.insert_test_runtime(pane_id, runtime);
        let target = app.public_pane_id(0, pane_id).unwrap();
        for source in [
            crate::api::schema::ReadSource::Recent,
            crate::api::schema::ReadSource::RecentUnwrapped,
        ] {
            for offset in [None, Some(0), Some(1)] {
                let response = app.handle_agent_read(
                    "read".into(),
                    crate::api::schema::AgentReadParams {
                        target: target.clone(),
                        source,
                        lines: Some(1),
                        format: crate::api::schema::ReadFormat::Ansi,
                        strip_ansi: false,
                        offset_from_bottom: offset,
                    },
                );
                let success: SuccessResponse = serde_json::from_str(&response).unwrap();
                let ResponseResult::PaneRead { read } = success.result else {
                    panic!("expected pane read response");
                };
                if offset == Some(1) {
                    assert_eq!(
                        read.text,
                        "\x1b]8;;https://example.com\x1b\\docs\x1b]8;;\x1b\\"
                    );
                } else {
                    assert_eq!(read.text, "plain");
                }
                assert_eq!(read.cols, Some(40));
                assert_eq!(read.effective_offset, offset);
            }
        }
    }

    #[tokio::test]
    async fn agent_read_reports_current_column_width() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_detected_state(Some(Agent::OpenCode), AgentState::Working);
        let runtime = crate::terminal::TerminalRuntime::test_with_screen_bytes(73, 5, b"> ");
        app.state.insert_test_runtime(pane_id, runtime);
        let target = app.public_pane_id(0, pane_id).unwrap();
        let response = app.handle_agent_read(
            "width".into(),
            crate::api::schema::AgentReadParams {
                target,
                source: crate::api::schema::ReadSource::Visible,
                lines: Some(5),
                format: crate::api::schema::ReadFormat::Text,
                strip_ansi: true,
                offset_from_bottom: None,
            },
        );
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::PaneRead { read } = success.result else {
            panic!("expected pane read response");
        };
        assert_eq!(read.cols, Some(73));
    }

    #[tokio::test]
    async fn agent_read_supports_offset_from_bottom() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        // Agent targets only resolve pane ids that currently host an agent.
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::OpenCode), AgentState::Working);
        let mut bytes = Vec::new();
        for line in 0..30 {
            bytes.extend_from_slice(format!("line {line:02}\r\n").as_bytes());
        }
        bytes.extend_from_slice(b"> ");
        let runtime =
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(20, 5, 100_000, &bytes);
        app.state.insert_test_runtime(pane_id, runtime);
        let target = app.public_pane_id(0, pane_id).unwrap();

        let read_params = |offset_from_bottom, source| crate::api::schema::AgentReadParams {
            target: target.clone(),
            source,
            lines: Some(10),
            format: crate::api::schema::ReadFormat::Text,
            strip_ansi: true,
            offset_from_bottom,
        };

        let response = app.handle_agent_read(
            "req".into(),
            read_params(Some(10), crate::api::schema::ReadSource::Recent),
        );
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::PaneRead { read } = success.result else {
            panic!("expected pane read response");
        };
        assert_eq!(read.effective_offset, Some(10));
        assert_eq!(read.has_more, Some(true));
        assert!(read.text.contains("line 11") && read.text.contains("line 20"));
        assert!(!read.text.contains("line 21"));

        let rejected = app.handle_agent_read(
            "req".into(),
            read_params(Some(1), crate::api::schema::ReadSource::Visible),
        );
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&rejected).unwrap();
        assert_eq!(error.error.code, "invalid_request");
    }

    #[test]
    fn agent_focus_marks_already_focused_done_agent_seen() {
        let mut app = app_with_agent();
        app.state.outer_terminal_focus = Some(false);

        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_detected_state(Some(Agent::Pi), AgentState::Idle);
        app.state.workspaces[0].tabs[0]
            .panes
            .get_mut(&pane_id)
            .unwrap()
            .seen = false;
        app.state.workspaces[0].tabs[0].layout.focus_pane(pane_id);

        let response = app.handle_agent_focus(
            "req".into(),
            AgentTarget {
                target: app.public_pane_id(0, pane_id).unwrap(),
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::AgentInfo { agent } = success.result else {
            panic!("expected agent info response");
        };
        assert_eq!(agent.agent_status, AgentStatus::Idle);
    }

    #[test]
    fn agent_rename_does_not_replace_the_pane_label() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_manual_label("shell-pane".into());
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        let target = app.public_pane_id(0, pane_id).unwrap();

        for name in [Some("reviewer".to_string()), None] {
            let response = app.handle_agent_rename(
                "req".into(),
                AgentRenameParams {
                    target: target.clone(),
                    name,
                },
            );
            let success: SuccessResponse = serde_json::from_str(&response).unwrap();
            assert!(matches!(success.result, ResponseResult::AgentInfo { .. }));
            assert_eq!(
                app.state.terminals[&terminal_id].manual_label.as_deref(),
                Some("shell-pane")
            );
        }
    }

    #[tokio::test]
    async fn agent_prompt_checks_expected_terminal_atomically() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let public_pane_id = app.public_pane_id(0, pane_id).unwrap();
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);

        // Even when the pane currently hosts only a shell, both mismatched and matching
        // terminal expectations report agent_mismatch without writing because no agent is live.
        let mut mismatched_shell = prompt_params(&public_pane_id, "do not send");
        mismatched_shell.expect_terminal = Some("term_other".into());
        let rejected = run_deferred_agent_prompt(&mut app, "req-shell-mismatch", mismatched_shell);
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&rejected).unwrap();
        assert_eq!(error.error.code, "agent_mismatch");
        assert!(rx.try_recv().is_err());

        let mut same_term_shell = prompt_params(&public_pane_id, "do not send");
        same_term_shell.expect_terminal = Some(terminal_id.to_string());
        let rejected = run_deferred_agent_prompt(&mut app, "req-same-term-shell", same_term_shell);
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&rejected).unwrap();
        assert_eq!(error.error.code, "agent_mismatch");
        assert!(rx.try_recv().is_err());

        let mut cleared_name = prompt_params("reviewer", "do not send");
        cleared_name.expect_terminal = Some(terminal_id.to_string());
        let rejected = run_deferred_agent_prompt(&mut app, "req-cleared-name", cleared_name);
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&rejected).unwrap();
        assert_eq!(error.error.code, "agent_mismatch");
        assert!(rx.try_recv().is_err());

        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);

        let mut mismatched = prompt_params("reviewer", "do not send");
        mismatched.expect_terminal = Some("term_other".into());
        let rejected = run_deferred_agent_prompt(&mut app, "req-mismatch", mismatched);
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&rejected).unwrap();
        assert_eq!(error.error.code, "agent_mismatch");
        assert!(rx.try_recv().is_err());

        let mut matched = prompt_params("reviewer", "deliver this");
        matched.expect_terminal = Some(terminal_id.to_string());
        let accepted = run_deferred_agent_prompt(&mut app, "req-match", matched);
        let success: SuccessResponse = serde_json::from_str(&accepted).unwrap();
        assert!(matches!(
            success.result,
            ResponseResult::AgentPrompted { .. }
        ));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"deliver this"));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\r"));
    }

    #[tokio::test]
    async fn agent_prompt_checks_expected_agent_and_session_atomically() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let public_pane_id = app.public_pane_id(0, pane_id).unwrap();
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);

        let mut shell_agent_mismatch = prompt_params(&public_pane_id, "do not send");
        shell_agent_mismatch.expect_agent = Some("claude".into());
        let rejected = run_deferred_agent_prompt(&mut app, "req-shell-agent", shell_agent_mismatch);
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&rejected).unwrap();
        assert_eq!(error.error.code, "agent_mismatch");
        assert!(rx.try_recv().is_err());

        // A persisted session left on a non-agent shell terminal does not satisfy expect_session.
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .persisted_agent_session = Some(crate::agent_resume::PersistedAgentSession {
            source: "test".into(),
            agent: "pi".into(),
            session_ref: crate::agent_resume::AgentSessionRef {
                kind: crate::agent_resume::AgentSessionRefKind::Id,
                value: "sess-123".into(),
                record_path: None,
            },
        });
        let mut shell_stale_session = prompt_params(&public_pane_id, "do not send");
        shell_stale_session.expect_session = Some("sess-123".into());
        let rejected =
            run_deferred_agent_prompt(&mut app, "req-shell-stale-session", shell_stale_session);
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&rejected).unwrap();
        assert_eq!(error.error.code, "agent_mismatch");
        assert!(rx.try_recv().is_err());

        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.persisted_agent_session = None;
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);

        let mut missing_session = prompt_params("reviewer", "do not send");
        missing_session.expect_session = Some("sess-123".into());
        let rejected = run_deferred_agent_prompt(&mut app, "req-no-session", missing_session);
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&rejected).unwrap();
        assert_eq!(error.error.code, "agent_mismatch");
        assert!(rx.try_recv().is_err());

        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.persisted_agent_session = Some(crate::agent_resume::PersistedAgentSession {
            source: "test".into(),
            agent: "pi".into(),
            session_ref: crate::agent_resume::AgentSessionRef {
                kind: crate::agent_resume::AgentSessionRefKind::Id,
                value: "sess-123".into(),
                record_path: None,
            },
        });

        let mut wrong_agent = prompt_params("reviewer", "do not send");
        wrong_agent.expect_agent = Some("claude".into());
        let rejected = run_deferred_agent_prompt(&mut app, "req-wrong-agent", wrong_agent);
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&rejected).unwrap();
        assert_eq!(error.error.code, "agent_mismatch");
        assert!(rx.try_recv().is_err());

        let mut wrong_session = prompt_params("reviewer", "do not send");
        wrong_session.expect_session = Some("sess-other".into());
        let rejected = run_deferred_agent_prompt(&mut app, "req-wrong-session", wrong_session);
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&rejected).unwrap();
        assert_eq!(error.error.code, "agent_mismatch");
        assert!(rx.try_recv().is_err());

        let mut matched = prompt_params("reviewer", "deliver this");
        matched.expect_agent = Some("pi".into());
        matched.expect_session = Some("sess-123".into());
        let accepted = run_deferred_agent_prompt(&mut app, "req-matched", matched);
        let success: SuccessResponse = serde_json::from_str(&accepted).unwrap();
        assert!(matches!(
            success.result,
            ResponseResult::AgentPrompted { .. }
        ));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"deliver this"));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\r"));
    }

    #[tokio::test]
    async fn agent_prompt_checks_if_status_atomically() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Working);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);

        let mut invalid_status = prompt_params("reviewer", "do not send");
        invalid_status.if_status = vec![AgentStatus::Unknown];
        let rejected = run_deferred_agent_prompt(&mut app, "req-invalid-status", invalid_status);
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&rejected).unwrap();
        assert_eq!(error.error.code, "invalid_request");
        assert!(rx.try_recv().is_err());

        let mut busy_working = prompt_params("reviewer", "do not send");
        busy_working.if_status = vec![AgentStatus::Idle, AgentStatus::Done];
        let rejected = run_deferred_agent_prompt(&mut app, "req-busy-working", busy_working);
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&rejected).unwrap();
        assert_eq!(error.error.code, "agent_busy");
        assert!(rx.try_recv().is_err());

        // Transition to Done (Idle + seen=false).
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_detected_state(Some(Agent::Pi), AgentState::Idle);
        app.state.workspaces[0].tabs[0]
            .panes
            .get_mut(&pane_id)
            .unwrap()
            .seen = false;

        let mut only_idle = prompt_params("reviewer", "do not send");
        only_idle.if_status = vec![AgentStatus::Idle];
        let rejected = run_deferred_agent_prompt(&mut app, "req-only-idle", only_idle);
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&rejected).unwrap();
        assert_eq!(error.error.code, "agent_busy");
        assert!(rx.try_recv().is_err());

        let mut idle_or_done = prompt_params("reviewer", "deliver on done");
        idle_or_done.if_status = vec![AgentStatus::Idle, AgentStatus::Done];
        let accepted = run_deferred_agent_prompt(&mut app, "req-done-match", idle_or_done);
        let success: SuccessResponse = serde_json::from_str(&accepted).unwrap();
        assert!(matches!(
            success.result,
            ResponseResult::AgentPrompted { .. }
        ));
        assert_eq!(
            rx.try_recv().unwrap(),
            Bytes::from_static(b"deliver on done")
        );
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\r"));

        // Transition to Blocked: rejected with agent_busy when if_status excludes blocked,
        // and delivered when if_status explicitly includes blocked.
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_detected_state(Some(Agent::Pi), AgentState::Blocked);

        let mut blocked_excluded = prompt_params("reviewer", "do not send");
        blocked_excluded.if_status = vec![AgentStatus::Idle, AgentStatus::Done];
        let rejected =
            run_deferred_agent_prompt(&mut app, "req-blocked-excluded", blocked_excluded);
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&rejected).unwrap();
        assert_eq!(error.error.code, "agent_busy");
        assert!(rx.try_recv().is_err());

        let mut blocked_included = prompt_params("reviewer", "answer blocker");
        blocked_included.if_status = vec![AgentStatus::Idle, AgentStatus::Blocked];
        let accepted =
            run_deferred_agent_prompt(&mut app, "req-blocked-included", blocked_included);
        let success: SuccessResponse = serde_json::from_str(&accepted).unwrap();
        assert!(matches!(
            success.result,
            ResponseResult::AgentPrompted { .. }
        ));
        assert_eq!(
            rx.try_recv().unwrap(),
            Bytes::from_static(b"answer blocker")
        );
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\r"));
    }

    #[tokio::test]
    async fn agent_prompt_rechecks_if_status_at_deferred_delivery_boundary() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        let (held_rx, release_tx) = runtime.test_hold_next_submission();
        app.state.insert_test_runtime(pane_id, runtime);

        let first_rx =
            start_deferred_agent_prompt(&mut app, "req-first", prompt_params("reviewer", "first"));
        held_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("first submission held after writing text");
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"first"));

        let mut second_params = prompt_params("reviewer", "second");
        second_params.if_status = vec![AgentStatus::Idle, AgentStatus::Done];
        let second_rx = start_deferred_agent_prompt(&mut app, "req-second", second_params);

        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_detected_state(Some(Agent::Pi), AgentState::Working);

        release_tx.send(()).unwrap();

        let first_response = first_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("first submission finishes");
        let first_success: SuccessResponse = serde_json::from_str(&first_response).unwrap();
        assert!(matches!(
            first_success.result,
            ResponseResult::AgentPrompted { .. }
        ));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\r"));

        let second_response = second_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("second submission refused at delivery boundary");
        let error: crate::api::schema::ErrorResponse =
            serde_json::from_str(&second_response).unwrap();
        assert_eq!(error.error.code, "agent_busy");
        assert!(
            rx.try_recv().is_err(),
            "refused deferred prompt must not write text or Enter"
        );
    }

    #[tokio::test]
    async fn agent_prompt_rechecks_identity_at_deferred_delivery_boundary() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::GithubCopilot), AgentState::Idle);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        let (held_rx, release_tx) = runtime.test_hold_next_submission();
        app.state.insert_test_runtime(pane_id, runtime);

        let first_rx =
            start_deferred_agent_prompt(&mut app, "req-first", prompt_params("reviewer", "first"));
        held_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("first submission held after writing text");
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\x1b[I"));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"first"));

        let mut second_params = prompt_params("reviewer", "second");
        second_params.expect_agent = Some("copilot".into());
        let second_rx = start_deferred_agent_prompt(&mut app, "req-second", second_params);

        app.lookup_runtime_sender(0, pane_id)
            .unwrap()
            .set_test_foreground_agent(Some(Some(Agent::Codex)));

        release_tx.send(()).unwrap();

        let first_response = first_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("first submission finishes");
        let first_success: SuccessResponse = serde_json::from_str(&first_response).unwrap();
        assert!(matches!(
            first_success.result,
            ResponseResult::AgentPrompted { .. }
        ));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\r"));

        let second_response = second_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("second submission refused at delivery boundary");
        let error: crate::api::schema::ErrorResponse =
            serde_json::from_str(&second_response).unwrap();
        assert_eq!(error.error.code, "agent_mismatch");
        assert!(
            rx.try_recv().is_err(),
            "refused deferred prompt must not write focus prefix, text, or Enter"
        );
    }
}
