use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use cli::{Cli, CliSession};
use session::{BackendKind, BackendKindConfig, RuntimeSettings};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::config::{Config, TelegramConfig};
use crate::event::{TerminalExitedEvent, WorkerEvent};
use crate::event_loop::WorkerEventLoop;
use crate::frontend::listener::FrontendListener;

#[derive(Debug, Clone, PartialEq, Eq)]
struct FrontendProcessSpec {
    label: String,
    binary_name: &'static str,
    args: Vec<OsString>,
}

pub async fn init(
    explicit_config_path: Option<PathBuf>,
    project: Option<String>,
    worker_id: Option<String>,
    backend: BackendKindConfig,
    workdir: PathBuf,
    token_index: usize,
) -> Result<()> {
    let config_path = resolve_config_path(explicit_config_path);
    let config = load_config(&config_path, project.as_deref())?;
    let workdir = validate_workdir(workdir)?;
    validate_token_index(&config, token_index)?;
    let worker_id = resolve_worker_id(worker_id);
    let runtime_settings = runtime_settings_for_backend(&config, backend);
    let mut cli_session = build_cli_session(&config, backend, &workdir)?;
    let native_session_id = cli_session.current_native_session_id()?;
    cli_session.attach_terminal(&runtime_settings, native_session_id.as_deref())?;
    let terminal_exit = cli_session.take_terminal_exit_handle()?;
    let frontend_listener = FrontendListener::bind(&config_path, &worker_id)?;
    let (event_tx, event_rx) = mpsc::channel(64);
    let frontend_task = tokio::spawn(frontend_listener.run(event_tx.clone()));
    let refresh_task = tokio::spawn(run_refresh_loop(event_tx.clone()));
    let terminal_task = tokio::spawn(run_terminal_exit_forwarder(terminal_exit, event_tx.clone()));
    let frontend_process_specs = build_frontend_process_specs(
        &config_path,
        &config,
        &worker_id,
        &workdir,
        token_index,
    )?;
    let frontend_processes = spawn_frontend_processes(&frontend_process_specs)?;
    let event_loop = Arc::new(WorkerEventLoop::new(
        worker_id,
        backend,
        workdir,
        runtime_settings,
        cli_session,
        event_rx,
    ));

    let run_result = event_loop.clone().run().await;
    refresh_task.abort();
    terminal_task.abort();
    frontend_task.abort();
    let _ = refresh_task.await;
    let _ = terminal_task.await;
    let _ = frontend_task.await;
    cleanup_frontend_processes(frontend_processes).await;
    let _ = crate::frontend::handler::with_cli_session(event_loop.as_ref(), |session| {
        session.cleanup_terminal();
        Ok(())
    });
    run_result
}

fn resolve_config_path(explicit: Option<PathBuf>) -> PathBuf {
    if let Some(path) = explicit {
        return path;
    }
    let local = PathBuf::from("config.toml");
    if local.exists() {
        return local;
    }
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|home| home.join(".codex-bot").join("config.toml"))
        .unwrap_or(local)
}

fn load_config(path: &Path, _selected_project: Option<&str>) -> Result<Config> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    let mut config = toml::from_str::<Config>(&raw)
        .with_context(|| format!("invalid codex-bot TOML in {}", path.display()))?;

    let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
    config.claude.work_dir = normalize_path(base_dir, &config.claude.work_dir);
    if let Some(state_path) = &config.state_path {
        config.state_path = Some(normalize_path(base_dir, state_path));
    }
    if let Some(socket_dir) = &config.rpc.socket_dir {
        config.rpc.socket_dir = Some(normalize_path(base_dir, socket_dir));
    }
    if config.log_level.trim().is_empty() {
        config.log_level = "info".to_string();
    }
    config.telegram = normalize_telegram_config(config.telegram)?;
    Ok(config)
}

