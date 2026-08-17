//! Case-scoped, append-only Hypothesis Ledger v2 domain model.
//!
//! This module deliberately contains no I/O, clocks, model calls or tool
//! registry access.  The reducer is shared by runtime validation and durable
//! replay so an event accepted at write time has exactly the same semantics
//! after a restart.

use crate::tool_types::ToolOutcomeStatus;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

pub const LEDGER_EVENT_SCHEMA_VERSION: u32 = 2;

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }
    };
}

string_id!(CaseId);
string_id!(HypothesisId);
string_id!(PredictionId);
string_id!(ExperimentId);
string_id!(EvidenceLinkId);
string_id!(ResolutionId);
string_id!(ActorId);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HypothesisStatus {
    Open,
    Confirmed,
    Rejected,
    Inconclusive,
    Superseded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "id")]
pub enum LedgerRef {
    Evidence(String),
    Resolution(ResolutionId),
    UserAssertion(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hypothesis {
    pub id: HypothesisId,
    pub case_id: CaseId,
    pub claim: String,
    pub premise_refs: Vec<LedgerRef>,
    pub alternative_group: Option<String>,
    pub priority: Priority,
    pub status: HypothesisStatus,
    pub revision: u64,
    pub created_by: ActorId,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidatorKind {
    CommandExit,
    FilePostcondition,
    NetworkDifferential,
    CodeTest,
    SecurityReproduction,
    HumanAttestation,
    Semantic,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Prediction {
    pub id: PredictionId,
    pub case_id: CaseId,
    pub hypothesis_id: HypothesisId,
    pub observable: String,
    pub expected_when_true: String,
    pub falsifier: String,
    pub validator: ValidatorKind,
    pub required: bool,
    pub revision: u64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentStatus {
    Planned,
    Queued,
    Running,
    Observed,
    Blocked,
    Failed,
    Cancelled,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Experiment {
    pub id: ExperimentId,
    pub case_id: CaseId,
    pub hypothesis_ids: Vec<HypothesisId>,
    pub prediction_ids: Vec<PredictionId>,
    pub action: String,
    pub expected_observations: Vec<String>,
    pub tool_allowlist: Vec<String>,
    pub risk: RiskLevel,
    pub status: ExperimentStatus,
    pub task_id: Option<String>,
    pub attempt: u32,
    pub idempotency_key: String,
    pub evidence_ids: Vec<String>,
    pub revision: u64,
    pub created_by: ActorId,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Runtime-only assignment carried from a validated `plan_experiment` commit
/// into `spawn_subagent`. It is not part of the model-visible tool schema and
/// is overwritten at the execution boundary, so a worker cannot self-assign a
/// different case or Experiment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentAssignment {
    pub case_id: CaseId,
    pub experiment_id: ExperimentId,
    pub expected_revision: u64,
    pub max_concurrent_per_case: usize,
    pub lease_ms: u64,
    pub safe_to_retry: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceBinding {
    pub case_id: CaseId,
    pub contract_id: Option<String>,
    pub requirement_ids: Vec<String>,
    pub experiment_id: Option<ExperimentId>,
    pub prediction_ids: Vec<PredictionId>,
    pub tool_call_id: Option<String>,
}

/// Immutable execution binding created during Commit, before a tool starts.
/// Runtime-derived contract/requirement ids may be attached to otherwise
/// unplanned calls; experiment/prediction ids only come from a validated
/// `plan_experiment.bind_calls` mapping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionBinding {
    pub case_id: CaseId,
    pub contract_id: Option<String>,
    pub requirement_ids: Vec<String>,
    pub experiment_id: Option<ExperimentId>,
    pub prediction_ids: Vec<PredictionId>,
    pub tool_call_id: String,
    pub attempt: u32,
}

/// Bounded, immutable receipt supplied by the execution boundary. Raw output
/// remains in the session ToolResult/blob stream; the Ledger stores only this
/// metadata plus hash/snippet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutcomeReceipt {
    pub tool: String,
    pub outcome_status: ToolOutcomeStatus,
    pub exit_code: Option<i32>,
    pub kind: EvidenceKind,
    pub input_summary: String,
    pub output_hash: String,
    pub output_snippet: String,
    pub predicate: String,
    pub verified_by: VerificationMethod,
    pub recorded_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    Deterministic,
    AuditOutcome,
    HumanAttestation,
    Semantic,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationMethod {
    Runtime,
    DomainValidator,
    Human,
    SemanticVerifier,
    LegacyUnverified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRecord {
    pub id: String,
    pub binding: EvidenceBinding,
    pub source_session_id: String,
    pub tool: String,
    pub tool_call_id: Option<String>,
    pub outcome_status: ToolOutcomeStatus,
    pub exit_code: Option<i32>,
    pub kind: EvidenceKind,
    pub input_summary: String,
    pub output_hash: String,
    pub output_snippet: String,
    pub predicate: String,
    pub verified_by: VerificationMethod,
    pub recorded_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceRelation {
    Supports,
    Contradicts,
    Inconclusive,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceStrength {
    Weak,
    Moderate,
    Strong,
    Decisive,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceLink {
    pub id: EvidenceLinkId,
    pub case_id: CaseId,
    pub evidence_id: String,
    pub hypothesis_id: HypothesisId,
    pub prediction_id: Option<PredictionId>,
    pub relation: EvidenceRelation,
    pub strength: EvidenceStrength,
    pub rationale: String,
    pub validator: ValidatorKind,
    pub actor: ActorId,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedStatus {
    Confirmed,
    Rejected,
    Inconclusive,
    Superseded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolution {
    pub id: ResolutionId,
    pub case_id: CaseId,
    pub hypothesis_id: HypothesisId,
    pub status: ResolvedStatus,
    pub evidence_link_ids: Vec<EvidenceLinkId>,
    pub unresolved_contradiction_ids: Vec<EvidenceLinkId>,
    pub validator_summary: String,
    pub requested_by: ActorId,
    pub verified_by: ActorId,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkMode {
    Fast,
    Adaptive,
    Deep,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommitOperation {
    Answer,
    AskWatson,
    Finish,
    ExecuteTools,
    LedgerOnly,
    LedgerAndTools,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ordinal {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallBinding {
    pub tool_call_id: String,
    pub experiment_id: ExperimentId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliberationCommit {
    pub id: String,
    pub case_id: CaseId,
    pub session_id: String,
    pub ledger_version: u64,
    pub mode: ThinkMode,
    pub considered_hypothesis_ids: Vec<HypothesisId>,
    pub selected_operation: CommitOperation,
    pub public_rationale: String,
    pub expected_information_gain: Ordinal,
    pub risk: RiskLevel,
    pub executable_call_bindings: Vec<CallBinding>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contradiction {
    pub id: String,
    pub case_id: CaseId,
    pub hypothesis_id: HypothesisId,
    pub evidence_link_ids: Vec<EvidenceLinkId>,
    pub detected_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AggregateKind {
    Hypothesis,
    Prediction,
    Experiment,
    Evidence,
    EvidenceLink,
    Resolution,
    Deliberation,
}

/// Every payload carries an explicit schema version. Unknown versions are a
/// hard replay error rather than an invitation to silently ignore fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LedgerEvent {
    HypothesisProposedV2 {
        schema_version: u32,
        hypothesis: Hypothesis,
    },
    PredictionDeclaredV2 {
        schema_version: u32,
        prediction: Prediction,
    },
    ExperimentPlannedV2 {
        schema_version: u32,
        experiment: Experiment,
    },
    ExperimentQueuedV2 {
        schema_version: u32,
        experiment_id: ExperimentId,
        expected_revision: u64,
        task_id: String,
        occurred_at: DateTime<Utc>,
    },
    ExperimentStartedV2 {
        schema_version: u32,
        experiment_id: ExperimentId,
        expected_revision: u64,
        attempt: u32,
        occurred_at: DateTime<Utc>,
    },
    ExperimentObservedV2 {
        schema_version: u32,
        experiment_id: ExperimentId,
        expected_revision: u64,
        evidence_ids: Vec<String>,
        occurred_at: DateTime<Utc>,
    },
    ExperimentBlockedV2 {
        schema_version: u32,
        experiment_id: ExperimentId,
        expected_revision: u64,
        reason: String,
        occurred_at: DateTime<Utc>,
    },
    ExperimentFailedV2 {
        schema_version: u32,
        experiment_id: ExperimentId,
        expected_revision: u64,
        reason: String,
        occurred_at: DateTime<Utc>,
    },
    ExperimentCancelledV2 {
        schema_version: u32,
        experiment_id: ExperimentId,
        expected_revision: u64,
        reason: String,
        occurred_at: DateTime<Utc>,
    },
    ExperimentExpiredV2 {
        schema_version: u32,
        experiment_id: ExperimentId,
        expected_revision: u64,
        occurred_at: DateTime<Utc>,
    },
    EvidenceRecordedV2 {
        schema_version: u32,
        evidence: EvidenceRecord,
    },
    EvidenceLinkedV2 {
        schema_version: u32,
        link: EvidenceLink,
    },
    EvidenceLinkRejectedV2 {
        schema_version: u32,
        proposal_id: String,
        hypothesis_id: HypothesisId,
        reason: String,
    },
    ContradictionDetectedV2 {
        schema_version: u32,
        contradiction: Contradiction,
    },
    ResolutionRequestedV2 {
        schema_version: u32,
        request_id: String,
        hypothesis_id: HypothesisId,
        expected_revision: u64,
        requested_status: ResolvedStatus,
        evidence_link_ids: Vec<EvidenceLinkId>,
        requested_by: ActorId,
    },
    HypothesisResolvedV2 {
        schema_version: u32,
        expected_revision: u64,
        resolution: Resolution,
    },
    ResolutionRejectedV2 {
        schema_version: u32,
        request_id: String,
        hypothesis_id: HypothesisId,
        expected_revision: u64,
        gaps: Vec<String>,
    },
    HypothesisReopenedV2 {
        schema_version: u32,
        hypothesis_id: HypothesisId,
        expected_revision: u64,
        reason: String,
        reopened_by: ActorId,
        occurred_at: DateTime<Utc>,
    },
    HypothesisSupersededV2 {
        schema_version: u32,
        hypothesis_id: HypothesisId,
        expected_revision: u64,
        superseded_by: HypothesisId,
        reason: String,
        occurred_at: DateTime<Utc>,
    },
    DeliberationCommittedV2 {
        schema_version: u32,
        commit: DeliberationCommit,
    },
}

impl LedgerEvent {
    pub fn schema_version(&self) -> u32 {
        match self {
            Self::HypothesisProposedV2 { schema_version, .. }
            | Self::PredictionDeclaredV2 { schema_version, .. }
            | Self::ExperimentPlannedV2 { schema_version, .. }
            | Self::ExperimentQueuedV2 { schema_version, .. }
            | Self::ExperimentStartedV2 { schema_version, .. }
            | Self::ExperimentObservedV2 { schema_version, .. }
            | Self::ExperimentBlockedV2 { schema_version, .. }
            | Self::ExperimentFailedV2 { schema_version, .. }
            | Self::ExperimentCancelledV2 { schema_version, .. }
            | Self::ExperimentExpiredV2 { schema_version, .. }
            | Self::EvidenceRecordedV2 { schema_version, .. }
            | Self::EvidenceLinkedV2 { schema_version, .. }
            | Self::EvidenceLinkRejectedV2 { schema_version, .. }
            | Self::ContradictionDetectedV2 { schema_version, .. }
            | Self::ResolutionRequestedV2 { schema_version, .. }
            | Self::HypothesisResolvedV2 { schema_version, .. }
            | Self::ResolutionRejectedV2 { schema_version, .. }
            | Self::HypothesisReopenedV2 { schema_version, .. }
            | Self::HypothesisSupersededV2 { schema_version, .. }
            | Self::DeliberationCommittedV2 { schema_version, .. } => *schema_version,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaseLedgerEvent {
    pub case_id: CaseId,
    pub seq: u64,
    pub event_id: String,
    pub aggregate_kind: AggregateKind,
    pub aggregate_id: String,
    pub aggregate_revision: u64,
    pub actor_session_id: String,
    pub event: LedgerEvent,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnstoredLedgerEvent {
    pub event_id: String,
    pub aggregate_kind: AggregateKind,
    pub aggregate_id: String,
    pub aggregate_revision: u64,
    pub actor_session_id: String,
    pub event: LedgerEvent,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerSnapshot {
    pub case_id: CaseId,
    pub version: u64,
    pub projected_seq: u64,
    pub hypotheses: BTreeMap<HypothesisId, Hypothesis>,
    pub predictions: BTreeMap<PredictionId, Prediction>,
    pub experiments: BTreeMap<ExperimentId, Experiment>,
    pub evidence: BTreeMap<String, EvidenceRecord>,
    pub evidence_links: BTreeMap<EvidenceLinkId, EvidenceLink>,
    pub resolutions: BTreeMap<ResolutionId, Resolution>,
    pub contradictions: BTreeMap<String, Contradiction>,
    pub deliberation_commits: BTreeMap<String, DeliberationCommit>,
    pub applied_event_ids: BTreeSet<String>,
}

impl LedgerSnapshot {
    pub fn empty(case_id: CaseId) -> Self {
        Self {
            case_id,
            version: 0,
            projected_seq: 0,
            hypotheses: BTreeMap::new(),
            predictions: BTreeMap::new(),
            experiments: BTreeMap::new(),
            evidence: BTreeMap::new(),
            evidence_links: BTreeMap::new(),
            resolutions: BTreeMap::new(),
            contradictions: BTreeMap::new(),
            deliberation_commits: BTreeMap::new(),
            applied_event_ids: BTreeSet::new(),
        }
    }
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum LedgerReduceError {
    #[error("event {event_id} belongs to case {actual}, expected {expected}")]
    CrossCase {
        event_id: String,
        expected: CaseId,
        actual: CaseId,
    },
    #[error("event {event_id} has seq {actual}; expected {expected}")]
    SequenceGap {
        event_id: String,
        expected: u64,
        actual: u64,
    },
    #[error("event {event_id} uses unsupported schema version {actual}; expected {expected}")]
    UnsupportedSchema {
        event_id: String,
        expected: u32,
        actual: u32,
    },
    #[error("invalid ledger event {event_id}: {reason}")]
    InvalidEvent { event_id: String, reason: String },
    #[error("aggregate revision conflict for {aggregate_id}: expected {expected}, got {actual}")]
    RevisionConflict {
        aggregate_id: String,
        expected: u64,
        actual: u64,
    },
}

fn invalid(event: &CaseLedgerEvent, reason: impl Into<String>) -> LedgerReduceError {
    LedgerReduceError::InvalidEvent {
        event_id: event.event_id.clone(),
        reason: reason.into(),
    }
}

fn require_revision(
    event: &CaseLedgerEvent,
    aggregate_id: &str,
    expected: u64,
) -> Result<(), LedgerReduceError> {
    if event.aggregate_id != aggregate_id || event.aggregate_revision != expected {
        return Err(LedgerReduceError::RevisionConflict {
            aggregate_id: aggregate_id.to_owned(),
            expected,
            actual: event.aggregate_revision,
        });
    }
    Ok(())
}

fn require_kind(event: &CaseLedgerEvent, expected: AggregateKind) -> Result<(), LedgerReduceError> {
    if event.aggregate_kind != expected {
        return Err(invalid(
            event,
            format!(
                "aggregate kind {:?} does not match expected {:?}",
                event.aggregate_kind, expected
            ),
        ));
    }
    Ok(())
}

struct ExperimentTransition<'a> {
    expected_revision: u64,
    status: ExperimentStatus,
    occurred_at: DateTime<Utc>,
    task_id: Option<&'a str>,
    attempt: Option<u32>,
    evidence_ids: Option<&'a [String]>,
}

fn transition_experiment(
    state: &mut LedgerSnapshot,
    envelope: &CaseLedgerEvent,
    experiment_id: &ExperimentId,
    transition: ExperimentTransition<'_>,
) -> Result<(), LedgerReduceError> {
    require_kind(envelope, AggregateKind::Experiment)?;
    let experiment = state
        .experiments
        .get_mut(experiment_id)
        .ok_or_else(|| invalid(envelope, format!("unknown experiment {experiment_id}")))?;
    if experiment.revision != transition.expected_revision {
        return Err(LedgerReduceError::RevisionConflict {
            aggregate_id: experiment_id.to_string(),
            expected: experiment.revision,
            actual: transition.expected_revision,
        });
    }
    require_revision(
        envelope,
        experiment_id.as_str(),
        transition.expected_revision + 1,
    )?;

    let allowed = matches!(
        (&experiment.status, &transition.status),
        (ExperimentStatus::Planned, ExperimentStatus::Queued)
            | (ExperimentStatus::Planned, ExperimentStatus::Running)
            | (ExperimentStatus::Planned, ExperimentStatus::Blocked)
            | (ExperimentStatus::Planned, ExperimentStatus::Failed)
            | (ExperimentStatus::Planned, ExperimentStatus::Cancelled)
            | (ExperimentStatus::Planned, ExperimentStatus::Expired)
            | (ExperimentStatus::Queued, ExperimentStatus::Running)
            | (ExperimentStatus::Queued, ExperimentStatus::Blocked)
            | (ExperimentStatus::Queued, ExperimentStatus::Failed)
            | (ExperimentStatus::Queued, ExperimentStatus::Cancelled)
            | (ExperimentStatus::Queued, ExperimentStatus::Expired)
            | (ExperimentStatus::Running, ExperimentStatus::Running)
            | (ExperimentStatus::Running, ExperimentStatus::Observed)
            | (ExperimentStatus::Running, ExperimentStatus::Blocked)
            | (ExperimentStatus::Running, ExperimentStatus::Failed)
            | (ExperimentStatus::Running, ExperimentStatus::Cancelled)
            | (ExperimentStatus::Running, ExperimentStatus::Expired)
    );
    if !allowed {
        return Err(invalid(
            envelope,
            format!(
                "illegal experiment transition {:?} -> {:?}",
                experiment.status, transition.status
            ),
        ));
    }

    if let Some(attempt) = transition.attempt {
        if attempt <= experiment.attempt {
            return Err(invalid(
                envelope,
                format!(
                    "experiment fencing attempt {attempt} must be greater than {}",
                    experiment.attempt
                ),
            ));
        }
    }

    if let Some(task_id) = transition.task_id {
        experiment.task_id = Some(task_id.to_owned());
    }
    if let Some(attempt) = transition.attempt {
        experiment.attempt = attempt;
    }
    if let Some(evidence_ids) = transition.evidence_ids {
        for evidence_id in evidence_ids {
            if !state.evidence.contains_key(evidence_id) {
                return Err(invalid(envelope, format!("unknown evidence {evidence_id}")));
            }
        }
        experiment.evidence_ids = evidence_ids.to_vec();
    }
    experiment.status = transition.status;
    experiment.revision += 1;
    experiment.updated_at = transition.occurred_at;
    Ok(())
}

/// Apply one event to a case projection.
///
/// Duplicate event IDs are idempotent only when replaying an already-applied
/// sequence. Any unknown old sequence or any gap is rejected.
pub fn apply_ledger_event(
    state: &mut LedgerSnapshot,
    envelope: &CaseLedgerEvent,
) -> Result<(), LedgerReduceError> {
    if envelope.case_id != state.case_id {
        return Err(LedgerReduceError::CrossCase {
            event_id: envelope.event_id.clone(),
            expected: state.case_id.clone(),
            actual: envelope.case_id.clone(),
        });
    }
    if state.applied_event_ids.contains(&envelope.event_id) {
        if envelope.seq <= state.projected_seq {
            return Ok(());
        }
        return Err(invalid(
            envelope,
            "duplicate event id appears at a future sequence",
        ));
    }
    let expected_seq = state.projected_seq + 1;
    if envelope.seq != expected_seq {
        return Err(LedgerReduceError::SequenceGap {
            event_id: envelope.event_id.clone(),
            expected: expected_seq,
            actual: envelope.seq,
        });
    }
    let schema_version = envelope.event.schema_version();
    if schema_version != LEDGER_EVENT_SCHEMA_VERSION {
        return Err(LedgerReduceError::UnsupportedSchema {
            event_id: envelope.event_id.clone(),
            expected: LEDGER_EVENT_SCHEMA_VERSION,
            actual: schema_version,
        });
    }

    match &envelope.event {
        LedgerEvent::HypothesisProposedV2 { hypothesis, .. } => {
            require_kind(envelope, AggregateKind::Hypothesis)?;
            require_revision(envelope, hypothesis.id.as_str(), 1)?;
            if hypothesis.case_id != state.case_id {
                return Err(invalid(envelope, "hypothesis has a different case_id"));
            }
            if hypothesis.revision != 1 || hypothesis.status != HypothesisStatus::Open {
                return Err(invalid(
                    envelope,
                    "new hypothesis must be Open at revision 1",
                ));
            }
            if hypothesis.claim.trim().is_empty() || hypothesis.claim.chars().count() > 1_000 {
                return Err(invalid(
                    envelope,
                    "hypothesis claim must contain 1..=1000 characters",
                ));
            }
            if state.hypotheses.contains_key(&hypothesis.id) {
                return Err(invalid(
                    envelope,
                    format!("hypothesis {} already exists", hypothesis.id),
                ));
            }
            for premise in &hypothesis.premise_refs {
                match premise {
                    LedgerRef::Evidence(id) if !state.evidence.contains_key(id) => {
                        return Err(invalid(envelope, format!("unknown evidence premise {id}")));
                    }
                    LedgerRef::Resolution(id) if !state.resolutions.contains_key(id) => {
                        return Err(invalid(
                            envelope,
                            format!("unknown resolution premise {id}"),
                        ));
                    }
                    _ => {}
                }
            }
            state
                .hypotheses
                .insert(hypothesis.id.clone(), hypothesis.clone());
        }
        LedgerEvent::PredictionDeclaredV2 { prediction, .. } => {
            require_kind(envelope, AggregateKind::Prediction)?;
            require_revision(envelope, prediction.id.as_str(), 1)?;
            if prediction.case_id != state.case_id || prediction.revision != 1 {
                return Err(invalid(
                    envelope,
                    "new prediction must belong to this case at revision 1",
                ));
            }
            if prediction.falsifier.trim().is_empty() {
                return Err(invalid(envelope, "prediction falsifier cannot be empty"));
            }
            if !state.hypotheses.contains_key(&prediction.hypothesis_id) {
                return Err(invalid(
                    envelope,
                    format!("unknown hypothesis {}", prediction.hypothesis_id),
                ));
            }
            if state.predictions.contains_key(&prediction.id) {
                return Err(invalid(
                    envelope,
                    format!("prediction {} already exists", prediction.id),
                ));
            }
            state
                .predictions
                .insert(prediction.id.clone(), prediction.clone());
        }
        LedgerEvent::ExperimentPlannedV2 { experiment, .. } => {
            require_kind(envelope, AggregateKind::Experiment)?;
            require_revision(envelope, experiment.id.as_str(), 1)?;
            if experiment.case_id != state.case_id
                || experiment.revision != 1
                || experiment.status != ExperimentStatus::Planned
            {
                return Err(invalid(
                    envelope,
                    "new experiment must be Planned in this case at revision 1",
                ));
            }
            if experiment.idempotency_key.trim().is_empty() {
                return Err(invalid(
                    envelope,
                    "experiment idempotency_key cannot be empty",
                ));
            }
            for id in &experiment.hypothesis_ids {
                if !state.hypotheses.contains_key(id) {
                    return Err(invalid(envelope, format!("unknown hypothesis {id}")));
                }
            }
            for id in &experiment.prediction_ids {
                let prediction = state
                    .predictions
                    .get(id)
                    .ok_or_else(|| invalid(envelope, format!("unknown prediction {id}")))?;
                if !experiment
                    .hypothesis_ids
                    .contains(&prediction.hypothesis_id)
                {
                    return Err(invalid(
                        envelope,
                        format!("prediction {id} is outside experiment hypotheses"),
                    ));
                }
            }
            if state.experiments.values().any(|existing| {
                existing.idempotency_key == experiment.idempotency_key
                    && existing.id != experiment.id
            }) {
                return Err(invalid(
                    envelope,
                    "experiment idempotency_key is already in use",
                ));
            }
            if state.experiments.contains_key(&experiment.id) {
                return Err(invalid(
                    envelope,
                    format!("experiment {} already exists", experiment.id),
                ));
            }
            state
                .experiments
                .insert(experiment.id.clone(), experiment.clone());
        }
        LedgerEvent::ExperimentQueuedV2 {
            experiment_id,
            expected_revision,
            task_id,
            occurred_at,
            ..
        } => transition_experiment(
            state,
            envelope,
            experiment_id,
            ExperimentTransition {
                expected_revision: *expected_revision,
                status: ExperimentStatus::Queued,
                occurred_at: *occurred_at,
                task_id: Some(task_id),
                attempt: None,
                evidence_ids: None,
            },
        )?,
        LedgerEvent::ExperimentStartedV2 {
            experiment_id,
            expected_revision,
            attempt,
            occurred_at,
            ..
        } => transition_experiment(
            state,
            envelope,
            experiment_id,
            ExperimentTransition {
                expected_revision: *expected_revision,
                status: ExperimentStatus::Running,
                occurred_at: *occurred_at,
                task_id: None,
                attempt: Some(*attempt),
                evidence_ids: None,
            },
        )?,
        LedgerEvent::ExperimentObservedV2 {
            experiment_id,
            expected_revision,
            evidence_ids,
            occurred_at,
            ..
        } => transition_experiment(
            state,
            envelope,
            experiment_id,
            ExperimentTransition {
                expected_revision: *expected_revision,
                status: ExperimentStatus::Observed,
                occurred_at: *occurred_at,
                task_id: None,
                attempt: None,
                evidence_ids: Some(evidence_ids),
            },
        )?,
        LedgerEvent::ExperimentBlockedV2 {
            experiment_id,
            expected_revision,
            occurred_at,
            ..
        } => transition_experiment(
            state,
            envelope,
            experiment_id,
            ExperimentTransition {
                expected_revision: *expected_revision,
                status: ExperimentStatus::Blocked,
                occurred_at: *occurred_at,
                task_id: None,
                attempt: None,
                evidence_ids: None,
            },
        )?,
        LedgerEvent::ExperimentFailedV2 {
            experiment_id,
            expected_revision,
            occurred_at,
            ..
        } => transition_experiment(
            state,
            envelope,
            experiment_id,
            ExperimentTransition {
                expected_revision: *expected_revision,
                status: ExperimentStatus::Failed,
                occurred_at: *occurred_at,
                task_id: None,
                attempt: None,
                evidence_ids: None,
            },
        )?,
        LedgerEvent::ExperimentCancelledV2 {
            experiment_id,
            expected_revision,
            occurred_at,
            ..
        } => transition_experiment(
            state,
            envelope,
            experiment_id,
            ExperimentTransition {
                expected_revision: *expected_revision,
                status: ExperimentStatus::Cancelled,
                occurred_at: *occurred_at,
                task_id: None,
                attempt: None,
                evidence_ids: None,
            },
        )?,
        LedgerEvent::ExperimentExpiredV2 {
            experiment_id,
            expected_revision,
            occurred_at,
            ..
        } => transition_experiment(
            state,
            envelope,
            experiment_id,
            ExperimentTransition {
                expected_revision: *expected_revision,
                status: ExperimentStatus::Expired,
                occurred_at: *occurred_at,
                task_id: None,
                attempt: None,
                evidence_ids: None,
            },
        )?,
        LedgerEvent::EvidenceRecordedV2 { evidence, .. } => {
            require_kind(envelope, AggregateKind::Evidence)?;
            require_revision(envelope, &evidence.id, 1)?;
            if evidence.binding.case_id != state.case_id {
                return Err(invalid(envelope, "evidence has a different case_id"));
            }
            if evidence.tool_call_id != evidence.binding.tool_call_id {
                return Err(invalid(
                    envelope,
                    "evidence tool_call_id disagrees with its binding",
                ));
            }
            if let Some(experiment_id) = &evidence.binding.experiment_id {
                let experiment = state.experiments.get(experiment_id).ok_or_else(|| {
                    invalid(envelope, format!("unknown experiment {experiment_id}"))
                })?;
                for prediction_id in &evidence.binding.prediction_ids {
                    if !experiment.prediction_ids.contains(prediction_id) {
                        return Err(invalid(
                            envelope,
                            format!("prediction {prediction_id} is not bound to the experiment"),
                        ));
                    }
                }
            } else if !evidence.binding.prediction_ids.is_empty() {
                return Err(invalid(
                    envelope,
                    "prediction-bound evidence must also bind an experiment",
                ));
            }
            if state.evidence.contains_key(&evidence.id) {
                return Err(invalid(
                    envelope,
                    format!("evidence {} already exists", evidence.id),
                ));
            }
            state.evidence.insert(evidence.id.clone(), evidence.clone());
        }
        LedgerEvent::EvidenceLinkedV2 { link, .. } => {
            require_kind(envelope, AggregateKind::EvidenceLink)?;
            require_revision(envelope, link.id.as_str(), 1)?;
            if link.case_id != state.case_id {
                return Err(invalid(envelope, "evidence link has a different case_id"));
            }
            let evidence = state.evidence.get(&link.evidence_id).ok_or_else(|| {
                invalid(envelope, format!("unknown evidence {}", link.evidence_id))
            })?;
            if !state.hypotheses.contains_key(&link.hypothesis_id) {
                return Err(invalid(
                    envelope,
                    format!("unknown hypothesis {}", link.hypothesis_id),
                ));
            }
            if let Some(prediction_id) = &link.prediction_id {
                let prediction = state.predictions.get(prediction_id).ok_or_else(|| {
                    invalid(envelope, format!("unknown prediction {prediction_id}"))
                })?;
                if prediction.hypothesis_id != link.hypothesis_id {
                    return Err(invalid(
                        envelope,
                        "prediction does not belong to the linked hypothesis",
                    ));
                }
                if !evidence.binding.prediction_ids.contains(prediction_id) {
                    return Err(invalid(
                        envelope,
                        "evidence binding does not contain the linked prediction",
                    ));
                }
            }
            if state.evidence_links.values().any(|existing| {
                existing.evidence_id == link.evidence_id
                    && existing.hypothesis_id == link.hypothesis_id
                    && existing.prediction_id == link.prediction_id
                    && existing.relation == link.relation
            }) {
                return Err(invalid(envelope, "duplicate semantic evidence link"));
            }
            if link.rationale.chars().count() > 800 {
                return Err(invalid(
                    envelope,
                    "evidence link rationale exceeds 800 characters",
                ));
            }
            state.evidence_links.insert(link.id.clone(), link.clone());
        }
        LedgerEvent::EvidenceLinkRejectedV2 { hypothesis_id, .. }
        | LedgerEvent::ResolutionRejectedV2 { hypothesis_id, .. } => {
            require_kind(envelope, AggregateKind::Hypothesis)?;
            let hypothesis = state
                .hypotheses
                .get(hypothesis_id)
                .ok_or_else(|| invalid(envelope, format!("unknown hypothesis {hypothesis_id}")))?;
            require_revision(envelope, hypothesis_id.as_str(), hypothesis.revision)?;
        }
        LedgerEvent::ContradictionDetectedV2 { contradiction, .. } => {
            require_kind(envelope, AggregateKind::Hypothesis)?;
            if contradiction.case_id != state.case_id {
                return Err(invalid(envelope, "contradiction has a different case_id"));
            }
            let hypothesis = state
                .hypotheses
                .get(&contradiction.hypothesis_id)
                .ok_or_else(|| {
                    invalid(
                        envelope,
                        format!("unknown hypothesis {}", contradiction.hypothesis_id),
                    )
                })?;
            require_revision(
                envelope,
                contradiction.hypothesis_id.as_str(),
                hypothesis.revision,
            )?;
            for link_id in &contradiction.evidence_link_ids {
                let link = state
                    .evidence_links
                    .get(link_id)
                    .ok_or_else(|| invalid(envelope, format!("unknown evidence link {link_id}")))?;
                if link.hypothesis_id != contradiction.hypothesis_id {
                    return Err(invalid(
                        envelope,
                        format!("evidence link {link_id} belongs to another hypothesis"),
                    ));
                }
            }
            if state.contradictions.contains_key(&contradiction.id) {
                return Err(invalid(
                    envelope,
                    format!("contradiction {} already exists", contradiction.id),
                ));
            }
            state
                .contradictions
                .insert(contradiction.id.clone(), contradiction.clone());
        }
        LedgerEvent::ResolutionRequestedV2 {
            hypothesis_id,
            expected_revision,
            evidence_link_ids,
            ..
        } => {
            require_kind(envelope, AggregateKind::Hypothesis)?;
            let hypothesis = state
                .hypotheses
                .get(hypothesis_id)
                .ok_or_else(|| invalid(envelope, format!("unknown hypothesis {hypothesis_id}")))?;
            if hypothesis.revision != *expected_revision {
                return Err(LedgerReduceError::RevisionConflict {
                    aggregate_id: hypothesis_id.to_string(),
                    expected: hypothesis.revision,
                    actual: *expected_revision,
                });
            }
            require_revision(envelope, hypothesis_id.as_str(), *expected_revision)?;
            for link_id in evidence_link_ids {
                let link = state
                    .evidence_links
                    .get(link_id)
                    .ok_or_else(|| invalid(envelope, format!("unknown evidence link {link_id}")))?;
                if link.hypothesis_id != *hypothesis_id {
                    return Err(invalid(
                        envelope,
                        format!("evidence link {link_id} belongs to another hypothesis"),
                    ));
                }
            }
        }
        LedgerEvent::HypothesisResolvedV2 {
            expected_revision,
            resolution,
            ..
        } => {
            require_kind(envelope, AggregateKind::Hypothesis)?;
            if resolution.case_id != state.case_id {
                return Err(invalid(envelope, "resolution has a different case_id"));
            }
            let hypothesis = state
                .hypotheses
                .get_mut(&resolution.hypothesis_id)
                .ok_or_else(|| {
                    invalid(
                        envelope,
                        format!("unknown hypothesis {}", resolution.hypothesis_id),
                    )
                })?;
            if hypothesis.revision != *expected_revision {
                return Err(LedgerReduceError::RevisionConflict {
                    aggregate_id: resolution.hypothesis_id.to_string(),
                    expected: hypothesis.revision,
                    actual: *expected_revision,
                });
            }
            require_revision(
                envelope,
                resolution.hypothesis_id.as_str(),
                *expected_revision + 1,
            )?;
            if hypothesis.status != HypothesisStatus::Open {
                return Err(invalid(envelope, "only an Open hypothesis can be resolved"));
            }
            for link_id in resolution
                .evidence_link_ids
                .iter()
                .chain(&resolution.unresolved_contradiction_ids)
            {
                let link = state
                    .evidence_links
                    .get(link_id)
                    .ok_or_else(|| invalid(envelope, format!("unknown evidence link {link_id}")))?;
                if link.hypothesis_id != resolution.hypothesis_id {
                    return Err(invalid(
                        envelope,
                        format!("evidence link {link_id} belongs to another hypothesis"),
                    ));
                }
            }
            if state.resolutions.contains_key(&resolution.id) {
                return Err(invalid(
                    envelope,
                    format!("resolution {} already exists", resolution.id),
                ));
            }
            hypothesis.status = match resolution.status {
                ResolvedStatus::Confirmed => HypothesisStatus::Confirmed,
                ResolvedStatus::Rejected => HypothesisStatus::Rejected,
                ResolvedStatus::Inconclusive => HypothesisStatus::Inconclusive,
                ResolvedStatus::Superseded => HypothesisStatus::Superseded,
            };
            hypothesis.revision += 1;
            hypothesis.updated_at = resolution.created_at;
            state
                .resolutions
                .insert(resolution.id.clone(), resolution.clone());
        }
        LedgerEvent::HypothesisReopenedV2 {
            hypothesis_id,
            expected_revision,
            occurred_at,
            ..
        } => {
            require_kind(envelope, AggregateKind::Hypothesis)?;
            let hypothesis = state
                .hypotheses
                .get_mut(hypothesis_id)
                .ok_or_else(|| invalid(envelope, format!("unknown hypothesis {hypothesis_id}")))?;
            if hypothesis.revision != *expected_revision {
                return Err(LedgerReduceError::RevisionConflict {
                    aggregate_id: hypothesis_id.to_string(),
                    expected: hypothesis.revision,
                    actual: *expected_revision,
                });
            }
            require_revision(envelope, hypothesis_id.as_str(), *expected_revision + 1)?;
            if hypothesis.status != HypothesisStatus::Inconclusive {
                return Err(invalid(
                    envelope,
                    "only an Inconclusive hypothesis can be reopened",
                ));
            }
            hypothesis.status = HypothesisStatus::Open;
            hypothesis.revision += 1;
            hypothesis.updated_at = *occurred_at;
        }
        LedgerEvent::HypothesisSupersededV2 {
            hypothesis_id,
            expected_revision,
            superseded_by,
            occurred_at,
            ..
        } => {
            require_kind(envelope, AggregateKind::Hypothesis)?;
            if hypothesis_id == superseded_by || !state.hypotheses.contains_key(superseded_by) {
                return Err(invalid(
                    envelope,
                    "superseding hypothesis must be a distinct existing hypothesis",
                ));
            }
            let hypothesis = state
                .hypotheses
                .get_mut(hypothesis_id)
                .ok_or_else(|| invalid(envelope, format!("unknown hypothesis {hypothesis_id}")))?;
            if hypothesis.revision != *expected_revision {
                return Err(LedgerReduceError::RevisionConflict {
                    aggregate_id: hypothesis_id.to_string(),
                    expected: hypothesis.revision,
                    actual: *expected_revision,
                });
            }
            require_revision(envelope, hypothesis_id.as_str(), *expected_revision + 1)?;
            if hypothesis.status == HypothesisStatus::Superseded {
                return Err(invalid(envelope, "hypothesis is already Superseded"));
            }
            hypothesis.status = HypothesisStatus::Superseded;
            hypothesis.revision += 1;
            hypothesis.updated_at = *occurred_at;
        }
        LedgerEvent::DeliberationCommittedV2 { commit, .. } => {
            require_kind(envelope, AggregateKind::Deliberation)?;
            require_revision(envelope, &commit.id, 1)?;
            if commit.case_id != state.case_id {
                return Err(invalid(
                    envelope,
                    "deliberation commit has a different case_id",
                ));
            }
            if commit.ledger_version != state.version {
                return Err(invalid(
                    envelope,
                    format!(
                        "deliberation commit read ledger version {}, current is {}",
                        commit.ledger_version, state.version
                    ),
                ));
            }
            for id in &commit.considered_hypothesis_ids {
                if !state.hypotheses.contains_key(id) {
                    return Err(invalid(envelope, format!("unknown hypothesis {id}")));
                }
            }
            for binding in &commit.executable_call_bindings {
                if !state.experiments.contains_key(&binding.experiment_id) {
                    return Err(invalid(
                        envelope,
                        format!("unknown experiment {}", binding.experiment_id),
                    ));
                }
            }
            if state.deliberation_commits.contains_key(&commit.id) {
                return Err(invalid(
                    envelope,
                    format!("deliberation commit {} already exists", commit.id),
                ));
            }
            state
                .deliberation_commits
                .insert(commit.id.clone(), commit.clone());
        }
    }

    state.projected_seq = envelope.seq;
    state.version = envelope.seq;
    state.applied_event_ids.insert(envelope.event_id.clone());
    Ok(())
}

pub fn reduce_ledger_events(
    case_id: CaseId,
    events: &[CaseLedgerEvent],
) -> Result<LedgerSnapshot, LedgerReduceError> {
    let mut state = LedgerSnapshot::empty(case_id);
    for event in events {
        apply_ledger_event(&mut state, event)?;
    }
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hypothesis(case_id: &CaseId, id: &str) -> Hypothesis {
        let now = Utc::now();
        Hypothesis {
            id: id.into(),
            case_id: case_id.clone(),
            claim: "A bounded, falsifiable claim".into(),
            premise_refs: Vec::new(),
            alternative_group: None,
            priority: Priority::High,
            status: HypothesisStatus::Open,
            revision: 1,
            created_by: "runtime".into(),
            created_at: now,
            updated_at: now,
        }
    }

    fn proposed(case_id: &CaseId, seq: u64, id: &str) -> CaseLedgerEvent {
        CaseLedgerEvent {
            case_id: case_id.clone(),
            seq,
            event_id: format!("event-{seq}"),
            aggregate_kind: AggregateKind::Hypothesis,
            aggregate_id: id.into(),
            aggregate_revision: 1,
            actor_session_id: "session-1".into(),
            event: LedgerEvent::HypothesisProposedV2 {
                schema_version: LEDGER_EVENT_SCHEMA_VERSION,
                hypothesis: hypothesis(case_id, id),
            },
            created_at: Utc::now(),
        }
    }

    #[test]
    fn reducer_is_deterministic_and_duplicate_event_is_idempotent() {
        let case_id = CaseId::from("case-1");
        let event = proposed(&case_id, 1, "hyp-1");
        let mut state = LedgerSnapshot::empty(case_id);
        apply_ledger_event(&mut state, &event).unwrap();
        let first = serde_json::to_string(&state).unwrap();
        apply_ledger_event(&mut state, &event).unwrap();
        assert_eq!(serde_json::to_string(&state).unwrap(), first);
    }

    #[test]
    fn reducer_rejects_sequence_gap_and_cross_case_event() {
        let case_id = CaseId::from("case-1");
        let mut state = LedgerSnapshot::empty(case_id.clone());
        let gap = proposed(&case_id, 2, "hyp-1");
        assert!(matches!(
            apply_ledger_event(&mut state, &gap),
            Err(LedgerReduceError::SequenceGap { .. })
        ));

        let foreign = proposed(&CaseId::from("case-2"), 1, "hyp-2");
        assert!(matches!(
            apply_ledger_event(&mut state, &foreign),
            Err(LedgerReduceError::CrossCase { .. })
        ));
    }

    #[test]
    fn reducer_rejects_revision_mismatch_and_unknown_schema() {
        let case_id = CaseId::from("case-1");
        let mut wrong_revision = proposed(&case_id, 1, "hyp-1");
        wrong_revision.aggregate_revision = 2;
        let mut state = LedgerSnapshot::empty(case_id.clone());
        assert!(matches!(
            apply_ledger_event(&mut state, &wrong_revision),
            Err(LedgerReduceError::RevisionConflict { .. })
        ));

        let mut unknown_schema = proposed(&case_id, 1, "hyp-1");
        if let LedgerEvent::HypothesisProposedV2 { schema_version, .. } = &mut unknown_schema.event
        {
            *schema_version = 99;
        }
        assert!(matches!(
            apply_ledger_event(&mut state, &unknown_schema),
            Err(LedgerReduceError::UnsupportedSchema { .. })
        ));
    }
}
