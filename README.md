# Holmes - AI Security Research Agent

Holmes is an advanced, autonomous AI security research agent built in Rust. It utilizes LLMs to act as a deeply integrated virtual security researcher, fully capable of delegating tasks to its recursive subagents, auditing and interacting with target systems, and organizing findings within its "Mind Palace."

## 🚀 Key Features

*   **Unified Agent Runtime**: Interactive chat, one-shot queries, tool execution, permissions, guard checks, event writes, and runtime yields flow through `holmes-runtime::AgentRuntime`.
*   **First-Class Recursive Subagents**: Holmes can spawn, isolate, and delegate work to subagents. Subagents run asynchronously with their own runtime context.
*   **Modern TUI & REPL**: A Pi-style full-screen TUI is the default interactive surface, with Reedline still available as a legacy line REPL. Features include command palettes, keyboard navigation, session tree inspection, event timelines, and fork-from-event workflows.
*   **Mind Palace Architecture**: A structured memory and knowledge management layer that organizes findings, tasks, and context into logical domains.
*   **User-Configurable Safety Layer**: `PermissionPolicy` and `GuardChain` are exposed through readable TUI commands, so users can inspect modes, allow/deny tools, and toggle guard behavior without hand-editing YAML.
*   **Persistent SQLite DB**: Complete event tracing, memory retrieval (using Full-Text Search), and dialogue saving locally so that sessions can be resumed or analyzed.
*   **Modular LLM Core**: Extensible tool registry and model backend abstraction (currently tailored for Claude/Anthropic APIs).

## 🛠️ System Architecture

The project is structured into multiple decoupled crates to ensure maximum flexibility and reliability:

*   **`holmes-cli`**: The primary entrypoint. Handles the full-screen TUI, legacy Reedline REPL, session loading, config reading, and slash commands.
*   **`holmes-core`**: Defines the shared types, structs, and traits (e.g., configurations, event protocols).
*   **`holmes-runtime`**: The heart of the agent. Manages the perception-deliberation-action cycle, tool execution, permissions, guards, runtime yields, and event persistence.
*   **`holmes-mind-palace`**: Context and memory layer logic.
*   **`holmes-session`**: Handles local SQLite persistence, FTS searches, and data concurrency control.
*   **`holmes-guards`**: Contains Pre- and Post-tool hooks designed to prevent infinite loops, restrict dangerous commands, or seed read states safely.
*   **`holmes-llm`**: Manages backend API integrations and prompt streaming.
*   **`holmes-tools`**: The extensible tool registry exposing command execution, Python, HTTP, reporting, hypothesis, optional subagent, and MCP-backed tools to the LLM.

## 📦 Getting Started

### Prerequisites
*   Rust toolchain (cargo)
*   A valid Anthropic/Claude API Key (or supported backend key).

### Building from Source

Clone the repository and build the workspace in release mode:
```bash
git clone git@github.com:d3sh1n/holmes.git
cd holmes
cargo build --release
```

### Running Holmes

Run the compiled binary. You will enter the full-screen TUI:
```bash
./target/release/holmes
```

Holmes starts a fresh session by default. Continue the most recent session only when you ask for it:

```bash
./target/release/holmes -c
./target/release/holmes --resume <session-id>
```

For the legacy Reedline REPL, use:

```bash
./target/release/holmes repl
# or
./target/release/holmes --repl
```

Type `/` in the TUI command editor or `Ctrl+L` for the command palette. In the legacy REPL, type `/` and press `Tab` to see available commands.

Useful TUI commands:

```text
/tree                         # Show the session tree
/tree events [limit]          # Inspect the current session event timeline
/tree fork <event_index>      # Fork from a specific event and switch to it
/permissions                  # Explain the current permission policy
/permissions mode read-only   # Switch policy mode
/permissions allow http_*     # Add allow/deny patterns
/guards                       # Explain active GuardChain checks
/guards disable repetition    # Toggle an individual guard
```

Full-screen TUI shortcuts:

```text
F1 help        F2 session tree     F3 event timeline
F4 permissions F5 GuardChain       F6 recent sessions
Ctrl+L commands  Ctrl+N new  Ctrl+B fork  Ctrl+O tool output
```

## 🤝 Contributing
Contributions, bug reports, and feature requests are always welcome! Feel free to open an issue or submit a pull request.

## 📝 License
This project is licensed under the MIT License.
