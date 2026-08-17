//! SQLite-backed case ledger event store.
//!
//! Appends use case-level optimistic concurrency and command-level
//! idempotency. Proposed events are replayed through the same pure reducer used
//! on load before the transaction is allowed to commit.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use holmes_core::ledger::{
    apply_ledger_event, ActionBinding, AggregateKind, CaseId, CaseLedgerEvent, EvidenceBinding,
    EvidenceKind, EvidenceRecord, LedgerEvent, LedgerReduceError, LedgerSnapshot,
    ToolOutcomeReceipt, UnstoredLedgerEvent, LEDGER_EVENT_SCHEMA_VERSION,
};
use holmes_core::{Event, ToolOutcomeStatus};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::BTreeSet;

use crate::db::SessionDB;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppendResult {
    pub previous_version: u64,
    pub version: u64,
    pub events: Vec<CaseLedgerEvent>,
    pub idempotent_replay: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceReceiptResult {
    pub session_event_index: u64,
    pub evidence: EvidenceRecord,
    pub ledger_version: u64,
    pub idempotent_replay: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotWriteResult {
    pub written: bool,
    pub projected_seq: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum LedgerStoreError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("case not found: {0}")]
    CaseNotFound(CaseId),
    #[error("session not found: {0}")]
    SessionNotFound(String),
    #[error("ledger version conflict for {case_id}: expected {expected}, actual {actual}")]
    VersionConflict {
        case_id: CaseId,
        expected: u64,
        actual: u64,
    },
    #[error("command {command_id} was already used with a different payload")]
    IdempotencyConflict { command_id: String },
    #[error("ledger event id is already in use: {0}")]
    EventIdConflict(String),
    #[error("invalid ledger event: {0}")]
    InvalidEvent(#[from] LedgerReduceError),
    #[error("invalid stored ledger event: {0}")]
    InvalidStoredEvent(String),
    #[error("ledger append requires a non-empty command id and at least one event")]
    EmptyAppend,
}

#[async_trait]
pub trait CaseLedgerStore: Send + Sync {
    async fn case_id_for_session(&self, session_id: &str) -> Result<CaseId, LedgerStoreError>;

    async fn load(&self, case_id: &CaseId) -> Result<LedgerSnapshot, LedgerStoreError>;

    /// Persist a checksum-protected projection once at least `minimum_events`
    /// new events have accumulated since the previous snapshot. A concurrent
    /// append makes the write a harmless no-op; the next refresh retries it.
    async fn compact_snapshot(
        &self,
        case_id: &CaseId,
        minimum_events: u64,
    ) -> Result<SnapshotWriteResult, LedgerStoreError>;

    async fn append(
        &self,
        case_id: &CaseId,
        expected_version: u64,
        command_id: &str,
        events: Vec<UnstoredLedgerEvent>,
    ) -> Result<AppendResult, LedgerStoreError>;

    /// Atomically persist the authoritative session ToolResult and its bounded
    /// case Evidence receipt. Unlike cognitive appends this always reads the
    /// current case version inside the transaction: an observation that already
    /// happened is never discarded because another agent advanced the Ledger.
    async fn record_evidence(
        &self,
        actor_session_id: &str,
        session_event: Event,
        binding: ActionBinding,
        receipt: ToolOutcomeReceipt,
    ) -> Result<EvidenceReceiptResult, LedgerStoreError>;
}

pub(crate) fn aggregate_kind_str(kind: &AggregateKind) -> &'static str {
    match kind {
        AggregateKind::Hypothesis => "hypothesis",
        AggregateKind::Prediction => "prediction",
        AggregateKind::Experiment => "experiment",
        AggregateKind::Evidence => "evidence",
        AggregateKind::EvidenceLink => "evidence_link",
        AggregateKind::Resolution => "resolution",
        AggregateKind::Deliberation => "deliberation",
    }
}

fn parse_aggregate_kind(value: &str) -> Result<AggregateKind, LedgerStoreError> {
    match value {
        "hypothesis" => Ok(AggregateKind::Hypothesis),
        "prediction" => Ok(AggregateKind::Prediction),
        "experiment" => Ok(AggregateKind::Experiment),
        "evidence" => Ok(AggregateKind::Evidence),
        "evidence_link" => Ok(AggregateKind::EvidenceLink),
        "resolution" => Ok(AggregateKind::Resolution),
        "deliberation" => Ok(AggregateKind::Deliberation),
        other => Err(LedgerStoreError::InvalidStoredEvent(format!(
            "unknown aggregate kind {other}"
        ))),
    }
}

pub(crate) fn ledger_event_type(event: &LedgerEvent) -> &'static str {
    match event {
        LedgerEvent::HypothesisProposedV2 { .. } => "hypothesis_proposed_v2",
        LedgerEvent::PredictionDeclaredV2 { .. } => "prediction_declared_v2",
        LedgerEvent::ExperimentPlannedV2 { .. } => "experiment_planned_v2",
        LedgerEvent::ExperimentQueuedV2 { .. } => "experiment_queued_v2",
        LedgerEvent::ExperimentStartedV2 { .. } => "experiment_started_v2",
        LedgerEvent::ExperimentObservedV2 { .. } => "experiment_observed_v2",
        LedgerEvent::ExperimentBlockedV2 { .. } => "experiment_blocked_v2",
        LedgerEvent::ExperimentFailedV2 { .. } => "experiment_failed_v2",
        LedgerEvent::ExperimentCancelledV2 { .. } => "experiment_cancelled_v2",
        LedgerEvent::ExperimentExpiredV2 { .. } => "experiment_expired_v2",
        LedgerEvent::EvidenceRecordedV2 { .. } => "evidence_recorded_v2",
        LedgerEvent::EvidenceLinkedV2 { .. } => "evidence_linked_v2",
        LedgerEvent::EvidenceLinkRejectedV2 { .. } => "evidence_link_rejected_v2",
        LedgerEvent::ContradictionDetectedV2 { .. } => "contradiction_detected_v2",
        LedgerEvent::ResolutionRequestedV2 { .. } => "resolution_requested_v2",
        LedgerEvent::HypothesisResolvedV2 { .. } => "hypothesis_resolved_v2",
        LedgerEvent::ResolutionRejectedV2 { .. } => "resolution_rejected_v2",
        LedgerEvent::HypothesisReopenedV2 { .. } => "hypothesis_reopened_v2",
        LedgerEvent::HypothesisSupersededV2 { .. } => "hypothesis_superseded_v2",
        LedgerEvent::DeliberationCommittedV2 { .. } => "deliberation_committed_v2",
    }
}

