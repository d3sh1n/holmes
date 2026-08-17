//! P2-02 crash-point matrix (session-store level): deterministic
//! drop-and-reopen ("simulated kill -9") tests at every durability boundary.
//! Each test maps to one row of the review's acceptance matrix (§10). The
//! full matrix across the workspace:
//!
//! | crash point                     | covering test (file)                                            |
//! |---------------------------------|-----------------------------------------------------------------|
//! | enqueue                         | crash_after_enqueue_converges_via_scheduler (holmes-runtime     |
//! |                                 |   durable_recovery_matrix.rs);                                  |
//! |                                 | idempotency_key_deduplicates_enqueue (durability_tests.rs)      |
//! | lease / heartbeat               | crash_after_lease_and_heartbeat_converges_via_scheduler         |
//! |                                 |   (holmes-runtime durable_recovery_matrix.rs)                   |
//! | terminal commit                 | crash_after_terminal_commit_delivers_exactly_once (ditto)       |
//! | parent event append (delivery)  | crash_between_terminal_commit_and_delivery in this file;        |
//! |                                 | failed_delivery_appends_no_partial_parent_event in this file    |
//! | mark_delivered                  | crash_after_mark_delivered_is_clean (holmes-runtime)            |
//! | task checkpoint write           | crash_after_checkpoint_write_keeps_checkpoint_across_recovery   |
//! |                                 |   in this file                                                  |
//! | large blob commit               | crash_after_large_blob_commit_restores_payload_and_projection,  |
//! |                                 | failed_blob_append_rolls_back_event_and_blob_rows in this file  |
//! | sidecar loss / projection fail  | large_tool_result_survives_total_sidecar_loss (session_tests);  |
//! |                                 | failed_projection_tail_is_rebuilt_on_reopen (durability_tests)  |
//! | migration concurrency           | concurrent_open_of_unmigrated_database_migrates_exactly_once    |
//! |                                 |   (migration_tests.rs)                                          |

use holmes_core::background::TaskDeliveryOutcome;
use holmes_core::event::Event;
use holmes_core::types::*;
use holmes_session::db::*;
use holmes_session::task_store::{NewTask, RecoveryResolution, TaskStatus};
use holmes_session::SessionStore;

fn session_params(id: &str) -> CreateSessionParams {
    CreateSessionParams {
        id: Some(id.into()),
        title: Some("crash point matrix".into()),
        mode: Some(SessionMode::Pentest),
        model: None,
        system_prompt: None,
        parent_session_id: None,
        fork_point: None,
        source: Some("test".into()),
        tags: vec![],
    }
}

fn new_task(task_id: &str, safe_to_retry: bool) -> NewTask {
    NewTask {
        task_id: task_id.into(),
        parent_session_id: Some("parent".into()),
        kind: "subagent".into(),
        description: format!("task {task_id}"),
        idempotency_key: None,
        safe_to_retry,
        payload: None,
    }
}

fn transcript_path(db_path: &std::path::Path, session_id: &str) -> std::path::PathBuf {
    db_path
        .parent()
        .unwrap()
        .join("sessions")
        .join(session_id)
        .join("transcript.jsonl")
}

// Boundary: crash after a running task's checkpoint write, before any terminal
// commit. Restart: the checkpoint is durable, recovery requeues the task under
// a new fencing token, the checkpoint survives for the next attempt to resume
// from, and the crashed attempt's late checkpoint write is fenced out.
#[tokio::test]
async fn crash_after_checkpoint_write_keeps_checkpoint_across_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("holmes.db");

    let crashed_fencing = {
        let db1 = SessionDB::open(&db_path).await.unwrap();
        db1.create_session(session_params("parent")).await.unwrap();
        let store1 = db1.task_store();
        store1.enqueue(new_task("t", true)).await.unwrap();
        // Lease already expired by the time the next process looks.
        let leased = store1
            .acquire_lease_with_duration("t", -1)
            .await
            .unwrap()
            .expect("leasable");
        assert!(store1
            .save_checkpoint("t", leased.attempt, "cp:step-3")
            .await
            .unwrap());
        leased.attempt
    }; // drop = crash after the checkpoint write

    let db2 = SessionDB::open(&db_path).await.unwrap();
    let store2 = db2.task_store();
    let record = store2.get("t").await.unwrap().unwrap();
    assert_eq!(
        record.checkpoint.as_deref(),
        Some("cp:step-3"),
        "checkpoint write survived the crash"
    );
    assert_eq!(record.state, TaskStatus::Running);

    // Restart recovery: expired lease -> Recovering -> Requeue -> re-lease.
    let outcome = store2
        .recover_expired_leases(chrono::Utc::now())
        .await
        .unwrap();
    assert_eq!(outcome.recovering.len(), 1);
    assert!(store2
        .resolve_recovering("t", RecoveryResolution::Requeue)
        .await
        .unwrap());
    let released = store2
        .acquire_lease("t")
        .await
        .unwrap()
        .expect("releasable");
    assert_eq!(released.attempt, crashed_fencing + 1, "new fencing token");
    assert_eq!(
        released.checkpoint.as_deref(),
        Some("cp:step-3"),
        "checkpoint still available for the resumed attempt"
    );

    // The crashed attempt's late write is fenced out after the reclaim.
    assert!(
        !store2
            .save_checkpoint("t", crashed_fencing, "stale-cp")
            .await
            .unwrap(),
        "superseded attempt must not overwrite the checkpoint"
    );
    assert_eq!(
        store2
            .get("t")
            .await
            .unwrap()
            .unwrap()
            .checkpoint
            .as_deref(),
        Some("cp:step-3")
    );
}

