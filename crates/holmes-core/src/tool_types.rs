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

/// Truncate like [`truncate_str`], appending a `...[truncated, {total} total]` note
/// when truncation occurs. Used for bounded tool output; never slices mid-character.
pub fn truncate_with_note(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        s.to_string()
    } else {
        format!(
            "{}...[truncated, {} total]",
            truncate_str(s, max_bytes),
            s.len()
        )
    }
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
    /// Extended-thinking blocks `(thinking, signature)` for an assistant turn. Preserved
    /// (not persisted) so they can be echoed back on the next request when thinking is on.
    #[serde(skip)]
    pub thinking_blocks: Option<Vec<(String, String)>>,
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
            thinking_blocks: None,
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
            thinking_blocks: None,
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
            thinking_blocks: None,
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
            thinking_blocks: None,
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
            thinking_blocks: None,
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
            thinking_blocks: None,
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
            thinking_blocks: None,
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
    /// Authoritative execution outcome. `is_error` is retained as the wire/UI
    /// compatibility projection; evidence and supervision must use `status`.
    pub status: ToolOutcomeStatus,
    pub is_error: bool,
}

/// Typed result semantics shared by every tool boundary. This distinguishes a
/// real successful observation from process failure, deadline, cancellation and
/// policy denial; all five used to collapse into a content string plus a boolean.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutcomeStatus {
    Succeeded,
    Failed,
    TimedOut,
    Cancelled,
    Denied,
}

impl ToolOutcomeStatus {
    pub fn is_success(self) -> bool {
        matches!(self, Self::Succeeded)
    }
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
            status: ToolOutcomeStatus::Succeeded,
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
            status: ToolOutcomeStatus::Failed,
            is_error: true,
        }
    }

    pub fn timed_out(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self::with_status(
            tool_call_id,
            tool_name,
            ToolOutcomeStatus::TimedOut,
            content,
        )
    }

    pub fn cancelled(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self::with_status(
            tool_call_id,
            tool_name,
            ToolOutcomeStatus::Cancelled,
            content,
        )
    }

    pub fn denied(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self::with_status(tool_call_id, tool_name, ToolOutcomeStatus::Denied, content)
    }

    pub fn with_status(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        status: ToolOutcomeStatus,
        content: impl Into<String>,
    ) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            content: vec![ContentBlock::Text(content.into())],
            status,
            is_error: !status.is_success(),
        }
    }

    pub fn blocked(tool_call_id: impl Into<String>, guidance: impl Into<String>) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            tool_name: "guard".into(),
            content: vec![ContentBlock::Text(format!("[GUARD] {}", guidance.into()))],
            status: ToolOutcomeStatus::Denied,
            is_error: true,
        }
    }

    pub fn is_success(&self) -> bool {
        self.status.is_success()
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
        Message::tool_result(&self.tool_call_id, &self.tool_name, self.text_content())
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
            Message::tool_result(&self.tool_call_id, &self.tool_name, self.text_content());
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
#[derive(Debug, Clone, Default)]
pub struct LlmResponse {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
    pub usage: Option<Usage>,
    /// Extended-thinking blocks `(thinking, signature)`, when the model returned them.
    /// Preserved so they can be echoed back on the next request (Anthropic requires the
    /// original signed thinking block to accompany the tool_use it preceded).
    pub thinking_blocks: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

impl LlmResponse {
    pub fn to_message(&self) -> Message {
        let mut msg = if !self.tool_calls.is_empty() {
            Message::assistant_with_content_and_tool_calls(
                self.content.clone(),
                self.tool_calls.clone(),
            )
        } else {
            Message::assistant(self.content.clone().unwrap_or_default())
        };
        // Carry signed thinking blocks so from_messages can echo them next turn.
        if !self.thinking_blocks.is_empty() {
            msg.thinking_blocks = Some(self.thinking_blocks.clone());
        }
        msg
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

#[cfg(test)]
mod tests {
    use super::{truncate_str, truncate_with_note};

    #[test]
    fn truncate_str_never_splits_multibyte_chars() {
        // '中' is 3 bytes; cutting at byte 4 or 5 must back off to the boundary at 3.
        let s = "中文字符";
        assert_eq!(truncate_str(s, 5), "中");
        assert_eq!(truncate_str(s, 4), "中");
        assert_eq!(truncate_str(s, 3), "中");
        assert_eq!(truncate_str(s, 6), "中文");
        // Emoji (4 bytes).
        let e = "a🦀b";
        assert_eq!(truncate_str(e, 3), "a");
        assert_eq!(truncate_str(e, 5), "a🦀");
        // Under/over limits and empty input.
        assert_eq!(truncate_str("abc", 10), "abc");
        assert_eq!(truncate_str("", 0), "");
        assert_eq!(truncate_str("中文", 0), "");
    }

    #[test]
    fn truncate_with_note_appends_note_only_when_truncated() {
        assert_eq!(truncate_with_note("short", 10), "short");
        let out = truncate_with_note("中文字符输出", 5);
        assert!(out.starts_with("中...[truncated, 18 total]"));
    }

    #[test]
    fn truncation_is_safe_for_random_multibyte_strings_and_cut_points() {
        // Deterministic xorshift PRNG; no external deps. Property: for any string and
        // any cut point, the result is a prefix of the input on a char boundary and
        // never exceeds the byte limit.
        let mut state: u64 = 0x9E3779B97F4A7C15;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let alphabet = ['a', 'Z', '中', '文', '🦀', 'é', ' ', '\n', 'ß', '日'];
        for _ in 0..500 {
            let len = (next() % 200) as usize;
            let s: String = (0..len)
                .map(|_| alphabet[(next() as usize) % alphabet.len()])
                .collect();
            let max = (next() % 300) as usize;
            let cut = truncate_str(&s, max);
            assert!(cut.len() <= max);
            assert!(cut.len() <= s.len());
            assert!(s.starts_with(cut));
            let noted = truncate_with_note(&s, max);
            assert!(noted.starts_with(cut));
        }
    }
}
