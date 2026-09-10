//! Message archive.
//!
//! One search, two mechanisms, split by the query itself.
//! A quoted term is a requirement and goes to the default FTS5 index; everything unquoted describes the subject and is matched by embedding similarity.
//! So `world cup "2025"` means messages that definitely contain 2025, ranked by how much they are about the world cup, whatever words they used for it.
//!
//! The trigram index and Jaro-Winkler ranking remain as the fallback for when a vector is not available:
//! during the initial backfill most rows have no embedding yet, and returning nothing until it finishes would be worse than returning approximate matches.

use anyhow::{Context, Result};
use rapidfuzz::distance::jaro_winkler;
use rusqlite::{Connection, params};
use std::path::Path;

use crate::embed;
use crate::query::{Term, escape, parse, required_expr};

const VEC_TABLE: &str = "message_vectors";
const VEC_KEY: &str = "message_rowid";

/// How many required-term matches are pulled before ranking them by meaning.
const CANDIDATE_CAP: usize = 500;

pub struct Archive {
    conn: Connection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Archived {
    pub sender: String,
    pub body: String,
    pub at: String,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS messages (
  event_id TEXT PRIMARY KEY,
  room_id  TEXT NOT NULL,
  sender   TEXT NOT NULL,
  body     TEXT NOT NULL,
  at       TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_messages_room ON messages(room_id, at);

CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts
  USING fts5(body, content='messages', content_rowid='rowid');

-- Trigram tokenizer: matches substrings, which the default tokenizer cannot,
-- and provides the candidate set that fuzzy ranking scores.
CREATE VIRTUAL TABLE IF NOT EXISTS messages_trigram
  USING fts5(body, content='messages', content_rowid='rowid', tokenize='trigram');

CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
  INSERT INTO messages_fts(rowid, body) VALUES (new.rowid, new.body);
  INSERT INTO messages_trigram(rowid, body) VALUES (new.rowid, new.body);
END;

CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
  INSERT INTO messages_fts(messages_fts, rowid, body) VALUES ('delete', old.rowid, old.body);
  INSERT INTO messages_trigram(messages_trigram, rowid, body) VALUES ('delete', old.rowid, old.body);
END;
"#;

impl Archive {
    pub fn open(path: &Path) -> Result<Self> {
        embed::register();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening message archive at {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA)
            .context("creating message schema")?;
        Ok(Self { conn })
    }

    /// Keyed by event id so a replayed sync does not duplicate a message.
    pub fn record(
        &self,
        event_id: &str,
        room_id: &str,
        sender: &str,
        body: &str,
        at: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO messages (event_id, room_id, sender, body, at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![event_id, room_id, sender, body, at],
        )?;
        Ok(())
    }

    /// The newest messages in a room, oldest first.
    /// Used to refill the ambient buffer after a restart, which would otherwise leave the bot with no idea what was just being discussed.
    pub fn recent(&self, room_id: &str, limit: usize) -> Result<Vec<Archived>> {
        let mut stmt = self.conn.prepare(
            "SELECT sender, body, at FROM (
               SELECT sender, body, at FROM messages
               WHERE room_id = ?1 ORDER BY at DESC LIMIT ?2
             ) ORDER BY at ASC",
        )?;
        let rows = stmt
            .query_map(params![room_id, limit as i64], to_archived)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))?)
    }

    /// Build the vector table for a given model, discarding vectors from a different one.
    pub fn enable_semantic(&self, model: &str, dimensions: usize) -> Result<()> {
        embed::ensure_table(&self.conn, VEC_TABLE, VEC_KEY, model, dimensions)
    }

    pub fn pending_count(&self) -> Result<i64> {
        embed::count_pending(&self.conn, "messages", VEC_TABLE, VEC_KEY)
    }
}

