//! Unified execution boundary for one turn of work (AGT-002).
//!
//! An `ExecutionContext` bundles the deadline/cancellation/budget state every external
//! call must respect: a turn-level deadline, a default per-tool deadline, a
//! `CancellationToken`, the owning task id, and an optional tool-call budget. It is
//! created (or renewed) by the runtime at turn start and propagated to the action
//! engine, tools, MCP transports, user hooks, the browser tool and subagent runs.
//!
//! Semantics:
//! - A tool's effective deadline is `min(requested or default tool deadline, remaining
//!   turn time)` — a tool may never outlive its turn.
//! - Once the token is cancelled, no new tool call starts and in-flight bounded calls
//!   resolve as [`BoundedOutcome::Cancelled`].
//! - All timeout/cancel paths emit structured tracing events (`ToolDeadlineExceeded`,
//!   `Cancellation*`) carrying tool, task, elapsed and deadline fields.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

/// Default per-tool deadline when neither the tool call nor the config overrides it.
/// Matches the historical per-tool caps (execute_command/execute_python cap at 300s),
/// so installing the boundary changes no default behaviour.
pub const DEFAULT_TOOL_DEADLINE: Duration = Duration::from_secs(300);

/// Bounded grace given to the losing future of a bounded race to run its own
/// cleanup before it is dropped (P1-01). Tools that honour the context (process
/// groups, MCP stdio transports) observe the same token/deadline and finish
/// their cleanup inside this window, so the outer race never drops a future
/// mid-cleanup and leaks the resource.
pub const CLEANUP_GRACE: Duration = Duration::from_secs(2);

/// Outcome of a bounded execution race.
#[derive(Debug)]
pub enum BoundedOutcome<T> {
    /// The future resolved before the deadline and before cancellation.
    Completed(T),
    /// The deadline fired first; the losing future was dropped.
    DeadlineExceeded,
    /// The cancellation token fired first; the losing future was dropped.
    Cancelled,
}

impl<T> BoundedOutcome<T> {
    pub fn completed(self) -> Option<T> {
        match self {
            Self::Completed(value) => Some(value),
            _ => None,
        }
    }
}

/// Optional cap on tool calls a single context may start. `None` = unlimited (default).
/// Enforcement happens in the action engine via [`ExecutionContext::try_consume_tool_call`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResourceBudget {
    pub max_tool_calls: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct ExecutionContext {
    task_id: String,
    cancel: CancellationToken,
    turn_deadline: Option<Instant>,
    default_tool_deadline: Duration,
    budget: ResourceBudget,
    tool_calls_used: Arc<AtomicU32>,
    /// Subagent nesting depth (AGT-014): 0 for a top-level turn. Set explicitly
    /// via `with_depth` when a runtime derives its turn context from a parent;
    /// `spawn_subagent` refuses to recurse past the configured depth.
    depth: u32,
    /// Isolated scratch directory for this unit of work (AGT-014): tools that
    /// need temporary files (e.g. execute_python) place them here instead of
    /// the shared system temp, so concurrent subagents never collide.
    temp_dir: Option<std::path::PathBuf>,
}

impl Default for ExecutionContext {
    fn default() -> Self {
        Self::new("task")
    }
}

impl ExecutionContext {
    /// A fresh context: no turn deadline, default tool deadline, fresh token, no budget.
    pub fn new(task_id: impl Into<String>) -> Self {
        Self {
            task_id: task_id.into(),
            cancel: CancellationToken::new(),
            turn_deadline: None,
            default_tool_deadline: DEFAULT_TOOL_DEADLINE,
            budget: ResourceBudget::default(),
            tool_calls_used: Arc::new(AtomicU32::new(0)),
            depth: 0,
            temp_dir: None,
        }
    }

    pub fn with_turn_deadline(mut self, deadline: Duration) -> Self {
        self.turn_deadline = Some(Instant::now() + deadline);
        self
    }

    pub fn with_tool_deadline(mut self, deadline: Duration) -> Self {
        self.default_tool_deadline = deadline;
        self
    }

    pub fn with_budget(mut self, budget: ResourceBudget) -> Self {
        self.budget = budget;
        self
    }

    /// Build this context's token as a child of `parent`: cancelling the parent
    /// propagates here (used to wire a parent turn into a subagent run).
    pub fn with_parent_token(mut self, parent: &CancellationToken) -> Self {
        self.cancel = parent.child_token();
        self
    }

    /// Derive a context for a unit of work owned by this one (e.g. a background
    /// subagent task): same turn deadline, child cancellation token, fresh task id
    /// and budget counter. Cancelling this context cancels the child. Depth is
    /// inherited (the detached task is a boundary, not a new nesting level — the
    /// subagent runtime raises the depth itself); the temp dir is NOT inherited:
    /// each subagent run installs its own isolated directory.
    pub fn child(&self, task_id: impl Into<String>) -> Self {
        Self {
            task_id: task_id.into(),
            cancel: self.cancel.child_token(),
            turn_deadline: self.turn_deadline,
            default_tool_deadline: self.default_tool_deadline,
            budget: self.budget,
            tool_calls_used: Arc::new(AtomicU32::new(0)),
            depth: self.depth,
            temp_dir: None,
        }
    }

    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    /// Subagent nesting depth of this context (0 = top-level turn).
    pub fn depth(&self) -> u32 {
        self.depth
    }

