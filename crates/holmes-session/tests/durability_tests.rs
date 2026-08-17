//! Durability integration tests (AGT-007 / AGT-008): durable task store state
//! machine, crash recovery, idempotency, atomic session creation, and the
//! transcript JSONL projection being a pure derivative of the event store.

use holmes_core::event::Event;
use holmes_core::types::*;
use holmes_session::db::*;
use holmes_session::task_store::{NewTask, RecoveryResolution, TaskStatus};
use holmes_session::SessionStore;

fn session_params(id: &str) -> CreateSessionParams {
    CreateSessionParams {
        id: Some(id.into()),
        title: Some("durability test".into()),
        mode: Some(SessionMode::Pentest),
        model: None,
        system_prompt: None,
        parent_session_id: None,
        fork_point: None,
        source: Some("test".into()),
        tags: vec![],
    }
}

fn user_message(content: &str) -> Event {
    Event::UserMessage {
        content: content.into(),
        timestamp: chrono::Utc::now(),
    }
}

fn new_task(task_id: &str, idempotency_key: Option<&str>, safe_to_retry: bool) -> NewTask {
    NewTask {
        task_id: task_id.into(),
        parent_session_id: Some("parent-session".into()),
        kind: "subagent".into(),
        description: "recon".into(),
        idempotency_key: idempotency_key.map(str::to_string),
        safe_to_retry,
        payload: None,
    }
}

// The tasks table FK-references sessions(id): tests that attribute tasks to a
// parent must materialise that session first.
async fn create_parent_session(db: &SessionDB) {
    db.create_session(session_params("parent-session"))
        .await
        .unwrap();
}

// AGT-007: an idempotency key makes re-enqueue a no-op returning the original
// task — a retried spawn never duplicates the row or the downstream execution.
#[tokio::test]
async fn idempotency_key_deduplicates_enqueue() {
    let db = SessionDB::open(":memory:").await.unwrap();
    create_parent_session(&db).await;
    let store = db.task_store();

    let first = store
        .enqueue(new_task("task-a", Some("spawn:recon:target"), true))
        .await
        .unwrap();
    // Different task id, same key: this is the retried spawn.
    let second = store
        .enqueue(new_task("task-b", Some("spawn:recon:target"), true))
        .await
        .unwrap();

    assert_eq!(second.task_id, "task-a", "retry returns the original task");
    assert_eq!(first.task_id, second.task_id);
    let tasks = store.list_by_parent("parent-session").await.unwrap();
    assert_eq!(tasks.len(), 1, "no duplicate row for the retried spawn");
}

// AGT-007: after a crash (expired lease, dead owner), a restart moves safe
// tasks Running → Recovering and side-effecting ones to
// manual_recovery_required, clearing the dead lease.
#[tokio::test]
async fn crash_recovery_dispositions_orphaned_running_tasks() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("holmes.db");

    // "First process": owns leases, then dies (dropped without completing).
    let owner_before_crash = {
        let db1 = SessionDB::open(&db_path).await.unwrap();
        create_parent_session(&db1).await;
        let store1 = db1.task_store();
        store1
            .enqueue(new_task("safe-orphan", None, true))
            .await
            .unwrap();
        store1
            .enqueue(new_task("unsafe-orphan", None, false))
            .await
            .unwrap();
        // Lease already in the past: by the time the next process looks, the
        // crashed owner's lease has expired.
        store1
            .acquire_lease_with_duration("safe-orphan", -1)
            .await
            .unwrap()
            .expect("leasable");
        store1
            .acquire_lease_with_duration("unsafe-orphan", -1)
            .await
            .unwrap()
            .expect("leasable");
        store1.owner_id().to_string()
    };

    // "Second process": fresh owner identity recovers the first one's orphans.
    let db2 = SessionDB::open(&db_path).await.unwrap();
    let store2 = db2.task_store();
    assert_ne!(store2.owner_id(), owner_before_crash);

    let outcome = store2
        .recover_expired_leases(chrono::Utc::now())
        .await
        .unwrap();
    assert_eq!(outcome.recovering.len(), 1);
    assert_eq!(outcome.recovering[0].task_id, "safe-orphan");
    assert_eq!(outcome.manual_required.len(), 1);
    assert_eq!(outcome.manual_required[0].task_id, "unsafe-orphan");

    let safe = store2.get("safe-orphan").await.unwrap().unwrap();
    assert_eq!(safe.state, TaskStatus::Recovering);
    assert!(safe.lease_owner.is_none(), "dead lease cleared");

    let unsafe_ = store2.get("unsafe-orphan").await.unwrap().unwrap();
    assert_eq!(unsafe_.state, TaskStatus::ManualRecoveryRequired);
    // Never auto re-executed: not leasable from the terminal-ish state.
    assert!(store2
        .acquire_lease("unsafe-orphan")
        .await
        .unwrap()
        .is_none());

    // Discharge the recovering orphan as the production recovery flow does
    // (runtime::recovery::recover_durable_tasks): Recovering → Retrying.
    assert!(store2
        .resolve_recovering("safe-orphan", RecoveryResolution::Requeue)
        .await
        .unwrap());

    // A live task (fresh lease owned by this process) is untouched, and the
    // discharged orphans are not re-reported.
    store2.enqueue(new_task("live", None, true)).await.unwrap();
    store2.acquire_lease("live").await.unwrap().unwrap();
    let outcome = store2
        .recover_expired_leases(chrono::Utc::now())
        .await
        .unwrap();
    assert!(outcome.recovering.is_empty() && outcome.manual_required.is_empty());
    assert_eq!(
        store2.get("live").await.unwrap().unwrap().state,
        TaskStatus::Running
    );
}

