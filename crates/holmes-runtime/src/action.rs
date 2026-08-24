use holmes_core::hook::AgentHook;
use holmes_core::{Event, Message, ToolCall, ToolResult};
use std::sync::Arc;

use crate::context::RuntimeContext;
use crate::deliberation::RuntimeError;
use crate::dialogue::DialogueEngine;
use crate::permissions::{ApprovalHandler, PermissionPolicy};
use crate::yield_stream::{RuntimeSink, RuntimeYield};
use holmes_core::config::PermissionMode;

/// Denial reason when `Ask` mode has no approval surface installed (AGT-005).
const APPROVAL_UNAVAILABLE_REASON: &str =
    "approval required but no approval surface is available (fail-closed)";

#[derive(Debug, Clone, Default)]
pub struct ActionEngine {
    pub hooks: Vec<Arc<dyn AgentHook>>,
    /// Consulted for mutating tool calls when permission mode is `Ask` (e.g. a TUI y/n
    /// prompt). `None` → `Ask` mutating calls are denied (fail-closed: there is no
    /// approval surface to ask).
    pub approver: Option<Arc<dyn ApprovalHandler>>,
}

#[derive(Debug, Clone, Default)]
pub struct ActionBatchResult {
    pub results: Vec<ToolResult>,
    pub messages: Vec<Message>,
    pub events: Vec<RuntimeYield>,
}

impl ActionEngine {
    pub fn new() -> Self {
        Self {
            hooks: Vec::new(),
            approver: None,
        }
    }

    /// Execute a batch of tool calls. When the batch has >1 call and every tool is a
    /// read-only registry tool, the slow tool `execute()` I/O runs concurrently while
    /// all state mutation (events, guards, messages) stays sequential and in order.
    /// Any other batch takes the fully sequential path.
    pub async fn execute_batch(
        &self,
        context: &mut RuntimeContext,
        calls: &[ToolCall],
        sink: &mut dyn RuntimeSink,
    ) -> Result<ActionBatchResult, RuntimeError> {
        if calls.len() > 1 && context.tools.can_parallelize(calls) {
            self.execute_batch_parallel(context, calls, sink).await
        } else {
            self.execute_batch_sequential(context, calls, sink).await
        }
    }

    async fn execute_batch_sequential(
        &self,
        context: &mut RuntimeContext,
        calls: &[ToolCall],
        sink: &mut dyn RuntimeSink,
    ) -> Result<ActionBatchResult, RuntimeError> {
        let mut batch = ActionBatchResult::default();
        let permissions = PermissionPolicy;

        for call in calls {
            // Cancellation gate (AGT-002): once the turn's token fires, no new tool
            // call starts — remaining calls are answered with a blocked result.
            if context.exec.is_cancelled() {
                let reason = format!(
                    "turn cancelled: tool '{}' was not started (task {})",
                    call.function.name,
                    context.exec.task_id()
                );
                tracing::info!(
                    event = "CancellationCompleted",
                    tool = %call.function.name,
                    task_id = %context.exec.task_id(),
                    "tool call skipped after cancellation"
                );
                record_tool_call(context, call).await?;
                record_tool_blocked(context, call, "cancelled", &reason).await?;
                let mut result = ToolResult::blocked(&call.id, reason);
                let middlewares = context.middlewares.clone();
                for mw in &middlewares {
                    mw.after_tool_call(context, &mut result).await?;
                }
                for hook in &self.hooks {
                    let _ = hook.post_tool_use(call, &result);
                }
                batch.messages.push(result.to_message_with_vision());
                let finished = DialogueEngine::tool_finished(&result);
                sink.emit_yield(&context.session_id, finished.clone());
                batch.events.push(finished);
                batch.results.push(result);
                continue;
            }

            // Tool-call budget (ExecutionContext resource_budget, off by default).
            if !context.exec.try_consume_tool_call() {
                let reason = format!(
                    "tool budget exhausted: tool '{}' was not started (task {})",
                    call.function.name,
                    context.exec.task_id()
                );
                record_tool_call(context, call).await?;
                record_tool_blocked(context, call, "budget", &reason).await?;
                let mut result = ToolResult::blocked(&call.id, reason);
                let middlewares = context.middlewares.clone();
                for mw in &middlewares {
                    mw.after_tool_call(context, &mut result).await?;
                }
                for hook in &self.hooks {
                    let _ = hook.post_tool_use(call, &result);
                }
                batch.messages.push(result.to_message_with_vision());
                let finished = DialogueEngine::tool_finished(&result);
                sink.emit_yield(&context.session_id, finished.clone());
                batch.events.push(finished);
                batch.results.push(result);
                continue;
            }

            let decision = permissions.evaluate(&context.config.permissions, &context.tools, call);
            let permission_event = RuntimeYield::PermissionDecision {
                tool_name: call.function.name.clone(),
                call_id: Some(call.id.clone()),
                allowed: decision.allowed,
                reason: decision.reason.clone(),
            };
            sink.emit_yield(&context.session_id, permission_event.clone());
            batch.events.push(permission_event);

            if !decision.allowed {
                record_tool_call(context, call).await?;
                record_tool_blocked(context, call, "permission", &decision.reason).await?;
                let mut result = ToolResult::blocked(&call.id, decision.reason);
                let middlewares = context.middlewares.clone();
                for mw in &middlewares {
                    mw.after_tool_call(context, &mut result).await?;
                }
                for hook in &self.hooks {
                    let _ = hook.post_tool_use(call, &result);
                }

                batch.messages.push(result.to_message_with_vision());
                let finished = DialogueEngine::tool_finished(&result);
                sink.emit_yield(&context.session_id, finished.clone());
                batch.events.push(finished);
                batch.results.push(result);
                continue;
            }

            // Interactive approval gate (Ask mode): a mutating tool call is referred to
            // the operator before it runs. Read-only tools and other modes skip this.
            // With no approver installed (REPL / one-shot / classic TUI) the call is
            // denied — fail-closed, never silently proceed.
            if context.config.permissions.mode == PermissionMode::Ask
                && context.tools.effect_of(call) == holmes_tools::Effect::Mutating
            {
                if self.approver.is_none() {
                    holmes_core::metrics::metrics().count("approval.unavailable");
                    tracing::warn!(
                        event = "ApprovalUnavailable",
                        tool = %call.function.name,
                        task_id = %context.exec.task_id(),
                        session_id = %context.session_id,
                        "mutating tool call denied: no approval surface (fail-closed)"
                    );
                }
                let block_reason = match &self.approver {
                    Some(approver) if approver.request_approval(call).await => None,
                    Some(_) => Some("denied by operator".to_string()),
                    None => Some(APPROVAL_UNAVAILABLE_REASON.to_string()),
                };
                if let Some(reason) = block_reason {
                    record_tool_call(context, call).await?;
                    record_tool_blocked(context, call, "approval", &reason).await?;
                    let mut result = ToolResult::blocked(&call.id, reason);
                    let middlewares = context.middlewares.clone();
                    for mw in &middlewares {
                        mw.after_tool_call(context, &mut result).await?;
                    }
                    for hook in &self.hooks {
                        let _ = hook.post_tool_use(call, &result);
                    }
                    batch.messages.push(result.to_message_with_vision());
                    let finished = DialogueEngine::tool_finished(&result);
                    sink.emit_yield(&context.session_id, finished.clone());
                    batch.events.push(finished);
                    batch.results.push(result);
                    continue;
                }
            }

            let mut hook_blocked = None;
            for hook in &self.hooks {
                if let Err(e) = hook.pre_tool_use(call) {
                    hook_blocked = Some(e);
                    break;
                }
            }

            if let Some(reason) = hook_blocked {
                record_tool_call(context, call).await?;
                record_tool_blocked(context, call, "hook", &reason).await?;
                let mut result = ToolResult::blocked(&call.id, reason);
                let middlewares = context.middlewares.clone();
                for mw in &middlewares {
                    mw.after_tool_call(context, &mut result).await?;
                }
                for hook in &self.hooks {
                    let _ = hook.post_tool_use(call, &result);
                }

                batch.messages.push(result.to_message_with_vision());
                let finished = DialogueEngine::tool_finished(&result);
                sink.emit_yield(&context.session_id, finished.clone());
                batch.events.push(finished);
                batch.results.push(result);
                continue;
            }

            let started = DialogueEngine::tool_started(call);
            sink.emit_yield(&context.session_id, started.clone());
            batch.events.push(started);

            record_tool_call(context, call).await?;

            let result = if !context.tools.contains(&call.function.name) {
                let mut result = context.tools.execute_bounded(call, &context.exec).await;
                let middlewares = context.middlewares.clone();
                for mw in &middlewares {
                    mw.after_tool_call(context, &mut result).await?;
                }
                record_tool_result(context, call, &result).await?;
                result
            } else {
                let verdict = context
                    .guards
                    .run_pre(call, &context.state.compatibility_state)
                    .await;

                if !verdict.allowed {
                    record_tool_blocked(context, call, "guard", &verdict.guidance).await?;
                    let mut result = ToolResult::blocked(&call.id, verdict.guidance);
                    let middlewares = context.middlewares.clone();
                    for mw in &middlewares {
                        mw.after_tool_call(context, &mut result).await?;
                    }
                    result
                } else {
                    let mut result = context.tools.execute_bounded(call, &context.exec).await;
                    let middlewares = context.middlewares.clone();
                    for mw in &middlewares {
                        mw.after_tool_call(context, &mut result).await?;
                    }
                    record_tool_result(context, call, &result).await?;

                    context
                        .guards
                        .run_post(call, &result, &mut context.state.compatibility_state)
                        .await;
                    persist_pending_findings(context).await?;

                    result
                }
            };

            for hook in &self.hooks {
                let _ = hook.post_tool_use(call, &result);
            }

            batch.messages.push(result.to_message_with_vision());
            let finished = DialogueEngine::tool_finished(&result);
            sink.emit_yield(&context.session_id, finished.clone());
            batch.events.push(finished);
            batch.results.push(result);
        }

        Ok(batch)
    }

