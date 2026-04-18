use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use portable_pty::CommandBuilder;
use rpc::{RpcClient, RpcRoute};
use tokio::process::{Child, Command};

use crate::interact::bot::TestingFrontendClient;
use crate::interact::pty::{PtyOptions, PtyTransport};
use crate::scenario::runtime::{ScenarioRuntime, WorkerHandle};

pub struct ScenarioSandbox {
    _root: tempfile::TempDir,
    config_path: PathBuf,
    workdir: PathBuf,
}

impl ScenarioSandbox {
    pub fn new() -> Result<Self> {
        let root = tempfile::tempdir().context("failed to create scenario tempdir")?;
        let workdir = root.path().join("workdir");
        std::fs::create_dir_all(&workdir).context("failed to create scenario workdir")?;
        let config_path = root.path().join("config.toml");
        let state_path = root.path().join("state.json");
        let claude_bin = claude_bin()?;
        let config = format!(
            "log_level = \"info\"\nstate_path = \"{}\"\n\n[rpc]\nsocket_dir = \"./codex-bot-sockets\"\n\n[telegram]\nmanager_token = \"123456:manager\"\nworker_tokens = [\"123456:worker0\", \"123456:worker1\"]\nallow_from = [123456789]\ngroup_reply_all = false\nshare_session_in_channel = false\npoll_timeout_seconds = 30\n\n[codex]\nbin = \"codex\"\nmodel = \"gpt-5.4\"\nreasoning_effort = \"high\"\nmode = \"suggest\"\nextra_env = []\n\n[claude]\nwork_dir = \"{}\"\nbin = \"{}\"\nextra_env = []\n\n[manager]\ndefault_backend = \"codex\"\n",
            state_path.display(),
            workdir.display(),
            claude_bin.display()
        );
        std::fs::write(&config_path, config).context("failed to write scenario config")?;
        Ok(Self {
            _root: root,
            config_path,
            workdir,
        })
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    pub fn unique_worker_id(&self) -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        format!("test-worker-{nanos}")
    }
}

pub struct ManagedChild {
    child: Child,
}

pub enum EnvStep {
    InitEnv,
    StartWorker {
        worker_id: Option<String>,
        backend: String,
        token_index: usize,
    },
    AttachWorkerClaudeCli,
}

impl ManagedChild {
    pub fn new(child: Child) -> Self {
        Self { child }
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
        Ok(())
    }
}

pub async fn execute_env_step(step: &EnvStep, runtime: &mut ScenarioRuntime) -> Result<()> {
    match step {
        EnvStep::InitEnv => {
            let sandbox = ScenarioSandbox::new()?;
            let (daemon, frontend) = init_environment(&sandbox).await?;
            runtime.sandbox = Some(sandbox);
            runtime.daemon = Some(daemon);
            runtime.frontend = Some(frontend);
            Ok(())
        }
        EnvStep::StartWorker {
            worker_id,
            backend,
            token_index,
        } => {
            let (worker_id, workdir, config_path) = {
                let sandbox = runtime
                    .sandbox
                    .as_ref()
                    .ok_or_else(|| anyhow!("InitEnv must run before StartWorker"))?;
                (
                    worker_id
                        .clone()
                        .unwrap_or_else(|| sandbox.unique_worker_id()),
                    sandbox.workdir().to_path_buf(),
                    sandbox.config_path().to_path_buf(),
                )
            };
            let mut frontend = runtime
                .frontend
                .take()
                .ok_or_else(|| anyhow!("InitEnv must run before StartWorker"))?;
            let worker_pty = start_worker(
                &config_path,
                &workdir,
                &mut frontend,
                &worker_id,
                backend,
                *token_index,
            )
            .await?;
            runtime.frontend = Some(frontend);
            runtime.worker_pty = Some(worker_pty);
            runtime.worker_handle = Some(WorkerHandle {
                worker_id: worker_id.clone(),
                workdir,
                backend: backend.clone(),
                token_index: *token_index,
            });
            runtime.store_var("worker_id", worker_id);
            Ok(())
        }
        EnvStep::AttachWorkerClaudeCli => {
            let cli = runtime
                .worker_pty
                .take()
                .ok_or_else(|| anyhow!("worker Claude CLI is not initialized"))?;
            runtime.claude_cli = Some(cli);
            Ok(())
        }
    }
}

pub async fn init_environment(
    sandbox: &ScenarioSandbox,
) -> Result<(ManagedChild, TestingFrontendClient)> {
    let mut daemon = spawn_daemon(sandbox)?;
    wait_for_manager_ready(&mut daemon.child, sandbox, Duration::from_secs(15)).await?;
    let frontend = spawn_testing_frontend(sandbox).await?;
    Ok((daemon, frontend))
}

