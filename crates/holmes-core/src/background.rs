//! Background subagent task registry (grok-build style backgrounded subagents).
//!
//! `spawn_subagent` with `run_in_background=true` registers a task here and detaches
//! the subagent onto a tokio task; when the subagent finishes, the task itself writes
//! the outcome back into the registry. `AgentRuntime::run_turn` drains finished,
//! not-yet-delivered tasks at iteration boundaries (the same safety point as steering
//! interjections) and injects them into the conversation, so results surface without
//! polling. `get_task_output` reads the same registry for explicit query/wait.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Utc};

pub type TaskId = String;

/// Persisted counterpart of a background task registration, handed to a
/// [`DurableTaskSink`] when the task starts (AGT-007).
#[derive(Debug, Clone)]
pub struct DurableTaskStart {
    pub task_id: TaskId,
    pub description: String,
    pub parent_session_id: Option<String>,
    /// Stable key derived from the caller's intent; a sink must treat a repeat
    /// start with the same key as the same task instead of executing twice.
    pub idempotency_key: Option<String>,
    /// Whether an automatic recovery pass may re-execute this task after a
    /// crash. Tasks with external side effects must be `false` so recovery
    /// marks them `manual_recovery_required` instead of re-running them.
    pub safe_to_retry: bool,
    /// Serialized spawn-time payload (e.g. the subagent args) persisted so a
    /// scheduler can re-execute the task after recovery; None when the task
    /// kind has no re-execution path.
    pub payload: Option<String>,
    /// Present only when this task is the durable execution adapter for a
    /// validated case-scoped Ledger Experiment.
    pub experiment: Option<crate::ledger::ExperimentAssignment>,
}

/// Durable write-back for background task state (AGT-007, P1-02). The in-memory
/// registry remains the hot path for draining completions into the turn; the
/// sink mirrors the same transitions into SQLite so a restart can recover (or
/// deterministically suspend) tasks whose runner died with the process.
///
/// Fencing: `task_started` returns the fencing token of the acquired attempt.
/// Every later call must present it; the store only accepts writes from the
/// attempt that currently owns the lease, so a superseded worker cannot
/// overwrite a newer attempt. Implementations live in `holmes-session` (core
/// cannot depend on session). Errors are strings because the registry/tool
/// boundary is `anyhow`-based and a sink failure must degrade the task, never
/// panic the turn.
#[async_trait]
pub trait DurableTaskSink: Send + Sync {
    /// Persist a newly-started task as Running under the caller's lease and
    /// return the attempt's fencing token. Called BEFORE the "task started"
    /// success is returned to the model: if this fails, the spawn fails closed
    /// instead of reporting success for a task no recovery pass could ever find.
    async fn task_started(&self, start: DurableTaskStart) -> Result<u64, String>;

    /// Persist the terminal outcome, fenced by the attempt token: a superseded
    /// attempt's write is rejected by the store. `Ok` → succeeded with the
    /// result payload, `Err` starting with "cancelled" → cancelled, any other
    /// `Err` → failed.
    async fn task_completed(
        &self,
        task_id: &str,
        fencing: u64,
        result: &Result<String, String>,
    ) -> Result<(), String>;

    /// Renew the runner's lease while the task is still running. `Ok(false)`
    /// means the lease was lost (reclaimed or superseded): the caller MUST
    /// cancel its worker immediately and never write to the sink again for
    /// this task. `Err` is transient (store hiccup) — retry on the next tick.
    async fn task_heartbeat(&self, task_id: &str, fencing: u64) -> Result<bool, String>;

    /// Record which child session the task's subagent produced (AGT-013): after a
    /// restart the parent can re-associate the task with the subagent's session
    /// (the checkpoint handle in the structured result). Default no-op for sinks
    /// without session tracking.
    async fn task_attached_session(
        &self,
        _task_id: &str,
        _fencing: u64,
        _child_session_id: &str,
    ) -> Result<(), String> {
        Ok(())
    }
}

/// A sink bound to the session the spawned tasks belong to (AGT-007). The
/// spawn tool records every background task under this parent so recovery can
/// attribute orphans to their session after a restart.
#[derive(Clone)]
pub struct DurableTaskBinding {
    pub sink: Arc<dyn DurableTaskSink>,
    pub parent_session_id: Option<String>,
}

/// A terminal durable task whose result has not been delivered into its parent
/// session's conversation yet (P1-02 durable result delivery).
#[derive(Debug, Clone)]
pub struct UndeliveredTaskResult {
    pub task_id: TaskId,
    pub description: String,
    /// Terminal state label: "succeeded" | "failed" | "cancelled".
    pub state: String,
    /// Result payload for succeeded tasks.
    pub result: Option<String>,
    /// Error for failed/cancelled tasks.
    pub error: Option<String>,
}

/// Outcome of an atomic result-delivery attempt (P1-02).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskDeliveryOutcome {
    /// The result event was appended and the task marked delivered in the same
    /// transaction — exactly-once at the database level.
    Appended,
    /// Already delivered earlier: idempotent no-op, present nothing again.
    AlreadyDelivered,
    /// The task exists but is not terminal — another attempt owns it; its
    /// result will arrive through that attempt's delivery.
    NotTerminal,
    /// No durable record exists for this task (registry-only): the caller
    /// falls back to plain in-memory delivery.
    UnknownTask,
}

