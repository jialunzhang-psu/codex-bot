use anyhow::Result;
use tokio::sync::mpsc;

use crate::handler::EventHandler;

pub(crate) async fn run_event_loop<E>(mut ctx: E::Context, mut rx: mpsc::Receiver<E>) -> Result<()>
where
    E: EventHandler<Output = Result<()>> + Send + 'static,
    E::Context: Send + 'static,
{
    while let Some(event) = rx.recv().await {
        event.handle(&mut ctx).await?;
    }
    Ok(())
}
