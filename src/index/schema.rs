use anyhow::Result;
use rusqlite::Connection;

/// Bump this whenever the schema or extracted-column set changes. On open the
/// DB's `user_version` is compared; if it differs we drop all tables and let
/// `ensure` rebuild + the next `scan_and_update` re-populate from JSONL.
const SCHEMA_VERSION: u32 = 8;

pub fn ensure(conn: &Connection) -> Result<()> {
    let current: u32 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap_or(0);
    if current != SCHEMA_VERSION {
        conn.execute_batch(
            r#"
            DROP TABLE IF EXISTS messages_fts;
            DROP TABLE IF EXISTS sessions_fts;
            DROP TABLE IF EXISTS messages;
            DROP TABLE IF EXISTS sessions;
            "#,
        )?;
    }

    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS sessions (
            session_id    TEXT PRIMARY KEY,
            agent         TEXT NOT NULL DEFAULT 'claude',
            native_session_id TEXT NOT NULL DEFAULT '',
            source_group  TEXT,
            cwd           TEXT NOT NULL,
            ai_title      TEXT,
            first_prompt  TEXT,
            mtime         INTEGER NOT NULL,
            size          INTEGER NOT NULL,
            msg_count     INTEGER,
            file_path     TEXT NOT NULL,
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

        CREATE TABLE IF NOT EXISTS messages (
            id          INTEGER PRIMARY KEY,
            session_id  TEXT NOT NULL,
            turn_index  INTEGER NOT NULL,
            role        TEXT NOT NULL,
            text        TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id);

        CREATE VIRTUAL TABLE IF NOT EXISTS sessions_fts USING fts5(
            ai_title,
            first_prompt,
            cwd,
            content='sessions',
            content_rowid='rowid',
            tokenize='trigram'
        );

        CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
            text,
            content='messages',
            content_rowid='id',
            tokenize='trigram'
        );

        CREATE TRIGGER IF NOT EXISTS sessions_ai AFTER INSERT ON sessions BEGIN
            INSERT INTO sessions_fts(rowid, ai_title, first_prompt, cwd)
            VALUES (new.rowid, new.ai_title, new.first_prompt, new.cwd);
        END;
        CREATE TRIGGER IF NOT EXISTS sessions_ad AFTER DELETE ON sessions BEGIN
            INSERT INTO sessions_fts(sessions_fts, rowid, ai_title, first_prompt, cwd)
            VALUES('delete', old.rowid, old.ai_title, old.first_prompt, old.cwd);
            DELETE FROM messages WHERE session_id = old.session_id;
        END;
        CREATE TRIGGER IF NOT EXISTS sessions_au AFTER UPDATE ON sessions BEGIN
            INSERT INTO sessions_fts(sessions_fts, rowid, ai_title, first_prompt, cwd)
            VALUES('delete', old.rowid, old.ai_title, old.first_prompt, old.cwd);
            INSERT INTO sessions_fts(rowid, ai_title, first_prompt, cwd)
            VALUES (new.rowid, new.ai_title, new.first_prompt, new.cwd);
        END;

        CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
            INSERT INTO messages_fts(rowid, text) VALUES (new.id, new.text);
        END;
        CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
            INSERT INTO messages_fts(messages_fts, rowid, text) VALUES('delete', old.id, old.text);
        END;
        CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE ON messages BEGIN
            INSERT INTO messages_fts(messages_fts, rowid, text) VALUES('delete', old.id, old.text);
            INSERT INTO messages_fts(rowid, text) VALUES (new.id, new.text);
        END;
        "#,
    )?;

    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn open_indexed_db() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        ensure(&conn).expect("schema");
        conn
    }

    fn table_columns(conn: &Connection, table: &str) -> Vec<String> {
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .expect("pragma");
        stmt.query_map([], |r| r.get(1))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("columns")
    }

    fn insert_session(conn: &Connection, id: &str) {
        conn.execute(
            "INSERT INTO sessions
               (session_id, cwd, mtime, size, file_path)
             VALUES (?1, '/cwd', 0, 0, '/f')",
            params![id],
        )
        .expect("insert session");
    }

    #[test]
    fn message_schema_is_created() {
        let conn = open_indexed_db();

        assert_eq!(
            table_columns(&conn, "messages"),
            ["id", "session_id", "turn_index", "role", "text"]
        );
        assert_eq!(table_columns(&conn, "messages_fts"), ["text"]);
    }

    #[test]
    fn session_schema_has_no_embedding_leftovers() {
        let conn = open_indexed_db();
        let columns = table_columns(&conn, "sessions");

        assert!(!columns.iter().any(|column| column == "preview"));
        assert!(!columns.iter().any(|column| column == "embedded_at"));
    }

    #[test]
    fn session_schema_tracks_source_identity() {
        let conn = open_indexed_db();
        let columns = table_columns(&conn, "sessions");

        assert!(columns.iter().any(|column| column == "agent"));
        assert!(columns.iter().any(|column| column == "native_session_id"));
        assert!(columns.iter().any(|column| column == "source_group"));
        assert!(!columns.iter().any(|column| column == "project_dir"));
    }

    #[test]
    fn message_fts_tracks_message_changes() {
        let conn = open_indexed_db();
        insert_session(&conn, "s1");
        conn.execute(
            "INSERT INTO messages (session_id, turn_index, role, text)
             VALUES ('s1', 0, 'user', 'phasebody initial')",
            [],
        )
        .expect("insert message");

        let count = conn
            .query_row(
                "SELECT count(*) FROM messages_fts WHERE messages_fts MATCH 'phasebody'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .expect("message fts count");
        assert_eq!(count, 1);

        conn.execute(
            "UPDATE messages SET text = 'changedbody' WHERE session_id = 's1'",
            [],
        )
        .expect("update message");
        let old_count = conn
            .query_row(
                "SELECT count(*) FROM messages_fts WHERE messages_fts MATCH 'phasebody'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .expect("old message fts count");
        let new_count = conn
            .query_row(
                "SELECT count(*) FROM messages_fts WHERE messages_fts MATCH 'changedbody'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .expect("new message fts count");

        assert_eq!(old_count, 0);
        assert_eq!(new_count, 1);
    }

    #[test]
    fn deleting_session_deletes_messages_and_message_fts() {
        let conn = open_indexed_db();
        insert_session(&conn, "s1");
        conn.execute(
            "INSERT INTO messages (session_id, turn_index, role, text)
             VALUES ('s1', 0, 'user', 'phasebody deleted')",
            [],
        )
        .expect("insert message");

        conn.execute("DELETE FROM sessions WHERE session_id = 's1'", [])
            .expect("delete session");

        let message_count = conn
            .query_row("SELECT count(*) FROM messages", [], |r| r.get::<_, i64>(0))
            .expect("message count");
        let fts_count = conn
            .query_row(
                "SELECT count(*) FROM messages_fts WHERE messages_fts MATCH 'phasebody'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .expect("message fts count");

        assert_eq!(message_count, 0);
        assert_eq!(fts_count, 0);
    }
}
