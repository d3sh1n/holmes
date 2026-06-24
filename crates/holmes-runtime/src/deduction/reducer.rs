use holmes_core::event::Event;
use holmes_core::state::AttackHypothesis;

use crate::context::{
    DeductionConclusionState, DeductionEvidenceState, DeductionExperimentState, DeductionFactState,
    DeductionHypothesisState, DeductionHypothesisStatus, DeductionLedgerDedupe,
    DeductionPredictionState, DeductionState, RuntimeState,
};

pub(crate) struct DeductionReducer;

impl DeductionReducer {
    pub(crate) fn already_applied(state: &RuntimeState, event: &Event) -> bool {
        let Some(key) = DeductionEventKey::from_event(event) else {
            return false;
        };
        contains_key(&state.deduction.ledger, &key)
    }

    pub(crate) fn apply(state: &mut RuntimeState, event: &Event) {
        let Some(key) = DeductionEventKey::from_event(event) else {
            return;
        };
        if !insert_key(&mut state.deduction.ledger, key) {
            return;
        }

        match event {
            Event::EvidenceObserved {
                evidence_id,
                summary,
                source,
                confidence,
            } => {
                state.deduction.evidence.push(DeductionEvidenceState {
                    id: evidence_id.clone(),
                    summary: summary.clone(),
                    source: source.clone(),
                    confidence: confidence.clone(),
                });
            }
            Event::FactRecorded {
                fact_id,
                statement,
                evidence_ids,
            } => {
                state.deduction.facts.push(DeductionFactState {
                    id: fact_id.clone(),
                    statement: statement.clone(),
                    evidence_ids: evidence_ids.clone(),
                });
            }
            Event::HypothesisProposed {
                hypothesis_id,
                statement,
                rationale,
                confidence,
                attack_type,
                entry_points,
            } => {
                let confidence = confidence.unwrap_or(0.5);
                upsert_hypothesis_state(
                    &mut state.deduction,
                    hypothesis_id,
                    statement,
                    confidence,
                    DeductionHypothesisStatus::Proposed,
                    attack_type.clone(),
                    entry_points.clone(),
                );
                if let Some(attack_type) = attack_type.as_deref() {
                    ensure_active_attack_hypothesis(
                        state,
                        attack_type,
                        confidence,
                        rationale,
                        entry_points,
                    );
                }
            }
            Event::PredictionMade {
                hypothesis_id,
                prediction,
            } => {
                state.deduction.predictions.push(DeductionPredictionState {
                    hypothesis_id: hypothesis_id.clone(),
                    prediction: prediction.clone(),
                });
            }
            Event::ExperimentPlanned {
                hypothesis_id,
                action,
                distinguishes,
            } => {
                state.deduction.experiments.push(DeductionExperimentState {
                    hypothesis_id: hypothesis_id.clone(),
                    action: action.clone(),
                    distinguishes: distinguishes.clone(),
                });
            }
            Event::HypothesisSupported {
                hypothesis_id,
                evidence_id,
                confidence,
                ..
            } => {
                let confidence = confidence.unwrap_or(0.72);
                update_hypothesis_state(
                    &mut state.deduction,
                    hypothesis_id,
                    DeductionHypothesisStatus::Supported,
                    confidence,
                    Some(evidence_id),
                    None,
                );
                update_active_attack_hypothesis(state, hypothesis_id, confidence);
            }
            Event::HypothesisContradicted {
                hypothesis_id,
                evidence_id,
                confidence,
                ..
            } => {
                let confidence = confidence.unwrap_or(0.2);
                update_hypothesis_state(
                    &mut state.deduction,
                    hypothesis_id,
                    DeductionHypothesisStatus::Contradicted,
                    confidence,
                    None,
                    Some(evidence_id),
                );
                update_active_attack_hypothesis(state, hypothesis_id, confidence);
            }
            Event::HypothesisRejected { hypothesis_id, .. } => {
                update_hypothesis_state(
                    &mut state.deduction,
                    hypothesis_id,
                    DeductionHypothesisStatus::Rejected,
                    0.0,
                    None,
                    None,
                );
                let attack_type = state
                    .deduction
                    .hypotheses
                    .iter()
                    .find(|hypothesis| hypothesis.id == *hypothesis_id)
                    .and_then(|hypothesis| hypothesis.attack_type.as_deref());
                state.active_hypotheses.retain(|hypothesis| {
                    attack_type
                        .map(|attack_type| hypothesis.attack_type != attack_type)
                        .unwrap_or_else(|| {
                            !active_hypothesis_matches(hypothesis_id, &hypothesis.attack_type)
                        })
                });
            }
            Event::HypothesisConfirmed {
                hypothesis_id,
                confidence,
                ..
            } => {
                let confidence = confidence.unwrap_or(0.92);
                update_hypothesis_state(
                    &mut state.deduction,
                    hypothesis_id,
                    DeductionHypothesisStatus::Confirmed,
                    confidence,
                    None,
                    None,
                );
                update_active_attack_hypothesis(state, hypothesis_id, confidence);
            }
            Event::ConclusionDrawn {
                conclusion,
                supporting_hypotheses,
                evidence_ids,
            } => {
                state.deduction.conclusions.push(DeductionConclusionState {
                    conclusion: conclusion.clone(),
                    supporting_hypotheses: supporting_hypotheses.clone(),
                    evidence_ids: evidence_ids.clone(),
                });
            }
            _ => {}
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DeductionEventKey {
    Evidence(String),
    Fact(String),
    Hypothesis(String),
    Prediction(String),
    Experiment(String),
    Support(String),
    Contradiction(String),
    Rejection(String),
    Confirmation(String),
    Conclusion(String),
}

impl DeductionEventKey {
    pub(crate) fn from_event(event: &Event) -> Option<Self> {
        match event {
            Event::EvidenceObserved { evidence_id, .. } => {
                Some(Self::Evidence(evidence_id.clone()))
            }
            Event::FactRecorded { fact_id, .. } => Some(Self::Fact(fact_id.clone())),
            Event::HypothesisProposed { hypothesis_id, .. } => {
                Some(Self::Hypothesis(hypothesis_id.clone()))
            }
            Event::PredictionMade {
                hypothesis_id,
                prediction,
            } => Some(Self::Prediction(format!("{hypothesis_id}:{prediction}"))),
            Event::ExperimentPlanned {
                hypothesis_id,
                action,
                ..
            } => Some(Self::Experiment(format!("{hypothesis_id}:{action}"))),
            Event::HypothesisSupported {
                hypothesis_id,
                evidence_id,
                ..
            } => Some(Self::Support(format!("{hypothesis_id}:{evidence_id}"))),
            Event::HypothesisContradicted {
                hypothesis_id,
                evidence_id,
                ..
            } => Some(Self::Contradiction(format!(
                "{hypothesis_id}:{evidence_id}"
            ))),
            Event::HypothesisRejected { hypothesis_id, .. } => {
                Some(Self::Rejection(hypothesis_id.clone()))
            }
            Event::HypothesisConfirmed { hypothesis_id, .. } => {
                Some(Self::Confirmation(hypothesis_id.clone()))
            }
            Event::ConclusionDrawn { conclusion, .. } => Some(Self::Conclusion(conclusion.clone())),
            _ => None,
        }
    }
}

fn contains_key(ledger: &DeductionLedgerDedupe, key: &DeductionEventKey) -> bool {
    match key {
        DeductionEventKey::Evidence(value) => ledger.evidence_ids.contains(value),
        DeductionEventKey::Fact(value) => ledger.fact_ids.contains(value),
        DeductionEventKey::Hypothesis(value) => ledger.hypothesis_ids.contains(value),
        DeductionEventKey::Prediction(value) => ledger.prediction_ids.contains(value),
        DeductionEventKey::Experiment(value) => ledger.experiment_ids.contains(value),
        DeductionEventKey::Support(value) => ledger.support_ids.contains(value),
        DeductionEventKey::Contradiction(value) => ledger.contradiction_ids.contains(value),
        DeductionEventKey::Rejection(value) => ledger.rejected_hypotheses.contains(value),
        DeductionEventKey::Confirmation(value) => ledger.confirmed_hypotheses.contains(value),
        DeductionEventKey::Conclusion(value) => ledger.conclusions.contains(value),
    }
}

fn insert_key(ledger: &mut DeductionLedgerDedupe, key: DeductionEventKey) -> bool {
    match key {
        DeductionEventKey::Evidence(value) => ledger.evidence_ids.insert(value),
        DeductionEventKey::Fact(value) => ledger.fact_ids.insert(value),
        DeductionEventKey::Hypothesis(value) => ledger.hypothesis_ids.insert(value),
        DeductionEventKey::Prediction(value) => ledger.prediction_ids.insert(value),
        DeductionEventKey::Experiment(value) => ledger.experiment_ids.insert(value),
        DeductionEventKey::Support(value) => ledger.support_ids.insert(value),
        DeductionEventKey::Contradiction(value) => ledger.contradiction_ids.insert(value),
        DeductionEventKey::Rejection(value) => ledger.rejected_hypotheses.insert(value),
        DeductionEventKey::Confirmation(value) => ledger.confirmed_hypotheses.insert(value),
        DeductionEventKey::Conclusion(value) => ledger.conclusions.insert(value),
    }
}

fn upsert_hypothesis_state(
    deduction: &mut DeductionState,
    id: &str,
    statement: &str,
    confidence: f32,
    status: DeductionHypothesisStatus,
    attack_type: Option<String>,
    entry_points: Vec<String>,
) {
    if let Some(existing) = deduction
        .hypotheses
        .iter_mut()
        .find(|hypothesis| hypothesis.id == id)
    {
        existing.statement = statement.into();
        existing.confidence = existing.confidence.max(confidence);
        if attack_type.is_some() {
            existing.attack_type = attack_type;
        }
        if !entry_points.is_empty() {
            existing.entry_points = entry_points;
        }
        if !matches!(
            existing.status,
            DeductionHypothesisStatus::Rejected | DeductionHypothesisStatus::Confirmed
        ) {
            existing.status = status;
        }
        return;
    }

    deduction.hypotheses.push(DeductionHypothesisState {
        id: id.into(),
        statement: statement.into(),
        confidence,
        status,
        attack_type,
        entry_points,
        supporting_evidence: Vec::new(),
        contradicting_evidence: Vec::new(),
    });
}

fn update_hypothesis_state(
    deduction: &mut DeductionState,
    id: &str,
    status: DeductionHypothesisStatus,
    confidence: f32,
    supporting_evidence: Option<&str>,
    contradicting_evidence: Option<&str>,
) {
    let Some(existing) = deduction
        .hypotheses
        .iter_mut()
        .find(|hypothesis| hypothesis.id == id)
    else {
        return;
    };

    if !matches!(
        existing.status,
        DeductionHypothesisStatus::Rejected | DeductionHypothesisStatus::Confirmed
    ) || matches!(
        status,
        DeductionHypothesisStatus::Rejected | DeductionHypothesisStatus::Confirmed
    ) {
        existing.status = status;
        existing.confidence = confidence;
    }
    if let Some(evidence_id) = supporting_evidence {
        push_unique(&mut existing.supporting_evidence, evidence_id);
    }
    if let Some(evidence_id) = contradicting_evidence {
        push_unique(&mut existing.contradicting_evidence, evidence_id);
    }
}

fn ensure_active_attack_hypothesis(
    state: &mut RuntimeState,
    attack_type: &str,
    confidence: f32,
    rationale: &str,
    entry_points: &[String],
) {
    if let Some(existing) = state
        .active_hypotheses
        .iter_mut()
        .find(|hypothesis| hypothesis.attack_type == attack_type)
    {
        existing.confidence = existing.confidence.max(confidence);
        return;
    }

    state.active_hypotheses.push(AttackHypothesis {
        attack_type: attack_type.into(),
        confidence,
        reasoning: rationale.into(),
        entry_points: entry_points.to_vec(),
    });
}

fn update_active_attack_hypothesis(state: &mut RuntimeState, hypothesis_id: &str, confidence: f32) {
    let attack_type = state
        .deduction
        .hypotheses
        .iter()
        .find(|hypothesis| hypothesis.id == hypothesis_id)
        .and_then(|hypothesis| hypothesis.attack_type.as_deref());

    for hypothesis in &mut state.active_hypotheses {
        let matches = attack_type
            .map(|attack_type| hypothesis.attack_type == attack_type)
            .unwrap_or_else(|| active_hypothesis_matches(hypothesis_id, &hypothesis.attack_type));
        if matches {
            hypothesis.confidence = confidence;
        }
    }
}

fn active_hypothesis_matches(hypothesis_id: &str, attack_type: &str) -> bool {
    matches!(
        (hypothesis_id, attack_type),
        ("hypothesis-user-enumeration", "user_enumeration")
    )
}

fn push_unique(values: &mut Vec<String>, value: &str) {
    if !values.iter().any(|existing| existing == value) {
        values.push(value.into());
    }
}
