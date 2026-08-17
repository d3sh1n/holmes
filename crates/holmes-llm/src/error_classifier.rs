use serde::{Deserialize, Serialize};
use std::time::Duration;

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
    Connection,
    StreamInterrupted,
    Unknown,
}

/// How a classified error affects provider health and failover decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// Request-level transient failure (timeout, connect error, 429, 5xx, stream
    /// interruption): counts toward provider health and may fail over.
    Transient,
    /// Provider-level configuration problem (auth, billing, unknown model): the
    /// provider is disabled until the config/credentials are fixed; fail over
    /// immediately, never auto-recover.
    ProviderConfig,
    /// The request content itself is rejected (context overflow, invalid params):
    /// failing over is pointless and provider health is unaffected — propagate.
    RequestContent,
}

#[derive(Debug, Clone)]
pub struct ClassifiedError {
    pub reason: FailoverReason,
    pub status_code: Option<u16>,
    pub message: String,
    pub retryable: bool,
    pub should_compress: bool,
    pub should_fallback: bool,
    /// Server-provided cooldown hint (from a `Retry-After` header), when present.
    pub retry_after: Option<Duration>,
}

impl ClassifiedError {
    pub fn failure_class(&self) -> FailureClass {
        match self.reason {
            FailoverReason::RateLimit
            | FailoverReason::Overloaded
            | FailoverReason::ServerError
            | FailoverReason::Timeout
            | FailoverReason::Connection
            | FailoverReason::StreamInterrupted
            | FailoverReason::Unknown => FailureClass::Transient,
            FailoverReason::Auth | FailoverReason::Billing | FailoverReason::ModelNotFound => {
                FailureClass::ProviderConfig
            }
            FailoverReason::ContextOverflow | FailoverReason::FormatError => {
                FailureClass::RequestContent
            }
        }
    }

    pub fn with_retry_after(mut self, retry_after: Option<Duration>) -> Self {
        self.retry_after = retry_after;
        self
    }
    fn new(
        reason: FailoverReason,
        status_code: Option<u16>,
        body: &str,
        retryable: bool,
        should_compress: bool,
        should_fallback: bool,
    ) -> Self {
        Self {
            reason,
            status_code,
            message: body.chars().take(200).collect(),
            retryable,
            should_compress,
            should_fallback,
            retry_after: None,
        }
    }

    pub fn from_status_and_body(status: u16, body: &str) -> Self {
        let lower = body.to_lowercase();

        match status {
            429 => {
                let is_billing = BILLING_PATTERNS.iter().any(|p| lower.contains(p));
                if is_billing {
                    Self::new(FailoverReason::Billing, Some(429), body, false, false, true)
                } else {
                    Self::new(
                        FailoverReason::RateLimit,
                        Some(429),
                        body,
                        true,
                        false,
                        true,
                    )
                }
            }
            400 => {
                let is_context = CONTEXT_OVERFLOW_PATTERNS.iter().any(|p| lower.contains(p));
                if is_context {
                    Self::new(
                        FailoverReason::ContextOverflow,
                        Some(400),
                        body,
                        false,
                        true,
                        false,
                    )
                } else {
                    Self::new(
                        FailoverReason::FormatError,
                        Some(400),
                        body,
                        false,
                        false,
                        false,
                    )
                }
            }
            401 | 403 => Self::new(FailoverReason::Auth, Some(status), body, false, false, true),
            402 => Self::new(FailoverReason::Billing, Some(402), body, false, false, true),
            404 => Self::new(
                FailoverReason::ModelNotFound,
                Some(404),
                body,
                false,
                false,
                true,
            ),
            500 | 502 => Self::new(
                FailoverReason::ServerError,
                Some(status),
                body,
                true,
                false,
                false,
            ),
            503 | 529 => Self::new(
                FailoverReason::Overloaded,
                Some(status),
                body,
                true,
                false,
                true,
            ),
            504 | 524 => Self::new(
                FailoverReason::Timeout,
                Some(status),
                body,
                true,
                false,
                false,
            ),
            _ => Self::new(
                FailoverReason::Unknown,
                Some(status),
                body,
                true,
                false,
                false,
            ),
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
            retry_after: None,
        }
    }

