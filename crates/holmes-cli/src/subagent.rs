use async_trait::async_trait;
use chrono::Utc;
use holmes_core::event::Event;
use holmes_core::session::RuntimeSession;
use holmes_core::subagent::{
    build_agent_task_result, AgentTaskResult, SubagentRunner, SubagentStop,
};
use holmes_core::types::{AgentType, SessionMode, SubAgentTask};
use holmes_guards::GuardChain;
use holmes_llm::client::LlmClient;
use holmes_mind_palace::MindPalace;
use holmes_runtime::context::{RuntimeContext, RuntimeState};
use holmes_runtime::deliberation::LlmBackend;
use holmes_runtime::runtime::{AgentRuntime, TurnOutcome};
use holmes_runtime::yield_stream::{RuntimeSink, RuntimeYield};
use holmes_runtime::StreamEvent;
use holmes_session::{memory_store::MemoryStore, SessionStore};
use holmes_tools::registry::ToolRegistry;
use std::sync::Arc;
use tokio::sync::Semaphore;
use uuid::Uuid;

use holmes_core::config::{resolve_attack_model_provider, Config};

fn active_tool_names(registry: &ToolRegistry) -> Vec<String> {
    let mut names = registry
        .definitions()
        .into_iter()
        .map(|definition| definition.function.name)
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    names
}

/// Apply the task capability contract after all builtin and MCP tools are known.
/// Missing names fail the spawn instead of silently widening/narrowing capabilities;
/// an empty allowlist intentionally leaves the child with no executable tools.
fn enforce_tool_allowlist(
    registry: &mut ToolRegistry,
    requested: &[String],
) -> Result<Vec<String>, String> {
    let available = active_tool_names(registry);
    let mut missing = requested
        .iter()
        .filter(|name| !available.iter().any(|candidate| candidate == *name))
        .cloned()
        .collect::<Vec<_>>();
    missing.sort();
    missing.dedup();
    if !missing.is_empty() {
        return Err(format!(
            "subagent tools_allowlist contains unavailable tool(s): {}",
            missing.join(", ")
        ));
    }
    registry.retain_allowed(requested);
    Ok(active_tool_names(registry))
}

