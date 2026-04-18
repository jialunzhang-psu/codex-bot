use crate::frontend::event::FrontendEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenerKind {
    Frontend,
}

#[derive(Debug)]
pub struct ListenerFailureEvent {
    pub source: ListenerKind,
    pub error: String,
}

#[derive(Debug)]
pub struct TerminalExitedEvent {
    pub error: Option<String>,
}

#[derive(Debug)]
pub enum WorkerEvent {
    RefreshTerminalSession,
    TerminalExited(TerminalExitedEvent),
    Frontend(FrontendEvent),
    ListenerFailed(ListenerFailureEvent),
}

impl TerminalExitedEvent {
    pub fn success() -> Self {
        Self { error: None }
    }

    pub fn failure(error: impl Into<String>) -> Self {
        Self {
            error: Some(error.into()),
        }
    }
}

impl From<TerminalExitedEvent> for WorkerEvent {
    fn from(value: TerminalExitedEvent) -> Self {
        Self::TerminalExited(value)
    }
}

impl WorkerEvent {
    pub fn refresh_terminal_session() -> Self {
        Self::RefreshTerminalSession
    }
}

impl From<FrontendEvent> for WorkerEvent {
    fn from(value: FrontendEvent) -> Self {
        Self::Frontend(value)
    }
}

impl ListenerFailureEvent {
    pub fn frontend(error: impl Into<String>) -> Self {
        Self {
            source: ListenerKind::Frontend,
            error: error.into(),
        }
    }
}

impl From<ListenerFailureEvent> for WorkerEvent {
    fn from(value: ListenerFailureEvent) -> Self {
        Self::ListenerFailed(value)
    }
}

impl ListenerKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Frontend => "frontend",
        }
    }
}
