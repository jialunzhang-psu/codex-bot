use std::path::PathBuf;

use chrono::{DateTime, Utc};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum BackendKindConfig {
    Codex,
    Claude,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    Codex,
    Claude,
}

impl BackendKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeSettings {
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub mode: String,
}

impl Default for RuntimeSettings {
    fn default() -> Self {
        Self {
            model: None,
            reasoning_effort: None,
            mode: "default".to_string(),
        }
    }
}

impl RuntimeSettings {
    pub fn new(
        model: Option<String>,
        reasoning_effort: Option<String>,
        mode: Option<String>,
    ) -> Self {
        Self {
            model: normalize_optional(model),
            reasoning_effort: normalize_optional(reasoning_effort),
            mode: normalize_mode(mode),
        }
    }

    pub fn merged_with(&self, defaults: &Self) -> Self {
        Self {
            model: self.model.clone().or_else(|| defaults.model.clone()),
            reasoning_effort: self
                .reasoning_effort
                .clone()
                .or_else(|| defaults.reasoning_effort.clone()),
            mode: if self.mode.trim().is_empty() || self.mode == "default" {
                defaults.mode.clone()
            } else {
                self.mode.clone()
            },
        }
    }
}

fn normalize_optional(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn normalize_mode(value: Option<String>) -> String {
    value
        .and_then(|value| {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        })
        .unwrap_or_else(|| "default".to_string())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEvent {
    pub manager_session_id: String,
    pub backend: BackendKind,
    pub event_index: u64,
    pub origin: SessionOrigin,
    pub payload: SessionEventPayload,
    pub created_at: DateTime<Utc>,
    pub run_id: Option<String>,
    pub native_session_id: Option<String>,
}

impl SessionEvent {
    pub fn new(
        manager_session_id: impl Into<String>,
        backend: BackendKind,
        event_index: u64,
        origin: SessionOrigin,
        payload: SessionEventPayload,
    ) -> Self {
        Self {
            manager_session_id: manager_session_id.into(),
            backend,
            event_index,
            origin,
            payload,
            created_at: Utc::now(),
            run_id: None,
            native_session_id: None,
        }
    }

    pub fn with_run_id(mut self, run_id: impl Into<String>) -> Self {
        self.run_id = Some(run_id.into());
        self
    }

    pub fn with_native_session_id(mut self, native_session_id: impl Into<String>) -> Self {
        self.native_session_id = Some(native_session_id.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionOrigin {
    Adapter,
    Daemon,
    Worker,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEventPayload {
    SessionDelegated {
        worker_id: String,
    },
    RunStarted {
        run_id: String,
        native_session_id: Option<String>,
    },
    RunFinished,
    RunCancelled,
    AssistantTextDelta {
        text: String,
    },
    AssistantTextFinal {
        text: String,
    },
    BackendSessionIdentified {
        native_session_id: String,
    },
    Status {
        message: String,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRequest {
    pub manager_session_id: String,
    pub native_session_id: Option<String>,
    pub run_id: String,
    pub prompt: String,
    pub settings: RuntimeSettings,
    pub workdir: PathBuf,
    pub worker_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionOutcome {
    pub native_session_id: Option<String>,
    pub final_text: Option<String>,
}

impl SessionOutcome {
    pub fn empty() -> Self {
        Self {
            native_session_id: None,
            final_text: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerConversationState {
    pub manager_session_id: Option<String>,
    pub native_session_id: Option<String>,
    pub workdir: PathBuf,
    pub generation: u64,
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerRunState {
    pub run_id: String,
    pub manager_session_id: Option<String>,
    pub started_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WorkerRuntimeSnapshot {
    pub conversation: Option<WorkerConversationState>,
    pub active_run: Option<WorkerRunState>,
}
