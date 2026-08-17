use std::collections::VecDeque;

use chrono::{DateTime, Utc};
use holmes_core::event::{Event, StoredEvent};
use holmes_core::types::SubTask;
use holmes_core::ToolOutcomeStatus;

use crate::task_contract::TaskContract;

/// Cap on retained action records — repeat detection only needs a short tail, and an
/// unbounded log would grow with the session.
const MAX_RECENT_ACTIONS: usize = 32;
/// Cap on retained evidence references (same rationale).
const MAX_EVIDENCE_REFS: usize = 64;
/// Snippet length for evidence labels derived from tool output.
const EVIDENCE_SNIPPET_CHARS: usize = 80;
/// Bound on the normalized-arguments summary stored in an evidence record.
const EVIDENCE_INPUT_SUMMARY_CHARS: usize = 200;

/// Tools that record agent-side bookkeeping rather than acting on the target. Their
/// success can never satisfy a contract's action-evidence requirement (P0-02).
const BOOKKEEPING_TOOLS: &[&str] = &[
    "write_todos",
    "report_progress",
    "add_hypothesis",
    "confirm_hypothesis",
    "reject_hypothesis",
];

/// What kind of action produced an evidence record — the basis for the
/// deterministic per-class verification split (P0-02 item 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceKind {
    /// File-modifying tools (`write_file`, `edit_file`).
    FileModification,
    /// Process-executing tools (`execute_command`, `execute_python`).
    CommandExecution,
    /// Network-collecting tools (`http_request`, `web_fetch`, `browser`).
    NetworkCapture,
    /// Any other executed tool (`read_file`, `search`, harness probes, ...).
    OtherTool,
    /// Bookkeeping tools (`write_todos`, progress/hypothesis reporters) — never
    /// counts as action evidence no matter the arguments.
    Bookkeeping,
    /// Projected observation (no tool call backing it).
    Observation,
}

/// How an evidence record was verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationMethod {
    /// The runtime deterministically confirmed the action executed successfully
    /// and bound the record to the output hash.
    Deterministic,
    /// The independent semantic verifier judged this record sufficient.
    Semantic,
    /// Recorded without verification (e.g. projected observations).
    Unverified,
}

impl VerificationMethod {
    pub fn is_deterministic(&self) -> bool {
        matches!(self, Self::Deterministic)
    }
}

/// Strongly typed evidence (P0-02 item 3): a successful tool call reduced to a
/// structured, verifiable record. "The call succeeded" alone is NOT completion
/// evidence — the completion gate additionally requires the record to be an action
/// kind, to reference the contract target, and (for semantic objectives) to survive
/// the independent model review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceRecord {
    /// Sequential id within the session (`ev-1`, `ev-2`, ...).
    pub id: String,
    /// Tool that produced the evidence; empty for projected observations.
    pub tool: String,
    /// Native tool-call id when known (events predate call_id persistence — P1-03 —
    /// so rebuilt records have `None`).
    pub tool_call_id: Option<String>,
    /// Contract this record was collected under, when a contract was active. The
    /// requirement-level binding is computed deterministically by the completion
    /// gate (target/argument matching), never self-declared by the model.
    pub contract_id: Option<String>,
    /// Operator turn sequence in which the observation was collected. Runtime
    /// injected user-role messages do not advance this sequence.
    pub turn_id: u64,
    /// Deterministically matched requirements of `contract_id`. Completion checks
    /// consume this binding instead of reusing any target-looking evidence.
    pub requirement_ids: Vec<String>,
    /// Evidence is only created for a successful typed outcome in Phase 0. Keeping
    /// the status explicit prevents later projections from inferring it from text.
    pub outcome: ToolOutcomeStatus,
    pub kind: EvidenceKind,
    /// Normalized (key-sorted) tool arguments, bounded.
    pub input_summary: String,
    /// SHA-256 of the full tool output — binds this record to the exact content.
    pub output_hash: String,
    /// Bounded single-line snippet of the output. UNTRUSTED data: it may carry
    /// injected instructions and is only ever used for relevance matching or passed
    /// to the semantic verifier inside explicit untrusted-data delimiters.
    pub output_snippet: String,
    /// What this record is claimed to prove (deterministic per-kind predicate).
    pub predicate: String,
    pub verified_by: VerificationMethod,
    /// Wall-clock recording time; `None` for records rebuilt from the event log
    /// (events carry no per-record timestamp).
    pub recorded_at: Option<DateTime<Utc>>,
}

impl EvidenceRecord {
    /// One-line label for logs, verification results and the semantic verifier's
    /// structured input.
    pub fn label(&self) -> String {
        if self.kind == EvidenceKind::Observation {
            return self.output_snippet.clone();
        }
        format!(
            "tool {} succeeded: {}",
            self.tool,
            self.output_snippet.trim()
        )
    }
}

/// Classify a tool into its evidence kind — the deterministic verifier split for
/// file modification / command execution / network capture (P0-02 item 4).
pub fn evidence_kind_for_tool(tool: &str) -> EvidenceKind {
    match tool {
        "write_file" | "edit_file" => EvidenceKind::FileModification,
        "execute_command" | "execute_python" => EvidenceKind::CommandExecution,
        "http_request" | "web_fetch" | "browser" => EvidenceKind::NetworkCapture,
        name if BOOKKEEPING_TOOLS.contains(&name) => EvidenceKind::Bookkeeping,
        _ => EvidenceKind::OtherTool,
    }
}

