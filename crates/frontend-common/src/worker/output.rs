use rpc::data::worker_to_frontend;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerOutput {
    Response {
        response: worker_to_frontend::Data,
    },
    StreamItem {
        item: worker_to_frontend::StreamItem,
    },
    Text {
        text: String,
    },
    Error {
        message: String,
    },
}
