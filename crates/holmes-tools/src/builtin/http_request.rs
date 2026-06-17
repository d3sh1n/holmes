use anyhow::Result;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;
use tracing::debug;

use crate::registry::Tool;
use holmes_core::{FunctionDefinition, ToolDefinition};

const BODY_LIMIT: usize = 32768;

pub struct HttpRequestTool {
    client: Client,
}

impl HttpRequestTool {
    pub fn new() -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .danger_accept_invalid_certs(true)
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .expect("failed to build HTTP client");
        Self { client }
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
                description: "Send HTTP request. Returns status, headers, and body (max 32K)."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "url": { "type": "string", "description": "Target URL" },
                        "method": { "type": "string", "description": "HTTP method (default GET)" },
                        "headers": { "type": "object", "description": "Request headers" },
                        "body": { "type": "string", "description": "Request body" }
                    },
                    "required": ["url"]
                }),
            },
        }
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, args: &str) -> Result<String> {
        let parsed: Args = serde_json::from_str(args)?;
        debug!(url = %parsed.url, method = %parsed.method, "http request");

        let method: reqwest::Method = parsed.method.parse().unwrap_or(reqwest::Method::GET);

        let mut req = self.client.request(method, &parsed.url);
        for (k, v) in &parsed.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        if let Some(body) = &parsed.body {
            req = req.body(body.clone());
        }

        let resp = req.send().await?;
        let status = resp.status().as_u16();
        let headers: HashMap<String, String> = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let body = resp.text().await.unwrap_or_default();
        let truncated = if body.len() > BODY_LIMIT {
            format!(
                "{}...[truncated, {} total]",
                &body[..BODY_LIMIT],
                body.len()
            )
        } else {
            body
        };

        Ok(json!({
            "status": status,
            "headers": headers,
            "body": truncated,
        })
        .to_string())
    }
}
