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
    preserve_tool_groups: true
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

    let prompt_timestamp = report
        .events
        .iter()
        .find_map(|event| match &event.event {
            Event::SessionSystemPromptSet { timestamp, .. } => Some(timestamp),
            _ => None,
        })
        .expect("session_system_prompt_set event");
    assert_eq!(*prompt_timestamp, expected_timestamp);
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
async fn runs_deductive_login_enumeration_scenario() {
    let scenario_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../scenarios/deductive-login-enumeration.yaml");
    let scenario = HarnessScenario::from_path(scenario_path).expect("load scenario");
    let report = HarnessRunner::new()
        .run(scenario)
        .await
        .expect("run scenario");

    assert!(report.success, "{:#?}", report.failed_expectations);
    assert!(report
        .events
        .iter()
        .any(|event| matches!(event.event, Event::EvidenceObserved { .. })));
    assert!(report.events.iter().any(|event| {
        matches!(
            &event.event,
            Event::HypothesisProposed { statement, .. }
                if statement.contains("leak user existence")
        )
    }));
    assert!(report.events.iter().any(|event| {
        matches!(
            &event.event,
            Event::ConclusionDrawn { conclusion, .. }
                if conclusion.contains("user-enumeration")
        )
    }));
}

#[tokio::test]
async fn runs_deductive_login_no_enumeration_scenario() {
    let scenario_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../scenarios/deductive-login-no-enumeration.yaml");
    let scenario = HarnessScenario::from_path(scenario_path).expect("load scenario");
    let report = HarnessRunner::new()
        .run(scenario)
        .await
        .expect("run scenario");

    assert!(report.success, "{:#?}", report.failed_expectations);
    assert!(report.events.iter().any(|event| {
        matches!(
            &event.event,
            Event::HypothesisContradicted { hypothesis_id, .. }
                if hypothesis_id == "hypothesis-user-enumeration"
        )
    }));
    assert!(report.events.iter().any(|event| {
        matches!(
            &event.event,
            Event::HypothesisRejected { hypothesis_id, .. }
                if hypothesis_id == "hypothesis-user-enumeration"
        )
    }));
    assert!(report.events.iter().any(|event| {
        matches!(
            &event.event,
            Event::HypothesisConfirmed { hypothesis_id, .. }
                if hypothesis_id == "hypothesis-generic-login-failure"
        )
    }));
}

#[tokio::test]
async fn runs_deductive_llm_trace_scenario() {
    let scenario_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scenarios/deductive-llm-trace.yaml");
    let scenario = HarnessScenario::from_path(scenario_path).expect("load scenario");
    let report = HarnessRunner::new()
        .run(scenario)
        .await
        .expect("run scenario");

    assert!(report.success, "{:#?}", report.failed_expectations);
    assert!(report.events.iter().any(|event| {
        matches!(
            &event.event,
            Event::HypothesisProposed { hypothesis_id, .. }
                if hypothesis_id == "hypothesis-admin-authz"
        )
    }));
    assert!(report.events.iter().any(|event| {
        matches!(
            &event.event,
            Event::HypothesisContradicted { hypothesis_id, .. }
                if hypothesis_id == "hypothesis-admin-missing"
        )
    }));
    assert!(report
        .yields
        .iter()
        .any(|event| matches!(event, holmes_runtime::RuntimeYield::PlanUpdate { .. })));
}
