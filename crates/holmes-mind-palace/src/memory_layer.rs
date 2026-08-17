use holmes_core::event::Event;
use holmes_session::memory_store::{MemoryEntry, MemoryStore};
use holmes_session::SessionStore;
use std::sync::Arc;

/// The memory layer: an in-memory mirror of the session event stream (rehydrated via
/// `replay`) plus a handle to the cross-session long-term `MemoryStore`. Long-term
/// recall itself runs through `holmes_runtime::MemoryEngine` (lexical FTS5/LIKE), not
/// here; this layer only ingests/replays events and proxies stores/consolidations.
pub struct MemoryLayer {
    pub(crate) session_events: Vec<Event>,
    long_term: Arc<MemoryStore>,
    session_db: Arc<dyn SessionStore>,
}

impl MemoryLayer {
    pub fn new(session_db: Arc<dyn SessionStore>, long_term: Arc<MemoryStore>) -> Self {
        Self {
            session_events: Vec::new(),
            long_term,
            session_db,
        }
    }

    pub fn ingest(&mut self, event: Event) {
        self.session_events.push(event);
    }

    pub async fn replay(&mut self, session_id: &str) -> Result<(), String> {
        let stored = self
            .session_db
            .get_events(session_id)
            .await
            .map_err(|e| e.to_string())?;
        self.session_events = stored.into_iter().map(|se| se.event).collect();
        Ok(())
    }

    pub async fn remember(&self, entry: MemoryEntry) -> Result<String, String> {
        self.long_term
            .store(entry)
            .await
            .map(|outcome| outcome.id)
            .map_err(|e| e.to_string())
    }

    pub async fn consolidate(
        &self,
        from_ids: &[String],
        into_content: &str,
        into_tags: &[String],
    ) -> Result<String, String> {
        self.long_term
            .consolidate(from_ids, into_content, into_tags)
            .await
            .map_err(|e| e.to_string())
    }

    pub fn event_count(&self) -> usize {
        self.session_events.len()
    }
}
