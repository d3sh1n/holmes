use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use holmes_core::config::HolmesConfig;
use holmes_core::event::{Event, StoredEvent};
use holmes_core::session::RuntimeSession;
use holmes_guards::GuardChain;
use holmes_mind_palace::MindPalace;
use holmes_runtime::context::{RuntimeContext, RuntimeState};
use holmes_runtime::runtime::{AgentRuntime, TurnOutcome};
use holmes_runtime::{RuntimeSink, RuntimeYield, StreamEvent};
use holmes_session::{memory_store::MemoryStore, CreateSessionParams, SessionDB};
use holmes_tools::ToolRegistry;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::llm::ScriptedLlmBackend;
use crate::scenario::{
    HarnessEventPayloadExpectation, HarnessExpectations, HarnessScenario, HarnessTool,
};
use crate::tool::HarnessMockTool;

#[derive(Debug, Clone, Default)]
pub struct HarnessRunner;

impl HarnessRunner {
    pub fn new() -> Self {
        Self
    }

    pub async fn run(&self, scenario: HarnessScenario) -> Result<HarnessReport> {
        let session_mode = scenario.mode();
        let session_id = Uuid::new_v4().to_string();
        let session_db = Arc::new(SessionDB::open(":memory:").await?);
        let memory_store = Arc::new(MemoryStore::open(":memory:").await?);
        let system_prompt = format!(
            "You are Holmes running inside a deterministic harness scenario named {}.",
            scenario.name
        );

        session_db
            .create_session(CreateSessionParams {
                id: Some(session_id.clone()),
                title: Some(format!("Harness: {}", scenario.name)),
                mode: Some(session_mode.clone()),
                model: Some("scripted".into()),
                system_prompt: Some(system_prompt.clone()),
                parent_session_id: None,
                fork_point: None,
                source: Some("harness".into()),
                tags: vec!["harness".into()],
            })
            .await?;

        let session = RuntimeSession::new(session_id.clone(), session_mode.clone())
            .with_system_prompt(&system_prompt);
        let llm = Arc::new(ScriptedLlmBackend::new(
            scenario
                .scripted_responses
                .clone()
                .into_iter()
                .map(|response| response.into_llm_response()),
        ));
        let tools = Arc::new(build_tool_registry(&scenario)?);
        let mind_palace = MindPalace::new(session_db.clone(), memory_store.clone());
        let context = RuntimeContext::new(
            session,
            session_db.clone(),
            memory_store,
            mind_palace,
            llm,
            tools,
            GuardChain::new(),
            RuntimeState::new(session_mode),
            harness_config(&scenario),
        );
        let mut runtime = AgentRuntime::new(context);
        let mut sink = RecordingSink::default();
        let mut turn_reports = Vec::new();
        let mut runtime_errors = Vec::new();
        let mut turn_expectation_failures = Vec::new();

        for turn in &scenario.turns {
            let turn_index = turn_reports.len();
            let outcome = run_harness_turn(
                &mut runtime,
                &mut sink,
                &mut turn_reports,
                &mut runtime_errors,
                turn.input.clone(),
            )
            .await;

            if turn.expect_needs_user && !matches!(outcome, Some(TurnOutcome::NeedsUser { .. })) {
                turn_expectation_failures.push(format!(
                    "turn {} expected needs_user outcome before reply",
                    turn_index
                ));
            }

            if let Some(reply) = turn.reply.as_ref() {
                if matches!(outcome, Some(TurnOutcome::NeedsUser { .. })) {
                    run_harness_turn(
                        &mut runtime,
                        &mut sink,
                        &mut turn_reports,
                        &mut runtime_errors,
                        reply.clone(),
                    )
                    .await;
                } else {
                    turn_expectation_failures.push(format!(
                        "turn {} provided reply but Holmes did not request Watson input",
                        turn_index
                    ));
                }
            }
        }

        let context = runtime.into_context();
        let stored_events = context.session_db.get_events(&context.session_id).await?;
        let yields = sink.into_yields();
        let metrics = HarnessMetrics::from_run(&turn_reports, &yields, &stored_events);
        let mut failed_expectations = evaluate_expectations(
            &scenario.expectations,
            &turn_reports,
            &yields,
            &stored_events,
            &runtime_errors,
            &metrics,
        );
        failed_expectations.extend(turn_expectation_failures);
        let success = failed_expectations.is_empty();
        let events = stored_events
            .into_iter()
            .map(HarnessEventReport::from)
            .collect();

        Ok(HarnessReport {
            name: scenario.name,
            success,
            session_id: context.session_id,
            turns: turn_reports,
            metrics,
            failed_expectations,
            runtime_errors,
            yields,
            events,
        })
    }
}

