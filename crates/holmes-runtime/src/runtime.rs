use chrono::Utc;
use holmes_core::event::Event;
use holmes_core::tool_types::LlmResponse;
use holmes_core::types::TokenDelta;
use holmes_core::InterventionLevel;

use crate::action::ActionEngine;
use crate::cognition::{CognitiveEngine, CognitiveResult};
use crate::compaction::{CaseCompactor, CompressionResult, PrefireResult, SpanFingerprint};
use crate::completion::{CompletionVerifier, Verification};
use crate::context::RuntimeContext;
use crate::decision::HolmesDecision;
use crate::deliberation::RuntimeError;
use crate::dialogue::DialogueEngine;
use crate::evidence::EvidenceEngine;
use crate::learning::{record_review_started, LearningEngine};
use crate::memory::MemoryEngine;
use crate::perception::PerceptionEngine;
use crate::permissions::ApprovalHandler;
use crate::reflection::{ReflectionEngine, ReflectionOutcome};
use crate::supervisor::{Supervision, TurnSupervisor};
use crate::yield_stream::{RuntimeSink, RuntimeYield};
use crate::DeliberationEngine;

/// Max force-compaction rounds when the API keeps reporting context overflow.
const MAX_OVERFLOW_COMPACTIONS: usize = 3;
/// Per-message content cap applied as an overflow last resort (when compaction can't
/// shrink the protected head/tail because a single message is enormous).
const EMERGENCY_MESSAGE_CHAR_CAP: usize = 8000;
/// Cap on the background-task result text injected into the conversation (and shown
/// in the `BackgroundTaskFinished` yield). The full result stays in the task registry,
/// queryable via `get_task_output`.
const BACKGROUND_INJECT_CHAR_CAP: usize = 2000;
/// How far below the compaction threshold the two-pass prefire window opens: once the
/// token estimate reaches `(threshold - PREFIRE_WINDOW) * context_limit`, a background
/// compressor-role summary of the would-be-compacted middle is spawned so the real
/// compaction can reuse it (grok-build prefire). LLM-summary mode only.
const PREFIRE_WINDOW: f64 = 0.10;

