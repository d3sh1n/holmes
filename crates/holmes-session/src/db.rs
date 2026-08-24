use crate::store::SessionStore;
use async_trait::async_trait;
use holmes_core::event::{Event, StoredEvent};
use holmes_core::types::*;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::compaction_archive::CompactionArchive;
use crate::schema;
use crate::task_store::TaskStore;
use crate::transcript_projection::TranscriptProjector;
use crate::write_contention::WriteContention;

pub struct SessionDB {
    pub(crate) conn: Arc<Mutex<Connection>>,
    pub(crate) write_contention: WriteContention,
    write_count: Arc<Mutex<u64>>,
    sessions_dir: PathBuf,
    task_store: TaskStore,
    pub(crate) projector: TranscriptProjector,
}

impl SessionDB {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, SessionError> {
        let mut conn = Connection::open(path.as_ref())?;

        // Open-time setup (per-connection pragmas + schema migrations) retries
        // on BUSY/LOCKED with a bounded backoff (P1-04): `PRAGMA
        // journal_mode=WAL` is the one statement that never consults the busy
        // handler — a peer mid-switch makes it fail instantly — and a
        // concurrent opener can hold the migration write lock past the point
        // where this connection's busy timeout started. Retrying the whole
        // setup is safe: the migration body re-checks the version inside its
        // transaction, so a retried pass over a migrated database is a no-op.
        let mut setup_attempt = 0u32;
        loop {
            match setup_connection(&mut conn) {
                Ok(()) => break,
                Err(error)
                    if is_contention_error(&error) && setup_attempt < OPEN_SETUP_MAX_ATTEMPTS =>
                {
                    setup_attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(
                        (5 + u64::from(setup_attempt) * 5).min(50),
                    ));
                }
                Err(error) => return Err(error.into()),
            }
        }

        let path_ref = path.as_ref();
        // In-memory databases (tests) get a unique per-instance sessions dir: the
        // relative "sessions" fallback would otherwise be shared by every parallel
        // test in the process, and same-named session ids would clobber each other's
        // compaction archive files.
        let (sessions_dir, db_identity) = if path_ref.as_os_str() == ":memory:" {
            let dir =
                std::env::temp_dir().join(format!("holmes-test-sessions-{}", uuid::Uuid::new_v4()));
            (dir.clone(), dir)
        } else {
            let dir = sessions_dir_for(path_ref);
            // The registry key is the canonical database identity (P1-13), so
            // two different database files in one directory can never share a
            // worker, a file namespace or projection offsets.
            (dir, canonical_db_identity(path_ref))
        };
        std::fs::create_dir_all(&sessions_dir).ok();

        let conn = Arc::new(Mutex::new(conn));
        let write_contention = WriteContention::new();
        // One projector per database identity (P1-08/P1-13): every handle on
        // the same file shares the worker, giving a path-level global
        // projection order; different files never do.
        let projector =
            TranscriptProjector::for_database(&db_identity, &sessions_dir, conn.clone());
        let db = Self {
            task_store: TaskStore::new(conn.clone(), write_contention.clone()),
            projector,
            conn,
            write_contention,
            write_count: Arc::new(Mutex::new(0)),
            sessions_dir,
        };
        // Self-heal derived transcripts (P1-08): any session whose projected
        // high watermark or on-disk line count disagrees with the authoritative
        // events table gets its transcript rebuilt before this handle serves.
        db.reconcile_projections().await;
        Ok(db)
    }

    /// Open-time reconcile (P1-08): compare each session's authoritative event
    /// count/high-watermark against the persisted projection offset and the
    /// transcript file itself, and rebuild (through the single worker, so
    /// in-flight appends serialise correctly) any session whose transcript is
    /// missing, truncated or stale. Best-effort: failures are logged and open
    /// still succeeds — the next open retries.
    async fn reconcile_projections(&self) {
        let sessions: Vec<(String, i64, i64)> = {
            let conn = self.conn.lock().await;
            let result = conn
                .prepare(
                    "SELECT s.id, COUNT(e.id), COALESCE(MAX(e.event_index), -1)
                     FROM sessions s LEFT JOIN events e ON e.session_id = s.id
                     GROUP BY s.id",
                )
                .and_then(|mut stmt| {
                    let collected: Result<Vec<(String, i64, i64)>, _> = stmt
                        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                        .collect();
                    collected
                });
            match result {
                Ok(rows) => rows,
                Err(error) => {
                    tracing::warn!(error = %error, "projection reconcile scan failed; skipping");
                    return;
                }
            }
        };
        let offsets: std::collections::HashMap<String, i64> = {
            let conn = self.conn.lock().await;
            match conn
                .prepare("SELECT session_id, projected_event_index FROM projection_state")
                .and_then(|mut stmt| {
                    let collected: Result<Vec<(String, i64)>, _> = stmt
                        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                        .collect();
                    collected
                }) {
                Ok(rows) => rows.into_iter().collect(),
                Err(error) => {
                    tracing::warn!(error = %error, "projection offset scan failed; skipping");
                    return;
                }
            }
        };

        for (session_id, event_count, max_index) in sessions {
            if event_count == 0 {
                continue;
            }
            // Defense in depth (P1-12): entry points reject path-unsafe ids,
            // but a database written before that validation could still hold
            // one — never join it onto the sessions directory.
            if validate_session_id_for_path(&session_id).is_err() {
                tracing::warn!(
                    session_id = %session_id,
                    "skipping projection reconcile for path-unsafe session id"
                );
                continue;
            }
            let offset = offsets.get(&session_id).copied().unwrap_or(-1);
            let transcript = self.sessions_dir.join(&session_id).join("transcript.jsonl");
            let line_count = std::fs::read_to_string(&transcript)
                .ok()
                .map(|content| content.lines().count() as i64);
            // A session needs a rebuild when the durable watermark is behind
            // (crash before the worker caught up), when the file is missing,
            // or when the line count disagrees with the event count (a
            // mid-sequence projection failure the watermark cannot express).
            if offset >= max_index && line_count == Some(event_count) {
                continue;
            }
            match self.projector.rebuild_via_worker(&session_id).await {
                Ok(lines) => {
                    tracing::info!(
                        session_id = %session_id,
                        lines,
                        "reconciled transcript projection with the event store"
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        session_id = %session_id,
                        error = %error,
                        "open-time transcript reconcile failed; will retry on next open"
                    );
                }
            }
        }
    }

    /// Durable background task store sharing this database's connection
    /// (AGT-007). Lease owner identity is per-`SessionDB` instance.
    pub fn task_store(&self) -> TaskStore {
        self.task_store.clone()
    }

    /// The async transcript projection fed after each committed event
    /// (AGT-008). Exposed for tests (`flush`) and rebuild-drain inspection.
    pub fn projector(&self) -> TranscriptProjector {
        self.projector.clone()
    }

    /// Rebuild a session's `transcript.jsonl` from the authoritative `events`
    /// table (AGT-008). Heals projection failures and proves the JSONL is a
    /// pure derivative: the database rows are written back out verbatim. Runs
    /// through the projection worker (P1-08) so the rewrite serialises against
    /// queued appends instead of racing them.
    pub async fn rebuild_transcript(&self, session_id: &str) -> Result<usize, SessionError> {
        match self.projector.rebuild_via_worker(session_id).await {
            Ok(lines) => Ok(lines),
            Err(worker_error) => {
                // Worker gone (shutdown edge): fall back to a direct rewrite
                // from the events table, the pre-P1-08 behaviour.
                tracing::warn!(
                    session_id = %session_id,
                    error = %worker_error,
                    "projector worker unavailable; rebuilding transcript directly"
                );
                let payloads: Vec<String> = {
                    let conn = self.conn.lock().await;
                    let mut stmt = conn.prepare(
                        "SELECT event_data FROM events WHERE session_id = ?1 ORDER BY event_index",
                    )?;
                    let rows = stmt.query_map(params![session_id], |row| row.get(0))?;
                    let collected: Result<Vec<String>, _> = rows.collect();
                    collected?
                };
                self.projector
                    .rebuild_from_events(session_id, &payloads)
                    .map_err(SessionError::from)
            }
        }
    }
}

#[async_trait]
impl SessionStore for SessionDB {
    async fn create_session(&self, params: CreateSessionParams) -> Result<Session, SessionError> {
        self.create_session_with_events(params, Vec::new()).await
    }

