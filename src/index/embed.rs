//! Embedding generation via fastembed (multilingual MiniLM, 384-dim).

#![cfg(feature = "embed")]

use std::sync::OnceLock;

use anyhow::{Context, Result};
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use rusqlite::{params, Connection};

static MODEL: OnceLock<std::sync::Mutex<TextEmbedding>> = OnceLock::new();

fn model() -> Result<&'static std::sync::Mutex<TextEmbedding>> {
    if MODEL.get().is_none() {
        let cache = crate::paths::cache_root().join("models");
        std::fs::create_dir_all(&cache)?;
        let m = TextEmbedding::try_new(
            InitOptions::new(EmbeddingModel::ParaphraseMLMiniLML12V2)
                .with_show_download_progress(false)
                .with_cache_dir(cache),
        )
        .context("init fastembed model")?;
        let _ = MODEL.set(std::sync::Mutex::new(m));
    }
    Ok(MODEL.get().expect("model just initialized"))
}

/// Embed a single query string. Returns a 384-dim f32 vector.
pub fn embed_query(text: &str) -> Result<Vec<f32>> {
    let lock = model()?;
    let m = lock.lock().expect("model poisoned");
    let v = m.embed(vec![text.to_string()], None)?;
    Ok(v.into_iter().next().unwrap_or_default())
}

/// Embed a batch of preview strings.
pub fn embed_batch(texts: &[String]) -> Result<Vec<Vec<f32>>> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let lock = model()?;
    let m = lock.lock().expect("model poisoned");
    m.embed(texts.to_vec(), None)
}

/// Generate embeddings for all sessions whose `embedded_at IS NULL` and store
/// them in `sessions_vec`. `progress_fn` is called as batches complete.
pub fn refresh_embeddings(
    conn: &mut Connection,
    batch_size: usize,
    mut progress_fn: impl FnMut(u32, u32),
) -> Result<u32> {
    let mut pending: Vec<(String, String)> = Vec::new();
    {
        let mut q = conn.prepare(
            "SELECT session_id, preview FROM sessions WHERE embedded_at IS NULL AND preview <> ''",
        )?;
        let rows = q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for r in rows {
            pending.push(r?);
        }
    }
    let total = pending.len() as u32;
    if total == 0 {
        progress_fn(0, 0);
        return Ok(0);
    }

    let mut done: u32 = 0;
    for chunk in pending.chunks(batch_size) {
        let texts: Vec<String> = chunk.iter().map(|(_, p)| p.clone()).collect();
        let vectors = embed_batch(&texts)?;

        let tx = conn.transaction()?;
        for ((session_id, _), v) in chunk.iter().zip(vectors) {
            // Replace any existing vector row.
            let _ = tx.execute(
                "DELETE FROM sessions_vec WHERE session_id = ?",
                params![session_id],
            );
            let bytes = vec_to_bytes(&v);
            tx.execute(
                "INSERT INTO sessions_vec(session_id, embedding) VALUES (?, ?)",
                params![session_id, bytes],
            )?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            tx.execute(
                "UPDATE sessions SET embedded_at = ? WHERE session_id = ?",
                params![now, session_id],
            )?;
        }
        tx.commit()?;

        done += chunk.len() as u32;
        progress_fn(done, total);
    }
    Ok(done)
}

/// KNN search by cosine distance. Returns the top-k session ids ordered by
/// distance (smaller is more similar) along with the distance score.
pub fn knn(conn: &Connection, query_vec: &[f32], k: usize) -> Result<Vec<(String, f64)>> {
    let bytes = vec_to_bytes(query_vec);
    let mut stmt = conn.prepare(
        "SELECT session_id, distance FROM sessions_vec
         WHERE embedding MATCH ? AND k = ?
         ORDER BY distance",
    )?;
    let rows = stmt.query_map(params![bytes, k as i64], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?))
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn vec_to_bytes(v: &[f32]) -> Vec<u8> {
    let mut b = Vec::with_capacity(v.len() * 4);
    for f in v {
        b.extend_from_slice(&f.to_le_bytes());
    }
    b
}
