use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use parking_lot::Mutex as ParkingMutex;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde_json::Value;
use session::{
    BackendKind, RuntimeSettings, SessionEvent, SessionEventPayload, SessionOrigin, SessionOutcome,
    SessionRequest,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use vt100::Parser;

use crate::{
    Cli, CliSession, MessageRole, SessionId, SpawnedRun, configure_command_process_group,
    spawn_process_killer,
};

type SessionSummary = (SessionId, Option<String>);
type HistoryEntry = (MessageRole, String);

#[derive(Debug, Clone)]
struct ClaudeCli {
    bin: String,
    extra_env: Vec<String>,
}

impl ClaudeCli {
    fn new(bin: impl Into<String>, extra_env: Vec<String>) -> Self {
        Self {
            bin: bin.into(),
            extra_env,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.bin);
        command.envs(base_claude_environment());
        for (name, value) in self
            .extra_env
            .iter()
            .filter_map(|pair| pair.split_once('='))
        {
            if !name.trim().is_empty() {
                command.env(name.trim(), value);
            }
        }
        command
    }

    fn build_args(&self, request: &SessionRequest, native_session_id: Option<&str>) -> Vec<String> {
        build_claude_args(
            &native_session_id.map(str::to_string),
            &request.settings,
            Some(request.prompt.as_str()),
        )
    }
}

pub(super) fn spawn(
    bin: impl Into<String>,
    extra_env: Vec<String>,
    workdir: &Path,
) -> Result<Box<dyn CliSession>> {
    ClaudeCli::new(bin, extra_env).spawn(workdir)
}

impl Cli for ClaudeCli {
    fn spawn(&self, workdir: &Path) -> Result<Box<dyn CliSession>> {
        let workdir = normalize_path(workdir)?;
        Ok(Box::new(ClaudeCliSession {
            cli: self.clone(),
            workdir: workdir.clone(),
            state: Arc::new(Mutex::new(ClaudeCliSessionState {
                current_session: latest_claude_session_for_workdir(&workdir)?.map(SessionId::from),
                active_cancel: None,
                terminal: None,
            })),
        }))
    }
}

struct ClaudeCliSession {
    cli: ClaudeCli,
    workdir: PathBuf,
    state: Arc<Mutex<ClaudeCliSessionState>>,
}

struct ClaudeCliSessionState {
    current_session: Option<SessionId>,
    active_cancel: Option<CancellationToken>,
    terminal: Option<ClaudeTerminal>,
}

impl CliSession for ClaudeCliSession {
    fn send_prompt(&mut self, request: SessionRequest) -> Result<SpawnedRun> {
        if request.prompt.trim().is_empty() {
            bail!("prompt is empty");
        }

        let mut effective_request = request.clone();
        effective_request.settings = self.effective_runtime_settings(&request.settings)?;
        let selected_session_id = {
            let state = self
                .state
                .lock()
                .map_err(|_| anyhow!("claude session state lock poisoned"))?;
            effective_request.native_session_id.clone().or_else(|| {
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
                .map_err(|_| anyhow!("claude session state lock poisoned"))?;
            if state.active_cancel.is_some() {
                bail!("claude run already active");
            }
            state.active_cancel = Some(cancel.clone());
        }

        let native_session_before = latest_claude_native_session_for_workdir(&self.workdir)?;
        let args = self
            .cli
            .build_args(&effective_request, selected_session_id.as_deref());
        let mut command = self.cli.command();
        command
            .args(&args)
            .current_dir(&self.workdir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdin(std::process::Stdio::null());
        configure_command_process_group(&mut command);

        let mut child = match command.spawn().with_context(|| {
            format!(
                "failed to start claude process in {}: {} {}",
                self.workdir.display(),
                self.cli.bin,
                args.join(" ")
            )
        }) {
            Ok(child) => child,
            Err(err) => {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| anyhow!("claude session state lock poisoned"))?;
                state.active_cancel = None;
                return Err(err);
            }
        };

        let pid = Arc::new(std::sync::atomic::AtomicU32::new(
            child.id().unwrap_or_default(),
        ));
        let stdout = child
            .stdout
            .take()
            .context("claude stdout pipe is unavailable")?;
        let stderr = child
            .stderr
            .take()
            .context("claude stderr pipe is unavailable")?;
        let child = Arc::new(tokio::sync::Mutex::new(child));
        let (tx, rx) = mpsc::channel(64);
        let manager_session_id = effective_request.manager_session_id.clone();
        let run_id = effective_request.run_id.clone();
        let state = Arc::clone(&self.state);
        let workdir = self.workdir.clone();
        let join = tokio::spawn(async move {
            let stderr_task = tokio::spawn(async move {
                let mut buf = String::new();
                let mut reader = tokio::io::BufReader::new(stderr);
                reader
                    .read_to_string(&mut buf)
                    .await
                    .context("failed to read claude stderr")?;
                Ok::<String, anyhow::Error>(buf)
            });

            let killer_task =
                spawn_process_killer(Arc::clone(&child), Arc::clone(&pid), cancel.clone());
            let mut lines = tokio::io::BufReader::new(stdout).lines();
            let mut sequence = 0u64;
            let mut last_nonempty = None;
            let mut identified_native_session_id = selected_session_id.clone();
            while let Some(line) = lines
                .next_line()
                .await
                .context("failed to read claude stdout line")?
            {
                let text = line.trim();
                if text.is_empty() {
                    continue;
                }
                last_nonempty = Some(text.to_string());
                sequence += 1;
                if tx
                    .send(
                        SessionEvent::new(
                            &manager_session_id,
                            BackendKind::Claude,
                            sequence,
                            SessionOrigin::Adapter,
                            SessionEventPayload::AssistantTextDelta {
                                text: text.to_string(),
                            },
                        )
                        .with_run_id(run_id.clone()),
                    )
                    .await
                    .is_err()
                {
                    break;
                }
            }

            let status = {
                let mut child = child.lock().await;
                let status = child.wait().await.context("failed to wait for claude")?;
                pid.store(0, std::sync::atomic::Ordering::Relaxed);
                status
            };
            killer_task.abort();
            let stderr = stderr_task.await.context("stderr task join failed")??;

            let result = if cancel.is_cancelled() {
                Err(anyhow!("request cancelled"))
            } else if !status.success() {
                let stderr = stderr.trim();
                if !stderr.is_empty() {
                    Err(anyhow!(stderr.to_string()))
                } else {
                    Err(anyhow!("claude exited with status {status}"))
                }
            } else {
                if identified_native_session_id.is_none() {
                    if let Some(native_session_id) = latest_claude_native_session_after(
                        &workdir,
                        native_session_before.as_deref(),
                    )? {
                        sequence += 1;
                        identified_native_session_id = Some(native_session_id.clone());
                        let identified_event = SessionEvent::new(
                            &manager_session_id,
                            BackendKind::Claude,
                            sequence,
                            SessionOrigin::Adapter,
                            SessionEventPayload::BackendSessionIdentified {
                                native_session_id: native_session_id.clone(),
                            },
                        )
                        .with_run_id(run_id.clone())
                        .with_native_session_id(native_session_id);
                        let _ = tx.send(identified_event).await;
                    }
                }

                Ok(SessionOutcome {
                    native_session_id: identified_native_session_id,
                    final_text: last_nonempty,
                })
            };

            drop(tx);

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
            .map_err(|_| anyhow!("claude session state lock poisoned"))?
            .active_cancel
            .clone()
            .ok_or_else(|| anyhow!("no active claude run"))?;
        cancel.cancel();
        Ok(())
    }

    fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        list_claude_sessions(&self.workdir)
    }

    fn current_native_session_id(&self) -> Result<Option<String>> {
        Ok(
            latest_claude_native_session_for_workdir(&self.workdir)?.or_else(|| {
                self.state.lock().ok().and_then(|state| {
                    state
                        .current_session
                        .as_ref()
                        .map(|session| session.as_str().to_string())
                })
            }),
        )
    }

    fn attach_terminal(
        &mut self,
        settings: &RuntimeSettings,
        native_session_id: Option<&str>,
    ) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("claude session state lock poisoned"))?;
        if state.terminal.is_some() {
            bail!("claude terminal is already attached");
        }
        let terminal =
            ClaudeTerminal::attach(&self.cli, &self.workdir, settings, native_session_id)?;
        state.terminal = Some(terminal);
        Ok(())
    }

    fn submit_terminal_prompt(&mut self, request: SessionRequest) -> Result<SpawnedRun> {
        if request.prompt.trim().is_empty() {
            bail!("prompt is empty");
        }

        let mut effective_request = request.clone();
        effective_request.settings = self.effective_runtime_settings(&request.settings)?;
        let selected_session_id = {
            let state = self
                .state
                .lock()
                .map_err(|_| anyhow!("claude session state lock poisoned"))?;
            effective_request.native_session_id.clone().or_else(|| {
                state
                    .current_session
                    .as_ref()
                    .map(|session| session.as_str().to_string())
            })
        };

        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("claude session state lock poisoned"))?;
        let terminal = state
            .terminal
            .as_mut()
            .ok_or_else(|| anyhow!("claude terminal is not attached"))?;
        let run = terminal.submit_prompt(&self.workdir, selected_session_id, effective_request)?;
        Ok(run)
    }

    fn interrupt_terminal(&mut self, allow_when_unknown: bool) -> Result<bool> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("claude session state lock poisoned"))?;
        let terminal = state
            .terminal
            .as_mut()
            .ok_or_else(|| anyhow!("claude terminal is not attached"))?;
        terminal.try_send_interrupt(allow_when_unknown)
    }

    fn sync_terminal_size(&mut self) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("claude session state lock poisoned"))?;
        let terminal = state
            .terminal
            .as_mut()
            .ok_or_else(|| anyhow!("claude terminal is not attached"))?;
        terminal.resize_from_current_terminal()
    }

    fn take_terminal_exit_handle(&mut self) -> Result<JoinHandle<Result<()>>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("claude session state lock poisoned"))?;
        let terminal = state
            .terminal
            .as_mut()
            .ok_or_else(|| anyhow!("claude terminal is not attached"))?;
        terminal.take_exit_handle()
    }

    fn cleanup_terminal(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            if let Some(terminal) = state.terminal.as_mut() {
                terminal.cleanup_after_exit();
            }
        }
    }

    fn switch_session(&mut self, session_id: &SessionId) -> Result<()> {
        let sessions = self.list_sessions()?;
        if sessions.iter().any(|session| &session.0 == session_id) {
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow!("claude session state lock poisoned"))?;
            state.current_session = Some(session_id.clone());
            Ok(())
        } else {
            bail!("claude session not found: {session_id}");
        }
    }

    fn delete_session(&mut self, session_id: &SessionId) -> Result<()> {
        let Some(path) = find_claude_session_file(&self.workdir, session_id.as_str())? else {
            bail!("claude session not found: {session_id}");
        };
        fs::remove_file(&path).with_context(|| format!("failed to remove {}", path.display()))?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("claude session state lock poisoned"))?;
        if state.current_session.as_ref() == Some(session_id) {
            state.current_session = None;
        }
        Ok(())
    }

    fn get_history(&self, limit: usize) -> Result<Vec<HistoryEntry>> {
        let session_id = self
            .state
            .lock()
            .map_err(|_| anyhow!("claude session state lock poisoned"))?
            .current_session
            .clone()
            .ok_or_else(|| anyhow!("no active claude session selected"))?;
        get_claude_history(&self.workdir, &session_id, limit)
    }

    fn effective_runtime_settings(&self, requested: &RuntimeSettings) -> Result<RuntimeSettings> {
        Ok(RuntimeSettings::new(
            requested.model.clone(),
            None,
            Some(requested.mode.clone()),
        ))
    }
}

