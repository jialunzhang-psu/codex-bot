mod listener;
mod render;

use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use clap::{Parser, ValueEnum};
use frontend_common::{ManagerRuntime, WorkerRuntime, resolve_config_path, socket_protocol_note};
use rpc::{RpcRoute, RpcServer};

use crate::listener::{run_manager_listener, run_worker_listener};

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

    match args.kind {
        FrontendKind::Manager => {
            if args.worker_id.is_some() {
                bail!("manager frontend does not accept --worker-id");
            }
            let runtime = ManagerRuntime::new(&config_path)?;
            let server = RpcServer::new(&config_path, RpcRoute::ToTestingFrontendOfManager)?;
            println!(
                "testing-frontend manager listening ({})",
                socket_protocol_note()
            );
            run_manager_listener(server, runtime, args.once).await
        }
        FrontendKind::Worker => {
            let worker_id = args
                .worker_id
                .ok_or_else(|| anyhow!("worker frontend requires --worker-id"))?;
            let server = RpcServer::new(
                &config_path,
                RpcRoute::ToTestingFrontendOfWorker {
                    worker_id: worker_id.clone(),
                },
            )?;
            let runtime = WorkerRuntime::new(&config_path, &worker_id)?;
            println!(
                "testing-frontend worker {} listening ({})",
                worker_id,
                socket_protocol_note()
            );
            run_worker_listener(server, runtime, args.once).await
        }
    }
}
