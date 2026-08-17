//! Durable background task store (AGT-007, P1-02).
//!
//! SQLite is the authoritative record for background task state. The in-memory
//! registry in `holmes-core::background` stays the hot path for delivering
//! completions inside a turn; every transition is mirrored here so a process
//! restart can tell a live task from an orphaned one via its lease and recover
//! (or deterministically suspend) the orphans.
//!
//! State machine:
//!
//! ```text
//! Queued --acquire_lease--> Running --complete--> Succeeded
//!                          Running --fail(retryable)--> Retrying --acquire_lease--> Running
//!                          Running --fail(permanent)--> Failed
//!                          Queued/Running/Retrying --cancel--> Cancelled
//! Running (lease expired, detected after restart):
//!     safe_to_retry     --> Recovering --resolve--> Retrying | Failed
//!     !safe_to_retry    --> ManualRecoveryRequired   (terminal: never auto re-executed)
//! ```
//!
//! Fencing (P1-02): every lease acquisition increments `attempt`, which doubles
//! as the fencing token for that attempt. Heartbeat, checkpoint, child-session
//! and terminal writes must match `(task_id, lease_owner, attempt)`, so a worker
//! whose lease was reclaimed can no longer write — a superseded attempt's late
//! completion loses the conditional UPDATE instead of overwriting the new
//! attempt's state.
//!
//! Boot/process identity (P1-02): `lease_owner` is `pid-<pid>-<uuid>`. A
//! periodic reaper (`holmes_runtime::scheduler`) expires leases whose owner pid
//! is verifiably dead without waiting for the lease deadline; anything else
//! converges when the lease expires. A live-but-reused pid can delay (never
//! prevent) reclaim until expiry.
//!
//! All multi-statement writes run in a single transaction and only BUSY/LOCKED
//! contention is retried (see `write_contention`).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use holmes_core::background::{DurableTaskSink, DurableTaskStart};
use holmes_core::ledger::{
    apply_ledger_event, AggregateKind, CaseId, CaseLedgerEvent, ExperimentAssignment, ExperimentId,
    ExperimentStatus, LedgerEvent, UnstoredLedgerEvent, LEDGER_EVENT_SCHEMA_VERSION,
};
use holmes_core::subagent::{verify_agent_task_result, AgentTaskStatus, VerifiedAgentTaskResult};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::db::SessionError;
use crate::write_contention::WriteContention;

const TASK_COLUMNS: &str = "task_id, parent_session_id, child_session_id, kind, description, \
    state, lease_owner, lease_expires_at, attempt, idempotency_key, checkpoint, result, \
    last_error, safe_to_retry, delivered, created_at, updated_at, payload, case_id, \
    experiment_id, lease_duration_ms, max_concurrent_per_case";

enum ExperimentTerminalInput {
    Completed(String),
    Failed(String),
    Cancelled(String),
}

/// Default lease a runner holds before it is considered dead: long enough to
/// survive slow LLM calls between heartbeats, short enough that a crashed
/// process is detected on the next startup without manual inspection.
pub const DEFAULT_LEASE_SECS: i64 = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Queued,
    Running,
    Succeeded,
    Retrying,
    Failed,
    Cancelled,
    Recovering,
    ManualRecoveryRequired,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskStatus::Queued => "queued",
            TaskStatus::Running => "running",
            TaskStatus::Succeeded => "succeeded",
            TaskStatus::Retrying => "retrying",
            TaskStatus::Failed => "failed",
            TaskStatus::Cancelled => "cancelled",
            TaskStatus::Recovering => "recovering",
            TaskStatus::ManualRecoveryRequired => "manual_recovery_required",
        }
    }

    fn from_str(s: &str) -> Result<Self, SessionError> {
        Ok(match s {
            "queued" => TaskStatus::Queued,
            "running" => TaskStatus::Running,
            "succeeded" => TaskStatus::Succeeded,
            "retrying" => TaskStatus::Retrying,
            "failed" => TaskStatus::Failed,
            "cancelled" => TaskStatus::Cancelled,
            "recovering" => TaskStatus::Recovering,
            "manual_recovery_required" => TaskStatus::ManualRecoveryRequired,
            other => {
                return Err(SessionError::Other(format!(
                    "unknown task state '{other}' in tasks table"
                )))
            }
        })
    }
}

#[derive(Debug, Clone)]
pub struct TaskRecord {
    pub task_id: String,
    pub parent_session_id: Option<String>,
    pub child_session_id: Option<String>,
    pub kind: String,
    pub description: String,
    pub state: TaskStatus,
    pub lease_owner: Option<String>,
    pub lease_expires_at: Option<DateTime<Utc>>,
    /// Monotonic per-task counter incremented by every lease acquisition;
    /// doubles as the fencing token an attempt must present to write.
    pub attempt: u32,
    pub idempotency_key: Option<String>,
    pub checkpoint: Option<String>,
    pub result: Option<String>,
    pub last_error: Option<String>,
    /// Spawn-time payload (e.g. serialized subagent args) a scheduler needs to
    /// re-execute the task; NULL when the kind has no registered executor.
    pub payload: Option<String>,
    /// Case/Experiment are both present only for delegated Ledger work.
    pub case_id: Option<String>,
    pub experiment_id: Option<String>,
    pub lease_duration_ms: u64,
    pub max_concurrent_per_case: usize,
    pub safe_to_retry: bool,
    pub delivered: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewTask {
    pub task_id: String,
    pub parent_session_id: Option<String>,
    pub kind: String,
    pub description: String,
    pub idempotency_key: Option<String>,
    pub safe_to_retry: bool,
    pub payload: Option<String>,
}

fn append_ledger_events_in_transaction(
    tx: &Transaction<'_>,
    case_id: &CaseId,
    command_id: &str,
    actor_session_id: &str,
    proposed: Vec<UnstoredLedgerEvent>,
) -> Result<u64, SessionError> {
    let existing = crate::ledger_store::load_events_from_connection(tx, case_id, None)
        .map_err(|error| SessionError::Other(error.to_string()))?;
    let mut snapshot = crate::ledger_store::project_events(case_id, &existing)
        .map_err(|error| SessionError::Other(error.to_string()))?;
    let expected_version = snapshot.version;
    let first_seq = expected_version + 1;
    let payload = serde_json::to_string(&proposed)?;
    let payload_hash = holmes_core::content_hash(&payload);
    let now = Utc::now();

    let mut stored = Vec::with_capacity(proposed.len());
    for (offset, event) in proposed.into_iter().enumerate() {
        let envelope = CaseLedgerEvent {
            case_id: case_id.clone(),
            seq: first_seq + offset as u64,
            event_id: event.event_id,
            aggregate_kind: event.aggregate_kind,
            aggregate_id: event.aggregate_id,
            aggregate_revision: event.aggregate_revision,
            actor_session_id: actor_session_id.to_owned(),
            event: event.event,
            created_at: event.created_at,
        };
        apply_ledger_event(&mut snapshot, &envelope)
            .map_err(|error| SessionError::Other(error.to_string()))?;
        stored.push(envelope);
    }

    tx.execute(
        "INSERT INTO case_ledger_commands
         (case_id, command_id, payload_hash, expected_version, first_seq, event_count, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            case_id.as_str(),
            command_id,
            payload_hash,
            expected_version as i64,
            first_seq as i64,
            stored.len() as i64,
            now.to_rfc3339(),
        ],
    )?;
    for event in &stored {
        tx.execute(
            "INSERT INTO case_ledger_events
             (case_id, seq, event_id, command_id, aggregate_kind, aggregate_id,
              aggregate_revision, actor_session_id, event_type, event_data, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                case_id.as_str(),
                event.seq as i64,
                event.event_id,
                command_id,
                crate::ledger_store::aggregate_kind_str(&event.aggregate_kind),
                event.aggregate_id,
                event.aggregate_revision as i64,
                event.actor_session_id,
                crate::ledger_store::ledger_event_type(&event.event),
                serde_json::to_string(&event.event)?,
                event.created_at.to_rfc3339(),
            ],
        )?;
    }
    tx.execute(
        "UPDATE cases SET ledger_version = ?2, updated_at = ?3 WHERE case_id = ?1",
        params![case_id.as_str(), snapshot.version as i64, now.to_rfc3339()],
    )?;
    Ok(snapshot.version)
}

/// How a task caught in `Recovering` after a restart is discharged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryResolution {
    /// Back to `Retrying`: eligible to be leased and executed again.
    Requeue,
    /// Terminal failure with the given reason recorded in `last_error`.
    Fail(String),
}

/// Outcome of `recover_expired_leases`: what changed and why.
#[derive(Debug, Clone, Default)]
pub struct LeaseRecoveryOutcome {
    /// Tasks moved Running → Recovering (safe to retry).
    pub recovering: Vec<TaskRecord>,
    /// Tasks moved Running → ManualRecoveryRequired (unsafe to re-execute).
    pub manual_required: Vec<TaskRecord>,
}

/// Shareable handle over the tasks table. Cheap to clone; every clone shares
/// the session database's connection and contention policy.
#[derive(Clone)]
pub struct TaskStore {
    conn: Arc<Mutex<Connection>>,
    write_contention: WriteContention,
    /// Identity of this process as a lease holder, stable for the store's
    /// lifetime: `pid-<pid>-<uuid>`. A lease held by any other owner (or an
    /// expired one) means the runner is gone.
    owner_id: String,
}

