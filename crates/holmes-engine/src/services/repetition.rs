use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct RepetitionDetector {
    threshold: usize,
    window: Vec<SemanticSignature>,
    max_window: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SemanticSignature(String);

impl RepetitionDetector {
    pub fn new(threshold: usize) -> Self {
        Self { threshold, window: Vec::new(), max_window: 50 }
    }

    pub fn record(&mut self, tool_name: &str, args: &str) {
        let sig = Self::normalize(tool_name, args);
        self.window.push(SemanticSignature(sig));
        if self.window.len() > self.max_window { self.window.remove(0); }
    }

    pub fn detect(&self) -> Option<RepetitionWarning> {
        if self.window.len() < self.threshold { return None; }
        let mut counts: HashMap<&SemanticSignature, usize> = HashMap::new();
        for sig in self.window.iter().rev().take(self.threshold * 2) {
            *counts.entry(sig).or_default() += 1;
        }
        for (sig, count) in counts {
            if count >= self.threshold {
                return Some(RepetitionWarning { pattern: sig.0.clone(), count });
            }
        }
        None
    }

    fn normalize(tool_name: &str, args: &str) -> String {
        let normalized: String = args.chars().map(|c| if c.is_ascii_digit() { 'N' } else { c }).collect();
        format!("{}:{}", tool_name, &normalized.chars().take(100).collect::<String>())
    }
}

#[derive(Debug, Clone)]
pub struct RepetitionWarning {
    pub pattern: String,
    pub count: usize,
}
