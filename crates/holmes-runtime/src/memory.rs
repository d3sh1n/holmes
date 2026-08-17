use holmes_core::event::Event;
use holmes_core::types::{MemoryCategory, MemoryScope, MemorySource, MemoryStatus};
use holmes_core::RecallTrigger;
use holmes_session::memory_store::{MemoryEntry, StoreError};

use crate::context::{RuntimeContext, RuntimeMemory};
use crate::deliberation::RuntimeError;
use crate::yield_stream::RuntimeYield;

const DEFAULT_RECALL_TOP_K: u32 = 3;

/// Cross-session memory (AGT-011). Recall is **hybrid**: FTS5/LIKE lexical
/// candidates fused with local-embedding cosine similarity, re-ranked by
/// confidence, freshness and scope (see `holmes_session::memory_store`). It
/// runs under a bounded time budget (`config.memory.recall_timeout_ms`); on
/// timeout the turn continues without memories instead of blocking.
/// Conflicting memories are never injected together — the preferred side is
/// kept and the suppression is audited with `MemoryConflictDetected`.
#[derive(Debug, Clone, Default)]
pub struct MemoryEngine;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MemoryProjection {
    pub recalled: Vec<RuntimeMemory>,
    pub stored_ids: Vec<String>,
    pub events: Vec<RuntimeYield>,
}

impl MemoryEngine {
    pub fn new() -> Self {
        Self
    }

