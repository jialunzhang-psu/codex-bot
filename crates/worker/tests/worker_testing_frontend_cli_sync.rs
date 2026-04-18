use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use cli::Cli;
use pty::{CommandBuilder, PtyKey, PtyOptions, PtySnapshot, PtyTransport};
use rpc::{
    RpcClient, RpcRoute,
    data::{frontend_to_worker, worker_to_frontend::Status as WorkerStatus},
};
use session::{
    BackendKind, BackendKindConfig, RuntimeSettings, SessionEvent, SessionEventPayload,
    SessionRequest,
};
use tokio::time::sleep;

#[path = "../../telegram-frontend/src/render.rs"]
mod telegram_render;

use frontend_common::{WorkerOutput, WorkerRuntime, backend::worker::WorkerBackend};
use telegram_render::render_worker_outputs as render_telegram_worker_response;

#[tokio::test(flavor = "multi_thread")]
async fn testing_frontend_worker_command_syncs_into_claude_tui() -> Result<()> {
    let root = tempfile::tempdir()?;
    let workdir = root.path().join("workdir");
    std::fs::create_dir_all(&workdir)?;
    let worker_config_path = write_worker_config(root.path(), &workdir)?;

    let seeded_session_id = seed_claude_session(&workdir).await?;

    let worker_id = "test-worker";
    let frontend_socket = worker_frontend_socket_path(&worker_config_path, worker_id)?;
    let testing_frontend_socket = testing_frontend_socket_path(&worker_config_path, worker_id)?;
    let mut worker = spawn_worker_pty(&worker_config_path, &workdir, worker_id)?;

    wait_for_path(&frontend_socket).await?;
    wait_for_path(&testing_frontend_socket).await?;

    let boot = wait_for_real_frontend_boot(&worker, worker_id, Duration::from_secs(15)).await?;
    assert!(
        !contains_any(
            &boot,
            &[
                "Current worker idle.",
                &format!("worker {worker_id} online")
            ]
        ),
        "worker still booted into idle service mode instead of a real frontend\n{}",
        render_snapshot(&boot)
    );

    let status_before = wait_for_worker_status_with_native_session_id(
        &worker_config_path,
        worker_id,
        Duration::from_secs(15),
    )
    .await?;
    assert_worker_status_shape(&status_before, worker_id, &workdir)?;
    assert_eq!(
        worker_native_session_id(&status_before),
        Some(seeded_session_id.as_str())
    );

    let prompt = "Reply with exactly TESTING_FRONTEND_CLI_SYNC_OK";
    let response = send_testing_frontend_message(&worker_config_path, worker_id, prompt)
        .await
        .context("failed to send prompt through worker-owned testing frontend socket")?;

    let prompt_snapshot = wait_for_snapshot_contains(
        &worker,
        prompt,
        Duration::from_secs(10),
        &format!("worker PTY did not show the testing frontend prompt {prompt:?}"),
    )
    .await?;
    assert!(
        snapshot_contains(&prompt_snapshot, prompt),
        "worker PTY did not show the testing frontend prompt\n{}",
        render_snapshot(&prompt_snapshot)
    );

    let reply_snapshot = wait_for_snapshot_contains(
        &worker,
        "TESTING_FRONTEND_CLI_SYNC_OK",
        Duration::from_secs(45),
        "worker PTY did not show the testing frontend assistant reply",
    )
    .await?;
    assert!(
        snapshot_contains(&reply_snapshot, "TESTING_FRONTEND_CLI_SYNC_OK"),
        "worker PTY did not show the testing frontend assistant reply\n{}",
        render_snapshot(&reply_snapshot)
    );

    let events = parse_session_events(&response)?;
    assert!(
        events.iter().any(event_contains_text),
        "testing-frontend worker response did not include assistant output\n{response}"
    );
    assert!(
        events.iter().any(event_is_finished),
        "testing-frontend worker response did not include run completion\n{response}"
    );
    assert!(
        events.iter().any(event_has_run_id),
        "testing-frontend worker response did not include run ids\n{response}"
    );
    assert!(
        events
            .iter()
            .any(|event| event_contains_needle(event, "TESTING_FRONTEND_CLI_SYNC_OK")),
        "testing-frontend worker response did not include expected reply\n{response}"
    );

    let status_after =
        wait_for_worker_idle(&worker_config_path, worker_id, Duration::from_secs(10)).await?;
    assert_worker_status_shape(&status_after, worker_id, &workdir)?;
    assert_eq!(
        worker_native_session_id(&status_after),
        Some(seeded_session_id.as_str())
    );

    worker.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn telegram_stop_cancels_frontend_originated_worker_run() -> Result<()> {
    let root = tempfile::tempdir()?;
    let workdir = root.path().join("workdir");
    std::fs::create_dir_all(&workdir)?;
    let worker_config_path = write_worker_config(root.path(), &workdir)?;

    let seeded_session_id = seed_claude_session(&workdir).await?;

    let worker_id = "test-worker-stop";
    let frontend_socket = worker_frontend_socket_path(&worker_config_path, worker_id)?;
    let testing_frontend_socket = testing_frontend_socket_path(&worker_config_path, worker_id)?;
    let mut worker = spawn_worker_pty(&worker_config_path, &workdir, worker_id)?;

    wait_for_path(&frontend_socket).await?;
    wait_for_path(&testing_frontend_socket).await?;
    wait_for_real_frontend_boot(&worker, worker_id, Duration::from_secs(15)).await?;

    let status_before = wait_for_worker_status_with_native_session_id(
        &worker_config_path,
        worker_id,
        Duration::from_secs(15),
    )
    .await?;
    assert_worker_status_shape(&status_before, worker_id, &workdir)?;
    assert_eq!(
        worker_native_session_id(&status_before),
        Some(seeded_session_id.as_str())
    );

    let prompt =
        "Output the exact token FRONTEND_STOP_TEST on 20000 separate lines and nothing else.";
    let submit = tokio::spawn({
        let config_path = worker_config_path.clone();
        let worker_id = worker_id.to_string();
        let prompt = prompt.to_string();
        async move { send_testing_frontend_message(&config_path, &worker_id, &prompt).await }
    });

    let busy_status =
        wait_for_worker_busy(&worker_config_path, worker_id, Duration::from_secs(15)).await?;
    assert_worker_busy(&busy_status)?;
    let busy_run_id = worker_active_run_id(&busy_status)
        .ok_or_else(|| anyhow!("worker busy status missing active run id"))?
        .to_string();

    let prompt_snapshot = wait_for_snapshot_contains(
        &worker,
        "FRONTEND_STOP_TEST",
        Duration::from_secs(15),
        "worker PTY never showed the frontend-originated prompt before stop",
    )
    .await?;
    assert!(snapshot_contains(&prompt_snapshot, "FRONTEND_STOP_TEST"));

    let runtime = WorkerRuntime::new(WorkerBackend::connect_from_config(
        &worker_config_path,
        worker_id,
    )?);
    let stop_response = runtime.handle_text("/stop").await?;
    let stop_rendered = render_telegram_worker_response(&stop_response);
    match stop_response.first() {
        Some(WorkerOutput::Response {
            response: rpc::data::worker_to_frontend::Data::StopAccepted { stopped },
        }) => assert!(*stopped),
        other => bail!("unexpected stop response: {other:?}"),
    }
    assert!(
        stop_rendered.contains("true"),
        "telegram /stop should confirm stop\n{stop_rendered}"
    );

    let response = tokio::time::timeout(Duration::from_secs(15), submit)
        .await
        .map_err(|_| anyhow!("timed out waiting for cancelled testing-frontend response"))?
        .context("frontend submit task join failed")??;
    let events = parse_session_events(&response)?;
    assert!(
        events.iter().any(|event| {
            event_is_cancelled(event)
                && event_run_id(event) == Some(busy_run_id.as_str())
                && event_native_session_id(event) == Some(seeded_session_id.as_str())
        }),
        "frontend-originated run should report cancellation after telegram /stop\n{response}"
    );
    assert!(
        !events.iter().any(event_is_finished),
        "frontend-originated cancelled run should not report finished\n{response}"
    );

    let status_after =
        wait_for_worker_idle(&worker_config_path, worker_id, Duration::from_secs(15)).await?;
    assert_worker_status_shape(&status_after, worker_id, &workdir)?;
    assert_eq!(
        worker_native_session_id(&status_after),
        Some(seeded_session_id.as_str())
    );

    worker.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn duplicate_telegram_stop_does_not_send_second_ctrl_c() -> Result<()> {
    let root = tempfile::tempdir()?;
    let workdir = root.path().join("workdir");
    std::fs::create_dir_all(&workdir)?;
    let worker_config_path = write_worker_config(root.path(), &workdir)?;

    let seeded_session_id = seed_claude_session(&workdir).await?;

    let worker_id = "test-worker-stop-twice";
    let frontend_socket = worker_frontend_socket_path(&worker_config_path, worker_id)?;
    let testing_frontend_socket = testing_frontend_socket_path(&worker_config_path, worker_id)?;
    let mut worker = spawn_worker_pty(&worker_config_path, &workdir, worker_id)?;

    wait_for_path(&frontend_socket).await?;
    wait_for_path(&testing_frontend_socket).await?;
    wait_for_real_frontend_boot(&worker, worker_id, Duration::from_secs(15)).await?;

    let status_before = wait_for_worker_status_with_native_session_id(
        &worker_config_path,
        worker_id,
        Duration::from_secs(15),
    )
    .await?;
    assert_worker_status_shape(&status_before, worker_id, &workdir)?;
    assert_eq!(
        worker_native_session_id(&status_before),
        Some(seeded_session_id.as_str())
    );

    let prompt =
        "Output the exact token DOUBLE_STOP_TEST on 20000 separate lines and nothing else.";
    let submit = tokio::spawn({
        let config_path = worker_config_path.clone();
        let worker_id = worker_id.to_string();
        let prompt = prompt.to_string();
        async move { send_testing_frontend_message(&config_path, &worker_id, &prompt).await }
    });

    let busy_status =
        wait_for_worker_busy(&worker_config_path, worker_id, Duration::from_secs(15)).await?;
    assert_worker_busy(&busy_status)?;

    let runtime = WorkerRuntime::new(WorkerBackend::connect_from_config(
        &worker_config_path,
        worker_id,
    )?);
    let first_stop_response = runtime.handle_text("/stop").await?;
    let first_stop = render_telegram_worker_response(&first_stop_response);
    match first_stop_response.first() {
        Some(WorkerOutput::Response {
            response: rpc::data::worker_to_frontend::Data::StopAccepted { stopped },
        }) => assert!(*stopped),
        other => bail!("unexpected first stop response: {other:?}"),
    }
    assert!(
        first_stop.contains("true"),
        "first /stop should be accepted\n{first_stop}"
    );

    let second_stop_response = runtime.handle_text("/stop").await?;
    let second_stop = render_telegram_worker_response(&second_stop_response);
    match second_stop_response.first() {
        Some(WorkerOutput::Response {
            response: rpc::data::worker_to_frontend::Data::StopAccepted { stopped },
        }) => assert!(!*stopped),
        other => bail!("unexpected second stop response: {other:?}"),
    }
    assert!(
        second_stop.contains("false"),
        "second /stop should be ignored while Claude is already in exit-confirmation state\n{second_stop}"
    );

    let response = tokio::time::timeout(Duration::from_secs(15), submit)
        .await
        .map_err(|_| anyhow!("timed out waiting for cancelled testing-frontend response"))?
        .context("frontend submit task join failed")??;
    let events = parse_session_events(&response)?;
    assert!(
        events.iter().any(event_is_cancelled),
        "frontend-originated run should be cancelled after first /stop\n{response}"
    );
    assert!(
        !events.iter().any(event_is_finished),
        "cancelled run should not report finished\n{response}"
    );

    let followup = "Reply with exactly DOUBLE_STOP_STILL_ALIVE";
    let followup_response =
        send_testing_frontend_message(&worker_config_path, worker_id, followup).await?;
    let followup_events = parse_session_events(&followup_response)?;
    assert!(
        followup_events
            .iter()
            .any(|event| event_contains_needle(event, "DOUBLE_STOP_STILL_ALIVE")),
        "worker frontend should still be usable after duplicate /stop\n{followup_response}"
    );
    assert!(followup_events.iter().any(event_is_finished));

    let followup_snapshot = wait_for_snapshot_contains(
        &worker,
        "DOUBLE_STOP_STILL_ALIVE",
        Duration::from_secs(20),
        "worker PTY never showed the follow-up reply after duplicate /stop",
    )
    .await?;
    assert!(snapshot_contains(
        &followup_snapshot,
        "DOUBLE_STOP_STILL_ALIVE"
    ));

    let status_after =
        wait_for_worker_idle(&worker_config_path, worker_id, Duration::from_secs(15)).await?;
    assert_worker_status_shape(&status_after, worker_id, &workdir)?;
    assert_eq!(
        worker_native_session_id(&status_after),
        Some(seeded_session_id.as_str())
    );

    worker.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn frontend_submit_clears_pending_cli_compose_buffer_before_injecting() -> Result<()> {
    let root = tempfile::tempdir()?;
    let workdir = root.path().join("workdir");
    std::fs::create_dir_all(&workdir)?;
    let worker_config_path = write_worker_config(root.path(), &workdir)?;

    let seeded_session_id = seed_claude_session(&workdir).await?;

    let worker_id = "test-worker-compose-clear";
    let frontend_socket = worker_frontend_socket_path(&worker_config_path, worker_id)?;
    let testing_frontend_socket = testing_frontend_socket_path(&worker_config_path, worker_id)?;
    let mut worker = spawn_worker_pty(&worker_config_path, &workdir, worker_id)?;

    wait_for_path(&frontend_socket).await?;
    wait_for_path(&testing_frontend_socket).await?;
    wait_for_real_frontend_boot(&worker, worker_id, Duration::from_secs(15)).await?;

    let status_before = wait_for_worker_status_with_native_session_id(
        &worker_config_path,
        worker_id,
        Duration::from_secs(15),
    )
    .await?;
    assert_worker_status_shape(&status_before, worker_id, &workdir)?;
    assert_eq!(
        worker_native_session_id(&status_before),
        Some(seeded_session_id.as_str())
    );

    worker.send_text("STALE_CLI_BUFFER")?;
    let stale_snapshot = wait_for_snapshot_contains(
        &worker,
        "STALE_CLI_BUFFER",
        Duration::from_secs(10),
        "worker PTY never showed the staged CLI compose buffer",
    )
    .await?;
    assert!(snapshot_contains(&stale_snapshot, "STALE_CLI_BUFFER"));

    let frontend_prompt = "Reply with exactly FRONTEND_BUFFER_CLEAR_OK";
    let response =
        send_testing_frontend_message(&worker_config_path, worker_id, frontend_prompt).await?;
    let events = parse_session_events(&response)?;
    assert!(
        events
            .iter()
            .any(|event| event_contains_needle(event, "FRONTEND_BUFFER_CLEAR_OK")),
        "frontend response did not include expected reply\n{response}"
    );
    assert!(events.iter().any(event_is_finished));

    let prompt_snapshot = wait_for_snapshot_contains(
        &worker,
        frontend_prompt,
        Duration::from_secs(15),
        "worker PTY never showed the frontend-submitted prompt",
    )
    .await?;
    assert!(snapshot_contains(&prompt_snapshot, frontend_prompt));
    assert!(
        !snapshot_contains(
            &prompt_snapshot,
            "STALE_CLI_BUFFERReply with exactly FRONTEND_BUFFER_CLEAR_OK"
        ),
        "frontend prompt was appended onto stale CLI compose text instead of clearing it first\n{}",
        render_snapshot(&prompt_snapshot)
    );

    worker.send_key(PtyKey::CtrlC)?;
    sleep(Duration::from_millis(400)).await;

    let post_interrupt_snapshot = worker.snapshot()?;
    assert!(
        !post_interrupt_snapshot.screen.contains("STALE_CLI_BUFFER"),
        "stale CLI compose text returned to the live compose area after ctrl-c; frontend submit should have cleared it first\n{}",
        render_snapshot(&post_interrupt_snapshot)
    );

    let followup = "Reply with exactly AFTER_CLEAR_BUFFER_OK";
    let followup_response =
        send_testing_frontend_message(&worker_config_path, worker_id, followup).await?;
    let followup_events = parse_session_events(&followup_response)?;
    assert!(
        followup_events
            .iter()
            .any(|event| event_contains_needle(event, "AFTER_CLEAR_BUFFER_OK")),
        "follow-up frontend response did not include expected reply\n{followup_response}"
    );
    assert!(followup_events.iter().any(event_is_finished));

    let followup_prompt_snapshot = wait_for_snapshot_contains(
        &worker,
        followup,
        Duration::from_secs(15),
        "worker PTY never showed the follow-up frontend prompt",
    )
    .await?;
    assert!(snapshot_contains(&followup_prompt_snapshot, followup));
    assert!(
        !snapshot_contains(
            &followup_prompt_snapshot,
            "STALE_CLI_BUFFERReply with exactly AFTER_CLEAR_BUFFER_OK"
        ),
        "follow-up frontend prompt still appended onto resurrected stale CLI text\n{}",
        render_snapshot(&followup_prompt_snapshot)
    );

    let status_after =
        wait_for_worker_idle(&worker_config_path, worker_id, Duration::from_secs(15)).await?;
    assert_worker_status_shape(&status_after, worker_id, &workdir)?;
    assert_eq!(
        worker_native_session_id(&status_after),
        Some(seeded_session_id.as_str())
    );

    worker.shutdown().await;
    Ok(())
}

fn write_worker_config(root: &Path, workdir: &Path) -> Result<PathBuf> {
    let state_path = root.join("state.json");
    let worker_config_path = root.join("worker-config.toml");
    let claude_bin = claude_bin()?;
    std::fs::write(
        &worker_config_path,
        format!(
            "log_level = \"info\"\nstate_path = \"{}\"\n\n[rpc]\nsocket_dir = \"./codex-bot-sockets\"\n\n[telegram]\nmanager_token = \"123456:manager\"\nworker_tokens = [\"123456:worker0\"]\nallow_from = [123456789]\ngroup_reply_all = false\nshare_session_in_channel = false\npoll_timeout_seconds = 30\n\n[codex]\nbin = \"codex\"\nmodel = \"gpt-5.4\"\nreasoning_effort = \"high\"\nmode = \"suggest\"\nextra_env = []\n\n[claude]\nwork_dir = \"{}\"\nbin = \"{}\"\nextra_env = []\n\n[manager]\ndefault_backend = \"claude\"\n\n",
            state_path.display(),
            workdir.display(),
            claude_bin.display(),
        ),
    )?;
    Ok(worker_config_path)
}

async fn seed_claude_session(workdir: &Path) -> Result<String> {
    let cli = (
        BackendKind::Claude,
        claude_bin()?.display().to_string(),
        Vec::<String>::new(),
    );
    let mut session = cli.spawn(workdir)?;
    let request = SessionRequest {
        manager_session_id: "worker-seed".to_string(),
        native_session_id: None,
        run_id: "seed-run".to_string(),
        prompt: "Reply with exactly SEEDED_WORKER_SESSION_OK".to_string(),
        settings: RuntimeSettings::default(),
        workdir: workdir.to_path_buf(),
        worker_id: "seed-worker".to_string(),
    };
    let (mut events, outcome) = session.send_prompt(request)?;
    let drain = tokio::spawn(async move { while events.recv().await.is_some() {} });
    let outcome = outcome.await.context("seed claude run join failed")??;
    drain.await.context("seed event drain join failed")?;
    outcome
        .native_session_id
        .ok_or_else(|| anyhow!("seed claude run did not produce native session id"))
}

fn spawn_worker_pty(config_path: &Path, workdir: &Path, worker_id: &str) -> Result<PtyTransport> {
    workspace_bin("testing-frontend")?;
    workspace_bin("telegram-frontend")?;
    let worker_bin = worker_bin()?;
    let mut command = CommandBuilder::new(worker_bin);
    command.arg("--config");
    command.arg(config_path);
    command.arg("--backend");
    command.arg("claude");
    command.arg("--workdir");
    command.arg(workdir);
    command.arg("--token-index");
    command.arg("0");
    command.arg("--worker-id");
    command.arg(worker_id);
    command.arg("--no-daemon");
    PtyTransport::spawn(command, PtyOptions::default())
}

async fn send_testing_frontend_message(
    config_path: &Path,
    worker_id: &str,
    text: &str,
) -> Result<String> {
    let client = RpcClient::new(
        config_path,
        RpcRoute::ToTestingFrontendOfWorker {
            worker_id: worker_id.to_string(),
        },
    )?;
    let response = client.request_line(text).await?;
    if response.trim().is_empty() {
        bail!(
            "testing-frontend returned empty response for worker {}",
            worker_id
        );
    }
    Ok(response)
}

async fn wait_for_worker_status_with_native_session_id(
    config_path: &Path,
    worker_id: &str,
    timeout: Duration,
) -> Result<WorkerStatus> {
    let deadline = Instant::now() + timeout;
    let mut last_error = None;
    loop {
        match request_worker_status(config_path, worker_id).await {
            Ok(status) => {
                if worker_native_session_id(&status).is_some() {
                    return Ok(status);
                }
            }
            Err(err) => last_error = Some(err),
        }
        if Instant::now() >= deadline {
            if let Some(err) = last_error {
                return Err(err).context("timed out waiting for worker status");
            }
            bail!("timed out waiting for worker status to expose native session id");
        }
        sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for_worker_busy(
    config_path: &Path,
    worker_id: &str,
    timeout: Duration,
) -> Result<WorkerStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        let status = request_worker_status(config_path, worker_id).await?;
        if status.runtime.busy {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for worker busy state: {status:?}");
        }
        sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_worker_idle(
    config_path: &Path,
    worker_id: &str,
    timeout: Duration,
) -> Result<WorkerStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        let status = request_worker_status(config_path, worker_id).await?;
        if !status.runtime.busy && status.runtime.active_run.is_none() {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for worker idle state: {status:?}");
        }
        sleep(Duration::from_millis(100)).await;
    }
}

async fn request_worker_status(config_path: &Path, worker_id: &str) -> Result<WorkerStatus> {
    let backend = WorkerBackend::connect_from_config(config_path, worker_id)?;
    match backend.request(frontend_to_worker::Data::GetStatus).await? {
        rpc::data::worker_to_frontend::Data::Status { status } => Ok(status),
        other => bail!("unexpected worker status response: {other:?}"),
    }
}

fn worker_frontend_socket_path(config_path: &Path, worker_id: &str) -> Result<PathBuf> {
    Ok(RpcClient::new(
        config_path,
        RpcRoute::ToFrontendOfWorker {
            worker_id: worker_id.to_string(),
        },
    )?
    .socket_path()
    .to_path_buf())
}

fn testing_frontend_socket_path(config_path: &Path, worker_id: &str) -> Result<PathBuf> {
    Ok(RpcClient::new(
        config_path,
        RpcRoute::ToTestingFrontendOfWorker {
            worker_id: worker_id.to_string(),
        },
    )?
    .socket_path()
    .to_path_buf())
}

fn worker_bin() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("CARGO_BIN_EXE_worker") {
        return Ok(PathBuf::from(path));
    }
    sibling_bin("worker")
}

fn sibling_bin(name: &str) -> Result<PathBuf> {
    let current_exe = std::env::current_exe()?;
    let deps_dir = current_exe
        .parent()
        .ok_or_else(|| anyhow!("test executable has no parent directory"))?;
    let target_debug_dir = deps_dir
        .parent()
        .ok_or_else(|| anyhow!("deps directory has no parent directory"))?;
    let binary = target_debug_dir.join(name);
    if binary.exists() {
        return Ok(binary);
    }
    Err(anyhow!(
        "failed to locate {name} binary from {}",
        current_exe.display()
    ))
}

fn claude_bin() -> Result<PathBuf> {
    std::env::var_os("CLAUDE_BIN")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|home| PathBuf::from(home).join(".local/share/mamba/bin/claude"))
        })
        .filter(|path| path.exists())
        .or_else(|| {
            std::env::var_os("PATH").and_then(|path| {
                std::env::split_paths(&path)
                    .map(|entry| entry.join("claude"))
                    .find(|candidate| candidate.exists())
            })
        })
        .ok_or_else(|| anyhow!("failed to locate claude binary"))
}

