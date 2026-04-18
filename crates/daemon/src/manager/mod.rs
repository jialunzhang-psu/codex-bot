pub mod server;

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use rpc::data::from_manager::{
    BotView, DirectoryEntry, DirectoryEntryKind, DirectoryListing, LaunchResult, WorkerView,
};
use session::BackendKindConfig;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::config::Config;
use crate::telegram::TelegramClient;

#[derive(Debug, Clone, PartialEq, Eq)]
struct DiscoveredWorker {
    pid: u32,
    worker_id: String,
    backend: BackendKindConfig,
    workdir: PathBuf,
    token_index: usize,
    project: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct ParsedWorkerArgs {
    config_path: Option<PathBuf>,
    project: Option<String>,
    worker_id: Option<String>,
    backend: Option<BackendKindConfig>,
    workdir: Option<PathBuf>,
    token_index: Option<usize>,
}

pub struct ManagerService {
    config_path: PathBuf,
    project: Option<String>,
    config: Config,
    binding_cwds: Mutex<HashMap<String, PathBuf>>,
    bot_usernames: Mutex<HashMap<usize, String>>,
}

impl ManagerService {
    pub fn new(config_path: PathBuf, config: Config, project: Option<String>) -> Arc<Self> {
        Arc::new(Self {
            config_path: normalize_runtime_path(&config_path),
            project,
            config,
            binding_cwds: Mutex::new(HashMap::new()),
            bot_usernames: Mutex::new(HashMap::new()),
        })
    }

    pub async fn list_workers(&self) -> Result<Vec<WorkerView>> {
        let mut workers = Vec::new();
        for worker in self.discover_workers()? {
            let bot_username = self.bot_username(worker.token_index).await.ok();
            workers.push(WorkerView {
                pid: worker.pid,
                worker_id: worker.worker_id,
                backend: worker.backend,
                workdir: worker.workdir,
                bot_index: worker.token_index,
                bot_username,
            });
        }
        workers.sort_by(|left, right| left.pid.cmp(&right.pid));
        Ok(workers)
    }

    pub async fn list_bots(&self) -> Result<Vec<BotView>> {
        let workers = self.discover_workers()?;
        let mut bots = Vec::new();
        for index in 0..self.config.telegram.worker_tokens.len() {
            let username = self.bot_username(index).await?;
            let busy_worker = workers.iter().find(|worker| worker.token_index == index);
            bots.push(BotView {
                index,
                username,
                busy: busy_worker.is_some(),
                pid: busy_worker.map(|worker| worker.pid),
                worker_id: busy_worker.map(|worker| worker.worker_id.clone()),
            });
        }
        Ok(bots)
    }

