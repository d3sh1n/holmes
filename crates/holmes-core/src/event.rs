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
        /// Native tool-call id from the model response (P1-03). Correlates this
        /// call with its `ToolResult`/`ToolBlocked` regardless of event ordering
        /// or parallelism. `None` only in events written before call-id
        /// persistence (serde default keeps old event payloads readable).
        #[serde(default)]
        call_id: Option<String>,
    },
    ToolResult {
        name: String,
        /// Legacy compatibility projection. New code must derive success from
        /// `outcome`; old events without it fall back to this boolean.
        success: bool,
        /// Typed execution status added after schema v6. Optional so every historic
        /// JSON event remains readable; `None` maps to Succeeded/Failed via `success`.
        #[serde(default)]
        outcome: Option<crate::tool_types::ToolOutcomeStatus>,
        content: String,
        error: Option<String>,
        artifacts: Vec<String>,
        /// Native tool-call id of the call this result answers (P1-03).
        #[serde(default)]
        call_id: Option<String>,
    },
    /// Audit record for a call that was never executed (permission / approval /
    /// hook / guard / cancellation / budget denial). Replay synthesizes a
    /// failure tool-result message from this event so the model history stays
    /// legal; the guard name and reason stay here as audit metadata.
    ToolBlocked {
        tool_name: String,
        guard_name: String,
        reason: String,
        /// Native tool-call id of the blocked call (P1-03).
        #[serde(default)]
        call_id: Option<String>,
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
    /// A finding recorded by `SkepticGate` into the validated zone. Durable so findings
    /// survive turn boundaries and session resume (the in-memory `AttackState` is rebuilt
    /// each turn; these events are replayed back into it).
    FindingRecorded {
        id: String,
        finding_type: String,
        confidence: String,
        severity: Severity,
        evidence: String,
        details: String,
        attack_type: String,
        location: String,
        evidence_source: Option<String>,
        #[serde(default)]
        resolution_ids: Vec<String>,
        #[serde(default)]
        affected_asset: Option<String>,
        #[serde(default)]
        evidence_artifacts: crate::bounty::EvidenceArtifacts,
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
    /// Audit event: a long-term memory write was refused (e.g. sensitive
    /// content, prompt injection). The content itself is never persisted —
    /// only a short summary and the rejection reason.
    MemoryRejected {
        content_summary: String,
        reason: String,
    },
    /// Two recalled memories are linked as conflicting (or one supersedes the
    /// other); only the preferred one was injected.
    MemoryConflictDetected {
        suppressed_id: String,
        chosen_id: String,
        reason: String,
    },
    /// Lifecycle transition of a memory/skill (staged → active, disabled,
    /// archived, rolled back, validation recorded).
    MemoryStatusChanged {
        memory_id: String,
        from_status: String,
        to_status: String,
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
        result: crate::subagent::AgentTaskResult,
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

    // === Authorized bounty research workflow ===
    ProgramScopeSet {
        program: crate::bounty::ProgramScope,
    },
    AssetRecorded {
        asset: crate::bounty::DiscoveredAsset,
    },

    // === Report ===
    ReportGenerated {
        report_type: ReportType,
        file_path: String,
        sections: Vec<String>,
        generated_by: ReportGenerator,
    },
}
