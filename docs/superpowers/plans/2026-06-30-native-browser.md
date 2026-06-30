# Native Browser Automation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give Holmes a native, long-lived, sandboxed Chromium (via `chromiumoxide`) the agent can drive through a single `browser` tool, with human-in-the-loop handoff reusing `AskWatson`.

**Architecture:** New `holmes-browser` crate wraps `chromiumoxide` and owns a `BrowserManager` that lives as an `Option<Arc<BrowserManager>>` on `ChatContext` (per-session `userDataDir`, survives registry rebuilds). `BrowserTool` becomes a thin shell injected with that `Arc`. Handoff is prompt-only via existing `AskWatson`. A `BrowserReadOnlyMiddleware` gates write actions under `read_only` permission mode. Chromium's built-in sandbox is always on; `--no-sandbox` is rejected.

**Tech Stack:** Rust, `chromiumoxide = "0.9.1"`, `tokio`, `serde`, existing `holmes-core`/`holmes-tools`/`holmes-runtime`/`holmes-cli`.

**Spec:** `docs/superpowers/specs/2026-06-30-native-browser-design.md`

---

## Scope Check

Single cohesive subsystem (browser automation). One plan, ~11 tasks, each independently testable. No decomposition needed.

## File Structure

### Create
- `crates/holmes-browser/Cargo.toml` — crate manifest (`chromiumoxide`, `holmes-core`, `tokio`, `anyhow`, `tracing`, `serde`, `serde_json`).
- `crates/holmes-browser/src/lib.rs` — module declarations + re-exports.
- `crates/holmes-browser/src/error.rs` — `BrowserError`.
- `crates/holmes-browser/src/profile.rs` — `profile_dir_for`.
- `crates/holmes-browser/src/manager.rs` — `BrowserManager`, `PageSnapshot`, `ActionOutcome`, `Screenshot`, launch-args sanitizer, action read/write classification.
- `crates/holmes-browser/tests/manager.rs` — integration tests (real Chromium, `#[ignore]` by default).

### Modify
- `crates/holmes-core/src/config.rs` — `BrowserConfig` field changes.
- `crates/holmes-tools/Cargo.toml` — add `holmes-browser` dep.
- `crates/holmes-tools/src/builtin/browser.rs` — rewrite `BrowserTool` as thin shell.
- `crates/holmes-tools/src/builtin/mod.rs` — `register_all` gains `browser` param.
- `crates/holmes-cli/src/chat.rs` — `ChatContext.browser`, `build_tool_registry` param, `create_chat_context` wiring, middleware install, `/browser close`, SYSTEM_PROMPT handoff text.
- `crates/holmes-runtime/src/middleware.rs` — `BrowserReadOnlyMiddleware`.
- `crates/holmes-runtime/src/lib.rs` — export middleware if not already.
- `crates/holmes-runtime/Cargo.toml` — no new deps (middleware uses existing `serde_json`).
- `config.default.yaml` — `browser:` block.

---

## Task 1: Scaffold `holmes-browser` crate

**Files:**
- Create: `crates/holmes-browser/Cargo.toml`
- Create: `crates/holmes-browser/src/lib.rs`
- Create: `crates/holmes-browser/src/error.rs`

- [ ] **Step 1: Create `crates/holmes-browser/Cargo.toml`**

```toml
[package]
name = "holmes-browser"
version = "0.1.0"
edition = "2021"

[dependencies]
holmes-core = { path = "../holmes-core" }
chromiumoxide = { version = "0.9", default-features = false, features = ["tokio-runtime"] }
tokio = { workspace = true }
anyhow = { workspace = true }
thiserror = { workspace = true }
tracing = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
futures = "0.3"
async-trait = { workspace = true }

[dev-dependencies]
tempfile = "3"
```

Note: verify `thiserror` and `futures` are in the workspace dependency set (`Cargo.toml` `[workspace.dependencies]`). If `thiserror` workspace entry exists (it does — `thiserror = "1"`), the `{ workspace = true }` form works. If not, pin directly: `thiserror = "1"`, `futures = "0.3"`.

- [ ] **Step 2: Create `crates/holmes-browser/src/error.rs`**

```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BrowserError {
    #[error("chromium launch failed: {0}")]
    LaunchFailed(String),
    #[error("browser action timed out after {0}s")]
    Timeout(u32),
    #[error("element not found for selector: {0}")]
    NotFound(String),
    #[error("javascript evaluation failed: {0}")]
    JsError(String),
    #[error("browser is not launched")]
    NotLaunched,
    #[error("sandbox-disabling launch flag rejected: {0}")]
    SandboxFlagRejected(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("cdp error: {0}")]
    Cdp(#[from] chromiumoxide::error::CdpError),
    #[error("other: {0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, BrowserError>;
```

- [ ] **Step 3: Create `crates/holmes-browser/src/lib.rs`**

```rust
pub mod error;
pub mod manager;
pub mod profile;

pub use error::{BrowserError, Result};
pub use manager::{
    action_is_read_only, BrowserManager, ActionOutcome, PageSnapshot, Screenshot,
};
pub use profile::profile_dir_for;
```

(The `manager` and `profile` modules are created in later tasks; to make this task compile, create them as minimal stubs now — see Step 4.)

- [ ] **Step 4: Create minimal stubs so the crate compiles**

`crates/holmes-browser/src/profile.rs`:
```rust
use std::path::PathBuf;

pub fn profile_dir_for(_sessions_dir: &std::path::Path, _session_id: &str) -> PathBuf {
    PathBuf::new()
}
```

`crates/holmes-browser/src/manager.rs`:
```rust
use crate::error::Result;
use std::path::PathBuf;

pub struct PageSnapshot {
    pub url: String,
    pub title: String,
    pub text_excerpt: String,
}

pub struct ActionOutcome {
    pub summary: String,
}

pub struct Screenshot {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
}

pub struct BrowserManager {
    _private: (),
}

impl BrowserManager {
    pub fn new(
        _session_id: &str,
        _sessions_dir: &std::path::Path,
        _config: holmes_core::config::BrowserConfig,
    ) -> Result<Self> {
        Ok(Self { _private: () })
    }
}

pub fn action_is_read_only(_action: &str) -> bool {
    false
}
```

- [ ] **Step 5: Verify the crate is picked up and compiles**