async fn bounded_ledger_slice(
    store: &Arc<dyn SessionStore>,
    parent_session_id: &str,
    task: &SubAgentTask,
) -> Result<Option<String>, String> {
    let Some(assignment) = &task.ledger_assignment else {
        return Ok(None);
    };
    let inherited_case = store
        .case_id_for_session(parent_session_id)
        .await
        .map_err(|error| format!("cannot resolve delegated Experiment case: {error}"))?;
    if inherited_case != assignment.case_id {
        return Err("delegated Experiment crosses the parent session case boundary".into());
    }
    let snapshot = store
        .load(&assignment.case_id)
        .await
        .map_err(|error| format!("cannot load delegated Experiment Ledger slice: {error}"))?;
    let experiment = snapshot
        .experiments
        .get(&assignment.experiment_id)
        .ok_or_else(|| {
            format!(
                "delegated Experiment {} is missing",
                assignment.experiment_id
            )
        })?;
    if experiment.status != holmes_core::ledger::ExperimentStatus::Running {
        return Err(format!(
            "delegated Experiment {} is {:?}, expected running",
            experiment.id, experiment.status
        ));
    }
    let mut expected_tools = experiment.tool_allowlist.clone();
    expected_tools.sort();
    expected_tools.dedup();
    let mut assigned_tools = task.constraints.tools_allowlist.clone();
    assigned_tools.sort();
    assigned_tools.dedup();
    if assigned_tools != expected_tools {
        return Err("delegated subagent tools do not match the Experiment allowlist".into());
    }

    let hypotheses = experiment
        .hypothesis_ids
        .iter()
        .filter_map(|id| snapshot.hypotheses.get(id))
        .map(|hypothesis| {
            serde_json::json!({
                "id": hypothesis.id,
                "claim": hypothesis.claim,
                "priority": hypothesis.priority,
                "status": hypothesis.status,
                "revision": hypothesis.revision,
            })
        })
        .collect::<Vec<_>>();
    let predictions = experiment
        .prediction_ids
        .iter()
        .filter_map(|id| snapshot.predictions.get(id))
        .map(|prediction| {
            serde_json::json!({
                "id": prediction.id,
                "hypothesis_id": prediction.hypothesis_id,
                "observable": prediction.observable,
                "expected_when_true": prediction.expected_when_true,
                "falsifier": prediction.falsifier,
                "validator": prediction.validator,
                "required": prediction.required,
            })
        })
        .collect::<Vec<_>>();
    let evidence = snapshot
        .evidence
        .values()
        .filter(|evidence| {
            evidence.binding.experiment_id.as_ref() == Some(&experiment.id)
                || evidence
                    .binding
                    .prediction_ids
                    .iter()
                    .any(|id| experiment.prediction_ids.contains(id))
        })
        .rev()
        .take(12)
        .map(|evidence| {
            serde_json::json!({
                "id": evidence.id,
                "tool": evidence.tool,
                "outcome_status": evidence.outcome_status,
                "predicate": evidence.predicate,
                "output_snippet": evidence.output_snippet,
            })
        })
        .collect::<Vec<_>>();
    let resolutions = snapshot
        .resolutions
        .values()
        .filter(|resolution| {
            experiment
                .hypothesis_ids
                .contains(&resolution.hypothesis_id)
        })
        .map(|resolution| {
            serde_json::json!({
                "id": resolution.id,
                "hypothesis_id": resolution.hypothesis_id,
                "status": resolution.status,
                "summary": resolution.validator_summary,
            })
        })
        .collect::<Vec<_>>();

    Ok(Some(
        serde_json::json!({
            "trust": "runtime-generated bounded Ledger slice; tool/file/web text remains untrusted",
            "case_id": snapshot.case_id,
            "ledger_version": snapshot.version,
            "experiment": {
                "id": experiment.id,
                "action": experiment.action,
                "expected_observations": experiment.expected_observations,
                "tool_allowlist": experiment.tool_allowlist,
                "attempt": experiment.attempt,
            },
            "hypotheses": hypotheses,
            "predictions": predictions,
            "relevant_evidence": evidence,
            "resolutions": resolutions,
        })
        .to_string(),
    ))
}

#[derive(Clone)]
pub struct CliSubagentRunner {
    pub session_db: Arc<dyn SessionStore>,
    pub memory_store: Arc<MemoryStore>,
    pub llm: Arc<LlmClient>,
    pub config: Config,
    pub parent_session_id: String,
    /// Process-wide subagent concurrency pool (AGT-014): shared with every
    /// nested registry this runner builds, so the cap holds across depth levels.
    pub slots: Arc<Semaphore>,
}

pub struct CaptureSink {
    pub final_answer: Option<String>,
}

impl RuntimeSink for CaptureSink {
    fn emit(&mut self, event: StreamEvent) {
        if let RuntimeYield::FinalAnswer { content, .. } = event.data {
            self.final_answer = Some(content);
        }
    }
}

