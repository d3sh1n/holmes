use anyhow::Context;
use holmes_core::config::AgentConfig;
use holmes_core::event::Event;
use holmes_core::state::AttackState;
use holmes_core::tool_types::*;
use holmes_guards::GuardChain;
use holmes_llm::client::LlmClient;
use holmes_mind_palace::MindPalace;
use holmes_session::db::SessionDB;
use holmes_tools::ToolRegistry;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Run the core LLM ↔ Tool execution loop for one user turn.
///
/// Takes the conversation `messages`, calls the LLM, executes any tool calls
/// through the [`GuardChain`], feeds results back, and repeats until the LLM
/// produces a final text response (no more tool calls).
pub async fn run_agent_loop(
    messages: &mut Vec<Message>,
    llm: &LlmClient,
    registry: &ToolRegistry,
    guards: Arc<Mutex<GuardChain>>,
    state: Arc<Mutex<AttackState>>,
    session_db: &SessionDB,
    session_id: &str,
    mind_palace: &mut MindPalace,
    agent_config: &AgentConfig,
) -> anyhow::Result<String> {
    let tools = registry.definitions();
    let max_iterations = agent_config.max_iterations as usize;
    let budget = IterationBudget::new(agent_config.max_iterations);

    // Inject situation summary as context.
    let situation = mind_palace.situation_summary(&holmes_core::types::SessionMode::Pentest);
    if !situation.is_empty() {
        messages.push(Message::user(format!("[当前态势]\n{}", situation)));
    }

    // Main LLM ↔ Tool loop.
    let mut iteration = 0;
    loop {
        if iteration >= max_iterations || !budget.consume() {
            return Ok("(达到最大迭代次数，请继续指示)".to_string());
        }
        iteration += 1;

        // Call LLM.
        let response = llm
            .chat_completion(messages, &tools, "attack_agent")
            .await
            .context("LLM call failed")?;

        // Record thinking event.
        if let Some(content) = response.content.as_ref() {
            if !content.is_empty() {
                let thinking_event = Event::Thinking {
                    content: content.clone(),
                    reasoning_type: None,
                };
                session_db.append_event(session_id, &thinking_event).await?;
                mind_palace.ingest(thinking_event);
            }
        }

        // Add response to messages.
        let assistant_msg = response.to_message();
        messages.push(assistant_msg);

        // If no tool calls, return the text response.
        if response.tool_calls.is_empty() {
            return Ok(response.content.unwrap_or_default());
        }

        // Execute tool calls. We always go sequential because GuardChain::run_post
        // requires `&mut self` and `&mut AttackState`.
        let tool_calls = response.tool_calls;
        let results = execute_sequential(
            &tool_calls,
            registry,
            guards.clone(),
            state.clone(),
            session_db,
            session_id,
            mind_palace,
        )
        .await;

        // Add results to messages.
        for result in &results {
            messages.push(result.to_message());
        }
    }
}

async fn execute_sequential(
    calls: &[ToolCall],
    registry: &ToolRegistry,
    guards: Arc<Mutex<GuardChain>>,
    state: Arc<Mutex<AttackState>>,
    session_db: &SessionDB,
    session_id: &str,
    mind_palace: &mut MindPalace,
) -> Vec<ToolResult> {
    let mut results = Vec::new();

    for call in calls {
        // Record tool call event.
        let args_json: serde_json::Value = serde_json::from_str(&call.function.arguments)
            .unwrap_or_else(|_| serde_json::Value::String(call.function.arguments.clone()));
        let tool_call_event = Event::ToolCall {
            name: call.function.name.clone(),
            arguments: args_json,
            purpose: None,
        };
        let _ = session_db.append_event(session_id, &tool_call_event).await;
        mind_palace.ingest(tool_call_event);

        // Run pre-guards.
        let verdict = {
            let guards_ref = guards.lock().await;
            let state_ref = state.lock().await;
            guards_ref.run_pre(call, &*state_ref).await
        };

        let result = if !verdict.allowed {
            let blocked_event = Event::ToolBlocked {
                tool_name: call.function.name.clone(),
                guard_name: "guard".to_string(),
                reason: verdict.guidance.clone(),
            };
            let _ = session_db.append_event(session_id, &blocked_event).await;
            mind_palace.ingest(blocked_event);
            ToolResult::blocked(&call.id, verdict.guidance)
        } else {
            // Execute the tool.
            let result = registry.execute(call).await;

            // Run post-guards.
            {
                let mut guards_mut = guards.lock().await;
                let mut state_mut = state.lock().await;
                guards_mut.run_post(call, &result, &mut *state_mut).await;
            }

            // Record tool result event.
            let text = result.text_content();
            let tool_result_event = Event::ToolResult {
                name: call.function.name.clone(),
                success: !result.is_error,
                content: text.clone(),
                error: if result.is_error { Some(text) } else { None },
                artifacts: vec![],
            };
            let _ = session_db
                .append_event(session_id, &tool_result_event)
                .await;
            mind_palace.ingest(tool_result_event);

            result
        };

        results.push(result);
    }

    results
}
