//! One database, opened once.
//!
//! Memories, messages and scheduled jobs were three files with three connections, three schemas and three locks, which meant three of everything to keep in step
//! and no way to ask a question that spanned them.
//! They are one file now, so a join is possible and the agent can be handed the schema and left to query it.

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::Path;

use crate::embed;

/// Every table, created on open.
/// Written out in full rather than migrated in steps: the shape is small enough to state plainly, and `IF NOT EXISTS` makes applying it to an existing file a no-op.
pub const SCHEMA: &str = r#"
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
  INSERT INTO memories_fts(rowid, key, content) VALUES (new.rowid, new.key, new.content);
END;
CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
  INSERT INTO memories_fts(memories_fts, rowid, key, content)
  VALUES ('delete', old.rowid, old.key, old.content);
END;
CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE ON memories BEGIN
  INSERT INTO memories_fts(memories_fts, rowid, key, content)
  VALUES ('delete', old.rowid, old.key, old.content);
  INSERT INTO memories_fts(rowid, key, content) VALUES (new.rowid, new.key, new.content);
END;

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

-- Trigram matches substrings, which the default tokenizer cannot, and supplies
-- the candidate set that approximate ranking scores.
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

CREATE TABLE IF NOT EXISTS cron_jobs (
  name        TEXT PRIMARY KEY,
  schedule    TEXT NOT NULL,
  timezone    TEXT NOT NULL,
  prompt      TEXT NOT NULL,
  room_id     TEXT NOT NULL,
  enabled     INTEGER NOT NULL DEFAULT 1,
  created_at  TEXT NOT NULL,
  last_run    TEXT,
  last_status TEXT
);
"#;

/// What the agent is told it can query.
/// Kept beside the schema so the two cannot drift.
pub const SCHEMA_SUMMARY: &str = "\
memories(id, key, content, category, room_id, created_at, updated_at)
messages(event_id, room_id, sender, body, at)
cron_jobs(name, schedule, timezone, prompt, room_id, enabled, created_at, last_run, last_status)
memories_fts(key, content), messages_fts(body), messages_trigram(body) — FTS5, use MATCH
memory_vectors(memory_rowid, embedding), message_vectors(message_rowid, embedding) — sqlite-vec";

pub fn open(path: &Path) -> Result<Connection> {
    embed::register();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let conn = Connection::open(path)
        .with_context(|| format!("opening database at {}", path.display()))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.execute_batch(SCHEMA).context("creating schema")?;
    Ok(conn)
}

/// Fold the three original files into this one, once.
///
/// Each is attached and copied rather than parsed, so the FTS and vector tables rebuild from their triggers and content tables rather than being copied in a half-consistent state.
/// A migrated file is renamed aside rather than deleted, because the cost of keeping it is nothing and the cost of being wrong is everything.
pub fn migrate_from_split_files(conn: &mut Connection, state_dir: &Path) -> Result<usize> {
    let sources = [
        (
            "memory.db",
            "memories",
            "id, key, content, category, room_id, created_at, updated_at",
        ),
        (
            "messages.db",
            "messages",
            "event_id, room_id, sender, body, at",
        ),
        (
            "cron.db",
            "cron_jobs",
            "name, schedule, timezone, prompt, room_id, enabled, created_at, last_run, last_status",
        ),
    ];

    let mut moved = 0usize;
    for (file, table, columns) in sources {
        let old = state_dir.join(file);
        if !old.exists() {
            continue;
        }

        conn.execute_batch(&format!("ATTACH DATABASE '{}' AS old;", old.display()))
            .with_context(|| format!("attaching {}", old.display()))?;

        let copied = conn
            .execute(
                &format!("INSERT OR IGNORE INTO main.{table} ({columns}) SELECT {columns} FROM old.{table}"),
                [],
            )
            .with_context(|| format!("copying {table}"))?;

        conn.execute_batch("DETACH DATABASE old;")?;

        tracing::info!(%file, table, rows = copied, "migrated into the single database");
        moved += copied;
        std::fs::rename(&old, old.with_extension("db.migrated")).ok();
        for suffix in ["-wal", "-shm"] {
            std::fs::remove_file(state_dir.join(format!("{file}{suffix}"))).ok();
        }
    }
    Ok(moved)
}

/// Run a read-only query and render it as a text table.
///
/// Read-only is enforced by SQLite itself rather than by inspecting the text: `readonly()` is the parser's own verdict,
/// where a prefix check would be fooled by a comment, a CTE wrapping a write, or a pragma with side effects.
pub fn query(conn: &Connection, sql: &str, max_rows: usize) -> Result<String> {
    let stmt = conn
        .prepare(sql)
        .with_context(|| format!("preparing query: {sql}"))?;
    if !stmt.readonly() {
        anyhow::bail!(
            "that statement would modify the database; only SELECT, WITH and EXPLAIN are allowed here"
        );
    }
    let mut stmt = stmt;

    let columns: Vec<String> = stmt.column_names().iter().map(|c| c.to_string()).collect();
    let mut rows = stmt.query([])?;
    let mut out: Vec<String> = vec![columns.join(" | ")];
    let mut count = 0usize;

    while let Some(row) = rows.next()? {
        if count >= max_rows {
            out.push(format!("… stopped at {max_rows} rows"));
            break;
        }
        let cells: Vec<String> = (0..columns.len())
            .map(|i| match row.get_ref(i) {
                Ok(rusqlite::types::ValueRef::Null) => "NULL".to_string(),
                Ok(rusqlite::types::ValueRef::Integer(v)) => v.to_string(),
                Ok(rusqlite::types::ValueRef::Real(v)) => v.to_string(),
                Ok(rusqlite::types::ValueRef::Text(v)) => String::from_utf8_lossy(v).to_string(),
                Ok(rusqlite::types::ValueRef::Blob(v)) => format!("<{} bytes>", v.len()),
                Err(_) => "?".to_string(),
            })
            .collect();
        out.push(cells.join(" | "));
        count += 1;
    }

    if count == 0 {
        return Ok("No rows.".into());
    }
    Ok(out.join("\n"))
}
