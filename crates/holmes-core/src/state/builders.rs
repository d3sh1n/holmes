use super::immutable::ImmutableFields;
use super::tool_truth::{AttackSurface, EvidenceBundle};
use super::validated::Finding;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub enum AttackPhase {
    Recon,
    Hypothesize,
    Validate,
    Exploit,
}

impl Default for AttackPhase {
    fn default() -> Self {
        Self::Recon
    }
}

impl std::fmt::Display for AttackPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Recon => write!(f, "recon"),
            Self::Hypothesize => write!(f, "hypothesize"),
            Self::Validate => write!(f, "validate"),
            Self::Exploit => write!(f, "exploit"),
        }
    }
}

/// Full attack state with HARNESS partitioning.
///
/// Partition enforcement:
/// - Immutable: `immutable` field, read-only getters
/// - Tool truth: `attack_surface` / `evidence_bundle`, only PostGuards write
/// - Validated: `findings`, only SkepticGate writes
/// - Free: everything else, agent loop writes directly
pub struct AttackState {
    // Immutable zone
    immutable: ImmutableFields,

    // Tool truth zone (pub(crate) — only PostGuards in this crate can write)
    pub(crate) attack_surface: AttackSurface,
    pub(crate) evidence_bundle: EvidenceBundle,

    // Validated zone (pub(crate) — only SkepticGate writes)
    pub(crate) findings: HashMap<String, Finding>,

    // Free zone (pub — agent loop writes directly)
    pub current_attack_type: String,
    pub current_objective: String,
    pub consecutive_failures: u32,
    pub no_tool_rounds: u32,
    pub is_finished: bool,
    pub is_authenticated: bool,
    pub flag: Option<String>,
    pub action_history: Vec<String>,
    pub phase: AttackPhase,
    pub soft404_baseline: Option<(u16, usize)>,
    pub last_progress_at: u32,
    pub file_access_tracker: HashMap<String, u64>,
}

impl AttackState {
    pub fn new(
        target_url: String,
        target_ip: String,
        challenge_id: String,
        challenge_name: String,
        hints: Vec<String>,
    ) -> Self {
        Self {
            immutable: ImmutableFields::new(
                target_url,
                target_ip,
                challenge_id,
                challenge_name,
                hints,
            ),
            attack_surface: AttackSurface::default(),
            evidence_bundle: EvidenceBundle::default(),
            findings: HashMap::new(),
            current_attack_type: String::new(),
            current_objective: String::new(),
            consecutive_failures: 0,
            no_tool_rounds: 0,
            is_finished: false,
            is_authenticated: false,
            flag: None,
            action_history: Vec::new(),
            phase: AttackPhase::default(),
            soft404_baseline: None,
            last_progress_at: 0,
            file_access_tracker: HashMap::new(),
        }
    }

    // Immutable zone — read-only delegation
    pub fn target_url(&self) -> &str {
        self.immutable.target_url()
    }
    pub fn target_ip(&self) -> &str {
        self.immutable.target_ip()
    }
    pub fn challenge_id(&self) -> &str {
        self.immutable.challenge_id()
    }
    pub fn challenge_name(&self) -> &str {
        self.immutable.challenge_name()
    }
    pub fn hints(&self) -> &[String] {
        self.immutable.hints()
    }

    // Tool truth zone — read-only public access
    pub fn attack_surface(&self) -> &AttackSurface {
        &self.attack_surface
    }
    pub fn evidence_bundle(&self) -> &EvidenceBundle {
        &self.evidence_bundle
    }
    pub fn attack_surface_mut(&mut self) -> &mut AttackSurface {
        &mut self.attack_surface
    }
    pub fn evidence_bundle_mut(&mut self) -> &mut EvidenceBundle {
        &mut self.evidence_bundle
    }

    pub fn findings_mut(&mut self) -> &mut HashMap<String, Finding> {
        &mut self.findings
    }
    pub fn findings(&self) -> &HashMap<String, Finding> {
        &self.findings
    }

    pub fn increment_no_tool_rounds(&mut self) {
        self.no_tool_rounds += 1;
    }
    pub fn reset_no_tool_rounds(&mut self) {
        self.no_tool_rounds = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_state() -> AttackState {
        AttackState::new(
            "http://target:8080".into(),
            "10.0.0.1".into(),
            "ch-001".into(),
            "Test Challenge".into(),
            vec!["hint1".into()],
        )
    }

    #[test]
    fn immutable_fields_accessible() {
        let state = make_state();
        assert_eq!(state.target_url(), "http://target:8080");
        assert_eq!(state.challenge_id(), "ch-001");
        assert_eq!(state.hints().len(), 1);
    }

    #[test]
    fn free_zone_writable() {
        let mut state = make_state();
        state.current_attack_type = "sqli".into();
        state.consecutive_failures = 3;
        state.is_finished = true;
        assert_eq!(state.current_attack_type, "sqli");
        assert!(state.is_finished);
    }

    #[test]
    fn no_tool_rounds_increment_and_reset() {
        let mut state = make_state();
        state.increment_no_tool_rounds();
        state.increment_no_tool_rounds();
        assert_eq!(state.no_tool_rounds, 2);
        state.reset_no_tool_rounds();
        assert_eq!(state.no_tool_rounds, 0);
    }
}
