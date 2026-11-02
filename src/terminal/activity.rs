use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Wall-clock input/command activity, shared with the PTY writer and process observer.
#[derive(Debug)]
pub(crate) struct InputActivity {
    at: AtomicU64,
    foreground: AtomicU32,
}

impl Default for InputActivity {
    fn default() -> Self {
        Self {
            at: AtomicU64::new(crate::pane::unix_now_secs()),
            foreground: AtomicU32::new(0),
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
        self.at
            .fetch_max(crate::pane::unix_now_secs(), Ordering::AcqRel);
    }

    pub(crate) fn observe_foreground(&self, pgid: Option<u32>) {
        let Some(pgid) = pgid.filter(|pgid| *pgid != 0) else {
            return;
        };
        let previous = self.foreground.swap(pgid, Ordering::AcqRel);
        // Initial discovery (including after restore) is a baseline, not a command.
        if previous != 0 && previous != pgid {
            self.record();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
