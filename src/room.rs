use std::collections::HashMap;
use std::sync::Mutex;

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

    pub fn render(&self, room_id: &str, skip_last: bool) -> Option<String> {
        let map = self.inner.lock().unwrap();
        let turns = map.get(room_id)?;
        let keep = turns.len().saturating_sub(usize::from(skip_last));
        if keep == 0 {
            return None;
        }
        Some(
            turns[..keep]
                .iter()
                .map(|t| format!("{}: {}", t.sender, t.body))
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }
}

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

    if m_mentions.iter().any(|id| id == user_id) {
        return true;
    }
    let haystack = body.to_lowercase();
    if haystack.contains(&user_id.to_lowercase()) {
        return true;
    }

    contains_word(&haystack, &localpart.to_lowercase())
        || (!display_name.is_empty() && contains_word(&haystack, &display_name.to_lowercase()))
}

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