fn normalize_telegram_config(config: TelegramConfig) -> Result<TelegramConfig> {
    let manager_token = config.manager_token.trim().to_string();
    if manager_token.is_empty() {
        bail!("telegram manager_token is empty");
    }
    if config.worker_tokens.is_empty() {
        bail!("telegram worker_tokens must not be empty");
    }
    let mut seen_workers = std::collections::HashSet::new();
    for token in &config.worker_tokens {
        let trimmed = token.trim();
        if trimmed.is_empty() {
            bail!("telegram tokens must not be empty");
        }
        if !seen_workers.insert(trimmed.to_string()) {
            bail!("duplicate telegram token configured");
        }
    }
    Ok(TelegramConfig {
        manager_token,
        worker_tokens: config
            .worker_tokens
            .into_iter()
            .map(|token| token.trim().to_string())
            .collect(),
        allow_from: config.allow_from,
        group_reply_all: config.group_reply_all,
        share_session_in_channel: config.share_session_in_channel,
        poll_timeout_seconds: config.poll_timeout_seconds,
    })
}

fn normalize_path(base_dir: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_dir.join(path)
    }
}

fn validate_workdir(workdir: PathBuf) -> Result<PathBuf> {
    if workdir.as_os_str().is_empty() {
        return Err(anyhow!("worker --workdir must not be empty"));
    }
    let workdir = if workdir.is_absolute() {
        workdir
    } else {
        std::env::current_dir()?.join(workdir)
    };
    if !workdir.exists() {
        return Err(anyhow!(
            "worker workdir does not exist: {}",
            workdir.display()
        ));
    }
    if !workdir.is_dir() {
        return Err(anyhow!(
            "worker workdir is not a directory: {}",
            workdir.display()
        ));
    }
    Ok(workdir)
}

fn validate_token_index(config: &Config, token_index: usize) -> Result<()> {
    if !config.telegram.worker_tokens.is_empty()
        && token_index >= config.telegram.worker_tokens.len()
    {
        return Err(anyhow!(
            "worker token_index {} is out of range (configured worker tokens: {})",
            token_index,
            config.telegram.worker_tokens.len()
        ));
    }
    Ok(())
}

fn resolve_worker_id(worker_id: Option<String>) -> String {
    worker_id.unwrap_or_else(|| Uuid::new_v4().to_string())
}

fn runtime_settings_for_backend(config: &Config, backend: BackendKindConfig) -> RuntimeSettings {
    match backend {
        BackendKindConfig::Codex => RuntimeSettings::new(
            config.codex.model.clone(),
            config.codex.reasoning_effort.clone(),
            config.codex.mode.clone(),
        ),
        BackendKindConfig::Claude => RuntimeSettings::new(config.claude.model.clone(), None, None),
    }
}

fn build_cli_session(
    config: &Config,
    backend: BackendKindConfig,
    workdir: &Path,
) -> Result<Box<dyn CliSession>> {
    let cli = match backend {
        BackendKindConfig::Claude => (
            BackendKind::Claude,
            config.claude.bin.clone(),
            config.claude.extra_env.clone(),
        ),
        BackendKindConfig::Codex => (
            BackendKind::Codex,
            config.codex.bin.clone(),
            config.codex.extra_env.clone(),
        ),
    };
    cli.spawn(workdir)
}

fn build_frontend_process_specs(
    config_path: &Path,
    config: &Config,
    worker_id: &str,
    _workdir: &Path,
    token_index: usize,
) -> Result<Vec<FrontendProcessSpec>> {
    let mut specs = vec![FrontendProcessSpec {
        label: format!("testing worker {worker_id}"),
        binary_name: "testing-frontend",
        args: vec![
            OsString::from("--config"),
            config_path.as_os_str().to_os_string(),
            OsString::from("--kind"),
            OsString::from("worker"),
            OsString::from("--worker-id"),
            OsString::from(worker_id),
        ],
    }];
    if !config.telegram.worker_tokens.is_empty() {
        specs.push(FrontendProcessSpec {
            label: format!("telegram worker {worker_id}"),
            binary_name: "telegram-frontend",
            args: vec![
                OsString::from("--config"),
                config_path.as_os_str().to_os_string(),
                OsString::from("--kind"),
                OsString::from("worker"),
                OsString::from("--worker-id"),
                OsString::from(worker_id),
                OsString::from("--token-index"),
                OsString::from(token_index.to_string()),
            ],
        });
    }
    Ok(specs)
}

fn spawn_frontend_processes(specs: &[FrontendProcessSpec]) -> Result<Vec<Child>> {
    let mut children = Vec::new();
    for spec in specs {
        children.push(
            spawn_frontend_process(spec)
                .with_context(|| format!("failed to start worker frontend {}", spec.label))?,
        );
    }
    Ok(children)
}

