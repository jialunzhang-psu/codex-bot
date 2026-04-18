use anyhow::Result;
use rpc::data::frontend_to_worker;

use crate::handler::{EventFuture, EventHandler};

use super::command::{WorkerCommand, help_text, parse_worker_command};
use super::event::WorkerEvent;
use super::output::WorkerOutput;
use super::state::{PendingRequest, WorkerRuntimeState};
use super::stream::forward_submit_stream;

impl EventHandler for WorkerEvent {
    type Context = WorkerRuntimeState;
    type Output = Result<()>;

    fn handle<'a>(self, ctx: &'a mut Self::Context) -> EventFuture<'a, Self::Output> {
        Box::pin(async move {
            match self {
                WorkerEvent::TransportCommandReceived {
                    text,
                    reply,
                    event_tx,
                } => handle_transport_command(ctx, text, reply, event_tx).await,
                WorkerEvent::BackendResponseReceived {
                    request_id,
                    response,
                    done,
                } => handle_backend_response(ctx, request_id, response, done).await,
                WorkerEvent::BackendStreamItemReceived { request_id, item } => {
                    handle_backend_stream_item(ctx, request_id, item).await
                }
                WorkerEvent::BackendFinished { request_id, result } => {
                    handle_backend_finished(ctx, request_id, result).await
                }
            }
        })
    }
}

async fn handle_transport_command(
    ctx: &mut WorkerRuntimeState,
    text: String,
    reply: Box<dyn crate::reply::WorkerReply>,
    event_tx: tokio::sync::mpsc::Sender<WorkerEvent>,
) -> Result<()> {
    match parse_worker_command(&text) {
        WorkerCommand::Help => {
            reply.send(vec![WorkerOutput::Text { text: help_text() }]).await?;
            Ok(())
        }
        WorkerCommand::Status => {
            start_pending_request(ctx, reply, |request_id, client| async move {
                let response = client
                    .request(&frontend_to_worker::Data::GetStatus)
                    .await
                    .map_err(|err| err.to_string());
                WorkerEvent::BackendResponseReceived {
                    request_id,
                    response,
                    done: true,
                }
            }, event_tx)
            .await
        }
        WorkerCommand::Stop => {
            start_pending_request(ctx, reply, |request_id, client| async move {
                let response = client
                    .request(&frontend_to_worker::Data::StopRun)
                    .await
                    .map_err(|err| err.to_string());
                WorkerEvent::BackendResponseReceived {
                    request_id,
                    response,
                    done: true,
                }
            }, event_tx)
            .await
        }
        WorkerCommand::Submit { text } => {
            ctx.next_request_id += 1;
            let request_id = ctx.next_request_id;
            ctx.pending.insert(
                request_id,
                PendingRequest {
                    outputs: Vec::new(),
                    reply,
                },
            );
            let client = ctx.backend.clone();
            tokio::spawn(async move {
                let _ = forward_submit_stream(client, request_id, text, event_tx).await;
            });
            Ok(())
        }
    }
}

async fn start_pending_request<F, Fut>(
    ctx: &mut WorkerRuntimeState,
    reply: Box<dyn crate::reply::WorkerReply>,
    build_event: F,
    event_tx: tokio::sync::mpsc::Sender<WorkerEvent>,
) -> Result<()>
where
    F: FnOnce(u64, rpc::RpcClient) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = WorkerEvent> + Send + 'static,
{
    ctx.next_request_id += 1;
    let request_id = ctx.next_request_id;
    ctx.pending.insert(
        request_id,
        PendingRequest {
            outputs: Vec::new(),
            reply,
        },
    );
    let client = ctx.backend.clone();
    tokio::spawn(async move {
        let event = build_event(request_id, client).await;
        let _ = event_tx.send(event).await;
    });
    Ok(())
}

async fn handle_backend_response(
    ctx: &mut WorkerRuntimeState,
    request_id: u64,
    response: std::result::Result<rpc::data::worker_to_frontend::Data, String>,
    done: bool,
) -> Result<()> {
    match response {
        Ok(response) => {
            if done {
                if let Some(pending) = ctx.pending.remove(&request_id) {
                    let mut outputs = pending.outputs;
                    outputs.push(WorkerOutput::Response { response });
                    pending.reply.send(outputs).await?;
                }
            } else if let Some(pending) = ctx.pending.get_mut(&request_id) {
                pending.outputs.push(WorkerOutput::Response { response });
            }
        }
        Err(message) => fail_pending_request(ctx, request_id, message).await?,
    }
    Ok(())
}

async fn handle_backend_stream_item(
    ctx: &mut WorkerRuntimeState,
    request_id: u64,
    item: rpc::data::worker_to_frontend::StreamItem,
) -> Result<()> {
    if let Some(pending) = ctx.pending.get_mut(&request_id) {
        pending.outputs.push(WorkerOutput::StreamItem { item });
    }
    Ok(())
}

async fn handle_backend_finished(
    ctx: &mut WorkerRuntimeState,
    request_id: u64,
    result: std::result::Result<(), String>,
) -> Result<()> {
    match result {
        Ok(()) => {
            if let Some(pending) = ctx.pending.remove(&request_id) {
                pending.reply.send(pending.outputs).await?;
            }
        }
        Err(message) => fail_pending_request(ctx, request_id, message).await?,
    }
    Ok(())
}

async fn fail_pending_request(
    ctx: &mut WorkerRuntimeState,
    request_id: u64,
    message: String,
) -> Result<()> {
    if let Some(pending) = ctx.pending.remove(&request_id) {
        pending
            .reply
            .send(vec![WorkerOutput::Error { message }])
            .await?;
    }
    Ok(())
}
