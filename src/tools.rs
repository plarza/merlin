use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::net::IpAddr;
use std::sync::Arc;

use crate::agent::Progress;
use crate::config::Config;
use crate::cron::Job;
use crate::embed::Embedder;
use crate::exec::Sandbox;
use crate::image::ImageGen;
use crate::workspace::{Edit, Workspace};
use crate::{cron, db, memory, messages};

pub struct Tools {
    pub dbs: Arc<db::RoomDbs>,
    pub sandbox: Arc<Sandbox>,
    pub workspace: Arc<Workspace>,
    pub images: Arc<ImageGen>,
    pub http: reqwest::Client,
    pub exa_key: Option<String>,
    pub embedder: Arc<Embedder>,
    pub config: Arc<Config>,
}

pub struct Ctx<'a> {
    pub room_id: &'a str,
    pub progress: Option<&'a Progress>,
}

pub enum Outcome {
    Text(String),
    Image {
        bytes: Vec<u8>,
        media_type: String,
        caption: String,
    },
}

impl Outcome {
    pub fn for_model(&self) -> String {
        match self {
            Outcome::Text(t) => t.clone(),
            Outcome::Image { caption, .. } => {
                format!("Image generated and sent to the room: {caption}")
            }
        }
    }
}

pub fn definitions() -> Vec<Value> {
    vec![
        f(
            "memory_recall",
            "Search your durable memory. Use this BEFORE answering about any person, character, project, file or past decision, and before saying you have no record of something. Unquoted words are matched by meaning, so you can describe what you are after rather than guess the wording. Put a word in double quotes to require it exactly.",
            json!({
                "query": { "type": "string", "description": "Describe the subject; \"quoted\" words are required exactly" },
                "limit": { "type": "integer", "description": "Max results, default 8" }
            }),
            &["query"],
        ),
        f(
            "memory_store",
            "Save something durably. Re-using an existing key revises that entry instead of creating a duplicate.",
            json!({
                "key": { "type": "string", "description": "Short stable identifier, reused to revise an entry, e.g. 'kettle' or 'owner-timezone'" },
                "content": { "type": "string" },
                "category": { "type": "string", "enum": ["core", "daily", "conversation"] }
            }),
            &["key", "content"],
        ),
        f(
            "memory_forget",
            "Delete a memory by key. Use this when something you stored turns out to be wrong, rather than storing a correction alongside it.",
            json!({ "key": { "type": "string" } }),
            &["key"],
        ),
        f(
            "search_messages",
            "Search the full history of messages in this chat, for what someone actually said. memory_recall searches notes you chose to keep; this searches everything. Unquoted words are matched by meaning, so a message is found even when it used none of your words. Put a word in double quotes to require it exactly. The two combine, so 'fifa \"2025\" world cup' finds messages that definitely mention 2025, ranked by how much they are about the world cup.",
            json!({
                "query": { "type": "string", "description": "Describe the subject; \"quoted\" words are required exactly" },
                "limit": { "type": "integer", "description": "Default 8" }
            }),
            &["query"],
        ),
        f(
            "send_message",
            "Send a message to the room right now, without ending your turn. Use this on a long task to say what you have found or what you are about to do, rather than working in silence. Your final answer is sent automatically, so do not repeat it here.",
            json!({ "text": { "type": "string" } }),
            &["text"],
        ),
        f(
            "web_search",
            "Search the web for current information. Not a substitute for memory_recall: private things discussed in this chat will never appear here.",
            json!({
                "query": { "type": "string" },
                "num_results": { "type": "integer", "description": "Default 5, max 10" }
            }),
            &["query"],
        ),
        f(
            "web_fetch",
            "Fetch a single web page and return it as plain text. Use web_search first if you do not already have the URL.",
            json!({ "url": { "type": "string" } }),
            &["url"],
        ),
        f(
            "generate_image",
            "Generate an image from a text prompt and post it to the room.",
            json!({
                "prompt": { "type": "string" },
                "model": { "type": "string", "description": "Optional override" }
            }),
            &["prompt"],
        ),
        f(
            "write_file",
            "Write a file in the workspace, creating parent directories and replacing any existing content. Use edit_file to change part of a file you already have.",
            json!({
                "path": { "type": "string" },
                "content": { "type": "string" }
            }),
            &["path", "content"],
        ),
        f(
            "edit_file",
            "Replace exact text in a file. Every edit is matched against the original file rather than against earlier edits, so pass several disjoint edits in one call instead of calling repeatedly. Each old_text must appear exactly once: include surrounding lines to make it unique, but no more than needed. Nothing is written unless every edit matches.",
            json!({
                "path": { "type": "string" },
                "edits": {
                    "type": "array",
                    "description": "Disjoint, non-overlapping replacements",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_text": { "type": "string", "description": "Exact text to replace, unique in the file" },
                            "new_text": { "type": "string" }
                        },
                        "required": ["old_text", "new_text"]
                    }
                }
            }),
            &["path", "edits"],
        ),
        f(
            "bash",
            "Run a bash script in this room's isolated sandbox and return its output. It starts in this room's workspace, so workspace files persist for later turns in this room only. The base system is read-only and includes bash, curl, git, jq, Python, pip, ripgrep, file and tar. The public internet works; the LAN and every other room's data are unreachable.",
            json!({ "script": { "type": "string" } }),
            &["script"],
        ),
        f(
            "sql_query",
            "Run a read-only SQL query against this room's physically separate database. It contains only this room's memories, message archive and scheduled jobs; other rooms cannot be attached or queried. Use this for counting, grouping, joining and questions the search tools do not shape well.\n\nThe engine is SQLite. Schema:\n\nmemories(id, key, content, category, room_id, created_at, updated_at)\nmessages(event_id, room_id, sender, body, at)\ncron_jobs(name, schedule, timezone, prompt, room_id, enabled, created_at, last_run, last_status)\n\nSELECT, WITH and EXPLAIN only; writes and ATTACH are refused. Use memory_store and cron_create to change things.",
            json!({
                "query": { "type": "string", "description": "A single SELECT, WITH or EXPLAIN statement" },
                "limit": { "type": "integer", "description": "Max rows returned, default 50" }
            }),
            &["query"],
        ),
        f(
            "cron_create",
            "Schedule a recurring job. The prompt runs as a normal turn at each firing and the result is posted to this room.",
            json!({
                "name": { "type": "string", "description": "Unique short name" },
                "schedule": { "type": "string", "description": "5-field cron expression, e.g. '0 7 * * *'" },
                "prompt": { "type": "string", "description": "What to do when it fires" },
                "timezone": { "type": "string", "description": "IANA zone, defaults to the configured one" }
            }),
            &["name", "schedule", "prompt"],
        ),
        f(
            "cron_delete",
            "Delete a scheduled job by name, stopping it from firing again.",
            json!({ "name": { "type": "string" } }),
            &["name"],
        ),
    ]
}

