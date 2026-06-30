# Holmes Runtime Design

Date: 2026-06-20
Status: Approved for planning
Scope: AI-native runtime architecture for Holmes

## Purpose

Holmes should become a real-time collaborative security research agent, not a pure automation script and not a simple CLI wrapper around LLM tool calls. The runtime must make Holmes worthy of the name: observant, evidence-driven, conversational, able to reason about hypotheses, able to pause and ask Watson for judgment, and able to preserve a complete case record.

The project has many of the right pieces: `RuntimeSession`, event sourcing, `MindPalace`, `ToolRegistry`, `GuardChain`, LLM providers, workflows, setup, and slash commands. This design originally targeted the split between the workflow loop and the older `agent_loop.rs`; Phase 1 has since converged normal chat and one-shot queries on `holmes-runtime::AgentRuntime`, and the legacy CLI loop has been removed. Browser, MCP, goal mode, long-term memory recall, and structured evidence are still areas that need continued hardening inside the unified runtime.

This design introduces a new `holmes-runtime` crate as the single engine used by CLI chat, one-shot queries, future goal mode, and future sub-agents.

## Design Principles

1. Real-time collaboration first.
   Holmes should talk with Watson while it works: explain observations, expose plans, stream tool progress, ask for input at pause points, and summarize evidence as it appears.

2. AI-native orchestration.
   The runtime should give the LLM a structured cognitive environment instead of relying on hardcoded procedural flows. Modes such as chat, recon, analysis, exploit, report, audit, and reverse should become runtime intent profiles, not separate tool loops.

3. Event-first case record.
   User input, assistant reasoning summaries, tool calls, tool results, blocked actions, evidence updates, reflections, and reports should be recorded as events as they happen.

4. Mind Palace as working memory.
   `MindPalace` should not be just a dashboard projection. It should be consulted every turn and updated throughout the turn, giving Holmes a stable sense of current situation, active hypotheses, evidence, failures, and context.

5. Guards as feedback, not personality.
   Guard output should become part of Holmes' observation stream. A blocked tool call should be recorded and explained so Holmes can adjust strategy. The design does not aim to make Holmes powerful by adding more hard constraints.

6. Incremental migration.
   The architecture is a major upgrade, but implementation should land in phases so the CLI remains usable and tests stay green.

## Non-Goals

Phase 1 does not implement a full autonomous goal system, browser control, MCP tool execution, sub-agent orchestration, or a full JSON decision protocol. It creates the runtime foundation that those features will use.

Phase 1 does not remove all existing workflow code. Existing workflow and selector tests should keep passing while normal chat and one-shot paths begin moving to the new runtime.

## Interaction Model

Holmes supports three interaction modes:

```rust
pub enum InteractionMode {
    Conversational,
    AssistedAutonomy,
    GoalDriven,
}
```

`Conversational` is the default. Holmes acts like a real-time collaborator: it answers, plans, asks questions, and uses tools when useful.

`AssistedAutonomy` allows Holmes to take several steps in sequence, but it still emits progress, observations, and pause points.

`GoalDriven` is reserved for explicit goal mode. It can run longer loops, but it must still support progress updates, interruption, and user feedback.

Runtime output should be streamed through yields:

```rust
pub enum RuntimeYield {
    MessageToUser { content: String },
    PlanUpdate { summary: String },
    ToolStarted { name: String, arguments_summary: String },
    ToolFinished { name: String, success: bool, summary: String },
    EvidenceUpdate { summary: String },
    NeedsUserInput { question: String, context: String },
    FinalAnswer { content: String },
    Error { message: String },
}
```

The CLI can initially print these synchronously. A future TUI, web UI, or socket transport can consume the same stream.

## Crate Boundary

Add a new crate:

```text
crates/holmes-runtime/
  Cargo.toml
  src/lib.rs
  src/runtime.rs
  src/context.rs
  src/yield_stream.rs
  src/perception.rs
  src/deliberation.rs
  src/action.rs
  src/evidence.rs
  src/reflection.rs
  src/dialogue.rs
```

`holmes-cli` remains responsible for CLI parsing, REPL, slash commands, setup, and display. It should not own the agent loop.

`holmes-runtime` owns the cognitive loop and depends on:

- `holmes-core`
- `holmes-session`
- `holmes-mind-palace`
- `holmes-llm`
- `holmes-tools`
- `holmes-guards`

## Core Runtime Types

```rust
pub struct AgentRuntime {
    context: RuntimeContext,
    perception: PerceptionEngine,
    deliberation: DeliberationEngine,
    action: ActionEngine,
    evidence: EvidenceEngine,
    reflection: ReflectionEngine,
    dialogue: DialogueEngine,
}
```

Primary methods:

```rust
impl AgentRuntime {
    pub async fn run_turn(
        &mut self,
        input: UserTurnInput,
        sink: &mut dyn RuntimeSink,
    ) -> Result<TurnOutcome, RuntimeError>;

    pub async fn run_oneshot(
        &mut self,
        input: String,
        sink: &mut dyn RuntimeSink,
    ) -> Result<TurnOutcome, RuntimeError>;
}
```

