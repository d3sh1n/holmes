use std::collections::BTreeMap;
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use holmes_core::config::HolmesConfig;
use holmes_core::execution_context::ExecutionContext;
use holmes_core::session::RuntimeSession;
use holmes_core::state::{AttackPhase, AttackState};
use holmes_core::types::SessionMode;
use holmes_guards::GuardChain;
use holmes_mind_palace::MindPalace;
use holmes_session::{memory_store::MemoryStore, SessionStore};
use holmes_tools::registry::ToolRegistry;

use crate::deliberation::LlmBackend;
use crate::middleware::RuntimeMiddleware;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum InteractionMode {
    Autonomous,
    #[default]
    Interactive,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum RuntimePhase {
    #[default]
    Initializing,
    Recon,
    Hypothesize,
    Validate,
    Exploit,
    Complete,
}

#[derive(Debug, Clone, Default)]
pub struct EvidenceProjectionDedupe {
    pub seen_ports: HashSet<String>,
    pub seen_tech: HashSet<String>,
    pub seen_endpoints: HashSet<String>,
    pub seen_credentials: HashSet<String>,
    pub seen_findings: HashSet<String>,
}

pub struct RuntimeState {
    pub interaction_mode: InteractionMode,
    pub session_mode: SessionMode,
    pub phase: RuntimePhase,
    pub active_goal: Option<String>,
    pub observations: Vec<String>,
    pub recalled_memories: Vec<RuntimeMemory>,
    pub failures: Vec<String>,
    pub compatibility_state: AttackState,
    pub evidence_projection: EvidenceProjectionDedupe,
    /// Structured supervision state (AGT-009): goal, subtasks, hypotheses, evidence,
    /// recent action signatures and progress counters. Rebuilt from the event log on
    /// the first turn after a resume (see `TaskControlState::rebuild`).
    pub task_control: crate::task_control::TaskControlState,
    /// Case-scoped epistemic projection. Loaded from the authoritative Ledger
    /// event stream at turn start and refreshed after every committed meta
    /// action or evidence receipt.
    pub ledger: Option<holmes_core::ledger::LedgerSnapshot>,
    /// Commit-time execution bindings keyed by native tool call id. They are
    /// consumed by the ToolOutcome -> Evidence receipt path.
    pub action_bindings: BTreeMap<String, holmes_core::ledger::ActionBinding>,
    /// Durable Experiment assigned to this subagent by its parent Runtime.
    /// Every otherwise-unplanned executable call is bound to this Experiment;
    /// the child may produce Evidence but cannot finalize the Resolution.
    pub assigned_experiment: Option<holmes_core::ledger::ExperimentAssignment>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeMemory {
    pub id: String,
    pub content: String,
    pub relevance_score: f64,
}

impl RuntimeState {
    pub fn new(session_mode: SessionMode) -> Self {
        Self {
            interaction_mode: InteractionMode::default(),
            session_mode,
            phase: RuntimePhase::default(),
            active_goal: None,
            observations: Vec::new(),
            recalled_memories: Vec::new(),
            failures: Vec::new(),
            compatibility_state: permissive_attack_state(),
            evidence_projection: EvidenceProjectionDedupe::default(),
            task_control: crate::task_control::TaskControlState::default(),
            ledger: None,
            action_bindings: BTreeMap::new(),
            assigned_experiment: None,
        }
    }
}

impl Default for RuntimeState {
    fn default() -> Self {
        Self::new(SessionMode::default())
    }
}

/// Thread-safe FIFO of operator messages typed while a turn is in flight
/// (grok-build's "interjection" mechanism). The UI pushes completed lines in;
/// `AgentRuntime::run_turn` drains them at iteration boundaries and injects them
/// into the conversation as wrapped user messages, so the very next LLM request
/// sees the steering instead of waiting for a follow-up turn.
pub type SteeringQueue = Arc<Mutex<VecDeque<String>>>;

pub fn new_steering_queue() -> SteeringQueue {
    Arc::new(Mutex::new(VecDeque::new()))
}

pub struct RuntimeContext {
    pub session: RuntimeSession,
    pub session_id: String,
    pub session_db: Arc<dyn SessionStore>,
    pub memory_store: Arc<MemoryStore>,
    pub mind_palace: MindPalace,
    pub llm: Arc<dyn LlmBackend>,
    pub tools: Arc<ToolRegistry>,
    pub guards: GuardChain,
    pub state: RuntimeState,
    pub config: HolmesConfig,
    pub middlewares: Vec<Arc<dyn RuntimeMiddleware>>,
    /// Cooperative cancellation flag. When set to `true` (e.g. by the TUI on Esc),
    /// `AgentRuntime::run_turn` interrupts the loop at the next iteration boundary and
    /// returns `TurnOutcome::Interrupted`. Shared with the caller via `set_cancel_flag`.
    pub cancel: Arc<AtomicBool>,
    /// Steering messages typed by the operator while the turn runs. Drained at each
    /// iteration boundary (see `AgentRuntime::drain_steering`); shared with the caller
    /// via `set_steering_queue`. Anything still queued when the turn ends was never
    /// seen by the agent and stays here for the caller to re-route (e.g. into
    /// `queued_turns`).
    pub steering: SteeringQueue,
    /// Background subagent tasks spawned by `spawn_subagent` in background mode. The
    /// same handle is shared with the tool registry by the caller (see
    /// `set_background_tasks`); `run_turn` drains finished tasks at iteration
    /// boundaries (and once more at turn end) and injects them as system-reminders.
    pub background_tasks: holmes_core::background::BackgroundTasks,
    /// Unified execution boundary for the in-flight turn (AGT-002): deadlines,
    /// cancellation token, task id and budget, propagated to the action engine, tools,
    /// MCP transports and user hooks. Renewed at every turn start by
    /// `renew_execution_context` (tokens are one-shot; a cancelled context must never
    /// gate the next turn).
    pub exec: ExecutionContext,
    /// When set (subagent runs), the next renewed execution context derives its token
    /// from this parent token, so cancelling the parent turn propagates here.
    parent_token: Option<tokio_util::sync::CancellationToken>,
    /// Parent turn's absolute deadline (subagent runs): the renewed context's own turn
    /// deadline is capped by it, so a subagent can never outlive its parent turn.
    parent_turn_deadline: Option<std::time::Instant>,
    /// Parent context's nesting depth (subagent runs): the renewed context runs one
    /// level deeper, so `spawn_subagent` can refuse recursion past the configured
    /// `subagent.max_depth` (AGT-014).
    parent_depth: u32,
    /// Isolated scratch directory for this runtime's turns (subagent runs, AGT-014):
    /// installed by the runner and carried into every renewed execution context, so
    /// the subagent's temporary files never land in the shared system temp.
    turn_temp_dir: Option<std::path::PathBuf>,
}

impl RuntimeContext {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session: RuntimeSession,
        session_db: Arc<dyn SessionStore>,
        memory_store: Arc<MemoryStore>,
        mind_palace: MindPalace,
        llm: Arc<dyn LlmBackend>,
        tools: Arc<ToolRegistry>,
        guards: GuardChain,
        state: RuntimeState,
        config: HolmesConfig,
    ) -> Self {
        let session_id = session.id.clone();
        Self {
            session,
            session_id,
            session_db,
            memory_store,
            mind_palace,
            llm,
            tools,
            guards,
            state,
            config,
            middlewares: Vec::new(),
            cancel: Arc::new(AtomicBool::new(false)),
            steering: new_steering_queue(),
            background_tasks: holmes_core::background::BackgroundTasks::new(),
            exec: ExecutionContext::new("session"),
            parent_token: None,
            parent_turn_deadline: None,
            parent_depth: 0,
            turn_temp_dir: None,
        }
    }

    /// Renew the execution boundary for a fresh turn, applying `config.execution`
    /// (turn deadline, default tool deadline) and re-deriving the cancellation token
    /// from the installed parent token (subagent runs) or a fresh root token. Called
    /// by `AgentRuntime::run_turn` at turn start; a cancelled context from a previous
    /// turn must not leak into the next one.
    pub fn renew_execution_context(&mut self) {
        let mut exec = ExecutionContext::new(self.session_id.clone()).with_tool_deadline(
            std::time::Duration::from_millis(self.config.execution.tool_deadline_ms),
        );
        // Turn deadline: the configured one, capped by the parent turn's deadline when
        // running as a subagent (a child may never outlive its parent turn), and by the
        // subagent-specific wall-clock cap (AGT-014) when configured.
        let configured = self
            .config
            .execution
            .turn_deadline_ms
            .filter(|ms| *ms > 0)
            .map(|ms| std::time::Instant::now() + std::time::Duration::from_millis(ms));
        let is_subagent = self.parent_token.is_some();
        let subagent_cap = is_subagent
            .then_some(self.config.subagent.max_wall_clock_ms)
            .flatten()
            .filter(|ms| *ms > 0)
            .map(|ms| std::time::Instant::now() + std::time::Duration::from_millis(ms));
        let deadline = [configured, self.parent_turn_deadline, subagent_cap]
            .into_iter()
            .flatten()
            .reduce(std::cmp::min);
        if let Some(deadline) = deadline {
            let now = std::time::Instant::now();
            exec = exec.with_turn_deadline(deadline.saturating_duration_since(now));
        }
        if let Some(parent) = &self.parent_token {
            exec = exec.with_parent_token(parent);
            // One nesting level deeper than the parent (AGT-014).
            exec = exec.with_depth(self.parent_depth + 1);
            // Per-subagent tool-call budget (AGT-014); the action engine's budget
            // gate refuses new tool calls once it is exhausted.
            if let Some(max_tool_calls) = self.config.subagent.max_tool_calls.filter(|n| *n > 0) {
                exec = exec.with_budget(holmes_core::execution_context::ResourceBudget {
                    max_tool_calls: Some(max_tool_calls),
                });
            }
        }
        if let Some(dir) = &self.turn_temp_dir {
            exec = exec.with_temp_dir(dir.clone());
        }
        self.exec = exec;
    }

    /// Install the parent execution boundary (subagent runs): this runtime's turns
    /// derive their cancellation token from the parent's and cap their turn deadline
    /// by the parent's, so cancelling/expiring the parent propagates here.
    pub fn set_parent_execution(&mut self, parent: &ExecutionContext) {
        self.parent_token = Some(parent.token());
        self.parent_turn_deadline = parent.turn_deadline_at();
        self.parent_depth = parent.depth();
    }

    /// Install this runtime's isolated scratch directory (subagent runs): every
    /// renewed execution context carries it so temporary-file tools write there.
    pub fn set_turn_temp_dir(&mut self, dir: std::path::PathBuf) {
        self.turn_temp_dir = Some(dir);
    }

    pub fn with_middlewares(mut self, middlewares: Vec<Arc<dyn RuntimeMiddleware>>) -> Self {
        self.middlewares = middlewares;
        self
    }

    /// Share an external cancellation flag so a caller (e.g. the TUI interrupt
    /// watcher) can request the current turn stop at the next iteration boundary.
    /// Also handed to the background task registry, so a blocking `get_task_output`
    /// wait returns early on the same interrupt.
    pub fn set_cancel_flag(&mut self, cancel: Arc<AtomicBool>) {
        self.background_tasks.set_cancel(cancel.clone());
        self.cancel = cancel;
    }

    /// Share an external steering queue so a caller (e.g. the inline UI's busy-loop
    /// key dispatch) can inject operator messages into the in-flight turn; the loop
    /// drains them at iteration boundaries.
    pub fn set_steering_queue(&mut self, steering: SteeringQueue) {
        self.steering = steering;
    }

    /// Share the surface's background task registry so finished background subagents
    /// spawned by the tools in `self.tools` are drained into this runtime's turns.
    pub fn set_background_tasks(
        &mut self,
        background_tasks: holmes_core::background::BackgroundTasks,
    ) {
        self.background_tasks = background_tasks;
    }

    /// Replay persisted `FindingRecorded` events from this session's log back into the
    /// (freshly-built) validated zone, so findings recorded in prior turns/resumed
    /// sessions are present again instead of being silently wiped each turn.
    pub async fn seed_findings_from_history(&mut self) {
        if let Ok(stored) = self.session_db.get_events(&self.session_id).await {
            let events: Vec<_> = stored.into_iter().map(|s| s.event).collect();
            crate::action::seed_findings_from_events(&mut self.state.compatibility_state, &events);
        }
    }
}

fn permissive_attack_state() -> AttackState {
    let mut state = AttackState::new(
        String::new(),
        String::new(),
        "runtime".into(),
        "Holmes Runtime".into(),
        Vec::new(),
    );
    state.phase = AttackPhase::Recon;
    state.current_objective = "runtime bootstrap".into();
    state
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_state_defaults_are_phase_one_ready() {
        let state = RuntimeState::default();

        assert_eq!(state.interaction_mode, InteractionMode::Interactive);
        assert_eq!(state.session_mode, SessionMode::Pentest);
        assert_eq!(state.phase, RuntimePhase::Initializing);
        assert_eq!(state.active_goal, None);
        assert!(state.observations.is_empty());
        assert!(state.recalled_memories.is_empty());
        assert!(state.failures.is_empty());
        assert_eq!(state.compatibility_state.phase, AttackPhase::Recon);
        assert!(!state.compatibility_state.is_finished);
        assert!(state.evidence_projection.seen_ports.is_empty());
        assert!(state.evidence_projection.seen_tech.is_empty());
        assert!(state.evidence_projection.seen_endpoints.is_empty());
        assert!(state.evidence_projection.seen_credentials.is_empty());
        assert!(state.evidence_projection.seen_findings.is_empty());
    }
}
