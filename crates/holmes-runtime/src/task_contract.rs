//! Deterministically derived task contract (P0-02).
//!
//! A `TaskContract` is built from the user's request by fixed heuristics — never by
//! asking the model whether the request "needs work". Once a contract exists, its
//! derived requirements cannot be dropped by the model: `set_goal` may refine the
//! standing goal (recorded separately in `TaskControlState`), but no model-facing
//! API mutates or clears contract requirements. Completion (via `finish` OR a plain
//! answer) is only verified when every derived requirement is met.

use crate::task_control::{EvidenceKind, EvidenceRecord};

/// Cap on the objective text retained from the user request.
const OBJECTIVE_MAX_CHARS: usize = 500;
/// Cap on extracted target tokens (hosts / IPs / URLs / paths).
const MAX_TARGETS: usize = 16;
/// Minimum target length for relevance matching — shorter tokens (e.g. "io")
/// produce too many false-positive substring hits.
const MIN_TARGET_MATCH_CHARS: usize = 4;

/// Action verbs (English + Chinese) that — combined with an extracted target —
/// classify a request as needing action/verification rather than a pure answer.
const ACTION_VERBS: &[&str] = &[
    "confirm",
    "verify",
    "test",
    "scan",
    "probe",
    "exploit",
    "enumerate",
    "inspect",
    "check",
    "fetch",
    "crawl",
    "audit",
    "attack",
    "reach",
    "collect",
    "dump",
    "brute",
    "pop a shell",
    "pentest",
    "验证",
    "确认",
    "测试",
    "扫描",
    "探测",
    "利用",
    "枚举",
    "检查",
    "抓取",
    "渗透",
    "攻击",
];

/// What a requirement demands before completion may be verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequirementKind {
    /// At least one successful, target-relevant action-tool evidence record.
    /// Bookkeeping tools (`write_todos`, progress/hypothesis reporters) and
    /// target-irrelevant calls never satisfy this — "the call succeeded" alone
    /// is not evidence.
    ActionEvidence,
    /// The objective itself: judged by the independent semantic verifier
    /// (`goal_evaluator` role) after all deterministic requirements pass.
    SemanticObjective,
}

/// Where a requirement came from. `Derived` requirements are established by the
/// runtime from the user request and cannot be waived by the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequirementSource {
    Derived,
    ModelDeclared,
}

/// One completion requirement within a task contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requirement {
    pub id: String,
    pub description: String,
    pub kind: RequirementKind,
    pub source: RequirementSource,
}

/// A deterministically derived contract for one user request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskContract {
    /// Stable id within a session (`contract-1`, `contract-2`, ... as new
    /// action-classified requests arrive).
    pub id: String,
    /// The user request this contract was derived from (bounded).
    pub objective: String,
    /// Target tokens extracted from the request (hosts / IPs / URLs / paths).
    /// Evidence must reference at least one of these to count as relevant;
    /// empty means relevance cannot be checked by token match.
    pub targets: Vec<String>,
    pub requirements: Vec<Requirement>,
    /// Set once the semantic verifier has confirmed the objective against the
    /// recorded evidence. Not persisted across a resume — the next completion
    /// re-verifies (fail-closed).
    pub objective_verified: bool,
}

impl TaskContract {
    /// Derive a contract from a user request. Returns `None` for pure
    /// chat/information requests — the deterministic exemption criterion for the
    /// completion gate: no extracted target AND no action verb means there is
    /// nothing to act on or verify, so a plain answer needs no evidence.
    ///
    /// `sequence` numbers the contract within the session (1-based).
    pub fn derive(input: &str, sequence: usize) -> Option<Self> {
        let lowered = input.to_lowercase();
        let targets = extract_targets(&lowered);
        let has_action_verb = ACTION_VERBS
            .iter()
            .any(|verb| contains_word(&lowered, verb));
        if targets.is_empty() || !has_action_verb {
            return None;
        }

        let objective: String = input.trim().chars().take(OBJECTIVE_MAX_CHARS).collect();
        let requirements = vec![
            Requirement {
                id: "req-1".into(),
                description: format!(
                    "perform at least one successful action referencing the task target ({})",
                    targets.join(", ")
                ),
                kind: RequirementKind::ActionEvidence,
                source: RequirementSource::Derived,
            },
            Requirement {
                id: "req-2".into(),
                description: format!("objective verified against recorded evidence: {objective}"),
                kind: RequirementKind::SemanticObjective,
                source: RequirementSource::Derived,
            },
        ];
        Some(Self {
            id: format!("contract-{sequence}"),
            objective,
            targets,
            requirements,
            objective_verified: false,
        })
    }

