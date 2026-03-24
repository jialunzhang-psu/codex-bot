use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt};
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::{
    CodexClient, RuntimeSettings, normalize_optional, normalize_reasoning, patch_session_source_in,
};

#[derive(Debug)]
pub enum TurnEvent {
    Status(String),
    FinalText(String),
    CodexSessionId,
    Error(String),
}

#[derive(Debug)]
pub struct TurnOutcome {
    pub codex_session_id: Option<String>,
    pub final_text: Option<String>,
}

pub struct SpawnedTurn {
    pub events: mpsc::Receiver<TurnEvent>,
    pub join: JoinHandle<Result<TurnOutcome>>,
    pub pid: Arc<AtomicU32>,
}

#[derive(Default)]
struct EventParser {
    codex_session_id: Option<String>,
    pending_messages: Vec<String>,
    final_text: Option<String>,
    fatal_error: Option<String>,
}

impl CodexClient {
    pub fn spawn_turn(
        &self,
        codex_session_id: Option<&str>,
        settings: &RuntimeSettings,
        prompt: &str,
        cancel: CancellationToken,
    ) -> Result<SpawnedTurn> {
        if prompt.trim().is_empty() {
            bail!("prompt is empty");
        }

        let args = build_exec_args(&self.work_dir, codex_session_id, settings, prompt);
        let mut command = self.command();
        command
            .args(&args)
            .current_dir(&self.work_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let mut child = command.spawn().with_context(|| {
            format!(
                "failed to start codex process: {} {}",
                self.bin,
                args.join(" ")
            )
        })?;

        let pid = Arc::new(AtomicU32::new(child.id().unwrap_or_default()));
        let stdout = child
            .stdout
            .take()
            .context("codex stdout pipe is unavailable")?;
        let stderr = child
            .stderr
            .take()
            .context("codex stderr pipe is unavailable")?;
        let child = Arc::new(Mutex::new(child));

        let (tx, rx) = mpsc::channel(64);
        let join = {
            let child = Arc::clone(&child);
            let pid_ref = Arc::clone(&pid);
            let codex_home = self.codex_home().ok();
            tokio::spawn(async move {
                let stderr_task = tokio::spawn(async move {
                    let mut buf = String::new();
                    let mut reader = tokio::io::BufReader::new(stderr);
                    reader
                        .read_to_string(&mut buf)
                        .await
                        .context("failed to read codex stderr")?;
                    Ok::<String, anyhow::Error>(buf)
                });

                let killer_task = {
                    let child = Arc::clone(&child);
                    let cancel = cancel.clone();
                    tokio::spawn(async move {
                        cancel.cancelled().await;
                        let mut child = child.lock().await;
                        let _ = child.start_kill();
                    })
                };

                let mut parser = EventParser::default();
                let mut lines = tokio::io::BufReader::new(stdout).lines();
                while let Some(line) = lines
                    .next_line()
                    .await
                    .context("failed to read codex stdout line")?
                {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
                        for event in parser.ingest(&value) {
                            if tx.send(event).await.is_err() {
                                break;
                            }
                        }
                    }
                }

                let status = {
                    let mut child = child.lock().await;
                    let status = child.wait().await.context("failed to wait for codex")?;
                    pid_ref.store(0, Ordering::Relaxed);
                    status
                };

                killer_task.abort();
                let stderr = stderr_task.await.context("stderr task join failed")??;

                if parser.final_text.is_none() {
                    if let Some(text) = parser.flush_pending() {
                        parser.final_text = Some(text.clone());
                        let _ = tx.send(TurnEvent::FinalText(text)).await;
                    }
                }

                drop(tx);

                if let (Some(codex_session_id), Some(codex_home)) =
                    (&parser.codex_session_id, codex_home.as_deref())
                {
                    let _ = patch_session_source_in(codex_home, codex_session_id);
                }

                if cancel.is_cancelled() {
                    let message = "request cancelled".to_string();
                    return Err(anyhow!(message));
                }

                if let Some(message) = parser.fatal_error {
                    return Err(anyhow!(message));
                }

                if !status.success() {
                    let stderr = stderr.trim();
                    if !stderr.is_empty() {
                        return Err(anyhow!(stderr.to_string()));
                    }
                    return Err(anyhow!("codex exited with status {status}"));
                }

                Ok(TurnOutcome {
                    codex_session_id: parser.codex_session_id,
                    final_text: parser.final_text,
                })
            })
        };

        Ok(SpawnedTurn {
            events: rx,
            join,
            pid,
        })
    }
}

