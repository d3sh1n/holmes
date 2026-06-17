use holmes_core::types::*;
use crate::condition::GoalCondition;

#[derive(Debug, Clone)]
pub struct GoalEvaluator {
    condition: GoalCondition,
    turn_count: u64,
    tokens_spent: u64,
}

impl GoalEvaluator {
    pub fn new(condition: GoalCondition) -> Self {
        Self { condition, turn_count: 0, tokens_spent: 0 }
    }

    pub async fn evaluate(
        &mut self,
        conversation_summary: &str,
        turn_delta: u64,
        token_delta: u64,
    ) -> GoalEvaluation {
        self.turn_count += turn_delta;
        self.tokens_spent += token_delta;

        if let Some(ref stop) = self.condition.stop_clause {
            if self.turn_count >= stop.max_turns as u64 {
                return GoalEvaluation {
                    satisfied: true,
                    reason: format!("达到停止条件: {} turns", self.turn_count),
                    turn_count: self.turn_count,
                    tokens_spent: self.tokens_spent,
                };
            }
        }

        let satisfied = conversation_summary.contains("report_generated")
            || conversation_summary.contains("ReportGenerated");

        GoalEvaluation {
            satisfied,
            reason: if satisfied { "对话记录显示报告已生成，条件满足".into() }
                    else { "条件尚未满足，继续工作".into() },
            turn_count: self.turn_count,
            tokens_spent: self.tokens_spent,
        }
    }

    pub fn status(&self) -> GoalStatus {
        GoalStatus {
            condition: self.condition.raw.clone(),
            satisfied: false, reason: None,
            turn_count: self.turn_count, tokens_spent: self.tokens_spent,
            subtasks: vec![],
        }
    }
}

#[derive(Debug, Clone)]
pub struct GoalEvaluation {
    pub satisfied: bool,
    pub reason: String,
    pub turn_count: u64,
    pub tokens_spent: u64,
}
