//! PostGuard that records authorized program scope and in-scope assets into the
//! AttackState free zone. Does not write findings (SkepticGate remains the sole
//! writer of the validated zone).

use crate::traits::PostGuard;
use holmes_core::bounty::{
    identifier_in_scope, record_asset_or_reject, AssetProvenance, DiscoveredAsset, ProgramScope,
    ScopeAssetKind, ScopeEntry,
};
use holmes_core::event::Event;
use holmes_core::state::AttackState;
use holmes_core::{ToolCall, ToolResult};

pub struct BountyPostGuard;

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

fn program_from_args(parsed: &serde_json::Value) -> Option<ProgramScope> {
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
    let out_of_scope = parse_scope_entries(parsed.get("out_of_scope").or_else(|| parsed.get("deny")));
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
    program.validate().ok()?;
    Some(program)
}

fn record_in_scope_asset(state: &mut AttackState, asset: DiscoveredAsset) {
    if record_asset_or_reject(state.bounty.program.as_ref(), &asset.identifier).is_err() {
        return;
    }
    state.pending_bounty_events.push(Event::AssetRecorded {
        asset: asset.clone(),
    });
    state.bounty.upsert_asset(asset);
}

#[async_trait::async_trait]
impl PostGuard for BountyPostGuard {
    fn name(&self) -> &str {
        "bounty"
    }

    async fn process(&mut self, call: &ToolCall, result: &ToolResult, state: &mut AttackState) {
        if !result.is_success() {
            return;
        }
        let parsed: serde_json::Value =
            serde_json::from_str(&call.function.arguments).unwrap_or(serde_json::Value::Null);
        match call.function.name.as_str() {
            "set_program_scope" => {
                if let Some(program) = program_from_args(&parsed) {
                    state.pending_bounty_events.push(Event::ProgramScopeSet {
                        program: program.clone(),
                    });
                    state.bounty.program = Some(program);
                }
            }
            "record_asset" => {
                let identifier = parsed
                    .get("identifier")
                    .or_else(|| parsed.get("host"))
                    .or_else(|| parsed.get("url"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if identifier.is_empty() {
                    return;
                }
                let kind = parsed
                    .get("kind")
                    .and_then(|v| v.as_str())
                    .map(parse_kind)
                    .unwrap_or_else(|| infer_kind(&identifier));
                let notes = parsed
                    .get("how_found")
                    .or_else(|| parsed.get("notes"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let how_found = parsed
                    .get("provenance")
                    .and_then(|v| v.as_str())
                    .map(|s| AssetProvenance::Other(s.to_string()))
                    .unwrap_or(AssetProvenance::RecordAsset);
                record_in_scope_asset(
                    state,
                    DiscoveredAsset::new(identifier, kind, how_found, notes),
                );
            }
            "report_recon" => {
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
                        let kind = asset
                            .get("kind")
                            .and_then(|v| v.as_str())
                            .map(parse_kind)
                            .unwrap_or_else(|| infer_kind(identifier));
                        let notes = asset
                            .get("how_found")
                            .or_else(|| asset.get("notes"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("report_recon")
                            .to_string();
                        record_in_scope_asset(
                            state,
                            DiscoveredAsset::new(
                                identifier,
                                kind,
                                AssetProvenance::ReportRecon,
                                notes,
                            ),
                        );
                    }
                }
                if let Some(endpoints) = parsed.get("endpoints").and_then(|v| v.as_array()) {
                    for ep in endpoints {
                        let path = ep.get("path").and_then(|v| v.as_str()).unwrap_or("");
                        if !path.contains("://") {
                            continue;
                        }
                        let in_scope = state
                            .bounty
                            .program
                            .as_ref()
                            .map(|program| identifier_in_scope(program, path))
                            .unwrap_or(false);
                        if !in_scope {
                            continue;
                        }
                        let notes = ep
                            .get("purpose")
                            .and_then(|v| v.as_str())
                            .unwrap_or("endpoint")
                            .to_string();
                        record_in_scope_asset(
                            state,
                            DiscoveredAsset::new(
                                path,
                                ScopeAssetKind::UrlPrefix,
                                AssetProvenance::ReportRecon,
                                notes,
                            ),
                        );
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holmes_core::bounty::{ProgramScope, ScopeAssetKind, ScopeEntry};
    use holmes_core::FunctionCall;

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
    async fn records_in_scope_asset_with_provenance() {
        let mut gate = BountyPostGuard;
        let mut state =
            AttackState::new(String::new(), String::new(), "c".into(), "t".into(), vec![]);
        state.bounty.program = Some(ProgramScope {
            name: "VDP".into(),
            in_scope: vec![ScopeEntry::new(ScopeAssetKind::DomainSuffix, "example.com")],
            out_of_scope: vec![],
            policy_notes: String::new(),
            allow_private: false,
        });
        gate.process(
            &call(
                "record_asset",
                serde_json::json!({
                    "identifier":"api.example.com",
                    "how_found":"dns enumeration"
                }),
            ),
            &ToolResult::success("1", "record_asset", "ok"),
            &mut state,
        )
        .await;
        assert_eq!(state.bounty.assets.len(), 1);
        assert_eq!(state.bounty.assets[0].identifier, "api.example.com");
        assert_eq!(
            state.bounty.assets[0].how_found,
            AssetProvenance::RecordAsset
        );
        assert_eq!(state.pending_bounty_events.len(), 1);
    }

    #[tokio::test]
    async fn refuses_out_of_scope_asset() {
        let mut gate = BountyPostGuard;
        let mut state =
            AttackState::new(String::new(), String::new(), "c".into(), "t".into(), vec![]);
        state.bounty.program = Some(ProgramScope {
            name: "VDP".into(),
            in_scope: vec![ScopeEntry::new(ScopeAssetKind::DomainSuffix, "example.com")],
            out_of_scope: vec![],
            policy_notes: String::new(),
            allow_private: false,
        });
        gate.process(
            &call("record_asset", serde_json::json!({"identifier":"evil.com"})),
            &ToolResult::success("1", "record_asset", "ok"),
            &mut state,
        )
        .await;
        assert!(state.bounty.assets.is_empty());
    }
}