fn parse_timestamp(value: String) -> Result<DateTime<Utc>, LedgerStoreError> {
    DateTime::parse_from_rfc3339(&value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| LedgerStoreError::InvalidStoredEvent(error.to_string()))
}

pub(crate) fn load_events_from_connection(
    conn: &Connection,
    case_id: &CaseId,
    range: Option<(u64, u64)>,
) -> Result<Vec<CaseLedgerEvent>, LedgerStoreError> {
    let (sql, lower, upper) = if let Some((first, count)) = range {
        (
            "SELECT seq, event_id, aggregate_kind, aggregate_id, aggregate_revision,
                    actor_session_id, event_data, created_at
             FROM case_ledger_events
             WHERE case_id = ?1 AND seq >= ?2 AND seq < ?3 ORDER BY seq",
            first as i64,
            first.saturating_add(count) as i64,
        )
    } else {
        (
            "SELECT seq, event_id, aggregate_kind, aggregate_id, aggregate_revision,
                    actor_session_id, event_data, created_at
             FROM case_ledger_events
             WHERE case_id = ?1 AND seq >= ?2 AND seq < ?3 ORDER BY seq",
            0,
            i64::MAX,
        )
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![case_id.as_str(), lower, upper], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, String>(7)?,
        ))
    })?;

    let mut events = Vec::new();
    for row in rows {
        let (seq, event_id, kind, aggregate_id, revision, actor, data, created_at) = row?;
        let event: LedgerEvent = serde_json::from_str(&data)?;
        events.push(CaseLedgerEvent {
            case_id: case_id.clone(),
            seq: seq as u64,
            event_id,
            aggregate_kind: parse_aggregate_kind(&kind)?,
            aggregate_id,
            aggregate_revision: revision as u64,
            actor_session_id: actor,
            event,
            created_at: parse_timestamp(created_at)?,
        });
    }
    Ok(events)
}

pub(crate) fn project_events(
    case_id: &CaseId,
    events: &[CaseLedgerEvent],
) -> Result<LedgerSnapshot, LedgerStoreError> {
    let mut snapshot = LedgerSnapshot::empty(case_id.clone());
    for event in events {
        apply_ledger_event(&mut snapshot, event)?;
    }
    Ok(snapshot)
}

