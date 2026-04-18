use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use frontend_common::socket_protocol_note;
use rpc::{RpcClient, RpcRoute};
use serde::{Deserialize, Serialize};
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use tracing_subscriber::{
    EnvFilter, Registry, layer::SubscriberExt, reload, util::SubscriberInitExt,
};

use daemon::config::Config;
use daemon::manager::ManagerService;
use daemon::manager::server::ManagerServer;
use daemon::process::{configure_command_process_group, kill_process_group};

#[derive(Parser)]
struct Args {
    #[arg(
        long,
        env = "BRIDGE_CONFIG",
        help = "Config file path. Defaults to ./config.toml, then ~/.codex-bot/config.toml"
    )]
    config: Option<PathBuf>,
    #[arg(long, env = "BRIDGE_PROJECT")]
    project: Option<String>,
    #[arg(long = "manager-frontend", value_enum)]
    manager_frontend: Vec<ManagerFrontendArg>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
enum ManagerFrontendArg {
    Telegram,
    Testing,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrontendKind {
    Telegram,
    Testing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FrontendLaunchSpec {
    kind: FrontendKind,
    label: String,
    binary_name: &'static str,
    args: Vec<OsString>,
    socket_path: Option<PathBuf>,
}

#[derive(Debug)]
struct FrontendProcessHandle {
    spec: FrontendLaunchSpec,
    child: Child,
    restart_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct DaemonIdentity {
    config_path: PathBuf,
    socket_path: PathBuf,
    project: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DaemonLease {
    pid: u32,
    identity: DaemonIdentity,
}

struct DaemonLeaseGuard {
    lease_path: PathBuf,
    pid: u32,
}

impl Drop for DaemonLeaseGuard {
    fn drop(&mut self) {
        let Ok(current) = read_daemon_lease(&self.lease_path) else {
            return;
        };
        let Some(current) = current else {
            return;
        };
        if current.pid == self.pid {
            let _ = fs::remove_file(&self.lease_path);
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let log_handle = init_tracing("info");
    if let Err(err) = run(log_handle).await {
        error!(error = %err, "daemon exited with error");
        return Err(err);
    }
    Ok(())
}

async fn run(log_handle: reload::Handle<EnvFilter, Registry>) -> Result<()> {
    let args = Args::parse();
    let config_path = resolve_config_path(args.config);
    let selected_frontends = normalize_frontends(args.manager_frontend)?;
    run_daemon(log_handle, config_path, args.project, selected_frontends).await
}

async fn run_daemon(
    log_handle: reload::Handle<EnvFilter, Registry>,
    config_path: PathBuf,
    project: Option<String>,
    selected_frontends: Vec<FrontendKind>,
) -> Result<()> {
    info!(config = %config_path.display(), frontends = ?selected_frontends, "starting daemon");
    let config = load_project_config(&log_handle, &config_path)?;
    let manager_client = RpcClient::new(&config_path, RpcRoute::ToManager)?;
    let socket_path = manager_client.socket_path().to_path_buf();
    let identity = DaemonIdentity {
        config_path: normalize_runtime_path(&config_path),
        socket_path: normalize_runtime_path(&socket_path),
        project: project.clone(),
    };
    terminate_other_daemon_processes()?;
    let _lease = acquire_daemon_lease(&identity)?;
    let manager_service = ManagerService::new(config_path.clone(), config.clone(), project.clone());
    let manager_server = ManagerServer::new(Arc::clone(&manager_service));
    let shutdown = CancellationToken::new();
    print_manager_startup_banner(&socket_path, &config_path, project.as_deref());
    let frontend = {
        let shutdown = shutdown.clone();
        let config_path = config_path.clone();
        tokio::spawn(async move {
            run_frontend_supervisor(selected_frontends, config_path, shutdown).await
        })
    };
    info!(socket = %socket_path.display(), "manager daemon listening");
    let result = tokio::select! {
        result = manager_server.run_until(&config_path, Some(shutdown.clone())) => result,
        result = frontend => result.context("frontend supervisor task failed")?,
        _ = shutdown_signal() => Ok(()),
    };
    shutdown.cancel();
    result
}

fn normalize_frontends(selected: Vec<ManagerFrontendArg>) -> Result<Vec<FrontendKind>> {
    if selected.is_empty() {
        return Ok(vec![FrontendKind::Telegram]);
    }
    if selected.contains(&ManagerFrontendArg::None) {
        if selected.len() > 1 {
            bail!("--manager-frontend none cannot be combined with other values");
        }
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for kind in [ManagerFrontendArg::Telegram, ManagerFrontendArg::Testing] {
        if selected.contains(&kind) {
            out.push(match kind {
                ManagerFrontendArg::Telegram => FrontendKind::Telegram,
                ManagerFrontendArg::Testing => FrontendKind::Testing,
                ManagerFrontendArg::None => unreachable!("none handled above"),
            });
        }
    }
    Ok(out)
}

async fn run_frontend_supervisor(
    selected: Vec<FrontendKind>,
    config_path: PathBuf,
    shutdown: CancellationToken,
) -> Result<()> {
    if selected.is_empty() {
        info!("manager frontend supervision disabled");
        shutdown.cancelled().await;
        return Ok(());
    }

    let specs = build_frontend_specs(&selected, &config_path)?;
    let mut children = Vec::new();
    for spec in specs {
        let child = spawn_frontend_process(&spec)?;
        let pid = child.id().unwrap_or_default();
        print_frontend_startup_banner(&spec, pid);
        info!(frontend = %frontend_spec_label(&spec), pid, "spawned frontend");
        children.push(FrontendProcessHandle {
            spec,
            child,
            restart_count: 0,
        });
    }

    loop {
        if shutdown.is_cancelled() {
            break;
        }

        for handle in &mut children {
            if let Some(status) = handle
                .child
                .try_wait()
                .context("failed checking frontend status")?
            {
                handle.restart_count += 1;
                warn!(frontend = %frontend_spec_label(&handle.spec), ?status, restart_count = handle.restart_count, "frontend exited unexpectedly");
                if handle.restart_count >= 5 {
                    bail!(
                        "frontend exited too many times: {}",
                        frontend_spec_label(&handle.spec)
                    );
                }
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_millis(500 * handle.restart_count as u64)) => {}
                }
                if shutdown.is_cancelled() {
                    break;
                }
                let replacement = spawn_frontend_process(&handle.spec)?;
                let pid = replacement.id().unwrap_or_default();
                print_frontend_startup_banner(&handle.spec, pid);
                info!(frontend = %frontend_spec_label(&handle.spec), pid, "restarted frontend");
                handle.child = replacement;
            }
        }

        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
        }
    }

    for handle in &mut children {
        terminate_child_process_group(&mut handle.child).await?;
    }
    Ok(())
}

fn build_frontend_specs(
    selected: &[FrontendKind],
    config_path: &Path,
) -> Result<Vec<FrontendLaunchSpec>> {
    selected
        .iter()
        .map(|frontend| manager_frontend_spec(*frontend, config_path))
        .collect()
}

fn manager_frontend_spec(frontend: FrontendKind, config_path: &Path) -> Result<FrontendLaunchSpec> {
    Ok(match frontend {
        FrontendKind::Telegram => FrontendLaunchSpec {
            kind: frontend,
            label: "telegram manager".to_string(),
            binary_name: "telegram-frontend",
            args: vec![
                OsString::from("--config"),
                config_path.as_os_str().to_os_string(),
                OsString::from("--kind"),
                OsString::from("manager"),
            ],
            socket_path: None,
        },
        FrontendKind::Testing => FrontendLaunchSpec {
            kind: frontend,
            label: "testing manager".to_string(),
            binary_name: "testing-frontend",
            args: vec![
                OsString::from("--config"),
                config_path.as_os_str().to_os_string(),
                OsString::from("--kind"),
                OsString::from("manager"),
            ],
            socket_path: Some(route_socket_path(
                config_path,
                RpcRoute::ToTestingFrontendOfManager,
            )?),
        },
    })
}


fn spawn_frontend_process(spec: &FrontendLaunchSpec) -> Result<Child> {
    let binary = sibling_binary_path(spec.binary_name)?;

    let mut command = Command::new(&binary);
    command
        .args(&spec.args)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    configure_command_process_group(&mut command);
    command
        .spawn()
        .with_context(|| format!("failed to start frontend {}", binary.display()))
}

fn print_manager_startup_banner(socket_path: &Path, config_path: &Path, project: Option<&str>) {
    println!("manager socket: {}", socket_path.display());
    println!("config path: {}", config_path.display());
    if let Some(project) = project {
        println!("project: {}", project);
    }
}

fn route_socket_path(config_path: &Path, route: RpcRoute) -> Result<PathBuf> {
    Ok(RpcClient::new(config_path, route)?
        .socket_path()
        .to_path_buf())
}

fn print_frontend_startup_banner(spec: &FrontendLaunchSpec, pid: u32) {
    if let Some(socket_path) = &spec.socket_path {
        println!(
            "{} launched pid={} socket={} protocol={}",
            spec.label,
            pid,
            socket_path.display(),
            socket_protocol_note()
        );
    } else {
        println!("{} launched pid={}", spec.label, pid);
    }
}

fn frontend_spec_label(spec: &FrontendLaunchSpec) -> &str {
    &spec.label
}

async fn terminate_child_process_group(child: &mut Child) -> Result<()> {
    if let Some(pid) = child.id() {
        let _ = kill_process_group(pid, libc::SIGTERM);
        for _ in 0..20 {
            if let Some(_status) = child.try_wait().context("failed checking child status")? {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let _ = kill_process_group(pid, libc::SIGKILL);
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
    Ok(())
}

fn sibling_binary_path(name: &str) -> Result<PathBuf> {
    let exe = std::env::current_exe().context("failed to resolve current daemon executable")?;
    let Some(dir) = exe.parent() else {
        bail!("failed to resolve daemon executable directory");
    };
    #[cfg(windows)]
    let candidate = dir.join(format!("{name}.exe"));
    #[cfg(not(windows))]
    let candidate = dir.join(name);
    if !candidate.exists() {
        bail!(
            "required frontend binary not found next to daemon executable: {}",
            candidate.display()
        );
    }
    Ok(candidate)
}

fn acquire_daemon_lease(identity: &DaemonIdentity) -> Result<DaemonLeaseGuard> {
    let lease_path = daemon_lease_path(&identity.socket_path);
    if let Some(existing) = read_daemon_lease(&lease_path)? {
        handle_existing_lease(&existing, identity, &lease_path)?;
    } else {
        cleanup_stale_socket(&identity.socket_path)?;
    }

    let lease = DaemonLease {
        pid: std::process::id(),
        identity: identity.clone(),
    };
    write_daemon_lease(&lease_path, &lease)?;
    Ok(DaemonLeaseGuard {
        lease_path,
        pid: lease.pid,
    })
}

fn handle_existing_lease(
    existing: &DaemonLease,
    desired: &DaemonIdentity,
    lease_path: &Path,
) -> Result<()> {
    if existing.identity != *desired {
        if process_is_running(existing.pid) {
            bail!(
                "daemon lease {} is owned by live pid {} with a different identity",
                lease_path.display(),
                existing.pid
            );
        }
        warn!(pid = existing.pid, lease = %lease_path.display(), "removing stale daemon lease with different identity");
        let _ = fs::remove_file(lease_path);
        cleanup_stale_socket(&desired.socket_path)?;
        return Ok(());
    }

    if process_is_running(existing.pid) {
        info!(pid = existing.pid, lease = %lease_path.display(), "stopping previous daemon instance before takeover");
        terminate_process(existing.pid)?;
    }
    let _ = fs::remove_file(lease_path);
    cleanup_stale_socket(&desired.socket_path)?;
    Ok(())
}

fn daemon_lease_path(socket_path: &Path) -> PathBuf {
    socket_path.with_extension("daemon.lock")
}

fn read_daemon_lease(path: &Path) -> Result<Option<DaemonLease>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read daemon lease {}", path.display()))?;
    let lease = serde_json::from_str(&raw)
        .with_context(|| format!("failed to decode daemon lease {}", path.display()))?;
    Ok(Some(lease))
}

fn write_daemon_lease(path: &Path, lease: &DaemonLease) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let payload = serde_json::to_vec_pretty(lease).context("failed to encode daemon lease")?;
    fs::write(path, payload)
        .with_context(|| format!("failed to write daemon lease {}", path.display()))
}

fn cleanup_stale_socket(socket_path: &Path) -> Result<()> {
    if !socket_path.exists() {
        return Ok(());
    }
    if std::os::unix::net::UnixStream::connect(socket_path).is_ok() {
        let owners = active_socket_owner_pids(socket_path)?;
        if owners.is_empty() {
            bail!("manager socket {} is already active", socket_path.display());
        }
        info!(socket = %socket_path.display(), owners = ?owners, "stopping active manager socket owners before startup");
        kill_socket_owners(socket_path)?;
        std::thread::sleep(Duration::from_millis(100));
        if std::os::unix::net::UnixStream::connect(socket_path).is_ok() {
            bail!("manager socket {} is already active", socket_path.display());
        }
    }
    fs::remove_file(socket_path)
        .with_context(|| format!("failed to remove stale socket {}", socket_path.display()))?;
    Ok(())
}

fn active_socket_owner_pids(socket_path: &Path) -> Result<Vec<u32>> {
    let output = StdCommand::new("fuser")
        .arg(socket_path)
        .output()
        .with_context(|| format!("failed to run fuser for {}", socket_path.display()))?;
    if output.status.success() || !output.stdout.is_empty() || !output.stderr.is_empty() {
        let text = String::from_utf8_lossy(&output.stdout).to_string()
            + &String::from_utf8_lossy(&output.stderr);
        return Ok(parse_fuser_pids(&text));
    }
    Ok(Vec::new())
}

fn kill_socket_owners(socket_path: &Path) -> Result<()> {
    let status = StdCommand::new("fuser")
        .arg("-k")
        .arg(socket_path)
        .status()
        .with_context(|| format!("failed to run fuser -k for {}", socket_path.display()))?;
    if status.success() {
        return Ok(());
    }
    bail!(
        "failed to kill active owner for manager socket {}",
        socket_path.display()
    )
}

fn parse_fuser_pids(text: &str) -> Vec<u32> {
    text.split(|ch: char| !ch.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse::<u32>().ok())
        .collect()
}

#[cfg(test)]
fn parse_fuser_pids_for_test(text: &str) -> Vec<u32> {
    parse_fuser_pids(text)
}

#[cfg(test)]
mod socket_owner_tests {
    use super::parse_fuser_pids_for_test;

    #[test]
    fn parse_fuser_pid_extracts_pid_from_output() {
        assert_eq!(parse_fuser_pids_for_test("12345\n"), vec![12345]);
        assert_eq!(
            parse_fuser_pids_for_test("/tmp/a.sock: 98765\n"),
            vec![98765]
        );
        assert_eq!(
            parse_fuser_pids_for_test("/tmp/a.sock:1499512\n"),
            vec![1499512]
        );
        assert!(parse_fuser_pids_for_test("no pid here").is_empty());
    }

    #[test]
    fn parse_fuser_pids_extracts_multiple_pids() {
        assert_eq!(
            parse_fuser_pids_for_test("/tmp/a.sock: 123 456 789\n"),
            vec![123, 456, 789]
        );
    }
}

fn terminate_process(pid: u32) -> Result<()> {
    #[cfg(unix)]
    {
        send_signal(pid, libc::SIGTERM)?;
        for _ in 0..20 {
            if !process_is_running(pid) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        send_signal(pid, libc::SIGKILL)?;
        for _ in 0..20 {
            if !process_is_running(pid) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        bail!("process {} did not exit after SIGKILL", pid);
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        Ok(())
    }
}

#[cfg(unix)]
fn send_signal(pid: u32, signal: libc::c_int) -> Result<()> {
    let rc = unsafe { libc::kill(pid as i32, signal) };
    if rc == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(err).with_context(|| format!("failed to signal pid {}", pid))
    }
}

fn process_is_running(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let rc = unsafe { libc::kill(pid as i32, 0) };
        if rc == 0 {
            return true;
        }
        let err = std::io::Error::last_os_error();
        err.raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

fn terminate_other_daemon_processes() -> Result<()> {
    #[cfg(unix)]
    {
        let current_pid = std::process::id();
        let current_exe =
            std::env::current_exe().context("failed to resolve current daemon executable")?;
        for entry in fs::read_dir("/proc").context("failed to read /proc for daemon cleanup")? {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            let file_name = entry.file_name();
            let Some(pid_str) = file_name.to_str() else {
                continue;
            };
            let Ok(pid) = pid_str.parse::<u32>() else {
                continue;
            };
            if pid == current_pid {
                continue;
            }
            if !is_matching_daemon_process(pid, &current_exe) {
                continue;
            }
            info!(pid, "stopping other daemon process before startup");
            terminate_process(pid)?;
        }
    }
    Ok(())
}

fn is_matching_daemon_process(pid: u32, current_exe: &Path) -> bool {
    #[cfg(unix)]
    {
        let exe_path = PathBuf::from(format!("/proc/{pid}/exe"));
        let cmdline_path = PathBuf::from(format!("/proc/{pid}/cmdline"));
        let Ok(exe) = fs::read_link(&exe_path) else {
            return false;
        };
        let exe = normalize_proc_exe_path(&exe);
        let current_exe = normalize_proc_exe_path(current_exe);
        if exe != current_exe {
            return false;
        }
        let Ok(cmdline) = fs::read(&cmdline_path) else {
            return false;
        };
        let args = cmdline
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .filter_map(|part| std::str::from_utf8(part).ok())
            .collect::<Vec<_>>();
        args.first()
            .is_some_and(|arg| arg.ends_with("/daemon") || *arg == "daemon")
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        let _ = current_exe;
        false
    }
}

fn normalize_proc_exe_path(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    if let Some(stripped) = raw.strip_suffix(" (deleted)") {
        PathBuf::from(stripped)
    } else {
        path.to_path_buf()
    }
}

#[cfg(test)]
fn normalize_proc_exe_path_for_test(path: &Path) -> PathBuf {
    normalize_proc_exe_path(path)
}

#[cfg(test)]
mod process_match_tests {
    use super::normalize_proc_exe_path_for_test;
    use std::path::Path;

    #[test]
    fn normalize_proc_exe_path_strips_deleted_suffix() {
        let normalized =
            normalize_proc_exe_path_for_test(Path::new("/tmp/daemon-binary (deleted)"));
        assert_eq!(normalized, Path::new("/tmp/daemon-binary"));
    }
}

fn load_project_config(
    log_handle: &reload::Handle<EnvFilter, Registry>,
    config_path: &PathBuf,
) -> Result<Config> {
    let config = Config::load(config_path)?;
    reload_log_level(log_handle, &config.log_level);
    info!(log_level = %config.log_level, "configuration loaded");
    Ok(config)
}

fn init_tracing(default_level: &str) -> reload::Handle<EnvFilter, Registry> {
    let (filter_layer, handle) = reload::Layer::new(build_env_filter(default_level));
    tracing_subscriber::registry()
        .with(filter_layer)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stdout)
                .with_target(false)
                .compact(),
        )
        .init();
    handle
}

fn reload_log_level(handle: &reload::Handle<EnvFilter, Registry>, default_level: &str) {
    if std::env::var_os("RUST_LOG").is_some() {
        return;
    }

    let _ = handle.reload(build_env_filter(default_level));
}

fn build_env_filter(default_level: &str) -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level.to_string()))
}

fn resolve_config_path(explicit: Option<PathBuf>) -> PathBuf {
    resolve_config_path_with(
        explicit,
        PathBuf::from("config.toml"),
        default_config_path(),
    )
}

fn default_config_path() -> Option<PathBuf> {
    default_config_path_from_home(
        std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from),
    )
}

fn resolve_config_path_with(
    explicit: Option<PathBuf>,
    local: PathBuf,
    default: Option<PathBuf>,
) -> PathBuf {
    if let Some(path) = explicit {
        return path;
    }
    if local.exists() {
        return local;
    }
    default.unwrap_or(local)
}

fn default_config_path_from_home(home: Option<PathBuf>) -> Option<PathBuf> {
    home.map(|home| home.join(".codex-bot").join("config.toml"))
}

fn normalize_runtime_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(path)
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut sigterm =
            signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use clap::Parser;
    use tempfile::TempDir;

