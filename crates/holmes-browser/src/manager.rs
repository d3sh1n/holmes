use crate::error::{BrowserError, Result};
use crate::profile::profile_dir_for;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::page::Page;
use futures::StreamExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

#[derive(Debug)]
pub struct PageSnapshot {
    pub url: String,
    pub title: String,
    pub text_excerpt: String,
}

#[derive(Debug)]
pub struct ActionOutcome {
    pub summary: String,
}

#[derive(Debug)]
pub struct Screenshot {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
}

pub struct BrowserManager {
    session_id: String,
    sessions_dir: PathBuf,
    config: holmes_core::config::BrowserConfig,
    state: Arc<Mutex<ManagerState>>,
}

struct ManagerState {
    browser: Option<Browser>,
    page: Option<Arc<Page>>,
    /// `true` when we launched the browser (we own its lifecycle and may kill
    /// it on `close`). `false` when we attached to an external Chrome via CDP
    /// — closing then only drops our handle, never the user's browser.
    owned: bool,
}

/// Max wait for a cached-page liveness probe before treating the CDP
/// connection as dead and relaunching. The background handler task can die
/// (browser crash, socket drop, anti-bot kill) and leave a stale page handle;
/// this probe lets us detect that and recover instead of failing forever with
/// "receiver gone".
const LIVENESS_PROBE: Duration = Duration::from_millis(1000);

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
                owned: true,
            })),
        })
    }

    pub fn profile_dir(&self) -> PathBuf {
        profile_dir_for(&self.sessions_dir, &self.session_id)
    }

    pub async fn is_launched(&self) -> bool {
        self.state.lock().await.browser.is_some()
    }

    /// Per-action timeout derived from `config.timeout` (clamped to >=1s).
    fn timeout_dur(&self) -> Duration {
        Duration::from_secs(self.config.timeout.max(1) as u64)
    }

    async fn ensure_launched(&self) -> Result<Arc<Page>> {
        // Fast path: a cached page is only usable if its CDP handler is still
        // alive. If the background handler task died, every action would fail
        // with "receiver gone" and the agent's retries could never recover.
        // Probe the cached page cheaply; on failure discard the stale handle
        // and fall through to a fresh launch (same per-session profile, so any
        // manual login survives the relaunch).
        if let Some(page) = self.state.lock().await.page.clone() {
            let alive = tokio::time::timeout(LIVENESS_PROBE, page.evaluate("1"))
                .await
                .map(|r| r.is_ok())
                .unwrap_or(false);
            if alive {
                return Ok(page);
            }
            let mut state = self.state.lock().await;
            state.page = None;
            state.browser = None;
        }

        let mut state = self.state.lock().await;

        // Decide attach-vs-launch. Attaching reuses the user's real Chrome
        // (profile, login, fingerprint) and defeats strong anti-bot systems
        // that would block an automation-launched browser.
        let (browser, mut handler, owned) = if let Some(endpoint) =
            self.config.cdp_endpoint.as_ref()
        {
            let (b, h) = tokio::time::timeout(self.timeout_dur(), Browser::connect(endpoint.clone()))
                .await
                .map_err(|_| BrowserError::Timeout(self.config.timeout))?
                .map_err(|e| BrowserError::LaunchFailed(format!("cdp connect {endpoint}: {e}")))?;
            (b, h, false)
        } else {
            let profile_dir = profile_dir_for(&self.sessions_dir, &self.session_id);
            tokio::fs::create_dir_all(&profile_dir).await?;

            let safe_extra = sanitize_launch_args(&self.config.extra_launch_args)?;
            // chromiumoxide's `arg(...)` renders as `--<arg>`, so strip any leading
            // `--` the user supplied to avoid `----name`.
            let normalized: Vec<String> = safe_extra
                .into_iter()
                .map(|a| a.trim_start_matches('-').to_string())
                .collect();

            let mut builder = BrowserConfig::builder();
            // v1 is always headed: the user must see and interact with the window.
            builder = builder.with_head();
            builder = builder.user_data_dir(profile_dir.clone());
            builder = builder.launch_timeout(self.timeout_dur());
            builder = builder.arg("no-first-run");
            builder = builder.arg("no-default-browser-check");
            // Baseline anti-fingerprint hardening for launched mode. This is a
            // best-effort nudge for light anti-bot targets; strong bot managers
            // still require attach mode (real browser fingerprint).
            builder = builder.arg("disable-blink-features=AutomationControlled");
            if self.config.ignore_https_errors {
                builder = builder.arg("ignore-certificate-errors");
            }
            if let Some(proxy) = &self.config.proxy {
                // Chrome proxy is a launch flag; chromiumoxide has no builder method for it.
                builder = builder.arg(format!("proxy-server={}", proxy));
            }
            if let Some(exe) = &self.config.executable_path {
                builder = builder.chrome_executable(exe);
            } else if let Some(system_chrome) = detect_system_chrome() {
                // Prefer the user's real Chrome/Edge over the (often older,
                // more fingerprintable) Chromium chromiumoxide would otherwise
                // download. Real binaries defeat strong anti-bot fingerprinting.
                builder = builder.chrome_executable(system_chrome);
            }
            for a in normalized {
                builder = builder.arg(a);
            }
            // The built-in Chromium sandbox stays on: we intentionally do NOT call
            // `no_sandbox()`. `sanitize_launch_args` already rejected user-supplied
            // sandbox-disabling flags.

            let cfg = builder
                .build()
                .map_err(|e| BrowserError::LaunchFailed(e.to_string()))?;
            let (b, h) = Browser::launch(cfg)
                .await
                .map_err(|e| BrowserError::LaunchFailed(e.to_string()))?;
            (b, h, true)
        };

        // Drive the CDP event loop on a background task. This must live for the
        // lifetime of the browser handle.
        tokio::spawn(async move {
            loop {
                match handler.next().await {
                    Some(_) => continue,
                    None => break,
                }
            }
        });

        let new_page_fut = browser.new_page("about:blank");
        let page = Arc::new(
            tokio::time::timeout(self.timeout_dur(), new_page_fut)
                .await
                .map_err(|_| BrowserError::Timeout(self.config.timeout))??,
        );

        // Stealth init script: hide the webdriver flag and fake common tells.
        // Harmless on an attached real Chrome; useful for launched mode.
        let _ = page.evaluate_on_new_document(STEALTH_JS).await;

        state.browser = Some(browser);
        state.page = Some(page.clone());
        state.owned = owned;
        Ok(page)
    }

    pub async fn navigate(&self, url: &str) -> Result<PageSnapshot> {
        let page = self.ensure_launched().await?;
        let dur = self.timeout_dur();
        tokio::time::timeout(dur, page.goto(url))
            .await
            .map_err(|_| BrowserError::Timeout(self.config.timeout))??;
        let title = page
            .evaluate("document.title")
            .await
            .map(|v| v.into_value::<String>().unwrap_or_default())
            .unwrap_or_default();
        let url_now = page
            .url()
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
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

    pub async fn click(&self, selector: &str) -> Result<ActionOutcome> {
        let page = self.ensure_launched().await?;
        let el = tokio::time::timeout(self.timeout_dur(), page.find_element(selector))
            .await
            .map_err(|_| BrowserError::Timeout(self.config.timeout))?
            .map_err(|_| BrowserError::NotFound(selector.to_string()))?;
        tokio::time::timeout(self.timeout_dur(), el.click())
            .await
            .map_err(|_| BrowserError::Timeout(self.config.timeout))??;
        Ok(ActionOutcome {
            summary: format!("clicked {selector}"),
        })
    }

    pub async fn fill(&self, selector: &str, value: &str) -> Result<ActionOutcome> {
        let page = self.ensure_launched().await?;
        let el = tokio::time::timeout(self.timeout_dur(), page.find_element(selector))
            .await
            .map_err(|_| BrowserError::Timeout(self.config.timeout))?
            .map_err(|_| BrowserError::NotFound(selector.to_string()))?;
        el.click().await.ok();
        tokio::time::timeout(self.timeout_dur(), el.type_str(value))
            .await
            .map_err(|_| BrowserError::Timeout(self.config.timeout))??;
        Ok(ActionOutcome {
            summary: format!("filled {selector}"),
        })
    }

    pub async fn get_content(&self, selector: Option<&str>) -> Result<String> {
        let page = self.ensure_launched().await?;
        let js = match selector {
            Some(sel) => {
                format!("var e=document.querySelector({:?}); e ? e.innerText : ''", sel)
            }
            None => "document.body ? document.body.innerText : ''".to_string(),
        };
        let result = tokio::time::timeout(self.timeout_dur(), page.evaluate(js.as_str()))
            .await
            .map_err(|_| BrowserError::Timeout(self.config.timeout))?
            .map_err(|e| BrowserError::JsError(e.to_string()))?;
        let text: String = result.into_value::<String>().unwrap_or_default();
        Ok(truncate(&text, self.config.content_limit))
    }

    pub async fn execute_js(&self, code: &str) -> Result<serde_json::Value> {
        let page = self.ensure_launched().await?;
        let result = tokio::time::timeout(self.timeout_dur(), page.evaluate(code))
            .await
            .map_err(|_| BrowserError::Timeout(self.config.timeout))?
            .map_err(|e| BrowserError::JsError(e.to_string()))?;
        Ok(result
            .into_value::<serde_json::Value>()
            .unwrap_or(serde_json::Value::Null))
    }

    pub async fn screenshot(&self, full_page: bool) -> Result<Screenshot> {
        let page = self.ensure_launched().await?;
        let dir = self
            .config
            .screenshot_dir
            .clone()
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                profile_dir_for(&self.sessions_dir, &self.session_id)
                    .parent()
                    .unwrap_or(&self.sessions_dir)
                    .join("browser-screenshots")
            });
        tokio::fs::create_dir_all(&dir).await?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let path = dir.join(format!("shot-{stamp}.png"));

        let mut builder = chromiumoxide::page::ScreenshotParams::builder();
        if full_page {
            builder = builder.full_page(true);
        }
        let bytes = tokio::time::timeout(self.timeout_dur(), page.screenshot(builder.build()))
            .await
            .map_err(|_| BrowserError::Timeout(self.config.timeout))??;
        tokio::fs::write(&path, &bytes).await?;
        Ok(Screenshot {
            path,
            width: 0,
            height: 0,
        })
    }

    pub async fn close(&self) {
        let mut state = self.state.lock().await;
        state.page = None;
        let owned = state.owned;
        if let Some(browser) = state.browser.take() {
            if owned {
                // We launched it: shut the whole browser down.
                let mut b = browser;
                let _ = b.close().await;
                let _ = b.wait().await;
            }
            // Attach mode: just drop the handle (disconnects the WebSocket).
            // We must NOT call `Browser::close` — that sends `Browser.close`
            // over CDP and would kill the user's real Chrome (every tab).
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

/// Init script injected before each document load to hide automation tells.
/// Best-effort baseline for launched mode; attach mode needs it less since the
/// browser fingerprint is already real.
const STEALTH_JS: &str = r#"
(() => {
  try { Object.defineProperty(navigator, 'webdriver', { get: () => undefined }); } catch (e) {}
  try { Object.defineProperty(navigator, 'languages', { get: () => ['zh-CN', 'zh', 'en-US', 'en'] }); } catch (e) {}
  try { Object.defineProperty(navigator, 'plugins', { get: () => [{}, {}, {}, {}, {}] }); } catch (e) {}
  try { window.chrome = window.chrome || { runtime: {} }; } catch (e) {}
})();
"#;

/// Probe common install locations for a real Chrome/Edge/Chromium binary.
/// Returns the first existing path. Used so that, by default, we launch the
/// user's real browser (real fingerprint) rather than the Chromium
/// chromiumoxide would download — the latter is easily fingerprinted by strong
/// anti-bot systems (e.g. Akamai Bot Manager on sites like xiaohongshu.com).
pub fn detect_system_chrome() -> Option<std::path::PathBuf> {
    let candidates: &[&str] = &[
        // macOS
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
        "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
        // Linux
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/usr/bin/microsoft-edge",
        "/usr/bin/brave-browser",
        "/snap/bin/chromium",
        // Windows
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
    ];
    for c in candidates {
        if std::path::Path::new(c).exists() {
            return Some(std::path::PathBuf::from(c));
        }
    }
    None
}

const READ_ONLY_ACTIONS: &[&str] = &["navigate", "screenshot", "get_content"];

const FORBIDDEN_LAUNCH_FLAGS: &[&str] = &[
    "--no-sandbox",
    "--disable-web-security",
    "--disable-setuid-sandbox",
    "--disable-site-isolation-trials",
    "--allow-running-insecure-content",
];

/// Reject sandbox-disabling launch args. The Chromium built-in sandbox must
/// stay on; users cannot disable it via config.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_no_sandbox_flag() {
        assert!(sanitize_launch_args(&["--no-sandbox".to_string()]).is_err());
        assert!(sanitize_launch_args(&["--disable-web-security".to_string()]).is_err());
        assert!(sanitize_launch_args(&["--disable-setuid-sandbox".to_string()]).is_err());
        assert!(sanitize_launch_args(&[
            "--disable-site-isolation-trials".to_string()
        ])
        .is_err());
        assert!(sanitize_launch_args(&[
            "--allow-running-insecure-content".to_string()
        ])
        .is_err());
    }

    #[test]
    fn rejects_flag_with_value_suffix() {
        assert!(sanitize_launch_args(&["--no-sandbox=1".to_string()]).is_err());
    }

    #[test]
    fn permits_benign_args() {
        let out = sanitize_launch_args(&[
            "--lang=en".to_string(),
            "--window-size=1280,720".to_string(),
        ])
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
