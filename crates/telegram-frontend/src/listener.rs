use anyhow::Result;
use frontend_common::{ManagerOutput, ManagerReply, ManagerRuntime, ReplyFuture, WorkerOutput, WorkerReply, WorkerRuntime};

use crate::render;
use crate::telegram::{Message, TelegramClient, TelegramParseMode};

pub async fn run_manager_listener(
    telegram: TelegramClient,
    runtime: ManagerRuntime,
    poll_timeout_seconds: u64,
    once: bool,
) -> Result<()> {
    let mut offset = 0_i64;
    loop {
        let updates = telegram.get_updates(offset, poll_timeout_seconds).await?;
        for update in updates {
            offset = update.update_id + 1;
            if let Some(message) = update.message {
                handle_manager_message(&telegram, &runtime, message).await?;
            }
        }
        if once {
            break;
        }
    }
    Ok(())
}

pub async fn run_worker_listener(
    telegram: TelegramClient,
    runtime: WorkerRuntime,
    poll_timeout_seconds: u64,
    once: bool,
) -> Result<()> {
    let mut offset = 0_i64;
    loop {
        let updates = telegram.get_updates(offset, poll_timeout_seconds).await?;
        for update in updates {
            offset = update.update_id + 1;
            if let Some(message) = update.message {
                handle_worker_message(&telegram, &runtime, message).await?;
            }
        }
        if once {
            break;
        }
    }
    Ok(())
}

async fn handle_manager_message(
    telegram: &TelegramClient,
    runtime: &ManagerRuntime,
    message: Message,
) -> Result<()> {
    let Some(text) = message.text.as_deref().map(str::trim) else {
        return Ok(());
    };
    let binding_key = format!("telegram:manager:{}", message.chat.id);
    runtime
        .enqueue_transport_command(
            binding_key,
            text.to_string(),
            Box::new(TelegramManagerReply {
                telegram: telegram.clone(),
                chat_id: message.chat.id,
                reply_to_message_id: Some(message.message_id),
            }),
        )
        .await
}

async fn handle_worker_message(
    telegram: &TelegramClient,
    runtime: &WorkerRuntime,
    message: Message,
) -> Result<()> {
    let Some(text) = message.text.as_deref().map(str::trim) else {
        return Ok(());
    };
    runtime
        .enqueue_transport_command(
            text.to_string(),
            Box::new(TelegramWorkerReply {
                telegram: telegram.clone(),
                chat_id: message.chat.id,
                reply_to_message_id: Some(message.message_id),
            }),
        )
        .await
}

struct TelegramManagerReply {
    telegram: TelegramClient,
    chat_id: i64,
    reply_to_message_id: Option<i64>,
}

struct TelegramWorkerReply {
    telegram: TelegramClient,
    chat_id: i64,
    reply_to_message_id: Option<i64>,
}

impl ManagerReply for TelegramManagerReply {
    fn send(self: Box<Self>, outputs: Vec<ManagerOutput>) -> ReplyFuture {
        Box::pin(async move {
            let text = render::render_manager_outputs(&outputs);
            if text.trim().is_empty() {
                return Ok(());
            }
            self.telegram
                .send_message_formatted(
                    self.chat_id,
                    &text,
                    self.reply_to_message_id,
                    Some(TelegramParseMode::Html),
                )
                .await
        })
    }
}

impl WorkerReply for TelegramWorkerReply {
    fn send(self: Box<Self>, outputs: Vec<WorkerOutput>) -> ReplyFuture {
        Box::pin(async move {
            let text = render::render_worker_outputs(&outputs);
            if text.trim().is_empty() {
                return Ok(());
            }
            self.telegram
                .send_message_formatted(
                    self.chat_id,
                    &text,
                    self.reply_to_message_id,
                    Some(TelegramParseMode::Html),
                )
                .await
        })
    }
}
