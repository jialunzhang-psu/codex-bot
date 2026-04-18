use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::RpcRoute;
use crate::endpoint::resolve_socket_path;

#[derive(Debug, Clone)]
pub struct RpcClient {
    socket_path: PathBuf,
}

pub struct RpcStream {
    reader: BufReader<UnixStream>,
}

#[derive(Debug, Deserialize)]
struct Envelope<S> {
    ok: bool,
    response: Option<S>,
    error: Option<String>,
}

impl RpcClient {
    pub fn new(config_path: &Path, route: RpcRoute) -> Result<Self> {
        Ok(Self {
            socket_path: resolve_socket_path(config_path, &route)?,
        })
    }

    pub async fn request<S, R>(&self, request: &S) -> Result<R>
    where
        S: Serialize,
        R: DeserializeOwned,
    {
        let payload = serde_json::to_string(request).context("failed to encode rpc payload")?;
        let stream = self.connect().await?;
        let (reader_half, mut writer_half) = stream.into_split();
        write_payload(&mut writer_half, &payload).await?;
        drop(writer_half);
        let mut reader = BufReader::new(reader_half);
        let line = read_required_line(&mut reader, "failed to read rpc response").await?;
        decode_response_line(&line)
    }

    pub async fn start_stream<S>(&self, request: &S) -> Result<RpcStream>
    where
        S: Serialize,
    {
        let payload = serde_json::to_string(request).context("failed to encode rpc payload")?;
        let mut stream = self.connect().await?;
        write_payload(&mut stream, &payload).await?;
        Ok(RpcStream {
            reader: BufReader::new(stream),
        })
    }

    pub async fn request_line(&self, line: &str) -> Result<String> {
        let mut stream = self.connect().await?;
        write_payload(&mut stream, line).await?;
        let mut reader = BufReader::new(stream);
        let response = read_required_line(&mut reader, "failed to read rpc response").await?;
        Ok(response.trim_end().to_string())
    }

    pub async fn probe(&self) -> Result<()> {
        self.connect().await.map(|_| ())
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    async fn connect(&self) -> Result<UnixStream> {
        UnixStream::connect(&self.socket_path)
            .await
            .with_context(|| format!("failed to connect socket {}", self.socket_path.display()))
    }
}

impl RpcStream {
    pub async fn next_item<Item>(&mut self) -> Result<Option<Item>>
    where
        Item: DeserializeOwned,
    {
        let Some(line) =
            read_optional_line(&mut self.reader, "failed to read rpc stream item").await?
        else {
            return Ok(None);
        };
        decode_stream_line(&line).map(Some)
    }
}

async fn read_optional_line<R>(reader: &mut R, context: &str) -> Result<Option<String>>
where
    R: AsyncBufReadExt + Unpin,
{
    let mut line = String::new();
    let read = reader
        .read_line(&mut line)
        .await
        .with_context(|| context.to_string())?;
    if read == 0 || line.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(line))
}

async fn read_required_line<R>(reader: &mut R, context: &str) -> Result<String>
where
    R: AsyncBufReadExt + Unpin,
{
    read_optional_line(reader, context)
        .await?
        .ok_or_else(|| anyhow!("socket closed without an expected rpc payload"))
}

fn decode_response_line<S>(line: &str) -> Result<S>
where
    S: DeserializeOwned,
{
    let line = line.trim();
    let envelope: Envelope<S> =
        serde_json::from_str(line).context("failed to decode rpc response")?;
    if !envelope.ok {
        return Err(anyhow!(
            envelope
                .error
                .unwrap_or_else(|| "rpc request failed".to_string())
        ));
    }
    envelope
        .response
        .ok_or_else(|| anyhow!("rpc response did not include a payload"))
}

fn decode_stream_line<S>(line: &str) -> Result<S>
where
    S: DeserializeOwned,
{
    serde_json::from_str(line.trim()).context("failed to decode rpc stream item")
}

pub(crate) async fn write_payload<W>(writer: &mut W, payload: &str) -> Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    writer
        .write_all(payload.as_bytes())
        .await
        .context("failed to write rpc payload")?;
    writer
        .write_all(b"\n")
        .await
        .context("failed to terminate rpc payload")?;
    writer
        .flush()
        .await
        .context("failed to flush rpc payload")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde::{Deserialize, Serialize};

    use crate::{RpcReply, RpcServer};

    use super::*;

    #[derive(Debug, Serialize, Deserialize)]
    struct PingRequest {
        value: String,
    }

    #[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
    struct PingResponse {
        value: String,
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn request_round_trips_with_config_resolved_socket() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, "[rpc]\nsocket_dir = \"./sockets\"\n")?;
        let server = RpcServer::new(&config_path, RpcRoute::ToManager)?;
        let task = tokio::spawn(async move {
            let (request, mut reply): (PingRequest, RpcReply) = server.accept().await?;
            reply
                .send_ok(&PingResponse {
                    value: format!("echo:{}", request.value),
                })
                .await
        });

        let client = RpcClient::new(&config_path, RpcRoute::ToManager)?;
        let response: PingResponse = client
            .request(&PingRequest {
                value: "ok".to_string(),
            })
            .await?;
        assert_eq!(
            response,
            PingResponse {
                value: "echo:ok".to_string()
            }
        );
        task.await??;
        Ok(())
    }
}
