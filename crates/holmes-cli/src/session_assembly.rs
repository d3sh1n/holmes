//! The single session lifecycle (P1-04). Every entry point — CLI startup
//! (fresh / `--resume` / `--continue`), interactive `/new`, `/resume`,
//! `/branch`, `/tree fork`, the classic TUI, the inline UI and subagent
//! spawns — creates and switches sessions through `SessionAssembler`, so each
//! session gets identical semantics regardless of where it was started:
//!
//! - creation is ONE `create_session_with_events` / `fork_session_with_events`
//!   transaction carrying the full startup event batch and the active-tools
//!   snapshot (a failure leaves no half-initialised session behind);
//! - resume always uses the canonical semantic replay
//!   (`load_session_runtime_from_store`) with the legacy fallback only for
//!   pre-semantics sessions — the same compaction / branch-summary / stored
//!   prompt handling as startup restore;
//! - switching rebuilds every session-scoped resource in one step
//!   (`switch_to`): runtime session, mind palace, tool registry (with the new
//!   durable-task parent binding), browser profile, guard chain, runtime
//!   state and workflow selector.

use anyhow::Context;
use holmes_core::background::BackgroundTasks;
use holmes_core::config::{Config, ResolvedModel};
use holmes_core::event::Event;
use holmes_core::session::RuntimeSession;
use holmes_core::types::SessionMode;
use holmes_guards::GuardChain;
use holmes_llm::client::LlmClient;
use holmes_mind_palace::MindPalace;
use holmes_runtime::RuntimeState;
use holmes_session::memory_store::MemoryStore;
use holmes_session::{BranchSummarySpec, CreateSessionParams, ForkStartupSpec, SessionStore};
use holmes_tools::ToolRegistry;
use std::path::PathBuf;
use std::sync::Arc;

use crate::chat::{
    active_tool_names, build_browser, build_tool_registry, load_session_runtime_from_store,
    rebuild_selector, ChatContext,
};

/// Everything a session switch installs into a `ChatContext`, produced only
/// by `SessionAssembler`.
pub(crate) struct AssembledSession {
    pub session_id: String,
    pub runtime_session: RuntimeSession,
    pub mind_palace: MindPalace,
    pub registry: Arc<ToolRegistry>,
    pub browser: Option<Arc<holmes_browser::BrowserManager>>,
    /// `false` when the session predates semantic startup metadata and the
    /// legacy replay fallback produced the runtime state.
    pub semantic_complete: bool,
    /// Durable background-task results already persisted for this session but
    /// not yet delivered into the conversation; the runtime drains them at
    /// the next turn boundary (P1-02), the UI just announces them.
    pub pending_task_results: usize,
    pub active_goal: Option<String>,
}

/// Builds sessions identically for every entry point. Clonable handles only;
/// `from_context` snapshots the pieces a `ChatContext` already carries.
#[derive(Clone)]
pub(crate) struct SessionAssembler {
    session_db: Arc<dyn SessionStore>,
    memory_store: Arc<MemoryStore>,
    llm: Arc<LlmClient>,
    config: Config,
    data_dir: PathBuf,
    /// The CLI template system prompt: fallback for sessions that predate
    /// stored prompts.
    system_prompt: String,
    background_tasks: BackgroundTasks,
    subagent_slots: Arc<tokio::sync::Semaphore>,
}

