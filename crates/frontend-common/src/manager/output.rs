use rpc::data::from_manager;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ManagerOutput {
    Response { response: from_manager::Data },
    Text { text: String },
    Error { message: String },
}
