//! Scheduled jobs,
//! created by the agent at runtime rather than written into config.
//!
//! A firing job has one output path: the scheduler runs the prompt and posts the result.

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::path::Path;
use std::str::FromStr;

pub struct CronStore {
    conn: Connection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub name: String,
    pub schedule: String,
    pub timezone: String,
    pub prompt: String,
    pub room_id: String,
    pub enabled: bool,
}

const SCHEMA: &str = r#"
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

impl CronStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening cron db at {}", path.display()))?;
        conn.execute_batch(SCHEMA).context("creating cron schema")?;
        Ok(Self { conn })
    }

    pub fn upsert(&self, job: &Job) -> Result<()> {
        self.conn.execute(
            "INSERT INTO cron_jobs (name, schedule, timezone, prompt, room_id, enabled, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(name) DO UPDATE SET
               schedule = excluded.schedule,
               timezone = excluded.timezone,
               prompt   = excluded.prompt,
               room_id  = excluded.room_id,
               enabled  = excluded.enabled",
            params![
                job.name,
                job.schedule,
                job.timezone,
                job.prompt,
                job.room_id,
                job.enabled as i32,
                chrono::Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<Job>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, schedule, timezone, prompt, room_id, enabled
             FROM cron_jobs ORDER BY name",
        )?;
        let jobs = stmt
            .query_map([], |r| {
                Ok(Job {
                    name: r.get(0)?,
                    schedule: r.get(1)?,
                    timezone: r.get(2)?,
                    prompt: r.get(3)?,
                    room_id: r.get(4)?,
                    enabled: r.get::<_, i32>(5)? != 0,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(jobs)
    }

    pub fn delete(&self, name: &str) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM cron_jobs WHERE name = ?1", params![name])?;
        Ok(n > 0)
    }

    pub fn record_run(&self, name: &str, status: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE cron_jobs SET last_run = ?2, last_status = ?3 WHERE name = ?1",
            params![name, chrono::Utc::now().to_rfc3339(), status],
        )?;
        Ok(())
    }
}

impl Job {
    /// Reject a bad expression at creation time,
    /// where the agent can correct it,
    /// rather than at the next restart.
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            anyhow::bail!("job name cannot be empty");
        }
        chrono_tz::Tz::from_str(&self.timezone)
            .map_err(|_| anyhow::anyhow!("unknown timezone '{}'", self.timezone))?;

        // tokio-cron-scheduler wants 6 fields (seconds first); the agent writes ordinary 5-field crontab syntax,
        // so normalising here keeps the tool surface familiar.
        let expr = self.six_field_schedule();
        tokio_cron_scheduler::Job::new_async_tz(expr.as_str(), chrono_tz::UTC, |_uuid, _lock| {
            Box::pin(async {})
        })
        .map_err(|e| anyhow::anyhow!("invalid cron expression '{}': {e}", self.schedule))?;
        Ok(())
    }

    pub fn six_field_schedule(&self) -> String {
        let fields: Vec<&str> = self.schedule.split_whitespace().collect();
        if fields.len() == 5 {
            format!("0 {}", self.schedule.trim())
        } else {
            self.schedule.trim().to_string()
        }
    }

    pub fn tz(&self) -> chrono_tz::Tz {
        chrono_tz::Tz::from_str(&self.timezone).unwrap_or(chrono_tz::UTC)
    }
}