fn f(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": { "type": "object", "properties": properties, "required": required }
        }
    })
}

impl Tools {
    pub async fn dispatch(&self, name: &str, args: &Value, ctx: &Ctx<'_>) -> Outcome {
        match self.run(name, args, ctx).await {
            Ok(outcome) => outcome,
            Err(e) => Outcome::Text(format!("Error from {name}: {e:#}")),
        }
    }

    async fn run(&self, name: &str, args: &Value, ctx: &Ctx<'_>) -> Result<Outcome> {
        match name {
            "memory_recall" => self.memory_recall(args, ctx).await,
            "memory_store" => self.memory_store(args, ctx),
            "memory_forget" => self.memory_forget(args, ctx),
            "search_messages" => self.search_messages(args, ctx).await,
            "sql_query" => self.sql_query(args, ctx),
            "send_message" => self.send_message(args, ctx),
            "web_search" => self.web_search(args).await,
            "web_fetch" => self.web_fetch(args).await,
            "generate_image" => self.generate_image(args).await,
            "bash" => self.bash(args, ctx).await,
            "write_file" => self.write_file(args, ctx),
            "edit_file" => self.edit_file(args, ctx),
            "cron_create" => self.cron_create(args, ctx),
            "cron_delete" => self.cron_delete(args, ctx),
            other => anyhow::bail!("unknown tool '{other}'"),
        }
    }

    async fn memory_recall(&self, args: &Value, ctx: &Ctx<'_>) -> Result<Outcome> {
        let query = str_arg(args, "query")?;
        let vector = self.embed_loose(&query).await;
        let db = self.dbs.get(ctx.room_id)?;
        let hits = memory::recall(
            &db.lock().unwrap(),
            &query,
            vector.as_deref(),
            limit(args, 8, 25),
        )?;
        Ok(Outcome::Text(if hits.is_empty() {
            format!("No memories matched '{query}'.")
        } else {
            render_memories(&hits)
        }))
    }

    fn memory_store(&self, args: &Value, ctx: &Ctx<'_>) -> Result<Outcome> {
        let key = str_arg(args, "key")?;
        let category = args
            .get("category")
            .and_then(Value::as_str)
            .unwrap_or("core");
        let db = self.dbs.get(ctx.room_id)?;
        memory::store(
            &db.lock().unwrap(),
            &key,
            &str_arg(args, "content")?,
            category,
            Some(ctx.room_id),
        )?;
        Ok(Outcome::Text(format!("Stored under '{key}'.")))
    }