    pub async fn recall_for_turn(
        &self,
        context: &mut RuntimeContext,
        query: &str,
    ) -> Result<MemoryProjection, RuntimeError> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(MemoryProjection::default());
        }

        let config = &context.config.memory;
        let budget = std::time::Duration::from_millis(config.recall_timeout_ms);
        // A zero budget disables turn-time recall entirely.
        if budget.is_zero() {
            return Ok(MemoryProjection::default());
        }
        let hybrid = config.hybrid_recall;
        let max_chars = config.recall_max_chars;

        let store = context.memory_store.clone();
        let session_id = context.session_id.clone();
        let query_owned = query.to_string();
        holmes_core::metrics::metrics().count("memory.recall.total");
        let batch = match tokio::time::timeout(budget, async move {
            store
                .recall(
                    &query_owned,
                    DEFAULT_RECALL_TOP_K,
                    hybrid,
                    Some(&session_id),
                )
                .await
        })
        .await
        {
            Ok(result) => result.map_err(|error| {
                RuntimeError::recoverable(format!("failed to recall Holmes memory: {error}"))
            })?,
            Err(_) => {
                // Recall missed its budget: continue the turn without memories
                // rather than blocking the main chain.
                holmes_core::metrics::metrics().count("memory.recall.timeout");
                tracing::warn!(
                    event = "MemoryRecallTimeout",
                    budget_ms = config.recall_timeout_ms,
                    "memory recall exceeded its budget; continuing turn without memories"
                );
                return Ok(MemoryProjection::default());
            }
        };

        // Conflict suppressions are audited: both ends were recalled, only the
        // preferred one is injected as fact.
        for (suppressed_id, chosen_id, reason) in &batch.suppressed {
            holmes_core::metrics::metrics().count("memory.conflict_detected");
            append_and_ingest(
                context,
                Event::MemoryConflictDetected {
                    suppressed_id: suppressed_id.clone(),
                    chosen_id: chosen_id.clone(),
                    reason: reason.clone(),
                },
            )
            .await?;
        }

        let existing = context
            .state
            .recalled_memories
            .iter()
            .map(|memory| memory.id.as_str())
            .collect::<std::collections::HashSet<_>>();

        // Dedup against what earlier turns already recalled, then clip to the
        // character budget so memory injection cannot blow up the prompt.
        let mut recalled: Vec<RuntimeMemory> = Vec::new();
        let mut used_chars = 0usize;
        for hit in batch.hits {
            if existing.contains(hit.memory.id.as_str()) {
                continue;
            }
            if used_chars + hit.memory.content.len() > max_chars && !recalled.is_empty() {
                break;
            }
            used_chars += hit.memory.content.len();
            recalled.push(RuntimeMemory {
                id: hit.memory.id,
                content: hit.memory.content,
                relevance_score: hit.score,
            });
        }

        if recalled.is_empty() {
            return Ok(MemoryProjection::default());
        }
        holmes_core::metrics::metrics().count("memory.recall.hit");

        let ids = recalled
            .iter()
            .map(|memory| memory.id.clone())
            .collect::<Vec<_>>();
        let relevance = recalled
            .iter()
            .map(|memory| memory.relevance_score)
            .collect::<Vec<_>>();
        append_and_ingest(
            context,
            Event::MemoryRecalled {
                memory_ids: ids.clone(),
                trigger: RecallTrigger::Query,
                relevance,
            },
        )
        .await?;

        context.state.recalled_memories.extend(recalled.clone());

        Ok(MemoryProjection {
            events: vec![RuntimeYield::PlanUpdate {
                content: format!("Recalled {} related memory item(s).", ids.len()),
            }],
            recalled,
            stored_ids: Vec::new(),
        })
    }

    /// Persist a pre-compaction flush note: the must-survive case state the compressor
    /// role extracted just before the transcript middle was summarized away. Category is
    /// `TargetKnowledge` (this is case/target state, not a reusable technique) with a high
    /// static relevance so a later `recall_for_turn` surfaces it — the double safety net
    /// next to the `## Preserved critical state` section in the compaction summary.
    ///
    /// Flush notes are session-scoped case state: they stay active (they must be
    /// recallable by this session) and may legitimately quote observed credentials,
    /// so they opt into sensitive storage explicitly.
    pub async fn remember_flush_note(
        &self,
        context: &mut RuntimeContext,
        note: &str,
    ) -> Result<Option<String>, RuntimeError> {
        let note = note.trim();
        if note.is_empty() {
            return Ok(None);
        }

        let tags = vec!["compaction".to_string(), "pre-compaction-flush".to_string()];
        let entry = MemoryEntry {
            category: MemoryCategory::TargetKnowledge,
            content: note.to_string(),
            tags: tags.clone(),
            success: true,
            relevance_score: 0.9,
            source_session_id: Some(context.session_id.clone()),
            confidence: 0.8,
            scope: MemoryScope::Session,
            allow_sensitive: true,
            ..Default::default()
        };
        let outcome = context.memory_store.store(entry).await.map_err(|error| {
            RuntimeError::recoverable(format!("failed to store flush note: {error}"))
        })?;

        append_and_ingest(
            context,
            Event::MemoryStored {
                category: MemoryCategory::TargetKnowledge,
                content: note.to_string(),
                tags,
                relevance_score: 0.9,
                source_session_id: Some(context.session_id.clone()),
            },
        )
        .await?;

        Ok(Some(outcome.id))
    }

    /// Persist agent-observed patterns. These are agent inferences, so they
    /// enter long-term memory as **staged** (never recalled until validated
    /// and approved) and are audited with `MemoryWriteStaged`. Content that
    /// screens positive for secrets or prompt injection is refused and
    /// audited with `MemoryRejected`; the rejected content itself is never
    /// persisted — only a short summary.
    pub async fn remember_observations(
        &self,
        context: &mut RuntimeContext,
        observations: &[String],
    ) -> Result<MemoryProjection, RuntimeError> {
        let mut stored_ids = Vec::new();

        for observation in observations {
            let observation = observation.trim();
            if observation.is_empty() {
                continue;
            }

            let entry = MemoryEntry {
                category: MemoryCategory::DiscoveredPattern,
                content: observation.to_string(),
                tags: vec!["runtime".into(), "observation".into()],
                success: true,
                relevance_score: 0.75,
                source_session_id: Some(context.session_id.clone()),
                source: MemorySource::AgentInferred,
                status: Some(MemoryStatus::Staged),
                ..Default::default()
            };
            let outcome = match context.memory_store.store(entry).await {
                Ok(outcome) => outcome,
                Err(StoreError::Rejected(reason)) => {
                    holmes_core::metrics::metrics().count("memory.rejected");
                    append_and_ingest(
                        context,
                        Event::MemoryRejected {
                            content_summary: holmes_core::truncate_str(observation, 120)
                                .to_string(),
                            reason,
                        },
                    )
                    .await?;
                    continue;
                }
                Err(error) => {
                    return Err(RuntimeError::recoverable(format!(
                        "failed to store Holmes memory: {error}"
                    )));
                }
            };

            append_and_ingest(
                context,
                Event::MemoryWriteStaged {
                    content: observation.to_string(),
                    reason: "agent-inferred observation enters staged review".into(),
                },
            )
            .await?;
            stored_ids.push(outcome.id);
        }

        Ok(MemoryProjection {
            events: Vec::new(),
            recalled: Vec::new(),
            stored_ids,
        })
    }

    /// Record a deterministic validation result for a staged memory/skill.
    pub async fn validate_memory(
        &self,
        context: &mut RuntimeContext,
        memory_id: &str,
        passed: bool,
    ) -> Result<(), RuntimeError> {
        context
            .memory_store
            .record_validation(memory_id, passed)
            .await
            .map_err(store_transition_error)?;
        append_and_ingest(
            context,
            Event::MemoryStatusChanged {
                memory_id: memory_id.to_string(),
                from_status: "staged".into(),
                to_status: "staged".into(),
                reason: if passed {
                    "validation passed".into()
                } else {
                    "validation failed".into()
                },
            },
        )
        .await
    }

    /// Promote a staged memory/skill to active. The store enforces the gate:
    /// skills need a recorded passed validation, everything needs an approver.
    pub async fn promote_memory(
        &self,
        context: &mut RuntimeContext,
        memory_id: &str,
        approved_by: &str,
    ) -> Result<(), RuntimeError> {
        let change = context
            .memory_store
            .promote(memory_id, approved_by)
            .await
            .map_err(store_transition_error)?;
        self.record_status_change(context, change, format!("approved by {approved_by}"))
            .await
    }

    /// Disable a low-quality memory/skill (first step before archival).
    pub async fn disable_memory(
        &self,
        context: &mut RuntimeContext,
        memory_id: &str,
        reason: &str,
    ) -> Result<(), RuntimeError> {
        let change = context
            .memory_store
            .disable(memory_id)
            .await
            .map_err(store_transition_error)?;
        self.record_status_change(context, change, reason.to_string())
            .await
    }

    /// Archive a disabled memory/skill. Hard deletion is never performed.
    pub async fn archive_memory(
        &self,
        context: &mut RuntimeContext,
        memory_id: &str,
        reason: &str,
    ) -> Result<(), RuntimeError> {
        let change = context
            .memory_store
            .archive(memory_id)
            .await
            .map_err(store_transition_error)?;
        self.record_status_change(context, change, reason.to_string())
            .await
    }

    /// Roll a skill back to its previous version.
    pub async fn rollback_memory(
        &self,
        context: &mut RuntimeContext,
        memory_id: &str,
        reason: &str,
    ) -> Result<String, RuntimeError> {
        let parent_id = context
            .memory_store
            .rollback(memory_id)
            .await
            .map_err(store_transition_error)?;
        append_and_ingest(
            context,
            Event::MemoryStatusChanged {
                memory_id: memory_id.to_string(),
                from_status: "active".into(),
                to_status: "disabled".into(),
                reason: format!("rolled back to previous version {parent_id}: {reason}"),
            },
        )
        .await?;
        Ok(parent_id)
    }

    /// Skill usage statistics (success/failure counters, last used).
    pub async fn record_skill_usage(
        &self,
        context: &RuntimeContext,
        memory_id: &str,
        success: bool,
    ) -> Result<(), RuntimeError> {
        context
            .memory_store
            .record_usage(memory_id, success)
            .await
            .map_err(store_transition_error)
    }

    async fn record_status_change(
        &self,
        context: &mut RuntimeContext,
        change: holmes_session::memory_store::StatusChange,
        reason: String,
    ) -> Result<(), RuntimeError> {
        append_and_ingest(
            context,
            Event::MemoryStatusChanged {
                memory_id: change.memory_id,
                from_status: status_str(change.from).to_string(),
                to_status: status_str(change.to).to_string(),
                reason,
            },
        )
        .await
    }
}

