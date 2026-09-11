use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio_cron_scheduler::{Job as CronJob, JobScheduler};

use crate::agent::{Agent, Incoming};
use crate::cron::{self, Job};
use crate::matrix::Bot;

pub async fn start(
    db: Arc<Mutex<rusqlite::Connection>>,
    agent: Arc<Agent>,
    bot: Arc<Bot>,
) -> Result<()> {
    let scheduler = JobScheduler::new()
        .await
        .context("creating job scheduler")?;
    scheduler.start().await.context("starting scheduler")?;

    tokio::spawn(async move {
        let mut live: HashMap<String, (uuid::Uuid, String)> = HashMap::new();

        loop {
            let desired = match db.lock() {
                Ok(conn) => cron::list(&conn).unwrap_or_default(),
                Err(_) => Vec::new(),
            };

            let wanted: HashMap<String, Job> = desired
                .into_iter()
                .filter(|j| j.enabled)
                .map(|j| (j.name.clone(), j))
                .collect();

            let stale: Vec<String> = live
                .iter()
                .filter(|(name, (_, fp))| {
                    wanted.get(*name).map(fingerprint).as_deref() != Some(fp.as_str())
                })
                .map(|(name, _)| name.clone())
                .collect();

            for name in stale {
                if let Some((uuid, _)) = live.remove(&name)
                    && scheduler.remove(&uuid).await.is_ok()
                {
                    tracing::info!(%name, "unscheduled");
                }
            }

            for (name, job) in &wanted {
                if live.contains_key(name) {
                    continue;
                }
                match register(
                    &scheduler,
                    job,
                    Arc::clone(&db),
                    Arc::clone(&agent),
                    Arc::clone(&bot),
                )
                .await
                {
                    Ok(uuid) => {
                        tracing::info!(%name, schedule = %job.schedule, "scheduled");
                        live.insert(name.clone(), (uuid, fingerprint(job)));
                    }
                    Err(e) => {
                        tracing::warn!(%name, error = %e, "skipping unschedulable job");
                        live.insert(name.clone(), (uuid::Uuid::nil(), fingerprint(job)));
                    }
                }
            }

            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    });

    Ok(())
}

fn fingerprint(job: &Job) -> String {
    format!(
        "{}|{}|{}|{}",
        job.schedule, job.timezone, job.prompt, job.room_id
    )
}

async fn register(
    scheduler: &JobScheduler,
    job: &Job,
    db: Arc<Mutex<rusqlite::Connection>>,
    agent: Arc<Agent>,
    bot: Arc<Bot>,
) -> Result<uuid::Uuid> {
    let expr = job.six_field_schedule();
    let tz = job.tz();
    let owned = job.clone();

    let cron_job = CronJob::new_async_tz(expr.as_str(), tz, move |_uuid, _lock| {
        let job = owned.clone();
        let db = Arc::clone(&db);
        let agent = Arc::clone(&agent);
        let bot = Arc::clone(&bot);

        Box::pin(async move {
            let status = match run_once(&job, &agent, &bot).await {
                Ok(()) => "ok".to_string(),
                Err(e) => {
                    tracing::warn!(name = %job.name, error = %e, "scheduled job failed");
                    format!("error: {e}")
                }
            };
            if let Ok(conn) = db.lock() {
                let _ = cron::record_run(&conn, &job.name, &status);
            }
        })
    })
    .map_err(|e| anyhow::anyhow!("invalid cron expression '{}': {e}", job.schedule))?;

    let uuid = scheduler
        .add(cron_job)
        .await
        .context("adding job to scheduler")?;
    Ok(uuid)
}

async fn run_once(job: &Job, agent: &Agent, bot: &Arc<Bot>) -> Result<()> {
    let (progress, mut updates) = tokio::sync::mpsc::unbounded_channel::<String>();
    let pump = {
        let bot = Arc::clone(bot);
        let room_id = job.room_id.clone();
        tokio::spawn(async move {
            while let Some(text) = updates.recv().await {
                if let Err(e) = bot.post(&room_id, &text).await {
                    tracing::warn!(error = %e, "failed sending an intermediate message");
                }
            }
        })
    };

    let result = agent
        .turn(
            Incoming {
                room_id: &job.room_id,
                sender: "scheduler",
                body: &job.prompt,
                ambient: None,
                reply_parent: None,
                attachments: Vec::new(),
            },
            Some(&progress),
        )
        .await?;

    drop(progress);
    let _ = pump.await;

    if !result.text.is_empty() {
        bot.post(&job.room_id, &result.text).await?;
    }
    for image in result.images {
        bot.post_image(&job.room_id, image).await?;
    }
    Ok(())
}
