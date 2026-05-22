use anyhow::Result;
use rusqlite::Connection;

/// Bump this whenever the schema or extracted-column set changes. On open the
/// DB's `user_version` is compared; if it differs we drop all tables and let
/// `ensure` rebuild + the next `scan_and_update` re-populate from JSONL.
const SCHEMA_VERSION: u32 = 4;

pub fn ensure(conn: &Connection) -> Result<()> {
    let current: u32 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap_or(0);
    if current != SCHEMA_VERSION {
        conn.execute_batch(
            r#"
            DROP TABLE IF EXISTS sessions_fts;
            DROP TABLE IF EXISTS sessions;
            "#,
        )?;
    }

    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS sessions (
            session_id    TEXT PRIMARY KEY,
            project_dir   TEXT NOT NULL,
            cwd           TEXT NOT NULL,
            ai_title      TEXT,
            first_prompt  TEXT,
            preview       TEXT,
            mtime         INTEGER NOT NULL,
            size          INTEGER NOT NULL,
            msg_count     INTEGER,
            file_path     TEXT NOT NULL,
            embedded_at   INTEGER,
            git_branch    TEXT,
            pr_number     INTEGER,
            pr_url        TEXT,
            pr_repo       TEXT,
            tokens_input        INTEGER NOT NULL DEFAULT 0,
            tokens_output       INTEGER NOT NULL DEFAULT 0,
            tokens_cache_read   INTEGER NOT NULL DEFAULT 0,
            tokens_cache_create INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_sessions_mtime ON sessions(mtime DESC);
        CREATE INDEX IF NOT EXISTS idx_sessions_cwd   ON sessions(cwd);

        CREATE VIRTUAL TABLE IF NOT EXISTS sessions_fts USING fts5(
            preview,
            cwd,
            content='sessions',
            content_rowid='rowid',
            tokenize='trigram'
        );

        CREATE TRIGGER IF NOT EXISTS sessions_ai AFTER INSERT ON sessions BEGIN
            INSERT INTO sessions_fts(rowid, preview, cwd) VALUES (new.rowid, new.preview, new.cwd);
        END;
        CREATE TRIGGER IF NOT EXISTS sessions_ad AFTER DELETE ON sessions BEGIN
            INSERT INTO sessions_fts(sessions_fts, rowid, preview, cwd) VALUES('delete', old.rowid, old.preview, old.cwd);
        END;
        CREATE TRIGGER IF NOT EXISTS sessions_au AFTER UPDATE ON sessions BEGIN
            INSERT INTO sessions_fts(sessions_fts, rowid, preview, cwd) VALUES('delete', old.rowid, old.preview, old.cwd);
            INSERT INTO sessions_fts(rowid, preview, cwd) VALUES (new.rowid, new.preview, new.cwd);
        END;
        "#,
    )?;

    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}
