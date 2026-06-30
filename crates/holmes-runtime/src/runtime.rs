use chrono::Utc;
use holmes_core::event::Event;
use holmes_core::tool_types::LlmResponse;
use holmes_core::types::TokenDelta;
use holmes_core::InterventionLevel;

use crate::action::ActionEngine;
use crate::compaction::{CaseCompactor, CompressionResult};
use crate::context::RuntimeContext;
use crate::decision::HolmesDecision;
use crate::deduction::DeductionEngine;
use crate::deliberation::RuntimeError;
use crate::dialogue::DialogueEngine;
use crate::evidence::EvidenceEngine;
use crate::learning::{record_review_started, LearningEngine};
use crate::memory::MemoryEngine;
use crate::perception::PerceptionEngine;
use crate::reflection::{ReflectionEngine, ReflectionOutcome};
use crate::yield_stream::{RuntimeSink, RuntimeYield};
use crate::DeliberationEngine;

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
    FinalAnswer { content: String, iterations: usize },
    NeedsUser { prompt: String, iterations: usize },
    MaxIterationsReached { message: String, iterations: usize },
}

pub struct AgentRuntime {
    context: RuntimeContext,
    perception: PerceptionEngine,
    deliberation: DeliberationEngine,
    action: ActionEngine,
    deduction: DeductionEngine,
    evidence: EvidenceEngine,
    learning: LearningEngine,
    memory: MemoryEngine,
    reflection: ReflectionEngine,
    dialogue: DialogueEngine,
    compactor: CaseCompactor,
}

