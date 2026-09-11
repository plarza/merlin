use anyhow::Result;
use std::sync::Arc;

use crate::llm::{Attachment, Llm, Message};
use crate::tools::{Ctx, Outcome, Tools, definitions};

const LOG_CHARS: usize = 512;

pub struct Agent {
    pub llm: Arc<Llm>,
    pub tools: Arc<Tools>,
    pub soul: String,
    pub max_iterations: usize,
    pub max_duration: std::time::Duration,
    pub timezone: chrono_tz::Tz,
}

#[derive(Default)]
pub struct TurnResult {
    pub text: String,
    pub images: Vec<Image>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub tools_used: Vec<String>,
}

pub type Progress = tokio::sync::mpsc::UnboundedSender<String>;

pub struct Image {
    pub bytes: Vec<u8>,
    pub media_type: String,
    pub caption: String,
}

pub struct Incoming<'a> {
    pub room_id: &'a str,
    pub sender: &'a str,
    pub body: &'a str,
    pub ambient: Option<String>,
    pub reply_parent: Option<String>,
    pub attachments: Vec<Attachment>,
}

impl Agent {
    pub async fn turn(
        &self,
        incoming: Incoming<'_>,
        progress: Option<&Progress>,
    ) -> Result<TurnResult> {
        let now = chrono::Utc::now()
            .with_timezone(&self.timezone)
            .format("%A, %-d %B %Y, %H:%M (%Z)")
            .to_string();
        let mut messages = vec![Message::system(system_prompt(&self.soul, &incoming, &now))];
        let text = format!("{}: {}", incoming.sender, incoming.body);
        messages.push(if incoming.attachments.is_empty() {
            Message::user(text)
        } else {
            Message::user_with_images(text, &incoming.attachments)
        });

        let ctx = Ctx {
            room_id: incoming.room_id,
            progress,
        };
        let tool_defs = definitions();
        let mut result = TurnResult::default();

        let deadline = std::time::Instant::now() + self.max_duration;

        for round in 0..self.max_iterations {
            if std::time::Instant::now() >= deadline {
                tracing::warn!(round, "turn exceeded its time budget");
                break;
            }

            let completion = self.llm.chat(&messages, &tool_defs).await?;
            result.prompt_tokens += completion.usage.prompt;
            result.completion_tokens += completion.usage.completion;
            let reply = completion.message;

            if reply.tool_calls.is_empty() {
                result.text = reply.text().unwrap_or_default().trim().to_string();
                return Ok(result);
            }

            tracing::debug!(
                round,
                calls = reply.tool_calls.len(),
                "model requested tools"
            );

            messages.push(reply.clone());

            for call in &reply.tool_calls {
                let args: serde_json::Value = serde_json::from_str(&call.function.arguments)
                    .unwrap_or_else(|_| serde_json::json!({}));

                let started = std::time::Instant::now();
                let outcome = self.tools.dispatch(&call.function.name, &args, &ctx).await;
                let rendered = outcome.for_model();
                tracing::info!(
                    tool = %call.function.name,
                    args = %summarise(&args),
                    ms = started.elapsed().as_millis() as u64,
                    result = %flatten(&rendered, LOG_CHARS),
                    "tool finished"
                );
                result.tools_used.push(call.function.name.clone());

                messages.push(Message::tool_result(&call.id, rendered));

                if let Outcome::Image {
                    bytes,
                    media_type,
                    caption,
                } = outcome
                {
                    result.images.push(Image {
                        bytes,
                        media_type,
                        caption,
                    });
                }
            }
        }

        messages.push(Message::user(
            "You have used all the time or tool steps available. Answer now with \
             what you have already found, and say plainly which parts you could \
             not confirm. Do not request more tools.",
        ));

        let forced = self.llm.chat(&messages, &[]).await?;
        result.prompt_tokens += forced.usage.prompt;
        result.completion_tokens += forced.usage.completion;
        result.text = forced.message.text().unwrap_or_default().trim().to_string();

        if result.text.is_empty() {
            result.text =
                "I ran out of steps before finding an answer. Narrow the question and I'll retry."
                    .to_string();
        }
        Ok(result)
    }
}

pub fn summarise(args: &serde_json::Value) -> String {
    let Some(object) = args.as_object() else {
        return String::new();
    };

    object
        .iter()
        .map(|(key, value)| {
            let text = match value {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            format!("{key}={}", flatten(&text, LOG_CHARS))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn flatten(text: &str, max: usize) -> String {
    crate::truncate(&text.split_whitespace().collect::<Vec<_>>().join(" "), max)
}

fn system_prompt(soul: &str, incoming: &Incoming<'_>, now: &str) -> String {
    let mut prompt = soul.to_string();

    prompt.push_str(&format!("\n\n## right now\n\n{now}\n"));

    prompt.push_str(
        "\n\n## your machine\n\n         You have a persistent Linux sandbox and a workspace directory that survive between \
         conversations. run_code executes bash or python there, starting in the workspace, \
         and you are root inside it: install whatever you need with apk, pip or npm and it \
         stays installed. Reach the internet with curl or wget from the shell rather than \
         asking for a tool. Nothing you do in there can touch anything else, so experiment \
         freely, but the LAN is unreachable by design.\n\n         Read, list and search with cat, ls and rg in the shell. Two things are tools rather \
         than shell commands because the shell cannot do them safely: write_file, which avoids \
         guessing a heredoc delimiter, and edit_file, which refuses unless the text you give it \
         matches exactly once and applies all of its edits or none.\n",
    );

    if let Some(parent) = &incoming.reply_parent {
        prompt.push_str(&format!(
            "\n\n## the message being replied to\n\n{parent}\n"
        ));
    }

    if let Some(ambient) = &incoming.ambient {
        prompt.push_str(&format!(
            "\n\n## recent room conversation\n\n\
             The room's recent messages, your own among them, each prefixed with who sent it. \
             Context only: answer the message above, not these.\n\n{ambient}\n"
        ));
    }

    prompt
}
