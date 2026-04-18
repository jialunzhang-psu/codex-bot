use anyhow::Result;
use rpc::data::to_manager;

use crate::handler::{EventFuture, EventHandler};

use super::command::{ManagerCommand, help_text, parse_manager_command};
use super::event::ManagerEvent;
use super::output::ManagerOutput;
use super::state::ManagerRuntimeState;

impl EventHandler for ManagerEvent {
    type Context = ManagerRuntimeState;
    type Output = Result<()>;

    fn handle<'a>(self, ctx: &'a mut Self::Context) -> EventFuture<'a, Self::Output> {
        Box::pin(async move {
            match self {
                ManagerEvent::TransportCommandReceived {
                    binding_key,
                    text,
                    reply,
                    event_tx,
                } => handle_transport_command(ctx, binding_key, text, reply, event_tx).await,
                ManagerEvent::BackendRequestFinished { request_id, result } => {
                    handle_backend_finished(ctx, request_id, result).await
                }
            }
        })
    }
}

async fn handle_transport_command(
    ctx: &mut ManagerRuntimeState,
    binding_key: String,
    text: String,
    reply: Box<dyn crate::reply::ManagerReply>,
    event_tx: tokio::sync::mpsc::Sender<ManagerEvent>,
) -> Result<()> {
    let text = text.trim().to_string();
    match parse_manager_command(&text) {
        Ok(ManagerCommand::Help) => {
            reply.send(vec![ManagerOutput::Text { text: help_text() }]).await?;
            Ok(())
        }
        Ok(command) => {
            ctx.next_request_id += 1;
            let request_id = ctx.next_request_id;
            ctx.pending.insert(request_id, reply);
            let backend = ctx.backend.clone();
            tokio::spawn(async move {
                let result = execute_manager_command(&backend, binding_key, command)
                    .await
                    .map_err(|err| err.to_string());
                let _ = event_tx
                    .send(ManagerEvent::BackendRequestFinished { request_id, result })
                    .await;
            });
            Ok(())
        }
        Err(usage) => {
            reply.send(vec![ManagerOutput::Error { message: usage }]).await?;
            Ok(())
        }
    }
}

async fn handle_backend_finished(
    ctx: &mut ManagerRuntimeState,
    request_id: u64,
    result: std::result::Result<Vec<ManagerOutput>, String>,
) -> Result<()> {
    if let Some(reply) = ctx.pending.remove(&request_id) {
        let outputs = match result {
            Ok(outputs) => outputs,
            Err(message) => vec![ManagerOutput::Error { message }],
        };
        reply.send(outputs).await?;
    }
    Ok(())
}

async fn execute_manager_command(
    backend: &rpc::RpcClient,
    binding_key: String,
    command: ManagerCommand,
) -> Result<Vec<ManagerOutput>> {
    let request = match command {
        ManagerCommand::Workers => to_manager::Data::ListWorkers,
        ManagerCommand::Bots => to_manager::Data::ListBots,
        ManagerCommand::Ls { path } => to_manager::Data::ListDirectory { binding_key, path },
        ManagerCommand::Cd { path } => to_manager::Data::ChangeDirectory { binding_key, path },
        ManagerCommand::Launch {
            backend,
            bot,
            workdir,
        } => to_manager::Data::LaunchWorker {
            binding_key,
            backend,
            bot,
            workdir,
        },
        ManagerCommand::Help => {
            return Ok(vec![ManagerOutput::Text { text: help_text() }]);
        }
    };
    request_output(backend, request).await
}

async fn request_output(
    backend: &rpc::RpcClient,
    request: to_manager::Data,
) -> Result<Vec<ManagerOutput>> {
    let response: rpc::data::from_manager::Data = backend.request(&request).await?;
    Ok(vec![ManagerOutput::Response { response }])
}
