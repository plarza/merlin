//! Message archive.
//!
//! Two FTS5 indexes over the same rows. The default tokenizer serves exact term
//! search. Fuzzy search uses the trigram index to gather candidates, then ranks
//! them by Jaro-Winkler similarity: trigram MATCH requires every trigram of the
//! query to be present, so it cannot match through a typo by itself.

use anyhow::{Context, Result};
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

    pub fn search(&self, query: &str, limit: usize, fuzzy: bool) -> Result<Vec<Archived>> {
        if fuzzy {
            return self.search_trigram(query, limit);
        }
        let terms = sanitize_fts(query);
        if terms.is_empty() {
            // Nothing tokenizable: fall through rather than return an FTS error.
            return self.search_trigram(query, limit);
        }
        let hits = self.query_index("messages_fts", &terms, limit)?;
        if hits.is_empty() {
            // An exact-term miss is exactly when fuzzy is worth trying.
            return self.search_trigram(query, limit);
        }
        Ok(hits)
    }

    /// Trigram MATCH is substring search, not typo tolerance: "shoelase"
    /// requires every one of its trigrams, and two of them are absent from
    /// "shoelace". So OR the trigrams to gather candidates cheaply, then rank
    /// them by edit distance in Rust.
    fn search_trigram(&self, query: &str, limit: usize) -> Result<Vec<Archived>> {
        let terms: Vec<String> = query
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| t.chars().count() >= 3)
            .map(str::to_lowercase)
            .collect();
        if terms.is_empty() {
            return Ok(Vec::new());
        }

        let mut grams: Vec<String> = Vec::new();
        for term in &terms {
            let chars: Vec<char> = term.chars().collect();
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
            .map(|g| format!("\"{g}\""))
            .collect::<Vec<_>>()
            .join(" OR ");

        // Over-fetch, because trigram overlap ranks poorly on its own.
        let candidates = self.query_index("messages_trigram", &expr, limit.saturating_mul(8).max(40))?;

        let mut scored: Vec<(f64, Archived)> = candidates
            .into_iter()
            .map(|row| (best_similarity(&terms, &row.body), row))
            .filter(|(score, _)| *score >= 0.82)
            .collect();

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        Ok(scored.into_iter().take(limit).map(|(_, row)| row).collect())
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

/// Best similarity between any query term and any word in the body.
fn best_similarity(terms: &[String], body: &str) -> f64 {
    let words: Vec<String> = body
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect();

    let mut best = 0.0_f64;
    for term in terms {
        for word in &words {
            let score = jaro_winkler(term, word);
            if score > best {
                best = score;
            }
        }
    }
    best
}

/// Jaro-Winkler similarity in [0, 1].
///
/// Preferred over Levenshtein here because it counts a transposition as one
/// edit rather than two, and typos are usually transpositions, and because the
/// prefix bonus favours words that start the same.
fn jaro_winkler(a: &str, b: &str) -> f64 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();

    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }

    // Characters further apart than this cannot be considered a match.
    let window = (a.len().max(b.len()) / 2).saturating_sub(1);

    let mut a_matched = vec![false; a.len()];
    let mut b_matched = vec![false; b.len()];
    let mut matches = 0usize;

    for (i, ca) in a.iter().enumerate() {
        let lo = i.saturating_sub(window);
        let hi = (i + window + 1).min(b.len());
        for j in lo..hi {
            if b_matched[j] || b[j] != *ca {
                continue;
            }
            a_matched[i] = true;
            b_matched[j] = true;
            matches += 1;
            break;
        }
    }

    if matches == 0 {
        return 0.0;
    }

    // Matched characters that appear in a different order.
    let mut transpositions = 0usize;
    let mut k = 0usize;
    for (i, matched) in a_matched.iter().enumerate() {
        if !matched {
            continue;
        }
        while !b_matched[k] {
            k += 1;
        }
        if a[i] != b[k] {
            transpositions += 1;
        }
        k += 1;
    }

    let m = matches as f64;
    let jaro = (m / a.len() as f64
        + m / b.len() as f64
        + (m - transpositions as f64 / 2.0) / m)
        / 3.0;

    let prefix = a
        .iter()
        .zip(b.iter())
        .take(4)
        .take_while(|(x, y)| x == y)
        .count() as f64;

    jaro + prefix * 0.1 * (1.0 - jaro)
}

/// FTS5 treats punctuation as syntax, so a raw query can be a syntax error
/// rather than a miss. Quote each term and OR them together.
fn sanitize_fts(query: &str) -> String {
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() > 1)
        .map(|t| format!("\"{}\"", t.to_lowercase()))
        .collect::<Vec<_>>()
        .join(" OR ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive() -> Archive {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        let a = Archive { conn };
        for (i, (sender, body)) in [
            ("@aiden", "file the zog shoelace incident"),
            ("@jakob", "john carroll is an australian gymnast"),
            ("@aiden", "what did the s&p do this week"),
        ]
        .iter()
        .enumerate()
        {
            a.record(
                &format!("$e{i}"),
                "!r:example.org",
                sender,
                body,
                "2026-09-10T00:00:00Z",
            )
            .unwrap();
        }
        a
    }

    #[test]
    fn exact_terms_are_found() {
        let hits = archive().search("shoelace", 5, false).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].body.contains("shoelace"));
    }

    #[test]
    fn fuzzy_survives_a_typo() {
        // The default tokenizer cannot match this; trigram can.
        let hits = archive().search("shoelase", 5, true).unwrap();
        assert_eq!(hits.len(), 1, "trigram should match through the typo");
        assert!(hits[0].body.contains("shoelace"));
    }

    #[test]
    fn exact_search_falls_back_to_fuzzy_on_a_miss() {
        // Not asking for fuzzy, but the exact term does not exist.
        let hits = archive().search("gymnas", 5, false).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].body.contains("gymnast"));
    }

    #[test]
    fn punctuation_query_does_not_error() {
        assert!(archive().search("s&p", 5, false).is_ok());
        assert!(archive().search("???", 5, false).is_ok());
    }

    #[test]
    fn duplicate_event_ids_are_ignored() {
        let a = archive();
        let before = a.count().unwrap();
        a.record("$e0", "!r:example.org", "@aiden", "different text", "now")
            .unwrap();
        assert_eq!(a.count().unwrap(), before);
    }

    #[test]
    fn short_fuzzy_queries_return_nothing_rather_than_erroring() {
        // Trigram cannot index fewer than three characters.
        assert!(archive().search("ab", 5, true).unwrap().is_empty());
    }

    #[test]
    fn jaro_winkler_ranks_typos_above_unrelated_words() {
        let typo = jaro_winkler("shoelace", "shoelase");
        let transposed = jaro_winkler("incident", "incidnet");
        let unrelated = jaro_winkler("shoelace", "elephant");
        assert!(typo > 0.9, "single substitution scored {typo}");
        assert!(transposed > 0.9, "transposition scored {transposed}");
        assert!(unrelated < 0.6, "unrelated scored {unrelated}");
        assert_eq!(jaro_winkler("same", "same"), 1.0);
    }

    #[test]
    fn transposition_beats_two_substitutions() {
        // A swap is one mistake, not two.
        let swap = jaro_winkler("gymnast", "gymnats");
        let two_subs = jaro_winkler("gymnast", "gymnaxy");
        assert!(swap > two_subs, "swap {swap} should beat substitutions {two_subs}");
    }

    #[test]
    fn unrelated_fuzzy_query_matches_nothing() {
        // The similarity floor must reject a word that merely shares trigrams.
        assert!(archive().search("elephant", 5, true).unwrap().is_empty());
    }
}