pub async fn start_worker(
    config_path: &Path,
    workdir: &Path,
    frontend: &mut TestingFrontendClient,
    worker_id: &str,
    backend: &str,
    token_index: usize,
) -> Result<PtyTransport> {
    let binary = worker_binary()?;
    let mut command = CommandBuilder::new(binary);
    command.cwd(workdir);
    command.arg("--config");
    command.arg(config_path);
    command.arg("--worker-id");
    command.arg(worker_id);
    command.arg("--backend");
    command.arg(backend);
    command.arg("--workdir");
    command.arg(workdir);
    command.arg("--token-index");
    command.arg(token_index.to_string());
    command.env("CODEX_BOT_FORCE_TTY", "1");

    let pty = PtyTransport::spawn(command, PtyOptions::default())?;
    wait_for_frontend_worker_ready(&pty, config_path, worker_id, Duration::from_secs(15)).await?;

    let frontend_binary = cargo_bin("testing-frontend")?;
    let worker_socket_path = route_socket_path(
        config_path,
        RpcRoute::ToTestingFrontendOfWorker {
            worker_id: worker_id.to_string(),
        },
    )?;
    let worker_log_path = workdir.join(format!("testing-frontend-worker-{worker_id}.log"));
    let mut worker_frontend = Command::new(frontend_binary);
    worker_frontend
        .arg("--config")
        .arg(config_path)
        .arg("--kind")
        .arg("worker")
        .arg("--worker-id")
        .arg(worker_id)
        .stdin(Stdio::null())
        .stdout(log_stdio(&worker_log_path)?)
        .stderr(log_stdio(&worker_log_path)?)
        .current_dir(workdir);
    let mut worker_frontend = worker_frontend
        .spawn()
        .with_context(|| format!("failed to spawn testing-frontend worker for {worker_id}"))?;
    wait_for_socket_with_logs(
        &mut worker_frontend,
        &worker_socket_path,
        Duration::from_secs(15),
        &format!("testing-frontend worker {worker_id}"),
        &worker_log_path,
    )
    .await?;
    frontend.register_worker(worker_id.to_string(), worker_frontend);
    wait_for_worker_frontend_ready(frontend, worker_id, Duration::from_secs(15)).await?;
    Ok(pty)
}

