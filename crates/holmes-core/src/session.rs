use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::tool_types::Message;
use crate::types::{ContextSnapshot, SessionMode, TokenDelta};

/// RuntimeSession is the core runtime value that flows between Workflows.
/// It replaces the v1 pattern of separate `Session` (DB record) + `Vec<Message>` (runtime).
///
/// Like PyTorch's Tensor, RuntimeSession carries structured data, lineage,
/// and usage statistics as it moves through the system.
#[derive(Debug, Clone)]
pub struct RuntimeSession {
    pub id: String,
    pub title: Option<String>,
    pub mode: SessionMode,
    /// Current conversation messages
    pub messages: Vec<Message>,
    /// Fork/detach/merge lineage tracking
    pub lineage: SessionLineage,
    /// Real-time token accounting
    pub tokens: TokenDelta,
    /// Current context snapshot
    pub context: ContextSnapshot,
    pub created_at: DateTime<Utc>,
}

/// Tracks a session's position in a tree of forks and branches.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionLineage {
    pub parent_id: Option<String>,
    pub fork_point: Option<u64>,
    pub branches: Vec<String>,
}

impl RuntimeSession {
    pub fn new(id: String, mode: SessionMode) -> Self {
        Self {
            id,
            title: None,
            mode,
            messages: Vec::new(),
            lineage: SessionLineage::default(),
            tokens: TokenDelta::default(),
            context: ContextSnapshot {
                summary: String::new(),
                preserved_keys: Vec::new(),
                active_contexts: Vec::new(),
                timestamp: Utc::now(),
            },
            created_at: Utc::now(),
        }
    }

    /// Create a branch from this session at the current point.
    /// The new session shares lineage up to this point.
    pub fn fork(&self) -> Self {
        let new_id = uuid::Uuid::new_v4().to_string();
        Self {
            id: new_id,
            title: self.title.as_ref().map(|t| format!("{} (fork)", t)),
            mode: self.mode.clone(),
            messages: self.messages.clone(),
            lineage: SessionLineage {
                parent_id: Some(self.id.clone()),
                fork_point: Some(self.messages.len() as u64),
                branches: Vec::new(),
            },
            tokens: self.tokens.clone(),
            context: self.context.clone(),
            created_at: Utc::now(),
        }
    }

    /// Detach from parent lineage — this session becomes a root.
    pub fn detach(&mut self) {
        self.lineage.parent_id = None;
        self.lineage.fork_point = None;
    }

    /// Merge another session's messages into this one.
    /// Useful for combining results from parallel sub-workflows.
    pub fn merge(&mut self, other: &Self) {
        // Only merge messages that aren't already present
        let existing: std::collections::HashSet<String> = self
            .messages
            .iter()
            .filter_map(|m| m.content.clone())
            .collect();
        for msg in &other.messages {
            if let Some(ref content) = msg.content {
                if !existing.contains(content) {
                    self.messages.push(msg.clone());
                }
            }
        }
        self.tokens.input += other.tokens.input;
        self.tokens.output += other.tokens.output;
    }

    /// Add a system message at the beginning
    pub fn with_system_prompt(mut self, prompt: &str) -> Self {
        self.messages.insert(0, Message::system(prompt));
        self
    }

    /// Add a user message and return the updated session
    pub fn with_user_message(mut self, content: &str) -> Self {
        self.messages.push(Message::user(content));
        self
    }

    /// Get the current message count
    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    /// Get a context snapshot for persistence
    pub fn snapshot(&self) -> ContextSnapshot {
        self.context.clone()
    }

    /// Update the context snapshot
    pub fn update_context(&mut self, ctx: ContextSnapshot) {
        self.context = ctx;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fork_creates_independent_session() {
        let original = RuntimeSession::new("orig-1".into(), SessionMode::Pentest)
            .with_system_prompt("test")
            .with_user_message("hello");

        let forked = original.fork();

        assert_ne!(original.id, forked.id);
        assert_eq!(forked.messages.len(), original.messages.len());
        assert_eq!(forked.lineage.parent_id, Some("orig-1".into()));
        assert_eq!(forked.lineage.fork_point, Some(2));
    }

    #[test]
    fn test_detach_clears_lineage() {
        let mut session = RuntimeSession::new("child".into(), SessionMode::Pentest);
        session.lineage.parent_id = Some("parent".into());
        session.detach();
        assert!(session.lineage.parent_id.is_none());
    }

    #[test]
    fn test_merge_combines_unique_messages() {
        let mut a =
            RuntimeSession::new("a".into(), SessionMode::Pentest).with_user_message("msg from a");

        let b = RuntimeSession::new("b".into(), SessionMode::Pentest)
            .with_user_message("msg from a") // duplicate
            .with_user_message("msg from b"); // unique

        a.merge(&b);

        assert_eq!(a.message_count(), 2); // "msg from a" + "msg from b"
    }
}
