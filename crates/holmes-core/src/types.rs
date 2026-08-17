use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ---- Session ----

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum SessionMode {
    #[default]
    Pentest,
    CodeAudit,
    Reverse,
    SecurityResearch,
    Mixed,
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
    /// Shared Hypothesis Ledger scope. Root sessions create a case; forks and
    /// subagents inherit it. Legacy deserializers may not have this field.
    #[serde(default)]
    pub case_id: String,
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
#[serde(deny_unknown_fields)]
pub struct SubAgentTask {
    pub task: String,
    pub context_summary: serde_json::Value,
    pub expected_output: OutputSchema,
    pub constraints: SubAgentConstraints,
    #[serde(default)]
    pub run_in_background: bool,
    #[serde(default, rename = "_ledger_assignment")]
    pub ledger_assignment: Option<crate::ledger::ExperimentAssignment>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputSchema {
    pub schema: String,
    pub required_fields: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubAgentConstraints {
    pub max_turns: u32,
    pub tools_allowlist: Vec<String>,
    pub isolation: Option<String>,
}

// `SubAgentResult` was removed in AGT-013: subagent runs now return the
// structured `AgentTaskResult` protocol from `crate::subagent`.

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

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryCategory {
    /// Reusable attack technique / task experience (任务经验).
    #[default]
    AttackExperience,
    DiscoveredPattern,
    ToolUsage,
    /// Case/target state and plain facts (事实).
    TargetKnowledge,
    Fact,
    UserPreference,
    ProjectConvention,
    /// A reusable, promotable capability (技能) — subject to the staged
    /// learning lifecycle (validation + approval before it can activate).
    Skill,
}

/// Where a memory came from (来源). Agent inferences are only allowed into the
/// staged area; user/tool-originated entries may activate directly.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemorySource {
    /// Explicit user input (e.g. a Watson correction).
    User,
    /// Evidence observed through tool results.
    ToolEvidence,
    /// Anything the agent/model inferred itself.
    #[default]
    AgentInferred,
}

/// Applicability scope (适用范围).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    Session,
    Project,
    User,
    #[default]
    Global,
}

/// Lifecycle status. Only `Active` memories are recalled; `Staged` entries
/// await validation/approval; low-quality entries are `Disabled` first and
/// only then `Archived` (never hard-deleted).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStatus {
    #[default]
    Active,
    Staged,
    Disabled,
    Archived,
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
    #[serde(default)]
    pub source: MemorySource,
    #[serde(default)]
    pub confidence: f64,
    #[serde(default)]
    pub scope: MemoryScope,
    #[serde(default)]
    pub status: MemoryStatus,
    #[serde(default)]
    pub last_verified_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
    /// IDs of memories this one conflicts with (冲突关系).
    #[serde(default)]
    pub conflicts_with: Vec<String>,
    /// ID of the memory this one replaces (替代关系).
    #[serde(default)]
    pub supersedes: Option<String>,
    /// Sensitivity marker (敏感性). Sensitive content is normally rejected at
    /// write time; this flag marks allowed-but-sensitive case state.
    #[serde(default)]
    pub sensitive: bool,
    /// Skill versioning: `parent_version_id` chains versions so a skill can be
    /// rolled back to the previous version.
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub parent_version_id: Option<String>,
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
    Message {
        content: String,
    },
    SlashCommand {
        command: String,
        args: String,
    },
    DirectTool {
        tool_name: String,
        arguments: String,
    },
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
