//! Durable memory.
//!
//! Flat by design: no agent or tenant foreign key, so renaming the bot does not orphan its records.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

use crate::embed;
use crate::query::{Term, loose_text, parse, required_expr};

const VEC_TABLE: &str = "memory_vectors";
const VEC_KEY: &str = "memory_rowid";

/// How many required-term matches are pulled before ranking them by meaning.
const CANDIDATE_CAP: usize = 500;

pub struct Memory {
    conn: Connection,
}

#[derive(Debug, Clone)]
pub struct Record {
    pub key: String,
    pub content: String,
    pub category: String,
    pub created_at: String,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS memories (
  id         TEXT PRIMARY KEY,
  key        TEXT NOT NULL UNIQUE,
  content    TEXT NOT NULL,
  category   TEXT NOT NULL DEFAULT 'core',
  room_id    TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts
  USING fts5(key, content, content='memories', content_rowid='rowid');

CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
  INSERT INTO memories_fts(rowid, key, content)
  VALUES (new.rowid, new.key, new.content);
END;

CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
  INSERT INTO memories_fts(memories_fts, rowid, key, content)
  VALUES ('delete', old.rowid, old.key, old.content);
END;

CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE ON memories BEGIN
  INSERT INTO memories_fts(memories_fts, rowid, key, content)
  VALUES ('delete', old.rowid, old.key, old.content);
  INSERT INTO memories_fts(rowid, key, content)
  VALUES (new.rowid, new.key, new.content);
END;
"#;

impl Memory {
    pub fn open(path: &Path) -> Result<Self> {
        embed::register();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening memory db at {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA)
            .context("creating memory schema")?;
        Ok(Self { conn })
    }

    /// Upsert by `key` so re-filing the same subject revises it rather than accumulating near-duplicates.
    pub fn store(
        &self,
        key: &str,
        content: &str,
        category: &str,
        room_id: Option<&str>,
    ) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO memories (id, key, content, category, room_id, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
             ON CONFLICT(key) DO UPDATE SET
               content = excluded.content,
               category = excluded.category,
               updated_at = excluded.updated_at",
            params![
                uuid::Uuid::new_v4().to_string(),
                key,
                content,
                category,
                room_id,
                now
            ],
        )?;

        // A revised entry keeps its rowid, so its vector would otherwise survive as a description of the old text.
        if let Some(rowid) = self.rowid(key)? {
            embed::invalidate(&self.conn, VEC_TABLE, VEC_KEY, rowid).ok();
        }
        Ok(())
    }

    fn rowid(&self, key: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT rowid FROM memories WHERE key = ?1",
                params![key],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Search with the shared query syntax.
    /// Quoted terms must appear exactly and select the candidate set; the unquoted remainder ranks that set by meaning.
    ///
    /// The caller supplies the embedding of the unquoted text, since embedding is a network call and this type is held behind a lock.
    /// Without a vector, or before the backlog loop has reached these rows, this falls back to keyword matching.
    pub fn recall(&self, query: &str, vector: Option<&[f32]>, limit: usize) -> Result<Vec<Record>> {
        let terms = parse(query);
        let exact: Vec<&Term> = terms.iter().filter(|t| t.exact).collect();

        // Every term was quoted, so the requirements are the whole query.
        if !exact.is_empty() && loose_text(query).is_empty() {
            let hits = self.required(&exact, limit)?;
            if !hits.is_empty() {
                return Ok(hits.into_iter().map(|(_, r)| r).collect());
            }
        }

        if let Some(vector) = vector {
            let hits = if exact.is_empty() {
                {
                    let ids = embed::nearest(&self.conn, VEC_TABLE, VEC_KEY, vector, limit)?;
                    embed::load_ordered(
                        &self.conn,
                        "SELECT key, content, category, created_at FROM memories WHERE rowid = ?1",
                        &ids,
                        row_to_record,
                    )?
                }
            } else {
                let candidates = self.required(&exact, CANDIDATE_CAP)?;
                embed::rank(&self.conn, VEC_TABLE, VEC_KEY, candidates, vector, limit)?
            };
            if !hits.is_empty() {
                return Ok(hits);
            }
        }

        self.recall_keyword(query, limit)
    }

    /// Memories containing every quoted term, in BM25 order.
    fn required(&self, exact: &[&Term], limit: usize) -> Result<Vec<(i64, Record)>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.rowid, m.key, m.content, m.category, m.created_at
             FROM memories_fts f
             JOIN memories m ON m.rowid = f.rowid
             WHERE memories_fts MATCH ?1
             ORDER BY bm25(memories_fts) LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![required_expr(exact), limit as i64], |r| {
                Ok((r.get(0)?, row_to_record_from(r, 1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The fallback when no embedding is available.
    /// Drops to a LIKE scan when the query has no usable FTS tokens, so a search for punctuation or a bare id still works.
    fn recall_keyword(&self, query: &str, limit: usize) -> Result<Vec<Record>> {
        let cleaned = sanitize_fts(query);

        if !cleaned.is_empty() {
            let mut stmt = self.conn.prepare(
                "SELECT m.key, m.content, m.category, m.created_at
                 FROM memories_fts f
                 JOIN memories m ON m.rowid = f.rowid
                 WHERE memories_fts MATCH ?1
                 ORDER BY bm25(memories_fts) LIMIT ?2",
            )?;
            let rows = stmt
                .query_map(params![cleaned, limit as i64], row_to_record)?
                .collect::<Result<Vec<_>, _>>()?;
            if !rows.is_empty() {
                return Ok(rows);
            }
        }

        let mut stmt = self.conn.prepare(
            "SELECT key, content, category, created_at FROM memories
             WHERE content LIKE ?1 OR key LIKE ?1
             ORDER BY updated_at DESC LIMIT ?2",
        )?;
        let pattern = format!("%{query}%");
        let rows = stmt
            .query_map(params![pattern, limit as i64], row_to_record)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn forget(&self, key: &str) -> Result<bool> {
        // Delete the vector first, while the rowid is still resolvable.
        // SQLite reuses freed rowids, so an orphaned vector would eventually answer for whichever memory lands on that rowid next.
        if let Some(rowid) = self.rowid(key)? {
            embed::invalidate(&self.conn, VEC_TABLE, VEC_KEY, rowid).ok();
        }
        let n = self
            .conn
            .execute("DELETE FROM memories WHERE key = ?1", params![key])?;
        Ok(n > 0)
    }

    /// Build the vector table for a given model, discarding vectors from a different one.
    pub fn enable_semantic(&self, model: &str, dimensions: usize) -> Result<()> {
        embed::ensure_table(&self.conn, VEC_TABLE, VEC_KEY, model, dimensions)
    }

    pub fn pending_count(&self) -> Result<i64> {
        embed::count_pending(&self.conn, "memories", VEC_TABLE, VEC_KEY)
    }
}

impl embed::Embeddable for Memory {
    /// Only the content is embedded.
    /// Keys are frequently opaque identifiers rather than descriptions, and feeding one into an embedding is noise.
    fn pending_embeddings(&self, limit: usize) -> Result<Vec<(i64, String)>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT rowid, content FROM memories
             WHERE rowid NOT IN (SELECT {VEC_KEY} FROM {VEC_TABLE})
             ORDER BY rowid DESC LIMIT ?1"
        ))?;
        let rows = stmt
            .query_map(params![limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn save_embeddings(&mut self, rows: &[(i64, Vec<f32>)]) -> Result<usize> {
        embed::save(&mut self.conn, VEC_TABLE, VEC_KEY, rows)
    }
}

impl Memory {
    pub fn count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT count(*) FROM memories", [], |r| r.get(0))
            .optional()?
            .unwrap_or(0))
    }

    /// One-shot import from a compatible table, dropping any `agent_id`.
    /// Returns how many rows were taken.
    pub fn import_legacy(&mut self, legacy: &Path) -> Result<usize> {
        let src = Connection::open(legacy)
            .with_context(|| format!("opening legacy db at {}", legacy.display()))?;

        let mut stmt =
            src.prepare("SELECT id, key, content, category, created_at, updated_at FROM memories")?;
        let rows: Vec<(String, String, String, String, String, String)> = stmt
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let tx = self.conn.transaction()?;
        let mut taken = 0usize;
        for (id, key, content, category, created, updated) in rows {
            // The legacy table allows duplicate keys across agents; ours does not.
            // Skipping a collision keeps the first, which is the older.
            let n = tx.execute(
                "INSERT OR IGNORE INTO memories
                   (id, key, content, category, room_id, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6)",
                params![id, key, content, category, created, updated],
            )?;
            taken += n;
        }
        tx.commit()?;
        Ok(taken)
    }
}

fn row_to_record(r: &rusqlite::Row<'_>) -> rusqlite::Result<Record> {
    row_to_record_from(r, 0)
}

/// The same four columns, at an offset, for queries that select the rowid first.
fn row_to_record_from(r: &rusqlite::Row<'_>, base: usize) -> rusqlite::Result<Record> {
    Ok(Record {
        key: r.get(base)?,
        content: r.get(base + 1)?,
        category: r.get(base + 2)?,
        created_at: r.get(base + 3)?,
    })
}

/// FTS5 treats most punctuation as syntax, so a raw user query can be a syntax error rather than a miss.
/// Keep alphanumerics, OR the terms together.
fn sanitize_fts(query: &str) -> String {
    let terms: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() > 1)
        .map(|t| format!("\"{}\"", t.to_lowercase()))
        .collect();
    terms.join(" OR ")
}
