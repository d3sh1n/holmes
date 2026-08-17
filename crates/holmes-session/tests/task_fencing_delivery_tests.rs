//! P1-02 acceptance tests: lease fencing, atomic result delivery, and
//! race-free idempotent enqueue for the durable task store.

use holmes_core::background::TaskDeliveryOutcome;
use holmes_core::types::*;
use holmes_session::db::*;
use holmes_session::task_store::{NewTask, RecoveryResolution, TaskStatus};
use holmes_session::SessionStore;

fn session_params(id: &str) -> CreateSessionParams {
    CreateSessionParams {
        id: Some(id.into()),
        title: Some("p1-02 test".into()),
        mode: Some(SessionMode::Pentest),
        model: None,
        system_prompt: None,
        parent_session_id: None,
        fork_point: None,
        source: Some("test".into()),
        tags: vec![],
    }
}

fn new_task(task_id: &str, idempotency_key: Option<&str>) -> NewTask {
    NewTask {
        task_id: task_id.into(),
        parent_session_id: Some("parent-session".into()),
        kind: "subagent".into(),
        description: "recon".into(),
        idempotency_key: idempotency_key.map(str::to_string),
        safe_to_retry: true,
        payload: None,
    }
}

async fn create_parent_session(db: &SessionDB) {
    db.create_session(session_params("parent-session"))
        .await
        .unwrap();
}

// P1-02 acceptance #3: 100 concurrent enqueues with the same idempotency key
// (each caller holding its own task id, split across two independent database
// handles — the cross-process shape) must produce exactly one row, and every
// caller must observe the SAME task id.
#[tokio::test]
async fn concurrent_enqueue_same_idempotency_key_yields_one_task() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("holmes.db");
    let db1 = std::sync::Arc::new(SessionDB::open(&db_path).await.unwrap());
    create_parent_session(&db1).await;
    let db2 = std::sync::Arc::new(SessionDB::open(&db_path).await.unwrap());

    let mut handles = Vec::new();
    for i in 0..100 {
        let store = if i % 2 == 0 {
            db1.task_store()
        } else {
            db2.task_store()
        };
        handles.push(tokio::spawn(async move {
            store
                .enqueue(new_task(&format!("caller-{i}"), Some("spawn:recon:target")))
                .await
                .expect("concurrent enqueue must not fail")
                .task_id
        }));
    }
    let mut returned_ids = std::collections::HashSet::new();
    for handle in handles {
        returned_ids.insert(handle.await.unwrap());
    }

    assert_eq!(
        returned_ids.len(),
        1,
        "every caller must get the same task id, got {returned_ids:?}"
    );
    let tasks = db1
        .task_store()
        .list_by_parent("parent-session")
        .await
        .unwrap();
    assert_eq!(tasks.len(), 1, "exactly one row for the raced key");
    assert!(returned_ids.contains(&tasks[0].task_id));
}

// P1-02 acceptance #2: once a new attempt owns the lease, the old attempt's
// heartbeat, checkpoint, child-session and terminal writes are all rejected.
#[tokio::test]
async fn old_attempt_writes_are_fenced_out_after_reclaim() {
    let db = SessionDB::open(":memory:").await.unwrap();
    create_parent_session(&db).await;
    let store = db.task_store();

    store.enqueue(new_task("t", None)).await.unwrap();
    let first = store.acquire_lease("t").await.unwrap().unwrap();
    assert_eq!(first.attempt, 1);

    // The first attempt dies; recovery requeues and a second attempt acquires.
    let outcome = store
        .recover_expired_leases(chrono::Utc::now() + chrono::Duration::seconds(600))
        .await
        .unwrap();
    assert_eq!(outcome.recovering.len(), 1);
    store
        .resolve_recovering("t", RecoveryResolution::Requeue)
        .await
        .unwrap();
    let second = store.acquire_lease("t").await.unwrap().unwrap();
    assert_eq!(second.attempt, 2, "new attempt bumps the fencing token");

    // Old attempt (fencing 1): every write loses.
    assert!(!store.heartbeat("t", 1).await.unwrap(), "stale heartbeat");
    assert!(
        !store.save_checkpoint("t", 1, "cp").await.unwrap(),
        "stale checkpoint"
    );
    assert!(
        !store.set_child_session("t", 1, "child-x").await.unwrap(),
        "stale child-session link"
    );
    assert!(
        !store.complete("t", 1, "old result").await.unwrap(),
        "stale completion"
    );
    assert!(
        !store.fail("t", 1, "old error", false).await.unwrap(),
        "stale failure"
    );
    assert!(!store.cancel_attempt("t", 1).await.unwrap(), "stale cancel");

    let record = store.get("t").await.unwrap().unwrap();
    assert_eq!(
        record.state,
        TaskStatus::Running,
        "old attempt changed nothing"
    );
    assert!(record.result.is_none() && record.checkpoint.is_none());

    // New attempt (fencing 2): every write lands.
    assert!(store.heartbeat("t", 2).await.unwrap());
    assert!(store.save_checkpoint("t", 2, "cp-2").await.unwrap());
    assert!(store.complete("t", 2, "new result").await.unwrap());
    let record = store.get("t").await.unwrap().unwrap();
    assert_eq!(record.state, TaskStatus::Succeeded);
    assert_eq!(record.result.as_deref(), Some("new result"));
    assert_eq!(record.checkpoint.as_deref(), Some("cp-2"));

    // A terminal task rejects further fenced writes from either attempt.
    assert!(!store.complete("t", 2, "double").await.unwrap());
}

