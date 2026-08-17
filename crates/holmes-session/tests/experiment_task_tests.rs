use chrono::Utc;
use holmes_core::background::{DurableTaskSink, DurableTaskStart};
use holmes_core::ledger::{
    AggregateKind, CaseId, EvidenceBinding, EvidenceKind, EvidenceRecord, Experiment,
    ExperimentAssignment, ExperimentId, ExperimentStatus, Hypothesis, HypothesisId,
    HypothesisStatus, LedgerEvent, Prediction, PredictionId, Priority, RiskLevel,
    UnstoredLedgerEvent, ValidatorKind, VerificationMethod, LEDGER_EVENT_SCHEMA_VERSION,
};
use holmes_core::subagent::{
    wrap_for_parent, AgentTaskResult, AgentTaskStatus, EvidenceRef, ResourceUsage,
};
use holmes_core::tool_types::ToolOutcomeStatus;
use holmes_core::types::{OutputSchema, SessionMode, SubAgentConstraints, SubAgentTask};
use holmes_session::{
    CaseLedgerStore, CreateSessionParams, RecoveryResolution, SessionDB, SessionStore, TaskStatus,
};
use std::sync::Arc;

fn session_params(id: &str, parent: Option<&str>) -> CreateSessionParams {
    CreateSessionParams {
        id: Some(id.into()),
        title: Some(id.into()),
        mode: Some(SessionMode::Mixed),
        model: None,
        system_prompt: None,
        parent_session_id: parent.map(ToOwned::to_owned),
        fork_point: None,
        source: Some("test".into()),
        tags: Vec::new(),
    }
}

async fn planned_experiment(
    db: &SessionDB,
    root_id: &str,
    safe_to_retry: bool,
) -> (CaseId, ExperimentAssignment) {
    let root = db
        .create_session(session_params(root_id, None))
        .await
        .unwrap();
    let case_id = CaseId::new(root.case_id);
    let now = Utc::now();
    let hypothesis_id = HypothesisId::new("hyp-1");
    let prediction_id = PredictionId::new("pred-1");
    let experiment_id = ExperimentId::new("exp-1");
    let events = vec![
        UnstoredLedgerEvent {
            event_id: format!("{root_id}-hyp"),
            aggregate_kind: AggregateKind::Hypothesis,
            aggregate_id: "hyp-1".into(),
            aggregate_revision: 1,
            actor_session_id: root_id.into(),
            event: LedgerEvent::HypothesisProposedV2 {
                schema_version: LEDGER_EVENT_SCHEMA_VERSION,
                hypothesis: Hypothesis {
                    id: hypothesis_id.clone(),
                    case_id: case_id.clone(),
                    claim: "A falsifiable claim".into(),
                    premise_refs: Vec::new(),
                    alternative_group: None,
                    priority: Priority::High,
                    status: HypothesisStatus::Open,
                    revision: 1,
                    created_by: root_id.into(),
                    created_at: now,
                    updated_at: now,
                },
            },
            created_at: now,
        },
        UnstoredLedgerEvent {
            event_id: format!("{root_id}-pred"),
            aggregate_kind: AggregateKind::Prediction,
            aggregate_id: "pred-1".into(),
            aggregate_revision: 1,
            actor_session_id: root_id.into(),
            event: LedgerEvent::PredictionDeclaredV2 {
                schema_version: LEDGER_EVENT_SCHEMA_VERSION,
                prediction: Prediction {
                    id: prediction_id.clone(),
                    case_id: case_id.clone(),
                    hypothesis_id: hypothesis_id.clone(),
                    observable: "command exits zero".into(),
                    expected_when_true: "exit 0".into(),
                    falsifier: "non-zero exit".into(),
                    validator: ValidatorKind::CommandExit,
                    required: true,
                    revision: 1,
                    created_at: now,
                },
            },
            created_at: now,
        },
        UnstoredLedgerEvent {
            event_id: format!("{root_id}-exp"),
            aggregate_kind: AggregateKind::Experiment,
            aggregate_id: "exp-1".into(),
            aggregate_revision: 1,
            actor_session_id: root_id.into(),
            event: LedgerEvent::ExperimentPlannedV2 {
                schema_version: LEDGER_EVENT_SCHEMA_VERSION,
                experiment: Experiment {
                    id: experiment_id.clone(),
                    case_id: case_id.clone(),
                    hypothesis_ids: vec![hypothesis_id],
                    prediction_ids: vec![prediction_id],
                    action: "run a read-only command".into(),
                    expected_observations: vec!["exit 0".into()],
                    tool_allowlist: vec!["execute_command".into()],
                    risk: RiskLevel::Low,
                    status: ExperimentStatus::Planned,
                    task_id: None,
                    attempt: 0,
                    idempotency_key: format!("{root_id}-experiment-key"),
                    evidence_ids: Vec::new(),
                    revision: 1,
                    created_by: root_id.into(),
                    created_at: now,
                    updated_at: now,
                },
            },
            created_at: now,
        },
    ];
    db.append(&case_id, 0, &format!("{root_id}-plan"), events)
        .await
        .unwrap();
    (
        case_id.clone(),
        ExperimentAssignment {
            case_id,
            experiment_id,
            expected_revision: 1,
            max_concurrent_per_case: 2,
            lease_ms: 60_000,
            safe_to_retry,
        },
    )
}

