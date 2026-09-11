use anyhow::Result;
use rapidfuzz::distance::jaro_winkler;
use rusqlite::{Connection, params};
use std::collections::BTreeSet;

use crate::embed::{self, Messages};
use crate::query::{Term, escape, parse, required_expr};

const CANDIDATE_CAP: usize = 500;
const MIN_SIMILARITY: f64 = 0.82;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Archived {
    pub sender: String,
    pub body: String,
    pub at: String,
}

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

fn approximate(
    conn: &Connection,
    exact: &[&Term],
    loose: &[&Term],
    limit: usize,
) -> Result<Vec<Archived>> {
    let over = limit.saturating_mul(8).max(40);
    let candidates = if exact.is_empty() {
        let grams: BTreeSet<String> = loose
            .iter()
            .flat_map(|t| {
                t.text
                    .chars()
                    .collect::<Vec<_>>()
                    .windows(3)
                    .map(|w| w.iter().collect())
                    .collect::<Vec<String>>()
            })
            .collect();
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

    let matchers: Vec<jaro_winkler::BatchComparator<char>> = loose
        .iter()
        .map(|t| jaro_winkler::BatchComparator::new(t.text.chars()))
        .collect();
    let mut scored: Vec<(f64, Archived)> = candidates
        .into_iter()
        .map(|(_, row)| (best_similarity(&matchers, &row.body), row))
        .filter(|(score, _)| *score >= MIN_SIMILARITY)
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    let ranked: Vec<Archived> = scored.into_iter().take(limit).map(|(_, r)| r).collect();

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

fn best_similarity(matchers: &[jaro_winkler::BatchComparator<char>], body: &str) -> f64 {
    let mut best = 0.0_f64;
    for word in body.split(|c: char| !c.is_alphanumeric()) {
        if word.is_empty() {
            continue;
        }
        for matcher in matchers {
            best = best.max(matcher.similarity(word.chars().flat_map(char::to_lowercase)));
            if best == 1.0 {
                return best;
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
