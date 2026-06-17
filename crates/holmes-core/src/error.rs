use std::fmt;

#[derive(Debug)]
pub enum ApeironError {
    Llm(LlmError),
    Tool(ToolError),
    Config(String),
    Guard(String),
    Io(std::io::Error),
}

#[derive(Debug)]
pub struct LlmError {
    pub provider: String,
    pub status: Option<u16>,
    pub message: String,
    pub retryable: bool,
}

#[derive(Debug)]
pub struct ToolError {
    pub tool_name: String,
    pub message: String,
}

impl fmt::Display for ApeironError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Llm(e) => write!(f, "LLM error ({}): {}", e.provider, e.message),
            Self::Tool(e) => write!(f, "Tool error ({}): {}", e.tool_name, e.message),
            Self::Config(msg) => write!(f, "Config error: {msg}"),
            Self::Guard(msg) => write!(f, "Guard error: {msg}"),
            Self::Io(e) => write!(f, "IO error: {e}"),
        }
    }
}

impl std::error::Error for ApeironError {}

impl From<std::io::Error> for ApeironError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl LlmError {
    pub fn retryable(provider: impl Into<String>, status: u16, message: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            status: Some(status),
            message: message.into(),
            retryable: matches!(status, 429 | 500 | 502 | 503 | 504 | 524),
        }
    }

    pub fn non_retryable(provider: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            status: None,
            message: message.into(),
            retryable: false,
        }
    }
}

pub type Result<T> = std::result::Result<T, ApeironError>;
