# Holmes Agent — Technical Documentation

**Version**: 0.1.0
**Date**: 2026-06-18
**License**: Internal

---

## Table of Contents

1. [Overview](#1-overview)
2. [Quick Start](#2-quick-start)
3. [Architecture](#3-architecture)
4. [CLI Usage](#4-cli-usage)
5. [Slash Commands](#5-slash-commands)
6. [Core Concepts](#6-core-concepts)
7. [API Reference](#7-api-reference)
8. [Configuration](#8-configuration)
9. [Development](#9-development)

---

## 1. Overview

Holmes is a CLI-native AI agent for penetration testing, security research, code auditing, and reverse engineering. It combines:

- **Turn-based conversation** — User (Watson) and Holmes collaborate through natural language
- **Workflow + Selector** — OpenRath-inspired composable workflows with LLM-driven routing
- **Mind Palace** — Unified memory + context awareness + dashboard cognitive system
- **Event Sourcing** — Every state change is an immutable event, enabling full session replay
- **RuntimeYield Stream** — SDK-like structured stream for messages, permissions, tools, compaction, and results
- **PermissionPolicy** — Central tool authorization layer with default, plan, read-only, dont-ask, and bypass modes
- **GuardChain** — Configurable pre/post guards for safety, evidence extraction, loop prevention, and read-state tracking
- **Harness** — Agent OS testbench for deterministic runtime scenarios
- **Core tools + MCP** — Shell execution, HTTP requests, Python scripting, reporting, hypotheses, optional subagents, and MCP-backed tools

### Design Metaphor

| Sherlock Holmes | Holmes Agent |
|----------------|-------------|
| Mind Palace | Memory + Context + Dashboard cognitive system |
| Deduction | Reflection + HypothesisTracker + Advisor reasoning |
| Investigation | Tool calls + information gathering + analysis loops |
| Watson | User — CLI conversation is the collaboration |
| Case | A Session — with goals, clues, and conclusions |
| 221B Baker Street | SessionDB — persistent case archive |
| Scotland Yard | Tool system / MCP — executing concrete actions |
| Baker Street Irregulars | Sub-agents — Scout, Analyst, Operative, Ghost, Chronicler |

### Use Cases

| Mode | Typical Tasks |
|------|--------------|
| `pentest` | Reconnaissance, vulnerability scanning, exploitation, privilege escalation, lateral movement |
| `code_audit` | Code review, taint tracking, vulnerability pattern matching, audit progress tracking |
| `reverse` | Binary analysis, disassembly, protocol reversing, algorithm identification |
| `security_research` | Hypothesis-driven exploration, cross-source correlation, reproducible research |
| `mixed` | All of the above |

---

## 2. Quick Start

### Prerequisites

- Rust toolchain (1.80+)
- An Anthropic API key, or an Anthropic-compatible `/v1/messages` endpoint

### Installation

```bash
cd holmes
cargo build --release
```

### First Run — Setup Wizard

```bash
./target/release/holmes setup
```

The wizard guides you through provider selection, API key entry, and model discovery:

```
╔══════════════════════════════════════════════╗
║  Holmes Setup — LLM Provider Configuration   ║
╚══════════════════════════════════════════════╝

Available providers:
  1. Anthropic (Claude)
  2. Custom Anthropic-compatible endpoint

Select provider [1-2]: 1
✓ Found ANTHROPIC_API_KEY in environment (sk-ant-...)

✓ Fetched 5 available models from API:
  1. claude-sonnet-4-6 ← default
  2. claude-opus-4-8
  3. claude-haiku-4-5
  4. claude-fable-5

Select model [1-4, or type a custom model name]: 

✓ Configuration saved to ~/.local/share/holmes/config.yaml
Setup complete! Run 'holmes' to start.
```

**Provider auto-detection**: Holmes automatically detects API keys from environment variables such as `ANTHROPIC_API_KEY` and `HOLMES_API_KEY`. Custom endpoints are spoken to through the Anthropic Messages protocol.

**Model auto-discovery**: Holmes queries the provider's model endpoint before saving configuration. Authentication failures stop setup instead of writing a broken config.

### Launch

```bash
./target/release/holmes
```

---

## 3. Architecture

### Crate Map

```
holmes/
├── crates/
│   ├── holmes-core/       # Event enum (40+ variants), types, config, RuntimeSession, Workflow trait
│   ├── holmes-session/    # SessionDB (SQLite + FTS5), MemoryStore, Selector
│   ├── holmes-llm/        # Anthropic Messages client, failover, rate limiting
│   ├── holmes-runtime/    # Agent loop, RuntimeYield stream, permissions, deduction, learning, compaction
│   ├── holmes-tools/      # Tool trait, ToolRegistry, built-in tools, MCP integration
│   ├── holmes-guards/     # GuardChain: configurable pre/post guards
│   ├── holmes-mind-palace/# Memory Layer + Context Layer + Dashboard Layer
│   ├── holmes-harness/    # Deterministic agent OS testbench
│   └── holmes-cli/        # CLI binary: full-screen TUI, legacy REPL, slash commands, setup wizard
```

### Dependency Graph

```
holmes-core  ← 被所有 crate 依赖 (基础类型)
    │
    ├── holmes-session    ← holmes-core (SessionDB + MemoryStore)
    │   └── holmes-llm    ← 被 holmes-session 依赖 (Selector 调用 LLM)
    │
    ├── holmes-tools      ← holmes-core
    │   └── holmes-guards  ← holmes-core + holmes-tools
    │
    ├── holmes-mind-palace ← holmes-core + holmes-session
    │
    └── holmes-cli        ← 所有 crate (组装一切)
```

### Data Flow

```
User Input
    │
    ▼
┌─────────────────────────────────────────────────────┐
│  holmes-cli (Reedline REPL / full-screen TUI)       │
│                                                     │
│  /slash command? → CommandRegistry.dispatch()       │
│  normal message  → AgentRuntime::run_turn()         │
│  one-shot query  → AgentRuntime::run_oneshot()      │
│                                                     │
│           Unified Runtime Loop                        │
│           ┌──────────────────────────┐               │
│           │ LlmClient.chat_completion│               │
│           │        ↓                  │               │
│           │ PermissionPolicy.evaluate│               │
│           │        ↓                  │               │
│           │ GuardChain.run_pre()     │               │
│           │        ↓                  │               │
│           │ ToolRegistry.execute()   │               │
│           │        ↓                  │               │
│           │ GuardChain.run_post()    │               │
│           │        ↓                  │               │
│           │ RuntimeYield stream      │               │
│           └──────────────────────────┘               │
│                   │                                   │
│                   ▼                                   │
│           MindPalace.ingest(event)                    │
│           SessionDB.append_event()                    │
└─────────────────────────────────────────────────────┘
```

Selector/workflow code remains available for compatibility and tests, but normal interactive turns and one-shot queries use `holmes-runtime::AgentRuntime` as the primary path.

### Event Sourcing

All state changes are immutable `Event` records stored in SQLite. This means:

- **Full replay**: Resume any session from any point
- **Branching**: Fork sessions at any event index
- **Auditability**: Every tool call, finding, and decision is traceable
- **Recovery**: Crash-safe — state is reconstructed from events

### Session Semantic Replay

New sessions are *semantically* replayable, not just message-replayable. At
session start Holmes appends `SessionCreated`, `SessionSystemPromptSet`,
`SessionModeSet`, `SessionModelSet`, and `ActiveToolsSet`; `/mode`, `/model`,
and MCP/tool-visibility changes append further semantic events. Use
`SessionStore::replay_session_context(session_id)` to rebuild a session — it
returns a `ReplayedSessionContext` carrying the reconstructed `RuntimeSession`
plus system prompt, model, active tools, branch summaries, compaction markers,
and a `semantic_complete` flag.

Old sessions created before this metadata existed are reported as
`semantic_complete = false`; the CLI falls back to legacy event replay for
them. New CLI resume/continue/`/resume`/rollback-rebuild paths all reconstruct
through semantic replay.

Forking is semantic too: `fork_session` excludes the parent's lifecycle
metadata, materializes the parent's mode/model/system-prompt at the fork point
(so a fork after `/mode research` keeps the research mode), preserves original
event indices (so `CompressionApplied.archived_event_range` stays valid), and a
`BranchSummary` event bridges the abandoned path's context into the child.

### Compaction (event-sourced, archive-backed)

Compaction no longer destructively discards history. Each compaction:

- writes the summarized-away messages and events to an external archive at
  `sessions/<session-id>/compactions/compaction_<event-index>.json`
- appends a `CompressionApplied` event recording the summary, `trigger`
  (`Manual` / `Threshold` / `Overflow`), `archived_event_range`, and the
  archive pointer
- is applied during replay by replacing the archived range with the summary
  message (raw history stays on disk for audit; a missing archive degrades to
  the event summary with a warning, never a block)

The archive is written **before** the event is appended, so a persisted
`CompressionApplied` always points at a readable archive.

### Context-overflow self-healing

When the LLM returns a context-overflow error, the runtime classifies it as
`RuntimeErrorKind::ContextOverflow`, runs one `Overflow`-triggered compaction,
and retries the same decision exactly once. If the retry still fails it returns
a clear error (no retry loop).

---


## 4. CLI Usage

### Commands

```bash
holmes                      # Start full-screen TUI with a fresh session
holmes chat                 # Same default full-screen TUI
holmes tui                  # Explicit full-screen TUI
holmes repl                 # Legacy Reedline line REPL
holmes --repl               # Same legacy REPL through the default command
holmes sessions             # List recent sessions
holmes setup                # Interactive LLM provider configuration
holmes version              # Show version

# Session management
holmes -r <id>              # Resume a specific session
holmes -c                   # Continue the most recent session; Holmes does not auto-continue
holmes -q "query"           # One-shot non-interactive query
holmes -m <model>           # Override model
holmes --mode audit         # Set session mode
```

### Full-Screen TUI

Holmes starts in a Pi-style full-screen TUI backed by the same `AgentRuntime` as one-shot and Reedline chat. It keeps the chat transcript, editor, session navigation, event timeline, permissions, and guards in one terminal app shell.

By default, `holmes` starts a fresh session. Use `holmes -c` to continue the most recent session or `holmes -r <id>` to resume a specific case.

```text
Holmes TUI  session=3f2a1c8d  mode=Pentest  permission=default
--------------------------------------------------------------------------------
Watson: 扫描 target.com 的开放端口

Holmes: 我会先确认授权范围，然后选择只读探测。

Tool:   http_request ok - output folded (512 chars, 12 lines)

--------------------------------------------------------------------------------
Watson > /dashboard
Ctrl+L commands  Ctrl+N new  Ctrl+B fork  Ctrl+O tool output
F1 help  F2 tree  F3 events  F4 permissions  F5 guards  F6 sessions
```

| Key | Action |
|-----|--------|
| `F1` | Help and keybinding reference |
| `F2` | Searchable session tree; `Enter` resumes, `f` forks selected session |
| `F3` | Current session event timeline; `Enter`/`f` forks from selected event |
| `F4` | PermissionPolicy panel; cycle modes, edit allow/deny lists |
| `F5` | GuardChain panel; toggle guards, adjust repetition window |
| `F6` | Flat recent-session selector |
| `Ctrl+L` | Slash command palette |
| `Ctrl+N` | New session |
| `Ctrl+B` | Fork current session at latest event |
| `Ctrl+O` | Toggle folded/full tool output |
| `PgUp` / `PgDn` | Scroll transcript |

The TUI follows Pi's session-tree semantics: switching sessions does not mark the previous session as ended; it simply moves the current leaf.

### Legacy REPL

The Reedline REPL remains available for line-oriented workflows:

```bash
holmes repl
holmes --repl
```

Type `/` and press `Tab` to see slash commands and completions.

### One-shot Mode

```bash
holmes -q "审计 src/auth/login.php 的 SQL 注入风险"
```

---

## 5. Slash Commands

All commands are available in the REPL by typing `/` followed by the command name.

### Session Management

| Command | Aliases | Description |
|---------|---------|-------------|
| `/new` | `/reset` | End current session, create new one |
| `/clear` | — | Clear screen + new session |
| `/resume <id\|title>` | — | Switch to another session |
| `/sessions` | `/history` | List recent sessions |
| `/session` | — | Show current session details |
| `/rename <title>` | `/title` | Rename current session |
| `/branch [title]` | `/fork` | Fork new session from the latest event and switch to it |
| `/tree` | — | Show the session tree |
| `/tree events [limit]` | — | Show the current session event timeline |
| `/tree fork <event_index> [title]` | — | Fork from a specific event and switch to it |
| `/compress` | `/compact` | Manually trigger context compression |
| `/retry` | — | Discard last turn, re-answer |
| `/undo` | — | Undo last turn (back to user input) |
| `/save` | `/export` | Export session as JSON |
| `/quit` | `/exit`, `/q` | Exit Holmes |

### Goal System

| Command | Description |
|---------|-------------|
| `/goal <condition>` | Set autonomous completion goal |
| `/goal` | Show current goal status |
| `/goal clear` | Clear active goal |

Goal behavior:
- Holmes works autonomously across multiple turns
- After each turn, an independent evaluator model checks the condition
- Automatically clears when satisfied
- Supports `or stop after N turns` stop clauses

### Configuration & Model

| Command | Description |
|---------|-------------|
| `/model [name\|list]` | View or switch models |
| `/provider` | Show current provider info |
| `/mode <pentest\|audit\|reverse\|research>` | Switch working mode |
| `/config` | Show current configuration |
| `/config set <key> <value>` | For safety settings, use `/permissions` or `/guards`; edit YAML for other keys |

### Safety Controls

| Command | Description |
|---------|-------------|
| `/permissions` | Explain the current permission mode, allow list, deny list, and read-only auto-approval |
| `/permissions mode <default\|plan\|read-only\|accept-edits\|dont-ask\|bypass>` | Switch tool authorization mode |
| `/permissions allow <tool\|prefix*\|*suffix>` | Add an allow pattern |
| `/permissions deny <tool\|prefix*\|*suffix>` | Add a deny pattern |
| `/permissions remove <allow\|deny> <pattern>` | Remove a policy pattern |
| `/permissions auto-read-only <on\|off>` | Toggle automatic approval for read-only tools |
| `/permissions reset` | Restore default permission policy |
| `/guards` | Explain enabled guard checks |
| `/guards enable <guard-name>` | Enable one guard |
| `/guards disable <guard-name>` | Disable one guard |
| `/guards all <on\|off>` | Enable or disable all guard checks |
| `/guards window <count>` | Set the repetition guard window |

### Tools

| Command | Description |
|---------|-------------|
| `/tools` | List available tools |
| `/tools <name>` | Show tool details |
| `/mcp` | Show MCP server status |
| `/mcp reload` | Hot-reload MCP servers |

### Info & Status

| Command | Description |
|---------|-------------|
| `/help` | Show all available commands |
| `/status` | Session status (ID, mode, turns, tokens) |
| `/dashboard` | Show current dashboard |
| `/usage` | Token usage and cost statistics |
| `/version` | Show version |

### Workflow Control

| Command | Description |
|---------|-------------|
| `/workflows` | List available workflows |
| `/workflow <name>` | Manually trigger a workflow |
| `/chat` | Switch back to chat mode |

### Direct Tool Invocation

| Syntax | Description |
|--------|-------------|
| `!<command>` | Execute shell command directly |
| `!!` | Repeat last command |

---

## 6. Core Concepts

### RuntimeSession

`RuntimeSession` is the flowing runtime value — analogous to PyTorch's `Tensor`. It carries messages, lineage, token accounting, and context as it passes through Workflows.

```rust
pub struct RuntimeSession {
    pub id: String,
    pub title: Option<String>,
    pub mode: SessionMode,
    pub messages: Vec<Message>,       // Current conversation
    pub lineage: SessionLineage,      // Fork/detach/merge tracking
    pub tokens: TokenDelta,           // Real-time token accounting
    pub context: ContextSnapshot,     // Current situational snapshot
    pub created_at: DateTime<Utc>,
}
```

Key operations:
- `fork()` — Create a branch with new ID, sharing lineage
- `detach()` — Cut parent lineage (become a root)
- `merge(other)` — Combine unique messages from another session
- `with_system_prompt(prompt)` / `with_user_message(content)` — Convenience builders

### Workflow

A `Workflow` is a composable unit of agent work — analogous to PyTorch's `nn.Module`. Each implements `forward(session) -> session`.

```rust
#[async_trait]
pub trait Workflow: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    async fn forward(&self, session: &mut RuntimeSession) -> Result<(), WorkflowError>;
}
```

**Built-in Workflows:**

| Workflow | Description | When Used |
|----------|-------------|-----------|
| `ChatWorkflow` | General Q&A, task clarification, planning | Default fallback |
| `ReconWorkflow` | Port scanning, subdomain enumeration, tech detection | Information gathering |
| `AnalysisWorkflow` | Code audit, vulnerability analysis, attack surface mapping | Deep analysis |
| `ExploitWorkflow` | Exploitation, privilege escalation, lateral movement | Active attack |
| `ReportWorkflow` | Report generation, dashboard updates | Output/deliverables |

### Selector

The `Selector` is an LLM-driven router that replaces hardcoded if/while with dynamic workflow selection. After each workflow completes, the Selector evaluates the session state and chooses the next workflow — or returns `DONE` to hand control back to the user.

```rust
let mut selector = Selector::new();
selector.register(Box::new(ChatWorkflow::new(...)));
selector.register(Box::new(ReconWorkflow::new(...)));

// In the REPL loop:
while let Some(name) = selector.select(&session, &llm).await? {
    let wf = selector.get(&name).unwrap();
    wf.forward(&mut session).await?;
}
```

### Mind Palace

The Mind Palace is Holmes' unified cognitive system with three layers:

```
┌─────────────────────────────────────────────┐
│ Dashboard Layer (呈现面)                     │
│ · Attack surface map                        │
│ · Findings timeline                         │
│ · Goal progress                             │
│ · Network topology                          │
│ · Audit/reverse progress                    │
├─────────────────────────────────────────────┤
│ Context Layer (认知面)                       │
│ · Current situation awareness               │
│ · Active context stack                      │
│ · Context switching                         │
│ · Context compression                       │
│ · Situation summary for LLM injection       │
├─────────────────────────────────────────────┤
│ Memory Layer (存储面)                        │
│ · Short-term: Session event stream          │
│ · Long-term: Cross-session knowledge        │
│ · Associative retrieval (FTS5)              │
│ · Memory consolidation                      │
└─────────────────────────────────────────────┘
```

### GuardChain

GuardChain is assembled from `guards:` config and can be inspected or changed from the TUI with `/guards`:

| Guard | Type | Function |
|-------|------|----------|
| `ImmutableFieldGuard` | Pre | Blocks requests to non-target hosts/IPs |
| `DangerousCommandGuard` | Pre | Blocks destructive commands (`rm -rf`, fork bombs, container escapes) |
| `RepetitionGuard` | Pre | Blocks the 4th repeat of the same semantic signature |
| `FileTrackerPreGuard` | Pre | Seeds read/write state before file-affecting tools |
| `AttackSurfaceUpdater` | Post | Extracts ports, services, tech stack, endpoints |
| `EvidenceExtractor` | Post | Extracts credentials, object references |
| `SkepticGate` | Post | Validates findings against action history |
| `FailureTracker` | Post | Tracks consecutive failures |
| `Soft404Detector` | Post | Fingerprints soft-404 responses |
| `FileTrackerPostGuard` | Post | Records read/write effects after file-affecting tools |

### Event System

All state changes are immutable `Event` records. The `Event` enum has 40+ variants across 12 categories:

| Category | Events |
|----------|--------|
| Session | `SessionCreated`, `SessionEnded`, `SessionModeSet` |
| Turn | `UserMessage`, `TurnComplete` |
| Goal | `GoalSet`, `GoalEvaluated`, `GoalCleared`, `GoalProgress`, `SubtaskUpdate` |
| Action | `Thinking`, `ToolCall`, `ToolResult`, `ToolBlocked` |
| Situational | `TargetDiscovered`, `AttackSurfaceUpdate`, `VulnerabilityFound`, `CodePatternFound`, `ReverseInsight`, `CredentialFound`, `HostCompromised`, `LateralMovement`, `NetworkTopologyUpdate` |
| Strategy | `DirectiveSet`, `ReflectionRecorded`, `HypothesisUpdate`, `AdvisorAction` |
| Mind Palace | `MemoryStored`, `MemoryRecalled`, `MemoryConsolidated`, `ContextSnapshotTaken`, `ContextSwitched`, `DashboardUpdated` |
| Context | `CompressionApplied` |
| Injection | `SkillInjected`, `KnowledgeInjected`, `HumanFeedback` |
| SubAgent | `SubAgentSpawned`, `SubAgentCompleted`, `SubAgentProgress` |
| Report | `ReportGenerated` |

### SessionDB

SQLite + FTS5 for persistent session storage. Key features:
- **WAL mode** with write contention retry (15 attempts, 20-150ms jitter)
- **FTS5 full-text search** across events with CJK LIKE fallback
- **Prefix resolution** for session IDs
- **Transactional fork** — copies events atomically
- **Periodic WAL checkpoint** every 50 writes

### Tool System

The CLI registers these core tools, then extends them with optional subagent and MCP tools:

| Tool | Description | Read-only |
|------|-------------|-----------|
| `execute_command` | Shell execution via `sh -c`, 30s timeout, 32KB output cap | No |
| `execute_python` | Python3 scripting with pre-imported libraries | No |
| `http_request` | HTTP client with redirects, self-signed cert acceptance | Yes |
| `report_recon` | Structured Phase 1 recon report | — |
| `report_finding` | Vulnerability finding (routed through SkepticGate) | Yes |
| `report_progress` | Manual progress signal | Yes |
| `add_hypothesis` | Register attack hypothesis | No |
| `reject_hypothesis` | Reject active hypothesis | No |
| `confirm_hypothesis` | Confirm active hypothesis with evidence | No |
| `spawn_subagent` | Delegate a task to a nested Holmes runtime when subagents are enabled | No |

The browser tool module is still present for future registration, but it is not part of the current default `register_all` path.

### LLM Layer

Multi-provider HTTP client with:
- **Anthropic Messages** (`/v1/messages`, `x-api-key` + `anthropic-version`) as the single wire protocol
- **Failover chain** — providers sorted by priority, marked unhealthy on failure
- **Rate limiting** — per-provider RPM caps
- **Exponential backoff retry** (500ms × 2^n) with error classification
- **Role-based routing** — `attack_agent` / `supervisor` / `compressor` / `skill_evolver` / `goal_evaluator` each map to a provider

---

## 7. API Reference

### holmes-core

#### Event (event.rs)

```rust
pub enum Event {
    SessionCreated { id, title?, mode, model?, system_prompt?, parent_id?, fork_point?, created_at, tags },
    SessionEnded { reason, summary? },
    UserMessage { content, timestamp },
    TurnComplete { event_range, tokens_used, sub_agents_spawned },
    GoalSet { condition, plan?, subtasks },
    GoalEvaluated { satisfied, reason, turn_count, tokens_spent },
    Thinking { content, reasoning_type? },
    ToolCall { name, arguments, purpose? },
    ToolResult { name, success, content, error?, artifacts },
    ToolBlocked { tool_name, guard_name, reason },
    TargetDiscovered { kind, details, confidence, source },
    AttackSurfaceUpdate { hosts, services, tech_stack, endpoints, credentials, notes? },
    VulnerabilityFound { title, cwe?, cvss?, severity, location, evidence, poc?, status },
    CodePatternFound { pattern_type, file, line_range?, snippet, risk_assessment, language? },
    ReverseInsight { insight_type, description, confidence, addresses },
    CredentialFound { username, credential_type, source_host, context, cracked? },
    HostCompromised { host, access_level, method, persistence?, session_id? },
    LateralMovement { from_host, to_host, method, credentials_used?, timestamp },
    NetworkTopologyUpdate { subnets, hosts, relationships, trust_paths, domain_info? },
    // ... (30+ more variants)
}
```

Key methods:
- `content_text() -> String` — human-readable text for FTS5 indexing
- `category() -> &str` — coarse bucket for querying
- `is_turn_start() / is_turn_end() -> bool`

#### RuntimeSession (session.rs)

```rust
impl RuntimeSession {
    pub fn new(id: String, mode: SessionMode) -> Self;
    pub fn fork(&self) -> Self;
    pub fn detach(&mut self);
    pub fn merge(&mut self, other: &Self);
    pub fn with_system_prompt(self, prompt: &str) -> Self;
    pub fn with_user_message(self, content: &str) -> Self;
    pub fn message_count(&self) -> usize;
    pub fn snapshot(&self) -> ContextSnapshot;
    pub fn update_context(&mut self, ctx: ContextSnapshot);
}
```

#### Workflow (workflow.rs)

```rust
#[async_trait]
pub trait Workflow: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    async fn forward(&self, session: &mut RuntimeSession) -> Result<(), WorkflowError>;
}

pub enum WorkflowError {
    Llm(String),
    Tool(String),
    GuardBlocked(String),
    Other(String),
}
```

### holmes-session

#### SessionDB (db.rs)

```rust
impl SessionDB {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, SessionError>;
    pub async fn create_session(&self, params: CreateSessionParams) -> Result<Session>;
    pub async fn append_event(&self, session_id: &str, event: &Event) -> Result<u64>;
    pub async fn get_events(&self, session_id: &str) -> Result<Vec<StoredEvent>>;
    pub async fn list_sessions(&self, filter: &SessionFilter) -> Result<Vec<SessionSummary>>;
    pub async fn end_session(&self, id: &str, reason: EndReason) -> Result<()>;
    pub async fn reopen_session(&self, id: &str) -> Result<()>;
    pub async fn get_session(&self, id: &str) -> Result<Option<Session>>;
    pub async fn fork_session(&self, id: &str, fork_point: u64, new_title: &str) -> Result<Session>;
    pub async fn update_token_counts(&self, id: &str, delta: &TokenDelta) -> Result<()>;
    pub async fn set_title(&self, id: &str, title: &str) -> Result<()>;
    pub async fn search_events(&self, query: &str, top_k: u32) -> Result<Vec<SearchResult>>;
}
```

#### Selector (selector.rs)

```rust
impl Selector {
    pub fn new() -> Self;
    pub fn register(&mut self, workflow: Box<dyn Workflow>);
    pub fn get(&self, name: &str) -> Option<&dyn Workflow>;
    pub fn workflow_names(&self) -> Vec<&str>;
    pub async fn select(
        &self, session: &RuntimeSession, llm: &LlmClient
    ) -> Result<Option<String>, WorkflowError>;
}
```

#### MemoryStore (memory_store.rs)

```rust
impl MemoryStore {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, rusqlite::Error>;
    pub async fn store(&self, entry: MemoryEntry) -> Result<String, rusqlite::Error>;
    pub async fn search(&self, query: &str, top_k: u32) -> Result<Vec<Memory>, rusqlite::Error>;
    pub async fn consolidate(
        &self, from_ids: &[String], into_content: &str, into_tags: &[String]
    ) -> Result<String, rusqlite::Error>;
}
```

### holmes-llm

#### LlmClient (client.rs)

```rust
impl LlmClient {
    pub fn new(config: &Config) -> Self;
    pub async fn chat_completion(
        &self, messages: &[Message], tools: &[ToolDefinition], role: &str
    ) -> Result<LlmResponse>;
    pub async fn chat_completion_oneshot(
        &self, system: &str, user: &str, role: &str
    ) -> Result<LlmResponse>;
}
```

### holmes-tools

#### Tool (registry.rs)

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn definition(&self) -> ToolDefinition;
    fn is_read_only(&self) -> bool;
    async fn execute(&self, args: &str) -> Result<String>;
}
```

#### ToolRegistry (registry.rs)

```rust
impl ToolRegistry {
    pub fn new() -> Self;
    pub fn register(&mut self, tool: Box<dyn Tool>);
    pub fn definitions(&self) -> Vec<ToolDefinition>;
    pub async fn execute(&self, call: &ToolCall) -> ToolResult;
    pub fn can_parallelize(&self, calls: &[ToolCall]) -> bool;
}
```

### holmes-guards

#### GuardChain (lib.rs)

```rust
impl GuardChain {
    pub fn new() -> Self;
    pub fn from_config(config: &GuardConfig) -> Self;
    pub async fn run_pre(&self, call: &ToolCall, state: &AttackState) -> GuardVerdict;
    pub async fn run_post(&mut self, call: &ToolCall, result: &ToolResult, state: &mut AttackState);
}
```

### holmes-mind-palace

#### MindPalace (lib.rs)

```rust
impl MindPalace {
    pub fn new(session_db: Arc<SessionDB>, long_term: Arc<MemoryStore>) -> Self;
    pub async fn from_events(
        session_id: &str, session_db: Arc<SessionDB>, long_term: Arc<MemoryStore>
    ) -> Result<Self, String>;
    pub fn ingest(&mut self, event: Event);
    pub fn dashboard(&self, mode: &SessionMode) -> DashboardSnapshot;
    pub fn situation_summary(&self) -> String;
    pub fn snapshot(&self) -> ContextSnapshot;
    pub fn compress(&mut self);
}
```

---

## 8. Configuration

Config file location: `~/.local/share/holmes/config.yaml` (created by `holmes setup`)

```yaml
agent:
  max_iterations: 90
  no_tool_threshold: 3
  hypothesis_budget: 8
  reflection_threshold: 5
  reflection_cooldown: 3
  stale_threshold: 8
  force_pivot_threshold: 15
  generate_reports: true

permissions:
  # YAML values: default | plan | read_only | accept_edits | dont_ask | bypass
  # TUI aliases: /permissions mode read-only | accept-edits | dont-ask
  mode: default
  allowed_tools: []
  disallowed_tools: []
  auto_approve_read_only: true

llm:
  providers:
    - name: anthropic
      base_url: "https://api.anthropic.com"
      api_key: "sk-ant-..."     # or set ANTHROPIC_API_KEY env var
      model: "claude-sonnet-4-6"
      api_format: anthropic
      priority: 0
      max_retries: 3
      rpm_limit: 50

  roles:
    attack_agent: anthropic     # Main agent model
    supervisor: anthropic       # Strategy advisor
    compressor: anthropic       # Context summarizer
    skill_evolver: anthropic    # Skill generation
    goal_evaluator: anthropic   # Goal condition evaluator

compressor:
  context_limit: 128000
  threshold: 0.75
  protected_head: 3
  protected_tail_tokens: 4000

advisor:
  enabled: true
  auto_apply_nudge: true
  auto_apply_suggest: false

guards:
  immutable_field: true
  dangerous_command: true
  repetition: true
  repetition_window: 10
  attack_surface: true
  evidence_extractor: true
  skeptic_gate: true
  failure_tracker: true
  soft404: true
  read_state_seeding: true

memory:
  db_path: data/memory.db
  consolidation_threshold: 0.85

skills:
  dir: skills
  auto_inject: true

browser:
  enabled: false
  vision: false
  headless: true

output_dir: output
```

---

## 9. Development

### Build

```bash
cargo build --release          # Optimized binary
cargo check --workspace        # Fast compile check (0 warnings expected)
cargo test --workspace         # Run all tests (130+ tests, 0 failures expected)
```

### Project Structure

```
holmes/
├── Cargo.toml                 # Workspace root
├── config.default.yaml        # Reference config template
├── skills/                    # Skill library (from Apeiron V3)
└── crates/
    ├── holmes-core/           # Base types, events, config, session, workflow trait
    ├── holmes-session/        # SessionDB, MemoryStore, Selector
    ├── holmes-llm/            # Multi-provider LLM client
    ├── holmes-tools/          # Tool trait, ToolRegistry, built-in tools, MCP
    ├── holmes-guards/         # GuardChain (configurable pre/post guards)
    ├── holmes-mind-palace/    # Mind Palace (memory + context + dashboard)
    └── holmes-cli/            # CLI binary (full-screen TUI, legacy REPL, setup, slash commands)
```

### Key Design Decisions

1. **Event Sourcing**: All state is derived from events. No mutable state survives a crash.
2. **Session as Runtime Value**: `RuntimeSession` carries messages, lineage, and tokens — like a tensor flowing through layers.
3. **Workflow Composition**: `Workflow::forward(session) -> session` enables clean composition. `Selector` replaces hardcoded routing.
4. **SQLite + WAL**: Single-file database with write contention handling. No external dependencies.
5. **Guard Separation**: PreGuards block, PostGuards extract. Clear responsibility boundaries.
6. **No Profiles**: Holmes is purpose-built for security work. The system prompt defines its identity.
