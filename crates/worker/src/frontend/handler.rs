use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use cli::CliSession;
use rpc::{RpcReply, data::worker_to_frontend};
use session::{
    BackendKind, RuntimeSettings, SessionEvent, SessionEventPayload, SessionOrigin, SessionOutcome,
    SessionRequest, WorkerConversationState, WorkerRunState,
};
use tracing::warn;

use crate::event_loop::WorkerEventLoop;
use crate::frontend::event::{
    FrontendEvent, GetStatusEvent, StopRunEvent, SubmitInputEvent, SubscribeAttachedEvent,
};
use crate::handler::{EventFuture, EventHandler, LoopControl};

pub(crate) fn with_cli_session<T>(
    event_loop: &WorkerEventLoop,
    f: impl FnOnce(&mut dyn CliSession) -> Result<T>,
) -> Result<T> {
    let mut session = event_loop
        .cli_session
        .lock()
        .map_err(|_| anyhow!("worker cli session lock poisoned"))?;
    f(session.as_mut())
}

fn frontend_backend_kind(event_loop: &WorkerEventLoop) -> BackendKind {
    match event_loop.backend {
        session::BackendKindConfig::Codex => BackendKind::Codex,
        session::BackendKindConfig::Claude => BackendKind::Claude,
    }
}

fn runtime_settings_clone(event_loop: &WorkerEventLoop) -> RuntimeSettings {
    event_loop.runtime_settings.clone()
}

fn workdir_path(event_loop: &WorkerEventLoop) -> PathBuf {
    event_loop.workdir.clone()
}

fn worker_id_string(event_loop: &WorkerEventLoop) -> String {
    event_loop.worker_id.clone()
}

pub(crate) async fn refresh_terminal_session_id(event_loop: &WorkerEventLoop) -> Result<()> {
    let Some(native_session_id) =
        with_cli_session(event_loop, |session| session.current_native_session_id())?
    else {
        return Ok(());
    };
    let mut runtime = event_loop.runtime.lock().await;
    let conversation = runtime
        .conversation
        .get_or_insert_with(|| WorkerConversationState {
            manager_session_id: None,
            native_session_id: Some(native_session_id.clone()),
            workdir: event_loop.workdir.clone(),
            generation: 0,
            display_name: None,
        });
    if conversation.native_session_id.as_deref() != Some(native_session_id.as_str()) {
        conversation.native_session_id = Some(native_session_id);
    }
    Ok(())
}

pub(crate) fn attach_native_session_id(
    event: SessionEvent,
    native_session_id: Option<String>,
) -> SessionEvent {
    if let Some(native_session_id) = native_session_id {
        event.with_native_session_id(native_session_id)
    } else {
        event
    }
}

async fn write_frontend_response(
    reply: &mut RpcReply,
    response: &worker_to_frontend::Data,
) -> Result<()> {
    reply.send_json(response).await
}

async fn write_frontend_stream_item(
    reply: &mut RpcReply,
    item: &worker_to_frontend::StreamItem,
) -> Result<()> {
    reply.send_json(item).await
}

async fn apply_frontend_event(event_loop: &WorkerEventLoop, event: &SessionEvent) {
    let mut runtime = event_loop.runtime.lock().await;
    let conversation = runtime
        .conversation
        .get_or_insert_with(|| WorkerConversationState {
            manager_session_id: Some(event.manager_session_id.clone()),
            native_session_id: event.native_session_id.clone(),
            workdir: event_loop.workdir.clone(),
            generation: 0,
            display_name: None,
        });
    conversation.manager_session_id = Some(event.manager_session_id.clone());
    if let Some(native_session_id) = &event.native_session_id {
        conversation.native_session_id = Some(native_session_id.clone());
    }
    if let SessionEventPayload::BackendSessionIdentified { native_session_id } = &event.payload {
        conversation.native_session_id = Some(native_session_id.clone());
    }
    if matches!(event.payload, SessionEventPayload::RunFinished) {
        runtime.active_run = None;
        runtime.cancelled_run_id = None;
    }
    if matches!(event.payload, SessionEventPayload::RunCancelled) {
        let run_matches = event
            .run_id
            .as_deref()
            .map(|run_id| runtime.cancelled_run_id.as_deref() == Some(run_id))
            .unwrap_or(false);
        if run_matches || runtime.cancelled_run_id.is_none() {
            runtime.active_run = None;
            runtime.cancelled_run_id = None;
        }
    }
}

pub(crate) async fn emit_frontend_event(
    event_loop: &WorkerEventLoop,
    reply: &mut RpcReply,
    event: SessionEvent,
) -> Result<()> {
    apply_frontend_event(event_loop, &event).await;
    write_frontend_stream_item(reply, &worker_to_frontend::StreamItem::Event(event)).await
}

async fn runtime_status(event_loop: &WorkerEventLoop) -> worker_to_frontend::Status {
    let runtime = event_loop.runtime.lock().await;
    worker_to_frontend::Status {
        worker_id: event_loop.worker_id.clone(),
        backend: event_loop.backend,
        workdir: event_loop.workdir.clone(),
        runtime: runtime.status(),
    }
}

async fn next_run_id(event_loop: &WorkerEventLoop) -> String {
    let mut next = event_loop.next_run_id.lock().await;
    *next += 1;
    format!("run-{}", *next)
}

async fn resolve_manager_session_id(
    event_loop: &WorkerEventLoop,
    requested: Option<String>,
) -> String {
    let runtime = event_loop.runtime.lock().await;
    requested
        .or_else(|| {
            runtime
                .conversation
                .as_ref()
                .and_then(|conversation| conversation.manager_session_id.clone())
        })
        .unwrap_or_else(|| "worker-local".to_string())
}

async fn begin_frontend_submit(
    event_loop: &WorkerEventLoop,
    requested_manager_session_id: Option<String>,
    run_id: &str,
) -> (String, Option<String>) {
    let mut runtime = event_loop.runtime.lock().await;
    let manager_session_id = requested_manager_session_id
        .or_else(|| {
            runtime
                .conversation
                .as_ref()
                .and_then(|conversation| conversation.manager_session_id.clone())
        })
        .unwrap_or_else(|| "worker-local".to_string());
    let native_session_id = runtime
        .conversation
        .as_ref()
        .and_then(|conversation| conversation.native_session_id.clone());
    runtime.active_run = Some(WorkerRunState {
        run_id: run_id.to_string(),
        manager_session_id: Some(manager_session_id.clone()),
        started_at: chrono::Utc::now(),
    });
    runtime.cancelled_run_id = None;
    if let Some(conversation) = runtime.conversation.as_mut() {
        conversation.manager_session_id = Some(manager_session_id.clone());
    }
    (manager_session_id, native_session_id)
}

