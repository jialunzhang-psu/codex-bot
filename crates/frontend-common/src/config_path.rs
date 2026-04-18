use std::path::{Path, PathBuf};

pub fn resolve_config_path(explicit: Option<PathBuf>) -> PathBuf {
    resolve_config_path_with(explicit, PathBuf::from("config.toml"), default_config_path())
}

fn default_config_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|home| home.join(".codex-bot").join("config.toml"))
}

fn resolve_config_path_with(
    explicit: Option<PathBuf>,
    local: PathBuf,
    default: Option<PathBuf>,
) -> PathBuf {
    if let Some(path) = explicit {
        return path;
    }
    if local.exists() {
        return local;
    }
    default.unwrap_or(local)
}

#[allow(dead_code)]
fn normalize_path(base_dir: Option<&Path>, path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    if let Some(base) = base_dir {
        return base.join(path);
    }
    path.to_path_buf()
}
