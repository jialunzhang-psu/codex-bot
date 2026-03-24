use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

use anyhow::{Context, Result, anyhow, bail};
use tokio::io::AsyncReadExt;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::{
    CodexClient, RuntimeSettings, merge_command_output, normalize_optional, normalize_reasoning,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewTarget {
    Uncommitted,
    Base(String),
    Commit(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewRequest {
    pub target: ReviewTarget,
    pub instructions: Option<String>,
}

#[derive(Debug)]
pub struct ReviewOutcome {
    pub final_text: String,
}

pub struct SpawnedReview {
    pub join: JoinHandle<Result<ReviewOutcome>>,
    pub pid: Arc<AtomicU32>,
}

impl ReviewRequest {
    pub fn from_args(args: &[String]) -> Result<Self> {
        if args.is_empty() {
            return Ok(Self {
                target: ReviewTarget::Uncommitted,
                instructions: None,
            });
        }

        match args[0].as_str() {
            "base" => {
                let Some(branch) = args.get(1).cloned() else {
                    bail!(
                        "Usage: /review [notes] | /review base <branch> [notes] | /review commit <sha> [notes]"
                    );
                };
                Ok(Self {
                    target: ReviewTarget::Base(branch),
                    instructions: join_instructions(&args[2..]),
                })
            }
            "commit" => {
                let Some(sha) = args.get(1).cloned() else {
                    bail!(
                        "Usage: /review [notes] | /review base <branch> [notes] | /review commit <sha> [notes]"
                    );
                };
                Ok(Self {
                    target: ReviewTarget::Commit(sha),
                    instructions: join_instructions(&args[2..]),
                })
            }
            "uncommitted" => Ok(Self {
                target: ReviewTarget::Uncommitted,
                instructions: join_instructions(&args[1..]),
            }),
            _ => Ok(Self {
                target: ReviewTarget::Uncommitted,
                instructions: join_instructions(args),
            }),
        }
    }
}

impl CodexClient {
    pub fn spawn_review(
        &self,
        settings: &RuntimeSettings,
        request: &ReviewRequest,
        cancel: CancellationToken,
    ) -> Result<SpawnedReview> {
        let args = build_review_args(settings, request);
        let mut command = self.command();
        command
            .args(&args)
            .current_dir(&self.work_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let mut child = command.spawn().with_context(|| {
            format!(
                "failed to start codex review process: {} {}",
                self.bin,
                args.join(" ")
            )
        })?;

        let pid = Arc::new(AtomicU32::new(child.id().unwrap_or_default()));
        let stdout = child
            .stdout
            .take()
            .context("codex review stdout pipe is unavailable")?;
        let stderr = child
            .stderr
            .take()
            .context("codex review stderr pipe is unavailable")?;
        let child = Arc::new(Mutex::new(child));

        let join = {
            let child = Arc::clone(&child);
            let pid_ref = Arc::clone(&pid);
            tokio::spawn(async move {
                let stdout_task = tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut reader = tokio::io::BufReader::new(stdout);
                    reader
                        .read_to_end(&mut buf)
                        .await
                        .context("failed to read codex review stdout")?;
                    Ok::<Vec<u8>, anyhow::Error>(buf)
                });
                let stderr_task = tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut reader = tokio::io::BufReader::new(stderr);
                    reader
                        .read_to_end(&mut buf)
                        .await
                        .context("failed to read codex review stderr")?;
                    Ok::<Vec<u8>, anyhow::Error>(buf)
                });

                let killer_task = {
                    let child = Arc::clone(&child);
                    let cancel = cancel.clone();
                    tokio::spawn(async move {
                        cancel.cancelled().await;
                        let mut child = child.lock().await;
                        let _ = child.start_kill();
                    })
                };

                let status = {
                    let mut child = child.lock().await;
                    let status = child
                        .wait()
                        .await
                        .context("failed to wait for codex review")?;
                    pid_ref.store(0, Ordering::Relaxed);
                    status
                };

                killer_task.abort();
                let stdout = stdout_task.await.context("stdout task join failed")??;
                let stderr = stderr_task.await.context("stderr task join failed")??;
                let output = merge_command_output(&stdout, &stderr);

                if cancel.is_cancelled() {
                    return Err(anyhow!("review cancelled"));
                }

                if !status.success() {
                    if !output.is_empty() {
                        return Err(anyhow!(output));
                    }
                    return Err(anyhow!("codex review exited with status {status}"));
                }

                Ok(ReviewOutcome { final_text: output })
            })
        };

        Ok(SpawnedReview { join, pid })
    }
}

fn build_review_args(settings: &RuntimeSettings, request: &ReviewRequest) -> Vec<String> {
    let mut args = Vec::new();

    if let Some(model) = normalize_optional(settings.model.clone()) {
        args.push("--model".to_string());
        args.push(model);
    }
    if let Some(effort) = normalize_reasoning(settings.reasoning_effort.clone()) {
        args.push("-c".to_string());
        args.push(format!("model_reasoning_effort={effort:?}"));
    }

    args.push("review".to_string());
    match &request.target {
        ReviewTarget::Uncommitted => args.push("--uncommitted".to_string()),
        ReviewTarget::Base(branch) => {
            args.push("--base".to_string());
            args.push(branch.clone());
        }
        ReviewTarget::Commit(sha) => {
            args.push("--commit".to_string());
            args.push(sha.clone());
        }
    }
    if let Some(instructions) = &request.instructions {
        args.push(instructions.clone());
    }

    args
}

fn join_instructions(args: &[String]) -> Option<String> {
    let text = args.join(" ");
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;

    use super::{ReviewRequest, ReviewTarget};

    #[test]
    fn review_request_defaults_to_uncommitted() -> Result<()> {
        let request = ReviewRequest::from_args(&[])?;
        assert_eq!(request.target, ReviewTarget::Uncommitted);
        assert_eq!(request.instructions, None);
        Ok(())
    }

    #[test]
    fn review_request_parses_base_target_and_notes() -> Result<()> {
        let args = vec![
            "base".to_string(),
            "main".to_string(),
            "focus".to_string(),
            "on".to_string(),
            "regressions".to_string(),
        ];
        let request = ReviewRequest::from_args(&args)?;
        assert_eq!(request.target, ReviewTarget::Base("main".to_string()));
        assert_eq!(
            request.instructions.as_deref(),
            Some("focus on regressions")
        );
        Ok(())
    }

    #[test]
    fn review_request_treats_plain_text_as_uncommitted_instructions() -> Result<()> {
        let args = vec!["bugs".to_string(), "only".to_string()];
        let request = ReviewRequest::from_args(&args)?;
        assert_eq!(request.target, ReviewTarget::Uncommitted);
        assert_eq!(request.instructions.as_deref(), Some("bugs only"));
        Ok(())
    }
}
