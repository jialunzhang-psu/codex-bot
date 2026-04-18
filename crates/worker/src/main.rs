use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use tracing::error;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use session::BackendKindConfig;
use worker::entrypoint::init;

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
    #[arg(long)]
    worker_id: Option<String>,
    #[arg(long)]
    backend: BackendKindConfig,
    #[arg(long)]
    workdir: PathBuf,
    #[arg(long)]
    token_index: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stdout)
                .with_target(false)
                .compact(),
        )
        .init();

    let args = Args::parse();
    if let Err(err) = init(
        args.config,
        args.project,
        args.worker_id,
        args.backend,
        args.workdir,
        args.token_index,
    )
    .await
    {
        error!(error = %err, "worker exited with error");
        return Err(err);
    }
    Ok(())
}