    pub async fn list_directory(
        &self,
        binding_key: &str,
        path: Option<PathBuf>,
    ) -> Result<DirectoryListing> {
        let current = self.binding_cwd(binding_key).await;
        let target = match path {
            Some(path) => resolve_existing_directory(&current, &path)?,
            None => current,
        };
        let mut entries = fs::read_dir(&target)
            .with_context(|| format!("failed to read directory {}", target.display()))?
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let kind = entry
                    .file_type()
                    .ok()
                    .map(|file_type| {
                        if file_type.is_dir() {
                            DirectoryEntryKind::Dir
                        } else {
                            DirectoryEntryKind::File
                        }
                    })?;
                Some(DirectoryEntry {
                    name: entry.file_name().to_string_lossy().to_string(),
                    kind,
                })
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            let left_rank = matches!(left.kind, DirectoryEntryKind::Dir) as u8;
            let right_rank = matches!(right.kind, DirectoryEntryKind::Dir) as u8;
            right_rank
                .cmp(&left_rank)
                .then_with(|| left.name.cmp(&right.name))
        });
        Ok(DirectoryListing {
            cwd: target,
            entries,
        })
    }

    pub async fn change_directory(&self, binding_key: &str, path: &Path) -> Result<PathBuf> {
        let current = self.binding_cwd(binding_key).await;
        let next = resolve_existing_directory(&current, path)?;
        self.binding_cwds
            .lock()
            .await
            .insert(binding_key.to_string(), next.clone());
        Ok(next)
    }

    pub async fn launch_worker(
        &self,
        binding_key: &str,
        backend: BackendKindConfig,
        bot: usize,
        workdir: PathBuf,
    ) -> Result<LaunchResult> {
        if bot >= self.config.telegram.worker_tokens.len() {
            bail!(
                "bot index {} is out of range (configured worker bots: {})",
                bot,
                self.config.telegram.worker_tokens.len()
            );
        }
        let current = self.binding_cwd(binding_key).await;
        let workdir = resolve_existing_directory(&current, &workdir)?;
        let workers = self.discover_workers()?;
        if workers.iter().any(|worker| worker.token_index == bot) {
            bail!("bot {} is already busy", bot);
        }

        let bot_username = self.bot_username(bot).await?;
        let worker_id = Uuid::new_v4().to_string();
        let tmux_session = tmux_session_name(bot, &workdir);
        let worker_binary = sibling_binary_path("worker")?;
        let command = worker_command(
            &worker_binary,
            &self.config_path,
            self.project.as_deref(),
            &worker_id,
            backend,
            &workdir,
            bot,
        );
        let status = StdCommand::new("tmux")
            .arg("new-session")
            .arg("-d")
            .arg("-s")
            .arg(&tmux_session)
            .arg("-c")
            .arg(&workdir)
            .arg(&command)
            .status()
            .context("failed to start tmux")?;
        if !status.success() {
            bail!("tmux failed to launch worker session {}", tmux_session);
        }

        Ok(LaunchResult {
            worker_id,
            backend,
            workdir,
            bot_index: bot,
            bot_username,
            tmux_session,
        })
    }

    async fn binding_cwd(&self, binding_key: &str) -> PathBuf {
        let mut map = self.binding_cwds.lock().await;
        map.entry(binding_key.to_string())
            .or_insert_with(|| default_binding_cwd(&self.config_path))
            .clone()
    }

    async fn bot_username(&self, index: usize) -> Result<String> {
        if let Some(username) = self.bot_usernames.lock().await.get(&index).cloned() {
            return Ok(username);
        }
        let token = self
            .config
            .telegram
            .worker_tokens
            .get(index)
            .cloned()
            .ok_or_else(|| anyhow!("worker bot token {} is not configured", index))?;
        let username = TelegramClient::new(token)?.get_me().await?.username;
        self.bot_usernames
            .lock()
            .await
            .insert(index, username.clone());
        Ok(username)
    }

    fn discover_workers(&self) -> Result<Vec<DiscoveredWorker>> {
        let mut workers = Vec::new();
        let proc_dir = Path::new("/proc");
        if !proc_dir.exists() {
            return Ok(workers);
        }
        for entry in fs::read_dir(proc_dir).context("failed to read /proc for worker discovery")? {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            let Some(pid_str) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let Ok(pid) = pid_str.parse::<u32>() else {
                continue;
            };
            let cmdline_path = entry.path().join("cmdline");
            let cwd_path = entry.path().join("cwd");
            let Ok(cmdline) = fs::read(&cmdline_path) else {
                continue;
            };
            if cmdline.is_empty() {
                continue;
            }
            let argv = cmdline
                .split(|byte| *byte == 0)
                .filter(|part| !part.is_empty())
                .filter_map(|part| String::from_utf8(part.to_vec()).ok())
                .collect::<Vec<_>>();
            let process_cwd = fs::read_link(&cwd_path).ok();
            if let Some(worker) = parse_worker_process(
                pid,
                &argv,
                process_cwd.as_deref(),
                &self.config_path,
                self.project.as_deref(),
            )? {
                workers.push(worker);
            }
        }
        workers.sort_by(|left, right| left.pid.cmp(&right.pid));
        Ok(workers)
    }
}

fn parse_worker_process(
    pid: u32,
    argv: &[String],
    process_cwd: Option<&Path>,
    config_path: &Path,
    project: Option<&str>,
) -> Result<Option<DiscoveredWorker>> {
    let Some(program) = argv.first() else {
        return Ok(None);
    };
    let Some(file_name) = Path::new(program).file_name().and_then(|name| name.to_str()) else {
        return Ok(None);
    };
    if file_name != "worker" {
        return Ok(None);
    }

    let parsed = parse_worker_args(&argv[1..], process_cwd);
    let Some(parsed_config_path) = parsed.config_path else {
        return Ok(None);
    };
    if parsed_config_path != normalize_runtime_path(config_path) {
        return Ok(None);
    }
    if let Some(project) = project {
        if parsed.project.as_deref() != Some(project) {
            return Ok(None);
        }
    }

    let Some(worker_id) = parsed.worker_id else {
        return Ok(None);
    };
    let Some(backend) = parsed.backend else {
        return Ok(None);
    };
    let Some(workdir) = parsed.workdir else {
        return Ok(None);
    };
    let Some(token_index) = parsed.token_index else {
        return Ok(None);
    };
    Ok(Some(DiscoveredWorker {
        pid,
        worker_id,
        backend,
        workdir,
        token_index,
        project: parsed.project,
    }))
}

