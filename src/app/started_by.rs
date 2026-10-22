//! Keeps started-by links true while their starters move, close, or a
//! started pane resumes an earlier conversation.

use crate::api::schema::{ClosedPaneStarter, PaneStartedBy};
use crate::layout::PaneId;
use crate::terminal::TerminalId;

use super::state::AppState;

/// A pane about to close, captured while it still has a public id and labels.
pub(crate) struct ClosingStarter {
    pane_id: String,
    label: String,
    terminal_id: TerminalId,
}

impl AppState {
    /// Captures what a closed marker needs from `pane_ids` before they close.
    pub(crate) fn closing_starters(
        &self,
        ws_idx: usize,
        pane_ids: impl IntoIterator<Item = PaneId>,
    ) -> Vec<ClosingStarter> {
        if !self
            .terminals
            .values()
            .any(|terminal| terminal.started_by.is_some())
        {
            return Vec::new();
        }
        let Some(ws) = self.workspaces.get(ws_idx) else {
            return Vec::new();
        };
        pane_ids
            .into_iter()
            .filter_map(|pane_id| {
                let number = ws.public_pane_number(pane_id)?;
                let terminal_id = ws.pane_state(pane_id)?.attached_terminal_id.clone();
                let label = self
                    .terminals
                    .get(&terminal_id)
                    .and_then(|terminal| terminal.manual_label.clone())
                    .or_else(|| ws.tab_display_name(ws.find_tab_index_for_pane(pane_id)?))?;
                Some(ClosingStarter {
                    pane_id: crate::workspace::public_pane_id_for_number(&ws.id, number),
                    label,
                    terminal_id,
                })
            })
            .collect()
    }

    /// Replaces every reference to the closing panes with a closed marker.
    /// Each marker reads its starter's own link when applied, so panes that
    /// close together still leave an unbroken chain.
    pub(crate) fn retire_starters(&mut self, closing: Vec<ClosingStarter>) {
        for starter in closing {
            let started_by = self
                .terminals
                .get(&starter.terminal_id)
                .and_then(|terminal| terminal.started_by.clone());
            let marker = PaneStartedBy::Closed {
                closed: Box::new(ClosedPaneStarter {
                    label: starter.label,
                    started_by,
                }),
            };
            self.rewrite_started_by(&starter.pane_id, &marker);
        }
    }

    pub(crate) fn retire_starter(&mut self, ws_idx: usize, pane_id: PaneId) {
        let closing = self.closing_starters(ws_idx, [pane_id]);
        self.retire_starters(closing);
    }

    /// Points every reference to a moved starter at its new public id.
    pub(crate) fn retarget_starter(&mut self, from: &str, to: String) {
        if from != to {
            self.rewrite_started_by(from, &PaneStartedBy::Pane { pane_id: to });
        }
    }

    /// Only a terminal's first session report decides: a started pane whose
    /// agent opens by resuming an earlier conversation begins its own family.
    pub(crate) fn note_session_report(&mut self, pane_id: PaneId, session_start_source: &str) {
        let Some(terminal_id) = self
            .workspaces
            .iter()
            .find_map(|ws| ws.pane_state(pane_id))
            .map(|pane| pane.attached_terminal_id.clone())
        else {
            return;
        };
        let Some(terminal) = self.terminals.get_mut(&terminal_id) else {
            return;
        };
        if std::mem::replace(&mut terminal.session_reported, true) {
            return;
        }
        if session_start_source == "resume" && terminal.started_by.take().is_some() {
            self.pending_started_by_updates.push(terminal_id);
        }
        self.mark_session_dirty();
    }

    fn rewrite_started_by(&mut self, from: &str, to: &PaneStartedBy) {
        let mut changed = false;
        for (terminal_id, terminal) in &mut self.terminals {
            if terminal
                .started_by
                .as_mut()
                .is_some_and(|link| replace_reference(link, from, to))
            {
                self.pending_started_by_updates.push(terminal_id.clone());
                changed = true;
            }
        }
        if changed {
            self.mark_session_dirty();
        }
    }
}

fn replace_reference(link: &mut PaneStartedBy, from: &str, to: &PaneStartedBy) -> bool {
    match link {
        PaneStartedBy::Pane { pane_id } if pane_id == from => {
            *link = to.clone();
            true
        }
        PaneStartedBy::Pane { .. } => false,
        PaneStartedBy::Closed { closed } => closed
            .started_by
            .as_mut()
            .is_some_and(|inner| replace_reference(inner, from, to)),
    }
}
