use crate::registry::Tool;
use anyhow::Result;
use async_trait::async_trait;
use holmes_core::background::{BackgroundTasks, DurableTaskBinding, TaskState, TaskStatus};
use holmes_core::execution_context::ExecutionContext;
use holmes_core::subagent::{wrap_for_parent, AgentTaskResult, SubagentLimits, SubagentRunner};
use holmes_core::types::SubAgentTask;
use holmes_core::{FunctionDefinition, ToolDefinition};
use std::sync::Arc;

/// Upper bound on `get_task_output`'s `wait_seconds` — long enough to outlast a
/// typical subagent turn, short enough that a forgotten wait can't park the agent.
const MAX_WAIT_SECONDS: f64 = 60.0;
/// Poll interval for the blocking wait. The registry is a plain Mutex map (no async
/// notify), and the cancel flag is a plain AtomicBool, so waiting is a short-poll
/// loop: cheap at this interval, and it lets an operator interrupt (Esc) break the
/// wait within ~100ms instead of being unobservable until the deadline.
const WAIT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
/// Lease heartbeat cadence for durable background tasks (AGT-007). Comfortably
/// below the store's lease duration, so one missed tick never orphans a live task.
const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

pub struct SpawnSubagentTool {
    runner: Arc<dyn SubagentRunner>,
    tasks: BackgroundTasks,
    durable: Option<DurableTaskBinding>,
    limits: SubagentLimits,
    heartbeat_interval: std::time::Duration,
}

impl SpawnSubagentTool {
    pub fn new(runner: Arc<dyn SubagentRunner>, tasks: BackgroundTasks) -> Self {
        Self {
            runner,
            tasks,
            durable: None,
            limits: SubagentLimits::unlimited(),
            heartbeat_interval: HEARTBEAT_INTERVAL,
        }
    }

    /// Mirror every background task's lifecycle into the durable task store
    /// (AGT-007): start is fail-closed, completion/cancellation is written
    /// back, and a heartbeat keeps the lease alive while the runner lives.
    pub fn with_durable_binding(mut self, binding: DurableTaskBinding) -> Self {
        self.durable = Some(binding);
        self
    }

