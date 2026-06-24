use std::collections::HashSet;
use std::fmt;
use std::mem;

use crate::context::RuntimeContext;

use super::{evidence_id, DeductionTrace};

pub(crate) struct TraceValidator;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeductionValidationError {
    errors: Vec<String>,
}

impl DeductionValidationError {
    fn new(errors: Vec<String>) -> Self {
        Self { errors }
    }
}

impl fmt::Display for DeductionValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.errors.join("; "))
    }
}

impl std::error::Error for DeductionValidationError {}

impl TraceValidator {
    pub(crate) fn validate(
        context: &RuntimeContext,
        mut trace: DeductionTrace,
    ) -> Result<DeductionTrace, DeductionValidationError> {
        let mut errors = Vec::new();
        let mut known_evidence: HashSet<String> =
            context.state.deduction.ledger.evidence_ids.clone();
        let mut known_hypotheses: HashSet<String> =
            context.state.deduction.ledger.hypothesis_ids.clone();

        for evidence in &mut trace.evidence {
            require_nonempty(&mut errors, "evidence.summary", &evidence.summary);
            require_nonempty(&mut errors, "evidence.source", &evidence.source);
            if let Some(id) = evidence.evidence_id.as_deref() {
                require_nonempty(&mut errors, "evidence.evidence_id", id);
                known_evidence.insert(id.to_string());
            } else {
                let id = evidence_id(&evidence.source, &evidence.summary);
                known_evidence.insert(id.clone());
                evidence.evidence_id = Some(id);
            }
        }

        for hypothesis in &trace.hypotheses {
            require_nonempty(
                &mut errors,
                "hypothesis.hypothesis_id",
                &hypothesis.hypothesis_id,
            );
            require_nonempty(&mut errors, "hypothesis.statement", &hypothesis.statement);
            validate_confidence(&mut errors, "hypothesis.confidence", hypothesis.confidence);
            known_hypotheses.insert(hypothesis.hypothesis_id.clone());
        }

        for prediction in &trace.predictions {
            require_known_hypothesis(
                &mut errors,
                &known_hypotheses,
                "prediction.hypothesis_id",
                &prediction.hypothesis_id,
            );
            require_nonempty(&mut errors, "prediction.prediction", &prediction.prediction);
        }

        for experiment in &trace.experiments {
            require_known_hypothesis(
                &mut errors,
                &known_hypotheses,
                "experiment.hypothesis_id",
                &experiment.hypothesis_id,
            );
            require_nonempty(&mut errors, "experiment.action", &experiment.action);
        }

        for relation in &trace.supports {
            validate_relation(
                &mut errors,
                &known_hypotheses,
                &known_evidence,
                "support",
                &relation.hypothesis_id,
                &relation.evidence_id,
                relation.confidence,
            );
        }

        for relation in &trace.contradictions {
            validate_relation(
                &mut errors,
                &known_hypotheses,
                &known_evidence,
                "contradiction",
                &relation.hypothesis_id,
                &relation.evidence_id,
                relation.confidence,
            );
        }

        for rejection in &trace.rejections {
            require_known_hypothesis(
                &mut errors,
                &known_hypotheses,
                "rejection.hypothesis_id",
                &rejection.hypothesis_id,
            );
            require_nonempty(&mut errors, "rejection.reason", &rejection.reason);
        }

        for confirmation in &trace.confirmations {
            require_known_hypothesis(
                &mut errors,
                &known_hypotheses,
                "confirmation.hypothesis_id",
                &confirmation.hypothesis_id,
            );
            require_nonempty(
                &mut errors,
                "confirmation.conclusion",
                &confirmation.conclusion,
            );
            validate_confidence(
                &mut errors,
                "confirmation.confidence",
                confirmation.confidence,
            );
        }

        for conclusion in &trace.conclusions {
            require_nonempty(&mut errors, "conclusion.conclusion", &conclusion.conclusion);
            for hypothesis_id in &conclusion.supporting_hypotheses {
                require_known_hypothesis(
                    &mut errors,
                    &known_hypotheses,
                    "conclusion.supporting_hypotheses",
                    hypothesis_id,
                );
            }
            for evidence_id in &conclusion.evidence_ids {
                require_known_evidence(
                    &mut errors,
                    &known_evidence,
                    "conclusion.evidence_ids",
                    evidence_id,
                );
            }
        }

        if errors.is_empty() {
            Ok(redact_trace(trace))
        } else {
            Err(DeductionValidationError::new(errors))
        }
    }
}

fn validate_relation(
    errors: &mut Vec<String>,
    known_hypotheses: &HashSet<String>,
    known_evidence: &HashSet<String>,
    prefix: &str,
    hypothesis_id: &str,
    evidence_id: &str,
    confidence: Option<f32>,
) {
    require_known_hypothesis(
        errors,
        known_hypotheses,
        &format!("{prefix}.hypothesis_id"),
        hypothesis_id,
    );
    require_known_evidence(
        errors,
        known_evidence,
        &format!("{prefix}.evidence_id"),
        evidence_id,
    );
    validate_confidence(errors, &format!("{prefix}.confidence"), confidence);
}

