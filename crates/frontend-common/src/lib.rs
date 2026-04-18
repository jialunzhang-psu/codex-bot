pub mod manager;
pub mod worker;

mod config_path;
mod event_loop;
mod handler;
mod reply;

use serde::{Deserialize, Serialize};

pub use config_path::resolve_config_path;
pub use manager::{ManagerEvent, ManagerOutput, ManagerRuntime, manager_commands};
pub use reply::{ManagerReply, ReplyFuture, WorkerReply};
pub use worker::{WorkerEvent, WorkerOutput, WorkerRuntime, worker_commands};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSpec {
    pub command: String,
    pub description: String,
    pub usage: String,
}

impl CommandSpec {
    pub fn new(command: &str, description: &str, usage: &str) -> Self {
        Self {
            command: command.to_string(),
            description: description.to_string(),
            usage: usage.to_string(),
        }
    }
}

pub fn socket_protocol_note() -> &'static str {
    "send one UTF-8 command per line; receive one UTF-8 response line"
}
