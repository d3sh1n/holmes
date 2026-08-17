use holmes_core::event::Event;
use holmes_harness::{HarnessRunner, HarnessScenario};
use std::path::PathBuf;

#[tokio::test]
async fn runs_basic_answer_scenario() {
    let scenario_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scenarios/basic-answer.yaml");
    let scenario = HarnessScenario::from_path(scenario_path).expect("load scenario");
    let report = HarnessRunner::new()
        .run(scenario)
        .await
        .expect("run scenario");

    assert!(report.success, "{:#?}", report.failed_expectations);
    assert_eq!(report.metrics.turns, 1);
    assert_eq!(report.metrics.final_answers, 1);
}

#[tokio::test]
async fn runs_basic_tool_scenario() {
    let scenario_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scenarios/basic-tool.yaml");
    let scenario = HarnessScenario::from_path(scenario_path).expect("load scenario");
    let report = HarnessRunner::new()
        .run(scenario)
        .await
        .expect("run scenario");

    assert!(report.success, "{:#?}", report.failed_expectations);
    assert_eq!(report.metrics.tool_calls, 1);
    assert_eq!(report.metrics.final_answers, 1);
}

#[tokio::test]
async fn applies_compressor_override_without_breaking_run() {
    let scenario: HarnessScenario = serde_yaml::from_str(
        r#"
name: override-smoke
config:
  compressor:
    enabled: true
    context_limit: 120
    threshold: 0.5
    protect_last_n: 2
    target_ratio: 0.4
    max_summary_tokens: 200
turns:
  - input: hello
scripted_responses:
  - content: '<holmes_decision>{"type":"answer","message":"ok"}</holmes_decision>'
expectations:
  final_contains: [ok]
  max_errors: 0
"#,
    )
    .expect("parse scenario");

    let report = HarnessRunner::new()
        .run(scenario)
        .await
        .expect("run scenario");

    assert!(report.success, "{:#?}", report.failed_expectations);
}

