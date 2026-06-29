# Session Semantic Kernel Design

Date: 2026-06-29
Status: Approved design, pending implementation plan

## Summary

Holmes should adopt Pi's strongest session-semantics idea without copying Pi's JSONL tree architecture: a new Holmes session must be replayable from its event stream into a complete runtime context, not just a message list. The Session Semantic Kernel v1 adds metadata events, branch summaries, compaction-as-event with external archives, a replay API, and one-shot overflow compaction retry.

This is scoped to the session core. It deliberately does not add RPC mode, steer/follow-up/next-turn queues, a TypeScript extension runtime, or automatic migration for old sessions.

## Goals

- Make new sessions replayable from SQLite events into a complete `RuntimeSession` and runtime semantic context.
- Persist startup and later changes to system prompt, mode, model, and active tools as events.
- Preserve branch context with `BranchSummary` events when forking or moving onto a new branch.
- Replace destructive message compaction with `CompactionApplied` events plus external archives.
- Automatically recover from context-overflow LLM errors by compacting once and retrying the same decision once.
- Keep Holmes' existing SQLite, `Event`, `SessionDB`, `MindPalace`, and `AgentRuntime` architecture.

## Non-goals

- Rewriting Holmes storage into Pi-style JSONL session trees.
- Migrating old sessions to the new semantic event format.
- Adding RPC/headless mode.
- Adding Pi-style steer/follow-up/next-turn queues.
- Adding a dynamic extension runtime.
- Replacing Holmes' security-specific state model, guards, or Mind Palace.

## Compatibility Policy

Only new sessions are required to be semantically complete. Old sessions that lack the new startup metadata events are considered legacy or incomplete. They may continue to use the existing resume path, but the new replay API should report `semantic_complete = false` or return a clear incomplete-session marker.

The implementation must not silently pretend old sessions are fully replayable. If the CLI resumes a legacy session, it should use the legacy path or show a warning such as: "This session was created before semantic replay metadata existed; resume will use legacy defaults."

## Architecture

The new layer is a Session Semantic Kernel built on top of the existing event stream.

1. New sessions append startup semantic events immediately after `SessionCreated`.
2. Runtime changes append semantic events at the mutation point, for example `/model` appends `SessionModelSet`.
3. Forks append a `BranchSummary` event to the new branch.
4. Compactions write archived raw context to disk and append a `CompactionApplied` event.
5. Replay folds events in order into `ReplayedSessionContext`.

This preserves the current SQLite event store while making replay semantics explicit and testable.

## Event Model

### `SessionSystemPromptSet`

Records the actual system prompt used by the session.

Fields:

- `prompt_hash: String`
- `content: String`
- `source: String` — values such as `startup`, `fork`, `manual`
- `timestamp: DateTime<Utc>`

The prompt hash helps compare prompts without scanning large strings. The content is stored because replay must not depend on whatever the current binary would generate today.

### `SessionModelSet`

Records the active model and optional provider.

Fields:

- `model: String`
- `provider: Option<String>`
- `source: String` — values such as `startup`, `slash_command`, `resume_override`
- `timestamp: DateTime<Utc>`

### `SessionModeSet`

If an existing `SessionModeSet` event already exists, extend it rather than creating a duplicate concept.

Fields:

- `mode: SessionMode`
- `source: String`
- `timestamp: DateTime<Utc>`

### `ActiveToolsSet`

Records which tool names were visible to the agent at a point in time.

Fields:

- `tool_names: Vec<String>`
- `source: String` — values such as `startup`, `mcp_reload`, `manual`
- `timestamp: DateTime<Utc>`

This event stores tool names, not complete tool schemas or implementations. Replay uses it for semantics and audit. Runtime still loads actual implementations from the current `ToolRegistry` and MCP configuration.

### `BranchSummary`

Records a semantic bridge when creating a new branch.

Fields:

- `from_event_index: u64`
- `to_event_index: u64`
- `summary: String`
- `reason: String`
- `method: SummaryMethod`
- `timestamp: DateTime<Utc>`

`SummaryMethod` should include at least `Llm` and `StaticFallback`.

A branch summary is not evidence and does not create findings. It is context that helps the new branch remember why the previous path matters.

### `CompactionApplied`

Upgrades the current compaction event into an event-sourced compaction record.

