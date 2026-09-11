use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::sync::{Mutex, Once};
use std::time::Duration;

const EMBED_URL: &str = "https://openrouter.ai/api/v1/embeddings";

const MAX_INPUT_CHARS: usize = 4000;

pub trait Embeddable {
    const SOURCE: &'static str;
    const TEXT: &'static str;
    const TABLE: &'static str;
    const KEY: &'static str;
}

pub struct Memories;
impl Embeddable for Memories {
    const SOURCE: &'static str = "memories";
    const TEXT: &'static str = "content";
    const TABLE: &'static str = "memory_vectors";
    const KEY: &'static str = "memory_rowid";
}

pub struct Messages;
impl Embeddable for Messages {
    const SOURCE: &'static str = "messages";
    const TEXT: &'static str = "body";
    const TABLE: &'static str = "message_vectors";
    const KEY: &'static str = "message_rowid";
}

pub struct Embedder {
    http: reqwest::Client,
    api_key: String,
    model: String,
    dimensions: usize,
}

impl Embedder {
    pub fn new(api_key: String, model: String, dimensions: usize, timeout_s: u64) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .read_timeout(Duration::from_secs(timeout_s))
                .connect_timeout(Duration::from_secs(20))
                .build()?,
            api_key,
            model,
            dimensions,
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn dimensions(&self) -> usize {
        self.dimensions
    }

    pub async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let input: Vec<String> = texts
            .iter()
            .map(|t| t.chars().take(MAX_INPUT_CHARS).collect())
            .collect();

        let resp = self
            .http
            .post(EMBED_URL)
            .bearer_auth(&self.api_key)
            .json(&json!({ "model": self.model, "input": input, "dimensions": self.dimensions }))
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

        let mut indexed: Vec<(usize, Vec<f32>)> = data
            .iter()
            .map(|item| {
                let index = item.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let vector: Vec<f32> = item
                    .get("embedding")
                    .and_then(Value::as_array)
                    .context("embedding item had no embedding array")?
                    .iter()
                    .map(|v| v.as_f64().unwrap_or(0.0) as f32)
                    .collect();
                anyhow::ensure!(
                    vector.len() == self.dimensions,
                    "model returned {} dimensions, expected {}",
                    vector.len(),
                    self.dimensions
                );
                Ok((index, vector))
            })
            .collect::<Result<_>>()?;

        anyhow::ensure!(
            indexed.len() == texts.len(),
            "asked for {} embeddings, got {}",
            texts.len(),
            indexed.len()
        );

        indexed.sort_by_key(|(i, _)| *i);
        Ok(indexed.into_iter().map(|(_, v)| v).collect())
    }
}

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

pub fn ensure_table<E: Embeddable>(
    conn: &Connection,
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
            params![E::TABLE],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;

    if let Some((have_model, have_dims)) = &current
        && (have_model != model || *have_dims != dimensions as i64)
    {
        tracing::warn!(
            table = E::TABLE, from = %have_model, to = %model,
            "embedding model changed; discarding stored vectors"
        );
        conn.execute_batch(&format!("DROP TABLE IF EXISTS {};", E::TABLE))?;
    }

    conn.execute_batch(&format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS {} USING vec0(
           {} INTEGER PRIMARY KEY,
           embedding float[{dimensions}] distance_metric=cosine);",
        E::TABLE,
        E::KEY
    ))?;
    conn.execute(
        "INSERT INTO embedding_meta (table_name, model, dimensions) VALUES (?1, ?2, ?3)
         ON CONFLICT(table_name) DO UPDATE SET model = excluded.model, dimensions = excluded.dimensions",
        params![E::TABLE, model, dimensions as i64],
    )?;
    Ok(())
}

