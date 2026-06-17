use anyhow::Result;
use serde::Deserialize;
use serde_json::json;

use crate::registry::Tool;
use holmes_core::{FunctionDefinition, ToolDefinition};

pub struct ReportReconTool;

#[derive(Debug, Deserialize)]
pub struct ReconReport {
    pub tech_stack: Vec<String>,
    pub auth_mechanism: String,
    pub endpoints: Vec<EndpointInfo>,
    pub interesting_findings: Vec<String>,
    pub attack_hypotheses: Vec<HypothesisProposal>,
}

#[derive(Debug, Deserialize)]
pub struct EndpointInfo {
    pub path: String,
    pub method: String,
    pub purpose: String,
    #[serde(default)]
    pub params: Vec<String>,
    #[serde(default)]
    pub auth_required: bool,
}

#[derive(Debug, Deserialize)]
pub struct HypothesisProposal {
    pub attack_type: String,
    pub target: String,
    pub confidence: String,
    pub reason: String,
    pub validation_plan: String,
}

#[async_trait::async_trait]
impl Tool for ReportReconTool {
    fn name(&self) -> &str {
        "report_recon"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            tool_type: "function".into(),
            function: FunctionDefinition {
                name: "report_recon".into(),
                description: "Submit structured reconnaissance report. Call this after completing initial recon to transition to hypothesis-driven attack phase. Include all discovered endpoints, tech stack, and ranked attack hypotheses.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "tech_stack": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Identified technologies (e.g. Flask, PHP/7.4, nginx)"
                        },
                        "auth_mechanism": {
                            "type": "string",
                            "description": "Authentication mechanism description (e.g. cookie-based two-step login)"
                        },
                        "endpoints": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "path": { "type": "string" },
                                    "method": { "type": "string" },
                                    "purpose": { "type": "string" },
                                    "params": { "type": "array", "items": { "type": "string" } },
                                    "auth_required": { "type": "boolean" }
                                },
                                "required": ["path", "method", "purpose"]
                            },
                            "description": "All discovered endpoints with their purpose"
                        },
                        "interesting_findings": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Notable observations (e.g. test credentials in HTML comments, soft-404 behavior)"
                        },
                        "attack_hypotheses": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "attack_type": { "type": "string", "description": "sqli|xss|idor|ssrf|rce|lfi|auth_bypass|session_forgery|crypto" },
                                    "target": { "type": "string", "description": "Specific endpoint or parameter" },
                                    "confidence": { "type": "string", "enum": ["high", "medium", "low"] },
                                    "reason": { "type": "string" },
                                    "validation_plan": { "type": "string", "description": "Specific steps to confirm or reject this hypothesis" }
                                },
                                "required": ["attack_type", "target", "confidence", "reason", "validation_plan"]
                            },
                            "description": "Ranked attack hypotheses with validation plans"
                        }
                    },
                    "required": ["tech_stack", "auth_mechanism", "endpoints", "interesting_findings", "attack_hypotheses"]
                }),
            },
        }
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, args: &str) -> Result<String> {
        let report: ReconReport = serde_json::from_str(args)?;

        let mut summary = Vec::new();
        summary.push("侦察报告已接收。".to_string());
        summary.push(format!("技术栈: {}", report.tech_stack.join(", ")));
        summary.push(format!("端点数: {}", report.endpoints.len()));
        summary.push(format!("攻击假设数: {}", report.attack_hypotheses.len()));

        if !report.attack_hypotheses.is_empty() {
            summary.push("假设优先级:".into());
            for (i, h) in report.attack_hypotheses.iter().enumerate() {
                summary.push(format!(
                    "  {}. [{}][{}] {} — {}",
                    i + 1,
                    h.confidence,
                    h.attack_type,
                    h.target,
                    h.reason
                ));
            }
            summary.push(
                "系统将按优先级逐一验证假设。每个假设有固定尝试预算，超出后自动切换到下一个。"
                    .into(),
            );
        }

        Ok(summary.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn execute_parses_valid_report() {
        let tool = ReportReconTool;
        let args = serde_json::json!({
            "tech_stack": ["Flask", "Python"],
            "auth_mechanism": "cookie-based",
            "endpoints": [
                {"path": "/login", "method": "POST", "purpose": "user login"}
            ],
            "interesting_findings": ["test creds in HTML"],
            "attack_hypotheses": [
                {
                    "attack_type": "sqli",
                    "target": "/search",
                    "confidence": "high",
                    "reason": "unfiltered input",
                    "validation_plan": "inject ' OR 1=1"
                }
            ]
        })
        .to_string();

        let result = tool.execute(&args).await.unwrap();
        assert!(result.contains("Flask"));
        assert!(result.contains("sqli"));
    }

    #[tokio::test]
    async fn execute_rejects_invalid_json() {
        let tool = ReportReconTool;
        let result = tool.execute("not json").await;
        assert!(result.is_err());
    }
}
