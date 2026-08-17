pub mod protocol;
pub mod transport;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use holmes_core::config::McpServerConfig;
use holmes_core::execution_context::ExecutionContext;
use holmes_core::{FunctionDefinition, ToolDefinition};
use serde_json::Value;
use tokio::sync::Mutex;
use tracing::{info, warn};

use protocol::JsonRpcRequest;
use transport::{HttpTransport, McpTransport, StdioTransport};

use crate::registry::{Tool, ToolRegistry};

pub struct McpToolProvider {
    servers: Vec<McpServer>,
    tool_to_server: HashMap<String, usize>,
}

struct McpServer {
    /// Kept so a terminated stdio transport can be restarted (P1-01): after a
    /// cancelled/timed-out call kills the server, the next call respawns it and
    /// redoes the handshake instead of reusing a broken pipe.
    cfg: McpServerConfig,
    request_timeout: Duration,
    transport: McpTransport,
    tools: Vec<ToolDefinition>,
}

impl McpToolProvider {
    pub async fn from_config(configs: &[McpServerConfig], request_timeout: Duration) -> Self {
        let mut servers = Vec::new();
        let mut tool_to_server = HashMap::new();

        for cfg in configs {
            match Self::connect_server(cfg, request_timeout).await {
                Ok(server) => {
                    let server_idx = servers.len();
                    for tool in &server.tools {
                        tool_to_server.insert(tool.function.name.clone(), server_idx);
                    }
                    info!(server = %cfg.name, tools = server.tools.len(), "MCP server connected");
                    servers.push(server);
                }
                Err(e) => {
                    warn!(server = %cfg.name, error = %e, "MCP server connection failed");
                }
            }
        }

        Self {
            servers,
            tool_to_server,
        }
    }

    async fn connect_server(cfg: &McpServerConfig, request_timeout: Duration) -> Result<McpServer> {
        use holmes_core::config::McpTransport as CfgTransport;
        let mut transport = match cfg.transport {
            CfgTransport::Stdio => {
                let cmd = cfg
                    .command
                    .as_deref()
                    .context("stdio transport requires command")?;
                let args: Vec<String> = cfg.args.clone().unwrap_or_default();
                McpTransport::Stdio(StdioTransport::spawn(cmd, &args).await?)
            }
            CfgTransport::Http => {
                let url = cfg.url.as_deref().context("http transport requires url")?;
                McpTransport::Http(HttpTransport::new(
                    url.to_string(),
                    // connect / read / total — explicit per AGT-002.
                    request_timeout.min(Duration::from_secs(10)),
                    request_timeout,
                    request_timeout,
                ))
            }
        };

        // The handshake is bounded by the same request timeout: a server that accepts
        // the connection but never answers initialize must not hang startup.
        let init_req = JsonRpcRequest::initialize(1);
        let _init_resp = transport.send(&init_req, request_timeout).await?;

        let list_req = JsonRpcRequest::tools_list(2);
        let list_resp = transport.send(&list_req, request_timeout).await?;

        let tools = Self::parse_tools_list(list_resp.result)?;

        Ok(McpServer {
            cfg: cfg.clone(),
            request_timeout,
            transport,
            tools,
        })
    }

    fn parse_tools_list(result: Option<Value>) -> Result<Vec<ToolDefinition>> {
        let result = result.context("empty tools/list result")?;
        let tools_arr = result
            .get("tools")
            .and_then(|v| v.as_array())
            .context("tools/list result missing tools array")?;

        let mut defs = Vec::new();
        for tool in tools_arr {
            let name = tool
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let desc = tool
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let params = tool
                .get("inputSchema")
                .cloned()
                .unwrap_or(serde_json::json!({}));
            defs.push(ToolDefinition {
                tool_type: "function".into(),
                function: FunctionDefinition {
                    name,
                    description: desc,
                    parameters: params,
                },
            });
        }
        Ok(defs)
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.servers.iter().flat_map(|s| s.tools.clone()).collect()
    }

    pub fn has_tool(&self, name: &str) -> bool {
        self.tool_to_server.contains_key(name)
    }

