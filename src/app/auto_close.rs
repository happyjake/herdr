//! Server-owned auto-close planning. Report passes never send input or close panes.
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::App;
use crate::api::schema::{AgentStatus, PaneStartedBy};
use crate::persist::logbook::{self, CloseBy, CloseSession, Event};

use crate::config::{AutoCloseConfig as Config, AutoCloseMode as Mode};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum EnabledMode {
    Report,
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Status {
    pub mode: EnabledMode,
    pub waiting: u64,
    pub closed_24h: u64,
}

#[derive(Debug, Clone, Default)]
pub struct SharedStatus(Arc<RwLock<StatusHistory>>);

#[derive(Debug, Clone, Default)]
struct StatusHistory {
    mode: Mode,
    waiting: u64,
    reports: VecDeque<u64>,
    closes: VecDeque<u64>,
}

impl SharedStatus {
    pub(crate) fn from_config(config: &Config) -> Self {
        let shared = Self::default();
        let mut history = shared.0.write().unwrap_or_else(|error| error.into_inner());
        history.mode = config.mode;
        let cutoff = logbook::now_ms().saturating_sub(86_400_000);
        if let Err(error) = logbook::for_each_event(&logbook::path(), |event| match event {
            Event::WouldClose { at, .. } if at >= cutoff => history.reports.push_back(at),
            Event::PaneClosed {
                by: Some(CloseBy::Desk),
                reason: Some(reason),
                age_secs: Some(_),
                at,
                ..
            } if at >= cutoff && matches!(reason.as_str(), "ended" | "dormant" | "plain-shell") => {
                history.closes.push_back(at)
            }
            _ => {}
        }) {
            tracing::warn!(%error, "failed to read auto-close counts");
        }
        drop(history);
        shared
    }

    pub fn current(&self) -> Option<Status> {
        self.current_at(logbook::now_ms())
    }

    fn current_at(&self, now: u64) -> Option<Status> {
        let state = self.0.read().unwrap_or_else(|error| error.into_inner());
        let mode = match state.mode {
            Mode::Off => return None,
            Mode::Report => EnabledMode::Report,
            Mode::Close => EnabledMode::Close,
        };
        Some(Status {
            mode,
            waiting: state.waiting,
            closed_24h: (if state.mode == Mode::Report {
                &state.reports
            } else {
                &state.closes
            })
            .iter()
            .filter(|at| **at <= now && now - **at < 86_400_000)
            .count() as u64,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    Ended,
    Dormant,
    PlainShell,
}

impl Kind {
    fn reason(self) -> &'static str {
        match self {
            Self::Ended => "ended",
            Self::Dormant => "dormant",
            Self::PlainShell => "plain-shell",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Close,
    Debrief,
    Wait,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Candidate {
    pub pane_id: String,
    pub kind: Kind,
    pub reason: String,
    pub age_secs: u64,
    pub action: Action,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main_checkout: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main_checkout_home: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Spared {
    pub pane_id: String,
    pub exemption: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main_checkout: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main_checkout_home: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Plan {
    pub candidates: Vec<Candidate>,
    pub spared: Vec<Spared>,
}

/// Facts are captured once; eligibility itself reads no process, filesystem or clock.
#[derive(Debug, Clone)]
struct PaneSnapshot {
    pane_id: String,
    workspace_id: String,
    agent: Option<String>,
    ended: bool,
    ended_at: Option<u64>,
    seen_agent: bool,
    status: AgentStatus,
    status_at: u64,
    input_at: u64,
    pinned: bool,
    flagged: bool,
    desk_focused: bool,
    restore_until: Option<u64>,
    main_checkout: Option<String>,
    group_refusal: bool,
    bare_prompt: bool,
    starter: Option<String>,
    session: Option<CloseSession>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReportKey {
    reason: String,
    since: u64,
    session: Option<CloseSession>,
}

pub(crate) struct AutoClose {
    pub(crate) config: Config,
    pub(crate) deadline: Option<Instant>,
    pub(crate) focused: HashSet<crate::layout::PaneId>,
    pub(crate) status: SharedStatus,
    reported: HashMap<String, ReportKey>,
    pending: Option<PendingPass>,
    last_pass: Option<Instant>,
}

struct PendingPass {
    receiver: std::sync::mpsc::Receiver<Plan>,
    task: tokio::task::JoinHandle<()>,
    panes: Vec<PaneSnapshot>,
    at: u64,
}

impl Drop for PendingPass {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl AutoClose {
    pub(crate) fn new(config: Config) -> Self {
        Self {
            config,
            deadline: None,
            focused: HashSet::new(),
            status: SharedStatus::default(),
            reported: HashMap::new(),
            pending: None,
            last_pass: None,
        }
    }
}

fn main_checkout_home<'a>(
    pane: &'a PaneSnapshot,
    panes: &'a [PaneSnapshot],
) -> Option<(&'a str, &'a PaneSnapshot)> {
    let checkout = pane.main_checkout.as_deref()?;
    let home = panes
        .iter()
        .chain(std::iter::once(pane))
        .filter(|candidate| candidate.main_checkout.as_deref() == Some(checkout))
        .min_by_key(|candidate| {
            (
                std::cmp::Reverse(candidate.status_at.max(candidate.input_at)),
                crate::workspace::public_workspace_number(&candidate.workspace_id)
                    .unwrap_or(usize::MAX),
                &candidate.workspace_id,
            )
        })?;
    Some((checkout, home))
}

fn main_checkout_fields(
    pane: &PaneSnapshot,
    panes: &[PaneSnapshot],
) -> (Option<String>, Option<String>) {
    main_checkout_home(pane, panes).map_or((None, None), |(checkout, home)| {
        (Some(checkout.to_owned()), Some(home.workspace_id.clone()))
    })
}

fn decide(
    pane: &PaneSnapshot,
    panes: &[PaneSnapshot],
    config: &Config,
    now: u64,
) -> Result<Candidate, &'static str> {
    if pane.pinned {
        return Err("pinned");
    }
    if pane.flagged {
        return Err("flagged");
    }
    if pane.desk_focused {
        return Err("desk-focused");
    }
    if pane.agent.is_some() {
        match pane.status {
            AgentStatus::Working => return Err("working"),
            AgentStatus::Blocked => return Err("blocked"),
            AgentStatus::Unknown => return Err("unknown"),
            AgentStatus::Done | AgentStatus::Idle => {}
        }
    }
    if pane.restore_until.is_some_and(|until| now < until) {
        return Err("restore-window");
    }
    if main_checkout_home(pane, panes)
        .is_some_and(|(_, home)| home.workspace_id == pane.workspace_id)
    {
        return Err("last-main-checkout-pane");
    }
    if pane.group_refusal {
        return Err("worktree-group");
    }
    if panes
        .iter()
        .any(|kin| kin.starter.as_deref() == Some(&pane.pane_id))
    {
        return Err("open-kin");
    }
    let (kind, since, threshold) = if pane.agent.is_some() {
        (
            Kind::Dormant,
            pane.status_at.max(pane.input_at),
            config.dormant_age_secs,
        )
    } else {
        if pane.seen_agent && !pane.ended {
            return Err("agent-history");
        }
        if !pane.bare_prompt {
            return Err("not-bare-prompt");
        }
        if pane.ended {
            (
                Kind::Ended,
                pane.ended_at.ok_or("ended-time-unknown")?,
                config.ended_age_secs,
            )
        } else {
            (Kind::PlainShell, pane.input_at, config.plain_shell_age_secs)
        }
    };
    let age_secs = now.saturating_sub(since);
    if now < since || age_secs < threshold {
        return Err("too-young");
    }
    let (main_checkout, main_checkout_home) = main_checkout_fields(pane, panes);
    Ok(Candidate {
        pane_id: pane.pane_id.clone(),
        kind,
        reason: kind.reason().into(),
        age_secs,
        action: Action::Close,
        wait_reason: None,
        main_checkout,
        main_checkout_home,
    })
}

fn plan(panes: &[PaneSnapshot], config: &Config, now: u64) -> Plan {
    let mut result = Plan::default();
    for pane in panes {
        match decide(pane, panes, config, now) {
            Ok(candidate) => result.candidates.push(candidate),
            Err(exemption) => {
                let (main_checkout, main_checkout_home) = main_checkout_fields(pane, panes);
                result.spared.push(Spared {
                    pane_id: pane.pane_id.clone(),
                    exemption: exemption.into(),
                    main_checkout,
                    main_checkout_home,
                });
            }
        }
    }
    result
}

fn debrief_waits(mut plan: Plan, panes: &[PaneSnapshot], config: &Config) -> Plan {
    let path = if let Some(relative) = config.debrief_skill_path.strip_prefix("~/") {
        std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(relative))
    } else {
        Some(std::path::PathBuf::from(&config.debrief_skill_path))
    };
    let skill_present = path.is_some_and(|path| path.is_file());
    for candidate in &mut plan.candidates {
        if candidate.kind != Kind::Dormant || !config.debrief {
            continue;
        }
        let agent = panes
            .iter()
            .find(|pane| pane.pane_id == candidate.pane_id)
            .and_then(|pane| pane.agent.as_deref());
        if agent == Some("agy") {
            continue;
        }
        candidate.action = Action::Wait;
        candidate.wait_reason = Some(
            if !matches!(agent, Some("claude" | "codex" | "pi")) {
                "unsupported-agent"
            } else if !skill_present {
                "skill-missing"
            } else {
                "gate-pending"
            }
            .into(),
        );
    }
    plan
}

async fn gate(command: &str, timeout_secs: u64) -> Result<(), &'static str> {
    if command.trim().is_empty() {
        return Err("gate-closed");
    }
    let mut process = crate::platform::detached_custom_command_process(command);
    process
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    crate::platform::configure_status_command(&mut process);
    let mut process = tokio::process::Command::from(process);
    process.kill_on_drop(true);
    let mut child = process.spawn().map_err(|_| "gate-closed")?;
    let _guard = crate::platform::StatusCommandGuard::new(&child).map_err(|_| "gate-closed")?;
    match tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait()).await {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(_) => Err("gate-closed"),
        Err(_) => Err("gate-timeout"),
    }
}

async fn evaluate(panes: &[PaneSnapshot], config: &Config, now: u64) -> Plan {
    let mut result = debrief_waits(plan(panes, config, now), panes, config);
    let mut gates = HashMap::new();
    for candidate in &mut result.candidates {
        if candidate.wait_reason.as_deref() != Some("gate-pending") {
            continue;
        }
        let Some(kind) = panes
            .iter()
            .find(|pane| pane.pane_id == candidate.pane_id)
            .and_then(|pane| pane.agent.as_deref())
        else {
            continue;
        };
        let settings = match kind {
            "claude" => &config.claude,
            "codex" => &config.codex,
            "pi" => &config.pi,
            _ => continue,
        };
        let outcome = match gates.get(kind) {
            Some(outcome) => *outcome,
            None => {
                let outcome = gate(&settings.gate_command, config.gate_timeout_secs).await;
                gates.insert(kind, outcome);
                outcome
            }
        };
        match outcome {
            Ok(()) => {
                candidate.action = Action::Debrief;
                candidate.wait_reason = None;
            }
            Err(reason) => candidate.wait_reason = Some(reason.into()),
        }
    }
    result
}

fn next_pass_delay(panes: &[PaneSnapshot], config: &Config, now: u64) -> u64 {
    if panes
        .iter()
        .any(|pane| decide(pane, panes, config, now) != Err("too-young"))
    {
        return config.pass_floor_secs.max(1);
    }
    let next_age = panes
        .iter()
        .filter_map(|pane| {
            if decide(pane, panes, config, now) != Err("too-young") {
                return None;
            }
            let due = if pane.agent.is_some() {
                pane.status_at
                    .max(pane.input_at)
                    .saturating_add(config.dormant_age_secs)
            } else if pane.ended {
                pane.ended_at?.saturating_add(config.ended_age_secs)
            } else {
                pane.input_at.saturating_add(config.plain_shell_age_secs)
            };
            Some(due.saturating_sub(now))
        })
        .min()
        .unwrap_or(0);
    config.pass_floor_secs.max(1).max(next_age)
}

fn live_starter(link: &PaneStartedBy) -> Option<String> {
    match link {
        PaneStartedBy::Pane { pane_id } => Some(pane_id.clone()),
        PaneStartedBy::Closed { closed } => closed.started_by.as_ref().and_then(live_starter),
    }
}

impl App {
    pub(crate) fn attach_auto_close_status(&mut self, shared: SharedStatus) {
        *shared.0.write().unwrap_or_else(|error| error.into_inner()) = self
            .auto_close
            .status
            .0
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        self.auto_close.status = shared;
    }

    pub(crate) fn initialize_auto_close_history(&mut self) {
        if !self.policy.persist_session {
            return;
        }
        let mut reported = HashMap::new();
        let mut history = self
            .auto_close
            .status
            .0
            .write()
            .unwrap_or_else(|error| error.into_inner());
        history.reports.clear();
        history.closes.clear();
        let cutoff = logbook::now_ms().saturating_sub(86_400_000);
        if let Err(error) = logbook::for_each_event(&logbook::path(), |event| match event {
            Event::WouldClose {
                pane,
                reason,
                age_secs,
                session,
                at,
                ..
            } => {
                reported.insert(
                    pane,
                    ReportKey {
                        reason,
                        since: (at / 1000).saturating_sub(age_secs),
                        session,
                    },
                );
                if at >= cutoff {
                    history.reports.push_back(at);
                }
            }
            Event::PaneClosed {
                pane,
                by: Some(CloseBy::Desk),
                reason: Some(reason),
                age_secs: Some(_),
                at,
                ..
            } => {
                reported.remove(&pane);
                if at >= cutoff && matches!(reason.as_str(), "ended" | "dormant" | "plain-shell") {
                    history.closes.push_back(at);
                }
            }
            Event::PaneClosed { pane, .. } => {
                reported.remove(&pane);
            }
            _ => {}
        }) {
            tracing::warn!(%error, "failed to read auto-close history");
        }
        self.auto_close.reported = reported;
    }

    pub(crate) fn wake_auto_close_for_changed_facts(&mut self, now: Instant) {
        if self.auto_close.config.mode == Mode::Off || self.auto_close.pending.is_some() {
            return;
        }
        let Some(floor) =
            self.auto_close
                .last_pass
                .unwrap_or(now)
                .checked_add(Duration::from_secs(
                    self.auto_close.config.pass_floor_secs.max(1),
                ))
        else {
            return;
        };
        self.auto_close.deadline = Some(
            self.auto_close
                .deadline
                .map_or(floor, |deadline| deadline.min(floor)),
        );
    }
    pub(crate) fn configure_auto_close(&mut self, config: Config) {
        if self.auto_close.config == config && self.auto_close.deadline.is_some() {
            return;
        }
        self.auto_close.pending = None;
        self.auto_close.config = config;
        self.auto_close.deadline = (self.auto_close.config.mode != Mode::Off)
            .then(|| {
                Instant::now().checked_add(Duration::from_secs(
                    self.auto_close.config.pass_floor_secs.max(1),
                ))
            })
            .flatten();
        let mut status = self
            .auto_close
            .status
            .0
            .write()
            .unwrap_or_else(|error| error.into_inner());
        status.mode = self.auto_close.config.mode;
        status.waiting = 0;
    }

    fn auto_close_snapshot(&self, now: u64) -> Vec<PaneSnapshot> {
        let mut panes = Vec::new();
        for (ws_idx, ws) in self.state.workspaces.iter().enumerate() {
            let last = ws.tabs.iter().map(|tab| tab.panes.len()).sum::<usize>() == 1;
            let main_checkout = last
                .then(|| {
                    ws.worktree_space()
                        .filter(|space| !space.is_linked_worktree)
                        .map(|space| {
                            crate::worktree::canonical_or_original(&space.checkout_path)
                                .display()
                                .to_string()
                        })
                        .or_else(|| {
                            crate::workspace::git_space_metadata(&ws.identity_cwd)
                                .filter(|info| !info.is_linked_worktree)
                                .map(|info| info.checkout_key)
                        })
                })
                .flatten();
            for tab in &ws.tabs {
                for (&id, pane) in &tab.panes {
                    let Some(terminal) = self.state.terminals.get(&pane.attached_terminal_id)
                    else {
                        continue;
                    };
                    let Some(pane_id) = self.public_pane_id(ws_idx, id) else {
                        continue;
                    };
                    let runtime = self.state.runtime_for_pane_in_workspace(
                        &self.terminal_runtimes,
                        ws_idx,
                        id,
                    );
                    let agent = terminal.effective_agent_label().map(str::to_owned);
                    let (status, status_at) = pane.reported_agent_status_pair_at(
                        super::api_helpers::pane_agent_status(terminal.state, pane.seen),
                        now,
                    );
                    let bare_prompt = agent.is_none()
                        && runtime
                            .and_then(|runtime| runtime.child_pid())
                            .is_some_and(|pid| {
                                crate::detect::pane_shell_is_idle(pid)
                                    && crate::platform::session_processes(pid) == vec![pid]
                            });
                    panes.push(PaneSnapshot {
                        pane_id,
                        workspace_id: ws.id.clone(),
                        agent,
                        ended: terminal.ended_agent.is_some(),
                        ended_at: terminal.ended_at,
                        seen_agent: terminal.session_reported
                            || terminal.persisted_agent_session.is_some()
                            || terminal.agent_name.is_some(),
                        status,
                        status_at,
                        input_at: runtime.map_or(terminal.input_or_command_at, |runtime| {
                            runtime.input_or_command_at()
                        }),
                        pinned: terminal.pinned,
                        flagged: terminal.flag.is_some(),
                        desk_focused: self.auto_close.focused.contains(&id),
                        restore_until: pane.agent_status_resolution_due_at(),
                        main_checkout: main_checkout.clone(),
                        group_refusal: last
                            && self
                                .state
                                .workspace_close_would_close_worktree_group(ws_idx),
                        bare_prompt,
                        starter: terminal.started_by.as_ref().and_then(live_starter),
                        session: self.state.logbook_key(id).map(|key| CloseSession {
                            agent: key.agent,
                            id: key.value,
                        }),
                    });
                }
            }
        }
        panes.sort_by(|a, b| a.pane_id.cmp(&b.pane_id));
        panes
    }

    pub(crate) fn request_auto_close_plan(
        &self,
        id: String,
        respond_to: std::sync::mpsc::Sender<String>,
    ) {
        let now = crate::pane::unix_now_secs();
        let panes = self.auto_close_snapshot(now);
        let config = self.auto_close.config.clone();
        tokio::spawn(async move {
            let plan = evaluate(&panes, &config, now).await;
            let response = super::api::responses::encode_success(
                id,
                crate::api::schema::ResponseResult::AutoClosePlan { plan },
            );
            let _ = respond_to.send(response);
        });
    }

    pub(crate) fn run_auto_close_pass(&mut self, monotonic: Instant) {
        if self
            .auto_close
            .deadline
            .is_none_or(|deadline| monotonic < deadline)
        {
            return;
        }
        if let Some(pending) = self.auto_close.pending.as_ref() {
            match pending.receiver.try_recv() {
                Ok(result) => {
                    if let Some(pending) = self.auto_close.pending.take() {
                        self.finish_auto_close_pass(result, &pending.panes, pending.at, monotonic);
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    self.auto_close.deadline = monotonic.checked_add(Duration::from_millis(25));
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.auto_close.pending = None;
                    self.auto_close.deadline = monotonic.checked_add(Duration::from_secs(
                        self.auto_close.config.pass_floor_secs.max(1),
                    ));
                    tracing::warn!("auto-close planning task stopped before producing a plan");
                }
            }
            return;
        }
        let now = crate::pane::unix_now_secs();
        let panes = self.auto_close_snapshot(now);
        let config = self.auto_close.config.clone();
        let input = panes.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        let task = tokio::spawn(async move {
            let _ = sender.send(evaluate(&input, &config, now).await);
        });
        self.auto_close.pending = Some(PendingPass {
            receiver,
            task,
            panes,
            at: now,
        });
        self.auto_close.deadline = monotonic.checked_add(Duration::from_millis(25));
    }

    fn finish_auto_close_pass(
        &mut self,
        mut result: Plan,
        previous: &[PaneSnapshot],
        at: u64,
        monotonic: Instant,
    ) {
        let now = crate::pane::unix_now_secs();
        // Gate execution may take seconds. Re-check protections and activity
        // before reporting a result about the still-live pane.
        let panes = self.auto_close_snapshot(now);
        let current = plan(&panes, &self.auto_close.config, now);
        result.candidates.retain(|candidate| {
            current.candidates.iter().any(|fresh| {
                fresh.pane_id == candidate.pane_id
                    && fresh.kind == candidate.kind
                    && now.saturating_sub(fresh.age_secs) == at.saturating_sub(candidate.age_secs)
                    && panes
                        .iter()
                        .find(|pane| pane.pane_id == candidate.pane_id)
                        .map(|pane| &pane.session)
                        == previous
                            .iter()
                            .find(|pane| pane.pane_id == candidate.pane_id)
                            .map(|pane| &pane.session)
            })
        });
        self.auto_close
            .status
            .0
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .waiting = result
            .candidates
            .iter()
            .filter(|candidate| candidate.action == Action::Wait)
            .count() as u64;
        if self.auto_close.config.mode == Mode::Report {
            for candidate in &result.candidates {
                let Some(pane) = panes.iter().find(|pane| pane.pane_id == candidate.pane_id) else {
                    continue;
                };
                let key = ReportKey {
                    reason: candidate.reason.clone(),
                    since: at.saturating_sub(candidate.age_secs),
                    session: pane.session.clone(),
                };
                if self.auto_close.reported.get(&pane.pane_id) == Some(&key) {
                    continue;
                }
                let at = logbook::now_ms();
                self.state.enqueue_logbook_events([Event::WouldClose {
                    pane: pane.pane_id.clone(),
                    by: CloseBy::Desk,
                    reason: key.reason.clone(),
                    age_secs: (at / 1000).saturating_sub(key.since),
                    debrief: (candidate.action == Action::Close).then_some(logbook::CloseDebrief {
                        outcome: crate::api::schema::DebriefOutcome::Skipped,
                        memories: None,
                        skill_version: None,
                    }),
                    session: key.session.clone(),
                    at,
                }]);
                self.auto_close.reported.insert(pane.pane_id.clone(), key);
                self.auto_close
                    .status
                    .0
                    .write()
                    .unwrap_or_else(|error| error.into_inner())
                    .reports
                    .push_back(at);
            }
            self.flush_logbook();
        }
        {
            let mut history = self
                .auto_close
                .status
                .0
                .write()
                .unwrap_or_else(|error| error.into_inner());
            let cutoff = logbook::now_ms().saturating_sub(86_400_000);
            history.reports.retain(|at| *at >= cutoff);
            history.closes.retain(|at| *at >= cutoff);
        }
        self.auto_close.last_pass = Some(monotonic);
        self.auto_close
            .reported
            .retain(|id, _| panes.iter().any(|pane| &pane.pane_id == id));
        self.auto_close.deadline = monotonic.checked_add(Duration::from_secs(next_pass_delay(
            &panes,
            &self.auto_close.config,
            now,
        )));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell() -> PaneSnapshot {
        PaneSnapshot {
            pane_id: "pane-1".into(),
            workspace_id: "w1".into(),
            agent: None,
            ended: false,
            ended_at: None,
            seen_agent: false,
            status: AgentStatus::Unknown,
            status_at: 100,
            input_at: 100,
            pinned: false,
            flagged: false,
            desk_focused: false,
            restore_until: None,
            main_checkout: None,
            group_refusal: false,
            bare_prompt: true,
            starter: None,
            session: None,
        }
    }

    fn main_checkout_shell(workspace_id: &str, status_at: u64, input_at: u64) -> PaneSnapshot {
        let mut pane = shell();
        pane.pane_id = format!("{workspace_id}:p1");
        pane.workspace_id = workspace_id.into();
        pane.status_at = status_at;
        pane.input_at = input_at;
        pane.main_checkout = Some("checkout".into());
        pane
    }

    #[test]
    fn auto_close_boundary_ages_and_dormant_activity_are_exact() {
        let config = Config {
            ended_age_secs: 10,
            dormant_age_secs: 20,
            plain_shell_age_secs: 30,
            ..Config::default()
        };
        let mut pane = shell();
        for (kind, due) in [
            (Kind::PlainShell, 130),
            (Kind::Ended, 110),
            (Kind::Dormant, 125),
        ] {
            pane.ended = kind == Kind::Ended;
            pane.ended_at = pane.ended.then_some(100);
            pane.agent = (kind == Kind::Dormant).then(|| "pi".into());
            pane.status = AgentStatus::Done;
            pane.input_at = if kind == Kind::Dormant { 105 } else { 100 };
            assert_eq!(decide(&pane, &[], &config, due - 1), Err("too-young"));
            let candidate = decide(&pane, &[], &config, due).unwrap();
            assert_eq!(candidate.kind, kind);
            assert_eq!(
                candidate.age_secs,
                match kind {
                    Kind::PlainShell => 30,
                    Kind::Ended => 10,
                    Kind::Dormant => 20,
                }
            );
        }
        pane.input_at = 130;
        assert_eq!(decide(&pane, &[], &config, 140), Err("too-young"));
        pane.status_at = 140;
        assert_eq!(decide(&pane, &[], &config, 159), Err("too-young"));
        assert!(decide(&pane, &[], &config, 160).is_ok());
        assert_eq!(decide(&pane, &[], &config, 99), Err("too-young"));
    }

    #[test]
    fn auto_close_ended_age_requires_an_independent_exit_timestamp() {
        let mut pane = shell();
        pane.ended = true;
        pane.status_at = 1;
        let config = Config {
            ended_age_secs: 10,
            pass_floor_secs: 1,
            ..Config::default()
        };
        assert_eq!(decide(&pane, &[], &config, 130), Err("ended-time-unknown"));
        pane.ended_at = Some(120);
        assert_eq!(decide(&pane, &[], &config, 129), Err("too-young"));
        assert_eq!(next_pass_delay(&[pane.clone()], &config, 125), 5);
        let candidate = decide(&pane, &[], &config, 130).unwrap();
        assert_eq!(candidate.age_secs, 10);
        pane.status_at = 1000;
        assert_eq!(decide(&pane, &[], &config, 130).unwrap(), candidate);
    }

    #[test]
    fn auto_close_exemptions_have_a_stable_precedence() {
        let config = Config {
            dormant_age_secs: 0,
            plain_shell_age_secs: 0,
            ..Config::default()
        };
        let mut pane = shell();
        pane.agent = Some("pi".into());
        pane.status = AgentStatus::Working;
        pane.pinned = true;
        pane.flagged = true;
        pane.desk_focused = true;
        pane.restore_until = Some(200);
        pane.main_checkout = Some("checkout".into());
        pane.group_refusal = true;
        assert_eq!(decide(&pane, &[], &config, 150), Err("pinned"));
        pane.pinned = false;
        assert_eq!(decide(&pane, &[], &config, 150), Err("flagged"));
        pane.flagged = false;
        assert_eq!(decide(&pane, &[], &config, 150), Err("desk-focused"));
        pane.desk_focused = false;
        for (status, exemption) in [
            (AgentStatus::Working, "working"),
            (AgentStatus::Blocked, "blocked"),
            (AgentStatus::Unknown, "unknown"),
        ] {
            pane.status = status;
            assert_eq!(decide(&pane, &[], &config, 150), Err(exemption));
        }
        pane.status = AgentStatus::Idle;
        assert_eq!(decide(&pane, &[], &config, 199), Err("restore-window"));
        assert_eq!(
            decide(&pane, &[], &config, 200),
            Err("last-main-checkout-pane")
        );
        pane.main_checkout = None;
        assert_eq!(decide(&pane, &[], &config, 200), Err("worktree-group"));
        pane.group_refusal = false;
        let mut kin = shell();
        kin.pane_id = "pane-2".into();
        kin.starter = Some(pane.pane_id.clone());
        assert_eq!(decide(&pane, &[kin], &config, 200), Err("open-kin"));
        assert!(decide(&pane, &[], &config, 200).is_ok());
        pane.agent = None;
        pane.bare_prompt = false;
        assert_eq!(decide(&pane, &[], &config, 200), Err("not-bare-prompt"));
    }
    #[test]
    fn auto_close_main_checkout_keeps_only_the_most_recent_home() {
        let config = Config {
            plain_shell_age_secs: 0,
            ..Config::default()
        };
        let only = main_checkout_shell("w1", 100, 100);
        assert_eq!(
            decide(&only, &[], &config, 200),
            Err("last-main-checkout-pane")
        );

        let panes = [
            main_checkout_shell("w1", 100, 140),
            main_checkout_shell("w2", 150, 100),
            main_checkout_shell("w3", 120, 130),
        ];
        let result = plan(&panes, &config, 200);
        assert_eq!(result.candidates.len(), 2, "{result:?}");
        assert_eq!(result.spared.len(), 1, "{result:?}");
        assert_eq!(result.spared[0].pane_id, "w2:p1");
        assert_eq!(result.spared[0].exemption, "last-main-checkout-pane");
        assert_eq!(result.spared[0].main_checkout.as_deref(), Some("checkout"));
        assert_eq!(result.spared[0].main_checkout_home.as_deref(), Some("w2"));
        for candidate in &result.candidates {
            assert!(matches!(candidate.pane_id.as_str(), "w1:p1" | "w3:p1"));
            assert_eq!(candidate.main_checkout.as_deref(), Some("checkout"));
            assert_eq!(candidate.main_checkout_home.as_deref(), Some("w2"));
        }
    }

    #[test]
    fn auto_close_main_checkout_ties_keep_the_oldest_workspace_id() {
        let config = Config {
            plain_shell_age_secs: 0,
            ..Config::default()
        };
        let panes = [
            main_checkout_shell("w10", 100, 100),
            main_checkout_shell("w2", 100, 100),
        ];
        let result = plan(&panes, &config, 200);
        assert_eq!(result.spared.len(), 1, "{result:?}");
        assert_eq!(result.spared[0].pane_id, "w2:p1");
        assert_eq!(result.candidates.len(), 1, "{result:?}");
        assert_eq!(result.candidates[0].pane_id, "w10:p1");
    }

    #[test]
    fn auto_close_linked_worktree_is_not_part_of_the_main_checkout_home() {
        let config = Config {
            plain_shell_age_secs: 0,
            ..Config::default()
        };
        let main = main_checkout_shell("w1", 100, 100);
        let mut linked = shell();
        linked.pane_id = "w2:p1".into();
        linked.workspace_id = "w2".into();
        let result = plan(&[main, linked], &config, 200);
        assert_eq!(result.spared.len(), 1, "{result:?}");
        assert_eq!(result.spared[0].pane_id, "w1:p1");
        assert_eq!(result.candidates.len(), 1, "{result:?}");
        assert_eq!(result.candidates[0].pane_id, "w2:p1");
        assert_eq!(result.candidates[0].main_checkout, None);
        assert_eq!(result.candidates[0].main_checkout_home, None);
    }

    #[test]
    fn auto_close_deadline_waits_for_the_next_age_but_never_beats_the_floor() {
        let mut config = Config {
            plain_shell_age_secs: 100,
            pass_floor_secs: 10,
            ..Config::default()
        };
        let mut older = shell();
        let mut younger = shell();
        older.input_at = 10;
        younger.input_at = 80;
        younger.pane_id = "pane-2".into();
        assert_eq!(
            next_pass_delay(&[older.clone(), younger.clone()], &config, 100),
            10
        );
        assert_eq!(next_pass_delay(&[younger.clone()], &config, 100), 80);
        config.pass_floor_secs = 90;
        assert_eq!(next_pass_delay(&[younger], &config, 100), 90);
        assert_eq!(next_pass_delay(&[older], &config, 200), 90);
    }

    #[test]
    fn auto_close_does_not_reclassify_old_agent_history_as_a_plain_shell() {
        let mut pane = shell();
        pane.seen_agent = true;
        let config = Config {
            plain_shell_age_secs: 0,
            ..Config::default()
        };
        assert_eq!(decide(&pane, &[], &config, 200), Err("agent-history"));
    }

    #[test]
    fn auto_close_skips_debrief_only_when_disabled_or_for_the_unsupported_ui() {
        let mut pane = shell();
        pane.agent = Some("agy".into());
        pane.status = AgentStatus::Idle;
        let mut config = Config {
            dormant_age_secs: 0,
            debrief_skill_path: String::new(),
            ..Config::default()
        };
        assert_eq!(
            debrief_waits(
                plan(&[pane.clone()], &config, 200),
                &[pane.clone()],
                &config
            )
            .candidates[0]
                .action,
            Action::Close
        );
        pane.agent = Some("pi".into());
        assert_eq!(
            debrief_waits(
                plan(&[pane.clone()], &config, 200),
                &[pane.clone()],
                &config
            )
            .candidates[0]
                .wait_reason
                .as_deref(),
            Some("skill-missing")
        );
        config.debrief = false;
        assert_eq!(
            debrief_waits(plan(&[pane.clone()], &config, 200), &[pane], &config).candidates[0]
                .action,
            Action::Close
        );
    }
}

#[cfg(test)]
mod contract_tests {
    use super::*;

    #[test]
    fn auto_close_config_defaults_and_partial_kind_overrides_use_integer_seconds() {
        let config: crate::config::Config = toml::from_str(
            "[auto_close]\nmode = 'report'\n[auto_close.codex]\ngate_command = 'exit 0'\n",
        )
        .unwrap();
        let config = config.auto_close;
        assert_eq!(config.ended_age_secs, 86_400);
        assert_eq!(config.dormant_age_secs, 259_200);
        assert_eq!(config.plain_shell_age_secs, 604_800);
        assert_eq!(config.pass_floor_secs, 600);
        assert_eq!(config.claude.limit_secs, 600);
        assert_eq!(config.pi.limit_secs, 600);
        assert_eq!(config.codex.limit_secs, 900);
        assert_eq!(config.codex.invocation, "$debrief");
        assert_eq!(config.claude.invocation, "/debrief");
        assert_eq!(config.pi.invocation, "/skill:debrief");
        assert_eq!(config.uptake_window_secs, 60);
        assert_eq!(config.settle_hold_secs, 60);
        assert_eq!(config.quit_grace_secs, 60);
        for value in ["-1", "1.5", "'3'"] {
            assert!(toml::from_str::<crate::config::Config>(&format!(
                "[auto_close]\nended_age_secs = {value}\n"
            ))
            .is_err());
        }
        assert!(
            toml::from_str::<crate::config::Config>("[auto_close]\nmode = 'invalid'\n").is_err()
        );
    }

    #[test]
    fn auto_close_hello_counts_only_the_current_mode_and_last_day() {
        let status = SharedStatus::default();
        {
            let mut history = status.0.write().unwrap();
            history.reports = [0, 1, 86_400_000, 86_400_001].into();
            history.closes = [50].into();
        }
        assert!(status.current_at(86_400_000).is_none());
        status.0.write().unwrap().mode = Mode::Report;
        assert_eq!(status.current_at(86_400_000).unwrap().closed_24h, 2);
        status.0.write().unwrap().mode = Mode::Close;
        assert_eq!(status.current_at(86_400_000).unwrap().closed_24h, 1);
        assert_eq!(status.current_at(172_800_000).unwrap().closed_24h, 0);
    }

    #[test]
    fn auto_close_would_close_event_roundtrips_the_hosting_session() {
        let line = serde_json::json!({"type":"would_close","pane":"pane-1","by":"desk","reason":"dormant","age_secs":100,"session":{"agent":"pi","id":"session-1"},"at":200_000});
        let event: Event = serde_json::from_value(line.clone()).unwrap();
        assert_eq!(serde_json::to_value(event).unwrap(), line);
    }
}
