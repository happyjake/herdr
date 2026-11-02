//! Debrief transitions are replayable Logbook facts, independent of eligibility.
use super::*;
use crate::api::schema::{AgentPromptParams, DebriefOutcome, Method, PaneCloseParams, Request};
use crate::persist::logbook::CloseDebrief;

pub(super) struct PendingDelivery {
    pane: String,
    text: String,
    persisted: std::sync::mpsc::Receiver<bool>,
}

pub(super) struct PendingSubmission {
    pane: String,
    quitting: bool,
    at: u64,
    response: std::sync::mpsc::Receiver<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Debrief {
    pub agent: String,
    pub session: Option<CloseSession>,
    pub age_secs: u64,
    pub input_at: u64,
    pub attempt: u8,
    pub phase: Phase,
    #[serde(default)]
    pub reply_start: Option<crate::pty::output::OutputPosition>,
    #[serde(default)]
    pub finished: Option<CloseDebrief>,
    #[serde(default)]
    pub quit_failures: u8,
    #[serde(default)]
    pub delivery_abort: Option<String>,
    #[serde(default)]
    pub recovered_delivery: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "kebab-case")]
pub(crate) enum Phase {
    Sent { at: u64 },
    Recovering { at: u64 },
    Running { at: u64 },
    Settling { at: u64, since: u64 },
    Quitting { at: u64, result: CloseDebrief },
    QuitRetry { at: u64 },
    Retry { reason: String },
    Waiting { reason: String },
    Aborted { reason: String },
}

impl Debrief {
    pub(super) fn deadline(&self, config: &Config) -> Option<u64> {
        let limit = kind_config_for(config, &self.agent)?
            .limit_secs
            .saturating_mul(1000);
        Some(match self.phase {
            Phase::Sent { at } => at.saturating_add(config.uptake_window_secs.saturating_mul(1000)),
            Phase::Recovering { at } | Phase::Running { at } => at.saturating_add(limit),
            Phase::Settling { at, since } => at
                .saturating_add(limit)
                .min(since.saturating_add(config.settle_hold_secs.saturating_mul(1000))),
            Phase::Quitting { at, .. } => {
                at.saturating_add(config.quit_grace_secs.saturating_mul(1000))
            }
            Phase::QuitRetry { at } => at,
            _ => return None,
        })
    }

    pub(super) fn active(&self) -> bool {
        matches!(
            self.phase,
            Phase::Sent { .. }
                | Phase::Recovering { .. }
                | Phase::Running { .. }
                | Phase::Settling { .. }
                | Phase::Quitting { .. }
                | Phase::QuitRetry { .. }
        ) || matches!(&self.phase, Phase::Waiting { reason } if reason == "delivery-pending")
    }

