use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use merlin::agent::Agent;
use merlin::config::{Config, Secrets};
use merlin::embed::{Embedder, Memories, Messages};
use merlin::exec::Sandbox;
use merlin::image::ImageGen;
use merlin::llm::Llm;
use merlin::matrix::Bot;
use merlin::room::{Buffers, Turn};
use merlin::tools::Tools;
use merlin::workspace::Workspace;
use merlin::{backfill, db, embed, matrix, memory, messages, scheduler};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "merlin=info,warn".into()),
        )
        .init();

    let args = Args::parse()?;
    let config = Arc::new(Config::load(&args.config)?);
    let db_path = config.state_dir.join("merlin.db");

    let secrets = Secrets::from_env()?;
    let db = open_database(&db_path, &config.state_dir)?;
    let runtime = Runtime::build(&config, &secrets, db)?;

    let link = matrix::connect(&config, &secrets).await?;
    tracing::info!(user = %config.user_id, "connected");

    if let Some(path) = &args.import_keys {
        return import_keys(&link, path).await;
    }
    if let Some(pages) = args.backfill {
        return backfill_history(&link, &config, &runtime.db, pages).await;
    }

    runtime.run(link, config).await
}

#[derive(Default)]
struct Args {
    config: PathBuf,
    import_keys: Option<PathBuf>,
    backfill: Option<usize>,
}

impl Args {
    fn parse() -> Result<Self> {
        let mut args = Args {
            config: PathBuf::from("/var/lib/merlin/config.toml"),
            ..Default::default()
        };
        let mut raw = std::env::args().skip(1);

        while let Some(arg) = raw.next() {
            let mut value =
                |flag: &str| raw.next().with_context(|| format!("{flag} needs a value"));
            match arg.as_str() {
                "--config" => args.config = value("--config")?.into(),
                "--import-keys" => args.import_keys = Some(value("--import-keys")?.into()),
                "--backfill" => {
                    args.backfill = Some(
                        raw.next()
                            .unwrap_or_else(|| "50".into())
                            .parse()
                            .context("--backfill needs a page count")?,
                    )
                }
                other => anyhow::bail!("unknown argument '{other}'"),
            }
        }
        Ok(args)
    }
}

struct Runtime {
    db: Arc<Mutex<Connection>>,
    agent: Arc<Agent>,
    embedder: Arc<Embedder>,
    workspace: Arc<Workspace>,
}

impl Runtime {
    fn build(config: &Arc<Config>, secrets: &Secrets, db: Arc<Mutex<Connection>>) -> Result<Self> {
        let llm = Arc::new(Llm::new(
            secrets.openrouter_api_key.clone(),
            config.model.chat.clone(),
            config.model.reasoning_effort.clone(),
            config.limits.request_timeout_s,
        )?);

        let images = Arc::new(ImageGen::new(
            config.model.image_provider.parse()?,
            config.model.image.clone(),
            secrets.openrouter_api_key.clone(),
            secrets.fal_api_key.clone(),
            config.limits.request_timeout_s,
        )?);
        tracing::info!(
            provider = ?images.provider(),
            model = images.model(),
            "image generation ready"
        );

        let embedder = Arc::new(Embedder::new(
            secrets.openrouter_api_key.clone(),
            config.model.embedding.clone(),
            config.model.embedding_dimensions,
            config.limits.request_timeout_s,
        )?);

        {
            let conn = db.lock().unwrap();
            embed::ensure_table::<Memories>(&conn, embedder.model(), embedder.dimensions())?;
            embed::ensure_table::<Messages>(&conn, embedder.model(), embedder.dimensions())?;
            tracing::info!(
                model = embedder.model(),
                dimensions = embedder.dimensions(),
                memories = embed::count_pending::<Memories>(&conn)?,
                messages = embed::count_pending::<Messages>(&conn)?,
                "semantic index ready"
            );
        }

        let workspace = Arc::new(Workspace::new(workspace_dir())?);
        tracing::info!(path = %workspace.root().display(), "workspace ready");

        let tools = Arc::new(Tools {
            db: Arc::clone(&db),
            workspace: Arc::clone(&workspace),
            sandbox: Arc::new(Sandbox::new(
                exec_runner(),
                config.limits.exec_timeout_s,
                config.limits.exec_memory_max.clone(),
            )),
            images: Arc::clone(&images),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(config.limits.request_timeout_s))
                .redirect(reqwest::redirect::Policy::custom(|attempt| {
                    if attempt.previous().len() >= 10 {
                        return attempt.stop();
                    }
                    match merlin::tools::public_url(attempt.url().as_str()) {
                        Ok(_) => attempt.follow(),
                        Err(e) => attempt.error(e),
                    }
                }))
                .build()?,
            exa_key: secrets.exa_api_key.clone(),
            embedder: Arc::clone(&embedder),
            config: Arc::clone(config),
        });