#[async_trait]
impl CaseLedgerStore for SessionDB {
    async fn case_id_for_session(&self, session_id: &str) -> Result<CaseId, LedgerStoreError> {
        let conn = self.conn.lock().await;
        conn.query_row(
            "SELECT case_id FROM sessions WHERE id = ?1",
            params![session_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .map(CaseId::new)
        .ok_or_else(|| LedgerStoreError::SessionNotFound(session_id.to_owned()))
    }

    async fn load(&self, case_id: &CaseId) -> Result<LedgerSnapshot, LedgerStoreError> {
        let conn = self.conn.lock().await;
        let durable_version = conn
            .query_row(
                "SELECT ledger_version FROM cases WHERE case_id = ?1",
                params![case_id.as_str()],
                |row| row.get::<_, i64>(0).map(|value| value as u64),
            )
            .optional()?
            .ok_or_else(|| LedgerStoreError::CaseNotFound(case_id.clone()))?;

        let stored_snapshot = conn
            .query_row(
                "SELECT projected_seq, state_json, checksum
                 FROM case_ledger_snapshots WHERE case_id = ?1",
                params![case_id.as_str()],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)? as u64,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        let had_stored_snapshot = stored_snapshot.is_some();
        let mut used_snapshot = false;
        let snapshot = if let Some((projected_seq, state_json, checksum)) = stored_snapshot {
            let parsed = if holmes_core::content_hash(&state_json) == checksum {
                serde_json::from_str::<LedgerSnapshot>(&state_json).ok()
            } else {
                None
            };
            if let Some(mut snapshot) = parsed.filter(|snapshot| {
                snapshot.case_id == *case_id
                    && snapshot.projected_seq == projected_seq
                    && snapshot.version == projected_seq
                    && projected_seq <= durable_version
            }) {
                let tail_count = durable_version.saturating_sub(projected_seq);
                let tail = load_events_from_connection(
                    &conn,
                    case_id,
                    Some((projected_seq.saturating_add(1), tail_count)),
                )?;
                let tail_valid = tail
                    .iter()
                    .try_for_each(|event| apply_ledger_event(&mut snapshot, event))
                    .is_ok();
                if tail_valid && snapshot.version == durable_version {
                    used_snapshot = true;
                    snapshot
                } else {
                    project_events(case_id, &load_events_from_connection(&conn, case_id, None)?)?
                }
            } else {
                project_events(case_id, &load_events_from_connection(&conn, case_id, None)?)?
            }
        } else {
            project_events(case_id, &load_events_from_connection(&conn, case_id, None)?)?
        };
        if had_stored_snapshot && !used_snapshot {
            holmes_core::metrics::metrics().count("ledger.snapshot_fallback");
            tracing::warn!(case_id = %case_id, "Ledger snapshot validation failed; replayed full event stream");
        }
        if snapshot.version != durable_version {
            return Err(LedgerStoreError::InvalidStoredEvent(format!(
                "case {} records version {}, projection rebuilt version {}",
                case_id, durable_version, snapshot.version
            )));
        }
        Ok(snapshot)
    }

    async fn compact_snapshot(
        &self,
        case_id: &CaseId,
        minimum_events: u64,
    ) -> Result<SnapshotWriteResult, LedgerStoreError> {
        let snapshot = self.load(case_id).await?;
        let state_json = serde_json::to_string(&snapshot)?;
        let checksum = holmes_core::content_hash(&state_json);
        let case_id_owned = case_id.clone();
        let projected_seq = snapshot.projected_seq;
        let minimum_events = minimum_events.max(1);
        let nested = self
            .write_contention
            .with_db_retry(|| {
                let case_id = case_id_owned.clone();
                let state_json = state_json.clone();
                let checksum = checksum.clone();
                async move {
                    let mut conn = self.conn.lock().await;
                    let tx = conn.transaction()?;
                    let durable_version = tx
                        .query_row(
                            "SELECT ledger_version FROM cases WHERE case_id = ?1",
                            params![case_id.as_str()],
                            |row| row.get::<_, i64>(0).map(|value| value as u64),
                        )
                        .optional()?;
                    let Some(durable_version) = durable_version else {
                        return Ok::<_, rusqlite::Error>(Err(LedgerStoreError::CaseNotFound(
                            case_id,
                        )));
                    };
                    let previous = tx
                        .query_row(
                            "SELECT projected_seq, state_json, checksum
                             FROM case_ledger_snapshots WHERE case_id = ?1",
                            params![case_id.as_str()],
                            |row| {
                                Ok((
                                    row.get::<_, i64>(0)? as u64,
                                    row.get::<_, String>(1)?,
                                    row.get::<_, String>(2)?,
                                ))
                            },
                        )
                        .optional()?;
                    let previous_seq = previous
                        .as_ref()
                        .map(|(projected_seq, _, _)| *projected_seq)
                        .unwrap_or(0);
                    let previous_valid = previous.as_ref().is_some_and(
                        |(stored_seq, stored_json, stored_checksum)| {
                            holmes_core::content_hash(stored_json) == *stored_checksum
                                && serde_json::from_str::<LedgerSnapshot>(stored_json)
                                    .ok()
                                    .is_some_and(|stored| {
                                        stored.case_id.as_str() == case_id.as_str()
                                            && stored.projected_seq == *stored_seq
                                            && stored.version == *stored_seq
                                    })
                        },
                    );
                    if durable_version != projected_seq
                        || (previous_valid
                            && projected_seq.saturating_sub(previous_seq) < minimum_events)
                    {
                        tx.commit()?;
                        return Ok(Ok(SnapshotWriteResult {
                            written: false,
                            projected_seq: previous_seq,
                        }));
                    }
                    tx.execute(
                        "INSERT INTO case_ledger_snapshots
                         (case_id, projected_seq, state_json, checksum, updated_at)
                         VALUES (?1, ?2, ?3, ?4, ?5)
                         ON CONFLICT(case_id) DO UPDATE SET
                           projected_seq = excluded.projected_seq,
                           state_json = excluded.state_json,
                           checksum = excluded.checksum,
                           updated_at = excluded.updated_at",
                        params![
                            case_id.as_str(),
                            projected_seq as i64,
                            state_json,
                            checksum,
                            Utc::now().to_rfc3339(),
                        ],
                    )?;
                    tx.commit()?;
                    Ok(Ok(SnapshotWriteResult {
                        written: true,
                        projected_seq,
                    }))
                }
            })
            .await?;
        let result = nested?;
        if result.written {
            holmes_core::metrics::metrics().count("ledger.snapshot_written");
        }
        Ok(result)
    }

    async fn append(
        &self,
        case_id: &CaseId,
        expected_version: u64,
        command_id: &str,
        events: Vec<UnstoredLedgerEvent>,
    ) -> Result<AppendResult, LedgerStoreError> {
        if command_id.trim().is_empty() || events.is_empty() {
            return Err(LedgerStoreError::EmptyAppend);
        }
        let mut ids = BTreeSet::new();
        for event in &events {
            if event.event_id.trim().is_empty() || !ids.insert(event.event_id.clone()) {
                return Err(LedgerStoreError::EventIdConflict(event.event_id.clone()));
            }
        }

        let payload = serde_json::to_string(&events)?;
        let payload_hash = holmes_core::content_hash(&payload);
        let case_id_owned = case_id.clone();
        let command_id_owned = command_id.to_owned();
        let events_for_retry = events.clone();

        let nested = self
            .write_contention
            .with_db_retry(|| {
                let case_id = case_id_owned.clone();
                let command_id = command_id_owned.clone();
                let payload_hash = payload_hash.clone();
                let proposed = events_for_retry.clone();
                async move {
                    let mut conn = self.conn.lock().await;
                    let tx = conn.transaction()?;

                    let existing_command = tx
                        .query_row(
                            "SELECT payload_hash, first_seq, event_count
                             FROM case_ledger_commands
                             WHERE case_id = ?1 AND command_id = ?2",
                            params![case_id.as_str(), command_id],
                            |row| {
                                Ok((
                                    row.get::<_, String>(0)?,
                                    row.get::<_, i64>(1)? as u64,
                                    row.get::<_, i64>(2)? as u64,
                                ))
                            },
                        )
                        .optional()?;
                    if let Some((stored_hash, first_seq, event_count)) = existing_command {
                        if stored_hash != payload_hash {
                            return Ok::<_, rusqlite::Error>(Err(
                                LedgerStoreError::IdempotencyConflict { command_id },
                            ));
                        }
                        let stored = match load_events_from_connection(
                            &tx,
                            &case_id,
                            Some((first_seq, event_count)),
                        ) {
                            Ok(events) => events,
                            Err(error) => return Ok(Err(error)),
                        };
                        let version = first_seq + event_count - 1;
                        tx.commit()?;
                        return Ok(Ok(AppendResult {
                            previous_version: first_seq - 1,
                            version,
                            events: stored,
                            idempotent_replay: true,
                        }));
                    }

                    let actual_version = match tx
                        .query_row(
                            "SELECT ledger_version FROM cases WHERE case_id = ?1",
                            params![case_id.as_str()],
                            |row| row.get::<_, i64>(0).map(|value| value as u64),
                        )
                        .optional()?
                    {
                        Some(version) => version,
                        None => {
                            return Ok(Err(LedgerStoreError::CaseNotFound(case_id.clone())));
                        }
                    };
                    if actual_version != expected_version {
                        return Ok(Err(LedgerStoreError::VersionConflict {
                            case_id: case_id.clone(),
                            expected: expected_version,
                            actual: actual_version,
                        }));
                    }

                    let existing = match load_events_from_connection(&tx, &case_id, None) {
                        Ok(events) => events,
                        Err(error) => return Ok(Err(error)),
                    };
                    let mut projection = match project_events(&case_id, &existing) {
                        Ok(snapshot) => snapshot,
                        Err(error) => return Ok(Err(error)),
                    };

                    let first_seq = actual_version + 1;
                    let mut stored = Vec::with_capacity(proposed.len());
                    for (offset, event) in proposed.iter().enumerate() {
                        let envelope = CaseLedgerEvent {
                            case_id: case_id.clone(),
                            seq: first_seq + offset as u64,
                            event_id: event.event_id.clone(),
                            aggregate_kind: event.aggregate_kind.clone(),
                            aggregate_id: event.aggregate_id.clone(),
                            aggregate_revision: event.aggregate_revision,
                            actor_session_id: event.actor_session_id.clone(),
                            event: event.event.clone(),
                            created_at: event.created_at,
                        };
                        if let Err(error) = apply_ledger_event(&mut projection, &envelope) {
                            return Ok(Err(LedgerStoreError::InvalidEvent(error)));
                        }
                        let duplicate = tx
                            .query_row(
                                "SELECT 1 FROM case_ledger_events WHERE event_id = ?1",
                                params![envelope.event_id],
                                |_| Ok(()),
                            )
                            .optional()?
                            .is_some();
                        if duplicate {
                            return Ok(Err(LedgerStoreError::EventIdConflict(
                                envelope.event_id,
                            )));
                        }
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
                            Utc::now().to_rfc3339(),
                        ],
                    )?;
                    for event in &stored {
                        let event_data = match serde_json::to_string(&event.event) {
                            Ok(data) => data,
                            Err(error) => {
                                return Ok(Err(LedgerStoreError::Serialization(error)));
                            }
                        };
                        tx.execute(
                            "INSERT INTO case_ledger_events
                             (case_id, seq, event_id, command_id, aggregate_kind,
                              aggregate_id, aggregate_revision, actor_session_id,
                              event_type, event_data, created_at)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                            params![
                                case_id.as_str(),
                                event.seq as i64,
                                event.event_id,
                                command_id,
                                aggregate_kind_str(&event.aggregate_kind),
                                event.aggregate_id,
                                event.aggregate_revision as i64,
                                event.actor_session_id,
                                ledger_event_type(&event.event),
                                event_data,
                                event.created_at.to_rfc3339(),
                            ],
                        )?;
                    }
                    tx.execute(
                        "UPDATE cases SET ledger_version = ?2, updated_at = ?3 WHERE case_id = ?1",
                        params![
                            case_id.as_str(),
                            projection.version as i64,
                            Utc::now().to_rfc3339()
                        ],
                    )?;
                    tx.commit()?;
                    Ok(Ok(AppendResult {
                        previous_version: actual_version,
                        version: projection.version,
                        events: stored,
                        idempotent_replay: false,
                    }))
                }
            })
            .await?;
        nested
    }

    async fn record_evidence(
        &self,
        actor_session_id: &str,
        session_event: Event,
        binding: ActionBinding,
        receipt: ToolOutcomeReceipt,
    ) -> Result<EvidenceReceiptResult, LedgerStoreError> {
        let Event::ToolResult {
            success,
            outcome,
            call_id,
            ..
        } = &session_event
        else {
            return Err(LedgerStoreError::InvalidStoredEvent(
                "evidence receipt requires a ToolResult session event".into(),
            ));
        };
        if call_id.as_deref() != Some(binding.tool_call_id.as_str())
            || outcome != &Some(receipt.outcome_status)
            || *success != receipt.outcome_status.is_success()
        {
            return Err(LedgerStoreError::InvalidStoredEvent(
                "ToolResult, ActionBinding and ToolOutcomeReceipt disagree".into(),
            ));
        }
        match receipt.outcome_status {
            ToolOutcomeStatus::Succeeded if receipt.kind == EvidenceKind::AuditOutcome => {
                return Err(LedgerStoreError::InvalidStoredEvent(
                    "successful receipt cannot be audit-only evidence".into(),
                ));
            }
            ToolOutcomeStatus::Failed | ToolOutcomeStatus::TimedOut
                if binding.experiment_id.is_none()
                    || receipt.kind != EvidenceKind::AuditOutcome =>
            {
                return Err(LedgerStoreError::InvalidStoredEvent(
                    "failed/timeout evidence requires an experiment binding and audit outcome"
                        .into(),
                ));
            }
            ToolOutcomeStatus::Cancelled | ToolOutcomeStatus::Denied => {
                return Err(LedgerStoreError::InvalidStoredEvent(
                    "cancelled/denied calls do not create linkable evidence".into(),
                ));
            }
            _ => {}
        }

        let command_id = format!(
            "receipt:{}:{}:{}",
            actor_session_id, binding.tool_call_id, binding.attempt
        );
        let payload = serde_json::to_string(&(&session_event, &binding, &receipt))?;
        let payload_hash = holmes_core::content_hash(&payload);

        let mut processed_event = session_event.clone();
        let mut prepared_blob = None;
        if let Event::ToolResult {
            ref mut content, ..
        } = processed_event
        {
            if content.len() > crate::blob_store::BLOB_OFFLOAD_THRESHOLD {
                let blob = crate::blob_store::prepare(content);
                *content = crate::blob_store::reference_for(&blob.sha256);
                prepared_blob = Some(blob);
            }
        }
        let event_type = crate::db::event_type_str(&processed_event).to_owned();
        let event_data = crate::db::serialize_event_for_storage(&processed_event)?;

        let actor_owned = actor_session_id.to_owned();
        let binding_for_retry = binding.clone();
        let receipt_for_retry = receipt.clone();
        let command_for_retry = command_id.clone();
        let payload_hash_for_retry = payload_hash.clone();
        let event_type_for_retry = event_type.clone();
        let event_data_for_retry = event_data.clone();
        let blob_for_retry = prepared_blob.clone();

        let nested = self
            .write_contention
            .with_db_retry(|| {
                let actor = actor_owned.clone();
                let binding = binding_for_retry.clone();
                let receipt = receipt_for_retry.clone();
                let command_id = command_for_retry.clone();
                let payload_hash = payload_hash_for_retry.clone();
                let event_type = event_type_for_retry.clone();
                let event_data = event_data_for_retry.clone();
                let blob = blob_for_retry.clone();
                async move {
                    let mut conn = self.conn.lock().await;
                    let tx = conn.transaction()?;

                    let session_case = tx
                        .query_row(
                            "SELECT case_id FROM sessions WHERE id = ?1",
                            params![actor],
                            |row| row.get::<_, String>(0),
                        )
                        .optional()?;
                    let Some(session_case) = session_case else {
                        return Ok::<_, rusqlite::Error>(Err(LedgerStoreError::SessionNotFound(
                            actor,
                        )));
                    };
                    if session_case != binding.case_id.as_str() {
                        return Ok(Err(LedgerStoreError::InvalidStoredEvent(
                            "session and evidence binding belong to different cases".into(),
                        )));
                    }

                    let existing = tx
                        .query_row(
                            "SELECT payload_hash, first_seq, event_count, session_event_index
                             FROM case_ledger_commands
                             WHERE case_id = ?1 AND command_id = ?2",
                            params![binding.case_id.as_str(), command_id],
                            |row| {
                                Ok((
                                    row.get::<_, String>(0)?,
                                    row.get::<_, i64>(1)? as u64,
                                    row.get::<_, i64>(2)? as u64,
                                    row.get::<_, Option<i64>>(3)?.map(|value| value as u64),
                                ))
                            },
                        )
                        .optional()?;
                    if let Some((stored_hash, first_seq, event_count, session_index)) = existing {
                        if stored_hash != payload_hash {
                            return Ok(Err(LedgerStoreError::IdempotencyConflict { command_id }));
                        }
                        let events = match load_events_from_connection(
                            &tx,
                            &binding.case_id,
                            Some((first_seq, event_count)),
                        ) {
                            Ok(events) => events,
                            Err(error) => return Ok(Err(error)),
                        };
                        let evidence = events.into_iter().find_map(|event| match event.event {
                            LedgerEvent::EvidenceRecordedV2 { evidence, .. } => Some(evidence),
                            _ => None,
                        });
                        let (Some(evidence), Some(session_event_index)) = (evidence, session_index)
                        else {
                            return Ok(Err(LedgerStoreError::InvalidStoredEvent(
                                "evidence receipt command is missing its durable outputs".into(),
                            )));
                        };
                        let ledger_version: u64 = tx.query_row(
                            "SELECT ledger_version FROM cases WHERE case_id = ?1",
                            params![binding.case_id.as_str()],
                            |row| row.get::<_, i64>(0).map(|value| value as u64),
                        )?;
                        tx.commit()?;
                        return Ok(Ok(EvidenceReceiptResult {
                            session_event_index,
                            evidence,
                            ledger_version,
                            idempotent_replay: true,
                        }));
                    }

                    let (ledger_version, next_evidence_seq): (u64, u64) = match tx
                        .query_row(
                            "SELECT ledger_version, next_evidence_seq
                             FROM cases WHERE case_id = ?1",
                            params![binding.case_id.as_str()],
                            |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64)),
                        )
                        .optional()?
                    {
                        Some(row) => row,
                        None => {
                            return Ok(Err(LedgerStoreError::CaseNotFound(
                                binding.case_id.clone(),
                            )));
                        }
                    };
                    let existing_events =
                        match load_events_from_connection(&tx, &binding.case_id, None) {
                            Ok(events) => events,
                            Err(error) => return Ok(Err(error)),
                        };
                    let mut projection = match project_events(&binding.case_id, &existing_events) {
                        Ok(snapshot) => snapshot,
                        Err(error) => return Ok(Err(error)),
                    };

                    let evidence = EvidenceRecord {
                        id: format!("ev-{next_evidence_seq}"),
                        binding: EvidenceBinding {
                            case_id: binding.case_id.clone(),
                            contract_id: binding.contract_id.clone(),
                            requirement_ids: binding.requirement_ids.clone(),
                            experiment_id: binding.experiment_id.clone(),
                            prediction_ids: binding.prediction_ids.clone(),
                            tool_call_id: Some(binding.tool_call_id.clone()),
                        },
                        source_session_id: actor.clone(),
                        tool: receipt.tool.clone(),
                        tool_call_id: Some(binding.tool_call_id.clone()),
                        outcome_status: receipt.outcome_status,
                        exit_code: receipt.exit_code,
                        kind: receipt.kind.clone(),
                        input_summary: receipt.input_summary.clone(),
                        output_hash: receipt.output_hash.clone(),
                        output_snippet: receipt.output_snippet.clone(),
                        predicate: receipt.predicate.clone(),
                        verified_by: receipt.verified_by.clone(),
                        recorded_at: receipt.recorded_at,
                    };
                    let ledger_event = CaseLedgerEvent {
                        case_id: binding.case_id.clone(),
                        seq: ledger_version + 1,
                        event_id: format!("event-{}", uuid::Uuid::new_v4()),
                        aggregate_kind: AggregateKind::Evidence,
                        aggregate_id: evidence.id.clone(),
                        aggregate_revision: 1,
                        actor_session_id: actor.clone(),
                        event: LedgerEvent::EvidenceRecordedV2 {
                            schema_version: LEDGER_EVENT_SCHEMA_VERSION,
                            evidence: evidence.clone(),
                        },
                        created_at: receipt.recorded_at,
                    };
                    if let Err(error) = apply_ledger_event(&mut projection, &ledger_event) {
                        return Ok(Err(LedgerStoreError::InvalidEvent(error)));
                    }

                    let session_event_index: u64 = tx
                        .query_row(
                            "SELECT COALESCE(MAX(event_index), -1) + 1
                             FROM events WHERE session_id = ?1",
                            params![actor],
                            |row| row.get(0),
                        )
                        .unwrap_or(0);
                    if let Some(blob) = &blob {
                        crate::blob_store::insert(&tx, blob)?;
                    }
                    tx.execute(
                        "INSERT INTO case_ledger_commands
                         (case_id, command_id, payload_hash, expected_version, first_seq,
                          event_count, created_at, session_event_index)
                         VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7)",
                        params![
                            binding.case_id.as_str(),
                            command_id,
                            payload_hash,
                            ledger_version as i64,
                            ledger_event.seq as i64,
                            receipt.recorded_at.to_rfc3339(),
                            session_event_index as i64,
                        ],
                    )?;
                    tx.execute(
                        "INSERT INTO events
                         (session_id, event_index, event_type, event_data, timestamp)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![
                            actor,
                            session_event_index as i64,
                            event_type,
                            event_data,
                            receipt.recorded_at.to_rfc3339(),
                        ],
                    )?;
                    tx.execute(
                        "INSERT INTO case_ledger_events
                         (case_id, seq, event_id, command_id, aggregate_kind, aggregate_id,
                          aggregate_revision, actor_session_id, event_type, event_data, created_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                        params![
                            binding.case_id.as_str(),
                            ledger_event.seq as i64,
                            ledger_event.event_id,
                            command_id,
                            aggregate_kind_str(&ledger_event.aggregate_kind),
                            ledger_event.aggregate_id,
                            ledger_event.aggregate_revision as i64,
                            ledger_event.actor_session_id,
                            ledger_event_type(&ledger_event.event),
                            serde_json::to_string(&ledger_event.event).map_err(|error| {
                                rusqlite::Error::ToSqlConversionFailure(Box::new(error))
                            })?,
                            ledger_event.created_at.to_rfc3339(),
                        ],
                    )?;
                    tx.execute(
                        "UPDATE cases
                         SET ledger_version = ?2, next_evidence_seq = ?3, updated_at = ?4
                         WHERE case_id = ?1",
                        params![
                            binding.case_id.as_str(),
                            projection.version as i64,
                            next_evidence_seq.saturating_add(1) as i64,
                            receipt.recorded_at.to_rfc3339(),
                        ],
                    )?;
                    tx.commit()?;
                    Ok(Ok(EvidenceReceiptResult {
                        session_event_index,
                        evidence,
                        ledger_version: projection.version,
                        idempotent_replay: false,
                    }))
                }
            })
            .await?;

