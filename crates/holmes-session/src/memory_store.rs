use chrono::{DateTime, Utc};
use holmes_core::types::*;
use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::embedding;
use crate::schema;
use crate::write_contention::WriteContention;

/// Weight of the lexical (FTS5/LIKE) component in hybrid recall; the embedding
/// cosine component takes the remainder.
const LEXICAL_WEIGHT: f64 = 0.6;
/// Pool multiplier: recall fetches `top_k * POOL` candidates per channel before
/// fusion and re-ranking.
const CANDIDATE_POOL: u32 = 4;

pub struct MemoryStore {
    conn: Arc<Mutex<Connection>>,
    write_contention: WriteContention,
}

#[derive(Debug, Clone)]
pub struct MemoryEntry {
    pub category: MemoryCategory,
    pub content: String,
    pub tags: Vec<String>,
    pub attack_type: Option<String>,
    pub tech_stack: Vec<String>,
    pub success: bool,
    pub relevance_score: f64,
    pub source_session_id: Option<String>,
    /// Provenance. Agent-inferred entries are recorded as such; the learning
    /// pipeline is responsible for staging them.
    pub source: MemorySource,
    pub confidence: f64,
    pub scope: MemoryScope,
    pub expires_at: Option<DateTime<Utc>>,
    /// Requested lifecycle status. `None` = default: `Active` for plain
    /// memories, `Staged` for skills. Skills can never be written `Active`
    /// directly — they only activate through `promote` (validation +
    /// approval gate).
    pub status: Option<MemoryStatus>,
    /// Explicit opt-in for case state that legitimately contains credentials
    /// (e.g. pre-compaction flush notes). Stored with the `sensitive` flag;
    /// without this opt-in, sensitive content is refused with
    /// `StoreError::Rejected`.
    pub allow_sensitive: bool,
    /// ID of the memory this one replaces.
    pub supersedes: Option<String>,
}

impl Default for MemoryEntry {
    fn default() -> Self {
        Self {
            category: MemoryCategory::default(),
            content: String::new(),
            tags: Vec::new(),
            attack_type: None,
            tech_stack: Vec::new(),
            success: false,
            relevance_score: 0.0,
            source_session_id: None,
            source: MemorySource::default(),
            confidence: 0.5,
            scope: MemoryScope::default(),
            expires_at: None,
            status: None,
            allow_sensitive: false,
            supersedes: None,
        }
    }
}

/// Result of a successful `store`.
#[derive(Debug, Clone)]
pub struct StoreOutcome {
    pub id: String,
    pub status: MemoryStatus,
    /// True when the entry was explicitly allowed despite screening positive.
    pub sensitive: bool,
    /// IDs of existing active memories this entry contradicts (same normalized
    /// content, opposite `success` verdict). Both sides are linked through
    /// `conflicts_with`.
    pub conflicts: Vec<String>,
}

/// One recalled memory plus its fused score.
#[derive(Debug, Clone)]
pub struct Recalled {
    pub memory: Memory,
    pub score: f64,
}

/// Hybrid recall result: the memories to inject plus conflict suppressions
/// (so the caller can emit `MemoryConflictDetected` audit events).
#[derive(Debug, Clone, Default)]
pub struct RecallBatch {
    pub hits: Vec<Recalled>,
    /// `(suppressed_id, chosen_id, reason)` — a conflicting pair was recalled;
    /// only the preferred one is in `hits`.
    pub suppressed: Vec<(String, String, String)>,
}

/// A lifecycle transition that was applied (returned so the runtime can emit
/// `MemoryStatusChanged`).
#[derive(Debug, Clone)]
pub struct StatusChange {
    pub memory_id: String,
    pub from: MemoryStatus,
    pub to: MemoryStatus,
}

#[derive(Debug)]
pub enum StoreError {
    /// Sensitive / prompt-injection content refused at the write boundary.
    Rejected(String),
    /// A lifecycle gate refused the transition (e.g. promote without
    /// validation, archive without disable, rollback without a parent).
    Blocked(String),
    NotFound(String),
    Sqlite(rusqlite::Error),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Rejected(reason) => write!(f, "memory write rejected: {reason}"),
            StoreError::Blocked(reason) => write!(f, "memory transition blocked: {reason}"),
            StoreError::NotFound(id) => write!(f, "memory not found: {id}"),
            StoreError::Sqlite(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        StoreError::Sqlite(error)
    }
}

const MEMORY_COLUMNS: &str = "m.id, m.category, m.content, m.tags, m.attack_type, m.tech_stack,
     m.success, m.relevance_score, m.source_session_id, m.consolidated_from, m.created_at,
     m.source, m.confidence, m.scope, m.status, m.last_verified_at, m.expires_at,
     m.conflicts_with, m.supersedes, m.sensitive, m.version, m.parent_version_id";

