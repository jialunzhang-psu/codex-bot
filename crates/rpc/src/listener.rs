use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Serialize, de::DeserializeOwned};
use tokio::io::AsyncBufReadExt;
use tokio::net::{UnixListener, UnixStream};

use crate::RpcRoute;
use crate::endpoint::resolve_socket_path;
use crate::transport::write_payload;

pub struct RpcServer {
    socket_path: PathBuf,
    listener: UnixListener,
}

#[derive(Debug)]
pub struct RpcReply {
    stream: UnixStream,
}

#[derive(Serialize)]
struct ResponseEnvelope<T> {
    ok: bool,
    response: Option<T>,
    error: Option<String>,
}

impl RpcServer {
    pub fn new(config_path: &Path, route: RpcRoute) -> Result<Self> {
        let socket_path = resolve_socket_path(config_path, &route)?;
        if let Some(parent) = socket_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create socket parent {}", parent.display()))?;
        }
        if socket_path.exists() {
            if std::os::unix::net::UnixStream::connect(&socket_path).is_ok() {
                return Err(anyhow!("socket already active: {}", socket_path.display()));
            }
            fs::remove_file(&socket_path).with_context(|| {
                format!("failed to remove stale socket {}", socket_path.display())
            })?;
        }
        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("failed to bind socket {}", socket_path.display()))?;
        Ok(Self {
            socket_path,
            listener,
        })
    }

    pub async fn accept_line(&self) -> Result<(String, RpcReply)> {
        let (stream, _) =
            self.listener.accept().await.with_context(|| {
                format!("failed to accept socket {}", self.socket_path.display())
            })?;
        let mut reader = tokio::io::BufReader::new(stream);
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .await
            .context("failed to read rpc request")?;
        if read == 0 {
            return Err(anyhow!("socket closed without request payload"));
        }
        Ok((line, RpcReply::from(reader.into_inner())))
    }

    pub async fn accept<Request>(&self) -> Result<(Request, RpcReply)>
    where
        Request: DeserializeOwned,
    {
        let (line, reply) = self.accept_line().await?;
        if line.trim().is_empty() {
            return Err(anyhow!("socket closed without request payload"));
        }
        let request = serde_json::from_str(line.trim()).context("failed to decode rpc request")?;
        Ok((request, reply))
    }
}

impl RpcReply {
    pub async fn send_line(&mut self, line: &str) -> Result<()> {
        write_payload(&mut self.stream, line).await
    }

    pub async fn send_json<T>(&mut self, value: &T) -> Result<()>
    where
        T: Serialize,
    {
        let payload = serde_json::to_string(value).context("failed to encode rpc response")?;
        write_payload(&mut self.stream, &payload).await
    }

    pub async fn send_ok<T>(&mut self, value: &T) -> Result<()>
    where
        T: Serialize,
    {
        self.send_json(&ResponseEnvelope {
            ok: true,
            response: Some(value),
            error: None,
        })
        .await
    }

    pub async fn send_err(&mut self, message: impl Into<String>) -> Result<()> {
        self.send_json(&ResponseEnvelope::<()> {
            ok: false,
            response: None,
            error: Some(message.into()),
        })
        .await
    }
}

impl From<UnixStream> for RpcReply {
    fn from(stream: UnixStream) -> Self {
        Self { stream }
    }
}

impl Drop for RpcServer {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket_path);
    }
}
