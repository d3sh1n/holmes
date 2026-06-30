use anyhow::{anyhow, Result};
use holmes_core::config::BrowserConfig;
use holmes_core::{ContentBlock, FunctionDefinition, ToolDefinition};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Child;
use tokio::sync::Mutex;
use tracing::{debug, info};

use crate::registry::Tool;

const VALID_ACTIONS: &[&str] = &[
    "navigate",
    "click",
    "fill",
    "screenshot",
    "get_content",
    "execute_js",
    "create_context",
    "close_context",
];

pub struct BrowserManager {
    config: BrowserConfig,
    output_dir: PathBuf,
    process: Option<Child>,
    stdin: Option<tokio::io::BufWriter<tokio::process::ChildStdin>>,
    stdout: Option<BufReader<tokio::process::ChildStdout>>,
    request_id: u64,
}

impl BrowserManager {
    pub fn new(config: BrowserConfig, output_dir: PathBuf) -> Self {
        Self {
            config,
            output_dir,
            process: None,
            stdin: None,
            stdout: None,
            request_id: 0,
        }
    }

    pub fn validate_args(args_str: &str) -> Result<serde_json::Value> {
        let args: serde_json::Value = serde_json::from_str(args_str)?;
        let action = args["action"]
            .as_str()
            .ok_or_else(|| anyhow!("missing 'action' field"))?;
        if !VALID_ACTIONS.contains(&action) {
            return Err(anyhow!(
                "unknown action '{}'. Valid: {:?}",
                action,
                VALID_ACTIONS
            ));
        }

        match action {
            "navigate" => {
                if args.get("url").and_then(|v| v.as_str()).is_none() {
                    return Err(anyhow!("navigate requires 'url'"));
                }
            }
            "click" => {
                if args.get("selector").and_then(|v| v.as_str()).is_none() {
                    return Err(anyhow!("click requires 'selector'"));
                }
            }
            "fill" => {
                if args.get("selector").and_then(|v| v.as_str()).is_none() {
                    return Err(anyhow!("fill requires 'selector'"));
                }
                if args.get("value").and_then(|v| v.as_str()).is_none() {
                    return Err(anyhow!("fill requires 'value'"));
                }
            }
            "execute_js" => {
                if args.get("code").and_then(|v| v.as_str()).is_none() {
                    return Err(anyhow!("execute_js requires 'code'"));
                }
            }
            "create_context" | "close_context" => {
                if args.get("name").and_then(|v| v.as_str()).is_none() {
                    return Err(anyhow!("{} requires 'name'", action));
                }
            }
            _ => {}
        }

        Ok(args)
    }

    async fn ensure_process(&mut self) -> Result<()> {
        if self.process.as_ref().map_or(true, |p| p.id().is_none()) {
            self.spawn_process().await?;
        }
        Ok(())
    }
    async fn spawn_process(&mut self) -> Result<()> {
        // The Node/MCP-based browser backend was removed. The whole BrowserManager
        // is rewritten onto `holmes-browser` (chromiumoxide) in a later task; this
        // stub keeps the crate compiling in the meantime and intentionally fails.
        Err(anyhow!(
            "browser tool backend is being rewritten onto holmes-browser; unavailable until then"
        ))
    }
    async fn send_request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        self.request_id += 1;
        let request = json!({
            "jsonrpc": "2.0",
            "id": self.request_id,
            "method": method,
            "params": params,
        });

        let stdin = self.stdin.as_mut().ok_or_else(|| anyhow!("no stdin"))?;
        let line = serde_json::to_string(&request)? + "\n";
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await?;

        let stdout = self.stdout.as_mut().ok_or_else(|| anyhow!("no stdout"))?;
        let mut response_line = String::new();
        let timeout = Duration::from_secs(self.config.timeout as u64);
        tokio::time::timeout(timeout, stdout.read_line(&mut response_line))
            .await
            .map_err(|_| anyhow!("MCP server response timeout after {}s", self.config.timeout))??;

        let response: serde_json::Value = serde_json::from_str(&response_line)?;

        if let Some(error) = response.get("error") {
            return Err(anyhow!("MCP error: {}", error));
        }