impl MemoryStore {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, rusqlite::Error> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=1000; PRAGMA foreign_keys=ON;",
        )?;

        // Ensure schema_version table and migrations are applied. The
        // `memories` table (along with its FTS5 virtual table and triggers)
        // is created by SessionDB schema migrations; run them here as well so
        // a MemoryStore can be opened on a fresh database without needing a
        // SessionDB instance to be created first.
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
            }
        }

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            write_contention: WriteContention::new(),
        })
    }

    pub async fn store(&self, entry: MemoryEntry) -> Result<StoreOutcome, StoreError> {
        // Sensitivity gate: refuse credentials / prompt-injection content
        // unless the caller explicitly opted in (case state such as flush
        // notes). The caller turns `Rejected` into a `MemoryRejected` audit
        // event.
        let flagged = holmes_core::screen_sensitive(&entry.content);
        if flagged.is_some() && !entry.allow_sensitive {
            return Err(StoreError::Rejected(flagged.unwrap_or_default()));
        }
        let sensitive = flagged.is_some();

        // Skills are always staged — they only become active through the
        // validation + approval gate in `promote`.
        let status = if matches!(entry.category, MemoryCategory::Skill) {
            MemoryStatus::Staged
        } else {
            entry.status.unwrap_or_default()
        };

        let id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let tags_json = serde_json::to_string(&entry.tags).unwrap_or_else(|_| "[]".into());
        let tech_json = serde_json::to_string(&entry.tech_stack).unwrap_or_else(|_| "[]".into());
        let embedding_json = embedding::embedding_to_json(&embedding::embed(&entry.content));
        let expires_at = entry.expires_at.map(|t| t.to_rfc3339());

        let id_clone = id.clone();
        self.write_contention
            .with_db_retry(|| {
                let id = id_clone.clone();
                let now = now.clone();
                let tags_json = tags_json.clone();
                let tech_json = tech_json.clone();
                let embedding_json = embedding_json.clone();
                let expires_at = expires_at.clone();
                let entry = entry.clone();
                async move {
                    let conn = self.conn.lock().await;
                    conn.execute(
                        "INSERT INTO memories (id, category, content, tags, attack_type, tech_stack,
                         success, relevance_score, source_session_id, created_at,
                         source, confidence, scope, status, expires_at, supersedes, sensitive,
                         embedding, version)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                                 ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, 1)",
                        params![
                            id,
                            category_to_str(&entry.category),
                            entry.content,
                            tags_json,
                            entry.attack_type,
                            tech_json,
                            entry.success as i32,
                            entry.relevance_score,
                            entry.source_session_id,
                            now,
                            source_to_str(entry.source),
                            entry.confidence,
                            scope_to_str(entry.scope),
                            status_to_str(status),
                            expires_at,
                            entry.supersedes,
                            sensitive as i32,
                            embedding_json,
                        ],
                    )?;
                    Ok::<_, rusqlite::Error>(())
                }
            })
            .await?;

        // Contradiction detection: an active memory in the same category with
        // the same normalized content but the opposite success verdict is a
        // conflict; link both sides so recall never injects both as fact.
        let conflicts = self.link_conflicts(&id, &entry).await?;

        Ok(StoreOutcome {
            id,
            status,
            sensitive,
            conflicts,
        })
    }

    /// Link the new memory with existing active memories it contradicts.
    async fn link_conflicts(
        &self,
        new_id: &str,
        entry: &MemoryEntry,
    ) -> Result<Vec<String>, StoreError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, content, success, conflicts_with FROM memories
             WHERE status = 'active' AND category = ?1 AND id != ?2",
        )?;
        let rows = stmt
            .query_map(params![category_to_str(&entry.category), new_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i32>(2)? != 0,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let normalized_new = normalize_text(&entry.content);
        let mut conflicts = Vec::new();
        for (other_id, other_content, other_success, other_conflicts_json) in rows {
            if other_success == entry.success || normalize_text(&other_content) != normalized_new {
                continue;
            }
            let mut other_conflicts: Vec<String> =
                serde_json::from_str(&other_conflicts_json).unwrap_or_default();
            if !other_conflicts.iter().any(|c| c == new_id) {
                other_conflicts.push(new_id.to_string());
                let json = serde_json::to_string(&other_conflicts).unwrap_or_else(|_| "[]".into());
                conn.execute(
                    "UPDATE memories SET conflicts_with = ?1 WHERE id = ?2",
                    params![json, other_id],
                )?;
            }
            conflicts.push(other_id);
        }

        if !conflicts.is_empty() {
            let json = serde_json::to_string(&conflicts).unwrap_or_else(|_| "[]".into());
            conn.execute(
                "UPDATE memories SET conflicts_with = ?1 WHERE id = ?2",
                params![json, new_id],
            )?;
        }
        Ok(conflicts)
    }

    /// Legacy lexical search across **all** statuses. Kept for internal
    /// consumers (e.g. learning dedup) that must also see staged entries;
    /// the turn-time injection path is `recall`.
    pub async fn search(&self, query: &str, top_k: u32) -> Result<Vec<Memory>, rusqlite::Error> {
        let conn = self.conn.lock().await;
        lexical_candidates(&conn, query, top_k, false)
            .map(|candidates| candidates.into_iter().map(|(memory, _)| memory).collect())
    }

    /// Turn-time recall: active, unexpired memories only. FTS/LIKE lexical
    /// candidates are fused with local-embedding cosine similarity when
    /// `hybrid` is on and embeddings are present; otherwise this degrades to
    /// the pure lexical path. Fused scores are re-ranked by confidence,
    /// freshness and scope, then conflicting pairs are suppressed (only the
    /// preferred memory is returned). Returned memories get their usage
    /// telemetry (`access_count` / `accessed_at`) bumped.
    pub async fn recall(
        &self,
        query: &str,
        top_k: u32,
        hybrid: bool,
        current_session_id: Option<&str>,
    ) -> Result<RecallBatch, StoreError> {
        let now = Utc::now();
        let now_str = now.to_rfc3339();
        let pool = top_k.saturating_mul(CANDIDATE_POOL).max(top_k).max(1);

        let conn = self.conn.lock().await;
        let lexical = lexical_candidates_active(&conn, query, pool, &now_str)?;

        // Semantic channel: cosine similarity over stored embeddings.
        let semantic: Vec<(String, f64)> = if hybrid {
            let expanded = embedding::expand_query(query);
            let query_embedding = embedding::embed(&expanded);
            let mut stmt = conn.prepare(
                "SELECT id, embedding FROM memories
                 WHERE status = 'active' AND embedding IS NOT NULL
                 AND (expires_at IS NULL OR expires_at > ?1)",
            )?;
            let rows = stmt
                .query_map(params![now_str], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let mut scored: Vec<(String, f64)> = rows
                .into_iter()
                .filter_map(|(id, json)| {
                    let vector = embedding::embedding_from_json(&json)?;
                    let sim = embedding::cosine(&query_embedding, &vector);
                    (sim > 0.0).then_some((id, sim))
                })
                .collect();
            scored.sort_by(|a, b| b.1.total_cmp(&a.1));
            scored.truncate(pool as usize);
            scored
        } else {
            Vec::new()
        };

        // Fuse: rank-normalized lexical score + cosine semantic score. BM25
        // ranks are negative (lower = better); normalizing against the best
        // rank gives ties (identical content) identical scores.
        let best_rank = lexical
            .iter()
            .map(|(_, rank)| *rank)
            .fold(f64::INFINITY, f64::min);
        let mut fused: std::collections::HashMap<String, (Memory, f64)> =
            std::collections::HashMap::new();
        for (memory, rank) in lexical {
            let lex_score = if best_rank < 0.0 {
                rank / best_rank
            } else {
                1.0
            };
            fused.insert(memory.id.clone(), (memory, LEXICAL_WEIGHT * lex_score));
        }
        if !fused.is_empty() || !semantic.is_empty() {
            for (id, sim) in &semantic {
                if let Some((_, score)) = fused.get_mut(id) {
                    *score += (1.0 - LEXICAL_WEIGHT) * sim;
                }
            }
            // Semantic-only hits (no lexical match) enter the competition with
            // their cosine score so paraphrases can be recalled at all.
            for (id, sim) in &semantic {
                if fused.contains_key(id) {
                    continue;
                }
                if let Some(memory) = load_memory(&conn, id)? {
                    fused.insert(id.clone(), (memory, (1.0 - LEXICAL_WEIGHT) * sim));
                }
            }
        }

        // Re-rank: confidence, freshness, scope.
        let mut ranked: Vec<Recalled> = fused
            .into_values()
            .map(|(memory, score)| {
                let weighted = score
                    * (0.5 + 0.5 * memory.confidence)
                    * freshness_factor(&memory, now)
                    * scope_factor(&memory, current_session_id);
                Recalled {
                    memory,
                    score: weighted,
                }
            })
            .collect();
        ranked.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| b.memory.created_at.cmp(&a.memory.created_at))
        });
        ranked.truncate(top_k as usize);

        // Conflict suppression: never inject both ends of a conflict /
        // supersede pair as fact. The higher-scored (tie: newer) memory wins.
        let mut suppressed: Vec<(String, String, String)> = Vec::new();
        let mut dropped: std::collections::HashSet<String> = std::collections::HashSet::new();
        let ordered: Vec<Recalled> = ranked;
        let mut kept: Vec<Recalled> = Vec::new();
        for hit in &ordered {
            if dropped.contains(&hit.memory.id) {
                continue;
            }
            for other in &ordered {
                if other.memory.id == hit.memory.id || dropped.contains(&other.memory.id) {
                    continue;
                }
                let linked = hit.memory.conflicts_with.contains(&other.memory.id)
                    || other.memory.conflicts_with.contains(&hit.memory.id);
                let supersedes = hit.memory.supersedes.as_deref() == Some(other.memory.id.as_str())
                    || other.memory.supersedes.as_deref() == Some(hit.memory.id.as_str());
                if !linked && !supersedes {
                    continue;
                }
                // `hit` ranks no lower than `other` (we iterate in rank order),
                // so `other` is suppressed.
                let reason = if supersedes {
                    "superseded by a newer memory".to_string()
                } else {
                    "conflicting memories recalled; only the preferred one was injected".to_string()
                };
                suppressed.push((other.memory.id.clone(), hit.memory.id.clone(), reason));
                dropped.insert(other.memory.id.clone());
            }
            kept.push(hit.clone());
        }

        // Usage telemetry for the memories actually returned.
        let kept_ids: Vec<String> = kept.iter().map(|hit| hit.memory.id.clone()).collect();
        for id in &kept_ids {
            conn.execute(
                "UPDATE memories SET access_count = access_count + 1, accessed_at = ?1
                 WHERE id = ?2",
                params![now_str, id],
            )?;
        }

        Ok(RecallBatch {
            hits: kept,
            suppressed,
        })
    }

    pub async fn get(&self, id: &str) -> Result<Option<Memory>, StoreError> {
        let conn = self.conn.lock().await;
        load_memory(&conn, id)
    }

    /// Record a deterministic validation result for a staged memory/skill.
    pub async fn record_validation(&self, id: &str, passed: bool) -> Result<(), StoreError> {
        let conn = self.conn.lock().await;
        let changed = conn.execute(
            "UPDATE memories SET validation_status = ?1, last_verified_at = ?2 WHERE id = ?3",
            params![
                if passed { "passed" } else { "failed" },
                Utc::now().to_rfc3339(),
                id
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(id.to_string()));
        }
        Ok(())
    }

    /// Staged skills that already carry a recorded passed validation — the set
    /// eligible for auto-promotion when `learning.skill_write_approval` is off.
    pub async fn staged_validated_skills(&self) -> Result<Vec<Memory>, StoreError> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id FROM memories \
             WHERE status = 'staged' AND category = 'skill' AND validation_status = 'passed'",
        )?;
        let ids = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        let mut memories = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(memory) = load_memory(&conn, &id)? {
                memories.push(memory);
            }
        }
        Ok(memories)
    }

    /// Promote a staged memory/skill to active. Skills additionally require a
    /// recorded passed validation; every promotion requires a non-empty
    /// `approved_by` (human or policy identity).
    pub async fn promote(&self, id: &str, approved_by: &str) -> Result<StatusChange, StoreError> {
        if approved_by.trim().is_empty() {
            return Err(StoreError::Blocked(
                "promotion requires an approver identity".into(),
            ));
        }
        let conn = self.conn.lock().await;
        let (status, category, validation): (String, String, String) = conn
            .query_row(
                "SELECT status, category, validation_status FROM memories WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => StoreError::NotFound(id.to_string()),
                other => StoreError::Sqlite(other),
            })?;
        if status != "staged" {
            return Err(StoreError::Blocked(format!(
                "only staged memories can be promoted (current status: {status})"
            )));
        }
        if category == "skill" && validation != "passed" {
            return Err(StoreError::Blocked(
                "skill promotion requires a recorded passed validation".into(),
            ));
        }
        conn.execute(
            "UPDATE memories SET status = 'active', approved_by = ?1 WHERE id = ?2",
            params![approved_by, id],
        )?;
        Ok(StatusChange {
            memory_id: id.to_string(),
            from: MemoryStatus::Staged,
            to: MemoryStatus::Active,
        })
    }

    /// Low-quality entries are disabled first (never hard-deleted).
    pub async fn disable(&self, id: &str) -> Result<StatusChange, StoreError> {
        self.transition(
            id,
            &[MemoryStatus::Active, MemoryStatus::Staged],
            MemoryStatus::Disabled,
        )
        .await
    }

    /// Archival is only allowed from `Disabled` — never directly from active.
    pub async fn archive(&self, id: &str) -> Result<StatusChange, StoreError> {
        self.transition(id, &[MemoryStatus::Disabled], MemoryStatus::Archived)
            .await
    }

    async fn transition(
        &self,
        id: &str,
        allowed_from: &[MemoryStatus],
        to: MemoryStatus,
    ) -> Result<StatusChange, StoreError> {
        let conn = self.conn.lock().await;
        let status: String = conn
            .query_row(
                "SELECT status FROM memories WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => StoreError::NotFound(id.to_string()),
                other => StoreError::Sqlite(other),
            })?;
        let from = str_to_status(&status);
        if !allowed_from.contains(&from) {
            return Err(StoreError::Blocked(format!(
                "cannot move memory from {status} to {}",
                status_to_str(to)
            )));
        }
        conn.execute(
            "UPDATE memories SET status = ?1 WHERE id = ?2",
            params![status_to_str(to), id],
        )?;
        Ok(StatusChange {
            memory_id: id.to_string(),
            from,
            to,
        })
    }

    /// Create the next version of a memory/skill: the new row starts `staged`
    /// with `parent_version_id` / `supersedes` pointing at the old one, and
    /// the old row is disabled. Returns `(new_id, old_id)`.
    pub async fn new_version(
        &self,
        id: &str,
        new_content: &str,
    ) -> Result<(String, String), StoreError> {
        if let Some(reason) = holmes_core::screen_sensitive(new_content) {
            return Err(StoreError::Rejected(reason));
        }
        let conn = self.conn.lock().await;
        let old = load_memory(&conn, id)?.ok_or_else(|| StoreError::NotFound(id.to_string()))?;

        let new_id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let tags_json = serde_json::to_string(&old.tags).unwrap_or_else(|_| "[]".into());
        let tech_json = serde_json::to_string(&old.tech_stack.clone().unwrap_or_default())
            .unwrap_or_else(|_| "[]".into());
        let embedding_json = embedding::embedding_to_json(&embedding::embed(new_content));
        conn.execute(
            "INSERT INTO memories (id, category, content, tags, attack_type, tech_stack,
             success, relevance_score, source_session_id, created_at,
             source, confidence, scope, status, supersedes, sensitive, embedding,
             version, parent_version_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                     ?11, ?12, ?13, 'staged', ?14, ?15, ?16, ?17, ?18)",
            params![
                new_id,
                category_to_str(&old.category),
                new_content,
                tags_json,
                old.attack_type,
                tech_json,
                old.success as i32,
                old.relevance_score,
                old.source_session_id,
                now,
                source_to_str(old.source),
                old.confidence,
                scope_to_str(old.scope),
                old.id,
                old.sensitive as i32,
                embedding_json,
                old.version + 1,
                old.id,
            ],
        )?;
        conn.execute(
            "UPDATE memories SET status = 'disabled' WHERE id = ?1",
            params![old.id],
        )?;
        Ok((new_id, old.id))
    }

    /// Roll a skill back to its previous version: the current row is disabled
    /// and the parent version is reactivated. Returns the parent id.
    pub async fn rollback(&self, id: &str) -> Result<String, StoreError> {
        let conn = self.conn.lock().await;
        let parent: Option<String> = conn
            .query_row(
                "SELECT parent_version_id FROM memories WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => StoreError::NotFound(id.to_string()),
                other => StoreError::Sqlite(other),
            })?;
        let parent = parent.ok_or_else(|| {
            StoreError::Blocked("memory has no previous version to roll back to".into())
        })?;
        conn.execute(
            "UPDATE memories SET status = 'disabled' WHERE id = ?1",
            params![id],
        )?;
        conn.execute(
            "UPDATE memories SET status = 'active' WHERE id = ?1",
            params![parent],
        )?;
        Ok(parent)
    }

    /// Skill usage statistics (success/failure rates, last used).
    pub async fn record_usage(&self, id: &str, success: bool) -> Result<(), StoreError> {
        let conn = self.conn.lock().await;
        let changed = conn.execute(
            "UPDATE memories SET use_count = use_count + 1,
             success_count = success_count + ?1,
             failure_count = failure_count + ?2,
             last_used_at = ?3
             WHERE id = ?4",
            params![
                success as i32,
                (!success) as i32,
                Utc::now().to_rfc3339(),
                id
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(id.to_string()));
        }
        Ok(())
    }

    pub async fn consolidate(
        &self,
        from_ids: &[String],
        into_content: &str,
        into_tags: &[String],
    ) -> Result<String, rusqlite::Error> {
        let new_id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let tags_json = serde_json::to_string(into_tags).unwrap_or_else(|_| "[]".into());
        let from_ids_json = serde_json::to_string(from_ids).unwrap_or_else(|_| "[]".into());
        let embedding_json = embedding::embedding_to_json(&embedding::embed(into_content));

        let new_id_clone = new_id.clone();
        self.write_contention
            .with_db_retry(|| {
                let new_id = new_id_clone.clone();
                let now = now.clone();
                let tags_json = tags_json.clone();
                let from_ids_json = from_ids_json.clone();
                let embedding_json = embedding_json.clone();
                let from_ids = from_ids.to_vec();
                let into_content = into_content.to_string();
                async move {
                    let conn = self.conn.lock().await;

                    // Build a placeholder list "(?, ?, ?)" matching the from_ids
                    // length so we can use plain IN(...) without the rarray
                    // extension (which is not enabled by default on rusqlite).
                    let placeholders = if from_ids.is_empty() {
                        "(NULL)".to_string()
                    } else {
                        let q: Vec<&str> = from_ids.iter().map(|_| "?").collect();
                        format!("({})", q.join(","))
                    };
                    let id_params: Vec<&dyn rusqlite::ToSql> =
                        from_ids.iter().map(|s| s as &dyn rusqlite::ToSql).collect();

                    // Get the highest relevance_score from the merged memories
                    let max_sql = format!(
                        "SELECT COALESCE(MAX(relevance_score), 0.0) FROM memories WHERE id IN {}",
                        placeholders
                    );
                    let max_score: f64 = conn
                        .query_row(&max_sql, id_params.as_slice(), |r| r.get(0))
                        .unwrap_or(0.0);

                    // Get the most common category
                    let cat_sql = format!(
                        "SELECT category FROM memories WHERE id IN {} \
                         GROUP BY category ORDER BY COUNT(*) DESC LIMIT 1",
                        placeholders
                    );
                    let category: String = conn
                        .query_row(&cat_sql, id_params.as_slice(), |r| r.get(0))
                        .unwrap_or_else(|_| "attack_experience".into());

                    // Get the most common attack_type
                    let atk_sql = format!(
                        "SELECT attack_type FROM memories WHERE id IN {} \
                         AND attack_type IS NOT NULL \
                         GROUP BY attack_type ORDER BY COUNT(*) DESC LIMIT 1",
                        placeholders
                    );
                    let attack_type: Option<String> = conn
                        .query_row(&atk_sql, id_params.as_slice(), |r| r.get(0))
                        .ok();

                    conn.execute(
                        "INSERT INTO memories (id, category, content, tags, attack_type, consolidated_from, relevance_score, created_at, embedding)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                        params![
                            new_id,
                            category,
                            into_content,
                            tags_json,
                            attack_type,
                            from_ids_json,
                            max_score,
                            now,
                            embedding_json,
                        ],
                    )?;

                    // Soft-delete old memories (set relevance to 0)
                    for id in &from_ids {
                        conn.execute(
                            "UPDATE memories SET relevance_score = 0.0 WHERE id = ?1",
                            params![id],
                        )?;
                    }

                    Ok::<_, rusqlite::Error>(())
                }
            })
            .await?;

        Ok(new_id)
    }
}

