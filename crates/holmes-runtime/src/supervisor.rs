use crate::deliberation::{RuntimeError, RuntimeErrorKind};
use crate::task_control::TaskControlState;

/// Four-way error classification for supervision (AGT-009), aligned with the
/// provider-level `FailureClass` semantics from the LLM client (PR2):
///
/// - `Retryable` ≈ `FailureClass::Transient`: the same action may succeed on retry.
/// - `StrategyChange` ≈ `FailureClass::RequestContent`: repeating the same approach
///   is pointless — the agent must change what it does, not just when.
/// - `NeedsUser` ≈ `FailureClass::ProviderConfig`: a human must fix something
///   (credentials, input, a decision) before progress is possible.
/// - `Unrecoverable`: the turn cannot continue; preserve partial results and stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupervisorErrorClass {
    Retryable,
    StrategyChange,
    NeedsUser,
    Unrecoverable,
}

/// Classify a runtime error into the supervision taxonomy.
pub fn classify_runtime_error(error: &RuntimeError) -> SupervisorErrorClass {
    match error.kind {
        RuntimeErrorKind::Recoverable => SupervisorErrorClass::Retryable,
        RuntimeErrorKind::ContextOverflow => SupervisorErrorClass::StrategyChange,
        RuntimeErrorKind::NeedsUser => SupervisorErrorClass::NeedsUser,
        RuntimeErrorKind::Fatal => SupervisorErrorClass::Unrecoverable,
        // Intercepted by the runtime before supervision ever sees it (P1-01).
        RuntimeErrorKind::Cancelled => SupervisorErrorClass::Unrecoverable,
    }
}

/// What the supervisor concludes after observing one iteration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Supervision {
    /// Nothing wrong; keep going.
    Continue,
    /// The same tool call (identical signature) keeps failing. First escalation:
    /// the runtime nudges the model to change strategy.
    ChangeStrategy {
        tool: String,
        signature: String,
        count: usize,
    },
    /// The same failing call kept repeating even after a strategy nudge. Final
    /// escalation: stop the turn and hand off to the operator.
    StopForUser {
        tool: String,
        signature: String,
        count: usize,
    },
    /// No progress (no new evidence, no novel successful action, no goal change)
    /// for `stagnation_limit` consecutive iterations. First escalation: the runtime
    /// injects a reflection prompt.
    Stagnation { iterations_without_progress: usize },
    /// Stagnation persisted for another full window after the reflection prompt.
    /// Final escalation: stop the turn with a resumable partial result.
    StagnationStop { iterations_without_progress: usize },
}

/// Watches the per-turn `TaskControlState` for the two failure modes an LLM loop
/// cannot reliably notice itself (AGT-009): banging on the same failing call, and
/// spinning without producing evidence or task-state change.
///
/// The supervisor is stateful across the iterations of one turn: it remembers which
/// signature it already nudged about (so the second detection escalates to a stop)
/// and whether it already fired a stagnation warning (so continued stagnation
/// escalates instead of re-warning every iteration).
#[derive(Debug, Clone)]
pub struct TurnSupervisor {
    /// Consecutive identical failing calls tolerated before a strategy nudge.
    max_repeat: usize,
    /// Consecutive progress-free iterations tolerated before a reflection prompt.
    stagnation_limit: usize,
    /// Signature already nudged for, and the repeat count at nudge time.
    nudged: Option<(String, usize)>,
    /// Whether a stagnation reflection prompt was already injected this turn.
    stagnation_warned: bool,
}

impl TurnSupervisor {
    pub fn new(max_repeat: usize, stagnation_limit: usize) -> Self {
        Self {
            max_repeat: max_repeat.max(1),
            stagnation_limit: stagnation_limit.max(1),
            nudged: None,
            stagnation_warned: false,
        }
    }

