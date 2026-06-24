//! Tool / LLM message types ported from apeiron-core for the Holmes tool stack.
//!
//! These provide the shared [`ToolCall`] / [`ToolResult`] / [`ToolDefinition`]
//! vocabulary used by holmes-tools, holmes-guards, and the Anthropic wire
//! adapter in holmes-llm.

use serde::{Deserialize, Serialize};

/// Truncate a string to at most `max_bytes` bytes on a valid UTF-8 char boundary.
pub fn truncate_str(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// A single message in the LLM conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Optional base64-encoded image blocks for vision mode: `(base64, media_type)`.
    #[serde(skip)]
    pub image_blocks: Option<Vec<(String, String)>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            image_blocks: None,
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            image_blocks: None,
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            image_blocks: None,
        }
    }
    pub fn assistant_with_tool_calls(tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: None,
            tool_calls: Some(tool_calls),
            tool_call_id: None,
            name: None,
            image_blocks: None,
        }
    }
    pub fn assistant_with_content_and_tool_calls(
        content: Option<String>,
        tool_calls: Vec<ToolCall>,
    ) -> Self {
        Self {
            role: Role::Assistant,
            content,
            tool_calls: Some(tool_calls),
            tool_call_id: None,
            name: None,
            image_blocks: None,
        }
    }
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
            name: None,
            image_blocks: None,
        }
    }
    pub fn tool_result(
        tool_call_id: impl Into<String>,
        name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            role: Role::Tool,
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
            name: Some(name.into()),
            image_blocks: None,
        }
    }
}

/// A tool call from the LLM response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

impl ToolCall {
    pub fn args_parsed(&self) -> serde_json::Result<serde_json::Value> {
        serde_json::from_str(&self.function.arguments)
    }

    pub fn args_summary(&self, max_len: usize) -> String {
        let args = &self.function.arguments;
        if args.len() <= max_len {
            args.clone()
        } else {
            format!("{}...", truncate_str(args, max_len.saturating_sub(3)))
        }
    }
}

/// A content block within a tool result — text or base64-encoded image.
#[derive(Debug, Clone)]
pub enum ContentBlock {
    Text(String),
    Image { base64: String, media_type: String },
}

impl From<String> for ContentBlock {
    fn from(s: String) -> Self {
        ContentBlock::Text(s)
    }
}

impl From<&str> for ContentBlock {
    fn from(s: &str) -> Self {
        ContentBlock::Text(s.to_string())
    }
}

/// Result of executing a tool.
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: Vec<ContentBlock>,
    pub is_error: bool,
}

impl ToolResult {
    pub fn success(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            content: vec![ContentBlock::Text(content.into())],
            is_error: false,
        }
    }
    pub fn error(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            content: vec![ContentBlock::Text(content.into())],
            is_error: true,
        }
    }
    pub fn blocked(tool_call_id: impl Into<String>, guidance: impl Into<String>) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            tool_name: "guard".into(),
            content: vec![ContentBlock::Text(format!("[GUARD] {}", guidance.into()))],
            is_error: true,
        }
    }

    pub fn text_content(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text(s) => Some(s.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn to_message(&self) -> Message {
        Message::tool_result(&self.tool_call_id, &self.tool_name, &self.text_content())
    }

    pub fn to_message_with_vision(&self) -> Message {
        let images: Vec<(String, String)> = self
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Image { base64, media_type } => {
                    Some((base64.clone(), media_type.clone()))
                }
                _ => None,
            })
            .collect();
        let mut msg =
            Message::tool_result(&self.tool_call_id, &self.tool_name, &self.text_content());
        if !images.is_empty() {
            msg.image_blocks = Some(images);
        }
        msg
    }
}

/// Shared tool definition for LLM adapters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: FunctionDefinition,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Normalized LLM API response.
#[derive(Debug, Clone)]
pub struct LlmResponse {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

impl LlmResponse {
    pub fn to_message(&self) -> Message {
        if !self.tool_calls.is_empty() {
            Message::assistant_with_content_and_tool_calls(
                self.content.clone(),
                self.tool_calls.clone(),
            )
        } else {
            Message::assistant(self.content.clone().unwrap_or_default())
        }
    }
}

/// Guard verdict — returned by PreGuard checks.
#[derive(Debug, Clone)]
pub struct GuardVerdict {
    pub allowed: bool,
    pub guidance: String,
}

impl GuardVerdict {
    pub fn allow() -> Self {
        Self {
            allowed: true,
            guidance: String::new(),
        }
    }
    pub fn block(guidance: impl Into<String>) -> Self {
        Self {
            allowed: false,
            guidance: guidance.into(),
        }
    }
}

/// Iteration budget — thread-safe counter.
pub struct IterationBudget {
    max: u32,
    used: std::sync::atomic::AtomicU32,
}

impl IterationBudget {
    pub fn new(max: u32) -> Self {
        Self {
            max,
            used: std::sync::atomic::AtomicU32::new(0),
        }
    }
    pub fn consume(&self) -> bool {
        let prev = self.used.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        prev < self.max
    }
    pub fn remaining(&self) -> u32 {
        self.max
            .saturating_sub(self.used.load(std::sync::atomic::Ordering::Relaxed))
    }
    pub fn used(&self) -> u32 {
        self.used.load(std::sync::atomic::Ordering::Relaxed)
    }
}