        let result = nested?;
        if !result.idempotent_replay {
            self.projector
                .project(actor_session_id, result.session_event_index, &event_data)
                .await;
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CreateSessionParams, SessionStore};
    use holmes_core::ledger::{
        ActorId, Experiment, ExperimentId, ExperimentStatus, RiskLevel, VerificationMethod,
    };
    use holmes_core::SessionMode;

    async fn session() -> (SessionDB, String, CaseId) {
        let db = SessionDB::open(":memory:").await.expect("open");
        let id = "receipt-session".to_string();
        db.create_session(CreateSessionParams {
            id: Some(id.clone()),
            title: None,
            mode: Some(SessionMode::Pentest),
            model: None,
            system_prompt: None,
            parent_session_id: None,
            fork_point: None,
            source: Some("test".into()),
            tags: Vec::new(),
        })
        .await
        .expect("session");
        let case_id = db.case_id_for_session(&id).await.expect("case");
        (db, id, case_id)
    }

    fn outcome(call_id: &str, status: ToolOutcomeStatus) -> Event {
        Event::ToolResult {
            name: "execute_command".into(),
            success: status == ToolOutcomeStatus::Succeeded,
            outcome: Some(status),
            content: r#"{"exit_code":0,"stdout":"ok"}"#.into(),
            error: None,
            artifacts: Vec::new(),
            call_id: Some(call_id.into()),
        }
    }