async fn wait_for_worker_frontend_ready(
    frontend: &mut TestingFrontendClient,
    worker_id: &str,
    timeout: Duration,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match frontend
            .worker_command(worker_id, "bootstrap", "/status")
            .await
        {
            Ok(_) => return Ok(()),
            Err(err) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(err).context(format!(
                        "testing frontend worker {worker_id} did not become ready"
                    ));
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub async fn spawn_testing_frontend(sandbox: &ScenarioSandbox) -> Result<TestingFrontendClient> {
    let binary = cargo_bin("testing-frontend")?;
    let manager_socket_path =
        route_socket_path(sandbox.config_path(), RpcRoute::ToTestingFrontendOfManager)?;
    let manager_log_path = sandbox.workdir().join("testing-frontend-manager.log");
    let mut command = Command::new(binary);
    command
        .arg("--config")
        .arg(sandbox.config_path())
        .arg("--kind")
        .arg("manager")
        .stdin(Stdio::null())
        .stdout(log_stdio(&manager_log_path)?)
        .stderr(log_stdio(&manager_log_path)?)
        .current_dir(sandbox.workdir());
    let mut child = command
        .spawn()
        .context("failed to spawn testing-frontend manager")?;
    wait_for_socket_with_logs(
        &mut child,
        &manager_socket_path,
        Duration::from_secs(15),
        "testing-frontend manager",
        &manager_log_path,
    )
    .await?;
    Ok(TestingFrontendClient::new(
        child,
        sandbox.config_path().to_path_buf(),
    ))
}

pub fn spawn_daemon(sandbox: &ScenarioSandbox) -> Result<ManagedChild> {
    let binary = cargo_bin("daemon")?;
    let daemon_log_path = sandbox.workdir().join("daemon.log");
    let mut command = Command::new(binary);
    command
        .arg("--config")
        .arg(sandbox.config_path())
        .arg("--manager-frontend")
        .arg("none")
        .stdin(Stdio::null())
        .stdout(log_stdio(&daemon_log_path)?)
        .stderr(log_stdio(&daemon_log_path)?)
        .current_dir(sandbox.workdir());
    let child = command.spawn().context("failed to spawn daemon")?;
    Ok(ManagedChild::new(child))
}

async fn wait_for_manager_ready(
    daemon: &mut Child,
    sandbox: &ScenarioSandbox,
    timeout: Duration,
) -> Result<()> {
    let socket_path = route_socket_path(sandbox.config_path(), RpcRoute::ToManager)?;
    let log_path = sandbox.workdir().join("daemon.log");
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if socket_path.exists() {
            return Ok(());
        }
        if let Some(status) = daemon.try_wait().context("failed to poll daemon process")? {
            bail!(
                "daemon exited before manager socket {} became ready: {}\nlogs:\n{}",
                socket_path.display(),
                status,
                read_log(&log_path).trim()
            );
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "manager socket did not appear: {}\nlogs:\n{}",
                socket_path.display(),
                read_log(&log_path).trim()
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for_frontend_worker_ready(
    worker: &PtyTransport,
    config_path: &Path,
    worker_id: &str,
    timeout: Duration,
) -> Result<()> {
    let socket_path = route_socket_path(
        config_path,
        RpcRoute::ToFrontendOfWorker {
            worker_id: worker_id.to_string(),
        },
    )?;
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let snapshot = worker.snapshot()?;
        if socket_path.exists() && worker_cli_ready(&snapshot, worker_id) {
            return Ok(());
        }
        if worker_cli_failed(&snapshot) {
            bail!(
                "worker {} failed before local socket became ready\nscreen:\n{}\n\ntranscript:\n{}",
                worker_id,
                snapshot.screen,
                snapshot.transcript
            );
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "worker {} did not become ready; local socket {} missing or CLI not ready\nscreen:\n{}\n\ntranscript:\n{}",
                worker_id,
                socket_path.display(),
                snapshot.screen,
                snapshot.transcript
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for_socket_with_logs(
    process: &mut Child,
    socket_path: &Path,
    timeout: Duration,
    role: &str,
    log_path: &Path,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if socket_path.exists() {
            return Ok(());
        }
        if let Some(status) = process.try_wait().context("failed to poll child process")? {
            bail!(
                "{} exited before socket {} became ready: {}\nlogs:\n{}",
                role,
                socket_path.display(),
                status,
                read_log(log_path).trim()
            );
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "{} socket did not appear: {}\nlogs:\n{}",
                role,
                socket_path.display(),
                read_log(log_path).trim()
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn worker_cli_ready(snapshot: &crate::interact::pty::PtySnapshot, worker_id: &str) -> bool {
    snapshot.screen.contains(worker_id)
        || snapshot.transcript.contains(worker_id)
        || snapshot.screen.contains("Current worker idle.")
        || snapshot.transcript.contains("Current worker idle.")
        || snapshot.screen.contains("online")
        || snapshot.transcript.contains("online")
        || snapshot.screen.contains("Claude Code")
        || snapshot.transcript.contains("Claude Code")
        || snapshot.screen.contains("Welcome back!")
        || snapshot.transcript.contains("Welcome back!")
        || snapshot.screen.contains("? for shortcuts")
        || snapshot.transcript.contains("? for shortcuts")
}

fn worker_cli_failed(snapshot: &crate::interact::pty::PtySnapshot) -> bool {
    snapshot.screen.contains("Error:")
        || snapshot.transcript.contains("Error:")
        || snapshot.screen.contains("panicked at")
        || snapshot.transcript.contains("panicked at")
}

fn route_socket_path(config_path: &Path, route: RpcRoute) -> Result<PathBuf> {
    Ok(RpcClient::new(config_path, route)?
        .socket_path()
        .to_path_buf())
}

fn claude_bin() -> Result<PathBuf> {
    std::env::var_os("CLAUDE_BIN")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|home| PathBuf::from(home).join(".local/share/mamba/bin/claude"))
        })
        .filter(|path| path.exists())
        .or_else(|| {
            std::env::var_os("PATH").and_then(|path| {
                std::env::split_paths(&path)
                    .map(|entry| entry.join("claude"))
                    .find(|candidate| candidate.exists())
            })
        })
        .ok_or_else(|| anyhow!("failed to locate claude binary"))
}

fn log_stdio(path: &Path) -> Result<Stdio> {
    Ok(Stdio::from(
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("failed to open log file {}", path.display()))?,
    ))
}

fn read_log(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

fn worker_binary() -> Result<String> {
    Ok(cargo_bin("worker")?.to_string_lossy().to_string())
}

fn cargo_bin(name: &str) -> Result<PathBuf> {
    let key = format!("CARGO_BIN_EXE_{name}").replace('-', "_");
    if let Some(path) = std::env::var_os(&key) {
        return Ok(PathBuf::from(path));
    }

    let workspace_root = workspace_root()?;
    let binary_path = workspace_root.join("target").join("debug").join(name);
    ensure_workspace_binary(&workspace_root, name, &binary_path)?;
    Ok(binary_path)
}

fn workspace_root() -> Result<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            anyhow!(
                "failed to resolve workspace root from {}",
                manifest_dir.display()
            )
        })
}

fn ensure_workspace_binary(workspace_root: &Path, name: &str, binary_path: &Path) -> Result<()> {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};

    static BUILT: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let built = BUILT.get_or_init(|| Mutex::new(HashSet::new()));

    {
        let built = built
            .lock()
            .map_err(|_| anyhow!("binary build cache poisoned"))?;
        if built.contains(name) && binary_path.exists() {
            return Ok(());
        }
    }

    let status = std::process::Command::new("cargo")
        .arg("build")
        .arg("-p")
        .arg(name)
        .current_dir(workspace_root)
        .status()
        .with_context(|| format!("failed to run cargo build -p {name}"))?;
    if !status.success() {
        bail!("cargo build -p {} failed with status {}", name, status);
    }
    if !binary_path.exists() {
        bail!(
            "cargo build produced no binary at {}",
            binary_path.display()
        );
    }

    let mut built = built
        .lock()
        .map_err(|_| anyhow!("binary build cache poisoned"))?;
    built.insert(name.to_string());
    Ok(())
}
