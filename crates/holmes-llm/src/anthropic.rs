use holmes_core::{FunctionCall, LlmResponse, Message, Role, ToolCall, ToolDefinition, Usage};
use serde::{Deserialize, Serialize};

// ── Request types ──

#[derive(Debug, Serialize)]
pub struct AnthropicRequest {
    pub model: String,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<Vec<SystemBlock>>,
    pub messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<AnthropicTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
}

/// A prompt-cache breakpoint. When attached to a content block, the entire prefix up
/// to and including that block is cached (Anthropic ephemeral cache, ~5 min TTL).
#[derive(Debug, Clone, Serialize)]
pub struct CacheControl {
    #[serde(rename = "type")]
    pub cache_type: String,
}

impl CacheControl {
    pub fn ephemeral() -> Self {
        Self {
            cache_type: "ephemeral".into(),
        }
    }
}

/// A system-prompt block. Modeled as an array so a `cache_control` breakpoint can be
/// attached to the (large, stable) system prompt.
#[derive(Debug, Serialize)]
pub struct SystemBlock {
    #[serde(rename = "type")]
    pub block_type: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
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
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    #[serde(rename = "thinking")]
    Thinking { thinking: String, signature: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: ToolResultContent,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

/// Attach an ephemeral cache breakpoint to the last content block of a message
/// (converting a plain-text message body to a block so the breakpoint has somewhere to
/// live). No-op for blocks that don't carry cache_control (thinking/image).
fn set_last_block_cache_control(msg: &mut AnthropicMessage) {
    match &mut msg.content {
        AnthropicContent::Blocks(blocks) => {
            if let Some(
                AnthropicContentBlock::Text { cache_control, .. }
                | AnthropicContentBlock::ToolUse { cache_control, .. }
                | AnthropicContentBlock::ToolResult { cache_control, .. },
            ) = blocks.last_mut()
            {
                *cache_control = Some(CacheControl::ephemeral());
            }
        }
        AnthropicContent::Text(t) => {
            let text = std::mem::take(t);
            msg.content = AnthropicContent::Blocks(vec![AnthropicContentBlock::Text {
                text,
                cache_control: Some(CacheControl::ephemeral()),
            }]);
        }
    }
}

impl AnthropicRequest {
    pub fn from_messages(
        model: impl Into<String>,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Self {
        let mut system_text: Option<String> = None;
        let mut anthropic_messages: Vec<AnthropicMessage> = Vec::new();
        let messages = sanitize_tool_use_groups(messages);

        for msg in &messages {
            let role_str = serde_json::to_value(&msg.role)
                .unwrap()
                .as_str()
                .unwrap()
                .to_string();

            match role_str.as_str() {
                "system" => {
                    // Anthropic uses top-level system field
                    if let Some(ref c) = msg.content {
                        system_text = Some(c.clone());
                    }
                }
                "assistant" => {
                    // Assistant message may have thinking + text + tool_calls
                    let mut blocks = Vec::new();

                    // Signed thinking blocks must come first and be echoed verbatim so
                    // the API accepts a tool_use that was preceded by extended thinking.
                    if let Some(ref thinking) = msg.thinking_blocks {
                        for (text, signature) in thinking {
                            blocks.push(AnthropicContentBlock::Thinking {
                                thinking: text.clone(),
                                signature: signature.clone(),
                            });
                        }
                    }

                    if let Some(ref content) = msg.content {
                        if !content.is_empty() {
                            blocks.push(AnthropicContentBlock::Text {
                                text: content.clone(),
                                cache_control: None,
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
                                cache_control: None,
                            });
                        }
                    }

                    if blocks.is_empty() {
                        blocks.push(AnthropicContentBlock::Text {
                            text: String::new(),
                            cache_control: None,
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
                        let mut blocks = vec![AnthropicContentBlock::Text {
                            text: content_str,
                            cache_control: None,
                        }];
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
                        cache_control: None,
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
                _ => {
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
        let mut merged = merge_consecutive_roles(anthropic_messages);

        // Rolling cache breakpoint on the last STABLE message (the second-to-last), so the
        // growing conversation history is cached across turns instead of reprocessed at
        // full price every turn. The final message carries the volatile per-turn transient
        // situation frame, so it is deliberately left after the breakpoint (uncached).
        if merged.len() >= 2 {
            let idx = merged.len() - 2;
            set_last_block_cache_control(&mut merged[idx]);
        }

        let anthropic_tools = if tools.is_empty() {
            None
        } else {
            let last = tools.len() - 1;
            Some(
                tools
                    .iter()
                    .enumerate()
                    .map(|(i, t)| AnthropicTool {
                        name: t.function.name.clone(),
                        description: t.function.description.clone(),
                        input_schema: t.function.parameters.clone(),
                        // Cache breakpoint on the last tool → the whole (stable) tool
                        // array is cached across turns.
                        cache_control: (i == last).then(CacheControl::ephemeral),
                    })
                    .collect(),
            )
        };

        // Cache the (large, stable) system prompt as well — a second breakpoint.
        let system = system_text.map(|text| {
            vec![SystemBlock {
                block_type: "text".into(),
                text,
                cache_control: Some(CacheControl::ephemeral()),
            }]
        });

        Self {
            model: model.into(),
            max_tokens: 16384,
            system,
            messages: merged,
            tools: anthropic_tools,
            temperature: None,
        }
    }
}

fn sanitize_tool_use_groups(messages: &[Message]) -> Vec<Message> {
    let mut sanitized = Vec::with_capacity(messages.len());
    let mut index = 0;

    while index < messages.len() {
        let message = messages[index].clone();
        index += 1;

        if message.role == Role::Tool {
            sanitized.push(orphan_tool_result_as_user_message(&message));
            continue;
        }

        let required_tool_results = if message.role == Role::Assistant {
            message
                .tool_calls
                .as_deref()
                .into_iter()
                .flat_map(|tool_calls| tool_calls.iter())
                .map(|tool_call| (tool_call.id.clone(), tool_call.function.name.clone()))
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        if required_tool_results.is_empty() {
            sanitized.push(message);
            continue;
        }

        let mut matched_results = Vec::new();
        let mut extra_results = Vec::new();

        while index < messages.len() && messages[index].role == Role::Tool {
            let tool_result = messages[index].clone();
            index += 1;

            if let Some(tool_call_id) = tool_result.tool_call_id.as_deref() {
                if required_tool_results
                    .iter()
                    .any(|(required_id, _)| required_id == tool_call_id)
                    && !matched_results.iter().any(|matched: &Message| {
                        matched.tool_call_id.as_deref() == Some(tool_call_id)
                    })
                {
                    matched_results.push(tool_result);
                    continue;
                }
            }

            extra_results.push(tool_result);
        }

        sanitized.push(message);
        for (tool_call_id, tool_name) in required_tool_results {
            if let Some(position) = matched_results
                .iter()
                .position(|result| result.tool_call_id.as_deref() == Some(tool_call_id.as_str()))
            {
                sanitized.push(matched_results.remove(position));
            } else {
                sanitized.push(Message::tool_result(
                    tool_call_id,
                    tool_name,
                    "[Tool output unavailable in Holmes conversation history; continue from current runtime state.]",
                ));
            }
        }

        sanitized.extend(extra_results.iter().map(orphan_tool_result_as_user_message));
    }

    sanitized
}

fn orphan_tool_result_as_user_message(message: &Message) -> Message {
    let name = message.name.as_deref().unwrap_or("unknown_tool");
    let content = message.content.as_deref().unwrap_or_default();
    Message::user(format!(
        "[Historical tool result without matching Anthropic tool_use: {name}]\n{content}"
    ))
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
                    AnthropicContent::Text(t) => vec![AnthropicContentBlock::Text {
                        text: t,
                        cache_control: None,
                    }],
                    AnthropicContent::Blocks(b) => b,
                };

                match msg.content {
                    AnthropicContent::Text(t) => {
                        blocks.push(AnthropicContentBlock::Text {
                            text: t,
                            cache_control: None,
                        });
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
    Thinking {
        thinking: String,
        #[serde(default)]
        signature: Option<String>,
    },
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
        let mut thinking_blocks = Vec::new();

        for block in self.content {
            match block {
                AnthropicResponseBlock::Text { text } => {
                    text_parts.push(text);
                }
                AnthropicResponseBlock::Thinking {
                    thinking,
                    signature,
                } => {
                    // Preserve the signed thinking block for round-trip echo (see
                    // from_messages). If unsigned, surface it as <think> text instead.
                    match signature {
                        Some(sig) => thinking_blocks.push((thinking, sig)),
                        None => text_parts.push(format!("<think>\n{}\n</think>", thinking)),
                    }
                }
                AnthropicResponseBlock::ToolUse { id, name, input } => {
                    tool_calls.push(ToolCall {
                        id,
                        call_type: "tool_use".into(),
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
            thinking_blocks,
        }
    }
}

/// Parse an Anthropic Messages **SSE stream** body into the same `LlmResponse` the
/// buffered path produces: text deltas accumulate into `content`, `input_json_delta`s
/// accumulate per tool_use block, and `message_delta` carries the stop reason + usage.
/// `on_text` is invoked with each text delta as it is seen (incremental display).
pub fn parse_sse_response(sse: &str, on_text: &mut dyn FnMut(&str)) -> LlmResponse {
    use serde_json::Value;

    #[derive(Default)]
    struct Block {
        kind: String,
        text: String,
        tool_id: String,
        tool_name: String,
        json: String,
    }

    let mut blocks: Vec<Block> = Vec::new();
    let mut stop_reason: Option<String> = None;
    let mut prompt_tokens = 0u32;
    let mut completion_tokens = 0u32;
    let ensure = |blocks: &mut Vec<Block>, idx: usize| {
        while blocks.len() <= idx {
            blocks.push(Block::default());
        }
    };

    for line in sse.lines() {
        let line = line.trim_start();
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                if let Some(u) = v.get("message").and_then(|m| m.get("usage")) {
                    prompt_tokens =
                        u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0) as u32;
                }
            }
            Some("content_block_start") => {
                let idx = v
                    .get("index")
                    .and_then(Value::as_u64)
                    .unwrap_or(blocks.len() as u64) as usize;
                ensure(&mut blocks, idx);
                let cb = v.get("content_block");
                let kind = cb
                    .and_then(|c| c.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or("text")
                    .to_string();
                let mut b = Block {
                    kind,
                    ..Default::default()
                };
                if b.kind == "tool_use" {
                    b.tool_id = cb
                        .and_then(|c| c.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    b.tool_name = cb
                        .and_then(|c| c.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                } else if let Some(t) = cb.and_then(|c| c.get("text")).and_then(Value::as_str) {
                    b.text.push_str(t);
                    on_text(t);
                }
                blocks[idx] = b;
            }
            Some("content_block_delta") => {
                let idx = v.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                ensure(&mut blocks, idx);
                let delta = v.get("delta");
                match delta.and_then(|d| d.get("type")).and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(t) = delta.and_then(|d| d.get("text")).and_then(Value::as_str) {
                            blocks[idx].text.push_str(t);
                            if blocks[idx].kind.is_empty() {
                                blocks[idx].kind = "text".into();
                            }
                            on_text(t);
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(j) = delta
                            .and_then(|d| d.get("partial_json"))
                            .and_then(Value::as_str)
                        {
                            blocks[idx].json.push_str(j);
                        }
                    }
                    _ => {}
                }
            }
            Some("message_delta") => {
                if let Some(sr) = v
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    stop_reason = Some(sr.to_string());
                }
                if let Some(o) = v
                    .get("usage")
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(Value::as_u64)
                {
                    completion_tokens = o as u32;
                }
            }
            _ => {}
        }
    }

    let mut text_parts = Vec::new();
    let mut tool_calls = Vec::new();
    for b in blocks {
        if b.kind == "tool_use" {
            tool_calls.push(ToolCall {
                id: b.tool_id,
                call_type: "tool_use".into(),
                function: FunctionCall {
                    name: b.tool_name,
                    arguments: if b.json.trim().is_empty() {
                        "{}".into()
                    } else {
                        b.json
                    },
                },
            });
        } else if !b.text.is_empty() {
            text_parts.push(b.text);
        }
    }

    let content = if text_parts.is_empty() {
        None
    } else {
        Some(text_parts.join(""))
    };
    let finish_reason = stop_reason.map(|sr| match sr.as_str() {
        "end_turn" => "stop".into(),
        "tool_use" => "tool_calls".into(),
        "max_tokens" => "length".into(),
        other => other.to_string(),
    });
    let usage = (prompt_tokens > 0 || completion_tokens > 0).then_some(Usage {
        prompt_tokens,
        completion_tokens,
        total_tokens: prompt_tokens + completion_tokens,
    });

    LlmResponse {
        content,
        tool_calls,
        finish_reason,
        usage,
        ..Default::default()
    }
}

/// Scan an SSE body for a terminal `error` event frame
/// (`data: {"type":"error","error":{"type":...,"message":...}}`), which Anthropic
/// sends mid-stream for overloaded/rate-limit/invalid-request failures after the
/// response has already been established. Returns `(error_type, message)`.
pub fn sse_error_event(sse: &str) -> Option<(String, String)> {
    use serde_json::Value;
    for line in sse.lines() {
        let Some(data) = line.trim_start().strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) == Some("error") {
            let err = v.get("error");
            let ty = err
                .and_then(|e| e.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("unknown_error")
                .to_string();
            let msg = err
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            return Some((ty, msg));
        }
    }
    None
}

/// Extract the assistant **text** carried by a single SSE line (`data: {...}`), for
/// incremental streaming display. Returns `Some(text)` for a `text_delta` or a text
/// `content_block_start`; `None` for anything else (tool-arg deltas, control frames, etc.).
/// The authoritative `LlmResponse` is still built by `parse_sse_response` over the full body —
/// this is display-only, so it deliberately ignores tool/thinking/usage frames.
pub fn sse_line_text_delta(line: &str) -> Option<String> {
    use serde_json::Value;
    let data = line.trim_start().strip_prefix("data:")?.trim();
    if data.is_empty() || data == "[DONE]" {
        return None;
    }
    let v = serde_json::from_str::<Value>(data).ok()?;
    match v.get("type").and_then(Value::as_str) {
        Some("content_block_delta") => {
            let delta = v.get("delta")?;
            if delta.get("type").and_then(Value::as_str) == Some("text_delta") {
                return delta
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            None
        }
        Some("content_block_start") => v
            .get("content_block")
            .filter(|c| c.get("type").and_then(Value::as_str) == Some("text"))
            .and_then(|c| c.get("text"))
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(str::to_string),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_line_text_delta_extracts_only_text() {
        assert_eq!(
            sse_line_text_delta(
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#
            ),
            Some("hi".to_string())
        );
        // tool-arg deltas are not display text
        assert_eq!(
            sse_line_text_delta(
                r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{"}}"#
            ),
            None
        );
        assert_eq!(sse_line_text_delta("event: ping"), None);
        assert_eq!(sse_line_text_delta("data: [DONE]"), None);
    }

    #[test]
    fn signed_thinking_block_is_captured_not_flattened() {
        let json = r#"{
            "content": [
                {"type": "thinking", "thinking": "let me reason", "signature": "sig-abc"},
                {"type": "text", "text": "answer"}
            ],
            "stop_reason": "end_turn"
        }"#;
        let resp: AnthropicResponse = serde_json::from_str(json).unwrap();
        let llm = resp.into_llm_response();
        assert_eq!(
            llm.thinking_blocks,
            vec![("let me reason".into(), "sig-abc".into())]
        );
        // The thinking is NOT dumped into content (it will be echoed structurally).
        assert_eq!(llm.content.as_deref(), Some("answer"));
    }

    #[test]
    fn assistant_thinking_blocks_are_echoed_first_in_the_request() {
        // An assistant turn that carried a signed thinking block must re-emit it (with
        // signature) ahead of its tool_use, or the API rejects the follow-up request.
        let mut assistant = Message::assistant_with_content_and_tool_calls(
            None,
            vec![ToolCall {
                id: "call_1".into(),
                call_type: "tool_use".into(),
                function: FunctionCall {
                    name: "read_file".into(),
                    arguments: "{}".into(),
                },
            }],
        );
        assistant.thinking_blocks = Some(vec![("reasoning".into(), "sig-xyz".into())]);
        let messages = vec![Message::user("go"), assistant];

        let req = AnthropicRequest::from_messages("m", &messages, &[]);
        let wire = serde_json::to_value(&req).unwrap();
        // Find the assistant message's first content block.
        let msgs = wire["messages"].as_array().unwrap();
        let asst = msgs.iter().find(|m| m["role"] == "assistant").unwrap();
        let first = &asst["content"][0];
        assert_eq!(first["type"], "thinking");
        assert_eq!(first["thinking"], "reasoning");
        assert_eq!(first["signature"], "sig-xyz");
    }

    #[test]
    fn parse_sse_text_stream_accumulates_and_emits_deltas() {
        let sse = "\
event: message_start
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":12}}}

event: content_block_start
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}

event: content_block_delta
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}

event: content_block_delta
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"lo\"}}

event: message_delta
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}

event: message_stop
data: {\"type\":\"message_stop\"}
";
        let mut deltas = String::new();
        let resp = parse_sse_response(sse, &mut |t| deltas.push_str(t));
        assert_eq!(resp.content.as_deref(), Some("Hello"));
        assert_eq!(
            deltas, "Hello",
            "each text delta is emitted for incremental display"
        );
        assert_eq!(resp.finish_reason.as_deref(), Some("stop"));
        let u = resp.usage.unwrap();
        assert_eq!(u.prompt_tokens, 12);
        assert_eq!(u.completion_tokens, 5);
    }

    #[test]
    fn parse_sse_tool_use_stream_reassembles_arguments() {
        let sse = "\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_1\",\"name\":\"read_file\"}}

data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}

data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"/tmp/x\\\"}\"}}

data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}
";
        let resp = parse_sse_response(sse, &mut |_| {});
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].function.name, "read_file");
        assert_eq!(
            resp.tool_calls[0].function.arguments,
            "{\"path\":\"/tmp/x\"}"
        );
        assert_eq!(resp.finish_reason.as_deref(), Some("tool_calls"));
    }

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
        assert_eq!(llm.tool_calls[0].call_type, "tool_use");
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
        let system = req.system.unwrap();
        assert_eq!(system[0].text, "You are a pentester.");
        // System prompt carries a cache breakpoint.
        assert!(system[0].cache_control.is_some());
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].role, "user");
    }

    #[test]
    fn conversation_history_gets_a_rolling_cache_breakpoint() {
        // A multi-turn history: the second-to-last message (stable) must carry a cache
        // breakpoint so the growing history is cached; the last (volatile transient) does not.
        let messages = vec![
            Message::user("first"),
            Message::assistant("reply one"),
            Message::user("second"),
            Message::assistant("reply two"),
            Message::user("[Current situation] transient tail"),
        ];
        let req = AnthropicRequest::from_messages("claude-3", &messages, &[]);
        let n = req.messages.len();
        let wire = serde_json::to_value(&req).unwrap();
        // Second-to-last message's last block has cache_control; the last does not.
        let second_last = &wire["messages"][n - 2]["content"];
        let last = &wire["messages"][n - 1]["content"];
        let has_cc = |v: &serde_json::Value| {
            v.as_array()
                .map(|blocks| blocks.iter().any(|b| b.get("cache_control").is_some()))
                .unwrap_or(false)
        };
        assert!(
            has_cc(second_last),
            "stable history must be cached: {second_last}"
        );
        assert!(
            !has_cc(last),
            "volatile transient tail must NOT be cached: {last}"
        );
    }

    #[test]
    fn tools_get_a_single_trailing_cache_breakpoint() {
        use holmes_core::{FunctionDefinition, ToolDefinition};
        let tools = vec![
            ToolDefinition {
                tool_type: "function".into(),
                function: FunctionDefinition {
                    name: "a".into(),
                    description: "first".into(),
                    parameters: serde_json::json!({}),
                },
            },
            ToolDefinition {
                tool_type: "function".into(),
                function: FunctionDefinition {
                    name: "b".into(),
                    description: "second".into(),
                    parameters: serde_json::json!({}),
                },
            },
        ];
        let req = AnthropicRequest::from_messages("claude-3", &[Message::user("hi")], &tools);
        let tools = req.tools.unwrap();
        assert!(tools[0].cache_control.is_none());
        assert!(
            tools[1].cache_control.is_some(),
            "last tool caches the array"
        );
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
    fn assistant_tool_use_without_result_gets_synthetic_tool_result() {
        let messages = vec![
            Message::system("sys"),
            Message::user("go"),
            Message::assistant_with_content_and_tool_calls(
                Some("I will inspect.".into()),
                vec![ToolCall {
                    id: "toolu_missing".into(),
                    call_type: "tool_use".into(),
                    function: FunctionCall {
                        name: "inspect_target".into(),
                        arguments: r#"{"path":"/Applications/Antigravity.app"}"#.into(),
                    },
                }],
            ),
            Message::user("continue"),
        ];

        let req = AnthropicRequest::from_messages("claude-3", &messages, &[]);
        let json = serde_json::to_value(&req).unwrap();
        let rendered = serde_json::to_string(&json).unwrap();

        assert!(!rendered.contains("tool_calls"));
        assert!(rendered.contains("tool_use"));
        assert!(rendered.contains("tool_result"));
        assert!(rendered.contains("toolu_missing"));
        assert!(rendered.contains("Tool output unavailable"));

        let messages = json["messages"].as_array().unwrap();
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert_eq!(messages[2]["content"][0]["tool_use_id"], "toolu_missing");
    }

    #[test]
    fn orphan_tool_result_is_preserved_as_plain_history_text() {
        let messages = vec![
            Message::system("sys"),
            Message::tool_result("orphan", "inspect_target", "old output"),
            Message::user("continue"),
        ];

        let req = AnthropicRequest::from_messages("claude-3", &messages, &[]);
        let json = serde_json::to_value(&req).unwrap();
        let rendered = serde_json::to_string(&json).unwrap();

        assert!(!rendered.contains("tool_result"));
        assert!(rendered.contains("Historical tool result without matching Anthropic tool_use"));
        assert!(rendered.contains("old output"));
    }

    #[test]
    fn tool_use_round_trip_uses_anthropic_wire_blocks() {
        let messages = vec![
            Message::system("sys"),
            Message::user("go"),
            Message::assistant_with_content_and_tool_calls(
                Some("Checking.".into()),
                vec![ToolCall {
                    id: "toolu_01".into(),
                    call_type: "tool_use".into(),
                    function: FunctionCall {
                        name: "execute_command".into(),
                        arguments: r#"{"cmd":"file /Applications/Antigravity.app"}"#.into(),
                    },
                }],
            ),
            Message::tool_result("toolu_01", "execute_command", "directory"),
        ];

        let req = AnthropicRequest::from_messages("claude-3", &messages, &[]);
        let rendered = serde_json::to_string(&serde_json::to_value(&req).unwrap()).unwrap();

        assert!(rendered.contains(r#""type":"tool_use""#));
        assert!(rendered.contains(r#""type":"tool_result""#));
        assert!(!rendered.contains("tool_calls"));
        assert!(!rendered.contains("tool_call_id"));
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
            status: holmes_core::ToolOutcomeStatus::Succeeded,
            is_error: false,
        };

        let msg = tr.to_message_with_vision();
        assert!(msg.image_blocks.is_some());
        assert_eq!(msg.image_blocks.as_ref().unwrap().len(), 1);

        let messages = vec![
            Message::system("sys"),
            Message::user("go"),
            Message::assistant_with_content_and_tool_calls(
                Some("ok".into()),
                vec![ToolCall {
                    id: "t1".into(),
                    call_type: "tool_use".into(),
                    function: FunctionCall {
                        name: "browser".into(),
                        arguments: "{}".into(),
                    },
                }],
            ),
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
            Message::assistant_with_content_and_tool_calls(
                Some("ok".into()),
                vec![ToolCall {
                    id: "t1".into(),
                    call_type: "tool_use".into(),
                    function: FunctionCall {
                        name: "lookup".into(),
                        arguments: "{}".into(),
                    },
                }],
            ),
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
