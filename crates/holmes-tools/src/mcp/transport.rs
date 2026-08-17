use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tracing::debug;

use super::protocol::{JsonRpcRequest, JsonRpcResponse};

/// Default request timeout when a caller does not supply one (matches the
/// `mcp_request_timeout_ms` config default).
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub enum McpTransport {
    Stdio(StdioTransport),
    Http(HttpTransport),
}

impl McpTransport {
    pub async fn send(
        &mut self,
        request: &JsonRpcRequest,
        timeout: Duration,
    ) -> Result<JsonRpcResponse> {
        match self {
            Self::Stdio(t) => t.send(request, timeout).await,
            Self::Http(t) => t.send(request).await,
        }
    }
}

pub struct StdioTransport {
    child: Child,
    reader: BufReader<tokio::process::ChildStdout>,
    /// Set to `false` after a timeout or protocol error kills the server;
    /// every later send fails fast instead of hanging on — or reading stale
    /// bytes from — a dead transport (AGT-002: a wedged MCP server must never
    /// take the turn down with it again; P1-14: a desynchronised stream must
    /// never be reused).
    alive: bool,
}

impl StdioTransport {
    pub async fn spawn(command: &str, args: &[String]) -> Result<Self> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning MCP server: {command}"))?;

        let stdout = child.stdout.take().context("no stdout from MCP server")?;
        let reader = BufReader::new(stdout);

        Ok(Self {
            child,
            reader,
            alive: true,
        })
    }

    pub fn is_alive(&self) -> bool {
        self.alive
    }

    pub async fn send(
        &mut self,
        request: &JsonRpcRequest,
        timeout: Duration,
    ) -> Result<JsonRpcResponse> {
        if !self.alive {
            anyhow::bail!(
                "MCP stdio transport was terminated after a prior timeout; refusing request '{}'",
                request.method
            );
        }
        let started = std::time::Instant::now();
        let mut stdin = self.child.stdin.take().context("no stdin")?;

        /// P1-01: if the send future is dropped mid-flight (outer cancellation), the
        /// request/response pairing on the pipe is broken — a later call could read
        /// the previous call's late response. Kill the server and mark the transport
        /// dead on drop so it is never reused in that state; the provider layer
        /// respawns a fresh transport for the next call.
        struct AbortGuard<'a> {
            child: &'a mut Child,
            alive: &'a mut bool,
            armed: bool,
        }
        impl Drop for AbortGuard<'_> {
            fn drop(&mut self) {
                if self.armed {
                    kill_child(self.child);
                    *self.alive = false;
                }
            }
        }
        let mut guard = AbortGuard {
            child: &mut self.child,
            alive: &mut self.alive,
            armed: true,
        };

        let reader = &mut self.reader;
        let io = async {
            let json = serde_json::to_string(request)?;
            debug!(json_len = json.len(), "MCP stdio send");
            stdin.write_all(json.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await?;

            let mut line = String::new();
            reader.read_line(&mut line).await?;
            let resp: JsonRpcResponse = serde_json::from_str(line.trim()).with_context(|| {
                format!(
                    "parsing MCP response: {}",
                    holmes_core::truncate_str(line.trim(), 200)
                )
            })?;
            Ok::<_, anyhow::Error>(resp)
        };

        match tokio::time::timeout(timeout, io).await {
            Ok(Ok(response)) if response.id == request.id => {
                // Completed cleanly AND the response answers THIS request: the
                // pipe is in sync — disarm and restore stdin (P1-14).
                guard.armed = false;
                drop(guard);
                self.child.stdin = Some(stdin);
                Ok(response)
            }
            Ok(Ok(response)) => {
                // A syntactically valid response with the WRONG id means the
                // stream is desynchronised (a late response to an earlier
                // request, or a broken server). Binding it to this call would
                // attach tool evidence to the wrong call, and keeping the
                // transport alive would let the NEXT call read this call's
                // late response — kill and mark dead (the armed guard's Drop),
                // the provider layer respawns a fresh transport (P1-14).
                let response_id = response.id;
                drop(guard);
                anyhow::bail!(
                    "MCP response id {} does not match request id {} for '{}'; transport terminated",
                    response_id,
                    request.id,
                    request.method
                )
            }
            Ok(Err(error)) => {
                // EOF, write failure or unparseable line: the stream's framing
                // can no longer be trusted (a late valid line may still be
                // buffered behind it), so this transport must not be reused —
                // kill and mark dead via the armed guard (P1-14).
                drop(guard);
                Err(error.context(format!(
                    "MCP stdio request '{}' failed; transport terminated",
                    request.method
                )))
            }
            Err(_) => {
                // The server wedged: kill it and fail fast forever after (the armed
                // guard's Drop does exactly that). Without the kill the server would
                // linger forever; without the alive flag the next call would block
                // again on a half-consumed stream.
                drop(guard);
                tracing::warn!(
                    event = "ToolDeadlineExceeded",
                    tool = %format!("mcp:{}", request.method),
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    deadline_ms = timeout.as_millis() as u64,
                    "MCP stdio request exceeded its deadline; transport terminated"
                );
                anyhow::bail!(
                    "MCP stdio request '{}' exceeded its deadline of {}ms; transport terminated",
                    request.method,
                    timeout.as_millis()
                )
            }
        }
    }
}

