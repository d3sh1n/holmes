use crate::traits::PostGuard;
use holmes_core::event::Severity;
use holmes_core::state::validated::{Finding, FindingConfidence};
use holmes_core::state::AttackState;
use holmes_core::{ToolCall, ToolResult};
use tracing::{info, warn};

/// Minimum length of the model's evidence text for a "confirmed" claim to be
/// honored as Confirmed rather than downgraded to Candidate.
const MIN_CONFIRMED_EVIDENCE_CHARS: usize = 20;

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

        if title.is_empty() {
            return;
        }

        // The model proposes confidence; the Runtime execution boundary validates
        // cited immutable Resolution IDs and overwrites this private attestation.
        // A direct/unattested call can only create a Candidate, so this post-guard
        // cannot be used as a self-confirmation path.
        let attested_status = report
            .get("_ledger_validation")
            .and_then(|value| value.get("status"))
            .and_then(serde_json::Value::as_str);
        let confidence = match (confidence_str, attested_status) {
            ("confirmed", Some("confirmed")) => FindingConfidence::Confirmed,
            ("not_vulnerable" | "rejected" | "ruled_out", Some("rejected")) => {
                FindingConfidence::Rejected
            }
            _ => FindingConfidence::Candidate,
        };
        let unattested_strong_claim = matches!(
            confidence_str,
            "confirmed" | "not_vulnerable" | "rejected" | "ruled_out"
        ) && confidence == FindingConfidence::Candidate;
        let thin_evidence = confidence == FindingConfidence::Confirmed
            && evidence.trim().len() < MIN_CONFIRMED_EVIDENCE_CHARS;
        if thin_evidence {
            warn!(title = %title, "skeptic gate: Ledger-validated finding has brief evidence text");
        }
        if unattested_strong_claim {
            warn!(title = %title, "skeptic gate: strong finding claim lacked Runtime Resolution attestation and was downgraded to Candidate");
        }

        let severity = report
            .get("severity")
            .and_then(|v| v.as_str())
            .map(parse_severity)
            .unwrap_or_default();
        let location = report
            .get("location")
            .or_else(|| report.get("affected_endpoint"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        // The model may cite the tool call whose output evidences this finding; fall back
        // to the report_finding call id so the finding always links to a transaction.
        let evidence_source = report
            .get("evidence_source")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| Some(call.id.clone()));

        info!(title = %title, confidence = ?confidence, severity = ?severity, "finding recorded");
        let mut details = report
            .get("details")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if thin_evidence {
            if !details.is_empty() {
                details.push(' ');
            }
            details.push_str(
                "[note: evidence text is brief; confidence is backed by a Runtime-validated Resolution]",
            );
        }
        if unattested_strong_claim {
            if !details.is_empty() {
                details.push(' ');
            }
            details.push_str(
                "[note: requested strong confidence lacked Runtime Resolution attestation and was recorded as Candidate]",
            );
        }
        let finding = Finding {
            id: title.clone(),
            finding_type: attack_type.clone(),
            confidence,
            evidence,
            details,
            attack_type,
            severity,
            location,
            evidence_source,
        };
        // Record + queue for durable persistence (monotonic: won't demote a Confirmed).
        state.record_and_persist_finding(finding);
    }
}

