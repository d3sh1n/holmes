use anyhow::Result;
use serde::Deserialize;
use serde_json::json;

use crate::registry::Tool;
use holmes_core::{FunctionDefinition, ToolDefinition};

pub struct ReportFindingTool;

/// Runtime-only attestation injected after cited Ledger resolutions have been
/// validated. It is deliberately absent from the model-visible JSON schema.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LedgerValidationAttestation {
    pub status: String,
    pub resolution_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FindingReport {
    pub title: String,
    pub attack_type: String,
    pub confidence: String,
    pub evidence: String,
    pub details: Option<String>,
    pub severity: Option<String>,
    pub location: Option<String>,
    pub evidence_source: Option<String>,
    #[serde(default)]
    pub resolution_ids: Vec<String>,
    #[serde(default, rename = "_ledger_validation")]
    pub ledger_validation: Option<LedgerValidationAttestation>,
}

#[async_trait::async_trait]
impl Tool for ReportFindingTool {
    fn name(&self) -> &str {
        "report_finding"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            tool_type: "function".into(),
            function: FunctionDefinition {
                name: "report_finding".into(),
                description: "Report a security finding. Recorded by SkepticGate into the \
                    validated zone and persisted so it survives resume. Provide severity \
                    and location for a usable report."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "title": { "type": "string", "description": "Finding title" },
                        "attack_type": { "type": "string", "description": "Attack type (sqli, xss, idor, etc)" },
                        "confidence": { "type": "string", "enum": ["confirmed", "likely", "possible", "not_vulnerable"], "description": "Confidence level. Use 'not_vulnerable' to record a tested-and-ruled-out negative result (coverage)." },
                        "severity": { "type": "string", "enum": ["critical", "high", "medium", "low", "info"], "description": "Triage severity" },
                        "location": { "type": "string", "description": "Affected endpoint/parameter/component" },
                        "evidence": { "type": "string", "description": "Evidence supporting the finding (request/response excerpt)" },
                        "evidence_source": { "type": "string", "description": "Reference to the tool call / URL that produced the evidence" },
                        "resolution_ids": { "type": "array", "items": {"type":"string"}, "description": "Persisted Ledger Resolution IDs. Required by Runtime for confirmed or not_vulnerable claims." },
                        "details": { "type": "string", "description": "Additional details" }
                    },
                    "required": ["title", "attack_type", "confidence", "evidence"]
                }),
            },
        }
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, args: &str) -> Result<String> {
        let report: FindingReport = serde_json::from_str(args)?;
        Ok(json!({
            "status": "pending_validation",
            "title": report.title,
            "attack_type": report.attack_type,
            "confidence": report.confidence,
        })
        .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::FindingReport;

    #[test]
    fn accepts_only_the_typed_runtime_attestation_as_a_private_field() {
        let report: FindingReport = serde_json::from_str(
            r#"{
                "title":"SQL injection",
                "attack_type":"sqli",
                "confidence":"confirmed",
                "evidence":"request and response differential",
                "resolution_ids":["res-1"],
                "_ledger_validation":{
                    "status":"confirmed",
                    "resolution_ids":["res-1"]
                }
            }"#,
        )
        .expect("Runtime attestation should be accepted");

        let attestation = report
            .ledger_validation
            .expect("Runtime attestation should be decoded");
        assert_eq!(attestation.status, "confirmed");
        assert_eq!(attestation.resolution_ids, ["res-1"]);

        let unknown = serde_json::from_str::<FindingReport>(
            r#"{
                "title":"SQL injection",
                "attack_type":"sqli",
                "confidence":"possible",
                "evidence":"candidate evidence",
                "model_authoritative":true
            }"#,
        );
        assert!(
            unknown.is_err(),
            "arbitrary private fields must be rejected"
        );
    }
}