impl SessionAssembler {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        session_db: Arc<dyn SessionStore>,
        memory_store: Arc<MemoryStore>,
        llm: Arc<LlmClient>,
        config: Config,
        data_dir: PathBuf,
        system_prompt: String,
        background_tasks: BackgroundTasks,
        subagent_slots: Arc<tokio::sync::Semaphore>,
    ) -> Self {
        Self {
            session_db,
            memory_store,
            llm,
            config,
            data_dir,
            system_prompt,
            background_tasks,
            subagent_slots,
        }
    }

    pub(crate) fn from_context(ctx: &ChatContext) -> Self {
        Self::new(
            ctx.session_db.clone(),
            ctx.memory_store.clone(),
            ctx.llm.clone(),
            ctx.config.clone(),
            ctx.data_dir.clone(),
            ctx.system_prompt.clone(),
            ctx.background_tasks.clone(),
            ctx.subagent_slots.clone(),
        )
    }

    /// Build the session-scoped resources against a known session id. The id
    /// is generated before anything persists so the registry's durable-task
    /// binding and the browser profile commit together with the session row.
    async fn build_resources(
        &self,
        session_id: &str,
    ) -> (
        Option<Arc<holmes_browser::BrowserManager>>,
        Arc<ToolRegistry>,
    ) {
        let browser = build_browser(&self.config, &self.data_dir, session_id);
        let registry = self.rebuild_registry(session_id, browser.clone()).await;
        (browser, registry)
    }

    /// The one registry builder for an existing session (P2-04): session
    /// assembly and `/mcp reload` both go through here, so a reload can only
    /// change the MCP tool set — the caller passes the session's CURRENT
    /// browser handle and builtin/session tools are rebuilt identically.
    pub(crate) async fn rebuild_registry(
        &self,
        session_id: &str,
        browser: Option<Arc<holmes_browser::BrowserManager>>,
    ) -> Arc<ToolRegistry> {
        Arc::new(
            build_tool_registry(
                &self.config,
                Some(self.session_db.clone()),
                Some(self.memory_store.clone()),
                Some(self.llm.clone()),
                Some(session_id.to_string()),
                browser,
                &self.background_tasks,
                &self.subagent_slots,
            )
            .await,
        )
    }

    /// Fresh session: registry/browser are built against a pre-generated id,
    /// then the session row plus the complete startup event batch commit in a
    /// single transaction.
    pub(crate) async fn assemble_fresh(
        &self,
        mode: SessionMode,
        resolved_model: Option<ResolvedModel>,
        system_prompt: String,
        source: &str,
    ) -> anyhow::Result<AssembledSession> {
        let session_id = uuid::Uuid::new_v4().to_string();
        let (browser, registry) = self.build_resources(&session_id).await;
        let events = startup_events(
            &session_id,
            None,
            &mode,
            resolved_model.as_ref(),
            &system_prompt,
            None,
            None,
            Vec::new(),
            active_tool_names(&registry),
            chrono::Utc::now(),
        );
        self.session_db
            .create_session_with_events(
                CreateSessionParams {
                    id: Some(session_id.clone()),
                    title: None,
                    mode: Some(mode.clone()),
                    model: resolved_model.as_ref().map(|model| model.model.clone()),
                    system_prompt: Some(system_prompt.clone()),
                    parent_session_id: None,
                    fork_point: None,
                    source: Some(source.into()),
                    tags: vec![],
                },
                events,
            )
            .await
            .context("failed to create session")?;
        let runtime_session =
            RuntimeSession::new(session_id.clone(), mode).with_system_prompt(&system_prompt);
        let mind_palace = MindPalace::new(self.session_db.clone(), self.memory_store.clone());
        Ok(AssembledSession {
            session_id,
            runtime_session,
            mind_palace,
            registry,
            browser,
            semantic_complete: true,
            pending_task_results: 0,
            active_goal: None,
        })
    }

    /// Resume an existing session through the canonical semantic replay and
    /// rebuild every session-scoped resource for it.
    pub(crate) async fn assemble_resume(
        &self,
        session_id: &str,
        fallback_mode: SessionMode,
    ) -> anyhow::Result<AssembledSession> {
        let (browser, registry) = self.build_resources(session_id).await;
        self.assemble_existing(session_id, fallback_mode, browser, registry)
            .await
    }

    /// Fork `parent_session_id` at `fork_point`: the copied events, the full
    /// startup batch and the branch summary commit in one transaction, then
    /// the child loads through the same canonical replay as any resume.
    pub(crate) async fn assemble_fork(
        &self,
        parent_session_id: &str,
        fork_point: u64,
        title: &str,
        reason: &str,
    ) -> anyhow::Result<AssembledSession> {
        let new_id = uuid::Uuid::new_v4().to_string();
        let (browser, registry) = self.build_resources(&new_id).await;

        // Static branch summary over the retained parent window, computed up
        // front so it commits inside the fork transaction.
        let parent_events = self.session_db.get_events(parent_session_id).await?;
        let window: Vec<_> = parent_events
            .into_iter()
            .filter(|stored| stored.event_index <= fork_point)
            .collect();
        let summary = holmes_runtime::summary::static_branch_summary(&window, reason);

        let resolved_model = holmes_core::config::resolve_attack_model_provider(&self.config, None);
        let startup = ForkStartupSpec {
            new_session_id: new_id.clone(),
            model: resolved_model.as_ref().map(|model| model.model.clone()),
            provider: resolved_model.and_then(|model| model.provider),
            fallback_system_prompt: self.system_prompt.clone(),
            active_tool_names: active_tool_names(&registry),
            branch_summary: Some(BranchSummarySpec {
                from_event_index: 0,
                to_event_index: fork_point,
                summary,
                reason: reason.to_string(),
            }),
        };
        let session = self
            .session_db
            .fork_session_with_events(parent_session_id, fork_point, title, startup)
            .await
            .context("failed to fork session")?;
        self.assemble_existing(&session.id, session.mode.clone(), browser, registry)
            .await
    }

    /// Canonical load of an already-persisted session with pre-built
    /// resources; shared by resume and fork.
    async fn assemble_existing(
        &self,
        session_id: &str,
        fallback_mode: SessionMode,
        browser: Option<Arc<holmes_browser::BrowserManager>>,
        registry: Arc<ToolRegistry>,
    ) -> anyhow::Result<AssembledSession> {
        let (runtime_session, mind_palace, semantic_complete) = load_session_runtime_from_store(
            self.session_db.clone(),
            self.memory_store.clone(),
            session_id,
            fallback_mode,
            &self.system_prompt,
        )
        .await?;
        let pending_task_results = self
            .session_db
            .list_undelivered_task_results(session_id)
            .await
            .map(|results| results.len())
            .unwrap_or(0);
        let active_goal = self
            .session_db
            .get_session(session_id)
            .await
            .ok()
            .flatten()
            .and_then(|record| record.goal_condition);
        Ok(AssembledSession {
            session_id: session_id.to_string(),
            runtime_session,
            mind_palace,
            registry,
            browser,
            semantic_complete,
            pending_task_results,
            active_goal,
        })
    }
}

