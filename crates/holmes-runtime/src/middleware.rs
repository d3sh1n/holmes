use async_trait::async_trait;
use crate::context::RuntimeContext;
use crate::deliberation::RuntimeError;
use holmes_core::event::Event;
use holmes_core::ToolResult;
use holmes_core::types::TokenDelta;
use std::sync::Mutex;
use regex::Regex;

#[async_trait]
pub trait RuntimeMiddleware: Send + Sync {
    // 会话启动时触发
    async fn on_session_start(&self, _ctx: &mut RuntimeContext) -> Result<(), RuntimeError> {
        Ok(())
    }

    // 在每一个 Step/Turn 循环开始前触发
    async fn before_step(&self, _ctx: &mut RuntimeContext) -> Result<(), RuntimeError> {
        Ok(())
    }

    // 在执行具体 Tool 之前触发，允许中间件拦截或修改工具参数
    async fn before_tool_call(
        &self,
        _ctx: &mut RuntimeContext,
        _tool_name: &mut String,
        _args: &mut serde_json::Value,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }

    // 在 Tool 执行完毕后触发，允许修改返回结果（如脱敏）
    async fn after_tool_call(
        &self,
        _ctx: &mut RuntimeContext,
        _result: &mut ToolResult,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }

    // 在一个 Step/Turn 结束后触发
    async fn after_step(&self, _ctx: &mut RuntimeContext) -> Result<(), RuntimeError> {
        Ok(())
    }

    // 在事件持久化之前触发，允许对事件数据进行脱敏处理
    async fn before_event_persist(
        &self,
        _ctx: &mut RuntimeContext,
        _event: &mut Event,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }

    // 每次发生 Token 消耗时触发
    async fn on_token_usage(
        &self,
        _ctx: &mut RuntimeContext,
        _delta: &TokenDelta,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }

    // 在最终输出结果给用户前触发，允许对结果文本进行最后脱敏
    async fn on_final_answer(
        &self,
        _ctx: &mut RuntimeContext,
        _content: &mut String,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }
}

// 1. GuardMiddleware
pub struct GuardMiddleware;

#[async_trait]
impl RuntimeMiddleware for GuardMiddleware {
    async fn before_tool_call(
        &self,
        _ctx: &mut RuntimeContext,
        tool_name: &mut String,
        args: &mut serde_json::Value,
    ) -> Result<(), RuntimeError> {
        // 进行静态危险命令拦截
        if tool_name == "run_command" {
            if let Some(cmd) = args.get("CommandLine").and_then(|v| v.as_str()) {
                let trimmed = cmd.trim();
                if trimmed.contains("rm -rf /") 
                    || trimmed.contains("mkfs") 
                    || trimmed.contains("dd if=")
                {
                    return Err(RuntimeError::recoverable(format!(
                        "GuardMiddleware: blocked dangerous command: {}",
                        cmd
                    )));
                }
            }
        }
        Ok(())
    }
}

// 2. SensitiveDataRedactMiddleware
pub struct SensitiveDataRedactMiddleware {
    regex_set: Vec<Regex>,
}

impl Default for SensitiveDataRedactMiddleware {
    fn default() -> Self {
        Self {
            regex_set: vec![
                Regex::new(r#"(?i)(api[_-]?key|token|auth|password|secret|passwd|private[_-]?key)(\s*[:=]\s*["']?)([a-zA-Z0-9_\-\.\+=/]{8,})(["']?)"#).unwrap()
            ],
        }
    }
}

impl SensitiveDataRedactMiddleware {
    pub fn new() -> Self {
        Self::default()
    }

    fn redact_text(&self, text: &str) -> String {
        let mut redacted = text.to_string();
        for re in &self.regex_set {
            redacted = re.replace_all(&redacted, |caps: &regex::Captures| {
                format!("{}{}[REDACTED]{}", &caps[1], &caps[2], &caps[4])
            }).to_string();
        }
        redacted
    }

    fn redact_json(&self, val: &mut serde_json::Value) {
        if let Ok(s) = serde_json::to_string(val) {
            let redacted_s = self.redact_text(&s);
            if let Ok(new_val) = serde_json::from_str(&redacted_s) {
                *val = new_val;
            }
        }
    }
}

#[async_trait]
impl RuntimeMiddleware for SensitiveDataRedactMiddleware {
    async fn after_tool_call(
        &self,
        _ctx: &mut RuntimeContext,
        result: &mut ToolResult,
    ) -> Result<(), RuntimeError> {
        for block in &mut result.content {
            if let holmes_core::tool_types::ContentBlock::Text(s) = block {
                *s = self.redact_text(s);
            }
        }
        Ok(())
    }