/// A hypothesis the agent has proposed and not yet confirmed or rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveHypothesis {
    pub id: String,
    pub statement: String,
}

/// One executed (or attempted) tool call, reduced to what supervision needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionRecord {
    pub tool: String,
    pub signature: String,
    pub success: bool,
}

/// Canonical signature of a tool call for repeat detection: the tool name plus its
/// arguments as normalized JSON (object keys sorted, whitespace removed). Two calls
/// with semantically identical arguments produce the same signature even if the raw
/// JSON text differs in key order or spacing; unparseable arguments fall back to the
/// trimmed raw text so detection still works.
pub fn action_signature(tool: &str, arguments: &str) -> String {
    let normalized = serde_json::from_str::<serde_json::Value>(arguments)
        .map(|value| value.to_string())
        .unwrap_or_else(|_| arguments.trim().to_string());
    format!("{tool} {normalized}")
}

/// Structured control state for one engagement (AGT-009): what the agent is trying to
/// achieve, what it believes, what it has proven, and how it has been acting. The
/// `TurnSupervisor` reads this to detect repetition and stagnation; the
/// `CompletionVerifier` reads it to gate `Finish`.
///
/// The state is derivable from the session event log: [`TaskControlState::rebuild`]
/// reconstructs it after a session resume so supervision stays consistent across
/// restarts (per-turn counters like `iterations_since_progress` intentionally restart
/// at zero — they describe the in-flight turn, not the session).
#[derive(Debug, Clone, Default)]
pub struct TaskControlState {
    /// Standing goal/success condition, as recorded by `set_goal`.
    pub goal: Option<String>,
    /// Whether the standing goal has been evaluated as satisfied.
    pub goal_satisfied: bool,
    /// Task contract deterministically derived from the latest action-classified
    /// user request (P0-02). The model can refine the goal but can never remove or
    /// weaken contract requirements.
    pub contract: Option<TaskContract>,
    /// Subtasks attached to the goal, with their latest known status.
    pub subtasks: Vec<SubTask>,
    /// Hypotheses proposed but not yet confirmed or rejected.
    pub hypotheses: Vec<ActiveHypothesis>,
    /// Typed evidence records: successful tool results and projected observations.
    pub evidence: Vec<EvidenceRecord>,
    /// Known gaps that still block completion (e.g. failed verification gaps).
    pub open_questions: Vec<String>,
    /// Recent action signatures, oldest first, capped at `MAX_RECENT_ACTIONS`.
    pub recent_actions: VecDeque<ActionRecord>,
    /// Whether any tool call was attempted this session (gates plain-text answers).
    pub has_tool_activity: bool,
    /// Cumulative progress units (evidence items, goal changes, subtask completions).
    pub progress_score: u64,
    /// Iterations observed this turn.
    pub iterations: usize,
    /// Consecutive iterations without any progress signal.
    pub iterations_since_progress: usize,
    /// Tokens spent so far (session totals, refreshed by the runtime each iteration).
    pub tokens_spent: u64,
    /// Iteration budget for the current turn (from `agent.max_iterations`).
    pub max_iterations: usize,
    /// Current strategy label (free-form; bumped alongside `strategy_switches`).
    pub strategy: String,
    /// How many times the supervisor forced a strategy change this turn.
    pub strategy_switches: usize,
    /// Monotonic session evidence sequence. It is never derived from the bounded
    /// retained vector length, so ev-N is not reused after old records are evicted.
    pub next_evidence_seq: u64,
    /// Genuine operator-turn sequence used to bind new evidence.
    pub current_turn_id: u64,
}

impl TaskControlState {
    pub fn new(max_iterations: usize) -> Self {
        Self {
            max_iterations,
            ..Self::default()
        }
    }

    pub fn set_iteration_budget(&mut self, max_iterations: usize) {
        self.max_iterations = max_iterations;
    }

    /// Begin one genuine operator turn. Steering/supervisor/background wrappers are
    /// part of the active turn and must not call this method.
    pub fn begin_turn(&mut self) {
        self.current_turn_id = self.current_turn_id.saturating_add(1);
    }

    pub fn remaining_iterations(&self) -> usize {
        self.max_iterations.saturating_sub(self.iterations)
    }

    pub fn set_goal(&mut self, condition: impl Into<String>, subtasks: Vec<SubTask>) {
        self.goal = Some(condition.into());
        self.goal_satisfied = false;
        self.subtasks = subtasks;
        self.mark_progress(1);
    }

    pub fn mark_goal_satisfied(&mut self) {
        self.goal_satisfied = true;
    }

    /// Record one tool call outcome. Returns `true` when this signature had not been
    /// seen recently (a novel action — used by the runtime as a progress signal).
    pub fn record_action(&mut self, tool: &str, arguments: &str, success: bool) -> bool {
        self.has_tool_activity = true;
        let signature = action_signature(tool, arguments);
        let is_novel = !self
            .recent_actions
            .iter()
            .any(|record| record.signature == signature);
        self.recent_actions.push_back(ActionRecord {
            tool: tool.to_string(),
            signature,
            success,
        });
        while self.recent_actions.len() > MAX_RECENT_ACTIONS {
            self.recent_actions.pop_front();
        }
        is_novel
    }

