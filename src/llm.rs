//! OpenRouter client.
//!
//! Two endpoints, one key.
//! Text goes through `/chat/completions` with function calling.
//! Images go through `/images`; image models are absent from the chat model list and return 404 from `/chat/completions`.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

const CHAT_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
const IMAGE_URL: &str = "https://openrouter.ai/api/v1/images";

pub struct Llm {
    http: reqwest::Client,
    api_key: String,
    chat_model: String,
    image_model: String,
    reasoning_effort: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    /// A plain string, or an array of parts when the message carries images.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "default_tool_type")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    /// Raw JSON string, per the OpenAI wire format.
    pub arguments: String,
}

fn default_tool_type() -> String {
    "function".into()
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self::plain("system", content)
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self::plain("user", content)
    }
    fn plain(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: Some(Value::String(content.into())),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// A user message carrying images alongside its text.
    /// Images are inlined as data URIs, which is the form the chat endpoint accepts.
    pub fn user_with_images(text: impl Into<String>, images: &[Attachment]) -> Self {
        let mut parts = vec![json!({ "type": "text", "text": text.into() })];
        for image in images {
            use base64::Engine;
            let encoded = base64::engine::general_purpose::STANDARD.encode(&image.bytes);
            parts.push(json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{};base64,{}", image.media_type, encoded) }
            }));
        }
        Self {
            role: "user".into(),
            content: Some(Value::Array(parts)),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// Text of the message, when it has any.
    pub fn text(&self) -> Option<&str> {
        self.content.as_ref().and_then(Value::as_str)
    }

    pub fn tool_result(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: Some(Value::String(content.into())),
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
        }
    }
}

/// Token counts for one completion, as reported by the provider.
#[derive(Debug, Default, Clone, Copy)]
pub struct Usage {
    pub prompt: u64,
    pub completion: u64,
}

impl Usage {
    pub fn total(&self) -> u64 {
        self.prompt + self.completion
    }
}

pub struct Completion {
    pub message: Message,
    pub usage: Usage,
}

/// A file received in a room, already downloaded and decrypted.
#[derive(Debug, Clone)]
pub struct Attachment {
    pub bytes: Vec<u8>,
    pub media_type: String,
}

pub struct GeneratedImage {
    pub bytes: Vec<u8>,
    pub media_type: String,
}

impl Llm {
    pub fn new(
        api_key: String,
        chat_model: String,
        image_model: String,
        reasoning_effort: String,
        timeout_s: u64,
    ) -> Result<Self> {
        // read_timeout applies between reads rather than to the whole response, so a long generation is fine and only a genuine stall fails.
        let http = reqwest::Client::builder()
            .read_timeout(Duration::from_secs(timeout_s))
            .connect_timeout(Duration::from_secs(20))
            .build()?;
        Ok(Self {
            http,
            api_key,
            chat_model,
            image_model,
            reasoning_effort,
        })
    }

    /// One completion round, streamed.
    ///
    /// Streaming is what makes a long answer safe: tokens arrive continuously, so the client can use an idle timeout rather than a deadline on the whole response.
    /// A non-streamed request sends nothing until it is finished, which means a slow generation is indistinguishable from a hang and trips a total timeout.
    /// One completion round, retried once on a failure that is plausibly transient.
    ///
    /// A gateway error or an idle timeout ends a turn with nothing to show for the tokens already spent, and both are common enough to be worth absorbing.
    /// A refusal, a bad request or a rate limit is returned immediately, since repeating it would only fail again.
    pub async fn chat(&self, messages: &[Message], tools: &[Value]) -> Result<Completion> {
        match self.chat_once(messages, tools).await {
            Ok(completion) => Ok(completion),
            Err(e) if is_transient(&e) => {
                tracing::warn!(error = %e, "chat failed; retrying once");
                tokio::time::sleep(Duration::from_secs(2)).await;
                self.chat_once(messages, tools).await
            }
            Err(e) => Err(e),
        }
    }

    async fn chat_once(&self, messages: &[Message], tools: &[Value]) -> Result<Completion> {
        let mut body = json!({
            "model": self.chat_model,
            "messages": messages,
            "stream": true,
            "stream_options": { "include_usage": true },
        });
        if !tools.is_empty() {
            body["tools"] = json!(tools);
            body["tool_choice"] = json!("auto");
        }

        // Reasoning cannot be switched off on every endpoint, but its budget can be capped.
        // Left uncapped, a model of this class spends the large majority of its output tokens thinking, on trivial questions as much as hard ones,
        // and pays that cost again on every tool round.
        if !self.reasoning_effort.is_empty() && self.reasoning_effort != "default" {
            body["reasoning"] = json!({ "effort": self.reasoning_effort });
        }

        let resp = self
            .http
            .post(CHAT_URL)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| classify(e, "chat"))?;

        let status = resp.status();
        if !status.is_success() {
            let raw = resp.text().await.unwrap_or_default();
            bail!("OpenRouter chat {status}: {}", head(&raw));
        }

