pub mod claude;
pub mod codex;

use std::path::PathBuf;

use anyhow::Result;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
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

pub fn extract_session(record: &SourceRecord) -> Result<(SourceSession, Vec<SourceMessage>)> {
    match record.agent {
        AgentKind::Claude => claude::extract_session(record),
        AgentKind::Codex => codex::extract_session(record),
    }
}
