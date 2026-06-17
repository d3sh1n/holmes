use crate::traits::PostGuard;
use holmes_core::state::AttackState;
use holmes_core::{ToolCall, ToolResult};
use std::collections::HashMap;
use tracing::debug;

pub struct Soft404Detector {
    response_fingerprints: HashMap<(u16, usize), u32>,
    threshold: u32,
}

impl Soft404Detector {
    pub fn new() -> Self {
        Self {
            response_fingerprints: HashMap::new(),
            threshold: 5,
        }
    }

    fn extract_fingerprint(content: &str) -> Option<(u16, usize)> {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(content) {
            let status = v
                .get("status_code")
                .or_else(|| v.get("status"))
                .and_then(|s| s.as_u64())
                .unwrap_or(0) as u16;
            let body_len = v
                .get("body")
                .and_then(|b| b.as_str())
                .map(|b| b.len())
                .or_else(|| {
                    v.get("content_length")
                        .and_then(|c| c.as_u64())
                        .map(|c| c as usize)
                })
                .unwrap_or(0);
            if status > 0 {
                return Some((status, body_len));
            }
        }
        None
    }
}

#[async_trait::async_trait]
impl PostGuard for Soft404Detector {
    fn name(&self) -> &str {
        "soft404_detector"
    }

    async fn process(&mut self, call: &ToolCall, result: &ToolResult, state: &mut AttackState) {
        if call.function.name != "http_request" || result.is_error {
            return;
        }

        if let Some(fp) = Self::extract_fingerprint(&result.text_content()) {
            let count = self.response_fingerprints.entry(fp).or_insert(0);
            *count += 1;

            if *count >= self.threshold && state.soft404_baseline.is_none() {
                debug!(
                    status = fp.0,
                    length = fp.1,
                    count = *count,
                    "soft-404 baseline detected"
                );
                state.soft404_baseline = Some(fp);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holmes_core::FunctionCall;

    fn make_state() -> AttackState {
        AttackState::new(
            "http://t:80".into(),
            "10.0.0.1".into(),
            "c".into(),
            "t".into(),
            vec![],
        )
    }

    fn http_call() -> ToolCall {
        ToolCall {
            id: "1".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "http_request".into(),
                arguments: "{}".into(),
            },
        }
    }

    #[tokio::test]
    async fn detects_soft_404_after_threshold() {
        let mut guard = Soft404Detector::new();
        let mut state = make_state();
        let body = "a".repeat(4835);
        let result_content = serde_json::json!({
            "status_code": 200,
            "body": body,
        })
        .to_string();

        for _ in 0..5 {
            let result = ToolResult::success("1", "http_request", &result_content);
            guard.process(&http_call(), &result, &mut state).await;
        }

        assert!(state.soft404_baseline.is_some());
        let (status, len) = state.soft404_baseline.unwrap();
        assert_eq!(status, 200);
        assert_eq!(len, 4835);
    }

    #[tokio::test]
    async fn no_detection_under_threshold() {
        let mut guard = Soft404Detector::new();
        let mut state = make_state();
        let result_content = serde_json::json!({
            "status_code": 200,
            "body": "unique content",
        })
        .to_string();

        for _ in 0..3 {
            let result = ToolResult::success("1", "http_request", &result_content);
            guard.process(&http_call(), &result, &mut state).await;
        }

        assert!(state.soft404_baseline.is_none());
    }

    #[tokio::test]
    async fn ignores_non_http_tools() {
        let mut guard = Soft404Detector::new();
        let mut state = make_state();
        let call = ToolCall {
            id: "1".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "execute_command".into(),
                arguments: "{}".into(),
            },
        };
        let result =
            ToolResult::success("1", "execute_command", r#"{"status_code":200,"body":"x"}"#);
        guard.process(&call, &result, &mut state).await;
        assert!(state.soft404_baseline.is_none());
    }
}