/// Lexical candidates across all statuses (legacy `search` path).
fn lexical_candidates(
    conn: &Connection,
    query: &str,
    top_k: u32,
    active_only: bool,
) -> Result<Vec<(Memory, f64)>, rusqlite::Error> {
    lexical_candidates_impl(conn, query, top_k, active_only, None)
}

/// Lexical candidates restricted to active, unexpired memories.
fn lexical_candidates_active(
    conn: &Connection,
    query: &str,
    top_k: u32,
    now: &str,
) -> Result<Vec<(Memory, f64)>, rusqlite::Error> {
    lexical_candidates_impl(conn, query, top_k, true, Some(now))
}

fn lexical_candidates_impl(
    conn: &Connection,
    query: &str,
    top_k: u32,
    active_only: bool,
    now: Option<&str>,
) -> Result<Vec<(Memory, f64)>, rusqlite::Error> {
    let sanitized = crate::fts::sanitize_fts5_query(query);
    // A query of only punctuation/stopwords sanitizes to empty; an empty FTS5
    // MATCH is a syntax error, so treat it as "no matches".
    if !crate::fts::contains_cjk(query) && sanitized.trim().is_empty() {
        return Ok(Vec::new());
    }

    let active_filter = if active_only {
        "AND m.status = 'active' AND (m.expires_at IS NULL OR m.expires_at > ?3)"
    } else {
        ""
    };

    let (sql, search_param) = if crate::fts::contains_cjk(query) {
        (
            format!(
                "SELECT {MEMORY_COLUMNS}, 0.0 FROM memories m WHERE m.content LIKE ?1 {active_filter}
                 ORDER BY m.relevance_score DESC LIMIT ?2"
            ),
            format!("%{}%", query),
        )
    } else {
        (
            format!(
                "SELECT {MEMORY_COLUMNS}, f.rank FROM memories m JOIN memories_fts f ON m.rowid = f.rowid
                 WHERE memories_fts MATCH ?1 {active_filter} ORDER BY f.rank LIMIT ?2"
            ),
            sanitized,
        )
    };

    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<(Memory, f64)> = if active_only {
        let now_param = now.unwrap_or("");
        stmt.query_map(params![search_param, top_k, now_param], |row| {
            Ok((row_to_memory(row)?, row.get::<_, f64>(22)?))
        })?
        .collect::<Result<Vec<_>, _>>()?
    } else {
        stmt.query_map(params![search_param, top_k], |row| {
            Ok((row_to_memory(row)?, row.get::<_, f64>(22)?))
        })?
        .collect::<Result<Vec<_>, _>>()?
    };
    Ok(rows)
}

