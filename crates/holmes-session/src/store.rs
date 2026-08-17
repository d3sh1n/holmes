use async_trait::async_trait;
use chrono::Utc;
use holmes_core::background::DurableTaskSink;
use holmes_core::event::{Event, StoredEvent};
use holmes_core::types::*;
use std::path::PathBuf;
use std::sync::Arc;

use crate::compaction_archive::CompactionArchive;
use crate::ledger_store::CaseLedgerStore;
use crate::replay::ReplayedSessionContext;
use crate::{CreateSessionParams, SearchResult, SessionError};

/// Startup semantics committed atomically with a fork (P1-04). The caller
/// (the CLI `SessionAssembler`) generates the child session id up front and
/// builds the tool registry / browser profile against it, so the fork
/// transaction can carry the complete startup event batch — SessionCreated,
/// SystemPromptSet, ModeSet, ModelSet, ActiveToolsSet and the branch summary —
/// instead of appending them one by one after the fact.
#[derive(Debug, Clone)]
pub struct ForkStartupSpec {
    /// Caller-generated child session id (registry/browser are pre-built
    /// against it before the fork commits).
    pub new_session_id: String,
    /// Model label for `SessionModelSet` when neither the fork-point replay
    /// nor the parent record carries one.
    pub model: Option<String>,
    pub provider: Option<String>,
    /// Prompt used when neither the fork-point replay nor the parent record
    /// carries a system prompt, so the child is always semantically complete.
    pub fallback_system_prompt: String,
    /// Active-tools snapshot of the child's freshly built registry.
    pub active_tool_names: Vec<String>,
    /// Static summary of the retained parent window, replayed as the branch's
    /// opening context.
    pub branch_summary: Option<BranchSummarySpec>,
}

#[derive(Debug, Clone)]
pub struct BranchSummarySpec {
    pub from_event_index: u64,
    pub to_event_index: u64,
    pub summary: String,
    pub reason: String,
}

impl ForkStartupSpec {
    /// Build the fork startup event batch from the child's resolved session
    /// values (mode/model/prompt already resolved by the fork path).
    pub fn to_events(&self, session: &Session) -> Vec<Event> {
        let now = Utc::now();
        let system_prompt = session
            .system_prompt
            .clone()
            .unwrap_or_else(|| self.fallback_system_prompt.clone());
        let mut events = vec![
            Event::SessionCreated {
                id: session.id.clone(),
                title: session.title.clone(),
                mode: session.mode.clone(),
                model: session.model.clone(),
                system_prompt: Some(system_prompt.clone()),
                parent_id: session.parent_session_id.clone(),
                fork_point: session.fork_point,
                created_at: now,
                tags: session.tags.clone(),
            },
            Event::SessionSystemPromptSet {
                prompt_hash: holmes_core::stable_prompt_hash(&system_prompt),
                content: system_prompt,
                source: "startup".into(),
                timestamp: now,
            },
            Event::SessionModeSet {
                mode: session.mode.clone(),
                source: Some("startup".into()),
                timestamp: Some(now),
            },
            Event::SessionModelSet {
                model: session
                    .model
                    .clone()
                    .or_else(|| self.model.clone())
                    .unwrap_or_else(|| "unknown".into()),
                provider: self.provider.clone(),
                source: "startup".into(),
                timestamp: now,
            },
            Event::ActiveToolsSet {
                tool_names: self.active_tool_names.clone(),
                source: "startup".into(),
                timestamp: now,
            },
        ];
        if let Some(summary) = &self.branch_summary {
            events.push(Event::BranchSummary {
                from_event_index: summary.from_event_index,
                to_event_index: summary.to_event_index,
                summary: summary.summary.clone(),
                reason: summary.reason.clone(),
                method: holmes_core::SummaryMethod::StaticFallback,
                timestamp: now,
            });
        }
        events
    }
}

