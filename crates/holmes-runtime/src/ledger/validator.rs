//! Deterministic Ledger validators. Model proposals are suggestions; these
//! functions are the authority for accepted relation strength and resolution
//! state transitions.

use holmes_core::ledger::{
    EvidenceRelation, EvidenceStrength, HypothesisStatus, LedgerSnapshot, Priority, ResolvedStatus,
    ValidatorKind,
};
use holmes_core::ToolOutcomeStatus;

use crate::decision::{EvidenceLinkProposal, ResolutionRequestProposal};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkValidation {
    Accepted {
        relation: EvidenceRelation,
        strength: EvidenceStrength,
    },
    Rejected {
        reason: String,
    },
}

fn capped_strength(proposed: &EvidenceStrength, cap: EvidenceStrength) -> EvidenceStrength {
    if proposed <= &cap {
        proposed.clone()
    } else {
        cap
    }
}

pub fn validate_link(snapshot: &LedgerSnapshot, proposal: &EvidenceLinkProposal) -> LinkValidation {
    let Some(evidence) = snapshot.evidence.get(&proposal.evidence_id) else {
        return LinkValidation::Rejected {
            reason: format!("unknown evidence {}", proposal.evidence_id),
        };
    };
    let Some(hypothesis) = snapshot.hypotheses.get(&proposal.hypothesis_id) else {
        return LinkValidation::Rejected {
            reason: format!("unknown hypothesis {}", proposal.hypothesis_id),
        };
    };
    if hypothesis.status == HypothesisStatus::Superseded {
        return LinkValidation::Rejected {
            reason: "cannot link new evidence to a Superseded hypothesis".into(),
        };
    }
    if proposal.rationale.trim().is_empty() || proposal.rationale.chars().count() > 800 {
        return LinkValidation::Rejected {
            reason: "rationale must contain 1..=800 characters".into(),
        };
    }

    if proposal.relation == EvidenceRelation::Inconclusive {
        return LinkValidation::Accepted {
            relation: EvidenceRelation::Inconclusive,
            strength: EvidenceStrength::Weak,
        };
    }

    if evidence.outcome_status != ToolOutcomeStatus::Succeeded {
        return LinkValidation::Rejected {
            reason: "failed/timeout audit outcomes can only be linked as Inconclusive".into(),
        };
    }
    let Some(prediction_id) = &proposal.prediction_id else {
        return LinkValidation::Rejected {
            reason: "Supports/Contradicts requires a declared prediction".into(),
        };
    };
    let Some(prediction) = snapshot.predictions.get(prediction_id) else {
        return LinkValidation::Rejected {
            reason: format!("unknown prediction {prediction_id}"),
        };
    };
    if prediction.hypothesis_id != proposal.hypothesis_id {
        return LinkValidation::Rejected {
            reason: "prediction belongs to another hypothesis".into(),
        };
    }
    if !evidence.binding.prediction_ids.contains(prediction_id) {
        return LinkValidation::Rejected {
            reason: "evidence was not bound to this prediction before execution".into(),
        };
    }
    if prediction.validator != proposal.validator {
        return LinkValidation::Rejected {
            reason: "proposal validator differs from the immutable prediction validator".into(),
        };
    }

    let cap = match proposal.validator {
        ValidatorKind::HumanAttestation | ValidatorKind::Semantic => EvidenceStrength::Moderate,
        ValidatorKind::CommandExit | ValidatorKind::NetworkDifferential => EvidenceStrength::Strong,
        ValidatorKind::FilePostcondition
        | ValidatorKind::CodeTest
        | ValidatorKind::SecurityReproduction => EvidenceStrength::Strong,
    };
    LinkValidation::Accepted {
        relation: proposal.relation.clone(),
        strength: capped_strength(&proposal.strength, cap),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolutionValidation {
    Accepted,
    Rejected { gaps: Vec<String> },
}

pub fn validate_resolution(
    snapshot: &LedgerSnapshot,
    request: &ResolutionRequestProposal,
) -> ResolutionValidation {
    let Some(hypothesis) = snapshot.hypotheses.get(&request.hypothesis_id) else {
        return ResolutionValidation::Rejected {
            gaps: vec![format!("unknown hypothesis {}", request.hypothesis_id)],
        };
    };
    let mut gaps = Vec::new();
    if hypothesis.status != HypothesisStatus::Open {
        gaps.push(format!(
            "hypothesis is {:?}; only Open hypotheses can be resolved",
            hypothesis.status
        ));
    }
    if hypothesis.revision != request.expected_revision {
        gaps.push(format!(
            "stale hypothesis revision: expected {}, current {}",
            request.expected_revision, hypothesis.revision
        ));
    }

    let mut links = Vec::new();
    for id in &request.evidence_link_ids {
        match snapshot.evidence_links.get(id) {
            Some(link) if link.hypothesis_id == request.hypothesis_id => links.push(link),
            Some(_) => gaps.push(format!("evidence link {id} belongs to another hypothesis")),
            None => gaps.push(format!("unknown evidence link {id}")),
        }
    }
    let strong_supports = links.iter().any(|link| {
        link.relation == EvidenceRelation::Supports && link.strength >= EvidenceStrength::Strong
    });
    let any_supports = links
        .iter()
        .any(|link| link.relation == EvidenceRelation::Supports);
    let strong_contradictions = snapshot.evidence_links.values().any(|link| {
        link.hypothesis_id == request.hypothesis_id
            && link.relation == EvidenceRelation::Contradicts
            && link.strength >= EvidenceStrength::Strong
    });
    let requested_strong_contradiction = links.iter().any(|link| {
        link.relation == EvidenceRelation::Contradicts && link.strength >= EvidenceStrength::Strong
    });
    let competing_strong_support = snapshot.evidence_links.values().any(|link| {
        link.hypothesis_id == request.hypothesis_id
            && link.relation == EvidenceRelation::Supports
            && link.strength >= EvidenceStrength::Strong
    });

    match request.requested_status {
        ResolvedStatus::Confirmed => {
            if !any_supports {
                gaps.push("Confirmed requires at least one Supports link".into());
            }
            if matches!(hypothesis.priority, Priority::High | Priority::Critical)
                && !strong_supports
            {
                gaps.push("High/Critical confirmation requires Strong evidence".into());
            }
            if strong_contradictions {
                gaps.push("unresolved Strong/Decisive contradiction blocks confirmation".into());
            }
            for prediction in snapshot.predictions.values().filter(|prediction| {
                prediction.hypothesis_id == request.hypothesis_id && prediction.required
            }) {
                let observed = links.iter().any(|link| {
                    link.prediction_id.as_ref() == Some(&prediction.id)
                        && link.relation == EvidenceRelation::Supports
                });
                if !observed {
                    gaps.push(format!(
                        "required prediction {} has no supporting observation",
                        prediction.id
                    ));
                }
            }
        }
        ResolvedStatus::Rejected => {
            if !requested_strong_contradiction {
                gaps.push("Rejected requires a Strong/Decisive falsifier link".into());
            }
            if competing_strong_support {
                gaps.push("competing Strong/Decisive support blocks rejection".into());
            }
        }
        ResolvedStatus::Inconclusive => {
            let terminal_experiment = snapshot.experiments.values().any(|experiment| {
                experiment.hypothesis_ids.contains(&request.hypothesis_id)
                    && matches!(
                        experiment.status,
                        holmes_core::ledger::ExperimentStatus::Observed
                            | holmes_core::ledger::ExperimentStatus::Blocked
                            | holmes_core::ledger::ExperimentStatus::Failed
                            | holmes_core::ledger::ExperimentStatus::Cancelled
                            | holmes_core::ledger::ExperimentStatus::Expired
                    )
            });
            if links.is_empty() && !terminal_experiment {
                gaps.push(
                    "Inconclusive requires bounded-search evidence or a terminal experiment".into(),
                );
            }
        }
        ResolvedStatus::Superseded => {
            gaps.push("use HypothesisSupersededV2 with an explicit replacement".into());
        }
    }

    if gaps.is_empty() {
        ResolutionValidation::Accepted
    } else {
        ResolutionValidation::Rejected { gaps }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_evidence_link_fails_closed() {
        let snapshot = LedgerSnapshot::empty("case-1".into());
        let proposal = EvidenceLinkProposal {
            evidence_id: "ev-1".into(),
            hypothesis_id: "hyp-1".into(),
            prediction_id: None,
            relation: EvidenceRelation::Supports,
            strength: EvidenceStrength::Decisive,
            rationale: "claim".into(),
            validator: ValidatorKind::Semantic,
        };
        assert!(matches!(
            validate_link(&snapshot, &proposal),
            LinkValidation::Rejected { .. }
        ));
    }
}
