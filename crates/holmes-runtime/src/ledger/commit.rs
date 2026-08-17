//! Deterministic assembly of model-proposed Ledger meta actions.

use std::collections::{BTreeMap, BTreeSet};

use chrono::Utc;
use holmes_core::ledger::{
    apply_ledger_event, ActionBinding, ActorId, AggregateKind, CaseLedgerEvent, Contradiction,
    EvidenceLink, EvidenceLinkId, EvidenceRelation, EvidenceStrength, Experiment, ExperimentId,
    ExperimentStatus, Hypothesis, HypothesisId, HypothesisStatus, LedgerEvent, LedgerRef,
    LedgerSnapshot, Prediction, PredictionId, Resolution, ResolutionId, UnstoredLedgerEvent,
    LEDGER_EVENT_SCHEMA_VERSION,
};
use holmes_core::{SubAgentTask, ToolCall};

use crate::decision::MetaAction;
use crate::ledger::resolution_verifier::{
    requires_independent_verification, IndependentResolutionReview,
};
use crate::ledger::validator::{
    validate_link, validate_resolution, LinkValidation, ResolutionValidation,
};

#[derive(Debug, Clone)]
pub struct LedgerCommitPlan {
    pub expected_version: u64,
    pub events: Vec<UnstoredLedgerEvent>,
    pub bindings: BTreeMap<String, ActionBinding>,
    pub summaries: Vec<String>,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum LedgerCommitError {
    #[error("invalid Ledger meta action: {0}")]
    Invalid(String),
    #[error("Ledger reducer rejected assembled event: {0}")]
    Reducer(String),
}

fn runtime_id(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4())
}

fn normalized_names(values: &[String]) -> Vec<String> {
    let mut values = values
        .iter()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    values.sort();
    values.dedup();
    values
}

fn experiment_idempotency_key(
    case_id: &holmes_core::ledger::CaseId,
    hypothesis_ids: &[HypothesisId],
    prediction_ids: &[PredictionId],
    action: &str,
    tool_allowlist: &[String],
) -> String {
    let mut hypotheses = hypothesis_ids
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    hypotheses.sort();
    let mut predictions = prediction_ids
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    predictions.sort();
    let canonical = serde_json::json!({
        "case_id": case_id,
        "hypothesis_ids": hypotheses,
        "prediction_ids": predictions,
        "action": action.split_whitespace().collect::<Vec<_>>().join(" "),
        "tool_allowlist": normalized_names(tool_allowlist),
        "environment": "runtime-v1",
    });
    format!(
        "experiment:{}",
        holmes_core::content_hash(&canonical.to_string())
    )
}

fn validate_bound_tool(
    call: &ToolCall,
    tool_allowlist: &[String],
    experiment_id: &ExperimentId,
) -> Result<(), LedgerCommitError> {
    if call.function.name == "spawn_subagent" {
        let task: SubAgentTask =
            serde_json::from_str(&call.function.arguments).map_err(|error| {
                LedgerCommitError::Invalid(format!(
                    "spawn_subagent bound to {experiment_id} has invalid arguments: {error}"
                ))
            })?;
        if normalized_names(&task.constraints.tools_allowlist) != normalized_names(tool_allowlist) {
            return Err(LedgerCommitError::Invalid(format!(
                "spawn_subagent child tools_allowlist does not exactly match experiment {experiment_id} allowlist"
            )));
        }
        return Ok(());
    }
    if tool_allowlist
        .iter()
        .any(|allowed| allowed == &call.function.name)
    {
        Ok(())
    } else {
        Err(LedgerCommitError::Invalid(format!(
            "tool {} is not in experiment {} allowlist",
            call.function.name, experiment_id
        )))
    }
}

struct Assembler<'a> {
    actor_session_id: &'a str,
    working: LedgerSnapshot,
    events: Vec<UnstoredLedgerEvent>,
}

impl<'a> Assembler<'a> {
    fn push(
        &mut self,
        aggregate_kind: AggregateKind,
        aggregate_id: String,
        aggregate_revision: u64,
        event: LedgerEvent,
    ) -> Result<(), LedgerCommitError> {
        let created_at = Utc::now();
        let unstored = UnstoredLedgerEvent {
            event_id: runtime_id("event"),
            aggregate_kind: aggregate_kind.clone(),
            aggregate_id: aggregate_id.clone(),
            aggregate_revision,
            actor_session_id: self.actor_session_id.to_owned(),
            event: event.clone(),
            created_at,
        };
        let envelope = CaseLedgerEvent {
            case_id: self.working.case_id.clone(),
            seq: self.working.version + 1,
            event_id: unstored.event_id.clone(),
            aggregate_kind,
            aggregate_id,
            aggregate_revision,
            actor_session_id: self.actor_session_id.to_owned(),
            event,
            created_at,
        };
        apply_ledger_event(&mut self.working, &envelope)
            .map_err(|error| LedgerCommitError::Reducer(error.to_string()))?;
        self.events.push(unstored);
        Ok(())
    }
}

