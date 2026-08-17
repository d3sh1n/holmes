//! Subagent structured result protocol and isolation knobs (AGT-013/014).
//!
//! A subagent run must end in an [`AgentTaskResult`]: the final prose is only the
//! display field (`summary`); everything the parent relies on — findings, evidence,
//! validations, remaining work, resource usage, checkpoint — is structured and
//! derived deterministically from the subagent session's event log
//! ([`build_agent_task_result`]), never from the model's own claims.
//!
//! The parent never trusts a result blindly: [`verify_agent_task_result`] applies
//! deterministic checks (placeholder summaries, findings without evidence, failed
//! validations under a "completed" claim, dangling evidence references), and
//! [`wrap_for_parent`] attaches the verdict and downgrades a defective `Completed`
//! to `Partial` so the defects reach the parent conversation instead of a
//! confident-but-false summary.

use crate::event::Event;
use crate::execution_context::ExecutionContext;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Semaphore;

#[async_trait]
pub trait SubagentRunner: Send + Sync {
    /// Spawn and run a subagent to completion.
    /// `args` is a JSON string matching SubAgentTask schema.
    ///
    /// `ctx` is the parent turn's execution boundary (AGT-002): the runner must derive
    /// the subagent's own context from it (child cancellation token, capped turn
    /// deadline) so cancelling the parent propagates into the subagent's tool calls.
    ///
    /// Run-level failures (the subagent could not be started or its session could not
    /// be persisted) are `Err`; task-level outcomes — including failure, cancellation
    /// and budget exhaustion — are `Ok` with the matching [`AgentTaskStatus`], so the
    /// parent always gets a structured, verifiable result.
    async fn run_subagent(
        &self,
        args: &str,
        ctx: &ExecutionContext,
    ) -> Result<AgentTaskResult, String>;
}

/// Terminal status of a subagent task (AGT-013).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentTaskStatus {
    /// Finished; all recorded validations passed and no work remains.
    Completed,
    /// Stopped early (budget, iteration cap, unverified finish) — partial progress
    /// is preserved and `remaining_work` says what is left.
    Partial,
    /// The task itself failed.
    Failed,
    /// Cancelled (typically via parent-turn cancellation propagation).
    Cancelled,
}

/// A single finding reported by the subagent. `evidence_refs` must point at
/// `reference` values of the result's `evidence` list — the parent-side verifier
/// rejects dangling references.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Finding {
    pub summary: String,
    pub severity: Option<String>,
    pub evidence_refs: Vec<String>,
}

/// A verifiable pointer to evidence backing the result (event id, tool call,
/// file path or command output reference).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvidenceRef {
    /// e.g. "finding", "tool_call", "file".
    pub kind: String,
    pub reference: String,
    pub note: Option<String>,
}

/// Outcome of one deterministic or model-based validation inside the subagent run
/// (e.g. the completion verifier's goal evaluation).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ValidationResult {
    pub name: String,
    pub passed: bool,
    pub detail: Option<String>,
}

/// Resource consumption of the subagent run.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResourceUsage {
    pub tokens_used: u64,
    pub tool_calls: u64,
    pub turns: u64,
    pub wall_clock_ms: u64,
}

/// Structured result every subagent run must produce (AGT-013). `summary` is the
/// display-only field; the parent consumes the structured fields.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentTaskResult {
    pub task_id: String,
    pub status: AgentTaskStatus,
    pub summary: String,
    pub findings: Vec<Finding>,
    pub evidence: Vec<EvidenceRef>,
    pub changed_files: Vec<String>,
    pub validations: Vec<ValidationResult>,
    pub remaining_work: Vec<String>,
    pub usage: ResourceUsage,
    /// Opaque handle for resuming/inspecting the run — currently the subagent's
    /// session id, which doubles as the fork point for recovery.
    pub checkpoint: Option<String>,
}

/// Placeholder used when the subagent produced no final answer; the parent-side
/// verifier treats it as a defect under a completion claim.
pub const NO_ANSWER_PLACEHOLDER: &str = "No final answer produced by subagent.";

