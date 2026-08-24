//! Fail-closed PreGuard for the authorized bounty research workflow.
//!
//! Self-disables when no case-level program is active (except `set_program_scope`
//! validation). Does not weaken PermissionPolicy, dangerous_command, or SkepticGate.

use crate::traits::PreGuard;
use holmes_core::bounty::{
    gate_finding, record_asset_or_reject, identifier_in_scope, ProgramScope, ScopeAssetKind,
    ScopeEntry,
};
use holmes_core::state::AttackState;
use holmes_core::{GuardVerdict, ToolCall};

pub struct BountyPreGuard;

fn parse_scope_entries(value: Option<&serde_json::Value>) -> Vec<ScopeEntry> {
    let Some(arr) = value.and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|item| {
            if let Some(s) = item.as_str() {
                return Some(ScopeEntry::new(infer_kind(s), s.to_string()));
            }
            let obj = item.as_object()?;
            let value = obj.get("value").and_then(|v| v.as_str())?.to_string();
            let kind = obj
                .get("kind")
                .and_then(|v| v.as_str())
                .map(parse_kind)
                .unwrap_or_else(|| infer_kind(&value));
            Some(ScopeEntry::new(kind, value))
        })
        .collect()
}

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
    } else if v.starts_with('.') {
        ScopeAssetKind::DomainSuffix
    } else {
        ScopeAssetKind::Host
    }
}

fn program_from_args(parsed: &serde_json::Value) -> Result<ProgramScope, String> {
    let name = parsed
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let in_scope = parse_scope_entries(
        parsed
            .get("in_scope")
            .or_else(|| parsed.get("allow"))
            .or_else(|| parsed.get("assets")),
    );
    let out_of_scope = parse_scope_entries(
        parsed
            .get("out_of_scope")
            .or_else(|| parsed.get("deny")),
    );
    let policy_notes = parsed
        .get("policy_notes")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let allow_private = parsed
        .get("allow_private")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let program = ProgramScope {
        name,
        in_scope,
        out_of_scope,
        policy_notes,
        allow_private,
    };
    program.validate()?;
    Ok(program)
}

#[async_trait::async_trait]
impl PreGuard for BountyPreGuard {
    fn name(&self) -> &str {
        "bounty"
    }

