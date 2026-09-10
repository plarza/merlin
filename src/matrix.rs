//! Matrix wiring.
//!
//! mxlink owns login, session persistence, key backup and cross-signing. What
//! is left here is deciding which messages deserve a turn, and sending replies
//! as plain text.

use anyhow::{Context, Result};
use std::sync::Arc;

use mxlink::matrix_sdk::Room;
use mxlink::matrix_sdk::ruma::events::room::message::{
    MessageType, OriginalSyncRoomMessageEvent, Relation, RoomMessageEventContent,
};
use mxlink::{
    CallbackError, InitConfig, LoginConfig, LoginCredentials, LoginEncryption, MatrixLink,
    MessageResponseType, PersistenceConfig,
};

use crate::agent::{Agent, Incoming};
use crate::config::{Config, Secrets};
use crate::room::{Buffers, Turn, is_addressed};

pub struct Bot {
    pub link: MatrixLink,
    pub agent: Arc<Agent>,
    pub buffers: Arc<Buffers>,
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

    // Without a recovery passphrase a wiped crypto store cannot restore room
    // keys, and previously readable messages become permanently undecryptable.
    let encryption = LoginEncryption::new(secrets.matrix_recovery_passphrase.clone(), false);

    let login = LoginConfig::new(
        config.homeserver.clone(),
        credentials,
        Some(encryption),
        config.display_name.clone(),
    );

    // mxlink wants exactly 32 bytes; derive them from the configured key
    // material so no separate 64-hex secret has to be provisioned and rotated.
    let key = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(b"merlin.session.v1");
        hasher.update(secrets.session_encryption_key.as_bytes());
        let digest: [u8; 32] = hasher.finalize().into();
        mxlink::helpers::encryption::EncryptionKey::new(digest)
    };

    let persistence = PersistenceConfig::new(
        state.join("session.json"),
        Some(key),
        state.join("matrix"),
    );

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

        let MessageType::Text(text) = &event.content.msgtype else {
            return Ok(());
        };
        let body = text.body.trim().to_string();
        if body.is_empty() {
            return Ok(());
        }

        let sender = event.sender.to_string();

        // Buffer first, unconditionally. Ambient context is the point: every
        // message is retained, and only addressing decides whether a turn runs.
        self.buffers.push(
            &room_id,
            Turn {
                sender: sender.clone(),
                body: body.clone(),
            },
        );

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

        // Addressed, but by someone who may not drive the bot. Their message
        // still counts as context, they just cannot start a turn.
        if !self.config.is_allowed_sender(&sender) {
            tracing::info!(%sender, "addressed by a sender who is not allowed");
            return Ok(());
        }

        // The buffer already contains this message; the turn passes it
        // separately, so drop the last entry from the ambient block.
        let ambient = {
            let mut turns = self.buffers.context(&room_id);
            turns.pop();
            if turns.is_empty() {
                None
            } else {
                Some(
                    turns
                        .iter()
                        .map(|t| format!("{}: {}", t.sender, t.body))
                        .collect::<Vec<_>>()
                        .join("\n"),
                )
            }
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
            })
            .await;

        let _ = room.typing_notice(false).await;

        match result {
            Ok(turn) => {
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
                tracing::warn!(error = %e, "turn failed");
                self.send_text(&room, &format!("that failed: {e}")).await?;
            }
        }

        Ok(())
    }

    /// Resolve a room by id and send plain text. Used by scheduled jobs, which
    /// have a room id rather than a live Room handle.
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
        let body = message
            .original_content()
            .and_then(|c| match c {
                mxlink::matrix_sdk::ruma::events::AnyMessageLikeEventContent::RoomMessage(m) => {
                    Some(m.body().to_string())
                }
                _ => None,
            })?;
        Some((sender, body))
    }

    /// Plain text, no formatted_body: the room should not render markdown the
    /// agent never intended.
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
        let mime: mxlink::mime::Mime = image
            .media_type
            .parse()
            .unwrap_or(mxlink::mime::IMAGE_PNG);

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
