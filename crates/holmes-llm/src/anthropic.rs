use holmes_core::{FunctionCall, LlmResponse, Message, ToolCall, ToolDefinition, Usage};
use serde::{Deserialize, Serialize};

// ── Request types ──

#[derive(Debug, Serialize)]
pub struct AnthropicRequest {
    pub model: String,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    pub messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<AnthropicTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
}

#[derive(Debug, Serialize)]
pub struct AnthropicMessage {
    pub role: String,
    pub content: AnthropicContent,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum AnthropicContent {
    Text(String),
    Blocks(Vec<AnthropicContentBlock>),
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum AnthropicContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: ToolResultContent,
    },
    #[serde(rename = "image")]
    Image { source: ImageSource },
}

#[derive(Debug, Serialize)]
pub struct ImageSource {
    #[serde(rename = "type")]
    pub source_type: String,
    pub media_type: String,
    pub data: String,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<AnthropicContentBlock>),
}

#[derive(Debug, Serialize)]
pub struct AnthropicTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

impl AnthropicRequest {
    pub fn from_messages(
        model: impl Into<String>,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Self {
        let mut system_text: Option<String> = None;
        let mut anthropic_messages: Vec<AnthropicMessage> = Vec::new();

        for msg in messages {
            let role_str = format!(
                "{}",
                serde_json::to_value(&msg.role).unwrap().as_str().unwrap()
            );

            match role_str.as_str() {
                "system" => {
                    // Anthropic uses top-level system field
                    if let Some(ref c) = msg.content {
                        system_text = Some(c.clone());
                    }
                }
                "assistant" => {
                    // Assistant message may have text + tool_calls
                    let mut blocks = Vec::new();

                    if let Some(ref content) = msg.content {
                        if !content.is_empty() {
                            blocks.push(AnthropicContentBlock::Text {
                                text: content.clone(),
                            });
                        }
                    }

                    if let Some(ref tcs) = msg.tool_calls {
                        for tc in tcs {
                            let input: serde_json::Value =
                                serde_json::from_str(&tc.function.arguments)
                                    .unwrap_or(serde_json::Value::Object(Default::default()));
                            blocks.push(AnthropicContentBlock::ToolUse {
                                id: tc.id.clone(),
                                name: tc.function.name.clone(),
                                input,
                            });
                        }
                    }

                    if blocks.is_empty() {
                        blocks.push(AnthropicContentBlock::Text {
                            text: String::new(),
                        });
                    }

                    anthropic_messages.push(AnthropicMessage {
                        role: "assistant".into(),
                        content: AnthropicContent::Blocks(blocks),
                    });
                }
                "tool" => {
                    // Tool result → merge into the last user message or create one
                    let tool_use_id = msg.tool_call_id.clone().unwrap_or_default();
                    let content_str = msg.content.clone().unwrap_or_default();

                    let content = if let Some(ref images) = msg.image_blocks {
                        let mut blocks = vec![AnthropicContentBlock::Text { text: content_str }];
                        for (base64, media_type) in images {
                            blocks.push(AnthropicContentBlock::Image {
                                source: ImageSource {
                                    source_type: "base64".into(),
                                    media_type: media_type.clone(),
                                    data: base64.clone(),
                                },
                            });
                        }
                        ToolResultContent::Blocks(blocks)
                    } else {
                        ToolResultContent::Text(content_str)
                    };

                    let block = AnthropicContentBlock::ToolResult {
                        tool_use_id,
                        content,
                    };

                    // Anthropic requires tool_result to be inside a "user" message
                    if let Some(last) = anthropic_messages.last_mut() {
                        if last.role == "user" {
                            if let AnthropicContent::Blocks(ref mut blocks) = last.content {
                                blocks.push(block);
                                continue;
                            }
                        }
                    }
                    anthropic_messages.push(AnthropicMessage {
                        role: "user".into(),
                        content: AnthropicContent::Blocks(vec![block]),
                    });
                }
                "user" | _ => {
                    let text = msg.content.clone().unwrap_or_default();
                    anthropic_messages.push(AnthropicMessage {
                        role: "user".into(),
                        content: AnthropicContent::Text(text),
                    });
                }
            }
        }

        // Ensure messages alternate user/assistant (Anthropic requirement)
        // Merge consecutive same-role messages
        let merged = merge_consecutive_roles(anthropic_messages);

        let anthropic_tools = if tools.is_empty() {
            None
        } else {
            Some(
                tools
                    .iter()
                    .map(|t| AnthropicTool {
                        name: t.function.name.clone(),
                        description: t.function.description.clone(),
                        input_schema: t.function.parameters.clone(),
                    })
                    .collect(),
            )
        };

        Self {
            model: model.into(),
            max_tokens: 16384,
            system: system_text,
            messages: merged,
            tools: anthropic_tools,
            temperature: None,
        }
    }
}

fn merge_consecutive_roles(messages: Vec<AnthropicMessage>) -> Vec<AnthropicMessage> {
    let mut result: Vec<AnthropicMessage> = Vec::new();

    for msg in messages {
        if let Some(last) = result.last_mut() {
            if last.role == msg.role {
                // Merge into last
                let mut blocks = match std::mem::replace(
                    &mut last.content,
                    AnthropicContent::Blocks(Vec::new()),
                ) {
                    AnthropicContent::Text(t) => vec![AnthropicContentBlock::Text { text: t }],
                    AnthropicContent::Blocks(b) => b,
                };

                match msg.content {
                    AnthropicContent::Text(t) => {
                        blocks.push(AnthropicContentBlock::Text { text: t });
                    }
                    AnthropicContent::Blocks(b) => {
                        blocks.extend(b);
                    }
                }

                last.content = AnthropicContent::Blocks(blocks);
                continue;
            }
        }
        result.push(msg);
    }

    result
}

// ── Response types ──

#[derive(Debug, Deserialize)]
pub struct AnthropicResponse {
    pub content: Vec<AnthropicResponseBlock>,
    pub stop_reason: Option<String>,
    pub usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum AnthropicResponseBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "thinking")]
    Thinking { thinking: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
}

