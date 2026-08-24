//! Runtime glue for the authorized bounty research workflow.
//!
//! Materializes get/list/report tool results from AttackState + validated
//! findings + Ledger evidence IDs. Does not write the validated findings zone.

use crate::context::RuntimeContext;
use crate::deliberation::RuntimeError;
use crate::middleware::RuntimeMiddleware;
use async_trait::async_trait;
use holmes_core::bounty::{
    render_bounty_report, reportable_findings, BountyReportInput,
};
use holmes_core::event::{Event, ReportGenerator, ReportType};
use holmes_core::tool_types::ContentBlock;
use holmes_core::ToolResult;

pub struct BountyWorkflowMiddleware;

fn rewrite_text(result: &mut ToolResult, text: String) {
    result.content = vec![ContentBlock::Text(text)];
}

#[async_trait]
impl RuntimeMiddleware for BountyWorkflowMiddleware {
    async fn after_tool_call(
        &self,
        ctx: &mut RuntimeContext,
        result: &mut ToolResult,
    ) -> Result<(), RuntimeError> {
        if !result.is_success() {
            return Ok(());
        }
        match result.tool_name.as_str() {
            "get_program_scope" => {
                let body = match &ctx.state.compatibility_state.bounty.program {
                    Some(program) => serde_json::json!({
                        "status": "ok",
                        "active": true,
                        "program": program,
                    }),
                    None => serde_json::json!({
                        "status": "ok",
                        "active": false,
                        "note": "No authorized program is attached. Call set_program_scope first."
                    }),
                };
                rewrite_text(result, body.to_string());
            }
            "list_assets" => {
                let body = serde_json::json!({
                    "status": "ok",
                    "assets": ctx.state.compatibility_state.bounty.assets,
                });
                rewrite_text(result, body.to_string());
            }
            "generate_bounty_report" => {
                let allow_no_findings = result
                    .text_content()
                    .parse::<serde_json::Value>()
                    .ok()
                    .and_then(|v| v.get("allow_no_findings").and_then(|x| x.as_bool()))
                    .unwrap_or(false);
                let bounty = &ctx.state.compatibility_state.bounty;
                let program = bounty.program.as_ref();
                let findings = reportable_findings(
                    program,
                    ctx.state.compatibility_state.findings().values(),
                );
                match render_bounty_report(BountyReportInput {
                    program,
                    assets: &bounty.assets,
                    findings,
                    allow_no_findings,
                    generated_at: chrono::Utc::now(),
                }) {
                    Ok(markdown) => {
                        ctx.state.compatibility_state.bounty.last_report = Some(markdown.clone());
                        ctx.state
                            .compatibility_state
                            .pending_bounty_events
                            .push(Event::ReportGenerated {
                                report_type: ReportType::VulnerabilityReport,
                                file_path: "bounty-report.md".into(),
                                sections: vec![
                                    "summary".into(),
                                    "program_scope".into(),
                                    "affected_assets".into(),
                                    "findings".into(),
                                    "evidence".into(),
                                    "remediation".into(),
                                ],
                                generated_by: ReportGenerator::Agent,
                            });
                        rewrite_text(result, markdown);
                    }
                    Err(reason) => {
                        *result = ToolResult::error(
                            result.tool_call_id.clone(),
                            result.tool_name.clone(),
                            reason,
                        );
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holmes_core::bounty::{
        DiscoveredAsset, EvidenceArtifacts, ProgramScope, ScopeAssetKind, ScopeEntry,
        AssetProvenance,
    };
    use holmes_core::event::Severity;
    use holmes_core::state::validated::{Finding, FindingConfidence};
    use holmes_core::state::AttackState;

    fn program() -> ProgramScope {
        ProgramScope {
            name: "Example VDP".into(),
            in_scope: vec![ScopeEntry::new(ScopeAssetKind::DomainSuffix, "example.com")],
            out_of_scope: vec![],
            policy_notes: "authorized only".into(),
            allow_private: false,
        }
    }

    #[test]
    fn report_renderer_filters_to_verified_in_scope() {
        let program = program();
        let mut state =
            AttackState::new(String::new(), String::new(), "c".into(), "t".into(), vec![]);
        state.bounty.program = Some(program.clone());
        state.bounty.upsert_asset(DiscoveredAsset::new(
            "api.example.com",
            ScopeAssetKind::Host,
            AssetProvenance::ReportRecon,
            "seen during recon",
        ));
        let good = Finding {
            id: "IDOR".into(),
            finding_type: "idor".into(),
            confidence: FindingConfidence::Confirmed,
            evidence: "Observed another user's object at the same endpoint.".into(),
            details: "Missing object-level authorization.".into(),
            attack_type: "idor".into(),
            severity: Severity::High,
            location: "https://api.example.com/users".into(),
            evidence_source: Some("call-1".into()),
            resolution_ids: vec!["res-1".into()],
            affected_asset: Some("https://api.example.com/users".into()),
            evidence_artifacts: EvidenceArtifacts {
                ledger_evidence_ids: vec!["ev-1".into()],
                ..EvidenceArtifacts::default()
            },
        };
        let oos = Finding {
            id: "Out of scope".into(),
            finding_type: "xss".into(),
            confidence: FindingConfidence::Confirmed,
            evidence: "n/a".into(),
            details: String::new(),
            attack_type: "xss".into(),
            severity: Severity::Low,
            location: "https://evil.com/".into(),
            evidence_source: None,
            resolution_ids: vec!["res-2".into()],
            affected_asset: Some("https://evil.com/".into()),
            evidence_artifacts: EvidenceArtifacts::default(),
        };
        state.record_finding(good);
        state.record_finding(oos);
        let selected = holmes_core::bounty::reportable_findings(
            state.bounty.program.as_ref(),
            state.findings().values(),
        );
        let md = render_bounty_report(BountyReportInput {
            program: state.bounty.program.as_ref(),
            assets: &state.bounty.assets,
            findings: selected,
            allow_no_findings: false,
            generated_at: chrono::Utc::now(),
        })
        .unwrap();
        assert!(md.contains("IDOR"));
        assert!(md.contains("seen during recon") || md.contains("api.example.com"));
        assert!(!md.contains("evil.com"));
        assert!(md.contains("res-1"));
        let _ = BountyWorkflowMiddleware;
    }
}
