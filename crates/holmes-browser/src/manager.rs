use crate::error::{BrowserError, Result};
use crate::profile::profile_dir_for;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::page::Page;
use futures::StreamExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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
        builder = builder.arg("no-first-run");
        builder = builder.arg("no-default-browser-check");
        if self.config.ignore_https_errors {
            builder = builder.arg("ignore-certificate-errors");
        }
        if let Some(exe) = &self.config.executable_path {
            builder = builder.chrome_executable(exe);
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
        let (browser, mut handler) = Browser::launch(cfg)
            .await
            .map_err(|e| BrowserError::LaunchFailed(e.to_string()))?;

        // Drive the CDP event loop on a background task. This must live for the
        // lifetime of the browser.
        tokio::spawn(async move {
            loop {
                match handler.next().await {
                    Some(_) => continue,
                    None => break,
                }
            }
        });

        let page = Arc::new(browser.new_page("about:blank").await?);

        state.browser = Some(browser);
        state.page = Some(page.clone());
        Ok(page)
    }

    pub async fn navigate(&self, url: &str) -> Result<PageSnapshot> {
        let page = self.ensure_launched().await?;
        page.goto(url).await?;
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
            Some(sel) => {
                format!("var e=document.querySelector({:?}); e ? e.innerText : ''", sel)
            }
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
            .map(|v| {
                v.into_value::<serde_json::Value>().unwrap_or(serde_json::Value::Null)
            })
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
        let bytes = page.screenshot(builder.build()).await?;
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
        if let Some(mut browser) = state.browser.take() {
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