    /// Derived requirements not yet met by the recorded evidence. Only
    /// deterministic kinds are evaluated here; `SemanticObjective` is handled by
    /// the model-based verifier and reported through `objective_verified`.
    pub fn unmet_deterministic_requirements(
        &self,
        evidence: &[EvidenceRecord],
    ) -> Vec<&Requirement> {
        self.requirements
            .iter()
            .filter(|requirement| match requirement.kind {
                RequirementKind::ActionEvidence => !self.has_action_evidence(evidence),
                RequirementKind::SemanticObjective => false,
            })
            .collect()
    }

    /// Whether the recorded evidence contains at least one successful action-tool
    /// record that references a contract target.
    fn has_action_evidence(&self, evidence: &[EvidenceRecord]) -> bool {
        self.requirements
            .iter()
            .filter(|requirement| requirement.kind == RequirementKind::ActionEvidence)
            .all(|requirement| {
                evidence.iter().any(|record| {
                    self.is_eligible_action_evidence(record)
                        && record.requirement_ids.contains(&requirement.id)
                })
            })
    }

    /// Compute immutable requirement bindings at evidence-recording time. The model
    /// never supplies these ids, and evidence from a previous contract cannot match.
    pub fn matching_requirement_ids(&self, record: &EvidenceRecord) -> Vec<String> {
        if !self.is_eligible_action_evidence(record) {
            return Vec::new();
        }
        self.requirements
            .iter()
            .filter(|requirement| requirement.kind == RequirementKind::ActionEvidence)
            .map(|requirement| requirement.id.clone())
            .collect()
    }

    fn is_eligible_action_evidence(&self, record: &EvidenceRecord) -> bool {
        record.contract_id.as_deref() == Some(self.id.as_str())
            && record.outcome.is_success()
            && record.kind.is_action()
            && record.verified_by.is_deterministic()
            && self.references_target(record)
    }

    fn references_target(&self, record: &EvidenceRecord) -> bool {
        let haystack = format!(
            "{}\n{}",
            record.input_summary.to_lowercase(),
            record.output_snippet.to_lowercase()
        );
        self.targets
            .iter()
            .filter(|target| target.len() >= MIN_TARGET_MATCH_CHARS)
            .any(|target| haystack.contains(target))
    }
}

impl EvidenceKind {
    /// Whether this evidence kind represents a real action against the world
    /// (file/command/network/other tool execution) as opposed to bookkeeping or
    /// projected observations.
    pub fn is_action(&self) -> bool {
        matches!(
            self,
            Self::FileModification
                | Self::CommandExecution
                | Self::NetworkCapture
                | Self::OtherTool
        )
    }
}

/// Word-ish containment: for ASCII verbs require non-alphanumeric boundaries so
/// "attest" does not match "test"; CJK verbs match as plain substrings.
fn contains_word(haystack: &str, needle: &str) -> bool {
    if !needle.is_ascii() {
        return haystack.contains(needle);
    }
    let mut start = 0;
    while let Some(offset) = haystack[start..].find(needle) {
        let at = start + offset;
        let before_ok = haystack[..at]
            .chars()
            .next_back()
            .is_none_or(|ch| !ch.is_ascii_alphanumeric());
        let after_ok = haystack[at + needle.len()..]
            .chars()
            .next()
            .is_none_or(|ch| !ch.is_ascii_alphanumeric());
        if before_ok && after_ok {
            return true;
        }
        start = at + 1;
    }
    false
}