struct ClaudeTerminal {
    writer: Arc<ParkingMutex<Box<dyn std::io::Write + Send>>>,
    master: Arc<ParkingMutex<Box<dyn MasterPty + Send>>>,
    screen_state: Arc<ParkingMutex<TerminalScreenState>>,
    run_state: Arc<ParkingMutex<TerminalRunState>>,
    output_thread: Option<std::thread::JoinHandle<()>>,
    raw_mode: Option<RawTerminalMode>,
    exit_task: Option<JoinHandle<Result<()>>>,
}

#[derive(Default)]
struct TerminalRunState {
    active_cancel: Option<CancellationToken>,
}

struct TerminalRunCancelGuard {
    run_state: Arc<ParkingMutex<TerminalRunState>>,
}

impl TerminalRunCancelGuard {
    fn new(run_state: Arc<ParkingMutex<TerminalRunState>>) -> Self {
        Self { run_state }
    }
}

impl Drop for TerminalRunCancelGuard {
    fn drop(&mut self) {
        self.run_state.lock().active_cancel = None;
    }
}

fn latest_distinct_assistant_text(
    workdir: &Path,
    current_session_id: Option<&str>,
    selected_session_id: Option<&str>,
    baseline_history_len: usize,
    baseline_last_assistant: Option<&str>,
) -> Result<Option<String>> {
    let Some(session_id) = current_session_id else {
        return Ok(None);
    };
    let history = get_claude_history(workdir, &SessionId::from(session_id.to_string()), 0)?;
    let history_start = if selected_session_id == Some(session_id) {
        baseline_history_len.min(history.len())
    } else {
        0
    };
    Ok(history[history_start..].iter().find_map(|(role, text)| {
        matches!(role, MessageRole::Assistant)
            .then(|| text.trim())
            .filter(|text| !text.is_empty())
            .filter(|text| baseline_last_assistant != Some(*text))
            .map(str::to_string)
    }))
}

