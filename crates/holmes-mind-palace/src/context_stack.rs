use crate::memory_layer::ContextTargetExt;
use holmes_core::types::*;

#[derive(Debug, Clone, Default)]
pub struct ContextStack {
    stack: Vec<ContextTarget>,
    history: Vec<ContextSwitchRecord>,
}

#[derive(Debug, Clone)]
pub struct ContextSwitchRecord {
    pub from: Option<ContextTarget>,
    pub to: ContextTarget,
    pub reason: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

impl ContextStack {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, target: ContextTarget, reason: &str) -> ContextSwitchRecord {
        let from = self.current().cloned();
        let record = ContextSwitchRecord {
            from,
            to: target.clone(),
            reason: reason.to_string(),
            timestamp: chrono::Utc::now(),
        };
        self.history.push(record.clone());
        self.stack.push(target);
        record
    }

    pub fn pop(&mut self) -> Option<ContextTarget> {
        self.stack.pop()
    }

    pub fn current(&self) -> Option<&ContextTarget> {
        self.stack.last()
    }

    pub fn list(&self) -> &[ContextTarget] {
        &self.stack
    }

    pub fn depth(&self) -> usize {
        self.stack.len()
    }

    pub fn chain(&self) -> String {
        self.stack
            .iter()
            .map(|c| format!("{}:{}", c.kind_str(), c.label))
            .collect::<Vec<_>>()
            .join(" → ")
    }

    pub fn recent_switches(&self, n: usize) -> &[ContextSwitchRecord] {
        let start = self.history.len().saturating_sub(n);
        &self.history[start..]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_stack_push_pop() {
        let mut stack = ContextStack::new();
        assert!(stack.current().is_none());

        let host = ContextTarget {
            kind: ContextKind::Host,
            identifier: "10.0.0.5".into(),
            label: "web-server".into(),
        };
        stack.push(host.clone(), "SSH login");
        assert_eq!(stack.depth(), 1);
        assert_eq!(stack.current().unwrap().identifier, "10.0.0.5");

        let file = ContextTarget {
            kind: ContextKind::File,
            identifier: "/var/www/html/login.php".into(),
            label: "login.php".into(),
        };
        stack.push(file, "analyzing source");
        assert_eq!(stack.depth(), 2);

        stack.pop();
        assert_eq!(stack.depth(), 1);
        assert_eq!(stack.current().unwrap().identifier, "10.0.0.5");
    }
}
