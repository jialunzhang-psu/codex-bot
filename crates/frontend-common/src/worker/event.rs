use crate::reply::WorkerReply;
use rpc::data::worker_to_frontend;

pub enum WorkerEvent {
    TransportCommandReceived {
        text: String,
        reply: Box<dyn WorkerReply>,
        event_tx: tokio::sync::mpsc::Sender<WorkerEvent>,
    },
    BackendResponseReceived {
        request_id: u64,
        response: std::result::Result<worker_to_frontend::Data, String>,
        done: bool,
    },
    BackendStreamItemReceived {
        request_id: u64,
        item: worker_to_frontend::StreamItem,
    },
    BackendFinished {
        request_id: u64,
        result: std::result::Result<(), String>,
    },
}
