use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU32, Ordering},
};
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Map, Value};
use session::{
    BackendKind, RuntimeSettings, SessionEvent, SessionEventPayload, SessionOrigin, SessionOutcome,
    SessionRequest,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use walkdir::WalkDir;

use crate::{
    Cli, CliSession, MessageRole, SessionId, SpawnedRun, configure_command_process_group,
    spawn_process_killer,
};

type SessionSummary = (SessionId, Option<String>);
type HistoryEntry = (MessageRole, String);

#[derive(Debug, Clone)]
struct CodexCli {
    bin: String,
    extra_env: Vec<String>,
}

impl CodexCli {
    fn new(bin: impl Into<String>, extra_env: Vec<String>) -> Self {
        Self {
            bin: bin.into(),
            extra_env,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.bin);
        if !self.extra_env.is_empty() {
            command.envs(
                self.extra_env
                    .iter()
                    .filter_map(|entry| parse_env_pair(entry)),
            );
        }
        command
    }

    fn codex_home(&self) -> Result<PathBuf> {
        if let Some(value) = self
            .extra_env
            .iter()
            .rev()
            .filter_map(|entry| parse_env_pair(entry))
            .find_map(|(name, value)| (name == "CODEX_HOME").then_some(value))
            .filter(|value| !value.trim().is_empty())
        {
            return Ok(PathBuf::from(value));
        }

        std::env::var("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|_| {
                std::env::var("HOME")
                    .map(PathBuf::from)
                    .map(|home| home.join(".codex"))
            })
            .map_err(|_| anyhow!("HOME is not set"))
    }

    fn spawn_turn_in(
        &self,
        work_dir: &Path,
        codex_session_id: Option<&str>,
        settings: &RuntimeSettings,
        prompt: &str,
        cancel: CancellationToken,
    ) -> Result<SpawnedTurn> {
        if prompt.trim().is_empty() {
            bail!("prompt is empty");
        }

        let args = build_exec_args(work_dir, codex_session_id, settings, prompt);
        let mut command = self.command();
        command
            .args(&args)
            .current_dir(work_dir)
            .env("CODEX_SESSION_WORKDIR", work_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        configure_command_process_group(&mut command);

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
        let child = Arc::new(tokio::sync::Mutex::new(child));

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

                let killer_task =
                    spawn_process_killer(Arc::clone(&child), Arc::clone(&pid_ref), cancel.clone());

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

                if let (Some(codex_session_id), Some(codex_home)) =
                    (&parser.codex_session_id, codex_home.as_deref())
                {
                    let _ = patch_session_source_in(codex_home, codex_session_id);
                }

                if cancel.is_cancelled() {
                    return Err(anyhow!("request cancelled"));
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
                    codex_session_id: parser.codex_session_id,
                    final_text: parser.final_text,
                })
            })
        };

        Ok(SpawnedTurn { events: rx, join })
    }
}

pub(super) fn spawn(
    bin: impl Into<String>,
    extra_env: Vec<String>,
    workdir: &Path,
) -> Result<Box<dyn CliSession>> {
    CodexCli::new(bin, extra_env).spawn(workdir)
}

impl Cli for CodexCli {
    fn spawn(&self, workdir: &Path) -> Result<Box<dyn CliSession>> {
        let workdir = normalize_path(workdir)?;
        let codex_home = self.codex_home()?;
        Ok(Box::new(CodexCliSession {
            cli: self.clone(),
            workdir: workdir.clone(),
            codex_home: codex_home.clone(),
            state: Arc::new(Mutex::new(CodexCliSessionState {
                current_session: list_sessions_in(&workdir, &codex_home)?
                    .into_iter()
                    .next()
                    .map(|session| session.0),
                active_cancel: None,
            })),
        }))
    }
}

struct CodexCliSession {
    cli: CodexCli,
    workdir: PathBuf,
    codex_home: PathBuf,
    state: Arc<Mutex<CodexCliSessionState>>,
}

struct CodexCliSessionState {
    current_session: Option<SessionId>,
    active_cancel: Option<CancellationToken>,
}

