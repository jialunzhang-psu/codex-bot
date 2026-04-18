use anyhow::Result;
use rpc::data::worker_to_frontend;

use super::event::WorkerEvent;

pub async fn forward_submit_stream(
    client: rpc::RpcClient,
    request_id: u64,
    text: String,
    event_tx: tokio::sync::mpsc::Sender<WorkerEvent>,
) -> Result<()> {
    let mut stream = client
        .start_stream(&rpc::data::frontend_to_worker::Data::SubmitInput {
            manager_session_id: None,
            text,
        })
        .await?;
    let accepted = stream
        .next_item::<worker_to_frontend::Data>()
        .await?
        .ok_or_else(|| anyhow::anyhow!("socket closed without frontend response"))?;
    let _ = event_tx
        .send(WorkerEvent::BackendResponseReceived {
            request_id,
            response: Ok(accepted),
            done: false,
        })
        .await;
    loop {
        match stream.next_item::<worker_to_frontend::StreamItem>().await {
            Ok(Some(item)) => {
                let _ = event_tx
                    .send(WorkerEvent::BackendStreamItemReceived { request_id, item })
                    .await;
            }
            Ok(None) => break,
            Err(err) => {
                let _ = event_tx
                    .send(WorkerEvent::BackendFinished {
                        request_id,
                        result: Err(err.to_string()),
                    })
                    .await;
                return Ok(());
            }
        }
    }
    let _ = event_tx
        .send(WorkerEvent::BackendFinished {
            request_id,
            result: Ok(()),
        })
        .await;
    Ok(())
}
