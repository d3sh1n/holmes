use holmes_core::event::Event;

#[derive(Debug, Clone)]
pub struct HypothesisTracker {
    pub hypotheses: Vec<Hypothesis>,
    budget: u32,
}

#[derive(Debug, Clone)]
pub struct Hypothesis {
    pub id: String,
    pub attack_type: String,
    pub target: String,
    pub description: String,
    pub confidence: f64,
    pub status: HypothesisStatus,
    pub attempts: u32,
    pub max_attempts: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum HypothesisStatus {
    Pending,
    Active,
    Confirmed,
    Rejected,
    Deferred,
}

impl HypothesisTracker {
    pub fn new(budget: u32) -> Self {
        Self { hypotheses: Vec::new(), budget }
    }

    pub fn add(&mut self, attack_type: &str, target: &str, description: &str, confidence: f64) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        self.hypotheses.push(Hypothesis {
            id: id.clone(), attack_type: attack_type.into(), target: target.into(),
            description: description.into(), confidence, status: HypothesisStatus::Pending,
            attempts: 0, max_attempts: self.budget,
        });
        id
    }

    pub fn activate_next(&mut self) -> Option<&Hypothesis> {
        if let Some(h) = self.hypotheses.iter_mut().find(|h| h.status == HypothesisStatus::Pending) {
            h.status = HypothesisStatus::Active;
            Some(&*h)
        } else { None }
    }

    pub fn active(&self) -> Option<&Hypothesis> {
        self.hypotheses.iter().find(|h| h.status == HypothesisStatus::Active)
    }

    pub fn pending_count(&self) -> usize {
        self.hypotheses.iter().filter(|h| h.status == HypothesisStatus::Pending).count()
    }

    pub fn rejected(&self) -> Vec<&Hypothesis> {
        self.hypotheses.iter().filter(|h| h.status == HypothesisStatus::Rejected).collect()
    }

    pub fn confirmed(&self) -> Vec<&Hypothesis> {
        self.hypotheses.iter().filter(|h| h.status == HypothesisStatus::Confirmed).collect()
    }

    pub fn reject_active(&mut self, reason: &str) {
        if let Some(h) = self.hypotheses.iter_mut().find(|h| h.status == HypothesisStatus::Active) {
            h.status = HypothesisStatus::Rejected;
            tracing::info!(id = %h.id, reason, "hypothesis rejected");
        }
    }

    pub fn confirm_active(&mut self, evidence: &str) {
        if let Some(h) = self.hypotheses.iter_mut().find(|h| h.status == HypothesisStatus::Active) {
            h.status = HypothesisStatus::Confirmed;
            tracing::info!(id = %h.id, evidence, "hypothesis confirmed");
        }
    }

    pub fn record_attempt(&mut self) {
        if let Some(h) = self.hypotheses.iter_mut().find(|h| h.status == HypothesisStatus::Active) {
            h.attempts += 1;
            if h.attempts >= h.max_attempts {
                h.status = HypothesisStatus::Rejected;
                tracing::info!(id = %h.id, attempts = h.attempts, "hypothesis auto-rejected");
            }
        }
    }

    pub fn is_empty(&self) -> bool { self.hypotheses.is_empty() }

    pub fn to_context_string(&self) -> String {
        let mut parts = Vec::new();
        if let Some(active) = self.active() {
            parts.push(format!("当前假设: [{}] {} — {} (尝试 {}/{})",
                active.attack_type, active.target, active.description, active.attempts, active.max_attempts));
        }
        let rejected = self.rejected();
        if !rejected.is_empty() {
            parts.push(format!("已否定假设: {}", rejected.iter()
                .map(|h| format!("[{}] {}", h.attack_type, h.target)).collect::<Vec<_>>().join(", ")));
        }
        parts.push(format!("待验证假设: {}", self.pending_count()));
        parts.join("\n")
    }

    pub fn to_event(&self) -> Event {
        Event::HypothesisUpdate {
            active: self.active().map(|h| format!("[{}] {}", h.attack_type, h.description)),
            pending_count: self.pending_count(),
            rejected: self.rejected().iter().map(|h| h.description.clone()).collect(),
            confirmed: self.confirmed().iter().map(|h| h.description.clone()).collect(),
        }
    }
}
