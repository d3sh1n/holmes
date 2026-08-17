pub mod memory_layer;

use holmes_core::event::Event;
use holmes_session::memory_store::MemoryStore;
use std::sync::Arc;

use memory_layer::MemoryLayer;

/// The Mind Palace is now a thin wrapper over the event-sourced memory layer. The old
/// typed "context layer" / dashboard projection was removed: it was fed by situational
/// events the runtime never emits, so it stayed empty and never reached the model. The
/// prompt's `[Current situation]` is projected live from `AttackState` in the runtime's
/// perception engine instead.
pub struct MindPalace {
    pub memory: MemoryLayer,
}

impl MindPalace {
    pub fn new(
        session_db: Arc<dyn holmes_session::SessionStore>,
        long_term: Arc<MemoryStore>,
    ) -> Self {
        Self {
            memory: MemoryLayer::new(session_db, long_term),
        }
    }

    pub async fn from_events(
        session_id: &str,
        session_db: Arc<dyn holmes_session::SessionStore>,
        long_term: Arc<MemoryStore>,
    ) -> Result<Self, String> {
        let mut palace = Self::new(session_db, long_term);
        palace.memory.replay(session_id).await?;
        Ok(palace)
    }

    pub fn ingest(&mut self, event: Event) {
        self.memory.ingest(event);
    }
}
