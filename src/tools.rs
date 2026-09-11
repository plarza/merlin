//! Tool definitions and dispatch.
//!
//! Every tool here exists because it was asked for in conversation.
//! There is no approval gate: the sender allowlist is the boundary.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

use crate::agent::Progress;
use crate::config::Config;
use crate::cron::Job;
use crate::embed::Embedder;
use crate::exec::Sandbox;
use crate::llm::Llm;
use crate::workspace::{Edit, Workspace};
use crate::{cron, db, memory, messages};

pub struct Tools {
    pub db: Arc<Mutex<rusqlite::Connection>>,
    pub sandbox: Arc<Sandbox>,
    pub workspace: Arc<Workspace>,
    pub llm: Arc<Llm>,
    pub http: reqwest::Client,
    pub exa_key: Option<String>,
    pub embedder: Arc<Embedder>,
    pub config: Arc<Config>,
}

/// Everything a tool needs to know about the turn it is running in.
pub struct Ctx<'a> {
    pub room_id: &'a str,
    /// Where an intermediate message goes, when the turn has somewhere to send one.
    pub progress: Option<&'a Progress>,
}

/// What a tool produced.
/// Images travel separately so the Matrix layer can upload bytes rather than stuffing base64 through the model context.
pub enum Outcome {
    Text(String),
    Image {
        bytes: Vec<u8>,
        media_type: String,
        caption: String,
    },
}

impl Outcome {
    /// What the model sees.
    /// For an image that is a short acknowledgement; the bytes go to Matrix directly.
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
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Describe the subject; \"quoted\" words are required exactly" },
                    "limit": { "type": "integer", "description": "Max results, default 8" }
                },
                "required": ["query"]
            }),
        ),
        f(
            "memory_store",
            "Save something durably. Re-using an existing key revises that entry instead of creating a duplicate.",
            json!({
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "Short stable identifier, reused to revise an entry, e.g. 'kettle' or 'owner-timezone'" },
                    "content": { "type": "string" },
                    "category": { "type": "string", "enum": ["core", "daily", "conversation"] }
                },
                "required": ["key", "content"]
            }),
        ),
        f(
            "memory_forget",
            "Delete a memory by key. Use this when something you stored turns out to be wrong, rather than storing a correction alongside it.",
            json!({
                "type": "object",
                "properties": { "key": { "type": "string" } },
                "required": ["key"]
            }),
        ),
        f(
            "search_messages",
            "Search the full history of messages in this chat, for what someone actually said. memory_recall searches notes you chose to keep; this searches everything. Unquoted words are matched by meaning, so a message is found even when it used none of your words. Put a word in double quotes to require it exactly. The two combine, so 'fifa \"2025\" world cup' finds messages that definitely mention 2025, ranked by how much they are about the world cup.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Describe the subject; \"quoted\" words are required exactly" },
                    "limit": { "type": "integer", "description": "Default 8" }
                },
                "required": ["query"]
            }),
        ),
        f(
            "send_message",
            "Send a message to the room right now, without ending your turn. Use this on a long task to say what you have found or what you are about to do, rather than working in silence. Your final answer is sent automatically, so do not repeat it here.",
            json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"]
            }),
        ),
        f(
            "web_search",
            "Search the web for current information. Not a substitute for memory_recall: private things discussed in this chat will never appear here.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "num_results": { "type": "integer", "description": "Default 5, max 10" }
                },
                "required": ["query"]
            }),
        ),
        f(
            "web_fetch",
            "Fetch a single web page and return it as plain text. Use web_search first if you do not already have the URL.",
            json!({
                "type": "object",
                "properties": { "url": { "type": "string" } },
                "required": ["url"]
            }),
        ),
        f(
            "generate_image",
            "Generate an image from a text prompt and post it to the room.",
            json!({
                "type": "object",
                "properties": {
                    "prompt": { "type": "string" },
                    "model": { "type": "string", "description": "Optional override" }
                },
                "required": ["prompt"]
            }),
        ),
        f(
            "write_file",
            "Write a file in the workspace, creating parent directories and replacing any existing content. Use edit_file to change part of a file you already have.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }),
        ),
        f(
            "edit_file",
            "Replace exact text in a file. Every edit is matched against the original file rather than against earlier edits, so pass several disjoint edits in one call instead of calling repeatedly. Each old_text must appear exactly once: include surrounding lines to make it unique, but no more than needed. Nothing is written unless every edit matches.",
            json!({
                "type": "object",
                "properties": {
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
                },
                "required": ["path", "edits"]
            }),
        ),
        f(
            "bash",
            "Run a bash script in your sandbox and return its output. It starts in the workspace, so files you wrote are there and anything it writes persists for later turns. You are root in there and it keeps what you install, so apk add, pip install and npm i all work. Reach python with python3, and the internet with curl. The LAN is unreachable and no secret is visible.",
            json!({
                "type": "object",
                "properties": { "script": { "type": "string" } },
                "required": ["script"]
            }),
        ),
        f(
            "sql_query",
            "Run a read-only SQL query against merlin's database, which holds memories, the full message archive and the scheduled jobs in one file. Use this for counting, grouping, joining and any question the search tools do not shape well, such as who sends the most messages or what was stored in a given week. Schema:\n\nmemories(id, key, content, category, room_id, created_at, updated_at)\nmessages(event_id, room_id, sender, body, at)\ncron_jobs(name, schedule, timezone, prompt, room_id, enabled, created_at, last_run, last_status)\n\nSELECT, WITH and EXPLAIN only; writes are refused. Use memory_store and cron_create to change things.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "A single SELECT, WITH or EXPLAIN statement" },
                    "limit": { "type": "integer", "description": "Max rows returned, default 50" }
                },
                "required": ["query"]
            }),
        ),
        f(
            "cron_create",
            "Schedule a recurring job. The prompt runs as a normal turn at each firing and the result is posted to this room.",
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Unique short name" },
                    "schedule": { "type": "string", "description": "5-field cron expression, e.g. '0 7 * * *'" },
                    "prompt": { "type": "string", "description": "What to do when it fires" },
                    "timezone": { "type": "string", "description": "IANA zone, defaults to the configured one" }
                },
                "required": ["name", "schedule", "prompt"]
            }),
        ),
        f(
            "cron_delete",
            "Delete a scheduled job by name, stopping it from firing again.",
            json!({
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"]
            }),
        ),
    ]
}

