//! Tool definitions and dispatch.
//!
//! Every tool here exists because it was asked for in conversation. There is no
//! approval gate: the sender allowlist is the boundary.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

use crate::config::Config;
use crate::cron::{CronStore, Job};
use crate::exec::Sandbox;
use crate::llm::Llm;
use crate::memory::Memory;
use crate::messages::Archive;

pub struct Tools {
    pub memory: Arc<Mutex<Memory>>,
    pub archive: Arc<Mutex<Archive>>,
    pub cron: Arc<Mutex<CronStore>>,
    pub sandbox: Arc<Sandbox>,
    pub llm: Arc<Llm>,
    pub http: reqwest::Client,
    pub exa_key: Option<String>,
    pub config: Arc<Config>,
}

/// What a tool produced. Images travel separately so the Matrix layer can
/// upload bytes rather than stuffing base64 through the model context.
pub enum Outcome {
    Text(String),
    Image {
        bytes: Vec<u8>,
        media_type: String,
        caption: String,
    },
}

impl Outcome {
    /// What the model sees. For an image that is a short acknowledgement; the
    /// bytes go to Matrix directly.
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
            "Search your durable memory. Use this BEFORE answering about any person, character, project, file or past decision, and before saying you have no record of something.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Keywords to search for" },
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
                    "key": { "type": "string", "description": "Short stable identifier, e.g. 'zog' or 'aiden-timezone'" },
                    "content": { "type": "string" },
                    "category": { "type": "string", "enum": ["core", "daily", "conversation"] }
                },
                "required": ["key", "content"]
            }),
        ),
        f(
            "memory_forget",
            "Delete a memory by key.",
            json!({
                "type": "object",
                "properties": { "key": { "type": "string" } },
                "required": ["key"]
            }),
        ),
        f(
            "search_messages",
            "Search the full history of messages in this chat, for what someone actually said. memory_recall searches notes you chose to keep; this searches everything. Bare words match approximately and tolerate misspellings. Put a word in double quotes to require it exactly. The two combine, so 'fifa \"2025\" world cup' finds messages about the world cup that definitely mention 2025.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Bare words are fuzzy; \"quoted\" words are required exactly" },
                    "limit": { "type": "integer", "description": "Default 8" }
                },
                "required": ["query"]
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
            "Fetch a web page and return it as plain text.",
            json!({
                "type": "object",
                "properties": { "url": { "type": "string" } },
                "required": ["url"]
            }),
        ),
        f(
            "http_request",
            "Make an arbitrary HTTP request, for JSON APIs. Public hosts only.",
            json!({
                "type": "object",
                "properties": {
                    "method": { "type": "string", "enum": ["GET", "POST", "PUT", "DELETE"] },
                    "url": { "type": "string" },
                    "headers": { "type": "object", "description": "Optional header map" },
                    "body": { "type": "string", "description": "Optional request body" }
                },
                "required": ["method", "url"]
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
            "run_code",
            "Execute code in a sandbox and return its output. Has network access but cannot reach the LAN or read any secrets. Use for calculation, data processing and backtesting.",
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
            "cron_list",
            "List scheduled jobs.",
            json!({ "type": "object", "properties": {} }),
        ),
        f(
            "cron_delete",
            "Delete a scheduled job by name.",
            json!({
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"]
            }),
        ),
        f(
            "time_now",
            "Current date and time.",
            json!({
                "type": "object",
                "properties": { "timezone": { "type": "string", "description": "IANA zone, optional" } }
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
    pub async fn dispatch(&self, name: &str, args: &Value, room_id: &str) -> Outcome {
        match self.run(name, args, room_id).await {
            Ok(outcome) => outcome,
            // Tool failures are information for the model, not turn-ending
            // errors: it should be able to try something else or say what broke.
            Err(e) => Outcome::Text(format!("Error from {name}: {e}")),
        }
    }

    async fn run(&self, name: &str, args: &Value, room_id: &str) -> Result<Outcome> {
        match name {
            "memory_recall" => {
                let query = str_arg(args, "query")?;
                let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(8) as usize;
                let hits = {
                    let mem = self.memory.lock().unwrap();
                    mem.recall(&query, limit.clamp(1, 25))?
                };
                if hits.is_empty() {
                    return Ok(Outcome::Text(format!("No memories matched '{query}'.")));
                }
                let body = hits
                    .iter()
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
                    .join("\n");
                Ok(Outcome::Text(body))
            }

            "memory_store" => {
                let key = str_arg(args, "key")?;
                let content = str_arg(args, "content")?;
                let category = args
                    .get("category")
                    .and_then(Value::as_str)
                    .unwrap_or("core");
                {
                    let mem = self.memory.lock().unwrap();
                    mem.store(&key, &content, category, Some(room_id))?;
                }
                Ok(Outcome::Text(format!("Stored under '{key}'.")))
            }

            "memory_forget" => {
                let key = str_arg(args, "key")?;
                let gone = {
                    let mem = self.memory.lock().unwrap();
                    mem.forget(&key)?
                };
                Ok(Outcome::Text(if gone {
                    format!("Deleted '{key}'.")
                } else {
                    format!("No memory under '{key}'.")
                }))
            }

            "search_messages" => {
                let query = str_arg(args, "query")?;
                let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(8) as usize;
                let hits = {
                    let a = self.archive.lock().unwrap();
                    a.search(&query, limit.clamp(1, 30))?
                };
                if hits.is_empty() {
                    return Ok(Outcome::Text(format!("No messages matched '{query}'.")));
                }
                let body = hits
                    .iter()
                    .map(|h| format!("[{}] {}: {}", &h.at[..10.min(h.at.len())], h.sender, h.body))
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(Outcome::Text(body))
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

            "http_request" => {
                let method = str_arg(args, "method").unwrap_or_else(|_| "GET".into());
                let url = str_arg(args, "url")?;
                let method = reqwest::Method::from_bytes(method.to_uppercase().as_bytes())
                    .context("unsupported HTTP method")?;
                let headers = args.get("headers").cloned();
                let body = args.get("body").and_then(Value::as_str).map(str::to_string);
                let text = self.fetch_capped(method, &url, headers, body).await?;
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
                    room_id: room_id.to_string(),
                    enabled: true,
                };
                job.validate()?;
                {
                    let store = self.cron.lock().unwrap();
                    store.upsert(&job)?;
                }
                Ok(Outcome::Text(format!(
                    "Scheduled '{name}' at '{schedule}'. It takes effect on the next restart or immediately if the scheduler picked it up."
                )))
            }

            "cron_list" => {
                let jobs = {
                    let store = self.cron.lock().unwrap();
                    store.list()?
                };
                if jobs.is_empty() {
                    return Ok(Outcome::Text("No scheduled jobs.".into()));
                }
                let body = jobs
                    .iter()
                    .map(|j| {
                        format!(
                            "{} — '{}' ({}) — {}",
                            j.name, j.schedule, j.timezone, j.prompt
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(Outcome::Text(body))
            }

            "cron_delete" => {
                let name = str_arg(args, "name")?;
                let gone = {
                    let store = self.cron.lock().unwrap();
                    store.delete(&name)?
                };
                Ok(Outcome::Text(if gone {
                    format!("Deleted job '{name}'.")
                } else {
                    format!("No job named '{name}'.")
                }))
            }

            "time_now" => {
                let tz: chrono_tz::Tz = args
                    .get("timezone")
                    .and_then(Value::as_str)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or_else(|| self.config.tz());
                let now = chrono::Utc::now().with_timezone(&tz);
                Ok(Outcome::Text(
                    now.format("%A, %-d %B %Y, %H:%M (%Z)").to_string(),
                ))
            }

            other => anyhow::bail!("unknown tool '{other}'"),
        }
    }

    /// Shared HTTP path for web_fetch and http_request. Errors above the byte
    /// cap rather than truncating, so a partial body is never mistaken for a
    /// whole one.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definitions_cover_the_agreed_surface() {
        let names: Vec<String> = definitions()
            .iter()
            .map(|d| d["function"]["name"].as_str().unwrap().to_string())
            .collect();
        for expected in [
            "memory_store",
            "memory_recall",
            "memory_forget",
            "web_search",
            "web_fetch",
            "http_request",
            "generate_image",
            "run_code",
            "cron_create",
            "cron_list",
            "cron_delete",
            "search_messages",
            "time_now",
        ] {
            assert!(names.contains(&expected.to_string()), "missing {expected}");
        }
    }
}
