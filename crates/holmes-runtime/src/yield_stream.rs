use chrono::Utc;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StreamEvent {
    pub event_id: String,
    pub session_id: String,
    pub timestamp: String,
    #[serde(flatten)]
    pub data: RuntimeYield,
}

impl StreamEvent {
    pub fn new(session_id: impl Into<String>, data: RuntimeYield) -> Self {
        Self {
            event_id: uuid::Uuid::new_v4().to_string(),
            session_id: session_id.into(),
            timestamp: Utc::now().to_rfc3339(),
            data,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeYield {
    MessageToUser {
        content: String,
    },
    PlanUpdate {
        content: String,
    },
    ToolStarted {
        name: String,
        call_id: Option<String>,
    },
    PermissionDecision {
        tool_name: String,
        call_id: Option<String>,
        allowed: bool,
        reason: String,
    },
    ToolFinished {
        name: String,
        call_id: Option<String>,
        success: bool,
        content: String,
        error: Option<String>,
        usage: Option<TokenUsage>,
    },
    EvidenceUpdate {
        content: String,
    },
    NeedsUserInput {
        prompt: String,
    },
    CompactionBoundary {
        before_count: usize,
        after_count: usize,
        summary: String,
        preserved_keys: Vec<String>,
        method: String,
    },
    FinalAnswer {
        content: String,
        usage: Option<TokenUsage>,
    },
    Error {
        message: String,
    },
}

pub trait RuntimeSink: Send {
    fn emit(&mut self, event: StreamEvent);

    fn emit_yield(&mut self, session_id: &str, yield_data: RuntimeYield) {
        self.emit(StreamEvent::new(session_id, yield_data));
    }
}

#[derive(Debug, Clone, Default)]
pub struct VecSink {
    pub events: Vec<StreamEvent>,
}

impl VecSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn into_events(self) -> Vec<StreamEvent> {
        self.events
    }

    pub fn yields(&self) -> Vec<RuntimeYield> {
        self.events.iter().map(|e| e.data.clone()).collect()
    }
}

impl RuntimeSink for VecSink {
    fn emit(&mut self, event: StreamEvent) {
        self.events.push(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vec_sink_collects_runtime_yields() {
        let mut sink = VecSink::new();
        let session_id = "test-session";

        sink.emit(StreamEvent::new(
            session_id,
            RuntimeYield::MessageToUser {
                content: "hello".into(),
            },
        ));
        sink.emit(StreamEvent::new(
            session_id,
            RuntimeYield::FinalAnswer {
                content: "done".into(),
                usage: None,
            },
        ));

        let events = sink.into_events();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0].data, RuntimeYield::MessageToUser { .. }));
        assert!(matches!(events[1].data, RuntimeYield::FinalAnswer { .. }));
        assert_eq!(events[0].session_id, session_id);
    }
}
