use anyhow::Result;
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;
use tokio::process::Command;
use tracing::debug;

use crate::registry::Tool;
use holmes_core::{FunctionDefinition, ToolDefinition};

const TIMEOUT_SECS: u64 = 30;
const OUTPUT_LIMIT: usize = 32768;

pub struct ExecuteCommandTool;

#[derive(Deserialize)]
struct Args {
    command: String,
    #[serde(default = "default_timeout")]
    timeout: u64,
}

fn default_timeout() -> u64 {
    TIMEOUT_SECS
}

#[async_trait::async_trait]
impl Tool for ExecuteCommandTool {
    fn name(&self) -> &str {
        "execute_command"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            tool_type: "function".into(),
            function: FunctionDefinition {
                name: "execute_command".into(),
                description: "Execute a shell command and return stdout/stderr/exit_code.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "Shell command to execute" },
                        "timeout": { "type": "integer", "description": "Timeout in seconds (default 30)" }
                    },
                    "required": ["command"]
                }),
            },
        }
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn execute(&self, args: &str) -> Result<String> {
        let parsed: Args = serde_json::from_str(args)?;
        debug!(command = %parsed.command, "executing command");

        let timeout = Duration::from_secs(parsed.timeout.min(300));
        let result = tokio::time::timeout(timeout, async {
            Command::new("sh")
                .arg("-c")
                .arg(&parsed.command)
                .output()
                .await
        })
        .await;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let truncated_stdout = truncate(&stdout, OUTPUT_LIMIT);
                let truncated_stderr = truncate(&stderr, OUTPUT_LIMIT / 4);
                Ok(json!({
                    "stdout": truncated_stdout,
                    "stderr": truncated_stderr,
                    "exit_code": output.status.code().unwrap_or(-1)
                })
                .to_string())
            }
            Ok(Err(e)) => Ok(json!({
                "stdout": "",
                "stderr": format!("execution error: {e}"),
                "exit_code": -1
            })
            .to_string()),
            Err(_) => Ok(json!({
                "stdout": "",
                "stderr": format!("command timed out after {}s", parsed.timeout),
                "exit_code": -1
            })
            .to_string()),
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...[truncated, {} total]", &s[..max], s.len())
    }
}
