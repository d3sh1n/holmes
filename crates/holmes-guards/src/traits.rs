use holmes_core::state::AttackState;
use holmes_core::{GuardVerdict, ToolCall, ToolResult};

#[async_trait::async_trait]
pub trait PreGuard: Send + Sync {
    fn name(&self) -> &str;
    async fn check(&self, call: &ToolCall, state: &AttackState) -> GuardVerdict;
}

#[async_trait::async_trait]
pub trait PostGuard: Send + Sync {
    fn name(&self) -> &str;
    async fn process(&mut self, call: &ToolCall, result: &ToolResult, state: &mut AttackState);
}
