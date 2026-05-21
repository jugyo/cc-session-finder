//! SQL queries used by both the TUI and CLI search paths.

use std::path::Path;

use anyhow::Result;
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
    pub keyword: Option<f64>,
    pub vector: Option<f64>,
    pub recency: Option<f64>,
}

/// Recency factor in [0, 1] using a log decay on age in days.
pub fn recency_score(mtime: i64) -> f64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let age_days = ((now - mtime).max(0) as f64) / 86_400.0;
    1.0 / (1.0 + (age_days + 1.0).ln())
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

/// FTS5 keyword search using trigram tokens. The query is matched as a
/// prefix-allowing phrase. cwd boost and recency are applied client-side.
pub fn keyword(
    conn: &Connection,
    query: &str,
    cwd: Option<&Path>,
    cwd_only: bool,
    limit: usize,
) -> Result<Vec<Hit>> {
    let cwd_s = cwd.map(|p| p.to_string_lossy().into_owned());
    let q = sanitize_fts_query(query);
    if q.is_empty() {
        return Ok(vec![]);
    }

    // Column layout must match map_hit (16 cols) followed by bm25 rank.
    let mut sql =
        "SELECT s.session_id, s.ai_title, s.cwd, s.mtime, s.msg_count, s.first_prompt, s.file_path,
                s.git_branch, s.pr_number, s.pr_url, s.pr_repo, s.project_dir,
                s.tokens_input, s.tokens_output, s.tokens_cache_read, s.tokens_cache_create,
                bm25(sessions_fts) AS rank
         FROM sessions_fts JOIN sessions s ON s.rowid = sessions_fts.rowid
         WHERE sessions_fts MATCH ?"
            .to_string();
    if cwd_only {
        sql.push_str(" AND s.cwd = ?");
    }
    sql.push_str(" ORDER BY rank LIMIT ?");

    let mut stmt = conn.prepare(&sql)?;

    let mut hits: Vec<(Hit, f64)> = Vec::new();
    let mut rows = if cwd_only {
        stmt.query(params![q, cwd_s.as_deref().unwrap_or(""), limit as i64 * 2])?
    } else {
        stmt.query(params![q, limit as i64 * 2])?
    };
    while let Some(r) = rows.next()? {
        let h = map_hit(r)?;
        let rank: f64 = r.get(16).unwrap_or(0.0);
        hits.push((h, rank));
    }

    // Score = bm25 (lower is better → negate) + cwd boost + recency.
    let mut scored: Vec<Hit> = hits
        .into_iter()
        .map(|(mut h, rank)| {
            let recency = recency_score(h.mtime);
            let cwd_boost = match cwd_s.as_deref() {
                Some(c) if c == h.cwd => 1.0,
                _ => 0.0,
            };
            let composite = -rank + cwd_boost * 2.0 + recency * 1.0;
            h.scores.keyword = Some(composite);
            h.scores.recency = Some(recency);
            h.labels.push("keyword".to_string());
            h
        })
        .collect();
    scored.sort_by(|a, b| {
        b.scores
            .keyword
            .partial_cmp(&a.scores.keyword)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.truncate(limit);

    Ok(annotate(scored, cwd_s.as_deref()))
}

fn sanitize_fts_query(q: &str) -> String {
    // Wrap in double quotes to treat as a phrase; escape internal quotes.
    let escaped = q.replace('"', "\"\"");
    format!("\"{}\"", escaped.trim())
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

/// Vector KNN search. Embeds the query, runs `sessions_vec MATCH`, and
/// hydrates the matched rows from `sessions`. Results are annotated with the
/// `vec` label and the cosine-distance score (lower = more similar).
#[cfg(feature = "embed")]
pub fn vector(
    conn: &Connection,
    query: &str,
    cwd: Option<&Path>,
    cwd_only: bool,
    limit: usize,
) -> Result<Vec<Hit>> {
    if query.trim().is_empty() {
        return Ok(vec![]);
    }
    let qv = crate::index::embed::embed_query(query)?;
    let k_expand = (limit * 2).max(20);
    let matches = crate::index::embed::knn(conn, &qv, k_expand)?;
    if matches.is_empty() {
        return Ok(vec![]);
    }

    let cwd_s = cwd.map(|p| p.to_string_lossy().into_owned());
    let mut out: Vec<Hit> = Vec::with_capacity(matches.len());
    for (session_id, dist) in matches {
        if let Some(mut h) = show(conn, &session_id)? {
            if cwd_only {
                if let Some(c) = cwd_s.as_deref() {
                    if c != h.cwd {
                        continue;
                    }
                }
            }
            h.scores.vector = Some(dist);
            h.scores.recency = Some(recency_score(h.mtime));
            h.labels.push("semantic".to_string());
            out.push(h);
        }
        if out.len() >= limit {
            break;
        }
    }
    Ok(annotate(out, cwd_s.as_deref()))
}

/// Merge keyword and vector hits, deduping by session_id (keyword wins). The
/// returned vector keeps keyword hits first, then vector-only hits.
pub fn merge(mut kw: Vec<Hit>, vec_hits: Vec<Hit>) -> Vec<Hit> {
    let seen: std::collections::HashSet<String> = kw.iter().map(|h| h.session_id.clone()).collect();
    for mut h in vec_hits {
        if seen.contains(&h.session_id) {
            continue;
        }
        // Vector-only hits: ensure the `semantic` label is present.
        if !h.labels.iter().any(|l| l == "semantic") {
            h.labels.push("semantic".to_string());
        }
        kw.push(h);
    }
    kw
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