    async fn create_session_with_events(
        &self,
        params: CreateSessionParams,
        events: Vec<Event>,
    ) -> Result<Session, SessionError> {
        let id = params
            .id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        // P1-12: a caller-supplied id becomes a directory name under the
        // sessions dir (transcript, compactions, browser profile). Reject
        // path-unsafe ids BEFORE any database or filesystem side effect.
        validate_session_id_for_path(&id)?;
        let now = chrono::Utc::now().to_rfc3339();
        let started_at = chrono::Utc::now();
        let case_id = if let Some(parent_id) = params.parent_session_id.as_deref() {
            let conn = self.conn.lock().await;
            conn.query_row(
                "SELECT case_id FROM sessions WHERE id = ?1",
                params![parent_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| SessionError::NotFound(parent_id.to_owned()))?
        } else {
            format!("case-{}", uuid::Uuid::new_v4())
        };

        // Serialise the startup events BEFORE opening the transaction: a
        // serialisation failure must surface without writing anything, not
        // roll back a half-open transaction.
        let mut prepared_events: Vec<(u64, String, String, String)> =
            Vec::with_capacity(events.len());
        let mut user_message_count: i64 = 0;
        let mut tool_call_count: i64 = 0;
        for (index, event) in events.iter().enumerate() {
            let event_type = event_type_str(event).to_string();
            if matches!(event, Event::UserMessage { .. }) {
                user_message_count += 1;
            }
            if matches!(event, Event::ToolCall { .. }) {
                tool_call_count += 1;
            }
            prepared_events.push((
                index as u64,
                event_type,
                serialize_event_for_storage(event)?,
                now.clone(),
            ));
        }

        let id_for_insert = id.clone();
        let case_id_for_insert = case_id.clone();
        let title_for_insert = params.title.clone();
        let mode_for_insert = params.mode.clone();
        let model_for_insert = params.model.clone();
        let system_prompt_for_insert = params.system_prompt.clone();
        let parent_for_insert = params.parent_session_id.clone();
        let fork_for_insert = params.fork_point;
        let source_for_insert = params.source.clone();
        let tags_for_insert = params.tags.clone();
        let now_for_insert = now.clone();
        let prepared_for_retry = prepared_events.clone();

        // Single transaction (AGT-008): the session row, its startup metadata
        // events and the denormalised counters commit atomically — a crash at
        // any point leaves no half-initialised session behind.
        self.write_contention.with_db_retry(|| {
            let id = id_for_insert.clone();
            let case_id = case_id_for_insert.clone();
            let title = title_for_insert.clone();
            let mode = mode_for_insert.clone();
            let model = model_for_insert.clone();
            let system_prompt = system_prompt_for_insert.clone();
            let parent = parent_for_insert.clone();
            let fork = fork_for_insert;
            let source = source_for_insert.clone();
            let tags = tags_for_insert.clone();
            let now = now_for_insert.clone();
            let prepared = prepared_for_retry.clone();
            async move {
                let mut conn = self.conn.lock().await;
                let tx = conn.transaction()?;
                if parent.is_none() {
                    tx.execute(
                        "INSERT INTO cases (case_id, root_session_id, status, ledger_version, next_evidence_seq, created_at, updated_at)
                         VALUES (?1, ?2, 'open', 0, 1, ?3, ?3)",
                        params![case_id, id, now],
                    )?;
                }
                tx.execute(
                    "INSERT INTO sessions (id, case_id, title, mode, model, system_prompt, parent_session_id, fork_point, source, tags, started_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                    params![
                        id,
                        case_id,
                        title,
                        mode.as_ref().map(|m| mode_to_str(m)).unwrap_or("pentest"),
                        model,
                        system_prompt,
                        parent,
                        fork.map(|f| f as i64),
                        source.as_deref().unwrap_or("cli"),
                        serde_json::to_string(&tags).unwrap_or_default(),
                        now,
                    ],
                )?;
                for (event_index, event_type, event_data, timestamp) in prepared.iter() {
                    tx.execute(
                        "INSERT INTO events (session_id, event_index, event_type, event_data, timestamp)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![id, *event_index as i64, event_type, event_data, timestamp],
                    )?;
                }
                if user_message_count > 0 || tool_call_count > 0 {
                    tx.execute(
                        "UPDATE sessions SET message_count = ?2, tool_call_count = ?3 WHERE id = ?1",
                        params![id, user_message_count, tool_call_count],
                    )?;
                }
                tx.commit()?;
                Ok::<_, rusqlite::Error>(())
            }
        }).await?;

        // Post-commit async projection of the startup events (AGT-008).
        for (event_index, _, event_data, _) in &prepared_events {
            self.projector.project(&id, *event_index, event_data).await;
        }

        let session_dir = self.sessions_dir.join(&id);
        std::fs::create_dir_all(session_dir.join("compactions")).ok();

        Ok(Session {
            id,
            case_id,
            title: params.title,
            mode: params.mode.unwrap_or_default(),
            model: params.model,
            model_config: None,
            system_prompt: params.system_prompt,
            parent_session_id: params.parent_session_id,
            fork_point: params.fork_point,
            source: params.source.unwrap_or_else(|| "cli".into()),
            tags: params.tags,
            started_at,
            ended_at: None,
            end_reason: None,
            message_count: user_message_count as u64,
            tool_call_count: tool_call_count as u64,
            subagent_count: 0,
            input_tokens: 0,
            output_tokens: 0,
            estimated_cost_usd: 0.0,
            goal_condition: None,
            goal_achieved: false,
        })
    }

    async fn append_event(&self, session_id: &str, event: &Event) -> Result<u64, SessionError> {
        let mut processed_event = event.clone();
        // P1-08: oversized tool results are offloaded into the blob tables —
        // inserted below in the SAME transaction as the event row, so SQLite
        // alone can always restore the payload. The in-event content becomes a
        // content-addressed marker; the old disk sidecar is gone.
        let mut prepared_blob: Option<crate::blob_store::PreparedBlob> = None;
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

        let event_type = event_type_str(&processed_event);
        let timestamp = chrono::Utc::now().to_rfc3339();

        let event_data = serialize_event_for_storage(&processed_event)?;

        let session_id_owned = session_id.to_string();
        let event_type_owned = event_type.to_string();
        let event_data_owned = event_data;
        let timestamp_owned = timestamp;
        let event_for_match = processed_event.clone();

        // Single transaction (AGT-008 + P1-08): index allocation, blob chunks,
        // event insert and the denormalised session counters commit or roll
        // back together, so a crash mid-append can never leave a counter ahead
        // of its event nor an event marker without its payload.
        let id = self.write_contention.with_db_retry(|| {
            let session_id = session_id_owned.clone();
            let event_type = event_type_owned.clone();
            let event_data = event_data_owned.clone();
            let timestamp = timestamp_owned.clone();
            let event = event_for_match.clone();
            let blob = prepared_blob.clone();
            async move {
                let mut conn = self.conn.lock().await;
                let tx = conn.transaction()?;
                let idx: u64 = tx
                    .query_row(
                        "SELECT COALESCE(MAX(event_index), -1) + 1 FROM events WHERE session_id = ?1",
                        params![session_id],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);

                if let Some(blob) = &blob {
                    crate::blob_store::insert(&tx, blob)?;
                }

                tx.execute(
                    "INSERT INTO events (session_id, event_index, event_type, event_data, timestamp)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![session_id, idx as i64, event_type, event_data, timestamp],
                )?;

                if matches!(event, Event::UserMessage { .. }) {
                    tx.execute(
                        "UPDATE sessions SET message_count = message_count + 1 WHERE id = ?1",
                        params![session_id],
                    )?;
                }
                if matches!(event, Event::ToolCall { .. }) {
                    tx.execute(
                        "UPDATE sessions SET tool_call_count = tool_call_count + 1 WHERE id = ?1",
                        params![session_id],
                    )?;
                }

                tx.commit()?;
                Ok::<_, rusqlite::Error>(idx)
            }
        }).await?;

        // Post-commit async projection (AGT-008): the database row above is
        // the fact; transcript.jsonl is derived from it. A projection failure
        // is queued for rebuild and must not fail this append.
        self.projector
            .project(session_id, id, &event_data_owned)
            .await;

        // Periodic WAL checkpoint
        let mut count = self.write_count.lock().await;
        *count += 1;
        if *count % 50 == 0 {
            let conn = self.conn.lock().await;
            conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);").ok();
        }

