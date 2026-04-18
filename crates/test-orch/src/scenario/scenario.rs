use std::time::Duration;

use anyhow::{Result, anyhow};

use crate::scenario::command::{CommandStep, FrontendTarget, execute_command_step};
use crate::scenario::env::{EnvStep, execute_env_step};
use crate::scenario::expect::Matcher;
use crate::scenario::runtime::ScenarioRuntime;

pub enum Step {
    Env(EnvStep),
    Command(CommandStep),
}

pub struct Scenario {
    name: String,
    steps: Vec<Step>,
}

impl Scenario {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            steps: Vec::new(),
        }
    }

    pub fn step(mut self, step: Step) -> Self {
        self.steps.push(step);
        self
    }

    pub fn init_env(self) -> Self {
        self.step(Step::Env(EnvStep::InitEnv))
    }

    pub fn start_worker(
        self,
        worker_id: Option<String>,
        backend: impl Into<String>,
        token_index: usize,
    ) -> Self {
        self.step(Step::Env(EnvStep::StartWorker {
            worker_id,
            backend: backend.into(),
            token_index,
        }))
    }

    pub fn attach_worker_claude_cli(self) -> Self {
        self.step(Step::Env(EnvStep::AttachWorkerClaudeCli))
    }

    pub fn frontend_command(self, target: FrontendTarget) -> Self {
        self.step(Step::Command(CommandStep::FrontendCommand(target)))
    }

    pub fn expect_frontend(self, matcher: Matcher) -> Self {
        self.step(Step::Command(CommandStep::ExpectFrontend(matcher)))
    }

    pub fn expect_claude_resume_list(self) -> Self {
        self.step(Step::Command(CommandStep::ExpectClaudeResumeList))
    }

    pub fn claude_resume(self, index: usize) -> Self {
        self.step(Step::Command(CommandStep::ClaudeResume { index }))
    }

    pub fn capture_claude_last_message(self) -> Self {
        self.step(Step::Command(CommandStep::CaptureClaudeLastMessage))
    }

    pub fn expect_claude_last_message(self, matcher: Matcher) -> Self {
        self.step(Step::Command(CommandStep::ExpectClaudeLastMessage(matcher)))
    }

    pub fn expect_claude_view(self, matcher: Matcher, timeout: Duration) -> Self {
        self.step(Step::Command(CommandStep::ExpectClaudeView {
            matcher,
            timeout,
        }))
    }

    pub async fn run(self) -> Result<ScenarioRuntime> {
        ScenarioRunner::default().run(self).await
    }
}

#[derive(Default)]
pub struct ScenarioRunner;

impl ScenarioRunner {
    pub async fn run(&self, scenario: Scenario) -> Result<ScenarioRuntime> {
        let mut runtime = ScenarioRuntime::default();
        let result = self.run_inner(&scenario, &mut runtime).await;
        if result.is_err() {
            runtime.shutdown().await;
        }
        result.map(|_| runtime)
    }

    async fn run_inner(&self, scenario: &Scenario, runtime: &mut ScenarioRuntime) -> Result<()> {
        for step in &scenario.steps {
            self.execute_step(step, runtime)
                .await
                .map_err(|err| anyhow!("scenario {} failed: {err}", scenario.name))?;
        }
        Ok(())
    }

    async fn execute_step(&self, step: &Step, runtime: &mut ScenarioRuntime) -> Result<()> {
        match step {
            Step::Env(step) => execute_env_step(step, runtime).await,
            Step::Command(step) => execute_command_step(step, runtime).await,
        }
    }
}