fn load_memory(conn: &Connection, id: &str) -> Result<Option<Memory>, StoreError> {
    let sql = format!("SELECT {MEMORY_COLUMNS} FROM memories m WHERE m.id = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map(params![id], row_to_memory)?;
    match rows.next() {
        Some(Ok(memory)) => Ok(Some(memory)),
        Some(Err(error)) => Err(StoreError::Sqlite(error)),
        None => Ok(None),
    }
}

fn row_to_memory(row: &rusqlite::Row<'_>) -> Result<Memory, rusqlite::Error> {
    let cat_str: String = row.get(1)?;
    let created_at_str: String = row.get(10)?;
    let source_str: String = row.get(11)?;
    let scope_str: String = row.get(13)?;
    let status_str: String = row.get(14)?;
    let last_verified_str: Option<String> = row.get(15)?;
    let expires_str: Option<String> = row.get(16)?;
    Ok(Memory {
        id: row.get(0)?,
        category: str_to_category(&cat_str),
        content: row.get(2)?,
        tags: row
            .get::<_, String>(3)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default(),
        attack_type: row.get(4)?,
        tech_stack: row
            .get::<_, String>(5)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok()),
        success: row.get::<_, i32>(6).unwrap_or(0) != 0,
        relevance_score: row.get(7)?,
        source_session_id: row.get(8)?,
        consolidated_from: row
            .get::<_, Option<String>>(9)?
            .and_then(|c| serde_json::from_str(&c).ok()),
        created_at: parse_time(&created_at_str).unwrap_or_else(Utc::now),
        source: str_to_source(&source_str),
        confidence: row.get(12)?,
        scope: str_to_scope(&scope_str),
        status: str_to_status(&status_str),
        last_verified_at: last_verified_str.and_then(|s| parse_time(&s)),
        expires_at: expires_str.and_then(|s| parse_time(&s)),
        conflicts_with: row
            .get::<_, String>(17)
            .ok()
            .and_then(|c| serde_json::from_str(&c).ok())
            .unwrap_or_default(),
        supersedes: row.get(18)?,
        sensitive: row.get::<_, i32>(19).unwrap_or(0) != 0,
        version: row.get::<_, i64>(20).unwrap_or(1).max(1) as u32,
        parent_version_id: row.get(21)?,
    })
}

fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .ok()
}

fn normalize_text(content: &str) -> String {
    content
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Freshness re-rank factor based on the most recent of
/// verification/creation. Coarse buckets keep it deterministic.
fn freshness_factor(memory: &Memory, now: DateTime<Utc>) -> f64 {
    let reference = memory.last_verified_at.unwrap_or(memory.created_at);
    let age_days = (now - reference).num_days();
    if age_days < 30 {
        1.0
    } else if age_days < 90 {
        0.85
    } else {
        0.7
    }
}

/// Scope re-rank factor: session-scoped memories are boosted when they belong
/// to the current session and dampened when they belong to another one.
fn scope_factor(memory: &Memory, current_session_id: Option<&str>) -> f64 {
    if !matches!(memory.scope, MemoryScope::Session) {
        return 1.0;
    }
    match (current_session_id, memory.source_session_id.as_deref()) {
        (Some(current), Some(owner)) if current == owner => 1.15,
        (Some(_), Some(_)) => 0.5,
        _ => 1.0,
    }
}

fn category_to_str(cat: &MemoryCategory) -> &'static str {
    match cat {
        MemoryCategory::AttackExperience => "attack_experience",
        MemoryCategory::DiscoveredPattern => "discovered_pattern",
        MemoryCategory::ToolUsage => "tool_usage",
        MemoryCategory::TargetKnowledge => "target_knowledge",
        MemoryCategory::Fact => "fact",
        MemoryCategory::UserPreference => "user_preference",
        MemoryCategory::ProjectConvention => "project_convention",
        MemoryCategory::Skill => "skill",
    }
}

