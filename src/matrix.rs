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

pub struct Bot {
    pub link: MatrixLink,
    pub agent: Arc<Agent>,
    pub buffers: Arc<Buffers>,
    pub archive: Arc<std::sync::Mutex<crate::messages::Archive>>,
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

    async fn on_message(&self, event: OriginalSyncRoomMessageEvent, room: Room) -> Result<()> {
        let room_id = room.room_id().to_string();
        if !self.config.is_allowed_room(&room_id) {
            return Ok(());
        }

        // An image carries its filename or caption as the body, which is what a
        // person sees, so it reads sensibly in the archive and the buffer too.
        let (body, image) = match &event.content.msgtype {
            MessageType::Text(text) => (text.body.trim().to_string(), None),
            MessageType::Image(image) => (
                format!("[image] {}", image.body.trim()),
                Some((
                    image.source.clone(),
                    image.info.as_ref().and_then(|i| i.mimetype.clone()),
                )),
            ),
            // Other attachment types are noted but not fetched: the model
            // cannot read them, and the note is enough for it to respond.
            MessageType::File(file) => (format!("[file] {}", file.body.trim()), None),
            MessageType::Video(video) => (format!("[video] {}", video.body.trim()), None),
            MessageType::Audio(audio) => (format!("[audio] {}", audio.body.trim()), None),
            _ => return Ok(()),
        };
        if body.is_empty() {
            return Ok(());
        }

        let sender = event.sender.to_string();

        // Buffer first, unconditionally.
        // Ambient context is the point: every message is retained, and only addressing decides whether a turn runs.
        self.buffers.push(
            &room_id,
            Turn {
                sender: sender.clone(),
                body: body.clone(),
            },
        );

        // Archived unconditionally, so history is searchable whether or not the bot was addressed.
        {
            let at = chrono::Utc::now().to_rfc3339();
            let archive = self.archive.lock().unwrap();
            if let Err(e) = archive.record(event.event_id.as_str(), &room_id, &sender, &body, &at) {
                tracing::warn!(error = %e, "failed archiving message");
            }
        }

        let mentions: Vec<String> = event
            .content
            .mentions
            .as_ref()
            .map(|m| m.user_ids.iter().map(|u| u.to_string()).collect())
            .unwrap_or_default();

        let reply_parent = self.reply_parent(&room, &event).await;
        let is_reply_to_bot = reply_parent
            .as_ref()
            .is_some_and(|(sender, _)| sender == &self.config.user_id);

        if !is_addressed(
            &body,
            &mentions,
            &self.config.user_id,
            self.config.localpart(),
            &self.config.display_name,
            is_reply_to_bot,
        ) {
            tracing::debug!(%sender, "not addressed; buffered only");
            return Ok(());
        }

        // Addressed, but by someone who may not drive the bot.
        // Their message still counts as context, they just cannot start a turn.
        if !self.config.is_allowed_sender(&sender) {
            tracing::info!(%sender, "addressed by a sender who is not allowed");
            return Ok(());
        }

        // The buffer already contains this message; the turn passes it separately, so drop the last entry from the ambient block.
        let ambient = self.buffers.render(&room_id, true);

        tracing::info!(%sender, chars = body.len(), "turn started");
        let started = std::time::Instant::now();

        // Fetched only for a turn that will actually run, so ambient images cost nothing.
        let attachments = match image {
            Some((source, mimetype)) => self.fetch_image(&room, source, mimetype).await,
            None => Vec::new(),
        };

        let typing = room.typing_notice(true).await;
        if typing.is_err() {
            tracing::debug!("could not send typing notice");
        }

        let result = self
            .agent
            .turn(Incoming {
                room_id: &room_id,
                sender: &sender,
                body: &body,
                ambient,
                reply_parent: reply_parent.map(|(_, body)| body),
                attachments,
            })
            .await;

        let _ = room.typing_notice(false).await;

        match result {
            Ok(turn) => {
                tracing::info!(
                    ms = started.elapsed().as_millis() as u64,
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
                tracing::warn!(
                    error = %e,
                    ms = started.elapsed().as_millis() as u64,
                    "turn failed"
                );
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

    /// Download and decrypt an image so the model can look at it.
    /// Oversized images are skipped rather than truncated, since a partial image is worse than none.
    async fn fetch_image(
        &self,
        room: &Room,
        source: mxlink::matrix_sdk::ruma::events::room::MediaSource,
        mimetype: Option<String>,
    ) -> Vec<Attachment> {
        const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;

        let request = MediaRequestParameters {
            source,
            format: MediaFormat::File,
        };

        match room
            .client()
            .media()
            .get_media_content(&request, true)
            .await
        {
            Ok(bytes) if bytes.len() <= MAX_IMAGE_BYTES => {
                tracing::info!(bytes = bytes.len(), "attachment fetched");
                vec![Attachment {
                    bytes,
                    media_type: mimetype.unwrap_or_else(|| "image/png".to_string()),
                }]
            }
            Ok(bytes) => {
                tracing::warn!(bytes = bytes.len(), "attachment over size cap, skipping");
                Vec::new()
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not fetch attachment");
                Vec::new()
            }
        }
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