    pub fn with_depth(mut self, depth: u32) -> Self {
        self.depth = depth;
        self
    }

    /// Isolated scratch directory for temporary files, when this unit of work
    /// has one installed (subagent runs).
    pub fn temp_dir(&self) -> Option<&std::path::Path> {
        self.temp_dir.as_deref()
    }

    pub fn with_temp_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.temp_dir = Some(dir);
        self
    }

    pub fn token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Request cancellation. Idempotent; emits `CancellationRequested`.
    pub fn cancel(&self) {
        if !self.cancel.is_cancelled() {
            crate::metrics::metrics().count("execution.cancellation_requested");
            tracing::info!(
                event = "CancellationRequested",
                task_id = %self.task_id,
                "execution context cancellation requested"
            );
        }
        self.cancel.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    pub fn budget(&self) -> ResourceBudget {
        self.budget
    }

    /// Consume one tool-call slot. Returns `false` when the budget is exhausted —
    /// the caller must not start the tool.
    pub fn try_consume_tool_call(&self) -> bool {
        match self.budget.max_tool_calls {
            Some(max) => self.tool_calls_used.fetch_add(1, Ordering::Relaxed) < max,
            None => true,
        }
    }

    /// Remaining time until the turn deadline; `None` when no turn deadline is set.
    /// `Some(0)` once expired.
    pub fn remaining_turn_time(&self) -> Option<Duration> {
        self.turn_deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
    }

    /// The absolute turn deadline instant, for propagating into derived contexts.
    pub fn turn_deadline_at(&self) -> Option<Instant> {
        self.turn_deadline
    }

    pub fn turn_expired(&self) -> bool {
        self.remaining_turn_time() == Some(Duration::ZERO)
    }

    /// Effective deadline for one tool call: the tighter of (the call's own request or
    /// the default tool deadline) and the remaining turn time. A tool may never outlive
    /// its turn; an already-expired turn yields `Duration::ZERO`.
    pub fn effective_deadline(&self, requested: Option<Duration>) -> Duration {
        let base = requested.unwrap_or(self.default_tool_deadline);
        match self.remaining_turn_time() {
            Some(remaining) => base.min(remaining),
            None => base,
        }
    }

    /// Race `future` against the effective deadline and the cancellation token,
    /// whichever fires first. Emits `ToolDeadlineExceeded` / `CancellationCompleted`
    /// with tool, task, elapsed and deadline fields on the losing paths.
    ///
    /// On a losing path the future is NOT dropped immediately: it gets up to
    /// [`CLEANUP_GRACE`] to observe the same token/deadline and run its own cleanup
    /// (kill a process group, terminate an MCP transport). Each external resource
    /// has exactly one cleanup owner — the inner future — and the outer race only
    /// drops it after that bounded grace (P1-01).
    pub async fn run_bounded<F, T>(
        &self,
        tool: &str,
        requested: Option<Duration>,
        future: F,
    ) -> BoundedOutcome<T>
    where
        F: std::future::Future<Output = T>,
    {
        let deadline = self.effective_deadline(requested);
        let started = Instant::now();
        tokio::pin!(future);
        enum Lost {
            Deadline,
            Cancel,
        }
        let lost = tokio::select! {
            value = &mut future => return BoundedOutcome::Completed(value),
            _ = tokio::time::sleep(deadline) => Lost::Deadline,
            _ = self.cancel.cancelled() => Lost::Cancel,
        };
        let elapsed = started.elapsed();
        // Bounded cleanup grace: the losing future owns its external resources and
        // typically observes the same token/deadline, so it finishes cleanup well
        // inside the grace; whatever remains afterwards is dropped here.
        let _ = tokio::time::timeout(CLEANUP_GRACE, &mut future).await;
        match lost {
            Lost::Deadline => {
                crate::metrics::metrics().count("tool.deadline_exceeded");
                tracing::warn!(
                    event = "ToolDeadlineExceeded",
                    tool = %tool,
                    task_id = %self.task_id,
                    elapsed_ms = elapsed.as_millis() as u64,
                    deadline_ms = deadline.as_millis() as u64,
                    "tool execution exceeded its deadline"
                );
                BoundedOutcome::DeadlineExceeded
            }
            Lost::Cancel => {
                crate::metrics::metrics().count("execution.cancellation_completed");
                tracing::info!(
                    event = "CancellationCompleted",
                    tool = %tool,
                    task_id = %self.task_id,
                    elapsed_ms = elapsed.as_millis() as u64,
                    deadline_ms = deadline.as_millis() as u64,
                    "bounded tool execution interrupted by cancellation"
                );
                BoundedOutcome::Cancelled
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_deadline_is_capped_by_remaining_turn_time() {
        let ctx = ExecutionContext::new("t")
            .with_tool_deadline(Duration::from_secs(300))
            .with_turn_deadline(Duration::from_secs(60));
        let effective = ctx.effective_deadline(Some(Duration::from_secs(120)));
        assert!(effective <= Duration::from_secs(60));
        assert!(effective > Duration::from_secs(55));
        // A tighter per-call request wins over the default.
        let tighter = ctx.effective_deadline(Some(Duration::from_secs(10)));
        assert_eq!(tighter, Duration::from_secs(10));
    }

    #[test]
    fn effective_deadline_without_turn_deadline_uses_request_or_default() {
        let ctx = ExecutionContext::new("t").with_tool_deadline(Duration::from_secs(42));
        assert_eq!(ctx.effective_deadline(None), Duration::from_secs(42));
        assert_eq!(
            ctx.effective_deadline(Some(Duration::from_secs(5))),
            Duration::from_secs(5)
        );
    }

    #[test]
    fn expired_turn_yields_zero_deadline() {
        let ctx = ExecutionContext::new("t").with_turn_deadline(Duration::from_millis(0));
        std::thread::sleep(Duration::from_millis(2));
        assert!(ctx.turn_expired());
        assert_eq!(
            ctx.effective_deadline(Some(Duration::from_secs(10))),
            Duration::ZERO
        );
    }

    #[test]
    fn child_inherits_turn_deadline_and_cascades_cancellation() {
        let parent = ExecutionContext::new("parent").with_turn_deadline(Duration::from_secs(60));
        let child = parent.child("child");
        assert!(child.remaining_turn_time().is_some());
        assert!(!child.is_cancelled());
        parent.cancel();
        assert!(child.is_cancelled());
    }

    #[test]
    fn child_inherits_depth_but_not_temp_dir() {
        // AGT-014: the detached boundary keeps the nesting depth (the subagent
        // runtime raises it) but must NOT share the parent's scratch directory.
        let parent = ExecutionContext::new("parent")
            .with_depth(1)
            .with_temp_dir(std::path::PathBuf::from("/tmp/holmes-parent"));
        let child = parent.child("child");
        assert_eq!(child.depth(), 1);
        assert!(child.temp_dir().is_none());
        assert_eq!(
            parent.temp_dir(),
            Some(std::path::Path::new("/tmp/holmes-parent"))
        );
    }

    #[test]
    fn budget_enforces_max_tool_calls() {
        let ctx = ExecutionContext::new("t").with_budget(ResourceBudget {
            max_tool_calls: Some(2),
        });
        assert!(ctx.try_consume_tool_call());
        assert!(ctx.try_consume_tool_call());
        assert!(!ctx.try_consume_tool_call());
        let unlimited = ExecutionContext::new("t");
        for _ in 0..1000 {
            assert!(unlimited.try_consume_tool_call());
        }
    }

    #[tokio::test]
    async fn run_bounded_reports_deadline_exceeded() {
        let ctx = ExecutionContext::new("t");
        let outcome = ctx
            .run_bounded("slow_tool", Some(Duration::from_millis(50)), async {
                tokio::time::sleep(Duration::from_secs(10)).await;
                1u32
            })
            .await;
        assert!(matches!(outcome, BoundedOutcome::DeadlineExceeded));
    }

    #[tokio::test]
    async fn run_bounded_reports_cancellation() {
        let ctx = ExecutionContext::new("t");
        let token = ctx.token();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            token.cancel();
        });
        let outcome = ctx
            .run_bounded("slow_tool", Some(Duration::from_secs(10)), async {
                tokio::time::sleep(Duration::from_secs(10)).await;
                1u32
            })
            .await;
        handle.await.unwrap();
        assert!(matches!(outcome, BoundedOutcome::Cancelled));
    }

    #[tokio::test]
    async fn run_bounded_waits_for_inner_cleanup_within_grace() {
        // P1-01: the outer race must not drop the losing future mid-cleanup. The
        // inner future observes the same token and needs ~50ms to clean up; the
        // bounded grace lets that cleanup finish before the future is dropped.
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let cleaned = Arc::new(AtomicBool::new(false));
        let cleaned_inner = cleaned.clone();
        let ctx = ExecutionContext::new("t");
        let token = ctx.token();
        let canceller = token.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            canceller.cancel();
        });
        let future = async move {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(60)) => 1u32,
                _ = token.cancelled() => {
                    // Simulated cleanup work (kill + reap) taking ~50ms.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    cleaned_inner.store(true, Ordering::SeqCst);
                    2u32
                }
            }
        };
        let started = Instant::now();
        let outcome = ctx
            .run_bounded("cleanup_tool", Some(Duration::from_secs(60)), future)
            .await;
        handle.await.unwrap();
        assert!(matches!(outcome, BoundedOutcome::Cancelled));
        assert!(
            cleaned.load(Ordering::SeqCst),
            "inner cleanup completed within the grace before the future was dropped"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "cleanup grace is bounded, took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn run_bounded_completes_fast_work() {
        let ctx = ExecutionContext::new("t");
        let outcome = ctx
            .run_bounded("fast_tool", Some(Duration::from_secs(5)), async { 7u32 })
            .await;
        assert_eq!(outcome.completed(), Some(7));
    }
}