#[derive(Debug, Deserialize)]
pub struct AnthropicUsage {
    #[serde(default)]
    pub input_tokens: u32,
    #[serde(default)]
    pub output_tokens: u32,
}

impl AnthropicResponse {
    pub fn into_llm_response(self) -> LlmResponse {
        let mut text_parts = Vec::new();
        let mut tool_calls = Vec::new();

        for block in self.content {
            match block {
                AnthropicResponseBlock::Text { text } => {
                    text_parts.push(text);
                }
                AnthropicResponseBlock::Thinking { thinking } => {
                    // Wrap thinking in <think> tags so the agent loop can extract it
                    text_parts.push(format!("<think>\n{}\n</think>", thinking));
                }
                AnthropicResponseBlock::ToolUse { id, name, input } => {
                    tool_calls.push(ToolCall {
                        id,
                        call_type: "function".into(),
                        function: FunctionCall {
                            name,
                            arguments: serde_json::to_string(&input).unwrap_or_default(),
                        },
                    });
                }
            }
        }

        let content = if text_parts.is_empty() {
            None
        } else {
            Some(text_parts.join("\n\n"))
        };

        let finish_reason = self.stop_reason.map(|sr| match sr.as_str() {
            "end_turn" => "stop".into(),
            "tool_use" => "tool_calls".into(),
            "max_tokens" => "length".into(),
            other => other.to_string(),
        });

        let usage = self.usage.map(|u| Usage {
            prompt_tokens: u.input_tokens,
            completion_tokens: u.output_tokens,
            total_tokens: u.input_tokens + u.output_tokens,
        });

        LlmResponse {
            content,
            tool_calls,
            finish_reason,
            usage,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_text_response() {
        let json = r#"{
            "content": [{"type": "text", "text": "Hello!"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        }"#;
        let resp: AnthropicResponse = serde_json::from_str(json).unwrap();
        let llm = resp.into_llm_response();
        assert_eq!(llm.content.unwrap(), "Hello!");
        assert!(llm.tool_calls.is_empty());
        assert_eq!(llm.finish_reason.unwrap(), "stop");
        assert_eq!(llm.usage.unwrap().total_tokens, 15);
    }

    #[test]
    fn parse_tool_use_response() {
        let json = r#"{
            "content": [
                {"type": "text", "text": "Let me check."},
                {"type": "tool_use", "id": "toolu_01", "name": "execute_command", "input": {"command": "nmap -sV 10.0.0.1"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 100, "output_tokens": 50}
        }"#;
        let resp: AnthropicResponse = serde_json::from_str(json).unwrap();
        let llm = resp.into_llm_response();
        assert!(llm.content.unwrap().contains("Let me check"));
        assert_eq!(llm.tool_calls.len(), 1);
        assert_eq!(llm.tool_calls[0].function.name, "execute_command");
        assert_eq!(llm.finish_reason.unwrap(), "tool_calls");
    }

    #[test]
    fn parse_thinking_response() {
        let json = r#"{
            "content": [
                {"type": "thinking", "thinking": "I should scan the target first."},
                {"type": "text", "text": "Starting scan."}
            ],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 20}
        }"#;
        let resp: AnthropicResponse = serde_json::from_str(json).unwrap();
        let llm = resp.into_llm_response();
        let content = llm.content.unwrap();
        assert!(content.contains("<think>"));
        assert!(content.contains("I should scan the target first."));
        assert!(content.contains("Starting scan."));
    }

    #[test]
    fn build_request_separates_system() {
        let messages = vec![
            Message::system("You are a pentester."),
            Message::user("Scan the target."),
        ];
        let req = AnthropicRequest::from_messages("claude-3", &messages, &[]);
        assert_eq!(req.system.unwrap(), "You are a pentester.");
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].role, "user");
    }

