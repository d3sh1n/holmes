use chrono::Utc;
use holmes_core::ledger::{
    AggregateKind, CaseId, Hypothesis, HypothesisStatus, LedgerEvent, Prediction, Priority,
    UnstoredLedgerEvent, ValidatorKind, LEDGER_EVENT_SCHEMA_VERSION,
};
use holmes_core::types::SessionMode;
use holmes_session::{
    schema, CaseLedgerStore, CreateSessionParams, LedgerStoreError, SessionDB, SessionStore,
};
use std::sync::Arc;

fn session_params(id: &str, parent_session_id: Option<&str>) -> CreateSessionParams {
    CreateSessionParams {
        id: Some(id.into()),
        title: Some(id.into()),
        mode: Some(SessionMode::Mixed),
        model: None,
        system_prompt: None,
        parent_session_id: parent_session_id.map(ToOwned::to_owned),
        fork_point: None,
        source: Some("test".into()),
        tags: Vec::new(),
    }
}

fn hypothesis_event(case_id: &CaseId, event_id: &str, hypothesis_id: &str) -> UnstoredLedgerEvent {
    let now = Utc::now();
    UnstoredLedgerEvent {
        event_id: event_id.into(),
        aggregate_kind: AggregateKind::Hypothesis,
        aggregate_id: hypothesis_id.into(),
        aggregate_revision: 1,
        actor_session_id: "root".into(),
        event: LedgerEvent::HypothesisProposedV2 {
            schema_version: LEDGER_EVENT_SCHEMA_VERSION,
            hypothesis: Hypothesis {
                id: hypothesis_id.into(),
                case_id: case_id.clone(),
                claim: format!("claim for {hypothesis_id}"),
                premise_refs: Vec::new(),
                alternative_group: None,
                priority: Priority::High,
                status: HypothesisStatus::Open,
                revision: 1,
                created_by: "runtime".into(),
                created_at: now,
                updated_at: now,
            },
        },
        created_at: now,
    }
}

#[tokio::test]
async fn roots_create_cases_and_children_and_forks_inherit_them() {
    let db = SessionDB::open(":memory:").await.unwrap();
    let root = db
        .create_session(session_params("root", None))
        .await
        .unwrap();
    let child = db
        .create_session(session_params("child", Some("root")))
        .await
        .unwrap();
    let other = db
        .create_session(session_params("other", None))
        .await
        .unwrap();
    let fork = db.fork_session("root", 0, "fork").await.unwrap();

    assert!(root.case_id.starts_with("case-"));
    assert_eq!(child.case_id, root.case_id);
    assert_eq!(fork.case_id, root.case_id);
    assert_ne!(other.case_id, root.case_id);
    assert_eq!(
        db.case_id_for_session("child").await.unwrap(),
        CaseId::new(root.case_id)
    );
}

#[tokio::test]
async fn append_is_atomic_versioned_idempotent_and_replayable() {
    let db = SessionDB::open(":memory:").await.unwrap();
    let root = db
        .create_session(session_params("root", None))
        .await
        .unwrap();
    let case_id = CaseId::new(root.case_id);
    let event = hypothesis_event(&case_id, "evt-1", "hyp-1");

    let first = db
        .append(&case_id, 0, "cmd-1", vec![event.clone()])
        .await
        .unwrap();
    assert_eq!(first.previous_version, 0);
    assert_eq!(first.version, 1);
    assert!(!first.idempotent_replay);

    let retry = db
        .append(&case_id, 0, "cmd-1", vec![event.clone()])
        .await
        .unwrap();
    assert!(retry.idempotent_replay);
    assert_eq!(retry.events, first.events);

    let conflict = db
        .append(
            &case_id,
            0,
            "cmd-stale",
            vec![hypothesis_event(&case_id, "evt-2", "hyp-2")],
        )
        .await
        .unwrap_err();
    assert!(matches!(
        conflict,
        LedgerStoreError::VersionConflict {
            expected: 0,
            actual: 1,
            ..
        }
    ));

    let mut changed = event;
    if let LedgerEvent::HypothesisProposedV2 { hypothesis, .. } = &mut changed.event {
        hypothesis.claim = "different payload".into();
    }
    assert!(matches!(
        db.append(&case_id, 0, "cmd-1", vec![changed])
            .await
            .unwrap_err(),
        LedgerStoreError::IdempotencyConflict { .. }
    ));

    let snapshot = db.load(&case_id).await.unwrap();
    assert_eq!(snapshot.version, 1);
    assert!(snapshot.hypotheses.contains_key(&"hyp-1".into()));
}