impl TaskStore {
    pub(crate) fn new(conn: Arc<Mutex<Connection>>, write_contention: WriteContention) -> Self {
        Self {
            conn,
            write_contention,
            owner_id: format!("pid-{}-{}", std::process::id(), uuid::Uuid::new_v4()),
        }
    }

    pub fn owner_id(&self) -> &str {
        &self.owner_id
    }

    /// Enqueue a new task. Idempotent on `idempotency_key`: a single UPSERT
    /// resolves the concurrent-enqueue race inside SQLite — the loser of an
    /// idempotency-key conflict gets the winner's existing row back via
    /// RETURNING, so every caller observes the same task id and exactly one
    /// row (and one downstream execution) exists per key.
    pub async fn enqueue(&self, task: NewTask) -> Result<TaskRecord, SessionError> {
        let task_for_retry = task.clone();
        self.write_contention
            .with_db_retry(|| {
                let task = task_for_retry.clone();
                async move {
                    let conn = self.conn.lock().await;
                    let now = Utc::now().to_rfc3339();
                    // ON CONFLICT ... DO UPDATE on the key itself is a no-op
                    // rewrite whose only job is to make RETURNING yield the
                    // pre-existing row (DO NOTHING would return zero rows).
                    conn.query_row(
                        "INSERT INTO tasks
                         (task_id, parent_session_id, child_session_id, kind, description,
                          state, lease_owner, lease_expires_at, attempt, idempotency_key,
                          checkpoint, result, last_error, safe_to_retry, delivered,
                          created_at, updated_at, payload)
                         VALUES (?1, ?2, NULL, ?3, ?4, 'queued', NULL, NULL, 0, ?5,
                                 NULL, NULL, NULL, ?6, 0, ?7, ?7, ?8)
                         ON CONFLICT(idempotency_key) DO UPDATE SET
                             idempotency_key = excluded.idempotency_key
                         RETURNING task_id, parent_session_id, child_session_id, kind,
                                   description, state, lease_owner, lease_expires_at, attempt,
                                   idempotency_key, checkpoint, result, last_error,
                                   safe_to_retry, delivered, created_at, updated_at, payload,
                                   case_id, experiment_id, lease_duration_ms,
                                   max_concurrent_per_case",
                        params![
                            task.task_id,
                            task.parent_session_id,
                            task.kind,
                            task.description,
                            task.idempotency_key,
                            task.safe_to_retry as i64,
                            now,
                            task.payload,
                        ],
                        row_to_record,
                    )
                }
            })
            .await
            .map_err(SessionError::from)
    }

    async fn enqueue_experiment_task(
        &self,
        start: &DurableTaskStart,
        assignment: &ExperimentAssignment,
    ) -> Result<TaskRecord, SessionError> {
        let parent_session_id = start.parent_session_id.clone().ok_or_else(|| {
            SessionError::Other("delegated Experiment task requires a parent session".into())
        })?;
        let start = start.clone();
        let assignment = assignment.clone();
        let nested = self
            .write_contention
            .with_db_retry(|| {
                let start = start.clone();
                let assignment = assignment.clone();
                let parent_session_id = parent_session_id.clone();
                async move {
                    let mut conn = self.conn.lock().await;
                    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                    let session_case = tx
                        .query_row(
                            "SELECT case_id FROM sessions WHERE id = ?1",
                            params![parent_session_id],
                            |row| row.get::<_, String>(0),
                        )
                        .optional()?;
                    if session_case.as_deref() != Some(assignment.case_id.as_str()) {
                        return Ok::<_, rusqlite::Error>(Err(SessionError::Other(
                            "delegated Experiment does not belong to the parent session case"
                                .into(),
                        )));
                    }

                    let existing_events =
                        match crate::ledger_store::load_events_from_connection(
                            &tx,
                            &assignment.case_id,
                            None,
                        ) {
                            Ok(events) => events,
                            Err(error) => {
                                return Ok(Err(SessionError::Other(error.to_string())))
                            }
                        };
                    let snapshot = match crate::ledger_store::project_events(
                        &assignment.case_id,
                        &existing_events,
                    ) {
                        Ok(snapshot) => snapshot,
                        Err(error) => return Ok(Err(SessionError::Other(error.to_string()))),
                    };
                    let Some(experiment) = snapshot.experiments.get(&assignment.experiment_id)
                    else {
                        return Ok(Err(SessionError::Other(format!(
                            "delegated Experiment {} does not exist",
                            assignment.experiment_id
                        ))));
                    };
                    if experiment.status != ExperimentStatus::Planned
                        || experiment.revision != assignment.expected_revision
                    {
                        return Ok(Err(SessionError::Other(format!(
                            "delegated Experiment {} is {:?} revision {}, expected Planned revision {}",
                            experiment.id,
                            experiment.status,
                            experiment.revision,
                            assignment.expected_revision
                        ))));
                    }

                    let sql = format!(
                        "SELECT {TASK_COLUMNS} FROM tasks WHERE case_id = ?1 AND experiment_id = ?2"
                    );
                    let existing_task = tx
                        .query_row(
                            &sql,
                            params![assignment.case_id.as_str(), assignment.experiment_id.as_str()],
                            row_to_record,
                        )
                        .optional()?;
                    if let Some(existing) = existing_task {
                        if existing.task_id == start.task_id
                            && matches!(existing.state, TaskStatus::Queued | TaskStatus::Retrying)
                        {
                            tx.commit()?;
                            return Ok(Ok(existing));
                        }
                        return Ok(Err(SessionError::Other(format!(
                            "Experiment {} already maps to durable task {} in {:?}",
                            assignment.experiment_id, existing.task_id, existing.state
                        ))));
                    }

                    let running: i64 = tx.query_row(
                        "SELECT COUNT(*) FROM tasks WHERE case_id = ?1 AND state = 'running'",
                        params![assignment.case_id.as_str()],
                        |row| row.get(0),
                    )?;
                    if running as usize >= assignment.max_concurrent_per_case.max(1) {
                        return Ok(Err(SessionError::Other(format!(
                            "case {} already has {} running Experiments (limit {})",
                            assignment.case_id,
                            running,
                            assignment.max_concurrent_per_case.max(1)
                        ))));
                    }

                    let now = Utc::now();
                    let idempotency_key = format!(
                        "ledger-experiment:{}:{}",
                        assignment.case_id, experiment.idempotency_key
                    );
                    tx.execute(
                        "INSERT INTO tasks
                         (task_id, parent_session_id, child_session_id, kind, description,
                          state, lease_owner, lease_expires_at, attempt, idempotency_key,
                          checkpoint, result, last_error, safe_to_retry, delivered,
                          created_at, updated_at, payload, case_id, experiment_id,
                          lease_duration_ms, max_concurrent_per_case)
                         VALUES (?1, ?2, NULL, 'subagent', ?3, 'queued', NULL, NULL, 0, ?4,
                                 NULL, NULL, NULL, ?5, 0, ?6, ?6, ?7, ?8, ?9, ?10, ?11)",
                        params![
                            start.task_id,
                            parent_session_id,
                            start.description,
                            idempotency_key,
                            assignment.safe_to_retry as i64,
                            now.to_rfc3339(),
                            start.payload,
                            assignment.case_id.as_str(),
                            assignment.experiment_id.as_str(),
                            assignment.lease_ms.max(1) as i64,
                            assignment.max_concurrent_per_case.max(1) as i64,
                        ],
                    )?;
                    let queued = UnstoredLedgerEvent {
                        event_id: format!("event-experiment-queue-{}", start.task_id),
                        aggregate_kind: AggregateKind::Experiment,
                        aggregate_id: experiment.id.to_string(),
                        aggregate_revision: experiment.revision + 1,
                        actor_session_id: parent_session_id.clone(),
                        event: LedgerEvent::ExperimentQueuedV2 {
                            schema_version: LEDGER_EVENT_SCHEMA_VERSION,
                            experiment_id: experiment.id.clone(),
                            expected_revision: experiment.revision,
                            task_id: start.task_id.clone(),
                            occurred_at: now,
                        },
                        created_at: now,
                    };
                    if let Err(error) = append_ledger_events_in_transaction(
                        &tx,
                        &assignment.case_id,
                        &format!("experiment-queue:{}", start.task_id),
                        &parent_session_id,
                        vec![queued],
                    ) {
                        return Ok(Err(error));
                    }
                    let sql = format!("SELECT {TASK_COLUMNS} FROM tasks WHERE task_id = ?1");
                    let record = tx.query_row(&sql, params![start.task_id], row_to_record)?;
                    tx.commit()?;
                    Ok(Ok(record))
                }
            })
            .await?;
        let record = nested?;
        holmes_core::metrics::metrics().count("experiment.queued");
        Ok(record)
    }

    /// Queued/Retrying → Running under a fresh lease owned by this store.
    /// Returns the updated record, or `None` when the task is not leasable
    /// (already running, terminal, or unknown) — the conditional UPDATE makes
    /// a double-acquire a no-op instead of two owners believing they run it.
    pub async fn acquire_lease(&self, task_id: &str) -> Result<Option<TaskRecord>, SessionError> {
        self.acquire_lease_with_duration(task_id, DEFAULT_LEASE_SECS)
            .await
    }

