pub mod ingest;
pub mod schema;
pub mod search;

#[cfg(feature = "embed")]
pub mod embed;

use std::path::PathBuf;
use std::sync::Once;

use anyhow::{Context, Result};
use rusqlite::Connection;

static REGISTER_VEC: Once = Once::new();

fn register_sqlite_vec() {
    REGISTER_VEC.call_once(|| unsafe {
        type SqliteAutoExtFn = unsafe extern "C" fn(
            *mut rusqlite::ffi::sqlite3,
            *mut *mut i8,
            *const rusqlite::ffi::sqlite3_api_routines,
        ) -> i32;
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute::<
            *const (),
            SqliteAutoExtFn,
        >(
            sqlite_vec::sqlite3_vec_init as *const ()
        )));
    });
}

/// Open the SQLite DB at the user's cache root, creating it if needed.
pub fn open() -> Result<Connection> {
    register_sqlite_vec();

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
