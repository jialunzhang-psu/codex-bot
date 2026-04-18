use anyhow::{Context, Result, anyhow, bail};
use frontend_common::CommandSpec;
use reqwest::Client;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub struct TelegramClient {
    http: Client,
    api_base: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Update {
    pub update_id: i64,
    pub message: Option<Message>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub message_id: i64,
    pub chat: Chat,
    pub text: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Chat {
    pub id: i64,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum TelegramParseMode {
    Html,
}

#[derive(Debug, Clone, Serialize)]
pub struct BotCommand {
    pub command: String,
    pub description: String,
}

pub fn bot_commands(commands: &[CommandSpec]) -> Vec<BotCommand> {
    commands
        .iter()
        .map(|command| BotCommand {
            command: command.command.clone(),
            description: command.description.clone(),
        })
        .collect()
}

#[derive(Debug, Deserialize)]
struct ApiEnvelope<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

impl TelegramClient {
    pub fn new(token: impl Into<String>) -> Result<Self> {
        let token = token.into();
        if token.trim().is_empty() {
            bail!("telegram token is empty");
        }
        Ok(Self {
            http: Client::builder()
                .user_agent(concat!(
                    env!("CARGO_PKG_NAME"),
                    "/",
                    env!("CARGO_PKG_VERSION")
                ))
                .build()
                .context("failed to build HTTP client")?,
            api_base: format!("https://api.telegram.org/bot{token}"),
        })
    }

    pub async fn get_updates(&self, offset: i64, timeout_seconds: u64) -> Result<Vec<Update>> {
        #[derive(Serialize)]
        struct Payload {
            offset: i64,
            timeout: u64,
            #[serde(rename = "allowed_updates")]
            allowed_updates: [&'static str; 1],
        }

        self.call(
            "getUpdates",
            &Payload {
                offset,
                timeout: timeout_seconds,
                allowed_updates: ["message"],
            },
        )
        .await
    }

    pub async fn set_my_commands(&self, commands: &[BotCommand]) -> Result<()> {
        #[derive(Serialize)]
        struct Payload<'a> {
            commands: &'a [BotCommand],
        }

        let _: bool = self.call("setMyCommands", &Payload { commands }).await?;
        Ok(())
    }

    pub async fn send_message_formatted(
        &self,
        chat_id: i64,
        text: &str,
        reply_to_message_id: Option<i64>,
        parse_mode: Option<TelegramParseMode>,
    ) -> Result<()> {
        #[derive(Serialize)]
        struct Payload<'a> {
            chat_id: i64,
            text: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            reply_to_message_id: Option<i64>,
            disable_web_page_preview: bool,
            #[serde(skip_serializing_if = "Option::is_none")]
            parse_mode: Option<TelegramParseMode>,
        }

        let _: serde_json::Value = self
            .call(
                "sendMessage",
                &Payload {
                    chat_id,
                    text,
                    reply_to_message_id,
                    disable_web_page_preview: true,
                    parse_mode,
                },
            )
            .await?;
        Ok(())
    }

    async fn call<T: DeserializeOwned, P: Serialize>(
        &self,
        method: &str,
        payload: &P,
    ) -> Result<T> {
        let url = format!("{}/{}", self.api_base, method);
        let response = self
            .http
            .post(url)
            .json(payload)
            .send()
            .await
            .context("telegram request failed")?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .context("failed to read telegram response")?;
        let envelope: ApiEnvelope<T> = serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "failed to decode telegram response (status {}): {}",
                status,
                String::from_utf8_lossy(&bytes)
            )
        })?;
        if envelope.ok {
            envelope
                .result
                .ok_or_else(|| anyhow!("telegram response missing result"))
        } else {
            Err(anyhow!(
                envelope
                    .description
                    .unwrap_or_else(|| "telegram request failed".to_string())
            ))
        }
    }
}
