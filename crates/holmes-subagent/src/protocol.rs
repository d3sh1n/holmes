use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatchTask {
    pub task: String,
    pub context_summary: ContextForSubAgent,
    pub expected_output: ExpectedOutput,
    pub constraints: TaskConstraints,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextForSubAgent {
    pub target: Option<String>,
    pub tech_stack: Vec<String>,
    pub known_info: String,
    pub relevant_memories: Vec<MemorySnippet>,
    pub active_contexts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemorySnippet {
    pub id: String,
    pub content: String,
    pub relevance: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpectedOutput {
    pub format: String,
    pub required_fields: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskConstraints {
    pub max_turns: u32,
    pub tools_allowlist: Vec<String>,
    pub isolation: Option<String>,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "format")]
pub enum TypedResult {
    #[serde(rename = "ScoutResult")]
    Scout(ScoutResult),
    #[serde(rename = "AnalystResult")]
    Analyst(AnalystResult),
    #[serde(rename = "OperativeResult")]
    Operative(OperativeResult),
    #[serde(rename = "ChroniclerResult")]
    Chronicler(ChroniclerResult),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoutResult {
    pub targets_found: Vec<DiscoveredTarget>,
    pub services: Vec<DiscoveredService>,
    pub endpoints: Vec<String>,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredTarget {
    pub kind: String,
    pub identifier: String,
    pub details: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredService {
    pub host: String,
    pub port: u16,
    pub service: String,
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalystResult {
    pub findings: Vec<AnalystFinding>,
    pub code_patterns: Vec<AnalystCodePattern>,
    pub risk_assessment: String,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalystFinding {
    pub finding_type: String,
    pub location: String,
    pub confidence: String,
    pub evidence: String,
    pub severity: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalystCodePattern {
    pub pattern_type: String,
    pub location: String,
    pub risk: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperativeResult {
    pub success: bool,
    pub access_gained: Vec<AccessGained>,
    pub evidence: String,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessGained {
    pub target: String,
    pub access_level: String,
    pub method: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChroniclerResult {
    pub report_sections: Vec<ReportSection>,
    pub dashboard_updates: Vec<DashboardUpdate>,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportSection {
    pub title: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardUpdate {
    pub section: String,
    pub content: String,
}
