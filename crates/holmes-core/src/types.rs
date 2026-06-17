use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ---- Session ----

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    Pentest,
    CodeAudit,
    Reverse,
    SecurityResearch,
    Mixed,
}

impl Default for SessionMode {
    fn default() -> Self { Self::Pentest }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    UserQuit,
    GoalAchieved,
    Aborted,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub title: Option<String>,
    pub mode: SessionMode,
    pub model: Option<String>,
    pub model_config: Option<serde_json::Value>,
    pub system_prompt: Option<String>,
    pub parent_session_id: Option<String>,
    pub fork_point: Option<u64>,
    pub source: String,
    pub tags: Vec<String>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub end_reason: Option<EndReason>,
    pub message_count: u64,
    pub tool_call_count: u64,
    pub subagent_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub estimated_cost_usd: f64,
    pub goal_condition: Option<String>,
    pub goal_achieved: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub title: Option<String>,
    pub mode: SessionMode,
    pub source: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub end_reason: Option<EndReason>,
    pub message_count: u64,
    pub parent_session_id: Option<String>,
    pub preview: Option<String>,
    pub last_active: Option<DateTime<Utc>>,
}

// ---- Turn ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnResult {
    pub turn_index: u64,
    pub reply: String,
    pub tokens_used: TokenDelta,
    pub sub_agents_spawned: Vec<String>,
    pub events_produced: (u64, u64),
    pub dashboard_snapshot: Option<DashboardSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TokenDelta {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

// ---- Context ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextTarget {
    pub kind: ContextKind,
    pub identifier: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextKind {
    Host,
    File,
    Function,
    Module,
    Network,
    Binary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextSnapshot {
    pub summary: String,
    pub preserved_keys: Vec<String>,
    pub active_contexts: Vec<ContextTarget>,
    pub timestamp: DateTime<Utc>,
}

// ---- Mind Palace ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MindPalaceSummary {
    pub memory_count: usize,
    pub active_contexts: Vec<ContextTarget>,
    pub findings_count: usize,
    pub vulnerabilities: Vec<FindingSummary>,
    pub attack_surface: AttackSurfaceSummary,
    pub goal_progress: Option<GoalProgressSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FindingSummary {
    pub title: String,
    pub severity: String,
    pub location: String,
    pub confidence: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AttackSurfaceSummary {
    pub hosts: Vec<String>,
    pub services: Vec<String>,
    pub tech_stack: Vec<String>,
    pub endpoints: Vec<String>,
    pub credentials_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalProgressSummary {
    pub total_subtasks: usize,
    pub completed_subtasks: usize,
    pub active_subtask: Option<String>,
    pub turns_spent: u64,
}

// ---- Dashboard ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardSnapshot {
    pub sections: HashMap<String, DashboardSection>,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardSection {
    pub title: String,
    pub content_summary: String,
    pub item_count: usize,
}

// ---- Sub-Agent ----

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AgentType {
    Scout,
    Analyst,
    Operative,
    Ghost,
    Chronicler,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentTask {
    pub task: String,
    pub context_summary: serde_json::Value,
    pub expected_output: OutputSchema,
    pub constraints: SubAgentConstraints,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputSchema {
    pub schema: String,
    pub required_fields: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentConstraints {
    pub max_turns: u32,
    pub tools_allowlist: Vec<String>,
    pub isolation: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentResult {
    pub findings: Vec<serde_json::Value>,
    pub risk_assessment: Option<String>,
    pub summary: String,
    pub tokens_used: u64,
    pub events_count: u64,
    pub success: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentHandle {
    pub sub_session_id: String,
    pub agent_type: AgentType,
    pub status: SubAgentStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SubAgentStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

// ---- Memory ----

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryCategory {
    AttackExperience,
    DiscoveredPattern,
    ToolUsage,
    TargetKnowledge,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    pub category: MemoryCategory,
    pub content: String,
    pub tags: Vec<String>,
    pub attack_type: Option<String>,
    pub tech_stack: Option<Vec<String>>,
    pub success: bool,
    pub relevance_score: f64,
    pub source_session_id: Option<String>,
    pub consolidated_from: Option<Vec<String>>,
    pub created_at: DateTime<Utc>,
}

// ---- Goal ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalStatus {
    pub condition: String,
    pub satisfied: bool,
    pub reason: Option<String>,
    pub turn_count: u64,
    pub tokens_spent: u64,
    pub subtasks: Vec<SubTask>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubTask {
    pub id: String,
    pub description: String,
    pub status: SubTaskStatus,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SubTaskStatus {
    Pending,
    Active,
    Completed,
    Blocked,
}

// ---- User Input ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UserInput {
    Message { content: String },
    SlashCommand { command: String, args: String },
    DirectTool { tool_name: String, arguments: String },
}

// ---- Session Filter ----

#[derive(Debug, Clone, Default)]
pub struct SessionFilter {
    pub source: Option<String>,
    pub mode: Option<SessionMode>,
    pub parent_session_id: Option<String>,
    pub include_children: bool,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
    pub search: Option<String>,
}
