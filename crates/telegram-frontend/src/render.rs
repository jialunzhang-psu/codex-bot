use frontend_common::{ManagerOutput, WorkerOutput};
use rpc::data::{from_manager, worker_to_frontend};
use session::{BackendKindConfig, SessionEvent, SessionEventPayload};

pub fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn code(value: &str) -> String {
    format!("<code>{}</code>", html_escape(value))
}

pub fn bold(value: &str) -> String {
    format!("<b>{}</b>", html_escape(value))
}

pub fn html_bool(value: bool) -> String {
    if value { code("true") } else { code("false") }
}

pub fn render_manager_outputs(outputs: &[ManagerOutput]) -> String {
    outputs
        .iter()
        .map(render_manager_output)
        .filter(|chunk| !chunk.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub fn render_worker_outputs(outputs: &[WorkerOutput]) -> String {
    outputs
        .iter()
        .map(render_worker_output)
        .filter(|chunk| !chunk.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_manager_output(output: &ManagerOutput) -> String {
    match output {
        ManagerOutput::Response { response } => render_manager_response(response),
        ManagerOutput::Text { text } => std::iter::once(bold("manager commands"))
            .chain(text.lines().map(code))
            .collect::<Vec<_>>()
            .join("\n"),
        ManagerOutput::Error { message } => html_escape(message),
    }
}

fn render_worker_output(output: &WorkerOutput) -> String {
    match output {
        WorkerOutput::Response { response } => render_worker_response(response),
        WorkerOutput::StreamItem { item } => render_stream_item(item),
        WorkerOutput::Text { text } => html_escape(text),
        WorkerOutput::Error { message } => html_escape(message),
    }
}

fn render_manager_response(response: &from_manager::Data) -> String {
    match response {
        from_manager::Data::WorkerList { workers } => format_workers(workers),
        from_manager::Data::BotList { bots } => format_bots(bots),
        from_manager::Data::DirectoryListing { listing } => format_directory_listing(listing),
        from_manager::Data::WorkingDirectory { cwd } => {
            format!("{} {}", bold("cwd"), code(&cwd.display().to_string()))
        }
        from_manager::Data::WorkerLaunched { launch } => format!(
            "{} {}\n{} {}\n{} {}\n{} {}\n{} {}\n{} {}",
            bold("worker"),
            code(&launch.worker_id),
            bold("backend"),
            code(backend_label(launch.backend)),
            bold("bot"),
            code(&format!("{} @{}", launch.bot_index, launch.bot_username)),
            bold("tmux"),
            code(&launch.tmux_session),
            bold("workdir"),
            code(&launch.workdir.display().to_string()),
            bold("status"),
            code("launched"),
        ),
    }
}

fn render_worker_response(response: &worker_to_frontend::Data) -> String {
    match response {
        worker_to_frontend::Data::Status { status } => {
            let conversation = status
                .runtime
                .conversation
                .as_ref()
                .and_then(|conversation| conversation.manager_session_id.as_deref())
                .unwrap_or("-");
            let active_run = status
                .runtime
                .active_run
                .as_ref()
                .map(|run| run.run_id.as_str())
                .unwrap_or("-");
            format!(
                "{} {}\n{} {}\n{} {}",
                bold("worker"),
                code(&status.worker_id),
                bold("conversation"),
                code(conversation),
                bold("run"),
                code(active_run),
            )
        }
        worker_to_frontend::Data::StopAccepted { stopped } => {
            format!("{} {}", bold("stopped"), html_bool(*stopped))
        }
        worker_to_frontend::Data::SubmitAccepted {
            run_id,
            manager_session_id,
        } => format!(
            "{} {}\n{} {}",
            bold("run"),
            code(run_id),
            bold("session"),
            code(manager_session_id.as_deref().unwrap_or("-")),
        ),
    }
}

fn render_stream_item(item: &worker_to_frontend::StreamItem) -> String {
    match item {
        worker_to_frontend::StreamItem::Event(event) => render_event_line(event),
    }
}

fn render_event_line(event: &SessionEvent) -> String {
    match &event.payload {
        SessionEventPayload::AssistantTextDelta { text }
        | SessionEventPayload::AssistantTextFinal { text }
        | SessionEventPayload::Status { message: text }
        | SessionEventPayload::Error { message: text } => html_escape(text),
        SessionEventPayload::RunFinished => "[run finished]".to_string(),
        SessionEventPayload::RunCancelled => "[run cancelled]".to_string(),
        _ => String::new(),
    }
}

fn format_workers(workers: &[from_manager::WorkerView]) -> String {
    if workers.is_empty() {
        return "<b>no workers</b>".to_string();
    }
    workers
        .iter()
        .map(|worker| {
            format!(
                "{} {}\n{} {}\n{} {}\n{} {}\n{} {}\n{} {}",
                bold("pid"),
                code(&worker.pid.to_string()),
                bold("worker"),
                code(&worker.worker_id),
                bold("backend"),
                code(backend_label(worker.backend)),
                bold("bot"),
                code(
                    &worker
                        .bot_username
                        .as_ref()
                        .map(|username| format!("{} @{}", worker.bot_index, username))
                        .unwrap_or_else(|| worker.bot_index.to_string())
                ),
                bold("workdir"),
                code(&worker.workdir.display().to_string()),
                bold("status"),
                code("live"),
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn format_bots(bots: &[from_manager::BotView]) -> String {
    if bots.is_empty() {
        return "<b>no bots</b>".to_string();
    }
    bots
        .iter()
        .map(|bot| {
            let status = if bot.busy { "busy" } else { "idle" };
            let owner = bot.worker_id.as_deref().unwrap_or("-");
            let pid = bot
                .pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "-".to_string());
            format!(
                "{} {}\n{} {}\n{} {}\n{} {}",
                bold("bot"),
                code(&format!("{} @{}", bot.index, bot.username)),
                bold("status"),
                code(status),
                bold("worker"),
                code(owner),
                bold("pid"),
                code(&pid),
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn format_directory_listing(listing: &from_manager::DirectoryListing) -> String {
    let mut lines = vec![format!("{} {}", bold("cwd"), code(&listing.cwd.display().to_string()))];
    if listing.entries.is_empty() {
        lines.push("<b>empty</b>".to_string());
    } else {
        lines.extend(listing.entries.iter().map(|entry| match entry.kind {
            from_manager::DirectoryEntryKind::Dir => code(&format!("{}/", entry.name)),
            from_manager::DirectoryEntryKind::File => code(&entry.name),
        }));
    }
    lines.join("\n")
}

fn backend_label(value: BackendKindConfig) -> &'static str {
    match value {
        BackendKindConfig::Codex => "codex",
        BackendKindConfig::Claude => "claude",
    }
}