    /// Transport-level connection failure (DNS, refused, TLS handshake, …).
    pub fn connection(message: impl Into<String>) -> Self {
        Self {
            reason: FailoverReason::Connection,
            status_code: None,
            message: message.into(),
            // Retried on another provider immediately; same-provider reconnect
            // retries rarely help and only add latency.
            retryable: false,
            should_compress: false,
            should_fallback: true,
            retry_after: None,
        }
    }

    /// The HTTP stream broke or its events failed to parse after the response
    /// was established.
    pub fn stream_interrupted(message: impl Into<String>) -> Self {
        Self {
            reason: FailoverReason::StreamInterrupted,
            status_code: None,
            message: message.into(),
            retryable: true,
            should_compress: false,
            should_fallback: true,
            retry_after: None,
        }
    }

    /// A 200 response whose body could not be parsed into a completion — gateway
    /// misbehavior rather than a request problem, so it counts as transient.
    pub fn invalid_response(message: impl Into<String>) -> Self {
        Self {
            reason: FailoverReason::Unknown,
            status_code: None,
            message: message.into(),
            retryable: true,
            should_compress: false,
            should_fallback: true,
            retry_after: None,
        }
    }

    /// Classify a terminal SSE `error` event frame (see `anthropic::sse_error_event`)
    /// using the same mapping as HTTP status codes.
    pub fn from_sse_error(error_type: &str, message: impl Into<String>) -> Self {
        let message = message.into();
        let (reason, retryable, fallback) = match error_type {
            "overloaded_error" => (FailoverReason::Overloaded, true, true),
            "rate_limit_error" => (FailoverReason::RateLimit, true, true),
            "authentication_error" | "permission_error" => (FailoverReason::Auth, false, true),
            "billing_error" => (FailoverReason::Billing, false, true),
            "not_found_error" => (FailoverReason::ModelNotFound, false, true),
            "invalid_request_error" => (FailoverReason::FormatError, false, false),
            _ => (FailoverReason::Unknown, true, false),
        };
        Self {
            reason,
            status_code: None,
            message,
            retryable,
            should_compress: false,
            should_fallback: fallback,
            retry_after: None,
        }
    }
}

/// Parse a `Retry-After` header value (delta-seconds form; HTTP-date falls back
/// to `None` since LLM gateways practically always send delta-seconds).
pub fn parse_retry_after(value: &str) -> Option<Duration> {
    let seconds: u64 = value.trim().parse().ok()?;
    Some(Duration::from_secs(seconds))
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

    #[test]
    fn failure_class_mapping() {
        assert_eq!(
            ClassifiedError::from_status_and_body(429, "rate limit").failure_class(),
            FailureClass::Transient
        );
        assert_eq!(
            ClassifiedError::from_status_and_body(500, "boom").failure_class(),
            FailureClass::Transient
        );
        assert_eq!(
            ClassifiedError::from_status_and_body(401, "bad key").failure_class(),
            FailureClass::ProviderConfig
        );
        assert_eq!(
            ClassifiedError::from_status_and_body(404, "model not found").failure_class(),
            FailureClass::ProviderConfig
        );
        assert_eq!(
            ClassifiedError::from_status_and_body(400, "invalid temperature").failure_class(),
            FailureClass::RequestContent
        );
        assert_eq!(
            ClassifiedError::from_status_and_body(400, "maximum context length exceeded")
                .failure_class(),
            FailureClass::RequestContent
        );
        assert_eq!(
            ClassifiedError::connection("refused").failure_class(),
            FailureClass::Transient
        );
        assert_eq!(
            ClassifiedError::stream_interrupted("eof").failure_class(),
            FailureClass::Transient
        );
    }

    #[test]
    fn retry_after_header_parsing() {
        assert_eq!(parse_retry_after("3"), Some(Duration::from_secs(3)));
        assert_eq!(parse_retry_after(" 12 "), Some(Duration::from_secs(12)));
        assert_eq!(parse_retry_after("not-a-number"), None);
    }

    #[test]
    fn retry_after_attached_via_builder() {
        let err = ClassifiedError::from_status_and_body(429, "slow down")
            .with_retry_after(Some(Duration::from_secs(7)));
        assert_eq!(err.retry_after, Some(Duration::from_secs(7)));
    }
}
