use crate::traits::PostGuard;
use holmes_core::state::validated::{Finding, FindingConfidence};
use holmes_core::state::AttackState;
use holmes_core::{ToolCall, ToolResult};
use tracing::{info, warn};

pub struct SkepticGate;

#[async_trait::async_trait]
impl PostGuard for SkepticGate {
    fn name(&self) -> &str {
        "skeptic_gate"
    }

    async fn process(&mut self, call: &ToolCall, _result: &ToolResult, state: &mut AttackState) {
        if call.function.name != "report_finding" {
            return;
        }

        let report: serde_json::Value = match serde_json::from_str(&call.function.arguments) {
            Ok(v) => v,
            Err(_) => return,
        };

        let title = report
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let attack_type = report
            .get("attack_type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let confidence_str = report
            .get("confidence")
            .and_then(|v| v.as_str())
            .unwrap_or("possible");
        let evidence = report
            .get("evidence")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let confidence = match confidence_str {
            "confirmed" => FindingConfidence::Confirmed,
            _ => FindingConfidence::Candidate,
        };

        if confidence == FindingConfidence::Confirmed {
            if evidence.len() < 20 {
                warn!(title = %title, "skeptic gate rejected: confirmed finding with insufficient evidence");
                return;
            }
            if !Self::evidence_cross_check(&evidence, state) {
                warn!(title = %title, "skeptic gate rejected: evidence not corroborated by tool output");
                return;
            }
        }

        if attack_type != state.current_attack_type && !state.current_attack_type.is_empty() {
            warn!(
                title = %title,
                finding_type = %attack_type,
                current_type = %state.current_attack_type,
                "skeptic gate: attack_type mismatch, downgrading to candidate"
            );
            let finding = Finding {
                id: title.clone(),
                finding_type: attack_type.clone(),
                confidence: FindingConfidence::Candidate,
                evidence,
                details: report
                    .get("details")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                attack_type,
            };
            state.findings_mut().insert(title, finding);
            return;
        }

        info!(title = %title, confidence = %confidence_str, "finding accepted by skeptic gate");
        let finding = Finding {
            id: title.clone(),
            finding_type: attack_type.clone(),
            confidence,
            evidence,
            details: report
                .get("details")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            attack_type,
        };
        state.findings_mut().insert(title, finding);
    }
}

impl SkepticGate {
    fn evidence_cross_check(evidence: &str, state: &AttackState) -> bool {
        let action_history = &state.action_history;
        if action_history.is_empty() {
            return false;
        }
        action_history.iter().any(|action| {
            evidence
                .split_whitespace()
                .any(|word| word.len() > 4 && action.contains(word))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holmes_core::FunctionCall;

    fn make_state() -> AttackState {
        let mut s = AttackState::new(
            "http://t:80".into(),
            "10.0.0.1".into(),
            "c".into(),
            "t".into(),
            vec![],
        );
        s.current_attack_type = "sqli".into();
        s.action_history
            .push("http_request(/search?q=1' OR 1=1--)".into());
        s
    }

    fn finding_call(title: &str, attack_type: &str, confidence: &str, evidence: &str) -> ToolCall {
        ToolCall {
            id: "1".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "report_finding".into(),
                arguments: serde_json::json!({
                    "title": title,
                    "attack_type": attack_type,
                    "confidence": confidence,
                    "evidence": evidence,
                })
                .to_string(),
            },
        }
    }

    #[tokio::test]
    async fn accepts_confirmed_with_evidence() {
        let mut gate = SkepticGate;
        let mut state = make_state();
        let call = finding_call(
            "SQL Injection",
            "sqli",
            "confirmed",
            "search endpoint returns database error with OR 1=1",
        );
        let result = ToolResult::success("1", "report_finding", "pending");
        gate.process(&call, &result, &mut state).await;
        assert!(state.findings().contains_key("SQL Injection"));
        assert_eq!(
            state.findings()["SQL Injection"].confidence,
            FindingConfidence::Confirmed
        );
    }

    #[tokio::test]
    async fn rejects_confirmed_without_evidence() {
        let mut gate = SkepticGate;
        let mut state = make_state();
        let call = finding_call("SQLi", "sqli", "confirmed", "yes");
        let result = ToolResult::success("1", "report_finding", "pending");
        gate.process(&call, &result, &mut state).await;
        assert!(!state.findings().contains_key("SQLi"));
    }

    #[tokio::test]
    async fn downgrades_mismatched_attack_type() {
        let mut gate = SkepticGate;
        let mut state = make_state();
        let call = finding_call(
            "XSS Found",
            "xss",
            "confirmed",
            "reflected script tag in search response output",
        );
        let result = ToolResult::success("1", "report_finding", "pending");
        gate.process(&call, &result, &mut state).await;
        assert!(state.findings().contains_key("XSS Found"));
        assert_eq!(
            state.findings()["XSS Found"].confidence,
            FindingConfidence::Candidate
        );
    }
}