    async fn check(&self, call: &ToolCall, state: &AttackState) -> GuardVerdict {
        let parsed: serde_json::Value = serde_json::from_str(&call.function.arguments)
            .unwrap_or(serde_json::Value::Null);

        match call.function.name.as_str() {
            "set_program_scope" => match program_from_args(&parsed) {
                Ok(_) => GuardVerdict::allow(),
                Err(reason) => GuardVerdict::block(reason),
            },
            "record_asset" => {
                let identifier = parsed
                    .get("identifier")
                    .or_else(|| parsed.get("host"))
                    .or_else(|| parsed.get("url"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                match record_asset_or_reject(state.bounty.program.as_ref(), identifier) {
                    Ok(()) => GuardVerdict::allow(),
                    Err(reason) => GuardVerdict::block(reason),
                }
            }
            "report_recon" => {
                let Some(program) = state.bounty.program.as_ref() else {
                    return GuardVerdict::allow();
                };
                if let Some(assets) = parsed.get("assets").and_then(|v| v.as_array()) {
                    for asset in assets {
                        let identifier = asset
                            .get("identifier")
                            .or_else(|| asset.get("host"))
                            .or_else(|| asset.get("url"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        if identifier.is_empty() {
                            continue;
                        }
                        if !identifier_in_scope(program, identifier) {
                            return GuardVerdict::block(format!(
                                "report_recon: refusing out-of-scope asset '{identifier}'"
                            ));
                        }
                    }
                }
                if let Some(endpoints) = parsed.get("endpoints").and_then(|v| v.as_array()) {
                    for ep in endpoints {
                        let path = ep.get("path").and_then(|v| v.as_str()).unwrap_or("");
                        if path.contains("://") && !identifier_in_scope(program, path) {
                            return GuardVerdict::block(format!(
                                "report_recon: refusing out-of-scope endpoint '{path}'"
                            ));
                        }
                    }
                }
                GuardVerdict::allow()
            }
            "report_finding" => {
                let Some(program) = state.bounty.program.as_ref() else {
                    return GuardVerdict::allow();
                };
                let resolution_ids: Vec<String> = parsed
                    .get("_ledger_validation")
                    .and_then(|v| v.get("resolution_ids"))
                    .or_else(|| parsed.get("resolution_ids"))
                    .and_then(|v| v.as_array())
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(serde_json::Value::as_str)
                            .map(ToOwned::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                let attested = parsed.get("_ledger_validation");
                if attested.is_none() {
                    return GuardVerdict::block(
                        "bounty workflow: report_finding requires Runtime Ledger attestation                          (_ledger_validation) with a verified Resolution ID",
                    );
                }
                // Runtime already checked that cited IDs exist and are verified
                // (confirmed/rejected) when a program is active. PreGuard fail-closes
                // on missing attestation/IDs and out-of-scope assets.
                let status_of = |id: &str| -> Option<String> {
                    if resolution_ids.iter().any(|x| x == id) {
                        Some("confirmed".into())
                    } else {
                        None
                    }
                };
                let asset = parsed
                    .get("affected_asset")
                    .or_else(|| parsed.get("location"))
                    .and_then(|v| v.as_str());
                match gate_finding(Some(program), &resolution_ids, status_of, asset) {
                    Ok(()) => GuardVerdict::allow(),
                    Err(err) => GuardVerdict::block(err.to_string()),
                }
            }
            "generate_bounty_report" => {
                if state.bounty.program.is_none() {
                    return GuardVerdict::block(
                        "bounty workflow: generate_bounty_report requires an active program scope",
                    );
                }
                GuardVerdict::allow()
            }
            _ => GuardVerdict::allow(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holmes_core::bounty::{ProgramScope, ScopeAssetKind, ScopeEntry};
    use holmes_core::FunctionCall;

    fn state_with_program() -> AttackState {
        let mut state = AttackState::new(String::new(), String::new(), "c".into(), "t".into(), vec![]);
        state.bounty.program = Some(ProgramScope {
            name: "Example VDP".into(),
            in_scope: vec![ScopeEntry::new(ScopeAssetKind::DomainSuffix, "example.com")],
            out_of_scope: vec![],
            policy_notes: String::new(),
            allow_private: false,
        });
        state
    }

    fn call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "1".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: name.into(),
                arguments: args.to_string(),
            },
        }
    }

    #[tokio::test]
    async fn blocks_out_of_scope_asset() {
        let g = BountyPreGuard;
        let state = state_with_program();
        let v = g
            .check(
                &call("record_asset", serde_json::json!({"identifier":"evil.com"})),
                &state,
            )
            .await;
        assert!(!v.allowed);
    }

    #[tokio::test]
    async fn blocks_finding_without_resolution() {
        let g = BountyPreGuard;
        let state = state_with_program();
        let v = g
            .check(
                &call(
                    "report_finding",
                    serde_json::json!({
                        "title":"x",
                        "attack_type":"idor",
                        "confidence":"possible",
                        "evidence":"observed differential",
                        "affected_asset":"api.example.com"
                    }),
                ),
                &state,
            )
            .await;
        assert!(!v.allowed, "missing attestation and resolution must block");
    }

    #[tokio::test]
    async fn allows_attested_in_scope_finding() {
        let g = BountyPreGuard;
        let state = state_with_program();
        let v = g
            .check(
                &call(
                    "report_finding",
                    serde_json::json!({
                        "title":"x",
                        "attack_type":"idor",
                        "confidence":"confirmed",
                        "evidence":"observed differential",
                        "affected_asset":"api.example.com",
                        "resolution_ids":["res-1"],
                        "_ledger_validation":{"status":"confirmed","resolution_ids":["res-1"]}
                    }),
                ),
                &state,
            )
            .await;
        assert!(v.allowed, "{}", v.guidance);
    }

    #[tokio::test]
    async fn noop_without_program_for_findings() {
        let g = BountyPreGuard;
        let state = AttackState::new(String::new(), String::new(), "c".into(), "t".into(), vec![]);
        let v = g
            .check(
                &call(
                    "report_finding",
                    serde_json::json!({
                        "title":"x",
                        "attack_type":"idor",
                        "confidence":"possible",
                        "evidence":"candidate"
                    }),
                ),
                &state,
            )
            .await;
        assert!(v.allowed);
    }

    #[tokio::test]
    async fn set_program_requires_name_and_scope() {
        let g = BountyPreGuard;
        let state = AttackState::new(String::new(), String::new(), "c".into(), "t".into(), vec![]);
        let v = g
            .check(
                &call("set_program_scope", serde_json::json!({"name":""})),
                &state,
            )
            .await;
        assert!(!v.allowed);
        let v = g
            .check(
                &call(
                    "set_program_scope",
                    serde_json::json!({
                        "name":"VDP",
                        "in_scope":["example.com"]
                    }),
                ),
                &state,
            )
            .await;
        assert!(v.allowed, "{}", v.guidance);
    }
}