// Boundary: crash between the terminal commit and the parent-event append.
// The result stays undelivered and is delivered exactly once afterwards.
// (The delivery transaction itself — append + mark_delivered — is
// all-or-nothing, so a crash inside it is indistinguishable from "crashed
// before it": the task simply remains undelivered.)
#[tokio::test]
async fn crash_between_terminal_commit_and_delivery_recovers_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("holmes.db");
    {
        let db1 = SessionDB::open(&db_path).await.unwrap();
        db1.create_session(session_params("parent")).await.unwrap();
        let store1 = db1.task_store();
        store1.enqueue(new_task("t", false)).await.unwrap();
        let fencing = store1.acquire_lease("t").await.unwrap().unwrap().attempt;
        store1
            .complete("t", fencing, "result-payload")
            .await
            .unwrap();
    } // drop = crash with the result committed but never delivered

    let db2 = SessionDB::open(&db_path).await.unwrap();
    let pending = db2.list_undelivered_task_results("parent").await.unwrap();
    assert_eq!(pending.len(), 1, "undelivered result survives the crash");
    assert_eq!(pending[0].result.as_deref(), Some("result-payload"));

    assert_eq!(
        db2.deliver_task_result("parent", "t", "reminder: done")
            .await
            .unwrap(),
        TaskDeliveryOutcome::Appended
    );
    assert!(db2
        .list_undelivered_task_results("parent")
        .await
        .unwrap()
        .is_empty());
}

// Failure-path atomicity of the parent-event append: deliveries that cannot
// commit (unknown task, not-yet-terminal task) append nothing — no partial
// parent event, no phantom pending delivery.
#[tokio::test]
async fn failed_delivery_appends_no_partial_parent_event() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("holmes.db");
    let db = SessionDB::open(&db_path).await.unwrap();
    db.create_session(session_params("parent")).await.unwrap();
    let store = db.task_store();
    store.enqueue(new_task("running", false)).await.unwrap();
    store.acquire_lease("running").await.unwrap().unwrap();

    assert_eq!(
        db.deliver_task_result("parent", "running", "too early")
            .await
            .unwrap(),
        TaskDeliveryOutcome::NotTerminal
    );
    assert_eq!(
        db.deliver_task_result("parent", "ghost", "nobody home")
            .await
            .unwrap(),
        TaskDeliveryOutcome::UnknownTask
    );
    assert!(
        db.get_events("parent").await.unwrap().is_empty(),
        "failed deliveries must not append a parent event"
    );
    assert!(db
        .list_undelivered_task_results("parent")
        .await
        .unwrap()
        .is_empty());
}

// Boundary: crash right after a large-blob event commit, with the transcript
// projection possibly still pending (no flush before drop). Restart: the
// payload is restored from SQLite alone and open-time reconcile catches the
// projection up to exactly one transcript line.
#[tokio::test]
async fn crash_after_large_blob_commit_restores_payload_and_projection() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("holmes.db");
    let large_content = "blob-crash-payload-".repeat(2000); // 38 000 chars > 10 000 threshold
    {
        let db1 = SessionDB::open(&db_path).await.unwrap();
        db1.create_session(session_params("s")).await.unwrap();
        db1.append_event(
            "s",
            &Event::ToolResult {
                name: "nmap".into(),
                success: true,
                outcome: Some(holmes_core::ToolOutcomeStatus::Succeeded),
                content: large_content.clone(),
                error: None,
                artifacts: vec![],
                call_id: Some("call-9".into()),
            },
        )
        .await
        .unwrap();
        // Deliberately no projector flush: drop = crash with the projection
        // queue possibly unprocessed.
    }

    let db2 = SessionDB::open(&db_path).await.unwrap();
    let events = db2.get_events("s").await.unwrap();
    assert_eq!(events.len(), 1);
    match &events[0].event {
        Event::ToolResult {
            content, call_id, ..
        } => {
            assert_eq!(content.as_str(), large_content.as_str());
            assert_eq!(call_id.as_deref(), Some("call-9"));
        }
        other => panic!("expected ToolResult, got {other:?}"),
    }

    // Open-time reconcile repaired whatever the projection missed.
    let transcript = std::fs::read_to_string(transcript_path(&db_path, "s")).unwrap();
    assert_eq!(transcript.lines().count(), 1, "exactly one projected line");
    assert!(transcript.contains(holmes_session::blob_store::BLOB_REF_PREFIX));
}

// Crash-equivalent of a large-blob commit that fails mid-transaction (here:
// FK rejection because the session does not exist): the blob rows and the
// event row commit or roll back together, so no event marker can ever point
// at a missing payload.
#[tokio::test]
async fn failed_blob_append_rolls_back_event_and_blob_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("holmes.db");
    let db = SessionDB::open(&db_path).await.unwrap();

    let result = db
        .append_event(
            "ghost-session",
            &Event::ToolResult {
                name: "nmap".into(),
                success: true,
                outcome: Some(holmes_core::ToolOutcomeStatus::Succeeded),
                content: "x".repeat(30_000),
                error: None,
                artifacts: vec![],
                call_id: None,
            },
        )
        .await;
    assert!(result.is_err(), "append to a missing session must fail");

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(
        count("SELECT COUNT(*) FROM events WHERE session_id = 'ghost-session'"),
        0,
        "no partial event row"
    );
    assert_eq!(count("SELECT COUNT(*) FROM blobs"), 0, "no orphaned blob");
    assert_eq!(
        count("SELECT COUNT(*) FROM blob_chunks"),
        0,
        "no orphaned blob chunks"
    );
}
