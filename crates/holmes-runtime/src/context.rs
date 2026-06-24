use std::collections::HashSet;
use std::sync::Arc;

use holmes_core::config::HolmesConfig;
use holmes_core::session::RuntimeSession;
use holmes_core::state::{AttackHypothesis, AttackPhase, AttackState};
use holmes_core::types::SessionMode;
use holmes_guards::GuardChain;
use holmes_mind_palace::MindPalace;
use holmes_session::{memory_store::MemoryStore, SessionDB};
use holmes_tools::registry::ToolRegistry;

use crate::deliberation::LlmBackend;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InteractionMode {
    Autonomous,
    Interactive,
}

impl Default for InteractionMode {
    fn default() -> Self {
        Self::Interactive
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimePhase {
    Initializing,
    Recon,
    Hypothesize,
    Validate,
    Exploit,
    Complete,
}

impl Default for RuntimePhase {
    fn default() -> Self {
        Self::Initializing
    }
}

#[derive(Debug, Clone, Default)]
pub struct EvidenceProjectionDedupe {
    pub seen_ports: HashSet<String>,
    pub seen_tech: HashSet<String>,
    pub seen_endpoints: HashSet<String>,
    pub seen_credentials: HashSet<String>,
    pub seen_findings: HashSet<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeductionLedgerDedupe {
    pub evidence_ids: HashSet<String>,
    pub fact_ids: HashSet<String>,
    pub hypothesis_ids: HashSet<String>,
    pub prediction_ids: HashSet<String>,
    pub experiment_ids: HashSet<String>,
    pub support_ids: HashSet<String>,
    pub contradiction_ids: HashSet<String>,
    pub rejected_hypotheses: HashSet<String>,
    pub confirmed_hypotheses: HashSet<String>,
    pub conclusions: HashSet<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DeductionHypothesisStatus {
    Proposed,
    Supported,
    Contradicted,
    Rejected,
    Confirmed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeductionHypothesisState {
    pub id: String,
    pub statement: String,
    pub confidence: f32,
    pub status: DeductionHypothesisStatus,
    pub attack_type: Option<String>,
    pub entry_points: Vec<String>,
    pub supporting_evidence: Vec<String>,
    pub contradicting_evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeductionEvidenceState {
    pub id: String,
    pub summary: String,
    pub source: String,
    pub confidence: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeductionFactState {
    pub id: String,
    pub statement: String,
    pub evidence_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeductionPredictionState {
    pub hypothesis_id: String,
    pub prediction: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeductionExperimentState {
    pub hypothesis_id: String,
    pub action: String,
    pub distinguishes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeductionConclusionState {
    pub conclusion: String,
    pub supporting_hypotheses: Vec<String>,
    pub evidence_ids: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeductionState {
    pub ledger: DeductionLedgerDedupe,
    pub evidence: Vec<DeductionEvidenceState>,
    pub facts: Vec<DeductionFactState>,
    pub hypotheses: Vec<DeductionHypothesisState>,
    pub predictions: Vec<DeductionPredictionState>,
    pub experiments: Vec<DeductionExperimentState>,
    pub conclusions: Vec<DeductionConclusionState>,
}

pub struct RuntimeState {
    pub interaction_mode: InteractionMode,
    pub session_mode: SessionMode,
    pub phase: RuntimePhase,
    pub active_goal: Option<String>,
    pub active_hypotheses: Vec<AttackHypothesis>,
    pub observations: Vec<String>,
    pub recalled_memories: Vec<RuntimeMemory>,
    pub failures: Vec<String>,
    pub compatibility_state: AttackState,
    pub evidence_projection: EvidenceProjectionDedupe,
    pub deduction: DeductionState,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeMemory {
    pub id: String,
    pub content: String,
    pub relevance_score: f64,
}

impl RuntimeState {
    pub fn new(session_mode: SessionMode) -> Self {
        Self {
            interaction_mode: InteractionMode::default(),
            session_mode,
            phase: RuntimePhase::default(),
            active_goal: None,
            active_hypotheses: Vec::new(),
            observations: Vec::new(),
            recalled_memories: Vec::new(),
            failures: Vec::new(),
            compatibility_state: permissive_attack_state(),
            evidence_projection: EvidenceProjectionDedupe::default(),
            deduction: DeductionState::default(),
        }
    }
}

impl Default for RuntimeState {
    fn default() -> Self {
        Self::new(SessionMode::default())
    }
}

pub struct RuntimeContext {
    pub session: RuntimeSession,
    pub session_id: String,
    pub session_db: Arc<SessionDB>,
    pub memory_store: Arc<MemoryStore>,
    pub mind_palace: MindPalace,
    pub llm: Arc<dyn LlmBackend>,
    pub tools: Arc<ToolRegistry>,
    pub guards: GuardChain,
    pub state: RuntimeState,
    pub config: HolmesConfig,
}

impl RuntimeContext {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session: RuntimeSession,
        session_db: Arc<SessionDB>,
        memory_store: Arc<MemoryStore>,
        mind_palace: MindPalace,
        llm: Arc<dyn LlmBackend>,
        tools: Arc<ToolRegistry>,
        guards: GuardChain,
        state: RuntimeState,
        config: HolmesConfig,
    ) -> Self {
        let session_id = session.id.clone();
        Self {
            session,
            session_id,
            session_db,
            memory_store,
            mind_palace,
            llm,
            tools,
            guards,
            state,
            config,
        }
    }
}

fn permissive_attack_state() -> AttackState {
    let mut state = AttackState::new(
        String::new(),
        String::new(),
        "runtime".into(),
        "Holmes Runtime".into(),
        Vec::new(),
    );
    state.phase = AttackPhase::Recon;
    state.current_objective = "runtime bootstrap".into();
    state
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_state_defaults_are_phase_one_ready() {
        let state = RuntimeState::default();

        assert_eq!(state.interaction_mode, InteractionMode::Interactive);
        assert_eq!(state.session_mode, SessionMode::Pentest);
        assert_eq!(state.phase, RuntimePhase::Initializing);
        assert_eq!(state.active_goal, None);
        assert!(state.active_hypotheses.is_empty());
        assert!(state.observations.is_empty());
        assert!(state.recalled_memories.is_empty());
        assert!(state.failures.is_empty());
        assert_eq!(state.compatibility_state.phase, AttackPhase::Recon);
        assert!(!state.compatibility_state.is_finished);
        assert!(state.evidence_projection.seen_ports.is_empty());
        assert!(state.evidence_projection.seen_tech.is_empty());
        assert!(state.evidence_projection.seen_endpoints.is_empty());
        assert!(state.evidence_projection.seen_credentials.is_empty());
        assert!(state.evidence_projection.seen_findings.is_empty());
        assert!(state.deduction.ledger.evidence_ids.is_empty());
        assert!(state.deduction.ledger.hypothesis_ids.is_empty());
        assert!(state.deduction.evidence.is_empty());
        assert!(state.deduction.facts.is_empty());
        assert!(state.deduction.hypotheses.is_empty());
        assert!(state.deduction.predictions.is_empty());
        assert!(state.deduction.experiments.is_empty());
        assert!(state.deduction.conclusions.is_empty());
    }
}
