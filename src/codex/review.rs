use std::fs;
use std::path::{Path, PathBuf};
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
    Prompt(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewRequest {
    pub repo_path: String,
    pub target: ReviewTarget,
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
            bail!(review_usage());
        }

        let repo_path = args[0].clone();
        match args.get(1).map(String::as_str) {
            None => Ok(Self {
                repo_path,
                target: ReviewTarget::Uncommitted,
            }),
            Some("uncommitted") => {
                if args.len() > 2 {
                    bail!(
                        "{}\n\nCodex CLI does not allow extra review notes together with --uncommitted.",
                        review_usage()
                    );
                }
                Ok(Self {
                    repo_path,
                    target: ReviewTarget::Uncommitted,
                })
            }
            Some("base") => {
                let Some(branch) = args.get(2).cloned() else {
                    bail!(review_usage());
                };
                if args.len() > 3 {
                    bail!(
                        "{}\n\nCodex CLI does not allow extra review notes together with --base.",
                        review_usage()
                    );
                }
                Ok(Self {
                    repo_path,
                    target: ReviewTarget::Base(branch),
                })
            }
            Some("commit") => {
                let Some(sha) = args.get(2).cloned() else {
                    bail!(review_usage());
                };
                if args.len() > 3 {
                    bail!(
                        "{}\n\nCodex CLI does not allow extra review notes together with --commit.",
                        review_usage()
                    );
                }
                Ok(Self {
                    repo_path,
                    target: ReviewTarget::Commit(sha),
                })
            }
            Some(_) => Ok(Self {
                repo_path,
                target: ReviewTarget::Prompt(join_instructions(&args[1..]).unwrap_or_default()),
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
        let repo_dir = resolve_review_repo(&self.work_dir, &request.repo_path)?;
        let args = build_review_args(settings, request);
        let mut command = self.command();
        command
            .args(&args)
            .current_dir(&repo_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let mut child = command.spawn().with_context(|| {
            format!(
                "failed to start codex review process in {}: {} {}",
                repo_dir.display(),
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

fn review_usage() -> &'static str {
    "Usage: /review <repo-path> | /review <repo-path> base <branch> | /review <repo-path> commit <sha> | /review <repo-path> <custom review prompt>"
}

fn resolve_review_repo(work_dir: &Path, repo_path: &str) -> Result<PathBuf> {
    let candidate = Path::new(repo_path);
    let candidate = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        work_dir.join(candidate)
    };
    let candidate = fs::canonicalize(&candidate)
        .with_context(|| format!("failed to resolve review path {}", candidate.display()))?;
    let search_from = if candidate.is_dir() {
        candidate.as_path()
    } else {
        candidate.parent().unwrap_or(candidate.as_path())
    };

    find_git_repo_root(search_from)
        .ok_or_else(|| anyhow!("{} is not inside a git repository", candidate.display()))
}

fn find_git_repo_root(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

fn build_review_args(settings: &RuntimeSettings, request: &ReviewRequest) -> Vec<String> {
    let mut args = vec!["review".to_string()];

    if let Some(model) = normalize_optional(settings.model.clone()) {
        args.push("--model".to_string());
        args.push(model);
    }
    if let Some(effort) = normalize_reasoning(settings.reasoning_effort.clone()) {
        args.push("-c".to_string());
        args.push(format!("model_reasoning_effort={effort:?}"));
    }

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
        ReviewTarget::Prompt(prompt) => args.push(prompt.clone()),
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

    use crate::codex::RuntimeSettings;

    use super::{ReviewRequest, ReviewTarget};

    #[test]
    fn review_request_requires_repo_path() {
        let err = ReviewRequest::from_args(&[]).unwrap_err();
        assert!(err.to_string().contains("/review <repo-path>"));
    }

    #[test]
    fn review_request_defaults_to_uncommitted() -> Result<()> {
        let request = ReviewRequest::from_args(&["auto-drive".to_string()])?;
        assert_eq!(request.repo_path, "auto-drive");
        assert_eq!(request.target, ReviewTarget::Uncommitted);
        Ok(())
    }

    #[test]
    fn review_request_parses_base_target() -> Result<()> {
        let args = vec![
            "auto-drive".to_string(),
            "base".to_string(),
            "main".to_string(),
        ];
        let request = ReviewRequest::from_args(&args)?;
        assert_eq!(request.repo_path, "auto-drive");
        assert_eq!(request.target, ReviewTarget::Base("main".to_string()));
        Ok(())
    }

    #[test]
    fn review_request_rejects_extra_notes_for_base() {
        let args = vec![
            "auto-drive".to_string(),
            "base".to_string(),
            "main".to_string(),
            "bugs".to_string(),
        ];
        let err = ReviewRequest::from_args(&args).unwrap_err();
        assert!(
            err.to_string()
                .contains("does not allow extra review notes")
        );
    }

    #[test]
    fn review_request_treats_remaining_args_as_custom_prompt() -> Result<()> {
        let args = vec![
            "auto-drive".to_string(),
            "focus".to_string(),
            "on".to_string(),
            "bugs".to_string(),
        ];
        let request = ReviewRequest::from_args(&args)?;
        assert_eq!(request.repo_path, "auto-drive");
        assert_eq!(
            request.target,
            ReviewTarget::Prompt("focus on bugs".to_string())
        );
        Ok(())
    }

    #[test]
    fn build_review_args_uses_top_level_review_command() {
        let settings = RuntimeSettings::default();
        let request = ReviewRequest {
            repo_path: "auto-drive".to_string(),
            target: ReviewTarget::Uncommitted,
        };
        let args = super::build_review_args(&settings, &request);
        assert_eq!(
            args,
            vec!["review".to_string(), "--uncommitted".to_string()]
        );
    }
}
