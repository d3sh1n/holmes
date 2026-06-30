use holmes_core::event::Event;
use holmes_core::types::MemoryCategory;
use holmes_core::RecallTrigger;
use holmes_session::memory_store::MemoryEntry;

use crate::context::{RuntimeContext, RuntimeMemory};
use crate::deliberation::RuntimeError;
use crate::yield_stream::RuntimeYield;

const DEFAULT_RECALL_TOP_K: u32 = 3;

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

        let memories = context
            .memory_store
            .search(query, DEFAULT_RECALL_TOP_K)
            .await
            .map_err(|error| {
                RuntimeError::recoverable(format!("failed to recall Holmes memory: {error}"))
            })?;

        let existing = context
            .state
            .recalled_memories
            .iter()
            .map(|memory| memory.id.as_str())
            .collect::<std::collections::HashSet<_>>();

        let recalled = memories
            .into_iter()
            .filter(|memory| !existing.contains(memory.id.as_str()))
            .map(|memory| RuntimeMemory {
                id: memory.id,
                content: memory.content,
                relevance_score: memory.relevance_score,
            })
            .collect::<Vec<_>>();

        if recalled.is_empty() {
            return Ok(MemoryProjection::default());
        }

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
                attack_type: None,
                tech_stack: Vec::new(),
                success: true,
                relevance_score: 0.75,
                source_session_id: Some(context.session_id.clone()),
            };
            let id = context.memory_store.store(entry).await.map_err(|error| {
                RuntimeError::recoverable(format!("failed to store Holmes memory: {error}"))
            })?;

            append_and_ingest(
                context,
                Event::MemoryStored {
                    category: MemoryCategory::DiscoveredPattern,
                    content: observation.to_string(),
                    tags: vec!["runtime".into(), "observation".into()],
                    relevance_score: 0.75,
                    source_session_id: Some(context.session_id.clone()),
                },
            )
            .await?;
            stored_ids.push(id);
        }

        Ok(MemoryProjection {
            events: Vec::new(),
            recalled: Vec::new(),
            stored_ids,
        })
    }
}

async fn append_and_ingest(context: &mut RuntimeContext, mut event: Event) -> Result<(), RuntimeError> {
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
        let mut context = make_context().await;
        context
            .memory_store
            .store(MemoryEntry {
                category: MemoryCategory::AttackExperience,
                content: "Login enumeration was visible through different error text.".into(),
                tags: vec!["login".into()],
                attack_type: Some("auth".into()),
                tech_stack: Vec::new(),
                success: true,
                relevance_score: 0.91,
                source_session_id: None,
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
    async fn remembers_new_observations_and_records_events() {
        let mut context = make_context().await;

        let projection = MemoryEngine::new()
            .remember_observations(
                &mut context,
                &["Identified technology: Django.".to_string()],
            )
            .await
            .expect("remember");

        assert_eq!(projection.stored_ids.len(), 1);
        let recalled = context
            .memory_store
            .search("Django", 3)
            .await
            .expect("search");
        assert_eq!(recalled.len(), 1);

        let events = context
            .session_db
            .get_events(&context.session_id)
            .await
            .expect("events");
        assert!(events
            .iter()
            .any(|event| matches!(event.event, Event::MemoryStored { .. })));
    }

    async fn make_context() -> RuntimeContext {
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
            HolmesConfig::default(),
        )
    }
}
