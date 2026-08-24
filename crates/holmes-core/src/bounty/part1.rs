use crate::config::ScopeConfig;
use crate::state::validated::{Finding, FindingConfidence};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;

/// How an in-scope asset was discovered. Provenance for the inventory, not a
/// reproduction recipe.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AssetProvenance {
    OperatorSupplied,
    ReportRecon,
    RecordAsset,
    HttpResponse,
    BrowserNavigate,
    Other(String),
}

impl AssetProvenance {
    pub fn label(&self) -> String {
        match self {
            Self::OperatorSupplied => "operator".into(),
            Self::ReportRecon => "report_recon".into(),
            Self::RecordAsset => "record_asset".into(),
            Self::HttpResponse => "http_response".into(),
            Self::BrowserNavigate => "browser_navigate".into(),
            Self::Other(s) => s.clone(),
        }
    }
}

/// Kind of a program-scope entry or discovered asset identifier.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScopeAssetKind {
    Host,
    DomainSuffix,
    UrlPrefix,
    Cidr,
}

/// A single in-scope or out-of-scope program entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScopeEntry {
    pub kind: ScopeAssetKind,
    pub value: String,
}

impl ScopeEntry {
    pub fn new(kind: ScopeAssetKind, value: impl Into<String>) -> Self {
        Self {
            kind,
            value: value.into(),
        }
    }

    pub fn normalized(&self) -> Self {
        Self {
            kind: self.kind,
            value: normalize_entry(&self.value),
        }
    }
}

/// Authorized bounty / VDP program attached to a case/session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProgramScope {
    pub name: String,
    #[serde(default)]
    pub in_scope: Vec<ScopeEntry>,
    #[serde(default)]
    pub out_of_scope: Vec<ScopeEntry>,
    #[serde(default)]
    pub policy_notes: String,
    /// Permit private/loopback/metadata addresses that otherwise match in-scope
    /// entries. Off by default (same heuristic as `guards.scope.allow_private`).
    #[serde(default)]
    pub allow_private: bool,
}

impl ProgramScope {
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("program name is required".into());
        }
        if self.in_scope.is_empty() {
            return Err("program must declare at least one in-scope asset".into());
        }
        for entry in self.in_scope.iter().chain(self.out_of_scope.iter()) {
            if entry.value.trim().is_empty() {
                return Err("scope entries cannot be empty".into());
            }
        }
        Ok(())
    }

    /// Hosts / suffixes / CIDRs (plus hosts extracted from URL prefixes) used by
    /// the heuristic scope guard.
    pub fn allow_hosts(&self) -> Vec<String> {
        let mut out = Vec::new();
        for entry in &self.in_scope {
            match entry.kind {
                ScopeAssetKind::Host | ScopeAssetKind::DomainSuffix | ScopeAssetKind::Cidr => {
                    out.push(normalize_entry(&entry.value));
                }
                ScopeAssetKind::UrlPrefix => {
                    if let Some(host) = host_of_url(&entry.value) {
                        out.push(normalize_entry(&host));
                    }
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }

    pub fn deny_hosts(&self) -> Vec<String> {
        let mut out = Vec::new();
        for entry in &self.out_of_scope {
            // URL prefixes deny paths, not the whole host.
            match entry.kind {
                ScopeAssetKind::Host | ScopeAssetKind::DomainSuffix | ScopeAssetKind::Cidr => {
                    out.push(normalize_entry(&entry.value));
                }
                ScopeAssetKind::UrlPrefix => {}
            }
        }
        out.sort();
        out.dedup();
        out
    }

    pub fn url_prefixes(&self) -> Vec<String> {
        self.in_scope
            .iter()
            .filter(|e| e.kind == ScopeAssetKind::UrlPrefix)
            .map(|e| normalize_url_prefix(&e.value))
            .filter(|s| !s.is_empty())
            .collect()
    }

    pub fn deny_url_prefixes(&self) -> Vec<String> {
        self.out_of_scope
            .iter()
            .filter(|e| e.kind == ScopeAssetKind::UrlPrefix)
            .map(|e| normalize_url_prefix(&e.value))
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// Config-shaped view of this program for the existing heuristic ScopeGuard.
    pub fn to_scope_config(&self) -> ScopeConfig {
        ScopeConfig {
            allow: self.allow_hosts(),
            deny: self.deny_hosts(),
            allow_private: self.allow_private,
        }
    }
}

/// An in-scope asset recorded in the case inventory.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscoveredAsset {
    pub id: String,
    pub identifier: String,
    pub kind: ScopeAssetKind,
    pub how_found: AssetProvenance,
    #[serde(default)]
    pub notes: String,
    pub recorded_at: DateTime<Utc>,
}

impl DiscoveredAsset {
    pub fn new(
        identifier: impl Into<String>,
        kind: ScopeAssetKind,
        how_found: AssetProvenance,
        notes: impl Into<String>,
    ) -> Self {
        let identifier = identifier.into();
        let id = format!(
            "asset-{}",
            crate::content_hash(&identifier).chars().take(12).collect::<String>()
        );
        Self {
            id,
            identifier,
            kind,
            how_found,
            notes: notes.into(),
            recorded_at: Utc::now(),
        }
    }
}

/// Artifacts already collected that back a finding. Not an exploit recipe.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct EvidenceArtifacts {
    #[serde(default)]
    pub ledger_evidence_ids: Vec<String>,
    #[serde(default)]
    pub screenshot_paths: Vec<String>,
    #[serde(default)]
    pub request_response_hashes: Vec<String>,
    #[serde(default)]
    pub log_excerpts: Vec<String>,
}

impl EvidenceArtifacts {
    pub fn is_empty(&self) -> bool {
        self.ledger_evidence_ids.is_empty()
            && self.screenshot_paths.is_empty()
            && self.request_response_hashes.is_empty()
            && self.log_excerpts.is_empty()
    }
}

/// Free-zone case sidecar: program + inventory. Findings stay in the validated
/// zone (SkepticGate is the sole writer there).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BountyCase {
    pub program: Option<ProgramScope>,
    #[serde(default)]
    pub assets: Vec<DiscoveredAsset>,
    #[serde(default)]
    pub last_report: Option<String>,
}

impl BountyCase {
    pub fn upsert_asset(&mut self, asset: DiscoveredAsset) {
        if let Some(existing) = self
            .assets
            .iter_mut()
            .find(|a| a.identifier == asset.identifier)
        {
            if existing.notes.is_empty() && !asset.notes.is_empty() {
                existing.notes = asset.notes;
            }
            return;
        }
        self.assets.push(asset);
    }
}
