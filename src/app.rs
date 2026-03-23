use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Result, anyhow};
use chrono::{DateTime, Local};
use parking_lot::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::codex::{
    CodexClient, RuntimeSettings, SessionSummary, SpawnedTurn, TurnEvent, TurnOutcome, UsageReport,
};
use crate::config::Config;
use crate::state::StateStore;
use crate::telegram::{BotCommand, BotIdentity, Message, SentMessage, TelegramClient, User};

const PAGE_SIZE: usize = 12;
const TELEGRAM_MESSAGE_LIMIT: usize = 3800;

#[derive(Clone)]
struct ActiveRun {
    run_id: u64,
    cancel: CancellationToken,
    started_at: Instant,
    pid: u32,
}

#[derive(Debug, Clone)]
struct MessageContext {
    session_key: String,
    chat_id: i64,
    message_id: i64,
    user_id: i64,
    user_name: String,
    chat_name: Option<String>,
    text: String,
}

#[derive(Debug)]
struct ParsedCommand {
    name: String,
    args: Vec<String>,
}

pub struct BridgeApp {
    config: Config,
    telegram: TelegramClient,
    state: Arc<StateStore>,
    codex: Arc<CodexClient>,
    bot: BotIdentity,
    active_runs: Mutex<HashMap<String, ActiveRun>>,
    next_run_id: AtomicU64,
    startup_unix: i64,
}

impl BridgeApp {
    pub async fn new(
        config: Config,
        telegram: TelegramClient,
        state: Arc<StateStore>,
        codex: Arc<CodexClient>,
    ) -> Result<Self> {
        let bot = telegram.get_me().await?;
        Ok(Self {
            config,
            telegram,
            state,
            codex,
            bot,
            active_runs: Mutex::new(HashMap::new()),
            next_run_id: AtomicU64::new(1),
            startup_unix: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64,
        })
    }