/// Handle for an in-flight background prefire summary. Dropping the slot aborts the
/// task, so a stale prefire never outlives the turn/session it was spawned for.
struct PrefireSlot {
    fingerprint: SpanFingerprint,
    result: std::sync::Arc<std::sync::Mutex<Option<PrefireResult>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for PrefireSlot {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserTurnInput {
    pub content: String,
}

impl UserTurnInput {
    pub fn new(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
        }
    }
}

impl From<String> for UserTurnInput {
    fn from(content: String) -> Self {
        Self::new(content)
    }
}

impl From<&str> for UserTurnInput {
    fn from(content: &str) -> Self {
        Self::new(content)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnOutcome {
    FinalAnswer {
        content: String,
        iterations: usize,
    },
    NeedsUser {
        prompt: String,
        iterations: usize,
    },
    MaxIterationsReached {
        message: String,
        iterations: usize,
    },
    /// The turn was cancelled cooperatively (e.g. the user pressed Esc in the TUI).
    /// The loop stopped at an iteration boundary; partial progress is persisted.
    Interrupted {
        iterations: usize,
    },
}

/// How the unified completion gate (P0-02) tells the turn loop to proceed.
enum CompletionGateOutcome {
    /// Claim verified: proceed to the final answer.
    Passed,
    /// Claim rejected; the gaps were fed back into the conversation — keep iterating.
    Rejected,
    /// Verification retry budget exhausted: end the turn with this explicit,
    /// resumable partial result instead of a fake completion.
    Exhausted(String),
}

pub struct AgentRuntime {
    context: RuntimeContext,
    perception: PerceptionEngine,
    deliberation: DeliberationEngine,
    cognition: CognitiveEngine,
    action: ActionEngine,
    evidence: EvidenceEngine,
    learning: LearningEngine,
    memory: MemoryEngine,
    reflection: ReflectionEngine,
    dialogue: DialogueEngine,
    compactor: CaseCompactor,
    /// Watches each iteration for repeated failing calls and stalled progress
    /// (AGT-009). Stateful within a turn: nudges escalate to stops.
    supervisor: TurnSupervisor,
    /// Independent gate on `Finish` decisions (AGT-010): deterministic checks first,
    /// model review only for semantic goals.
    completion: CompletionVerifier,
    /// Whether `task_control` was rebuilt from the persisted event log (first turn
    /// after construction/resume).
    control_seeded: bool,
    /// Actual `prompt_tokens` from the most recent LLM response — used as a calibrated
    /// floor for the compaction trigger (the char/4 message estimate omits the system
    /// prompt + tool definitions and undercounts non-ASCII text).
    last_prompt_tokens: u64,
    /// In-flight (or finished) background prefire summary for the next compaction.
    prefire: Option<PrefireSlot>,
}

impl AgentRuntime {
    pub fn new(context: RuntimeContext) -> Self {
        let max_iterations = context.config.agent.max_iterations.max(1) as usize;
        let supervisor_config = context.config.supervisor.clone();
        let mut action = ActionEngine::new();
        // Checkpoints live in the persistent per-session directory when the
        // session store has one (P1-05); the system-temp fallback only serves
        // callers without a store.
        action.hooks.push(std::sync::Arc::new(
            crate::hooks::checkpoint::CheckpointHook::for_session(
                &context.session_id,
                context.session_db.sessions_dir().as_deref(),
            ),
        ));

        Self {
            context,
            perception: PerceptionEngine,
            deliberation: DeliberationEngine::default(),
            cognition: CognitiveEngine,
            action,
            evidence: EvidenceEngine::new(),
            learning: LearningEngine::new(),
            memory: MemoryEngine::new(),
            reflection: ReflectionEngine::new(max_iterations),
            dialogue: DialogueEngine,
            compactor: CaseCompactor::new(),
            supervisor: TurnSupervisor::new(
                supervisor_config.max_repeat_action as usize,
                supervisor_config.stagnation_limit as usize,
            ),
            completion: CompletionVerifier::new(supervisor_config.model_verification),
            control_seeded: false,
            last_prompt_tokens: 0,
            prefire: None,
        }
    }

    pub fn context(&self) -> &RuntimeContext {
        &self.context
    }

    pub fn context_mut(&mut self) -> &mut RuntimeContext {
        &mut self.context
    }

    pub fn into_context(self) -> RuntimeContext {
        self.context
    }

    /// Install an interactive approval handler. It is consulted for mutating tool calls
    /// when the permission mode is `Ask`; surfaces without an approval UI (REPL, one-shot)
    /// simply never call this.
    pub fn set_approver(&mut self, approver: std::sync::Arc<dyn ApprovalHandler>) {
        self.action.approver = Some(approver);
    }

    pub async fn run_oneshot(
        &mut self,
        input: impl Into<UserTurnInput>,
        sink: &mut dyn RuntimeSink,
    ) -> Result<TurnOutcome, RuntimeError> {
        self.run_turn(input, sink).await
    }

    pub async fn compact_now(&mut self) -> Result<Option<CompressionResult>, RuntimeError> {
        self.compact_with_trigger(true, holmes_core::CompactionTrigger::Manual)
            .await
    }

    pub async fn run_turn(
        &mut self,
        input: impl Into<UserTurnInput>,
        sink: &mut dyn RuntimeSink,
    ) -> Result<TurnOutcome, RuntimeError> {
        // Fresh execution boundary per turn (AGT-002): tokens are one-shot, so a
        // cancelled token from an earlier turn must not gate this one. Config-derived
        // turn/tool deadlines apply from here on.
        self.context.renew_execution_context();

        // Bridge the legacy AtomicBool interrupt (TUI Esc) into the execution token so
        // in-flight tools observe cancellation promptly instead of at the next
        // iteration boundary. Aborted on every exit path below.
        let bridge = {
            let flag = self.context.cancel.clone();
            let token = self.context.exec.token();
            tokio::spawn(async move {
                loop {
                    if flag.load(std::sync::atomic::Ordering::Relaxed) || token.is_cancelled() {
                        token.cancel();
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                }
            })
        };

        let started_at = std::time::Instant::now();
        let outcome = self.run_turn_inner(input, sink).await;

        bridge.abort();
        if self.context.exec.is_cancelled() {
            tracing::info!(
                event = "CancellationCompleted",
                task_id = %self.context.exec.task_id(),
                session_id = %self.context.session_id,
                "turn finished after cancellation was requested"
            );
        }

        // Turn-level metrics (AGT-015): latency sample plus one outcome counter —
        // the success rate and percentiles the plan requires derive from these.
        let metrics = holmes_core::metrics::metrics();
        metrics.record_duration("turn.duration_ms", started_at.elapsed());
        match &outcome {
            Ok(TurnOutcome::FinalAnswer { .. }) => metrics.count("turn.final_answer"),
            Ok(TurnOutcome::NeedsUser { .. }) => metrics.count("turn.needs_user"),
            Ok(TurnOutcome::MaxIterationsReached { .. }) => {
                metrics.count("turn.max_iterations_reached")
            }
            Ok(TurnOutcome::Interrupted { .. }) => metrics.count("turn.interrupted"),
            Err(_) => metrics.count("turn.error"),
        }
        outcome
    }

    async fn run_turn_inner(
        &mut self,
        input: impl Into<UserTurnInput>,
        sink: &mut dyn RuntimeSink,
    ) -> Result<TurnOutcome, RuntimeError> {
        let middlewares = self.context.middlewares.clone();
        for mw in &middlewares {
            mw.on_session_start(&mut self.context).await?;
        }

        // Restore findings recorded in prior turns/resumed sessions into the fresh
        // AttackState so the validated zone (and the [Current situation] frame) reflects
        // everything found so far, not just this turn.
        self.context.seed_findings_from_history().await;

        // Rebuild the supervision control state from the persisted event log once per
        // runtime lifetime, so a resumed session supervises against the same goal,
        // evidence and action history (AGT-009/010). Best-effort like the findings
        // seed above: a read failure must not block the turn.
        if !self.control_seeded {
            let max_iterations = self.context.config.agent.max_iterations.max(1) as usize;
            if let Ok(stored) = self
                .context
                .session_db
                .get_events(&self.context.session_id)
                .await
            {
                self.context.state.task_control =
                    crate::task_control::TaskControlState::rebuild_from_stored(
                        &stored,
                        max_iterations,
                    );
            }
            self.context
                .state
                .task_control
                .set_iteration_budget(max_iterations);
            self.control_seeded = true;
        }
        self.refresh_ledger().await?;
        self.context.state.action_bindings.clear();

        // Rejected terminal claims (finish or gated answers) are fed back into the
        // loop this many times before the turn ends with a partial result listing
        // the remaining work (AGT-010/P0-02). Read inside the completion gate.
        let mut verification_failures = 0usize;
        let mut cognitive_rebases = 0u8;

        let input = input.into();
        let turn_start_index = self.next_event_index().await?;
        let mut turn_tokens = TokenDelta::default();
        self.record_user_message(&input.content).await?;
        self.context.state.task_control.begin_turn();
        // Derive/refresh the task contract from the operator's request (P0-02) —
        // deterministic, before any deliberation, and independent of whether the
        // model volunteers a `set_goal`.
        self.context
            .state
            .task_control
            .set_contract_from_input(&input.content);
        let recall = match self
            .memory
            .recall_for_turn(&mut self.context, &input.content)
            .await
        {
            Ok(recall) => recall,
            Err(error) => return self.stop_for_error(error, 0, sink),
        };
        for event in recall.events {
            sink.emit_yield(&self.context.session_id, event);
        }

        let mut iterations = 0;
        let mut auto_compacted_this_turn = false;
        loop {
            let middlewares = self.context.middlewares.clone();
            for mw in &middlewares {
                mw.before_step(&mut self.context).await?;
            }

            // Cooperative cancellation: if the caller (e.g. the TUI's busy-loop key
            // dispatch on Esc/Ctrl+C) asked to interrupt, stop at this iteration
            // boundary. Any completed steps are already persisted; the flag is reset
            // inside the helper so the next turn starts clean.
            if self
                .context
                .cancel
                .swap(false, std::sync::atomic::Ordering::Relaxed)
            {
                return self
                    .finish_turn_interrupted(turn_start_index, &turn_tokens, iterations, sink)
                    .await;
            }

            // Turn deadline (AGT-002): a turn may not run past its configured wall
            // clock. Tool deadlines are already capped by the remaining turn time;
            // here the loop itself stops. Reported via the existing budget-exhaustion
            // outcome so callers need no new variant.
            if self.context.exec.turn_expired() {
                self.context.exec.cancel();
                return self
                    .finish_turn_deadline(turn_start_index, &turn_tokens, iterations, sink)
                    .await;
            }

            // Steering safety point (grok-build interjection): anything the operator
            // typed while the previous step ran is drained HERE, at the iteration
            // boundary, so this iteration's perception/deliberation already sees it.
            // A post-tool-batch drain would be redundant: after a tool batch the loop
            // immediately re-enters and hits this point before the next LLM call.
            if let Err(error) = self.drain_steering(sink).await {
                return self.stop_for_error(error, iterations, sink);
            }

            // Background subagent completions (grok-build backgrounded tasks): same
            // safety point as steering — finished tasks are injected HERE so this
            // iteration's LLM request already carries their results, poll-free.
            if let Err(error) = self.drain_background_tasks(sink).await {
                return self.stop_for_error(error, iterations, sink);
            }

            if let ReflectionOutcome::MaxIterationsReached(reason) =
                self.reflection.assess_iteration_budget(iterations)
            {
                let message = self
                    .supervisor
                    .budget_exhausted_message(&reason, &self.context.state.task_control);
                // Final background-task drain (see drain_background_tasks): folds in
                // tasks that finished during the last LLM call of this turn.
                if let Err(error) = self.drain_background_tasks(sink).await {
                    return self.stop_for_error(error, iterations, sink);
                }
                if let Err(error) = self.review_learning_for_turn(turn_start_index).await {
                    return self.stop_for_error(error, iterations, sink);
                }
                self.record_turn_complete(turn_start_index, &turn_tokens)
                    .await?;
                sink.emit_yield(
                    &self.context.session_id,
                    RuntimeYield::Error {
                        message: message.clone(),
                    },
                );
                let middlewares = self.context.middlewares.clone();
                for mw in &middlewares {
                    mw.after_step(&mut self.context).await?;
                }
                return Ok(TurnOutcome::MaxIterationsReached {
                    message,
                    iterations,
                });
            }

            if !auto_compacted_this_turn {
                match self.maybe_compact().await {
                    Ok(Some(result)) => {
                        auto_compacted_this_turn = true;
                        sink.emit_yield(&self.context.session_id, compaction_event(&result));
                    }
                    Ok(None) => {}
                    Err(error) => return self.stop_for_error(error, iterations, sink),
                }
            }

            let frame = self.perception.perceive(&self.context);
            let deliberation = match self.decide_with_overflow_retry(&frame, sink).await {
                Ok(deliberation) => deliberation,
                Err(error) if error.kind == crate::deliberation::RuntimeErrorKind::Cancelled => {
                    return self
                        .finish_after_llm_interrupt(
                            turn_start_index,
                            &turn_tokens,
                            iterations,
                            sink,
                        )
                        .await;
                }
                Err(error) => return self.stop_for_error(error, iterations, sink),
            };
            iterations += 1;

            let token_delta = match self.apply_usage(&deliberation.response).await {
                Ok(delta) => delta,
                Err(error) => return self.stop_for_error(error, iterations, sink),
            };
            let middlewares = self.context.middlewares.clone();
            for mw in &middlewares {
                mw.on_token_usage(&mut self.context, &token_delta).await?;
            }
            accumulate_tokens(&mut turn_tokens, &token_delta);
            match self.cognitive_snapshot_changed(&deliberation).await {
                Ok(false) => {}
                Ok(true) => {
                    cognitive_rebases = cognitive_rebases.saturating_add(1);
                    holmes_core::metrics::metrics().count("cognition.commit_invalidated");
                    tracing::warn!(
                        event = "CognitiveCommitInvalidated",
                        session_id = %self.context.session_id,
                        started_version = deliberation.trace.started_ledger_version,
                        rebase_attempt = cognitive_rebases,
                        "Ledger changed during the private cognitive loop; discarding Commit"
                    );
                    self.refresh_ledger().await?;
                    if cognitive_rebases > self.context.config.cognition.max_rebases {
                        return self.stop_for_error(
                            RuntimeError::recoverable(
                                "cognitive Commit was repeatedly invalidated by concurrent Ledger changes; no action was executed",
                            ),
                            iterations,
                            sink,
                        );
                    }
                    if let Err(error) = self
                        .inject_supervisor_note(
                            "The case Ledger changed while you were deliberating. Your proposed Commit was discarded before persistence or execution. Re-perceive the refreshed Ledger and submit a new Commit.",
                            sink,
                        )
                        .await
                    {
                        return self.stop_for_error(error, iterations, sink);
                    }
                    continue;
                }
                Err(error) => return self.stop_for_error(error, iterations, sink),
            }
            let response = self.response_for_record(&deliberation.response, &deliberation.parsed);
            self.record_assistant_response(&response).await?;

            // Apply any meta-actions the model attached to this step (set_goal /
            // reflect / deduce) BEFORE the primary decision, so a single message can
            // both record ledger state and act on tools in the same iteration.
            let meta_actions = deliberation.parsed.meta_actions.clone();
            if !meta_actions.is_empty() {
                let executable_calls = match &deliberation.parsed.decision {
                    HolmesDecision::UseTools { calls, .. } => calls.as_slice(),
                    _ => &[],
                };
                if let Err(error) = self
                    .apply_meta_actions(&meta_actions, executable_calls, sink)
                    .await
                {
                    return self.stop_for_error(error, iterations, sink);
                }
            }
            if let Err(error) = self.persist_cognitive_commit(&deliberation).await {
                return self.stop_for_error(error, iterations, sink);
            }
            let deliberation = deliberation.deliberation;

            // Progress signal for supervision (AGT-009): set by the decision arms below
            // when this iteration produced new evidence, a novel successful action, or
            // a goal change. Terminal arms return before the supervisor runs.
            let mut progress_this_iteration = false;

            match deliberation.parsed.decision {
                HolmesDecision::Answer { message } => {
                    let mut content = if message.trim().is_empty() {
                        response.content.clone().unwrap_or_default()
                    } else {
                        message
                    };
                    let middlewares = self.context.middlewares.clone();
                    for mw in &middlewares {
                        mw.on_final_answer(&mut self.context, &mut content).await?;
                    }

                    if self.ledger_requires_structured_finish() {
                        let note = "This case has material Ledger conclusions or unresolved high-priority hypotheses. A plain answer cannot close it. Emit `finish` alone with persisted conclusion_refs and all important remaining_hypothesis_ids, or continue investigating.";
                        if let Err(error) = self.inject_supervisor_note(note, sink).await {
                            return self.stop_for_error(error, iterations, sink);
                        }
                        continue;
                    }

                    // Unified completion gate (P0-02): a plain-text answer is a
                    // terminal outcome too. When the task was deterministically
                    // classified as needing action/verification (standing goal,
                    // derived task contract, or tool activity this session), the
                    // answer passes through the SAME gate as `finish` — the model
                    // can no longer bypass verification by just not calling finish.
                    // Pure chat/information exchanges (none of the three triggers)
                    // are exempt and keep the old fast path.
                    let mut answer_rejected = false;
                    if self.context.state.task_control.completion_gate_required() {
                        match self
                            .run_completion_gate(
                                &content,
                                iterations,
                                &mut verification_failures,
                                sink,
                            )
                            .await
                        {
                            Ok(CompletionGateOutcome::Passed) => {}
                            Ok(CompletionGateOutcome::Exhausted(partial)) => content = partial,
                            Ok(CompletionGateOutcome::Rejected) => answer_rejected = true,
                            Err(error)
                                if error.kind
                                    == crate::deliberation::RuntimeErrorKind::Cancelled =>
                            {
                                return self
                                    .finish_after_llm_interrupt(
                                        turn_start_index,
                                        &turn_tokens,
                                        iterations,
                                        sink,
                                    )
                                    .await;
                            }
                            Err(error) => return self.stop_for_error(error, iterations, sink),
                        }
                    }

                    if !answer_rejected {
                        let event = self.dialogue.format_final_answer(&content);
                        sink.emit_yield(&self.context.session_id, event);
                        // Final background-task drain (see drain_background_tasks): folds
                        // in tasks that finished during the last LLM call of this turn.
                        if let Err(error) = self.drain_background_tasks(sink).await {
                            return self.stop_for_error(error, iterations, sink);
                        }
                        if let Err(error) = self.review_learning_for_turn(turn_start_index).await {
                            return self.stop_for_error(error, iterations, sink);
                        }
                        self.record_turn_complete(turn_start_index, &turn_tokens)
                            .await?;
                        let middlewares = self.context.middlewares.clone();
                        for mw in &middlewares {
                            mw.after_step(&mut self.context).await?;
                        }
                        return Ok(TurnOutcome::FinalAnswer {
                            content: content.trim().to_string(),
                            iterations,
                        });
                    }
                }
                HolmesDecision::Finish {
                    summary,
                    conclusion_refs,
                    remaining_hypothesis_ids,
                } => {
                    let mut content = if summary.trim().is_empty() {
                        response.content.clone().unwrap_or_default()
                    } else {
                        summary
                    };
                    let middlewares = self.context.middlewares.clone();
                    for mw in &middlewares {
                        mw.on_final_answer(&mut self.context, &mut content).await?;
                    }

                    // Unified completion gate (P0-02, formerly the Finish-only
                    // verifier from AGT-010): deterministic checks first, a model
                    // review only for semantic items. A rejected finish feeds the
                    // gaps back into the loop instead of completing.
                    let mut finish_rejected = match self
                        .verify_ledger_completion(&conclusion_refs, &remaining_hypothesis_ids)
                    {
                        Ok(()) => false,
                        Err(gaps) => {
                            let note = format!("Ledger completion rejected: {}", gaps.join("; "));
                            if let Err(error) = self.inject_supervisor_note(&note, sink).await {
                                return self.stop_for_error(error, iterations, sink);
                            }
                            true
                        }
                    };
                    if !finish_rejected {
                        match self
                            .run_completion_gate(
                                &content,
                                iterations,
                                &mut verification_failures,
                                sink,
                            )
                            .await
                        {
                            Ok(CompletionGateOutcome::Passed) => {}
                            Ok(CompletionGateOutcome::Exhausted(partial)) => content = partial,
                            Ok(CompletionGateOutcome::Rejected) => finish_rejected = true,
                            Err(error)
                                if error.kind
                                    == crate::deliberation::RuntimeErrorKind::Cancelled =>
                            {
                                return self
                                    .finish_after_llm_interrupt(
                                        turn_start_index,
                                        &turn_tokens,
                                        iterations,
                                        sink,
                                    )
                                    .await;
                            }
                            Err(error) => return self.stop_for_error(error, iterations, sink),
                        }
                    }

                    if !finish_rejected {
                        let event = self.dialogue.format_final_answer(&content);
                        sink.emit_yield(&self.context.session_id, event);
                        // Final background-task drain (see drain_background_tasks): folds
                        // in tasks that finished during the last LLM call of this turn.
                        if let Err(error) = self.drain_background_tasks(sink).await {
                            return self.stop_for_error(error, iterations, sink);
                        }
                        if let Err(error) = self.review_learning_for_turn(turn_start_index).await {
                            return self.stop_for_error(error, iterations, sink);
                        }
                        self.record_turn_complete(turn_start_index, &turn_tokens)
                            .await?;
                        let middlewares = self.context.middlewares.clone();
                        for mw in &middlewares {
                            mw.after_step(&mut self.context).await?;
                        }
                        return Ok(TurnOutcome::FinalAnswer {
                            content: content.trim().to_string(),
                            iterations,
                        });
                    }
                }
                HolmesDecision::AskWatson {
                    question,
                    context,
                    options,
                } => {
                    self.emit_intermediate_content(
                        deliberation.parsed.display_content.as_deref(),
                        sink,
                    );
                    let prompt = format_watson_prompt(&question, context.as_deref(), &options);
                    if let Err(error) = self
                        .record_ask_watson(&question, context.as_deref(), &options)
                        .await
                    {
                        return self.stop_for_error(error, iterations, sink);
                    }
                    sink.emit_yield(
                        &self.context.session_id,
                        RuntimeYield::NeedsUserInput {
                            prompt: prompt.clone(),
                        },
                    );
                    // Final background-task drain (see drain_background_tasks): folds
                    // in tasks that finished during the last LLM call of this turn.
                    if let Err(error) = self.drain_background_tasks(sink).await {
                        return self.stop_for_error(error, iterations, sink);
                    }
                    if let Err(error) = self.review_learning_for_turn(turn_start_index).await {
                        return self.stop_for_error(error, iterations, sink);
                    }
                    self.record_turn_complete(turn_start_index, &turn_tokens)
                        .await?;
                    let middlewares = self.context.middlewares.clone();
                    for mw in &middlewares {
                        mw.after_step(&mut self.context).await?;
                    }
                    return Ok(TurnOutcome::NeedsUser { prompt, iterations });
                }
                HolmesDecision::UseTools { rationale, calls } => {
                    self.emit_intermediate_content(
                        deliberation
                            .parsed
                            .display_content
                            .as_deref()
                            .or(rationale.as_deref()),
                        sink,
                    );

                    if calls.is_empty() {
                        let error = RuntimeError::recoverable(
                            "Holmes decided to use tools but did not provide any tool calls.",
                        );
                        return self.stop_for_error(error, iterations, sink);
                    }

                    let mut calls = calls;
                    let middlewares = self.context.middlewares.clone();
                    for call in &mut calls {
                        let mut args: serde_json::Value =
                            serde_json::from_str(&call.function.arguments)
                                .unwrap_or(serde_json::Value::Null);
                        for mw in &middlewares {
                            mw.before_tool_call(
                                &mut self.context,
                                &mut call.function.name,
                                &mut args,
                            )
                            .await?;
                        }
                        call.function.arguments = args.to_string();
                    }

                    if let Err(message) = self.validate_bound_and_finding_calls(&mut calls) {
                        if let Err(error) = self.inject_supervisor_note(&message, sink).await {
                            return self.stop_for_error(error, iterations, sink);
                        }
                        continue;
                    }
                    if let Err(error) = self.start_bound_experiments(&calls).await {
                        return self.stop_for_error(error, iterations, sink);
                    }

                    let action_batch = match self
                        .action
                        .execute_batch(&mut self.context, &calls, sink)
                        .await
                    {
                        Ok(batch) => batch,
                        Err(error) => return self.stop_for_error(error, iterations, sink),
                    };

                    self.context.session.messages.extend(action_batch.messages);

                    // Feed the supervision control state (AGT-009): every call outcome
                    // is an action record; successful results become typed evidence
                    // records bound to the call id and output hash (P0-02).
                    let mut novel_success = false;
                    for (call, result) in calls.iter().zip(action_batch.results.iter()) {
                        let success = result.is_success();
                        let is_novel = self.context.state.task_control.record_action(
                            &call.function.name,
                            &call.function.arguments,
                            success,
                        );
                        if success {
                            novel_success |= is_novel;
                            self.context.state.task_control.record_tool_evidence(
                                &call.function.name,
                                Some(call.id.as_str()),
                                &call.function.arguments,
                                &result.text_content(),
                            );
                        }
                    }
                    if let Err(error) = self
                        .finalize_bound_experiments(&calls, &action_batch.results)
                        .await
                    {
                        return self.stop_for_error(error, iterations, sink);
                    }

                    let projection = self.evidence.project(&mut self.context);
                    for update in &projection.updates {
                        self.context
                            .state
                            .task_control
                            .record_evidence(update.clone());
                    }
                    progress_this_iteration = novel_success || !projection.updates.is_empty();
                    let memory_projection = match self
                        .memory
                        .remember_observations(&mut self.context, &projection.updates)
                        .await
                    {
                        Ok(projection) => projection,
                        Err(error) => return self.stop_for_error(error, iterations, sink),
                    };
                    for event in projection.events {
                        sink.emit_yield(&self.context.session_id, event);
                    }
                    for event in memory_projection.events {
                        sink.emit_yield(&self.context.session_id, event);
                    }
                }
                HolmesDecision::ProtocolViolation { message } => {
                    // finish/ask_watson mixed with executable tool calls (P0-02):
                    // nothing from that response ran — feed the violation back and
                    // let the model re-emit the parts in separate steps.
                    self.emit_intermediate_content(
                        deliberation.parsed.display_content.as_deref(),
                        sink,
                    );
                    holmes_core::metrics::metrics().count("completion.protocol_violation");
                    tracing::warn!(
                        event = "CompletionProtocolViolation",
                        session_id = %self.context.session_id,
                        violation = %message,
                        "terminal control call mixed with executable tool calls; response rejected"
                    );
                    let note = format!(
                        "Protocol violation: {message}. Nothing in that response was executed. \
                         Re-issue the tool calls on their own first; only call finish (alone, in a \
                         later response) once the work they represent is actually done."
                    );
                    if let Err(error) = self.inject_supervisor_note(&note, sink).await {
                        return self.stop_for_error(error, iterations, sink);
                    }
                }
                HolmesDecision::SetGoal { condition, reason } => {
                    self.emit_intermediate_content(
                        deliberation.parsed.display_content.as_deref(),
                        sink,
                    );
                    if let Err(error) = self.set_runtime_goal(&condition, reason.as_deref()).await {
                        return self.stop_for_error(error, iterations, sink);
                    }
                    sink.emit_yield(
                        &self.context.session_id,
                        RuntimeYield::PlanUpdate {
                            content: format!("Goal set: {condition}"),
                        },
                    );
                    progress_this_iteration = true;
                }
                HolmesDecision::Continue => {
                    self.emit_intermediate_content(
                        deliberation.parsed.display_content.as_deref(),
                        sink,
                    );
                    progress_this_iteration = true;
                }
            }

            // Turn supervision (AGT-009): update progress counters, then check for
            // repeated failing calls and stalled progress. First detections inject
            // feedback and let the loop continue; ignored feedback escalates to a
            // stop with a resumable partial result.
            self.context
                .state
                .task_control
                .advance_iteration(progress_this_iteration);
            self.context.state.task_control.tokens_spent =
                self.context.session.tokens.input + self.context.session.tokens.output;
            match self.supervisor.assess(&self.context.state.task_control) {
                Supervision::Continue => {}
                Supervision::ChangeStrategy { tool, count, .. } => {
                    self.context.state.task_control.strategy_switches += 1;
                    holmes_core::metrics::metrics().count("supervisor.strategy_changed");
                    tracing::info!(
                        event = "StrategyChanged",
                        session_id = %self.context.session_id,
                        tool = %tool,
                        repeat_count = count,
                        strategy_switches = self.context.state.task_control.strategy_switches,
                        "repeated failing tool call; forcing strategy change"
                    );
                    let note = format!(
                        "You have called '{tool}' with equivalent arguments {count} times and every attempt failed. Do not repeat that call. Change strategy: use a different tool, different arguments, or a different hypothesis; if the approach is fundamentally blocked, ask the operator."
                    );
                    if let Err(error) = self.inject_supervisor_note(&note, sink).await {
                        return self.stop_for_error(error, iterations, sink);
                    }
                }
                Supervision::StopForUser { tool, count, .. } => {
                    holmes_core::metrics::metrics().count("supervisor.stop_for_user");
                    tracing::warn!(
                        event = "StrategyChanged",
                        session_id = %self.context.session_id,
                        tool = %tool,
                        repeat_count = count,
                        outcome = "escalated_to_user",
                        "failing tool call repeated after a strategy change; stopping turn"
                    );
                    let reason = format!(
                        "tool '{tool}' failed {count} times with identical arguments and the same call was repeated after a strategy change was requested; stopping so the operator can redirect"
                    );
                    let prompt = self
                        .supervisor
                        .budget_exhausted_message(&reason, &self.context.state.task_control);
                    if let Err(error) = self.drain_background_tasks(sink).await {
                        return self.stop_for_error(error, iterations, sink);
                    }
                    if let Err(error) = self.review_learning_for_turn(turn_start_index).await {
                        return self.stop_for_error(error, iterations, sink);
                    }
                    self.record_turn_complete(turn_start_index, &turn_tokens)
                        .await?;
                    sink.emit_yield(
                        &self.context.session_id,
                        RuntimeYield::NeedsUserInput {
                            prompt: prompt.clone(),
                        },
                    );
                    let middlewares = self.context.middlewares.clone();
                    for mw in &middlewares {
                        mw.after_step(&mut self.context).await?;
                    }
                    return Ok(TurnOutcome::NeedsUser { prompt, iterations });
                }
                Supervision::Stagnation {
                    iterations_without_progress,
                } => {
                    let metrics = holmes_core::metrics::metrics();
                    metrics.count("supervisor.stagnation_detected");
                    metrics.record_ms(
                        "supervisor.iterations_without_progress",
                        iterations_without_progress as u64,
                    );
                    tracing::warn!(
                        event = "StagnationDetected",
                        session_id = %self.context.session_id,
                        iterations_without_progress,
                        "no progress; injecting reflection prompt"
                    );
                    let note = format!(
                        "No progress for {iterations_without_progress} iterations (no new evidence, no novel successful action, no goal change). Reflect before acting: what is proven so far, what is the current hypothesis, and what single action would produce the most new information? Consider narrowing the goal, switching tools, or asking the operator."
                    );
                    if let Err(error) = self.inject_supervisor_note(&note, sink).await {
                        return self.stop_for_error(error, iterations, sink);
                    }
                }
                Supervision::StagnationStop {
                    iterations_without_progress,
                } => {
                    let metrics = holmes_core::metrics::metrics();
                    metrics.count("supervisor.stagnation_detected");
                    metrics.count("supervisor.stagnation_stop");
                    metrics.record_ms(
                        "supervisor.iterations_without_progress",
                        iterations_without_progress as u64,
                    );
                    tracing::warn!(
                        event = "StagnationDetected",
                        session_id = %self.context.session_id,
                        iterations_without_progress,
                        escalated = true,
                        "stagnation persisted after a reflection prompt; stopping turn"
                    );
                    let reason = format!(
                        "no progress for {iterations_without_progress} consecutive iterations, persisting after a reflection prompt; stopping with partial progress preserved"
                    );
                    let prompt = self
                        .supervisor
                        .budget_exhausted_message(&reason, &self.context.state.task_control);
                    if let Err(error) = self.drain_background_tasks(sink).await {
                        return self.stop_for_error(error, iterations, sink);
                    }
                    if let Err(error) = self.review_learning_for_turn(turn_start_index).await {
                        return self.stop_for_error(error, iterations, sink);
                    }
                    self.record_turn_complete(turn_start_index, &turn_tokens)
                        .await?;
                    sink.emit_yield(
                        &self.context.session_id,
                        RuntimeYield::NeedsUserInput {
                            prompt: prompt.clone(),
                        },
                    );
                    let middlewares = self.context.middlewares.clone();
                    for mw in &middlewares {
                        mw.after_step(&mut self.context).await?;
                    }
                    return Ok(TurnOutcome::NeedsUser { prompt, iterations });
                }
            }

            let middlewares = self.context.middlewares.clone();
            for mw in &middlewares {
                mw.after_step(&mut self.context).await?;
            }
        }
    }

    /// Inject supervisor feedback (strategy nudge, reflection prompt, verification
    /// gaps) into the conversation as a wrapped user message so the very next
    /// deliberation sees it, and surface a one-line notice to the operator.
    async fn inject_supervisor_note(
        &mut self,
        note: &str,
        sink: &mut dyn RuntimeSink,
    ) -> Result<(), RuntimeError> {
        let wrapped = format!(
            "The turn supervisor intervened:\n<supervisor_note>\n{note}\n</supervisor_note>"
        );
        self.record_user_message(&wrapped).await?;
        sink.emit_yield(
            &self.context.session_id,
            RuntimeYield::PlanUpdate {
                content: note.to_string(),
            },
        );
        Ok(())
    }

    /// Apply non-terminal goal/Ledger meta-actions before executable calls. All
    /// Ledger events from one model response are validated against one snapshot
    /// and appended atomically with a stable command receipt.
    async fn apply_meta_actions(
        &mut self,
        metas: &[crate::decision::MetaAction],
        executable_calls: &[holmes_core::ToolCall],
        sink: &mut dyn RuntimeSink,
    ) -> Result<(), RuntimeError> {
        use crate::decision::MetaAction;
        for meta in metas {
            match meta {
                MetaAction::SetGoal { condition, reason } => {
                    self.set_runtime_goal(condition, reason.as_deref()).await?;
                    sink.emit_yield(
                        &self.context.session_id,
                        RuntimeYield::PlanUpdate {
                            content: format!("Goal set: {condition}"),
                        },
                    );
                }
                MetaAction::ProposeHypothesis(_)
                | MetaAction::PlanExperiment(_)
                | MetaAction::LinkEvidence(_)
                | MetaAction::RequestResolution(_) => {}
            }
        }

        let snapshot = self.context.state.ledger.clone().ok_or_else(|| {
            RuntimeError::recoverable("case Ledger was not loaded before meta-action commit")
        })?;
        let resolution_reviews = crate::ledger::resolution_verifier::review_resolution_requests(
            &self.context,
            &snapshot,
            metas,
        )
        .await;
        let plan = crate::ledger::commit::assemble_meta_commit(
            &snapshot,
            metas,
            executable_calls,
            &self.context.session_id,
            &resolution_reviews,
        )
        .map_err(|error| RuntimeError::recoverable(error.to_string()))?;
        if let Some(plan) = plan {
            let result = self
                .context
                .session_db
                .append(
                    &snapshot.case_id,
                    plan.expected_version,
                    &format!("ledger-commit-{}", uuid::Uuid::new_v4()),
                    plan.events,
                )
                .await
                .map_err(|error| {
                    RuntimeError::recoverable(format!("Ledger commit rejected: {error}"))
                })?;
            for (call_id, mut binding) in plan.bindings {
                if let Some(call) = executable_calls.iter().find(|call| call.id == call_id) {
                    let preview = self.context.state.task_control.preview_tool_evidence(
                        &call.function.name,
                        Some(&call.id),
                        &call.function.arguments,
                        "",
                    );
                    binding.contract_id = preview.contract_id;
                    binding.requirement_ids = preview.requirement_ids;
                }
                self.context.state.action_bindings.insert(call_id, binding);
            }
            for summary in plan.summaries {
                sink.emit_yield(
                    &self.context.session_id,
                    RuntimeYield::PlanUpdate { content: summary },
                );
            }
            self.refresh_ledger().await?;
            debug_assert_eq!(
                self.context
                    .state
                    .ledger
                    .as_ref()
                    .map(|ledger| ledger.version),
                Some(result.version)
            );
        }
        Ok(())
    }

    async fn refresh_ledger(&mut self) -> Result<(), RuntimeError> {
        let case_id = self
            .context
            .session_db
            .case_id_for_session(&self.context.session_id)
            .await
            .map_err(|error| {
                RuntimeError::recoverable(format!("failed to resolve case: {error}"))
            })?;
        let snapshot = self
            .context
            .session_db
            .load(&case_id)
            .await
            .map_err(|error| {
                RuntimeError::recoverable(format!("failed to replay Ledger: {error}"))
            })?;
        if let Err(error) = self
            .context
            .session_db
            .compact_snapshot(&case_id, self.context.config.ledger.snapshot_every_events)
            .await
        {
            // Snapshotting is an optimization. The verified event replay above
            // remains authoritative, so a compaction failure must not stop the turn.
            tracing::warn!(case_id = %case_id, error = %error, "Ledger snapshot compaction failed");
            holmes_core::metrics::metrics().count("ledger.snapshot_write_failed");
        }
        self.context.state.ledger = Some(snapshot);
        Ok(())
    }

    async fn cognitive_snapshot_changed(
        &self,
        result: &CognitiveResult,
    ) -> Result<bool, RuntimeError> {
        let case_id = self
            .context
            .state
            .ledger
            .as_ref()
            .map(|ledger| ledger.case_id.clone())
            .ok_or_else(|| RuntimeError::recoverable("case Ledger is unavailable"))?;
        let current = self
            .context
            .session_db
            .load(&case_id)
            .await
            .map_err(|error| {
                RuntimeError::recoverable(format!(
                    "failed to validate cognitive snapshot version: {error}"
                ))
            })?;
        Ok(current.version != result.trace.started_ledger_version)
    }

    /// Persist the public result of a Cognitive Loop. Proposal/Critique bodies
    /// never enter this method; only bounded IDs, fixed issue codes and the final
    /// native operation are reduced into `DeliberationCommittedV2`.
    async fn persist_cognitive_commit(
        &mut self,
        result: &CognitiveResult,
    ) -> Result<(), RuntimeError> {
        use holmes_core::ledger::{
            AggregateKind, CallBinding, CommitOperation, DeliberationCommit, LedgerEvent, Ordinal,
            RiskLevel, UnstoredLedgerEvent, LEDGER_EVENT_SCHEMA_VERSION,
        };
        use holmes_tools::registry::Effect;

        if !self.context.config.cognition.enabled
            || !self.context.config.cognition.persist_commit_summary
            || matches!(
                result.parsed.decision,
                HolmesDecision::ProtocolViolation { .. }
            )
        {
            return Ok(());
        }
        let snapshot = self.context.state.ledger.as_ref().ok_or_else(|| {
            RuntimeError::recoverable("cannot persist cognitive commit without a case Ledger")
        })?;
        let has_ledger_meta = result
            .parsed
            .meta_actions
            .iter()
            .any(|meta| !matches!(meta, crate::decision::MetaAction::SetGoal { .. }));
        let selected_operation = match &result.parsed.decision {
            HolmesDecision::Answer { .. } => CommitOperation::Answer,
            HolmesDecision::AskWatson { .. } => CommitOperation::AskWatson,
            HolmesDecision::Finish { .. } => CommitOperation::Finish,
            HolmesDecision::UseTools { .. } if has_ledger_meta => CommitOperation::LedgerAndTools,
            HolmesDecision::UseTools { .. } => CommitOperation::ExecuteTools,
            HolmesDecision::Continue | HolmesDecision::SetGoal { .. } => {
                CommitOperation::LedgerOnly
            }
            HolmesDecision::ProtocolViolation { .. } => CommitOperation::Answer,
        };
        let risk = result
            .parsed
            .meta_actions
            .iter()
            .filter_map(|meta| match meta {
                crate::decision::MetaAction::PlanExperiment(proposal) => {
                    Some(proposal.risk.clone())
                }
                _ => None,
            })
            .max_by_key(risk_rank)
            .or_else(|| match &result.parsed.decision {
                HolmesDecision::UseTools { calls, .. }
                    if calls
                        .iter()
                        .any(|call| self.context.tools.effect_of(call) == Effect::Mutating) =>
                {
                    Some(RiskLevel::High)
                }
                HolmesDecision::UseTools { .. } => Some(RiskLevel::Low),
                _ => None,
            })
            .unwrap_or(RiskLevel::Low);
        let expected_information_gain = if result
            .parsed
            .meta_actions
            .iter()
            .any(|meta| matches!(meta, crate::decision::MetaAction::PlanExperiment(_)))
        {
            Ordinal::High
        } else if matches!(result.parsed.decision, HolmesDecision::UseTools { .. }) {
            Ordinal::Medium
        } else {
            Ordinal::Low
        };
        let public_rationale = result
            .parsed
            .display_content
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(|value| value.chars().take(800).collect())
            .unwrap_or_else(|| {
                format!(
                    "Validated {:?} Commit after {} bounded cognitive pass(es); critique codes: {}",
                    selected_operation,
                    result.trace.pass_count,
                    result
                        .trace
                        .critique_codes
                        .iter()
                        .map(|code| format!("{code:?}"))
                        .collect::<Vec<_>>()
                        .join(",")
                )
            });
        let executable_call_bindings = self
            .context
            .state
            .action_bindings
            .values()
            .filter_map(|binding| {
                binding
                    .experiment_id
                    .clone()
                    .map(|experiment_id| CallBinding {
                        tool_call_id: binding.tool_call_id.clone(),
                        experiment_id,
                    })
            })
            .collect();
        let id = format!("delib-{}", uuid::Uuid::new_v4());
        let commit = DeliberationCommit {
            id: id.clone(),
            case_id: snapshot.case_id.clone(),
            session_id: self.context.session_id.clone(),
            ledger_version: snapshot.version,
            mode: result.trace.mode.clone(),
            considered_hypothesis_ids: snapshot
                .hypotheses
                .keys()
                .take(self.context.config.ledger.max_active_hypotheses_in_context)
                .cloned()
                .collect(),
            selected_operation,
            public_rationale,
            expected_information_gain,
            risk,
            executable_call_bindings,
            created_at: Utc::now(),
        };
        let event = UnstoredLedgerEvent {
            event_id: format!("event-{}", uuid::Uuid::new_v4()),
            aggregate_kind: AggregateKind::Deliberation,
            aggregate_id: id.clone(),
            aggregate_revision: 1,
            actor_session_id: self.context.session_id.clone(),
            event: LedgerEvent::DeliberationCommittedV2 {
                schema_version: LEDGER_EVENT_SCHEMA_VERSION,
                commit,
            },
            created_at: Utc::now(),
        };
        self.context
            .session_db
            .append(
                &snapshot.case_id,
                snapshot.version,
                &format!("cognitive-commit-{id}"),
                vec![event],
            )
            .await
            .map_err(|error| {
                RuntimeError::recoverable(format!(
                    "cognitive Commit was invalidated by a Ledger change: {error}"
                ))
            })?;
        holmes_core::metrics::metrics().count("cognition.commit_validated");
        tracing::info!(
            event = "CognitiveCommitValidated",
            session_id = %self.context.session_id,
            commit_id = %id,
            mode = ?result.trace.mode,
            pass_count = result.trace.pass_count,
            "public cognitive Commit persisted"
        );
        self.refresh_ledger().await
    }

    fn verify_ledger_completion(
        &self,
        conclusion_refs: &[String],
        remaining_hypothesis_ids: &[holmes_core::ledger::HypothesisId],
    ) -> Result<(), Vec<String>> {
        use holmes_core::ledger::{HypothesisStatus, Priority, ResolutionId, ResolvedStatus};
        let Some(ledger) = self.context.state.ledger.as_ref() else {
            return Err(vec!["case Ledger is unavailable".into()]);
        };
        let mut gaps = Vec::new();
        let remaining: std::collections::BTreeSet<_> =
            remaining_hypothesis_ids.iter().cloned().collect();
        for id in &remaining {
            match ledger.hypotheses.get(id) {
                Some(hypothesis)
                    if matches!(
                        hypothesis.status,
                        HypothesisStatus::Open | HypothesisStatus::Inconclusive
                    ) => {}
                Some(hypothesis) => gaps.push(format!(
                    "remaining hypothesis {id} is {:?}, not Open/Inconclusive",
                    hypothesis.status
                )),
                None => gaps.push(format!("unknown remaining hypothesis {id}")),
            }
        }

        let mut important_remaining = std::collections::BTreeSet::new();
        for hypothesis in ledger.hypotheses.values() {
            if matches!(
                hypothesis.status,
                HypothesisStatus::Open | HypothesisStatus::Inconclusive
            ) && matches!(hypothesis.priority, Priority::High | Priority::Critical)
            {
                important_remaining.insert(hypothesis.id.clone());
            }
        }
        for contradiction in ledger.contradictions.values() {
            important_remaining.insert(contradiction.hypothesis_id.clone());
        }
        for undisclosed in important_remaining.difference(&remaining) {
            gaps.push(format!(
                "important unresolved hypothesis {undisclosed} was not disclosed"
            ));
        }

        let has_strong_resolution = ledger.resolutions.values().any(|resolution| {
            matches!(
                resolution.status,
                ResolvedStatus::Confirmed | ResolvedStatus::Rejected
            )
        });
        if has_strong_resolution && conclusion_refs.is_empty() {
            gaps.push("final conclusions must cite persisted Resolution IDs".into());
        }
        for reference in conclusion_refs {
            let id = ResolutionId::new(reference);
            let Some(resolution) = ledger.resolutions.get(&id) else {
                gaps.push(format!("unknown conclusion Resolution {reference}"));
                continue;
            };
            if !matches!(
                resolution.status,
                ResolvedStatus::Confirmed | ResolvedStatus::Rejected
            ) {
                gaps.push(format!(
                    "Resolution {reference} is {:?} and cannot support a strong conclusion",
                    resolution.status
                ));
            }
            if ledger
                .contradictions
                .values()
                .any(|conflict| conflict.hypothesis_id == resolution.hypothesis_id)
            {
                gaps.push(format!(
                    "Resolution {reference} has an unresolved contradiction"
                ));
            }
        }
        if gaps.is_empty() {
            Ok(())
        } else {
            Err(gaps)
        }
    }

    fn ledger_requires_structured_finish(&self) -> bool {
        use holmes_core::ledger::{HypothesisStatus, Priority};
        self.context.state.ledger.as_ref().is_some_and(|ledger| {
            !ledger.resolutions.is_empty()
                || !ledger.contradictions.is_empty()
                || ledger.hypotheses.values().any(|hypothesis| {
                    matches!(hypothesis.priority, Priority::High | Priority::Critical)
                        && matches!(
                            hypothesis.status,
                            HypothesisStatus::Open | HypothesisStatus::Inconclusive
                        )
                })
        })
    }

    fn validate_bound_and_finding_calls(
        &mut self,
        calls: &mut [holmes_core::ToolCall],
    ) -> Result<(), String> {
        use holmes_core::ledger::ResolvedStatus;
        let assigned = self.context.state.assigned_experiment.clone();
        let ledger = self
            .context
            .state
            .ledger
            .as_ref()
            .ok_or_else(|| "case Ledger is unavailable".to_string())?;
        if let Some(assignment) = &assigned {
            if assignment.case_id != ledger.case_id {
                return Err("assigned Experiment belongs to a different case".into());
            }
            let experiment = ledger
                .experiments
                .get(&assignment.experiment_id)
                .ok_or_else(|| {
                    format!(
                        "assigned experiment {} no longer exists",
                        assignment.experiment_id
                    )
                })?;
            if experiment.status != holmes_core::ledger::ExperimentStatus::Running
                || experiment.attempt == 0
            {
                return Err(format!(
                    "assigned experiment {} is {:?}, expected Running",
                    assignment.experiment_id, experiment.status
                ));
            }
            let bindings = calls
                .iter()
                .filter(|call| !self.context.state.action_bindings.contains_key(&call.id))
                .map(|call| {
                    (
                        call.id.clone(),
                        holmes_core::ledger::ActionBinding {
                            case_id: ledger.case_id.clone(),
                            contract_id: None,
                            requirement_ids: Vec::new(),
                            experiment_id: Some(experiment.id.clone()),
                            prediction_ids: experiment.prediction_ids.clone(),
                            tool_call_id: call.id.clone(),
                            attempt: experiment.attempt,
                        },
                    )
                })
                .collect::<Vec<_>>();
            for (call_id, binding) in bindings {
                self.context.state.action_bindings.insert(call_id, binding);
            }
        }
        let ledger = self
            .context
            .state
            .ledger
            .as_ref()
            .expect("Ledger remains loaded after assignment binding");
        for call in calls {
            if call.function.name == "spawn_subagent"
                && self
                    .context
                    .state
                    .action_bindings
                    .get(&call.id)
                    .and_then(|binding| binding.experiment_id.as_ref())
                    .is_none()
            {
                let mut task: holmes_core::SubAgentTask =
                    serde_json::from_str(&call.function.arguments)
                        .map_err(|error| format!("invalid spawn_subagent arguments: {error}"))?;
                // The private assignment channel is Runtime-owned. Strip any
                // model-supplied value when no validated Commit binding exists.
                task.ledger_assignment = None;
                call.function.arguments = serde_json::to_string(&task)
                    .map_err(|error| format!("cannot sanitize spawn_subagent: {error}"))?;
            }
            if let Some(binding) = self.context.state.action_bindings.get(&call.id) {
                if let Some(experiment_id) = &binding.experiment_id {
                    let experiment = ledger.experiments.get(experiment_id).ok_or_else(|| {
                        format!("bound experiment {experiment_id} no longer exists")
                    })?;
                    if call.function.name == "spawn_subagent" {
                        let mut task: holmes_core::SubAgentTask =
                            serde_json::from_str(&call.function.arguments).map_err(|error| {
                                format!(
                                    "spawn_subagent bound to {experiment_id} has invalid arguments: {error}"
                                )
                            })?;
                        let normalized = |values: &[String]| {
                            let mut values = values
                                .iter()
                                .map(|value| value.trim().to_owned())
                                .filter(|value| !value.is_empty())
                                .collect::<Vec<_>>();
                            values.sort();
                            values.dedup();
                            values
                        };
                        if normalized(&task.constraints.tools_allowlist)
                            != normalized(&experiment.tool_allowlist)
                        {
                            return Err(format!(
                                "spawn_subagent child tools_allowlist does not match experiment {experiment_id}"
                            ));
                        }
                        let safe_to_retry =
                            experiment.risk == holmes_core::ledger::RiskLevel::Low
                                && task.constraints.tools_allowlist.iter().all(|tool| {
                                    self.context.tools.is_read_only(tool) == Some(true)
                                });
                        task.ledger_assignment = Some(holmes_core::ledger::ExperimentAssignment {
                            case_id: ledger.case_id.clone(),
                            experiment_id: experiment_id.clone(),
                            expected_revision: experiment.revision,
                            max_concurrent_per_case: self
                                .context
                                .config
                                .experiments
                                .max_concurrent_per_case,
                            lease_ms: self.context.config.experiments.default_lease_ms,
                            safe_to_retry,
                        });
                        call.function.arguments =
                            serde_json::to_string(&task).map_err(|error| {
                                format!("cannot bind delegated experiment: {error}")
                            })?;
                    } else if !experiment
                        .tool_allowlist
                        .iter()
                        .any(|allowed| allowed == &call.function.name)
                    {
                        return Err(format!(
                            "Ledger commit invalidated: tool {} is outside experiment {} allowlist",
                            call.function.name, experiment_id
                        ));
                    }
                }
            }
            if call.function.name != "report_finding" {
                continue;
            }
            let mut args: serde_json::Value = serde_json::from_str(&call.function.arguments)
                .map_err(|error| format!("invalid report_finding arguments: {error}"))?;
            let confidence = args
                .get("confidence")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("possible");
            let resolution_ids: Vec<String> = args
                .get("resolution_ids")
                .and_then(serde_json::Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .map(ToOwned::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            let required = match confidence {
                "confirmed" => Some(ResolvedStatus::Confirmed),
                "not_vulnerable" | "rejected" | "ruled_out" => Some(ResolvedStatus::Rejected),
                _ => None,
            };
            if let Some(required) = &required {
                if resolution_ids.is_empty() {
                    return Err(format!(
                        "report_finding confidence '{confidence}' requires at least one persisted {:?} Resolution ID",
                        required
                    ));
                }
                for id in &resolution_ids {
                    let resolution_id = holmes_core::ledger::ResolutionId::new(id.clone());
                    let resolution = ledger.resolutions.get(&resolution_id).ok_or_else(|| {
                        format!("report_finding references unknown Resolution {id}")
                    })?;
                    if &resolution.status != required {
                        return Err(format!(
                            "Resolution {id} is {:?}, expected {:?}",
                            resolution.status, required
                        ));
                    }
                    if ledger.contradictions.values().any(|contradiction| {
                        contradiction.hypothesis_id == resolution.hypothesis_id
                    }) {
                        return Err(format!(
                            "Resolution {id} has an unresolved contradiction and cannot back a finding"
                        ));
                    }
                }
            }
            if self
                .context
                .state
                .compatibility_state
                .bounty
                .program
                .is_some()
            {
                let asset = args
                    .get("affected_asset")
                    .and_then(serde_json::Value::as_str)
                    .or_else(|| args.get("location").and_then(serde_json::Value::as_str));
                let status_of = |id: &str| -> Option<String> {
                    let resolution_id = holmes_core::ledger::ResolutionId::new(id.to_string());
                    ledger.resolutions.get(&resolution_id).map(|resolution| {
                        match &resolution.status {
                            ResolvedStatus::Confirmed => "confirmed".to_string(),
                            ResolvedStatus::Rejected => "rejected".to_string(),
                            other => format!("{other:?}").to_lowercase(),
                        }
                    })
                };
                if let Err(err) = holmes_core::bounty::gate_finding(
                    self.context
                        .state
                        .compatibility_state
                        .bounty
                        .program
                        .as_ref(),
                    &resolution_ids,
                    status_of,
                    asset,
                ) {
                    return Err(err.to_string());
                }
            }
            let validation = match required {
                Some(ResolvedStatus::Confirmed) => "confirmed",
                Some(ResolvedStatus::Rejected) => "rejected",
                _ => "candidate",
            };
            let object = args
                .as_object_mut()
                .ok_or_else(|| "report_finding arguments must be a JSON object".to_string())?;
            // Execution-boundary attestation. SkepticGate trusts this Runtime
            // marker, not the model's raw confidence string. Any model-supplied
            // value is overwritten after Resolution validation.
            object.insert(
                "_ledger_validation".into(),
                serde_json::json!({
                    "status": validation,
                    "resolution_ids": resolution_ids,
                }),
            );
            call.function.arguments = serde_json::to_string(&args)
                .map_err(|error| format!("cannot attest report_finding: {error}"))?;
        }
        Ok(())
    }

    async fn start_bound_experiments(
        &mut self,
        calls: &[holmes_core::ToolCall],
    ) -> Result<(), RuntimeError> {
        use holmes_core::ledger::{
            AggregateKind, ExperimentStatus, LedgerEvent, UnstoredLedgerEvent,
            LEDGER_EVENT_SCHEMA_VERSION,
        };
        let Some(snapshot) = self.context.state.ledger.as_ref() else {
            return Ok(());
        };
        let mut ids = std::collections::BTreeSet::new();
        for call in calls {
            if let Some(id) = self
                .context
                .state
                .action_bindings
                .get(&call.id)
                .and_then(|binding| binding.experiment_id.clone())
            {
                ids.insert(id);
            }
        }
        let now = Utc::now();
        let mut events = Vec::new();
        for id in ids {
            if self
                .context
                .state
                .assigned_experiment
                .as_ref()
                .is_some_and(|assignment| assignment.experiment_id == id)
            {
                continue;
            }
            if calls.iter().any(|call| {
                call.function.name == "spawn_subagent"
                    && self
                        .context
                        .state
                        .action_bindings
                        .get(&call.id)
                        .and_then(|binding| binding.experiment_id.as_ref())
                        == Some(&id)
            }) {
                // Delegated Experiments are queued and started atomically by
                // the durable task sink, which owns their lease/fencing token.
                continue;
            }
            let experiment = snapshot.experiments.get(&id).ok_or_else(|| {
                RuntimeError::recoverable(format!("bound experiment {id} disappeared"))
            })?;
            if experiment.status != ExperimentStatus::Planned {
                return Err(RuntimeError::recoverable(format!(
                    "experiment {id} is {:?}, expected Planned",
                    experiment.status
                )));
            }
            events.push(UnstoredLedgerEvent {
                event_id: format!("event-{}", uuid::Uuid::new_v4()),
                aggregate_kind: AggregateKind::Experiment,
                aggregate_id: id.to_string(),
                aggregate_revision: experiment.revision + 1,
                actor_session_id: self.context.session_id.clone(),
                event: LedgerEvent::ExperimentStartedV2 {
                    schema_version: LEDGER_EVENT_SCHEMA_VERSION,
                    experiment_id: id.clone(),
                    expected_revision: experiment.revision,
                    attempt: experiment.attempt.saturating_add(1),
                    occurred_at: now,
                },
                created_at: now,
            });
            for binding in self.context.state.action_bindings.values_mut() {
                if binding.experiment_id.as_ref() == Some(&id) {
                    binding.attempt = experiment.attempt.saturating_add(1);
                }
            }
        }
        if events.is_empty() {
            return Ok(());
        }
        self.context
            .session_db
            .append(
                &snapshot.case_id,
                snapshot.version,
                &format!("experiment-start-{}", uuid::Uuid::new_v4()),
                events,
            )
            .await
            .map_err(|error| {
                RuntimeError::recoverable(format!("failed to start Ledger experiment: {error}"))
            })?;
        self.refresh_ledger().await
    }

    async fn finalize_bound_experiments(
        &mut self,
        calls: &[holmes_core::ToolCall],
        results: &[holmes_core::ToolResult],
    ) -> Result<(), RuntimeError> {
        use holmes_core::ledger::{
            AggregateKind, ExperimentStatus, LedgerEvent, UnstoredLedgerEvent,
            LEDGER_EVENT_SCHEMA_VERSION,
        };
        self.refresh_ledger().await?;
        let Some(snapshot) = self.context.state.ledger.as_ref() else {
            return Ok(());
        };
        let mut grouped: std::collections::BTreeMap<
            holmes_core::ledger::ExperimentId,
            Vec<(&holmes_core::ToolCall, &holmes_core::ToolResult)>,
        > = std::collections::BTreeMap::new();
        for (call, result) in calls.iter().zip(results) {
            if self
                .context
                .state
                .assigned_experiment
                .as_ref()
                .and_then(|assignment| {
                    self.context
                        .state
                        .action_bindings
                        .get(&call.id)
                        .and_then(|binding| binding.experiment_id.as_ref())
                        .map(|id| id == &assignment.experiment_id)
                })
                == Some(true)
            {
                continue;
            }
            if call.function.name == "spawn_subagent" {
                // The durable Experiment task finalizes only when the child
                // result arrives, not when this orchestration call returns.
                continue;
            }
            if let Some(id) = self
                .context
                .state
                .action_bindings
                .get(&call.id)
                .and_then(|binding| binding.experiment_id.clone())
            {
                grouped.entry(id).or_default().push((call, result));
            }
        }
        let now = Utc::now();
        let mut events = Vec::new();
        for (id, outcomes) in grouped {
            let experiment = snapshot.experiments.get(&id).ok_or_else(|| {
                RuntimeError::recoverable(format!("running experiment {id} disappeared"))
            })?;
            if experiment.status != ExperimentStatus::Running {
                return Err(RuntimeError::recoverable(format!(
                    "experiment {id} is {:?}, expected Running",
                    experiment.status
                )));
            }
            let evidence_ids: Vec<String> = snapshot
                .evidence
                .values()
                .filter(|evidence| {
                    outcomes
                        .iter()
                        .any(|(call, _)| evidence.tool_call_id.as_deref() == Some(call.id.as_str()))
                })
                .map(|evidence| evidence.id.clone())
                .collect();
            let event = if outcomes.iter().any(|(_, result)| result.is_success()) {
                LedgerEvent::ExperimentObservedV2 {
                    schema_version: LEDGER_EVENT_SCHEMA_VERSION,
                    experiment_id: id.clone(),
                    expected_revision: experiment.revision,
                    evidence_ids,
                    occurred_at: now,
                }
            } else if outcomes
                .iter()
                .any(|(_, result)| result.status == holmes_core::ToolOutcomeStatus::Denied)
            {
                LedgerEvent::ExperimentBlockedV2 {
                    schema_version: LEDGER_EVENT_SCHEMA_VERSION,
                    experiment_id: id.clone(),
                    expected_revision: experiment.revision,
                    reason: "execution denied by permission/guard boundary".into(),
                    occurred_at: now,
                }
            } else if outcomes
                .iter()
                .any(|(_, result)| result.status == holmes_core::ToolOutcomeStatus::Cancelled)
            {
                LedgerEvent::ExperimentCancelledV2 {
                    schema_version: LEDGER_EVENT_SCHEMA_VERSION,
                    experiment_id: id.clone(),
                    expected_revision: experiment.revision,
                    reason: "execution cancelled".into(),
                    occurred_at: now,
                }
            } else {
                LedgerEvent::ExperimentFailedV2 {
                    schema_version: LEDGER_EVENT_SCHEMA_VERSION,
                    experiment_id: id.clone(),
                    expected_revision: experiment.revision,
                    reason: "all bound tool calls failed or timed out".into(),
                    occurred_at: now,
                }
            };
            events.push(UnstoredLedgerEvent {
                event_id: format!("event-{}", uuid::Uuid::new_v4()),
                aggregate_kind: AggregateKind::Experiment,
                aggregate_id: id.to_string(),
                aggregate_revision: experiment.revision + 1,
                actor_session_id: self.context.session_id.clone(),
                event,
                created_at: now,
            });
        }
        if events.is_empty() {
            return Ok(());
        }
        self.context
            .session_db
            .append(
                &snapshot.case_id,
                snapshot.version,
                &format!("experiment-finish-{}", uuid::Uuid::new_v4()),
                events,
            )
            .await
            .map_err(|error| {
                RuntimeError::recoverable(format!("failed to finalize Ledger experiment: {error}"))
            })?;
        self.refresh_ledger().await
    }

    /// Summarize the middle of the transcript with an LLM (compressor role). Returns
    /// `None` (→ static fallback) on empty input or any LLM error, so compaction never
    /// fails because of the summarizer.
    async fn llm_summarize_middle(&self, middle: &[holmes_core::Message]) -> Option<String> {
        summarize_middle_with(&self.context.llm, middle).await
    }

    /// Spawn the background two-pass prefire: a compressor-role summary of the exact
    /// middle span a later compaction would fold away. Only worthwhile in LLM-summary
    /// mode — the static template makes no LLM call, so there is nothing to warm and
    /// scripted harness replays stay untouched. One prefire in flight at a time; the
    /// task owns an Arc'd backend plus a message snapshot, never a borrow of the runtime.
    fn maybe_spawn_prefire(&mut self, plan: &crate::compaction::CompressionPlan) {
        let compressor = &self.context.config.compressor;
        if !compressor.enabled || !compressor.llm_summary {
            return;
        }
        if self.prefire.is_some() {
            return;
        }
        let Some((head, tail)) = plan.archived_message_range else {
            return;
        };
        // Same estimate the compaction trigger uses (char/4 maxed with the real
        // prompt_tokens floor); only the window edge differs.
        let prefire_floor = (compressor.context_limit as f64
            * (compressor.threshold - PREFIRE_WINDOW).max(0.0)) as u64;
        if plan.estimated_tokens < prefire_floor.max(1) {
            return;
        }
        let messages = &self.context.session.messages;
        let head = head.min(messages.len());
        let tail = tail.min(messages.len());
        if head >= tail {
            return;
        }
        let fingerprint = SpanFingerprint::of(messages, head, tail);
        let middle: Vec<holmes_core::Message> = messages[head..tail].to_vec();
        let llm = self.context.llm.clone();
        let result = std::sync::Arc::new(std::sync::Mutex::new(None));
        let task_result = result.clone();
        let handle = tokio::spawn(async move {
            // Failure is silently dropped: the synchronous path re-runs the summary at
            // compaction time, so a failed prefire only costs the warm-up call.
            if let Some(summary) = summarize_middle_with(&llm, &middle).await {
                if let Ok(mut slot) = task_result.lock() {
                    *slot = Some(PrefireResult {
                        fingerprint,
                        summary,
                    });
                }
            }
        });
        self.prefire = Some(PrefireSlot {
            fingerprint,
            result,
            handle,
        });
    }

    /// Take the prefire summary if its span fingerprint still matches the span about to
    /// be compacted; otherwise discard the result. The in-flight task is always awaited
    /// first — on a match this is the near-zero-latency path (usually already done), and
    /// on a mismatch it guarantees the background task can no longer interleave LLM
    /// calls with the synchronous fallback summary (also what keeps scripted backends
    /// deterministic). The task is a single compressor call, so the wait is bounded by
    /// exactly the work the synchronous path would otherwise redo.
    async fn take_matching_prefire(&mut self, head: usize, tail: usize) -> Option<String> {
        let mut slot = self.prefire.take()?;
        let _ = (&mut slot.handle).await;
        let current = SpanFingerprint::of(&self.context.session.messages, head, tail);
        if slot.fingerprint != current {
            return None;
        }
        let result = slot.result.lock().ok()?.take()?;
        (result.fingerprint == current).then_some(result.summary)
    }

    /// Drop any in-flight prefire at a turn boundary so a stale background summary is
    /// never reused for a span it no longer describes.
    fn discard_prefire(&mut self) {
        self.prefire = None;
    }

    /// Pre-compaction flush: ask the compressor role to extract the case state that must
    /// survive compaction (confirmed findings / access / hypotheses / next steps, in the
    /// spirit of the skeptic gate's four quadrants). Returns `None` on empty input or any
    /// LLM error — best-effort, never blocks compaction.
    async fn pre_compact_flush(&self, middle: &[holmes_core::Message]) -> Option<String> {
        let transcript = middle_transcript(middle);
        if transcript.trim().is_empty() {
            return None;
        }
        let prompt = format!(
            "The middle of an authorized penetration-testing engagement transcript is about \
             to be compacted away. Extract the critical state that MUST survive, as terse \
             markdown notes with exactly these sections (omit any section with nothing \
             substantive):\n\
             - ## Confirmed findings — verified vulnerabilities/facts with evidence and location\n\
             - ## Access & credentials — credentials, tokens, sessions, or access levels obtained\n\
             - ## Leads & hypotheses — promising endpoints/attack surface and current working hypotheses\n\
             - ## Ruled out & next steps — eliminated dead ends and the immediate planned actions\n\
             \nTranscript:\n\n{transcript}"
        );
        let messages = vec![holmes_core::Message::user(prompt)];
        match self
            .context
            .llm
            .chat_completion(&messages, &[], "compressor")
            .await
        {
            Ok(response) => response.content.filter(|c| !c.trim().is_empty()),
            Err(_) => None,
        }
    }

    async fn decide_with_overflow_retry(
        &mut self,
        frame: &crate::perception::PerceptionFrame,
        sink: &mut dyn RuntimeSink,
    ) -> Result<CognitiveResult, RuntimeError> {
        use crate::deliberation::RuntimeErrorKind::ContextOverflow;

        // First attempt with the given frame. Stream assistant text deltas to the sink as they
        // arrive (visible when `llm.stream` is on) so the UI can render the answer live. The
        // rare overflow-recovery retries below stay non-streaming.
        let mut overflow_msg = {
            let session_id = self.context.session_id.clone();
            let mut on_text = |delta: &str| {
                sink.emit_yield(
                    &session_id,
                    RuntimeYield::TextDelta {
                        content: delta.to_string(),
                    },
                );
            };
            // P1-01: the deliberation races the turn token/deadline, so Esc or turn
            // expiry ends the in-flight LLM call instead of waiting it out.
            match self
                .interruptible_llm(self.cognition.deliberate_streaming(
                    &self.deliberation,
                    &self.context,
                    frame,
                    &mut on_text,
                ))
                .await
            {
                Ok(Ok(result)) => return Ok(result),
                Ok(Err(error)) if error.kind == ContextOverflow => error.message,
                Ok(Err(error)) => return Err(error),
                Err(cancelled) => return Err(cancelled),
            }
        };

        // Recovery: force-compact and retry, bounded by MAX_OVERFLOW_COMPACTIONS. We retry
        // on any successful compaction (content shrinks even when the message *count* is
        // unchanged, e.g. a one-message middle → summary); a `None` means there's nothing
        // left to compress (the overflow is in the protected head/tail) → emergency path.
        for _ in 0..MAX_OVERFLOW_COMPACTIONS {
            match self
                .compact_with_trigger(true, holmes_core::CompactionTrigger::Overflow)
                .await?
            {
                Some(result) => {
                    sink.emit_yield(&self.context.session_id, compaction_event(&result));
                }
                None => break,
            }
            let retry_frame = self.perception.perceive(&self.context);
            match self
                .interruptible_llm(self.cognition.deliberate_streaming(
                    &self.deliberation,
                    &self.context,
                    &retry_frame,
                    &mut |_| {},
                ))
                .await
            {
                Ok(Ok(result)) => return Ok(result),
                Ok(Err(error)) if error.kind == ContextOverflow => {
                    overflow_msg = error.message;
                    continue;
                }
                Ok(Err(error)) => return Err(error),
                Err(cancelled) => return Err(cancelled),
            }
        }

        // Last resort: a single oversized message (e.g. a huge tool result in the
        // protected tail) that compaction can't shrink. Truncate oversized message
        // *content* — this preserves message count and tool_use/tool_result pairing — and
        // retry once. Better a truncated turn than a hard failure mid-engagement.
        if self.emergency_truncate_messages(EMERGENCY_MESSAGE_CHAR_CAP) {
            let retry_frame = self.perception.perceive(&self.context);
            match self
                .interruptible_llm(self.cognition.deliberate_streaming(
                    &self.deliberation,
                    &self.context,
                    &retry_frame,
                    &mut |_| {},
                ))
                .await
            {
                Ok(Ok(result)) => return Ok(result),
                Ok(Err(_)) => {}
                Err(cancelled) => return Err(cancelled),
            }
        }

        Err(RuntimeError::recoverable(format!(
            "context overflow persists after compaction + emergency truncation: {overflow_msg}"
        )))
    }

    /// Truncate the *content* of any message longer than `cap` chars (leaves message
    /// count and tool_call ids untouched, so tool pairing stays valid). Returns whether
    /// anything was truncated. Used only as an overflow last resort.
    fn emergency_truncate_messages(&mut self, cap: usize) -> bool {
        let mut changed = false;
        for msg in &mut self.context.session.messages {
            if let Some(content) = msg.content.as_mut() {
                if content.chars().count() > cap {
                    let truncated: String = content.chars().take(cap).collect();
                    *content =
                        format!("{truncated}\n[...truncated during context-overflow recovery]");
                    changed = true;
                }
            }
        }
        changed
    }

    fn emit_intermediate_content(&self, content: Option<&str>, sink: &mut dyn RuntimeSink) {
        if let Some(content) = content {
            if let Some(event) = DialogueEngine::message_to_user(content) {
                sink.emit_yield(&self.context.session_id, event);
            }
        }
    }

    fn response_for_record(
        &self,
        response: &LlmResponse,
        parsed: &crate::decision::ParsedDecision,
    ) -> LlmResponse {
        let mut response = response.clone();
        response.content = recordable_content(parsed).or_else(|| response.content.clone());
        response
    }

    async fn record_user_message(&mut self, content: &str) -> Result<(), RuntimeError> {
        let event = Event::UserMessage {
            content: content.to_string(),
            timestamp: Utc::now(),
        };
        append_and_ingest(&mut self.context, event).await?;
        self.context
            .session
            .messages
            .push(holmes_core::Message::user(content.to_string()));
        Ok(())
    }

    /// Drain operator steering messages typed while the turn was in flight and inject
    /// them into the conversation, so the very next perception/deliberation sees them.
    /// Each line is wrapped (grok-build interjection style) so the model can tell a
    /// mid-turn correction apart from the original instruction, recorded as a
    /// `UserMessage` event (keeping the event log the source of truth), and echoed to
    /// the UI as `SteeringInjected` so the operator gets confirmation it landed.
    /// Messages pushed after the last drain of a turn stay in the shared queue; the
    /// caller re-routes those (e.g. into follow-up turns).
    async fn drain_steering(&mut self, sink: &mut dyn RuntimeSink) -> Result<(), RuntimeError> {
        let pending: Vec<String> = {
            let mut queue = self
                .context
                .steering
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            queue.drain(..).collect()
        };
        for message in pending {
            let trimmed = message.trim();
            if trimmed.is_empty() {
                continue;
            }
            let wrapped = format!(
                "The operator sent a message while you were working:\n<operator_message>\n{trimmed}\n</operator_message>"
            );
            self.record_user_message(&wrapped).await?;
            sink.emit_yield(
                &self.context.session_id,
                RuntimeYield::SteeringInjected {
                    content: trimmed.to_string(),
                },
            );
        }
        Ok(())
    }

    /// Drain finished background subagent tasks and inject each outcome into the
    /// conversation as a `<system-reminder>`-wrapped user message (same pattern as
    /// `drain_steering`, grok-build style), so the next LLM request sees the result
    /// without any polling.
    ///
    /// Durable result delivery (P1-02): the task store is authoritative. Each
    /// drain first delivers every terminal, not-yet-delivered task for this
    /// session from the DATABASE (results persisted before a crash, or by a
    /// scheduler-run re-execution), then drains the in-memory registry. Every
    /// durable delivery goes through `deliver_task_result`, which appends the
    /// result event and marks the task delivered in one transaction — no crash
    /// window between "result in the conversation" and "delivery recorded", and
    /// repeats return `AlreadyDelivered` instead of double-presenting.
    ///
    /// Runs at every iteration boundary and once more at each turn exit: a task that
    /// finishes during the final LLM call is still folded into history (the next turn
    /// then sees it as a plain past message), while tasks still running at turn end
    /// stay in the shared registry and are delivered by the next turn's first drain.
    async fn drain_background_tasks(
        &mut self,
        sink: &mut dyn RuntimeSink,
    ) -> Result<(), RuntimeError> {
        use holmes_core::background::TaskDeliveryOutcome;

        let session_id = self.context.session_id.clone();
        match self
            .context
            .session_db
            .list_undelivered_task_results(&session_id)
            .await
        {
            Ok(pending) => {
                for task in pending {
                    let (outcome, body, success) = match task.state.as_str() {
                        "succeeded" => (
                            "completed",
                            truncate_chars(
                                task.result.as_deref().unwrap_or_default(),
                                BACKGROUND_INJECT_CHAR_CAP,
                            ),
                            true,
                        ),
                        "cancelled" => (
                            "cancelled",
                            truncate_chars(
                                task.error.as_deref().unwrap_or_default(),
                                BACKGROUND_INJECT_CHAR_CAP,
                            ),
                            false,
                        ),
                        _ => (
                            "failed",
                            truncate_chars(
                                task.error.as_deref().unwrap_or_default(),
                                BACKGROUND_INJECT_CHAR_CAP,
                            ),
                            false,
                        ),
                    };
                    let wrapped = format!(
                        "<system-reminder>Background task {} (\"{}\") {outcome}:\n{body}\n</system-reminder>",
                        task.task_id, task.description
                    );
                    match self.deliver_durable_result(&task.task_id, &wrapped).await {
                        Ok(TaskDeliveryOutcome::Appended) => {
                            self.present_delivered_task(
                                sink,
                                task.task_id,
                                task.description,
                                success,
                                body,
                                wrapped,
                            )
                            .await?;
                        }
                        // AlreadyDelivered: presented earlier (or in a previous
                        // process, hence in replayed history). NotTerminal: a
                        // newer attempt owns the task; its result will arrive.
                        Ok(_) => {}
                        Err(error) => {
                            tracing::warn!(task_id = %task.task_id, error = %error,
                                "durable task delivery failed; retried on the next drain");
                        }
                    }
                }
            }
            Err(error) => {
                tracing::warn!(error = %error,
                    "undelivered durable task scan failed; in-memory drain continues");
            }
        }

        for task in self.context.background_tasks.take_finished_undelivered() {
            let success = task.result.is_ok();
            let (outcome, body) = match &task.result {
                Ok(output) => (
                    "completed",
                    truncate_chars(output, BACKGROUND_INJECT_CHAR_CAP),
                ),
                Err(error) => ("failed", truncate_chars(error, BACKGROUND_INJECT_CHAR_CAP)),
            };
            let wrapped = format!(
                "<system-reminder>Background task {} (\"{}\") {outcome}:\n{body}\n</system-reminder>",
                task.id, task.description
            );
            match self.deliver_durable_result(&task.id, &wrapped).await {
                Ok(TaskDeliveryOutcome::Appended) => {
                    self.present_delivered_task(
                        sink,
                        task.id,
                        task.description,
                        success,
                        body,
                        wrapped,
                    )
                    .await?;
                }
                Ok(TaskDeliveryOutcome::AlreadyDelivered)
                | Ok(TaskDeliveryOutcome::NotTerminal) => {}
                // UnknownTask (no durable record — registry-only task) or a
                // store error: fall back to the plain append path so the
                // result still reaches the conversation.
                Ok(TaskDeliveryOutcome::UnknownTask) | Err(_) => {
                    self.record_user_message(&wrapped).await?;
                    sink.emit_yield(
                        &self.context.session_id,
                        RuntimeYield::BackgroundTaskFinished {
                            task_id: task.id,
                            description: task.description,
                            success,
                            summary: body,
                        },
                    );
                }
            }
        }
        Ok(())
    }

    /// Persist a rendered result reminder through the atomic delivery path
    /// (P1-02). `before_event_persist` middlewares still run, keeping the
    /// audit/mutation semantics of the plain `append_and_ingest` path.
    async fn deliver_durable_result(
        &mut self,
        task_id: &str,
        content: &str,
    ) -> Result<holmes_core::background::TaskDeliveryOutcome, holmes_session::db::SessionError>
    {
        let mut event = Event::UserMessage {
            content: content.to_string(),
            timestamp: Utc::now(),
        };
        let middlewares = self.context.middlewares.clone();
        for mw in &middlewares {
            if let Err(error) = mw.before_event_persist(&mut self.context, &mut event).await {
                tracing::warn!(task_id = %task_id, error = %error,
                    "middleware rejected durable task delivery event; delivering raw content");
            }
        }
        let content = match &event {
            Event::UserMessage { content, .. } => content.clone(),
            _ => content.to_string(),
        };
        self.context
            .session_db
            .deliver_task_result(&self.context.session_id, task_id, &content)
            .await
    }

    /// Present a durably-delivered task result in the live turn: the event is
    /// already persisted (atomically with the delivery mark), so this only
    /// ingests into the working set, pushes the in-memory message and emits
    /// the UI yield.
    async fn present_delivered_task(
        &mut self,
        sink: &mut dyn RuntimeSink,
        task_id: String,
        description: String,
        success: bool,
        body: String,
        wrapped: String,
    ) -> Result<(), RuntimeError> {
        self.context.mind_palace.ingest(Event::UserMessage {
            content: wrapped.clone(),
            timestamp: Utc::now(),
        });
        self.context
            .session
            .messages
            .push(holmes_core::Message::user(wrapped));
        sink.emit_yield(
            &self.context.session_id,
            RuntimeYield::BackgroundTaskFinished {
                task_id,
                description,
                success,
                summary: body,
            },
        );
        Ok(())
    }

    async fn maybe_compact(&mut self) -> Result<Option<CompressionResult>, RuntimeError> {
        self.compact_with_trigger(false, holmes_core::CompactionTrigger::Threshold)
            .await
    }

    async fn compact_with_trigger(
        &mut self,
        force: bool,
        trigger: holmes_core::CompactionTrigger,
    ) -> Result<Option<CompressionResult>, RuntimeError> {
        let current_msg_count = self.context.session.messages.len();
        for hook in &self.action.hooks {
            if let Err(e) = hook.pre_compact(current_msg_count) {
                return Err(RuntimeError::recoverable(format!(
                    "Hook blocked compaction: {}",
                    e
                )));
            }
        }

        let plan = self.compactor.plan_with_floor(
            &self.context.session,
            &self.context.config,
            force,
            self.last_prompt_tokens,
        );
        let plan_protected_head = plan.protected_head;
        let plan_protected_tail_start = plan.protected_tail_start;
        let protected_tail_tokens = self.context.config.compressor.protected_tail_tokens as usize;

        if !plan.should_compress {
            // Below the compaction threshold: opportunistically warm the summary in the
            // background once usage enters the prefire window (threshold - 0.10). When
            // the threshold is later crossed with this exact span still intact, the
            // summary is reused and compaction costs zero LLM latency (grok-build
            // two-pass prefire).
            self.maybe_spawn_prefire(&plan);
            return Ok(None);
        }

        // Snapshot the pre-compaction state so we can archive the messages and
        // events that are about to be summarized away.
        let events_before = self
            .context
            .session_db
            .get_events(&self.context.session_id)
            .await
            .map_err(|error| {
                RuntimeError::recoverable(format!(
                    "failed to snapshot events before compaction in session {}: {}",
                    self.context.session_id, error
                ))
            })?;
        let messages_before = self.context.session.messages.clone();
        let head = plan_protected_head.min(messages_before.len());
        let tail = plan_protected_tail_start.min(messages_before.len());
        let has_middle = head < tail;
        let llm_summary_enabled = self.context.config.compressor.llm_summary;

        // Resolve any background prefire BEFORE the flush side call: the prefire task
        // and the flush both hit the same backend, and settling the prefire first keeps
        // the call order deterministic (prefire → optional sync summary → flush).
        //
        // Optionally summarize the middle with an LLM (higher-fidelity than the static
        // keyword template). Off by default; falls back to static on any failure. A
        // background prefire result whose span fingerprint still matches is reused
        // instead of issuing a duplicate synchronous call.
        let summary_override = if llm_summary_enabled && has_middle {
            match self.take_matching_prefire(head, tail).await {
                Some(summary) => Some(summary),
                None => {
                    self.llm_summarize_middle(&messages_before[head..tail])
                        .await
                }
            }
        } else {
            None
        };

        // Pre-compaction flush (grok-build memory flush): before the middle is
        // summarized away, ask the compressor role to extract the case state that MUST
        // survive, persist it to long-term memory (recallable in later turns), and fold
        // it into the summary head. LLM mode only, so the static path — and scripted
        // harness replays — see zero extra LLM calls. Best-effort: failures never block
        // compaction.
        let preserved_state = if llm_summary_enabled
            && self.context.config.compressor.pre_compact_flush
            && has_middle
        {
            match self.pre_compact_flush(&messages_before[head..tail]).await {
                Some(notes) => {
                    if let Err(error) = self
                        .memory
                        .remember_flush_note(&mut self.context, &notes)
                        .await
                    {
                        tracing::warn!("pre-compaction flush note could not be stored: {error}");
                    }
                    Some(notes)
                }
                None => {
                    tracing::warn!("pre-compaction flush returned no notes; compacting without preserved-state section");
                    None
                }
            }
        } else {
            None
        };

        let state_context = Some(crate::perception::situation_from_state(
            &self.context.state.compatibility_state,
        ));
        let Some(mut result) = self
            .compactor
            .compress_session(
                &mut self.context.session,
                &self.context.config,
                plan,
                trigger.clone(),
                summary_override,
                state_context,
                preserved_state,
            )
            .map_err(|error| RuntimeError::recoverable(error.to_string()))?
        else {
            return Ok(None);
        };

        if let Some((start, end)) = result.archived_message_range {
            let archived_messages = messages_before
                .get(start..end)
                .map(|slice| slice.to_vec())
                .unwrap_or_default();
            let archived_event_range = events_before
                .first()
                .zip(events_before.last())
                .map(|(first, last)| (first.event_index, last.event_index));
            let next_index = self.next_event_index().await?;

            let archive = holmes_session::CompactionArchive {
                schema_version: holmes_session::COMPACTION_ARCHIVE_SCHEMA_VERSION,
                session_id: self.context.session_id.clone(),
                compaction_event_index: next_index,
                trigger: trigger.clone(),
                archived_event_range: archived_event_range
                    .map(|(s, e)| holmes_session::ArchivedEventRange { start: s, end: e }),
                messages: archived_messages,
                events: events_before
                    .iter()
                    .map(holmes_session::ArchivedEvent::from_stored)
                    .collect(),
                created_at: Utc::now(),
            };

            // Write the archive before appending the CompressionApplied event so
            // the persisted event always points at a readable archive (atomic).
            let archive_path = self
                .context
                .session_db
                .write_compaction_archive(&self.context.session_id, next_index, &archive)
                .await
                .map_err(|error| {
                    RuntimeError::recoverable(format!(
                        "failed to write compaction archive for session {}: {}",
                        self.context.session_id, error
                    ))
                })?;

            result.archive_path = Some(archive_path);
            result.archived_event_range = archived_event_range;
        }

        append_and_ingest(
            &mut self.context,
            Event::CompressionApplied {
                before_count: result.before_count,
                after_count: result.after_count,
                summary: result.summary.clone(),
                preserved_keys: result.preserved_keys.clone(),
                method: result.method.clone(),
                preserved_head: Some(plan_protected_head),
                preserved_tail_tokens: Some(protected_tail_tokens),
                archive_path: result.archive_path.clone(),
                archived_event_range: result.archived_event_range,
                trigger: Some(result.trigger.clone()),
                timestamp: Some(Utc::now()),
            },
        )
        .await?;

        Ok(Some(result))
    }

    async fn review_learning_for_turn(
        &mut self,
        turn_start_index: u64,
    ) -> Result<(), RuntimeError> {
        if !self.context.config.learning.enabled {
            return Ok(());
        }

        let events = self
            .context
            .session_db
            .get_events(&self.context.session_id)
            .await
            .map_err(|error| {
                RuntimeError::recoverable(format!(
                    "failed to load turn events for learning review in session {}: {}",
                    self.context.session_id, error
                ))
            })?;

        // Review cadence (learning.review_interval_turns): 1 = every turn
        // (default), N = only every Nth turn runs a review. The turn number is
        // derived from the authoritative event log — completed `TurnComplete`
        // events plus the in-flight turn — not from in-memory state, because
        // the CLI rebuilds `AgentRuntime` for every user turn (P2-03), so an
        // in-memory counter never gets past 1 in production.
        let interval = u64::from(self.context.config.learning.review_interval_turns.max(1));
        let completed_turns = events
            .iter()
            .filter(|event| matches!(event.event, Event::TurnComplete { .. }))
            .count() as u64;
        let current_turn = completed_turns + 1;
        if !current_turn.is_multiple_of(interval) {
            return Ok(());
        }

        let turn_events = events
            .into_iter()
            .filter(|event| event.event_index >= turn_start_index)
            .collect::<Vec<_>>();

        let Some(last_event) = turn_events.last() else {
            return Ok(());
        };

        let review = self.learning.review_turn(&self.context, &turn_events);
        if review.candidates.is_empty() {
            return Ok(());
        }

        let trigger = if review.trigger.trim().is_empty() {
            "deterministic_signal".into()
        } else {
            review.trigger.clone()
        };
        record_review_started(
            &mut self.context,
            trigger,
            (turn_start_index, last_event.event_index),
        )
        .await?;
        self.learning
            .apply_review(&mut self.context, review)
            .await?;

        Ok(())
    }

    async fn record_assistant_response(
        &mut self,
        response: &LlmResponse,
    ) -> Result<(), RuntimeError> {
        let content = response.content.as_ref().map(|content| content.trim());
        let has_content = content.is_some_and(|content| !content.is_empty());

        if let Some(content) = content.filter(|content| !content.is_empty()) {
            let event = Event::Thinking {
                content: content.to_string(),
                reasoning_type: None,
            };
            append_and_ingest(&mut self.context, event).await?;
        }

        if has_content || !response.tool_calls.is_empty() {
            self.context.session.messages.push(response.to_message());
        }
        Ok(())
    }

    async fn apply_usage(&mut self, response: &LlmResponse) -> Result<TokenDelta, RuntimeError> {
        let Some(usage) = response.usage.as_ref() else {
            return Ok(TokenDelta::default());
        };

        // Remember the real prompt size (system + tools + full history) so the next
        // compaction check has an accurate floor instead of the messages-only char/4 guess.
        self.last_prompt_tokens = usage.prompt_tokens as u64;
        let delta = TokenDelta {
            input: usage.prompt_tokens as u64,
            output: usage.completion_tokens as u64,
            cache_read: 0,
            cache_write: 0,
        };
        self.context
            .session_db
            .update_token_counts(&self.context.session_id, &delta)
            .await
            .map_err(|error| {
                RuntimeError::recoverable(format!(
                    "failed to update token counts for session {}: {}",
                    self.context.session_id, error
                ))
            })?;
        self.context.session.tokens.input += delta.input;
        self.context.session.tokens.output += delta.output;
        Ok(delta)
    }

    async fn set_runtime_goal(
        &mut self,
        condition: &str,
        reason: Option<&str>,
    ) -> Result<(), RuntimeError> {
        let condition = condition.trim();
        if condition.is_empty() {
            return Err(RuntimeError::recoverable(
                "Holmes tried to set an empty goal condition.",
            ));
        }

        self.context
            .session_db
            .set_goal_condition(&self.context.session_id, Some(condition))
            .await
            .map_err(|error| {
                RuntimeError::recoverable(format!(
                    "failed to persist goal for session {}: {}",
                    self.context.session_id, error
                ))
            })?;

        let event = Event::GoalSet {
            condition: condition.to_string(),
            plan: reason.map(ToOwned::to_owned),
            subtasks: Vec::new(),
        };
        append_and_ingest(&mut self.context, event).await?;
        self.context.state.active_goal = Some(condition.to_string());
        self.context
            .state
            .task_control
            .set_goal(condition.to_string(), Vec::new());
        Ok(())
    }

    async fn record_ask_watson(
        &mut self,
        question: &str,
        context: Option<&str>,
        options: &[String],
    ) -> Result<(), RuntimeError> {
        let mut advice = question.trim().to_string();
        if !options.is_empty() {
            advice.push_str(&format!(" Options: {}", options.join(" / ")));
        }
        let event = Event::AdvisorAction {
            level: InterventionLevel::Suggest,
            advice,
            reasoning: context
                .unwrap_or("Holmes requested Watson input")
                .to_string(),
            auto_applied: false,
        };
        append_and_ingest(&mut self.context, event).await
    }

    async fn record_goal_evaluated(
        &mut self,
        satisfied: bool,
        reason: &str,
        iterations: usize,
    ) -> Result<(), RuntimeError> {
        // Key off the legacy in-memory goal, the (rebuildable) control-state goal,
        // and the derived task contract: after a session resume only the latter two
        // may be populated, and completion-gate evaluations must still be recorded.
        let has_goal = self.context.state.active_goal.is_some()
            || self.context.state.task_control.goal.is_some();
        let has_contract = self.context.state.task_control.contract.is_some();
        if !has_goal && !has_contract {
            return Ok(());
        }

        if satisfied {
            self.context.state.task_control.mark_goal_satisfied();
            self.context
                .state
                .task_control
                .mark_contract_objective_verified();
            if has_goal {
                self.context
                    .session_db
                    .mark_goal_achieved(&self.context.session_id)
                    .await
                    .map_err(|error| {
                        RuntimeError::recoverable(format!(
                            "failed to mark goal achieved for session {}: {}",
                            self.context.session_id, error
                        ))
                    })?;
            }
        }

        let event = Event::GoalEvaluated {
            satisfied,
            reason: reason.trim().to_string(),
            turn_count: iterations as u64,
            tokens_spent: self.context.session.tokens.input + self.context.session.tokens.output,
        };
        append_and_ingest(&mut self.context, event).await
    }

    /// Unified completion gate (P0-02): every terminal outcome — a `finish` call OR
    /// a plain-text answer on a gated task — passes through this one verifier flow.
    /// Deterministic checks are authoritative; the semantic review (independent
    /// `goal_evaluator` role) only judges what deterministic rules cannot.
    async fn run_completion_gate(
        &mut self,
        content: &str,
        iterations: usize,
        verification_failures: &mut usize,
        sink: &mut dyn RuntimeSink,
    ) -> Result<CompletionGateOutcome, RuntimeError> {
        let max_verification_retries =
            self.context.config.supervisor.max_verification_retries as usize;
        // P1-01: the verifier's model call races the turn token/deadline like any
        // other in-flight LLM call; a loss propagates as the Cancelled marker.
        let verification = self
            .interruptible_llm(self.completion.verify(
                &self.context.llm,
                &self.context.state.task_control,
                content,
            ))
            .await?;
        match verification {
            Verification::Passed { evidence_refs } => {
                holmes_core::metrics::metrics().count("completion.verification_passed");
                tracing::info!(
                    event = "CompletionVerificationPassed",
                    session_id = %self.context.session_id,
                    evidence_count = evidence_refs.len(),
                    "completion claim passed verification"
                );
                let reason = if evidence_refs.is_empty() {
                    content.to_string()
                } else {
                    format!(
                        "{content}\n[verified; evidence: {}]",
                        evidence_refs.join("; ")
                    )
                };
                self.record_goal_evaluated(true, &reason, iterations)
                    .await?;
                Ok(CompletionGateOutcome::Passed)
            }
            Verification::Failed { gaps } => {
                *verification_failures += 1;
                holmes_core::metrics::metrics().count("completion.verification_failed");
                tracing::warn!(
                    event = "CompletionVerificationFailed",
                    session_id = %self.context.session_id,
                    attempt = *verification_failures,
                    gaps = ?gaps,
                    "completion claim rejected by verification"
                );
                let gap_note = gaps.join("; ");
                self.record_goal_evaluated(
                    false,
                    &format!("completion verification failed: {gap_note}"),
                    iterations,
                )
                .await?;

                if *verification_failures > max_verification_retries {
                    // Retries exhausted: end with a partial, resumable result that
                    // names what is missing — never a fake completion, never a bare
                    // failure (AGT-009/010).
                    for gap in &gaps {
                        self.context
                            .state
                            .task_control
                            .add_open_question(gap.clone());
                    }
                    let partial = self.supervisor.budget_exhausted_message(
                        &format!(
                            "completion could not be verified after {verification_failures} attempt(s); the unverified claim was: {content}"
                        ),
                        &self.context.state.task_control,
                    );
                    Ok(CompletionGateOutcome::Exhausted(partial))
                } else {
                    // Feed the gaps back and continue the loop.
                    let note = format!(
                        "Completion verification rejected the completion claim. Gaps:\n{}\nResolve these before declaring completion again — do not re-declare completion until they are addressed.",
                        gaps.iter()
                            .map(|gap| format!("- {gap}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    );
                    self.inject_supervisor_note(&note, sink).await?;
                    Ok(CompletionGateOutcome::Rejected)
                }
            }
        }
    }

    async fn next_event_index(&self) -> Result<u64, RuntimeError> {
        self.context
            .session_db
            .get_events(&self.context.session_id)
            .await
            .map(|events| events.len() as u64)
            .map_err(|error| {
                RuntimeError::recoverable(format!(
                    "failed to inspect event index for session {}: {}",
                    self.context.session_id, error
                ))
            })
    }

    async fn record_turn_complete(
        &mut self,
        turn_start_index: u64,
        tokens_used: &TokenDelta,
    ) -> Result<(), RuntimeError> {
        // Turn boundary: any in-flight prefire describes this turn's span, so drop it
        // (aborting the task) rather than risk reusing it after the next turn's appends.
        self.discard_prefire();
        let next_index = self.next_event_index().await?;
        let event_range = if next_index == 0 {
            (turn_start_index, turn_start_index)
        } else {
            (turn_start_index, next_index.saturating_sub(1))
        };
        append_and_ingest(
            &mut self.context,
            Event::TurnComplete {
                event_range,
                tokens_used: tokens_used.clone(),
                sub_agents_spawned: Vec::new(),
            },
        )
        .await
    }

    /// Race an in-flight LLM-side future (deliberation, completion verification)
    /// against the turn's cancellation token and absolute turn deadline (P1-01): an
    /// in-flight call ends with the turn instead of only being noticed at the next
    /// iteration boundary. On either signal the turn's token is cancelled (so
    /// in-flight tools/transports clean up as well) and a `Cancelled` marker error
    /// is returned for the turn loop to intercept. Dropping the losing future aborts
    /// the underlying HTTP request — no background provider request lingers.
    async fn interruptible_llm<F, T>(&self, future: F) -> Result<T, RuntimeError>
    where
        F: std::future::Future<Output = T>,
    {
        let token = self.context.exec.token();
        let remaining = self.context.exec.remaining_turn_time();
        tokio::pin!(future);
        let signal = tokio::select! {
            value = &mut future => return Ok(value),
            _ = token.cancelled() => "cancellation",
            _ = async {
                match remaining {
                    Some(wait) => tokio::time::sleep(wait).await,
                    None => std::future::pending().await,
                }
            } => "deadline",
        };
        self.context.exec.cancel();
        Err(RuntimeError::cancelled(format!(
            "turn {signal} interrupted an in-flight LLM call (task {})",
            self.context.exec.task_id()
        )))
    }

    /// Turn ending after a cooperative interrupt (Esc / parent cancellation), used by
    /// both the iteration-boundary check and the mid-iteration LLM interrupt (P1-01).
    /// Completed steps are already persisted; the cancel flag is reset so the next
    /// turn starts clean.
    async fn finish_turn_interrupted(
        &mut self,
        turn_start_index: u64,
        turn_tokens: &TokenDelta,
        iterations: usize,
        sink: &mut dyn RuntimeSink,
    ) -> Result<TurnOutcome, RuntimeError> {
        self.context
            .cancel
            .swap(false, std::sync::atomic::Ordering::Relaxed);
        // Final background-task drain (see drain_background_tasks): folds in
        // tasks that finished during the last LLM call of this turn.
        if let Err(error) = self.drain_background_tasks(sink).await {
            return self.stop_for_error(error, iterations, sink);
        }
        if let Err(error) = self.review_learning_for_turn(turn_start_index).await {
            return self.stop_for_error(error, iterations, sink);
        }
        self.record_turn_complete(turn_start_index, turn_tokens)
            .await?;
        sink.emit_yield(
            &self.context.session_id,
            RuntimeYield::MessageToUser {
                content: "⏸ Turn interrupted by user.".into(),
            },
        );
        let middlewares = self.context.middlewares.clone();
        for mw in &middlewares {
            mw.after_step(&mut self.context).await?;
        }
        Ok(TurnOutcome::Interrupted { iterations })
    }

    /// Turn ending after the absolute turn deadline fired — at the iteration boundary
    /// or mid-LLM-call (P1-01). Reported via the budget-exhaustion outcome so callers
    /// need no new variant; partial progress is persisted.
    async fn finish_turn_deadline(
        &mut self,
        turn_start_index: u64,
        turn_tokens: &TokenDelta,
        iterations: usize,
        sink: &mut dyn RuntimeSink,
    ) -> Result<TurnOutcome, RuntimeError> {
        self.context.exec.cancel();
        if let Err(error) = self.drain_background_tasks(sink).await {
            return self.stop_for_error(error, iterations, sink);
        }
        if let Err(error) = self.review_learning_for_turn(turn_start_index).await {
            return self.stop_for_error(error, iterations, sink);
        }
        self.record_turn_complete(turn_start_index, turn_tokens)
            .await?;
        let reason = format!(
            "turn deadline exceeded ({} ms); stopping with partial progress persisted",
            self.context.config.execution.turn_deadline_ms.unwrap_or(0)
        );
        let message = self
            .supervisor
            .budget_exhausted_message(&reason, &self.context.state.task_control);
        tracing::warn!(
            event = "ToolDeadlineExceeded",
            tool = "turn",
            task_id = %self.context.exec.task_id(),
            deadline_ms = self.context.config.execution.turn_deadline_ms.unwrap_or(0),
            "turn deadline exceeded"
        );
        sink.emit_yield(
            &self.context.session_id,
            RuntimeYield::Error {
                message: message.clone(),
            },
        );
        let middlewares = self.context.middlewares.clone();
        for mw in &middlewares {
            mw.after_step(&mut self.context).await?;
        }
        Ok(TurnOutcome::MaxIterationsReached {
            message,
            iterations,
        })
    }

    /// Terminal bookkeeping after an in-flight LLM call lost the race against the
    /// turn token/deadline (P1-01): folds into the same turn-ending paths the
    /// iteration boundary uses — an expired turn reports the deadline outcome, any
    /// other cancellation reports Interrupted.
    async fn finish_after_llm_interrupt(
        &mut self,
        turn_start_index: u64,
        turn_tokens: &TokenDelta,
        iterations: usize,
        sink: &mut dyn RuntimeSink,
    ) -> Result<TurnOutcome, RuntimeError> {
        if self.context.exec.turn_expired() {
            self.finish_turn_deadline(turn_start_index, turn_tokens, iterations, sink)
                .await
        } else {
            self.finish_turn_interrupted(turn_start_index, turn_tokens, iterations, sink)
                .await
        }
    }

    fn stop_for_error(
        &mut self,
        error: RuntimeError,
        iterations: usize,
        sink: &mut dyn RuntimeSink,
    ) -> Result<TurnOutcome, RuntimeError> {
        self.discard_prefire();
        self.context.state.failures.push(error.message.clone());
        match self.reflection.assess_error(&error) {
            ReflectionOutcome::NeedsUser(prompt) => {
                sink.emit_yield(&self.context.session_id, self.dialogue.format_error(&error));
                Ok(TurnOutcome::NeedsUser { prompt, iterations })
            }
            ReflectionOutcome::RuntimeError { kind, message } => {
                sink.emit_yield(&self.context.session_id, self.dialogue.format_error(&error));
                Err(RuntimeError { kind, message })
            }
            _ => {
                sink.emit_yield(
                    &self.context.session_id,
                    RuntimeYield::Error {
                        message: error.message.clone(),
                    },
                );
                Err(error)
            }
        }
    }
}

fn accumulate_tokens(total: &mut TokenDelta, delta: &TokenDelta) {
    total.input += delta.input;
    total.output += delta.output;
    total.cache_read += delta.cache_read;
    total.cache_write += delta.cache_write;
}

/// Char-wise truncation for injected background-task results (char boundary safe).
fn truncate_chars(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text.to_string();
    }
    let truncated: String = text.chars().take(cap).collect();
    format!("{truncated}\n[...truncated]")
}

/// Render the to-be-compacted middle as a flat `[Role] content` transcript for
/// compressor-role side calls (summary + pre-compaction flush).
fn middle_transcript(middle: &[holmes_core::Message]) -> String {
    middle
        .iter()
        .filter_map(|m| {
            m.content
                .as_deref()
                .filter(|c| !c.trim().is_empty())
                .map(|c| format!("[{:?}] {c}", m.role))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Shared compressor-role summary call used by BOTH the synchronous compaction path and
/// the background prefire task, so both build the identical request (identical prompt is
/// what lets the provider's prompt-prefix cache make the prefire a near-free warm-up).
/// Returns `None` on empty input or any LLM error.
async fn summarize_middle_with(
    llm: &std::sync::Arc<dyn crate::deliberation::LlmBackend>,
    middle: &[holmes_core::Message],
) -> Option<String> {
    let transcript = middle_transcript(middle);
    if transcript.trim().is_empty() {
        return None;
    }
    let prompt = format!(
        "Summarize the following investigation transcript concisely for case continuity. \
         Preserve: confirmed findings, credentials/access gained, key evidence and endpoints, \
         active hypotheses, and the immediate next steps. Drop chit-chat and dead ends. \
         Transcript:\n\n{transcript}"
    );
    let messages = vec![holmes_core::Message::user(prompt)];
    match llm.chat_completion(&messages, &[], "compressor").await {
        Ok(response) => response.content.filter(|c| !c.trim().is_empty()),
        Err(_) => None,
    }
}

fn compaction_event(result: &CompressionResult) -> RuntimeYield {
    RuntimeYield::CompactionBoundary {
        before_count: result.before_count,
        after_count: result.after_count,
        summary: result.summary.clone(),
        preserved_keys: result.preserved_keys.clone(),
        method: compression_method_name(&result.method).into(),
    }
}

fn compression_method_name(method: &holmes_core::CompressionMethod) -> &'static str {
    match method {
        holmes_core::CompressionMethod::LlmSummary => "llm_summary",
        holmes_core::CompressionMethod::StaticFallback => "static_fallback",
    }
}

fn format_watson_prompt(question: &str, context: Option<&str>, options: &[String]) -> String {
    let mut sections = Vec::new();
    sections.push(question.trim().to_string());
    if let Some(context) = context.map(str::trim).filter(|context| !context.is_empty()) {
        sections.push(format!("Context: {context}"));
    }
    if !options.is_empty() {
        sections.push(format!("Options: {}", options.join(" / ")));
    }
    sections.join("\n")
}

fn recordable_content(parsed: &crate::decision::ParsedDecision) -> Option<String> {
    if parsed.display_content.is_some() {
        return parsed.display_content.clone();
    }

    match &parsed.decision {
        HolmesDecision::Answer { message } => nonempty(message),
        HolmesDecision::AskWatson { question, .. } => {
            nonempty(format!("Asked Watson: {}", question.trim()))
        }
        HolmesDecision::UseTools { rationale, .. } => rationale.as_deref().and_then(nonempty),
        HolmesDecision::SetGoal { condition, reason } => {
            let content =
                if let Some(reason) = reason.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    format!("Set goal: {} ({})", condition.trim(), reason)
                } else {
                    format!("Set goal: {}", condition.trim())
                };
            nonempty(content)
        }
        HolmesDecision::Finish { summary, .. } => nonempty(summary),
        HolmesDecision::Continue => None,
        HolmesDecision::ProtocolViolation { message } => nonempty(message),
    }
}

fn risk_rank(risk: &holmes_core::ledger::RiskLevel) -> u8 {
    match risk {
        holmes_core::ledger::RiskLevel::Low => 0,
        holmes_core::ledger::RiskLevel::Medium => 1,
        holmes_core::ledger::RiskLevel::High => 2,
        holmes_core::ledger::RiskLevel::Critical => 3,
    }
}

fn nonempty(content: impl AsRef<str>) -> Option<String> {
    let content = content.as_ref().trim();
    if content.is_empty() {
        None
    } else {
        Some(content.to_string())
    }
}

async fn append_and_ingest(
    context: &mut RuntimeContext,
    mut event: Event,
) -> Result<(), RuntimeError> {
    let middlewares = context.middlewares.clone();
    for mw in &middlewares {
        mw.before_event_persist(context, &mut event).await?;
    }
    context
        .session_db
        .append_event(&context.session_id, &event)
        .await
        .map_err(|error| {
            RuntimeError::recoverable(format!(
                "failed to persist runtime event for session {}: {}",
                context.session_id, error
            ))
        })?;
    context.mind_palace.ingest(event);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    use anyhow::Result;
    use async_trait::async_trait;
    use holmes_core::config::HolmesConfig;
    use holmes_core::session::RuntimeSession;
    use holmes_core::state::{AttackState, PortInfo};
    use holmes_core::tool_types::{
        FunctionCall, FunctionDefinition, LlmResponse, Message, ToolCall, ToolDefinition,
        ToolResult, Usage,
    };
    use holmes_core::{GuardVerdict, SessionMode};
    use holmes_guards::traits::{PostGuard, PreGuard};
    use holmes_guards::GuardChain;
    use holmes_mind_palace::MindPalace;
    use holmes_session::{memory_store::MemoryStore, CreateSessionParams, SessionDB, SessionStore};
    use holmes_tools::{Tool, ToolRegistry};

    use crate::deliberation::LlmBackend;
    use crate::yield_stream::VecSink;
    use crate::{RuntimeContext, RuntimeState};

    use super::*;

    #[tokio::test]
    async fn emergency_truncate_shortens_oversized_messages_preserving_count() {
        let llm = Arc::new(QueueLlmBackend::new(vec![Ok(final_response("x"))]));
        let context = make_context(llm, ToolRegistry::new(), GuardChain::new()).await;
        let mut runtime = AgentRuntime::new(context);
        runtime
            .context
            .session
            .messages
            .push(Message::user("small"));
        runtime
            .context
            .session
            .messages
            .push(Message::assistant("A".repeat(20_000)));
        let before = runtime.context.session.messages.len();

        let changed = runtime.emergency_truncate_messages(EMERGENCY_MESSAGE_CHAR_CAP);

        assert!(changed);
        assert_eq!(
            runtime.context.session.messages.len(),
            before,
            "count unchanged"
        );
        assert_eq!(
            runtime.context.session.messages[0].content.as_deref(),
            Some("small")
        );
        let big = runtime.context.session.messages[1]
            .content
            .as_deref()
            .unwrap();
        assert!(big.chars().count() < 20_000, "oversized message truncated");
        assert!(big.contains("truncated during context-overflow recovery"));
    }

    #[tokio::test]
    async fn run_turn_returns_final_answer_and_records_messages() {
        let llm = Arc::new(QueueLlmBackend::new(vec![Ok(final_response(
            "hello Watson",
        ))]));
        let context = make_context(llm, ToolRegistry::new(), GuardChain::new()).await;
        let mut runtime = AgentRuntime::new(context);
        let mut sink = VecSink::new();

        let outcome = runtime
            .run_turn("hello", &mut sink)
            .await
            .expect("turn outcome");

        assert_eq!(
            outcome,
            TurnOutcome::FinalAnswer {
                content: "hello Watson".into(),
                iterations: 1,
            }
        );
        assert_eq!(
            sink.yields(),
            vec![RuntimeYield::FinalAnswer {
                content: "hello Watson".into(),
                usage: None
            }]
        );
        assert_eq!(runtime.context().session.messages.len(), 2);
        assert_eq!(runtime.context().session.tokens.input, 7);
        assert_eq!(runtime.context().session.tokens.output, 3);

        let stored = runtime
            .context()
            .session_db
            .get_events(&runtime.context().session_id)
            .await
            .expect("stored events");
        assert!(matches!(stored[0].event, Event::UserMessage { .. }));
        assert!(matches!(stored[1].event, Event::Thinking { .. }));
        assert!(matches!(
            &stored[2].event,
            Event::TurnComplete {
                event_range: (0, 1),
                tokens_used,
                ..
            } if tokens_used.input == 7 && tokens_used.output == 3
        ));
        assert_eq!(runtime.context().mind_palace.memory.event_count(), 3);
    }

    /// Mutating counterpart of `MockTool` for Ask-mode approval tests.
    struct WriteMockTool;

    #[async_trait]
    impl Tool for WriteMockTool {
        fn name(&self) -> &str {
            "write_mock"
        }

        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                tool_type: "function".into(),
                function: FunctionDefinition {
                    name: "write_mock".into(),
                    description: "mutating mock tool".into(),
                    parameters: Default::default(),
                },
            }
        }

        fn is_read_only(&self) -> bool {
            false
        }

        async fn execute(&self, _args: &str) -> Result<String> {
            Ok("wrote".into())
        }
    }

    #[derive(Debug)]
    struct FixedApprover(bool);

    #[async_trait]
    impl ApprovalHandler for FixedApprover {
        async fn request_approval(&self, _call: &ToolCall) -> bool {
            self.0
        }
    }

    #[tokio::test]
    async fn ask_mode_turn_runs_mutating_tool_when_approver_allows() {
        let call = make_call("write_mock", "{}");
        let llm = Arc::new(QueueLlmBackend::new(vec![
            Ok(tool_response(call)),
            Ok(final_response("done")),
        ]));
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(WriteMockTool));
        let mut config = HolmesConfig::default();
        config.permissions.mode = holmes_core::config::PermissionMode::Ask;
        let context = make_context_with_config(llm, tools, GuardChain::new(), config).await;
        let mut runtime = AgentRuntime::new(context);
        runtime.set_approver(Arc::new(FixedApprover(true)));
        let mut sink = VecSink::new();

        let outcome = runtime
            .run_turn("go", &mut sink)
            .await
            .expect("turn outcome");

        assert!(matches!(outcome, TurnOutcome::FinalAnswer { .. }));
        let finished = sink
            .yields()
            .into_iter()
            .find(|y| matches!(y, RuntimeYield::ToolFinished { name, .. } if name == "write_mock"))
            .expect("write_mock finished");
        assert!(
            matches!(finished, RuntimeYield::ToolFinished { success, .. } if success),
            "approved call executed"
        );
    }

    #[tokio::test]
    async fn ask_mode_turn_blocks_mutating_tool_when_approver_denies() {
        let call = make_call("write_mock", "{}");
        let llm = Arc::new(QueueLlmBackend::new(vec![
            Ok(tool_response(call)),
            Ok(final_response("stopped")),
        ]));
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(WriteMockTool));
        let mut config = HolmesConfig::default();
        config.permissions.mode = holmes_core::config::PermissionMode::Ask;
        // The blocked call leaves an unresolved failure, so the completion gate
        // (P0-02) rejects the plain-text answer; with zero verification retries the
        // turn ends as an explicit partial result that still carries the report.
        config.supervisor.max_verification_retries = 0;
        let context = make_context_with_config(llm, tools, GuardChain::new(), config).await;
        let mut runtime = AgentRuntime::new(context);
        runtime.set_approver(Arc::new(FixedApprover(false)));
        let mut sink = VecSink::new();

        let outcome = runtime
            .run_turn("go", &mut sink)
            .await
            .expect("turn outcome");

        let content = match outcome {
            TurnOutcome::FinalAnswer { content, .. } => content,
            other => panic!("expected FinalAnswer, got {other:?}"),
        };
        assert!(
            content.contains("stopped") && content.contains("Remaining work"),
            "gated answer must end as an explicit partial carrying the report, got: {content}"
        );
        // A blocked call surfaces as a failed ToolFinished named "guard" (see
        // `ToolResult::blocked`), carrying the denial reason.
        let finished = sink
            .yields()
            .into_iter()
            .find(|y| matches!(y, RuntimeYield::ToolFinished { success, .. } if !success))
            .expect("blocked tool result");
        assert!(
            matches!(&finished, RuntimeYield::ToolFinished { content, .. }
                if content.contains("denied by operator")),
            "denied call blocked, got {finished:?}"
        );
    }

    #[tokio::test]
    async fn run_turn_executes_tools_projects_evidence_and_finishes() {
        let call = make_call("mock_tool", "{}");
        let llm = Arc::new(QueueLlmBackend::new(vec![
            Ok(tool_response(call)),
            Ok(final_response("done")),
        ]));
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(MockTool));
        let mut guards = GuardChain::new();
        guards.post.push(Box::new(ServicePostGuard));
        let context = make_context(llm, tools, guards).await;
        let mut runtime = AgentRuntime::new(context);
        let mut sink = VecSink::new();

        let outcome = runtime
            .run_turn("inspect", &mut sink)
            .await
            .expect("turn outcome");

        assert_eq!(
            outcome,
            TurnOutcome::FinalAnswer {
                content: "done".into(),
                iterations: 2,
            }
        );
        assert_eq!(
            sink.yields(),
            vec![
                RuntimeYield::PermissionDecision {
                    tool_name: "mock_tool".into(),
                    call_id: Some("call-1".into()),
                    allowed: true,
                    reason: "read-only tool auto-approved".into()
                },
                RuntimeYield::ToolStarted {
                    name: "mock_tool".into(),
                    call_id: Some("call-1".into()),
                    args: Some("{}".into())
                },
                RuntimeYield::ToolFinished {
                    name: "mock_tool".into(),
                    call_id: Some("call-1".into()),
                    success: true,
                    content: "mock output".into(),
                    error: None,
                    usage: None
                },
                RuntimeYield::EvidenceUpdate {
                    content: "Discovered service on port 443: https nginx.".into()
                },
                RuntimeYield::FinalAnswer {
                    content: "done".into(),
                    usage: None
                },
            ]
        );
        assert_eq!(runtime.context().session.messages.len(), 4);
        assert_eq!(
            runtime.context().state.observations,
            vec!["Discovered service on port 443: https nginx."]
        );

        let stored = runtime
            .context()
            .session_db
            .get_events(&runtime.context().session_id)
            .await
            .expect("stored events");
        assert!(stored
            .iter()
            .any(|stored| matches!(stored.event, Event::ToolCall { .. })));
        assert!(stored
            .iter()
            .any(|stored| matches!(stored.event, Event::ToolResult { .. })));
    }

    #[tokio::test]
    async fn run_turn_streams_assistant_message_before_tool_progress() {
        let call = make_call("mock_tool", "{}");
        let llm = Arc::new(QueueLlmBackend::new(vec![
            Ok(tool_response_with_message(
                "I will inspect the exposed service first.",
                call,
            )),
            Ok(final_response("done")),
        ]));
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(MockTool));
        let context = make_context(llm, tools, GuardChain::new()).await;
        let mut runtime = AgentRuntime::new(context);
        let mut sink = VecSink::new();

        runtime
            .run_turn("inspect", &mut sink)
            .await
            .expect("turn outcome");

        assert_eq!(
            &sink.yields()[..4],
            &[
                RuntimeYield::MessageToUser {
                    content: "I will inspect the exposed service first.".into()
                },
                RuntimeYield::PermissionDecision {
                    tool_name: "mock_tool".into(),
                    call_id: Some("call-1".into()),
                    allowed: true,
                    reason: "read-only tool auto-approved".into()
                },
                RuntimeYield::ToolStarted {
                    name: "mock_tool".into(),
                    call_id: Some("call-1".into()),
                    args: Some("{}".into())
                },
                RuntimeYield::ToolFinished {
                    name: "mock_tool".into(),
                    call_id: Some("call-1".into()),
                    success: true,
                    content: "mock output".into(),
                    error: None,
                    usage: None
                },
            ]
        );
    }

    #[tokio::test]
    async fn run_turn_feeds_blocked_tool_result_back_to_llm() {
        let call = make_call("mock_tool", "{}");
        let llm = Arc::new(QueueLlmBackend::new(vec![
            Ok(tool_response(call)),
            Ok(final_response("adjusted")),
        ]));
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(MockTool));
        let mut guards = GuardChain::new();
        guards.pre.push(Box::new(BlockGuard));
        // The blocked call is an unresolved failure; the completion gate (P0-02)
        // refuses to let the plain-text answer complete the task, so with zero
        // verification retries the turn ends as an explicit partial result.
        let mut config = HolmesConfig::default();
        config.supervisor.max_verification_retries = 0;
        let context = make_context_with_config(llm, tools, guards, config).await;
        let mut runtime = AgentRuntime::new(context);
        let mut sink = VecSink::new();

        let outcome = runtime
            .run_turn("try it", &mut sink)
            .await
            .expect("turn outcome");

        match outcome {
            TurnOutcome::FinalAnswer {
                content,
                iterations,
            } => {
                assert_eq!(iterations, 2);
                assert!(
                    content.contains("adjusted") && content.contains("Remaining work"),
                    "gated answer must surface as a partial result, got: {content}"
                );
            }
            other => panic!("expected FinalAnswer, got {other:?}"),
        }
        assert!(sink.events.iter().any(|event| {
            matches!(
                event.data,
                RuntimeYield::ToolFinished { success: false, .. }
            )
        }));
        let stored = runtime
            .context()
            .session_db
            .get_events(&runtime.context().session_id)
            .await
            .expect("stored events");
        assert!(stored
            .iter()
            .any(|stored| matches!(stored.event, Event::ToolBlocked { .. })));
        assert!(runtime
            .context()
            .session
            .messages
            .iter()
            .any(|message| message
                .content
                .as_deref()
                .unwrap_or_default()
                .contains("[GUARD] blocked by test")));
    }

    #[tokio::test]
    async fn run_oneshot_maps_missing_provider_to_needs_user() {
        let llm = Arc::new(QueueLlmBackend::new(vec![Err(
            "LLM call failed: no healthy LLM provider available".into(),
        )]));
        let context = make_context(llm, ToolRegistry::new(), GuardChain::new()).await;
        let mut runtime = AgentRuntime::new(context);
        let mut sink = VecSink::new();

        let outcome = runtime
            .run_oneshot("hello", &mut sink)
            .await
            .expect("needs user outcome");

        assert!(matches!(outcome, TurnOutcome::NeedsUser { .. }));
        assert!(matches!(
            sink.yields().first().cloned(),
            Some(RuntimeYield::NeedsUserInput { .. })
        ));
    }

    #[tokio::test]
    async fn run_turn_honors_ask_watson_decision() {
        let llm = Arc::new(QueueLlmBackend::new(vec![Ok(final_response(
            r#"I need your call.
<holmes_decision>{"type":"ask_watson","question":"May I test the login form?","context":"This validates the current hypothesis.","options":["yes","no"]}</holmes_decision>"#,
        ))]));
        let context = make_context(llm, ToolRegistry::new(), GuardChain::new()).await;
        let mut runtime = AgentRuntime::new(context);
        let mut sink = VecSink::new();

        let outcome = runtime
            .run_turn("inspect login", &mut sink)
            .await
            .expect("turn outcome");

        assert!(matches!(outcome, TurnOutcome::NeedsUser { .. }));
        assert_eq!(
            sink.yields(),
            vec![
                RuntimeYield::MessageToUser {
                    content: "I need your call.".into()
                },
                RuntimeYield::NeedsUserInput {
                    prompt: "May I test the login form?\nContext: This validates the current hypothesis.\nOptions: yes / no".into()
                }
            ]
        );
        assert!(runtime
            .context()
            .session
            .messages
            .iter()
            .filter_map(|message| message.content.as_deref())
            .all(|content| !content.contains("holmes_decision")));
        let events = runtime
            .context()
            .session_db
            .get_events(&runtime.context().session_id)
            .await
            .expect("events");
        assert!(events
            .iter()
            .any(|event| matches!(event.event, Event::AdvisorAction { .. })));
    }

    #[tokio::test]
    async fn run_turn_interrupts_on_cancel_flag() {
        // The backend would keep the loop going; a set cancel flag must break out at
        // the next iteration boundary and yield an interrupt notice.
        let llm = Arc::new(QueueLlmBackend::new(vec![Ok(final_response("working..."))]));
        let context = make_context(llm, ToolRegistry::new(), GuardChain::new()).await;
        let mut runtime = AgentRuntime::new(context);
        // Request cancellation before the turn runs; the first boundary check trips.
        runtime
            .context()
            .cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let mut sink = VecSink::new();

        let outcome = runtime
            .run_turn("do work", &mut sink)
            .await
            .expect("turn outcome");

        assert!(matches!(
            outcome,
            TurnOutcome::Interrupted { iterations: 0 }
        ));
        assert!(sink.yields().iter().any(|yield_event| matches!(
            yield_event,
            RuntimeYield::MessageToUser { content } if content.contains("interrupted")
        )));
        // The flag is consumed, so a subsequent turn is not pre-cancelled.
        assert!(!runtime
            .context()
            .cancel
            .load(std::sync::atomic::Ordering::Relaxed));
    }

    /// Simulates the operator typing while a tool runs: executing the tool pushes a
    /// steering line into the shared queue, exactly like the UI's busy-loop dispatch.
    struct SteeringPushTool {
        steering: crate::SteeringQueue,
        message: String,
    }

    #[async_trait]
    impl Tool for SteeringPushTool {
        fn name(&self) -> &str {
            "steering_push"
        }

        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                tool_type: "function".into(),
                function: FunctionDefinition {
                    name: "steering_push".into(),
                    description: "pushes a steering message mid-turn".into(),
                    parameters: Default::default(),
                },
            }
        }