fn parse_worker_args(args: &[String], process_cwd: Option<&Path>) -> ParsedWorkerArgs {
    let mut parsed = ParsedWorkerArgs::default();
    let mut index = 0;
    while index < args.len() {
        if let Some(value) = flag_value(args, &mut index, "--config") {
            parsed.config_path = Some(resolve_process_arg_path(process_cwd, &value));
        } else if let Some(value) = flag_value(args, &mut index, "--project") {
            parsed.project = Some(value);
        } else if let Some(value) = flag_value(args, &mut index, "--worker-id") {
            parsed.worker_id = Some(value);
        } else if let Some(value) = flag_value(args, &mut index, "--backend") {
            parsed.backend = parse_backend(&value).ok();
        } else if let Some(value) = flag_value(args, &mut index, "--workdir") {
            parsed.workdir = Some(resolve_process_arg_path(process_cwd, &value));
        } else if let Some(value) = flag_value(args, &mut index, "--token-index") {
            parsed.token_index = value.parse::<usize>().ok();
        }
        index += 1;
    }
    parsed
}

fn flag_value(args: &[String], index: &mut usize, flag: &str) -> Option<String> {
    let arg = args.get(*index)?;
    if let Some(value) = arg.strip_prefix(&format!("{flag}=")) {
        return Some(value.to_string());
    }
    if arg != flag {
        return None;
    }
    *index += 1;
    args.get(*index).cloned()
}

fn parse_backend(value: &str) -> Result<BackendKindConfig> {
    match value {
        "codex" => Ok(BackendKindConfig::Codex),
        "claude" => Ok(BackendKindConfig::Claude),
        other => Err(anyhow!("unknown backend: {other}")),
    }
}

fn resolve_existing_directory(base: &Path, path: &Path) -> Result<PathBuf> {
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    let canonical = candidate
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", candidate.display()))?;
    if !canonical.is_dir() {
        bail!("{} is not a directory", canonical.display());
    }
    Ok(canonical)
}

fn resolve_process_arg_path(base: Option<&Path>, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    let joined = if path.is_absolute() {
        path
    } else if let Some(base) = base {
        base.join(path)
    } else {
        path
    };
    joined.canonicalize().unwrap_or(joined)
}

fn default_binding_cwd(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .map(normalize_runtime_path)
        .unwrap_or_else(|| normalize_runtime_path(Path::new(".")))
}

fn normalize_runtime_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(path)
}

fn tmux_session_name(bot: usize, workdir: &Path) -> String {
    format!("codex-bot-worker-{bot}-{}", workdir_slug(workdir))
}

fn workdir_slug(workdir: &Path) -> String {
    let source = workdir
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("root");
    let mut slug = String::new();
    let mut last_dash = false;
    for ch in source.chars() {
        let keep = ch.is_ascii_alphanumeric();
        let out = if keep { ch.to_ascii_lowercase() } else { '-' };
        if out == '-' {
            if last_dash {
                continue;
            }
            last_dash = true;
        } else {
            last_dash = false;
        }
        slug.push(out);
        if slug.len() >= 32 {
            break;
        }
    }
    let trimmed = slug.trim_matches('-');
    if trimmed.is_empty() {
        "worker".to_string()
    } else {
        trimmed.to_string()
    }
}

fn worker_command(
    worker_binary: &Path,
    config_path: &Path,
    project: Option<&str>,
    worker_id: &str,
    backend: BackendKindConfig,
    workdir: &Path,
    bot: usize,
) -> String {
    let mut argv = vec![
        worker_binary.as_os_str().to_os_string(),
        OsString::from("--config"),
        config_path.as_os_str().to_os_string(),
    ];
    if let Some(project) = project {
        argv.push(OsString::from("--project"));
        argv.push(OsString::from(project));
    }
    argv.push(OsString::from("--worker-id"));
    argv.push(OsString::from(worker_id));
    argv.push(OsString::from("--backend"));
    argv.push(OsString::from(match backend {
        BackendKindConfig::Codex => "codex",
        BackendKindConfig::Claude => "claude",
    }));
    argv.push(OsString::from("--workdir"));
    argv.push(workdir.as_os_str().to_os_string());
    argv.push(OsString::from("--token-index"));
    argv.push(OsString::from(bot.to_string()));
    format!(
        "exec {}",
        argv.into_iter()
            .map(shell_quote)
            .collect::<Vec<_>>()
            .join(" ")
    )
}