fn task_start(root_id: &str, task_id: &str, assignment: ExperimentAssignment) -> DurableTaskStart {
    let payload = serde_json::to_string(&SubAgentTask {
        task: "execute the assigned experiment".into(),
        context_summary: serde_json::json!({"case": assignment.case_id}),
        expected_output: OutputSchema {
            schema: "AgentTaskResult".into(),
            required_fields: vec!["evidence".into()],
        },
        constraints: SubAgentConstraints {
            tools_allowlist: vec!["execute_command".into()],
            max_turns: 2,
            isolation: Some("child_session".into()),
        },
        run_in_background: true,
        ledger_assignment: Some(assignment.clone()),
    })
    .unwrap();
    DurableTaskStart {
        task_id: task_id.into(),
        description: "assigned experiment".into(),
        parent_session_id: Some(root_id.into()),
        idempotency_key: None,
        safe_to_retry: assignment.safe_to_retry,
        payload: Some(payload),
        experiment: Some(assignment),
    }
}

#[tokio::test]
async fn experiment_task_queues_starts_and_observes_only_verified_child_evidence() {
    let db = SessionDB::open(":memory:").await.unwrap();
    let (case_id, assignment) = planned_experiment(&db, "root-observe", true).await;
    let store = db.task_store();
    let fencing = store
        .task_started(task_start(
            "root-observe",
            "task-observe",
            assignment.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(fencing, 1);
    let running = db.load(&case_id).await.unwrap();
    let experiment = running.experiments.get(&assignment.experiment_id).unwrap();
    assert_eq!(experiment.status, ExperimentStatus::Running);
    assert_eq!(experiment.task_id.as_deref(), Some("task-observe"));
    assert_eq!(experiment.attempt, 1);

    db.create_session(session_params("child-observe", Some("root-observe")))
        .await
        .unwrap();
    store
        .task_attached_session("task-observe", fencing, "child-observe")
        .await
        .unwrap();
    let now = Utc::now();
    let evidence = EvidenceRecord {
        id: "ev-child-1".into(),
        binding: EvidenceBinding {
            case_id: case_id.clone(),
            contract_id: None,
            requirement_ids: Vec::new(),
            experiment_id: Some(assignment.experiment_id.clone()),
            prediction_ids: vec!["pred-1".into()],
            tool_call_id: Some("call-child-1".into()),
        },
        source_session_id: "child-observe".into(),
        tool: "execute_command".into(),
        tool_call_id: Some("call-child-1".into()),
        outcome_status: ToolOutcomeStatus::Succeeded,
        exit_code: Some(0),
        kind: EvidenceKind::Deterministic,
        input_summary: "read-only command".into(),
        output_hash: holmes_core::content_hash("ok"),
        output_snippet: "ok".into(),
        predicate: "exit_code == 0".into(),
        verified_by: VerificationMethod::Runtime,
        recorded_at: now,
    };
    db.append(
        &case_id,
        running.version,
        "child-evidence",
        vec![UnstoredLedgerEvent {
            event_id: "event-child-evidence".into(),
            aggregate_kind: AggregateKind::Evidence,
            aggregate_id: evidence.id.clone(),
            aggregate_revision: 1,
            actor_session_id: "child-observe".into(),
            event: LedgerEvent::EvidenceRecordedV2 {
                schema_version: LEDGER_EVENT_SCHEMA_VERSION,
                evidence,
            },
            created_at: now,
        }],
    )
    .await
    .unwrap();

    let verified = wrap_for_parent(AgentTaskResult {
        task_id: "task-observe".into(),
        status: AgentTaskStatus::Completed,
        summary: "experiment executed".into(),
        findings: Vec::new(),
        evidence: vec![EvidenceRef {
            kind: "ledger_evidence".into(),
            reference: "ev-child-1".into(),
            note: None,
        }],
        changed_files: Vec::new(),
        validations: Vec::new(),
        remaining_work: Vec::new(),
        usage: ResourceUsage::default(),
        checkpoint: Some("child-observe".into()),
    });
    store
        .task_completed(
            "task-observe",
            fencing,
            &Ok(serde_json::to_string(&verified).unwrap()),
        )
        .await
        .unwrap();

    assert_eq!(
        store.get("task-observe").await.unwrap().unwrap().state,
        TaskStatus::Succeeded
    );
    let observed = db.load(&case_id).await.unwrap();
    let experiment = observed.experiments.get(&assignment.experiment_id).unwrap();
    assert_eq!(experiment.status, ExperimentStatus::Observed);
    assert_eq!(experiment.evidence_ids, vec!["ev-child-1"]);
}

#[tokio::test]
async fn duplicate_mapping_is_exclusive_and_reclaimed_attempt_fences_late_writer() {
    let db = Arc::new(SessionDB::open(":memory:").await.unwrap());
    let (case_id, assignment) = planned_experiment(&db, "root-fence", true).await;
    let store = db.task_store();
    let first = store
        .task_started(task_start("root-fence", "task-fence", assignment.clone()))
        .await
        .unwrap();
    assert!(store
        .task_started(task_start(
            "root-fence",
            "task-duplicate",
            assignment.clone()
        ))
        .await
        .is_err());
    assert!(store
        .fail("task-fence", first as u32, "transient", true)
        .await
        .unwrap());
    let second = store.acquire_lease("task-fence").await.unwrap().unwrap();
    assert_eq!(second.attempt, 2);
    assert!(!store
        .complete("task-fence", first as u32, "late")
        .await
        .unwrap());
    let snapshot = db.load(&case_id).await.unwrap();
    let experiment = snapshot.experiments.get(&assignment.experiment_id).unwrap();
    assert_eq!(experiment.status, ExperimentStatus::Running);
    assert_eq!(experiment.attempt, 2);
    assert!(store.cancel_attempt("task-fence", 2).await.unwrap());
}

#[tokio::test]
async fn concurrent_experiment_claim_has_exactly_one_winner() {
    let db = Arc::new(SessionDB::open(":memory:").await.unwrap());
    let (_, assignment) = planned_experiment(&db, "root-race", true).await;
    let left = db.task_store();
    let right = db.task_store();
    let left_start = task_start("root-race", "task-race-left", assignment.clone());
    let right_start = task_start("root-race", "task-race-right", assignment);
    let (left_result, right_result) = tokio::join!(
        left.task_started(left_start),
        right.task_started(right_start)
    );
    assert_eq!(
        usize::from(left_result.is_ok()) + usize::from(right_result.is_ok()),
        1
    );
    let tasks = db.task_store().list_by_parent("root-race").await.unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].state, TaskStatus::Running);
}

