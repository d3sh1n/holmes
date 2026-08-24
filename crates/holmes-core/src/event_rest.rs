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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Critical,
    High,
    Medium,
    Low,
    #[default]
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
            Event::SessionSystemPromptSet {
                content, source, ..
            } => {
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
            Event::SessionModeSet { mode, source, .. } => match source {
                Some(source) => format!("session_mode source={} {:?}", source, mode),
                None => format!("session_mode {:?}", mode),
            },
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
            Event::MemoryRejected {
                content_summary,
                reason,
            } => {
                format!("memory rejected: {} ({})", content_summary, reason)
            }
            Event::MemoryConflictDetected { reason, .. } => reason.clone(),
            Event::MemoryStatusChanged {
                from_status,
                to_status,
                reason,
                ..
            } => {
                format!("memory {} -> {}: {}", from_status, to_status, reason)
            }
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
            Event::ProgramScopeSet { program } => {
                format!("program_scope {}", program.name)
            }
            Event::AssetRecorded { asset } => {
                format!("asset {} ({})", asset.identifier, asset.how_found.label())
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
            | Event::FindingRecorded { .. }
            | Event::CodePatternFound { .. }
            | Event::ReverseInsight { .. }
            | Event::CredentialFound { .. }
            | Event::HostCompromised { .. }
            | Event::LateralMovement { .. }
            | Event::NetworkTopologyUpdate { .. }
            | Event::ProgramScopeSet { .. }
            | Event::AssetRecorded { .. } => "situational",
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
            | Event::MemoryConflictDetected { .. }
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
            | Event::MemoryWriteStaged { .. }
            | Event::MemoryRejected { .. }
            | Event::MemoryStatusChanged { .. } => "learning",
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
            Event::SessionModeSet {
                mode: SessionMode::SecurityResearch,
                source: Some("startup".into()),
                timestamp: Some(now),
            },
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
        assert!(
            events[0].content_text().contains("SecurityResearch")
                || events[0].content_text().contains("security_research")
        );
        assert_eq!(events[1].category(), "session");
        assert!(events[1].content_text().contains("system prompt content"));
        assert_eq!(events[2].category(), "session");
        assert!(events[2].content_text().contains("claude-sonnet-4-6"));
        assert_eq!(events[3].category(), "session");
        assert!(events[3].content_text().contains("http_request"));
        assert_eq!(events[4].category(), "context");
        assert!(events[4].content_text().contains("idor evidence"));
        assert_eq!(events[5].category(), "context");
        assert!(events[5].content_text().contains("auth investigation"));

        for event in events {
            let encoded = serde_json::to_string(&event).expect("serialize event");
            let decoded: Event = serde_json::from_str(&encoded).expect("deserialize event");
            assert_eq!(decoded.category(), event.category());
        }
    }
}
