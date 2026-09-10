//! The turn loop: prompt, tool calls, reply.

use anyhow::Result;
use std::sync::Arc;

use crate::llm::{Llm, Message};
use crate::tools::{Outcome, Tools, definitions};

pub struct Agent {
    pub llm: Arc<Llm>,
    pub tools: Arc<Tools>,
    pub soul: String,
    pub max_iterations: usize,
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
}

impl Agent {
    pub async fn turn(&self, incoming: Incoming<'_>) -> Result<TurnResult> {
        let mut messages = vec![Message::system(system_prompt(&self.soul, &incoming))];
        messages.push(Message::user(format!(
            "{}: {}",
            incoming.sender, incoming.body
        )));

        let tool_defs = definitions();
        let mut result = TurnResult::default();

        for round in 0..self.max_iterations {
            let completion = self.llm.chat(&messages, &tool_defs).await?;
            result.prompt_tokens += completion.usage.prompt;
            result.completion_tokens += completion.usage.completion;
            let reply = completion.message;

            if reply.tool_calls.is_empty() {
                result.text = reply.content.unwrap_or_default().trim().to_string();
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
                let outcome = self
                    .tools
                    .dispatch(&call.function.name, &args, incoming.room_id)
                    .await;
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
        result.text = forced
            .message
            .content
            .unwrap_or_default()
            .trim()
            .to_string();

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
fn system_prompt(soul: &str, incoming: &Incoming<'_>) -> String {
    let mut prompt = soul.to_string();

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