impl CliSession for CodexCliSession {
    fn send_prompt(&mut self, request: SessionRequest) -> Result<SpawnedRun> {
        if request.prompt.trim().is_empty() {
            bail!("prompt is empty");
        }

        let settings = self.effective_runtime_settings(&request.settings)?;
        let selected_session_id = {
            let state = self
                .state
                .lock()
                .map_err(|_| anyhow!("codex session state lock poisoned"))?;
            request.native_session_id.clone().or_else(|| {
                state
                    .current_session
                    .as_ref()
                    .map(|session| session.as_str().to_string())
            })
        };
        let cancel = CancellationToken::new();
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow!("codex session state lock poisoned"))?;
            if state.active_cancel.is_some() {
                bail!("codex run already active");
            }
            state.active_cancel = Some(cancel.clone());
        }

        let spawned = match self.cli.spawn_turn_in(
            &self.workdir,
            selected_session_id.as_deref(),
            &settings,
            &request.prompt,
            cancel,
        ) {
            Ok(spawned) => spawned,
            Err(err) => {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| anyhow!("codex session state lock poisoned"))?;
                state.active_cancel = None;
                return Err(err);
            }
        };

        let (tx, rx) = mpsc::channel(64);
        let manager_session_id = request.manager_session_id.clone();
        let run_id = request.run_id.clone();
        let state = Arc::clone(&self.state);
        let join = tokio::spawn(async move {
            let mut sequence = 0u64;
            let mut events = spawned.events;
            while let Some(event) = events.recv().await {
                sequence += 1;
                let session_event = map_turn_event(
                    &manager_session_id,
                    BackendKind::Codex,
                    &run_id,
                    sequence,
                    event,
                );
                if tx.send(session_event).await.is_err() {
                    break;
                }
            }
            drop(tx);

            let result = match spawned.join.await {
                Ok(Ok(outcome)) => Ok(SessionOutcome {
                    native_session_id: outcome
                        .codex_session_id
                        .clone()
                        .or(selected_session_id.clone()),
                    final_text: outcome.final_text,
                }),
                Ok(Err(err)) => Err(err),
                Err(err) => Err(anyhow!("codex session task join failed: {err}")),
            };

            if let Ok(mut state) = state.lock() {
                state.active_cancel = None;
                match &result {
                    Ok(outcome) => {
                        state.current_session =
                            outcome.native_session_id.clone().map(SessionId::from);
                    }
                    Err(_) => {
                        if state.current_session.is_none() {
                            state.current_session =
                                selected_session_id.clone().map(SessionId::from);
                        }
                    }
                }
            }

            result
        });

        Ok((rx, join))
    }

    fn shutdown(&mut self) -> Result<()> {
        let cancel = self
            .state
            .lock()
            .map_err(|_| anyhow!("codex session state lock poisoned"))?
            .active_cancel
            .clone()
            .ok_or_else(|| anyhow!("no active codex run"))?;
        cancel.cancel();
        Ok(())
    }

    fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        list_sessions_in(&self.workdir, &self.codex_home)
    }

    fn current_native_session_id(&self) -> Result<Option<String>> {
        Ok(self
            .state
            .lock()
            .map_err(|_| anyhow!("codex session state lock poisoned"))?
            .current_session
            .as_ref()
            .map(|session| session.as_str().to_string()))
    }

    fn switch_session(&mut self, session_id: &SessionId) -> Result<()> {
        let sessions = self.list_sessions()?;
        if sessions.iter().any(|session| &session.0 == session_id) {
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow!("codex session state lock poisoned"))?;
            state.current_session = Some(session_id.clone());
            Ok(())
        } else {
            bail!("codex session not found: {session_id}");
        }
    }

    fn delete_session(&mut self, session_id: &SessionId) -> Result<()> {
        delete_session_artifacts_in(&self.codex_home, session_id.as_str())?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("codex session state lock poisoned"))?;
        if state.current_session.as_ref() == Some(session_id) {
            state.current_session = None;
        }
        Ok(())
    }

    fn get_history(&self, limit: usize) -> Result<Vec<HistoryEntry>> {
        let session_id = self
            .state
            .lock()
            .map_err(|_| anyhow!("codex session state lock poisoned"))?
            .current_session
            .clone()
            .ok_or_else(|| anyhow!("no active codex session selected"))?;
        get_session_history_in(session_id.as_str(), limit, &self.codex_home)
    }

    fn effective_runtime_settings(&self, requested: &RuntimeSettings) -> Result<RuntimeSettings> {
        let defaults = load_codex_defaults(&self.codex_home, &self.workdir)?;
        Ok(RuntimeSettings::new(
            normalize_optional(requested.model.clone()).or(defaults.model),
            normalize_reasoning(requested.reasoning_effort.clone()).or(defaults.reasoning_effort),
            Some(requested.mode.clone()),
        ))
    }
}