fn f(name: &str, description: &str, parameters: Value) -> Value {
    json!({
        "type": "function",
        "function": { "name": name, "description": description, "parameters": parameters }
    })
}

impl Tools {
    pub async fn dispatch(&self, name: &str, args: &Value, ctx: &Ctx<'_>) -> Outcome {
        match self.run(name, args, ctx).await {
            Ok(outcome) => outcome,
            // Tool failures are information for the model, not turn-ending errors: it should be able to try something else or say what broke.
            Err(e) => Outcome::Text(format!("Error from {name}: {e}")),
        }
    }

    /// Dispatch spine.
    /// Each arm names the tool and nothing else; the work lives in a method per tool, so this stays a table of contents.
    async fn run(&self, name: &str, args: &Value, ctx: &Ctx<'_>) -> Result<Outcome> {
        match name {
            "memory_recall" => self.memory_recall(args).await,
            "memory_store" => self.memory_store(args, ctx),
            "memory_forget" => self.memory_forget(args),
            "search_messages" => self.search_messages(args).await,
            "sql_query" => self.sql_query(args),
            "send_message" => self.send_message(args, ctx),
            "web_search" => self.web_search(args).await,
            "web_fetch" => self.web_fetch(args).await,
            "generate_image" => self.generate_image(args).await,
            "bash" => self.bash(args).await,
            "write_file" => self.write_file(args),
            "edit_file" => self.edit_file(args),
            "cron_create" => self.cron_create(args, ctx),
            "cron_delete" => self.cron_delete(args),
            other => anyhow::bail!("unknown tool '{other}'"),
        }
    }

