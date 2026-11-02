use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const PRESERVED_INPUT_FOREGROUND_GRACE: Duration = Duration::from_secs(10);

/// Wall-clock input/command activity, shared with the PTY writer and process observer.
#[derive(Debug)]
pub(crate) struct InputActivity {
    at: AtomicU64,
    foreground: AtomicU32,
    suppress_foreground_until: Mutex<Option<Instant>>,
}

impl Default for InputActivity {
    fn default() -> Self {
        Self {
            at: AtomicU64::new(crate::pane::unix_now_secs()),
            foreground: AtomicU32::new(0),
            suppress_foreground_until: Mutex::new(None),
        }
    }
}

impl InputActivity {
    pub(crate) fn get(&self) -> u64 {
        self.at.load(Ordering::Acquire)
    }

    pub(crate) fn restore(&self, at: u64) {
        self.at.store(at, Ordering::Release);
    }

    pub(crate) fn record(&self) {
        let mut suppression = self
            .suppress_foreground_until
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        *suppression = None;
        self.at
            .fetch_max(crate::pane::unix_now_secs(), Ordering::AcqRel);
    }

    pub(crate) fn preserve(&self) {
        self.preserve_at(Instant::now());
    }

    pub(crate) fn input_delivered(&self, record_activity: bool) {
        if record_activity {
            self.record();
        } else {
            self.preserve();
        }
    }

    fn preserve_at(&self, now: Instant) {
        let mut suppression = self
            .suppress_foreground_until
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        *suppression = Some(now + PRESERVED_INPUT_FOREGROUND_GRACE);
    }

    #[cfg(test)]
    pub(crate) fn observe_foreground(&self, pgid: Option<u32>) {
        self.observe_foreground_at(pgid, Instant::now());
    }

    pub(crate) fn observe_foreground_at(&self, pgid: Option<u32>, now: Instant) {
        let Some(pgid) = pgid.filter(|pgid| *pgid != 0) else {
            return;
        };
        let previous = self.foreground.swap(pgid, Ordering::AcqRel);
        // Initial discovery (including after restore) is a baseline, not a command.
        if previous != 0 && previous != pgid {
            let mut suppression = self
                .suppress_foreground_until
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            if suppression.is_some_and(|until| now < until) {
                return;
            }
            *suppression = None;
            self.at
                .fetch_max(crate::pane::unix_now_secs(), Ordering::AcqRel);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserved_input_suppresses_foreground_changes_until_the_deadline() {
        let activity = InputActivity::default();
        let now = Instant::now();
        activity.restore(100);
        activity.observe_foreground_at(Some(42), now);
        activity.preserve_at(now);
        activity.observe_foreground_at(Some(43), now + Duration::from_secs(1));
        activity.observe_foreground_at(Some(42), now + Duration::from_secs(9));
        assert_eq!(activity.get(), 100);
        activity.observe_foreground_at(Some(44), now + Duration::from_secs(10));
        assert!(activity.get() > 100);
    }

    #[test]
    fn real_input_ends_foreground_suppression_immediately() {
        let activity = InputActivity::default();
        let now = Instant::now();
        activity.observe_foreground_at(Some(42), now);
        activity.preserve_at(now);
        activity.record();
        activity.restore(100);
        activity.observe_foreground_at(Some(43), now + Duration::from_secs(1));
        assert!(activity.get() > 100);
    }

    #[test]
    fn input_activity_foreground_baseline_and_missing_observations_keep_the_date() {
        let activity = InputActivity::default();
        activity.restore(100);
        activity.observe_foreground(Some(42));
        activity.observe_foreground(None);
        activity.observe_foreground(Some(42));
        assert_eq!(activity.get(), 100);
        activity.observe_foreground(Some(43));
        assert!(activity.get() > 100);
        activity.restore(200);
        activity.observe_foreground(Some(43));
        assert_eq!(activity.get(), 200);
    }

    #[test]
    fn input_activity_never_runs_ahead_for_bursts_or_backwards_for_clock_skew() {
        let activity = InputActivity::default();
        activity.restore(u64::MAX - 1);
        activity.record();
        activity.record();
        assert_eq!(activity.get(), u64::MAX - 1);
    }
}
