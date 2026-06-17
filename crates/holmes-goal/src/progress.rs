use holmes_core::event::Event;

#[derive(Debug, Clone)]
pub struct ProgressTracker {
    pub turns: u64,
    pub tokens: u64,
    pub start_time: chrono::DateTime<chrono::Utc>,
    pub last_summary: Option<String>,
}

impl ProgressTracker {
    pub fn new() -> Self {
        Self { turns: 0, tokens: 0, start_time: chrono::Utc::now(), last_summary: None }
    }

    pub fn record_turn(&mut self, tokens: u64, summary: Option<&str>) {
        self.turns += 1;
        self.tokens += tokens;
        if let Some(s) = summary { self.last_summary = Some(s.to_string()); }
    }

    pub fn elapsed(&self) -> chrono::Duration {
        chrono::Utc::now() - self.start_time
    }

    pub fn to_event(&self) -> Event {
        Event::GoalProgress {
            turns: self.turns,
            tokens: self.tokens,
            summary: self.last_summary.clone().unwrap_or_default(),
        }
    }
}

impl Default for ProgressTracker {
    fn default() -> Self { Self::new() }
}
