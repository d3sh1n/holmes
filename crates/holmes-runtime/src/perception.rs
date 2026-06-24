use holmes_core::tool_types::Message;
use holmes_core::types::SessionMode;

use crate::context::{InteractionMode, RuntimeContext, RuntimePhase};
use crate::deduction::DeductionLedgerProjection;

const DEFAULT_RECENT_OBSERVATIONS: usize = 5;

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
    pub active_goal: Option<String>,
    pub recent_observations: Vec<String>,
    pub recalled_memories: Vec<String>,
    pub deduction_evidence: Vec<String>,
    pub deduction_facts: Vec<String>,
    pub deduction_hypotheses: Vec<String>,
    pub deduction_experiments: Vec<String>,
    pub deduction_conclusions: Vec<String>,
    pub failure_count: usize,
}

impl PerceptionFrame {
    pub fn from_context(context: &RuntimeContext) -> Self {
        let observations = &context.state.observations;
        let start = observations
            .len()
            .saturating_sub(DEFAULT_RECENT_OBSERVATIONS);
        let deduction = DeductionLedgerProjection::from_state(&context.state.deduction);

        Self {
            situation_summary: context.mind_palace.situation_summary(&context.state.session_mode),
            session_mode: context.state.session_mode.clone(),
            interaction_mode: context.state.interaction_mode.clone(),
            phase: context.state.phase.clone(),
            active_goal: context.state.active_goal.clone(),
            recent_observations: observations[start..].to_vec(),
            recalled_memories: context
                .state
                .recalled_memories
                .iter()
                .map(|memory| {
                    format!(
                        "[{} score {:.2}] {}",
                        memory.id, memory.relevance_score, memory.content
                    )
                })
                .collect(),
            deduction_evidence: deduction.evidence,
            deduction_facts: deduction.facts,
            deduction_hypotheses: deduction.hypotheses,
            deduction_experiments: deduction.experiments,
            deduction_conclusions: deduction.conclusions,
            failure_count: context.state.failures.len(),
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
            "[Runtime]\nSession mode: {:?}\nInteraction mode: {:?}\nPhase: {:?}\nFailure count: {}",
            self.session_mode, self.interaction_mode, self.phase, self.failure_count
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

        if !self.deduction_evidence.is_empty()
            || !self.deduction_facts.is_empty()
            || !self.deduction_hypotheses.is_empty()
            || !self.deduction_experiments.is_empty()
            || !self.deduction_conclusions.is_empty()
        {
            let mut lines = Vec::new();
            lines.extend(
                self.deduction_evidence
                    .iter()
                    .map(|evidence| format!("Evidence: {evidence}")),
            );
            lines.extend(
                self.deduction_facts
                    .iter()
                    .map(|fact| format!("Fact: {fact}")),
            );
            lines.extend(
                self.deduction_hypotheses
                    .iter()
                    .map(|hypothesis| format!("Hypothesis: {hypothesis}")),
            );
            lines.extend(
                self.deduction_experiments
                    .iter()
                    .map(|experiment| format!("Experiment: {experiment}")),
            );
            lines.extend(
                self.deduction_conclusions
                    .iter()
                    .map(|conclusion| format!("Conclusion: {conclusion}")),
            );
            sections.push(format!(
                "[Deduction ledger]\n{}",
                lines
                    .iter()
                    .map(|line| format!("- {line}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }

        sections.push(
            r#"[Holmes decision protocol]
Prefer native tool calls whenever you need tools. If you need Watson's judgment before continuing, include a hidden directive and no tool calls:
<holmes_decision>{"type":"ask_watson","question":"...","context":"...","options":["..."]}</holmes_decision>
If you need to record a standing goal:
<holmes_decision>{"type":"set_goal","condition":"...","reason":"..."}</holmes_decision>
If you need to update the deduction ledger before choosing the next action:
<holmes_decision>{"type":"deduce","message":"...","trace":{"evidence":[],"hypotheses":[],"predictions":[],"experiments":[],"supports":[],"contradictions":[],"rejections":[],"confirmations":[],"conclusions":[]}}</holmes_decision>
If you are stuck, emit:
<holmes_decision>{"type":"reflect","diagnosis":"...","next_strategy":"..."}</holmes_decision>"#
                .to_string(),
        );

        sections.join("\n\n")
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

    use crate::context::{
        DeductionConclusionState, DeductionEvidenceState, DeductionExperimentState,
        DeductionFactState, DeductionHypothesisState, DeductionHypothesisStatus, RuntimeContext,
        RuntimeState,
    };
    use crate::deliberation::StaticLlmBackend;

    use super::*;

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
        assert!(frame.deduction_evidence.is_empty());
        assert!(frame.deduction_facts.is_empty());
        assert!(frame.deduction_hypotheses.is_empty());
        assert!(frame.deduction_experiments.is_empty());
        assert!(frame.deduction_conclusions.is_empty());
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

    #[tokio::test]
    async fn perception_frame_includes_deduction_ledger() {
        let session_db = Arc::new(SessionDB::open(":memory:").await.expect("session db"));
        let memory_store = Arc::new(MemoryStore::open(":memory:").await.expect("memory store"));
        let mind_palace = MindPalace::new(session_db.clone(), memory_store.clone());
        let llm = Arc::new(StaticLlmBackend::new(LlmResponse {
            content: Some("ok".into()),
            tool_calls: Vec::new(),
            finish_reason: None,
            usage: None,
        }));
        let mut state = RuntimeState::new(SessionMode::Pentest);
        state.deduction.evidence.push(DeductionEvidenceState {
            id: "evidence-1".into(),
            summary: "/login differed by username".into(),
            source: "login_probe".into(),
            confidence: "tool_observed".into(),
        });
        state.deduction.facts.push(DeductionFactState {
            id: "fact-1".into(),
            statement: "Controlled login probes differed.".into(),
            evidence_ids: vec!["evidence-1".into()],
        });
        state.deduction.hypotheses.push(DeductionHypothesisState {
            id: "hypothesis-user-enumeration".into(),
            statement: "Login may leak user existence.".into(),
            confidence: 0.78,
            status: DeductionHypothesisStatus::Supported,
            attack_type: Some("user_enumeration".into()),
            entry_points: vec!["/login".into()],
            supporting_evidence: vec!["evidence-1".into()],
            contradicting_evidence: Vec::new(),
        });
        state.deduction.experiments.push(DeductionExperimentState {
            hypothesis_id: "hypothesis-user-enumeration".into(),
            action: "Compare valid and invalid username probes.".into(),
            distinguishes: vec!["user_enumeration".into(), "generic_failure".into()],
        });
        state.deduction.conclusions.push(DeductionConclusionState {
            conclusion: "Login responses support enumeration.".into(),
            supporting_hypotheses: vec!["hypothesis-user-enumeration".into()],
            evidence_ids: vec!["evidence-1".into()],
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

        assert_eq!(frame.deduction_hypotheses.len(), 1);
        assert!(content.contains("[Deduction ledger]"));
        assert!(content.contains("Evidence:"));
        assert!(content.contains("Fact:"));
        assert!(content.contains("hypothesis-user-enumeration"));
        assert!(content.contains("Experiment:"));
        assert!(content.contains("Conclusion:"));
        assert!(content.contains("evidence-1"));
    }
}
