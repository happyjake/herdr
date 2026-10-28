use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::cli::pane::bindings as binding;
use crate::cli::projection::{Field, Input};

use super::agents::AgentSessionInfo;
use super::common::{AgentStatus, PaneAgentState, ReadFormat, ReadSource, SplitDirection};

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum PaneRightClickTarget {
    #[default]
    Herdr,
    Pane,
}

crate::cli::projection::cli_params! {
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
    pub struct PaneSplitParams => PaneSplitProjection {
        /// Local CLI provenance, not part of the advertised client-shell contract.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(skip)]
        pub caller_pane_id: Option<String> => Field::new(None::<String>),
        #[serde(skip)]
        #[schemars(skip)]
        pub peer: Option<crate::api::RequestPeer> => Field::new(None::<crate::api::RequestPeer>),
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub workspace_id: Option<String> => Field::new(None::<String>),
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub target_pane_id: Option<String> => binding::target(true),
        pub direction: SplitDirection => Field::new(SplitDirection::Right).input(binding::split_direction()),
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub ratio: Option<f32> => Field::new(None).input(binding::finite("ratio")),
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub cwd: Option<String> => Field::new(None).input(binding::optional_text("cwd", "PATH").path()),
        #[serde(default)]
        pub focus: bool => Field::new(false).input(binding::boolean("focus", true)).input(binding::boolean("no-focus", false)),
        #[serde(default)]
        pub right_click: PaneRightClickTarget => Field::new(PaneRightClickTarget::Herdr).input(binding::right_click()),
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        pub env: HashMap<String, String> => Field::new(HashMap::new()).input(binding::environment()),
    }
}

