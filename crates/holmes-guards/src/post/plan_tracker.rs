use crate::traits::PostGuard;
use holmes_core::state::{AttackState, TodoItem};
use holmes_core::{ToolCall, ToolResult};
use tracing::debug;

/// Captures the agent's working task list from `write_todos` calls into `AttackState.plan`
/// (free zone), where the perception frame renders it as `[Plan]` each turn. This makes
/// a structured, always-current todo list part of the agent's cognition, mirroring the
/// TodoWrite pattern.
pub struct PlanTracker;

const VALID_STATUSES: &[&str] = &["pending", "in_progress", "completed"];

#[async_trait::async_trait]
impl PostGuard for PlanTracker {
    fn name(&self) -> &str {
        "plan_tracker"
    }

    async fn process(&mut self, call: &ToolCall, _result: &ToolResult, state: &mut AttackState) {
        if call.function.name != "write_todos" {
            return;
        }
        let Ok(args) = serde_json::from_str::<serde_json::Value>(&call.function.arguments) else {
            return;
        };
        let Some(items) = args.get("todos").and_then(|v| v.as_array()) else {
            return;
        };

        let plan: Vec<TodoItem> = items
            .iter()
            .filter_map(|item| {
                let content = item.get("content")?.as_str()?.trim().to_string();
                if content.is_empty() {
                    return None;
                }
                let status = item
                    .get("status")
                    .and_then(|s| s.as_str())
                    .map(str::trim)
                    .filter(|s| VALID_STATUSES.contains(s))
                    .unwrap_or("pending")
                    .to_string();
                Some(TodoItem { content, status })
            })
            .collect();

        debug!(items = plan.len(), "plan updated");
        // A write_todos call is the full plan for that step — replace, don't append.
        state.plan = plan;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holmes_core::FunctionCall;

    fn call(args: &str) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "write_todos".into(),
                arguments: args.into(),
            },
        }
    }

    fn state() -> AttackState {
        AttackState::new(
            "http://t".into(),
            String::new(),
            "c".into(),
            "t".into(),
            vec![],
        )
    }

    #[tokio::test]
    async fn captures_and_replaces_plan() {
        let mut guard = PlanTracker;
        let mut s = state();
        let result = ToolResult::success("c1", "write_todos", "ok");
        guard
            .process(
                &call(r#"{"todos":[{"content":"recon","status":"completed"},{"content":"exploit","status":"in_progress"}]}"#),
                &result,
                &mut s,
            )
            .await;
        assert_eq!(s.plan.len(), 2);
        assert_eq!(s.plan[0].content, "recon");
        assert_eq!(s.plan[0].status, "completed");
        assert_eq!(s.plan[1].status, "in_progress");

        // A second call replaces the whole plan.
        guard
            .process(
                &call(r#"{"todos":[{"content":"report","status":"pending"}]}"#),
                &result,
                &mut s,
            )
            .await;
        assert_eq!(s.plan.len(), 1);
        assert_eq!(s.plan[0].content, "report");
    }

    #[tokio::test]
    async fn ignores_other_tools_and_bad_status() {
        let mut guard = PlanTracker;
        let mut s = state();
        let result = ToolResult::success("c1", "x", "ok");
        // wrong tool
        let mut other = call("{}");
        other.function.name = "read_file".into();
        guard.process(&other, &result, &mut s).await;
        assert!(s.plan.is_empty());
        // invalid status falls back to pending
        guard
            .process(
                &call(r#"{"todos":[{"content":"x","status":"bogus"}]}"#),
                &result,
                &mut s,
            )
            .await;
        assert_eq!(s.plan[0].status, "pending");
    }
}
