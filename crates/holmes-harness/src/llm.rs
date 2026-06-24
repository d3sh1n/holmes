use std::collections::VecDeque;
use std::sync::Mutex;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use holmes_core::tool_types::{LlmResponse, Message, ToolDefinition};
use holmes_runtime::deliberation::LlmBackend;

#[derive(Debug)]
pub struct ScriptedLlmBackend {
    responses: Mutex<VecDeque<LlmResponse>>,
}

impl ScriptedLlmBackend {
    pub fn new(responses: impl IntoIterator<Item = LlmResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
        }
    }
}

#[async_trait]
impl LlmBackend for ScriptedLlmBackend {
    async fn chat_completion(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _role: &str,
    ) -> Result<LlmResponse> {
        let mut responses = self
            .responses
            .lock()
            .map_err(|_| anyhow!("scripted LLM response queue is poisoned"))?;
        responses
            .pop_front()
            .ok_or_else(|| anyhow!("scripted LLM response queue is empty"))
    }
}
