//! Incremental scan: walk agent session sources, diff against DB, UPSERT changes.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use crate::agent::{self, AgentKind, SourceMessage, SourceSession};

#[derive(Debug, Default, Clone)]
pub struct IngestStats {
    pub indexed: u32,
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

/// Scan all session records and update the DB.
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

    let mut known: HashMap<(AgentKind, String), (i64, i64)> = HashMap::new();
    {
        let mut q = conn.prepare("SELECT agent, session_id, mtime, size FROM sessions")?;
        let rows = q.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        for r in rows {
            let (agent, id, mt, sz) = r?;
            let Some(agent) = AgentKind::from_db(&agent) else {
                continue;
            };
            known.insert((agent, id), (mt, sz));
        }
    }

    let mut sources = Vec::new();
    for kind in agent::all_kinds() {
        match agent::list_sessions(*kind) {
            Ok(records) => sources.push((*kind, records)),
            Err(err) => tracing::warn!("scan {} sessions: {}", kind, err),
        }
    }

    stats.total = sources
        .iter()
        .map(|(_, records)| records.len() as u32)
        .sum();
    progress.on_total(stats.total);

    let mut seen_by_agent: HashMap<AgentKind, HashSet<String>> = HashMap::new();
    let mut done = 0u32;

    for (kind, records) in &sources {
        let seen = seen_by_agent.entry(*kind).or_default();
        for record in records {
            progress.on_file(done, stats.total, &record.path);
            done = done.saturating_add(1);
            seen.insert(record.session_id.clone());

            let key = (*kind, record.session_id.clone());
            let stale = reindex
                || match known.get(&key) {
                    Some((m, s)) => *m != record.mtime || *s != record.size,
                    None => true,
                };
            if !stale {
                continue;
            }

            let (meta, messages) = match agent::extract_session(record) {
                Ok(session) => session,
                Err(e) => {
                    tracing::warn!("parse {}: {}", record.path.display(), e);
                    continue;
                }
            };
            upsert(conn, &meta)?;
            replace_messages(conn, &meta.session_id, &messages)?;
            stats.upserted += 1;
            stats.indexed += 1;
        }
    }

    let to_delete: Vec<(AgentKind, String)> = known
        .keys()
        .filter(|(kind, id)| {
            seen_by_agent
                .get(kind)
                .map(|seen| !seen.contains(id))
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    for (kind, id) in &to_delete {
        conn.execute(
            "DELETE FROM sessions WHERE agent = ?1 AND session_id = ?2",
            params![kind.as_str(), id],
        )?;
        stats.deleted += 1;
    }

    progress.on_done(&stats);
    Ok(stats)
}

fn replace_messages(conn: &Connection, session_id: &str, messages: &[SourceMessage]) -> Result<()> {
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

fn upsert(conn: &Connection, m: &SourceSession) -> Result<()> {
    conn.execute(
        r#"INSERT INTO sessions
              (session_id, agent, native_session_id, source_group,
               cwd, ai_title, first_prompt,
               mtime, size, msg_count, file_path,
               git_branch, pr_number, pr_url, pr_repo,
               tokens_input, tokens_output, tokens_cache_read, tokens_cache_create,
               model, models_json)
           VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21)
           ON CONFLICT(session_id) DO UPDATE SET
              agent=excluded.agent,
              native_session_id=excluded.native_session_id,
              source_group=excluded.source_group,
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
              tokens_cache_create=excluded.tokens_cache_create,
              model=excluded.model,
              models_json=excluded.models_json
        "#,
        params![
            m.session_id,
            m.agent.as_str(),
            m.native_session_id,
            m.source_group.as_deref(),
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
            m.model.as_deref(),
            serde_json::to_string(&m.models).unwrap_or_else(|_| "[]".to_string()),
        ],
    )
    .with_context(|| format!("upsert {}", m.session_id))?;
    Ok(())
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
               (session_id, cwd, mtime, size, file_path)
             VALUES (?1, '/cwd', 0, 0, '/f')",
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
            &[SourceMessage {
                turn_index: 0,
                role: "user".to_string(),
                text: "oldphase text".to_string(),
            }],
        )
        .expect("insert old messages");

        replace_messages(
            &conn,
            "s1",
            &[SourceMessage {
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
            &[SourceMessage {
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