fn last_assistant_text_for_session(
    workdir: &Path,
    session_id: Option<&str>,
) -> Result<Option<String>> {
    let Some(session_id) = session_id else {
        return Ok(None);
    };
    Ok(
        get_claude_history(workdir, &SessionId::from(session_id.to_string()), 0)?
            .into_iter()
            .rev()
            .find_map(|(role, text)| {
                matches!(role, MessageRole::Assistant)
                    .then_some(text.trim().to_string())
                    .filter(|text| !text.is_empty())
            }),
    )
}

fn make_cancelled_event(
    manager_session_id: &str,
    run_id: &str,
    sequence: u64,
    native_session_id: Option<String>,
) -> SessionEvent {
    let event = SessionEvent::new(
        manager_session_id,
        BackendKind::Claude,
        sequence,
        SessionOrigin::Adapter,
        SessionEventPayload::RunCancelled,
    )
    .with_run_id(run_id.to_string());
    if let Some(native_session_id) = native_session_id {
        event.with_native_session_id(native_session_id)
    } else {
        event
    }
}

fn outcome_with_current_state(
    workdir: &Path,
    native_session_id: Option<String>,
) -> Result<SessionOutcome> {
    let final_text = last_assistant_text_for_session(workdir, native_session_id.as_deref())?;
    Ok(SessionOutcome {
        native_session_id,
        final_text,
    })
}

