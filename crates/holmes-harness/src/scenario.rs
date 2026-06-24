use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use holmes_core::tool_types::{LlmResponse, ToolCall};
use holmes_core::types::SessionMode;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessScenario {
    pub name: String,
    #[serde(skip)]
    pub base_dir: Option<PathBuf>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub mode: Option<SessionMode>,
    #[serde(default)]
    pub turns: Vec<HarnessTurn>,
    #[serde(default)]
    pub scripted_responses: Vec<ScriptedLlmResponse>,
    #[serde(default)]
    pub tools: Vec<HarnessTool>,
    #[serde(default)]
    pub artifacts: Vec<HarnessArtifact>,
    #[serde(default)]
    pub expectations: HarnessExpectations,
    #[serde(default)]
    pub config: Option<HarnessConfigOverride>,
}

impl HarnessScenario {
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read harness scenario {}", path.display()))?;
        let mut scenario: Self = serde_yaml::from_str(&raw)
            .with_context(|| format!("failed to parse harness scenario {}", path.display()))?;
        scenario.base_dir = path.parent().map(Path::to_path_buf);
        Ok(scenario)
    }

    pub fn mode(&self) -> SessionMode {
        self.mode.clone().unwrap_or_default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessTurn {
    pub input: String,
    #[serde(default)]
    pub expect_needs_user: bool,
    #[serde(default)]
    pub reply: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HarnessConfigOverride {
    #[serde(default)]
    pub compressor: Option<HarnessCompressorOverride>,
    #[serde(default)]
    pub learning: Option<HarnessLearningOverride>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HarnessCompressorOverride {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub context_limit: Option<u32>,
    #[serde(default)]
    pub threshold: Option<f64>,
    #[serde(default)]
    pub protected_head: Option<usize>,
    #[serde(default)]
    pub protected_tail_tokens: Option<u32>,
    #[serde(default)]
    pub protect_last_n: Option<usize>,
    #[serde(default)]
    pub target_ratio: Option<f64>,
    #[serde(default)]
    pub max_summary_tokens: Option<u32>,
    #[serde(default)]
    pub preserve_tool_groups: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HarnessLearningOverride {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub background: Option<bool>,
    #[serde(default)]
    pub review_interval_turns: Option<u32>,
    #[serde(default)]
    pub max_candidates_per_turn: Option<usize>,
    #[serde(default)]
    pub memory_write_approval: Option<bool>,
    #[serde(default)]
    pub skill_write_approval: Option<bool>,
    #[serde(default)]
    pub rule_write_approval: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptedLlmResponse {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

impl ScriptedLlmResponse {
    pub fn into_llm_response(self) -> LlmResponse {
        LlmResponse {
            content: self.content,
            tool_calls: self.tool_calls,
            finish_reason: self.finish_reason,
            usage: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessTool {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default = "default_tool_output")]
    pub output: String,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub fail: bool,
}

fn default_tool_output() -> String {
    "ok".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessArtifact {
    pub path: PathBuf,
    pub as_tool_output: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HarnessExpectations {
    #[serde(default)]
    pub final_contains: Vec<String>,
    #[serde(default)]
    pub event_types: Vec<String>,
    #[serde(default)]
    pub event_sequence: Vec<String>,
    #[serde(default)]
    pub event_payloads: Vec<HarnessEventPayloadExpectation>,
    #[serde(default)]
    pub yield_types: Vec<String>,
    #[serde(default)]
    pub tool_calls: Vec<String>,
    #[serde(default)]
    pub needs_user_count: Option<usize>,
    #[serde(default)]
    pub max_errors: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HarnessEventPayloadExpectation {
    pub event_type: String,
    #[serde(default)]
    pub contains: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_compressor_override() {
        let scenario: HarnessScenario = serde_yaml::from_str(
            r#"
name: compression
config:
  compressor:
    enabled: true
    context_limit: 120
    threshold: 0.5
    protected_head: 1
    protected_tail_tokens: 80
    protect_last_n: 2
    target_ratio: 0.4
    max_summary_tokens: 200
    preserve_tool_groups: true
"#,
        )
        .expect("scenario parses");

        let compressor = scenario
            .config
            .expect("config override")
            .compressor
            .expect("compressor override");
        assert_eq!(compressor.context_limit, Some(120));
        assert_eq!(compressor.protect_last_n, Some(2));
        assert_eq!(compressor.enabled, Some(true));
    }

    #[test]
    fn parses_learning_override() {
        let scenario: HarnessScenario = serde_yaml::from_str(
            r#"
name: learning
config:
  learning:
    enabled: true
    background: false
    review_interval_turns: 2
    max_candidates_per_turn: 3
    memory_write_approval: true
    skill_write_approval: true
    rule_write_approval: false
"#,
        )
        .expect("scenario parses");

        let learning = scenario
            .config
            .expect("config override")
            .learning
            .expect("learning override");
        assert_eq!(learning.enabled, Some(true));
        assert_eq!(learning.background, Some(false));
        assert_eq!(learning.review_interval_turns, Some(2));
        assert_eq!(learning.max_candidates_per_turn, Some(3));
        assert_eq!(learning.memory_write_approval, Some(true));
        assert_eq!(learning.rule_write_approval, Some(false));
    }

    #[test]
    fn parses_event_payload_expectations() {
        let scenario: HarnessScenario = serde_yaml::from_str(
            r#"
name: payload
expectations:
  event_payloads:
    - event_type: hypothesis_proposed
      contains:
        - hypothesis-admin-authz
        - authorization
"#,
        )
        .expect("scenario parses");

        let payload = &scenario.expectations.event_payloads[0];
        assert_eq!(payload.event_type, "hypothesis_proposed");
        assert_eq!(
            payload.contains,
            vec!["hypothesis-admin-authz", "authorization"]
        );
    }
}
