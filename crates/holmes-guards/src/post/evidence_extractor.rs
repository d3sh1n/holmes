use crate::traits::PostGuard;
use holmes_core::state::tool_truth::{Credential, ObjectRef};
use holmes_core::state::AttackState;
use holmes_core::{ToolCall, ToolResult};
use regex::Regex;
use tracing::debug;

pub struct EvidenceExtractor {
    cred_re: Regex,
    object_id_re: Regex,
}

impl EvidenceExtractor {
    pub fn new() -> Self {
        Self {
            cred_re: Regex::new(r"(?i)(?:user(?:name)?|login|email)\s*[:=]\s*(\S+)\s*(?:pass(?:word)?|pwd)\s*[:=]\s*(\S+)").unwrap(),
            object_id_re: Regex::new(r"(?i)(?:id|user_id|order_id|account_id)\s*[:=]\s*(\d+)").unwrap(),
        }
    }
}

#[async_trait::async_trait]
impl PostGuard for EvidenceExtractor {
    fn name(&self) -> &str {
        "evidence_extractor"
    }

    async fn process(&mut self, _call: &ToolCall, result: &ToolResult, state: &mut AttackState) {
        if result.is_error {
            return;
        }

        let content = &result.text_content();
        let bundle = state.evidence_bundle_mut();

        for cap in self.cred_re.captures_iter(content) {
            let username = cap[1].to_string();
            let password = cap[2].to_string();
            if !bundle
                .credentials
                .iter()
                .any(|c| c.username == username && c.password == password)
            {
                debug!(username = %username, "extracted credential");
                bundle.credentials.push(Credential {
                    username,
                    password,
                    source: result.tool_name.clone(),
                });
            }
        }

        for cap in self.object_id_re.captures_iter(content) {
            let id_value = cap[1].to_string();
            let path = cap[0]
                .split(|c: char| c == ':' || c == '=')
                .next()
                .unwrap_or("id")
                .trim()
                .to_string();
            if !bundle
                .object_refs
                .iter()
                .any(|r| r.id_value == id_value && r.path == path)
            {
                debug!(path = %path, id_value = %id_value, "extracted object reference");
                bundle.object_refs.push(ObjectRef { path, id_value });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_state() -> AttackState {
        AttackState::new(
            "http://t:80".into(),
            "10.0.0.1".into(),
            "c".into(),
            "t".into(),
            vec![],
        )
    }

    fn make_call() -> ToolCall {
        ToolCall {
            id: "1".into(),
            call_type: "function".into(),
            function: holmes_core::FunctionCall {
                name: "http_request".into(),
                arguments: "{}".into(),
            },
        }
    }

    #[tokio::test]
    async fn extracts_credentials() {
        let mut guard = EvidenceExtractor::new();
        let mut state = make_state();
        let result =
            ToolResult::success("1", "http_request", "username: admin password: secret123");
        guard.process(&make_call(), &result, &mut state).await;
        assert_eq!(state.evidence_bundle().credentials.len(), 1);
        assert_eq!(state.evidence_bundle().credentials[0].username, "admin");
        assert_eq!(state.evidence_bundle().credentials[0].password, "secret123");
    }

    #[tokio::test]
    async fn extracts_object_ids() {
        let mut guard = EvidenceExtractor::new();
        let mut state = make_state();
        let result = ToolResult::success("1", "http_request", "user_id: 42\norder_id: 100");
        guard.process(&make_call(), &result, &mut state).await;
        assert!(state.evidence_bundle().object_refs.len() >= 1);
    }

    #[tokio::test]
    async fn deduplicates_credentials() {
        let mut guard = EvidenceExtractor::new();
        let mut state = make_state();
        let result = ToolResult::success("1", "cmd", "username: admin password: pass");
        guard.process(&make_call(), &result, &mut state).await;
        guard.process(&make_call(), &result, &mut state).await;
        assert_eq!(state.evidence_bundle().credentials.len(), 1);
    }
}