/// Extract target tokens (URLs, domains, IPv4s, absolute paths) from a request.
/// Input is expected lowercased. Extraction is heuristic by design — it only
/// feeds evidence relevance matching, never an allow/deny decision.
fn extract_targets(lowered: &str) -> Vec<String> {
    let mut targets: Vec<String> = Vec::new();
    for token in lowered.split(|ch: char| {
        !(ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | '/' | ':' | '~'))
    }) {
        // Trim stray punctuation from the edges, but keep a leading '/' so absolute
        // paths stay recognizable.
        let token = token
            .trim_end_matches(['.', '/', ':', '~'])
            .trim_start_matches(['.', ':']);
        if token.is_empty() {
            continue;
        }
        let is_url = token.starts_with("http://") || token.starts_with("https://");
        let is_ipv4 = token.split('.').count() == 4
            && token
                .split('.')
                .all(|octet| !octet.is_empty() && octet.bytes().all(|b| b.is_ascii_digit()));
        let is_domain = !is_ipv4
            && token.contains('.')
            && token
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | ':'))
            && token.split('.').any(|label| label.len() >= 2)
            && token.rsplit('.').next().is_some_and(|tld| {
                !tld.is_empty() && tld.chars().all(|ch| ch.is_ascii_alphabetic())
            });
        let is_path = token.starts_with('/') && token.len() > 1;
        if (is_url || is_ipv4 || is_domain || is_path)
            && !targets.iter().any(|existing| existing == token)
        {
            targets.push(token.to_string());
        }
        if targets.len() >= MAX_TARGETS {
            break;
        }
    }
    targets
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_control::{EvidenceKind, EvidenceRecord, VerificationMethod};

    fn record(tool: &str, kind: EvidenceKind, input: &str, output: &str) -> EvidenceRecord {
        EvidenceRecord {
            id: "ev-1".into(),
            tool: tool.into(),
            tool_call_id: None,
            contract_id: Some("contract-1".into()),
            turn_id: 1,
            requirement_ids: vec!["req-1".into()],
            outcome: holmes_core::ToolOutcomeStatus::Succeeded,
            kind,
            input_summary: input.into(),
            output_hash: String::new(),
            output_snippet: output.into(),
            predicate: String::new(),
            verified_by: VerificationMethod::Deterministic,
            recorded_at: None,
        }
    }

    #[test]
    fn action_request_with_target_derives_contract() {
        let contract = TaskContract::derive("Confirm example.test is reachable.", 1)
            .expect("action request derives a contract");
        assert_eq!(contract.id, "contract-1");
        assert_eq!(contract.targets, vec!["example.test"]);
        assert_eq!(contract.requirements.len(), 2);
        assert!(!contract.objective_verified);
    }

    #[test]
    fn chinese_action_request_derives_contract() {
        assert!(TaskContract::derive("验证 example.test 的登录接口是否可达", 1).is_some());
    }

    #[test]
    fn pure_chat_requests_are_exempt() {
        assert!(TaskContract::derive("hello", 1).is_none());
        assert!(TaskContract::derive("What is 2+2?", 1).is_none());
        // Action verb but no target: nothing concrete to verify against.
        assert!(TaskContract::derive("Test the login flow.", 1).is_none());
        // Target but no action verb: a statement, not a task.
        assert!(TaskContract::derive("We are authorized for staging.example only.", 1).is_none());
    }

    #[test]
    fn target_extraction_covers_urls_ips_and_paths() {
        let targets = extract_targets(
            "scan https://app.example.com/login and 10.0.0.8 then read /etc/passwd",
        );
        assert!(targets.contains(&"https://app.example.com/login".to_string()));
        assert!(targets.contains(&"10.0.0.8".to_string()));
        assert!(targets.contains(&"/etc/passwd".to_string()));
    }

    #[test]
    fn verb_matching_respects_word_boundaries() {
        assert!(contains_word("confirm example.test", "confirm"));
        assert!(!contains_word("attest to it", "test"));
        assert!(!contains_word("contest the result", "test"));
    }

    #[test]
    fn irrelevant_and_bookkeeping_evidence_does_not_satisfy_action_requirement() {
        let contract =
            TaskContract::derive("Confirm example.test is reachable.", 1).expect("contract");
        // Bookkeeping call (classified Observation-adjacent): not action evidence.
        let todos = EvidenceRecord {
            kind: EvidenceKind::Bookkeeping,
            ..record(
                "write_todos",
                EvidenceKind::Bookkeeping,
                "{}",
                "example.test",
            )
        };
        assert_eq!(contract.unmet_deterministic_requirements(&[todos]).len(), 1);
        // Real tool but unrelated to the target: still unmet.
        let unrelated = record(
            "read_file",
            EvidenceKind::OtherTool,
            r#"{"path":"/tmp/notes.txt"}"#,
            "unrelated content",
        );
        assert_eq!(
            contract
                .unmet_deterministic_requirements(&[unrelated])
                .len(),
            1
        );
    }

    #[test]
    fn target_relevant_action_evidence_satisfies_the_requirement() {
        let contract =
            TaskContract::derive("Confirm example.test is reachable.", 1).expect("contract");
        let probe = record(
            "echo_probe",
            EvidenceKind::OtherTool,
            r#"{"target":"example.test"}"#,
            "example.test is reachable",
        );
        assert!(contract
            .unmet_deterministic_requirements(&[probe])
            .is_empty());
    }

    #[test]
    fn unverified_semantic_objective_is_not_a_deterministic_gap() {
        let contract =
            TaskContract::derive("Confirm example.test is reachable.", 1).expect("contract");
        // With no evidence at all only the action requirement is unmet; the
        // semantic objective is deferred to the model-based verifier.
        let unmet = contract.unmet_deterministic_requirements(&[]);
        assert_eq!(unmet.len(), 1);
        assert_eq!(unmet[0].kind, RequirementKind::ActionEvidence);
    }
}
