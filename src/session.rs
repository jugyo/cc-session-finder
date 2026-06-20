//! Claude Code JSONL session file parser.

use std::collections::HashSet;
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
    pub model: Option<String>,
    pub models: Vec<String>,
    pub tool_call_count: u64,
    pub tool_error_count: u64,
    pub thinking_tokens: u64,
    pub wall_clock_ms: i64,
}

/// Indexable message text extracted from one JSONL record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexableMessage {
    pub turn_index: u32,
    pub role: String,
    pub text: String,
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
    let mut tool_call_count: u64 = 0;
    let mut tool_error_count: u64 = 0;
    let mut thinking_chars: u64 = 0;
    let mut first_ts_ms: Option<i64> = None;
    let mut last_ts_ms: Option<i64> = None;
    let mut models = crate::agent::ModelCollector::default();
    // One API response is split across multiple assistant rows (thinking /
    // text / tool_use) that each repeat the same `usage`. Count each
    // `message.id` only once so tokens are not multi-counted.
    let mut counted_usage_ids: HashSet<String> = HashSet::new();

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

        if let Some(ms) = v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(parse_timestamp_ms)
        {
            first_ts_ms = Some(first_ts_ms.map_or(ms, |first| first.min(ms)));
            last_ts_ms = Some(last_ts_ms.map_or(ms, |last| last.max(ms)));
        }

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
                        first_prompt = Some(truncate(&text, 500));
                    }
                }
                if let Some(content) = v.get("message").and_then(|m| m.get("content")) {
                    tool_error_count = tool_error_count.saturating_add(count_tool_errors(content));
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
                if let Some(model) = v
                    .get("message")
                    .and_then(|m| m.get("model"))
                    .and_then(|m| m.as_str())
                {
                    if is_concrete_model(model) {
                        models.observe(model);
                    }
                }
                if let Some(content) = v.get("message").and_then(|m| m.get("content")) {
                    tool_call_count = tool_call_count.saturating_add(count_tool_uses(content));
                    thinking_chars = thinking_chars.saturating_add(count_thinking_chars(content));
                }
                let msg = v.get("message");
                let already_counted = msg
                    .and_then(|m| m.get("id"))
                    .and_then(|id| id.as_str())
                    .is_some_and(|id| !counted_usage_ids.insert(id.to_string()));
                if !already_counted {
                    if let Some(u) = msg.and_then(|m| m.get("usage")) {
                        tokens_input = tokens_input.saturating_add(usage_u64(u, "input_tokens"));
                        tokens_output = tokens_output.saturating_add(usage_u64(u, "output_tokens"));
                        tokens_cache_read = tokens_cache_read
                            .saturating_add(usage_u64(u, "cache_read_input_tokens"));
                        tokens_cache_create = tokens_cache_create
                            .saturating_add(usage_u64(u, "cache_creation_input_tokens"));
                    }
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

    // No per-block token breakdown exists in the JSONL, so approximate thinking
    // output from its character length (~4 chars/token). Used only as a coarse
    // ratio signal, where the constant factor cancels out.
    let thinking_tokens = thinking_chars / 4;
    let wall_clock_ms = match (first_ts_ms, last_ts_ms) {
        (Some(first), Some(last)) => (last - first).max(0),
        _ => 0,
    };

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
        model: models.latest(),
        models: models.into_models(),
        tool_call_count,
        tool_error_count,
        thinking_tokens,
        wall_clock_ms,
    })
}

/// Claude marks synthetic assistant records (e.g. interrupts) with the
/// `<synthetic>` placeholder rather than a real model id; those are not a model.
fn is_concrete_model(model: &str) -> bool {
    let model = model.trim();
    !model.is_empty() && model != "<synthetic>"
}

pub fn extract_indexable_messages_from_file(path: &Path) -> Result<Vec<IndexableMessage>> {
    let f = File::open(path)?;
    let reader = BufReader::new(f);
    let mut messages = Vec::new();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };

        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if let Some((role, text)) = indexable_message_text(&v) {
            messages.push(IndexableMessage {
                turn_index: messages.len() as u32,
                role,
                text,
            });
        }
    }

    Ok(messages)
}

fn usage_u64(usage: &Value, key: &str) -> u64 {
    usage.get(key).and_then(|v| v.as_u64()).unwrap_or(0)
}