Fields:

- `before_message_count: usize`
- `after_message_count: usize`
- `summary: String`
- `preserved_head: usize`
- `preserved_tail_tokens: usize`
- `archive_path: Option<String>`
- `archived_event_range: Option<(u64, u64)>`
- `method: CompressionMethod`
- `trigger: CompactionTrigger`
- `timestamp: DateTime<Utc>`

`CompressionMethod` should distinguish `Llm` and `StaticFallback`. `CompactionTrigger` should include `Manual`, `Threshold`, and `Overflow`.

## Replay API

Add an explicit replay output type:

```rust
pub struct ReplayedSessionContext {
    pub session: RuntimeSession,
    pub system_prompt: Option<String>,
    pub model: Option<String>,
    pub active_tools: Vec<String>,
    pub compactions: Vec<CompactionReplayMarker>,
    pub branch_summaries: Vec<String>,
    pub semantic_complete: bool,
}
```

`CompactionReplayMarker` should include the compaction event index, summary, archive path, and archived range.

Add a replay method, preferably on `SessionStore` if it can be represented generically, otherwise first on `SessionDB`:

```rust
async fn replay_session_context(&self, session_id: &str) -> Result<ReplayedSessionContext>;
```

Replay algorithm:

1. Read stored events in order.
2. Fold ordinary message events into `RuntimeSession.messages` as today.
3. On `SessionSystemPromptSet`, update replay system prompt and ensure the runtime session uses that prompt.
4. On `SessionModelSet`, update replay model.
5. On `SessionModeSet`, update replay mode.
6. On `ActiveToolsSet`, update replay active tool names.
7. On `BranchSummary`, add a summary/system message to the replayed context and record it in `branch_summaries`.
8. On `CompactionApplied`, replace the compacted range in replayed messages with a compaction summary message and record a `CompactionReplayMarker`.
9. Return `semantic_complete = true` only if required startup metadata events were present.

Required startup metadata for semantic completeness:

- `SessionCreated`
- `SessionSystemPromptSet`
- `SessionModeSet`
- `SessionModelSet`
- `ActiveToolsSet`

## New Session Flow

Centralize new-session initialization in one helper used by REPL, TUI, one-shot, and subagents. The helper should:

1. Create the session record.
2. Append `SessionCreated` if not already appended by the existing path.
3. Build the system prompt.
4. Append `SessionSystemPromptSet`.
5. Append `SessionModeSet`.
6. Append `SessionModelSet`.
7. Build/register tools and append `ActiveToolsSet`.
8. Construct `RuntimeSession` from the same semantic values.

The implementation should avoid separate TUI and REPL paths writing different event sets.

## Runtime Mutation Flow

### `/model`

When the user changes model:

1. Update current context.
2. Append `SessionModelSet { source: "slash_command" }`.
3. Future replay uses the last model event.

### `/mode`

When the user changes mode:

1. Update current context/session mode.
2. Append `SessionModeSet { source: "slash_command" }`.
3. Future replay uses the last mode event.

### Active tools and MCP reload

When active tools change, append `ActiveToolsSet`. If replayed active tools are not currently available, resume should warn but not fail. Tool availability is an execution concern, not a replay blocker.

## Branch Summary Flow

Trigger points:

- `/branch` and `/fork`
- `/tree fork <event_index>`
- TUI event timeline fork
- Future Pi-style leaf movement, if implemented

Generation flow:

1. Select the event window being left or copied into the new branch.
2. Try LLM summary first.
3. If LLM summary fails, produce a static fallback from user messages, tool calls, findings, errors, and the last assistant answer.
4. Append `BranchSummary` to the new branch session.
5. Replay injects the summary as a context/system message.

The summary prompt should ask for:

- What happened on the abandoned or parent path.
- Key evidence.
- Failed paths and why they failed.
- Pitfalls not to repeat.
- Confirmed and unconfirmed findings.
- Useful next steps.

Target length: roughly 500-1200 characters.

## Compaction Archive Flow

Compaction should stop being only an in-memory destructive rewrite. The flow should be:

1. Determine the compacted event/message range.
2. Generate a summary using LLM-first, static-fallback behavior.
3. Write an archive file.
4. Append `CompactionApplied` with summary and archive pointer.
5. Update in-memory `session.messages` for the current process.
6. Future replay applies the compaction event instead of recomputing compaction.