fn spawn_frontend_process(spec: &FrontendProcessSpec) -> Result<Child> {
    let binary = sibling_binary_path(spec.binary_name)?;
    let mut command = Command::new(&binary);
    command
        .args(&spec.args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_command_process_group(&mut command);
    command.spawn().map_err(Into::into)
}

fn sibling_binary_path(name: &str) -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let Some(dir) = exe.parent() else {
        bail!("failed to resolve worker executable directory");
    };
    #[cfg(windows)]
    let candidate = dir.join(format!("{name}.exe"));
    #[cfg(not(windows))]
    let candidate = dir.join(name);
    if !candidate.exists() {
        bail!(
            "required frontend binary not found next to worker executable: {}",
            candidate.display()
        );
    }
    Ok(candidate)
}

fn configure_command_process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
}

async fn run_refresh_loop(event_tx: mpsc::Sender<WorkerEvent>) {
    let mut interval = tokio::time::interval(Duration::from_millis(250));
    loop {
        interval.tick().await;
        if event_tx
            .send(WorkerEvent::refresh_terminal_session())
            .await
            .is_err()
        {
            break;
        }
    }
}

async fn run_terminal_exit_forwarder(
    terminal_exit: tokio::task::JoinHandle<Result<()>>,
    event_tx: mpsc::Sender<WorkerEvent>,
) {
    let event = match terminal_exit.await {
        Ok(Ok(())) => TerminalExitedEvent::success(),
        Ok(Err(err)) => TerminalExitedEvent::failure(err.to_string()),
        Err(err) => TerminalExitedEvent::failure(format!("join failed: {err}")),
    };
    let _ = event_tx.send(event.into()).await;
}

async fn cleanup_frontend_processes(mut frontend_processes: Vec<Child>) {
    for child in &mut frontend_processes {
        let _ = child.start_kill();
    }
    for child in &mut frontend_processes {
        let _ = child.wait().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ClaudeConfig, CodexConfig, ManagerConfig, RpcConfig};

    #[test]
    fn builds_testing_and_telegram_specs() {
        let config = Config {
            telegram: TelegramConfig {
                manager_token: "manager".to_string(),
                worker_tokens: vec!["worker-token".to_string()],
                allow_from: Vec::new(),
                group_reply_all: false,
                share_session_in_channel: false,
                poll_timeout_seconds: 30,
            },
            codex: CodexConfig {
                bin: "codex".to_string(),
                model: None,
                reasoning_effort: None,
                mode: None,
                extra_env: Vec::new(),
            },
            claude: ClaudeConfig::default(),
            manager: ManagerConfig::default(),
            rpc: RpcConfig::default(),
            state_path: None,
            log_level: "info".to_string(),
        };
        let config_path = PathBuf::from("/tmp/config.toml");
        let workdir = PathBuf::from("/tmp/workdir");

        let specs = build_frontend_process_specs(&config_path, &config, "worker-1", &workdir, 0)
            .expect("specs should build without checking binaries");
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].binary_name, "testing-frontend");
        assert_eq!(specs[1].binary_name, "telegram-frontend");
    }

    #[test]
    fn without_worker_tokens_builds_testing_only() {
        let config = Config {
            telegram: TelegramConfig {
                manager_token: "manager".to_string(),
                worker_tokens: Vec::new(),
                allow_from: Vec::new(),
                group_reply_all: false,
                share_session_in_channel: false,
                poll_timeout_seconds: 30,
            },
            codex: CodexConfig {
                bin: "codex".to_string(),
                model: None,
                reasoning_effort: None,
                mode: None,
                extra_env: Vec::new(),
            },
            claude: ClaudeConfig::default(),
            manager: ManagerConfig::default(),
            rpc: RpcConfig::default(),
            state_path: None,
            log_level: "info".to_string(),
        };
        let config_path = PathBuf::from("/tmp/config.toml");
        let workdir = PathBuf::from("/tmp/workdir");

        let specs = build_frontend_process_specs(&config_path, &config, "worker-1", &workdir, 0)
            .expect("specs should build without checking binaries");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].binary_name, "testing-frontend");
    }
}
