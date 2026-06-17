use holmes_core::event::{Event, StoredEvent};
use holmes_core::types::*;
use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::schema;
use crate::write_contention::WriteContention;

pub struct SessionDB {
    conn: Arc<Mutex<Connection>>,
    write_contention: WriteContention,
    write_count: Arc<Mutex<u64>>,
}

impl SessionDB {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, SessionError> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=1000; PRAGMA foreign_keys=ON;"
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
            write_contention: WriteContention::new(),
            write_count: Arc::new(Mutex::new(0)),
        })
    }

    pub async fn create_session(&self, params: CreateSessionParams) -> Result<Session, SessionError> {
        let id = params.id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
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
        let event_json = serde_json::to_string(event)?;
        let content_text = event.content_text();
        let timestamp = chrono::Utc::now().to_rfc3339();

        // Wrap event_data to include content_text for FTS5
        let escaped = content_text
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n");
        let event_data = if event_json.len() > 2 {
            // event_json starts with '{' — splice content_text in front
            format!(r#"{{"content_text":"{}",{}"#, escaped, &event_json[1..])
        } else {
            format!(r#"{{"content_text":"{}"}}"#, escaped)
        };

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
             FROM events WHERE session_id = ?1 ORDER BY event_index"
        )?;

        let rows = stmt.query_map(params![session_id], |row| {
            let data: String = row.get(5)?;
            // The event_data has content_text injected at the front. Strip it for deserialization.
            let event: Event = strip_content_text_and_parse(&data)
                .or_else(|_| serde_json::from_str(&data))
                .map_err(|e| rusqlite::Error::FromSqlConversionFailure(
                    5,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                ))?;

            let id: i64 = row.get(0)?;
            let event_index: i64 = row.get(2)?;
            let turn_index: Option<i64> = row.get(3)?;
            let timestamp_str: String = row.get(6)?;

            Ok(StoredEvent {
                id: id as u64,
                session_id: row.get(1)?,
                event_index: event_index as u64,
                turn_index: turn_index.map(|v| v as u64),
                timestamp: chrono::DateTime::parse_from_rfc3339(&timestamp_str)
                    .map(|d| d.with_timezone(&chrono::Utc))
                    .unwrap_or_else(|_| chrono::Utc::now()),
                event,
            })
        })?;

        let mut events = Vec::new();
        for row in rows {
            events.push(row?);
        }
        Ok(events)
    }

    pub async fn list_sessions(&self, filter: &SessionFilter) -> Result<Vec<SessionSummary>, SessionError> {
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
             FROM sessions s WHERE 1=1"
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
            sql.push_str(" AND s.id IN (SELECT session_id FROM events_fts WHERE events_fts MATCH ?)");
            param_values.push(Box::new(search.clone()));
        }

        sql.push_str(" ORDER BY s.started_at DESC");

        if let Some(limit) = filter.limit {
            sql.push_str(&format!(" LIMIT {}", limit));
        }
        if let Some(offset) = filter.offset {
            sql.push_str(&format!(" OFFSET {}", offset));
        }

        let params_refs: Vec<&dyn rusqlite::types::ToSql> = param_values.iter().map(|p| p.as_ref()).collect();
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
                end_reason: row.get::<_, Option<String>>(6)?.and_then(|r| str_to_end_reason(&r)),
                message_count: message_count as u64,
                parent_session_id: row.get(8)?,
                preview: row.get(9)?,
                last_active: row.get::<_, Option<String>>(10)?.map(|s| parse_datetime(&s)),
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
        self.write_contention.with_retry(|| {
            let id = id_owned.clone();
            let reason = reason_owned.clone();
            async move {
                let conn = self.conn.lock().await;
                conn.execute(
                    "UPDATE sessions SET ended_at = ?1, end_reason = ?2 WHERE id = ?3",
                    params![chrono::Utc::now().to_rfc3339(), end_reason_to_str(&reason), id],
                )?;
                Ok::<_, rusqlite::Error>(())
            }
        }).await?;
        Ok(())
    }

    pub async fn reopen_session(&self, id: &str) -> Result<(), SessionError> {
        let id_owned = id.to_string();
        self.write_contention.with_retry(|| {
            let id = id_owned.clone();
            async move {
                let conn = self.conn.lock().await;
                conn.execute(
                    "UPDATE sessions SET ended_at = NULL, end_reason = NULL WHERE id = ?1",
                    params![id],
                )?;
                Ok::<_, rusqlite::Error>(())
            }
        }).await?;
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
                    model_config: row.get::<_, Option<String>>(4)?.and_then(|v| serde_json::from_str(&v).ok()),
                    system_prompt: row.get(5)?,
                    parent_session_id: row.get(6)?,
                    fork_point: fork_point.map(|v| v as u64),
                    source: row.get(8)?,
                    tags: row.get::<_, String>(9).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default(),
                    started_at: parse_datetime(&row.get::<_, String>(10)?),
                    ended_at: row.get::<_, Option<String>>(11)?.map(|s| parse_datetime(&s)),
                    end_reason: row.get::<_, Option<String>>(12)?.and_then(|r| str_to_end_reason(&r)),
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
                    "SELECT id FROM sessions WHERE id LIKE ?1 ORDER BY started_at DESC LIMIT 2"
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

    pub async fn fork_session(&self, id: &str, fork_point: u64, new_title: &str) -> Result<Session, SessionError> {
        let parent = self.get_session(id).await?.ok_or(SessionError::NotFound(id.to_string()))?;
        let events = self.get_events(id).await?;
        let forked_events: Vec<StoredEvent> = events.into_iter().filter(|e| e.event_index <= fork_point).collect();

        let new_session = self.create_session(CreateSessionParams {
            id: None,
            title: Some(new_title.to_string()),
            mode: Some(parent.mode),
            model: parent.model,
            system_prompt: parent.system_prompt,
            parent_session_id: Some(id.to_string()),
            fork_point: Some(fork_point),
            source: Some("fork".into()),
            tags: parent.tags,
        }).await?;

        for evt in forked_events {
            self.append_event(&new_session.id, &evt.event).await?;
        }

        Ok(new_session)
    }

    pub async fn update_token_counts(&self, id: &str, delta: &TokenDelta) -> Result<(), SessionError> {
        let id_owned = id.to_string();
        let delta_owned = delta.clone();
        self.write_contention.with_retry(|| {
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
                    params![delta.input as i64, delta.output as i64, delta.cache_read as i64, delta.cache_write as i64, id],
                )?;
                Ok::<_, rusqlite::Error>(())
            }
        }).await?;
        Ok(())
    }

    pub async fn set_title(&self, id: &str, title: &str) -> Result<(), SessionError> {
        let id_owned = id.to_string();
        let title_owned = title.to_string();
        self.write_contention.with_retry(|| {
            let id = id_owned.clone();
            let title = title_owned.clone();
            async move {
                let conn = self.conn.lock().await;
                conn.execute("UPDATE sessions SET title = ?1 WHERE id = ?2", params![title, id])?;
                Ok::<_, rusqlite::Error>(())
            }
        }).await?;
        Ok(())
    }

    pub async fn search_events(&self, query: &str, top_k: u32) -> Result<Vec<SearchResult>, SessionError> {
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
    #[error("session not found: {0}")]
    NotFound(String),
}

// === Helper functions ===

/// Try to parse event_data that has a `content_text` field injected at the
/// front by removing that field before deserializing back into Event.
fn strip_content_text_and_parse(data: &str) -> Result<Event, serde_json::Error> {
    // First try as-is (Event has #[serde(tag = "type")] and will ignore unknown fields by default? No, it won't)
    // We deserialize via serde_json::Value, remove content_text, and re-serialize.
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
        Event::SkillInjected { .. } => "skill_injected",
        Event::KnowledgeInjected { .. } => "knowledge_injected",
        Event::HumanFeedback { .. } => "human_feedback",
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
