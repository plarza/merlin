//! Matrix wiring.
//!
//! mxlink owns login, session persistence, key backup and cross-signing.
//! What is left here is deciding which messages deserve a turn, and sending replies as plain text.

use anyhow::{Context, Result};
use std::sync::Arc;

use mxlink::matrix_sdk::Room;
use mxlink::matrix_sdk::media::{MediaFormat, MediaRequestParameters};
use mxlink::matrix_sdk::ruma::events::room::message::{
    MessageType, OriginalSyncRoomMessageEvent, Relation, RoomMessageEventContent,
};
use mxlink::{
    CallbackError, InitConfig, LoginConfig, LoginCredentials, LoginEncryption, MatrixLink,
    MessageResponseType, PersistenceConfig,
};

use crate::agent::{Agent, Incoming};
use crate::config::{Config, Secrets};
use crate::llm::Attachment;
use crate::room::{Buffers, Turn, is_addressed};

/// A file that arrived with a message.
struct Attached {
    source: mxlink::matrix_sdk::ruma::events::room::MediaSource,
    name: String,
    media_type: Option<String>,
    /// Whether the model can look at it, which in practice means an image.
    viewable: bool,
}

/// The body to record, and the file to fetch if a turn runs.
///
/// Every attachment takes the same path, because an image is a file too: all of them land in the workspace,
/// and an image is additionally handed to the model, which is the one thing it can do with bytes directly.
/// `None` means a message type the bot does not handle at all.
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
        // Only an image carries its media type onward, since only an image is sent as bytes.
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

    // Without a recovery passphrase a wiped crypto store cannot restore room keys, and previously readable messages become permanently undecryptable.
    let encryption = LoginEncryption::new(secrets.matrix_recovery_passphrase.clone(), false);

    let login = LoginConfig::new(
        config.homeserver.clone(),
        credentials,
        Some(encryption),
        config.display_name.clone(),
    );

    // mxlink wants exactly 32 bytes; derive them from the configured key material so no separate 64-hex secret has to be provisioned and rotated.
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
        self.remember(&event, &room_id, &sender, &body);

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

    /// Buffer and archive every message, whether or not it was addressed to the bot.
    /// Ambient context is the point: addressing decides only whether a turn runs.
    fn remember(
        &self,
        event: &OriginalSyncRoomMessageEvent,
        room_id: &str,
        sender: &str,
        body: &str,
    ) {
        self.buffers.push(
            room_id,
            Turn {
                sender: sender.to_string(),
                body: body.to_string(),
            },
        );

        let at = chrono::Utc::now().to_rfc3339();
        let conn = self.db.lock().unwrap();
        if let Err(e) =
            crate::messages::record(&conn, event.event_id.as_str(), room_id, sender, body, &at)
        {
            tracing::warn!(error = %e, "failed archiving message");
        }
    }

    /// Whether this message should start a turn.
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

        // Addressed, but by someone who may not drive the bot.
        // Their message still counts as context, they just cannot start a turn.
        if !self.config.is_allowed_sender(sender) {
            tracing::info!(%sender, "addressed by a sender who is not allowed");
            return false;
        }
        true
    }

    /// Run one turn and report the result to the room.
    async fn answer(
        self: &Arc<Self>,
        room: Room,
        room_id: String,
        sender: String,
        body: String,
        attached: Option<Attached>,
        reply_parent: Option<String>,
    ) -> Result<()> {
        // The buffer already contains this message; the turn passes it separately, so drop the last entry from the ambient block.
        let ambient = self.buffers.render(&room_id, true);

        tracing::info!(%sender, chars = body.len(), "turn started");
        let started = std::time::Instant::now();

        // Fetched only for a turn that will actually run, so an attachment nobody asked about costs nothing.
        let mut body = body;
        let attachments = match attached {
            Some(file) => self.receive(&room, file, &mut body).await,
            None => Vec::new(),
        };

        if room.typing_notice(true).await.is_err() {
            tracing::debug!("could not send typing notice");
        }

        // Intermediate messages are posted as they arrive rather than collected,
        // so a long task reads as progress instead of a minute of silence.
        let (progress, mut updates) = tokio::sync::mpsc::unbounded_channel::<String>();
        let pump = {
            let bot = Arc::clone(self);
            let room = room.clone();
            tokio::spawn(async move {
                while let Some(text) = updates.recv().await {
                    if let Err(e) = bot.send_text(&room, &text).await {
                        tracing::warn!(error = %e, "failed sending an intermediate message");
                    }
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

        // Closing the channel and waiting for the pump guarantees every update has landed before the final answer follows it.
        drop(progress);
        let _ = pump.await;
        let _ = room.typing_notice(false).await;

        let ms = started.elapsed().as_millis() as u64;
        match result {
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
                if !turn.text.is_empty() {
                    self.send_text(&room, &turn.text).await?;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, ms, "turn failed");
                self.send_text(&room, &format!("that failed: {e}")).await?;
            }
        }
        Ok(())
    }

    /// Resolve a room by id and send plain text.
    /// Used by scheduled jobs, which have a room id rather than a live Room handle.
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

    /// Download an attachment once, keep it in the workspace, and hand back
    /// anything the model can look at directly.
    ///
    /// The saved path is appended to the message body, so the agent knows the file is there and can open it with the shell if it decides the contents matter.
    /// Nothing is parsed here: a PDF costs nothing until it is read.
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

    /// Sender and body of the message being replied to, when there is one.
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

    /// Plain text with no formatted_body, so nothing renders as markdown.
    async fn send_text(&self, room: &Room, text: &str) -> Result<()> {
        let mut content = RoomMessageEventContent::text_plain(text);
        self.link
            .messaging()
            .send_event(room, &mut content, MessageResponseType::InRoom)
            .await
            .map_err(|e| anyhow::anyhow!("sending message failed: {e:?}"))?;
        Ok(())
    }

    async fn send_image(&self, room: &Room, image: crate::agent::Image) -> Result<()> {
        let mime: mxlink::mime::Mime = image.media_type.parse().unwrap_or(mxlink::mime::IMAGE_PNG);

        let mut content = self
            .link
            .media()
            .upload_and_prepare_event_content(room, &mime, image.bytes, &image.caption)
            .await
            .map_err(|e| anyhow::anyhow!("uploading image failed: {e:?}"))?;

        self.link
            .messaging()
            .send_event(room, &mut content, MessageResponseType::InRoom)
            .await
            .map_err(|e| anyhow::anyhow!("sending image failed: {e:?}"))?;
        Ok(())
    }
}
