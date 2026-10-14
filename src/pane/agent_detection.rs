use std::sync::atomic::{AtomicU64, Ordering};

use crate::detect::{Agent, AgentDetection, AgentState};

pub(super) const AGENT_PENDING_IDLE_RECHECK: std::time::Duration =
    std::time::Duration::from_millis(100);
const AGENT_PENDING_IDLE_CONFIRMATIONS: u8 = 3;
pub(super) const AGENT_PENDING_IDLE_CAP: std::time::Duration =
    std::time::Duration::from_millis(700);
pub(super) const STABLE_VISIBLE_SIGNAL_REFRESH: std::time::Duration =
    std::time::Duration::from_millis(800);
pub(super) const AGENT_STARTUP_GRACE_WINDOW: std::time::Duration =
    std::time::Duration::from_secs(3);
/// How long an idle verdict that needs settling must hold on every read
/// before it publishes. Such a verdict rests on text a streaming answer can
/// draw for a moment with no status line on screen; a finished turn's screen
/// stays put. The flicker cap above covers frame-to-frame redraws, not pauses
/// between streamed chunks, so this matches the startup grace window instead:
/// long enough to outlast a pause in a stream, short enough for idle waits.
pub(super) const SETTLED_IDLE_WINDOW: std::time::Duration = AGENT_STARTUP_GRACE_WINDOW;
/// How long a working verdict that only the terminal title supports must
/// persist over an unchanged screen before it starts a turn. Agents animate
/// their title for work that is not a turn: booting a tool server spins it
/// for a second or two with the screen untouched, and a server that fails to
/// boot spins it until the agent's startup timeout. A turn draws on the
/// screen, and a changed screen does not wait here.
pub(super) const TITLE_ONLY_WORKING_GRACE: std::time::Duration = std::time::Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DetectionPublishState {
    pub(super) state: AgentState,
    pub(super) visible_idle: bool,
    pub(super) visible_blocker: bool,
    pub(super) visible_working: bool,
    pub(super) idle_needs_settling: bool,
    pub(super) working_from_title_only: bool,
}

#[derive(Debug, Default)]
pub(super) struct PendingIdleConfirmation {
    started_at: Option<std::time::Instant>,
    confirmations: u8,
    settling: bool,
    /// When a title-only working verdict began to be held back.
    title_only_working_since: Option<std::time::Instant>,
}

impl PendingIdleConfirmation {
    /// Whether a held verdict waits on the next read. A held title-only
    /// working verdict counts, so its screen keeps being read until the
    /// title settles or the hold matures.
    pub(super) fn active(&self) -> bool {
        self.started_at.is_some() || self.title_only_working_since.is_some()
    }

    pub(super) fn clear(&mut self) {
        self.started_at = None;
        self.confirmations = 0;
        self.settling = false;
        self.title_only_working_since = None;
    }

    /// Hold back a working verdict that only the terminal title supports
    /// while the screen stays unchanged, until it has persisted for
    /// [`TITLE_ONLY_WORKING_GRACE`].
    ///
    /// Publishing working and then idle again would read as a finished turn,
    /// so a title that spins for a moment over a screen that never changed
    /// would otherwise mint a completion nobody caused. A screen that changes
    /// on the read, such as an answer streaming in with its status row
    /// hidden, is treated as evidence of a turn and publishes at once; so is
    /// an unrelated redraw, which is why the hold only narrows the case of a
    /// title spinning over a still screen. Any other verdict ends the
    /// hold without publishing. A pane already working keeps working on the
    /// title alone; only the start of a turn is held.
    pub(super) fn should_hold_title_only_working(
        &mut self,
        previous: DetectionPublishState,
        next: DetectionPublishState,
        screen_changed: bool,
        agent_changed: bool,
        process_exited: bool,
        now: std::time::Instant,
    ) -> bool {
        let starts_on_title_only = previous.state != AgentState::Working
            && next.state == AgentState::Working
            && next.working_from_title_only
            && !screen_changed
            && !agent_changed
            && !process_exited;

        if !starts_on_title_only {
            self.title_only_working_since = None;
            return false;
        }
        let hold = match self.title_only_working_since {
            Some(since) if now.duration_since(since) >= TITLE_ONLY_WORKING_GRACE => false,
            Some(_) => true,
            None => {
                self.title_only_working_since = Some(now);
                true
            }
        };
        if hold {
            // A held reading is not idle, so an idle confirmation in progress
            // starts over rather than counting these reads toward it.
            self.started_at = None;
            self.confirmations = 0;
            self.settling = false;
        } else {
            self.title_only_working_since = None;
        }
        hold
    }