    /// Parallel path: every call is a read-only registry tool. Gating (permission /
    /// hook / guard-pre) and finalization (middleware / guard-post / events / messages)
    /// stay sequential and in order; only the tool `execute()` I/O runs concurrently.
    async fn execute_batch_parallel(
        &self,
        context: &mut RuntimeContext,
        calls: &[ToolCall],
        sink: &mut dyn RuntimeSink,
    ) -> Result<ActionBatchResult, RuntimeError> {
        let mut batch = ActionBatchResult::default();
        let permissions = PermissionPolicy;

        // A per-call slot: either it was blocked before execution, or it is runnable.
        enum Slot {
            Blocked(ToolResult),
            Run,
        }

        // Pass A — sequential gating, in order. Records ToolCall/started/blocked events.
        let mut slots = Vec::with_capacity(calls.len());
        for call in calls {
            // Cancellation gate (AGT-002): after the token fires no new tool starts.
            if context.exec.is_cancelled() {
                let reason = format!(
                    "turn cancelled: tool '{}' was not started (task {})",
                    call.function.name,
                    context.exec.task_id()
                );
                tracing::info!(
                    event = "CancellationCompleted",
                    tool = %call.function.name,
                    task_id = %context.exec.task_id(),
                    "tool call skipped after cancellation"
                );
                record_tool_call(context, call).await?;
                record_tool_blocked(context, call, "cancelled", &reason).await?;
                slots.push(Slot::Blocked(ToolResult::blocked(&call.id, reason)));
                continue;
            }
            if !context.exec.try_consume_tool_call() {
                let reason = format!(
                    "tool budget exhausted: tool '{}' was not started (task {})",
                    call.function.name,
                    context.exec.task_id()
                );
                record_tool_call(context, call).await?;
                record_tool_blocked(context, call, "budget", &reason).await?;
                slots.push(Slot::Blocked(ToolResult::blocked(&call.id, reason)));
                continue;
            }
            let decision = permissions.evaluate(&context.config.permissions, &context.tools, call);
            let permission_event = RuntimeYield::PermissionDecision {
                tool_name: call.function.name.clone(),
                call_id: Some(call.id.clone()),
                allowed: decision.allowed,
                reason: decision.reason.clone(),
            };
            sink.emit_yield(&context.session_id, permission_event.clone());
            batch.events.push(permission_event);

            if !decision.allowed {
                record_tool_call(context, call).await?;
                record_tool_blocked(context, call, "permission", &decision.reason).await?;
                slots.push(Slot::Blocked(ToolResult::blocked(
                    &call.id,
                    decision.reason,
                )));
                continue;
            }

            let mut hook_blocked = None;
            for hook in &self.hooks {
                if let Err(e) = hook.pre_tool_use(call) {
                    hook_blocked = Some(e);
                    break;
                }
            }
            if let Some(reason) = hook_blocked {
                record_tool_call(context, call).await?;
                record_tool_blocked(context, call, "hook", &reason).await?;
                slots.push(Slot::Blocked(ToolResult::blocked(&call.id, reason)));
                continue;
            }

            let verdict = context
                .guards
                .run_pre(call, &context.state.compatibility_state)
                .await;

            let started = DialogueEngine::tool_started(call);
            sink.emit_yield(&context.session_id, started.clone());
            batch.events.push(started);
            record_tool_call(context, call).await?;

            if !verdict.allowed {
                record_tool_blocked(context, call, "guard", &verdict.guidance).await?;
                slots.push(Slot::Blocked(ToolResult::blocked(
                    &call.id,
                    verdict.guidance,
                )));
                continue;
            }

            // Interactive approval gate (Ask mode): a mutating call (e.g. spawn_subagent)
            // that reaches the parallel path is still referred to the operator before it
            // joins the concurrent execute set. Read-only tools skip this. With no
            // approver installed the call is denied — fail-closed.
            if context.config.permissions.mode == PermissionMode::Ask
                && context.tools.effect_of(call) == holmes_tools::Effect::Mutating
            {
                if self.approver.is_none() {
                    holmes_core::metrics::metrics().count("approval.unavailable");
                    tracing::warn!(
                        event = "ApprovalUnavailable",
                        tool = %call.function.name,
                        task_id = %context.exec.task_id(),
                        session_id = %context.session_id,
                        "mutating tool call denied: no approval surface (fail-closed)"
                    );
                }
                let block_reason = match &self.approver {
                    Some(approver) if approver.request_approval(call).await => None,
                    Some(_) => Some("denied by operator".to_string()),
                    None => Some(APPROVAL_UNAVAILABLE_REASON.to_string()),
                };
                if let Some(reason) = block_reason {
                    record_tool_blocked(context, call, "approval", &reason).await?;
                    slots.push(Slot::Blocked(ToolResult::blocked(&call.id, reason)));
                    continue;
                }
            }
            slots.push(Slot::Run);
        }

        // Pass B — run the runnable executes concurrently (tools registry is Arc; each
        // future only borrows the shared registry and its own call). If cancellation
        // landed during pass A gating, nothing new starts: Run slots degrade to
        // blocked results in pass C.
        let run_skipped = context.exec.is_cancelled();
        if run_skipped && slots.iter().any(|slot| matches!(slot, Slot::Run)) {
            tracing::info!(
                event = "CancellationCompleted",
                task_id = %context.exec.task_id(),
                "parallel tool batch aborted before execution: turn cancelled"
            );
        }
        let tools = context.tools.clone();
        let exec = context.exec.clone();
        let run_futures = calls
            .iter()
            .zip(slots.iter())
            .filter(|(_, slot)| matches!(slot, Slot::Run) && !run_skipped)
            .map(|(call, _)| tools.execute_bounded(call, &exec));
        let mut executed = futures::future::join_all(run_futures).await.into_iter();

        // Pass C — sequential finalization, in original order.
        for (call, slot) in calls.iter().zip(slots) {
            let result = match slot {
                Slot::Blocked(mut result) => {
                    let middlewares = context.middlewares.clone();
                    for mw in &middlewares {
                        mw.after_tool_call(context, &mut result).await?;
                    }
                    result
                }
                Slot::Run => {
                    let mut result = if run_skipped {
                        ToolResult::blocked(
                            &call.id,
                            format!(
                                "turn cancelled: tool '{}' was not started (task {})",
                                call.function.name,
                                context.exec.task_id()
                            ),
                        )
                    } else {
                        executed
                            .next()
                            .expect("one execute result per runnable slot")
                    };
                    let middlewares = context.middlewares.clone();
                    for mw in &middlewares {
                        mw.after_tool_call(context, &mut result).await?;
                    }
                    if run_skipped {
                        record_tool_blocked(
                            context,
                            call,
                            "cancelled",
                            "turn cancelled before execution started",
                        )
                        .await?;
                    } else {
                        record_tool_result(context, call, &result).await?;
                        context
                            .guards
                            .run_post(call, &result, &mut context.state.compatibility_state)
                            .await;
                        persist_pending_findings(context).await?;
                    }
                    result
                }
            };

            for hook in &self.hooks {
                let _ = hook.post_tool_use(call, &result);
            }
            batch.messages.push(result.to_message_with_vision());
            let finished = DialogueEngine::tool_finished(&result);
            sink.emit_yield(&context.session_id, finished.clone());
            batch.events.push(finished);
            batch.results.push(result);
        }

        Ok(batch)
    }
}

