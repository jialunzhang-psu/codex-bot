mod config;
mod listener;
mod render;
mod telegram;

use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use clap::{Parser, ValueEnum};
use frontend_common::{ManagerRuntime, WorkerRuntime, manager_commands, resolve_config_path, worker_commands};

use crate::config::TelegramFrontendConfig;
use crate::listener::{run_manager_listener, run_worker_listener};
use crate::telegram::{TelegramClient, bot_commands};

#[derive(Parser)]
struct Args {
    #[arg(
        long,
        env = "BRIDGE_CONFIG",
        help = "Config file path. Defaults to ./config.toml, then ~/.codex-bot/config.toml"
    )]
    config: Option<PathBuf>,
    #[arg(long, value_enum)]
    kind: FrontendKind,
    #[arg(long)]
    worker_id: Option<String>,
    #[arg(long)]
    token_index: Option<usize>,
    #[arg(long)]
    once: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum FrontendKind {
    Manager,
    Worker,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let config_path = resolve_config_path(args.config);
    let config = TelegramFrontendConfig::load(&config_path)?;

    match args.kind {
        FrontendKind::Manager => {
            if args.worker_id.is_some() || args.token_index.is_some() {
                bail!("manager frontend does not accept --worker-id or --token-index");
            }
            let telegram = TelegramClient::new(config.manager_token.clone())?;
            telegram
                .set_my_commands(&bot_commands(&manager_commands()))
                .await?;
            let runtime = ManagerRuntime::new(&config_path)?;
            run_manager_listener(telegram, runtime, config.poll_timeout_seconds, args.once).await
        }
        FrontendKind::Worker => {
            let worker_id = args
                .worker_id
                .ok_or_else(|| anyhow!("worker frontend requires --worker-id"))?;
            let token_index = args
                .token_index
                .ok_or_else(|| anyhow!("worker frontend requires --token-index"))?;
            let telegram = TelegramClient::new(config.worker_token(token_index)?)?;
            telegram
                .set_my_commands(&bot_commands(&worker_commands()))
                .await?;
            let runtime = WorkerRuntime::new(&config_path, &worker_id)?;
            run_worker_listener(telegram, runtime, config.poll_timeout_seconds, args.once).await
        }
    }
}
