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

        chain
            .pre
            .push(Box::new(pre::immutable_field::ImmutableFieldGuard));
        chain
            .pre
            .push(Box::new(pre::dangerous_command::DangerousCommandGuard));
        chain
            .pre
            .push(Box::new(pre::repetition::RepetitionGuard::new(
                config.repetition_window,
            )));

        chain
            .post
            .push(Box::new(post::attack_surface::AttackSurfaceUpdater::new()));
        chain
            .post
            .push(Box::new(post::evidence_extractor::EvidenceExtractor::new()));
        chain.post.push(Box::new(post::skeptic_gate::SkepticGate));
        chain
            .post
            .push(Box::new(post::failure_tracker::FailureTracker));
        chain
            .post
            .push(Box::new(post::soft404::Soft404Detector::new()));

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
