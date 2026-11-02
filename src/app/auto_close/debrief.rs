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
    /// Prior sentinels distinguish this reply without retaining conversation text.
    pub before: Vec<(String, u64)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "kebab-case")]
pub(crate) enum Phase {
    Sent { at: u64 },
    Recovering { at: u64 },
    Running { at: u64 },
    Settling { at: u64, since: u64 },
    Quitting { at: u64, result: CloseDebrief },
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
        )
    }

    pub(super) fn retry(&self, reason: &str) -> Phase {
        if self.attempt >= 3 {
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
enum Step {
    Wait,
    Change(Phase),
    Quit,
    Close,
}

fn elapsed(now: u64, at: u64, seconds: u64) -> bool {
    now >= at && now - at >= seconds.saturating_mul(1000)
}

pub(super) fn protected(pane: &PaneSnapshot, state: &Debrief) -> bool {
    let quit_exit =
        matches!(state.phase, Phase::Quitting { .. }) && pane.ended && pane.agent.is_none();
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

fn step(state: &Debrief, status: AgentStatus, ended: bool, config: &Config, now: u64) -> Step {
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
                Step::Change(state.retry("limit"))
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
            Step::Close
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

fn finished(reply: &str, before: &[(String, u64)]) -> CloseDebrief {
    let current = sentinels(reply);
    let overlap = (0..=before.len().min(current.len()))
        .rev()
        .find(|length| before[before.len() - length..] == current[..*length])
        .unwrap_or(0);
    let sentinel = current[overlap..].last();
    CloseDebrief {
        outcome: DebriefOutcome::Finished,
        memories: sentinel.as_ref().map(|(_, count)| *count),
        skill_version: sentinel.map(|(version, _)| version.clone()),
    }
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
        if let Step::Change(phase) = step(
            &state,
            status,
            false,
            &self.auto_close.config,
            logbook::now_ms(),
        ) {
            self.record_debrief_phase(pane, phase);
        }
        if matches!(state.phase, Phase::Quitting { .. }) && status == AgentStatus::Unknown {
            self.auto_close.deadline = Some(Instant::now());
        }
    }

    fn record_debrief_phase(&mut self, pane: &str, phase: Phase) {
        let Some(state) = self.auto_close.debriefs.get_mut(pane) else {
            return;
        };
        if state.phase == phase {
            return;
        }
        state.phase = phase;
        let event = if state.active() {
            Event::DebriefProgress {
                pane: pane.into(),
                state: state.clone(),
                at: logbook::now_ms(),
            }
        } else {
            Event::DebriefOutcome {
                pane: pane.into(),
                state: state.clone(),
                outcome: CloseDebrief {
                    outcome: DebriefOutcome::Abandoned,
                    memories: None,
                    skill_version: None,
                },
                at: logbook::now_ms(),
            }
        };
        let quiet = state.active();
        self.set_debrief_quiet(pane, quiet);
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
        self.parse_current_public_pane_id(pane)
            .and_then(|(ws, id)| self.lookup_runtime(ws, id))
            .map(|(runtime, _)| runtime.recent_unwrapped_text_snapshot(4000).text)
            .unwrap_or_default()
    }

    pub(super) fn start_debrief(&mut self, pane: &PaneSnapshot, candidate: &Candidate) {
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
            before: sentinels(&self.debrief_text(&pane.pane_id)),
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
        self.handle_deferred_agent_api_request(
            Request {
                id: "auto-close-debrief".into(),
                method: Method::AgentPrompt(params),
            },
            sender,
        );
        self.auto_close.submission = Some(PendingSubmission {
            pane: pane.into(),
            quitting,
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
        }
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
                        if !accepted {
                            self.record_debrief_phase(&submission.pane, state.retry("no-uptake"));
                        } else if submission.quitting {
                            if let Phase::Quitting { result, .. } = state.phase {
                                self.record_debrief_phase(
                                    &submission.pane,
                                    Phase::Quitting { at: now, result },
                                );
                            }
                        }
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    let expired = self
                        .auto_close
                        .debriefs
                        .get(&submission.pane)
                        .filter(|state| state.active())
                        .filter(|state| match state.phase {
                            Phase::Sent { at } => {
                                elapsed(now, at, self.auto_close.config.uptake_window_secs)
                            }
                            Phase::Quitting { at, .. } => {
                                elapsed(now, at, self.auto_close.config.quit_grace_secs)
                            }
                            _ => false,
                        })
                        .cloned();
                    if let Some(state) = expired {
                        self.record_debrief_phase(&submission.pane, state.retry("no-uptake"));
                    } else {
                        self.auto_close.submission = Some(submission);
                        return;
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    if let Some(state) = self
                        .auto_close
                        .debriefs
                        .get(&submission.pane)
                        .cloned()
                        .filter(Debrief::active)
                    {
                        self.record_debrief_phase(&submission.pane, state.retry("no-uptake"));
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
            if pane.restore_until.is_some_and(|until| now / 1000 < until) {
                continue;
            }
            match step(
                &state,
                pane.status,
                pane.ended && pane.bare_prompt,
                &self.auto_close.config,
                now,
            ) {
                Step::Wait => {}
                Step::Change(phase) => self.record_debrief_phase(&pane.pane_id, phase),
                Step::Quit => {
                    let text = self.debrief_text(&pane.pane_id);
                    let result = finished(&text, &state.before);
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
                    if let Phase::Quitting { result, .. } = state.phase {
                        self.close_dormant_pane(&pane.pane_id, state.age_secs, result);
                    }
                }
            }
        }
    }

    pub(super) fn close_dormant_pane(&mut self, pane: &str, age: u64, debrief: CloseDebrief) {
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
        let live: HashSet<_> = self
            .auto_close_snapshot(crate::pane::unix_now_secs())
            .into_iter()
            .map(|pane| pane.pane_id)
            .collect();
        self.auto_close
            .debriefs
            .retain(|pane, _| live.contains(pane));
        for (pane, state) in self.auto_close.debriefs.clone() {
            if let Phase::Sent { at } = state.phase {
                if let Some(restored) = self.auto_close.debriefs.get_mut(&pane) {
                    restored.phase = Phase::Recovering { at };
                }
            }
            self.set_debrief_quiet(&pane, state.active());
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
            before: Vec::new(),
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
            result: finished("", &[]),
        });
        assert_eq!(
            step(&quitting, AgentStatus::Working, false, &config, 60_999),
            Step::Wait
        );
        assert_eq!(
            step(&quitting, AgentStatus::Working, false, &config, 61_000),
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
        let result = finished("debrief v1: 8 memories saved\ndebrief v12: 0 memories saved\nNOT LANDED: topic — debrief\n", &[]);
        assert_eq!(result.skill_version.as_deref(), Some("v12"));
        assert_eq!(result.memories, Some(0));
        for text in [
            "debrief v: 1 memories saved",
            "debrief v2: nope memories saved",
            "quoted debrief v2: 1 memories saved",
            "debrief v2: 1 memories saved extra",
        ] {
            assert_eq!(finished(text, &[]).memories, None);
        }
        let previous = sentinels("debrief v1: 8 memories saved");
        assert_eq!(
            finished(
                "debrief v1: 8 memories saved\nNo sentinel in this reply.",
                &previous
            )
            .memories,
            None
        );
        assert_eq!(
            finished(
                "debrief v1: 8 memories saved\ndebrief v1: 8 memories saved",
                &previous
            )
            .memories,
            Some(8)
        );
    }

    #[test]
    fn debrief_recovery_waits_without_mistaking_idle_for_an_answer() {
        let config = Config::default();
        let recovering = state(Phase::Recovering { at: 1000 });
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
}