#[tokio::test]
async fn malformed_or_evidence_free_result_fails_task_and_experiment_closed() {
    let db = SessionDB::open(":memory:").await.unwrap();
    let (case_id, assignment) = planned_experiment(&db, "root-invalid", true).await;
    let store = db.task_store();
    let fencing = store
        .task_started(task_start(
            "root-invalid",
            "task-invalid",
            assignment.clone(),
        ))
        .await
        .unwrap();
    store
        .task_completed("task-invalid", fencing, &Ok("{}".into()))
        .await
        .unwrap();
    let task = store.get("task-invalid").await.unwrap().unwrap();
    assert_eq!(task.state, TaskStatus::Failed);
    assert!(task
        .last_error
        .as_deref()
        .is_some_and(|error| error.contains("not a valid VerifiedAgentTaskResult")));
    assert_eq!(
        db.load(&case_id)
            .await
            .unwrap()
            .experiments
            .get(&assignment.experiment_id)
            .unwrap()
            .status,
        ExperimentStatus::Failed
    );
}

#[tokio::test]
async fn operator_cancel_and_unsafe_expiry_update_task_and_ledger_atomically() {
    let db = SessionDB::open(":memory:").await.unwrap();
    let (cancel_case, cancel_assignment) = planned_experiment(&db, "root-cancel", true).await;
    let store = db.task_store();
    store
        .task_started(task_start(
            "root-cancel",
            "task-cancel",
            cancel_assignment.clone(),
        ))
        .await
        .unwrap();
    assert!(store.cancel("task-cancel").await.unwrap());
    assert_eq!(
        db.load(&cancel_case)
            .await
            .unwrap()
            .experiments
            .get(&cancel_assignment.experiment_id)
            .unwrap()
            .status,
        ExperimentStatus::Cancelled
    );

    let (expire_case, mut expire_assignment) = planned_experiment(&db, "root-expire", false).await;
    expire_assignment.lease_ms = 1;
    store
        .task_started(task_start(
            "root-expire",
            "task-expire",
            expire_assignment.clone(),
        ))
        .await
        .unwrap();
    let outcome = store
        .recover_expired_leases(Utc::now() + chrono::Duration::seconds(1))
        .await
        .unwrap();
    assert!(outcome
        .manual_required
        .iter()
        .any(|record| record.task_id == "task-expire"));
    assert_eq!(
        db.load(&expire_case)
            .await
            .unwrap()
            .experiments
            .get(&expire_assignment.experiment_id)
            .unwrap()
            .status,
        ExperimentStatus::Expired
    );
}

