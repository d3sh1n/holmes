# Holmes Runtime Phase 1 Implementation Plan

## Goal

Implement Phase 1 of the approved Holmes runtime design: add a unified AI-native runtime path and route normal chat plus one-shot queries through it. The result should make Holmes behave less like a scattered set of loops and more like a real-time collaborative agent: user input becomes an event immediately, tool calls flow through one action gateway, tool results and blocked actions are recorded, Mind Palace is updated during the turn, and CLI receives runtime yields as work happens.

This plan covers Phase 1 only. Browser, MCP, `/goal`, long-term memory behavior, structured `HolmesDecision`, and full resume reconstruction remain Phase 2/3 work unless they are needed as seams for the runtime.

## Current Context

- The approved spec lives at `docs/superpowers/specs/2026-06-20-holmes-runtime-design.md`.
- The current REPL and one-shot path call `run_selector_loop` in `crates/holmes-cli/src/chat.rs`.
- `run_selector_loop` delegates to workflows in `crates/holmes-cli/src/workflows.rs`.
- `crates/holmes-cli/src/agent_loop.rs` contains a more complete tool/event/post-guard loop, but it is not the primary path.
- `holmes-guards` post guards can extract attack surface, evidence, findings, failure signals, and soft-404 baselines, but the main workflow path does not run post guards.
- `MindPalace` updates only from events it is fed. The main workflow path does not produce enough structured events for it.
- Tests currently pass with `cargo test --workspace --quiet`.
- `cargo clippy --workspace --all-targets -- -D warnings` currently fails on existing `holmes-core` style lints. Do not make clippy a blocking gate for this implementation unless separately fixing that lint debt.

## Assumptions

- Holmes should stay AI-native. Phase 1 should not focus on adding hard target restrictions.
- Existing guard behavior can remain, but guard output should be recorded and surfaced as feedback.
- Phase 1 can keep existing LLM tool-call protocol rather than introducing JSON decisions.
- Existing slash commands should keep working. Commands that currently stub features, such as `/goal`, can remain stubs in Phase 1.
- Existing workflows and selector tests should keep passing. The old workflow loop can remain temporarily, but normal chat and one-shot should move to the new runtime.

## Proposed Approach

Create a new crate, `crates/holmes-runtime`, with a small but real runtime engine:

- `AgentRuntime` owns turn execution.
- `RuntimeContext` holds session, DB, Mind Palace, tools, guards, LLM backend, config, and runtime state.
- `RuntimeYield` and `RuntimeSink` let CLI display real-time progress.
- `PerceptionEngine` builds transient prompt context without permanently polluting the message history.
- `DeliberationEngine` calls the LLM using current tool definitions.
- `ActionEngine` is the only tool execution gateway in the new runtime.
- `EvidenceEngine` projects post-guard state into event-sourced evidence.
- `ReflectionEngine` handles simple Phase 1 stop conditions.
- `DialogueEngine` formats yields for user-facing progress.

Then update `holmes-cli` so normal REPL messages and `holmes -q` use `AgentRuntime`.

## Step-by-Step Plan

### 1. Add the runtime crate

Create:

- `crates/holmes-runtime/Cargo.toml`
- `crates/holmes-runtime/src/lib.rs`
- `crates/holmes-runtime/src/runtime.rs`
- `crates/holmes-runtime/src/context.rs`
- `crates/holmes-runtime/src/yield_stream.rs`
- `crates/holmes-runtime/src/perception.rs`
- `crates/holmes-runtime/src/deliberation.rs`
- `crates/holmes-runtime/src/action.rs`
- `crates/holmes-runtime/src/evidence.rs`
- `crates/holmes-runtime/src/reflection.rs`
- `crates/holmes-runtime/src/dialogue.rs`

Because the workspace uses `members = ["crates/*"]`, adding this crate should include it automatically.

Dependencies for `holmes-runtime`:

- `holmes-core`
- `holmes-session`
- `holmes-mind-palace`
- `holmes-llm`
- `holmes-tools`
- `holmes-guards`
- workspace `tokio`, `async-trait`, `anyhow`, `thiserror`, `serde`, `serde_json`, `chrono`, `tracing`

### 2. Define runtime output

In `yield_stream.rs`, add:

- `RuntimeYield`
- `RuntimeSink`
- `VecSink` for tests

Suggested variants:

- `MessageToUser { content }`
- `PlanUpdate { summary }`
- `ToolStarted { name, arguments_summary }`
- `ToolFinished { name, success, summary }`
- `EvidenceUpdate { summary }`
- `NeedsUserInput { question, context }`
- `FinalAnswer { content }`
- `Error { message }`

Keep the type plain and serializable if convenient, but do not block Phase 1 on serialization.

### 3. Define runtime context and state

In `context.rs`, add:

- `RuntimeContext`
- `RuntimeState`
- `InteractionMode`
- `RuntimePhase`
- `UserTurnInput`
- `TurnOutcome`

`RuntimeContext` should include:

- `RuntimeSession`
- `session_id`
- `Arc<SessionDB>`
- `Arc<MemoryStore>`
- `MindPalace`
- LLM backend
- `Arc<ToolRegistry>`
- `Arc<Mutex<GuardChain>>`
- runtime state
- `HolmesConfig`

For Phase 1, include a compatibility tool state for guards/post-guards. This can wrap the existing `AttackState`, but do not expose it as the identity of the runtime. Initialize it permissively enough that Holmes remains AI-native; Phase 1 is not a target boundary project.

Also include dedup tracking for evidence projection, for example:

- seen ports
- seen tech strings
- seen endpoints
- seen credentials
- seen findings

### 4. Add an LLM test seam

Add a local runtime abstraction so tests do not call real providers.

Preferred option:

```rust
#[async_trait]
pub trait LlmBackend: Send + Sync {
    async fn chat_completion(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        role: &str,
    ) -> anyhow::Result<LlmResponse>;
}
```

Implement it for `holmes_llm::client::LlmClient`.

`RuntimeContext` can hold `Arc<dyn LlmBackend>`. CLI wraps its existing `Arc<LlmClient>` as that trait object.

Add a `MockLlmBackend` in tests that can return:

- a final answer
- a tool call followed by a final answer
- an error

### 5. Build PerceptionEngine

In `perception.rs`, implement:

- `PerceptionFrame`
- `PerceptionEngine::build(&RuntimeContext) -> PerceptionFrame`
- `PerceptionFrame::to_messages(&RuntimeSession) -> Vec<Message>`

Phase 1 frame contents:

- transient situation summary from `MindPalace`
- current session mode and interaction mode
- recent observations from `RuntimeState`
- recent failure count

Important: do not permanently push repeated `[当前态势]` messages into `RuntimeSession.messages`. Build prompt messages transiently for the LLM call.

### 6. Build DeliberationEngine

In `deliberation.rs`, implement:

- `DeliberationEngine::decide(context, frame) -> Result<LlmResponse, RuntimeError>`

Behavior:

- call `LlmBackend::chat_completion`
- pass `ToolRegistry::definitions()`
- map missing provider / no healthy provider to a friendly `RuntimeErrorKind::NeedsUser`
- preserve response usage so `AgentRuntime` can update token counters

Keep the existing tool-call format. Do not introduce `HolmesDecision` in Phase 1.

### 7. Build ActionEngine

In `action.rs`, implement:

- `ActionEngine::execute_call(context, call, sink) -> ToolResult`
- `ActionEngine::execute_calls(context, calls, sink) -> Vec<ToolResult>`

Use sequential execution in Phase 1. Do not use `ToolRegistry::can_parallelize` yet, because post-guards and state updates need a single ordered path.

For each call:

1. emit `ToolStarted`
2. append `Event::ToolCall`
3. ingest event into Mind Palace
4. run pre-guards
5. if blocked:
   - append `Event::ToolBlocked`
   - ingest event
   - emit `ToolFinished` or `Error` with blocked summary
   - return `ToolResult::blocked`
6. execute tool through `ToolRegistry`
7. append a concise action-history entry to the compatibility state for `SkepticGate`
8. run post-guards
9. append `Event::ToolResult`
10. ingest event
11. emit `ToolFinished`

Use `ToolResult::text_content()` for summaries, truncated with `holmes_core::truncate_str`.

### 8. Build EvidenceEngine

In `evidence.rs`, implement:

- `EvidenceEngine::project(context, sink) -> Result<Vec<Event>, RuntimeError>`