Run:
```bash
cargo check -p holmes-browser
```
Expected: compiles. (Workspace `members = ["crates/*"]` auto-includes it.) If `chromiumoxide` fails to resolve, verify network access or pin `chromiumoxide = { version = "0.9.1" ...}`.

- [ ] **Step 6: Commit**

```bash
git add crates/holmes-browser
git commit -m "feat(browser): scaffold holmes-browser crate"
```

---

## Task 2: `profile_dir_for` resolver

**Files:**
- Modify: `crates/holmes-browser/src/profile.rs`
- Test: `crates/holmes-browser/src/profile.rs` (inline `#[cfg(test)]`)

- [ ] **Step 1: Write the failing test**

Append to `crates/holmes-browser/src/profile.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_dir_resolves_under_sessions_with_session_id() {
        let dir = profile_dir_for(std::path::Path::new("/data/sessions"), "abc-123");
        assert_eq!(
            dir,
            std::path::PathBuf::from("/data/sessions/abc-123/browser-profile")
        );
    }

    #[test]
    fn profile_dir_rejects_traversal_session_id() {
        // A session id containing path traversal must not escape sessions_dir.
        let dir = profile_dir_for(std::path::Path::new("/data/sessions"), "..%2fevil");
        assert!(dir.starts_with("/data/sessions"));
        assert!(!dir.to_string_lossy().contains(".."));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo test -p holmes-browser profile_dir
```
Expected: FAIL (stub returns empty path).

- [ ] **Step 3: Implement `profile_dir_for`**

Replace `crates/holmes-browser/src/profile.rs` contents:
```rust
use std::path::{Component, Path, PathBuf};

/// Resolve the per-session browser profile directory.
///
/// Layout: `<sessions_dir>/<sanitized_session_id>/browser-profile`.
/// The session id is sanitized so a hostile or malformed id cannot escape
/// `sessions_dir` via traversal or absolute paths.
pub fn profile_dir_for(sessions_dir: &Path, session_id: &str) -> PathBuf {
    let safe = sanitize_session_id(session_id);
    sessions_dir.join(safe).join("browser-profile")
}

fn sanitize_session_id(session_id: &str) -> String {
    let cleaned: String = session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('_').to_string();
    if cleaned.is_empty() {
        "session".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_dir_resolves_under_sessions_with_session_id() {
        let dir = profile_dir_for(Path::new("/data/sessions"), "abc-123");
        assert_eq!(
            dir,
            PathBuf::from("/data/sessions/abc-123/browser-profile")
        );
    }

    #[test]
    fn profile_dir_rejects_traversal_session_id() {
        let dir = profile_dir_for(Path::new("/data/sessions"), "../evil");
        assert!(dir.starts_with("/data/sessions"));
        assert!(!dir.to_string_lossy().contains(".."));
        // ensure no component escapes
        assert!(dir.components().all(|c| !matches!(c, Component::ParentDir)));
    }

    #[test]
    fn profile_dir_handles_empty_session_id() {
        let dir = profile_dir_for(Path::new("/data/sessions"), "");
        assert_eq!(dir, PathBuf::from("/data/sessions/session/browser-profile"));
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p holmes-browser profile_dir
```
Expected: 3 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/holmes-browser/src/profile.rs
git commit -m "feat(browser): resolve per-session profile dir"
```

---

## Task 3: Extend `BrowserConfig`

**Files:**
- Modify: `crates/holmes-core/src/config.rs:349-376`
- Test: inline `#[cfg(test)]` in config.rs

- [ ] **Step 1: Write the failing test**

Add to the bottom of `crates/holmes-core/src/config.rs` (in the existing `#[cfg(test)]` module, or create one):
```rust
#[test]
fn browser_config_serde_round_trip_new_fields() {
    let cfg = BrowserConfig {
        enabled: true,
        headless: false,
        vision: false,
        content_limit: 7000,
        timeout: 45,
        proxy: Some("http://127.0.0.1:8080".into()),
        ignore_https_errors: true,
        executable_path: Some("/usr/bin/chromium".into()),
        extra_launch_args: vec!["--lang=en".into()],
        screenshot_dir: None,
    };
    let yaml = serde_yaml::to_string(&cfg).unwrap();
    let back: BrowserConfig = serde_yaml::from_str(&yaml).unwrap();
    assert_eq!(back.enabled, true);
    assert_eq!(back.executable_path.as_deref(), Some("/usr/bin/chromium"));
    assert_eq!(back.extra_launch_args, vec!["--lang=en".to_string()]);
}

#[test]
fn browser_config_defaults_include_new_fields() {
    let cfg = BrowserConfig::default();
    assert!(!cfg.enabled);
    assert!(cfg.executable_path.is_none());
    assert!(cfg.extra_launch_args.is_empty());
    assert!(cfg.screenshot_dir.is_none());
}

#[test]
fn browser_config_legacy_yaml_without_new_fields_loads() {
    let yaml = "enabled: false\nheadless: true\nvision: false\ncontent_limit: 5000\ntimeout: 30\nignore_https_errors: true\n";
    let cfg: BrowserConfig = serde_yaml::from_str(yaml).unwrap();
    assert!(cfg.executable_path.is_none());
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo test -p holmes-core browser_config
```
Expected: FAIL (fields do not exist).

- [ ] **Step 3: Update the `BrowserConfig` struct**

Replace the struct + `Default` impl at `crates/holmes-core/src/config.rs`:
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_headless")]
    pub headless: bool,
    #[serde(default)]
    pub vision: bool,
    #[serde(default = "default_content_limit")]
    pub content_limit: usize,
    #[serde(default = "default_timeout")]
    pub timeout: u32,
    #[serde(default)]
    pub proxy: Option<String>,
    #[serde(default = "default_ignore_https")]
    pub ignore_https_errors: bool,
    #[serde(default)]
    pub executable_path: Option<String>,
    #[serde(default)]
    pub extra_launch_args: Vec<String>,
    #[serde(default)]
    pub screenshot_dir: Option<String>,
}