fn str_to_category(s: &str) -> MemoryCategory {
    match s {
        "discovered_pattern" => MemoryCategory::DiscoveredPattern,
        "tool_usage" => MemoryCategory::ToolUsage,
        "target_knowledge" => MemoryCategory::TargetKnowledge,
        "fact" => MemoryCategory::Fact,
        "user_preference" => MemoryCategory::UserPreference,
        "project_convention" => MemoryCategory::ProjectConvention,
        "skill" => MemoryCategory::Skill,
        _ => MemoryCategory::AttackExperience,
    }
}

fn source_to_str(source: MemorySource) -> &'static str {
    match source {
        MemorySource::User => "user",
        MemorySource::ToolEvidence => "tool_evidence",
        MemorySource::AgentInferred => "agent_inferred",
    }
}

fn str_to_source(s: &str) -> MemorySource {
    match s {
        "user" => MemorySource::User,
        "tool_evidence" => MemorySource::ToolEvidence,
        _ => MemorySource::AgentInferred,
    }
}

fn scope_to_str(scope: MemoryScope) -> &'static str {
    match scope {
        MemoryScope::Session => "session",
        MemoryScope::Project => "project",
        MemoryScope::User => "user",
        MemoryScope::Global => "global",
    }
}

fn str_to_scope(s: &str) -> MemoryScope {
    match s {
        "session" => MemoryScope::Session,
        "project" => MemoryScope::Project,
        "user" => MemoryScope::User,
        _ => MemoryScope::Global,
    }
}

fn status_to_str(status: MemoryStatus) -> &'static str {
    match status {
        MemoryStatus::Active => "active",
        MemoryStatus::Staged => "staged",
        MemoryStatus::Disabled => "disabled",
        MemoryStatus::Archived => "archived",
    }
}

