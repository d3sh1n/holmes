//! Authorized bounty / VDP research tools.
//!
//! Tools are argument validators + acknowledgements. Case state (program,
//! assets, reports) is applied by BountyPostGuard / BountyWorkflowMiddleware so
//! the Tool trait stays free of AttackState. No exploit content.

use anyhow::{bail, Result};
use serde::Deserialize;
use serde_json::json;

use crate::registry::Tool;
use holmes_core::bounty::{ProgramScope, ScopeAssetKind, ScopeEntry};
use holmes_core::{FunctionDefinition, ToolDefinition};

fn parse_kind(s: &str) -> ScopeAssetKind {
    match s.to_lowercase().as_str() {
        "host" => ScopeAssetKind::Host,
        "domain_suffix" | "suffix" | "domain" => ScopeAssetKind::DomainSuffix,
        "url_prefix" | "url" | "prefix" => ScopeAssetKind::UrlPrefix,
        "cidr" => ScopeAssetKind::Cidr,
        _ => ScopeAssetKind::Host,
    }
}

fn infer_kind(value: &str) -> ScopeAssetKind {
    let v = value.trim();
    if v.contains("://") || v.starts_with('/') {
        ScopeAssetKind::UrlPrefix
    } else if v.contains('/') {
        ScopeAssetKind::Cidr
    } else {
        ScopeAssetKind::Host
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ScopeEntryArg {
    String(String),
    Object { kind: Option<String>, value: String },
}

impl ScopeEntryArg {
    fn into_entry(self) -> ScopeEntry {
        match self {
            Self::String(value) => ScopeEntry::new(infer_kind(&value), value),
            Self::Object { kind, value } => {
                let kind = kind.as_deref().map(parse_kind).unwrap_or_else(|| infer_kind(&value));
                ScopeEntry::new(kind, value)
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct SetProgramArgs {
    name: String,
    #[serde(default, alias = "allow", alias = "assets")]
    in_scope: Vec<ScopeEntryArg>,
    #[serde(default, alias = "deny")]
    out_of_scope: Vec<ScopeEntryArg>,
    #[serde(default)]
    policy_notes: String,
    #[serde(default)]
    allow_private: bool,
}

impl SetProgramArgs {
    fn into_program(self) -> Result<ProgramScope> {
        let program = ProgramScope {
            name: self.name,
            in_scope: self.in_scope.into_iter().map(ScopeEntryArg::into_entry).collect(),
            out_of_scope: self
                .out_of_scope
                .into_iter()
                .map(ScopeEntryArg::into_entry)
                .collect(),
            policy_notes: self.policy_notes,
            allow_private: self.allow_private,
        };
        program.validate().map_err(|e| anyhow::anyhow!(e))?;
        Ok(program)
    }
}

pub struct SetProgramScopeTool;

#[async_trait::async_trait]
impl Tool for SetProgramScopeTool {
    fn name(&self) -> &str {
        "set_program_scope"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            tool_type: "function".into(),
            function: FunctionDefinition {
                name: "set_program_scope".into(),
                description: "Attach an authorized bug-bounty / VDP program to this case. \
                    Required: program name and at least one in-scope asset (host, domain suffix, \
                    URL prefix, or CIDR). Out-of-scope entries and policy notes are optional. \
                    While a program is active, the heuristic scope guard is driven from it \
                    (same matching as guards.scope: host / suffix / CIDR, plus URL prefixes). \
                    Authorized programs only — never set scope for unauthorized testing."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "name": { "type": "string", "description": "Program / VDP name" },
                        "in_scope": {
                            "type": "array",
                            "description": "In-scope assets. Strings or {kind,value} objects. kind: host | domain_suffix | url_prefix | cidr.",
                            "items": {
                                "oneOf": [
                                    { "type": "string" },
                                    {
                                        "type": "object",
                                        "properties": {
                                            "kind": { "type": "string" },
                                            "value": { "type": "string" }
                                        },
                                        "required": ["value"]
                                    }
                                ]
                            }
                        },
                        "out_of_scope": {
                            "type": "array",
                            "items": {
                                "oneOf": [
                                    { "type": "string" },
                                    {
                                        "type": "object",
                                        "properties": {
                                            "kind": { "type": "string" },
                                            "value": { "type": "string" }
                                        },
                                        "required": ["value"]
                                    }
                                ]
                            }
                        },
                        "policy_notes": { "type": "string" },
                        "allow_private": { "type": "boolean" }
                    },
                    "required": ["name", "in_scope"]
                }),
            },
        }
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn execute(&self, args: &str) -> Result<String> {
        let parsed: SetProgramArgs = serde_json::from_str(args)?;
        let program = parsed.into_program()?;
        Ok(json!({
            "status": "accepted",
            "program": program.name,
            "in_scope": program.in_scope,
            "out_of_scope": program.out_of_scope,
            "note": "Case state is updated by the bounty post-guard. Resume reloads this program from the event log."
        })
        .to_string())
    }
}

pub struct GetProgramScopeTool;

#[async_trait::async_trait]
impl Tool for GetProgramScopeTool {
    fn name(&self) -> &str {
        "get_program_scope"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            tool_type: "function".into(),
            function: FunctionDefinition {
                name: "get_program_scope".into(),
                description: "Return the authorized bounty/VDP program attached to this case, \
                    or report that none is active."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {}
                }),
            },
        }
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, _args: &str) -> Result<String> {
        Ok(json!({
            "status": "ok",
            "materialize": "get_program_scope"
        })
        .to_string())
    }
}