#[tokio::test]
async fn recovery_requeue_preserves_running_experiment_until_new_fenced_attempt() {
    let db = SessionDB::open(":memory:").await.unwrap();
    let (case_id, mut assignment) = planned_experiment(&db, "root-retry", true).await;
    assignment.lease_ms = 1;
    let store = db.task_store();
    store
        .task_started(task_start("root-retry", "task-retry", assignment.clone()))
        .await
        .unwrap();
    let outcome = store
        .recover_expired_leases(Utc::now() + chrono::Duration::seconds(1))
        .await
        .unwrap();
    assert_eq!(outcome.recovering.len(), 1);
    assert!(store
        .resolve_recovering("task-retry", RecoveryResolution::Requeue)
        .await
        .unwrap());
    let second = store.acquire_lease("task-retry").await.unwrap().unwrap();
    assert_eq!(second.attempt, 2);
    let snapshot = db.load(&case_id).await.unwrap();
    let experiment = snapshot.experiments.get(&assignment.experiment_id).unwrap();
    assert_eq!(experiment.status, ExperimentStatus::Running);
    assert_eq!(experiment.attempt, 2);
}

#[tokio::test]
async fn operator_failure_of_recovering_task_fails_experiment_in_same_transaction() {
    let db = SessionDB::open(":memory:").await.unwrap();
    let (case_id, mut assignment) = planned_experiment(&db, "root-recovery-fail", true).await;
    assignment.lease_ms = 1;
    let store = db.task_store();
    store
        .task_started(task_start(
            "root-recovery-fail",
            "task-recovery-fail",
            assignment.clone(),
        ))
        .await
        .unwrap();
    store
        .recover_expired_leases(Utc::now() + chrono::Duration::seconds(1))
        .await
        .unwrap();
    assert!(store
        .resolve_recovering(
            "task-recovery-fail",
            RecoveryResolution::Fail("operator rejected retry".into())
        )
        .await
        .unwrap());
    assert_eq!(
        store
            .get("task-recovery-fail")
            .await
            .unwrap()
            .unwrap()
            .state,
        TaskStatus::Failed
    );
    assert_eq!(
        db.load(&case_id)
            .await
            .unwrap()
            .experiments
            .get(&assignment.experiment_id)
            .unwrap()
            .status,
        ExperimentStatus::Failed
    );
}