    /// Record a projected observation as evidence (no backing tool call).
    pub fn record_evidence(&mut self, reference: String) {
        let id = self.allocate_evidence_id();
        self.push_evidence(EvidenceRecord {
            id,
            tool: String::new(),
            tool_call_id: None,
            contract_id: self.contract.as_ref().map(|contract| contract.id.clone()),
            turn_id: self.current_turn_id,
            requirement_ids: Vec::new(),
            outcome: ToolOutcomeStatus::Succeeded,
            kind: EvidenceKind::Observation,
            input_summary: String::new(),
            output_hash: holmes_core::content_hash(&reference),
            output_snippet: reference,
            predicate: "projected observation".into(),
            verified_by: VerificationMethod::Unverified,
            recorded_at: Some(Utc::now()),
        });
    }

    /// Record a successful tool execution as typed evidence (P0-02): bound to the
    /// tool call id, the normalized input, and the output hash, with the per-class
    /// deterministic predicate.
    pub fn record_tool_evidence(
        &mut self,
        tool: &str,
        tool_call_id: Option<&str>,
        arguments: &str,
        content: &str,
    ) {
        let mut record = self.preview_tool_evidence(tool, tool_call_id, arguments, content);
        record.id = self.allocate_evidence_id();
        self.push_evidence(record);
    }

    /// Build the deterministic compatibility projection for a successful tool
    /// result without mutating the session-scoped ring. The ActionEngine uses
    /// its contract/requirement binding while atomically recording the
    /// case-scoped v2 receipt; `record_tool_evidence` then stores the legacy
    /// projection after execution for existing completion/supervisor callers.
    pub fn preview_tool_evidence(
        &self,
        tool: &str,
        tool_call_id: Option<&str>,
        arguments: &str,
        content: &str,
    ) -> EvidenceRecord {
        let kind = evidence_kind_for_tool(tool);
        let input_summary = normalize_arguments(arguments);
        let single_line: String = content
            .chars()
            .map(|ch| if ch.is_whitespace() { ' ' } else { ch })
            .collect();
        let predicate = match kind {
            EvidenceKind::FileModification => format!("file modified by {tool}"),
            EvidenceKind::CommandExecution => format!("command executed by {tool}"),
            EvidenceKind::NetworkCapture => format!("network response captured by {tool}"),
            EvidenceKind::Bookkeeping => format!("bookkeeping call {tool} succeeded"),
            EvidenceKind::OtherTool => format!("tool {tool} executed successfully"),
            EvidenceKind::Observation => unreachable!("tool evidence is never Observation"),
        };
        let mut record = EvidenceRecord {
            id: String::new(),
            tool: tool.to_string(),
            tool_call_id: tool_call_id.map(ToOwned::to_owned),
            contract_id: self.contract.as_ref().map(|contract| contract.id.clone()),
            turn_id: self.current_turn_id,
            requirement_ids: Vec::new(),
            outcome: ToolOutcomeStatus::Succeeded,
            kind,
            input_summary,
            output_hash: holmes_core::content_hash(content),
            output_snippet: single_line
                .trim()
                .chars()
                .take(EVIDENCE_SNIPPET_CHARS)
                .collect(),
            predicate,
            verified_by: VerificationMethod::Deterministic,
            recorded_at: Some(Utc::now()),
        };
        if let Some(contract) = &self.contract {
            record.requirement_ids = contract.matching_requirement_ids(&record);
        }
        record
    }

    fn push_evidence(&mut self, record: EvidenceRecord) {
        self.evidence.push(record);
        while self.evidence.len() > MAX_EVIDENCE_REFS {
            self.evidence.remove(0);
        }
        self.mark_progress(1);
    }

    fn allocate_evidence_id(&mut self) -> String {
        self.next_evidence_seq = self.next_evidence_seq.saturating_add(1);
        format!("ev-{}", self.next_evidence_seq)
    }

    /// One-line evidence labels (for verification results and operator output).
    pub fn evidence_labels(&self) -> Vec<String> {
        self.evidence.iter().map(EvidenceRecord::label).collect()
    }

    /// Derive and install a task contract from a user request. Non-action requests
    /// return `None` and leave any existing contract untouched — a follow-up "thanks"
    /// must not clobber the standing contract of an engagement.
    pub fn set_contract_from_input(&mut self, input: &str) {
        let sequence = self
            .contract
            .as_ref()
            .map(|contract| {
                contract
                    .id
                    .rsplit('-')
                    .next()
                    .and_then(|n| n.parse::<usize>().ok())
                    .unwrap_or(0)
                    + 1
            })
            .unwrap_or(1);
        if let Some(contract) = TaskContract::derive(input, sequence) {
            self.contract = Some(contract);
        }
    }

    pub fn mark_contract_objective_verified(&mut self) {
        if let Some(contract) = &mut self.contract {
            contract.objective_verified = true;
        }
    }

    /// Whether ANY terminal outcome (a `finish` call or a plain-text answer) must pass
    /// the completion gate. Deterministic criteria, none of them model-declared:
    /// a standing goal exists, a task contract was derived from the user's request,
    /// or tools were used this session. Pure chat (none of the three) is exempt.
    pub fn completion_gate_required(&self) -> bool {
        self.goal.is_some() || self.contract.is_some() || self.has_tool_activity
    }

