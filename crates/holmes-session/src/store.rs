use async_trait::async_trait;
use holmes_core::event::{Event, StoredEvent};
use holmes_core::types::*;

use crate::compaction_archive::CompactionArchive;
use crate::db::{CreateSessionParams, SearchResult, SessionDB, SessionError};
use crate::replay::ReplayedSessionContext;

#[async_trait(?Send)]
pub trait SessionStore {
    async fn create_session(&self, params: CreateSessionParams) -> Result<Session, SessionError>;

    async fn append_event(&self, session_id: &str, event: &Event) -> Result<u64, SessionError>;

    async fn get_events(&self, session_id: &str) -> Result<Vec<StoredEvent>, SessionError>;

    async fn replay_session_context(
        &self,
        session_id: &str,
    ) -> Result<ReplayedSessionContext, SessionError>;

    async fn session_workspace(&self, session_id: &str)
        -> Result<std::path::PathBuf, SessionError>;

    async fn write_compaction_archive(
        &self,
        session_id: &str,
        compaction_event_index: u64,
        archive: &CompactionArchive,
    ) -> Result<String, SessionError>;

    async fn read_compaction_archive(&self, path: &str) -> Result<CompactionArchive, SessionError>;

    async fn list_sessions(
        &self,
        filter: &SessionFilter,
    ) -> Result<Vec<SessionSummary>, SessionError>;

    async fn end_session(&self, id: &str, reason: EndReason) -> Result<(), SessionError>;

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

    async fn update_token_counts(&self, id: &str, delta: &TokenDelta) -> Result<(), SessionError>;

    async fn truncate_events_after(
        &self,
        session_id: &str,
        event_index: u64,
    ) -> Result<(), SessionError>;

    async fn set_title(&self, id: &str, title: &str) -> Result<(), SessionError>;

    async fn search_events(
        &self,
        query: &str,
        top_k: u32,
    ) -> Result<Vec<SearchResult>, SessionError>;
}

#[async_trait(?Send)]
impl SessionStore for SessionDB {
    async fn create_session(&self, params: CreateSessionParams) -> Result<Session, SessionError> {
        SessionDB::create_session(self, params).await
    }

    async fn append_event(&self, session_id: &str, event: &Event) -> Result<u64, SessionError> {
        SessionDB::append_event(self, session_id, event).await
    }

    async fn get_events(&self, session_id: &str) -> Result<Vec<StoredEvent>, SessionError> {
        SessionDB::get_events(self, session_id).await
    }

    async fn replay_session_context(
        &self,
        session_id: &str,
    ) -> Result<ReplayedSessionContext, SessionError> {
        SessionDB::replay_session_context(self, session_id).await
    }

    async fn session_workspace(
        &self,
        session_id: &str,
    ) -> Result<std::path::PathBuf, SessionError> {
        SessionDB::session_workspace(self, session_id).await
    }

    async fn write_compaction_archive(
        &self,
        session_id: &str,
        compaction_event_index: u64,
        archive: &CompactionArchive,
    ) -> Result<String, SessionError> {
        SessionDB::write_compaction_archive(self, session_id, compaction_event_index, archive).await
    }

    async fn read_compaction_archive(&self, path: &str) -> Result<CompactionArchive, SessionError> {
        SessionDB::read_compaction_archive(self, path).await
    }

    async fn list_sessions(
        &self,
        filter: &SessionFilter,
    ) -> Result<Vec<SessionSummary>, SessionError> {
        SessionDB::list_sessions(self, filter).await
    }

    async fn end_session(&self, id: &str, reason: EndReason) -> Result<(), SessionError> {
        SessionDB::end_session(self, id, reason).await
    }

    async fn reopen_session(&self, id: &str) -> Result<(), SessionError> {
        SessionDB::reopen_session(self, id).await
    }

    async fn set_goal_condition(
        &self,
        id: &str,
        condition: Option<&str>,
    ) -> Result<(), SessionError> {
        SessionDB::set_goal_condition(self, id, condition).await
    }

    async fn mark_goal_achieved(&self, id: &str) -> Result<(), SessionError> {
        SessionDB::mark_goal_achieved(self, id).await
    }

    async fn get_session(&self, id: &str) -> Result<Option<Session>, SessionError> {
        SessionDB::get_session(self, id).await
    }

    async fn fork_session(
        &self,
        id: &str,
        fork_point: u64,
        new_title: &str,
    ) -> Result<Session, SessionError> {
        SessionDB::fork_session(self, id, fork_point, new_title).await
    }

    async fn update_token_counts(&self, id: &str, delta: &TokenDelta) -> Result<(), SessionError> {
        SessionDB::update_token_counts(self, id, delta).await
    }

    async fn truncate_events_after(
        &self,
        session_id: &str,
        event_index: u64,
    ) -> Result<(), SessionError> {
        SessionDB::truncate_events_after(self, session_id, event_index).await
    }

    async fn set_title(&self, id: &str, title: &str) -> Result<(), SessionError> {
        SessionDB::set_title(self, id, title).await
    }

    async fn search_events(
        &self,
        query: &str,
        top_k: u32,
    ) -> Result<Vec<SearchResult>, SessionError> {
        SessionDB::search_events(self, query, top_k).await
    }
}