fn resolve_hypothesis_ref(
    reference: &str,
    clients: &BTreeMap<String, HypothesisId>,
    snapshot: &LedgerSnapshot,
) -> Result<HypothesisId, LedgerCommitError> {
    clients
        .get(reference)
        .cloned()
        .or_else(|| {
            let id = HypothesisId::new(reference);
            snapshot.hypotheses.contains_key(&id).then_some(id)
        })
        .ok_or_else(|| LedgerCommitError::Invalid(format!("unknown hypothesis ref {reference}")))
}

fn resolve_prediction_ref(
    reference: &str,
    clients: &BTreeMap<String, PredictionId>,
    snapshot: &LedgerSnapshot,
) -> Result<PredictionId, LedgerCommitError> {
    clients
        .get(reference)
        .cloned()
        .or_else(|| {
            let id = PredictionId::new(reference);
            snapshot.predictions.contains_key(&id).then_some(id)
        })
        .ok_or_else(|| LedgerCommitError::Invalid(format!("unknown prediction ref {reference}")))
}

fn premise_ref(value: &str, snapshot: &LedgerSnapshot) -> Result<LedgerRef, LedgerCommitError> {
    if snapshot.evidence.contains_key(value) {
        return Ok(LedgerRef::Evidence(value.to_owned()));
    }
    let resolution_id = ResolutionId::new(value);
    if snapshot.resolutions.contains_key(&resolution_id) {
        return Ok(LedgerRef::Resolution(resolution_id));
    }
    if let Some(assertion) = value.strip_prefix("assert:") {
        if !assertion.trim().is_empty() {
            return Ok(LedgerRef::UserAssertion(assertion.trim().to_owned()));
        }
    }
    Err(LedgerCommitError::Invalid(format!(
        "unknown or unverified premise ref {value}"
    )))
}