        Ok(id)
    }

    fn durable_task_sink(&self) -> Option<Arc<dyn holmes_core::background::DurableTaskSink>> {
        Some(Arc::new(self.task_store()))
    }

    async fn list_undelivered_task_results(
        &self,
        parent_session_id: &str,
    ) -> Result<Vec<holmes_core::background::UndeliveredTaskResult>, SessionError> {
        let pending = self
            .task_store
            .list_undelivered_terminal(parent_session_id)
            .await?;
        Ok(pending
            .into_iter()
            .map(|record| holmes_core::background::UndeliveredTaskResult {
                task_id: record.task_id,
                description: record.description,
                state: record.state.as_str().to_string(),
                result: record.result,
                error: record.last_error,
            })
            .collect())
    }

    async fn deliver_task_result(
        &self,
        parent_session_id: &str,
        task_id: &str,
        content: &str,
    ) -> Result<holmes_core::background::TaskDeliveryOutcome, SessionError> {
        use holmes_core::background::TaskDeliveryOutcome;

        let event = Event::UserMessage {
            content: content.to_string(),
            timestamp: chrono::Utc::now(),
        };
        let event_type = event_type_str(&event);
        let event_data = serialize_event_for_storage(&event)?;
        let session_id_owned = parent_session_id.to_string();
        let task_id_owned = task_id.to_string();

        // Single transaction (P1-02): read the task's delivery state, append
        // the result event (with index allocation and the denormalised
        // message counter) and mark the task delivered — all or nothing. A
        // crash cannot leave the event persisted but the task undelivered
        // (which would double-present on restart) nor the reverse (which
        // would lose the result).
        let (outcome, appended) = self.write_contention.with_db_retry(|| {
            let session_id = session_id_owned.clone();
            let task_id = task_id_owned.clone();
            let event_data = event_data.clone();
            async move {
                let mut conn = self.conn.lock().await;
                let tx = conn.transaction()?;
                let row: Option<(String, i64)> = tx
                    .query_row(
                        "SELECT state, delivered FROM tasks
                         WHERE task_id = ?1 AND parent_session_id = ?2",
                        params![task_id, session_id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                let Some((state, delivered)) = row else {
                    return Ok::<_, rusqlite::Error>((TaskDeliveryOutcome::UnknownTask, None));
                };
                if !matches!(state.as_str(), "succeeded" | "failed" | "cancelled") {
                    return Ok((TaskDeliveryOutcome::NotTerminal, None));
                }
                if delivered != 0 {
                    return Ok((TaskDeliveryOutcome::AlreadyDelivered, None));
                }
                let idx: u64 = tx
                    .query_row(
                        "SELECT COALESCE(MAX(event_index), -1) + 1 FROM events WHERE session_id = ?1",
                        params![session_id],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                tx.execute(
                    "INSERT INTO events (session_id, event_index, event_type, event_data, timestamp)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        session_id,
                        idx as i64,
                        event_type,
                        event_data,
                        chrono::Utc::now().to_rfc3339()
                    ],
                )?;
                tx.execute(
                    "UPDATE sessions SET message_count = message_count + 1 WHERE id = ?1",
                    params![session_id],
                )?;
                let marked = tx.execute(
                    "UPDATE tasks SET delivered = 1, updated_at = ?2
                     WHERE task_id = ?1 AND delivered = 0",
                    params![task_id, chrono::Utc::now().to_rfc3339()],
                )?;
                debug_assert_eq!(marked, 1, "delivery race inside the write transaction");
                tx.commit()?;
                Ok((TaskDeliveryOutcome::Appended, Some((idx, event_data))))
            }
        }).await?;

        // Post-commit async projection, same contract as append_event: the
        // database row is the fact; transcript.jsonl is derived.
        if let Some((idx, event_data)) = appended {
            self.projector
                .project(parent_session_id, idx, &event_data)
                .await;
        }
        Ok(outcome)
    }

    async fn get_events(&self, session_id: &str) -> Result<Vec<StoredEvent>, SessionError> {
        let mut events = {
            let conn = self.conn.lock().await;
            let mut stmt = conn.prepare(
                "SELECT id, session_id, event_index, turn_index, event_type, event_data, timestamp
                 FROM events WHERE session_id = ?1 ORDER BY event_index",
            )?;

            let rows = stmt.query_map(params![session_id], |row| {
                let id: i64 = row.get(0)?;
                let session_id: String = row.get(1)?;
                let event_index: i64 = row.get(2)?;
                let turn_index: Option<i64> = row.get(3)?;
                let data: String = row.get(5)?;
                let timestamp_str: String = row.get(6)?;
                Ok((id, session_id, event_index, turn_index, data, timestamp_str))
            })?;

            let mut temp_events = Vec::new();
            for row in rows {
                let (id, s_id, event_index, turn_index, data, timestamp_str) = row?;
                let event: Event = parse_event_data(&data).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        5,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;

                let timestamp = match chrono::DateTime::parse_from_rfc3339(&timestamp_str) {
                    Ok(d) => d.with_timezone(&chrono::Utc),
                    Err(e) => {
                        tracing::warn!(
                            event_id = id,
                            timestamp = %timestamp_str,
                            error = %e,
                            "failed to parse event timestamp; falling back to now()",
                        );
                        chrono::Utc::now()
                    }
                };

                temp_events.push(StoredEvent {
                    id: id as u64,
                    session_id: s_id,
                    event_index: event_index as u64,
                    turn_index: turn_index.map(|v| v as u64),
                    timestamp,
                    event,
                });
            }
            temp_events
        };

        for stored in &mut events {
            if let Event::ToolResult {
                ref mut content, ..
            } = stored.event
            {
                if let Some(sha256) = crate::blob_store::parse_reference(content) {
                    // P1-08: restore the offloaded payload from the blob
                    // tables. Failure keeps the marker (degraded read) rather
                    // than failing the whole history.
                    let sha256 = sha256.to_string();
                    let restored = {
                        let conn = self.conn.lock().await;
                        crate::blob_store::load(&conn, &sha256)
                    };
                    match restored {
                        Ok(full_content) => {
                            *content = full_content;
                        }
                        Err(e) => {
                            tracing::error!(
                                sha256 = %sha256,
                                error = %e,
                                "failed to restore blob-backed event content; keeping marker"
                            );
                        }
                    }
                } else if content.starts_with(crate::blob_store::LEGACY_BYPASS_PREFIX) {
                    // Pre-v6 pointer into a tool-results sidecar file. Still
                    // honoured when the file exists; degrades to the pointer
                    // otherwise (the database cannot restore pre-v6 payloads).
                    let file_path_str = &content[crate::blob_store::LEGACY_BYPASS_PREFIX.len()..];
                    let file_path = std::path::PathBuf::from(file_path_str);
                    match tokio::fs::read_to_string(&file_path).await {
                        Ok(full_content) => {
                            *content = full_content;
                        }
                        Err(e) => {
                            tracing::error!(
                                path = %file_path.display(),
                                error = %e,
                                "failed to read bypassed event content file; keeping metadata link"
                            );
                        }
                    }
                }
            }
        }
        Ok(events)
    }

    async fn replay_session_context(
        &self,
        session_id: &str,
    ) -> Result<crate::ReplayedSessionContext, SessionError> {
        let events = self.get_events(session_id).await?;
        Ok(crate::replay::replay_events(session_id, &events))
    }

    async fn session_workspace(
        &self,
        session_id: &str,
    ) -> Result<std::path::PathBuf, SessionError> {
        validate_session_id_for_path(session_id)?;
        let path = self.sessions_dir.join(session_id);
        tokio::fs::create_dir_all(&path).await?;
        Ok(path)
    }

    fn sessions_dir(&self) -> Option<std::path::PathBuf> {
        Some(self.sessions_dir.clone())
    }

    async fn write_compaction_archive(
        &self,
        session_id: &str,
        compaction_event_index: u64,
        archive: &CompactionArchive,
    ) -> Result<String, SessionError> {
        let dir = self
            .session_workspace(session_id)
            .await?
            .join("compactions");
        tokio::fs::create_dir_all(&dir).await?;
        let path = dir.join(format!("compaction_{compaction_event_index}.json"));
        let content = serde_json::to_string_pretty(archive)?;
        tokio::fs::write(&path, content).await?;
        Ok(path.to_string_lossy().to_string())
    }

    async fn read_compaction_archive(&self, path: &str) -> Result<CompactionArchive, SessionError> {
        validate_archive_path(&self.sessions_dir, path).await?;
        let content = tokio::fs::read_to_string(path).await?;
        Ok(serde_json::from_str(&content)?)
    }

    async fn list_sessions(
        &self,
        filter: &SessionFilter,
    ) -> Result<Vec<SessionSummary>, SessionError> {
        let conn = self.conn.lock().await;
        let mut sql = String::from(
            "SELECT s.id, s.title, s.mode, s.source, s.started_at, s.ended_at,
                    s.end_reason, s.message_count, s.parent_session_id,
                    (SELECT SUBSTR(e.event_data, 1, 120) FROM events e
                     WHERE e.session_id = s.id AND e.event_type = 'user_message'
                     ORDER BY e.event_index LIMIT 1) as preview,
                    (SELECT e.timestamp FROM events e
                     WHERE e.session_id = s.id
                     ORDER BY e.event_index DESC LIMIT 1) as last_active
             FROM sessions s WHERE 1=1",
        );

        let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        if let Some(source) = &filter.source {
            sql.push_str(" AND s.source = ?");
            param_values.push(Box::new(source.clone()));
        }
        if let Some(mode) = &filter.mode {
            sql.push_str(" AND s.mode = ?");
            param_values.push(Box::new(mode_to_str(mode).to_string()));
        }
        if !filter.include_children {
            sql.push_str(" AND s.parent_session_id IS NULL");
        } else if let Some(parent_id) = &filter.parent_session_id {
            sql.push_str(" AND s.parent_session_id = ?");
            param_values.push(Box::new(parent_id.clone()));
        }
        if let Some(search) = &filter.search {
            sql.push_str(
                " AND s.id IN (SELECT session_id FROM events_fts WHERE events_fts MATCH ?)",
            );
            param_values.push(Box::new(search.clone()));
        }

        sql.push_str(" ORDER BY s.started_at DESC");

        if let Some(limit) = filter.limit {
            sql.push_str(&format!(" LIMIT {}", limit));
        }
        if let Some(offset) = filter.offset {
            sql.push_str(&format!(" OFFSET {}", offset));
        }

        let params_refs: Vec<&dyn rusqlite::types::ToSql> =
            param_values.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_refs.as_slice(), |row| {
            let message_count: i64 = row.get(7)?;
            Ok(SessionSummary {
                id: row.get(0)?,
                title: row.get(1)?,
                mode: str_to_mode(&row.get::<_, String>(2)?),
                source: row.get(3)?,
                started_at: parse_datetime(&row.get::<_, String>(4)?),
                ended_at: row.get::<_, Option<String>>(5)?.map(|s| parse_datetime(&s)),
                end_reason: row
                    .get::<_, Option<String>>(6)?
                    .and_then(|r| str_to_end_reason(&r)),
                message_count: message_count as u64,
                parent_session_id: row.get(8)?,
                preview: row.get(9)?,
                last_active: row
                    .get::<_, Option<String>>(10)?
                    .map(|s| parse_datetime(&s)),
            })
        })?;

        let mut sessions = Vec::new();
        for row in rows {
            sessions.push(row?);
        }
        Ok(sessions)
    }

    async fn end_session(&self, id: &str, reason: EndReason) -> Result<(), SessionError> {
        let id_owned = id.to_string();
        let reason_owned = reason;
        self.write_contention
            .with_db_retry(|| {
                let id = id_owned.clone();
                let reason = reason_owned.clone();
                async move {
                    let conn = self.conn.lock().await;
                    conn.execute(
                        "UPDATE sessions SET ended_at = ?1, end_reason = ?2 WHERE id = ?3",
                        params![
                            chrono::Utc::now().to_rfc3339(),
                            end_reason_to_str(&reason),
                            id
                        ],
                    )?;
                    Ok::<_, rusqlite::Error>(())
                }
            })
            .await?;
        Ok(())
    }

    async fn reopen_session(&self, id: &str) -> Result<(), SessionError> {
        let id_owned = id.to_string();
        self.write_contention
            .with_db_retry(|| {
                let id = id_owned.clone();
                async move {
                    let conn = self.conn.lock().await;
                    conn.execute(
                        "UPDATE sessions SET ended_at = NULL, end_reason = NULL WHERE id = ?1",
                        params![id],
                    )?;
                    Ok::<_, rusqlite::Error>(())
                }
            })
            .await?;
        Ok(())
    }

    async fn set_goal_condition(
        &self,
        id: &str,
        condition: Option<&str>,
    ) -> Result<(), SessionError> {
        let id_owned = id.to_string();
        let condition_owned = condition.map(ToOwned::to_owned);
        self.write_contention
            .with_db_retry(|| {
                let id = id_owned.clone();
                let condition = condition_owned.clone();
                async move {
                    let conn = self.conn.lock().await;
                    conn.execute(
                        "UPDATE sessions SET goal_condition = ?1, goal_achieved = 0 WHERE id = ?2",
                        params![condition, id],
                    )?;
                    Ok::<_, rusqlite::Error>(())
                }
            })
            .await?;
        Ok(())
    }

    async fn mark_goal_achieved(&self, id: &str) -> Result<(), SessionError> {
        let id_owned = id.to_string();
        self.write_contention
            .with_db_retry(|| {
                let id = id_owned.clone();
                async move {
                    let conn = self.conn.lock().await;
                    conn.execute(
                        "UPDATE sessions SET goal_achieved = 1 WHERE id = ?1",
                        params![id],
                    )?;
                    Ok::<_, rusqlite::Error>(())
                }
            })
            .await?;
        Ok(())
    }

    async fn get_session(&self, id: &str) -> Result<Option<Session>, SessionError> {
        let resolved_id = {
            let conn = self.conn.lock().await;
            let result = conn.query_row(
                "SELECT id, case_id, title, mode, model, model_config, system_prompt, parent_session_id,
                        fork_point, source, tags, started_at, ended_at, end_reason,
                        message_count, tool_call_count, subagent_count,
                        input_tokens, output_tokens, estimated_cost_usd,
                        goal_condition, goal_achieved
                 FROM sessions WHERE id = ?1",
                params![id],
                |row| {
                    let fork_point: Option<i64> = row.get(8)?;
                    let message_count: i64 = row.get(14)?;
                    let tool_call_count: i64 = row.get(15)?;
                    let subagent_count: i64 = row.get(16)?;
                    let input_tokens: i64 = row.get(17)?;
                    let output_tokens: i64 = row.get(18)?;
                    let goal_achieved: i64 = row.get(21)?;
                    Ok(Session {
                        id: row.get(0)?,
                        case_id: row.get(1)?,
                        title: row.get(2)?,
                        mode: str_to_mode(&row.get::<_, String>(3)?),
                        model: row.get(4)?,
                        model_config: row
                            .get::<_, Option<String>>(5)?
                            .and_then(|v| serde_json::from_str(&v).ok()),
                        system_prompt: row.get(6)?,
                        parent_session_id: row.get(7)?,
                        fork_point: fork_point.map(|v| v as u64),
                        source: row.get(9)?,
                        tags: row
                            .get::<_, String>(10)
                            .ok()
                            .and_then(|t| serde_json::from_str(&t).ok())
                            .unwrap_or_default(),
                        started_at: parse_datetime(&row.get::<_, String>(11)?),
                        ended_at: row
                            .get::<_, Option<String>>(12)?
                            .map(|s| parse_datetime(&s)),
                        end_reason: row
                            .get::<_, Option<String>>(13)?
                            .and_then(|r| str_to_end_reason(&r)),
                        message_count: message_count as u64,
                        tool_call_count: tool_call_count as u64,
                        subagent_count: subagent_count as u64,
                        input_tokens: input_tokens as u64,
                        output_tokens: output_tokens as u64,
                        estimated_cost_usd: row.get(19)?,
                        goal_condition: row.get(20)?,
                        goal_achieved: goal_achieved != 0,
                    })
                },
            );

            match result {
                Ok(session) => return Ok(Some(session)),
                Err(rusqlite::Error::QueryReturnedNoRows) => {
                    // Try prefix match
                    let mut stmt = conn.prepare(
                        "SELECT id FROM sessions WHERE id LIKE ?1 ORDER BY started_at DESC LIMIT 2",
                    )?;
                    let matches: Vec<String> = stmt
                        .query_map(params![format!("{}%", id)], |r| r.get(0))?
                        .filter_map(|r| r.ok())
                        .collect();

                    if matches.len() == 1 {
                        matches[0].clone()
                    } else {
                        return Ok(None);
                    }
                }
                Err(e) => return Err(e.into()),
            }
        };

        Box::pin(self.get_session(&resolved_id)).await
    }

    async fn fork_session(
        &self,
        id: &str,
        fork_point: u64,
        new_title: &str,
    ) -> Result<Session, SessionError> {
        fork_session_inner(self, id, fork_point, new_title, None).await
    }

    async fn fork_session_with_events(
        &self,
        id: &str,
        fork_point: u64,
        new_title: &str,
        startup: crate::store::ForkStartupSpec,
    ) -> Result<Session, SessionError> {
        fork_session_inner(self, id, fork_point, new_title, Some(startup)).await
    }

    async fn update_token_counts(&self, id: &str, delta: &TokenDelta) -> Result<(), SessionError> {
        let id_owned = id.to_string();
        let delta_owned = delta.clone();
        self.write_contention
            .with_db_retry(|| {
                let id = id_owned.clone();
                let delta = delta_owned.clone();
                async move {
                    let conn = self.conn.lock().await;
                    conn.execute(
                        "UPDATE sessions SET
                        input_tokens = input_tokens + ?1,
                        output_tokens = output_tokens + ?2,
                        cache_read_tokens = cache_read_tokens + ?3,
                        cache_write_tokens = cache_write_tokens + ?4
                     WHERE id = ?5",
                        params![
                            delta.input as i64,
                            delta.output as i64,
                            delta.cache_read as i64,
                            delta.cache_write as i64,
                            id
                        ],
                    )?;
                    Ok::<_, rusqlite::Error>(())
                }
            })
            .await?;
        Ok(())
    }

    async fn truncate_events_after(
        &self,
        session_id: &str,
        event_index: u64,
    ) -> Result<(), SessionError> {
        let session_id_owned = session_id.to_string();
        self.write_contention
            .with_db_retry(|| {
                let session_id = session_id_owned.clone();
                async move {
                    let conn = self.conn.lock().await;
                    conn.execute(
                        "DELETE FROM events WHERE session_id = ?1 AND event_index > ?2",
                        params![session_id, event_index as i64],
                    )?;

                    let message_count: i64 = conn.query_row(
                        "SELECT COUNT(*) FROM events WHERE session_id = ?1 AND event_type = 'user_message'",
                        params![session_id],
                        |row| row.get(0),
                    )?;
                    let tool_call_count: i64 = conn.query_row(
                        "SELECT COUNT(*) FROM events WHERE session_id = ?1 AND event_type = 'tool_call'",
                        params![session_id],
                        |row| row.get(0),
                    )?;

                    let last_goal_set = conn
                        .query_row(
                            "SELECT event_index, json_extract(event_data, '$.condition')
                             FROM events
                             WHERE session_id = ?1 AND event_type = 'goal_set'
                             ORDER BY event_index DESC LIMIT 1",
                            params![session_id],
                            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
                        )
                        .optional()?;
                    let last_goal_clear_index = conn
                        .query_row(
                            "SELECT MAX(event_index)
                             FROM events
                             WHERE session_id = ?1 AND event_type = 'goal_cleared'",
                            params![session_id],
                            |row| row.get::<_, Option<i64>>(0),
                        )?
                        .unwrap_or(-1);

                    let (goal_condition, goal_achieved) =
                        if let Some((goal_index, condition)) = last_goal_set {
                            if goal_index > last_goal_clear_index {
                                let achieved_count: i64 = conn.query_row(
                                    "SELECT COUNT(*) FROM events
                                     WHERE session_id = ?1
                                       AND event_type = 'goal_evaluated'
                                       AND event_index > ?2
                                       AND json_extract(event_data, '$.satisfied') = 1",
                                    params![session_id, goal_index],
                                    |row| row.get(0),
                                )?;
                                (condition, achieved_count > 0)
                            } else {
                                (None, false)
                            }
                        } else {
                            (None, false)
                        };

                    conn.execute(
                        "UPDATE sessions
                         SET message_count = ?1,
                             tool_call_count = ?2,
                             goal_condition = ?3,
                             goal_achieved = ?4
                         WHERE id = ?5",
                        params![
                            message_count,
                            tool_call_count,
                            goal_condition,
                            goal_achieved as i32,
                            session_id
                        ],
                    )?;

                    Ok::<_, rusqlite::Error>(())
                }
            })
            .await?;
        Ok(())
    }

    async fn set_title(&self, id: &str, title: &str) -> Result<(), SessionError> {
        let id_owned = id.to_string();
        let title_owned = title.to_string();
        self.write_contention
            .with_db_retry(|| {
                let id = id_owned.clone();
                let title = title_owned.clone();
                async move {
                    let conn = self.conn.lock().await;
                    conn.execute(
                        "UPDATE sessions SET title = ?1 WHERE id = ?2",
                        params![title, id],
                    )?;
                    Ok::<_, rusqlite::Error>(())
                }
            })
            .await?;
        Ok(())
    }

    async fn set_mode(&self, id: &str, mode: SessionMode) -> Result<(), SessionError> {
        let id_owned = id.to_string();
        let mode_owned = mode;
        self.write_contention
            .with_db_retry(|| {
                let id = id_owned.clone();
                let mode = mode_owned.clone();
                async move {
                    let conn = self.conn.lock().await;
                    conn.execute(
                        "UPDATE sessions SET mode = ?1 WHERE id = ?2",
                        params![mode_to_str(&mode), id],
                    )?;
                    Ok::<_, rusqlite::Error>(())
                }
            })
            .await?;
        Ok(())
    }

    async fn set_model(&self, id: &str, model: &str) -> Result<(), SessionError> {
        let id_owned = id.to_string();
        let model_owned = model.to_string();
        self.write_contention
            .with_db_retry(|| {
                let id = id_owned.clone();
                let model = model_owned.clone();
                async move {
                    let conn = self.conn.lock().await;
                    conn.execute(
                        "UPDATE sessions SET model = ?1 WHERE id = ?2",
                        params![model, id],
                    )?;
                    Ok::<_, rusqlite::Error>(())
                }
            })
            .await?;
        Ok(())
    }

    async fn search_events(
        &self,
        query: &str,
        top_k: u32,
    ) -> Result<Vec<SearchResult>, SessionError> {
        let sanitized = crate::fts::sanitize_fts5_query(query);
        let conn = self.conn.lock().await;

        let (sql, search_param) = if crate::fts::contains_cjk(query) {
            (
                "SELECT e.id, e.session_id, e.event_index, e.event_type,
                        json_extract(e.event_data, '$.content_text') as content_text,
                        s.title as session_title
                 FROM events e JOIN sessions s ON e.session_id = s.id
                 WHERE json_extract(e.event_data, '$.content_text') LIKE ?1 AND e.session_id NOT LIKE 'sub-%'
                 ORDER BY e.id DESC LIMIT ?2",
                format!("%{}%", query),
            )
        } else {
            (
                "SELECT e.id, e.session_id, e.event_index, e.event_type,
                        json_extract(e.event_data, '$.content_text') as content_text,
                        s.title as session_title
                 FROM events e JOIN events_fts f ON e.id = f.rowid
                 JOIN sessions s ON e.session_id = s.id
                 WHERE events_fts MATCH ?1 AND e.session_id NOT LIKE 'sub-%'
                 ORDER BY rank LIMIT ?2",
                sanitized,
            )
        };

        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params![search_param, top_k], |row| {
            let event_id: i64 = row.get(0)?;
            let event_index: i64 = row.get(2)?;
            Ok(SearchResult {
                event_id: event_id as u64,
                session_id: row.get(1)?,
                event_index: event_index as u64,
                event_type: row.get(3)?,
                snippet: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                session_title: row.get(5)?,
            })
        })?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }
}

