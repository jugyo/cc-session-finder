//! JSONL session file parser.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;

/// Extracted metadata for one session JSONL file.
#[derive(Debug, Clone)]
pub struct SessionMeta {
    pub session_id: String,
    pub project_dir: String,
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

pub fn extract_from_file(path: &Path) -> Result<SessionMeta> {
    let metadata = std::fs::metadata(path)?;
    let mtime = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let size = metadata.len() as i64;

    let file_name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let project_dir = path
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut ai_title: Option<String> = None;
    let mut first_prompt: Option<String> = None;
    let mut msg_count: u32 = 0;
    let mut cwd: Option<PathBuf> = None;
    let mut git_branch: Option<String> = None;
    let mut pr_number: Option<i64> = None;
    let mut pr_url: Option<String> = None;
    let mut pr_repo: Option<String> = None;
    let mut tokens_input: u64 = 0;
    let mut tokens_output: u64 = 0;
    let mut tokens_cache_read: u64 = 0;
    let mut tokens_cache_create: u64 = 0;

    let f = File::open(path)?;
    let reader = BufReader::new(f);

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };

        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match ty {
            "ai-title" => {
                if let Some(t) = v.get("aiTitle").and_then(|t| t.as_str()) {
                    ai_title = Some(t.to_string());
                }
            }
            "user" => {
                msg_count += 1;
                if cwd.is_none() {
                    if let Some(c) = v.get("cwd").and_then(|c| c.as_str()) {
                        cwd = Some(PathBuf::from(c));
                    }
                }
                if git_branch.is_none() {
                    if let Some(b) = v.get("gitBranch").and_then(|b| b.as_str()) {
                        if !b.is_empty() {
                            git_branch = Some(b.to_string());
                        }
                    }
                }
                if first_prompt.is_none() {
                    if let Some(text) = first_user_text(&v) {
                        if !is_noise(&text) {
                            first_prompt = Some(truncate(&text, 500));
                        }
                    }
                }
            }
            "assistant" => {
                msg_count += 1;
                if cwd.is_none() {
                    if let Some(c) = v.get("cwd").and_then(|c| c.as_str()) {
                        cwd = Some(PathBuf::from(c));
                    }
                }
                if git_branch.is_none() {
                    if let Some(b) = v.get("gitBranch").and_then(|b| b.as_str()) {
                        if !b.is_empty() {
                            git_branch = Some(b.to_string());
                        }
                    }
                }
                if let Some(u) = v.get("message").and_then(|m| m.get("usage")) {
                    tokens_input = tokens_input.saturating_add(usage_u64(u, "input_tokens"));
                    tokens_output = tokens_output.saturating_add(usage_u64(u, "output_tokens"));
                    tokens_cache_read =
                        tokens_cache_read.saturating_add(usage_u64(u, "cache_read_input_tokens"));
                    tokens_cache_create = tokens_cache_create
                        .saturating_add(usage_u64(u, "cache_creation_input_tokens"));
                }
            }
            "pr-link" => {
                // Take the latest pr-link record (sessions sometimes touch
                // multiple PRs; the most recent is most relevant).
                if let Some(n) = v.get("prNumber").and_then(|n| n.as_i64()) {
                    pr_number = Some(n);
                }
                if let Some(u) = v.get("prUrl").and_then(|u| u.as_str()) {
                    pr_url = Some(u.to_string());
                }
                if let Some(r) = v.get("prRepository").and_then(|r| r.as_str()) {
                    pr_repo = Some(r.to_string());
                }
            }
            _ => {}
        }
    }

    let cwd = cwd.unwrap_or_else(|| crate::paths::decode_dir_hint(&project_dir));

    Ok(SessionMeta {
        session_id: file_name,
        project_dir,
        cwd,
        ai_title,
        first_prompt,
        msg_count,
        mtime,
        size,
        file_path: path.to_path_buf(),
        git_branch,
        pr_number,
        pr_url,
        pr_repo,
        tokens_input,
        tokens_output,
        tokens_cache_read,
        tokens_cache_create,
    })
}

fn usage_u64(usage: &Value, key: &str) -> u64 {
    usage.get(key).and_then(|v| v.as_u64()).unwrap_or(0)
}

/// Pull the first text-type content from a user message record.
fn first_user_text(v: &Value) -> Option<String> {
    let msg = v.get("message")?;
    let content = msg.get("content")?;

    // content can be a string or an array of parts.
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    let arr = content.as_array()?;
    for part in arr {
        if part.get("type").and_then(|t| t.as_str()) == Some("text") {
            if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                return Some(t.to_string());
            }
        }
    }
    None
}

fn is_noise(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.is_empty()
        || trimmed.starts_with("<ide_opened_file>")
        || trimmed.starts_with("<ide_selection>")
        || trimmed.starts_with("<command-")
        || trimmed.starts_with("<system-reminder>")
        || trimmed.starts_with("<local-command-")
}

fn truncate(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i >= max_chars {
            out.push('…');
            break;
        }
        out.push(c);
    }
    out
}

/// preview = title + " | " + first_prompt (for FTS5 indexing).
pub fn build_preview(meta: &SessionMeta) -> String {
    let mut s = String::new();
    if let Some(t) = &meta.ai_title {
        s.push_str(t);
    }
    if let Some(p) = &meta.first_prompt {
        if !s.is_empty() {
            s.push_str(" | ");
        }
        s.push_str(p);
    }
    s
}
