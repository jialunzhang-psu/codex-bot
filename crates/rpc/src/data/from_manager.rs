use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use session::BackendKindConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerView {
    pub pid: u32,
    pub worker_id: String,
    pub backend: BackendKindConfig,
    pub workdir: PathBuf,
    pub bot_index: usize,
    pub bot_username: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotView {
    pub index: usize,
    pub username: String,
    pub busy: bool,
    pub pid: Option<u32>,
    pub worker_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectoryEntryKind {
    Dir,
    File,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirectoryEntry {
    pub name: String,
    pub kind: DirectoryEntryKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirectoryListing {
    pub cwd: PathBuf,
    pub entries: Vec<DirectoryEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchResult {
    pub worker_id: String,
    pub backend: BackendKindConfig,
    pub workdir: PathBuf,
    pub bot_index: usize,
    pub bot_username: String,
    pub tmux_session: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Data {
    WorkerList {
        workers: Vec<WorkerView>,
    },
    BotList {
        bots: Vec<BotView>,
    },
    DirectoryListing {
        listing: DirectoryListing,
    },
    WorkingDirectory {
        cwd: PathBuf,
    },
    WorkerLaunched {
        launch: LaunchResult,
    },
}