    pub(super) fn retry(&self, reason: &str) -> Phase {
        if self.recovered_delivery || self.attempt >= 3 {
            Phase::Waiting {
                reason: reason.into(),
            }
        } else {
            Phase::Retry {
                reason: reason.into(),
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Step {
    Wait,
    Change(Phase),
    Quit,
    QuitFailed(&'static str),
    Close,
}

fn elapsed(now: u64, at: u64, seconds: u64) -> bool {
    now >= at && now - at >= seconds.saturating_mul(1000)
}

pub(super) fn protected(pane: &PaneSnapshot, state: &Debrief) -> bool {
    let quit_exit = matches!(
        state.phase,
        Phase::Quitting { .. } | Phase::QuitRetry { .. }
    ) && pane.ended
        && pane.agent.is_none();
    pane.pinned
        || pane.flagged
        || pane.desk_focused
        || pane.group_refusal
        || pane.input_at != state.input_at
        || pane
            .agent
            .as_ref()
            .is_some_and(|agent| agent != &state.agent)
        || (pane.session != state.session && !(quit_exit && pane.session.is_none()))
}

pub(super) fn step(
    state: &Debrief,
    status: AgentStatus,
    ended: bool,
    config: &Config,
    now: u64,
) -> Step {
    if status == AgentStatus::Blocked {
        return Step::Change(Phase::Aborted {
            reason: "blocked".into(),
        });
    }
    let settled = matches!(status, AgentStatus::Done | AgentStatus::Idle);
    let limit = kind_config_for(config, &state.agent).map_or(0, |kind| kind.limit_secs);
    match &state.phase {
        Phase::Sent { at } => {
            if status == AgentStatus::Working && !elapsed(now, *at, config.uptake_window_secs) {
                Step::Change(Phase::Running { at: *at })
            } else if elapsed(now, *at, config.uptake_window_secs) {
                Step::Change(state.retry("no-uptake"))
            } else {
                Step::Wait
            }
        }
        Phase::Recovering { at } => {
            if elapsed(now, *at, limit) {
                Step::Change(Phase::Waiting {
                    reason: "recovered-limit".into(),
                })
            } else if status == AgentStatus::Working {
                Step::Change(Phase::Running { at: *at })
            } else {
                Step::Wait
            }
        }
        Phase::Running { at } | Phase::Settling { at, .. } => {
            if let Phase::Settling { since, .. } = state.phase {
                if settled
                    && elapsed(now, since, config.settle_hold_secs)
                    && since.saturating_add(config.settle_hold_secs.saturating_mul(1000))
                        <= at.saturating_add(limit.saturating_mul(1000))
                {
                    return Step::Quit;
                }
            }
            if elapsed(now, *at, limit) {
                Step::Change(state.retry("limit"))
            } else if settled && matches!(state.phase, Phase::Running { .. }) {
                Step::Change(Phase::Settling {
                    at: *at,
                    since: now,
                })
            } else if !settled && matches!(state.phase, Phase::Settling { .. }) {
                Step::Change(Phase::Running { at: *at })
            } else {
                Step::Wait
            }
        }
        Phase::Quitting { at, .. } if ended || elapsed(now, *at, config.quit_grace_secs) => {
            if status == AgentStatus::Working {
                Step::QuitFailed("working-limit")
            } else {
                Step::Close
            }
        }
        Phase::QuitRetry { .. } if ended => Step::Close,
        Phase::QuitRetry { at } if now >= *at => {
            if settled {
                Step::Quit
            } else {
                Step::QuitFailed("working-limit")
            }
        }
        _ => Step::Wait,
    }
}

pub(super) fn kind_config_for<'a>(config: &'a Config, agent: &str) -> Option<&'a KindConfig> {
    match agent {
        "claude" => Some(&config.claude),
        "codex" => Some(&config.codex),
        "pi" => Some(&config.pi),
        _ => None,
    }
}

fn sentinels(reply: &str) -> Vec<(String, u64)> {
    reply
        .lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("debrief v")?;
            let (version, rest) = rest.split_once(": ")?;
            if version.is_empty() || !version.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            let count = rest.strip_suffix(" memories saved")?.parse::<u64>().ok()?;
            Some((format!("v{version}"), count))
        })
        .collect()
}

fn finished(reply: &str) -> CloseDebrief {
    let current = sentinels(reply);
    let sentinel = current.last();
    CloseDebrief {
        outcome: DebriefOutcome::Finished,
        memories: sentinel.as_ref().map(|(_, count)| *count),
        skill_version: sentinel.map(|(version, _)| version.clone()),
    }
}

fn reply_text(bytes: &[u8], rows: u16, cols: u16) -> String {
    let cols = cols.max(1);
    let Ok(mut terminal) = crate::ghostty::Terminal::new(cols, rows.max(1), 256 * 1024) else {
        return String::new();
    };
    terminal.write(bytes);
    let Some(last) = terminal
        .total_rows()
        .ok()
        .and_then(|rows| rows.checked_sub(1))
        .and_then(|last| u32::try_from(last).ok())
    else {
        return String::new();
    };
    terminal
        .read_text_screen((0, 0), (cols - 1, last), false)
        .unwrap_or_default()
}

impl App {
    pub(crate) fn observe_debrief_status(&mut self, pane: &str, status: AgentStatus) {
        let Some(state) = self
            .auto_close
            .debriefs
            .get(pane)
            .cloned()
            .filter(Debrief::active)
        else {
            return;
        };
        let unresolved = self
            .parse_current_public_pane_id(pane)
            .and_then(|(ws, id)| self.state.workspaces.get(ws)?.pane_state(id))
            .and_then(|pane| pane.agent_status_resolution_due_at())
            .is_some_and(|until| crate::pane::unix_now_secs() < until);
        if unresolved && !matches!(status, AgentStatus::Working | AgentStatus::Blocked) {
            return;
        }
        if let Some(submission) = self
            .auto_close
            .submission
            .as_ref()
            .filter(|submission| submission.pane == pane)
        {
            if status == AgentStatus::Blocked {
                self.record_debrief_phase(
                    pane,
                    Phase::Aborted {
                        reason: "blocked".into(),
                    },
                );
            } else if !submission.quitting
                && status == AgentStatus::Working
                && state.delivery_abort.is_none()
            {
                self.record_debrief_phase(pane, Phase::Running { at: submission.at });
            }
            return;
        }
        if let Step::Change(phase) = step(
            &state,
            status,
            false,
            &self.auto_close.config,
            logbook::now_ms(),
        ) {
            self.record_debrief_phase(pane, phase);
        }
        if matches!(
            state.phase,
            Phase::Quitting { .. } | Phase::QuitRetry { .. }
        ) && status == AgentStatus::Unknown
        {
            self.auto_close.deadline = Some(Instant::now());
        }
    }

