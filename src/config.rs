use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

use crate::codex::RuntimeSettings;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub project_name: Option<String>,
    pub telegram: TelegramConfig,
    pub codex: CodexConfig,
    #[serde(default)]
    pub state_path: Option<PathBuf>,
    #[serde(default = "default_log_level")]
    pub log_level: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramConfig {
    pub token: String,
    #[serde(default)]
    pub allow_from: Vec<i64>,
    #[serde(default)]
    pub group_reply_all: bool,
    #[serde(default)]
    pub share_session_in_channel: bool,
    #[serde(default = "default_poll_timeout_seconds")]
    pub poll_timeout_seconds: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CodexConfig {
    pub work_dir: PathBuf,
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

fn default_log_level() -> String {
    "info".to_string()
}

fn default_poll_timeout_seconds() -> u64 {
    30
}

fn default_codex_bin() -> String {
    "codex".to_string()
}

impl Config {
    pub fn discover_projects(path: &Path) -> Result<Vec<String>> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        let value: toml::Value =
            toml::from_str(&raw).with_context(|| format!("invalid TOML in {}", path.display()))?;

        if value.get("projects").is_none() {
            return Ok(Vec::new());
        }

        let legacy: LegacyRoot = toml::from_str(&raw)
            .with_context(|| format!("invalid cc-connect TOML in {}", path.display()))?;
        Ok(legacy
            .projects
            .into_iter()
            .filter(|project| {
                project.agent.kind.eq_ignore_ascii_case("codex")
                    && project
                        .platforms
                        .iter()
                        .any(|platform| platform.kind.eq_ignore_ascii_case("telegram"))
            })
            .map(|project| project.name)
            .collect())
    }

    pub fn load(path: &Path, selected_project: Option<&str>) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        let value: toml::Value =
            toml::from_str(&raw).with_context(|| format!("invalid TOML in {}", path.display()))?;

        let mut config = if value.get("telegram").is_some() && value.get("codex").is_some() {
            toml::from_str::<Self>(&raw)
                .with_context(|| format!("invalid bridge TOML in {}", path.display()))?
        } else if value.get("projects").is_some() {
            let legacy: LegacyRoot = toml::from_str(&raw)
                .with_context(|| format!("invalid cc-connect TOML in {}", path.display()))?;
            Config::from_legacy(path, legacy, selected_project)?
        } else {
            bail!(
                "unsupported config format in {}: expected bridge config or cc-connect [[projects]]",
                path.display()
            );
        };

        config.codex.work_dir = normalize_path(path.parent(), &config.codex.work_dir);
        if let Some(state_path) = &config.state_path {
            config.state_path = Some(normalize_path(path.parent(), state_path));
        }

        Ok(config)
    }

    pub fn default_state_path(&self, config_path: &Path) -> PathBuf {
        if let Some(path) = &self.state_path {
            return path.clone();
        }

        let base_dir = config_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        normalize_path(
            Some(&base_dir),
            Path::new("codex-telegram-bridge-state.json"),
        )
    }

    pub fn default_runtime_settings(&self) -> RuntimeSettings {
        RuntimeSettings::new(
            self.codex.model.clone(),
            self.codex.reasoning_effort.clone(),
            self.codex.mode.clone(),
        )
    }

    pub fn user_allowed(&self, user_id: i64) -> bool {
        self.telegram.allow_from.is_empty() || self.telegram.allow_from.contains(&user_id)
    }

    fn from_legacy(
        path: &Path,
        legacy: LegacyRoot,
        selected_project: Option<&str>,
    ) -> Result<Self> {
        let project = if let Some(name) = selected_project {
            legacy
                .projects
                .iter()
                .find(|project| project.name == name)
                .ok_or_else(|| anyhow!("project {name:?} not found in {}", path.display()))?
        } else {
            legacy
                .projects
                .first()
                .ok_or_else(|| anyhow!("no projects found in {}", path.display()))?
        };

        if !project.agent.kind.eq_ignore_ascii_case("codex") {
            bail!(
                "project {:?} uses agent {:?}, but this bridge only supports codex",
                project.name,
                project.agent.kind
            );
        }

        let telegram = project
            .platforms
            .iter()
            .find(|platform| platform.kind.eq_ignore_ascii_case("telegram"))
            .ok_or_else(|| anyhow!("project {:?} has no telegram platform", project.name))?;

        let state_path = Some(legacy_state_path(
            path,
            legacy.data_dir.as_deref(),
            &project.name,
        ));
        Ok(Self {
            project_name: Some(project.name.clone()),
            telegram: TelegramConfig {
                token: telegram.options.token.clone(),
                allow_from: parse_allow_from(telegram.options.allow_from.as_ref()),
                group_reply_all: telegram.options.group_reply_all,
                share_session_in_channel: telegram.options.share_session_in_channel,
                poll_timeout_seconds: telegram
                    .options
                    .poll_timeout_seconds
                    .unwrap_or_else(default_poll_timeout_seconds),
            },
            codex: CodexConfig {
                work_dir: project.agent.options.work_dir.clone(),
                bin: project
                    .agent
                    .options
                    .bin
                    .clone()
                    .unwrap_or_else(default_codex_bin),
                model: project.agent.options.model.clone(),
                reasoning_effort: project.agent.options.reasoning_effort.clone(),
                mode: project.agent.options.mode.clone(),
                extra_env: project.agent.options.extra_env.clone(),
            },
            state_path,
            log_level: legacy
                .log
                .and_then(|log| log.level)
                .unwrap_or_else(default_log_level),
        })
    }
}