    pub(super) fn should_hold_idle(
        &mut self,
        previous: DetectionPublishState,
        next: DetectionPublishState,
        agent_changed: bool,
        process_exited: bool,
        now: std::time::Instant,
    ) -> bool {
        let is_idle_to_settle = previous.state != AgentState::Idle
            && next.state == AgentState::Idle
            && next.idle_needs_settling
            && !next.visible_blocker
            && !agent_changed
            && !process_exited;

        if is_idle_to_settle {
            // Any other verdict in between clears the hold and restarts the window.
            match self.started_at.filter(|_| self.settling) {
                Some(started_at) if now.duration_since(started_at) >= SETTLED_IDLE_WINDOW => {
                    self.clear();
                    return false;
                }
                Some(_) => {}
                None => {
                    self.started_at = Some(now);
                    self.confirmations = 0;
                    self.settling = true;
                }
            }
            return true;
        }
        if self.settling {
            self.clear();
        }

        let is_working_to_plain_idle = previous.state == AgentState::Working
            && next.state == AgentState::Idle
            && !next.visible_idle
            && !next.visible_blocker
            && !agent_changed
            && !process_exited;

        if !is_working_to_plain_idle {
            self.clear();
            return false;
        }

        let Some(started_at) = self.started_at else {
            self.started_at = Some(now);
            self.confirmations = 0;
            return true;
        };

        if now.duration_since(started_at) >= AGENT_PENDING_IDLE_CAP {
            self.clear();
            return false;
        }

        self.confirmations = self.confirmations.saturating_add(1);
        if self.confirmations >= AGENT_PENDING_IDLE_CONFIRMATIONS {
            self.clear();
            return false;
        }

        true
    }
}

/// The screen text a pane task last read.
#[derive(Debug, Default)]
pub(super) struct ScreenTextBaseline {
    text: String,
    observed: bool,
}

/// What one read says about the screen text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ScreenTextChange {
    /// The text differs from what the baseline holds, empty before any read.
    pub(super) content_changed: bool,
    /// The text differs from an earlier read's. A first read has nothing to
    /// differ from, so it is never a change.
    pub(super) screen_changed: bool,
}

impl ScreenTextBaseline {
    pub(super) fn observe(&mut self, content: &str) -> ScreenTextChange {
        let content_changed = content != self.text;
        let change = ScreenTextChange {
            content_changed,
            screen_changed: self.observed && content_changed,
        };
        if content_changed {
            self.text.clear();
            self.text.push_str(content);
        }
        self.observed = true;
        change
    }

