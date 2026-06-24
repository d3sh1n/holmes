use async_trait::async_trait;
use crate::types::SubAgentResult;

#[async_trait]
pub trait SubagentRunner: Send + Sync {
    /// Spawn and run a subagent to completion.
    /// `args` is a JSON string matching SubAgentTask schema.
    async fn run_subagent(&self, args: &str) -> Result<SubAgentResult, String>;
}
