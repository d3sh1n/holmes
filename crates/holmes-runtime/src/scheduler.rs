//! Resident durable task scheduler (P1-02).
//!
//! Startup recovery (`recovery::recover_durable_tasks`) dispositions orphans
//! once; this scheduler is what makes durable tasks actually converge while
//! the process lives. Every pass:
//!
//! 1. **dead-owner reaper** — leases whose owner pid is verifiably dead are
//!    expired immediately (boot/process identity), without waiting out the
//!    lease deadline;
//! 2. **expired-lease recovery** — the same disposition as startup recovery
//!    (safe-to-retry → Retrying, side-effecting → manual_recovery_required),
//!    so a task whose owner crashed right after a heartbeat still converges;
//! 3. **lease & execute** — queued/retrying tasks with a registered executor
//!    are claimed atomically via the conditional lease UPDATE and re-executed
//!    on a detached worker with the same heartbeat/fencing discipline as a
//!    freshly spawned task: the fencing token (`attempt`) gates every write,
//!    and a heartbeat that reports a lost lease cancels the worker and
//!    forbids its write-back.
//!
//! Executors are registered per task `kind`. Kinds without an executor stay
//! queued/retrying (visible to the operator) instead of being executed by a
//! wrong or nonexistent path.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use holmes_session::db::SessionError;
use holmes_session::task_store::{TaskRecord, TaskStore};
use tokio_util::sync::CancellationToken;

/// Default scan cadence for `spawn`. Well below the 300s lease so an expired
/// or dead-owner lease is reclaimed promptly.
pub const DEFAULT_SCAN_INTERVAL: Duration = Duration::from_secs(30);
/// Heartbeat cadence for scheduler-run workers (matches the subagent tool's).
const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);
/// Attempts after which a re-executed task's failure becomes permanent:
/// recovery requeues, the scheduler re-runs, and if it keeps failing the task
/// fails terminally instead of looping forever.
pub const DEFAULT_MAX_ATTEMPTS: u32 = 3;
/// Grace given to a worker to wind down after its cancellation token fires
/// (lost lease or scheduler shutdown) before its future is dropped.
const WORKER_CLEANUP_GRACE: Duration = Duration::from_secs(2);

/// A task leased by the scheduler for re-execution.
#[derive(Debug, Clone)]
pub struct LeasedTask {
    pub task_id: String,
    /// Fencing token of this attempt (the task's `attempt` counter at lease
    /// time); every durable write the worker makes must present it.
    pub fencing: u32,
    pub kind: String,
    pub description: String,
    /// Spawn-time payload recorded at enqueue (e.g. serialized subagent args).
    pub payload: Option<String>,
    pub parent_session_id: Option<String>,
}

impl From<&TaskRecord> for LeasedTask {
    fn from(record: &TaskRecord) -> Self {
        Self {
            task_id: record.task_id.clone(),
            fencing: record.attempt,
            kind: record.kind.clone(),
            description: record.description.clone(),
            payload: record.payload.clone(),
            parent_session_id: record.parent_session_id.clone(),
        }
    }
}

/// Re-execution backend for one task kind. Implementations run the leased
/// task to completion (Ok → succeeded with payload, Err → failed) and must
/// observe `cancel`: when it fires (lost lease or scheduler shutdown) the
/// worker is expected to wind down within the cleanup grace.
#[async_trait]
pub trait DurableTaskExecutor: Send + Sync {
    async fn execute(&self, task: LeasedTask, cancel: CancellationToken) -> Result<String, String>;
}

/// What one scheduler pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SchedulerPass {
    /// Leases expired early because their owner pid is dead.
    pub dead_owner_leases_expired: usize,
    /// Orphaned safe-to-retry tasks requeued this pass.
    pub requeued: usize,
    /// Orphaned side-effecting tasks suspended for manual recovery.
    pub manual_required: usize,
    /// Tasks leased and handed to an executor this pass.
    pub leased: usize,
    /// Leasable tasks skipped because no executor is registered for their kind.
    pub skipped_no_executor: usize,
}

pub struct DurableTaskScheduler {
    store: TaskStore,
    executors: HashMap<String, Arc<dyn DurableTaskExecutor>>,
    heartbeat_interval: Duration,
    max_attempts: u32,
    shutdown: CancellationToken,
}