    pub async fn run(&self) -> Result<()> {
        self.telegram.set_my_commands(&menu_commands()).await?;
        info!(
            bot = %self.bot.username,
            work_dir = %self.codex.work_dir().display(),
            "telegram bridge started"
        );

        let mut offset = 0i64;
        loop {
            match self
                .telegram
                .get_updates(offset, self.config.telegram.poll_timeout_seconds)
                .await
            {
                Ok(updates) => {
                    for update in updates {
                        offset = update.update_id + 1;
                        if let Some(message) = update.message {
                            if let Err(err) = self.handle_message(message).await {
                                warn!(error = %err, "failed to handle Telegram message");
                            }
                        }
                    }
                }
                Err(err) => {
                    warn!(error = %err, "telegram poll failed");
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
            }
        }
    }

    async fn handle_message(&self, message: Message) -> Result<()> {
        if message.date < self.startup_unix.saturating_sub(5) {
            return Ok(());
        }

        let Some(from) = &message.from else {
            return Ok(());
        };
        if !self.config.user_allowed(from.id) {
            return Ok(());
        }

        let Some(raw_text) = message.text.as_deref() else {
            return Ok(());
        };
        let text = raw_text.trim();
        if text.is_empty() {
            return Ok(());
        }

        if message.is_group()
            && !self.config.telegram.group_reply_all
            && !self.is_directed_at_bot(&message)
        {
            return Ok(());
        }

        let text = strip_bot_mentions(text, &self.bot.username);
        let context = MessageContext {
            session_key: self.session_key(&message, from.id),
            chat_id: message.chat.id,
            message_id: message.message_id,
            user_id: from.id,
            user_name: display_name(from),
            chat_name: message.chat.title.clone(),
            text,
        };

        if let Some(command) = parse_command(&context.text, &self.bot.username) {
            self.handle_command(&context, command).await?;
        } else {
            self.handle_prompt(&context).await?;
        }

        Ok(())
    }

    async fn handle_command(&self, context: &MessageContext, command: ParsedCommand) -> Result<()> {
        match command.name.as_str() {
            "start" | "help" => self.reply_text(context, &help_text()).await?,
            "new" => self.cmd_new(context, command.args).await?,
            "list" => self.cmd_list(context, command.args).await?,
            "switch" => self.cmd_switch(context, command.args).await?,
            "history" => self.cmd_history(context, command.args).await?,
            "usage" => self.cmd_usage(context).await?,
            "quiet" => self.cmd_quiet(context, command.args).await?,
            "remove" | "delete" => self.cmd_remove(context, command.args).await?,
            "current" => self.cmd_current(context).await?,
            "status" => self.cmd_status(context).await?,
            "mode" => self.cmd_mode(context, command.args).await?,
            "model" => self.cmd_model(context, command.args).await?,
            "reasoning" => self.cmd_reasoning(context, command.args).await?,
            "stop" => self.cmd_stop(context).await?,
            _ => {
                self.reply_text(
                    context,
                    "Unknown command. Use /help to see the available commands.",
                )
                .await?;
            }
        }
        Ok(())
    }

    async fn handle_prompt(&self, context: &MessageContext) -> Result<()> {
        if self.active_run(&context.session_key).is_some() {
            self.reply_text(
                context,
                "A Codex request is already running for this chat. Wait for it to finish or use /stop.",
            )
            .await?;
            return Ok(());
        }

        let session = self.state.session(&context.session_key);
        let settings = self.state.runtime_settings();
        let cancel = CancellationToken::new();
        let preview = self
            .telegram
            .send_message(context.chat_id, "Processing...", Some(context.message_id))
            .await?;
        let spawned = match self.codex.spawn_turn(
            session.thread_id.as_deref(),
            &settings,
            &context.text,
            cancel.clone(),
        ) {
            Ok(spawned) => spawned,
            Err(err) => {
                self.render_terminal_text(context, preview.message_id, &format!("Error: {err}"))
                    .await?;
                return Err(err);
            }
        };

        let run_id = self.register_run(
            &context.session_key,
            cancel.clone(),
            spawned.pid.load(Ordering::Relaxed),
        );
        let typing_task = self.spawn_typing_task(context.chat_id, cancel.clone());
        let result = self.collect_turn_output(context, preview, spawned).await;
        cancel.cancel();
        typing_task.abort();
        self.finish_run(&context.session_key, run_id);

        match result {
            Ok(outcome) => {
                if let Some(thread_id) = &outcome.thread_id {
                    let _ = self.state.assign_thread_if_generation(
                        &context.session_key,
                        session.generation,
                        thread_id,
                    )?;
                }
            }
            Err(err) => {
                warn!(session = %context.session_key, error = %err, "prompt failed");
            }
        }

        Ok(())
    }

    async fn collect_turn_output(
        &self,
        context: &MessageContext,
        preview: SentMessage,
        mut spawned: SpawnedTurn,
    ) -> Result<TurnOutcome> {
        let mut last_status = "Processing...".to_string();
        let mut final_text: Option<String> = None;
        let quiet = self.state.session(&context.session_key).quiet;

        while let Some(event) = spawned.events.recv().await {
            match event {
                TurnEvent::Status(status) => {
                    if quiet {
                        continue;
                    }
                    let next_status = format!("Processing...\n\n{}", truncate_text(&status, 1200));
                    if next_status != last_status {
                        let _ = self
                            .telegram
                            .edit_message(context.chat_id, preview.message_id, &next_status)
                            .await;
                        last_status = next_status;
                    }
                }
                TurnEvent::FinalText(text) => {
                    final_text = Some(text);
                }
                TurnEvent::ThreadId => {}
                TurnEvent::Error(message) => {
                    let next_status = format!("Error: {}", truncate_text(&message, 2000));
                    let _ = self
                        .telegram
                        .edit_message(context.chat_id, preview.message_id, &next_status)
                        .await;
                    last_status = next_status;
                }
            }
        }

        let outcome = match spawned.join.await {
            Ok(Ok(mut outcome)) => {
                if outcome.final_text.is_none() {
                    outcome.final_text = final_text;
                }
                outcome
            }
            Ok(Err(err)) => {
                let message = err.to_string();
                if message.contains("request cancelled") {
                    self.render_terminal_text(context, preview.message_id, "Stopped.")
                        .await?;
                } else {
                    self.render_terminal_text(
                        context,
                        preview.message_id,
                        &format!("Error: {message}"),
                    )
                    .await?;
                }
                return Err(err);
            }
            Err(err) => {
                let message = format!("codex task join failed: {err}");
                self.render_terminal_text(
                    context,
                    preview.message_id,
                    &format!("Error: {message}"),
                )
                .await?;
                return Err(anyhow!(message));
            }
        };

        if let Some(text) = &outcome.final_text {
            self.render_terminal_text(context, preview.message_id, text)
                .await?;
        } else {
            self.render_terminal_text(context, preview.message_id, "Done.")
                .await?;
        }

        Ok(outcome)
    }

    async fn render_terminal_text(
        &self,
        context: &MessageContext,
        preview_message_id: i64,
        text: &str,
    ) -> Result<()> {
        let chunks = split_text(text, TELEGRAM_MESSAGE_LIMIT);
        if chunks.is_empty() {
            self.telegram
                .edit_message(context.chat_id, preview_message_id, "(empty response)")
                .await?;
            return Ok(());
        }

        self.telegram
            .edit_message(context.chat_id, preview_message_id, &chunks[0])
            .await?;
        for chunk in chunks.iter().skip(1) {
            self.telegram
                .send_message(context.chat_id, chunk, Some(context.message_id))
                .await?;
        }
        Ok(())
    }

    async fn cmd_new(&self, context: &MessageContext, args: Vec<String>) -> Result<()> {
        let pending_name = join_args(args);
        let busy = self.active_run(&context.session_key);
        self.state
            .reset_session(&context.session_key, pending_name.clone())?;

        if let Some(run) = busy {
            run.cancel.cancel();
            self.reply_text(
                context,
                "Session reset requested. The current run is stopping; send your next message after it exits.",
            )
            .await?;
            return Ok(());
        }

        if let Some(name) = pending_name {
            self.reply_text(context, &format!("Started a new session: {name}"))
                .await?;
        } else {
            self.reply_text(context, "Started a new session.").await?;
        }
        Ok(())
    }

    async fn cmd_list(&self, context: &MessageContext, args: Vec<String>) -> Result<()> {
        let page = args
            .first()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(1);
        let sessions = self.codex.list_sessions()?;
        if sessions.is_empty() {
            self.reply_text(context, "No Codex sessions were found for this workdir.")
                .await?;
            return Ok(());
        }

        let current = self.state.session(&context.session_key).thread_id;
        let names = self.state.all_thread_names();
        let total_pages = sessions.len().div_ceil(PAGE_SIZE);
        let page = page.min(total_pages.max(1));
        let start = (page - 1) * PAGE_SIZE;
        let end = (start + PAGE_SIZE).min(sessions.len());

        let mut lines = Vec::new();
        lines.push(format!(
            "Codex sessions for {}\nPage {page}/{total_pages}",
            self.codex.work_dir().display()
        ));
        for (index, session) in sessions[start..end].iter().enumerate() {
            let absolute_index = start + index + 1;
            let marker = if current.as_deref() == Some(session.id.as_str()) {
                "*"
            } else {
                " "
            };
            let display = session_display_name(session, &names);
            let modified = format_time(session.modified_at);
            lines.push(format!(
                "{marker} {absolute_index}. {display} [{id}] msgs={msgs} updated={modified}",
                id = short_id(&session.id),
                msgs = session.message_count
            ));
        }
        lines.push(
            "Use /switch <number|id|name> to switch, or /remove <number|id|name> to delete."
                .to_string(),
        );
        self.reply_text(context, &lines.join("\n")).await?;
        Ok(())
    }

    async fn cmd_switch(&self, context: &MessageContext, args: Vec<String>) -> Result<()> {
        if args.is_empty() {
            self.reply_text(context, "Usage: /switch <number|id_prefix|name>")
                .await?;
            return Ok(());
        }

        let query = args.join(" ");
        let sessions = self.codex.list_sessions()?;
        let names = self.state.all_thread_names();
        let Some(matched) = match_session(&sessions, &names, &query) else {
            self.reply_text(context, &format!("No session matched: {query}"))
                .await?;
            return Ok(());
        };

        let busy = self.active_run(&context.session_key);
        self.state
            .switch_session(&context.session_key, matched.id.clone())?;
        if let Some(run) = busy {
            run.cancel.cancel();
            self.reply_text(
                context,
                &format!(
                    "Switched to {}. The current run is stopping; your next prompt will use the new session.",
                    session_display_name(matched, &names)
                ),
            )
            .await?;
            return Ok(());
        }

        self.reply_text(
            context,
            &format!("Switched to {}.", session_display_name(matched, &names)),
        )
        .await?;
        Ok(())
    }

    async fn cmd_current(&self, context: &MessageContext) -> Result<()> {
        let session = self.state.session(&context.session_key);
        if let Some(thread_id) = session.thread_id {
            let names = self.state.all_thread_names();
            let name = names
                .get(&thread_id)
                .cloned()
                .unwrap_or_else(|| short_id(&thread_id).to_string());
            self.reply_text(
                context,
                &format!("Current session: {name} ({})", short_id(&thread_id)),
            )
            .await?;
            return Ok(());
        }

        if let Some(name) = session.pending_name {
            self.reply_text(
                context,
                &format!("Current session is fresh and will be named: {name}"),
            )
            .await?;
        } else {
            self.reply_text(context, "No active Codex session is selected yet.")
                .await?;
        }
        Ok(())
    }

    async fn cmd_history(&self, context: &MessageContext, args: Vec<String>) -> Result<()> {
        let limit = args
            .first()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(10)
            .min(50);
        let session = self.state.session(&context.session_key);
        let Some(thread_id) = session.thread_id else {
            self.reply_text(context, "No active Codex session is selected yet.")
                .await?;
            return Ok(());
        };

        let entries = match self.codex.get_session_history(&thread_id, limit) {
            Ok(entries) => entries,
            Err(err) => {
                self.reply_text(context, &format!("Failed to load history: {err}"))
                    .await?;
                return Err(err);
            }
        };
        if entries.is_empty() {
            self.reply_text(context, "No history is available for the current session.")
                .await?;
            return Ok(());
        }

        let mut lines = Vec::new();
        lines.push(format!("History (last {}):", entries.len()));
        lines.push(String::new());
        for entry in entries {
            let role = if entry.role == "assistant" { "A" } else { "U" };
            let timestamp = entry
                .timestamp
                .with_timezone(&Local)
                .format("%m-%d %H:%M")
                .to_string();
            lines.push(format!(
                "{role} [{timestamp}]\n{}",
                truncate_text(&entry.content, 240)
            ));
            lines.push(String::new());
        }

        let text = lines.join("\n");
        self.reply_text(context, text.trim_end()).await?;
        Ok(())
    }

    async fn cmd_usage(&self, context: &MessageContext) -> Result<()> {
        let report = match self.codex.get_usage().await {
            Ok(report) => report,
            Err(err) => {
                self.reply_text(context, &format!("Failed to fetch usage: {err}"))
                    .await?;
                return Err(err);
            }
        };
        self.reply_text(context, &format_usage_report(&report))
            .await?;
        Ok(())
    }

    async fn cmd_quiet(&self, context: &MessageContext, args: Vec<String>) -> Result<()> {
        let current = self.state.session(&context.session_key);
        let next = match args.first().map(|value| value.to_ascii_lowercase()) {
            Some(value) if matches!(value.as_str(), "on" | "true" | "1") => true,
            Some(value) if matches!(value.as_str(), "off" | "false" | "0") => false,
            Some(value) if value == "toggle" => !current.quiet,
            Some(_) => {
                self.reply_text(context, "Usage: /quiet [on|off]").await?;
                return Ok(());
            }
            None => !current.quiet,
        };

        self.state.set_quiet(&context.session_key, next)?;
        let text = if next {
            "Quiet mode ON. Thinking and tool progress messages are hidden for this chat."
        } else {
            "Quiet mode OFF. Thinking and tool progress messages are visible for this chat."
        };
        self.reply_text(context, text).await?;
        Ok(())
    }

    async fn cmd_remove(&self, context: &MessageContext, args: Vec<String>) -> Result<()> {
        if args.is_empty() {
            self.reply_text(context, "Usage: /remove <number|id|name>")
                .await?;
            return Ok(());
        }

        let query = args.join(" ");
        let sessions = self.codex.list_sessions()?;
        let names = self.state.all_thread_names();
        let Some(matched) = match_session(&sessions, &names, &query) else {
            self.reply_text(context, &format!("No session matched: {query}"))
                .await?;
            return Ok(());
        };

        let active_thread = self.state.session(&context.session_key).thread_id;
        if active_thread.as_deref() == Some(matched.id.as_str()) {
            self.reply_text(
                context,
                "Cannot remove the current active session. Switch away from it or use /new first.",
            )
            .await?;
            return Ok(());
        }

        let display = session_display_name(matched, &names);
        if let Err(err) = self.codex.delete_session(&matched.id) {
            self.reply_text(context, &format!("Failed to remove session: {err}"))
                .await?;
            return Err(err);
        }
        self.state.remove_thread_everywhere(&matched.id)?;
        self.reply_text(context, &format!("Removed session: {display}"))
            .await?;
        Ok(())
    }

    async fn cmd_status(&self, context: &MessageContext) -> Result<()> {
        let session = self.state.session(&context.session_key);
        let runtime = self.state.runtime_settings();
        let running = self.active_run(&context.session_key);
        let current = session
            .thread_id
            .as_deref()
            .map(short_id)
            .unwrap_or("(fresh)");
        let pending_name = session.pending_name.unwrap_or_else(|| "-".to_string());
        let running_text = running
            .map(|run| {
                format!(
                    "yes ({}s, pid={})",
                    run.started_at.elapsed().as_secs(),
                    run.pid
                )
            })
            .unwrap_or_else(|| "no".to_string());

        let text = format!(
            "Bot: @{bot}\nUser: {user}\nUser ID: {user_id}\nChat: {chat}\nSession key: {session_key}\nWorkdir: {workdir}\nMode: {mode}\nModel: {model}\nReasoning: {reasoning}\nQuiet: {quiet}\nActive thread: {current}\nPending name: {pending_name}\nRunning: {running}",
            bot = self.bot.username,
            user = context.user_name,
            user_id = context.user_id,
            chat = context
                .chat_name
                .clone()
                .unwrap_or_else(|| context.chat_id.to_string()),
            session_key = context.session_key,
            workdir = self.codex.work_dir().display(),
            mode = runtime.mode,
            model = runtime.model.unwrap_or_else(|| "(default)".to_string()),
            reasoning = runtime
                .reasoning_effort
                .unwrap_or_else(|| "(default)".to_string()),
            quiet = if session.quiet { "on" } else { "off" },
            running = running_text,
        );
        self.reply_text(context, &text).await?;
        Ok(())
    }

    async fn cmd_mode(&self, context: &MessageContext, args: Vec<String>) -> Result<()> {
        let current = self.state.runtime_settings();
        if args.is_empty() {
            self.reply_text(
                context,
                &format!(
                    "Current mode: {}\nAvailable modes: suggest, full-auto, yolo",
                    current.mode
                ),
            )
            .await?;
            return Ok(());
        }

        let runtime = RuntimeSettings::new(
            current.model,
            current.reasoning_effort,
            Some(args[0].clone()),
        );
        self.state.set_runtime_settings(runtime.clone())?;
        self.reply_text(
            context,
            &format!("Mode set to {}. It will apply to new turns.", runtime.mode),
        )
        .await?;
        Ok(())
    }

    async fn cmd_model(&self, context: &MessageContext, args: Vec<String>) -> Result<()> {
        let mut runtime = self.state.runtime_settings();
        if args.is_empty() {
            self.reply_text(
                context,
                &format!(
                    "Current model: {}",
                    runtime.model.unwrap_or_else(|| "(default)".to_string())
                ),
            )
            .await?;
            return Ok(());
        }

        runtime.model = Some(args.join(" "));
        self.state.set_runtime_settings(runtime.clone())?;
        self.reply_text(
            context,
            &format!(
                "Model set to {}. It will apply to new turns.",
                runtime.model.unwrap_or_default()
            ),
        )
        .await?;
        Ok(())
    }

    async fn cmd_reasoning(&self, context: &MessageContext, args: Vec<String>) -> Result<()> {
        let current = self.state.runtime_settings();
        if args.is_empty() {
            self.reply_text(
                context,
                &format!(
                    "Current reasoning effort: {}\nAvailable values: low, medium, high, xhigh",
                    current
                        .reasoning_effort
                        .unwrap_or_else(|| "(default)".to_string())
                ),
            )
            .await?;
            return Ok(());
        }

        let runtime =
            RuntimeSettings::new(current.model, Some(args[0].clone()), Some(current.mode));
        self.state.set_runtime_settings(runtime.clone())?;
        self.reply_text(
            context,
            &format!(
                "Reasoning effort set to {}. It will apply to new turns.",
                runtime
                    .reasoning_effort
                    .unwrap_or_else(|| "(default)".to_string())
            ),
        )
        .await?;
        Ok(())
    }

    async fn cmd_stop(&self, context: &MessageContext) -> Result<()> {
        if let Some(run) = self.active_run(&context.session_key) {
            run.cancel.cancel();
            self.reply_text(context, "Stopping the current request.")
                .await?;
        } else {
            self.reply_text(context, "No active request is running.")
                .await?;
        }
        Ok(())
    }

    async fn reply_text(&self, context: &MessageContext, text: &str) -> Result<()> {
        for chunk in split_text(text, TELEGRAM_MESSAGE_LIMIT) {
            self.telegram
                .send_message(context.chat_id, &chunk, Some(context.message_id))
                .await?;
        }
        Ok(())
    }

    fn register_run(&self, session_key: &str, cancel: CancellationToken, pid: u32) -> u64 {
        let run_id = self.next_run_id.fetch_add(1, Ordering::Relaxed);
        self.active_runs.lock().insert(
            session_key.to_string(),
            ActiveRun {
                run_id,
                cancel,
                started_at: Instant::now(),
                pid,
            },
        );
        run_id
    }

    fn finish_run(&self, session_key: &str, run_id: u64) {
        let mut active_runs = self.active_runs.lock();
        let should_remove = active_runs
            .get(session_key)
            .is_some_and(|active| active.run_id == run_id);
        if should_remove {
            active_runs.remove(session_key);
        }
    }

    fn active_run(&self, session_key: &str) -> Option<ActiveRun> {
        self.active_runs.lock().get(session_key).cloned()
    }

    fn session_key(&self, message: &Message, user_id: i64) -> String {
        if self.config.telegram.share_session_in_channel {
            format!("telegram:{}", message.chat.id)
        } else {
            format!("telegram:{}:{user_id}", message.chat.id)
        }
    }

    fn is_directed_at_bot(&self, message: &Message) -> bool {
        let Some(text) = message.text.as_deref() else {
            return false;
        };

        if let Some(command) = text.split_whitespace().next() {
            if command.starts_with('/') {
                if let Some((_, target)) = command.trim_start_matches('/').split_once('@') {
                    return target.eq_ignore_ascii_case(&self.bot.username);
                }
                return true;
            }
        }

        let mention = format!("@{}", self.bot.username.to_ascii_lowercase());
        if text.to_ascii_lowercase().contains(&mention) {
            return true;
        }

        message
            .reply_to_message
            .as_ref()
            .and_then(|reply| reply.from.as_ref())
            .is_some_and(|from| from.id == self.bot.id)
    }

    fn spawn_typing_task(&self, chat_id: i64, cancel: CancellationToken) -> JoinHandle<()> {
        let telegram = self.telegram.clone();
        tokio::spawn(async move {
            loop {
                if cancel.is_cancelled() {
                    return;
                }
                let _ = telegram.send_chat_action(chat_id, "typing").await;
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                }
            }
        })
    }
}

