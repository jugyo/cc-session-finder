//! Message-level SQL helpers backing the MCP-first session tools. These read
//! only the cache DB and return the shared [`SessionMessage`] shape, keyed by
//! `turn_index` (exposed as `message_index`).

use anyhow::Result;
use rusqlite::{params, Connection};

use super::models::SessionMessage;
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