#[derive(Debug, Clone)]
pub enum TaskStatus {
    Running,
    /// Ok carries the pretty-printed `SubAgentResult` JSON, Err the failure message.
    Completed(Result<String, String>),
}

#[derive(Debug, Clone)]
pub struct TaskState {
    pub description: String,
    pub status: TaskStatus,
    pub started_at: DateTime<Utc>,
    /// Whether the completion was already injected into the conversation by the
    /// runtime's drain. Delivered tasks stay in the registry so `get_task_output`
    /// can still return their full result afterwards.
    pub delivered: bool,
}

/// A finished task handed to the runtime for injection.
#[derive(Debug, Clone)]
pub struct FinishedTask {
    pub id: TaskId,
    pub description: String,
    pub result: Result<String, String>,
}

/// Shared registry of background subagent tasks. Cheap to clone — every clone points
/// at the same map, so the tool that spawns tasks, the tool that queries them, and the
/// runtime that drains them all observe the same state.
///
/// The `cancel` flag mirrors the runtime's cooperative-cancellation flag so a blocking
/// `get_task_output` wait can return early when the operator interrupts the turn
/// (`select!` on the flag is not possible through the sync registry, so the waiter
/// polls it on a short interval — see the tool for the trade-off note).
#[derive(Debug, Clone)]
pub struct BackgroundTasks {
    inner: Arc<Mutex<HashMap<TaskId, TaskState>>>,
    cancel: Arc<AtomicBool>,
}

impl Default for BackgroundTasks {
    fn default() -> Self {
        Self::new()
    }
}

impl BackgroundTasks {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Create a registry whose cancellation waits observe the given flag. Clones keep
    /// sharing this flag, so construct once at the surface (next to the turn's cancel
    /// flag) and clone it into the tools and the runtime context.
    pub fn with_cancel(cancel: Arc<AtomicBool>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            cancel,
        }
    }

    /// Replace the cancellation flag observed by `is_cancelled`. Only affects this
    /// clone's flag handle; other clones keep theirs (see `with_cancel`).
    pub fn set_cancel(&mut self, cancel: Arc<AtomicBool>) {
        self.cancel = cancel;
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Register a freshly-spawned background task; returns its generated id.
    pub fn register(&self, description: String) -> TaskId {
        let id = uuid::Uuid::new_v4().to_string();
        self.lock().insert(
            id.clone(),
            TaskState {
                description,
                status: TaskStatus::Running,
                started_at: Utc::now(),
                delivered: false,
            },
        );
        id
    }

    /// Record the outcome of a task (called by the detached task itself). Unknown ids
    /// are ignored — the registry never fails the finishing task.
    pub fn complete(&self, id: &str, result: Result<String, String>) {
        if let Some(state) = self.lock().get_mut(id) {
            state.status = TaskStatus::Completed(result);
        }
    }

    /// Take every finished task whose completion has not been delivered yet, marking
    /// each as delivered so the next drain does not inject it twice.
    pub fn take_finished_undelivered(&self) -> Vec<FinishedTask> {
        let mut finished = Vec::new();
        for (id, state) in self.lock().iter_mut() {
            if state.delivered {
                continue;
            }
            if let TaskStatus::Completed(result) = &state.status {
                state.delivered = true;
                finished.push(FinishedTask {
                    id: id.clone(),
                    description: state.description.clone(),
                    result: result.clone(),
                });
            }
        }
        finished
    }

    pub fn snapshot(&self, id: &str) -> Option<TaskState> {
        self.lock().get(id).cloned()
    }

    pub fn running_count(&self) -> usize {
        self.lock()
            .values()
            .filter(|state| matches!(state.status, TaskStatus::Running))
            .count()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<TaskId, TaskState>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_register_complete_take_once() {
        let tasks = BackgroundTasks::new();
        let id = tasks.register("scan ports".into());
        assert_eq!(tasks.running_count(), 1);
        assert!(tasks.take_finished_undelivered().is_empty());

        tasks.complete(&id, Ok("done".into()));
        assert_eq!(tasks.running_count(), 0);

        let finished = tasks.take_finished_undelivered();
        assert_eq!(finished.len(), 1);
        assert_eq!(finished[0].id, id);
        assert_eq!(finished[0].description, "scan ports");
        assert_eq!(finished[0].result, Ok("done".into()));

        // Second drain must not re-deliver; the result stays queryable.
        assert!(tasks.take_finished_undelivered().is_empty());
        let snapshot = tasks.snapshot(&id).expect("task still registered");
        assert!(snapshot.delivered);
        assert!(matches!(snapshot.status, TaskStatus::Completed(Ok(_))));
    }

    #[test]
    fn complete_unknown_id_is_ignored() {
        let tasks = BackgroundTasks::new();
        tasks.complete("nope", Err("boom".into()));
        assert!(tasks.take_finished_undelivered().is_empty());
    }

    #[test]
    fn cancel_flag_is_shared_across_clones() {
        let flag = Arc::new(AtomicBool::new(false));
        let tasks = BackgroundTasks::with_cancel(flag.clone());
        let clone = tasks.clone();
        assert!(!clone.is_cancelled());
        flag.store(true, Ordering::Relaxed);
        assert!(tasks.is_cancelled());
        assert!(clone.is_cancelled());
    }
}
