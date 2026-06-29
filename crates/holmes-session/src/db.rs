use holmes_core::event::{Event, StoredEvent};
use holmes_core::types::*;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::compaction_archive::CompactionArchive;
use crate::schema;
use crate::write_contention::WriteContention;

pub struct SessionDB {
    conn: Arc<Mutex<Connection>>,
    sessions_dir: PathBuf,
    write_contention: WriteContention,
    write_count: Arc<Mutex<u64>>,
}

impl SessionDB {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, SessionError> {
        let path = path.as_ref();
        let conn = Connection::open(path)?;
        let sessions_dir = path
            .parent()
            .map(|parent| parent.join("sessions"))
            .unwrap_or_else(|| PathBuf::from("sessions"));
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=1000; PRAGMA foreign_keys=ON;",
        )?;

        // Run migrations
        conn.execute_batch(schema::schema_version_table())?;

        let current_version: u32 = conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_version",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);

        for (i, migration) in schema::MIGRATIONS.iter().enumerate() {
            let version = (i + 1) as u32;
            if version > current_version {
                conn.execute_batch(migration)?;
                conn.execute(
                    "INSERT INTO schema_version (version) VALUES (?1)",
                    params![version],
                )?;
                tracing::info!(version, "applied schema migration");
            }
        }

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            sessions_dir,
            write_contention: WriteContention::new(),
            write_count: Arc::new(Mutex::new(0)),
        })
    }

    pub async fn create_session(
        &self,
        params: CreateSessionParams,
    ) -> Result<Session, SessionError> {
        let id = params
            .id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let now = chrono::Utc::now().to_rfc3339();
        let started_at = chrono::Utc::now();

        let id_for_insert = id.clone();
        let title_for_insert = params.title.clone();
        let mode_for_insert = params.mode.clone();
        let model_for_insert = params.model.clone();
        let system_prompt_for_insert = params.system_prompt.clone();
        let parent_for_insert = params.parent_session_id.clone();
        let fork_for_insert = params.fork_point;
        let source_for_insert = params.source.clone();
        let tags_for_insert = params.tags.clone();
        let now_for_insert = now.clone();

        self.write_contention.with_retry(|| {
            let id = id_for_insert.clone();
            let title = title_for_insert.clone();
            let mode = mode_for_insert.clone();
            let model = model_for_insert.clone();
            let system_prompt = system_prompt_for_insert.clone();
            let parent = parent_for_insert.clone();
            let fork = fork_for_insert;
            let source = source_for_insert.clone();
            let tags = tags_for_insert.clone();
            let now = now_for_insert.clone();
            async move {
                let conn = self.conn.lock().await;
                conn.execute(
                    "INSERT INTO sessions (id, title, mode, model, system_prompt, parent_session_id, fork_point, source, tags, started_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![
                        id,
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
                Ok::<_, rusqlite::Error>(())
            }
        }).await?;

        let session_dir = self.sessions_dir.join(&id);
        std::fs::create_dir_all(session_dir.join("tool-results")).ok();
        std::fs::create_dir_all(session_dir.join("compactions")).ok();

        Ok(Session {
            id,
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
            message_count: 0,
            tool_call_count: 0,
            subagent_count: 0,
            input_tokens: 0,
            output_tokens: 0,
            estimated_cost_usd: 0.0,
            goal_condition: None,
            goal_achieved: false,
        })
    }

    pub async fn append_event(&self, session_id: &str, event: &Event) -> Result<u64, SessionError> {
        let event_type = event_type_str(event);
        let content_text = event.content_text();
        let timestamp = chrono::Utc::now().to_rfc3339();

        // Wrap event_data to include content_text for FTS5 by injecting it into
        // the serialized JSON object via serde_json::Value (safe and structured).
        let mut v: serde_json::Value = serde_json::to_value(event)?;
        if let Some(obj) = v.as_object_mut() {
            obj.insert(
                "content_text".to_string(),
                serde_json::Value::String(content_text),
            );
        }
        let event_data = serde_json::to_string(&v)?;

        let session_id_owned = session_id.to_string();
        let event_type_owned = event_type.to_string();
        let event_data_owned = event_data;
        let timestamp_owned = timestamp;
        let event_for_match = event.clone();

        let id = self.write_contention.with_retry(|| {
            let session_id = session_id_owned.clone();
            let event_type = event_type_owned.clone();
            let event_data = event_data_owned.clone();
            let timestamp = timestamp_owned.clone();
            let event = event_for_match.clone();
            async move {
                let conn = self.conn.lock().await;
                let idx: u64 = conn
                    .query_row(
                        "SELECT COALESCE(MAX(event_index), -1) + 1 FROM events WHERE session_id = ?1",
                        params![session_id],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);

                conn.execute(
                    "INSERT INTO events (session_id, event_index, event_type, event_data, timestamp)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![session_id, idx as i64, event_type, event_data, timestamp],
                )?;

                if matches!(event, Event::UserMessage { .. }) {
                    conn.execute(
                        "UPDATE sessions SET message_count = message_count + 1 WHERE id = ?1",
                        params![session_id],
                    )?;
                }
                if matches!(event, Event::ToolCall { .. }) {
                    conn.execute(
                        "UPDATE sessions SET tool_call_count = tool_call_count + 1 WHERE id = ?1",
                        params![session_id],
                    )?;
                }

                Ok::<_, rusqlite::Error>(idx)
            }
        }).await?;

        // Periodic WAL checkpoint
        let mut count = self.write_count.lock().await;
        *count += 1;
        if *count % 50 == 0 {
            let conn = self.conn.lock().await;
            conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);").ok();
        }

        Ok(id)
    }

    pub async fn get_events(&self, session_id: &str) -> Result<Vec<StoredEvent>, SessionError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, session_id, event_index, turn_index, event_type, event_data, timestamp
             FROM events WHERE session_id = ?1 ORDER BY event_index",
        )?;

        let rows = stmt.query_map(params![session_id], |row| {
            let data: String = row.get(5)?;
            // event_data has a `content_text` field injected for FTS. Parse as
            // Value, drop that field, then deserialize the remainder as Event.
            let event: Event = parse_event_data(&data).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    5,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;

            let id: i64 = row.get(0)?;
            let event_index: i64 = row.get(2)?;
            let turn_index: Option<i64> = row.get(3)?;
            let timestamp_str: String = row.get(6)?;

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

            Ok(StoredEvent {
                id: id as u64,
                session_id: row.get(1)?,
                event_index: event_index as u64,
                turn_index: turn_index.map(|v| v as u64),
                timestamp,
                event,
            })
        })?;

        let mut events = Vec::new();
        for row in rows {
            events.push(row?);
        }
        Ok(events)
    }

    pub async fn session_workspace(
        &self,
        session_id: &str,
    ) -> Result<std::path::PathBuf, SessionError> {
        let path = self.sessions_dir.join(session_id);
        tokio::fs::create_dir_all(&path).await?;
        Ok(path)
    }

    pub async fn write_compaction_archive(
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

    pub async fn read_compaction_archive(
        &self,
        path: &str,
    ) -> Result<CompactionArchive, SessionError> {
        let content = tokio::fs::read_to_string(path).await?;
        Ok(serde_json::from_str(&content)?)
    }

    pub async fn list_sessions(
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

    pub async fn end_session(&self, id: &str, reason: EndReason) -> Result<(), SessionError> {
        let id_owned = id.to_string();
        let reason_owned = reason;
        self.write_contention
            .with_retry(|| {
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

    pub async fn reopen_session(&self, id: &str) -> Result<(), SessionError> {
        let id_owned = id.to_string();
        self.write_contention
            .with_retry(|| {
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

    pub async fn set_goal_condition(
        &self,
        id: &str,
        condition: Option<&str>,
    ) -> Result<(), SessionError> {
        let id_owned = id.to_string();
        let condition_owned = condition.map(ToOwned::to_owned);
        self.write_contention
            .with_retry(|| {
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

    pub async fn mark_goal_achieved(&self, id: &str) -> Result<(), SessionError> {
        let id_owned = id.to_string();
        self.write_contention
            .with_retry(|| {
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

    pub async fn get_session(&self, id: &str) -> Result<Option<Session>, SessionError> {
        let conn = self.conn.lock().await;
        let result = conn.query_row(
            "SELECT id, title, mode, model, model_config, system_prompt, parent_session_id,
                    fork_point, source, tags, started_at, ended_at, end_reason,
                    message_count, tool_call_count, subagent_count,
                    input_tokens, output_tokens, estimated_cost_usd,
                    goal_condition, goal_achieved
             FROM sessions WHERE id = ?1",
            params![id],
            |row| {
                let fork_point: Option<i64> = row.get(7)?;
                let message_count: i64 = row.get(13)?;
                let tool_call_count: i64 = row.get(14)?;
                let subagent_count: i64 = row.get(15)?;
                let input_tokens: i64 = row.get(16)?;
                let output_tokens: i64 = row.get(17)?;
                let goal_achieved: i64 = row.get(20)?;
                Ok(Session {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    mode: str_to_mode(&row.get::<_, String>(2)?),
                    model: row.get(3)?,
                    model_config: row
                        .get::<_, Option<String>>(4)?
                        .and_then(|v| serde_json::from_str(&v).ok()),
                    system_prompt: row.get(5)?,
                    parent_session_id: row.get(6)?,
                    fork_point: fork_point.map(|v| v as u64),
                    source: row.get(8)?,
                    tags: row
                        .get::<_, String>(9)
                        .ok()
                        .and_then(|t| serde_json::from_str(&t).ok())
                        .unwrap_or_default(),
                    started_at: parse_datetime(&row.get::<_, String>(10)?),
                    ended_at: row
                        .get::<_, Option<String>>(11)?
                        .map(|s| parse_datetime(&s)),
                    end_reason: row
                        .get::<_, Option<String>>(12)?
                        .and_then(|r| str_to_end_reason(&r)),
                    message_count: message_count as u64,
                    tool_call_count: tool_call_count as u64,
                    subagent_count: subagent_count as u64,
                    input_tokens: input_tokens as u64,
                    output_tokens: output_tokens as u64,
                    estimated_cost_usd: row.get(18)?,
                    goal_condition: row.get(19)?,
                    goal_achieved: goal_achieved != 0,
                })
            },
        );

        match result {
            Ok(session) => Ok(Some(session)),
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
                    let resolved = matches[0].clone();
                    drop(stmt);
                    drop(conn);
                    Box::pin(self.get_session(&resolved)).await
                } else {
                    Ok(None)
                }
            }
            Err(e) => Err(e.into()),
        }
    }

    pub async fn fork_session(
        &self,
        id: &str,
        fork_point: u64,
        new_title: &str,
    ) -> Result<Session, SessionError> {
        let parent = self
            .get_session(id)
            .await?
            .ok_or(SessionError::NotFound(id.to_string()))?;
        let events = self.get_events(id).await?;
        let forked_events: Vec<StoredEvent> = events
            .into_iter()
            .filter(|e| e.event_index <= fork_point)
            .collect();

        // Pre-compute everything that doesn't need the DB lock so that the
        // transaction below stays short.
        let new_id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now();
        let now_str = now.to_rfc3339();
        let new_mode = parent.mode.clone();
        let new_model = parent.model.clone();
        let new_system_prompt = parent.system_prompt.clone();
        let new_tags = parent.tags.clone();
        let parent_id = id.to_string();
        let title = new_title.to_string();

        // Serialize forked events up front so the transaction body is purely DB ops.
        let prepared_events: Vec<(String, String, String)> = forked_events
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
                Ok::<_, serde_json::Error>((event_type, event_data, now_str.clone()))
            })
            .collect::<Result<_, _>>()?;

        // Run create + appends inside a single transaction with retry on
        // contention. The closure returns the necessary state on success.
        let new_id_for_retry = new_id.clone();
        let title_for_retry = title.clone();
        let mode_for_retry = new_mode.clone();
        let model_for_retry = new_model.clone();
        let prompt_for_retry = new_system_prompt.clone();
        let tags_for_retry = new_tags.clone();
        let parent_id_for_retry = parent_id.clone();
        let now_str_for_retry = now_str.clone();
        let prepared_for_retry = prepared_events.clone();

        self.write_contention
            .with_retry(|| {
                let new_id = new_id_for_retry.clone();
                let title = title_for_retry.clone();
                let mode = mode_for_retry.clone();
                let model = model_for_retry.clone();
                let system_prompt = prompt_for_retry.clone();
                let tags = tags_for_retry.clone();
                let parent_id = parent_id_for_retry.clone();
                let now_str = now_str_for_retry.clone();
                let prepared = prepared_for_retry.clone();
                async move {
                    let mut conn = self.conn.lock().await;
                    let tx = conn.transaction()?;

                    tx.execute(
                        "INSERT INTO sessions (id, title, mode, model, system_prompt, parent_session_id, fork_point, source, tags, started_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                        params![
                            new_id,
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
                    for (idx, (event_type, event_data, ts)) in prepared.iter().enumerate() {
                        tx.execute(
                            "INSERT INTO events (session_id, event_index, event_type, event_data, timestamp)
                             VALUES (?1, ?2, ?3, ?4, ?5)",
                            params![new_id, idx as i64, event_type, event_data, ts],
                        )?;
                        if event_type == "user_message" {
                            user_msg_count += 1;
                        }
                        if event_type == "tool_call" {
                            tool_call_count += 1;
                        }
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

        Ok(Session {
            id: new_id,
            title: Some(title),
            mode: new_mode,
            model: new_model,
            model_config: None,
            system_prompt: new_system_prompt,
            parent_session_id: Some(parent_id),
            fork_point: Some(fork_point),
            source: "fork".into(),
            tags: new_tags,
            started_at: now,
            ended_at: None,
            end_reason: None,
            message_count: prepared_events
                .iter()
                .filter(|(t, _, _)| t == "user_message")
                .count() as u64,
            tool_call_count: prepared_events
                .iter()
                .filter(|(t, _, _)| t == "tool_call")
                .count() as u64,
            subagent_count: 0,
            input_tokens: 0,
            output_tokens: 0,
            estimated_cost_usd: 0.0,
            goal_condition: None,
            goal_achieved: false,
        })
    }

    pub async fn update_token_counts(
        &self,
        id: &str,
        delta: &TokenDelta,
    ) -> Result<(), SessionError> {
        let id_owned = id.to_string();
        let delta_owned = delta.clone();
        self.write_contention
            .with_retry(|| {
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

    pub async fn truncate_events_after(
        &self,
        session_id: &str,
        event_index: u64,
    ) -> Result<(), SessionError> {
        let session_id_owned = session_id.to_string();
        self.write_contention
            .with_retry(|| {
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

    pub async fn set_title(&self, id: &str, title: &str) -> Result<(), SessionError> {
        let id_owned = id.to_string();
        let title_owned = title.to_string();
        self.write_contention
            .with_retry(|| {
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

    pub async fn search_events(
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
                 WHERE json_extract(e.event_data, '$.content_text') LIKE ?1
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
                 WHERE events_fts MATCH ?1
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
}

// === Helper functions ===

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

fn event_type_str(event: &Event) -> &'static str {
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
        Event::SubAgentSpawned { .. } => "subagent_spawned",
        Event::SubAgentCompleted { .. } => "subagent_completed",
        Event::SubAgentProgress { .. } => "subagent_progress",
        Event::ReportGenerated { .. } => "report_generated",
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
