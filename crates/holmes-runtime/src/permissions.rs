use holmes_core::config::{PermissionConfig, PermissionMode};
use holmes_core::ToolCall;
use holmes_tools::ToolRegistry;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionDecision {
    pub allowed: bool,
    pub reason: String,
}

impl PermissionDecision {
    pub fn allow(reason: impl Into<String>) -> Self {
        Self {
            allowed: true,
            reason: reason.into(),
        }
    }

    pub fn deny(reason: impl Into<String>) -> Self {
        Self {
            allowed: false,
            reason: reason.into(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct PermissionPolicy;

impl PermissionPolicy {
    pub fn evaluate(
        &self,
        config: &PermissionConfig,
        registry: &ToolRegistry,
        call: &ToolCall,
    ) -> PermissionDecision {
        let tool_name = call.function.name.as_str();
        let read_only = registry.is_read_only(tool_name).unwrap_or(false);

        if matches_any(&config.disallowed_tools, tool_name) {
            return PermissionDecision::deny(format!(
                "tool '{tool_name}' is disallowed by Holmes permissions"
            ));
        }

        if !config.allowed_tools.is_empty() && !matches_any(&config.allowed_tools, tool_name) {
            return PermissionDecision::deny(format!(
                "tool '{tool_name}' is not in the allowed_tools policy"
            ));
        }

        match config.mode {
            PermissionMode::Plan => PermissionDecision::deny(format!(
                "permission mode 'plan' blocks tool '{tool_name}'; Holmes must continue by planning or asking Watson"
            )),
            PermissionMode::ReadOnly if !read_only => PermissionDecision::deny(format!(
                "permission mode 'read_only' blocks mutating tool '{tool_name}'"
            )),
            PermissionMode::ReadOnly => PermissionDecision::allow("read-only tool allowed"),
            PermissionMode::Default if read_only && config.auto_approve_read_only => {
                PermissionDecision::allow("read-only tool auto-approved")
            }
            PermissionMode::Default => PermissionDecision::allow("default permission policy allowed"),
            PermissionMode::AcceptEdits => PermissionDecision::allow("accept-edits permission policy allowed"),
            PermissionMode::DontAsk => PermissionDecision::allow("dont_ask permission policy allowed"),
            PermissionMode::Bypass => PermissionDecision::allow("bypass permission policy allowed"),
        }
    }
}

fn matches_any(patterns: &[String], tool_name: &str) -> bool {
    patterns
        .iter()
        .any(|pattern| matches_pattern(pattern, tool_name))
}

fn matches_pattern(pattern: &str, tool_name: &str) -> bool {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return false;
    }
    if pattern == "*" || pattern == tool_name {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return tool_name.starts_with(prefix);
    }
    if let Some(suffix) = pattern.strip_prefix('*') {
        return tool_name.ends_with(suffix);
    }
    false
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use async_trait::async_trait;
    use holmes_core::config::{PermissionConfig, PermissionMode};
    use holmes_core::{FunctionCall, FunctionDefinition, ToolCall, ToolDefinition};
    use holmes_tools::{Tool, ToolRegistry};
    use serde_json::json;

    use super::*;

    struct MockTool {
        name: &'static str,
        read_only: bool,
    }

    #[async_trait]
    impl Tool for MockTool {
        fn name(&self) -> &str {
            self.name
        }

        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                tool_type: "function".into(),
                function: FunctionDefinition {
                    name: self.name.into(),
                    description: "mock".into(),
                    parameters: json!({}),
                },
            }
        }

        fn is_read_only(&self) -> bool {
            self.read_only
        }

        async fn execute(&self, _args: &str) -> Result<String> {
            Ok("ok".into())
        }
    }

    fn registry() -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(MockTool {
            name: "read_file",
            read_only: true,
        }));
        registry.register(Box::new(MockTool {
            name: "execute_command",
            read_only: false,
        }));
        registry
    }

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: "call-1".into(),
            call_type: "tool_use".into(),
            function: FunctionCall {
                name: name.into(),
                arguments: "{}".into(),
            },
        }
    }

    #[test]
    fn plan_mode_blocks_all_tools() {
        let config = PermissionConfig {
            mode: PermissionMode::Plan,
            ..PermissionConfig::default()
        };

        let decision = PermissionPolicy.evaluate(&config, &registry(), &call("read_file"));

        assert!(!decision.allowed);
        assert!(decision.reason.contains("plan"));
    }

    #[test]
    fn read_only_mode_blocks_mutating_tools() {
        let config = PermissionConfig {
            mode: PermissionMode::ReadOnly,
            ..PermissionConfig::default()
        };

        let decision = PermissionPolicy.evaluate(&config, &registry(), &call("execute_command"));

        assert!(!decision.allowed);
        assert!(decision.reason.contains("read_only"));
    }

    #[test]
    fn read_only_mode_allows_read_only_tools() {
        let config = PermissionConfig {
            mode: PermissionMode::ReadOnly,
            ..PermissionConfig::default()
        };

        let decision = PermissionPolicy.evaluate(&config, &registry(), &call("read_file"));

        assert!(decision.allowed);
    }

    #[test]
    fn policy_lists_support_prefix_patterns() {
        let config = PermissionConfig {
            allowed_tools: vec!["read_*".into()],
            disallowed_tools: vec!["*_secret".into()],
            ..PermissionConfig::default()
        };

        let allowed = PermissionPolicy.evaluate(&config, &registry(), &call("read_file"));
        let denied = PermissionPolicy.evaluate(&config, &registry(), &call("execute_command"));

        assert!(allowed.allowed);
        assert!(!denied.allowed);
    }
}
