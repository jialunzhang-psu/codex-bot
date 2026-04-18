use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcRoute {
    ToManager,
    ToFrontendOfWorker { worker_id: String },
    ToTestingFrontendOfManager,
    ToTestingFrontendOfWorker { worker_id: String },
}

#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    rpc: RpcSection,
}

#[derive(Debug, Default, Deserialize)]
struct RpcSection {
    #[serde(default)]
    socket_dir: Option<PathBuf>,
}

pub(crate) fn resolve_socket_path(config_path: &Path, route: &RpcRoute) -> Result<PathBuf> {
    Ok(load_socket_dir(config_path)?.join(route_socket_file_name(route)))
}

fn load_socket_dir(config_path: &Path) -> Result<PathBuf> {
    let raw = fs::read_to_string(config_path)
        .with_context(|| format!("failed to read config file {}", config_path.display()))?;
    let config: ConfigFile = toml::from_str(&raw)
        .with_context(|| format!("invalid config TOML in {}", config_path.display()))?;
    let base_dir = config_path.parent().unwrap_or_else(|| Path::new("."));
    Ok(resolve_path(
        base_dir,
        config
            .rpc
            .socket_dir
            .unwrap_or_else(|| PathBuf::from("codex-bot-sockets")),
    ))
}

fn route_socket_file_name(route: &RpcRoute) -> String {
    match route {
        RpcRoute::ToManager => "manager.sock".to_string(),
        RpcRoute::ToFrontendOfWorker { worker_id } => {
            format!(
                "worker-frontend-{}.sock",
                worker_digest("frontend", worker_id)
            )
        }
        RpcRoute::ToTestingFrontendOfManager => "testing-frontend-manager.sock".to_string(),
        RpcRoute::ToTestingFrontendOfWorker { worker_id } => format!(
            "testing-frontend-worker-{}.sock",
            worker_digest("testing-frontend", worker_id)
        ),
    }
}

fn worker_digest(kind: &str, worker_id: &str) -> String {
    let mut hasher = DefaultHasher::new();
    kind.hash(&mut hasher);
    worker_id.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn resolve_path(base_dir: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        base_dir.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_routes_use_distinct_socket_names() {
        let frontend = route_socket_file_name(&RpcRoute::ToFrontendOfWorker {
            worker_id: "worker:a".to_string(),
        });
        let testing = route_socket_file_name(&RpcRoute::ToTestingFrontendOfWorker {
            worker_id: "worker:a".to_string(),
        });
        assert_ne!(frontend, testing);
    }
}
