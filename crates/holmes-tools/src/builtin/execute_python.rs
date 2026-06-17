use anyhow::Result;
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;
use tokio::process::Command;
use tracing::debug;

use crate::registry::Tool;
use holmes_core::{FunctionDefinition, ToolDefinition};

const TIMEOUT_SECS: u64 = 60;
const OUTPUT_LIMIT: usize = 32768;

const AUTO_IMPORTS: &str = r#"
import sys, os, re, json, base64, hashlib, urllib.parse, subprocess
from pathlib import Path
try:
    import requests
except ImportError:
    pass
"#;

pub struct ExecutePythonTool;

#[derive(Deserialize)]
struct Args {
    code: String,
    #[serde(default = "default_timeout")]
    timeout: u64,
}

fn default_timeout() -> u64 {
    TIMEOUT_SECS
}

#[async_trait::async_trait]
impl Tool for ExecutePythonTool {
    fn name(&self) -> &str {
        "execute_python"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            tool_type: "function".into(),
            function: FunctionDefinition {
                name: "execute_python".into(),
                description: "Execute Python code. Common libs auto-imported (requests, re, json, base64, hashlib, etc).".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "code": { "type": "string", "description": "Python code to execute" },
                        "timeout": { "type": "integer", "description": "Timeout in seconds (default 60)" }
                    },
                    "required": ["code"]
                }),
            },
        }
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn execute(&self, args: &str) -> Result<String> {
        let parsed: Args = serde_json::from_str(args)?;
        debug!(code_len = parsed.code.len(), "executing python");

        let full_code = format!("{}\n{}", AUTO_IMPORTS, parsed.code);
        let tmp = std::env::temp_dir().join(format!("apeiron_{}.py", std::process::id()));
        tokio::fs::write(&tmp, &full_code).await?;

        let timeout = Duration::from_secs(parsed.timeout.min(300));
        let result = tokio::time::timeout(timeout, async {
            Command::new("python3").arg(&tmp).output().await
        })
        .await;

        let _ = tokio::fs::remove_file(&tmp).await;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                Ok(json!({
                    "stdout": truncate(&stdout, OUTPUT_LIMIT),
                    "stderr": truncate(&stderr, OUTPUT_LIMIT / 4),
                })
                .to_string())
            }
            Ok(Err(e)) => Ok(json!({
                "stdout": "",
                "stderr": format!("execution error: {e}"),
            })
            .to_string()),
            Err(_) => Ok(json!({
                "stdout": "",
                "stderr": format!("python timed out after {}s", parsed.timeout),
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
