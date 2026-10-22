//! Keeps started-by links true while their starters move, close, or a
//! started pane resumes an earlier conversation.
//!
//! A link is a chain: zero or more closed markers ending in at most one live
//! pane reference. Every holder also keeps the label its live starter had when
//! the link was made, which becomes that starter's closed-marker label.

use crate::api::schema::{ClosedPaneStarter, PaneStartedBy};
use crate::layout::PaneId;
use crate::terminal::{TerminalId, TerminalState};

use super::state::AppState;

/// A verified starter link with its starter's label at start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StarterLink {
    pub(crate) started_by: PaneStartedBy,
    pub(crate) label: Option<String>,
}

impl StarterLink {
    pub(crate) fn attach(link: Option<Self>, terminal: &mut TerminalState) {
        let (started_by, label) =
            link.map_or((None, None), |link| (Some(link.started_by), link.label));
        terminal.started_by = started_by;
        terminal.starter_label = label;
    }
}

/// A pane about to close, captured while it still has a public id and labels.
pub(crate) struct ClosingStarter {
    pane_id: String,
    label: Option<String>,
    terminal_id: TerminalId,
}

impl AppState {
    /// A pane's label for a link made now: its pane label, else its tab label.
    pub(crate) fn starter_label(&self, ws_idx: usize, pane_id: PaneId) -> Option<String> {
        let ws = self.workspaces.get(ws_idx)?;
        let terminal_id = &ws.pane_state(pane_id)?.attached_terminal_id;
        self.terminals
            .get(terminal_id)
            .and_then(|terminal| terminal.manual_label.clone())
            .or_else(|| ws.tab_display_name(ws.find_tab_index_for_pane(pane_id)?))
    }

    /// Captures what a closed marker needs from `pane_ids` before they close.
    pub(crate) fn closing_starters(
        &self,
        ws_idx: usize,
        pane_ids: impl IntoIterator<Item = PaneId>,
    ) -> Vec<ClosingStarter> {
        if !self.any_started_by_links() {
            return Vec::new();
        }
        let Some(ws) = self.workspaces.get(ws_idx) else {
            return Vec::new();
        };
        pane_ids
            .into_iter()
            .filter_map(|pane_id| {
                let number = ws.public_pane_number(pane_id)?;
                Some(ClosingStarter {
                    pane_id: crate::workspace::public_pane_id_for_number(&ws.id, number),
                    // Only for links made before labels were kept with them.
                    label: self.starter_label(ws_idx, pane_id),
                    terminal_id: ws.pane_state(pane_id)?.attached_terminal_id.clone(),
                })
            })
            .collect()
    }

    /// Replaces every live reference to the closing panes with a closed
    /// marker. Each marker reads its starter's own link when applied, so panes
    /// that close together still leave an unbroken chain.
    pub(crate) fn retire_starters(&mut self, closing: Vec<ClosingStarter>) {
        for starter in closing {
            let (started_by, starter_label) = self
                .terminals
                .get(&starter.terminal_id)
                .map(|terminal| (terminal.started_by.clone(), terminal.starter_label.clone()))
                .unwrap_or_default();
            self.rewrite_live_references(&starter.pane_id, |reference, label| {
                *reference = PaneStartedBy::Closed {
                    closed: Box::new(ClosedPaneStarter {
                        label: label
                            .take()
                            .or_else(|| starter.label.clone())
                            .unwrap_or_default(),
                        started_by: started_by.clone(),
                    }),
                };
                // The holder's live starter is now the closed one's starter.
                *label = starter_label.clone();
            });
        }
    }

    pub(crate) fn retire_starter(&mut self, ws_idx: usize, pane_id: PaneId) {
        let closing = self.closing_starters(ws_idx, [pane_id]);
        self.retire_starters(closing);
    }

    /// Points every live reference to a moved starter at its new public id.
    pub(crate) fn retarget_starter(&mut self, from: &str, to: String) {
        if from != to {
            self.rewrite_live_references(from, |reference, _| {
                *reference = PaneStartedBy::Pane {
                    pane_id: to.clone(),
                };
            });
        }
    }

    pub(crate) fn terminal_for_pane(&self, pane_id: PaneId) -> Option<&TerminalState> {
        let pane = self
            .workspaces
            .iter()
            .find_map(|ws| ws.pane_state(pane_id))?;
        self.terminals.get(&pane.attached_terminal_id)
    }

    /// Only a terminal's first applied session report decides: a started pane
    /// whose agent opens by resuming an earlier conversation begins its own
    /// family.
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
            terminal.starter_label = None;
            self.pending_started_by_updates.push(terminal_id);
        }
        self.mark_session_dirty();
    }

    fn any_started_by_links(&self) -> bool {
        !self.pending_starter_links.is_empty()
            || self
                .terminals
                .values()
                .any(|terminal| terminal.started_by.is_some())
    }

    /// Applies `rewrite` to every link, held by a terminal or by a create
    /// still in flight, whose live reference is `pane_id`.
    fn rewrite_live_references(
        &mut self,
        pane_id: &str,
        mut rewrite: impl FnMut(&mut PaneStartedBy, &mut Option<String>),
    ) {
        let mut changed = false;
        for (terminal_id, terminal) in &mut self.terminals {
            if let Some(reference) = terminal
                .started_by
                .as_mut()
                .and_then(|link| live_reference_to(link, pane_id))
            {
                rewrite(reference, &mut terminal.starter_label);
                self.pending_started_by_updates.push(terminal_id.clone());
                changed = true;
            }
        }
        for link in self.pending_starter_links.values_mut() {
            if let Some(reference) = live_reference_to(&mut link.started_by, pane_id) {
                rewrite(reference, &mut link.label);
            }
        }
        if changed {
            self.mark_session_dirty();
        }
    }
}

fn live_reference_to<'a>(
    link: &'a mut PaneStartedBy,
    pane_id: &str,
) -> Option<&'a mut PaneStartedBy> {
    if matches!(link, PaneStartedBy::Pane { pane_id: id } if id == pane_id) {
        return Some(link);
    }
    match link {
        PaneStartedBy::Pane { .. } => None,
        PaneStartedBy::Closed { closed } => closed
            .started_by
            .as_mut()
            .and_then(|inner| live_reference_to(inner, pane_id)),
    }
}
