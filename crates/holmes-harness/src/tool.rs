use std::sync::Mutex;

use anyhow::{bail, Result};
use async_trait::async_trait;
use holmes_core::tool_types::{FunctionDefinition, ToolDefinition};
use holmes_tools::Tool;
use serde_json::json;

use crate::scenario::HarnessTool;

#[derive(Debug)]
pub struct HarnessMockTool {
    name: String,
    description: String,
    output: String,
    read_only: bool,
    fail: bool,
    /// Remaining forced failures (`fail_times` from the scenario); 0 → succeed.
    remaining_failures: Mutex<u32>,
    /// Artificial latency (`delay_ms` from the scenario) for deadline injection.
    delay_ms: Option<u64>,
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
            remaining_failures: Mutex::new(config.fail_times.unwrap_or(0)),
            delay_ms: config.delay_ms,
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
        // Artificial latency first: a hung tool must be observable by the outer
        // bounded race regardless of its fail/success disposition.
        if let Some(delay_ms) = self.delay_ms {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        }
        if self.fail {
            bail!(self.output.clone());
        }
        {
            let mut remaining = self
                .remaining_failures
                .lock()
                .map_err(|_| anyhow::anyhow!("harness mock tool failure counter is poisoned"))?;
            if *remaining > 0 {
                *remaining -= 1;
                drop(remaining);
                bail!(self.output.clone());
            }
        }
        Ok(self.output.clone())
    }
}
