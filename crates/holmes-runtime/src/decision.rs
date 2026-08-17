use holmes_core::ledger::{
    EvidenceLinkId, EvidenceRelation, EvidenceStrength, HypothesisId, Priority, ResolvedStatus,
    RiskLevel, ValidatorKind,
};
use holmes_core::tool_types::{FunctionDefinition, LlmResponse, ToolCall, ToolDefinition};
use serde::{Deserialize, Serialize};
use serde_json::json;

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
    Finish {
        summary: String,
        conclusion_refs: Vec<String>,
        remaining_hypothesis_ids: Vec<HypothesisId>,
    },
    /// One or more non-terminal meta actions were committed without an
    /// executable or terminal primary action.
    Continue,
    /// The response mixed a terminal control call (`finish` / `ask_watson`) with
    /// executable tool calls in one message (P0-02). This is a protocol error:
    /// nothing in the response is executed — neither the terminal decision nor the
    /// tools — and the runtime feeds the violation back so the model re-emits the
    /// terminal call alone (after the tools have run in their own step). Previously
    /// the tools were silently dropped, which let a `finish` smuggle past pending
    /// work.
    ProtocolViolation {
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionSource {
    NativeToolCalls,
    Directive,
    Heuristic,
}

/// A non-terminal meta-action the model can attach to the SAME step as executable
/// tool calls. Unlike a terminal decision, a meta-action records state and lets the
/// turn keep going — so the model can, in one message, e.g. `set_goal` AND
/// `run_command` (true interleaving of thinking and acting) instead of spending a
/// whole extra LLM round-trip just to record the goal.
#[derive(Debug, Clone, PartialEq)]
pub enum MetaAction {
    SetGoal {
        condition: String,
        reason: Option<String>,
    },
    ProposeHypothesis(HypothesisProposal),
    PlanExperiment(ExperimentProposal),
    LinkEvidence(EvidenceLinkProposal),
    RequestResolution(ResolutionRequestProposal),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PredictionProposal {
    pub client_ref: String,
    pub observable: String,
    pub expected_when_true: String,
    pub falsifier: String,
    pub validator: ValidatorKind,
    #[serde(default)]
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HypothesisProposal {
    pub client_ref: String,
    pub claim: String,
    #[serde(default)]
    pub premise_refs: Vec<String>,
    #[serde(default)]
    pub alternative_group: Option<String>,
    pub priority: Priority,
    #[serde(default)]
    pub predictions: Vec<PredictionProposal>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentProposal {
    pub client_ref: String,
    pub hypothesis_refs: Vec<String>,
    #[serde(default)]
    pub prediction_refs: Vec<String>,
    pub action: String,
    #[serde(default)]
    pub expected_observations: Vec<String>,
    #[serde(default)]
    pub tool_allowlist: Vec<String>,
    pub risk: RiskLevel,
    #[serde(default)]
    pub bind_calls: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceLinkProposal {
    pub evidence_id: String,
    pub hypothesis_id: HypothesisId,
    #[serde(default)]
    pub prediction_id: Option<holmes_core::ledger::PredictionId>,
    pub relation: EvidenceRelation,
    pub strength: EvidenceStrength,
    pub rationale: String,
    pub validator: ValidatorKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResolutionRequestProposal {
    pub hypothesis_id: HypothesisId,
    pub expected_revision: u64,
    pub requested_status: ResolvedStatus,
    #[serde(default)]
    pub evidence_link_ids: Vec<EvidenceLinkId>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedDecision {
    /// The step's primary action (execute tools / answer / ask / finish).
    pub decision: HolmesDecision,
    /// Meta-actions to apply BEFORE the primary decision in the same step. Empty in
    /// the common case; non-empty only when the model attaches `set_goal` alongside
    /// other work in one response.
    pub meta_actions: Vec<MetaAction>,
    pub source: DecisionSource,
    pub display_content: Option<String>,
}

impl ParsedDecision {
    pub fn from_response(response: &LlmResponse) -> Self {
        let raw_content = response.content.as_deref().unwrap_or_default();
        let stripped_content = strip_decision_directive(raw_content);

        // 1. Native tool_use is the primary, robust channel. Partition the model's
        //    tool calls into: meta-actions (set_goal — non-terminal), a terminal
        //    control decision (ask_watson/finish), and executable tools.
        //    Because the arguments arrive as API-validated structured JSON, there is
        //    no scraping of hand-written JSON out of prose.
        if !response.tool_calls.is_empty() {
            let mut meta_actions: Vec<MetaAction> = Vec::new();
            let mut terminal: Option<HolmesDecision> = None;
            let mut exec_calls: Vec<ToolCall> = Vec::new();
            let mut invalid_control: Option<String> = None;

            for call in &response.tool_calls {
                match classify_control_call(call) {
                    Some(ClassifiedControl::Meta(meta)) => meta_actions.push(meta),
                    Some(ClassifiedControl::Terminal(decision)) => {
                        // First terminal wins; a turn can only end one way.
                        if terminal.is_none() {
                            terminal = Some(decision);
                        }
                    }
                    Some(ClassifiedControl::Invalid(message)) => {
                        invalid_control.get_or_insert(message);
                    }
                    None => exec_calls.push(call.clone()),
                }
            }

            if let Some(message) = invalid_control {
                return Self {
                    decision: HolmesDecision::ProtocolViolation { message },
                    meta_actions: Vec::new(),
                    source: DecisionSource::NativeToolCalls,
                    display_content: nonempty(&stripped_content),
                };
            }

            // Protocol violation (P0-02): a terminal control call (finish /
            // ask_watson) in the SAME response as executable tool calls. The whole
            // response is rejected — neither the terminal decision nor the tools are
            // honored, and even meta-actions are dropped — so a finish can never
            // smuggle past pending tool work by hiding it in a discarded batch.
            if let Some(terminal) = &terminal {
                if !exec_calls.is_empty() || !meta_actions.is_empty() {
                    let terminal_name = match terminal {
                        HolmesDecision::Finish { .. } => "finish",
                        HolmesDecision::AskWatson { .. } => "ask_watson",
                        _ => "terminal control",
                    };
                    return Self {
                        decision: HolmesDecision::ProtocolViolation {
                            message: format!(
                                "response combined '{terminal_name}' with {} executable tool call(s) \
                                 and {} meta action(s); a terminal control call must be emitted alone",
                                exec_calls.len(), meta_actions.len()
                            ),
                        },
                        meta_actions: Vec::new(),
                        source: DecisionSource::NativeToolCalls,
                        display_content: nonempty(&stripped_content),
                    };
                }
            }

            // Primary decision precedence: terminate > act on tools > drive on a
            // meta-action alone. Meta-actions attached to a terminal/exec decision
            // are applied first, then the primary decision runs.
            let decision = if let Some(terminal) = terminal {
                terminal
            } else if !exec_calls.is_empty() {
                HolmesDecision::UseTools {
                    rationale: nonempty(&stripped_content),
                    calls: exec_calls,
                }
            } else if meta_actions.len() == 1
                && matches!(meta_actions.first(), Some(MetaAction::SetGoal { .. }))
            {
                match meta_actions.remove(0) {
                    MetaAction::SetGoal { condition, reason } => {
                        HolmesDecision::SetGoal { condition, reason }
                    }
                    _ => unreachable!("lone SetGoal was checked above"),
                }
            } else if !meta_actions.is_empty() {
                HolmesDecision::Continue
            } else {
                // Native tool calls existed but none were control tools and none were
                // executable (unreachable in practice); fall back to answering.
                HolmesDecision::Answer {
                    message: stripped_content.trim().to_string(),
                }
            };

            return Self {
                decision,
                meta_actions,
                source: DecisionSource::NativeToolCalls,
                display_content: nonempty(&stripped_content),
            };
        }

        // 2. Deprecated fallback: some models/gateways still emit the decision as a
        //    `<holmes_decision>{...}</holmes_decision>` blob in free text. Kept for
        //    resilience; native tool_use above is preferred and takes precedence.
        if let Some(decision) = parse_directive(raw_content, &response.tool_calls) {
            return Self {
                decision,
                meta_actions: Vec::new(),
                source: DecisionSource::Directive,
                display_content: nonempty(stripped_content),
            };
        }

        Self {
            decision: HolmesDecision::Answer {
                message: stripped_content.trim().to_string(),
            },
            meta_actions: Vec::new(),
            source: DecisionSource::Heuristic,
            display_content: nonempty(stripped_content),
        }
    }
}

/// Reserved tool names that map to loop-control decisions rather than executable
/// tools. They are advertised to the model (see [`control_tool_definitions`]) and
/// intercepted by name in [`ParsedDecision::from_response`]; they are never sent to
/// the `ToolRegistry` for execution.
pub const CONTROL_TOOL_NAMES: &[&str] = &[
    "set_goal",
    "ask_watson",
    "finish",
    "propose_hypothesis",
    "plan_experiment",
    "link_evidence",
    "request_resolution",
];

/// Whether `name` is a reserved control-tool name (see [`CONTROL_TOOL_NAMES`]).
pub fn is_control_tool(name: &str) -> bool {
    CONTROL_TOOL_NAMES.contains(&name)
}

/// A control-tool call classified by whether it ends the turn (terminal) or merely
/// records state and lets the turn continue (meta).
enum ClassifiedControl {
    Meta(MetaAction),
    Terminal(HolmesDecision),
    Invalid(String),
}

/// Classify a native tool call: `None` if it is a normal executable tool,
/// `Some(Meta|Terminal)` if it is one of the reserved control tools.
fn classify_control_call(call: &ToolCall) -> Option<ClassifiedControl> {
    let name = call.function.name.as_str();
    if !is_control_tool(name) {
        return None;
    }
    let invalid = || {
        Some(ClassifiedControl::Invalid(format!(
            "reserved control tool '{name}' received arguments that do not match its strict schema"
        )))
    };
    match name {
        "propose_hypothesis" => serde_json::from_str(&call.function.arguments)
            .ok()
            .map(|value| ClassifiedControl::Meta(MetaAction::ProposeHypothesis(value)))
            .or_else(invalid),
        "plan_experiment" => serde_json::from_str(&call.function.arguments)
            .ok()
            .map(|value| ClassifiedControl::Meta(MetaAction::PlanExperiment(value)))
            .or_else(invalid),
        "link_evidence" => serde_json::from_str(&call.function.arguments)
            .ok()
            .map(|value| ClassifiedControl::Meta(MetaAction::LinkEvidence(value)))
            .or_else(invalid),
        "request_resolution" => serde_json::from_str(&call.function.arguments)
            .ok()
            .map(|value| ClassifiedControl::Meta(MetaAction::RequestResolution(value)))
            .or_else(invalid),
        _ => match control_decision_from_call(call) {
            Some(HolmesDecision::SetGoal { condition, reason }) => {
                Some(ClassifiedControl::Meta(MetaAction::SetGoal {
                    condition,
                    reason,
                }))
            }
            Some(terminal @ (HolmesDecision::AskWatson { .. } | HolmesDecision::Finish { .. })) => {
                Some(ClassifiedControl::Terminal(terminal))
            }
            _ => invalid(),
        },
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FinishArgs {
    summary: String,
    conclusion_refs: Vec<String>,
    remaining_hypothesis_ids: Vec<HypothesisId>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SetGoalArgs {
    condition: String,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AskWatsonArgs {
    question: String,
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    options: Vec<String>,
}

fn control_decision_from_call(call: &ToolCall) -> Option<HolmesDecision> {
    match call.function.name.as_str() {
        "set_goal" => serde_json::from_str::<SetGoalArgs>(&call.function.arguments)
            .ok()
            .map(|args| HolmesDecision::SetGoal {
                condition: args.condition,
                reason: args.reason,
            }),
        "ask_watson" => serde_json::from_str::<AskWatsonArgs>(&call.function.arguments)
            .ok()
            .map(|args| HolmesDecision::AskWatson {
                question: args.question,
                context: args.context,
                options: args.options,
            }),
        "finish" => serde_json::from_str::<FinishArgs>(&call.function.arguments)
            .ok()
            .map(|args| HolmesDecision::Finish {
                summary: args.summary,
                conclusion_refs: args.conclusion_refs,
                remaining_hypothesis_ids: args.remaining_hypothesis_ids,
            }),
        _ => None,
    }
}

/*
 * The old DecisionDirective conversion remains below for the deprecated text
 * fallback. Native calls are parsed by the strict per-control structs above.
 */

/// JSON-schema definitions for the control tools, appended to the executable-tool
/// list sent to the model so meta-actions travel over native tool_use instead of
/// hand-written JSON in prose.
pub fn control_tool_definitions() -> Vec<ToolDefinition> {
    fn def(name: &str, description: &str, parameters: serde_json::Value) -> ToolDefinition {
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: name.to_string(),
                description: description.to_string(),
                parameters,
            },
        }
    }

    vec![
        def(
            "set_goal",
            "Record a standing goal/success condition for the engagement. Emit alone, without executable tool calls.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "condition": { "type": "string", "description": "The success condition to hold as the standing goal." },
                    "reason": { "type": "string", "description": "Optional rationale for the goal." }
                },
                "required": ["condition"]
            }),
        ),
        def(
            "propose_hypothesis",
            "Propose one falsifiable case-scoped hypothesis and optional predictions. This is a non-terminal Ledger meta action and may accompany executable calls.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "client_ref": {"type":"string"},
                    "claim": {"type":"string", "maxLength":1000},
                    "premise_refs": {"type":"array", "items":{"type":"string"}},
                    "alternative_group": {"type":"string"},
                    "priority": {"type":"string", "enum":["low","medium","high","critical"]},
                    "predictions": {"type":"array", "items":{
                        "type":"object", "additionalProperties":false,
                        "properties":{
                            "client_ref":{"type":"string"},
                            "observable":{"type":"string"},
                            "expected_when_true":{"type":"string"},
                            "falsifier":{"type":"string"},
                            "validator":{"type":"string", "enum":["command_exit","file_postcondition","network_differential","code_test","security_reproduction","human_attestation","semantic"]},
                            "required":{"type":"boolean"}
                        },
                        "required":["client_ref","observable","expected_when_true","falsifier","validator","required"]
                    }}
                },
                "required":["client_ref","claim","priority","predictions"]
            }),
        ),
        def(
            "plan_experiment",
            "Plan a bounded experiment and optionally bind it to executable calls in this response by zero-based executable-call index.",
            json!({
                "type":"object", "additionalProperties":false,
                "properties":{
                    "client_ref":{"type":"string"},
                    "hypothesis_refs":{"type":"array","items":{"type":"string"}},
                    "prediction_refs":{"type":"array","items":{"type":"string"}},
                    "action":{"type":"string"},
                    "expected_observations":{"type":"array","items":{"type":"string"}},
                    "tool_allowlist":{"type":"array","items":{"type":"string"}},
                    "risk":{"type":"string","enum":["low","medium","high","critical"]},
                    "bind_calls":{"type":"array","items":{"type":"integer","minimum":0}}
                },
                "required":["client_ref","hypothesis_refs","action","tool_allowlist","risk","bind_calls"]
            }),
        ),
        def(
            "link_evidence",
            "Request a validated relationship between existing case evidence and a hypothesis/prediction. Runtime may downgrade or reject relation strength.",
            json!({
                "type":"object", "additionalProperties":false,
                "properties":{
                    "evidence_id":{"type":"string"},
                    "hypothesis_id":{"type":"string"},
                    "prediction_id":{"type":"string"},
                    "relation":{"type":"string","enum":["supports","contradicts","inconclusive"]},
                    "strength":{"type":"string","enum":["weak","moderate","strong","decisive"]},
                    "rationale":{"type":"string","maxLength":800},
                    "validator":{"type":"string","enum":["command_exit","file_postcondition","network_differential","code_test","security_reproduction","human_attestation","semantic"]}
                },
                "required":["evidence_id","hypothesis_id","relation","strength","rationale","validator"]
            }),
        ),
        def(
            "request_resolution",
            "Request Runtime validation of an existing hypothesis. This records the request; only validators can create Confirmed/Rejected/Inconclusive state.",
            json!({
                "type":"object", "additionalProperties":false,
                "properties":{
                    "hypothesis_id":{"type":"string"},
                    "expected_revision":{"type":"integer","minimum":1},
                    "requested_status":{"type":"string","enum":["confirmed","rejected","inconclusive"]},
                    "evidence_link_ids":{"type":"array","items":{"type":"string"}},
                    "reason":{"type":"string"}
                },
                "required":["hypothesis_id","expected_revision","requested_status","evidence_link_ids","reason"]
            }),
        ),
        def(
            "ask_watson",
            "Pause and hand off to the human operator (Watson) for a decision or a manual step (login / 2FA / CAPTCHA). Emit alone, with no executable tool calls; the turn ends until the operator replies.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "question": { "type": "string", "description": "The question or action requested from the operator." },
                    "context": { "type": "string", "description": "Optional supporting context." },
                    "options": { "type": "array", "items": { "type": "string" }, "description": "Optional discrete choices for the operator." }
                },
                "required": ["question"]
            }),
        ),
        def(
            "finish",
            "Declare the standing goal satisfied. Emit alone and cite durable Resolution IDs for strong conclusions; disclose important unresolved hypotheses.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "summary": { "type": "string", "description": "Final summary of what was achieved." },
                    "conclusion_refs": {"type":"array","items":{"type":"string"},"description":"Previously persisted Resolution IDs supporting final strong conclusions."},
                    "remaining_hypothesis_ids": {"type":"array","items":{"type":"string"},"description":"Important Open/Inconclusive hypotheses disclosed as remaining uncertainty."}
                },
                "required": ["summary", "conclusion_refs", "remaining_hypothesis_ids"]
            }),
        ),
    ]
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
    Finish {
        summary: String,
        #[serde(default)]
        conclusion_refs: Vec<String>,
        #[serde(default)]
        remaining_hypothesis_ids: Vec<HypothesisId>,
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
            Self::Finish {
                summary,
                conclusion_refs,
                remaining_hypothesis_ids,
            } => HolmesDecision::Finish {
                summary,
                conclusion_refs,
                remaining_hypothesis_ids,
            },
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
 ..Default::default() };

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
            ..Default::default()
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
            ..Default::default()
        };

        assert_eq!(
            ParsedDecision::from_response(&response).decision,
            HolmesDecision::Answer {
                message: "Done.".into()
            }
        );
    }

    fn control_call(name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: format!("call-{name}"),
            call_type: "function".into(),
            function: FunctionCall {
                name: name.into(),
                arguments: arguments.into(),
            },
        }
    }

    fn response_with_calls(content: Option<&str>, calls: Vec<ToolCall>) -> LlmResponse {
        LlmResponse {
            content: content.map(Into::into),
            tool_calls: calls,
            finish_reason: None,
            usage: None,
            ..Default::default()
        }
    }

    #[test]
    fn native_ask_watson_tool_use_maps_to_ask_watson() {
        let response = response_with_calls(
            Some("I need one decision."),
            vec![control_call(
                "ask_watson",
                r#"{"question":"May I test the login form?","context":"Next validation step.","options":["yes","no"]}"#,
            )],
        );

        let parsed = ParsedDecision::from_response(&response);

        assert_eq!(parsed.source, DecisionSource::NativeToolCalls);
        assert_eq!(
            parsed.decision,
            HolmesDecision::AskWatson {
                question: "May I test the login form?".into(),
                context: Some("Next validation step.".into()),
                options: vec!["yes".into(), "no".into()],
            }
        );
        // Prose is preserved as display content, unaffected by the structured call.
        assert_eq!(
            parsed.display_content.as_deref(),
            Some("I need one decision.")
        );
    }

    #[test]
    fn native_set_goal_tool_use_maps_to_set_goal() {
        let response = response_with_calls(
            None,
            vec![control_call(
                "set_goal",
                r#"{"condition":"exfiltrate the flag","reason":"engagement objective"}"#,
            )],
        );

        assert_eq!(
            ParsedDecision::from_response(&response).decision,
            HolmesDecision::SetGoal {
                condition: "exfiltrate the flag".into(),
                reason: Some("engagement objective".into()),
            }
        );
    }

    #[test]
    fn native_finish_tool_use_maps_to_finish() {
        let response = response_with_calls(
            None,
            vec![control_call(
                "finish",
                r#"{"summary":"goal achieved","conclusion_refs":[],"remaining_hypothesis_ids":[]}"#,
            )],
        );

        assert_eq!(
            ParsedDecision::from_response(&response).decision,
            HolmesDecision::Finish {
                summary: "goal achieved".into(),
                conclusion_refs: Vec::new(),
                remaining_hypothesis_ids: Vec::new(),
            }
        );
    }

    #[test]
    fn native_control_tool_takes_precedence_over_text_directive() {
        // A stray legacy blob in prose must not override a real native control call.
        let response = response_with_calls(
            Some(r#"<holmes_decision>{"type":"finish","summary":"stale"}</holmes_decision>"#),
            vec![control_call("set_goal", r#"{"condition":"live goal"}"#)],
        );

        assert_eq!(
            ParsedDecision::from_response(&response).decision,
            HolmesDecision::SetGoal {
                condition: "live goal".into(),
                reason: None,
            }
        );
    }

    #[test]
    fn executable_tool_named_normally_still_becomes_use_tools() {
        let response =
            response_with_calls(Some("inspecting"), vec![control_call("http_request", "{}")]);

        assert_eq!(
            ParsedDecision::from_response(&response).source,
            DecisionSource::NativeToolCalls
        );
        assert!(matches!(
            ParsedDecision::from_response(&response).decision,
            HolmesDecision::UseTools { .. }
        ));
    }

    #[test]
    fn meta_tool_interleaves_with_executable_tool_in_one_step() {
        // set_goal + a real tool in the same message: the tool drives the step and
        // the goal rides along as a meta-action (true interleaving — no wasted turn).
        let response = response_with_calls(
            Some("recording the goal and probing"),
            vec![
                control_call("set_goal", r#"{"condition":"pop a shell"}"#),
                control_call("http_request", r#"{"url":"http://t"}"#),
            ],
        );

        let parsed = ParsedDecision::from_response(&response);

        match &parsed.decision {
            HolmesDecision::UseTools { calls, .. } => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].function.name, "http_request");
            }
            other => panic!("expected UseTools, got {other:?}"),
        }
        assert_eq!(
            parsed.meta_actions,
            vec![MetaAction::SetGoal {
                condition: "pop a shell".into(),
                reason: None,
            }]
        );
    }

    #[test]
    fn ledger_client_refs_and_call_bindings_parse_as_native_meta_actions() {
        let response = response_with_calls(
            None,
            vec![
                control_call(
                    "propose_hypothesis",
                    r#"{"client_ref":"h-local","claim":"service is exposed","priority":"high","predictions":[{"client_ref":"p-local","observable":"port 443 accepts TLS","expected_when_true":"handshake succeeds","falsifier":"connection refused","validator":"network_differential","required":true}]}"#,
                ),
                control_call(
                    "plan_experiment",
                    r#"{"client_ref":"x-local","hypothesis_refs":["h-local"],"prediction_refs":["p-local"],"action":"probe TLS","tool_allowlist":["http_request"],"risk":"low","bind_calls":[0]}"#,
                ),
                control_call("http_request", r#"{"url":"https://target.test"}"#),
            ],
        );
        let parsed = ParsedDecision::from_response(&response);
        assert!(matches!(parsed.decision, HolmesDecision::UseTools { .. }));
        assert!(matches!(
            &parsed.meta_actions[0],
            MetaAction::ProposeHypothesis(proposal) if proposal.client_ref == "h-local"
        ));
        assert!(matches!(
            &parsed.meta_actions[1],
            MetaAction::PlanExperiment(proposal) if proposal.bind_calls == vec![0]
        ));
    }

    #[test]
    fn malformed_reserved_ledger_control_fails_closed() {
        let response = response_with_calls(
            None,
            vec![control_call(
                "plan_experiment",
                r#"{"client_ref":"x","bind_calls":[99],"unexpected":true}"#,
            )],
        );
        assert!(matches!(
            ParsedDecision::from_response(&response).decision,
            HolmesDecision::ProtocolViolation { .. }
        ));
    }

    #[test]
    fn terminal_tool_mixed_with_meta_action_is_rejected_atomically() {
        // A terminal action must be alone. Rejecting the whole response prevents
        // a partially committed goal update followed by an ambiguous finish.
        let response = response_with_calls(
            None,
            vec![
                control_call("set_goal", r#"{"condition":"exfiltrate the flag"}"#),
                control_call(
                    "finish",
                    r#"{"summary":"goal met","conclusion_refs":[],"remaining_hypothesis_ids":[]}"#,
                ),
            ],
        );

        let parsed = ParsedDecision::from_response(&response);

        assert!(matches!(
            parsed.decision,
            HolmesDecision::ProtocolViolation { .. }
        ));
        assert!(parsed.meta_actions.is_empty());
    }

    #[test]
    fn lone_meta_tool_has_no_extra_meta_actions() {
        // A single set_goal (no executable tools) still drives as a SetGoal decision
        // with an empty meta-action list — unchanged from the pre-interleaving path.
        let response =
            response_with_calls(None, vec![control_call("set_goal", r#"{"condition":"x"}"#)]);
        let parsed = ParsedDecision::from_response(&response);
        assert!(matches!(parsed.decision, HolmesDecision::SetGoal { .. }));
        assert!(parsed.meta_actions.is_empty());
    }

    #[test]
    fn control_tool_definitions_cover_all_reserved_names() {
        let defs = control_tool_definitions();
        let names: Vec<_> = defs.iter().map(|d| d.function.name.as_str()).collect();
        for reserved in CONTROL_TOOL_NAMES {
            assert!(names.contains(reserved), "missing control tool {reserved}");
        }
    }

    #[test]
    fn finish_mixed_with_executable_tool_is_a_protocol_violation() {
        // finish + a real tool in one response: previously the tool was silently
        // dropped and the finish won; now the whole response is rejected.
        let response = response_with_calls(
            Some("wrapping up"),
            vec![
                control_call("http_request", r#"{"url":"http://example.test"}"#),
                control_call(
                    "finish",
                    r#"{"summary":"done","conclusion_refs":[],"remaining_hypothesis_ids":[]}"#,
                ),
            ],
        );

        let parsed = ParsedDecision::from_response(&response);
        match &parsed.decision {
            HolmesDecision::ProtocolViolation { message } => {
                assert!(message.contains("finish"));
                assert!(message.contains("1 executable tool call"));
            }
            other => panic!("expected ProtocolViolation, got {other:?}"),
        }
        // Even attached meta-actions are dropped with the rejected response.
        assert!(parsed.meta_actions.is_empty());
    }

    #[test]
    fn ask_watson_mixed_with_executable_tool_is_a_protocol_violation() {
        let response = response_with_calls(
            None,
            vec![
                control_call("ask_watson", r#"{"question":"proceed?"}"#),
                control_call("http_request", r#"{"url":"http://example.test"}"#),
            ],
        );

        assert!(matches!(
            ParsedDecision::from_response(&response).decision,
            HolmesDecision::ProtocolViolation { .. }
        ));
    }

    #[test]
    fn terminal_with_meta_is_a_protocol_violation() {
        // Terminal controls must be emitted alone so the response has one atomic
        // meaning and no state update is silently committed before termination.
        let response = response_with_calls(
            None,
            vec![
                control_call("set_goal", r#"{"condition":"pop a shell"}"#),
                control_call(
                    "finish",
                    r#"{"summary":"done","conclusion_refs":[],"remaining_hypothesis_ids":[]}"#,
                ),
            ],
        );
        assert!(matches!(
            ParsedDecision::from_response(&response).decision,
            HolmesDecision::ProtocolViolation { .. }
        ));
    }
}
