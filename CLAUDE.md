# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
# Build
cargo build                                  # debug
cargo build --release                        # produces ./target/release/holmes
cargo check --workspace                      # fast type-check across all crates

# Test
cargo test --workspace                       # full suite
cargo test -p holmes-runtime                 # one crate
cargo test -p holmes-harness scenario        # tests in crates/holmes-harness/tests/scenario.rs
cargo test -p holmes-cli slash_commands::    # filter by test path
cargo test <name> -- --nocapture             # see println / tracing output

# Run
./target/release/holmes                      # full-screen TUI (default)
./target/release/holmes repl                 # legacy Reedline REPL
./target/release/holmes -q "question"        # one-shot, non-interactive
./target/release/holmes -c                   # continue most recent session
./target/release/holmes -r <session-id>      # resume specific session
./target/release/holmes setup                # interactive provider/model wizard
```

LLM: only the Anthropic Messages wire protocol is implemented (`/v1/messages`). Any Anthropic-compatible gateway works by adding it as another entry under `llm.providers`. `ANTHROPIC_API_KEY` and `HOLMES_API_KEY` are auto-detected during `setup`. Default config lives at `~/.local/share/holmes/config.yaml`; `config.default.yaml` at the repo root is the reference template.

Harness scenarios (deterministic LLM replay tests) live in `scenarios/*.yaml` and are driven by `crates/holmes-harness/tests/scenario.rs` — add a YAML, the runner picks it up.

## Architecture

### Crate dependency direction

`holmes-core` is the base layer (types, `Event`, `Config`, `RuntimeSession`, `AgentHook` trait, `SubagentRunner` trait, the four-zone `AttackState` in `state/`). Everything else depends on it. The runtime layer (`holmes-runtime`) sits above the storage/LLM/tool/guard/mind-palace crates and is consumed by `holmes-cli` and `holmes-harness`. **The runtime is the single agent loop** — there is no separate `agent_loop.rs` anywhere (a prior one was removed); both interactive turns and one-shot queries go through `holmes_runtime::runtime::AgentRuntime`.

Note: `holmes-runtime/src/lib.rs` re-exports most submodules but **not `runtime` itself**. Callers must write `holmes_runtime::runtime::AgentRuntime` (this trips up `use` statements).

### What one turn actually does

The docs' "LLM → Permission → Guard → Tool → Guard → Yield" picture is incomplete. The real `AgentRuntime::run_turn` orchestrates 10 engines per iteration:

```
on_session_start (middleware, fires every turn despite the name)
record UserMessage event
memory.recall_for_turn         ← top-3 semantic recall into context
loop:
  reflection.assess_iteration_budget   ← max-iterations gate
  maybe_compact                        ← at most once per turn; emits CompactionBoundary
  perception.perceive                  ← builds a frame from RuntimeContext
  deliberation.decide                  ← LLM call → ParsedDecision
  middleware.on_token_usage / record assistant response
  match HolmesDecision:
    Answer | Finish     → learning.review_turn; emit FinalAnswer; return
    AskWatson           → emit NeedsUserInput; return NeedsUser
    UseTools            → action.execute_batch (Permission → AgentHook.pre → middleware.before_tool_call → tool → middleware.after_tool_call → AgentHook.post)
                          → evidence.project → deduction.review_tool_results → memory.remember_observations
    SetGoal | Reflect | Deduce → record event + emit PlanUpdate (stay in loop)
  middleware.after_step
```

`HolmesDecision` has **seven** variants, not two. `SetGoal`/`Reflect`/`Deduce` are first-class loop-internal outcomes, not terminal states. `TurnOutcome` exits with `FinalAnswer`, `NeedsUser`, or `MaxIterationsReached`.

### Three independent safety layers (don't confuse them)

| Layer | Where | What it does |
|---|---|---|
| `PermissionPolicy` (`holmes-runtime/src/permissions.rs`) | Per tool call, inside `action.execute_batch` | User-facing authorization. Modes: `default / plan / read_only / accept_edits / dont_ask / bypass`. Allow/deny glob lists. |
| `GuardChain` (`holmes-guards`) | Pre-tool (block) and post-tool (extract into `AttackState`) | System-level. **`SkepticGate` is the only writer to the validated zone** of `AttackState`. PreGuards: `immutable_field`, `dangerous_command`, `repetition`, `file_tracker`. PostGuards: `attack_surface`, `evidence_extractor`, `skeptic_gate`, `failure_tracker`, `soft404`, `file_tracker`. |
| `RuntimeMiddleware` (`holmes-runtime/src/middleware.rs`, newer) | Cross-cutting: `on_session_start / before_step / after_step / before_tool_call / after_tool_call / before_event_persist / on_token_usage / on_final_answer` | Built-ins: `GuardMiddleware` (command blocklist), `SensitiveDataRedactMiddleware` (regex redaction across content blocks, events, and final answers), `TokenAuditMiddleware` (budget). |

Live caveats:
- `GuardMiddleware` checks `args.get("CommandLine")` (PascalCase) — the actual `execute_command` tool uses lowercase keys; this guard is currently a no-op for real calls.
- `AgentRuntime::new` auto-installs a `CheckpointHook` into `action.hooks`; it's not configurable from outside.
- `AgentRuntime` itself does **not** install a `GuardChain::from_config` — only the CLI bootstrap (`chat.rs`, `subagent.rs`, `tui.rs`) does. Constructing `AgentRuntime` directly (e.g. in custom harness code) without first wiring guards silently disables all PostGuard data flow: `attack_surface`, `evidence_bundle`, and the `findings` validated zone remain empty, and `evidence.project` produces no observations. The harness `runner.rs` and `RuntimeContext::permissive_attack_state` both bypass this on purpose for tests, but it's a sharp edge.

### State partitioning — `holmes-core/src/state/`

`AttackState` is split into four explicit zones with disciplined writers:

- **Immutable** (`immutable.rs`) — target URL/IP, challenge metadata. Constructed once via `pub(crate) new`, no setters anywhere.
- **Tool truth** (`tool_truth.rs`) — `AttackSurface`, `EvidenceBundle`, credentials, object refs. Written **only by PostGuards** from raw tool output.
- **Validated** (`validated.rs`) — `Finding`s with `FindingConfidence`. The blessed writer is `AttackState::record_finding(finding)`, which `SkepticGate` uses; production code outside SkepticGate should not call it. `findings_mut()` still exists (raw) but is doc-marked as test-fixture / SkepticGate-only — calling it from new production code breaks the validated-zone contract.
- **Free** (`builders.rs`) — phase, scratch fields, aggregator. Mutable everywhere.

If you find a writer to the wrong zone, that is a real bug — the comments call this out as a design invariant.

### Event sourcing model

Every state change is an immutable `Event` (40+ variants in `holmes-core/src/event.rs`) appended to `SessionDB` (SQLite + FTS5 + WAL). Sessions reconstruct by replaying events; **`MindPalace::from_events` is the canonical rehydrate path**. Forking is a transactional event copy at a chosen index. Token deltas are accumulated separately via `update_token_counts`.

`holmes-session/src/store.rs` defines the `SessionStore` async trait (14 methods); `SessionDB` implements it. Downstream code (`MindPalace`, harness `runner.rs`, subagent runner) takes `Arc<dyn SessionStore>` — prefer the trait when introducing new consumers so tests can substitute an in-memory store.

When a `ToolResult.content` exceeds 10 000 chars, `SessionDB::append_event` **offloads it to disk** at `sessions/<session-id>/tool-results/call_<uuid>.txt` and rewrites the in-event content to a pointer. Downstream readers must tolerate both inline and pointer-form content.

### Mind Palace (three layers, one type)

`holmes-mind-palace::MindPalace` composes:

- **Memory layer** — raw `session_events: Vec<Event>` + long-term `MemoryStore` (FTS5). Replays via `SessionStore::get_events`.
- **Context layer** — typed working summaries: attack surface, vulnerabilities, code patterns, reverse insights, credentials, compromised hosts, lateral movements, topology, directive, hypothesis, reflections, current phase, **pitfall summaries** (rendered as "避坑经验 / Historical Pitfalls" in the LLM prompt, capped at 20).
- **Dashboard layer** — stateless. `DashboardLayer::generate(&context, mode)` projects the context into a `DashboardSnapshot` shaped by `SessionMode` (Pentest / SecurityResearch / CodeAudit / Reverse / Mixed). Pentest sections are rendered in Chinese.

`MindPalace::ingest(event)` is the single ingestion point — every `Event` flowing through the runtime is mirrored into the palace.

### Compaction

`CaseCompactor::plan` decides when to compact based on `compressor.threshold` over `context_limit`, preserving `protected_head` messages at the front and `protected_tail_tokens` tokens at the tail. The middle is summarized and a `PitfallSummary` is extracted. **The result method is hardcoded to `CompressionMethod::StaticFallback`** — there is no LLM-summary path live, despite the enum suggesting one. `AgentRuntime::compact_now` exposes a manual entry point.

### Subagents

`SpawnSubagentTool` is registered only when a `SubagentRunner` is supplied. `holmes-cli/src/subagent.rs::CliSubagentRunner` mints a `sub-<uuid>` `RuntimeSession`, builds a **fresh** `MindPalace` and `GuardChain` from the parent's config but **shares the parent's `session_db`, `memory_store`, and `LlmClient`**. The captured final answer (from a `RuntimeYield::FinalAnswer`) becomes `SubAgentResult.summary`. Subagents are not isolated processes — they share the same SQLite store, so their events land in the same DB tagged under their sub-session id.

### CLI surfaces

`holmes` with no args starts the **full-screen TUI** (`tui.rs`, crossterm-based, alternate screen + raw mode). The legacy Reedline REPL (`chat.rs`) is reached via `holmes repl`, `--repl`, or `-q`. Both surfaces consume the same `ChatContext` / `run_runtime_input_with_sink` from `chat.rs` — `chat.rs` is now the shared "session + runtime plumbing" library, not just the REPL. When changing the agent loop or runtime wiring, expect both surfaces to be affected.

35 slash commands are registered in `commands.rs::CommandRegistry` across nine categories; `all_command_hints` powers tab-completion in Reedline and the `Ctrl+L` command palette in the TUI.

### LLM client

`holmes-llm::LlmClient` is multi-provider over a single wire format (Anthropic Messages). `FailoverChain` tracks per-endpoint health (`consecutive_failures`, `last_failure`); `RateLimiter` enforces per-provider RPM; `RoleAssignment` routes calls by role (`attack_agent / supervisor / compressor / skill_evolver / goal_evaluator`) to a chosen provider. Errors are classified as retryable/rate-limit/permanent in `error_classifier.rs`; retries use exponential backoff. To add a different vendor, implement an adapter that translates to/from `AnthropicRequest`/`AnthropicResponse` (see `anthropic.rs`) — do not introduce a new wire format alongside.

### Harness

Add a scenario YAML to `scenarios/`, point it at scripted LLM responses and mocked tools, declare expectations, and `cargo test -p holmes-harness` exercises it through a real `AgentRuntime` with an in-memory `SessionDB` and `ScriptedLlmBackend`. Transcripts are written to `crates/holmes-harness/sessions/<uuid>/transcript.jsonl` (gitignored as artifacts). Use this in preference to mocking the runtime when adding deterministic regression tests for agent behavior.

### Bundled Pentest methodology (Pentest mode default)

`skills/pentest-lyan/` ships with Holmes as a bundled skill (Pentest-Lyan v2.3, MIT, from `HeaSec/Pentest-Lyan`). It is wired into the default session in two layers:

1. **Hardcoded methodology** — `project_knowledge.rs::PENTEST_METHODOLOGY` (the精炼 essence: three phases, 12-dimension threat model, 9 Banned Patterns, `coverage_note` 三问) is injected into the system prompt by `build_system_prompt(..., mode)` **only when `mode == SessionMode::Pentest`** (the default mode). Other modes are not polluted.
2. **On-demand details** — The full `SKILL.md`, `gates.md`, `references/*.md`, `schema/*.json` stay on disk; the existing skill auto-discovery (`skills.dir = "skills"`) indexes them, and the prompt points the model to `Read` specific references when it needs depth.

When changing `SYSTEM_PROMPT` or `PENTEST_METHODOLOGY` in `chat.rs`/`project_knowledge.rs`, remember the methodology is a **default operating standard, not a slash command** — the model is expected to apply Banned Patterns (e.g. "200 ≠ vulnerability", no hardcoded victim IDs in cross-role tests, `not_vulnerable` must list `unruled_out`) without being asked. Tests `pentest_mode_injects_pentest_methodology` and `non_pentest_mode_skips_pentest_methodology` lock in the per-mode behavior.
