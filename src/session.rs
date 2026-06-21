//! Claude Code JSONL session file parser.

use std::collections::{HashMap, HashSet};
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

/// One step (`trajectory` row) of a session: a tool call, an assistant turn
/// without tool use, an API error, or a context-compaction event. Tool inputs
/// are stored in full up to [`TOOL_INPUT_CAP_BYTES`]; `tool_input_bytes` keeps
/// the original size so truncation is detectable. Tool result bodies are stored
/// only when [`store_tool_results_enabled`] is set; `tool_result_bytes` is
/// always recorded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrajectoryStep {
    pub step_index: u32,
    /// 0-based position of this step within its autonomous run — the count of
    /// steps since the last human turn. 0 marks the first action taken right
    /// after human input; a higher value means a longer uninterrupted run.
    pub autonomous_run_index: u32,
    pub role: String,
    pub tool_name: Option<String>,
    pub tool_input: Option<String>,
    pub tool_input_bytes: u64,
    pub tool_result_bytes: u64,
    pub tool_result: Option<String>,
    pub is_error: bool,
    pub tokens_input: u64,
    pub tokens_output: u64,
    pub tokens_cache_read: u64,
    pub tokens_cache_create: u64,
    pub timestamp: Option<i64>,
    pub is_sidechain: bool,
    pub context_management: Option<String>,
    pub is_api_error: bool,
    pub api_error_status: Option<i64>,
    pub retry_attempt: Option<i64>,
    pub max_retries: Option<i64>,
    pub stop_reason: Option<String>,
    pub attribution_mcp_tool: Option<String>,
    pub attribution_mcp_server: Option<String>,
    pub attribution_skill: Option<String>,
    pub duration_ms: Option<i64>,
    pub permission_mode: Option<String>,
    pub parent_uuid: Option<String>,
}

/// Max bytes of a `tool_use` input stored verbatim; larger inputs are truncated
/// on a char boundary while `tool_input_bytes` keeps the original size.
pub const TOOL_INPUT_CAP_BYTES: usize = 128 * 1024;
/// Max bytes of an opt-in `tool_result` body stored verbatim.
pub const TOOL_RESULT_CAP_BYTES: usize = 128 * 1024;

