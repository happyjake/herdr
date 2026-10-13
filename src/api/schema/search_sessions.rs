use serde::{Deserialize, Serialize};

/// Resume identity without the reporting source. Paths identify pi sessions;
/// old Claude history without session ids uses a project-day key.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct SearchSessionKey {
    pub agent: String,
    pub kind: SearchSessionKeyKind,
    pub value: String,
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SearchSessionKeyKind {
    Id,
    Path,
    Day,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionHit {
    pub key: SearchSessionKey,
    pub agent: String,
    pub cwd: Option<String>,
    pub place_state: SessionPlaceState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nearest_existing: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub first_prompt: String,
    pub recent_prompts: Vec<String>,
    pub started_at_ms: u64,
    pub last_active_at_ms: u64,
    pub labels: Vec<SessionLabel>,
    pub state: SessionState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_pane_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outside: Option<SessionOutside>,
    pub resume: Option<SessionResume>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_blocked: Option<SessionResumeBlocked>,
    pub headless: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<SearchSessionKey>,
    pub subagent_count: u32,
    pub why: Option<SessionWhy>,
    pub score: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionPlaceState {
    Present,
    Gone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Live,
    Outside,
    Ended,
    RecordGone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionResumeBlocked {
    Live,
    Outside,
    RecordGone,
    FolderGone,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionResume {
    pub command: String,
    pub cwd: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionOutside {
    pub pid: u32,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionLabel {
    pub label: String,
    pub source: SessionLabelSource,
    pub pane_id: String,
    pub from_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionLabelSource {
    Editor,
    Person,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionWhy {
    pub field: SessionWhyField,
    pub line: String,
    pub terms: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionWhyField {
    Label,
    Title,
    FirstPrompt,
    Prompt,
    Cwd,
    Branch,
}
