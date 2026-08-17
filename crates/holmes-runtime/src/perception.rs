use holmes_core::state::{AttackState, Finding, FindingConfidence};
use holmes_core::tool_types::Message;
use holmes_core::types::SessionMode;

use crate::context::{InteractionMode, RuntimeContext, RuntimePhase};

const DEFAULT_RECENT_OBSERVATIONS: usize = 5;
/// Cap on how many endpoints/findings the situation snapshot lists inline before
/// collapsing to a count — keeps the persistent snapshot bounded.
const SITUATION_LIST_CAP: usize = 12;

#[derive(Debug, Clone, Default)]
pub struct PerceptionEngine;

impl PerceptionEngine {
    pub fn build(context: &RuntimeContext) -> PerceptionFrame {
        PerceptionFrame::from_context(context)
    }

    pub fn perceive(&self, context: &RuntimeContext) -> PerceptionFrame {
        Self::build(context)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PerceptionFrame {
    pub situation_summary: String,
    pub session_mode: SessionMode,
    pub interaction_mode: InteractionMode,
    pub phase: RuntimePhase,
    /// Engagement phase inferred from live state (recon → attack → exploitation/report),
    /// so the frame reflects progress instead of a static `Initializing`.
    pub engagement_phase: String,
    pub active_goal: Option<String>,
    pub recent_observations: Vec<String>,
    pub recalled_memories: Vec<String>,
    pub failure_count: usize,
    pub ledger_summary: Option<String>,
}

impl PerceptionFrame {
    pub fn from_context(context: &RuntimeContext) -> Self {
        let observations = &context.state.observations;
        let start = observations
            .len()
            .saturating_sub(DEFAULT_RECENT_OBSERVATIONS);

        Self {
            // Current situation is projected from the LIVE tool-truth/validated state
            // (populated by PostGuards), not the MindPalace dashboard — that dashboard
            // is fed by events the runtime never emits, so it only ever rendered a stub.
            situation_summary: situation_from_state(&context.state.compatibility_state),
            session_mode: context.state.session_mode.clone(),
            interaction_mode: context.state.interaction_mode.clone(),
            phase: context.state.phase.clone(),
            engagement_phase: infer_engagement_phase(&context.state.compatibility_state),
            active_goal: context.state.active_goal.clone(),
            recent_observations: observations[start..].to_vec(),
            recalled_memories: context
                .state
                .recalled_memories
                .iter()
                // No score shown: recall is lexical (FTS5/LIKE) and the stored
                // relevance value is not a per-query match score.
                .map(|memory| format!("[{}] {}", memory.id, memory.content))
                .collect(),
            failure_count: context.state.failures.len(),
            ledger_summary: context
                .state
                .ledger
                .as_ref()
                .map(|snapshot| bounded_ledger_summary(snapshot, &context.config.ledger)),
        }
    }

    pub fn transient_situation_message(&self) -> Option<Message> {
        let content = self.transient_situation_content();
        if content.is_empty() {
            None
        } else {
            Some(Message::user(content))
        }
    }

    pub fn build_transient_messages(&self, session_messages: &[Message]) -> Vec<Message> {
        let mut messages = session_messages.to_vec();
        if let Some(message) = self.transient_situation_message() {
            messages.push(message);
        }
        messages
    }

    fn transient_situation_content(&self) -> String {
        let mut sections = Vec::new();
        sections.push(format!(
            "[Runtime]\nSession mode: {:?}\nInteraction mode: {:?}\nPhase: {}\nFailure count: {}",
            self.session_mode, self.interaction_mode, self.engagement_phase, self.failure_count
        ));

        if !self.situation_summary.trim().is_empty() {
            sections.push(format!(
                "[Current situation]\n{}",
                self.situation_summary.trim()
            ));
        }

        if let Some(goal) = self
            .active_goal
            .as_deref()
            .map(str::trim)
            .filter(|goal| !goal.is_empty())
        {
            sections.push(format!("[Active goal]\n{goal}"));
        }

        if !self.recent_observations.is_empty() {
            sections.push(format!(
                "[Recent observations]\n{}",
                self.recent_observations
                    .iter()
                    .map(|observation| format!("- {observation}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }

        if !self.recalled_memories.is_empty() {
            sections.push(format!(
                "[Recalled memory]\n{}",
                self.recalled_memories
                    .iter()
                    .map(|memory| format!("- {memory}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }

        if let Some(ledger) = self
            .ledger_summary
            .as_deref()
            .filter(|summary| !summary.trim().is_empty())
        {
            sections.push(format!("[Hypothesis Ledger v2]\n{ledger}"));
        }

        sections.push(
            r#"[Holmes decision protocol]
Prefer native tool calls (never emit decisions as JSON inside your text). Keep private reasoning private; expose only concise, auditable rationale. Alongside executable tools you have control tools:
- `set_goal` {condition, reason?} — record a standing goal / success condition. You MAY emit it in the SAME step as executable tool calls (e.g. `set_goal` + `execute_command` together).
- `propose_hypothesis` — propose a falsifiable hypothesis and predictions; Runtime assigns IDs and validates references.
- `plan_experiment` — plan an experiment and bind it to executable call indexes; bound tools must be in its allowlist.
- `link_evidence` — request a Supports/Contradicts/Inconclusive relation for already recorded evidence; validators may downgrade or reject it.
- `request_resolution` — request Confirmed/Rejected/Inconclusive; the Runtime validator, not your confidence, decides the transition.
- `ask_watson` {question, context?, options?} — pause for the human operator's judgment or a manual step (login / 2FA / CAPTCHA). Ends the turn; emit alone.
- `finish` {summary, conclusion_refs, remaining_hypothesis_ids} — end the engagement. Cite existing Resolution IDs and disclose important unresolved hypotheses. Emit alone.
Every terminal outcome is verified against the recorded evidence before the turn ends — this applies to `finish` AND to plain-text answers on action tasks alike. Declaring completion without target-relevant evidence is rejected and handed back to you.
Never combine `finish` or `ask_watson` with executable tool calls in one response: that is a protocol error — nothing in such a response is executed, and you will be asked to re-emit the parts separately.
To simply answer a conversational/informational request, reply in plain text."#
                .to_string(),
        );

        sections.join("\n\n")
    }
}

fn bounded_ledger_summary(
    snapshot: &holmes_core::ledger::LedgerSnapshot,
    config: &holmes_core::config::LedgerConfig,
) -> String {
    let mut lines = vec![format!(
        "case={} version={} hypotheses={} experiments={} evidence={} resolutions={}",
        snapshot.case_id,
        snapshot.version,
        snapshot.hypotheses.len(),
        snapshot.experiments.len(),
        snapshot.evidence.len(),
        snapshot.resolutions.len()
    )];
    for hypothesis in snapshot
        .hypotheses
        .values()
        .take(config.max_active_hypotheses_in_context)
    {
        lines.push(format!(
            "- {} [{:?}/{:?}/r{}] {}",
            hypothesis.id,
            hypothesis.status,
            hypothesis.priority,
            hypothesis.revision,
            hypothesis.claim.chars().take(180).collect::<String>()
        ));
    }
    let predictions: Vec<_> = snapshot
        .predictions
        .values()
        .take(config.max_predictions_in_context)
        .collect();
    if !predictions.is_empty() {
        lines.push("Predictions:".into());
        for prediction in predictions {
            lines.push(format!(
                "- {} hypothesis={} validator={:?} required={} observable={}",
                prediction.id,
                prediction.hypothesis_id,
                prediction.validator,
                prediction.required,
                prediction.observable.chars().take(160).collect::<String>()
            ));
        }
    }
    let unlinked: Vec<_> = snapshot
        .evidence
        .values()
        .filter(|evidence| {
            !snapshot
                .evidence_links
                .values()
                .any(|link| link.evidence_id == evidence.id)
        })
        .take(config.max_recent_evidence_in_context)
        .collect();
    if !unlinked.is_empty() {
        lines.push("Unlinked evidence:".into());
        for evidence in unlinked {
            lines.push(format!(
                "- {} tool={} outcome={:?} experiment={}",
                evidence.id,
                evidence.tool,
                evidence.outcome_status,
                evidence
                    .binding
                    .experiment_id
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "none".into())
            ));
        }
    }
    if !snapshot.contradictions.is_empty() {
        lines.push(format!(
            "Blocking contradictions: {}",
            snapshot
                .contradictions
                .values()
                .take(config.max_contradictions_in_context)
                .map(|contradiction| contradiction.hypothesis_id.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    lines.join("\n")
}

/// Render a current, deduped snapshot of the live attack state (tool-truth +
/// validated zones, populated by PostGuards) for the `[Current situation]` block.
/// Returns an empty string when nothing has been discovered yet, so the section is
/// simply omitted rather than showing a stub.
/// Infer the engagement phase from live state so the frame reflects real progress
/// (the static `AttackPhase`/`RuntimePhase` never advanced past their defaults).
fn infer_engagement_phase(state: &AttackState) -> String {
    let findings = state.findings();
    let has_confirmed = findings
        .values()
        .any(|f| f.confidence == FindingConfidence::Confirmed);
    let has_active = findings
        .values()
        .any(|f| f.confidence != FindingConfidence::Rejected);
    let surface = state.attack_surface();
    let recon_done =
        !surface.ports.is_empty() || !surface.links.is_empty() || !surface.tech_stack.is_empty();

    if has_confirmed {
        "exploitation/reporting (confirmed findings — verify coverage before finishing)".into()
    } else if has_active {
        "attack (candidate findings under investigation)".into()
    } else if recon_done {
        "attack (surface mapped — probe for vulnerabilities)".into()
    } else {
        "recon (map the attack surface first)".into()
    }
}

pub(crate) fn situation_from_state(state: &AttackState) -> String {
    let surface = state.attack_surface();
    let bundle = state.evidence_bundle();
    let findings = state.findings();
    let mut lines: Vec<String> = Vec::new();

    if !state.plan.is_empty() {
        let items: Vec<String> = state
            .plan
            .iter()
            .map(|t| {
                let mark = match t.status.as_str() {
                    "completed" => "[x]",
                    "in_progress" => "[~]",
                    _ => "[ ]",
                };
                format!("  {mark} {}", t.content)
            })
            .collect();
        lines.push(format!("Plan:\n{}", items.join("\n")));
    }

    if !surface.ports.is_empty() {
        let services: Vec<String> = surface
            .ports
            .iter()
            .map(|p| {
                let version = p.version.trim();
                if version.is_empty() {
                    format!("{}/{}", p.port, p.service.trim())
                } else {
                    format!("{}/{} {}", p.port, p.service.trim(), version)
                }
            })
            .collect();
        lines.push(format!("Services: {}", services.join(", ")));
    }

    if !surface.tech_stack.is_empty() {
        lines.push(format!("Tech: {}", surface.tech_stack.join(", ")));
    }

    if !surface.links.is_empty() || !surface.forms.is_empty() {
        let mut parts = Vec::new();
        if !surface.links.is_empty() {
            parts.push(capped_list("Endpoints", &surface.links, SITUATION_LIST_CAP));
        }
        if !surface.forms.is_empty() {
            parts.push(format!("forms: {}", surface.forms.len()));
        }
        lines.push(parts.join(" | "));
    }

    // Usernames + count only — never echo passwords into the frame (the redaction
    // middleware does not cover this transient, non-persisted message).
    if !bundle.credentials.is_empty() {
        let users: Vec<&str> = bundle
            .credentials
            .iter()
            .take(SITUATION_LIST_CAP)
            .map(|c| c.username.as_str())
            .collect();
        lines.push(format!(
            "Credentials: {} ({})",
            bundle.credentials.len(),
            users.join(", ")
        ));
    }

    let mut evidence_counts = Vec::new();
    if !bundle.object_refs.is_empty() {
        evidence_counts.push(format!("object_refs: {}", bundle.object_refs.len()));
    }
    if !bundle.vulns.is_empty() {
        evidence_counts.push(format!("vuln_evidence: {}", bundle.vulns.len()));
    }
    if !evidence_counts.is_empty() {
        lines.push(evidence_counts.join(" | "));
    }

    // Ruled-out findings are coverage (tested-and-safe), not active findings — render
    // them separately so "ruled X out" is distinct from "never tried X".
    let ruled_out: Vec<&Finding> = findings
        .values()
        .filter(|f| f.confidence == FindingConfidence::Rejected)
        .collect();

    let active: Vec<&Finding> = findings
        .values()
        .filter(|f| f.confidence != FindingConfidence::Rejected)
        .collect();

    if !active.is_empty() {
        let mut items: Vec<&Finding> = active.clone();
        // Confirmed first, then candidates; stable by type for determinism.
        items.sort_by(|a, b| {
            confidence_rank(&a.confidence)
                .cmp(&confidence_rank(&b.confidence))
                .then_with(|| a.finding_type.cmp(&b.finding_type))
        });
        let shown = items.len().min(SITUATION_LIST_CAP);
        let mut rendered = Vec::new();
        for finding in items.iter().take(shown) {
            let confidence = format!("{:?}", finding.confidence).to_ascii_lowercase();
            let severity = format!("{:?}", finding.severity).to_ascii_lowercase();
            let label = if finding.finding_type.trim().is_empty() {
                finding.attack_type.trim()
            } else {
                finding.finding_type.trim()
            };
            let loc = finding.location.trim();
            let loc_part = if loc.is_empty() {
                String::new()
            } else {
                format!(" @ {loc}")
            };
            let evidence = finding.evidence.trim();
            let suffix = if evidence.is_empty() {
                String::new()
            } else {
                format!(" — {}", first_line(evidence, 120))
            };
            rendered.push(format!(
                "  - [{confidence}/{severity}] {label}{loc_part}{suffix}"
            ));
        }
        if items.len() > shown {
            rendered.push(format!("  - (+{} more)", items.len() - shown));
        }
        lines.push(format!(
            "Findings ({}):\n{}",
            active.len(),
            rendered.join("\n")
        ));
    }

    if !ruled_out.is_empty() {
        let mut labels: Vec<String> = ruled_out
            .iter()
            .map(|f| {
                let label = if f.finding_type.trim().is_empty() {
                    f.attack_type.trim()
                } else {
                    f.finding_type.trim()
                };
                let loc = f.location.trim();
                if loc.is_empty() {
                    label.to_string()
                } else {
                    format!("{label}@{loc}")
                }
            })
            .collect();
        labels.sort();
        lines.push(format!(
            "Ruled out (tested, negative — do not re-test): {}",
            labels.join(", ")
        ));
    }

    // Soft-404 baseline: tell the model the site's not-found template shape so it can
    // discount a 200 of that shape instead of treating it as a real hit (the "200 ≠
    // vulnerability" pitfall). This is the consumption the baseline previously lacked.
    if let Some((status, len)) = state.soft404_baseline {
        lines.push(format!(
            "Soft-404 baseline: responses of shape (HTTP {status}, ~{len} bytes) are this \
             site's not-found/placeholder template — treat a matching response as NEGATIVE, \
             not a discovery."
        ));
    }

    lines.join("\n")
}

fn confidence_rank(confidence: &FindingConfidence) -> u8 {
    match confidence {
        FindingConfidence::Confirmed => 0,
        FindingConfidence::Candidate => 1,
        FindingConfidence::Rejected => 2,
    }
}

/// List up to `cap` items inline, else show the count plus a capped sample.
fn capped_list(label: &str, items: &[String], cap: usize) -> String {
    if items.len() <= cap {
        format!("{label}: {}", items.join(", "))
    } else {
        let sample = items[..cap].join(", ");
        format!("{label}: {} ({sample}, …)", items.len())
    }
}

/// First line of `text`, truncated to `max` chars on a char boundary.
fn first_line(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    match line.char_indices().nth(max) {
        Some((byte_idx, _)) => format!("{}…", &line[..byte_idx]),
        None => line.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use holmes_core::config::HolmesConfig;
    use holmes_core::session::RuntimeSession;
    use holmes_core::tool_types::{LlmResponse, Role};
    use holmes_core::types::SessionMode;
    use holmes_guards::GuardChain;
    use holmes_mind_palace::MindPalace;
    use holmes_session::{memory_store::MemoryStore, SessionDB};
    use holmes_tools::ToolRegistry;

    use crate::context::{RuntimeContext, RuntimeState};
    use crate::deliberation::StaticLlmBackend;
    use holmes_core::state::tool_truth::PortInfo;

    use super::*;

    #[test]
    fn situation_snapshot_renders_live_state_not_a_stub() {
        let mut state = AttackState::new(
            "http://t".into(),
            "1.2.3.4".into(),
            "c".into(),
            "C".into(),
            Vec::new(),
        );
        state.attack_surface_mut().ports.push(PortInfo {
            port: 443,
            service: "https".into(),
            version: "nginx".into(),
        });
        state.attack_surface_mut().tech_stack.push("Django".into());
        state.record_finding(Finding {
            id: "f-candidate".into(),
            finding_type: "xss".into(),
            confidence: FindingConfidence::Candidate,
            evidence: "reflected param q".into(),
            details: String::new(),
            attack_type: "xss".into(),
            ..Default::default()
        });
        state.record_finding(Finding {
            id: "f-confirmed".into(),
            finding_type: "idor".into(),
            confidence: FindingConfidence::Confirmed,
            evidence: "user2 record returned for user1 token".into(),
            details: String::new(),
            attack_type: "idor".into(),
            ..Default::default()
        });

        let snapshot = situation_from_state(&state);

        assert!(
            snapshot.contains("443/https nginx"),
            "real services: {snapshot}"
        );
        assert!(snapshot.contains("Django"), "tech: {snapshot}");
        assert!(
            snapshot.contains("[confirmed/info] idor"),
            "finding content: {snapshot}"
        );
        assert!(
            snapshot.contains("[candidate/info] xss"),
            "finding content: {snapshot}"
        );
        // Confirmed must sort before candidate.
        let confirmed_at = snapshot.find("[confirmed/").unwrap();
        let candidate_at = snapshot.find("[candidate/").unwrap();
        assert!(
            confirmed_at < candidate_at,
            "confirmed before candidate: {snapshot}"
        );
    }

    #[test]
    fn situation_snapshot_renders_plan() {
        use holmes_core::state::TodoItem;
        let mut state = AttackState::new(
            "http://t".into(),
            String::new(),
            "c".into(),
            "C".into(),
            Vec::new(),
        );
        state.plan = vec![
            TodoItem {
                content: "recon".into(),
                status: "completed".into(),
            },
            TodoItem {
                content: "exploit idor".into(),
                status: "in_progress".into(),
            },
        ];
        let snapshot = situation_from_state(&state);
        assert!(snapshot.contains("Plan:"));
        assert!(snapshot.contains("[x] recon"));
        assert!(snapshot.contains("[~] exploit idor"));
    }

    #[test]
    fn situation_snapshot_is_empty_when_nothing_discovered() {
        let state = AttackState::new(
            "http://t".into(),
            String::new(),
            "c".into(),
            "C".into(),
            Vec::new(),
        );
        assert!(situation_from_state(&state).is_empty());
    }

    #[tokio::test]
    async fn perception_frame_builds_transient_messages_without_mutating_session() {
        let session_db = Arc::new(SessionDB::open(":memory:").await.expect("session db"));
        let memory_store = Arc::new(MemoryStore::open(":memory:").await.expect("memory store"));
        let mind_palace = MindPalace::new(session_db.clone(), memory_store.clone());
        let llm = Arc::new(StaticLlmBackend::new(LlmResponse {
            content: Some("ok".into()),
            tool_calls: Vec::new(),
            finish_reason: None,
            usage: None,
            ..Default::default()
        }));
        let mut state = RuntimeState::new(SessionMode::Pentest);
        state.observations = vec![
            "one".into(),
            "two".into(),
            "three".into(),
            "four".into(),
            "five".into(),
            "six".into(),
        ];
        state.failures = vec!["timeout".into()];

        let context = RuntimeContext::new(
            RuntimeSession::new("session-1".into(), SessionMode::Pentest)
                .with_user_message("investigate example.test"),
            session_db,
            memory_store,
            mind_palace,
            llm,
            Arc::new(ToolRegistry::new()),
            GuardChain::new(),
            state,
            HolmesConfig::default(),
        );
        let original_len = context.session.messages.len();

        let frame = PerceptionEngine::build(&context);
        let transient_messages = frame.build_transient_messages(&context.session.messages);

        assert_eq!(context.session.messages.len(), original_len);
        assert_eq!(transient_messages.len(), original_len + 1);
        assert_eq!(
            transient_messages.last().expect("transient").role,
            Role::User
        );
        assert!(transient_messages
            .last()
            .and_then(|message| message.content.as_ref())
            .expect("content")
            .contains("Failure count: 1"));
        assert_eq!(
            frame.recent_observations,
            vec!["two", "three", "four", "five", "six"]
        );
        assert!(frame.active_goal.is_none());
        assert!(frame.recalled_memories.is_empty());
        assert_eq!(frame, PerceptionFrame::from_context(&context));
    }

    #[tokio::test]
    async fn perception_frame_includes_goal_memories_and_decision_protocol() {
        let session_db = Arc::new(SessionDB::open(":memory:").await.expect("session db"));
        let memory_store = Arc::new(MemoryStore::open(":memory:").await.expect("memory store"));
        let mind_palace = MindPalace::new(session_db.clone(), memory_store.clone());
        let llm = Arc::new(StaticLlmBackend::new(LlmResponse {
            content: Some("ok".into()),
            tool_calls: Vec::new(),
            finish_reason: None,
            usage: None,
            ..Default::default()
        }));
        let mut state = RuntimeState::new(SessionMode::Pentest);
        state.active_goal = Some("validate login behavior".into());
        state.recalled_memories.push(crate::context::RuntimeMemory {
            id: "mem-1".into(),
            content: "Similar login tests required response diffing.".into(),
            relevance_score: 0.87,
        });

        let context = RuntimeContext::new(
            RuntimeSession::new("session-1".into(), SessionMode::Pentest),
            session_db,
            memory_store,
            mind_palace,
            llm,
            Arc::new(ToolRegistry::new()),
            GuardChain::new(),
            state,
            HolmesConfig::default(),
        );

        let frame = PerceptionFrame::from_context(&context);
        let content = frame.transient_situation_content();

        assert!(content.contains("[Active goal]\nvalidate login behavior"));
        assert!(content.contains("Similar login tests required response diffing."));
        assert!(content.contains("[Holmes decision protocol]"));
    }
}
