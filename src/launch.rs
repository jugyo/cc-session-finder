//! Exec the native resume command for a selected session.

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};

use crate::agent::AgentKind;
use crate::index::search::Hit;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResumeCommand {
    pub program: &'static str,
    pub args: Vec<String>,
}

pub(crate) fn resume_command(hit: &Hit) -> ResumeCommand {
    match hit.agent {
        AgentKind::Claude => ResumeCommand {
            program: "claude",
            args: vec!["--resume".to_string(), hit.native_session_id.clone()],
        },
        AgentKind::Codex => ResumeCommand {
            program: "codex",
            args: vec!["resume".to_string(), hit.native_session_id.clone()],
        },
    }
}

pub fn resume(hit: &Hit) -> Result<()> {
    let cwd = PathBuf::from(&hit.cwd);
    if cwd.is_dir() {
        std::env::set_current_dir(&cwd).with_context(|| format!("chdir {}", cwd.display()))?;
    } else {
        tracing::warn!(
            "session cwd {} does not exist; running in current dir",
            cwd.display()
        );
    }

    let command = resume_command(hit);
    let err = Command::new(command.program).args(command.args).exec();
    Err(anyhow::anyhow!("exec {} failed: {}", command.program, err))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::search::{Hit, Scores};

    fn hit(agent: AgentKind, native_session_id: &str) -> Hit {
        Hit {
            session_id: native_session_id.to_string(),
            agent,
            native_session_id: native_session_id.to_string(),
            ai_title: None,
            cwd: "/repo".to_string(),
            mtime: 0,
            msg_count: Some(1),
            first_prompt: None,
            file_path: "/repo/session.jsonl".to_string(),
            git_branch: None,
            pr_number: None,
            pr_url: None,
            pr_repo: None,
            is_worktree: false,
            tokens_input: 0,
            tokens_output: 0,
            tokens_cache_read: 0,
            tokens_cache_create: 0,
            snippet: None,
            snippet_role: None,
            snippet_message_count: None,
            scores: Scores::default(),
        }
    }

    #[test]
    fn claude_resume_command_uses_native_session_id() {
        assert_eq!(
            resume_command(&hit(AgentKind::Claude, "native-123")),
            ResumeCommand {
                program: "claude",
                args: vec!["--resume".to_string(), "native-123".to_string()],
            }
        );
    }

    #[test]
    fn codex_resume_command_uses_native_session_id() {
        assert_eq!(
            resume_command(&hit(AgentKind::Codex, "native-123")),
            ResumeCommand {
                program: "codex",
                args: vec!["resume".to_string(), "native-123".to_string()],
            }
        );
    }
}
