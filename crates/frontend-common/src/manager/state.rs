use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use rpc::{RpcClient, RpcRoute};

use crate::reply::ManagerReply;

pub(crate) struct ManagerRuntimeState {
    pub backend: RpcClient,
    pub next_request_id: u64,
    pub pending: HashMap<u64, Box<dyn ManagerReply>>,
}

impl ManagerRuntimeState {
    pub fn new(config_path: &Path) -> Result<Self> {
        Ok(Self {
            backend: RpcClient::new(config_path, RpcRoute::ToManager)?,
            next_request_id: 0,
            pending: HashMap::new(),
        })
    }
}
