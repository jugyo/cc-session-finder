pub mod claude;
pub mod codex;

use std::path::PathBuf;

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    Claude,
    Codex,
}

impl AgentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }
}

impl std::fmt::Display for AgentKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct SourceRecord {
    pub agent: AgentKind,
    pub session_id: String,
    pub path: PathBuf,
    pub mtime: i64,
    pub size: i64,
}

#[derive(Debug, Clone)]
pub struct SourceSession {
    pub session_id: String,
    pub agent: AgentKind,
    pub native_session_id: String,
    pub source_group: Option<String>,
    pub cwd: PathBuf,
    pub ai_title: Option<String>,
    pub first_prompt: Option<String>,
    pub msg_count: u32,
    pub mtime: i64,
    pub size: i64,
    pub file_path: PathBuf,
    pub git_branch: Option<String>,
    pub pr_number: Option<i64>,
    pub pr_url: Option<String>,
    pub pr_repo: Option<String>,
    pub tokens_input: u64,
    pub tokens_output: u64,
    pub tokens_cache_read: u64,
    pub tokens_cache_create: u64,
    pub model: Option<String>,
    pub models: Vec<String>,
    pub tool_call_count: u64,
    pub tool_error_count: u64,
    pub thinking_tokens: u64,
    pub wall_clock_ms: i64,
}

/// Collects model names observed across a session in order, keeping the set
/// unique while remembering the most recently seen value. `<synthetic>` and
/// other non-concrete markers are filtered by callers before `observe`.
#[derive(Debug, Default, Clone)]
pub struct ModelCollector {
    models: Vec<String>,
    latest: Option<String>,
}

impl ModelCollector {
    pub fn observe(&mut self, model: &str) {
        let model = model.trim();
        if model.is_empty() {
            return;
        }
        self.latest = Some(model.to_string());
        if !self.models.iter().any(|seen| seen == model) {
            self.models.push(model.to_string());
        }
    }

    /// The most recently observed model, or `None` when nothing was seen.
    pub fn latest(&self) -> Option<String> {
        self.latest.clone()
    }

    pub fn into_models(self) -> Vec<String> {
        self.models
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceMessage {
    pub turn_index: u32,
    pub role: String,
    pub text: String,
}

pub fn all_kinds() -> &'static [AgentKind] {
    &[AgentKind::Claude, AgentKind::Codex]
}

pub fn list_sessions(kind: AgentKind) -> Result<Vec<SourceRecord>> {
    match kind {
        AgentKind::Claude => claude::list_sessions(),
        AgentKind::Codex => codex::list_sessions(),
    }
}

/// Parsed output for one session: metadata, indexable messages, and the
/// per-step trajectory. Codex sessions currently yield an empty trajectory.
pub struct ExtractedSession {
    pub session: SourceSession,
    pub messages: Vec<SourceMessage>,
    pub trajectory: Vec<crate::session::TrajectoryStep>,
}

pub fn extract_session(record: &SourceRecord) -> Result<ExtractedSession> {
    match record.agent {
        AgentKind::Claude => claude::extract_session(record),
        AgentKind::Codex => codex::extract_session(record),
    }
}