Phase 1 projections:

- new ports/tech/endpoints from `AttackState.attack_surface()`
- new credentials from `AttackState.evidence_bundle()`
- accepted findings from `AttackState.findings()`

Emit and persist:

- `Event::AttackSurfaceUpdate` when new attack-surface facts appear
- `Event::CredentialFound` for new plaintext credentials
- `Event::VulnerabilityFound` for findings that are confirmed or suitable to report

Each projected event must:

- append to `SessionDB`
- ingest into `MindPalace`
- emit `EvidenceUpdate` with a short summary
- update dedup sets

Do not try to perfectly model all event fields in Phase 1. Use conservative defaults where existing post-guard state lacks data.

### 9. Build ReflectionEngine

In `reflection.rs`, implement simple Phase 1 checks:

- max iterations reached
- no tool calls means final answer
- missing provider means needs user
- guard block returns feedback to LLM if the LLM still has budget

Do not implement deep hypothesis or stagnation logic yet. Leave those for Phase 2.

### 10. Build DialogueEngine

In `dialogue.rs`, implement helpers to format runtime-facing details into concise user-facing strings:

- tool started message
- tool finished summary
- evidence update summary
- missing provider message
- final answer message

Keep output short. The CLI can decide exact styling later.

### 11. Implement AgentRuntime

In `runtime.rs`, implement:

- `AgentRuntime::new(context) -> Self`
- `run_turn`
- `run_oneshot`

`run_turn` flow:

1. append `Event::UserMessage`
2. ingest event
3. push `Message::user` into `RuntimeSession.messages`
4. loop until final answer, needs-user, fatal error, or iteration budget
5. build `PerceptionFrame`
6. call `DeliberationEngine`
7. update token counters in `RuntimeSession`
8. call `SessionDB::update_token_counts`
9. push assistant message into `RuntimeSession.messages`
10. append `Event::Thinking` for non-empty assistant content
11. if no tool calls:
    - emit `FinalAnswer`
    - return `TurnOutcome`
12. if tool calls:
    - execute through `ActionEngine`
    - push tool result messages into `RuntimeSession.messages`
    - project evidence through `EvidenceEngine`
    - continue loop

`run_oneshot` can call `run_turn` and return the final outcome.

### 12. Update CLI dependencies and construction

In `crates/holmes-cli/Cargo.toml`, add:

- `holmes-runtime = { path = "../holmes-runtime" }`

In `crates/holmes-cli/src/chat.rs`:

- add a helper to build `RuntimeContext`
- add a `CliRuntimeSink` that prints `RuntimeYield`
- update the one-shot path to call `AgentRuntime::run_oneshot`
- update normal REPL message handling to call `AgentRuntime::run_turn`

Keep slash commands as they are for Phase 1, except make sure commands that inspect `ctx.runtime_session` still see the updated runtime session after each turn.

Avoid deleting `run_selector_loop` in the first implementation pass. It can remain for `/workflow` or as a compatibility path until tests are adjusted.

### 13. Preserve session resume behavior for Phase 1

Do not fully solve resume in Phase 1. Keep current resume behavior unless runtime construction requires small adjustments.

If touched, preserve the existing behavior:

- replay events into Mind Palace
- rebuild runtime messages from user messages

Add a migration note in comments if necessary, but do not broaden the implementation to full assistant/tool replay.

### 14. Improve missing-provider UX

When `config.llm.providers` is empty or LLM selection fails with no healthy provider, emit a friendly message:

```text
Holmes: I do not have a configured LLM provider yet. Run `holmes setup` or edit the config file before starting an investigation.
```

Return a non-panicking `TurnOutcome` or a `RuntimeErrorKind::NeedsUser` that CLI displays cleanly.

This should replace raw one-shot output like `Error: LLM error: no healthy LLM provider available`.

### 15. Add tests

Runtime tests in `crates/holmes-runtime/src/...`:

- final answer: mock LLM returns content only
- tool call: mock LLM returns one tool call, then final answer
- blocked call: mock LLM calls a command blocked by guard, assert `ToolBlocked`
- evidence projection: mock tool output contains nmap-like ports or credentials, assert projected events
- yield order: assert `ToolStarted` occurs before `ToolFinished`