fn parse_severity(s: &str) -> Severity {
    match s.trim().to_lowercase().as_str() {
        "critical" => Severity::Critical,
        "high" => Severity::High,
        "medium" | "moderate" => Severity::Medium,
        "low" => Severity::Low,
        _ => Severity::Info,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holmes_core::FunctionCall;

    // Note: no `current_attack_type` / `action_history` priming — those fields are
    // never populated in production, so the gate must behave correctly without them.
    fn make_state() -> AttackState {
        AttackState::new(
            "http://t:80".into(),
            "10.0.0.1".into(),
            "c".into(),
            "t".into(),
            vec![],
        )
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

    fn validated_finding_call(
        title: &str,
        attack_type: &str,
        confidence: &str,
        evidence: &str,
    ) -> ToolCall {
        let mut call = finding_call(title, attack_type, confidence, evidence);
        let status = if confidence == "confirmed" {
            "confirmed"
        } else {
            "rejected"
        };
        let mut args: serde_json::Value =
            serde_json::from_str(&call.function.arguments).expect("arguments");
        args.as_object_mut().expect("object").insert(
            "_ledger_validation".into(),
            serde_json::json!({"status": status, "resolution_ids": ["res-1"]}),
        );
        call.function.arguments = args.to_string();
        call
    }

    // The core regression: a well-evidenced "confirmed" finding must be recorded as
    // Confirmed even with an EMPTY action_history (the real production state). The old
    // gate silently dropped it here.
    #[tokio::test]
    async fn confirmed_with_evidence_is_recorded_confirmed_in_production_state() {
        let mut gate = SkepticGate;
        let mut state = make_state();
        let call = validated_finding_call(
            "SQL Injection",
            "sqli",
            "confirmed",
            "search endpoint returns database error with OR 1=1",
        );
        let result = ToolResult::success("1", "report_finding", "pending");
        gate.process(&call, &result, &mut state).await;
        assert_eq!(
            state.findings()["SQL Injection"].confidence,
            FindingConfidence::Confirmed
        );
    }

    // A direct model claim without execution-boundary Resolution attestation can
    // never self-promote into the validated zone.
    #[tokio::test]
    async fn unattested_confirmed_claim_is_downgraded_to_candidate() {
        let mut gate = SkepticGate;
        let mut state = make_state();
        let call = finding_call("SQLi", "sqli", "confirmed", "yes");
        let result = ToolResult::success("1", "report_finding", "pending");
        gate.process(&call, &result, &mut state).await;
        let f = &state.findings()["SQLi"];
        assert_eq!(
            f.confidence,
            FindingConfidence::Candidate,
            "only a Runtime-validated Resolution may back Confirmed"
        );
        assert!(
            f.details.contains("lacked Runtime Resolution attestation"),
            "the downgrade reason must be auditable: {}",
            f.details
        );
    }

    #[tokio::test]
    async fn non_confirmed_confidence_records_a_candidate() {
        let mut gate = SkepticGate;
        let mut state = make_state();
        let call = finding_call("Reflected XSS", "xss", "possible", "reflected q param");
        let result = ToolResult::success("1", "report_finding", "pending");
        gate.process(&call, &result, &mut state).await;
        assert_eq!(
            state.findings()["Reflected XSS"].confidence,
            FindingConfidence::Candidate
        );
    }

    #[tokio::test]
    async fn parses_severity_location_and_queues_for_persistence() {
        let mut gate = SkepticGate;
        let mut state = make_state();
        let call = ToolCall {
            id: "call-42".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "report_finding".into(),
                arguments: serde_json::json!({
                    "title": "IDOR on /orders",
                    "attack_type": "idor",
                    "confidence": "confirmed",
                    "severity": "high",
                    "location": "/api/orders/{id}",
                    "evidence": "user A read user B's order via id=1002",
                    "_ledger_validation": {"status":"confirmed","resolution_ids":["res-1"]},
                })
                .to_string(),
            },
        };
        let result = ToolResult::success("call-42", "report_finding", "pending");
        gate.process(&call, &result, &mut state).await;
        let f = &state.findings()["IDOR on /orders"];
        assert_eq!(f.severity, holmes_core::event::Severity::High);
        assert_eq!(f.location, "/api/orders/{id}");
        assert_eq!(f.evidence_source.as_deref(), Some("call-42"));
        // Queued for durable persistence.
        assert_eq!(state.take_pending_findings().len(), 1);
    }

    #[tokio::test]
    async fn confirmed_finding_is_not_downgraded_by_thin_restatement() {
        let mut gate = SkepticGate;
        let mut state = make_state();
        // First: confirmed with strong evidence.
        gate.process(
            &validated_finding_call(
                "SQLi",
                "sqli",
                "confirmed",
                "error-based dump via id=1 OR 1=1",
            ),
            &ToolResult::success("1", "report_finding", "pending"),
            &mut state,
        )
        .await;
        // Later: same title, thin evidence → would become Candidate on its own.
        gate.process(
            &finding_call("SQLi", "sqli", "confirmed", "yes"),
            &ToolResult::success("2", "report_finding", "pending"),
            &mut state,
        )
        .await;
        assert_eq!(
            state.findings()["SQLi"].confidence,
            FindingConfidence::Confirmed,
            "a confirmed finding must not be demoted by a weak restatement"
        );
    }

    #[tokio::test]
    async fn finding_without_title_is_ignored() {
        let mut gate = SkepticGate;
        let mut state = make_state();
        let call = finding_call("", "xss", "possible", "something");
        let result = ToolResult::success("1", "report_finding", "pending");
        gate.process(&call, &result, &mut state).await;
        assert!(state.findings().is_empty());
    }
}
