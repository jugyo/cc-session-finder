//! Exec `claude --resume <session_id>` in the session's cwd.

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};

use crate::index::search::Hit;

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

    // `exec` replaces the current process with `claude` on success.
    let err = Command::new("claude")
        .arg("--resume")
        .arg(&hit.session_id)
        .exec();
    Err(anyhow::anyhow!("exec claude failed: {}", err))
}