async fn wait_for_terminal_prompt_ready(
    screen_state: Arc<ParkingMutex<TerminalScreenState>>,
    timeout: std::time::Duration,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut first_ready_at: Option<Instant> = None;
    loop {
        let ready_now = screen_state.lock().ready_for_submit();
        if ready_now {
            let start = first_ready_at.get_or_insert_with(Instant::now);
            if start.elapsed() >= std::time::Duration::from_millis(250) {
                return Ok(());
            }
        } else {
            first_ready_at = None;
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("timed out waiting for terminal frontend prompt after cancellation");
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

impl ClaudeTerminal {
    fn attach(
        cli: &ClaudeCli,
        workdir: &Path,
        settings: &RuntimeSettings,
        native_session_id: Option<&str>,
    ) -> Result<Self> {
        let size = normalize_terminal_size(current_terminal_size());
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(size)?;
        let mut command = CommandBuilder::new(&cli.bin);
        if let Some(session_id) = native_session_id {
            command.arg("--resume");
            command.arg(session_id);
        }
        if let Some(model) = &settings.model {
            command.arg("--model");
            command.arg(model);
        }
        for (name, value) in cli.extra_env.iter().filter_map(|pair| pair.split_once('=')) {
            if !name.trim().is_empty() {
                command.env(name.trim(), value);
            }
        }
        command.cwd(workdir);

        let child = pair.slave.spawn_command(command).with_context(|| {
            format!(
                "failed to start claude terminal frontend in {}",
                workdir.display()
            )
        })?;
        let reader = pair
            .master
            .try_clone_reader()
            .context("failed to clone terminal frontend PTY reader")?;
        let writer = Arc::new(ParkingMutex::new(
            pair.master
                .take_writer()
                .context("failed to take terminal frontend PTY writer")?,
        ));
        let master = Arc::new(ParkingMutex::new(pair.master));
        let screen_state = Arc::new(ParkingMutex::new(TerminalScreenState::new(size)));
        let run_state = Arc::new(ParkingMutex::new(TerminalRunState::default()));
        let output_thread = Some(spawn_output_proxy(reader, Arc::clone(&screen_state)));
        let raw_mode = RawTerminalMode::activate_if_tty()?;
        if stdin_is_tty() {
            spawn_stdin_proxy(Arc::clone(&writer));
        }
        let exit_task = Some(tokio::spawn(async move {
            tokio::task::spawn_blocking(move || {
                let mut child = child;
                child.wait().map(|_| ()).map_err(anyhow::Error::from)
            })
            .await
            .context("terminal frontend wait task join failed")?
        }));

        Ok(Self {
            writer,
            master,
            screen_state,
            run_state,
            output_thread,
            raw_mode,
            exit_task,
        })
    }

    fn submit_prompt(
        &mut self,
        workdir: &Path,
        selected_session_id: Option<String>,
        request: SessionRequest,
    ) -> Result<SpawnedRun> {
        self.dismiss_exit_confirmation()?;
        self.screen_state.lock().clear_pending_interrupt();
        let cancel = CancellationToken::new();
        self.run_state.lock().active_cancel = Some(cancel.clone());
        let baseline_history = selected_session_id
            .as_deref()
            .and_then(|session_id| {
                get_claude_history(workdir, &SessionId::from(session_id.to_string()), 0).ok()
            })
            .unwrap_or_default();
        let baseline_history_len = baseline_history.len();
        let baseline_last_assistant = baseline_history.iter().rev().find_map(|(role, text)| {
            matches!(role, MessageRole::Assistant)
                .then_some(text.trim())
                .filter(|text| !text.is_empty())
                .map(str::to_string)
        });
        let native_session_before = latest_claude_native_session_for_workdir(workdir)?;

        {
            let mut writer = self.writer.lock();
            writer
                .write_all(b"\x15\x0b")
                .context("failed to clear terminal frontend pending input")?;
            writer
                .write_all(request.prompt.as_bytes())
                .context("failed to write prompt to terminal frontend")?;
            writer
                .write_all(b"\r")
                .context("failed to submit prompt to terminal frontend")?;
            writer
                .flush()
                .context("failed to flush terminal frontend prompt")?;
        }

        let workdir = workdir.to_path_buf();
        let manager_session_id = request.manager_session_id.clone();
        let run_id = request.run_id.clone();
        let run_state = Arc::clone(&self.run_state);
        let screen_state = Arc::clone(&self.screen_state);
        let (tx, rx) = mpsc::channel(16);
        let join = tokio::spawn(async move {
            let _cancel_guard = TerminalRunCancelGuard::new(Arc::clone(&run_state));

            let current_session_id = loop {
                if cancel.is_cancelled() {
                    let native_session_id = latest_claude_native_session_for_workdir(&workdir)?
                        .or(selected_session_id.clone());
                    let _ = tx
                        .send(make_cancelled_event(
                            &manager_session_id,
                            &run_id,
                            1,
                            native_session_id.clone(),
                        ))
                        .await;
                    return outcome_with_current_state(&workdir, native_session_id);
                }
                if let Some(session_id) = latest_claude_native_session_for_workdir(&workdir)? {
                    if selected_session_id.as_deref() == Some(session_id.as_str())
                        || selected_session_id.is_none()
                    {
                        break Some(session_id);
                    }
                }
                if let Some(session_id) =
                    latest_claude_native_session_after(&workdir, native_session_before.as_deref())?
                {
                    break Some(session_id);
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            };

            let mut sequence = 0u64;
            if let Some(native_session_id) = current_session_id.clone() {
                if selected_session_id.as_deref() != Some(native_session_id.as_str()) {
                    sequence += 1;
                    if tx
                        .send(
                            SessionEvent::new(
                                &manager_session_id,
                                BackendKind::Claude,
                                sequence,
                                SessionOrigin::Adapter,
                                SessionEventPayload::BackendSessionIdentified {
                                    native_session_id: native_session_id.clone(),
                                },
                            )
                            .with_run_id(run_id.clone())
                            .with_native_session_id(native_session_id),
                        )
                        .await
                        .is_err()
                    {
                        return Ok(SessionOutcome::empty());
                    }
                }
            }

            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
            loop {
                let current_session_id = latest_claude_native_session_for_workdir(&workdir)?
                    .or_else(|| current_session_id.clone());
                if let Some(assistant_text) = latest_distinct_assistant_text(
                    &workdir,
                    current_session_id.as_deref(),
                    selected_session_id.as_deref(),
                    baseline_history_len,
                    baseline_last_assistant.as_deref(),
                )? {
                    sequence += 1;
                    let final_event = SessionEvent::new(
                        &manager_session_id,
                        BackendKind::Claude,
                        sequence,
                        SessionOrigin::Adapter,
                        SessionEventPayload::AssistantTextFinal {
                            text: assistant_text.clone(),
                        },
                    )
                    .with_run_id(run_id.clone());
                    let final_event = if let Some(native_session_id) = current_session_id.clone() {
                        final_event.with_native_session_id(native_session_id)
                    } else {
                        final_event
                    };
                    if tx.send(final_event).await.is_err() {
                        return outcome_with_current_state(&workdir, current_session_id);
                    }

                    sequence += 1;
                    let finished_event = SessionEvent::new(
                        &manager_session_id,
                        BackendKind::Claude,
                        sequence,
                        SessionOrigin::Adapter,
                        SessionEventPayload::RunFinished,
                    )
                    .with_run_id(run_id.clone());
                    let finished_event = if let Some(native_session_id) = current_session_id.clone()
                    {
                        finished_event.with_native_session_id(native_session_id.clone())
                    } else {
                        finished_event
                    };
                    let _ = tx.send(finished_event).await;
                    return outcome_with_current_state(&workdir, current_session_id);
                }

                if cancel.is_cancelled() {
                    sequence += 1;
                    let native_session_id = latest_claude_native_session_for_workdir(&workdir)?
                        .or_else(|| current_session_id.clone());
                    let _ = tx
                        .send(make_cancelled_event(
                            &manager_session_id,
                            &run_id,
                            sequence,
                            native_session_id.clone(),
                        ))
                        .await;
                    wait_for_terminal_prompt_ready(
                        Arc::clone(&screen_state),
                        std::time::Duration::from_secs(5),
                    )
                    .await?;
                    return outcome_with_current_state(&workdir, native_session_id);
                }

                if tokio::time::Instant::now() >= deadline {
                    bail!("timed out waiting for terminal frontend response");
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        });
        Ok((rx, join))
    }

    fn try_send_interrupt(&mut self, allow_when_unknown: bool) -> Result<bool> {
        if let Some(cancel) = self.run_state.lock().active_cancel.clone() {
            cancel.cancel();
        }
        {
            let mut screen_state = self.screen_state.lock();
            if !screen_state.begin_interrupt(allow_when_unknown) {
                return Ok(false);
            }
        }

        let mut writer = self.writer.lock();
        if let Err(err) = writer.write_all(b"\x03") {
            self.screen_state.lock().clear_pending_interrupt();
            return Err(err).context("failed to write interrupt to terminal frontend");
        }
        if let Err(err) = writer.flush() {
            self.screen_state.lock().clear_pending_interrupt();
            return Err(err).context("failed to flush terminal frontend interrupt");
        }
        Ok(true)
    }

    fn dismiss_exit_confirmation(&mut self) -> Result<()> {
        let awaiting_exit_confirmation = {
            let screen_state = self.screen_state.lock();
            matches!(
                screen_state.ui_state(),
                TerminalUiState::AwaitingExitConfirmation
            )
        };
        if !awaiting_exit_confirmation {
            return Ok(());
        }

        {
            let mut writer = self.writer.lock();
            writer
                .write_all(b"q")
                .context("failed to dismiss terminal exit confirmation")?;
            writer
                .flush()
                .context("failed to flush terminal exit confirmation dismiss")?;
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            {
                let screen_state = self.screen_state.lock();
                if !matches!(
                    screen_state.ui_state(),
                    TerminalUiState::AwaitingExitConfirmation
                ) {
                    return Ok(());
                }
            }
            if std::time::Instant::now() >= deadline {
                bail!("timed out dismissing terminal exit confirmation")
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    fn resize_from_current_terminal(&mut self) -> Result<()> {
        let size = normalize_terminal_size(current_terminal_size());
        self.master.lock().resize(size)?;
        self.screen_state.lock().resize(size);
        Ok(())
    }

    fn take_exit_handle(&mut self) -> Result<JoinHandle<Result<()>>> {
        self.exit_task
            .take()
            .ok_or_else(|| anyhow!("terminal frontend exit handle already taken"))
    }

    fn cleanup_after_exit(&mut self) {
        if let Some(handle) = self.output_thread.take() {
            let _ = handle.join();
        }
        if let Some(raw_mode) = self.raw_mode.take() {
            let _ = raw_mode.restore();
        }
    }
}

impl Drop for ClaudeTerminal {
    fn drop(&mut self) {
        if let Some(raw_mode) = self.raw_mode.take() {
            let _ = raw_mode.restore();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalUiState {
    Running,
    AwaitingExitConfirmation,
    IdleOrUnknown,
}

struct TerminalScreenState {
    parser: Parser,
    pending_interrupt: bool,
}

impl TerminalScreenState {
    fn new(size: PtySize) -> Self {
        let size = normalize_terminal_size(size);
        Self {
            parser: Parser::new(size.rows, size.cols, 0),
            pending_interrupt: false,
        }
    }

    fn resize(&mut self, size: PtySize) {
        let size = normalize_terminal_size(size);
        self.parser.screen_mut().set_size(size.rows, size.cols);
    }

    fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
        if !matches!(self.ui_state(), TerminalUiState::Running) {
            self.pending_interrupt = false;
        }
    }

    fn clear_pending_interrupt(&mut self) {
        self.pending_interrupt = false;
    }

    fn ready_for_submit(&self) -> bool {
        if !matches!(self.ui_state(), TerminalUiState::IdleOrUnknown) {
            return false;
        }
        self.parser
            .screen()
            .contents()
            .lines()
            .filter_map(|line| {
                let trimmed = line.trim_start();
                trimmed.strip_prefix('❯').map(str::trim)
            })
            .next_back()
            .map(|rest| rest.is_empty())
            .unwrap_or(false)
    }

    fn begin_interrupt(&mut self, allow_when_unknown: bool) -> bool {
        if self.pending_interrupt {
            return false;
        }
        match self.ui_state() {
            TerminalUiState::Running => {
                self.pending_interrupt = true;
                true
            }
            TerminalUiState::IdleOrUnknown if allow_when_unknown => {
                self.pending_interrupt = true;
                true
            }
            TerminalUiState::AwaitingExitConfirmation | TerminalUiState::IdleOrUnknown => false,
        }
    }

    fn ui_state(&self) -> TerminalUiState {
        let screen = self.parser.screen().contents().to_ascii_lowercase();
        if screen.contains("press ctrl-c again to exit") {
            TerminalUiState::AwaitingExitConfirmation
        } else if screen.contains("esc to interrupt") {
            TerminalUiState::Running
        } else {
            TerminalUiState::IdleOrUnknown
        }
    }
}

#[derive(Debug)]
struct RawTerminalMode {
    fd: i32,
    original: libc::termios,
}

impl RawTerminalMode {
    fn activate_if_tty() -> Result<Option<Self>> {
        let fd = libc::STDIN_FILENO;
        if unsafe { libc::isatty(fd) } != 1 {
            return Ok(None);
        }
        let mut original = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut original) } == -1 {
            return Err(io::Error::last_os_error()).context("failed to read terminal mode");
        }
        let mut raw = original;
        unsafe {
            libc::cfmakeraw(&mut raw);
        }
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } == -1 {
            return Err(io::Error::last_os_error()).context("failed to enable raw terminal mode");
        }
        Ok(Some(Self { fd, original }))
    }

    fn restore(self) -> Result<()> {
        if unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.original) } == -1 {
            return Err(io::Error::last_os_error()).context("failed to restore terminal mode");
        }
        Ok(())
    }
}

fn stdin_is_tty() -> bool {
    (unsafe { libc::isatty(libc::STDIN_FILENO) }) == 1
}

fn normalize_terminal_size(size: PtySize) -> PtySize {
    PtySize {
        rows: size.rows.max(1),
        cols: size.cols.max(1),
        pixel_width: size.pixel_width,
        pixel_height: size.pixel_height,
    }
}

fn current_terminal_size() -> PtySize {
    let fd = if (unsafe { libc::isatty(libc::STDOUT_FILENO) }) == 1 {
        libc::STDOUT_FILENO
    } else if (unsafe { libc::isatty(libc::STDIN_FILENO) }) == 1 {
        libc::STDIN_FILENO
    } else {
        return PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        };
    };
    let mut winsize = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut winsize) } == -1
        || winsize.ws_row == 0
        || winsize.ws_col == 0
    {
        return PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        };
    }
    PtySize {
        rows: winsize.ws_row,
        cols: winsize.ws_col,
        pixel_width: winsize.ws_xpixel,
        pixel_height: winsize.ws_ypixel,
    }
}

fn spawn_output_proxy(
    mut reader: Box<dyn std::io::Read + Send>,
    screen_state: Arc<ParkingMutex<TerminalScreenState>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut stdout = std::io::stdout().lock();
        let mut buf = [0_u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    screen_state.lock().process(&buf[..n]);
                    if stdout.write_all(&buf[..n]).is_err() {
                        break;
                    }
                    if stdout.flush().is_err() {
                        break;
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    })
}

fn spawn_stdin_proxy(writer: Arc<ParkingMutex<Box<dyn std::io::Write + Send>>>) {
    let _ = std::thread::Builder::new()
        .name("worker-terminal-stdin".to_string())
        .spawn(move || {
            let mut stdin = std::io::stdin();
            let mut buf = [0_u8; 1024];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let mut locked = writer.lock();
                        if locked.write_all(&buf[..n]).is_err() {
                            break;
                        }
                        if locked.flush().is_err() {
                            break;
                        }
                    }
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        });
}

fn build_claude_args(
    native_session_id: &Option<String>,
    settings: &RuntimeSettings,
    prompt: Option<&str>,
) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(session_id) = native_session_id {
        args.push("--resume".to_string());
        args.push(session_id.clone());
    }
    if let Some(model) = &settings.model {
        args.push("--model".to_string());
        args.push(model.clone());
    }
    if let Some(prompt) = prompt {
        args.push(prompt.to_string());
    }
    args
}

