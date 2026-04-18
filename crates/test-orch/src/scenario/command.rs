use std::time::Duration;

use anyhow::Result;

use crate::scenario::cli;
use crate::scenario::expect::Matcher;
use crate::scenario::runtime::ScenarioRuntime;

#[derive(Debug, Clone)]
pub enum FrontendTarget {
    Manager {
        chat: String,
        text: String,
    },
    Worker {
        worker_id: Option<String>,
        chat: String,
        text: String,
    },
}

pub enum CommandStep {
    FrontendCommand(FrontendTarget),
    ExpectFrontend(Matcher),
    ExpectClaudeResumeList,
    ClaudeResume { index: usize },
    CaptureClaudeLastMessage,
    ExpectClaudeLastMessage(Matcher),
    ExpectClaudeView { matcher: Matcher, timeout: Duration },
}

pub async fn execute_command_step(step: &CommandStep, runtime: &mut ScenarioRuntime) -> Result<()> {
    match step {
        CommandStep::FrontendCommand(target) => {
            let response = match target {
                FrontendTarget::Manager { chat, text } => serde_json::to_string(
                    &runtime.frontend_mut()?.manager_command(chat, text).await?,
                )?,
                FrontendTarget::Worker {
                    worker_id,
                    chat,
                    text,
                } => {
                    let worker_id = match worker_id {
                        Some(worker_id) => worker_id.clone(),
                        None => runtime.worker_handle()?.worker_id.clone(),
                    };
                    serde_json::to_string(
                        &runtime
                            .frontend_mut()?
                            .worker_command(&worker_id, chat, text)
                            .await?,
                    )?
                }
            };
            runtime.set_last_frontend_response(response);
            Ok(())
        }
        CommandStep::ExpectFrontend(matcher) => {
            matcher.check(runtime.last_frontend_response()?, "frontend response")
        }
        CommandStep::ExpectClaudeResumeList => {
            let _ = cli::expect_resume_list(runtime, Duration::from_secs(15)).await?;
            Ok(())
        }
        CommandStep::ClaudeResume { index } => {
            let _ = cli::resume_session(runtime, *index, Duration::from_secs(15)).await?;
            Ok(())
        }
        CommandStep::CaptureClaudeLastMessage => {
            let _ = cli::capture_last_message(runtime)?;
            Ok(())
        }
        CommandStep::ExpectClaudeLastMessage(matcher) => cli::expect_last_message(runtime, matcher),
        CommandStep::ExpectClaudeView { matcher, timeout } => {
            let _ = cli::expect_view(runtime, matcher, *timeout).await?;
            Ok(())
        }
    }
}
