//! Message-level SQL helpers backing the MCP-first session tools. These read
//! only the cache DB and return the shared [`SessionMessage`] shape, keyed by
//! `turn_index` (exposed as `message_index`).

use anyhow::Result;
use rusqlite::{params, Connection};

use super::models::{truncate_text, SessionMessage, TrajectoryStepView, TRAJECTORY_TEXT_CAP};
use crate::index::search::build_fts_query;
use crate::session::is_human_visible_text;

/// Direction for paged message reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageOrder {
    Asc,
    Desc,
}

impl MessageOrder {
    fn sql(self) -> &'static str {
        match self {
            MessageOrder::Asc => "ASC",
            MessageOrder::Desc => "DESC",
        }
    }
}

/// Number of indexed visible user/assistant messages in a session.
pub fn message_count(conn: &Connection, id: &str) -> Result<u32> {
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM messages
         WHERE session_id = ?1 AND role IN ('user', 'assistant')",
        params![id],
        |r| r.get(0),
    )?;
    Ok(count.max(0) as u32)
}

/// Latest visible user/assistant message in a session.
pub fn latest_message(conn: &Connection, id: &str, cap: usize) -> Result<Option<SessionMessage>> {
    edge_message(conn, id, "DESC", cap)
}

/// First visible user/assistant message in a session.
pub fn first_message(conn: &Connection, id: &str, cap: usize) -> Result<Option<SessionMessage>> {
    edge_message(conn, id, "ASC", cap)
}

fn edge_message(
    conn: &Connection,
    id: &str,
    direction: &str,
    cap: usize,
) -> Result<Option<SessionMessage>> {
    let sql = format!(
        "SELECT turn_index, role, text FROM messages
         WHERE session_id = ?1 AND role IN ('user', 'assistant') AND trim(text) != ''
         ORDER BY turn_index {direction}, id {direction}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![id])?;
    while let Some(r) = rows.next()? {
        let turn_index: i64 = r.get(0)?;
        let role: String = r.get(1)?;
        let text: String = r.get(2)?;
        if !is_human_visible_text(&text) {
            continue;
        }
        return Ok(Some(SessionMessage::new(
            turn_index.max(0) as u32,
            role,
            text,
            cap,
        )));
    }
    Ok(None)
}

/// Up to `limit` messages in a session matching an FTS query, ranked by bm25.
pub fn fts_messages_in_session(
    conn: &Connection,
    id: &str,
    query: &str,
    limit: usize,
    cap: usize,
) -> Result<Vec<SessionMessage>> {
    let fts = build_fts_query(query);
    if fts.is_empty() {
        return Ok(vec![]);
    }
    let mut stmt = conn.prepare(
        "SELECT m.turn_index, m.role, m.text
         FROM messages_fts
         JOIN messages m ON m.id = messages_fts.rowid
         WHERE messages_fts MATCH ?1
           AND m.session_id = ?2
           AND m.role IN ('user', 'assistant')
         ORDER BY bm25(messages_fts) ASC, m.turn_index ASC
         LIMIT ?3",
    )?;
    let mut rows = stmt.query(params![fts, id, limit as i64])?;
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        let turn_index: i64 = r.get(0)?;
        let role: String = r.get(1)?;
        let text: String = r.get(2)?;
        if !is_human_visible_text(&text) {
            continue;
        }
        out.push(SessionMessage::new(
            turn_index.max(0) as u32,
            role,
            text,
            cap,
        ));
    }
    Ok(out)
}

/// Raw per-session efficiency columns backing
/// [`super::find_inefficient_sessions`]. Holds only counts and token buckets —
/// never message text or tool bodies.
pub struct EfficiencyRow {
    pub session_id: String,
    pub agent: String,
    pub ai_title: Option<String>,
    pub cwd: String,
    pub mtime: i64,
    pub msg_count: Option<u32>,
    pub tokens_input: u64,
    pub tokens_output: u64,
    pub tokens_cache_read: u64,
    pub tokens_cache_create: u64,
    pub tool_call_count: u64,
    pub tool_error_count: u64,
    pub thinking_tokens: u64,
    pub wall_clock_ms: i64,
}