fn parse_command(text: &str, bot_username: &str) -> Option<ParsedCommand> {
    let mut parts = text.split_whitespace();
    let first = parts.next()?;
    if !first.starts_with('/') {
        return None;
    }

    let first = first.trim_start_matches('/');
    let (name, target) = first
        .split_once('@')
        .map(|(name, target)| (name.to_ascii_lowercase(), Some(target)))
        .unwrap_or_else(|| (first.to_ascii_lowercase(), None));

    if let Some(target) = target {
        if !target.eq_ignore_ascii_case(bot_username) {
            return None;
        }
    }

    Some(ParsedCommand {
        name,
        args: parts.map(str::to_string).collect(),
    })
}

fn strip_bot_mentions(text: &str, bot_username: &str) -> String {
    let mention = format!("@{bot_username}");
    text.replace(&mention, "").trim().to_string()
}

fn help_text() -> String {
    [
        "/help - Show this help",
        "/new [name] - Start a fresh session",
        "/list [page] - List Codex sessions in this workdir",
        "/switch <number|id|name> - Switch to a previous session",
        "/history [n] - Show recent messages for the current session",
        "/usage - Show Codex quota usage",
        "/quiet [on|off] - Toggle progress messages for this chat",
        "/remove <number|id|name> - Delete a session from local Codex storage",
        "/current - Show the current session",
        "/status - Show runtime status",
        "/mode [suggest|full-auto|yolo] - Show or set mode",
        "/model [name] - Show or set the Codex model",
        "/reasoning [low|medium|high|xhigh] - Show or set reasoning effort",
        "/stop - Stop the current request",
        "",
        "Any non-command text is forwarded to Codex.",
    ]
    .join("\n")
}

