use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OpenFlags};
use serde_json::Value;

use super::{AgentKind, ExtractedSession, SourceMessage, SourceRecord, SourceSession};

const SESSION_PREFIX: &str = "codex:";

#[derive(Debug, Clone)]
struct ThreadRow {
    id: String,
    rollout_path: Option<PathBuf>,
    cwd: PathBuf,
    title: Option<String>,
    first_user_message: Option<String>,
    preview: Option<String>,
    updated_at: i64,
    updated_at_ms: Option<i64>,
    source: Option<String>,
    model_provider: Option<String>,
    git_branch: Option<String>,
    tokens_used: u64,
}

pub fn list_sessions() -> Result<Vec<SourceRecord>> {
    let db_path = crate::paths::codex_state_db();
    if !db_path.exists() {
        return Ok(Vec::new());
    }

    let conn = open_state_db(&db_path)?;
    let mut stmt = conn.prepare(
        "SELECT id, rollout_path, cwd, title, first_user_message, preview,
                updated_at, updated_at_ms, source, model_provider, git_branch, tokens_used
         FROM threads
         WHERE id != '' AND cwd != ''
         ORDER BY updated_at DESC",
    )?;

    let rows = stmt.query_map([], thread_row_from_sql)?;
    let mut records = Vec::new();
    for row in rows {
        let row = row?;
        let source_path = resolve_rollout_path(&row).unwrap_or_else(|| db_path.clone());
        let (mtime, size) = fingerprint(&row, source_path.as_path());
        records.push(SourceRecord {
            agent: AgentKind::Codex,
            session_id: stable_session_id(&row.id),
            path: source_path,
            mtime,
            size,
        });
    }

    Ok(records)
}

pub fn extract_session(record: &SourceRecord) -> Result<ExtractedSession> {
    let db_path = crate::paths::codex_state_db();
    let native_session_id = native_session_id(&record.session_id);
    let conn = open_state_db(&db_path)?;
    let row = conn
        .query_row(
            "SELECT id, rollout_path, cwd, title, first_user_message, preview,
                    updated_at, updated_at_ms, source, model_provider, git_branch, tokens_used
             FROM threads
             WHERE id = ?1",
            params![native_session_id],
            thread_row_from_sql,
        )
        .with_context(|| format!("query codex thread {native_session_id}"))?;

    let source_path = resolve_rollout_path(&row).unwrap_or_else(|| record.path.clone());
    let (mtime, size) = fingerprint(&row, source_path.as_path());
    let mut messages = if source_path.is_file() {
        extract_messages_from_file(&source_path)?
    } else {
        Vec::new()
    };
    let usage = if source_path.is_file() {
        token_usage_from_file(&source_path).unwrap_or_default()
    } else {
        TokenUsage::default()
    };
    let models = if source_path.is_file() {
        models_from_file(&source_path).unwrap_or_default()
    } else {
        super::ModelCollector::default()
    };
    let first_prompt = row
        .first_user_message
        .clone()
        .filter(|text| crate::session::is_human_visible_text(text))
        .or_else(|| first_user_message(&messages));

    Ok(ExtractedSession {
        session: SourceSession {
            session_id: stable_session_id(&row.id),
            agent: AgentKind::Codex,
            native_session_id: row.id.clone(),
            source_group: source_group(&row),
            cwd: row.cwd,
            ai_title: row.title.or(row.preview),
            first_prompt,
            msg_count: messages.len() as u32,
            mtime,
            size,
            file_path: source_path,
            git_branch: row.git_branch,
            pr_number: None,
            pr_url: None,
            pr_repo: None,
            tokens_input: usage.or_total(row.tokens_used),
            tokens_output: usage.output_tokens,
            tokens_cache_read: usage.cached_input_tokens,
            tokens_cache_create: 0,
            model: models.latest(),
            models: models.into_models(),
            // Derived efficiency metrics are Claude-only for now; the Codex
            // rollout schema differs and is out of scope for this scaffold.
            tool_call_count: 0,
            tool_error_count: 0,
            thinking_tokens: 0,
            wall_clock_ms: 0,
        },
        messages: {
            for (turn_index, message) in messages.iter_mut().enumerate() {
                message.turn_index = turn_index as u32;
            }
            messages
        },
        // Step-level trajectory is Claude-only for now; the Codex rollout
        // schema differs and is out of scope for this scaffold.
        trajectory: Vec::new(),
    })
}

