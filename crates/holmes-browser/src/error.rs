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
    Cdp(String),
    #[error("other: {0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, BrowserError>;