/// Whether `tool_result` bodies should be stored in the `trajectory` table.
/// Off by default (results can run ~45 MB/month); opt in by setting
/// `CC_SESSION_FINDER_STORE_TOOL_RESULTS` to a truthy value (`1`/`true`/`yes`).
pub fn store_tool_results_enabled() -> bool {
    std::env::var("CC_SESSION_FINDER_STORE_TOOL_RESULTS")
        .map(|value| {
            let value = value.trim().to_ascii_lowercase();
            matches!(value.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(false)
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

/// Parse a Claude JSONL session into ordered [`TrajectoryStep`]s. Tokens are
/// attributed once per `message.id` (the same dedup rule as session totals), so
/// the per-step sums match [`SessionMeta`]. `store_tool_results` opts into
/// keeping result bodies; otherwise only `tool_result_bytes` is recorded.
pub fn extract_trajectory_from_file(
    path: &Path,
    store_tool_results: bool,
) -> Result<Vec<TrajectoryStep>> {
    let f = File::open(path)?;
    let reader = BufReader::new(f);
    let mut records: Vec<Value> = Vec::new();
    for line in reader.lines() {
        let Ok(line) = line else { continue };
        if let Ok(v) = serde_json::from_str::<Value>(&line) {
            records.push(v);
        }
    }
    Ok(build_trajectory(&records, store_tool_results))
}

/// The portion of a `tool_result` block carried back to its originating
/// `tool_use` step.
#[derive(Debug, Default, Clone)]
struct ToolResultInfo {
    is_error: bool,
    bytes: u64,
    body: Option<String>,
    mcp_server: Option<String>,
    mcp_tool: Option<String>,
    skill: Option<String>,
}

fn build_trajectory(records: &[Value], store_tool_results: bool) -> Vec<TrajectoryStep> {
    let results = collect_tool_results(records, store_tool_results);

    let mut steps: Vec<TrajectoryStep> = Vec::new();
    let mut counted_usage_ids: HashSet<String> = HashSet::new();
    let mut group_start = 0usize;
    // Steps emitted since the last human turn. Reset to 0 on each human turn so
    // that every step records its 0-based position within its autonomous run.
    let mut run: u32 = 0;

    while group_start < records.len() {
        let record = &records[group_start];
        let ty = record.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let before = steps.len();

        match ty {
            "assistant" if is_api_error_record(record) => {
                steps.push(api_error_step(record, &mut counted_usage_ids));
                group_start += 1;
            }
            "assistant" => {
                let group_end = assistant_group_end(records, group_start);
                emit_assistant_group(
                    &records[group_start..group_end],
                    &results,
                    &mut counted_usage_ids,
                    &mut steps,
                );
                group_start = group_end;
            }
            "user" if is_compact_summary(record) => {
                steps.push(compaction_step(record));
                group_start += 1;
            }
            _ => {
                // A real human turn resets the autonomous run; tool_result and
                // other non-human user records do not.
                if is_human_turn(record) {
                    run = 0;
                }
                group_start += 1;
            }
        }

        for step in &mut steps[before..] {
            step.autonomous_run_index = run;
            run += 1;
        }
    }

    for (index, step) in steps.iter_mut().enumerate() {
        step.step_index = index as u32;
    }
    fill_durations(&mut steps);
    steps
}

/// Map each `tool_use_id` to its result: error flag, body size, optional body,
/// and the MCP / skill attribution recorded on the result-bearing record.
fn collect_tool_results(
    records: &[Value],
    store_tool_results: bool,
) -> HashMap<String, ToolResultInfo> {
    let mut map = HashMap::new();
    for record in records {
        if record.get("type").and_then(|t| t.as_str()) != Some("user") {
            continue;
        }
        let Some(parts) = record
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        else {
            continue;
        };
        let mcp_server = string_field(record, "attributionMcpServer");
        let mcp_tool = string_field(record, "attributionMcpTool");
        let skill = string_field(record, "attributionSkill");
        for part in parts {
            if part.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
                continue;
            }
            let Some(id) = part.get("tool_use_id").and_then(|i| i.as_str()) else {
                continue;
            };
            let (bytes, body) = result_content_size(part.get("content"), store_tool_results);
            map.insert(
                id.to_string(),
                ToolResultInfo {
                    is_error: part.get("is_error").and_then(|e| e.as_bool()) == Some(true),
                    bytes,
                    body,
                    mcp_server: mcp_server.clone(),
                    mcp_tool: mcp_tool.clone(),
                    skill: skill.clone(),
                },
            );
        }
    }
    map
}

/// Byte size of a `tool_result` content payload, plus an optional capped body.
fn result_content_size(content: Option<&Value>, store: bool) -> (u64, Option<String>) {
    let Some(content) = content else {
        return (0, None);
    };
    let text = match content.as_str() {
        Some(s) => s.to_string(),
        None => serde_json::to_string(content).unwrap_or_default(),
    };
    let bytes = text.len() as u64;
    let body = store.then(|| cap_bytes(&text, TOOL_RESULT_CAP_BYTES));
    (bytes, body)
}

/// Index one past the last consecutive `assistant` record sharing the same
/// `message.id` (a single API response split across thinking / text / tool_use
/// rows). Records without an id form a singleton group.
fn assistant_group_end(records: &[Value], start: usize) -> usize {
    let id = message_id(&records[start]);
    let mut end = start + 1;
    if id.is_none() {
        return end;
    }
    while end < records.len() {
        let record = &records[end];
        if record.get("type").and_then(|t| t.as_str()) != Some("assistant")
            || is_api_error_record(record)
            || message_id(record) != id
        {
            break;
        }
        end += 1;
    }
    end
}

fn emit_assistant_group(
    group: &[Value],
    results: &HashMap<String, ToolResultInfo>,
    counted_usage_ids: &mut HashSet<String>,
    steps: &mut Vec<TrajectoryStep>,
) {
    let head = &group[0];
    let id = message_id(head);
    // Read usage from the head record only, matching the session-total rule
    // (the first record bearing an id wins the dedup), so per-step token sums
    // equal the session aggregates exactly.
    let usage = head.get("message").and_then(|m| m.get("usage"));
    let stop_reason = group
        .iter()
        .rev()
        .find_map(|r| r.get("message").and_then(|m| m.get("stop_reason")))
        .and_then(|s| s.as_str())
        .map(str::to_string);

    // Attribute usage once per message.id (rows lacking an id cannot be deduped
    // and are each counted, matching the session-level totals).
    let carries_tokens = match id {
        Some(id) => counted_usage_ids.insert(id),
        None => true,
    };

    let tool_uses: Vec<&Value> = group
        .iter()
        .filter_map(|r| r.get("message").and_then(|m| m.get("content")))
        .filter_map(|c| c.as_array())
        .flatten()
        .filter(|part| part.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
        .collect();

    let mut first = true;
    let mut push = |mut step: TrajectoryStep, steps: &mut Vec<TrajectoryStep>| {
        if first && carries_tokens {
            if let Some(usage) = usage {
                step.tokens_input = usage_u64(usage, "input_tokens");
                step.tokens_output = usage_u64(usage, "output_tokens");
                step.tokens_cache_read = usage_u64(usage, "cache_read_input_tokens");
                step.tokens_cache_create = usage_u64(usage, "cache_creation_input_tokens");
            }
        }
        first = false;
        steps.push(step);
    };

    if tool_uses.is_empty() {
        let mut step = base_step(head, "assistant");
        step.stop_reason = stop_reason;
        push(step, steps);
        return;
    }

    for tool_use in tool_uses {
        let mut step = base_step(head, "assistant");
        step.stop_reason = stop_reason.clone();
        step.tool_name = tool_use
            .get("name")
            .and_then(|n| n.as_str())
            .map(str::to_string);
        if let Some(input) = tool_use.get("input") {
            let serialized = serde_json::to_string(input).unwrap_or_default();
            step.tool_input_bytes = serialized.len() as u64;
            step.tool_input = Some(cap_bytes(&serialized, TOOL_INPUT_CAP_BYTES));
        }
        if let Some(result) = tool_use
            .get("id")
            .and_then(|i| i.as_str())
            .and_then(|id| results.get(id))
        {
            step.is_error = result.is_error;
            step.tool_result_bytes = result.bytes;
            step.tool_result = result.body.clone();
            step.attribution_mcp_server = result.mcp_server.clone();
            step.attribution_mcp_tool = result.mcp_tool.clone();
            step.attribution_skill = result.skill.clone();
        }
        // MCP attribution can also be inferred from the tool name when the
        // result record did not carry it (`mcp__server__tool`).
        if step.attribution_mcp_server.is_none() {
            if let Some((server, tool)) = step.tool_name.as_deref().and_then(parse_mcp_tool_name) {
                step.attribution_mcp_server = Some(server);
                step.attribution_mcp_tool = Some(tool);
            }
        }
        push(step, steps);
    }
}

/// Shared per-record metadata (sidechain, permission mode, parent, timestamp)
/// applied to every step derived from `record`.
fn base_step(record: &Value, role: &str) -> TrajectoryStep {
    TrajectoryStep {
        role: role.to_string(),
        timestamp: record
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(parse_timestamp_ms),
        is_sidechain: record.get("isSidechain").and_then(|s| s.as_bool()) == Some(true),
        permission_mode: string_field(record, "permissionMode"),
        parent_uuid: string_field(record, "parentUuid"),
        ..Default::default()
    }
}

fn api_error_step(record: &Value, counted_usage_ids: &mut HashSet<String>) -> TrajectoryStep {
    let mut step = base_step(record, "assistant");
    step.is_api_error = true;
    let text = record
        .get("message")
        .and_then(|m| m.get("content"))
        .map(value_to_text)
        .unwrap_or_default();
    step.api_error_status = parse_http_status(&text);
    step.stop_reason = record
        .get("message")
        .and_then(|m| m.get("stop_reason"))
        .and_then(|s| s.as_str())
        .map(str::to_string);
    // API-error records are `type: "assistant"` in the session-total loop, so
    // attribute usage by the same rule: dedup by id, but always count records
    // that lack an id.
    let carries_tokens = match message_id(record) {
        Some(id) => counted_usage_ids.insert(id),
        None => true,
    };
    if carries_tokens {
        if let Some(usage) = record.get("message").and_then(|m| m.get("usage")) {
            step.tokens_input = usage_u64(usage, "input_tokens");
            step.tokens_output = usage_u64(usage, "output_tokens");
            step.tokens_cache_read = usage_u64(usage, "cache_read_input_tokens");
            step.tokens_cache_create = usage_u64(usage, "cache_creation_input_tokens");
        }
    }
    step
}

fn compaction_step(record: &Value) -> TrajectoryStep {
    let mut step = base_step(record, "system");
    step.context_management = Some("compact_summary".to_string());
    step
}

/// Set `duration_ms` for each step from the gap to the next step's timestamp.
/// The final step (and any with no usable neighbour) is left unset.
fn fill_durations(steps: &mut [TrajectoryStep]) {
    for i in 0..steps.len() {
        let Some(current) = steps[i].timestamp else {
            continue;
        };
        if let Some(next) = steps[i + 1..].iter().find_map(|s| s.timestamp) {
            if next >= current {
                steps[i].duration_ms = Some(next - current);
            }
        }
    }
}

fn message_id(record: &Value) -> Option<String> {
    record
        .get("message")
        .and_then(|m| m.get("id"))
        .and_then(|i| i.as_str())
        .map(str::to_string)
}

fn is_api_error_record(record: &Value) -> bool {
    record.get("isApiErrorMessage").and_then(|e| e.as_bool()) == Some(true)
}

fn is_compact_summary(record: &Value) -> bool {
    record.get("isCompactSummary").and_then(|c| c.as_bool()) == Some(true)
}

fn string_field(record: &Value, key: &str) -> Option<String> {
    record
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Split an `mcp__server__tool` name into its server and tool parts.
fn parse_mcp_tool_name(name: &str) -> Option<(String, String)> {
    let rest = name.strip_prefix("mcp__")?;
    let (server, tool) = rest.split_once("__")?;
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    Some((server.to_string(), tool.to_string()))
}

/// Flatten a message `content` value (string or block array) into plain text,
/// used to scrape API error messages.
fn value_to_text(content: &Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    let Some(parts) = content.as_array() else {
        return String::new();
    };
    parts
        .iter()
        .filter_map(|part| part.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Pull the first 3-digit HTTP status code out of an API error string.
fn parse_http_status(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 3 <= bytes.len() {
        if bytes[i].is_ascii_digit() {
            let run_end = bytes[i..].iter().take_while(|b| b.is_ascii_digit()).count();
            if run_end == 3 {
                if let Ok(code) = text[i..i + 3].parse::<i64>() {
                    if (100..=599).contains(&code) {
                        return Some(code);
                    }
                }
            }
            i += run_end.max(1);
        } else {
            i += 1;
        }
    }
    None
}

/// Truncate `s` to at most `max` bytes on a char boundary.
fn cap_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
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

/// A real human turn: a `user` record carrying human-visible text. This matches
/// exactly the `user` rows stored in `messages`, so run boundaries line up with
/// the human turns counted elsewhere. Tool-result and other synthetic `user`
/// records carry no conversation text and are not human turns.
fn is_human_turn(v: &Value) -> bool {
    matches!(indexable_message_text(v), Some((role, _)) if role == "user")
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

    fn trajectory(lines: &[Value]) -> Vec<TrajectoryStep> {
        build_trajectory(lines, false)
    }

    #[test]
    fn autonomous_run_index_resets_on_human_turn() {
        let assistant = |id: &str, tool: &str| {
            json!({"type":"assistant","message":{"id":id,"content":[
                {"type":"tool_use","id":tool,"name":"Bash","input":{"cmd":"x"}}]}})
        };
        let human = |text: &str| json!({"type":"user","message":{"role":"user","content":text}});
        // tool_result user record: must NOT reset the run.
        let tool_result = json!({"type":"user","message":{"content":[
            {"type":"tool_result","tool_use_id":"a1","content":"ok"}]}});

        let steps = trajectory(&[
            human("first prompt"),
            assistant("m1", "a1"),
            tool_result,
            assistant("m2", "a2"),
            human("second prompt"),
            assistant("m3", "a3"),
        ]);

        let runs: Vec<u32> = steps.iter().map(|s| s.autonomous_run_index).collect();
        // run 1: two assistant steps (the tool_result is not a step and does not
        // reset); run 2: resets to 0 after the second human turn.
        assert_eq!(runs, vec![0, 1, 0]);
    }

    #[test]
    fn autonomous_run_index_counts_leading_run_without_human_turn() {
        let steps = trajectory(&[
            json!({"type":"assistant","message":{"id":"m1","content":[
                {"type":"tool_use","id":"t1","name":"Read","input":{}}]}}),
            json!({"type":"assistant","message":{"id":"m2","content":[
                {"type":"tool_use","id":"t2","name":"Edit","input":{}}]}}),
        ]);

        let runs: Vec<u32> = steps.iter().map(|s| s.autonomous_run_index).collect();
        assert_eq!(runs, vec![0, 1]);
    }

    #[test]
    fn trajectory_emits_one_step_per_tool_use_with_token_dedup() {
        // One API response split across thinking / text / two tool_use rows,
        // all sharing message id msg_A and repeating the same usage.
        let usage = json!({
            "input_tokens": 100, "output_tokens": 10,
            "cache_read_input_tokens": 1000, "cache_creation_input_tokens": 50
        });
        let steps = trajectory(&[
            json!({"type":"assistant","timestamp":"2026-06-20T01:00:00.000Z","message":{"id":"msg_A","usage":usage,"stop_reason":"tool_use","content":[{"type":"thinking","thinking":"x"}]}}),
            json!({"type":"assistant","timestamp":"2026-06-20T01:00:00.000Z","message":{"id":"msg_A","usage":usage,"content":[{"type":"text","text":"y"}]}}),
            json!({"type":"assistant","timestamp":"2026-06-20T01:00:01.000Z","message":{"id":"msg_A","usage":usage,"content":[{"type":"tool_use","id":"t1","name":"Edit","input":{"path":"a"}}]}}),
            json!({"type":"assistant","timestamp":"2026-06-20T01:00:02.000Z","message":{"id":"msg_A","usage":usage,"content":[{"type":"tool_use","id":"t2","name":"Bash","input":{"cmd":"ls"}}]}}),
        ]);

        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].step_index, 0);
        assert_eq!(steps[0].tool_name.as_deref(), Some("Edit"));
        assert_eq!(steps[1].tool_name.as_deref(), Some("Bash"));
        // Stop reason from the group applies to every step.
        assert_eq!(steps[0].stop_reason.as_deref(), Some("tool_use"));
        // Usage attributed once: first step carries it, second is zero.
        assert_eq!(steps[0].tokens_input, 100);
        assert_eq!(steps[0].tokens_cache_read, 1000);
        assert_eq!(steps[1].tokens_input, 0);
        assert_eq!(steps[1].tokens_output, 0);
    }

    #[test]
    fn trajectory_token_sum_matches_session_totals() {
        let body = r#"{"type":"assistant","message":{"id":"msg_A","usage":{"input_tokens":100,"output_tokens":10,"cache_read_input_tokens":1000,"cache_creation_input_tokens":50},"content":[{"type":"thinking","thinking":"x"}]}}
{"type":"assistant","message":{"id":"msg_A","usage":{"input_tokens":100,"output_tokens":10,"cache_read_input_tokens":1000,"cache_creation_input_tokens":50},"content":[{"type":"tool_use","id":"t1","name":"Edit","input":{}}]}}
{"type":"assistant","message":{"id":"msg_B","usage":{"input_tokens":5,"output_tokens":2,"cache_read_input_tokens":3,"cache_creation_input_tokens":1},"content":[{"type":"text","text":"z"}]}}"#;
        let path = write_fixture("traj-sum", body);
        let meta = extract_from_file(&path).expect("meta");
        let steps = extract_trajectory_from_file(&path, false).expect("steps");
        let _ = std::fs::remove_file(&path);

        let sum = |f: fn(&TrajectoryStep) -> u64| steps.iter().map(f).sum::<u64>();
        assert_eq!(sum(|s| s.tokens_input), meta.tokens_input);
        assert_eq!(sum(|s| s.tokens_output), meta.tokens_output);
        assert_eq!(sum(|s| s.tokens_cache_read), meta.tokens_cache_read);
        assert_eq!(sum(|s| s.tokens_cache_create), meta.tokens_cache_create);
    }

    #[test]
    fn trajectory_stores_tool_input_full_and_caps_oversized() {
        let small = trajectory(&[json!({"type":"assistant","message":{"id":"m","content":[
            {"type":"tool_use","id":"t","name":"Edit","input":{"path":"src/x.rs","text":"hello"}}
        ]}})]);
        let input = small[0].tool_input.as_deref().unwrap();
        assert!(input.contains("src/x.rs") && input.contains("hello"));
        assert_eq!(small[0].tool_input_bytes, input.len() as u64);

        let huge = "a".repeat(TOOL_INPUT_CAP_BYTES * 2);
        let big = trajectory(&[json!({"type":"assistant","message":{"id":"m","content":[
            {"type":"tool_use","id":"t","name":"Bash","input":{"cmd":huge}}
        ]}})]);
        let stored = big[0].tool_input.as_deref().unwrap();
        assert!(stored.len() <= TOOL_INPUT_CAP_BYTES);
        // Original size is preserved even though the body was truncated.
        assert!(big[0].tool_input_bytes > TOOL_INPUT_CAP_BYTES as u64);
    }

    #[test]
    fn trajectory_attributes_error_and_attribution_from_tool_result() {
        let steps = trajectory(&[
            json!({"type":"assistant","message":{"id":"m","content":[
                {"type":"tool_use","id":"call_1","name":"Bash","input":{"cmd":"false"}}
            ]}}),
            json!({"type":"user","attributionSkill":"deploy","message":{"content":[
                {"type":"tool_result","tool_use_id":"call_1","is_error":true,"content":"boom"}
            ]}}),
        ]);

        assert_eq!(steps.len(), 1);
        assert!(steps[0].is_error);
        assert_eq!(steps[0].tool_result_bytes, 4);
        assert_eq!(steps[0].attribution_skill.as_deref(), Some("deploy"));
        // Body is not stored unless opted in.
        assert!(steps[0].tool_result.is_none());
    }

    #[test]
    fn trajectory_stores_tool_result_body_when_opted_in() {
        let steps = build_trajectory(
            &[
                json!({"type":"assistant","message":{"id":"m","content":[
                    {"type":"tool_use","id":"c","name":"Bash","input":{}}
                ]}}),
                json!({"type":"user","message":{"content":[
                    {"type":"tool_result","tool_use_id":"c","content":"output text"}
                ]}}),
            ],
            true,
        );
        assert_eq!(steps[0].tool_result.as_deref(), Some("output text"));
        assert_eq!(steps[0].tool_result_bytes, 11);
    }

    #[test]
    fn trajectory_infers_mcp_attribution_from_tool_name() {
        let steps = trajectory(&[json!({"type":"assistant","message":{"id":"m","content":[
            {"type":"tool_use","id":"t","name":"mcp__plugin_firebase__list_apps","input":{}}
        ]}})]);
        assert_eq!(
            steps[0].attribution_mcp_server.as_deref(),
            Some("plugin_firebase")
        );
        assert_eq!(steps[0].attribution_mcp_tool.as_deref(), Some("list_apps"));
    }

    #[test]
    fn trajectory_captures_sidechain_compaction_and_api_error() {
        let steps = trajectory(&[
            json!({"type":"assistant","isSidechain":true,"permissionMode":"plan","message":{"id":"m1","content":[
                {"type":"tool_use","id":"t","name":"Read","input":{}}
            ]}}),
            json!({"type":"user","isCompactSummary":true,"message":{"content":[{"type":"text","text":"summary"}]}}),
            json!({"type":"assistant","isApiErrorMessage":true,"message":{"stop_reason":"stop_sequence","content":"API Error: 400 bad request"}}),
        ]);

        assert_eq!(steps.len(), 3);
        assert!(steps[0].is_sidechain);
        assert_eq!(steps[0].permission_mode.as_deref(), Some("plan"));
        assert_eq!(steps[1].role, "system");
        assert_eq!(
            steps[1].context_management.as_deref(),
            Some("compact_summary")
        );
        assert!(steps[2].is_api_error);
        assert_eq!(steps[2].api_error_status, Some(400));
        assert_eq!(steps[2].stop_reason.as_deref(), Some("stop_sequence"));
    }

    #[test]
    fn trajectory_assistant_turn_without_tool_use_keeps_token_attribution() {
        let steps = trajectory(&[json!({"type":"assistant","message":{
            "id":"m","usage":{"input_tokens":7,"output_tokens":3},
            "content":[{"type":"text","text":"final answer"}]
        }})]);
        assert_eq!(steps.len(), 1);
        assert!(steps[0].tool_name.is_none());
        assert_eq!(steps[0].tokens_input, 7);
        assert_eq!(steps[0].tokens_output, 3);
    }

    #[test]
    fn trajectory_token_attribution_matches_totals_on_api_error_without_id() {
        // An API-error assistant record carrying usage but no message.id is
        // counted by the session-total loop, so the trajectory must count it
        // too (no-id records are always attributed).
        let body = r#"{"type":"assistant","isApiErrorMessage":true,"message":{"usage":{"input_tokens":9,"output_tokens":4,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"stop_reason":"stop_sequence","content":"API Error: 529 overloaded"}}"#;
        let path = write_fixture("traj-apierr-noid", body);
        let meta = extract_from_file(&path).expect("meta");
        let steps = extract_trajectory_from_file(&path, false).expect("steps");
        let _ = std::fs::remove_file(&path);

        assert_eq!(steps.len(), 1);
        assert!(steps[0].is_api_error);
        assert_eq!(steps[0].api_error_status, Some(529));
        assert_eq!(steps[0].tokens_input, meta.tokens_input);
        assert_eq!(steps[0].tokens_output, meta.tokens_output);
        assert_eq!(steps[0].tokens_input, 9);
    }

    #[test]
    fn trajectory_derives_duration_from_timestamp_gaps() {
        let steps = trajectory(&[
            json!({"type":"assistant","timestamp":"2026-06-20T01:00:00.000Z","message":{"id":"a","content":[{"type":"tool_use","id":"t1","name":"Read","input":{}}]}}),
            json!({"type":"assistant","timestamp":"2026-06-20T01:00:02.500Z","message":{"id":"b","content":[{"type":"tool_use","id":"t2","name":"Edit","input":{}}]}}),
        ]);
        assert_eq!(steps[0].duration_ms, Some(2_500));
        // Last step has no following timestamp to measure against.
        assert_eq!(steps[1].duration_ms, None);
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
