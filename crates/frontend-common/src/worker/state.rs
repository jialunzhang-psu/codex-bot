use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use rpc::{RpcClient, RpcRoute};

use crate::reply::WorkerReply;

pub(crate) struct WorkerRuntimeState {
    pub backend: RpcClient,
    pub next_request_id: u64,
    pub pending: HashMap<u64, PendingRequest>,
}

pub(crate) struct PendingRequest {
    pub outputs: Vec<super::output::WorkerOutput>,
    pub reply: Box<dyn WorkerReply>,
}

impl WorkerRuntimeState {
    pub fn new(config_path: &Path, worker_id: &str) -> Result<Self> {
        Ok(Self {
            backend: RpcClient::new(
                config_path,
                RpcRoute::ToFrontendOfWorker {
                    worker_id: worker_id.to_string(),
                },
            )?,
            next_request_id: 0,
            pending: HashMap::new(),
        })
    }
}
