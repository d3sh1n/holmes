use crate::tool_types::{ToolCall, ToolResult};

/// Lifecycle hook for agent execution phases.
/// 
/// Allows external modules to hook into the agent loop and monitor, log, 
/// or block specific phases of execution without hardcoding logic into the loop.
pub trait AgentHook: Send + Sync + std::fmt::Debug {
    /// Triggered before a tool call is executed.
    /// If an error is returned, the tool execution is aborted and the error is yielded.
    fn pre_tool_use(&self, _call: &ToolCall) -> Result<(), String> {
        Ok(())
    }

    /// Triggered after a tool call completes execution.
    fn post_tool_use(&self, _call: &ToolCall, _result: &ToolResult) -> Result<(), String> {
        Ok(())
    }

    /// Triggered before semantic compaction boundary.
    fn pre_compact(&self, _before_count: usize) -> Result<(), String> {
        Ok(())
    }
}