    async fn memory_recall(&self, args: &Value) -> Result<Outcome> {
        let query = str_arg(args, "query")?;
        let vector = self.embed_loose(&query).await;
        let hits = memory::recall(
            &self.db.lock().unwrap(),
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
        memory::store(
            &self.db.lock().unwrap(),
            &key,
            &str_arg(args, "content")?,
            category,
            Some(ctx.room_id),
        )?;
        Ok(Outcome::Text(format!("Stored under '{key}'.")))
    }

    fn memory_forget(&self, args: &Value) -> Result<Outcome> {
        let key = str_arg(args, "key")?;
        Ok(Outcome::Text(
            if memory::forget(&self.db.lock().unwrap(), &key)? {
                format!("Deleted '{key}'.")
            } else {
                format!("No memory under '{key}'.")
            },
        ))
    }

    async fn search_messages(&self, args: &Value) -> Result<Outcome> {
        let query = str_arg(args, "query")?;
        let vector = self.embed_loose(&query).await;
        let hits = messages::search(
            &self.db.lock().unwrap(),
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

    fn sql_query(&self, args: &Value) -> Result<Outcome> {
        let query = str_arg(args, "query")?;
        Ok(Outcome::Text(db::query(
            &self.db.lock().unwrap(),
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
        let url = str_arg(args, "url")?;
        let body = self
            .fetch_capped(reqwest::Method::GET, &url, None, None)
            .await?;
        let text = html2text::from_read(body.as_bytes(), 100).unwrap_or_else(|_| body.clone());
        Ok(Outcome::Text(crate::truncate(&text, 12_000)))
    }

    async fn generate_image(&self, args: &Value) -> Result<Outcome> {
        let prompt = str_arg(args, "prompt")?;
        let image = self
            .llm
            .image(&prompt, args.get("model").and_then(Value::as_str))
            .await?;
        Ok(Outcome::Image {
            bytes: image.bytes,
            media_type: image.media_type,
            caption: prompt,
        })
    }

    async fn bash(&self, args: &Value) -> Result<Outcome> {
        let out = self.sandbox.run(&str_arg(args, "script")?).await?;

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

    fn write_file(&self, args: &Value) -> Result<Outcome> {
        Ok(Outcome::Text(self.workspace.write(
            &str_arg(args, "path")?,
            &str_arg(args, "content")?,
        )?))
    }

    fn edit_file(&self, args: &Value) -> Result<Outcome> {
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
            self.workspace.edit(&str_arg(args, "path")?, &edits)?,
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
        cron::upsert(&self.db.lock().unwrap(), &job)?;
        Ok(Outcome::Text(format!(
            "Scheduled '{}' at '{}'. The reconcile loop picks it up within a minute.",
            job.name, job.schedule
        )))
    }

    fn cron_delete(&self, args: &Value) -> Result<Outcome> {
        let name = str_arg(args, "name")?;
        Ok(Outcome::Text(
            if cron::delete(&self.db.lock().unwrap(), &name)? {
                format!("Deleted job '{name}'.")
            } else {
                format!("No job named '{name}'.")
            },
        ))
    }

    /// Embed the unquoted part of a query, which is what ranking by meaning uses.
    ///
    /// Returns None when everything was quoted, so a purely exact search costs no round trip.
    /// A failure here is also None rather than an error: search then falls back to keyword matching, which is far better than failing the tool.
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

    /// Shared HTTP path for web_fetch and http_request.
    /// Errors above the byte cap rather than truncating, so a partial body is never mistaken for a whole one.
    async fn fetch_capped(
        &self,
        method: reqwest::Method,
        url: &str,
        headers: Option<Value>,
        body: Option<String>,
    ) -> Result<String> {
        let parsed = reqwest::Url::parse(url).context("invalid URL")?;
        if !matches!(parsed.scheme(), "http" | "https") {
            anyhow::bail!("only http and https are permitted");
        }

        let mut req = self.http.request(method, parsed);
        if let Some(Value::Object(map)) = headers {
            for (k, v) in map {
                if let Some(v) = v.as_str() {
                    req = req.header(k, v);
                }
            }
        }
        if let Some(b) = body {
            req = req.body(b);
        }

        let resp = req.send().await.context("request failed")?;
        let status = resp.status();
        let bytes = resp.bytes().await.context("reading response body")?;

        if bytes.len() > self.config.limits.max_response_bytes {
            anyhow::bail!(
                "response was {} bytes, over the {} byte cap",
                bytes.len(),
                self.config.limits.max_response_bytes
            );
        }

        let text = String::from_utf8_lossy(&bytes).to_string();
        if !status.is_success() {
            return Ok(format!("HTTP {status}\n{}", crate::truncate(&text, 2000)));
        }
        Ok(text)
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

/// A caller-supplied row limit, defaulted and clamped.
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