impl DurableTaskScheduler {
    pub fn new(store: TaskStore) -> Self {
        Self {
            store,
            executors: HashMap::new(),
            heartbeat_interval: DEFAULT_HEARTBEAT_INTERVAL,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            shutdown: CancellationToken::new(),
        }
    }

    pub fn with_executor(mut self, kind: &str, executor: Arc<dyn DurableTaskExecutor>) -> Self {
        self.executors.insert(kind.to_string(), executor);
        self
    }

    pub fn with_heartbeat_interval(mut self, interval: Duration) -> Self {
        self.heartbeat_interval = interval;
        self
    }

    pub fn with_max_attempts(mut self, max_attempts: u32) -> Self {
        self.max_attempts = max_attempts;
        self
    }

    /// Shutdown token shared by the scan loop and every in-flight worker.
    pub fn with_shutdown(mut self, shutdown: CancellationToken) -> Self {
        self.shutdown = shutdown;
        self
    }

    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    /// One scheduler pass: reap dead-owner leases, recover expired ones, then
    /// lease and execute everything leasable. Errors fail the pass (the next
    /// tick retries); individual task failures are recorded on the task.
    pub async fn run_once(self: &Arc<Self>) -> Result<SchedulerPass, SessionError> {
        let dead_owner_leases_expired = self.store.expire_leases_of_dead_owners(&pid_alive).await?;

        let recovery = crate::recovery::recover_durable_tasks(&self.store).await?;
        let mut pass = SchedulerPass {
            dead_owner_leases_expired,
            requeued: recovery.requeued.len(),
            manual_required: recovery.manual_required.len(),
            ..Default::default()
        };

        for record in self.store.list_leasable().await? {
            if self.shutdown.is_cancelled() {
                break;
            }
            let Some(executor) = self.executors.get(record.kind.as_str()).cloned() else {
                pass.skipped_no_executor += 1;
                tracing::warn!(
                    event = "DurableTaskNoExecutor",
                    task_id = %record.task_id,
                    kind = %record.kind,
                    "leasable durable task has no registered executor; leaving it queued"
                );
                continue;
            };
            // The conditional lease UPDATE is the atomic claim: a racing
            // acquirer (another scheduler pass, a fresh spawn) flips the state
            // first and this acquire returns None.
            let Some(leased) = self.store.acquire_lease(&record.task_id).await? else {
                continue;
            };
            pass.leased += 1;
            tracing::info!(
                event = "DurableTaskLeased",
                task_id = %leased.task_id,
                kind = %leased.kind,
                attempt = leased.attempt,
                "scheduler leased durable task for execution"
            );
            holmes_core::metrics::metrics().count("task.leased");
            self.spawn_worker(&leased, executor);
        }

        Ok(pass)
    }