// === Supporting types ===

#[derive(Debug, Clone)]
pub struct CreateSessionParams {
    pub id: Option<String>,
    pub title: Option<String>,
    pub mode: Option<SessionMode>,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    pub parent_session_id: Option<String>,
    pub fork_point: Option<u64>,
    pub source: Option<String>,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub event_id: u64,
    pub session_id: String,
    pub event_index: u64,
    pub event_type: String,
    pub snippet: String,
    pub session_title: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("session not found: {0}")]
    NotFound(String),
    #[error("{0}")]
    Other(String),
}

// === Helper functions ===

pub(crate) fn validate_session_id_for_path(session_id: &str) -> Result<(), SessionError> {
    if session_id.is_empty() {
        return Err(SessionError::Other(
            "invalid session id: cannot be empty".into(),
        ));
    }

    let path = Path::new(session_id);
    if path.is_absolute() {
        return Err(SessionError::Other(format!(
            "invalid session id '{}': absolute paths are not allowed",
            session_id
        )));
    }

    let mut components = path.components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(component)), None) if component == session_id => Ok(()),
        _ => Err(SessionError::Other(format!(
            "invalid session id '{}': must be a single path component without separators or '..'",
            session_id
        ))),
    }
}

/// Canonical absolute form of a database path, used as the projector
/// registry identity (P1-13). The database file already exists by the time
/// `SessionDB::open` calls this (the connection created it); the fallback
/// covers exotic cases by canonicalizing the parent and re-appending the file
/// name.
fn canonical_db_identity(db_path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(db_path) {
        return canonical;
    }
    match (db_path.parent(), db_path.file_name()) {
        (Some(parent), Some(name)) => std::fs::canonicalize(parent)
            .map(|canonical_parent| canonical_parent.join(name))
            .unwrap_or_else(|_| db_path.to_path_buf()),
        _ => db_path.to_path_buf(),
    }
}

