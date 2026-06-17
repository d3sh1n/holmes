use anyhow::Result;
use holmes_core::{ToolCall, ToolDefinition, ToolResult};
use std::collections::HashMap;

#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn definition(&self) -> ToolDefinition;
    fn is_read_only(&self) -> bool;
    async fn execute(&self, args: &str) -> Result<String>;
}

pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|t| t.definition()).collect()
    }

    pub async fn execute(&self, call: &ToolCall) -> ToolResult {
        match self.tools.get(&call.function.name) {
            Some(tool) => match tool.execute(&call.function.arguments).await {
                Ok(output) => ToolResult::success(&call.id, &call.function.name, output),
                Err(e) => ToolResult::error(&call.id, &call.function.name, e.to_string()),
            },
            None => ToolResult::error(
                &call.id,
                &call.function.name,
                format!("unknown tool: {}", call.function.name),
            ),
        }
    }

    pub fn can_parallelize(&self, calls: &[ToolCall]) -> bool {
        calls.iter().all(|c| {
            self.tools
                .get(&c.function.name)
                .map_or(false, |t| t.is_read_only())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holmes_core::{FunctionCall, ToolCall};

    struct MockTool {
        read_only: bool,
    }

    #[async_trait::async_trait]
    impl Tool for MockTool {
        fn name(&self) -> &str {
            "mock"
        }
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                tool_type: "function".into(),
                function: holmes_core::FunctionDefinition {
                    name: "mock".into(),
                    description: "mock tool".into(),
                    parameters: serde_json::json!({}),
                },
            }
        }
        fn is_read_only(&self) -> bool {
            self.read_only
        }
        async fn execute(&self, _args: &str) -> Result<String> {
            Ok("mock result".into())
        }
    }

    fn make_call(name: &str) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: name.into(),
                arguments: "{}".into(),
            },
        }
    }

    #[tokio::test]
    async fn execute_known_tool() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(MockTool { read_only: true }));
        let result = reg.execute(&make_call("mock")).await;
        assert!(!result.is_error);
        assert_eq!(result.text_content(), "mock result");
    }

    #[tokio::test]
    async fn execute_unknown_tool() {
        let reg = ToolRegistry::new();
        let result = reg.execute(&make_call("nonexistent")).await;
        assert!(result.is_error);
        assert!(result.text_content().contains("unknown tool"));
    }

    #[test]
    fn can_parallelize_all_read_only() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(MockTool { read_only: true }));
        assert!(reg.can_parallelize(&[make_call("mock"), make_call("mock")]));
    }

    #[test]
    fn cannot_parallelize_with_write_tool() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(MockTool { read_only: false }));
        assert!(!reg.can_parallelize(&[make_call("mock")]));
    }
}
