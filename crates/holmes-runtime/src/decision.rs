use holmes_core::tool_types::{LlmResponse, ToolCall};
use serde::{Deserialize, Serialize};

use crate::deduction::DeductionTrace;

#[derive(Debug, Clone, PartialEq)]
pub enum HolmesDecision {
    Answer {
        message: String,
    },
    AskWatson {
        question: String,
        context: Option<String>,
        options: Vec<String>,
    },
    UseTools {
        rationale: Option<String>,
        calls: Vec<ToolCall>,
    },
    SetGoal {
        condition: String,
        reason: Option<String>,
    },
    Reflect {
        diagnosis: String,
        next_strategy: String,
    },
    Deduce {
        trace: DeductionTrace,
        message: Option<String>,
    },
    Finish {
        summary: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionSource {
    NativeToolCalls,
    Directive,
    Heuristic,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedDecision {
    pub decision: HolmesDecision,
    pub source: DecisionSource,
    pub display_content: Option<String>,
}

impl ParsedDecision {
    pub fn from_response(response: &LlmResponse) -> Self {
        let raw_content = response.content.as_deref().unwrap_or_default();
        let stripped_content = strip_decision_directive(raw_content);

        if let Some(decision) = parse_directive(raw_content, &response.tool_calls) {
            return Self {
                decision,
                source: DecisionSource::Directive,
                display_content: nonempty(stripped_content),
            };
        }

        if !response.tool_calls.is_empty() {
            return Self {
                decision: HolmesDecision::UseTools {
                    rationale: nonempty(&stripped_content),
                    calls: response.tool_calls.clone(),
                },
                source: DecisionSource::NativeToolCalls,
                display_content: nonempty(&stripped_content),
            };
        }

        Self {
            decision: HolmesDecision::Answer {
                message: stripped_content.trim().to_string(),
            },
            source: DecisionSource::Heuristic,
            display_content: nonempty(stripped_content),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum DecisionDirective {
    Answer {
        message: String,
    },
    AskWatson {
        question: String,
        #[serde(default)]
        context: Option<String>,
        #[serde(default)]
        options: Vec<String>,
    },
    UseTools {
        #[serde(default)]
        rationale: Option<String>,
        #[serde(default)]
        calls: Vec<ToolCall>,
    },
    SetGoal {
        condition: String,
        #[serde(default)]
        reason: Option<String>,
    },
    Reflect {
        diagnosis: String,
        next_strategy: String,
    },
    Deduce {
        #[serde(default)]
        trace: DeductionTrace,
        #[serde(default)]
        message: Option<String>,
    },
    Finish {
        summary: String,
    },
}

impl DecisionDirective {
    fn into_decision(self, native_calls: &[ToolCall]) -> HolmesDecision {
        match self {
            Self::Answer { message } => HolmesDecision::Answer { message },
            Self::AskWatson {
                question,
                context,
                options,
            } => HolmesDecision::AskWatson {
                question,
                context,
                options,
            },
            Self::UseTools { rationale, calls } => HolmesDecision::UseTools {
                rationale,
                calls: if calls.is_empty() {
                    native_calls.to_vec()
                } else {
                    calls
                },
            },
            Self::SetGoal { condition, reason } => HolmesDecision::SetGoal { condition, reason },
            Self::Reflect {
                diagnosis,
                next_strategy,
            } => HolmesDecision::Reflect {
                diagnosis,
                next_strategy,
            },
            Self::Deduce { trace, message } => HolmesDecision::Deduce { trace, message },
            Self::Finish { summary } => HolmesDecision::Finish { summary },
        }
    }
}

fn parse_directive(content: &str, native_calls: &[ToolCall]) -> Option<HolmesDecision> {
    extract_tagged(content, "holmes_decision")
        .or_else(|| extract_prefixed(content, "HOLMES_DECISION:"))
        .or_else(|| extract_json_fence(content))
        .and_then(parse_decision_directive)
        .map(|directive| directive.into_decision(native_calls))
}

fn parse_decision_directive(content: &str) -> Option<DecisionDirective> {
    let trimmed = content.trim();
    serde_json::from_str::<DecisionDirective>(trimmed)
        .ok()
        .or_else(|| {
            extract_first_json_object(trimmed)
                .and_then(|json| serde_json::from_str::<DecisionDirective>(json).ok())
        })
}

pub fn strip_decision_directive(content: &str) -> String {
    let mut out = strip_tagged(content, "holmes_decision");
    out = strip_prefixed_block(&out, "HOLMES_DECISION:");
    out = strip_json_fence(&out);
    out.trim().to_string()
}

fn nonempty(content: impl AsRef<str>) -> Option<String> {
    let trimmed = content.as_ref().trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn extract_tagged<'a>(content: &'a str, tag: &str) -> Option<&'a str> {
    let start_tag = format!("<{tag}>");
    let end_tag = format!("</{tag}>");
    let start = content.find(&start_tag)? + start_tag.len();
    if let Some(relative_end) = content[start..].find(&end_tag) {
        return Some(&content[start..start + relative_end]);
    }
    extract_first_json_object(&content[start..])
}

fn strip_tagged(content: &str, tag: &str) -> String {
    let start_tag = format!("<{tag}>");
    let end_tag = format!("</{tag}>");
    let Some(start) = content.find(&start_tag) else {
        return content.to_string();
    };
    let after_start = start + start_tag.len();
    let end = if let Some(relative_end) = content[after_start..].find(&end_tag) {
        after_start + relative_end + end_tag.len()
    } else if let Some((_json_start, json_end)) =
        find_first_json_object_bounds(&content[after_start..])
    {
        let after_json = after_start + json_end;
        after_json + strip_malformed_close_tag(&content[after_json..])
    } else {
        return content.to_string();
    };
    format!("{}{}", &content[..start], &content[end..])
}

fn extract_prefixed<'a>(content: &'a str, prefix: &str) -> Option<&'a str> {
    let start = content.find(prefix)? + prefix.len();
    Some(content[start..].trim())
}

fn strip_prefixed_block(content: &str, prefix: &str) -> String {
    let Some(start) = content.find(prefix) else {
        return content.to_string();
    };
    content[..start].trim().to_string()
}

fn extract_json_fence(content: &str) -> Option<&str> {
    extract_fence(content, "```holmes_decision")
        .or_else(|| extract_fence(content, "```json holmes_decision"))
}

fn strip_json_fence(content: &str) -> String {
    strip_fence(content, "```holmes_decision")
        .or_else(|| strip_fence(content, "```json holmes_decision"))
        .unwrap_or_else(|| content.to_string())
        .trim()
        .to_string()
}

fn extract_fence<'a>(content: &'a str, opening: &str) -> Option<&'a str> {
    let start = content.find(opening)? + opening.len();
    let after_opening = content[start..]
        .strip_prefix('\n')
        .unwrap_or(&content[start..]);
    let adjusted_start = content.len() - after_opening.len();
    let end = content[adjusted_start..].find("```")? + adjusted_start;
    Some(&content[adjusted_start..end])
}

fn strip_fence(content: &str, opening: &str) -> Option<String> {
    let start = content.find(opening)?;
    let after_start = start + opening.len();
    let after_opening = content[after_start..]
        .strip_prefix('\n')
        .unwrap_or(&content[after_start..]);
    let adjusted_start = content.len() - after_opening.len();
    let relative_end = content[adjusted_start..].find("```")?;
    let end = adjusted_start + relative_end + 3;
    Some(format!("{}{}", &content[..start], &content[end..]))
}

fn extract_first_json_object(content: &str) -> Option<&str> {
    let (start, end) = find_first_json_object_bounds(content)?;
    Some(&content[start..end])
}

fn find_first_json_object_bounds(content: &str) -> Option<(usize, usize)> {
    let start = content.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escape = false;

    for (offset, ch) in content[start..].char_indices() {
        if in_string {
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some((start, start + offset + ch.len_utf8()));
                }
            }
            _ => {}
        }
    }

    None
}

