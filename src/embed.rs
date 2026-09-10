//! Semantic search.
//!
//! Embeddings come from OpenRouter and live in sqlite-vec virtual tables beside the rows they describe.
//! Cosine is the distance metric because it ignores magnitude, and Matryoshka truncation returns vectors that are not unit length.
//!
//! This is a second retrieval path rather than a component of the first.
//! Keyword search answers "who said this exact thing" and semantic search answers "what was said about this", and their scores are never mixed:
//! BM25 ranks by term statistics and cosine ranks by direction in embedding space, so a weighted sum of the two is a number with no meaning.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use crate::memory::Memory;
use crate::messages::Archive;

const EMBED_URL: &str = "https://openrouter.ai/api/v1/embeddings";

/// Inputs longer than this are cut before embedding.
/// Long inputs cost more and dilute the vector, and the tail of a long message rarely changes what it is about.
const MAX_INPUT_CHARS: usize = 4000;

pub struct Embedder {
    http: reqwest::Client,
    api_key: String,
    model: String,
    dimensions: usize,
}

impl Embedder {
    pub fn new(api_key: String, model: String, dimensions: usize, timeout_s: u64) -> Result<Self> {
        let http = reqwest::Client::builder()
            .read_timeout(Duration::from_secs(timeout_s))
            .connect_timeout(Duration::from_secs(20))
            .build()?;
        Ok(Self {
            http,
            api_key,
            model,
            dimensions,
        })
    }

    pub fn dimensions(&self) -> usize {
        self.dimensions
    }

    /// Embed a batch, returning one vector per input in the same order.
    pub async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let input: Vec<String> = texts.iter().map(|t| truncate(t, MAX_INPUT_CHARS)).collect();
        let body = json!({
            "model": self.model,
            "input": input,
            "dimensions": self.dimensions,
        });

        let resp = self
            .http
            .post(EMBED_URL)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .context("calling OpenRouter embeddings")?;

        let status = resp.status();
        let raw = resp.text().await.context("reading embeddings response")?;
        if !status.is_success() {
            anyhow::bail!(
                "OpenRouter embeddings {status}: {}",
                raw.chars().take(200).collect::<String>()
            );
        }

        let payload: Value = serde_json::from_str(&raw).context("decoding embeddings response")?;
        let data = payload
            .get("data")
            .and_then(Value::as_array)
            .context("embeddings response had no data array")?;

        // The wire format carries an index per item and does not promise request order.
        let mut indexed: Vec<(usize, Vec<f32>)> = Vec::with_capacity(data.len());
        for item in data {
            let index = item.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let vector = item
                .get("embedding")
                .and_then(Value::as_array)
                .context("embedding item had no embedding array")?
                .iter()
                .map(|v| v.as_f64().unwrap_or(0.0) as f32)
                .collect::<Vec<f32>>();

            if vector.len() != self.dimensions {
                anyhow::bail!(
                    "model returned {} dimensions, expected {}",
                    vector.len(),
                    self.dimensions
                );
            }
            indexed.push((index, vector));
        }

        if indexed.len() != texts.len() {
            anyhow::bail!(
                "asked for {} embeddings, got {}",
                texts.len(),
                indexed.len()
            );
        }

        indexed.sort_by_key(|(i, _)| *i);
        Ok(indexed.into_iter().map(|(_, v)| v).collect())
    }
}

/// sqlite-vec is a compiled-in extension rather than a loadable one, and it has to be registered before any connection is opened.
pub fn register() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute::<
            *const (),
            unsafe extern "C" fn(
                *mut rusqlite::ffi::sqlite3,
                *mut *mut i8,
                *const rusqlite::ffi::sqlite3_api_routines,
            ) -> i32,
        >(
            sqlite_vec::sqlite3_vec_init as *const ()
        )));
    });
}