async fn clear_active_run(event_loop: &WorkerEventLoop, run_id: &str) {
    let mut runtime = event_loop.runtime.lock().await;
    if runtime.cancelled_run_id.as_deref() == Some(run_id) {
        runtime.cancelled_run_id = None;
        runtime.active_run = None;
        return;
    }
    if runtime.active_run.as_ref().map(|run| run.run_id.as_str()) == Some(run_id) {
        runtime.active_run = None;
    }
}

async fn frontend_active_run_id(event_loop: &WorkerEventLoop) -> Option<String> {
    let runtime = event_loop.runtime.lock().await;
    runtime.active_run.as_ref().map(|run| run.run_id.clone())
}

async fn record_frontend_stop(event_loop: &WorkerEventLoop, active_run_id: Option<String>) {
    let mut runtime = event_loop.runtime.lock().await;
    runtime.cancelled_run_id = active_run_id;
}

async fn record_frontend_outcome_native_session(
    event_loop: &WorkerEventLoop,
    manager_session_id: &str,
    native_session_id: String,
) {
    let mut runtime = event_loop.runtime.lock().await;
    let conversation = runtime
        .conversation
        .get_or_insert_with(|| WorkerConversationState {
            manager_session_id: Some(manager_session_id.to_string()),
            native_session_id: Some(native_session_id.clone()),
            workdir: event_loop.workdir.clone(),
            generation: 0,
            display_name: None,
        });
    conversation.manager_session_id = Some(manager_session_id.to_string());
    if conversation.native_session_id.as_deref() != Some(native_session_id.as_str()) {
        conversation.native_session_id = Some(native_session_id);
    }
}

async fn emit_fallback_completion(
    event_loop: &WorkerEventLoop,
    reply: &mut RpcReply,
    effective_manager_session_id: &str,
    run_id: &str,
    mut fallback_sequence: u64,
    outcome: SessionOutcome,
) -> Result<()> {
    let native_session_id = outcome.native_session_id.clone();
    if let Some(final_text) = outcome.final_text {
        emit_frontend_event(
            event_loop,
            reply,
            attach_native_session_id(
                SessionEvent::new(
                    effective_manager_session_id.to_string(),
                    frontend_backend_kind(event_loop),
                    fallback_sequence,
                    SessionOrigin::Worker,
                    SessionEventPayload::AssistantTextFinal { text: final_text },
                )
                .with_run_id(run_id.to_string()),
                native_session_id.clone(),
            ),
        )
        .await?;
        fallback_sequence += 1;
    }
    emit_frontend_event(
        event_loop,
        reply,
        attach_native_session_id(
            SessionEvent::new(
                effective_manager_session_id.to_string(),
                frontend_backend_kind(event_loop),
                fallback_sequence,
                SessionOrigin::Worker,
                SessionEventPayload::RunFinished,
            )
            .with_run_id(run_id.to_string()),
            native_session_id,
        ),
    )
    .await
}

async fn stream_submit_output(
    event_loop: Arc<WorkerEventLoop>,
    mut reply: RpcReply,
    effective_manager_session_id: String,
    run_id: String,
    mut events: tokio::sync::mpsc::Receiver<SessionEvent>,
    outcome: tokio::task::JoinHandle<Result<SessionOutcome>>,
) -> Result<()> {
    let mut terminal_completed = false;
    let mut fallback_sequence = 1u64;
    while let Some(event) = events.recv().await {
        fallback_sequence = event.event_index + 1;
        terminal_completed = matches!(
            event.payload,
            SessionEventPayload::RunFinished | SessionEventPayload::RunCancelled
        );
        emit_frontend_event(event_loop.as_ref(), &mut reply, event).await?;
        if terminal_completed {
            break;
        }
    }

    let outcome = match outcome.await {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(err)) => {
            clear_active_run(event_loop.as_ref(), &run_id).await;
            if !terminal_completed {
                emit_frontend_event(
                    event_loop.as_ref(),
                    &mut reply,
                    SessionEvent::new(
                        effective_manager_session_id.clone(),
                        frontend_backend_kind(event_loop.as_ref()),
                        fallback_sequence,
                        SessionOrigin::Worker,
                        SessionEventPayload::Error {
                            message: err.to_string(),
                        },
                    )
                    .with_run_id(run_id.clone()),
                )
                .await?;
                emit_frontend_event(
                    event_loop.as_ref(),
                    &mut reply,
                    SessionEvent::new(
                        effective_manager_session_id,
                        frontend_backend_kind(event_loop.as_ref()),
                        fallback_sequence + 1,
                        SessionOrigin::Worker,
                        SessionEventPayload::RunCancelled,
                    )
                    .with_run_id(run_id),
                )
                .await?;
            }
            return Ok(());
        }
        Err(err) => {
            clear_active_run(event_loop.as_ref(), &run_id).await;
            if !terminal_completed {
                emit_frontend_event(
                    event_loop.as_ref(),
                    &mut reply,
                    SessionEvent::new(
                        effective_manager_session_id.clone(),
                        frontend_backend_kind(event_loop.as_ref()),
                        fallback_sequence,
                        SessionOrigin::Worker,
                        SessionEventPayload::Error {
                            message: format!("terminal prompt join failed: {err}"),
                        },
                    )
                    .with_run_id(run_id.clone()),
                )
                .await?;
                emit_frontend_event(
                    event_loop.as_ref(),
                    &mut reply,
                    SessionEvent::new(
                        effective_manager_session_id,
                        frontend_backend_kind(event_loop.as_ref()),
                        fallback_sequence + 1,
                        SessionOrigin::Worker,
                        SessionEventPayload::RunCancelled,
                    )
                    .with_run_id(run_id),
                )
                .await?;
            }
            return Ok(());
        }
    };

    clear_active_run(event_loop.as_ref(), &run_id).await;
    if let Some(native_session_id) = outcome.native_session_id.clone() {
        record_frontend_outcome_native_session(
            event_loop.as_ref(),
            &effective_manager_session_id,
            native_session_id,
        )
        .await;
    }

    if !terminal_completed {
        emit_fallback_completion(
            event_loop.as_ref(),
            &mut reply,
            &effective_manager_session_id,
            &run_id,
            fallback_sequence,
            outcome,
        )
        .await?;
    }

    Ok(())
}