    pub fn mark_progress(&mut self, units: u64) {
        self.progress_score = self.progress_score.saturating_add(units);
    }

    /// Advance the per-turn iteration counters. `progress_made` is the runtime's
    /// verdict for this iteration (new evidence, a novel successful action, a goal
    /// change); without it the stagnation counter grows.
    pub fn advance_iteration(&mut self, progress_made: bool) {
        self.iterations += 1;
        if progress_made {
            self.iterations_since_progress = 0;
        } else {
            self.iterations_since_progress += 1;
        }
    }

    /// The trailing run of identical action signatures: `(record, count)` of the most
    /// recent signature and how many times it repeats at the tail of the log.
    pub fn trailing_repeat(&self) -> Option<(&ActionRecord, usize)> {
        let last = self.recent_actions.back()?;
        let count = self
            .recent_actions
            .iter()
            .rev()
            .take_while(|record| record.signature == last.signature)
            .count();
        Some((last, count))
    }

    /// Actions whose most recent attempt failed and which were never followed by a
    /// successful call with the same signature. These block completion: a failed step
    /// that was never resolved is a gap, not evidence.
    pub fn unresolved_failures(&self) -> Vec<&ActionRecord> {
        let mut unresolved: Vec<&ActionRecord> = Vec::new();
        for record in self.recent_actions.iter() {
            if record.success {
                unresolved.retain(|existing| existing.signature != record.signature);
            } else if !unresolved
                .iter()
                .any(|existing| existing.signature == record.signature)
            {
                unresolved.push(record);
            }
        }
        unresolved
    }

    pub fn add_open_question(&mut self, question: impl Into<String>) {
        let question = question.into();
        if !question.trim().is_empty() && !self.open_questions.contains(&question) {
            self.open_questions.push(question);
        }
    }

    /// Everything still outstanding, phrased for the operator: the unsatisfied goal,
    /// incomplete subtasks, unmet contract requirements, and recorded open questions.
    /// A budget-exhausted or partial result must list these instead of pretending
    /// completion.
    pub fn remaining_work(&self) -> Vec<String> {
        let mut remaining = Vec::new();
        if let Some(goal) = &self.goal {
            if !self.goal_satisfied {
                remaining.push(format!("goal not satisfied: {goal}"));
            }
        }
        if let Some(contract) = &self.contract {
            for requirement in contract.unmet_deterministic_requirements(&self.evidence) {
                remaining.push(format!(
                    "contract requirement '{}' not met: {}",
                    requirement.id, requirement.description
                ));
            }
            if !contract.objective_verified {
                remaining.push(format!(
                    "contract objective not yet verified: {}",
                    contract.objective
                ));
            }
        }
        for subtask in &self.subtasks {
            if !matches!(subtask.status, holmes_core::types::SubTaskStatus::Completed) {
                remaining.push(format!(
                    "subtask '{}' is {:?}",
                    subtask.description, subtask.status
                ));
            }
        }
        remaining.extend(self.open_questions.iter().cloned());
        remaining
    }

    /// Rebuild the control state from the persisted session event log, so a resumed
    /// session supervises against the same goal, subtasks, hypotheses, evidence and
    /// action history instead of starting blank. The task contract is re-derived
    /// from the recorded operator messages with the same deterministic heuristic the
    /// live path uses (P0-02); `objective_verified` intentionally does not survive a
    /// resume — the next completion re-verifies (fail-closed).
    pub fn rebuild(events: &[Event], max_iterations: usize) -> Self {
        Self::rebuild_inner(events.iter().map(|event| (event, None)), max_iterations)
    }

    /// Same as [`TaskControlState::rebuild`] but consumes stored events, so rebuilt
    /// evidence records also recover their recording timestamp (P1-03).
    pub fn rebuild_from_stored(events: &[StoredEvent], max_iterations: usize) -> Self {
        Self::rebuild_inner(
            events
                .iter()
                .map(|stored| (&stored.event, Some(stored.timestamp))),
            max_iterations,
        )
    }