    /// Detached worker for one leased task: run the executor against the
    /// heartbeat loop. Every durable write carries the fencing token; a
    /// heartbeat reporting a lost lease cancels the worker and suppresses its
    /// write-back, so a superseded attempt never touches the task again.
    fn spawn_worker(self: &Arc<Self>, record: &TaskRecord, executor: Arc<dyn DurableTaskExecutor>) {
        let store = self.store.clone();
        let heartbeat_interval = self.heartbeat_interval;
        let max_attempts = self.max_attempts;
        let shutdown = self.shutdown.clone();
        let task = LeasedTask::from(record);
        tokio::spawn(async move {
            let task_id = task.task_id.clone();
            let fencing = task.fencing;
            let cancel = CancellationToken::new();
            let run = executor.execute(task.clone(), cancel.clone());
            tokio::pin!(run);
            let mut heartbeat = tokio::time::interval(heartbeat_interval);
            heartbeat.tick().await; // consume the immediate first tick
            let mut lost_lease = false;
            let result = loop {
                tokio::select! {
                    result = &mut run => break Some(result),
                    _ = shutdown.cancelled() => {
                        cancel.cancel();
                        break None;
                    }
                    _ = heartbeat.tick() => {
                        match store.heartbeat(&task_id, fencing).await {
                            Ok(true) => {}
                            Ok(false) => {
                                lost_lease = true;
                                tracing::warn!(
                                    event = "DurableLeaseLost",
                                    task_id = %task_id,
                                    attempt = fencing,
                                    "scheduler worker lost its lease; stopping without write-back"
                                );
                                holmes_core::metrics::metrics().count("task.lease_lost");
                                cancel.cancel();
                                break None;
                            }
                            Err(error) => {
                                tracing::warn!(task_id = %task_id, error = %error,
                                    "scheduler worker heartbeat failed");
                            }
                        }
                    }
                }
            };
            if result.is_none() {
                // Lost lease or shutdown: give the worker a bounded grace to
                // clean up its own resources, then drop it. No write-back:
                // the lease owner (a newer attempt, or a later recovery pass)
                // owns every later transition.
                let _ = tokio::time::timeout(WORKER_CLEANUP_GRACE, &mut run).await;
                if lost_lease {
                    return;
                }
                // Shutdown: leave the lease to expire; the next process or a
                // later pass recovers it.
                return;
            }
            match result {
                Some(Ok(output)) => match store.complete(&task_id, fencing, &output).await {
                    Ok(true) => {
                        holmes_core::metrics::metrics().count("task.reexecuted_ok");
                        tracing::info!(
                            event = "DurableTaskReExecuted",
                            task_id = %task_id,
                            attempt = fencing,
                            outcome = "succeeded",
                            "scheduler-run task completed"
                        );
                    }
                    Ok(false) => tracing::warn!(task_id = %task_id, attempt = fencing,
                            "scheduler worker completion fenced out (superseded)"),
                    Err(error) => tracing::error!(task_id = %task_id, error = %error,
                            "scheduler worker completion write-back failed"),
                },
                Some(Err(error)) => {
                    // Retryable until the attempt budget is spent; recovery
                    // requeues retrying tasks on the next pass.
                    let retryable = fencing < max_attempts;
                    match store.fail(&task_id, fencing, &error, retryable).await {
                        Ok(true) => {
                            holmes_core::metrics::metrics().count("task.reexecuted_failed");
                            tracing::warn!(
                                event = "DurableTaskReExecuted",
                                task_id = %task_id,
                                attempt = fencing,
                                outcome = if retryable { "retrying" } else { "failed" },
                                error = %error,
                                "scheduler-run task failed"
                            );
                        }
                        Ok(false) => tracing::warn!(task_id = %task_id, attempt = fencing,
                            "scheduler worker failure fenced out (superseded)"),
                        Err(write_error) => {
                            tracing::error!(task_id = %task_id, error = %write_error,
                            "scheduler worker failure write-back failed")
                        }
                    }
                }
                None => unreachable!("None result returned above"),
            }
        });
    }

    /// Resident scan loop: run a pass every `interval` until shutdown.
    pub fn spawn(self: Arc<Self>, interval: Duration) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.tick().await; // startup recovery already ran; first scan after one interval
            loop {
                tokio::select! {
                    _ = self.shutdown.cancelled() => break,
                    _ = ticker.tick() => {
                        if let Err(error) = self.run_once().await {
                            tracing::warn!(
                                event = "DurableSchedulerPassFailed",
                                error = %error,
                                "durable task scheduler pass failed; retrying next tick"
                            );
                        }
                    }
                }
            }
        })
    }
}

