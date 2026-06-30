use holmes_core::{Event, Message, ToolCall, ToolResult};
use holmes_core::hook::AgentHook;
use std::sync::Arc;

use crate::context::RuntimeContext;
use crate::deliberation::RuntimeError;
use crate::dialogue::DialogueEngine;
use crate::permissions::PermissionPolicy;
use crate::yield_stream::{RuntimeSink, RuntimeYield};

#[derive(Debug, Clone, Default)]
pub struct ActionEngine {
    pub hooks: Vec<Arc<dyn AgentHook>>,
}

#[derive(Debug, Clone, Default)]
pub struct ActionBatchResult {
    pub results: Vec<ToolResult>,
    pub messages: Vec<Message>,
    pub events: Vec<RuntimeYield>,
}

impl ActionEngine {
    pub fn new() -> Self {
        Self { hooks: Vec::new() }
    }

    pub async fn execute_batch(
        &self,
        context: &mut RuntimeContext,
        calls: &[ToolCall],
        sink: &mut dyn RuntimeSink,
    ) -> Result<ActionBatchResult, RuntimeError> {
        let mut batch = ActionBatchResult::default();
        let permissions = PermissionPolicy;

        for call in calls {
            let decision = permissions.evaluate(&context.config.permissions, &context.tools, call);
            let permission_event = RuntimeYield::PermissionDecision {
                tool_name: call.function.name.clone(),
                call_id: Some(call.id.clone()),
                allowed: decision.allowed,
                reason: decision.reason.clone(),
            };
            sink.emit_yield(&context.session_id, permission_event.clone());
            batch.events.push(permission_event);

            if !decision.allowed {
                record_tool_call(context, call).await?;
                record_tool_blocked(context, call, "permission", &decision.reason).await?;
                let mut result = ToolResult::blocked(&call.id, decision.reason);
                let middlewares = context.middlewares.clone();
                for mw in &middlewares {
                    mw.after_tool_call(context, &mut result).await?;
                }
                            for hook in &self.hooks {
                let _ = hook.post_tool_use(call, &result);
            }

            batch.messages.push(result.to_message_with_vision());
                let finished = DialogueEngine::tool_finished(&result);
                sink.emit_yield(&context.session_id, finished.clone());
                batch.events.push(finished);
                batch.results.push(result);
                continue;
            }

                        let mut hook_blocked = None;
            for hook in &self.hooks {
                if let Err(e) = hook.pre_tool_use(call) {
                    hook_blocked = Some(e);
                    break;
                }
            }

            if let Some(reason) = hook_blocked {
                record_tool_call(context, call).await?;
                record_tool_blocked(context, call, "hook", &reason).await?;
                let mut result = ToolResult::blocked(&call.id, reason);
                let middlewares = context.middlewares.clone();
                for mw in &middlewares {
                    mw.after_tool_call(context, &mut result).await?;
                }
                            for hook in &self.hooks {
                let _ = hook.post_tool_use(call, &result);
            }

            batch.messages.push(result.to_message_with_vision());
                let finished = DialogueEngine::tool_finished(&result);
                sink.emit_yield(&context.session_id, finished.clone());
                batch.events.push(finished);
                batch.results.push(result);
                continue;
            }

            let started = DialogueEngine::tool_started(call);
            sink.emit_yield(&context.session_id, started.clone());
            batch.events.push(started);

            record_tool_call(context, call).await?;

            let result = if !context.tools.contains(&call.function.name) {
                let mut result = context.tools.execute(call).await;
                let middlewares = context.middlewares.clone();
                for mw in &middlewares {
                    mw.after_tool_call(context, &mut result).await?;
                }
                record_tool_result(context, call, &result).await?;
                result
            } else {
                let verdict = context
                    .guards
                    .run_pre(call, &context.state.compatibility_state)
                    .await;

                if !verdict.allowed {
                    record_tool_blocked(context, call, "guard", &verdict.guidance).await?;
                    let mut result = ToolResult::blocked(&call.id, verdict.guidance);
                    let middlewares = context.middlewares.clone();
                    for mw in &middlewares {
                        mw.after_tool_call(context, &mut result).await?;
                    }
                    result
                } else {
                    let mut result = context.tools.execute(call).await;
                    let middlewares = context.middlewares.clone();
                    for mw in &middlewares {
                        mw.after_tool_call(context, &mut result).await?;
                    }
                    record_tool_result(context, call, &result).await?;

                    context
                        .guards
                        .run_post(call, &result, &mut context.state.compatibility_state)
                        .await;

                    result
                }
            };

                        for hook in &self.hooks {
                let _ = hook.post_tool_use(call, &result);
            }

            batch.messages.push(result.to_message_with_vision());
            let finished = DialogueEngine::tool_finished(&result);
            sink.emit_yield(&context.session_id, finished.clone());
            batch.events.push(finished);
            batch.results.push(result);
        }

        Ok(batch)
    }
}

