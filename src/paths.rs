//! cwd ↔ Claude project_dir encoding.

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

/// Root for Claude Code project sessions.
pub fn projects_root() -> PathBuf {
    let mut p = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    p.push(".claude");
    p.push("projects");
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
        assert_eq!(encode_cwd(Path::new("/Users/jugyo/.claude")), "-Users-jugyo--claude");
    }

    #[test]
    fn decode_hint_roundtrips_simple() {
        let decoded = decode_dir_hint("-Users-jugyo--claude");
        assert_eq!(decoded, PathBuf::from("/Users/jugyo/.claude"));
    }
}