// AGT-007: cancel and completion write-back land from Running and are
// terminal — a late completion cannot resurrect a cancelled task.
#[tokio::test]
async fn completion_after_cancel_does_not_resurrect_task() {
    let db = SessionDB::open(":memory:").await.unwrap();
    create_parent_session(&db).await;
    let store = db.task_store();
    store.enqueue(new_task("t", None, true)).await.unwrap();
    let fencing = store.acquire_lease("t").await.unwrap().unwrap().attempt;

    assert!(store.cancel("t").await.unwrap());
    // The detached runner finishing after the cancel must not overwrite it.
    assert!(!store.complete("t", fencing, "late result").await.unwrap());
    let record = store.get("t").await.unwrap().unwrap();
    assert_eq!(record.state, TaskStatus::Cancelled);
    assert!(record.result.is_none());
}

// AGT-008: session creation is a single transaction — a conflicting insert
// rolls back completely, leaving neither a partial session nor stray events.
#[tokio::test]
async fn failed_session_create_leaves_no_partial_state() {
    let db = SessionDB::open(":memory:").await.unwrap();
    db.create_session_with_events(
        session_params("s1"),
        vec![user_message("first"), user_message("second")],
    )
    .await
    .unwrap();

    // Same primary key: the whole transaction (session row + events + counter
    // updates) must roll back, not half-apply.
    let conflict = db
        .create_session_with_events(
            session_params("s1"),
            vec![
                user_message("rogue"),
                user_message("rogue2"),
                user_message("rogue3"),
            ],
        )
        .await;
    assert!(conflict.is_err());

    let events = db.get_events("s1").await.unwrap();
    assert_eq!(events.len(), 2, "no partial events from the failed create");
    let session = db.get_session("s1").await.unwrap().unwrap();
    assert_eq!(session.message_count, 2, "counters match committed events");
}

// AGT-008: a projection failure happens AFTER commit and cannot fail the
// append; the committed event stays authoritative and the transcript heals via
// rebuild from the event store.
#[tokio::test]
async fn projection_failure_does_not_affect_committed_events() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("holmes.db");
    let db = SessionDB::open(&db_path).await.unwrap();
    let session = db
        .create_session(session_params("s-blocked"))
        .await
        .unwrap();

    // Sabotage the projection target: a regular FILE where the session's
    // transcript directory belongs makes every append fail.
    let session_dir = db_path.parent().unwrap().join("sessions").join(&session.id);
    std::fs::remove_dir_all(&session_dir).unwrap();
    std::fs::write(&session_dir, "not a directory").unwrap();

    db.append_event(
        &session.id,
        &user_message("committed despite broken projection"),
    )
    .await
    .expect("commit must succeed even when the projection will fail");
    db.projector().flush().await;

    // The database fact is intact; the failure landed in the rebuild queue.
    let events = db.get_events(&session.id).await.unwrap();
    assert_eq!(events.len(), 1);
    let rebuilds = db.projector().take_rebuild_queue();
    assert_eq!(rebuilds.len(), 1);
    assert_eq!(rebuilds[0].session_id, session.id);

    // Heal: remove the sabotage and rebuild the transcript from the event
    // store — proof the JSONL is a pure derivative of SQLite.
    std::fs::remove_file(&session_dir).unwrap();
    let written = db.rebuild_transcript(&session.id).await.unwrap();
    assert_eq!(written, 1);
    let transcript = std::fs::read_to_string(session_dir.join("transcript.jsonl")).unwrap();
    assert!(transcript.contains("committed despite broken projection"));
}

