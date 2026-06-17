use holmes_core::{FunctionCall, LlmResponse, ToolCall, Usage};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct ChatCompletionResponse {
    pub choices: Vec<Choice>,
    #[serde(default)]
    pub usage: Option<UsageResponse>,
}

#[derive(Debug, Deserialize)]
pub struct Choice {
    pub message: ChoiceMessage,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ChoiceMessage {
    pub role: String,
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCallResponse>>,
}

#[derive(Debug, Deserialize)]
pub struct ToolCallResponse {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: FunctionCallResponse,
}

#[derive(Debug, Deserialize)]
pub struct FunctionCallResponse {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Deserialize)]
pub struct UsageResponse {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

impl ChatCompletionResponse {
    pub fn into_llm_response(self) -> Option<LlmResponse> {
        let choice = self.choices.into_iter().next()?;

        let tool_calls = choice
            .message
            .tool_calls
            .unwrap_or_default()
            .into_iter()
            .map(|tc| ToolCall {
                id: tc.id,
                call_type: tc.call_type,
                function: FunctionCall {
                    name: tc.function.name,
                    arguments: tc.function.arguments,
                },
            })
            .collect();

        let usage = self.usage.map(|u| Usage {
            prompt_tokens: u.prompt_tokens,
            completion_tokens: u.completion_tokens,
            total_tokens: u.total_tokens,
        });

        Some(LlmResponse {
            content: choice.message.content,
            tool_calls,
            finish_reason: choice.finish_reason,
            usage,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_text_response() {
        let json = r#"{
            "choices": [{
                "message": {"role": "assistant", "content": "Hello!"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        }"#;
        let resp: ChatCompletionResponse = serde_json::from_str(json).unwrap();
        let llm = resp.into_llm_response().unwrap();
        assert_eq!(llm.content.unwrap(), "Hello!");
        assert!(llm.tool_calls.is_empty());
        assert_eq!(llm.usage.unwrap().total_tokens, 15);
    }

    #[test]
    fn parse_tool_call_response() {
        let json = r#"{
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_123",
                        "type": "function",
                        "function": {
                            "name": "execute_command",
                            "arguments": "{\"command\": \"nmap -sV 10.0.0.1\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        }"#;
        let resp: ChatCompletionResponse = serde_json::from_str(json).unwrap();
        let llm = resp.into_llm_response().unwrap();
        assert!(llm.content.is_none());
        assert_eq!(llm.tool_calls.len(), 1);
        assert_eq!(llm.tool_calls[0].function.name, "execute_command");
    }

    #[test]
    fn parse_multiple_tool_calls() {
        let json = r#"{
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [
                        {"id": "c1", "type": "function", "function": {"name": "http_request", "arguments": "{}"}},
                        {"id": "c2", "type": "function", "function": {"name": "execute_command", "arguments": "{}"}}
                    ]
                },
                "finish_reason": "tool_calls"
            }]
        }"#;
        let resp: ChatCompletionResponse = serde_json::from_str(json).unwrap();
        let llm = resp.into_llm_response().unwrap();
        assert_eq!(llm.tool_calls.len(), 2);
    }

    #[test]
    fn empty_choices_returns_none() {
        let json = r#"{"choices": []}"#;
        let resp: ChatCompletionResponse = serde_json::from_str(json).unwrap();
        assert!(resp.into_llm_response().is_none());
    }
}