    pub async fn acquire_lease_with_duration(
        &self,
        task_id: &str,
        lease_secs: i64,
    ) -> Result<Option<TaskRecord>, SessionError> {
        // Negative durations are intentionally supported by crash/recovery
        // fault-injection tests for ordinary tasks. Delegated Experiments never
        // accept a caller-supplied duration; they use their persisted positive
        // lease policy through `acquire_experiment_lease` below.
        if lease_secs <= 0
            && self
                .get(task_id)
                .await?
                .is_none_or(|record| record.experiment_id.is_none())
        {
            return self.acquire_generic_lease(task_id, lease_secs).await;
        }
        self.acquire_lease_with_duration_ms(
            task_id,
            u64::try_from(lease_secs.max(1)).unwrap_or(1) * 1_000,
        )
        .await
    }

    pub async fn acquire_lease_with_duration_ms(
        &self,
        task_id: &str,
        lease_ms: u64,
    ) -> Result<Option<TaskRecord>, SessionError> {
        if self
            .get(task_id)
            .await?
            .is_some_and(|record| record.experiment_id.is_some())
        {
            return self.acquire_experiment_lease(task_id, lease_ms).await;
        }
        self.acquire_generic_lease(
            task_id,
            i64::try_from(lease_ms.div_ceil(1_000)).unwrap_or(i64::MAX),
        )
        .await
    }

    async fn acquire_generic_lease(
        &self,
        task_id: &str,
        lease_secs: i64,
    ) -> Result<Option<TaskRecord>, SessionError> {
        let changed = self
            .update_where(
                "UPDATE tasks SET state = 'running', attempt = attempt + 1,
                        lease_owner = ?2, lease_expires_at = ?3, updated_at = ?4
                WHERE task_id = ?1 AND state IN ('queued', 'retrying')",
                task_id,
                lease_secs,
            )
            .await?;
        debug_assert!(changed <= 1);
        if changed == 0 {
            return Ok(None);
        }
        self.get(task_id).await
    }

    async fn acquire_experiment_lease(
        &self,
        task_id: &str,
        _requested_lease_ms: u64,
    ) -> Result<Option<TaskRecord>, SessionError> {
        let task_id = task_id.to_owned();
        let owner = self.owner_id.clone();
        let nested = self
            .write_contention
            .with_db_retry(|| {
                let task_id = task_id.clone();
                let owner = owner.clone();
                async move {
                    let mut conn = self.conn.lock().await;
                    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                    let sql = format!("SELECT {TASK_COLUMNS} FROM tasks WHERE task_id = ?1");
                    let record = tx
                        .query_row(&sql, params![task_id], row_to_record)
                        .optional()?;
                    let Some(record) = record else {
                        tx.commit()?;
                        return Ok::<_, rusqlite::Error>(Ok(None));
                    };
                    if !matches!(record.state, TaskStatus::Queued | TaskStatus::Retrying) {
                        tx.commit()?;
                        return Ok(Ok(None));
                    }
                    let (Some(case), Some(experiment)) =
                        (record.case_id.as_deref(), record.experiment_id.as_deref())
                    else {
                        return Ok(Err(SessionError::Other(
                            "Experiment task is missing its durable mapping".into(),
                        )));
                    };
                    let running: i64 = tx.query_row(
                        "SELECT COUNT(*) FROM tasks
                         WHERE case_id = ?1 AND state = 'running' AND task_id <> ?2",
                        params![case, task_id],
                        |row| row.get(0),
                    )?;
                    if running as usize >= record.max_concurrent_per_case {
                        tx.commit()?;
                        return Ok(Ok(None));
                    }
                    let case_id = CaseId::new(case);
                    let experiment_id = ExperimentId::new(experiment);
                    let events =
                        match crate::ledger_store::load_events_from_connection(&tx, &case_id, None)
                        {
                            Ok(events) => events,
                            Err(error) => return Ok(Err(SessionError::Other(error.to_string()))),
                        };
                    let snapshot = match crate::ledger_store::project_events(&case_id, &events) {
                        Ok(snapshot) => snapshot,
                        Err(error) => return Ok(Err(SessionError::Other(error.to_string()))),
                    };
                    let Some(experiment) = snapshot.experiments.get(&experiment_id) else {
                        return Ok(Err(SessionError::Other(format!(
                            "mapped Experiment {experiment_id} no longer exists"
                        ))));
                    };
                    if experiment.task_id.as_deref() != Some(record.task_id.as_str())
                        || !matches!(
                            experiment.status,
                            ExperimentStatus::Queued | ExperimentStatus::Running
                        )
                    {
                        return Ok(Err(SessionError::Other(format!(
                            "mapped Experiment {experiment_id} is {:?} for task {:?}",
                            experiment.status, experiment.task_id
                        ))));
                    }
                    let attempt = record.attempt.saturating_add(1);
                    let lease_ms = record.lease_duration_ms.max(1);
                    let millis = i64::try_from(lease_ms).unwrap_or(i64::MAX);
                    let now = Utc::now();
                    let expiry = (now + chrono::Duration::milliseconds(millis)).to_rfc3339();
                    let changed = tx.execute(
                        "UPDATE tasks SET state = 'running', attempt = ?2, lease_owner = ?3,
                                lease_expires_at = ?4, lease_duration_ms = ?5, updated_at = ?6
                         WHERE task_id = ?1 AND state IN ('queued', 'retrying')",
                        params![
                            task_id,
                            attempt,
                            owner,
                            expiry,
                            lease_ms as i64,
                            now.to_rfc3339()
                        ],
                    )?;
                    if changed != 1 {
                        tx.commit()?;
                        return Ok(Ok(None));
                    }
                    let started = UnstoredLedgerEvent {
                        event_id: format!("event-experiment-start-{}-{attempt}", record.task_id),
                        aggregate_kind: AggregateKind::Experiment,
                        aggregate_id: experiment.id.to_string(),
                        aggregate_revision: experiment.revision + 1,
                        actor_session_id: record.parent_session_id.clone().unwrap_or_default(),
                        event: LedgerEvent::ExperimentStartedV2 {
                            schema_version: LEDGER_EVENT_SCHEMA_VERSION,
                            experiment_id: experiment.id.clone(),
                            expected_revision: experiment.revision,
                            attempt,
                            occurred_at: now,
                        },
                        created_at: now,
                    };
                    if let Err(error) = append_ledger_events_in_transaction(
                        &tx,
                        &case_id,
                        &format!("experiment-start:{}:{attempt}", record.task_id),
                        record.parent_session_id.as_deref().unwrap_or("runtime"),
                        vec![started],
                    ) {
                        return Ok(Err(error));
                    }
                    let updated = tx.query_row(&sql, params![record.task_id], row_to_record)?;
                    tx.commit()?;
                    Ok(Ok(Some(updated)))
                }
            })
            .await?;
        let leased = nested?;
        if leased.is_some() {
            holmes_core::metrics::metrics().count("experiment.leased");
        }
        Ok(leased)
    }