    use super::FrontendKind;
    use super::{
        Args, DaemonIdentity, DaemonLease, ManagerFrontendArg, acquire_daemon_lease,
        daemon_lease_path, default_config_path_from_home, normalize_frontends,
        normalize_runtime_path, resolve_config_path_with,
    };

    #[test]
    fn resolve_config_path_prefers_explicit_path() {
        let explicit = PathBuf::from("/tmp/custom-config.toml");
        let local = PathBuf::from("/tmp/config.toml");
        let default = Some(PathBuf::from("/tmp/.codex-bot/config.toml"));
        assert_eq!(
            resolve_config_path_with(Some(explicit.clone()), local, default),
            explicit
        );
    }

    #[test]
    fn resolve_config_path_prefers_local_config_when_present() {
        let temp_dir = TempDir::new().expect("temp dir");
        let local = temp_dir.path().join("config.toml");
        let default = temp_dir.path().join(".codex-bot").join("config.toml");
        fs::write(&local, "").expect("write local config");
        assert_eq!(
            resolve_config_path_with(None, local.clone(), Some(default)),
            local
        );
    }

    #[test]
    fn resolve_config_path_falls_back_to_default_home_path() {
        let temp_dir = TempDir::new().expect("temp dir");
        let local = temp_dir.path().join("config.toml");
        let default =
            default_config_path_from_home(Some(temp_dir.path().to_path_buf())).expect("default");
        assert_eq!(
            resolve_config_path_with(None, local, Some(default.clone())),
            default
        );
    }