pub fn assemble_meta_commit(
    snapshot: &LedgerSnapshot,
    metas: &[MetaAction],
    executable_calls: &[ToolCall],
    actor_session_id: &str,
    resolution_reviews: &BTreeMap<HypothesisId, IndependentResolutionReview>,
) -> Result<Option<LedgerCommitPlan>, LedgerCommitError> {
    let expected_version = snapshot.version;
    let mut assembler = Assembler {
        actor_session_id,
        working: snapshot.clone(),
        events: Vec::new(),
    };
    let mut hypothesis_clients = BTreeMap::new();
    let mut prediction_clients = BTreeMap::new();
    let mut experiment_clients = BTreeMap::new();
    let mut bindings = BTreeMap::new();
    let mut bound_call_indexes = BTreeSet::new();
    let mut summaries = Vec::new();

    for meta in metas {
        match meta {
            MetaAction::SetGoal { .. } => {}
            MetaAction::ProposeHypothesis(proposal) => {
                if proposal.client_ref.trim().is_empty()
                    || hypothesis_clients.contains_key(&proposal.client_ref)
                {
                    return Err(LedgerCommitError::Invalid(
                        "hypothesis client_ref must be non-empty and unique in this commit".into(),
                    ));
                }
                let now = Utc::now();
                let hypothesis_id = HypothesisId::new(runtime_id("hyp"));
                let premise_refs = proposal
                    .premise_refs
                    .iter()
                    .map(|reference| premise_ref(reference, &assembler.working))
                    .collect::<Result<Vec<_>, _>>()?;
                let hypothesis = Hypothesis {
                    id: hypothesis_id.clone(),
                    case_id: assembler.working.case_id.clone(),
                    claim: proposal.claim.clone(),
                    premise_refs,
                    alternative_group: proposal.alternative_group.clone(),
                    priority: proposal.priority.clone(),
                    status: HypothesisStatus::Open,
                    revision: 1,
                    created_by: ActorId::new(actor_session_id),
                    created_at: now,
                    updated_at: now,
                };
                assembler.push(
                    AggregateKind::Hypothesis,
                    hypothesis_id.to_string(),
                    1,
                    LedgerEvent::HypothesisProposedV2 {
                        schema_version: LEDGER_EVENT_SCHEMA_VERSION,
                        hypothesis,
                    },
                )?;
                hypothesis_clients.insert(proposal.client_ref.clone(), hypothesis_id.clone());
                summaries.push(format!("Proposed {hypothesis_id}: {}", proposal.claim));

                for prediction_proposal in &proposal.predictions {
                    if prediction_proposal.client_ref.trim().is_empty()
                        || prediction_clients.contains_key(&prediction_proposal.client_ref)
                    {
                        return Err(LedgerCommitError::Invalid(
                            "prediction client_ref must be non-empty and unique in this commit"
                                .into(),
                        ));
                    }
                    let prediction_id = PredictionId::new(runtime_id("pred"));
                    let prediction = Prediction {
                        id: prediction_id.clone(),
                        case_id: assembler.working.case_id.clone(),
                        hypothesis_id: hypothesis_id.clone(),
                        observable: prediction_proposal.observable.clone(),
                        expected_when_true: prediction_proposal.expected_when_true.clone(),
                        falsifier: prediction_proposal.falsifier.clone(),
                        validator: prediction_proposal.validator.clone(),
                        required: prediction_proposal.required,
                        revision: 1,
                        created_at: now,
                    };
                    assembler.push(
                        AggregateKind::Prediction,
                        prediction_id.to_string(),
                        1,
                        LedgerEvent::PredictionDeclaredV2 {
                            schema_version: LEDGER_EVENT_SCHEMA_VERSION,
                            prediction,
                        },
                    )?;
                    prediction_clients
                        .insert(prediction_proposal.client_ref.clone(), prediction_id);
                }
            }
            MetaAction::PlanExperiment(proposal) => {
                if proposal.client_ref.trim().is_empty()
                    || experiment_clients.contains_key(&proposal.client_ref)
                {
                    return Err(LedgerCommitError::Invalid(
                        "experiment client_ref must be non-empty and unique in this commit".into(),
                    ));
                }
                let hypothesis_ids = proposal
                    .hypothesis_refs
                    .iter()
                    .map(|reference| {
                        resolve_hypothesis_ref(reference, &hypothesis_clients, &assembler.working)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let prediction_ids = proposal
                    .prediction_refs
                    .iter()
                    .map(|reference| {
                        resolve_prediction_ref(reference, &prediction_clients, &assembler.working)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let idempotency_key = experiment_idempotency_key(
                    &assembler.working.case_id,
                    &hypothesis_ids,
                    &prediction_ids,
                    &proposal.action,
                    &proposal.tool_allowlist,
                );
                if let Some(existing) = assembler
                    .working
                    .experiments
                    .values()
                    .find(|experiment| experiment.idempotency_key == idempotency_key)
                    .cloned()
                {
                    experiment_clients.insert(proposal.client_ref.clone(), existing.id.clone());
                    summaries.push(format!(
                        "Reused {} for duplicate experiment proposal",
                        existing.id
                    ));
                    if !proposal.bind_calls.is_empty() {
                        return Err(LedgerCommitError::Invalid(format!(
                            "experiment {} already exists in {:?}; duplicate executable calls were refused",
                            existing.id, existing.status
                        )));
                    }
                    continue;
                }

                let experiment_id = ExperimentId::new(runtime_id("exp"));
                let now = Utc::now();
                let experiment = Experiment {
                    id: experiment_id.clone(),
                    case_id: assembler.working.case_id.clone(),
                    hypothesis_ids,
                    prediction_ids: prediction_ids.clone(),
                    action: proposal.action.clone(),
                    expected_observations: proposal.expected_observations.clone(),
                    tool_allowlist: proposal.tool_allowlist.clone(),
                    risk: proposal.risk.clone(),
                    status: ExperimentStatus::Planned,
                    task_id: None,
                    attempt: 0,
                    idempotency_key,
                    evidence_ids: Vec::new(),
                    revision: 1,
                    created_by: ActorId::new(actor_session_id),
                    created_at: now,
                    updated_at: now,
                };
                assembler.push(
                    AggregateKind::Experiment,
                    experiment_id.to_string(),
                    1,
                    LedgerEvent::ExperimentPlannedV2 {
                        schema_version: LEDGER_EVENT_SCHEMA_VERSION,
                        experiment,
                    },
                )?;
                experiment_clients.insert(proposal.client_ref.clone(), experiment_id.clone());
                summaries.push(format!("Planned {experiment_id}: {}", proposal.action));

                let delegated_count = proposal
                    .bind_calls
                    .iter()
                    .filter(|index| {
                        executable_calls
                            .get(**index)
                            .is_some_and(|call| call.function.name == "spawn_subagent")
                    })
                    .count();
                if delegated_count > 0 && proposal.bind_calls.len() != 1 {
                    return Err(LedgerCommitError::Invalid(format!(
                        "delegated experiment {experiment_id} must bind exactly one spawn_subagent call"
                    )));
                }
                for index in &proposal.bind_calls {
                    let Some(call) = executable_calls.get(*index) else {
                        return Err(LedgerCommitError::Invalid(format!(
                            "bind_calls index {index} is outside {} executable calls",
                            executable_calls.len()
                        )));
                    };
                    if !bound_call_indexes.insert(*index) {
                        return Err(LedgerCommitError::Invalid(format!(
                            "executable call index {index} is bound more than once"
                        )));
                    }
                    validate_bound_tool(call, &proposal.tool_allowlist, &experiment_id)?;
                    bindings.insert(
                        call.id.clone(),
                        ActionBinding {
                            case_id: assembler.working.case_id.clone(),
                            contract_id: None,
                            requirement_ids: Vec::new(),
                            experiment_id: Some(experiment_id.clone()),
                            prediction_ids: prediction_ids.clone(),
                            tool_call_id: call.id.clone(),
                            attempt: 0,
                        },
                    );
                }
            }
            MetaAction::LinkEvidence(proposal) => match validate_link(&assembler.working, proposal)
            {
                LinkValidation::Accepted { relation, strength } => {
                    let link_id = EvidenceLinkId::new(runtime_id("el"));
                    let link = EvidenceLink {
                        id: link_id.clone(),
                        case_id: assembler.working.case_id.clone(),
                        evidence_id: proposal.evidence_id.clone(),
                        hypothesis_id: proposal.hypothesis_id.clone(),
                        prediction_id: proposal.prediction_id.clone(),
                        relation: relation.clone(),
                        strength,
                        rationale: proposal.rationale.clone(),
                        validator: proposal.validator.clone(),
                        actor: ActorId::new(actor_session_id),
                        created_at: Utc::now(),
                    };
                    let conflict_ids: Vec<EvidenceLinkId> = assembler
                        .working
                        .evidence_links
                        .values()
                        .filter(|existing| {
                            existing.evidence_id == link.evidence_id
                                && existing.hypothesis_id == link.hypothesis_id
                                && existing.prediction_id == link.prediction_id
                                && matches!(
                                    (&existing.relation, &relation),
                                    (EvidenceRelation::Supports, EvidenceRelation::Contradicts)
                                        | (
                                            EvidenceRelation::Contradicts,
                                            EvidenceRelation::Supports
                                        )
                                )
                        })
                        .map(|existing| existing.id.clone())
                        .collect();
                    assembler.push(
                        AggregateKind::EvidenceLink,
                        link_id.to_string(),
                        1,
                        LedgerEvent::EvidenceLinkedV2 {
                            schema_version: LEDGER_EVENT_SCHEMA_VERSION,
                            link,
                        },
                    )?;
                    summaries.push(format!("Linked {} as {:?}", proposal.evidence_id, relation));
                    if !conflict_ids.is_empty() {
                        let mut evidence_link_ids = conflict_ids;
                        evidence_link_ids.push(link_id);
                        let hypothesis = assembler
                            .working
                            .hypotheses
                            .get(&proposal.hypothesis_id)
                            .ok_or_else(|| {
                                LedgerCommitError::Invalid("linked hypothesis disappeared".into())
                            })?;
                        let contradiction = Contradiction {
                            id: runtime_id("contradiction"),
                            case_id: assembler.working.case_id.clone(),
                            hypothesis_id: proposal.hypothesis_id.clone(),
                            evidence_link_ids,
                            detected_at: Utc::now(),
                        };
                        assembler.push(
                            AggregateKind::Hypothesis,
                            proposal.hypothesis_id.to_string(),
                            hypothesis.revision,
                            LedgerEvent::ContradictionDetectedV2 {
                                schema_version: LEDGER_EVENT_SCHEMA_VERSION,
                                contradiction,
                            },
                        )?;
                    }
                }
                LinkValidation::Rejected { reason } => {
                    let hypothesis = assembler
                        .working
                        .hypotheses
                        .get(&proposal.hypothesis_id)
                        .ok_or_else(|| LedgerCommitError::Invalid(reason.clone()))?;
                    assembler.push(
                        AggregateKind::Hypothesis,
                        proposal.hypothesis_id.to_string(),
                        hypothesis.revision,
                        LedgerEvent::EvidenceLinkRejectedV2 {
                            schema_version: LEDGER_EVENT_SCHEMA_VERSION,
                            proposal_id: runtime_id("link-proposal"),
                            hypothesis_id: proposal.hypothesis_id.clone(),
                            reason: reason.clone(),
                        },
                    )?;
                    summaries.push(format!("Rejected evidence link: {reason}"));
                }
            },
            MetaAction::RequestResolution(request) => {
                let request_id = runtime_id("resolution-request");
                assembler.push(
                    AggregateKind::Hypothesis,
                    request.hypothesis_id.to_string(),
                    request.expected_revision,
                    LedgerEvent::ResolutionRequestedV2 {
                        schema_version: LEDGER_EVENT_SCHEMA_VERSION,
                        request_id: request_id.clone(),
                        hypothesis_id: request.hypothesis_id.clone(),
                        expected_revision: request.expected_revision,
                        requested_status: request.requested_status.clone(),
                        evidence_link_ids: request.evidence_link_ids.clone(),
                        requested_by: ActorId::new(actor_session_id),
                    },
                )?;
                match validate_resolution(&assembler.working, request) {
                    ResolutionValidation::Accepted => {
                        let independent_required =
                            requires_independent_verification(&assembler.working, request);
                        let verification = if independent_required {
                            match resolution_reviews.get(&request.hypothesis_id) {
                                Some(IndependentResolutionReview::Approved {
                                    verified_by,
                                    summary,
                                }) => Ok((verified_by.clone(), summary.clone())),
                                Some(IndependentResolutionReview::Rejected { gaps }) => {
                                    Err(gaps.clone())
                                }
                                None => Err(vec![
                                    "independent semantic verification was required but no bound verdict was available"
                                        .into(),
                                ]),
                            }
                        } else {
                            Ok((
                                ActorId::new("runtime:deterministic-validator"),
                                request.reason.clone(),
                            ))
                        };
                        let (verified_by, validator_summary) = match verification {
                            Ok(verification) => verification,
                            Err(gaps) => {
                                assembler.push(
                                    AggregateKind::Hypothesis,
                                    request.hypothesis_id.to_string(),
                                    request.expected_revision,
                                    LedgerEvent::ResolutionRejectedV2 {
                                        schema_version: LEDGER_EVENT_SCHEMA_VERSION,
                                        request_id,
                                        hypothesis_id: request.hypothesis_id.clone(),
                                        expected_revision: request.expected_revision,
                                        gaps: gaps.clone(),
                                    },
                                )?;
                                summaries.push(format!("Resolution rejected: {}", gaps.join("; ")));
                                continue;
                            }
                        };
                        let resolution_id = ResolutionId::new(runtime_id("res"));
                        let unresolved_contradiction_ids = assembler
                            .working
                            .evidence_links
                            .values()
                            .filter(|link| {
                                link.hypothesis_id == request.hypothesis_id
                                    && link.relation == EvidenceRelation::Contradicts
                                    && link.strength >= EvidenceStrength::Strong
                            })
                            .map(|link| link.id.clone())
                            .collect();
                        let resolution = Resolution {
                            id: resolution_id.clone(),
                            case_id: assembler.working.case_id.clone(),
                            hypothesis_id: request.hypothesis_id.clone(),
                            status: request.requested_status.clone(),
                            evidence_link_ids: request.evidence_link_ids.clone(),
                            unresolved_contradiction_ids,
                            validator_summary,
                            requested_by: ActorId::new(actor_session_id),
                            verified_by,
                            created_at: Utc::now(),
                        };
                        assembler.push(
                            AggregateKind::Hypothesis,
                            request.hypothesis_id.to_string(),
                            request.expected_revision + 1,
                            LedgerEvent::HypothesisResolvedV2 {
                                schema_version: LEDGER_EVENT_SCHEMA_VERSION,
                                expected_revision: request.expected_revision,
                                resolution,
                            },
                        )?;
                        summaries.push(format!(
                            "Resolved {} as {:?} ({resolution_id})",
                            request.hypothesis_id, request.requested_status
                        ));
                    }
                    ResolutionValidation::Rejected { gaps } => {
                        assembler.push(
                            AggregateKind::Hypothesis,
                            request.hypothesis_id.to_string(),
                            request.expected_revision,
                            LedgerEvent::ResolutionRejectedV2 {
                                schema_version: LEDGER_EVENT_SCHEMA_VERSION,
                                request_id,
                                hypothesis_id: request.hypothesis_id.clone(),
                                expected_revision: request.expected_revision,
                                gaps: gaps.clone(),
                            },
                        )?;
                        summaries.push(format!("Resolution rejected: {}", gaps.join("; ")));
                    }
                }
            }
        }
    }

    if assembler.events.is_empty() {
        return Ok(None);
    }
    Ok(Some(LedgerCommitPlan {
        expected_version,
        events: assembler.events,
        bindings,
        summaries,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use holmes_core::ledger::{EvidenceStrength, Priority, ResolvedStatus, ValidatorKind};

    fn resolvable_high_priority_snapshot() -> (LedgerSnapshot, HypothesisId, EvidenceLinkId) {
        let case_id = holmes_core::ledger::CaseId::new("case-1");
        let hypothesis_id = HypothesisId::new("hyp-1");
        let link_id = EvidenceLinkId::new("el-1");
        let now = Utc::now();
        let mut snapshot = LedgerSnapshot::empty(case_id.clone());
        snapshot.hypotheses.insert(
            hypothesis_id.clone(),
            Hypothesis {
                id: hypothesis_id.clone(),
                case_id: case_id.clone(),
                claim: "the target is vulnerable".into(),
                premise_refs: Vec::new(),
                alternative_group: None,
                priority: Priority::High,
                status: HypothesisStatus::Open,
                revision: 1,
                created_by: ActorId::new("agent"),
                created_at: now,
                updated_at: now,
            },
        );
        snapshot.evidence_links.insert(
            link_id.clone(),
            EvidenceLink {
                id: link_id.clone(),
                case_id,
                evidence_id: "ev-1".into(),
                hypothesis_id: hypothesis_id.clone(),
                prediction_id: None,
                relation: EvidenceRelation::Supports,
                strength: EvidenceStrength::Strong,
                rationale: "deterministic reproduction".into(),
                validator: ValidatorKind::SecurityReproduction,
                actor: ActorId::new("runtime"),
                created_at: now,
            },
        );
        (snapshot, hypothesis_id, link_id)
    }

    fn resolution_meta(hypothesis_id: HypothesisId, link_id: EvidenceLinkId) -> MetaAction {
        MetaAction::RequestResolution(crate::decision::ResolutionRequestProposal {
            hypothesis_id,
            expected_revision: 1,
            requested_status: ResolvedStatus::Confirmed,
            evidence_link_ids: vec![link_id],
            reason: "reproduced".into(),
        })
    }

    #[test]
    fn high_priority_resolution_without_independent_verdict_is_rejected() {
        let (snapshot, hypothesis_id, link_id) = resolvable_high_priority_snapshot();
        let plan = assemble_meta_commit(
            &snapshot,
            &[resolution_meta(hypothesis_id, link_id)],
            &[],
            "agent",
            &BTreeMap::new(),
        )
        .unwrap()
        .unwrap();
        assert!(plan
            .events
            .iter()
            .any(|event| matches!(event.event, LedgerEvent::ResolutionRejectedV2 { .. })));
        assert!(!plan
            .events
            .iter()
            .any(|event| matches!(event.event, LedgerEvent::HypothesisResolvedV2 { .. })));
    }

    #[test]
    fn bound_independent_verdict_can_authorize_high_priority_resolution() {
        let (snapshot, hypothesis_id, link_id) = resolvable_high_priority_snapshot();
        let mut reviews = BTreeMap::new();
        reviews.insert(
            hypothesis_id.clone(),
            IndependentResolutionReview::Approved {
                verified_by: ActorId::new("independent:test"),
                summary: "semantic relation confirmed independently".into(),
            },
        );
        let plan = assemble_meta_commit(
            &snapshot,
            &[resolution_meta(hypothesis_id, link_id)],
            &[],
            "agent",
            &reviews,
        )
        .unwrap()
        .unwrap();
        let resolution = plan.events.iter().find_map(|event| match &event.event {
            LedgerEvent::HypothesisResolvedV2 { resolution, .. } => Some(resolution),
            _ => None,
        });
        assert_eq!(
            resolution.map(|value| value.verified_by.as_str()),
            Some("independent:test")
        );
    }
}
