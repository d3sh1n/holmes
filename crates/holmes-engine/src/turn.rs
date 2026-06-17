use holmes_core::types::*;

#[derive(Debug, Clone)]
pub struct TurnState {
    pub index: u64,
    pub events_start: u64,
    pub events_end: Option<u64>,
    pub tokens: TokenDelta,
    pub sub_agents_spawned: Vec<String>,
    pub phase: TurnPhase,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TurnPhase {
    Receiving,
    Understanding,
    Deciding,
    Executing,
    Synthesizing,
    Replying,
    Complete,
}

impl TurnState {
    pub fn new(index: u64, events_start: u64) -> Self {
        Self {
            index,
            events_start,
            events_end: None,
            tokens: TokenDelta::default(),
            sub_agents_spawned: Vec::new(),
            phase: TurnPhase::Receiving,
        }
    }

    pub fn transition(&mut self, phase: TurnPhase) {
        self.phase = phase;
    }

    pub fn complete(mut self, events_end: u64) -> TurnResult {
        self.events_end = Some(events_end);
        TurnResult {
            turn_index: self.index,
            reply: String::new(),
            tokens_used: self.tokens,
            sub_agents_spawned: self.sub_agents_spawned,
            events_produced: (self.events_start, events_end),
            dashboard_snapshot: None,
        }
    }
}
