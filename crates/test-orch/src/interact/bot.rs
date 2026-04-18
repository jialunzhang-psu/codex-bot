use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use tokio::process::Child;

pub use frontend_common::{ManagerOutput, WorkerOutput};
use rpc::{RpcClient, RpcRoute};

pub struct TestingFrontendClient {
    manager_process: Child,
    config_path: PathBuf,
    worker_processes: HashMap<String, Child>,
}

impl TestingFrontendClient {
    pub fn new(manager_process: Child, config_path: PathBuf) -> Self {
        Self {
            manager_process,
            config_path,
            worker_processes: HashMap::new(),
        }
    }

    pub fn register_worker(&mut self, worker_id: impl Into<String>, process: Child) {
        let worker_id = worker_id.into();
        self.worker_processes.insert(worker_id, process);
    }

    pub async fn manager_command(&mut self, _chat: &str, text: &str) -> Result<Vec<ManagerOutput>> {
        let raw = request(
            &self.config_path,
            RpcRoute::ToTestingFrontendOfManager,
            text,
        )
        .await?;
        Ok(serde_json::from_str(&raw)?)
    }

    pub async fn worker_command(
        &mut self,
        worker_id: &str,
        _chat: &str,
        text: &str,
    ) -> Result<Vec<WorkerOutput>> {
        if !self.worker_processes.contains_key(worker_id) {
            return Err(anyhow!(
                "testing frontend worker not registered for {worker_id}"
            ));
        }
        let raw = request(
            &self.config_path,
            RpcRoute::ToTestingFrontendOfWorker {
                worker_id: worker_id.to_string(),
            },
            text,
        )
        .await?;
        Ok(serde_json::from_str(&raw)?)
    }

    pub async fn manager_status(
        &mut self,
        chat: &str,
        session_id: &str,
    ) -> Result<Vec<ManagerOutput>> {
        self.manager_command(chat, &format!("/status {session_id}"))
            .await
    }

    pub async fn worker_status(
        &mut self,
        worker_id: &str,
        chat: &str,
    ) -> Result<Vec<WorkerOutput>> {
        self.worker_command(worker_id, chat, "/status").await
    }

    pub async fn worker_prompt(
        &mut self,
        worker_id: &str,
        chat: &str,
        prompt: &str,
    ) -> Result<Vec<WorkerOutput>> {
        self.worker_command(worker_id, chat, prompt).await
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        for process in self.worker_processes.values_mut() {
            let _ = process.start_kill();
        }
        for process in self.worker_processes.values_mut() {
            let _ = process.wait().await;
        }
        let _ = self.manager_process.start_kill();
        let _ = self.manager_process.wait().await;
        Ok(())
    }
}

async fn request(config_path: &Path, route: RpcRoute, text: &str) -> Result<String> {
    let client = RpcClient::new(config_path, route)?;
    tokio::time::timeout(Duration::from_secs(30), client.request_line(text))
        .await
        .with_context(|| {
            format!(
                "timed out waiting for testing frontend reply via config {}",
                config_path.display()
            )
        })?
}
