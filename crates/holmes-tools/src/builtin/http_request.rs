use anyhow::Result;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;
use tracing::{debug, warn};

use crate::registry::{Effect, Tool};
use holmes_core::{FunctionDefinition, ToolDefinition};

const BODY_LIMIT: usize = 32768;

/// Default TLS policy: certificate validation is always enforced. Invalid certs are
/// only accepted when a call explicitly passes `insecure: true` (logged as a warning).
const ACCEPT_INVALID_CERTS_BY_DEFAULT: bool = false;

pub struct HttpRequestTool {
    client: Client,
    insecure_client: OnceLock<Client>,
}

impl Default for HttpRequestTool {
    fn default() -> Self {
        Self::new()
    }
}

fn build_client(accept_invalid_certs: bool) -> Client {
    Client::builder()
        .timeout(Duration::from_secs(30))
        .danger_accept_invalid_certs(accept_invalid_certs)
        // Persist cookies across calls so multi-step auth / session / CSRF flows work
        // without the model hand-threading Set-Cookie on every request.
        .cookie_store(true)
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .expect("failed to build HTTP client")
}

impl HttpRequestTool {
    pub fn new() -> Self {
        let client = build_client(ACCEPT_INVALID_CERTS_BY_DEFAULT);
        Self {
            client,
            insecure_client: OnceLock::new(),
        }
    }

    /// Whether this concrete call has side effects. GET/HEAD/OPTIONS are reads;
    /// anything else (or unparseable args / unknown method) is mutating (fail-closed).
    fn method_is_read_only(args: &str) -> bool {
        let parsed: Result<Args, _> = serde_json::from_str(args);
        match parsed {
            Ok(args) => matches!(
                args.method.to_ascii_uppercase().as_str(),
                "GET" | "HEAD" | "OPTIONS"
            ),
            Err(_) => false,
        }
    }
}

#[derive(Deserialize)]
struct Args {
    url: String,
    #[serde(default = "default_method")]
    method: String,
    #[serde(default)]
    headers: HashMap<String, String>,
    #[serde(default)]
    body: Option<String>,
    /// Explicit opt-in to skip TLS certificate validation for this request.
    #[serde(default)]
    insecure: bool,
}

fn default_method() -> String {
    "GET".into()
}

#[async_trait::async_trait]
impl Tool for HttpRequestTool {
    fn name(&self) -> &str {
        "http_request"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            tool_type: "function".into(),
            function: FunctionDefinition {
                name: "http_request".into(),
                description: "Send an HTTP request. Returns status, final_url (post-redirect), \
                    redirected, headers, and body (max 32K). Cookies persist across calls in \
                    the same session, so multi-step auth / CSRF / session flows work without \
                    manually threading Set-Cookie."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "url": { "type": "string", "description": "Target URL" },
                        "method": { "type": "string", "description": "HTTP method (default GET)" },
                        "headers": { "type": "object", "description": "Request headers" },
                        "body": { "type": "string", "description": "Request body" },
                        "insecure": { "type": "boolean", "description": "Skip TLS certificate validation (default false; use only for deliberately self-signed test targets)" }
                    },
                    "required": ["url"]
                }),
            },
        }
    }

    fn is_read_only(&self) -> bool {
        // Static classification is conservative: the tool *can* write, so callers that
        // cannot inspect arguments (UI display, static listings) treat it as mutating.
        // Per-call classification happens in `effect_of`.
        false
    }

    fn effect_of(&self, args: &str) -> Effect {
        if Self::method_is_read_only(args) {
            Effect::ReadOnly
        } else {
            Effect::Mutating
        }
    }

    async fn execute(&self, args: &str) -> Result<String> {
        let parsed: Args = serde_json::from_str(args)?;
        debug!(url = %parsed.url, method = %parsed.method, "http request");

        let method: reqwest::Method = parsed.method.parse().unwrap_or(reqwest::Method::GET);

        let client = if parsed.insecure {
            warn!(
                url = %parsed.url,
                "http_request called with insecure=true: TLS certificate validation disabled"
            );
            self.insecure_client.get_or_init(|| build_client(true))
        } else {
            &self.client
        };

        let mut req = client.request(method, &parsed.url);
        for (k, v) in &parsed.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        if let Some(body) = &parsed.body {
            req = req.body(body.clone());
        }

        let resp = req.send().await?;
        let status = resp.status().as_u16();
        // Surface the post-redirect URL so auth/open-redirect chains are visible instead
        // of being silently followed and hidden.
        let final_url = resp.url().to_string();
        let redirected = final_url != parsed.url;
        let headers: HashMap<String, String> = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let body = resp.text().await.unwrap_or_default();
        let truncated = holmes_core::truncate_with_note(&body, BODY_LIMIT);

        Ok(json!({
            "status": status,
            "final_url": final_url,
            "redirected": redirected,
            "headers": headers,
            "body": truncated,
        })
        .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Compile-time guarantee: the default client must never be built with cert
    // validation disabled (AGT-017).
    const _: () = assert!(!ACCEPT_INVALID_CERTS_BY_DEFAULT);

    #[test]
    fn read_methods_classify_as_read_only() {
        let tool = HttpRequestTool::new();
        for method in ["GET", "HEAD", "OPTIONS", "get", "head"] {
            let args = json!({ "url": "https://example.test", "method": method }).to_string();
            assert_eq!(tool.effect_of(&args), Effect::ReadOnly, "method {method}");
        }
        // Default method is GET.
        let args = json!({ "url": "https://example.test" }).to_string();
        assert_eq!(tool.effect_of(&args), Effect::ReadOnly);
    }

    #[test]
    fn write_methods_and_bad_args_classify_as_mutating() {
        let tool = HttpRequestTool::new();
        for method in ["POST", "PUT", "PATCH", "DELETE", "post", "delete"] {
            let args = json!({ "url": "https://example.test", "method": method }).to_string();
            assert_eq!(tool.effect_of(&args), Effect::Mutating, "method {method}");
        }
        // Unparseable args fail closed.
        assert_eq!(tool.effect_of("not-json"), Effect::Mutating);
    }

    #[test]
    fn insecure_skip_is_explicit_per_call_opt_in() {
        // `insecure` defaults to false; skipping cert validation requires an explicit
        // per-call flag (the default client never accepts invalid certs — see the
        // compile-time assertion above).
        let args: Args = serde_json::from_str(r#"{"url":"https://example.test"}"#).unwrap();
        assert!(!args.insecure);
        let args: Args =
            serde_json::from_str(r#"{"url":"https://example.test","insecure":true}"#).unwrap();
        assert!(args.insecure);
    }
}
