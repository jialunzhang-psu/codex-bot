mod account_cli;
mod accounts;
mod app;
mod codex;
mod config;
mod state;
mod telegram;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tokio::process::Command;
use tracing::{error, info, warn};
use tracing_subscriber::{
    EnvFilter, Registry, layer::SubscriberExt, reload, util::SubscriberInitExt,
};

use crate::app::BridgeApp;
use crate::codex::CodexClient;
use crate::config::Config;
use crate::state::StateStore;
use crate::telegram::TelegramClient;

#[derive(Parser)]
struct Args {
    #[arg(long, env = "BRIDGE_CONFIG", default_value = "config.toml")]
    config: PathBuf,
    #[arg(long, env = "BRIDGE_PROJECT")]
    project: Option<String>,
    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Subcommand)]
enum CliCommand {
    #[command(alias = "account")]
    Accounts {
        #[command(subcommand)]
        command: account_cli::AccountsCommand,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let log_handle = init_tracing("info");
    if let Err(err) = run(log_handle).await {
        error!(error = %err, "codex-bot exited with error");
        return Err(err);
    }
    Ok(())
}

async fn run(log_handle: reload::Handle<EnvFilter, Registry>) -> Result<()> {
    let args = Args::parse();
    match args.command {
        Some(CliCommand::Accounts { command }) => {
            account_cli::run(&args.config, args.project.as_deref(), command).await
        }
        None => run_bridge(log_handle, args.config, args.project).await,
    }
}

async fn run_bridge(
    log_handle: reload::Handle<EnvFilter, Registry>,
    config_path: PathBuf,
    project: Option<String>,
) -> Result<()> {
    info!(config = %config_path.display(), "starting codex-bot");
    let discovered_projects = Config::discover_projects(&config_path)?;
    if project.is_none() && discovered_projects.len() > 1 {
        info!(
            count = discovered_projects.len(),
            "starting supervisor mode for multi-project cc-connect config"
        );
        return run_supervisor(config_path, discovered_projects).await;
    }

    let config = Config::load(&config_path, project.as_deref())?;
    reload_log_level(&log_handle, &config.log_level);

    if let Some(project_name) = &config.project_name {
        info!(project = %project_name, "loaded project from cc-connect config");
    }
    info!(
        work_dir = %config.codex.work_dir.display(),
        log_level = %config.log_level,
        "configuration loaded"
    );

    let state_path = config.default_state_path(&config_path);
    let telegram = TelegramClient::new(config.telegram.token.clone())?;
    let codex = Arc::new(CodexClient::new(&config.codex));
    let state = Arc::new(StateStore::load(
        state_path,
        config.default_runtime_settings(),
    )?);
    info!("fetching Telegram bot identity");
    let app = BridgeApp::new(config, telegram, state, codex).await?;
    app.run().await
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

async fn run_supervisor(config_path: PathBuf, projects: Vec<String>) -> Result<()> {
    let exe = std::env::current_exe()?;
    let mut children = Vec::with_capacity(projects.len());

    for project in projects {
        let mut command = Command::new(&exe);
        command
            .arg("--config")
            .arg(&config_path)
            .arg("--project")
            .arg(&project)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit());

        let child = command.spawn()?;
        info!(project = %project, pid = child.id().unwrap_or_default(), "spawned worker");
        children.push((project, child));
    }

    loop {
        tokio::select! {
            _ = shutdown_signal() => {
                info!("shutdown signal received, stopping workers");
                break;
            }
            _ = tokio::time::sleep(Duration::from_secs(1)) => {
                let mut index = 0usize;
                while index < children.len() {
                    let (project, child) = &mut children[index];
                    if let Some(status) = child.try_wait()? {
                        warn!(project = %project, status = %status, "worker exited");
                        children.remove(index);
                    } else {
                        index += 1;
                    }
                }

                if children.is_empty() {
                    info!("all workers exited");
                    return Ok(());
                }
            }
        }
    }

    for (_, child) in &mut children {
        let _ = child.start_kill();
    }
    for (project, child) in &mut children {
        let _ = child.wait().await;
        info!(project = %project, "worker stopped");
    }
    Ok(())
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