fn list_claude_sessions(workdir: &Path) -> Result<Vec<SessionSummary>> {
    let workdir = normalize_path(workdir)?;
    let root = claude_projects_root();
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut sessions = Vec::new();
    for project in
        fs::read_dir(&root).with_context(|| format!("failed to read {}", root.display()))?
    {
        let project = project?;
        if !project.file_type()?.is_dir() {
            continue;
        }
        for child in fs::read_dir(project.path())? {
            let child = child?;
            if !child.file_type()?.is_file() {
                continue;
            }
            let path = child.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(summary) = parse_claude_session_file(&path, &workdir)? else {
                continue;
            };
            sessions.push((
                child
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH),
                summary,
            ));
        }
    }
    sessions.sort_by(|left, right| right.0.cmp(&left.0));
    Ok(sessions.into_iter().map(|(_, summary)| summary).collect())
}

fn parse_claude_session_file(path: &Path, workdir: &Path) -> Result<Option<SessionSummary>> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut session_id: Option<String> = None;
    let mut cwd: Option<PathBuf> = None;
    let mut title: Option<String> = None;

    for line in reader.lines().take(200) {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if session_id.is_none() {
            session_id = value
                .get("sessionId")
                .and_then(|v| v.as_str())
                .map(str::to_string);
        }
        if cwd.is_none() {
            cwd = value
                .get("cwd")
                .and_then(|v| v.as_str())
                .map(PathBuf::from)
                .map(|path| normalize_path(&path))
                .transpose()?;
        }
        if title.is_none() {
            title = extract_claude_title(&value);
        }
        if session_id.is_some() && cwd.is_some() && title.is_some() {
            break;
        }
    }

    match (session_id, cwd) {
        (Some(session_id), Some(cwd)) if cwd == workdir => {
            Ok(Some((SessionId::from(session_id), title)))
        }
        _ => Ok(None),
    }
}