fn open_state_db(path: &Path) -> Result<Connection> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open codex state db {}", path.display()))
}

fn thread_row_from_sql(row: &rusqlite::Row<'_>) -> rusqlite::Result<ThreadRow> {
    Ok(ThreadRow {
        id: row.get(0)?,
        rollout_path: optional_path(row.get::<_, Option<String>>(1)?),
        cwd: PathBuf::from(row.get::<_, String>(2)?),
        title: non_empty(row.get(3)?),
        first_user_message: non_empty(row.get(4)?),
        preview: non_empty(row.get(5)?),
        updated_at: row.get(6)?,
        updated_at_ms: row.get(7)?,
        source: non_empty(row.get(8)?),
        model_provider: non_empty(row.get(9)?),
        git_branch: non_empty(row.get(10)?),
        tokens_used: row.get::<_, i64>(11).unwrap_or(0).max(0) as u64,
    })
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim().to_string();
        (!value.is_empty()).then_some(value)
    })
}

fn optional_path(value: Option<String>) -> Option<PathBuf> {
    non_empty(value).map(PathBuf::from)
}

fn stable_session_id(native_session_id: &str) -> String {
    format!("{SESSION_PREFIX}{native_session_id}")
}

fn native_session_id(session_id: &str) -> &str {
    session_id
        .strip_prefix(SESSION_PREFIX)
        .unwrap_or(session_id)
}

fn source_group(row: &ThreadRow) -> Option<String> {
    row.source.clone().or_else(|| row.model_provider.clone())
}

fn first_user_message(messages: &[SourceMessage]) -> Option<String> {
    messages
        .iter()
        .find(|message| message.role == "user")
        .map(|message| truncate(&message.text, 500))
}

fn fingerprint(row: &ThreadRow, path: &Path) -> (i64, i64) {
    let mut mtime = row
        .updated_at_ms
        .filter(|value| *value > 0)
        .map(|value| value / 1000)
        .unwrap_or(row.updated_at);
    let mut size = row.tokens_used as i64;

    if let Ok(metadata) = std::fs::metadata(path) {
        size = metadata.len() as i64;
        if let Some(file_mtime) = file_mtime(&metadata) {
            mtime = mtime.max(file_mtime);
        }
    }

    (mtime, size)
}

fn file_mtime(metadata: &std::fs::Metadata) -> Option<i64> {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as i64)
}

fn resolve_rollout_path(row: &ThreadRow) -> Option<PathBuf> {
    if let Some(path) = row.rollout_path.as_ref().filter(|path| path.is_file()) {
        return Some(path.clone());
    }

    for root in [
        crate::paths::codex_sessions_root(),
        crate::paths::codex_archived_sessions_root(),
    ] {
        let pattern = format!("{}/**/*{}*.jsonl", root.to_string_lossy(), row.id);
        if let Ok(paths) = glob::glob(&pattern) {
            if let Some(path) = paths.flatten().find(|path| path.is_file()) {
                return Some(path);
            }
        }
    }

    None
}

fn extract_messages_from_file(path: &Path) -> Result<Vec<SourceMessage>> {
    let file =
        File::open(path).with_context(|| format!("open codex rollout {}", path.display()))?;
    extract_messages(BufReader::new(file))
}

