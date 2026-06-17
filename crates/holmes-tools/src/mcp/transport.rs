use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tracing::debug;

use super::protocol::{JsonRpcRequest, JsonRpcResponse};

pub enum McpTransport {
    Stdio(StdioTransport),
    Http(HttpTransport),
}

impl McpTransport {
    pub async fn send(&mut self, request: &JsonRpcRequest) -> Result<JsonRpcResponse> {
        match self {
            Self::Stdio(t) => t.send(request).await,
            Self::Http(t) => t.send(request).await,
        }
    }
}

pub struct StdioTransport {
    child: Child,
    reader: BufReader<tokio::process::ChildStdout>,
}

impl StdioTransport {
    pub async fn spawn(command: &str, args: &[String]) -> Result<Self> {
        let mut child = Command::new(command)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .with_context(|| format!("spawning MCP server: {command}"))?;

        let stdout = child.stdout.take().context("no stdout from MCP server")?;
        let reader = BufReader::new(stdout);

        Ok(Self { child, reader })
    }

    pub async fn send(&mut self, request: &JsonRpcRequest) -> Result<JsonRpcResponse> {
        let stdin = self.child.stdin.as_mut().context("no stdin")?;
        let json = serde_json::to_string(request)?;
        debug!(json_len = json.len(), "MCP stdio send");
        stdin.write_all(json.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;

        let mut line = String::new();
        self.reader.read_line(&mut line).await?;
        let resp: JsonRpcResponse = serde_json::from_str(line.trim()).with_context(|| {
            format!(
                "parsing MCP response: {}",
                holmes_core::truncate_str(line.trim(), 200)
            )
        })?;
        Ok(resp)
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

pub struct HttpTransport {
    url: String,
    client: reqwest::Client,
}

impl HttpTransport {
    pub fn new(url: String) -> Self {
        Self {
            url,
            client: reqwest::Client::new(),
        }
    }

    pub async fn send(&self, request: &JsonRpcRequest) -> Result<JsonRpcResponse> {
        debug!(url = %self.url, "MCP HTTP send");
        let resp = self
            .client
            .post(&self.url)
            .json(request)
            .send()
            .await
            .context("MCP HTTP request failed")?;

        let body = resp.text().await?;
        let parsed: JsonRpcResponse = serde_json::from_str(&body).with_context(|| {
            format!(
                "parsing MCP HTTP response: {}",
                holmes_core::truncate_str(&body, 200)
            )
        })?;
        Ok(parsed)
    }
}
