use anyhow::Result;
use async_trait::async_trait;
use holmes_core::tool_types::{LlmResponse, Message, ToolDefinition};
use holmes_llm::client::LlmClient;

use crate::context::RuntimeContext;
use crate::decision::ParsedDecision;
use crate::perception::PerceptionFrame;

pub const MISSING_PROVIDER_MESSAGE: &str = "Holmes: I do not have a configured LLM provider yet. Run `holmes setup` or edit the config file before starting an investigation.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeErrorKind {
    Recoverable,
    NeedsUser,
    Fatal,
    ContextOverflow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeError {
    pub kind: RuntimeErrorKind,
    pub message: String,
}

impl RuntimeError {
    pub fn recoverable(message: impl Into<String>) -> Self {
        Self {
            kind: RuntimeErrorKind::Recoverable,
            message: message.into(),
        }
    }

    pub fn needs_user(message: impl Into<String>) -> Self {
        Self {
            kind: RuntimeErrorKind::NeedsUser,
            message: message.into(),
        }
    }

    pub fn fatal(message: impl Into<String>) -> Self {
        Self {
            kind: RuntimeErrorKind::Fatal,
            message: message.into(),
        }
    }

    pub fn missing_provider() -> Self {
        Self::needs_user(MISSING_PROVIDER_MESSAGE)
    }

    pub fn from_llm_error(error: anyhow::Error, configured_provider_count: usize) -> Self {
        let message = error.to_string();
        if is_context_overflow_error(&message) {
            Self {
                kind: RuntimeErrorKind::ContextOverflow,
                message,
            }
        } else if configured_provider_count == 0 && is_missing_provider_error(&message) {
            Self::missing_provider()
        } else {
            Self::recoverable(message)
        }
    }

    pub fn is_needs_user(&self) -> bool {
        self.kind == RuntimeErrorKind::NeedsUser
    }
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RuntimeError {}

fn is_missing_provider_error(message: &str) -> bool {
    let normalized = message.to_ascii_lowercase();
    normalized.contains("no healthy llm provider available")
        || normalized.contains("no configured llm provider")
        || normalized.contains("missing llm provider")
}

fn is_context_overflow_error(message: &str) -> bool {
    let normalized = message.to_ascii_lowercase();
    normalized.contains("context length")
        || normalized.contains("context window")
        || normalized.contains("maximum context")
        || normalized.contains("too many tokens")
        || normalized.contains("prompt is too long")
}

#[async_trait]
pub trait LlmBackend: Send + Sync {
    async fn chat_completion(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        role: &str,
    ) -> Result<LlmResponse>;
}

#[async_trait]
impl LlmBackend for LlmClient {
    async fn chat_completion(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        role: &str,
    ) -> Result<LlmResponse> {
        LlmClient::chat_completion(self, messages, tools, role).await
    }
}

#[derive(Debug, Clone)]
pub struct StaticLlmBackend {
    response: LlmResponse,
}

impl StaticLlmBackend {
    pub fn new(response: LlmResponse) -> Self {
        Self { response }
    }
}

#[async_trait]
impl LlmBackend for StaticLlmBackend {
    async fn chat_completion(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _role: &str,
    ) -> Result<LlmResponse> {
        Ok(self.response.clone())
    }
}

#[derive(Debug, Clone)]
pub struct DeliberationResult {
    pub response: LlmResponse,
    pub parsed: ParsedDecision,
}

impl DeliberationResult {
    pub fn from_response(response: LlmResponse) -> Self {
        let parsed = ParsedDecision::from_response(&response);
        Self { response, parsed }
    }
}

#[derive(Debug, Clone)]
pub struct DeliberationEngine {
    role: String,
}

impl DeliberationEngine {
    pub fn new(role: impl Into<String>) -> Self {
        Self { role: role.into() }
    }