        fn is_read_only(&self) -> bool {
            true
        }

        async fn execute(&self, _args: &str) -> Result<String> {
            self.steering
                .lock()
                .expect("steering lock")
                .push_back(self.message.clone());
            Ok("pushed".into())
        }
    }

    #[tokio::test]
    async fn run_turn_drains_steering_at_iteration_boundary() {
        // Iteration 1 runs a tool that "types" a steering line; the iteration-2
        // boundary drain must wrap it, persist it, and make it visible to the next
        // LLM request.
        let steering = crate::new_steering_queue();
        let call = make_call("steering_push", "{}");
        let llm = Arc::new(QueueLlmBackend::new(vec![
            Ok(tool_response(call)),
            Ok(final_response("done")),
        ]));
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(SteeringPushTool {
            steering: steering.clone(),
            message: "focus on port 8080".into(),
        }));
        let mut context = make_context(llm.clone(), tools, GuardChain::new()).await;
        context.set_steering_queue(steering.clone());
        let mut runtime = AgentRuntime::new(context);
        let mut sink = VecSink::new();

        let outcome = runtime
            .run_turn("inspect", &mut sink)
            .await
            .expect("turn outcome");

        assert_eq!(
            outcome,
            TurnOutcome::FinalAnswer {
                content: "done".into(),
                iterations: 2,
            }
        );