#[async_trait]
impl SubagentRunner for CliSubagentRunner {
    async fn run_subagent(
        &self,
        args: &str,
        exec: &holmes_core::execution_context::ExecutionContext,
    ) -> Result<AgentTaskResult, String> {
        // Attempt to parse the arguments into a SubAgentTask to ensure schema validity.
        let task: SubAgentTask =
            serde_json::from_str(args).map_err(|e| format!("Failed to parse task: {}", e))?;
        let ledger_slice =
            bounded_ledger_slice(&self.session_db, &self.parent_session_id, &task).await?;

        let sub_session_id = format!("sub-{}", Uuid::new_v4());
        let mode = SessionMode::default();
        let system_prompt = if let Some(ledger_slice) = &ledger_slice {
            format!(
                "You are Holmes subagent for parent session {}. Context: {}\n\
                 <assigned_ledger_slice>{}</assigned_ledger_slice>\n\
                 Execute only the assigned Experiment. Produce observations and durable Evidence; \
                 do not claim that the Hypothesis or parent task is complete.",
                self.parent_session_id, task.context_summary, ledger_slice
            )
        } else {
            format!(
                "You are Holmes subagent for parent session {}. Context: {}",
                self.parent_session_id, task.context_summary
            )
        };
        let resolved_model = resolve_attack_model_provider(&self.config, None);
        let model = resolved_model
            .as_ref()
            .map(|resolved| resolved.model.clone())
            .unwrap_or_else(|| "unknown".into());

        let mut registry = ToolRegistry::new();
        // Nested background spawns get their own registry per subagent run, wired into
        // the subagent's own runtime context below so its completions drain inside
        // the subagent's turn loop. Durable write-back (AGT-007) attributes nested
        // tasks to this subagent session.
        let background_tasks = holmes_core::background::BackgroundTasks::new();
        let durable_binding = self.session_db.durable_task_sink().map(|sink| {
            holmes_core::background::DurableTaskBinding {
                sink,
                parent_session_id: Some(sub_session_id.clone()),
            }
        });
        holmes_tools::builtin::register_all(
            &mut registry,
            &self.config,
            Some(Arc::new(self.clone())),
            None,
            Some(background_tasks.clone()),
            durable_binding,
            Some(self.slots.clone()),
        );
        holmes_tools::mcp::register_mcp_tools(
            &mut registry,
            &self.config.mcp.servers,
            std::time::Duration::from_millis(self.config.execution.mcp_request_timeout_ms),
        )
        .await;
        let tool_names = enforce_tool_allowlist(&mut registry, &task.constraints.tools_allowlist)?;

        // Atomic creation (P1-04): the session row and the complete startup
        // event batch (SessionCreated with the parent binding, SystemPromptSet,
        // ModeSet, ModelSet, ActiveToolsSet) commit in a single transaction —
        // a failure leaves no half-initialised subagent session behind.
        let startup_events = crate::session_assembly::startup_events(
            &sub_session_id,
            Some("Subagent".into()),
            &mode,
            resolved_model.as_ref(),
            &system_prompt,
            Some(self.parent_session_id.clone()),
            None,
            vec!["subagent".into()],
            tool_names.clone(),
            Utc::now(),
        );
        self.session_db
            .create_session_with_events(
                holmes_session::db::CreateSessionParams {
                    id: Some(sub_session_id.clone()),
                    title: Some("Subagent".into()),
                    mode: Some(mode.clone()),
                    model: Some(model.clone()),
                    system_prompt: Some(system_prompt.clone()),
                    parent_session_id: Some(self.parent_session_id.clone()),
                    fork_point: None,
                    source: Some("subagent".into()),
                    tags: vec!["subagent".into()],
                },
                startup_events,
            )
            .await
            .map_err(|e| e.to_string())?;

        // Persistent parent-side association (AGT-013): the parent's own event log
        // records which subagent session was spawned for what, so after a restart
        // the parent can re-associate tasks with subagent sessions even without the
        // task store. Best-effort: a logging failure must not kill the run.
        if let Err(error) = self
            .session_db
            .append_event(
                &self.parent_session_id,
                &Event::SubAgentSpawned {
                    sub_session_id: sub_session_id.clone(),
                    agent_type: AgentType::Operative,
                    task_description: task.task.clone(),
                    context_summary: task.context_summary.clone(),
                    isolation: task.constraints.isolation.clone(),
                    model: model.clone(),
                    tools: tool_names,
                    max_turns: task.constraints.max_turns,
                },
            )
            .await
        {
            tracing::warn!(sub_session_id = %sub_session_id, error = %error,
                "failed to record SubAgentSpawned on the parent session");
        }

        // Isolation (AGT-014): the run gets its own scratch directory for temporary
        // files, installed into every renewed execution context. Dropped (and
        // deleted) when the run ends.
        let temp_dir = tempfile::Builder::new()
            .prefix("holmes-subagent-")
            .tempdir()
            .map_err(|e| format!("failed to create subagent scratch dir: {e}"))?;

        let session = RuntimeSession::new(sub_session_id.clone(), mode.clone())
            .with_system_prompt(&system_prompt);
        let mind_palace = MindPalace::new(self.session_db.clone(), self.memory_store.clone());
        let runtime_guards = GuardChain::from_config(&self.config.guards);
        let mut runtime_state = RuntimeState::new(mode);
        runtime_state.assigned_experiment = task.ledger_assignment.clone();

        // The task's constraints are part of the spawn contract (AGT-014): max_turns
        // caps the subagent's iteration budget so a runaway delegation stops
        // deterministically with a partial result instead of burning the turn.
        let mut sub_config = self.config.clone();
        if task.constraints.max_turns > 0 {
            sub_config.agent.max_iterations = task.constraints.max_turns;
        }

        let runtime_context = RuntimeContext::new(
            session,
            self.session_db.clone(),
            self.memory_store.clone(),
            mind_palace,
            self.llm.clone() as Arc<dyn LlmBackend>,
            Arc::new(registry),
            runtime_guards,
            runtime_state,
            sub_config,
        );

        let mut runtime = AgentRuntime::new(runtime_context);
        runtime.context_mut().set_background_tasks(background_tasks);
        // Derive the subagent's turn boundary from the parent's (AGT-002): cancelling
        // or expiring the parent turn propagates into this runtime's tool calls.
        runtime.context_mut().set_parent_execution(exec);
        runtime
            .context_mut()
            .set_turn_temp_dir(temp_dir.path().to_path_buf());
        let mut sink = CaptureSink { final_answer: None };

        let started = std::time::Instant::now();
        let outcome = runtime.run_oneshot(task.task.clone(), &mut sink).await;
        let wall_clock_ms = started.elapsed().as_millis() as u64;

        // Task-level outcomes become a structured result; only run-level failures
        // below (event-log unreadable) stay errors. This way the parent always gets
        // status/remaining_work instead of a bare string (AGT-013).
        let (final_answer, stop) = match outcome {
            Ok(TurnOutcome::FinalAnswer { content, .. }) => (Some(content), SubagentStop::Finished),
            Ok(TurnOutcome::MaxIterationsReached { message, .. }) => {
                (sink.final_answer.take(), SubagentStop::Stopped(message))
            }
            Ok(TurnOutcome::NeedsUser { prompt, .. }) => (
                sink.final_answer.take(),
                SubagentStop::Stopped(format!(
                    "subagent cannot ask the operator; it stopped needing input: {prompt}"
                )),
            ),
            Ok(TurnOutcome::Interrupted { .. }) => {
                (sink.final_answer.take(), SubagentStop::Cancelled)
            }
            Err(error) => {
                let message = error.to_string();
                // Cooperative cancellation surfaces as a runtime error on some
                // paths; classify by the parent boundary so the status is accurate.
                if exec.is_cancelled() {
                    (sink.final_answer.take(), SubagentStop::Cancelled)
                } else {
                    (sink.final_answer.take(), SubagentStop::Failed(message))
                }
            }
        };

        // Build the result deterministically from the subagent session's persisted
        // event log — findings, evidence, validations and usage come from recorded
        // events, never from the model's self-report (AGT-013).
        let events = self
            .session_db
            .get_events(&sub_session_id)
            .await
            .map(|stored| stored.into_iter().map(|s| s.event).collect::<Vec<_>>())
            .unwrap_or_else(|error| {
                tracing::warn!(sub_session_id = %sub_session_id, error = %error,
                    "failed to read subagent event log; result carries no recorded evidence");
                Vec::new()
            });
        let mut result = build_agent_task_result(
            exec.task_id().to_string(),
            &events,
            final_answer,
            stop,
            wall_clock_ms,
            Some(sub_session_id.clone()),
        );
        if let Some(assignment) = &task.ledger_assignment {
            let snapshot = self
                .session_db
                .load(&assignment.case_id)
                .await
                .map_err(|error| format!("failed to verify delegated Evidence: {error}"))?;
            for evidence in snapshot.evidence.values().filter(|evidence| {
                evidence.source_session_id == sub_session_id
                    && evidence.binding.experiment_id.as_ref() == Some(&assignment.experiment_id)
            }) {
                if !result
                    .evidence
                    .iter()
                    .any(|reference| reference.reference == evidence.id)
                {
                    result.evidence.push(holmes_core::subagent::EvidenceRef {
                        kind: "ledger_evidence".into(),
                        reference: evidence.id.clone(),
                        note: Some(evidence.predicate.clone()),
                    });
                }
            }
        }

        if let Err(error) = self
            .session_db
            .append_event(
                &self.parent_session_id,
                &Event::SubAgentCompleted {
                    sub_session_id: sub_session_id.clone(),
                    tokens_used: result.usage.tokens_used,
                    events_count: events.len() as u64,
                    findings_count: result.findings.len(),
                    result: result.clone(),
                },
            )
            .await
        {
            tracing::warn!(sub_session_id = %sub_session_id, error = %error,
                "failed to record SubAgentCompleted on the parent session");
        }

        Ok(result)
    }
}