impl EventParser {
    fn ingest(&mut self, raw: &Value) -> Vec<TurnEvent> {
        let Some(event_type) = raw.get("type").and_then(Value::as_str) else {
            return Vec::new();
        };

        match event_type {
            "thread.started" => raw
                .get("thread_id")
                .and_then(Value::as_str)
                .map(|codex_session_id| {
                    self.codex_session_id = Some(codex_session_id.to_string());
                    vec![TurnEvent::CodexSessionId]
                })
                .unwrap_or_default(),
            "turn.started" => {
                self.pending_messages.clear();
                Vec::new()
            }
            "item.started" => raw
                .get("item")
                .and_then(Value::as_object)
                .and_then(describe_item_started)
                .map(TurnEvent::Status)
                .into_iter()
                .collect(),
            "item.completed" => raw
                .get("item")
                .and_then(Value::as_object)
                .map(|item| self.handle_item_completed(item))
                .unwrap_or_default(),
            "turn.completed" => self
                .flush_pending()
                .map(|text| {
                    self.final_text = Some(text.clone());
                    vec![TurnEvent::FinalText(text)]
                })
                .unwrap_or_default(),
            "turn.failed" => {
                let message = raw
                    .get("error")
                    .and_then(Value::as_object)
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("turn failed")
                    .to_string();
                self.fatal_error = Some(message.clone());
                vec![TurnEvent::Error(message)]
            }
            "error" => raw
                .get("message")
                .and_then(Value::as_str)
                .map(|message| {
                    if message.contains("Reconnecting") || message.contains("Falling back") {
                        TurnEvent::Status(message.to_string())
                    } else {
                        TurnEvent::Error(message.to_string())
                    }
                })
                .into_iter()
                .collect(),
            _ => Vec::new(),
        }
    }

    fn handle_item_completed(&mut self, item: &Map<String, Value>) -> Vec<TurnEvent> {
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
        match item_type {
            "reasoning" => extract_item_text(item, "summary", "summary_text")
                .map(TurnEvent::Status)
                .into_iter()
                .collect(),
            "agent_message" | "message" => {
                if let Some(text) = extract_item_text(item, "content", "output_text") {
                    self.pending_messages.push(text);
                }
                Vec::new()
            }
            "command_execution" | "function_call" | "function_call_output" | "error" => Vec::new(),
            other => tool_display_name(other)
                .map(|tool_name| {
                    let input = extract_tool_input(item);
                    if input.is_empty() {
                        TurnEvent::Status(tool_name.to_string())
                    } else {
                        TurnEvent::Status(format!("{tool_name}: {input}"))
                    }
                })
                .into_iter()
                .collect(),
        }
    }

    fn flush_pending(&mut self) -> Option<String> {
        if self.pending_messages.is_empty() {
            return None;
        }
        let text = self.pending_messages.join("\n\n");
        self.pending_messages.clear();
        Some(text)
    }
}

fn build_exec_args(
    work_dir: &Path,
    codex_session_id: Option<&str>,
    settings: &RuntimeSettings,
    prompt: &str,
) -> Vec<String> {
    let mut args = if codex_session_id.is_some() {
        vec![
            "exec".to_string(),
            "resume".to_string(),
            "--skip-git-repo-check".to_string(),
        ]
    } else {
        vec!["exec".to_string(), "--skip-git-repo-check".to_string()]
    };

    match settings.mode.as_str() {
        "auto-edit" | "full-auto" => args.push("--full-auto".to_string()),
        "yolo" => args.push("--dangerously-bypass-approvals-and-sandbox".to_string()),
        _ => {}
    }

    if let Some(model) = normalize_optional(settings.model.clone()) {
        args.push("--model".to_string());
        args.push(model);
    }
    if let Some(effort) = normalize_reasoning(settings.reasoning_effort.clone()) {
        args.push("-c".to_string());
        args.push(format!("model_reasoning_effort={effort:?}"));
    }

    if let Some(codex_session_id) = codex_session_id {
        args.push(codex_session_id.to_string());
        args.push("--json".to_string());
        args.push(prompt.to_string());
    } else {
        args.push("--json".to_string());
        args.push("--cd".to_string());
        args.push(work_dir.display().to_string());
        args.push(prompt.to_string());
    }

    args
}