crate::cli::projection::cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
    pub struct PaneInputSetParams => PaneInputProjection {
        pub pane_id: String => binding::input_target(),
        pub right_click: PaneRightClickTarget => Field::new(PaneRightClickTarget::Herdr).input(binding::right_click().required()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneLinkActivateParams {
    pub pane_id: String,
    pub viewport_row: u16,
    pub col: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset_from_bottom: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PaneDirection {
    Left,
    Right,
    Up,
    Down,
}

crate::cli::projection::cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
    pub struct PaneSwapParams => PaneSwapProjection {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub pane_id: Option<String> => binding::target(false),
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub direction: Option<PaneDirection> => Field::new(None).input(binding::optional_direction().configure(|arg| arg.conflicts_with_all(["source-pane", "target-pane"]))),
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub source_pane_id: Option<String> => Field::new(None).input(binding::optional_text("source-pane", "ID").configure(|arg| arg.requires("target-pane"))),
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub target_pane_id: Option<String> => Field::new(None).input(binding::optional_text("target-pane", "ID").configure(|arg| arg.requires("source-pane"))),
    }
}

crate::cli::projection::cli_params! {
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
    pub struct PaneMoveParams => PaneMoveProjection {
        pub pane_id: String => Field::new(String::new()).input(binding::pane_id()),
        pub destination: PaneMoveDestination => binding::destination(),
        #[serde(default)]
        pub focus: bool => Field::new(true).input(binding::boolean("focus", true)).input(binding::boolean("no-focus", false)),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PaneMoveDestination {
    Tab {
        tab_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target_pane_id: Option<String>,
        split: SplitDirection,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ratio: Option<f32>,
    },
    NewTab {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    NewWorkspace {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tab_label: Option<String>,
    },
}

crate::cli::projection::cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
    pub struct PaneZoomParams => PaneZoomProjection {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub pane_id: Option<String> => binding::target(true),
        #[serde(default)]
        pub mode: PaneZoomMode => Field::new(PaneZoomMode::Toggle)
            .input(Input::flag("toggle", |slot, _| { *slot = PaneZoomMode::Toggle; Ok(()) }).configure(|arg| arg.conflicts_with_all(["on", "off"])))
            .input(Input::flag("on", |slot, _| { *slot = PaneZoomMode::On; Ok(()) }).configure(|arg| arg.conflicts_with("off")))
            .input(Input::flag("off", |slot, _| { *slot = PaneZoomMode::Off; Ok(()) })),
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum PaneZoomMode {
    #[default]
    Toggle,
    On,
    Off,
}

crate::cli::projection::cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
    pub struct PaneLayoutParams => PaneLayoutProjection {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub pane_id: Option<String> => binding::target(false),
    }
}

crate::cli::projection::cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
    pub struct PaneProcessInfoParams => PaneProcessInfoProjection {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub pane_id: Option<String> => binding::target(false),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct LayoutExportParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LayoutApplyParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_label: Option<String>,
    #[serde(default)]
    pub focus: bool,
    pub root: LayoutNode,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LayoutSetSplitRatioParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    pub path: Vec<bool>,
    pub ratio: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LayoutDescription {
    pub workspace_id: String,
    pub tab_id: String,
    pub zoomed: bool,
    pub focused_pane_id: String,
    pub root: LayoutNode,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LayoutNode {
    Pane {
        #[serde(flatten)]
        pane: LayoutPane,
    },
    Split {
        direction: SplitDirection,
        ratio: f32,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct LayoutPane {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
}

crate::cli::projection::cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
    pub struct PaneNeighborParams => PaneNeighborProjection {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub pane_id: Option<String> => binding::target(false),
        pub direction: PaneDirection => Field::new(PaneDirection::Left).input(binding::direction()),
    }
}

crate::cli::projection::cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
    pub struct PaneEdgesParams => PaneEdgesProjection {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub pane_id: Option<String> => binding::target(false),
    }
}

crate::cli::projection::cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
    pub struct PaneFocusDirectionParams => PaneFocusProjection {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub pane_id: Option<String> => binding::target(false),
        pub direction: PaneDirection => Field::new(PaneDirection::Left).input(binding::direction()),
    }
}

crate::cli::projection::cli_params! {
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
    pub struct PaneResizeParams => PaneResizeProjection {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub pane_id: Option<String> => binding::target(false),
        pub direction: PaneDirection => Field::new(PaneDirection::Left).input(binding::direction()),
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub amount: Option<f32> => Field::new(None).input(binding::finite("amount")),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneEditScrollbackParams {
    pub pane_id: String,
    /// 1-based line of the scrollback dump to open the editor at, passed to
    /// the editor as the vi-family `+N` argument. Absent opens the dump the
    /// way the editor opens any file. A line past the end of the dump opens
    /// at its last line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneScrollParams {
    pub pane_id: String,
    pub offset_from_bottom: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneTextPoint {
    pub row: u32,
    pub col: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneSelectionReadParams {
    pub pane_id: String,
    pub anchor: PaneTextPoint,
    pub cursor: PaneTextPoint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_revision: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PaneCopyMotion {
    LineEnd,
    FirstNonBlank,
    NextWordStart,
    PreviousWordStart,
    NextWordEnd,
    NextBigWordStart,
    PreviousBigWordStart,
    NextBigWordEnd,
    PreviousParagraph,
    NextParagraph,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneCopyMotionParams {
    pub pane_id: String,
    pub cursor: PaneTextPoint,
    pub motion: PaneCopyMotion,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_revision: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PaneCopySearchDirection {
    Forward,
    Backward,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneTextRange {
    pub start: PaneTextPoint,
    pub end: PaneTextPoint,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneCopySearchParams {
    pub pane_id: String,
    pub query: String,
    pub direction: PaneCopySearchDirection,
    pub cursor: PaneTextPoint,
    pub content_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<PaneTextRange>,
}

crate::cli::projection::cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
    pub struct PaneListParams => PaneListProjection {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub workspace_id: Option<String> => Field::new(None).input(Input::value("workspace", "WORKSPACE_ID", |slot, value| { *slot = Some(crate::cli::normalize_workspace_id(value)); Ok(()) })),
    }
}

crate::cli::projection::cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
    pub struct PaneCurrentParams => PaneCurrentProjection {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub caller_pane_id: Option<String> => binding::target(false),
    }
}

crate::cli::projection::cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
    pub struct PaneRenameParams => PaneRenameProjection {
        pub pane_id: String => Field::new(String::new()).input(binding::pane_id()),
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub label: Option<String> => binding::label(),
        /// Who this name is from, when it is not from a person.
        ///
        /// Absent is the ordinary rename — someone typing a name, at the desk
        /// or anywhere else — and it clears any source the label was standing
        /// with. A blank or absent `label` clears the label, the source and the
        /// time together. Additive: a server that predates the field ignores
        /// it, keeps no source, and every name there reads as a person's.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub source: Option<LabelSource> => Field::new(None::<LabelSource>),
    }
}

/// Who wrote a pane's label, when it was not a person.
///
/// Kept beside the label rather than derived, and never shown: it is there
/// so a client that names panes by itself can tell its own names from the
/// ones people and skills gave, and leave those alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LabelSource {
    /// A client's own automatic namer wrote it.
    Editor,
}

crate::cli::projection::cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
    pub struct PaneSetPinnedParams => PaneSetPinnedProjection {
        pub pane_id: String => Field::new(String::new()).input(binding::pane_id()),
        pub pinned: bool => Field::new(false),
    }
}

/// A user-kept to-do. Dates are whole unix seconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneFlag {
    pub note: Option<String>,
    pub due_at: Option<u64>,
    pub set_at: u64,
    pub announced_at: Option<u64>,
}

crate::cli::projection::cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
    pub struct PaneSetFlagParams => PaneSetFlagProjection {
        pub pane_id: String => Field::new(String::new()).input(binding::pane_id()),
        #[serde(default)]
        pub note: Option<String> => Field::new(None).input(binding::optional_text("note", "TEXT").once()),
        #[serde(default)]
        pub due_at: Option<u64> => Field::new(None).input(binding::flag_due()),
    }
}

crate::cli::projection::cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
    pub struct PaneSendTextParams => PaneSendTextProjection {
        pub pane_id: String => Field::new(String::new()).input(binding::pane_id()),
        pub text: String => Field::new(String::new()).input(binding::joined("text", "TEXT")),
    }
}

crate::cli::projection::cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
    pub struct PaneSendKeysParams => PaneSendKeysProjection {
        pub pane_id: String => Field::new(String::new()).input(binding::pane_id()),
        pub keys: Vec<String> => Field::new(Vec::new()).input(binding::keys()),
    }
}

crate::cli::projection::cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
    pub struct PaneSendInputParams => PaneRunProjection {
        pub pane_id: String => Field::new(String::new()).input(binding::pane_id()),
        #[serde(default, skip_serializing_if = "String::is_empty")]
        pub text: String => Field::new(String::new()).input(binding::joined("command", "COMMAND")),
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub keys: Vec<String> => Field::new(vec![String::from("Enter")]),
        /// Client-generated identity of this logical send. When present, an id
        /// the server already applied is acknowledged without writing again —
        /// the re-affirmation contract behind the `send_affirm` capability.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub send_id: Option<String> => Field::new(None::<String>),
    }
}

/// Mouse input addressed in zero-based terminal cells. Coordinates outside
/// the live pane grid are clamped to its last row or column before encoding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneSendMouseParams {
    pub pane_id: String,
    pub action: PaneMouseAction,
    pub row: u32,
    pub col: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PaneMouseAction {
    Click,
    ScrollUp,
    ScrollDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PaneMouseRouting {
    MouseReport,
    AlternateScroll,
    HostScroll,
}

crate::cli::projection::cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
    pub struct PaneReadParams => PaneReadProjection {
        pub pane_id: String => Field::new(String::new()).input(binding::pane_id()),
        pub source: ReadSource => Field::new(ReadSource::Recent).input(binding::read_source(true)),
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub lines: Option<u32> => Field::new(None).input(binding::lines()),
        #[serde(default)]
        pub format: ReadFormat => binding::read_format(),
        #[serde(default = "super::default_true")]
        pub strip_ansi: bool => Field::new(true).input(binding::boolean("raw", false)),
        #[serde(skip)]
        #[schemars(skip)]
        pub(crate) intent: super::common::ReadIntent => Field::new(super::common::ReadIntent::Interactive),
        #[serde(skip)]
        #[schemars(skip)]
        pub(crate) span: Option<super::common::ReadSpan> => Field::new(None::<super::common::ReadSpan>),
        /// Shift the recent-source window up from the bottom anchor: physical
        /// rows for `recent`, logical lines for `recent_unwrapped`. Inside a
        /// logical line taller than the window, one step degrades to one
        /// window of physical rows so every prefix row stays reachable; page
        /// by the echoed `effective_offset` rather than assuming fixed units.
        /// Panes whose only history lives in the Windows console fallback echo
        /// `has_more: false` at offset zero and reject nonzero offsets.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub offset_from_bottom: Option<u64> => Field::new(None::<u64>),
    }
}

crate::cli::projection::cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
    pub struct PaneReportAgentParams => PaneReportAgentProjection {
        pub pane_id: String => Field::new(String::new()).input(binding::pane_id()),
        pub source: String => Field::new(String::new()).input(binding::source()),
        pub agent: String => Field::new(String::new()).input(binding::text("agent", "LABEL").required()),
        pub state: PaneAgentState => Field::new(PaneAgentState::Unknown).input(binding::state()),
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub message: Option<String> => Field::new(None).input(binding::optional_text("message", "TEXT")),
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub seq: Option<u64> => Field::new(None).input(binding::number("seq", "N")),
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub agent_session_id: Option<String> => Field::new(None).input(binding::optional_text("agent-session-id", "ID")),
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub agent_session_path: Option<String> => Field::new(None).input(binding::optional_text("agent-session-path", "PATH").path()),
        /// Command that resumes this agent's session after a Herdr restart. The
        /// first element must be a plain command name.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub resume_argv: Option<Vec<String>> => Field::new(None).input(binding::resume()),
        /// With `state: idle`, the user cancelled the run rather than it
        /// finishing: the pane goes idle with no completion, toast, or sound.
        /// Ignored with any other state. Honored only by servers that declare
        /// `agent_report_cancelled`.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        pub cancelled: bool => Field::new(false),
        /// The local-socket sender, filled in by the server; never read from
        /// or written to the wire.
        #[serde(skip)]
        #[schemars(skip)]
        pub peer: Option<crate::api::RequestPeer> => Field::new(None::<crate::api::RequestPeer>),
    }
}

crate::cli::projection::cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
    pub struct PaneReportAgentSessionParams => PaneReportSessionProjection {
        pub pane_id: String => Field::new(String::new()).input(binding::pane_id()),
        pub source: String => Field::new(String::new()).input(binding::source()),
        pub agent: String => Field::new(String::new()).input(binding::text("agent", "LABEL").required()),
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub seq: Option<u64> => Field::new(None).input(binding::number("seq", "N")),
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub agent_session_id: Option<String> => Field::new(None).input(binding::optional_text("agent-session-id", "ID")),
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub agent_session_path: Option<String> => Field::new(None).input(binding::optional_text("agent-session-path", "PATH").path()),
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub session_start_source: Option<String> => Field::new(None).input(binding::optional_text("session-start-source", "SOURCE")),
        /// Command that resumes this agent's session after a Herdr restart. The
        /// first element must be a plain command name.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub resume_argv: Option<Vec<String>> => Field::new(None).input(binding::resume()),
        /// The local-socket sender, filled in by the server; never read from
        /// or written to the wire.
        #[serde(skip)]
        #[schemars(skip)]
        pub peer: Option<crate::api::RequestPeer> => Field::new(None::<crate::api::RequestPeer>),
    }
}

crate::cli::projection::cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
    pub struct PaneReportMetadataParams => PaneMetadataProjection {
        pub pane_id: String => Field::new(String::new()).input(binding::pane_id()),
        pub source: String => Field::new(String::new()).input(binding::source()),
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub agent: Option<String> => Field::new(None).input(binding::optional_text("agent", "LABEL")),
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub applies_to_source: Option<String> => Field::new(None).input(binding::optional_text("applies-to-source", "ID")),
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub title: Option<String> => Field::new(None).input(binding::optional_text("title", "TEXT").configure(|arg| arg.conflicts_with("clear-title"))),
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub display_agent: Option<String> => Field::new(None).input(binding::optional_text("display-agent", "TEXT").configure(|arg| arg.conflicts_with("clear-display-agent"))),
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        pub state_labels: HashMap<String, String> => Field::new(HashMap::new()).input(binding::state_label().configure(|arg| arg.conflicts_with("clear-state-labels"))),
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        #[schemars(schema_with = "super::common::metadata_token_patch_schema")]
        pub tokens: HashMap<String, Option<String>> => Field::new(HashMap::new()).input(binding::token()).input(binding::clear_token()),
        #[serde(default)]
        pub clear_title: bool => Field::new(false).input(binding::boolean("clear-title", true)),
        #[serde(default)]
        pub clear_display_agent: bool => Field::new(false).input(binding::boolean("clear-display-agent", true)),
        #[serde(default)]
        pub clear_state_labels: bool => Field::new(false).input(binding::boolean("clear-state-labels", true)),
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub seq: Option<u64> => Field::new(None).input(binding::number("seq", "N")),
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(range(min = 1, max = 86_400_000))]
        pub ttl_ms: Option<u64> => Field::new(None).input(binding::number("ttl-ms", "N")),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneClearAgentAuthorityParams {
    pub pane_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
}

crate::cli::projection::cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
    pub struct PaneReleaseAgentParams => PaneReleaseProjection {
        pub pane_id: String => Field::new(String::new()).input(binding::pane_id()),
        pub source: String => Field::new(String::new()).input(binding::source()),
        pub agent: String => Field::new(String::new()).input(binding::text("agent", "LABEL").required()),
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub seq: Option<u64> => Field::new(None).input(binding::number("seq", "N")),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneInfo {
    /// Always serialized: the last agent's kind after exit, or null until an agent ends.
    #[serde(default)]
    pub ended_agent: Option<String>,
    /// Always serialized: null means no starter; absence means an older server.
    #[serde(default)]
    pub started_by: Option<PaneStartedBy>,
    pub pane_id: String,
    pub terminal_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub focused: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground_cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_title_stripped: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_agent: Option<String>,
    pub agent_status: AgentStatus,
    /// The current idle completed work. Always serialized; null means no completion.
    #[serde(default)]
    pub completion_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub state_labels: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[schemars(schema_with = "super::common::metadata_token_values_schema")]
    pub tokens: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session: Option<AgentSessionInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scroll: Option<PaneScrollInfo>,
    /// Whether the pane's live terminal state currently reports mouse input.
    /// Additive: payloads from older servers omit this and deserialize false.
    #[serde(default)]
    pub mouse_tracking: bool,
    /// Whether the pane's live terminal state currently uses the alternate screen.
    /// Additive: payloads from older servers omit this and deserialize false.
    #[serde(default)]
    pub alternate_screen: bool,
    /// Unix seconds at which `agent_status` last changed for this pane.
    ///
    /// This server always sends it, so a client that has just connected can
    /// date a status it never watched change. Additive and optional rather
    /// than a plain integer: payloads from older servers omit it, and absent
    /// has to stay distinguishable from a real timestamp so a client renders
    /// no age instead of an age measured from the unix epoch.
    ///
    /// This always dates exactly the `agent_status` in the same payload, never
    /// some other status the pane held earlier.
    ///
    /// A pane's dates also name its transitions: every change of the reported
    /// status is dated later than anything the pane reported before, even when
    /// two changes fall inside one second, so a client can tell two
    /// transitions apart by their dates alone. A burst of changes can run the
    /// date a few seconds ahead of the wall clock until the clock passes it.
    /// The guarantee crosses a live handoff between servers that keep it; a
    /// handoff back to a server that predates it may reuse a date inside that
    /// run-ahead.
    ///
    /// The captured status and date remain the reported pair during a restore
    /// window, until classification or its 15-second deadline. Provisional
    /// readings and display-only changes do not publish another pair. A
    /// classification matching the captured status keeps its date, including
    /// after provisional readings of another status; this is not a transition.
    /// A differing classification publishes one new pair with a date past the
    /// published floor. A differing deadline resolution uses the deadline or
    /// the next date past that floor, whichever is later.
    ///
    /// An unresolved legacy window may already have published newer
    /// placeholders. Importing it rebases the captured pair once past that
    /// published floor. A marked window from this server preserves its pair
    /// across further handoffs; a settled legacy claim preserves its current
    /// reported pair. Desktop TUI snapshots independently use live detection.
    ///
    /// Every change of the reported status restamps this, including a Done to
    /// Idle change caused by the pane being seen at the desk rather than by
    /// the agent. A client that folds read-driven transitions without ageing
    /// its own rows should expect an age seeded from here to be at most as old
    /// as the last desk read of that pane, not to match its own stamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_status_changed_at: Option<u64>,
    /// Whether the user has pinned this pane, as `pane.set_pinned` and
    /// `herdr pane pin` set it.
    ///
    /// Always serialized, unlike the optional presentation fields: a client
    /// reads the absence of this key as a server that predates the pin and
    /// offers no pinning at all, so `false` has to stay distinguishable from
    /// "this server has no such notion".
    ///
    /// The pin is the user's own mark. It is theirs to set: read it freely,
    /// and write it only when the user asks for it.
    #[serde(default)]
    pub pinned: bool,
    /// Always serialized: null is unflagged; an absent key means no flag capability.
    #[serde(default)]
    pub flag: Option<PaneFlag>,
    /// Who wrote the standing `label`, when it was not a person.
    ///
    /// Always serialized, like `pinned` and for the same reason: a client
    /// reads the absence of this key as a server that keeps no provenance
    /// at all, so a null — a name a person or a skill gave — has to stay
    /// distinguishable from "this server has no such notion".
    #[serde(default)]
    pub label_source: Option<LabelSource>,
    /// Unix seconds at which the standing `label` was written.
    ///
    /// Always serialized, for the same reason. Null when no label stands,
    /// and on a label restored from a session captured before the write
    /// time was kept.
    #[serde(default)]
    pub label_at: Option<u64>,
    pub revision: u64,
}

/// The public pane that started this terminal, or a retained closed starter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum PaneStartedBy {
    Pane { pane_id: String },
    Closed { closed: Box<ClosedPaneStarter> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ClosedPaneStarter {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_by: Option<PaneStartedBy>,
}

/// What a pane's session record says was asked of it.
///
/// Read from the harness's own file of the conversation, never from the
/// screen: the person's words, or a supervising agent's, and never the
/// agent's tool traffic, its results, or what the harness injected on the
/// person's behalf. Every field is always serialized, so a client reads a
/// null as a real absence rather than as a server that has nothing to say.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PromptTrail {
    /// The harness whose record this was read from.
    pub agent: PromptTrailAgent,
    /// The first real prompt of the conversation, cut to about four hundred
    /// code points. Null when the record holds no prompt at all.
    pub first: Option<TrailPrompt>,
    /// The last three real prompts, oldest first, each cut to about two
    /// hundred and forty code points. The first prompt appears here too
    /// while it is still among the last three.
    pub recent: Vec<TrailPrompt>,
    /// The title the harness wrote for this conversation, when it wrote one
    /// worth reading. Null for a harness that writes none, and for a
    /// fallback title that says nothing the prompts do not.
    pub title: Option<String>,
    /// Real prompts in the record.
    pub count: u32,
    /// Unix seconds at which the newest real prompt arrived, as the harness
    /// dated it. Null when the record dates nothing.
    pub newest_at: Option<u64>,
}

/// One prompt of a trail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TrailPrompt {
    /// The prompt as it was asked, whitespace collapsed and cut to the
    /// length its place in the trail allows.
    pub text: String,
    /// Unix seconds at which the harness recorded it, when it recorded one.
    pub at: Option<u64>,
}

/// A harness whose session record a trail can be read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PromptTrailAgent {
    Claude,
    Codex,
    Pi,
    // Read for turn indexes only; agy panes have no prompt trail.
    Agy,
}

/// Why a pane has no trail. Never an error: a pane with no trail is an
/// ordinary pane, and a client reads it from the screen instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PromptTrailReason {
    /// The pane reports no agent session — a plain shell, or a pane whose
    /// harness never reported one.
    NoSession,
    /// The pane reports a session for a harness whose records this server
    /// does not read.
    UnsupportedAgent,
    /// The record the session names is not on this desk.
    NoRecord,
    /// The record is there but could not be read within its bounds.
    Unreadable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneScrollInfo {
    pub offset_from_bottom: u64,
    pub max_offset_from_bottom: u64,
    pub viewport_rows: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneProcessInfo {
    pub pane_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground_process_group_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tty: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub foreground_processes: Vec<PaneProcessInfoProcess>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneProcessInfoProcess {
    pub pid: u32,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argv0: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argv: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cmdline: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneSwapResult {
    pub changed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<PaneSwapReason>,
    pub source_pane_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_pane_id: Option<String>,
    pub focused_pane_id: String,
    pub layout: PaneLayoutSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PaneSwapReason {
    NoNeighbor,
    SamePane,
    NotFound,
    CrossTab,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneMoveResult {
    pub changed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<PaneMoveReason>,
    pub previous_pane_id: String,
    pub previous_workspace_id: String,
    pub previous_tab_id: String,
    pub pane: Box<PaneInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_layout: Option<Box<PaneLayoutSnapshot>>,
    pub target_layout: Box<PaneLayoutSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_workspace: Option<super::WorkspaceInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_tab: Option<super::TabInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_tab_id: Option<String>,
    pub focused_pane_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PaneMoveReason {
    SameTab,
    ZoomedTab,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneZoomResult {
    pub changed: bool,
    pub zoom_changed: bool,
    pub focus_changed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<PaneZoomReason>,
    pub pane_id: String,
    pub focused_pane_id: String,
    pub zoomed: bool,
    pub layout: PaneLayoutSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PaneZoomReason {
    SinglePane,
    AlreadyZoomed,
    AlreadyUnzoomed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneLayoutSnapshot {
    pub workspace_id: String,
    pub tab_id: String,
    pub zoomed: bool,
    pub area: PaneLayoutRect,
    pub focused_pane_id: String,
    pub panes: Vec<PaneLayoutPane>,
    pub splits: Vec<PaneLayoutSplit>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneLayoutRect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneLayoutPane {
    pub pane_id: String,
    pub focused: bool,
    pub rect: PaneLayoutRect,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneLayoutSplit {
    pub id: String,
    pub direction: SplitDirection,
    pub ratio: f32,
    pub rect: PaneLayoutRect,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneNeighborResult {
    pub pane_id: String,
    pub direction: PaneDirection,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub neighbor_pane_id: Option<String>,
    pub layout: PaneLayoutSnapshot,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneEdgesResult {
    pub pane_id: String,
    pub left: bool,
    pub right: bool,
    pub up: bool,
    pub down: bool,
    pub layout: PaneLayoutSnapshot,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneFocusDirectionResult {
    pub changed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<PaneFocusDirectionReason>,
    pub source_pane_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused_pane_id: Option<String>,
    pub layout: PaneLayoutSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PaneFocusDirectionReason {
    NoNeighbor,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneResizeResult {
    pub changed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<PaneResizeReason>,
    pub pane_id: String,
    pub focused_pane_id: String,
    pub layout: PaneLayoutSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PaneResizeReason {
    Unchanged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneReadResult {
    pub pane_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    /// Current terminal width in columns. Older servers omit this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cols: Option<u16>,
    pub source: ReadSource,
    pub format: ReadFormat,
    pub text: String,
    pub revision: u64,
    pub truncated: bool,
    /// Echoed on offset reads: the requested offset_from_bottom clamped at
    /// the top of scrollback. Absent when the request carried no offset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_offset: Option<u64>,
    /// Echoed on offset reads: whether content remains above the returned
    /// window. Absent when the request carried no offset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_more: Option<bool>,
}

/// Inclusive display-cell columns on a pane's current viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneLinkRegion {
    pub row: u16,
    pub start_col: u16,
    pub end_col: u16,
}

/// Recent turns in a session record: a pi record's last-recorded branch,
/// or an agy conversation transcript. All nullable fields are serialized
/// so absence is distinct from an unsupported capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TurnIndex {
    /// The harness that wrote the record, such as `pi` or `agy`.
    pub agent: String,
    pub session_id: String,
    /// The last recorded entry: null in a record with no entries.
    pub leaf_id: Option<String>,
    /// Opaque record revision, for equality comparisons only.
    pub revision: String,
    pub partial: bool,
    /// At most the last 64 turns, oldest first.
    pub turns: Vec<IndexedTurn>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IndexedTurn {
    pub id: String,
    /// Start time in unix seconds.
    pub at: u64,
    pub start: TurnStart,
    pub prompt: Option<String>,
    pub end: TurnEnd,
    pub answer: Option<String>,
    pub answer_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TurnStart {
    Prompt,
    Wake,
    /// A background task finishing. The pi reader never emits it.
    Task,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TurnEnd {
    Answered,
    Aborted,
    Error,
    Open,
}

/// `pane.preface` parameters: where the client's oldest held screen line is,
/// and how many prompted turns it has room for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PanePrefaceParams {
    pub pane_id: String,
    /// `offset_from_bottom` of the oldest screen line the client holds,
    /// counted as `recent_unwrapped` offset pages count; null when the
    /// client holds the screen's top.
    pub top_offset: Option<u64>,
    /// The first lines the client holds, oldest first, beginning with the
    /// line at `top_offset`: 1 to 64 of them. The server finds the whole
    /// run near `top_offset` in case the tail has grown since, and requires
    /// it there exactly once. Null exactly when `top_offset` is.
    pub top_lines: Option<Vec<String>>,
    /// Prompted turns the client has room for, at most 200. Wake and task
    /// turns inside the kept span ride along under a separate server cap.
    pub max_turns: u32,
}

/// The turns of a pane's session record older than the oldest prompt the
/// client holds on screen, each a prompt and its final answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Preface {
    pub agent: String,
    pub session_id: String,
    /// Opaque record revision, for equality comparisons only.
    pub revision: String,
    /// Oldest first.
    pub turns: Vec<PrefaceTurn>,
    /// Turns older than the seam that were not sent.
    pub omitted: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PrefaceTurn {
    pub id: String,
    /// Start time in unix seconds.
    pub at: u64,
    pub start: TurnStart,
    /// The whole prompt up to the server's byte cap; null for wake and
    /// task turns.
    pub prompt: Option<String>,
    pub end: TurnEnd,
    /// The whole final answer up to the server's byte cap; null unless
    /// the turn was answered.
    pub answer: Option<String>,
    /// Present and true when the byte cap cut the prompt.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub prompt_cut: bool,
    /// Present and true when the byte cap cut the answer.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub answer_cut: bool,
}

/// Why a pane has no preface: the prompt trail's reasons, plus the two a
/// seam adds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PrefaceReason {
    NoSession,
    UnsupportedAgent,
    NoRecord,
    Unreadable,
    /// The client's oldest held line, or any record prompt below it, could
    /// not be found on the screen.
    SeamNotFound,
    /// The screen already reaches the record's first turn.
    NothingAbove,
}

impl From<PromptTrailReason> for PrefaceReason {
    fn from(reason: PromptTrailReason) -> Self {
        match reason {
            PromptTrailReason::NoSession => Self::NoSession,
            PromptTrailReason::UnsupportedAgent => Self::UnsupportedAgent,
            PromptTrailReason::NoRecord => Self::NoRecord,
            PromptTrailReason::Unreadable => Self::Unreadable,
        }
    }
}
