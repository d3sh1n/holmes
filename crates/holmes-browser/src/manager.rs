use crate::error::{BrowserError, Result};
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