`RuntimeContext`:

```rust
pub struct RuntimeContext {
    pub session: RuntimeSession,
    pub session_id: String,
    pub session_db: Arc<SessionDB>,
    pub memory_store: Arc<MemoryStore>,
    pub mind_palace: MindPalace,
    pub llm: Arc<LlmClient>,
    pub tools: Arc<ToolRegistry>,
    pub guards: Arc<Mutex<GuardChain>>,
    pub state: RuntimeState,
    pub config: HolmesConfig,
}
```

`RuntimeState` should not be named `AttackState`, because Holmes also handles code audit, reverse engineering, and general security research. Phase 1 may wrap or mirror existing `AttackState` internally for compatibility.

```rust
pub struct RuntimeState {
    pub interaction_mode: InteractionMode,
    pub session_mode: SessionMode,
    pub phase: RuntimePhase,
    pub active_hypotheses: Vec<String>,
    pub observations: Vec<String>,
    pub failures: u32,
}
```

## Engine Responsibilities

### PerceptionEngine

Builds a `PerceptionFrame` for each turn. It answers: what does Holmes currently know?

Inputs:

- recent messages
- recent events
- Mind Palace situation summary
- active hypotheses
- recent tool outcomes
- failure or repetition signals
- recalled long-term memories when available
- interaction mode
- session mode

Phase 1 may use a text prompt frame. Later phases can move toward a structured frame.

### DeliberationEngine

Calls the LLM and interprets its response. In Phase 1, this stays compatible with existing LLM tool calling. Later phases can introduce a structured decision protocol:

```rust
pub enum HolmesDecision {
    Answer { message: String },
    AskWatson { question: String, options: Vec<String>, reason: String },
    UseTools { rationale: String, calls: Vec<ToolCall> },
    SetHypothesis { hypothesis: String },
    SwitchMode { mode: SessionMode, reason: String },
    Reflect { diagnosis: String, next_strategy: String },
    Finish { summary: String },
}
```

### ActionEngine

The single gateway for tools.

Execution order:

1. Emit `RuntimeYield::ToolStarted`.
2. Append `Event::ToolCall`.
3. Run pre-guards.
4. If blocked, append `Event::ToolBlocked`, emit feedback, and return a blocked result.
5. Execute the tool.
6. Run post-guards.
7. Append `Event::ToolResult`.
8. Emit `RuntimeYield::ToolFinished`.

This replaces the historical split between `workflows.rs::run_agent_turn` and `agent_loop.rs`; the current primary path is `AgentRuntime`.

### EvidenceEngine

Projects tool results and post-guard state into event-sourced evidence.

Phase 1 should adapt existing post-guard state rather than rewriting extraction from scratch:

- `AttackState.attack_surface()` -> `Event::AttackSurfaceUpdate`
- `EvidenceBundle.credentials` -> `Event::CredentialFound`
- `Finding` entries -> `Event::VulnerabilityFound` when confidence warrants it

The engine must deduplicate evidence events so repeated turns do not flood the event stream.

### ReflectionEngine

Evaluates progress after each tool batch or final answer:

- Is the user question answered?
- Is there enough evidence?
- Is Holmes stuck?
- Should Holmes ask Watson for input?
- Should Holmes change investigation route?
- Should Holmes summarize and stop?

Phase 1 may implement simple heuristics. Phase 2 should make reflection richer and LLM-assisted.

### DialogueEngine

Turns internal runtime state into user-facing conversation.

It is responsible for concise, real-time updates:

- what Holmes thinks it is doing
- why a tool is being used
- what changed after a tool result
- what evidence was found
- why it needs Watson
- what it recommends next

This is what makes Holmes feel like a collaborator rather than an automation script.

## Turn Flow

One conversational turn:

```text
Watson input
  -> append Event::UserMessage
  -> emit MessageToUser when useful
  -> PerceptionEngine builds PerceptionFrame
  -> DeliberationEngine calls LLM
  -> assistant message is appended to runtime messages
  -> if final answer: append Thinking/response event and emit FinalAnswer
  -> if tool calls: ActionEngine executes each call
  -> EvidenceEngine projects new facts into events
  -> MindPalace ingests all new events
  -> ReflectionEngine decides continue / ask user / finish
  -> DialogueEngine emits user-facing summary
```

The runtime may loop within one turn until one of these stop conditions:

- LLM gives a final answer.
- The runtime reaches the configured iteration budget.
- Holmes emits `NeedsUserInput`.
- A fatal runtime error occurs.
- The interaction mode requires pausing after an evidence or plan update.

## Phase Plan

### Phase 1: Runtime Foundation

Goal: create a unified runtime and route normal chat and one-shot through it.

Implementation scope:

