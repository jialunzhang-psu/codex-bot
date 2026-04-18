use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use pty::{CommandBuilder, PtyOptions, PtySnapshot, PtyTransport};
use tokio::time::sleep;

#[path = "../src/render.rs"]
mod render;

use frontend_common::WorkerRuntime;
use rpc::{RpcClient, RpcRoute};

#[tokio::test(flavor = "multi_thread")]
async fn telegram_worker_command_syncs_prompt_and_reply_into_worker_tui() -> Result<()> {
    let root = tempfile::tempdir()?;
    let workdir = root.path().join("workdir");
    std::fs::create_dir_all(&workdir)?;
    let config_path = root.path().join("config.toml");
    let state_path = root.path().join("state.json");
    std::fs::write(
        &config_path,
        format!(
            "state_path = \"{}\"\n\n[telegram]\nmanager_token = \"123456:manager\"\nworker_tokens = [\"123456:worker0\"]\npoll_timeout_seconds = 30\n\n[manager]\ndefault_backend = \"claude\"\n",
            state_path.display(),
        ),
    )?;

    let worker_config_path = root.path().join("worker-config.toml");
    std::fs::write(
        &worker_config_path,
        format!(
            "log_level = \"info\"\nstate_path = \"{}\"\n\n[rpc]\nsocket_dir = \"./codex-bot-sockets\"\n\n[telegram]\nmanager_token = \"123456:manager\"\nworker_tokens = [\"123456:worker0\"]\nallow_from = [123456789]\ngroup_reply_all = false\nshare_session_in_channel = false\npoll_timeout_seconds = 30\n\n[codex]\nbin = \"codex\"\nmodel = \"gpt-5.4\"\nreasoning_effort = \"high\"\nmode = \"suggest\"\nextra_env = []\n\n[claude]\nwork_dir = \"{}\"\nbin = \"{}\"\nextra_env = []\n\n[manager]\ndefault_backend = \"codex\"\n",
            state_path.display(),
            workdir.display(),
            claude_bin()?.display(),
        ),
    )?;

    let worker_id = "test-worker";
    let frontend_socket = worker_frontend_socket_path(&worker_config_path, worker_id)?;
    let mut worker = spawn_worker_pty(&worker_config_path, &workdir, worker_id)?;

    wait_for_path(&frontend_socket).await?;
    let boot = wait_for_real_frontend_boot(&worker, Duration::from_secs(15)).await?;
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

    let prompt = "Reply with exactly TELEGRAM_WORKER_SYNC_OK";
    let runtime = WorkerRuntime::new(&worker_config_path, worker_id)?;
    let response = runtime.submit_text(prompt.to_string()).await?;
    let rendered = render::render_worker_outputs(&response);

    let prompt_snapshot = wait_for_snapshot_contains(
        &worker,
        prompt,
        Duration::from_secs(10),
        &format!("worker PTY did not show the telegram worker prompt {prompt:?}"),
    )
    .await?;
    assert!(
        snapshot_contains(&prompt_snapshot, prompt),
        "worker PTY did not show the telegram worker prompt\n{}",
        render_snapshot(&prompt_snapshot)
    );

    let reply_snapshot = wait_for_snapshot_contains(
        &worker,
        "TELEGRAM_WORKER_SYNC_OK",
        Duration::from_secs(45),
        "worker PTY did not show the telegram worker assistant reply",
    )
    .await?;
    assert!(
        snapshot_contains(&reply_snapshot, "TELEGRAM_WORKER_SYNC_OK"),
        "worker PTY did not show the telegram worker assistant reply\n{}",
        render_snapshot(&reply_snapshot)
    );

    let rendered_text = html_to_text(&rendered);
    assert!(
        rendered_text.contains("TELEGRAM_WORKER_SYNC_OK"),
        "telegram worker response did not include expected reply\n{rendered}"
    );
    assert!(
        rendered_text.contains("[run finished]"),
        "telegram worker response did not include run completion\n{rendered}"
    );

    worker.shutdown().await;
    Ok(())
}

fn html_to_text(value: &str) -> String {
    value
        .replace("<b>", "")
        .replace("</b>", "")
        .replace("<code>", "")
        .replace("</code>", "")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn spawn_worker_pty(config_path: &Path, workdir: &Path, worker_id: &str) -> Result<PtyTransport> {
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

fn worker_bin() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("CARGO_BIN_EXE_worker") {
        return Ok(PathBuf::from(path));
    }

    let current_exe = std::env::current_exe()?;
    let deps_dir = current_exe
        .parent()
        .ok_or_else(|| anyhow!("test executable has no parent directory"))?;
    let target_debug_dir = deps_dir
        .parent()
        .ok_or_else(|| anyhow!("deps directory has no parent directory"))?;
    let worker = target_debug_dir.join("worker");
    if worker.exists() {
        return Ok(worker);
    }

    Err(anyhow!(
        "failed to locate worker binary from {}",
        current_exe.display()
    ))
}

fn workspace_bin(name: &str) -> Result<PathBuf> {
    let key = format!("CARGO_BIN_EXE_{name}").replace('-', "_");
    if let Some(path) = std::env::var_os(&key) {
        return Ok(PathBuf::from(path));
    }

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
    timeout: Duration,
) -> Result<PtySnapshot> {
    worker
        .wait_for_snapshot(
            timeout,
            |snapshot| {
                !snapshot.screen.trim().is_empty()
                    && !contains_any(snapshot, &["Current worker idle.", " worker "])
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
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let snapshot = worker.snapshot()?;
        if snapshot_contains(&snapshot, needle) {
            return Ok(snapshot);
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "{message}\nexpected needle: {needle:?}\n{}",
                render_snapshot(&snapshot)
            );
        }
        sleep(Duration::from_millis(200)).await;
    }
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