    fn memory_forget(&self, args: &Value, ctx: &Ctx<'_>) -> Result<Outcome> {
        let key = str_arg(args, "key")?;
        let db = self.dbs.get(ctx.room_id)?;
        Ok(Outcome::Text(
            if memory::forget(&db.lock().unwrap(), &key)? {
                format!("Deleted '{key}'.")
            } else {
                format!("No memory under '{key}'.")
            },
        ))
    }

    async fn search_messages(&self, args: &Value, ctx: &Ctx<'_>) -> Result<Outcome> {
        let query = str_arg(args, "query")?;
        let vector = self.embed_loose(&query).await;
        let db = self.dbs.get(ctx.room_id)?;
        let hits = messages::search(
            &db.lock().unwrap(),
            &query,
            vector.as_deref(),
            limit(args, 8, 30),
        )?;
        Ok(Outcome::Text(if hits.is_empty() {
            format!("No messages matched '{query}'.")
        } else {
            render_messages(&hits)
        }))
    }

    fn sql_query(&self, args: &Value, ctx: &Ctx<'_>) -> Result<Outcome> {
        let query = str_arg(args, "query")?;
        let db = self.dbs.get(ctx.room_id)?;
        Ok(Outcome::Text(db::query(
            &db.lock().unwrap(),
            &query,
            limit(args, 50, 500),
        )?))
    }

    fn send_message(&self, args: &Value, ctx: &Ctx<'_>) -> Result<Outcome> {
        let text = str_arg(args, "text")?;
        Ok(Outcome::Text(match ctx.progress {
            Some(sink) => {
                let _ = sink.send(text);
                "Sent to the room.".into()
            }
            None => "There is no room to send to from here.".to_string(),
        }))
    }