async fn run_harness_turn(
    runtime: &mut AgentRuntime,
    sink: &mut RecordingSink,
    turn_reports: &mut Vec<HarnessTurnReport>,
    runtime_errors: &mut Vec<String>,
    input: String,
) -> Option<TurnOutcome> {
    let index = turn_reports.len();
    match runtime.run_turn(input.clone(), sink).await {
        Ok(outcome) => {
            turn_reports.push(HarnessTurnReport {
                index,
                input,
                outcome: Some(TurnOutcomeReport::from(outcome.clone())),
                error: None,
            });
            Some(outcome)
        }
        Err(error) => {
            let message = error.to_string();
            runtime_errors.push(message.clone());
            turn_reports.push(HarnessTurnReport {
                index,
                input,
                outcome: None,
                error: Some(message),
            });
            None
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessReport {
    pub name: String,
    pub success: bool,
    pub session_id: String,
    pub turns: Vec<HarnessTurnReport>,
    pub metrics: HarnessMetrics,
    pub failed_expectations: Vec<String>,
    pub runtime_errors: Vec<String>,
    pub yields: Vec<RuntimeYield>,
    pub events: Vec<HarnessEventReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessTurnReport {
    pub index: usize,
    pub input: String,
    pub outcome: Option<TurnOutcomeReport>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessEventReport {
    pub id: u64,
    pub session_id: String,
    pub event_index: u64,
    pub turn_index: Option<u64>,
    pub stored_at: chrono::DateTime<chrono::Utc>,
    pub event_type: String,
    pub event: Event,
}

impl From<StoredEvent> for HarnessEventReport {
    fn from(stored: StoredEvent) -> Self {
        Self {
            id: stored.id,
            session_id: stored.session_id,
            event_index: stored.event_index,
            turn_index: stored.turn_index,
            stored_at: stored.timestamp,
            event_type: event_type_from_event(&stored.event),
            event: stored.event,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TurnOutcomeReport {
    FinalAnswer { content: String, iterations: usize },
    NeedsUser { prompt: String, iterations: usize },
    MaxIterationsReached { message: String, iterations: usize },
}

impl From<TurnOutcome> for TurnOutcomeReport {
    fn from(outcome: TurnOutcome) -> Self {
        match outcome {
            TurnOutcome::FinalAnswer {
                content,
                iterations,
            } => Self::FinalAnswer {
                content,
                iterations,
            },
            TurnOutcome::NeedsUser { prompt, iterations } => Self::NeedsUser { prompt, iterations },
            TurnOutcome::MaxIterationsReached {
                message,
                iterations,
            } => Self::MaxIterationsReached {
                message,
                iterations,
            },
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HarnessMetrics {
    pub turns: usize,
    pub final_answers: usize,
    pub needs_user: usize,
    pub max_iterations_reached: usize,
    pub tool_calls: usize,
    pub tool_failures: usize,
    pub runtime_errors: usize,
    pub yield_errors: usize,
    pub event_count: usize,
}

impl HarnessMetrics {
    fn from_run(
        turns: &[HarnessTurnReport],
        yields: &[RuntimeYield],
        events: &[StoredEvent],
    ) -> Self {
        Self {
            turns: turns.len(),
            final_answers: turns
                .iter()
                .filter(|turn| matches!(turn.outcome, Some(TurnOutcomeReport::FinalAnswer { .. })))
                .count(),
            needs_user: turns
                .iter()
                .filter(|turn| matches!(turn.outcome, Some(TurnOutcomeReport::NeedsUser { .. })))
                .count(),
            max_iterations_reached: turns
                .iter()
                .filter(|turn| {
                    matches!(
                        turn.outcome,
                        Some(TurnOutcomeReport::MaxIterationsReached { .. })
                    )
                })
                .count(),
            tool_calls: events
                .iter()
                .filter(|event| event_type(event) == "tool_call")
                .count(),
            tool_failures: yields
                .iter()
                .filter(|event| matches!(event, RuntimeYield::ToolFinished { success: false, .. }))
                .count(),
            runtime_errors: turns.iter().filter(|turn| turn.error.is_some()).count(),
            yield_errors: yields
                .iter()
                .filter(|event| matches!(event, RuntimeYield::Error { .. }))
                .count(),
            event_count: events.len(),
        }
    }
}

#[derive(Debug, Default)]
struct RecordingSink {
    yields: Vec<RuntimeYield>,
}

impl RecordingSink {
    fn into_yields(self) -> Vec<RuntimeYield> {
        self.yields
    }
}

impl RuntimeSink for RecordingSink {
    fn emit(&mut self, event: StreamEvent) {
        self.yields.push(event.data);
    }
}

fn build_tool_registry(scenario: &HarnessScenario) -> Result<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    for tool in scenario.tools.clone() {
        registry.register(Box::new(HarnessMockTool::from_config(
            tool_with_artifact_output(tool, scenario)?,
        )));
    }
    Ok(registry)
}

fn tool_with_artifact_output(
    mut tool: HarnessTool,
    scenario: &HarnessScenario,
) -> Result<HarnessTool> {
    let Some(artifact) = scenario
        .artifacts
        .iter()
        .find(|artifact| artifact.as_tool_output == tool.name)
    else {
        return Ok(tool);
    };

    let path = resolve_artifact_path(&artifact.path, scenario);
    tool.output = fs::read_to_string(&path).map_err(|error| {
        anyhow::anyhow!(
            "failed to read harness artifact {} for tool {}: {}",
            path.display(),
            tool.name,
            error
        )
    })?;
    Ok(tool)
}

fn resolve_artifact_path(path: &PathBuf, scenario: &HarnessScenario) -> PathBuf {
    if path.is_absolute() {
        return path.clone();
    }

    scenario
        .base_dir
        .as_ref()
        .map(|base_dir| base_dir.join(path))
        .unwrap_or_else(|| path.clone())
}

fn harness_config(scenario: &HarnessScenario) -> HolmesConfig {
    let mut config = HolmesConfig::default();
    config.agent.max_iterations = 12;
    config.llm.providers.clear();

    if let Some(compressor) = scenario
        .config
        .as_ref()
        .and_then(|config| config.compressor.as_ref())
    {
        if let Some(enabled) = compressor.enabled {
            config.compressor.enabled = enabled;
        }
        if let Some(context_limit) = compressor.context_limit {
            config.compressor.context_limit = context_limit;
        }
        if let Some(threshold) = compressor.threshold {
            config.compressor.threshold = threshold;
        }
        if let Some(protected_head) = compressor.protected_head {
            config.compressor.protected_head = protected_head;
        }
        if let Some(protected_tail_tokens) = compressor.protected_tail_tokens {
            config.compressor.protected_tail_tokens = protected_tail_tokens;
        }
        if let Some(protect_last_n) = compressor.protect_last_n {
            config.compressor.protect_last_n = protect_last_n;
        }
        if let Some(target_ratio) = compressor.target_ratio {
            config.compressor.target_ratio = target_ratio;
        }
        if let Some(max_summary_tokens) = compressor.max_summary_tokens {
            config.compressor.max_summary_tokens = max_summary_tokens;
        }
        if let Some(preserve_tool_groups) = compressor.preserve_tool_groups {
            config.compressor.preserve_tool_groups = preserve_tool_groups;
        }
    }

    if let Some(learning) = scenario
        .config
        .as_ref()
        .and_then(|config| config.learning.as_ref())
    {
        if let Some(enabled) = learning.enabled {
            config.learning.enabled = enabled;
        }
        if let Some(background) = learning.background {
            config.learning.background = background;
        }
        if let Some(review_interval_turns) = learning.review_interval_turns {
            config.learning.review_interval_turns = review_interval_turns;
        }
        if let Some(max_candidates_per_turn) = learning.max_candidates_per_turn {
            config.learning.max_candidates_per_turn = max_candidates_per_turn;
        }
        if let Some(memory_write_approval) = learning.memory_write_approval {
            config.learning.memory_write_approval = memory_write_approval;
        }
        if let Some(skill_write_approval) = learning.skill_write_approval {
            config.learning.skill_write_approval = skill_write_approval;
        }
        if let Some(rule_write_approval) = learning.rule_write_approval {
            config.learning.rule_write_approval = rule_write_approval;
        }
    }

    config
}

fn evaluate_expectations(
    expectations: &HarnessExpectations,
    turns: &[HarnessTurnReport],
    yields: &[RuntimeYield],
    events: &[StoredEvent],
    runtime_errors: &[String],
    metrics: &HarnessMetrics,
) -> Vec<String> {
    let mut failures = Vec::new();

    for needle in &expectations.final_contains {
        if !final_content_contains(turns, yields, needle) {
            failures.push(format!("missing final answer containing {:?}", needle));
        }
    }

    let actual_event_types: Vec<String> = events.iter().map(event_type).collect();
    for expected in &expectations.event_types {
        if !actual_event_types.iter().any(|actual| actual == expected) {
            failures.push(format!("missing event type {:?}", expected));
        }
    }

    if !expectations.event_sequence.is_empty()
        && !contains_ordered_subsequence(&actual_event_types, &expectations.event_sequence)
    {
        failures.push(format!(
            "missing event sequence {:?}; actual sequence {:?}",
            expectations.event_sequence, actual_event_types
        ));
    }

    for expected in &expectations.event_payloads {
        if !event_payload_matches(events, expected) {
            failures.push(format!(
                "missing event payload for {:?} containing {:?}",
                expected.event_type, expected.contains
            ));
        }
    }

    let actual_yield_types: Vec<String> = yields.iter().map(yield_type).collect();
    for expected in &expectations.yield_types {
        if !actual_yield_types.iter().any(|actual| actual == expected) {
            failures.push(format!("missing yield type {:?}", expected));
        }
    }

    for expected in &expectations.tool_calls {
        if !events.iter().any(|event| match &event.event {
            holmes_core::event::Event::ToolCall { name, .. } => name == expected,
            _ => false,
        }) {
            failures.push(format!("missing tool call {:?}", expected));
        }
    }

    let observed_errors = runtime_errors.len() + metrics.yield_errors + metrics.tool_failures;
    match expectations.max_errors {
        Some(max_errors) if observed_errors > max_errors => failures.push(format!(
            "observed {} errors, expected at most {}",
            observed_errors, max_errors
        )),
        None if observed_errors > 0 => failures.push(format!(
            "observed {} errors, expected none",
            observed_errors
        )),
        _ => {}
    }

    if let Some(expected) = expectations.needs_user_count {
        if metrics.needs_user != expected {
            failures.push(format!(
                "observed {} needs_user outcomes, expected {}",
                metrics.needs_user, expected
            ));
        }
    }

    failures
}

fn event_payload_matches(
    events: &[StoredEvent],
    expected: &HarnessEventPayloadExpectation,
) -> bool {
    events
        .iter()
        .filter(|event| event_type(event) == expected.event_type)
        .any(|event| {
            serde_json::to_string(&event.event)
                .map(|payload| {
                    expected
                        .contains
                        .iter()
                        .all(|needle| payload.contains(needle))
                })
                .unwrap_or(false)
        })
}

fn contains_ordered_subsequence(actual: &[String], expected: &[String]) -> bool {
    let mut actual = actual.iter();
    expected
        .iter()
        .all(|expected| actual.any(|actual| actual == expected))
}

fn final_content_contains(
    turns: &[HarnessTurnReport],
    yields: &[RuntimeYield],
    needle: &str,
) -> bool {
    turns.iter().any(|turn| match &turn.outcome {
        Some(TurnOutcomeReport::FinalAnswer { content, .. }) => content.contains(needle),
        _ => false,
    }) || yields.iter().any(|event| match event {
        RuntimeYield::FinalAnswer { content, .. } => content.contains(needle),
        _ => false,
    })
}

fn event_type(event: &StoredEvent) -> String {
    event_type_from_event(&event.event)
}

fn yield_type(event: &RuntimeYield) -> String {
    serde_json::to_value(event)
        .ok()
        .and_then(|value| {
            value
                .get("type")
                .and_then(|kind| kind.as_str())
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| "unknown".into())
}

fn event_type_from_event(event: &Event) -> String {
    serde_json::to_value(event)
        .ok()
        .and_then(|value| {
            value
                .get("type")
                .and_then(|kind| kind.as_str())
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| "unknown".into())
}
