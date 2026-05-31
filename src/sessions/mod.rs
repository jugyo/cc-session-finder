//! MCP-first core for exposing indexed sessions. The `sessions` CLI subcommand
//! group and the `mcp` server are thin wrappers over these functions; both emit
//! the exact same JSON shapes. Nothing here returns native session ids, source
//! paths, raw scores, or index update status.

pub mod models;
pub mod query;

#[cfg(test)]
mod tests;

use std::path::PathBuf;

use anyhow::Result;
use rusqlite::Connection;

use crate::index::search::{self, Hit, TimeRange};

pub use models::{
    MessageSearchResponse, MessagesResponse, OverviewResponse, OverviewSession, SearchResponse,
    SessionCard, SessionMessage, SessionRef,
};
pub use query::MessageOrder;

use models::{
    cap_limit, capped_first_prompt, format_updated_at, metadata_from_hit, CARD_MESSAGE_CAP,
    FULL_MESSAGE_CAP, MATCHES_PER_SESSION, MESSAGES_LIMIT_CAP, MESSAGES_LIMIT_DEFAULT,
    OVERVIEW_MESSAGE_CAP, SEARCH_LIMIT_CAP, SEARCH_LIMIT_DEFAULT, SEARCH_MESSAGES_LIMIT_CAP,
    SEARCH_MESSAGES_LIMIT_DEFAULT,
};

/// Inputs for [`search_sessions`]. Callers parse `since`/`until` into a
/// [`TimeRange`] and resolve the effective cwd before calling.
#[derive(Debug, Clone, Default)]
pub struct SearchParams {
    pub query: Option<String>,
    pub limit: Option<usize>,
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

fn query_is_present(query: Option<&str>) -> bool {
    query.map(|q| !q.trim().is_empty()).unwrap_or(false)
}

/// Search indexed sessions, or list recent sessions when no query is given.
pub fn search_sessions(conn: &Connection, params: SearchParams) -> Result<SearchResponse> {
    let limit = cap_limit(params.limit, SEARCH_LIMIT_DEFAULT, SEARCH_LIMIT_CAP);
    let cwd = params.cwd.as_deref();
    let query = params.query.as_deref();

    let results = if query_is_present(query) {
        let query = query.unwrap();
        let hits = search::text_search_with_time_range(
            conn,
            query,
            cwd,
            params.cwd_only,
            params.time_range,
            limit,
        )?;
        let mut cards = Vec::with_capacity(hits.len());
        for hit in &hits {
            let matches = query::fts_messages_in_session(
                conn,
                &hit.session_id,
                query,
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
        let hits =
            search::list_with_time_range(conn, cwd, params.cwd_only, params.time_range, limit)?;
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

    Ok(SearchResponse {
        count: results.len(),
        results,
    })
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

fn session_ref(conn: &Connection, id: &str) -> Result<Option<SessionRef>> {
    Ok(search::show(conn, id)?.map(|hit| SessionRef {
        id: hit.session_id,
        agent: hit.agent,
        title: hit.ai_title,
    }))
}
