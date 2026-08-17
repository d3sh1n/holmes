//! Restart recovery for durable background tasks (AGT-007).
//!
//! When the process restarts, every task still marked `running` whose lease has
//! expired belongs to a runner that died with the previous process. Recovery is
//! deliberately deterministic and side-effect free:
//!
//! - safe-to-retry orphans move Running → Recovering → Retrying, making them
//!   eligible to be leased again without executing anything here;
//! - tasks with external side effects move to `manual_recovery_required` and
//!   are surfaced in the report so an operator can reconcile them — nothing
//!   re-executes them automatically;
//! - already-terminal tasks (Succeeded/Failed/Cancelled) are untouched, so a
//!   confirmed side effect is never repeated.
//!
//! Run once at startup, before any new work is leased.

use chrono::Utc;
use holmes_session::db::SessionError;
use holmes_session::task_store::{RecoveryResolution, TaskRecord, TaskStore};

/// What a restart-recovery pass found and did.
#[derive(Debug, Clone, Default)]
pub struct TaskRecoveryReport {
    /// Orphaned safe-to-retry tasks returned to `retrying` (Recovering resolved
    /// as requeue). Nothing has re-executed them; they are leasable again.
    pub requeued: Vec<TaskRecord>,
    /// Orphaned tasks with external side effects, suspended as
    /// `manual_recovery_required`. Operator action is required.
    pub manual_required: Vec<TaskRecord>,
}

impl TaskRecoveryReport {
    /// True when no orphaned tasks were found.
    pub fn is_clean(&self) -> bool {
        self.requeued.is_empty() && self.manual_required.is_empty()
    }
}

/// Recover tasks orphaned by a previous process (AGT-007).
///
/// The scan-and-disposition inside `recover_expired_leases` commits in a single
/// transaction, so a crash during recovery itself leaves the pre-recovery state
/// intact and the next startup simply runs it again.
pub async fn recover_durable_tasks(store: &TaskStore) -> Result<TaskRecoveryReport, SessionError> {
    let outcome = store.recover_expired_leases(Utc::now()).await?;

    let mut report = TaskRecoveryReport::default();

    for record in outcome.recovering {
        holmes_core::metrics::metrics().count("task.recovered");
        tracing::warn!(
            event = "TaskRecovered",
            task_id = %record.task_id,
            parent_session_id = ?record.parent_session_id,
            attempt = record.attempt,
            disposition = "requeue",
            "background task orphaned by process restart; requeueing (safe to retry)"
        );
        if store
            .resolve_recovering(&record.task_id, RecoveryResolution::Requeue)
            .await?
        {
            report.requeued.push(record);
        }
    }

    for record in outcome.manual_required {
        holmes_core::metrics::metrics().count("task.manual_recovery_required");
        tracing::warn!(
            event = "ManualRecoveryRequired",
            task_id = %record.task_id,
            parent_session_id = ?record.parent_session_id,
            last_error = ?record.last_error,
            "background task with external side effects orphaned by process restart; \
             suspended as manual_recovery_required"
        );
        report.manual_required.push(record);
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use holmes_session::db::SessionDB;
    use holmes_session::task_store::{NewTask, TaskStatus};

    async fn running_task(db: &SessionDB, task_id: &str, safe_to_retry: bool) {
        let store = db.task_store();
        store
            .enqueue(NewTask {
                task_id: task_id.into(),
                parent_session_id: None,
                kind: "subagent".into(),
                description: "recon".into(),
                idempotency_key: None,
                safe_to_retry,
                payload: None,
            })
            .await
            .unwrap();
        // A lease already in the past simulates the crashed process: the
        // previous owner is gone and its lease has expired.
        store
            .acquire_lease_with_duration(task_id, -1)
            .await
            .unwrap()
            .expect("task leasable");
    }

    #[tokio::test]
    async fn restart_requeues_safe_orphans_and_suspends_unsafe_ones() {
        let db = SessionDB::open(":memory:").await.unwrap();
        running_task(&db, "safe-task", true).await;
        running_task(&db, "unsafe-task", false).await;

        let report = recover_durable_tasks(&db.task_store()).await.unwrap();

        assert_eq!(report.requeued.len(), 1);
        assert_eq!(report.requeued[0].task_id, "safe-task");
        assert_eq!(report.manual_required.len(), 1);
        assert_eq!(report.manual_required[0].task_id, "unsafe-task");

        let store = db.task_store();
        let safe = store.get("safe-task").await.unwrap().unwrap();
        assert_eq!(safe.state, TaskStatus::Retrying);
        assert!(safe.lease_owner.is_none());
        let unsafe_ = store.get("unsafe-task").await.unwrap().unwrap();
        assert_eq!(unsafe_.state, TaskStatus::ManualRecoveryRequired);
        // A suspended task must never be leased again automatically.
        assert!(store.acquire_lease("unsafe-task").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn recovery_ignores_live_and_terminal_tasks() {
        let db = SessionDB::open(":memory:").await.unwrap();
        let store = db.task_store();
        // Live task: lease far in the future.
        store
            .enqueue(NewTask {
                task_id: "live".into(),
                parent_session_id: None,
                kind: "subagent".into(),
                description: "running".into(),
                idempotency_key: None,
                safe_to_retry: true,
                payload: None,
            })
            .await
            .unwrap();
        store.acquire_lease("live").await.unwrap().unwrap();
        // Terminal task: lease expired but already succeeded — a confirmed side
        // effect recovery must not touch.
        running_task(&db, "done", true).await;
        let fencing = store.get("done").await.unwrap().unwrap().attempt;
        store.complete("done", fencing, "result").await.unwrap();

        let report = recover_durable_tasks(&store).await.unwrap();
        assert!(report.is_clean());
        assert_eq!(
            store.get("live").await.unwrap().unwrap().state,
            TaskStatus::Running
        );
        assert_eq!(
            store.get("done").await.unwrap().unwrap().state,
            TaskStatus::Succeeded
        );
    }

    #[tokio::test]
    async fn recovery_is_idempotent_across_restarts() {
        let db = SessionDB::open(":memory:").await.unwrap();
        running_task(&db, "orphan", true).await;

        let first = recover_durable_tasks(&db.task_store()).await.unwrap();
        assert_eq!(first.requeued.len(), 1);
        // A second pass (recovery crashed between passes, or operator re-runs)
        // finds nothing new: the orphan was already discharged.
        let second = recover_durable_tasks(&db.task_store()).await.unwrap();
        assert!(second.is_clean());
    }

    #[tokio::test]
    async fn clean_startup_recovers_nothing() {
        let db = SessionDB::open(":memory:").await.unwrap();
        let report = recover_durable_tasks(&db.task_store()).await.unwrap();
        assert!(report.is_clean());
    }
}
