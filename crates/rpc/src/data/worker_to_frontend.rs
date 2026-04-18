use serde::{Deserialize, Serialize};
use session::{BackendKindConfig, SessionEvent, WorkerConversationState, WorkerRunState};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RuntimeStatus {
    pub conversation: Option<WorkerConversationState>,
    pub active_run: Option<WorkerRunState>,
    pub busy: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub worker_id: String,
    pub backend: BackendKindConfig,
    pub workdir: std::path::PathBuf,
    pub runtime: RuntimeStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Data {
    Status {
        status: Status,
    },
    SubmitAccepted {
        run_id: String,
        manager_session_id: Option<String>,
    },
    StopAccepted {
        stopped: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamItem {
    Event(SessionEvent),
}