        let agent = Arc::new(Agent {
            llm,
            tools,
            soul: soul(&config.state_dir),
            max_iterations: config.limits.tool_iterations,
            max_duration: Duration::from_secs(config.limits.turn_timeout_s),
            timezone: config.tz(),
        });

        Ok(Self {
            db,
            agent,
            embedder,
            workspace,
        })
    }

    async fn run(self, link: mxlink::MatrixLink, config: Arc<Config>) -> Result<()> {
        tokio::spawn(embed::run(
            self.embedder,
            Arc::clone(&self.db),
            config.limits.embed_batch,
        ));

        let bot = Arc::new(Bot {
            link,
            agent: Arc::clone(&self.agent),
            buffers: seed_buffers(&self.db, &config),
            db: Arc::clone(&self.db),
            workspace: self.workspace,
            config: Arc::clone(&config),
        });

        scheduler::start(self.db, self.agent, Arc::clone(&bot)).await?;
        bot.run().await
    }
}

fn open_database(path: &Path, state_dir: &Path) -> Result<Arc<Mutex<Connection>>> {
    let mut conn = db::open(path)?;
    match db::migrate_from_split_files(&mut conn, state_dir)? {
        0 => {}
        rows => tracing::info!(rows, "folded the old split databases into one"),
    }
    tracing::info!(
        memories = memory::count(&conn)?,
        messages = messages::count(&conn)?,
        "database ready"
    );
    Ok(Arc::new(Mutex::new(conn)))
}

fn seed_buffers(db: &Mutex<Connection>, config: &Config) -> Arc<Buffers> {
    let buffers = Arc::new(Buffers::new(config.context_window));
    let conn = db.lock().unwrap();

    for room_id in &config.allowed_rooms {
        match messages::recent(&conn, room_id, config.context_window) {
            Ok(rows) => {
                let seeded = rows.len();
                for row in rows {
                    buffers.push(
                        room_id,
                        Turn {
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
    buffers
}

fn soul(state_dir: &Path) -> String {
    std::fs::read_to_string(state_dir.join("SOUL.md")).unwrap_or_else(|_| {
        tracing::warn!("no SOUL.md found; running without a persona");
        String::new()
    })
}

async fn import_keys(link: &mxlink::MatrixLink, path: &Path) -> Result<()> {
    let passphrase = std::env::var("MATRIX_KEY_EXPORT_PASSPHRASE")
        .context("MATRIX_KEY_EXPORT_PASSPHRASE must hold the passphrase used for the export")?;
    let result = link
        .client()
        .encryption()
        .import_room_keys(path.to_path_buf(), &passphrase)
        .await
        .map_err(|e| anyhow::anyhow!("importing room keys failed: {e}"))?;
    println!(
        "imported {} of {} room keys",
        result.imported_count, result.total_count
    );
    Ok(())
}

async fn backfill_history(
    link: &mxlink::MatrixLink,
    config: &Config,
    db: &Mutex<Connection>,
    pages: usize,
) -> Result<()> {
    tokio::time::sleep(Duration::from_secs(5)).await;
    let stats = backfill::run(link, config, db, pages).await?;
    println!("backfill: {stats}");

    if stats.undecryptable > stats.archived {
        println!(
            "most events could not be decrypted: this device has no room keys for them. \
             Set up Secure Backup on the account and provide MATRIX_RECOVERY_PASSPHRASE, \
             or share keys to this device from a session that has them."
        );
    }
    Ok(())
}

fn workspace_dir() -> PathBuf {
    match std::env::var("MERLIN_WORKSPACE") {
        Ok(v) if !v.trim().is_empty() => PathBuf::from(v),
        _ => PathBuf::from("/var/lib/merlin-workspace"),
    }
}

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
