use anyhow::{Context, Result};
use std::sync::Arc;
use std::time::Duration;

use mxlink::matrix_sdk::Room;
use mxlink::matrix_sdk::media::{MediaFormat, MediaRequestParameters};
use mxlink::matrix_sdk::ruma::api::client::typing::create_typing_event;
use mxlink::matrix_sdk::ruma::events::room::message::{
    MessageType, OriginalSyncRoomMessageEvent, Relation, RoomMessageEventContent,
};
use mxlink::matrix_sdk::ruma::events::typing::SyncTypingEvent;
use mxlink::{
    CallbackError, InitConfig, InvitationDecision, LoginConfig, LoginCredentials, MatrixLink,
    MessageResponseType, PersistenceConfig,
};

use crate::agent::{Agent, Incoming};
use crate::commands::Command;
use crate::config::{Config, Secrets};
use crate::llm::Attachment;
use crate::room::{Buffers, Turn, is_addressed};

const TYPING_TIMEOUT: Duration = Duration::from_secs(30);
const TYPING_REFRESH: Duration = Duration::from_secs(10);
const HELP: &str = "Available commands:\n/help — Show this help\n/model — Show the current model\n/model <model-slug> — Switch the chat model\n/reasoning — Show the current reasoning effort\n/reasoning <effort> — Switch the reasoning effort";

struct Attached {
    source: mxlink::matrix_sdk::ruma::events::room::MediaSource,
    name: String,
    media_type: Option<String>,
    viewable: bool,
}

fn extract(msgtype: &MessageType) -> Option<(String, Option<Attached>)> {
    let file = |name: &str, source, media_type, viewable| {
        let name = name.trim().to_string();
        Some((
            format!("[file] {name}"),
            Some(Attached {
                source,
                name,
                media_type,
                viewable,
            }),
        ))
    };

    match msgtype {
        MessageType::Text(m) => Some((m.body.trim().to_string(), None)),
        MessageType::Image(m) => file(
            &m.body,
            m.source.clone(),
            m.info.as_ref().and_then(|i| i.mimetype.clone()),
            true,
        ),
        MessageType::File(m) => file(&m.body, m.source.clone(), None, false),
        MessageType::Video(m) => file(&m.body, m.source.clone(), None, false),
        MessageType::Audio(m) => file(&m.body, m.source.clone(), None, false),
        _ => None,
    }
}

pub struct Bot {
    pub link: MatrixLink,
    pub agent: Arc<Agent>,
    pub buffers: Arc<Buffers>,
    pub db: Arc<std::sync::Mutex<rusqlite::Connection>>,
    pub workspace: Arc<crate::workspace::Workspace>,
    pub config: Arc<Config>,
}

pub async fn connect(config: &Config, secrets: &Secrets) -> Result<MatrixLink> {
    let state = &config.state_dir;
    std::fs::create_dir_all(state)
        .with_context(|| format!("creating state dir {}", state.display()))?;

    let credentials = LoginCredentials::UserPassword(
        config.localpart().to_string(),
        secrets.matrix_password.clone(),
    );

    // No recovery config: this account keeps no key backup, so there is nothing
    // to recover from. Room keys reach the device by --import-keys or not at all.
    let login = LoginConfig::new(
        config.homeserver.clone(),
        credentials,
        None,
        config.display_name.clone(),
    );

    let key = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(b"merlin.session.v1");
        hasher.update(secrets.session_encryption_key.as_bytes());
        let digest: [u8; 32] = hasher.finalize().into();
        mxlink::helpers::encryption::EncryptionKey::new(digest)
    };

    let persistence =
        PersistenceConfig::new(state.join("session.json"), Some(key), state.join("matrix"));

    mxlink::init(&InitConfig::new(login, persistence))
        .await
        .map_err(|e| anyhow::anyhow!("matrix login failed: {e:?}"))
}