/// Parse an RFC3339 transcript timestamp into Unix milliseconds.
fn parse_timestamp_ms(value: &str) -> Option<i64> {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::parse(value, &Rfc3339)
        .ok()
        .map(|dt| (dt.unix_timestamp_nanos() / 1_000_000) as i64)
}

/// Count `tool_use` blocks in an assistant message's content array.
fn count_tool_uses(content: &Value) -> u64 {
    content_blocks(content, "tool_use")
}

/// Count `tool_result` blocks flagged `is_error: true` in a user message's
/// content array.
fn count_tool_errors(content: &Value) -> u64 {
    let Some(parts) = content.as_array() else {
        return 0;
    };
    parts
        .iter()
        .filter(|part| part.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
        .filter(|part| part.get("is_error").and_then(|e| e.as_bool()) == Some(true))
        .count() as u64
}

/// Sum the character length of `thinking` blocks in an assistant message's
/// content array.
fn count_thinking_chars(content: &Value) -> u64 {
    let Some(parts) = content.as_array() else {
        return 0;
    };
    parts
        .iter()
        .filter(|part| part.get("type").and_then(|t| t.as_str()) == Some("thinking"))
        .filter_map(|part| part.get("thinking").and_then(|t| t.as_str()))
        .map(|text| text.chars().count() as u64)
        .sum()
}

fn content_blocks(content: &Value, block_type: &str) -> u64 {
    let Some(parts) = content.as_array() else {
        return 0;
    };
    parts
        .iter()
        .filter(|part| part.get("type").and_then(|t| t.as_str()) == Some(block_type))
        .count() as u64
}

fn indexable_message_text(v: &Value) -> Option<(String, String)> {
    let ty = v.get("type").and_then(|t| t.as_str())?;
    if ty != "user" && ty != "assistant" {
        return None;
    }

    let msg = v.get("message")?;
    let role = msg.get("role").and_then(|r| r.as_str())?;
    if role != ty {
        return None;
    }

    let text = message_text(msg.get("content")?)?;

    Some((role.to_string(), text))
}

fn message_text(content: &Value) -> Option<String> {
    if let Some(s) = content.as_str() {
        return conversation_text(s).map(str::to_string);
    }

    let parts = content.as_array()?;
    let texts: Vec<&str> = parts
        .iter()
        .filter(|part| part.get("type").and_then(|t| t.as_str()) == Some("text"))
        .filter_map(|part| part.get("text").and_then(|t| t.as_str()))
        .filter_map(conversation_text)
        .collect();

    if texts.is_empty() {
        None
    } else {
        Some(texts.join("\n\n"))
    }
}

/// Pull the first human-visible text-type content from a user message record.
fn first_user_text(v: &Value) -> Option<String> {
    let msg = v.get("message")?;
    let content = msg.get("content")?;

    if let Some(s) = content.as_str() {
        return conversation_text(s).map(str::to_string);
    }
    let arr = content.as_array()?;
    for part in arr {
        if part.get("type").and_then(|t| t.as_str()) == Some("text") {
            if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                if let Some(text) = conversation_text(t) {
                    return Some(text.to_string());
                }
            }
        }
    }
    None
}

fn conversation_text(text: &str) -> Option<&str> {
    is_human_visible_text(text).then_some(text)
}

pub(crate) fn is_human_visible_text(text: &str) -> bool {
    let trimmed = text.trim_start();
    !trimmed.is_empty() && !is_internal_transcript_text(trimmed)
}

