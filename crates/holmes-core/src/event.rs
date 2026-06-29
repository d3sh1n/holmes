use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    // === Session Lifecycle ===
    SessionCreated {
        id: String,
        title: Option<String>,
        mode: SessionMode,
        model: Option<String>,
        system_prompt: Option<String>,
        parent_id: Option<String>,
        fork_point: Option<u64>,
        created_at: DateTime<Utc>,
        tags: Vec<String>,
    },
    SessionEnded {
        reason: EndReason,
        summary: Option<String>,
    },
    SessionModeSet {
        mode: SessionMode,
        #[serde(default)]
        source: Option<String>,
        #[serde(default)]
        timestamp: Option<DateTime<Utc>>,
    },
    SessionSystemPromptSet {
        prompt_hash: String,
        content: String,
        source: String,
        timestamp: DateTime<Utc>,
    },
    SessionModelSet {
        model: String,
        provider: Option<String>,
        source: String,
        timestamp: DateTime<Utc>,
    },
    ActiveToolsSet {
        tool_names: Vec<String>,
        source: String,
        timestamp: DateTime<Utc>,
    },

    // === Turn Boundaries ===
    UserMessage {
        content: String,
        timestamp: DateTime<Utc>,
    },
    TurnComplete {
        event_range: (u64, u64),
        tokens_used: TokenDelta,
        sub_agents_spawned: Vec<String>,
    },

    // === Goal System ===
    GoalSet {
        condition: String,
        plan: Option<String>,
        subtasks: Vec<SubTask>,
    },
    GoalEvaluated {
        satisfied: bool,
        reason: String,
        turn_count: u64,
        tokens_spent: u64,
    },
    GoalCleared {
        reason: String,
    },
    GoalProgress {
        turns: u64,
        tokens: u64,
        summary: String,
    },
    SubtaskUpdate {
        subtask_id: String,
        status: SubTaskStatus,
        note: Option<String>,
    },

    // === Agent Thought & Action ===
    Thinking {
        content: String,
        reasoning_type: Option<String>,
    },
    ToolCall {
        name: String,
        arguments: serde_json::Value,
        purpose: Option<String>,
    },
    ToolResult {
        name: String,
        success: bool,
        content: String,
        error: Option<String>,
        artifacts: Vec<String>,
    },
    ToolBlocked {
        tool_name: String,
        guard_name: String,
        reason: String,
    },

    // === Situational Awareness ===
    TargetDiscovered {
        kind: TargetKind,
        details: serde_json::Value,
        confidence: String,
        source: String,
    },
    AttackSurfaceUpdate {
        hosts: Vec<String>,
        services: Vec<ServiceInfo>,
        tech_stack: Vec<String>,
        endpoints: Vec<String>,
        credentials: Vec<CredentialRef>,
        notes: Option<String>,
    },
    VulnerabilityFound {
        title: String,
        cwe: Option<String>,
        cvss: Option<f64>,
        severity: Severity,
        location: String,
        evidence: String,
        poc: Option<String>,
        status: FindingStatus,
    },
    CodePatternFound {
        pattern_type: String,
        file: String,
        line_range: Option<(u32, u32)>,
        snippet: String,
        risk_assessment: String,
        language: Option<String>,
    },
    ReverseInsight {
        insight_type: ReverseInsightType,
        description: String,
        confidence: String,
        addresses: Vec<String>,
    },
    CredentialFound {
        username: String,
        credential_type: CredentialType,
        source_host: String,
        context: String,
        cracked: Option<bool>,
    },
    HostCompromised {
        host: String,
        access_level: AccessLevel,
        method: String,
        persistence: Option<String>,
        session_id: Option<String>,
    },
    LateralMovement {
        from_host: String,
        to_host: String,
        method: String,
        credentials_used: Option<String>,
        timestamp: DateTime<Utc>,
    },
    NetworkTopologyUpdate {
        subnets: Vec<String>,
        hosts: Vec<HostInfo>,
        relationships: Vec<HostRelationship>,
        trust_paths: Vec<Vec<String>>,
        domain_info: Option<DomainInfo>,
    },

    // === Deduction Ledger ===
    EvidenceObserved {
        evidence_id: String,
        summary: String,
        source: String,
        confidence: String,
    },
    FactRecorded {
        fact_id: String,
        statement: String,
        evidence_ids: Vec<String>,
    },
    HypothesisProposed {
        hypothesis_id: String,
        statement: String,
        rationale: String,
        #[serde(default)]
        confidence: Option<f32>,
        #[serde(default)]
        attack_type: Option<String>,
        #[serde(default)]
        entry_points: Vec<String>,
    },
    PredictionMade {
        hypothesis_id: String,
        prediction: String,
    },
    ExperimentPlanned {
        hypothesis_id: String,
        action: String,
        distinguishes: Vec<String>,
    },
    HypothesisSupported {
        hypothesis_id: String,
        evidence_id: String,
        rationale: String,
        #[serde(default)]
        confidence: Option<f32>,
    },
    HypothesisContradicted {
        hypothesis_id: String,
        evidence_id: String,
        rationale: String,
        #[serde(default)]
        confidence: Option<f32>,
    },
    HypothesisRejected {
        hypothesis_id: String,
        reason: String,
    },
    HypothesisConfirmed {
        hypothesis_id: String,
        conclusion: String,
        #[serde(default)]
        confidence: Option<f32>,
    },
    ConclusionDrawn {
        conclusion: String,
        supporting_hypotheses: Vec<String>,
        evidence_ids: Vec<String>,
    },

    // === Strategy & Reflection ===
    DirectiveSet {
        attack_type: Option<String>,
        objective: String,
        approach: String,
        entry_points: Vec<String>,
        recommended_skills: Vec<String>,
    },
    ReflectionRecorded {
        diagnosis: String,
        failure_type: String,
        lessons_learned: String,
        suggestions: Vec<String>,
        triggered_by: String,
    },
    HypothesisUpdate {
        active: Option<String>,
        pending_count: usize,
        rejected: Vec<String>,
        confirmed: Vec<String>,
    },
    AdvisorAction {
        level: InterventionLevel,
        advice: String,
        reasoning: String,
        auto_applied: bool,
    },

    // === Mind Palace Operations ===
    MemoryStored {
        category: MemoryCategory,
        content: String,
        tags: Vec<String>,
        relevance_score: f64,
        source_session_id: Option<String>,
    },
    MemoryRecalled {
        memory_ids: Vec<String>,
        trigger: RecallTrigger,
        relevance: Vec<f64>,
    },
    MemoryConsolidated {
        from_ids: Vec<String>,
        into_id: String,
        summary: String,
    },
    ContextSnapshotTaken {
        summary: String,
        preserved_keys: Vec<String>,
        active_contexts: Vec<ContextTarget>,
    },
    ContextSwitched {
        from_context: Option<ContextTarget>,
        to_context: ContextTarget,
        reason: String,
    },
    DashboardUpdated {
        section: String,
        content_summary: String,
        timestamp: DateTime<Utc>,
    },

    // === Context Management ===
    CompressionApplied {
        before_count: usize,
        after_count: usize,
        summary: String,
        preserved_keys: Vec<String>,
        method: CompressionMethod,
        #[serde(default)]
        preserved_head: Option<usize>,
        #[serde(default)]
        preserved_tail_tokens: Option<usize>,
        #[serde(default)]
        archive_path: Option<String>,
        #[serde(default)]
        archived_event_range: Option<(u64, u64)>,
        #[serde(default)]
        trigger: Option<CompactionTrigger>,
        #[serde(default)]
        timestamp: Option<DateTime<Utc>>,
    },
    BranchSummary {
        from_event_index: u64,
        to_event_index: u64,
        summary: String,
        reason: String,
        method: SummaryMethod,
        timestamp: DateTime<Utc>,
    },

    // === Skill & Knowledge Injection ===
    SkillInjected {
        skill_name: String,
        source: InjectionSource,
        match_reason: Option<String>,
    },
    KnowledgeInjected {
        source: KnowledgeSource,
        content: String,
        relevance_context: String,
    },
    HumanFeedback {
        content: String,
        target_event: Option<u64>,
        timestamp: DateTime<Utc>,
    },
    LearningReviewStarted {
        trigger: String,
        event_range: (u64, u64),
    },
    LearningReviewCompleted {
        candidates: usize,
        applied: usize,
        staged: usize,
    },
    LearningCandidateRejected {
        kind: String,
        reason: String,
    },
    MemoryWriteStaged {
        content: String,
        reason: String,
    },

    // === Sub-Agent ===
    SubAgentSpawned {
        sub_session_id: String,
        agent_type: AgentType,
        task_description: String,
        context_summary: serde_json::Value,
        isolation: Option<String>,
        model: String,
        tools: Vec<String>,
        max_turns: u32,
    },
    SubAgentCompleted {
        sub_session_id: String,
        result: SubAgentResult,
        tokens_used: u64,
        events_count: u64,
        findings_count: usize,
    },
    SubAgentProgress {
        sub_session_id: String,
        status: SubAgentStatus,
        current_turn: u32,
        summary: Option<String>,
    },

    // === Report ===
    ReportGenerated {
        report_type: ReportType,
        file_path: String,
        sections: Vec<String>,
        generated_by: ReportGenerator,
    },
}

