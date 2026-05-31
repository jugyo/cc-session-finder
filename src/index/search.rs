//! SQL queries used by both the TUI and CLI search paths.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Result};
use rusqlite::functions::FunctionFlags;
use rusqlite::{params, Connection};
use serde::Serialize;

use crate::agent::AgentKind;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TimeRange {
    pub since: Option<i64>,
    pub until: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub session_id: String,
    pub agent: AgentKind,
    pub native_session_id: String,
    pub ai_title: Option<String>,
    pub cwd: String,
    pub mtime: i64,
    pub msg_count: Option<u32>,
    pub first_prompt: Option<String>,
    pub file_path: String,
    pub git_branch: Option<String>,
    pub pr_number: Option<i64>,
    pub pr_url: Option<String>,
    pub pr_repo: Option<String>,
    pub is_worktree: bool,
    pub tokens_input: u64,
    pub tokens_output: u64,
    pub tokens_cache_read: u64,
    pub tokens_cache_create: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet_role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet_message_count: Option<u32>,
    pub scores: Scores,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Scores {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_search: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bm25_rank: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keyword_score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recency: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freshness_boost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relevance_score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata_score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_bm25_rank: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_weighted_score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_match_count: Option<u32>,
}

/// Recency factor in [0, 1] with a one-day half-life.
#[cfg(test)]
fn recency_score(mtime: i64) -> f64 {
    recency_score_at(mtime, current_unix_secs())
}

fn current_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn recency_score_at(mtime: i64, now: i64) -> f64 {
    let age_days = ((now - mtime).max(0) as f64) / 86_400.0;
    0.5_f64.powf(age_days)
}

fn register_search_functions(conn: &Connection) -> Result<()> {
    let now = current_unix_secs();
    conn.create_scalar_function(
        "ccsf_recency_score",
        1,
        FunctionFlags::SQLITE_UTF8,
        move |ctx| {
            let mtime = ctx.get::<i64>(0)?;
            Ok(recency_score_at(mtime, now))
        },
    )?;
    Ok(())
}

const MESSAGE_SCORE_WEIGHT: f64 = 0.75;
const FRESHNESS_BOOST_WEIGHT: f64 = 1.0;

/// Newest sessions, optionally restricted to a cwd.
pub fn list(
    conn: &Connection,
    cwd: Option<&Path>,
    cwd_only: bool,
    since_secs: Option<i64>,
    limit: usize,
) -> Result<Vec<Hit>> {
    let time_range = TimeRange {
        since: since_secs.map(|s| current_unix_secs() - s),
        until: None,
    };
    list_with_time_range(conn, cwd, cwd_only, time_range, limit)
}

pub fn list_with_time_range(
    conn: &Connection,
    cwd: Option<&Path>,
    cwd_only: bool,
    time_range: TimeRange,
    limit: usize,
) -> Result<Vec<Hit>> {
    let cwd_s = cwd.map(|p| p.to_string_lossy().into_owned());

    let mut sql = format!(
        "SELECT {HIT_COLS}
         FROM sessions WHERE 1=1"
    );
    let mut bound: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if cwd_only {
        if let Some(cw) = &cwd_s {
            sql.push_str(" AND cwd = ?");
            bound.push(Box::new(cw.clone()));
        }
    }
    push_time_range_filter(&mut sql, &mut bound, "mtime", time_range);

    sql.push_str(" ORDER BY mtime DESC, session_id ASC");
    sql.push_str(" LIMIT ?");
    bound.push(Box::new(limit as i64));

    let mut stmt = conn.prepare(&sql)?;
    let params_iter: Vec<&dyn rusqlite::ToSql> = bound.iter().map(|b| b.as_ref()).collect();
    let mut rows: Vec<Hit> = stmt
        .query_map(params_iter.as_slice(), map_hit)?
        .collect::<Result<_, _>>()?;
    attach_latest_message_snippets(conn, &mut rows)?;

    Ok(rows)
}

fn push_time_range_filter(
    sql: &mut String,
    bound: &mut Vec<Box<dyn rusqlite::ToSql>>,
    column: &str,
    time_range: TimeRange,
) {
    if let Some(since) = time_range.since {
        sql.push_str(" AND ");
        sql.push_str(column);
        sql.push_str(" >= ?");
        bound.push(Box::new(since));
    }
    if let Some(until) = time_range.until {
        sql.push_str(" AND ");
        sql.push_str(column);
        sql.push_str(" <= ?");
        bound.push(Box::new(until));
    }
}

fn attach_latest_message_snippets(conn: &Connection, hits: &mut [Hit]) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT role, text
         FROM messages
         WHERE session_id = ?1
           AND role IN ('user', 'assistant')
           AND trim(text) != ''
         ORDER BY turn_index DESC, id DESC",
    )?;

    for hit in hits {
        let mut rows = stmt.query(params![&hit.session_id])?;
        while let Some(r) = rows.next()? {
            let role = r.get::<_, String>(0)?;
            let text = r.get::<_, String>(1)?;
            if !crate::session::is_human_visible_text(&text) {
                continue;
            }
            hit.snippet = Some(text);
            hit.snippet_role = Some(role);
            break;
        }
    }

    Ok(())
}

