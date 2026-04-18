use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{Result, anyhow};

use crate::event::{ListenerFailureEvent, TerminalExitedEvent, WorkerEvent};
use crate::event_loop::WorkerEventLoop;

pub type EventFuture = Pin<Box<dyn Future<Output = Result<LoopControl>> + Send + 'static>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopControl {
    Continue,
    Stop,
}

pub trait EventHandler {
    fn handle(self, event_loop: Arc<WorkerEventLoop>) -> EventFuture;

    fn handle_while_busy(self, event_loop: Arc<WorkerEventLoop>) -> EventFuture
    where
        Self: Sized,
    {
        self.handle(event_loop)
    }
}

impl EventHandler for ListenerFailureEvent {
    fn handle(self, _event_loop: Arc<WorkerEventLoop>) -> EventFuture {
        Box::pin(async move {
            Err(anyhow!(
                "{source} listener exited: {error}",
                source = self.source.as_str(),
                error = self.error
            ))
        })
    }
}

impl EventHandler for TerminalExitedEvent {
    fn handle(self, _event_loop: Arc<WorkerEventLoop>) -> EventFuture {
        Box::pin(async move {
            match self.error {
                Some(error) => Err(anyhow!("worker terminal exited with error: {error}")),
                None => Ok(LoopControl::Stop),
            }
        })
    }
}

impl EventHandler for WorkerEvent {
    fn handle(self, event_loop: Arc<WorkerEventLoop>) -> EventFuture {
        match self {
            WorkerEvent::RefreshTerminalSession => Box::pin(async move {
                crate::frontend::handler::refresh_terminal_session_id(event_loop.as_ref()).await?;
                let _ =
                    crate::frontend::handler::with_cli_session(event_loop.as_ref(), |session| {
                        session.sync_terminal_size()
                    });
                Ok(LoopControl::Continue)
            }),
            WorkerEvent::TerminalExited(event) => event.handle(event_loop),
            WorkerEvent::Frontend(event) => event.handle(event_loop),
            WorkerEvent::ListenerFailed(event) => event.handle(event_loop),
        }
    }
}