/// Fetch efficiency columns for every session, optionally restricted to those
/// updated at or after `since` (Unix seconds).
pub fn efficiency_rows(conn: &Connection, since: Option<i64>) -> Result<Vec<EfficiencyRow>> {
    let mut sql = String::from(
        "SELECT session_id, agent, ai_title, cwd, mtime, msg_count,
                tokens_input, tokens_output, tokens_cache_read, tokens_cache_create,
                tool_call_count, tool_error_count, thinking_tokens, wall_clock_ms
         FROM sessions",
    );
    let mut bound: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(since) = since {
        sql.push_str(" WHERE mtime >= ?1");
        bound.push(Box::new(since));
    }

    let mut stmt = conn.prepare(&sql)?;
    let params_iter: Vec<&dyn rusqlite::ToSql> = bound.iter().map(|b| b.as_ref()).collect();
    let rows = stmt.query_map(params_iter.as_slice(), |r| {
        Ok(EfficiencyRow {
            session_id: r.get(0)?,
            agent: r.get(1)?,
            ai_title: r.get(2)?,
            cwd: r.get(3)?,
            mtime: r.get(4)?,
            msg_count: r.get::<_, Option<i64>>(5)?.map(|n| n.max(0) as u32),
            tokens_input: r.get::<_, i64>(6).unwrap_or(0).max(0) as u64,
            tokens_output: r.get::<_, i64>(7).unwrap_or(0).max(0) as u64,
            tokens_cache_read: r.get::<_, i64>(8).unwrap_or(0).max(0) as u64,
            tokens_cache_create: r.get::<_, i64>(9).unwrap_or(0).max(0) as u64,
            tool_call_count: r.get::<_, i64>(10).unwrap_or(0).max(0) as u64,
            tool_error_count: r.get::<_, i64>(11).unwrap_or(0).max(0) as u64,
            thinking_tokens: r.get::<_, i64>(12).unwrap_or(0).max(0) as u64,
            wall_clock_ms: r.get::<_, i64>(13).unwrap_or(0),
        })
    })?;
    rows.collect::<Result<_, _>>().map_err(Into::into)
}

/// One autonomous run: a maximal stretch of trajectory steps between human
/// turns. Carries the owning session's metadata (constant per session) plus the
/// run's length in steps.
pub struct AutonomyRunRow {
    pub session_id: String,
    pub agent: String,
    pub ai_title: Option<String>,
    pub cwd: String,
    pub mtime: i64,
    pub tool_call_count: u64,
    pub run_len: u32,
}

/// Fetch every autonomous run (one row per run) for Claude sessions, optionally
/// restricted to those updated at or after `since`. Runs are delimited by
/// `autonomous_run_index = 0`; each run's length is its step count.
pub fn autonomy_run_rows(conn: &Connection, since: Option<i64>) -> Result<Vec<AutonomyRunRow>> {
    let mut sql = String::from(
        "WITH runs AS (
             SELECT t.session_id, s.agent, s.ai_title, s.cwd, s.mtime, s.tool_call_count,
                    sum(CASE WHEN t.autonomous_run_index = 0 THEN 1 ELSE 0 END)
                      OVER (PARTITION BY t.session_id ORDER BY t.step_index
                            ROWS UNBOUNDED PRECEDING) AS run_no
             FROM trajectory t JOIN sessions s ON s.session_id = t.session_id
             WHERE t.agent = 'claude'",
    );
    let mut bound: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(since) = since {
        sql.push_str(" AND s.mtime >= ?1");
        bound.push(Box::new(since));
    }
    sql.push_str(
        "
         )
         SELECT session_id, agent, ai_title, cwd, mtime, tool_call_count, count(*) AS run_len
         FROM runs
         GROUP BY session_id, run_no",
    );

    let mut stmt = conn.prepare(&sql)?;
    let params_iter: Vec<&dyn rusqlite::ToSql> = bound.iter().map(|b| b.as_ref()).collect();
    let rows = stmt.query_map(params_iter.as_slice(), |r| {
        Ok(AutonomyRunRow {
            session_id: r.get(0)?,
            agent: r.get(1)?,
            ai_title: r.get(2)?,
            cwd: r.get(3)?,
            mtime: r.get(4)?,
            tool_call_count: r.get::<_, i64>(5).unwrap_or(0).max(0) as u64,
            run_len: r.get::<_, i64>(6)?.max(0) as u32,
        })
    })?;
    rows.collect::<Result<_, _>>().map_err(Into::into)
}

/// Read up to `limit` trajectory steps for a session ordered by `step_index`,
/// optionally starting after a given index. Returns the page plus whether more
/// steps follow it. Long tool input / result text is capped for the wire.
pub fn trajectory_steps(
    conn: &Connection,
    id: &str,
    after_step_index: Option<u32>,
    limit: usize,
) -> Result<(Vec<TrajectoryStepView>, bool)> {
    let mut sql = String::from(
        "SELECT step_index, role, tool_name, tool_input, tool_input_bytes,
                tool_result_bytes, tool_result, is_error,
                tokens_input, tokens_output, tokens_cache_read, tokens_cache_create,
                timestamp, is_sidechain, context_management,
                is_api_error, api_error_status, retry_attempt, max_retries,
                stop_reason, attribution_mcp_tool, attribution_mcp_server,
                attribution_skill, duration_ms, permission_mode, parent_uuid
         FROM trajectory WHERE session_id = ?1",
    );
    let mut bound: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(id.to_string())];
    if let Some(after) = after_step_index {
        sql.push_str(" AND step_index > ?");
        bound.push(Box::new(after as i64));
    }
    sql.push_str(" ORDER BY step_index ASC LIMIT ?");
    bound.push(Box::new(limit as i64 + 1));

    let mut stmt = conn.prepare(&sql)?;
    let params_iter: Vec<&dyn rusqlite::ToSql> = bound.iter().map(|b| b.as_ref()).collect();
    let mut rows = stmt.query(params_iter.as_slice())?;
    let mut steps = Vec::new();
    while let Some(r) = rows.next()? {
        steps.push(step_view_from_row(r)?);
    }

    let has_more = steps.len() > limit;
    steps.truncate(limit);
    Ok((steps, has_more))
}

