//! merlin, a Matrix assistant.

pub mod agent;
pub mod backfill;
pub mod config;
pub mod cron;
pub mod db;
pub mod dream;
pub mod embed;
pub mod exec;
pub mod image;
pub mod llm;
pub mod matrix;
pub mod memory;
pub mod messages;
pub mod query;
pub mod room;
pub mod scheduler;
pub mod tools;
pub mod workspace;

/// Cut a string to a byte budget on a character boundary.
/// Shared because both tool output and sandbox output need it and neither owns it.
pub fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}
