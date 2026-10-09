use crate::api::schema::AgentStatus;
use crate::events::StatusReading;
use crate::terminal::TerminalId;

/// How long a rebuilt pane may wait for something to classify it.
///
/// The detector's own startup grace is a few seconds, so this only comes into
/// play for panes nothing ever classifies — a hook-driven agent that never
/// reports, say. Without it a pane could report its captured status forever.
const RESTORE_RESOLUTION_DEADLINE_SECS: u64 = 15;

/// How far back a test has to place a restore for its window to be past the
/// deadline already.
#[cfg(test)]
pub(crate) const RESTORE_DEADLINE_PROBE_SECS: u64 = RESTORE_RESOLUTION_DEADLINE_SECS + 1;

/// Wall-clock seconds since the unix epoch, saturating at the epoch itself.
///
/// Agent status ages are reported to clients as absolute unix seconds so a
/// client that connects long after a transition can still date it.
pub fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since_epoch| since_epoch.as_secs())
}

/// What a pane rebuilt from a snapshot knows before anything has classified it.
///
/// The captured pair remains the reported pair until classification or the
/// deadline. Provisional live readings are observed without being published.
/// Exactly one resolution ends the window.
#[derive(Debug, Clone, Copy)]
pub struct RestoreWindow {
    /// What the snapshot recorded, and since when.
    captured: (AgentStatus, u64),
    /// The restart, a floor for the date of a differing classification.
    restored_at: u64,
    /// When the window ends even if nothing has classified the pane.
    ///
    /// Carried in the snapshot rather than minted at each restore, so a
    /// manifest that takes half a minute to reach the new process arrives with
    /// a claim its reader can see has already expired.
    resolve_by: u64,
    /// Whether another known status was observed. Retained in snapshots for
    /// rollback to older processes; unpublished readings no longer re-date a
    /// confirmation of the captured status.
    saw_other_status: bool,
}

impl RestoreWindow {
    fn live_at(&self, now_unix: u64) -> bool {
        now_unix < self.resolve_by
    }

    /// The date this window leaves behind when it closes on `status`.
    ///
    /// Confirming the captured status keeps its date: no other pair was
    /// published inside the window. A differing deadline resolution dates
    /// from the deadline.
    fn closed_changed_at(&self, status: AgentStatus, now_unix: u64) -> u64 {
        if self.inherits(status) {
            self.captured.1
        } else {
            now_unix.min(self.resolve_by).max(self.restored_at)
        }
    }

    /// Whether closing on `status` re-publishes the captured pair rather than
    /// dating a new one.
    fn inherits(&self, status: AgentStatus) -> bool {
        self.captured.0 == status
    }
}

/// A pane's status claim as it travels between processes.
///
/// Everything a later process needs to decide whether the claim may still be
/// honoured travels with it, so no reader has to reconstruct any of it from
/// context it does not have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentStatusClaim {
    /// What the pane was reporting, and since when.
    pub status: AgentStatus,
    pub changed_at: u64,
    /// Past this second the claim is not worth honouring. Absent when the
    /// claim is not waiting on anything, which is the ordinary case for a
    /// session saved while its panes were settled.
    pub resolve_by: Option<u64>,
    /// Whether another known status was observed while this claim was
    /// outstanding. Preserved for readers predating captured-pair reporting.
    pub saw_other_status: bool,
    /// The latest date the pane had published for any status. The process
    /// that adopts the claim dates everything after it, so no pair it
    /// publishes reuses a date a client already holds. Absent in a claim
    /// written before this existed, when `changed_at` is the best floor known.
    pub reported_through: Option<u64>,
}

/// Viewport state for a pane.
///
/// Terminal identity, cwd, labels, and agent metadata live in TerminalState.
pub struct PaneState {
    pub attached_terminal_id: TerminalId,
    /// Whether the user has seen this pane since its last state change to Idle.
    /// False = "Done" (agent finished while user was in another workspace).
    pub seen: bool,
    /// Whether unmodified right-click gestures should be forwarded to the pane application.
    pub right_click_passthrough: bool,
    /// Effective agent status this pane last reported.
    ///
    /// This and `agent_status_changed_at` are written together and only
    /// together, by [`PaneState::set_pair`] on behalf of
    /// [`PaneState::record_agent_status_at`] and nothing else:
    /// the date always dates exactly this status, because the two are reported
    /// to clients as one pair and a client cannot tell a torn pair from a true
    /// one.
    agent_status: AgentStatus,
    /// The last live reading, including unpublished restore placeholders.
    /// Incidental reconciliation settles a window only if this value moves.
    observed_agent_status: AgentStatus,
    /// Unix seconds at which the pane started reporting `agent_status`.
    ///
    /// Clients name a transition by this date, so it is a name before it is a
    /// clock reading: every new pair is dated after everything the pane has
    /// already published ([`PaneState::fresh_date`]), even when two moves fall
    /// inside one second. A burst of moves can run it a few seconds ahead of
    /// the wall clock; the clock takes over again once it passes.
    agent_status_changed_at: u64,
    /// The floor for every fresh date: all dates published by this pane or
    /// its predecessor, and the restore time even though it publishes no pair.
    reported_through: u64,
    /// Present only between a restore and the one resolution that ends it.
    restore_window: Option<RestoreWindow>,
}