fn extract_claude_title(value: &Value) -> Option<String> {
    let role = value
        .get("message")
        .and_then(|message| message.get("role"))
        .and_then(Value::as_str);
    if role != Some("user") {
        return None;
    }

    let content = value
        .get("message")
        .and_then(|message| message.get("content"))?;
    match content {
        Value::String(text) => normalize_title(text),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .find_map(normalize_title),
        _ => None,
    }
}

fn get_claude_history(
    workdir: &Path,
    session_id: &SessionId,
    limit: usize,
) -> Result<Vec<HistoryEntry>> {
    let Some(path) = find_claude_session_file(workdir, session_id.as_str())? else {
        bail!("claude session not found: {session_id}");
    };

    let file =
        fs::File::open(&path).with_context(|| format!("failed to open {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut entries = Vec::new();

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(entry) = parse_claude_history_entry(&value) else {
            continue;
        };
        entries.push(entry);
    }

    if limit > 0 && entries.len() > limit {
        entries = entries.split_off(entries.len() - limit);
    }
    Ok(entries)
}

fn parse_claude_history_entry(value: &Value) -> Option<HistoryEntry> {
    let role = value
        .get("message")
        .and_then(|message| message.get("role"))
        .and_then(Value::as_str)?;
    let content = value
        .get("message")
        .and_then(|message| message.get("content"))?;
    let text = match content {
        Value::String(text) => text.trim().to_string(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };
    if text.is_empty() {
        return None;
    }

    let role = match role {
        "user" => MessageRole::User,
        "assistant" => MessageRole::Assistant,
        "system" => MessageRole::System,
        _ => return None,
    };
    Some((role, text))
}

fn latest_claude_session_for_workdir(workdir: &Path) -> Result<Option<String>> {
    latest_claude_session_entry_for_workdir(workdir)
        .map(|entry| entry.map(|(session_id, _)| session_id))
}

fn latest_claude_native_session_for_workdir(workdir: &Path) -> Result<Option<String>> {
    latest_claude_session_for_workdir(workdir)
}

fn latest_claude_native_session_after(
    workdir: &Path,
    previous: Option<&str>,
) -> Result<Option<String>> {
    let root = claude_projects_root();
    if !root.exists() {
        return Ok(None);
    }

    let workdir = normalize_path(workdir)?;
    let mut latest: Option<(String, SystemTime)> = None;
    for project in
        fs::read_dir(&root).with_context(|| format!("failed to read {}", root.display()))?
    {
        let project = project?;
        if !project.file_type()?.is_dir() {
            continue;
        }
        for child in fs::read_dir(project.path())? {
            let child = child?;
            if !child.file_type()?.is_file() {
                continue;
            }
            let path = child.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            let Some((session_id, cwd)) = parse_claude_session_entry(&path)? else {
                continue;
            };
            if normalize_path(&cwd)? != workdir || previous == Some(session_id.as_str()) {
                continue;
            }
            let modified = child
                .metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            let replace = latest
                .as_ref()
                .map(|(_, current_modified)| modified >= *current_modified)
                .unwrap_or(true);
            if replace {
                latest = Some((session_id, modified));
            }
        }
    }
    Ok(latest.map(|(session_id, _)| session_id))
}

fn latest_claude_session_entry_for_workdir(workdir: &Path) -> Result<Option<(String, SystemTime)>> {
    let root = claude_projects_root();
    if !root.exists() {
        return Ok(None);
    }
    let workdir = normalize_path(workdir)?;
    let mut latest: Option<(String, SystemTime)> = None;
    for project in
        fs::read_dir(&root).with_context(|| format!("failed to read {}", root.display()))?
    {
        let project = project?;
        if !project.file_type()?.is_dir() {
            continue;
        }
        for child in fs::read_dir(project.path())? {
            let child = child?;
            if !child.file_type()?.is_file() {
                continue;
            }
            let path = child.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            let Some((session_id, cwd)) = parse_claude_session_entry(&path)? else {
                continue;
            };
            if normalize_path(&cwd)? != workdir {
                continue;
            }
            let modified = child
                .metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            let replace = latest
                .as_ref()
                .map(|(_, current_modified)| modified >= *current_modified)
                .unwrap_or(true);
            if replace {
                latest = Some((session_id, modified));
            }
        }
    }
    Ok(latest)
}

fn find_claude_session_file(workdir: &Path, session_id: &str) -> Result<Option<PathBuf>> {
    let root = claude_projects_root();
    if !root.exists() {
        return Ok(None);
    }

    let workdir = normalize_path(workdir)?;
    for project in
        fs::read_dir(&root).with_context(|| format!("failed to read {}", root.display()))?
    {
        let project = project?;
        if !project.file_type()?.is_dir() {
            continue;
        }
        for child in fs::read_dir(project.path())? {
            let child = child?;
            if !child.file_type()?.is_file() {
                continue;
            }
            let path = child.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            let Some((found_session_id, cwd)) = parse_claude_session_entry(&path)? else {
                continue;
            };
            if found_session_id == session_id && normalize_path(&cwd)? == workdir {
                return Ok(Some(path));
            }
        }
    }
    Ok(None)
}

fn parse_claude_session_entry(path: &Path) -> Result<Option<(String, PathBuf)>> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut session_id: Option<String> = None;
    let mut cwd: Option<PathBuf> = None;
    for line in reader.lines().take(200) {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if session_id.is_none() {
            session_id = value
                .get("sessionId")
                .and_then(|v| v.as_str())
                .map(str::to_string);
        }
        if cwd.is_none() {
            cwd = value.get("cwd").and_then(|v| v.as_str()).map(PathBuf::from);
        }
        if session_id.is_some() && cwd.is_some() {
            break;
        }
    }
    match (session_id, cwd) {
        (Some(session_id), Some(cwd)) => Ok(Some((session_id, cwd))),
        _ => Ok(None),
    }
}

fn claude_projects_root() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("projects")
}

