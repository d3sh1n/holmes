pub mod protocol;
pub mod transport;

use anyhow::{Context, Result};
use holmes_core::config::McpServerConfig;
use holmes_core::{FunctionDefinition, ToolDefinition};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
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
    transport: McpTransport,
    tools: Vec<ToolDefinition>,
}

impl McpToolProvider {
    pub async fn from_config(configs: &[McpServerConfig]) -> Self {
        let mut servers = Vec::new();
        let mut tool_to_server = HashMap::new();

        for cfg in configs {
            match Self::connect_server(cfg).await {
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

    async fn connect_server(cfg: &McpServerConfig) -> Result<McpServer> {
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
                McpTransport::Http(HttpTransport::new(url.to_string()))
            }
        };

        let init_req = JsonRpcRequest::initialize(1);
        let _init_resp = transport.send(&init_req).await?;

        let list_req = JsonRpcRequest::tools_list(2);
        let list_resp = transport.send(&list_req).await?;

        let tools = Self::parse_tools_list(list_resp.result)?;

        Ok(McpServer { transport, tools })
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

    pub async fn execute(&mut self, tool_name: &str, arguments: Value) -> Result<String> {
        let idx = *self
            .tool_to_server
            .get(tool_name)
            .context(format!("MCP tool not found: {tool_name}"))?;
        let server = &mut self.servers[idx];

        let req = JsonRpcRequest::tools_call(3, tool_name, arguments);
        let resp = server.transport.send(&req).await?;

        if let Some(err) = resp.error {
            anyhow::bail!("MCP error {}: {}", err.code, err.message);
        }

        Ok(resp
            .result
            .map(|v| v.to_string())
            .unwrap_or_else(|| "null".into()))
    }
}

pub async fn register_mcp_tools(registry: &mut ToolRegistry, configs: &[McpServerConfig]) -> usize {
    if configs.is_empty() {
        return 0;
    }

    let provider = McpToolProvider::from_config(configs).await;
    let definitions = provider.definitions();
    let provider = Arc::new(Mutex::new(provider));
    let count = definitions.len();

    for definition in definitions {
        registry.register(Box::new(McpTool {
            name: definition.function.name.clone(),
            definition,
            provider: provider.clone(),
        }));
    }

    count
}

struct McpTool {
    name: String,
    definition: ToolDefinition,
    provider: Arc<Mutex<McpToolProvider>>,
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
        let arguments =
            serde_json::from_str::<Value>(args).unwrap_or_else(|_| Value::String(args.into()));
        self.provider
            .lock()
            .await
            .execute(&self.name, arguments)
            .await
    }
}
