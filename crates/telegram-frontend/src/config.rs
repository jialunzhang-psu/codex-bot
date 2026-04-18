use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct TelegramFrontendConfig {
    pub manager_token: String,
    pub worker_tokens: Vec<String>,
    pub poll_timeout_seconds: u64,
}

#[derive(Debug, Deserialize)]
struct FileConfig {
    telegram: RawTelegramConfig,
}

#[derive(Debug, Deserialize)]
struct RawTelegramConfig {
    manager_token: String,
    worker_tokens: Vec<String>,
    #[serde(default = "default_poll_timeout_seconds")]
    poll_timeout_seconds: u64,
}

impl TelegramFrontendConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        let config = toml::from_str::<FileConfig>(&raw)
            .with_context(|| format!("invalid telegram frontend TOML in {}", path.display()))?;
        normalize(config.telegram)
    }

    pub fn worker_token(&self, token_index: usize) -> Result<String> {
        self.worker_tokens
            .get(token_index)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("worker token_index {} is out of range", token_index))
    }
}

fn normalize(raw: RawTelegramConfig) -> Result<TelegramFrontendConfig> {
    let manager_token = raw.manager_token.trim().to_string();
    if manager_token.is_empty() {
        bail!("telegram manager_token is empty");
    }
    if raw.worker_tokens.is_empty() {
        bail!("telegram worker_tokens must not be empty");
    }
    let worker_tokens = raw
        .worker_tokens
        .into_iter()
        .map(|token| token.trim().to_string())
        .collect::<Vec<_>>();
    if worker_tokens.iter().any(|token| token.is_empty()) {
        bail!("telegram worker_tokens must not contain empty entries");
    }
    Ok(TelegramFrontendConfig {
        manager_token,
        worker_tokens,
        poll_timeout_seconds: raw.poll_timeout_seconds,
    })
}

fn default_poll_timeout_seconds() -> u64 {
    30
}
