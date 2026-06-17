use holmes_core::event::InterventionLevel;

#[derive(Debug, Clone)]
pub struct Advisor {
    pub enabled: bool,
    pub auto_apply_nudge: bool,
    pub auto_apply_suggest: bool,
    interventions: u32,
}

impl Advisor {
    pub fn new(enabled: bool, auto_apply_nudge: bool, auto_apply_suggest: bool) -> Self {
        Self { enabled, auto_apply_nudge, auto_apply_suggest, interventions: 0 }
    }

    pub fn evaluate(&mut self, stale_iterations: u32, stale_threshold: u32, force_threshold: u32, no_tool_rounds: u32) -> Option<AdvisorRecommendation> {
        if !self.enabled { return None; }
        if no_tool_rounds >= 3 {
            self.interventions += 1;
            return Some(AdvisorRecommendation {
                level: InterventionLevel::Nudge,
                advice: "连续多轮没有使用工具。请执行具体操作推进任务。".into(),
                reasoning: format!("no_tool_rounds={}", no_tool_rounds),
                auto_applied: self.auto_apply_nudge,
            });
        }
        if stale_iterations >= force_threshold {
            self.interventions += 1;
            return Some(AdvisorRecommendation {
                level: InterventionLevel::ForcePivot,
                advice: "长期没有进展，建议彻底改变当前方向。".into(),
                reasoning: format!("stale={} >= force={}", stale_iterations, force_threshold),
                auto_applied: self.auto_apply_suggest,
            });
        }
        if stale_iterations >= stale_threshold {
            self.interventions += 1;
            return Some(AdvisorRecommendation {
                level: InterventionLevel::Suggest,
                advice: "当前方向可能存在问题，建议考虑替代方案。".into(),
                reasoning: format!("stale={} >= threshold={}", stale_iterations, stale_threshold),
                auto_applied: self.auto_apply_suggest,
            });
        }
        None
    }

    pub fn interventions(&self) -> u32 { self.interventions }
}

#[derive(Debug, Clone)]
pub struct AdvisorRecommendation {
    pub level: InterventionLevel,
    pub advice: String,
    pub reasoning: String,
    pub auto_applied: bool,
}