fn menu_commands() -> Vec<BotCommand> {
    vec![
        BotCommand {
            command: "help".to_string(),
            description: "Show help".to_string(),
        },
        BotCommand {
            command: "new".to_string(),
            description: "Start a fresh session".to_string(),
        },
        BotCommand {
            command: "list".to_string(),
            description: "List Codex sessions".to_string(),
        },
        BotCommand {
            command: "switch".to_string(),
            description: "Switch to another session".to_string(),
        },
        BotCommand {
            command: "history".to_string(),
            description: "Show recent session history".to_string(),
        },
        BotCommand {
            command: "usage".to_string(),
            description: "Show Codex quota usage".to_string(),
        },
        BotCommand {
            command: "quiet".to_string(),
            description: "Toggle progress messages".to_string(),
        },
        BotCommand {
            command: "remove".to_string(),
            description: "Delete a saved session".to_string(),
        },
        BotCommand {
            command: "current".to_string(),
            description: "Show current session".to_string(),
        },
        BotCommand {
            command: "status".to_string(),
            description: "Show bridge status".to_string(),
        },
        BotCommand {
            command: "mode".to_string(),
            description: "Show or set Codex mode".to_string(),
        },
        BotCommand {
            command: "model".to_string(),
            description: "Show or set model".to_string(),
        },
        BotCommand {
            command: "reasoning".to_string(),
            description: "Show or set reasoning effort".to_string(),
        },
        BotCommand {
            command: "stop".to_string(),
            description: "Stop the current request".to_string(),
        },
    ]
}

