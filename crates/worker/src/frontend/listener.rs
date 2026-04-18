use std::path::Path;

use anyhow::Result;
use rpc::{RpcRoute, RpcServer};
use tokio::sync::mpsc;

use crate::event::{ListenerFailureEvent, WorkerEvent};
use crate::frontend::event::FrontendEvent;

pub struct FrontendListener {
    listener: RpcServer,
}

impl FrontendListener {
    pub fn bind(config_path: &Path, worker_id: &str) -> Result<Self> {
        let listener = RpcServer::new(
            config_path,
            RpcRoute::ToFrontendOfWorker {
                worker_id: worker_id.to_string(),
            },
        )?;
        Ok(Self { listener })
    }

    async fn accept_event(&self) -> Result<FrontendEvent> {
        let (request, reply) = self.listener.accept().await?;
        Ok(FrontendEvent::from_request(request, reply))
    }

    pub async fn run(self, tx: mpsc::Sender<WorkerEvent>) -> Result<()> {
        loop {
            match self.accept_event().await {
                Ok(event) => {
                    if tx.send(event.into()).await.is_err() {
                        return Ok(());
                    }
                }
                Err(error) => {
                    let failed = tx
                        .send(ListenerFailureEvent::frontend(error.to_string()).into())
                        .await;
                    return match failed {
                        Ok(()) => Ok(()),
                        Err(_) => Ok(()),
                    };
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use rpc::{RpcClient, RpcRoute, data::frontend_to_worker};
    use tokio::sync::mpsc;

    use super::FrontendListener;
    use crate::event::WorkerEvent;
    use crate::frontend::event::FrontendEvent;

    #[tokio::test(flavor = "multi_thread")]
    async fn listener_forwards_requests_into_event_channel() -> Result<()> {
        let tempdir = tempfile::tempdir()?;
        let config_path = tempdir.path().join("config.toml");
        std::fs::write(&config_path, "[rpc]\nsocket_dir = \"./sockets\"\n")?;
        let listener = FrontendListener::bind(&config_path, "worker-1")?;
        let (tx, mut rx) = mpsc::channel(4);
        let task = tokio::spawn(async move { listener.run(tx).await });

        let client = RpcClient::new(
            &config_path,
            RpcRoute::ToFrontendOfWorker {
                worker_id: "worker-1".to_string(),
            },
        )?;
        let request = frontend_to_worker::Data::SubmitInput {
            manager_session_id: Some("manager-1".to_string()),
            text: "hello".to_string(),
        };
        let _: serde_json::Value = client
            .request(&request)
            .await
            .err()
            .map(|_| serde_json::json!(null))
            .unwrap_or_else(|| serde_json::json!(null));

        match rx.recv().await {
            Some(WorkerEvent::Frontend(FrontendEvent::SubmitInput(event))) => {
                assert_eq!(event.manager_session_id.as_deref(), Some("manager-1"));
                assert_eq!(event.text, "hello");
            }
            other => anyhow::bail!("unexpected frontend event: {other:?}"),
        }

        task.abort();
        let _ = task.await;
        drop(rx);
        Ok(())
    }
}