fn workspace_bin(name: &str) -> Result<PathBuf> {
    let key = format!("CARGO_BIN_EXE_{name}").replace('-', "_");
    if let Some(path) = std::env::var_os(&key) {
        return Ok(PathBuf::from(path));
    }

    let binary = sibling_bin(name)?;
    if binary.exists() {
        return Ok(binary);
    }

    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow!("failed to resolve workspace root"))?;
    let status = std::process::Command::new("cargo")
        .arg("build")
        .arg("-p")
        .arg(name)
        .current_dir(&workspace_root)
        .status()
        .with_context(|| format!("failed to run cargo build -p {name}"))?;
    if !status.success() {
        bail!("cargo build -p {} failed with status {}", name, status);
    }
    let binary = workspace_root.join("target").join("debug").join(name);
    if binary.exists() {
        return Ok(binary);
    }
    Err(anyhow!(
        "cargo build produced no binary at {}",
        binary.display()
    ))
}

async fn wait_for_path(path: &Path) -> Result<()> {
    for _ in 0..100 {
        if path.exists() {
            return Ok(());
        }
        sleep(Duration::from_millis(100)).await;
    }
    Err(anyhow!("timed out waiting for {}", path.display()))
}

async fn wait_for_real_frontend_boot(
    worker: &PtyTransport,
    worker_id: &str,
    timeout: Duration,
) -> Result<PtySnapshot> {
    worker
        .wait_for_snapshot(
            timeout,
            |snapshot| {
                !snapshot.screen.trim().is_empty()
                    && !contains_any(
                        snapshot,
                        &[
                            "Current worker idle.",
                            &format!("worker {worker_id} online"),
                        ],
                    )
            },
            |snapshot| {
                format!(
                    "worker PTY never entered a real frontend boot state\n{}",
                    render_snapshot(snapshot)
                )
            },
        )
        .await
}