async fn handle_submit_input(
    event: SubmitInputEvent,
    event_loop: Arc<WorkerEventLoop>,
) -> Result<LoopControl> {
    let SubmitInputEvent {
        reply,
        manager_session_id,
        text,
    } = event;
    let mut reply = reply;
    let run_id = next_run_id(event_loop.as_ref()).await;
    write_frontend_response(
        &mut reply,
        &worker_to_frontend::Data::SubmitAccepted {
            run_id: run_id.clone(),
            manager_session_id: manager_session_id.clone(),
        },
    )
    .await?;

    let (effective_manager_session_id, baseline_native_session_id) =
        begin_frontend_submit(event_loop.as_ref(), manager_session_id, &run_id).await;

    let request = SessionRequest {
        manager_session_id: effective_manager_session_id.clone(),
        native_session_id: baseline_native_session_id.clone(),
        run_id: run_id.clone(),
        prompt: text,
        settings: runtime_settings_clone(event_loop.as_ref()),
        workdir: workdir_path(event_loop.as_ref()),
        worker_id: worker_id_string(event_loop.as_ref()),
    };

    let (events, outcome) = match with_cli_session(event_loop.as_ref(), |session| {
        session.submit_terminal_prompt(request)
    }) {
        Ok(run) => run,
        Err(err) => {
            clear_active_run(event_loop.as_ref(), &run_id).await;
            emit_frontend_event(
                event_loop.as_ref(),
                &mut reply,
                SessionEvent::new(
                    effective_manager_session_id.clone(),
                    frontend_backend_kind(event_loop.as_ref()),
                    1,
                    SessionOrigin::Worker,
                    SessionEventPayload::Error {
                        message: err.to_string(),
                    },
                )
                .with_run_id(run_id.clone()),
            )
            .await?;
            emit_frontend_event(
                event_loop.as_ref(),
                &mut reply,
                SessionEvent::new(
                    effective_manager_session_id,
                    frontend_backend_kind(event_loop.as_ref()),
                    2,
                    SessionOrigin::Worker,
                    SessionEventPayload::RunCancelled,
                )
                .with_run_id(run_id),
            )
            .await?;
            return Ok(LoopControl::Continue);
        }
    };

    tokio::spawn(async move {
        if let Err(err) = stream_submit_output(
            event_loop,
            reply,
            effective_manager_session_id,
            run_id,
            events,
            outcome,
        )
        .await
        {
            warn!(error = %err, "frontend submit stream failed");
        }
    });

    Ok(LoopControl::Continue)
}

async fn reject_submit_while_busy(
    event: SubmitInputEvent,
    event_loop: Arc<WorkerEventLoop>,
) -> Result<LoopControl> {
    let SubmitInputEvent {
        reply,
        manager_session_id,
        ..
    } = event;
    let mut reply = reply;
    let run_id = next_run_id(event_loop.as_ref()).await;
    write_frontend_response(
        &mut reply,
        &worker_to_frontend::Data::SubmitAccepted {
            run_id: run_id.clone(),
            manager_session_id: manager_session_id.clone(),
        },
    )
    .await?;

    let effective_manager_session_id =
        resolve_manager_session_id(event_loop.as_ref(), manager_session_id).await;
    write_frontend_stream_item(
        &mut reply,
        &worker_to_frontend::StreamItem::Event(
            SessionEvent::new(
                effective_manager_session_id.clone(),
                frontend_backend_kind(event_loop.as_ref()),
                1,
                SessionOrigin::Worker,
                SessionEventPayload::Error {
                    message: "worker already has an active frontend-submitted run".to_string(),
                },
            )
            .with_run_id(run_id.clone()),
        ),
    )
    .await?;
    write_frontend_stream_item(
        &mut reply,
        &worker_to_frontend::StreamItem::Event(
            SessionEvent::new(
                effective_manager_session_id,
                frontend_backend_kind(event_loop.as_ref()),
                2,
                SessionOrigin::Worker,
                SessionEventPayload::RunCancelled,
            )
            .with_run_id(run_id),
        ),
    )
    .await?;

    Ok(LoopControl::Continue)
}

fn dispatch_frontend_event(event: FrontendEvent, event_loop: Arc<WorkerEventLoop>) -> EventFuture {
    match event {
        FrontendEvent::GetStatus(event) => event.handle(event_loop),
        FrontendEvent::SubscribeAttached(event) => event.handle(event_loop),
        FrontendEvent::SubmitInput(event) => event.handle(event_loop),
        FrontendEvent::StopRun(event) => event.handle(event_loop),
    }
}

fn dispatch_frontend_event_while_busy(
    event: FrontendEvent,
    event_loop: Arc<WorkerEventLoop>,
) -> EventFuture {
    match event {
        FrontendEvent::GetStatus(event) => event.handle(event_loop),
        FrontendEvent::SubscribeAttached(event) => event.handle(event_loop),
        FrontendEvent::SubmitInput(event) => event.handle_while_busy(event_loop),
        FrontendEvent::StopRun(event) => event.handle(event_loop),
    }
}

impl EventHandler for GetStatusEvent {
    fn handle(self, event_loop: Arc<WorkerEventLoop>) -> EventFuture {
        Box::pin(async move {
            let mut reply = self.reply;
            write_frontend_response(
                &mut reply,
                &worker_to_frontend::Data::Status {
                    status: runtime_status(event_loop.as_ref()).await,
                },
            )
            .await?;
            Ok(LoopControl::Continue)
        })
    }
}

impl EventHandler for SubscribeAttachedEvent {
    fn handle(self, event_loop: Arc<WorkerEventLoop>) -> EventFuture {
        Box::pin(async move {
            GetStatusEvent { reply: self.reply }
                .handle(event_loop)
                .await
        })
    }
}

