use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub homeserver: String,
    pub user_id: String,
    pub display_name: String,

    #[serde(default)]
    pub allowed_rooms: Vec<String>,

    #[serde(default)]
    pub allowed_senders: Vec<String>,

    #[serde(default)]
    pub admin_senders: Vec<String>,

    #[serde(default)]
    pub untrusted_senders: Vec<String>,

    #[serde(default = "default_context_window")]
    pub context_window: usize,

    #[serde(default = "default_timezone")]
    pub timezone: String,

    #[serde(default)]
    pub model: ModelConfig,

    #[serde(default)]
    pub limits: Limits,

    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelConfig {
    #[serde(default = "default_chat_model")]
    pub chat: String,
    #[serde(default = "default_image_model")]
    pub image: String,
    #[serde(default = "default_image_provider")]
    pub image_provider: String,
    #[serde(default = "default_reasoning_effort")]
    pub reasoning_effort: String,
    #[serde(default = "default_embedding_model")]
    pub embedding: String,
    #[serde(default = "default_embedding_dimensions")]
    pub embedding_dimensions: usize,
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
    #[serde(default = "default_turn_timeout_s")]
    pub turn_timeout_s: u64,
    #[serde(default = "default_exec_memory_max")]
    pub exec_memory_max: String,
    #[serde(default = "default_embed_batch")]
    pub embed_batch: usize,
}

#[derive(Clone)]
pub struct Secrets {
    pub matrix_password: String,
    pub session_encryption_key: String,
    pub openrouter_api_key: String,
    pub exa_api_key: Option<String>,
    pub fal_api_key: Option<String>,
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
        if config.admin_senders.is_empty() {
            anyhow::bail!("admin_senders is empty; nobody could use administrative commands");
        }
        if let Some(admin) = config
            .admin_senders
            .iter()
            .find(|admin| !config.allowed_senders.contains(admin))
        {
            anyhow::bail!("admin sender {admin} is not also an allowed sender");
        }
        if let Some(sender) = config
            .untrusted_senders
            .iter()
            .find(|sender| !config.allowed_senders.contains(sender))
        {
            anyhow::bail!("untrusted sender {sender} is not also an allowed sender");
        }
        if let Some(sender) = config
            .untrusted_senders
            .iter()
            .find(|sender| config.admin_senders.contains(sender))
        {
            anyhow::bail!("sender {sender} cannot be both an admin and untrusted");
        }
        Ok(config)
    }

    pub fn apply_env_overrides(&mut self) {
        if let Some(rooms) = list_from_env("MERLIN_ALLOWED_ROOMS") {
            self.allowed_rooms = rooms;
        }
        if let Some(senders) = list_from_env("MERLIN_ALLOWED_SENDERS") {
            self.allowed_senders = senders;
        }
        if let Some(admins) = list_from_env("MERLIN_ADMIN_SENDERS") {
            self.admin_senders = admins;
        }
        if let Some(senders) = list_from_env("MERLIN_UNTRUSTED_SENDERS") {
            self.untrusted_senders = senders;
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

    pub fn is_admin(&self, sender: &str) -> bool {
        self.admin_senders.iter().any(|s| s == sender)
    }

    pub fn trust_level(&self, sender: &str) -> TrustLevel {
        if self.is_admin(sender) {
            TrustLevel::Admin
        } else if self.untrusted_senders.iter().any(|s| s == sender) {
            TrustLevel::Untrusted
        } else if self.is_allowed_sender(sender) {
            TrustLevel::Trusted
        } else {
            TrustLevel::Untrusted
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustLevel {
    Admin,
    Trusted,
    Untrusted,
}

impl Secrets {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            matrix_password: req("MATRIX_PASSWORD")?,
            session_encryption_key: opt("SESSION_ENCRYPTION_KEY")
                .unwrap_or_else(|| req("MATRIX_PASSWORD").unwrap_or_default()),
            openrouter_api_key: req("OPENROUTER_API_KEY")?,
            exa_api_key: opt("EXA_API_KEY"),
            fal_api_key: opt("FAL_API_KEY").or_else(|| opt("FAL_KEY")),
        })
    }
}

fn req(key: &str) -> Result<String> {
    opt(key).with_context(|| format!("{key} must be set in the environment and not be empty"))
}

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

macro_rules! defaults {
    ($($name:ident -> $ty:ty = $value:expr;)*) => {
        $(fn $name() -> $ty { $value.into() })*
    };
}

defaults! {
    default_context_window     -> usize   = 40usize;
    default_timezone           -> String  = "Australia/Sydney";
    default_chat_model         -> String  = "z-ai/glm-5.3-flash";
    default_image_model        -> String  = "meta/muse-image";
    default_image_provider     -> String  = "openrouter";
    default_reasoning_effort   -> String  = "medium";
    default_embedding_model    -> String  = "google/gemini-embedding-001";
    default_embedding_dimensions -> usize = 768usize;
    default_embed_batch        -> usize   = 32usize;
    default_max_response_bytes -> usize   = 8usize * 1024 * 1024;
    default_tool_iterations    -> usize   = 64usize;
    default_request_timeout_s  -> u64     = 120u64;
    default_exec_timeout_s     -> u64     = 60u64;
    default_turn_timeout_s     -> u64     = 600u64;
    default_exec_memory_max    -> String  = "1G";
    default_state_dir          -> PathBuf = PathBuf::from("/var/lib/merlin");
}

macro_rules! default_via_serde {
    ($($ty:ty),*) => {
        $(impl Default for $ty {
            fn default() -> Self {
                toml::from_str("").expect("every field has a serde default")
            }
        })*
    };
}

default_via_serde!(ModelConfig, Limits);