/// How a subagent run ended, fed into [`build_agent_task_result`].
#[derive(Debug, Clone)]
pub enum SubagentStop {
    /// The turn produced a final answer.
    Finished,
    /// The turn stopped early (iteration budget, stagnation, needs-user); the
    /// message is the runtime's resumable-stop text.
    Stopped(String),
    /// The turn errored.
    Failed(String),
    /// The turn was cancelled (parent propagation or operator).
    Cancelled,
}

/// Build the structured result from the subagent session's persisted events —
/// deterministic: findings come from `FindingRecorded`, validations from
/// `GoalEvaluated`, changed files from `write_file`/`edit_file` tool calls, and
/// usage from event counts, so nothing depends on the model's self-report.
pub fn build_agent_task_result(
    task_id: impl Into<String>,
    events: &[Event],
    final_answer: Option<String>,
    stop: SubagentStop,
    wall_clock_ms: u64,
    checkpoint: Option<String>,
) -> AgentTaskResult {
    let mut findings = Vec::new();
    let mut evidence = Vec::new();
    let mut changed_files = Vec::new();
    let mut validations = Vec::new();
    let mut remaining_work = Vec::new();
    let mut tool_calls: u64 = 0;
    let mut tokens_used: u64 = 0;
    let mut turns: u64 = 0;

    for event in events {
        match event {
            Event::FindingRecorded {
                id,
                details,
                severity,
                evidence: finding_evidence,
                ..
            } => {
                findings.push(Finding {
                    summary: details.clone(),
                    severity: Some(format!("{severity:?}").to_lowercase()),
                    evidence_refs: vec![id.clone()],
                });
                evidence.push(EvidenceRef {
                    kind: "finding".into(),
                    reference: id.clone(),
                    note: Some(finding_evidence.clone()),
                });
            }
            Event::GoalEvaluated {
                satisfied,
                reason,
                turn_count,
                tokens_spent,
            } => {
                validations.push(ValidationResult {
                    name: "goal_evaluated".into(),
                    passed: *satisfied,
                    detail: Some(reason.clone()),
                });
                if !satisfied {
                    remaining_work.push(format!("goal not satisfied: {reason}"));
                }
                turns = turns.max(*turn_count);
                tokens_used = tokens_used.max(*tokens_spent);
            }
            Event::ToolCall {
                name, arguments, ..
            } => {
                tool_calls += 1;
                if matches!(name.as_str(), "write_file" | "edit_file") {
                    if let Some(path) = arguments.get("path").and_then(|v| v.as_str()) {
                        if !changed_files.iter().any(|p: &String| p == path) {
                            changed_files.push(path.to_string());
                        }
                    }
                }
            }
            _ => {}
        }
    }

    let (status, summary) = match &stop {
        SubagentStop::Finished => {
            let summary = final_answer.unwrap_or_else(|| NO_ANSWER_PLACEHOLDER.into());
            // A finish with a failed recorded validation is not a completion; the
            // parent verifier would downgrade it anyway, so mark it here.
            let status = if validations.iter().any(|v| !v.passed) {
                AgentTaskStatus::Partial
            } else {
                AgentTaskStatus::Completed
            };
            (status, summary)
        }
        SubagentStop::Stopped(message) => {
            remaining_work.push(format!("run stopped before completion: {message}"));
            (
                AgentTaskStatus::Partial,
                final_answer.unwrap_or_else(|| message.clone()),
            )
        }
        SubagentStop::Failed(error) => {
            remaining_work.push(format!("run failed: {error}"));
            (
                AgentTaskStatus::Failed,
                final_answer.unwrap_or_else(|| format!("subagent run failed: {error}")),
            )
        }
        SubagentStop::Cancelled => {
            remaining_work.push("run cancelled before completion".into());
            (
                AgentTaskStatus::Cancelled,
                final_answer.unwrap_or_else(|| "subagent run cancelled".into()),
            )
        }
    };

    AgentTaskResult {
        task_id: task_id.into(),
        status,
        summary,
        findings,
        evidence,
        changed_files,
        validations,
        remaining_work,
        usage: ResourceUsage {
            tokens_used,
            tool_calls,
            turns,
            wall_clock_ms,
        },
        checkpoint,
    }
}