- Add `holmes-runtime`.
- Keep the legacy `agent_loop.rs` removed; new behavior belongs in runtime components.
- Use `ActionEngine` as the only tool execution path for new runtime flows.
- Make normal REPL messages call `AgentRuntime::run_turn`.
- Make `holmes -q` call `AgentRuntime::run_oneshot`.
- Emit `RuntimeYield` values to CLI.
- Append tool call, tool result, blocked action, and user message events as they happen.
- Update `MindPalace` during the turn.
- Project basic evidence events from existing post-guard state.
- Keep current LLM tool calling format.
- Keep existing workflows and selector code available for tests and migration.

Phase 1 success criteria:

- A one-shot query uses the new runtime.
- A REPL message uses the new runtime.
- Tool calls produce `ToolCall` and `ToolResult` events.
- Blocked calls produce `ToolBlocked` events and user-visible feedback.
- Mind Palace receives newly generated events.
- Dashboard can begin reflecting projected evidence events.
- Existing tests pass.
- New runtime tests cover event order, yield order, guard blocked calls, and evidence projection.

### Phase 2: AI-Native Cognition

Goal: make Holmes more clearly behave like Holmes.

Implementation scope:

- Introduce `PerceptionFrame`.
- Add richer `DialogueEngine` output.
- Add `HolmesDecision` support or a compatible structured decision protocol.
- Add active hypotheses as first-class runtime state.
- Add reflection-driven pause points.
- Add stagnation detection and strategy switching.
- Add `NeedsUserInput` behavior for ambiguous, risky, or evidence-poor situations.

Phase 2 success criteria:

- Holmes can state active hypotheses.
- Holmes can explain evidence for and against a route.
- Holmes pauses for Watson when the investigation branches.
- Holmes reflects after repeated failure instead of looping blindly.

### Phase 3: Capability Expansion

Goal: route all major existing capabilities through the runtime.

Implementation scope:

- Register `BrowserTool` when `browser.enabled` is true.
- Connect `McpToolProvider` to runtime tool discovery and execution.
- Implement `/goal` using `InteractionMode::GoalDriven`.
- Resume sessions with full assistant/tool context, not only user messages.
- Use long-term memory recall and remember in `PerceptionEngine` and `MemoryEngine`.
- Move report generation toward structured evidence.

Phase 3 success criteria:

- Browser, MCP, and goal mode all use the same runtime path.
- Resume reconstructs enough context for Holmes to continue naturally.
- Long-term memory influences future turns.
- Reports are grounded in event-sourced evidence.

## Error Handling

Errors should be classified by runtime behavior:

```rust
pub enum RuntimeErrorKind {
    Recoverable,
    NeedsUser,
    Fatal,
}
```

Recoverable errors include tool failures, guard feedback, selector uncertainty, and parse fallback. They should be recorded and explained when relevant.

Needs-user errors include missing provider config, missing model, ambiguous user intent, or missing context required to proceed.

Fatal errors include database open failure, invalid config syntax, unrecoverable LLM provider failure, or internal runtime invariant failure.

Current missing-provider behavior should become a friendly user-facing runtime error:

```text
Holmes: I do not have a configured LLM provider yet. Run `holmes setup` or edit the config file before starting an investigation.
```

## Testing Strategy

Phase 1 requires test seams that avoid real LLM calls.

Add one of these abstractions:

- `LlmBackend` trait implemented by `LlmClient` and `MockLlmBackend`, or
- a runtime-level `DeliberationEngine` trait that can be mocked.

Required tests:

1. Runtime final answer test.
   Mock LLM returns text only. Assert `UserMessage` and final yield are produced.

2. Runtime tool call test.
   Mock LLM returns a tool call. Mock tool returns success. Assert yield order and event order.

3. Guard blocked test.
   Tool call is blocked. Assert `ToolBlocked` event and `ToolFinished` or `Error` yield are emitted.

4. Evidence projection test.
   Tool output contains recognizable attack surface or credential evidence. Assert projected events and Mind Palace dashboard update.

5. Missing provider CLI smoke test.
   No provider configured. Assert user-friendly message rather than a raw `no healthy LLM provider` error.

6. Regression test.
   Existing workspace tests continue to pass.

## Migration Notes

`crates/holmes-cli/src/agent_loop.rs` has been removed. It should not be reintroduced as a second primary loop; new execution behavior belongs in `holmes-runtime`.

`crates/holmes-cli/src/workflows.rs` can remain for selector tests and compatibility, but normal chat and one-shot should keep using `AgentRuntime`.

`GuardChain::from_config` now respects boolean config flags. The next step is to keep those controls understandable in the TUI and documented for users.

`http_request` read-only semantics should be revisited later. The runtime design can handle this by classifying tool effects and exposing them to deliberation.

## Open Choices Already Resolved

The chosen approach is the aggressive runtime architecture. The project should not merely patch the existing workflow loop. The runtime must preserve real-time conversation as a first-class behavior.

The implementation should begin with Phase 1, because all other capabilities need a single runtime path before they can become reliable.
