//! MCP-first core for exposing indexed sessions. The `sessions` CLI subcommand
//! group and the `mcp` server are thin wrappers over these functions; both emit
//! the exact same JSON shapes. Nothing here returns native session ids, source
//! paths, raw scores, or index update status.

pub mod models;
pub mod query;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::agent::AgentKind;
use crate::index::search::{self, Hit, TimeRange};

pub use models::{
    InefficientSession, InefficientSessionsResponse, MessageSearchResponse, MessagesResponse,
    OverviewResponse, OverviewSession, SearchResponse, SessionCard, SessionMessage, SessionRef,
    TrajectoryResponse,
};
pub use query::MessageOrder;

use models::{
    cap_limit, capped_first_prompt, format_updated_at, metadata_from_hit, CARD_MESSAGE_CAP,
    FULL_MESSAGE_CAP, MATCHES_PER_SESSION, MESSAGES_LIMIT_CAP, MESSAGES_LIMIT_DEFAULT,
    OVERVIEW_MESSAGE_CAP, SEARCH_LIMIT_CAP, SEARCH_LIMIT_DEFAULT, SEARCH_MESSAGES_LIMIT_CAP,
    SEARCH_MESSAGES_LIMIT_DEFAULT, TRAJECTORY_LIMIT_CAP, TRAJECTORY_LIMIT_DEFAULT,
};

/// Inputs for [`search_sessions`]. Callers parse `since`/`until` into a
/// [`TimeRange`] and resolve the effective cwd before calling.
#[derive(Debug, Clone, Default)]
pub struct SearchParams {
    pub query: Option<String>,
    pub limit: Option<usize>,
    pub cursor: Option<String>,
    pub cwd: Option<PathBuf>,
    pub cwd_only: bool,
    pub time_range: TimeRange,
}

/// Inputs for [`get_session_messages`].
#[derive(Debug, Clone)]
pub struct MessagesParams {
    pub id: String,
    pub limit: Option<usize>,
    pub order: MessageOrder,
    pub after_message_index: Option<u32>,
    pub before_message_index: Option<u32>,
}

/// Inputs for [`get_session_trajectory`].
#[derive(Debug, Clone)]
pub struct TrajectoryParams {
    pub id: String,
    pub limit: Option<usize>,
    pub after_step_index: Option<u32>,
}

/// Sort key for [`find_inefficient_sessions`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InefficientSort {
    BillableTokens,
    ErrorRate,
    CacheReadRatio,
}

impl InefficientSort {
    /// Parse a `sort_by` string, defaulting to `BillableTokens` when absent.
    pub fn parse(value: Option<&str>) -> Result<Self> {
        match value.map(str::trim) {
            None | Some("") | Some("billable_tokens") => Ok(Self::BillableTokens),
            Some("error_rate") => Ok(Self::ErrorRate),
            Some("cache_read_ratio") => Ok(Self::CacheReadRatio),
            Some(other) => bail!(
                "invalid sort_by {other:?}; expected billable_tokens, error_rate, or cache_read_ratio"
            ),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::BillableTokens => "billable_tokens",
            Self::ErrorRate => "error_rate",
            Self::CacheReadRatio => "cache_read_ratio",
        }
    }
}

/// Inputs for [`find_inefficient_sessions`].
#[derive(Debug, Clone)]
pub struct InefficientParams {
    pub since: Option<i64>,
    pub limit: Option<usize>,
    pub sort_by: InefficientSort,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SearchSpec {
    query: Option<String>,
    cwd: Option<String>,
    cwd_only: bool,
    since: Option<i64>,
    until: Option<i64>,
}

impl SearchSpec {
    fn from_params(params: SearchParams) -> Self {
        Self {
            query: normalize_query(params.query),
            cwd: params.cwd.map(|p| p.to_string_lossy().into_owned()),
            cwd_only: params.cwd_only,
            since: params.time_range.since,
            until: params.time_range.until,
        }
    }

    fn time_range(&self) -> TimeRange {
        TimeRange {
            since: self.since,
            until: self.until,
        }
    }