        self.collect_stream(resp).await
    }

    /// Accumulate one assistant message from server-sent events.
    async fn collect_stream(&self, resp: reqwest::Response) -> Result<Completion> {
        use futures_util::StreamExt;

        let mut content = String::new();
        let mut calls: Vec<ToolCall> = Vec::new();
        let mut usage = Usage::default();
        let mut buffer = String::new();
        let mut stream = resp.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| classify(e, "chat stream"))?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            // Events are separated by newlines; keep any partial line for the next chunk.
            while let Some(newline) = buffer.find('\n') {
                let line = buffer[..newline].trim().to_string();
                buffer.drain(..=newline);

                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data == "[DONE]" {
                    break;
                }
                let Ok(event): std::result::Result<Value, _> = serde_json::from_str(data) else {
                    continue;
                };

                if let Some(u) = event.get("usage") {
                    usage.prompt = u
                        .get("prompt_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(usage.prompt);
                    usage.completion = u
                        .get("completion_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(usage.completion);
                }

                let Some(delta) = event.pointer("/choices/0/delta") else {
                    continue;
                };
                if let Some(text) = delta.get("content").and_then(Value::as_str) {
                    content.push_str(text);
                }
                if let Some(parts) = delta.get("tool_calls").and_then(Value::as_array) {
                    merge_tool_calls(&mut calls, parts);
                }
            }
        }

        Ok(Completion {
            message: Message {
                role: "assistant".into(),
                content: if content.is_empty() {
                    None
                } else {
                    Some(Value::String(content))
                },
                tool_calls: calls,
                tool_call_id: None,
            },
            usage,
        })
    }

    /// Image generation.
    /// A separate endpoint with a prompt rather than a message list; returns base64 plus the media type to upload as.
    pub async fn image(&self, prompt: &str, model: Option<&str>) -> Result<GeneratedImage> {
        let body = json!({
            "model": model.unwrap_or(&self.image_model),
            "prompt": prompt,
        });

        let resp = self
            .http
            .post(IMAGE_URL)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| classify(e, "images"))?;

        let status = resp.status();
        let raw = resp.text().await.map_err(|e| classify(e, "images"))?;
        let payload: Value = serde_json::from_str(&raw).map_err(|e| {
            anyhow::anyhow!(
                "OpenRouter images returned non-JSON ({status}): {e}: {}",
                head(&raw)
            )
        })?;

        if !status.is_success() {
            bail!(
                "OpenRouter images {}: {}",
                status,
                payload
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
            );
        }

        let first = payload
            .pointer("/data/0")
            .context("image response had no data")?;

        let b64 = first
            .get("b64_json")
            .and_then(Value::as_str)
            .context("image response had no b64_json")?;
        let media_type = first
            .get("media_type")
            .and_then(Value::as_str)
            .unwrap_or("image/png")
            .to_string();

        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .context("decoding image base64")?;

        Ok(GeneratedImage { bytes, media_type })
    }
}

/// Tool calls arrive in fragments across events: the name once, the arguments a few characters at a time, each identified by its index.
fn merge_tool_calls(calls: &mut Vec<ToolCall>, parts: &[Value]) {
    for part in parts {
        let index = part.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
        while calls.len() <= index {
            calls.push(ToolCall {
                id: String::new(),
                kind: "function".into(),
                function: FunctionCall {
                    name: String::new(),
                    arguments: String::new(),
                },
            });
        }
        let call = &mut calls[index];
        if let Some(id) = part.get("id").and_then(Value::as_str) {
            call.id = id.to_string();
        }
        if let Some(f) = part.get("function") {
            if let Some(name) = f.get("name").and_then(Value::as_str) {
                call.function.name.push_str(name);
            }
            if let Some(args) = f.get("arguments").and_then(Value::as_str) {
                call.function.arguments.push_str(args);
            }
        }
    }
}

/// Whether a failure is worth repeating.
/// Matched on the text because the underlying reqwest error is consumed by `classify` before it reaches here, and the status codes are the ones that mean "ask again later".
fn is_transient(e: &anyhow::Error) -> bool {
    let text = e.to_string();
    text.contains("timed out")
        || text.contains("could not reach")
        || ["500", "502", "503", "504"]
            .iter()
            .any(|code| text.contains(&format!("chat {code}")))
}

/// Distinguishes a timeout from a connection failure, which need different responses.
fn classify(e: reqwest::Error, what: &str) -> anyhow::Error {
    if e.is_timeout() {
        anyhow::anyhow!("OpenRouter {what} timed out; the model took too long to respond")
    } else if e.is_connect() {
        anyhow::anyhow!("could not reach OpenRouter {what}: {e}")
    } else {
        anyhow::anyhow!("OpenRouter {what} request failed: {e}")
    }
}

/// First line of a response body, for error messages.
fn head(raw: &str) -> String {
    let first: String = raw.lines().next().unwrap_or("").chars().take(200).collect();
    if first.is_empty() {
        "(empty body)".into()
    } else {
        first
    }
}
