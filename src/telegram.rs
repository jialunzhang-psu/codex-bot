use anyhow::{Context, Result, anyhow, bail};
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
    pub date: i64,
    pub from: Option<User>,
    pub chat: Chat,
    pub text: Option<String>,
    pub reply_to_message: Option<Box<Message>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub id: i64,
    pub username: Option<String>,
    pub first_name: String,
    pub last_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Chat {
    pub id: i64,
    #[serde(rename = "type")]
    pub kind: String,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SentMessage {
    pub message_id: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BotIdentity {
    pub id: i64,
    pub username: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BotCommand {
    pub command: String,
    pub description: String,
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

    pub async fn get_me(&self) -> Result<BotIdentity> {
        #[derive(Deserialize)]
        struct MeResult {
            id: i64,
            username: String,
        }

        let result: MeResult = self.call("getMe", &EmptyPayload {}).await?;
        Ok(BotIdentity {
            id: result.id,
            username: result.username,
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

    pub async fn send_message(
        &self,
        chat_id: i64,
        text: &str,
        reply_to_message_id: Option<i64>,
    ) -> Result<SentMessage> {
        #[derive(Serialize)]
        struct Payload<'a> {
            chat_id: i64,
            text: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            reply_to_message_id: Option<i64>,
            disable_web_page_preview: bool,
        }

        self.call(
            "sendMessage",
            &Payload {
                chat_id,
                text,
                reply_to_message_id,
                disable_web_page_preview: true,
            },
        )
        .await
    }

    pub async fn edit_message(&self, chat_id: i64, message_id: i64, text: &str) -> Result<()> {
        #[derive(Serialize)]
        struct Payload<'a> {
            chat_id: i64,
            message_id: i64,
            text: &'a str,
            disable_web_page_preview: bool,
        }

        let response: Result<SentMessage> = self
            .call(
                "editMessageText",
                &Payload {
                    chat_id,
                    message_id,
                    text,
                    disable_web_page_preview: true,
                },
            )
            .await;

        match response {
            Ok(_) => Ok(()),
            Err(err) if err.to_string().contains("message is not modified") => Ok(()),
            Err(err) => Err(err),
        }
    }

    pub async fn send_chat_action(&self, chat_id: i64, action: &str) -> Result<()> {
        #[derive(Serialize)]
        struct Payload<'a> {
            chat_id: i64,
            action: &'a str,
        }

        let _: bool = self
            .call("sendChatAction", &Payload { chat_id, action })
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
            .with_context(|| format!("telegram API request failed for {method}"))?;

        let status = response.status();
        let body = response
            .text()
            .await
            .with_context(|| format!("telegram API body read failed for {method}"))?;

        if !status.is_success() {
            return Err(anyhow!("telegram API {method} returned {status}: {body}"));
        }

        let envelope: ApiEnvelope<T> = serde_json::from_str(&body)
            .with_context(|| format!("telegram API {method} returned invalid JSON"))?;
        if !envelope.ok {
            return Err(anyhow!(
                "telegram API {method} failed: {}",
                envelope
                    .description
                    .unwrap_or_else(|| "unknown error".to_string())
            ));
        }

        envelope
            .result
            .ok_or_else(|| anyhow!("telegram API {method} returned no result"))
    }
}

#[derive(Serialize)]
struct EmptyPayload {}

impl User {
    pub fn display_name(&self) -> String {
        self.username.clone().unwrap_or_else(|| {
            let mut parts = vec![self.first_name.clone()];
            if let Some(last_name) = &self.last_name {
                if !last_name.trim().is_empty() {
                    parts.push(last_name.clone());
                }
            }
            parts.join(" ")
        })
    }
}

impl Message {
    pub fn is_group(&self) -> bool {
        matches!(self.chat.kind.as_str(), "group" | "supergroup")
    }
}
