//! Dreaming: a nightly pass where the agent consolidates its own memory.
//!
//! Memory accumulates by appending. `memory_store` upserts on `key`, but only when the agent reuses a key,
//! and in practice almost every key is distinct, so near-duplicates pile up and nothing ever revises them.
//!
//! This runs as an ordinary turn with the ordinary tools, so consolidation is the agent's own judgement rather than a similarity threshold chosen here.
//! It posts nothing: a room does not want a report at five in the morning, and the result is visible in the memory itself.

use anyhow::Result;
use std::sync::Arc;

use crate::agent::{Agent, Incoming};

pub const JOB_NAME: &str = "dreaming";

const PROMPT: &str = "\
You are consolidating your own memory. Nobody is watching and nothing you say \
here is sent to anyone, so spend the effort on the work rather than on a report.

Use sql_query to look at the memories table: how many there are, which are \
near-duplicates of each other, which are stale, and which are worded so vaguely \
they will never be recalled. memory_recall finds entries by meaning, which is \
how you spot two records of the same fact written differently.

Then improve it:
- where several memories say the same thing, write one good memory under a \
short descriptive key and memory_forget the others
- where a memory records something that has since changed, revise it
- where a memory is a fragment of a conversation with no lasting value, forget it
- give consolidated entries short, stable, descriptive keys, so that storing the \
same subject again revises the entry instead of adding another

Be conservative. Forget something only when its content is genuinely captured \
elsewhere or genuinely worthless.";

/// Run one consolidation pass.
pub async fn run(agent: &Agent, room_id: &str) -> Result<()> {
    let started = std::time::Instant::now();

    let result = agent
        .turn(
            Incoming {
                room_id,
                sender: "dreaming",
                body: PROMPT,
                ambient: None,
                reply_parent: None,
                attachments: Vec::new(),
            },
            // No progress sink: an intermediate message would post to the room, which is the one thing this must not do.
            None,
        )
        .await?;

    tracing::info!(
        ms = started.elapsed().as_millis() as u64,
        tools = result.tools_used.len(),
        prompt_tokens = result.prompt_tokens,
        completion_tokens = result.completion_tokens,
        "dreaming finished"
    );
    Ok(())
}

/// Schedule the nightly pass.
/// Registered here rather than written into the cron table, so it cannot be deleted by accident and needs no migration to appear.
pub async fn schedule(
    scheduler: &tokio_cron_scheduler::JobScheduler,
    agent: Arc<Agent>,
    room_id: String,
    schedule: &str,
    tz: chrono_tz::Tz,
) -> Result<()> {
    let expr = if schedule.split_whitespace().count() == 5 {
        format!("0 {schedule}")
    } else {
        schedule.to_string()
    };

    let job = tokio_cron_scheduler::Job::new_async_tz(expr.as_str(), tz, move |_uuid, _lock| {
        let agent = Arc::clone(&agent);
        let room_id = room_id.clone();
        Box::pin(async move {
            if let Err(e) = run(&agent, &room_id).await {
                tracing::warn!(error = %e, "dreaming failed");
            }
        })
    })
    .map_err(|e| anyhow::anyhow!("invalid dreaming schedule '{schedule}': {e}"))?;

    scheduler
        .add(job)
        .await
        .map_err(|e| anyhow::anyhow!("scheduling dreaming: {e}"))?;
    Ok(())
}
