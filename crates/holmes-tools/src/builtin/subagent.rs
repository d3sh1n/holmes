use anyhow::Result;
use async_trait::async_trait;
use holmes_core::subagent::SubagentRunner;
use holmes_core::{FunctionDefinition, ToolDefinition};
use std::sync::Arc;
use crate::registry::Tool;

pub struct SpawnSubagentTool {
    runner: Arc<dyn SubagentRunner>,
}

impl SpawnSubagentTool {
    pub fn new(runner: Arc<dyn SubagentRunner>) -> Self {
        Self { runner }
    }
}

#[async_trait]
impl Tool for SpawnSubagentTool {
    fn name(&self) -> &str {
        "spawn_subagent"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "spawn_subagent".to_string(),
                description: "Spawn an isolated subagent to perform a complex, multi-step task. Use this when a task is too complex, requires multiple file lookups, or causes context bloat for the main agent. Blocks until subagent completes.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "task": {
                            "type": "string",
                            "description": "A clear, actionable task description for the subagent."
                        },
                        "context_summary": {
                            "type": "object",
                            "description": "Key information the subagent needs to know to start."
                        },
                        "expected_output": {
                            "type": "object",
                            "properties": {
                                "schema": { "type": "string" },
                                "required_fields": {
                                    "type": "array",
                                    "items": { "type": "string" }
                                }
                            },
                            "required": ["schema", "required_fields"]
                        },
                        "constraints": {
                            "type": "object",
                            "properties": {
                                "max_turns": { "type": "integer", "description": "Maximum allowed turns before aborting." },
                                "tools_allowlist": { "type": "array", "items": { "type": "string" } },
                                "isolation": { "type": "string" }
                            },
                            "required": ["max_turns", "tools_allowlist"]
                        }
                    },
                    "required": ["task", "context_summary", "expected_output", "constraints"]
                }),
            },
        }
    }

    fn is_read_only(&self) -> bool {
        // Technically it might spawn a mutating subagent, but the tool itself is an orchestrator.
        false
    }

    async fn execute(&self, args: &str) -> Result<String> {
        let result = self.runner.run_subagent(args).await.map_err(|e| anyhow::anyhow!(e))?;
        Ok(serde_json::to_string_pretty(&result)?)
    }
}