impl PaneState {
    pub fn new(attached_terminal_id: TerminalId) -> Self {
        Self::new_at(attached_terminal_id, unix_now_secs())
    }

    /// Same as [`PaneState::new`] with an explicit clock, for tests.
    pub fn new_at(attached_terminal_id: TerminalId, now_unix: u64) -> Self {
        Self {
            attached_terminal_id,
            seen: true,
            right_click_passthrough: false,
            // A pane with no detected agent reports Unknown, and it has
            // reported it since the moment it existed.
            agent_status: AgentStatus::Unknown,
            observed_agent_status: AgentStatus::Unknown,
            agent_status_changed_at: now_unix,
            reported_through: now_unix,
            restore_window: None,
        }
    }

    /// Rebuild a pane from a claim with no history of its own: a fresh
    /// deadline, no interlude recorded.
    #[cfg(test)]
    pub fn restored(
        attached_terminal_id: TerminalId,
        agent_status: AgentStatus,
        agent_status_changed_at: u64,
        now_unix: u64,
    ) -> Self {
        Self::restored_from(
            attached_terminal_id,
            AgentStatusClaim {
                status: agent_status,
                changed_at: agent_status_changed_at,
                resolve_by: None,
                saw_other_status: false,
                reported_through: None,
            },
            now_unix,
        )
    }

    /// Rebuild a pane around a claim carried from another process.
    ///
    /// The claim is judged here, at the moment it is adopted, against the
    /// clock of the process adopting it. A claim whose deadline has passed is
    /// not adopted at all and the pane is built as if the session had said
    /// nothing about it — there is deliberately no way to hand this an
    /// unchecked claim, because a check anywhere else leaves an interval
    /// between the check and the use in which the claim can die.
    ///
    /// A claim that is adopted keeps what it already knew about itself: its
    /// deadline is honoured rather than replaced, and its captured pair is
    /// reported immediately. The live observation starts at Unknown.
    ///
    /// Every later fresh date must exceed everything the claim says was
    /// published, even when the predecessor's clock ran ahead.
    pub fn restored_from(
        attached_terminal_id: TerminalId,
        claim: AgentStatusClaim,
        now_unix: u64,
    ) -> Self {
        let resolve_by = claim
            .resolve_by
            .unwrap_or_else(|| now_unix.saturating_add(RESTORE_RESOLUTION_DEADLINE_SECS));
        let published = claim
            .reported_through
            .unwrap_or(claim.changed_at)
            .max(claim.changed_at);
        if now_unix >= resolve_by {
            return Self::new_at(
                attached_terminal_id,
                now_unix.max(published.saturating_add(1)),
            );
        }
        Self {
            attached_terminal_id,
            seen: true,
            right_click_passthrough: false,
            agent_status: claim.status,
            observed_agent_status: AgentStatus::Unknown,
            agent_status_changed_at: claim.changed_at,
            reported_through: published.max(now_unix),
            restore_window: Some(RestoreWindow {
                captured: (claim.status, claim.changed_at),
                restored_at: now_unix,
                resolve_by,
                saw_other_status: claim.saw_other_status,
            }),
        }
    }

    /// The last recorded reported status, including before deadline resolution.
    /// Event writers capture this before recording the next reading.
    pub fn agent_status(&self) -> AgentStatus {
        self.agent_status
    }

    /// Unix seconds at which this pane's current agent status began.
    #[cfg(test)]
    pub fn agent_status_changed_at(&self) -> u64 {
        self.agent_status_changed_at
    }

    /// Whether this pane is still waiting for something to classify it.
    #[cfg(test)]
    pub fn awaits_agent_status_resolution(&self) -> bool {
        self.restore_window.is_some()
    }

    /// Backdate the pane's status clock so a test can tell a re-date apart
    /// from a stamp that was already close to now.
    #[cfg(test)]
    pub fn backdate_agent_status_for_test(&mut self, at_unix: u64) {
        self.agent_status_changed_at = at_unix;
        self.reported_through = at_unix;
    }

    /// When this pane's restore window ends even if nothing classifies it.
    ///
    /// The event loops schedule against this so a pane nothing ever
    /// classifies still resolves on time, rather than waiting for whatever
    /// request happens to arrive next.
    pub fn agent_status_resolution_due_at(&self) -> Option<u64> {
        self.restore_window.map(|window| window.resolve_by)
    }

    /// Whether this pane is a completion nobody has read, carried over from
    /// another process, whose restore window is still open.
    ///
    /// Holds while the pane is unseen, the session recorded it as done, and
    /// its restore window is live. It ends when a client views the pane, when
    /// a reading that settles the window classifies it, or at the window's
    /// deadline: 15 seconds after the restore, or the deadline the claim
    /// already carried from an earlier restore. Until then, what the new
    /// process learns about the pane is recognition rather than activity:
    /// finding the agent again and the not-yet-classified readings that come
    /// first are not a client looking at the pane, so they must not mark the
    /// completion read.
    pub fn holds_unread_restored_completion(&self) -> bool {
        !self.seen
            && self.restore_window.is_some_and(|window| {
                window.captured.0 == AgentStatus::Done && window.live_at(unix_now_secs())
            })
    }