async fn wait_for_snapshot_contains(
    worker: &PtyTransport,
    needle: &str,
    timeout: Duration,
    message: &str,
) -> Result<PtySnapshot> {
    let deadline = Instant::now() + timeout;
    loop {
        let snapshot = worker.snapshot()?;
        if snapshot_contains(&snapshot, needle) {
            return Ok(snapshot);
        }
        if Instant::now() >= deadline {
            bail!(
                "{message}\nexpected needle: {needle:?}\n{}",
                render_snapshot(&snapshot)
            );
        }
        sleep(Duration::from_millis(200)).await;
    }
}

fn parse_session_events(response: &str) -> Result<Vec<SessionEvent>> {
    let outputs: Vec<WorkerOutput> = serde_json::from_str(response)?;
    let events = outputs
        .into_iter()
        .filter_map(|output| match output {
            WorkerOutput::StreamItem {
                item: rpc::data::worker_to_frontend::StreamItem::Event(event),
            } => Some(event),
            _ => None,
        })
        .collect::<Vec<_>>();
    if events.is_empty() {
        bail!("testing-frontend worker response missing stream events: {response}");
    }
    Ok(events)
}

fn assert_worker_status_shape(
    status: &WorkerStatus,
    worker_id: &str,
    workdir: &Path,
) -> Result<()> {
    if status.worker_id != worker_id {
        bail!("unexpected worker_id in status: {status:?}");
    }
    if status.backend != BackendKindConfig::Claude {
        bail!("unexpected backend in status: {status:?}");
    }
    if status.workdir != workdir {
        bail!("unexpected workdir in status: {status:?}");
    }
    if status.runtime.conversation.is_none() {
        bail!("worker status missing runtime conversation: {status:?}");
    }
    Ok(())
}