fn describe_item_started(item: &Map<String, Value>) -> Option<String> {
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
    match item_type {
        "reasoning" | "message" | "agent_message" => None,
        "command_execution" => item
            .get("command")
            .and_then(Value::as_str)
            .map(|command| format!("Bash: {command}"))
            .or_else(|| Some("Bash".to_string())),
        "function_call" => item
            .get("name")
            .and_then(Value::as_str)
            .map(|name| format!("Tool: {name}"))
            .or_else(|| Some("Tool".to_string())),
        other => tool_display_name(other).map(|name| {
            let input = extract_tool_input(item);
            if input.is_empty() {
                name.to_string()
            } else {
                format!("{name}: {input}")
            }
        }),
    }
}

fn tool_display_name(item_type: &str) -> Option<&'static str> {
    match item_type {
        "web_search" => Some("WebSearch"),
        "file_search" => Some("FileSearch"),
        "code_interpreter" => Some("CodeInterpreter"),
        "computer_use" => Some("ComputerUse"),
        "mcp_tool" => Some("MCP"),
        _ => None,
    }
}

fn extract_item_text(
    item: &Map<String, Value>,
    array_field: &str,
    element_type: &str,
) -> Option<String> {
    if let Some(elements) = item.get(array_field).and_then(Value::as_array) {
        let parts = elements
            .iter()
            .filter_map(Value::as_object)
            .filter(|element| {
                element_type.is_empty()
                    || element
                        .get("type")
                        .and_then(Value::as_str)
                        .is_some_and(|kind| kind == element_type)
            })
            .filter_map(|element| element.get("text").and_then(Value::as_str))
            .filter(|text| !text.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        if !parts.is_empty() {
            return Some(parts.join("\n"));
        }
    }

    item.get("text")
        .and_then(Value::as_str)
        .and_then(|text| (!text.is_empty()).then(|| text.to_string()))
}

fn extract_tool_input(item: &Map<String, Value>) -> String {
    if let Some(action) = item.get("action").and_then(Value::as_object) {
        if let Some(queries) = action.get("queries").and_then(Value::as_array) {
            let parts = queries
                .iter()
                .filter_map(|query| match query {
                    Value::String(text) => Some(text.clone()),
                    Value::Object(object) => object
                        .get("query")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    _ => None,
                })
                .filter(|text| !text.trim().is_empty())
                .collect::<Vec<_>>();
            if !parts.is_empty() {
                return parts.join("\n");
            }
        }

        if let Some(query) = action.get("query").and_then(Value::as_str) {
            if !query.trim().is_empty() {
                return query.to_string();
            }
        }
    }

    item.get("query")
        .and_then(Value::as_str)
        .or_else(|| item.get("name").and_then(Value::as_str))
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_ignores_missing_item() {
        let mut parser = EventParser::default();
        let raw: Value = serde_json::json!({
            "type": "item.completed",
            "item": null
        });
        let events = parser.ingest(&raw);
        assert!(events.is_empty());
        assert!(parser.final_text.is_none());
    }

    #[test]
    fn parser_collects_agent_message_text() {
        let mut parser = EventParser::default();
        let events = parser.ingest(&serde_json::json!({
            "type": "item.completed",
            "item": {
                "type": "agent_message",
                "content": [
                    {"type": "output_text", "text": "hello"},
                    {"type": "output_text", "text": "world"}
                ]
            }
        }));
        assert!(events.is_empty());

        let events = parser.ingest(&serde_json::json!({
            "type": "turn.completed"
        }));
        assert!(
            matches!(events.as_slice(), [TurnEvent::FinalText(text)] if text == "hello\nworld")
        );
    }

    #[test]
    fn parser_extracts_tool_queries_without_panicking() {
        let mut parser = EventParser::default();
        let events = parser.ingest(&serde_json::json!({
            "type": "item.completed",
            "item": {
                "type": "web_search",
                "action": {
                    "queries": [
                        {"query": "rust telegram"},
                        "codex cli"
                    ]
                }
            }
        }));
        assert!(
            matches!(events.as_slice(), [TurnEvent::Status(text)] if text.contains("rust telegram") && text.contains("codex cli"))
        );
    }
}