impl Bot {
    pub async fn run(self: Arc<Self>) -> Result<()> {
        let link = self.link.clone();

        let invite_config = Arc::clone(&self.config);
        link.rooms().on_invitation(move |_event, room| {
            let config = Arc::clone(&invite_config);
            async move {
                Ok(if config.is_allowed_room(room.room_id().as_str()) {
                    InvitationDecision::Join
                } else {
                    InvitationDecision::Reject
                })
            }
        });

        let for_handler = Arc::clone(&self);
        link.messaging()
            .on_actionable_room_message(move |event, room| {
                let bot = Arc::clone(&for_handler);
                async move {
                    if let Err(e) = bot.on_message(event, room).await {
                        tracing::warn!(error = %e, "failed handling message");
                    }
                    Ok::<(), CallbackError>(())
                }
            });

        link.client()
            .add_event_handler(|ev: SyncTypingEvent, room: Room| async move {
                tracing::info!(
                    room_id = %room.room_id(),
                    typing = ?ev.content.user_ids,
                    "typing state broadcast by the server"
                );
            });

        link.start()
            .await
            .map_err(|e| anyhow::anyhow!("matrix sync stopped: {e:?}"))
    }

    async fn on_message(
        self: &Arc<Self>,
        event: OriginalSyncRoomMessageEvent,
        room: Room,
    ) -> Result<()> {
        let room_id = room.room_id().to_string();
        if !self.config.is_allowed_room(&room_id) {
            return Ok(());
        }

        let Some((body, attached)) = extract(&event.content.msgtype) else {
            return Ok(());
        };
        if body.is_empty() {
            return Ok(());
        }

        let sender = event.sender.to_string();
        self.remember(event.event_id.as_str(), &room_id, &sender, &body);

        if let Some(command) = Command::parse(&body) {
            if !self.config.is_allowed_sender(&sender) {
                tracing::info!(%sender, "command sent by a sender who is not allowed");
                return Ok(());
            }
            return self.run_command(&room, command).await;
        }

        let reply_parent = self.reply_parent(&room, &event).await;
        if !self.should_answer(&event, &body, &sender, reply_parent.as_ref()) {
            return Ok(());
        }

        self.answer(
            room,
            room_id,
            sender,
            body,
            attached,
            reply_parent.map(|(_, body)| body),
        )
        .await
    }

    async fn run_command(&self, room: &Room, command: Command<'_>) -> Result<()> {
        match command {
            Command::Help => self.send_text(room, HELP).await,
            Command::Model(None) => {
                self.send_text(room, &format!("Current model: {}", self.agent.llm.model()))
                    .await
            }
            Command::Model(Some(model)) => {
                let previous = self.agent.llm.switch_model(model);
                tracing::info!(from = %previous, to = %model, "chat model switched");
                self.send_text(room, &format!("Switched model from {previous} to {model}"))
                    .await
            }
            Command::Reasoning(None) => {
                self.send_text(
                    room,
                    &format!(
                        "Current reasoning effort: {}",
                        self.agent.llm.reasoning_effort()
                    ),
                )
                .await
            }
            Command::Reasoning(Some(effort)) => {
                let previous = self.agent.llm.switch_reasoning_effort(effort);
                tracing::info!(from = %previous, to = %effort, "reasoning effort switched");
                self.send_text(
                    room,
                    &format!("Switched reasoning effort from {previous} to {effort}"),
                )
                .await
            }
        }
    }

    fn remember(&self, event_id: &str, room_id: &str, sender: &str, body: &str) {
        self.buffers.push(
            room_id,
            Turn {
                sender: sender.to_string(),
                body: body.to_string(),
            },
        );

        let at = chrono::Utc::now().to_rfc3339();
        let conn = self.db.lock().unwrap();
        if let Err(e) = crate::messages::record(&conn, event_id, room_id, sender, body, &at) {
            tracing::warn!(error = %e, "failed archiving message");
        }
    }

    // The sync handler only ever sees other people: the SDK drops the bot's own
    // events before the callback. Anything merlin says has to be remembered on
    // the way out or it is gone, leaving him reading a transcript of a
    // conversation he appears to take no part in.
    fn remember_own(&self, room: &Room, event_id: &str, body: &str) {
        self.remember(
            event_id,
            room.room_id().as_str(),
            &self.config.user_id,
            body,
        );
    }

    fn should_answer(
        &self,
        event: &OriginalSyncRoomMessageEvent,
        body: &str,
        sender: &str,
        reply_parent: Option<&(String, String)>,
    ) -> bool {
        let mentions: Vec<String> = event
            .content
            .mentions
            .as_ref()
            .map(|m| m.user_ids.iter().map(|u| u.to_string()).collect())
            .unwrap_or_default();

        let is_reply_to_bot = reply_parent.is_some_and(|(from, _)| from == &self.config.user_id);

        if !is_addressed(
            body,
            &mentions,
            &self.config.user_id,
            self.config.localpart(),
            &self.config.display_name,
            is_reply_to_bot,
        ) {
            tracing::debug!(%sender, "not addressed; buffered only");
            return false;
        }

        if !self.config.is_allowed_sender(sender) {
            tracing::info!(%sender, "addressed by a sender who is not allowed");
            return false;
        }
        true
    }