#[async_trait]
/// Session persistence also exposes the case-scoped ledger boundary. Keeping
/// this as a supertrait makes the existing `Arc<dyn SessionStore>` runtime
/// handle ready for Ledger integration without downcasting to SQLite.
pub trait SessionStore: CaseLedgerStore + Send + Sync {
    async fn create_session(&self, params: CreateSessionParams) -> Result<Session, SessionError>;

    /// Atomically create a session together with its startup metadata events
    /// (AGT-008): the session row, every initial event and the denormalised
    /// counters commit in a single transaction, so a crash at any point of
    /// session initialisation leaves no half-initialised, resumable session.
    async fn create_session_with_events(
        &self,
        params: CreateSessionParams,
        events: Vec<Event>,
    ) -> Result<Session, SessionError>;

    async fn append_event(&self, session_id: &str, event: &Event) -> Result<u64, SessionError>;

    async fn get_events(&self, session_id: &str) -> Result<Vec<StoredEvent>, SessionError>;

    /// Rebuild a complete runtime context from the session's event stream.
    async fn replay_session_context(
        &self,
        session_id: &str,
    ) -> Result<ReplayedSessionContext, SessionError>;

    /// Resolve (creating if needed) the on-disk workspace directory for a session.
    async fn session_workspace(&self, session_id: &str) -> Result<PathBuf, SessionError>;

    /// The on-disk directory under which per-session artifacts (transcripts,
    /// compaction archives, checkpoints) persist, for stores that have one.
    /// `None` for purely in-memory stores (the default).
    fn sessions_dir(&self) -> Option<PathBuf> {
        None
    }

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

    /// Fork a session and commit its full startup semantics in the SAME
    /// transaction (P1-04): copied parent events, the startup event batch and
    /// the branch summary land atomically, so a fork can never leave a child
    /// that replay reports as semantically incomplete. The default falls back
    /// to a plain fork plus per-event appends for stores without transactional
    /// fork support; `SessionDB` overrides it with a single transaction.
    async fn fork_session_with_events(
        &self,
        id: &str,
        fork_point: u64,
        new_title: &str,
        startup: ForkStartupSpec,
    ) -> Result<Session, SessionError> {
        let session = self.fork_session(id, fork_point, new_title).await?;
        for event in startup.to_events(&session) {
            self.append_event(&session.id, &event).await?;
        }
        Ok(session)
    }

    async fn update_token_counts(&self, id: &str, delta: &TokenDelta) -> Result<(), SessionError>;

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

    /// Durable background-task sink (AGT-007), when this store persists task
    /// state. Stores without durable tasks return `None`; background spawns
    /// then run registry-only (no restart recovery), as before.
    fn durable_task_sink(&self) -> Option<Arc<dyn DurableTaskSink>> {
        None
    }

    /// Terminal durable tasks attributed to `parent_session_id` whose results
    /// have not been delivered into the conversation yet (P1-02). The runtime
    /// drains these at turn boundaries so results written before a crash still
    /// reach the parent. Default: no durable tasks, nothing pending.
    async fn list_undelivered_task_results(
        &self,
        _parent_session_id: &str,
    ) -> Result<Vec<holmes_core::background::UndeliveredTaskResult>, SessionError> {
        Ok(Vec::new())
    }

    /// Atomically deliver a terminal task's result into its parent session
    /// (P1-02): append the rendered `content` as a `UserMessage` event and mark
    /// the task delivered in the SAME transaction — there is no crash window
    /// between "result persisted in the conversation" and "delivery recorded",
    /// and a repeat call returns `AlreadyDelivered` instead of duplicating the
    /// event. Default for stores without durable tasks: `UnknownTask`, which
    /// tells the caller to fall back to plain in-memory delivery.
    async fn deliver_task_result(
        &self,
        _parent_session_id: &str,
        _task_id: &str,
        _content: &str,
    ) -> Result<holmes_core::background::TaskDeliveryOutcome, SessionError> {
        Ok(holmes_core::background::TaskDeliveryOutcome::UnknownTask)
    }
}
