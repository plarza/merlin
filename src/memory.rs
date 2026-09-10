//! Durable memory.
//!
//! Flat by design: no agent or tenant foreign key, so renaming the bot does not
//! orphan its records.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

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
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening memory db at {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA).context("creating memory schema")?;
        Ok(Self { conn })
    }

    /// Upsert by `key` so re-filing the same subject revises it rather than
    /// accumulating near-duplicates.
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
        Ok(())
    }

    /// BM25 keyword search. Falls back to a LIKE scan when the query has no
    /// usable FTS tokens, so a search for punctuation or a bare id still works.
    pub fn recall(&self, query: &str, limit: usize) -> Result<Vec<Record>> {
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
        let n = self
            .conn
            .execute("DELETE FROM memories WHERE key = ?1", params![key])?;
        Ok(n > 0)
    }

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

        let mut stmt = src.prepare(
            "SELECT id, key, content, category, created_at, updated_at FROM memories",
        )?;
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
            // The legacy table allows duplicate keys across agents; ours does
            // not. Skipping a collision keeps the first, which is the older.
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
    Ok(Record {
        key: r.get(0)?,
        content: r.get(1)?,
        category: r.get(2)?,
        created_at: r.get(3)?,
    })
}

/// FTS5 treats most punctuation as syntax, so a raw user query can be a syntax
/// error rather than a miss. Keep alphanumerics, OR the terms together.
fn sanitize_fts(query: &str) -> String {
    let terms: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() > 1)
        .map(|t| format!("\"{}\"", t.to_lowercase()))
        .collect();
    terms.join(" OR ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Memory {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        Memory { conn }
    }

    #[test]
    fn store_then_recall() {
        let m = mem();
        m.store("zog", "Zog is an alien with his own file", "core", None)
            .unwrap();
        let hits = m.recall("zog alien", 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].key, "zog");
    }

    #[test]
    fn store_is_upsert_not_duplicate() {
        let m = mem();
        m.store("zog", "first", "core", None).unwrap();
        m.store("zog", "second", "core", None).unwrap();
        assert_eq!(m.count().unwrap(), 1);
        assert_eq!(m.recall("zog", 5).unwrap()[0].content, "second");
    }

    #[test]
    fn punctuation_query_does_not_error() {
        let m = mem();
        m.store("k", "a note about !rooms:servers", "core", None)
            .unwrap();
        assert!(m.recall("!!!", 5).is_ok());
        assert!(m.recall("rooms", 5).is_ok());
    }

    #[test]
    fn forget_removes_and_reports() {
        let m = mem();
        m.store("k", "v", "core", None).unwrap();
        assert!(m.forget("k").unwrap());
        assert!(!m.forget("k").unwrap());
        assert_eq!(m.count().unwrap(), 0);
    }
}
