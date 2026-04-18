use anyhow::Result;
use frontend_common::{ManagerOutput, ManagerReply, ManagerRuntime, ReplyFuture, WorkerOutput, WorkerReply, WorkerRuntime};
use rpc::RpcReply;

use crate::render::{serialize_manager_outputs, serialize_worker_outputs};

pub async fn run_manager_listener(
    server: rpc::RpcServer,
    runtime: ManagerRuntime,
    once: bool,
) -> Result<()> {
    loop {
        let (text, reply) = server.accept_line().await?;
        runtime
            .enqueue_transport_command(
                "testing:manager".to_string(),
                text.trim().to_string(),
                Box::new(LineManagerReply { reply }),
            )
            .await?;
        if once {
            break;
        }
    }
    Ok(())
}

pub async fn run_worker_listener(
    server: rpc::RpcServer,
    runtime: WorkerRuntime,
    once: bool,
) -> Result<()> {
    loop {
        let (text, reply) = server.accept_line().await?;
        runtime
            .enqueue_transport_command(text.trim().to_string(), Box::new(LineWorkerReply { reply }))
            .await?;
        if once {
            break;
        }
    }
    Ok(())
}

struct LineManagerReply {
    reply: RpcReply,
}

struct LineWorkerReply {
    reply: RpcReply,
}

impl ManagerReply for LineManagerReply {
    fn send(mut self: Box<Self>, outputs: Vec<ManagerOutput>) -> ReplyFuture {
        Box::pin(async move {
            let rendered = serialize_manager_outputs(&outputs)?;
            self.reply.send_line(&rendered).await
        })
    }
}

impl WorkerReply for LineWorkerReply {
    fn send(mut self: Box<Self>, outputs: Vec<WorkerOutput>) -> ReplyFuture {
        Box::pin(async move {
            let rendered = serialize_worker_outputs(&outputs)?;
            self.reply.send_line(&rendered).await
        })
    }
}
