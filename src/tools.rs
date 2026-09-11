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
            "run_code",
            "Execute code in the sandbox and return its output. It runs in the workspace, so files you wrote are there and files it writes persist for later turns and later tools. Has network access but cannot reach the LAN or read any secrets. Use for calculation, data processing, and for running and testing code you have written.",
            json!({
                "type": "object",
                "properties": {
                    "language": { "type": "string", "enum": ["python", "bash"] },
                    "source": { "type": "string" }
                },
                "required": ["language", "source"]
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

    async fn run(&self, name: &str, args: &Value, ctx: &Ctx<'_>) -> Result<Outcome> {
        match name {
            "memory_recall" => {
                let query = str_arg(args, "query")?;
                let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(8) as usize;
                let vector = self.embed_loose(&query).await;
                let hits = memory::recall(
                    &self.db.lock().unwrap(),
                    &query,
                    vector.as_deref(),
                    limit.clamp(1, 25),
                )?;
                if hits.is_empty() {
                    return Ok(Outcome::Text(format!("No memories matched '{query}'.")));
                }
                Ok(Outcome::Text(render_memories(&hits)))
            }

            "memory_store" => {
                let key = str_arg(args, "key")?;
                let content = str_arg(args, "content")?;
                let category = args
                    .get("category")
                    .and_then(Value::as_str)
                    .unwrap_or("core");
                memory::store(
                    &self.db.lock().unwrap(),
                    &key,
                    &content,
                    category,
                    Some(ctx.room_id),
                )?;
                Ok(Outcome::Text(format!("Stored under '{key}'.")))
            }

            "memory_forget" => {
                let key = str_arg(args, "key")?;
                let gone = memory::forget(&self.db.lock().unwrap(), &key)?;
                Ok(Outcome::Text(if gone {
                    format!("Deleted '{key}'.")
                } else {
                    format!("No memory under '{key}'.")
                }))
            }

            "search_messages" => {
                let query = str_arg(args, "query")?;
                let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(8) as usize;
                let vector = self.embed_loose(&query).await;
                let hits = messages::search(
                    &self.db.lock().unwrap(),
                    &query,
                    vector.as_deref(),
                    limit.clamp(1, 30),
                )?;
                if hits.is_empty() {
                    return Ok(Outcome::Text(format!("No messages matched '{query}'.")));
                }
                Ok(Outcome::Text(render_messages(&hits)))
            }

            "send_message" => {
                let text = str_arg(args, "text")?;
                match ctx.progress {
                    Some(sink) => {
                        let _ = sink.send(text);
                        Ok(Outcome::Text("Sent to the room.".into()))
                    }
                    None => Ok(Outcome::Text(
                        "There is no room to send to from here.".into(),
                    )),
                }
            }

            "web_search" => {
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

                let resp = self
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
                    .context("calling Exa")?;

                let payload: Value = resp.json().await.context("decoding Exa response")?;
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
                        let title = r
                            .get("title")
                            .and_then(Value::as_str)
                            .unwrap_or("(untitled)");
                        let url = r.get("url").and_then(Value::as_str).unwrap_or("");
                        let text = r.get("text").and_then(Value::as_str).unwrap_or("");
                        format!("{title}\n{url}\n{}", truncate(text, 900))
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n---\n\n");
                Ok(Outcome::Text(body))
            }

            "web_fetch" => {
                let url = str_arg(args, "url")?;
                let body = self
                    .fetch_capped(reqwest::Method::GET, &url, None, None)
                    .await?;
                let text =
                    html2text::from_read(body.as_bytes(), 100).unwrap_or_else(|_| body.clone());
                Ok(Outcome::Text(truncate(&text, 12_000)))
            }

            "generate_image" => {
                let prompt = str_arg(args, "prompt")?;
                let model = args.get("model").and_then(Value::as_str);
                let image = self.llm.image(&prompt, model).await?;
                Ok(Outcome::Image {
                    bytes: image.bytes,
                    media_type: image.media_type,
                    caption: prompt,
                })
            }

            "run_code" => {
                let language = str_arg(args, "language")?;
                let source = str_arg(args, "source")?;
                let out = self.sandbox.run(&language, &source, None).await?;
                let mut report = String::new();
                if !out.stdout.is_empty() {
                    report.push_str(&out.stdout);
                }
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

            "write_file" => {
                let path = str_arg(args, "path")?;
                let content = str_arg(args, "content")?;
                Ok(Outcome::Text(self.workspace.write(&path, &content)?))
            }

            "edit_file" => {
                let path = str_arg(args, "path")?;
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
                Ok(Outcome::Text(self.workspace.edit(&path, &edits)?))
            }

            "sql_query" => {
                let query = str_arg(args, "query")?;
                let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(50) as usize;
                Ok(Outcome::Text(db::query(
                    &self.db.lock().unwrap(),
                    &query,
                    limit.clamp(1, 500),
                )?))
            }

            "cron_create" => {
                let name = str_arg(args, "name")?;
                let schedule = str_arg(args, "schedule")?;
                let prompt = str_arg(args, "prompt")?;
                let tz = args
                    .get("timezone")
                    .and_then(Value::as_str)
                    .unwrap_or(&self.config.timezone)
                    .to_string();

                let job = Job {
                    name: name.clone(),
                    schedule: schedule.clone(),
                    timezone: tz,
                    prompt,
                    room_id: ctx.room_id.to_string(),
                    enabled: true,
                };
                job.validate()?;
                cron::upsert(&self.db.lock().unwrap(), &job)?;
                Ok(Outcome::Text(format!(
                    "Scheduled '{name}' at '{schedule}'. It takes effect on the next restart or immediately if the scheduler picked it up."
                )))
            }

            "cron_delete" => {
                let name = str_arg(args, "name")?;
                let gone = cron::delete(&self.db.lock().unwrap(), &name)?;
                Ok(Outcome::Text(if gone {
                    format!("Deleted job '{name}'.")
                } else {
                    format!("No job named '{name}'.")
                }))
            }

            other => anyhow::bail!("unknown tool '{other}'"),
        }
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
            return Ok(format!("HTTP {status}\n{}", truncate(&text, 2000)));
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

fn str_arg(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .with_context(|| format!("missing required argument '{key}'"))
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}
