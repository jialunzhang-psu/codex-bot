use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub telegram: TelegramConfig,
    #[serde(default = "default_log_level")]
    pub log_level: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramConfig {
    #[serde(default)]
    pub worker_tokens: Vec<String>,
}

fn default_log_level() -> String {
    "info".to_string()
}

fn normalize_log_level(level: impl Into<String>) -> String {
    let level = level.into();
    if level.trim().is_empty() {
        default_log_level()
    } else {
        level
    }
}

impl TelegramConfig {
    fn normalized(self) -> Result<Self> {
        if self.worker_tokens.is_empty() {
            bail!("telegram worker_tokens must not be empty");
        }
        let mut seen_workers = std::collections::HashSet::new();
        for token in &self.worker_tokens {
            let trimmed = token.trim();
            if trimmed.is_empty() {
                bail!("telegram worker_tokens must not contain empty entries");
            }
            if !seen_workers.insert(trimmed.to_string()) {
                bail!("duplicate telegram token configured");
            }
        }
        Ok(Self {
            worker_tokens: self
                .worker_tokens
                .into_iter()
                .map(|token| token.trim().to_string())
                .collect(),
        })
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        let mut config = toml::from_str::<Self>(&raw)
            .with_context(|| format!("invalid daemon config TOML in {}", path.display()))?;
        config.log_level = normalize_log_level(config.log_level);
        config.telegram = config.telegram.normalized()?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn load_accepts_current_full_config_shape() {
        let dir = tempdir().expect("temp dir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            concat!(
                "project_name = \"demo\"\n",
                "state_path = \"./state.json\"\n",
                "log_level = \"debug\"\n\n",
                "[rpc]\n",
                "socket_dir = \"./sockets\"\n\n",
                "[telegram]\n",
                "manager_token = \"manager\"\n",
                "worker_tokens = [\"worker-a\", \"worker-b\"]\n",
                "allow_from = [1]\n",
                "group_reply_all = false\n",
                "share_session_in_channel = false\n",
                "poll_timeout_seconds = 30\n\n",
                "[codex]\n",
                "bin = \"codex\"\n",
                "extra_env = []\n\n",
                "[claude]\n",
                "bin = \"claude\"\n",
                "extra_env = []\n\n",
                "[manager]\n",
                "default_backend = \"claude\"\n",
            ),
        )
        .expect("write config");

        let config = Config::load(&config_path).expect("config should load");
        assert_eq!(config.log_level, "debug");
        assert_eq!(config.telegram.worker_tokens, vec!["worker-a", "worker-b"]);
    }

    #[test]
    fn load_rejects_empty_worker_list() {
        let dir = tempdir().expect("temp dir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            concat!(
                "log_level = \"info\"\n\n",
                "[telegram]\n",
                "worker_tokens = []\n",
            ),
        )
        .expect("write config");

        let err = Config::load(&config_path).expect_err("empty worker list should fail");
        assert!(err.to_string().contains("worker_tokens"));
    }

    #[test]
    fn load_rejects_duplicate_worker_tokens() {
        let dir = tempdir().expect("temp dir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            concat!(
                "log_level = \"info\"\n\n",
                "[telegram]\n",
                "worker_tokens = [\"dup\", \"dup\"]\n",
            ),
        )
        .expect("write config");

        let err = Config::load(&config_path).expect_err("duplicate worker tokens should fail");
        assert!(err.to_string().contains("duplicate telegram token"));
    }

    #[test]
    fn load_normalizes_blank_log_level() {
        let dir = tempdir().expect("temp dir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            concat!(
                "log_level = \"\"\n\n",
                "[telegram]\n",
                "worker_tokens = [\"worker\"]\n",
            ),
        )
        .expect("write config");

        let config = Config::load(&config_path).expect("config should load");
        assert_eq!(config.log_level, "info");
    }
}