fn display_name(user: &User) -> String {
    user.display_name()
}

fn session_display_name(session: &SessionSummary, names: &HashMap<String, String>) -> String {
    names
        .get(&session.id)
        .cloned()
        .filter(|name| !name.trim().is_empty())
        .or_else(|| (!session.summary.trim().is_empty()).then(|| session.summary.clone()))
        .unwrap_or_else(|| "(empty)".to_string())
}

fn match_session<'a>(
    sessions: &'a [SessionSummary],
    names: &'a HashMap<String, String>,
    query: &str,
) -> Option<&'a SessionSummary> {
    if let Ok(index) = query.trim().parse::<usize>() {
        if (1..=sessions.len()).contains(&index) {
            return sessions.get(index - 1);
        }
    }

    let query_lower = query.trim().to_ascii_lowercase();
    if query_lower.is_empty() {
        return None;
    }

    for session in sessions {
        if names
            .get(&session.id)
            .is_some_and(|name| name.eq_ignore_ascii_case(&query_lower))
        {
            return Some(session);
        }
    }

    for session in sessions {
        if session.id.starts_with(query) {
            return Some(session);
        }
    }

    for session in sessions {
        if names
            .get(&session.id)
            .is_some_and(|name| name.to_ascii_lowercase().starts_with(&query_lower))
        {
            return Some(session);
        }
    }

    sessions
        .iter()
        .find(|session| session.summary.to_ascii_lowercase().contains(&query_lower))
}