// P1-02 acceptance #1 (delivery boundaries): a terminal result committed
// before a crash is delivered after restart exactly once — the event append
// and the delivered mark commit atomically, and repeats are no-ops.
#[tokio::test]
async fn durable_delivery_is_atomic_and_idempotent_across_restart() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("holmes.db");

    // "First process": finish a task, then die before any delivery.
    {
        let db1 = SessionDB::open(&db_path).await.unwrap();
        create_parent_session(&db1).await;
        let store = db1.task_store();
        store.enqueue(new_task("t", None)).await.unwrap();
        let fencing = store.acquire_lease("t").await.unwrap().unwrap().attempt;
        assert!(store.complete("t", fencing, "recon result").await.unwrap());
    }

    // "Second process": the result is still pending delivery.
    let db2 = SessionDB::open(&db_path).await.unwrap();
    let pending = db2
        .list_undelivered_task_results("parent-session")
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].task_id, "t");
    assert_eq!(pending[0].result.as_deref(), Some("recon result"));

    let events_before = db2.get_events("parent-session").await.unwrap().len();
    let outcome = db2
        .deliver_task_result(
            "parent-session",
            "t",
            "<system-reminder>done</system-reminder>",
        )
        .await
        .unwrap();
    assert_eq!(outcome, TaskDeliveryOutcome::Appended);

    // Both sides of the atomic commit landed together: event appended AND the
    // task marked delivered (no crash window between them), counter in sync.
    let events = db2.get_events("parent-session").await.unwrap();
    assert_eq!(events.len(), events_before + 1);
    assert!(events.iter().any(|stored| matches!(
        &stored.event,
        holmes_core::event::Event::UserMessage { content, .. }
            if content.contains("<system-reminder>done</system-reminder>")
    )));
    let record = db2.task_store().get("t").await.unwrap().unwrap();
    assert!(record.delivered);
    let session = db2.get_session("parent-session").await.unwrap().unwrap();
    let user_messages = events
        .iter()
        .filter(|stored| matches!(stored.event, holmes_core::event::Event::UserMessage { .. }))
        .count() as u64;
    assert_eq!(session.message_count, user_messages);

    // A repeat (restart after the mark, or a second drain) appends nothing.
    let outcome = db2
        .deliver_task_result(
            "parent-session",
            "t",
            "<system-reminder>done</system-reminder>",
        )
        .await
        .unwrap();
    assert_eq!(outcome, TaskDeliveryOutcome::AlreadyDelivered);
    assert_eq!(
        db2.get_events("parent-session").await.unwrap().len(),
        events_before + 1,
        "idempotent: no duplicate reminder"
    );
    assert!(db2
        .list_undelivered_task_results("parent-session")
        .await
        .unwrap()
        .is_empty());

    // Delivery outcome taxonomy: unknown task vs non-terminal task.
    let unknown = db2
        .deliver_task_result("parent-session", "nope", "x")
        .await
        .unwrap();
    assert_eq!(unknown, TaskDeliveryOutcome::UnknownTask);
    db2.task_store()
        .enqueue(NewTask {
            task_id: "running".into(),
            ..new_task("running", None)
        })
        .await
        .unwrap();
    let running = db2
        .deliver_task_result("parent-session", "running", "x")
        .await
        .unwrap();
    assert_eq!(running, TaskDeliveryOutcome::NotTerminal);
}
