use anyhow::Result;
use serde_json::json;

use crate::registry::Tool;
use holmes_core::{FunctionDefinition, ToolDefinition};

pub struct ReportProgressTool;

#[async_trait::async_trait]
impl Tool for ReportProgressTool {
    fn name(&self) -> &str {
        "report_progress"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            tool_type: "function".into(),
            function: FunctionDefinition {
                name: "report_progress".into(),
                description: "Report a meaningful observation or progress signal. Call this whenever you observe something that changes your understanding of the target — a differential response, a new attack surface, a bypass confirmation, a state change. This tells the system you are making progress so it won't interrupt your work.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "description": {
                            "type": "string",
                            "description": "What you observed and why it matters (e.g. 'admin username exists — login returns password error instead of username error', 'SQLi confirmed — UNION SELECT changes response length')"
                        }
                    },
                    "required": ["description"]
                }),
            },
        }
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, args: &str) -> Result<String> {
        let v: serde_json::Value = serde_json::from_str(args)?;
        let desc = v["description"].as_str().unwrap_or("");
        if desc.is_empty() {
            anyhow::bail!("description is required");
        }
        Ok(format!("进展已记录: {}", desc))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn execute_returns_confirmation() {
        let tool = ReportProgressTool;
        let args = json!({"description": "admin user exists"}).to_string();
        let result = tool.execute(&args).await.unwrap();
        assert!(result.contains("进展已记录"));
        assert!(result.contains("admin user exists"));
    }

    #[tokio::test]
    async fn execute_rejects_empty_description() {
        let tool = ReportProgressTool;
        let args = json!({"description": ""}).to_string();
        assert!(tool.execute(&args).await.is_err());
    }

    #[tokio::test]
    async fn execute_rejects_invalid_json() {
        let tool = ReportProgressTool;
        assert!(tool.execute("not json").await.is_err());
    }
}
