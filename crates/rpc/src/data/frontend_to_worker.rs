use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Data {
    GetStatus,
    SubscribeAttached,
    SubmitInput {
        manager_session_id: Option<String>,
        text: String,
    },
    StopRun,
}
