//! Message archive.
//!
//! Two FTS5 indexes over the same rows.
//! The default tokenizer serves exact term search.
//! Fuzzy search uses the trigram index to gather candidates,
//! then ranks them by Jaro-Winkler similarity: trigram MATCH requires every trigram of the query to be present,
//! so it cannot match through a typo by itself.
//!
//! Similarity comes from rapidfuzz,
//! whose implementations are bit-parallel and carry no dependencies of their own.

use anyhow::{Context, Result};
use rapidfuzz::distance::jaro_winkler;
use rusqlite::{Connection, params};
use std::path::Path;

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

    pub fn count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))?)
    }

    /// Search with Google-style syntax: bare words match approximately,
    /// quoted words must appear exactly,
    /// and the two combine.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Archived>> {
        let terms = parse_query(query);
        if terms.is_empty() {
            return Ok(Vec::new());
        }

        let exact: Vec<&Term> = terms.iter().filter(|t| t.exact).collect();
        let loose: Vec<&Term> = terms.iter().filter(|t| !t.exact).collect();

        // Quoted terms are requirements,
        // so they select the candidate set.
        let candidates = if !exact.is_empty() {
            let expr = exact
                .iter()
                .map(|t| format!("\"{}\"", escape(&t.text)))
                .collect::<Vec<_>>()
                .join(" AND ");
            let over = if loose.is_empty() {
                limit
            } else {
                limit.saturating_mul(8).max(40)
            };
            self.query_index("messages_fts", &expr, over)?
        } else {
            self.trigram_candidates(&loose, limit.saturating_mul(8).max(40))?
        };

        // With nothing loose to rank by,
        // FTS order already stands.
        if loose.is_empty() {
            return Ok(candidates.into_iter().take(limit).collect());
        }

        let words: Vec<String> = loose.iter().map(|t| t.text.clone()).collect();
        let mut scored: Vec<(f64, Archived)> = candidates
            .into_iter()
            .map(|row| (best_similarity(&words, &row.body), row))
            .filter(|(score, _)| *score >= 0.82)
            .collect();

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        let ranked: Vec<Archived> = scored.into_iter().take(limit).map(|(_, r)| r).collect();

        // A required term with no fuzzy neighbour still beats returning nothing.
        if ranked.is_empty() && !exact.is_empty() {
            let expr = exact
                .iter()
                .map(|t| format!("\"{}\"", escape(&t.text)))
                .collect::<Vec<_>>()
                .join(" AND ");
            return self.query_index("messages_fts", &expr, limit);
        }
        Ok(ranked)
    }

    /// Rows sharing any trigram with the loose terms.
    /// Cheap and generous; the similarity pass does the real filtering.
    fn trigram_candidates(&self, loose: &[&Term], limit: usize) -> Result<Vec<Archived>> {
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

    fn query_index(&self, index: &str, expr: &str, limit: usize) -> Result<Vec<Archived>> {
        let sql = format!(
            "SELECT m.sender, m.body, m.at
             FROM {index} f JOIN messages m ON m.rowid = f.rowid
             WHERE {index} MATCH ?1
             ORDER BY bm25({index}) LIMIT ?2"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![expr, limit as i64], |r| {
                Ok(Archived {
                    sender: r.get(0)?,
                    body: r.get(1)?,
                    at: r.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

/// One parsed query term.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Term {
    text: String,
    exact: bool,
}

/// Split a query into terms,
/// treating double-quoted runs as exact.
/// An unterminated quote is treated as if it closed at the end.
fn parse_query(query: &str) -> Vec<Term> {
    let mut terms = Vec::new();
    let mut buf = String::new();
    let mut in_quotes = false;

    let flush = |buf: &mut String, exact: bool, terms: &mut Vec<Term>| {
        let text = buf.trim().to_lowercase();
        buf.clear();
        // Trigram needs three characters; exact terms are useful shorter.
        let floor = if exact { 1 } else { 3 };
        if text.chars().count() >= floor {
            terms.push(Term { text, exact });
        }
    };

    for c in query.chars() {
        match c {
            '"' => {
                flush(&mut buf, in_quotes, &mut terms);
                in_quotes = !in_quotes;
            }
            c if c.is_whitespace() && !in_quotes => flush(&mut buf, false, &mut terms),
            c if c.is_alphanumeric() || in_quotes => buf.push(c),
            _ => flush(&mut buf, false, &mut terms),
        }
    }
    flush(&mut buf, in_quotes, &mut terms);
    terms
}

/// FTS5 string literals escape a quote by doubling it.
fn escape(term: &str) -> String {
    term.replace('"', "\"\"")
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
