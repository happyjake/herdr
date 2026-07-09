#[cfg(unix)]
use serde::{Deserialize, Serialize};

/// Long-lived pane runtime transferred during server replacement.
///
/// Handoff preserves server-owned session state such as PTYs, processes, agent
/// identity, and durable plugin/session metadata. It intentionally does not
/// preserve transient coordination such as in-flight requests, waits,
/// subscriptions, client sockets, or pane-to-pane messages; clients reconnect
/// and retry those operations after replacement.
#[cfg(unix)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct HandoffRuntimeState {
    pub pane_id: u32,
    pub child_pid: u32,
    pub rows: u16,
    pub cols: u16,
    pub cell_width_px: u32,
    pub cell_height_px: u32,
    #[serde(default)]
    pub keyboard_protocol_flags: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyboard_protocol_ansi: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_state: Option<crate::pane::InputState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_history_ansi: Option<String>,
}

#[cfg(unix)]
impl HandoffRuntimeState {
    pub fn with_pane_id(mut self, pane_id: crate::layout::PaneId) -> Self {
        self.pane_id = pane_id.raw();
        self
    }
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HandoffScreenRestore {
    Primary,
    Alternate,
}

#[cfg(unix)]
impl HandoffScreenRestore {
    pub(crate) fn is_alternate(self) -> bool {
        matches!(self, Self::Alternate)
    }
}

#[cfg(unix)]
pub(crate) fn handoff_screen_restore(
    input_state: Option<&crate::pane::InputState>,
    terminal: Option<&crate::terminal::TerminalState>,
) -> HandoffScreenRestore {
    if input_state.is_some_and(|input_state| input_state.alternate_screen)
        || terminal.is_some_and(|terminal| terminal.needs_handoff_alternate_screen_recovery())
        || input_state.is_some_and(|input_state| {
            !input_state.alternate_screen && input_state.indicates_fullscreen_application()
        })
    {
        HandoffScreenRestore::Alternate
    } else {
        HandoffScreenRestore::Primary
    }
}

#[derive(Debug)]
pub(crate) struct ImportedHandoffRuntime {
    #[cfg(unix)]
    pub master_fd: std::os::fd::RawFd,
    #[cfg(unix)]
    pub state: HandoffRuntimeState,
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn input_state(alternate_screen: bool, application_cursor: bool) -> crate::pane::InputState {
        crate::pane::InputState {
            alternate_screen,
            application_cursor,
            bracketed_paste: false,
            focus_reporting: false,
            mouse_protocol_mode: crate::input::MouseProtocolMode::None,
            mouse_protocol_encoding: crate::input::MouseProtocolEncoding::Default,
            mouse_alternate_scroll: false,
            modify_other_keys: false,
        }
    }

    #[test]
    fn restore_alternate_screen_decision_matches_known_recovery_signals() {
        let plain = input_state(false, false);
        assert_eq!(
            handoff_screen_restore(Some(&plain), None),
            HandoffScreenRestore::Primary
        );

        let active_alt = input_state(true, false);
        assert_eq!(
            handoff_screen_restore(Some(&active_alt), None),
            HandoffScreenRestore::Alternate
        );

        let fullscreen_modes = input_state(false, true);
        assert_eq!(
            handoff_screen_restore(Some(&fullscreen_modes), None),
            HandoffScreenRestore::Alternate
        );

        let mut agent_terminal = crate::terminal::TerminalState::new(
            crate::terminal::TerminalId::alloc(),
            "/tmp".into(),
        );
        agent_terminal.set_detected_state(
            Some(crate::detect::Agent::Pi),
            crate::detect::AgentState::Working,
        );
        assert_eq!(
            handoff_screen_restore(Some(&plain), Some(&agent_terminal)),
            HandoffScreenRestore::Alternate
        );
    }

    #[test]
    fn alternate_restore_reports_alternate() {
        assert!(HandoffScreenRestore::Alternate.is_alternate());
        assert!(!HandoffScreenRestore::Primary.is_alternate());
    }
}
