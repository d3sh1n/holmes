use chrono::{DateTime, Utc};
use holmes_core::event::StoredEvent;
use holmes_core::{CompactionTrigger, Event, Message};
use serde::{Deserialize, Serialize};

pub const COMPACTION_ARCHIVE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionArchive {
    pub schema_version: u32,
    pub session_id: String,
    pub compaction_event_index: u64,
    pub trigger: CompactionTrigger,
    pub archived_event_range: Option<ArchivedEventRange>,
    pub messages: Vec<Message>,
    pub events: Vec<ArchivedEvent>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchivedEventRange {
    pub start: u64,
    pub end: u64,
}

impl ArchivedEventRange {
    pub fn new(start: u64, end: u64) -> Self {
        Self { start, end }
    }
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