#[tokio::test]
async fn invalid_multi_event_command_writes_nothing() {
    let db = SessionDB::open(":memory:").await.unwrap();
    let root = db
        .create_session(session_params("root", None))
        .await
        .unwrap();
    let case_id = CaseId::new(root.case_id);
    let hypothesis = hypothesis_event(&case_id, "evt-1", "hyp-1");
    let now = Utc::now();
    let bad_prediction = UnstoredLedgerEvent {
        event_id: "evt-2".into(),
        aggregate_kind: AggregateKind::Prediction,
        aggregate_id: "pred-1".into(),
        aggregate_revision: 1,
        actor_session_id: "root".into(),
        event: LedgerEvent::PredictionDeclaredV2 {
            schema_version: LEDGER_EVENT_SCHEMA_VERSION,
            prediction: Prediction {
                id: "pred-1".into(),
                case_id: case_id.clone(),
                hypothesis_id: "missing-hypothesis".into(),
                observable: "observable".into(),
                expected_when_true: "expected".into(),
                falsifier: "falsifier".into(),
                validator: ValidatorKind::CommandExit,
                required: true,
                revision: 1,
                created_at: now,
            },
        },
        created_at: now,
    };

    assert!(matches!(
        db.append(
            &case_id,
            0,
            "invalid-command",
            vec![hypothesis, bad_prediction]
        )
        .await
        .unwrap_err(),
        LedgerStoreError::InvalidEvent(_)
    ));
    let snapshot = db.load(&case_id).await.unwrap();
    assert_eq!(snapshot.version, 0);
    assert!(snapshot.hypotheses.is_empty());
}

#[tokio::test]
async fn concurrent_writers_use_optimistic_case_versioning() {
    let db = Arc::new(SessionDB::open(":memory:").await.unwrap());
    let root = db
        .create_session(session_params("root", None))
        .await
        .unwrap();
    let case_id = CaseId::new(root.case_id);

    let mut handles = Vec::new();
    for index in 1..=2 {
        let db = db.clone();
        let case_id = case_id.clone();
        handles.push(tokio::spawn(async move {
            db.append(
                &case_id,
                0,
                &format!("cmd-{index}"),
                vec![hypothesis_event(
                    &case_id,
                    &format!("evt-{index}"),
                    &format!("hyp-{index}"),
                )],
            )
            .await
        }));
    }
    let mut successes = 0;
    let mut conflicts = 0;
    for handle in handles {
        match handle.await.unwrap() {
            Ok(_) => successes += 1,
            Err(LedgerStoreError::VersionConflict { .. }) => conflicts += 1,
            Err(error) => panic!("unexpected append error: {error}"),
        }
    }
    assert_eq!(successes, 1);
    assert_eq!(conflicts, 1);
    assert_eq!(db.load(&case_id).await.unwrap().version, 1);
}

#[tokio::test]
async fn migration_backfills_legacy_parent_tree_into_one_case() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(schema::schema_version_table()).unwrap();
        for (index, migration) in schema::MIGRATIONS.iter().take(7).enumerate() {
            conn.execute_batch(migration).unwrap();
            conn.execute(
                "INSERT INTO schema_version (version) VALUES (?1)",
                rusqlite::params![(index + 1) as u32],
            )
            .unwrap();
        }
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO sessions (id, started_at) VALUES ('legacy-root', ?1)",
            rusqlite::params![now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, parent_session_id, started_at)
             VALUES ('legacy-child', 'legacy-root', ?1)",
            rusqlite::params![now],
        )
        .unwrap();
    }

    let db = SessionDB::open(&path).await.unwrap();
    let root = db.get_session("legacy-root").await.unwrap().unwrap();
    let child = db.get_session("legacy-child").await.unwrap().unwrap();
    assert_eq!(root.case_id, "legacy-root");
    assert_eq!(child.case_id, root.case_id);
    assert_eq!(
        db.load(&CaseId::new("legacy-root")).await.unwrap().version,
        0
    );
}

#[tokio::test]
async fn checksum_snapshot_loads_tail_and_corruption_falls_back_to_full_replay() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger-snapshot.db");
    let db = SessionDB::open(&path).await.unwrap();
    let root = db
        .create_session(session_params("snapshot-root", None))
        .await
        .unwrap();
    let case_id = CaseId::new(root.case_id);

    db.append(
        &case_id,
        0,
        "snapshot-cmd-1",
        vec![hypothesis_event(
            &case_id,
            "snapshot-evt-1",
            "snapshot-hyp-1",
        )],
    )
    .await
    .unwrap();
    let first = db.compact_snapshot(&case_id, 1).await.unwrap();
    assert!(first.written);
    assert_eq!(first.projected_seq, 1);

    db.append(
        &case_id,
        1,
        "snapshot-cmd-2",
        vec![hypothesis_event(
            &case_id,
            "snapshot-evt-2",
            "snapshot-hyp-2",
        )],
    )
    .await
    .unwrap();
    let with_tail = db.load(&case_id).await.unwrap();
    assert_eq!(with_tail.version, 2);
    assert_eq!(with_tail.hypotheses.len(), 2);
    assert!(!db.compact_snapshot(&case_id, 2).await.unwrap().written);

    rusqlite::Connection::open(&path)
        .unwrap()
        .execute(
            "UPDATE case_ledger_snapshots SET checksum = 'corrupt' WHERE case_id = ?1",
            rusqlite::params![case_id.as_str()],
        )
        .unwrap();
    let replayed = db.load(&case_id).await.unwrap();
    assert_eq!(replayed, with_tail);

    let repaired = db.compact_snapshot(&case_id, 1).await.unwrap();
    assert!(repaired.written);
    assert_eq!(repaired.projected_seq, 2);

    rusqlite::Connection::open(&path)
        .unwrap()
        .execute(
            "UPDATE case_ledger_snapshots SET checksum = 'corrupt-again' WHERE case_id = ?1",
            rusqlite::params![case_id.as_str()],
        )
        .unwrap();
    assert!(db.compact_snapshot(&case_id, 100).await.unwrap().written);
}
