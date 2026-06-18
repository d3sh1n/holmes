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
- **GuardChain** — 3 pre-guards + 5 post-guards for safety and evidence extraction
- **10 built-in tools** — Shell execution, HTTP requests, Python scripting, browser automation, MCP

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
- An LLM API key (Anthropic, OpenAI, DeepSeek, OpenRouter, Groq, or custom endpoint)

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
  2. OpenAI (GPT-4o, o4, etc.)
  3. DeepSeek
  4. OpenRouter (multi-provider aggregator)
  5. Groq (fast inference)
  6. Custom endpoint (enter URL manually)

Select provider [1-6]: 1
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

**Provider auto-detection**: Holmes automatically detects API keys from environment variables (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `DEEPSEEK_API_KEY`, `OPENROUTER_API_KEY`, `GROQ_API_KEY`).

**Model auto-discovery**: Holmes queries the provider's `/models` endpoint to fetch available models, with curated fallback lists for each provider when the API is unreachable.

### Launch

```bash
./target/release/holmes
```

---

## 3. Architecture

### Crate Map (7 crates)

```
holmes/
├── crates/
│   ├── holmes-core/       # Event enum (40+ variants), types, config, RuntimeSession, Workflow trait
│   ├── holmes-session/    # SessionDB (SQLite + FTS5), MemoryStore, Selector
│   ├── holmes-llm/        # Multi-provider LLM client (OpenAI + Anthropic), failover, rate limiting
│   ├── holmes-tools/      # Tool trait, ToolRegistry, 9 built-in tools, MCP integration
│   ├── holmes-guards/     # GuardChain: 3 PreGuards + 5 PostGuards
│   ├── holmes-mind-palace/# Memory Layer + Context Layer + Dashboard Layer
│   └── holmes-cli/        # CLI binary: REPL, slash commands, setup wizard, workflows, goal loop
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
│  holmes-cli (REPL)                                  │
│                                                     │
│  /slash command? → CommandRegistry.dispatch()       │
│  normal message  → ChatWorkflow.forward(session)    │
│                                                     │
│  Selector.select(session) → Workflow.forward()      │
│       │                                               │
│       └── ReconWorkflow / AnalysisWorkflow /         │
│           ExploitWorkflow / ReportWorkflow           │
│                   │                                   │
│                   ▼                                   │
│           LLM ↔ Tool Loop                             │
│           ┌──────────────────────────┐               │
│           │ LlmClient.chat_completion│               │
│           │        ↓                  │               │
│           │ GuardChain.run_pre()     │               │
│           │        ↓                  │               │
│           │ ToolRegistry.execute()   │               │
│           │        ↓                  │               │
│           │ GuardChain.run_post()    │               │
│           └──────────────────────────┘               │
│                   │                                   │
│                   ▼                                   │
│           MindPalace.ingest(event)                    │
│           SessionDB.append_event()                    │
└─────────────────────────────────────────────────────┘
```

### Event Sourcing

All state changes are immutable `Event` records stored in SQLite. This means:

- **Full replay**: Resume any session from any point
- **Branching**: Fork sessions at any event index
- **Auditability**: Every tool call, finding, and decision is traceable
- **Recovery**: Crash-safe — state is reconstructed from events

---

## 4. CLI Usage

### Commands

```bash
holmes                      # Start interactive chat (default)
holmes chat                 # Same as above
holmes sessions             # List recent sessions
holmes setup                # Interactive LLM provider configuration
holmes version              # Show version

# Session management
holmes -r <id>              # Resume a specific session
holmes -c                   # Continue the most recent session
holmes -q "query"           # One-shot non-interactive query
holmes -m <model>           # Override model
holmes --mode audit         # Set session mode
```

### Interactive REPL

```
╔══════════════════════════════════════════════╗
║  Holmes — AI Security Research Agent         ║
║  Type /help for commands, /quit to exit      ║
╚══════════════════════════════════════════════╝

> 扫描 target.com 的开放端口
🤔 
Holmes: 我将使用 nmap 扫描 target.com 的开放端口...
  → recon

> /dashboard
  [攻击面]
    主机: 1, 服务: 5, 端点: 12, 凭据: 0
  [漏洞发现]
    Open SSH 7.4 (中危)
```

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
| `/branch [title]` | `/fork` | Fork new session from current point |
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
| `/config set <key> <value>` | Modify configuration |

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

Three PreGuards (block before execution) + five PostGuards (process results, update state):

| Guard | Type | Function |
|-------|------|----------|
| `ImmutableFieldGuard` | Pre | Blocks requests to non-target hosts/IPs |
| `DangerousCommandGuard` | Pre | Blocks destructive commands (`rm -rf`, fork bombs, container escapes) |
| `RepetitionGuard` | Pre | Blocks the 4th repeat of the same semantic signature |
| `AttackSurfaceUpdater` | Post | Extracts ports, services, tech stack, endpoints |
| `EvidenceExtractor` | Post | Extracts credentials, object references |
| `SkepticGate` | Post | Validates findings against action history |
| `FailureTracker` | Post | Tracks consecutive failures |
| `Soft404Detector` | Post | Fingerprints soft-404 responses |

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

Nine built-in tools, extensible via the `Tool` trait and MCP:

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
| `browser` | Playwright browser automation (when enabled) | No |

### LLM Layer

Multi-provider HTTP client with:
- **OpenAI-compatible** (`/chat/completions`, Bearer auth) and **Anthropic native** (`/v1/messages`, `x-api-key` + `anthropic-version`) formats
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
    ├── holmes-guards/         # GuardChain (PreGuards + PostGuards)
    ├── holmes-mind-palace/    # Mind Palace (memory + context + dashboard)
    └── holmes-cli/            # CLI binary (REPL, setup, slash commands, workflows)
```

### Key Design Decisions

1. **Event Sourcing**: All state is derived from events. No mutable state survives a crash.
2. **Session as Runtime Value**: `RuntimeSession` carries messages, lineage, and tokens — like a tensor flowing through layers.
3. **Workflow Composition**: `Workflow::forward(session) -> session` enables clean composition. `Selector` replaces hardcoded routing.
4. **SQLite + WAL**: Single-file database with write contention handling. No external dependencies.
5. **Guard Separation**: PreGuards block, PostGuards extract. Clear responsibility boundaries.
6. **No Profiles**: Holmes is purpose-built for security work. The system prompt defines its identity.
