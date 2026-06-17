use crate::traits::PostGuard;
use holmes_core::state::AttackState;
use holmes_core::{ToolCall, ToolResult};
use tracing::warn;

pub struct FailureTracker;

#[async_trait::async_trait]
impl PostGuard for FailureTracker {
    fn name(&self) -> &str {
        "failure_tracker"
    }

    async fn process(&mut self, _call: &ToolCall, result: &ToolResult, state: &mut AttackState) {
        if result.is_error {
            state.consecutive_failures += 1;
            if state.consecutive_failures >= 5 {
                warn!(
                    failures = state.consecutive_failures,
                    "consecutive failure threshold reached"
                );
            }
        } else {
            state.consecutive_failures = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_state() -> AttackState {
        AttackState::new(
            "http://t:80".into(),
            "10.0.0.1".into(),
            "c".into(),
            "t".into(),
            vec![],
        )
    }

    fn make_call() -> ToolCall {
        ToolCall {
            id: "1".into(),
            call_type: "function".into(),
            function: holmes_core::FunctionCall {
                name: "cmd".into(),
                arguments: "{}".into(),
            },
        }
    }

    #[tokio::test]
    async fn increments_on_error() {
        let mut guard = FailureTracker;
        let mut state = make_state();
        let err = ToolResult::error("1", "cmd", "failed");
        guard.process(&make_call(), &err, &mut state).await;
        guard.process(&make_call(), &err, &mut state).await;
        assert_eq!(state.consecutive_failures, 2);
    }

    #[tokio::test]
    async fn resets_on_success() {
        let mut guard = FailureTracker;
        let mut state = make_state();
        state.consecutive_failures = 3;
        let ok = ToolResult::success("1", "cmd", "ok");
        guard.process(&make_call(), &ok, &mut state).await;
        assert_eq!(state.consecutive_failures, 0);
    }
}
