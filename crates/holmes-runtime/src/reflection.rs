use crate::deliberation::{RuntimeError, RuntimeErrorKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReflectionOutcome {
    Continue,
    FinalAnswer(String),
    MaxIterationsReached(String),
    NeedsUser(String),
    RuntimeError {
        kind: RuntimeErrorKind,
        message: String,
    },
}

impl ReflectionOutcome {
    pub fn should_stop(&self) -> bool {
        !matches!(self, Self::Continue)
    }
}

#[derive(Debug, Clone)]
pub struct ReflectionEngine {
    max_iterations: usize,
}

impl ReflectionEngine {
    pub fn new(max_iterations: usize) -> Self {
        Self { max_iterations }
    }

    pub fn assess_iteration_budget(&self, iteration: usize) -> ReflectionOutcome {
        if iteration >= self.max_iterations {
            ReflectionOutcome::MaxIterationsReached(format!(
                "Holmes reached the maximum iteration limit ({}) before completing the investigation.",
                self.max_iterations
            ))
        } else {
            ReflectionOutcome::Continue
        }
    }

    pub fn assess_error(&self, error: &RuntimeError) -> ReflectionOutcome {
        match error.kind {
            RuntimeErrorKind::NeedsUser => ReflectionOutcome::NeedsUser(error.message.clone()),
            RuntimeErrorKind::Recoverable
            | RuntimeErrorKind::Fatal
            | RuntimeErrorKind::ContextOverflow => ReflectionOutcome::RuntimeError {
                kind: error.kind.clone(),
                message: error.message.clone(),
            },
        }
    }
}

impl Default for ReflectionEngine {
    fn default() -> Self {
        Self::new(90)
    }
}

#[cfg(test)]
mod tests {
    use crate::deliberation::{RuntimeError, MISSING_PROVIDER_MESSAGE};

    use super::*;

    #[test]
    fn reflection_maps_missing_provider_to_needs_user() {
        let engine = ReflectionEngine::default();
        let error = RuntimeError::missing_provider();

        assert_eq!(
            engine.assess_error(&error),
            ReflectionOutcome::NeedsUser(MISSING_PROVIDER_MESSAGE.into())
        );
    }

    #[test]
    fn reflection_preserves_recoverable_runtime_error_kind() {
        let engine = ReflectionEngine::default();
        let error = RuntimeError::recoverable("temporary failure");

        assert_eq!(
            engine.assess_error(&error),
            ReflectionOutcome::RuntimeError {
                kind: RuntimeErrorKind::Recoverable,
                message: "temporary failure".into()
            }
        );
    }

    #[test]
    fn reflection_preserves_fatal_runtime_error_kind() {
        let engine = ReflectionEngine::default();
        let error = RuntimeError::fatal("fatal failure");

        assert_eq!(
            engine.assess_error(&error),
            ReflectionOutcome::RuntimeError {
                kind: RuntimeErrorKind::Fatal,
                message: "fatal failure".into()
            }
        );
    }

    #[test]
    fn reflection_stops_at_max_iterations() {
        let engine = ReflectionEngine::new(2);

        assert!(engine.assess_iteration_budget(2).should_stop());
    }
}
