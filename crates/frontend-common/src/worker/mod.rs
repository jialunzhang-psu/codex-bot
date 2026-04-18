mod command;
mod event;
mod handler;
mod output;
mod state;
mod stream;

use anyhow::{Result, anyhow};
use tokio::sync::mpsc;

use crate::event_loop::run_event_loop;

pub use command::worker_commands;
pub use event::WorkerEvent;
pub use output::WorkerOutput;

#[derive(Clone)]
pub struct WorkerRuntime {
    event_tx: mpsc::Sender<WorkerEvent>,
}

impl WorkerRuntime {
    pub fn new(config_path: &std::path::Path, worker_id: &str) -> Result<Self> {
        let (event_tx, event_rx) = mpsc::channel(64);
        let state = state::WorkerRuntimeState::new(config_path, worker_id)?;
        tokio::spawn(async move {
            let _ = run_event_loop(state, event_rx).await;
        });
        Ok(Self { event_tx })
    }

    pub async fn enqueue_transport_command(
        &self,
        text: String,
        reply: Box<dyn crate::reply::WorkerReply>,
    ) -> Result<()> {
        self.event_tx
            .send(WorkerEvent::TransportCommandReceived {
                text,
                reply,
                event_tx: self.event_tx.clone(),
            })
            .await
            .map_err(|_| anyhow!("worker runtime event loop stopped"))
    }
}