/// Boot/process identity (P1-02): whether the process owning a lease is still
/// alive. Unix: `kill(pid, 0)` — ESRCH means gone, EPERM means alive (foreign
/// user). Pid reuse can delay a reclaim until the lease expires, never prevent
/// it. Non-Unix: conservative `true`, so reclaim always waits for lease expiry.
#[cfg(unix)]
fn pid_alive(pid: i32) -> bool {
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn pid_alive(_pid: i32) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use holmes_session::db::SessionDB;
    use holmes_session::task_store::{NewTask, TaskStatus};
    use std::sync::Mutex;

    fn new_task(task_id: &str, kind: &str, safe_to_retry: bool, payload: Option<&str>) -> NewTask {
        NewTask {
            task_id: task_id.into(),
            parent_session_id: None,
            kind: kind.into(),
            description: "scheduled task".into(),
            idempotency_key: None,
            safe_to_retry,
            payload: payload.map(str::to_string),
        }
    }

    /// Executor that records what it ran and how it should finish.
    struct StubExecutor {
        runs: Mutex<Vec<(String, u32, Option<String>)>>,
        fail: bool,
        block: bool,
    }

    #[async_trait]
    impl DurableTaskExecutor for StubExecutor {
        async fn execute(
            &self,
            task: LeasedTask,
            cancel: CancellationToken,
        ) -> Result<String, String> {
            self.runs.lock().unwrap().push((
                task.task_id.clone(),
                task.fencing,
                task.payload.clone(),
            ));
            if self.block {
                cancel.cancelled().await;
                return Err("cancelled: executor stopped".into());
            }
            if self.fail {
                return Err("executor exploded".into());
            }
            Ok(format!("done:{}", task.task_id))
        }
    }

    fn scheduler(db: &SessionDB, executor: Arc<StubExecutor>) -> Arc<DurableTaskScheduler> {
        Arc::new(
            DurableTaskScheduler::new(db.task_store())
                .with_executor("stub", executor)
                .with_heartbeat_interval(Duration::from_millis(20)),
        )
    }

    async fn wait_for_state(store: &TaskStore, task_id: &str, state: TaskStatus) -> TaskRecord {
        for _ in 0..200 {
            if let Some(record) = store.get(task_id).await.unwrap() {
                if record.state == state {
                    return record;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("task {task_id} never reached {state:?}");
    }

    #[tokio::test]
    async fn scheduler_leases_and_executes_queued_task() {
        let db = SessionDB::open(":memory:").await.unwrap();
        let store = db.task_store();
        store
            .enqueue(new_task("t1", "stub", true, Some("payload-1")))
            .await
            .unwrap();
        let executor = Arc::new(StubExecutor {
            runs: Mutex::new(vec![]),
            fail: false,
            block: false,
        });
        let scheduler = scheduler(&db, executor.clone());

        let pass = scheduler.run_once().await.unwrap();
        assert_eq!(pass.leased, 1);

        let record = wait_for_state(&store, "t1", TaskStatus::Succeeded).await;
        assert_eq!(record.result.as_deref(), Some("done:t1"));
        // The executor saw the fencing token and the persisted payload.
        let runs = executor.runs.lock().unwrap();
        assert_eq!(
            runs.as_slice(),
            &[("t1".to_string(), 1, Some("payload-1".to_string()))]
        );
    }

    #[tokio::test]
    async fn scheduler_retries_failed_task_until_attempt_budget() {
        let db = SessionDB::open(":memory:").await.unwrap();
        let store = db.task_store();
        store
            .enqueue(new_task("t1", "stub", true, None))
            .await
            .unwrap();
        let executor = Arc::new(StubExecutor {
            runs: Mutex::new(vec![]),
            fail: true,
            block: false,
        });
        let scheduler = Arc::new(
            DurableTaskScheduler::new(db.task_store())
                .with_executor("stub", executor.clone())
                .with_heartbeat_interval(Duration::from_millis(20))
                .with_max_attempts(2),
        );

        scheduler.run_once().await.unwrap();
        // Attempt 1 fails retryable → back to Retrying.
        let record = wait_for_state(&store, "t1", TaskStatus::Retrying).await;
        assert_eq!(record.attempt, 1);
        // Attempt 2 exhausts the budget → terminal Failed.
        scheduler.run_once().await.unwrap();
        let record = wait_for_state(&store, "t1", TaskStatus::Failed).await;
        assert_eq!(record.attempt, 2);
        assert!(record
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("executor exploded"));
        assert_eq!(executor.runs.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn scheduler_recovers_expired_orphan_and_reexecutes_it() {
        // Simulates the full crash loop: first process leased and died; the
        // scheduler (not just startup recovery) reaps and re-runs the task.
        let db = SessionDB::open(":memory:").await.unwrap();
        let store = db.task_store();
        store
            .enqueue(new_task("t1", "stub", true, None))
            .await
            .unwrap();
        store
            .acquire_lease_with_duration("t1", -1)
            .await
            .unwrap()
            .expect("leasable");
        let executor = Arc::new(StubExecutor {
            runs: Mutex::new(vec![]),
            fail: false,
            block: false,
        });
        let scheduler = scheduler(&db, executor.clone());

        let pass = scheduler.run_once().await.unwrap();
        assert_eq!(pass.requeued, 1, "expired orphan requeued by the scheduler");
        assert_eq!(pass.leased, 1, "and leased in the same pass");
        let record = wait_for_state(&store, "t1", TaskStatus::Succeeded).await;
        assert_eq!(record.attempt, 2, "re-execution is a new fenced attempt");
        assert_eq!(executor.runs.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn scheduler_worker_stops_without_writeback_when_lease_lost() {
        let db = SessionDB::open(":memory:").await.unwrap();
        let store = db.task_store();
        store
            .enqueue(new_task("t1", "stub", true, None))
            .await
            .unwrap();
        let executor = Arc::new(StubExecutor {
            runs: Mutex::new(vec![]),
            fail: false,
            block: true,
        });
        let scheduler = scheduler(&db, executor.clone());

        scheduler.run_once().await.unwrap();
        // Wait for the worker to start (attempt 1 running under the scheduler).
        wait_for_state(&store, "t1", TaskStatus::Running).await;

        // Another process reclaims the task: recover + resolve + re-lease
        // under a different owner bumps the fencing token.
        let outcome = store
            .recover_expired_leases(chrono::Utc::now() + chrono::Duration::seconds(600))
            .await
            .unwrap();
        assert_eq!(outcome.recovering.len(), 1);
        store
            .resolve_recovering(
                "t1",
                holmes_session::task_store::RecoveryResolution::Requeue,
            )
            .await
            .unwrap();
        let second = store
            .acquire_lease("t1")
            .await
            .unwrap()
            .expect("re-leasable");
        assert_eq!(second.attempt, 2);

        // The old worker's next heartbeat reports the loss; it must stop and
        // its late completion must never land: the task stays Running under
        // attempt 2.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let record = store.get("t1").await.unwrap().unwrap();
        assert_eq!(
            record.state,
            TaskStatus::Running,
            "old attempt must not write"
        );
        assert_eq!(record.attempt, 2);
        assert!(record.result.is_none());
    }

    #[tokio::test]
    async fn scheduler_skips_kinds_without_executor() {
        let db = SessionDB::open(":memory:").await.unwrap();
        let store = db.task_store();
        store
            .enqueue(new_task("t1", "unknown-kind", true, None))
            .await
            .unwrap();
        let executor = Arc::new(StubExecutor {
            runs: Mutex::new(vec![]),
            fail: false,
            block: false,
        });
        let scheduler = scheduler(&db, executor);

        let pass = scheduler.run_once().await.unwrap();
        assert_eq!(pass.skipped_no_executor, 1);
        assert_eq!(pass.leased, 0);
        assert_eq!(
            store.get("t1").await.unwrap().unwrap().state,
            TaskStatus::Queued
        );
    }

    #[tokio::test]
    async fn dead_owner_leases_are_expired_immediately() {
        let db = SessionDB::open(":memory:").await.unwrap();
        let store = db.task_store();
        store
            .enqueue(new_task("t1", "stub", true, None))
            .await
            .unwrap();
        // Live lease far in the future, but the owner pid check says dead.
        store.acquire_lease("t1").await.unwrap().unwrap();

        let expired = store
            .expire_leases_of_dead_owners(&|_| false)
            .await
            .unwrap();
        assert_eq!(expired, 1);
        let outcome = store
            .recover_expired_leases(chrono::Utc::now())
            .await
            .unwrap();
        assert_eq!(
            outcome.recovering.len(),
            1,
            "dead owner converges without waiting out the lease"
        );

        // A live owner is untouched.
        store
            .resolve_recovering(
                "t1",
                holmes_session::task_store::RecoveryResolution::Requeue,
            )
            .await
            .unwrap();
        store.acquire_lease("t1").await.unwrap().unwrap();
        let expired = store.expire_leases_of_dead_owners(&|_| true).await.unwrap();
        assert_eq!(expired, 0);
        assert_eq!(
            store.get("t1").await.unwrap().unwrap().state,
            TaskStatus::Running
        );
    }
}
