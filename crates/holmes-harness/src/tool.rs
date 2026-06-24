use anyhow::{bail, Result};
use async_trait::async_trait;
use holmes_core::tool_types::{FunctionDefinition, ToolDefinition};
use holmes_tools::Tool;
use serde_json::json;

use crate::scenario::HarnessTool;

#[derive(Debug, Clone)]
pub struct HarnessMockTool {
    name: String,
    description: String,
    output: String,
    read_only: bool,
    fail: bool,
}

impl HarnessMockTool {
    pub fn from_config(config: HarnessTool) -> Self {
        Self {
            description: config
                .description
                .unwrap_or_else(|| format!("Deterministic harness tool {}", config.name)),
            name: config.name,
            output: config.output,
            read_only: config.read_only,
            fail: config.fail,
        }
    }
}

#[async_trait]
impl Tool for HarnessMockTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            tool_type: "function".into(),
            function: FunctionDefinition {
                name: self.name.clone(),
                description: self.description.clone(),
                parameters: json!({
                    "type": "object",
                    "additionalProperties": true
                }),
            },
        }
    }

    fn is_read_only(&self) -> bool {
        self.read_only
    }

    async fn execute(&self, _args: &str) -> Result<String> {
        if self.fail {
            bail!(self.output.clone());
        }
        Ok(self.output.clone())
    }
}
