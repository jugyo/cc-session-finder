//! Incremental scan: walk ~/.claude/projects, diff against DB, UPSERT changes.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use crate::session::{self, IndexableMessage, SessionMeta};

#[derive(Debug, Default, Clone)]
pub struct IngestStats {
    pub scanned: u32,
    pub upserted: u32,
    pub deleted: u32,
    pub total: u32,
}

/// Progress callback shape used by both CLI and TUI front-ends.
pub trait Progress: Send + Sync {
    fn on_total(&self, _total: u32) {}
    fn on_file(&self, _done: u32, _total: u32, _current: &Path) {}
    fn on_done(&self, _stats: &IngestStats) {}
}

pub struct NoopProgress;
impl Progress for NoopProgress {}

/// Scan all session files and update the DB.
///
/// - `reindex=true` reparses every file regardless of mtime/size.
///
/// Each upserted row commits in its own implicit transaction so that
/// concurrent readers (the search query) see the list grow as the scan
/// progresses.
pub fn scan_and_update(
    conn: &mut Connection,
    reindex: bool,
    progress: &dyn Progress,
) -> Result<IngestStats> {
    let mut stats = IngestStats::default();

    // 1. Existing rows: session_id -> (mtime, size)
    let mut known: std::collections::HashMap<String, (i64, i64)> = std::collections::HashMap::new();
    {
        let mut q = conn.prepare("SELECT session_id, mtime, size FROM sessions")?;
        let rows = q.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        for r in rows {
            let (id, mt, sz) = r?;
            known.insert(id, (mt, sz));
        }
    }

    // 2. Files on disk
    let files = list_session_files()?;
    stats.total = files.len() as u32;
    progress.on_total(stats.total);

    let mut seen: HashSet<String> = HashSet::with_capacity(files.len());

    for (i, path) in files.iter().enumerate() {
        progress.on_file(i as u32, stats.total, path);
        let id = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        seen.insert(id.clone());

        let md = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let mtime = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let size = md.len() as i64;

        let stale = reindex
            || match known.get(&id) {
                Some((m, s)) => *m != mtime || *s != size,
                None => true,
            };
        if !stale {
            continue;
        }

        let meta = match session::extract_from_file(path) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("parse {}: {}", path.display(), e);
                continue;
            }
        };
        let messages = match session::extract_indexable_messages_from_file(path) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("parse messages {}: {}", path.display(), e);
                Vec::new()
            }
        };
        upsert(conn, &meta)?;
        replace_messages(conn, &meta.session_id, &messages)?;
        stats.upserted += 1;
        stats.scanned += 1;
    }

    // 3. Delete vanished sessions
    let to_delete: Vec<String> = known
        .keys()
        .filter(|id| !seen.contains(*id))
        .cloned()
        .collect();
    for id in &to_delete {
        conn.execute("DELETE FROM sessions WHERE session_id = ?", params![id])?;
        stats.deleted += 1;
    }

    progress.on_done(&stats);
    Ok(stats)
}

fn replace_messages(
    conn: &Connection,
    session_id: &str,
    messages: &[IndexableMessage],
) -> Result<()> {
    conn.execute(
        "DELETE FROM messages WHERE session_id = ?",
        params![session_id],
    )?;

    let mut stmt = conn.prepare(
        "INSERT INTO messages (session_id, turn_index, role, text)
         VALUES (?1, ?2, ?3, ?4)",
    )?;
    for message in messages {
        stmt.execute(params![
            session_id,
            message.turn_index as i64,
            message.role,
            message.text,
        ])?;
    }

    Ok(())
}