/// Create the vector table, and drop it first if it was built for a different model or width.
///
/// A vec0 table fixes its dimension at creation, so changing either setting leaves rows that can never be compared against a new query.
/// Rebuilding is cheap: the loop re-embeds whatever is missing.
pub fn ensure_table(
    conn: &Connection,
    table: &str,
    key: &str,
    model: &str,
    dimensions: usize,
) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS embedding_meta (
           table_name TEXT PRIMARY KEY, model TEXT NOT NULL, dimensions INTEGER NOT NULL);",
    )?;

    let current: Option<(String, i64)> = conn
        .query_row(
            "SELECT model, dimensions FROM embedding_meta WHERE table_name = ?1",
            params![table],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();

    if let Some((have_model, have_dims)) = &current
        && (have_model != model || *have_dims != dimensions as i64)
    {
        tracing::warn!(
            %table, from = %have_model, from_dims = have_dims, to = %model, to_dims = dimensions,
            "embedding model changed; discarding stored vectors"
        );
        conn.execute_batch(&format!("DROP TABLE IF EXISTS {table};"))?;
    }

    conn.execute_batch(&format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS {table} USING vec0(
           {key} INTEGER PRIMARY KEY,
           embedding float[{dimensions}] distance_metric=cosine
         );"
    ))?;
    conn.execute(
        "INSERT INTO embedding_meta (table_name, model, dimensions) VALUES (?1, ?2, ?3)
         ON CONFLICT(table_name) DO UPDATE SET model = excluded.model, dimensions = excluded.dimensions",
        params![table, model, dimensions as i64],
    )?;
    Ok(())
}

pub fn count_pending(conn: &Connection, source: &str, table: &str, key: &str) -> Result<i64> {
    let sql =
        format!("SELECT count(*) FROM {source} WHERE rowid NOT IN (SELECT {key} FROM {table})");
    Ok(conn.query_row(&sql, [], |r| r.get(0))?)
}

pub fn save(
    conn: &mut Connection,
    table: &str,
    key: &str,
    rows: &[(i64, Vec<f32>)],
) -> Result<usize> {
    let tx = conn.transaction()?;
    {
        let sql = format!("INSERT OR REPLACE INTO {table}({key}, embedding) VALUES (?1, ?2)");
        let mut stmt = tx.prepare(&sql)?;
        for (rowid, vector) in rows {
            stmt.execute(params![rowid, to_blob(vector)])?;
        }
    }
    tx.commit()?;
    Ok(rows.len())
}

/// Drop one row's vector, so a rewritten row is re-embedded rather than found under its old meaning.
pub fn invalidate(conn: &Connection, table: &str, key: &str, rowid: i64) -> Result<()> {
    let sql = format!("DELETE FROM {table} WHERE {key} = ?1");
    conn.execute(&sql, params![rowid])?;
    Ok(())
}