    fn record_debrief_phase(&mut self, pane: &str, mut phase: Phase) {
        let pending = self
            .auto_close
            .submission
            .as_ref()
            .is_some_and(|submission| submission.pane == pane);
        let Some(state) = self.auto_close.debriefs.get_mut(pane) else {
            return;
        };
        if pending {
            if let Phase::Aborted { reason } = &phase {
                state.delivery_abort = Some(reason.clone());
                phase = Phase::Waiting {
                    reason: "delivery-pending".into(),
                };
            }
        }
        if state.phase == phase {
            return;
        }
        if matches!(state.phase, Phase::Recovering { .. }) {
            state.recovered_delivery = true;
        }
        state.phase = phase;
        let event = if state.active() && !matches!(state.phase, Phase::Waiting { .. }) {
            Event::DebriefProgress {
                pane: pane.into(),
                state: state.clone(),
                at: logbook::now_ms(),
            }
        } else {
            Event::DebriefOutcome {
                pane: pane.into(),
                state: state.clone(),
                outcome: state.finished.clone().unwrap_or(CloseDebrief {
                    outcome: DebriefOutcome::Abandoned,
                    memories: None,
                    skill_version: None,
                }),
                at: logbook::now_ms(),
            }
        };
        let quiet = state.active() && state.delivery_abort.is_none();
        let retaining_output = state.active() && state.finished.is_none();
        self.set_debrief_quiet(pane, quiet);
        if !retaining_output {
            self.end_debrief_reply(pane);
        }
        self.state.enqueue_logbook_events([event]);
        self.flush_logbook();
        self.schedule_debrief_tick(Instant::now());
    }

    fn set_debrief_quiet(&mut self, pane: &str, quiet: bool) {
        let Some((ws, pane_id)) = self.parse_current_public_pane_id(pane) else {
            return;
        };
        if let Some(terminal) = self
            .state
            .terminal_id_for_pane(ws, pane_id)
            .and_then(|id| self.state.terminals.get_mut(&id))
        {
            terminal.debrief_quiet = quiet;
            if quiet {
                terminal.last_agent_completion_seq = None;
            }
        }
    }

    fn debrief_text(&self, pane: &str) -> String {
        let Some(position) = self
            .auto_close
            .debriefs
            .get(pane)
            .and_then(|state| state.reply_start)
        else {
            return String::new();
        };
        let Some((runtime, _)) = self
            .parse_current_public_pane_id(pane)
            .and_then(|(ws, id)| self.lookup_runtime(ws, id))
        else {
            return String::new();
        };
        let Some(reply) = runtime.read_output_after(position) else {
            return String::new();
        };
        let bytes = if reply.truncated {
            reply
                .bytes
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(&[][..], |index| &reply.bytes[index + 1..])
        } else {
            &reply.bytes
        };
        let (rows, cols) = runtime.current_size();
        reply_text(bytes, rows, cols)
    }

    fn begin_debrief_reply(&self, pane: &str) -> Option<crate::pty::output::OutputPosition> {
        self.parse_current_public_pane_id(pane)
            .and_then(|(ws, id)| self.lookup_runtime(ws, id))
            .map(|(runtime, _)| runtime.begin_output_capture())
    }

    fn end_debrief_reply(&self, pane: &str) {
        if let Some((runtime, _)) = self
            .parse_current_public_pane_id(pane)
            .and_then(|(ws, id)| self.lookup_runtime(ws, id))
        {
            runtime.end_output_capture();
        }
    }