#[derive(Debug, Deserialize)]
struct RecordAssetArgs {
    identifier: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    how_found: Option<String>,
    #[serde(default)]
    notes: Option<String>,
    #[serde(default)]
    provenance: Option<String>,
}

pub struct RecordAssetTool;

#[async_trait::async_trait]
impl Tool for RecordAssetTool {
    fn name(&self) -> &str {
        "record_asset"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            tool_type: "function".into(),
            function: FunctionDefinition {
                name: "record_asset".into(),
                description: "Record an IN-SCOPE discovered asset with provenance (how it was \
                    found). Out-of-scope identifiers are refused. Requires an active program."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "identifier": { "type": "string", "description": "Host, URL, suffix, or CIDR" },
                        "kind": { "type": "string", "enum": ["host", "domain_suffix", "url_prefix", "cidr"] },
                        "how_found": { "type": "string", "description": "Provenance: how this asset was discovered" },
                        "notes": { "type": "string" },
                        "provenance": { "type": "string" }
                    },
                    "required": ["identifier"]
                }),
            },
        }
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn execute(&self, args: &str) -> Result<String> {
        let parsed: RecordAssetArgs = serde_json::from_str(args)?;
        if parsed.identifier.trim().is_empty() {
            bail!("identifier is required");
        }
        Ok(json!({
            "status": "accepted",
            "identifier": parsed.identifier,
            "kind": parsed.kind,
            "how_found": parsed.how_found.or(parsed.notes),
            "provenance": parsed.provenance,
            "note": "Inventory is updated by the bounty post-guard only if the asset is in scope."
        })
        .to_string())
    }
}

pub struct ListAssetsTool;

#[async_trait::async_trait]
impl Tool for ListAssetsTool {
    fn name(&self) -> &str {
        "list_assets"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            tool_type: "function".into(),
            function: FunctionDefinition {
                name: "list_assets".into(),
                description: "List in-scope assets recorded for this case, with provenance."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {}
                }),
            },
        }
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, _args: &str) -> Result<String> {
        Ok(json!({
            "status": "ok",
            "materialize": "list_assets"
        })
        .to_string())
    }
}

#[derive(Debug, Deserialize)]
struct GenerateReportArgs {
    #[serde(default)]
    allow_no_findings: bool,
}

pub struct GenerateBountyReportTool;

#[async_trait::async_trait]
impl Tool for GenerateBountyReportTool {
    fn name(&self) -> &str {
        "generate_bounty_report"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            tool_type: "function".into(),
            function: FunctionDefinition {
                name: "generate_bounty_report".into(),
                description: "Generate a bounty-ready markdown report from in-scope, \
                    ledger-verified findings only. Includes summary, program scope, affected \
                    assets, business/security impact, evidence appendix, and high-level \
                    remediation. Reproduction is high-level (what was observed + evidence IDs) \
                    — never step-by-step attack or payload. Set allow_no_findings=true for an \
                    explicit empty report when the operator asks."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "allow_no_findings": {
                            "type": "boolean",
                            "description": "If true, emit an explicit no-findings report when the inventory has no verified in-scope findings."
                        }
                    }
                }),
            },
        }
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, args: &str) -> Result<String> {
        let parsed: GenerateReportArgs = if args.trim().is_empty() {
            GenerateReportArgs {
                allow_no_findings: false,
            }
        } else {
            serde_json::from_str(args)?
        };
        Ok(json!({
            "status": "ok",
            "materialize": "generate_bounty_report",
            "allow_no_findings": parsed.allow_no_findings
        })
        .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn set_program_rejects_empty_scope() {
        let tool = SetProgramScopeTool;
        let err = tool
            .execute(r#"{"name":"VDP","in_scope":[]}"#)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("in-scope"));
    }

    #[tokio::test]
    async fn set_program_accepts_authorized_program() {
        let tool = SetProgramScopeTool;
        let out = tool
            .execute(r#"{"name":"Example VDP","in_scope":["example.com"],"policy_notes":"authorized only"}"#)
            .await
            .unwrap();
        assert!(out.contains("Example VDP"));
        assert!(out.contains("accepted"));
    }

    #[tokio::test]
    async fn record_asset_requires_identifier() {
        let tool = RecordAssetTool;
        let err = tool.execute(r#"{"identifier":"  "}"#).await.unwrap_err();
        assert!(err.to_string().contains("identifier"));
    }

    #[tokio::test]
    async fn generate_report_echoes_empty_flag() {
        let tool = GenerateBountyReportTool;
        let out = tool
            .execute(r#"{"allow_no_findings":true}"#)
            .await
            .unwrap();
        assert!(out.contains("allow_no_findings"));
        assert!(out.contains("true"));
    }
}
