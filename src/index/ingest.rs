//! Incremental scan: walk agent session sources, diff against DB, UPSERT changes.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use crate::agent::{self, AgentKind, SourceMessage, SourceSession};
use crate::session::TrajectoryStep;

#[derive(Debug, Default, Clone)]
pub struct IngestStats {
    pub indexed: u32,
    pub upserted: u32,
    /// Sessions whose source vanished and were flagged as archived this scan.
    pub archived: u32,
    /// Previously archived sessions whose source reappeared this scan.
    pub unarchived: u32,
    pub total: u32,
}

#[derive(Clone, Copy)]
struct KnownRow {
    mtime: i64,
    size: i64,
    archived: bool,
}

fn now_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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

    let mut known: HashMap<(AgentKind, String), KnownRow> = HashMap::new();
    {
        let mut q =
            conn.prepare("SELECT agent, session_id, mtime, size, archived_at FROM sessions")?;
        let rows = q.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, Option<i64>>(4)?,
            ))
        })?;
        for r in rows {
            let (agent, id, mt, sz, archived_at) = r?;
            let Some(agent) = AgentKind::from_db(&agent) else {
                continue;
            };
            known.insert(
                (agent, id),
                KnownRow {
                    mtime: mt,
                    size: sz,
                    archived: archived_at.is_some(),
                },
            );
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
                    Some(row) => row.mtime != record.mtime || row.size != record.size,
                    None => true,
                };
            if !stale {
                continue;
            }

            let extracted = match agent::extract_session(record) {
                Ok(session) => session,
                Err(e) => {
                    tracing::warn!("parse {}: {}", record.path.display(), e);
                    continue;
                }
            };
            let meta = &extracted.session;
            upsert(conn, meta)?;
            replace_messages(conn, &meta.session_id, &extracted.messages)?;
            replace_trajectory(conn, &meta.session_id, meta.agent, &extracted.trajectory)?;
            stats.upserted += 1;
            stats.indexed += 1;
        }
    }

    let (archived, unarchived) =
        reconcile_archive_state(conn, &known, &seen_by_agent, now_unix_secs())?;
    stats.archived = archived;
    stats.unarchived = unarchived;

    progress.on_done(&stats);
    Ok(stats)
}

/// Reconcile each known session's archive flag against what the scan saw.
///
/// Sessions whose source vanished are archived, not deleted: this index is the
/// long-term home for sessions whose source agent has already expired them
/// (Claude's `cleanupPeriodDays`, ~30 days). Archiving an agent's sessions
/// requires that agent to have produced at least one record this run, so a
/// source that is merely unreadable — whether it errors or just resolves to an
/// empty/missing root (moved mount, changed path, permissions blip) — leaves
/// its sessions untouched rather than misflagging them all as gone. A
/// previously archived session whose source reappears is un-archived, making
/// the flag self-healing.
fn reconcile_archive_state(
    conn: &Connection,
    known: &HashMap<(AgentKind, String), KnownRow>,
    seen_by_agent: &HashMap<AgentKind, HashSet<String>>,
    now: i64,
) -> Result<(u32, u32)> {
    let mut archived = 0u32;
    let mut unarchived = 0u32;
    for ((kind, id), row) in known {
        let agent_seen = seen_by_agent.get(kind);
        let seen = agent_seen.map(|seen| seen.contains(id)).unwrap_or(false);
        // Only archive when the agent proved its source is readable this run by
        // returning at least one session; an empty result is treated as "source
        // unavailable", not "everything vanished".
        let source_readable = agent_seen.map(|seen| !seen.is_empty()).unwrap_or(false);
        if seen {
            if row.archived {
                conn.execute(
                    "UPDATE sessions SET archived_at = NULL WHERE agent = ?1 AND session_id = ?2",
                    params![kind.as_str(), id],
                )?;
                unarchived += 1;
            }
        } else if source_readable && !row.archived {
            conn.execute(
                "UPDATE sessions SET archived_at = ?3 WHERE agent = ?1 AND session_id = ?2",
                params![kind.as_str(), id, now],
            )?;
            archived += 1;
        }
    }
    Ok((archived, unarchived))
}

