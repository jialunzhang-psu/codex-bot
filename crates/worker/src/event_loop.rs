use std::sync::Arc;

use anyhow::{Result, anyhow};
use cli::CliSession;
use rpc::data::worker_to_frontend;
use session::{BackendKindConfig, RuntimeSettings, WorkerConversationState, WorkerRunState};
use tokio::sync::{Mutex, mpsc};

use crate::event::WorkerEvent;
use crate::handler::{EventHandler, LoopControl};

pub(crate) type SharedCliSession = Arc<std::sync::Mutex<Box<dyn CliSession>>>;

pub struct WorkerEventLoop {
    pub(crate) worker_id: String,
    pub(crate) backend: BackendKindConfig,
    pub(crate) workdir: std::path::PathBuf,
    pub(crate) runtime_settings: RuntimeSettings,
    pub(crate) cli_session: SharedCliSession,
    pub(crate) runtime: Mutex<WorkerRuntimeState>,
    pub(crate) next_run_id: Mutex<u64>,
    event_rx: Mutex<Option<mpsc::Receiver<WorkerEvent>>>,
}

#[derive(Debug, Clone)]
pub(crate) struct WorkerRuntimeState {
    pub(crate) conversation: Option<WorkerConversationState>,
    pub(crate) active_run: Option<WorkerRunState>,
    pub(crate) cancelled_run_id: Option<String>,
}

impl WorkerRuntimeState {
    pub(crate) fn new(workdir: &std::path::Path, native_session_id: Option<String>) -> Self {
        Self {
            conversation: Some(WorkerConversationState {
                manager_session_id: None,
                native_session_id,
                workdir: workdir.to_path_buf(),
                generation: 0,
                display_name: None,
            }),
            active_run: None,
            cancelled_run_id: None,
        }
    }

    pub(crate) fn status(&self) -> worker_to_frontend::RuntimeStatus {
        worker_to_frontend::RuntimeStatus {
            conversation: self.conversation.clone(),
            active_run: self.active_run.clone(),
            busy: self.active_run.is_some(),
        }
    }
}

impl WorkerEventLoop {
    pub fn new(
        worker_id: String,
        backend: BackendKindConfig,
        workdir: std::path::PathBuf,
        runtime_settings: RuntimeSettings,
        cli_session: Box<dyn CliSession>,
        event_rx: mpsc::Receiver<WorkerEvent>,
    ) -> Self {
        let initial_native_session_id = cli_session.current_native_session_id().ok().flatten();
        Self {
            worker_id,
            backend,
            workdir: workdir.clone(),
            runtime_settings,
            cli_session: Arc::new(std::sync::Mutex::new(cli_session)),
            runtime: Mutex::new(WorkerRuntimeState::new(&workdir, initial_native_session_id)),
            next_run_id: Mutex::new(0),
            event_rx: Mutex::new(Some(event_rx)),
        }
    }