    async fn finish_experiment_task(
        &self,
        task_id: &str,
        fencing: u32,
        input: ExperimentTerminalInput,
    ) -> Result<bool, SessionError> {
        let task_id = task_id.to_owned();
        let owner = self.owner_id.clone();
        let nested = self
            .write_contention
            .with_db_retry(|| {
                let task_id = task_id.clone();
                let owner = owner.clone();
                let input = match &input {
                    ExperimentTerminalInput::Completed(value) => {
                        ExperimentTerminalInput::Completed(value.clone())
                    }
                    ExperimentTerminalInput::Failed(value) => {
                        ExperimentTerminalInput::Failed(value.clone())
                    }
                    ExperimentTerminalInput::Cancelled(value) => {
                        ExperimentTerminalInput::Cancelled(value.clone())
                    }
                };
                async move {
                    let mut conn = self.conn.lock().await;
                    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                    let sql = format!("SELECT {TASK_COLUMNS} FROM tasks WHERE task_id = ?1");
                    let record = tx
                        .query_row(&sql, params![task_id], row_to_record)
                        .optional()?;
                    let Some(record) = record else {
                        tx.commit()?;
                        return Ok::<_, rusqlite::Error>(Ok(false));
                    };
                    if record.state != TaskStatus::Running
                        || record.lease_owner.as_deref() != Some(owner.as_str())
                        || record.attempt != fencing
                    {
                        tx.commit()?;
                        return Ok(Ok(false));
                    }
                    let (Some(case), Some(experiment)) =
                        (record.case_id.as_deref(), record.experiment_id.as_deref())
                    else {
                        return Ok(Err(SessionError::Other(
                            "Experiment task lost its durable mapping".into(),
                        )));
                    };
                    let case_id = CaseId::new(case);
                    let experiment_id = ExperimentId::new(experiment);
                    let stored =
                        match crate::ledger_store::load_events_from_connection(&tx, &case_id, None)
                        {
                            Ok(events) => events,
                            Err(error) => {
                                return Ok(Err(SessionError::Other(error.to_string())))
                            }
                        };
                    let snapshot = match crate::ledger_store::project_events(&case_id, &stored) {
                        Ok(snapshot) => snapshot,
                        Err(error) => return Ok(Err(SessionError::Other(error.to_string()))),
                    };
                    let Some(experiment) = snapshot.experiments.get(&experiment_id) else {
                        return Ok(Err(SessionError::Other(format!(
                            "mapped Experiment {experiment_id} no longer exists"
                        ))));
                    };
                    if experiment.status != ExperimentStatus::Running
                        || experiment.task_id.as_deref() != Some(record.task_id.as_str())
                        || experiment.attempt != fencing
                    {
                        tx.commit()?;
                        return Ok(Ok(false));
                    }

                    let now = Utc::now();
                    let mut child_session_id = record.child_session_id.clone();
                    let (task_state, task_result, task_error, ledger_event, outcome_name) =
                        match input {
                            ExperimentTerminalInput::Completed(output) => {
                                let parsed = serde_json::from_str::<VerifiedAgentTaskResult>(&output);
                                match parsed {
                                    Ok(verified) => {
                                        if child_session_id.is_none() {
                                            child_session_id = verified.result.checkpoint.clone();
                                        }
                                        let recomputed =
                                            verify_agent_task_result(&verified.result);
                                        let mut defects = recomputed.defects.clone();
                                        if recomputed != verified.verification {
                                            defects.push(
                                                "serialized verification verdict does not match deterministic recomputation"
                                                    .into(),
                                            );
                                        }
                                        if verified.result.task_id != record.task_id {
                                            defects.push(format!(
                                                "result task_id {} does not match {}",
                                                verified.result.task_id, record.task_id
                                            ));
                                        }
                                        let mut evidence_ids = verified
                                            .result
                                            .evidence
                                            .iter()
                                            .filter(|reference| reference.kind == "ledger_evidence")
                                            .map(|reference| reference.reference.clone())
                                            .collect::<Vec<_>>();
                                        evidence_ids.sort();
                                        evidence_ids.dedup();
                                        if evidence_ids.is_empty()
                                            && matches!(
                                                verified.result.status,
                                                AgentTaskStatus::Completed | AgentTaskStatus::Partial
                                            )
                                        {
                                            defects.push(
                                                "delegated Experiment produced no durable Ledger Evidence"
                                                    .into(),
                                            );
                                        }
                                        if let Some(child_session) = child_session_id.as_deref() {
                                            let child_case = tx
                                                .query_row(
                                                    "SELECT case_id FROM sessions WHERE id = ?1",
                                                    params![child_session],
                                                    |row| row.get::<_, String>(0),
                                                )
                                                .optional()?;
                                            if child_case.as_deref() != Some(case_id.as_str()) {
                                                defects.push(format!(
                                                    "child session {child_session} is missing or belongs to another case"
                                                ));
                                            }
                                        } else if matches!(
                                            verified.result.status,
                                            AgentTaskStatus::Completed | AgentTaskStatus::Partial
                                        ) {
                                            defects.push(
                                                "delegated result has no durable child session checkpoint"
                                                    .into(),
                                            );
                                        }
                                        for evidence_id in &evidence_ids {
                                            let Some(evidence) = snapshot.evidence.get(evidence_id)
                                            else {
                                                defects.push(format!(
                                                    "result references unknown case Evidence {evidence_id}"
                                                ));
                                                continue;
                                            };
                                            if evidence.binding.experiment_id.as_ref()
                                                != Some(&experiment_id)
                                            {
                                                defects.push(format!(
                                                    "Evidence {evidence_id} is not bound to Experiment {experiment_id}"
                                                ));
                                            }
                                            if child_session_id.as_deref()
                                                != Some(evidence.source_session_id.as_str())
                                            {
                                                defects.push(format!(
                                                    "Evidence {evidence_id} came from a different child session"
                                                ));
                                            }
                                        }

                                        if !defects.is_empty() {
                                            let reason = defects.join("; ");
                                            (
                                                "failed",
                                                None,
                                                Some(reason.clone()),
                                                LedgerEvent::ExperimentFailedV2 {
                                                    schema_version: LEDGER_EVENT_SCHEMA_VERSION,
                                                    experiment_id: experiment_id.clone(),
                                                    expected_revision: experiment.revision,
                                                    reason,
                                                    occurred_at: now,
                                                },
                                                "failed",
                                            )
                                        } else {
                                            match verified.result.status {
                                                AgentTaskStatus::Completed
                                                | AgentTaskStatus::Partial => (
                                                    "succeeded",
                                                    Some(output),
                                                    None,
                                                    LedgerEvent::ExperimentObservedV2 {
                                                        schema_version:
                                                            LEDGER_EVENT_SCHEMA_VERSION,
                                                        experiment_id: experiment_id.clone(),
                                                        expected_revision: experiment.revision,
                                                        evidence_ids,
                                                        occurred_at: now,
                                                    },
                                                    "observed",
                                                ),
                                                AgentTaskStatus::Failed => (
                                                    "failed",
                                                    None,
                                                    Some(verified.result.summary.clone()),
                                                    LedgerEvent::ExperimentFailedV2 {
                                                        schema_version:
                                                            LEDGER_EVENT_SCHEMA_VERSION,
                                                        experiment_id: experiment_id.clone(),
                                                        expected_revision: experiment.revision,
                                                        reason: verified.result.summary,
                                                        occurred_at: now,
                                                    },
                                                    "failed",
                                                ),
                                                AgentTaskStatus::Cancelled => (
                                                    "cancelled",
                                                    None,
                                                    Some(verified.result.summary.clone()),
                                                    LedgerEvent::ExperimentCancelledV2 {
                                                        schema_version:
                                                            LEDGER_EVENT_SCHEMA_VERSION,
                                                        experiment_id: experiment_id.clone(),
                                                        expected_revision: experiment.revision,
                                                        reason: verified.result.summary,
                                                        occurred_at: now,
                                                    },
                                                    "cancelled",
                                                ),
                                            }
                                        }
                                    }
                                    Err(error) => {
                                        let reason = format!(
                                            "delegated result is not a valid VerifiedAgentTaskResult: {error}"
                                        );
                                        (
                                            "failed",
                                            None,
                                            Some(reason.clone()),
                                            LedgerEvent::ExperimentFailedV2 {
                                                schema_version: LEDGER_EVENT_SCHEMA_VERSION,
                                                experiment_id: experiment_id.clone(),
                                                expected_revision: experiment.revision,
                                                reason,
                                                occurred_at: now,
                                            },
                                            "failed",
                                        )
                                    }
                                }
                            }
                            ExperimentTerminalInput::Failed(reason) => (
                                "failed",
                                None,
                                Some(reason.clone()),
                                LedgerEvent::ExperimentFailedV2 {
                                    schema_version: LEDGER_EVENT_SCHEMA_VERSION,
                                    experiment_id: experiment_id.clone(),
                                    expected_revision: experiment.revision,
                                    reason,
                                    occurred_at: now,
                                },
                                "failed",
                            ),
                            ExperimentTerminalInput::Cancelled(reason) => (
                                "cancelled",
                                None,
                                Some(reason.clone()),
                                LedgerEvent::ExperimentCancelledV2 {
                                    schema_version: LEDGER_EVENT_SCHEMA_VERSION,
                                    experiment_id: experiment_id.clone(),
                                    expected_revision: experiment.revision,
                                    reason,
                                    occurred_at: now,
                                },
                                "cancelled",
                            ),
                        };
                    let changed = tx.execute(
                        "UPDATE tasks SET state = ?2, result = ?3, last_error = ?4,
                                child_session_id = COALESCE(child_session_id, ?5),
                                lease_owner = NULL, lease_expires_at = NULL, updated_at = ?6
                         WHERE task_id = ?1 AND state = 'running'
                           AND lease_owner = ?7 AND attempt = ?8",
                        params![
                            record.task_id,
                            task_state,
                            task_result,
                            task_error,
                            child_session_id,
                            now.to_rfc3339(),
                            owner,
                            fencing,
                        ],
                    )?;
                    if changed != 1 {
                        tx.commit()?;
                        return Ok(Ok(false));
                    }
                    let event = UnstoredLedgerEvent {
                        event_id: format!(
                            "event-experiment-{outcome_name}-{}-{fencing}",
                            record.task_id
                        ),
                        aggregate_kind: AggregateKind::Experiment,
                        aggregate_id: experiment_id.to_string(),
                        aggregate_revision: experiment.revision + 1,
                        actor_session_id: child_session_id
                            .clone()
                            .or(record.parent_session_id.clone())
                            .unwrap_or_else(|| "runtime".into()),
                        event: ledger_event,
                        created_at: now,
                    };
                    if let Err(error) = append_ledger_events_in_transaction(
                        &tx,
                        &case_id,
                        &format!(
                            "experiment-terminal:{}:{fencing}:{outcome_name}",
                            record.task_id
                        ),
                        child_session_id
                            .as_deref()
                            .or(record.parent_session_id.as_deref())
                            .unwrap_or("runtime"),
                        vec![event],
                    ) {
                        return Ok(Err(error));
                    }
                    tx.commit()?;
                    Ok(Ok(true))
                }
            })
            .await?;
        let changed = nested?;
        if changed {
            let metric = match self.get(&task_id).await?.map(|record| record.state) {
                Some(TaskStatus::Succeeded) => "experiment.observed",
                Some(TaskStatus::Cancelled) => "experiment.cancelled",
                _ => "experiment.failed",
            };
            holmes_core::metrics::metrics().count(metric);
        } else {
            holmes_core::metrics::metrics().count("experiment.fenced_write_rejected");
        }
        Ok(changed)
    }