    fn binding(case_id: &CaseId, call_id: &str) -> ActionBinding {
        ActionBinding {
            case_id: case_id.clone(),
            contract_id: Some("contract-1".into()),
            requirement_ids: vec!["req-1".into()],
            experiment_id: None,
            prediction_ids: Vec::new(),
            tool_call_id: call_id.into(),
            attempt: 1,
        }
    }

    fn receipt(status: ToolOutcomeStatus) -> ToolOutcomeReceipt {
        ToolOutcomeReceipt {
            tool: "execute_command".into(),
            outcome_status: status,
            exit_code: Some(0),
            kind: EvidenceKind::Deterministic,
            input_summary: "pwd".into(),
            output_hash: holmes_core::content_hash("ok"),
            output_snippet: "ok".into(),
            predicate: "command completed".into(),
            verified_by: VerificationMethod::Runtime,
            recorded_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn evidence_receipt_is_atomic_monotonic_and_idempotent() {
        let (db, session_id, case_id) = session().await;
        let event = outcome("call-1", ToolOutcomeStatus::Succeeded);
        let first_binding = binding(&case_id, "call-1");
        let first_receipt = receipt(ToolOutcomeStatus::Succeeded);

        let first = db
            .record_evidence(
                &session_id,
                event.clone(),
                first_binding.clone(),
                first_receipt.clone(),
            )
            .await
            .expect("receipt");
        assert_eq!(first.evidence.id, "ev-1");
        assert!(!first.idempotent_replay);
        let replay = db
            .record_evidence(&session_id, event, first_binding, first_receipt)
            .await
            .expect("replay");
        assert!(replay.idempotent_replay);
        assert_eq!(replay.evidence.id, "ev-1");
        assert_eq!(db.get_events(&session_id).await.unwrap().len(), 1);
        assert_eq!(db.load(&case_id).await.unwrap().evidence.len(), 1);

        let second = db
            .record_evidence(
                &session_id,
                outcome("call-2", ToolOutcomeStatus::Succeeded),
                binding(&case_id, "call-2"),
                receipt(ToolOutcomeStatus::Succeeded),
            )
            .await
            .expect("second receipt");
        assert_eq!(second.evidence.id, "ev-2");
        assert_eq!(db.get_events(&session_id).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn failed_outcome_requires_experiment_bound_audit_receipt() {
        let (db, session_id, case_id) = session().await;
        let mut failed_receipt = receipt(ToolOutcomeStatus::Failed);
        failed_receipt.kind = EvidenceKind::AuditOutcome;
        failed_receipt.verified_by = VerificationMethod::DomainValidator;
        let error = db
            .record_evidence(
                &session_id,
                outcome("call-f", ToolOutcomeStatus::Failed),
                binding(&case_id, "call-f"),
                failed_receipt.clone(),
            )
            .await
            .expect_err("unbound failure must fail closed");
        assert!(error.to_string().contains("experiment binding"));
        assert!(db.get_events(&session_id).await.unwrap().is_empty());

        let now = Utc::now();
        let experiment_id = ExperimentId::new("exp-1");
        db.append(
            &case_id,
            0,
            "plan-exp-1",
            vec![UnstoredLedgerEvent {
                event_id: "event-plan-exp-1".into(),
                aggregate_kind: AggregateKind::Experiment,
                aggregate_id: experiment_id.to_string(),
                aggregate_revision: 1,
                actor_session_id: session_id.clone(),
                event: LedgerEvent::ExperimentPlannedV2 {
                    schema_version: LEDGER_EVENT_SCHEMA_VERSION,
                    experiment: Experiment {
                        id: experiment_id.clone(),
                        case_id: case_id.clone(),
                        hypothesis_ids: Vec::new(),
                        prediction_ids: Vec::new(),
                        action: "capture a failed command outcome".into(),
                        expected_observations: Vec::new(),
                        tool_allowlist: vec!["execute_command".into()],
                        risk: RiskLevel::Low,
                        status: ExperimentStatus::Planned,
                        task_id: None,
                        attempt: 0,
                        idempotency_key: "exp-1-key".into(),
                        evidence_ids: Vec::new(),
                        revision: 1,
                        created_by: ActorId::new(&session_id),
                        created_at: now,
                        updated_at: now,
                    },
                },
                created_at: now,
            }],
        )
        .await
        .expect("plan experiment");
        let mut bound = binding(&case_id, "call-f");
        bound.experiment_id = Some(experiment_id);
        let accepted = db
            .record_evidence(
                &session_id,
                outcome("call-f", ToolOutcomeStatus::Failed),
                bound,
                failed_receipt,
            )
            .await
            .expect("bound audit receipt");
        assert_eq!(accepted.evidence.kind, EvidenceKind::AuditOutcome);
    }
}
