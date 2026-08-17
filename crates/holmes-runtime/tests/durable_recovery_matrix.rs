//! P1-02 crash-boundary matrix (acceptance #1): durable tasks converge
//! automatically after a simulated crash (drop + reopen of the database) at
//! every state-transition boundary — enqueue, lease, heartbeat, terminal
//! commit, parent event append, mark_delivered. Convergence = the resident
//! scheduler reaps/requeues/executes, and the atomic delivery path presents
//! the result exactly once.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use holmes_core::background::TaskDeliveryOutcome;
use holmes_core::types::*;
use holmes_runtime::scheduler::{DurableTaskExecutor, DurableTaskScheduler, LeasedTask};
use holmes_session::db::*;
use holmes_session::task_store::{NewTask, TaskStatus};
use holmes_session::SessionStore;
use tokio_util::sync::CancellationToken;

fn session_params(id: &str) -> CreateSessionParams {
    CreateSessionParams {
        id: Some(id.into()),
        title: Some("crash matrix".into()),
        mode: Some(SessionMode::Pentest),
        model: None,
        system_prompt: None,
        parent_session_id: None,
        fork_point: None,
        source: Some("test".into()),
        tags: vec![],
    }
}

fn task(task_id: &str, kind: &str, safe_to_retry: bool, payload: Option<&str>) -> NewTask {
    NewTask {
        task_id: task_id.into(),
        parent_session_id: Some("parent".into()),
        kind: kind.into(),
        description: format!("task {task_id}"),
        idempotency_key: None,
        safe_to_retry,
        payload: payload.map(str::to_string),
    }
}

/// Executor recording its runs; succeeds with the task payload echoed back.
#[derive(Default)]
struct RecordingExecutor {
    runs: Mutex<Vec<String>>,
}

#[async_trait]
impl DurableTaskExecutor for RecordingExecutor {
    async fn execute(
        &self,
        task: LeasedTask,
        _cancel: CancellationToken,
    ) -> Result<String, String> {
        self.runs.lock().unwrap().push(task.task_id.clone());
        Ok(format!(
            "output:{}",
            task.payload.as_deref().unwrap_or("none")
        ))
    }
}

fn scheduler(db: &SessionDB, executor: Arc<RecordingExecutor>) -> Arc<DurableTaskScheduler> {
    Arc::new(
        DurableTaskScheduler::new(db.task_store())
            .with_executor("stub", executor)
            .with_heartbeat_interval(Duration::from_millis(20)),
    )
}

async fn wait_for_state(db: &SessionDB, task_id: &str, state: TaskStatus) {
    for _ in 0..200 {
        if let Some(record) = db.task_store().get(task_id).await.unwrap() {
            if record.state == state {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("task {task_id} never reached {state:?}");
}

// Boundary: crash after enqueue (never leased). Restart: the scheduler leases
// and executes it; the result is then deliverable to the parent.
#[tokio::test]
async fn crash_after_enqueue_converges_via_scheduler() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("holmes.db");
    {
        let db1 = SessionDB::open(&db_path).await.unwrap();
        db1.create_session(session_params("parent")).await.unwrap();
        db1.task_store()
            .enqueue(task("t", "stub", true, Some("args-v1")))
            .await
            .unwrap();
    } // drop = crash

    let db2 = SessionDB::open(&db_path).await.unwrap();
    let executor = Arc::new(RecordingExecutor::default());
    let pass = scheduler(&db2, executor.clone()).run_once().await.unwrap();
    assert_eq!(pass.leased, 1);

    wait_for_state(&db2, "t", TaskStatus::Succeeded).await;
    let record = db2.task_store().get("t").await.unwrap().unwrap();
    assert_eq!(record.result.as_deref(), Some("output:args-v1"));
    assert_eq!(
        executor.runs.lock().unwrap().len(),
        1,
        "executed exactly once"
    );

    let outcome = db2
        .deliver_task_result("parent", "t", "reminder: done")
        .await
        .unwrap();
    assert_eq!(outcome, TaskDeliveryOutcome::Appended);
}

// Boundary: crash after lease acquire / between heartbeats (lease expires
// unrenewed). Restart: scheduler recovery requeues the safe orphan and
// re-executes it in the same pass.
#[tokio::test]
async fn crash_after_lease_and_heartbeat_converges_via_scheduler() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("holmes.db");
    let crashed_owner;
    {
        let db1 = SessionDB::open(&db_path).await.unwrap();
        db1.create_session(session_params("parent")).await.unwrap();
        let store = db1.task_store();
        crashed_owner = store.owner_id().to_string();
        store
            .enqueue(task("t", "stub", true, Some("args-v1")))
            .await
            .unwrap();
        // Lease already expired by the time the next process looks.
        store
            .acquire_lease_with_duration("t", -1)
            .await
            .unwrap()
            .expect("leasable");
    } // drop = crash

    let db2 = SessionDB::open(&db_path).await.unwrap();
    assert_ne!(db2.task_store().owner_id(), crashed_owner);
    let executor = Arc::new(RecordingExecutor::default());
    let pass = scheduler(&db2, executor.clone()).run_once().await.unwrap();
    assert_eq!(pass.requeued, 1, "orphan requeued by the scheduler pass");
    assert_eq!(pass.leased, 1, "and leased in the same pass");

    wait_for_state(&db2, "t", TaskStatus::Succeeded).await;
    let record = db2.task_store().get("t").await.unwrap().unwrap();
    assert_eq!(
        record.attempt, 2,
        "re-execution ran under a new fencing token"
    );
    assert_eq!(record.result.as_deref(), Some("output:args-v1"));
}

// Boundary: crash after the terminal commit but before delivery. Restart: the
// undelivered result is found and delivered exactly once.
#[tokio::test]
async fn crash_after_terminal_commit_delivers_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("holmes.db");
    {
        let db1 = SessionDB::open(&db_path).await.unwrap();
        db1.create_session(session_params("parent")).await.unwrap();
        let store = db1.task_store();
        store.enqueue(task("t", "stub", false, None)).await.unwrap();
        let fencing = store.acquire_lease("t").await.unwrap().unwrap().attempt;
        store.complete("t", fencing, "finished").await.unwrap();
    } // drop = crash between terminal commit and delivery

    let db2 = SessionDB::open(&db_path).await.unwrap();
    let pending = db2.list_undelivered_task_results("parent").await.unwrap();
    assert_eq!(pending.len(), 1);

    assert_eq!(
        db2.deliver_task_result("parent", "t", "reminder")
            .await
            .unwrap(),
        TaskDeliveryOutcome::Appended
    );
    // Crash-safety of the delivery boundary itself: append + mark committed
    // together, so a "restart" here finds nothing pending and re-delivering
    // is a no-op instead of a duplicate.
    drop(db2);
    let db3 = SessionDB::open(&db_path).await.unwrap();
    assert!(db3
        .list_undelivered_task_results("parent")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        db3.deliver_task_result("parent", "t", "reminder")
            .await
            .unwrap(),
        TaskDeliveryOutcome::AlreadyDelivered
    );
    let reminders = db3
        .get_events("parent")
        .await
        .unwrap()
        .iter()
        .filter(|stored| {
            matches!(
                &stored.event,
                holmes_core::event::Event::UserMessage { content, .. } if content == "reminder"
            )
        })
        .count();
    assert_eq!(
        reminders, 1,
        "result presented exactly once across restarts"
    );
}

