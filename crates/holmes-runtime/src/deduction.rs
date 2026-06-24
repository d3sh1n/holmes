use holmes_core::event::Event;
use holmes_core::state::AttackHypothesis;
use holmes_core::{truncate_str, ToolResult};
use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize};

mod projection;
mod reducer;
mod validator;

pub use projection::DeductionLedgerProjection;

use crate::context::RuntimeContext;
use crate::deliberation::RuntimeError;

use reducer::DeductionReducer;
use validator::TraceValidator;

const MAX_EVIDENCE_SUMMARY_BYTES: usize = 360;

#[derive(Debug, Clone, Default)]
pub struct DeductionEngine;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeductionProjection {
    pub recorded: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DeductionTrace {
    #[serde(default, deserialize_with = "deserialize_evidence_inputs")]
    pub evidence: Vec<DeductionEvidenceInput>,
    #[serde(default, deserialize_with = "deserialize_hypothesis_inputs")]
    pub hypotheses: Vec<DeductionHypothesisInput>,
    #[serde(default)]
    pub predictions: Vec<DeductionPredictionInput>,
    #[serde(default)]
    pub experiments: Vec<DeductionExperimentInput>,
    #[serde(default, deserialize_with = "deserialize_relation_inputs")]
    pub supports: Vec<DeductionRelationInput>,
    #[serde(default, deserialize_with = "deserialize_relation_inputs")]
    pub contradictions: Vec<DeductionRelationInput>,
    #[serde(default)]
    pub rejections: Vec<DeductionRejectionInput>,
    #[serde(default)]
    pub confirmations: Vec<DeductionConfirmationInput>,
    #[serde(default, deserialize_with = "deserialize_conclusion_inputs")]
    pub conclusions: Vec<DeductionConclusionInput>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeductionEvidenceInput {
    #[serde(default)]
    pub evidence_id: Option<String>,
    pub summary: String,
    #[serde(default = "default_trace_source")]
    pub source: String,
    #[serde(default = "default_trace_confidence")]
    pub confidence: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeductionHypothesisInput {
    pub hypothesis_id: String,
    pub statement: String,
    #[serde(default)]
    pub rationale: String,
    #[serde(default)]
    pub confidence: Option<f32>,
    #[serde(default)]
    pub attack_type: Option<String>,
    #[serde(default)]
    pub entry_points: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeductionPredictionInput {
    pub hypothesis_id: String,
    pub prediction: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeductionExperimentInput {
    pub hypothesis_id: String,
    pub action: String,
    #[serde(default)]
    pub distinguishes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeductionRelationInput {
    pub hypothesis_id: String,
    pub evidence_id: String,
    pub rationale: String,
    #[serde(default)]
    pub confidence: Option<f32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeductionRejectionInput {
    pub hypothesis_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeductionConfirmationInput {
    pub hypothesis_id: String,
    pub conclusion: String,
    #[serde(default)]
    pub confidence: Option<f32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeductionConclusionInput {
    pub conclusion: String,
    #[serde(default)]
    pub supporting_hypotheses: Vec<String>,
    #[serde(default)]
    pub evidence_ids: Vec<String>,
}

fn default_trace_source() -> String {
    "llm_deduction".into()
}

fn default_trace_confidence() -> String {
    "reasoned".into()
}

fn deserialize_evidence_inputs<'de, D>(
    deserializer: D,
) -> Result<Vec<DeductionEvidenceInput>, D::Error>
where
    D: Deserializer<'de>,
{
    let values = deserialize_json_value_list(deserializer)?;
    values
        .into_iter()
        .map(evidence_input_from_value)
        .collect::<Result<Vec<_>, _>>()
        .map_err(D::Error::custom)
}

fn deserialize_hypothesis_inputs<'de, D>(
    deserializer: D,
) -> Result<Vec<DeductionHypothesisInput>, D::Error>
where
    D: Deserializer<'de>,
{
    let values = deserialize_json_value_list(deserializer)?;
    values
        .into_iter()
        .map(hypothesis_input_from_value)
        .collect::<Result<Vec<_>, _>>()
        .map_err(D::Error::custom)
}

fn deserialize_relation_inputs<'de, D>(
    deserializer: D,
) -> Result<Vec<DeductionRelationInput>, D::Error>
where
    D: Deserializer<'de>,
{
    let values = deserialize_json_value_list(deserializer)?;
    values
        .into_iter()
        .map(relation_input_from_value)
        .collect::<Result<Vec<_>, _>>()
        .map_err(D::Error::custom)
}

fn deserialize_conclusion_inputs<'de, D>(
    deserializer: D,
) -> Result<Vec<DeductionConclusionInput>, D::Error>
where
    D: Deserializer<'de>,
{
    let values = deserialize_json_value_list(deserializer)?;
    values
        .into_iter()
        .map(conclusion_input_from_value)
        .collect::<Result<Vec<_>, _>>()
        .map_err(D::Error::custom)
}

fn deserialize_json_value_list<'de, D>(deserializer: D) -> Result<Vec<serde_json::Value>, D::Error>
where
    D: Deserializer<'de>,
{
    match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::Null => Ok(Vec::new()),
        serde_json::Value::Array(values) => Ok(values),
        value => Ok(vec![value]),
    }
}

fn evidence_input_from_value(
    value: serde_json::Value,
) -> Result<DeductionEvidenceInput, serde_json::Error> {
    if let Some(raw) = value.as_str() {
        let (id, summary) = split_labeled_statement(raw);
        return Ok(DeductionEvidenceInput {
            evidence_id: id.or_else(|| Some(evidence_id("llm_deduction", &summary))),
            summary,
            source: default_trace_source(),
            confidence: default_trace_confidence(),
        });
    }

    serde_json::from_value::<DeductionEvidenceInput>(value.clone()).or_else(|_| {
        let loose: LooseEvidenceInput = serde_json::from_value(value)?;
        let summary = loose
            .summary
            .or(loose.text)
            .or(loose.content)
            .unwrap_or_default();
        Ok(DeductionEvidenceInput {
            evidence_id: loose.evidence_id.or(loose.id),
            summary,
            source: loose.source.unwrap_or_else(default_trace_source),
            confidence: loose
                .confidence
                .map(stringify_json_value)
                .unwrap_or_else(default_trace_confidence),
        })
    })
}

fn hypothesis_input_from_value(
    value: serde_json::Value,
) -> Result<DeductionHypothesisInput, serde_json::Error> {
    if let Some(raw) = value.as_str() {
        let (id, statement) = split_labeled_statement(raw);
        let hypothesis_id = id.unwrap_or_else(|| format!("hypothesis-{:016x}", stable_hash(raw)));
        return Ok(DeductionHypothesisInput {
            hypothesis_id,
            statement,
            rationale: String::new(),
            confidence: None,
            attack_type: None,
            entry_points: Vec::new(),
        });
    }

    serde_json::from_value::<DeductionHypothesisInput>(value.clone()).or_else(|_| {
        let loose: LooseHypothesisInput = serde_json::from_value(value)?;
        let statement = loose
            .statement
            .or(loose.summary)
            .or(loose.text)
            .unwrap_or_default();
        let hypothesis_id = loose
            .hypothesis_id
            .or(loose.id)
            .unwrap_or_else(|| format!("hypothesis-{:016x}", stable_hash(&statement)));
        Ok(DeductionHypothesisInput {
            hypothesis_id,
            statement,
            rationale: loose.rationale.unwrap_or_default(),
            confidence: loose.confidence,
            attack_type: loose.attack_type,
            entry_points: loose.entry_points,
        })
    })
}

fn relation_input_from_value(
    value: serde_json::Value,
) -> Result<DeductionRelationInput, serde_json::Error> {
    if let Some(raw) = value.as_str() {
        return Ok(relation_input_from_string(raw));
    }

    serde_json::from_value::<DeductionRelationInput>(value.clone()).or_else(|_| {
        let loose: LooseRelationInput = serde_json::from_value(value)?;
        Ok(DeductionRelationInput {
            hypothesis_id: loose.hypothesis_id.or(loose.target).unwrap_or_default(),
            evidence_id: loose.evidence_id.or(loose.source).unwrap_or_default(),
            rationale: loose
                .rationale
                .or(loose.relation)
                .or(loose.reason)
                .unwrap_or_default(),
            confidence: loose.confidence,
        })
    })
}

fn conclusion_input_from_value(
    value: serde_json::Value,
) -> Result<DeductionConclusionInput, serde_json::Error> {
    if let Some(raw) = value.as_str() {
        return Ok(DeductionConclusionInput {
            conclusion: raw.trim().to_string(),
            supporting_hypotheses: Vec::new(),
            evidence_ids: Vec::new(),
        });
    }

    serde_json::from_value::<DeductionConclusionInput>(value.clone()).or_else(|_| {
        let loose: LooseConclusionInput = serde_json::from_value(value)?;
        Ok(DeductionConclusionInput {
            conclusion: loose
                .conclusion
                .or(loose.summary)
                .or(loose.text)
                .unwrap_or_default(),
            supporting_hypotheses: loose.supporting_hypotheses,
            evidence_ids: loose.evidence_ids,
        })
    })
}

#[derive(Debug, Deserialize)]
struct LooseEvidenceInput {
    #[serde(default)]
    evidence_id: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    confidence: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct LooseHypothesisInput {
    #[serde(default)]
    hypothesis_id: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    statement: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    rationale: Option<String>,
    #[serde(default)]
    confidence: Option<f32>,
    #[serde(default)]
    attack_type: Option<String>,
    #[serde(default)]
    entry_points: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct LooseRelationInput {
    #[serde(default)]
    hypothesis_id: Option<String>,
    #[serde(default)]
    evidence_id: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    relation: Option<String>,
    #[serde(default)]
    rationale: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    confidence: Option<f32>,
}

#[derive(Debug, Deserialize)]
struct LooseConclusionInput {
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    supporting_hypotheses: Vec<String>,
    #[serde(default)]
    evidence_ids: Vec<String>,
}

fn split_labeled_statement(raw: &str) -> (Option<String>, String) {
    let trimmed = raw.trim();
    let Some((label, statement)) = split_once_label_separator(trimmed) else {
        return (None, trimmed.to_string());
    };

    let label = label.trim();
    let statement = statement.trim();
    if label.is_empty() || statement.is_empty() {
        return (None, trimmed.to_string());
    }

    (Some(label.to_string()), statement.to_string())
}

fn split_once_label_separator(raw: &str) -> Option<(&str, &str)> {
    raw.split_once(':').or_else(|| raw.split_once('：'))
}

fn relation_input_from_string(raw: &str) -> DeductionRelationInput {
    let (statement, rationale) = split_once_label_separator(raw)
        .map(|(statement, rationale)| (statement.trim(), rationale.trim()))
        .unwrap_or((raw.trim(), raw.trim()));

    let lower = statement.to_lowercase();
    let connectors = [
        " supports ",
        " support ",
        " 支持 ",
        " => ",
        " -> ",
        " contradicts ",
        " contradict ",
        " 反驳 ",
        " 矛盾 ",
    ];
    let endpoints = connectors.iter().find_map(|connector| {
        lower.find(connector).map(|index| {
            (
                statement[..index].trim().to_string(),
                statement[index + connector.len()..].trim().to_string(),
            )
        })
    });
    let (evidence_id, hypothesis_id) = endpoints.unwrap_or_else(|| {
        (
            extract_token_with_prefix(statement, "evidence-").unwrap_or_default(),
            extract_token_with_prefix(statement, "hypothesis-").unwrap_or_default(),
        )
    });

    DeductionRelationInput {
        hypothesis_id,
        evidence_id,
        rationale: rationale.to_string(),
        confidence: None,
    }
}

fn extract_token_with_prefix(raw: &str, prefix: &str) -> Option<String> {
    let start = raw.find(prefix)?;
    let token = raw[start..]
        .split(|ch: char| ch.is_whitespace() || matches!(ch, ',' | ';' | '，' | '；' | ':' | '：'))
        .next()?;
    Some(
        token
            .trim_matches(|ch| matches!(ch, '"' | '\'' | '`'))
            .to_string(),
    )
}

fn stringify_json_value(value: serde_json::Value) -> String {
    match value {
        serde_json::Value::String(value) => value,
        serde_json::Value::Number(value) => value.to_string(),
        serde_json::Value::Bool(value) => value.to_string(),
        serde_json::Value::Null => default_trace_confidence(),
        other => other.to_string(),
    }
}

impl DeductionEngine {
    pub fn new() -> Self {
        Self
    }

    pub async fn review_tool_results(
        &self,
        context: &mut RuntimeContext,
        results: &[ToolResult],
        observations: &[String],
    ) -> Result<DeductionProjection, RuntimeError> {
        let mut projection = DeductionProjection::default();

        for result in results {
            if result.is_error {
                continue;
            }

            let content = result.text_content();
            let content = content.trim();
            if content.is_empty() {
                continue;
            }

            let evidence_id = evidence_id(&result.tool_name, content);
            if record_unique_deduction_event(
                context,
                Event::EvidenceObserved {
                    evidence_id: evidence_id.clone(),
                    summary: format!(
                        "Tool {} returned: {}",
                        result.tool_name,
                        truncate_str(content, MAX_EVIDENCE_SUMMARY_BYTES)
                    ),
                    source: result.tool_name.clone(),
                    confidence: "tool_observed".into(),
                },
            )
            .await?
            {
                projection.recorded += 1;
            }

            let fact_id = fact_id(&result.tool_name, content);
            if record_unique_deduction_event(
                context,
                Event::FactRecorded {
                    fact_id,
                    statement: fact_statement(result, observations),
                    evidence_ids: vec![evidence_id.clone()],
                },
            )
            .await?
            {
                projection.recorded += 1;
            }

            projection.recorded += self
                .review_user_enumeration_signal(context, content, &evidence_id)
                .await?;
        }

        Ok(projection)
    }

    pub async fn apply_trace(
        &self,
        context: &mut RuntimeContext,
        trace: DeductionTrace,
    ) -> Result<DeductionProjection, RuntimeError> {
        let trace = TraceValidator::validate(context, trace).map_err(|error| {
            RuntimeError::recoverable(format!("invalid deduction trace: {error}"))
        })?;
        let mut projection = DeductionProjection::default();

        for evidence in trace.evidence {
            let evidence_id = evidence
                .evidence_id
                .unwrap_or_else(|| evidence_id(&evidence.source, &evidence.summary));
            if record_unique_deduction_event(
                context,
                Event::EvidenceObserved {
                    evidence_id,
                    summary: evidence.summary,
                    source: evidence.source,
                    confidence: evidence.confidence,
                },
            )
            .await?
            {
                projection.recorded += 1;
            }
        }

        for hypothesis in trace.hypotheses {
            let active = hypothesis
                .attack_type
                .as_ref()
                .map(|attack_type| AttackHypothesis {
                    attack_type: attack_type.clone(),
                    confidence: hypothesis.confidence.unwrap_or(0.5),
                    reasoning: hypothesis.rationale.clone(),
                    entry_points: hypothesis.entry_points.clone(),
                });
            projection.recorded += self
                .ensure_hypothesis(
                    context,
                    &hypothesis.hypothesis_id,
                    &hypothesis.statement,
                    &hypothesis.rationale,
                    hypothesis.confidence.unwrap_or(0.5),
                    active,
                )
                .await?;
        }

        for prediction in trace.predictions {
            projection.recorded += self
                .ensure_prediction(context, &prediction.hypothesis_id, &prediction.prediction)
                .await?;
        }

        for experiment in trace.experiments {
            projection.recorded += self
                .ensure_experiment(
                    context,
                    &experiment.hypothesis_id,
                    &experiment.action,
                    experiment.distinguishes,
                )
                .await?;
        }

        for relation in trace.supports {
            projection.recorded += self
                .support_hypothesis(
                    context,
                    &relation.hypothesis_id,
                    &relation.evidence_id,
                    &relation.rationale,
                    relation.confidence.unwrap_or(0.72),
                )
                .await?;
        }

        for relation in trace.contradictions {
            projection.recorded += self
                .contradict_hypothesis(
                    context,
                    &relation.hypothesis_id,
                    &relation.evidence_id,
                    &relation.rationale,
                    relation.confidence.unwrap_or(0.2),
                )
                .await?;
        }

        for rejection in trace.rejections {
            projection.recorded += self
                .reject_hypothesis(context, &rejection.hypothesis_id, &rejection.reason)
                .await?;
        }

        for confirmation in trace.confirmations {
            projection.recorded += self
                .confirm_hypothesis(
                    context,
                    &confirmation.hypothesis_id,
                    &confirmation.conclusion,
                    confirmation.confidence,
                )
                .await?;
        }

        for conclusion in trace.conclusions {
            projection.recorded += self
                .draw_conclusion(
                    context,
                    &conclusion.conclusion,
                    conclusion.supporting_hypotheses,
                    conclusion.evidence_ids,
                )
                .await?;
        }

        Ok(projection)
    }

    async fn review_user_enumeration_signal(
        &self,
        context: &mut RuntimeContext,
        content: &str,
        evidence_id: &str,
    ) -> Result<usize, RuntimeError> {
        let Some(signal) = user_enumeration_signal(content) else {
            return Ok(0);
        };

        let enumeration_id = "hypothesis-user-enumeration";
        let generic_failure_id = "hypothesis-generic-login-failure";
        let mut recorded = 0;

        recorded += self
            .ensure_hypothesis(
                context,
                enumeration_id,
                "The login endpoint may leak user existence through response differences.",
                "Valid and invalid user probes are being compared for distinguishable signals.",
                0.45,
                Some(AttackHypothesis {
                    attack_type: "user_enumeration".into(),
                    confidence: 0.45,
                    reasoning:
                        "Valid and invalid user probes are being compared for distinguishable response signals."
                            .into(),
                    entry_points: vec!["login".into()],
                }),
            )
            .await?;
        recorded += self
            .ensure_hypothesis(
                context,
                generic_failure_id,
                "The login endpoint may return generic failure responses that do not reveal user existence.",
                "A safe alternative explanation is that all failed logins share the same response shape.",
                0.35,
                None,
            )
            .await?;

        recorded += self
            .ensure_prediction(
                context,
                enumeration_id,
                "If user enumeration is present, valid and invalid usernames should produce distinguishable status, body, or timing signals.",
            )
            .await?;
        recorded += self
            .ensure_prediction(
                context,
                generic_failure_id,
                "If login failures are generic, valid and invalid usernames should produce equivalent response signals.",
            )
            .await?;
        recorded += self
            .ensure_experiment(
                context,
                enumeration_id,
                "Compare controlled valid-user and invalid-user login probes under the same request shape.",
                vec!["user_enumeration".into(), "generic_login_failure".into()],
            )
            .await?;

        match signal {
            UserEnumerationSignal::SupportsEnumeration => {
                recorded += self
                    .support_hypothesis(
                        context,
                        enumeration_id,
                        evidence_id,
                        "The evidence contains a response-difference signal between valid and invalid usernames.",
                        0.78,
                    )
                    .await?;
                recorded += self
                    .contradict_hypothesis(
                        context,
                        generic_failure_id,
                        evidence_id,
                        "Different valid/invalid-user responses contradict the generic-login-failure explanation.",
                        0.2,
                    )
                    .await?;

                if content.to_ascii_lowercase().contains("confirmed=true") {
                    recorded += self
                        .reject_hypothesis(
                            context,
                            generic_failure_id,
                            "Controlled response differences rule out the generic-login-failure alternative.",
                        )
                        .await?;
                    recorded += self
                        .confirm_hypothesis(
                            context,
                            enumeration_id,
                            "Controlled probes support user enumeration through login response differences.",
                            None,
                        )
                        .await?;
                    recorded += self
                        .draw_conclusion(
                            context,
                            "Login response differences support a user-enumeration finding.",
                            vec![enumeration_id.into()],
                            vec![evidence_id.to_string()],
                        )
                        .await?;
                }
            }
            UserEnumerationSignal::ContradictsEnumeration => {
                recorded += self
                    .support_hypothesis(
                        context,
                        generic_failure_id,
                        evidence_id,
                        "Equivalent valid/invalid-user responses support the generic-login-failure explanation.",
                        0.72,
                    )
                    .await?;
                recorded += self
                    .contradict_hypothesis(
                        context,
                        enumeration_id,
                        evidence_id,
                        "Equivalent valid/invalid-user responses contradict user enumeration.",
                        0.18,
                    )
                    .await?;

                if content.to_ascii_lowercase().contains("confirmed=true") {
                    recorded += self
                        .reject_hypothesis(
                            context,
                            enumeration_id,
                            "Controlled equivalent responses rule out user enumeration for this probe.",
                        )
                        .await?;
                    recorded += self
                        .confirm_hypothesis(
                            context,
                            generic_failure_id,
                            "Controlled probes support generic login failure responses.",
                            None,
                        )
                        .await?;
                    recorded += self
                        .draw_conclusion(
                            context,
                            "Current controlled login probes do not support user enumeration.",
                            vec![generic_failure_id.into()],
                            vec![evidence_id.to_string()],
                        )
                        .await?;
                }
            }
        }

        Ok(recorded)
    }

    async fn ensure_hypothesis(
        &self,
        context: &mut RuntimeContext,
        hypothesis_id: &str,
        statement: &str,
        rationale: &str,
        confidence: f32,
        active_attack_hypothesis: Option<AttackHypothesis>,
    ) -> Result<usize, RuntimeError> {
        let attack_type = active_attack_hypothesis
            .as_ref()
            .map(|hypothesis| hypothesis.attack_type.clone());
        let entry_points = active_attack_hypothesis
            .as_ref()
            .map(|hypothesis| hypothesis.entry_points.clone())
            .unwrap_or_default();

        let recorded = record_unique_deduction_event(
            context,
            Event::HypothesisProposed {
                hypothesis_id: hypothesis_id.into(),
                statement: statement.into(),
                rationale: rationale.into(),
                confidence: Some(confidence),
                attack_type,
                entry_points,
            },
        )
        .await?;

        Ok(usize::from(recorded))
    }

    async fn ensure_prediction(
        &self,
        context: &mut RuntimeContext,
        hypothesis_id: &str,
        prediction: &str,
    ) -> Result<usize, RuntimeError> {
        let recorded = record_unique_deduction_event(
            context,
            Event::PredictionMade {
                hypothesis_id: hypothesis_id.into(),
                prediction: prediction.into(),
            },
        )
        .await?;
        Ok(usize::from(recorded))
    }

    async fn ensure_experiment(
        &self,
        context: &mut RuntimeContext,
        hypothesis_id: &str,
        action: &str,
        distinguishes: Vec<String>,
    ) -> Result<usize, RuntimeError> {
        let recorded = record_unique_deduction_event(
            context,
            Event::ExperimentPlanned {
                hypothesis_id: hypothesis_id.into(),
                action: action.into(),
                distinguishes,
            },
        )
        .await?;
        Ok(usize::from(recorded))
    }

    async fn support_hypothesis(
        &self,
        context: &mut RuntimeContext,
        hypothesis_id: &str,
        evidence_id: &str,
        rationale: &str,
        confidence: f32,
    ) -> Result<usize, RuntimeError> {
        let recorded = record_unique_deduction_event(
            context,
            Event::HypothesisSupported {
                hypothesis_id: hypothesis_id.into(),
                evidence_id: evidence_id.into(),
                rationale: rationale.into(),
                confidence: Some(confidence),
            },
        )
        .await?;
        Ok(usize::from(recorded))
    }

    async fn contradict_hypothesis(
        &self,
        context: &mut RuntimeContext,
        hypothesis_id: &str,
        evidence_id: &str,
        rationale: &str,
        confidence: f32,
    ) -> Result<usize, RuntimeError> {
        let recorded = record_unique_deduction_event(
            context,
            Event::HypothesisContradicted {
                hypothesis_id: hypothesis_id.into(),
                evidence_id: evidence_id.into(),
                rationale: rationale.into(),
                confidence: Some(confidence),
            },
        )
        .await?;
        Ok(usize::from(recorded))
    }

    async fn reject_hypothesis(
        &self,
        context: &mut RuntimeContext,
        hypothesis_id: &str,
        reason: &str,
    ) -> Result<usize, RuntimeError> {
        let recorded = record_unique_deduction_event(
            context,
            Event::HypothesisRejected {
                hypothesis_id: hypothesis_id.into(),
                reason: reason.into(),
            },
        )
        .await?;
        Ok(usize::from(recorded))
    }

    async fn confirm_hypothesis(
        &self,
        context: &mut RuntimeContext,
        hypothesis_id: &str,
        conclusion: &str,
        confidence: Option<f32>,
    ) -> Result<usize, RuntimeError> {
        let recorded = record_unique_deduction_event(
            context,
            Event::HypothesisConfirmed {
                hypothesis_id: hypothesis_id.into(),
                conclusion: conclusion.into(),
                confidence,
            },
        )
        .await?;
        Ok(usize::from(recorded))
    }

    async fn draw_conclusion(
        &self,
        context: &mut RuntimeContext,
        conclusion: &str,
        supporting_hypotheses: Vec<String>,
        evidence_ids: Vec<String>,
    ) -> Result<usize, RuntimeError> {
        let recorded = record_unique_deduction_event(
            context,
            Event::ConclusionDrawn {
                conclusion: conclusion.into(),
                supporting_hypotheses,
                evidence_ids,
            },
        )
        .await?;
        Ok(usize::from(recorded))
    }
}

async fn record_deduction_event(
    context: &mut RuntimeContext,
    event: Event,
) -> Result<(), RuntimeError> {
    context
        .session_db
        .append_event(&context.session_id, &event)
        .await
        .map_err(|error| {
            RuntimeError::recoverable(format!(
                "failed to persist deduction event for session {}: {}",
                context.session_id, error
            ))
        })?;
    DeductionReducer::apply(&mut context.state, &event);
    context.mind_palace.ingest(event);
    Ok(())
}

async fn record_unique_deduction_event(
    context: &mut RuntimeContext,
    event: Event,
) -> Result<bool, RuntimeError> {
    if DeductionReducer::already_applied(&context.state, &event) {
        return Ok(false);
    }
    record_deduction_event(context, event).await?;
    Ok(true)
}

fn fact_statement(result: &ToolResult, observations: &[String]) -> String {
    if observations.is_empty() {
        return format!("{} produced successful evidence output.", result.tool_name);
    }

    format!(
        "{} produced evidence consistent with: {}",
        result.tool_name,
        observations.join(" ")
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UserEnumerationSignal {
    SupportsEnumeration,
    ContradictsEnumeration,
}

fn user_enumeration_signal(content: &str) -> Option<UserEnumerationSignal> {
    let lower = content.to_ascii_lowercase();
    let compares_users = lower.contains("valid_user") && lower.contains("invalid_user")
        || lower.contains("valid username") && lower.contains("invalid username");

    if lower.contains("no_user_enumeration")
        || lower.contains("user_enumeration=false")
        || lower.contains("response_diff=false")
        || lower.contains("same_response=true")
        || lower.contains("uniform_response=true")
        || lower.contains("generic_failure=true")
        || (compares_users
            && (lower.contains("same response")
                || lower.contains("equivalent")
                || lower.contains("no difference")))
    {
        return Some(UserEnumerationSignal::ContradictsEnumeration);
    }

    if lower.contains("user_enumeration")
        || lower.contains("response_diff=true")
        || (compares_users && (lower.contains("diff") || lower.contains("different")))
    {
        return Some(UserEnumerationSignal::SupportsEnumeration);
    }

    None
}

pub(crate) fn evidence_id(source: &str, content: &str) -> String {
    format!(
        "evidence-{:016x}",
        stable_hash(&format!("{source}:{content}"))
    )
}

fn fact_id(source: &str, content: &str) -> String {
    format!("fact-{:016x}", stable_hash(&format!("{source}:{content}")))
}

fn stable_hash(content: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in content.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use holmes_core::config::HolmesConfig;
    use holmes_core::session::RuntimeSession;
    use holmes_core::{LlmResponse, SessionMode};
    use holmes_guards::GuardChain;
    use holmes_mind_palace::MindPalace;
    use holmes_session::{memory_store::MemoryStore, CreateSessionParams, SessionDB};
    use holmes_tools::ToolRegistry;

    use crate::context::{DeductionHypothesisStatus, RuntimeState};
    use crate::deliberation::StaticLlmBackend;

    use super::*;

    #[test]
    fn loose_deepseek_trace_deserializes_to_structured_trace() {
        let trace: DeductionTrace = serde_json::from_value(serde_json::json!({
            "evidence": "evidence-admin-403：/admin 返回 403 且包含 request-id req-live-1",
            "hypotheses": [
                "hypothesis-admin-authz: /admin 存在但需要授权"
            ],
            "supports": [
                "evidence-admin-403 支持 hypothesis-admin-authz：403 响应明确指示该路径存在但被权限层阻断"
            ],
            "conclusions": [
                "/admin 应视为受保护端点，等待授权安全比较阶段再行处理"
            ]
        }))
        .expect("loose trace should deserialize");

        assert_eq!(trace.evidence.len(), 1);
        assert_eq!(
            trace.evidence[0].evidence_id.as_deref(),
            Some("evidence-admin-403")
        );
        assert_eq!(
            trace.evidence[0].summary,
            "/admin 返回 403 且包含 request-id req-live-1"
        );
        assert_eq!(trace.hypotheses[0].hypothesis_id, "hypothesis-admin-authz");
        assert_eq!(trace.hypotheses[0].statement, "/admin 存在但需要授权");
        assert_eq!(trace.supports[0].evidence_id, "evidence-admin-403");
        assert_eq!(trace.supports[0].hypothesis_id, "hypothesis-admin-authz");
        assert!(trace.supports[0].rationale.contains("权限层阻断"));
        assert!(trace.conclusions[0].conclusion.contains("受保护端点"));
    }

    #[tokio::test]
    async fn user_enumeration_signal_records_deduction_trace() {
        let mut context = make_context().await;
        let result = ToolResult::success(
            "call-1",
            "login_probe",
            "valid_user_status=401 invalid_user_status=404 response_diff=true signal=user_enumeration confirmed=true",
        );

        let projection = DeductionEngine::new()
            .review_tool_results(&mut context, &[result], &[])
            .await
            .expect("review deduction");

        assert!(projection.recorded >= 8);
        assert_eq!(context.state.deduction.evidence.len(), 1);
        assert_eq!(context.state.deduction.facts.len(), 1);
        assert!(!context.state.deduction.predictions.is_empty());
        assert!(!context.state.deduction.experiments.is_empty());
        assert_eq!(context.state.deduction.conclusions.len(), 1);
        assert_eq!(context.state.active_hypotheses.len(), 1);
        let events = context
            .session_db
            .get_events(&context.session_id)
            .await
            .expect("events");
        assert!(events
            .iter()
            .any(|event| matches!(event.event, Event::EvidenceObserved { .. })));
        assert!(events
            .iter()
            .any(|event| matches!(event.event, Event::HypothesisProposed { .. })));
        assert!(events
            .iter()
            .any(|event| matches!(event.event, Event::PredictionMade { .. })));
        assert!(events
            .iter()
            .any(|event| matches!(event.event, Event::HypothesisSupported { .. })));
        assert!(events
            .iter()
            .any(|event| matches!(event.event, Event::ConclusionDrawn { .. })));
        assert!(context.state.deduction.hypotheses.iter().any(|hypothesis| {
            hypothesis.id == "hypothesis-user-enumeration"
                && hypothesis.status == DeductionHypothesisStatus::Confirmed
                && !hypothesis.supporting_evidence.is_empty()
        }));
        assert!(context.state.deduction.hypotheses.iter().any(|hypothesis| {
            hypothesis.id == "hypothesis-generic-login-failure"
                && hypothesis.status == DeductionHypothesisStatus::Rejected
                && !hypothesis.contradicting_evidence.is_empty()
        }));
    }

    #[tokio::test]
    async fn equivalent_login_responses_reject_user_enumeration() {
        let mut context = make_context().await;
        let result = ToolResult::success(
            "call-1",
            "login_probe",
            "valid_user_status=401 invalid_user_status=401 response_diff=false generic_failure=true confirmed=true",
        );

        let projection = DeductionEngine::new()
            .review_tool_results(&mut context, &[result], &[])
            .await
            .expect("review deduction");

        assert!(projection.recorded >= 8);
        assert_eq!(context.state.deduction.evidence.len(), 1);
        assert_eq!(context.state.deduction.conclusions.len(), 1);
        let events = context
            .session_db
            .get_events(&context.session_id)
            .await
            .expect("events");
        assert!(events.iter().any(|event| {
            matches!(
                event.event,
                Event::HypothesisContradicted {
                    ref hypothesis_id,
                    ..
                } if hypothesis_id == "hypothesis-user-enumeration"
            )
        }));
        assert!(events.iter().any(|event| {
            matches!(
                event.event,
                Event::HypothesisRejected {
                    ref hypothesis_id,
                    ..
                } if hypothesis_id == "hypothesis-user-enumeration"
            )
        }));
        assert!(events.iter().any(|event| {
            matches!(
                event.event,
                Event::HypothesisConfirmed {
                    ref hypothesis_id,
                    ..
                } if hypothesis_id == "hypothesis-generic-login-failure"
            )
        }));
        assert!(context.state.active_hypotheses.is_empty());
        assert!(context.state.deduction.hypotheses.iter().any(|hypothesis| {
            hypothesis.id == "hypothesis-user-enumeration"
                && hypothesis.status == DeductionHypothesisStatus::Rejected
                && !hypothesis.contradicting_evidence.is_empty()
        }));
        assert!(context.state.deduction.hypotheses.iter().any(|hypothesis| {
            hypothesis.id == "hypothesis-generic-login-failure"
                && hypothesis.status == DeductionHypothesisStatus::Confirmed
                && !hypothesis.supporting_evidence.is_empty()
        }));
    }

    #[tokio::test]
    async fn invalid_trace_does_not_partially_update_deduction_state() {
        let mut context = make_context().await;
        let mut trace = DeductionTrace::default();
        trace.hypotheses.push(DeductionHypothesisInput {
            hypothesis_id: "hypothesis-missing-evidence".into(),
            statement: "A hypothesis with an invalid support reference.".into(),
            rationale: "The support evidence is absent.".into(),
            confidence: Some(0.4),
            attack_type: None,
            entry_points: Vec::new(),
        });
        trace.supports.push(DeductionRelationInput {
            hypothesis_id: "hypothesis-missing-evidence".into(),
            evidence_id: "evidence-does-not-exist".into(),
            rationale: "This should not validate.".into(),
            confidence: Some(0.6),
        });

        let error = DeductionEngine::new()
            .apply_trace(&mut context, trace)
            .await
            .expect_err("invalid trace should fail");

        assert!(error.message.contains("unknown evidence"));
        assert!(context.state.deduction.hypotheses.is_empty());
        assert!(context.state.deduction.evidence.is_empty());
    }

    #[tokio::test]
    async fn trace_validation_redacts_secret_markers_before_persisting_events() {
        let mut context = make_context().await;
        let mut trace = DeductionTrace::default();
        trace.evidence.push(DeductionEvidenceInput {
            evidence_id: Some("evidence-secret".into()),
            summary: "Login response included password=hunter2 token=abc123".into(),
            source: "manual_note".into(),
            confidence: "Authorization: Bearer abc123".into(),
        });

        let projection = DeductionEngine::new()
            .apply_trace(&mut context, trace)
            .await
            .expect("valid trace");

        assert_eq!(projection.recorded, 1);
        let event = context
            .session_db
            .get_events(&context.session_id)
            .await
            .expect("events")
            .into_iter()
            .find_map(|event| match event.event {
                Event::EvidenceObserved {
                    summary,
                    confidence,
                    ..
                } => Some((summary, confidence)),
                _ => None,
            })
            .expect("evidence event");

        assert!(event.0.contains("password=<redacted>"));
        assert!(event.0.contains("token=<redacted>"));
        assert!(!event.0.contains("hunter2"));
        assert!(!event.0.contains("abc123"));
        assert_eq!(event.1, "Authorization:<redacted>");
    }

    async fn make_context() -> RuntimeContext {
        let session_id = "session-1".to_string();
        let session_db = Arc::new(SessionDB::open(":memory:").await.expect("session db"));
        session_db
            .create_session(CreateSessionParams {
                id: Some(session_id.clone()),
                title: None,
                mode: Some(SessionMode::Pentest),
                model: None,
                system_prompt: None,
                parent_session_id: None,
                fork_point: None,
                source: Some("test".into()),
                tags: Vec::new(),
            })
            .await
            .expect("create session");
        let memory_store = Arc::new(MemoryStore::open(":memory:").await.expect("memory store"));
        RuntimeContext::new(
            RuntimeSession::new(session_id, SessionMode::Pentest),
            session_db.clone(),
            memory_store.clone(),
            MindPalace::new(session_db, memory_store),
            Arc::new(StaticLlmBackend::new(LlmResponse {
                content: None,
                tool_calls: Vec::new(),
                finish_reason: None,
                usage: None,
            })),
            Arc::new(ToolRegistry::new()),
            GuardChain::new(),
            RuntimeState::new(SessionMode::Pentest),
            HolmesConfig::default(),
        )
    }
}
