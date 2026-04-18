use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use rpc::{
    RpcReply, RpcRoute, RpcServer,
    data::{from_manager::Data as ManagerResponse, to_manager::Data as ManagerRequest},
};

use crate::manager::ManagerService;

#[derive(Clone)]
pub struct ManagerServer {
    service: Arc<ManagerService>,
}

impl ManagerServer {
    pub fn new(service: Arc<ManagerService>) -> Self {
        Self { service }
    }

    pub async fn run_until(
        &self,
        config_path: &Path,
        shutdown: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<()> {
        let listener = RpcServer::new(config_path, RpcRoute::ToManager).with_context(|| {
            format!(
                "failed to bind manager server from {}",
                config_path.display()
            )
        })?;
        loop {
            let accepted = async {
                listener
                    .accept::<ManagerRequest>()
                    .await
                    .context("failed to accept manager client")
            };
            let (request, reply) = if let Some(token) = shutdown.as_ref() {
                tokio::select! {
                    _ = token.cancelled() => return Ok(()),
                    accepted = accepted => accepted?,
                }
            } else {
                accepted.await?
            };
            let service = Arc::clone(&self.service);
            tokio::spawn(async move {
                let _ = handle_connection(request, reply, service).await;
            });
        }
    }
}

async fn handle_connection(
    request: ManagerRequest,
    mut reply: RpcReply,
    service: Arc<ManagerService>,
) -> Result<()> {
    match dispatch_request(request, &service).await {
        Ok(response) => reply.send_ok(&response).await,
        Err(err) => reply.send_err(err.to_string()).await,
    }
}

async fn dispatch_request(
    request: ManagerRequest,
    service: &Arc<ManagerService>,
) -> Result<ManagerResponse> {
    match request {
        ManagerRequest::ListWorkers => Ok(ManagerResponse::WorkerList {
            workers: service.list_workers().await?,
        }),
        ManagerRequest::ListBots => Ok(ManagerResponse::BotList {
            bots: service.list_bots().await?,
        }),
        ManagerRequest::ListDirectory { binding_key, path } => {
            Ok(ManagerResponse::DirectoryListing {
                listing: service.list_directory(&binding_key, path).await?,
            })
        }
        ManagerRequest::ChangeDirectory { binding_key, path } => {
            Ok(ManagerResponse::WorkingDirectory {
                cwd: service.change_directory(&binding_key, &path).await?,
            })
        }
        ManagerRequest::LaunchWorker {
            binding_key,
            backend,
            bot,
            workdir,
        } => Ok(ManagerResponse::WorkerLaunched {
            launch: service
                .launch_worker(&binding_key, backend, bot, workdir)
                .await?,
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use anyhow::Result;
    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;

    use crate::config::Config;
    use rpc::{RpcClient, RpcRoute};

    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_until_cleans_up_manager_socket_files_on_shutdown() -> Result<()> {
        let dir = tempdir()?;
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            concat!(
                "log_level = \"info\"\n",
                "[rpc]\n",
                "socket_dir = \"./sockets\"\n\n",
                "[telegram]\n",
                "manager_token = \"manager\"\n",
                "worker_tokens = [\"worker\"]\n",
                "allow_from = []\n",
                "group_reply_all = false\n",
                "share_session_in_channel = false\n",
                "poll_timeout_seconds = 30\n\n",
                "[codex]\n",
                "extra_env = []\n\n",
                "[claude]\n",
                "extra_env = []\n",
            ),
        )?;
        let socket_path = RpcClient::new(&config_path, RpcRoute::ToManager)?
            .socket_path()
            .to_path_buf();
        let config = Config::load(&config_path, None)?;
        let service = crate::manager::ManagerService::new(config_path.clone(), config, None);
        let server = ManagerServer::new(service);
        let shutdown = CancellationToken::new();
        let task = tokio::spawn({
            let config_path = config_path.clone();
            let shutdown = shutdown.clone();
            async move { server.run_until(&config_path, Some(shutdown)).await }
        });

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            if socket_path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(socket_path.exists());

        shutdown.cancel();
        task.await??;
        assert!(!socket_path.exists());
        Ok(())
    }
}