    /// Renew this store's lease on a running task. `fencing` must be the
    /// `attempt` the caller acquired its lease with: the update only lands
    /// while that attempt still owns the task. Returns false when the lease
    /// is no longer ours (reclaimed/superseded, or the task left Running) —
    /// the caller must stop its worker and never write again.
    pub async fn heartbeat(&self, task_id: &str, fencing: u32) -> Result<bool, SessionError> {
        let lease_duration_ms = self
            .get(task_id)
            .await?
            .map(|record| record.lease_duration_ms)
            .unwrap_or((DEFAULT_LEASE_SECS as u64) * 1_000)
            .max(1);
        let task_id_owned = task_id.to_string();
        let owner = self.owner_id.clone();
        let expiry = (Utc::now()
            + chrono::Duration::milliseconds(i64::try_from(lease_duration_ms).unwrap_or(i64::MAX)))
        .to_rfc3339();
        let changed = self
            .write_contention
            .with_db_retry(|| {
                let task_id = task_id_owned.clone();
                let owner = owner.clone();
                let expiry = expiry.clone();
                async move {
                    let conn = self.conn.lock().await;
                    let changed = conn.execute(
                        "UPDATE tasks SET lease_expires_at = ?3, updated_at = ?4
                         WHERE task_id = ?1 AND state = 'running'
                           AND lease_owner = ?2 AND attempt = ?5",
                        params![task_id, owner, expiry, Utc::now().to_rfc3339(), fencing],
                    )?;
                    Ok::<_, rusqlite::Error>(changed)
                }
            })
            .await?;
        Ok(changed == 1)
    }

    /// Running → Succeeded with the result payload. Fenced: only the attempt
    /// that owns the lease may commit the terminal state.
    pub async fn complete(
        &self,
        task_id: &str,
        fencing: u32,
        result: &str,
    ) -> Result<bool, SessionError> {
        if self
            .get(task_id)
            .await?
            .is_some_and(|record| record.experiment_id.is_some())
        {
            return self
                .finish_experiment_task(
                    task_id,
                    fencing,
                    ExperimentTerminalInput::Completed(result.to_owned()),
                )
                .await;
        }
        let result_owned = result.to_string();
        self.transition_terminal(task_id, fencing, "succeeded", Some(result_owned), None)
            .await
    }

    /// Running → Retrying (retryable) or Failed (permanent), error recorded.
    /// Fenced like `complete`.
    pub async fn fail(
        &self,
        task_id: &str,
        fencing: u32,
        error: &str,
        retryable: bool,
    ) -> Result<bool, SessionError> {
        if self
            .get(task_id)
            .await?
            .is_some_and(|record| record.experiment_id.is_some())
            && !retryable
        {
            return self
                .finish_experiment_task(
                    task_id,
                    fencing,
                    ExperimentTerminalInput::Failed(error.to_owned()),
                )
                .await;
        }
        let state = if retryable { "retrying" } else { "failed" };
        self.transition_terminal(task_id, fencing, state, None, Some(error.to_string()))
            .await
    }

    /// Queued/Running/Retrying → Cancelled. Unfenced on purpose: this is the
    /// operator-facing cancel, which must win against any attempt. A worker
    /// reporting its own cancellation goes through `cancel_attempt` instead.
    pub async fn cancel(&self, task_id: &str) -> Result<bool, SessionError> {
        if self
            .get(task_id)
            .await?
            .is_some_and(|record| record.experiment_id.is_some())
        {
            return self.cancel_experiment_task(task_id).await;
        }
        let task_id_owned = task_id.to_string();
        let changed = self
            .write_contention
            .with_db_retry(|| {
                let task_id = task_id_owned.clone();
                async move {
                    let conn = self.conn.lock().await;
                    let changed = conn.execute(
                        "UPDATE tasks SET state = 'cancelled', lease_owner = NULL,
                                lease_expires_at = NULL, updated_at = ?2
                         WHERE task_id = ?1 AND state IN ('queued', 'running', 'retrying')",
                        params![task_id, Utc::now().to_rfc3339()],
                    )?;
                    Ok::<_, rusqlite::Error>(changed)
                }
            })
            .await?;
        Ok(changed == 1)
    }

