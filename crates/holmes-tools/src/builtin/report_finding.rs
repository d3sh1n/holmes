use anyhow::Result;
use serde::Deserialize;
use serde_json::json;

use crate::registry::Tool;
use holmes_core::{FunctionDefinition, ToolDefinition};

pub struct ReportFindingTool;

#[derive(Debug, Deserialize)]
pub struct FindingReport {
    pub title: String,
    pub attack_type: String,
    pub confidence: String,
    pub evidence: String,
    pub details: Option<String>,
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
                description: "Report a security finding. Validated by SkepticGate PostGuard."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "title": { "type": "string", "description": "Finding title" },
                        "attack_type": { "type": "string", "description": "Attack type (sqli, xss, idor, etc)" },
                        "confidence": { "type": "string", "enum": ["confirmed", "likely", "possible"], "description": "Confidence level" },
                        "evidence": { "type": "string", "description": "Evidence supporting the finding" },
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
