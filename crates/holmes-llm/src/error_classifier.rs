use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FailoverReason {
    RateLimit,
    Overloaded,
    ServerError,
    Timeout,
    ContextOverflow,
    Auth,
    Billing,
    ModelNotFound,
    FormatError,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct ClassifiedError {
    pub reason: FailoverReason,
    pub status_code: Option<u16>,
    pub message: String,
    pub retryable: bool,
    pub should_compress: bool,
    pub should_fallback: bool,
}

impl ClassifiedError {
    pub fn from_status_and_body(status: u16, body: &str) -> Self {
        let lower = body.to_lowercase();

        match status {
            429 => {
                let is_billing = BILLING_PATTERNS.iter().any(|p| lower.contains(p));
                if is_billing {
                    Self {
                        reason: FailoverReason::Billing,
                        status_code: Some(429),
                        message: body.chars().take(200).collect(),
                        retryable: false,
                        should_compress: false,
                        should_fallback: true,
                    }
                } else {
                    Self {
                        reason: FailoverReason::RateLimit,
                        status_code: Some(429),
                        message: body.chars().take(200).collect(),
                        retryable: true,
                        should_compress: false,
                        should_fallback: true,
                    }
                }
            }
            400 => {
                let is_context = CONTEXT_OVERFLOW_PATTERNS.iter().any(|p| lower.contains(p));
                if is_context {
                    Self {
                        reason: FailoverReason::ContextOverflow,
                        status_code: Some(400),
                        message: body.chars().take(200).collect(),
                        retryable: false,
                        should_compress: true,
                        should_fallback: false,
                    }
                } else {
                    Self {
                        reason: FailoverReason::FormatError,
                        status_code: Some(400),
                        message: body.chars().take(200).collect(),
                        retryable: false,
                        should_compress: false,
                        should_fallback: false,
                    }
                }
            }
            401 | 403 => Self {
                reason: FailoverReason::Auth,
                status_code: Some(status),
                message: body.chars().take(200).collect(),
                retryable: false,
                should_compress: false,
                should_fallback: true,
            },
            402 => Self {
                reason: FailoverReason::Billing,
                status_code: Some(402),
                message: body.chars().take(200).collect(),
                retryable: false,
                should_compress: false,
                should_fallback: true,
            },
            404 => Self {
                reason: FailoverReason::ModelNotFound,
                status_code: Some(404),
                message: body.chars().take(200).collect(),
                retryable: false,
                should_compress: false,
                should_fallback: true,
            },
            500 | 502 => Self {
                reason: FailoverReason::ServerError,
                status_code: Some(status),
                message: body.chars().take(200).collect(),
                retryable: true,
                should_compress: false,
                should_fallback: false,
            },
            503 | 529 => Self {
                reason: FailoverReason::Overloaded,
                status_code: Some(status),
                message: body.chars().take(200).collect(),
                retryable: true,
                should_compress: false,
                should_fallback: true,
            },
            504 | 524 => Self {
                reason: FailoverReason::Timeout,
                status_code: Some(status),
                message: body.chars().take(200).collect(),
                retryable: true,
                should_compress: false,
                should_fallback: false,
            },
            _ => Self {
                reason: FailoverReason::Unknown,
                status_code: Some(status),
                message: body.chars().take(200).collect(),
                retryable: true,
                should_compress: false,
                should_fallback: false,
            },
        }
    }

    pub fn timeout(message: impl Into<String>) -> Self {
        Self {
            reason: FailoverReason::Timeout,
            status_code: None,
            message: message.into(),
            retryable: true,
            should_compress: false,
            should_fallback: false,
        }
    }
}

const BILLING_PATTERNS: &[&str] = &[
    "insufficient credits",
    "insufficient_quota",
    "credit balance",
    "credits have been exhausted",
    "payment required",
    "billing hard limit",
    "exceeded your current quota",
];

const CONTEXT_OVERFLOW_PATTERNS: &[&str] = &[
    "maximum context length",
    "context_length_exceeded",
    "token limit",
    "too many tokens",
    "context window",
    "max_tokens",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_429() {
        let err = ClassifiedError::from_status_and_body(429, "rate limit exceeded");
        assert_eq!(err.reason, FailoverReason::RateLimit);
        assert!(err.retryable);
        assert!(err.should_fallback);
    }

    #[test]
    fn billing_429() {
        let err = ClassifiedError::from_status_and_body(429, "insufficient credits remaining");
        assert_eq!(err.reason, FailoverReason::Billing);
        assert!(!err.retryable);
        assert!(err.should_fallback);
    }

    #[test]
    fn context_overflow_400() {
        let err = ClassifiedError::from_status_and_body(400, "maximum context length exceeded");
        assert_eq!(err.reason, FailoverReason::ContextOverflow);
        assert!(err.should_compress);
        assert!(!err.should_fallback);
    }

    #[test]
    fn server_error_500() {
        let err = ClassifiedError::from_status_and_body(500, "internal server error");
        assert_eq!(err.reason, FailoverReason::ServerError);
        assert!(err.retryable);
        assert!(!err.should_fallback);
    }

    #[test]
    fn overloaded_503() {
        let err = ClassifiedError::from_status_and_body(503, "service overloaded");
        assert_eq!(err.reason, FailoverReason::Overloaded);
        assert!(err.retryable);
        assert!(err.should_fallback);
    }

    #[test]
    fn auth_401() {
        let err = ClassifiedError::from_status_and_body(401, "invalid api key");
        assert_eq!(err.reason, FailoverReason::Auth);
        assert!(!err.retryable);
        assert!(err.should_fallback);
    }

    #[test]
    fn timeout_constructor() {
        let err = ClassifiedError::timeout("connection timed out");
        assert_eq!(err.reason, FailoverReason::Timeout);
        assert!(err.retryable);
        assert!(err.status_code.is_none());
    }
}
