use crate::context::DeductionState;

const MAX_EVIDENCE_ITEMS: usize = 6;
const MAX_FACT_ITEMS: usize = 6;
const MAX_HYPOTHESIS_ITEMS: usize = 6;
const MAX_EXPERIMENT_ITEMS: usize = 4;
const MAX_CONCLUSION_ITEMS: usize = 4;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeductionLedgerProjection {
    pub evidence: Vec<String>,
    pub facts: Vec<String>,
    pub hypotheses: Vec<String>,
    pub experiments: Vec<String>,
    pub conclusions: Vec<String>,
}

impl DeductionLedgerProjection {
    pub fn from_state(state: &DeductionState) -> Self {
        Self {
            evidence: recent(&state.evidence, MAX_EVIDENCE_ITEMS)
                .iter()
                .map(|evidence| {
                    format!(
                        "[{} {} via {}] {}",
                        evidence.id, evidence.confidence, evidence.source, evidence.summary
                    )
                })
                .collect(),
            facts: recent(&state.facts, MAX_FACT_ITEMS)
                .iter()
                .map(|fact| format!("[{}] {}", fact.id, fact.statement))
                .collect(),
            hypotheses: recent(&state.hypotheses, MAX_HYPOTHESIS_ITEMS)
                .iter()
                .map(|hypothesis| {
                    let attack_type = hypothesis.attack_type.as_deref().unwrap_or("general");
                    let entry_points = if hypothesis.entry_points.is_empty() {
                        "none".into()
                    } else {
                        hypothesis.entry_points.join(", ")
                    };
                    format!(
                        "[{} {:?} {} confidence {:.2}] {}\n  entry points: {}\n  supports: {}\n  contradicts: {}",
                        hypothesis.id,
                        hypothesis.status,
                        attack_type,
                        hypothesis.confidence,
                        hypothesis.statement,
                        entry_points,
                        format_refs(&hypothesis.supporting_evidence),
                        format_refs(&hypothesis.contradicting_evidence)
                    )
                })
                .collect(),
            experiments: recent(&state.experiments, MAX_EXPERIMENT_ITEMS)
                .iter()
                .map(|experiment| {
                    format!(
                        "[{}] {} (distinguishes: {})",
                        experiment.hypothesis_id,
                        experiment.action,
                        format_refs(&experiment.distinguishes)
                    )
                })
                .collect(),
            conclusions: recent(&state.conclusions, MAX_CONCLUSION_ITEMS)
                .iter()
                .map(|conclusion| {
                    format!(
                        "{} (hypotheses: {}; evidence: {})",
                        conclusion.conclusion,
                        format_refs(&conclusion.supporting_hypotheses),
                        format_refs(&conclusion.evidence_ids)
                    )
                })
                .collect(),
        }
    }
}

fn recent<T>(items: &[T], max: usize) -> &[T] {
    &items[items.len().saturating_sub(max)..]
}

fn format_refs(values: &[String]) -> String {
    if values.is_empty() {
        "none".into()
    } else {
        values.join(", ")
    }
}
