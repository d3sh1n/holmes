# Session Semantic Kernel Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make new Holmes sessions replayable from SQLite events into complete runtime context, including startup metadata, branch summaries, event-sourced compaction archives, and one-shot overflow recovery.

**Architecture:** Add a semantic replay layer on the existing SQLite event store instead of replacing it with Pi-style JSONL trees. New events capture system prompt, model, active tools, branch summaries, and archive-backed compactions; `SessionStore::replay_session_context` folds them into `RuntimeSession` and replay metadata. Runtime and CLI paths write semantic events at creation, mutation, fork, and compaction boundaries.

**Tech Stack:** Rust workspace, `serde`, `chrono`, `tokio`, `rusqlite`, `async-trait`, existing `holmes-core`, `holmes-session`, `holmes-runtime`, `holmes-cli`, `holmes-harness` crates.

---

## Scope Check

The approved spec covers one cohesive subsystem: session semantic replay. It touches core events, session storage, runtime compaction/error handling, and CLI/TUI session flows, but all changes serve one outcome: a new session can be replayed from events into complete runtime context. This plan keeps it as one implementation sequence with frequent testable commits.

## File Structure

### Create

- `crates/holmes-session/src/replay.rs` — `ReplayedSessionContext`, `CompactionReplayMarker`, replay fold logic, and helper tests for metadata completeness.
- `crates/holmes-session/src/compaction_archive.rs` — archive DTOs and archive path helper logic that `SessionDB` uses.
- `crates/holmes-runtime/src/summary.rs` — branch/compaction summary service with LLM-first and static fallback behavior.

### Modify

- `crates/holmes-core/src/event.rs` — add semantic event variants, `SummaryMethod`, `CompactionTrigger`, event text/category/type behavior.
- `crates/holmes-session/src/lib.rs` — export new `replay` and `compaction_archive` modules.
- `crates/holmes-session/src/store.rs` — add `replay_session_context`, `write_compaction_archive`, and `session_workspace` trait methods.
- `crates/holmes-session/src/db.rs` — implement new trait methods; update `event_type_str`; ensure session workspace dirs include `compactions/`.
- `crates/holmes-session/tests/session_tests.rs` — integration tests for startup metadata replay and compaction archive round trip.
- `crates/holmes-runtime/src/compaction.rs` — include compacted range and trigger-aware output fields.
- `crates/holmes-runtime/src/runtime.rs` — write archive-backed compaction events; add overflow compact+retry path; emit replay warnings through yields.
- `crates/holmes-runtime/src/deliberation.rs` — classify context overflow errors.
- `crates/holmes-runtime/src/lib.rs` — export `summary` module.
- `crates/holmes-cli/src/chat.rs` — centralize fresh session semantic initialization, use replay API for new sessions, append mutation events for `/mode` and `/model`, branch summary integration.
- `crates/holmes-cli/src/tui.rs` — ensure TUI session/fork helpers use the same chat helpers after they are centralized.
- `crates/holmes-cli/src/subagent.rs` — write startup metadata for subagent sessions.
- `crates/holmes-harness/src/runner.rs` — write startup metadata for harness-created sessions or mark harness sessions intentionally legacy in tests.
- `CLAUDE.md` — document the new session semantic replay contract.

---

## Task 0: Prepare an Isolated Worktree

**Files:**
- No source files.

- [ ] **Step 1: Create a feature branch or worktree before implementation**

Run from `/Users/sh1n/PWC/holmes`:

```bash
git status --short
git switch -c feat/session-semantic-kernel
```

Expected: new branch is created. If the working tree has user changes that must not move branches, use a sibling worktree instead:

```bash
git worktree add ../holmes-session-semantic-kernel -b feat/session-semantic-kernel
cd ../holmes-session-semantic-kernel
```

- [ ] **Step 2: Confirm baseline tests compile**

Run:

```bash
cargo check --workspace
```

Expected: command exits 0 before code changes begin.

---

## Task 1: Add Semantic Event Types

**Files:**
- Modify: `crates/holmes-core/src/event.rs:8-356`
- Test: `crates/holmes-core/src/event.rs` unit tests module to add at file bottom if absent

- [ ] **Step 1: Write failing tests for new event serialization and categories**