fn require_nonempty(errors: &mut Vec<String>, field: &str, value: &str) {
    if value.trim().is_empty() {
        errors.push(format!("{field} must not be empty"));
    }
}

fn require_known_hypothesis(
    errors: &mut Vec<String>,
    known_hypotheses: &HashSet<String>,
    field: &str,
    hypothesis_id: &str,
) {
    if !known_hypotheses.contains(hypothesis_id) {
        errors.push(format!(
            "{field} references unknown hypothesis {hypothesis_id:?}"
        ));
    }
}

fn require_known_evidence(
    errors: &mut Vec<String>,
    known_evidence: &HashSet<String>,
    field: &str,
    evidence_id: &str,
) {
    if !known_evidence.contains(evidence_id) {
        errors.push(format!(
            "{field} references unknown evidence {evidence_id:?}"
        ));
    }
}

fn validate_confidence(errors: &mut Vec<String>, field: &str, confidence: Option<f32>) {
    let Some(confidence) = confidence else {
        return;
    };
    if !(0.0..=1.0).contains(&confidence) {
        errors.push(format!("{field} must be between 0.0 and 1.0"));
    }
}

fn redact_trace(mut trace: DeductionTrace) -> DeductionTrace {
    for evidence in &mut trace.evidence {
        evidence.summary = redact_sensitive(&evidence.summary);
        evidence.source = redact_sensitive(&evidence.source);
        evidence.confidence = redact_sensitive(&evidence.confidence);
    }
    for hypothesis in &mut trace.hypotheses {
        hypothesis.statement = redact_sensitive(&hypothesis.statement);
        hypothesis.rationale = redact_sensitive(&hypothesis.rationale);
        hypothesis.attack_type = hypothesis
            .attack_type
            .take()
            .map(|attack_type| redact_sensitive(&attack_type));
        hypothesis.entry_points = mem::take(&mut hypothesis.entry_points)
            .into_iter()
            .map(|entry_point| redact_sensitive(&entry_point))
            .collect();
    }
    for prediction in &mut trace.predictions {
        prediction.prediction = redact_sensitive(&prediction.prediction);
    }
    for experiment in &mut trace.experiments {
        experiment.action = redact_sensitive(&experiment.action);
        experiment.distinguishes = mem::take(&mut experiment.distinguishes)
            .into_iter()
            .map(|value| redact_sensitive(&value))
            .collect();
    }
    for relation in trace
        .supports
        .iter_mut()
        .chain(trace.contradictions.iter_mut())
    {
        relation.rationale = redact_sensitive(&relation.rationale);
    }
    for rejection in &mut trace.rejections {
        rejection.reason = redact_sensitive(&rejection.reason);
    }
    for confirmation in &mut trace.confirmations {
        confirmation.conclusion = redact_sensitive(&confirmation.conclusion);
    }
    for conclusion in &mut trace.conclusions {
        conclusion.conclusion = redact_sensitive(&conclusion.conclusion);
    }
    trace
}

fn redact_sensitive(value: &str) -> String {
    let mut output = Vec::new();
    let mut skip_words = 0usize;

    for word in value.split_whitespace() {
        if skip_words > 0 {
            skip_words -= 1;
            continue;
        }

        let lower = word.to_ascii_lowercase();
        if lower == "bearer" {
            output.push("Bearer <redacted>".to_string());
            skip_words = 1;
        } else if lower.starts_with("authorization:") {
            output.push(redact_keyed_word(word, "authorization:"));
            if lower == "authorization:" {
                skip_words = 2;
            }
        } else if let Some(redacted) = redact_first_marker(
            word,
            &[
                "password=",
                "password:",
                "passwd=",
                "token=",
                "token:",
                "api_key=",
                "apikey=",
                "secret=",
                "secret:",
            ],
        ) {
            output.push(redacted);
        } else {
            output.push(word.to_string());
        }
    }

    output.join(" ")
}

fn redact_first_marker(word: &str, markers: &[&str]) -> Option<String> {
    let lower = word.to_ascii_lowercase();
    markers
        .iter()
        .find(|marker| lower.contains(**marker))
        .map(|marker| redact_keyed_word(word, marker))
}

fn redact_keyed_word(word: &str, marker: &str) -> String {
    let lower = word.to_ascii_lowercase();
    let Some(start) = lower.find(marker) else {
        return word.to_string();
    };
    let end = start + marker.len();
    format!("{}{}<redacted>", &word[..start], &word[start..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_obvious_secret_markers() {
        assert_eq!(
            redact_sensitive("password=hunter2 token:abc Authorization: Bearer xyz"),
            "password=<redacted> token:<redacted> Authorization:<redacted>"
        );
    }
}