async fn record_tool_call(
    context: &mut RuntimeContext,
    call: &ToolCall,
) -> Result<(), RuntimeError> {
    let arguments = call
        .args_parsed()
        .unwrap_or_else(|_| call.function.arguments.clone().into());
    let event = Event::ToolCall {
        name: call.function.name.clone(),
        arguments,
        purpose: None,
        call_id: Some(call.id.clone()),
    };
    append_and_ingest(context, event).await
}

async fn record_tool_result(
    context: &mut RuntimeContext,
    call: &ToolCall,
    result: &ToolResult,
) -> Result<(), RuntimeError> {
    let text = result.text_content();
    let mut result_event = Event::ToolResult {
        name: call.function.name.clone(),
        success: result.is_success(),
        outcome: Some(result.status),
        content: text.clone(),
        error: (!result.is_success()).then_some(text),
        artifacts: vec![],
        call_id: Some(call.id.clone()),
    };
    let middlewares = context.middlewares.clone();
    for middleware in &middlewares {
        middleware
            .before_event_persist(context, &mut result_event)
            .await?;
    }

    let persisted_content = match &result_event {
        Event::ToolResult { content, .. } => content.clone(),
        _ => unreachable!("result_event is a ToolResult"),
    };
    let planned_binding = context.state.action_bindings.get(&call.id).cloned();
    // `spawn_subagent` success means only that orchestration started/returned;
    // it is not an observation of the delegated Experiment. The child task
    // writes its own case Evidence and the durable task sink finalizes the
    // Experiment after fenced result verification.
    let should_record = call.function.name != "spawn_subagent"
        && (result.is_success()
            || (matches!(
                result.status,
                holmes_core::ToolOutcomeStatus::Failed | holmes_core::ToolOutcomeStatus::TimedOut
            ) && planned_binding
                .as_ref()
                .and_then(|binding| binding.experiment_id.as_ref())
                .is_some()));

    if should_record {
        // Runtime normally loads the case snapshot at the turn boundary. Keep the
        // ActionEngine safe when it is embedded or exercised directly as well:
        // evidence persistence must still go through the same authoritative case
        // receipt path, never fall back to a transcript-only ToolResult.
        if context.state.ledger.is_none() {
            let case_id = context
                .session_db
                .case_id_for_session(&context.session_id)
                .await
                .map_err(|error| {
                    RuntimeError::recoverable(format!(
                        "tool outcome cannot resolve its case Ledger: {error}"
                    ))
                })?;
            context.state.ledger =
                Some(context.session_db.load(&case_id).await.map_err(|error| {
                    RuntimeError::recoverable(format!(
                        "tool outcome cannot load its case Ledger: {error}"
                    ))
                })?);
        }
        let ledger = context
            .state
            .ledger
            .as_ref()
            .expect("Ledger was loaded immediately above");
        let preview = context.state.task_control.preview_tool_evidence(
            &call.function.name,
            Some(&call.id),
            &call.function.arguments,
            &persisted_content,
        );
        let mut binding = planned_binding.unwrap_or_else(|| holmes_core::ledger::ActionBinding {
            case_id: ledger.case_id.clone(),
            contract_id: preview.contract_id.clone(),
            requirement_ids: preview.requirement_ids.clone(),
            experiment_id: None,
            prediction_ids: Vec::new(),
            tool_call_id: call.id.clone(),
            attempt: 0,
        });
        binding.contract_id = preview.contract_id;
        binding.requirement_ids = if result.is_success() {
            preview.requirement_ids
        } else {
            Vec::new()
        };
        let kind = if result.is_success() {
            holmes_core::ledger::EvidenceKind::Deterministic
        } else {
            holmes_core::ledger::EvidenceKind::AuditOutcome
        };
        let verified_by = if result.is_success() {
            holmes_core::ledger::VerificationMethod::Runtime
        } else {
            holmes_core::ledger::VerificationMethod::DomainValidator
        };
        let exit_code = serde_json::from_str::<serde_json::Value>(&persisted_content)
            .ok()
            .and_then(|value| value.get("exit_code").and_then(|value| value.as_i64()))
            .and_then(|value| i32::try_from(value).ok());
        let single_line: String = persisted_content
            .chars()
            .map(|character| {
                if character.is_whitespace() {
                    ' '
                } else {
                    character
                }
            })
            .take(500)
            .collect();
        let receipt = holmes_core::ledger::ToolOutcomeReceipt {
            tool: call.function.name.clone(),
            outcome_status: result.status,
            exit_code,
            kind,
            input_summary: preview.input_summary,
            output_hash: holmes_core::content_hash(&persisted_content),
            output_snippet: single_line.trim().to_owned(),
            predicate: preview.predicate,
            verified_by,
            recorded_at: chrono::Utc::now(),
        };
        let receipt_result = context
            .session_db
            .record_evidence(&context.session_id, result_event.clone(), binding, receipt)
            .await
            .map_err(|error| {
                RuntimeError::recoverable(format!(
                    "failed to atomically record ToolResult and Ledger evidence: {error}"
                ))
            })?;
        context.state.ledger = Some(context.session_db.load(&ledger.case_id).await.map_err(
            |error| {
                RuntimeError::recoverable(format!(
                    "failed to refresh Ledger after evidence receipt: {error}"
                ))
            },
        )?);
        debug_assert_eq!(
            context.state.ledger.as_ref().map(|state| state.version),
            Some(receipt_result.ledger_version)
        );
        context.mind_palace.ingest(result_event);
        Ok(())
    } else {
        context
            .session_db
            .append_event(&context.session_id, &result_event)
            .await
            .map_err(|error| {
                RuntimeError::recoverable(format!(
                    "failed to persist runtime event for session {}: {}",
                    context.session_id, error
                ))
            })?;
        context.mind_palace.ingest(result_event);
        Ok(())
    }
}

