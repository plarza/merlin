//! OpenRouter client.
//!
//! Two endpoints, one key. Text goes through `/chat/completions` with function
//! calling. Images go through `/images`; image models are absent from the chat
//! model list and return 404 from `/chat/completions`.

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
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
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn tool_result(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
        }
    }
}

pub struct GeneratedImage {
    pub bytes: Vec<u8>,
    pub media_type: String,
}

impl Llm {
    pub fn new(api_key: String, chat_model: String, image_model: String, timeout_s: u64) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout_s))
            .build()?;
        Ok(Self {
            http,
            api_key,
            chat_model,
            image_model,
        })
    }

    /// One completion round. Returns the assistant message, which may carry
    /// tool calls instead of content — the caller runs the loop.
    pub async fn chat(&self, messages: &[Message], tools: &[Value]) -> Result<Message> {
        let mut body = json!({
            "model": self.chat_model,
            "messages": messages,
        });
        if !tools.is_empty() {
            body["tools"] = json!(tools);
            body["tool_choice"] = json!("auto");
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
        // Read as text first: reqwest's timeout covers the body, so a slow
        // model surfaces here rather than at send(), and .json() would report
        // it as a parse failure.
        let raw = resp.text().await.map_err(|e| classify(e, "chat"))?;
        let payload: Value = serde_json::from_str(&raw).map_err(|e| {
            anyhow::anyhow!("OpenRouter chat returned non-JSON ({status}): {e}: {}", head(&raw))
        })?;

        if !status.is_success() {
            bail!(
                "OpenRouter chat {}: {}",
                status,
                payload
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
            );
        }

        let choice = payload
            .pointer("/choices/0/message")
            .cloned()
            .context("chat response had no choices")?;

        serde_json::from_value(choice).context("parsing assistant message")
    }

    /// Image generation. A separate endpoint with a prompt rather than a
    /// message list; returns base64 plus the media type to upload as.
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
            anyhow::anyhow!("OpenRouter images returned non-JSON ({status}): {e}: {}", head(&raw))
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

/// Distinguishes a timeout from a connection failure, which need different
/// responses.
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
    if first.is_empty() { "(empty body)".into() } else { first }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_result_shape_matches_wire_format() {
        let m = Message::tool_result("call_1", "42");
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["role"], "tool");
        assert_eq!(v["tool_call_id"], "call_1");
        assert_eq!(v["content"], "42");
        // An empty tool_calls list must not be serialised onto a tool result.
        assert!(v.get("tool_calls").is_none());
    }



    #[test]
    fn assistant_tool_call_parses() {
        let raw = json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "c1",
                "type": "function",
                "function": { "name": "memory_recall", "arguments": "{\"query\":\"zog\"}" }
            }]
        });
        let m: Message = serde_json::from_value(raw).unwrap();
        assert_eq!(m.tool_calls.len(), 1);
        assert_eq!(m.tool_calls[0].function.name, "memory_recall");
    }
}
