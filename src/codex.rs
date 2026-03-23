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
use serde_json::{Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use walkdir::WalkDir;

use crate::config::CodexConfig;

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
pub struct SessionSummary {
    pub id: String,
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

#[derive(Debug)]
pub enum TurnEvent {
    Status(String),
    FinalText(String),
    ThreadId,
    Error(String),
}

#[derive(Debug)]
pub struct TurnOutcome {
    pub thread_id: Option<String>,
    pub final_text: Option<String>,
}

pub struct SpawnedTurn {
    pub events: mpsc::Receiver<TurnEvent>,
    pub join: JoinHandle<Result<TurnOutcome>>,
    pub pid: Arc<AtomicU32>,
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
struct EventParser {
    thread_id: Option<String>,
    pending_messages: Vec<String>,
    final_text: Option<String>,
    fatal_error: Option<String>,
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

    pub fn spawn_turn(
        &self,
        thread_id: Option<&str>,
        settings: &RuntimeSettings,
        prompt: &str,
        cancel: CancellationToken,
    ) -> Result<SpawnedTurn> {
        if prompt.trim().is_empty() {
            bail!("prompt is empty");
        }

        let args = build_exec_args(&self.work_dir, thread_id, settings, prompt);
        let mut command = self.command();
        command
            .args(&args)
            .current_dir(&self.work_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let mut child = command.spawn().with_context(|| {
            format!(
                "failed to start codex process: {} {}",
                self.bin,
                args.join(" ")
            )
        })?;

        let pid = Arc::new(AtomicU32::new(child.id().unwrap_or_default()));
        let stdout = child
            .stdout
            .take()
            .context("codex stdout pipe is unavailable")?;
        let stderr = child
            .stderr
            .take()
            .context("codex stderr pipe is unavailable")?;
        let child = Arc::new(Mutex::new(child));

        let (tx, rx) = mpsc::channel(64);
        let join = {
            let child = Arc::clone(&child);
            let pid_ref = Arc::clone(&pid);
            let codex_home = self.codex_home().ok();
            tokio::spawn(async move {
                let stderr_task = tokio::spawn(async move {
                    let mut buf = String::new();
                    let mut reader = tokio::io::BufReader::new(stderr);
                    reader
                        .read_to_string(&mut buf)
                        .await
                        .context("failed to read codex stderr")?;
                    Ok::<String, anyhow::Error>(buf)
                });

                let killer_task = {
                    let child = Arc::clone(&child);
                    let cancel = cancel.clone();
                    tokio::spawn(async move {
                        cancel.cancelled().await;
                        let mut child = child.lock().await;
                        let _ = child.start_kill();
                    })
                };

                let mut parser = EventParser::default();
                let mut lines = tokio::io::BufReader::new(stdout).lines();
                while let Some(line) = lines
                    .next_line()
                    .await
                    .context("failed to read codex stdout line")?
                {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
                        for event in parser.ingest(&value) {
                            if tx.send(event).await.is_err() {
                                break;
                            }
                        }
                    }
                }

                let status = {
                    let mut child = child.lock().await;
                    let status = child.wait().await.context("failed to wait for codex")?;
                    pid_ref.store(0, Ordering::Relaxed);
                    status
                };

                killer_task.abort();
                let stderr = stderr_task.await.context("stderr task join failed")??;

                if parser.final_text.is_none() {
                    if let Some(text) = parser.flush_pending() {
                        parser.final_text = Some(text.clone());
                        let _ = tx.send(TurnEvent::FinalText(text)).await;
                    }
                }

                drop(tx);

                if let (Some(thread_id), Some(codex_home)) =
                    (&parser.thread_id, codex_home.as_deref())
                {
                    let _ = patch_session_source_in(codex_home, thread_id);
                }

                if cancel.is_cancelled() {
                    let message = "request cancelled".to_string();
                    return Err(anyhow!(message));
                }

                if let Some(message) = parser.fatal_error {
                    return Err(anyhow!(message));
                }

                if !status.success() {
                    let stderr = stderr.trim();
                    if !stderr.is_empty() {
                        return Err(anyhow!(stderr.to_string()));
                    }
                    return Err(anyhow!("codex exited with status {status}"));
                }

                Ok(TurnOutcome {
                    thread_id: parser.thread_id,
                    final_text: parser.final_text,
                })
            })
        };

        Ok(SpawnedTurn {
            events: rx,
            join,
            pid,
        })
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        list_sessions_in(&self.work_dir, &self.codex_home()?)
    }

    pub fn get_session_history(&self, session_id: &str, limit: usize) -> Result<Vec<HistoryEntry>> {
        get_session_history_in(session_id, limit, &self.codex_home()?)
    }

    pub async fn get_usage(&self) -> Result<UsageReport> {
        let tokens = read_oauth_tokens_in(&self.codex_home()?)?;
        fetch_usage(&Client::new(), tokens).await
    }

    pub fn delete_session(&self, session_id: &str) -> Result<()> {
        delete_session_artifacts_in(&self.codex_home()?, session_id)
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

    pub async fn logout(&self) -> Result<AuthStatus> {
        let output = self.run_command_capture(&["logout"]).await?;
        if !output.status.success() && summarize_auth_status(&output.output) != "Not logged in" {
            return Err(anyhow!(
                "failed to log out: {}",
                if output.output.is_empty() {
                    format!("codex logout exited with {}", output.status)
                } else {
                    output.output
                }
            ));
        }

        self.login_status().await
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

impl EventParser {
    fn ingest(&mut self, raw: &Value) -> Vec<TurnEvent> {
        let Some(event_type) = raw.get("type").and_then(Value::as_str) else {
            return Vec::new();
        };

        match event_type {
            "thread.started" => raw
                .get("thread_id")
                .and_then(Value::as_str)
                .map(|thread_id| {
                    self.thread_id = Some(thread_id.to_string());
                    vec![TurnEvent::ThreadId]
                })
                .unwrap_or_default(),
            "turn.started" => {
                self.pending_messages.clear();
                Vec::new()
            }
            "item.started" => raw
                .get("item")
                .and_then(Value::as_object)
                .and_then(describe_item_started)
                .map(TurnEvent::Status)
                .into_iter()
                .collect(),
            "item.completed" => raw
                .get("item")
                .and_then(Value::as_object)
                .map(|item| self.handle_item_completed(item))
                .unwrap_or_default(),
            "turn.completed" => self
                .flush_pending()
                .map(|text| {
                    self.final_text = Some(text.clone());
                    vec![TurnEvent::FinalText(text)]
                })
                .unwrap_or_default(),
            "turn.failed" => {
                let message = raw
                    .get("error")
                    .and_then(Value::as_object)
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("turn failed")
                    .to_string();
                self.fatal_error = Some(message.clone());
                vec![TurnEvent::Error(message)]
            }
            "error" => raw
                .get("message")
                .and_then(Value::as_str)
                .map(|message| {
                    if message.contains("Reconnecting") || message.contains("Falling back") {
                        TurnEvent::Status(message.to_string())
                    } else {
                        TurnEvent::Error(message.to_string())
                    }
                })
                .into_iter()
                .collect(),
            _ => Vec::new(),
        }
    }

    fn handle_item_completed(&mut self, item: &Map<String, Value>) -> Vec<TurnEvent> {
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
        match item_type {
            "reasoning" => extract_item_text(item, "summary", "summary_text")
                .map(TurnEvent::Status)
                .into_iter()
                .collect(),
            "agent_message" | "message" => {
                if let Some(text) = extract_item_text(item, "content", "output_text") {
                    self.pending_messages.push(text);
                }
                Vec::new()
            }
            "command_execution" | "function_call" | "function_call_output" | "error" => Vec::new(),
            other => tool_display_name(other)
                .map(|tool_name| {
                    let input = extract_tool_input(item);
                    if input.is_empty() {
                        TurnEvent::Status(tool_name.to_string())
                    } else {
                        TurnEvent::Status(format!("{tool_name}: {input}"))
                    }
                })
                .into_iter()
                .collect(),
        }
    }

    fn flush_pending(&mut self) -> Option<String> {
        if self.pending_messages.is_empty() {
            return None;
        }
        let text = self.pending_messages.join("\n\n");
        self.pending_messages.clear();
        Some(text)
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

fn build_exec_args(
    work_dir: &Path,
    thread_id: Option<&str>,
    settings: &RuntimeSettings,
    prompt: &str,
) -> Vec<String> {
    let mut args = if thread_id.is_some() {
        vec![
            "exec".to_string(),
            "resume".to_string(),
            "--skip-git-repo-check".to_string(),
        ]
    } else {
        vec!["exec".to_string(), "--skip-git-repo-check".to_string()]
    };

    match settings.mode.as_str() {
        "auto-edit" | "full-auto" => args.push("--full-auto".to_string()),
        "yolo" => args.push("--dangerously-bypass-approvals-and-sandbox".to_string()),
        _ => {}
    }

    if let Some(model) = normalize_optional(settings.model.clone()) {
        args.push("--model".to_string());
        args.push(model);
    }
    if let Some(effort) = normalize_reasoning(settings.reasoning_effort.clone()) {
        args.push("-c".to_string());
        args.push(format!("model_reasoning_effort={effort:?}"));
    }

    if let Some(thread_id) = thread_id {
        args.push(thread_id.to_string());
        args.push("--json".to_string());
        args.push(prompt.to_string());
    } else {
        args.push("--json".to_string());
        args.push("--cd".to_string());
        args.push(work_dir.display().to_string());
        args.push(prompt.to_string());
    }

    args
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

fn describe_item_started(item: &Map<String, Value>) -> Option<String> {
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
    match item_type {
        "reasoning" | "message" | "agent_message" => None,
        "command_execution" => item
            .get("command")
            .and_then(Value::as_str)
            .map(|command| format!("Bash: {command}"))
            .or_else(|| Some("Bash".to_string())),
        "function_call" => item
            .get("name")
            .and_then(Value::as_str)
            .map(|name| format!("Tool: {name}"))
            .or_else(|| Some("Tool".to_string())),
        other => tool_display_name(other).map(|name| {
            let input = extract_tool_input(item);
            if input.is_empty() {
                name.to_string()
            } else {
                format!("{name}: {input}")
            }
        }),
    }
}

fn tool_display_name(item_type: &str) -> Option<&'static str> {
    match item_type {
        "web_search" => Some("WebSearch"),
        "file_search" => Some("FileSearch"),
        "code_interpreter" => Some("CodeInterpreter"),
        "computer_use" => Some("ComputerUse"),
        "mcp_tool" => Some("MCP"),
        _ => None,
    }
}

fn extract_item_text(
    item: &Map<String, Value>,
    array_field: &str,
    element_type: &str,
) -> Option<String> {
    if let Some(elements) = item.get(array_field).and_then(Value::as_array) {
        let parts = elements
            .iter()
            .filter_map(Value::as_object)
            .filter(|element| {
                element_type.is_empty()
                    || element
                        .get("type")
                        .and_then(Value::as_str)
                        .is_some_and(|kind| kind == element_type)
            })
            .filter_map(|element| element.get("text").and_then(Value::as_str))
            .filter(|text| !text.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        if !parts.is_empty() {
            return Some(parts.join("\n"));
        }
    }

    item.get("text")
        .and_then(Value::as_str)
        .and_then(|text| (!text.is_empty()).then(|| text.to_string()))
}

fn extract_tool_input(item: &Map<String, Value>) -> String {
    if let Some(action) = item.get("action").and_then(Value::as_object) {
        if let Some(queries) = action.get("queries").and_then(Value::as_array) {
            let parts = queries
                .iter()
                .filter_map(|query| match query {
                    Value::String(text) => Some(text.clone()),
                    Value::Object(object) => object
                        .get("query")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    _ => None,
                })
                .filter(|text| !text.trim().is_empty())
                .collect::<Vec<_>>();
            if !parts.is_empty() {
                return parts.join("\n");
            }
        }

        if let Some(query) = action.get("query").and_then(Value::as_str) {
            if !query.trim().is_empty() {
                return query.to_string();
            }
        }
    }

    item.get("query")
        .and_then(Value::as_str)
        .or_else(|| item.get("name").and_then(Value::as_str))
        .unwrap_or_default()
        .to_string()
}

fn list_sessions_in(work_dir: &Path, codex_home: &Path) -> Result<Vec<SessionSummary>> {
    let work_dir = normalize_path(work_dir)?;
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

fn parse_session_file(path: &Path, filter_cwd: &Path) -> Result<Option<SessionSummary>> {
    let file = fs::File::open(path)
        .with_context(|| format!("failed to open session transcript {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to stat {}", path.display()))?;
    let mut session_id = None;
    let mut session_cwd = None;
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
                                summary = content.text;
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

    if summary.chars().count() > 60 {
        summary = summary.chars().take(60).collect::<String>() + "...";
    }

    Ok(Some(SessionSummary {
        id: session_id,
        summary,
        message_count,
        modified_at: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
    }))
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
    let raw = fs::read(&store.auth_path)
        .with_context(|| format!("failed to read {}", store.auth_path.display()))?;
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

    #[test]
    fn parser_ignores_missing_item() {
        let mut parser = EventParser::default();
        let raw: Value = serde_json::json!({
            "type": "item.completed",
            "item": null
        });
        let events = parser.ingest(&raw);
        assert!(events.is_empty());
        assert!(parser.final_text.is_none());
    }

    #[test]
    fn parser_collects_agent_message_text() {
        let mut parser = EventParser::default();
        let events = parser.ingest(&serde_json::json!({
            "type": "item.completed",
            "item": {
                "type": "agent_message",
                "content": [
                    {"type": "output_text", "text": "hello"},
                    {"type": "output_text", "text": "world"}
                ]
            }
        }));
        assert!(events.is_empty());

        let events = parser.ingest(&serde_json::json!({
            "type": "turn.completed"
        }));
        assert!(
            matches!(events.as_slice(), [TurnEvent::FinalText(text)] if text == "hello\nworld")
        );
    }

    #[test]
    fn parser_extracts_tool_queries_without_panicking() {
        let mut parser = EventParser::default();
        let events = parser.ingest(&serde_json::json!({
            "type": "item.completed",
            "item": {
                "type": "web_search",
                "action": {
                    "queries": [
                        {"query": "rust telegram"},
                        "codex cli"
                    ]
                }
            }
        }));
        assert!(
            matches!(events.as_slice(), [TurnEvent::Status(text)] if text.contains("rust telegram") && text.contains("codex cli"))
        );
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
        assert_eq!(summary.summary, "hello");
        assert_eq!(summary.message_count, 2);
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
