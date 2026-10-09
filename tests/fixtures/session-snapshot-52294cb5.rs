// Frozen serde models from 52294cb5: src/persist/snapshot.rs.
// Keep old field declarations intact to exercise reading a new snapshot.
use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};


/// Current snapshot format version.
pub(super) const SNAPSHOT_VERSION: u32 = 3;

/// Serializable snapshot of the entire herdr session.
#[derive(Serialize, Deserialize)]
pub struct SessionSnapshot {
    /// Format version — used to detect incompatible changes.
    #[serde(default)]
    pub version: u32,
    pub workspaces: Vec<WorkspaceSnapshot>,
    pub active: Option<usize>,
    pub selected: usize,
    #[serde(default)]
    pub sidebar_width: Option<u16>,
    #[serde(default)]
    pub sidebar_section_split: Option<f32>,
    #[serde(default)]
    pub collapsed_space_keys: std::collections::HashSet<String>,
}

#[derive(Serialize, Deserialize)]
pub struct SessionHistorySnapshot {
    /// Format version follows the matching session snapshot version.
    #[serde(default)]
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout_fingerprint: Option<String>,
    pub workspaces: Vec<WorkspaceHistorySnapshot>,
}

#[derive(Serialize, Deserialize)]
pub struct WorkspaceHistorySnapshot {
    pub tabs: Vec<TabHistorySnapshot>,
}

#[derive(Serialize, Deserialize)]
pub struct TabHistorySnapshot {
    pub panes: HashMap<u32, PaneHistorySnapshot>,
}

#[derive(Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub custom_name: Option<String>,
    pub identity_cwd: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_space: Option<crate::workspace::WorktreeSpaceMembership>,
    #[serde(default)]
    pub public_pane_numbers: HashMap<u32, usize>,
    #[serde(default)]
    pub next_public_pane_number: usize,
    #[serde(default)]
    pub public_tab_numbers: Vec<usize>,
    #[serde(default)]
    pub next_public_tab_number: usize,
    pub tabs: Vec<TabSnapshot>,
    #[serde(default)]
    pub active_tab: usize,
}

#[derive(Deserialize)]
struct LegacyWorkspaceSnapshot {
    #[serde(default)]
    custom_name: Option<String>,
    layout: LayoutSnapshot,
    panes: HashMap<u32, PaneSnapshot>,
    zoomed: bool,
    #[serde(default)]
    focused: Option<u32>,
    #[serde(default)]
    root_pane: Option<u32>,
}

#[derive(Serialize, Deserialize)]
pub struct TabSnapshot {
    #[serde(default)]
    pub custom_name: Option<String>,
    pub layout: LayoutSnapshot,
    pub panes: HashMap<u32, PaneSnapshot>,
    pub zoomed: bool,
    #[serde(default)]
    pub focused: Option<u32>,
    #[serde(default)]
    pub root_pane: Option<u32>,
}

#[derive(Serialize, Deserialize)]
pub struct PaneSnapshot {
    pub cwd: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_agent_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session: Option<PaneAgentSessionSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_argv: Option<Vec<String>>,
    /// Effective agent status the pane was reporting when this was captured,
    /// under the same name the socket API uses.
    ///
    /// Held as a name rather than the wire enum so a snapshot written by a
    /// build that knows a status this one does not still restores; the
    /// unreadable name is dropped instead of failing the whole session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_status: Option<String>,
    /// Unix seconds at which that status began. Restored together with
    /// `agent_status` so pane ages survive a restart or a live handoff.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_status_changed_at: Option<u64>,
    /// Unix second past which this claim is no longer worth honouring,
    /// present only while the pane it came from was still waiting to be
    /// classified.
    ///
    /// The deadline travels with the claim so every reader can check it
    /// against its own clock. A manifest that takes half a minute to reach the
    /// process that reads it cannot renew a claim by being slow, and no future
    /// reader has to remember to re-check anything the writer thought it knew.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_status_resolve_by: Option<u64>,
    /// Whether the pane had reported some other known status while this claim
    /// was outstanding.
    ///
    /// Travels with the claim because it is part of what the claim asserts: a
    /// claim that saw an interlude before a handoff is still a claim that saw
    /// one after it, and a reader that reset this would let the next process
    /// inherit an age across a status the previous one already reported.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub agent_status_saw_other: bool,
    /// The latest status date the pane had published when this was captured.
    ///
    /// Clients name a transition by its date, so the process that restores
    /// the pane dates everything after this and never hands a client a date it
    /// already holds for another status. Absent from a snapshot written before
    /// it existed, and ignored by a process that predates it: both fall back
    /// to `agent_status_changed_at`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_status_reported_through: Option<u64>,
    /// Whether the user had pinned this pane's terminal.
    ///
    /// Defaulted rather than required, so a snapshot written before the pin
    /// existed restores every pane unpinned instead of failing the session.
    #[serde(default)]
    pub pinned: bool,
    /// Who wrote the label, when it was not a person. Absent from a
    /// snapshot written before provenance was kept, which restores a label
    /// that is nobody's — the same thing it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_source: Option<crate::api::schema::LabelSource>,
    /// Unix seconds at which the label was written, absent for the same
    /// reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_at: Option<u64>,
}

/// The socket API's name for a status, as stored in a snapshot.
pub fn agent_status_snapshot_name(status: crate::api::schema::AgentStatus) -> &'static str {
    use crate::api::schema::AgentStatus;
    match status {
        AgentStatus::Idle => "idle",
        AgentStatus::Working => "working",
        AgentStatus::Blocked => "blocked",
        AgentStatus::Done => "done",
        AgentStatus::Unknown => "unknown",
    }
}

/// Read back a name written by [`agent_status_snapshot_name`], or `None` when
/// the snapshot names a status this build does not know.
pub fn agent_status_from_snapshot_name(name: &str) -> Option<crate::api::schema::AgentStatus> {
    use crate::api::schema::AgentStatus;
    match name {
        "idle" => Some(AgentStatus::Idle),
        "working" => Some(AgentStatus::Working),
        "blocked" => Some(AgentStatus::Blocked),
        "done" => Some(AgentStatus::Done),
        "unknown" => Some(AgentStatus::Unknown),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneAgentSessionSnapshot {
    pub source: String,
    pub agent: String,
    pub kind: crate::agent_resume::AgentSessionRefKind,
    pub value: String,
    /// The record file the harness reported for this session, when it named
    /// one its `value` does not already name.
    ///
    /// Captured so a restart or a handoff keeps reading the record the
    /// harness itself pointed at, rather than falling back to a derivation
    /// that may not find a conversation filed somewhere unusual. Absent
    /// from a snapshot written before it was kept, and from a session whose
    /// harness reported no path; a reader derives the file from `value`
    /// then, exactly as it did before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_path: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct PaneHistorySnapshot {
    pub ansi: String,
    pub lines: usize,
}

/// Serializable BSP tree.
#[derive(Serialize, Deserialize)]
pub enum LayoutSnapshot {
    Pane(u32),
    Split {
        direction: DirectionSnapshot,
        ratio: f32,
        first: Box<LayoutSnapshot>,
        second: Box<LayoutSnapshot>,
    },
}

#[derive(Serialize, Deserialize)]
pub enum DirectionSnapshot {
    Horizontal,
    Vertical,
}