/// The on-disk namespace for a database's derived session files (transcripts,
/// compaction archives, checkpoints).
///
/// P1-13 layout rule: the default database file (`holmes.db`, what the CLI
/// opens) keeps the historical `<db-parent>/sessions` directory, so existing
/// installs are byte-for-byte untouched. ANY other database file gets a
/// hash-namespaced sibling `<db-parent>/sessions-<sha256(canonical db
/// path)[..12]>`: two databases in one directory therefore never share
/// transcript files, projection offsets or a projection worker. The rule is
/// deterministic in the canonical path, so reopening the same file always
/// lands in the same namespace, and two files can never collide (same
/// directory ⇒ different names ⇒ different canonical paths ⇒ different
/// hashes). Renaming a database file moves its namespace; the open-time
/// reconcile rebuilds transcripts from the authoritative events table, but
/// compaction archives (referenced by absolute path from stored events) do
/// not follow — treat the database file name as stable once created.
pub fn sessions_dir_for(db_path: &Path) -> PathBuf {
    const DEFAULT_DB_NAME: &str = "holmes.db";
    let identity = canonical_db_identity(db_path);
    let parent = identity
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    if identity.file_name().is_some_and(|n| n == DEFAULT_DB_NAME) {
        return parent.join("sessions");
    }
    let hash = {
        use sha2::Digest;
        let digest = sha2::Sha256::digest(identity.to_string_lossy().as_bytes());
        let mut out = String::with_capacity(12);
        for b in &digest[..6] {
            out.push_str(&format!("{b:02x}"));
        }
        out
    };
    parent.join(format!("sessions-{hash}"))
}