    fn rebuild_inner<'a>(
        events: impl IntoIterator<Item = (&'a Event, Option<DateTime<Utc>>)>,
        max_iterations: usize,
    ) -> Self {
        let mut state = Self::new(max_iterations);
        // Call ↔ outcome correlation (P1-03): keyed by the native call id, never
        // by adjacency — a parallel batch records every call before any outcome.
        // Events written before call-id persistence fall back to a FIFO of
        // name-matched legacy calls.
        let mut pending_by_id: std::collections::HashMap<String, (String, String)> =
            std::collections::HashMap::new();
        let mut legacy_pending: VecDeque<(String, String)> = VecDeque::new();

        for (event, timestamp) in events {
            match event {
                Event::UserMessage { content, .. } => {
                    // Only genuine operator input can establish a contract — never
                    // runtime-injected wrappers (steering, supervisor notes, background
                    // task reminders), which are persisted as user messages too.
                    if !is_runtime_injected_message(content) {
                        state.begin_turn();
                        state.set_contract_from_input(content);
                    }
                }
                Event::GoalSet {
                    condition,
                    subtasks,
                    ..
                } => {
                    state.goal = Some(condition.clone());
                    state.goal_satisfied = false;
                    state.subtasks = subtasks.clone();
                }
                Event::SubtaskUpdate {
                    subtask_id,
                    status,
                    note,
                } => {
                    if let Some(subtask) = state
                        .subtasks
                        .iter_mut()
                        .find(|task| task.id == *subtask_id)
                    {
                        subtask.status = status.clone();
                        subtask.note = note.clone();
                    }
                }
                Event::GoalEvaluated { satisfied, .. } => {
                    state.goal_satisfied = *satisfied;
                }
                Event::GoalCleared { .. } => {
                    state.goal = None;
                    state.goal_satisfied = false;
                    state.subtasks.clear();
                }
                Event::HypothesisProposed {
                    hypothesis_id,
                    statement,
                    ..
                } => {
                    state.hypotheses.push(ActiveHypothesis {
                        id: hypothesis_id.clone(),
                        statement: statement.clone(),
                    });
                }
                Event::HypothesisConfirmed { hypothesis_id, .. }
                | Event::HypothesisRejected { hypothesis_id, .. } => {
                    state.hypotheses.retain(|h| h.id != *hypothesis_id);
                }
                Event::ToolCall {
                    name,
                    arguments,
                    call_id,
                    ..
                } => {
                    let arguments = arguments.to_string();
                    if let Some(call_id) = call_id {
                        pending_by_id.insert(call_id.clone(), (name.clone(), arguments));
                    } else {
                        legacy_pending.push_back((name.clone(), arguments));
                    }
                }
                Event::ToolResult {
                    name,
                    success,
                    outcome,
                    content,
                    call_id,
                    ..
                } => {
                    let arguments = take_pending(
                        &mut pending_by_id,
                        &mut legacy_pending,
                        call_id.as_deref(),
                        name,
                    )
                    .unwrap_or_default();
                    let typed_success = outcome
                        .unwrap_or(if *success {
                            ToolOutcomeStatus::Succeeded
                        } else {
                            ToolOutcomeStatus::Failed
                        })
                        .is_success()
                        && *success;
                    state.record_action(name, &arguments, typed_success);
                    if typed_success {
                        state.record_tool_evidence(name, call_id.as_deref(), &arguments, content);
                        // Rebuilt records take the persisted event timestamp, not
                        // the rebuild wall clock (closes the P0-02 rebuild tail).
                        if let (Some(timestamp), Some(record)) =
                            (timestamp, state.evidence.last_mut())
                        {
                            record.recorded_at = Some(timestamp);
                        }
                    }
                }
                Event::ToolBlocked {
                    tool_name, call_id, ..
                } => {
                    // A blocked call is a failed action: restore it as one so the
                    // supervisor's repeat/stagnation detection and the completion
                    // gate's unresolved-failure check see it after a resume.
                    let arguments = take_pending(
                        &mut pending_by_id,
                        &mut legacy_pending,
                        call_id.as_deref(),
                        tool_name,
                    )
                    .unwrap_or_default();
                    state.record_action(tool_name, &arguments, false);
                }
                Event::EvidenceObserved { summary, .. } => {
                    state.record_evidence(summary.clone());
                }
                _ => {}
            }
        }

        // Rebuilt counters describe the session, not an in-flight turn.
        state.iterations = 0;
        state.iterations_since_progress = 0;
        state
    }
}

/// Resolve the arguments of the call an outcome event answers (P1-03): by native
/// call id when present, otherwise the oldest still-pending legacy call with the
/// same tool name. Returns `None` when no pending call matches.
fn take_pending(
    pending_by_id: &mut std::collections::HashMap<String, (String, String)>,
    legacy_pending: &mut VecDeque<(String, String)>,
    call_id: Option<&str>,
    name: &str,
) -> Option<String> {
    if let Some(call_id) = call_id {
        return pending_by_id
            .remove(call_id)
            .map(|(_, arguments)| arguments);
    }
    let position = legacy_pending.iter().position(|(tool, _)| tool == name)?;
    legacy_pending
        .remove(position)
        .map(|(_, arguments)| arguments)
}

/// Whether a persisted user message was injected by the runtime itself (steering
/// drain, supervisor note, background-task reminder) rather than typed by the
/// operator. Injected messages must not (re)derive task contracts.
fn is_runtime_injected_message(content: &str) -> bool {
    let trimmed = content.trim_start();
    trimmed.starts_with("The operator sent a message while you were working:")
        || trimmed.starts_with("The turn supervisor intervened:")
        || trimmed.starts_with("<system-reminder>")
}

