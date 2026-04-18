use anyhow::Result;
use frontend_common::{ManagerOutput, WorkerOutput};

pub fn serialize_manager_outputs(outputs: &[ManagerOutput]) -> Result<String> {
    Ok(serde_json::to_string(outputs)?)
}

pub fn serialize_worker_outputs(outputs: &[WorkerOutput]) -> Result<String> {
    Ok(serde_json::to_string(outputs)?)
}