Add this test module near the bottom of `crates/holmes-core/src/event.rs` after the `impl Event` block:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_events_have_text_and_categories() {
        let now = Utc::now();
        let events = vec![
            Event::SessionSystemPromptSet {
                prompt_hash: "hash123".into(),
                content: "system prompt content".into(),
                source: "startup".into(),
                timestamp: now,
            },
            Event::SessionModelSet {
                model: "claude-sonnet-4-6".into(),
                provider: Some("default".into()),
                source: "startup".into(),
                timestamp: now,
            },
            Event::ActiveToolsSet {
                tool_names: vec!["http_request".into(), "report_finding".into()],
                source: "startup".into(),
                timestamp: now,
            },
            Event::BranchSummary {
                from_event_index: 1,
                to_event_index: 4,
                summary: "branch path found idor evidence".into(),
                reason: "fork".into(),
                method: SummaryMethod::StaticFallback,
                timestamp: now,
            },
            Event::CompressionApplied {
                before_count: 10,
                after_count: 4,
                summary: "compacted auth investigation".into(),
                preserved_keys: vec!["system_prompt".into()],
                method: CompressionMethod::StaticFallback,
                preserved_head: Some(2),
                preserved_tail_tokens: Some(4000),
                archive_path: Some("sessions/s/compactions/compaction_7.json".into()),
                archived_event_range: Some((2, 7)),
                trigger: Some(CompactionTrigger::Manual),
                timestamp: Some(now),
            },
        ];

        assert_eq!(events[0].category(), "session");
        assert!(events[0].content_text().contains("system prompt content"));
        assert_eq!(events[1].category(), "session");
        assert!(events[1].content_text().contains("claude-sonnet-4-6"));
        assert_eq!(events[2].category(), "session");
        assert!(events[2].content_text().contains("http_request"));
        assert_eq!(events[3].category(), "context");
        assert!(events[3].content_text().contains("idor evidence"));
        assert_eq!(events[4].category(), "context");
        assert!(events[4].content_text().contains("auth investigation"));

        for event in events {
            let encoded = serde_json::to_string(&event).expect("serialize event");
            let decoded: Event = serde_json::from_str(&encoded).expect("deserialize event");
            assert_eq!(decoded.category(), event.category());
        }
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run:

```bash
cargo test -p holmes-core semantic_events_have_text_and_categories
```

Expected: FAIL with missing variants such as `SessionSystemPromptSet`, `SessionModelSet`, `ActiveToolsSet`, `BranchSummary`, `SummaryMethod`, or `CompactionTrigger`.

- [ ] **Step 3: Add the semantic event variants and supporting enums**

Modify `Event` near the session lifecycle and context management sections:

```rust
    SessionModeSet {
        mode: SessionMode,
        #[serde(default)]
        source: Option<String>,
        #[serde(default)]
        timestamp: Option<DateTime<Utc>>,
    },
    SessionSystemPromptSet {
        prompt_hash: String,
        content: String,
        source: String,
        timestamp: DateTime<Utc>,
    },
    SessionModelSet {
        model: String,
        provider: Option<String>,
        source: String,
        timestamp: DateTime<Utc>,
    },
    ActiveToolsSet {
        tool_names: Vec<String>,
        source: String,
        timestamp: DateTime<Utc>,
    },
```

Replace the existing `CompressionApplied` variant with backward-compatible optional fields:

```rust
    CompressionApplied {
        before_count: usize,
        after_count: usize,
        summary: String,
        preserved_keys: Vec<String>,
        method: CompressionMethod,
        #[serde(default)]
        preserved_head: Option<usize>,
        #[serde(default)]
        preserved_tail_tokens: Option<usize>,
        #[serde(default)]
        archive_path: Option<String>,
        #[serde(default)]
        archived_event_range: Option<(u64, u64)>,
        #[serde(default)]
        trigger: Option<CompactionTrigger>,
        #[serde(default)]
        timestamp: Option<DateTime<Utc>>,
    },
    BranchSummary {
        from_event_index: u64,
        to_event_index: u64,
        summary: String,
        reason: String,
        method: SummaryMethod,
        timestamp: DateTime<Utc>,
    },
```

Add supporting enums near `CompressionMethod`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SummaryMethod {
    Llm,
    StaticFallback,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompactionTrigger {
    Manual,
    Threshold,
    Overflow,
}
```

- [ ] **Step 4: Update `content_text` and `category` matches**

In `Event::content_text`, add these match arms before `_ => String::new()`:

```rust
            Event::SessionSystemPromptSet { content, source, .. } => {
                format!("system_prompt source={} {}", source, content)
            }
            Event::SessionModelSet {
                model,
                provider,
                source,
                ..
            } => format!(
                "model source={} provider={} {}",
                source,
                provider.as_deref().unwrap_or(""),
                model
            ),
            Event::ActiveToolsSet {
                tool_names, source, ..
            } => format!("active_tools source={} {}", source, tool_names.join(" ")),
            Event::BranchSummary {
                summary, reason, ..
            } => format!("branch_summary reason={} {}", reason, summary),
            Event::CompressionApplied { summary, .. } => summary.clone(),
```

In `Event::category`, include semantic session variants in `"session"` and branch/compaction in `"context"`:

```rust
            Event::SessionCreated { .. }
            | Event::SessionEnded { .. }
            | Event::SessionModeSet { .. }
            | Event::SessionSystemPromptSet { .. }
            | Event::SessionModelSet { .. }
            | Event::ActiveToolsSet { .. } => "session",
```

```rust
            Event::CompressionApplied { .. } | Event::BranchSummary { .. } => "context",
```

Ensure there is no duplicate `Event::CompressionApplied` match arm in `content_text` or `category`.

- [ ] **Step 5: Update all current `SessionModeSet` construction sites**

Run:

```bash
grep -rn "SessionModeSet" crates | cat
```

For every construction that currently uses `Event::SessionModeSet { mode }`, change it to:

```rust
Event::SessionModeSet {
    mode,
    source: Some("slash_command".into()),
    timestamp: Some(chrono::Utc::now()),
}
```

For tests that need deterministic output, use a local `let now = chrono::Utc::now();` and pass `timestamp: Some(now)`.

- [ ] **Step 6: Run tests**

Run:

```bash
cargo test -p holmes-core semantic_events_have_text_and_categories
cargo check --workspace
```

Expected: both commands exit 0.

- [ ] **Step 7: Commit**

```bash
git add crates/holmes-core/src/event.rs
git commit -m "feat(core): add semantic session events"
```

---

## Task 2: Add Compaction Archive DTOs and SessionStore Methods

**Files:**
- Create: `crates/holmes-session/src/compaction_archive.rs`
- Modify: `crates/holmes-session/src/lib.rs`
- Modify: `crates/holmes-session/src/store.rs`
- Modify: `crates/holmes-session/src/db.rs:13-18, 123-125`
- Test: `crates/holmes-session/tests/session_tests.rs`

- [ ] **Step 1: Write failing archive round-trip test**

Append this test to `crates/holmes-session/tests/session_tests.rs`:

```rust
#[tokio::test]
async fn compaction_archive_round_trips_through_session_store() {
    let temp_dir = std::env::temp_dir().join(format!("holmes_archive_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let db_path = temp_dir.join("holmes.db");
    let db = SessionDB::open(&db_path).await.unwrap();

    let session = db
        .create_session(CreateSessionParams {
            id: Some("archive_session".into()),
            title: Some("archive test".into()),
            mode: Some(SessionMode::Pentest),
            model: None,
            system_prompt: None,
            parent_session_id: None,
            fork_point: None,
            source: Some("test".into()),
            tags: vec![],
        })
        .await
        .unwrap();

    let event = Event::UserMessage {
        content: "old context".into(),
        timestamp: chrono::Utc::now(),
    };
    db.append_event(&session.id, &event).await.unwrap();
    let stored = db.get_events(&session.id).await.unwrap();

    let archive = holmes_session::CompactionArchive {
        session_id: session.id.clone(),
        compaction_event_index: 7,
        trigger: holmes_core::CompactionTrigger::Manual,
        archived_event_range: Some((0, 0)),
        messages: vec![holmes_core::Message::user("old context")],
        events: stored
            .iter()
            .map(holmes_session::ArchivedEvent::from_stored)
            .collect(),
        created_at: chrono::Utc::now(),
    };

    let path = db.write_compaction_archive(&session.id, 7, &archive).await.unwrap();
    assert!(path.ends_with("sessions/archive_session/compactions/compaction_7.json"));

    let loaded = db.read_compaction_archive(&path).await.unwrap();
    assert_eq!(loaded.session_id, session.id);
    assert_eq!(loaded.compaction_event_index, 7);
    assert_eq!(loaded.messages.len(), 1);
    assert_eq!(loaded.events.len(), 1);

    std::fs::remove_dir_all(temp_dir).ok();
}
```

- [ ] **Step 2: Run test to verify it fails**

Run:

```bash
cargo test -p holmes-session compaction_archive_round_trips_through_session_store
```

Expected: FAIL because `CompactionArchive`, `ArchivedEvent`, `write_compaction_archive`, and `read_compaction_archive` do not exist.

- [ ] **Step 3: Create archive DTO module**

Create `crates/holmes-session/src/compaction_archive.rs`:

```rust
use chrono::{DateTime, Utc};
use holmes_core::event::StoredEvent;
use holmes_core::{CompactionTrigger, Event, Message};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionArchive {
    pub session_id: String,
    pub compaction_event_index: u64,
    pub trigger: CompactionTrigger,
    pub archived_event_range: Option<(u64, u64)>,
    pub messages: Vec<Message>,
    pub events: Vec<ArchivedEvent>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchivedEvent {
    pub event_index: u64,
    pub event_type: String,
    pub timestamp: DateTime<Utc>,
    pub event: Event,
}

impl ArchivedEvent {
    pub fn from_stored(stored: &StoredEvent) -> Self {
        Self {
            event_index: stored.event_index,
            event_type: stored.event.category().to_string(),
            timestamp: stored.timestamp,
            event: stored.event.clone(),
        }
    }
}
```

- [ ] **Step 4: Export archive module**

Modify `crates/holmes-session/src/lib.rs`:

```rust
pub mod compaction_archive;
pub use compaction_archive::*;
```

Keep existing exports unchanged.

- [ ] **Step 5: Add trait methods to `SessionStore`**

Modify `crates/holmes-session/src/store.rs` imports and trait:

```rust
use crate::{CompactionArchive, CreateSessionParams, SessionError, SearchResult};
```

Add methods after `get_events`:

```rust
    async fn session_workspace(&self, session_id: &str) -> Result<std::path::PathBuf, SessionError>;

    async fn write_compaction_archive(
        &self,
        session_id: &str,
        compaction_event_index: u64,
        archive: &CompactionArchive,
    ) -> Result<String, SessionError>;

    async fn read_compaction_archive(&self, path: &str) -> Result<CompactionArchive, SessionError>;
```

- [ ] **Step 6: Implement archive methods for `SessionDB`**

In `crates/holmes-session/src/db.rs`, add imports:

```rust
use crate::{CompactionArchive, SessionError, SearchResult};
```

Inside `impl SessionStore for SessionDB`, after `get_events`, add:

```rust
    async fn session_workspace(&self, session_id: &str) -> Result<std::path::PathBuf, SessionError> {
        let path = self.sessions_dir.join(session_id);
        tokio::fs::create_dir_all(&path).await?;
        Ok(path)
    }

    async fn write_compaction_archive(
        &self,
        session_id: &str,
        compaction_event_index: u64,
        archive: &CompactionArchive,
    ) -> Result<String, SessionError> {
        let dir = self.session_workspace(session_id).await?.join("compactions");
        tokio::fs::create_dir_all(&dir).await?;
        let path = dir.join(format!("compaction_{compaction_event_index}.json"));
        let content = serde_json::to_string_pretty(archive)?;
        tokio::fs::write(&path, content).await?;
        Ok(path.to_string_lossy().to_string())
    }

    async fn read_compaction_archive(&self, path: &str) -> Result<CompactionArchive, SessionError> {
        let content = tokio::fs::read_to_string(path).await?;
        Ok(serde_json::from_str(&content)?)
    }
```

Ensure `SessionError` already supports `std::io::Error` and `serde_json::Error` through existing `From` impls. If not, add variants or `#[from]` fields in the existing error type in `db.rs`.

- [ ] **Step 7: Ensure new sessions create `compactions/` directory**

In `create_session`, change:

```rust
std::fs::create_dir_all(session_dir.join("tool-results")).ok();
```

To:

```rust
std::fs::create_dir_all(session_dir.join("tool-results")).ok();
std::fs::create_dir_all(session_dir.join("compactions")).ok();
```

- [ ] **Step 8: Run tests**

Run:

```bash
cargo test -p holmes-session compaction_archive_round_trips_through_session_store
cargo check --workspace
```

Expected: both commands exit 0.

- [ ] **Step 9: Commit**

```bash
git add crates/holmes-session/src/compaction_archive.rs crates/holmes-session/src/lib.rs crates/holmes-session/src/store.rs crates/holmes-session/src/db.rs crates/holmes-session/tests/session_tests.rs
git commit -m "feat(session): add compaction archive storage"
```

---

## Task 3: Implement Semantic Replay API

**Files:**
- Create: `crates/holmes-session/src/replay.rs`
- Modify: `crates/holmes-session/src/lib.rs`
- Modify: `crates/holmes-session/src/store.rs`
- Modify: `crates/holmes-session/src/db.rs`
- Test: `crates/holmes-session/tests/session_tests.rs`

- [ ] **Step 1: Write failing replay tests**

Append these tests to `crates/holmes-session/tests/session_tests.rs`:

```rust
#[tokio::test]
async fn replay_complete_semantic_session_restores_metadata_and_messages() {
    let db = SessionDB::open(":memory:").await.unwrap();
    let session = db
        .create_session(CreateSessionParams {
            id: Some("semantic_session".into()),
            title: Some("semantic".into()),
            mode: Some(SessionMode::Pentest),
            model: Some("claude-sonnet-4-6".into()),
            system_prompt: Some("old table prompt".into()),
            parent_session_id: None,
            fork_point: None,
            source: Some("test".into()),
            tags: vec![],
        })
        .await
        .unwrap();
    let now = chrono::Utc::now();

    db.append_event(&session.id, &Event::SessionCreated {
        id: session.id.clone(),
        title: session.title.clone(),
        mode: SessionMode::Pentest,
        model: Some("claude-sonnet-4-6".into()),
        system_prompt: Some("semantic prompt".into()),
        parent_id: None,
        fork_point: None,
        created_at: now,
        tags: vec![],
    }).await.unwrap();
    db.append_event(&session.id, &Event::SessionSystemPromptSet {
        prompt_hash: "hash-semantic".into(),
        content: "semantic prompt".into(),
        source: "startup".into(),
        timestamp: now,
    }).await.unwrap();
    db.append_event(&session.id, &Event::SessionModeSet {
        mode: SessionMode::SecurityResearch,
        source: Some("startup".into()),
        timestamp: Some(now),
    }).await.unwrap();
    db.append_event(&session.id, &Event::SessionModelSet {
        model: "claude-opus-4-8".into(),
        provider: Some("default".into()),
        source: "startup".into(),
        timestamp: now,
    }).await.unwrap();
    db.append_event(&session.id, &Event::ActiveToolsSet {
        tool_names: vec!["http_request".into(), "report_finding".into()],
        source: "startup".into(),
        timestamp: now,
    }).await.unwrap();
    db.append_event(&session.id, &Event::UserMessage {
        content: "hello".into(),
        timestamp: now,
    }).await.unwrap();

    let replayed = db.replay_session_context(&session.id).await.unwrap();
    assert!(replayed.semantic_complete);
    assert_eq!(replayed.system_prompt.as_deref(), Some("semantic prompt"));
    assert_eq!(replayed.model.as_deref(), Some("claude-opus-4-8"));
    assert_eq!(replayed.active_tools, vec!["http_request", "report_finding"]);
    assert_eq!(replayed.session.mode, SessionMode::SecurityResearch);
    assert_eq!(replayed.session.messages[0].content.as_deref(), Some("semantic prompt"));
    assert_eq!(replayed.session.messages.last().unwrap().content.as_deref(), Some("hello"));
}

#[tokio::test]
async fn replay_legacy_session_reports_incomplete() {
    let db = SessionDB::open(":memory:").await.unwrap();
    let session = db
        .create_session(CreateSessionParams {
            id: Some("legacy_session".into()),
            title: Some("legacy".into()),
            mode: Some(SessionMode::Pentest),
            model: None,
            system_prompt: None,
            parent_session_id: None,
            fork_point: None,
            source: Some("test".into()),
            tags: vec![],
        })
        .await
        .unwrap();
    db.append_event(&session.id, &Event::UserMessage {
        content: "legacy hello".into(),
        timestamp: chrono::Utc::now(),
    }).await.unwrap();

    let replayed = db.replay_session_context(&session.id).await.unwrap();
    assert!(!replayed.semantic_complete);
    assert_eq!(replayed.session.messages.len(), 1);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run:

```bash
cargo test -p holmes-session replay_complete_semantic_session_restores_metadata_and_messages replay_legacy_session_reports_incomplete
```

Expected: FAIL because `replay_session_context` and replay types do not exist.

- [ ] **Step 3: Create replay module**

Create `crates/holmes-session/src/replay.rs`:

```rust
use holmes_core::event::{Event, StoredEvent};
use holmes_core::session::RuntimeSession;
use holmes_core::{Message, Role, SessionMode};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct ReplayedSessionContext {
    pub session: RuntimeSession,
    pub system_prompt: Option<String>,
    pub model: Option<String>,
    pub active_tools: Vec<String>,
    pub compactions: Vec<CompactionReplayMarker>,
    pub branch_summaries: Vec<String>,
    pub semantic_complete: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionReplayMarker {
    pub event_index: u64,
    pub summary: String,
    pub archive_path: Option<String>,
    pub archived_event_range: Option<(u64, u64)>,
}

#[derive(Debug, Default)]
struct ReplayState {
    saw_session_created: bool,
    saw_system_prompt: bool,
    saw_mode: bool,
    saw_model: bool,
    saw_active_tools: bool,
    system_prompt: Option<String>,
    model: Option<String>,
    mode: Option<SessionMode>,
    active_tools: Vec<String>,
    messages: Vec<Message>,
    compactions: Vec<CompactionReplayMarker>,
    branch_summaries: Vec<String>,
    warnings: Vec<String>,
}

pub fn replay_events(session_id: &str, events: &[StoredEvent]) -> ReplayedSessionContext {
    let mut state = ReplayState::default();

    for stored in events {
        match &stored.event {
            Event::SessionCreated { mode, model, system_prompt, .. } => {
                state.saw_session_created = true;
                state.mode = Some(mode.clone());
                if state.model.is_none() {
                    state.model = model.clone();
                }
                if state.system_prompt.is_none() {
                    state.system_prompt = system_prompt.clone();
                }
            }
            Event::SessionSystemPromptSet { content, .. } => {
                state.saw_system_prompt = true;
                state.system_prompt = Some(content.clone());
            }
            Event::SessionModeSet { mode, .. } => {
                state.saw_mode = true;
                state.mode = Some(mode.clone());
            }
            Event::SessionModelSet { model, .. } => {
                state.saw_model = true;
                state.model = Some(model.clone());
            }
            Event::ActiveToolsSet { tool_names, .. } => {
                state.saw_active_tools = true;
                state.active_tools = tool_names.clone();
            }
            Event::BranchSummary { summary, .. } => {
                state.branch_summaries.push(summary.clone());
                state.messages.push(Message {
                    role: Role::System,
                    content: Some(format!("[Branch summary]\n{}", summary)),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                });
            }
            Event::CompressionApplied {
                summary,
                archive_path,
                archived_event_range,
                ..
            } => {
                state.compactions.push(CompactionReplayMarker {
                    event_index: stored.event_index,
                    summary: summary.clone(),
                    archive_path: archive_path.clone(),
                    archived_event_range: *archived_event_range,
                });
                apply_compaction_summary(&mut state.messages, summary);
            }
            Event::UserMessage { content, .. } => {
                state.messages.push(Message::user(content.clone()));
            }
            Event::Thinking { content, .. } => {
                state.messages.push(Message::assistant(content.clone()));
            }
            Event::ToolResult { name, content, .. } => {
                let call_id = format!("replayed-tool-{}", stored.event_index);
                state.messages.push(Message::tool_result(call_id, name.clone(), content.clone()));
            }
            _ => {}
        }
    }

    let mode = state.mode.clone().unwrap_or_default();
    let mut session = RuntimeSession::new(session_id.to_string(), mode);
    if let Some(prompt) = &state.system_prompt {
        session.messages.push(Message::system(prompt.clone()));
    }
    session.messages.extend(state.messages);

    let semantic_complete = state.saw_session_created
        && state.saw_system_prompt
        && state.saw_mode
        && state.saw_model
        && state.saw_active_tools;

    ReplayedSessionContext {
        session,
        system_prompt: state.system_prompt,
        model: state.model,
        active_tools: state.active_tools,
        compactions: state.compactions,
        branch_summaries: state.branch_summaries,
        semantic_complete,
        warnings: state.warnings,
    }
}

fn apply_compaction_summary(messages: &mut Vec<Message>, summary: &str) {
    if messages.len() > 2 {
        messages.truncate(1);
    }
    messages.push(Message::assistant(format!("[Compaction summary]\n{}", summary)));
}
```

- [ ] **Step 4: Export replay module**

Modify `crates/holmes-session/src/lib.rs`:

```rust
pub mod replay;
pub use replay::*;
```

- [ ] **Step 5: Add replay method to `SessionStore`**

Modify `crates/holmes-session/src/store.rs` imports:

```rust
use crate::{CompactionArchive, CreateSessionParams, ReplayedSessionContext, SessionError, SearchResult};
```

Add to trait after `get_events`:

```rust
    async fn replay_session_context(
        &self,
        session_id: &str,
    ) -> Result<ReplayedSessionContext, SessionError>;
```

- [ ] **Step 6: Implement replay method in `SessionDB`**

In `crates/holmes-session/src/db.rs`, add to `impl SessionStore for SessionDB` after `get_events`:

```rust
    async fn replay_session_context(
        &self,
        session_id: &str,
    ) -> Result<crate::ReplayedSessionContext, SessionError> {
        let events = self.get_events(session_id).await?;
        Ok(crate::replay::replay_events(session_id, &events))
    }
```

- [ ] **Step 7: Run replay tests**

Run:

```bash
cargo test -p holmes-session replay_complete_semantic_session_restores_metadata_and_messages
cargo test -p holmes-session replay_legacy_session_reports_incomplete
cargo check --workspace
```

Expected: all commands exit 0.

- [ ] **Step 8: Commit**

```bash
git add crates/holmes-session/src/replay.rs crates/holmes-session/src/lib.rs crates/holmes-session/src/store.rs crates/holmes-session/src/db.rs crates/holmes-session/tests/session_tests.rs
git commit -m "feat(session): replay semantic session context"
```

---

## Task 4: Write Startup Metadata for New Sessions

**Files:**
- Modify: `crates/holmes-cli/src/chat.rs:1224-1252, 1259-1383`
- Modify: `crates/holmes-cli/src/subagent.rs`
- Modify: `crates/holmes-harness/src/runner.rs`
- Test: `crates/holmes-cli/tests/slash_commands.rs` or new unit tests in `chat.rs` if helpers stay private

- [ ] **Step 1: Add helper functions in `chat.rs`**

Near `create_fresh_runtime_session`, add:

```rust
fn prompt_hash(prompt: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    prompt.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn configured_model(config: &HolmesConfig, override_model: Option<String>) -> Option<String> {
    override_model.or_else(|| {
        let role = &config.llm.roles.attack_agent;
        config
            .llm
            .providers
            .iter()
            .find(|provider| &provider.name == role)
            .or_else(|| config.llm.providers.first())
            .map(|provider| provider.model.clone())
    })
}

fn active_tool_names(registry: &ToolRegistry) -> Vec<String> {
    let mut names = registry
        .definitions()
        .into_iter()
        .map(|definition| definition.function.name)
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    names
}
```

- [ ] **Step 2: Change `create_fresh_runtime_session` signature**

Replace the current helper signature with:

```rust
async fn create_fresh_runtime_session(
    session_db: Arc<dyn SessionStore>,
    memory_store: Arc<MemoryStore>,
    mode: SessionMode,
    model: Option<String>,
    system_prompt: String,
    tool_names: Vec<String>,
) -> anyhow::Result<(String, RuntimeSession, MindPalace, bool)> {
```

Inside the helper, after `create_session(...).await?`, append metadata events:

```rust
    let now = chrono::Utc::now();
    session_db
        .append_event(
            &session.id,
            &Event::SessionCreated {
                id: session.id.clone(),
                title: session.title.clone(),
                mode: mode.clone(),
                model: model.clone(),
                system_prompt: Some(system_prompt.clone()),
                parent_id: None,
                fork_point: None,
                created_at: now,
                tags: session.tags.clone(),
            },
        )
        .await?;
    session_db
        .append_event(
            &session.id,
            &Event::SessionSystemPromptSet {
                prompt_hash: prompt_hash(&system_prompt),
                content: system_prompt.clone(),
                source: "startup".into(),
                timestamp: now,
            },
        )
        .await?;
    session_db
        .append_event(
            &session.id,
            &Event::SessionModeSet {
                mode: mode.clone(),
                source: Some("startup".into()),
                timestamp: Some(now),
            },
        )
        .await?;
    if let Some(model_name) = &model {
        session_db
            .append_event(
                &session.id,
                &Event::SessionModelSet {
                    model: model_name.clone(),
                    provider: None,
                    source: "startup".into(),
                    timestamp: now,
                },
            )
            .await?;
    }
    session_db
        .append_event(
            &session.id,
            &Event::ActiveToolsSet {
                tool_names,
                source: "startup".into(),
                timestamp: now,
            },
        )
        .await?;
```

- [ ] **Step 3: Build registry before choosing fresh/resume path**

In `create_chat_context`, move the `build_tool_registry` call before the session selection block. Use this structure:

```rust
    let registry = Arc::new(
        build_tool_registry(
            &config,
            Some(session_db.clone()),
            Some(memory_store.clone()),
            Some(llm.clone()),
            None,
        )
        .await,
    );
    let tool_names = active_tool_names(&registry);
    let startup_model = configured_model(&config, model.clone());
```

Then pass `startup_model.clone()` and `tool_names.clone()` into `create_fresh_runtime_session`.

After the final `session_id` is known, rebuild registry with `Some(session_id.clone())` as today so subagent runner receives the correct session id:

```rust
    let registry = Arc::new(
        build_tool_registry(
            &config,
            Some(session_db.clone()),
            Some(memory_store.clone()),
            Some(llm.clone()),
            Some(session_id.clone()),
        )
        .await,
    );
```

- [ ] **Step 4: Use replay API for resume, with legacy fallback warning**

Replace resume branches that manually call `get_events` + `replay_events_into_runtime` with:

```rust
        let replayed = session_db.replay_session_context(&id).await?;
        let mut mp = MindPalace::new(session_db.clone(), memory_store.clone());
        let events = session_db.get_events(&id).await?;
        for stored in &events {
            mp.ingest(stored.event.clone());
        }
        let session = if replayed.semantic_complete {
            replayed.session
        } else {
            let mut legacy = RuntimeSession::new(id.clone(), mode.clone()).with_system_prompt(&system_prompt);
            replay_events_into_runtime(&mut legacy, &mut mp, &events);
            if announce {
                eprintln!("⚠ Legacy session {}; semantic replay metadata is incomplete.", &id[..8.min(id.len())]);
            }
            legacy
        };
```

Use the same pattern for `continue_last`.

- [ ] **Step 5: Add startup metadata assertions in CLI tests**

If `create_chat_context` cannot be tested without config files, add this helper-level unit test in `chat.rs` under `#[cfg(test)]`:

```rust
#[test]
fn configured_model_prefers_override_then_role_provider() {
    let mut config = HolmesConfig::default();
    config.llm.roles.attack_agent = "main".into();
    config.llm.providers[0].name = "main".into();
    config.llm.providers[0].model = "role-model".into();

    assert_eq!(configured_model(&config, Some("override".into())).as_deref(), Some("override"));
    assert_eq!(configured_model(&config, None).as_deref(), Some("role-model"));
}
```

- [ ] **Step 6: Run tests**

Run:

```bash
cargo test -p holmes-cli configured_model_prefers_override_then_role_provider
cargo check --workspace
```

Expected: both commands exit 0.

- [ ] **Step 7: Commit**

```bash
git add crates/holmes-cli/src/chat.rs crates/holmes-cli/src/subagent.rs crates/holmes-harness/src/runner.rs
git commit -m "feat(cli): write startup semantic session metadata"
```

---

## Task 5: Persist Runtime Mutation Events

**Files:**
- Modify: `crates/holmes-cli/src/chat.rs` slash command handlers for `/model`, `/mode`, `/mcp reload`
- Modify: `crates/holmes-cli/src/tui.rs` if TUI has direct mode/model/tool mutation paths
- Test: `crates/holmes-cli/tests/slash_commands.rs`

- [ ] **Step 1: Locate mutation handlers**

Run:

```bash
grep -n '"model"\|"mode"\|"mcp"' crates/holmes-cli/src/chat.rs crates/holmes-cli/src/tui.rs
```

Expected: output includes the slash command match arms in `chat.rs` and any TUI overlay handlers.

- [ ] **Step 2: Add helper for appending active tool events**

Near `active_tool_names` in `chat.rs`, add:

```rust
async fn append_active_tools_event(ctx: &ChatContext, source: &str) -> anyhow::Result<()> {
    ctx.session_db
        .append_event(
            &ctx.session_id,
            &Event::ActiveToolsSet {
                tool_names: active_tool_names(&ctx.registry),
                source: source.to_string(),
                timestamp: chrono::Utc::now(),
            },
        )
        .await?;
    Ok(())
}
```

- [ ] **Step 3: Update `/mode` handler**

In the `/mode` handler after updating `ctx.runtime_session.mode` and `ctx.runtime_state.session_mode`, append:

```rust
ctx.session_db
    .append_event(
        &ctx.session_id,
        &Event::SessionModeSet {
            mode: new_mode.clone(),
            source: Some("slash_command".into()),
            timestamp: Some(chrono::Utc::now()),
        },
    )
    .await
    .ok();
```

Use the actual local variable name for the parsed mode. If the existing handler calls it `mode`, keep the field `mode: mode.clone()`.

- [ ] **Step 4: Update `/model` handler**

In the `/model` handler after the selected model is applied to the current context/config, append:

```rust
ctx.session_db
    .append_event(
        &ctx.session_id,
        &Event::SessionModelSet {
            model: selected_model.clone(),
            provider: None,
            source: "slash_command".into(),
            timestamp: chrono::Utc::now(),
        },
    )
    .await
    .ok();
```

Use the actual variable name for the selected model. If the current handler only writes config and does not store current model in `ChatContext`, append the event at the same point it prints success.

- [ ] **Step 5: Update MCP reload / active tools mutation**

After MCP reload rebuilds or updates the registry, call:

```rust
if let Err(error) = append_active_tools_event(ctx, "mcp_reload").await {
    eprintln!("Warning: failed to persist active tools: {error}");
}
```

- [ ] **Step 6: Add slash command persistence test**

If existing slash command tests can instantiate a `ChatContext`, add assertions that `/mode research` writes `SessionModeSet` and `/model <name>` writes `SessionModelSet`. If they only test registry resolution, add a unit test for serialization instead:

```rust
#[test]
fn model_and_mode_mutation_events_serialize() {
    let now = chrono::Utc::now();
    let mode_event = Event::SessionModeSet {
        mode: SessionMode::SecurityResearch,
        source: Some("slash_command".into()),
        timestamp: Some(now),
    };
    let model_event = Event::SessionModelSet {
        model: "claude-opus-4-8".into(),
        provider: None,
        source: "slash_command".into(),
        timestamp: now,
    };
    assert!(serde_json::to_string(&mode_event).unwrap().contains("slash_command"));
    assert!(serde_json::to_string(&model_event).unwrap().contains("claude-opus-4-8"));
}
```

- [ ] **Step 7: Run tests**

Run:

```bash
cargo test -p holmes-cli slash_commands
cargo check --workspace
```

Expected: both commands exit 0.

- [ ] **Step 8: Commit**

```bash
git add crates/holmes-cli/src/chat.rs crates/holmes-cli/src/tui.rs crates/holmes-cli/tests/slash_commands.rs
git commit -m "feat(cli): persist runtime session mutations"
```

---

## Task 6: Add Summary Service

**Files:**
- Create: `crates/holmes-runtime/src/summary.rs`
- Modify: `crates/holmes-runtime/src/lib.rs`
- Test: `crates/holmes-runtime/src/summary.rs`

- [ ] **Step 1: Write tests for static branch and compaction summaries**

Create `crates/holmes-runtime/src/summary.rs` with tests first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use holmes_core::event::Event;

    #[test]
    fn static_branch_summary_mentions_user_tools_and_findings() {
        let now = chrono::Utc::now();
        let events = vec![
            stored(0, Event::UserMessage { content: "test login".into(), timestamp: now }),
            stored(1, Event::ToolCall { name: "http_request".into(), arguments: serde_json::json!({"url":"/login"}), purpose: Some("probe".into()) }),
            stored(2, Event::VulnerabilityFound { title: "IDOR".into(), cwe: None, cvss: None, severity: holmes_core::Severity::High, location: "/api/users/2".into(), evidence: "user2 data returned".into(), poc: None, status: holmes_core::FindingStatus::Suspicious }),
        ];
        let summary = static_branch_summary(&events, "fork");
        assert!(summary.contains("test login"));
        assert!(summary.contains("http_request"));
        assert!(summary.contains("IDOR"));
    }

    fn stored(event_index: u64, event: Event) -> holmes_core::event::StoredEvent {
        holmes_core::event::StoredEvent {
            id: event_index,
            session_id: "s".into(),
            event_index,
            turn_index: None,
            timestamp: chrono::Utc::now(),
            event,
        }
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run:

```bash
cargo test -p holmes-runtime static_branch_summary_mentions_user_tools_and_findings
```

Expected: FAIL because `static_branch_summary` is not defined.

- [ ] **Step 3: Implement summary module**

Fill `crates/holmes-runtime/src/summary.rs`:

```rust
use holmes_core::event::{Event, StoredEvent};
use holmes_core::{SummaryMethod, truncate_str};

#[derive(Debug, Clone)]
pub struct GeneratedSummary {
    pub summary: String,
    pub method: SummaryMethod,
}

pub fn static_branch_summary(events: &[StoredEvent], reason: &str) -> String {
    let mut user_messages = Vec::new();
    let mut tool_calls = Vec::new();
    let mut findings = Vec::new();
    let mut errors = Vec::new();
    let mut last_assistant = None;

    for stored in events {
        match &stored.event {
            Event::UserMessage { content, .. } => user_messages.push(content.clone()),
            Event::Thinking { content, .. } => last_assistant = Some(content.clone()),
            Event::ToolCall { name, .. } => tool_calls.push(name.clone()),
            Event::ToolResult { name, success, error, .. } => {
                if !success {
                    errors.push(format!("{}: {}", name, error.as_deref().unwrap_or("failed")));
                }
            }
            Event::VulnerabilityFound { title, evidence, .. } => {
                findings.push(format!("{} — {}", title, evidence));
            }
            _ => {}
        }
    }

    let summary = format!(
        "Branch summary ({reason})\n- User objectives: {}\n- Tools used: {}\n- Findings: {}\n- Errors: {}\n- Last assistant note: {}",
        join_or_none(&user_messages),
        join_or_none(&tool_calls),
        join_or_none(&findings),
        join_or_none(&errors),
        last_assistant.as_deref().unwrap_or("none"),
    );
    truncate_str(&summary, 1200).to_string()
}

pub fn static_compaction_summary(messages: &[holmes_core::Message]) -> String {
    let sample = messages
        .iter()
        .filter_map(|message| message.content.as_deref())
        .take(8)
        .collect::<Vec<_>>()
        .join("\n- ");
    truncate_str(
        &format!(
            "Compaction summary\n- Messages compacted: {}\n- Preserved notes:\n- {}",
            messages.len(),
            sample
        ),
        1600,
    )
    .to_string()
}

pub fn fallback_summary(summary: String) -> GeneratedSummary {
    GeneratedSummary {
        summary,
        method: SummaryMethod::StaticFallback,
    }
}

fn join_or_none(items: &[String]) -> String {
    if items.is_empty() {
        "none".into()
    } else {
        items.join("; ")
    }
}
```

This task creates static fallback first. LLM-first summary is wired in Task 7 using this module as fallback.

- [ ] **Step 4: Export module**

Modify `crates/holmes-runtime/src/lib.rs`:

```rust
pub mod summary;
```

- [ ] **Step 5: Run tests**

Run:

```bash
cargo test -p holmes-runtime static_branch_summary_mentions_user_tools_and_findings
cargo check --workspace
```

Expected: both commands exit 0.

- [ ] **Step 6: Commit**

```bash
git add crates/holmes-runtime/src/summary.rs crates/holmes-runtime/src/lib.rs
git commit -m "feat(runtime): add semantic summary fallbacks"
```

---

## Task 7: Append BranchSummary on Fork

**Files:**
- Modify: `crates/holmes-session/src/db.rs:574-650`
- Modify: `crates/holmes-cli/src/chat.rs:1973-2019, 2046-2088`
- Modify: `crates/holmes-cli/src/tui.rs` fork handlers
- Test: `crates/holmes-session/tests/session_tests.rs`

- [ ] **Step 1: Write failing fork summary test**

Append this test to `crates/holmes-session/tests/session_tests.rs`:

```rust
#[tokio::test]
async fn forked_session_can_receive_branch_summary_event() {
    let db = SessionDB::open(":memory:").await.unwrap();
    let parent = db.create_session(CreateSessionParams {
        id: Some("parent_branch".into()),
        title: Some("parent".into()),
        mode: Some(SessionMode::Pentest),
        model: None,
        system_prompt: None,
        parent_session_id: None,
        fork_point: None,
        source: Some("test".into()),
        tags: vec![],
    }).await.unwrap();
    db.append_event(&parent.id, &Event::UserMessage {
        content: "investigate idor".into(),
        timestamp: chrono::Utc::now(),
    }).await.unwrap();

    let child = db.fork_session(&parent.id, 0, "child").await.unwrap();
    db.append_event(&child.id, &Event::BranchSummary {
        from_event_index: 0,
        to_event_index: 0,
        summary: "parent investigated idor".into(),
        reason: "fork".into(),
        method: holmes_core::SummaryMethod::StaticFallback,
        timestamp: chrono::Utc::now(),
    }).await.unwrap();

    let replayed = db.replay_session_context(&child.id).await.unwrap();
    assert_eq!(replayed.branch_summaries, vec!["parent investigated idor"]);
    assert!(replayed.session.messages.iter().any(|m| m.content.as_deref().unwrap_or("").contains("Branch summary")));
}
```

- [ ] **Step 2: Run test**

Run:

```bash
cargo test -p holmes-session forked_session_can_receive_branch_summary_event
```

Expected: PASS if Task 3 replay handles `BranchSummary`. If it fails, fix replay before continuing.

- [ ] **Step 3: Add CLI helper to append branch summary**

In `crates/holmes-cli/src/chat.rs`, add:

```rust
async fn append_branch_summary(
    ctx: &ChatContext,
    new_session_id: &str,
    from_event_index: u64,
    to_event_index: u64,
    reason: &str,
) -> anyhow::Result<()> {
    let events = ctx.session_db.get_events(&ctx.session_id).await?;
    let window = events
        .into_iter()
        .filter(|event| event.event_index >= from_event_index && event.event_index <= to_event_index)
        .collect::<Vec<_>>();
    let summary = holmes_runtime::summary::static_branch_summary(&window, reason);
    ctx.session_db
        .append_event(
            new_session_id,
            &Event::BranchSummary {
                from_event_index,
                to_event_index,
                summary,
                reason: reason.to_string(),
                method: holmes_core::SummaryMethod::StaticFallback,
                timestamp: chrono::Utc::now(),
            },
        )
        .await?;
    Ok(())
}
```

This implements static fallback. LLM-first can be added by extending this helper to call `ctx.llm.chat_completion_oneshot` if the current `LlmClient` exposes that path; do not block this task on LLM summary.

- [ ] **Step 4: Call helper in `/tree fork` handler**

After `fork_session` succeeds and before `load_session_runtime`, add:

```rust
if let Err(error) = append_branch_summary(
    ctx,
    &new_session.id,
    0,
    fork_point,
    "tree_fork",
)
.await
{
    eprintln!("Warning: failed to persist branch summary: {error}");
}
```

- [ ] **Step 5: Call helper in `/branch` handler**

After `fork_session` succeeds and before `load_session_runtime`, add:

```rust
if let Err(error) = append_branch_summary(
    ctx,
    &new_session.id,
    0,
    fork_point,
    "branch",
)
.await
{
    eprintln!("Warning: failed to persist branch summary: {error}");
}
```

- [ ] **Step 6: Mirror the same helper path in TUI fork handlers**

In `crates/holmes-cli/src/tui.rs`, replace direct fork reload logic with the same chat helper if accessible. If the helper is private, make it `pub(crate)`:

```rust
pub(crate) async fn append_branch_summary(...)
```

Call it with `reason` values `"tui_event_fork"` or `"tui_latest_fork"`.

- [ ] **Step 7: Run tests and check**

Run:

```bash
cargo test -p holmes-session forked_session_can_receive_branch_summary_event
cargo check --workspace
```

Expected: both commands exit 0.

- [ ] **Step 8: Commit**

```bash
git add crates/holmes-session/tests/session_tests.rs crates/holmes-cli/src/chat.rs crates/holmes-cli/src/tui.rs
git commit -m "feat(cli): persist branch summaries on fork"
```

---

## Task 8: Make Runtime Compaction Archive-Backed

**Files:**
- Modify: `crates/holmes-runtime/src/compaction.rs`
- Modify: `crates/holmes-runtime/src/runtime.rs:437-480`
- Test: `crates/holmes-runtime/src/compaction.rs`
- Test: `crates/holmes-runtime/src/runtime.rs` existing compaction tests

- [ ] **Step 1: Extend compaction structs**

In `crates/holmes-runtime/src/compaction.rs`, change `CompressionPlan` and `CompressionResult`:

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct CompressionPlan {
    pub should_compress: bool,
    pub force: bool,
    pub before_count: usize,
    pub estimated_tokens: u64,
    pub threshold_tokens: u64,
    pub protected_head: usize,
    pub protected_tail_start: usize,
    pub archived_message_range: Option<(usize, usize)>,
}

#[derive(Debug, Clone)]
pub struct CompressionResult {
    pub before_count: usize,
    pub after_count: usize,
    pub summary: String,
    pub preserved_keys: Vec<String>,
    pub extracted_pitfalls: Vec<PitfallSummary>,
    pub method: CompressionMethod,
    pub archived_message_range: Option<(usize, usize)>,
    pub trigger: holmes_core::CompactionTrigger,
    pub archive_path: Option<String>,
    pub archived_event_range: Option<(u64, u64)>,
}
```

In `plan`, set:

```rust
            archived_message_range: has_middle.then_some((protected_head, protected_tail_start)),
```

- [ ] **Step 2: Change `compress_session` signature**

Change:

```rust
pub fn compress_session(
    &mut self,
    session: &mut RuntimeSession,
    config: &HolmesConfig,
    plan: CompressionPlan,
) -> Result<Option<CompressionResult>>
```

To:

```rust
pub fn compress_session(
    &mut self,
    session: &mut RuntimeSession,
    config: &HolmesConfig,
    plan: CompressionPlan,
    trigger: holmes_core::CompactionTrigger,
) -> Result<Option<CompressionResult>>
```

Populate new result fields:

```rust
            archived_message_range: plan.archived_message_range,
            trigger,
            archive_path: None,
            archived_event_range: None,
```

- [ ] **Step 3: Update compaction unit tests**

In compaction tests, update calls:

```rust
.compress_session(&mut session, &config, plan, holmes_core::CompactionTrigger::Manual)
```

Add assertions in `static_compaction_preserves_head_summary_and_tail`:

```rust
assert_eq!(result.archived_message_range, Some((1, 3)));
assert_eq!(result.trigger, holmes_core::CompactionTrigger::Manual);
assert_eq!(result.archive_path, None);
```

- [ ] **Step 4: Add archive creation in runtime compaction**

In `crates/holmes-runtime/src/runtime.rs`, replace `compact_with_force(force)` with trigger-aware helper:

```rust
async fn compact_with_trigger(
    &mut self,
    force: bool,
    trigger: holmes_core::CompactionTrigger,
) -> Result<Option<CompressionResult>, RuntimeError> {
```

Update `maybe_compact`:

```rust
async fn maybe_compact(&mut self) -> Result<Option<CompressionResult>, RuntimeError> {
    self.compact_with_trigger(false, holmes_core::CompactionTrigger::Threshold).await
}
```

Update `compact_now` to call:

```rust
self.compact_with_trigger(true, holmes_core::CompactionTrigger::Manual).await
```

Inside `compact_with_trigger`, before calling `compress_session`, clone messages and events for archive:

```rust
let events_before = self
    .context
    .session_db
    .get_events(&self.context.session_id)
    .await
    .map_err(|error| RuntimeError::recoverable(format!("failed to load events before compaction: {error}")))?;
let messages_before = self.context.session.messages.clone();
```

Call compactor:

```rust
let Some(mut result) = self
    .compactor
    .compress_session(
        &mut self.context.session,
        &self.context.config,
        plan,
        trigger.clone(),
    )
    .map_err(|error| RuntimeError::recoverable(error.to_string()))?
else {
    return Ok(None);
};
```

Then archive if a message range exists:

```rust
if let Some((start, end)) = result.archived_message_range {
    let archived_messages = messages_before[start..end].to_vec();
    let archived_event_range = events_before
        .first()
        .zip(events_before.last())
        .map(|(first, last)| (first.event_index, last.event_index));
    let next_index = self.next_event_index().await?;
    let archive = holmes_session::CompactionArchive {
        session_id: self.context.session_id.clone(),
        compaction_event_index: next_index,
        trigger: trigger.clone(),
        archived_event_range,
        messages: archived_messages,
        events: events_before
            .iter()
            .map(holmes_session::ArchivedEvent::from_stored)
            .collect(),
        created_at: chrono::Utc::now(),
    };
    let archive_path = self
        .context
        .session_db
        .write_compaction_archive(&self.context.session_id, next_index, &archive)
        .await
        .map_err(|error| RuntimeError::recoverable(format!("failed to write compaction archive: {error}")))?;
    result.archive_path = Some(archive_path);
    result.archived_event_range = archived_event_range;
}
```

- [ ] **Step 5: Append enriched `CompressionApplied` event**

Replace existing append with:

```rust
append_and_ingest(
    &mut self.context,
    Event::CompressionApplied {
        before_count: result.before_count,
        after_count: result.after_count,
        summary: result.summary.clone(),
        preserved_keys: result.preserved_keys.clone(),
        method: result.method.clone(),
        preserved_head: Some(plan.protected_head),
        preserved_tail_tokens: Some(self.context.config.compressor.protected_tail_tokens),
        archive_path: result.archive_path.clone(),
        archived_event_range: result.archived_event_range,
        trigger: Some(result.trigger.clone()),
        timestamp: Some(chrono::Utc::now()),
    },
)
.await?;
```

If `plan` has moved into `compress_session`, clone the values before passing it.

- [ ] **Step 6: Run compaction tests**

Run:

```bash
cargo test -p holmes-runtime compaction
cargo test -p holmes-runtime compact_now
cargo check --workspace
```

Expected: all commands exit 0.

- [ ] **Step 7: Commit**

```bash
git add crates/holmes-runtime/src/compaction.rs crates/holmes-runtime/src/runtime.rs
git commit -m "feat(runtime): persist archive-backed compaction events"
```

---

## Task 9: Replay Compaction Events and Missing Archives

**Files:**
- Modify: `crates/holmes-session/src/replay.rs`
- Test: `crates/holmes-session/tests/session_tests.rs`

- [ ] **Step 1: Write replay compaction test**

Append this test to `crates/holmes-session/tests/session_tests.rs`:

```rust
#[tokio::test]
async fn replay_injects_compaction_summary_and_marker() {
    let db = SessionDB::open(":memory:").await.unwrap();
    let session = db.create_session(CreateSessionParams {
        id: Some("compact_replay".into()),
        title: Some("compact replay".into()),
        mode: Some(SessionMode::Pentest),
        model: None,
        system_prompt: None,
        parent_session_id: None,
        fork_point: None,
        source: Some("test".into()),
        tags: vec![],
    }).await.unwrap();
    let now = chrono::Utc::now();
    db.append_event(&session.id, &Event::SessionCreated { id: session.id.clone(), title: None, mode: SessionMode::Pentest, model: Some("m".into()), system_prompt: Some("system".into()), parent_id: None, fork_point: None, created_at: now, tags: vec![] }).await.unwrap();
    db.append_event(&session.id, &Event::SessionSystemPromptSet { prompt_hash: "h".into(), content: "system".into(), source: "startup".into(), timestamp: now }).await.unwrap();
    db.append_event(&session.id, &Event::SessionModeSet { mode: SessionMode::Pentest, source: Some("startup".into()), timestamp: Some(now) }).await.unwrap();
    db.append_event(&session.id, &Event::SessionModelSet { model: "m".into(), provider: None, source: "startup".into(), timestamp: now }).await.unwrap();
    db.append_event(&session.id, &Event::ActiveToolsSet { tool_names: vec![], source: "startup".into(), timestamp: now }).await.unwrap();
    db.append_event(&session.id, &Event::UserMessage { content: "old".into(), timestamp: now }).await.unwrap();
    db.append_event(&session.id, &Event::CompressionApplied {
        before_count: 6,
        after_count: 3,
        summary: "old work summarized".into(),
        preserved_keys: vec!["system_prompt".into()],
        method: holmes_core::CompressionMethod::StaticFallback,
        preserved_head: Some(1),
        preserved_tail_tokens: Some(4000),
        archive_path: Some("missing.json".into()),
        archived_event_range: Some((0, 5)),
        trigger: Some(holmes_core::CompactionTrigger::Manual),
        timestamp: Some(now),
    }).await.unwrap();

    let replayed = db.replay_session_context(&session.id).await.unwrap();
    assert_eq!(replayed.compactions.len(), 1);
    assert!(replayed.session.messages.iter().any(|m| m.content.as_deref().unwrap_or("").contains("old work summarized")));
}
```

- [ ] **Step 2: Run test**

Run:

```bash
cargo test -p holmes-session replay_injects_compaction_summary_and_marker
```

Expected: PASS if Task 3 already implemented summary injection; FAIL if replay does not preserve markers.

- [ ] **Step 3: Strengthen replay marker behavior**

In `replay.rs`, ensure the `CompressionApplied` arm stores marker fields exactly:

```rust
state.compactions.push(CompactionReplayMarker {
    event_index: stored.event_index,
    summary: summary.clone(),
    archive_path: archive_path.clone(),
    archived_event_range: *archived_event_range,
});
```

Ensure `apply_compaction_summary` never removes the system prompt:

```rust
fn apply_compaction_summary(messages: &mut Vec<Message>, summary: &str) {
    let system = messages.first().filter(|m| m.role == Role::System).cloned();
    messages.clear();
    if let Some(system) = system {
        messages.push(system);
    }
    messages.push(Message::assistant(format!("[Compaction summary]\n{}", summary)));
}
```

- [ ] **Step 4: Run tests**

Run:

```bash
cargo test -p holmes-session replay_injects_compaction_summary_and_marker
cargo check --workspace
```

Expected: both commands exit 0.

- [ ] **Step 5: Commit**

```bash
git add crates/holmes-session/src/replay.rs crates/holmes-session/tests/session_tests.rs
git commit -m "feat(session): replay compaction summaries"
```

---

## Task 10: Add Context Overflow Classification and Retry

**Files:**
- Modify: `crates/holmes-runtime/src/deliberation.rs`
- Modify: `crates/holmes-runtime/src/runtime.rs:178-182, 437-480`
- Test: `crates/holmes-runtime/src/deliberation.rs`
- Test: `crates/holmes-runtime/src/runtime.rs`

- [ ] **Step 1: Add failing overflow classification test**

In `crates/holmes-runtime/src/deliberation.rs` tests, add:

```rust
#[test]
fn llm_context_overflow_maps_to_context_overflow_kind() {
    let error = RuntimeError::from_llm_error(
        anyhow::anyhow!("context length exceeded maximum context window"),
        1,
    );
    assert_eq!(error.kind, RuntimeErrorKind::ContextOverflow);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run:

```bash
cargo test -p holmes-runtime llm_context_overflow_maps_to_context_overflow_kind
```

Expected: FAIL because `ContextOverflow` variant does not exist.

- [ ] **Step 3: Add `ContextOverflow` error kind**

Modify `RuntimeErrorKind`:

```rust
pub enum RuntimeErrorKind {
    Recoverable,
    NeedsUser,
    Fatal,
    ContextOverflow,
}
```

Modify `RuntimeError::from_llm_error`:

```rust
        if is_context_overflow_error(&message) {
            Self {
                kind: RuntimeErrorKind::ContextOverflow,
                message,
            }
        } else if configured_provider_count == 0 && is_missing_provider_error(&message) {
            Self::missing_provider()
        } else {
            Self::recoverable(message)
        }
```

Add helper:

```rust
fn is_context_overflow_error(message: &str) -> bool {
    let normalized = message.to_ascii_lowercase();
    normalized.contains("context length")
        || normalized.contains("context window")
        || normalized.contains("maximum context")
        || normalized.contains("too many tokens")
        || normalized.contains("prompt is too long")
}
```

Update any exhaustive matches on `RuntimeErrorKind` to include `ContextOverflow`. For `ReflectionEngine::assess_error`, treat it like recoverable:

```rust
RuntimeErrorKind::Recoverable | RuntimeErrorKind::Fatal | RuntimeErrorKind::ContextOverflow => { ... }
```

- [ ] **Step 4: Refactor runtime LLM call into retry helper**

In `runtime.rs`, add method near `run_turn`:

```rust
async fn decide_with_overflow_retry(
    &mut self,
    frame: &crate::perception::PerceptionFrame,
    sink: &mut dyn RuntimeSink,
) -> Result<crate::deliberation::DeliberationResult, RuntimeError> {
    match self.deliberation.decide(&self.context, frame).await {
        Ok(result) => Ok(result),
        Err(error) if error.kind == crate::deliberation::RuntimeErrorKind::ContextOverflow => {
            let original_message = error.message.clone();
            let compacted = self
                .compact_with_trigger(true, holmes_core::CompactionTrigger::Overflow)
                .await?;
            let Some(result) = compacted else {
                return Err(RuntimeError::recoverable(format!(
                    "context overflow and compaction produced no smaller context: {original_message}"
                )));
            };
            sink.emit_yield(&self.context.session_id, compaction_event(&result));
            let retry_frame = self.perception.perceive(&self.context);
            self.deliberation
                .decide(&self.context, &retry_frame)
                .await
                .map_err(|retry_error| {
                    RuntimeError::recoverable(format!(
                        "context overflow retry failed after compaction: {}; original overflow: {}",
                        retry_error.message, original_message
                    ))
                })
        }
        Err(error) => Err(error),
    }
}
```

- [ ] **Step 5: Use retry helper in `run_turn`**

Replace:

```rust
let deliberation = match self.deliberation.decide(&self.context, &frame).await {
    Ok(deliberation) => deliberation,
    Err(error) => return self.stop_for_error(error, iterations, sink),
};
```

With:

```rust
let deliberation = match self.decide_with_overflow_retry(&frame, sink).await {
    Ok(deliberation) => deliberation,
    Err(error) => return self.stop_for_error(error, iterations, sink),
};
```

- [ ] **Step 6: Add runtime test using a failing-then-success backend**

In `runtime.rs` tests, add a backend that returns context overflow first and final answer second. Use existing test helper patterns. The assertion should verify exactly one `CompressionApplied` event exists and outcome is `FinalAnswer`:

```rust
#[tokio::test]
async fn run_turn_compacts_and_retries_once_on_context_overflow() {
    let backend = Arc::new(SequenceLlmBackend::new(vec![
        Err(anyhow::anyhow!("context length exceeded maximum context window")),
        Ok(response_with_answer("after compaction")),
    ]));
    let mut runtime = make_runtime_with_backend_and_many_messages(backend).await;
    let mut sink = VecSink::default();

    let outcome = runtime.run_turn("continue", &mut sink).await.expect("retry succeeds");
    assert!(matches!(outcome, TurnOutcome::FinalAnswer { .. }));
    let events = runtime.context().session_db.get_events(runtime.context().session_id.as_str()).await.unwrap();
    assert_eq!(events.iter().filter(|event| matches!(event.event, Event::CompressionApplied { trigger: Some(holmes_core::CompactionTrigger::Overflow), .. })).count(), 1);
}
```

Use existing `response_with_answer` or add it following current runtime test helper style.

- [ ] **Step 7: Run tests**

Run:

```bash
cargo test -p holmes-runtime llm_context_overflow_maps_to_context_overflow_kind
cargo test -p holmes-runtime run_turn_compacts_and_retries_once_on_context_overflow
cargo check --workspace
```

Expected: all commands exit 0.

- [ ] **Step 8: Commit**

```bash
git add crates/holmes-runtime/src/deliberation.rs crates/holmes-runtime/src/runtime.rs
git commit -m "feat(runtime): compact and retry on context overflow"
```

---

## Task 11: Integrate Replay into CLI/TUI Resume Paths

**Files:**
- Modify: `crates/holmes-cli/src/chat.rs:1294-1340`
- Modify: `crates/holmes-cli/src/tui.rs` session switching / fork reload paths
- Test: `crates/holmes-cli/tests/workflow_integration.rs` or `slash_commands.rs`

- [ ] **Step 1: Add helper for loading session runtime through replay**

Replace or wrap `load_session_runtime` in `chat.rs` with:

```rust
pub(crate) async fn load_session_runtime(
    ctx: &ChatContext,
    session_id: &str,
    fallback_mode: SessionMode,
) -> anyhow::Result<(RuntimeSession, MindPalace)> {
    let replayed = ctx.session_db.replay_session_context(session_id).await?;
    let events = ctx.session_db.get_events(session_id).await?;
    let mut mind_palace = MindPalace::new(ctx.session_db.clone(), ctx.memory_store.clone());
    for stored in &events {
        mind_palace.ingest(stored.event.clone());
    }

    if replayed.semantic_complete {
        Ok((replayed.session, mind_palace))
    } else {
        let mut session = RuntimeSession::new(session_id.to_string(), fallback_mode)
            .with_system_prompt(&ctx.system_prompt);
        replay_events_into_runtime(&mut session, &mut mind_palace, &events);
        Ok((session, mind_palace))
    }
}
```

If a function with this name already exists, update its body to this implementation.

- [ ] **Step 2: Use helper for `resume_id` and `continue_last`**

In `create_chat_context`, replace manual replay blocks with calls to `load_session_runtime` after constructing a temporary/minimal `ChatContext` is not feasible. If helper needs full ctx, introduce a lower-level helper:

```rust
async fn load_session_runtime_from_store(
    session_db: Arc<dyn SessionStore>,
    memory_store: Arc<MemoryStore>,
    session_id: &str,
    fallback_mode: SessionMode,
    fallback_system_prompt: &str,
) -> anyhow::Result<(RuntimeSession, MindPalace, bool)> { ... }
```

Return the boolean as `semantic_complete` and print warning when false.

- [ ] **Step 3: Ensure TUI session switch uses same helper**

In `tui.rs`, find session switch/fork reload code that constructs `RuntimeSession::new(...).with_system_prompt(...)` directly. Replace with the shared `chat::load_session_runtime` or `load_session_runtime_from_store` helper.

- [ ] **Step 4: Add CLI integration test**

Add a test that creates a semantic session in an in-memory DB and calls the helper. If helper is not exported to tests, add a unit test in `chat.rs`:

```rust
#[tokio::test]
async fn load_session_runtime_from_store_prefers_semantic_replay() {
    let db: Arc<dyn SessionStore> = Arc::new(SessionDB::open(":memory:").await.unwrap());
    let memory = Arc::new(MemoryStore::open(":memory:").await.unwrap());
    let session = db.create_session(CreateSessionParams { id: Some("load_semantic".into()), title: None, mode: Some(SessionMode::Pentest), model: None, system_prompt: None, parent_session_id: None, fork_point: None, source: Some("test".into()), tags: vec![] }).await.unwrap();
    let now = chrono::Utc::now();
    db.append_event(&session.id, &Event::SessionCreated { id: session.id.clone(), title: None, mode: SessionMode::Pentest, model: Some("m".into()), system_prompt: Some("semantic system".into()), parent_id: None, fork_point: None, created_at: now, tags: vec![] }).await.unwrap();
    db.append_event(&session.id, &Event::SessionSystemPromptSet { prompt_hash: "h".into(), content: "semantic system".into(), source: "startup".into(), timestamp: now }).await.unwrap();
    db.append_event(&session.id, &Event::SessionModeSet { mode: SessionMode::Mixed, source: Some("startup".into()), timestamp: Some(now) }).await.unwrap();
    db.append_event(&session.id, &Event::SessionModelSet { model: "m".into(), provider: None, source: "startup".into(), timestamp: now }).await.unwrap();
    db.append_event(&session.id, &Event::ActiveToolsSet { tool_names: vec![], source: "startup".into(), timestamp: now }).await.unwrap();

    let (runtime, _mp, semantic_complete) = load_session_runtime_from_store(db, memory, &session.id, SessionMode::Pentest, "fallback").await.unwrap();
    assert!(semantic_complete);
    assert_eq!(runtime.mode, SessionMode::Mixed);
    assert_eq!(runtime.messages[0].content.as_deref(), Some("semantic system"));
}
```

- [ ] **Step 5: Run tests**

Run:

```bash
cargo test -p holmes-cli load_session_runtime_from_store_prefers_semantic_replay
cargo check --workspace
```

Expected: both commands exit 0.

- [ ] **Step 6: Commit**

```bash
git add crates/holmes-cli/src/chat.rs crates/holmes-cli/src/tui.rs crates/holmes-cli/tests/workflow_integration.rs crates/holmes-cli/tests/slash_commands.rs
git commit -m "feat(cli): resume sessions through semantic replay"
```

---

## Task 12: Update Harness and Long-Compression Tests

**Files:**
- Modify: `crates/holmes-harness/src/runner.rs`
- Modify: `scenarios/long-compression.yaml` only if the scenario needs expected event updates
- Test: `crates/holmes-harness/tests/scenario.rs`

- [ ] **Step 1: Write startup metadata in harness sessions**

In `crates/holmes-harness/src/runner.rs`, after `create_session`, append the same startup events used by CLI. Use deterministic source values:

```rust
let now = chrono::Utc::now();
session_db.append_event(&session.id, &Event::SessionCreated {
    id: session.id.clone(),
    title: session.title.clone(),
    mode: session.mode.clone(),
    model: scenario.config.llm.providers.first().map(|provider| provider.model.clone()),
    system_prompt: Some(system_prompt.clone()),
    parent_id: None,
    fork_point: None,
    created_at: now,
    tags: vec!["harness".into()],
}).await?;
session_db.append_event(&session.id, &Event::SessionSystemPromptSet {
    prompt_hash: "harness".into(),
    content: system_prompt.clone(),
    source: "startup".into(),
    timestamp: now,
}).await?;
session_db.append_event(&session.id, &Event::SessionModeSet {
    mode: session.mode.clone(),
    source: Some("startup".into()),
    timestamp: Some(now),
}).await?;
if let Some(model) = scenario.config.llm.providers.first().map(|provider| provider.model.clone()) {
    session_db.append_event(&session.id, &Event::SessionModelSet {
        model,
        provider: Some("harness".into()),
        source: "startup".into(),
        timestamp: now,
    }).await?;
}
session_db.append_event(&session.id, &Event::ActiveToolsSet {
    tool_names: scenario.tools.iter().map(|tool| tool.name.clone()).collect(),
    source: "startup".into(),
    timestamp: now,
}).await?;
```

Use actual variable names in `runner.rs` for session, scenario, and system prompt.

- [ ] **Step 2: Run harness scenarios**

Run:

```bash
cargo test -p holmes-harness scenario -- --nocapture
```

Expected: all scenario tests pass. If expected event counts changed, update only the affected scenario expectation fields to account for the five startup metadata events.

- [ ] **Step 3: Add replay assertion to long-compression scenario test**

If `scenario.rs` has access to the generated session id, assert replay contains a compaction marker for `long-compression.yaml`. If not, add a unit test in `holmes-runtime` that covers the same behavior and leave scenario expectations focused on event presence.

- [ ] **Step 4: Run workspace tests**

Run:

```bash
cargo test --workspace
```

Expected: all tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/holmes-harness/src/runner.rs scenarios/long-compression.yaml crates/holmes-harness/tests/scenario.rs
git commit -m "test(harness): include semantic session metadata"
```

---

## Task 13: Update Documentation

**Files:**
- Modify: `CLAUDE.md`
- Modify: `docs/HOLMES.md`
- Modify: `README.md` if command behavior or session semantics changed visibly

- [ ] **Step 1: Update `CLAUDE.md` session section**

Replace the current event sourcing paragraph with:

```markdown
### Event-sourced session semantic replay

New sessions are semantically replayable from events. Startup writes `SessionSystemPromptSet`, `SessionModeSet`, `SessionModelSet`, and `ActiveToolsSet` after `SessionCreated`; `/mode`, `/model`, and tool visibility changes append new semantic events. Use `SessionStore::replay_session_context(session_id)` for new resume/fork code instead of hand-replaying only `UserMessage`/`Thinking`/`ToolResult`.

Old sessions that lack startup semantic events are legacy. The replay API marks them `semantic_complete = false`; CLI code may fall back to legacy replay, but new code must not silently assume exact replay.

Compaction is event-sourced: `CompressionApplied` records summary, trigger, archive path, and archived event range. Raw compacted context is stored under `sessions/<session-id>/compactions/compaction_<event-index>.json`.
```

- [ ] **Step 2: Update `docs/HOLMES.md` Event Sourcing and Compaction sections**

Add the same facts in the architecture section:

```markdown
Semantic replay now includes session metadata, not only messages: system prompt, mode, model, active tools, branch summaries, and compaction markers. `BranchSummary` carries fork context into new branches. `CompressionApplied` points to a compaction archive file so replay can inject the summary while preserving raw history for audit.
```

- [ ] **Step 3: Run documentation grep check**

Run:

```bash
grep -rn "agent_loop.rs\|before_event_persist.*no call site\|CompressionMethod::StaticFallback.*only" CLAUDE.md docs/HOLMES.md README.md || true
```

Expected: no stale claims about removed `agent_loop.rs` or orphaned `before_event_persist`. StaticFallback may still be mentioned only if it says LLM summary is not implemented for a specific path.

- [ ] **Step 4: Commit**

```bash
git add CLAUDE.md docs/HOLMES.md README.md
git commit -m "docs: document semantic session replay"
```

---

## Task 14: Final Verification

**Files:**
- No source changes expected unless tests expose a bug.

- [ ] **Step 1: Run focused test matrix**

Run:

```bash
cargo test -p holmes-core semantic_events_have_text_and_categories
cargo test -p holmes-session replay_complete_semantic_session_restores_metadata_and_messages
cargo test -p holmes-session compaction_archive_round_trips_through_session_store
cargo test -p holmes-runtime llm_context_overflow_maps_to_context_overflow_kind
cargo test -p holmes-cli load_session_runtime_from_store_prefers_semantic_replay
cargo test -p holmes-harness scenario -- --nocapture
```

Expected: all commands exit 0.

- [ ] **Step 2: Run full workspace tests**

Run:

```bash
cargo test --workspace
```

Expected: all tests pass.

- [ ] **Step 3: Run compile check**

Run:

```bash
cargo check --workspace
```

Expected: exits 0.

- [ ] **Step 4: Inspect event type coverage**

Run:

```bash
grep -n "SessionSystemPromptSet\|SessionModelSet\|ActiveToolsSet\|BranchSummary\|CompressionApplied" crates/holmes-core/src/event.rs crates/holmes-session/src/db.rs crates/holmes-session/src/replay.rs crates/holmes-runtime/src/runtime.rs crates/holmes-cli/src/chat.rs
```

Expected: each new event appears in `event.rs`, `db.rs` event type mapping, replay, and at least one writer path.

- [ ] **Step 5: Confirm clean task branch**

Run:

```bash
git status --short
```

Expected: no untracked or modified source files after final commits.

---

## Self-Review

### Spec coverage

- Metadata events: Task 1, Task 4, Task 5.
- `SessionStore::replay_session_context`: Task 3, Task 11.
- Branch summary: Task 6, Task 7.
- Compaction-as-event and archive DTO: Task 2, Task 8, Task 9.
- Overflow compact+retry: Task 10.
- CLI/TUI integration: Task 4, Task 5, Task 7, Task 11.
- Harness/tests/docs: Task 12, Task 13, Task 14.
- No migration of old sessions: Task 3 and Task 11 mark incomplete sessions and preserve fallback.

### Placeholder scan

This plan intentionally avoids open implementation decisions. It defines concrete types, methods, fields, test code, commands, and commits for each task.

### Type consistency

The plan uses these names consistently:

- `SummaryMethod::{Llm, StaticFallback}`
- `CompactionTrigger::{Manual, Threshold, Overflow}`
- `SessionStore::replay_session_context`
- `SessionStore::write_compaction_archive`
- `SessionStore::read_compaction_archive`
- `ReplayedSessionContext`
- `CompactionReplayMarker`
- `CompactionArchive`
- `ArchivedEvent`

The existing `Event::CompressionApplied` variant is extended with optional fields instead of replaced, preserving old event deserialization.
