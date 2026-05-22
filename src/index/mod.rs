pub mod ingest;
pub mod schema;
pub mod search;

use std::path::PathBuf;

use anyhow::{Context, Result};
use rusqlite::Connection;

/// Open the SQLite DB at the user's cache root, creating it if needed.
pub fn open() -> Result<Connection> {
    let dir = crate::paths::cache_root();
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = db_path();
    let conn = Connection::open(&path).with_context(|| format!("open {}", path.display()))?;

    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;

    schema::ensure(&conn)?;
    Ok(conn)
}

pub fn db_path() -> PathBuf {
    let mut p = crate::paths::cache_root();
    p.push("index.db");
    p
}