/// Normalize tool arguments for an evidence record's input summary: parsed JSON
/// with sorted keys (same canonicalization as action signatures), bounded length;
/// unparseable input falls back to the trimmed raw text.
fn normalize_arguments(arguments: &str) -> String {
    let normalized = serde_json::from_str::<serde_json::Value>(arguments)
        .map(|value| value.to_string())
        .unwrap_or_else(|_| arguments.trim().to_string());
    normalized
        .chars()
        .take(EVIDENCE_INPUT_SUMMARY_CHARS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use holmes_core::types::SubTaskStatus;

    #[test]
    fn action_signature_normalizes_key_order_and_whitespace() {
        let a = action_signature("http_request", r#"{ "url": "http://t", "method": "GET" }"#);
        let b = action_signature("http_request", r#"{"method":"GET","url":"http://t"}"#);
        assert_eq!(a, b);
    }

    #[test]
    fn action_signature_distinguishes_different_arguments() {
        let a = action_signature("http_request", r#"{"url":"http://a"}"#);
        let b = action_signature("http_request", r#"{"url":"http://b"}"#);
        assert_ne!(a, b);
    }

    #[test]
    fn action_signature_falls_back_to_trimmed_raw_text() {
        assert_eq!(action_signature("tool", "  not json  "), "tool not json");
    }

    #[test]
    fn record_action_reports_novelty_and_caps_history() {
        let mut state = TaskControlState::default();
        assert!(state.record_action("t", "{}", true));
        assert!(!state.record_action("t", "{}", true));
        for _ in 0..MAX_RECENT_ACTIONS {
            state.record_action("other", "{}", true);
        }
        assert_eq!(state.recent_actions.len(), MAX_RECENT_ACTIONS);
    }

    #[test]
    fn evidence_ids_remain_monotonic_after_retention_eviction() {
        let mut state = TaskControlState::default();
        for index in 0..(MAX_EVIDENCE_REFS + 5) {
            state.record_evidence(format!("observation-{index}"));
        }
        assert_eq!(state.evidence.len(), MAX_EVIDENCE_REFS);
        assert_eq!(state.evidence.first().unwrap().id, "ev-6");
        assert_eq!(state.evidence.last().unwrap().id, "ev-69");
        assert_eq!(state.next_evidence_seq, 69);
    }

    #[test]
    fn evidence_binds_to_operator_turn_contract_and_requirement() {
        let mut state = TaskControlState::default();
        state.begin_turn();
        state.set_contract_from_input("Confirm example.test is reachable.");
        state.record_tool_evidence(
            "http_request",
            Some("call-1"),
            r#"{"url":"https://example.test"}"#,
            "200 OK",
        );
        let first = state.evidence.last().unwrap();
        assert_eq!(first.turn_id, 1);
        assert_eq!(first.contract_id.as_deref(), Some("contract-1"));
        assert_eq!(first.requirement_ids, vec!["req-1"]);

        state.begin_turn();
        state.set_contract_from_input("Confirm example.test is reachable again.");
        assert_eq!(state.contract.as_ref().unwrap().id, "contract-2");
        assert_eq!(
            state
                .contract
                .as_ref()
                .unwrap()
                .unmet_deterministic_requirements(&state.evidence)
                .len(),
            1,
            "old-contract evidence must not satisfy the new contract"
        );
        state.record_tool_evidence(
            "http_request",
            Some("call-2"),
            r#"{"url":"https://example.test"}"#,
            "200 OK",
        );
        let second = state.evidence.last().unwrap();
        assert_eq!(second.turn_id, 2);
        assert_eq!(second.contract_id.as_deref(), Some("contract-2"));
        assert!(state
            .contract
            .as_ref()
            .unwrap()
            .unmet_deterministic_requirements(&state.evidence)
            .is_empty());
    }

    #[test]
    fn persisted_outcome_disagreement_fails_closed() {
        let events = vec![
            Event::ToolCall {
                name: "execute_command".into(),
                arguments: serde_json::json!({"command": "false"}),
                purpose: None,
                call_id: Some("c-1".into()),
            },
            Event::ToolResult {
                name: "execute_command".into(),
                success: true,
                outcome: Some(ToolOutcomeStatus::Failed),
                content: r#"{"exit_code":1}"#.into(),
                error: None,
                artifacts: Vec::new(),
                call_id: Some("c-1".into()),
            },
        ];
        let state = TaskControlState::rebuild(&events, 10);
        assert!(state.evidence.is_empty());
        assert!(!state.recent_actions.back().unwrap().success);
    }

    #[test]
    fn trailing_repeat_counts_only_the_identical_tail() {
        let mut state = TaskControlState::default();
        state.record_action("a", "{}", true);
        state.record_action("b", "{}", true);
        state.record_action("b", "{}", false);
        state.record_action("b", "{}", false);

        let (record, count) = state.trailing_repeat().expect("a trailing run");
        assert_eq!(record.tool, "b");
        assert_eq!(count, 3);
    }

    #[test]
    fn unresolved_failures_clear_once_the_same_call_succeeds() {
        let mut state = TaskControlState::default();
        state.record_action("nmap", r#"{"target":"t"}"#, false);
        state.record_action("curl", r#"{"url":"u"}"#, false);
        assert_eq!(state.unresolved_failures().len(), 2);

        state.record_action("nmap", r#"{"target":"t"}"#, true);
        let unresolved = state.unresolved_failures();
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].tool, "curl");
    }

    #[test]
    fn advance_iteration_tracks_stagnation() {
        let mut state = TaskControlState::default();
        state.advance_iteration(false);
        state.advance_iteration(false);
        assert_eq!(state.iterations_since_progress, 2);
        state.advance_iteration(true);
        assert_eq!(state.iterations_since_progress, 0);
        assert_eq!(state.iterations, 3);
    }

    #[test]
    fn remaining_work_lists_goal_subtasks_and_open_questions() {
        let mut state = TaskControlState::default();
        state.set_goal(
            "exfiltrate the flag",
            vec![
                SubTask {
                    id: "1".into(),
                    description: "find the endpoint".into(),
                    status: SubTaskStatus::Completed,
                    note: None,
                },
                SubTask {
                    id: "2".into(),
                    description: "bypass the filter".into(),
                    status: SubTaskStatus::Active,
                    note: None,
                },
            ],
        );
        state.add_open_question("is the admin panel reachable?");

        let remaining = state.remaining_work();
        assert_eq!(remaining.len(), 3);
        assert!(remaining[0].contains("goal not satisfied"));
        assert!(remaining[1].contains("bypass the filter"));
        assert!(remaining[2].contains("admin panel"));

        state.mark_goal_satisfied();
        state.subtasks[1].status = SubTaskStatus::Completed;
        let remaining = state.remaining_work();
        assert_eq!(remaining.len(), 1);
    }

    #[test]
    fn rebuild_restores_goal_evidence_and_actions_from_events() {
        let events = vec![
            Event::GoalSet {
                condition: "pop a shell".into(),
                plan: None,
                subtasks: vec![SubTask {
                    id: "1".into(),
                    description: "gain rce".into(),
                    status: SubTaskStatus::Pending,
                    note: None,
                }],
            },
            Event::HypothesisProposed {
                hypothesis_id: "h1".into(),
                statement: "endpoint is injectable".into(),
                rationale: "error banner".into(),
                confidence: None,
                attack_type: None,
                entry_points: Vec::new(),
            },
            Event::ToolCall {
                name: "echo_probe".into(),
                arguments: serde_json::json!({"target": "example.test"}),
                purpose: None,
                call_id: None,
            },
            Event::ToolResult {
                name: "echo_probe".into(),
                success: true,
                outcome: Some(ToolOutcomeStatus::Succeeded),
                content: "example.test is reachable".into(),
                error: None,
                artifacts: Vec::new(),
                call_id: None,
            },
            Event::ToolCall {
                name: "echo_probe".into(),
                arguments: serde_json::json!({"target": "other.test"}),
                purpose: None,
                call_id: None,
            },
            Event::ToolResult {
                name: "echo_probe".into(),
                success: false,
                outcome: Some(ToolOutcomeStatus::Failed),
                content: String::new(),
                error: Some("connection refused".into()),
                artifacts: Vec::new(),
                call_id: None,
            },
        ];

        let state = TaskControlState::rebuild(&events, 90);

        assert_eq!(state.goal.as_deref(), Some("pop a shell"));
        assert!(!state.goal_satisfied);
        assert_eq!(state.subtasks.len(), 1);
        assert_eq!(state.hypotheses.len(), 1);
        assert_eq!(state.hypotheses[0].statement, "endpoint is injectable");
        assert_eq!(state.recent_actions.len(), 2);
        assert_eq!(state.evidence.len(), 1);
        assert_eq!(state.evidence[0].tool, "echo_probe");
        assert!(state.evidence[0].output_snippet.contains("reachable"));
        assert!(state.has_tool_activity);
        assert_eq!(state.unresolved_failures().len(), 1);
        assert!(state.progress_score >= 1);
        assert_eq!(state.iterations_since_progress, 0);

        // A satisfied goal evaluation in the log flips the rebuilt state too.
        let mut events = events;
        events.push(Event::GoalEvaluated {
            satisfied: true,
            reason: "verified".into(),
            turn_count: 3,
            tokens_spent: 100,
        });
        assert!(TaskControlState::rebuild(&events, 90).goal_satisfied);
    }

    #[test]
    fn completion_gate_triggers_are_deterministic() {
        // Pure chat: no goal, no contract, no tool activity — exempt.
        let state = TaskControlState::default();
        assert!(!state.completion_gate_required());

        // Tool activity alone gates.
        let mut state = TaskControlState::default();
        state.record_action("read_file", r#"{"path":"/tmp/x"}"#, true);
        assert!(state.completion_gate_required());

        // A standing goal gates.
        let mut state = TaskControlState::default();
        state.set_goal("confirm the flag", Vec::new());
        assert!(state.completion_gate_required());

        // An action-classified user request gates via the derived contract.
        let mut state = TaskControlState::default();
        state.set_contract_from_input("Confirm example.test is reachable.");
        assert!(state.contract.is_some());
        assert!(state.completion_gate_required());

        // Chat input never derives a contract.
        let mut state = TaskControlState::default();
        state.set_contract_from_input("what is a csrf token?");
        assert!(state.contract.is_none());
    }

    #[test]
    fn contract_does_not_weaken_when_follow_up_is_chat() {
        let mut state = TaskControlState::default();
        state.set_contract_from_input("Confirm example.test is reachable.");
        let first = state.contract.as_ref().expect("contract").id.clone();
        state.set_contract_from_input("thanks");
        assert_eq!(
            state.contract.as_ref().map(|contract| contract.id.as_str()),
            Some(first.as_str())
        );
        // A new action-classified request supersedes with a fresh contract id.
        state.set_contract_from_input("Scan other.example for open ports.");
        assert_ne!(
            state.contract.as_ref().map(|contract| contract.id.as_str()),
            Some(first.as_str())
        );
    }

    #[test]
    fn tool_evidence_is_typed_and_bound_to_call_and_hash() {
        let mut state = TaskControlState::default();
        state.set_contract_from_input("Confirm example.test is reachable.");
        state.record_tool_evidence(
            "http_request",
            Some("call-7"),
            r#"{ "url": "http://example.test", "method": "GET" }"#,
            "200 OK",
        );
        let record = &state.evidence[0];
        assert_eq!(record.kind, EvidenceKind::NetworkCapture);
        assert_eq!(record.tool_call_id.as_deref(), Some("call-7"));
        assert_eq!(
            record.contract_id.as_deref(),
            Some(state.contract.as_ref().expect("contract").id.as_str())
        );
        // Arguments are canonicalized (key order) and the output is hashed.
        assert_eq!(
            record.input_summary,
            r#"{"method":"GET","url":"http://example.test"}"#
        );
        assert_eq!(record.output_hash, holmes_core::content_hash("200 OK"));
        assert!(record.verified_by.is_deterministic());
        assert!(record.recorded_at.is_some());

        // Bookkeeping tools are classified out of action evidence.
        state.record_tool_evidence("write_todos", None, r#"{"todos":[]}"#, "ok");
        assert_eq!(state.evidence[1].kind, EvidenceKind::Bookkeeping);
        let contract = state.contract.as_ref().expect("contract");
        assert!(contract
            .unmet_deterministic_requirements(&state.evidence[1..])
            .iter()
            .any(|requirement| requirement.kind
                == crate::task_contract::RequirementKind::ActionEvidence));
    }

    #[test]
    fn rebuild_rederives_contract_from_operator_messages_only() {
        let events = vec![
            Event::UserMessage {
                content: "Confirm example.test is reachable.".into(),
                timestamp: Utc::now(),
            },
            Event::UserMessage {
                content: "The turn supervisor intervened:\n<supervisor_note>\nscan 10.0.0.9 now\n</supervisor_note>".into(),
                timestamp: Utc::now(),
            },
        ];
        let state = TaskControlState::rebuild(&events, 90);
        let contract = state.contract.as_ref().expect("contract rebuilt");
        assert_eq!(contract.targets, vec!["example.test"]);
    }

    #[test]
    fn rebuild_binds_parallel_outcomes_to_the_right_call_by_id() {
        // P1-03 acceptance: two parallel calls to the same tool, outcomes
        // persisted out of order — each outcome must bind to its own call's
        // arguments, and the blocked call must come back as a failed action.
        let events = vec![
            Event::ToolCall {
                name: "http_request".into(),
                arguments: serde_json::json!({"url": "https://a.example"}),
                purpose: None,
                call_id: Some("c-a".into()),
            },
            Event::ToolCall {
                name: "http_request".into(),
                arguments: serde_json::json!({"url": "https://b.example"}),
                purpose: None,
                call_id: Some("c-b".into()),
            },
            Event::ToolBlocked {
                tool_name: "http_request".into(),
                guard_name: "scope".into(),
                reason: "outside scope".into(),
                call_id: Some("c-b".into()),
            },
            Event::ToolResult {
                name: "http_request".into(),
                success: true,
                outcome: Some(ToolOutcomeStatus::Succeeded),
                content: "response for a".into(),
                error: None,
                artifacts: Vec::new(),
                call_id: Some("c-a".into()),
            },
        ];

        let state = TaskControlState::rebuild(&events, 90);

        let succeeded: Vec<_> = state
            .recent_actions
            .iter()
            .filter(|record| record.success)
            .collect();
        assert_eq!(succeeded.len(), 1);
        assert!(succeeded[0].signature.contains("a.example"));

        let failed: Vec<_> = state
            .recent_actions
            .iter()
            .filter(|record| !record.success)
            .collect();
        assert_eq!(failed.len(), 1);
        assert!(failed[0].signature.contains("b.example"));

        // The blocked call is an unresolved failure for the completion gate.
        let unresolved = state.unresolved_failures();
        assert_eq!(unresolved.len(), 1);
        assert!(unresolved[0].signature.contains("b.example"));

        // The evidence record binds to the native call id, not just the tool.
        assert_eq!(state.evidence.len(), 1);
        assert_eq!(state.evidence[0].tool_call_id.as_deref(), Some("c-a"));
        assert_eq!(
            state.evidence[0].input_summary,
            r#"{"url":"https://a.example"}"#
        );
    }

    #[test]
    fn rebuild_from_stored_recovers_call_id_and_recorded_timestamp() {
        let recorded = DateTime::from_timestamp(1_700_000_000, 0).expect("valid timestamp");
        let stored = vec![
            StoredEvent {
                id: 0,
                session_id: "s".into(),
                event_index: 0,
                turn_index: None,
                timestamp: recorded,
                event: Event::ToolCall {
                    name: "http_request".into(),
                    arguments: serde_json::json!({"url": "https://a.example"}),
                    purpose: None,
                    call_id: Some("c-1".into()),
                },
            },
            StoredEvent {
                id: 1,
                session_id: "s".into(),
                event_index: 1,
                turn_index: None,
                timestamp: recorded,
                event: Event::ToolResult {
                    name: "http_request".into(),
                    success: true,
                    outcome: Some(ToolOutcomeStatus::Succeeded),
                    content: "ok".into(),
                    error: None,
                    artifacts: Vec::new(),
                    call_id: Some("c-1".into()),
                },
            },
        ];

        let state = TaskControlState::rebuild_from_stored(&stored, 90);
        assert_eq!(state.evidence.len(), 1);
        assert_eq!(state.evidence[0].tool_call_id.as_deref(), Some("c-1"));
        assert_eq!(state.evidence[0].recorded_at, Some(recorded));
    }
}