async fn canonicalize_existing_or_parent(path: &Path) -> Result<PathBuf, SessionError> {
    match tokio::fs::canonicalize(path).await {
        Ok(canonical) => Ok(canonical),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().ok_or_else(|| {
                SessionError::Other(format!(
                    "path '{}' has no parent directory to canonicalize",
                    path.display()
                ))
            })?;
            let canonical_parent = tokio::fs::canonicalize(parent).await?;
            let file_name = path.file_name().ok_or_else(|| {
                SessionError::Other(format!(
                    "path '{}' has no final component to canonicalize",
                    path.display()
                ))
            })?;
            Ok(canonical_parent.join(file_name))
        }
        Err(error) => Err(error.into()),
    }
}

/// Validate that a compaction archive path resolves inside the sessions
/// directory, guarding against path traversal.
async fn validate_archive_path(sessions_dir: &Path, path: &str) -> Result<PathBuf, SessionError> {
    let requested = Path::new(path);
    let canonical_sessions_dir = canonicalize_existing_or_parent(sessions_dir).await?;

    let canonical_requested = match tokio::fs::canonicalize(requested).await {
        Ok(canonical) => canonical,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = requested.parent().ok_or_else(|| {
                SessionError::Other(format!(
                    "compaction archive path '{}' has no parent directory",
                    path
                ))
            })?;
            let canonical_parent = tokio::fs::canonicalize(parent).await?;
            if !canonical_parent.starts_with(&canonical_sessions_dir) {
                return Err(SessionError::Other(format!(
                    "compaction archive path '{}' is outside sessions directory '{}'",
                    path,
                    canonical_sessions_dir.display()
                )));
            }
            return Ok(requested.to_path_buf());
        }
        Err(error) => return Err(error.into()),
    };

    if !canonical_requested.starts_with(&canonical_sessions_dir) {
        return Err(SessionError::Other(format!(
            "compaction archive path '{}' is outside sessions directory '{}'",
            path,
            canonical_sessions_dir.display()
        )));
    }

    Ok(canonical_requested)
}

/// Parse an event_data JSON blob back into an `Event`.
///
/// `append_event` injects a `content_text` field into the serialized event for
/// FTS5 indexing; this helper strips that field before deserializing so the
/// resulting JSON matches the `Event` schema exactly.
fn parse_event_data(data: &str) -> Result<Event, serde_json::Error> {
    let mut value: serde_json::Value = serde_json::from_str(data)?;
    if let Some(obj) = value.as_object_mut() {
        obj.remove("content_text");
    }
    serde_json::from_value(value)
}

/// Serialise an event into the stored `event_data` payload, injecting the
/// flattened `content_text` field the FTS5 triggers index on. Single source
/// for the storage shape so append / atomic create / transcript rebuild all
/// emit byte-identical lines.
pub(crate) fn serialize_event_for_storage(event: &Event) -> Result<String, serde_json::Error> {
    let mut v: serde_json::Value = serde_json::to_value(event)?;
    if let Some(obj) = v.as_object_mut() {
        obj.insert(
            "content_text".to_string(),
            serde_json::Value::String(event.content_text()),
        );
    }
    serde_json::to_string(&v)
}

/// Shared fork implementation (P1-04). With `startup`, the child id is
/// caller-generated (the CLI assembler pre-builds the child registry and
/// browser profile against it) and the complete startup event batch —
/// SessionCreated, SystemPromptSet, ModeSet, ModelSet, ActiveToolsSet and the
/// branch summary — commits in the SAME transaction as the copied parent
/// events, so a crash mid-fork cannot leave a semantically incomplete child.
async fn fork_session_inner(
    db: &SessionDB,
    id: &str,
    fork_point: u64,
    new_title: &str,
    startup: Option<crate::store::ForkStartupSpec>,
) -> Result<Session, SessionError> {
    let parent: Session = db
        .get_session(id)
        .await?
        .ok_or(SessionError::NotFound(id.to_string()))?;
    let events = db.get_events(id).await?;
    let events_at_fork: Vec<StoredEvent> = events
        .into_iter()
        .filter(|e| e.event_index <= fork_point)
        .collect();
    let replayed_at_fork = crate::replay::replay_events(id, &events_at_fork);
    let forked_events: Vec<StoredEvent> = events_at_fork
        .into_iter()
        .filter(|e| !is_session_lifecycle_metadata_event(&e.event))
        .collect();

    // Pre-compute everything that doesn't need the DB lock so that the
    // transaction below stays short.
    let new_id = startup
        .as_ref()
        .map(|spec| spec.new_session_id.clone())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    // P1-12: same path-safety rule as session creation — the child id names a
    // directory under the sessions dir.
    validate_session_id_for_path(&new_id)?;
    let now = chrono::Utc::now();
    let now_str = now.to_rfc3339();
    let (new_mode, new_model, new_system_prompt) = if replayed_at_fork.semantic_complete {
        (
            replayed_at_fork.session.mode.clone(),
            replayed_at_fork.model.clone().or(parent.model.clone()),
            replayed_at_fork
                .system_prompt
                .clone()
                .or(parent.system_prompt.clone()),
        )
    } else {
        (
            parent.mode.clone(),
            parent.model.clone(),
            parent.system_prompt.clone(),
        )
    };
    // The startup batch always carries a prompt (P1-04): the child must be
    // semantically complete even when the parent predates stored prompts.
    let new_system_prompt = match (new_system_prompt, &startup) {
        (Some(prompt), _) => Some(prompt),
        (None, Some(spec)) => Some(spec.fallback_system_prompt.clone()),
        (None, None) => None,
    };
    let new_tags = parent.tags.clone();
    let parent_id = id.to_string();
    let title = new_title.to_string();

    // Serialize forked events up front so the transaction body is purely DB ops.
    let prepared_events: Vec<(u64, String, String, String)> = forked_events
        .iter()
        .map(|stored| {
            let event_type = event_type_str(&stored.event).to_string();
            let content_text = stored.event.content_text();
            let mut v: serde_json::Value = serde_json::to_value(&stored.event)?;
            if let Some(obj) = v.as_object_mut() {
                obj.insert(
                    "content_text".to_string(),
                    serde_json::Value::String(content_text),
                );
            }
            let event_data = serde_json::to_string(&v)?;
            Ok::<_, serde_json::Error>((
                stored.event_index,
                event_type,
                event_data,
                now_str.clone(),
            ))
        })
        .collect::<Result<_, _>>()?;

    let session = Session {
        id: new_id.clone(),
        case_id: parent.case_id.clone(),
        title: Some(title.clone()),
        mode: new_mode.clone(),
        model: new_model.clone(),
        model_config: None,
        system_prompt: new_system_prompt.clone(),
        parent_session_id: Some(parent_id.clone()),
        fork_point: Some(fork_point),
        source: "fork".into(),
        tags: new_tags.clone(),
        started_at: now,
        ended_at: None,
        end_reason: None,
        message_count: prepared_events
            .iter()
            .filter(|(_, t, _, _)| t == "user_message")
            .count() as u64,
        tool_call_count: prepared_events
            .iter()
            .filter(|(_, t, _, _)| t == "tool_call")
            .count() as u64,
        subagent_count: 0,
        input_tokens: 0,
        output_tokens: 0,
        estimated_cost_usd: 0.0,
        goal_condition: None,
        goal_achieved: false,
    };

    // Serialise the startup batch up front (same pre-transaction rule as the
    // copied events); its indices continue after the highest retained parent
    // index so ordering stays monotonic.
    let mut prepared_extra: Vec<(u64, String, String, String)> = Vec::new();
    if let Some(spec) = &startup {
        let mut next_index = prepared_events
            .iter()
            .map(|(index, _, _, _)| *index)
            .max()
            .map(|max| max + 1)
            .unwrap_or(0);
        for event in spec.to_events(&session) {
            prepared_extra.push((
                next_index,
                event_type_str(&event).to_string(),
                serialize_event_for_storage(&event)?,
                now_str.clone(),
            ));
            next_index += 1;
        }
    }

    // Run create + appends inside a single transaction with retry on
    // contention. The closure returns the necessary state on success.
    let new_id_for_retry = new_id.clone();
    let title_for_retry = title.clone();
    let mode_for_retry = new_mode.clone();
    let model_for_retry = new_model.clone();
    let prompt_for_retry = new_system_prompt.clone();
    let tags_for_retry = new_tags.clone();
    let parent_id_for_retry = parent_id.clone();
    let case_id_for_retry = parent.case_id.clone();
    let now_str_for_retry = now_str.clone();
    let prepared_for_retry = prepared_events.clone();
    let prepared_extra_for_retry = prepared_extra.clone();

    db.write_contention
        .with_db_retry(|| {
            let new_id = new_id_for_retry.clone();
            let title = title_for_retry.clone();
            let mode = mode_for_retry.clone();
            let model = model_for_retry.clone();
            let system_prompt = prompt_for_retry.clone();
            let tags = tags_for_retry.clone();
            let parent_id = parent_id_for_retry.clone();
            let case_id = case_id_for_retry.clone();
            let now_str = now_str_for_retry.clone();
            let prepared = prepared_for_retry.clone();
            let prepared_extra = prepared_extra_for_retry.clone();
            async move {
                let mut conn = db.conn.lock().await;
                let tx = conn.transaction()?;

                tx.execute(
                    "INSERT INTO sessions (id, case_id, title, mode, model, system_prompt, parent_session_id, fork_point, source, tags, started_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                    params![
                        new_id,
                        case_id,
                        title,
                        mode_to_str(&mode),
                        model,
                        system_prompt,
                        parent_id,
                        fork_point as i64,
                        "fork",
                        serde_json::to_string(&tags).unwrap_or_default(),
                        now_str,
                    ],
                )?;

                let mut user_msg_count: i64 = 0;
                let mut tool_call_count: i64 = 0;
                for (event_index, event_type, event_data, ts) in prepared.iter() {
                    tx.execute(
                        "INSERT INTO events (session_id, event_index, event_type, event_data, timestamp)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![new_id, *event_index as i64, event_type, event_data, ts],
                    )?;
                    if event_type == "user_message" {
                        user_msg_count += 1;
                    }
                    if event_type == "tool_call" {
                        tool_call_count += 1;
                    }
                }
                for (event_index, event_type, event_data, ts) in prepared_extra.iter() {
                    tx.execute(
                        "INSERT INTO events (session_id, event_index, event_type, event_data, timestamp)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![new_id, *event_index as i64, event_type, event_data, ts],
                    )?;
                }
                if user_msg_count > 0 || tool_call_count > 0 {
                    tx.execute(
                        "UPDATE sessions SET message_count = message_count + ?1,
                                              tool_call_count = tool_call_count + ?2
                         WHERE id = ?3",
                        params![user_msg_count, tool_call_count, new_id],
                    )?;
                }

                tx.commit()?;
                Ok::<_, rusqlite::Error>(())
            }
        })
        .await?;

    Ok(session)
}