impl embed::Embeddable for Archive {
    /// Recent history is the part most likely to be asked about, so it is embedded before the backlog.
    fn pending_embeddings(&self, limit: usize) -> Result<Vec<(i64, String)>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT rowid, body FROM messages
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

impl Archive {
    /// Search with Google-style syntax.
    /// Quoted terms must appear exactly and select the candidate set; the unquoted remainder describes the subject and ranks that set by meaning.
    ///
    /// The caller supplies the embedding of the unquoted text, because embedding is a network call and this type is held behind a lock.
    /// Passing `None`, or holding rows the backlog loop has not reached, falls back to approximate string matching rather than returning nothing.
    pub fn search(
        &self,
        query: &str,
        vector: Option<&[f32]>,
        limit: usize,
    ) -> Result<Vec<Archived>> {
        let terms = parse(query);
        if terms.is_empty() {
            return Ok(Vec::new());
        }

        let exact: Vec<&Term> = terms.iter().filter(|t| t.exact).collect();
        let loose: Vec<&Term> = terms.iter().filter(|t| !t.exact).collect();

        // Every term was quoted, so the requirements are the whole query and BM25 order stands.
        if loose.is_empty() {
            return Ok(self
                .query_index("messages_fts", &required_expr(&exact), limit)?
                .into_iter()
                .map(|(_, row)| row)
                .collect());
        }

        if let Some(vector) = vector {
            let hits = if exact.is_empty() {
                {
                    let ids = embed::nearest(&self.conn, VEC_TABLE, VEC_KEY, vector, limit)?;
                    embed::load_ordered(
                        &self.conn,
                        "SELECT sender, body, at FROM messages WHERE rowid = ?1",
                        &ids,
                        to_archived,
                    )?
                }
            } else {
                let candidates =
                    self.query_index("messages_fts", &required_expr(&exact), CANDIDATE_CAP)?;
                embed::rank(&self.conn, VEC_TABLE, VEC_KEY, candidates, vector, limit)?
            };
            if !hits.is_empty() {
                return Ok(hits);
            }
        }

        self.search_approximate(&exact, &loose, limit)
    }

    /// The fallback when no embedding is available: trigram candidates ranked by Jaro-Winkler similarity.
    fn search_approximate(
        &self,
        exact: &[&Term],
        loose: &[&Term],
        limit: usize,
    ) -> Result<Vec<Archived>> {
        let over = limit.saturating_mul(8).max(40);
        let candidates = if exact.is_empty() {
            self.trigram_candidates(loose, over)?
        } else {
            self.query_index("messages_fts", &required_expr(exact), over)?
        };

        let words: Vec<String> = loose.iter().map(|t| t.text.clone()).collect();
        let mut scored: Vec<(f64, Archived)> = candidates
            .into_iter()
            .map(|(_, row)| (best_similarity(&words, &row.body), row))
            .filter(|(score, _)| *score >= 0.82)
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        let ranked: Vec<Archived> = scored.into_iter().take(limit).map(|(_, r)| r).collect();

        // A required term with no approximate neighbour still beats returning nothing.
        if ranked.is_empty() && !exact.is_empty() {
            return Ok(self
                .query_index("messages_fts", &required_expr(exact), limit)?
                .into_iter()
                .map(|(_, r)| r)
                .collect());
        }
        Ok(ranked)
    }

    /// Rows sharing any trigram with the loose terms.
    /// Cheap and generous; the similarity pass does the real filtering.
    fn trigram_candidates(&self, loose: &[&Term], limit: usize) -> Result<Vec<(i64, Archived)>> {
        let mut grams: Vec<String> = Vec::new();
        for term in loose {
            let chars: Vec<char> = term.text.chars().collect();
            for w in chars.windows(3) {
                let g: String = w.iter().collect();
                if !grams.contains(&g) {
                    grams.push(g);
                }
            }
        }
        if grams.is_empty() {
            return Ok(Vec::new());
        }
        let expr = grams
            .iter()
            .map(|g| format!("\"{}\"", escape(g)))
            .collect::<Vec<_>>()
            .join(" OR ");
        self.query_index("messages_trigram", &expr, limit)
    }

    fn query_index(&self, index: &str, expr: &str, limit: usize) -> Result<Vec<(i64, Archived)>> {
        let sql = format!(
            "SELECT m.rowid, m.sender, m.body, m.at
             FROM {index} f JOIN messages m ON m.rowid = f.rowid
             WHERE {index} MATCH ?1
             ORDER BY bm25({index}) LIMIT ?2"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![expr, limit as i64], |r| {
                Ok((
                    r.get(0)?,
                    Archived {
                        sender: r.get(1)?,
                        body: r.get(2)?,
                        at: r.get(3)?,
                    },
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

/// Best similarity between any loose term and any word in the body.
fn best_similarity(terms: &[String], body: &str) -> f64 {
    let words: Vec<String> = body
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect();

    let mut best = 0.0_f64;
    for term in terms {
        for word in &words {
            let score = jaro_winkler::similarity(term.chars(), word.chars());
            if score > best {
                best = score;
            }
        }
    }
    best
}

fn to_archived(r: &rusqlite::Row<'_>) -> rusqlite::Result<Archived> {
    Ok(Archived {
        sender: r.get(0)?,
        body: r.get(1)?,
        at: r.get(2)?,
    })
}
