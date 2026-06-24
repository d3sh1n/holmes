# Holmes - AI Security Research Agent

Holmes is an advanced, autonomous AI security research agent built in Rust. It utilizes LLMs to act as a deeply integrated virtual security researcher, fully capable of delegating tasks to its recursive subagents, auditing and interacting with target systems, and organizing findings within its "Mind Palace."

## 🚀 Key Features

*   **First-Class Recursive Subagents**: A physical agent loop allowing the primary Holmes agent to autonomously spawn, isolate, and delegate tasks to subagents. Subagents run asynchronously in their own Tokios runtime with their own sandboxed contexts.
*   **Modern TUI & REPL**: Integrated with [Reedline](https://github.com/nushell/reedline) (the engine behind Nushell) to provide a rich, interactive REPL. Features include IDE-like dropdown auto-completions, syntax highlighting, and keyboard navigation for Slash (`/`) commands.
*   **Mind Palace Architecture**: A structured memory and knowledge management layer that organizes findings, tasks, and context into logical domains.
*   **GuardChain Security**: A flexible and powerful interception system (`holmes-guards`) that monitors all actions. It supports read-only locking, soft-404 detection, repetition prevention, and state-seeding safeguards.
*   **Persistent SQLite DB**: Complete event tracing, memory retrieval (using Full-Text Search), and dialogue saving locally so that sessions can be resumed or analyzed.
*   **Modular LLM Core**: Extensible tool registry and model backend abstraction (currently tailored for Claude/Anthropic APIs).

## 🛠️ System Architecture

The project is structured into multiple decoupled crates to ensure maximum flexibility and reliability:

*   **`holmes-cli`**: The primary entrypoint. Handles the interactive Reedline TUI, session loading, config reading, and slash commands.
*   **`holmes-core`**: Defines the shared types, structs, and traits (e.g., configurations, event protocols).
*   **`holmes-runtime`**: The heart of the agent. Manages the perception-deliberation-action cycle (the Agent Loop).
*   **`holmes-mind-palace`**: Context and memory layer logic.
*   **`holmes-session`**: Handles local SQLite persistence, FTS searches, and data concurrency control.
*   **`holmes-guards`**: Contains Pre- and Post-tool hooks designed to prevent infinite loops, restrict dangerous commands, or seed read states safely.
*   **`holmes-llm`**: Manages backend API integrations and prompt streaming.
*   **`holmes-tools`**: The extensible tool registry exposing tools (like command execution, web browsing, MCP) to the LLM.

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

Run the compiled binary. You will enter the interactive REPL:
```bash
./target/release/holmes
```

Type `/` and press `Tab` (or just wait for the dropdown) to see all available commands along with their descriptions. You can use `/help` to view detailed instructions or `/new` to start a fresh autonomous session.

## 🤝 Contributing
Contributions, bug reports, and feature requests are always welcome! Feel free to open an issue or submit a pull request.

## 📝 License
This project is licensed under the MIT License.