/// Deterministic parent-side verdict on a subagent result (AGT-013): the parent
/// must not trust `summary` alone.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResultVerification {
    pub passed: bool,
    pub defects: Vec<String>,
}

/// Deterministic checks a parent applies to any subagent result. A defect means
/// the result cannot be taken at face value.
pub fn verify_agent_task_result(result: &AgentTaskResult) -> ResultVerification {
    let mut defects = Vec::new();

    if result.summary.trim().is_empty() || result.summary.trim() == NO_ANSWER_PLACEHOLDER {
        defects.push("summary is empty or the no-answer placeholder".into());
    }

    if result.status == AgentTaskStatus::Completed {
        for validation in result.validations.iter().filter(|v| !v.passed) {
            defects.push(format!(
                "claims completed but validation '{}' failed",
                validation.name
            ));
        }
        if !result.remaining_work.is_empty() {
            defects.push("claims completed but remaining_work is non-empty".into());
        }
        // Completion with findings but zero evidence is the classic false-finish
        // shape; completion with neither is vacuous but not a false claim.
        if !result.findings.is_empty() && result.evidence.is_empty() {
            defects.push("findings reported without any evidence reference".into());
        }
    }

    if matches!(
        result.status,
        AgentTaskStatus::Partial | AgentTaskStatus::Failed
    ) && result.remaining_work.is_empty()
    {
        defects.push(format!(
            "status {:?} without remaining_work — the parent cannot resume or delegate the rest",
            result.status
        ));
    }

    let known: std::collections::HashSet<&str> = result
        .evidence
        .iter()
        .map(|e| e.reference.as_str())
        .collect();
    for finding in &result.findings {
        for reference in &finding.evidence_refs {
            if !known.contains(reference.as_str()) {
                defects.push(format!("finding references unknown evidence '{reference}'"));
            }
        }
    }

    ResultVerification {
        passed: defects.is_empty(),
        defects,
    }
}

/// What the parent actually consumes: the structured result plus the deterministic
/// verification verdict.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifiedAgentTaskResult {
    #[serde(flatten)]
    pub result: AgentTaskResult,
    pub verification: ResultVerification,
}

/// Verify a result for parent consumption. A defective `Completed` is downgraded
/// to `Partial` and the defects are appended to `remaining_work`, so a false
/// finish can never reach the parent conversation as a clean success (AGT-013).
pub fn wrap_for_parent(mut result: AgentTaskResult) -> VerifiedAgentTaskResult {
    let verification = verify_agent_task_result(&result);
    if !verification.passed && result.status == AgentTaskStatus::Completed {
        result.status = AgentTaskStatus::Partial;
        result.remaining_work.extend(
            verification
                .defects
                .iter()
                .map(|d| format!("verification: {d}")),
        );
    }
    VerifiedAgentTaskResult {
        result,
        verification,
    }
}

/// Isolation knobs shared by every `spawn_subagent` tool instance in the process
/// (AGT-014): the semaphore caps total concurrent subagent runs across nesting
/// levels, and `max_depth` bounds recursion. Cheap to clone — the semaphore is
/// shared, so nested registries draw from the same pool.
#[derive(Clone)]
pub struct SubagentLimits {
    pub max_depth: u32,
    pub slots: Arc<Semaphore>,
}

impl SubagentLimits {
    pub fn new(max_depth: u32, max_concurrent: usize) -> Self {
        Self {
            max_depth,
            slots: Arc::new(Semaphore::new(max_concurrent.max(1))),
        }
    }