// === Supporting Types ===

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    Host,
    Service,
    Endpoint,
    File,
    Function,
    Protocol,
    Credential,
    Vulnerability,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceInfo {
    pub host: String,
    pub port: u16,
    pub protocol: String,
    pub service: String,
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialRef {
    pub username: String,
    pub credential_type: String,
    pub host: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Critical,
    High,
    Medium,
    Low,
    Info,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FindingStatus {
    Confirmed,
    Suspicious,
    FalsePositive,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReverseInsightType {
    FunctionIdentified,
    ProtocolReverse,
    AlgorithmRecovery,
    ObfuscationBypass,
    StringDecode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialType {
    Plaintext,
    Hash,
    Token,
    KerberosTicket,
    SshKey,
    ApiKey,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessLevel {
    User,
    Root,
    System,
    DomainAdmin,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostInfo {
    pub host: String,
    pub ip: Option<String>,
    pub os: Option<String>,
    pub access_level: Option<AccessLevel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostRelationship {
    pub from: String,
    pub to: String,
    pub relationship_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DomainInfo {
    pub domain_name: String,
    pub domain_controllers: Vec<String>,
    pub trusted_domains: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterventionLevel {
    Nudge,
    Suggest,
    ForcePivot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecallTrigger {
    Query,
    Context,
    Similarity,
    SkillMatch,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompressionMethod {
    LlmSummary,
    StaticFallback,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SummaryMethod {
    Llm,
    StaticFallback,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompactionTrigger {
    Manual,
    Threshold,
    Overflow,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InjectionSource {
    Initial,
    Perception,
    Reflection,
    User,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeSource {
    Memory,
    Skill,
    User,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportType {
    Writeup,
    VulnerabilityReport,
    CodeAuditReport,
    ReverseEngineeringReport,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportGenerator {
    Agent,
    SubAgent,
    User,
}

// === StoredEvent ===

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredEvent {
    pub id: u64,
    pub session_id: String,
    pub event_index: u64,
    pub turn_index: Option<u64>,
    pub timestamp: DateTime<Utc>,
    #[serde(flatten)]
    pub event: Event,
}

impl Event {
    pub fn content_text(&self) -> String {
        match self {
            Event::UserMessage { content, .. } => content.clone(),
            Event::Thinking { content, .. } => content.clone(),
            Event::ToolCall {
                name,
                arguments,
                purpose,
                ..
            } => {
                format!(
                    "{} {:?} {}",
                    name,
                    arguments,
                    purpose.as_deref().unwrap_or("")
                )
            }
            Event::ToolResult { name, content, .. } => {
                format!("{} {}", name, content)
            }
            Event::SessionSystemPromptSet { content, source, .. } => {
                format!("system_prompt source={} {}", source, content)
            }
            Event::SessionModelSet {
                model,
                provider,
                source,
                ..
            } => format!(
                "model source={} provider={} {}",
                source,
                provider.as_deref().unwrap_or(""),
                model
            ),
            Event::ActiveToolsSet {
                tool_names, source, ..
            } => format!("active_tools source={} {}", source, tool_names.join(" ")),
            Event::BranchSummary {
                summary, reason, ..
            } => format!("branch_summary reason={} {}", reason, summary),
            Event::CompressionApplied { summary, .. } => summary.clone(),
            Event::VulnerabilityFound {
                title, evidence, ..
            } => {
                format!("{} {}", title, evidence)
            }
            Event::EvidenceObserved { summary, .. } => summary.clone(),
            Event::FactRecorded { statement, .. } => statement.clone(),
            Event::HypothesisProposed {
                statement,
                rationale,
                ..
            } => {
                format!("{} {}", statement, rationale)
            }
            Event::PredictionMade { prediction, .. } => prediction.clone(),
            Event::ExperimentPlanned { action, .. } => action.clone(),
            Event::HypothesisSupported { rationale, .. }
            | Event::HypothesisContradicted { rationale, .. } => rationale.clone(),
            Event::HypothesisRejected { reason, .. } => reason.clone(),
            Event::HypothesisConfirmed { conclusion, .. }
            | Event::ConclusionDrawn { conclusion, .. } => conclusion.clone(),
            Event::ReflectionRecorded { diagnosis, .. } => diagnosis.clone(),
            Event::MemoryStored { content, .. } => content.clone(),
            Event::LearningReviewStarted { trigger, .. } => trigger.clone(),
            Event::LearningReviewCompleted {
                candidates,
                applied,
                staged,
            } => {
                format!(
                    "learning candidates={} applied={} staged={}",
                    candidates, applied, staged
                )
            }
            Event::LearningCandidateRejected { kind, reason } => {
                format!("{} {}", kind, reason)
            }
            Event::MemoryWriteStaged { content, .. } => content.clone(),
            Event::SubAgentSpawned {
                task_description, ..
            } => task_description.clone(),
            Event::SubAgentCompleted { result, .. } => result.summary.clone(),
            Event::ReportGenerated {
                file_path,
                sections,
                ..
            } => {
                format!("{} {:?}", file_path, sections)
            }
            _ => String::new(),
        }
    }

    pub fn is_turn_start(&self) -> bool {
        matches!(self, Event::UserMessage { .. })
    }

    pub fn is_turn_end(&self) -> bool {
        matches!(self, Event::TurnComplete { .. })
    }

    pub fn category(&self) -> &'static str {
        match self {
            Event::SessionCreated { .. }
            | Event::SessionEnded { .. }
            | Event::SessionModeSet { .. }
            | Event::SessionSystemPromptSet { .. }
            | Event::SessionModelSet { .. }
            | Event::ActiveToolsSet { .. } => "session",
            Event::UserMessage { .. } | Event::TurnComplete { .. } => "turn",
            Event::GoalSet { .. }
            | Event::GoalEvaluated { .. }
            | Event::GoalCleared { .. }
            | Event::GoalProgress { .. }
            | Event::SubtaskUpdate { .. } => "goal",
            Event::Thinking { .. }
            | Event::ToolCall { .. }
            | Event::ToolResult { .. }
            | Event::ToolBlocked { .. } => "action",
            Event::TargetDiscovered { .. }
            | Event::AttackSurfaceUpdate { .. }
            | Event::VulnerabilityFound { .. }
            | Event::CodePatternFound { .. }
            | Event::ReverseInsight { .. }
            | Event::CredentialFound { .. }
            | Event::HostCompromised { .. }
            | Event::LateralMovement { .. }
            | Event::NetworkTopologyUpdate { .. } => "situational",
            Event::EvidenceObserved { .. }
            | Event::FactRecorded { .. }
            | Event::HypothesisProposed { .. }
            | Event::PredictionMade { .. }
            | Event::ExperimentPlanned { .. }
            | Event::HypothesisSupported { .. }
            | Event::HypothesisContradicted { .. }
            | Event::HypothesisRejected { .. }
            | Event::HypothesisConfirmed { .. }
            | Event::ConclusionDrawn { .. } => "deduction",
            Event::DirectiveSet { .. }
            | Event::ReflectionRecorded { .. }
            | Event::HypothesisUpdate { .. }
            | Event::AdvisorAction { .. } => "strategy",
            Event::MemoryStored { .. }
            | Event::MemoryRecalled { .. }
            | Event::MemoryConsolidated { .. }
            | Event::ContextSnapshotTaken { .. }
            | Event::ContextSwitched { .. }
            | Event::DashboardUpdated { .. } => "mind_palace",
            Event::CompressionApplied { .. } | Event::BranchSummary { .. } => "context",
            Event::SkillInjected { .. }
            | Event::KnowledgeInjected { .. }
            | Event::HumanFeedback { .. } => "injection",
            Event::LearningReviewStarted { .. }
            | Event::LearningReviewCompleted { .. }
            | Event::LearningCandidateRejected { .. }
            | Event::MemoryWriteStaged { .. } => "learning",
            Event::SubAgentSpawned { .. }
            | Event::SubAgentCompleted { .. }
            | Event::SubAgentProgress { .. } => "subagent",
            Event::ReportGenerated { .. } => "report",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_events_have_text_and_categories() {
        let now = Utc::now();
        let events = vec![
            Event::SessionSystemPromptSet {
                prompt_hash: "hash123".into(),
                content: "system prompt content".into(),
                source: "startup".into(),
                timestamp: now,
            },
            Event::SessionModelSet {
                model: "claude-sonnet-4-6".into(),
                provider: Some("default".into()),
                source: "startup".into(),
                timestamp: now,
            },
            Event::ActiveToolsSet {
                tool_names: vec!["http_request".into(), "report_finding".into()],
                source: "startup".into(),
                timestamp: now,
            },
            Event::BranchSummary {
                from_event_index: 1,
                to_event_index: 4,
                summary: "branch path found idor evidence".into(),
                reason: "fork".into(),
                method: SummaryMethod::StaticFallback,
                timestamp: now,
            },
            Event::CompressionApplied {
                before_count: 10,
                after_count: 4,
                summary: "compacted auth investigation".into(),
                preserved_keys: vec!["system_prompt".into()],
                method: CompressionMethod::StaticFallback,
                preserved_head: Some(2),
                preserved_tail_tokens: Some(4000),
                archive_path: Some("sessions/s/compactions/compaction_7.json".into()),
                archived_event_range: Some((2, 7)),
                trigger: Some(CompactionTrigger::Manual),
                timestamp: Some(now),
            },
        ];

        assert_eq!(events[0].category(), "session");
        assert!(events[0].content_text().contains("system prompt content"));
        assert_eq!(events[1].category(), "session");
        assert!(events[1].content_text().contains("claude-sonnet-4-6"));
        assert_eq!(events[2].category(), "session");
        assert!(events[2].content_text().contains("http_request"));
        assert_eq!(events[3].category(), "context");
        assert!(events[3].content_text().contains("idor evidence"));
        assert_eq!(events[4].category(), "context");
        assert!(events[4].content_text().contains("auth investigation"));

        for event in events {
            let encoded = serde_json::to_string(&event).expect("serialize event");
            let decoded: Event = serde_json::from_str(&encoded).expect("deserialize event");
            assert_eq!(decoded.category(), event.category());
        }
    }
}
