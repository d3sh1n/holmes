use async_trait::async_trait;

use crate::session::RuntimeSession;

/// A Workflow is a reusable, composable unit of agent work.
///
/// Like PyTorch's `nn.Module`, each Workflow implements `forward(session) -> session`.
/// Workflows can be nested, chained, and selected dynamically.
///
/// Each Workflow has a `description()` that the Selector uses to decide
/// when to route to it.
#[async_trait]
pub trait Workflow: Send + Sync {
    /// Human-readable name
    fn name(&self) -> &str;

    /// Description for the Selector — what this workflow does and when to use it
    fn description(&self) -> &str;

    /// Execute the workflow: take a session, return a modified session.
    async fn forward(&self, session: &mut RuntimeSession) -> Result<(), WorkflowError>;
}

/// Error type for workflow execution
#[derive(Debug, thiserror::Error)]
pub enum WorkflowError {
    #[error("LLM error: {0}")]
    Llm(String),
    #[error("Tool error: {0}")]
    Tool(String),
    #[error("Guard blocked: {0}")]
    GuardBlocked(String),
    #[error("Workflow error: {0}")]
    Other(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestWorkflow {
        name: String,
        desc: String,
    }

    #[async_trait]
    impl Workflow for TestWorkflow {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            &self.desc
        }
        async fn forward(&self, _session: &mut RuntimeSession) -> Result<(), WorkflowError> {
            Ok(())
        }
    }

    #[test]
    fn test_workflow_trait() {
        let wf = TestWorkflow {
            name: "test".into(),
            desc: "test workflow".into(),
        };
        assert_eq!(wf.name(), "test");
        assert_eq!(wf.description(), "test workflow");
    }
}