fn strip_malformed_close_tag(tail: &str) -> usize {
    let whitespace_len = tail.len() - tail.trim_start().len();
    let trimmed = &tail[whitespace_len..];
    if !trimmed.starts_with("</") {
        return 0;
    }

    trimmed
        .find('>')
        .map(|end| whitespace_len + end + 1)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use holmes_core::tool_types::{FunctionCall, LlmResponse};

    use super::*;

    #[test]
    fn parses_ask_watson_directive_and_strips_display_content() {
        let response = LlmResponse {
            content: Some(
                r#"I need one decision.
<holmes_decision>{"type":"ask_watson","question":"May I test the login form?","context":"This is the next validation step.","options":["yes","no"]}</holmes_decision>"#
                    .into(),
            ),
            tool_calls: Vec::new(),
            finish_reason: None,
            usage: None,
        };

        let parsed = ParsedDecision::from_response(&response);

        assert_eq!(parsed.source, DecisionSource::Directive);
        assert_eq!(
            parsed.decision,
            HolmesDecision::AskWatson {
                question: "May I test the login form?".into(),
                context: Some("This is the next validation step.".into()),
                options: vec!["yes".into(), "no".into()],
            }
        );
        assert_eq!(
            parsed.display_content.as_deref(),
            Some("I need one decision.")
        );
    }

    #[test]
    fn parses_deepseek_loose_deduce_directive_with_malformed_closing_tag() {
        let response = LlmResponse {
            content: Some(
                r#"<holmes_decision>{"type":"deduce","message":"已更新 deduction ledger：受保护端点映射","trace":{"evidence":["evidence-admin-403：/admin 返回 403 且包含 request-id req-live-1"],"hypotheses":["hypothesis-admin-authz：/admin 存在但需要授权"],"predictions":[],"experiments":[],"supports":[{"source":"evidence-admin-403","target":"hypothesis-admin-authz","relation":"支持 - 403 响应明确指示该路径存在但被权限层阻断"}],"contradictions":[],"rejections":[],"confirmations":[],"conclusions":["/admin 应视为受保护端点，等待授权安全比较阶段再行处理"]}}</｜｜DSML｜｜tool_calls>

---
**Deduction ledger 已更新。**"#
                    .into(),
            ),
            tool_calls: Vec::new(),
            finish_reason: Some("stop".into()),
            usage: None,
        };

        let parsed = ParsedDecision::from_response(&response);

        assert_eq!(parsed.source, DecisionSource::Directive);
        let HolmesDecision::Deduce { trace, message } = parsed.decision else {
            panic!("deduce decision expected");
        };
        assert_eq!(
            message.as_deref(),
            Some("已更新 deduction ledger：受保护端点映射")
        );
        assert_eq!(
            trace.evidence[0].evidence_id.as_deref(),
            Some("evidence-admin-403")
        );
        assert!(trace.evidence[0].summary.contains("/admin 返回 403"));
        assert_eq!(trace.hypotheses[0].hypothesis_id, "hypothesis-admin-authz");
        assert_eq!(trace.supports[0].evidence_id, "evidence-admin-403");
        assert_eq!(trace.supports[0].hypothesis_id, "hypothesis-admin-authz");
        assert!(trace.conclusions[0].conclusion.contains("受保护端点"));
        assert_eq!(
            parsed.display_content.as_deref(),
            Some("---\n**Deduction ledger 已更新。**")
        );
    }

    #[test]
    fn native_tool_calls_become_use_tools_decision() {
        let call = ToolCall {
            id: "call-1".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "http_request".into(),
                arguments: "{}".into(),
            },
        };
        let response = LlmResponse {
            content: Some("I will inspect the endpoint.".into()),
            tool_calls: vec![call.clone()],
            finish_reason: None,
            usage: None,
        };

        let parsed = ParsedDecision::from_response(&response);

        assert_eq!(
            parsed.decision,
            HolmesDecision::UseTools {
                rationale: Some("I will inspect the endpoint.".into()),
                calls: vec![call],
            }
        );
        assert_eq!(parsed.source, DecisionSource::NativeToolCalls);
    }

    #[test]
    fn plain_response_becomes_answer() {
        let response = LlmResponse {
            content: Some("Done.".into()),
            tool_calls: Vec::new(),
            finish_reason: None,
            usage: None,
        };

        assert_eq!(
            ParsedDecision::from_response(&response).decision,
            HolmesDecision::Answer {
                message: "Done.".into()
            }
        );
    }
}
