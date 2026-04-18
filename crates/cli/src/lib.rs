use std::io;
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};
use std::time::Duration;

use anyhow::{Result, bail};
use session::{BackendKind, RuntimeSettings, SessionEvent, SessionOutcome, SessionRequest};
use tokio::process::Child;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

mod claude;
mod codex;

pub trait Cli: Send + Sync {
    fn spawn(&self, workdir: &Path) -> Result<Box<dyn CliSession>>;
}

impl Cli for (BackendKind, String, Vec<String>) {
    fn spawn(&self, workdir: &Path) -> Result<Box<dyn CliSession>> {
        match self.0 {
            BackendKind::Claude => claude::spawn(self.1.clone(), self.2.clone(), workdir),
            BackendKind::Codex => codex::spawn(self.1.clone(), self.2.clone(), workdir),
        }
    }
}

pub type SpawnedRun = (
    mpsc::Receiver<SessionEvent>,
    JoinHandle<Result<SessionOutcome>>,
);

pub trait CliSession: Send {
    fn send_prompt(&mut self, request: SessionRequest) -> Result<SpawnedRun>;

    fn shutdown(&mut self) -> Result<()>;

    fn list_sessions(&self) -> Result<Vec<(SessionId, Option<String>)>>;

    fn current_native_session_id(&self) -> Result<Option<String>>;

    fn attach_terminal(
        &mut self,
        settings: &RuntimeSettings,
        native_session_id: Option<&str>,
    ) -> Result<()> {
        let _ = (settings, native_session_id);
        bail!("interactive terminal mode is not implemented for this backend")
    }

    fn submit_terminal_prompt(&mut self, request: SessionRequest) -> Result<SpawnedRun> {
        let _ = request;
        bail!("interactive terminal prompt submission is not implemented for this backend")
    }

    fn interrupt_terminal(&mut self, allow_when_unknown: bool) -> Result<bool> {
        let _ = allow_when_unknown;
        bail!("interactive terminal interrupt is not implemented for this backend")
    }

    fn sync_terminal_size(&mut self) -> Result<()> {
        bail!("interactive terminal resize is not implemented for this backend")
    }

    fn take_terminal_exit_handle(&mut self) -> Result<JoinHandle<Result<()>>> {
        bail!("interactive terminal exit handling is not implemented for this backend")
    }

    fn cleanup_terminal(&mut self) {}

    fn switch_session(&mut self, session_id: &SessionId) -> Result<()>;

    fn delete_session(&mut self, session_id: &SessionId) -> Result<()>;

    fn get_history(&self, limit: usize) -> Result<Vec<(MessageRole, String)>>;

    fn effective_runtime_settings(&self, requested: &RuntimeSettings) -> Result<RuntimeSettings>;
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct SessionId(String);

impl SessionId {
    fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for SessionId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for SessionId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl From<SessionId> for String {
    fn from(value: SessionId) -> Self {
        value.0
    }
}

impl AsRef<str> for SessionId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl std::borrow::Borrow<str> for SessionId {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    User,
    Assistant,
    System,
}

fn configure_command_process_group(command: &mut tokio::process::Command) {
    #[cfg(unix)]
    {
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
}

fn spawn_process_killer(
    child: Arc<Mutex<Child>>,
    pid: Arc<AtomicU32>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        cancel.cancelled().await;

        let group_pid = pid.load(Ordering::Relaxed);
        if group_pid != 0 {
            let _ = kill_process_group(group_pid, libc::SIGTERM);
            tokio::time::sleep(Duration::from_millis(750)).await;
            let group_pid = pid.load(Ordering::Relaxed);
            if group_pid != 0 {
                let _ = kill_process_group(group_pid, libc::SIGKILL);
            }
        }

        let mut child = child.lock().await;
        let _ = child.start_kill();
    })
}

#[cfg(unix)]
fn kill_process_group(pid: u32, signal: libc::c_int) -> io::Result<()> {
    let result = unsafe { libc::kill(-(pid as i32), signal) };
    if result == 0 {
        return Ok(());
    }

    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(err)
    }
}

#[cfg(not(unix))]
fn kill_process_group(_pid: u32, _signal: libc::c_int) -> io::Result<()> {
    Ok(())
}
