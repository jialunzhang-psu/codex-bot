use crate::CommandSpec;

pub enum WorkerCommand {
    Help,
    Status,
    Stop,
    Submit { text: String },
}

pub fn worker_commands() -> Vec<CommandSpec> {
    vec![
        CommandSpec::new("status", "Show worker status", "/status"),
        CommandSpec::new("stop", "Stop current run", "/stop"),
    ]
}

pub fn help_text() -> String {
    worker_commands()
        .into_iter()
        .map(|command| command.usage)
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn parse_worker_command(text: &str) -> WorkerCommand {
    let trimmed = text.trim();
    if trimmed.is_empty() || matches!(trimmed, "/help" | "/start") {
        return WorkerCommand::Help;
    }
    match trimmed.split_whitespace().next().unwrap_or_default() {
        "/status" => WorkerCommand::Status,
        "/stop" => WorkerCommand::Stop,
        _ => WorkerCommand::Submit {
            text: trimmed.to_string(),
        },
    }
}