fn shell_quote(value: OsString) -> String {
    let text = value.to_string_lossy();
    format!("'{}'", text.replace('"', "\\\"").replace('\'', "'\"'\"'"))
}

fn sibling_binary_path(name: &str) -> Result<PathBuf> {
    let exe = std::env::current_exe().context("failed to resolve daemon executable path")?;
    let Some(dir) = exe.parent() else {
        bail!("failed to resolve daemon executable directory");
    };
    #[cfg(windows)]
    let candidate = dir.join(format!("{name}.exe"));
    #[cfg(not(windows))]
    let candidate = dir.join(name);
    if !candidate.exists() {
        bail!(
            "required binary not found next to daemon executable: {}",
            candidate.display()
        );
    }
    Ok(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn test_config() -> Config {
        Config {
            telegram: TelegramConfig {
                worker_tokens: vec!["worker-token".to_string()],
            },
            log_level: "info".to_string(),
        }
    }

    #[test]
    fn parse_worker_args_resolves_expected_flags() {
        let tempdir = tempdir().unwrap();
        let config_path = tempdir.path().join("config.toml");
        let workdir = tempdir.path().join("repo");
        std::fs::create_dir_all(&workdir).unwrap();
        std::fs::write(&config_path, "log_level = \"info\"\n[telegram]\nmanager_token = \"m\"\nworker_tokens = [\"w\"]\n[codex]\nextra_env = []\n").unwrap();
        let parsed = parse_worker_args(
            &[
                "--config".to_string(),
                config_path.display().to_string(),
                "--project".to_string(),
                "demo".to_string(),
                "--worker-id".to_string(),
                "worker-1".to_string(),
                "--backend".to_string(),
                "claude".to_string(),
                "--workdir".to_string(),
                workdir.display().to_string(),
                "--token-index".to_string(),
                "0".to_string(),
            ],
            Some(tempdir.path()),
        );
        assert_eq!(parsed.project.as_deref(), Some("demo"));
        assert_eq!(parsed.worker_id.as_deref(), Some("worker-1"));
        assert_eq!(parsed.backend, Some(BackendKindConfig::Claude));
        assert_eq!(parsed.token_index, Some(0));
        assert_eq!(parsed.config_path.as_deref(), Some(config_path.as_path()));
        assert_eq!(parsed.workdir.as_deref(), Some(workdir.as_path()));
    }

    #[test]
    fn tmux_session_name_slugifies_workdir() {
        let session = tmux_session_name(3, Path::new("/tmp/My Project"));
        assert_eq!(session, "codex-bot-worker-3-my-project");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn change_directory_is_scoped_per_binding_key() {
        let tempdir = tempdir().unwrap();
        let config_path = tempdir.path().join("config.toml");
        let alpha = tempdir.path().join("alpha");
        let beta = tempdir.path().join("beta");
        std::fs::create_dir_all(&alpha).unwrap();
        std::fs::create_dir_all(&beta).unwrap();
        let service = ManagerService::new(config_path.clone(), test_config(), None);

        let alpha_cwd = service.change_directory("binding-a", &alpha).await.unwrap();
        assert_eq!(alpha_cwd, alpha.canonicalize().unwrap());
        let listed = service.list_directory("binding-a", None).await.unwrap();
        assert_eq!(listed.cwd, alpha.canonicalize().unwrap());

        let default_cwd = service.list_directory("binding-b", None).await.unwrap();
        assert_eq!(default_cwd.cwd, default_binding_cwd(&config_path));
        let beta_cwd = service.change_directory("binding-b", &beta).await.unwrap();
        assert_eq!(beta_cwd, beta.canonicalize().unwrap());
        let alpha_again = service.list_directory("binding-a", None).await.unwrap();
        assert_eq!(alpha_again.cwd, alpha.canonicalize().unwrap());
    }
}