impl EventHandler for StopRunEvent {
    fn handle(self, event_loop: Arc<WorkerEventLoop>) -> EventFuture {
        Box::pin(async move {
            let mut reply = self.reply;
            let active_run_id = frontend_active_run_id(event_loop.as_ref()).await;
            let stopped = with_cli_session(event_loop.as_ref(), |session| {
                session.interrupt_terminal(active_run_id.is_some())
            })?;
            if stopped {
                record_frontend_stop(event_loop.as_ref(), active_run_id).await;
            }
            write_frontend_response(
                &mut reply,
                &worker_to_frontend::Data::StopAccepted { stopped },
            )
            .await?;
            Ok(LoopControl::Continue)
        })
    }
}

impl EventHandler for SubmitInputEvent {
    fn handle(self, event_loop: Arc<WorkerEventLoop>) -> EventFuture {
        Box::pin(handle_submit_input(self, event_loop))
    }

    fn handle_while_busy(self, event_loop: Arc<WorkerEventLoop>) -> EventFuture {
        Box::pin(reject_submit_while_busy(self, event_loop))
    }
}

impl EventHandler for FrontendEvent {
    fn handle(self, event_loop: Arc<WorkerEventLoop>) -> EventFuture {
        Box::pin(async move {
            let busy = event_loop.runtime.lock().await.active_run.is_some();
            if busy {
                if let FrontendEvent::SubmitInput(_) = self {
                    warn!("rejecting frontend submit while worker already has an active run");
                }
                dispatch_frontend_event_while_busy(self, event_loop).await
            } else {
                dispatch_frontend_event(self, event_loop).await
            }
        })
    }