CLI-level test if practical:

- missing provider one-shot returns friendly message without trying a real LLM

If CLI process tests are too heavy, cover the missing-provider mapping at runtime level and leave full CLI smoke to manual validation.

## Files Likely to Change

New files:

- `crates/holmes-runtime/Cargo.toml`
- `crates/holmes-runtime/src/lib.rs`
- `crates/holmes-runtime/src/runtime.rs`
- `crates/holmes-runtime/src/context.rs`
- `crates/holmes-runtime/src/yield_stream.rs`
- `crates/holmes-runtime/src/perception.rs`
- `crates/holmes-runtime/src/deliberation.rs`
- `crates/holmes-runtime/src/action.rs`
- `crates/holmes-runtime/src/evidence.rs`
- `crates/holmes-runtime/src/reflection.rs`
- `crates/holmes-runtime/src/dialogue.rs`

Modified files:

- `crates/holmes-cli/Cargo.toml`
- `crates/holmes-cli/src/chat.rs`
- optionally `crates/holmes-cli/src/agent_loop.rs` if turning it into a compatibility wrapper
- optionally `crates/holmes-cli/src/workflows.rs` only if imports or comments need adjustment

Test files:

- runtime module tests colocated in `crates/holmes-runtime/src/*.rs`
- optional `crates/holmes-runtime/tests/runtime_tests.rs`
- optional `crates/holmes-cli/tests/runtime_cli.rs`

## Validation

Required:

```bash
cargo fmt --check
cargo test --workspace --quiet
cargo run -q -p holmes-cli -- version
cargo run -q -p holmes-cli -- --help
```

Manual smoke:

```bash
tmp=$(mktemp -d)
HOME="$tmp" ./target/debug/holmes -q "hello"
rm -rf "$tmp"
```

Expected missing-provider behavior: friendly setup guidance, not a raw provider error.

Optional:

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

Do not treat clippy as blocking until the existing `holmes-core` clippy debt is fixed.

## Risks and Mitigations

Risk: creating `holmes-runtime` introduces circular dependencies.
Mitigation: keep runtime depending on existing crates; do not make existing lower-level crates depend on runtime.

Risk: CLI state diverges from runtime state.
Mitigation: after each runtime turn, write the updated `RuntimeContext` session and Mind Palace back into `ChatContext`.

Risk: evidence projection duplicates events every turn.
Mitigation: keep dedup sets in `RuntimeState` and test projection idempotence.

Risk: tests require real LLM providers.
Mitigation: introduce `LlmBackend` and use mocks for runtime tests.

Risk: Phase 1 becomes too broad.
Mitigation: do not implement Browser/MCP/Goal, full resume replay, or JSON decisions in this phase.

Risk: existing slash commands assume the old loop.
Mitigation: leave command handlers mostly unchanged and route only normal messages plus one-shot through runtime in the first pass.

## Open Questions

1. Should `RuntimeContext` own `MindPalace` directly, or should CLI keep ownership and pass a mutable reference per turn?
   Recommendation: runtime owns it for the turn and CLI stores the returned context afterward.

2. Should `ToolResult` messages use `to_message()` or `to_message_with_vision()`?
   Recommendation: use `to_message()` in Phase 1 because Browser/Vision is Phase 3.

3. Should Phase 1 delete `agent_loop.rs`?
   Recommendation: no. First migrate behavior into runtime and leave a compatibility wrapper or dead-code cleanup for a later pass.

4. Should Phase 1 fix `GuardChain::from_config` boolean flags?
   Recommendation: no, unless it is trivial while touching construction. The Phase 1 goal is runtime unification, not guard configuration semantics.

## Completion Criteria

Phase 1 is complete when:

- `holmes-runtime` exists and compiles.
- Normal REPL messages use `AgentRuntime::run_turn`.
- `holmes -q` uses `AgentRuntime::run_oneshot`.
- Tool calls in runtime produce `ToolCall` and `ToolResult` events.
- Guard-blocked calls produce `ToolBlocked` events.
- Post-guard state can project at least one structured evidence event.
- Mind Palace ingests runtime events during the turn.
- Runtime emits user-visible yields.
- Missing provider produces friendly setup guidance.
- Existing tests pass.
- New runtime tests cover the main event and yield flow.