// Boundary: crash after mark_delivered (fully delivered). Restart: nothing is
// pending and nothing is re-presented.
#[tokio::test]
async fn crash_after_mark_delivered_is_clean() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("holmes.db");
    {
        let db1 = SessionDB::open(&db_path).await.unwrap();
        db1.create_session(session_params("parent")).await.unwrap();
        let store = db1.task_store();
        store.enqueue(task("t", "stub", false, None)).await.unwrap();
        let fencing = store.acquire_lease("t").await.unwrap().unwrap().attempt;
        store.complete("t", fencing, "finished").await.unwrap();
        assert_eq!(
            db1.deliver_task_result("parent", "t", "reminder")
                .await
                .unwrap(),
            TaskDeliveryOutcome::Appended
        );
    } // drop = crash after a fully-delivered result

    let db2 = SessionDB::open(&db_path).await.unwrap();
    assert!(db2
        .list_undelivered_task_results("parent")
        .await
        .unwrap()
        .is_empty());
    // The scheduler has nothing to do either: terminal tasks are not leasable.
    let executor = Arc::new(RecordingExecutor::default());
    let pass = scheduler(&db2, executor).run_once().await.unwrap();
    assert_eq!(pass.leased, 0);
    assert_eq!(pass.requeued, 0);
}

// A crashed side-effecting task (safe_to_retry=false) is never re-executed:
// it converges to manual_recovery_required and the scheduler leaves it alone.
#[tokio::test]
async fn crashed_side_effecting_task_suspends_instead_of_reexecuting() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("holmes.db");
    {
        let db1 = SessionDB::open(&db_path).await.unwrap();
        db1.create_session(session_params("parent")).await.unwrap();
        let store = db1.task_store();
        store
            .enqueue(task("t", "stub", false, Some("args")))
            .await
            .unwrap();
        store
            .acquire_lease_with_duration("t", -1)
            .await
            .unwrap()
            .expect("leasable");
    }

    let db2 = SessionDB::open(&db_path).await.unwrap();
    let executor = Arc::new(RecordingExecutor::default());
    let pass = scheduler(&db2, executor.clone()).run_once().await.unwrap();
    assert_eq!(pass.manual_required, 1);
    assert_eq!(pass.leased, 0, "suspended tasks are never leased");
    assert!(
        executor.runs.lock().unwrap().is_empty(),
        "never re-executed"
    );
    assert_eq!(
        db2.task_store().get("t").await.unwrap().unwrap().state,
        TaskStatus::ManualRecoveryRequired
    );
}