        // The second LLM request carries the wrapped steering message.
        let wrapped = "The operator sent a message while you were working:\n<operator_message>\nfocus on port 8080\n</operator_message>";
        let requests = llm.recorded_requests();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[1]
                .iter()
                .any(|m| m.content.as_deref() == Some(wrapped)),
            "second request must include the wrapped steering message, got {:?}",
            requests[1]
                .iter()
                .filter_map(|m| m.content.as_deref())
                .collect::<Vec<_>>()
        );

        // The UI gets a confirmation yield so the operator knows the line landed.
        assert!(sink.yields().iter().any(
            |y| matches!(y, RuntimeYield::SteeringInjected { content } if content == "focus on port 8080")
        ));

        // Event sourcing stays complete: the injection is a persisted UserMessage.
        let stored = runtime
            .context()
            .session_db
            .get_events(&runtime.context().session_id)
            .await
            .expect("stored events");
        assert!(stored
            .iter()
            .any(|s| matches!(&s.event, Event::UserMessage { content, .. } if content == wrapped)));

        // The shared queue is fully drained.
        assert!(steering.lock().expect("steering lock").is_empty());
    }

    /// Backend that "types" a steering line DURING the only LLM call — after the last
    /// iteration-boundary drain — so the turn finishes without the agent seeing it.
    struct LateSteeringBackend {
        steering: crate::SteeringQueue,
        message: String,
    }

    #[async_trait]
    impl LlmBackend for LateSteeringBackend {
        async fn chat_completion(
            &self,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _role: &str,
        ) -> Result<LlmResponse> {
            self.steering
                .lock()
                .expect("steering lock")
                .push_back(self.message.clone());
            Ok(final_response("done"))
        }
    }

    #[tokio::test]
    async fn run_turn_leaves_late_steering_as_leftovers() {
        let steering = crate::new_steering_queue();
        let llm = Arc::new(LateSteeringBackend {
            steering: steering.clone(),
            message: "too late".into(),
        });
        let mut context = make_context(llm, ToolRegistry::new(), GuardChain::new()).await;
        context.set_steering_queue(steering.clone());
        let mut runtime = AgentRuntime::new(context);
        let mut sink = VecSink::new();

        let outcome = runtime
            .run_turn("go", &mut sink)
            .await
            .expect("turn outcome");

        assert!(matches!(outcome, TurnOutcome::FinalAnswer { .. }));
        // Never drained → no injection, no yield; the line stays queued for the caller
        // to re-route into a follow-up turn.
        assert!(!sink
            .yields()
            .iter()
            .any(|y| matches!(y, RuntimeYield::SteeringInjected { .. })));
        let leftovers: Vec<String> = steering
            .lock()
            .expect("steering lock")
            .iter()
            .cloned()
            .collect();
        assert_eq!(leftovers, vec!["too late".to_string()]);
    }

    /// Simulates a background subagent finishing while a tool runs: executing the tool
    /// registers a task in the shared registry and immediately completes it, exactly
    /// like the detached task spawned by spawn_subagent's background mode.
    struct BackgroundCompleteTool {
        tasks: holmes_core::background::BackgroundTasks,
        description: String,
        result: String,
    }

    #[async_trait]
    impl Tool for BackgroundCompleteTool {
        fn name(&self) -> &str {
            "background_complete"
        }

        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                tool_type: "function".into(),
                function: FunctionDefinition {
                    name: "background_complete".into(),
                    description: "registers and completes a background task mid-turn".into(),
                    parameters: Default::default(),
                },
            }
        }

        fn is_read_only(&self) -> bool {
            true
        }

        async fn execute(&self, _args: &str) -> Result<String> {
            let id = self.tasks.register(self.description.clone());
            self.tasks.complete(&id, Ok(self.result.clone()));
            Ok(format!("task started: {id}"))
        }
    }

    #[tokio::test]
    async fn run_turn_injects_finished_background_task_at_iteration_boundary() {
        // Iteration 1 runs a tool under which a background task completes; the
        // iteration-2 boundary drain must inject the outcome as a system-reminder,
        // persist it, and emit the yield — exactly once.
        let tasks = holmes_core::background::BackgroundTasks::new();
        let call = make_call("background_complete", "{}");
        let llm = Arc::new(QueueLlmBackend::new(vec![
            Ok(tool_response(call)),
            Ok(final_response("done")),
            Ok(final_response("again done")),
        ]));
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(BackgroundCompleteTool {
            tasks: tasks.clone(),
            description: "port scan".into(),
            result: "22/tcp open ssh".into(),
        }));
        let mut context = make_context(llm.clone(), tools, GuardChain::new()).await;
        context.set_background_tasks(tasks.clone());
        let mut runtime = AgentRuntime::new(context);
        let mut sink = VecSink::new();

        let outcome = runtime
            .run_turn("inspect", &mut sink)
            .await
            .expect("turn outcome");
        assert!(matches!(outcome, TurnOutcome::FinalAnswer { .. }));

        // The second LLM request carries the wrapped completion.
        let requests = llm.recorded_requests();
        assert_eq!(requests.len(), 2);
        let reminder = requests[1]
            .iter()
            .filter_map(|m| m.content.as_deref())
            .find(|c| c.contains("<system-reminder>Background task"))
            .expect("second request must include the background-task reminder");
        assert!(reminder.contains("\"port scan\""), "{reminder}");
        assert!(reminder.contains("completed"), "{reminder}");
        assert!(reminder.contains("22/tcp open ssh"), "{reminder}");

        // UI yield + persisted event.
        assert!(sink.yields().iter().any(|y| matches!(
            y,
            RuntimeYield::BackgroundTaskFinished { description, success: true, .. }
                if description == "port scan"
        )));
        let stored = runtime
            .context()
            .session_db
            .get_events(&runtime.context().session_id)
            .await
            .expect("stored events");
        assert!(stored.iter().any(|s| matches!(
            &s.event,
            Event::UserMessage { content, .. } if content.contains("<system-reminder>Background task")
        )));

        // A follow-up turn must NOT re-inject: the reminder rides along as plain
        // history, but no new injection event/yield appears.
        let outcome = runtime
            .run_turn("continue", &mut sink)
            .await
            .expect("second turn outcome");
        assert!(matches!(outcome, TurnOutcome::FinalAnswer { .. }));
        let finished_yields = sink
            .yields()
            .iter()
            .filter(|y| matches!(y, RuntimeYield::BackgroundTaskFinished { .. }))
            .count();
        assert_eq!(finished_yields, 1, "completion delivered exactly once");
        let stored = runtime
            .context()
            .session_db
            .get_events(&runtime.context().session_id)
            .await
            .expect("stored events");
        let reminder_events = stored
            .iter()
            .filter(|s| matches!(
                &s.event,
                Event::UserMessage { content, .. } if content.contains("<system-reminder>Background task")
            ))
            .count();
        assert_eq!(reminder_events, 1, "reminder persisted exactly once");
    }

    // P1-02: a durable task whose result was committed but never delivered
    // (crash between write-back and injection, or a scheduler-run re-execution)
    // is delivered from the DATABASE at the turn's first drain — before the
    // first LLM request — and marked delivered atomically.
    #[tokio::test]
    async fn run_turn_delivers_undelivered_durable_result_from_db() {
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
        // Seed a terminal, undelivered durable task for this session.
        let store = session_db.task_store();
        store
            .enqueue(holmes_session::task_store::NewTask {
                task_id: "durable-t".into(),
                parent_session_id: Some(session_id.clone()),
                kind: "subagent".into(),
                description: "port scan".into(),
                idempotency_key: None,
                safe_to_retry: false,
                payload: None,
            })
            .await
            .unwrap();
        let fencing = store
            .acquire_lease("durable-t")
            .await
            .unwrap()
            .unwrap()
            .attempt;
        store
            .complete("durable-t", fencing, "22/tcp open ssh")
            .await
            .unwrap();

        let llm = Arc::new(QueueLlmBackend::new(vec![Ok(final_response("done"))]));
        let memory_store = Arc::new(MemoryStore::open(":memory:").await.expect("memory store"));
        let mind_palace = MindPalace::new(session_db.clone(), memory_store.clone());
        let context = RuntimeContext::new(
            RuntimeSession::new(session_id.clone(), SessionMode::Pentest),
            session_db,
            memory_store,
            mind_palace,
            llm.clone(),
            Arc::new(ToolRegistry::new()),
            GuardChain::new(),
            RuntimeState::new(SessionMode::Pentest),
            HolmesConfig::default(),
        );
        let mut runtime = AgentRuntime::new(context);
        let mut sink = VecSink::new();

        let outcome = runtime
            .run_turn("go", &mut sink)
            .await
            .expect("turn outcome");
        assert!(matches!(outcome, TurnOutcome::FinalAnswer { .. }));

        // The very first LLM request already carries the delivered reminder.
        let requests = llm.recorded_requests();
        let reminder = requests[0]
            .iter()
            .filter_map(|m| m.content.as_deref())
            .find(|c| c.contains("<system-reminder>Background task"))
            .expect("first request must include the durable reminder");
        assert!(reminder.contains("durable-t"), "{reminder}");
        assert!(reminder.contains("22/tcp open ssh"), "{reminder}");

        // Delivered atomically: marked in the store, event persisted once,
        // in-memory history carries it (no duplicate on the next drain).
        let record = store.get("durable-t").await.unwrap().unwrap();
        assert!(
            record.delivered,
            "delivery marked atomically with the append"
        );
        assert!(sink.yields().iter().any(|y| matches!(
            y,
            RuntimeYield::BackgroundTaskFinished { task_id, success: true, .. } if task_id == "durable-t"
        )));
        let stored = runtime
            .context()
            .session_db
            .get_events(&session_id)
            .await
            .expect("stored events");
        let reminders = stored
            .iter()
            .filter(|s| matches!(
                &s.event,
                Event::UserMessage { content, .. } if content.contains("<system-reminder>Background task")
            ))
            .count();
        assert_eq!(reminders, 1, "durable reminder persisted exactly once");
    }

    /// Backend that finishes a background task DURING the only LLM call — after the
    /// last iteration-boundary drain — so only the turn-end drain can deliver it.
    struct LateBackgroundBackend {
        tasks: holmes_core::background::BackgroundTasks,
    }

    #[async_trait]
    impl LlmBackend for LateBackgroundBackend {
        async fn chat_completion(
            &self,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _role: &str,
        ) -> Result<LlmResponse> {
            let id = self.tasks.register("late recon".into());
            self.tasks.complete(&id, Ok("late result".into()));
            Ok(final_response("done"))
        }
    }

    #[tokio::test]
    async fn run_turn_end_drain_delivers_task_finished_during_final_llm_call() {
        let tasks = holmes_core::background::BackgroundTasks::new();
        let llm = Arc::new(LateBackgroundBackend {
            tasks: tasks.clone(),
        });
        let mut context = make_context(llm, ToolRegistry::new(), GuardChain::new()).await;
        context.set_background_tasks(tasks);
        let mut runtime = AgentRuntime::new(context);
        let mut sink = VecSink::new();

        let outcome = runtime
            .run_turn("go", &mut sink)
            .await
            .expect("turn outcome");
        assert!(matches!(outcome, TurnOutcome::FinalAnswer { .. }));

        // The turn-end drain still folds the completion into history (the next turn
        // sees it as a plain past message) and notifies the UI.
        assert!(sink.yields().iter().any(|y| matches!(
            y,
            RuntimeYield::BackgroundTaskFinished { description, .. } if description == "late recon"
        )));
        let last = runtime
            .context()
            .session
            .messages
            .last()
            .and_then(|m| m.content.as_deref())
            .expect("injected reminder message");
        assert!(last.contains("<system-reminder>Background task"), "{last}");
        assert!(last.contains("late result"), "{last}");
    }

    #[tokio::test]
    async fn background_task_completed_between_turns_is_injected_on_next_turn() {
        // A task still running when the turn ends stays in the shared registry;
        // whatever completes it afterwards is delivered by the next turn's first
        // drain — no polling, no lost results.
        let tasks = holmes_core::background::BackgroundTasks::new();
        let llm = Arc::new(QueueLlmBackend::new(vec![
            Ok(final_response("first")),
            Ok(final_response("second")),
        ]));
        let mut context = make_context(llm.clone(), ToolRegistry::new(), GuardChain::new()).await;
        context.set_background_tasks(tasks.clone());
        let task_id = tasks.register("slow recon".into());
        let mut runtime = AgentRuntime::new(context);
        let mut sink = VecSink::new();

        let outcome = runtime
            .run_turn("turn one", &mut sink)
            .await
            .expect("first turn outcome");
        assert!(matches!(outcome, TurnOutcome::FinalAnswer { .. }));
        assert!(!sink
            .yields()
            .iter()
            .any(|y| matches!(y, RuntimeYield::BackgroundTaskFinished { .. })));
        assert_eq!(
            tasks.running_count(),
            1,
            "task still running after turn one"
        );

        // The task finishes while no turn is running.
        tasks.complete(&task_id, Ok("between-turns result".into()));

        let outcome = runtime
            .run_turn("turn two", &mut sink)
            .await
            .expect("second turn outcome");
        assert!(matches!(outcome, TurnOutcome::FinalAnswer { .. }));
        let requests = llm.recorded_requests();
        assert_eq!(requests.len(), 2);
        let reminder = requests[1]
            .iter()
            .filter_map(|m| m.content.as_deref())
            .find(|c| c.contains("<system-reminder>Background task"))
            .expect("next turn's first request carries the completion");
        assert!(reminder.contains("\"slow recon\""), "{reminder}");
        assert!(reminder.contains("between-turns result"), "{reminder}");
        assert!(sink.yields().iter().any(|y| matches!(
            y,
            RuntimeYield::BackgroundTaskFinished { description, success: true, .. }
                if description == "slow recon"
        )));
    }

    #[tokio::test]
    async fn run_turn_can_set_runtime_goal_and_continue() {
        // With a standing goal, a plain-text answer is gated (P0-02): the model must
        // produce evidence first, and the semantic verifier (goal_evaluator role)
        // signs off before the answer completes the turn.
        let llm = Arc::new(QueueLlmBackend::new(vec![
            Ok(final_response(
                r#"<holmes_decision>{"type":"set_goal","condition":"validate the login behavior","reason":"Watson asked for a standing objective."}</holmes_decision>"#,
            )),
            Ok(tool_response(make_call("mock_tool", "{}"))),
            Ok(final_response("Goal is active.")),
            Ok(verifier_satisfied_response()),
        ]));
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(MockTool));
        let context = make_context(llm, tools, GuardChain::new()).await;
        let mut runtime = AgentRuntime::new(context);
        let mut sink = VecSink::new();

        let outcome = runtime
            .run_turn("keep working until login is understood", &mut sink)
            .await
            .expect("turn outcome");

        assert_eq!(
            outcome,
            TurnOutcome::FinalAnswer {
                content: "Goal is active.".into(),
                iterations: 3,
            }
        );
        assert_eq!(
            runtime.context().state.active_goal.as_deref(),
            Some("validate the login behavior")
        );
        let session = runtime
            .context()
            .session_db
            .get_session(&runtime.context().session_id)
            .await
            .expect("session")
            .expect("session exists");
        assert_eq!(
            session.goal_condition.as_deref(),
            Some("validate the login behavior")
        );
        assert!(sink.yields().iter().any(|event| matches!(
            event,
            RuntimeYield::PlanUpdate { content } if content.contains("Goal set")
        )));
    }

    #[tokio::test]
    async fn finish_decision_marks_active_goal_achieved() {
        let llm = Arc::new(QueueLlmBackend::new(vec![Ok(final_response(
            r#"<holmes_decision>{"type":"finish","summary":"Goal completed with supporting evidence."}</holmes_decision>"#,
        ))]));
        let mut context = make_context(llm, ToolRegistry::new(), GuardChain::new()).await;
        context.state.active_goal = Some("validate behavior".into());
        context
            .session_db
            .set_goal_condition(&context.session_id, Some("validate behavior"))
            .await
            .expect("set goal");
        let mut runtime = AgentRuntime::new(context);
        let mut sink = VecSink::new();

        let outcome = runtime
            .run_turn("finish when complete", &mut sink)
            .await
            .expect("turn outcome");

        assert_eq!(
            outcome,
            TurnOutcome::FinalAnswer {
                content: "Goal completed with supporting evidence.".into(),
                iterations: 1,
            }
        );
        let session = runtime
            .context()
            .session_db
            .get_session(&runtime.context().session_id)
            .await
            .expect("session")
            .expect("session exists");
        assert!(session.goal_achieved);

        let events = runtime
            .context()
            .session_db
            .get_events(&runtime.context().session_id)
            .await
            .expect("events");
        assert!(events.iter().any(|event| matches!(
            event.event,
            Event::GoalEvaluated {
                satisfied: true,
                ..
            }
        )));
    }

    /// P0-02 core regression: on an action-classified task, a plain-text answer
    /// without the required tool work must NOT complete the turn — the unified gate
    /// rejects it, execution continues, and only the evidence-backed answer passes.
    #[tokio::test]
    async fn plain_answer_on_action_task_is_gated_until_evidence_exists() {
        let llm = Arc::new(QueueLlmBackend::new(vec![
            // 1. Model tries to end the task with plain text, no tool ever ran.
            Ok(final_response("example.test is reachable.")),
            // 2. After the rejection feedback it does the actual probe.
            Ok(tool_response(make_call(
                "mock_tool",
                r#"{"target":"example.test"}"#,
            ))),
            // 3. And answers again, now backed by recorded evidence.
            Ok(final_response("Reachability confirmed by probe.")),
            // 4. Consumed by the semantic verifier (goal_evaluator role).
            Ok(verifier_satisfied_response()),
        ]));
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(MockTool));
        let context = make_context(llm, tools, GuardChain::new()).await;
        let mut runtime = AgentRuntime::new(context);
        let mut sink = VecSink::new();

        let outcome = runtime
            .run_turn("Confirm example.test is reachable.", &mut sink)
            .await
            .expect("turn outcome");

        assert_eq!(
            outcome,
            TurnOutcome::FinalAnswer {
                content: "Reachability confirmed by probe.".into(),
                iterations: 3,
            }
        );
        // The premature plain-text answer was never emitted as a final answer.
        let final_answers: Vec<_> = sink
            .yields()
            .into_iter()
            .filter_map(|y| match y {
                RuntimeYield::FinalAnswer { content, .. } => Some(content),
                _ => None,
            })
            .collect();
        assert_eq!(final_answers, vec!["Reachability confirmed by probe."]);

        let events = runtime
            .context()
            .session_db
            .get_events(&runtime.context().session_id)
            .await
            .expect("events");
        let evaluations: Vec<(bool, String)> = events
            .iter()
            .filter_map(|event| match &event.event {
                Event::GoalEvaluated {
                    satisfied, reason, ..
                } => Some((*satisfied, reason.clone())),
                _ => None,
            })
            .collect();
        // First the gated rejection (contract requirement gap), then the verified pass.
        assert_eq!(evaluations.len(), 2, "evaluations: {evaluations:?}");
        assert!(!evaluations[0].0);
        assert!(evaluations[0].1.contains("req-1"), "{}", evaluations[0].1);
        assert!(evaluations[1].0);
        assert!(evaluations[1].1.contains("verified; evidence"));
    }

    /// P0-02 protocol error: finish mixed with an executable tool call in one
    /// response is rejected wholesale — the tool is NOT silently dropped nor
    /// executed — and the model recovers by re-emitting the parts separately.
    #[tokio::test]
    async fn mixed_finish_and_tool_call_is_rejected_and_recoverable() {
        let mixed = LlmResponse {
            content: None,
            tool_calls: vec![
                make_call("mock_tool", r#"{"target":"example.test"}"#),
                ToolCall {
                    id: "call-finish".into(),
                    call_type: "function".into(),
                    function: FunctionCall {
                        name: "finish".into(),
                        arguments: r#"{"summary":"done","conclusion_refs":[],"remaining_hypothesis_ids":[]}"#.into(),
                    },
                },
            ],
            finish_reason: Some("tool_calls".into()),
            usage: None,
            ..Default::default()
        };
        let clean_finish = LlmResponse {
            content: None,
            tool_calls: vec![ToolCall {
                id: "call-finish-2".into(),
                call_type: "function".into(),
                function: FunctionCall {
                    name: "finish".into(),
                    arguments: r#"{"summary":"Reachability confirmed by probe.","conclusion_refs":[],"remaining_hypothesis_ids":[]}"#.into(),
                },
            }],
            finish_reason: Some("tool_calls".into()),
            usage: None,
            ..Default::default()
        };
        let llm = Arc::new(QueueLlmBackend::new(vec![
            Ok(mixed),
            Ok(tool_response(make_call(
                "mock_tool",
                r#"{"target":"example.test"}"#,
            ))),
            Ok(clean_finish),
            // Consumed by the semantic verifier.
            Ok(verifier_satisfied_response()),
        ]));
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(MockTool));
        let context = make_context(llm, tools, GuardChain::new()).await;
        let mut runtime = AgentRuntime::new(context);
        let mut sink = VecSink::new();

        let outcome = runtime
            .run_turn("Confirm example.test is reachable.", &mut sink)
            .await
            .expect("turn outcome");

        assert_eq!(
            outcome,
            TurnOutcome::FinalAnswer {
                content: "Reachability confirmed by probe.".into(),
                iterations: 3,
            }
        );
        let events = runtime
            .context()
            .session_db
            .get_events(&runtime.context().session_id)
            .await
            .expect("events");
        // The mixed response executed nothing: exactly one tool call ran (the
        // re-issued one), and the violation was fed back as a supervisor note.
        let tool_calls = events
            .iter()
            .filter(|event| matches!(event.event, Event::ToolCall { .. }))
            .count();
        assert_eq!(tool_calls, 1, "mixed response must not execute tools");
        assert!(events.iter().any(|event| matches!(
            &event.event,
            Event::UserMessage { content, .. } if content.contains("Protocol violation")
        )));
        assert!(events.iter().any(|event| matches!(
            event.event,
            Event::GoalEvaluated {
                satisfied: true,
                ..
            }
        )));
    }

    /// P1-01 test double: a silent provider — every LLM call pends forever.
    struct SilentLlmBackend;

    #[async_trait]
    impl LlmBackend for SilentLlmBackend {
        async fn chat_completion(
            &self,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _role: &str,
        ) -> Result<LlmResponse> {
            std::future::pending().await
        }
    }

    /// P1-01 test double: serves the queued responses but the semantic completion
    /// verifier call (goal_evaluator role) pends forever — a silent verifier.
    struct HangingVerifierBackend {
        responses: Mutex<VecDeque<LlmResponse>>,
    }

    #[async_trait]
    impl LlmBackend for HangingVerifierBackend {
        async fn chat_completion(
            &self,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            role: &str,
        ) -> Result<LlmResponse> {
            if role == "goal_evaluator" {
                std::future::pending().await
            }
            Ok(self
                .responses
                .lock()
                .expect("responses lock")
                .pop_front()
                .expect("queued response"))
        }
    }

    fn esc_after(runtime_context_cancel: Arc<AtomicBool>, delay: std::time::Duration) {
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            runtime_context_cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        });
    }

    /// P1-01 acceptance: Esc interrupts an in-flight (silent) LLM deliberation —
    /// the turn ends as Interrupted within the cancellation SLO instead of waiting
    /// out the provider.
    #[tokio::test]
    async fn esc_interrupts_an_in_flight_deliberation() {
        let context = make_context(
            Arc::new(SilentLlmBackend),
            ToolRegistry::new(),
            GuardChain::new(),
        )
        .await;
        let mut runtime = AgentRuntime::new(context);
        esc_after(
            runtime.context.cancel.clone(),
            std::time::Duration::from_millis(150),
        );
        let mut sink = VecSink::new();

        let start = std::time::Instant::now();
        let outcome = runtime
            .run_turn("investigate example.test", &mut sink)
            .await
            .expect("turn ends after Esc");

        assert!(
            matches!(outcome, TurnOutcome::Interrupted { .. }),
            "got {outcome:?}"
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "in-flight deliberation interrupted, took {:?}",
            start.elapsed()
        );
    }

    /// P1-01 acceptance: the absolute turn deadline ends an in-flight (silent) LLM
    /// deliberation — reported via the budget-exhaustion outcome with partial
    /// progress persisted.
    #[tokio::test]
    async fn turn_deadline_ends_an_in_flight_deliberation() {
        let mut config = HolmesConfig::default();
        config.execution.turn_deadline_ms = Some(300);
        let context = make_context_with_config(
            Arc::new(SilentLlmBackend),
            ToolRegistry::new(),
            GuardChain::new(),
            config,
        )
        .await;
        let mut runtime = AgentRuntime::new(context);
        let mut sink = VecSink::new();

        let start = std::time::Instant::now();
        let outcome = runtime
            .run_turn("investigate example.test", &mut sink)
            .await
            .expect("turn ends at the deadline");

        match outcome {
            TurnOutcome::MaxIterationsReached { message, .. } => {
                assert!(message.contains("turn deadline"), "got: {message}");
            }
            other => panic!("expected MaxIterationsReached, got {other:?}"),
        }
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "in-flight deliberation bounded by turn deadline, took {:?}",
            start.elapsed()
        );
    }

    /// P1-01 acceptance: Esc also interrupts an in-flight completion-verifier call
    /// (the semantic gate), not just deliberation.
    #[tokio::test]
    async fn esc_interrupts_an_in_flight_completion_verifier() {
        let finish = LlmResponse {
            content: None,
            tool_calls: vec![ToolCall {
                id: "call-finish".into(),
                call_type: "function".into(),
                function: FunctionCall {
                    name: "finish".into(),
                    arguments: r#"{"summary":"Reachability confirmed by probe.","conclusion_refs":[],"remaining_hypothesis_ids":[]}"#.into(),
                },
            }],
            finish_reason: Some("tool_calls".into()),
            usage: None,
            ..Default::default()
        };
        let llm = Arc::new(HangingVerifierBackend {
            responses: Mutex::new(
                vec![
                    tool_response(make_call("mock_tool", r#"{"target":"example.test"}"#)),
                    finish,
                ]
                .into(),
            ),
        });
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(MockTool));
        let context = make_context(llm, tools, GuardChain::new()).await;
        let mut runtime = AgentRuntime::new(context);
        esc_after(
            runtime.context.cancel.clone(),
            std::time::Duration::from_millis(500),
        );
        let mut sink = VecSink::new();

        let start = std::time::Instant::now();
        let outcome = runtime
            .run_turn("Confirm example.test is reachable.", &mut sink)
            .await
            .expect("turn ends after Esc");

        assert!(
            matches!(outcome, TurnOutcome::Interrupted { .. }),
            "got {outcome:?}"
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "in-flight verifier interrupted, took {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn run_turn_reviews_watson_correction_and_stages_memory() {
        let llm = Arc::new(QueueLlmBackend::new(vec![Ok(final_response(
            "I will remember that preference.",
        ))]));
        let mut config = HolmesConfig::default();
        config.compressor.enabled = false;
        config.learning.memory_write_approval = true;
        let context =
            make_context_with_config(llm, ToolRegistry::new(), GuardChain::new(), config).await;
        let mut runtime = AgentRuntime::new(context);
        let mut sink = VecSink::new();

        let outcome = runtime
            .run_turn(
                "Remember next time: we prefer HEAD requests before GET requests.",
                &mut sink,
            )
            .await
            .expect("turn outcome");

        assert_eq!(
            outcome,
            TurnOutcome::FinalAnswer {
                content: "I will remember that preference.".into(),
                iterations: 1,
            }
        );
        let events = runtime
            .context()
            .session_db
            .get_events(&runtime.context().session_id)
            .await
            .expect("events");
        assert!(events
            .iter()
            .any(|event| matches!(event.event, Event::LearningReviewStarted { .. })));
        assert!(events
            .iter()
            .any(|event| matches!(event.event, Event::MemoryWriteStaged { .. })));
        assert!(events
            .iter()
            .any(|event| matches!(event.event, Event::LearningReviewCompleted { .. })));
        assert!(matches!(
            events.last().map(|event| &event.event),
            Some(Event::TurnComplete { .. })
        ));
    }

    #[tokio::test]
    async fn learning_review_runs_only_every_interval_turns() {
        // review_interval_turns = 2: the first turn skips the review, the
        // second runs it.
        let llm = Arc::new(QueueLlmBackend::new(vec![
            Ok(final_response("Noted.")),
            Ok(final_response("Noted again.")),
        ]));
        let mut config = HolmesConfig::default();
        config.compressor.enabled = false;
        config.learning.review_interval_turns = 2;
        let context =
            make_context_with_config(llm, ToolRegistry::new(), GuardChain::new(), config).await;
        let mut runtime = AgentRuntime::new(context);
        let mut sink = VecSink::new();

        runtime
            .run_turn(
                "Remember next time: we prefer HEAD requests before GET requests.",
                &mut sink,
            )
            .await
            .expect("first turn");
        let events = runtime
            .context()
            .session_db
            .get_events(&runtime.context().session_id)
            .await
            .expect("events");
        assert!(
            !events
                .iter()
                .any(|event| matches!(event.event, Event::LearningReviewStarted { .. })),
            "interval 2 must skip the review on turn 1"
        );

        runtime
            .run_turn(
                "Remember next time: we prefer HEAD requests before GET requests.",
                &mut sink,
            )
            .await
            .expect("second turn");
        let events = runtime
            .context()
            .session_db
            .get_events(&runtime.context().session_id)
            .await
            .expect("events");
        let reviews = events
            .iter()
            .filter(|event| matches!(event.event, Event::LearningReviewStarted { .. }))
            .count();
        assert_eq!(reviews, 1, "interval 2 reviews exactly the 2nd turn");
    }

    #[tokio::test]
    async fn learning_review_interval_survives_per_turn_runtime_rebuild() {
        // P2-03: the production assembly (`run_runtime_input_with_sink` in
        // holmes-cli) builds a FRESH `AgentRuntime` for every user turn over
        // the same session store, carrying only the session / mind palace /
        // runtime state forward. An in-memory review counter therefore never
        // got past 1 and `review_interval_turns > 1` never fired in the real
        // CLI; the cadence must come from the authoritative event log. This
        // test reproduces that assembly: two turns, two runtimes, one store.
        let session_db = Arc::new(SessionDB::open(":memory:").await.expect("session db"));
        let session_id = "session-rebuild".to_string();
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
        let llm: Arc<dyn LlmBackend> = Arc::new(QueueLlmBackend::new(vec![
            Ok(final_response("Noted.")),
            Ok(final_response("Noted again.")),
        ]));
        let mut config = HolmesConfig::default();
        config.compressor.enabled = false;
        config.cognition.mode = holmes_core::ledger::ThinkMode::Fast;
        config.learning.review_interval_turns = 2;
        let registry = Arc::new(ToolRegistry::new());

        let build_context = |session: RuntimeSession,
                             mind_palace: MindPalace,
                             state: RuntimeState,
                             llm: Arc<dyn LlmBackend>| {
            RuntimeContext::new(
                session,
                session_db.clone(),
                memory_store.clone(),
                mind_palace,
                llm,
                registry.clone(),
                GuardChain::new(),
                state,
                config.clone(),
            )
        };

        // Turn 1 with runtime #1: interval 2 must skip the review.
        let mut runtime = AgentRuntime::new(build_context(
            RuntimeSession::new(session_id.clone(), SessionMode::Pentest),
            MindPalace::new(session_db.clone(), memory_store.clone()),
            RuntimeState::new(SessionMode::Pentest),
            llm.clone(),
        ));
        let mut sink = VecSink::new();
        runtime
            .run_turn(
                "Remember next time: we prefer HEAD requests before GET requests.",
                &mut sink,
            )
            .await
            .expect("first turn");
        let carried = runtime.into_context();
        let events = session_db.get_events(&session_id).await.expect("events");
        assert!(
            !events
                .iter()
                .any(|event| matches!(event.event, Event::LearningReviewStarted { .. })),
            "interval 2 must skip the review on turn 1"
        );

        // Turn 2 with a FRESH runtime #2 over the carried-forward pieces —
        // the exact production wiring that reset the old in-memory counter.
        let mut runtime = AgentRuntime::new(build_context(
            carried.session,
            carried.mind_palace,
            carried.state,
            llm.clone(),
        ));
        let mut sink = VecSink::new();
        runtime
            .run_turn(
                "Remember next time: we prefer HEAD requests before GET requests.",
                &mut sink,
            )
            .await
            .expect("second turn");
        let events = session_db.get_events(&session_id).await.expect("events");
        let reviews = events
            .iter()
            .filter(|event| matches!(event.event, Event::LearningReviewStarted { .. }))
            .count();
        assert_eq!(
            reviews, 1,
            "interval 2 must run the review on the 2nd turn even across a runtime rebuild"
        );
    }

    #[tokio::test]
    async fn run_turn_stops_at_iteration_budget() {
        let call = make_call("mock_tool", "{}");
        let llm = Arc::new(QueueLlmBackend::new(vec![Ok(tool_response(call))]));
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(MockTool));
        let mut config = HolmesConfig::default();
        config.agent.max_iterations = 1;
        let context = make_context_with_config(llm, tools, GuardChain::new(), config).await;
        let mut runtime = AgentRuntime::new(context);
        let mut sink = VecSink::new();

        let outcome = runtime
            .run_turn("inspect", &mut sink)
            .await
            .expect("max iteration outcome");

        assert!(matches!(outcome, TurnOutcome::MaxIterationsReached { .. }));
        assert!(matches!(
            sink.yields().last().cloned(),
            Some(RuntimeYield::Error { .. })
        ));
    }

    #[tokio::test]
    async fn manual_runtime_compaction_records_event() {
        let llm = Arc::new(QueueLlmBackend::new(Vec::new()));
        let mut config = HolmesConfig::default();
        config.compressor.context_limit = 1;
        config.compressor.threshold = 1.0;
        config.compressor.protected_head = 1;
        config.compressor.protect_last_n = 1;
        let context =
            make_context_with_config(llm, ToolRegistry::new(), GuardChain::new(), config).await;
        let mut runtime = AgentRuntime::new(context);

        runtime
            .context_mut()
            .session
            .messages
            .push(Message::system("system prompt"));
        runtime
            .context_mut()
            .session
            .messages
            .push(Message::user("old finding one"));
        runtime
            .context_mut()
            .session
            .messages
            .push(Message::assistant("old reasoning two"));
        runtime
            .context_mut()
            .session
            .messages
            .push(Message::user("latest question"));

        let result = runtime.compact_now().await.expect("compact runtime");

        assert!(result.is_some());
        let events = runtime
            .context()
            .session_db
            .get_events(&runtime.context().session_id)
            .await
            .expect("stored events");
        assert!(events
            .iter()
            .any(|event| matches!(event.event, Event::CompressionApplied { .. })));
    }

    #[tokio::test]
    async fn llm_summary_compaction_uses_model_summary() {
        // The compressor role call is served this scripted summary.
        let llm = Arc::new(QueueLlmBackend::new(vec![Ok(final_response(
            "SUMMARY: confirmed IDOR at /api/users; creds admin:hunter2; next: escalate.",
        ))]));
        let mut config = HolmesConfig::default();
        config.compressor.context_limit = 1;
        config.compressor.threshold = 1.0;
        config.compressor.protected_head = 1;
        config.compressor.protect_last_n = 1;
        config.compressor.llm_summary = true;
        // Keep this test focused on the summary path: the flush would consume an extra
        // scripted response.
        config.compressor.pre_compact_flush = false;
        let context =
            make_context_with_config(llm, ToolRegistry::new(), GuardChain::new(), config).await;
        let mut runtime = AgentRuntime::new(context);
        for msg in [
            Message::system("system prompt"),
            Message::user("old finding one"),
            Message::assistant("old reasoning two"),
            Message::user("latest question"),
        ] {
            runtime.context_mut().session.messages.push(msg);
        }

        let result = runtime
            .compact_now()
            .await
            .expect("compact runtime")
            .expect("compressed");

        assert!(matches!(
            result.method,
            holmes_core::CompressionMethod::LlmSummary
        ));
        assert!(result.summary.contains("confirmed IDOR"));
    }

    /// llm_summary config with a tiny window: threshold at 750/1000 tokens, prefire
    /// window opens at 650.
    fn prefire_test_config() -> HolmesConfig {
        let mut config = HolmesConfig::default();
        config.compressor.context_limit = 1000;
        config.compressor.threshold = 0.75;
        config.compressor.protected_head = 1;
        config.compressor.protect_last_n = 1;
        config.compressor.llm_summary = true;
        config
    }

    fn prefire_test_messages() -> Vec<Message> {
        vec![
            Message::system("system prompt"),
            Message::user("old finding one"),
            Message::assistant("old reasoning two"),
            Message::user("latest question"),
        ]
    }

    #[tokio::test]
    async fn prefire_reused_when_span_unchanged_means_zero_sync_summary_call() {
        // Call sequence must be exactly: background prefire summary + pre-compaction
        // flush. The compaction itself reuses the prefire result (no third LLM call).
        let llm = Arc::new(QueueLlmBackend::new(vec![
            Ok(final_response("PREFIRE: confirmed IDOR at /api/users")),
            Ok(final_response("## Access & credentials\n- admin:hunter2")),
        ]));
        let context = make_context_with_config(
            llm.clone(),
            ToolRegistry::new(),
            GuardChain::new(),
            prefire_test_config(),
        )
        .await;
        let mut runtime = AgentRuntime::new(context);
        for msg in prefire_test_messages() {
            runtime.context_mut().session.messages.push(msg);
        }

        // In the prefire window (650..750): no compaction, but a prefire spawns.
        runtime.last_prompt_tokens = 700;
        assert!(runtime
            .maybe_compact()
            .await
            .expect("maybe compact")
            .is_none());
        assert!(runtime.prefire.is_some(), "prefire should have spawned");

        // Crossing the threshold with the message span unchanged → prefire reuse
        // (take_matching_prefire awaits the in-flight task, so ordering is
        // deterministic: prefire response consumed first, then the flush call).
        runtime.last_prompt_tokens = 800;
        let result = runtime
            .maybe_compact()
            .await
            .expect("maybe compact")
            .expect("compressed");

        assert!(matches!(
            result.method,
            holmes_core::CompressionMethod::LlmSummary
        ));
        assert!(
            result.summary.contains("PREFIRE: confirmed IDOR"),
            "prefire summary reused: {}",
            result.summary
        );
        assert!(
            result.summary.contains("## Preserved critical state")
                && result.summary.contains("admin:hunter2"),
            "flush notes folded into summary head: {}",
            result.summary
        );
        assert_eq!(
            llm.recorded_requests().len(),
            2,
            "exactly prefire + flush calls, no synchronous summary call"
        );
        assert!(runtime.prefire.is_none(), "prefire slot consumed");

        // The flush note is also in long-term memory (recallable in later turns).
        let recalled = runtime
            .context()
            .memory_store
            .search("admin:hunter2", 3)
            .await
            .expect("memory search");
        assert_eq!(recalled.len(), 1, "flush note must be searchable in memory");
    }

    #[tokio::test]
    async fn prefire_discarded_on_fingerprint_mismatch_falls_back_to_sync_summary() {
        // Call order: background prefire, then (on fingerprint mismatch) the synchronous
        // summary, then the flush.
        let llm = Arc::new(QueueLlmBackend::new(vec![
            Ok(final_response("PREFIRE: stale summary")),
            Ok(final_response("SYNC: fresh summary with the new messages")),
            Ok(final_response(
                "## Confirmed findings\n- stored xss at /comment",
            )),
        ]));
        let context = make_context_with_config(
            llm.clone(),
            ToolRegistry::new(),
            GuardChain::new(),
            prefire_test_config(),
        )
        .await;
        let mut runtime = AgentRuntime::new(context);
        for msg in prefire_test_messages() {
            runtime.context_mut().session.messages.push(msg);
        }

        runtime.last_prompt_tokens = 700;
        assert!(runtime
            .maybe_compact()
            .await
            .expect("maybe compact")
            .is_none());

        // A new message arrives before the threshold is crossed → the prefire span no
        // longer matches the compaction span → discard + synchronous summary.
        runtime
            .context_mut()
            .session
            .messages
            .push(Message::assistant("brand new step"));
        runtime.last_prompt_tokens = 800;
        let result = runtime
            .maybe_compact()
            .await
            .expect("maybe compact")
            .expect("compressed");

        assert!(
            result.summary.contains("SYNC: fresh summary"),
            "sync fallback summary used: {}",
            result.summary
        );
        assert!(
            !result.summary.contains("PREFIRE: stale summary"),
            "stale prefire must be discarded: {}",
            result.summary
        );
        assert_eq!(
            llm.recorded_requests().len(),
            3,
            "prefire + flush + sync summary"
        );
    }

    #[tokio::test]
    async fn prefire_not_spawned_below_window() {
        let llm = Arc::new(QueueLlmBackend::new(Vec::new()));
        let context = make_context_with_config(
            llm.clone(),
            ToolRegistry::new(),
            GuardChain::new(),
            prefire_test_config(),
        )
        .await;
        let mut runtime = AgentRuntime::new(context);
        for msg in prefire_test_messages() {
            runtime.context_mut().session.messages.push(msg);
        }

        // 600 < 650 (threshold - 0.10): outside the prefire window.
        runtime.last_prompt_tokens = 600;
        assert!(runtime
            .maybe_compact()
            .await
            .expect("maybe compact")
            .is_none());
        assert!(runtime.prefire.is_none(), "no prefire below the window");
        assert!(llm.recorded_requests().is_empty(), "no LLM calls at all");
    }

    #[tokio::test]
    async fn prefire_never_spawns_in_static_template_mode() {
        // Static mode (default) makes zero LLM calls across the whole compaction
        // lifecycle — this is what keeps scripted harness replays intact.
        let llm = Arc::new(QueueLlmBackend::new(Vec::new()));
        let mut config = prefire_test_config();
        config.compressor.llm_summary = false;
        let context =
            make_context_with_config(llm.clone(), ToolRegistry::new(), GuardChain::new(), config)
                .await;
        let mut runtime = AgentRuntime::new(context);
        for msg in prefire_test_messages() {
            runtime.context_mut().session.messages.push(msg);
        }

        runtime.last_prompt_tokens = 700;
        assert!(runtime
            .maybe_compact()
            .await
            .expect("maybe compact")
            .is_none());
        assert!(runtime.prefire.is_none(), "static mode never prefires");

        runtime.last_prompt_tokens = 800;
        let result = runtime
            .maybe_compact()
            .await
            .expect("maybe compact")
            .expect("compressed");
        assert!(matches!(
            result.method,
            holmes_core::CompressionMethod::StaticFallback
        ));
        assert!(
            llm.recorded_requests().is_empty(),
            "static path must not touch the LLM (harness replay safety)"
        );
    }

    #[tokio::test]
    async fn flush_failure_does_not_block_compaction() {
        // Summary succeeds, then the flush errors: compaction must still land, just
        // without the preserved-state section. (Call order: summary first, flush second.)
        let llm = Arc::new(QueueLlmBackend::new(vec![
            Ok(final_response("SYNC: summary after failed flush")),
            Err("flush boom".to_string()),
        ]));
        let context = make_context_with_config(
            llm.clone(),
            ToolRegistry::new(),
            GuardChain::new(),
            prefire_test_config(),
        )
        .await;
        let mut runtime = AgentRuntime::new(context);
        for msg in prefire_test_messages() {
            runtime.context_mut().session.messages.push(msg);
        }

        // Straight over the threshold (no prefire in flight).
        runtime.last_prompt_tokens = 800;
        let result = runtime
            .maybe_compact()
            .await
            .expect("maybe compact")
            .expect("compressed despite flush failure");

        assert!(matches!(
            result.method,
            holmes_core::CompressionMethod::LlmSummary
        ));
        assert!(result.summary.contains("SYNC: summary after failed flush"));
        assert!(
            !result.summary.contains("## Preserved critical state"),
            "no preserved section when the flush failed: {}",
            result.summary
        );
        assert_eq!(
            llm.recorded_requests().len(),
            2,
            "flush attempt + sync summary"
        );
    }

    #[tokio::test]
    async fn flush_disabled_by_config_skips_call_and_section() {
        let llm = Arc::new(QueueLlmBackend::new(vec![Ok(final_response(
            "SYNC: summary only",
        ))]));
        let mut config = prefire_test_config();
        config.compressor.pre_compact_flush = false;
        let context =
            make_context_with_config(llm.clone(), ToolRegistry::new(), GuardChain::new(), config)
                .await;
        let mut runtime = AgentRuntime::new(context);
        for msg in prefire_test_messages() {
            runtime.context_mut().session.messages.push(msg);
        }

        runtime.last_prompt_tokens = 800;
        let result = runtime
            .maybe_compact()
            .await
            .expect("maybe compact")
            .expect("compressed");

        assert!(result.summary.contains("SYNC: summary only"));
        assert!(!result.summary.contains("## Preserved critical state"));
        assert_eq!(llm.recorded_requests().len(), 1, "summary call only");
    }

    #[tokio::test]
    async fn manual_compaction_persists_archive_backed_event() {
        let llm = Arc::new(QueueLlmBackend::new(Vec::new()));
        let mut config = HolmesConfig::default();
        config.compressor.context_limit = 1;
        config.compressor.threshold = 1.0;
        config.compressor.protected_head = 1;
        config.compressor.protect_last_n = 1;
        let context =
            make_context_with_config(llm, ToolRegistry::new(), GuardChain::new(), config).await;
        let mut runtime = AgentRuntime::new(context);

        runtime
            .context_mut()
            .session
            .messages
            .push(Message::system("system prompt"));
        runtime
            .context_mut()
            .session
            .messages
            .push(Message::user("old finding one"));
        runtime
            .context_mut()
            .session
            .messages
            .push(Message::assistant("old reasoning two"));
        runtime
            .context_mut()
            .session
            .messages
            .push(Message::user("latest question"));

        let result = runtime
            .compact_now()
            .await
            .expect("compact runtime")
            .expect("compressed");
        assert_eq!(result.trigger, holmes_core::CompactionTrigger::Manual);
        let archive_path = result.archive_path.clone().expect("archive path");

        let events = runtime
            .context()
            .session_db
            .get_events(&runtime.context().session_id)
            .await
            .expect("stored events");
        let compression = events
            .iter()
            .find_map(|event| match &event.event {
                Event::CompressionApplied {
                    trigger,
                    archive_path,
                    ..
                } => Some((trigger.clone(), archive_path.clone())),
                _ => None,
            })
            .expect("compression event");
        assert_eq!(compression.0, Some(holmes_core::CompactionTrigger::Manual));
        assert_eq!(compression.1, Some(archive_path.clone()));

        let archive = runtime
            .context()
            .session_db
            .read_compaction_archive(&archive_path)
            .await
            .expect("readable archive");
        assert_eq!(archive.trigger, holmes_core::CompactionTrigger::Manual);
        assert_eq!(
            archive.schema_version,
            holmes_session::COMPACTION_ARCHIVE_SCHEMA_VERSION
        );
        assert!(!archive.messages.is_empty());
    }

    #[tokio::test]
    async fn run_turn_auto_compacts_before_deliberation() {
        let llm = Arc::new(QueueLlmBackend::new(vec![Ok(final_response("done"))]));
        let mut config = HolmesConfig::default();
        config.compressor.context_limit = 20;
        config.compressor.threshold = 0.5;
        config.compressor.protected_head = 1;
        config.compressor.protect_last_n = 1;
        let context =
            make_context_with_config(llm, ToolRegistry::new(), GuardChain::new(), config).await;
        let mut runtime = AgentRuntime::new(context);
        runtime
            .context_mut()
            .session
            .messages
            .push(Message::system("system prompt"));
        runtime.context_mut().session.messages.push(Message::user(
            "old reconnaissance notes with enough detail to cross the tiny compressor threshold",
        ));
        runtime
            .context_mut()
            .session
            .messages
            .push(Message::assistant(
                "old reasoning and observations that should be summarized before deliberation",
            ));
        let mut sink = VecSink::new();

        runtime
            .run_turn("continue", &mut sink)
            .await
            .expect("turn outcome");

        assert!(matches!(
            sink.yields().first().cloned(),
            Some(RuntimeYield::CompactionBoundary {
                before_count: 4,
                after_count: 3,
                method,
                ..
            }) if method == "static_fallback"
        ));
        assert!(matches!(
            sink.yields().last().cloned(),
            Some(RuntimeYield::FinalAnswer { content, .. }) if content == "done"
        ));
        let events = runtime
            .context()
            .session_db
            .get_events(&runtime.context().session_id)
            .await
            .expect("stored events");
        assert!(events
            .iter()
            .any(|event| matches!(event.event, Event::CompressionApplied { .. })));
    }

    #[tokio::test]
    async fn repeated_auto_compaction_only_compacts_once_per_turn() {
        let call = make_call("mock_tool", "{}");
        let llm = Arc::new(QueueLlmBackend::new(vec![
            Ok(tool_response(call)),
            Ok(final_response("done")),
        ]));
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(MockTool));
        let mut config = HolmesConfig::default();
        config.compressor.context_limit = 20;
        config.compressor.threshold = 0.5;
        config.compressor.protected_head = 1;
        config.compressor.protect_last_n = 1;
        let context = make_context_with_config(llm, tools, GuardChain::new(), config).await;
        let mut runtime = AgentRuntime::new(context);
        runtime
            .context_mut()
            .session
            .messages
            .push(Message::system("system prompt"));
        runtime.context_mut().session.messages.push(Message::user(
            "old reconnaissance notes with enough detail to cross the tiny compressor threshold",
        ));
        runtime
            .context_mut()
            .session
            .messages
            .push(Message::assistant(
                "old reasoning and observations that should be summarized before deliberation",
            ));
        let mut sink = VecSink::new();

        let outcome = runtime
            .run_turn("continue", &mut sink)
            .await
            .expect("turn outcome");

        assert_eq!(
            outcome,
            TurnOutcome::FinalAnswer {
                content: "done".into(),
                iterations: 2,
            }
        );
        let events = runtime
            .context()
            .session_db
            .get_events(&runtime.context().session_id)
            .await
            .expect("stored events");
        let compression_events = events
            .iter()
            .filter(|event| matches!(event.event, Event::CompressionApplied { .. }))
            .count();
        assert_eq!(compression_events, 1);
    }

    #[tokio::test]
    async fn run_turn_compacts_and_retries_once_on_context_overflow() {
        let llm = Arc::new(QueueLlmBackend::new(vec![
            Err("context length exceeded maximum context window".into()),
            Ok(final_response("recovered")),
        ]));
        let mut config = HolmesConfig::default();
        config.compressor.context_limit = 1;
        config.compressor.threshold = 1.0;
        config.compressor.protected_head = 1;
        config.compressor.protect_last_n = 1;
        let context =
            make_context_with_config(llm, ToolRegistry::new(), GuardChain::new(), config).await;
        let mut runtime = AgentRuntime::new(context);
        // Seed enough messages so the forced overflow compaction yields a smaller context.
        runtime
            .context_mut()
            .session
            .messages
            .push(Message::system("system prompt"));
        runtime
            .context_mut()
            .session
            .messages
            .push(Message::user("old finding one"));
        runtime
            .context_mut()
            .session
            .messages
            .push(Message::assistant("old reasoning two"));
        runtime
            .context_mut()
            .session
            .messages
            .push(Message::user("latest question"));
        let mut sink = VecSink::new();

        let outcome = runtime
            .run_turn("continue", &mut sink)
            .await
            .expect("turn outcome");

        assert!(matches!(outcome, TurnOutcome::FinalAnswer { .. }));

        let events = runtime
            .context()
            .session_db
            .get_events(&runtime.context().session_id)
            .await
            .expect("stored events");
        let overflow_compactions = events
            .iter()
            .filter(|event| {
                matches!(
                    &event.event,
                    Event::CompressionApplied {
                        trigger: Some(holmes_core::CompactionTrigger::Overflow),
                        ..
                    }
                )
            })
            .count();
        assert_eq!(overflow_compactions, 1);
    }

    struct QueueLlmBackend {
        responses: Mutex<VecDeque<std::result::Result<LlmResponse, String>>>,
        requests: Mutex<Vec<Vec<Message>>>,
    }

    impl QueueLlmBackend {
        fn new(responses: Vec<std::result::Result<LlmResponse, String>>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                requests: Mutex::new(Vec::new()),
            }
        }

        /// Every message list the backend was called with, in call order.
        fn recorded_requests(&self) -> Vec<Vec<Message>> {
            self.requests.lock().expect("requests lock").clone()
        }
    }

    #[async_trait]
    impl LlmBackend for QueueLlmBackend {
        async fn chat_completion(
            &self,
            messages: &[Message],
            _tools: &[ToolDefinition],
            _role: &str,
        ) -> Result<LlmResponse> {
            self.requests
                .lock()
                .expect("requests lock")
                .push(messages.to_vec());
            match self.responses.lock().expect("responses lock").pop_front() {
                Some(Ok(response)) => Ok(response),
                Some(Err(message)) => anyhow::bail!(message),
                None => anyhow::bail!("no queued response"),
            }
        }
    }

    struct MockTool;

    #[async_trait]
    impl Tool for MockTool {
        fn name(&self) -> &str {
            "mock_tool"
        }

        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                tool_type: "function".into(),
                function: FunctionDefinition {
                    name: "mock_tool".into(),
                    description: "mock tool".into(),
                    parameters: Default::default(),
                },
            }
        }

        fn is_read_only(&self) -> bool {
            true
        }

        async fn execute(&self, _args: &str) -> Result<String> {
            Ok("mock output".into())
        }
    }

    struct ServicePostGuard;

    #[async_trait]
    impl PostGuard for ServicePostGuard {
        fn name(&self) -> &str {
            "service"
        }

        async fn process(
            &mut self,
            _call: &ToolCall,
            _result: &ToolResult,
            state: &mut AttackState,
        ) {
            state.attack_surface_mut().ports.push(PortInfo {
                port: 443,
                service: "https".into(),
                version: "nginx".into(),
            });
        }
    }

    struct BlockGuard;

    #[async_trait]
    impl PreGuard for BlockGuard {
        fn name(&self) -> &str {
            "block"
        }

        async fn check(&self, _call: &ToolCall, _state: &AttackState) -> GuardVerdict {
            GuardVerdict::block("blocked by test")
        }
    }

    async fn make_context(
        llm: Arc<dyn LlmBackend>,
        tools: ToolRegistry,
        guards: GuardChain,
    ) -> RuntimeContext {
        make_context_with_config(llm, tools, guards, HolmesConfig::default()).await
    }

    async fn make_context_with_config(
        llm: Arc<dyn LlmBackend>,
        tools: ToolRegistry,
        guards: GuardChain,
        mut config: HolmesConfig,
    ) -> RuntimeContext {
        // Legacy runtime tests script one response per iteration. Keep those
        // fixtures on the explicit fast path; CognitiveEngine has dedicated
        // multi-pass tests with Proposal/Critique/Commit response queues.
        config.cognition.mode = holmes_core::ledger::ThinkMode::Fast;
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
        let mind_palace = MindPalace::new(session_db.clone(), memory_store.clone());

        RuntimeContext::new(
            RuntimeSession::new(session_id, SessionMode::Pentest),
            session_db,
            memory_store,
            mind_palace,
            llm,
            Arc::new(tools),
            guards,
            RuntimeState::new(SessionMode::Pentest),
            config,
        )
    }

    fn final_response(content: &str) -> LlmResponse {
        LlmResponse {
            content: Some(content.into()),
            tool_calls: Vec::new(),
            finish_reason: Some("stop".into()),
            usage: Some(Usage {
                prompt_tokens: 7,
                completion_tokens: 3,
                total_tokens: 10,
            }),
            ..Default::default()
        }
    }

    fn verifier_satisfied_response() -> LlmResponse {
        final_response(
            &serde_json::json!({
                "schema_version": 1,
                "satisfied": true,
                "gaps": [],
                "evidence_ids": ["ev-1"],
            })
            .to_string(),
        )
    }

    fn tool_response(call: ToolCall) -> LlmResponse {
        tool_response_with_optional_message(None, call)
    }

    fn tool_response_with_message(content: &str, call: ToolCall) -> LlmResponse {
        tool_response_with_optional_message(Some(content), call)
    }

    fn tool_response_with_optional_message(content: Option<&str>, call: ToolCall) -> LlmResponse {
        LlmResponse {
            content: content.map(Into::into),
            tool_calls: vec![call],
            finish_reason: Some("tool_calls".into()),
            usage: None,
            ..Default::default()
        }
    }

    fn make_call(name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: "call-1".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: name.into(),
                arguments: arguments.into(),
            },
        }
    }

    #[tokio::test]
    async fn deep_cognition_persists_only_public_deliberation_commit() {
        let llm = Arc::new(QueueLlmBackend::new(vec![
            Ok(final_response(
                r#"{"schema_version":1,"candidate_hypotheses":[{"candidate_ref":"private-h1","claim":"PRIVATE_WORKSPACE_MARKER","falsifier":"mismatch"}],"candidate_experiments":[],"completion_candidate":null,"open_uncertainties":[]}"#,
            )),
            Ok(final_response(
                r#"{"schema_version":1,"issues":[],"missing_alternatives":[],"unsupported_claim_refs":[],"recommended_candidate_ref":"private-h1","completion_safe":true}"#,
            )),
            Ok(final_response("PUBLIC_COMMIT_MARKER")),
        ]));
        let mut context = make_context(llm, ToolRegistry::new(), GuardChain::new()).await;
        context.config.cognition.mode = holmes_core::ledger::ThinkMode::Deep;
        let mut runtime = AgentRuntime::new(context);
        let mut sink = VecSink::new();
        let outcome = runtime.run_turn("review", &mut sink).await.unwrap();
        assert!(matches!(outcome, TurnOutcome::FinalAnswer { .. }));

        let events = runtime
            .context
            .session_db
            .get_events(&runtime.context.session_id)
            .await
            .unwrap();
        let persisted = events
            .iter()
            .map(|event| serde_json::to_string(&event.event).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(persisted.contains("PUBLIC_COMMIT_MARKER"));
        assert!(!persisted.contains("PRIVATE_WORKSPACE_MARKER"));
        assert!(!persisted.contains("private-h1"));

        let ledger = runtime.context.state.ledger.as_ref().unwrap();
        assert_eq!(ledger.deliberation_commits.len(), 1);
        let commit = ledger.deliberation_commits.values().next().unwrap();
        assert_eq!(commit.mode, holmes_core::ledger::ThinkMode::Deep);
        assert!(commit.public_rationale.contains("PUBLIC_COMMIT_MARKER"));
        assert!(!commit.public_rationale.contains("PRIVATE_WORKSPACE_MARKER"));
    }

    #[tokio::test]
    async fn subagent_renewed_context_carries_isolation_and_budget() {
        // AGT-014: a runtime with a parent execution boundary renews into a context
        // one depth level deeper, with the configured per-subagent tool budget, the
        // subagent wall-clock cap (min'ed with the parent's deadline), and the
        // installed scratch directory.
        let mut config = HolmesConfig::default();
        config.subagent.max_tool_calls = Some(5);
        config.subagent.max_wall_clock_ms = Some(60_000);
        let llm = Arc::new(QueueLlmBackend::new(vec![]));
        let mut context =
            make_context_with_config(llm, ToolRegistry::new(), GuardChain::new(), config).await;

        let parent = holmes_core::execution_context::ExecutionContext::new("parent-turn")
            .with_turn_deadline(std::time::Duration::from_secs(600));
        context.set_parent_execution(&parent);
        context.set_turn_temp_dir(std::path::PathBuf::from("/tmp/holmes-sub-x"));
        context.renew_execution_context();

        assert_eq!(context.exec.depth(), 1, "one level deeper than the parent");
        assert_eq!(
            context.exec.budget().max_tool_calls,
            Some(5),
            "per-subagent tool budget applied"
        );
        assert_eq!(
            context.exec.temp_dir(),
            Some(std::path::Path::new("/tmp/holmes-sub-x"))
        );
        // Wall-clock cap (60s) wins over the parent's 600s deadline.
        let remaining = context
            .exec
            .remaining_turn_time()
            .expect("subagent turn has a deadline");
        assert!(
            remaining <= std::time::Duration::from_secs(60),
            "subagent wall-clock cap must bound the turn: {remaining:?}"
        );
        // Cancellation still propagates from the parent.
        assert!(!context.exec.is_cancelled());
        parent.cancel();
        assert!(context.exec.is_cancelled());

        // A top-level runtime (no parent) stays at depth 0 with no budget.
        let mut top = make_context(
            Arc::new(QueueLlmBackend::new(vec![])),
            ToolRegistry::new(),
            GuardChain::new(),
        )
        .await;
        top.renew_execution_context();
        assert_eq!(top.exec.depth(), 0);
        assert_eq!(top.exec.budget().max_tool_calls, None);
        assert!(top.exec.temp_dir().is_none());
    }

    #[tokio::test]
    async fn test_middlewares_guard_redact_audit() {
        use crate::middleware::{
            GuardMiddleware, SensitiveDataRedactMiddleware, TokenAuditMiddleware,
        };

        // 1. Test GuardMiddleware dangerous command blocking (real tool name + arg key)
        let cmd_call = make_call("execute_command", r#"{"command": "rm -rf /"}"#);
        let llm = Arc::new(QueueLlmBackend::new(vec![
            Ok(tool_response(cmd_call)),
            Ok(final_response("done")),
        ]));
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(MockTool));
        let context = make_context(llm, tools, GuardChain::new())
            .await
            .with_middlewares(vec![Arc::new(GuardMiddleware)]);
        let mut runtime = AgentRuntime::new(context);
        let mut sink = VecSink::new();
        let result = runtime.run_turn("test", &mut sink).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .message
            .contains("blocked dangerous command"));

        // 2. Test SensitiveDataRedactMiddleware data redaction
        let sec_call = make_call("mock_tool", r#"{"secret": "my-secret-token"}"#);
        let llm_sec = Arc::new(QueueLlmBackend::new(vec![
            Ok(tool_response(sec_call)),
            Ok(final_response("done api_key=1234567890")),
        ]));
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(MockTool));
        let context = make_context(llm_sec, tools, GuardChain::new())
            .await
            .with_middlewares(vec![Arc::new(SensitiveDataRedactMiddleware::new())]);
        let mut runtime = AgentRuntime::new(context);
        let mut sink = VecSink::new();
        let outcome = runtime.run_turn("test", &mut sink).await.expect("success");
        if let TurnOutcome::FinalAnswer { content, .. } = outcome {
            assert!(content.contains("[REDACTED]"));
            assert!(!content.contains("1234567890"));
        } else {
            panic!("Expected FinalAnswer");
        }

        // 3. Test TokenAuditMiddleware token budget auditing & fuse
        let llm_audit = Arc::new(QueueLlmBackend::new(vec![Ok(final_response("done"))]));
        let context = make_context(llm_audit, ToolRegistry::new(), GuardChain::new())
            .await
            .with_middlewares(vec![Arc::new(TokenAuditMiddleware::new(5))]);
        let mut runtime = AgentRuntime::new(context);
        let mut sink = VecSink::new();
        let result = runtime.run_turn("test", &mut sink).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("token limit exceeded"));
    }
}