    pub(super) fn start_debrief(&mut self, pane: &PaneSnapshot, candidate: &Candidate) {
        // A receipt still reserves the slot even if its pane has disappeared.
        if self.auto_close.submission.is_some() || self.auto_close.delivery.is_some() {
            return;
        }
        let Some(agent) = pane.agent.as_deref() else {
            return;
        };
        let Some(kind) = kind_config_for(&self.auto_close.config, agent) else {
            return;
        };
        let text = kind.invocation.clone();
        let state = Debrief {
            agent: agent.into(),
            session: pane.session.clone(),
            age_secs: candidate.age_secs,
            input_at: pane.input_at,
            attempt: self
                .auto_close
                .debriefs
                .get(&pane.pane_id)
                .map_or(1, |state| state.attempt.saturating_add(1)),
            phase: Phase::Sent {
                at: logbook::now_ms(),
            },
            reply_start: self.begin_debrief_reply(&pane.pane_id),
            finished: None,
            quit_failures: 0,
            delivery_abort: None,
            recovered_delivery: false,
        };
        self.set_debrief_quiet(&pane.pane_id, true);
        self.auto_close
            .debriefs
            .insert(pane.pane_id.clone(), state.clone());
        self.persist_debrief_input(
            &pane.pane_id,
            text,
            Event::DebriefSent {
                pane: pane.pane_id.clone(),
                state,
                at: logbook::now_ms(),
            },
        );
        if let Some((ws, id)) = self.parse_current_public_pane_id(&pane.pane_id) {
            self.emit_pane_kept_presentation_changed(ws, id);
        }
    }

    fn persist_debrief_input(&mut self, pane: &str, text: String, event: Event) {
        self.flush_logbook();
        if let Some(writer) = &self.logbook_writer {
            self.auto_close.delivery = Some(PendingDelivery {
                pane: pane.into(),
                text,
                persisted: writer.append_confirmed(vec![event]),
            });
        } else {
            self.record_debrief_phase(
                pane,
                Phase::Waiting {
                    reason: "logbook-unavailable".into(),
                },
            );
        }
    }

    fn send_debrief_prompt(&mut self, pane: &str, text: String) {
        let Some(state) = self.auto_close.debriefs.get(pane) else {
            return;
        };
        let quitting = text == "/quit";
        if quitting {
            self.preserve_debrief_quit_activity(pane, self.auto_close.config.quit_grace_secs);
        }
        let delivery_limit = if quitting {
            self.auto_close.config.quit_grace_secs
        } else {
            self.auto_close.config.uptake_window_secs
        };
        let params = AgentPromptParams {
            target: pane.into(),
            text,
            preserve_activity: true,
            expect_terminal: self
                .parse_current_public_pane_id(pane)
                .and_then(|(ws, id)| self.state.terminal_id_for_pane(ws, id))
                .map(|id| id.to_string()),
            expect_agent: Some(state.agent.clone()),
            expect_session: state.session.as_ref().map(|session| session.id.clone()),
            if_status: vec![AgentStatus::Idle, AgentStatus::Done],
            wait: Some(crate::api::schema::AgentPromptWaitOptions {
                until: Vec::new(),
                timeout_ms: None,
                submission_deadline: Instant::now()
                    .checked_add(Duration::from_secs(delivery_limit)),
            }),
        };
        let (sender, response) = std::sync::mpsc::channel();
        let _deadline_scope = Instant::now()
            .checked_add(Duration::from_secs(delivery_limit))
            .and_then(|deadline| {
                self.parse_current_public_pane_id(pane)
                    .and_then(|(ws, id)| self.lookup_runtime(ws, id))
                    .map(|(runtime, _)| runtime.scope_submission_deadline(deadline))
            });
        self.handle_internal_agent_prompt(
            Request {
                id: "auto-close-debrief".into(),
                method: Method::AgentPrompt(params),
            },
            sender,
        );
        self.auto_close.submission = Some(PendingSubmission {
            pane: pane.into(),
            quitting,
            at: logbook::now_ms(),
            response,
        });
    }

