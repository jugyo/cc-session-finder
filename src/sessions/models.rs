//! MCP-first response models shared by the `sessions` CLI harness and the MCP
//! server. These intentionally omit native/debug fields (native session ids,
//! source paths, raw scores, index update status) so the same shapes are safe
//! to expose over MCP.

use schemars::JsonSchema;
use serde::Serialize;

use crate::agent::AgentKind;
use crate::index::search::Hit;

/// Drop non-standard integer-width `format` annotations (e.g. `uint32`,
/// `int64`) that schemars emits from Rust integer types. JSON has no integer
/// width, so the annotation is meaningless on the wire and only makes MCP
/// clients warn about an unknown `format`. Applied as a schemars container
/// transform to types with integer fields.
pub(crate) fn strip_int_formats(schema: &mut schemars::Schema) {
    let Some(properties) = schema
        .as_object_mut()
        .and_then(|object| object.get_mut("properties"))
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    for value in properties.values_mut() {
        let Some(field) = value.as_object_mut() else {
            continue;
        };
        let is_width_format = field
            .get("format")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|format| format.starts_with("int") || format.starts_with("uint"));
        if is_width_format {
            field.remove("format");
        }
    }
}

/// Default and capped limits. Character caps are fixed internally and not
/// exposed as knobs.
pub const SEARCH_LIMIT_DEFAULT: usize = 20;
pub const SEARCH_LIMIT_CAP: usize = 100;
pub const MATCHES_PER_SESSION: usize = 3;
pub const MESSAGES_LIMIT_DEFAULT: usize = 10;
pub const MESSAGES_LIMIT_CAP: usize = 30;
pub const SEARCH_MESSAGES_LIMIT_DEFAULT: usize = 10;
pub const SEARCH_MESSAGES_LIMIT_CAP: usize = 30;
pub const TRAJECTORY_LIMIT_DEFAULT: usize = 30;
pub const TRAJECTORY_LIMIT_CAP: usize = 100;

const CARD_TEXT_CAP: usize = 1200;
const OVERVIEW_TEXT_CAP: usize = 1200;
const MESSAGES_TEXT_CAP: usize = 2000;
const FIRST_PROMPT_CAP: usize = 500;
/// Char cap applied to `tool_input` / `tool_result` text in trajectory reads,
/// independent of the larger byte cap used when storing them.
pub(crate) const TRAJECTORY_TEXT_CAP: usize = 2000;

/// Cap used for matching / latest / first messages embedded in search cards and
/// overviews.
pub(crate) const CARD_MESSAGE_CAP: usize = CARD_TEXT_CAP;
pub(crate) const OVERVIEW_MESSAGE_CAP: usize = OVERVIEW_TEXT_CAP;
/// Cap used for full message paging and session-local message search.
pub(crate) const FULL_MESSAGE_CAP: usize = MESSAGES_TEXT_CAP;

/// A single visible user/assistant message. `message_index` maps to the DB
/// `turn_index` column.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
#[schemars(transform = strip_int_formats)]
pub struct SessionMessage {
    pub message_index: u32,
    pub role: String,
    pub text: String,
    pub truncated: bool,
}

impl SessionMessage {
    pub(crate) fn new(message_index: u32, role: String, text: String, cap: usize) -> Self {
        let (text, truncated) = truncate_text(&text, cap);
        Self {
            message_index,
            role,
            text,
            truncated,
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(transform = strip_int_formats)]
pub struct SessionMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    pub tokens_total: u64,
    pub message_count: u32,
    pub tool_call_count: u64,
    pub tool_error_count: u64,
    pub thinking_tokens: u64,
    pub wall_clock_ms: i64,
}

/// Compact session card returned by `search_sessions`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SessionCard {
    pub id: String,
    pub agent: AgentKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub cwd: String,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_prompt: Option<String>,
    pub match_reasons: Vec<String>,
    pub matches: Vec<SessionMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_message: Option<SessionMessage>,
    pub metadata: SessionMetadata,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(transform = strip_int_formats)]
