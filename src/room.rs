//! Addressing rules and the ambient context buffer.
//!
//! Pure logic,
//! deliberately separate from the Matrix plumbing so it can be tested without a homeserver — this is the part that decides whether a message costs money.

use std::collections::HashMap;
use std::sync::Mutex;

/// One buffered message.
/// Never persisted: the ring dies with the process,
/// so ambient conversation does not silently become permanently searchable.
#[derive(Debug, Clone)]
pub struct Turn {
    pub sender: String,
    pub body: String,
}

pub struct Buffers {
    inner: Mutex<HashMap<String, Vec<Turn>>>,
    capacity: usize,
}

impl Buffers {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            capacity: capacity.max(1),
        }
    }

    pub fn push(&self, room_id: &str, turn: Turn) {
        let mut map = self.inner.lock().unwrap();
        let ring = map.entry(room_id.to_string()).or_default();
        ring.push(turn);
        if ring.len() > self.capacity {
            let overflow = ring.len() - self.capacity;
            ring.drain(0..overflow);
        }
    }

    /// Everything buffered for a room,
    /// oldest first,
    /// excluding the message currently being answered (which the caller passes separately).
    pub fn context(&self, room_id: &str) -> Vec<Turn> {
        self.inner
            .lock()
            .unwrap()
            .get(room_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Ambient context as prompt text.
    /// `skip_last` drops the message currently being answered,
    /// which the caller passes to the model separately.
    pub fn render(&self, room_id: &str, skip_last: bool) -> Option<String> {
        let mut turns = self.context(room_id);
        if skip_last {
            turns.pop();
        }
        if turns.is_empty() {
            return None;
        }
        Some(
            turns
                .iter()
                .map(|t| format!("{}: {}", t.sender, t.body))
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }
}

/// Whether a message is addressed to the bot.
///
/// The name check is word-boundary,
/// so an unrelated use of the name in ordinary conversation does not trigger a turn.
pub fn is_addressed(
    body: &str,
    m_mentions: &[String],
    user_id: &str,
    localpart: &str,
    display_name: &str,
    is_reply_to_bot: bool,
) -> bool {
    if is_reply_to_bot {
        return true;
    }

    // An explicit pill is authoritative in both directions: a client that sent m.mentions listed everyone it meant.
    if !m_mentions.is_empty() {
        return m_mentions.iter().any(|id| id == user_id);
    }

    let haystack = body.to_lowercase();
    if haystack.contains(&user_id.to_lowercase()) {
        return true;
    }

    contains_word(&haystack, &localpart.to_lowercase())
        || (!display_name.is_empty() && contains_word(&haystack, &display_name.to_lowercase()))
}

/// Word-boundary containment without pulling a regex per call.
/// A match must not be flanked by alphanumerics,
/// so "merlin" hits in "merlin,
/// hello" and "@merlin" but not in "merlinesque".
fn contains_word(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let bytes = haystack.as_bytes();
    let mut from = 0;
    while let Some(pos) = haystack[from..].find(needle) {
        let start = from + pos;
        let end = start + needle.len();

        let before_ok = start == 0
            || !bytes
                .get(start - 1)
                .is_some_and(|b| b.is_ascii_alphanumeric());
        let after_ok =
            end >= bytes.len() || !bytes.get(end).is_some_and(|b| b.is_ascii_alphanumeric());

        if before_ok && after_ok {
            return true;
        }
        from = end;
        if from >= haystack.len() {
            break;
        }
    }
    false
}