/// FTS5 text search using trigram tokens. The query is matched as a
/// prefix-allowing phrase.
pub fn text_search(
    conn: &Connection,
    query: &str,
    cwd: Option<&Path>,
    cwd_only: bool,
    limit: usize,
) -> Result<Vec<Hit>> {
    text_search_with_time_range(conn, query, cwd, cwd_only, TimeRange::default(), limit)
}

pub fn text_search_with_time_range(
    conn: &Connection,
    query: &str,
    cwd: Option<&Path>,
    cwd_only: bool,
    time_range: TimeRange,
    limit: usize,
) -> Result<Vec<Hit>> {
    let cwd_s = cwd.map(|p| p.to_string_lossy().into_owned());
    let q = build_fts_query(query);
    if q.is_empty() {
        return Ok(vec![]);
    }

    register_search_functions(conn)?;

    let mut hits_by_id: HashMap<String, Hit> = HashMap::new();
    for hit in metadata_hits(conn, &q, cwd_s.as_deref(), cwd_only, time_range)? {
        hits_by_id.insert(hit.session_id.clone(), hit);
    }

    for message_hit in message_hits(conn, &q, cwd_s.as_deref(), cwd_only, time_range)? {
        let entry = hits_by_id
            .entry(message_hit.hit.session_id.clone())
            .or_insert_with(|| {
                let mut hit = message_hit.hit.clone();
                hit.scores.recency = Some(message_hit.recency);
                hit.scores.freshness_boost = Some(message_hit.freshness_boost);
                hit.scores.metadata_score = Some(0.0);
                hit.scores.relevance_score = Some(0.0);
                hit
            });
        apply_message_score(entry, &message_hit);
    }

    let mut hits: Vec<Hit> = hits_by_id.into_values().collect();
    hits.sort_by(|a, b| {
        b.mtime
            .cmp(&a.mtime)
            .then_with(|| {
                b.scores
                    .relevance_score
                    .partial_cmp(&a.scores.relevance_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    hits.truncate(limit);

    Ok(hits)
}

fn metadata_hits(
    conn: &Connection,
    q: &str,
    cwd: Option<&str>,
    cwd_only: bool,
    time_range: TimeRange,
) -> Result<Vec<Hit>> {
    let mut sql = "WITH ranked AS (
             SELECT s.session_id, s.ai_title, s.cwd, s.mtime, s.msg_count, s.first_prompt,
                    s.file_path, s.git_branch, s.pr_number, s.pr_url, s.pr_repo,
                    s.tokens_input, s.tokens_output, s.tokens_cache_read, s.tokens_cache_create,
                    s.agent, s.native_session_id, s.source_group,
                    bm25(sessions_fts, 1.5, 3.0, 0.8) AS bm25_rank
             FROM sessions_fts JOIN sessions s ON s.rowid = sessions_fts.rowid
             WHERE sessions_fts MATCH ?"
        .to_string();
    let mut bound: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(q.to_string())];
    if cwd_only {
        sql.push_str(" AND s.cwd = ?");
        bound.push(Box::new(cwd.unwrap_or("").to_string()));
    }
    push_time_range_filter(&mut sql, &mut bound, "s.mtime", time_range);
    sql.push_str(&format!(
        "
         ),
         components AS (
             SELECT ranked.*,
                    -bm25_rank AS keyword_score,
                    ccsf_recency_score(mtime) AS recency
             FROM ranked
         ),
         scored AS (
             SELECT *,
                    keyword_score AS relevance_score,
                    1.0 + recency * {FRESHNESS_BOOST_WEIGHT} AS freshness_boost,
                    keyword_score * (1.0 + recency * {FRESHNESS_BOOST_WEIGHT}) AS final_score
             FROM components
         )
         SELECT session_id, ai_title, cwd, mtime, msg_count, first_prompt, file_path,
                git_branch, pr_number, pr_url, pr_repo,
                tokens_input, tokens_output, tokens_cache_read, tokens_cache_create,
                agent, native_session_id, source_group,
                bm25_rank, keyword_score, recency, freshness_boost, relevance_score, final_score
         FROM scored
         ORDER BY mtime DESC, bm25_rank ASC, session_id ASC"
    ));

    let mut stmt = conn.prepare(&sql)?;
    let params_iter: Vec<&dyn rusqlite::ToSql> = bound.iter().map(|b| b.as_ref()).collect();

    let mut hits: Vec<Hit> = Vec::new();
    let mut rows = stmt.query(params_iter.as_slice())?;
    while let Some(r) = rows.next()? {
        let mut h = map_hit(r)?;
        let final_score = r.get(COL_FINAL_SCORE).unwrap_or(0.0);
        h.scores.text_search = Some(final_score);
        h.scores.bm25_rank = Some(r.get(COL_BM25_RANK).unwrap_or(0.0));
        h.scores.keyword_score = Some(r.get(COL_KEYWORD_SCORE).unwrap_or(0.0));
        h.scores.recency = Some(r.get(COL_RECENCY).unwrap_or(0.0));
        h.scores.freshness_boost = Some(r.get(COL_FRESHNESS_BOOST).unwrap_or(1.0));
        h.scores.relevance_score = Some(r.get(COL_RELEVANCE_SCORE).unwrap_or(0.0));
        h.scores.metadata_score = h.scores.keyword_score;
        h.scores.final_score = Some(final_score);
        hits.push(h);
    }

    Ok(hits)
}

#[derive(Clone)]
struct MessageHit {
    hit: Hit,
    bm25_rank: f64,
    match_count: u32,
    recency: f64,
    freshness_boost: f64,
}

fn message_hits(
    conn: &Connection,
    q: &str,
    cwd: Option<&str>,
    cwd_only: bool,
    time_range: TimeRange,
) -> Result<Vec<MessageHit>> {
    let mut sql = "WITH scored AS (
             SELECT s.session_id, s.ai_title, s.cwd, s.mtime, s.msg_count, s.first_prompt,
                    s.file_path, s.git_branch, s.pr_number, s.pr_url, s.pr_repo,
                    s.tokens_input, s.tokens_output, s.tokens_cache_read, s.tokens_cache_create,
                    s.agent, s.native_session_id, s.source_group,
                    bm25(messages_fts) AS rank,
                    m.role,
                    m.turn_index,
                    snippet(messages_fts, 0, '', '', ' ... ', 64) AS snippet,
                    ccsf_recency_score(s.mtime) AS recency
             FROM messages_fts
             JOIN messages m ON m.id = messages_fts.rowid
             JOIN sessions s ON s.session_id = m.session_id
             WHERE messages_fts MATCH ?"
        .to_string();
    let mut bound: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(q.to_string())];
    if cwd_only {
        sql.push_str(" AND s.cwd = ?");
        bound.push(Box::new(cwd.unwrap_or("").to_string()));
    }
    push_time_range_filter(&mut sql, &mut bound, "s.mtime", time_range);
    sql.push_str(&format!(
        "
         )
         SELECT session_id, ai_title, cwd, mtime, msg_count, first_prompt, file_path,
                git_branch, pr_number, pr_url, pr_repo,
                tokens_input, tokens_output, tokens_cache_read, tokens_cache_create,
                agent, native_session_id, source_group,
                rank, role, snippet, recency, 1.0 + recency * {FRESHNESS_BOOST_WEIGHT} AS freshness_boost
         FROM scored
         ORDER BY rank ASC, mtime DESC, session_id ASC, turn_index ASC",
    ));

    let mut stmt = conn.prepare(&sql)?;
    let params_iter: Vec<&dyn rusqlite::ToSql> = bound.iter().map(|b| b.as_ref()).collect();
    let mut rows = stmt.query(params_iter.as_slice())?;

    let mut hits_by_id: HashMap<String, MessageHit> = HashMap::new();
    while let Some(r) = rows.next()? {
        let mut hit = map_hit(r)?;
        let bm25_rank = r.get(COL_MESSAGE_RANK).unwrap_or(0.0);
        let role: Option<String> = r.get(COL_MESSAGE_ROLE).ok();
        let snippet: Option<String> = r.get(COL_MESSAGE_SNIPPET).ok();
        let recency = r.get(COL_MESSAGE_RECENCY).unwrap_or(0.0);
        let freshness_boost = r.get(COL_MESSAGE_FRESHNESS_BOOST).unwrap_or(1.0);
        hit.snippet = snippet.clone();
        hit.snippet_role = role.clone();
        hits_by_id
            .entry(hit.session_id.clone())
            .and_modify(|message_hit| {
                message_hit.match_count = message_hit.match_count.saturating_add(1);
                if bm25_rank < message_hit.bm25_rank {
                    message_hit.bm25_rank = bm25_rank;
                    message_hit.hit.snippet = snippet.clone();
                    message_hit.hit.snippet_role = role.clone();
                }
            })
            .or_insert(MessageHit {
                hit,
                bm25_rank,
                match_count: 1,
                recency,
                freshness_boost,
            });
    }

    Ok(hits_by_id.into_values().collect())
}

fn apply_message_score(hit: &mut Hit, message_hit: &MessageHit) {
    let message_score = -message_hit.bm25_rank;
    let weighted = message_score * MESSAGE_SCORE_WEIGHT;
    let metadata_score = hit.scores.metadata_score.unwrap_or(0.0);
    let relevance_score = metadata_score.max(weighted);
    let freshness_boost = hit
        .scores
        .freshness_boost
        .unwrap_or(message_hit.freshness_boost);
    let final_score = relevance_score * freshness_boost;

    hit.scores.message_bm25_rank = Some(message_hit.bm25_rank);
    hit.scores.message_score = Some(message_score);
    hit.scores.message_weighted_score = Some(weighted);
    hit.scores.message_match_count = Some(message_hit.match_count);
    hit.scores.recency = Some(message_hit.recency);
    hit.scores.freshness_boost = Some(freshness_boost);
    hit.scores.relevance_score = Some(relevance_score);
    hit.scores.text_search = Some(final_score);
    hit.scores.final_score = Some(final_score);
    hit.snippet = message_hit.hit.snippet.clone();
    hit.snippet_role = message_hit.hit.snippet_role.clone();
    hit.snippet_message_count = Some(message_hit.match_count);
}

/// trigram tokenizer ignores tokens shorter than 3 characters, so we drop them
/// from the AND/NEAR clauses to avoid producing an unmatchable query.
const TRIGRAM_MIN_LEN: usize = 3;
/// Distance window for the NEAR clause (FTS5's own default).
const NEAR_DISTANCE: u32 = 10;

/// Translate a user query into an FTS5 MATCH expression of the form
/// `(t1 AND t2 ...) OR NEAR(t1 t2 ..., 10)`. All tokens must appear (any
/// order); the NEAR clause adds a bm25 boost when they also occur near each
/// other. Tokens shorter than 3 chars are dropped; if nothing survives, the
/// whole input is matched as a phrase as a last resort.
pub(crate) fn build_fts_query(q: &str) -> String {
    let trimmed = q.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let tokens: Vec<String> = trimmed
        .split_whitespace()
        .filter(|t| t.chars().count() >= TRIGRAM_MIN_LEN)
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();

    match tokens.len() {
        0 => format!("\"{}\"", trimmed.replace('"', "\"\"")),
        1 => tokens.into_iter().next().unwrap(),
        _ => {
            let and_clause = tokens.join(" AND ");
            let near_clause = format!("NEAR({}, {})", tokens.join(" "), NEAR_DISTANCE);
            format!("({}) OR {}", and_clause, near_clause)
        }
    }
}

/// Columns expected (in this exact order) by `map_hit`.
const HIT_COLS: &str = "session_id, ai_title, cwd, mtime, msg_count, first_prompt, file_path, \
     git_branch, pr_number, pr_url, pr_repo, \
     tokens_input, tokens_output, tokens_cache_read, tokens_cache_create, \
     agent, native_session_id, source_group";

const HIT_COL_COUNT: usize = 18;
const COL_BM25_RANK: usize = HIT_COL_COUNT;
const COL_KEYWORD_SCORE: usize = HIT_COL_COUNT + 1;
const COL_RECENCY: usize = HIT_COL_COUNT + 2;
const COL_FRESHNESS_BOOST: usize = HIT_COL_COUNT + 3;
const COL_RELEVANCE_SCORE: usize = HIT_COL_COUNT + 4;
const COL_FINAL_SCORE: usize = HIT_COL_COUNT + 5;

const COL_MESSAGE_RANK: usize = HIT_COL_COUNT;
const COL_MESSAGE_ROLE: usize = HIT_COL_COUNT + 1;
const COL_MESSAGE_SNIPPET: usize = HIT_COL_COUNT + 2;
const COL_MESSAGE_RECENCY: usize = HIT_COL_COUNT + 3;
const COL_MESSAGE_FRESHNESS_BOOST: usize = HIT_COL_COUNT + 4;

fn map_hit(r: &rusqlite::Row<'_>) -> rusqlite::Result<Hit> {
    let session_id: String = r.get(0)?;
    let agent = r
        .get::<_, String>(15)
        .ok()
        .and_then(|agent| AgentKind::from_db(&agent))
        .unwrap_or(AgentKind::Claude);
    let native_session_id = r
        .get::<_, Option<String>>(16)
        .ok()
        .flatten()
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| session_id.clone());
    let source_group = r.get::<_, Option<String>>(17).ok().flatten();
    let is_worktree = source_group
        .as_deref()
        .is_some_and(|source_group| source_group.contains("--claude-worktrees-"));
    Ok(Hit {
        session_id,
        agent,
        native_session_id,
        ai_title: r.get(1)?,
        cwd: r.get(2)?,
        mtime: r.get(3)?,
        msg_count: r.get::<_, Option<u32>>(4)?,
        first_prompt: r.get(5)?,
        file_path: r.get(6)?,
        git_branch: r.get(7)?,
        pr_number: r.get::<_, Option<i64>>(8)?,
        pr_url: r.get(9)?,
        pr_repo: r.get(10)?,
        is_worktree,
        tokens_input: r.get::<_, i64>(11).unwrap_or(0).max(0) as u64,
        tokens_output: r.get::<_, i64>(12).unwrap_or(0).max(0) as u64,
        tokens_cache_read: r.get::<_, i64>(13).unwrap_or(0).max(0) as u64,
        tokens_cache_create: r.get::<_, i64>(14).unwrap_or(0).max(0) as u64,
        snippet: None,
        snippet_role: None,
        snippet_message_count: None,
        scores: Scores::default(),
    })
}