Archive path:

```text
sessions/<session-id>/compactions/compaction_<event-index>.json
```

Archive JSON structure:

```json
{
  "session_id": "...",
  "compaction_event_index": 123,
  "trigger": "overflow",
  "archived_event_range": [12, 87],
  "messages": [],
  "events": [],
  "created_at": "..."
}
```

The archive includes both messages and events. Messages enable context restoration; events preserve audit detail such as tool calls, permissions, evidence updates, and deduction events.

If archive writing fails, the implementation must not append `CompactionApplied`.

## Overflow Retry Flow

At the LLM call boundary, detect context overflow using the existing error classification path or a new `RuntimeErrorKind` if necessary.

Algorithm:

1. Call LLM for the current decision.
2. If the error is not context overflow, return it as today.
3. If it is context overflow and this iteration has not retried:
   - run compaction with `trigger = Overflow`
   - write archive
   - append `CompactionApplied`
   - retry the same decision once
4. If retry succeeds, continue normally.
5. If retry fails or compaction fails, return a clear error including the retry/compaction failure reason.

No loop is allowed. Retry at most once per decision.

## Error Handling

- LLM summary generation fails: use static fallback and mark method `StaticFallback`.
- Static fallback fails: return a recoverable runtime error.
- Archive write fails: do not append compaction event; return an error for manual compaction, or return the original overflow error with archive failure detail for overflow compaction.
- Event append fails: do not mutate in-memory session as if it were persisted; return a recoverable runtime error.
- Replay sees missing archive file: do not block resume; inject the summary from the event and emit/log a warning that raw archived history is unavailable.
- Replay sees unknown future semantic event: ignore the event for replay while preserving it in storage.
- Replay sees incomplete metadata: return `semantic_complete = false` or an explicit incomplete-session error, depending on the caller.

## Testing Strategy

### Unit tests

- Event serialization/deserialization for all new variants.
- `Event::content_text()` and `Event::category()` for new variants.
- Startup metadata completeness detection.
- Replay of a complete new session.
- Replay of an incomplete legacy session.
- Branch summary LLM success path.
- Branch summary static fallback path.
- Compaction archive write/read.
- Missing archive during replay.
- Overflow retry runs at most once.

### Integration and harness tests

- A long-compression scenario resumes and contains the compaction summary message.
- A simulated context-overflow response triggers compaction and then successfully retries once.
- A forked session contains `BranchSummary` in replayed context.
- `/model` and `/mode` changes persist through replay.
- MCP/tool reload or active tool changes persist as `ActiveToolsSet`.

## Implementation Boundaries

The implementation should avoid broad rewrites. Work should land in small layers:

1. Event types and serialization.
2. Session initialization helper and metadata writes.
3. Replay API.
4. Branch summary service.
5. Compaction archive service.
6. Overflow retry wiring.
7. CLI/TUI resume/fork integration.
8. Tests and docs.

## Risks and Mitigations

### Risk: Event model duplicates existing fields

Mitigation: keep old table/session fields for listing and compatibility, but make new-session replay depend on events. Avoid removing old columns in this phase.

### Risk: Summary generation adds latency

Mitigation: use LLM-first only at fork/compaction boundaries, not every turn. Static fallback prevents blocking when providers fail.

### Risk: Archive files drift from SQLite events

Mitigation: append `CompactionApplied` only after archive write succeeds. Missing archives during replay produce warnings and use the event summary.

### Risk: Active tools replay cannot recreate old tool implementations

Mitigation: treat `ActiveToolsSet` as semantic/audit metadata. Runtime loads tools from current registry and warns on mismatch.

### Risk: Old sessions behave differently

Mitigation: no migration. Mark old sessions incomplete and use legacy resume or warnings.

## Open Implementation Decisions

These are implementation choices, not product ambiguities:

- Whether `replay_session_context` belongs first on `SessionDB` or directly on `SessionStore`.
- Whether compaction archive files store fully serialized `StoredEvent`s or a narrower event archive DTO.
- Whether `SessionSystemPromptSet.content` should be compressed if it becomes very large.
- Exact wording for replay warning yields in TUI/REPL.

The design is complete without resolving these now; the implementation plan should choose one option for each.
