//! Addressing rules and the ambient context buffer.
//!
//! Pure logic, deliberately separate from the Matrix plumbing so it can be
//! tested without a homeserver — this is the part that decides whether a
//! message costs money.

use std::collections::HashMap;
use std::sync::Mutex;

/// One buffered message. Never persisted: the ring dies with the process, so
/// ambient conversation does not silently become permanently searchable.
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

    /// Everything buffered for a room, oldest first, excluding the message
    /// currently being answered (which the caller passes separately).
    pub fn context(&self, room_id: &str) -> Vec<Turn> {
        self.inner
            .lock()
            .unwrap()
            .get(room_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn render(&self, room_id: &str) -> Option<String> {
        let turns = self.context(room_id);
        if turns.is_empty() {
            return None;
        }
        let body = turns
            .iter()
            .map(|t| format!("{}: {}", t.sender, t.body))
            .collect::<Vec<_>>()
            .join("\n");
        Some(body)
    }
}

/// Whether a message is addressed to the bot.
///
/// The name check is word-boundary, not substring. The previous bot matched any
/// occurrence, so "merlin" the bird or the wizard woke it, and so did
/// "merlin dont respond".
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

    // An explicit pill is authoritative in both directions: a client that sent
    // m.mentions listed everyone it meant.
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

/// Word-boundary containment without pulling a regex per call. A match must not
/// be flanked by alphanumerics, so "merlin" hits in "merlin, hello" and
/// "@merlin" but not in "merlinesque".
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
        let after_ok = end >= bytes.len()
            || !bytes.get(end).is_some_and(|b| b.is_ascii_alphanumeric());

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

#[cfg(test)]
mod tests {
    use super::*;

    const UID: &str = "@merlin:matrix.aza.network";

    fn addressed(body: &str) -> bool {
        is_addressed(body, &[], UID, "merlin", "merlin", false)
    }

    #[test]
    fn plain_name_addresses() {
        assert!(addressed("merlin what day is it"));
        assert!(addressed("hey Merlin, you there?"));
        assert!(addressed("@merlin hello"));
        assert!(addressed("ask @merlin:matrix.aza.network about it"));
    }

    #[test]
    fn substring_does_not_address() {
        // The exact class of false positive the previous bot had.
        assert!(!addressed("merlinesque behaviour"));
        assert!(!addressed("submerlin"));
    }

    #[test]
    fn unrelated_chat_is_ignored() {
        assert!(!addressed("what do you reckon about the game"));
        assert!(!addressed(""));
    }

    #[test]
    fn explicit_mentions_are_authoritative() {
        // Listed: addressed even though the body never names it.
        assert!(is_addressed(
            "can you look at this",
            &[UID.to_string()],
            UID,
            "merlin",
            "merlin",
            false
        ));
        // Listed someone else: not addressed, even though the body says merlin.
        assert!(!is_addressed(
            "merlin is a bird",
            &["@jakob:sadairs.com".to_string()],
            UID,
            "merlin",
            "merlin",
            false
        ));
    }

    #[test]
    fn reply_to_bot_addresses_without_a_name() {
        assert!(is_addressed("what did you mean", &[], UID, "merlin", "merlin", true));
    }

    #[test]
    fn ring_buffer_evicts_oldest() {
        let b = Buffers::new(3);
        for i in 0..5 {
            b.push("!r", Turn { sender: "@a".into(), body: i.to_string() });
        }
        let ctx = b.context("!r");
        assert_eq!(ctx.len(), 3);
        assert_eq!(ctx[0].body, "2");
        assert_eq!(ctx[2].body, "4");
    }

    #[test]
    fn buffers_are_per_room() {
        let b = Buffers::new(5);
        b.push("!a", Turn { sender: "@x".into(), body: "one".into() });
        b.push("!b", Turn { sender: "@y".into(), body: "two".into() });
        assert_eq!(b.context("!a").len(), 1);
        assert_eq!(b.context("!b")[0].body, "two");
        assert!(b.render("!missing").is_none());
    }

    #[test]
    fn render_labels_each_line_with_its_sender() {
        let b = Buffers::new(5);
        b.push("!r", Turn { sender: "@aiden".into(), body: "hi".into() });
        b.push("!r", Turn { sender: "@jakob".into(), body: "yo".into() });
        assert_eq!(b.render("!r").unwrap(), "@aiden: hi\n@jakob: yo");
    }
}