/// Re-execution backend for durable subagent tasks (P1-02), registered with
/// the resident `DurableTaskScheduler` under kind "subagent". Automatic
/// requeue never targets subagent tasks (they spawn as `safe_to_retry:
/// false`: a subagent drives arbitrary mutating tools picked
/// non-deterministically by the LLM, and fencing cannot make duplicated
/// EXTERNAL side effects idempotent). This executor exists for the operator
/// requeue path: a task suspended as `manual_recovery_required` that an
/// operator inspects and resets to `retrying` is leased by the scheduler and
/// re-run here from its persisted payload, under a fresh fenced attempt.
pub struct SubagentTaskExecutor {
    pub session_db: Arc<dyn SessionStore>,
    pub memory_store: Arc<MemoryStore>,
    pub llm: Arc<LlmClient>,
    pub config: Config,
    pub slots: Arc<Semaphore>,
}

#[async_trait]
impl holmes_runtime::scheduler::DurableTaskExecutor for SubagentTaskExecutor {
    async fn execute(
        &self,
        task: holmes_runtime::scheduler::LeasedTask,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<String, String> {
        let payload = task
            .payload
            .clone()
            .ok_or_else(|| "durable subagent task has no stored payload".to_string())?;
        let runner = CliSubagentRunner {
            session_db: self.session_db.clone(),
            memory_store: self.memory_store.clone(),
            llm: self.llm.clone(),
            config: self.config.clone(),
            parent_session_id: task.parent_session_id.clone().unwrap_or_default(),
            slots: self.slots.clone(),
        };
        // Fresh root boundary for the re-run; bridge the scheduler's
        // cancellation (lost lease / shutdown) into it so the run stops
        // instead of finishing detached.
        let exec = holmes_core::execution_context::ExecutionContext::new(task.task_id.clone());
        let bridge = exec.token();
        let watcher = tokio::spawn(async move {
            cancel.cancelled().await;
            bridge.cancel();
        });
        let result = runner.run_subagent(&payload, &exec).await;
        watcher.abort();
        let result = result?;
        // Same parent-facing verification as the spawn path (AGT-013).
        let verified = holmes_core::subagent::wrap_for_parent(result);
        serde_json::to_string_pretty(&verified).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holmes_core::{FunctionDefinition, ToolDefinition};
    use holmes_tools::registry::Tool;

    struct NamedTool(&'static str);

    #[async_trait]
    impl Tool for NamedTool {
        fn name(&self) -> &str {
            self.0
        }

        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                tool_type: "function".into(),
                function: FunctionDefinition {
                    name: self.0.into(),
                    description: "test tool".into(),
                    parameters: serde_json::json!({"type": "object"}),
                },
            }
        }

        fn is_read_only(&self) -> bool {
            true
        }

        async fn execute(&self, _args: &str) -> anyhow::Result<String> {
            Ok("ok".into())
        }
    }

    fn make_registry() -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(NamedTool("read_file")));
        registry.register(Box::new(NamedTool("http_request")));
        registry
    }

    #[test]
    fn subagent_allowlist_filters_registry_and_empty_means_no_tools() {
        let mut registry = make_registry();
        let active = enforce_tool_allowlist(&mut registry, &["http_request".into()]).unwrap();
        assert_eq!(active, vec!["http_request"]);
        assert!(!registry.contains("read_file"));

        let mut registry = make_registry();
        let active = enforce_tool_allowlist(&mut registry, &[]).unwrap();
        assert!(active.is_empty());
        assert!(registry.definitions().is_empty());
    }

    #[test]
    fn subagent_allowlist_rejects_unavailable_tools() {
        let mut registry = make_registry();
        let error = enforce_tool_allowlist(&mut registry, &["missing_tool".into()])
            .expect_err("unknown capability must fail closed");
        assert!(error.contains("missing_tool"));
        assert!(registry.contains("read_file"), "failure is non-mutating");
    }
}
