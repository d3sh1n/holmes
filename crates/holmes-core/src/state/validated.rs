use crate::event::Severity;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum FindingConfidence {
    #[default]
    Candidate,
    Confirmed,
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Finding {
    pub id: String,
    pub finding_type: String,
    pub confidence: FindingConfidence,
    pub evidence: String,
    pub details: String,
    pub attack_type: String,
    /// Triage severity (defaults to `Info` until the model scores it).
    #[serde(default)]
    pub severity: Severity,
    /// Where the issue lives — endpoint / parameter / component. Empty until set.
    #[serde(default)]
    pub location: String,
    /// The tool call id whose raw request/response evidences this finding, linking the
    /// finding back to the reproducible transaction (the blob-offloaded ToolResult
    /// payload, restorable from the `blobs` tables alone).
    #[serde(default)]
    pub evidence_source: Option<String>,
    /// Verified Hypothesis Ledger Resolution IDs that back this finding.
    #[serde(default)]
    pub resolution_ids: Vec<String>,
    /// In-scope asset this finding affects (host / URL). Defaults to `location`.
    #[serde(default)]
    pub affected_asset: Option<String>,
    /// Already-collected artifacts (ledger evidence IDs, screenshots, hashes, logs).
    /// Not an exploit recipe.
    #[serde(default)]
    pub evidence_artifacts: crate::bounty::EvidenceArtifacts,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttackHypothesis {
    pub attack_type: String,
    pub confidence: f32,
    pub reasoning: String,
    pub entry_points: Vec<String>,
}