/// Kill an MCP server process (its process group on Unix, so forked grandchildren
/// die with it) — used by both the explicit terminate path and the drop guard.
fn kill_child(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // Safety: killpg on an already-exited group leader is a no-op (ESRCH).
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
        }
    }
    let _ = child.start_kill();
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
    /// Build a client with explicit connect / read / total timeouts (AGT-002: the
    /// default `reqwest::Client::new()` has no total deadline at all).
    pub fn new(url: String, connect: Duration, read: Duration, total: Duration) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(connect)
            .read_timeout(read)
            .timeout(total)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { url, client }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake MCP server that echoes one canned JSON-RPC response per input line.
    const ECHO_SERVER: &str =
        r#"while IFS= read -r line; do echo '{"jsonrpc":"2.0","id":1,"result":{}}'; done"#;

    #[tokio::test]
    async fn stdio_round_trip_within_deadline() {
        let mut transport = StdioTransport::spawn("sh", &["-c".into(), ECHO_SERVER.into()])
            .await
            .expect("spawn echo server");
        let resp = transport
            .send(&JsonRpcRequest::tools_list(1), Duration::from_secs(5))
            .await
            .expect("response within deadline");
        assert!(resp.result.is_some());
        assert!(transport.is_alive());
    }

    /// P1-14: a server that emits an unparseable line first and a valid (late)
    /// response after must lose the transport on the FIRST error — the late
    /// response must never be mistaken for the next request's answer.
    #[tokio::test]
    async fn stdio_invalid_line_terminates_transport_before_late_valid_response() {
        let script = r#"IFS= read -r line; echo 'this is not json'; while IFS= read -r line; do echo '{"jsonrpc":"2.0","id":1,"result":{}}'; done"#;
        let mut transport = StdioTransport::spawn("sh", &["-c".into(), script.into()])
            .await
            .expect("spawn garbage-then-valid server");

        let err = transport
            .send(&JsonRpcRequest::tools_list(1), Duration::from_secs(5))
            .await
            .expect_err("unparseable line must fail the request");
        assert!(err.to_string().contains("terminated"), "got: {err}");
        assert!(
            !transport.is_alive(),
            "protocol error must mark the transport dead"
        );

        // The late valid line the server would still emit must never be read:
        // a dead transport refuses new requests instead of reusing the stream.
        let start = std::time::Instant::now();
        let err = transport
            .send(&JsonRpcRequest::tools_list(2), Duration::from_secs(60))
            .await
            .expect_err("dead transport refuses new requests");
        assert!(start.elapsed() < Duration::from_millis(500));
        assert!(err.to_string().contains("terminated"), "got: {err}");
    }

    /// P1-14: a response whose id does not match the request id terminates the
    /// transport instead of being bound to the wrong call.
    #[tokio::test]
    async fn stdio_wrong_response_id_terminates_transport() {
        let script =
            r#"while IFS= read -r line; do echo '{"jsonrpc":"2.0","id":999,"result":{}}'; done"#;
        let mut transport = StdioTransport::spawn("sh", &["-c".into(), script.into()])
            .await
            .expect("spawn wrong-id server");

        let err = transport
            .send(&JsonRpcRequest::tools_list(1), Duration::from_secs(5))
            .await
            .expect_err("mismatched response id must fail the request");
        let msg = err.to_string();
        assert!(msg.contains("does not match"), "got: {msg}");
        assert!(msg.contains("999"), "got: {msg}");
        assert!(!transport.is_alive(), "id mismatch kills the transport");

        let start = std::time::Instant::now();
        let err = transport
            .send(&JsonRpcRequest::tools_list(2), Duration::from_secs(60))
            .await
            .expect_err("dead transport refuses new requests");
        assert!(start.elapsed() < Duration::from_millis(500));
        assert!(err.to_string().contains("terminated"), "got: {err}");
    }

    #[tokio::test]
    async fn stdio_timeout_terminates_transport_and_fails_fast_after() {
        // Server accepts stdin but never writes a response: the request deadline must
        // fire, kill the server, and every later call must fail fast instead of
        // hanging again (AGT-002 acceptance: MCP never returns → agent regains
        // control within the configured time).
        let mut transport = StdioTransport::spawn("sh", &["-c".into(), "sleep 300".into()])
            .await
            .expect("spawn wedged server");

        let start = std::time::Instant::now();
        let err = transport
            .send(&JsonRpcRequest::tools_list(1), Duration::from_millis(200))
            .await
            .expect_err("wedged server must time out");
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "agent regained control at deadline, took {elapsed:?}"
        );
        let msg = err.to_string();
        assert!(msg.contains("deadline"), "got: {msg}");
        assert!(msg.contains("terminated"), "got: {msg}");
        assert!(!transport.is_alive(), "transport marked dead after timeout");

        // Fail fast: the next send does not block on the dead transport.
        let start = std::time::Instant::now();
        let err = transport
            .send(&JsonRpcRequest::tools_list(2), Duration::from_secs(60))
            .await
            .expect_err("dead transport refuses new requests");
        assert!(start.elapsed() < Duration::from_millis(500));
        assert!(err.to_string().contains("terminated"), "got: {err}");
    }

    #[tokio::test]
    async fn stdio_dropped_send_marks_transport_dead_and_kills_server() {
        // P1-01: outer cancellation drops the send future mid-flight. The drop guard
        // must kill the server and mark the transport non-reusable, so the next call
        // can never read this call's late/stale response.
        let mut transport = StdioTransport::spawn("sh", &["-c".into(), "sleep 300".into()])
            .await
            .expect("spawn wedged server");

        let request = JsonRpcRequest::tools_list(1);
        {
            let send = transport.send(&request, Duration::from_secs(60));
            tokio::pin!(send);
            tokio::select! {
                _ = &mut send => panic!("wedged server must not answer"),
                _ = tokio::time::sleep(Duration::from_millis(150)) => {}
            }
        } // the pinned send future is dropped here — the drop guard fires

        assert!(
            !transport.is_alive(),
            "dropped send must mark the transport dead"
        );
        let start = std::time::Instant::now();
        let err = transport
            .send(&JsonRpcRequest::tools_list(2), Duration::from_secs(60))
            .await
            .expect_err("dead transport refuses new requests");
        assert!(start.elapsed() < Duration::from_millis(500));
        assert!(err.to_string().contains("terminated"), "got: {err}");
    }

    #[tokio::test]
    async fn stdio_half_packet_response_times_out_and_terminates() {
        // P1-01 acceptance: a server that writes a partial line (no newline) and then
        // hangs must be bounded by the request deadline — the read never completes, the
        // transport is terminated, and later calls fail fast.
        let mut transport = StdioTransport::spawn(
            "sh",
            &[
                "-c".into(),
                r#"printf '{"jsonrpc":"2.0"'; sleep 300"#.into(),
            ],
        )
        .await
        .expect("spawn half-packet server");

        let start = std::time::Instant::now();
        let err = transport
            .send(&JsonRpcRequest::tools_list(1), Duration::from_millis(250))
            .await
            .expect_err("half-packet response must time out");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "bounded by deadline, took {:?}",
            start.elapsed()
        );
        assert!(err.to_string().contains("terminated"), "got: {err}");
        assert!(!transport.is_alive());
    }

    #[tokio::test]
    async fn http_total_timeout_bounds_a_server_that_never_responds() {
        // A TCP peer that accepts and then goes silent: connect succeeds, the total
        // request timeout must bound the read.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                std::mem::forget(socket); // hold the connection open, never answer
            }
        });

        let transport = HttpTransport::new(
            format!("http://{addr}/mcp"),
            Duration::from_millis(500),
            Duration::from_millis(300),
            Duration::from_millis(300),
        );
        let start = std::time::Instant::now();
        let result = transport.send(&JsonRpcRequest::tools_list(1)).await;
        let elapsed = start.elapsed();
        assert!(result.is_err(), "silent server must error out");
        assert!(
            elapsed < Duration::from_secs(3),
            "bounded by total timeout, took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn http_refused_connection_errors_promptly() {
        // Nothing listens here: the request fails fast (no retry loop, no hang).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let transport = HttpTransport::new(
            format!("http://{addr}/mcp"),
            Duration::from_millis(300),
            Duration::from_millis(300),
            Duration::from_secs(30),
        );
        let start = std::time::Instant::now();
        let result = transport.send(&JsonRpcRequest::tools_list(1)).await;
        assert!(result.is_err());
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