    /// Assess the control state after one iteration. Repetition takes precedence
    /// over stagnation: a tight failing loop should be stopped before the slower
    /// stagnation window matters.
    pub fn assess(&mut self, control: &TaskControlState) -> Supervision {
        if control.iterations_since_progress == 0 {
            self.stagnation_warned = false;
        }

        if let Some((last, count)) = control.trailing_repeat() {
            if !last.success && count >= self.max_repeat {
                let signature = last.signature.clone();
                let nudged_at = self
                    .nudged
                    .as_ref()
                    .filter(|(nudged_sig, _)| *nudged_sig == signature)
                    .map(|(_, nudged_count)| *nudged_count);
                return match nudged_at {
                    // The model ignored the nudge and repeated the same failing call
                    // again — escalate to a stop.
                    Some(nudged_count) if count > nudged_count => Supervision::StopForUser {
                        tool: last.tool.clone(),
                        signature,
                        count,
                    },
                    Some(_) => Supervision::Continue,
                    None => {
                        self.nudged = Some((signature.clone(), count));
                        Supervision::ChangeStrategy {
                            tool: last.tool.clone(),
                            signature,
                            count,
                        }
                    }
                };
            }
        }

        let stalled = control.iterations_since_progress;
        if stalled >= self.stagnation_limit {
            if self.stagnation_warned && stalled >= self.stagnation_limit * 2 {
                return Supervision::StagnationStop {
                    iterations_without_progress: stalled,
                };
            }
            if !self.stagnation_warned {
                self.stagnation_warned = true;
                return Supervision::Stagnation {
                    iterations_without_progress: stalled,
                };
            }
        }

        Supervision::Continue
    }