fn normalize_path(base_dir: Option<&Path>, path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }

    if let Some(base) = base_dir {
        return base.join(path);
    }

    path.to_path_buf()
}

fn legacy_state_path(config_path: &Path, data_dir: Option<&str>, project_name: &str) -> PathBuf {
    let file_name = format!(
        "codex-telegram-bridge-{}.json",
        sanitize_project_name(project_name)
    );
    if let Some(dir) = data_dir.filter(|value| !value.trim().is_empty()) {
        return normalize_path(config_path.parent(), Path::new(dir)).join(file_name);
    }

    let base_dir = config_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    base_dir.join(file_name)
}

fn sanitize_project_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push('_');
        }
    }
    out.trim_matches('_').to_string()
}

fn parse_allow_from(value: Option<&AllowFromValue>) -> Vec<i64> {
    match value {
        Some(AllowFromValue::Single(id)) => vec![*id],
        Some(AllowFromValue::List(ids)) => ids.clone(),
        Some(AllowFromValue::Csv(raw)) => raw
            .split(',')
            .filter_map(|part| part.trim().parse::<i64>().ok())
            .collect(),
        None => Vec::new(),
    }
}

#[derive(Debug, Deserialize)]
struct LegacyRoot {
    #[serde(default)]
    data_dir: Option<String>,
    #[serde(default)]
    log: Option<LegacyLog>,
    #[serde(default)]
    projects: Vec<LegacyProject>,
}

#[derive(Debug, Deserialize)]
struct LegacyLog {
    #[serde(default)]
    level: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LegacyProject {
    name: String,
    agent: LegacyAgent,
    #[serde(default)]
    platforms: Vec<LegacyPlatform>,
}

#[derive(Debug, Deserialize)]
struct LegacyAgent {
    #[serde(rename = "type")]
    kind: String,
    options: LegacyAgentOptions,
}

#[derive(Debug, Deserialize)]
struct LegacyAgentOptions {
    work_dir: PathBuf,
    #[serde(default)]
    bin: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    extra_env: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct LegacyPlatform {
    #[serde(rename = "type")]
    kind: String,
    options: LegacyPlatformOptions,
}

#[derive(Debug, Deserialize)]
struct LegacyPlatformOptions {
    token: String,
    #[serde(default)]
    allow_from: Option<AllowFromValue>,
    #[serde(default)]
    group_reply_all: bool,
    #[serde(default)]
    share_session_in_channel: bool,
    #[serde(default)]
    poll_timeout_seconds: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum AllowFromValue {
    Single(i64),
    List(Vec<i64>),
    Csv(String),
}