        Ok(response["result"].clone())
    }

    async fn call_tool(&mut self, action: &str, args: &serde_json::Value) -> Result<String> {
        self.ensure_process().await?;

        let mcp_tool_name = format!("browser_{}", action);
        let result = self
            .send_request(
                "tools/call",
                json!({
                    "name": mcp_tool_name,
                    "arguments": args,
                }),
            )
            .await?;
        let text = result["content"]
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|block| block["text"].as_str())
            .unwrap_or("")
            .to_string();

        Ok(text)
    }

    fn truncate_content(&self, content: &str) -> String {
        if content.len() <= self.config.content_limit {
            content.to_string()
        } else {
            format!(
                "{}...[truncated, {} chars total, showing first {}]",
                &content[..self.config.content_limit],
                content.len(),
                self.config.content_limit,
            )
        }
    }

    async fn handle_screenshot_result(&self, raw: &str) -> Result<Vec<ContentBlock>> {
        let parsed: serde_json::Value = serde_json::from_str(raw)?;
        let base64_data = parsed["base64"].as_str().unwrap_or("");

        let screenshots_dir = self.output_dir.join("screenshots");
        std::fs::create_dir_all(&screenshots_dir)?;
        let filename = format!("{}.png", chrono::Utc::now().format("%Y%m%d-%H%M%S-%3f"));
        let filepath = screenshots_dir.join(&filename);
        let bytes =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, base64_data)?;
        std::fs::write(&filepath, &bytes)?;

        let mut blocks = vec![ContentBlock::Text(format!(
            "Screenshot saved to {}",
            filepath.display()
        ))];

        if self.config.vision {
            blocks.push(ContentBlock::Image {
                base64: base64_data.to_string(),
                media_type: "image/png".into(),
            });
        }

        Ok(blocks)
    }
    pub async fn execute_action(&mut self, args_str: &str) -> Result<Vec<ContentBlock>> {
        let args = Self::validate_args(args_str)?;
        let action = args["action"].as_str().unwrap();

        let raw_result = self.call_tool(action, &args).await?;

        if action == "screenshot" {
            return self.handle_screenshot_result(&raw_result).await;
        }

        let truncated = self.truncate_content(&raw_result);
        Ok(vec![ContentBlock::Text(truncated)])
    }

    pub async fn shutdown(&mut self) {
        if self.stdin.is_some() {
            let _ = self.send_request("shutdown", json!({})).await;
        }
        if let Some(mut child) = self.process.take() {
            let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
            let _ = child.kill().await;
        }
        self.stdin = None;
        self.stdout = None;
    }
}

pub struct BrowserTool {
    manager: Arc<Mutex<BrowserManager>>,
}

impl BrowserTool {
    pub fn new(config: BrowserConfig, output_dir: PathBuf) -> Self {
        Self {
            manager: Arc::new(Mutex::new(BrowserManager::new(config, output_dir))),
        }
    }

    pub fn manager(&self) -> Arc<Mutex<BrowserManager>> {
        self.manager.clone()
    }
}
#[async_trait::async_trait]
impl Tool for BrowserTool {
    fn name(&self) -> &str {
        "browser"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            tool_type: "function".into(),
            function: FunctionDefinition {
                name: "browser".into(),
                description: "Control a real browser for pages that require JavaScript rendering, \
                              form interaction, or visual inspection. Actions: navigate, click, \
                              fill, screenshot, get_content, execute_js, create_context, \
                              close_context."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": VALID_ACTIONS,
                            "description": "Browser action to perform"
                        },
                        "url": { "type": "string", "description": "URL for navigate" },
                        "selector": { "type": "string", "description": "CSS selector for click/fill/get_content" },
                        "value": { "type": "string", "description": "Value for fill" },
                        "code": { "type": "string", "description": "JavaScript code for execute_js" },
                        "name": { "type": "string", "description": "Context name for create_context/close_context" },
                        "context": { "type": "string", "description": "Browser context to use (default: 'default')" },
                        "full_page": { "type": "boolean", "description": "Full page screenshot (default: false)" }
                    },
                    "required": ["action"]
                }),
            },
        }
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn execute(&self, args: &str) -> Result<String> {
        let mut mgr = self.manager.lock().await;
        let blocks = mgr.execute_action(args).await?;
        let text: String = blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text(s) => Some(s.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(text)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Tool;

    #[test]
    fn browser_tool_name() {
        let config = BrowserConfig::default();
        let tool = BrowserTool::new(config, PathBuf::from("/tmp"));
        assert_eq!(tool.name(), "browser");
    }

    #[test]
    fn browser_tool_not_read_only() {
        let config = BrowserConfig::default();
        let tool = BrowserTool::new(config, PathBuf::from("/tmp"));
        assert!(!tool.is_read_only());
    }

    #[test]
    fn browser_tool_definition_has_action_param() {
        let config = BrowserConfig::default();
        let tool = BrowserTool::new(config, PathBuf::from("/tmp"));
        let def = tool.definition();
        let params = &def.function.parameters;
        assert!(params["properties"]["action"].is_object());
        assert!(params["required"]
            .as_array()
            .unwrap()
            .contains(&json!("action")));
    }

    #[test]
    fn validate_args_rejects_unknown_action() {
        let result = BrowserManager::validate_args(r#"{"action":"destroy"}"#);
        assert!(result.is_err());
    }

    #[test]
    fn validate_args_accepts_navigate() {
        let result = BrowserManager::validate_args(r#"{"action":"navigate","url":"http://x"}"#);
        assert!(result.is_ok());
    }

    #[test]
    fn validate_args_rejects_navigate_without_url() {
        let result = BrowserManager::validate_args(r#"{"action":"navigate"}"#);
        assert!(result.is_err());
    }

    #[test]
    fn validate_args_accepts_click() {
        let result = BrowserManager::validate_args(r##"{"action":"click","selector":"#btn"}"##);
        assert!(result.is_ok());
    }

    #[test]
    fn validate_args_rejects_fill_without_value() {
        let result = BrowserManager::validate_args(r##"{"action":"fill","selector":"#x"}"##);
        assert!(result.is_err());
    }
}
