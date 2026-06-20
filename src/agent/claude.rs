use anyhow::Result;

use super::{AgentKind, SourceMessage, SourceRecord, SourceSession};

pub fn list_sessions() -> Result<Vec<SourceRecord>> {
    let root = crate::paths::claude_projects_root();
    let pattern = format!("{}/*/*.jsonl", root.to_string_lossy());
    let mut out = Vec::new();
    for path in glob::glob(&pattern)?.flatten() {
        let Some(session_id) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        out.push(SourceRecord {
            agent: AgentKind::Claude,
            session_id: session_id.to_string(),
            mtime: file_mtime(&metadata),
            size: metadata.len() as i64,
            path,
        });
    }
    Ok(out)
}

pub fn extract_session(record: &SourceRecord) -> Result<(SourceSession, Vec<SourceMessage>)> {
    let meta = crate::session::extract_from_file(&record.path)?;
    let messages = crate::session::extract_indexable_messages_from_file(&record.path)?
        .into_iter()
        .map(|message| SourceMessage {
            turn_index: message.turn_index,
            role: message.role,
            text: message.text,
        })
        .collect();

    Ok((
        SourceSession {
            native_session_id: meta.session_id.clone(),
            session_id: meta.session_id,
            agent: AgentKind::Claude,
            source_group: Some(meta.project_dir),
            cwd: meta.cwd,
            ai_title: meta.ai_title,
            first_prompt: meta.first_prompt,
            msg_count: meta.msg_count,
            mtime: meta.mtime,
            size: meta.size,
            file_path: meta.file_path,
            git_branch: meta.git_branch,
            pr_number: meta.pr_number,
            pr_url: meta.pr_url,
            pr_repo: meta.pr_repo,
            tokens_input: meta.tokens_input,
            tokens_output: meta.tokens_output,
            tokens_cache_read: meta.tokens_cache_read,
            tokens_cache_create: meta.tokens_cache_create,
            model: meta.model,
            models: meta.models,
            tool_call_count: meta.tool_call_count,
            tool_error_count: meta.tool_error_count,
            thinking_tokens: meta.thinking_tokens,
            wall_clock_ms: meta.wall_clock_ms,
        },
        messages,
    ))
}

fn file_mtime(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
