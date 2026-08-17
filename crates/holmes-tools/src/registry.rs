use anyhow::Result;
use holmes_core::execution_context::{BoundedOutcome, ExecutionContext};
use holmes_core::{ToolCall, ToolDefinition, ToolOutcomeStatus, ToolResult};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::time::Duration;

/// Grace added on top of a tool's own effective deadline by the registry-level race.
/// Tools with internal deadlines (processes, MCP) clean up themselves at `deadline`;
/// this outer bound only fires for tools that ignore the context, so it must leave
/// room for their kill/reap path to win first.
const BOUNDED_GRACE: Duration = Duration::from_secs(3);

/// Two-class side-effect classification for a single tool call.
///
/// `is_read_only()` is static per tool, but some tools (e.g. `http_request`) only have
/// side effects for certain arguments — `effect_of` classifies the concrete call.
/// Unknown tools and unparseable arguments classify as `Mutating` (fail-closed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// No externally visible side effects for this call.
    ReadOnly,
    /// May mutate state or perform externally visible side effects.
    Mutating,
}

/// A tool-internal failure with an explicit outcome category. Tool implementations
/// return this through `anyhow::Error`; the registry downcasts it and preserves the
/// status in `ToolResult` instead of guessing from free-form JSON/text.
#[derive(Debug)]
pub struct ToolExecutionFailure {
    pub status: ToolOutcomeStatus,
    pub content: String,
}

impl ToolExecutionFailure {
    pub fn failed(content: impl Into<String>) -> Self {
        Self {
            status: ToolOutcomeStatus::Failed,
            content: content.into(),
        }
    }

    pub fn timed_out(content: impl Into<String>) -> Self {
        Self {
            status: ToolOutcomeStatus::TimedOut,
            content: content.into(),
        }
    }

    pub fn cancelled(content: impl Into<String>) -> Self {
        Self {
            status: ToolOutcomeStatus::Cancelled,
            content: content.into(),
        }
    }
}

impl fmt::Display for ToolExecutionFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.content)
    }
}

impl std::error::Error for ToolExecutionFailure {}

#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn definition(&self) -> ToolDefinition;
    fn is_read_only(&self) -> bool;
    /// Effect of the concrete call given its raw JSON arguments. Defaults to the
    /// static `is_read_only()` classification; override when the effect depends on
    /// the arguments (e.g. `http_request` maps GET/HEAD/OPTIONS to `ReadOnly`).
    fn effect_of(&self, args: &str) -> Effect {
        let _ = args;
        if self.is_read_only() {
            Effect::ReadOnly
        } else {
            Effect::Mutating
        }
    }
    async fn execute(&self, args: &str) -> Result<String>;

    /// Execute under a unified execution boundary (AGT-002). The default ignores the
    /// context (the registry's `execute_bounded` race still bounds it); tools that can
    /// honour deadlines/cancellation internally (processes, MCP, subagents) override
    /// this to clean up themselves — kill process trees, terminate transports — before
    /// the outer race would drop their future.
    async fn execute_with_context(&self, args: &str, ctx: &ExecutionContext) -> Result<String> {
        let _ = ctx;
        self.execute(args).await
    }
}

pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
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

    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// Enforce an explicit capability set. An empty set intentionally removes every
    /// executable tool; callers must never interpret it as "allow all".
    pub fn retain_allowed(&mut self, allowed: &[String]) {
        let allowed: HashSet<&str> = allowed.iter().map(String::as_str).collect();
        self.tools.retain(|name, _| allowed.contains(name.as_str()));
    }

    pub fn is_read_only(&self, name: &str) -> Option<bool> {
        self.tools.get(name).map(|tool| tool.is_read_only())
    }

    /// Effect of a concrete call. Unknown tools classify as `Mutating` (fail-closed).
    pub fn effect_of(&self, call: &ToolCall) -> Effect {
        self.tools
            .get(&call.function.name)
            .map(|tool| tool.effect_of(&call.function.arguments))
            .unwrap_or(Effect::Mutating)
    }

    pub async fn execute(&self, call: &ToolCall) -> ToolResult {
        match self.tools.get(&call.function.name) {
            Some(tool) => match tool.execute(&call.function.arguments).await {
                Ok(output) => ToolResult::success(&call.id, &call.function.name, output),
                Err(error) => execution_error_result(call, &error),
            },
            None => ToolResult::error(
                &call.id,
                &call.function.name,
                format!("unknown tool: {}", call.function.name),
            ),
        }
    }

    /// Execute a call under the unified execution boundary:
    /// - a cancelled context refuses to start the tool at all ("no new tool calls
    ///   after cancellation", AGT-002);
    /// - the tool runs inside `ExecutionContext::run_bounded`, so even a tool that
    ///   ignores the context cannot hold the turn past its deadline (+ grace) and is
    ///   interrupted on cancellation.
    pub async fn execute_bounded(&self, call: &ToolCall, ctx: &ExecutionContext) -> ToolResult {
        let Some(tool) = self.tools.get(&call.function.name) else {
            return ToolResult::error(
                &call.id,
                &call.function.name,
                format!("unknown tool: {}", call.function.name),
            );
        };
        if ctx.is_cancelled() {
            return ToolResult::cancelled(
                &call.id,
                &call.function.name,
                format!(
                    "tool '{}' was not started: turn cancelled (task {})",
                    call.function.name,
                    ctx.task_id()
                ),
            );
        }
        let deadline = ctx.effective_deadline(None);
        let future = tool.execute_with_context(&call.function.arguments, ctx);
        match ctx
            .run_bounded(&call.function.name, Some(deadline + BOUNDED_GRACE), future)
            .await
        {
            BoundedOutcome::Completed(Ok(output)) => {
                ToolResult::success(&call.id, &call.function.name, output)
            }
            BoundedOutcome::Completed(Err(error)) => execution_error_result(call, &error),
            BoundedOutcome::DeadlineExceeded => ToolResult::timed_out(
                &call.id,
                &call.function.name,
                format!(
                    "tool '{}' exceeded its deadline of {}ms (task {})",
                    call.function.name,
                    deadline.as_millis(),
                    ctx.task_id()
                ),
            ),
            BoundedOutcome::Cancelled => ToolResult::cancelled(
                &call.id,
                &call.function.name,
                format!(
                    "tool '{}' interrupted by cancellation (task {})",
                    call.function.name,
                    ctx.task_id()
                ),
            ),
        }
    }

    /// Whether a batch can run its slow `execute()` I/O concurrently. Safe when every
    /// call is either read-only for these arguments (no cross-call mutation) or
    /// `spawn_subagent` (each subagent runs in an isolated context; shared stores
    /// serialize internally). All state mutation/bookkeeping still happens
    /// sequentially in the caller.
    pub fn can_parallelize(&self, calls: &[ToolCall]) -> bool {
        calls
            .iter()
            .all(|c| c.function.name == "spawn_subagent" || self.effect_of(c) == Effect::ReadOnly)
    }
}

fn execution_error_result(call: &ToolCall, error: &anyhow::Error) -> ToolResult {
    if let Some(failure) = error.downcast_ref::<ToolExecutionFailure>() {
        return ToolResult::with_status(
            &call.id,
            &call.function.name,
            failure.status,
            failure.content.clone(),
        );
    }
    ToolResult::error(&call.id, &call.function.name, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use holmes_core::{FunctionCall, ToolCall};

    struct MockTool {
        read_only: bool,
    }

    struct TimeoutTool;

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

    #[async_trait::async_trait]
    impl Tool for TimeoutTool {
        fn name(&self) -> &str {
            "timeout"
        }

        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                tool_type: "function".into(),
                function: holmes_core::FunctionDefinition {
                    name: "timeout".into(),
                    description: "times out".into(),
                    parameters: serde_json::json!({"type": "object"}),
                },
            }
        }

        fn is_read_only(&self) -> bool {
            true
        }

        async fn execute(&self, _args: &str) -> Result<String> {
            Err(ToolExecutionFailure::timed_out("deadline").into())
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

    #[tokio::test]
    async fn execute_preserves_typed_tool_failure() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(TimeoutTool));
        let result = reg.execute(&make_call("timeout")).await;
        assert_eq!(result.status, ToolOutcomeStatus::TimedOut);
        assert!(!result.is_success());
        assert!(result.is_error);
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

    #[test]
    fn retain_allowed_enforces_empty_and_named_capability_sets() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(MockTool { read_only: true }));
        reg.retain_allowed(&[]);
        assert!(!reg.contains("mock"), "empty allowlist must allow no tools");

        let mut reg = ToolRegistry::new();
        reg.register(Box::new(MockTool { read_only: true }));
        reg.retain_allowed(&["mock".to_string()]);
        assert!(reg.contains("mock"));
    }
}