/// The `k` nearest rowids, closest first.
pub fn nearest(
    conn: &Connection,
    table: &str,
    key: &str,
    query: &[f32],
    k: usize,
) -> Result<Vec<i64>> {
    let sql = format!(
        "SELECT {key} FROM {table}
         WHERE embedding MATCH ?1 AND k = ?2
         ORDER BY distance"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![to_blob(query), k as i64], |r| r.get(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// A store whose rows can be embedded.
/// Memories and messages differ in every other respect but drain identically, so the backlog loop is written once against this.
pub trait Embeddable {
    /// Rows with no vector yet, newest first, as (rowid, text to embed).
    fn pending_embeddings(&self, limit: usize) -> Result<Vec<(i64, String)>>;
    fn save_embeddings(&mut self, rows: &[(i64, Vec<f32>)]) -> Result<usize>;
}

/// Load rows by rowid, preserving the order given and skipping any that have gone.
/// Vector search returns an order that a SQL `IN` clause would discard, which is why these are fetched one at a time.
pub fn load_ordered<T>(
    conn: &Connection,
    sql: &str,
    ids: &[i64],
    map: impl Fn(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>> {
    let mut stmt = conn.prepare(sql)?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(row) = stmt.query_row(params![id], &map).optional()? {
            out.push(row);
        }
    }
    Ok(out)
}

/// Rank a candidate set by cosine against the query vector.
/// Candidates with no vector yet fall in behind everything scored rather than disappearing, which keeps results sane mid-backfill.
pub fn rank<T>(
    conn: &Connection,
    table: &str,
    key: &str,
    candidates: Vec<(i64, T)>,
    vector: &[f32],
    limit: usize,
) -> Result<Vec<T>> {
    let ids: Vec<i64> = candidates.iter().map(|(id, _)| *id).collect();
    let vectors = vectors_for(conn, table, key, &ids)?;
    if vectors.is_empty() {
        return Ok(Vec::new());
    }

    let mut scored: Vec<(f32, T)> = Vec::new();
    let mut unscored: Vec<T> = Vec::new();
    for (id, row) in candidates {
        match vectors.get(&id) {
            Some(v) => scored.push((cosine(vector, v), row)),
            None => unscored.push(row),
        }
    }
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    let mut out: Vec<T> = scored.into_iter().map(|(_, r)| r).collect();
    out.extend(unscored);
    out.truncate(limit);
    Ok(out)
}

/// Vectors for specific rowids, for ranking a candidate set that was chosen by some other index.
/// Rows with no vector yet are absent from the map rather than an error, since the backlog loop may not have reached them.
pub fn vectors_for(
    conn: &Connection,
    table: &str,
    key: &str,
    rowids: &[i64],
) -> Result<std::collections::HashMap<i64, Vec<f32>>> {
    let sql = format!("SELECT embedding FROM {table} WHERE {key} = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let mut out = std::collections::HashMap::with_capacity(rowids.len());
    for id in rowids {
        let blob: Option<Vec<u8>> = stmt
            .query_row(params![id], |r| r.get(0))
            .optional()
            .unwrap_or(None);
        if let Some(blob) = blob {
            out.insert(*id, from_blob(&blob));
        }
    }
    Ok(out)
}

/// Cosine similarity, higher is closer.
/// Magnitude is divided out, so truncated vectors of any length compare correctly.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

fn from_blob(b: &[u8]) -> Vec<f32> {
    b.as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
        .collect()
}

/// sqlite-vec reads a float vector as raw little-endian f32.
fn to_blob(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

/// Embeds whatever is not embedded yet, forever.
///
/// One mechanism covers both the initial backfill and steady state, so there is no separate import path that can drift from the live one.
/// Writes never block on the network: a new row is simply pending until this loop reaches it.
pub async fn run(
    embedder: Arc<Embedder>,
    memory: Arc<Mutex<Memory>>,
    archive: Arc<Mutex<Archive>>,
    batch: usize,
) {
    let idle = Duration::from_secs(60);

    loop {
        let mut worked = false;

        match drain(&embedder, memory.as_ref(), batch).await {
            Ok(0) => {}
            Ok(n) => {
                worked = true;
                tracing::info!(count = n, "embedded memories");
            }
            Err(e) => tracing::warn!(error = %e, "embedding memories failed"),
        }

        match drain(&embedder, archive.as_ref(), batch).await {
            Ok(0) => {}
            Ok(n) => {
                worked = true;
                tracing::info!(count = n, "embedded messages");
            }
            Err(e) => tracing::warn!(error = %e, "embedding messages failed"),
        }

        // Pause between batches while catching up, and sleep properly once there is nothing left.
        tokio::time::sleep(if worked { Duration::from_secs(2) } else { idle }).await;
    }
}

async fn drain<S: Embeddable>(
    embedder: &Embedder,
    store: &Mutex<S>,
    batch: usize,
) -> Result<usize> {
    // The lock is released before the request: holding it across an await would stall every tool for the duration of the call.
    let work = {
        let store = store.lock().unwrap();
        store.pending_embeddings(batch)?
    };
    if work.is_empty() {
        return Ok(0);
    }

    let texts: Vec<String> = work.iter().map(|(_, t)| t.clone()).collect();
    let vectors = embedder.embed(&texts).await?;
    let rows: Vec<(i64, Vec<f32>)> = work.iter().map(|(id, _)| *id).zip(vectors).collect();

    store.lock().unwrap().save_embeddings(&rows)
}
