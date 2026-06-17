use anyhow::{Context, Result};
use serde_json::json;

use crate::registry::Tool;
use holmes_core::{FunctionDefinition, ToolDefinition};

pub struct AddHypothesisTool;
pub struct RejectHypothesisTool;
pub struct ConfirmHypothesisTool;

#[async_trait::async_trait]
impl Tool for AddHypothesisTool {
    fn name(&self) -> &str {
        "add_hypothesis"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            tool_type: "function".into(),
            function: FunctionDefinition {
                name: "add_hypothesis".into(),
                description: "添加一个攻击假设。在 HYPOTHESIZE 阶段使用，记录你对目标的攻击猜想。"
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "attack_type": {
                            "type": "string",
                            "description": "攻击类型 (sqli/xss/idor/ssrf/rce/lfi/upload/auth_bypass/crypto)"
                        },
                        "target": {
                            "type": "string",
                            "description": "目标入口点 (URL path 或参数名)"
                        },
                        "confidence": {
                            "type": "string",
                            "enum": ["high", "medium", "low"],
                            "description": "置信度"
                        },
                        "description": {
                            "type": "string",
                            "description": "假设描述"
                        },
                        "validation_plan": {
                            "type": "string",
                            "description": "验证方法 — 如何确认漏洞存在"
                        }
                    },
                    "required": ["attack_type", "target", "confidence", "description", "validation_plan"]
                }),
            },
        }
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn execute(&self, args: &str) -> Result<String> {
        let v: serde_json::Value = serde_json::from_str(args)?;
        let attack_type = v["attack_type"].as_str().context("missing attack_type")?;
        let target = v["target"].as_str().context("missing target")?;
        let confidence = v["confidence"].as_str().unwrap_or("medium");
        let description = v["description"].as_str().context("missing description")?;
        let validation_plan = v["validation_plan"]
            .as_str()
            .context("missing validation_plan")?;
        Ok(json!({
            "status": "registered",
            "attack_type": attack_type,
            "target": target,
            "confidence": confidence,
            "description": description,
            "validation_plan": validation_plan,
        })
        .to_string())
    }
}

#[async_trait::async_trait]
impl Tool for RejectHypothesisTool {
    fn name(&self) -> &str {
        "reject_hypothesis"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            tool_type: "function".into(),
            function: FunctionDefinition {
                name: "reject_hypothesis".into(),
                description:
                    "否定当前活跃的攻击假设。验证失败时调用，系统会自动激活下一个待验证假设。"
                        .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "reason": {
                            "type": "string",
                            "description": "否定原因 — 为什么这个假设不成立"
                        }
                    },
                    "required": ["reason"]
                }),
            },
        }
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn execute(&self, args: &str) -> Result<String> {
        let v: serde_json::Value = serde_json::from_str(args)?;
        let reason = v["reason"].as_str().context("missing reason")?;
        Ok(json!({
            "status": "rejected",
            "reason": reason,
        })
        .to_string())
    }
}

#[async_trait::async_trait]
impl Tool for ConfirmHypothesisTool {
    fn name(&self) -> &str {
        "confirm_hypothesis"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            tool_type: "function".into(),
            function: FunctionDefinition {
                name: "confirm_hypothesis".into(),
                description: "确认当前活跃的攻击假设。漏洞验证成功时调用。".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "evidence": {
                            "type": "string",
                            "description": "确认证据 — 证明漏洞存在的具体响应或行为"
                        }
                    },
                    "required": ["evidence"]
                }),
            },
        }
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn execute(&self, args: &str) -> Result<String> {
        let v: serde_json::Value = serde_json::from_str(args)?;
        let evidence = v["evidence"].as_str().context("missing evidence")?;
        Ok(json!({
            "status": "confirmed",
            "evidence": evidence,
        })
        .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn add_hypothesis_validates_fields() {
        let tool = AddHypothesisTool;
        let args = json!({
            "attack_type": "xss",
            "target": "/search?q=",
            "confidence": "high",
            "description": "reflected XSS in search param",
            "validation_plan": "inject <script>alert(1)</script>"
        })
        .to_string();
        let result = tool.execute(&args).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["status"], "registered");
        assert_eq!(v["attack_type"], "xss");
    }

    #[tokio::test]
    async fn add_hypothesis_missing_field_errors() {
        let tool = AddHypothesisTool;
        let args = json!({"attack_type": "xss"}).to_string();
        assert!(tool.execute(&args).await.is_err());
    }

    #[tokio::test]
    async fn reject_hypothesis_returns_reason() {
        let tool = RejectHypothesisTool;
        let args = json!({"reason": "no error response"}).to_string();
        let result = tool.execute(&args).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["status"], "rejected");
        assert_eq!(v["reason"], "no error response");
    }

    #[tokio::test]
    async fn confirm_hypothesis_returns_evidence() {
        let tool = ConfirmHypothesisTool;
        let args = json!({"evidence": "alert(1) fired"}).to_string();
        let result = tool.execute(&args).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["status"], "confirmed");
    }
}