    pub async fn execute(
        &mut self,
        tool_name: &str,
        arguments: Value,
        timeout: Duration,
    ) -> Result<String> {
        let idx = *self
            .tool_to_server
            .get(tool_name)
            .context(format!("MCP tool not found: {tool_name}"))?;
        let server = &mut self.servers[idx];

        // P1-01: a stdio transport killed after a cancelled/timed-out call is never
        // reused — restart the server and redo the handshake so the next call gets a
        // fresh, in-sync pipe instead of a stale-response read.
        if matches!(&server.transport, McpTransport::Stdio(t) if !t.is_alive()) {
            info!(
                server = %server.cfg.name,
                "MCP stdio transport was terminated; restarting before the next call"
            );
            let fresh = Self::connect_server(&server.cfg, server.request_timeout)
                .await
                .context("restarting terminated MCP server")?;
            server.transport = fresh.transport;
        }

        let req = JsonRpcRequest::tools_call(3, tool_name, arguments);
        let resp = server.transport.send(&req, timeout).await?;

        if let Some(err) = resp.error {
            anyhow::bail!("MCP error {}: {}", err.code, err.message);
        }

        Ok(resp
            .result
            .map(|v| v.to_string())
            .unwrap_or_else(|| "null".into()))
    }
}

pub async fn register_mcp_tools(
    registry: &mut ToolRegistry,
    configs: &[McpServerConfig],
    request_timeout: Duration,
) -> usize {
    if configs.is_empty() {
        return 0;
    }

    let provider = McpToolProvider::from_config(configs, request_timeout).await;
    let definitions = provider.definitions();
    let provider = Arc::new(Mutex::new(provider));
    let count = definitions.len();

    for definition in definitions {
        registry.register(Box::new(McpTool {
            name: definition.function.name.clone(),
            definition,
            provider: provider.clone(),
            request_timeout,
        }));
    }

    count
}

struct McpTool {
    name: String,
    definition: ToolDefinition,
    provider: Arc<Mutex<McpToolProvider>>,
    request_timeout: Duration,
}

#[async_trait::async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn execute(&self, args: &str) -> Result<String> {
        self.execute_with_context(args, &ExecutionContext::default())
            .await
    }

    async fn execute_with_context(&self, args: &str, ctx: &ExecutionContext) -> Result<String> {
        let arguments =
            serde_json::from_str::<Value>(args).unwrap_or_else(|_| Value::String(args.into()));
        // The configured per-request timeout, further capped by the turn boundary.
        let timeout = ctx.effective_deadline(Some(self.request_timeout));
        self.provider
            .lock()
            .await
            .execute(&self.name, arguments, timeout)
            .await
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// Canned JSON-RPC response carrying a one-tool `tools/list` result; used for
    /// initialize/tools-list/tools-call alike (the provider only parses the list).
    /// `%ID%` is replaced with the request's own id — responses must echo the
    /// request id or the transport refuses them (P1-14).
    const RESP: &str = r#"{"jsonrpc":"2.0","id":%ID%,"result":{"tools":[{"name":"t","description":"d","inputSchema":{}}]}}"#;

    fn stdio_config(script: String) -> McpServerConfig {
        McpServerConfig {
            name: "stub".into(),
            transport: holmes_core::config::McpTransport::Stdio,
            command: Some("sh".into()),
            args: Some(vec!["-c".into(), script]),
            url: None,
        }
    }

    #[tokio::test]
    async fn terminated_stdio_transport_is_restarted_on_next_call() {
        // P1-01 acceptance: after a cancelled/timed-out call kills the stdio server,
        // the next call respawns it (fresh handshake) instead of reading a stale
        // response off the broken pipe. The stub answers every line EXCEPT the first
        // process's third line (the first tools/call), which hangs until killed.
        let dir = tempfile::tempdir().unwrap();
        let flag = dir.path().join("hung-once");
        let script = format!(
            r#"flag="{flag}"
n=0
while IFS= read -r line; do
  n=$((n+1))
  if [ ! -f "$flag" ] && [ "$n" -eq 3 ]; then
    touch "$flag"
    sleep 300
  fi
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  echo '{RESP}' | sed "s/%ID%/${{id:-1}}/"
done
"#,
            flag = flag.display(),
            RESP = RESP
        );
        let configs = vec![stdio_config(script)];
        let mut provider = McpToolProvider::from_config(&configs, Duration::from_millis(300)).await;
        assert!(provider.has_tool("t"), "handshake listed the stub tool");

        // First call hits the wedged third line: the deadline kills the transport.
        let start = std::time::Instant::now();
        let err = provider
            .execute("t", serde_json::json!({}), Duration::from_millis(300))
            .await
            .expect_err("wedged first call must time out");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "bounded by deadline, took {:?}",
            start.elapsed()
        );
        assert!(err.to_string().contains("terminated"), "got: {err}");

        // The next call restarts the transport: fresh process, fresh handshake,
        // and this server answers the call.
        let out = provider
            .execute("t", serde_json::json!({}), Duration::from_secs(5))
            .await
            .expect("dead transport restarted on next call");
        assert!(out.contains("tools"), "fresh response, got: {out}");
    }
}