    #[test]
    fn multiple_tool_results_merge_into_user() {
        let messages = vec![
            Message::system("sys"),
            Message::user("go"),
            Message::assistant_with_content_and_tool_calls(
                None,
                vec![
                    ToolCall {
                        id: "t1".into(),
                        call_type: "function".into(),
                        function: FunctionCall {
                            name: "cmd".into(),
                            arguments: "{}".into(),
                        },
                    },
                    ToolCall {
                        id: "t2".into(),
                        call_type: "function".into(),
                        function: FunctionCall {
                            name: "cmd".into(),
                            arguments: "{}".into(),
                        },
                    },
                ],
            ),
            Message::tool("t1", "result1"),
            Message::tool("t2", "result2"),
        ];
        let req = AnthropicRequest::from_messages("claude-3", &messages, &[]);
        // system is extracted, so messages = [user, assistant, user(tool_results)]
        assert_eq!(req.messages.len(), 3);
        // Last message should be user with 2 tool_result blocks
        let last = &req.messages[2];
        assert_eq!(last.role, "user");
        match &last.content {
            AnthropicContent::Blocks(blocks) => assert_eq!(blocks.len(), 2),
            _ => panic!("expected blocks"),
        }
    }

    #[test]
    fn tool_result_with_image_serializes_as_content_blocks() {
        use holmes_core::{ContentBlock, ToolResult};

        let tr = ToolResult {
            tool_call_id: "t1".into(),
            tool_name: "browser".into(),
            content: vec![
                ContentBlock::Text("screenshot saved".into()),
                ContentBlock::Image {
                    base64: "iVBOR".into(),
                    media_type: "image/png".into(),
                },
            ],
            is_error: false,
        };

        let msg = tr.to_message_with_vision();
        assert!(msg.image_blocks.is_some());
        assert_eq!(msg.image_blocks.as_ref().unwrap().len(), 1);

        let messages = vec![
            Message::system("sys"),
            Message::user("go"),
            Message::assistant("ok"),
            msg,
        ];
        let req = AnthropicRequest::from_messages("claude-3", &messages, &[]);
        let json = serde_json::to_value(&req).unwrap();
        let last_msg = json["messages"].as_array().unwrap().last().unwrap();
        let content = &last_msg["content"];
        let blocks = content.as_array().unwrap();
        // Should have a tool_result block
        let tool_result = &blocks[0];
        assert_eq!(tool_result["type"], "tool_result");
        // The content of the tool_result should be an array with text + image
        let inner = tool_result["content"].as_array().unwrap();
        assert_eq!(inner.len(), 2);
        assert_eq!(inner[0]["type"], "text");
        assert_eq!(inner[1]["type"], "image");
        assert_eq!(inner[1]["source"]["type"], "base64");
        assert_eq!(inner[1]["source"]["media_type"], "image/png");
        assert_eq!(inner[1]["source"]["data"], "iVBOR");
    }

    #[test]
    fn tool_result_without_images_serializes_as_string() {
        let messages = vec![
            Message::system("sys"),
            Message::user("go"),
            Message::assistant("ok"),
            Message::tool("t1", "plain text result"),
        ];
        let req = AnthropicRequest::from_messages("claude-3", &messages, &[]);
        let json = serde_json::to_value(&req).unwrap();
        let last_msg = json["messages"].as_array().unwrap().last().unwrap();
        let blocks = last_msg["content"].as_array().unwrap();
        let tool_result = &blocks[0];
        assert_eq!(tool_result["type"], "tool_result");
        // content should be a plain string, not an array
        assert!(tool_result["content"].is_string());
        assert_eq!(tool_result["content"], "plain text result");
    }
}