    fn handle_while_busy(self, event_loop: Arc<WorkerEventLoop>) -> EventFuture {
        dispatch_frontend_event_while_busy(self, event_loop)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::{Duration, Instant};

    use anyhow::{Result, anyhow, bail};
    use cli::CliSession;
    use session::{
        BackendKindConfig, SessionEvent, SessionEventPayload, SessionOrigin, SessionOutcome,
        SessionRequest,
    };
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::net::UnixStream;
    use tokio::sync::{Notify, mpsc};

    use super::*;
    use crate::event::WorkerEvent;
    use crate::event_loop::WorkerEventLoop;

    #[tokio::test(flavor = "multi_thread")]
    async fn get_status_event_reports_seeded_runtime_snapshot() -> Result<()> {
        let harness = TestHarness::new(FakeCliState {
            current_native_session_id: Some("seeded-native".to_string()),
            ..Default::default()
        })?;

        let status = request_status(&harness.event_loop).await?;
        assert_eq!(status.worker_id, "worker-test");
        assert_eq!(status.backend, BackendKindConfig::Claude);
        assert_eq!(status.workdir, harness.workdir);
        assert_eq!(
            status
                .runtime
                .conversation
                .as_ref()
                .and_then(|conversation| conversation.native_session_id.as_deref()),
            Some("seeded-native")
        );
        assert!(!status.runtime.busy);
        assert!(status.runtime.active_run.is_none());

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn subscribe_attached_event_returns_same_snapshot_as_get_status() -> Result<()> {
        let harness = TestHarness::new(FakeCliState {
            current_native_session_id: Some("seeded-native".to_string()),
            ..Default::default()
        })?;

        let status_via_get = request_status(&harness.event_loop).await?;
        let status_via_subscribe = request_status_with(
            |stream| SubscribeAttachedEvent {
                reply: stream.into(),
            },
            &harness.event_loop,
        )
        .await?;

        assert_eq!(status_via_subscribe.worker_id, status_via_get.worker_id);
        assert_eq!(status_via_subscribe.backend, status_via_get.backend);
        assert_eq!(status_via_subscribe.workdir, status_via_get.workdir);
        assert_eq!(
            status_via_subscribe.runtime.busy,
            status_via_get.runtime.busy
        );
        assert_eq!(
            status_via_subscribe
                .runtime
                .conversation
                .as_ref()
                .and_then(|conversation| conversation.native_session_id.as_deref()),
            status_via_get
                .runtime
                .conversation
                .as_ref()
                .and_then(|conversation| conversation.native_session_id.as_deref())
        );

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn submit_input_event_updates_runtime_and_streams_completion() -> Result<()> {
        let harness = TestHarness::new(FakeCliState {
            current_native_session_id: Some("seeded-native".to_string()),
            submit_plans: VecDeque::from([SubmitPlan::immediate(
                vec![
                    SessionEvent::new(
                        "worker-local",
                        BackendKind::Claude,
                        1,
                        SessionOrigin::Worker,
                        SessionEventPayload::BackendSessionIdentified {
                            native_session_id: "new-native".to_string(),
                        },
                    )
                    .with_run_id("run-1"),
                ],
                Ok(SessionOutcome {
                    native_session_id: Some("new-native".to_string()),
                    final_text: Some("assistant final".to_string()),
                }),
            )]),
            ..Default::default()
        })?;

        let lines = run_event_and_collect_lines(
            |stream| SubmitInputEvent {
                reply: stream.into(),
                manager_session_id: None,
                text: "hello from frontend".to_string(),
            },
            &harness.event_loop,
        )
        .await?;
        let (response, events) = parse_submit_output(&lines)?;

        match response {
            worker_to_frontend::Data::SubmitAccepted {
                run_id,
                manager_session_id,
            } => {
                assert_eq!(run_id, "run-1");
                assert_eq!(manager_session_id, None);
            }
            other => bail!("unexpected submit response: {other:?}"),
        }
        assert_eq!(events.len(), 3);
        assert!(matches!(
            &events[0].payload,
            SessionEventPayload::BackendSessionIdentified { native_session_id }
            if native_session_id == "new-native"
        ));
        assert_eq!(events[0].run_id.as_deref(), Some("run-1"));
        assert!(matches!(
            &events[1].payload,
            SessionEventPayload::AssistantTextFinal { text }
            if text == "assistant final"
        ));
        assert_eq!(events[1].run_id.as_deref(), Some("run-1"));
        assert_eq!(events[1].native_session_id.as_deref(), Some("new-native"));
        assert!(matches!(
            events[2].payload,
            SessionEventPayload::RunFinished
        ));
        assert_eq!(events[2].run_id.as_deref(), Some("run-1"));
        assert_eq!(events[2].native_session_id.as_deref(), Some("new-native"));

        let submitted_requests = harness.cli_state.lock().unwrap().submit_requests.clone();
        assert_eq!(submitted_requests.len(), 1);
        let request = &submitted_requests[0];
        assert_eq!(request.run_id, "run-1");
        assert_eq!(request.manager_session_id, "worker-local");
        assert_eq!(request.native_session_id.as_deref(), Some("seeded-native"));
        assert_eq!(request.prompt, "hello from frontend");
        assert_eq!(request.worker_id, "worker-test");
        assert_eq!(request.workdir, harness.workdir);

        let status = request_status(&harness.event_loop).await?;
        assert!(!status.runtime.busy);
        assert!(status.runtime.active_run.is_none());
        assert_eq!(
            status
                .runtime
                .conversation
                .as_ref()
                .and_then(|conversation| conversation.manager_session_id.as_deref()),
            Some("worker-local")
        );
        assert_eq!(
            status
                .runtime
                .conversation
                .as_ref()
                .and_then(|conversation| conversation.native_session_id.as_deref()),
            Some("new-native")
        );

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn busy_submit_via_enum_handler_leaves_existing_run_active() -> Result<()> {
        let release = Arc::new(ReleaseGate::default());
        let harness = TestHarness::new(FakeCliState {
            current_native_session_id: Some("seeded-native".to_string()),
            submit_plans: VecDeque::from([SubmitPlan::gated(
                release.clone(),
                vec![
                    SessionEvent::new(
                        "worker-local",
                        BackendKind::Claude,
                        1,
                        SessionOrigin::Worker,
                        SessionEventPayload::RunFinished,
                    )
                    .with_run_id("run-1")
                    .with_native_session_id("seeded-native"),
                ],
                Ok(SessionOutcome::empty()),
            )]),
            ..Default::default()
        })?;

        let (submit_task, submit_reader) =
            spawn_handler(harness.event_loop.clone(), |stream| SubmitInputEvent {
                reply: stream.into(),
                manager_session_id: None,
                text: "first prompt".to_string(),
            })?;
        let busy_status = wait_for_busy_status(&harness.event_loop).await?;
        assert!(busy_status.runtime.busy);
        assert_eq!(
            busy_status
                .runtime
                .active_run
                .as_ref()
                .map(|run| run.run_id.as_str()),
            Some("run-1")
        );

        let busy_lines = run_event_and_collect_lines(
            |stream| {
                FrontendEvent::SubmitInput(SubmitInputEvent {
                    reply: stream.into(),
                    manager_session_id: None,
                    text: "second prompt".to_string(),
                })
            },
            &harness.event_loop,
        )
        .await?;
        let (response, events) = parse_submit_output(&busy_lines)?;

        match response {
            worker_to_frontend::Data::SubmitAccepted { run_id, .. } => {
                assert_eq!(run_id, "run-2");
            }
            other => bail!("unexpected busy submit response: {other:?}"),
        }
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events[0].payload,
            SessionEventPayload::Error { .. }
        ));
        assert_eq!(events[0].run_id.as_deref(), Some("run-2"));
        assert!(matches!(
            events[1].payload,
            SessionEventPayload::RunCancelled
        ));
        assert_eq!(events[1].run_id.as_deref(), Some("run-2"));

        let during_busy_status = request_status(&harness.event_loop).await?;
        assert!(during_busy_status.runtime.busy);
        assert_eq!(
            during_busy_status
                .runtime
                .active_run
                .as_ref()
                .map(|run| run.run_id.as_str()),
            Some("run-1")
        );
        assert_eq!(harness.cli_state.lock().unwrap().submit_requests.len(), 1);

        release.release();
        let _ = submit_task.await??;
        let first_lines = submit_reader.await??;
        let (_response, first_events) = parse_submit_output(&first_lines)?;
        assert!(
            first_events
                .iter()
                .any(|event| matches!(event.payload, SessionEventPayload::RunFinished))
        );

        let final_status = request_status(&harness.event_loop).await?;
        assert!(!final_status.runtime.busy);
        assert!(final_status.runtime.active_run.is_none());

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stop_run_event_interrupts_active_run_and_clears_busy_state() -> Result<()> {
        let release = Arc::new(ReleaseGate::default());
        let harness = TestHarness::new(FakeCliState {
            current_native_session_id: Some("seeded-native".to_string()),
            interrupt_results: VecDeque::from([true]),
            submit_plans: VecDeque::from([SubmitPlan::gated(
                release.clone(),
                vec![
                    SessionEvent::new(
                        "worker-local",
                        BackendKind::Claude,
                        1,
                        SessionOrigin::Worker,
                        SessionEventPayload::RunCancelled,
                    )
                    .with_run_id("run-1")
                    .with_native_session_id("seeded-native"),
                ],
                Ok(SessionOutcome::empty()),
            )]),
            ..Default::default()
        })?;

        let (submit_task, submit_reader) =
            spawn_handler(harness.event_loop.clone(), |stream| SubmitInputEvent {
                reply: stream.into(),
                manager_session_id: None,
                text: "long running prompt".to_string(),
            })?;
        let busy_status = wait_for_busy_status(&harness.event_loop).await?;
        assert!(busy_status.runtime.busy);

        let stop_lines = run_event_and_collect_lines(
            |stream| StopRunEvent {
                reply: stream.into(),
            },
            &harness.event_loop,
        )
        .await?;
        assert_eq!(stop_lines.len(), 1);
        match serde_json::from_str::<worker_to_frontend::Data>(&stop_lines[0])? {
            worker_to_frontend::Data::StopAccepted { stopped } => assert!(stopped),
            other => bail!("unexpected stop response: {other:?}"),
        }
        assert_eq!(
            harness.cli_state.lock().unwrap().interrupt_calls,
            vec![true]
        );

        let status_after_stop = request_status(&harness.event_loop).await?;
        assert!(status_after_stop.runtime.busy);
        assert_eq!(
            status_after_stop
                .runtime
                .active_run
                .as_ref()
                .map(|run| run.run_id.as_str()),
            Some("run-1")
        );

        release.release();
        let _ = submit_task.await??;
        let submit_lines = submit_reader.await??;
        let (_response, submit_events) = parse_submit_output(&submit_lines)?;
        assert!(
            submit_events
                .iter()
                .any(|event| matches!(event.payload, SessionEventPayload::RunCancelled))
        );

        let final_status = request_status(&harness.event_loop).await?;
        assert!(!final_status.runtime.busy);
        assert!(final_status.runtime.active_run.is_none());

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stop_run_event_keeps_run_busy_until_cancellation_finishes() -> Result<()> {
        let release = Arc::new(ReleaseGate::default());
        let harness = TestHarness::new(FakeCliState {
            current_native_session_id: Some("seeded-native".to_string()),
            interrupt_results: VecDeque::from([true]),
            submit_plans: VecDeque::from([SubmitPlan::gated(
                release.clone(),
                vec![
                    SessionEvent::new(
                        "worker-local",
                        BackendKind::Claude,
                        1,
                        SessionOrigin::Worker,
                        SessionEventPayload::RunCancelled,
                    )
                    .with_run_id("run-1")
                    .with_native_session_id("seeded-native"),
                ],
                Ok(SessionOutcome::empty()),
            )]),
            ..Default::default()
        })?;

        let (submit_task, submit_reader) =
            spawn_handler(harness.event_loop.clone(), |stream| SubmitInputEvent {
                reply: stream.into(),
                manager_session_id: None,
                text: "long running prompt".to_string(),
            })?;
        let busy_status = wait_for_busy_status(&harness.event_loop).await?;
        assert_eq!(
            busy_status
                .runtime
                .active_run
                .as_ref()
                .map(|run| run.run_id.as_str()),
            Some("run-1")
        );

        let _stop_lines = run_event_and_collect_lines(
            |stream| StopRunEvent {
                reply: stream.into(),
            },
            &harness.event_loop,
        )
        .await?;

        let rejected_lines = run_event_and_collect_lines(
            |stream| {
                FrontendEvent::SubmitInput(SubmitInputEvent {
                    reply: stream.into(),
                    manager_session_id: None,
                    text: "follow-up prompt".to_string(),
                })
            },
            &harness.event_loop,
        )
        .await?;
        let (_response, rejected_events) = parse_submit_output(&rejected_lines)?;
        assert!(matches!(
            rejected_events.first().map(|event| &event.payload),
            Some(SessionEventPayload::Error { message })
            if message == "worker already has an active frontend-submitted run"
        ));

        release.release();
        let _ = submit_task.await??;
        let _ = submit_reader.await??;

        let final_status = request_status(&harness.event_loop).await?;
        assert!(!final_status.runtime.busy);
        assert!(final_status.runtime.active_run.is_none());
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn second_stop_after_cancel_acknowledges_without_clearing_run() -> Result<()> {
        let release = Arc::new(ReleaseGate::default());
        let harness = TestHarness::new(FakeCliState {
            current_native_session_id: Some("seeded-native".to_string()),
            interrupt_results: VecDeque::from([true, false]),
            submit_plans: VecDeque::from([SubmitPlan::gated(
                release.clone(),
                vec![
                    SessionEvent::new(
                        "worker-local",
                        BackendKind::Claude,
                        1,
                        SessionOrigin::Worker,
                        SessionEventPayload::RunCancelled,
                    )
                    .with_run_id("run-1")
                    .with_native_session_id("seeded-native"),
                ],
                Ok(SessionOutcome::empty()),
            )]),
            ..Default::default()
        })?;

        let (submit_task, submit_reader) =
            spawn_handler(harness.event_loop.clone(), |stream| SubmitInputEvent {
                reply: stream.into(),
                manager_session_id: None,
                text: "long running prompt".to_string(),
            })?;
        let _busy_status = wait_for_busy_status(&harness.event_loop).await?;

        let first_stop_lines = run_event_and_collect_lines(
            |stream| StopRunEvent {
                reply: stream.into(),
            },
            &harness.event_loop,
        )
        .await?;
        match serde_json::from_str::<worker_to_frontend::Data>(&first_stop_lines[0])? {
            worker_to_frontend::Data::StopAccepted { stopped } => assert!(stopped),
            other => bail!("unexpected first stop response: {other:?}"),
        }

        let second_stop_lines = run_event_and_collect_lines(
            |stream| StopRunEvent {
                reply: stream.into(),
            },
            &harness.event_loop,
        )
        .await?;
        match serde_json::from_str::<worker_to_frontend::Data>(&second_stop_lines[0])? {
            worker_to_frontend::Data::StopAccepted { stopped } => assert!(!stopped),
            other => bail!("unexpected second stop response: {other:?}"),
        }

        let status_after_second_stop = request_status(&harness.event_loop).await?;
        assert!(status_after_second_stop.runtime.busy);
        assert_eq!(
            status_after_second_stop
                .runtime
                .active_run
                .as_ref()
                .map(|run| run.run_id.as_str()),
            Some("run-1")
        );

        release.release();
        let _ = submit_task.await??;
        let _ = submit_reader.await??;

        let final_status = request_status(&harness.event_loop).await?;
        assert!(!final_status.runtime.busy);
        assert!(final_status.runtime.active_run.is_none());
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn followup_submit_succeeds_after_cancel_completes() -> Result<()> {
        let first_release = Arc::new(ReleaseGate::default());
        let harness = TestHarness::new(FakeCliState {
            current_native_session_id: Some("seeded-native".to_string()),
            interrupt_results: VecDeque::from([true, false]),
            submit_plans: VecDeque::from([
                SubmitPlan::gated(
                    first_release.clone(),
                    vec![
                        SessionEvent::new(
                            "worker-local",
                            BackendKind::Claude,
                            1,
                            SessionOrigin::Worker,
                            SessionEventPayload::RunCancelled,
                        )
                        .with_run_id("run-1")
                        .with_native_session_id("seeded-native"),
                    ],
                    Ok(SessionOutcome::empty()),
                ),
                SubmitPlan::immediate(
                    vec![
                        SessionEvent::new(
                            "worker-local",
                            BackendKind::Claude,
                            1,
                            SessionOrigin::Worker,
                            SessionEventPayload::AssistantTextFinal {
                                text: "follow-up ok".to_string(),
                            },
                        )
                        .with_run_id("run-2")
                        .with_native_session_id("seeded-native"),
                        SessionEvent::new(
                            "worker-local",
                            BackendKind::Claude,
                            2,
                            SessionOrigin::Worker,
                            SessionEventPayload::RunFinished,
                        )
                        .with_run_id("run-2")
                        .with_native_session_id("seeded-native"),
                    ],
                    Ok(SessionOutcome {
                        native_session_id: Some("seeded-native".to_string()),
                        final_text: Some("follow-up ok".to_string()),
                    }),
                ),
            ]),
            ..Default::default()
        })?;

        let (submit_task, submit_reader) =
            spawn_handler(harness.event_loop.clone(), |stream| SubmitInputEvent {
                reply: stream.into(),
                manager_session_id: None,
                text: "first prompt".to_string(),
            })?;
        let _busy_status = wait_for_busy_status(&harness.event_loop).await?;

        let _first_stop = run_event_and_collect_lines(
            |stream| StopRunEvent {
                reply: stream.into(),
            },
            &harness.event_loop,
        )
        .await?;
        let _second_stop = run_event_and_collect_lines(
            |stream| StopRunEvent {
                reply: stream.into(),
            },
            &harness.event_loop,
        )
        .await?;

        first_release.release();
        let _ = submit_task.await??;
        let _ = submit_reader.await??;

        let followup_lines = run_event_and_collect_lines(
            |stream| SubmitInputEvent {
                reply: stream.into(),
                manager_session_id: None,
                text: "follow-up prompt".to_string(),
            },
            &harness.event_loop,
        )
        .await?;
        let (_response, followup_events) = parse_submit_output(&followup_lines)?;
        assert!(followup_events.iter().any(|event| matches!(
            &event.payload,
            SessionEventPayload::AssistantTextFinal { text } if text == "follow-up ok"
        )));
        assert!(
            followup_events
                .iter()
                .any(|event| matches!(event.payload, SessionEventPayload::RunFinished))
        );

        Ok(())
    }

    struct TestHarness {
        event_loop: Arc<WorkerEventLoop>,
        cli_state: Arc<StdMutex<FakeCliState>>,
        workdir: std::path::PathBuf,
        _tempdir: tempfile::TempDir,
    }

    impl TestHarness {
        fn new(cli_state: FakeCliState) -> Result<Self> {
            let tempdir = tempfile::tempdir()?;
            let workdir = tempdir.path().join("workdir");
            std::fs::create_dir_all(&workdir)?;
            let cli_state = Arc::new(StdMutex::new(cli_state));
            let (_tx, rx) = mpsc::channel(16);
            let cli_session: Box<dyn CliSession> = Box::new(FakeCliSession::new(cli_state.clone()));
            let event_loop = Arc::new(WorkerEventLoop::new(
                "worker-test".to_string(),
                BackendKindConfig::Claude,
                workdir.clone(),
                RuntimeSettings::default(),
                cli_session,
                rx,
            ));
            Ok(Self {
                event_loop,
                cli_state,
                workdir,
                _tempdir: tempdir,
            })
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn event_loop_run_consumes_events_from_channel() -> Result<()> {
        let tempdir = tempfile::tempdir()?;
        let workdir = tempdir.path().join("workdir");
        std::fs::create_dir_all(&workdir)?;
        let cli_state = Arc::new(StdMutex::new(FakeCliState {
            current_native_session_id: Some("seeded-native".to_string()),
            ..Default::default()
        }));
        let (tx, rx) = mpsc::channel(16);
        let cli_session: Box<dyn CliSession> = Box::new(FakeCliSession::new(cli_state));
        let event_loop = Arc::new(WorkerEventLoop::new(
            "worker-test".to_string(),
            BackendKindConfig::Claude,
            workdir.clone(),
            RuntimeSettings::default(),
            cli_session,
            rx,
        ));
        let run_task = tokio::spawn(event_loop.clone().run());
        let (stream, _peer) = UnixStream::pair()?;

        tx.send(WorkerEvent::refresh_terminal_session()).await?;
        drop(tx);

        run_task.await??;
        let status = request_status(&event_loop).await?;
        assert_eq!(
            status
                .runtime
                .conversation
                .as_ref()
                .and_then(|conversation| conversation.native_session_id.as_deref()),
            Some("seeded-native")
        );
        assert!(status.runtime.active_run.is_none());
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn event_loop_returns_listener_failures() -> Result<()> {
        let tempdir = tempfile::tempdir()?;
        let workdir = tempdir.path().join("workdir");
        std::fs::create_dir_all(&workdir)?;
        let cli_state = Arc::new(StdMutex::new(FakeCliState {
            current_native_session_id: Some("seeded-native".to_string()),
            ..Default::default()
        }));
        let (tx, rx) = mpsc::channel(4);
        let cli_session: Box<dyn CliSession> = Box::new(FakeCliSession::new(cli_state));
        let event_loop = Arc::new(WorkerEventLoop::new(
            "worker-test".to_string(),
            BackendKindConfig::Claude,
            workdir,
            RuntimeSettings::default(),
            cli_session,
            rx,
        ));

        tx.send(crate::event::ListenerFailureEvent::frontend("boom").into())
            .await?;
        drop(tx);

        let err = event_loop
            .run()
            .await
            .expect_err("listener failure should bubble up");
        assert!(err.to_string().contains("frontend listener exited: boom"));
        Ok(())
    }

    #[derive(Default)]
    struct FakeCliState {
        current_native_session_id: Option<String>,
        interrupt_results: std::collections::VecDeque<bool>,
        interrupt_calls: Vec<bool>,
        submit_plans: std::collections::VecDeque<SubmitPlan>,
        submit_requests: Vec<SessionRequest>,
    }

    struct FakeCliSession {
        state: Arc<StdMutex<FakeCliState>>,
    }

    impl FakeCliSession {
        fn new(state: Arc<StdMutex<FakeCliState>>) -> Self {
            Self { state }
        }
    }

    impl CliSession for FakeCliSession {
        fn send_prompt(&mut self, _request: SessionRequest) -> Result<cli::SpawnedRun> {
            anyhow::bail!("headless send_prompt is unused in frontend handler tests")
        }

        fn shutdown(&mut self) -> Result<()> {
            Ok(())
        }

        fn list_sessions(&self) -> Result<Vec<(cli::SessionId, Option<String>)>> {
            Ok(Vec::new())
        }

        fn current_native_session_id(&self) -> Result<Option<String>> {
            Ok(self.state.lock().unwrap().current_native_session_id.clone())
        }

        fn attach_terminal(
            &mut self,
            _settings: &RuntimeSettings,
            _native_session_id: Option<&str>,
        ) -> Result<()> {
            Ok(())
        }

        fn submit_terminal_prompt(&mut self, request: SessionRequest) -> Result<cli::SpawnedRun> {
            let plan = {
                let mut state = self.state.lock().unwrap();
                state.submit_requests.push(request);
                state
                    .submit_plans
                    .pop_front()
                    .ok_or_else(|| anyhow!("missing fake submit plan"))?
            };
            let (tx, rx) = mpsc::channel(16);
            let event_gate = plan.gate.clone();
            let events = plan.events;
            tokio::spawn(async move {
                if let Some(gate) = event_gate {
                    gate.wait().await;
                }
                for event in events {
                    if tx.send(event).await.is_err() {
                        break;
                    }
                }
            });

            let outcome_gate = plan.gate;
            let outcome = plan.outcome;
            let handle = tokio::spawn(async move {
                if let Some(gate) = outcome_gate {
                    gate.wait().await;
                }
                outcome.map_err(anyhow::Error::msg)
            });
            Ok((rx, handle))
        }

        fn interrupt_terminal(&mut self, allow_when_unknown: bool) -> Result<bool> {
            let mut state = self.state.lock().unwrap();
            state.interrupt_calls.push(allow_when_unknown);
            Ok(state.interrupt_results.pop_front().unwrap_or(false))
        }

        fn sync_terminal_size(&mut self) -> Result<()> {
            Ok(())
        }

        fn take_terminal_exit_handle(&mut self) -> Result<tokio::task::JoinHandle<Result<()>>> {
            Ok(tokio::spawn(async {
                std::future::pending::<()>().await;
                #[allow(unreachable_code)]
                Ok(())
            }))
        }

        fn switch_session(&mut self, _session_id: &cli::SessionId) -> Result<()> {
            Ok(())
        }

        fn delete_session(&mut self, _session_id: &cli::SessionId) -> Result<()> {
            Ok(())
        }

        fn get_history(&self, _limit: usize) -> Result<Vec<(cli::MessageRole, String)>> {
            Ok(Vec::new())
        }

        fn effective_runtime_settings(
            &self,
            requested: &RuntimeSettings,
        ) -> Result<RuntimeSettings> {
            Ok(requested.clone())
        }
    }

    struct SubmitPlan {
        gate: Option<Arc<ReleaseGate>>,
        events: Vec<SessionEvent>,
        outcome: std::result::Result<SessionOutcome, String>,
    }

    impl SubmitPlan {
        fn immediate(
            events: Vec<SessionEvent>,
            outcome: std::result::Result<SessionOutcome, String>,
        ) -> Self {
            Self {
                gate: None,
                events,
                outcome,
            }
        }

        fn gated(
            gate: Arc<ReleaseGate>,
            events: Vec<SessionEvent>,
            outcome: std::result::Result<SessionOutcome, String>,
        ) -> Self {
            Self {
                gate: Some(gate),
                events,
                outcome,
            }
        }
    }

    #[derive(Default)]
    struct ReleaseGate {
        released: AtomicBool,
        notify: Notify,
    }

    impl ReleaseGate {
        async fn wait(&self) {
            if self.released.load(Ordering::SeqCst) {
                return;
            }
            loop {
                self.notify.notified().await;
                if self.released.load(Ordering::SeqCst) {
                    return;
                }
            }
        }

        fn release(&self) {
            self.released.store(true, Ordering::SeqCst);
            self.notify.notify_waiters();
        }
    }

    async fn request_status(
        event_loop: &Arc<WorkerEventLoop>,
    ) -> Result<worker_to_frontend::Status> {
        request_status_with(
            |stream| GetStatusEvent {
                reply: stream.into(),
            },
            event_loop,
        )
        .await
    }

    async fn request_status_with<E, F>(
        build: F,
        event_loop: &Arc<WorkerEventLoop>,
    ) -> Result<worker_to_frontend::Status>
    where
        E: EventHandler,
        F: FnOnce(UnixStream) -> E,
    {
        let lines = run_event_and_collect_lines(build, event_loop).await?;
        if lines.len() != 1 {
            anyhow::bail!("expected exactly one status line, got {}", lines.len());
        }
        match serde_json::from_str::<worker_to_frontend::Data>(&lines[0])? {
            worker_to_frontend::Data::Status { status } => Ok(status),
            other => anyhow::bail!("unexpected status response: {other:?}"),
        }
    }

    async fn run_event_and_collect_lines<E, F>(
        build: F,
        event_loop: &Arc<WorkerEventLoop>,
    ) -> Result<Vec<String>>
    where
        E: EventHandler,
        F: FnOnce(UnixStream) -> E,
    {
        let (writer, reader) = UnixStream::pair()?;
        let _ = build(writer).handle(event_loop.clone()).await?;
        read_all_lines(reader).await
    }

    fn spawn_handler<E, F>(
        event_loop: Arc<WorkerEventLoop>,
        build: F,
    ) -> Result<(
        tokio::task::JoinHandle<Result<LoopControl>>,
        tokio::task::JoinHandle<Result<Vec<String>>>,
    )>
    where
        E: EventHandler + Send + 'static,
        F: FnOnce(UnixStream) -> E + Send + 'static,
    {
        let (writer, reader) = UnixStream::pair()?;
        let handler = tokio::spawn(async move { build(writer).handle(event_loop).await });
        let reader = tokio::spawn(async move { read_all_lines(reader).await });
        Ok((handler, reader))
    }

    async fn wait_for_busy_status(
        event_loop: &Arc<WorkerEventLoop>,
    ) -> Result<worker_to_frontend::Status> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let status = request_status(event_loop).await?;
            if status.runtime.busy {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                anyhow::bail!("worker never became busy");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn read_all_lines(stream: UnixStream) -> Result<Vec<String>> {
        let mut reader = BufReader::new(stream);
        let mut lines = Vec::new();
        loop {
            let mut line = String::new();
            let read = reader.read_line(&mut line).await?;
            if read == 0 {
                break;
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            lines.push(trimmed.to_string());
        }
        Ok(lines)
    }

    fn parse_submit_output(
        lines: &[String],
    ) -> Result<(worker_to_frontend::Data, Vec<SessionEvent>)> {
        let Some(first) = lines.first() else {
            anyhow::bail!("missing submit output lines");
        };
        let response = serde_json::from_str::<worker_to_frontend::Data>(first)?;
        let mut events = Vec::new();
        for line in &lines[1..] {
            let item = serde_json::from_str::<worker_to_frontend::StreamItem>(line)?;
            let worker_to_frontend::StreamItem::Event(event) = item;
            events.push(event);
        }
        Ok((response, events))
    }
}