    pub(super) fn advance_debriefs(&mut self) {
        let missing: Vec<_> = self
            .auto_close
            .debriefs
            .keys()
            .filter(|pane| self.parse_current_public_pane_id(pane).is_none())
            .cloned()
            .collect();
        for pane in missing {
            self.auto_close.debriefs.remove(&pane);
            self.auto_close.close_probe_after.remove(&pane);
        }
        if !self.auto_close.debriefs.values().any(Debrief::active)
            && self.auto_close.delivery.is_none() && self.auto_close.submission.is_none() { return; }
        let now = logbook::now_ms();
        let panes = self.auto_close_snapshot_for(now / 1000, true);
        for pane in &panes {
            let Some(state) = self.auto_close.debriefs.get(&pane.pane_id) else {
                continue;
            };
            if protected(pane, state) {
                self.record_debrief_phase(
                    &pane.pane_id,
                    Phase::Aborted {
                        reason: "pane-changed".into(),
                    },
                );
            }
        }
        if let Some(delivery) = self.auto_close.delivery.take() {
            match delivery.persisted.try_recv() {
                Ok(true) => {
                    if self
                        .auto_close
                        .debriefs
                        .get(&delivery.pane)
                        .is_some_and(Debrief::active)
                    {
                        self.send_debrief_prompt(&delivery.pane, delivery.text);
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    self.auto_close.delivery = Some(delivery);
                    return;
                }
                _ => self.record_debrief_phase(
                    &delivery.pane,
                    Phase::Waiting {
                        reason: "logbook-unavailable".into(),
                    },
                ),
            }
        }
        if let Some(submission) = self.auto_close.submission.take() {
            match submission.response.try_recv() {
                Ok(response) => {
                    let accepted = serde_json::from_str::<serde_json::Value>(&response)
                        .ok()
                        .is_some_and(|value| value.get("result").is_some());
                    if let Some(state) = self
                        .auto_close
                        .debriefs
                        .get(&submission.pane)
                        .cloned()
                        .filter(Debrief::active)
                    {
                        if let Some(reason) = state.delivery_abort {
                            self.record_debrief_phase(&submission.pane, Phase::Aborted { reason });
                        } else if !accepted && submission.quitting {
                            self.debrief_quit_failed(&submission.pane, "delivery");
                        } else if matches!(&state.phase, Phase::Waiting { reason } if reason == "delivery-pending")
                        {
                            self.record_debrief_phase(
                                &submission.pane,
                                Phase::Waiting {
                                    reason: "delivery-timeout".into(),
                                },
                            );
                        } else if !accepted {
                            self.record_debrief_phase(&submission.pane, state.retry("no-uptake"));
                        } else if submission.quitting {
                            if let Phase::Quitting { result, .. } = state.phase {
                                self.preserve_debrief_quit_activity(
                                    &submission.pane,
                                    self.auto_close.config.quit_grace_secs,
                                );
                                self.record_debrief_phase(
                                    &submission.pane,
                                    Phase::Quitting { at: now, result },
                                );
                            }
                        } else if matches!(state.phase, Phase::Sent { .. }) {
                            self.record_debrief_phase(&submission.pane, Phase::Sent { at: now });
                        }
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    let expired = self
                        .auto_close
                        .debriefs
                        .get(&submission.pane)
                        .filter(|state| state.active())
                        .filter(|state| {
                            kind_config_for(&self.auto_close.config, &state.agent)
                                .is_some_and(|kind| elapsed(now, submission.at, kind.limit_secs))
                        })
                        .cloned();
                    let pane = submission.pane.clone();
                    self.auto_close.submission = Some(submission);
                    if expired.is_some() {
                        self.record_debrief_phase(
                            &pane,
                            Phase::Waiting {
                                reason: "delivery-pending".into(),
                            },
                        );
                    }
                    return;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    if self
                        .auto_close
                        .debriefs
                        .get(&submission.pane)
                        .is_some_and(Debrief::active)
                    {
                        // Losing the receipt is not evidence that the write stopped.
                        // Keep the slot until the pane goes away or a restart cancels its writer.
                        self.record_debrief_phase(
                            &submission.pane,
                            Phase::Waiting {
                                reason: "delivery-pending".into(),
                            },
                        );
                    }
                }
            }
        }
        for pane in panes {
            let Some(state) = self
                .auto_close
                .debriefs
                .get(&pane.pane_id)
                .cloned()
                .filter(Debrief::active)
            else {
                continue;
            };
            let restoring = pane.restore_until.is_some_and(|until| now / 1000 < until);
            match step(
                &state,
                if restoring
                    && !matches!(state.phase, Phase::Settling { .. } | Phase::Quitting { .. })
                {
                    AgentStatus::Unknown
                } else {
                    pane.status
                },
                pane.ended && pane.bare_prompt,
                &self.auto_close.config,
                now,
            ) {
                Step::Wait => {}
                Step::Change(phase) => self.record_debrief_phase(&pane.pane_id, phase),
                Step::QuitFailed(reason) => self.debrief_quit_failed(&pane.pane_id, reason),
                Step::Quit => {
                    if restoring {
                        continue;
                    }
                    let text = self.debrief_text(&pane.pane_id);
                    let result = state.finished.unwrap_or_else(|| finished(&text));
                    if let Some(current) = self.auto_close.debriefs.get_mut(&pane.pane_id) {
                        current.finished = Some(result.clone());
                    }
                    self.record_debrief_phase(
                        &pane.pane_id,
                        Phase::Quitting {
                            at: now,
                            result: result.clone(),
                        },
                    );
                    self.persist_debrief_input(
                        &pane.pane_id,
                        "/quit".into(),
                        Event::DebriefOutcome {
                            pane: pane.pane_id.clone(),
                            state: self.auto_close.debriefs[&pane.pane_id].clone(),
                            outcome: result,
                            at: now,
                        },
                    );
                }
                Step::Close => {
                    if restoring {
                        continue;
                    }
                    if let Some(result) = state.finished.or_else(|| {
                        if let Phase::Quitting { result, .. } = state.phase {
                            Some(result)
                        } else {
                            None
                        }
                    }) {
                        self.close_dormant_pane(&pane, state.age_secs, result);
                    }
                }
            }
        }
    }

    fn debrief_quit_failed(&mut self, pane: &str, reason: &str) {
        let Some(state) = self.auto_close.debriefs.get_mut(pane) else {
            return;
        };
        state.quit_failures = state.quit_failures.saturating_add(1);
        let phase = if state.quit_failures >= 3 {
            Phase::Waiting {
                reason: format!("quit-{reason}"),
            }
        } else {
            Phase::QuitRetry {
                at: logbook::now_ms().saturating_add(
                    self.auto_close
                        .config
                        .pass_floor_secs
                        .max(1)
                        .saturating_mul(1000),
                ),
            }
        };
        self.record_debrief_phase(pane, phase);
    }

    fn preserve_debrief_quit_activity(&self, pane: &str, seconds: u64) {
        if let Some((runtime, _)) = self
            .parse_current_public_pane_id(pane)
            .and_then(|(ws, id)| self.lookup_runtime(ws, id))
        {
            runtime.preserve_foreground_activity_for(Duration::from_secs(seconds));
        }
    }

    pub(super) fn close_dormant_pane(
        &mut self,
        observed: &PaneSnapshot,
        age: u64,
        debrief: CloseDebrief,
    ) {
        let now = crate::pane::unix_now_secs();
        let mut fresh = self.auto_close_snapshot(now);
        if observed.debrief.is_some() {
            let after = *self
                .auto_close
                .close_probe_after
                .entry(observed.pane_id.clone())
                .or_insert_with(Instant::now);
            if !self.auto_close.probes.refresh(&mut fresh, after) {
                return;
            }
        }
        let Ok(candidate) = decide(observed, &fresh, &self.auto_close.config, now) else {
            return;
        };
        if !candidate_ready_to_close(
            &candidate,
            std::slice::from_ref(observed),
            &fresh,
            &self.auto_close.refused,
            &mut HashSet::new(),
            &self.auto_close.config,
            now,
        ) {
            return;
        }
        let pane = observed.pane_id.as_str();
        let session = self
            .auto_close
            .debriefs
            .get(pane)
            .and_then(|state| state.session.clone());
        if self
            .close_pane_with_auto_close(
                "auto-close".into(),
                &PaneCloseParams {
                    pane_id: pane.into(),
                    by: Some(CloseBy::Desk),
                    reason: Some("dormant".into()),
                },
                Some((age, debrief, session)),
            )
            .is_err()
        {
            return;
        }
        self.auto_close.debriefs.remove(pane);
        self.auto_close.close_probe_after.remove(pane);
        self.flush_logbook();
    }

    pub(crate) fn annotate_auto_close(
        &mut self,
        pane: &str,
        details: Option<(u64, CloseDebrief, Option<CloseSession>)>,
    ) {
        let Some((age, debrief, hosting)) = details else {
            return;
        };
        if let Some(events) = &mut self.state.logbook_events {
            for event in events.iter_mut().rev() {
                if let Event::PaneClosed {
                    pane: closed,
                    age_secs,
                    debrief: result,
                    session,
                    ..
                } = event
                {
                    if closed == pane {
                        *age_secs = Some(age);
                        *result = Some(debrief);
                        if session.is_none() {
                            *session = hosting;
                        }
                        break;
                    }
                }
            }
        }
    }

    pub(super) fn restore_debriefs(&mut self, states: HashMap<String, Debrief>) {
        self.auto_close.debriefs = states;
        let mut live = HashSet::new();
        for (ws_idx, workspace) in self.state.workspaces.iter().enumerate() {
            for pane_id in workspace.tabs.iter().flat_map(|tab| tab.panes.keys()) {
                if let Some(id) = self.public_pane_id(ws_idx, *pane_id) {
                    live.insert(id);
                }
            }
        }
        self.auto_close
            .debriefs
            .retain(|pane, _| live.contains(pane));
        for (pane, state) in self.auto_close.debriefs.clone() {
            if let Phase::Quitting { result, .. } = &state.phase {
                if let Some(restored) = self.auto_close.debriefs.get_mut(&pane) {
                    restored.finished.get_or_insert_with(|| result.clone());
                }
            }
            if state.active() && state.finished.is_none() {
                let retained = state.reply_start.is_some_and(|position| {
                    self.parse_current_public_pane_id(&pane)
                        .and_then(|(ws, id)| self.lookup_runtime(ws, id))
                        .and_then(|(runtime, _)| runtime.read_output_after(position))
                        .is_some()
                });
                if !retained {
                    let position = self.begin_debrief_reply(&pane);
                    if let Some(restored) = self.auto_close.debriefs.get_mut(&pane) {
                        restored.reply_start = position;
                    }
                }
            }
            if let Phase::Quitting { at, .. } = state.phase {
                let remaining = at
                    .saturating_add(self.auto_close.config.quit_grace_secs.saturating_mul(1000))
                    .saturating_sub(logbook::now_ms())
                    .div_ceil(1000);
                self.preserve_debrief_quit_activity(&pane, remaining);
            }
            if let Phase::Sent { at } = state.phase {
                if let Some(restored) = self.auto_close.debriefs.get_mut(&pane) {
                    restored.phase = Phase::Recovering { at };
                    restored.recovered_delivery = true;
                }
            }
            if matches!(&state.phase, Phase::Waiting { reason } if reason == "delivery-pending") {
                if let Some(restored) = self.auto_close.debriefs.get_mut(&pane) {
                    restored.phase = Phase::Waiting {
                        reason: "delivery-interrupted".into(),
                    };
                }
            }
            let active = self
                .auto_close
                .debriefs
                .get(&pane)
                .is_some_and(Debrief::active);
            self.set_debrief_quiet(&pane, active && state.delivery_abort.is_none());
            if !active {
                self.end_debrief_reply(&pane);
            }
        }
        self.auto_close
            .status
            .0
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .waiting = self
            .auto_close
            .debriefs
            .values()
            .filter(|state| matches!(state.phase, Phase::Waiting { .. }))
            .count() as u64;
        self.schedule_debrief_tick(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(phase: Phase) -> Debrief {
        Debrief {
            agent: "pi".into(),
            session: None,
            age_secs: 300,
            input_at: 100,
            attempt: 1,
            phase,
            reply_start: None,
            finished: None,
            quit_failures: 0,
            delivery_abort: None,
            recovered_delivery: false,
        }
    }

    #[test]
    fn debrief_uptake_requires_working_before_the_deadline() {
        let config = Config::default();
        let sent = state(Phase::Sent { at: 1000 });
        assert_eq!(sent.deadline(&config), Some(61_000));
        assert_eq!(
            step(&sent, AgentStatus::Done, false, &config, 60_999),
            Step::Wait
        );
        assert_eq!(
            step(&sent, AgentStatus::Working, false, &config, 60_999),
            Step::Change(Phase::Running { at: 1000 })
        );
        assert_eq!(
            step(&sent, AgentStatus::Idle, false, &config, 61_000),
            Step::Change(Phase::Retry {
                reason: "no-uptake".into()
            })
        );
        let third = Debrief { attempt: 3, ..sent };
        assert_eq!(
            step(&third, AgentStatus::Idle, false, &config, 61_000),
            Step::Change(Phase::Waiting {
                reason: "no-uptake".into()
            })
        );
    }

    #[test]
    fn debrief_settle_must_fit_inside_the_kind_limit() {
        let config = Config::default();
        let mut running = state(Phase::Running { at: 1000 });
        for (agent, limit) in [("claude", 600_000), ("pi", 600_000), ("codex", 900_000)] {
            running.agent = agent.into();
            assert_eq!(running.deadline(&config), Some(limit + 1000));
            assert_eq!(
                step(&running, AgentStatus::Working, false, &config, limit),
                Step::Wait
            );
            assert_eq!(
                step(&running, AgentStatus::Working, false, &config, limit + 1000),
                Step::Change(Phase::Retry {
                    reason: "limit".into()
                })
            );
            let mut settling = running.clone();
            settling.phase = Phase::Settling {
                at: 1000,
                since: limit - 59_000,
            };
            assert_eq!(
                step(&settling, AgentStatus::Idle, false, &config, limit + 1000),
                Step::Quit
            );
            settling.phase = Phase::Settling {
                at: 1000,
                since: limit - 58_999,
            };
            assert_eq!(
                step(&settling, AgentStatus::Idle, false, &config, limit + 1001),
                Step::Change(Phase::Retry {
                    reason: "limit".into()
                })
            );
        }
    }

    #[test]
    fn debrief_quit_grace_and_blocked_are_independent_of_agent_exit() {
        let config = Config::default();
        let quitting = state(Phase::Quitting {
            at: 1000,
            result: finished(""),
        });
        assert_eq!(
            step(&quitting, AgentStatus::Working, false, &config, 60_999),
            Step::Wait
        );
        assert_ne!(
            step(&quitting, AgentStatus::Working, false, &config, 61_000),
            Step::Close
        );
        assert_eq!(
            step(&quitting, AgentStatus::Working, false, &config, 61_000),
            Step::QuitFailed("working-limit")
        );
        assert_eq!(
            step(&quitting, AgentStatus::Idle, false, &config, 61_000),
            Step::Close
        );
        let retry = Debrief {
            finished: Some(finished("debrief v1: 1 memories saved")),
            ..state(Phase::QuitRetry { at: 10_000 })
        };
        assert_eq!(
            step(&retry, AgentStatus::Unknown, true, &config, 1000),
            Step::Close
        );
        assert_eq!(
            step(&quitting, AgentStatus::Unknown, true, &config, 1001),
            Step::Close
        );
        assert_eq!(
            step(&quitting, AgentStatus::Blocked, false, &config, 61_000),
            Step::Change(Phase::Aborted {
                reason: "blocked".into()
            })
        );
    }

    #[test]
    fn debrief_sentinel_uses_the_last_complete_line() {
        let result = finished("debrief v1: 8 memories saved\ndebrief v12: 0 memories saved\nNOT LANDED: topic — debrief\n");
        assert_eq!(result.skill_version.as_deref(), Some("v12"));
        assert_eq!(result.memories, Some(0));
        for text in [
            "debrief v: 1 memories saved",
            "debrief v2: nope memories saved",
            "quoted debrief v2: 1 memories saved",
            "debrief v2: 1 memories saved extra",
        ] {
            assert_eq!(finished(text).memories, None);
        }
        assert_eq!(
            finished("debrief v1: 8 memories saved\ndebrief v1: 8 memories saved").memories,
            Some(8)
        );
    }

    #[test]
    fn debrief_reply_capture_interprets_cursor_addressing_and_styles() {
        let text = reply_text(b"old line\x1b[2;1H\x1b[32mdebrief v2: 3 memories saved\x1b[0m\x1b[3;1HNOT LANDED: topic", 8, 80);
        assert_eq!(finished(&text).memories, Some(3));
        assert_eq!(finished(&text).skill_version.as_deref(), Some("v2"));
    }

    #[test]
    fn debrief_recovery_waits_without_mistaking_idle_for_an_answer() {
        let config = Config::default();
        let recovering = state(Phase::Recovering { at: 1000 });
        assert_eq!(
            step(&recovering, AgentStatus::Idle, false, &config, 601_000),
            Step::Change(Phase::Waiting {
                reason: "recovered-limit".into()
            })
        );
        assert_eq!(
            step(&recovering, AgentStatus::Done, false, &config, 120_000),
            Step::Wait
        );
        assert_eq!(
            step(&recovering, AgentStatus::Unknown, false, &config, 120_000),
            Step::Wait
        );
        assert_eq!(
            step(&recovering, AgentStatus::Working, false, &config, 120_000),
            Step::Change(Phase::Running { at: 1000 })
        );
    }

    #[test]
    fn debrief_recovered_delivery_never_retries_the_skill_after_late_uptake() {
        let config = Config::default();
        let running = Debrief {
            recovered_delivery: true,
            ..state(Phase::Running { at: 1000 })
        };
        assert_eq!(
            step(&running, AgentStatus::Working, false, &config, 601_000),
            Step::Change(Phase::Waiting {
                reason: "limit".into()
            })
        );
        assert!(state(Phase::Waiting {
            reason: "delivery-pending".into()
        })
        .active());
        assert!(!state(Phase::Waiting {
            reason: "limit".into()
        })
        .active());
    }
}