    /// Build the message for a budget-exhausted (or otherwise stopped) turn: the
    /// reason plus an explicit list of what remains undone, so the result is
    /// resumable instead of a bare failure or a fake completion (AGT-009/010).
    pub fn budget_exhausted_message(&self, reason: &str, control: &TaskControlState) -> String {
        let mut message = format!(
            "{reason}\n\nPartial progress is preserved (progress score {}, {} iterations, {} evidence items).",
            control.progress_score,
            control.iterations,
            control.evidence.len()
        );
        let remaining = control.remaining_work();
        if remaining.is_empty() {
            message.push_str("\nRemaining work: none recorded.");
        } else {
            message.push_str("\nRemaining work:");
            for item in remaining {
                message.push_str("\n- ");
                message.push_str(&item);
            }
        }
        message
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn control_with_failed_repeats(repeats: usize) -> TaskControlState {
        let mut control = TaskControlState::default();
        for _ in 0..repeats {
            control.record_action("nmap", r#"{"target":"t"}"#, false);
            control.advance_iteration(false);
        }
        control
    }

    #[test]
    fn repeated_identical_failing_call_triggers_strategy_change_at_threshold() {
        let mut supervisor = TurnSupervisor::new(3, 10);
        let control = control_with_failed_repeats(2);
        assert_eq!(supervisor.assess(&control), Supervision::Continue);

        let control = control_with_failed_repeats(3);
        match supervisor.assess(&control) {
            Supervision::ChangeStrategy { tool, count, .. } => {
                assert_eq!(tool, "nmap");
                assert_eq!(count, 3);
            }
            other => panic!("expected ChangeStrategy, got {other:?}"),
        }
    }

    #[test]
    fn equivalent_arguments_count_as_the_same_action() {
        let mut supervisor = TurnSupervisor::new(2, 10);
        let mut control = TaskControlState::default();
        control.record_action("http", r#"{ "url": "http://t", "method": "GET" }"#, false);
        control.record_action("http", r#"{"method":"GET","url":"http://t"}"#, false);

        assert!(matches!(
            supervisor.assess(&control),
            Supervision::ChangeStrategy { .. }
        ));
    }

    #[test]
    fn distinct_or_successful_calls_do_not_trigger_repetition() {
        let mut supervisor = TurnSupervisor::new(3, 10);
        let mut control = TaskControlState::default();
        control.record_action("nmap", "{}", false);
        control.record_action("nmap", "{}", false);
        control.record_action("nmap", "{}", true);
        assert_eq!(supervisor.assess(&control), Supervision::Continue);

        let mut control = TaskControlState::default();
        control.record_action("a", "{}", false);
        control.record_action("b", "{}", false);
        control.record_action("c", "{}", false);
        assert_eq!(supervisor.assess(&control), Supervision::Continue);
    }

    #[test]
    fn repeating_after_a_nudge_escalates_to_stop_for_user() {
        let mut supervisor = TurnSupervisor::new(3, 10);
        let control = control_with_failed_repeats(3);
        assert!(matches!(
            supervisor.assess(&control),
            Supervision::ChangeStrategy { .. }
        ));

        // Same call again after the nudge: escalate.
        let control = control_with_failed_repeats(4);
        assert!(matches!(
            supervisor.assess(&control),
            Supervision::StopForUser { count: 4, .. }
        ));
    }

    #[test]
    fn n_iterations_without_progress_trigger_stagnation_once_then_stop() {
        let mut supervisor = TurnSupervisor::new(5, 3);
        let mut control = TaskControlState::default();

        for _ in 0..2 {
            control.advance_iteration(false);
        }
        assert_eq!(supervisor.assess(&control), Supervision::Continue);

        control.advance_iteration(false);
        assert_eq!(
            supervisor.assess(&control),
            Supervision::Stagnation {
                iterations_without_progress: 3
            }
        );

        // Still stalled inside the second window: no repeated warning.
        control.advance_iteration(false);
        assert_eq!(supervisor.assess(&control), Supervision::Continue);

        // A second full window with no progress: stop with a resumable result.
        control.advance_iteration(false);
        control.advance_iteration(false);
        assert_eq!(
            supervisor.assess(&control),
            Supervision::StagnationStop {
                iterations_without_progress: 6
            }
        );
    }

    #[test]
    fn progress_resets_the_stagnation_warning() {
        let mut supervisor = TurnSupervisor::new(5, 2);
        let mut control = TaskControlState::default();
        control.advance_iteration(false);
        control.advance_iteration(false);
        assert!(matches!(
            supervisor.assess(&control),
            Supervision::Stagnation { .. }
        ));

        control.advance_iteration(true);
        assert_eq!(supervisor.assess(&control), Supervision::Continue);

        control.advance_iteration(false);
        control.advance_iteration(false);
        assert!(matches!(
            supervisor.assess(&control),
            Supervision::Stagnation { .. }
        ));
    }

    #[test]
    fn budget_exhausted_message_lists_remaining_work() {
        let supervisor = TurnSupervisor::new(3, 4);
        let mut control = TaskControlState::new(90);
        control.set_goal("exfiltrate the flag", Vec::new());
        control.add_open_question("is the admin panel reachable?");
        control.record_evidence("tool echo_probe succeeded: ok".into());

        let message = supervisor.budget_exhausted_message("iteration budget exhausted", &control);

        assert!(message.contains("iteration budget exhausted"));
        assert!(message.contains("Remaining work:"));
        assert!(message.contains("goal not satisfied: exfiltrate the flag"));
        assert!(message.contains("is the admin panel reachable?"));
    }

    #[test]
    fn budget_exhausted_message_handles_nothing_remaining() {
        let supervisor = TurnSupervisor::new(3, 4);
        let control = TaskControlState::default();
        let message = supervisor.budget_exhausted_message("turn deadline", &control);
        assert!(message.contains("Remaining work: none recorded."));
    }

    #[test]
    fn error_classification_aligns_with_failure_class_semantics() {
        assert_eq!(
            classify_runtime_error(&RuntimeError::recoverable("flaky")),
            SupervisorErrorClass::Retryable
        );
        assert_eq!(
            classify_runtime_error(&RuntimeError {
                kind: RuntimeErrorKind::ContextOverflow,
                message: "too long".into(),
            }),
            SupervisorErrorClass::StrategyChange
        );
        assert_eq!(
            classify_runtime_error(&RuntimeError::needs_user("need creds")),
            SupervisorErrorClass::NeedsUser
        );
        assert_eq!(
            classify_runtime_error(&RuntimeError::fatal("boom")),
            SupervisorErrorClass::Unrecoverable
        );
    }
}