async fn record_tool_blocked(
    context: &mut RuntimeContext,
    call: &ToolCall,
    guard_name: &str,
    reason: &str,
) -> Result<(), RuntimeError> {
    let blocked_event = Event::ToolBlocked {
        tool_name: call.function.name.clone(),
        guard_name: guard_name.into(),
        reason: reason.into(),
        call_id: Some(call.id.clone()),
    };
    append_and_ingest(context, blocked_event).await
}

/// Drain findings queued by SkepticGate this step and persist each as a durable
/// `FindingRecorded` event so the validated zone survives turn boundaries / resume.
async fn persist_pending_findings(context: &mut RuntimeContext) -> Result<(), RuntimeError> {
    let pending = context.state.compatibility_state.take_pending_findings();
    for f in pending {
        let confidence = match f.confidence {
            holmes_core::state::validated::FindingConfidence::Confirmed => "confirmed",
            holmes_core::state::validated::FindingConfidence::Candidate => "candidate",
            holmes_core::state::validated::FindingConfidence::Rejected => "rejected",
        }
        .to_string();
        let event = Event::FindingRecorded {
            id: f.id,
            finding_type: f.finding_type,
            confidence,
            severity: f.severity,
            evidence: f.evidence,
            details: f.details,
            attack_type: f.attack_type,
            location: f.location,
            evidence_source: f.evidence_source,
            resolution_ids: f.resolution_ids,
            affected_asset: f.affected_asset,
            evidence_artifacts: f.evidence_artifacts,
        };
        append_and_ingest(context, event).await?;
    }
    for event in context.state.compatibility_state.take_pending_bounty_events() {
        append_and_ingest(context, event).await?;
    }
    Ok(())
}

