use crate::api::schema::AgentStatus;
use crate::events::StatusReading;
use crate::terminal::TerminalId;

/// How long a rebuilt pane may wait for something to classify it.
///
/// The detector's own startup grace is a few seconds, so this only comes into
/// play for panes nothing ever classifies — a hook-driven agent that never
/// reports, say. Without it a pane could sit unresolved forever and its age
/// would be pinned to the restore for the rest of the session.
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
/// A restored pane reports whatever its terminal says, but nothing it says in
/// this window is a verdict on what the pane was doing before the restart, so
/// the whole window is reported as dating from the restart and the captured
/// pair waits to one side. Exactly one resolution ends the window.
#[derive(Debug, Clone, Copy)]
pub struct RestoreWindow {
    /// What the snapshot recorded, and since when.
    captured: (AgentStatus, u64),
    /// The restart. Every status reported inside the window dates from here.
    restored_at: u64,
    /// When the window ends even if nothing has classified the pane.
    ///
    /// Carried in the snapshot rather than minted at each restore, so a
    /// manifest that takes half a minute to reach the new process arrives with
    /// a claim its reader can see has already expired.
    resolve_by: u64,
    /// Whether this pane has reported a status other than the captured one
    /// since the restart.
    ///
    /// Inheriting the captured date claims the pane has been doing one thing
    /// without interruption since the session recorded it. That claim is only
    /// safe if nothing else was reported in between, whatever reported it.
    /// Unknown does not count: it says the server cannot see what the pane is
    /// doing, which contradicts nothing.
    saw_other_status: bool,
}

impl RestoreWindow {
    fn live_at(&self, now_unix: u64) -> bool {
        now_unix < self.resolve_by
    }

    /// The date this window leaves behind when it closes on `status`.
    ///
    /// Confirming what the session recorded, with nothing else reported in
    /// between, means the pane has been doing that since the session said so.
    /// Anything else is this process's own reading, and a window closed by its
    /// own deadline dates from the deadline.
    fn closed_changed_at(&self, status: AgentStatus, now_unix: u64) -> u64 {
        if self.captured.0 == status && !self.saw_other_status {
            self.captured.1
        } else {
            now_unix.min(self.resolve_by).max(self.restored_at)
        }
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
    /// Whether the pane reported some other known status while this claim was
    /// outstanding. A claim that saw an interlude before a handoff is still a
    /// claim that saw one after it, so this travels rather than resetting.
    pub saw_other_status: bool,
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
    /// together, by [`PaneState::record_agent_status_at`] and nothing else:
    /// the date always dates exactly this status, because the two are reported
    /// to clients as one pair and a client cannot tell a torn pair from a true
    /// one.
    agent_status: AgentStatus,
    /// Unix seconds at which the pane started reporting `agent_status`.
    agent_status_changed_at: u64,
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
            agent_status_changed_at: now_unix,
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
    /// deadline is honoured rather than replaced, and an interlude it saw
    /// before the handoff still counts against it after.
    pub fn restored_from(
        attached_terminal_id: TerminalId,
        claim: AgentStatusClaim,
        now_unix: u64,
    ) -> Self {
        let resolve_by = claim
            .resolve_by
            .unwrap_or_else(|| now_unix.saturating_add(RESTORE_RESOLUTION_DEADLINE_SECS));
        if now_unix >= resolve_by {
            return Self::new_at(attached_terminal_id, now_unix);
        }
        Self {
            attached_terminal_id,
            seen: true,
            right_click_passthrough: false,
            agent_status: AgentStatus::Unknown,
            agent_status_changed_at: now_unix,
            restore_window: Some(RestoreWindow {
                captured: (claim.status, claim.changed_at),
                restored_at: now_unix,
                resolve_by,
                saw_other_status: claim.saw_other_status,
            }),
        }
    }

    /// The effective agent status this pane last reported.
    #[cfg(test)]
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
    }

    /// When this pane's restore window ends even if nothing classifies it.
    ///
    /// The event loops schedule against this so a pane nothing ever
    /// classifies still resolves on time, rather than waiting for whatever
    /// request happens to arrive next.
    pub fn agent_status_resolution_due_at(&self) -> Option<u64> {
        self.restore_window.map(|window| window.resolve_by)
    }

    /// The date to report alongside `status`.
    ///
    /// Callers report the status derived live from the terminal, so this
    /// answers for that status and not for whatever was recorded earlier. The
    /// two are read together so the pair a client receives can never date one
    /// status with another one's time.
    pub fn agent_status_changed_at_for(&self, status: AgentStatus) -> u64 {
        self.agent_status_changed_at_for_at(status, unix_now_secs())
    }

