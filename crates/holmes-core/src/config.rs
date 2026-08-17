use serde::{Deserialize, Serialize};

use crate::ledger::ThinkMode;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HolmesConfig {
    pub agent: AgentConfig,
    #[serde(default)]
    pub permissions: PermissionConfig,
    pub llm: LlmConfig,
    pub compressor: CompressorConfig,
    #[serde(default)]
    pub learning: LearningConfig,
    pub guards: GuardConfig,
    pub memory: MemoryConfig,
    pub skills: SkillsConfig,
    pub mcp: McpConfig,
    pub browser: BrowserConfig,
    #[serde(default)]
    pub safety: SafetyConfig,
    #[serde(default)]
    pub hooks: HooksConfig,
    #[serde(default)]
    pub execution: ExecutionConfig,
    #[serde(default)]
    pub supervisor: SupervisorConfig,
    #[serde(default)]
    pub subagent: SubagentConfig,
    #[serde(default)]
    pub cognition: CognitionConfig,
    #[serde(default)]
    pub ledger: LedgerConfig,
    #[serde(default)]
    pub experiments: ExperimentConfig,
    pub output_dir: String,
}

/// Bounded policy for the private Propose/Critique/Commit loop. Raw proposal
/// and critique bodies are deliberately not a configurable persistence option.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CognitionConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_think_mode")]
    pub mode: ThinkMode,
    /// Total LLM calls in one cognitive loop, including the final Commit.
    #[serde(default = "default_cognition_max_rounds")]
    pub max_rounds: u8,
    #[serde(default = "default_cognition_max_candidates")]
    pub max_candidates: usize,
    #[serde(default = "default_cognition_max_think_tokens")]
    pub max_think_tokens: u32,
    #[serde(default = "default_cognition_max_think_time_ms")]
    pub max_think_time_ms: u64,
    #[serde(default = "default_cognition_max_rebases")]
    pub max_rebases: u8,
    #[serde(default = "default_true")]
    pub deep_on_finish: bool,
    #[serde(default = "default_true")]
    pub deep_on_finding: bool,
    #[serde(default = "default_true")]
    pub deep_on_contradiction: bool,
    #[serde(default = "default_true")]
    pub deep_on_high_risk_action: bool,
    #[serde(default = "default_true")]
    pub deep_on_stagnation: bool,
    #[serde(default = "default_true")]
    pub persist_commit_summary: bool,
    /// This field exists only to reject unsafe legacy/custom configuration.
    /// v2 never persists private Proposal/Critique output.
    #[serde(default, deserialize_with = "reject_raw_reasoning_persistence")]
    pub persist_raw_reasoning: bool,
}

fn default_think_mode() -> ThinkMode {
    ThinkMode::Adaptive
}
fn default_cognition_max_rounds() -> u8 {
    3
}
fn default_cognition_max_candidates() -> usize {
    4
}
fn default_cognition_max_think_tokens() -> u32 {
    6_000
}
fn default_cognition_max_think_time_ms() -> u64 {
    45_000
}
fn default_cognition_max_rebases() -> u8 {
    2
}

fn reject_raw_reasoning_persistence<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let enabled = bool::deserialize(deserializer)?;
    if enabled {
        return Err(serde::de::Error::custom(
            "cognition.persist_raw_reasoning=true is forbidden by Hypothesis Ledger v2",
        ));
    }
    Ok(false)
}

impl Default for CognitionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: default_think_mode(),
            max_rounds: default_cognition_max_rounds(),
            max_candidates: default_cognition_max_candidates(),
            max_think_tokens: default_cognition_max_think_tokens(),
            max_think_time_ms: default_cognition_max_think_time_ms(),
            max_rebases: default_cognition_max_rebases(),
            deep_on_finish: true,
            deep_on_finding: true,
            deep_on_contradiction: true,
            deep_on_high_risk_action: true,
            deep_on_stagnation: true,
            persist_commit_summary: true,
            persist_raw_reasoning: false,
        }
    }
}

/// Bounds for the public Ledger projection and resolution policy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LedgerConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_active_hypotheses")]
    pub max_active_hypotheses_in_context: usize,
    #[serde(default = "default_predictions_in_context")]
    pub max_predictions_in_context: usize,
    #[serde(default = "default_recent_evidence_in_context")]
    pub max_recent_evidence_in_context: usize,
    #[serde(default = "default_contradictions_in_context")]
    pub max_contradictions_in_context: usize,
    #[serde(default = "default_snapshot_every_events")]
    pub snapshot_every_events: u64,
    #[serde(default = "default_true")]
    pub semantic_verifier: bool,
    #[serde(default = "default_true")]
    pub require_prediction_for_high_priority: bool,
}

fn default_active_hypotheses() -> usize {
    8
}
fn default_predictions_in_context() -> usize {
    8
}
fn default_recent_evidence_in_context() -> usize {
    12
}
fn default_contradictions_in_context() -> usize {
    5
}
fn default_snapshot_every_events() -> u64 {
    100
}

impl Default for LedgerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_active_hypotheses_in_context: default_active_hypotheses(),
            max_predictions_in_context: default_predictions_in_context(),
            max_recent_evidence_in_context: default_recent_evidence_in_context(),
            max_contradictions_in_context: default_contradictions_in_context(),
            snapshot_every_events: default_snapshot_every_events(),
            semantic_verifier: true,
            require_prediction_for_high_priority: true,
        }
    }
}

/// Durable Experiment dispatch policy. The per-case cap composes with the
/// existing process-wide subagent semaphore; the smaller limit wins.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExperimentConfig {
    #[serde(default = "default_experiment_concurrency")]
    pub max_concurrent_per_case: usize,
    #[serde(default = "default_experiment_lease_ms")]
    pub default_lease_ms: u64,
    #[serde(default = "default_experiment_heartbeat_ms")]
    pub heartbeat_ms: u64,
    #[serde(default = "default_experiment_attempts")]
    pub max_attempts: u32,
}

fn default_experiment_concurrency() -> usize {
    4
}
fn default_experiment_lease_ms() -> u64 {
    60_000
}
fn default_experiment_heartbeat_ms() -> u64 {
    20_000
}
fn default_experiment_attempts() -> u32 {
    3
}

impl Default for ExperimentConfig {
    fn default() -> Self {
        Self {
            max_concurrent_per_case: default_experiment_concurrency(),
            default_lease_ms: default_experiment_lease_ms(),
            heartbeat_ms: default_experiment_heartbeat_ms(),
            max_attempts: default_experiment_attempts(),
        }
    }
}

/// Turn supervision and completion verification (AGT-009/010): repetition and
/// stagnation detection thresholds, and how `Finish` claims are gated.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SupervisorConfig {
    /// Consecutive identical failing tool calls (same tool + equivalent arguments)
    /// tolerated before the supervisor forces a strategy change; repeating again
    /// after that stops the turn and asks the operator.
    #[serde(default = "default_max_repeat_action")]
    pub max_repeat_action: u32,
    /// Consecutive iterations without progress (no new evidence, no novel successful
    /// action) before a reflection prompt is injected; a second full window stops
    /// the turn with a resumable partial result.
    #[serde(default = "default_stagnation_limit")]
    pub stagnation_limit: u32,
    /// How many times a rejected `Finish` is fed back into the loop before the turn
    /// ends with a partial result listing the remaining work.
    #[serde(default = "default_max_verification_retries")]
    pub max_verification_retries: u32,
    /// Whether a standing (semantic) goal gets a model-based review after the
    /// deterministic completion checks pass. Deterministic checks always run.
    #[serde(default = "default_model_verification")]
    pub model_verification: bool,
}

