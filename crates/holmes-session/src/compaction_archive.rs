use chrono::{DateTime, Utc};
use holmes_core::event::StoredEvent;
use holmes_core::{CompactionTrigger, Event, Message};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionArchive {
    pub session_id: String,
    pub compaction_event_index: u64,
    pub trigger: CompactionTrigger,
    pub archived_event_range: Option<(u64, u64)>,
    pub messages: Vec<Message>,
    pub events: Vec<ArchivedEvent>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchivedEvent {
    pub event_index: u64,
    pub event_type: String,
    pub timestamp: DateTime<Utc>,
    pub event: Event,
}

impl ArchivedEvent {
    pub fn from_stored(stored: &StoredEvent) -> Self {
        Self {
            event_index: stored.event_index,
            event_type: stored.event.category().to_string(),
            timestamp: stored.timestamp,
            event: stored.event.clone(),
        }
    }
}