fn base_claude_environment() -> &'static BTreeMap<String, String> {
    static ENV: OnceLock<BTreeMap<String, String>> = OnceLock::new();
    ENV.get_or_init(load_base_claude_environment)
}

fn load_base_claude_environment() -> BTreeMap<String, String> {
    let mut env = std::env::vars().collect::<BTreeMap<_, _>>();
    apply_ccr_fish_environment(&mut env);
    env
}

fn apply_ccr_fish_environment(env: &mut BTreeMap<String, String>) {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let config = PathBuf::from(home)
        .join(".config")
        .join("fish")
        .join("functions")
        .join("ccr-env.fish");
    let Ok(contents) = std::fs::read_to_string(&config) else {
        return;
    };
    let Some(router_url) = extract_fish_router_url(&contents) else {
        return;
    };
    env.insert("ANTHROPIC_AUTH_TOKEN".to_string(), "test".to_string());
    env.insert("ANTHROPIC_BASE_URL".to_string(), router_url);
    env.insert("NO_PROXY".to_string(), "127.0.0.1".to_string());
    env.insert("DISABLE_TELEMETRY".to_string(), "true".to_string());
    env.insert("DISABLE_COST_WARNINGS".to_string(), "true".to_string());
    env.insert("API_TIMEOUT_MS".to_string(), "600000".to_string());
    env.remove("CLAUDE_CODE_USE_BEDROCK");
}