fn str_to_status(s: &str) -> MemoryStatus {
    match s {
        "staged" => MemoryStatus::Staged,
        "disabled" => MemoryStatus::Disabled,
        "archived" => MemoryStatus::Archived,
        _ => MemoryStatus::Active,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::process;

    fn temp_db_path() -> String {
        let mut dir = env::temp_dir();
        dir.push(format!(
            "holmes_test_memory_{}_{}.db",
            process::id(),
            uuid::Uuid::new_v4()
        ));
        dir.to_string_lossy().to_string()
    }

    fn entry(content: &str) -> MemoryEntry {
        MemoryEntry {
            category: MemoryCategory::AttackExperience,
            content: content.into(),
            tags: vec!["sqli".into()],
            attack_type: Some("sqli".into()),
            tech_stack: vec!["PHP".into()],
            success: true,
            relevance_score: 0.9,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn test_store_and_search() {
        let path = temp_db_path();
        let store = MemoryStore::open(&path).await.unwrap();

        let outcome = store
            .store(MemoryEntry {
                content: "SQL injection via UNION SELECT on login.php parameter username".into(),
                tags: vec!["sqli".into(), "union".into(), "login".into()],
                tech_stack: vec!["PHP".into(), "MySQL".into()],
                relevance_score: 0.9,
                source_session_id: Some("test-session-1".into()),
                ..entry("x")
            })
            .await
            .unwrap();

        assert!(!outcome.id.is_empty());
        assert_eq!(outcome.status, MemoryStatus::Active);

        let results = store.search("SQL injection", 10).await.unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].tags, vec!["sqli", "union", "login"]);

        // Clean up
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn sensitive_content_is_rejected_unless_explicitly_allowed() {
        let path = temp_db_path();
        let store = MemoryStore::open(&path).await.unwrap();

        let rejected = store.store(entry("remember password=hunter2")).await;
        assert!(matches!(rejected, Err(StoreError::Rejected(_))));

        // Explicit opt-in (case state) is stored but flagged sensitive.
        let allowed = store
            .store(MemoryEntry {
                allow_sensitive: true,
                ..entry("found credential password=hunter2 in config.php")
            })
            .await
            .unwrap();
        assert!(allowed.sensitive);
        let stored = store.get(&allowed.id).await.unwrap().unwrap();
        assert!(stored.sensitive);

        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn skills_are_staged_and_require_validation_plus_approval() {
        let path = temp_db_path();
        let store = MemoryStore::open(&path).await.unwrap();

        let outcome = store
            .store(MemoryEntry {
                category: MemoryCategory::Skill,
                ..entry("skill: enumerate headers before fuzzing parameters")
            })
            .await
            .unwrap();
        assert_eq!(outcome.status, MemoryStatus::Staged);

        // Staged skills are invisible to turn-time recall.
        let batch = store
            .recall("enumerate headers", 5, true, None)
            .await
            .unwrap();
        assert!(batch.hits.is_empty());
        // ...but still visible to the lexical dedup path.
        assert!(!store
            .search("enumerate headers", 5)
            .await
            .unwrap()
            .is_empty());

        // Promotion without validation is blocked for skills.
        let blocked = store.promote(&outcome.id, "watson").await;
        assert!(matches!(blocked, Err(StoreError::Blocked(_))));

        // Promotion without an approver is blocked.
        store.record_validation(&outcome.id, true).await.unwrap();
        let blocked = store.promote(&outcome.id, "  ").await;
        assert!(matches!(blocked, Err(StoreError::Blocked(_))));

        // Validation + approval promotes.
        let change = store.promote(&outcome.id, "watson").await.unwrap();
        assert_eq!(change.from, MemoryStatus::Staged);
        assert_eq!(change.to, MemoryStatus::Active);

        let batch = store
            .recall("enumerate headers", 5, true, None)
            .await
            .unwrap();
        assert_eq!(batch.hits.len(), 1);

        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn archive_requires_disabled_first() {
        let path = temp_db_path();
        let store = MemoryStore::open(&path).await.unwrap();
        let outcome = store.store(entry("some active memory")).await.unwrap();

        let blocked = store.archive(&outcome.id).await;
        assert!(matches!(blocked, Err(StoreError::Blocked(_))));

        let change = store.disable(&outcome.id).await.unwrap();
        assert_eq!(change.to, MemoryStatus::Disabled);
        // Disabled memories are no longer recalled.
        let batch = store
            .recall("some active memory", 5, false, None)
            .await
            .unwrap();
        assert!(batch.hits.is_empty());

        let change = store.archive(&outcome.id).await.unwrap();
        assert_eq!(change.from, MemoryStatus::Disabled);
        assert_eq!(change.to, MemoryStatus::Archived);

        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn skill_versions_can_be_rolled_back() {
        let path = temp_db_path();
        let store = MemoryStore::open(&path).await.unwrap();

        let v1 = store
            .store(MemoryEntry {
                category: MemoryCategory::Skill,
                ..entry("skill v1: always enumerate first")
            })
            .await
            .unwrap();
        store.record_validation(&v1.id, true).await.unwrap();
        store.promote(&v1.id, "watson").await.unwrap();

        // New version starts staged; old version is disabled.
        let (v2_id, old_id) = store
            .new_version(&v1.id, "skill v2: enumerate, then fuzz")
            .await
            .unwrap();
        assert_eq!(old_id, v1.id);
        let v2 = store.get(&v2_id).await.unwrap().unwrap();
        assert_eq!(v2.version, 2);
        assert_eq!(v2.parent_version_id.as_deref(), Some(v1.id.as_str()));
        assert_eq!(v2.status, MemoryStatus::Staged);
        assert_eq!(
            store.get(&v1.id).await.unwrap().unwrap().status,
            MemoryStatus::Disabled
        );

        // Promote v2, then roll back to v1.
        store.record_validation(&v2_id, true).await.unwrap();
        store.promote(&v2_id, "watson").await.unwrap();
        let parent = store.rollback(&v2_id).await.unwrap();
        assert_eq!(parent, v1.id);
        assert_eq!(
            store.get(&v1.id).await.unwrap().unwrap().status,
            MemoryStatus::Active
        );
        assert_eq!(
            store.get(&v2_id).await.unwrap().unwrap().status,
            MemoryStatus::Disabled
        );

        // A version without a parent cannot roll back.
        let blocked = store.rollback(&v1.id).await;
        assert!(matches!(blocked, Err(StoreError::Blocked(_))));

        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn conflicting_memories_are_linked_and_suppressed_at_recall() {
        let path = temp_db_path();
        let store = MemoryStore::open(&path).await.unwrap();

        let a = store
            .store(MemoryEntry {
                success: true,
                ..entry("login.php is vulnerable to SQL injection")
            })
            .await
            .unwrap();
        assert!(a.conflicts.is_empty());

        // Same normalized claim, opposite verdict → conflict link.
        let b = store
            .store(MemoryEntry {
                success: false,
                confidence: 0.9,
                ..entry("login.php is vulnerable to SQL injection")
            })
            .await
            .unwrap();
        assert_eq!(b.conflicts, vec![a.id.clone()]);

        let batch = store
            .recall("login.php SQL injection", 5, false, None)
            .await
            .unwrap();
        assert_eq!(batch.hits.len(), 1);
        assert_eq!(batch.suppressed.len(), 1);
        let (suppressed_id, chosen_id, _) = &batch.suppressed[0];
        // Higher confidence wins.
        assert_eq!(chosen_id, &b.id);
        assert_eq!(suppressed_id, &a.id);

        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn hybrid_recall_finds_paraphrase_and_degrades_to_lexical() {
        let path = temp_db_path();
        let store = MemoryStore::open(&path).await.unwrap();

        store
            .store(entry(
                "UNION SELECT based SQL injection bypasses the login form",
            ))
            .await
            .unwrap();
        store
            .store(entry("phillips hue firmware update notes"))
            .await
            .unwrap();

        // Synonym-expanded hybrid recall: "sqli" never appears in the memory.
        let batch = store.recall("sqli bypass", 5, true, None).await.unwrap();
        assert!(!batch.hits.is_empty());
        assert!(batch.hits[0].memory.content.contains("UNION SELECT"));

        // Pure lexical path (hybrid off): no synonym expansion, but the
        // lexical component still matches directly.
        let batch = store.recall("UNION SELECT", 5, false, None).await.unwrap();
        assert!(!batch.hits.is_empty());

        // Embedding unavailable (column cleared): hybrid degrades to lexical
        // instead of failing or returning nothing.
        {
            let conn = store.conn.lock().await;
            conn.execute("UPDATE memories SET embedding = NULL", [])
                .unwrap();
        }
        let batch = store.recall("login form", 5, true, None).await.unwrap();
        assert_eq!(batch.hits.len(), 1);
        assert!(batch.hits[0].memory.content.contains("UNION SELECT"));

        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn expired_memories_are_not_recalled() {
        let path = temp_db_path();
        let store = MemoryStore::open(&path).await.unwrap();
        store
            .store(MemoryEntry {
                expires_at: Some(Utc::now() - chrono::Duration::days(1)),
                ..entry("stale scan result for old engagement")
            })
            .await
            .unwrap();

        let batch = store
            .recall("stale scan result", 5, true, None)
            .await
            .unwrap();
        assert!(batch.hits.is_empty());

        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn recall_records_usage_telemetry() {
        let path = temp_db_path();
        let store = MemoryStore::open(&path).await.unwrap();
        let outcome = store.store(entry("telemetry probe target")).await.unwrap();

        let batch = store
            .recall("telemetry probe", 5, false, None)
            .await
            .unwrap();
        assert_eq!(batch.hits.len(), 1);

        let count: i64 = {
            let conn = store.conn.lock().await;
            conn.query_row(
                "SELECT access_count FROM memories WHERE id = ?1",
                params![outcome.id],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(count, 1);

        // Skill usage stats.
        store.record_usage(&outcome.id, true).await.unwrap();
        store.record_usage(&outcome.id, false).await.unwrap();
        let (uses, succ, fail): (i64, i64, i64) = {
            let conn = store.conn.lock().await;
            conn.query_row(
                "SELECT use_count, success_count, failure_count FROM memories WHERE id = ?1",
                params![outcome.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap()
        };
        assert_eq!((uses, succ, fail), (2, 1, 1));

        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn test_consolidate() {
        let path = temp_db_path();
        let store = MemoryStore::open(&path).await.unwrap();

        let id1 = store
            .store(MemoryEntry {
                content: "SQL injection in login.php via POST username".into(),
                tags: vec!["sqli".into(), "post".into()],
                relevance_score: 0.8,
                ..entry("x")
            })
            .await
            .unwrap()
            .id;

        let id2 = store
            .store(MemoryEntry {
                content: "SQL injection in search.php via GET q parameter".into(),
                tags: vec!["sqli".into(), "get".into()],
                relevance_score: 0.7,
                ..entry("x")
            })
            .await
            .unwrap()
            .id;

        let consolidated_id = store
            .consolidate(
                &[id1.clone(), id2.clone()],
                "Multiple SQL injection points found in PHP app - both POST and GET vectors",
                &["sqli".into(), "php".into(), "consolidated".into()],
            )
            .await
            .unwrap();

        assert!(!consolidated_id.is_empty());

        // Old memories should have relevance 0
        let results = store.search("sqli php", 5).await.unwrap();
        // The consolidated entry should appear with the new tags
        let consolidated = results.iter().find(|m| m.id == consolidated_id);
        assert!(consolidated.is_some());
        assert!(consolidated
            .unwrap()
            .tags
            .contains(&"consolidated".to_string()));

        std::fs::remove_file(&path).ok();
    }
}
