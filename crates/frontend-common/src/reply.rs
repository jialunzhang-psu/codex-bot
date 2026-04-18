use std::future::Future;
use std::pin::Pin;

use anyhow::Result;
pub type ReplyFuture = Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>;

pub trait ManagerReply: Send + 'static {
    fn send(self: Box<Self>, outputs: Vec<crate::manager::ManagerOutput>) -> ReplyFuture;
}

pub trait WorkerReply: Send + 'static {
    fn send(self: Box<Self>, outputs: Vec<crate::worker::WorkerOutput>) -> ReplyFuture;
}