fn assert_worker_busy(status: &WorkerStatus) -> Result<()> {
    if !status.runtime.busy {
        bail!("worker status should be busy while run is active: {status:?}");
    }
    if status.runtime.active_run.is_none() {
        bail!("worker status should report an active run while busy: {status:?}");
    }
    Ok(())
}

fn worker_native_session_id(status: &WorkerStatus) -> Option<&str> {
    status
        .runtime
        .conversation
        .as_ref()
        .and_then(|conversation| conversation.native_session_id.as_deref())
        .filter(|value| !value.trim().is_empty())
}

fn worker_active_run_id(status: &WorkerStatus) -> Option<&str> {
    status
        .runtime
        .active_run
        .as_ref()
        .map(|run| run.run_id.as_str())
}

fn contains_any(snapshot: &PtySnapshot, needles: &[&str]) -> bool {
    needles
        .iter()
        .any(|needle| snapshot_contains(snapshot, needle))
}

fn snapshot_contains(snapshot: &PtySnapshot, needle: &str) -> bool {
    snapshot.screen.contains(needle) || snapshot.transcript.contains(needle)
}

fn render_snapshot(snapshot: &PtySnapshot) -> String {
    format!(
        "screen:\n{}\n\ntranscript:\n{}\n\ntranscript_debug:\n{}",
        snapshot.screen, snapshot.transcript, snapshot.transcript_debug
    )
}

fn event_contains_text(event: &SessionEvent) -> bool {
    matches!(
        &event.payload,
        SessionEventPayload::AssistantTextDelta { .. }
            | SessionEventPayload::AssistantTextFinal { .. }
    )
}

fn event_is_finished(event: &SessionEvent) -> bool {
    matches!(event.payload, SessionEventPayload::RunFinished)
}

fn event_is_cancelled(event: &SessionEvent) -> bool {
    matches!(event.payload, SessionEventPayload::RunCancelled)
}

fn event_has_run_id(event: &SessionEvent) -> bool {
    event_run_id(event)
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
}

fn event_run_id(event: &SessionEvent) -> Option<&str> {
    event.run_id.as_deref()
}

fn event_native_session_id(event: &SessionEvent) -> Option<&str> {
    event.native_session_id.as_deref()
}

fn event_contains_needle(event: &SessionEvent, needle: &str) -> bool {
    match &event.payload {
        SessionEventPayload::AssistantTextDelta { text }
        | SessionEventPayload::AssistantTextFinal { text }
        | SessionEventPayload::Status { message: text }
        | SessionEventPayload::Error { message: text } => text.contains(needle),
        _ => false,
    }
}
