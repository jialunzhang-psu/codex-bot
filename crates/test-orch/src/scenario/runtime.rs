use std::path::PathBuf;

use anyhow::{Result, anyhow};

use crate::interact::bot::TestingFrontendClient;
use crate::interact::pty::{PtySnapshot, PtyTransport};
use crate::scenario::env::{ManagedChild, ScenarioSandbox};

#[derive(Debug, Clone)]
pub struct WorkerHandle {
    pub worker_id: String,
    pub workdir: PathBuf,
    pub backend: String,
    pub token_index: usize,
}

pub struct ScenarioRuntime {
    pub sandbox: Option<ScenarioSandbox>,
    pub frontend: Option<TestingFrontendClient>,
    pub daemon: Option<ManagedChild>,
    pub worker_pty: Option<PtyTransport>,
    pub claude_cli: Option<PtyTransport>,
    pub worker_handle: Option<WorkerHandle>,
    pub last_frontend_response: Option<String>,
    pub last_claude_snapshot: Option<PtySnapshot>,
    pub last_claude_message: Option<String>,
    pub vars: std::collections::HashMap<String, String>,
}

impl Default for ScenarioRuntime {
    fn default() -> Self {
        Self {
            sandbox: None,
            frontend: None,
            daemon: None,
            worker_pty: None,
            claude_cli: None,
            worker_handle: None,
            last_frontend_response: None,
            last_claude_snapshot: None,
            last_claude_message: None,
            vars: std::collections::HashMap::new(),
        }
    }
}

impl ScenarioRuntime {
    pub fn frontend_mut(&mut self) -> Result<&mut TestingFrontendClient> {
        self.frontend
            .as_mut()
            .ok_or_else(|| anyhow!("scenario frontend is not initialized"))
    }

    pub fn claude_cli_mut(&mut self) -> Result<&mut PtyTransport> {
        self.claude_cli
            .as_mut()
            .ok_or_else(|| anyhow!("Claude CLI is not attached"))
    }

    pub fn worker_handle(&self) -> Result<&WorkerHandle> {
        self.worker_handle
            .as_ref()
            .ok_or_else(|| anyhow!("scenario worker is not initialized"))
    }

    pub fn store_var(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.vars.insert(key.into(), value.into());
    }

    pub fn var(&self, key: &str) -> Result<&str> {
        self.vars
            .get(key)
            .map(String::as_str)
            .ok_or_else(|| anyhow!("scenario variable not set: {key}"))
    }

    pub fn set_last_frontend_response(&mut self, value: String) {
        self.last_frontend_response = Some(value);
    }

    pub fn last_frontend_response(&self) -> Result<&str> {
        self.last_frontend_response
            .as_deref()
            .ok_or_else(|| anyhow!("no frontend response recorded yet"))
    }

    pub async fn shutdown(&mut self) {
        if let Some(mut worker_pty) = self.worker_pty.take() {
            worker_pty.shutdown().await;
        }
        if let Some(mut claude_cli) = self.claude_cli.take() {
            claude_cli.shutdown().await;
        }
        if let Some(frontend) = self.frontend.as_mut() {
            let _ = frontend.shutdown().await;
        }
        self.frontend = None;
        if let Some(daemon) = self.daemon.as_mut() {
            let _ = daemon.shutdown().await;
        }
        self.daemon = None;
    }
}