fn default_headless() -> bool {
    true
}
fn default_content_limit() -> usize {
    5000
}
fn default_timeout() -> u32 {
    30
}
fn default_ignore_https() -> bool {
    true
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            headless: true,
            vision: false,
            content_limit: 5000,
            timeout: 30,
            proxy: None,
            ignore_https_errors: true,
            executable_path: None,
            extra_launch_args: Vec::new(),
            screenshot_dir: None,
        }
    }
}
```

Note: this **removes** `mcp_command` and `mcp_args`. Grep for usages (`rg "mcp_command|mcp_args" crates/`) and remove them from existing call sites (notably the old `BrowserManager` in `browser.rs`, which is rewritten in Task 7, and `config.default.yaml`, updated in Task 10). If any other code reads them, delete those reads.

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p holmes-core browser_config
```
Expected: 3 passed.

- [ ] **Step 5: Check workspace still compiles (expect breakage in browser.rs — fix by commenting the old manager's mcp fields temporarily if needed, since Task 7 rewrites it)**

```bash
cargo check --workspace 2>&1 | tail -20
```
If `browser.rs` fails due to removed `mcp_command`/`mcp_args`, temporarily comment out the lines that read them (the whole `BrowserManager` is replaced in Task 7). Do not spend time fixing the old implementation.

- [ ] **Step 6: Commit**

```bash
git add crates/holmes-core/src/config.rs
git commit -m "feat(core): extend BrowserConfig for native browser"
```

---

## Task 4: Launch-args sanitizer + read/write classification

**Files:**
- Modify: `crates/holmes-browser/src/manager.rs`
- Test: inline `#[cfg(test)]`

- [ ] **Step 1: Write the failing tests**

Append to `crates/holmes-browser/src/manager.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_no_sandbox_flag() {
        assert!(sanitize_launch_args(&["--no-sandbox".to_string()]).is_err());
        assert!(sanitize_launch_args(&["--disable-web-security".to_string()]).is_err());
        assert!(sanitize_launch_args(&[
            "--disable-setuid-sandbox".to_string()
        ])
        .is_err());
    }

    #[test]
    fn permits_benign_args() {
        let out = sanitize_launch_args(&["--lang=en".to_string(), "--window-size=1280,720".to_string()])
            .expect("benign args");
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn read_only_actions_classified_correctly() {
        assert!(action_is_read_only("navigate"));
        assert!(action_is_read_only("screenshot"));
        assert!(action_is_read_only("get_content"));
        assert!(!action_is_read_only("click"));
        assert!(!action_is_read_only("fill"));
        assert!(!action_is_read_only("execute_js"));
        assert!(!action_is_read_only("unknown"));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p holmes-browser sanitize_launch_args
cargo test -p holmes-browser read_only_actions
```
Expected: FAIL.

- [ ] **Step 3: Implement sanitizer and classifier**

Add to `crates/holmes-browser/src/manager.rs` (keeping the existing stub structs/`BrowserManager::new` for now):
```rust
const READ_ONLY_ACTIONS: &[&str] = &["navigate", "screenshot", "get_content"];

const FORBIDDEN_LAUNCH_FLAGS: &[&str] = &[
    "--no-sandbox",
    "--disable-web-security",
    "--disable-setuid-sandbox",
    "--disable-site-isolation-trials",
    "--allow-running-insecure-content",
];

/// Strip/react to sandbox-disabling launch args. v1 rejects them outright.
pub fn sanitize_launch_args(args: &[String]) -> Result<Vec<String>> {
    for arg in args {
        let normalized = arg.to_ascii_lowercase();
        for forbidden in FORBIDDEN_LAUNCH_FLAGS {
            if normalized == *forbidden || normalized.starts_with(&format!("{forbidden}=")) {
                return Err(BrowserError::SandboxFlagRejected(arg.clone()));
            }
        }
    }
    Ok(args.to_vec())
}

pub fn action_is_read_only(action: &str) -> bool {
    READ_ONLY_ACTIONS.contains(&action)
}
```
(Add `use crate::error::{BrowserError, Result};` at the top of the file if not present.)

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p holmes-browser sanitize_launch_args
cargo test -p holmes-browser read_only_actions
cargo test -p holmes-browser
```
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add crates/holmes-browser/src/manager.rs
git commit -m "feat(browser): sanitize launch args + classify actions"
```

---

## Task 5: `BrowserManager` core — lazy launch, navigate, close

**Files:**
- Modify: `crates/holmes-browser/src/manager.rs`
- Create: `crates/holmes-browser/tests/manager.rs`

- [ ] **Step 1: Implement the real `BrowserManager`**

Replace the `BrowserManager` struct + `new` in `crates/holmes-browser/src/manager.rs` with:
```rust
use crate::error::{BrowserError, Result};
use crate::profile::profile_dir_for;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::page::Page;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct BrowserManager {
    session_id: String,
    sessions_dir: PathBuf,
    config: holmes_core::config::BrowserConfig,
    state: Arc<Mutex<ManagerState>>,
}

struct ManagerState {
    browser: Option<Browser>,
    page: Option<Arc<Page>>,
}

impl BrowserManager {
    pub fn new(
        session_id: &str,
        sessions_dir: &Path,
        config: holmes_core::config::BrowserConfig,
    ) -> Result<Self> {
        Ok(Self {
            session_id: session_id.to_string(),
            sessions_dir: sessions_dir.to_path_buf(),
            config,
            state: Arc::new(Mutex::new(ManagerState {
                browser: None,
                page: None,
            })),
        })
    }

    pub fn profile_dir(&self) -> PathBuf {
        profile_dir_for(&self.sessions_dir, &self.session_id)
    }

    pub async fn is_launched(&self) -> bool {
        self.state.lock().await.browser.is_some()
    }

    async fn ensure_launched(&self) -> Result<Arc<Page>> {
        let mut state = self.state.lock().await;
        if let Some(page) = state.page.clone() {
            return Ok(page);
        }
        let profile_dir = profile_dir_for(&self.sessions_dir, &self.session_id);
        tokio::fs::create_dir_all(&profile_dir).await?;

        let safe_extra = sanitize_launch_args(&self.config.extra_launch_args)?;

        let mut builder = BrowserConfig::builder()
            .no_sandbox(false)
            .arg("no-first-run")
            .arg("no-default-browser-check")
            .user_data_dir(Some(profile_dir.clone()))
            .ignore_certificate_errors(self.config.ignore_https_errors);

        // v1 is always headed (the user must see/interact). Ignore config.headless.
        builder = builder.headless(false);

        if let Some(exe) = &self.config.executable_path {
            builder = builder.chrome_executable(exe);
        }
        for arg in safe_extra {
            builder = builder.arg(arg.trim_start_matches("--"));
        }
        if let Some(proxy) = &self.config.proxy {
            builder = builder.proxy_config(chromiumoxide::handler::httphandler::ProxyConfig::new(proxy));
        }

        let cfg = builder.build().map_err(|e| BrowserError::LaunchFailed(e.to_string()))?;
        let (browser, mut handler) = Browser::launch(cfg)
            .await
            .map_err(|e| BrowserError::LaunchFailed(e.to_string()))?;

        // Drive the CDP handler on a background task.
        let _handle = tokio::spawn(async move {
            while let Some(_event) = handler.next().await {
                // drain
            }
        });

        let page = Arc::new(
            browser.new_page("about:blank").await?,
        );

        state.browser = Some(browser);
        state.page = Some(page.clone());
        Ok(page)
    }

    pub async fn navigate(&self, url: &str) -> Result<PageSnapshot> {
        let page = self.ensure_launched().await?;
        let _ = page.goto(url).await?;
        page.wait_for_navigation().await.ok();
        let title = page.title().await.unwrap_or_default();
        let url_now = page.url().await.unwrap_or_default().unwrap_or_default();
        let text = self.extract_text(&page).await?;
        Ok(PageSnapshot {
            url: url_now,
            title,
            text_excerpt: text,
        })
    }

    async fn extract_text(&self, page: &Page) -> Result<String> {
        let text: String = page
            .evaluate("document.body ? document.body.innerText : ''")
            .await
            .map(|v| v.into_value::<String>().unwrap_or_default())
            .unwrap_or_default();
        Ok(truncate(&text, self.config.content_limit))
    }

    pub async fn close(&self) {
        let mut state = self.state.lock().await;
        state.page = None;
        if let Some(browser) = state.browser.take() {
            let _ = browser.close().await;
            let _ = browser.wait().await;
        }
    }
}

fn truncate(s: &str, limit: usize) -> String {
    if s.chars().count() <= limit {
        return s.to_string();
    }
    let mut out: String = s.chars().take(limit).collect();
    out.push_str("…[truncated]");
    out
}
```

**Important note for implementer:** the exact `chromiumoxide` 0.9 builder method names (`no_sandbox`, `arg`, `user_data_dir`, `chrome_executable`, `proxy_config`, `headless`, `ignore_certificate_errors`) must be verified against the installed version's docs (`cargo doc -p chromiumoxide --open` or source). If a method name differs, adapt to the real API — the intent is: sandbox enabled, headed, isolated user data dir, optional executable/proxy. Do NOT pass any sandbox-disabling option. The `proxy_config` line in particular may need adjustment (e.g., `chromiumoxide::browser::ProxyConfig`); if it complicates this task, leave proxy wiring as a follow-up TODO only if the compiler can't resolve it, and note it.

Keep the existing `sanitize_launch_args`, `action_is_read_only`, DTOs (`PageSnapshot`, `ActionOutcome`, `Screenshot`) from previous tasks.

- [ ] **Step 2: Write an integration test (marked `#[ignore]` since it launches real Chromium)**

Create `crates/holmes-browser/tests/manager.rs`:
```rust
use holmes_browser::{BrowserManager, BrowserError};
use holmes_core::config::BrowserConfig;

fn enabled_config() -> BrowserConfig {
    BrowserConfig {
        enabled: true,
        headless: false,
        vision: false,
        content_limit: 2000,
        timeout: 30,
        proxy: None,
        ignore_https_errors: true,
        executable_path: None,
        extra_launch_args: vec![],
        screenshot_dir: None,
    }
}

#[tokio::test]
#[ignore = "launches real Chromium; run with --ignored"]
async fn manager_navigates_and_returns_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let mgr = BrowserManager::new(
        "it-session",
        tmp.path(),
        enabled_config(),
    )
    .unwrap();
    assert!(!mgr.is_launched().await);

    let snap = mgr.navigate("data:text/html,<html><head><title>T</title></head><body><p id=x>hello</p></body></html>")
        .await
        .expect("navigate");
    assert!(snap.text_excerpt.contains("hello"));
    assert!(mgr.is_launched().await);

    mgr.close().await;
    assert!(!mgr.is_launched().await);
}

#[tokio::test]
#[ignore = "launches real Chromium; run with --ignored"]
async fn manager_resume_reuses_profile_cookie() {
    let tmp = tempfile::tempdir().unwrap();
    let mgr = BrowserManager::new("resume-session", tmp.path(), enabled_config()).unwrap();
    mgr.navigate("data:text/html,<body></body>").await.unwrap();
    // document.cookie write round-trip via execute_js would go here once Task 6 lands.
    mgr.close().await;
    // Re-open on the SAME profile dir: should still be launched-capable.
    let mgr2 = BrowserManager::new("resume-session", tmp.path(), enabled_config()).unwrap();
    mgr2.navigate("data:text/html,<body>ok</body>").await.expect("re-navigate");
    mgr2.close().await;
}
```

- [ ] **Step 3: Compile-check**

```bash
cargo check -p holmes-browser
```
Fix any API mismatches against the real `chromiumoxide` 0.9 API. Iterate until it compiles.

- [ ] **Step 4: Run unit tests (non-ignored) and ignored integration tests if a local Chromium is available**

```bash
cargo test -p holmes-browser
cargo test -p holmes-browser -- --ignored || echo "no local chromium; ok"
```
Expected: unit tests pass; ignored tests pass if Chromium is available.

- [ ] **Step 5: Commit**

```bash
git add crates/holmes-browser/src/manager.rs crates/holmes-browser/tests/manager.rs
git commit -m "feat(browser): BrowserManager lazy launch + navigate + close"
```

---

## Task 6: Remaining browser actions

**Files:**
- Modify: `crates/holmes-browser/src/manager.rs`

- [ ] **Step 1: Add `click`, `fill`, `get_content`, `execute_js`, `screenshot` methods on `BrowserManager`**

Add inside the existing `impl BrowserManager` block in `crates/holmes-browser/src/manager.rs`:
```rust
pub async fn click(&self, selector: &str) -> Result<ActionOutcome> {
    let page = self.ensure_launched().await?;
    let el = page
        .find_element(selector)
        .await
        .map_err(|_| BrowserError::NotFound(selector.to_string()))?;
    el.click().await?;
    Ok(ActionOutcome {
        summary: format!("clicked {selector}"),
    })
}

pub async fn fill(&self, selector: &str, value: &str) -> Result<ActionOutcome> {
    let page = self.ensure_launched().await?;
    let el = page
        .find_element(selector)
        .await
        .map_err(|_| BrowserError::NotFound(selector.to_string()))?;
    el.click().await.ok();
    el.type_str(value).await?;
    Ok(ActionOutcome {
        summary: format!("filled {selector}"),
    })
}

pub async fn get_content(&self, selector: Option<&str>) -> Result<String> {
    let page = self.ensure_launched().await?;
    let js = match selector {
        Some(sel) => format!(
            "var e=document.querySelector({:?}); e ? e.innerText : ''",
            sel
        ),
        None => "document.body ? document.body.innerText : ''".to_string(),
    };
    let text: String = page
        .evaluate(js.as_str())
        .await
        .map(|v| v.into_value::<String>().unwrap_or_default())
        .map_err(|e| BrowserError::JsError(e.to_string()))?;
    Ok(truncate(&text, self.config.content_limit))
}

pub async fn execute_js(&self, code: &str) -> Result<serde_json::Value> {
    let page = self.ensure_launched().await?;
    page.evaluate(code)
        .await
        .map(|v| v.into_value::<serde_json::Value>().unwrap_or(serde_json::Value::Null))
        .map_err(|e| BrowserError::JsError(e.to_string()))
}

pub async fn screenshot(&self, full_page: bool) -> Result<Screenshot> {
    let page = self.ensure_launched().await?;
    let dir = self
        .config
        .screenshot_dir
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            crate::profile::profile_dir_for(&self.sessions_dir, &self.session_id)
                .parent()
                .unwrap_or(&self.sessions_dir)
                .join("browser-screenshots")
        });
    tokio::fs::create_dir_all(&dir).await?;
    let path = dir.join(format!("shot-{}.png", chrono::Utc::now().timestamp_millis()));
    let mut fmt = chromiumoxide::page::ScreenshotParams::builder();
    if full_page {
        fmt = fmt.full_page(true);
    }
    let bytes = page
        .screenshot(fmt.build())
        .await
        .map_err(|e| BrowserError::Other(e.to_string()))?;
    tokio::fs::write(&path, &bytes).await?;
    Ok(Screenshot {
        path,
        width: 0,
        height: 0,
    })
}
```

Note: `chrono` may not be a dependency of `holmes-browser`. Either add `chrono = { workspace = true }` to its `Cargo.toml`, or replace the timestamp with a counter/UUID (uuid is fine too). Pick whichever is already available; the value just needs to be unique-ish per screenshot filename.

Note: the exact `ScreenshotParams` builder / `screenshot()` signature in `chromiumoxide` 0.9 may differ (`page.screenshot(ScreenshotFormat::PNG).await?` returns `Vec<u8>` in some versions). Adapt to the real API; the intent is a PNG byte buffer written to a file.

- [ ] **Step 2: Compile-check**

```bash
cargo check -p holmes-browser
```
Iterate on API mismatches.

- [ ] **Step 3: Add an integration test for fill + get_content (ignored)**

Append to `crates/holmes-browser/tests/manager.rs`:
```rust
#[tokio::test]
#[ignore = "launches real Chromium"]
async fn fill_and_get_content_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let mgr = BrowserManager::new("fill-session", tmp.path(), enabled_config()).unwrap();
    mgr.navigate("data:text/html,<body><input id=q></body>").await.unwrap();
    mgr.fill("#q", "search-term").await.expect("fill");
    let content = mgr.get_content(Some("#q")).await.expect("get");
    assert!(content.contains("search-term"));
    mgr.close().await;
}
```

- [ ] **Step 4: Run tests**

```bash
cargo test -p holmes-browser
cargo test -p holmes-browser -- --ignored || echo "no chromium"
```

- [ ] **Step 5: Commit**

```bash
git add crates/holmes-browser/src/manager.rs crates/holmes-browser/Cargo.toml crates/holmes-browser/tests/manager.rs
git commit -m "feat(browser): click/fill/get_content/execute_js/screenshot"
```

---

## Task 7: Rewrite `BrowserTool` as thin shell + register

**Files:**
- Modify: `crates/holmes-tools/Cargo.toml`
- Modify: `crates/holmes-tools/src/builtin/browser.rs` (full rewrite)
- Modify: `crates/holmes-tools/src/builtin/mod.rs:16` (`register_all`)

- [ ] **Step 1: Add `holmes-browser` dependency to `holmes-tools`**

In `crates/holmes-tools/Cargo.toml` `[dependencies]`, add:
```toml
holmes-browser = { path = "../holmes-browser" }
```

- [ ] **Step 2: Rewrite `crates/holmes-tools/src/builtin/browser.rs`**

Replace the entire file with:
```rust
use anyhow::{anyhow, Result};
use holmes_browser::BrowserManager;
use holmes_core::{FunctionDefinition, ToolDefinition};
use serde_json::{json, Value};
use std::sync::Arc;

const VALID_ACTIONS: &[&str] = &[
    "navigate",
    "click",
    "fill",
    "screenshot",
    "get_content",
    "execute_js",
];

/// Thin tool shell over a long-lived `BrowserManager` (owned by `ChatContext`).
pub struct BrowserTool {
    manager: Arc<BrowserManager>,
}

impl BrowserTool {
    pub fn new(manager: Arc<BrowserManager>) -> Self {
        Self { manager }
    }
}

#[async_trait::async_trait]
impl holmes_core::Tool for BrowserTool {
    fn name(&self) -> &str {
        "browser"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            tool_type: "function".into(),
            function: FunctionDefinition {
                name: "browser".into(),
                description: "Drive a real headed browser. Use for JS-rendered pages, form \
                              interaction, or when a target needs a manual human step (login/2FA). \
                              Actions: navigate, click, fill, screenshot, get_content, execute_js. \
                              When a page needs a manual action you cannot/should not automate, \
                              navigate there, then use AskWatson to tell the user what to do in \
                              the browser window and what to reply when done."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": VALID_ACTIONS,
                            "description": "Browser action"
                        },
                        "url": { "type": "string", "description": "URL for navigate" },
                        "selector": { "type": "string", "description": "CSS selector for click/fill/get_content" },
                        "value": { "type": "string", "description": "Value for fill" },
                        "code": { "type": "string", "description": "JavaScript for execute_js" },
                        "full_page": { "type": "boolean", "description": "Full-page screenshot (default false)" }
                    },
                    "required": ["action"]
                }),
            },
        }
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn execute(&self, args: &str) -> Result<String> {
        let v: Value = serde_json::from_str(args)
            .map_err(|e| anyhow!("invalid browser args: {e}"))?;
        let action = v
            .get("action")
            .and_then(|a| a.as_str())
            .ok_or_else(|| anyhow!("missing action"))?
            .to_string();
        match action.as_str() {
            "navigate" => {
                let url = v.get("url").and_then(|x| x.as_str()).ok_or_else(|| anyhow!("missing url"))?;
                let snap = self.manager.navigate(url).await.map_err(|e| anyhow!(e.to_string()))?;
                Ok(format!("url: {}\ntitle: {}\n{}\n", snap.url, snap.title, snap.text_excerpt))
            }
            "click" => {
                let sel = v.get("selector").and_then(|x| x.as_str()).ok_or_else(|| anyhow!("missing selector"))?;
                let o = self.manager.click(sel).await.map_err(|e| anyhow!(e.to_string()))?;
                Ok(o.summary)
            }
            "fill" => {
                let sel = v.get("selector").and_then(|x| x.as_str()).ok_or_else(|| anyhow!("missing selector"))?;
                let val = v.get("value").and_then(|x| x.as_str()).ok_or_else(|| anyhow!("missing value"))?;
                let o = self.manager.fill(sel, val).await.map_err(|e| anyhow!(e.to_string()))?;
                Ok(o.summary)
            }
            "screenshot" => {
                let full = v.get("full_page").and_then(|x| x.as_bool()).unwrap_or(false);
                let shot = self.manager.screenshot(full).await.map_err(|e| anyhow!(e.to_string()))?;
                Ok(format!("screenshot: {}", shot.path.display()))
            }
            "get_content" => {
                let sel = v.get("selector").and_then(|x| x.as_str());
                self.manager.get_content(sel).await.map_err(|e| anyhow!(e.to_string()))
            }
            "execute_js" => {
                let code = v.get("code").and_then(|x| x.as_str()).ok_or_else(|| anyhow!("missing code"))?;
                let val = self.manager.execute_js(code).await.map_err(|e| anyhow!(e.to_string()))?;
                Ok(serde_json::to_string_pretty(&val).unwrap_or_else(|_| val.to_string()))
            }
            other => Err(anyhow!("unknown browser action: {other}")),
        }
    }
}
```

Note: verify the exact path of the `Tool` trait re-export. Existing code uses `use crate::registry::Tool;` and `holmes_core::{ContentBlock, FunctionDefinition, ToolDefinition}`. The trait itself is in `holmes_tools::registry::Tool`. Adjust the `impl` bound to `impl crate::registry::Tool for BrowserTool` to match the file's existing style. The `async_trait::async_trait` macro is already used in this crate.

- [ ] **Step 3: Update `register_all` to accept an optional browser manager**

In `crates/holmes-tools/src/builtin/mod.rs`, change the signature and body:
```rust
use holmes_browser::BrowserManager;
use std::sync::Arc;

pub fn register_all(
    registry: &mut ToolRegistry,
    _config: &HolmesConfig,
    runner: Option<Arc<dyn SubagentRunner>>,
    browser: Option<Arc<BrowserManager>>,
) {
    registry.register(Box::new(execute_command::ExecuteCommandTool));
    registry.register(Box::new(execute_python::ExecutePythonTool));
    registry.register(Box::new(http_request::HttpRequestTool::new()));
    registry.register(Box::new(report_finding::ReportFindingTool));
    registry.register(Box::new(report_progress::ReportProgressTool));
    registry.register(Box::new(report_recon::ReportReconTool));
    registry.register(Box::new(hypothesis::AddHypothesisTool));
    registry.register(Box::new(hypothesis::RejectHypothesisTool));
    registry.register(Box::new(hypothesis::ConfirmHypothesisTool));

    if let Some(r) = runner {
        registry.register(Box::new(subagent::SpawnSubagentTool::new(r)));
    }
    if let Some(mgr) = browser {
        registry.register(Box::new(browser::BrowserTool::new(mgr)));
    }
}
```
Add the necessary imports at the top of the file (`use holmes_browser::BrowserManager;`, `use std::sync::Arc;`, `use crate::registry::ToolRegistry;` — match existing style).

- [ ] **Step 4: Update all `register_all` call sites to pass a 4th argument**

Search and update:
```bash
rg -n "register_all\(" crates/
```
For each caller (the main one is `crates/holmes-cli/src/chat.rs` inside `build_tool_registry`), pass `browser` through. In Task 8 we thread `browser: Option<Arc<BrowserManager>>` into `build_tool_registry`; for this task, update the call inside `build_tool_registry` to pass a parameter it will receive. To keep this task compiling in isolation, pass `None` for now and Task 8 fills the real value:
```rust
holmes_tools::builtin::register_all(&mut registry, config, runner, browser_arg);
```
where `build_tool_registry` gains a `browser: Option<Arc<BrowserManager>>` parameter now (add it). Update all callers of `build_tool_registry` to pass `None` temporarily (Task 8 makes them real). Use `rg -n "build_tool_registry\(" crates/` to find them.

- [ ] **Step 5: Compile-check the workspace**

```bash
cargo check --workspace 2>&1 | tail -30
```
Iterate until it compiles. Run `cargo test -p holmes-tools` to ensure existing tool tests still pass.

- [ ] **Step 6: Commit**

```bash
git add crates/holmes-tools
git commit -m "feat(tools): rewrite BrowserTool as thin shell, register via manager"
```

---

## Task 8: Wire `ChatContext.browser`, `create_chat_context`, `/browser close`

**Files:**
- Modify: `crates/holmes-cli/src/chat.rs`
- Modify: `crates/holmes-cli/src/commands.rs` (if slash command handlers live here) — verify location with `rg -n "\"browser\"|/browser" crates/holmes-cli/src/`

- [ ] **Step 1: Add `browser` field to `ChatContext`**

In `crates/holmes-cli/src/chat.rs`, find `pub struct ChatContext` (~line 223) and add:
```rust
pub browser: Option<Arc<holmes_browser::BrowserManager>>,
```

- [ ] **Step 2: Thread `browser` through `build_tool_registry`**

Update `build_tool_registry` signature to add `browser: Option<Arc<holmes_browser::BrowserManager>>`, and pass it to `register_all`. Update ALL callers of `build_tool_registry` (`rg -n "build_tool_registry\(" crates/holmes-cli/src/chat.rs`) — most can pass `None` except the main one in `create_chat_context` (Step 3) and the `/new` + `/branch` reload paths, which should pass `ctx.browser.clone()`.

- [ ] **Step 3: Construct the manager in `create_chat_context`**

In `create_chat_context` (after `session_id` is known, before `build_tool_registry`), add:
```rust
let browser: Option<Arc<holmes_browser::BrowserManager>> = if ctx_enabled.browser.enabled {
    let sessions_dir = data_dir.join("sessions");
    match holmes_browser::BrowserManager::new(&session_id, &sessions_dir, ctx_enabled.browser.clone()) {
        Ok(mgr) => Some(Arc::new(mgr)),
        Err(e) => {
            eprintln!("Warning: browser disabled: {e}");
            None
        }
    }
} else {
    None
};
```
(`ctx_enabled` is the local `config` variable; use the actual name.) Add this to the `ChatContext { ... }` literal: `browser: browser.clone(),`.

Pass `browser` into the main `build_tool_registry(...)` call here. For `/new`, `/branch`, `/tree fork` rebuild paths, pass `ctx.browser.clone()`.

- [ ] **Step 4: Add `/browser close` slash command**

Find the slash-command dispatch (verify with `rg -n "\"compress\"|\"branch\"" crates/holmes-cli/src/chat.rs`). Add a new arm near them:
```rust
"browser" => {
    match args.trim() {
        "close" => {
            if let Some(mgr) = ctx.browser.as_ref() {
                mgr.close().await;
                println!("Browser closed; next browser action will relaunch.");
            } else {
                println!("Browser is not enabled in config.");
            }
        }
        other => {
            println!("Usage: /browser close");
            println!("Unknown subcommand: {other}");
        }
    }
    SlashResult::Handled
}
```

- [ ] **Step 5: Compile-check**

```bash
cargo check --workspace 2>&1 | tail -30
```

- [ ] **Step 6: Register the `/browser` command in `CommandRegistry` if it has a registry list**

Search `rg -n "CommandRegistry|all_command_hints|/compress" crates/holmes-cli/src/commands.rs` and add a `/browser` entry alongside `/compress` so it appears in tab-completion and `/help`.

- [ ] **Step 7: Commit**

```bash
git add crates/holmes-cli/src/chat.rs crates/holmes-cli/src/commands.rs
git commit -m "feat(cli): wire BrowserManager into ChatContext + /browser close"
```

---

## Task 9: `BrowserReadOnlyMiddleware` + install

**Files:**
- Modify: `crates/holmes-runtime/src/middleware.rs`
- Modify: `crates/holmes-cli/src/chat.rs` (install in both runtime builders)

- [ ] **Step 1: Write the failing test**

Append to `crates/holmes-runtime/src/middleware.rs` test module:
```rust
#[cfg(test)]
mod browser_mw_tests {
    use super::*;
    use serde_json::json;

    fn make_args(action: &str) -> serde_json::Value {
        json!({ "action": action })
    }

    #[tokio::test]
    async fn read_only_mode_blocks_write_actions() {
        let mw = BrowserReadOnlyMiddleware;
        let mut ctx = test_context_with_mode(crate::permissions::PermissionMode::ReadOnly);
        let mut name = "browser".to_string();
        let mut args = make_args("click");
        let res = mw.before_tool_call(&mut ctx, &mut name, &mut args).await;
        assert!(res.is_err(), "click must be blocked under read_only");
    }

    #[tokio::test]
    async fn read_only_mode_permits_read_actions() {
        let mw = BrowserReadOnlyMiddleware;
        let mut ctx = test_context_with_mode(crate::permissions::PermissionMode::ReadOnly);
        let mut name = "browser".to_string();
        let mut args = make_args("get_content");
        let res = mw.before_tool_call(&mut ctx, &mut name, &mut args).await;
        assert!(res.is_ok(), "get_content must be allowed under read_only");
    }

    #[tokio::test]
    async fn non_browser_tool_is_ignored() {
        let mw = BrowserReadOnlyMiddleware;
        let mut ctx = test_context_with_mode(crate::permissions::PermissionMode::ReadOnly);
        let mut name = "http_request".to_string();
        let mut args = make_args("click");
        let res = mw.before_tool_call(&mut ctx, &mut name, &mut args).await;
        assert!(res.is_ok());
    }
}
```

Note: `test_context_with_mode` and the exact `PermissionMode` enum path depend on the runtime's test helpers. Inspect existing middleware tests in this file for the right pattern and adapt. The key is: under `read_only`, `browser` with a write action errors; with a read action it is allowed; non-`browser` tools pass through.

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p holmes-runtime browser_mw
```
Expected: FAIL (`BrowserReadOnlyMiddleware` does not exist).

- [ ] **Step 3: Implement `BrowserReadOnlyMiddleware`**

Add to `crates/holmes-runtime/src/middleware.rs` (alongside the existing built-in middlewares):
```rust
pub struct BrowserReadOnlyMiddleware;

#[async_trait::async_trait]
impl RuntimeMiddleware for BrowserReadOnlyMiddleware {
    async fn before_tool_call(
        &self,
        ctx: &mut RuntimeContext,
        tool_name: &mut String,
        args: &mut serde_json::Value,
    ) -> Result<(), RuntimeError> {
        if tool_name != "browser" {
            return Ok(());
        }
        // Only enforce under read-only permission mode.
        let is_read_only = matches!(
            ctx.config.permissions.mode,
            holmes_core::config::PermissionMode::ReadOnly
        );
        if !is_read_only {
            return Ok(());
        }
        let action = args
            .get("action")
            .and_then(|a| a.as_str())
            .unwrap_or("");
        if matches!(action, "click" | "fill" | "execute_js") {
            return Err(RuntimeError::recoverable(format!(
                "browser action '{action}' is a write and is blocked under read_only permission mode"
            )));
        }
        Ok(())
    }
}
```

Note: verify the real `PermissionMode` enum path and variant names in `holmes-core/src/config.rs` (`rg -n "enum PermissionMode|ReadOnly|read_only" crates/holmes-core/src/config.rs`). The config `permissions.mode` field type is `PermissionMode`; adapt the match to the real variant spelling.

- [ ] **Step 4: Install the middleware in both runtime builders**

In `crates/holmes-cli/src/chat.rs`, in `run_runtime_input_with_sink` (~line 923) AND the second builder (`run_runtime_input`, ~line 970), after `let mut runtime = AgentRuntime::new(runtime_context);`:
```rust
// before run_oneshot/run_turn, install browser guard if browser is enabled
let middlewares: Vec<Arc<dyn RuntimeMiddleware>> = if ctx.browser.is_some() {
    vec![Arc::new(holmes_runtime::middleware::BrowserReadOnlyMiddleware)]
} else {
    Vec::new()
};
runtime.context_mut().middlewares.extend(middlewares);
```
(Verify `context_mut()` exists — `rg -n "fn context_mut" crates/holmes-runtime/src/runtime.rs`. If `RuntimeContext.middlewares` is public, set it directly. Alternatively, build the context with `.with_middlewares(...)` before `AgentRuntime::new` — preferred. Adjust to whichever the real API supports.)

- [ ] **Step 5: Run tests + compile-check**

```bash
cargo test -p holmes-runtime browser_mw
cargo check --workspace 2>&1 | tail -30
```

- [ ] **Step 6: Commit**

```bash
git add crates/holmes-runtime/src/middleware.rs crates/holmes-cli/src/chat.rs
git commit -m "feat(runtime): BrowserReadOnlyMiddleware gates write actions"
```

---

## Task 10: Handoff prompt + config template

**Files:**
- Modify: `crates/holmes-cli/src/chat.rs` (`SYSTEM_PROMPT` or `project_knowledge.rs`)
- Modify: `config.default.yaml`

- [ ] **Step 1: Add handoff guidance to the system prompt**

Find `SYSTEM_PROMPT` (`rg -n "const SYSTEM_PROMPT" crates/holmes-cli/src/chat.rs`) or the `NATIVE_CAPABILITIES` block in `project_knowledge.rs`. Append a section that is unconditional (the agent only sees the `browser` tool if it is registered, so the guidance is inert when browser is off):
```text
## Browser tool (when available)
- The `browser` tool drives a long-lived headed browser that stays open across turns.
- When a page requires a manual human step (login, 2FA, CAPTCHA, or anything you should not automate), first `browser navigate` to the page, then emit AskWatson describing exactly what the user must do in the browser window and what to reply when done (e.g. "continue"). The browser stays open; after the user replies, continue operating on the same authenticated page.
- Never put credentials you observe in the browser into findings or memory unless explicitly asked; treat the user's authenticated session as out-of-scope evidence.
```

- [ ] **Step 2: Update `config.default.yaml` browser block**

Replace the existing `browser:` block with:
```yaml
browser:
  enabled: false
  headless: false           # v1 always launches headed; field kept for compatibility
  vision: false
  content_limit: 5000
  timeout: 30
  proxy: null
  ignore_https_errors: true
  executable_path: null     # set to a Chromium binary path to override auto-detection
  extra_launch_args: []     # NOTE: --no-sandbox / --disable-web-security are rejected
  screenshot_dir: null      # default: sessions/<id>/browser-screenshots
```

- [ ] **Step 3: Compile + commit**

```bash
cargo check --workspace
git add crates/holmes-cli/src/chat.rs crates/holmes-cli/src/project_knowledge.rs config.default.yaml
git commit -m "feat(cli): browser handoff prompt + config template"
```

---

## Task 11: Final verification

**Files:** none

- [ ] **Step 1: Full workspace test (single-threaded to avoid the known project_knowledge parallel flake)**

```bash
cargo test --workspace -- --test-threads=1 2>&1 | tail -30
```
Expected: all pass.

- [ ] **Step 2: Release build**

```bash
cargo build --release 2>&1 | tail -5
```
Expected: Finished.

- [ ] **Step 3: Manual smoke test (documented, not automated)**

Run with a config that has `browser.enabled: true`:
```bash
./target/release/holmes -q "navigate to https://example.com and get_content"
```
Verify a headed Chromium opens, the agent reports the page text, and the browser stays open until `/browser close` or `/quit`. (Operator runs this manually; record result in the commit message or a test-results note.)

- [ ] **Step 4: Commit any final fixes**

```bash
git add -A
git commit -m "test(browser): final verification"
```

---

## Self-Review

### Spec coverage

- New `holmes-browser` crate: Task 1.
- `profile_dir_for` + resume/fork profile semantics: Task 2 (resume reuse test in Task 5; fork isolation = per-session id, covered by design).
- `BrowserConfig` extension (remove mcp_*): Task 3.
- Chromium sandbox always on + `extra_launch_args` sanitizer: Task 4 (+ test).
- `BrowserManager` lazy launch / navigate / close: Task 5.
- Remaining actions (click/fill/get_content/execute_js/screenshot): Task 6.
- Thin `BrowserTool` + register: Task 7.
- `ChatContext.browser` + construction + `/browser close`: Task 8.
- `BrowserReadOnlyMiddleware` + install: Task 9.
- Handoff via `AskWatson` (prompt-only, no runtime change): Task 10.
- `config.default.yaml`: Task 10.
- Release build + tests: Task 11.
- Subagent not sharing browser: explicitly deferred (spec non-goal) — no task needed.

### Placeholder scan

Concrete code in every step. Where the exact `chromiumoxide` 0.9 API is uncertain (launch builder method names, `ScreenshotParams`), the task explicitly tells the implementer to verify against installed docs and adapt — this is not a placeholder, it is a known API-verification point with the intent spelled out.

### Type consistency

`BrowserManager::new(session_id, sessions_dir, config)` signature consistent across Tasks 1/5/8. `BrowserTool::new(Arc<BrowserManager>)` consistent across Tasks 1/7/8. `register_all(..., browser: Option<Arc<BrowserManager>>)` consistent across Tasks 7/8. `BrowserReadOnlyMiddleware` consistent across Tasks 9. `BrowserConfig` fields consistent across Tasks 3/5/6.
