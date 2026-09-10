//! merlin — a Matrix assistant.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use merlin::agent::Agent;
use merlin::config::{Config, Secrets};
use merlin::cron::CronStore;
use merlin::embed::Embedder;
use merlin::exec::Sandbox;
use merlin::llm::Llm;
use merlin::matrix::Bot;
use merlin::memory::Memory;
use merlin::messages::Archive;
use merlin::room::Buffers;
use merlin::tools::Tools;
use merlin::workspace::Workspace;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "merlin=info,warn".into()),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let mut config_path = PathBuf::from("/var/lib/merlin/config.toml");
    let mut import_from: Option<PathBuf> = None;
    let mut backfill_pages: Option<usize> = None;
    let mut import_keys: Option<PathBuf> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => config_path = args.next().context("--config needs a path")?.into(),
            "--import-memories" => {
                import_from = Some(
                    args.next()
                        .context("--import-memories needs a path")?
                        .into(),
                )
            }
            "--import-keys" => {
                import_keys = Some(args.next().context("--import-keys needs a path")?.into())
            }
            "--backfill" => {
                let pages = args.next().unwrap_or_else(|| "50".into());
                backfill_pages = Some(pages.parse().context("--backfill needs a page count")?);
            }
            other => anyhow::bail!("unknown argument '{other}'"),
        }
    }

    let config = Arc::new(Config::load(&config_path)?);
    let memory_path = config.state_dir.join("memory.db");

    // Import is a one-shot maintenance mode, not part of startup: it runs against the same schema the bot uses and then exits, so the result can be verified before anything goes live.
    if let Some(legacy) = import_from {
        let mut memory = Memory::open(&memory_path)?;
        let before = memory.count()?;
        let taken = memory.import_legacy(&legacy)?;
        let after = memory.count()?;
        println!("imported {taken} rows ({before} -> {after} total)");
        return Ok(());
    }

    let secrets = Secrets::from_env()?;

    let soul = std::fs::read_to_string(config.state_dir.join("SOUL.md")).unwrap_or_else(|_| {
        tracing::warn!("no SOUL.md found; running without a persona");
        String::new()
    });

    let memory = Arc::new(Mutex::new(Memory::open(&memory_path)?));
    tracing::info!(count = memory.lock().unwrap().count()?, "memory ready");

    let archive = Arc::new(Mutex::new(Archive::open(
        &config.state_dir.join("messages.db"),
    )?));
    tracing::info!(count = archive.lock().unwrap().count()?, "archive ready");

    let cron_store = Arc::new(Mutex::new(CronStore::open(
        &config.state_dir.join("cron.db"),
    )?));

    let llm = Arc::new(Llm::new(
        secrets.openrouter_api_key.clone(),
        config.model.chat.clone(),
        config.model.image.clone(),
        config.model.reasoning_effort.clone(),
        config.limits.request_timeout_s,
    )?);

    let embedder = Arc::new(Embedder::new(
        secrets.openrouter_api_key.clone(),
        config.model.embedding.clone(),
        config.model.embedding_dimensions,
        config.limits.request_timeout_s,
    )?);

    {
        let memory = memory.lock().unwrap();
        memory.enable_semantic(&config.model.embedding, embedder.dimensions())?;
        let archive = archive.lock().unwrap();
        archive.enable_semantic(&config.model.embedding, embedder.dimensions())?;
        tracing::info!(
            model = %config.model.embedding,
            dimensions = embedder.dimensions(),
            memories = memory.pending_count()?,
            messages = archive.pending_count()?,
            "semantic index ready"
        );
    }

    let sandbox = Arc::new(Sandbox::new(
        exec_runner(),
        config.limits.exec_timeout_s,
        config.limits.exec_memory_max.clone(),
    ));

    // Deliberately outside the 0700 state directory: the sandbox uid shares this
    // directory, and must not be given a foothold beside the databases.
    let workspace = Arc::new(Workspace::new(workspace_dir())?);
    tracing::info!(path = %workspace.root().display(), "workspace ready");

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(
            config.limits.request_timeout_s,
        ))
        .build()?;

    let tools = Arc::new(Tools {
        memory: Arc::clone(&memory),
        archive: Arc::clone(&archive),
        cron: Arc::clone(&cron_store),
        sandbox,
        workspace,
        llm: Arc::clone(&llm),
        http,
        exa_key: secrets.exa_api_key.clone(),
        embedder: Arc::clone(&embedder),
        config: Arc::clone(&config),
    });

    let agent = Arc::new(Agent {
        llm,
        tools,
        soul,
        max_iterations: config.limits.tool_iterations,
        timezone: config.tz(),
    });

    let link = merlin::matrix::connect(&config, &secrets).await?;
    tracing::info!(user = %config.user_id, "connected");

    if let Some(path) = import_keys {
        let passphrase = std::env::var("MATRIX_KEY_EXPORT_PASSPHRASE")
            .context("MATRIX_KEY_EXPORT_PASSPHRASE must hold the passphrase used for the export")?;
        let result = link
            .client()
            .encryption()
            .import_room_keys(path, &passphrase)
            .await
            .map_err(|e| anyhow::anyhow!("importing room keys failed: {e}"))?;
        println!(
            "imported {} of {} room keys",
            result.imported_count, result.total_count
        );
        return Ok(());
    }

    if let Some(pages) = backfill_pages {
        // Sync once so the client has joined rooms and whatever keys the server will hand over before we start reading history.
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        let stats = merlin::backfill::run(&link, &config, &archive, pages).await?;
        println!("backfill: {stats}");
        if stats.undecryptable > stats.archived {
            println!(
                "most events could not be decrypted: this device has no room keys for them. \
                 Set up Secure Backup on the account and provide MATRIX_RECOVERY_PASSPHRASE, \
                 or share keys to this device from a session that has them."
            );
        }
        return Ok(());
    }

    tokio::spawn(merlin::embed::run(
        embedder,
        Arc::clone(&memory),
        Arc::clone(&archive),
        config.limits.embed_batch,
    ));

    // Refill the ambient buffer from the archive, so a restart does not leave the bot blind to what was just said.
    let buffers = Arc::new(Buffers::new(config.context_window));
    {
        let archive = archive.lock().unwrap();
        for room_id in &config.allowed_rooms {
            match archive.recent(room_id, config.context_window) {
                Ok(rows) => {
                    let seeded = rows.len();
                    for row in rows {
                        buffers.push(
                            room_id,
                            merlin::room::Turn {
                                sender: row.sender,
                                body: row.body,
                            },
                        );
                    }
                    tracing::info!(room = %room_id, seeded, "context restored");
                }
                Err(e) => tracing::warn!(room = %room_id, error = %e, "could not restore context"),
            }
        }
    }

    let bot = Arc::new(Bot {
        link,
        agent: Arc::clone(&agent),
        buffers,
        archive: Arc::clone(&archive),
        config: Arc::clone(&config),
    });

    // Started before sync so a job due at boot is not missed.
    merlin::scheduler::start(Arc::clone(&cron_store), agent, Arc::clone(&bot)).await?;

    bot.run().await
}

/// Where the agent's files live.
/// Shared with the sandbox uid, so it sits beside the state directory rather than inside it.
fn workspace_dir() -> PathBuf {
    match std::env::var("MERLIN_WORKSPACE") {
        Ok(v) if !v.trim().is_empty() => PathBuf::from(v),
        _ => PathBuf::from("/var/lib/merlin-workspace"),
    }
}

/// How `run_code` reaches the sandbox.
/// Overridable so the bot can run outside NixOS, where the production wrapper does not exist.
fn exec_runner() -> Vec<String> {
    match std::env::var("MERLIN_EXEC_RUNNER") {
        Ok(v) if !v.trim().is_empty() => v.split_whitespace().map(str::to_string).collect(),
        _ => vec![
            "sudo".into(),
            "-n".into(),
            "-u".into(),
            "merlin-exec".into(),
            "/run/current-system/sw/bin/merlin-sandbox-configured".into(),
        ],
    }
}
