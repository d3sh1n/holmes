use holmes_core::{ToolCall, ToolResult};

use crate::deliberation::{RuntimeError, RuntimeErrorKind, MISSING_PROVIDER_MESSAGE};
use crate::yield_stream::RuntimeYield;

#[derive(Debug, Clone, Default)]
pub struct DialogueEngine;

impl DialogueEngine {
    pub fn message_to_user(content: impl AsRef<str>) -> Option<RuntimeYield> {
        let content = content.as_ref().trim();
        if content.is_empty() {
            None
        } else {
            Some(RuntimeYield::MessageToUser {
                content: content.to_string(),
            })
        }
    }

    pub fn format_error(&self, error: &RuntimeError) -> RuntimeYield {
        Self::error(error)
    }

    pub fn format_final_answer(&self, content: impl AsRef<str>) -> RuntimeYield {
        Self::final_answer(content)
    }

    pub fn tool_started(call: &ToolCall) -> RuntimeYield {
        RuntimeYield::ToolStarted {
            name: call.function.name.clone(),
            call_id: Some(call.id.clone()),
        }
    }

    pub fn tool_finished(result: &ToolResult) -> RuntimeYield {
        RuntimeYield::ToolFinished {
            name: result.tool_name.clone(),
            call_id: Some(result.tool_call_id.clone()),
            success: !result.is_error,
            content: concise(&result.text_content()),
            error: None,
            usage: None,
        }
    }

    pub fn evidence_update(content: impl AsRef<str>) -> RuntimeYield {
        RuntimeYield::EvidenceUpdate {
            content: concise(content.as_ref()),
        }
    }

    pub fn missing_provider() -> RuntimeYield {
        RuntimeYield::NeedsUserInput {
            prompt: MISSING_PROVIDER_MESSAGE.into(),
        }
    }

    pub fn error(error: &RuntimeError) -> RuntimeYield {
        match error.kind {
            RuntimeErrorKind::NeedsUser => RuntimeYield::NeedsUserInput {
                prompt: error.message.clone(),
            },
            RuntimeErrorKind::Recoverable | RuntimeErrorKind::Fatal => RuntimeYield::Error {
                message: error.message.clone(),
            },
        }
    }

    pub fn final_answer(content: impl AsRef<str>) -> RuntimeYield {
        RuntimeYield::FinalAnswer {
            content: content.as_ref().trim().to_string(),
            usage: None,
        }
    }
}

fn concise(content: &str) -> String {
    const MAX_CHARS: usize = 500;
    let trimmed = content.trim();
    if trimmed.chars().count() <= MAX_CHARS {
        return trimmed.to_string();
    }

    let mut out: String = trimmed.chars().take(MAX_CHARS.saturating_sub(3)).collect();
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use holmes_core::{FunctionCall, ToolCall, ToolResult};

    use super::*;

    #[test]
    fn dialogue_formats_tool_events_and_final_answer() {
        let call = ToolCall {
            id: "call-1".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "http_request".into(),
                arguments: "{}".into(),
            },
        };
        let result = ToolResult::success("call-1", "http_request", " 200 OK ");

        assert_eq!(
            DialogueEngine::tool_started(&call),
            RuntimeYield::ToolStarted {
                name: "http_request".into(),
                call_id: Some("call-1".into())
            }
        );
        assert_eq!(
            DialogueEngine::tool_finished(&result),
            RuntimeYield::ToolFinished {
                name: "http_request".into(),
                call_id: Some("call-1".into()),
                success: true,
                content: "200 OK".into()
            , error: None, usage: None }
        );
        assert_eq!(
            DialogueEngine::final_answer(" done "),
            RuntimeYield::FinalAnswer {
                content: "done".into()
            , usage: None }
        );
    }

    #[test]
    fn dialogue_formats_nonempty_message_to_user() {
        assert_eq!(
            DialogueEngine::message_to_user(" I will inspect the service. "),
            Some(RuntimeYield::MessageToUser {
                content: "I will inspect the service.".into()
            })
        );
        assert_eq!(DialogueEngine::message_to_user("  "), None);
    }

    #[test]
    fn dialogue_formats_evidence_update() {
        assert_eq!(
            DialogueEngine::evidence_update(" evidence captured "),
            RuntimeYield::EvidenceUpdate {
                content: "evidence captured".into()
            }
        );

        let long_content = "x".repeat(600);
        assert_eq!(
            DialogueEngine::evidence_update(long_content),
            RuntimeYield::EvidenceUpdate {
                content: format!("{}...", "x".repeat(497))
            }
        );
    }

    #[test]
    fn dialogue_formats_missing_provider_as_user_prompt() {
        assert_eq!(
            DialogueEngine::missing_provider(),
            RuntimeYield::NeedsUserInput {
                prompt: MISSING_PROVIDER_MESSAGE.into()
            }
        );
    }

    #[test]
    fn dialogue_maps_needs_user_error_to_user_prompt() {
        let error = RuntimeError::needs_user("configure a provider");

        assert_eq!(
            DialogueEngine::error(&error),
            RuntimeYield::NeedsUserInput {
                prompt: "configure a provider".into()
            }
        );
    }

    #[test]
    fn dialogue_maps_recoverable_error_to_error_yield() {
        let error = RuntimeError::recoverable("temporary failure");

        assert_eq!(
            DialogueEngine::error(&error),
            RuntimeYield::Error {
                message: "temporary failure".into()
            }
        );
    }

    #[test]
    fn dialogue_maps_fatal_error_to_error_yield() {
        let error = RuntimeError::fatal("fatal failure");

        assert_eq!(
            DialogueEngine::error(&error),
            RuntimeYield::Error {
                message: "fatal failure".into()
            }
        );
    }
}