    /// Same as [`PaneState::agent_status_changed_at_for`] with an explicit
    /// clock.
    pub fn agent_status_changed_at_for_at(&self, status: AgentStatus, now_unix: u64) -> u64 {
        match self.restore_window {
            // Nothing inside a live window is older than the restart, whatever
            // shape the pane has: a bare shell, a placeholder Idle from the
            // detector, or an Idle pre-seeded for an agent about to resume.
            Some(window) if window.live_at(now_unix) => window.restored_at,
            // Past its deadline the window is over whether or not anything has
            // got around to closing it, so answer exactly as closing it would.
            Some(window) => window.closed_changed_at(status, now_unix),
            None if self.agent_status == status => self.agent_status_changed_at,
            None => now_unix,
        }
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
    pub fn durable_agent_status_at(&self, now_unix: u64) -> AgentStatusClaim {
        match self.restore_window {
            Some(window) if window.live_at(now_unix) => AgentStatusClaim {
                status: window.captured.0,
                changed_at: window.captured.1,
                resolve_by: Some(window.resolve_by),
                saw_other_status: window.saw_other_status,
            },
            // A dead window must not be written into a snapshot as if it were
            // alive. The reader checks the deadline too, so this is the
            // writer agreeing with what the reader would decide anyway.
            Some(window) => AgentStatusClaim {
                status: self.agent_status,
                changed_at: window.closed_changed_at(self.agent_status, now_unix),
                resolve_by: None,
                saw_other_status: false,
            },
            None => AgentStatusClaim {
                status: self.agent_status,
                changed_at: self.agent_status_changed_at,
                resolve_by: None,
                saw_other_status: false,
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
        let moved = self.agent_status != status;

        if let Some(window) = self.restore_window.as_mut() {
            // Anything reported that is not what the session recorded breaks
            // the continuity inheriting its date would claim. Unknown is not a
            // competing claim, so it does not break anything.
            if status != window.captured.0 && status != AgentStatus::Unknown {
                window.saw_other_status = true;
            }
        }

        let settles = match reading {
            // A statement about status settles what a session claimed,
            // whichever way the two compare and whether or not the value moved.
            StatusReading::Verdict => true,
            // Not a statement, but this process watched the pane move.
            StatusReading::Incidental => moved,
            // A placeholder may still be revised, and a title says nothing
            // about status at all.
            StatusReading::Provisional | StatusReading::DisplayOnly => false,
        };
        if settles {
            return self.close_window_on(status, now_unix, moved);
        }

        match self.restore_window {
            Some(window) if window.live_at(now_unix) => {
                self.agent_status = status;
                self.agent_status_changed_at = window.restored_at;
                moved
            }
            // The deadline is a resolution of its own, dated at the deadline.
            Some(window) => self.close_window_on(status, window.resolve_by, moved),
            None if !moved => false,
            None => {
                self.agent_status = status;
                self.agent_status_changed_at = now_unix;
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

    fn close_window_on(&mut self, status: AgentStatus, now_unix: u64, moved: bool) -> bool {
        self.agent_status = status;
        self.agent_status_changed_at = match self.restore_window.take() {
            Some(window) => window.closed_changed_at(status, now_unix),
            None if !moved => self.agent_status_changed_at,
            None => now_unix,
        };
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

    #[test]
    fn everything_reported_inside_the_window_dates_from_the_restart() {
        let mut pane = restored_working();

        assert_eq!(pane.agent_status(), AgentStatus::Unknown);
        assert_eq!(pane.agent_status_changed_at(), RESTORED_AT);

        // A placeholder Idle from the detector, or an Idle pre-seeded for an
        // agent about to resume, reads the same way: since the restart.
        observe(&mut pane, AgentStatus::Idle, IN_WINDOW);
        assert_eq!(pane.agent_status(), AgentStatus::Idle);
        assert_eq!(pane.agent_status_changed_at(), RESTORED_AT);
        assert_eq!(
            pane.agent_status_changed_at_for_at(AgentStatus::Idle, IN_WINDOW),
            RESTORED_AT
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
    fn a_confirmation_across_an_interlude_stamps_fresh() {
        let mut pane = restored_working();

        // Something else was reported in between, so claiming the pane has
        // been working since the snapshot would span an interlude a client
        // could have seen.
        observe(&mut pane, AgentStatus::Idle, IN_WINDOW);
        pane.resolve_agent_status_at(AgentStatus::Working, LATER_IN_WINDOW);

        assert_eq!(
            pane.agent_status_changed_at(),
            LATER_IN_WINDOW,
            "an interrupted window cannot claim continuity"
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
    fn a_classification_that_moves_nothing_still_spends_the_captured_pair() {
        let mut pane = restored_working();
        observe(&mut pane, AgentStatus::Idle, IN_WINDOW);

        assert!(!pane.resolve_agent_status_at(AgentStatus::Idle, LATER_IN_WINDOW));
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

        assert!(!pane.resolve_agent_status_at(AgentStatus::Unknown, IN_WINDOW));

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

        assert!(observe(&mut pane, AgentStatus::Working, due_at));

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
    fn a_claim_read_after_its_deadline_is_dead_on_arrival() {
        // A snapshot written while the claim was alive, read after it expired.
        let pane = PaneState::restored_from(
            TerminalId::alloc(),
            AgentStatusClaim {
                status: AgentStatus::Working,
                changed_at: CAPTURED_AT,
                resolve_by: Some(RESTORED_AT + 1),
                saw_other_status: false,
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
            None,
            "an expired claim carries no deadline onward"
        );
    }

    #[test]
    fn an_interlude_seen_before_a_handoff_still_counts_after_it() {
        let mut pane = restored_working();
        // Something else was reported, then the pane was captured and rebuilt
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
            RESTORED_AT + 8,
            "a claim that saw an interlude cannot inherit after a handoff either"
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
        };

        let mut pane = PaneState::restored_from(TerminalId::alloc(), claim, RESTORED_AT);

        assert!(
            !pane.awaits_agent_status_resolution(),
            "the acceptor judges the claim, so no interval can hide its death"
        );
        // The pre-seed matches what the dead claim recorded, and still cannot
        // reach its date.
        pane.resolve_agent_status_at(AgentStatus::Idle, RESTORED_AT);
        assert_eq!(
            pane.agent_status_changed_at(),
            RESTORED_AT,
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