pub fn pending<E: Embeddable>(conn: &Connection, limit: usize) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT rowid, {} FROM {} WHERE rowid NOT IN (SELECT {} FROM {})
         ORDER BY rowid DESC LIMIT ?1",
        E::TEXT,
        E::SOURCE,
        E::KEY,
        E::TABLE
    ))?;
    Ok(stmt
        .query_map(params![limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<Vec<_>, _>>()?)
}

pub fn count_pending<E: Embeddable>(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row(
        &format!(
            "SELECT count(*) FROM {} WHERE rowid NOT IN (SELECT {} FROM {})",
            E::SOURCE,
            E::KEY,
            E::TABLE
        ),
        [],
        |r| r.get(0),
    )?)
}

pub fn save<E: Embeddable>(conn: &mut Connection, rows: &[(i64, Vec<f32>)]) -> Result<usize> {
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare(&format!(
            "INSERT OR REPLACE INTO {}({}, embedding) VALUES (?1, ?2)",
            E::TABLE,
            E::KEY
        ))?;
        for (rowid, vector) in rows {
            stmt.execute(params![rowid, to_blob(vector)])?;
        }
    }
    tx.commit()?;
    Ok(rows.len())
}

pub fn invalidate<E: Embeddable>(conn: &Connection, rowid: i64) -> Result<()> {
    conn.execute(
        &format!("DELETE FROM {} WHERE {} = ?1", E::TABLE, E::KEY),
        params![rowid],
    )?;
    Ok(())
}

pub fn nearest<E: Embeddable>(conn: &Connection, query: &[f32], k: usize) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {} FROM {} WHERE embedding MATCH ?1 AND k = ?2 ORDER BY distance",
        E::KEY,
        E::TABLE
    ))?;
    Ok(stmt
        .query_map(params![to_blob(query), k as i64], |r| r.get(0))?
        .collect::<Result<Vec<_>, _>>()?)
}

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

pub fn rank<E: Embeddable, T>(
    conn: &Connection,
    candidates: Vec<(i64, T)>,
    vector: &[f32],
    limit: usize,
) -> Result<Vec<T>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT embedding FROM {} WHERE {} = ?1",
        E::TABLE,
        E::KEY
    ))?;

    let mut scored: Vec<(f32, T)> = Vec::new();
    let mut unscored: Vec<T> = Vec::new();
    for (id, row) in candidates {
        let blob: Option<Vec<u8>> = stmt.query_row(params![id], |r| r.get(0)).optional()?;
        match blob {
            Some(b) => scored.push((cosine(vector, &from_blob(&b)), row)),
            None => unscored.push(row),
        }
    }

    if scored.is_empty() {
        return Ok(Vec::new());
    }
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    let mut out: Vec<T> = scored.into_iter().map(|(_, r)| r).collect();
    out.extend(unscored);
    out.truncate(limit);
    Ok(out)
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
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

fn to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn from_blob(b: &[u8]) -> Vec<f32> {
    b.as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
        .collect()
}

pub async fn run(
    embedder: std::sync::Arc<Embedder>,
    db: std::sync::Arc<Mutex<Connection>>,
    batch: usize,
) {
    loop {
        let mut worked = false;
        for (label, drained) in [
            ("memories", drain::<Memories>(&embedder, &db, batch).await),
            ("messages", drain::<Messages>(&embedder, &db, batch).await),
        ] {
            match drained {
                Ok(0) => {}
                Ok(n) => {
                    worked = true;
                    tracing::info!(count = n, "embedded {label}");
                }
                Err(e) => tracing::warn!(error = %e, "embedding {label} failed"),
            }
        }

        tokio::time::sleep(Duration::from_secs(if worked { 2 } else { 60 })).await;
    }
}

async fn drain<E: Embeddable>(
    embedder: &Embedder,
    db: &Mutex<Connection>,
    batch: usize,
) -> Result<usize> {
    let work = pending::<E>(&db.lock().unwrap(), batch)?;
    if work.is_empty() {
        return Ok(0);
    }

    let texts: Vec<String> = work.iter().map(|(_, t)| t.clone()).collect();
    let vectors = embedder.embed(&texts).await?;
    let rows: Vec<(i64, Vec<f32>)> = work.iter().map(|(id, _)| *id).zip(vectors).collect();

    save::<E>(&mut db.lock().unwrap(), &rows)
}