fn status_str(status: MemoryStatus) -> &'static str {
    match status {
        MemoryStatus::Active => "active",
        MemoryStatus::Staged => "staged",
        MemoryStatus::Disabled => "disabled",
        MemoryStatus::Archived => "archived",
    }
}

pub(crate) fn store_transition_error(error: StoreError) -> RuntimeError {
    RuntimeError::recoverable(format!("memory lifecycle transition failed: {error}"))
}

async fn append_and_ingest(
    context: &mut RuntimeContext,
    mut event: Event,
) -> Result<(), RuntimeError> {
    let middlewares = context.middlewares.clone();
    for mw in &middlewares {
        mw.before_event_persist(context, &mut event).await?;
    }
    context
        .session_db
        .append_event(&context.session_id, &event)
        .await
        .map_err(|error| {
            RuntimeError::recoverable(format!(
                "failed to persist memory event for session {}: {}",
                context.session_id, error
            ))
        })?;
    context.mind_palace.ingest(event);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use holmes_core::config::HolmesConfig;
    use holmes_core::session::RuntimeSession;
    use holmes_core::{LlmResponse, SessionMode};
    use holmes_guards::GuardChain;
    use holmes_mind_palace::MindPalace;
    use holmes_session::{memory_store::MemoryStore, CreateSessionParams, SessionDB, SessionStore};
    use holmes_tools::ToolRegistry;

    use crate::context::RuntimeState;
    use crate::deliberation::StaticLlmBackend;

    use super::*;

    #[tokio::test]
    async fn recalls_matching_memory_and_records_event() {
        let mut context = make_context(HolmesConfig::default()).await;
        context
            .memory_store
            .store(MemoryEntry {
                category: MemoryCategory::AttackExperience,
                content: "Login enumeration was visible through different error text.".into(),
                tags: vec!["login".into()],
                attack_type: Some("auth".into()),
                relevance_score: 0.91,
                ..Default::default()
            })
            .await
            .expect("store memory");

        let projection = MemoryEngine::new()
            .recall_for_turn(&mut context, "login enumeration")
            .await
            .expect("recall");

        assert_eq!(projection.recalled.len(), 1);
        assert_eq!(context.state.recalled_memories.len(), 1);
        assert!(matches!(
            projection.events.first(),
            Some(RuntimeYield::PlanUpdate { .. })
        ));

        let events = context
            .session_db
            .get_events(&context.session_id)
            .await
            .expect("events");
        assert!(events
            .iter()
            .any(|event| matches!(event.event, Event::MemoryRecalled { .. })));
    }

    #[tokio::test]
    async fn staged_memories_are_not_recalled() {
        let mut context = make_context(HolmesConfig::default()).await;
        context
            .memory_store
            .store(MemoryEntry {
                category: MemoryCategory::Skill,
                content: "staged skill must not leak into turns".into(),
                ..Default::default()
            })
            .await
            .expect("store memory");

        let projection = MemoryEngine::new()
            .recall_for_turn(&mut context, "staged skill")
            .await
            .expect("recall");
        assert!(projection.recalled.is_empty());
    }

    #[tokio::test]
    async fn zero_recall_budget_skips_recall_without_blocking_turn() {
        let mut config = HolmesConfig::default();
        config.memory.recall_timeout_ms = 0;
        let mut context = make_context(config).await;
        context
            .memory_store
            .store(MemoryEntry {
                content: "Login enumeration was visible through different error text.".into(),
                relevance_score: 0.91,
                ..Default::default()
            })
            .await
            .expect("store memory");

        let projection = MemoryEngine::new()
            .recall_for_turn(&mut context, "login enumeration")
            .await
            .expect("recall must not fail when the budget is exhausted");
        assert!(projection.recalled.is_empty());
        assert!(projection.events.is_empty());
    }

    #[tokio::test]
    async fn conflicting_memories_are_not_injected_together() {
        let mut context = make_context(HolmesConfig::default()).await;
        context
            .memory_store
            .store(MemoryEntry {
                content: "admin panel accepts default credentials".into(),
                success: true,
                confidence: 0.4,
                ..Default::default()
            })
            .await
            .expect("store memory a");
        context
            .memory_store
            .store(MemoryEntry {
                content: "admin panel accepts default credentials".into(),
                success: false,
                confidence: 0.9,
                ..Default::default()
            })
            .await
            .expect("store memory b");

        let projection = MemoryEngine::new()
            .recall_for_turn(&mut context, "admin panel default credentials")
            .await
            .expect("recall");

        assert_eq!(projection.recalled.len(), 1);
        let events = context
            .session_db
            .get_events(&context.session_id)
            .await
            .expect("events");
        assert!(events
            .iter()
            .any(|event| matches!(event.event, Event::MemoryConflictDetected { .. })));
    }

    #[tokio::test]
    async fn sensitive_observation_is_rejected_and_audited() {
        let mut context = make_context(HolmesConfig::default()).await;

        let projection = MemoryEngine::new()
            .remember_observations(
                &mut context,
                &["the admin password=Sup3rSecret! was reused".to_string()],
            )
            .await
            .expect("remember");

        assert!(projection.stored_ids.is_empty());
        let events = context
            .session_db
            .get_events(&context.session_id)
            .await
            .expect("events");
        assert!(events.iter().any(|event| matches!(
            &event.event,
            Event::MemoryRejected { reason, .. } if reason.contains("secret")
        )));
    }

    #[tokio::test]
    async fn agent_observations_are_staged_not_stored_active() {
        let mut context = make_context(HolmesConfig::default()).await;

        let projection = MemoryEngine::new()
            .remember_observations(
                &mut context,
                &["Identified technology: Django.".to_string()],
            )
            .await
            .expect("remember");

        assert_eq!(projection.stored_ids.len(), 1);
        // Staged: visible to the lexical dedup path, invisible to recall.
        let found = context
            .memory_store
            .search("Django", 3)
            .await
            .expect("search");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].status, MemoryStatus::Staged);

        let recall = context
            .memory_store
            .recall("Django", 3, true, None)
            .await
            .expect("recall");
        assert!(recall.hits.is_empty());

        let events = context
            .session_db
            .get_events(&context.session_id)
            .await
            .expect("events");
        assert!(events
            .iter()
            .any(|event| matches!(event.event, Event::MemoryWriteStaged { .. })));
    }

    #[tokio::test]
    async fn skill_lifecycle_promote_and_rollback_emit_status_events() {
        let mut context = make_context(HolmesConfig::default()).await;
        let engine = MemoryEngine::new();

        let outcome = context
            .memory_store
            .store(MemoryEntry {
                category: MemoryCategory::Skill,
                content: "skill: enumerate before fuzzing".into(),
                ..Default::default()
            })
            .await
            .expect("store skill");

        // Gate: unvalidated skills cannot be promoted.
        assert!(engine
            .promote_memory(&mut context, &outcome.id, "watson")
            .await
            .is_err());

        engine
            .validate_memory(&mut context, &outcome.id, true)
            .await
            .expect("validate");
        engine
            .promote_memory(&mut context, &outcome.id, "watson")
            .await
            .expect("promote");

        // Version + rollback.
        let (v2, _v1) = context
            .memory_store
            .new_version(&outcome.id, "skill: enumerate, fuzz, then verify")
            .await
            .expect("new version");
        engine
            .validate_memory(&mut context, &v2, true)
            .await
            .expect("validate v2");
        engine
            .promote_memory(&mut context, &v2, "watson")
            .await
            .expect("promote v2");
        let parent = engine
            .rollback_memory(&mut context, &v2, "regression in v2")
            .await
            .expect("rollback");
        assert_eq!(parent, outcome.id);

        let events = context
            .session_db
            .get_events(&context.session_id)
            .await
            .expect("events");
        let status_changes = events
            .iter()
            .filter(|event| matches!(event.event, Event::MemoryStatusChanged { .. }))
            .count();
        assert!(status_changes >= 4);
    }

    async fn make_context(config: HolmesConfig) -> RuntimeContext {
        let session_id = "session-1".to_string();
        let session_db = Arc::new(SessionDB::open(":memory:").await.expect("session db"));
        session_db
            .create_session(CreateSessionParams {
                id: Some(session_id.clone()),
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
        let memory_store = Arc::new(MemoryStore::open(":memory:").await.expect("memory store"));
        let mind_palace = MindPalace::new(session_db.clone(), memory_store.clone());
        let llm = Arc::new(StaticLlmBackend::new(LlmResponse {
            content: Some("ok".into()),
            tool_calls: Vec::new(),
            finish_reason: None,
            usage: None,
            ..Default::default()
        }));

        RuntimeContext::new(
            RuntimeSession::new(session_id, SessionMode::Pentest),
            session_db,
            memory_store,
            mind_palace,
            llm,
            Arc::new(ToolRegistry::new()),
            GuardChain::new(),
            RuntimeState::new(SessionMode::Pentest),
            config,
        )
    }
}
