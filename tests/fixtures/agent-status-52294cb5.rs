// Frozen deployed recorder from 52294cb5: src/pane/state.rs, before mod tests.
// Exercises upgrade and rollback against the actual old transition rules.
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
        if self.inherits(status) {
            self.captured.1
        } else {
            now_unix.min(self.resolve_by).max(self.restored_at)
        }
    }

    /// Whether closing on `status` re-publishes the captured pair rather than
    /// dating a new one.
    fn inherits(&self, status: AgentStatus) -> bool {
        self.captured.0 == status && !self.saw_other_status
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
    /// Unix seconds at which the pane started reporting `agent_status`.
    ///
    /// Clients name a transition by this date, so it is a name before it is a
    /// clock reading: every new pair is dated after everything the pane has
    /// already published ([`PaneState::fresh_date`]), even when two moves fall
    /// inside one second. A burst of moves can run it a few seconds ahead of
    /// the wall clock; the clock takes over again once it passes.
    agent_status_changed_at: u64,
    /// The latest date this pane has published for any status. Usually the
    /// current date, but an inheritance can re-publish an older captured date
    /// after the window already published newer ones.
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
    /// deadline is honoured rather than replaced, and an interlude it saw
    /// before the handoff still counts against it after.
    ///
    /// Either way the rebuilt pane dates its first pair after everything the
    /// claim says was published, so a handoff inside the second of a
    /// transition still gives clients a new name for the restart.
    pub fn restored_from(
        attached_terminal_id: TerminalId,
        claim: AgentStatusClaim,
        now_unix: u64,
    ) -> Self {
        let resolve_by = claim
            .resolve_by
            .unwrap_or_else(|| now_unix.saturating_add(RESTORE_RESOLUTION_DEADLINE_SECS));
        let published = claim.reported_through.unwrap_or(claim.changed_at);
        let restored_at = now_unix.max(published.saturating_add(1));
        if now_unix >= resolve_by {
            return Self::new_at(attached_terminal_id, restored_at);
        }
        Self {
            attached_terminal_id,
            seen: true,
            right_click_passthrough: false,
            agent_status: AgentStatus::Unknown,
            agent_status_changed_at: restored_at,
            reported_through: restored_at,
            restore_window: Some(RestoreWindow {
                captured: (claim.status, claim.changed_at),
                restored_at,
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
        // A read answers what recording `status` would store, so a client never
        // holds a date for the pair that differs from the one it is given next.
        match self.restore_window {
            // Nothing inside a live window is older than the restart, whatever
            // shape the pane has: a bare shell, a placeholder Idle from the
            // detector, or an Idle pre-seeded for an agent about to resume.
            Some(window) if window.live_at(now_unix) => {
                if self.agent_status == status {
                    self.agent_status_changed_at
                } else {
                    self.fresh_date(window.restored_at)
                }
            }
            // Past its deadline the window is over whether or not anything has
            // got around to closing it, so answer exactly as closing it would.
            Some(window) => self.closing_date(window, status, now_unix),
            None if self.agent_status == status => self.agent_status_changed_at,
            None => self.fresh_date(now_unix),
        }
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
    pub fn durable_agent_status_at(&self, now_unix: u64) -> AgentStatusClaim {
        match self.restore_window {
            Some(window) if window.live_at(now_unix) => AgentStatusClaim {
                status: window.captured.0,
                changed_at: window.captured.1,
                resolve_by: Some(window.resolve_by),
                saw_other_status: window.saw_other_status,
                reported_through: Some(self.reported_through),
            },
            // A dead window must not be written into a snapshot as if it were
            // alive. The reader checks the deadline too, so this is the
            // writer agreeing with what the reader would decide anyway.
            Some(window) => {
                let changed_at = self.closing_date(window, self.agent_status, now_unix);
                AgentStatusClaim {
                    status: self.agent_status,
                    changed_at,
                    resolve_by: None,
                    saw_other_status: false,
                    reported_through: Some(self.reported_through.max(changed_at)),
                }
            }
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
                if moved {
                    let changed_at = self.fresh_date(window.restored_at);
                    self.set_pair(status, changed_at);
                }
                moved
            }
            // The deadline is a resolution of its own, dated at the deadline.
            Some(window) => self.close_window_on(status, window.resolve_by, moved),
            None if !moved => false,
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

    fn close_window_on(&mut self, status: AgentStatus, now_unix: u64, moved: bool) -> bool {
        let changed_at = match self.restore_window.take() {
            Some(window) => self.closing_date(window, status, now_unix),
            None if !moved => self.agent_status_changed_at,
            None => self.fresh_date(now_unix),
        };
        self.set_pair(status, changed_at);
        moved
    }
}
