use anyhow::{Context, Result};
use mxlink::MatrixLink;
use mxlink::matrix_sdk::room::MessagesOptions;
use mxlink::matrix_sdk::ruma::events::{AnyMessageLikeEventContent, AnySyncTimelineEvent};
use mxlink::matrix_sdk::ruma::{RoomId, UInt};

use crate::config::Config;
use crate::{db::RoomDbs, messages};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub pages: usize,
    pub seen: usize,
    pub archived: usize,
    pub undecryptable: usize,
    pub not_text: usize,
}

impl std::fmt::Display for Stats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} pages, {} events: {} archived, {} undecryptable, {} not text",
            self.pages, self.seen, self.archived, self.undecryptable, self.not_text
        )
    }
}

pub async fn run(
    link: &MatrixLink,
    config: &Config,
    dbs: &RoomDbs,
    max_pages: usize,
) -> Result<Stats> {
    let client = link.client();
    let mut total = Stats::default();

    for room_id in &config.allowed_rooms {
        let db = dbs.get(room_id)?;
        let parsed = RoomId::parse(room_id)
            .map_err(|e| anyhow::anyhow!("invalid room id '{room_id}': {e}"))?;
        let Some(room) = client.get_room(&parsed) else {
            tracing::warn!(%room_id, "not joined; skipping");
            continue;
        };

        let mut from: Option<String> = None;

        for page in 0..max_pages {
            let mut options = MessagesOptions::backward();
            options.limit = UInt::new(100).unwrap_or(UInt::MAX);
            options.from = from.clone();

            let batch = room
                .messages(options)
                .await
                .with_context(|| format!("paging {room_id} (page {page})"))?;

            total.pages += 1;
            if batch.chunk.is_empty() {
                break;
            }

            for event in &batch.chunk {
                total.seen += 1;
                match extract(event) {
                    Extracted::Text {
                        event_id,
                        sender,
                        body,
                        at,
                    } => {
                        let conn = db.lock().unwrap();
                        match messages::record(&conn, &event_id, room_id, &sender, &body, &at) {
                            Ok(()) => total.archived += 1,
                            Err(e) => tracing::warn!(error = %e, "failed archiving"),
                        }
                    }
                    Extracted::Undecryptable => total.undecryptable += 1,
                    Extracted::Other => total.not_text += 1,
                }
            }

            match batch.end {
                None => break,
                Some(end) => from = Some(end),
            }
        }
    }

    Ok(total)
}

enum Extracted {
    Text {
        event_id: String,
        sender: String,
        body: String,
        at: String,
    },
    Undecryptable,
    Other,
}

fn extract(event: &mxlink::matrix_sdk::deserialized_responses::TimelineEvent) -> Extracted {
    let Ok(parsed) = event.raw().deserialize() else {
        return Extracted::Other;
    };

    let AnySyncTimelineEvent::MessageLike(message) = parsed else {
        return Extracted::Other;
    };

    if matches!(
        message,
        mxlink::matrix_sdk::ruma::events::AnySyncMessageLikeEvent::RoomEncrypted(_)
    ) {
        return Extracted::Undecryptable;
    }

    let Some(AnyMessageLikeEventContent::RoomMessage(content)) = message.original_content() else {
        return Extracted::Other;
    };

    let body = content.body().trim().to_string();
    if body.is_empty() {
        return Extracted::Other;
    }

    let at = chrono::DateTime::from_timestamp_millis(i64::from(message.origin_server_ts().get()))
        .map(|t| t.to_rfc3339())
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());

    Extracted::Text {
        event_id: message.event_id().to_string(),
        sender: message.sender().to_string(),
        body,
        at,
    }
}
