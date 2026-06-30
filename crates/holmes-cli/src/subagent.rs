use async_trait::async_trait;
use holmes_core::session::RuntimeSession;
use holmes_core::subagent::SubagentRunner;
use holmes_core::types::{SessionMode, SubAgentResult, SubAgentTask};
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

use holmes_core::config::Config;

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
        let session = RuntimeSession::new(sub_session_id.clone(), mode.clone());

        let mind_palace = MindPalace::new(self.session_db.clone(), self.memory_store.clone());
        let runtime_guards = GuardChain::from_config(&self.config.guards);
        let runtime_state = RuntimeState::new(mode);

        let mut registry = ToolRegistry::new();
        holmes_tools::builtin::register_all(
            &mut registry,
            &self.config,
            Some(Arc::new(self.clone())),
        );
        holmes_tools::mcp::register_mcp_tools(&mut registry, &self.config.mcp.servers).await;

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