    fn cwd_path(&self) -> Option<&Path> {
        self.cwd.as_deref().map(Path::new)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SearchCursor {
    offset: usize,
    spec: SearchSpec,
}

fn normalize_query(query: Option<String>) -> Option<String> {
    query
        .map(|q| q.trim().to_string())
        .filter(|q| !q.is_empty())
}

/// Search indexed sessions, or list recent sessions when no query is given.
pub fn search_sessions(conn: &Connection, params: SearchParams) -> Result<SearchResponse> {
    let limit = cap_limit(params.limit, SEARCH_LIMIT_DEFAULT, SEARCH_LIMIT_CAP);
    let cursor = params.cursor.clone();
    let spec = SearchSpec::from_params(params);
    let (spec, offset) = match cursor.as_deref() {
        Some(cursor) => decode_search_cursor(cursor)?,
        None => (spec, 0),
    };
    let search_query = spec.query.as_deref();
    let page_limit = limit.checked_add(1).context("limit overflow")?;

    let mut results = if let Some(search_query) = search_query {
        let hits = search::text_search_with_time_range_paged(
            conn,
            search_query,
            spec.cwd_path(),
            spec.cwd_only,
            spec.time_range(),
            page_limit,
            offset,
        )?;
        let mut cards = Vec::with_capacity(hits.len());
        for hit in &hits {
            let matches = query::fts_messages_in_session(
                conn,
                &hit.session_id,
                search_query,
                MATCHES_PER_SESSION,
                CARD_MESSAGE_CAP,
            )?;
            let reason = if matches.is_empty() {
                "metadata"
            } else {
                "message"
            };
            cards.push(card_from_hit(conn, hit, vec![reason.to_string()], matches)?);
        }
        cards
    } else {
        let hits = search::list_with_time_range_paged(
            conn,
            spec.cwd_path(),
            spec.cwd_only,
            spec.time_range(),
            page_limit,
            offset,
        )?;
        let mut cards = Vec::with_capacity(hits.len());
        for hit in &hits {
            cards.push(card_from_hit(
                conn,
                hit,
                vec!["recent".to_string()],
                Vec::new(),
            )?);
        }
        cards
    };

    let has_more = results.len() > limit;
    results.truncate(limit);
    let next_cursor = if has_more {
        let next_offset = offset
            .checked_add(results.len())
            .context("cursor offset overflow")?;
        Some(encode_search_cursor(&spec, next_offset)?)
    } else {
        None
    };

    Ok(SearchResponse {
        count: results.len(),
        has_more,
        next_cursor,
        results,
    })
}

/// Rank sessions by an efficiency signal to surface outliers (heavy billable
/// usage, high tool-error rate, or runaway cache re-reads). Read-only: returns
/// counts and ratios only, never message text or tool bodies.
pub fn find_inefficient_sessions(
    conn: &Connection,
    params: InefficientParams,
) -> Result<InefficientSessionsResponse> {
    let limit = cap_limit(params.limit, SEARCH_LIMIT_DEFAULT, SEARCH_LIMIT_CAP);
    let mut sessions: Vec<InefficientSession> = query::efficiency_rows(conn, params.since)?
        .into_iter()
        .map(|row| {
            let billable_tokens = row
                .tokens_input
                .saturating_add(row.tokens_output)
                .saturating_add(row.tokens_cache_create);
            let cache_read_ratio = row.tokens_cache_read as f64 / row.tokens_output.max(1) as f64;
            let error_rate = row.tool_error_count as f64 / row.tool_call_count.max(1) as f64;
            InefficientSession {
                id: row.session_id,
                agent: AgentKind::from_db(&row.agent).unwrap_or(AgentKind::Claude),
                title: row.ai_title,
                cwd: row.cwd,
                updated_at: format_updated_at(row.mtime),
                billable_tokens,
                tokens_output: row.tokens_output,
                tokens_cache_read: row.tokens_cache_read,
                cache_read_ratio,
                tool_call_count: row.tool_call_count,
                tool_error_count: row.tool_error_count,
                error_rate,
                thinking_tokens: row.thinking_tokens,
                wall_clock_ms: row.wall_clock_ms,
                message_count: row.msg_count.unwrap_or(0),
            }
        })
        .collect();

    sessions.sort_by(|a, b| {
        let key = |s: &InefficientSession| match params.sort_by {
            InefficientSort::BillableTokens => s.billable_tokens as f64,
            InefficientSort::ErrorRate => s.error_rate,
            InefficientSort::CacheReadRatio => s.cache_read_ratio,
        };
        key(b)
            .partial_cmp(&key(a))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.billable_tokens.cmp(&a.billable_tokens))
            .then_with(|| a.id.cmp(&b.id))
    });
    sessions.truncate(limit);

    Ok(InefficientSessionsResponse {
        count: sessions.len(),
        sort_by: params.sort_by.as_str().to_string(),
        results: sessions,
    })
}

fn encode_search_cursor(spec: &SearchSpec, offset: usize) -> Result<String> {
    let bytes = serde_json::to_vec(&SearchCursor {
        offset,
        spec: spec.clone(),
    })?;
    Ok(format!("v1.{}", encode_hex(&bytes)))
}

fn decode_search_cursor(cursor: &str) -> Result<(SearchSpec, usize)> {
    let hex = cursor
        .strip_prefix("v1.")
        .context("invalid search cursor")?;
    let bytes = decode_hex(hex).context("invalid search cursor")?;
    let cursor: SearchCursor = serde_json::from_slice(&bytes).context("invalid search cursor")?;
    Ok((cursor.spec, cursor.offset))
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn decode_hex(s: &str) -> Result<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        bail!("hex input has odd length");
    }

    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let hi = hex_value(pair[0]).context("hex input contains invalid digit")?;
        let lo = hex_value(pair[1]).context("hex input contains invalid digit")?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn card_from_hit(
    conn: &Connection,
    hit: &Hit,
    match_reasons: Vec<String>,
    matches: Vec<SessionMessage>,
) -> Result<SessionCard> {
    let message_count = query::message_count(conn, &hit.session_id)?;
    let latest_message = query::latest_message(conn, &hit.session_id, CARD_MESSAGE_CAP)?;
    Ok(SessionCard {
        id: hit.session_id.clone(),
        agent: hit.agent,
        title: hit.ai_title.clone(),
        cwd: hit.cwd.clone(),
        updated_at: format_updated_at(hit.mtime),
        first_prompt: capped_first_prompt(hit.first_prompt.as_deref()),
        match_reasons,
        matches,
        latest_message,
        metadata: metadata_from_hit(hit, message_count),
    })
}

/// Lightweight overview of a single session, or `None` if not found.
pub fn get_session_overview(conn: &Connection, id: &str) -> Result<Option<OverviewResponse>> {
    let Some(hit) = search::show(conn, id)? else {
        return Ok(None);
    };
    let message_count = query::message_count(conn, &hit.session_id)?;
    let first_message = query::first_message(conn, &hit.session_id, OVERVIEW_MESSAGE_CAP)?;
    let latest_message = query::latest_message(conn, &hit.session_id, OVERVIEW_MESSAGE_CAP)?;
    Ok(Some(OverviewResponse {
        session: OverviewSession {
            id: hit.session_id.clone(),
            agent: hit.agent,
            title: hit.ai_title.clone(),
            cwd: hit.cwd.clone(),
            updated_at: format_updated_at(hit.mtime),
            first_prompt: capped_first_prompt(hit.first_prompt.as_deref()),
            metadata: metadata_from_hit(&hit, message_count),
        },
        first_message,
        latest_message,
        message_count,
    }))
}

/// Page through visible messages of one session, or `None` if not found.
pub fn get_session_messages(
    conn: &Connection,
    params: MessagesParams,
) -> Result<Option<MessagesResponse>> {
    let Some(reference) = session_ref(conn, &params.id)? else {
        return Ok(None);
    };
    let limit = cap_limit(params.limit, MESSAGES_LIMIT_DEFAULT, MESSAGES_LIMIT_CAP);
    let page = query::paged_messages(
        conn,
        &reference.id,
        params.order,
        params.after_message_index,
        params.before_message_index,
        limit,
        FULL_MESSAGE_CAP,
    )?;
    Ok(Some(MessagesResponse {
        session: reference,
        messages: page.messages,
        has_more_before: page.has_more_before,
        has_more_after: page.has_more_after,
    }))
}

/// Search visible messages within one session, or `None` if not found.
pub fn search_session_messages(
    conn: &Connection,
    id: &str,
    query: &str,
    limit: Option<usize>,
) -> Result<Option<MessageSearchResponse>> {
    let Some(reference) = session_ref(conn, id)? else {
        return Ok(None);
    };
    let limit = cap_limit(
        limit,
        SEARCH_MESSAGES_LIMIT_DEFAULT,
        SEARCH_MESSAGES_LIMIT_CAP,
    );
    let matches =
        query::fts_messages_in_session(conn, &reference.id, query, limit, FULL_MESSAGE_CAP)?;
    Ok(Some(MessageSearchResponse {
        session: reference,
        count: matches.len(),
        matches,
    }))
}

/// Page through the step-level trajectory of one session, or `None` if the
/// session is not indexed. Returns counts and per-step metadata; tool input and
/// (opt-in) result bodies are capped for the wire.
pub fn get_session_trajectory(
    conn: &Connection,
    params: TrajectoryParams,
) -> Result<Option<TrajectoryResponse>> {
    let Some(reference) = session_ref(conn, &params.id)? else {
        return Ok(None);
    };
    let limit = cap_limit(params.limit, TRAJECTORY_LIMIT_DEFAULT, TRAJECTORY_LIMIT_CAP);
    let (steps, has_more) =
        query::trajectory_steps(conn, &reference.id, params.after_step_index, limit)?;
    Ok(Some(TrajectoryResponse {
        session: reference,
        count: steps.len(),
        steps,
        has_more,
    }))
}

fn session_ref(conn: &Connection, id: &str) -> Result<Option<SessionRef>> {
    Ok(search::show(conn, id)?.map(|hit| SessionRef {
        id: hit.session_id,
        agent: hit.agent,
        title: hit.ai_title,
    }))
}