/// The semantic startup metadata batch (SessionCreated, SystemPromptSet,
/// ModeSet, ModelSet, ActiveToolsSet), built in memory so it can commit in
/// the same transaction as the session row. Shared by the assembler and the
/// subagent runner.
#[allow(clippy::too_many_arguments)]
pub(crate) fn startup_events(
    session_id: &str,
    title: Option<String>,
    mode: &SessionMode,
    resolved_model: Option<&ResolvedModel>,
    system_prompt: &str,
    parent_id: Option<String>,
    fork_point: Option<u64>,
    tags: Vec<String>,
    tool_names: Vec<String>,
    timestamp: chrono::DateTime<chrono::Utc>,
) -> Vec<Event> {
    vec![
        Event::SessionCreated {
            id: session_id.to_string(),
            title,
            mode: mode.clone(),
            model: resolved_model.map(|model| model.model.clone()),
            system_prompt: Some(system_prompt.to_string()),
            parent_id,
            fork_point,
            created_at: timestamp,
            tags,
        },
        Event::SessionSystemPromptSet {
            prompt_hash: holmes_core::stable_prompt_hash(system_prompt),
            content: system_prompt.to_string(),
            source: "startup".into(),
            timestamp,
        },
        Event::SessionModeSet {
            mode: mode.clone(),
            source: Some("startup".into()),
            timestamp: Some(timestamp),
        },
        Event::SessionModelSet {
            model: resolved_model
                .map(|model| model.model.clone())
                .unwrap_or_else(|| "unknown".into()),
            provider: resolved_model.and_then(|model| model.provider.clone()),
            source: "startup".into(),
            timestamp,
        },
        Event::ActiveToolsSet {
            tool_names,
            source: "startup".into(),
            timestamp,
        },
    ]
}

/// Install an assembled session into the context: the one place that swaps
/// runtime session, mind palace, registry, browser profile, guards, runtime
/// state and selector together (P1-04).
pub(crate) fn switch_to(ctx: &mut ChatContext, assembled: AssembledSession) {
    ctx.session_id = assembled.session_id;
    ctx.runtime_session = assembled.runtime_session;
    ctx.mind_palace = assembled.mind_palace;
    ctx.registry = assembled.registry;
    ctx.browser = assembled.browser;
    ctx.runtime_guards = GuardChain::from_config(&ctx.config.guards);
    let mut runtime_state = RuntimeState::new(ctx.runtime_session.mode.clone());
    runtime_state.active_goal = assembled.active_goal;
    ctx.runtime_state = runtime_state;
    ctx.queued_turns.clear();
    ctx.steering_notes.clear();
    ctx.steering
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    rebuild_selector(ctx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use holmes_session::SessionDB;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// P2-04: `/mcp reload` rebuilds the registry through
    /// `SessionAssembler::rebuild_registry` with the session's CURRENT browser
    /// handle. The tool set after a reload must be identical to the assembled
    /// one (no MCP servers configured here, so not even an MCP diff), and the
    /// `browser` tool in particular must survive.
    #[tokio::test]
    async fn rebuild_registry_preserves_non_mcp_tools() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let data_dir = std::env::temp_dir().join(format!("holmes-session-assembly-{nanos}"));
        std::fs::create_dir_all(&data_dir).unwrap();

        let config = Config::default(); // browser.enabled defaults to true
        let session_db: Arc<dyn SessionStore> =
            Arc::new(SessionDB::open(":memory:").await.expect("session db"));
        let memory_store = Arc::new(MemoryStore::open(":memory:").await.expect("memory store"));
        let llm = Arc::new(LlmClient::new(&config));
        let assembler = SessionAssembler::new(
            session_db,
            memory_store,
            llm,
            config,
            data_dir.clone(),
            "prompt".to_string(),
            BackgroundTasks::new(),
            Arc::new(tokio::sync::Semaphore::new(1)),
        );

        let session_id = "sess-reload".to_string();
        let (browser, assembled) = assembler.build_resources(&session_id).await;
        assert!(
            browser.is_some(),
            "browser.enabled defaults to true, so a handle must exist"
        );
        let before = active_tool_names(&assembled);
        assert!(
            before.iter().any(|name| name == "browser"),
            "assembled registry must contain the browser tool: {before:?}"
        );

        // The reload path: same builder, current browser handle.
        let reloaded = assembler
            .rebuild_registry(&session_id, browser.clone())
            .await;
        assert_eq!(
            before,
            active_tool_names(&reloaded),
            "reload must not change the non-MCP tool set"
        );

        // The regression itself: a reload without the handle drops the tool.
        let without_browser = assembler.rebuild_registry(&session_id, None).await;
        assert!(
            !active_tool_names(&without_browser)
                .iter()
                .any(|name| name == "browser"),
            "control case: no handle must mean no browser tool"
        );

        let _ = std::fs::remove_dir_all(data_dir);
    }
}