fn extract_fish_router_url(contents: &str) -> Option<String> {
    contents
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("set -l router_url "))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn normalize_title(text: &str) -> Option<String> {
    let title = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if title.is_empty() {
        None
    } else {
        Some(title.chars().take(80).collect())
    }
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use portable_pty::PtySize;

    fn spawn_foreground_command(
        mut command: Command,
        bin: &str,
        args: &[String],
        workdir: &Path,
    ) -> Result<tokio::process::Child> {
        command
            .args(args)
            .current_dir(workdir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        command.spawn().with_context(|| {
            format!(
                "failed to start foreground command in {}: {} {}",
                workdir.display(),
                bin,
                args.join(" ")
            )
        })
    }

    #[test]
    fn ready_for_submit_requires_blank_prompt_line() {
        let size = PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut state = TerminalScreenState::new(size);
        state.process("❯ Reply with exactly HELLO\n".as_bytes());
        assert!(!state.ready_for_submit());

        let mut blank_prompt = TerminalScreenState::new(size);
        blank_prompt.process("❯\u{a0}\n  ? for shortcuts\n".as_bytes());
        assert!(blank_prompt.ready_for_submit());
    }

    #[test]
    fn ready_for_submit_rejects_old_nonblank_prompt_when_new_blank_prompt_exists() {
        let size = PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut state = TerminalScreenState::new(size);
        state.process(
            "❯ Reply with exactly DOUBLE_STOP_TEST\n❯\u{a0}\n  ? for shortcuts\n".as_bytes(),
        );
        assert!(state.ready_for_submit());
    }

    #[test]
    fn ready_for_submit_rejects_latest_nonblank_prompt() {
        let size = PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut state = TerminalScreenState::new(size);
        state.process("❯\u{a0}\n❯ Reply with exactly HELLO\n".as_bytes());
        assert!(!state.ready_for_submit());
    }

    #[test]
    fn ready_for_submit_handles_multiple_prompt_lines() {
        let size = PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut state = TerminalScreenState::new(size);
        state.process("history\n❯ first\noutput\n❯\u{a0}\n".as_bytes());
        assert!(state.ready_for_submit());
    }

    #[test]
    fn ready_for_submit_rejects_when_no_prompt_exists() {
        let size = PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut state = TerminalScreenState::new(size);
        state.process("plain output only\n".as_bytes());
        assert!(!state.ready_for_submit());
    }

    #[test]
    fn ready_for_submit_rejects_exit_confirmation_state() {
        let size = PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut state = TerminalScreenState::new(size);
        state.process("Press Ctrl-C again to exit\n❯\u{a0}\n".as_bytes());
        assert!(!state.ready_for_submit());
    }

    #[test]
    fn ready_for_submit_rejects_running_state_even_with_blank_prompt() {
        let size = PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut state = TerminalScreenState::new(size);
        state.process("Esc to interrupt\n❯\u{a0}\n".as_bytes());
        assert!(!state.ready_for_submit());
    }

    #[test]
    fn ready_for_submit_treats_indented_blank_prompt_as_ready() {
        let size = PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut state = TerminalScreenState::new(size);
        state.process("   ❯   \n".as_bytes());
        assert!(state.ready_for_submit());
    }

    #[test]
    fn ready_for_submit_rejects_latest_prompt_with_text_after_old_blank_prompt() {
        let size = PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut state = TerminalScreenState::new(size);
        state.process("❯\u{a0}\noutput\n❯ follow-up text\n".as_bytes());
        assert!(!state.ready_for_submit());
    }

    #[test]
    fn ready_for_submit_accepts_latest_blank_prompt_after_old_nonblank_prompt() {
        let size = PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut state = TerminalScreenState::new(size);
        state.process("❯ old text\noutput\n❯\u{a0}\n".as_bytes());
        assert!(state.ready_for_submit());
    }

    #[test]
    fn ready_for_submit_rejects_prompt_without_space_clearing() {
        let size = PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut state = TerminalScreenState::new(size);
        state.process("❯still typing\n".as_bytes());
        assert!(!state.ready_for_submit());
    }

    #[test]
    fn ready_for_submit_accepts_nonbreaking_space_only_prompt() {
        let size = PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut state = TerminalScreenState::new(size);
        state.process("❯\u{a0}\n".as_bytes());
        assert!(state.ready_for_submit());
    }

    #[test]
    fn ready_for_submit_uses_latest_prompt_line() {
        let size = PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut state = TerminalScreenState::new(size);
        state.process("❯ old text\n❯ newer text\n".as_bytes());
        assert!(!state.ready_for_submit());

        let mut ready = TerminalScreenState::new(size);
        ready.process("❯ old text\n❯\u{a0}\n".as_bytes());
        assert!(ready.ready_for_submit());
    }

    #[test]
    fn ready_for_submit_ignores_non_prompt_lines_after_blank_prompt() {
        let size = PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut state = TerminalScreenState::new(size);
        state.process("❯\u{a0}\n  ? for shortcuts\n".as_bytes());
        assert!(state.ready_for_submit());
    }

    #[test]
    fn ready_for_submit_rejects_single_visible_character_after_prompt() {
        let size = PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut state = TerminalScreenState::new(size);
        state.process("❯ x\n".as_bytes());
        assert!(!state.ready_for_submit());
    }

    #[test]
    fn ready_for_submit_accepts_empty_prompt_with_trailing_spaces() {
        let size = PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut state = TerminalScreenState::new(size);
        state.process("❯   \n".as_bytes());
        assert!(state.ready_for_submit());
    }

    #[test]
    fn ready_for_submit_rejects_when_latest_prompt_contains_old_text() {
        let size = PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut state = TerminalScreenState::new(size);
        state.process("❯ old text\nother\n❯ old text\n".as_bytes());
        assert!(!state.ready_for_submit());
    }

    #[test]
    fn ready_for_submit_accepts_when_latest_prompt_is_only_prompt() {
        let size = PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut state = TerminalScreenState::new(size);
        state.process("❯\n".as_bytes());
        assert!(state.ready_for_submit());
    }

    #[test]
    fn ready_for_submit_rejects_nonblank_latest_prompt_with_shortcuts_line() {
        let size = PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut state = TerminalScreenState::new(size);
        state.process("❯ still here\n  ? for shortcuts\n".as_bytes());
        assert!(!state.ready_for_submit());
    }

    #[test]
    fn ready_for_submit_accepts_blank_latest_prompt_with_prior_output() {
        let size = PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut state = TerminalScreenState::new(size);
        state.process("assistant reply\n❯\u{a0}\n".as_bytes());
        assert!(state.ready_for_submit());
    }

    #[test]
    fn ready_for_submit_rejects_blank_prompt_if_running_marker_present() {
        let size = PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut state = TerminalScreenState::new(size);
        state.process("esc to interrupt\nassistant reply\n❯\u{a0}\n".as_bytes());
        assert!(!state.ready_for_submit());
    }

    #[test]
    fn ready_for_submit_rejects_blank_prompt_if_exit_marker_present() {
        let size = PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut state = TerminalScreenState::new(size);
        state.process("press ctrl-c again to exit\nassistant reply\n❯\u{a0}\n".as_bytes());
        assert!(!state.ready_for_submit());
    }

    #[test]
    fn ready_for_submit_accepts_latest_blank_prompt_among_multiple_blank_prompts() {
        let size = PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut state = TerminalScreenState::new(size);
        state.process("❯\u{a0}\noutput\n❯\u{a0}\n".as_bytes());
        assert!(state.ready_for_submit());
    }

    #[test]
    fn ready_for_submit_rejects_if_last_prompt_has_any_visible_text() {
        let size = PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut state = TerminalScreenState::new(size);
        state.process("❯\u{a0}\noutput\n❯ last\n".as_bytes());
        assert!(!state.ready_for_submit());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn foreground_process_stays_in_current_process_group() {
        let args = vec!["-c".to_string(), "sleep 5".to_string()];
        let mut child =
            spawn_foreground_command(Command::new("/bin/sh"), "/bin/sh", &args, Path::new("."))
                .unwrap();

        let child_pid = child.id().unwrap() as i32;
        let child_pgid = unsafe { libc::getpgid(child_pid) };
        let current_pgid = unsafe { libc::getpgrp() };

        let _ = child.start_kill();
        let _ = child.wait().await;

        assert_eq!(
            child_pgid, current_pgid,
            "foreground child should stay in current foreground process group"
        );
    }
}