fn format_time(time: SystemTime) -> String {
    let local: DateTime<Local> = DateTime::from(time);
    local.format("%m-%d %H:%M").to_string()
}

fn short_id(session_id: &str) -> &str {
    let max = 12.min(session_id.len());
    &session_id[..max]
}

fn split_text(text: &str, max_chars: usize) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }

    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize;
    for ch in text.chars() {
        current.push(ch);
        current_len += 1;
        if current_len >= max_chars {
            chunks.push(current);
            current = String::new();
            current_len = 0;
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn truncate_text(text: &str, max_chars: usize) -> String {
    let mut out = text.chars().take(max_chars).collect::<String>();
    if text.chars().count() > max_chars {
        out.push_str("...");
    }
    out
}

fn join_args(args: Vec<String>) -> Option<String> {
    let value = args.join(" ");
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn format_usage_report(report: &UsageReport) -> String {
    let mut lines = Vec::new();
    lines.push(format!("Account: {}", usage_account_display(report)));
    lines.push(format!(
        "Provider: {}",
        if report.provider.trim().is_empty() {
            "codex"
        } else {
            report.provider.as_str()
        }
    ));

    if let Some(credits) = &report.credits {
        lines.push(format!("Credits: {}", format_credits(credits)));
    }

    if report.buckets.is_empty() {
        lines.push("No usage buckets were returned.".to_string());
        return lines.join("\n");
    }

    for bucket in &report.buckets {
        lines.push(String::new());
        lines.push(format!(
            "{}: allowed={}, limit_reached={}",
            bucket.name,
            yes_no(bucket.allowed),
            yes_no(bucket.limit_reached)
        ));

        if bucket.windows.is_empty() {
            lines.push("No windows reported.".to_string());
            continue;
        }

        for window in &bucket.windows {
            let remaining = (100 - window.used_percent).max(0);
            lines.push(format!(
                "{}: remaining={}%, resets in {}",
                usage_window_label(window.window_seconds),
                remaining,
                format_reset_after(window.reset_after_seconds)
            ));
        }
    }

    lines.join("\n")
}

fn usage_account_display(report: &UsageReport) -> String {
    let base = if !report.email.trim().is_empty() {
        report.email.clone()
    } else if !report.account_id.trim().is_empty() {
        report.account_id.clone()
    } else if !report.user_id.trim().is_empty() {
        report.user_id.clone()
    } else {
        "-".to_string()
    };

    if report.plan.trim().is_empty() {
        base
    } else {
        format!("{base} ({})", report.plan)
    }
}

fn format_credits(credits: &crate::codex::UsageCredits) -> String {
    if credits.unlimited {
        return "unlimited".to_string();
    }
    if let Some(balance) = &credits.balance {
        return format!(
            "available={}, balance={}",
            yes_no(credits.has_credits),
            balance
        );
    }
    format!("available={}", yes_no(credits.has_credits))
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn usage_window_label(seconds: i64) -> String {
    match seconds {
        18_000 => "5h limit".to_string(),
        604_800 => "7d limit".to_string(),
        86_400 => "24h limit".to_string(),
        _ if seconds > 0 => format!("{} window", format_duration(seconds)),
        _ => "window".to_string(),
    }
}

fn format_reset_after(seconds: i64) -> String {
    if seconds <= 0 {
        "-".to_string()
    } else {
        format_duration(seconds)
    }
}

fn format_duration(total_seconds: i64) -> String {
    let total_seconds = total_seconds.max(0);
    let days = total_seconds / 86_400;
    let hours = (total_seconds % 86_400) / 3_600;
    let minutes = (total_seconds % 3_600) / 60;

    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{total_seconds}s")
    }
}