    pub async fn run(self: Arc<Self>) -> Result<()> {
        let mut event_rx = self
            .event_rx
            .lock()
            .await
            .take()
            .ok_or_else(|| anyhow!("worker event receiver missing"))?;

        while let Some(event) = event_rx.recv().await {
            match event.handle(self.clone()).await? {
                LoopControl::Continue => {}
                LoopControl::Stop => break,
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex as StdMutex};

    use anyhow::Result;
    use cli::CliSession;
    use session::{BackendKindConfig, RuntimeSettings, SessionRequest};
    use tokio::sync::mpsc;

    use super::WorkerEventLoop;
    use crate::event::{ListenerFailureEvent, TerminalExitedEvent, WorkerEvent};

    #[derive(Default)]
    struct FakeCliState {
        current_native_session_id: Option<String>,
        sync_terminal_size_calls: usize,
    }

    struct FakeCliSession {
        state: Arc<StdMutex<FakeCliState>>,
    }

    impl FakeCliSession {
        fn new(state: Arc<StdMutex<FakeCliState>>) -> Self {
            Self { state }
        }
    }

    impl CliSession for FakeCliSession {
        fn send_prompt(&mut self, _request: SessionRequest) -> Result<cli::SpawnedRun> {
            anyhow::bail!("unused in worker event loop tests")
        }

        fn shutdown(&mut self) -> Result<()> {
            Ok(())
        }

        fn list_sessions(&self) -> Result<Vec<(cli::SessionId, Option<String>)>> {
            Ok(Vec::new())
        }

        fn current_native_session_id(&self) -> Result<Option<String>> {
            Ok(self.state.lock().unwrap().current_native_session_id.clone())
        }

        fn attach_terminal(
            &mut self,
            _settings: &RuntimeSettings,
            _native_session_id: Option<&str>,
        ) -> Result<()> {
            Ok(())
        }

        fn submit_terminal_prompt(&mut self, _request: SessionRequest) -> Result<cli::SpawnedRun> {
            anyhow::bail!("unused in worker event loop tests")
        }

        fn interrupt_terminal(&mut self, _allow_when_unknown: bool) -> Result<bool> {
            Ok(false)
        }

        fn sync_terminal_size(&mut self) -> Result<()> {
            self.state.lock().unwrap().sync_terminal_size_calls += 1;
            Ok(())
        }

        fn take_terminal_exit_handle(&mut self) -> Result<tokio::task::JoinHandle<Result<()>>> {
            Ok(tokio::spawn(async {
                std::future::pending::<()>().await;
                #[allow(unreachable_code)]
                Ok(())
            }))
        }

        fn switch_session(&mut self, _session_id: &cli::SessionId) -> Result<()> {
            Ok(())
        }

        fn delete_session(&mut self, _session_id: &cli::SessionId) -> Result<()> {
            Ok(())
        }

        fn get_history(&self, _limit: usize) -> Result<Vec<(cli::MessageRole, String)>> {
            Ok(Vec::new())
        }

        fn effective_runtime_settings(
            &self,
            requested: &RuntimeSettings,
        ) -> Result<RuntimeSettings> {
            Ok(requested.clone())
        }
    }

    fn build_event_loop(
        state: Arc<StdMutex<FakeCliState>>,
        rx: mpsc::Receiver<WorkerEvent>,
    ) -> Arc<WorkerEventLoop> {
        let cli_session: Box<dyn CliSession> = Box::new(FakeCliSession::new(state));
        Arc::new(WorkerEventLoop::new(
            "worker-test".to_string(),
            BackendKindConfig::Claude,
            std::env::temp_dir(),
            RuntimeSettings::default(),
            cli_session,
            rx,
        ))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn run_consumes_refresh_and_stop_events() -> Result<()> {
        let state = Arc::new(StdMutex::new(FakeCliState {
            current_native_session_id: Some("native-1".to_string()),
            ..Default::default()
        }));
        let (tx, rx) = mpsc::channel(8);
        let event_loop = build_event_loop(state.clone(), rx);
        tx.send(WorkerEvent::refresh_terminal_session()).await?;
        tx.send(TerminalExitedEvent::success().into()).await?;
        drop(tx);

        event_loop.clone().run().await?;

        let state = state.lock().unwrap();
        assert_eq!(state.sync_terminal_size_calls, 1);
        assert_eq!(
            event_loop
                .runtime
                .lock()
                .await
                .conversation
                .as_ref()
                .and_then(|conversation| conversation.native_session_id.as_deref()),
            Some("native-1")
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn run_fails_on_listener_failure_event() -> Result<()> {
        let state = Arc::new(StdMutex::new(FakeCliState::default()));
        let (tx, rx) = mpsc::channel(4);
        let event_loop = build_event_loop(state, rx);
        tx.send(ListenerFailureEvent::frontend("boom").into())
            .await?;
        drop(tx);

        let err = event_loop
            .run()
            .await
            .expect_err("listener failure should bubble up");
        assert!(err.to_string().contains("frontend listener exited: boom"));
        Ok(())
    }
}