    /// Resource-isolation knobs (AGT-014): a process-wide concurrency semaphore
    /// shared across nesting levels, and the maximum subagent nesting depth.
    pub fn with_limits(mut self, limits: SubagentLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Override the durable-lease heartbeat cadence. Tests use a short interval
    /// to exercise the lost-lease stop path without waiting out the 60s default.
    pub fn with_heartbeat_interval(mut self, interval: std::time::Duration) -> Self {
        self.heartbeat_interval = interval;
        self
    }

    /// AGT-014 admission control, applied to both spawn modes: nesting depth and
    /// the shared concurrency semaphore. A refused spawn fails synchronously so
    /// the model can retry later or run the work itself.
    fn admit(
        &self,
        ctx: &ExecutionContext,
    ) -> std::result::Result<tokio::sync::OwnedSemaphorePermit, anyhow::Error> {
        if ctx.depth() >= self.limits.max_depth {
            holmes_core::metrics::metrics().count("subagent.spawn_rejected");
            tracing::warn!(
                event = "SubagentSpawnRejected",
                reason = "max_depth",
                depth = ctx.depth(),
                max_depth = self.limits.max_depth,
                task_id = %ctx.task_id(),
                "subagent spawn refused: nesting depth limit"
            );
            return Err(anyhow::anyhow!(
                "subagent spawn refused: nesting depth {} reached the configured limit {}; run the task in this agent instead",
                ctx.depth(),
                self.limits.max_depth
            ));
        }
        match self.limits.slots.clone().try_acquire_owned() {
            Ok(permit) => Ok(permit),
            Err(_) => {
                holmes_core::metrics::metrics().count("subagent.spawn_rejected");
                tracing::warn!(
                    event = "SubagentSpawnRejected",
                    reason = "max_concurrent",
                    task_id = %ctx.task_id(),
                    "subagent spawn refused: concurrency limit"
                );
                Err(anyhow::anyhow!(
                    "subagent spawn refused: the configured concurrent subagent limit is reached; wait for a running subagent to finish or use run_in_background=false later"
                ))
            }
        }
    }
}

/// Verify a finished subagent result for parent consumption (AGT-013): attaches
/// the deterministic verdict, downgrades a defective `Completed` to `Partial`,
/// and emits the structured audit event either way.
fn verify_for_parent(result: AgentTaskResult) -> holmes_core::subagent::VerifiedAgentTaskResult {
    let task_id = result.task_id.clone();
    let verified = wrap_for_parent(result);
    if verified.verification.passed {
        holmes_core::metrics::metrics().count("subagent.result_verified");
        tracing::info!(
            event = "SubagentResultVerified",
            task_id = %task_id,
            status = ?verified.result.status,
            "subagent result passed deterministic verification"
        );
    } else {
        holmes_core::metrics::metrics().count("subagent.result_rejected");
        tracing::warn!(
            event = "SubagentResultRejected",
            task_id = %task_id,
            status = ?verified.result.status,
            defects = ?verified.verification.defects,
            "subagent result failed deterministic verification"
        );
    }
    verified
}

/// Metrics every finished subagent run reports (AGT-015): outcome counter keyed
/// by status plus cost samples (wall clock, tokens, tool calls).
fn record_subagent_outcome_metrics(result: &AgentTaskResult) {
    let metrics = holmes_core::metrics::metrics();
    let outcome = match result.status {
        holmes_core::subagent::AgentTaskStatus::Completed => "subagent.completed",
        holmes_core::subagent::AgentTaskStatus::Partial => "subagent.partial",
        holmes_core::subagent::AgentTaskStatus::Failed => "subagent.failed",
        holmes_core::subagent::AgentTaskStatus::Cancelled => "subagent.cancelled",
    };
    metrics.count(outcome);
    metrics.record_ms("subagent.wall_clock_ms", result.usage.wall_clock_ms);
    metrics.record_ms("subagent.tokens_used", result.usage.tokens_used);
    metrics.record_ms("subagent.tool_calls", result.usage.tool_calls);
}

#[async_trait]
impl Tool for SpawnSubagentTool {
    fn name(&self) -> &str {
        "spawn_subagent"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "spawn_subagent".to_string(),
                description: "Spawn an isolated subagent to perform a complex, multi-step task. Use this when a task is too complex, requires multiple file lookups, or causes context bloat for the main agent. To parallelize independent subtasks, emit SEVERAL spawn_subagent calls in the SAME response — they run concurrently and you get all summaries back together. Set run_in_background=true to detach the subagent: the call returns a task id immediately and the result is injected as a system-reminder as soon as the task finishes (no polling needed); use get_task_output to check on it or wait for it explicitly. Prefer the background mode for parallel recon whose results you don't need right away; keep the default synchronous mode when the next step depends on the result.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "task": {
                            "type": "string",
                            "description": "A clear, actionable task description for the subagent."
                        },
                        "context_summary": {
                            "type": "object",
                            "description": "Key information the subagent needs to know to start."
                        },
                        "expected_output": {
                            "type": "object",
                            "properties": {
                                "schema": { "type": "string" },
                                "required_fields": {
                                    "type": "array",
                                    "items": { "type": "string" }
                                }
                            },
                            "required": ["schema", "required_fields"]
                        },
                        "constraints": {
                            "type": "object",
                            "properties": {
                                "max_turns": { "type": "integer", "description": "Maximum allowed turns before aborting." },
                                "tools_allowlist": { "type": "array", "items": { "type": "string" } },
                                "isolation": { "type": "string" }
                            },
                            "required": ["max_turns", "tools_allowlist"]
                        },
                        "run_in_background": {
                            "type": "boolean",
                            "description": "If true, detach the subagent: return a task id immediately and get the result via an automatic system-reminder injection on completion (or via get_task_output). Default false: block until the subagent finishes and return its result directly."
                        }
                    },
                    "required": ["task", "context_summary", "expected_output", "constraints"]
                }),
            },
        }
    }

    fn is_read_only(&self) -> bool {
        // Technically it might spawn a mutating subagent, but the tool itself is an orchestrator.
        false
    }

    async fn execute(&self, args: &str) -> Result<String> {
        self.execute_with_context(args, &ExecutionContext::default())
            .await
    }

    async fn execute_with_context(&self, args: &str, ctx: &ExecutionContext) -> Result<String> {
        let background = serde_json::from_str::<serde_json::Value>(args)
            .ok()
            .and_then(|value| value.get("run_in_background").and_then(|v| v.as_bool()))
            .unwrap_or(false);

        // Admission control first (AGT-014): a refused spawn never starts work.
        // The permit is held for the whole run and released on completion.
        let permit = self.admit(ctx)?;

        if !background {
            // Synchronous path: block until the subagent finishes. The registry's
            // bounded race still applies around this, and the runner derives the
            // subagent's own boundary from `ctx` (cancel propagates in both layers).
            // The result is verified before the parent sees it (AGT-013).
            let task_id = format!("sync-{}", uuid::Uuid::new_v4());
            let child_ctx = ctx.child(task_id.clone());
            let delegated_experiment = serde_json::from_str::<SubAgentTask>(args)
                .ok()
                .is_some_and(|task| task.ledger_assignment.is_some());

            // Best-effort durable mirror (unlike the background path this is not
            // fail-closed: a synchronous subagent's result is returned to the parent
            // turn directly, so a crash mid-run is covered by the parent session's
            // own recovery, not by task-store rediscovery). The fencing token
            // scopes every later write to this attempt.
            let fencing = if let Some(binding) = &self.durable {
                let start = holmes_core::background::DurableTaskStart {
                    task_id: task_id.clone(),
                    description: serde_json::from_str::<SubAgentTask>(args)
                        .map(|t| truncate_description(&t.task))
                        .unwrap_or_else(|_| "subagent task".into()),
                    parent_session_id: binding.parent_session_id.clone(),
                    idempotency_key: None,
                    safe_to_retry: serde_json::from_str::<SubAgentTask>(args)
                        .ok()
                        .and_then(|task| task.ledger_assignment)
                        .is_some_and(|assignment| assignment.safe_to_retry),
                    payload: Some(args.to_string()),
                    experiment: serde_json::from_str::<SubAgentTask>(args)
                        .ok()
                        .and_then(|task| task.ledger_assignment),
                };
                let delegated = start.experiment.is_some();
                match binding.sink.task_started(start).await {
                    Ok(fencing) => Some(fencing),
                    Err(error) if delegated => {
                        return Err(anyhow::anyhow!(
                            "failed to claim delegated Experiment task: {error}"
                        ));
                    }
                    Err(error) => {
                        tracing::warn!(task_id = %task_id, error = %error,
                            "durable mirror for synchronous subagent failed; continuing without it");
                        None
                    }
                }
            } else {
                None
            };

            tracing::info!(
                event = "SubagentStarted",
                task_id = %task_id,
                depth = child_ctx.depth(),
                background = false,
                "subagent run started"
            );
            holmes_core::metrics::metrics().count("subagent.started");

            // Run on its own tokio task so a panicking runner is caught as a
            // JoinError instead of unwinding through the parent's tool batch
            // (AGT-014: one crashing subagent must not take the others down).
            let runner = self.runner.clone();
            let args_owned = args.to_string();
            let run_ctx = child_ctx.clone();
            let handle = tokio::spawn(async move {
                let _permit = permit;
                runner.run_subagent(&args_owned, &run_ctx).await
            });
            let result = match handle.await {
                Ok(Ok(result)) => result,
                Ok(Err(error)) => {
                    if let (Some(binding), Some(fencing)) = (&self.durable, fencing) {
                        if let Err(write_error) = binding
                            .sink
                            .task_completed(&task_id, fencing, &Err(error.clone()))
                            .await
                        {
                            tracing::error!(task_id = %task_id, error = %write_error,
                                "durable failure write-back for synchronous subagent failed");
                        }
                    }
                    return Err(anyhow::anyhow!(error));
                }
                Err(join_error) => {
                    holmes_core::metrics::metrics().count("subagent.panicked");
                    tracing::error!(
                        event = "SubagentPanicked",
                        task_id = %task_id,
                        error = %join_error,
                        "subagent runner task failed to join"
                    );
                    if let (Some(binding), Some(fencing)) = (&self.durable, fencing) {
                        let failure = format!("subagent run failed to join: {join_error}");
                        if let Err(write_error) = binding
                            .sink
                            .task_completed(&task_id, fencing, &Err(failure))
                            .await
                        {
                            tracing::error!(task_id = %task_id, error = %write_error,
                                "durable panic write-back for synchronous subagent failed");
                        }
                    }
                    return Err(anyhow::anyhow!("subagent run failed to join: {join_error}"));
                }
            };

            record_subagent_outcome_metrics(&result);
            tracing::info!(
                event = "SubagentCompleted",
                task_id = %task_id,
                status = ?result.status,
                tokens_used = result.usage.tokens_used,
                tool_calls = result.usage.tool_calls,
                wall_clock_ms = result.usage.wall_clock_ms,
                "subagent run finished"
            );

            let verified = verify_for_parent(result);
            let output = serde_json::to_string_pretty(&verified)?;
            if let (Some(binding), Some(fencing)) = (&self.durable, fencing) {
                if let Some(checkpoint) = &verified.result.checkpoint {
                    if let Err(error) = binding
                        .sink
                        .task_attached_session(&task_id, fencing, checkpoint)
                        .await
                    {
                        tracing::warn!(task_id = %task_id, error = %error,
                            "durable child-session link failed");
                    }
                }
                if let Err(error) = binding
                    .sink
                    .task_completed(&task_id, fencing, &Ok(output.clone()))
                    .await
                {
                    if delegated_experiment {
                        return Err(anyhow::anyhow!(
                            "delegated Experiment completion was not durably accepted: {error}"
                        ));
                    }
                    tracing::warn!(task_id = %task_id, error = %error,
                        "durable completion write-back for synchronous subagent failed");
                }
            }
            return Ok(output);
        }

        // Background mode (grok-build detached subagents). Validate the task schema up
        // front so a malformed request fails synchronously instead of dying inside the
        // detached task; the runner re-validates anyway, so error semantics match the
        // synchronous path.
        let task: SubAgentTask = serde_json::from_str(args)
            .map_err(|e| anyhow::anyhow!("Failed to parse task: {}", e))?;
        let description = truncate_description(&task.task);
        let task_id = self.tasks.register(description.clone());

        // Durability before acknowledgement (AGT-007): the start must be
        // persisted before the model sees success, otherwise a crash would
        // leave a running task no recovery pass could ever find. Fail closed:
        // wind the in-memory registration back to a terminal error so nothing
        // stays Running anywhere. The returned fencing token scopes every
        // later durable write to this attempt (P1-02).
        let fencing = if let Some(binding) = &self.durable {
            let start = holmes_core::background::DurableTaskStart {
                task_id: task_id.clone(),
                description: description.clone(),
                parent_session_id: binding.parent_session_id.clone(),
                idempotency_key: None,
                // A subagent drives arbitrary tools with external side effects;
                // re-executing it blindly after a crash could duplicate them,
                // so recovery must suspend (manual_recovery_required), not retry.
                safe_to_retry: task
                    .ledger_assignment
                    .as_ref()
                    .is_some_and(|assignment| assignment.safe_to_retry),
                payload: Some(args.to_string()),
                experiment: task.ledger_assignment.clone(),
            };
            match binding.sink.task_started(start).await {
                Ok(fencing) => Some(fencing),
                Err(error) => {
                    self.tasks.complete(
                        &task_id,
                        Err(format!("durable task store rejected start: {error}")),
                    );
                    return Err(anyhow::anyhow!(
                        "failed to persist background task start (fail-closed): {error}"
                    ));
                }
            }
        } else {
            None
        };

        // No queue: admission control above caps concurrency, and the turn's
        // iteration budget plus the tool batch size bound how many background
        // subagents a model can realistically keep in flight.
        let runner = self.runner.clone();
        let tasks = self.tasks.clone();
        let durable = self.durable.clone();
        let heartbeat_interval = self.heartbeat_interval;
        let id = task_id.clone();
        let args_owned = args.to_string();
        // The detached task runs under a child boundary: cancelling the parent turn
        // resolves the race below and completes the task as cancelled instead of
        // leaving a Running orphan (AGT-002).
        let child_ctx = ctx.child(task_id.clone());
        tracing::info!(
            event = "SubagentStarted",
            task_id = %task_id,
            depth = child_ctx.depth(),
            background = true,
            "subagent run started"
        );
        holmes_core::metrics::metrics().count("subagent.started");
        let run_handle = tokio::spawn(async move {
            let _permit = permit;
            let cancelled = child_ctx.token();
            let run = runner.run_subagent(&args_owned, &child_ctx);
            tokio::pin!(run);
            let mut heartbeat = tokio::time::interval(heartbeat_interval);
            heartbeat.tick().await; // consume the immediate first tick
                                    // Set when a heartbeat reports the lease was reclaimed or
                                    // superseded (P1-02): the worker is cancelled and MUST NOT write
                                    // to the sink again — the new attempt owns every later transition.
            let mut lost_lease = false;
            let result = loop {
                tokio::select! {
                    result = &mut run => break result.and_then(|r| {
                        record_subagent_outcome_metrics(&r);
                        tracing::info!(
                            event = "SubagentCompleted",
                            task_id = %id,
                            status = ?r.status,
                            tokens_used = r.usage.tokens_used,
                            tool_calls = r.usage.tool_calls,
                            wall_clock_ms = r.usage.wall_clock_ms,
                            "subagent run finished"
                        );
                        let verified = verify_for_parent(r);
                        serde_json::to_string_pretty(&verified).map_err(|e| e.to_string())
                    }),
                    _ = cancelled.cancelled() => {
                        break Err("cancelled: parent turn was interrupted".into());
                    }
                    _ = heartbeat.tick() => {
                        if let (Some(binding), Some(fencing)) = (&durable, fencing) {
                            match binding.sink.task_heartbeat(&id, fencing).await {
                                Ok(true) => {}
                                Ok(false) => {
                                    // Lost lease: stop now. The token cancel
                                    // winds the runner down through the normal
                                    // cancellation path; every later durable
                                    // write would be fenced out anyway, and we
                                    // skip them explicitly below.
                                    lost_lease = true;
                                    tracing::warn!(
                                        event = "DurableLeaseLost",
                                        task_id = %id,
                                        "durable task lease lost (reclaimed or superseded); stopping worker"
                                    );
                                    holmes_core::metrics::metrics().count("task.lease_lost");
                                    child_ctx.cancel();
                                    break Err("cancelled: durable task lease lost to another attempt".into());
                                }
                                Err(error) => {
                                    // Transient store failure: the next tick
                                    // (or recovery) sorts it out.
                                    tracing::warn!(task_id = %id, error = %error,
                                        "durable task heartbeat failed");
                                }
                            }
                        }
                    }
                }
            };
            tasks.complete(&id, result.clone());
            if lost_lease {
                // Another attempt owns the task now: its completion (not ours)
                // is what gets delivered.
                return;
            }
            if let (Some(binding), Some(fencing)) = (&durable, fencing) {
                if let Err(error) = binding.sink.task_completed(&id, fencing, &result).await {
                    tracing::error!(task_id = %id, error = %error,
                        "durable task completion write-back failed; restart recovery will treat the lease as orphaned");
                }
            }
        });
        // Join supervisor (AGT-014): a panicking subagent task must surface as a
        // failed result, not vanish — without this the registry entry would stay
        // Running forever and sibling subagents would be unaffected but blind.
        let tasks = self.tasks.clone();
        let durable = self.durable.clone();
        let id = task_id.clone();
        tokio::spawn(async move {
            if let Err(join_error) = run_handle.await {
                let payload = if join_error.is_panic() {
                    format!("subagent panicked: {join_error}")
                } else {
                    format!("subagent task failed to join: {join_error}")
                };
                holmes_core::metrics::metrics().count("subagent.panicked");
                tracing::error!(
                    event = "SubagentPanicked",
                    task_id = %id,
                    error = %join_error,
                    "background subagent task failed to join"
                );
                tasks.complete(&id, Err(payload.clone()));
                if let (Some(binding), Some(fencing)) = (&durable, fencing) {
                    // Fenced: if the run task lost its lease before panicking,
                    // this write is rejected and the owning attempt prevails.
                    if let Err(error) = binding
                        .sink
                        .task_completed(&id, fencing, &Err(payload))
                        .await
                    {
                        tracing::error!(task_id = %id, error = %error,
                            "durable write-back for panicked subagent failed");
                    }
                }
            }
        });

        Ok(format!(
            "background task started: {task_id} ({description})\n\
             The result will be injected as a system-reminder when the task completes. \
             Use get_task_output with this task id to check progress or wait for the result."
        ))
    }
}