/// Fetch a single session by id.
pub fn show(conn: &Connection, session_id: &str) -> Result<Option<Hit>> {
    let sql = format!(
        "SELECT {HIT_COLS}
         FROM sessions
         WHERE session_id = ?1 OR native_session_id = ?1
         ORDER BY CASE WHEN session_id = ?1 THEN 0 ELSE 1 END, agent, session_id"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![session_id])?;
    let mut hits = Vec::new();
    while let Some(r) = rows.next()? {
        hits.push(map_hit(r)?);
    }
    if hits.is_empty() {
        return Ok(None);
    }
    if hits[0].session_id == session_id || hits.len() == 1 {
        return Ok(hits.into_iter().next());
    }
    bail!("ambiguous session id: {session_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_returns_empty() {
        assert_eq!(build_fts_query(""), "");
        assert_eq!(build_fts_query("   "), "");
    }

    #[test]
    fn single_token_is_just_that_token() {
        assert_eq!(build_fts_query("foo"), "\"foo\"");
    }

    #[test]
    fn multi_tokens_combine_and_with_near() {
        assert_eq!(
            build_fts_query("foo bar buz"),
            "(\"foo\" AND \"bar\" AND \"buz\") OR NEAR(\"foo\" \"bar\" \"buz\", 10)"
        );
    }

    #[test]
    fn drops_tokens_shorter_than_trigram_min() {
        // "ab" is filtered; only "foo" and "bar" remain.
        assert_eq!(
            build_fts_query("ab foo bar"),
            "(\"foo\" AND \"bar\") OR NEAR(\"foo\" \"bar\", 10)"
        );
    }

    #[test]
    fn short_remainder_collapses_to_single_token() {
        // After filtering shorts, exactly one token left → no AND/NEAR.
        assert_eq!(build_fts_query("a b foo"), "\"foo\"");
    }

    #[test]
    fn all_short_falls_back_to_phrase() {
        // Nothing survives the trigram filter → phrase fallback on trimmed input.
        assert_eq!(build_fts_query("ab cd"), "\"ab cd\"");
    }

    #[test]
    fn escapes_quotes_inside_tokens() {
        assert_eq!(
            build_fts_query("foo a\"bc"),
            "(\"foo\" AND \"a\"\"bc\") OR NEAR(\"foo\" \"a\"\"bc\", 10)"
        );
    }

    // ---- integration tests against an in-memory DB ----

    fn open_indexed_db() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        crate::index::schema::ensure(&conn).expect("schema");
        conn
    }

    fn insert_session(
        conn: &Connection,
        id: &str,
        cwd: &str,
        ai_title: Option<&str>,
        first_prompt: Option<&str>,
    ) {
        insert_session_at(conn, id, cwd, ai_title, first_prompt, 0);
    }

    fn insert_session_at(
        conn: &Connection,
        id: &str,
        cwd: &str,
        ai_title: Option<&str>,
        first_prompt: Option<&str>,
        mtime: i64,
    ) {
        conn.execute(
            "INSERT INTO sessions
               (session_id, cwd, ai_title, first_prompt, mtime, size, file_path)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, '/f')",
            params![id, cwd, ai_title, first_prompt, mtime],
        )
        .expect("insert");
    }

    fn insert_message(
        conn: &Connection,
        session_id: &str,
        turn_index: i64,
        role: &str,
        text: &str,
    ) {
        conn.execute(
            "INSERT INTO messages (session_id, turn_index, role, text)
             VALUES (?1, ?2, ?3, ?4)",
            params![session_id, turn_index, role, text],
        )
        .expect("insert message");
    }

    fn insert_agent_session(
        conn: &Connection,
        session_id: &str,
        agent: AgentKind,
        native_session_id: &str,
    ) {
        conn.execute(
            "INSERT INTO sessions
               (session_id, agent, native_session_id, cwd, mtime, size, file_path)
             VALUES (?1, ?2, ?3, '/repo/current', 0, 0, '/f')",
            params![session_id, agent.as_str(), native_session_id],
        )
        .expect("insert agent session");
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-9,
            "actual {actual} != expected {expected}"
        );
    }

    fn assert_near(actual: f64, expected: f64, tolerance: f64) {
        assert!(
            (actual - expected).abs() < tolerance,
            "actual {actual} != expected {expected} within {tolerance}"
        );
    }

    #[test]
    fn schema_fts_columns_are_split_metadata() {
        let conn = open_indexed_db();
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(sessions_fts)")
            .unwrap()
            .query_map([], |r| r.get(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();

        assert_eq!(cols, ["ai_title", "first_prompt", "cwd"]);
    }

    #[test]
    fn show_can_find_session_by_native_id() {
        let conn = open_indexed_db();
        insert_agent_session(&conn, "codex:abc", AgentKind::Codex, "abc");

        let hit = show(&conn, "abc").unwrap().expect("hit");

        assert_eq!(hit.session_id, "codex:abc");
        assert_eq!(hit.agent, AgentKind::Codex);
        assert_eq!(hit.native_session_id, "abc");
    }

    #[test]
    fn text_search_matches_against_cwd() {
        let conn = open_indexed_db();
        insert_session(
            &conn,
            "s1",
            "/Users/foo/cc-session-finder",
            None,
            Some("hello world"),
        );

        let hits = text_search(&conn, "session-finder", None, false, 10).unwrap();
        assert!(hits.iter().any(|h| h.session_id == "s1"), "{hits:?}");
    }

    #[test]
    fn list_attaches_latest_message_as_snippet() {
        let conn = open_indexed_db();
        insert_session(&conn, "s1", "/repo/current", Some("title"), None);
        insert_message(&conn, "s1", 0, "user", "older message");
        insert_message(&conn, "s1", 1, "assistant", "latest message body");

        let hits = list(&conn, None, false, None, 10).unwrap();
        let hit = hits.iter().find(|h| h.session_id == "s1").expect("hit");

        assert_eq!(hit.snippet.as_deref(), Some("latest message body"));
        assert_eq!(hit.snippet_role.as_deref(), Some("assistant"));
    }

    #[test]
    fn list_snippet_ignores_non_message_roles() {
        let conn = open_indexed_db();
        insert_session(&conn, "s1", "/repo/current", Some("title"), None);
        insert_message(&conn, "s1", 0, "user", "visible user text");
        insert_message(&conn, "s1", 1, "tool", "tool output should be ignored");
        insert_message(&conn, "s1", 2, "system", "system output should be ignored");
        insert_message(&conn, "s1", 3, "assistant", "   ");

        let hits = list(&conn, None, false, None, 10).unwrap();
        let hit = hits.iter().find(|h| h.session_id == "s1").expect("hit");

        assert_eq!(hit.snippet.as_deref(), Some("visible user text"));
        assert_eq!(hit.snippet_role.as_deref(), Some("user"));
    }

    #[test]
    fn list_snippet_ignores_noise_message_text() {
        let conn = open_indexed_db();
        insert_session(&conn, "s1", "/repo/current", Some("title"), None);
        insert_message(&conn, "s1", 0, "assistant", "visible assistant text");
        insert_message(
            &conn,
            "s1",
            1,
            "user",
            "<local-command-stdout>cargo test output</local-command-stdout>",
        );
        insert_message(
            &conn,
            "s1",
            2,
            "user",
            "<task-notification><task-id>abc</task-id></task-notification>",
        );

        let hits = list(&conn, None, false, None, 10).unwrap();
        let hit = hits.iter().find(|h| h.session_id == "s1").expect("hit");

        assert_eq!(hit.snippet.as_deref(), Some("visible assistant text"));
        assert_eq!(hit.snippet_role.as_deref(), Some("assistant"));
    }

    #[test]
    fn list_filters_by_time_range() {
        let conn = open_indexed_db();
        insert_session_at(&conn, "old", "/repo/current", Some("old"), None, 100);
        insert_session_at(&conn, "middle", "/repo/current", Some("middle"), None, 200);
        insert_session_at(&conn, "new", "/repo/current", Some("new"), None, 300);

        let hits = list_with_time_range(
            &conn,
            None,
            false,
            TimeRange {
                since: Some(150),
                until: Some(250),
            },
            10,
        )
        .unwrap();

        let ids: Vec<_> = hits.iter().map(|h| h.session_id.as_str()).collect();
        assert_eq!(ids, ["middle"]);
    }

    #[test]
    fn list_does_not_boost_current_cwd_before_newer_results() {
        let conn = open_indexed_db();
        insert_session_at(
            &conn,
            "current",
            "/repo/current",
            Some("current"),
            None,
            1_700_000_000,
        );
        insert_session_at(
            &conn,
            "other",
            "/repo/other",
            Some("other"),
            None,
            current_unix_secs(),
        );

        let hits = list(&conn, Some(Path::new("/repo/current")), false, None, 2).unwrap();

        assert_eq!(hits.first().map(|h| h.session_id.as_str()), Some("other"));
    }

    #[test]
    fn text_search_ands_metadata_and_cwd_columns() {
        let conn = open_indexed_db();
        insert_session(
            &conn,
            "s1",
            "/Users/foo/cc-session-finder",
            None,
            Some("hello world"),
        );
        insert_session(
            &conn,
            "s2",
            "/Users/bar/other-project",
            None,
            Some("hello again"),
        );

        let hits = text_search(&conn, "hello session-finder", None, false, 10).unwrap();
        let ids: Vec<_> = hits.iter().map(|h| h.session_id.as_str()).collect();
        assert!(ids.contains(&"s1"), "{ids:?}");
        assert!(!ids.contains(&"s2"), "{ids:?}");
    }

    #[test]
    fn text_search_ranks_first_prompt_above_title_match() {
        let conn = open_indexed_db();
        insert_session(&conn, "title", "/p", Some("phaseone ranking"), None);
        insert_session(&conn, "prompt", "/p", None, Some("phaseone ranking"));

        let hits = text_search(&conn, "phaseone", None, false, 10).unwrap();
        let ids: Vec<_> = hits.iter().map(|h| h.session_id.as_str()).collect();

        assert_eq!(ids.first(), Some(&"prompt"));
    }

    #[test]
    fn text_search_stores_score_breakdown() {
        let conn = open_indexed_db();
        insert_session_at(
            &conn,
            "s1",
            "/repo/current",
            None,
            Some("phaseone ranking"),
            1_700_000_000,
        );

        let hits = text_search(
            &conn,
            "phaseone",
            Some(Path::new("/repo/current")),
            false,
            10,
        )
        .unwrap();
        let scores = &hits
            .iter()
            .find(|h| h.session_id == "s1")
            .expect("matching hit")
            .scores;
        let hit = hits.iter().find(|h| h.session_id == "s1").unwrap();

        let bm25_rank = scores.bm25_rank.expect("bm25 rank");
        let keyword_score = scores.keyword_score.expect("keyword score");
        let recency = scores.recency.expect("recency score");
        let freshness_boost = scores.freshness_boost.expect("freshness boost");
        let relevance_score = scores.relevance_score.expect("relevance score");
        let final_score = scores.final_score.expect("final score");

        assert_close(keyword_score, -bm25_rank);
        assert_near(recency, recency_score(1_700_000_000), 0.0001);
        assert_close(freshness_boost, 1.0 + recency * FRESHNESS_BOOST_WEIGHT);
        assert_close(relevance_score, keyword_score);
        assert_close(
            scores.metadata_score.expect("metadata score"),
            keyword_score,
        );
        assert_close(final_score, relevance_score * freshness_boost);
        assert_close(scores.text_search.expect("text search score"), final_score);
        assert!(hit.snippet.is_none());
        assert!(hit.snippet_role.is_none());
        assert!(hit.snippet_message_count.is_none());
    }

    #[test]
    fn text_search_sorts_matches_by_recency_before_relevance() {
        let conn = open_indexed_db();
        insert_session_at(
            &conn,
            "old",
            "/repo/current",
            None,
            Some("phaseboost phaseboost phaseboost"),
            1_700_000_000,
        );
        insert_session_at(
            &conn,
            "new",
            "/repo/other",
            Some("phaseboost"),
            None,
            current_unix_secs(),
        );

        let hits = text_search(&conn, "phaseboost", None, false, 2).unwrap();

        assert_eq!(hits.first().map(|h| h.session_id.as_str()), Some("new"));
        assert!(
            hits[1].scores.relevance_score > hits[0].scores.relevance_score,
            "{:?}",
            hits.iter()
                .map(|h| (&h.session_id, h.scores.relevance_score))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn text_search_does_not_boost_current_cwd_before_newer_results() {
        let conn = open_indexed_db();
        insert_session_at(
            &conn,
            "current",
            "/repo/current",
            None,
            Some("phasecwd match"),
            1_700_000_000,
        );
        insert_session_at(
            &conn,
            "other",
            "/repo/other",
            None,
            Some("phasecwd match"),
            current_unix_secs(),
        );

        let hits = text_search(
            &conn,
            "phasecwd",
            Some(Path::new("/repo/current")),
            false,
            2,
        )
        .unwrap();

        assert_eq!(hits.first().map(|h| h.session_id.as_str()), Some("other"));
    }

    #[test]
    fn text_search_finds_message_only_hit() {
        let conn = open_indexed_db();
        insert_session(&conn, "s1", "/repo/current", Some("unrelated"), None);
        insert_message(&conn, "s1", 0, "user", "bodyonly needle appears here");

        let hits = text_search(&conn, "bodyonly", None, false, 10).unwrap();
        let hit = hits.iter().find(|h| h.session_id == "s1").expect("hit");
        let scores = &hit.scores;

        assert!(scores.bm25_rank.is_none());
        assert!(scores.keyword_score.is_none());
        assert_eq!(hit.snippet_role.as_deref(), Some("user"));
        assert_eq!(hit.snippet_message_count, Some(1));
        assert!(
            hit.snippet
                .as_deref()
                .is_some_and(|snippet| snippet.contains("bodyonly")),
            "{:?}",
            hit.snippet
        );
        assert!(
            hit.snippet
                .as_deref()
                .is_some_and(|snippet| !snippet.contains("[bodyonly]")),
            "{:?}",
            hit.snippet
        );
        let json = serde_json::to_value(hit).unwrap();
        assert!(json.get("snippet").is_some());
        assert_eq!(json["snippet_role"], "user");
        assert_eq!(json["snippet_message_count"], 1);
        assert_eq!(scores.message_match_count, Some(1));
        assert_close(
            scores.message_score.expect("message score"),
            -scores.message_bm25_rank.expect("message bm25 rank"),
        );
        assert_close(
            scores
                .message_weighted_score
                .expect("weighted message score"),
            scores.message_score.expect("message score") * MESSAGE_SCORE_WEIGHT,
        );
        assert_close(
            scores.final_score.expect("final score"),
            scores.relevance_score.expect("relevance score")
                * scores.freshness_boost.expect("freshness boost"),
        );
        assert_close(
            scores.relevance_score.expect("relevance score"),
            scores.metadata_score.expect("metadata score").max(
                scores
                    .message_weighted_score
                    .expect("weighted message score"),
            ),
        );
    }

    #[test]
    fn text_search_filters_metadata_and_messages_by_time_range() {
        let conn = open_indexed_db();
        insert_session_at(&conn, "old", "/repo/current", None, Some("timerange"), 100);
        insert_session_at(
            &conn,
            "middle",
            "/repo/current",
            None,
            Some("timerange"),
            200,
        );
        insert_session_at(&conn, "new", "/repo/current", None, Some("timerange"), 300);
        insert_session_at(&conn, "body", "/repo/current", Some("other"), None, 220);
        insert_message(&conn, "body", 0, "user", "timerange appears in the body");

        let hits = text_search_with_time_range(
            &conn,
            "timerange",
            None,
            false,
            TimeRange {
                since: Some(150),
                until: Some(250),
            },
            10,
        )
        .unwrap();

        let ids: Vec<_> = hits.iter().map(|h| h.session_id.as_str()).collect();
        assert_eq!(ids, ["body", "middle"]);
    }

    #[test]
    fn text_search_merges_metadata_and_message_scores() {
        let conn = open_indexed_db();
        insert_session(
            &conn,
            "s1",
            "/repo/current",
            None,
            Some("phaseword in prompt"),
        );
        insert_message(&conn, "s1", 0, "user", "phaseword in body");
        insert_message(&conn, "s1", 1, "assistant", "phaseword appears again");

        let hits = text_search(&conn, "phaseword", None, false, 10).unwrap();
        let hit = hits.iter().find(|h| h.session_id == "s1").expect("hit");
        let scores = &hit.scores;

        assert_eq!(scores.message_match_count, Some(2));
        assert_eq!(hit.snippet_message_count, Some(2));
        assert!(hit.snippet.is_some());
        assert!(scores.keyword_score.is_some());
        assert_close(
            scores.metadata_score.expect("metadata score"),
            scores.keyword_score.expect("keyword score"),
        );
        assert_close(
            scores.relevance_score.expect("relevance score"),
            scores.metadata_score.expect("metadata score").max(
                scores
                    .message_weighted_score
                    .expect("weighted message score"),
            ),
        );
        assert_close(
            scores.final_score.expect("final score"),
            scores.relevance_score.expect("relevance score")
                * scores.freshness_boost.expect("freshness boost"),
        );
    }

    #[test]
    fn message_match_count_does_not_affect_relevance() {
        let conn = open_indexed_db();
        insert_session(&conn, "s1", "/repo/current", None, None);
        for i in 0..50 {
            insert_message(&conn, "s1", i, "assistant", "manymatches token");
        }

        let hits = text_search(&conn, "manymatches", None, false, 10).unwrap();
        let scores = &hits.first().expect("hit").scores;

        assert_eq!(scores.message_match_count, Some(50));
        assert_close(
            scores.relevance_score.expect("relevance score"),
            scores
                .message_weighted_score
                .expect("weighted message score"),
        );
    }

    #[test]
    fn message_snippet_updates_after_message_replacement() {
        let conn = open_indexed_db();
        insert_session(&conn, "s1", "/repo/current", None, None);
        insert_message(&conn, "s1", 0, "user", "oldsnippet token");

        assert_eq!(
            text_search(&conn, "oldsnippet", None, false, 10)
                .unwrap()
                .len(),
            1
        );

        conn.execute("DELETE FROM messages WHERE session_id = ?1", params!["s1"])
            .unwrap();
        insert_message(&conn, "s1", 0, "assistant", "newsnippet token");

        assert!(text_search(&conn, "oldsnippet", None, false, 10)
            .unwrap()
            .is_empty());
        let hits = text_search(&conn, "newsnippet", None, false, 10).unwrap();
        let hit = hits.first().expect("hit");

        assert_eq!(hit.snippet_role.as_deref(), Some("assistant"));
        assert!(
            hit.snippet
                .as_deref()
                .is_some_and(|snippet| snippet.contains("newsnippet")),
            "{:?}",
            hit.snippet
        );
        assert!(
            hit.snippet
                .as_deref()
                .is_some_and(|snippet| !snippet.contains("[newsnippet]")),
            "{:?}",
            hit.snippet
        );
    }

    #[test]
    fn empty_score_breakdown_fields_are_omitted_from_json() {
        let scores = serde_json::to_value(Scores::default()).unwrap();

        assert_eq!(scores, serde_json::json!({}));
    }
}