/// Open-time connection setup: per-connection pragmas, then all pending
/// schema migrations in ONE immediate transaction (P1-04). busy_timeout comes
/// first so every later statement waits on contended locks; the WAL switch is
/// only issued when the file is not already in WAL mode (switching journal
/// modes never consults the busy handler, so callers retry the whole setup on
/// BUSY). BEGIN IMMEDIATE takes the database write lock before the version
/// read, so the version check, every pending DDL batch and the version writes
/// commit or roll back together — a second process opening the same
/// unmigrated database waits, then observes the fully migrated schema instead
/// of racing the DDL. (Migration texts carry no PRAGMAs: journal_mode /
/// foreign_keys cannot change inside a transaction.)
fn setup_connection(conn: &mut Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch("PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;")?;
    let journal_mode: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
    }

    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute_batch(schema::schema_version_table())?;

    let current_version: u32 = tx
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);

    for (i, migration) in schema::MIGRATIONS.iter().enumerate() {
        let version = (i + 1) as u32;
        if version > current_version {
            tx.execute_batch(migration)?;
            tx.execute(
                "INSERT INTO schema_version (version) VALUES (?1)",
                params![version],
            )?;
            tracing::info!(version, "applied schema migration");
        }
    }
    tx.commit()?;
    Ok(())
}

/// Bounded open-time setup attempts against a concurrently migrating peer
/// (P1-04): ~100 attempts at up to 50ms ≈ 5s, matching the busy timeout.
const OPEN_SETUP_MAX_ATTEMPTS: u32 = 100;

fn is_contention_error(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(code, _)
            if matches!(
                code.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
    )
}

fn is_session_lifecycle_metadata_event(event: &Event) -> bool {
    matches!(
        event,
        Event::SessionCreated { .. }
            | Event::SessionEnded { .. }
            | Event::SessionModeSet { .. }
            | Event::SessionSystemPromptSet { .. }
            | Event::SessionModelSet { .. }
            | Event::ActiveToolsSet { .. }
    )
}

pub(crate) fn event_type_str(event: &Event) -> &'static str {
    match event {
        Event::SessionCreated { .. } => "session_created",
        Event::SessionEnded { .. } => "session_ended",
        Event::SessionModeSet { .. } => "session_mode_set",
        Event::SessionSystemPromptSet { .. } => "session_system_prompt_set",
        Event::SessionModelSet { .. } => "session_model_set",
        Event::ActiveToolsSet { .. } => "active_tools_set",
        Event::UserMessage { .. } => "user_message",
        Event::TurnComplete { .. } => "turn_complete",
        Event::GoalSet { .. } => "goal_set",
        Event::GoalEvaluated { .. } => "goal_evaluated",
        Event::GoalCleared { .. } => "goal_cleared",
        Event::GoalProgress { .. } => "goal_progress",
        Event::SubtaskUpdate { .. } => "subtask_update",
        Event::Thinking { .. } => "thinking",
        Event::ToolCall { .. } => "tool_call",
        Event::ToolResult { .. } => "tool_result",
        Event::ToolBlocked { .. } => "tool_blocked",
        Event::TargetDiscovered { .. } => "target_discovered",
        Event::AttackSurfaceUpdate { .. } => "attack_surface_update",
        Event::VulnerabilityFound { .. } => "vulnerability_found",
        Event::FindingRecorded { .. } => "finding_recorded",
        Event::CodePatternFound { .. } => "code_pattern_found",
        Event::ReverseInsight { .. } => "reverse_insight",
        Event::CredentialFound { .. } => "credential_found",
        Event::HostCompromised { .. } => "host_compromised",
        Event::LateralMovement { .. } => "lateral_movement",
        Event::NetworkTopologyUpdate { .. } => "network_topology_update",
        Event::EvidenceObserved { .. } => "evidence_observed",
        Event::FactRecorded { .. } => "fact_recorded",
        Event::HypothesisProposed { .. } => "hypothesis_proposed",
        Event::PredictionMade { .. } => "prediction_made",
        Event::ExperimentPlanned { .. } => "experiment_planned",
        Event::HypothesisSupported { .. } => "hypothesis_supported",
        Event::HypothesisContradicted { .. } => "hypothesis_contradicted",
        Event::HypothesisRejected { .. } => "hypothesis_rejected",
        Event::HypothesisConfirmed { .. } => "hypothesis_confirmed",
        Event::ConclusionDrawn { .. } => "conclusion_drawn",
        Event::DirectiveSet { .. } => "directive_set",
        Event::ReflectionRecorded { .. } => "reflection_recorded",
        Event::HypothesisUpdate { .. } => "hypothesis_update",
        Event::AdvisorAction { .. } => "advisor_action",
        Event::MemoryStored { .. } => "memory_stored",
        Event::MemoryRecalled { .. } => "memory_recalled",
        Event::MemoryConsolidated { .. } => "memory_consolidated",
        Event::ContextSnapshotTaken { .. } => "context_snapshot_taken",
        Event::ContextSwitched { .. } => "context_switched",
        Event::DashboardUpdated { .. } => "dashboard_updated",
        Event::CompressionApplied { .. } => "compression_applied",
        Event::BranchSummary { .. } => "branch_summary",
        Event::SkillInjected { .. } => "skill_injected",
        Event::KnowledgeInjected { .. } => "knowledge_injected",
        Event::HumanFeedback { .. } => "human_feedback",
        Event::LearningReviewStarted { .. } => "learning_review_started",
        Event::LearningReviewCompleted { .. } => "learning_review_completed",
        Event::LearningCandidateRejected { .. } => "learning_candidate_rejected",
        Event::MemoryWriteStaged { .. } => "memory_write_staged",
        Event::MemoryRejected { .. } => "memory_rejected",
        Event::MemoryConflictDetected { .. } => "memory_conflict_detected",
        Event::MemoryStatusChanged { .. } => "memory_status_changed",
        Event::SubAgentSpawned { .. } => "subagent_spawned",
        Event::SubAgentCompleted { .. } => "subagent_completed",
        Event::SubAgentProgress { .. } => "subagent_progress",
        Event::ReportGenerated { .. } => "report_generated",
        Event::ProgramScopeSet { .. } => "program_scope_set",
        Event::AssetRecorded { .. } => "asset_recorded",
    }
}

fn mode_to_str(mode: &SessionMode) -> &'static str {
    match mode {
        SessionMode::Pentest => "pentest",
        SessionMode::CodeAudit => "code_audit",
        SessionMode::Reverse => "reverse",
        SessionMode::SecurityResearch => "security_research",
        SessionMode::Mixed => "mixed",
    }
}

fn str_to_mode(s: &str) -> SessionMode {
    match s {
        "code_audit" => SessionMode::CodeAudit,
        "reverse" => SessionMode::Reverse,
        "security_research" => SessionMode::SecurityResearch,
        "mixed" => SessionMode::Mixed,
        _ => SessionMode::Pentest,
    }
}

fn end_reason_to_str(reason: &EndReason) -> &'static str {
    match reason {
        EndReason::UserQuit => "user_quit",
        EndReason::GoalAchieved => "goal_achieved",
        EndReason::Aborted => "aborted",
        EndReason::Error => "error",
    }
}

fn str_to_end_reason(s: &str) -> Option<EndReason> {
    match s {
        "user_quit" => Some(EndReason::UserQuit),
        "goal_achieved" => Some(EndReason::GoalAchieved),
        "aborted" => Some(EndReason::Aborted),
        "error" => Some(EndReason::Error),
        _ => None,
    }
}