    async fn cancel_experiment_task(&self, task_id: &str) -> Result<bool, SessionError> {
        let task_id = task_id.to_owned();
        let nested = self
            .write_contention
            .with_db_retry(|| {
                let task_id = task_id.clone();
                async move {
                    let mut conn = self.conn.lock().await;
                    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                    let sql = format!("SELECT {TASK_COLUMNS} FROM tasks WHERE task_id = ?1");
                    let record = tx
                        .query_row(&sql, params![task_id], row_to_record)
                        .optional()?;
                    let Some(record) = record else {
                        tx.commit()?;
                        return Ok::<_, rusqlite::Error>(Ok(false));
                    };
                    if !matches!(
                        record.state,
                        TaskStatus::Queued | TaskStatus::Running | TaskStatus::Retrying
                    ) {
                        tx.commit()?;
                        return Ok(Ok(false));
                    }
                    let (Some(case), Some(experiment)) =
                        (record.case_id.as_deref(), record.experiment_id.as_deref())
                    else {
                        return Ok(Err(SessionError::Other(
                            "Experiment task lost its durable mapping".into(),
                        )));
                    };
                    let case_id = CaseId::new(case);
                    let experiment_id = ExperimentId::new(experiment);
                    let events =
                        crate::ledger_store::load_events_from_connection(&tx, &case_id, None)
                            .map_err(|error| {
                                rusqlite::Error::ToSqlConversionFailure(Box::new(error))
                            })?;
                    let snapshot = crate::ledger_store::project_events(&case_id, &events).map_err(
                        |error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)),
                    )?;
                    let experiment = snapshot.experiments.get(&experiment_id).ok_or_else(|| {
                        rusqlite::Error::InvalidParameterName(format!(
                            "unknown mapped Experiment {experiment_id}"
                        ))
                    })?;
                    if experiment.task_id.as_deref() != Some(record.task_id.as_str())
                        || !matches!(
                            experiment.status,
                            ExperimentStatus::Queued | ExperimentStatus::Running
                        )
                    {
                        return Ok(Err(SessionError::Other(format!(
                            "mapped Experiment {experiment_id} is not cancellable"
                        ))));
                    }
                    let now = Utc::now();
                    let changed = tx.execute(
                        "UPDATE tasks SET state = 'cancelled', lease_owner = NULL,
                                lease_expires_at = NULL, last_error = 'operator cancelled',
                                updated_at = ?2
                         WHERE task_id = ?1 AND state IN ('queued', 'running', 'retrying')",
                        params![record.task_id, now.to_rfc3339()],
                    )?;
                    if changed != 1 {
                        tx.commit()?;
                        return Ok(Ok(false));
                    }
                    let actor = record.parent_session_id.as_deref().unwrap_or("operator");
                    let event = UnstoredLedgerEvent {
                        event_id: format!("event-experiment-operator-cancel-{}", record.task_id),
                        aggregate_kind: AggregateKind::Experiment,
                        aggregate_id: experiment_id.to_string(),
                        aggregate_revision: experiment.revision + 1,
                        actor_session_id: actor.to_owned(),
                        event: LedgerEvent::ExperimentCancelledV2 {
                            schema_version: LEDGER_EVENT_SCHEMA_VERSION,
                            experiment_id,
                            expected_revision: experiment.revision,
                            reason: "operator cancelled durable task".into(),
                            occurred_at: now,
                        },
                        created_at: now,
                    };
                    if let Err(error) = append_ledger_events_in_transaction(
                        &tx,
                        &case_id,
                        &format!("experiment-operator-cancel:{}", record.task_id),
                        actor,
                        vec![event],
                    ) {
                        return Ok(Err(error));
                    }
                    tx.commit()?;
                    Ok(Ok(true))
                }
            })
            .await?;
        let changed = nested?;
        if changed {
            holmes_core::metrics::metrics().count("experiment.cancelled");
        }
        Ok(changed)
    }

    /// Running → Cancelled, fenced: only the owning attempt may report its own
    /// cancellation (a superseded attempt's late cancel must not clobber the
    /// new attempt's Running state).
    pub async fn cancel_attempt(&self, task_id: &str, fencing: u32) -> Result<bool, SessionError> {
        if self
            .get(task_id)
            .await?
            .is_some_and(|record| record.experiment_id.is_some())
        {
            return self
                .finish_experiment_task(
                    task_id,
                    fencing,
                    ExperimentTerminalInput::Cancelled("worker cancelled".into()),
                )
                .await;
        }
        let task_id_owned = task_id.to_string();
        let owner = self.owner_id.clone();
        let changed = self
            .write_contention
            .with_db_retry(|| {
                let task_id = task_id_owned.clone();
                let owner = owner.clone();
                async move {
                    let conn = self.conn.lock().await;
                    let changed = conn.execute(
                        "UPDATE tasks SET state = 'cancelled', lease_owner = NULL,
                                lease_expires_at = NULL, updated_at = ?3
                         WHERE task_id = ?1 AND state = 'running'
                           AND lease_owner = ?2 AND attempt = ?4",
                        params![task_id, owner, Utc::now().to_rfc3339(), fencing],
                    )?;
                    Ok::<_, rusqlite::Error>(changed)
                }
            })
            .await?;
        Ok(changed == 1)
    }

    /// Record the child session a running subagent task produced. Fenced:
    /// a superseded attempt's write is rejected (returns false).
    pub async fn set_child_session(
        &self,
        task_id: &str,
        fencing: u32,
        child_session_id: &str,
    ) -> Result<bool, SessionError> {
        let task_id_owned = task_id.to_string();
        let child_owned = child_session_id.to_string();
        let owner = self.owner_id.clone();
        let changed = self
            .write_contention
            .with_db_retry(|| {
                let task_id = task_id_owned.clone();
                let child = child_owned.clone();
                let owner = owner.clone();
                async move {
                    let conn = self.conn.lock().await;
                    let changed = conn.execute(
                        "UPDATE tasks SET child_session_id = ?3, updated_at = ?4
                         WHERE task_id = ?1 AND state = 'running'
                           AND lease_owner = ?2 AND attempt = ?5",
                        params![task_id, owner, child, Utc::now().to_rfc3339(), fencing],
                    )?;
                    Ok::<_, rusqlite::Error>(changed)
                }
            })
            .await?;
        Ok(changed == 1)
    }

    /// Persist a checkpoint blob for a running task. Fenced: a superseded
    /// attempt's checkpoint is rejected (returns false).
    pub async fn save_checkpoint(
        &self,
        task_id: &str,
        fencing: u32,
        checkpoint: &str,
    ) -> Result<bool, SessionError> {
        let task_id_owned = task_id.to_string();
        let checkpoint_owned = checkpoint.to_string();
        let owner = self.owner_id.clone();
        let changed = self
            .write_contention
            .with_db_retry(|| {
                let task_id = task_id_owned.clone();
                let checkpoint = checkpoint_owned.clone();
                let owner = owner.clone();
                async move {
                    let conn = self.conn.lock().await;
                    let changed = conn.execute(
                        "UPDATE tasks SET checkpoint = ?3, updated_at = ?4
                         WHERE task_id = ?1 AND state = 'running'
                           AND lease_owner = ?2 AND attempt = ?5",
                        params![task_id, owner, checkpoint, Utc::now().to_rfc3339(), fencing],
                    )?;
                    Ok::<_, rusqlite::Error>(changed)
                }
            })
            .await?;
        Ok(changed == 1)
    }

    /// Restart detection: every Running (or previously-detected Recovering)
    /// task whose lease has expired is no longer owned by a live process.
    /// Safe-to-retry tasks move to Recovering; tasks with external side
    /// effects move to ManualRecoveryRequired so nothing re-executes them
    /// automatically (AGT-007 / §11.5).
    pub async fn recover_expired_leases(
        &self,
        now: DateTime<Utc>,
    ) -> Result<LeaseRecoveryOutcome, SessionError> {
        let now_str = now.to_rfc3339();
        let now_for_retry = now_str.clone();
        let occurred_at_for_retry = now;
        let nested = self
            .write_contention
            .with_db_retry(|| {
                let now = now_for_retry.clone();
                let occurred_at = occurred_at_for_retry;
                async move {
                    let mut conn = self.conn.lock().await;
                    let tx = conn.transaction()?;
                    let mut stmt = tx.prepare(
                        "SELECT task_id FROM tasks
                         WHERE state IN ('running', 'recovering')
                           AND lease_expires_at IS NOT NULL
                           AND lease_expires_at < ?1",
                    )?;
                    let ids: Vec<String> = stmt
                        .query_map(params![now], |row| row.get(0))?
                        .collect::<Result<_, _>>()?;
                    drop(stmt);
                    for id in &ids {
                        let sql = format!("SELECT {TASK_COLUMNS} FROM tasks WHERE task_id = ?1");
                        let record = tx.query_row(&sql, params![id], row_to_record)?;
                        // Per-row disposition by retry safety; both transitions
                        // land in this single transaction, so a crash here
                        // leaves the pre-restart state untouched.
                        tx.execute(
                            "UPDATE tasks SET
                                state = CASE WHEN safe_to_retry = 1
                                             THEN 'recovering'
                                             ELSE 'manual_recovery_required' END,
                                lease_owner = NULL,
                                lease_expires_at = NULL,
                                last_error = CASE WHEN safe_to_retry = 1 THEN last_error
                                                  ELSE 'lease expired; manual reconciliation required' END,
                                updated_at = ?2
                             WHERE task_id = ?1
                               AND state IN ('running', 'recovering')",
                            params![id, now],
                        )?;
                        if !record.safe_to_retry {
                            if let (Some(case), Some(experiment)) =
                                (record.case_id.as_deref(), record.experiment_id.as_deref())
                            {
                                let case_id = CaseId::new(case);
                                let experiment_id = ExperimentId::new(experiment);
                                let events = match crate::ledger_store::load_events_from_connection(
                                    &tx,
                                    &case_id,
                                    None,
                                ) {
                                    Ok(events) => events,
                                    Err(error) => {
                                        return Ok::<_, rusqlite::Error>(Err(SessionError::Other(
                                            error.to_string(),
                                        )))
                                    }
                                };
                                let snapshot = match crate::ledger_store::project_events(
                                    &case_id,
                                    &events,
                                ) {
                                    Ok(snapshot) => snapshot,
                                    Err(error) => {
                                        return Ok(Err(SessionError::Other(error.to_string())))
                                    }
                                };
                                let Some(experiment) = snapshot.experiments.get(&experiment_id)
                                else {
                                    return Ok(Err(SessionError::Other(format!(
                                        "mapped Experiment {experiment_id} disappeared during recovery"
                                    ))));
                                };
                                if experiment.status != ExperimentStatus::Running
                                    || experiment.task_id.as_deref()
                                        != Some(record.task_id.as_str())
                                    || experiment.attempt != record.attempt
                                {
                                    return Ok(Err(SessionError::Other(format!(
                                        "task/Experiment fencing state diverged during recovery for {}",
                                        record.task_id
                                    ))));
                                }
                                let event = UnstoredLedgerEvent {
                                    event_id: format!(
                                        "event-experiment-expired-{}-{}",
                                        record.task_id, record.attempt
                                    ),
                                    aggregate_kind: AggregateKind::Experiment,
                                    aggregate_id: experiment_id.to_string(),
                                    aggregate_revision: experiment.revision + 1,
                                    actor_session_id: record
                                        .parent_session_id
                                        .clone()
                                        .unwrap_or_else(|| "recovery".into()),
                                    event: LedgerEvent::ExperimentExpiredV2 {
                                        schema_version: LEDGER_EVENT_SCHEMA_VERSION,
                                        experiment_id,
                                        expected_revision: experiment.revision,
                                        occurred_at,
                                    },
                                    created_at: occurred_at,
                                };
                                if let Err(error) = append_ledger_events_in_transaction(
                                    &tx,
                                    &case_id,
                                    &format!(
                                        "experiment-expired:{}:{}",
                                        record.task_id, record.attempt
                                    ),
                                    record.parent_session_id.as_deref().unwrap_or("recovery"),
                                    vec![event],
                                ) {
                                    return Ok(Err(error));
                                }
                            }
                        }
                    }
                    tx.commit()?;
                    Ok::<_, rusqlite::Error>(Ok(ids))
                }
            })
            .await?;
        let affected: Vec<String> = nested?;

        let mut outcome = LeaseRecoveryOutcome::default();
        for id in affected {
            if let Some(record) = self.get(&id).await? {
                match record.state {
                    TaskStatus::Recovering => {
                        if record.experiment_id.is_some() {
                            holmes_core::metrics::metrics().count("experiment.lease_recovered");
                        }
                        outcome.recovering.push(record)
                    }
                    TaskStatus::ManualRecoveryRequired => {
                        if record.experiment_id.is_some() {
                            holmes_core::metrics::metrics().count("experiment.expired");
                        }
                        outcome.manual_required.push(record)
                    }
                    _ => {}
                }
            }
        }
        Ok(outcome)
    }

    /// Discharge a Recovering task per the recovery policy.
    pub async fn resolve_recovering(
        &self,
        task_id: &str,
        resolution: RecoveryResolution,
    ) -> Result<bool, SessionError> {
        if matches!(&resolution, RecoveryResolution::Fail(_))
            && self
                .get(task_id)
                .await?
                .is_some_and(|record| record.experiment_id.is_some())
        {
            let RecoveryResolution::Fail(reason) = resolution else {
                unreachable!()
            };
            return self.fail_recovering_experiment(task_id, &reason).await;
        }
        let task_id_owned = task_id.to_string();
        let resolution_owned = resolution.clone();
        let changed = self
            .write_contention
            .with_db_retry(|| {
                let task_id = task_id_owned.clone();
                let resolution = resolution_owned.clone();
                async move {
                    let conn = self.conn.lock().await;
                    let changed = match &resolution {
                        RecoveryResolution::Requeue => conn.execute(
                            "UPDATE tasks SET state = 'retrying', updated_at = ?2
                             WHERE task_id = ?1 AND state = 'recovering'",
                            params![task_id, Utc::now().to_rfc3339()],
                        )?,
                        RecoveryResolution::Fail(reason) => conn.execute(
                            "UPDATE tasks SET state = 'failed', last_error = ?2, updated_at = ?3
                             WHERE task_id = ?1 AND state = 'recovering'",
                            params![task_id, reason, Utc::now().to_rfc3339()],
                        )?,
                    };
                    Ok::<_, rusqlite::Error>(changed)
                }
            })
            .await?;
        Ok(changed == 1)
    }

    async fn fail_recovering_experiment(
        &self,
        task_id: &str,
        reason: &str,
    ) -> Result<bool, SessionError> {
        let task_id = task_id.to_owned();
        let reason = reason.to_owned();
        let nested = self
            .write_contention
            .with_db_retry(|| {
                let task_id = task_id.clone();
                let reason = reason.clone();
                async move {
                    let mut conn = self.conn.lock().await;
                    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                    let sql = format!("SELECT {TASK_COLUMNS} FROM tasks WHERE task_id = ?1");
                    let record = tx
                        .query_row(&sql, params![task_id], row_to_record)
                        .optional()?;
                    let Some(record) = record else {
                        tx.commit()?;
                        return Ok::<_, rusqlite::Error>(Ok(false));
                    };
                    if record.state != TaskStatus::Recovering {
                        tx.commit()?;
                        return Ok(Ok(false));
                    }
                    let (Some(case), Some(experiment)) =
                        (record.case_id.as_deref(), record.experiment_id.as_deref())
                    else {
                        return Ok(Err(SessionError::Other(
                            "recovering Experiment task lost its durable mapping".into(),
                        )));
                    };
                    let case_id = CaseId::new(case);
                    let experiment_id = ExperimentId::new(experiment);
                    let events =
                        match crate::ledger_store::load_events_from_connection(&tx, &case_id, None)
                        {
                            Ok(events) => events,
                            Err(error) => return Ok(Err(SessionError::Other(error.to_string()))),
                        };
                    let snapshot = match crate::ledger_store::project_events(&case_id, &events) {
                        Ok(snapshot) => snapshot,
                        Err(error) => return Ok(Err(SessionError::Other(error.to_string()))),
                    };
                    let Some(experiment) = snapshot.experiments.get(&experiment_id) else {
                        return Ok(Err(SessionError::Other(format!(
                            "mapped Experiment {experiment_id} disappeared during recovery"
                        ))));
                    };
                    if experiment.status != ExperimentStatus::Running
                        || experiment.task_id.as_deref() != Some(record.task_id.as_str())
                        || experiment.attempt != record.attempt
                    {
                        return Ok(Err(SessionError::Other(format!(
                            "task/Experiment fencing state diverged during recovery failure for {}",
                            record.task_id
                        ))));
                    }
                    let now = Utc::now();
                    let changed = tx.execute(
                        "UPDATE tasks SET state = 'failed', last_error = ?2,
                                lease_owner = NULL, lease_expires_at = NULL, updated_at = ?3
                         WHERE task_id = ?1 AND state = 'recovering'",
                        params![record.task_id, reason, now.to_rfc3339()],
                    )?;
                    if changed != 1 {
                        tx.commit()?;
                        return Ok(Ok(false));
                    }
                    let actor = record.parent_session_id.as_deref().unwrap_or("recovery");
                    let event = UnstoredLedgerEvent {
                        event_id: format!(
                            "event-experiment-recovery-failed-{}-{}",
                            record.task_id, record.attempt
                        ),
                        aggregate_kind: AggregateKind::Experiment,
                        aggregate_id: experiment_id.to_string(),
                        aggregate_revision: experiment.revision + 1,
                        actor_session_id: actor.to_owned(),
                        event: LedgerEvent::ExperimentFailedV2 {
                            schema_version: LEDGER_EVENT_SCHEMA_VERSION,
                            experiment_id,
                            expected_revision: experiment.revision,
                            reason: reason.clone(),
                            occurred_at: now,
                        },
                        created_at: now,
                    };
                    if let Err(error) = append_ledger_events_in_transaction(
                        &tx,
                        &case_id,
                        &format!(
                            "experiment-recovery-failed:{}:{}",
                            record.task_id, record.attempt
                        ),
                        actor,
                        vec![event],
                    ) {
                        return Ok(Err(error));
                    }
                    tx.commit()?;
                    Ok(Ok(true))
                }
            })
            .await?;
        let changed = nested?;
        if changed {
            holmes_core::metrics::metrics().count("experiment.failed");
        }
        Ok(changed)
    }

    /// Mark a finished task's result as delivered to the parent conversation.
    pub async fn mark_delivered(&self, task_id: &str) -> Result<(), SessionError> {
        let task_id_owned = task_id.to_string();
        self.write_contention
            .with_db_retry(|| {
                let task_id = task_id_owned.clone();
                async move {
                    let conn = self.conn.lock().await;
                    conn.execute(
                        "UPDATE tasks SET delivered = 1, updated_at = ?2 WHERE task_id = ?1",
                        params![task_id, Utc::now().to_rfc3339()],
                    )?;
                    Ok::<_, rusqlite::Error>(())
                }
            })
            .await?;
        Ok(())
    }

    pub async fn get(&self, task_id: &str) -> Result<Option<TaskRecord>, SessionError> {
        let conn = self.conn.lock().await;
        conn.query_row(
            "SELECT task_id, parent_session_id, child_session_id, kind, description,
                    state, lease_owner, lease_expires_at, attempt, idempotency_key,
                    checkpoint, result, last_error, safe_to_retry, delivered,
                    created_at, updated_at, payload, case_id, experiment_id, lease_duration_ms,
                    max_concurrent_per_case
             FROM tasks WHERE task_id = ?1",
            params![task_id],
            row_to_record,
        )
        .optional()
        .map_err(SessionError::from)
    }

    pub async fn find_by_idempotency_key(
        &self,
        key: &str,
    ) -> Result<Option<TaskRecord>, SessionError> {
        let conn = self.conn.lock().await;
        conn.query_row(
            "SELECT task_id, parent_session_id, child_session_id, kind, description,
                    state, lease_owner, lease_expires_at, attempt, idempotency_key,
                    checkpoint, result, last_error, safe_to_retry, delivered,
                    created_at, updated_at, payload, case_id, experiment_id, lease_duration_ms,
                    max_concurrent_per_case
             FROM tasks WHERE idempotency_key = ?1",
            params![key],
            row_to_record,
        )
        .optional()
        .map_err(SessionError::from)
    }

    pub async fn list_by_parent(
        &self,
        parent_session_id: &str,
    ) -> Result<Vec<TaskRecord>, SessionError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT task_id, parent_session_id, child_session_id, kind, description,
                    state, lease_owner, lease_expires_at, attempt, idempotency_key,
                    checkpoint, result, last_error, safe_to_retry, delivered,
                    created_at, updated_at, payload, case_id, experiment_id, lease_duration_ms,
                    max_concurrent_per_case
             FROM tasks WHERE parent_session_id = ?1 ORDER BY created_at",
        )?;
        let rows = stmt.query_map(params![parent_session_id], row_to_record)?;
        let mut records = Vec::new();
        for row in rows {
            records.push(row?);
        }
        Ok(records)
    }

    /// Tasks waiting to be leased and executed (queued, or retrying after a
    /// recovery/requeue). The scheduler scans these each pass and claims them
    /// with `acquire_lease`, whose conditional UPDATE keeps the claim atomic.
    pub async fn list_leasable(&self) -> Result<Vec<TaskRecord>, SessionError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT task_id, parent_session_id, child_session_id, kind, description,
                    state, lease_owner, lease_expires_at, attempt, idempotency_key,
                    checkpoint, result, last_error, safe_to_retry, delivered,
                    created_at, updated_at, payload, case_id, experiment_id, lease_duration_ms,
                    max_concurrent_per_case
             FROM tasks WHERE state IN ('queued', 'retrying') ORDER BY created_at",
        )?;
        let rows = stmt.query_map([], row_to_record)?;
        let mut records = Vec::new();
        for row in rows {
            records.push(row?);
        }
        Ok(records)
    }

    /// Terminal tasks for this parent session whose result has not been
    /// delivered into the conversation yet (P1-02 durable result delivery).
    pub async fn list_undelivered_terminal(
        &self,
        parent_session_id: &str,
    ) -> Result<Vec<TaskRecord>, SessionError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT task_id, parent_session_id, child_session_id, kind, description,
                    state, lease_owner, lease_expires_at, attempt, idempotency_key,
                    checkpoint, result, last_error, safe_to_retry, delivered,
                    created_at, updated_at, payload, case_id, experiment_id, lease_duration_ms,
                    max_concurrent_per_case
             FROM tasks
             WHERE parent_session_id = ?1 AND delivered = 0
               AND state IN ('succeeded', 'failed', 'cancelled')
             ORDER BY created_at",
        )?;
        let rows = stmt.query_map(params![parent_session_id], row_to_record)?;
        let mut records = Vec::new();
        for row in rows {
            records.push(row?);
        }
        Ok(records)
    }

    /// Boot/process identity (P1-02): expire leases whose owner process is
    /// verifiably dead, without waiting for the lease deadline. `is_alive`
    /// receives the pid parsed from the `pid-<pid>-<uuid>` owner string. Rows
    /// whose owner is dead get their expiry stamped in the past so the regular
    /// `recover_expired_leases` pass dispositions them (Recovering or
    /// ManualRecoveryRequired); anything already expired or owned by a live
    /// process is untouched. Returns how many leases were expired this way.
    pub async fn expire_leases_of_dead_owners(
        &self,
        is_alive: &(dyn Fn(i32) -> bool + Send + Sync),
    ) -> Result<usize, SessionError> {
        let dead: Vec<(String, String)> = {
            let conn = self.conn.lock().await;
            let mut stmt = conn.prepare(
                "SELECT task_id, lease_owner FROM tasks
                 WHERE state IN ('running', 'recovering') AND lease_owner IS NOT NULL",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            let mut dead = Vec::new();
            for row in rows {
                let (task_id, owner) = row?;
                match owner_pid(&owner) {
                    // Unknown owner format: leave it to lease-expiry recovery.
                    Some(pid) if !is_alive(pid) => dead.push((task_id, owner)),
                    _ => {}
                }
            }
            dead
        };
        if dead.is_empty() {
            return Ok(0);
        }
        let dead_for_retry = dead.clone();
        self.write_contention
            .with_db_retry(|| {
                let dead = dead_for_retry.clone();
                async move {
                    let mut conn = self.conn.lock().await;
                    let tx = conn.transaction()?;
                    let mut expired = 0usize;
                    for (task_id, owner) in &dead {
                        // Guard on the exact owner: a task reclaimed and
                        // re-leased between the scan and this write must not
                        // have its fresh lease expired underneath it.
                        expired += tx.execute(
                            "UPDATE tasks SET lease_expires_at = ?3, updated_at = ?4
                             WHERE task_id = ?1 AND lease_owner = ?2
                               AND state IN ('running', 'recovering')",
                            params![
                                task_id,
                                owner,
                                "1970-01-01T00:00:00Z",
                                Utc::now().to_rfc3339()
                            ],
                        )?;
                    }
                    tx.commit()?;
                    Ok::<_, rusqlite::Error>(expired)
                }
            })
            .await
            .map_err(SessionError::from)
    }

    /// Shared helper for the lease-conditional updates: ?1 = task_id,
    /// ?2 = this store's owner id, ?3 = new expiry, ?4 = updated_at.
    async fn update_where(
        &self,
        sql: &str,
        task_id: &str,
        lease_secs: i64,
    ) -> Result<usize, SessionError> {
        let task_id_owned = task_id.to_string();
        let owner = self.owner_id.clone();
        let expiry = (Utc::now() + chrono::Duration::seconds(lease_secs)).to_rfc3339();
        self.write_contention
            .with_db_retry(|| {
                let task_id = task_id_owned.clone();
                let owner = owner.clone();
                let expiry = expiry.clone();
                async move {
                    let conn = self.conn.lock().await;
                    let changed = conn.execute(
                        sql,
                        params![task_id, owner, expiry, Utc::now().to_rfc3339()],
                    )?;
                    Ok::<_, rusqlite::Error>(changed)
                }
            })
            .await
            .map_err(SessionError::from)
    }

    async fn transition_terminal(
        &self,
        task_id: &str,
        fencing: u32,
        new_state: &'static str,
        result: Option<String>,
        error: Option<String>,
    ) -> Result<bool, SessionError> {
        let task_id_owned = task_id.to_string();
        let owner = self.owner_id.clone();
        let changed = self
            .write_contention
            .with_db_retry(|| {
                let task_id = task_id_owned.clone();
                let owner = owner.clone();
                let result = result.clone();
                let error = error.clone();
                async move {
                    let conn = self.conn.lock().await;
                    // Terminal writes only land from the attempt that still owns
                    // the lease (task id + owner + fencing token): a completion
                    // arriving after a cancel/restart/reclaim must not resurrect
                    // the task or overwrite the state that won the race.
                    let changed = conn.execute(
                        "UPDATE tasks SET state = ?2, result = ?3, last_error = ?4,
                                lease_owner = NULL, lease_expires_at = NULL, updated_at = ?5
                         WHERE task_id = ?1 AND state = 'running'
                           AND lease_owner = ?6 AND attempt = ?7",
                        params![
                            task_id,
                            new_state,
                            result,
                            error,
                            Utc::now().to_rfc3339(),
                            owner,
                            fencing
                        ],
                    )?;
                    Ok::<_, rusqlite::Error>(changed)
                }
            })
            .await?;
        Ok(changed == 1)
    }
}

