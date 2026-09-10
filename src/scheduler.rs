//! Scheduled jobs.
//!
//! A firing job has exactly one output path: run the prompt as a turn, post the
//! result to its room. Nothing else delivers it, which is what stops the same
//! digest arriving twice.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio_cron_scheduler::{Job as CronJob, JobScheduler};

use crate::agent::{Agent, Incoming};
use crate::cron::{CronStore, Job};
use crate::matrix::Bot;

/// Start the scheduler and keep it in step with the store.
///
/// Jobs the agent creates through `cron_create` land in SQLite, not in this
/// process, so a reconcile loop picks them up. Without it a new job would only
/// fire after a restart, which is not what "schedule this" should mean.
pub async fn start(
    store: Arc<Mutex<CronStore>>,
    agent: Arc<Agent>,
    bot: Arc<Bot>,
) -> Result<()> {
    let scheduler = JobScheduler::new()
        .await
        .context("creating job scheduler")?;
    scheduler.start().await.context("starting scheduler")?;

    tokio::spawn(async move {
        // name -> (uuid, fingerprint). The fingerprint catches an edited job,
        // which must be removed and re-added rather than left on its old cron.
        let mut live: HashMap<String, (uuid::Uuid, String)> = HashMap::new();

        loop {
            let desired = {
                match store.lock() {
                    Ok(s) => s.list().unwrap_or_default(),
                    Err(_) => Vec::new(),
                }
            };

            let wanted: HashMap<String, Job> = desired
                .into_iter()
                .filter(|j| j.enabled)
                .map(|j| (j.name.clone(), j))
                .collect();

            // Drop jobs that were deleted or changed.
            let stale: Vec<String> = live
                .iter()
                .filter(|(name, (_, fp))| {
                    wanted.get(*name).map(fingerprint).as_deref() != Some(fp.as_str())
                })
                .map(|(name, _)| name.clone())
                .collect();

            for name in stale {
                if let Some((uuid, _)) = live.remove(&name) {
                    if scheduler.remove(&uuid).await.is_ok() {
                        tracing::info!(%name, "unscheduled");
                    }
                }
            }

            for (name, job) in &wanted {
                if live.contains_key(name) {
                    continue;
                }
                match register(
                    &scheduler,
                    job,
                    Arc::clone(&store),
                    Arc::clone(&agent),
                    Arc::clone(&bot),
                )
                .await
                {
                    Ok(uuid) => {
                        tracing::info!(%name, schedule = %job.schedule, "scheduled");
                        live.insert(name.clone(), (uuid, fingerprint(job)));
                    }
                    // One bad expression must not stop the others from loading.
                    Err(e) => {
                        tracing::warn!(%name, error = %e, "skipping unschedulable job");
                        // Remember it as broken so the warning is not repeated
                        // every minute.
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
    format!("{}|{}|{}|{}", job.schedule, job.timezone, job.prompt, job.room_id)
}

async fn register(
    scheduler: &JobScheduler,
    job: &Job,
    store: Arc<Mutex<CronStore>>,
    agent: Arc<Agent>,
    bot: Arc<Bot>,
) -> Result<uuid::Uuid> {
    let expr = job.six_field_schedule();
    let tz = job.tz();
    let owned = job.clone();

    let cron_job = CronJob::new_async_tz(expr.as_str(), tz, move |_uuid, _lock| {
        let job = owned.clone();
        let store = Arc::clone(&store);
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
            if let Ok(s) = store.lock() {
                let _ = s.record_run(&job.name, &status);
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

async fn run_once(job: &Job, agent: &Agent, bot: &Bot) -> Result<()> {
    let result = agent
        .turn(Incoming {
            room_id: &job.room_id,
            // Marked as the scheduler rather than a person, so the model does
            // not address the reply to whoever last spoke.
            sender: "scheduler",
            body: &job.prompt,
            ambient: None,
            reply_parent: None,
        })
        .await?;

    if !result.text.is_empty() {
        bot.post(&job.room_id, &result.text).await?;
    }
    for image in result.images {
        bot.post_image(&job.room_id, image).await?;
    }
    Ok(())
}