    async fn before_event_persist(
        &self,
        _ctx: &mut RuntimeContext,
        event: &mut Event,
    ) -> Result<(), RuntimeError> {
        match event {
            Event::UserMessage { content, .. } => {
                *content = self.redact_text(content);
            }
            Event::Thinking { content, .. } => {
                *content = self.redact_text(content);
            }
            Event::ToolCall { arguments, .. } => {
                self.redact_json(arguments);
            }
            Event::ToolResult { content, error, .. } => {
                *content = self.redact_text(content);
                if let Some(err) = error {
                    *err = self.redact_text(err);
                }
            }
            Event::ToolBlocked { reason, .. } => {
                *reason = self.redact_text(reason);
            }
            Event::GoalSet { plan, .. } => {
                if let Some(p) = plan {
                    *p = self.redact_text(p);
                }
            }
            Event::GoalEvaluated { reason, .. } => {
                *reason = self.redact_text(reason);
            }
            _ => {}
        }
        Ok(())
    }

    async fn on_final_answer(
        &self,
        _ctx: &mut RuntimeContext,
        content: &mut String,
    ) -> Result<(), RuntimeError> {
        *content = self.redact_text(content);
        Ok(())
    }
}

// 3. TokenAuditMiddleware
pub struct TokenAuditMiddleware {
    max_tokens: u64,
    accumulated_tokens: Mutex<u64>,
}

impl TokenAuditMiddleware {
    pub fn new(max_tokens: u64) -> Self {
        Self {
            max_tokens,
            accumulated_tokens: Mutex::new(0),
        }
    }
}

#[async_trait]
impl RuntimeMiddleware for TokenAuditMiddleware {
    async fn on_token_usage(
        &self,
        _ctx: &mut RuntimeContext,
        delta: &TokenDelta,
    ) -> Result<(), RuntimeError> {
        let mut accum = self.accumulated_tokens.lock().unwrap();
        let total = delta.input + delta.output + delta.cache_read + delta.cache_write;
        *accum += total;
        if *accum > self.max_tokens {
            return Err(RuntimeError::fatal(format!(
                "TokenAuditMiddleware: token limit exceeded! max={}, accumulated={}",
                self.max_tokens, *accum
            )));
        }
        Ok(())
    }
}

/// Block mutating `browser` actions (`click`, `fill`, `execute_js`) while the
/// session is running under `read_only` permission mode. Read-only actions
/// (`navigate`, `screenshot`, `get_content`) pass through.
pub struct BrowserReadOnlyMiddleware;

/// Pure decision: returns `Some(reason)` if the call must be blocked.
pub fn browser_write_blocked_under_readonly(
    mode: &holmes_core::config::PermissionMode,
    tool_name: &str,
    action: &str,
) -> Option<String> {
    if tool_name != "browser" {
        return None;
    }
    if !matches!(mode, holmes_core::config::PermissionMode::ReadOnly) {
        return None;
    }
    if matches!(action, "click" | "fill" | "execute_js") {
        return Some(format!(
            "browser action '{action}' is a write and is blocked under read_only permission mode"
        ));
    }
    None
}

#[async_trait]
impl RuntimeMiddleware for BrowserReadOnlyMiddleware {
    async fn before_tool_call(
        &self,
        ctx: &mut RuntimeContext,
        tool_name: &mut String,
        args: &mut serde_json::Value,
    ) -> Result<(), RuntimeError> {
        let action = args
            .get("action")
            .and_then(|a| a.as_str())
            .unwrap_or("");
        if let Some(reason) = browser_write_blocked_under_readonly(
            &ctx.config.permissions.mode,
            tool_name,
            action,
        ) {
            return Err(RuntimeError::recoverable(reason));
        }
        Ok(())
    }
}

#[cfg(test)]
mod browser_middleware_tests {
    use super::*;
    use holmes_core::config::PermissionMode;

    #[test]
    fn read_only_blocks_write_actions() {
        for action in ["click", "fill", "execute_js"] {
            assert!(browser_write_blocked_under_readonly(
                &PermissionMode::ReadOnly,
                "browser",
                action
            )
            .is_some(),);
        }
    }

    #[test]
    fn read_only_permits_read_actions() {
        for action in ["navigate", "screenshot", "get_content"] {
            assert!(browser_write_blocked_under_readonly(
                &PermissionMode::ReadOnly,
                "browser",
                action
            )
            .is_none(),);
        }
    }

    #[test]
    fn non_read_only_mode_allows_writes() {
        assert!(browser_write_blocked_under_readonly(
            &PermissionMode::Default,
            "browser",
            "click"
        )
        .is_none());
    }

    #[test]
    fn non_browser_tool_passes_through() {
        assert!(browser_write_blocked_under_readonly(
            &PermissionMode::ReadOnly,
            "http_request",
            "click"
        )
        .is_none());
    }
}