fn parse_datetime(s: &str) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&chrono::Utc))
        .unwrap_or_else(|_| chrono::Utc::now())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a unique temp DB path. We avoid pulling in `tempfile` as a dep.
    fn temp_db_path(label: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "holmes-session-{}-{}-{}.sqlite",
            label,
            std::process::id(),
            uuid::Uuid::new_v4(),
        ));
        p
    }

    #[tokio::test]
    async fn round_trip_session_and_events() {
        let path = temp_db_path("roundtrip");
        let db = SessionDB::open(&path).await.expect("open db");

        let session = db
            .create_session(CreateSessionParams {
                id: None,
                title: Some("test session".into()),
                mode: Some(SessionMode::Pentest),
                model: Some("claude-sonnet-4-5".into()),
                system_prompt: Some("be helpful".into()),
                parent_session_id: None,
                fork_point: None,
                source: Some("test".into()),
                tags: vec!["unit".into(), "round-trip".into()],
            })
            .await
            .expect("create session");

        // Mix of events including content with characters that previously
        // exercised the manual JSON splicing (quotes, backslashes, newlines,
        // unicode).
        let events = vec![
            Event::UserMessage {
                content: "hello \"world\"\n with \\ backslash and 中文".into(),
                timestamp: chrono::Utc::now(),
            },
            Event::Thinking {
                content: "let me think about this".into(),
                reasoning_type: Some("plan".into()),
            },
            Event::UserMessage {
                content: "second message".into(),
                timestamp: chrono::Utc::now(),
            },
        ];

        for e in &events {
            db.append_event(&session.id, e).await.expect("append event");
        }

        let stored = db.get_events(&session.id).await.expect("get events");
        assert_eq!(stored.len(), events.len(), "event count mismatch");

        for (i, (got, want)) in stored.iter().zip(events.iter()).enumerate() {
            assert_eq!(got.event_index, i as u64, "event_index ordering");
            // Re-serialize both sides via Value to compare structurally
            // (avoids field-order or float-formatting quirks).
            let got_v = serde_json::to_value(&got.event).unwrap();
            let want_v = serde_json::to_value(want).unwrap();
            assert_eq!(got_v, want_v, "event {} did not round-trip", i);
        }

        // Cleanup. Best-effort — ignore errors on shared CI temp dirs.
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn fork_session_excludes_parent_lifecycle_metadata() {
        let path = temp_db_path("fork-lifecycle");
        let db = SessionDB::open(&path).await.expect("open db");

        let parent = db
            .create_session(CreateSessionParams {
                id: Some("parent_session".into()),
                title: Some("parent".into()),
                mode: Some(SessionMode::Pentest),
                model: Some("parent-model".into()),
                system_prompt: Some("parent prompt".into()),
                parent_session_id: None,
                fork_point: None,
                source: Some("test".into()),
                tags: vec!["parent-tag".into()],
            })
            .await
            .expect("create parent session");
        let now = chrono::Utc::now();

        let parent_startup_events = [
            Event::SessionCreated {
                id: parent.id.clone(),
                title: parent.title.clone(),
                mode: SessionMode::Pentest,
                model: Some("parent-model".into()),
                system_prompt: Some("parent prompt".into()),
                parent_id: None,
                fork_point: None,
                created_at: now,
                tags: vec!["parent-tag".into()],
            },
            Event::SessionSystemPromptSet {
                prompt_hash: "parent-prompt-hash".into(),
                content: "parent prompt".into(),
                source: "parent-startup".into(),
                timestamp: now,
            },
            Event::SessionModeSet {
                mode: SessionMode::SecurityResearch,
                source: Some("parent-startup".into()),
                timestamp: Some(now),
            },
            Event::SessionModelSet {
                model: "parent-model".into(),
                provider: Some("parent-provider".into()),
                source: "parent-startup".into(),
                timestamp: now,
            },
            Event::ActiveToolsSet {
                tool_names: vec!["parent_tool".into()],
                source: "parent-startup".into(),
                timestamp: now,
            },
        ];

        for event in parent_startup_events {
            db.append_event(&parent.id, &event)
                .await
                .expect("append parent startup event");
        }
        db.append_event(
            &parent.id,
            &Event::UserMessage {
                content: "first turn".into(),
                timestamp: now,
            },
        )
        .await
        .expect("append user event");
        db.append_event(
            &parent.id,
            &Event::Thinking {
                content: "keep this history".into(),
                reasoning_type: Some("trace".into()),
            },
        )
        .await
        .expect("append thinking event");
        let fork_point = db
            .append_event(
                &parent.id,
                &Event::SessionEnded {
                    reason: EndReason::UserQuit,
                    summary: Some("parent ended".into()),
                },
            )
            .await
            .expect("append lifecycle end event");

        let child = db
            .fork_session(&parent.id, fork_point, "child")
            .await
            .expect("fork session");

        let child_events = db.get_events(&child.id).await.expect("child events");
        assert!(
            child_events.iter().any(|stored| matches!(
                &stored.event,
                Event::UserMessage { content, .. } if content == "first turn"
            )),
            "fork should preserve conversational history"
        );
        assert!(
            child_events.iter().any(|stored| matches!(
                &stored.event,
                Event::Thinking { content, .. } if content == "keep this history"
            )),
            "fork should preserve meaningful assistant history"
        );
        assert!(
            child_events.iter().all(|stored| !matches!(
                &stored.event,
                Event::SessionCreated { .. }
                    | Event::SessionSystemPromptSet { .. }
                    | Event::SessionModeSet { .. }
                    | Event::SessionModelSet { .. }
                    | Event::ActiveToolsSet { .. }
                    | Event::SessionEnded { .. }
            )),
            "child fork copied parent lifecycle metadata: {child_events:?}"
        );
        assert!(
            child_events.iter().all(|stored| !matches!(
                &stored.event,
                Event::SessionCreated { id, .. } if id == &parent.id
            )),
            "child must not contain parent SessionCreated payload"
        );
        assert_eq!(child.message_count, 1);
        assert_eq!(child.tool_call_count, 0);

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn goal_condition_can_be_set_cleared_and_marked_achieved() {
        let path = temp_db_path("goal");
        let db = SessionDB::open(&path).await.expect("open db");

        let session = db
            .create_session(CreateSessionParams {
                id: None,
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
            .expect("create session");

        db.set_goal_condition(&session.id, Some("prove the login behavior"))
            .await
            .expect("set goal");
        let with_goal = db
            .get_session(&session.id)
            .await
            .expect("get session")
            .expect("session");
        assert_eq!(
            with_goal.goal_condition.as_deref(),
            Some("prove the login behavior")
        );
        assert!(!with_goal.goal_achieved);

        db.mark_goal_achieved(&session.id)
            .await
            .expect("mark achieved");
        let achieved = db
            .get_session(&session.id)
            .await
            .expect("get session")
            .expect("session");
        assert!(achieved.goal_achieved);

        db.set_goal_condition(&session.id, None)
            .await
            .expect("clear goal");
        let cleared = db
            .get_session(&session.id)
            .await
            .expect("get session")
            .expect("session");
        assert!(cleared.goal_condition.is_none());
        assert!(!cleared.goal_achieved);

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn truncate_events_after_removes_future_events_and_rebuilds_counters() {
        let path = temp_db_path("truncate");
        let db = SessionDB::open(&path).await.expect("open db");

        let session = db
            .create_session(CreateSessionParams {
                id: None,
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
            .expect("create session");

        db.append_event(
            &session.id,
            &Event::UserMessage {
                content: "before checkpoint".into(),
                timestamp: chrono::Utc::now(),
            },
        )
        .await
        .expect("append user");
        let checkpoint = db
            .append_event(
                &session.id,
                &Event::ContextSnapshotTaken {
                    summary: "checkpoint".into(),
                    preserved_keys: Vec::new(),
                    active_contexts: Vec::new(),
                },
            )
            .await
            .expect("append checkpoint");
        db.append_event(
            &session.id,
            &Event::GoalSet {
                condition: "future goal".into(),
                plan: None,
                subtasks: Vec::new(),
            },
        )
        .await
        .expect("append goal");
        db.append_event(
            &session.id,
            &Event::UserMessage {
                content: "after checkpoint".into(),
                timestamp: chrono::Utc::now(),
            },
        )
        .await
        .expect("append user");

        db.truncate_events_after(&session.id, checkpoint)
            .await
            .expect("truncate");

        let events = db.get_events(&session.id).await.expect("events");
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events.last().unwrap().event,
            Event::ContextSnapshotTaken { .. }
        ));

        let session = db
            .get_session(&session.id)
            .await
            .expect("session")
            .expect("session exists");
        assert_eq!(session.message_count, 1);
        assert_eq!(session.tool_call_count, 0);
        assert!(session.goal_condition.is_none());
        assert!(!session.goal_achieved);

        let _ = std::fs::remove_file(&path);
    }
}
