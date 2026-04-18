mod command;
mod event;
mod handler;
mod output;
mod state;

use anyhow::{Result, anyhow};
use tokio::sync::mpsc;

use crate::event_loop::run_event_loop;

pub use command::manager_commands;
pub use event::ManagerEvent;
pub use output::ManagerOutput;

#[derive(Clone)]
pub struct ManagerRuntime {
    event_tx: mpsc::Sender<ManagerEvent>,
}

impl ManagerRuntime {
    pub fn new(config_path: &std::path::Path) -> Result<Self> {
        let (event_tx, event_rx) = mpsc::channel(64);
        let state = state::ManagerRuntimeState::new(config_path)?;
        tokio::spawn(async move {
            let _ = run_event_loop(state, event_rx).await;
        });
        Ok(Self { event_tx })
    }

    pub async fn enqueue_transport_command(
        &self,
        binding_key: String,
        text: String,
        reply: Box<dyn crate::reply::ManagerReply>,
    ) -> Result<()> {
        self.event_tx
            .send(ManagerEvent::TransportCommandReceived {
                binding_key,
                text,
                reply,
                event_tx: self.event_tx.clone(),
            })
            .await
            .map_err(|_| anyhow!("manager runtime event loop stopped"))
    }
}