    /// The API status for a live terminal reading. Desktop views stay live.
    pub fn reported_agent_status(&self, live: AgentStatus) -> AgentStatus {
        self.reported_agent_status_pair_at(live, unix_now_secs()).0
    }

    /// Read the API pair with one clock sample, including at the deadline.
    pub fn reported_agent_status_pair_at(
        &self,
        live: AgentStatus,
        now_unix: u64,
    ) -> (AgentStatus, u64) {
        let changed_at = match self.restore_window {
            Some(window) if window.live_at(now_unix) => return window.captured,
            Some(window) => self.closing_date(window, live, now_unix),
            None if self.agent_status == live => self.agent_status_changed_at,
            None => self.fresh_date(now_unix),
        };
        (live, changed_at)
    }

    /// The date for the reported status corresponding to the original live
    /// reading. Use the pair accessor when publishing both fields together.
    #[cfg(test)]
    pub fn agent_status_changed_at_for(&self, live: AgentStatus) -> u64 {
        self.agent_status_changed_at_for_at(live, unix_now_secs())
    }

    /// Same as [`PaneState::agent_status_changed_at_for`] with an explicit
    /// clock.
    #[cfg(test)]
    pub fn agent_status_changed_at_for_at(&self, live: AgentStatus, now_unix: u64) -> u64 {
        self.reported_agent_status_pair_at(live, now_unix).1
    }

    /// A date for a new pair: `candidate`, unless the pane has already
    /// published that date or a later one, in which case the second after the
    /// latest. Two transitions a client could tell apart never share a date.
    fn fresh_date(&self, candidate: u64) -> u64 {
        candidate.max(self.reported_through.saturating_add(1))
    }

    /// The date closing `window` on `status` leaves behind: the captured date
    /// when the close inherits it (a name clients already hold), otherwise a
    /// fresh one.
    fn closing_date(&self, window: RestoreWindow, status: AgentStatus, now_unix: u64) -> u64 {
        if window.inherits(status) {
            window.closed_changed_at(status, now_unix)
        } else {
            self.fresh_date(window.closed_changed_at(status, now_unix))
        }
    }

    /// Write the reported pair, keeping the high-water mark of what was
    /// published.
    fn set_pair(&mut self, status: AgentStatus, changed_at: u64) {
        self.agent_status = status;
        self.agent_status_changed_at = changed_at;
        self.reported_through = self.reported_through.max(changed_at);
    }

    /// The status and date to write to a snapshot, and the deadline that
    /// travels with them.
    ///
    /// A pane captured before it was classified hands on the pair it came in
    /// with and the deadline that pair already had, so an age survives two
    /// restarts in quick succession without the claim ever getting a fresh
    /// lease from the second one.
    pub fn durable_agent_status(&self) -> AgentStatusClaim {
        self.durable_agent_status_at(unix_now_secs())
    }

    /// Same as [`PaneState::durable_agent_status`] with an explicit clock.
    /// Until resolution, even an expired window keeps its pair and original
    /// deadline. A later adopter rejects it instead of renewing its lifetime.
    pub fn durable_agent_status_at(&self, _now_unix: u64) -> AgentStatusClaim {
        match self.restore_window {
            Some(window) => AgentStatusClaim {
                status: window.captured.0,
                changed_at: window.captured.1,
                resolve_by: Some(window.resolve_by),
                saw_other_status: window.saw_other_status,
                reported_through: Some(self.reported_through),
            },
            None => AgentStatusClaim {
                status: self.agent_status,
                changed_at: self.agent_status_changed_at,
                resolve_by: None,
                saw_other_status: false,
                reported_through: Some(self.reported_through),
            },
        }
    }

    /// Record what this pane's effective status is now.
    ///
    /// Every write of the reported pair goes through here, so the comparison
    /// that decides whether the pane moved happens in one place and no caller
    /// can apply its own rules by writing the fields directly. `reading` says
    /// what the write claims about the pane; movement is decided here, by
    /// comparing the value.
    ///
    /// Returns whether the reported status moved.
    pub fn record_agent_status_at(
        &mut self,
        status: AgentStatus,
        now_unix: u64,
        reading: StatusReading,
    ) -> bool {
        let observed_moved = self.observed_agent_status != status;
        self.observed_agent_status = status;

        if let Some(window) = self.restore_window.as_mut() {
            // Older readers use this bit; keep carrying it for rollback.
            if status != window.captured.0 && status != AgentStatus::Unknown {
                window.saw_other_status = true;
            }
        }

        let settles = match reading {
            // A statement about status settles what a session claimed,
            // whichever way the two compare and whether or not the value moved.
            StatusReading::Verdict => true,
            // Not a statement, but this process watched the pane move.
            StatusReading::Incidental => observed_moved,
            // A placeholder may still be revised, and a title says nothing
            // about status at all.
            StatusReading::Provisional | StatusReading::DisplayOnly => false,
        };
        if settles {
            return self.close_window_on(status, now_unix);
        }

        match self.restore_window {
            Some(window) if window.live_at(now_unix) => false,
            // The deadline is a resolution of its own, dated at the deadline.
            Some(window) => self.close_window_on(status, window.resolve_by),
            None if self.agent_status == status => false,
            None => {
                let changed_at = self.fresh_date(now_unix);
                self.set_pair(status, changed_at);
                true
            }
        }
    }