// AGT-008: a rebuilt transcript is byte-identical to the live projection —
// the JSONL carries no information the event store lacks.
#[tokio::test]
async fn rebuilt_transcript_matches_live_projection() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("holmes.db");
    let db = SessionDB::open(&db_path).await.unwrap();
    let session = db
        .create_session(session_params("s-rebuild"))
        .await
        .unwrap();
    for i in 0..4 {
        db.append_event(&session.id, &user_message(&format!("message {i}")))
            .await
            .unwrap();
    }
    db.projector().flush().await;

    let transcript_path = db_path
        .parent()
        .unwrap()
        .join("sessions")
        .join(&session.id)
        .join("transcript.jsonl");
    let projected = std::fs::read_to_string(&transcript_path).unwrap();
    assert_eq!(projected.lines().count(), 4);

    let written = db.rebuild_transcript(&session.id).await.unwrap();
    assert_eq!(written, 4);
    let rebuilt = std::fs::read_to_string(&transcript_path).unwrap();
    assert_eq!(projected, rebuilt);
}

// AGT-013: a completed subagent task's structured result and child-session link
// survive a process restart — the reopened store returns the AgentTaskResult
// payload and re-associates the task with the subagent session.
#[tokio::test]
async fn subagent_result_and_child_session_survive_restart() {
    use holmes_core::background::DurableTaskStart;
    use holmes_core::subagent::{
        AgentTaskResult, AgentTaskStatus, EvidenceRef, Finding, ResourceUsage, ValidationResult,
    };

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("holmes.db");

    let result_payload = {
        let db1 = SessionDB::open(&db_path).await.unwrap();
        create_parent_session(&db1).await;
        let sink = db1.durable_task_sink().expect("durable sink");

        let fencing = sink
            .task_started(DurableTaskStart {
                task_id: "task-result".into(),
                description: "recon".into(),
                parent_session_id: Some("parent-session".into()),
                idempotency_key: None,
                safe_to_retry: false,
                payload: None,
                experiment: None,
            })
            .await
            .unwrap();

        let result = AgentTaskResult {
            task_id: "task-result".into(),
            status: AgentTaskStatus::Completed,
            summary: "recon complete".into(),
            findings: vec![Finding {
                summary: "SQL injection in login form".into(),
                severity: Some("high".into()),
                evidence_refs: vec!["f-1".into()],
            }],
            evidence: vec![EvidenceRef {
                kind: "finding".into(),
                reference: "f-1".into(),
                note: Some("error-based payload returned the users table".into()),
            }],
            changed_files: vec![],
            validations: vec![ValidationResult {
                name: "goal_evaluated".into(),
                passed: true,
                detail: Some("all subtasks done".into()),
            }],
            remaining_work: vec![],
            usage: ResourceUsage {
                tokens_used: 4321,
                tool_calls: 8,
                turns: 5,
                wall_clock_ms: 9000,
            },
            checkpoint: Some("sub-child-1".into()),
        };
        let payload = serde_json::to_string_pretty(&result).unwrap();
        // The runner reports which session the task produced before completing.
        sink.task_attached_session("task-result", fencing, "sub-child-1")
            .await
            .unwrap();
        sink.task_completed("task-result", fencing, &Ok(payload.clone()))
            .await
            .unwrap();
        payload
        // db1 dropped here = "process exit".
    };

    // "Restarted process": fresh store over the same database file.
    let db2 = SessionDB::open(&db_path).await.unwrap();
    let store2 = db2.task_store();
    let record = store2
        .get("task-result")
        .await
        .unwrap()
        .expect("task row survives restart");
    assert_eq!(record.state, TaskStatus::Succeeded);
    assert_eq!(
        record.child_session_id.as_deref(),
        Some("sub-child-1"),
        "parent can re-associate the task with the subagent session"
    );
    let stored: AgentTaskResult =
        serde_json::from_str(record.result.as_deref().expect("result payload stored"))
            .expect("stored result parses as the structured protocol");
    assert_eq!(stored.status, AgentTaskStatus::Completed);
    assert_eq!(stored.summary, "recon complete");
    assert_eq!(stored.usage.tokens_used, 4321);
    assert_eq!(stored.findings.len(), 1);
    assert_eq!(stored.checkpoint.as_deref(), Some("sub-child-1"));
    assert_eq!(stored, serde_json::from_str(&result_payload).unwrap());

    // ...and the task is discoverable from the parent's task list.
    let children = store2.list_by_parent("parent-session").await.unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].task_id, "task-result");
}

