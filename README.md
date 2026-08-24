# Holmes — AI Security Research Agent

Holmes is an autonomous, Rust-native AI agent for security research, penetration testing, code auditing, and reverse engineering. It runs a single cognitive loop — perception → deliberation → action — backed by an event-sourced session store, a Mind Palace memory layer, and native browser automation with human-in-the-loop handoff.

## 🚀 Key Features

- **Unified Agent Runtime** — every turn flows through one engine (`AgentRuntime::run_turn`): budget/compaction → bounded Fast/Adaptive/Deep cognitive pass → commit validation → permission/guard/middleware → tool batch → typed Evidence → memory.
- **Six-Way Decision Model** — `Answer`, `Finish`, `AskWatson` (human handoff), `UseTools`, `SetGoal`, and fail-closed `ProtocolViolation` are first-class loop outcomes.
- **Hypothesis Ledger v2** — case-scoped append-only Hypotheses, Predictions, Experiments, Evidence, Resolutions, and bounded deliberation commits. Strong findings and finishes cite verified Resolution IDs; delegated Experiments use durable leases, heartbeat, fencing, and evidence-bound structured results.
- **Authorized bounty research workflow** — case-level program scope, in-scope asset inventory with provenance, findings gated on verified Ledger Resolution IDs, and a bounty-ready markdown report. Research workflow only (no exploit capability). See [docs/architecture/bounty-workflow.md](docs/architecture/bounty-workflow.md).
- **Three Independent Safety Layers** — `PermissionPolicy` (user authorization, 6 modes), `GuardChain` (system-level pre/post tool hooks; `SkepticGate` is the sole writer to the validated state zone), and `RuntimeMiddleware` (cross-cutting command blocklist, sensitive-data redaction, token budget).
- **Event-Sourced Sessions + Semantic Kernel** — every state change is an immutable `Event` in SQLite (FTS5 + WAL). Sessions replay from events; the Semantic Kernel persists prompt/model/mode/tools metadata so a session resumes with full context. Forking is a transactional event copy; compaction is archived and replayable.
- **Mind Palace** — event-backed and long-term lexical (FTS5/LIKE) memory with staged learning and conflict handling; live case state is projected by the Runtime and Ledger.
- **Native Browser Automation** *(new)* — a headed Chromium the agent launches and keeps open across turns via native CDP (`chromiumoxide`). When a page needs a human (login / 2FA / CAPTCHA), the agent hands off with `AskWatson`, you act in the browser window, reply `continue`, and the agent continues on the same authenticated page. Auto-detects your real Chrome to bypass anti-bot fingerprinting; per-session profile + userDataDir; Chromium sandbox always on.
- **Recursive Subagents** — bounded, isolated subagent runtimes with structured `AgentTaskResult`; Ledger Experiments receive a least-privilege slice and an exact tool allowlist.
- **Modern TUI + Legacy REPL** — full-screen TUI (default) with mouse-wheel scrollback, command palette, session tree, event timeline, fork-from-event; Reedline REPL still available for one-shot and scripted use.
- **Bundled Pentest Methodology** — [Pentest-Lyan](https://github.com/HeaSec/Pentest-Lyan) (v2.3, MIT) is internalized as the default operating standard in Pentest mode (three phases, 12-dimension threat model, 9 Banned Patterns).
- **Multi-Provider LLM** — any Anthropic-Messages-compatible gateway (Anthropic, GLM/Zhipu, …) with failover, rate limiting, and role-based routing over a single wire format.
- **Deterministic Harness** — scripted-LLM scenario tests in YAML drive a real `AgentRuntime` with an in-memory store for regression coverage.

## 🛠️ System Architecture

The workspace is split into focused crates. `holmes-core` is the base (everything depends on it); `holmes-cli` and `holmes-harness` sit at the top.

| Crate | Role |
|---|---|
| `holmes-core` | Base layer: types, `Event`, `Config`, `RuntimeSession`, `AgentHook` / `SubagentRunner` traits, four-zone `AttackState`. |
| `holmes-session` | SQLite + FTS5 + WAL event store; `SessionStore` trait; semantic replay, fork, compaction archive. |
| `holmes-llm` | Multi-provider client over the Anthropic Messages wire format; failover, rate limit, role routing, error classification. |
| `holmes-tools` | Extensible tool registry: command exec, Python, HTTP, web fetch, file read/write/edit, grep, glob, todo plan, PDF reading, reporting, bounty research tools, optional subagent + MCP, browser. Hypothesis state is owned by the Runtime-intercepted case Ledger protocol. |
| `holmes-guards` | Pre/Post-tool guard chain: `immutable_field`, `dangerous_command`, `repetition`, `attack_surface`, `evidence_extractor`, `skeptic_gate`, `failure_tracker`, `soft404`, plus always-on heuristic `scope` and bounty research gates. |
| `holmes-mind-palace` | Event-backed and long-term memory with staged learning. |
| `holmes-runtime` | Agent loop, bounded cognition, Hypothesis Ledger protocol, compaction, middleware, permissions, supervision, and completion gates. |
| `holmes-browser` | Native CDP browser automation (`chromiumoxide`): lazy launch, per-session profile, read-only gating, stealth, sandbox-safe. |
| `holmes-harness` | Deterministic scenario runner for regression tests. |
| `holmes-cli` | Entrypoint: TUI, legacy REPL, one-shot, session/config plumbing, slash commands. |

## 📦 Getting Started

### Prerequisites
- Rust toolchain (cargo, stable)
- An API key for an Anthropic-Messages-compatible provider (Anthropic Claude, GLM/Zhipu, …). `ANTHROPIC_API_KEY` and `HOLMES_API_KEY` are auto-detected by `setup`.

### Build

```bash
git clone git@github.com:d3sh1n/holmes.git
cd holmes
cargo build --release          # produces ./target/release/holmes
```

### Configure

Run the interactive wizard (writes `config.yaml` under your data dir):

```bash
./target/release/holmes setup
```

- Config location: `dirs::data_dir()/holmes/config.yaml` — e.g. `~/Library/Application Support/holmes/config.yaml` on macOS, `~/.local/share/holmes/config.yaml` on Linux.
- `config.default.yaml` at the repo root is the reference template with every option documented.
- To use an Anthropic-compatible gateway (e.g. GLM), add it as another entry under `llm.providers` with `api_format: anthropic`.

### Run

```bash
./target/release/holmes                       # full-screen TUI (default)
./target/release/holmes repl                  # legacy Reedline REPL
./target/release/holmes -q "your question"    # one-shot, non-interactive
./target/release/holmes -c                    # continue most recent session
./target/release/holmes -r <session-id>       # resume a specific session
./target/release/holmes --mode pentest        # pentest | security-research | code-audit | reverse | mixed
```

### TUI Shortcuts

```text
F1 help        F2 session tree     F3 event timeline
F4 permissions F5 GuardChain       F6 recent sessions
Ctrl+L command palette   Ctrl+N new   Ctrl+B fork   Ctrl+O fold/expand tool output
Mouse wheel / PgUp / PgDn   scroll chat history
```

Useful slash commands:

```text
/tree                         # session tree
/tree events [limit]          # event timeline
/tree fork <event_index>      # fork from an event
/permissions                  # show current permission policy
/permissions mode read-only   # switch mode
/guards                       # show active guards
/browser close                # close the long-lived browser (if open)
/ledger                       # inspect the current case Ledger
/ledger compact               # verify/rebuild the checksum snapshot
/bounty                       # inspect the authorized program scope and asset inventory
```

## 🌐 Browser Automation

Enable in `config.yaml`:

```yaml
browser:
  enabled: true
  timeout: 45                 # per-action seconds
  # executable_path: null     # auto-detects your real Chrome/Edge (recommended; bypasses anti-bot)
  # cdp_endpoint: "http://127.0.0.1:9222"   # attach to your running Chrome instead of launching
```

Then ask the agent to drive it:

```
用 browser 打开 https://example.com 并告诉我页面标题
```

**Login handoff** — when a page needs a human step, the agent navigates there, emits `AskWatson` describing what to do, and pauses. You act in the browser window, then reply `continue`; the agent continues on the same authenticated page. The browser stays open across turns and is closed only on `/browser close`, `/quit`, or session end.

**Security** — browser actions go through `PermissionPolicy` + `GuardChain`. The Chromium built-in sandbox is always on; `--no-sandbox` / `--disable-web-security` and similar flags in `extra_launch_args` are rejected. In `read_only` mode, write actions (`click` / `fill` / `execute_js`) are blocked by `BrowserReadOnlyMiddleware`.

## 🧪 Testing

```bash
cargo test --workspace                        # full suite
cargo test -p holmes-runtime                  # one crate
cargo test -p holmes-harness                  # deterministic scenario tests (scenarios/*.yaml)
```

Add a YAML to `scenarios/` with scripted LLM responses + mocked tools + expectations, and the harness exercises it through a real `AgentRuntime`.

Bounty-workflow unit tests live in `holmes-core`, `holmes-guards`, `holmes-tools`, and `holmes-runtime`. The operator should run `cargo test --workspace` (the GitHub MCP path cannot run the suite).

## 🤝 Contributing

Contributions, bug reports, and feature requests are welcome — open an issue or a pull request.

## 📝 License & Credits

MIT License. Bundled methodology: [Pentest-Lyan](https://github.com/HeaSec/Pentest-Lyan) (v2.3, MIT, by HeaSec).