    async fn web_search(&self, args: &Value) -> Result<Outcome> {
        let query = str_arg(args, "query")?;
        let n = args
            .get("num_results")
            .and_then(Value::as_u64)
            .unwrap_or(5)
            .clamp(1, 10);
        let key = self
            .exa_key
            .as_ref()
            .context("web search is unavailable: EXA_API_KEY is not set")?;

        let payload: Value = self
            .http
            .post("https://api.exa.ai/search")
            .header("x-api-key", key)
            .json(&json!({
                "query": query,
                "numResults": n,
                "contents": { "text": { "maxCharacters": 1200 } }
            }))
            .send()
            .await
            .context("calling Exa")?
            .json()
            .await
            .context("decoding Exa response")?;

        let results = payload
            .get("results")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if results.is_empty() {
            return Ok(Outcome::Text(format!("No results for '{query}'.")));
        }

        let body = results
            .iter()
            .map(|r| {
                let field = |k| r.get(k).and_then(Value::as_str).unwrap_or_default();
                format!(
                    "{}\n{}\n{}",
                    r.get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("(untitled)"),
                    field("url"),
                    crate::truncate(field("text"), 900)
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n---\n\n");
        Ok(Outcome::Text(body))
    }

    async fn web_fetch(&self, args: &Value) -> Result<Outcome> {
        let url = public_url(&str_arg(args, "url")?)?;
        let resp = self.http.get(url).send().await.context("request failed")?;
        let status = resp.status();
        let cap = self.config.limits.max_response_bytes;
        let bytes = resp.bytes().await.context("reading response body")?;
        anyhow::ensure!(bytes.len() <= cap, "response over the {cap} byte cap");

        let body = String::from_utf8_lossy(&bytes);
        if !status.is_success() {
            return Ok(Outcome::Text(format!(
                "HTTP {status}\n{}",
                crate::truncate(&body, 2000)
            )));
        }
        let text = html2text::from_read(body.as_bytes(), 100).unwrap_or_else(|_| body.to_string());
        Ok(Outcome::Text(crate::truncate(&text, 12_000)))
    }

    async fn generate_image(&self, args: &Value) -> Result<Outcome> {
        let prompt = str_arg(args, "prompt")?;
        let image = self
            .images
            .generate(&prompt, args.get("model").and_then(Value::as_str))
            .await?;
        Ok(Outcome::Image {
            bytes: image.bytes,
            media_type: image.media_type,
            caption: prompt,
        })
    }

    async fn bash(&self, args: &Value, ctx: &Ctx<'_>) -> Result<Outcome> {
        let out = self
            .sandbox
            .run(&str_arg(args, "script")?, ctx.room_id)
            .await?;

        let mut report = out.stdout;
        if !out.stderr.is_empty() {
            report.push_str(&format!("\n[stderr]\n{}", out.stderr));
        }
        if out.timed_out {
            report.push_str("\n[timed out]");
        } else if out.exit_code.unwrap_or(0) != 0 {
            report.push_str(&format!("\n[exit {}]", out.exit_code.unwrap_or(-1)));
        }
        if report.trim().is_empty() {
            report.push_str("(no output)");
        }
        Ok(Outcome::Text(report))
    }

    fn write_file(&self, args: &Value, ctx: &Ctx<'_>) -> Result<Outcome> {
        Ok(Outcome::Text(self.workspace.scoped(ctx.room_id)?.write(
            &str_arg(args, "path")?,
            &str_arg(args, "content")?,
        )?))
    }

    fn edit_file(&self, args: &Value, ctx: &Ctx<'_>) -> Result<Outcome> {
        let edits = args
            .get("edits")
            .and_then(Value::as_array)
            .context("missing required argument 'edits'")?
            .iter()
            .map(|e| {
                Ok(Edit {
                    old: str_arg(e, "old_text")?,
                    new: str_arg(e, "new_text")?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Outcome::Text(
            self.workspace
                .scoped(ctx.room_id)?
                .edit(&str_arg(args, "path")?, &edits)?,
        ))
    }

    fn cron_create(&self, args: &Value, ctx: &Ctx<'_>) -> Result<Outcome> {
        let job = Job {
            name: str_arg(args, "name")?,
            schedule: str_arg(args, "schedule")?,
            timezone: args
                .get("timezone")
                .and_then(Value::as_str)
                .unwrap_or(&self.config.timezone)
                .to_string(),
            prompt: str_arg(args, "prompt")?,
            room_id: ctx.room_id.to_string(),
            enabled: true,
        };
        job.validate()?;
        let db = self.dbs.get(ctx.room_id)?;
        cron::upsert(&db.lock().unwrap(), &job)?;
        Ok(Outcome::Text(format!(
            "Scheduled '{}' at '{}'. The reconcile loop picks it up within a minute.",
            job.name, job.schedule
        )))
    }

    fn cron_delete(&self, args: &Value, ctx: &Ctx<'_>) -> Result<Outcome> {
        let name = str_arg(args, "name")?;
        let db = self.dbs.get(ctx.room_id)?;
        Ok(Outcome::Text(
            if cron::delete(&db.lock().unwrap(), &name)? {
                format!("Deleted job '{name}'.")
            } else {
                format!("No job named '{name}'.")
            },
        ))
    }

    async fn embed_loose(&self, query: &str) -> Option<Vec<f32>> {
        let text = crate::query::loose_text(query);
        if text.is_empty() {
            return None;
        }
        match self.embedder.embed(&[text]).await {
            Ok(mut vectors) => vectors.pop(),
            Err(e) => {
                tracing::warn!(error = %e, "embedding the query failed; falling back to keyword search");
                None
            }
        }
    }
}

pub fn public_url(url: &str) -> Result<reqwest::Url> {
    let parsed = reqwest::Url::parse(url).context("invalid URL")?;
    anyhow::ensure!(
        matches!(parsed.scheme(), "http" | "https"),
        "only http and https are permitted"
    );
    match parsed
        .socket_addrs(|| None)
        .context("resolving the host")?
        .iter()
        .find(|a| is_internal(a.ip()))
    {
        Some(a) => anyhow::bail!("that host resolves to {}, which is not public", a.ip()),
        None => Ok(parsed),
    }
}

fn is_internal(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_private() || v4.is_loopback() || v4.is_link_local() || v4.is_unspecified()
        }
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_internal(v4.into()),
            None => {
                v6.is_loopback()
                    || v6.is_unspecified()
                    || matches!(v6.segments()[0] & 0xfe00, 0xfc00 | 0xfe00)
            }
        },
    }
}

fn render_memories(hits: &[memory::Record]) -> String {
    hits.iter()
        .map(|r| {
            format!(
                "[{}] ({}, {}) {}",
                r.key,
                r.category,
                &r.created_at[..10.min(r.created_at.len())],
                r.content
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_messages(hits: &[messages::Archived]) -> String {
    hits.iter()
        .map(|h| format!("[{}] {}: {}", &h.at[..10.min(h.at.len())], h.sender, h.body))
        .collect::<Vec<_>>()
        .join("\n")
}

fn limit(args: &Value, default: u64, max: u64) -> usize {
    args.get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(default)
        .clamp(1, max) as usize
}

fn str_arg(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .with_context(|| format!("missing required argument '{key}'"))
}
