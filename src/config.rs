//! Configuration and secrets.
//!
//! The TOML file is world-readable and lives in the Nix store or the state
//! directory; every credential comes from the environment instead, so nothing
//! ever renders a resolved config containing secrets to disk.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub homeserver: String,
    pub user_id: String,
    pub display_name: String,

    /// Canonical room IDs (`!abc:server`). Empty means no rooms, not all of
    /// them: an omitted allowlist must never be a grant.
    #[serde(default)]
    pub allowed_rooms: Vec<String>,

    /// MXIDs permitted to trigger a turn. Everyone else is still buffered as
    /// ambient context, they just cannot address the bot.
    #[serde(default)]
    pub allowed_senders: Vec<String>,

    /// Ambient messages retained per room, in memory only.
    #[serde(default = "default_context_window")]
    pub context_window: usize,

    #[serde(default = "default_timezone")]
    pub timezone: String,

    #[serde(default)]
    pub model: ModelConfig,

    #[serde(default)]
    pub limits: Limits,

    /// Where session, memory and cron state live.
    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelConfig {
    #[serde(default = "default_chat_model")]
    pub chat: String,
    #[serde(default = "default_image_model")]
    pub image: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Limits {
    #[serde(default = "default_max_response_bytes")]
    pub max_response_bytes: usize,
    #[serde(default = "default_tool_iterations")]
    pub tool_iterations: usize,
    #[serde(default = "default_request_timeout_s")]
    pub request_timeout_s: u64,
    #[serde(default = "default_exec_timeout_s")]
    pub exec_timeout_s: u64,
    #[serde(default = "default_exec_memory_max")]
    pub exec_memory_max: String,
}

/// Credentials, read from the environment only.
#[derive(Clone)]
pub struct Secrets {
    pub matrix_password: String,
    /// Recovery passphrase for server-side key backup. Without it a wiped
    /// crypto store cannot restore room keys and old messages stay unreadable.
    pub matrix_recovery_passphrase: Option<String>,
    /// Encrypts the persisted session blob at rest.
    pub session_encryption_key: String,
    pub openrouter_api_key: String,
    pub exa_api_key: Option<String>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config at {}", path.display()))?;
        let mut config: Config = toml::from_str(&raw).context("parsing config TOML")?;
        config.apply_env_overrides();

        if config.allowed_rooms.is_empty() {
            anyhow::bail!("allowed_rooms is empty; the bot would join nothing");
        }
        if config.allowed_senders.is_empty() {
            anyhow::bail!("allowed_senders is empty; nobody could address the bot");
        }
        Ok(config)
    }

    /// Identifiers can come from the environment instead of the file. The
    /// config is rendered into the world-readable Nix store from a public
    /// repository, and a private room's id does not belong there even though it
    /// is not a credential.
    pub fn apply_env_overrides(&mut self) {
        if let Some(rooms) = list_from_env("MERLIN_ALLOWED_ROOMS") {
            self.allowed_rooms = rooms;
        }
        if let Some(senders) = list_from_env("MERLIN_ALLOWED_SENDERS") {
            self.allowed_senders = senders;
        }
    }

    pub fn localpart(&self) -> &str {
        self.user_id
            .trim_start_matches('@')
            .split(':')
            .next()
            .unwrap_or(&self.user_id)
    }

    pub fn tz(&self) -> chrono_tz::Tz {
        self.timezone
            .parse()
            .unwrap_or(chrono_tz::Australia::Sydney)
    }

    pub fn is_allowed_room(&self, room_id: &str) -> bool {
        self.allowed_rooms.iter().any(|r| r == room_id)
    }

    pub fn is_allowed_sender(&self, sender: &str) -> bool {
        self.allowed_senders.iter().any(|s| s == sender)
    }
}

impl Secrets {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            matrix_password: req("MATRIX_PASSWORD")?,
            matrix_recovery_passphrase: opt("MATRIX_RECOVERY_PASSPHRASE"),
            // Derived from the Matrix password when unset so a fresh deploy
            // works without inventing another secret to manage.
            session_encryption_key: opt("SESSION_ENCRYPTION_KEY")
                .unwrap_or_else(|| req("MATRIX_PASSWORD").unwrap_or_default()),
            openrouter_api_key: req("OPENROUTER_API_KEY")?,
            exa_api_key: opt("EXA_API_KEY"),
        })
    }
}

fn req(key: &str) -> Result<String> {
    std::env::var(key)
        .with_context(|| format!("{key} must be set in the environment"))
        .map(|v| v.trim().to_string())
        .and_then(|v| {
            if v.is_empty() {
                anyhow::bail!("{key} is set but empty")
            } else {
                Ok(v)
            }
        })
}

/// Comma-separated env list, empty entries dropped.
fn list_from_env(key: &str) -> Option<Vec<String>> {
    let raw = std::env::var(key).ok()?;
    let items: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if items.is_empty() { None } else { Some(items) }
}

fn opt(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            chat: default_chat_model(),
            image: default_image_model(),
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_response_bytes: default_max_response_bytes(),
            tool_iterations: default_tool_iterations(),
            request_timeout_s: default_request_timeout_s(),
            exec_timeout_s: default_exec_timeout_s(),
            exec_memory_max: default_exec_memory_max(),
        }
    }
}

fn default_context_window() -> usize { 40 }
fn default_timezone() -> String { "Australia/Sydney".into() }
fn default_chat_model() -> String { "z-ai/glm-5.3-flash".into() }
fn default_image_model() -> String { "meta/muse-image".into() }
fn default_max_response_bytes() -> usize { 8 * 1024 * 1024 }
fn default_tool_iterations() -> usize { 6 }
fn default_request_timeout_s() -> u64 { 60 }
fn default_exec_timeout_s() -> u64 { 60 }
fn default_exec_memory_max() -> String { "1G".into() }
fn default_state_dir() -> PathBuf { PathBuf::from("/var/lib/merlin") }

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(user_id: &str) -> Config {
        Config {
            homeserver: "https://example.org".into(),
            user_id: user_id.into(),
            display_name: "merlin".into(),
            allowed_rooms: vec!["!a:example.org".into()],
            allowed_senders: vec!["@aiden:example.org".into()],
            context_window: 40,
            timezone: "Australia/Sydney".into(),
            model: ModelConfig::default(),
            limits: Limits::default(),
            state_dir: "/tmp".into(),
        }
    }

    #[test]
    fn localpart_strips_sigil_and_server() {
        assert_eq!(cfg("@merlin:matrix.aza.network").localpart(), "merlin");
    }

    #[test]
    fn env_overrides_replace_config_lists() {
        // SAFETY: single-threaded test process, no other reader of this var.
        unsafe {
            std::env::set_var("MERLIN_ALLOWED_ROOMS", " !x:example.org , ,!y:example.org ");
        }
        let mut c = cfg("@merlin:example.org");
        c.apply_env_overrides();
        assert_eq!(c.allowed_rooms, vec!["!x:example.org", "!y:example.org"]);
        unsafe { std::env::remove_var("MERLIN_ALLOWED_ROOMS"); }
    }

    #[test]
    fn allowlists_are_exact() {
        let c = cfg("@merlin:example.org");
        assert!(c.is_allowed_room("!a:example.org"));
        assert!(!c.is_allowed_room("!b:example.org"));
        assert!(c.is_allowed_sender("@aiden:example.org"));
        assert!(!c.is_allowed_sender("@mallory:example.org"));
    }
}
