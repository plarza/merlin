//! The turn loop: prompt, tool calls, reply.

use anyhow::Result;
use std::sync::Arc;

use crate::llm::{Attachment, Llm, Message};
use crate::tools::{Ctx, Outcome, Tools, definitions};

pub struct Agent {
    pub llm: Arc<Llm>,
    pub tools: Arc<Tools>,
    pub soul: String,
    pub max_iterations: usize,
    pub timezone: chrono_tz::Tz,
}

#[derive(Default)]
pub struct TurnResult {
    pub text: String,
    pub images: Vec<Image>,
    /// Tokens across every completion in the turn, for cost accounting.
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Tool names in the order they ran, so a slow turn can be explained.
    pub tools_used: Vec<String>,
}

/// Where intermediate messages go while a turn is still running.
/// Fed by the `send_message` tool, so the model chooses when an update is worth sending rather than having its narration forwarded whether it meant it or not.
pub type Progress = tokio::sync::mpsc::UnboundedSender<String>;

pub struct Image {
    pub bytes: Vec<u8>,
    pub media_type: String,
    pub caption: String,
}

/// Context for one inbound message.
pub struct Incoming<'a> {
    pub room_id: &'a str,
    pub sender: &'a str,
    pub body: &'a str,
    /// Ambient messages seen but not answered, oldest first.
    pub ambient: Option<String>,
    /// Text of the message being replied to, when this is a reply, so a reply carrying only the bot's name still has its subject.
    pub reply_parent: Option<String>,
    /// Images sent with the message, already downloaded and decrypted.
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

        for round in 0..self.max_iterations {
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

            // Echo the assistant's tool-call message back before the results, or the next request is malformed.
            messages.push(reply.clone());

            for call in &reply.tool_calls {
                let args: serde_json::Value = serde_json::from_str(&call.function.arguments)
                    .unwrap_or_else(|_| serde_json::json!({}));

                let started = std::time::Instant::now();
                let outcome = self.tools.dispatch(&call.function.name, &args, &ctx).await;
                tracing::info!(
                    tool = %call.function.name,
                    ms = started.elapsed().as_millis() as u64,
                    "tool finished"
                );
                result.tools_used.push(call.function.name.clone());

                messages.push(Message::tool_result(&call.id, outcome.for_model()));

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

        // Out of iterations.
        // Rather than reporting the limit, which tells the user nothing, ask for an answer from what was already gathered.
        // Tools are withheld from this call so the model cannot spend another round.
        messages.push(Message::user(
            "You have used all available tool steps. Answer now with what you \
             have already found, and say plainly which parts you could not \
             confirm. Do not request more tools.",
        ));

        let forced = self.llm.chat(&messages, &[]).await?;
        result.prompt_tokens += forced.usage.prompt;
        result.completion_tokens += forced.usage.completion;
        result.text = forced.message.text().unwrap_or_default().trim().to_string();

        if result.text.is_empty() {
            result.text =
                "I hit the tool limit before finding an answer. Narrow the question and I'll retry."
                    .to_string();
        }
        Ok(result)
    }
}

/// Assemble the system prompt.
/// Free-standing so it can be tested without constructing an LLM client or a tool registry.
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
             Messages you were not addressed in, for context. Do not reply to them.\n\n{ambient}\n"
        ));
    }

    prompt
}
