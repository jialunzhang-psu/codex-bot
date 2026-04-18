use std::path::PathBuf;

use session::BackendKindConfig;

use crate::CommandSpec;

pub enum ManagerCommand {
    Help,
    Workers,
    Bots,
    Ls {
        path: Option<PathBuf>,
    },
    Cd {
        path: PathBuf,
    },
    Launch {
        backend: BackendKindConfig,
        bot: usize,
        workdir: PathBuf,
    },
}

pub fn manager_commands() -> Vec<CommandSpec> {
    vec![
        CommandSpec::new("workers", "List live workers", "/workers"),
        CommandSpec::new("bots", "List available worker bots", "/bots"),
        CommandSpec::new("ls", "List files", "/ls [path]"),
        CommandSpec::new("cd", "Change working directory", "/cd <path>"),
        CommandSpec::new(
            "launch",
            "Launch a worker in tmux",
            "/launch <backend> <bot> <workdir>",
        ),
    ]
}

pub fn help_text() -> String {
    manager_commands()
        .into_iter()
        .map(|command| command.usage)
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn parse_manager_command(text: &str) -> Result<ManagerCommand, String> {
    if matches!(text, "" | "/help" | "/start") {
        return Ok(ManagerCommand::Help);
    }

    let mut parts = text.split_whitespace();
    let command = parts.next().unwrap_or_default();
    match command {
        "/workers" => Ok(ManagerCommand::Workers),
        "/bots" => Ok(ManagerCommand::Bots),
        "/ls" => Ok(ManagerCommand::Ls {
            path: parts.next().map(PathBuf::from),
        }),
        "/cd" => {
            let Some(path) = parts.next() else {
                return Err("/cd <path>".to_string());
            };
            Ok(ManagerCommand::Cd {
                path: PathBuf::from(path),
            })
        }
        "/launch" => {
            let Some(backend) = parts.next() else {
                return Err("/launch <backend> <bot> <workdir>".to_string());
            };
            let Some(bot) = parts.next() else {
                return Err("/launch <backend> <bot> <workdir>".to_string());
            };
            let Some(workdir) = parts.next() else {
                return Err("/launch <backend> <bot> <workdir>".to_string());
            };
            Ok(ManagerCommand::Launch {
                backend: parse_backend(backend)?,
                bot: bot
                    .parse::<usize>()
                    .map_err(|_| format!("invalid bot index: {bot}"))?,
                workdir: PathBuf::from(workdir),
            })
        }
        _ => Ok(ManagerCommand::Help),
    }
}

fn parse_backend(value: &str) -> Result<BackendKindConfig, String> {
    match value {
        "codex" => Ok(BackendKindConfig::Codex),
        "claude" => Ok(BackendKindConfig::Claude),
        other => Err(format!("unknown backend: {other}")),
    }
}