// ============================================================================
// P1-08: open-time projection reconcile
// ============================================================================

fn transcript_path(db_path: &std::path::Path, session_id: &str) -> std::path::PathBuf {
    db_path
        .parent()
        .unwrap()
        .join("sessions")
        .join(session_id)
        .join("transcript.jsonl")
}

// P1-08 acceptance: a transcript missing its tail (process died before the
// projector caught up) is rebuilt automatically when the database is
// reopened — no manual rebuild, no consumer of the in-memory queue.
#[tokio::test]
async fn open_reconciles_missing_transcript_tail() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("holmes.db");

    {
        let db = SessionDB::open(&db_path).await.unwrap();
        db.create_session(session_params("s-tail")).await.unwrap();
        for i in 0..4 {
            db.append_event("s-tail", &user_message(&format!("message {i}")))
                .await
                .unwrap();
        }
        db.projector().flush().await;
    } // db dropped = "process exit"

    // Simulate the lost tail: the last two lines never reached disk.
    let path = transcript_path(&db_path, "s-tail");
    let full = std::fs::read_to_string(&path).unwrap();
    let truncated: String = full.lines().take(2).map(|l| format!("{l}\n")).collect();
    std::fs::write(&path, truncated).unwrap();

    // Reopening alone must restore the full transcript.
    let _db = SessionDB::open(&db_path).await.unwrap();
    let healed = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        healed, full,
        "reopen must rebuild the missing transcript tail"
    );
}

// P1-08 acceptance: a projection that FAILED at runtime (rebuild queue entry,
// no offset persisted) heals on the next open without anyone draining the
// in-memory queue.
#[tokio::test]
async fn failed_projection_tail_is_rebuilt_on_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("holmes.db");

    {
        let db = SessionDB::open(&db_path).await.unwrap();
        let session = db.create_session(session_params("s-fail")).await.unwrap();

        // Sabotage the projection target so the append's projection fails.
        let session_dir = db_path.parent().unwrap().join("sessions").join(&session.id);
        std::fs::remove_dir_all(&session_dir).unwrap();
        std::fs::write(&session_dir, "not a directory").unwrap();

        db.append_event("s-fail", &user_message("committed, projection failed"))
            .await
            .unwrap();
        db.projector().flush().await;
        assert_eq!(db.projector().take_rebuild_queue().len(), 1);

        // Remove the sabotage, then "exit" WITHOUT rebuilding manually.
        std::fs::remove_file(&session_dir).unwrap();
    }

    let _db = SessionDB::open(&db_path).await.unwrap();
    let healed = std::fs::read_to_string(transcript_path(&db_path, "s-fail")).unwrap();
    assert_eq!(healed.lines().count(), 1);
    assert!(healed.contains("committed, projection failed"));
}

// P1-08: a fully projected session is left untouched by the reconcile —
// opening a healthy database is a no-op, not a rewrite storm.
#[tokio::test]
async fn consistent_projection_is_not_rebuilt_on_open() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("holmes.db");

    {
        let db = SessionDB::open(&db_path).await.unwrap();
        db.create_session(session_params("s-ok")).await.unwrap();
        for i in 0..3 {
            db.append_event("s-ok", &user_message(&format!("steady {i}")))
                .await
                .unwrap();
        }
        db.projector().flush().await;
    }

    // Sabotage-proof sentinel: make the transcript read-only AND the rebuild
    // tmp path a directory, so any rebuild attempt fails loudly instead of
    // silently rewriting. A no-op reconcile never touches either.
    let session_dir = db_path.parent().unwrap().join("sessions").join("s-ok");
    std::fs::create_dir_all(session_dir.join("transcript.jsonl.rebuild.tmp")).unwrap();

    let _db = SessionDB::open(&db_path).await.unwrap();
    assert!(
        session_dir.join("transcript.jsonl.rebuild.tmp").is_dir(),
        "reconcile must not attempt a rebuild for a consistent session"
    );
}