fn default_max_repeat_action() -> u32 {
    3
}
fn default_stagnation_limit() -> u32 {
    4
}
fn default_max_verification_retries() -> u32 {
    2
}
fn default_model_verification() -> bool {
    true
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            max_repeat_action: default_max_repeat_action(),
            stagnation_limit: default_stagnation_limit(),
            max_verification_retries: default_max_verification_retries(),
            model_verification: default_model_verification(),
        }
    }
}

/// Subagent protocol and resource-isolation configuration (AGT-013/014): how deep
/// subagents may nest, how many may run concurrently process-wide, and the
/// per-run resource budget.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubagentConfig {
    /// Maximum nesting depth of subagent runs: the top-level turn is depth 0, a
    /// subagent it spawns runs at depth 1, and a spawn attempted at depth
    /// `max_depth` is refused. Prevents unbounded recursive delegation.
    #[serde(default = "default_subagent_max_depth")]
    pub max_depth: u32,
    /// Process-wide cap on concurrently running subagents (all nesting levels
    /// share one semaphore). A spawn beyond the cap is refused so the model can
    /// retry or run synchronously instead of over-committing the provider.
    #[serde(default = "default_subagent_max_concurrent")]
    pub max_concurrent: u32,
    /// Per-subagent tool-call budget enforced through the execution context.
    /// `None`/0 = unlimited (default).
    #[serde(default)]
    pub max_tool_calls: Option<u32>,
    /// Wall-clock cap (ms) for a single subagent run, applied on top of (and
    /// capped by) the parent turn's deadline. `None`/0 = no subagent-specific cap.
    #[serde(default)]
    pub max_wall_clock_ms: Option<u64>,
}

fn default_subagent_max_depth() -> u32 {
    2
}
fn default_subagent_max_concurrent() -> u32 {
    4
}

impl Default for SubagentConfig {
    fn default() -> Self {
        Self {
            max_depth: default_subagent_max_depth(),
            max_concurrent: default_subagent_max_concurrent(),
            max_tool_calls: None,
            max_wall_clock_ms: None,
        }
    }
}

/// Unified execution-boundary configuration (AGT-002): deadlines and failure policies
/// shared by tools, MCP transports and user hooks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionConfig {
    /// Wall-clock deadline (ms) for a whole turn. `None`/0 = no turn deadline
    /// (default). When set, every tool deadline is additionally capped by the
    /// remaining turn time and the loop stops once the turn expires.
    #[serde(default)]
    pub turn_deadline_ms: Option<u64>,
    /// Default per-tool deadline (ms) applied when a tool call does not request a
    /// tighter timeout itself. 300s matches the historical per-tool caps.
    #[serde(default = "default_tool_deadline_ms")]
    pub tool_deadline_ms: u64,
    /// Per-request timeout (ms) for MCP calls (stdio read/write and HTTP requests).
    /// After a stdio timeout the transport is terminated and later calls fail fast.
    #[serde(default = "default_mcp_request_timeout_ms")]
    pub mcp_request_timeout_ms: u64,
}

fn default_tool_deadline_ms() -> u64 {
    300_000
}
fn default_mcp_request_timeout_ms() -> u64 {
    30_000
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            turn_deadline_ms: None,
            tool_deadline_ms: default_tool_deadline_ms(),
            mcp_request_timeout_ms: default_mcp_request_timeout_ms(),
        }
    }
}

/// What to do when a `before_tool` hook cannot produce a verdict (timeout, spawn or
/// wait failure) — distinct from a hook vetoing via its exit code.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HookFailurePolicy {
    /// Fail closed: a hook that cannot run blocks the tool call (default for hooks
    /// guarding side effects).
    #[default]
    Deny,
    /// Allow the call and log a warning.
    Warn,
    /// Allow the call silently.
    Skip,
}

/// User-configurable shell hooks fired around tool calls — an escape hatch for deterministic
/// policy, auditing, or side effects (à la Claude Code hooks). Each hook receives the tool
/// name in `$HOLMES_TOOL_NAME` and the tool arguments as JSON both in `$HOLMES_TOOL_ARGS` and
/// on stdin. A `before_tool` hook with `blocking: true` that exits non-zero blocks the call
/// (its stderr becomes the block reason).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HooksConfig {
    /// Hooks run before each tool call (in order). A blocking hook can veto the call.
    #[serde(default)]
    pub before_tool: Vec<HookConfig>,
    /// Hooks run after each tool call (in order). Advisory only — cannot block.
    #[serde(default)]
    pub after_tool: Vec<HookConfig>,
    /// Max wall time (ms) a single hook may run before it is killed (process group
    /// included). Default 10s.
    #[serde(default = "default_hook_timeout_ms")]
    pub timeout_ms: u64,
    /// Policy applied when a `before_tool` hook fails to produce a verdict (timeout /
    /// spawn / wait error). Default `deny` — fail closed before side effects.
    #[serde(default)]
    pub on_failure: HookFailurePolicy,
}

fn default_hook_timeout_ms() -> u64 {
    10_000
}

