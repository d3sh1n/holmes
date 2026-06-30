use anyhow::{anyhow, Result};
use holmes_browser::BrowserManager;
use holmes_core::{FunctionDefinition, ToolDefinition};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::registry::Tool;

const VALID_ACTIONS: &[&str] = &[
    "navigate",
    "click",
    "fill",
    "screenshot",
    "get_content",
    "execute_js",
];

/// Thin tool shell over a long-lived `BrowserManager` owned by `ChatContext`.
///
/// The browser process is launched lazily on the first action and kept alive
/// across turns. When a page needs a manual human step (login/2FA/CAPTCHA),
/// the agent should `navigate` there and then emit `AskWatson` to hand control
/// back to the user; the browser stays open and the next turn resumes on the
/// same authenticated page.
pub struct BrowserTool {
    manager: Arc<BrowserManager>,
}

impl BrowserTool {
    pub fn new(manager: Arc<BrowserManager>) -> Self {
        Self { manager }
    }

    pub fn manager(&self) -> &Arc<BrowserManager> {
        &self.manager
    }
}

#[async_trait::async_trait]
impl Tool for BrowserTool {
    fn name(&self) -> &str {
        "browser"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            tool_type: "function".into(),
            function: FunctionDefinition {
                name: "browser".into(),
                description: "Drive a real headed browser (long-lived, stays open across turns). \
                              Use for JS-rendered pages, form interaction, or when a target needs \
                              a manual human step. Actions: navigate, click, fill, screenshot, \
                              get_content, execute_js. When a page needs a manual action you \
                              cannot or should not automate, navigate there then use AskWatson to \
                              tell the user what to do in the browser window and what to reply \
                              when done."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": VALID_ACTIONS,
                            "description": "Browser action to perform"
                        },
                        "url": { "type": "string", "description": "URL for navigate" },
                        "selector": { "type": "string", "description": "CSS selector for click/fill/get_content" },
                        "value": { "type": "string", "description": "Value for fill" },
                        "code": { "type": "string", "description": "JavaScript code for execute_js" },
                        "full_page": { "type": "boolean", "description": "Full-page screenshot (default: false)" }
                    },
                    "required": ["action"]
                }),
            },
        }
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn execute(&self, args: &str) -> Result<String> {
        let v: Value = serde_json::from_str(args).map_err(|e| anyhow!("invalid browser args: {e}"))?;
        let action = v
            .get("action")
            .and_then(|a| a.as_str())
            .ok_or_else(|| anyhow!("missing action"))?
            .to_string();
        match action.as_str() {
            "navigate" => {
                let url = v
                    .get("url")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| anyhow!("missing url"))?;
                let snap = self
                    .manager
                    .navigate(url)
                    .await
                    .map_err(|e| anyhow!(e.to_string()))?;
                Ok(format!(
                    "url: {}\ntitle: {}\n{}",
                    snap.url, snap.title, snap.text_excerpt
                ))
            }
            "click" => {
                let sel = v
                    .get("selector")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| anyhow!("missing selector"))?;
                let o = self
                    .manager
                    .click(sel)
                    .await
                    .map_err(|e| anyhow!(e.to_string()))?;
                Ok(o.summary)
            }
            "fill" => {
                let sel = v
                    .get("selector")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| anyhow!("missing selector"))?;
                let val = v
                    .get("value")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| anyhow!("missing value"))?;
                let o = self
                    .manager
                    .fill(sel, val)
                    .await
                    .map_err(|e| anyhow!(e.to_string()))?;
                Ok(o.summary)
            }
            "screenshot" => {
                let full = v.get("full_page").and_then(|x| x.as_bool()).unwrap_or(false);
                let shot = self
                    .manager
                    .screenshot(full)
                    .await
                    .map_err(|e| anyhow!(e.to_string()))?;
                Ok(format!("screenshot: {}", shot.path.display()))
            }
            "get_content" => {
                let sel = v.get("selector").and_then(|x| x.as_str());
                self.manager
                    .get_content(sel)
                    .await
                    .map_err(|e| anyhow!(e.to_string()))
            }
            "execute_js" => {
                let code = v
                    .get("code")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| anyhow!("missing code"))?;
                let val = self
                    .manager
                    .execute_js(code)
                    .await
                    .map_err(|e| anyhow!(e.to_string()))?;
                Ok(serde_json::to_string_pretty(&val).unwrap_or_else(|_| val.to_string()))
            }
            other => Err(anyhow!("unknown browser action: {other}")),
        }
    }
}