    /// Classify this pane, ending any restore window.
    ///
    /// Shorthand for a [`StatusReading::Verdict`] write, used where the
    /// classification is the event itself rather than a change to the pane:
    /// a resume finishing, a window running out of time.
    pub fn resolve_agent_status_at(&mut self, status: AgentStatus, now_unix: u64) -> bool {
        self.record_agent_status_at(status, now_unix, StatusReading::Verdict)
    }

    fn close_window_on(&mut self, status: AgentStatus, now_unix: u64) -> bool {
        let moved = self.agent_status != status;
        let changed_at = match self.restore_window.take() {
            Some(window) => self.closing_date(window, status, now_unix),
            None if !moved => self.agent_status_changed_at,
            None => self.fresh_date(now_unix),
        };
        self.set_pair(status, changed_at);
        moved
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESTORED_AT: u64 = 5_000;
    const CAPTURED_AT: u64 = 1_000;
    const IN_WINDOW: u64 = RESTORED_AT + 2;
    const LATER_IN_WINDOW: u64 = RESTORED_AT + 5;

    fn restored_working() -> PaneState {
        PaneState::restored(
            TerminalId::alloc(),
            AgentStatus::Working,
            CAPTURED_AT,
            RESTORED_AT,
        )
    }

    fn observe(pane: &mut PaneState, status: AgentStatus, now: u64) -> bool {
        pane.record_agent_status_at(status, now, StatusReading::Provisional)
    }

    fn incidental(pane: &mut PaneState, status: AgentStatus, now: u64) -> bool {
        pane.record_agent_status_at(status, now, StatusReading::Incidental)
    }

    #[test]
    fn restore_and_incidental_unknown_keep_the_captured_pair() {
        let mut pane = restored_working();
        assert_eq!(pane.agent_status(), AgentStatus::Working);
        assert_eq!(pane.agent_status_changed_at(), CAPTURED_AT);
        assert_eq!(
            pane.agent_status_changed_at_for_at(AgentStatus::Unknown, IN_WINDOW),
            CAPTURED_AT
        );
        assert!(!incidental(&mut pane, AgentStatus::Unknown, IN_WINDOW));
        assert!(pane.awaits_agent_status_resolution());
        assert_eq!(pane.agent_status(), AgentStatus::Working);
        assert_eq!(pane.agent_status_changed_at(), CAPTURED_AT);
    }

    #[test]
    fn a_new_pane_dates_its_status_to_its_own_creation() {
        let pane = PaneState::new_at(TerminalId::alloc(), 1_000);

        assert_eq!(pane.agent_status_changed_at(), 1_000);
    }

    #[test]
    fn the_first_detected_status_counts_as_a_transition() {
        let mut pane = PaneState::new_at(TerminalId::alloc(), 1_000);

        assert!(incidental(&mut pane, AgentStatus::Working, 1_500));
        assert_eq!(pane.agent_status_changed_at(), 1_500);
    }

    #[test]
    fn a_steady_status_keeps_dating_the_transition_that_produced_it() {
        let mut pane = PaneState::new_at(TerminalId::alloc(), 1_000);
        incidental(&mut pane, AgentStatus::Working, 1_500);

        assert!(!incidental(&mut pane, AgentStatus::Working, 9_000));
        assert_eq!(pane.agent_status_changed_at(), 1_500);
    }

    #[test]
    fn every_later_transition_restamps() {
        let mut pane = PaneState::new_at(TerminalId::alloc(), 1_000);
        incidental(&mut pane, AgentStatus::Working, 1_500);

        assert!(incidental(&mut pane, AgentStatus::Done, 2_000));
        assert_eq!(pane.agent_status_changed_at(), 2_000);

        assert!(incidental(&mut pane, AgentStatus::Idle, 2_400));
        assert_eq!(pane.agent_status_changed_at(), 2_400);
    }

    const STATUSES: [AgentStatus; 5] = [
        AgentStatus::Unknown,
        AgentStatus::Idle,
        AgentStatus::Working,
        AgentStatus::Blocked,
        AgentStatus::Done,
    ];

    #[test]
    fn two_moves_inside_one_second_get_two_dates() {
        let mut pane = PaneState::new_at(TerminalId::alloc(), 1_000);
        incidental(&mut pane, AgentStatus::Done, 2_000);

        // An agent that finishes a turn and starts the next inside one second:
        // a client names each transition by its date, so the two must differ,
        // and the later one must be the later name.
        assert!(incidental(&mut pane, AgentStatus::Working, 2_000));
        assert_eq!(pane.agent_status_changed_at(), 2_001);
    }

    #[test]
    fn a_move_never_dates_at_or_before_the_pair_it_replaces() {
        for from in STATUSES {
            for to in STATUSES.into_iter().filter(|to| *to != from) {
                let mut pane = PaneState::new_at(TerminalId::alloc(), 1_000);
                incidental(&mut pane, from, 2_000);
                let before = pane.agent_status_changed_at();

                incidental(&mut pane, to, 2_000);

                assert!(
                    pane.agent_status_changed_at() > before,
                    "{from:?} -> {to:?} inside one second reused or rewound its date"
                );
            }
        }
    }

    #[test]
    fn the_clock_takes_over_once_it_passes_the_floor() {
        let mut pane = PaneState::new_at(TerminalId::alloc(), 1_000);
        incidental(&mut pane, AgentStatus::Done, 2_000);
        incidental(&mut pane, AgentStatus::Working, 2_000);
        incidental(&mut pane, AgentStatus::Idle, 2_000);
        assert_eq!(pane.agent_status_changed_at(), 2_002);

        incidental(&mut pane, AgentStatus::Done, 5_000);
        assert_eq!(pane.agent_status_changed_at(), 5_000);
    }

    #[test]
    fn non_settling_readings_update_only_the_observed_status() {
        let mut pane = restored_working();
        for reading in [StatusReading::Provisional, StatusReading::DisplayOnly] {
            for status in STATUSES {
                assert!(!pane.record_agent_status_at(status, IN_WINDOW, reading));
                assert_eq!(pane.observed_agent_status, status);
                assert_eq!(
                    pane.reported_agent_status_pair_at(status, IN_WINDOW),
                    (AgentStatus::Working, CAPTURED_AT)
                );
                // Global clock sync must not mistake a placeholder for movement.
                assert!(!incidental(&mut pane, status, IN_WINDOW));
                assert!(pane.awaits_agent_status_resolution());
            }
        }
    }

    #[test]
    fn a_predecessors_published_date_floors_the_next_move() {
        let claim = AgentStatusClaim {
            reported_through: Some(RESTORED_AT + 20),
            ..restored_working().durable_agent_status_at(IN_WINDOW)
        };
        let mut pane = PaneState::restored_from(TerminalId::alloc(), claim, RESTORED_AT);
        observe(&mut pane, AgentStatus::Idle, IN_WINDOW);
        assert!(!pane.resolve_agent_status_at(AgentStatus::Working, IN_WINDOW));
        assert_eq!(pane.agent_status_changed_at(), CAPTURED_AT);
        assert!(incidental(&mut pane, AgentStatus::Done, IN_WINDOW));
        assert_eq!(pane.agent_status_changed_at(), RESTORED_AT + 21);
    }

    #[test]
    fn reported_pairs_remain_monotone_through_restore_readings_and_deadline() {
        // Integer wrapping arithmetic gives deterministic, distinct sequences.
        let mut rng = 0x91ac_571du64;
        let mut next = || {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            (rng >> 32) as usize
        };
        let mut matching_deadlines = 0;
        let mut differing_deadlines = 0;
        for captured in STATUSES {
            for trial in 0..256 {
                let claim = AgentStatusClaim {
                    status: captured,
                    changed_at: CAPTURED_AT,
                    resolve_by: None,
                    saw_other_status: trial % 2 == 0,
                    reported_through: Some(RESTORED_AT + (trial % 20)),
                };
                let mut pane = PaneState::restored_from(TerminalId::alloc(), claim, RESTORED_AT);
                let due = pane.agent_status_resolution_due_at().unwrap();
                let mut before = (captured, CAPTURED_AT);
                let mut dates = std::collections::BTreeMap::new();
                dates.insert(before.1, before.0);
                for step in 0..40 {
                    let status = STATUSES[next() % STATUSES.len()];
                    let reading = [
                        StatusReading::Provisional,
                        StatusReading::DisplayOnly,
                        StatusReading::Incidental,
                        StatusReading::Verdict,
                    ][next() % if trial % 2 == 0 { 2 } else { 4 }];
                    let now = RESTORED_AT + step / 4;
                    pane.record_agent_status_at(status, now, reading);
                    let pair = pane.reported_agent_status_pair_at(status, now);
                    assert!(pair.1 >= before.1, "{before:?} -> {pair:?}");
                    if pair != before {
                        assert!(pair.1 > before.1);
                    }
                    if let Some(old) = dates.insert(pair.1, pair.0) {
                        assert_eq!(old, pair.0);
                    }
                    before = pair;
                    if trial % 2 == 0 {
                        assert!(!incidental(&mut pane, status, now));
                        assert!(pane.awaits_agent_status_resolution());
                    }
                }
                let live = pane.observed_agent_status;
                if trial % 2 == 0 {
                    if live == captured {
                        matching_deadlines += 1;
                    } else {
                        differing_deadlines += 1;
                    }
                }
                let at_deadline = pane.reported_agent_status_pair_at(live, due);
                pane.resolve_agent_status_at(live, due);
                assert_eq!(pane.reported_agent_status_pair_at(live, due), at_deadline);
                assert!(at_deadline.1 >= before.1);
                if let Some(old) = dates.insert(at_deadline.1, at_deadline.0) {
                    assert_eq!(old, at_deadline.0);
                }
            }
        }
        assert!(matching_deadlines > 0 && differing_deadlines > 0);
    }

    #[test]
    fn a_deadline_changes_only_a_differing_reported_status() {
        for live in STATUSES {
            let mut pane = restored_working();
            observe(&mut pane, live, IN_WINDOW);
            let due = pane.agent_status_resolution_due_at().unwrap();
            let expected = if live == AgentStatus::Working {
                (live, CAPTURED_AT)
            } else {
                (live, due)
            };
            assert_eq!(
                pane.reported_agent_status_pair_at(live, due - 1),
                (AgentStatus::Working, CAPTURED_AT)
            );
            assert_eq!(pane.reported_agent_status_pair_at(live, due), expected);
            assert_eq!(
                pane.resolve_agent_status_at(live, due),
                live != AgentStatus::Working
            );
            assert_eq!(pane.reported_agent_status_pair_at(live, due), expected);
            assert!(!pane.resolve_agent_status_at(live, due + 1));
        }
    }

    #[test]
    fn a_differing_verdict_exceeds_the_claims_published_floor() {
        let claim = AgentStatusClaim {
            reported_through: Some(RESTORED_AT + 30),
            ..restored_working().durable_agent_status_at(IN_WINDOW)
        };
        let mut pane = PaneState::restored_from(TerminalId::alloc(), claim, RESTORED_AT);
        assert!(pane.resolve_agent_status_at(AgentStatus::Unknown, IN_WINDOW));
        assert_eq!(pane.agent_status_changed_at(), RESTORED_AT + 31);
    }

    #[test]
    fn a_durable_claim_is_unchanged_between_deadline_and_resolution() {
        let mut pane = restored_working();
        observe(&mut pane, AgentStatus::Idle, IN_WINDOW);
        let claim = pane.durable_agent_status_at(IN_WINDOW);
        let due = claim.resolve_by.unwrap();
        assert_eq!(pane.durable_agent_status_at(due + 1), claim);
        let adopted = PaneState::restored_from(TerminalId::alloc(), claim, due + 1);
        assert!(!adopted.awaits_agent_status_resolution());
        assert_eq!(adopted.agent_status(), AgentStatus::Unknown);
        assert!(adopted.agent_status_changed_at() > claim.reported_through.unwrap());
    }

    #[test]
    fn a_claim_carries_the_latest_date_it_reported() {
        let mut pane = PaneState::new_at(TerminalId::alloc(), 1_000);
        incidental(&mut pane, AgentStatus::Done, 2_000);
        incidental(&mut pane, AgentStatus::Working, 2_000);
        assert_eq!(
            pane.durable_agent_status_at(2_000).reported_through,
            Some(2_001)
        );

        // A pane captured inside its window hands on the captured pair, and
        // the reserved date floor travels beside it.
        let mut restored = restored_working();
        observe(&mut restored, AgentStatus::Idle, IN_WINDOW);
        observe(&mut restored, AgentStatus::Working, IN_WINDOW);
        let claim = restored.durable_agent_status_at(IN_WINDOW);
        assert_eq!(
            (claim.status, claim.changed_at, claim.reported_through),
            (AgentStatus::Working, CAPTURED_AT, Some(RESTORED_AT))
        );
    }

    #[test]
    fn a_restore_keeps_the_pair_and_reserves_everything_the_claim_reported() {
        let ran_ahead = AgentStatusClaim {
            status: AgentStatus::Working,
            changed_at: RESTORED_AT - 1,
            resolve_by: None,
            saw_other_status: false,
            reported_through: Some(RESTORED_AT + 3),
        };
        let adopted = PaneState::restored_from(TerminalId::alloc(), ran_ahead, RESTORED_AT);
        assert!(adopted.awaits_agent_status_resolution());
        assert_eq!(adopted.agent_status_changed_at(), RESTORED_AT - 1);
        assert_eq!(adopted.reported_through, RESTORED_AT + 3);

        let expired = AgentStatusClaim {
            resolve_by: Some(RESTORED_AT),
            ..ran_ahead
        };
        let unadopted = PaneState::restored_from(TerminalId::alloc(), expired, RESTORED_AT);
        assert!(!unadopted.awaits_agent_status_resolution());
        assert_eq!(unadopted.agent_status_changed_at(), RESTORED_AT + 4);

        // A claim from a process that predates the floor: its own date is the
        // best floor known; adopting it does not publish a new pair.
        let legacy = AgentStatusClaim {
            changed_at: RESTORED_AT,
            reported_through: None,
            ..ran_ahead
        };
        let rebuilt = PaneState::restored_from(TerminalId::alloc(), legacy, RESTORED_AT);
        assert_eq!(rebuilt.agent_status_changed_at(), RESTORED_AT);
    }

    #[test]
    fn a_read_in_the_window_answers_the_recorded_date() {
        let mut pane = restored_working();
        observe(&mut pane, AgentStatus::Idle, IN_WINDOW);

        assert_eq!(
            pane.agent_status_changed_at_for_at(AgentStatus::Idle, IN_WINDOW),
            pane.agent_status_changed_at()
        );
        // Read-ahead placeholders still report the same captured date.
        let read = pane.agent_status_changed_at_for_at(AgentStatus::Working, IN_WINDOW);
        observe(&mut pane, AgentStatus::Working, IN_WINDOW);
        assert_eq!(read, pane.agent_status_changed_at());
    }

    #[test]
    fn the_status_and_date_accessors_agree_at_restore() {
        let now = unix_now_secs();
        let pane = PaneState::restored(TerminalId::alloc(), AgentStatus::Done, CAPTURED_AT, now);
        assert_eq!(
            pane.reported_agent_status(AgentStatus::Unknown),
            AgentStatus::Done
        );
        assert_eq!(
            pane.agent_status_changed_at_for(AgentStatus::Unknown),
            CAPTURED_AT
        );
    }

    #[test]
    fn an_unrevised_reading_never_ends_the_window() {
        let mut pane = restored_working();

        observe(&mut pane, AgentStatus::Working, IN_WINDOW);
        assert!(pane.awaits_agent_status_resolution());

        observe(&mut pane, AgentStatus::Idle, LATER_IN_WINDOW);
        assert!(pane.awaits_agent_status_resolution());
    }

    #[test]
    fn an_uninterrupted_confirmation_inherits_the_captured_date() {
        let mut pane = restored_working();
        // Unknown is the absence of a reading, not a competing one, so it does
        // not break the continuity the captured date claims.
        observe(&mut pane, AgentStatus::Unknown, IN_WINDOW);

        pane.resolve_agent_status_at(AgentStatus::Working, LATER_IN_WINDOW);

        assert!(!pane.awaits_agent_status_resolution());
        assert_eq!(pane.agent_status_changed_at(), CAPTURED_AT);
    }

    #[test]
    fn a_confirmation_across_a_provisional_interlude_keeps_the_captured_date() {
        let mut pane = restored_working();

        // The provisional interlude was observed, but never published.
        observe(&mut pane, AgentStatus::Idle, IN_WINDOW);
        pane.resolve_agent_status_at(AgentStatus::Working, LATER_IN_WINDOW);

        assert_eq!(
            pane.agent_status_changed_at(),
            CAPTURED_AT,
            "unpublished readings must not re-date the captured status"
        );
    }

    #[test]
    fn a_differing_classification_dates_the_pane_at_the_classification() {
        let mut pane = restored_working();

        pane.resolve_agent_status_at(AgentStatus::Blocked, IN_WINDOW);

        assert!(!pane.awaits_agent_status_resolution());
        assert_eq!(pane.agent_status_changed_at(), IN_WINDOW);
    }

    #[test]
    fn a_classification_that_matches_the_observation_can_move_the_reported_pair() {
        let mut pane = restored_working();
        observe(&mut pane, AgentStatus::Idle, IN_WINDOW);

        assert!(pane.resolve_agent_status_at(AgentStatus::Idle, LATER_IN_WINDOW));
        assert!(!pane.awaits_agent_status_resolution());

        assert!(incidental(&mut pane, AgentStatus::Working, 6_000));
        assert_eq!(
            pane.agent_status_changed_at(),
            6_000,
            "a spent pair cannot be inherited by a later transition"
        );
    }

    #[test]
    fn movement_from_any_source_ends_the_window_at_its_own_moment() {
        let mut pane = restored_working();

        // A seen flip turning Done into Idle is not a statement about the
        // agent, but it is the pane moving and this process watched it.
        assert!(incidental(&mut pane, AgentStatus::Idle, IN_WINDOW));

        assert!(!pane.awaits_agent_status_resolution());
        assert_eq!(pane.agent_status_changed_at(), IN_WINDOW);
    }

    #[test]
    fn a_classification_finding_no_agent_spends_the_pair_too() {
        let mut pane = restored_working();

        assert!(pane.resolve_agent_status_at(AgentStatus::Unknown, IN_WINDOW));

        assert!(!pane.awaits_agent_status_resolution());
        assert!(incidental(&mut pane, AgentStatus::Working, 6_000));
        assert_eq!(pane.agent_status_changed_at(), 6_000);
    }

    #[test]
    fn resolving_twice_settles_nothing_further() {
        let mut pane = restored_working();
        pane.resolve_agent_status_at(AgentStatus::Working, IN_WINDOW);

        assert!(!pane.resolve_agent_status_at(AgentStatus::Working, 7_000));
        assert_eq!(pane.agent_status_changed_at(), CAPTURED_AT);
    }

    #[test]
    fn the_window_cannot_outlive_its_deadline() {
        let mut pane = restored_working();
        let due_at = pane
            .agent_status_resolution_due_at()
            .expect("an unresolved pane has a deadline");

        assert!(!observe(&mut pane, AgentStatus::Working, due_at));

        assert!(!pane.awaits_agent_status_resolution());
        assert_eq!(pane.agent_status_changed_at(), CAPTURED_AT);
        assert!(incidental(&mut pane, AgentStatus::Done, 90_000));
        assert_eq!(pane.agent_status_changed_at(), 90_000);
    }

    #[test]
    fn a_pane_captured_before_its_classification_hands_on_its_claim_and_its_deadline() {
        let mut pane = restored_working();
        observe(&mut pane, AgentStatus::Unknown, IN_WINDOW);
        let due_at = pane
            .agent_status_resolution_due_at()
            .expect("an unresolved pane has a deadline");

        let claim = pane.durable_agent_status_at(IN_WINDOW);
        assert_eq!(claim.status, AgentStatus::Working);
        assert_eq!(claim.changed_at, CAPTURED_AT);
        assert_eq!(claim.resolve_by, Some(due_at));
    }

    #[test]
    fn a_read_after_the_deadline_agrees_with_resolution_but_preserves_the_pending_claim() {
        // A snapshot written while the claim was alive, read after it expired.
        let pane = PaneState::restored_from(
            TerminalId::alloc(),
            AgentStatusClaim {
                status: AgentStatus::Working,
                changed_at: CAPTURED_AT,
                resolve_by: Some(RESTORED_AT + 1),
                saw_other_status: false,
                reported_through: None,
            },
            RESTORED_AT,
        );

        // The reader's own clock decides, so a slow read cannot renew it.
        assert_eq!(
            pane.agent_status_changed_at_for_at(AgentStatus::Working, RESTORED_AT + 30),
            CAPTURED_AT,
            "an expired claim still answers as closing it would"
        );
        assert_eq!(
            pane.durable_agent_status_at(RESTORED_AT + 30).resolve_by,
            Some(RESTORED_AT + 1),
            "an unresolved claim must carry its expired deadline onward"
        );
    }

    #[test]
    fn an_interlude_survives_a_handoff_for_rollback_but_does_not_redate() {
        let mut pane = restored_working();
        // Something else was observed, then the pane was captured and rebuilt
        // in another process before anything classified it.
        observe(&mut pane, AgentStatus::Idle, IN_WINDOW);
        let claim = pane.durable_agent_status_at(LATER_IN_WINDOW);
        assert!(
            claim.saw_other_status,
            "the interlude is part of what the claim asserts"
        );

        // Rebuilt inside the deadline the claim carried, so only the
        // interlude is under test.
        let mut rebuilt = PaneState::restored_from(TerminalId::alloc(), claim, RESTORED_AT + 6);
        rebuilt.resolve_agent_status_at(AgentStatus::Working, RESTORED_AT + 8);

        assert_eq!(
            rebuilt.agent_status_changed_at(),
            CAPTURED_AT,
            "the interlude was never published, including across a handoff"
        );
    }

    #[test]
    fn a_claim_that_dies_between_being_read_and_being_adopted_is_not_adopted() {
        // Alive when the restore started reading the session, expired by the
        // time the pane it belongs to had a terminal to attach to.
        let claim = AgentStatusClaim {
            status: AgentStatus::Idle,
            changed_at: CAPTURED_AT,
            resolve_by: Some(RESTORED_AT),
            saw_other_status: false,
            reported_through: None,
        };

        let mut pane = PaneState::restored_from(TerminalId::alloc(), claim, RESTORED_AT);

        assert!(
            !pane.awaits_agent_status_resolution(),
            "the acceptor judges the claim, so no interval can hide its death"
        );
        // The pre-seed matches what the dead claim recorded, and still cannot
        // reach its date: it is a move from the rebuilt pane's own Unknown, so
        // it takes the second after that.
        pane.resolve_agent_status_at(AgentStatus::Idle, RESTORED_AT);
        assert_eq!(
            pane.agent_status_changed_at(),
            RESTORED_AT + 1,
            "a matching pre-seed must not confirm a claim that was never adopted"
        );
    }

    #[test]
    fn a_claim_alive_at_adoption_is_adopted_whole() {
        let claim = AgentStatusClaim {
            status: AgentStatus::Idle,
            changed_at: CAPTURED_AT,
            resolve_by: Some(RESTORED_AT + 1),
            saw_other_status: false,
            reported_through: None,
        };

        let mut pane = PaneState::restored_from(TerminalId::alloc(), claim, RESTORED_AT);

        assert!(pane.awaits_agent_status_resolution());
        pane.resolve_agent_status_at(AgentStatus::Idle, RESTORED_AT);
        assert_eq!(pane.agent_status_changed_at(), CAPTURED_AT);
    }

    #[test]
    fn an_uninterrupted_claim_still_inherits_after_a_handoff() {
        let mut pane = restored_working();
        observe(&mut pane, AgentStatus::Unknown, IN_WINDOW);
        let claim = pane.durable_agent_status_at(LATER_IN_WINDOW);
        assert!(!claim.saw_other_status);

        let mut rebuilt = PaneState::restored_from(TerminalId::alloc(), claim, RESTORED_AT + 6);
        rebuilt.resolve_agent_status_at(AgentStatus::Working, RESTORED_AT + 8);

        assert_eq!(rebuilt.agent_status_changed_at(), CAPTURED_AT);
    }

    #[test]
    fn a_pane_captured_after_a_differing_classification_hands_on_the_verdict() {
        let mut pane = restored_working();
        pane.resolve_agent_status_at(AgentStatus::Blocked, IN_WINDOW);

        let claim = pane.durable_agent_status_at(LATER_IN_WINDOW);
        assert_eq!(
            (claim.status, claim.changed_at, claim.resolve_by),
            (AgentStatus::Blocked, IN_WINDOW, None),
            "a spent pair must not be written back into a snapshot"
        );
    }
}