impl Default for HooksConfig {
    fn default() -> Self {
        Self {
            before_tool: Vec::new(),
            after_tool: Vec::new(),
            timeout_ms: default_hook_timeout_ms(),
            on_failure: HookFailurePolicy::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookConfig {
    /// Tool-name matcher: exact name, `*` (or empty) for all, or a `prefix*` glob.
    #[serde(default)]
    pub matcher: String,
    /// Shell command to run (`sh -c <command>`).
    pub command: String,
    /// If true (before_tool only), a non-zero exit blocks the tool call.
    #[serde(default)]
    pub blocking: bool,
}

impl HookConfig {
    /// Whether this hook applies to `tool_name` (exact, `*`/empty wildcard, or `prefix*`).
    pub fn matches(&self, tool_name: &str) -> bool {
        let m = self.matcher.trim();
        if m.is_empty() || m == "*" {
            return true;
        }
        if let Some(prefix) = m.strip_suffix('*') {
            return tool_name.starts_with(prefix);
        }
        m == tool_name
    }
}

/// Operational safety limits distinct from LLM rate limiting.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct SafetyConfig {
    /// Max outbound egress tool calls (http_request / web_fetch / browser /
    /// execute_command / execute_python) per minute, throttled within a turn. `None`/0 =
    /// unlimited. Prevents a runaway loop from hammering (DoS-ing) the target.
    #[serde(default)]
    pub egress_rpm: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub max_iterations: u32,
    pub generate_reports: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PermissionConfig {
    #[serde(default)]
    pub mode: PermissionMode,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default)]
    pub disallowed_tools: Vec<String>,
    #[serde(default = "default_true")]
    pub auto_approve_read_only: bool,
}

impl Default for PermissionConfig {
    fn default() -> Self {
        Self {
            mode: PermissionMode::Default,
            allowed_tools: Vec::new(),
            disallowed_tools: Vec::new(),
            auto_approve_read_only: default_true(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum PermissionMode {
    /// Normal Holmes behavior: policy lists and guards decide.
    #[default]
    Default,
    /// Planning-only mode. Tools are blocked so Holmes must reason or ask.
    Plan,
    /// Read-only mode. Only tools marked read-only can run.
    ReadOnly,
    /// Accept edits mode. Holmes can perform file edits without explicit confirmation.
    AcceptEdits,
    /// Non-interactive mode. Policy lists still apply, but Holmes will not ask for approval.
    DontAsk,
    /// Interactive approval: read-only tools run freely, but each mutating tool call is
    /// referred to an `ApprovalHandler` (e.g. a TUI y/n prompt) before it executes.
    Ask,
    /// Maximum autonomy. Policy lists still apply; security guards remain active.
    Bypass,
}

impl std::str::FromStr for PermissionMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "default" => Ok(PermissionMode::Default),
            "plan" => Ok(PermissionMode::Plan),
            "read-only" | "readonly" => Ok(PermissionMode::ReadOnly),
            "accept-edits" | "acceptedits" => Ok(PermissionMode::AcceptEdits),
            "dont-ask" | "dontask" => Ok(PermissionMode::DontAsk),
            "ask" => Ok(PermissionMode::Ask),
            "bypass" => Ok(PermissionMode::Bypass),
            _ => Err("Invalid permission mode. Expected 'plan', 'default', 'read-only', 'accept-edits', 'dont-ask', 'ask', or 'bypass'.".to_string()),
        }
    }
}

impl std::fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PermissionMode::Default => write!(f, "default"),
            PermissionMode::Plan => write!(f, "plan"),
            PermissionMode::ReadOnly => write!(f, "read-only"),
            PermissionMode::AcceptEdits => write!(f, "accept-edits"),
            PermissionMode::DontAsk => write!(f, "dont-ask"),
            PermissionMode::Ask => write!(f, "ask"),
            PermissionMode::Bypass => write!(f, "bypass"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    pub providers: Vec<ProviderConfig>,
    pub roles: RoleConfig,
    /// Use the streaming (SSE) wire path for LLM calls. Off by default (the buffered
    /// path is unchanged). The agent still needs the full response to act, so this
    /// mainly enables incremental text display and stream-required gateways.
    #[serde(default)]
    pub stream: bool,
    /// Extended-thinking budget in tokens. 0 = disabled. When > 0, requests enable
    /// thinking; the model's signed thinking blocks are preserved and echoed back on
    /// the next request (required by the API when they precede a tool_use).
    #[serde(default)]
    pub thinking_budget: u32,
    /// Base cooldown (ms) applied to a provider after a terminal request-level failure.
    /// The window doubles on each consecutive failure (with ±20% jitter) up to
    /// `provider_cooldown_max_ms`, then the provider is probed again half-open.
    #[serde(default = "default_provider_cooldown_base_ms")]
    pub provider_cooldown_base_ms: u64,
    /// Upper bound (ms) for the exponential provider cooldown window.
    #[serde(default = "default_provider_cooldown_max_ms")]
    pub provider_cooldown_max_ms: u64,
    /// Total deadline (ms) for a single LLM call across all retries, failovers and
    /// cooldown waits. When exceeded the call fails deterministically with an
    /// "all providers unavailable" error instead of waiting for a recovery.
    #[serde(default = "default_call_deadline_ms")]
    pub call_deadline_ms: u64,
}

fn default_provider_cooldown_base_ms() -> u64 {
    5_000
}
fn default_provider_cooldown_max_ms() -> u64 {
    300_000
}
fn default_call_deadline_ms() -> u64 {
    300_000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    /// Backend HTTP base URL.
    #[serde(alias = "api_base")]
    pub base_url: String,
    /// Resolved API key (after env-var lookup).
    #[serde(default)]
    pub api_key: String,
    /// Optional environment variable to read the key from at runtime.
    #[serde(default)]
    pub api_key_env: Option<String>,
    pub model: String,
    /// API wire format (`openai` | `anthropic`).
    #[serde(default, alias = "format")]
    pub api_format: ApiFormat,
    #[serde(default)]
    pub priority: u32,
    #[serde(default = "default_rpm_limit")]
    pub rpm_limit: u32,
}

fn default_rpm_limit() -> u32 {
    60
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum ApiFormat {
    Openai,
    #[default]
    Anthropic,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModel {
    pub model: String,
    pub provider: Option<String>,
}

pub fn resolve_attack_model_provider(
    config: &HolmesConfig,
    override_model: Option<String>,
) -> Option<ResolvedModel> {
    if let Some(model) = override_model {
        return Some(ResolvedModel {
            model,
            provider: None,
        });
    }

    let role = &config.llm.roles.attack_agent;
    config
        .llm
        .providers
        .iter()
        .find(|provider| &provider.name == role)
        .or_else(|| {
            config
                .llm
                .providers
                .iter()
                .min_by_key(|provider| provider.priority)
        })
        .map(|provider| ResolvedModel {
            model: provider.model.clone(),
            provider: Some(provider.name.clone()),
        })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleConfig {
    pub attack_agent: String,
    #[serde(default)]
    pub supervisor: String,
    #[serde(default)]
    pub compressor: String,
    #[serde(default)]
    pub skill_evolver: String,
    #[serde(default)]
    pub goal_evaluator: String,
}

/// Alias for compatibility with apeiron-core call sites that imported `RoleAssignment`.
pub type RoleAssignment = RoleConfig;

/// Alias for compatibility with apeiron-core call sites that imported `Config`.
pub type Config = HolmesConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressorConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_protect_last_n")]
    pub protect_last_n: usize,
    #[serde(default = "default_target_ratio")]
    pub target_ratio: f64,
    #[serde(default = "default_max_summary_tokens")]
    pub max_summary_tokens: u32,
    /// When true, the middle of the transcript is summarized by an LLM call instead of
    /// the static keyword template. Off by default (keeps compaction deterministic).
    #[serde(default)]
    pub llm_summary: bool,
    /// When true (and `llm_summary` is on), the compressor role is asked to extract
    /// must-survive case state (findings/credentials/hypotheses/next steps) right before
    /// compaction; the notes are written to long-term memory AND prepended to the
    /// compaction summary. Only meaningful in LLM mode — the static path stays free of
    /// extra LLM calls so scripted harness replays are unaffected.
    #[serde(default = "default_true")]
    pub pre_compact_flush: bool,
    pub context_limit: u32,
    pub threshold: f64,
    pub protected_head: usize,
    pub protected_tail_tokens: u32,
}

fn default_true() -> bool {
    true
}

fn default_protect_last_n() -> usize {
    20
}

fn default_target_ratio() -> f64 {
    0.25
}

fn default_max_summary_tokens() -> u32 {
    12_000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearningConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Run the end-of-turn learning review every N turns (1 = every turn).
    /// Consumed by the runtime's turn loop (`review_learning_for_turn`).
    #[serde(default = "default_review_interval_turns")]
    pub review_interval_turns: u32,
    #[serde(default = "default_max_learning_candidates")]
    pub max_candidates_per_turn: usize,
    #[serde(default)]
    pub memory_write_approval: bool,
    /// When true (default), staged skills activate only through the manual
    /// approval gate (`MemoryStore::promote` with an approver identity).
    /// When false, the learning review auto-promotes staged skills that
    /// already carry a recorded passed validation — the AGT-012 validation
    /// gate still applies; only the approval step is automated.
    #[serde(default = "default_true")]
    pub skill_write_approval: bool,
    /// When true, a turn that ends with a *verified* satisfied goal yields a
    /// Skill learning candidate. Skill candidates are always staged — they
    /// only become active after a recorded deterministic validation plus
    /// approval (AGT-012).
    #[serde(default = "default_true")]
    pub skill_extraction: bool,
}

impl Default for LearningConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            review_interval_turns: default_review_interval_turns(),
            max_candidates_per_turn: default_max_learning_candidates(),
            memory_write_approval: false,
            skill_write_approval: default_true(),
            skill_extraction: default_true(),
        }
    }
}

fn default_review_interval_turns() -> u32 {
    1
}

fn default_max_learning_candidates() -> usize {
    5
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuardConfig {
    pub immutable_field: bool,
    pub dangerous_command: bool,
    pub repetition: bool,
    pub attack_surface: bool,
    pub evidence_extractor: bool,
    pub skeptic_gate: bool,
    pub failure_tracker: bool,
    pub soft404: bool,
    #[serde(default = "default_true")]
    pub read_state_seeding: bool,
    /// Window size for the repetition guard (number of recent calls to track).
    #[serde(default = "default_repetition_window")]
    pub repetition_window: usize,
    /// Engagement scope. When `allow` is non-empty, the scope guard rejects egress
    /// requests whose target host it can identify as out of scope (see `ScopeGuard`).
    /// This is a heuristic guard, not a hard boundary — see `ScopeConfig`.
    /// Empty = not enforced (warn only).
    #[serde(default)]
    pub scope: ScopeConfig,
}

fn default_repetition_window() -> usize {
    10
}

/// In-scope targeting for an engagement. Checked by `ScopeGuard` over the initial
/// arguments of known egress tools (`http_request`, `web_fetch`, `browser` navigate,
/// `execute_command`, `execute_python`, `spawn_subagent`). When `allow` is non-empty,
/// requests whose extracted host does not match are blocked. This is a HEURISTIC
/// guard, not a hard security boundary (P0-01): redirects, DNS rebinding, unknown or
/// MCP tools, and dynamically constructed commands can bypass it. When `allow` is
/// empty, scope is not enforced (the runtime warns once).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ScopeConfig {
    /// In-scope hosts / domain suffixes / IPs / CIDRs. Non-empty turns on enforcement.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Explicitly out-of-scope entries — checked before `allow`, always blocking.
    #[serde(default)]
    pub deny: Vec<String>,
    /// Permit private / loopback / link-local / cloud-metadata addresses that match an
    /// `allow` entry. Off by default so SSRF-to-internal is blocked even inside scope.
    #[serde(default)]
    pub allow_private: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    /// Path of the long-term memory database. Relative paths resolve against
    /// the Holmes data directory; consumed by CLI session startup.
    pub db_path: String,
    /// Hybrid recall: FTS lexical candidates are fused with local embedding
    /// cosine similarity. When false (or when no embeddings are available),
    /// recall degrades to the pure lexical path.
    #[serde(default = "default_true")]
    pub hybrid_recall: bool,
    /// Bounded wait for recall inside a turn; on timeout the turn continues
    /// without memories instead of blocking.
    #[serde(default = "default_recall_timeout_ms")]
    pub recall_timeout_ms: u64,
    /// Character budget for recalled memory content injected into a turn;
    /// lowest-ranked entries beyond the budget are dropped.
    #[serde(default = "default_recall_max_chars")]
    pub recall_max_chars: usize,
}

fn default_recall_timeout_ms() -> u64 {
    250
}

fn default_recall_max_chars() -> usize {
    1500
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillsConfig {
    pub dir: String,
    pub auto_inject: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConfig {
    pub servers: Vec<McpServerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub transport: McpTransport,
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpTransport {
    Stdio,
    Http,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_content_limit")]
    pub content_limit: usize,
    #[serde(default = "default_timeout")]
    pub timeout: u32,
    #[serde(default)]
    pub proxy: Option<String>,
    #[serde(default = "default_ignore_https")]
    pub ignore_https_errors: bool,
    #[serde(default)]
    pub executable_path: Option<String>,
    #[serde(default)]
    pub extra_launch_args: Vec<String>,
    #[serde(default)]
    pub screenshot_dir: Option<String>,
    /// If set, attach to an already-running Chrome at this CDP endpoint
    /// (e.g. "http://127.0.0.1:9222") instead of launching a new browser.
    /// Attach reuses the user's real profile/login state and defeats strong
    /// anti-bot fingerprinting that would block a launched automation browser.
    #[serde(default)]
    pub cdp_endpoint: Option<String>,
}

fn default_content_limit() -> usize {
    5000
}
fn default_timeout() -> u32 {
    30
}
fn default_ignore_https() -> bool {
    true
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            // On by default: the tool + its prompt guidance register, but Chrome only
            // launches lazily on the first browser action, so idle cost is zero.
            enabled: true,
            content_limit: 5000,
            timeout: 30,
            proxy: None,
            ignore_https_errors: true,
            executable_path: None,
            extra_launch_args: Vec::new(),
            screenshot_dir: None,
            cdp_endpoint: None,
        }
    }
}

impl Default for HolmesConfig {
    fn default() -> Self {
        Self {
            agent: AgentConfig {
                max_iterations: 90,
                generate_reports: true,
            },
            permissions: PermissionConfig::default(),
            llm: LlmConfig {
                providers: vec![],
                roles: RoleConfig {
                    attack_agent: "default".into(),
                    supervisor: String::new(),
                    compressor: String::new(),
                    skill_evolver: String::new(),
                    goal_evaluator: String::new(),
                },
                stream: false,
                thinking_budget: 0,
                provider_cooldown_base_ms: default_provider_cooldown_base_ms(),
                provider_cooldown_max_ms: default_provider_cooldown_max_ms(),
                call_deadline_ms: default_call_deadline_ms(),
            },
            compressor: CompressorConfig {
                enabled: default_true(),
                protect_last_n: default_protect_last_n(),
                target_ratio: default_target_ratio(),
                max_summary_tokens: default_max_summary_tokens(),
                llm_summary: false,
                pre_compact_flush: default_true(),
                context_limit: 128000,
                threshold: 0.75,
                protected_head: 3,
                protected_tail_tokens: 4000,
            },
            learning: LearningConfig::default(),
            subagent: SubagentConfig::default(),
            guards: GuardConfig {
                immutable_field: true,
                dangerous_command: true,
                repetition: true,
                attack_surface: true,
                evidence_extractor: true,
                skeptic_gate: true,
                failure_tracker: true,
                soft404: true,
                read_state_seeding: true,
                repetition_window: 10,
                scope: ScopeConfig::default(),
            },
            memory: MemoryConfig {
                // Relative paths resolve against the Holmes data directory;
                // "memory.db" matches the historical hardcoded location.
                db_path: "memory.db".into(),
                hybrid_recall: true,
                recall_timeout_ms: default_recall_timeout_ms(),
                recall_max_chars: default_recall_max_chars(),
            },
            skills: SkillsConfig {
                dir: "skills".into(),
                auto_inject: true,
            },
            mcp: McpConfig { servers: vec![] },
            browser: BrowserConfig {
                enabled: true,
                content_limit: 5000,
                timeout: 30,
                proxy: None,
                ignore_https_errors: true,
                executable_path: None,
                extra_launch_args: Vec::new(),
                screenshot_dir: None,
                cdp_endpoint: None,
            },
            safety: SafetyConfig::default(),
            hooks: HooksConfig::default(),
            execution: ExecutionConfig::default(),
            supervisor: SupervisorConfig::default(),
            cognition: CognitionConfig::default(),
            ledger: LedgerConfig::default(),
            experiments: ExperimentConfig::default(),
            output_dir: "output".into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Startup configuration diagnostics (P2-01)
// ---------------------------------------------------------------------------

/// What kind of problem a startup config diagnostic reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticKind {
    /// A key the current schema does not know at all — ignored by serde.
    UnknownField,
    /// A key that existed once but was removed because nothing ever consumed
    /// it; still ignored, but the operator should delete it from the file.
    RemovedField,
    /// A value (or combination of values) that is out of range or without
    /// effect; the runtime falls back to the documented behavior.
    InvalidValue,
}

/// A non-fatal startup configuration diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigDiagnostic {
    pub kind: DiagnosticKind,
    /// Dotted config path, e.g. `learning.rule_write_approval`.
    pub path: String,
    pub message: String,
}

/// Top-level sections of `HolmesConfig`.
const TOP_LEVEL_KEYS: &[&str] = &[
    "agent",
    "permissions",
    "llm",
    "compressor",
    "learning",
    "guards",
    "memory",
    "skills",
    "mcp",
    "browser",
    "safety",
    "hooks",
    "execution",
    "supervisor",
    "subagent",
    "cognition",
    "ledger",
    "experiments",
    "output_dir",
];

const AGENT_KEYS: &[&str] = &["max_iterations", "generate_reports"];
const PERMISSIONS_KEYS: &[&str] = &[
    "mode",
    "allowed_tools",
    "disallowed_tools",
    "auto_approve_read_only",
];
const LLM_KEYS: &[&str] = &[
    "providers",
    "roles",
    "stream",
    "thinking_budget",
    "provider_cooldown_base_ms",
    "provider_cooldown_max_ms",
    "call_deadline_ms",
];
const PROVIDER_KEYS: &[&str] = &[
    "name",
    "base_url",
    "api_base", // serde alias of base_url
    "api_key",
    "api_key_env",
    "model",
    "api_format",
    "format", // serde alias of api_format
    "priority",
    "rpm_limit",
];
const ROLE_KEYS: &[&str] = &[
    "attack_agent",
    "supervisor",
    "compressor",
    "skill_evolver",
    "goal_evaluator",
];
const COMPRESSOR_KEYS: &[&str] = &[
    "enabled",
    "protect_last_n",
    "target_ratio",
    "max_summary_tokens",
    "llm_summary",
    "pre_compact_flush",
    "context_limit",
    "threshold",
    "protected_head",
    "protected_tail_tokens",
];
const LEARNING_KEYS: &[&str] = &[
    "enabled",
    "review_interval_turns",
    "max_candidates_per_turn",
    "memory_write_approval",
    "skill_write_approval",
    "skill_extraction",
];
const GUARD_KEYS: &[&str] = &[
    "immutable_field",
    "dangerous_command",
    "repetition",
    "attack_surface",
    "evidence_extractor",
    "skeptic_gate",
    "failure_tracker",
    "soft404",
    "read_state_seeding",
    "repetition_window",
    "scope",
];
const SCOPE_KEYS: &[&str] = &["allow", "deny", "allow_private"];
const MEMORY_KEYS: &[&str] = &[
    "db_path",
    "hybrid_recall",
    "recall_timeout_ms",
    "recall_max_chars",
];
const SKILLS_KEYS: &[&str] = &["dir", "auto_inject"];
const MCP_KEYS: &[&str] = &["servers"];
const MCP_SERVER_KEYS: &[&str] = &["name", "transport", "command", "args", "url"];
const BROWSER_KEYS: &[&str] = &[
    "enabled",
    "content_limit",
    "timeout",
    "proxy",
    "ignore_https_errors",
    "executable_path",
    "extra_launch_args",
    "screenshot_dir",
    "cdp_endpoint",
];
const SAFETY_KEYS: &[&str] = &["egress_rpm"];
const HOOKS_KEYS: &[&str] = &["before_tool", "after_tool", "timeout_ms", "on_failure"];
const HOOK_ENTRY_KEYS: &[&str] = &["matcher", "command", "blocking"];
const EXECUTION_KEYS: &[&str] = &[
    "turn_deadline_ms",
    "tool_deadline_ms",
    "mcp_request_timeout_ms",
];
const SUPERVISOR_KEYS: &[&str] = &[
    "max_repeat_action",
    "stagnation_limit",
    "max_verification_retries",
    "model_verification",
];
const SUBAGENT_KEYS: &[&str] = &[
    "max_depth",
    "max_concurrent",
    "max_tool_calls",
    "max_wall_clock_ms",
];
const COGNITION_KEYS: &[&str] = &[
    "enabled",
    "mode",
    "max_rounds",
    "max_candidates",
    "max_think_tokens",
    "max_think_time_ms",
    "max_rebases",
    "deep_on_finish",
    "deep_on_finding",
    "deep_on_contradiction",
    "deep_on_high_risk_action",
    "deep_on_stagnation",
    "persist_commit_summary",
    "persist_raw_reasoning",
];
const LEDGER_KEYS: &[&str] = &[
    "enabled",
    "max_active_hypotheses_in_context",
    "max_predictions_in_context",
    "max_recent_evidence_in_context",
    "max_contradictions_in_context",
    "snapshot_every_events",
    "semantic_verifier",
    "require_prediction_for_high_priority",
];
const EXPERIMENT_KEYS: &[&str] = &[
    "max_concurrent_per_case",
    "default_lease_ms",
    "heartbeat_ms",
    "max_attempts",
];

/// Keys that once existed but were removed (P2-01) because no production code
/// path ever consumed them. Old configs still load (serde ignores the keys);
/// the diagnostic tells the operator to delete them.
const REMOVED_KEYS: &[(&str, &str)] = &[
    (
        "agent.no_tool_threshold",
        "superseded by `supervisor.stagnation_limit` (AGT-009); it was never read",
    ),
    (
        "agent.hypothesis_budget",
        "no hypothesis budget enforcement exists; it was never read",
    ),
    (
        "agent.stale_threshold",
        "superseded by `supervisor.stagnation_limit` (AGT-009); it was never read",
    ),
    (
        "agent.force_pivot_threshold",
        "superseded by `supervisor.max_repeat_action` (AGT-009); it was never read",
    ),
    (
        "advisor",
        "no advisor subsystem exists; the whole section was never read",
    ),
    (
        "learning.background",
        "the learning review always runs inline at end of turn; it was never read",
    ),
    (
        "learning.rule_write_approval",
        "there is no rule memory category; it was never read",
    ),
    (
        "compressor.preserve_tool_groups",
        "the compactor never grouped tool messages; it was never read",
    ),
    (
        "memory.consolidation_threshold",
        "no consolidation job exists; it was never read",
    ),
    (
        "recon",
        "no auto-recon pipeline exists; the whole section was never read",
    ),
    (
        "browser.headless",
        "the browser always launches headed (v1 design); it was never read",
    ),
    (
        "browser.vision",
        "no vision pipeline consumes it; it was never read",
    ),
    (
        "llm.providers[].max_retries",
        "each provider is attempted at most once per call (failover state machine); it was never read",
    ),
];

fn check_keys(
    diagnostics: &mut Vec<ConfigDiagnostic>,
    section: &serde_yaml::Value,
    prefix: &str,
    known: &[&str],
) {
    let Some(mapping) = section.as_mapping() else {
        return;
    };
    for (key, _) in mapping {
        let Some(key) = key.as_str() else { continue };
        let path = if prefix.is_empty() {
            key.to_string()
        } else {
            format!("{prefix}.{key}")
        };
        if let Some((_, reason)) = REMOVED_KEYS.iter().find(|(removed, _)| *removed == path) {
            diagnostics.push(ConfigDiagnostic {
                kind: DiagnosticKind::RemovedField,
                path: path.clone(),
                message: format!("removed config key: {reason}. It is ignored; delete it."),
            });
        } else if !known.contains(&key) {
            diagnostics.push(ConfigDiagnostic {
                kind: DiagnosticKind::UnknownField,
                path,
                message: "unknown config key; it is ignored.".into(),
            });
        }
    }
}

fn check_sequence_entries(
    diagnostics: &mut Vec<ConfigDiagnostic>,
    section: &serde_yaml::Value,
    list_key: &str,
    prefix: &str,
    known: &[&str],
) {
    let Some(entries) = section.get(list_key).and_then(|v| v.as_sequence()) else {
        return;
    };
    for entry in entries {
        let Some(mapping) = entry.as_mapping() else {
            continue;
        };
        for (key, _) in mapping {
            let Some(key) = key.as_str() else { continue };
            let wildcard_path = format!("{prefix}.{list_key}[].{key}");
            if let Some((_, reason)) = REMOVED_KEYS
                .iter()
                .find(|(removed, _)| *removed == wildcard_path)
            {
                diagnostics.push(ConfigDiagnostic {
                    kind: DiagnosticKind::RemovedField,
                    path: wildcard_path.clone(),
                    message: format!("removed config key: {reason}. It is ignored; delete it."),
                });
            } else if !known.contains(&key) {
                diagnostics.push(ConfigDiagnostic {
                    kind: DiagnosticKind::UnknownField,
                    path: wildcard_path,
                    message: "unknown config key; it is ignored.".into(),
                });
            }
        }
    }
}

/// Inspect the raw config YAML plus the parsed config and return startup
/// diagnostics: unknown keys, removed keys and invalid values/combinations.
/// Everything here is a warning — loading never fails because of these.
pub fn diagnose_config(raw: &serde_yaml::Value, config: &HolmesConfig) -> Vec<ConfigDiagnostic> {
    let mut diagnostics = Vec::new();

    // --- unknown / removed keys -------------------------------------------
    check_keys(&mut diagnostics, raw, "", TOP_LEVEL_KEYS);
    let section = |name: &str| raw.get(name).cloned().unwrap_or(serde_yaml::Value::Null);
    check_keys(&mut diagnostics, &section("agent"), "agent", AGENT_KEYS);
    check_keys(
        &mut diagnostics,
        &section("permissions"),
        "permissions",
        PERMISSIONS_KEYS,
    );
    check_keys(&mut diagnostics, &section("llm"), "llm", LLM_KEYS);
    check_sequence_entries(
        &mut diagnostics,
        &section("llm"),
        "providers",
        "llm",
        PROVIDER_KEYS,
    );
    check_keys(
        &mut diagnostics,
        &section("llm")
            .get("roles")
            .cloned()
            .unwrap_or(serde_yaml::Value::Null),
        "llm.roles",
        ROLE_KEYS,
    );
    check_keys(
        &mut diagnostics,
        &section("compressor"),
        "compressor",
        COMPRESSOR_KEYS,
    );
    check_keys(
        &mut diagnostics,
        &section("learning"),
        "learning",
        LEARNING_KEYS,
    );
    check_keys(&mut diagnostics, &section("guards"), "guards", GUARD_KEYS);
    check_keys(
        &mut diagnostics,
        &section("guards")
            .get("scope")
            .cloned()
            .unwrap_or(serde_yaml::Value::Null),
        "guards.scope",
        SCOPE_KEYS,
    );
    check_keys(&mut diagnostics, &section("memory"), "memory", MEMORY_KEYS);
    check_keys(&mut diagnostics, &section("skills"), "skills", SKILLS_KEYS);
    check_keys(&mut diagnostics, &section("mcp"), "mcp", MCP_KEYS);
    check_sequence_entries(
        &mut diagnostics,
        &section("mcp"),
        "servers",
        "mcp",
        MCP_SERVER_KEYS,
    );
    check_keys(
        &mut diagnostics,
        &section("browser"),
        "browser",
        BROWSER_KEYS,
    );
    check_keys(&mut diagnostics, &section("safety"), "safety", SAFETY_KEYS);
    check_keys(&mut diagnostics, &section("hooks"), "hooks", HOOKS_KEYS);
    check_sequence_entries(
        &mut diagnostics,
        &section("hooks"),
        "before_tool",
        "hooks",
        HOOK_ENTRY_KEYS,
    );
    check_sequence_entries(
        &mut diagnostics,
        &section("hooks"),
        "after_tool",
        "hooks",
        HOOK_ENTRY_KEYS,
    );
    check_keys(
        &mut diagnostics,
        &section("execution"),
        "execution",
        EXECUTION_KEYS,
    );
    check_keys(
        &mut diagnostics,
        &section("supervisor"),
        "supervisor",
        SUPERVISOR_KEYS,
    );
    check_keys(
        &mut diagnostics,
        &section("subagent"),
        "subagent",
        SUBAGENT_KEYS,
    );
    check_keys(
        &mut diagnostics,
        &section("cognition"),
        "cognition",
        COGNITION_KEYS,
    );
    check_keys(&mut diagnostics, &section("ledger"), "ledger", LEDGER_KEYS);
    check_keys(
        &mut diagnostics,
        &section("experiments"),
        "experiments",
        EXPERIMENT_KEYS,
    );

    // --- invalid values / combinations ------------------------------------
    let mut invalid = |path: &str, message: String| {
        diagnostics.push(ConfigDiagnostic {
            kind: DiagnosticKind::InvalidValue,
            path: path.to_string(),
            message,
        });
    };

    if config.agent.max_iterations == 0 {
        invalid(
            "agent.max_iterations",
            "0 is not a usable turn budget; the runtime clamps it to 1.".into(),
        );
    }
    if !(0.0..=1.0).contains(&config.compressor.threshold) || config.compressor.threshold == 0.0 {
        invalid(
            "compressor.threshold",
            format!(
                "threshold {} is outside (0, 1]; compaction triggers at context_limit * threshold.",
                config.compressor.threshold
            ),
        );
    }
    if config.compressor.context_limit == 0 {
        invalid(
            "compressor.context_limit",
            "0 means compaction is considered due on every turn.".into(),
        );
    }
    // Only warn when the operator *explicitly* set pre_compact_flush: the
    // shipped default (true) is silent under the default static compactor.
    let flush_explicitly_on = section("compressor")
        .get("pre_compact_flush")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if flush_explicitly_on && !config.compressor.llm_summary {
        invalid(
            "compressor.pre_compact_flush",
            "has no effect unless compressor.llm_summary is true (static compaction never flushes)."
                .into(),
        );
    }
    if config.llm.providers.is_empty() {
        invalid(
            "llm.providers",
            "no LLM provider configured; every LLM call will fail.".into(),
        );
    } else {
        for (role, name) in [
            ("attack_agent", &config.llm.roles.attack_agent),
            ("supervisor", &config.llm.roles.supervisor),
            ("compressor", &config.llm.roles.compressor),
            ("skill_evolver", &config.llm.roles.skill_evolver),
            ("goal_evaluator", &config.llm.roles.goal_evaluator),
        ] {
            if !name.is_empty() && !config.llm.providers.iter().any(|p| &p.name == name) {
                invalid(
                    "llm.roles",
                    format!(
                        "role `{role}` names provider `{name}`, which is not in llm.providers; \
                         provider selection falls back to priority order."
                    ),
                );
            }
        }
    }
    if config.llm.provider_cooldown_base_ms > config.llm.provider_cooldown_max_ms {
        invalid(
            "llm.provider_cooldown_base_ms",
            format!(
                "base cooldown {}ms exceeds provider_cooldown_max_ms {}ms; the window starts above its cap.",
                config.llm.provider_cooldown_base_ms, config.llm.provider_cooldown_max_ms
            ),
        );
    }
    if config.learning.review_interval_turns == 0 {
        invalid(
            "learning.review_interval_turns",
            "0 is treated as 1 (review every turn).".into(),
        );
    }
    if config.learning.enabled && config.learning.max_candidates_per_turn == 0 {
        invalid(
            "learning.max_candidates_per_turn",
            "0 means the learning review never produces candidates.".into(),
        );
    }
    if matches!(config.safety.egress_rpm, Some(0)) {
        invalid(
            "safety.egress_rpm",
            "0 is treated as unlimited; use null or a positive value.".into(),
        );
    }
    if config.execution.tool_deadline_ms == 0 {
        invalid(
            "execution.tool_deadline_ms",
            "0 gives every tool call an instant deadline; use a positive value.".into(),
        );
    }
    if matches!(config.subagent.max_tool_calls, Some(0)) {
        invalid(
            "subagent.max_tool_calls",
            "0 is treated as unlimited; use null or a positive value.".into(),
        );
    }
    if matches!(config.subagent.max_wall_clock_ms, Some(0)) {
        invalid(
            "subagent.max_wall_clock_ms",
            "0 is treated as no cap; use null or a positive value.".into(),
        );
    }
    if !(1..=3).contains(&config.cognition.max_rounds) {
        invalid(
            "cognition.max_rounds",
            format!(
                "{} is outside the supported 1..=3 total LLM calls; runtime clamps it",
                config.cognition.max_rounds
            ),
        );
    }
    if config.cognition.max_candidates == 0 {
        invalid(
            "cognition.max_candidates",
            "0 leaves the Propose pass unable to return any candidate; runtime clamps it to 1."
                .into(),
        );
    }
    if config.cognition.max_think_time_ms == 0 {
        invalid(
            "cognition.max_think_time_ms",
            "0 gives the cognitive loop no execution time; use a positive value.".into(),
        );
    }
    if config.ledger.snapshot_every_events == 0 {
        invalid(
            "ledger.snapshot_every_events",
            "0 disables the rebuild cache and is not supported; use a positive interval.".into(),
        );
    }
    if config.experiments.max_concurrent_per_case == 0 {
        invalid(
            "experiments.max_concurrent_per_case",
            "0 prevents every Experiment from being claimed; use at least 1.".into(),
        );
    }
    if config.experiments.default_lease_ms == 0 {
        invalid(
            "experiments.default_lease_ms",
            "0 creates an already-expired lease; use a positive duration.".into(),
        );
    }
    if config.experiments.heartbeat_ms == 0
        || config.experiments.heartbeat_ms >= config.experiments.default_lease_ms / 2
    {
        invalid(
            "experiments.heartbeat_ms",
            format!(
                "{} must be positive and less than half of default_lease_ms ({})",
                config.experiments.heartbeat_ms, config.experiments.default_lease_ms
            ),
        );
    }
    if config.experiments.max_attempts == 0 {
        invalid(
            "experiments.max_attempts",
            "0 prevents recovery from acquiring any fenced attempt; use at least 1.".into(),
        );
    }
    if !config.browser.enabled && config.browser.cdp_endpoint.is_some() {
        invalid(
            "browser.cdp_endpoint",
            "has no effect while browser.enabled is false.".into(),
        );
    }

    diagnostics
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn browser_config_serde_round_trip_new_fields() {
        let cfg = BrowserConfig {
            enabled: true,
            content_limit: 7000,
            timeout: 45,
            proxy: Some("http://127.0.0.1:8080".into()),
            ignore_https_errors: true,
            executable_path: Some("/usr/bin/chromium".into()),
            extra_launch_args: vec!["--lang=en".into()],
            screenshot_dir: None,
            cdp_endpoint: None,
        };
        let yaml = serde_json::to_string(&cfg).unwrap();
        let back: BrowserConfig = serde_json::from_str(&yaml).unwrap();
        assert!(back.enabled);
        assert_eq!(back.executable_path.as_deref(), Some("/usr/bin/chromium"));
        assert_eq!(back.extra_launch_args, vec!["--lang=en".to_string()]);
    }

    #[test]
    fn browser_config_defaults_include_new_fields() {
        let cfg = BrowserConfig::default();
        assert!(cfg.enabled); // on by default; Chrome launches lazily on first action
        assert!(cfg.executable_path.is_none());
        assert!(cfg.extra_launch_args.is_empty());
        assert!(cfg.screenshot_dir.is_none());
    }

    #[test]
    fn browser_config_legacy_yaml_without_new_fields_loads() {
        // Legacy configs may still carry the removed `headless`/`vision` keys;
        // serde ignores them (the startup diagnostics flag them as removed).
        let json = r#"{"enabled":false,"headless":true,"vision":false,"content_limit":5000,"timeout":30,"ignore_https_errors":true}"#;
        let cfg: BrowserConfig = serde_json::from_str(json).unwrap();
        assert!(!cfg.enabled);
        assert!(cfg.executable_path.is_none());
        assert!(cfg.extra_launch_args.is_empty());
    }

    #[test]
    fn resolve_attack_model_provider_prefers_override_then_role_provider() {
        let mut config = HolmesConfig::default();
        config.llm.roles.attack_agent = "main".into();
        config.llm.providers.push(ProviderConfig {
            name: "main".into(),
            base_url: "http://localhost".into(),
            api_key: String::new(),
            api_key_env: None,
            model: "role-model".into(),
            api_format: ApiFormat::Anthropic,
            priority: 0,
            rpm_limit: 60,
        });

        let override_resolved = resolve_attack_model_provider(&config, Some("override".into()));
        assert_eq!(
            override_resolved
                .as_ref()
                .map(|resolved| resolved.model.as_str()),
            Some("override")
        );
        assert_eq!(
            override_resolved.and_then(|resolved| resolved.provider),
            None
        );

        let role_resolved = resolve_attack_model_provider(&config, None);
        assert_eq!(
            role_resolved
                .as_ref()
                .map(|resolved| resolved.model.as_str()),
            Some("role-model")
        );
        assert_eq!(
            role_resolved.and_then(|resolved| resolved.provider),
            Some("main".into())
        );
    }

    #[test]
    fn resolve_attack_model_provider_falls_back_to_lowest_priority_provider() {
        let mut config = HolmesConfig::default();
        config.llm.roles.attack_agent = "missing".into();
        config.llm.providers = vec![
            ProviderConfig {
                name: "listed-first".into(),
                base_url: "http://localhost".into(),
                api_key: String::new(),
                api_key_env: None,
                model: "listed-first-model".into(),
                api_format: ApiFormat::Anthropic,
                priority: 20,
                rpm_limit: 60,
            },
            ProviderConfig {
                name: "highest-priority".into(),
                base_url: "http://localhost".into(),
                api_key: String::new(),
                api_key_env: None,
                model: "highest-priority-model".into(),
                api_format: ApiFormat::Anthropic,
                priority: 1,
                rpm_limit: 60,
            },
        ];

        let resolved = resolve_attack_model_provider(&config, None).expect("provider selected");

        assert_eq!(resolved.model, "highest-priority-model");
        assert_eq!(resolved.provider.as_deref(), Some("highest-priority"));
    }

    #[test]
    fn compressor_defaults_enable_static_compaction() {
        let compressor = HolmesConfig::default().compressor;

        assert!(compressor.enabled);
        assert_eq!(compressor.protect_last_n, 20);
        assert_eq!(compressor.target_ratio, 0.25);
        assert_eq!(compressor.max_summary_tokens, 12_000);
    }

    #[test]
    fn learning_defaults_enable_memory_review_with_approval_gates() {
        let learning = HolmesConfig::default().learning;

        assert!(learning.enabled);
        assert_eq!(learning.review_interval_turns, 1);
        assert_eq!(learning.max_candidates_per_turn, 5);
        assert!(!learning.memory_write_approval);
        assert!(learning.skill_write_approval);
        assert!(learning.skill_extraction);
    }

    #[test]
    fn shipped_default_config_parses_without_diagnostics() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config.default.yaml");
        let yaml = std::fs::read_to_string(path).expect("config.default.yaml readable");
        let raw: serde_yaml::Value = serde_yaml::from_str(&yaml).expect("default yaml parses");
        let config: HolmesConfig =
            serde_yaml::from_value(raw.clone()).expect("default yaml deserializes");
        let diagnostics = diagnose_config(&raw, &config);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn raw_cognitive_reasoning_persistence_is_rejected_at_parse_boundary() {
        let error = serde_yaml::from_str::<CognitionConfig>("persist_raw_reasoning: true\n")
            .expect_err("must be forbidden");
        assert!(error.to_string().contains("persist_raw_reasoning=true"));
    }

    #[test]
    fn cognition_round_bound_is_diagnosed() {
        let mut config = HolmesConfig::default();
        config.cognition.max_rounds = 9;
        let raw = serde_yaml::to_value(&config).unwrap();
        let diagnostics = diagnose_config(&raw, &config);
        assert!(diagnostics
            .iter()
            .any(|diagnostic| diagnostic.path == "cognition.max_rounds"));
    }

    #[test]
    fn ledger_snapshot_and_experiment_defaults_are_operational() {
        let config = HolmesConfig::default();
        assert_eq!(config.ledger.snapshot_every_events, 100);
        assert_eq!(config.experiments.max_concurrent_per_case, 4);
        assert_eq!(config.experiments.default_lease_ms, 60_000);
        assert_eq!(config.experiments.heartbeat_ms, 20_000);
        assert_eq!(config.experiments.max_attempts, 3);
    }

    #[test]
    fn invalid_snapshot_and_experiment_lease_policy_is_diagnosed() {
        let mut config = HolmesConfig::default();
        config.ledger.snapshot_every_events = 0;
        config.experiments.max_concurrent_per_case = 0;
        config.experiments.heartbeat_ms = config.experiments.default_lease_ms / 2;
        config.experiments.max_attempts = 0;
        let raw = serde_yaml::to_value(&config).unwrap();
        let diagnostics = diagnose_config(&raw, &config);
        for path in [
            "ledger.snapshot_every_events",
            "experiments.max_concurrent_per_case",
            "experiments.heartbeat_ms",
            "experiments.max_attempts",
        ] {
            assert!(
                diagnostics.iter().any(|diagnostic| diagnostic.path == path),
                "missing diagnostic for {path}: {diagnostics:?}"
            );
        }
    }

    #[test]
    fn permission_defaults_match_interactive_agent_mode() {
        let permissions = HolmesConfig::default().permissions;

        assert_eq!(permissions.mode, PermissionMode::Default);
        assert!(permissions.allowed_tools.is_empty());
        assert!(permissions.disallowed_tools.is_empty());
        assert!(permissions.auto_approve_read_only);
    }

    fn diagnose_yaml(yaml: &str) -> Vec<ConfigDiagnostic> {
        let raw: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        let config: HolmesConfig = serde_yaml::from_value(raw.clone()).unwrap();
        diagnose_config(&raw, &config)
    }

    #[test]
    fn diagnostics_flag_unknown_and_removed_keys() {
        let yaml = r#"
agent:
  max_iterations: 90
  generate_reports: true
  hypothesis_budget: 8      # removed
  mystery_knob: 1           # unknown
advisor:                    # removed section
  enabled: true
learning:
  rule_write_approval: true # removed
llm:
  providers:
    - name: default
      base_url: "https://api.anthropic.com"
      model: "claude-sonnet-4-6"
      max_retries: 3        # removed
  roles:
    attack_agent: default
compressor:
  context_limit: 128000
  threshold: 0.75
  protected_head: 3
  protected_tail_tokens: 4000
guards:
  immutable_field: true
  dangerous_command: true
  repetition: true
  attack_surface: true
  evidence_extractor: true
  skeptic_gate: true
  failure_tracker: true
  soft404: true
memory:
  db_path: data/memory.db
skills:
  dir: skills
  auto_inject: true
mcp:
  servers: []
browser: {}
output_dir: output
"#;
        let diagnostics = diagnose_yaml(yaml);
        let removed: Vec<&str> = diagnostics
            .iter()
            .filter(|d| d.kind == DiagnosticKind::RemovedField)
            .map(|d| d.path.as_str())
            .collect();
        assert!(removed.contains(&"agent.hypothesis_budget"), "{removed:?}");
        assert!(removed.contains(&"advisor"), "{removed:?}");
        assert!(
            removed.contains(&"learning.rule_write_approval"),
            "{removed:?}"
        );
        assert!(
            removed.contains(&"llm.providers[].max_retries"),
            "{removed:?}"
        );
        assert!(diagnostics
            .iter()
            .any(|d| d.kind == DiagnosticKind::UnknownField && d.path == "agent.mystery_knob"));
    }

    #[test]
    fn diagnostics_flag_invalid_combinations() {
        let yaml = r#"
agent:
  max_iterations: 0
  generate_reports: true
llm:
  provider_cooldown_base_ms: 400000
  provider_cooldown_max_ms: 300000
  providers:
    - name: default
      base_url: "https://api.anthropic.com"
      model: "claude-sonnet-4-6"
  roles:
    attack_agent: ghost-provider
compressor:
  context_limit: 128000
  threshold: 1.5
  protected_head: 3
  protected_tail_tokens: 4000
  pre_compact_flush: true
  llm_summary: false
guards:
  immutable_field: true
  dangerous_command: true
  repetition: true
  attack_surface: true
  evidence_extractor: true
  skeptic_gate: true
  failure_tracker: true
  soft404: true
memory:
  db_path: data/memory.db
skills:
  dir: skills
  auto_inject: true
mcp:
  servers: []
browser:
  enabled: false
  cdp_endpoint: "http://127.0.0.1:9222"
safety:
  egress_rpm: 0
output_dir: output
"#;
        let diagnostics = diagnose_yaml(yaml);
        let paths: Vec<&str> = diagnostics
            .iter()
            .filter(|d| d.kind == DiagnosticKind::InvalidValue)
            .map(|d| d.path.as_str())
            .collect();
        for expected in [
            "agent.max_iterations",
            "compressor.threshold",
            "compressor.pre_compact_flush",
            "llm.roles",
            "llm.provider_cooldown_base_ms",
            "safety.egress_rpm",
            "browser.cdp_endpoint",
        ] {
            assert!(paths.contains(&expected), "missing {expected}: {paths:?}");
        }
    }

    #[test]
    fn diagnostics_are_quiet_for_a_clean_minimal_config() {
        let yaml = r#"
agent:
  max_iterations: 90
  generate_reports: true
llm:
  providers:
    - name: default
      base_url: "https://api.anthropic.com"
      model: "claude-sonnet-4-6"
  roles:
    attack_agent: default
compressor:
  context_limit: 128000
  threshold: 0.75
  protected_head: 3
  protected_tail_tokens: 4000
guards:
  immutable_field: true
  dangerous_command: true
  repetition: true
  attack_surface: true
  evidence_extractor: true
  skeptic_gate: true
  failure_tracker: true
  soft404: true
memory:
  db_path: data/memory.db
skills:
  dir: skills
  auto_inject: true
mcp:
  servers: []
browser: {}
output_dir: output
"#;
        let diagnostics = diagnose_yaml(yaml);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn legacy_config_without_removed_fields_still_loads() {
        // Configs written before the cleanup carry removed keys; they must
        // still deserialize (serde ignores them) and only warn.
        let yaml = r#"
agent:
  max_iterations: 90
  no_tool_threshold: 3
  hypothesis_budget: 8
  stale_threshold: 8
  force_pivot_threshold: 15
  generate_reports: true
recon:
  auto_run: false
  nmap_top_ports: 100
learning:
  background: true
  rule_write_approval: true
memory:
  db_path: data/memory.db
  consolidation_threshold: 0.85
llm:
  providers:
    - name: default
      base_url: "https://api.anthropic.com"
      model: "claude-sonnet-4-6"
  roles:
    attack_agent: default
compressor:
  context_limit: 128000
  threshold: 0.75
  protected_head: 3
  protected_tail_tokens: 4000
  preserve_tool_groups: true
guards:
  immutable_field: true
  dangerous_command: true
  repetition: true
  attack_surface: true
  evidence_extractor: true
  skeptic_gate: true
  failure_tracker: true
  soft404: true
skills:
  dir: skills
  auto_inject: true
mcp:
  servers: []
browser:
  headless: false
  vision: false
output_dir: output
"#;
        let config: HolmesConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.agent.max_iterations, 90);
        let diagnostics = diagnose_yaml(yaml);
        assert!(diagnostics
            .iter()
            .all(|d| d.kind == DiagnosticKind::RemovedField));
        assert!(diagnostics.len() >= 10, "{diagnostics:?}");
    }
}
