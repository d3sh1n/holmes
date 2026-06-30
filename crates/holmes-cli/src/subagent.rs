use async_trait::async_trait;
use chrono::Utc;
use holmes_core::event::Event;
use holmes_core::session::RuntimeSession;
use holmes_core::subagent::SubagentRunner;
use holmes_core::types::{EndReason, SessionMode, SubAgentResult, SubAgentTask};
use holmes_guards::GuardChain;
use holmes_llm::client::LlmClient;
use holmes_mind_palace::MindPalace;
use holmes_runtime::context::{RuntimeContext, RuntimeState};
use holmes_runtime::deliberation::LlmBackend;
use holmes_runtime::runtime::AgentRuntime;
use holmes_runtime::yield_stream::{RuntimeSink, RuntimeYield};
use holmes_runtime::StreamEvent;
use holmes_session::{memory_store::MemoryStore, SessionStore};
use holmes_tools::registry::ToolRegistry;
use std::sync::Arc;
use uuid::Uuid;

use holmes_core::config::{resolve_attack_model_provider, Config};

fn active_tool_names(registry: &ToolRegistry) -> Vec<String> {
    let mut names = registry
        .definitions()
        .into_iter()
        .map(|definition| definition.function.name)
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    names
}

#[derive(Clone)]
pub struct CliSubagentRunner {
    pub session_db: Arc<dyn SessionStore>,
    pub memory_store: Arc<MemoryStore>,
    pub llm: Arc<LlmClient>,
    pub config: Config,
    pub parent_session_id: String,
}

pub struct CaptureSink {
    pub final_answer: Option<String>,
}

impl RuntimeSink for CaptureSink {
    fn emit(&mut self, event: StreamEvent) {
        if let RuntimeYield::FinalAnswer { content, .. } = event.data {
            self.final_answer = Some(content);
        }
    }
}

#[async_trait]
impl SubagentRunner for CliSubagentRunner {
    async fn run_subagent(&self, args: &str) -> Result<SubAgentResult, String> {
        // Attempt to parse the arguments into a SubAgentTask to ensure schema validity.
        let task: SubAgentTask =
            serde_json::from_str(args).map_err(|e| format!("Failed to parse task: {}", e))?;

        let sub_session_id = format!("sub-{}", Uuid::new_v4());
        let mode = SessionMode::default();
        let system_prompt = format!(
            "You are Holmes subagent for parent session {}. Context: {}",
            self.parent_session_id, task.context_summary
        );
        let resolved_model = resolve_attack_model_provider(&self.config, None);
        let model = resolved_model
            .as_ref()
            .map(|resolved| resolved.model.clone())
            .unwrap_or_else(|| "unknown".into());
        let provider = resolved_model.and_then(|resolved| resolved.provider);

        let _sub_session = self
            .session_db
            .create_session(holmes_session::db::CreateSessionParams {
                id: Some(sub_session_id.clone()),
                title: Some("Subagent".into()),
                mode: Some(mode.clone()),
                model: Some(model.clone()),
                system_prompt: Some(system_prompt.clone()),
                parent_session_id: Some(self.parent_session_id.clone()),
                fork_point: None,
                source: Some("subagent".into()),
                tags: vec!["subagent".into()],
            })
            .await
            .map_err(|e| e.to_string())?;

        let mut registry = ToolRegistry::new();
        holmes_tools::builtin::register_all(
            &mut registry,
            &self.config,
            Some(Arc::new(self.clone())),
            None,
        );
        holmes_tools::mcp::register_mcp_tools(&mut registry, &self.config.mcp.servers).await;
        let tool_names = active_tool_names(&registry);
        let now = Utc::now();
        let startup_events = [
            Event::SessionCreated {
                id: sub_session_id.clone(),
                title: Some("Subagent".into()),
                mode: mode.clone(),
                model: Some(model.clone()),
                system_prompt: Some(system_prompt.clone()),
                parent_id: Some(self.parent_session_id.clone()),
                fork_point: None,
                created_at: now,
                tags: vec!["subagent".into()],
            },
            Event::SessionSystemPromptSet {
                prompt_hash: holmes_core::stable_prompt_hash(&system_prompt),
                content: system_prompt.clone(),
                source: "startup".into(),
                timestamp: now,
            },
            Event::SessionModeSet {
                mode: mode.clone(),
                source: Some("startup".into()),
                timestamp: Some(now),
            },
            Event::SessionModelSet {
                model: model.clone(),
                provider: provider.clone(),
                source: "startup".into(),
                timestamp: now,
            },
            Event::ActiveToolsSet {
                tool_names,
                source: "startup".into(),
                timestamp: now,
            },
        ];
        for event in startup_events {
            if let Err(error) = self.session_db.append_event(&sub_session_id, &event).await {
                self.session_db
                    .end_session(&sub_session_id, EndReason::Error)
                    .await
                    .ok();
                return Err(error.to_string());
            }
        }

        let session = RuntimeSession::new(sub_session_id.clone(), mode.clone())
            .with_system_prompt(&system_prompt);
        let mind_palace = MindPalace::new(self.session_db.clone(), self.memory_store.clone());
        let runtime_guards = GuardChain::from_config(&self.config.guards);
        let runtime_state = RuntimeState::new(mode);

        let runtime_context = RuntimeContext::new(
            session,
            self.session_db.clone(),
            self.memory_store.clone(),
            mind_palace,
            self.llm.clone() as Arc<dyn LlmBackend>,
            Arc::new(registry),
            runtime_guards,
            runtime_state,
            self.config.clone(),
        );

        let mut runtime = AgentRuntime::new(runtime_context);
        let mut sink = CaptureSink { final_answer: None };

        runtime
            .run_oneshot(task.task.clone(), &mut sink)
            .await
            .map_err(|e| e.to_string())?;

        Ok(SubAgentResult {
            findings: vec![],
            risk_assessment: None,
            summary: sink
                .final_answer
                .unwrap_or_else(|| "No final answer produced by subagent.".to_string()),
            tokens_used: 0,
            events_count: 0,
            success: true,
            error: None,
        })
    }
}
