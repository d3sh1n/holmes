use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum FindingConfidence {
    Candidate,
    Confirmed,
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub id: String,
    pub finding_type: String,
    pub confidence: FindingConfidence,
    pub evidence: String,
    pub details: String,
    pub attack_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttackHypothesis {
    pub attack_type: String,
    pub confidence: f32,
    pub reasoning: String,
    pub entry_points: Vec<String>,
}