async fn record_tool_call(
    context: &mut RuntimeContext,
    call: &ToolCall,
) -> Result<(), RuntimeError> {
    let arguments = call
        .args_parsed()
        .unwrap_or_else(|_| call.function.arguments.clone().into());
    let event = Event::ToolCall {
        name: call.function.name.clone(),
        arguments,
        purpose: None,
    };
    append_and_ingest(context, event).await
}

async fn record_tool_result(
    context: &mut RuntimeContext,
    call: &ToolCall,
    result: &ToolResult,
) -> Result<(), RuntimeError> {
    let text = result.text_content();
    let result_event = Event::ToolResult {
        name: call.function.name.clone(),
        success: !result.is_error,
        content: text.clone(),
        error: result.is_error.then_some(text),
        artifacts: vec![],
    };
    append_and_ingest(context, result_event).await
}

async fn record_tool_blocked(
    context: &mut RuntimeContext,
    call: &ToolCall,
    guard_name: &str,
    reason: &str,
) -> Result<(), RuntimeError> {
    let blocked_event = Event::ToolBlocked {
        tool_name: call.function.name.clone(),
        guard_name: guard_name.into(),
        reason: reason.into(),
    };
    append_and_ingest(context, blocked_event).await
}

async fn append_and_ingest(context: &mut RuntimeContext, mut event: Event) -> Result<(), RuntimeError> {
    let middlewares = context.middlewares.clone();
    for mw in &middlewares {
        mw.before_event_persist(context, &mut event).await?;
    }
    context
        .session_db
        .append_event(&context.session_id, &event)
        .await
        .map_err(|error| {
            RuntimeError::recoverable(format!(
                "failed to persist runtime event for session {}: {}",
                context.session_id, error
            ))
        })?;
    context.mind_palace.ingest(event);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use anyhow::Result;
    use async_trait::async_trait;
    use holmes_core::config::HolmesConfig;
    use holmes_core::session::RuntimeSession;
    use holmes_core::state::AttackState;
    use holmes_core::{
        FunctionCall, FunctionDefinition, GuardVerdict, LlmResponse, SessionMode, ToolDefinition,
    };
    use holmes_guards::traits::{PostGuard, PreGuard};
    use holmes_guards::GuardChain;
    use holmes_mind_palace::MindPalace;
    use holmes_session::{memory_store::MemoryStore, CreateSessionParams, SessionDB, SessionStore};
    use holmes_tools::{Tool, ToolRegistry};

    use crate::context::{RuntimeContext, RuntimeState};
    use crate::deliberation::StaticLlmBackend;
    use crate::yield_stream::VecSink;

    use super::*;

    #[tokio::test]
    async fn successful_tool_emits_yields_results_and_events() {
        let mut context = make_context(GuardChain::new()).await;
        let call = make_call("mock_tool", r#"{"target":"example.test"}"#);
        let mut sink = VecSink::new();

        let batch = ActionEngine::new()
            .execute_batch(&mut context, &[call.clone()], &mut sink)
            .await
            .expect("execute batch");

        assert_eq!(batch.results.len(), 1);
        assert_eq!(batch.messages.len(), 1);
        assert!(!batch.results[0].is_error);
        assert_eq!(batch.results[0].text_content(), "mock output");
        assert_eq!(
            batch.events,
            vec![
                RuntimeYield::PermissionDecision {
                    tool_name: "mock_tool".into(),
                    call_id: Some("call-1".into()),
                    allowed: true,
                    reason: "read-only tool auto-approved".into(),
                },
                RuntimeYield::ToolStarted {
                    name: "mock_tool".into(),
                    call_id: Some("call-1".into()),
                },
                RuntimeYield::ToolFinished {
                    name: "mock_tool".into(),
                    call_id: Some("call-1".into()),
                    success: true,
                    content: "mock output".into(),
                 error: None, usage: None },
            ]
        );
        assert_eq!(sink.yields(), batch.events);
        assert!(context.session.messages.is_empty());
        assert_eq!(batch.messages[0].tool_call_id.as_deref(), Some("call-1"));
        assert_eq!(batch.messages[0].name.as_deref(), Some("mock_tool"));
        assert_eq!(
            context.state.compatibility_state.action_history,
            vec!["post:mock_tool:false"]
        );

        let stored = context
            .session_db
            .get_events(&context.session_id)
            .await
            .expect("stored events");
        assert_eq!(stored.len(), 2);
        assert!(matches!(
            &stored[0].event,
            Event::ToolCall { name, arguments, .. }
                if name == "mock_tool" && arguments["target"] == "example.test"
        ));
        assert!(matches!(
            &stored[1].event,
            Event::ToolResult { name, success, content, .. }
                if name == "mock_tool" && *success && content == "mock output"
        ));
        assert_eq!(context.mind_palace.memory.event_count(), 2);
    }

    #[tokio::test]
    async fn blocked_tool_returns_blocked_result_and_failure_yield() {
        let mut guards = GuardChain::new();
        guards.pre.push(Box::new(BlockGuard));
        let mut context = make_context(guards).await;
        let mut sink = VecSink::new();

        let batch = ActionEngine::new()
            .execute_batch(
                &mut context,
                &[make_call("mock_tool", "not-json")],
                &mut sink,
            )
            .await
            .expect("execute batch");

        assert_eq!(batch.results.len(), 1);
        assert_eq!(batch.messages.len(), 1);
        assert!(batch.results[0].is_error);
        assert!(batch.results[0]
            .text_content()
            .contains("[GUARD] blocked by test"));
        assert_eq!(batch.events.len(), 3);
        assert!(matches!(
            batch.events[2],
            RuntimeYield::ToolFinished { success: false, .. }
        ));
        assert_eq!(sink.yields(), batch.events);
        assert!(context.state.compatibility_state.action_history.is_empty());

        let stored = context
            .session_db
            .get_events(&context.session_id)
            .await
            .expect("stored events");
        assert_eq!(stored.len(), 2);
        assert!(matches!(
            &stored[0].event,
            Event::ToolCall { arguments, .. }
                if arguments.as_str() == Some("not-json")
        ));
        assert!(matches!(
            &stored[1].event,
            Event::ToolBlocked { tool_name, reason, .. }
                if tool_name == "mock_tool" && reason == "blocked by test"
        ));
        assert_eq!(context.mind_palace.memory.event_count(), 2);
    }

    #[tokio::test]
    async fn unknown_tool_produces_error_result_and_failure_yield() {
        let mut context = make_context(GuardChain::new()).await;
        let mut sink = VecSink::new();

        let batch = ActionEngine::new()
            .execute_batch(&mut context, &[make_call("missing_tool", "{}")], &mut sink)
            .await
            .expect("execute batch");

        assert_eq!(batch.results.len(), 1);
        assert_eq!(batch.messages.len(), 1);
        assert!(batch.results[0].is_error);
        assert_eq!(batch.results[0].tool_name, "missing_tool");
        assert!(batch.results[0].text_content().contains("unknown tool"));
        assert_eq!(batch.events.len(), 3);
        assert!(matches!(
            &batch.events[2],
            RuntimeYield::ToolFinished {
                name,
                success: false,
                ..
            } if name == "missing_tool"
        ));
        assert_eq!(sink.yields(), batch.events);

        let stored = context
            .session_db
            .get_events(&context.session_id)
            .await
            .expect("stored events");
        assert_eq!(stored.len(), 2);
        assert!(matches!(&stored[0].event, Event::ToolCall { name, .. } if name == "missing_tool"));
        assert!(matches!(
            &stored[1].event,
            Event::ToolResult { name, success, error, .. }
                if name == "missing_tool" && !success && error.is_some()
        ));
        assert_eq!(
            context.state.compatibility_state.action_history,
            Vec::<String>::new()
        );
    }

    #[tokio::test]
    async fn failed_event_append_returns_error_without_ingesting() {
        let mut context = make_context_without_db_session(GuardChain::new()).await;
        let mut sink = VecSink::new();

        let error = ActionEngine::new()
            .execute_batch(&mut context, &[make_call("mock_tool", "{}")], &mut sink)
            .await
            .expect_err("missing DB session should fail event append");

        assert!(error.message.contains("failed to persist runtime event"));
        assert_eq!(
            sink.yields(),
            vec![
                RuntimeYield::PermissionDecision {
                    tool_name: "mock_tool".into(),
                    call_id: Some("call-1".into()),
                    allowed: true,
                    reason: "read-only tool auto-approved".into(),
                },
                RuntimeYield::ToolStarted {
                    name: "mock_tool".into(),
                    call_id: Some("call-1".into()),
                },
            ]
        );
        assert_eq!(context.mind_palace.memory.event_count(), 0);
        assert!(context.state.compatibility_state.action_history.is_empty());
        assert!(context.session.messages.is_empty());
    }

    #[tokio::test]
    async fn permission_denial_returns_blocked_tool_result_without_execution() {
        let mut context = make_context(GuardChain::new()).await;
        context.config.permissions.mode = holmes_core::config::PermissionMode::Plan;
        let mut sink = VecSink::new();

        let batch = ActionEngine::new()
            .execute_batch(&mut context, &[make_call("mock_tool", "{}")], &mut sink)
            .await
            .expect("execute batch");

        assert_eq!(batch.results.len(), 1);
        assert!(batch.results[0].is_error);
        assert!(batch.results[0].text_content().contains("permission mode"));
        assert_eq!(
            batch.events,
            vec![
                RuntimeYield::PermissionDecision {
                    tool_name: "mock_tool".into(),
                    call_id: Some("call-1".into()),
                    allowed: false,
                    reason: "permission mode 'plan' blocks tool 'mock_tool'; Holmes must continue by planning or asking Watson".into(),
                },
                RuntimeYield::ToolFinished {
                    name: "guard".into(),
                    call_id: Some("call-1".into()),
                    success: false,
                    content: "[GUARD] permission mode 'plan' blocks tool 'mock_tool'; Holmes must continue by planning or asking Watson".into(),
                 error: None, usage: None },
            ]
        );
        assert_eq!(sink.yields(), batch.events);
        assert!(context.state.compatibility_state.action_history.is_empty());

        let stored = context
            .session_db
            .get_events(&context.session_id)
            .await
            .expect("stored events");
        assert_eq!(stored.len(), 2);
        assert!(matches!(&stored[0].event, Event::ToolCall { name, .. } if name == "mock_tool"));
        assert!(matches!(
            &stored[1].event,
            Event::ToolBlocked { tool_name, guard_name, reason }
                if tool_name == "mock_tool" && guard_name == "permission" && reason.contains("plan")
        ));
        assert_eq!(context.mind_palace.memory.event_count(), 2);
        assert_eq!(batch.messages[0].tool_call_id.as_deref(), Some("call-1"));
    }

    async fn make_context(guards: GuardChain) -> RuntimeContext {
        make_context_with_session(guards, true).await
    }

    async fn make_context_without_db_session(guards: GuardChain) -> RuntimeContext {
        make_context_with_session(guards, false).await
    }

    async fn make_context_with_session(
        mut guards: GuardChain,
        create_session: bool,
    ) -> RuntimeContext {
        guards.post.push(Box::new(RecordingPostGuard));

        let session_id = "session-1".to_string();
        let session_db = Arc::new(SessionDB::open(":memory:").await.expect("session db"));
        if create_session {
            session_db
                .create_session(CreateSessionParams {
                    id: Some(session_id.clone()),
                    title: None,
                    mode: Some(SessionMode::Pentest),
                    model: None,
                    system_prompt: None,
                    parent_session_id: None,
                    fork_point: None,
                    source: Some("test".into()),
                    tags: Vec::new(),
                })
                .await
                .expect("create session");
        }
        let memory_store = Arc::new(MemoryStore::open(":memory:").await.expect("memory store"));
        let mind_palace = MindPalace::new(session_db.clone(), memory_store.clone());
        let llm = Arc::new(StaticLlmBackend::new(LlmResponse {
            content: Some("ok".into()),
            tool_calls: Vec::new(),
            finish_reason: None,
            usage: None,
        }));
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(MockTool));

        RuntimeContext::new(
            RuntimeSession::new(session_id, SessionMode::Pentest),
            session_db,
            memory_store,
            mind_palace,
            llm,
            Arc::new(tools),
            guards,
            RuntimeState::new(SessionMode::Pentest),
            HolmesConfig::default(),
        )
    }

    fn make_call(name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: "call-1".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: name.into(),
                arguments: arguments.into(),
            },
        }
    }

    struct MockTool;

    #[async_trait]
    impl Tool for MockTool {
        fn name(&self) -> &str {
            "mock_tool"
        }

        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                tool_type: "function".into(),
                function: FunctionDefinition {
                    name: "mock_tool".into(),
                    description: "mock tool".into(),
                    parameters: Default::default(),
                },
            }
        }

        fn is_read_only(&self) -> bool {
            true
        }

        async fn execute(&self, _args: &str) -> Result<String> {
            Ok("mock output".into())
        }
    }

    struct BlockGuard;

    #[async_trait]
    impl PreGuard for BlockGuard {
        fn name(&self) -> &str {
            "block"
        }

        async fn check(&self, _call: &ToolCall, _state: &AttackState) -> GuardVerdict {
            GuardVerdict::block("blocked by test")
        }
    }

    struct RecordingPostGuard;

    #[async_trait]
    impl PostGuard for RecordingPostGuard {
        fn name(&self) -> &str {
            "record"
        }

        async fn process(&mut self, call: &ToolCall, result: &ToolResult, state: &mut AttackState) {
            state
                .action_history
                .push(format!("post:{}:{}", call.function.name, result.is_error));
        }
    }
}