    async fn answer(
        self: &Arc<Self>,
        room: Room,
        room_id: String,
        sender: String,
        body: String,
        attached: Option<Attached>,
        reply_parent: Option<String>,
    ) -> Result<()> {
        let ambient = self.buffers.render(&room_id, true);

        tracing::info!(%sender, chars = body.len(), "turn started");
        let started = std::time::Instant::now();

        let typing = Typing::start(room.clone());

        let mut body = body;
        let attachments = match attached {
            Some(file) => self.receive(&room, file, &mut body).await,
            None => Vec::new(),
        };

        let (progress, mut updates) = tokio::sync::mpsc::unbounded_channel::<String>();
        let pump = {
            let bot = Arc::clone(self);
            let room = room.clone();
            tokio::spawn(async move {
                while let Some(text) = updates.recv().await {
                    if let Err(e) = bot.send_text(&room, &text).await {
                        tracing::warn!(error = %e, "failed sending an intermediate message");
                    }
                    // Clients drop the indicator when a message from the sender
                    // arrives, and re-asserting `true` alone would change
                    // nothing server-side and so produce no event to wake them
                    // back up. The retraction is what makes the next notice a
                    // change worth broadcasting.
                    let _ = set_typing(&room, false).await;
                    let _ = set_typing(&room, true).await;
                }
            })
        };

        let result = self
            .agent
            .turn(
                Incoming {
                    room_id: &room_id,
                    sender: &sender,
                    body: &body,
                    ambient,
                    reply_parent,
                    attachments,
                },
                Some(&progress),
            )
            .await;

        drop(progress);
        let _ = pump.await;
        let ms = started.elapsed().as_millis() as u64;
        let sent = match result {
            Ok(turn) => {
                tracing::info!(
                    ms,
                    reply_chars = turn.text.len(),
                    images = turn.images.len(),
                    prompt_tokens = turn.prompt_tokens,
                    completion_tokens = turn.completion_tokens,
                    tools = %if turn.tools_used.is_empty() {
                        "none".to_string()
                    } else {
                        turn.tools_used.join(",")
                    },
                    "turn finished"
                );
                for image in turn.images {
                    if let Err(e) = self.send_image(&room, image).await {
                        tracing::warn!(error = %e, "failed sending image");
                    }
                }
                if turn.text.is_empty() {
                    Ok(())
                } else {
                    self.send_text(&room, &turn.text).await
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, ms, "turn failed");
                self.send_text(&room, &format!("that failed: {e}")).await
            }
        };

        typing.finish().await;
        sent
    }

    pub async fn post(&self, room_id: &str, text: &str) -> Result<()> {
        let room = self.resolve_room(room_id)?;
        self.send_text(&room, text).await
    }

    pub async fn post_image(&self, room_id: &str, image: crate::agent::Image) -> Result<()> {
        let room = self.resolve_room(room_id)?;
        self.send_image(&room, image).await
    }

    fn resolve_room(&self, room_id: &str) -> Result<Room> {
        let parsed = mxlink::matrix_sdk::ruma::RoomId::parse(room_id)
            .map_err(|e| anyhow::anyhow!("invalid room id '{room_id}': {e}"))?;
        self.link
            .client()
            .get_room(&parsed)
            .with_context(|| format!("not joined to room {room_id}"))
    }

    async fn receive(&self, room: &Room, file: Attached, body: &mut String) -> Vec<Attachment> {
        let request = MediaRequestParameters {
            source: file.source,
            format: MediaFormat::File,
        };

        let bytes = match room
            .client()
            .media()
            .get_media_content(&request, true)
            .await
        {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::warn!(error = %e, "could not fetch attachment");
                return Vec::new();
            }
        };

        if bytes.len() > self.config.limits.max_response_bytes {
            tracing::warn!(bytes = bytes.len(), "attachment over the size cap");
            body.push_str(&format!(" (too large to save, {} bytes)", bytes.len()));
            return Vec::new();
        }