    pub(super) fn clear(&mut self) {
        self.text.clear();
        self.observed = false;
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct IdleScreenScanSkipInput {
    pub(super) state: AgentState,
    pub(super) agent: Option<Agent>,
    pub(super) pending_idle_active: bool,
    pub(super) agent_changed: bool,
    pub(super) process_exited: bool,
    pub(super) current_detection_content_seq: Option<u64>,
    pub(super) last_screen_scan_detection_content_seq: Option<u64>,
}

pub(super) fn should_skip_idle_screen_scan(input: IdleScreenScanSkipInput) -> bool {
    let stable_state = input.state == AgentState::Idle
        || (input.state == AgentState::Unknown && input.agent == Some(Agent::Codex));
    if !stable_state
        || input.agent.is_none()
        || input.pending_idle_active
        || input.agent_changed
        || input.process_exited
    {
        return false;
    }

    input.current_detection_content_seq.is_some()
        && input.last_screen_scan_detection_content_seq == input.current_detection_content_seq
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DetectionScreenReadDecision {
    Read,
    Skip,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct DetectionScreenReadInput {
    pub(super) state: AgentState,
    pub(super) agent: Option<Agent>,
    pub(super) pending_idle_active: bool,
    pub(super) agent_changed: bool,
    pub(super) process_exited: bool,
    pub(super) current_detection_content_seq: Option<u64>,
    pub(super) last_screen_scan_detection_content_seq: Option<u64>,
}

pub(super) fn decide_detection_screen_read(
    input: DetectionScreenReadInput,
) -> DetectionScreenReadDecision {
    if should_skip_idle_screen_scan(IdleScreenScanSkipInput {
        state: input.state,
        agent: input.agent,
        pending_idle_active: input.pending_idle_active,
        agent_changed: input.agent_changed,
        process_exited: input.process_exited,
        current_detection_content_seq: input.current_detection_content_seq,
        last_screen_scan_detection_content_seq: input.last_screen_scan_detection_content_seq,
    }) {
        DetectionScreenReadDecision::Skip
    } else {
        DetectionScreenReadDecision::Read
    }
}

pub(super) fn should_publish_detection_update(
    previous: DetectionPublishState,
    next: DetectionPublishState,
    agent_changed: bool,
    process_exited: bool,
    stable_visible_signal_refresh_due: bool,
) -> bool {
    next.state != previous.state
        || next.visible_idle != previous.visible_idle
        || next.visible_blocker != previous.visible_blocker
        || next.visible_working != previous.visible_working
        || agent_changed
        || process_exited
        || (stable_visible_signal_refresh_due && next.visible_blocker && previous.visible_blocker)
}

pub(super) fn stable_visible_signal_refresh_due(
    previous: DetectionPublishState,
    next: DetectionPublishState,
    last_refresh: Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    let stable_visible_signal = next.visible_blocker && previous.visible_blocker;

    stable_visible_signal
        && last_refresh.is_none_or(|last_refresh| {
            now.duration_since(last_refresh) >= STABLE_VISIBLE_SIGNAL_REFRESH
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DetectionTransitionDecision {
    NoPublish,
    PublishNext,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct DetectionTransitionInput {
    pub(super) previous_publish: DetectionPublishState,
    pub(super) next_publish: DetectionPublishState,
    /// The screen text differs from an earlier read's. A first read has
    /// nothing to differ from, so it is not a change.
    pub(super) screen_changed: bool,
    pub(super) agent_changed: bool,
    pub(super) process_exited: bool,
    pub(super) stable_refresh_due: bool,
    pub(super) now: std::time::Instant,
}

pub(super) fn decide_detection_transition(
    input: DetectionTransitionInput,
    pending_idle: &mut PendingIdleConfirmation,
) -> DetectionTransitionDecision {
    // Ahead of the idle hold: a held title restarts any idle confirmation
    // itself, and its reads must not count toward one.
    if pending_idle.should_hold_title_only_working(
        input.previous_publish,
        input.next_publish,
        input.screen_changed,
        input.agent_changed,
        input.process_exited,
        input.now,
    ) {
        return DetectionTransitionDecision::NoPublish;
    }

    if pending_idle.should_hold_idle(
        input.previous_publish,
        input.next_publish,
        input.agent_changed,
        input.process_exited,
        input.now,
    ) {
        return DetectionTransitionDecision::NoPublish;
    }

    if should_publish_detection_update(
        input.previous_publish,
        input.next_publish,
        input.agent_changed,
        input.process_exited,
        input.stable_refresh_due,
    ) {
        return DetectionTransitionDecision::PublishNext;
    }

    DetectionTransitionDecision::NoPublish
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DetectionPublishDecision {
    NoPublish,
    Publish {
        state: AgentState,
        visible_idle: bool,
        visible_blocker: bool,
        visible_working: bool,
        process_exited: bool,
    },
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ScreenDetectionPublishInput {
    pub(super) current_state: AgentState,
    pub(super) last_visible_idle: bool,
    pub(super) last_visible_blocker: bool,
    pub(super) last_visible_working: bool,
    pub(super) last_visible_signal_refresh: Option<std::time::Instant>,
    pub(super) screen_detection: AgentDetection,
    /// The screen text differs from an earlier read's. A first read has
    /// nothing to differ from, so it is not a change.
    pub(super) screen_changed: bool,
    pub(super) process_exited: bool,
    pub(super) agent_changed: bool,
    pub(super) now: std::time::Instant,
}

pub(super) fn decide_screen_detection_publish(
    input: ScreenDetectionPublishInput,
    pending_idle: &mut PendingIdleConfirmation,
) -> DetectionPublishDecision {
    let detection = input.screen_detection;
    let new_state = crate::terminal::state::stabilize_agent_detection(detection);
    let visible_idle = detection.visible_idle && new_state == AgentState::Idle;
    let visible_blocker = detection.visible_blocker && new_state == AgentState::Blocked;
    let visible_working = detection.visible_working && new_state == AgentState::Working;
    let idle_needs_settling = detection.idle_needs_settling && new_state == AgentState::Idle;
    let working_from_title_only =
        detection.working_from_title_only && new_state == AgentState::Working;

    let previous_publish = DetectionPublishState {
        state: input.current_state,
        visible_idle: input.last_visible_idle,
        visible_blocker: input.last_visible_blocker,
        visible_working: input.last_visible_working,
        idle_needs_settling: false,
        working_from_title_only: false,
    };
    let next_publish = DetectionPublishState {
        state: new_state,
        visible_idle,
        visible_blocker,
        visible_working,
        idle_needs_settling,
        working_from_title_only,
    };
    let stable_refresh_due = stable_visible_signal_refresh_due(
        previous_publish,
        next_publish,
        input.last_visible_signal_refresh,
        input.now,
    );

    match decide_detection_transition(
        DetectionTransitionInput {
            previous_publish,
            next_publish,
            screen_changed: input.screen_changed,
            agent_changed: input.agent_changed,
            process_exited: input.process_exited,
            stable_refresh_due,
            now: input.now,
        },
        pending_idle,
    ) {
        DetectionTransitionDecision::NoPublish => DetectionPublishDecision::NoPublish,
        DetectionTransitionDecision::PublishNext => DetectionPublishDecision::Publish {
            state: new_state,
            visible_idle,
            visible_blocker,
            visible_working,
            process_exited: input.process_exited,
        },
    }
}

#[allow(dead_code)] // shim for tests; detection_update_for_publish_with_osc is the real path
pub(super) fn detection_update_for_publish(
    agent: Option<Agent>,
    content: &str,
    process_exited: bool,
) -> Option<crate::detect::AgentDetection> {
    detection_update_for_publish_with_osc(agent, content, "", "", process_exited)
}

pub(super) fn detection_update_for_publish_with_osc(
    agent: Option<Agent>,
    content: &str,
    osc_title: &str,
    osc_progress: &str,
    process_exited: bool,
) -> Option<crate::detect::AgentDetection> {
    if process_exited {
        return Some(crate::detect::AgentDetection {
            state: AgentState::Idle,
            skip_state_update: false,
            visible_idle: true,
            visible_blocker: false,
            visible_working: false,
            idle_needs_settling: false,
            working_from_title_only: false,
        });
    }

    let detection = crate::detect::detect_agent_with_osc(agent, content, osc_title, osc_progress);
    (!detection.skip_state_update).then_some(detection)
}

pub(super) fn codex_prompt_ready(content: &str) -> bool {
    // The composer can remain visible during a turn; this is startup evidence only.
    let recent: String = content
        .lines()
        .rev()
        .take(12)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .flat_map(str::chars)
        .filter(|ch| !ch.is_whitespace())
        .collect();
    recent.contains("›AskCodextodoanything")
        && !recent.contains("model:loading")
        && !recent.contains("Resumingsession")
}

pub(super) fn observe_detection_content_change(bytes: &[u8], detection_content_seq: &AtomicU64) {
    if !bytes.is_empty() {
        detection_content_seq.fetch_add(1, Ordering::Relaxed);
    }
}

pub(super) fn mark_detection_content_changed(detection_content_seq: &AtomicU64) {
    detection_content_seq.fetch_add(1, Ordering::Relaxed);
}

#[cfg(test)]
mod settled_idle_tests;

#[cfg(test)]
mod title_only_working_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn publish_state(state: AgentState) -> DetectionPublishState {
        DetectionPublishState {
            state,
            visible_idle: false,
            visible_blocker: false,
            visible_working: false,
            idle_needs_settling: false,
            working_from_title_only: false,
        }
    }

    fn screen_detection(state: AgentState) -> AgentDetection {
        AgentDetection {
            state,
            skip_state_update: false,
            visible_idle: state == AgentState::Idle,
            visible_blocker: false,
            visible_working: state == AgentState::Working,
            idle_needs_settling: false,
            working_from_title_only: false,
        }
    }

    fn screen_publish_input(
        current_state: AgentState,
        screen_detection: AgentDetection,
        now: std::time::Instant,
    ) -> ScreenDetectionPublishInput {
        ScreenDetectionPublishInput {
            current_state,
            last_visible_idle: false,
            last_visible_blocker: false,
            last_visible_working: false,
            last_visible_signal_refresh: None,
            screen_detection,
            screen_changed: false,
            process_exited: false,
            agent_changed: false,
            now,
        }
    }

    fn screen_read_input(state: AgentState, current_seq: u64) -> DetectionScreenReadInput {
        DetectionScreenReadInput {
            state,
            agent: Some(Agent::Codex),
            pending_idle_active: false,
            agent_changed: false,
            process_exited: false,
            current_detection_content_seq: Some(current_seq),
            last_screen_scan_detection_content_seq: Some(10),
        }
    }

    #[test]
    fn codex_startup_prompt_survives_terminal_wraps() {
        let wrapped = "header\n› Ask Codex to do\nanything\nfooter";
        assert!(codex_prompt_ready(wrapped));
        assert!(!codex_prompt_ready(&format!("model: load\ning\n{wrapped}")));
    }

    #[test]
    fn screen_read_skips_unchanged_idle_bottom_buffer() {
        assert_eq!(
            decide_detection_screen_read(screen_read_input(AgentState::Idle, 10)),
            DetectionScreenReadDecision::Skip
        );
    }

    #[test]
    fn screen_read_skips_unchanged_ambiguous_codex_but_not_new_content_or_replacement() {
        let mut input = screen_read_input(AgentState::Unknown, 10);
        assert_eq!(
            decide_detection_screen_read(input),
            DetectionScreenReadDecision::Skip
        );
        input.current_detection_content_seq = Some(11);
        assert_eq!(
            decide_detection_screen_read(input),
            DetectionScreenReadDecision::Read
        );
        input.current_detection_content_seq = Some(10);
        input.agent_changed = true;
        assert_eq!(
            decide_detection_screen_read(input),
            DetectionScreenReadDecision::Read
        );
        input.agent_changed = false;
        input.agent = Some(Agent::Pi);
        assert_eq!(
            decide_detection_screen_read(input),
            DetectionScreenReadDecision::Read
        );
    }

    #[test]
    fn screen_read_reads_when_idle_bottom_buffer_changes() {
        assert_eq!(
            decide_detection_screen_read(screen_read_input(AgentState::Idle, 11)),
            DetectionScreenReadDecision::Read
        );
    }

    #[test]
    fn screen_read_reads_for_transitions_and_missing_agent() {
        let mut input = screen_read_input(AgentState::Idle, 10);
        input.pending_idle_active = true;
        assert_eq!(
            decide_detection_screen_read(input),
            DetectionScreenReadDecision::Read
        );

        let mut input = screen_read_input(AgentState::Idle, 10);
        input.agent_changed = true;
        assert_eq!(
            decide_detection_screen_read(input),
            DetectionScreenReadDecision::Read
        );

        let mut input = screen_read_input(AgentState::Idle, 10);
        input.process_exited = true;
        assert_eq!(
            decide_detection_screen_read(input),
            DetectionScreenReadDecision::Read
        );

        let mut input = screen_read_input(AgentState::Idle, 10);
        input.agent = None;
        assert_eq!(
            decide_detection_screen_read(input),
            DetectionScreenReadDecision::Read
        );
    }

    #[test]
    fn pending_idle_holds_working_to_plain_idle_until_confirmed() {
        let now = std::time::Instant::now();
        let previous = publish_state(AgentState::Working);
        let next = publish_state(AgentState::Idle);
        let mut pending = PendingIdleConfirmation::default();

        assert!(pending.should_hold_idle(previous, next, false, false, now));
        assert!(pending.should_hold_idle(
            previous,
            next,
            false,
            false,
            now + AGENT_PENDING_IDLE_RECHECK
        ));
        assert!(pending.should_hold_idle(
            previous,
            next,
            false,
            false,
            now + AGENT_PENDING_IDLE_RECHECK * 2
        ));
        assert!(!pending.should_hold_idle(
            previous,
            next,
            false,
            false,
            now + AGENT_PENDING_IDLE_RECHECK * 3
        ));
    }

    #[test]
    fn visible_idle_bypasses_plain_idle_hold() {
        let now = std::time::Instant::now();
        let previous = publish_state(AgentState::Working);
        let mut next = publish_state(AgentState::Idle);
        next.visible_idle = true;
        let mut pending = PendingIdleConfirmation::default();

        assert!(!pending.should_hold_idle(previous, next, false, false, now));
    }

    #[test]
    fn transition_decision_publishes_next_for_visible_blocker() {
        let now = std::time::Instant::now();
        let mut pending_idle = PendingIdleConfirmation::default();
        let mut blocked = publish_state(AgentState::Blocked);
        blocked.visible_blocker = true;

        assert_eq!(
            decide_detection_transition(
                DetectionTransitionInput {
                    previous_publish: publish_state(AgentState::Idle),
                    next_publish: blocked,
                    screen_changed: false,
                    agent_changed: false,
                    process_exited: false,
                    stable_refresh_due: false,
                    now,
                },
                &mut pending_idle,
            ),
            DetectionTransitionDecision::PublishNext
        );
    }

    #[test]
    fn screen_publish_keeps_visible_working_without_pty_activity() {
        let now = std::time::Instant::now();
        let mut pending_idle = PendingIdleConfirmation::default();

        assert_eq!(
            decide_screen_detection_publish(
                screen_publish_input(AgentState::Idle, screen_detection(AgentState::Working), now,),
                &mut pending_idle,
            ),
            DetectionPublishDecision::Publish {
                state: AgentState::Working,
                visible_idle: false,
                visible_blocker: false,
                visible_working: true,
                process_exited: false,
            }
        );
    }

    #[test]
    fn screen_publish_can_publish_idle_without_input_taint_delay() {
        let now = std::time::Instant::now();
        let mut pending_idle = PendingIdleConfirmation::default();

        assert_eq!(
            decide_screen_detection_publish(
                screen_publish_input(AgentState::Blocked, screen_detection(AgentState::Idle), now,),
                &mut pending_idle,
            ),
            DetectionPublishDecision::Publish {
                state: AgentState::Idle,
                visible_idle: true,
                visible_blocker: false,
                visible_working: false,
                process_exited: false,
            }
        );
    }

    #[test]
    fn detection_content_change_tracks_raw_nonempty_reads_for_scan_scheduling() {
        let seq = AtomicU64::new(0);

        observe_detection_content_change(b"", &seq);
        assert_eq!(seq.load(Ordering::Relaxed), 0);

        observe_detection_content_change(b"\x1b[?2026h", &seq);
        assert_eq!(seq.load(Ordering::Relaxed), 1);

        observe_detection_content_change(b"body bytes", &seq);
        assert_eq!(seq.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn local_terminal_mutations_can_invalidate_idle_scan_skip() {
        let seq = AtomicU64::new(0);

        mark_detection_content_changed(&seq);

        assert_eq!(seq.load(Ordering::Relaxed), 1);
    }
}
