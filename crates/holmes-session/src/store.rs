use async_trait::async_trait;
use holmes_core::event::{Event, StoredEvent};
use holmes_core::types::*;
use std::path::PathBuf;

use crate::compaction_archive::CompactionArchive;
use crate::replay::ReplayedSessionContext;
use crate::{CreateSessionParams, SearchResult, SessionError};

#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn create_session(
        &self,
        params: CreateSessionParams,
    ) -> Result<Session, SessionError>;

    async fn append_event(
        &self,
        session_id: &str,
        event: &Event,
    ) -> Result<u64, SessionError>;

    async fn get_events(
        &self,
        session_id: &str,
    ) -> Result<Vec<StoredEvent>, SessionError>;

    /// Rebuild a complete runtime context from the session's event stream.
    async fn replay_session_context(
        &self,
        session_id: &str,
    ) -> Result<ReplayedSessionContext, SessionError>;

    /// Resolve (creating if needed) the on-disk workspace directory for a session.
    async fn session_workspace(&self, session_id: &str) -> Result<PathBuf, SessionError>;

    /// Persist a compaction archive and return its absolute path.
    async fn write_compaction_archive(
        &self,
        session_id: &str,
        compaction_event_index: u64,
        archive: &CompactionArchive,
    ) -> Result<String, SessionError>;

    /// Read back a compaction archive from a previously written path.
    async fn read_compaction_archive(&self, path: &str) -> Result<CompactionArchive, SessionError>;

    async fn list_sessions(
        &self,
        filter: &SessionFilter,
    ) -> Result<Vec<SessionSummary>, SessionError>;

    async fn end_session(
        &self,
        id: &str,
        reason: EndReason,
    ) -> Result<(), SessionError>;

    async fn reopen_session(&self, id: &str) -> Result<(), SessionError>;

    async fn set_goal_condition(
        &self,
        id: &str,
        condition: Option<&str>,
    ) -> Result<(), SessionError>;

    async fn mark_goal_achieved(&self, id: &str) -> Result<(), SessionError>;

    async fn get_session(&self, id: &str) -> Result<Option<Session>, SessionError>;

    async fn fork_session(
        &self,
        id: &str,
        fork_point: u64,
        new_title: &str,
    ) -> Result<Session, SessionError>;

    async fn update_token_counts(
        &self,
        id: &str,
        delta: &TokenDelta,
    ) -> Result<(), SessionError>;

    async fn truncate_events_after(
        &self,
        session_id: &str,
        event_index: u64,
    ) -> Result<(), SessionError>;

    async fn set_title(&self, id: &str, title: &str) -> Result<(), SessionError>;

    /// Persist the session's active mode (semantic-replay metadata sync).
    async fn set_mode(&self, id: &str, mode: SessionMode) -> Result<(), SessionError>;

    /// Persist the session's active model (semantic-replay metadata sync).
    async fn set_model(&self, id: &str, model: &str) -> Result<(), SessionError>;

    async fn search_events(
        &self,
        query: &str,
        top_k: u32,
    ) -> Result<Vec<SearchResult>, SessionError>;
}