    pub async fn decide(
        &self,
        context: &RuntimeContext,
        frame: &PerceptionFrame,
    ) -> std::result::Result<DeliberationResult, RuntimeError> {
        let messages = frame.build_transient_messages(&context.session.messages);
        let tools = context.tools.definitions();
        let configured_provider_count = context.config.llm.providers.len();

        context
            .llm
            .chat_completion(&messages, &tools, &self.role)
            .await
            .map(DeliberationResult::from_response)
            .map_err(|error| RuntimeError::from_llm_error(error, configured_provider_count))
    }
}

impl Default for DeliberationEngine {
    fn default() -> Self {
        Self::new("attack_agent")
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use holmes_core::config::{HolmesConfig, ProviderConfig};
    use holmes_core::session::RuntimeSession;
    use holmes_core::tool_types::{FunctionDefinition, Role};
    use holmes_core::types::SessionMode;
    use holmes_guards::GuardChain;
    use holmes_mind_palace::MindPalace;
    use holmes_session::{memory_store::MemoryStore, SessionDB};
    use holmes_tools::{Tool, ToolRegistry};

    use crate::context::{RuntimeContext, RuntimeState};

    use super::*;

    #[test]
    fn missing_provider_errors_map_to_needs_user_when_no_providers_are_configured() {
        let error = RuntimeError::from_llm_error(
            anyhow::anyhow!("LLM call failed: no healthy LLM provider available"),
            0,
        );

        assert_eq!(error.kind, RuntimeErrorKind::NeedsUser);
        assert_eq!(error.message, MISSING_PROVIDER_MESSAGE);
    }

    #[test]
    fn llm_context_overflow_maps_to_context_overflow_kind() {
        let error = RuntimeError::from_llm_error(
            anyhow::anyhow!("context length exceeded maximum context window"),
            1,
        );
        assert_eq!(error.kind, RuntimeErrorKind::ContextOverflow);
    }

    #[test]
    fn no_healthy_provider_errors_remain_recoverable_when_providers_are_configured() {
        let error = RuntimeError::from_llm_error(
            anyhow::anyhow!("LLM call failed: no healthy LLM provider available"),
            1,
        );

        assert_eq!(error.kind, RuntimeErrorKind::Recoverable);
        assert_eq!(
            error.message,
            "LLM call failed: no healthy LLM provider available"
        );
    }

    #[tokio::test]
    async fn decide_calls_backend_even_when_no_providers_are_configured() {
        let backend = Arc::new(RecordingLlmBackend::err(
            "LLM call failed: no healthy LLM provider available",
        ));
        let context = make_context(backend.clone(), HolmesConfig::default()).await;
        let frame = PerceptionFrame::from_context(&context);

        let error = DeliberationEngine::default()
            .decide(&context, &frame)
            .await
            .expect_err("missing provider should map to runtime error");

        assert_eq!(backend.call_count(), 1);
        assert_eq!(error.kind, RuntimeErrorKind::NeedsUser);
        assert_eq!(error.message, MISSING_PROVIDER_MESSAGE);
    }

    #[tokio::test]
    async fn decide_forwards_transient_messages_tools_and_role_to_backend() {
        let backend = Arc::new(RecordingLlmBackend::ok(LlmResponse {
            content: Some("done".into()),
            tool_calls: Vec::new(),
            finish_reason: Some("stop".into()),
            usage: None,
        }));
        let mut config = HolmesConfig::default();
        config.llm.providers.push(make_provider("primary"));
        let mut context = make_context(backend.clone(), config).await;
        context.state.observations.push("open port 443".into());
        context.state.failures.push("timeout".into());

        let frame = PerceptionFrame::from_context(&context);
        let response = DeliberationEngine::new("supervisor")
            .decide(&context, &frame)
            .await
            .expect("backend response");

        let call = backend.last_call().expect("recorded backend call");
        assert_eq!(response.response.content.as_deref(), Some("done"));
        assert_eq!(call.role, "supervisor");
        assert_eq!(call.messages.len(), context.session.messages.len() + 1);
        assert_eq!(call.messages[0].role, Role::User);
        assert_eq!(
            call.messages[0].content.as_deref(),
            Some("investigate example.test")
        );
        let transient = call.messages.last().expect("transient message");
        assert_eq!(transient.role, Role::User);
        assert!(transient
            .content
            .as_deref()
            .expect("transient content")
            .contains("open port 443"));
        assert_eq!(call.tools.len(), 1);
        assert_eq!(call.tools[0].function.name, "inspect_target");
    }

    #[tokio::test]
    async fn decide_keeps_no_healthy_provider_recoverable_when_providers_are_configured() {
        let backend = Arc::new(RecordingLlmBackend::err(
            "LLM call failed: no healthy LLM provider available",
        ));
        let mut config = HolmesConfig::default();
        config.llm.providers.push(make_provider("primary"));
        let context = make_context(backend, config).await;
        let frame = PerceptionFrame::from_context(&context);

        let error = DeliberationEngine::default()
            .decide(&context, &frame)
            .await
            .expect_err("configured unhealthy provider should be recoverable");

        assert_eq!(error.kind, RuntimeErrorKind::Recoverable);
        assert_eq!(
            error.message,
            "LLM call failed: no healthy LLM provider available"
        );
    }

    #[derive(Clone)]
    struct RecordedCall {
        messages: Vec<Message>,
        tools: Vec<ToolDefinition>,
        role: String,
    }

    struct RecordingLlmBackend {
        response: std::result::Result<LlmResponse, String>,
        calls: Mutex<Vec<RecordedCall>>,
    }

    impl RecordingLlmBackend {
        fn ok(response: LlmResponse) -> Self {
            Self {
                response: Ok(response),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn err(message: impl Into<String>) -> Self {
            Self {
                response: Err(message.into()),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.lock().expect("calls lock").len()
        }

        fn last_call(&self) -> Option<RecordedCall> {
            self.calls.lock().expect("calls lock").last().cloned()
        }
    }

    #[async_trait]
    impl LlmBackend for RecordingLlmBackend {
        async fn chat_completion(
            &self,
            messages: &[Message],
            tools: &[ToolDefinition],
            role: &str,
        ) -> Result<LlmResponse> {
            self.calls.lock().expect("calls lock").push(RecordedCall {
                messages: messages.to_vec(),
                tools: tools.to_vec(),
                role: role.into(),
            });

            match &self.response {
                Ok(response) => Ok(response.clone()),
                Err(message) => anyhow::bail!(message.clone()),
            }
        }
    }

    struct InspectTargetTool;

    #[async_trait]
    impl Tool for InspectTargetTool {
        fn name(&self) -> &str {
            "inspect_target"
        }

        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                tool_type: "function".into(),
                function: FunctionDefinition {
                    name: "inspect_target".into(),
                    description: "Inspect a target".into(),
                    parameters: Default::default(),
                },
            }
        }

        fn is_read_only(&self) -> bool {
            true
        }

        async fn execute(&self, _args: &str) -> Result<String> {
            Ok("ok".into())
        }
    }

    async fn make_context(llm: Arc<dyn LlmBackend>, config: HolmesConfig) -> RuntimeContext {
        let session_db = Arc::new(SessionDB::open(":memory:").await.expect("session db"));
        let memory_store = Arc::new(MemoryStore::open(":memory:").await.expect("memory store"));
        let mind_palace = MindPalace::new(session_db.clone(), memory_store.clone());
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(InspectTargetTool));

        RuntimeContext::new(
            RuntimeSession::new("session-1".into(), SessionMode::Pentest)
                .with_user_message("investigate example.test"),
            session_db,
            memory_store,
            mind_palace,
            llm,
            Arc::new(tools),
            GuardChain::new(),
            RuntimeState::new(SessionMode::Pentest),
            config,
        )
    }

    fn make_provider(name: &str) -> ProviderConfig {
        ProviderConfig {
            name: name.into(),
            base_url: format!("https://{name}.example.test/v1"),
            api_key: "test-key".into(),
            api_key_env: None,
            model: "test-model".into(),
            api_format: Default::default(),
            priority: 0,
            max_retries: 3,
            rpm_limit: 0,
        }
    }
}
