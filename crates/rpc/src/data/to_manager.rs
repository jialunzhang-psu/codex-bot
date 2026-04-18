use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use session::BackendKindConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Data {
    ListWorkers,
    ListBots,
    ListDirectory {
        binding_key: String,
        path: Option<PathBuf>,
    },
    ChangeDirectory {
        binding_key: String,
        path: PathBuf,
    },
    LaunchWorker {
        binding_key: String,
        backend: BackendKindConfig,
        bot: usize,
        workdir: PathBuf,
    },
}
