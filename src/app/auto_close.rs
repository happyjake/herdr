//! Server-owned auto-close planning. Report passes never send input or close panes.
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::App;
use crate::api::schema::{AgentStatus, PaneStartedBy};
use crate::persist::logbook::{self, CloseBy, CloseSession, Event};

use crate::config::{
    AutoCloseConfig as Config, AutoCloseKindConfig as KindConfig, AutoCloseMode as Mode,
};

pub(crate) mod debrief;
use debrief::{Debrief, Phase};

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
    input_seq: u64,
    pinned: bool,
    flagged: bool,
    desk_focused: bool,
    restore_until: Option<u64>,
    single_pane_workspace: bool,
    main_checkout: Option<String>,
    group_refusal: bool,
    group_members: Vec<String>,
    bare_prompt: bool,
    starter: Option<String>,
    session: Option<CloseSession>,
    debrief: Option<Debrief>,
}

impl PaneSnapshot {
    fn same_close_facts(&self, other: &Self) -> bool {
        self.input_at == other.input_at
            && self.input_seq == other.input_seq
            && self.status == other.status
            && self.status_at == other.status_at
            && self.agent == other.agent
            && self.ended == other.ended
            && self.ended_at == other.ended_at
            && self.pinned == other.pinned
            && self.flagged == other.flagged
            && self.desk_focused == other.desk_focused
            && self.main_checkout == other.main_checkout
            && self.session == other.session
    }

    fn refusal_key(&self, reason: &str, now: u64, age_secs: u64) -> RefusalKey {
        RefusalKey {
            reason: reason.to_owned(),
            since: now.saturating_sub(age_secs),
            input_seq: self.input_seq,
            session: self.session.clone(),
            group_members: self.group_members.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReportKey {
    reason: String,
    since: u64,
    session: Option<CloseSession>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RefusalKey {
    reason: String,
    since: u64,
    input_seq: u64,
    session: Option<CloseSession>,
    group_members: Vec<String>,
}

pub(crate) struct AutoClose {
    pub(crate) config: Config,
    pub(crate) deadline: Option<Instant>,
    pub(crate) focused: HashSet<crate::layout::PaneId>,
    pub(crate) status: SharedStatus,
    reported: HashMap<String, ReportKey>,
    refused: HashMap<String, RefusalKey>,
    pending: Option<PendingPass>,
    last_pass: Option<Instant>,
    pass_deadline: Option<Instant>,
    debriefs: HashMap<String, Debrief>,
    delivery: Option<debrief::PendingDelivery>,
    submission: Option<debrief::PendingSubmission>,
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
            refused: HashMap::new(),
            pending: None,
            last_pass: None,
            pass_deadline: None,
            debriefs: HashMap::new(),
            delivery: None,
            submission: None,
        }
    }
}

fn main_checkout_home<'a>(
    pane: &'a PaneSnapshot,
    panes: &'a [PaneSnapshot],
) -> Option<&'a PaneSnapshot> {
    let checkout = pane.main_checkout.as_deref()?;
    panes
        .iter()
        .chain(std::iter::once(pane))
        .filter(|candidate| {
            candidate.single_pane_workspace && candidate.main_checkout.as_deref() == Some(checkout)
        })
        .min_by_key(|candidate| {
            (
                std::cmp::Reverse(candidate.status_at.max(candidate.input_at)),
                crate::workspace::public_workspace_number(&candidate.workspace_id)
                    .unwrap_or(usize::MAX),
                &candidate.workspace_id,
            )
        })
}

