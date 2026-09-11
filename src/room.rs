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

    pub fn context(&self, room_id: &str) -> Vec<Turn> {
        self.inner
            .lock()
            .unwrap()
            .get(room_id)
            .cloned()
            .unwrap_or_default()
    }

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
