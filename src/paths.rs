//! Paths for source session stores and the local cache.

use std::path::{Path, PathBuf};

/// Forward encoding used by Claude Code for `~/.claude/projects/<dir>`.
///
/// Replaces both `/` and `.` with `-`. The leading `/` becomes a leading `-`.
/// Example: `/Users/jugyo/.claude` → `-Users-jugyo--claude`.
///
/// Currently only exercised by tests; kept as the symmetric inverse of
/// [`decode_dir_hint`] so the encoding contract is documented in one place.
#[allow(dead_code)]
pub fn encode_cwd(cwd: &Path) -> String {
    let mut s = String::new();
    for c in cwd.to_string_lossy().chars() {
        match c {
            '/' | '.' => s.push('-'),
            other => s.push(other),
        }
    }
    s
}

/// Best-effort reverse of `encode_cwd`. Ambiguous in the general case (a `-`
/// in a real directory name is indistinguishable from a separator), so the
/// caller should treat the result as a hint and prefer cwd values read from
/// inside JSONL records when available.
pub fn decode_dir_hint(dir_name: &str) -> PathBuf {
    // Heuristic: a literal `--` decodes to `/.` (path separator + hidden-dir
    // dot). A single `-` decodes to `/`.
    let mut s = String::new();
    let bytes = dir_name.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'-' {
            if i + 1 < bytes.len() && bytes[i + 1] == b'-' {
                s.push_str("/.");
                i += 2;
                continue;
            }
            s.push('/');
            i += 1;
            continue;
        }
        s.push(bytes[i] as char);
        i += 1;
    }
    PathBuf::from(s)
}

/// Normalize a user-supplied cwd filter to the absolute, symlink-resolved
/// form stored in the index.
///
/// The index records cwd values as written by the source agent: absolute and
/// symlink-resolved (what `getcwd` returns). A literal `cwd = ?` filter only
/// matches when the input is in that exact shape, so a relative path, a
/// trailing slash, or an unresolved symlink would otherwise silently match
/// nothing. `canonicalize` collapses all of those; when the path does not
/// exist on disk we fall back to a lexical absolutization that at least fixes
/// relative inputs and trailing slashes.
pub fn normalize_cwd_filter(cwd: &Path) -> PathBuf {
    let absolute = if cwd.is_absolute() {
        cwd.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|base| base.join(cwd))
            .unwrap_or_else(|_| cwd.to_path_buf())
    };
    std::fs::canonicalize(&absolute).unwrap_or_else(|_| lexical_normalize(&absolute))
}

/// Resolve `.` / `..` / empty components without touching the filesystem.
fn lexical_normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(component);
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Root for Claude Code project sessions.
pub fn claude_projects_root() -> PathBuf {
    let mut p = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    p.push(".claude");
    p.push("projects");
    p
}

/// Root for Codex local state.
pub fn codex_home() -> PathBuf {
    let mut p = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    p.push(".codex");
    p
}

/// Codex local state database.
pub fn codex_state_db() -> PathBuf {
    let mut p = codex_home();
    p.push("state_5.sqlite");
    p
}

/// Root for active Codex rollout transcripts.
pub fn codex_sessions_root() -> PathBuf {
    let mut p = codex_home();
    p.push("sessions");
    p
}

/// Root for archived Codex rollout transcripts.
pub fn codex_archived_sessions_root() -> PathBuf {
    let mut p = codex_home();
    p.push("archived_sessions");
    p
}

/// Cache root for our DB and downloaded models.
pub fn cache_root() -> PathBuf {
    let mut p = dirs::cache_dir().unwrap_or_else(|| {
        let mut h = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        h.push(".cache");
        h
    });
    p.push("cc-session-finder");
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_normal_path() {
        assert_eq!(
            encode_cwd(Path::new("/Users/jugyo/workspace/jugyo/cc-session-finder")),
            "-Users-jugyo-workspace-jugyo-cc-session-finder"
        );
    }

    #[test]
    fn encodes_hidden_dir() {
        assert_eq!(
            encode_cwd(Path::new("/Users/jugyo/.claude")),
            "-Users-jugyo--claude"
        );
    }

    #[test]
    fn decode_hint_roundtrips_simple() {
        let decoded = decode_dir_hint("-Users-jugyo--claude");
        assert_eq!(decoded, PathBuf::from("/Users/jugyo/.claude"));
    }

    #[test]
    fn normalize_strips_trailing_slash_on_missing_path() {
        assert_eq!(
            normalize_cwd_filter(Path::new("/no/such/dir/")),
            PathBuf::from("/no/such/dir")
        );
    }

    #[test]
    fn normalize_resolves_dot_components_on_missing_path() {
        assert_eq!(
            normalize_cwd_filter(Path::new("/a/b/../c/./d")),
            PathBuf::from("/a/c/d")
        );
    }

    #[test]
    fn normalize_makes_relative_paths_absolute() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(normalize_cwd_filter(Path::new(".")), cwd);
    }
}
