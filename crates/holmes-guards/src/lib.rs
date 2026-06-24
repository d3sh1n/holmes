pub mod post;
pub mod pre;
pub mod traits;

use holmes_core::config::GuardConfig;
use holmes_core::state::AttackState;
use holmes_core::{GuardVerdict, ToolCall, ToolResult};
use tracing::debug;
use traits::{PostGuard, PreGuard};

pub struct GuardChain {
    pub pre: Vec<Box<dyn PreGuard>>,
    pub post: Vec<Box<dyn PostGuard>>,
}

impl GuardChain {
    pub fn new() -> Self {
        Self {
            pre: Vec::new(),
            post: Vec::new(),
        }
    }

    pub fn from_config(config: &GuardConfig) -> Self {
        let mut chain = Self::new();

        if config.immutable_field {
            chain
                .pre
                .push(Box::new(pre::immutable_field::ImmutableFieldGuard));
        }
        if config.dangerous_command {
            chain
                .pre
                .push(Box::new(pre::dangerous_command::DangerousCommandGuard));
        }
        if config.repetition {
            chain
                .pre
                .push(Box::new(pre::repetition::RepetitionGuard::new(
                    config.repetition_window,
                )));
        }
        if config.read_state_seeding {
            chain
                .pre
                .push(Box::new(pre::file_tracker::FileTrackerPreGuard));
        }

        if config.attack_surface {
            chain
                .post
                .push(Box::new(post::attack_surface::AttackSurfaceUpdater::new()));
        }
        if config.evidence_extractor {
            chain
                .post
                .push(Box::new(post::evidence_extractor::EvidenceExtractor::new()));
        }
        if config.skeptic_gate {
            chain.post.push(Box::new(post::skeptic_gate::SkepticGate));
        }
        if config.failure_tracker {
            chain
                .post
                .push(Box::new(post::failure_tracker::FailureTracker));
        }
        if config.soft404 {
            chain
                .post
                .push(Box::new(post::soft404::Soft404Detector::new()));
        }
        if config.read_state_seeding {
            chain
                .post
                .push(Box::new(post::file_tracker::FileTrackerPostGuard));
        }

        chain
    }

    pub async fn run_pre(&self, call: &ToolCall, state: &AttackState) -> GuardVerdict {
        for guard in &self.pre {
            let verdict = guard.check(call, state).await;
            if !verdict.allowed {
                debug!(guard = guard.name(), tool = %call.function.name, "blocked by pre-guard");
                return verdict;
            }
        }
        GuardVerdict::allow()
    }

    pub async fn run_post(
        &mut self,
        call: &ToolCall,
        result: &ToolResult,
        state: &mut AttackState,
    ) {
        for guard in &mut self.post {
            guard.process(call, result, state).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use holmes_core::config::{GuardConfig, HolmesConfig};

    use super::*;

    #[test]
    fn guard_chain_respects_disabled_config_flags() {
        let mut config = HolmesConfig::default().guards;
        config.immutable_field = false;
        config.dangerous_command = false;
        config.repetition = false;
        config.attack_surface = false;
        config.evidence_extractor = false;
        config.skeptic_gate = false;
        config.failure_tracker = false;
        config.soft404 = false;
        config.read_state_seeding = false;

        let chain = GuardChain::from_config(&config);

        assert!(chain.pre.is_empty());
        assert!(chain.post.is_empty());
    }

    #[test]
    fn guard_chain_loads_enabled_defaults() {
        let config = GuardConfig {
            repetition_window: 10,
            ..HolmesConfig::default().guards
        };

        let chain = GuardChain::from_config(&config);

        assert_eq!(chain.pre.len(), 4);
        assert_eq!(chain.post.len(), 6);
    }
}