fn extract_messages<R: BufRead>(reader: R) -> Result<Vec<SourceMessage>> {
    let mut response_messages = Vec::new();
    let mut event_messages = Vec::new();

    for line in reader.lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => continue,
        };
        let value: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => continue,
        };

        if let Some((role, text)) = event_message_text(&value) {
            event_messages.push(source_message(event_messages.len(), role, text));
        }
        if let Some((role, text)) = response_message_text(&value) {
            response_messages.push(source_message(response_messages.len(), role, text));
        }
    }

    if event_messages.is_empty() {
        Ok(response_messages)
    } else {
        Ok(event_messages)
    }
}

fn source_message(turn_index: usize, role: &str, text: String) -> SourceMessage {
    SourceMessage {
        turn_index: turn_index as u32,
        role: role.to_string(),
        text,
    }
}

fn event_message_text(value: &Value) -> Option<(&'static str, String)> {
    if value.get("type").and_then(|value| value.as_str())? != "event_msg" {
        return None;
    }
    let payload = value.get("payload")?;
    let role = match payload.get("type").and_then(|value| value.as_str())? {
        "user_message" => "user",
        "agent_message" => "assistant",
        _ => return None,
    };
    let text = payload.get("message")?.as_str()?;
    crate::session::is_human_visible_text(text)
        .then(|| text.to_string())
        .map(|text| (role, text))
}

fn response_message_text(value: &Value) -> Option<(&'static str, String)> {
    if value.get("type").and_then(|value| value.as_str())? != "response_item" {
        return None;
    }
    let payload = value.get("payload")?;
    if payload.get("type").and_then(|value| value.as_str())? != "message" {
        return None;
    }
    let role = match payload.get("role").and_then(|value| value.as_str())? {
        "user" => "user",
        "assistant" => "assistant",
        _ => return None,
    };
    let text = content_text(payload.get("content")?)?;
    Some((role, text))
}

fn content_text(content: &Value) -> Option<String> {
    if let Some(text) = content.as_str() {
        return crate::session::is_human_visible_text(text).then(|| text.to_string());
    }

    let texts: Vec<&str> = content
        .as_array()?
        .iter()
        .filter(|part| {
            matches!(
                part.get("type").and_then(|value| value.as_str()),
                Some("input_text" | "output_text" | "text")
            )
        })
        .filter_map(|part| part.get("text").and_then(|value| value.as_str()))
        .filter(|text| crate::session::is_human_visible_text(text))
        .collect();

    (!texts.is_empty()).then(|| texts.join("\n\n"))
}

#[derive(Debug, Default)]
struct TokenUsage {
    input_tokens: u64,
    output_tokens: u64,
    cached_input_tokens: u64,
}

impl TokenUsage {
    fn or_total(&self, total: u64) -> u64 {
        if self.input_tokens == 0 && self.output_tokens == 0 && self.cached_input_tokens == 0 {
            total
        } else {
            self.input_tokens
        }
    }
}

fn token_usage_from_file(path: &Path) -> Result<TokenUsage> {
    let file =
        File::open(path).with_context(|| format!("open codex rollout {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut usage = TokenUsage::default();

    for line in reader.lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => continue,
        };
        let value: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let Some(total) = value
            .get("payload")
            .and_then(|payload| payload.get("info"))
            .and_then(|info| info.get("total_token_usage"))
        else {
            continue;
        };
        usage.input_tokens = json_u64(total, "input_tokens");
        usage.output_tokens = json_u64(total, "output_tokens");
        usage.cached_input_tokens = json_u64(total, "cached_input_tokens");
    }

    Ok(usage)
}

fn models_from_file(path: &Path) -> Result<super::ModelCollector> {
    let file =
        File::open(path).with_context(|| format!("open codex rollout {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut models = super::ModelCollector::default();

    for line in reader.lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => continue,
        };
        let value: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if value.get("type").and_then(|value| value.as_str()) != Some("turn_context") {
            continue;
        }
        if let Some(model) = value
            .get("payload")
            .and_then(|payload| payload.get("model"))
            .and_then(|model| model.as_str())
        {
            models.observe(model);
        }
    }

    Ok(models)
}

