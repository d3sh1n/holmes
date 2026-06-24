use holmes_core::event::Event;
use holmes_core::types::*;
use holmes_session::db::SessionDB;
use holmes_session::memory_store::{MemoryEntry, MemoryStore};
use std::collections::HashMap;
use std::sync::Arc;

pub struct MemoryLayer {
    pub(crate) session_events: Vec<Event>,
    long_term: Arc<MemoryStore>,
    session_db: Arc<SessionDB>,
    recall_cache: HashMap<String, Vec<Memory>>,
}

impl MemoryLayer {
    pub fn new(session_db: Arc<SessionDB>, long_term: Arc<MemoryStore>) -> Self {
        Self {
            session_events: Vec::new(),
            long_term,
            session_db,
            recall_cache: HashMap::new(),
        }
    }

    pub fn ingest(&mut self, event: Event) {
        self.session_events.push(event);
    }

    pub fn recent(&self, n: usize) -> &[Event] {
        let start = self.session_events.len().saturating_sub(n);
        &self.session_events[start..]
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

    pub async fn recall(&mut self, context: &ContextSnapshot, top_k: usize) -> Vec<Memory> {
        let query = build_recall_query(context);
        if let Some(cached) = self.recall_cache.get(&query) {
            return cached.clone();
        }
        let results = self
            .long_term
            .search(&query, top_k as u32)
            .await
            .unwrap_or_default();
        self.recall_cache.insert(query, results.clone());
        results
    }

    pub async fn remember(&self, entry: MemoryEntry) -> Result<String, String> {
        self.long_term.store(entry).await.map_err(|e| e.to_string())
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

    pub fn similarity(a: &Memory, b: &Memory) -> f64 {
        let a_tags: std::collections::HashSet<&String> = a.tags.iter().collect();
        let b_tags: std::collections::HashSet<&String> = b.tags.iter().collect();
        let intersection = a_tags.intersection(&b_tags).count();
        let union = a_tags.union(&b_tags).count();
        if union == 0 {
            return 0.0;
        }
        intersection as f64 / union as f64
    }

    pub fn event_count(&self) -> usize {
        self.session_events.len()
    }
}

fn build_recall_query(context: &ContextSnapshot) -> String {
    let mut parts = vec![context.summary.clone()];
    parts.extend(context.preserved_keys.clone());
    for ctx in &context.active_contexts {
        parts.push(format!("{} {}", ctx.kind_str(), ctx.identifier));
    }
    parts.join(" ")
}

pub(crate) trait ContextTargetExt {
    fn kind_str(&self) -> &'static str;
}

impl ContextTargetExt for ContextTarget {
    fn kind_str(&self) -> &'static str {
        match self.kind {
            ContextKind::Host => "host",
            ContextKind::File => "file",
            ContextKind::Function => "function",
            ContextKind::Module => "module",
            ContextKind::Network => "network",
            ContextKind::Binary => "binary",
        }
    }
}