/// Sink wiring the in-memory background registry into this store (AGT-007).
/// The spawned subagent tool calls these around the detached task's lifetime.
/// Every write after `task_started` is fenced by the attempt token returned
/// there: a worker whose lease was reclaimed loses the race on every write.
#[async_trait]
impl DurableTaskSink for TaskStore {
    async fn task_started(&self, start: DurableTaskStart) -> Result<u64, String> {
        let record = if let Some(assignment) = &start.experiment {
            self.enqueue_experiment_task(&start, assignment)
                .await
                .map_err(|error| error.to_string())?
        } else {
            self.enqueue(NewTask {
                task_id: start.task_id.clone(),
                parent_session_id: start.parent_session_id,
                kind: "subagent".into(),
                description: start.description,
                idempotency_key: start.idempotency_key,
                safe_to_retry: start.safe_to_retry,
                payload: start.payload,
            })
            .await
            .map_err(|e| e.to_string())?
        };
        // The enqueue may have returned a pre-existing (idempotent) record;
        // only lease it if it is actually ours to run.
        let leased = self
            .acquire_lease_with_duration_ms(&record.task_id, record.lease_duration_ms)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| {
                format!(
                    "task '{}' is not leasable (state {:?}); refusing duplicate execution",
                    record.task_id, record.state
                )
            })?;
        Ok(leased.attempt as u64)
    }

    async fn task_completed(
        &self,
        task_id: &str,
        fencing: u64,
        result: &Result<String, String>,
    ) -> Result<(), String> {
        let fencing = fencing as u32;
        let changed = match result {
            Ok(output) => self.complete(task_id, fencing, output).await,
            Err(error) if error.starts_with("cancelled") => {
                self.cancel_attempt(task_id, fencing).await
            }
            Err(error) => {
                // A subagent failure observed in-process is permanent for this
                // attempt; whether a later retry happens is recovery's call.
                self.fail(task_id, fencing, error, false).await
            }
        }
        .map_err(|e| e.to_string())?;
        if changed {
            Ok(())
        } else {
            Err("durable task completion was fenced out".into())
        }
    }

    async fn task_heartbeat(&self, task_id: &str, fencing: u64) -> Result<bool, String> {
        self.heartbeat(task_id, fencing as u32)
            .await
            .map_err(|e| e.to_string())
    }

    async fn task_attached_session(
        &self,
        task_id: &str,
        fencing: u64,
        child_session_id: &str,
    ) -> Result<(), String> {
        self.set_child_session(task_id, fencing as u32, child_session_id)
            .await
            .map_err(|e| e.to_string())?
            .then_some(())
            .ok_or_else(|| "child-session attachment was fenced out".to_string())
    }
}

