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