    #[test]
    fn cli_parses_repeated_frontends() {
        let parsed = Args::try_parse_from([
            "daemon",
            "--manager-frontend",
            "telegram",
            "--manager-frontend",
            "testing",
        ])
        .expect("daemon args should parse");
        assert_eq!(
            normalize_frontends(parsed.manager_frontend).unwrap(),
            vec![FrontendKind::Telegram, FrontendKind::Testing]
        );
    }

    #[test]
    fn normalize_frontends_rejects_none_combo() {
        assert!(
            normalize_frontends(vec![ManagerFrontendArg::None, ManagerFrontendArg::Telegram])
                .is_err()
        );
    }

    #[test]
    fn daemon_lease_round_trip_uses_socket_lock_path() {
        let temp_dir = TempDir::new().expect("temp dir");
        let identity = DaemonIdentity {
            config_path: temp_dir.path().join("config.toml"),
            socket_path: temp_dir.path().join("manager.sock"),
            project: Some("demo".to_string()),
        };
        let guard = acquire_daemon_lease(&identity).expect("lease should be created");
        let lease_path = daemon_lease_path(&identity.socket_path);
        let raw = fs::read_to_string(&lease_path).expect("lease should exist");
        let lease: DaemonLease = serde_json::from_str(&raw).expect("lease should decode");
        assert_eq!(lease.pid, std::process::id());
        assert_eq!(lease.identity, identity);
        drop(guard);
        assert!(!lease_path.exists());
    }

    #[test]
    fn normalize_runtime_path_makes_relative_paths_absolute() {
        let normalized = normalize_runtime_path(PathBuf::from("config.toml").as_path());
        assert!(normalized.is_absolute());
        assert!(normalized.ends_with("config.toml"));
    }
}