fn is_internal_transcript_text(trimmed: &str) -> bool {
    trimmed.starts_with("<ide_opened_file>")
        || trimmed.starts_with("<ide_selection>")
        || trimmed.starts_with("<command-")
        || trimmed.starts_with("<system-reminder>")
        || trimmed.starts_with("<local-command-")
        || trimmed.starts_with("<bash-")
        || trimmed.starts_with("<task-notification>")
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn message(v: Value) -> Option<(String, String)> {
        indexable_message_text(&v)
    }

    fn write_fixture(tag: &str, body: &str) -> PathBuf {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "ccsf-model-test-{}-{tag}.jsonl",
            std::process::id()
        ));
        std::fs::write(&path, body).expect("write fixture");
        path
    }

    #[test]
    fn extracts_single_assistant_model() {
        let path = write_fixture(
            "single",
            r#"{"type":"user","message":{"role":"user","content":"hi"}}
{"type":"assistant","message":{"role":"assistant","model":"claude-opus-4-8","content":[{"type":"text","text":"hello"}]}}"#,
        );

        let meta = extract_from_file(&path).expect("meta");
        let _ = std::fs::remove_file(&path);

        assert_eq!(meta.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(meta.models, vec!["claude-opus-4-8".to_string()]);
    }

    #[test]
    fn extracts_ordered_unique_models_with_latest() {
        let path = write_fixture(
            "two",
            r#"{"type":"assistant","message":{"role":"assistant","model":"claude-opus-4-7","content":[{"type":"text","text":"a"}]}}
{"type":"assistant","message":{"role":"assistant","model":"claude-opus-4-8","content":[{"type":"text","text":"b"}]}}
{"type":"assistant","message":{"role":"assistant","model":"claude-opus-4-7","content":[{"type":"text","text":"c"}]}}"#,
        );

        let meta = extract_from_file(&path).expect("meta");
        let _ = std::fs::remove_file(&path);

        assert_eq!(
            meta.models,
            vec!["claude-opus-4-7".to_string(), "claude-opus-4-8".to_string()]
        );
        assert_eq!(meta.model.as_deref(), Some("claude-opus-4-7"));
    }

    #[test]
    fn ignores_synthetic_and_missing_models() {
        let path = write_fixture(
            "synthetic",
            r#"{"type":"assistant","message":{"role":"assistant","model":"<synthetic>","content":[{"type":"text","text":"a"}]}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"b"}]}}"#,
        );

        let meta = extract_from_file(&path).expect("meta");
        let _ = std::fs::remove_file(&path);

        assert_eq!(meta.model, None);
        assert!(meta.models.is_empty());
    }

    #[test]
    fn counts_usage_once_per_message_id() {
        // Three assistant rows (thinking / text / tool_use) split from one API
        // response repeat the same id and usage; a fourth row is a distinct
        // response. Usage must sum once per id, not once per row.
        let path = write_fixture(
            "dedup",
            r#"{"type":"assistant","message":{"role":"assistant","id":"msg_A","model":"claude-opus-4-8","usage":{"input_tokens":100,"output_tokens":10,"cache_read_input_tokens":1000,"cache_creation_input_tokens":50},"content":[{"type":"thinking","thinking":"x"}]}}
{"type":"assistant","message":{"role":"assistant","id":"msg_A","model":"claude-opus-4-8","usage":{"input_tokens":100,"output_tokens":10,"cache_read_input_tokens":1000,"cache_creation_input_tokens":50},"content":[{"type":"text","text":"y"}]}}
{"type":"assistant","message":{"role":"assistant","id":"msg_A","model":"claude-opus-4-8","usage":{"input_tokens":100,"output_tokens":10,"cache_read_input_tokens":1000,"cache_creation_input_tokens":50},"content":[{"type":"tool_use","name":"Edit"}]}}
{"type":"assistant","message":{"role":"assistant","id":"msg_B","model":"claude-opus-4-8","usage":{"input_tokens":5,"output_tokens":2,"cache_read_input_tokens":3,"cache_creation_input_tokens":1},"content":[{"type":"text","text":"z"}]}}"#,
        );

        let meta = extract_from_file(&path).expect("meta");
        let _ = std::fs::remove_file(&path);

        assert_eq!(meta.tokens_input, 105);
        assert_eq!(meta.tokens_output, 12);
        assert_eq!(meta.tokens_cache_read, 1003);
        assert_eq!(meta.tokens_cache_create, 51);
    }

    #[test]
    fn counts_usage_for_rows_without_message_id() {
        // Rows lacking an id cannot be deduped, so each is counted.
        let path = write_fixture(
            "no-id",
            r#"{"type":"assistant","message":{"role":"assistant","model":"claude-opus-4-8","usage":{"input_tokens":7,"output_tokens":3,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"a"}]}}
{"type":"assistant","message":{"role":"assistant","model":"claude-opus-4-8","usage":{"input_tokens":7,"output_tokens":3,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"b"}]}}"#,
        );

        let meta = extract_from_file(&path).expect("meta");
        let _ = std::fs::remove_file(&path);

        assert_eq!(meta.tokens_input, 14);
        assert_eq!(meta.tokens_output, 6);
    }

    #[test]
    fn derives_tool_thinking_and_wall_clock_metrics() {
        let path = write_fixture(
            "derived",
            r#"{"type":"user","timestamp":"2026-06-20T01:00:00.000Z","message":{"role":"user","content":"go"}}
{"type":"assistant","timestamp":"2026-06-20T01:00:01.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"12345678"}]}}
{"type":"assistant","timestamp":"2026-06-20T01:00:02.000Z","message":{"role":"assistant","content":[{"type":"tool_use","name":"Edit"},{"type":"tool_use","name":"Bash"}]}}
{"type":"user","timestamp":"2026-06-20T01:00:05.500Z","message":{"role":"user","content":[{"type":"tool_result","is_error":true,"content":"boom"},{"type":"tool_result","content":"ok"}]}}"#,
        );

        let meta = extract_from_file(&path).expect("meta");
        let _ = std::fs::remove_file(&path);

        assert_eq!(meta.tool_call_count, 2);
        assert_eq!(meta.tool_error_count, 1);
        // 8 thinking chars / 4 ≈ 2 tokens.
        assert_eq!(meta.thinking_tokens, 2);
        // 01:00:00.000 → 01:00:05.500 = 5500 ms.
        assert_eq!(meta.wall_clock_ms, 5_500);
    }

    #[test]
    fn extracts_user_string_content() {
        let out = message(json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": "hello from user"
            }
        }));

        assert_eq!(
            out,
            Some(("user".to_string(), "hello from user".to_string()))
        );
    }

    #[test]
    fn extracts_user_text_parts() {
        let out = message(json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [
                    {"type": "text", "text": "first"},
                    {"type": "image", "source": {}},
                    {"type": "text", "text": "second"}
                ]
            }
        }));

        assert_eq!(
            out,
            Some(("user".to_string(), "first\n\nsecond".to_string()))
        );
    }

    #[test]
    fn extracts_only_human_visible_text_parts() {
        let out = message(json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [
                    {"type": "text", "text": "<bash-stdout>command output</bash-stdout>"},
                    {"type": "text", "text": "please explain this failure"},
                    {"type": "text", "text": "<task-notification><task-id>abc</task-id></task-notification>"}
                ]
            }
        }));

        assert_eq!(
            out,
            Some((
                "user".to_string(),
                "please explain this failure".to_string()
            ))
        );
    }

    #[test]
    fn extracts_assistant_text_parts() {
        let out = message(json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "hidden"},
                    {"type": "text", "text": "assistant answer"},
                    {"type": "tool_use", "name": "Edit"}
                ]
            }
        }));

        assert_eq!(
            out,
            Some(("assistant".to_string(), "assistant answer".to_string()))
        );
    }

    #[test]
    fn skips_tool_result_and_thinking_only_messages() {
        let tool_result = message(json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [
                    {"type": "tool_result", "content": "command output"}
                ]
            }
        }));
        let thinking = message(json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "hidden"}
                ]
            }
        }));

        assert_eq!(tool_result, None);
        assert_eq!(thinking, None);
    }

    #[test]
    fn skips_noise_text_messages() {
        let local_command = message(json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": "<local-command-stdout>cargo output</local-command-stdout>"
            }
        }));
        let task_notification = message(json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": "<task-notification><task-id>abc</task-id></task-notification>"
            }
        }));
        let bash_stdout = message(json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": "<bash-stdout>command output</bash-stdout>"
            }
        }));

        assert_eq!(local_command, None);
        assert_eq!(task_notification, None);
        assert_eq!(bash_stdout, None);
    }

    #[test]
    fn skips_non_message_records_and_role_mismatches() {
        let attachment = message(json!({
            "type": "attachment",
            "message": {
                "role": "user",
                "content": "attached"
            }
        }));
        let mismatch = message(json!({
            "type": "user",
            "message": {
                "role": "assistant",
                "content": "wrong role"
            }
        }));

        assert_eq!(attachment, None);
        assert_eq!(mismatch, None);
    }
}
