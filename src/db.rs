use anyhow::{Context, Result};
use rusqlite::{Connection, limits::Limit};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::embed;

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

pub struct RoomDbs {
    rooms: HashMap<String, Arc<Mutex<Connection>>>,
}

impl RoomDbs {
    pub fn open(state_dir: &Path, room_ids: &[String]) -> Result<Self> {
        anyhow::ensure!(!room_ids.is_empty(), "cannot open databases without rooms");
        let rooms_dir = state_dir.join("rooms");
        std::fs::create_dir_all(&rooms_dir)
            .with_context(|| format!("creating {}", rooms_dir.display()))?;

        migrate_legacy(state_dir, &rooms_dir, room_ids)?;

        let mut rooms = HashMap::new();
        for room_id in room_ids {
            let path = rooms_dir.join(format!("{}.db", room_key(room_id)));
            rooms.insert(room_id.clone(), Arc::new(Mutex::new(open(&path)?)));
        }
        Ok(Self { rooms })
    }

    pub fn get(&self, room_id: &str) -> Result<Arc<Mutex<Connection>>> {
        self.rooms
            .get(room_id)
            .cloned()
            .with_context(|| format!("no database for room {room_id}"))
    }

    pub fn all(&self) -> impl Iterator<Item = (&str, Arc<Mutex<Connection>>)> + '_ {
        self.rooms
            .iter()
            .map(|(id, db)| (id.as_str(), Arc::clone(db)))
    }
}

pub fn room_key(room_id: &str) -> String {
    format!("{:x}", Sha256::digest(room_id.as_bytes()))
}

fn migrate_legacy(state_dir: &Path, rooms_dir: &Path, room_ids: &[String]) -> Result<()> {
    let legacy_path = state_dir.join("merlin.db");
    let split_exists = ["memory.db", "messages.db", "cron.db"]
        .iter()
        .any(|name| state_dir.join(name).exists());
    if !legacy_path.exists() && !split_exists {
        return Ok(());
    }

    let mut legacy = open(&legacy_path)?;
    migrate_from_split_files(&mut legacy, state_dir)?;
    drop(legacy);

    for (index, room_id) in room_ids.iter().enumerate() {
        let path = rooms_dir.join(format!("{}.db", room_key(room_id)));
        let conn = open(&path)?;
        conn.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 1)?;
        conn.execute(
            "ATTACH DATABASE ?1 AS legacy",
            [legacy_path.to_string_lossy().as_ref()],
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO memories (id, key, content, category, room_id, created_at, updated_at)
             SELECT id, key, content, category, ?1, created_at, updated_at FROM legacy.memories
             WHERE room_id = ?1 OR (room_id IS NULL AND ?2 = 0)",
            rusqlite::params![room_id, index],
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO messages (event_id, room_id, sender, body, at)
             SELECT event_id, room_id, sender, body, at FROM legacy.messages WHERE room_id = ?1",
            [room_id],
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO cron_jobs
             (name, schedule, timezone, prompt, room_id, enabled, created_at, last_run, last_status)
             SELECT name, schedule, timezone, prompt, room_id, enabled, created_at, last_run, last_status
             FROM legacy.cron_jobs WHERE room_id = ?1",
            [room_id],
        )?;
        conn.execute_batch("DETACH DATABASE legacy")?;
        conn.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0)?;
    }

    let migrated = unique_migrated_path(&legacy_path);
    std::fs::rename(&legacy_path, &migrated).with_context(|| {
        format!(
            "moving legacy database from {} to {}",
            legacy_path.display(),
            migrated.display()
        )
    })?;
    for suffix in ["-wal", "-shm"] {
        let path = PathBuf::from(format!("{}{}", legacy_path.display(), suffix));
        if path.exists() {
            std::fs::rename(
                &path,
                PathBuf::from(format!("{}{}", migrated.display(), suffix)),
            )?;
        }
    }
    tracing::info!(rooms = room_ids.len(), "split legacy database by room");
    Ok(())
}

fn unique_migrated_path(path: &Path) -> PathBuf {
    let first = path.with_extension("db.migrated");
    if !first.exists() {
        return first;
    }
    for index in 1.. {
        let candidate = path.with_extension(format!("db.migrated.{index}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!()
}

pub fn open(path: &Path) -> Result<Connection> {
    embed::register();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let conn = Connection::open(path)
        .with_context(|| format!("opening database at {}", path.display()))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.execute_batch(SCHEMA).context("creating schema")?;
    conn.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0)?;
    Ok(conn)
}

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
    conn.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 1)?;
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

    conn.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0)?;
    Ok(moved)
}

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