fn row_to_record(row: &rusqlite::Row) -> Result<TaskRecord, rusqlite::Error> {
    let state_str: String = row.get(5)?;
    let lease_expires: Option<String> = row.get(7)?;
    let attempt: i64 = row.get(8)?;
    let safe_to_retry: i64 = row.get(13)?;
    let delivered: i64 = row.get(14)?;
    let created_at: String = row.get(15)?;
    let updated_at: String = row.get(16)?;
    Ok(TaskRecord {
        task_id: row.get(0)?,
        parent_session_id: row.get(1)?,
        child_session_id: row.get(2)?,
        kind: row.get(3)?,
        description: row.get(4)?,
        state: TaskStatus::from_str(&state_str).unwrap_or(TaskStatus::Failed),
        lease_owner: row.get(6)?,
        lease_expires_at: lease_expires.and_then(|s| parse_rfc3339(&s)),
        attempt: attempt.max(0) as u32,
        idempotency_key: row.get(9)?,
        checkpoint: row.get(10)?,
        result: row.get(11)?,
        last_error: row.get(12)?,
        safe_to_retry: safe_to_retry != 0,
        delivered: delivered != 0,
        created_at: parse_rfc3339(&created_at).unwrap_or_else(Utc::now),
        updated_at: parse_rfc3339(&updated_at).unwrap_or_else(Utc::now),
        payload: row.get(17)?,
        case_id: row.get(18)?,
        experiment_id: row.get(19)?,
        lease_duration_ms: row.get::<_, i64>(20)?.max(1) as u64,
        max_concurrent_per_case: row.get::<_, i64>(21)?.max(1) as usize,
    })
}

/// Parse the pid out of a `pid-<pid>-<uuid>` lease owner string. Unknown
/// formats (older rows, foreign writers) return None and are left to
/// lease-expiry recovery.
fn owner_pid(owner: &str) -> Option<i32> {
    owner.strip_prefix("pid-")?.split('-').next()?.parse().ok()
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}
