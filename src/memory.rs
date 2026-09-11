use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use crate::embed::{self, Memories};
use crate::query::{Term, loose_text, parse, required_expr};

const CANDIDATE_CAP: usize = 500;

#[derive(Debug, Clone)]
pub struct Record {
    pub key: String,
    pub content: String,
    pub category: String,
    pub created_at: String,
}

pub fn store(
    conn: &Connection,
    key: &str,
    content: &str,
    category: &str,
    room_id: Option<&str>,
) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
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

    if let Some(rowid) = rowid(conn, key)? {
        embed::invalidate::<Memories>(conn, rowid).ok();
    }
    Ok(())
}

pub fn forget(conn: &Connection, key: &str) -> Result<bool> {
    if let Some(rowid) = rowid(conn, key)? {
        embed::invalidate::<Memories>(conn, rowid).ok();
    }
    Ok(conn.execute("DELETE FROM memories WHERE key = ?1", params![key])? > 0)
}

pub fn recall(
    conn: &Connection,
    query: &str,
    vector: Option<&[f32]>,
    limit: usize,
) -> Result<Vec<Record>> {
    let terms = parse(query);
    let exact: Vec<&Term> = terms.iter().filter(|t| t.exact).collect();

    if !exact.is_empty() && loose_text(query).is_empty() {
        let hits = required(conn, &exact, limit)?;
        if !hits.is_empty() {
            return Ok(hits.into_iter().map(|(_, r)| r).collect());
        }
    }

    if let Some(vector) = vector {
        let hits = if exact.is_empty() {
            let ids = embed::nearest::<Memories>(conn, vector, limit)?;
            embed::load_ordered(conn, SELECT_BY_ROWID, &ids, to_record)?
        } else {
            embed::rank::<Memories, _>(conn, required(conn, &exact, CANDIDATE_CAP)?, vector, limit)?
        };
        if !hits.is_empty() {
            return Ok(hits);
        }
    }

    keyword(conn, query, limit)
}

const SELECT_BY_ROWID: &str =
    "SELECT key, content, category, created_at FROM memories WHERE rowid = ?1";

fn required(conn: &Connection, exact: &[&Term], limit: usize) -> Result<Vec<(i64, Record)>> {
    let mut stmt = conn.prepare(
        "SELECT m.rowid, m.key, m.content, m.category, m.created_at
         FROM memories_fts f JOIN memories m ON m.rowid = f.rowid
         WHERE memories_fts MATCH ?1 ORDER BY bm25(memories_fts) LIMIT ?2",
    )?;
    Ok(stmt
        .query_map(params![required_expr(exact), limit as i64], |r| {
            Ok((
                r.get(0)?,
                Record {
                    key: r.get(1)?,
                    content: r.get(2)?,
                    category: r.get(3)?,
                    created_at: r.get(4)?,
                },
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?)
}

fn keyword(conn: &Connection, query: &str, limit: usize) -> Result<Vec<Record>> {
    let cleaned = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() > 1)
        .map(|t| format!("\"{}\"", t.to_lowercase()))
        .collect::<Vec<_>>()
        .join(" OR ");

    if !cleaned.is_empty() {
        let mut stmt = conn.prepare(
            "SELECT m.key, m.content, m.category, m.created_at
             FROM memories_fts f JOIN memories m ON m.rowid = f.rowid
             WHERE memories_fts MATCH ?1 ORDER BY bm25(memories_fts) LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![cleaned, limit as i64], to_record)?
            .collect::<Result<Vec<_>, _>>()?;
        if !rows.is_empty() {
            return Ok(rows);
        }
    }

    let mut stmt = conn.prepare(
        "SELECT key, content, category, created_at FROM memories
         WHERE content LIKE ?1 OR key LIKE ?1 ORDER BY updated_at DESC LIMIT ?2",
    )?;
    Ok(stmt
        .query_map(params![format!("%{query}%"), limit as i64], to_record)?
        .collect::<Result<Vec<_>, _>>()?)
}

pub fn count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT count(*) FROM memories", [], |r| r.get(0))?)
}

fn rowid(conn: &Connection, key: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT rowid FROM memories WHERE key = ?1",
            params![key],
            |r| r.get(0),
        )
        .optional()?)
}

fn to_record(r: &rusqlite::Row<'_>) -> rusqlite::Result<Record> {
    Ok(Record {
        key: r.get(0)?,
        content: r.get(1)?,
        category: r.get(2)?,
        created_at: r.get(3)?,
    })
}
