//! Message archive.
//!
//! One search, two mechanisms, split by the query itself.
//! A quoted term is a requirement and goes to the default FTS5 index; everything unquoted describes the subject and is matched by embedding similarity.
//! So `world cup "2025"` means messages that definitely contain 2025, ranked by how much they are about the world cup, whatever words they used for it.
//!
//! The trigram index and Jaro-Winkler ranking remain the fallback for when a vector is not available:
//! during the initial backfill most rows have no embedding yet, and returning nothing until it finishes would be worse than returning approximate matches.

use anyhow::Result;
use rapidfuzz::distance::jaro_winkler;
use rusqlite::{Connection, params};

use crate::embed::{self, Messages};
use crate::query::{Term, escape, parse, required_expr};

/// How many required-term matches are pulled before ranking them by meaning.
const CANDIDATE_CAP: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Archived {
    pub sender: String,
    pub body: String,
    pub at: String,
}

/// Keyed by event id so a replayed sync does not duplicate a message.
pub fn record(
    conn: &Connection,
    event_id: &str,
    room_id: &str,
    sender: &str,
    body: &str,
    at: &str,
) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO messages (event_id, room_id, sender, body, at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![event_id, room_id, sender, body, at],
    )?;
    Ok(())
}

/// The newest messages in a room, oldest first.
/// Used to refill the ambient buffer after a restart, which would otherwise leave the bot with no idea what was just being discussed.
pub fn recent(conn: &Connection, room_id: &str, limit: usize) -> Result<Vec<Archived>> {
    let mut stmt = conn.prepare(
        "SELECT sender, body, at FROM (
           SELECT sender, body, at FROM messages WHERE room_id = ?1 ORDER BY at DESC LIMIT ?2
         ) ORDER BY at ASC",
    )?;
    Ok(stmt
        .query_map(params![room_id, limit as i64], to_archived)?
        .collect::<Result<Vec<_>, _>>()?)
}

pub fn count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT count(*) FROM messages", [], |r| r.get(0))?)
}

/// Search with the shared query syntax.
/// Quoted terms must appear exactly and select the candidate set; the unquoted remainder ranks that set by meaning.
///
/// The caller supplies the embedding of the unquoted text, because embedding is a network call and this runs under a lock.
/// Passing `None`, or holding rows the backlog loop has not reached, falls back to approximate string matching rather than returning nothing.
pub fn search(
    conn: &Connection,
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
        return Ok(
            from_index(conn, "messages_fts", &required_expr(&exact), limit)?
                .into_iter()
                .map(|(_, row)| row)
                .collect(),
        );
    }

    if let Some(vector) = vector {
        let hits = if exact.is_empty() {
            let ids = embed::nearest::<Messages>(conn, vector, limit)?;
            embed::load_ordered(
                conn,
                "SELECT sender, body, at FROM messages WHERE rowid = ?1",
                &ids,
                to_archived,
            )?
        } else {
            let candidates =
                from_index(conn, "messages_fts", &required_expr(&exact), CANDIDATE_CAP)?;
            embed::rank::<Messages, _>(conn, candidates, vector, limit)?
        };
        if !hits.is_empty() {
            return Ok(hits);
        }
    }

    approximate(conn, &exact, &loose, limit)
}

/// The fallback when no embedding is available: trigram candidates ranked by Jaro-Winkler similarity.
fn approximate(
    conn: &Connection,
    exact: &[&Term],
    loose: &[&Term],
    limit: usize,
) -> Result<Vec<Archived>> {
    let over = limit.saturating_mul(8).max(40);
    let candidates = if exact.is_empty() {
        // Rows sharing any trigram with the loose terms: cheap and generous, since the similarity pass does the real filtering.
        let mut grams: Vec<String> = Vec::new();
        for term in loose {
            for window in term.text.chars().collect::<Vec<_>>().windows(3) {
                let gram: String = window.iter().collect();
                if !grams.contains(&gram) {
                    grams.push(gram);
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
        from_index(conn, "messages_trigram", &expr, over)?
    } else {
        from_index(conn, "messages_fts", &required_expr(exact), over)?
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
        return Ok(
            from_index(conn, "messages_fts", &required_expr(exact), limit)?
                .into_iter()
                .map(|(_, r)| r)
                .collect(),
        );
    }
    Ok(ranked)
}

fn from_index(
    conn: &Connection,
    index: &str,
    expr: &str,
    limit: usize,
) -> Result<Vec<(i64, Archived)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT m.rowid, m.sender, m.body, m.at
         FROM {index} f JOIN messages m ON m.rowid = f.rowid
         WHERE {index} MATCH ?1 ORDER BY bm25({index}) LIMIT ?2"
    ))?;
    Ok(stmt
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
        .collect::<Result<Vec<_>, _>>()?)
}

/// Best similarity between any loose term and any word in the body.
fn best_similarity(terms: &[String], body: &str) -> f64 {
    body.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .flat_map(|word| {
            terms
                .iter()
                .map(move |term| jaro_winkler::similarity(term.chars(), word.chars()))
        })
        .fold(0.0_f64, f64::max)
}

fn to_archived(r: &rusqlite::Row<'_>) -> rusqlite::Result<Archived> {
    Ok(Archived {
        sender: r.get(0)?,
        body: r.get(1)?,
        at: r.get(2)?,
    })
}
