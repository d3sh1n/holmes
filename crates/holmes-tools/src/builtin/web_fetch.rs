//! `web_fetch` — fetch a URL and return cleaned, readable text (scripts/styles/tags
//! stripped, entities decoded, whitespace collapsed). Mirrors a modern agent's WebFetch:
//! `http_request` returns the raw body, this returns something the model can actually read.

use anyhow::{anyhow, Result};
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;
use tracing::debug;

use crate::registry::Tool;
use holmes_core::{FunctionDefinition, ToolDefinition};

const DEFAULT_MAX_CHARS: usize = 30_000;
const HARD_MAX_CHARS: usize = 200_000;

pub struct WebFetchTool {
    client: reqwest::Client,
}

impl WebFetchTool {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .user_agent("Mozilla/5.0 (compatible; Holmes/1.0)")
                .build()
                .unwrap_or_default(),
        }
    }
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
struct Args {
    url: String,
    #[serde(default)]
    max_chars: Option<usize>,
}

#[async_trait::async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            tool_type: "function".into(),
            function: FunctionDefinition {
                name: "web_fetch".into(),
                description: "Fetch a web page and return its cleaned, readable text content \
                    (HTML tags, scripts, and styles removed). Use this to read articles, docs, \
                    or advisories. For raw HTTP (headers, status, JSON APIs, non-GET) use \
                    `http_request`; for JS-rendered pages use `browser`."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "url": { "type": "string", "description": "The URL to fetch (http/https)." },
                        "max_chars": { "type": "integer", "description": "Cap on returned characters (default 30000)." }
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
        let parsed: Args =
            serde_json::from_str(args).map_err(|e| anyhow!("invalid arguments: {e}"))?;
        if !parsed.url.starts_with("http://") && !parsed.url.starts_with("https://") {
            return Err(anyhow!("url must start with http:// or https://"));
        }
        let max = parsed
            .max_chars
            .unwrap_or(DEFAULT_MAX_CHARS)
            .clamp(1, HARD_MAX_CHARS);
        debug!(url = %parsed.url, "web_fetch");

        let resp = self
            .client
            .get(&parsed.url)
            .send()
            .await
            .map_err(|e| anyhow!("request failed: {e}"))?;
        let status = resp.status();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = resp
            .text()
            .await
            .map_err(|e| anyhow!("reading body failed: {e}"))?;

        let text = if content_type.contains("html") || body.trim_start().starts_with('<') {
            html_to_text(&body)
        } else {
            body
        };
        let (text, truncated) = truncate_chars(text.trim(), max);

        let mut out = format!("[{}] {}\n\n{}", status.as_u16(), parsed.url, text);
        if truncated {
            out.push_str("\n\n… [truncated]");
        }
        Ok(out)
    }
}

/// Strip HTML to readable text: drop script/style/head, turn block tags into newlines,
/// remove remaining tags, decode common entities, and collapse whitespace.
fn html_to_text(html: &str) -> String {
    let mut s = html.to_string();
    // Remove script/style/head/noscript blocks (case-insensitive, across newlines).
    for tag in ["script", "style", "head", "noscript", "svg"] {
        let re = regex::Regex::new(&format!(r"(?is)<{tag}\b.*?</{tag}>")).unwrap();
        s = re.replace_all(&s, " ").into_owned();
    }
    // Block-level tags → newlines so structure survives.
    let block = regex::Regex::new(
        r"(?i)</?(p|div|br|li|tr|h[1-6]|section|article|header|footer|ul|ol|table)\b[^>]*>",
    )
    .unwrap();
    s = block.replace_all(&s, "\n").into_owned();
    // Strip all remaining tags.
    let tags = regex::Regex::new(r"(?s)<[^>]+>").unwrap();
    s = tags.replace_all(&s, "").into_owned();
    // Decode a few common entities.
    for (from, to) in [
        ("&amp;", "&"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
        ("&#39;", "'"),
        ("&apos;", "'"),
        ("&nbsp;", " "),
    ] {
        s = s.replace(from, to);
    }
    // Collapse runs of blank lines / horizontal whitespace.
    let ws = regex::Regex::new(r"[ \t]+").unwrap();
    s = ws.replace_all(&s, " ").into_owned();
    let blanks = regex::Regex::new(r"\n[ \t]*\n[ \t]*(\n[ \t]*)*").unwrap();
    s = blanks.replace_all(&s, "\n\n").into_owned();
    s.lines()
        .map(str::trim)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn truncate_chars(s: &str, max: usize) -> (String, bool) {
    match s.char_indices().nth(max) {
        Some((byte_idx, _)) => (s[..byte_idx].to_string(), true),
        None => (s.to_string(), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_to_text_strips_tags_scripts_and_decodes_entities() {
        let html = r#"<html><head><title>T</title><style>.x{color:red}</style></head>
        <body><script>alert('x')</script><h1>Hello &amp; Welcome</h1>
        <p>First&nbsp;paragraph with <a href="/x">a link</a>.</p>
        <div>Second block</div></body></html>"#;
        let text = html_to_text(html);
        assert!(text.contains("Hello & Welcome"));
        assert!(text.contains("First paragraph with a link."));
        assert!(text.contains("Second block"));
        assert!(!text.contains("alert"), "scripts stripped");
        assert!(!text.contains("color:red"), "styles stripped");
        assert!(!text.contains('<'), "tags stripped");
    }

    #[tokio::test]
    async fn rejects_non_http_url() {
        let err = WebFetchTool::new()
            .execute(r#"{"url":"file:///etc/passwd"}"#)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("http"));
    }

    #[test]
    fn read_only_and_named() {
        let t = WebFetchTool::new();
        assert_eq!(t.name(), "web_fetch");
        assert!(t.is_read_only());
    }
}
