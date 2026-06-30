# Native Browser Automation Design

Date: 2026-06-30
Status: Approved design, pending implementation plan

## Summary

Give Holmes a native, long-lived browser automation capability so the agent
can drive a real headed Chromium for JavaScript-rendered pages, form
interaction, and — critically — human-in-the-loop handoff: the agent opens the
browser, asks the user to perform a manual action (login, 2FA, captcha), then
continues operating in the **same** browser with the user's authenticated
session.

The build uses the Rust-native CDP library `chromiumoxide` (no Node/MCP
subprocess). Human handoff reuses the existing `HolmesDecision::AskWatson`
path purely via system-prompt guidance (no runtime changes). Each Holmes
session owns an isolated browser profile (`userDataDir`), so login state
survives across turns and across resume, but is **not** inherited by forks or
subagents.

## Goals

- Agent launches a headed Chromium itself and drives it through a single
  `browser` tool (action-parameter style, matching Holmes' other tools).
- Browser process is long-lived: survives across turns, `/mcp reload`, and
  registry rebuilds; closed only on `/browser close`, `/quit`, or session end.
- When a manual step is needed, the agent pauses via the existing `AskWatson`
  decision; the user acts in the browser window and resumes from the CLI.
- Login state persists per session (`userDataDir`), so resume reuses it; forks
  and subagents start clean.
- Browser actions go through `PermissionPolicy`; read-only actions are
  permitted under `read_only` mode, write actions are not.
- `chromiumoxide` dependency is isolated in a new `holmes-browser` crate so it
  does not bloat `holmes-tools` consumers.

## Non-goals

- No domain allow-listing for browser navigation (scope control is via
  `PermissionPolicy` user approval, by explicit decision).
- No new runtime handoff primitive — `AskWatson` is reused as-is.
- No subagent access to the parent's browser (deferred).
- No vision/OCR pipeline in v1 (screenshot returns a path + size; `vision`
  flag reserved for later).
- No CDP-attach-to-user's-Chrome mode (agent launches its own Chromium).

## Architecture

### Crate layout

```
crates/holmes-browser/         (NEW)
  Cargo.toml                   deps: chromiumoxide, holmes-core, tokio, anyhow, tracing
  src/lib.rs                   pub re-exports
  src/manager.rs               BrowserManager
  src/profile.rs               profile_dir_for(session_id, base) -> PathBuf
  src/error.rs                 BrowserError

crates/holmes-tools/src/builtin/browser.rs   (MODIFIED)
  BrowserTool                  thin shell holding Arc<BrowserManager>

crates/holmes-core/src/config.rs             (MODIFIED)
  BrowserConfig                add fields (see Configuration)

crates/holmes-cli/src/chat.rs                (MODIFIED)
  ChatContext.browser: Option<Arc<BrowserManager>>
  create_chat_context          construct + inject when enabled

crates/holmes-runtime/src/middleware.rs      (MODIFIED)
  BrowserReadOnlyMiddleware   gate write actions under read_only mode
```

`holmes-browser` depends only on `holmes-core` (for `BrowserConfig`, `ContentBlock`),
never on `holmes-tools`, so there is no cycle. `holmes-tools` depends on
`holmes-browser`.

### Lifecycle and ownership

- `BrowserManager` is a `ChatContext`-level `Option<Arc<BrowserManager>>`.
  Constructed once when `config.browser.enabled && mode` allows, at
  `create_chat_context` time, keyed to the session id.
- `BrowserTool::new(manager: Arc<BrowserManager>)` receives the same `Arc`
  during `build_tool_registry`. On `/mcp reload` the registry is rebuilt and a
  new `BrowserTool` is constructed, but it receives the **same** `Arc` from
  `ChatContext`, so the browser process is untouched.
- `BrowserManager::close()` is called on `/browser close`, `/quit`, and when
  `ChatContext` is dropped (Drop impl on a wrapper). Close kills the Chromium
  process and deletes nothing on disk (profile dir stays for resume reuse).

### Data flow (login handoff)

```
1. LLM  → UseTools(browser, {action:navigate, url:target/login})
2. BrowserTool.execute → manager.navigate(url) → returns page summary text
3. LLM  → AskWatson("请在浏览器窗口完成登录，回复 continue")
          runtime: TurnOutcome::NeedsUser(prompt) → CLI prints + pauses
4. USER → manually logs in / 2FA in the browser window
5. USER → types "continue" in CLI
6. NEW TURN → LLM → browser {action:get_content / click / fill …}
              on the SAME page, now authenticated
```

The runtime is unchanged. The agent reaches `AskWatson` by its own decision;
the system prompt teaches it to do so when a manual browser step is required.

## Components

### `BrowserManager` (holmes-browser)

```rust
pub struct BrowserManager {
    inner: Mutex<ManagerState>,
    config: BrowserConfig,
    profile_dir: PathBuf,
}

struct ManagerState {
    browser: Option<chromiumoxide::Browser>,
    page: Option<chromiumoxide::Page>,
    // lazily launched on first action
}
```

Public API (all async):

- `new(session_id, sessions_dir, config) -> Result<Self>`: computes
  `profile_dir = sessions_dir.join(session_id).join("browser-profile")`,
  creates the dir, but does **not** launch yet (lazy).
- `navigate(url) -> Result<PageSnapshot>`
- `click(selector) -> Result<ActionOutcome>`
- `fill(selector, value) -> Result<ActionOutcome>`
- `screenshot(full_page) -> Result<Screenshot>` (path + dims; no inline bytes)
- `get_content(selector: Option) -> Result<String>` (text or HTML excerpt, `content_limit` cap)
- `execute_js(code) -> Result<serde_json::Value>`
- `is_launched() -> bool`
- `close()`: drop page + browser, kill subprocess.

Lazy launch: the first action call invokes `ensure_launched()` which calls
`chromiumoxide::Browser::launch` with:
- `headless: false` (always headed — the whole point is the user can see/interact)
- `user_data_dir: Some(profile_dir)`
- `ignore_https_errors`
- `executable` from `config.browser.executable_path` if set, else
  `chromiumoxide`'s default Chromium discovery
- `timeout` from config
- `proxy` from config if set

`PageSnapshot` / `ActionOutcome` are plain DTOs; `BrowserTool` formats them to
text for the LLM.

Serialization to the LLM is text-only and length-capped by
`config.browser.content_limit`. Screenshots are saved under
`sessions/<id>/browser-screenshots/` and reported as a path + dimensions (no
image bytes into the transcript in v1).

### `BrowserTool` (holmes-tools)

Unchanged single-tool, action-parameter schema. `is_read_only()` returns
`false` (the tool as a whole can mutate); per-action read/write gating is done
by `BrowserReadOnlyMiddleware` (see Safety). Action list:

- Read-only: `navigate`, `screenshot`, `get_content`
- Write: `click`, `fill`, `execute_js`
- Lifecycle: `create_context`, `close_context` (kept for compat; map to
  profile-tab management or no-op in v1; `close` is reserved as a CLI command,
  not an LLM action)

`execute()` parses the action, dispatches to `BrowserManager`, formats the
result DTO into a text `String` (consistent with Holmes' `Tool::execute`
contract).

### `ChatContext` integration

Add field `pub browser: Option<Arc<BrowserManager>>`. In
`create_chat_context`, after `session_id` is known:

```rust
let browser = if config.browser.enabled {
    Some(Arc::new(BrowserManager::new(
        &session_id, &sessions_dir, config.browser.clone(),
    )?))
} else {
    None
};
```

`build_tool_registry` gains an `Option<Arc<BrowserManager>>` argument; when
`Some`, it constructs `BrowserTool::new(manager.clone())` and registers it.
On `/mcp reload`, the rebuilt registry receives the same `Arc` from
`ChatContext`.

### Profile management

- `profile_dir = <data_dir>/sessions/<session_id>/browser-profile/`.
- Resume (`-r <id>` / `-c`): same `session_id` → same `profile_dir` → cookies
  and login state present. `BrowserManager::new` does not wipe it.
- Fork: new `session_id` → new empty `profile_dir` → **no inherited login**
  (security: a forked branch must not silently carry the parent's auth).
- `/branch` and `fork_session` do **not** copy the browser-profile dir.
- Subagents: `CliSubagentRunner` does not receive the parent's
  `BrowserManager`; if `config.browser.enabled`, the subagent may create its
  own (separate profile) — but v1 leaves subagent browser unconfigured
  (deferred).

## Safety and Permissions

- **No domain allow-list** (by explicit decision). Browser scope is governed
  by the existing `PermissionPolicy`:
  - Under `default`/`plan`: the `browser` tool call is subject to the normal
    approval flow (allow/deny globs on tool name `browser` / `browser_*`).
  - Under `read_only`: only read-only actions may run.
- **`BrowserReadOnlyMiddleware`** (new `RuntimeMiddleware`): in
  `before_tool_call`, if the tool is `browser` and the active permission mode
  is `read_only`, inspect `args.action`; reject `click` / `fill` /
  `execute_js` with a clear reason. This keeps the `BrowserTool` itself
  mode-agnostic.
- The human-handoff step (login) is performed by the user, so no credentials
  enter the agent context; the agent only observes the resulting cookies/page.
- Chromium is launched with `--no-first-run --no-default-browser-check` and the
  isolated `userDataDir`; it is not the user's personal Chrome profile.

## Handoff via AskWatson (no runtime change)

The `SYSTEM_PROMPT` (and the Pentest methodology block when in Pentest mode)
gets a short addition, enabled only when the `browser` tool is registered:

> When a page requires a manual human step (login, 2FA, CAPTCHA, or any action
> you cannot or should not automate), first `browser navigate` to the relevant
> page, then emit `AskWatson` describing exactly what the user must do in the
> browser window and what to reply when done (e.g. "continue"). The browser
> stays open between turns; after the user replies, continue operating on the
> same authenticated page.

No new `HolmesDecision` variant, no new yield, no `action.rs` changes.

## Configuration

`BrowserConfig` additions (all backward-compatible via `#[serde(default)]`):

```rust
pub struct BrowserConfig {
    pub enabled: bool,
    pub headless: bool,            // v1: IGNORED — launch is always headed (see Fixed decisions)
    pub vision: bool,              // reserved (v1: unused)
    pub content_limit: usize,
    pub timeout: u32,
    pub proxy: Option<String>,
    pub ignore_https_errors: bool,
    pub executable_path: Option<String>,   // NEW: override Chromium binary
    pub extra_launch_args: Vec<String>,    // NEW: extra CDP launch args
    pub screenshot_dir: Option<String>,    // NEW: default sessions/<id>/browser-screenshots
    // mcp_command / mcp_args REMOVED (no longer Node-based)
}
```

Note on `headless`: the field is kept for config compatibility but v1 launches
headed unconditionally (the whole point is the user must see/interact with the
window). A future headless+vision mode may honor it; v1 ignores it.

`config.default.yaml` updates the `browser:` block to the new shape and sets
`enabled: false` by default.

## Error handling

- **Launch failure** (Chromium missing / won't start): `BrowserManager::new`
  is lazy so it does not fail at session start; the first action's
  `ensure_launched()` returns a `BrowserError::LaunchFailed` which the tool
  surfaces as a normal tool-error result to the LLM.
- **Navigation timeout**: respect `config.browser.timeout`; return
  `BrowserError::Timeout`, not a hang.
- **Selector not found** (click/fill/get_content): `BrowserError::NotFound`.
- **Browser crash** (process died): next action detects dead handle, calls
  `close()` then re-launches on the same profile (state survives via
  `userDataDir`); emit a warning yield.
- **Drop without close**: `BrowserManager` Drop kills the subprocess
  best-effort; profile dir is left intact.
- **`execute_js` exception**: capture the JS exception message into the
  returned error; do not crash the runtime.

## Testing strategy

### Unit (holmes-browser, headless, no real browser needed for some)

- `profile_dir_for` resolves and is idempotent.
- `BrowserConfig` serde round-trip with new fields + defaults.
- Action read/write classification helper used by the middleware.

### Integration (real Chromium, gated behind a feature / `--ignored`)

- Launch (headless for CI), navigate to a `data:` URL, `get_content` returns
  expected text.
- `fill` + `get_content` round-trip on a local fixture HTML.
- `execute_js` returns a computed value.
- Re-open `BrowserManager` with the same `profile_dir` after `close()`:
  a cookie set before close is still present (proves resume reuse).
- A fresh `profile_dir` does **not** see that cookie (proves fork isolation).

### Runtime / harness

- `BrowserReadOnlyMiddleware` rejects `click`/`fill`/`execute_js` under
  `read_only` mode; permits `navigate`/`screenshot`/`get_content`.
- A scripted scenario where the LLM (after `browser navigate`) emits
  `AskWatson` produces `TurnOutcome::NeedsUser` with the expected prompt.

### CLI

- `/browser close` closes the manager and the next `browser` action
  re-launches.
- `/quit` and session end close the manager.

## Risks and mitigations

- **chromiumoxide maintenance / platform**: pin a known-good version; allow
  `executable_path` override so users can point at a system Chrome if the
  bundled fetch fails. Mitigation: fall back to a clear error message.
- **Headed requirement on headless servers**: v1 is headed by design (the user
  must see the window). Document that this needs a GUI; a future `headless +
  screenshot-only` mode is out of scope.
- **Long-lived process leaks**: explicit `close()` on session end + Drop guard;
  `/browser close` for manual control.
- **Registry rebuild dropping the browser**: solved by `Arc` in `ChatContext`.
- **Login-state leakage into forks**: solved by per-session `userDataDir` and
  no profile copy on fork.

## Fixed implementation decisions

- `chromiumoxide` (not `headless-chrome`); default Chromium discovery, with
  `executable_path` override.
- `BrowserManager` is `ChatContext`-level `Option<Arc<...>>`; `BrowserTool`
  receives the `Arc` at construction.
- Handoff is prompt-only via existing `AskWatson`; zero runtime/decision/yield
  changes.
- Single `browser` tool, action parameter; per-action read/write gating via a
  new `BrowserReadOnlyMiddleware`.
- `userDataDir = sessions/<session_id>/browser-profile/`; resume reuses,
  fork/subagent do not inherit.
- v1 is headed (`headless=false` at launch regardless of config when the user
  is expected to interact); `vision` and inline image content deferred.
