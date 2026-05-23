//! SQL queries used by both the TUI and CLI search paths.

use std::path::Path;

use anyhow::Result;
use rusqlite::functions::FunctionFlags;
use rusqlite::{params, Connection};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub session_id: String,
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
    pub labels: Vec<String>,
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
    pub cwd_boost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd_score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recency: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_score: Option<f64>,
}

/// Recency factor in [0, 1] using a log decay on age in days.
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
    1.0 / (1.0 + (age_days + 1.0).ln())
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

/// Newest sessions, optionally restricted to a cwd.
pub fn list(
    conn: &Connection,
    cwd: Option<&Path>,
    cwd_only: bool,
    since_secs: Option<i64>,
    limit: usize,
) -> Result<Vec<Hit>> {
    let cwd_s = cwd.map(|p| p.to_string_lossy().into_owned());
    let cutoff = since_secs.map(|s| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        now - s
    });

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
    if let Some(c) = cutoff {
        sql.push_str(" AND mtime >= ?");
        bound.push(Box::new(c));
    }

    if cwd.is_some() && !cwd_only {
        // ORDER BY (cwd = ?) — use a separate placeholder for the boost.
        sql.push_str(" ORDER BY (cwd = ?) DESC, mtime DESC");
        bound.push(Box::new(cwd_s.clone().unwrap_or_default()));
    } else {
        sql.push_str(" ORDER BY mtime DESC");
    }
    sql.push_str(" LIMIT ?");
    bound.push(Box::new(limit as i64));

    let mut stmt = conn.prepare(&sql)?;
    let params_iter: Vec<&dyn rusqlite::ToSql> = bound.iter().map(|b| b.as_ref()).collect();
    let rows: Vec<Hit> = stmt
        .query_map(params_iter.as_slice(), map_hit)?
        .collect::<Result<_, _>>()?;

    Ok(annotate(rows, cwd_s.as_deref()))
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
    let cwd_s = cwd.map(|p| p.to_string_lossy().into_owned());
    let q = build_fts_query(query);
    if q.is_empty() {
        return Ok(vec![]);
    }

    register_search_functions(conn)?;

    let cwd_boost_param = if cwd_only { "?3" } else { "?2" };
    let limit_param = if cwd_only { "?4" } else { "?3" };
    let mut sql = "WITH ranked AS (
             SELECT s.session_id, s.ai_title, s.cwd, s.mtime, s.msg_count, s.first_prompt,
                    s.file_path, s.git_branch, s.pr_number, s.pr_url, s.pr_repo, s.project_dir,
                    s.tokens_input, s.tokens_output, s.tokens_cache_read, s.tokens_cache_create,
                    bm25(sessions_fts, 1.5, 3.0, 0.8) AS bm25_rank
             FROM sessions_fts JOIN sessions s ON s.rowid = sessions_fts.rowid
             WHERE sessions_fts MATCH ?1"
        .to_string();
    if cwd_only {
        sql.push_str(" AND s.cwd = ?2");
    }
    sql.push_str(&format!(
        "
         ),
         components AS (
             SELECT ranked.*,
                    -bm25_rank AS keyword_score,
                    CASE
                        WHEN {cwd_boost_param} IS NOT NULL AND cwd = {cwd_boost_param} THEN 1.0
                        ELSE 0.0
                    END AS cwd_boost,
                    ccsf_recency_score(mtime) AS recency
             FROM ranked
         ),
         scored AS (
             SELECT *,
                    cwd_boost * 2.0 AS cwd_score,
                    keyword_score + (cwd_boost * 2.0) + recency AS final_score
             FROM components
         )
         SELECT session_id, ai_title, cwd, mtime, msg_count, first_prompt, file_path,
                git_branch, pr_number, pr_url, pr_repo, project_dir,
                tokens_input, tokens_output, tokens_cache_read, tokens_cache_create,
                bm25_rank, keyword_score, cwd_boost, cwd_score, recency, final_score
         FROM scored
         ORDER BY final_score DESC, bm25_rank ASC, mtime DESC, session_id ASC
         LIMIT {limit_param}"
    ));

    let mut stmt = conn.prepare(&sql)?;

    let mut hits: Vec<Hit> = Vec::new();
    let mut rows = if cwd_only {
        stmt.query(params![
            q,
            cwd_s.as_deref().unwrap_or(""),
            cwd_s.as_deref(),
            limit as i64
        ])?
    } else {
        stmt.query(params![q, cwd_s.as_deref(), limit as i64])?
    };
    while let Some(r) = rows.next()? {
        let mut h = map_hit(r)?;
        let final_score = r.get(21).unwrap_or(0.0);
        h.scores.text_search = Some(final_score);
        h.scores.bm25_rank = Some(r.get(16).unwrap_or(0.0));
        h.scores.keyword_score = Some(r.get(17).unwrap_or(0.0));
        h.scores.cwd_boost = Some(r.get(18).unwrap_or(0.0));
        h.scores.cwd_score = Some(r.get(19).unwrap_or(0.0));
        h.scores.recency = Some(r.get(20).unwrap_or(0.0));
        h.scores.final_score = Some(final_score);
        h.labels.push("match".to_string());
        hits.push(h);
    }

    Ok(annotate(hits, cwd_s.as_deref()))
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
fn build_fts_query(q: &str) -> String {
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
     git_branch, pr_number, pr_url, pr_repo, project_dir, \
     tokens_input, tokens_output, tokens_cache_read, tokens_cache_create";

fn map_hit(r: &rusqlite::Row<'_>) -> rusqlite::Result<Hit> {
    let project_dir: String = r.get(11)?;
    let is_worktree = project_dir.contains("--claude-worktrees-");
    Ok(Hit {
        session_id: r.get(0)?,
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
        tokens_input: r.get::<_, i64>(12).unwrap_or(0).max(0) as u64,
        tokens_output: r.get::<_, i64>(13).unwrap_or(0).max(0) as u64,
        tokens_cache_read: r.get::<_, i64>(14).unwrap_or(0).max(0) as u64,
        tokens_cache_create: r.get::<_, i64>(15).unwrap_or(0).max(0) as u64,
        labels: Vec::new(),
        scores: Scores::default(),
    })
}

fn annotate(mut hits: Vec<Hit>, cwd: Option<&str>) -> Vec<Hit> {
    for h in &mut hits {
        if let Some(c) = cwd {
            if c == h.cwd {
                h.labels.insert(0, "cwd".to_string());
            }
        }
        if h.labels.is_empty() {
            h.labels.push("recent".to_string());
        }
    }
    hits
}

/// Fetch a single session by id.
pub fn show(conn: &Connection, session_id: &str) -> Result<Option<Hit>> {
    let sql = format!("SELECT {HIT_COLS} FROM sessions WHERE session_id = ?");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![session_id])?;
    if let Some(r) = rows.next()? {
        return Ok(Some(map_hit(r)?));
    }
    Ok(None)
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
        let preview = [ai_title, first_prompt]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" | ");
        conn.execute(
            "INSERT INTO sessions
               (session_id, project_dir, cwd, ai_title, first_prompt, preview, mtime, size, file_path)
             VALUES (?1, '/p', ?2, ?3, ?4, ?5, ?6, 0, '/f')",
            params![id, cwd, ai_title, first_prompt, preview, mtime],
        )
        .expect("insert");
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

        let bm25_rank = scores.bm25_rank.expect("bm25 rank");
        let keyword_score = scores.keyword_score.expect("keyword score");
        let cwd_boost = scores.cwd_boost.expect("cwd boost");
        let cwd_score = scores.cwd_score.expect("cwd score");
        let recency = scores.recency.expect("recency score");
        let final_score = scores.final_score.expect("final score");

        assert_close(keyword_score, -bm25_rank);
        assert_close(cwd_boost, 1.0);
        assert_close(cwd_score, cwd_boost * 2.0);
        assert_near(recency, recency_score(1_700_000_000), 0.0001);
        assert_close(final_score, keyword_score + cwd_score + recency);
        assert_close(scores.text_search.expect("text search score"), final_score);
    }

    #[test]
    fn text_search_without_cwd_has_zero_cwd_score() {
        let conn = open_indexed_db();
        insert_session(&conn, "s1", "/repo/current", None, Some("phaseone ranking"));

        let hits = text_search(&conn, "phaseone", None, false, 10).unwrap();
        let scores = &hits.first().expect("matching hit").scores;

        assert_close(scores.cwd_boost.expect("cwd boost"), 0.0);
        assert_close(scores.cwd_score.expect("cwd score"), 0.0);
    }

    #[test]
    fn text_search_sql_ordering_is_not_cut_by_bm25_window() {
        let conn = open_indexed_db();
        for i in 0..5 {
            insert_session_at(
                &conn,
                &format!("bm25-{i}"),
                "/repo/other",
                None,
                Some("phaseboost"),
                1_700_000_000,
            );
        }
        insert_session_at(
            &conn,
            "cwd-boosted",
            "/repo/current",
            Some("phaseboost"),
            None,
            1_700_000_000,
        );

        let bm25_window: Vec<String> = conn
            .prepare(
                "SELECT s.session_id
                 FROM sessions_fts JOIN sessions s ON s.rowid = sessions_fts.rowid
                 WHERE sessions_fts MATCH ?1
                 ORDER BY bm25(sessions_fts, 1.5, 3.0, 0.8)
                 LIMIT 2",
            )
            .unwrap()
            .query_map(params![build_fts_query("phaseboost")], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(!bm25_window.iter().any(|id| id == "cwd-boosted"));

        let hits = text_search(
            &conn,
            "phaseboost",
            Some(Path::new("/repo/current")),
            false,
            1,
        )
        .unwrap();

        assert_eq!(
            hits.first().map(|h| h.session_id.as_str()),
            Some("cwd-boosted")
        );
        assert_close(hits[0].scores.cwd_boost.expect("cwd boost"), 1.0);
    }

    #[test]
    fn empty_score_breakdown_fields_are_omitted_from_json() {
        let scores = serde_json::to_value(Scores::default()).unwrap();

        assert_eq!(scores, serde_json::json!({}));
    }
}