/// Truncate a task description for registry/display purposes (char boundary safe).
fn truncate_description(task: &str) -> String {
    const CAP: usize = 60;
    let collapsed = task.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= CAP {
        return collapsed;
    }
    format!("{}…", collapsed.chars().take(CAP).collect::<String>())
}

pub struct GetTaskOutputTool {
    tasks: BackgroundTasks,
}

impl GetTaskOutputTool {
    pub fn new(tasks: BackgroundTasks) -> Self {
        Self { tasks }
    }
}

#[async_trait]
impl Tool for GetTaskOutputTool {
    fn name(&self) -> &str {
        "get_task_output"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "get_task_output".to_string(),
                description: "Check on a background task started by spawn_subagent with run_in_background=true. Returns the task's full result once it has completed; while it is still running, either returns a status line immediately (wait_seconds omitted or 0) or blocks up to wait_seconds (max 60) for completion. Completed results are also injected automatically as system-reminders, so only call this when you need the result sooner.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "task_id": {
                            "type": "string",
                            "description": "The task id returned by spawn_subagent."
                        },
                        "wait_seconds": {
                            "type": "number",
                            "description": "How long to block waiting for completion (0 = return immediately, max 60). Default 0."
                        }
                    },
                    "required": ["task_id"]
                }),
            },
        }
    }

    fn is_read_only(&self) -> bool {
        // Pure registry read (plus a bounded wait) — safe inside parallel batches.
        true
    }

    async fn execute(&self, args: &str) -> Result<String> {
        let parsed: serde_json::Value = serde_json::from_str(args)
            .map_err(|e| anyhow::anyhow!("invalid get_task_output arguments: {}", e))?;
        let task_id = parsed
            .get("task_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing required string argument: task_id"))?;
        let wait_seconds = parsed
            .get("wait_seconds")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0)
            .clamp(0.0, MAX_WAIT_SECONDS);

        let Some(state) = self.tasks.snapshot(task_id) else {
            return Err(anyhow::anyhow!("unknown background task: {task_id}"));
        };
        if let TaskStatus::Completed(result) = &state.status {
            return Ok(match result {
                Ok(output) => output.clone(),
                Err(error) => format!("task failed: {error}"),
            });
        }
        if wait_seconds <= 0.0 {
            return Ok(format!(
                "task still running: {task_id} (\"{}\") — call again with wait_seconds to block until it finishes",
                state.description
            ));
        }

        // Short-poll wait (see WAIT_POLL_INTERVAL): checks completion and the cancel
        // flag each tick so an operator interrupt breaks the wait early.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs_f64(wait_seconds);
        loop {
            tokio::time::sleep(WAIT_POLL_INTERVAL).await;
            if self.tasks.is_cancelled() {
                return Ok(format!(
                    "wait interrupted by operator; task still running: {task_id} (\"{}\")",
                    state.description
                ));
            }
            if let Some(TaskState {
                status: TaskStatus::Completed(result),
                ..
            }) = self.tasks.snapshot(task_id)
            {
                return Ok(match result {
                    Ok(output) => output,
                    Err(error) => format!("task failed: {error}"),
                });
            }
            if std::time::Instant::now() >= deadline {
                return Ok(format!(
                    "task still running after waiting {}s: {task_id} (\"{}\")",
                    wait_seconds, state.description
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holmes_core::subagent::{
        AgentTaskStatus, EvidenceRef, Finding, ResourceUsage, ValidationResult,
    };
    use std::sync::atomic::{AtomicBool, Ordering};

    fn clean_result(task_id: &str) -> AgentTaskResult {
        AgentTaskResult {
            task_id: task_id.into(),
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
                note: Some("error-based payload returned the users table".into()),
            }],
            changed_files: vec![],
            validations: vec![ValidationResult {
                name: "goal_evaluated".into(),
                passed: true,
                detail: Some("all subtasks done".into()),
            }],
            remaining_work: vec![],
            usage: ResourceUsage {
                tokens_used: 100,
                tool_calls: 3,
                turns: 2,
                wall_clock_ms: 50,
            },
            checkpoint: Some("sub-test".into()),
        }
    }

    /// Runner whose completion the test controls: waits until `release` is set (or
    /// `block_forever`), then returns a fixed result or error.
    struct GatedRunner {
        release: Arc<AtomicBool>,
        block_forever: bool,
        fail: bool,
    }

    #[async_trait]
    impl SubagentRunner for GatedRunner {
        async fn run_subagent(
            &self,
            _args: &str,
            ctx: &ExecutionContext,
        ) -> std::result::Result<AgentTaskResult, String> {
            if self.block_forever {
                std::future::pending::<()>().await;
            }
            while !self.release.load(Ordering::Relaxed) {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            if self.fail {
                return Err("subagent exploded".into());
            }
            Ok(clean_result(ctx.task_id()))
        }
    }

    fn task_args(run_in_background: bool) -> String {
        serde_json::json!({
            "task": "scan the login endpoint for auth bypass",
            "context_summary": {"target": "https://example.test"},
            "expected_output": {"schema": "text", "required_fields": ["summary"]},
            "constraints": {"max_turns": 4, "tools_allowlist": ["http_request"]},
            "run_in_background": run_in_background
        })
        .to_string()
    }

    fn extract_task_id(output: &str) -> String {
        output
            .strip_prefix("background task started: ")
            .and_then(|rest| rest.split(' ').next())
            .expect("task id in tool output")
            .to_string()
    }

    async fn wait_for_completion(tasks: &BackgroundTasks, task_id: &str) -> TaskState {
        // Generous budget: heartbeat-driven paths (lost-lease stop) wait one
        // scheduler tick plus scheduling slack on loaded CI machines.
        for _ in 0..500 {
            if let Some(state) = tasks.snapshot(task_id) {
                if matches!(state.status, TaskStatus::Completed(_)) {
                    return state;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("task {task_id} did not complete in time");
    }

    #[tokio::test]
    async fn background_spawn_returns_immediately_and_registers_task() {
        let release = Arc::new(AtomicBool::new(false));
        let runner = Arc::new(GatedRunner {
            release: release.clone(),
            block_forever: false,
            fail: false,
        });
        let tasks = BackgroundTasks::new();
        let tool = SpawnSubagentTool::new(runner, tasks.clone());

        let output = tool.execute(&task_args(true)).await.expect("spawn ok");
        let task_id = extract_task_id(&output);
        assert!(output.contains("scan the login endpoint"));

        // Runner is gated → the task is registered as Running right after spawn.
        let state = tasks.snapshot(&task_id).expect("task registered");
        assert!(matches!(state.status, TaskStatus::Running));
        assert_eq!(tasks.running_count(), 1);

        // Release the runner; the detached task writes the outcome into the registry.
        release.store(true, Ordering::Relaxed);
        let state = wait_for_completion(&tasks, &task_id).await;
        match state.status {
            TaskStatus::Completed(Ok(output)) => {
                assert!(output.contains("recon complete"), "{output}");
                assert!(output.contains("\"verification\""), "{output}");
            }
            other => panic!("expected completed task, got {other:?}"),
        }
        assert_eq!(tasks.running_count(), 0);
    }

    #[tokio::test]
    async fn background_spawn_records_runner_failure_as_task_error() {
        let runner = Arc::new(GatedRunner {
            release: Arc::new(AtomicBool::new(true)),
            block_forever: false,
            fail: true,
        });
        let tasks = BackgroundTasks::new();
        let tool = SpawnSubagentTool::new(runner, tasks.clone());

        let output = tool.execute(&task_args(true)).await.expect("spawn ok");
        let task_id = extract_task_id(&output);
        let state = wait_for_completion(&tasks, &task_id).await;
        match state.status {
            TaskStatus::Completed(Err(error)) => assert!(error.contains("subagent exploded")),
            other => panic!("expected failed task, got {other:?}"),
        }

        // The failure is queryable like any completed result.
        let lookup = GetTaskOutputTool::new(tasks);
        let output = lookup
            .execute(&serde_json::json!({"task_id": task_id}).to_string())
            .await
            .expect("failed task lookup");
        assert!(output.contains("task failed"), "{output}");
        assert!(output.contains("subagent exploded"), "{output}");
    }

    #[tokio::test]
    async fn background_task_completes_as_cancelled_when_parent_turn_cancels() {
        // Parent turn cancelled while the subagent blocks forever: the detached task
        // must resolve as a cancelled completion, not linger as Running (AGT-002).
        let runner = Arc::new(GatedRunner {
            release: Arc::new(AtomicBool::new(false)),
            block_forever: true,
            fail: false,
        });
        let tasks = BackgroundTasks::new();
        let tool = SpawnSubagentTool::new(runner, tasks.clone());
        let ctx = ExecutionContext::new("parent-turn");

        let output = tool
            .execute_with_context(&task_args(true), &ctx)
            .await
            .expect("spawn ok");
        let task_id = extract_task_id(&output);
        assert!(matches!(
            tasks.snapshot(&task_id).map(|s| s.status),
            Some(TaskStatus::Running)
        ));

        ctx.cancel();
        let state = wait_for_completion(&tasks, &task_id).await;
        match state.status {
            TaskStatus::Completed(Err(error)) => {
                assert!(error.contains("cancelled"), "got: {error}")
            }
            other => panic!("expected cancelled completion, got {other:?}"),
        }
        assert_eq!(tasks.running_count(), 0);
    }

    #[tokio::test]
    async fn background_spawn_validates_schema_synchronously() {
        let runner = Arc::new(GatedRunner {
            release: Arc::new(AtomicBool::new(true)),
            block_forever: false,
            fail: false,
        });
        let tool = SpawnSubagentTool::new(runner, BackgroundTasks::new());
        let result = tool.execute(r#"{"run_in_background": true}"#).await;
        assert!(
            result.is_err(),
            "malformed background task must fail synchronously"
        );
    }

    #[tokio::test]
    async fn sync_spawn_returns_verified_structured_result() {
        let runner = Arc::new(GatedRunner {
            release: Arc::new(AtomicBool::new(true)),
            block_forever: false,
            fail: false,
        });
        let tasks = BackgroundTasks::new();
        let tool = SpawnSubagentTool::new(runner, tasks.clone());

        let output = tool
            .execute(&task_args(false))
            .await
            .expect("sync spawn ok");
        let parsed: serde_json::Value = serde_json::from_str(&output).expect("json result");
        assert_eq!(parsed["status"], "completed");
        assert_eq!(parsed["summary"], "recon complete");
        assert_eq!(parsed["verification"]["passed"], true);
        assert_eq!(parsed["usage"]["tool_calls"], 3);
        assert_eq!(parsed["findings"][0]["evidence_refs"][0], "f-1");
        assert!(!output.contains("background task started"));
        assert_eq!(
            tasks.running_count(),
            0,
            "sync path never touches the registry"
        );
    }

    #[tokio::test]
    async fn parent_cancellation_propagates_into_sync_subagent() {
        // The runner observes the cancellation through the token derived from the
        // parent context and reports a structured Cancelled result (AGT-002/014).
        struct CancelAwareRunner;
        #[async_trait]
        impl SubagentRunner for CancelAwareRunner {
            async fn run_subagent(
                &self,
                _args: &str,
                ctx: &ExecutionContext,
            ) -> std::result::Result<AgentTaskResult, String> {
                ctx.token().cancelled().await;
                let mut result = clean_result(ctx.task_id());
                result.status = AgentTaskStatus::Cancelled;
                result.summary = "subagent run cancelled".into();
                result.remaining_work = vec!["run cancelled before completion".into()];
                Ok(result)
            }
        }

        let tool = SpawnSubagentTool::new(Arc::new(CancelAwareRunner), BackgroundTasks::new());
        let parent = ExecutionContext::new("parent-turn");
        let canceller = parent.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            canceller.cancel();
        });
        let output = tool
            .execute_with_context(&task_args(false), &parent)
            .await
            .expect("cancelled run still returns a structured result");
        let parsed: serde_json::Value = serde_json::from_str(&output).expect("json result");
        assert_eq!(parsed["status"], "cancelled");
        assert!(parsed["remaining_work"]
            .as_array()
            .expect("remaining_work array")
            .iter()
            .any(|w| w.as_str().unwrap_or_default().contains("cancelled")));
    }

    #[tokio::test]
    async fn parent_rejects_false_completion_without_evidence() {
        // Runner claims completion with findings but no evidence and a placeholder
        // summary: the parent-side verifier must downgrade and flag it (AGT-013).
        struct FalseFinishRunner;
        #[async_trait]
        impl SubagentRunner for FalseFinishRunner {
            async fn run_subagent(
                &self,
                _args: &str,
                ctx: &ExecutionContext,
            ) -> std::result::Result<AgentTaskResult, String> {
                Ok(AgentTaskResult {
                    task_id: ctx.task_id().into(),
                    status: AgentTaskStatus::Completed,
                    summary: holmes_core::subagent::NO_ANSWER_PLACEHOLDER.into(),
                    findings: vec![Finding {
                        summary: "critical RCE".into(),
                        severity: Some("critical".into()),
                        evidence_refs: vec![],
                    }],
                    evidence: vec![],
                    changed_files: vec![],
                    validations: vec![],
                    remaining_work: vec![],
                    usage: ResourceUsage::default(),
                    checkpoint: None,
                })
            }
        }

        let tool = SpawnSubagentTool::new(Arc::new(FalseFinishRunner), BackgroundTasks::new());
        let output = tool
            .execute(&task_args(false))
            .await
            .expect("sync spawn ok");
        let parsed: serde_json::Value = serde_json::from_str(&output).expect("json result");
        assert_eq!(parsed["verification"]["passed"], false, "{output}");
        assert_eq!(
            parsed["status"], "partial",
            "defective completion must be downgraded: {output}"
        );
        let defects = parsed["verification"]["defects"]
            .as_array()
            .expect("defects array");
        assert!(defects
            .iter()
            .any(|d| d.as_str().unwrap_or_default().contains("evidence")));
        assert!(parsed["remaining_work"]
            .as_array()
            .expect("remaining_work array")
            .iter()
            .any(|w| w.as_str().unwrap_or_default().starts_with("verification:")));
    }

    #[tokio::test]
    async fn budget_exhausted_partial_result_carries_remaining_work() {
        // A subagent that runs out of budget reports Partial with remaining_work;
        // the protocol must carry it through to the parent untouched (AGT-013/014).
        struct BudgetExhaustedRunner;
        #[async_trait]
        impl SubagentRunner for BudgetExhaustedRunner {
            async fn run_subagent(
                &self,
                _args: &str,
                ctx: &ExecutionContext,
            ) -> std::result::Result<AgentTaskResult, String> {
                Ok(AgentTaskResult {
                    task_id: ctx.task_id().into(),
                    status: AgentTaskStatus::Partial,
                    summary: "scanned 2 of 5 endpoints before the tool budget ran out".into(),
                    findings: vec![],
                    evidence: vec![],
                    changed_files: vec![],
                    validations: vec![],
                    remaining_work: vec![
                        "scan remaining 3 endpoints".into(),
                        "run stopped before completion: tool budget exhausted".into(),
                    ],
                    usage: ResourceUsage {
                        tokens_used: 500,
                        tool_calls: 10,
                        turns: 4,
                        wall_clock_ms: 900,
                    },
                    checkpoint: Some("sub-budget".into()),
                })
            }
        }

        let tool = SpawnSubagentTool::new(Arc::new(BudgetExhaustedRunner), BackgroundTasks::new());
        let output = tool
            .execute(&task_args(false))
            .await
            .expect("sync spawn ok");
        let parsed: serde_json::Value = serde_json::from_str(&output).expect("json result");
        assert_eq!(parsed["status"], "partial");
        assert_eq!(parsed["verification"]["passed"], true, "{output}");
        let remaining = parsed["remaining_work"].as_array().expect("array");
        assert!(remaining
            .iter()
            .any(|w| w.as_str().unwrap_or_default().contains("budget exhausted")));
        assert_eq!(parsed["checkpoint"], "sub-budget");
    }

    #[tokio::test]
    async fn panicking_subagent_does_not_affect_siblings() {
        // AGT-014: one crashing subagent resolves as a failed task; a sibling
        // started alongside still completes normally.
        struct PanicRunner;
        #[async_trait]
        impl SubagentRunner for PanicRunner {
            async fn run_subagent(
                &self,
                _args: &str,
                _ctx: &ExecutionContext,
            ) -> std::result::Result<AgentTaskResult, String> {
                panic!("subagent exploded mid-run");
            }
        }

        let tasks = BackgroundTasks::new();
        let panicking = SpawnSubagentTool::new(Arc::new(PanicRunner), tasks.clone());
        let healthy = SpawnSubagentTool::new(
            Arc::new(GatedRunner {
                release: Arc::new(AtomicBool::new(true)),
                block_forever: false,
                fail: false,
            }),
            tasks.clone(),
        );

        let panic_output = panicking.execute(&task_args(true)).await.expect("spawn ok");
        let healthy_output = healthy.execute(&task_args(true)).await.expect("spawn ok");
        let panic_id = extract_task_id(&panic_output);
        let healthy_id = extract_task_id(&healthy_output);

        let panic_state = wait_for_completion(&tasks, &panic_id).await;
        match panic_state.status {
            TaskStatus::Completed(Err(error)) => {
                assert!(error.contains("panicked"), "got: {error}")
            }
            other => panic!("expected panic failure, got {other:?}"),
        }
        let healthy_state = wait_for_completion(&tasks, &healthy_id).await;
        match healthy_state.status {
            TaskStatus::Completed(Ok(output)) => assert!(output.contains("recon complete")),
            other => panic!("sibling must complete normally, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn spawn_refused_beyond_max_depth() {
        let runner = Arc::new(GatedRunner {
            release: Arc::new(AtomicBool::new(true)),
            block_forever: false,
            fail: false,
        });
        let limits = SubagentLimits::new(2, 4);
        let tool = SpawnSubagentTool::new(runner, BackgroundTasks::new()).with_limits(limits);
        let ctx = ExecutionContext::new("deep-turn").with_depth(2);
        let result = tool.execute_with_context(&task_args(false), &ctx).await;
        let error = result.expect_err("spawn at max depth must be refused");
        assert!(error.to_string().contains("nesting depth"), "{error}");
    }

    #[tokio::test]
    async fn spawn_refused_beyond_concurrency_limit() {
        let release = Arc::new(AtomicBool::new(false));
        let runner = Arc::new(GatedRunner {
            release: release.clone(),
            block_forever: false,
            fail: false,
        });
        let limits = SubagentLimits::new(4, 1);
        let tasks = BackgroundTasks::new();
        let tool = SpawnSubagentTool::new(runner, tasks.clone()).with_limits(limits);

        // First background spawn holds the only slot while the runner is gated.
        let first = tool
            .execute(&task_args(true))
            .await
            .expect("first spawn ok");
        let first_id = extract_task_id(&first);
        // Give the detached task a moment to take the permit.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let second = tool.execute(&task_args(true)).await;
        let error = second.expect_err("second spawn must hit the concurrency limit");
        assert!(error.to_string().contains("concurrent"), "{error}");

        // Releasing the runner frees the slot and later spawns succeed again.
        release.store(true, Ordering::Relaxed);
        wait_for_completion(&tasks, &first_id).await;
        let third = tool.execute(&task_args(false)).await;
        assert!(third.is_ok(), "slot must be released after completion");
    }

    #[tokio::test]
    async fn get_task_output_returns_completed_result() {
        let tasks = BackgroundTasks::new();
        let id = tasks.register("recon".into());
        tasks.complete(&id, Ok("full result body".into()));
        let tool = GetTaskOutputTool::new(tasks);
        let output = tool
            .execute(&serde_json::json!({"task_id": id}).to_string())
            .await
            .expect("completed lookup");
        assert_eq!(output, "full result body");
    }

    #[tokio::test]
    async fn get_task_output_waits_for_completion() {
        let tasks = BackgroundTasks::new();
        let id = tasks.register("recon".into());
        let writer = tasks.clone();
        let writer_id = id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            writer.complete(&writer_id, Ok("waited result".into()));
        });
        let tool = GetTaskOutputTool::new(tasks);
        let output = tool
            .execute(&serde_json::json!({"task_id": id, "wait_seconds": 5}).to_string())
            .await
            .expect("waited lookup");
        assert_eq!(output, "waited result");
    }

    #[tokio::test]
    async fn get_task_output_times_out_on_running_task() {
        let tasks = BackgroundTasks::new();
        let id = tasks.register("slow recon".into());
        let tool = GetTaskOutputTool::new(tasks);
        let start = std::time::Instant::now();
        let output = tool
            .execute(&serde_json::json!({"task_id": id, "wait_seconds": 0.3}).to_string())
            .await
            .expect("timeout lookup");
        assert!(output.contains("still running"), "got: {output}");
        assert!(start.elapsed() >= std::time::Duration::from_millis(300));
    }

    #[tokio::test]
    async fn get_task_output_unknown_task_errors() {
        let tool = GetTaskOutputTool::new(BackgroundTasks::new());
        let result = tool
            .execute(&serde_json::json!({"task_id": "nope"}).to_string())
            .await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("unknown background task"));
    }

    #[tokio::test]
    async fn get_task_output_wait_breaks_on_cancel() {
        let flag = Arc::new(AtomicBool::new(false));
        let tasks = BackgroundTasks::with_cancel(flag.clone());
        let id = tasks.register("recon".into());
        let tool = GetTaskOutputTool::new(tasks);
        let setter = flag.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            setter.store(true, Ordering::Relaxed);
        });
        let start = std::time::Instant::now();
        let output = tool
            .execute(&serde_json::json!({"task_id": id, "wait_seconds": 60}).to_string())
            .await
            .expect("cancelled wait returns");
        assert!(output.contains("interrupted"), "got: {output}");
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
    }

    /// In-memory durable sink recording every call (AGT-013): the synchronous
    /// spawn path must mirror start / child-session attach / completion into the
    /// durable task store too, not just the background path.
    #[derive(Default)]
    struct RecordingSink {
        started: std::sync::Mutex<Vec<String>>,
        attached: std::sync::Mutex<Vec<(String, String)>>,
        completed: std::sync::Mutex<Vec<(String, std::result::Result<String, String>)>>,
        /// When set, heartbeats report lease loss (P1-02 lost-lease stop path).
        heartbeat_ok: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl holmes_core::background::DurableTaskSink for RecordingSink {
        async fn task_started(
            &self,
            start: holmes_core::background::DurableTaskStart,
        ) -> std::result::Result<u64, String> {
            self.started.lock().unwrap().push(start.task_id);
            Ok(1)
        }

        async fn task_completed(
            &self,
            task_id: &str,
            _fencing: u64,
            result: &std::result::Result<String, String>,
        ) -> std::result::Result<(), String> {
            self.completed
                .lock()
                .unwrap()
                .push((task_id.to_string(), result.clone()));
            Ok(())
        }

        async fn task_heartbeat(
            &self,
            _task_id: &str,
            _fencing: u64,
        ) -> std::result::Result<bool, String> {
            Ok(self.heartbeat_ok.load(Ordering::Relaxed))
        }

        async fn task_attached_session(
            &self,
            task_id: &str,
            _fencing: u64,
            child_session_id: &str,
        ) -> std::result::Result<(), String> {
            self.attached
                .lock()
                .unwrap()
                .push((task_id.to_string(), child_session_id.to_string()));
            Ok(())
        }
    }

    #[tokio::test]
    async fn sync_spawn_mirrors_result_into_durable_task_store() {
        let runner = Arc::new(GatedRunner {
            release: Arc::new(AtomicBool::new(true)),
            block_forever: false,
            fail: false,
        });
        let sink = Arc::new(RecordingSink {
            heartbeat_ok: std::sync::atomic::AtomicBool::new(true),
            ..Default::default()
        });
        let binding = holmes_core::background::DurableTaskBinding {
            sink: sink.clone(),
            parent_session_id: Some("parent-session".into()),
        };
        let tool =
            SpawnSubagentTool::new(runner, BackgroundTasks::new()).with_durable_binding(binding);

        let output = tool
            .execute(&task_args(false))
            .await
            .expect("sync spawn ok");

        let started = sink.started.lock().unwrap();
        assert_eq!(started.len(), 1, "sync spawn persists its start");
        let task_id = started[0].clone();
        drop(started);

        let attached = sink.attached.lock().unwrap();
        assert_eq!(
            attached.as_slice(),
            &[(task_id.clone(), "sub-test".to_string())],
            "checkpoint handle is linked as the child session"
        );
        drop(attached);

        let completed = sink.completed.lock().unwrap();
        assert_eq!(completed.len(), 1);
        let (completed_id, payload) = &completed[0];
        assert_eq!(completed_id, &task_id);
        let stored = payload.as_ref().expect("success payload");
        assert_eq!(stored, &output, "stored payload is the parent-facing JSON");
        let parsed: serde_json::Value = serde_json::from_str(stored).expect("stored json");
        assert_eq!(parsed["verification"]["passed"], true);
    }

    #[tokio::test]
    async fn sync_runner_failure_is_written_to_durable_task_before_returning() {
        let runner = Arc::new(GatedRunner {
            release: Arc::new(AtomicBool::new(true)),
            block_forever: false,
            fail: true,
        });
        let sink = Arc::new(RecordingSink {
            heartbeat_ok: std::sync::atomic::AtomicBool::new(true),
            ..Default::default()
        });
        let binding = holmes_core::background::DurableTaskBinding {
            sink: sink.clone(),
            parent_session_id: Some("parent-session".into()),
        };
        let tool =
            SpawnSubagentTool::new(runner, BackgroundTasks::new()).with_durable_binding(binding);

        let error = tool
            .execute(&task_args(false))
            .await
            .expect_err("runner failure must propagate");
        assert!(error.to_string().contains("subagent exploded"));
        let completed = sink.completed.lock().unwrap();
        assert_eq!(completed.len(), 1);
        assert_eq!(
            completed[0].1.as_ref().expect_err("durable failure"),
            "subagent exploded"
        );
    }

    // P1-02: a heartbeat reporting a lost lease must stop the worker
    // immediately and forbid any later durable write from it.
    #[tokio::test]
    async fn background_worker_stops_when_heartbeat_reports_lost_lease() {
        let runner = Arc::new(GatedRunner {
            release: Arc::new(AtomicBool::new(false)),
            block_forever: true,
            fail: false,
        });
        let sink = Arc::new(RecordingSink::default()); // heartbeat_ok = false: lease already gone
        let binding = holmes_core::background::DurableTaskBinding {
            sink: sink.clone(),
            parent_session_id: Some("parent-session".into()),
        };
        let tasks = BackgroundTasks::new();
        let tool = SpawnSubagentTool::new(runner, tasks.clone())
            .with_durable_binding(binding)
            .with_heartbeat_interval(std::time::Duration::from_millis(10));

        let output = tool.execute(&task_args(true)).await.expect("spawn ok");
        let task_id = extract_task_id(&output);

        // The first heartbeat tick reports the loss: the run resolves as
        // cancelled without waiting for the (blocked-forever) runner.
        let state = wait_for_completion(&tasks, &task_id).await;
        match state.status {
            TaskStatus::Completed(Err(error)) => {
                assert!(error.contains("lease lost"), "got: {error}")
            }
            other => panic!("expected lease-lost cancellation, got {other:?}"),
        }
        // Forbidden from writing back: no completion ever reached the sink.
        assert!(
            sink.completed.lock().unwrap().is_empty(),
            "a worker that lost its lease must not write to the sink"
        );
    }
}