#[tokio::test]
async fn startup_metadata_is_deterministic() {
    let scenario: HarnessScenario = serde_yaml::from_str(
        r#"
name: Deterministic Name!
turns:
  - input: hello
scripted_responses:
  - content: '<holmes_decision>{"type":"answer","message":"ok"}</holmes_decision>'
expectations:
  final_contains: [ok]
  max_errors: 0
"#,
    )
    .expect("parse scenario");

    let report = HarnessRunner::new()
        .run(scenario)
        .await
        .expect("run scenario");

    assert!(report.success, "{:#?}", report.failed_expectations);
    assert_eq!(report.session_id, "harness-deterministic-name");

    let expected_timestamp = chrono::DateTime::parse_from_rfc3339("1970-01-01T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);

    let created = report
        .events
        .iter()
        .find_map(|event| match &event.event {
            Event::SessionCreated { id, created_at, .. } => Some((id, created_at)),
            _ => None,
        })
        .expect("session_created event");
    assert_eq!(created.0, "harness-deterministic-name");
    assert_eq!(*created.1, expected_timestamp);

    let prompt_metadata = report
        .events
        .iter()
        .find_map(|event| match &event.event {
            Event::SessionSystemPromptSet {
                prompt_hash,
                timestamp,
                ..
            } => Some((prompt_hash, timestamp)),
            _ => None,
        })
        .expect("session_system_prompt_set event");
    assert_eq!(
        prompt_metadata.0,
        "27f09f96e8ec41f95a274560736cdda417e696ec8651179cb4bc5d2fa7edb79d"
    );
    assert_eq!(*prompt_metadata.1, expected_timestamp);
}

#[tokio::test]
async fn runs_long_compression_scenario() {
    let scenario_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scenarios/long-compression.yaml");
    let scenario = HarnessScenario::from_path(scenario_path).expect("load scenario");
    let report = HarnessRunner::new()
        .run(scenario)
        .await
        .expect("run scenario");

    assert!(report.success, "{:#?}", report.failed_expectations);
    let compression_event = report
        .events
        .iter()
        .find(|event| matches!(&event.event, Event::CompressionApplied { .. }))
        .expect("compression_applied event");
    let compression_json = serde_json::to_value(&compression_event.event)
        .expect("serialize compression_applied event");

    assert_eq!(compression_event.event_type, "compression_applied");
    assert_eq!(
        compression_json["type"], "compression_applied",
        "{compression_json:#?}"
    );
}

#[tokio::test]
async fn long_compression_session_replays_with_compaction_summary() {
    let scenario_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scenarios/long-compression.yaml");
    let scenario = HarnessScenario::from_path(scenario_path).expect("load scenario");
    let report = HarnessRunner::new()
        .run(scenario)
        .await
        .expect("run scenario");

    assert!(report.success, "{:#?}", report.failed_expectations);

    // Reconstruct StoredEvents from the report and replay the session, exactly
    // like a production resume would. The compaction must be applied as a
    // summary marker, not replayed as raw archived history.
    let stored: Vec<holmes_core::event::StoredEvent> = report
        .events
        .iter()
        .map(|reported| holmes_core::event::StoredEvent {
            id: reported.id,
            session_id: reported.session_id.clone(),
            event_index: reported.event_index,
            turn_index: reported.turn_index,
            timestamp: reported.stored_at,
            event: reported.event.clone(),
        })
        .collect();

    let replayed = holmes_session::replay::replay_events(&report.session_id, &stored);

    assert!(
        !replayed.compactions.is_empty(),
        "expected at least one compaction marker after replay",
    );
    assert!(
        replayed
            .session
            .messages
            .iter()
            .filter_map(|message| message.content.as_deref())
            .any(|content| content.contains("[Compaction summary]")),
        "replayed session should contain an injected compaction summary message",
    );
}

#[tokio::test]
async fn runs_learning_correction_scenario() {
    let scenario_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scenarios/learning-correction.yaml");
    let scenario = HarnessScenario::from_path(scenario_path).expect("load scenario");
    let report = HarnessRunner::new()
        .run(scenario)
        .await
        .expect("run scenario");

    assert!(report.success, "{:#?}", report.failed_expectations);
    assert!(report
        .events
        .iter()
        .any(|event| event.event_type == "memory_write_staged"));
    assert!(!report
        .events
        .iter()
        .any(|event| event.event_type == "memory_stored"));
}

#[tokio::test]
async fn runs_interactive_ask_watson_scenario() {
    let scenario_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../scenarios/interactive-ask-watson.yaml");
    let scenario = HarnessScenario::from_path(scenario_path).expect("load scenario");
    let report = HarnessRunner::new()
        .run(scenario)
        .await
        .expect("run scenario");

    assert!(report.success, "{:#?}", report.failed_expectations);
    assert_eq!(report.metrics.needs_user, 1);
    assert_eq!(report.turns.len(), 2);
    assert!(matches!(
        report.turns[0].outcome,
        Some(holmes_harness::TurnOutcomeReport::NeedsUser { .. })
    ));
    assert!(matches!(
        report.turns[1].outcome,
        Some(holmes_harness::TurnOutcomeReport::FinalAnswer { .. })
    ));
}

#[tokio::test]
async fn runs_native_control_ask_watson_scenario() {
    let scenario_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../scenarios/native-control-ask-watson.yaml");
    let scenario = HarnessScenario::from_path(scenario_path).expect("load scenario");
    let report = HarnessRunner::new()
        .run(scenario)
        .await
        .expect("run scenario");

    assert!(report.success, "{:#?}", report.failed_expectations);
    assert_eq!(report.metrics.needs_user, 1);
    assert_eq!(report.turns.len(), 2);
    // ask_watson arrived as a native tool_use call, yet still pauses for the operator.
    assert!(matches!(
        report.turns[0].outcome,
        Some(holmes_harness::TurnOutcomeReport::NeedsUser { .. })
    ));
    assert!(matches!(
        report.turns[1].outcome,
        Some(holmes_harness::TurnOutcomeReport::FinalAnswer { .. })
    ));
}

#[tokio::test]
async fn runs_native_control_interleaved_scenario() {
    let scenario_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../scenarios/native-control-interleaved.yaml");
    let scenario = HarnessScenario::from_path(scenario_path).expect("load scenario");
    let report = HarnessRunner::new()
        .run(scenario)
        .await
        .expect("run scenario");

    assert!(report.success, "{:#?}", report.failed_expectations);
    // One user turn; the goal (meta-action) and the tool call both landed within it,
    // before the single turn_complete — proving they interleaved in one iteration.
    assert_eq!(report.turns.len(), 1);
    let events_before_turn_complete: Vec<_> = report
        .events
        .iter()
        .take_while(|event| !matches!(event.event, Event::TurnComplete { .. }))
        .map(|event| &event.event)
        .collect();
    assert!(
        events_before_turn_complete
            .iter()
            .any(|event| matches!(event, Event::GoalSet { .. })),
        "goal_set must be recorded in the interleaved turn"
    );
    assert!(
        events_before_turn_complete
            .iter()
            .any(|event| matches!(event, Event::ToolCall { .. })),
        "tool_call must be recorded in the same interleaved turn"
    );
}

#[tokio::test]
async fn runs_artifact_tool_scenario() {
    let scenario_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scenarios/artifact-tool.yaml");
    let scenario = HarnessScenario::from_path(scenario_path).expect("load scenario");
    let report = HarnessRunner::new()
        .run(scenario)
        .await
        .expect("run scenario");

    assert!(report.success, "{:#?}", report.failed_expectations);
    let tool_result = report
        .events
        .iter()
        .find_map(|event| match &event.event {
            Event::ToolResult { content, .. } => Some(content),
            _ => None,
        })
        .expect("tool result");
    assert!(tool_result.contains("req-harness-001"));
    assert!(tool_result.contains("mfa_required"));
}

#[tokio::test]
async fn runs_premature_finish_scenario() {
    let scenario_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scenarios/premature-finish.yaml");
    let scenario = HarnessScenario::from_path(scenario_path).expect("load scenario");
    let report = HarnessRunner::new()
        .run(scenario)
        .await
        .expect("run scenario");

    assert!(report.success, "{:#?}", report.failed_expectations);
    // The rejected finish and the verified finish both leave a GoalEvaluated record.
    let evaluations: Vec<bool> = report
        .events
        .iter()
        .filter_map(|event| match &event.event {
            Event::GoalEvaluated { satisfied, .. } => Some(*satisfied),
            _ => None,
        })
        .collect();
    assert_eq!(
        evaluations,
        vec![false, true],
        "expected a rejected finish followed by a verified finish"
    );
}

#[tokio::test]
async fn runs_unverifiable_finish_scenario() {
    let scenario_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scenarios/unverifiable-finish.yaml");
    let scenario = HarnessScenario::from_path(scenario_path).expect("load scenario");
    let report = HarnessRunner::new()
        .run(scenario)
        .await
        .expect("run scenario");

    assert!(report.success, "{:#?}", report.failed_expectations);
    // No verified completion may have been recorded.
    assert!(
        report.events.iter().all(|event| !matches!(
            &event.event,
            Event::GoalEvaluated {
                satisfied: true,
                ..
            }
        )),
        "an unverifiable claim must never be recorded as a satisfied goal"
    );
}

#[tokio::test]
async fn runs_false_evidence_finish_scenario() {
    let scenario_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../scenarios/false-evidence-finish.yaml");
    let scenario = HarnessScenario::from_path(scenario_path).expect("load scenario");
    let report = HarnessRunner::new()
        .run(scenario)
        .await
        .expect("run scenario");

    assert!(report.success, "{:#?}", report.failed_expectations);
    let evaluations: Vec<bool> = report
        .events
        .iter()
        .filter_map(|event| match &event.event {
            Event::GoalEvaluated { satisfied, .. } => Some(*satisfied),
            _ => None,
        })
        .collect();
    assert_eq!(
        evaluations,
        vec![false, true],
        "expected the failed-probe finish to be rejected before the retried finish verifies"
    );
}

#[tokio::test]
async fn runs_answer_gate_plain_text_scenario() {
    let scenario_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../scenarios/answer-gate-plain-text.yaml");
    let scenario = HarnessScenario::from_path(scenario_path).expect("load scenario");
    let report = HarnessRunner::new()
        .run(scenario)
        .await
        .expect("run scenario");

    assert!(report.success, "{:#?}", report.failed_expectations);
    // The premature plain-text answer was rejected; the turn ran the probe and only
    // then completed — a rejected evaluation followed by a verified one.
    let evaluations: Vec<bool> = report
        .events
        .iter()
        .filter_map(|event| match &event.event {
            Event::GoalEvaluated { satisfied, .. } => Some(*satisfied),
            _ => None,
        })
        .collect();
    assert_eq!(
        evaluations,
        vec![false, true],
        "the unbacked answer must be rejected before the evidence-backed one verifies"
    );
    // Exactly one final answer: the premature text never reached the user.
    assert_eq!(report.metrics.final_answers, 1);
}

#[tokio::test]
async fn runs_answer_gate_irrelevant_evidence_scenario() {
    let scenario_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../scenarios/answer-gate-irrelevant-evidence.yaml");
    let scenario = HarnessScenario::from_path(scenario_path).expect("load scenario");
    let report = HarnessRunner::new()
        .run(scenario)
        .await
        .expect("run scenario");

    assert!(report.success, "{:#?}", report.failed_expectations);
    let evaluations: Vec<bool> = report
        .events
        .iter()
        .filter_map(|event| match &event.event {
            Event::GoalEvaluated { satisfied, .. } => Some(*satisfied),
            _ => None,
        })
        .collect();
    assert_eq!(
        evaluations,
        vec![false, true],
        "irrelevant evidence must not satisfy the task contract"
    );
}

#[tokio::test]
async fn runs_completion_gate_injection_scenario() {
    let scenario_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../scenarios/completion-gate-injection.yaml");
    let scenario = HarnessScenario::from_path(scenario_path).expect("load scenario");
    let report = HarnessRunner::new()
        .run(scenario)
        .await
        .expect("run scenario");

    assert!(report.success, "{:#?}", report.failed_expectations);
    // The injected "SATISFIED" inside the tool output must never flip the verdict:
    // no satisfied evaluation may have been recorded.
    assert!(
        report.events.iter().all(|event| !matches!(
            &event.event,
            Event::GoalEvaluated {
                satisfied: true,
                ..
            }
        )),
        "injected tool output must not produce a verified completion"
    );
}

#[tokio::test]
async fn runs_mixed_terminal_protocol_scenario() {
    let scenario_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../scenarios/mixed-terminal-protocol.yaml");
    let scenario = HarnessScenario::from_path(scenario_path).expect("load scenario");
    let report = HarnessRunner::new()
        .run(scenario)
        .await
        .expect("run scenario");

    assert!(report.success, "{:#?}", report.failed_expectations);
    // The mixed finish+tool response executed nothing: exactly one tool call ran —
    // the one re-issued after the protocol error feedback.
    assert_eq!(
        report.metrics.tool_calls, 1,
        "the mixed response must not execute its tool calls"
    );
}

#[tokio::test]
async fn runs_repeated_failure_stop_scenario() {
    let scenario_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../scenarios/repeated-failure-stop.yaml");
    let scenario = HarnessScenario::from_path(scenario_path).expect("load scenario");
    let report = HarnessRunner::new()
        .run(scenario)
        .await
        .expect("run scenario");

    assert!(report.success, "{:#?}", report.failed_expectations);
    assert_eq!(report.metrics.tool_failures, 4);
    // The supervisor stopped the turn and handed the operator a resumable
    // partial result instead of letting the failing loop run on.
    let prompt = report
        .turns
        .iter()
        .find_map(|turn| match &turn.outcome {
            Some(holmes_harness::TurnOutcomeReport::NeedsUser { prompt, .. }) => {
                Some(prompt.clone())
            }
            _ => None,
        })
        .expect("supervised stop must end in a needs_user outcome");
    assert!(
        prompt.contains("failed 4 times with identical arguments"),
        "stop reason must name the repeated failing call: {prompt}"
    );
    assert!(
        prompt.contains("Remaining work:"),
        "partial result must list what is left: {prompt}"
    );
}

#[tokio::test]
async fn runs_stagnation_stop_scenario() {
    let scenario_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scenarios/stagnation-stop.yaml");
    let scenario = HarnessScenario::from_path(scenario_path).expect("load scenario");
    let report = HarnessRunner::new()
        .run(scenario)
        .await
        .expect("run scenario");

    assert!(report.success, "{:#?}", report.failed_expectations);
    let prompt = report
        .turns
        .iter()
        .find_map(|turn| match &turn.outcome {
            Some(holmes_harness::TurnOutcomeReport::NeedsUser { prompt, .. }) => {
                Some(prompt.clone())
            }
            _ => None,
        })
        .expect("stagnation stop must end in a needs_user outcome");
    assert!(
        prompt.contains("no progress for 4 consecutive iterations"),
        "stop reason must report the stagnation window: {prompt}"
    );
}

#[tokio::test]
async fn runs_tool_deadline_scenario() {
    let scenario_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scenarios/tool-deadline.yaml");
    let scenario = HarnessScenario::from_path(scenario_path).expect("load scenario");
    let started = std::time::Instant::now();
    let report = HarnessRunner::new()
        .run(scenario)
        .await
        .expect("run scenario");
    let elapsed = started.elapsed();

    assert!(report.success, "{:#?}", report.failed_expectations);
    // The tool sleeps 10s; the 150ms deadline (plus the 3s registry grace for
    // ctx-ignoring tools) must cut it off well before that.
    assert!(
        elapsed < std::time::Duration::from_secs(9),
        "hung tool held the turn for {elapsed:?}"
    );
    let tool_results: Vec<&String> = report
        .events
        .iter()
        .filter_map(|event| match &event.event {
            Event::ToolResult { content, .. } => Some(content),
            _ => None,
        })
        .collect();
    assert!(
        tool_results
            .iter()
            .any(|content| content.contains("exceeded its deadline")),
        "deadline error must be fed back to the model: {tool_results:?}"
    );
    assert!(
        tool_results
            .iter()
            .all(|content| !content.contains("SHOULD-NEVER-APPEAR")),
        "the hung tool's late output must never surface"
    );
}

#[tokio::test]
async fn runs_approval_fail_closed_scenario() {
    let scenario_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scenarios/approval-fail-closed.yaml");
    let scenario = HarnessScenario::from_path(scenario_path).expect("load scenario");
    let report = HarnessRunner::new()
        .run(scenario)
        .await
        .expect("run scenario");

    assert!(report.success, "{:#?}", report.failed_expectations);
    // The denial is recorded as a ToolBlocked event (guard_name "approval"),
    // carrying the fail-closed reason back to the model.
    let blocked: Vec<(String, String)> = report
        .events
        .iter()
        .filter_map(|event| match &event.event {
            Event::ToolBlocked {
                guard_name, reason, ..
            } => Some((guard_name.clone(), reason.clone())),
            _ => None,
        })
        .collect();
    assert!(
        blocked
            .iter()
            .any(|(guard, reason)| guard == "approval" && reason.contains("no approval surface")),
        "the fail-closed denial must be recorded as an approval block: {blocked:?}"
    );
    let tool_results: Vec<&String> = report
        .events
        .iter()
        .filter_map(|event| match &event.event {
            Event::ToolResult { content, .. } => Some(content),
            _ => None,
        })
        .collect();
    assert!(
        tool_results
            .iter()
            .all(|content| !content.contains("MUTATED")),
        "the mutating tool must never execute without an approver"
    );
}
