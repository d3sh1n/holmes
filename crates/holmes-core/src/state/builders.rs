use super::immutable::ImmutableFields;
use super::tool_truth::{AttackSurface, EvidenceBundle};
use super::validated::Finding;
use crate::bounty::BountyCase;
use crate::event::Event;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A single task in the agent's working plan (written via the `write_todos` tool,
/// tracked by the `PlanTracker` PostGuard, surfaced in the perception frame).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    /// "pending" | "in_progress" | "completed".
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum AttackPhase {
    #[default]
    Recon,
    Hypothesize,
    Validate,
    Exploit,
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
    pub current_objective: String,
    pub consecutive_failures: u32,
    pub no_tool_rounds: u32,
    pub is_finished: bool,
    pub is_authenticated: bool,
    pub flag: Option<String>,
    /// The agent's working task list (written by `write_todos` / `PlanTracker`).
    pub plan: Vec<TodoItem>,
    pub phase: AttackPhase,
    pub soft404_baseline: Option<(u16, usize)>,
    pub last_progress_at: u32,
    pub file_access_tracker: HashMap<String, u64>,
    /// Findings recorded this step that the runtime should persist as `FindingRecorded`
    /// events (drained after PostGuards run, so findings survive turn boundaries/resume).
    pub pending_findings: Vec<Finding>,
    /// Authorized bounty/VDP program + in-scope asset inventory (free zone).
    /// Findings are NOT stored here — SkepticGate remains the sole validated-zone writer.
    pub bounty: BountyCase,
    /// Program/asset/report events queued this step for durable persistence.
    pub pending_bounty_events: Vec<Event>,
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
            current_objective: String::new(),
            consecutive_failures: 0,
            no_tool_rounds: 0,
            is_finished: false,
            is_authenticated: false,
            flag: None,
            plan: Vec::new(),
            phase: AttackPhase::default(),
            soft404_baseline: None,
            last_progress_at: 0,
            file_access_tracker: HashMap::new(),
            pending_findings: Vec::new(),
            bounty: BountyCase::default(),
            pending_bounty_events: Vec::new(),
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

    /// **Blessed writer for the validated zone.**
    ///
    /// In production code only `SkepticGate` (a PostGuard) should call this
    /// path — it owns the contract that a `Finding` has passed evidence
    /// cross-checks before being recorded. Callers in other production
    /// modules should treat findings as read-only via `findings()`.
    ///
    /// The key is `finding.id`. Confidence is **monotonic**: a re-report of an
    /// already-`Confirmed` finding with a weaker confidence (e.g. a thin restatement
    /// downgraded to `Candidate`) does NOT clobber the stronger record — it keeps the
    /// Confirmed one (but still refreshes evidence/severity/location if richer). This
    /// prevents a sloppy restatement from demoting a real vulnerability.
    pub fn record_finding(&mut self, finding: Finding) {
        use crate::state::validated::FindingConfidence;
        if let Some(existing) = self.findings.get(&finding.id) {
            if existing.confidence == FindingConfidence::Confirmed
                && finding.confidence != FindingConfidence::Confirmed
            {
                // Keep the confirmed verdict; adopt longer evidence/location if provided.
                let mut merged = existing.clone();
                if finding.evidence.len() > merged.evidence.len() {
                    merged.evidence = finding.evidence;
                }
                if !finding.location.is_empty() {
                    merged.location = finding.location;
                }
                self.findings.insert(merged.id.clone(), merged);
                return;
            }
        }
        self.findings.insert(finding.id.clone(), finding);
    }

    /// Record a finding AND queue it for durable persistence as a `FindingRecorded`
    /// event (drained by the runtime after PostGuards). Use this from SkepticGate so
    /// findings survive turn boundaries and session resume.
    pub fn record_and_persist_finding(&mut self, finding: Finding) {
        self.record_finding(finding.clone());
        // Persist the *effective* stored finding (post-monotonic-merge).
        if let Some(stored) = self.findings.get(&finding.id).cloned() {
            self.pending_findings.push(stored);
        }
    }

    /// Drain findings awaiting persistence (called by the runtime after PostGuards).
    pub fn take_pending_findings(&mut self) -> Vec<Finding> {
        std::mem::take(&mut self.pending_findings)
    }

    /// Drain bounty workflow events awaiting persistence.
    pub fn take_pending_bounty_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.pending_bounty_events)
    }

    /// Raw mutable access to the validated zone.
    ///
    /// ⚠ Only `SkepticGate` (via `record_finding`) and test fixtures should
    /// reach in here. Touching this from production paths outside SkepticGate
    /// breaks the four-zone invariant that validated findings are gated by
    /// evidence cross-checks. Use `record_finding` for the blessed path.
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
        state.current_objective = "enumerate".into();
        state.consecutive_failures = 3;
        state.is_finished = true;
        assert_eq!(state.current_objective, "enumerate");
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
