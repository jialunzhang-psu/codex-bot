use std::path::PathBuf;

use serde::{Deserialize, Deserializer};

pub use session::BackendKindConfig;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub telegram: TelegramConfig,
    pub codex: CodexConfig,
    #[serde(default)]
    pub claude: ClaudeConfig,
    #[serde(default)]
    pub manager: ManagerConfig,
    #[serde(default)]
    pub rpc: RpcConfig,
    #[serde(default)]
    pub state_path: Option<PathBuf>,
    #[serde(default = "default_log_level")]
    pub log_level: String,
}

#[derive(Debug, Clone)]
pub struct TelegramConfig {
    pub manager_token: String,
    pub worker_tokens: Vec<String>,
    pub allow_from: Vec<i64>,
    pub group_reply_all: bool,
    pub share_session_in_channel: bool,
    pub poll_timeout_seconds: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodexConfig {
    #[serde(default = "default_codex_bin")]
    pub bin: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub extra_env: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClaudeConfig {
    #[serde(default = "default_claude_work_dir")]
    pub work_dir: PathBuf,
    #[serde(default = "default_claude_bin")]
    pub bin: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub extra_env: Vec<String>,
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        Self {
            work_dir: default_claude_work_dir(),
            bin: default_claude_bin(),
            model: None,
            extra_env: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ManagerConfig {
    #[serde(default = "default_backend_kind")]
    pub default_backend: BackendKindConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RpcConfig {
    #[serde(default)]
    pub socket_dir: Option<PathBuf>,
}

impl Default for ManagerConfig {
    fn default() -> Self {
        Self {
            default_backend: default_backend_kind(),
        }
    }
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_poll_timeout_seconds() -> u64 {
    30
}

fn default_codex_bin() -> String {
    "codex".to_string()
}

fn default_claude_bin() -> String {
    "claude".to_string()
}

fn default_claude_work_dir() -> PathBuf {
    PathBuf::from(".")
}

fn default_backend_kind() -> BackendKindConfig {
    BackendKindConfig::Codex
}

#[derive(Debug, Clone, Deserialize)]
struct RawTelegramConfig {
    #[serde(default)]
    manager_token: Option<String>,
    #[serde(default)]
    worker_tokens: Vec<String>,
    #[serde(default)]
    allow_from: Vec<i64>,
    #[serde(default)]
    group_reply_all: bool,
    #[serde(default)]
    share_session_in_channel: bool,
    #[serde(default = "default_poll_timeout_seconds")]
    poll_timeout_seconds: u64,
}

impl<'de> Deserialize<'de> for TelegramConfig {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawTelegramConfig::deserialize(deserializer)?;
        Ok(Self {
            manager_token: raw.manager_token.unwrap_or_default(),
            worker_tokens: raw.worker_tokens,
            allow_from: raw.allow_from,
            group_reply_all: raw.group_reply_all,
            share_session_in_channel: raw.share_session_in_channel,
            poll_timeout_seconds: raw.poll_timeout_seconds,
        })
    }
}