/// Remove archived sessions (those whose source has vanished) from the index.
///
/// With `older_than_secs`, only archived rows whose `archived_at` is at least
/// that many seconds in the past are removed; `None` removes every archived
/// session. Live sessions are never touched. Returns the number of rows
/// deleted.
pub fn prune_archived(conn: &Connection, older_than_secs: Option<i64>) -> Result<u32> {
    let deleted = match older_than_secs {
        Some(secs) => {
            let cutoff = now_unix_secs() - secs;
            conn.execute(
                "DELETE FROM sessions WHERE archived_at IS NOT NULL AND archived_at <= ?1",
                params![cutoff],
            )?
        }
        None => conn.execute("DELETE FROM sessions WHERE archived_at IS NOT NULL", [])?,
    };
    Ok(deleted as u32)
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

fn replace_trajectory(
    conn: &Connection,
    session_id: &str,
    agent: AgentKind,
    steps: &[TrajectoryStep],
) -> Result<()> {
    conn.execute(
        "DELETE FROM trajectory WHERE session_id = ?",
        params![session_id],
    )?;

    let mut stmt = conn.prepare(
        "INSERT INTO trajectory
           (session_id, agent, step_index, autonomous_run_index, role, tool_name, tool_input,
            tool_input_bytes, tool_result_bytes, tool_result, is_error,
            tokens_input, tokens_output, tokens_cache_read, tokens_cache_create,
            timestamp, is_sidechain, context_management,
            is_api_error, api_error_status, retry_attempt, max_retries,
            stop_reason, attribution_mcp_tool, attribution_mcp_server,
            attribution_skill, duration_ms, permission_mode, parent_uuid)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,
                 ?18,?19,?20,?21,?22,?23,?24,?25,?26,?27,?28,?29)",
    )?;
    for step in steps {
        stmt.execute(params![
            session_id,
            agent.as_str(),
            step.step_index as i64,
            step.autonomous_run_index as i64,
            step.role,
            step.tool_name,
            step.tool_input,
            step.tool_input_bytes as i64,
            step.tool_result_bytes as i64,
            step.tool_result,
            step.is_error as i64,
            step.tokens_input as i64,
            step.tokens_output as i64,
            step.tokens_cache_read as i64,
            step.tokens_cache_create as i64,
            step.timestamp,
            step.is_sidechain as i64,
            step.context_management,
            step.is_api_error as i64,
            step.api_error_status,
            step.retry_attempt,
            step.max_retries,
            step.stop_reason,
            step.attribution_mcp_tool,
            step.attribution_mcp_server,
            step.attribution_skill,
            step.duration_ms,
            step.permission_mode,
            step.parent_uuid,
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
               model, models_json,
               tool_call_count, tool_error_count, thinking_tokens, wall_clock_ms)
           VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,
                   ?22,?23,?24,?25)
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
              models_json=excluded.models_json,
              tool_call_count=excluded.tool_call_count,
              tool_error_count=excluded.tool_error_count,
              thinking_tokens=excluded.thinking_tokens,
              wall_clock_ms=excluded.wall_clock_ms
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
            m.tool_call_count as i64,
            m.tool_error_count as i64,
            m.thinking_tokens as i64,
            m.wall_clock_ms,
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

    fn insert_agent_session(conn: &Connection, agent: AgentKind, session_id: &str) {
        conn.execute(
            "INSERT INTO sessions
               (session_id, agent, cwd, mtime, size, file_path)
             VALUES (?1, ?2, '/cwd', 0, 0, '/f')",
            params![session_id, agent.as_str()],
        )
        .expect("insert agent session");
    }

    fn set_archived(conn: &Connection, session_id: &str, archived_at: i64) {
        conn.execute(
            "UPDATE sessions SET archived_at = ?2 WHERE session_id = ?1",
            params![session_id, archived_at],
        )
        .expect("set archived");
    }

    fn archived_at(conn: &Connection, session_id: &str) -> Option<i64> {
        conn.query_row(
            "SELECT archived_at FROM sessions WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )
        .expect("archived_at")
    }

    fn known_row(mtime: i64, size: i64, archived: bool) -> KnownRow {
        KnownRow {
            mtime,
            size,
            archived,
        }
    }

    #[test]
    fn reconcile_archives_vanished_session() {
        let conn = open_indexed_db();
        insert_agent_session(&conn, AgentKind::Claude, "s1");
        insert_agent_session(&conn, AgentKind::Claude, "still_here");

        let mut known = HashMap::new();
        known.insert(
            (AgentKind::Claude, "s1".to_string()),
            known_row(0, 0, false),
        );
        // The agent also returned a live session this run, proving the source is
        // readable, so the vanished `s1` is safe to archive.
        let mut seen_by_agent: HashMap<AgentKind, HashSet<String>> = HashMap::new();
        seen_by_agent.insert(AgentKind::Claude, HashSet::from(["still_here".to_string()]));

        let (archived, unarchived) =
            reconcile_archive_state(&conn, &known, &seen_by_agent, 1000).unwrap();

        assert_eq!((archived, unarchived), (1, 0));
        assert_eq!(archived_at(&conn, "s1"), Some(1000));
    }

    #[test]
    fn reconcile_skips_archive_when_agent_scan_is_empty() {
        let conn = open_indexed_db();
        insert_agent_session(&conn, AgentKind::Claude, "s1");

        let mut known = HashMap::new();
        known.insert(
            (AgentKind::Claude, "s1".to_string()),
            known_row(0, 0, false),
        );
        // Source resolved to an empty/missing root this run (e.g. unmounted):
        // a successful-but-empty scan must not archive everything.
        let mut seen_by_agent: HashMap<AgentKind, HashSet<String>> = HashMap::new();
        seen_by_agent.insert(AgentKind::Claude, HashSet::new());

        let (archived, unarchived) =
            reconcile_archive_state(&conn, &known, &seen_by_agent, 1000).unwrap();

        assert_eq!((archived, unarchived), (0, 0));
        assert_eq!(archived_at(&conn, "s1"), None);
    }

    #[test]
    fn reconcile_unarchives_reappeared_session() {
        let conn = open_indexed_db();
        insert_agent_session(&conn, AgentKind::Claude, "s1");
        set_archived(&conn, "s1", 500);

        let mut known = HashMap::new();
        known.insert((AgentKind::Claude, "s1".to_string()), known_row(0, 0, true));
        let mut seen_by_agent: HashMap<AgentKind, HashSet<String>> = HashMap::new();
        seen_by_agent.insert(AgentKind::Claude, HashSet::from(["s1".to_string()]));

        let (archived, unarchived) =
            reconcile_archive_state(&conn, &known, &seen_by_agent, 1000).unwrap();

        assert_eq!((archived, unarchived), (0, 1));
        assert_eq!(archived_at(&conn, "s1"), None);
    }

    #[test]
    fn reconcile_skips_session_from_unscanned_agent() {
        let conn = open_indexed_db();
        insert_agent_session(&conn, AgentKind::Claude, "s1");

        let mut known = HashMap::new();
        known.insert(
            (AgentKind::Claude, "s1".to_string()),
            known_row(0, 0, false),
        );
        // Claude's source errored entirely this run: no entry in seen_by_agent.
        let seen_by_agent: HashMap<AgentKind, HashSet<String>> = HashMap::new();

        let (archived, unarchived) =
            reconcile_archive_state(&conn, &known, &seen_by_agent, 1000).unwrap();

        assert_eq!((archived, unarchived), (0, 0));
        assert_eq!(archived_at(&conn, "s1"), None);
    }

    #[test]
    fn prune_archived_removes_only_archived_sessions() {
        let conn = open_indexed_db();
        insert_agent_session(&conn, AgentKind::Claude, "live");
        insert_agent_session(&conn, AgentKind::Claude, "gone");
        set_archived(&conn, "gone", 100);

        let deleted = prune_archived(&conn, None).unwrap();

        assert_eq!(deleted, 1);
        let remaining: Vec<String> = conn
            .prepare("SELECT session_id FROM sessions ORDER BY session_id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(remaining, vec!["live".to_string()]);
    }

    #[test]
    fn prune_archived_respects_age_cutoff() {
        let conn = open_indexed_db();
        insert_agent_session(&conn, AgentKind::Claude, "old");
        insert_agent_session(&conn, AgentKind::Claude, "recent");
        let now = now_unix_secs();
        set_archived(&conn, "old", now - 40 * 86_400);
        set_archived(&conn, "recent", now - 86_400);

        let deleted = prune_archived(&conn, Some(30 * 86_400)).unwrap();

        assert_eq!(deleted, 1);
        assert!(archived_at(&conn, "recent").is_some());
        let old_exists: i64 = conn
            .query_row(
                "SELECT count(*) FROM sessions WHERE session_id = 'old'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(old_exists, 0);
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
    fn replace_trajectory_overwrites_prior_steps() {
        let conn = open_indexed_db();
        insert_session(&conn, "s1");
        let step = |idx: u32, tool: &str| TrajectoryStep {
            step_index: idx,
            role: "assistant".to_string(),
            tool_name: Some(tool.to_string()),
            tool_input: Some("{}".to_string()),
            tool_input_bytes: 2,
            tokens_input: 10,
            ..Default::default()
        };

        replace_trajectory(
            &conn,
            "s1",
            AgentKind::Claude,
            &[step(0, "Edit"), step(1, "Bash")],
        )
        .expect("first write");
        replace_trajectory(&conn, "s1", AgentKind::Claude, &[step(0, "Read")]).expect("rewrite");

        let rows: Vec<(i64, String, i64)> = conn
            .prepare("SELECT step_index, tool_name, tokens_input FROM trajectory WHERE session_id='s1' ORDER BY step_index")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();

        assert_eq!(rows, vec![(0, "Read".to_string(), 10)]);
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
