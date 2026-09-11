//! Scheduled jobs, created by the agent at runtime rather than written into config.
//!
//! A firing job has one output path: the scheduler runs the prompt and posts the result.

use anyhow::Result;
use rusqlite::{Connection, params};
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub name: String,
    pub schedule: String,
    pub timezone: String,
    pub prompt: String,
    pub room_id: String,
    pub enabled: bool,
}

pub fn upsert(conn: &Connection, job: &Job) -> Result<()> {
    conn.execute(
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

pub fn list(conn: &Connection) -> Result<Vec<Job>> {
    let mut stmt = conn.prepare(
        "SELECT name, schedule, timezone, prompt, room_id, enabled FROM cron_jobs ORDER BY name",
    )?;
    Ok(stmt
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
        .collect::<Result<Vec<_>, _>>()?)
}

pub fn delete(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn.execute("DELETE FROM cron_jobs WHERE name = ?1", params![name])? > 0)
}

pub fn record_run(conn: &Connection, name: &str, status: &str) -> Result<()> {
    conn.execute(
        "UPDATE cron_jobs SET last_run = ?2, last_status = ?3 WHERE name = ?1",
        params![name, chrono::Utc::now().to_rfc3339(), status],
    )?;
    Ok(())
}

impl Job {
    /// Reject a bad expression at creation time, where the agent can correct it, rather than at the next restart.
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(!self.name.trim().is_empty(), "job name cannot be empty");
        chrono_tz::Tz::from_str(&self.timezone)
            .map_err(|_| anyhow::anyhow!("unknown timezone '{}'", self.timezone))?;

        // tokio-cron-scheduler wants 6 fields (seconds first); the agent writes ordinary 5-field crontab syntax, so normalising here keeps the tool surface familiar.
        tokio_cron_scheduler::Job::new_async_tz(
            self.six_field_schedule().as_str(),
            chrono_tz::UTC,
            |_uuid, _lock| Box::pin(async {}),
        )
        .map_err(|e| anyhow::anyhow!("invalid cron expression '{}': {e}", self.schedule))?;
        Ok(())
    }

    pub fn six_field_schedule(&self) -> String {
        if self.schedule.split_whitespace().count() == 5 {
            format!("0 {}", self.schedule.trim())
        } else {
            self.schedule.trim().to_string()
        }
    }

    pub fn tz(&self) -> chrono_tz::Tz {
        chrono_tz::Tz::from_str(&self.timezone).unwrap_or(chrono_tz::UTC)
    }
}