#[derive(Debug)]
enum TurnEvent {
    Status(String),
    FinalText(String),
    CodexSessionId(String),
    Error(String),
}

#[derive(Debug)]
struct TurnOutcome {
    codex_session_id: Option<String>,
    final_text: Option<String>,
}

struct SpawnedTurn {
    events: mpsc::Receiver<TurnEvent>,
    join: JoinHandle<Result<TurnOutcome>>,
}

#[derive(Default)]
struct EventParser {
    codex_session_id: Option<String>,
    pending_messages: Vec<String>,
    final_text: Option<String>,
    fatal_error: Option<String>,
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

fn map_turn_event(
    manager_session_id: &str,
    backend: BackendKind,
    run_id: &str,
    sequence: u64,
    event: TurnEvent,
) -> SessionEvent {
    match event {
        TurnEvent::Status(message) => SessionEvent::new(
            manager_session_id,
            backend,
            sequence,
            SessionOrigin::Adapter,
            SessionEventPayload::Status { message },
        )
        .with_run_id(run_id.to_string()),
        TurnEvent::FinalText(text) => SessionEvent::new(
            manager_session_id,
            backend,
            sequence,
            SessionOrigin::Adapter,
            SessionEventPayload::AssistantTextFinal { text },
        )
        .with_run_id(run_id.to_string()),
        TurnEvent::CodexSessionId(native_session_id) => SessionEvent::new(
            manager_session_id,
            backend,
            sequence,
            SessionOrigin::Adapter,
            SessionEventPayload::BackendSessionIdentified {
                native_session_id: native_session_id.clone(),
            },
        )
        .with_run_id(run_id.to_string())
        .with_native_session_id(native_session_id),
        TurnEvent::Error(message) => SessionEvent::new(
            manager_session_id,
            backend,
            sequence,
            SessionOrigin::Adapter,
            SessionEventPayload::Error { message },
        )
        .with_run_id(run_id.to_string()),
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
                .map(|codex_session_id| {
                    self.codex_session_id = Some(codex_session_id.to_string());
                    vec![TurnEvent::CodexSessionId(codex_session_id.to_string())]
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

fn build_exec_args(
    work_dir: &Path,
    codex_session_id: Option<&str>,
    settings: &RuntimeSettings,
    prompt: &str,
) -> Vec<String> {
    let mut args = if codex_session_id.is_some() {
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

    if let Some(codex_session_id) = codex_session_id {
        args.push(codex_session_id.to_string());
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
    let sessions_dir = codex_home.join("sessions");
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

    let indexed_names = load_indexed_session_names(&codex_home.join("session_index.jsonl"))?;
    let history_names = load_history_session_names(&codex_home.join("history.jsonl"))?;
    for session in &mut sessions {
        if session.1.is_none() {
            session.1 = indexed_names
                .get(session.0.as_str())
                .cloned()
                .or_else(|| history_names.get(session.0.as_str()).cloned());
        }
    }

    sessions.sort_by(|left, right| {
        let left_time =
            session_modified_at(codex_home, left.0.as_str()).unwrap_or(SystemTime::UNIX_EPOCH);
        let right_time =
            session_modified_at(codex_home, right.0.as_str()).unwrap_or(SystemTime::UNIX_EPOCH);
        right_time.cmp(&left_time)
    });
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
                        entries.push((MessageRole::User, text));
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
                        entries.push((MessageRole::Assistant, text));
                    }
                }
                Some("system") => {
                    let text = item
                        .content
                        .iter()
                        .map(|content| content.text.trim().to_string())
                        .filter(|text| !text.is_empty())
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !text.is_empty() {
                        entries.push((MessageRole::System, text));
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
    let mut session_id = None;
    let mut session_cwd = None;
    let mut title = None;

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
                if title.is_none() {
                    if let Ok(item) = serde_json::from_value::<ResponseItem>(entry.payload) {
                        if item.role.as_deref() == Some("user") {
                            for content in item.content {
                                if content.kind == "input_text"
                                    && !content.text.trim().is_empty()
                                    && is_user_prompt(&content.text)
                                {
                                    title = derived_session_name_from_text(&content.text);
                                    break;
                                }
                            }
                        }
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

    Ok(Some((SessionId::from(session_id), title)))
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

fn find_session_file_in(codex_home: &Path, session_id: &str) -> Result<Option<PathBuf>> {
    let sessions_root = codex_home.join("sessions");
    if !sessions_root.exists() {
        return Ok(None);
    }

    let best = WalkDir::new(&sessions_root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| entry.path().extension() == Some(OsStr::new("jsonl")))
        .filter_map(|entry| {
            let path = entry.into_path();
            match session_id_for_file(&path) {
                Ok(Some(found)) if found == session_id => {
                    let modified = fs::metadata(&path)
                        .and_then(|metadata| metadata.modified())
                        .ok()?;
                    Some((modified, path))
                }
                _ => None,
            }
        })
        .max_by(|left, right| left.0.cmp(&right.0))
        .map(|(_, path)| path);
    Ok(best)
}

fn session_modified_at(codex_home: &Path, session_id: &str) -> Result<SystemTime> {
    let Some(path) = find_session_file_in(codex_home, session_id)? else {
        return Ok(SystemTime::UNIX_EPOCH);
    };
    Ok(fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH))
}

fn delete_session_artifacts_in(codex_home: &Path, session_id: &str) -> Result<()> {
    let sessions_root = codex_home.join("sessions");
    let paths = find_session_files_in(&sessions_root, session_id)?;
    if paths.is_empty() {
        bail!("session file not found: {session_id}");
    }

    for path in &paths {
        if path.exists() {
            fs::remove_file(path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
            prune_empty_parents(path, &sessions_root)?;
        }
    }

    rewrite_filtered_jsonl(&codex_home.join("session_index.jsonl"), |line| {
        parse_index_session_id(line)
            .map(|id| id != session_id)
            .unwrap_or(true)
    })?;
    rewrite_filtered_jsonl(&codex_home.join("history.jsonl"), |line| {
        parse_history_session_id(line)
            .map(|id| id != session_id)
            .unwrap_or(true)
    })?;

    Ok(())
}

fn find_session_files_in(sessions_root: &Path, session_id: &str) -> Result<Vec<PathBuf>> {
    if !sessions_root.exists() {
        return Ok(Vec::new());
    }

    let mut paths = Vec::new();
    for entry in WalkDir::new(sessions_root)
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

fn parse_env_pair(entry: &str) -> Option<(String, String)> {
    let (key, value) = entry.split_once('=')?;
    Some((key.to_string(), value.to_string()))
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
        .windows(br#"\"source\":\"exec\""#.len())
        .any(|slice| slice == br#"\"source\":\"exec\""#)
    {
        return Ok(());
    }

    let first_line =
        String::from_utf8(first_line.to_vec()).context("session header is not UTF-8")?;
    let patched = first_line
        .replace(r#"\"source\":\"exec\""#, r#"\"source\":\"cli\""#)
        .replace(
            r#"\"originator\":\"codex_exec\""#,
            r#"\"originator\":\"codex_cli_rs\""#,
        );

    if patched == first_line {
        return Ok(());
    }

    let mut output = patched.into_bytes();
    output.extend_from_slice(&original[first_newline..]);
    fs::write(&path, output).with_context(|| format!("failed to patch {}", path.display()))?;
    Ok(())
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

#[derive(Debug, Deserialize)]
struct SessionLine {
    #[serde(rename = "type")]
    kind: String,
    payload: Value,
}

#[derive(Debug, Deserialize)]
struct TimestampedSessionLine {
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
