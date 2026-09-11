use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

const CHAT_URL: &str = "https://openrouter.ai/api/v1/chat/completions";

pub struct Llm {
    http: reqwest::Client,
    api_key: String,
    chat_model: String,
    reasoning_effort: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
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

#[derive(Debug, Default, Clone, Copy)]
pub struct Usage {
    pub prompt: u64,
    pub completion: u64,
}

pub struct Completion {
    pub message: Message,
    pub usage: Usage,
}

#[derive(Debug, Clone)]
pub struct Attachment {
    pub bytes: Vec<u8>,
    pub media_type: String,
}

impl Llm {
    pub fn new(
        api_key: String,
        chat_model: String,
        reasoning_effort: String,
        timeout_s: u64,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .read_timeout(Duration::from_secs(timeout_s))
            .connect_timeout(Duration::from_secs(20))
            .build()?;
        Ok(Self {
            http,
            api_key,
            chat_model,
            reasoning_effort,
        })
    }

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
}

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

fn is_transient(e: &anyhow::Error) -> bool {
    let text = e.to_string();
    text.contains("timed out")
        || text.contains("could not reach")
        || ["500", "502", "503", "504"]
            .iter()
            .any(|code| text.contains(&format!("chat {code}")))
}

fn classify(e: reqwest::Error, what: &str) -> anyhow::Error {
    if e.is_timeout() {
        anyhow::anyhow!("OpenRouter {what} timed out; the model took too long to respond")
    } else if e.is_connect() {
        anyhow::anyhow!("could not reach OpenRouter {what}: {e}")
    } else {
        anyhow::anyhow!("OpenRouter {what} request failed: {e}")
    }
}

fn head(raw: &str) -> String {
    let first: String = raw.lines().next().unwrap_or("").chars().take(200).collect();
    if first.is_empty() {
        "(empty body)".into()
    } else {
        first
    }
}