fn json_u64(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(|value| value.as_u64()).unwrap_or(0)
}

fn truncate(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (index, ch) in s.chars().enumerate() {
        if index >= max_chars {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn extracts_event_messages_when_available() {
        let input = r#"{"type":"event_msg","payload":{"type":"user_message","message":"hello codex"}}
{"type":"event_msg","payload":{"type":"agent_message","message":"hi user"}}
{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"duplicate"}]}}"#;

        let messages = extract_messages(Cursor::new(input)).expect("messages");

        assert_eq!(
            messages,
            vec![
                SourceMessage {
                    turn_index: 0,
                    role: "user".to_string(),
                    text: "hello codex".to_string(),
                },
                SourceMessage {
                    turn_index: 1,
                    role: "assistant".to_string(),
                    text: "hi user".to_string(),
                },
            ]
        );
    }

    #[test]
    fn falls_back_to_response_items() {
        let input = r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"hidden"}]}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"question"},{"type":"image","text":"ignored"}]}}
{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"reasoning","text":"hidden"},{"type":"output_text","text":"answer"}]}}"#;

        let messages = extract_messages(Cursor::new(input)).expect("messages");

        assert_eq!(
            messages,
            vec![
                SourceMessage {
                    turn_index: 0,
                    role: "user".to_string(),
                    text: "question".to_string(),
                },
                SourceMessage {
                    turn_index: 1,
                    role: "assistant".to_string(),
                    text: "answer".to_string(),
                },
            ]
        );
    }

    #[test]
    fn skips_internal_text() {
        let input = r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<bash-stdout>noise</bash-stdout>"},{"type":"input_text","text":"real request"}]}}"#;

        let messages = extract_messages(Cursor::new(input)).expect("messages");

        assert_eq!(messages[0].text, "real request");
    }

    fn write_model_fixture(tag: &str, body: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "ccsf-codex-model-test-{}-{tag}.jsonl",
            std::process::id()
        ));
        std::fs::write(&path, body).expect("write fixture");
        path
    }

    #[test]
    fn extracts_turn_context_model() {
        let path = write_model_fixture(
            "turn-context",
            r#"{"type":"session_meta","payload":{"model":null,"model_provider":"openai"}}
{"type":"turn_context","payload":{"model":"gpt-5.5"}}
{"type":"turn_context","payload":{"model":"gpt-5.5"}}"#,
        );

        let models = models_from_file(&path).expect("models");
        let _ = std::fs::remove_file(&path);

        assert_eq!(models.latest().as_deref(), Some("gpt-5.5"));
        assert_eq!(models.into_models(), vec!["gpt-5.5".to_string()]);
    }

    #[test]
    fn model_provider_alone_yields_no_concrete_model() {
        let path = write_model_fixture(
            "provider-only",
            r#"{"type":"session_meta","payload":{"model":null,"model_provider":"openai"}}
{"type":"turn_context","payload":{"cwd":"/repo"}}"#,
        );

        let models = models_from_file(&path).expect("models");
        let _ = std::fs::remove_file(&path);

        assert_eq!(models.latest(), None);
        assert!(models.into_models().is_empty());
    }

    #[test]
    fn latest_token_count_wins() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "ccsf-token-test-{}-{}.jsonl",
            std::process::id(),
            "latest"
        ));
        std::fs::write(
            &path,
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1,"output_tokens":2,"cached_input_tokens":3}}}}
{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"output_tokens":20,"cached_input_tokens":30}}}}"#,
        )
        .expect("write fixture");

        let usage = token_usage_from_file(&path).expect("usage");
        let _ = std::fs::remove_file(&path);

        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 20);
        assert_eq!(usage.cached_input_tokens, 30);
    }
}
