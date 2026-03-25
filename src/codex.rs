mod exec;
mod review;

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use reqwest::Client;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead};
use tokio::process::Command;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use toml::Value as TomlValue;
use toml::map::Map as TomlMap;
use walkdir::WalkDir;

pub use crate::accounts::{
    AddAccountResult, PoolAccountView, PoolList, RemoveAccountResult, RemoveAllAccountsResult,
    StoredAccount, SwitchAccountResult,
};
use crate::config::CodexConfig;
pub use exec::{SpawnedTurn, TurnEvent, TurnOutcome};
pub use review::{ReviewRequest, SpawnedReview};

const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeSettings {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default = "default_mode")]
    pub mode: String,
}

#[derive(Debug, Clone)]
pub struct CodexSessionSummary {
    pub id: String,
    pub display_name: Option<String>,
    pub summary: String,
    pub message_count: usize,
    pub modified_at: SystemTime,
}

#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub role: String,
    pub content: String,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageReport {
    pub provider: String,
    pub account_id: String,
    pub user_id: String,
    pub email: String,
    pub plan: String,
    pub buckets: Vec<UsageBucket>,
    pub credits: Option<UsageCredits>,
}

#[derive(Debug, Clone)]
pub struct EffectiveCodexSettings {
    pub codex_home: PathBuf,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TrustDirResult {
    pub config_path: PathBuf,
    pub trusted_dir: PathBuf,
    pub already_trusted: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageBucket {
    pub name: String,
    pub allowed: bool,
    pub limit_reached: bool,
    pub windows: Vec<UsageWindow>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageWindow {
    pub name: String,
    pub used_percent: i64,
    pub window_seconds: i64,
    pub reset_after_seconds: i64,
    pub reset_at_unix: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageCredits {
    pub has_credits: bool,
    pub unlimited: bool,
    pub balance: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AccountUsageView {
    pub account: StoredAccount,
    pub pooled: bool,
    pub active: bool,
    pub status: String,
    pub message: Option<String>,
    pub usage: Option<UsageReport>,
}

#[derive(Debug, Clone)]
pub struct PoolUsageReport {
    pub pool_dir: PathBuf,
    pub codex_auth_path: PathBuf,
    pub active_in_pool: bool,
    pub accounts: Vec<AccountUsageView>,
}

struct CodexStore {
    sessions_root: PathBuf,
    session_index_path: PathBuf,
    history_path: PathBuf,
    state_db_path: Option<PathBuf>,
    auth_path: PathBuf,
}

#[derive(Debug, Clone)]
struct OAuthTokens {
    access_token: String,
    account_id: String,
}

#[derive(Debug, Deserialize)]
struct OAuthPayload {
    tokens: OAuthPayloadTokens,
}

#[derive(Debug, Deserialize)]
struct OAuthPayloadTokens {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    account_id: String,
}

#[derive(Debug, Deserialize)]
struct RawUsageResponse {
    #[serde(default)]
    user_id: String,
    #[serde(default)]
    account_id: String,
    #[serde(default)]
    email: String,
    #[serde(default)]
    plan_type: String,
    #[serde(default)]
    rate_limit: Option<RawUsageBucket>,
    #[serde(default)]
    code_review_rate_limit: Option<RawUsageBucket>,
    #[serde(default)]
    credits: Option<RawUsageCredits>,
}

#[derive(Debug, Deserialize)]
struct RawUsageBucket {
    #[serde(default)]
    allowed: bool,
    #[serde(default)]
    limit_reached: bool,
    #[serde(default)]
    primary_window: Option<RawUsageWindow>,
    #[serde(default)]
    secondary_window: Option<RawUsageWindow>,
}

#[derive(Debug, Deserialize)]
struct RawUsageWindow {
    #[serde(default)]
    used_percent: i64,
    #[serde(default)]
    limit_window_seconds: i64,
    #[serde(default)]
    reset_after_seconds: i64,
    #[serde(default)]
    reset_at: i64,
}

#[derive(Debug, Deserialize)]
struct RawUsageCredits {
    #[serde(default)]
    has_credits: bool,
    #[serde(default)]
    unlimited: bool,
    #[serde(default)]
    balance: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
struct CodexHomeConfigFile {
    #[serde(default)]
    model: Option<String>,
    #[serde(default, rename = "model_reasoning_effort")]
    reasoning_effort: Option<String>,
    #[serde(default)]
    projects: HashMap<String, CodexProjectConfigFile>,
}

#[derive(Debug, Default, Deserialize)]
struct CodexProjectConfigFile {
    #[serde(default)]
    model: Option<String>,
    #[serde(default, rename = "model_reasoning_effort")]
    reasoning_effort: Option<String>,
}

#[derive(Debug, Default)]
struct CodexConfigDefaults {
    model: Option<String>,
    reasoning_effort: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AuthStatus {
    pub logged_in: bool,
    pub summary: String,
}

#[derive(Debug, Clone)]
pub struct DeviceAuthPrompt {
    pub verification_uri: String,
    pub user_code: String,
    pub expires_in_minutes: Option<u64>,
}

#[derive(Debug)]
pub enum LoginEvent {
    Prompt(DeviceAuthPrompt),
}

#[derive(Debug)]
pub struct LoginOutcome {
    pub output: String,
}

pub struct SpawnedLogin {
    pub events: mpsc::Receiver<LoginEvent>,
    pub join: JoinHandle<Result<LoginOutcome>>,
    pub pid: Arc<AtomicU32>,
}

#[derive(Clone)]
pub struct CodexClient {
    bin: String,
    work_dir: PathBuf,
    extra_env: Vec<String>,
}

#[derive(Default)]
struct DeviceAuthParser {
    verification_uri: Option<String>,
    user_code: Option<String>,
    expires_in_minutes: Option<u64>,
    output_lines: Vec<String>,
    prompt_emitted: bool,
}

fn default_mode() -> String {
    "suggest".to_string()
}

impl Default for RuntimeSettings {
    fn default() -> Self {
        Self {
            model: None,
            reasoning_effort: None,
            mode: default_mode(),
        }
    }
}

impl RuntimeSettings {
    pub fn new(
        model: Option<String>,
        reasoning_effort: Option<String>,
        mode: Option<String>,
    ) -> Self {
        Self {
            model: normalize_optional(model),
            reasoning_effort: normalize_reasoning(reasoning_effort),
            mode: normalize_mode(mode),
        }
    }

    pub fn merged_with(&self, defaults: &Self) -> Self {
        Self {
            model: self.model.clone().or_else(|| defaults.model.clone()),
            reasoning_effort: self
                .reasoning_effort
                .clone()
                .or_else(|| defaults.reasoning_effort.clone()),
            mode: if self.mode.trim().is_empty() {
                defaults.mode.clone()
            } else {
                normalize_mode(Some(self.mode.clone()))
            },
        }
    }
}

impl CodexStore {
    fn from_home(codex_home: PathBuf) -> Result<Self> {
        let state_db_path = find_latest_state_db(&codex_home)?;
        Ok(Self {
            sessions_root: codex_home.join("sessions"),
            session_index_path: codex_home.join("session_index.jsonl"),
            history_path: codex_home.join("history.jsonl"),
            state_db_path,
            auth_path: codex_home.join("auth.json"),
        })
    }
}

impl CodexClient {
    pub fn new(config: &CodexConfig) -> Self {
        Self {
            bin: config.bin.clone(),
            work_dir: config.work_dir.clone(),
            extra_env: config.extra_env.clone(),
        }
    }

    pub fn work_dir(&self) -> &Path {
        &self.work_dir
    }

    pub fn codex_home(&self) -> Result<PathBuf> {
        if let Some(value) = self
            .extra_env
            .iter()
            .rev()
            .filter_map(parse_env_pair)
            .find_map(|(name, value)| (name == "CODEX_HOME").then_some(value))
            .filter(|value| !value.trim().is_empty())
        {
            return Ok(PathBuf::from(value));
        }

        codex_home()
    }

    pub fn effective_settings(&self, runtime: &RuntimeSettings) -> Result<EffectiveCodexSettings> {
        let codex_home = self.codex_home()?;
        let defaults = load_codex_defaults(&codex_home, &self.work_dir)?;
        Ok(EffectiveCodexSettings {
            codex_home,
            model: normalize_optional(runtime.model.clone()).or(defaults.model),
            reasoning_effort: normalize_reasoning(runtime.reasoning_effort.clone())
                .or(defaults.reasoning_effort),
        })
    }

    pub fn trust_dir(&self, dir: Option<&Path>) -> Result<TrustDirResult> {
        let codex_home = self.codex_home()?;
        let config_path = codex_home.join("config.toml");
        let trusted_dir = resolve_trust_dir(&self.work_dir, dir);
        let already_trusted = upsert_trusted_project(&config_path, &trusted_dir)?;
        Ok(TrustDirResult {
            config_path,
            trusted_dir,
            already_trusted,
        })
    }

    pub fn list_codex_sessions(&self) -> Result<Vec<CodexSessionSummary>> {
        list_sessions_in(&self.work_dir, &self.codex_home()?)
    }

    pub fn list_accounts(&self) -> Result<PoolList> {
        crate::accounts::list_accounts(&self.codex_home()?)
    }

    pub fn add_current_account(&self, label: Option<String>) -> Result<AddAccountResult> {
        crate::accounts::add_current_account(&self.codex_home()?, label)
    }

    pub fn switch_account(&self, account_id: &str) -> Result<SwitchAccountResult> {
        crate::accounts::switch_account(&self.codex_home()?, account_id)
    }

    pub fn remove_account(&self, account_id: &str) -> Result<RemoveAccountResult> {
        crate::accounts::remove_account(&self.codex_home()?, account_id)
    }

    pub fn remove_all_accounts(&self) -> Result<RemoveAllAccountsResult> {
        crate::accounts::remove_all_accounts(&self.codex_home()?)
    }

    pub fn get_codex_session_history(
        &self,
        codex_session_id: &str,
        limit: usize,
    ) -> Result<Vec<HistoryEntry>> {
        get_session_history_in(codex_session_id, limit, &self.codex_home()?)
    }

    pub async fn get_all_usage(&self) -> Result<PoolUsageReport> {
        let codex_home = self.codex_home()?;
        let resolved = crate::accounts::resolve_accounts(&codex_home)?;
        let client = Client::new();

        if resolved.accounts.is_empty() {
            let usage = fetch_usage(&client, read_oauth_tokens_in(&codex_home)?).await?;
            return Ok(PoolUsageReport {
                pool_dir: resolved.pool_dir,
                codex_auth_path: resolved.codex_auth_path,
                active_in_pool: false,
                accounts: vec![AccountUsageView {
                    account: inferred_current_account(&usage),
                    pooled: false,
                    active: true,
                    status: "ok".to_string(),
                    message: None,
                    usage: Some(usage),
                }],
            });
        }

        let mut accounts = Vec::new();
        for entry in resolved.accounts {
            if !entry.auth_path.exists() {
                accounts.push(AccountUsageView {
                    account: entry.account,
                    pooled: true,
                    active: entry.active,
                    status: "missing_auth".to_string(),
                    message: Some(format!(
                        "pooled auth file missing: {}",
                        entry.auth_path.display()
                    )),
                    usage: None,
                });
                continue;
            }

            match read_oauth_tokens_from_path(&entry.auth_path) {
                Ok(tokens) => match fetch_usage(&client, tokens).await {
                    Ok(usage) => accounts.push(AccountUsageView {
                        account: entry.account,
                        pooled: true,
                        active: entry.active,
                        status: "ok".to_string(),
                        message: None,
                        usage: Some(usage),
                    }),
                    Err(err) => {
                        let message = err.to_string();
                        accounts.push(AccountUsageView {
                            account: entry.account,
                            pooled: true,
                            active: entry.active,
                            status: if looks_like_usage_limit_error(&message) {
                                "quota_exhausted".to_string()
                            } else {
                                "usage_error".to_string()
                            },
                            message: Some(message),
                            usage: None,
                        });
                    }
                },
                Err(err) => accounts.push(AccountUsageView {
                    account: entry.account,
                    pooled: true,
                    active: entry.active,
                    status: "invalid_auth".to_string(),
                    message: Some(err.to_string()),
                    usage: None,
                }),
            }
        }

        Ok(PoolUsageReport {
            pool_dir: resolved.pool_dir,
            codex_auth_path: resolved.codex_auth_path,
            active_in_pool: resolved.active_in_pool,
            accounts,
        })
    }

    pub fn delete_codex_session(&self, codex_session_id: &str) -> Result<()> {
        delete_session_artifacts_in(&self.codex_home()?, codex_session_id)
    }

    pub async fn login_status(&self) -> Result<AuthStatus> {
        let output = self.run_command_capture(&["login", "status"]).await?;
        let summary = summarize_auth_status(&output.output);

        if output.status.success() {
            return Ok(AuthStatus {
                logged_in: true,
                summary,
            });
        }

        if summary == "Not logged in" {
            return Ok(AuthStatus {
                logged_in: false,
                summary,
            });
        }

        Err(anyhow!(
            "failed to read login status: {}",
            if output.output.is_empty() {
                format!("codex login status exited with {}", output.status)
            } else {
                output.output
            }
        ))
    }

    pub async fn run_device_login_interactive(&self) -> Result<()> {
        let args = ["login", "--device-auth"];
        let status = self
            .command()
            .args(args)
            .current_dir(&self.work_dir)
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()
            .await
            .with_context(|| format!("failed to run {} {}", self.bin, args.join(" ")))?;
        if !status.success() {
            bail!("codex login exited with status {status}");
        }
        Ok(())
    }

    pub fn spawn_device_login(&self, cancel: CancellationToken) -> Result<SpawnedLogin> {
        let args = ["login", "--device-auth"];
        let mut command = self.command();
        command
            .args(args)
            .current_dir(&self.work_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let mut child = command.spawn().with_context(|| {
            format!(
                "failed to start codex login process: {} {}",
                self.bin,
                args.join(" ")
            )
        })?;

        let pid = Arc::new(AtomicU32::new(child.id().unwrap_or_default()));
        let stdout = child
            .stdout
            .take()
            .context("codex login stdout pipe is unavailable")?;
        let stderr = child
            .stderr
            .take()
            .context("codex login stderr pipe is unavailable")?;
        let child = Arc::new(Mutex::new(child));

        let (tx, rx) = mpsc::channel(8);
        let join = {
            let child = Arc::clone(&child);
            let pid_ref = Arc::clone(&pid);
            tokio::spawn(async move {
                let (line_tx, mut line_rx) = mpsc::channel(64);
                let stdout_task = tokio::spawn(stream_lines(stdout, line_tx.clone()));
                let stderr_task = tokio::spawn(stream_lines(stderr, line_tx));

                let killer_task = {
                    let child = Arc::clone(&child);
                    let cancel = cancel.clone();
                    tokio::spawn(async move {
                        cancel.cancelled().await;
                        let mut child = child.lock().await;
                        let _ = child.start_kill();
                    })
                };

                let mut parser = DeviceAuthParser::default();
                while let Some(line) = line_rx.recv().await {
                    if let Some(prompt) = parser.ingest_line(&line) {
                        if tx.send(LoginEvent::Prompt(prompt)).await.is_err() {
                            break;
                        }
                    }
                }

                let status = {
                    let mut child = child.lock().await;
                    let status = child
                        .wait()
                        .await
                        .context("failed to wait for codex login")?;
                    pid_ref.store(0, Ordering::Relaxed);
                    status
                };

                killer_task.abort();
                stdout_task.await.context("stdout task join failed")??;
                stderr_task.await.context("stderr task join failed")??;
                drop(tx);

                if cancel.is_cancelled() {
                    return Err(anyhow!("login cancelled"));
                }

                if !status.success() {
                    let output = parser.output_text();
                    if !output.is_empty() {
                        return Err(anyhow!(output));
                    }
                    return Err(anyhow!("codex login exited with status {status}"));
                }

                Ok(LoginOutcome {
                    output: parser.output_text(),
                })
            })
        };

        Ok(SpawnedLogin {
            events: rx,
            join,
            pid,
        })
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.bin);
        if !self.extra_env.is_empty() {
            command.envs(self.extra_env.iter().filter_map(parse_env_pair));
        }
        command
    }

    async fn run_command_capture(&self, args: &[&str]) -> Result<CapturedOutput> {
        let output = self
            .command()
            .args(args)
            .current_dir(&self.work_dir)
            .output()
            .await
            .with_context(|| format!("failed to run {} {}", self.bin, args.join(" ")))?;

        Ok(CapturedOutput {
            status: output.status,
            output: merge_command_output(&output.stdout, &output.stderr),
        })
    }
}

impl DeviceAuthParser {
    fn ingest_line(&mut self, raw_line: &str) -> Option<DeviceAuthPrompt> {
        let line = strip_ansi_codes(raw_line).replace('\r', "");
        let line = line.trim();
        if line.is_empty() {
            return None;
        }

        self.output_lines.push(line.to_string());

        if self.verification_uri.is_none() {
            self.verification_uri = extract_url(line);
        }
        if self.user_code.is_none() {
            self.user_code = extract_device_code(line);
        }
        if self.expires_in_minutes.is_none() {
            self.expires_in_minutes = extract_expiry_minutes(line);
        }

        if self.prompt_emitted {
            return None;
        }

        let (Some(verification_uri), Some(user_code)) =
            (self.verification_uri.clone(), self.user_code.clone())
        else {
            return None;
        };

        self.prompt_emitted = true;
        Some(DeviceAuthPrompt {
            verification_uri,
            user_code,
            expires_in_minutes: self.expires_in_minutes,
        })
    }

    fn output_text(&self) -> String {
        self.output_lines.join("\n")
    }
}

struct CapturedOutput {
    status: std::process::ExitStatus,
    output: String,
}

fn parse_env_pair(entry: &String) -> Option<(String, String)> {
    let (key, value) = entry.split_once('=')?;
    Some((key.to_string(), value.to_string()))
}

fn normalize_optional(value: Option<String>) -> Option<String> {
    value.and_then(|text| {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn normalize_mode(mode: Option<String>) -> String {
    match mode
        .unwrap_or_else(default_mode)
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "default" | "suggest" => "suggest".to_string(),
        "auto-edit" | "auto_edit" | "autoedit" | "edit" => "auto-edit".to_string(),
        "acceptedits" | "accept_edits" => "auto-edit".to_string(),
        "full-auto" | "full_auto" | "fullauto" | "auto" => "full-auto".to_string(),
        "yolo" | "dangerously-bypass" | "bypass" | "bypasspermissions" | "bypass_permissions" => {
            "yolo".to_string()
        }
        _ => "suggest".to_string(),
    }
}

fn normalize_reasoning(reasoning: Option<String>) -> Option<String> {
    match reasoning
        .and_then(|value| normalize_optional(Some(value)))
        .unwrap_or_default()
        .as_str()
    {
        "" => None,
        "low" => Some("low".to_string()),
        "medium" | "med" => Some("medium".to_string()),
        "high" => Some("high".to_string()),
        "xhigh" | "x-high" | "very-high" => Some("xhigh".to_string()),
        _ => None,
    }
}

impl CodexHomeConfigFile {
    fn resolve_for_work_dir(&self, work_dir: &Path) -> CodexConfigDefaults {
        let mut defaults = CodexConfigDefaults {
            model: normalize_optional(self.model.clone()),
            reasoning_effort: normalize_reasoning(self.reasoning_effort.clone()),
        };

        if let Some(project) = self.best_matching_project(work_dir) {
            if let Some(model) = normalize_optional(project.model.clone()) {
                defaults.model = Some(model);
            }
            if let Some(reasoning_effort) = normalize_reasoning(project.reasoning_effort.clone()) {
                defaults.reasoning_effort = Some(reasoning_effort);
            }
        }

        defaults
    }

    fn best_matching_project(&self, work_dir: &Path) -> Option<&CodexProjectConfigFile> {
        let work_dir = canonicalize_or_clone(work_dir);
        self.projects
            .iter()
            .filter_map(|(project_path, config)| {
                let project_path = canonicalize_or_clone(Path::new(project_path));
                path_prefix_len(&project_path, &work_dir).map(|len| (len, config))
            })
            .max_by_key(|(len, _)| *len)
            .map(|(_, config)| config)
    }
}

fn load_codex_defaults(codex_home: &Path, work_dir: &Path) -> Result<CodexConfigDefaults> {
    let config_path = codex_home.join("config.toml");
    if !config_path.exists() {
        return Ok(CodexConfigDefaults::default());
    }

    let raw = fs::read_to_string(&config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let config: CodexHomeConfigFile = toml::from_str(&raw)
        .with_context(|| format!("invalid TOML in {}", config_path.display()))?;
    Ok(config.resolve_for_work_dir(work_dir))
}

fn resolve_trust_dir(work_dir: &Path, dir: Option<&Path>) -> PathBuf {
    let target = match dir {
        Some(path) if path.is_absolute() => path.to_path_buf(),
        Some(path) => work_dir.join(path),
        None => work_dir.to_path_buf(),
    };
    canonicalize_or_clone(&target)
}

fn upsert_trusted_project(config_path: &Path, trusted_dir: &Path) -> Result<bool> {
    let mut config = if config_path.exists() {
        let raw = fs::read_to_string(config_path)
            .with_context(|| format!("failed to read {}", config_path.display()))?;
        if raw.trim().is_empty() {
            TomlValue::Table(TomlMap::new())
        } else {
            toml::from_str::<TomlValue>(&raw)
                .with_context(|| format!("invalid TOML in {}", config_path.display()))?
        }
    } else {
        TomlValue::Table(TomlMap::new())
    };

    let root = config
        .as_table_mut()
        .ok_or_else(|| anyhow!("top-level Codex config must be a TOML table"))?;
    let projects = root
        .entry("projects")
        .or_insert_with(|| TomlValue::Table(TomlMap::new()));
    let projects = projects
        .as_table_mut()
        .ok_or_else(|| anyhow!("Codex config key `projects` must be a TOML table"))?;
    let project_key = trusted_dir.display().to_string();
    let project = projects
        .entry(project_key)
        .or_insert_with(|| TomlValue::Table(TomlMap::new()));
    let project = project
        .as_table_mut()
        .ok_or_else(|| anyhow!("Codex project entry must be a TOML table"))?;

    let already_trusted = project
        .get("trust_level")
        .and_then(TomlValue::as_str)
        .is_some_and(|value| value == "trusted");
    project.insert(
        "trust_level".to_string(),
        TomlValue::String("trusted".to_string()),
    );

    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let raw = toml::to_string_pretty(&config).context("failed to encode Codex config TOML")?;
    let tmp_path = config_path.with_extension("tmp");
    fs::write(&tmp_path, raw).with_context(|| format!("failed to write {}", tmp_path.display()))?;
    fs::rename(&tmp_path, config_path)
        .with_context(|| format!("failed to replace {}", config_path.display()))?;

    Ok(already_trusted)
}

fn canonicalize_or_clone(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn path_prefix_len(prefix: &Path, path: &Path) -> Option<usize> {
    let mut len = 0usize;
    let mut prefix_components = prefix.components();
    let mut path_components = path.components();
    loop {
        match (prefix_components.next(), path_components.next()) {
            (None, _) => return Some(len),
            (Some(left), Some(right)) if left == right => len += 1,
            _ => return None,
        }
    }
}

fn list_sessions_in(work_dir: &Path, codex_home: &Path) -> Result<Vec<CodexSessionSummary>> {
    let work_dir = normalize_path(work_dir)?;
    let store = CodexStore::from_home(codex_home.to_path_buf())?;
    let sessions_dir = sessions_root_in(codex_home);
    if !sessions_dir.exists() {
        return Ok(Vec::new());
    }

    let mut sessions = WalkDir::new(&sessions_dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .filter_map(|entry| parse_session_file(entry.path(), &work_dir).transpose())
        .collect::<Result<Vec<_>>>()?;

    let indexed_names = load_indexed_session_names(&store.session_index_path)?;
    let state_titles = load_state_thread_titles(store.state_db_path.as_deref())?;
    let history_names = load_history_session_names(&store.history_path)?;
    for session in &mut sessions {
        let derived_name = session.display_name.clone();
        session.display_name = indexed_names
            .get(&session.id)
            .cloned()
            .or_else(|| state_titles.get(&session.id).cloned())
            .or(derived_name)
            .or_else(|| history_names.get(&session.id).cloned());
    }

    sessions.sort_by(|left, right| right.modified_at.cmp(&left.modified_at));
    Ok(sessions)
}

fn get_session_history_in(
    session_id: &str,
    limit: usize,
    codex_home: &Path,
) -> Result<Vec<HistoryEntry>> {
    let Some(path) = find_session_file_in(codex_home, session_id)? else {
        bail!("session file not found: {session_id}");
    };

    let file = fs::File::open(&path)
        .with_context(|| format!("failed to open session transcript {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut entries = Vec::new();

    while reader
        .read_line(&mut line)
        .with_context(|| format!("failed to read {}", path.display()))?
        > 0
    {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            line.clear();
            continue;
        }

        let Ok(entry) = serde_json::from_str::<TimestampedSessionLine>(trimmed) else {
            line.clear();
            continue;
        };
        if entry.kind != "response_item" {
            line.clear();
            continue;
        }

        if let Ok(item) = serde_json::from_value::<ResponseItem>(entry.payload) {
            let timestamp = entry
                .timestamp
                .and_then(|value| DateTime::parse_from_rfc3339(&value).ok())
                .map(|value| value.with_timezone(&Utc))
                .unwrap_or_else(Utc::now);

            match item.role.as_deref() {
                Some("user") => {
                    let text = item
                        .content
                        .iter()
                        .filter(|content| {
                            content.kind == "input_text" && is_user_prompt(&content.text)
                        })
                        .map(|content| content.text.trim().to_string())
                        .filter(|text| !text.is_empty())
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !text.is_empty() {
                        entries.push(HistoryEntry {
                            role: "user".to_string(),
                            content: text,
                            timestamp,
                        });
                    }
                }
                Some("assistant") => {
                    let text = item
                        .content
                        .iter()
                        .filter(|content| content.kind == "output_text")
                        .map(|content| content.text.trim().to_string())
                        .filter(|text| !text.is_empty())
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !text.is_empty() {
                        entries.push(HistoryEntry {
                            role: "assistant".to_string(),
                            content: text,
                            timestamp,
                        });
                    }
                }
                _ => {}
            }
        }

        line.clear();
    }

    if limit > 0 && entries.len() > limit {
        entries = entries.split_off(entries.len() - limit);
    }
    Ok(entries)
}

fn parse_session_file(path: &Path, filter_cwd: &Path) -> Result<Option<CodexSessionSummary>> {
    let file = fs::File::open(path)
        .with_context(|| format!("failed to open session transcript {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to stat {}", path.display()))?;
    let mut session_id = None;
    let mut session_cwd = None;
    let mut display_name = None;
    let mut summary = String::new();
    let mut message_count = 0usize;

    let mut reader = BufReader::new(file);
    let mut line = String::new();
    while reader
        .read_line(&mut line)
        .with_context(|| format!("failed to read {}", path.display()))?
        > 0
    {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            line.clear();
            continue;
        }

        let Ok(entry) = serde_json::from_str::<SessionLine>(trimmed) else {
            line.clear();
            continue;
        };

        match entry.kind.as_str() {
            "session_meta" => {
                if let Ok(meta) = serde_json::from_value::<SessionMeta>(entry.payload) {
                    session_id = Some(meta.id);
                    session_cwd = Some(meta.cwd);
                }
            }
            "response_item" => {
                if let Ok(item) = serde_json::from_value::<ResponseItem>(entry.payload) {
                    if item.role.as_deref() == Some("user") {
                        message_count += 1;
                        for content in item.content {
                            if content.kind == "input_text"
                                && !content.text.trim().is_empty()
                                && is_user_prompt(&content.text)
                            {
                                if display_name.is_none() {
                                    display_name = derived_session_name_from_text(&content.text);
                                }
                                summary =
                                    session_summary_from_text(&content.text).unwrap_or_default();
                            }
                        }
                    } else if item.role.as_deref() == Some("assistant") {
                        message_count += 1;
                    }
                }
            }
            _ => {}
        }

        line.clear();
    }

    let Some(session_id) = session_id else {
        return Ok(None);
    };

    if let Some(session_cwd) = session_cwd {
        if normalize_path(Path::new(&session_cwd))? != filter_cwd {
            return Ok(None);
        }
    }

    Ok(Some(CodexSessionSummary {
        id: session_id,
        display_name,
        summary,
        message_count,
        modified_at: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
    }))
}

fn load_indexed_session_names(path: &Path) -> Result<HashMap<String, String>> {
    let mut names = HashMap::new();
    if !path.exists() {
        return Ok(names);
    }

    let file = fs::File::open(path)
        .with_context(|| format!("failed to open session index {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    while reader
        .read_line(&mut line)
        .with_context(|| format!("failed to read {}", path.display()))?
        > 0
    {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            if let Ok(entry) = serde_json::from_str::<SessionIndexEntry>(trimmed) {
                if let Some(name) = normalize_display_name(&entry.thread_name) {
                    names.insert(entry.id, name);
                }
            }
        }
        line.clear();
    }

    Ok(names)
}

fn load_state_thread_titles(path: Option<&Path>) -> Result<HashMap<String, String>> {
    let mut titles = HashMap::new();
    let Some(path) = path else {
        return Ok(titles);
    };
    if !path.exists() {
        return Ok(titles);
    }

    let connection =
        Connection::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    connection.busy_timeout(Duration::from_secs(5))?;
    if !table_exists(&connection, "threads")? {
        return Ok(titles);
    }

    let mut statement = connection.prepare("SELECT id, title FROM threads WHERE id <> ''")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
    })?;
    for row in rows {
        let (id, title) = row?;
        if let Some(title) = title.as_deref().and_then(normalize_display_name) {
            titles.insert(id, title);
        }
    }

    Ok(titles)
}

fn load_history_session_names(path: &Path) -> Result<HashMap<String, String>> {
    let mut names = HashMap::new();
    if !path.exists() {
        return Ok(names);
    }

    let file = fs::File::open(path)
        .with_context(|| format!("failed to open session history {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    while reader
        .read_line(&mut line)
        .with_context(|| format!("failed to read {}", path.display()))?
        > 0
    {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            if let Ok(entry) = serde_json::from_str::<HistorySessionLine>(trimmed) {
                if !names.contains_key(&entry.session_id) {
                    if let Some(name) = derived_session_name_from_text(&entry.text) {
                        names.insert(entry.session_id, name);
                    }
                }
            }
        }
        line.clear();
    }

    Ok(names)
}

fn sessions_root_in(codex_home: &Path) -> PathBuf {
    codex_home.join("sessions")
}

fn find_session_file_in(codex_home: &Path, session_id: &str) -> Result<Option<PathBuf>> {
    let store = CodexStore::from_home(codex_home.to_path_buf())?;
    let paths = find_session_files_in(&store, session_id)?;
    let best = paths
        .into_iter()
        .filter_map(|path| {
            let modified = fs::metadata(&path)
                .and_then(|metadata| metadata.modified())
                .ok()?;
            Some((modified, path))
        })
        .max_by(|left, right| left.0.cmp(&right.0))
        .map(|(_, path)| path);
    Ok(best)
}

fn delete_session_artifacts_in(codex_home: &Path, session_id: &str) -> Result<()> {
    let store = CodexStore::from_home(codex_home.to_path_buf())?;
    let paths = find_session_files_in(&store, session_id)?;
    if paths.is_empty() {
        bail!("session file not found: {session_id}");
    }

    for path in &paths {
        if path.exists() {
            fs::remove_file(path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
            prune_empty_parents(path, &store.sessions_root)?;
        }
    }

    rewrite_filtered_jsonl(&store.session_index_path, |line| {
        parse_index_session_id(line)
            .map(|id| id != session_id)
            .unwrap_or(true)
    })?;
    rewrite_filtered_jsonl(&store.history_path, |line| {
        parse_history_session_id(line)
            .map(|id| id != session_id)
            .unwrap_or(true)
    })?;

    if let Some(path) = &store.state_db_path {
        delete_threads_from_state_db(path, session_id)?;
    }

    Ok(())
}

fn find_session_files_in(store: &CodexStore, session_id: &str) -> Result<Vec<PathBuf>> {
    if !store.sessions_root.exists() {
        return Ok(Vec::new());
    }

    let mut paths = Vec::new();
    for entry in WalkDir::new(&store.sessions_root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
    {
        if entry.path().extension() != Some(OsStr::new("jsonl")) {
            continue;
        }

        if session_id_for_file(entry.path())?.as_deref() == Some(session_id) {
            paths.push(entry.into_path());
        }
    }

    Ok(paths)
}

fn session_id_for_file(path: &Path) -> Result<Option<String>> {
    let file = fs::File::open(path)
        .with_context(|| format!("failed to open session transcript {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();

    while reader
        .read_line(&mut line)
        .with_context(|| format!("failed to read {}", path.display()))?
        > 0
    {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            line.clear();
            continue;
        }

        let Ok(entry) = serde_json::from_str::<SessionLine>(trimmed) else {
            line.clear();
            continue;
        };
        if entry.kind == "session_meta" {
            let meta = serde_json::from_value::<SessionMeta>(entry.payload)
                .with_context(|| format!("invalid session_meta in {}", path.display()))?;
            return Ok(Some(meta.id));
        }

        line.clear();
    }

    Ok(None)
}

fn rewrite_filtered_jsonl<F>(path: &Path, mut keep: F) -> Result<()>
where
    F: FnMut(&str) -> bool,
{
    if !path.exists() {
        return Ok(());
    }

    let file =
        fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let reader = BufReader::new(file);
    let filtered = reader
        .lines()
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("failed to read {}", path.display()))?
        .into_iter()
        .filter(|line| keep(line))
        .collect::<Vec<_>>();

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let tmp_path = path.with_extension("tmp");
    let mut file = fs::File::create(&tmp_path)
        .with_context(|| format!("failed to create {}", tmp_path.display()))?;
    for line in filtered {
        writeln!(file, "{line}")?;
    }
    file.flush()?;
    fs::rename(&tmp_path, path).with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

fn parse_index_session_id(line: &str) -> Option<String> {
    let value: Value = serde_json::from_str(line).ok()?;
    value.get("id")?.as_str().map(str::to_string)
}

fn parse_history_session_id(line: &str) -> Option<String> {
    let value: Value = serde_json::from_str(line).ok()?;
    value.get("session_id")?.as_str().map(str::to_string)
}

fn delete_threads_from_state_db(path: &Path, session_id: &str) -> Result<()> {
    let connection =
        Connection::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    connection.busy_timeout(Duration::from_secs(5))?;

    if table_exists(&connection, "threads")? {
        let mut statement = connection.prepare("DELETE FROM threads WHERE id = ?1")?;
        statement.execute(params![session_id])?;
    }
    if table_exists(&connection, "thread_dynamic_tools")? {
        let mut statement =
            connection.prepare("DELETE FROM thread_dynamic_tools WHERE thread_id = ?1")?;
        statement.execute(params![session_id])?;
    }

    Ok(())
}

fn table_exists(connection: &Connection, table_name: &str) -> Result<bool> {
    let exists = connection.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1 LIMIT 1",
        params![table_name],
        |_| Ok(()),
    );
    Ok(exists.is_ok())
}

fn prune_empty_parents(path: &Path, stop_at: &Path) -> Result<()> {
    let mut current = path.parent();
    while let Some(dir) = current {
        if dir == stop_at {
            break;
        }

        let mut entries =
            fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))?;
        if entries.next().is_some() {
            break;
        }

        fs::remove_dir(dir).with_context(|| format!("failed to remove {}", dir.display()))?;
        current = dir.parent();
    }
    Ok(())
}

fn codex_home() -> Result<PathBuf> {
    std::env::var("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|_| home_dir().map(|home| home.join(".codex")))
}

fn find_latest_state_db(codex_home: &Path) -> Result<Option<PathBuf>> {
    if !codex_home.exists() {
        return Ok(None);
    }

    let mut best: Option<(u64, PathBuf)> = None;
    for entry in fs::read_dir(codex_home)
        .with_context(|| format!("failed to read {}", codex_home.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension() != Some(OsStr::new("sqlite")) {
            continue;
        }

        let Some(file_name) = path.file_name().and_then(OsStr::to_str) else {
            continue;
        };
        let Some(version) = file_name
            .strip_prefix("state_")
            .and_then(|rest| rest.strip_suffix(".sqlite"))
            .and_then(|value| value.parse::<u64>().ok())
        else {
            continue;
        };

        match &best {
            Some((best_version, _)) if *best_version >= version => {}
            _ => best = Some((version, path)),
        }
    }

    Ok(best.map(|(_, path)| path))
}

fn read_oauth_tokens_in(codex_home: &Path) -> Result<OAuthTokens> {
    let store = CodexStore::from_home(codex_home.to_path_buf())?;
    read_oauth_tokens_from_path(&store.auth_path)
}

fn read_oauth_tokens_from_path(path: &Path) -> Result<OAuthTokens> {
    let raw = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let payload: OAuthPayload =
        serde_json::from_slice(&raw).context("failed to parse Codex auth.json")?;

    let access_token = payload.tokens.access_token.trim().to_string();
    if access_token.is_empty() {
        bail!("auth.json is missing tokens.access_token");
    }

    let account_id = payload.tokens.account_id.trim().to_string();
    if account_id.is_empty() {
        bail!("auth.json is missing tokens.account_id");
    }

    Ok(OAuthTokens {
        access_token,
        account_id,
    })
}

fn inferred_current_account(usage: &UsageReport) -> StoredAccount {
    let id = if !usage.account_id.trim().is_empty() {
        usage.account_id.clone()
    } else if !usage.email.trim().is_empty() {
        usage.email.clone()
    } else if !usage.user_id.trim().is_empty() {
        usage.user_id.clone()
    } else {
        "current".to_string()
    };

    StoredAccount {
        id,
        label: Some("current".to_string()),
        created_ms: 0,
    }
}

fn looks_like_usage_limit_error(message: &str) -> bool {
    let lowered = message.to_ascii_lowercase();
    lowered.contains("you've hit your usage limit")
        || lowered.contains("usage limit")
        || lowered.contains("insufficient_quota")
        || lowered.contains("insufficient quota")
        || lowered.contains("quota exceeded")
        || lowered.contains("quota")
}

async fn fetch_usage(client: &Client, tokens: OAuthTokens) -> Result<UsageReport> {
    let response = client
        .get(CODEX_USAGE_URL)
        .header("Authorization", format!("Bearer {}", tokens.access_token))
        .header("ChatGPT-Account-Id", &tokens.account_id)
        .header("User-Agent", "codex-cli")
        .send()
        .await
        .context("failed to request Codex usage endpoint")?;

    let status = response.status();
    let body = response
        .text()
        .await
        .context("failed to read Codex usage response body")?;

    if !status.is_success() {
        bail!(
            "usage endpoint returned status {}: {}",
            status.as_u16(),
            body.trim()
        );
    }

    let payload: RawUsageResponse =
        serde_json::from_str(&body).context("failed to decode Codex usage response")?;
    Ok(map_usage_report(payload, &tokens))
}

fn map_usage_report(payload: RawUsageResponse, tokens: &OAuthTokens) -> UsageReport {
    let mut report = UsageReport {
        provider: "codex".to_string(),
        account_id: if payload.account_id.trim().is_empty() {
            tokens.account_id.clone()
        } else {
            payload.account_id
        },
        user_id: payload.user_id,
        email: payload.email,
        plan: payload.plan_type,
        buckets: Vec::new(),
        credits: None,
    };

    if let Some(bucket) = payload.rate_limit {
        report.buckets.push(UsageBucket {
            name: "Rate limit".to_string(),
            allowed: bucket.allowed,
            limit_reached: bucket.limit_reached,
            windows: map_usage_windows(bucket),
        });
    }
    if let Some(bucket) = payload.code_review_rate_limit {
        report.buckets.push(UsageBucket {
            name: "Code review".to_string(),
            allowed: bucket.allowed,
            limit_reached: bucket.limit_reached,
            windows: map_usage_windows(bucket),
        });
    }
    if let Some(credits) = payload.credits {
        report.credits = Some(UsageCredits {
            has_credits: credits.has_credits,
            unlimited: credits.unlimited,
            balance: credits.balance.map(|value| match value {
                Value::String(text) => text,
                other => other.to_string(),
            }),
        });
    }

    report
}

fn map_usage_windows(bucket: RawUsageBucket) -> Vec<UsageWindow> {
    let mut windows = Vec::new();
    if let Some(window) = bucket.primary_window {
        windows.push(UsageWindow {
            name: "Primary".to_string(),
            used_percent: window.used_percent,
            window_seconds: window.limit_window_seconds,
            reset_after_seconds: window.reset_after_seconds,
            reset_at_unix: window.reset_at,
        });
    }
    if let Some(window) = bucket.secondary_window {
        windows.push(UsageWindow {
            name: "Secondary".to_string(),
            used_percent: window.used_percent,
            window_seconds: window.limit_window_seconds,
            reset_after_seconds: window.reset_after_seconds,
            reset_at_unix: window.reset_at,
        });
    }
    windows
}

fn patch_session_source_in(codex_home: &Path, session_id: &str) -> Result<()> {
    let Some(path) = find_session_file_in(codex_home, session_id)? else {
        return Ok(());
    };

    let original = fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let Some(first_newline) = original.iter().position(|byte| *byte == b'\n') else {
        return Ok(());
    };

    let first_line = &original[..first_newline];
    if !first_line
        .windows(br#""source":"exec""#.len())
        .any(|slice| slice == br#""source":"exec""#)
    {
        return Ok(());
    }

    let first_line =
        String::from_utf8(first_line.to_vec()).context("session header is not UTF-8")?;
    let patched = first_line
        .replace(r#""source":"exec""#, r#""source":"cli""#)
        .replace(
            r#""originator":"codex_exec""#,
            r#""originator":"codex_cli_rs""#,
        );

    if patched == first_line {
        return Ok(());
    }

    let mut output = patched.into_bytes();
    output.extend_from_slice(&original[first_newline..]);
    fs::write(&path, output).with_context(|| format!("failed to patch {}", path.display()))?;
    Ok(())
}

async fn stream_lines<R>(reader: R, tx: mpsc::Sender<String>) -> Result<()>
where
    R: AsyncRead + Unpin,
{
    let mut lines = tokio::io::BufReader::new(reader).lines();
    while let Some(line) = lines
        .next_line()
        .await
        .context("failed to read codex login output")?
    {
        if tx.send(line).await.is_err() {
            break;
        }
    }
    Ok(())
}

fn merge_command_output(stdout: &[u8], stderr: &[u8]) -> String {
    let mut lines = Vec::new();
    for buf in [stdout, stderr] {
        let text = String::from_utf8_lossy(buf);
        for line in strip_ansi_codes(&text).lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                lines.push(trimmed.to_string());
            }
        }
    }
    lines.join("\n")
}

fn summarize_auth_status(output: &str) -> String {
    output
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("WARNING:"))
        .map(str::to_string)
        .unwrap_or_else(|| "Not logged in".to_string())
}

fn strip_ansi_codes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if matches!(chars.peek(), Some('[')) {
                let _ = chars.next();
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(ch);
    }

    out
}

fn extract_url(line: &str) -> Option<String> {
    line.split_whitespace()
        .map(|part| part.trim_matches(|ch: char| ",.;:()[]{}".contains(ch)))
        .find(|part| part.starts_with("https://") || part.starts_with("http://"))
        .map(str::to_string)
}

fn extract_device_code(line: &str) -> Option<String> {
    line.split_whitespace()
        .map(|part| part.trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '-'))
        .find(|part| looks_like_device_code(part))
        .map(str::to_string)
}

fn looks_like_device_code(value: &str) -> bool {
    let mut groups = 0usize;
    for part in value.split('-') {
        if part.len() < 3
            || !part
                .chars()
                .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit())
        {
            return false;
        }
        groups += 1;
    }
    groups >= 2
}

fn extract_expiry_minutes(line: &str) -> Option<u64> {
    let lower = line.to_ascii_lowercase();
    let marker = "expires in ";
    let start = lower.find(marker)? + marker.len();
    let suffix = &lower[start..];
    let number = suffix.split_whitespace().next()?.parse::<u64>().ok()?;
    if suffix.contains("minute") {
        Some(number)
    } else {
        None
    }
}

fn is_user_prompt(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.starts_with('<') {
        return false;
    }
    if trimmed.starts_with("# AGENTS.md") || trimmed.starts_with("#AGENTS.md") {
        return false;
    }
    true
}

fn truncate_chars(text: &str, limit: usize) -> String {
    let truncated: String = text.chars().take(limit).collect();
    if text.chars().count() > limit {
        format!("{truncated}...")
    } else {
        truncated
    }
}

fn normalize_display_name(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    let first_non_empty_line = trimmed
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(trimmed);
    let collapsed = first_non_empty_line
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if collapsed.is_empty() {
        None
    } else {
        Some(collapsed)
    }
}

fn derived_session_name_from_text(text: &str) -> Option<String> {
    normalize_display_name(text).map(|line| truncate_chars(&line, 80))
}

fn session_summary_from_text(text: &str) -> Option<String> {
    normalize_display_name(text).map(|line| truncate_chars(&line, 60))
}

fn normalize_path(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        fs::canonicalize(path).with_context(|| format!("failed to canonicalize {}", path.display()))
    } else if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()
            .context("failed to resolve current directory")?
            .join(path))
    }
}

fn home_dir() -> Result<PathBuf> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| anyhow!("HOME is not set"))
}

#[derive(Debug, Deserialize)]
struct SessionLine {
    #[serde(rename = "type")]
    kind: String,
    payload: Value,
}

#[derive(Debug, Deserialize)]
struct TimestampedSessionLine {
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    payload: Value,
}

#[derive(Debug, Deserialize)]
struct SessionMeta {
    id: String,
    cwd: String,
}

#[derive(Debug, Deserialize)]
struct SessionIndexEntry {
    id: String,
    #[serde(default)]
    thread_name: String,
}

#[derive(Debug, Deserialize)]
struct HistorySessionLine {
    session_id: String,
    #[serde(default)]
    text: String,
}

#[derive(Debug, Deserialize)]
struct ResponseItem {
    role: Option<String>,
    #[serde(default)]
    content: Vec<ResponseContent>,
}

#[derive(Debug, Deserialize)]
struct ResponseContent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_client(codex_home: &Path, work_dir: &Path) -> CodexClient {
        CodexClient {
            bin: "codex".to_string(),
            work_dir: work_dir.to_path_buf(),
            extra_env: vec![format!("CODEX_HOME={}", codex_home.display())],
        }
    }

    #[test]
    fn device_auth_parser_extracts_prompt_from_ansi_output() {
        let mut parser = DeviceAuthParser::default();
        assert!(
            parser
                .ingest_line("\u{1b}[94mhttps://auth.openai.com/codex/device\u{1b}[0m")
                .is_none()
        );

        let prompt = parser
            .ingest_line("  \u{1b}[94mD0XJ-05VIZ\u{1b}[0m")
            .expect("expected device auth prompt");
        assert_eq!(
            prompt.verification_uri,
            "https://auth.openai.com/codex/device"
        );
        assert_eq!(prompt.user_code, "D0XJ-05VIZ");
        assert_eq!(prompt.expires_in_minutes, None);
    }

    #[test]
    fn device_auth_parser_extracts_expiry_minutes() {
        let mut parser = DeviceAuthParser::default();
        assert!(
            parser
                .ingest_line("2. Enter this one-time code (expires in 15 minutes)")
                .is_none()
        );
        assert!(
            parser
                .ingest_line("https://auth.openai.com/codex/device")
                .is_none()
        );

        let prompt = parser
            .ingest_line("ABCD-12345")
            .expect("expected device auth prompt");
        assert_eq!(prompt.expires_in_minutes, Some(15));
    }

    #[test]
    fn effective_settings_reads_codex_home_defaults() -> Result<()> {
        let tmp = TempDir::new()?;
        let codex_home = tmp.path().join("codex-home");
        let work_dir = tmp.path().join("work");
        fs::create_dir_all(&codex_home)?;
        fs::create_dir_all(&work_dir)?;
        fs::write(
            codex_home.join("config.toml"),
            "model = \"gpt-5.4\"\nmodel_reasoning_effort = \"xhigh\"\n",
        )?;

        let resolved =
            test_client(&codex_home, &work_dir).effective_settings(&RuntimeSettings::default())?;
        assert_eq!(resolved.codex_home, codex_home);
        assert_eq!(resolved.model.as_deref(), Some("gpt-5.4"));
        assert_eq!(resolved.reasoning_effort.as_deref(), Some("xhigh"));
        Ok(())
    }

    #[test]
    fn effective_settings_prefers_project_and_runtime_overrides() -> Result<()> {
        let tmp = TempDir::new()?;
        let codex_home = tmp.path().join("codex-home");
        let project_root = tmp.path().join("project");
        let project_override = project_root.join("nested");
        let work_dir = project_override.join("child");
        fs::create_dir_all(&codex_home)?;
        fs::create_dir_all(&work_dir)?;
        fs::write(
            codex_home.join("config.toml"),
            format!(
                "model = \"gpt-root\"\nmodel_reasoning_effort = \"medium\"\n[projects.\"{}\"]\nmodel = \"gpt-project\"\n[projects.\"{}\"]\nmodel = \"gpt-specific\"\nmodel_reasoning_effort = \"high\"\n",
                project_root.display(),
                project_override.display()
            ),
        )?;

        let client = test_client(&codex_home, &work_dir);
        let resolved = client.effective_settings(&RuntimeSettings::default())?;
        assert_eq!(resolved.model.as_deref(), Some("gpt-specific"));
        assert_eq!(resolved.reasoning_effort.as_deref(), Some("high"));

        let runtime = RuntimeSettings::new(
            Some("gpt-runtime".to_string()),
            Some("xhigh".to_string()),
            None,
        );
        let overridden = client.effective_settings(&runtime)?;
        assert_eq!(overridden.model.as_deref(), Some("gpt-runtime"));
        assert_eq!(overridden.reasoning_effort.as_deref(), Some("xhigh"));
        Ok(())
    }

    #[test]
    fn trust_dir_uses_work_dir_by_default() -> Result<()> {
        let tmp = TempDir::new()?;
        let codex_home = tmp.path().join("codex-home");
        let work_dir = tmp.path().join("work");
        fs::create_dir_all(&work_dir)?;

        let result = test_client(&codex_home, &work_dir).trust_dir(None)?;
        assert_eq!(result.config_path, codex_home.join("config.toml"));
        assert_eq!(result.trusted_dir, canonicalize_or_clone(&work_dir));
        assert!(!result.already_trusted);

        let raw = fs::read_to_string(codex_home.join("config.toml"))?;
        let config: TomlValue = toml::from_str(&raw)?;
        assert_eq!(
            config
                .get("projects")
                .and_then(TomlValue::as_table)
                .and_then(|projects| projects.get(&result.trusted_dir.display().to_string()))
                .and_then(TomlValue::as_table)
                .and_then(|project| project.get("trust_level"))
                .and_then(TomlValue::as_str),
            Some("trusted")
        );
        Ok(())
    }

    #[test]
    fn trust_dir_updates_existing_project() -> Result<()> {
        let tmp = TempDir::new()?;
        let codex_home = tmp.path().join("codex-home");
        let work_dir = tmp.path().join("work");
        fs::create_dir_all(&codex_home)?;
        fs::create_dir_all(&work_dir)?;
        fs::write(
            codex_home.join("config.toml"),
            format!(
                "model = \"gpt-5.4\"\n[projects.\"{}\"]\ntrust_level = \"untrusted\"\n",
                canonicalize_or_clone(&work_dir).display()
            ),
        )?;

        let result = test_client(&codex_home, &work_dir).trust_dir(None)?;
        assert!(!result.already_trusted);

        let second = test_client(&codex_home, &work_dir).trust_dir(None)?;
        assert!(second.already_trusted);
        Ok(())
    }

    #[test]
    fn list_sessions_filters_by_cwd() -> Result<()> {
        let tmp = TempDir::new()?;
        let session_file = tmp.path().join("session.jsonl");
        fs::write(
            &session_file,
            r#"{"type":"session_meta","payload":{"id":"abc123","cwd":"/tmp/work"}}
{"type":"response_item","payload":{"role":"user","content":[{"type":"input_text","text":"hello"}]}}
{"type":"response_item","payload":{"role":"assistant","content":[{"type":"output_text","text":"world"}]}}
"#,
        )?;

        let summary = parse_session_file(&session_file, Path::new("/tmp/work"))?;
        let summary = summary.expect("expected session summary");
        assert_eq!(summary.id, "abc123");
        assert_eq!(summary.display_name.as_deref(), Some("hello"));
        assert_eq!(summary.summary, "hello");
        assert_eq!(summary.message_count, 2);
        Ok(())
    }

    #[test]
    fn list_sessions_loads_display_names_from_codex_metadata() -> Result<()> {
        let tmp = TempDir::new()?;
        let codex_home = tmp.path();
        let work_dir = Path::new("/tmp/work");
        let sessions_dir = codex_home.join("sessions/2026/03/23");
        fs::create_dir_all(&sessions_dir)?;

        fs::write(
            sessions_dir.join("index.jsonl"),
            r#"{"type":"session_meta","payload":{"id":"session-index","cwd":"/tmp/work"}}
{"type":"response_item","payload":{"role":"user","content":[{"type":"input_text","text":"rollout fallback"}]}}
"#,
        )?;
        fs::write(
            sessions_dir.join("state.jsonl"),
            r#"{"type":"session_meta","payload":{"id":"session-state","cwd":"/tmp/work"}}
{"type":"response_item","payload":{"role":"user","content":[{"type":"input_text","text":"rollout state fallback"}]}}
"#,
        )?;
        fs::write(
            sessions_dir.join("derived.jsonl"),
            r#"{"type":"session_meta","payload":{"id":"session-derived","cwd":"/tmp/work"}}
{"type":"response_item","payload":{"role":"user","content":[{"type":"input_text","text":"derived from rollout"}]}}
"#,
        )?;
        fs::write(
            sessions_dir.join("history.jsonl"),
            r#"{"type":"session_meta","payload":{"id":"session-history","cwd":"/tmp/work"}}
"#,
        )?;

        fs::write(
            codex_home.join("session_index.jsonl"),
            r#"{"id":"session-index","thread_name":"indexed title","updated_at":"2026-03-23T14:26:41.943Z"}
"#,
        )?;
        fs::write(
            codex_home.join("history.jsonl"),
            r#"{"session_id":"session-history","ts":1774283201,"text":"history fallback title"}
"#,
        )?;

        let db_path = codex_home.join("state_7.sqlite");
        let connection = Connection::open(&db_path)?;
        connection.execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, title TEXT);")?;
        connection.execute(
            "INSERT INTO threads (id, title) VALUES (?1, ?2)",
            params!["session-state", "state title"],
        )?;

        let sessions = list_sessions_in(work_dir, codex_home)?;
        let sessions_by_id = sessions
            .into_iter()
            .map(|session| (session.id.clone(), session))
            .collect::<HashMap<_, _>>();

        assert_eq!(
            sessions_by_id
                .get("session-index")
                .and_then(|session| session.display_name.as_deref()),
            Some("indexed title")
        );
        assert_eq!(
            sessions_by_id
                .get("session-state")
                .and_then(|session| session.display_name.as_deref()),
            Some("state title")
        );
        assert_eq!(
            sessions_by_id
                .get("session-derived")
                .and_then(|session| session.display_name.as_deref()),
            Some("derived from rollout")
        );
        assert_eq!(
            sessions_by_id
                .get("session-history")
                .and_then(|session| session.display_name.as_deref()),
            Some("history fallback title")
        );
        Ok(())
    }

    #[test]
    fn delete_session_artifacts_removes_all_local_state() -> Result<()> {
        let tmp = TempDir::new()?;
        let codex_home = tmp.path();
        let target_id = "session-target";
        let keep_id = "session-keep";

        let target_dir = codex_home.join("sessions/2026/03/23/target");
        let keep_dir = codex_home.join("sessions/2026/03/23");
        fs::create_dir_all(&target_dir)?;
        fs::create_dir_all(&keep_dir)?;

        let target_file = target_dir.join("rollout-target.jsonl");
        let keep_file = keep_dir.join("rollout-keep.jsonl");
        fs::write(
            &target_file,
            format!(
                "{{\"timestamp\":\"2026-03-23T14:26:41.943Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{target_id}\",\"cwd\":\"/tmp/work\"}}}}\n"
            ),
        )?;
        fs::write(
            &keep_file,
            format!(
                "{{\"timestamp\":\"2026-03-23T14:26:41.943Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{keep_id}\",\"cwd\":\"/tmp/work\"}}}}\n"
            ),
        )?;

        fs::write(
            codex_home.join("session_index.jsonl"),
            format!(
                "{{\"id\":\"{target_id}\",\"thread_name\":\"target\",\"updated_at\":\"2026-03-23T14:26:41.943Z\"}}\n{{\"id\":\"{keep_id}\",\"thread_name\":\"keep\",\"updated_at\":\"2026-03-23T14:26:41.943Z\"}}\n"
            ),
        )?;
        fs::write(
            codex_home.join("history.jsonl"),
            format!(
                "{{\"session_id\":\"{target_id}\",\"ts\":1774283201,\"text\":\"target text\"}}\n{{\"session_id\":\"{keep_id}\",\"ts\":1774283202,\"text\":\"keep text\"}}\n"
            ),
        )?;

        let db_path = codex_home.join("state_7.sqlite");
        let connection = Connection::open(&db_path)?;
        connection.execute_batch(
            "
            CREATE TABLE threads (id TEXT PRIMARY KEY, title TEXT);
            CREATE TABLE thread_dynamic_tools (thread_id TEXT, name TEXT);
            ",
        )?;
        connection.execute(
            "INSERT INTO threads (id, title) VALUES (?1, 'target')",
            params![target_id],
        )?;
        connection.execute(
            "INSERT INTO threads (id, title) VALUES (?1, 'keep')",
            params![keep_id],
        )?;
        connection.execute(
            "INSERT INTO thread_dynamic_tools (thread_id, name) VALUES (?1, 'tool')",
            params![target_id],
        )?;
        connection.execute(
            "INSERT INTO thread_dynamic_tools (thread_id, name) VALUES (?1, 'tool')",
            params![keep_id],
        )?;

        delete_session_artifacts_in(codex_home, target_id)?;

        assert!(!target_file.exists());
        assert!(!target_dir.exists());
        assert!(keep_file.exists());

        let index = fs::read_to_string(codex_home.join("session_index.jsonl"))?;
        assert!(!index.contains(target_id));
        assert!(index.contains(keep_id));

        let history = fs::read_to_string(codex_home.join("history.jsonl"))?;
        assert!(!history.contains(target_id));
        assert!(history.contains(keep_id));

        let connection = Connection::open(&db_path)?;
        let target_threads: i64 = connection.query_row(
            "SELECT COUNT(*) FROM threads WHERE id = ?1",
            params![target_id],
            |row| row.get(0),
        )?;
        let keep_threads: i64 = connection.query_row(
            "SELECT COUNT(*) FROM threads WHERE id = ?1",
            params![keep_id],
            |row| row.get(0),
        )?;
        let target_tools: i64 = connection.query_row(
            "SELECT COUNT(*) FROM thread_dynamic_tools WHERE thread_id = ?1",
            params![target_id],
            |row| row.get(0),
        )?;
        let keep_tools: i64 = connection.query_row(
            "SELECT COUNT(*) FROM thread_dynamic_tools WHERE thread_id = ?1",
            params![keep_id],
            |row| row.get(0),
        )?;

        assert_eq!(target_threads, 0);
        assert_eq!(keep_threads, 1);
        assert_eq!(target_tools, 0);
        assert_eq!(keep_tools, 1);
        Ok(())
    }
}
