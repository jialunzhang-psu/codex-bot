use crate::reply::ManagerReply;

pub enum ManagerEvent {
    TransportCommandReceived {
        binding_key: String,
        text: String,
        reply: Box<dyn ManagerReply>,
        event_tx: tokio::sync::mpsc::Sender<ManagerEvent>,
    },
    BackendRequestFinished {
        request_id: u64,
        result: std::result::Result<Vec<super::output::ManagerOutput>, String>,
    },
}