impl AgentRuntime {
    pub fn new(context: RuntimeContext) -> Self {
        let max_iterations = context.config.agent.max_iterations.max(1) as usize;
        let mut action = ActionEngine::new();
        action.hooks.push(std::sync::Arc::new(crate::hooks::checkpoint::CheckpointHook::new(&context.session_id)));
        
        Self {
            context,
            perception: PerceptionEngine,
            deliberation: DeliberationEngine::default(),
            action,
            deduction: DeductionEngine::new(),
            evidence: EvidenceEngine::new(),
            learning: LearningEngine::new(),
            memory: MemoryEngine::new(),
            reflection: ReflectionEngine::new(max_iterations),
            dialogue: DialogueEngine,
            compactor: CaseCompactor::new(),
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
        let input = input.into();
        let turn_start_index = self.next_event_index().await?;
        let mut turn_tokens = TokenDelta::default();
        self.record_user_message(&input.content).await?;
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
            if let ReflectionOutcome::MaxIterationsReached(message) =
                self.reflection.assess_iteration_budget(iterations)
            {
                if let Err(error) = self.review_learning_for_turn(turn_start_index).await {
                    return self.stop_for_error(error, iterations, sink);
                }
                self.record_turn_complete(turn_start_index, &turn_tokens)
                    .await?;
                sink.emit_yield(&self.context.session_id, RuntimeYield::Error {
                    message: message.clone(),
                });
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
                Err(error) => return self.stop_for_error(error, iterations, sink),
            };
            iterations += 1;

            let token_delta = match self.apply_usage(&deliberation.response).await {
                Ok(delta) => delta,
                Err(error) => return self.stop_for_error(error, iterations, sink),
            };
            accumulate_tokens(&mut turn_tokens, &token_delta);
            let response = self.response_for_record(&deliberation.response, &deliberation.parsed);
            self.record_assistant_response(&response).await?;

            match deliberation.parsed.decision {
                HolmesDecision::Answer { message } => {
                    let content = if message.trim().is_empty() {
                        response.content.clone().unwrap_or_default()
                    } else {
                        message
                    };
                    let event = self.dialogue.format_final_answer(&content);
                    sink.emit_yield(&self.context.session_id, event);
                    if let Err(error) = self.review_learning_for_turn(turn_start_index).await {
                        return self.stop_for_error(error, iterations, sink);
                    }
                    self.record_turn_complete(turn_start_index, &turn_tokens)
                        .await?;
                    return Ok(TurnOutcome::FinalAnswer {
                        content: content.trim().to_string(),
                        iterations,
                    });
                }
                HolmesDecision::Finish { summary } => {
                    let content = if summary.trim().is_empty() {
                        response.content.clone().unwrap_or_default()
                    } else {
                        summary
                    };
                    if let Err(error) = self.record_goal_evaluated(true, &content, iterations).await
                    {
                        return self.stop_for_error(error, iterations, sink);
                    }
                    let event = self.dialogue.format_final_answer(&content);
                    sink.emit_yield(&self.context.session_id, event);
                    if let Err(error) = self.review_learning_for_turn(turn_start_index).await {
                        return self.stop_for_error(error, iterations, sink);
                    }
                    self.record_turn_complete(turn_start_index, &turn_tokens)
                        .await?;
                    return Ok(TurnOutcome::FinalAnswer {
                        content: content.trim().to_string(),
                        iterations,
                    });
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
                    sink.emit_yield(&self.context.session_id, RuntimeYield::NeedsUserInput {
                        prompt: prompt.clone(),
                    });
                    if let Err(error) = self.review_learning_for_turn(turn_start_index).await {
                        return self.stop_for_error(error, iterations, sink);
                    }
                    self.record_turn_complete(turn_start_index, &turn_tokens)
                        .await?;
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

                    let action_batch = match self
                        .action
                        .execute_batch(&mut self.context, &calls, sink)
                        .await
                    {
                        Ok(batch) => batch,
                        Err(error) => return self.stop_for_error(error, iterations, sink),
                    };

                    self.context.session.messages.extend(action_batch.messages);

                    let projection = self.evidence.project(&mut self.context);
                    if let Err(error) = self
                        .deduction
                        .review_tool_results(
                            &mut self.context,
                            &action_batch.results,
                            &projection.updates,
                        )
                        .await
                    {
                        return self.stop_for_error(error, iterations, sink);
                    }
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
                HolmesDecision::SetGoal { condition, reason } => {
                    self.emit_intermediate_content(
                        deliberation.parsed.display_content.as_deref(),
                        sink,
                    );
                    if let Err(error) = self.set_runtime_goal(&condition, reason.as_deref()).await {
                        return self.stop_for_error(error, iterations, sink);
                    }
                    sink.emit_yield(&self.context.session_id, RuntimeYield::PlanUpdate {
                        content: format!("Goal set: {condition}"),
                    });
                }
                HolmesDecision::Reflect {
                    diagnosis,
                    next_strategy,
                } => {
                    self.emit_intermediate_content(
                        deliberation.parsed.display_content.as_deref(),
                        sink,
                    );
                    if let Err(error) = self.record_reflection(&diagnosis, &next_strategy).await {
                        return self.stop_for_error(error, iterations, sink);
                    }
                    sink.emit_yield(&self.context.session_id, RuntimeYield::PlanUpdate {
                        content: format!("Reflection: {diagnosis}\nNext strategy: {next_strategy}"),
                    });
                }
                HolmesDecision::Deduce { trace, message } => {
                    self.emit_intermediate_content(
                        deliberation
                            .parsed
                            .display_content
                            .as_deref()
                            .or(message.as_deref()),
                        sink,
                    );
                    let projection =
                        match self.deduction.apply_trace(&mut self.context, trace).await {
                            Ok(projection) => projection,
                            Err(error) => return self.stop_for_error(error, iterations, sink),
                        };
                    sink.emit_yield(&self.context.session_id, RuntimeYield::PlanUpdate {
                        content: format!(
                            "Deduction ledger updated with {} event(s).",
                            projection.recorded
                        ),
                    });
                }
            }
        }
    }

    async fn decide_with_overflow_retry(
        &mut self,
        frame: &crate::perception::PerceptionFrame,
        sink: &mut dyn RuntimeSink,
    ) -> Result<crate::deliberation::DeliberationResult, RuntimeError> {
        match self.deliberation.decide(&self.context, frame).await {
            Ok(result) => Ok(result),
            Err(error)
                if error.kind == crate::deliberation::RuntimeErrorKind::ContextOverflow =>
            {
                let original_message = error.message.clone();
                let compacted = self
                    .compact_with_trigger(true, holmes_core::CompactionTrigger::Overflow)
                    .await?;
                let Some(result) = compacted else {
                    return Err(RuntimeError::recoverable(format!(
                        "context overflow and compaction produced no smaller context: {original_message}"
                    )));
                };
                sink.emit_yield(&self.context.session_id, compaction_event(&result));
                let retry_frame = self.perception.perceive(&self.context);
                self.deliberation
                    .decide(&self.context, &retry_frame)
                    .await
                    .map_err(|retry_error| {
                        RuntimeError::recoverable(format!(
                            "context overflow retry failed after compaction: {}; original overflow: {}",
                            retry_error.message, original_message
                        ))
                    })
            }
            Err(error) => Err(error),
        }
    }

    fn emit_intermediate_content(&self, content: Option<&str>, sink: &mut dyn RuntimeSink) {        if let Some(content) = content {
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
                return Err(RuntimeError::recoverable(format!("Hook blocked compaction: {}", e)));
            }
        }

        let plan = self
            .compactor
            .plan(&self.context.session, &self.context.config, force);
        let plan_protected_head = plan.protected_head;
        let protected_tail_tokens =
            self.context.config.compressor.protected_tail_tokens as usize;

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

        let Some(mut result) = self
            .compactor
            .compress_session(
                &mut self.context.session,
                &self.context.config,
                plan,
                trigger.clone(),
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

        let turn_events = self
            .context
            .session_db
            .get_events(&self.context.session_id)
            .await
            .map_err(|error| {
                RuntimeError::recoverable(format!(
                    "failed to load turn events for learning review in session {}: {}",
                    self.context.session_id, error
                ))
            })?
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

    async fn record_reflection(
        &mut self,
        diagnosis: &str,
        next_strategy: &str,
    ) -> Result<(), RuntimeError> {
        let event = Event::ReflectionRecorded {
            diagnosis: diagnosis.trim().to_string(),
            failure_type: "runtime_reflection".into(),
            lessons_learned: next_strategy.trim().to_string(),
            suggestions: vec![next_strategy.trim().to_string()],
            triggered_by: "holmes_decision".into(),
        };
        append_and_ingest(&mut self.context, event).await
    }

    async fn record_goal_evaluated(
        &mut self,
        satisfied: bool,
        reason: &str,
        iterations: usize,
    ) -> Result<(), RuntimeError> {
        if self.context.state.active_goal.is_none() {
            return Ok(());
        }

        if satisfied {
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

        let event = Event::GoalEvaluated {
            satisfied,
            reason: reason.trim().to_string(),
            turn_count: iterations as u64,
            tokens_spent: self.context.session.tokens.input + self.context.session.tokens.output,
        };
        append_and_ingest(&mut self.context, event).await
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

    fn stop_for_error(
        &mut self,
        error: RuntimeError,
        iterations: usize,
        sink: &mut dyn RuntimeSink,
    ) -> Result<TurnOutcome, RuntimeError> {
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
                sink.emit_yield(&self.context.session_id, RuntimeYield::Error {
                    message: error.message.clone(),
                });
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
        HolmesDecision::Reflect {
            diagnosis,
            next_strategy,
        } => nonempty(format!(
            "Reflection: {}\nNext strategy: {}",
            diagnosis.trim(),
            next_strategy.trim()
        )),
        HolmesDecision::Deduce { message, .. } => message
            .as_deref()
            .and_then(nonempty)
            .or_else(|| nonempty("Updated deduction ledger.")),
        HolmesDecision::Finish { summary } => nonempty(summary),
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

async fn append_and_ingest(context: &mut RuntimeContext, event: Event) -> Result<(), RuntimeError> {
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
    use holmes_session::{memory_store::MemoryStore, CreateSessionParams, SessionDB};
    use holmes_tools::{Tool, ToolRegistry};

    use crate::deliberation::LlmBackend;
    use crate::yield_stream::VecSink;
    use crate::{RuntimeContext, RuntimeState};

    use super::*;

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
                content: "hello Watson".into()
            , usage: None }]
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
                    call_id: Some("call-1".into())
                },
                RuntimeYield::ToolFinished {
                    name: "mock_tool".into(),
                    call_id: Some("call-1".into()),
                    success: true,
                    content: "mock output".into(),
                    error: None, usage: None },
                RuntimeYield::EvidenceUpdate {
                    content: "Discovered service on port 443: https nginx.".into()
                },
                RuntimeYield::FinalAnswer {
                    content: "done".into()
                , usage: None },
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
                    call_id: Some("call-1".into())
                },
                RuntimeYield::ToolFinished {
                    name: "mock_tool".into(),
                    call_id: Some("call-1".into()),
                    success: true,
                    content: "mock output".into(),
                    error: None, usage: None },
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
        let context = make_context(llm, tools, guards).await;
        let mut runtime = AgentRuntime::new(context);
        let mut sink = VecSink::new();

        let outcome = runtime
            .run_turn("try it", &mut sink)
            .await
            .expect("turn outcome");

        assert_eq!(
            outcome,
            TurnOutcome::FinalAnswer {
                content: "adjusted".into(),
                iterations: 2,
            }
        );
        assert!(sink
            .events
            .iter()
            .any(|event| { matches!(event.data, RuntimeYield::ToolFinished {  success: false, .. }) }));
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
            sink.yields().first().map(|y| y.clone()),
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
    async fn run_turn_can_set_runtime_goal_and_continue() {
        let llm = Arc::new(QueueLlmBackend::new(vec![
            Ok(final_response(
                r#"<holmes_decision>{"type":"set_goal","condition":"validate the login behavior","reason":"Watson asked for a standing objective."}</holmes_decision>"#,
            )),
            Ok(final_response("Goal is active.")),
        ]));
        let context = make_context(llm, ToolRegistry::new(), GuardChain::new()).await;
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
                iterations: 2,
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
    async fn run_turn_records_reflection_decision() {
        let llm = Arc::new(QueueLlmBackend::new(vec![
            Ok(final_response(
                r#"<holmes_decision>{"type":"reflect","diagnosis":"The probe repeated the same result.","next_strategy":"Pivot to response diffing."}</holmes_decision>"#,
            )),
            Ok(final_response("Pivot prepared.")),
        ]));
        let context = make_context(llm, ToolRegistry::new(), GuardChain::new()).await;
        let mut runtime = AgentRuntime::new(context);
        let mut sink = VecSink::new();

        let outcome = runtime
            .run_turn("continue", &mut sink)
            .await
            .expect("turn outcome");

        assert_eq!(
            outcome,
            TurnOutcome::FinalAnswer {
                content: "Pivot prepared.".into(),
                iterations: 2,
            }
        );
        assert!(sink.yields().iter().any(|event| matches!(
            event,
            RuntimeYield::PlanUpdate { content } if content.contains("Pivot to response diffing")
        )));
        let events = runtime
            .context()
            .session_db
            .get_events(&runtime.context().session_id)
            .await
            .expect("events");
        assert!(events
            .iter()
            .any(|event| matches!(event.event, Event::ReflectionRecorded { .. })));
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
            sink.yields().last().map(|y| y.clone()),
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
        assert_eq!(
            compression.0,
            Some(holmes_core::CompactionTrigger::Manual)
        );
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
            sink.yields().first().map(|y| y.clone()),
            Some(RuntimeYield::CompactionBoundary {
                before_count: 4,
                after_count: 3,
                method,
                ..
            }) if method == "static_fallback"
        ));
        assert!(matches!(
            sink.yields().last().map(|y| y.clone()),
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
    }

    impl QueueLlmBackend {
        fn new(responses: Vec<std::result::Result<LlmResponse, String>>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
            }
        }
    }

    #[async_trait]
    impl LlmBackend for QueueLlmBackend {
        async fn chat_completion(
            &self,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _role: &str,
        ) -> Result<LlmResponse> {
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
        config: HolmesConfig,
    ) -> RuntimeContext {
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
        }
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
}