        match self.workspace.save_incoming(&file.name, &bytes) {
            Ok(path) => {
                tracing::info!(%path, bytes = bytes.len(), viewable = file.viewable, "attachment saved");
                body.push_str(&format!(" (saved at {path})"));
            }
            Err(e) => tracing::warn!(error = %e, "could not save attachment"),
        }

        if !file.viewable {
            return Vec::new();
        }
        vec![Attachment {
            bytes,
            media_type: file.media_type.unwrap_or_else(|| "image/png".to_string()),
        }]
    }

    async fn reply_parent(
        &self,
        room: &Room,
        event: &OriginalSyncRoomMessageEvent,
    ) -> Option<(String, String)> {
        let Some(Relation::Reply(reply)) = &event.content.relates_to else {
            return None;
        };

        let parent = room.event(&reply.in_reply_to.event_id, None).await.ok()?;
        let raw = parent.raw().deserialize().ok()?;

        use mxlink::matrix_sdk::ruma::events::AnySyncTimelineEvent;
        let AnySyncTimelineEvent::MessageLike(message) = raw else {
            return None;
        };
        let sender = message.sender().to_string();
        let body = message.original_content().and_then(|c| match c {
            mxlink::matrix_sdk::ruma::events::AnyMessageLikeEventContent::RoomMessage(m) => {
                Some(m.body().to_string())
            }
            _ => None,
        })?;
        Some((sender, body))
    }

    async fn send_text(&self, room: &Room, text: &str) -> Result<()> {
        let mut content = RoomMessageEventContent::text_plain(text);
        let sent = self
            .link
            .messaging()
            .send_event(room, &mut content, MessageResponseType::InRoom)
            .await
            .map_err(|e| anyhow::anyhow!("sending message failed: {e:?}"))?;
        self.remember_own(room, sent.event_id.as_str(), text);
        Ok(())
    }

    async fn send_image(&self, room: &Room, image: crate::agent::Image) -> Result<()> {
        let mime: mxlink::mime::Mime = image.media_type.parse().unwrap_or(mxlink::mime::IMAGE_PNG);
        let caption = image.caption.clone();

        let mut content = self
            .link
            .media()
            .upload_and_prepare_event_content(room, &mime, image.bytes, &image.caption)
            .await
            .map_err(|e| anyhow::anyhow!("uploading image failed: {e:?}"))?;

        let sent = self
            .link
            .messaging()
            .send_event(room, &mut content, MessageResponseType::InRoom)
            .await
            .map_err(|e| anyhow::anyhow!("sending image failed: {e:?}"))?;
        // Same shape an incoming attachment is archived under, so the
        // transcript reads consistently whoever sent the picture.
        self.remember_own(room, sent.event_id.as_str(), &format!("[file] {caption}"));
        Ok(())
    }
}

// A typing notice lapses unless it is refreshed, and it has to be retracted
// exactly once, after the last refresh has landed. Aborting the refresh task
// and retracting from the outside raced: a `true` already in flight could
// arrive after the `false` and leave merlin typing until the server's timeout
// ran out. That is worse than it sounds — the next turn's notice then changes
// nothing server-side, so no event is broadcast, and a client that has since
// hidden the indicator is never told to show it again. Owning the retraction
// here keeps it ordered behind every refresh.
struct Typing {
    stop: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

impl Typing {
    fn start(room: Room) -> Self {
        let (stop, mut halt) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            loop {
                if let Err(e) = set_typing(&room, true).await {
                    tracing::warn!(error = %e, "could not send typing notice");
                }
                tokio::select! {
                    _ = tokio::time::sleep(TYPING_REFRESH) => {}
                    _ = &mut halt => break,
                }
            }
            if let Err(e) = set_typing(&room, false).await {
                tracing::warn!(error = %e, "could not retract typing notice");
            }
        });
        Self { stop, task }
    }

    async fn finish(self) {
        let _ = self.stop.send(());
        let _ = self.task.await;
    }
}

async fn set_typing(room: &Room, typing: bool) -> Result<()> {
    let state = if typing {
        create_typing_event::v3::Typing::Yes(create_typing_event::v3::TypingInfo::new(
            TYPING_TIMEOUT,
        ))
    } else {
        create_typing_event::v3::Typing::No
    };
    let request = create_typing_event::v3::Request::new(
        room.own_user_id().to_owned(),
        room.room_id().to_owned(),
        state,
    );
    room.client()
        .send(request)
        .await
        .context("sending a typing notice")?;
    Ok(())
}
