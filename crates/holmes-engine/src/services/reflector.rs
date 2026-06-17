use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct Reflector {
    pub stagnation: StagnationTracker,
    threshold: u32,
    cooldown: u32,
    cooldown_remaining: u32,
}

#[derive(Debug, Clone, Default)]
pub struct StagnationTracker {
    counts: HashMap<String, u32>,
}

impl StagnationTracker {
    pub fn record_call(&mut self, entry_point: &str) {
        *self.counts.entry(entry_point.to_string()).or_default() += 1;
    }
    pub fn count(&self, entry_point: &str) -> u32 {
        self.counts.get(entry_point).copied().unwrap_or(0)
    }
}

impl Reflector {
    pub fn new(threshold: u32, cooldown: u32) -> Self {
        Self { stagnation: StagnationTracker::default(), threshold, cooldown, cooldown_remaining: 0 }
    }
    pub fn tick_cooldown(&mut self) {
        if self.cooldown_remaining > 0 { self.cooldown_remaining -= 1; }
    }
    pub fn is_stagnated(&self, entry_point: &str) -> bool {
        self.stagnation.count(entry_point) >= self.threshold
    }
    pub fn check_entry_point(&mut self, entry_point: &str) -> ReflectionAction {
        if self.cooldown_remaining > 0 { return ReflectionAction::None; }
        let count = self.stagnation.count(entry_point);
        if count >= self.threshold * 3 { ReflectionAction::Escalate }
        else if count >= self.threshold { self.cooldown_remaining = self.cooldown; ReflectionAction::Reflect }
        else { ReflectionAction::None }
    }
    pub fn extract_entry_point(tool_name: &str, args: &str) -> String {
        format!("{}:{}", tool_name, &args.chars().take(50).collect::<String>())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ReflectionAction { None, Reflect, Escalate }