fn step_view_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<TrajectoryStepView> {
    let tool_input_raw: Option<String> = r.get(3)?;
    let (tool_input, tool_input_truncated) = match tool_input_raw {
        Some(text) => {
            let (capped, truncated) = truncate_text(&text, TRAJECTORY_TEXT_CAP);
            (Some(capped), truncated)
        }
        None => (None, false),
    };
    let tool_result: Option<String> = r
        .get::<_, Option<String>>(6)?
        .map(|text| truncate_text(&text, TRAJECTORY_TEXT_CAP).0);
    Ok(TrajectoryStepView {
        step_index: r.get::<_, i64>(0)?.max(0) as u32,
        role: r.get(1)?,
        tool_name: r.get(2)?,
        tool_input,
        tool_input_truncated,
        tool_input_bytes: r.get::<_, i64>(4)?.max(0) as u64,
        tool_result_bytes: r.get::<_, i64>(5)?.max(0) as u64,
        tool_result,
        is_error: r.get::<_, i64>(7)? != 0,
        tokens_input: r.get::<_, i64>(8)?.max(0) as u64,
        tokens_output: r.get::<_, i64>(9)?.max(0) as u64,
        tokens_cache_read: r.get::<_, i64>(10)?.max(0) as u64,
        tokens_cache_create: r.get::<_, i64>(11)?.max(0) as u64,
        timestamp: r.get(12)?,
        is_sidechain: r.get::<_, i64>(13)? != 0,
        context_management: r.get(14)?,
        is_api_error: r.get::<_, i64>(15)? != 0,
        api_error_status: r.get(16)?,
        retry_attempt: r.get(17)?,
        max_retries: r.get(18)?,
        stop_reason: r.get(19)?,
        attribution_mcp_tool: r.get(20)?,
        attribution_mcp_server: r.get(21)?,
        attribution_skill: r.get(22)?,
        duration_ms: r.get(23)?,
        permission_mode: r.get(24)?,
        parent_uuid: r.get(25)?,
    })
}

/// A page of messages plus whether more exist on either side of the returned
/// window within the full session.
pub struct MessagePage {
    pub messages: Vec<SessionMessage>,
    pub has_more_before: bool,
    pub has_more_after: bool,
}

/// Page through visible messages by `turn_index`, honoring optional exclusive
/// bounds and the requested order.
pub fn paged_messages(
    conn: &Connection,
    id: &str,
    order: MessageOrder,
    after_message_index: Option<u32>,
    before_message_index: Option<u32>,
    limit: usize,
    cap: usize,
) -> Result<MessagePage> {
    let mut sql = String::from(
        "SELECT turn_index, role, text FROM messages
         WHERE session_id = ?1 AND role IN ('user', 'assistant')",
    );
    let mut bound: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(id.to_string())];
    if let Some(after) = after_message_index {
        sql.push_str(" AND turn_index > ?");
        bound.push(Box::new(after as i64));
    }
    if let Some(before) = before_message_index {
        sql.push_str(" AND turn_index < ?");
        bound.push(Box::new(before as i64));
    }
    sql.push_str(&format!(
        " ORDER BY turn_index {0}, id {0} LIMIT ?",
        order.sql()
    ));
    bound.push(Box::new(limit as i64));

    let mut stmt = conn.prepare(&sql)?;
    let params_iter: Vec<&dyn rusqlite::ToSql> = bound.iter().map(|b| b.as_ref()).collect();
    let mut rows = stmt.query(params_iter.as_slice())?;
    let mut messages = Vec::new();
    while let Some(r) = rows.next()? {
        let turn_index: i64 = r.get(0)?;
        let role: String = r.get(1)?;
        let text: String = r.get(2)?;
        if !is_human_visible_text(&text) {
            continue;
        }
        messages.push(SessionMessage::new(
            turn_index.max(0) as u32,
            role,
            text,
            cap,
        ));
    }

    let (has_more_before, has_more_after) = match (
        messages.iter().map(|m| m.message_index).min(),
        messages.iter().map(|m| m.message_index).max(),
    ) {
        (Some(min_idx), Some(max_idx)) => (
            message_exists_outside(conn, id, "<", min_idx)?,
            message_exists_outside(conn, id, ">", max_idx)?,
        ),
        _ => (false, false),
    };

    Ok(MessagePage {
        messages,
        has_more_before,
        has_more_after,
    })
}

fn message_exists_outside(conn: &Connection, id: &str, op: &str, bound: u32) -> Result<bool> {
    let sql = format!(
        "SELECT EXISTS(
             SELECT 1 FROM messages
             WHERE session_id = ?1 AND role IN ('user', 'assistant') AND turn_index {op} ?2
         )"
    );
    let exists: i64 = conn.query_row(&sql, params![id, bound as i64], |r| r.get(0))?;
    Ok(exists != 0)
}