/// Replay persisted `FindingRecorded` events back into a fresh `AttackState` — used at
/// context construction so findings recorded in prior turns/sessions are present again.
pub fn seed_findings_from_events(state: &mut holmes_core::state::AttackState, events: &[Event]) {
    use holmes_core::state::validated::{Finding, FindingConfidence};
    for event in events {
        match event {
            Event::FindingRecorded {
                id,
                finding_type,
                confidence,
                severity,
                evidence,
                details,
                attack_type,
                location,
                evidence_source,
                resolution_ids,
                affected_asset,
                evidence_artifacts,
            } => {
                let confidence = match confidence.as_str() {
                    "confirmed" => FindingConfidence::Confirmed,
                    "rejected" => FindingConfidence::Rejected,
                    _ => FindingConfidence::Candidate,
                };
                state.record_finding(Finding {
                    id: id.clone(),
                    finding_type: finding_type.clone(),
                    confidence,
                    evidence: evidence.clone(),
                    details: details.clone(),
                    attack_type: attack_type.clone(),
                    severity: severity.clone(),
                    location: location.clone(),
                    evidence_source: evidence_source.clone(),
                    resolution_ids: resolution_ids.clone(),
                    affected_asset: affected_asset.clone(),
                    evidence_artifacts: evidence_artifacts.clone(),
                });
            }
            Event::ProgramScopeSet { program } => {
                state.bounty.program = Some(program.clone());
            }
            Event::AssetRecorded { asset } => {
                state.bounty.upsert_asset(asset.clone());
            }
            Event::ReportGenerated { .. } => {
                // last_report is regenerated on demand; nothing to seed.
            }
            _ => {}
        }
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
    use std::sync::Arc;

    use anyhow::Result;
    use async_trait::async_trait;
    use holmes_core::config::HolmesConfig;
    use holmes_core::session::RuntimeSession;
    use holmes_core::state::AttackState;
    use holmes_core::{
        FunctionCall, FunctionDefinition, GuardVerdict, LlmResponse, SessionMode, ToolDefinition,
    };
    use holmes_guards::traits::{PostGuard, PreGuard};
    use holmes_guards::GuardChain;
    use holmes_mind_palace::MindPalace;
    use holmes_session::{memory_store::MemoryStore, CreateSessionParams, SessionDB, SessionStore};
    use holmes_tools::{Tool, ToolRegistry};

    use crate::context::{RuntimeContext, RuntimeState};
    use crate::deliberation::StaticLlmBackend;
    use crate::yield_stream::VecSink;

    use super::*;

    #[tokio::test]
    async fn successful_tool_emits_yields_results_and_events() {
        let mut context = make_context(GuardChain::new()).await;
        let call = make_call("mock_tool", r#"{"target":"example.test"}"#);
        let mut sink = VecSink::new();

        let batch = ActionEngine::new()
            .execute_batch(&mut context, std::slice::from_ref(&call), &mut sink)
            .await
            .expect("execute batch");

        assert_eq!(batch.results.len(), 1);
        assert_eq!(batch.messages.len(), 1);
        assert!(!batch.results[0].is_error);
        assert_eq!(batch.results[0].text_content(), "mock output");
        assert_eq!(
            batch.events,
            vec![
                RuntimeYield::PermissionDecision {
                    tool_name: "mock_tool".into(),
                    call_id: Some("call-1".into()),
                    allowed: true,
                    reason: "read-only tool auto-approved".into(),
                },
                RuntimeYield::ToolStarted {
                    name: "mock_tool".into(),
                    call_id: Some("call-1".into()),
                    args: Some(r#"{"target":"example.test"}"#.into()),
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
        assert_eq!(sink.yields(), batch.events);
        assert!(context.session.messages.is_empty());
        assert_eq!(batch.messages[0].tool_call_id.as_deref(), Some("call-1"));
        assert_eq!(batch.messages[0].name.as_deref(), Some("mock_tool"));
        assert_eq!(
            context.state.compatibility_state.flag.as_deref(),
            Some("post:mock_tool:false")
        );

        let stored = context
            .session_db
            .get_events(&context.session_id)
            .await
            .expect("stored events");
        assert_eq!(stored.len(), 2);
        assert!(matches!(
            &stored[0].event,
            Event::ToolCall { name, arguments, .. }
                if name == "mock_tool" && arguments["target"] == "example.test"
        ));
        assert!(matches!(
            &stored[1].event,
            Event::ToolResult { name, success, content, .. }
                if name == "mock_tool" && *success && content == "mock output"
        ));
        assert_eq!(context.mind_palace.memory.event_count(), 2);
    }

    #[tokio::test]
    async fn blocked_tool_returns_blocked_result_and_failure_yield() {
        let mut guards = GuardChain::new();
        guards.pre.push(Box::new(BlockGuard));
        let mut context = make_context(guards).await;
        let mut sink = VecSink::new();

        let batch = ActionEngine::new()
            .execute_batch(
                &mut context,
                &[make_call("mock_tool", "not-json")],
                &mut sink,
            )
            .await
            .expect("execute batch");

        assert_eq!(batch.results.len(), 1);
        assert_eq!(batch.messages.len(), 1);
        assert!(batch.results[0].is_error);
        assert!(batch.results[0]
            .text_content()
            .contains("[GUARD] blocked by test"));
        assert_eq!(batch.events.len(), 3);
        assert!(matches!(
            batch.events[2],
            RuntimeYield::ToolFinished { success: false, .. }
        ));
        assert_eq!(sink.yields(), batch.events);
        assert!(context.state.compatibility_state.flag.is_none());

        let stored = context
            .session_db
            .get_events(&context.session_id)
            .await
            .expect("stored events");
        assert_eq!(stored.len(), 2);
        assert!(matches!(
            &stored[0].event,
            Event::ToolCall { arguments, .. }
                if arguments.as_str() == Some("not-json")
        ));
        assert!(matches!(
            &stored[1].event,
            Event::ToolBlocked { tool_name, reason, .. }
                if tool_name == "mock_tool" && reason == "blocked by test"
        ));
        assert_eq!(context.mind_palace.memory.event_count(), 2);
    }

    #[tokio::test]
    async fn unknown_tool_produces_error_result_and_failure_yield() {
        let mut context = make_context(GuardChain::new()).await;
        let mut sink = VecSink::new();

        let batch = ActionEngine::new()
            .execute_batch(&mut context, &[make_call("missing_tool", "{}")], &mut sink)
            .await
            .expect("execute batch");

        assert_eq!(batch.results.len(), 1);
        assert_eq!(batch.messages.len(), 1);
        assert!(batch.results[0].is_error);
        assert_eq!(batch.results[0].tool_name, "missing_tool");
        assert!(batch.results[0].text_content().contains("unknown tool"));
        assert_eq!(batch.events.len(), 3);
        assert!(matches!(
            &batch.events[2],
            RuntimeYield::ToolFinished {
                name,
                success: false,
                ..
            } if name == "missing_tool"
        ));
        assert_eq!(sink.yields(), batch.events);

        let stored = context
            .session_db
            .get_events(&context.session_id)
            .await
            .expect("stored events");
        assert_eq!(stored.len(), 2);
        assert!(matches!(&stored[0].event, Event::ToolCall { name, .. } if name == "missing_tool"));
        assert!(matches!(
            &stored[1].event,
            Event::ToolResult { name, success, error, .. }
                if name == "missing_tool" && !success && error.is_some()
        ));
        assert!(context.state.compatibility_state.flag.is_none());
    }

    #[tokio::test]
    async fn failed_event_append_returns_error_without_ingesting() {
        let mut context = make_context_without_db_session(GuardChain::new()).await;
        let mut sink = VecSink::new();

        let error = ActionEngine::new()
            .execute_batch(&mut context, &[make_call("mock_tool", "{}")], &mut sink)
            .await
            .expect_err("missing DB session should fail event append");

        assert!(error.message.contains("failed to persist runtime event"));
        assert_eq!(
            sink.yields(),
            vec![
                RuntimeYield::PermissionDecision {
                    tool_name: "mock_tool".into(),
                    call_id: Some("call-1".into()),
                    allowed: true,
                    reason: "read-only tool auto-approved".into(),
                },
                RuntimeYield::ToolStarted {
                    name: "mock_tool".into(),
                    call_id: Some("call-1".into()),
                    args: Some("{}".into()),
                },
            ]
        );
        assert_eq!(context.mind_palace.memory.event_count(), 0);
        assert!(context.state.compatibility_state.flag.is_none());
        assert!(context.session.messages.is_empty());
    }

    #[tokio::test]
    async fn permission_denial_returns_blocked_tool_result_without_execution() {
        let mut context = make_context(GuardChain::new()).await;
        context.config.permissions.mode = holmes_core::config::PermissionMode::Plan;
        let mut sink = VecSink::new();

        let batch = ActionEngine::new()
            .execute_batch(&mut context, &[make_call("mock_tool", "{}")], &mut sink)
            .await
            .expect("execute batch");

        assert_eq!(batch.results.len(), 1);
        assert!(batch.results[0].is_error);
        assert!(batch.results[0].text_content().contains("permission mode"));
        assert_eq!(
            batch.events,
            vec![
                RuntimeYield::PermissionDecision {
                    tool_name: "mock_tool".into(),
                    call_id: Some("call-1".into()),
                    allowed: false,
                    reason: "permission mode 'plan' blocks tool 'mock_tool'; Holmes must continue by planning or asking Watson".into(),
                },
                RuntimeYield::ToolFinished {
                    name: "guard".into(),
                    call_id: Some("call-1".into()),
                    success: false,
                    content: "[GUARD] permission mode 'plan' blocks tool 'mock_tool'; Holmes must continue by planning or asking Watson".into(),
                 error: None, usage: None },
            ]
        );
        assert_eq!(sink.yields(), batch.events);
        assert!(context.state.compatibility_state.flag.is_none());

        let stored = context
            .session_db
            .get_events(&context.session_id)
            .await
            .expect("stored events");
        assert_eq!(stored.len(), 2);
        assert!(matches!(&stored[0].event, Event::ToolCall { name, .. } if name == "mock_tool"));
        assert!(matches!(
            &stored[1].event,
            Event::ToolBlocked { tool_name, guard_name, reason, .. }
                if tool_name == "mock_tool" && guard_name == "permission" && reason.contains("plan")
        ));
        assert_eq!(context.mind_palace.memory.event_count(), 2);
        assert_eq!(batch.messages[0].tool_call_id.as_deref(), Some("call-1"));
    }

    async fn make_context(guards: GuardChain) -> RuntimeContext {
        make_context_with_session(guards, true).await
    }

    async fn make_context_without_db_session(guards: GuardChain) -> RuntimeContext {
        make_context_with_session(guards, false).await
    }

    async fn make_context_with_session(guards: GuardChain, create_session: bool) -> RuntimeContext {
        make_context_full(guards, create_session, Vec::new()).await
    }

    async fn make_context_full(
        mut guards: GuardChain,
        create_session: bool,
        extra_tools: Vec<Box<dyn Tool>>,
    ) -> RuntimeContext {
        guards.post.push(Box::new(RecordingPostGuard));

        let session_id = "session-1".to_string();
        let session_db = Arc::new(SessionDB::open(":memory:").await.expect("session db"));
        if create_session {
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
        }
        let memory_store = Arc::new(MemoryStore::open(":memory:").await.expect("memory store"));
        let mind_palace = MindPalace::new(session_db.clone(), memory_store.clone());
        let llm = Arc::new(StaticLlmBackend::new(LlmResponse {
            content: Some("ok".into()),
            tool_calls: Vec::new(),
            finish_reason: None,
            usage: None,
            ..Default::default()
        }));
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(MockTool));
        tools.register(Box::new(SleepyTool));
        tools.register(Box::new(WriteMockTool));
        tools.register(Box::new(SubagentMock));
        for tool in extra_tools {
            tools.register(tool);
        }

        RuntimeContext::new(
            RuntimeSession::new(session_id, SessionMode::Pentest),
            session_db,
            memory_store,
            mind_palace,
            llm,
            Arc::new(tools),
            guards,
            RuntimeState::new(SessionMode::Pentest),
            HolmesConfig::default(),
        )
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

    /// A read-only tool that sleeps, then echoes its args — used to prove that a batch
    /// of read-only calls executes concurrently (not serially).
    struct SleepyTool;

    #[async_trait]
    impl Tool for SleepyTool {
        fn name(&self) -> &str {
            "sleepy"
        }
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                tool_type: "function".into(),
                function: FunctionDefinition {
                    name: "sleepy".into(),
                    description: "sleeps and echoes".into(),
                    parameters: Default::default(),
                },
            }
        }
        fn is_read_only(&self) -> bool {
            true
        }
        async fn execute(&self, args: &str) -> Result<String> {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            Ok(args.to_string())
        }
    }

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
                    description: "a mutating tool".into(),
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

    /// Stand-in for `spawn_subagent`: a mutating (non-read-only) tool that sleeps to
    /// model a subagent's LLM loop. The parallel dispatch keys on the *name*
    /// `spawn_subagent`, so this proves a subagent fan-out runs concurrently.
    struct SubagentMock;
    #[async_trait]
    impl Tool for SubagentMock {
        fn name(&self) -> &str {
            "spawn_subagent"
        }
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                tool_type: "function".into(),
                function: FunctionDefinition {
                    name: "spawn_subagent".into(),
                    description: "spawns an isolated subagent".into(),
                    parameters: Default::default(),
                },
            }
        }
        fn is_read_only(&self) -> bool {
            false
        }
        async fn execute(&self, args: &str) -> Result<String> {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            Ok(args.to_string())
        }
    }

    fn subagent_call(id: &str, args: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "spawn_subagent".into(),
                arguments: args.into(),
            },
        }
    }

    #[tokio::test]
    async fn subagent_batch_runs_concurrently_and_preserves_order() {
        let mut context = make_context(GuardChain::new()).await;
        let calls = vec![
            subagent_call("s1", "alpha"),
            subagent_call("s2", "beta"),
            subagent_call("s3", "gamma"),
        ];
        let mut sink = VecSink::new();

        let start = std::time::Instant::now();
        let batch = ActionEngine::new()
            .execute_batch(&mut context, &calls, &mut sink)
            .await
            .expect("execute batch");
        let elapsed = start.elapsed();

        // 3 subagents × 50ms serial ≈ 150ms; concurrent ≈ 50ms.
        assert!(
            elapsed < std::time::Duration::from_millis(130),
            "expected concurrent subagent execution, took {elapsed:?}"
        );
        assert_eq!(batch.results.len(), 3);
        assert_eq!(batch.results[0].text_content(), "alpha");
        assert_eq!(batch.results[1].text_content(), "beta");
        assert_eq!(batch.results[2].text_content(), "gamma");
        assert_eq!(batch.messages[0].tool_call_id.as_deref(), Some("s1"));
        assert_eq!(batch.messages[2].tool_call_id.as_deref(), Some("s3"));
    }

    #[tokio::test]
    async fn subagent_batch_respects_ask_mode_denial() {
        let mut context = make_context(GuardChain::new()).await;
        context.config.permissions.mode = holmes_core::config::PermissionMode::Ask;
        let calls = vec![subagent_call("s1", "alpha"), subagent_call("s2", "beta")];
        let mut sink = VecSink::new();
        let mut engine = ActionEngine::new();
        engine.approver = Some(Arc::new(FixedApprover(false)));

        // Operator denies: both mutating subagent calls must be blocked, none executed.
        let batch = engine
            .execute_batch(&mut context, &calls, &mut sink)
            .await
            .expect("execute batch");
        assert_eq!(batch.results.len(), 2);
        assert!(batch.results.iter().all(|r| r.is_error));
        assert!(batch.results[0].text_content().contains("denied"));
    }

    #[derive(Debug)]
    struct FixedApprover(bool);
    #[async_trait]
    impl crate::permissions::ApprovalHandler for FixedApprover {
        async fn request_approval(&self, _call: &ToolCall) -> bool {
            self.0
        }
    }

    #[tokio::test]
    async fn ask_mode_blocks_mutating_tool_when_operator_denies() {
        let mut context = make_context(GuardChain::new()).await;
        context.config.permissions.mode = holmes_core::config::PermissionMode::Ask;
        let mut engine = ActionEngine::new();
        engine.approver = Some(Arc::new(FixedApprover(false)));
        let mut sink = VecSink::new();

        let batch = engine
            .execute_batch(&mut context, &[make_call("write_mock", "{}")], &mut sink)
            .await
            .expect("batch");
        assert!(batch.results[0].is_error);
        assert!(batch.results[0]
            .text_content()
            .contains("denied by operator"));
    }

    #[tokio::test]
    async fn ask_mode_runs_mutating_tool_when_operator_approves() {
        let mut context = make_context(GuardChain::new()).await;
        context.config.permissions.mode = holmes_core::config::PermissionMode::Ask;
        let mut engine = ActionEngine::new();
        engine.approver = Some(Arc::new(FixedApprover(true)));
        let mut sink = VecSink::new();

        let batch = engine
            .execute_batch(&mut context, &[make_call("write_mock", "{}")], &mut sink)
            .await
            .expect("batch");
        assert!(!batch.results[0].is_error);
        assert_eq!(batch.results[0].text_content(), "wrote");
    }

    #[tokio::test]
    async fn ask_mode_never_prompts_for_read_only_tools() {
        // A denying approver must NOT block a read-only tool — approval is writes-only.
        let mut context = make_context(GuardChain::new()).await;
        context.config.permissions.mode = holmes_core::config::PermissionMode::Ask;
        let mut engine = ActionEngine::new();
        engine.approver = Some(Arc::new(FixedApprover(false)));
        let mut sink = VecSink::new();

        let batch = engine
            .execute_batch(&mut context, &[make_call("mock_tool", "{}")], &mut sink)
            .await
            .expect("batch");
        assert!(!batch.results[0].is_error);
    }

    #[tokio::test]
    async fn ask_mode_without_approver_denies_mutating_tool() {
        // Fail-closed (AGT-005): Ask + mutating + no approval surface = denied.
        let mut context = make_context(GuardChain::new()).await;
        context.config.permissions.mode = holmes_core::config::PermissionMode::Ask;
        let mut sink = VecSink::new();

        let batch = ActionEngine::new()
            .execute_batch(&mut context, &[make_call("write_mock", "{}")], &mut sink)
            .await
            .expect("batch");

        assert_eq!(batch.results.len(), 1);
        assert!(batch.results[0].is_error);
        assert!(batch.results[0]
            .text_content()
            .contains("no approval surface is available"));
        assert!(context.state.compatibility_state.flag.is_none());

        let stored = context
            .session_db
            .get_events(&context.session_id)
            .await
            .expect("stored events");
        assert!(stored.iter().any(|e| matches!(
            &e.event,
            Event::ToolBlocked { tool_name, guard_name, reason, .. }
                if tool_name == "write_mock" && guard_name == "approval" && reason.contains("fail-closed")
        )));
    }

    #[tokio::test]
    async fn ask_mode_without_approver_denies_subagent_batch() {
        // Same fail-closed rule on the parallel path.
        let mut context = make_context(GuardChain::new()).await;
        context.config.permissions.mode = holmes_core::config::PermissionMode::Ask;
        let calls = vec![subagent_call("s1", "alpha"), subagent_call("s2", "beta")];
        let mut sink = VecSink::new();

        let batch = ActionEngine::new()
            .execute_batch(&mut context, &calls, &mut sink)
            .await
            .expect("batch");

        assert_eq!(batch.results.len(), 2);
        assert!(batch.results.iter().all(|r| r.is_error));
        assert!(batch.results[0]
            .text_content()
            .contains("no approval surface is available"));
    }

    fn sleepy_call(id: &str, args: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "sleepy".into(),
                arguments: args.into(),
            },
        }
    }

    #[tokio::test]
    async fn read_only_batch_executes_concurrently_and_preserves_order() {
        let mut context = make_context(GuardChain::new()).await;
        let calls = vec![
            sleepy_call("c1", "one"),
            sleepy_call("c2", "two"),
            sleepy_call("c3", "three"),
        ];
        let mut sink = VecSink::new();

        let start = std::time::Instant::now();
        let batch = ActionEngine::new()
            .execute_batch(&mut context, &calls, &mut sink)
            .await
            .expect("execute batch");
        let elapsed = start.elapsed();

        // 3 × 50ms serial ≈ 150ms; concurrent ≈ 50ms. Generous ceiling avoids flakiness.
        assert!(
            elapsed < std::time::Duration::from_millis(130),
            "expected concurrent execution, took {elapsed:?}"
        );

        // Results and messages stay in submission order.
        assert_eq!(batch.results.len(), 3);
        assert_eq!(batch.results[0].text_content(), "one");
        assert_eq!(batch.results[1].text_content(), "two");
        assert_eq!(batch.results[2].text_content(), "three");
        assert_eq!(batch.messages[0].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(batch.messages[1].tool_call_id.as_deref(), Some("c2"));
        assert_eq!(batch.messages[2].tool_call_id.as_deref(), Some("c3"));

        // Every call still produced a ToolResult event, in order.
        let stored = context
            .session_db
            .get_events(&context.session_id)
            .await
            .expect("stored events");
        let tool_results: Vec<_> = stored
            .iter()
            .filter_map(|e| match &e.event {
                Event::ToolResult { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(tool_results, vec!["one", "two", "three"]);
    }

    #[tokio::test]
    async fn single_read_only_call_uses_sequential_path() {
        let mut context = make_context(GuardChain::new()).await;
        let mut sink = VecSink::new();
        let batch = ActionEngine::new()
            .execute_batch(&mut context, &[sleepy_call("c1", "solo")], &mut sink)
            .await
            .expect("execute batch");
        assert_eq!(batch.results.len(), 1);
        assert_eq!(batch.results[0].text_content(), "solo");
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

    struct RecordingPostGuard;

    #[async_trait]
    impl PostGuard for RecordingPostGuard {
        fn name(&self) -> &str {
            "record"
        }

        async fn process(&mut self, call: &ToolCall, result: &ToolResult, state: &mut AttackState) {
            // Test-only marker proving a PostGuard ran and can mutate AttackState.
            state.flag = Some(format!("post:{}:{}", call.function.name, result.is_error));
        }
    }

    /// A mutating tool that cancels the turn's execution token when executed — used to
    /// prove that later calls in the same batch never start (AGT-002).
    struct CancellerTool {
        token: tokio_util::sync::CancellationToken,
    }

    #[async_trait]
    impl Tool for CancellerTool {
        fn name(&self) -> &str {
            "canceller"
        }
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                tool_type: "function".into(),
                function: FunctionDefinition {
                    name: "canceller".into(),
                    description: "cancels the turn".into(),
                    parameters: Default::default(),
                },
            }
        }
        fn is_read_only(&self) -> bool {
            // Mutating on purpose: forces the sequential batch path so the second
            // call's gating runs strictly after this one executed.
            false
        }
        async fn execute(&self, _args: &str) -> Result<String> {
            self.token.cancel();
            Ok("turn cancelled from inside a tool".into())
        }
    }

    fn canceller_call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "canceller".into(),
                arguments: "{}".into(),
            },
        }
    }

    #[tokio::test]
    async fn cancelled_context_starts_no_tool_call() {
        // AGT-002 acceptance: after cancellation, no new tool starts.
        let mut context = make_context(GuardChain::new()).await;
        context.exec.cancel();
        let mut sink = VecSink::new();

        let batch = ActionEngine::new()
            .execute_batch(&mut context, &[make_call("mock_tool", "{}")], &mut sink)
            .await
            .expect("execute batch");

        assert_eq!(batch.results.len(), 1);
        assert!(batch.results[0].is_error);
        assert!(
            batch.results[0].text_content().contains("was not started"),
            "got: {}",
            batch.results[0].text_content()
        );
        // The block is persisted with the cancellation guard name, not executed.
        let stored = context
            .session_db
            .get_events(&context.session_id)
            .await
            .expect("stored events");
        assert!(stored.iter().any(|s| matches!(
            &s.event,
            Event::ToolBlocked { tool_name, guard_name, .. }
                if tool_name == "mock_tool" && guard_name == "cancelled"
        )));
        // No ToolResult event: the tool never ran.
        assert!(!stored
            .iter()
            .any(|s| matches!(&s.event, Event::ToolResult { name, .. } if name == "mock_tool")));
    }

    #[tokio::test]
    async fn tool_that_cancels_turn_blocks_later_calls_in_batch() {
        let exec = holmes_core::execution_context::ExecutionContext::new("turn-mid-cancel");
        let token = exec.token();
        let mut context = make_context_full(
            GuardChain::new(),
            true,
            vec![Box::new(CancellerTool { token })],
        )
        .await;
        context.exec = exec;
        let mut sink = VecSink::new();

        let batch = ActionEngine::new()
            .execute_batch(
                &mut context,
                &[canceller_call("c1"), make_call("write_mock", "{}")],
                &mut sink,
            )
            .await
            .expect("execute batch");

        assert_eq!(batch.results.len(), 2);
        // The cancelling tool itself ran (either completed, or its completion raced
        // the cancellation it triggered — both prove it started).
        // The call AFTER the cancellation must not have started.
        assert!(batch.results[1].is_error);
        assert!(
            batch.results[1].text_content().contains("was not started"),
            "got: {}",
            batch.results[1].text_content()
        );
    }

    #[tokio::test]
    async fn tool_budget_blocks_calls_past_the_cap() {
        let mut context = make_context(GuardChain::new()).await;
        context.exec = holmes_core::execution_context::ExecutionContext::new("budgeted")
            .with_budget(holmes_core::execution_context::ResourceBudget {
                max_tool_calls: Some(1),
            });
        let mut sink = VecSink::new();

        // Two read-only calls take the parallel path; pass A consumes the budget in
        // order, so the second call is blocked before anything runs concurrently.
        let batch = ActionEngine::new()
            .execute_batch(
                &mut context,
                &[make_call("mock_tool", "{}"), sleepy_call("c2", "second")],
                &mut sink,
            )
            .await
            .expect("execute batch");

        assert_eq!(batch.results.len(), 2);
        assert!(!batch.results[0].is_error);
        assert!(batch.results[1].is_error);
        assert!(
            batch.results[1].text_content().contains("budget exhausted"),
            "got: {}",
            batch.results[1].text_content()
        );
    }

    #[tokio::test]
    async fn renew_execution_context_applies_config_and_parent() {
        let mut context = make_context(GuardChain::new()).await;
        context.config.execution.turn_deadline_ms = Some(60_000);
        context.config.execution.tool_deadline_ms = 123_000;

        let parent = holmes_core::execution_context::ExecutionContext::new("parent")
            .with_turn_deadline(std::time::Duration::from_secs(10));
        context.set_parent_execution(&parent);
        context.renew_execution_context();

        // Turn deadline is capped by the parent's tighter one.
        let remaining = context
            .exec
            .remaining_turn_time()
            .expect("turn deadline set");
        assert!(remaining <= std::time::Duration::from_secs(10));
        assert!(remaining > std::time::Duration::from_secs(5));
        // Tool deadline comes from config (capped by the turn remainder).
        let effective = context.exec.effective_deadline(None);
        let expected = std::time::Duration::from_millis(123_000).min(remaining);
        assert!(
            effective <= expected && effective > expected - std::time::Duration::from_secs(1),
            "effective {effective:?} vs expected {expected:?}"
        );
        // Parent cancellation propagates into the renewed context.
        parent.cancel();
        assert!(context.exec.is_cancelled());

        // Without a parent, renew produces a fresh, un-cancelled token even after a
        // previous turn's context was cancelled (one-shot tokens must not leak turns).
        let mut context = make_context(GuardChain::new()).await;
        context.exec.cancel();
        context.renew_execution_context();
        assert!(!context.exec.is_cancelled());
    }

    #[tokio::test]
    async fn blocked_calls_from_all_gates_replay_as_legal_tool_history() {
        // P1-03 acceptance: guard-denied, permission-denied, budget-exhausted and
        // cancelled calls each persist ToolCall + ToolBlocked bound by the native
        // call id, and replaying the persisted log (resume) yields a legal model
        // history — every tool_use answered by a failure tool_result carrying the
        // block reason.
        use holmes_core::execution_context::{ExecutionContext, ResourceBudget};

        fn gate_call(id: &str) -> ToolCall {
            ToolCall {
                id: id.into(),
                call_type: "function".into(),
                function: FunctionCall {
                    name: "mock_tool".into(),
                    arguments: "{}".into(),
                },
            }
        }

        let mut guards = GuardChain::new();
        guards.pre.push(Box::new(BlockGuard));
        let mut context = make_context(guards).await;
        let engine = ActionEngine::new();
        let mut sink = VecSink::new();

        // 1. Guard denied.
        engine
            .execute_batch(&mut context, &[gate_call("gate-guard")], &mut sink)
            .await
            .expect("guard batch");
        // 2. Permission denied (permission gate fires before the guard).
        context.config.permissions.mode = holmes_core::config::PermissionMode::Plan;
        engine
            .execute_batch(&mut context, &[gate_call("gate-permission")], &mut sink)
            .await
            .expect("permission batch");
        // 3. Budget exhausted (budget gate fires before permission).
        context.exec = ExecutionContext::new("budgeted").with_budget(ResourceBudget {
            max_tool_calls: Some(0),
        });
        engine
            .execute_batch(&mut context, &[gate_call("gate-budget")], &mut sink)
            .await
            .expect("budget batch");
        // 4. Cancelled.
        context.exec.cancel();
        engine
            .execute_batch(&mut context, &[gate_call("gate-cancelled")], &mut sink)
            .await
            .expect("cancelled batch");

        let stored = context
            .session_db
            .get_events(&context.session_id)
            .await
            .expect("stored events");
        assert_eq!(stored.len(), 8);
        for pair in stored.chunks(2) {
            let (
                Event::ToolCall { call_id, .. },
                Event::ToolBlocked {
                    call_id: blocked_id,
                    ..
                },
            ) = (&pair[0].event, &pair[1].event)
            else {
                panic!("expected ToolCall + ToolBlocked pair, got {pair:?}");
            };
            assert!(call_id.is_some());
            assert_eq!(call_id, blocked_id);
        }

        // Resume: replay the persisted log.
        let replayed = holmes_session::replay::replay_events(&context.session_id, &stored);
        let messages = &replayed.session.messages;
        let answered: std::collections::HashSet<&str> = messages
            .iter()
            .filter(|message| message.role == holmes_core::Role::Tool)
            .filter_map(|message| message.tool_call_id.as_deref())
            .collect();
        for message in messages {
            if message.role != holmes_core::Role::Assistant {
                continue;
            }
            for call in message.tool_calls.as_deref().unwrap_or(&[]) {
                assert!(
                    answered.contains(call.id.as_str()),
                    "tool_use '{}' has no tool_result",
                    call.id
                );
            }
        }

        let result_content = |id: &str| {
            messages
                .iter()
                .find(|message| message.tool_call_id.as_deref() == Some(id))
                .and_then(|message| message.content.as_deref())
                .expect("blocked result content")
                .to_string()
        };
        assert!(result_content("gate-guard").contains("blocked by test"));
        assert!(result_content("gate-permission").contains("permission mode"));
        assert!(result_content("gate-budget").contains("budget"));
        assert!(result_content("gate-cancelled").contains("cancelled"));
    }
}