pub struct SearchResponse {
    pub results: Vec<SessionCard>,
    pub count: usize,
    pub has_more: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// Lightweight session reference used in message-returning responses.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SessionRef {
    pub id: String,
    pub agent: AgentKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct OverviewSession {
    pub id: String,
    pub agent: AgentKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub cwd: String,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_prompt: Option<String>,
    pub metadata: SessionMetadata,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(transform = strip_int_formats)]
pub struct OverviewResponse {
    pub session: OverviewSession,
    pub first_message: Option<SessionMessage>,
    pub latest_message: Option<SessionMessage>,
    pub message_count: u32,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MessagesResponse {
    pub session: SessionRef,
    pub messages: Vec<SessionMessage>,
    pub has_more_before: bool,
    pub has_more_after: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(transform = strip_int_formats)]
pub struct MessageSearchResponse {
    pub session: SessionRef,
    pub matches: Vec<SessionMessage>,
    pub count: usize,
}

/// One `trajectory` step exposed over the read API. Long `tool_input` /
/// `tool_result` text is capped to [`TRAJECTORY_TEXT_CAP`] chars; `*_bytes`
/// fields report the original stored size.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
#[schemars(transform = strip_int_formats)]
pub struct TrajectoryStepView {
    pub step_index: u32,
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_input: Option<String>,
    pub tool_input_truncated: bool,
    pub tool_input_bytes: u64,
    pub tool_result_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_result: Option<String>,
    pub is_error: bool,
    pub tokens_input: u64,
    pub tokens_output: u64,
    pub tokens_cache_read: u64,
    pub tokens_cache_create: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<i64>,
    pub is_sidechain: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_management: Option<String>,
    pub is_api_error: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_error_status: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_attempt: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attribution_mcp_tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attribution_mcp_server: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attribution_skill: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_uuid: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(transform = strip_int_formats)]
pub struct TrajectoryResponse {
    pub session: SessionRef,
    pub steps: Vec<TrajectoryStepView>,
    pub count: usize,
    pub has_more: bool,
}

/// One row of [`super::find_inefficient_sessions`]: a session reduced to the
/// efficiency signals used to surface outliers. Carries no message text or tool
/// bodies, only counts and ratios.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(transform = strip_int_formats)]
pub struct InefficientSession {
    pub id: String,
    pub agent: AgentKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub cwd: String,
    pub updated_at: String,
    /// `input + output + cache_create`. Excludes the cheap `cache_read` bucket,
    /// which is surfaced separately via `cache_read_ratio`.
    pub billable_tokens: u64,
    pub tokens_output: u64,
    pub tokens_cache_read: u64,
    /// `cache_read / max(output, 1)`. Large values flag sessions that re-read a
    /// big context for little new output (e.g. tight sub-agent loops).
    pub cache_read_ratio: f64,
    pub tool_call_count: u64,
    pub tool_error_count: u64,
    /// `tool_error_count / max(tool_call_count, 1)`.
    pub error_rate: f64,
    pub thinking_tokens: u64,
    pub wall_clock_ms: i64,
    pub message_count: u32,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[schemars(transform = strip_int_formats)]
pub struct InefficientSessionsResponse {
    pub sort_by: String,
    pub results: Vec<InefficientSession>,
    pub count: usize,
}

/// Clamp a requested limit to `[1, max]`, falling back to `default` when not
/// provided or zero.
pub(crate) fn cap_limit(requested: Option<usize>, default: usize, max: usize) -> usize {
    match requested {
        None => default,
        Some(0) => default,
        Some(n) => n.min(max),
    }
}

/// Truncate to `max` characters on a char boundary, reporting whether the input
/// was cut. A `max` of 0 leaves the text untouched.
pub(crate) fn truncate_text(s: &str, max: usize) -> (String, bool) {
    if max == 0 || s.chars().count() <= max {
        return (s.to_string(), false);
    }
    let truncated: String = s.chars().take(max).collect();
    (truncated, true)
}

/// RFC3339 timestamp in the local offset, falling back to UTC then the raw
/// epoch seconds.
pub(crate) fn format_updated_at(mtime: i64) -> String {
    use time::{format_description::well_known::Rfc3339, OffsetDateTime, UtcOffset};
    let Ok(utc) = OffsetDateTime::from_unix_timestamp(mtime) else {
        return mtime.to_string();
    };
    let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    utc.to_offset(offset)
        .format(&Rfc3339)
        .unwrap_or_else(|_| mtime.to_string())
}

/// Sum of all token buckets recorded for a session. Coarse signal for an LLM,
/// not a billing figure.
pub(crate) fn tokens_total(hit: &Hit) -> u64 {
    hit.tokens_input
        .saturating_add(hit.tokens_output)
        .saturating_add(hit.tokens_cache_read)
        .saturating_add(hit.tokens_cache_create)
}

pub(crate) fn metadata_from_hit(hit: &Hit, message_count: u32) -> SessionMetadata {
    SessionMetadata {
        git_branch: hit.git_branch.clone(),
        pr_number: hit.pr_number,
        pr_url: hit.pr_url.clone(),
        pr_repo: hit.pr_repo.clone(),
        model: hit.model.clone(),
        models: hit.models.clone(),
        tokens_total: tokens_total(hit),
        message_count,
        tool_call_count: hit.tool_call_count,
        tool_error_count: hit.tool_error_count,
        thinking_tokens: hit.thinking_tokens,
        wall_clock_ms: hit.wall_clock_ms,
    }
}

pub(crate) fn capped_first_prompt(first_prompt: Option<&str>) -> Option<String> {
    first_prompt.map(|p| {
        let (text, _) = truncate_text(p, FIRST_PROMPT_CAP);
        text
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_respects_char_boundaries() {
        let (text, truncated) = truncate_text("héllo wörld", 5);
        assert_eq!(text, "héllo");
        assert!(truncated);

        let (text, truncated) = truncate_text("short", 50);
        assert_eq!(text, "short");
        assert!(!truncated);

        let (text, truncated) = truncate_text("untouched", 0);
        assert_eq!(text, "untouched");
        assert!(!truncated);
    }

    #[test]
    fn cap_limit_clamps_and_defaults() {
        assert_eq!(cap_limit(None, 20, 100), 20);
        assert_eq!(cap_limit(Some(0), 20, 100), 20);
        assert_eq!(cap_limit(Some(5), 20, 100), 5);
        assert_eq!(cap_limit(Some(500), 20, 100), 100);
    }
}
