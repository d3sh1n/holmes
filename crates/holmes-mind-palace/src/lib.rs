pub mod context_layer;
pub mod context_stack;
pub mod dashboard_layer;
pub mod memory_layer;
pub mod retrieval;

use holmes_core::event::Event;
use holmes_core::types::*;
use holmes_session::memory_store::MemoryStore;
use std::sync::Arc;

use context_layer::ContextLayer;
use dashboard_layer::DashboardLayer;
use memory_layer::MemoryLayer;

pub struct MindPalace {
    pub memory: MemoryLayer,
    pub context: ContextLayer,
}

impl MindPalace {
    pub fn new(session_db: Arc<holmes_session::db::SessionDB>, long_term: Arc<MemoryStore>) -> Self {
        Self {
            memory: MemoryLayer::new(session_db, long_term),
            context: ContextLayer::new(),
        }
    }

    pub async fn from_events(
        session_id: &str,
        session_db: Arc<holmes_session::db::SessionDB>,
        long_term: Arc<MemoryStore>,
    ) -> Result<Self, String> {
        let mut palace = Self::new(session_db, long_term);
        palace.memory.replay(session_id).await?;
        let events = palace.memory.session_events.clone();
        for event in &events {
            palace.context.ingest(event);
        }
        Ok(palace)
    }

    pub fn ingest(&mut self, event: Event) {
        self.context.ingest(&event);
        self.memory.ingest(event);
    }

    pub fn dashboard(&self, mode: &SessionMode) -> DashboardSnapshot {
        DashboardLayer::generate(&self.context, mode)
    }

    pub fn situation_summary(&self) -> String {
        self.context.situation_summary()
    }

    pub fn snapshot(&self) -> ContextSnapshot {
        self.context.snapshot()
    }

    pub fn compress(&mut self) {
        self.context.compress();
    }
}