fn upsert(conn: &Connection, m: &SessionMeta) -> Result<()> {
    conn.execute(
        r#"INSERT INTO sessions
              (session_id, project_dir, cwd, ai_title, first_prompt,
               mtime, size, msg_count, file_path,
               git_branch, pr_number, pr_url, pr_repo,
               tokens_input, tokens_output, tokens_cache_read, tokens_cache_create)
           VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)
           ON CONFLICT(session_id) DO UPDATE SET
              project_dir=excluded.project_dir,
              cwd=excluded.cwd,
              ai_title=excluded.ai_title,
              first_prompt=excluded.first_prompt,
              mtime=excluded.mtime,
              size=excluded.size,
              msg_count=excluded.msg_count,
              file_path=excluded.file_path,
              git_branch=excluded.git_branch,
              pr_number=excluded.pr_number,
              pr_url=excluded.pr_url,
              pr_repo=excluded.pr_repo,
              tokens_input=excluded.tokens_input,
              tokens_output=excluded.tokens_output,
              tokens_cache_read=excluded.tokens_cache_read,
              tokens_cache_create=excluded.tokens_cache_create
        "#,
        params![
            m.session_id,
            m.project_dir,
            m.cwd.to_string_lossy(),
            m.ai_title,
            m.first_prompt,
            m.mtime,
            m.size,
            m.msg_count,
            m.file_path.to_string_lossy(),
            m.git_branch,
            m.pr_number,
            m.pr_url,
            m.pr_repo,
            m.tokens_input as i64,
            m.tokens_output as i64,
            m.tokens_cache_read as i64,
            m.tokens_cache_create as i64,
        ],
    )
    .with_context(|| format!("upsert {}", m.session_id))?;
    Ok(())
}

fn list_session_files() -> Result<Vec<PathBuf>> {
    let root = crate::paths::projects_root();
    let pattern = format!("{}/*/*.jsonl", root.to_string_lossy());
    let mut out = Vec::new();
    for p in glob::glob(&pattern)?.flatten() {
        out.push(p);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::schema;

    fn open_indexed_db() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        schema::ensure(&conn).expect("schema");
        conn
    }

    fn insert_session(conn: &Connection, session_id: &str) {
        conn.execute(
            "INSERT INTO sessions
               (session_id, project_dir, cwd, mtime, size, file_path)
             VALUES (?1, '/p', '/cwd', 0, 0, '/f')",
            params![session_id],
        )
        .expect("insert session");
    }

    fn fts_count(conn: &Connection, query: &str) -> i64 {
        conn.query_row(
            "SELECT count(*) FROM messages_fts WHERE messages_fts MATCH ?",
            params![query],
            |r| r.get(0),
        )
        .expect("fts count")
    }

    #[test]
    fn replace_messages_removes_stale_messages_and_fts() {
        let conn = open_indexed_db();
        insert_session(&conn, "s1");
        replace_messages(
            &conn,
            "s1",
            &[IndexableMessage {
                turn_index: 0,
                role: "user".to_string(),
                text: "oldphase text".to_string(),
            }],
        )
        .expect("insert old messages");

        replace_messages(
            &conn,
            "s1",
            &[IndexableMessage {
                turn_index: 0,
                role: "assistant".to_string(),
                text: "newphase text".to_string(),
            }],
        )
        .expect("replace messages");

        let rows: Vec<(i64, String, String)> = conn
            .prepare("SELECT turn_index, role, text FROM messages WHERE session_id = 's1'")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();

        assert_eq!(
            rows,
            vec![(0, "assistant".to_string(), "newphase text".to_string())]
        );
        assert_eq!(fts_count(&conn, "oldphase"), 0);
        assert_eq!(fts_count(&conn, "newphase"), 1);
    }

    #[test]
    fn deleting_session_removes_messages_and_fts() {
        let conn = open_indexed_db();
        insert_session(&conn, "s1");
        replace_messages(
            &conn,
            "s1",
            &[IndexableMessage {
                turn_index: 0,
                role: "user".to_string(),
                text: "deletephase text".to_string(),
            }],
        )
        .expect("insert messages");

        conn.execute("DELETE FROM sessions WHERE session_id = 's1'", [])
            .expect("delete session");

        let count = conn
            .query_row("SELECT count(*) FROM messages", [], |r| r.get::<_, i64>(0))
            .expect("messages count");
        assert_eq!(count, 0);
        assert_eq!(fts_count(&conn, "deletephase"), 0);
    }
}