    /// Effectively unlimited; the default for ad-hoc tool construction in tests.
    pub fn unlimited() -> Self {
        Self {
            max_depth: u32::MAX,
            slots: Arc::new(Semaphore::new(Semaphore::MAX_PERMITS)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Severity;

    fn finding_event(id: &str) -> Event {
        Event::FindingRecorded {
            id: id.into(),
            finding_type: "sqli".into(),
            confidence: "high".into(),
            severity: Severity::High,
            evidence: "error-based payload returned users table".into(),
            details: "SQL injection in login form".into(),
            attack_type: "sqli".into(),
            location: "https://example.test/login".into(),
            evidence_source: Some("http_request".into()),
        }
    }

    fn goal_event(satisfied: bool, reason: &str) -> Event {
        Event::GoalEvaluated {
            satisfied,
            reason: reason.into(),
            turn_count: 7,
            tokens_spent: 1234,
        }
    }

    #[test]
    fn result_protocol_serializes_round_trip() {
        let result = AgentTaskResult {
            task_id: "task-1".into(),
            status: AgentTaskStatus::Completed,
            summary: "recon complete".into(),
            findings: vec![Finding {
                summary: "SQL injection in login form".into(),
                severity: Some("high".into()),
                evidence_refs: vec!["f-1".into()],
            }],
            evidence: vec![EvidenceRef {
                kind: "finding".into(),
                reference: "f-1".into(),
                note: Some("error-based payload returned users table".into()),
            }],
            changed_files: vec!["/tmp/notes.md".into()],
            validations: vec![ValidationResult {
                name: "goal_evaluated".into(),
                passed: true,
                detail: Some("all subtasks done".into()),
            }],
            remaining_work: vec![],
            usage: ResourceUsage {
                tokens_used: 1234,
                tool_calls: 9,
                turns: 7,
                wall_clock_ms: 4200,
            },
            checkpoint: Some("sub-abc".into()),
        };
        let json = serde_json::to_string(&result).expect("serialize");
        let parsed: AgentTaskResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, result);
        // Status serializes in the documented snake_case wire form.
        assert!(json.contains("\"status\":\"completed\""));

        let verified = wrap_for_parent(result);
        let wire = serde_json::to_string(&verified).expect("serialize verified");
        let back: serde_json::Value = serde_json::from_str(&wire).expect("parse verified");
        assert_eq!(back["verification"]["passed"], true);
        assert_eq!(back["task_id"], "task-1", "flatten keeps protocol fields");
    }

    #[test]
    fn build_collects_findings_validations_files_and_usage_from_events() {
        let events = vec![
            finding_event("f-1"),
            goal_event(false, "subtask scan open"),
            Event::ToolCall {
                name: "write_file".into(),
                arguments: serde_json::json!({"path": "/tmp/report.md", "content": "x"}),
                purpose: None,
                call_id: None,
            },
            Event::ToolCall {
                name: "edit_file".into(),
                arguments: serde_json::json!({"path": "/tmp/report.md"}),
                purpose: None,
                call_id: None,
            },
            Event::ToolCall {
                name: "http_request".into(),
                arguments: serde_json::json!({"url": "https://example.test"}),
                purpose: None,
                call_id: None,
            },
        ];
        let result = build_agent_task_result(
            "task-9",
            &events,
            Some("partial recon".into()),
            SubagentStop::Stopped("max iterations reached".into()),
            1500,
            Some("sub-9".into()),
        );
        assert_eq!(result.status, AgentTaskStatus::Partial);
        assert_eq!(result.findings.len(), 1);
        assert_eq!(result.evidence.len(), 1);
        assert_eq!(result.evidence[0].reference, "f-1");
        assert_eq!(result.changed_files, vec!["/tmp/report.md".to_string()]);
        assert_eq!(result.usage.tool_calls, 3);
        assert_eq!(result.usage.tokens_used, 1234);
        assert_eq!(result.usage.turns, 7);
        assert_eq!(result.usage.wall_clock_ms, 1500);
        assert!(
            result
                .remaining_work
                .iter()
                .any(|w| w.contains("goal not satisfied")),
            "unsatisfied goal lands in remaining_work: {:?}",
            result.remaining_work
        );
        assert!(
            result
                .remaining_work
                .iter()
                .any(|w| w.contains("max iterations reached")),
            "budget/iteration stop lands in remaining_work"
        );
        assert_eq!(result.checkpoint.as_deref(), Some("sub-9"));
    }

    #[test]
    fn build_marks_finish_with_failed_validation_as_partial() {
        let events = vec![goal_event(false, "zero evidence for goal")];
        let result = build_agent_task_result(
            "t",
            &events,
            Some("all done".into()),
            SubagentStop::Finished,
            10,
            None,
        );
        assert_eq!(result.status, AgentTaskStatus::Partial);
    }

    #[test]
    fn verify_rejects_false_completion_without_evidence() {
        // Claims completed, reports findings, but carries no evidence at all.
        let result = AgentTaskResult {
            task_id: "t".into(),
            status: AgentTaskStatus::Completed,
            summary: "found critical vulns".into(),
            findings: vec![Finding {
                summary: "RCE everywhere".into(),
                severity: Some("critical".into()),
                evidence_refs: vec![],
            }],
            evidence: vec![],
            changed_files: vec![],
            validations: vec![],
            remaining_work: vec![],
            usage: ResourceUsage::default(),
            checkpoint: None,
        };
        let verification = verify_agent_task_result(&result);
        assert!(!verification.passed);
        assert!(verification
            .defects
            .iter()
            .any(|d| d.contains("without any evidence")));
    }

    #[test]
    fn verify_rejects_placeholder_summary_and_failed_validation() {
        let result = build_agent_task_result(
            "t",
            &[goal_event(false, "no progress")],
            None,
            SubagentStop::Finished,
            10,
            None,
        );
        // build already downgraded to Partial; verifier must still flag the
        // placeholder summary so the parent sees both defects.
        let verification = verify_agent_task_result(&result);
        assert!(!verification.passed);
        assert!(verification
            .defects
            .iter()
            .any(|d| d.contains("placeholder")));
    }

    #[test]
    fn verify_rejects_dangling_evidence_reference() {
        let result = AgentTaskResult {
            task_id: "t".into(),
            status: AgentTaskStatus::Completed,
            summary: "done".into(),
            findings: vec![Finding {
                summary: "x".into(),
                severity: None,
                evidence_refs: vec!["ghost".into()],
            }],
            evidence: vec![EvidenceRef {
                kind: "finding".into(),
                reference: "f-1".into(),
                note: None,
            }],
            changed_files: vec![],
            validations: vec![],
            remaining_work: vec![],
            usage: ResourceUsage::default(),
            checkpoint: None,
        };
        let verification = verify_agent_task_result(&result);
        assert!(!verification.passed);
        assert!(verification
            .defects
            .iter()
            .any(|d| d.contains("unknown evidence 'ghost'")));
    }

    #[test]
    fn wrap_downgrades_defective_completed_to_partial() {
        let result = AgentTaskResult {
            task_id: "t".into(),
            status: AgentTaskStatus::Completed,
            summary: NO_ANSWER_PLACEHOLDER.into(),
            findings: vec![],
            evidence: vec![],
            changed_files: vec![],
            validations: vec![],
            remaining_work: vec![],
            usage: ResourceUsage::default(),
            checkpoint: None,
        };
        let verified = wrap_for_parent(result);
        assert!(!verified.verification.passed);
        assert_eq!(verified.result.status, AgentTaskStatus::Partial);
        assert!(verified
            .result
            .remaining_work
            .iter()
            .any(|w| w.starts_with("verification:")));
    }

    #[test]
    fn wrap_accepts_clean_result() {
        let result = build_agent_task_result(
            "t",
            &[finding_event("f-1"), goal_event(true, "all subtasks done")],
            Some("recon complete".into()),
            SubagentStop::Finished,
            10,
            None,
        );
        let verified = wrap_for_parent(result);
        assert!(verified.verification.passed, "{:?}", verified.verification);
        assert_eq!(verified.result.status, AgentTaskStatus::Completed);
    }
}