fn main_checkout_fields(
    pane: &PaneSnapshot,
    panes: &[PaneSnapshot],
) -> (Option<String>, Option<String>) {
    let home = main_checkout_home(pane, panes);
    (
        pane.main_checkout.clone(),
        home.map(|home| home.workspace_id.clone()),
    )
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
    if pane.agent.is_some() && pane.debrief.is_none() {
        match pane.status {
            AgentStatus::Working => return Err("working"),
            AgentStatus::Blocked => return Err("blocked"),
            AgentStatus::Unknown => return Err("unknown"),
            AgentStatus::Done | AgentStatus::Idle => {}
        }
    }
    if pane.restore_until.is_some_and(|until| now < until)
        && pane
            .debrief
            .as_ref()
            .is_none_or(|state| matches!(state.phase, Phase::Retry { .. }))
    {
        return Err("restore-window");
    }
    if pane.debrief.is_some() && pane.status == AgentStatus::Blocked {
        return Err("blocked");
    }
    if pane.single_pane_workspace
        && main_checkout_home(pane, panes)
            .is_some_and(|home| home.workspace_id == pane.workspace_id)
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
    if let Some(state) = &pane.debrief {
        if debrief::protected(pane, state) {
            return Err("debrief-pane-changed");
        }
        let (action, reason) = match &state.phase {
            _ if debrief::step(
                state,
                pane.status,
                pane.ended && pane.bare_prompt,
                config,
                now.saturating_mul(1000),
            ) == debrief::Step::Close =>
            {
                (Action::Close, None)
            }
            Phase::Waiting { .. } | Phase::Retry { .. }
                if !config.debrief
                    && matches!(pane.status, AgentStatus::Idle | AgentStatus::Done) =>
            {
                (Action::Close, None)
            }
            Phase::Aborted { .. } => return Err("debrief-aborted"),
            Phase::Waiting { reason } => (Action::Wait, Some(reason.clone())),
            Phase::Retry { .. }
                if !matches!(pane.status, AgentStatus::Idle | AgentStatus::Done) =>
            {
                (Action::Wait, Some("retry-working".into()))
            }
            Phase::Retry { .. } => (Action::Close, None),
            _ => (Action::Wait, Some("debrief-in-progress".into())),
        };
        if action == Action::Close && pane.agent.is_some() && pane.status == AgentStatus::Unknown {
            return Err("unknown");
        }
        let (main_checkout, main_checkout_home) = main_checkout_fields(pane, panes);
        return Ok(Candidate {
            pane_id: pane.pane_id.clone(),
            kind: Kind::Dormant,
            reason: "dormant".into(),
            age_secs: state.age_secs,
            action,
            wait_reason: reason,
            main_checkout,
            main_checkout_home,
        });
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
                pane.ended_at
                    .ok_or("ended-time-unknown")?
                    .max(pane.input_at),
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
        if candidate.action == Action::Wait {
            continue;
        }
        if panes
            .iter()
            .find(|pane| pane.pane_id == candidate.pane_id)
            .and_then(|pane| pane.debrief.as_ref())
            .is_some_and(|state| !matches!(state.phase, Phase::Retry { .. }))
        {
            continue;
        }
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
                pane.ended_at?
                    .max(pane.input_at)
                    .saturating_add(config.ended_age_secs)
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

fn record_group_refusal(
    refused: &mut HashMap<String, RefusalKey>,
    still_refused: &mut HashSet<String>,
    pane: &PaneSnapshot,
    panes: &[PaneSnapshot],
    config: &Config,
    now: u64,
) -> Option<Candidate> {
    if !pane.group_refusal {
        return None;
    }
    let mut probe = pane.clone();
    probe.main_checkout = None;
    probe.group_refusal = false;
    let candidate = decide(&probe, panes, config, now).ok()?;
    if !matches!(candidate.kind, Kind::Ended | Kind::PlainShell) {
        return None;
    }
    let key = pane.refusal_key(&candidate.reason, now, candidate.age_secs);
    still_refused.insert(pane.pane_id.clone());
    if refused.get(&pane.pane_id) == Some(&key) {
        return None;
    }
    refused.insert(pane.pane_id.clone(), key);
    Some(candidate)
}

fn candidate_ready_to_close(
    candidate: &Candidate,
    previous: &[PaneSnapshot],
    fresh_panes: &[PaneSnapshot],
    refused: &HashMap<String, RefusalKey>,
    still_refused: &mut HashSet<String>,
    config: &Config,
    now: u64,
) -> bool {
    let Some((prev, fresh)) = previous
        .iter()
        .find(|prev| prev.pane_id == candidate.pane_id)
        .zip(
            fresh_panes
                .iter()
                .find(|pane| pane.pane_id == candidate.pane_id),
        )
    else {
        return false;
    };
    let key = fresh.refusal_key(&candidate.reason, now, candidate.age_secs);
    if refused.get(&fresh.pane_id) == Some(&key) {
        still_refused.insert(fresh.pane_id.clone());
        return false;
    }
    fresh.same_close_facts(prev)
        && decide(fresh, fresh_panes, config, now).is_ok_and(|fresh_candidate| {
            fresh_candidate.kind == candidate.kind && fresh_candidate.action == Action::Close
        })
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
        let mut debriefs = HashMap::new();
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
            Event::DebriefSent { pane, state, .. }
            | Event::DebriefProgress { pane, state, .. }
            | Event::DebriefOutcome { pane, state, .. } => {
                debriefs.insert(pane, state);
            }
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
                debriefs.remove(&pane);
                if at >= cutoff && matches!(reason.as_str(), "ended" | "dormant" | "plain-shell") {
                    history.closes.push_back(at);
                }
            }
            Event::PaneClosed { pane, .. } => {
                reported.remove(&pane);
                debriefs.remove(&pane);
            }
            _ => {}
        }) {
            tracing::warn!(%error, "failed to read auto-close history");
        }
        self.auto_close.reported = reported;
        drop(history);
        self.restore_debriefs(debriefs);
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
        self.auto_close.pass_deadline = Some(
            self.auto_close
                .pass_deadline
                .map_or(floor, |deadline| deadline.min(floor)),
        );
    }
    pub(crate) fn configure_auto_close(&mut self, config: Config) {
        if self.auto_close.config == config && self.auto_close.deadline.is_some() {
            return;
        }
        self.auto_close.pending = None;
        self.auto_close.refused.clear();
        self.auto_close.config = config;
        self.auto_close.deadline = (self.auto_close.config.mode != Mode::Off)
            .then(|| {
                Instant::now().checked_add(Duration::from_secs(
                    self.auto_close.config.pass_floor_secs.max(1),
                ))
            })
            .flatten();
        self.auto_close.pass_deadline = self.auto_close.deadline;
        let mut status = self
            .auto_close
            .status
            .0
            .write()
            .unwrap_or_else(|error| error.into_inner());
        status.mode = self.auto_close.config.mode;
        status.waiting = self
            .auto_close
            .debriefs
            .values()
            .filter(|state| matches!(state.phase, Phase::Waiting { .. }))
            .count() as u64;
    }

    fn auto_close_snapshot(&self, now: u64) -> Vec<PaneSnapshot> {
        self.auto_close_snapshot_for(now, false)
    }

    fn auto_close_snapshot_for(&self, now: u64, active_only: bool) -> Vec<PaneSnapshot> {
        let mut panes = Vec::new();
        for (ws_idx, ws) in self.state.workspaces.iter().enumerate() {
            let single_pane_workspace =
                ws.tabs.iter().map(|tab| tab.panes.len()).sum::<usize>() == 1;
            let group_refusal = single_pane_workspace
                && self
                    .state
                    .workspace_close_would_close_worktree_group(ws_idx);
            let group_members = ws.worktree_space().map_or_else(Vec::new, |space| {
                let mut members = Vec::new();
                for (other_idx, other_ws) in self.state.workspaces.iter().enumerate() {
                    if other_ws
                        .worktree_space()
                        .is_none_or(|other_space| other_space.key != space.key)
                    {
                        continue;
                    }
                    members.push(format!("ws:{}", other_ws.id));
                    for other_tab in &other_ws.tabs {
                        for (&other_id, other_pane) in &other_tab.panes {
                            let Some(other_pane_id) = self.public_pane_id(other_idx, other_id)
                            else {
                                continue;
                            };
                            let (pinned, flagged, ended, agent) = self
                                .state
                                .terminals
                                .get(&other_pane.attached_terminal_id)
                                .map_or((false, false, false, ""), |term| {
                                    (
                                        term.pinned,
                                        term.flag.is_some(),
                                        term.ended_agent.is_some(),
                                        term.effective_agent_label().unwrap_or(""),
                                    )
                                });
                            members.push(format!(
                                "pane:{other_pane_id}:{pinned}:{flagged}:{ended}:{agent}"
                            ));
                        }
                    }
                }
                members.sort();
                members
            });
            for tab in &ws.tabs {
                for (&id, pane) in &tab.panes {
                    let Some(terminal) = self.state.terminals.get(&pane.attached_terminal_id)
                    else {
                        continue;
                    };
                    let Some(pane_id) = self.public_pane_id(ws_idx, id) else {
                        continue;
                    };
                    if active_only
                        && !self
                            .auto_close
                            .debriefs
                            .get(&pane_id)
                            .is_some_and(Debrief::active)
                    {
                        continue;
                    }
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
                        debrief: self.auto_close.debriefs.get(&pane_id).cloned(),
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
                        input_seq: runtime.map_or(0, |runtime| runtime.input_or_command_seq()),
                        pinned: terminal.pinned,
                        flagged: terminal.flag.is_some(),
                        desk_focused: self.auto_close.focused.contains(&id),
                        restore_until: pane.agent_status_resolution_due_at(),
                        single_pane_workspace,
                        main_checkout: tab
                            .foreground_cwd_for_pane(id, &self.terminal_runtimes)
                            .or_else(|| {
                                tab.cwd_for_pane(id, &self.state.terminals, &self.terminal_runtimes)
                            })
                            .and_then(|cwd| crate::workspace::git_space_metadata(&cwd))
                            .filter(|info| !info.is_linked_worktree)
                            .map(|info| info.checkout_key),
                        group_refusal,
                        group_members: group_members.clone(),
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

    pub(crate) fn run_auto_close_pass(&mut self, monotonic: Instant) -> bool {
        let sequence = self.event_hub.current_sequence();
        let closed = self.advance_auto_close_pass(monotonic);
        closed || self.event_hub.current_sequence() != sequence
    }

    fn advance_auto_close_pass(&mut self, monotonic: Instant) -> bool {
        if self
            .auto_close
            .deadline
            .is_none_or(|deadline| monotonic < deadline)
        {
            return false;
        }
        if self.auto_close.config.mode == Mode::Close {
            self.advance_debriefs();
        }
        if self.auto_close.pending.is_none()
            && self
                .auto_close
                .pass_deadline
                .is_some_and(|deadline| monotonic < deadline)
        {
            self.auto_close.deadline = self.auto_close.pass_deadline;
            self.schedule_debrief_tick(monotonic);
            return false;
        }
        if let Some(pending) = self.auto_close.pending.as_ref() {
            match pending.receiver.try_recv() {
                Ok(result) => {
                    if let Some(pending) = self.auto_close.pending.take() {
                        return self.finish_auto_close_pass(
                            result,
                            &pending.panes,
                            pending.at,
                            monotonic,
                        );
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
            return false;
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
        false
    }

    pub(crate) fn record_auto_close_pane_closed(
        &mut self,
        pane_id: &str,
        by: CloseBy,
        reason: Option<&str>,
        age_secs: Option<u64>,
    ) {
        self.auto_close.reported.remove(pane_id);
        self.auto_close.refused.remove(pane_id);
        if by == CloseBy::Desk
            && age_secs.is_some()
            && reason.is_some_and(|reason| matches!(reason, "ended" | "dormant" | "plain-shell"))
        {
            self.auto_close
                .status
                .0
                .write()
                .unwrap_or_else(|error| error.into_inner())
                .closes
                .push_back(logbook::now_ms());
        }
    }

    pub(crate) fn close_auto_close_pane(
        &mut self,
        pane_id: &str,
        reason: &str,
        age_secs: u64,
    ) -> bool {
        let params = crate::api::schema::PaneCloseParams {
            pane_id: pane_id.to_owned(),
            by: Some(CloseBy::Desk),
            reason: Some(reason.to_owned()),
        };
        match self.close_pane_with_age("auto-close".into(), &params, Some(age_secs)) {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(
                    pane_id = %pane_id,
                    reason = %reason,
                    %error,
                    "auto-close refused to close pane"
                );
                false
            }
        }
    }

    fn finish_auto_close_pass(
        &mut self,
        mut result: Plan,
        previous: &[PaneSnapshot],
        at: u64,
        monotonic: Instant,
    ) -> bool {
        let now = crate::pane::unix_now_secs();
        // Gate execution may take seconds. Re-check protections and activity
        // before reporting a result about the still-live pane.
        let mut panes = self.auto_close_snapshot(now);
        let current = plan(&panes, &self.auto_close.config, now);
        result.candidates.retain(|candidate| {
            current.candidates.iter().any(|fresh| {
                fresh.pane_id == candidate.pane_id
                    && fresh.kind == candidate.kind
                    && (panes
                        .iter()
                        .find(|pane| pane.pane_id == candidate.pane_id)
                        .is_some_and(|pane| pane.debrief.is_some())
                        || now.saturating_sub(fresh.age_secs)
                            == at.saturating_sub(candidate.age_secs))
                    && panes
                        .iter()
                        .find(|pane| pane.pane_id == candidate.pane_id)
                        .zip(
                            previous
                                .iter()
                                .find(|pane| pane.pane_id == candidate.pane_id),
                        )
                        .is_some_and(|(pane, prev)| pane.same_close_facts(prev))
            })
        });
        let mut closed_any = false;
        if self.auto_close.config.mode == Mode::Close {
            let mut attempted = HashSet::new();
            let mut still_refused = HashSet::new();
            loop {
                let step_now = crate::pane::unix_now_secs().max(now);
                panes = self.auto_close_snapshot(step_now);
                for pane in &panes {
                    if !attempted.insert(format!("refusal:{}", pane.pane_id)) {
                        continue;
                    }
                    if let Some(candidate) = record_group_refusal(
                        &mut self.auto_close.refused,
                        &mut still_refused,
                        pane,
                        &panes,
                        &self.auto_close.config,
                        step_now,
                    ) {
                        tracing::warn!(
                            pane_id = %pane.pane_id,
                            reason = %candidate.reason,
                            code = "confirmation_required",
                            "auto-close refused to close pane that would close a worktree group"
                        );
                    }
                }
                let current = plan(&panes, &self.auto_close.config, step_now);
                let next = current.candidates.into_iter().find(|candidate| {
                    matches!(candidate.kind, Kind::Ended | Kind::PlainShell)
                        && candidate.action == Action::Close
                        && !attempted.contains(&candidate.pane_id)
                        && candidate_ready_to_close(
                            candidate,
                            previous,
                            &panes,
                            &self.auto_close.refused,
                            &mut still_refused,
                            &self.auto_close.config,
                            step_now,
                        )
                });
                let Some(candidate) = next else {
                    break;
                };
                attempted.insert(candidate.pane_id.clone());
                let check_now = crate::pane::unix_now_secs().max(step_now);
                let fresh_panes = self.auto_close_snapshot(check_now);
                if !candidate_ready_to_close(
                    &candidate,
                    previous,
                    &fresh_panes,
                    &self.auto_close.refused,
                    &mut still_refused,
                    &self.auto_close.config,
                    check_now,
                ) {
                    continue;
                }
                if self.close_auto_close_pane(
                    &candidate.pane_id,
                    &candidate.reason,
                    candidate.age_secs,
                ) {
                    closed_any = true;
                } else if let Some(pane) = fresh_panes
                    .iter()
                    .find(|pane| pane.pane_id == candidate.pane_id)
                {
                    let key = pane.refusal_key(&candidate.reason, check_now, candidate.age_secs);
                    still_refused.insert(pane.pane_id.clone());
                    self.auto_close.refused.insert(pane.pane_id.clone(), key);
                }
            }
            self.auto_close
                .refused
                .retain(|id, _| still_refused.contains(id));
            self.flush_logbook();
            panes = self.auto_close_snapshot(now);
        }
        self.auto_close
            .status
            .0
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .waiting = result
            .candidates
            .iter()
            .filter(|candidate| {
                candidate.action == Action::Wait
                    && !matches!(
                        candidate.wait_reason.as_deref(),
                        Some("debrief-in-progress" | "retry-working")
                    )
            })
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
        } else if self.auto_close.config.mode == Mode::Close {
            for candidate in &result.candidates {
                if candidate.kind != Kind::Dormant {
                    continue;
                }
                let Some(pane) = panes.iter().find(|pane| pane.pane_id == candidate.pane_id) else {
                    continue;
                };
                match candidate.action {
                    Action::Debrief if !self.auto_close.debriefs.values().any(Debrief::active) => {
                        self.start_debrief(pane, candidate)
                    }
                    Action::Close if pane.debrief.is_none() || !self.auto_close.config.debrief => {
                        self.close_dormant_pane(
                            pane,
                            candidate.age_secs,
                            logbook::CloseDebrief {
                                outcome: crate::api::schema::DebriefOutcome::Skipped,
                                memories: None,
                                skill_version: None,
                            },
                        )
                    }
                    _ => {}
                }
            }
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
        self.auto_close.pass_deadline = self.auto_close.deadline;
        self.schedule_debrief_tick(monotonic);
        closed_any
    }

    fn schedule_debrief_tick(&mut self, now: Instant) {
        if self.auto_close.config.mode == Mode::Close
            && self.auto_close.debriefs.values().any(Debrief::active)
        {
            let delay =
                if self.auto_close.delivery.is_some() || self.auto_close.submission.is_some() {
                    25
                } else {
                    self.auto_close
                        .debriefs
                        .values()
                        .filter_map(|state| state.deadline(&self.auto_close.config))
                        .min()
                        .unwrap_or(logbook::now_ms())
                        .saturating_sub(logbook::now_ms())
                        .max(100)
                };
            let Some(tick) = now.checked_add(Duration::from_millis(delay)) else {
                return;
            };
            self.auto_close.deadline = Some(
                self.auto_close
                    .deadline
                    .map_or(tick, |deadline| deadline.min(tick)),
            );
        }
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
            input_seq: 0,
            pinned: false,
            flagged: false,
            desk_focused: false,
            restore_until: None,
            single_pane_workspace: true,
            main_checkout: None,
            group_refusal: false,
            group_members: Vec::new(),
            bare_prompt: true,
            starter: None,
            session: None,
            debrief: None,
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
        pane.input_at = 125;
        assert_eq!(decide(&pane, &[], &config, 134), Err("too-young"));
        assert_eq!(next_pass_delay(&[pane.clone()], &config, 130), 5);
        let candidate = decide(&pane, &[], &config, 135).unwrap();
        assert_eq!(candidate.kind, Kind::Ended);
        assert_eq!(candidate.age_secs, 10);
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

    #[tokio::test]
    async fn auto_close_close_mode_waits_for_dormant_agents_while_closing_shells_and_ended_panes() {
        let mut dormant = shell();
        dormant.pane_id = "pane-dormant".into();
        dormant.agent = Some("pi".into());
        dormant.status = AgentStatus::Idle;
        let mut ended = shell();
        ended.pane_id = "pane-ended".into();
        ended.ended = true;
        ended.ended_at = Some(100);
        let mut legacy_ended = shell();
        legacy_ended.pane_id = "pane-legacy-ended".into();
        legacy_ended.ended = true;
        let mut plain = shell();
        plain.pane_id = "pane-shell".into();
        let panes = vec![dormant, ended, legacy_ended, plain];
        let config = Config {
            mode: Mode::Close,
            ended_age_secs: 0,
            dormant_age_secs: 0,
            plain_shell_age_secs: 0,
            debrief: true,
            debrief_skill_path: String::new(),
            ..Config::default()
        };
        let evaluated = evaluate(&panes, &config, 200).await;
        assert_eq!(evaluated.candidates.len(), 3);
        assert_eq!(evaluated.spared.len(), 1);
        assert_eq!(evaluated.spared[0].pane_id, "pane-legacy-ended");
        assert_eq!(evaluated.spared[0].exemption, "ended-time-unknown");
        assert_eq!(evaluated.candidates[0].kind, Kind::Dormant);
        assert_eq!(evaluated.candidates[0].action, Action::Wait);
        assert_eq!(
            evaluated.candidates[0].wait_reason.as_deref(),
            Some("skill-missing")
        );
        assert_eq!(evaluated.candidates[1].kind, Kind::Ended);
        assert_eq!(evaluated.candidates[1].action, Action::Close);
        assert_eq!(evaluated.candidates[2].kind, Kind::PlainShell);
        assert_eq!(evaluated.candidates[2].action, Action::Close);
    }

    #[test]
    fn auto_close_close_pane_records_desk_provenance_and_refuses_confirmation_required_group() {
        let path = std::env::temp_dir().join(format!(
            "herdr-auto-close-unit-{}-{}.jsonl",
            std::process::id(),
            logbook::now_ms()
        ));
        let (_, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.logbook_events = Some(Vec::new());
        app.logbook_writer = Some(
            logbook::BackgroundWriter::start(path.clone(), Vec::new(), HashSet::new()).unwrap(),
        );
        app.state.confirm_close = true;
        let mut parent = crate::workspace::Workspace::test_new("parent");
        parent.id = "wparent".into();
        parent.worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "/tmp/repo/.git".into(),
            label: "repo".into(),
            repo_root: "/tmp/repo".into(),
            checkout_path: "/tmp/repo".into(),
            is_linked_worktree: false,
        });
        parent.test_add_tab(None);
        let mut child = crate::workspace::Workspace::test_new("child");
        child.id = "wchild".into();
        child.worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "/tmp/repo/.git".into(),
            label: "repo".into(),
            repo_root: "/tmp/repo".into(),
            checkout_path: "/tmp/repo-child".into(),
            is_linked_worktree: true,
        });
        app.state.workspaces = vec![parent, child];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);

        let pane_ids = app.state.pane_ids_for_workspace(0);
        let first = app.public_pane_id(0, pane_ids[0]).unwrap();
        let second = app.public_pane_id(0, pane_ids[1]).unwrap();

        assert!(app.close_auto_close_pane(&second, "ended", 42));
        app.drain_logbook();
        assert_eq!(
            app.auto_close.status.0.read().unwrap().closes.len(),
            1,
            "first close should increment closes history"
        );
        let mut events = Vec::new();
        logbook::for_each_event(&path, |event| events.push(event)).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            Event::PaneClosed {
                pane,
                by,
                reason,
                age_secs,
                ..
            } => {
                assert_eq!(pane, &second);
                assert_eq!(*by, Some(CloseBy::Desk));
                assert_eq!(reason.as_deref(), Some("ended"));
                assert_eq!(*age_secs, Some(42));
            }
            other => panic!("unexpected event: {other:?}"),
        }

        // Now `first` is the last pane in the parent workspace of a worktree group;
        // `close_auto_close_pane` must refuse with `confirmation_required` and never force.
        assert!(!app.close_auto_close_pane(&first, "plain-shell", 99));
        app.drain_logbook();
        assert_eq!(app.auto_close.status.0.read().unwrap().closes.len(), 1);
        let mut after_refusal = Vec::new();
        logbook::for_each_event(&path, |event| after_refusal.push(event)).unwrap();
        assert_eq!(after_refusal.len(), 1);
        assert_eq!(
            app.public_pane_id(0, pane_ids[0]).as_deref(),
            Some(first.as_str())
        );
        app.shutdown_logbook();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn auto_close_pre_close_recheck_aborts_on_any_changed_pane_fact() {
        let config = Config {
            mode: Mode::Close,
            ended_age_secs: 10,
            plain_shell_age_secs: 10,
            ..Config::default()
        };
        let mut base = shell();
        base.ended = true;
        base.ended_at = Some(100);
        base.input_at = 100;
        base.input_seq = 3;
        let candidate = decide(&base, std::slice::from_ref(&base), &config, 120).unwrap();
        let refused = HashMap::new();
        let mut still_refused = HashSet::new();
        assert!(candidate_ready_to_close(
            &candidate,
            std::slice::from_ref(&base),
            std::slice::from_ref(&base),
            &refused,
            &mut still_refused,
            &config,
            120,
        ));

        let mutations: [fn(&mut PaneSnapshot); 12] = [
            |pane| pane.input_at = 105,
            |pane| pane.input_seq = 4,
            |pane| pane.status = AgentStatus::Working,
            |pane| pane.status_at = 105,
            |pane| pane.agent = Some("pi".into()),
            |pane| pane.ended = false,
            |pane| pane.ended_at = Some(101),
            |pane| pane.pinned = true,
            |pane| pane.flagged = true,
            |pane| pane.desk_focused = true,
            |pane| pane.main_checkout = Some("checkout".into()),
            |pane| {
                pane.session = Some(CloseSession {
                    agent: "pi".into(),
                    id: "session-2".into(),
                })
            },
        ];
        for mutate in mutations {
            let mut changed = base.clone();
            mutate(&mut changed);
            assert!(!candidate_ready_to_close(
                &candidate,
                std::slice::from_ref(&base),
                std::slice::from_ref(&changed),
                &refused,
                &mut still_refused,
                &config,
                120,
            ));
        }

        // Re-evaluating the main-checkout home exemption on the live workspace state
        // immediately before each close ensures closing one sibling never takes the
        // last pane of a checkout in the same pass.
        let mut first = main_checkout_shell("w1", 100, 100);
        first.single_pane_workspace = false;
        let mut second = main_checkout_shell("w1", 100, 100);
        second.pane_id = "w1:p2".into();
        second.single_pane_workspace = false;
        let initial_panes = [first.clone(), second.clone()];
        let second_candidate = decide(&second, &initial_panes, &config, 120).unwrap();
        let mut live_second = second.clone();
        live_second.single_pane_workspace = true;
        assert!(!candidate_ready_to_close(
            &second_candidate,
            &initial_panes,
            std::slice::from_ref(&live_second),
            &refused,
            &mut still_refused,
            &config,
            120,
        ));
    }

    #[test]
    fn auto_close_deduplicates_unchanged_group_refusals_until_group_or_pane_facts_change() {
        let config = Config {
            mode: Mode::Close,
            plain_shell_age_secs: 10,
            ..Config::default()
        };
        let mut root = shell();
        root.pane_id = "pane-root".into();
        root.main_checkout = Some("checkout".into());
        root.group_refusal = true;
        root.group_members = vec![
            "pane:pane-root:false:false:false:".into(),
            "pane:pane-child-1:true:false:false:".into(),
            "pane:pane-child-2:true:false:false:".into(),
        ];
        let mut refused = HashMap::new();

        // Pass 1 logs the refusal once.
        let mut still_refused = HashSet::new();
        assert!(record_group_refusal(
            &mut refused,
            &mut still_refused,
            &root,
            std::slice::from_ref(&root),
            &config,
            200,
        )
        .is_some());
        refused.retain(|id, _| still_refused.contains(id));

        // Subsequent passes with unchanged pane and group facts do not re-log.
        for now in [201, 205] {
            let mut still_refused = HashSet::new();
            assert!(record_group_refusal(
                &mut refused,
                &mut still_refused,
                &root,
                std::slice::from_ref(&root),
                &config,
                now,
            )
            .is_none());
            refused.retain(|id, _| still_refused.contains(id));
            assert!(refused.contains_key("pane-root"));
        }

        // Changing the group (e.g. a pinned child closes) lets the refusal be reconsidered.
        root.group_members.pop();
        let mut still_refused = HashSet::new();
        assert!(record_group_refusal(
            &mut refused,
            &mut still_refused,
            &root,
            std::slice::from_ref(&root),
            &config,
            210,
        )
        .is_some());
        refused.retain(|id, _| still_refused.contains(id));

        // When the last child closes so `group_refusal` clears, the refusal entry is dropped.
        root.group_refusal = false;
        let mut still_refused = HashSet::new();
        assert!(record_group_refusal(
            &mut refused,
            &mut still_refused,
            &root,
            std::slice::from_ref(&root),
            &config,
            215,
        )
        .is_none());
        refused.retain(|id, _| still_refused.contains(id));
        assert!(refused.is_empty());
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
