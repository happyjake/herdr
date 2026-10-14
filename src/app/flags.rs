use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::App;

fn unix_elapsed() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}

impl App {
    pub(crate) fn sync_flag_deadline(&mut self) {
        self.sync_flag_deadline_at(Instant::now(), unix_elapsed());
    }

    fn sync_flag_deadline_at(&mut self, now: Instant, wall_time: Duration) {
        self.flag_deadline = self
            .state
            .terminals
            .values()
            .filter_map(|terminal| terminal.flag.as_ref())
            .filter(|flag| flag.announced_at.is_none())
            .filter_map(|flag| flag.due_at)
            .min()
            .and_then(|due| now.checked_add(Duration::from_secs(due).saturating_sub(wall_time)));
    }

    pub(crate) fn announce_due_flags(&mut self, now: Instant) {
        if self.flag_deadline.is_some_and(|deadline| now >= deadline) {
            self.announce_due_flags_at(now, unix_elapsed());
        }
    }

    pub(super) fn announce_due_flags_at(&mut self, now: Instant, wall_time: Duration) {
        if self.flag_deadline.is_none_or(|deadline| now < deadline) {
            return;
        }
        let now_unix = wall_time.as_secs();
        let mut due = Vec::new();
        for (ws_idx, workspace) in self.state.workspaces.iter().enumerate() {
            for tab in &workspace.tabs {
                for (pane_id, pane) in &tab.panes {
                    let Some(flag) = self
                        .state
                        .terminals
                        .get(&pane.attached_terminal_id)
                        .and_then(|terminal| terminal.flag.as_ref())
                    else {
                        continue;
                    };
                    if let Some(due_at) = flag
                        .due_at
                        .filter(|due_at| flag.announced_at.is_none() && *due_at <= now_unix)
                    {
                        due.push((due_at, ws_idx, *pane_id, pane.attached_terminal_id.clone()));
                    }
                }
            }
        }
        due.sort_by_key(|(due_at, ..)| *due_at);
        let mut changed = false;
        for (_, ws_idx, pane_id, terminal_id) in due {
            if self
                .state
                .terminals
                .get_mut(&terminal_id)
                .is_some_and(|terminal| terminal.announce_flag_at(now_unix))
            {
                self.emit_pane_kept_presentation_changed(ws_idx, pane_id);
                changed = true;
            }
        }
        self.sync_flag_deadline_at(now, wall_time);
        if changed {
            self.state.mark_session_dirty();
            // The event loop may sleep after this tick; arm persistence now.
            self.sync_session_save_schedule();
        }
    }
}
