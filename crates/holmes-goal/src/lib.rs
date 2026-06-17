pub mod condition;
pub mod decomposer;
pub mod evaluator;
pub mod progress;

use holmes_core::event::Event;
use holmes_core::types::*;
use condition::GoalCondition;
use decomposer::TaskDecomposer;
use evaluator::{GoalEvaluator, GoalEvaluation};
use progress::ProgressTracker;

pub struct GoalManager {
    pub condition: Option<GoalCondition>,
    evaluator: Option<GoalEvaluator>,
    decomposer: TaskDecomposer,
    tracker: ProgressTracker,
}

impl GoalManager {
    pub fn new() -> Self {
        Self {
            condition: None, evaluator: None,
            decomposer: TaskDecomposer::new(), tracker: ProgressTracker::new(),
        }
    }

    pub fn set(&mut self, condition_str: &str) -> Vec<Event> {
        let condition = GoalCondition::parse(condition_str);
        let subtasks = self.decomposer.decompose(condition_str);
        self.evaluator = Some(GoalEvaluator::new(condition.clone()));
        self.condition = Some(condition);
        self.tracker = ProgressTracker::new();
        vec![Event::GoalSet { condition: condition_str.to_string(), plan: None, subtasks }]
    }

    pub async fn evaluate(&mut self, conversation_summary: &str, turn_tokens: u64) -> GoalEvaluation {
        self.tracker.record_turn(turn_tokens, Some(conversation_summary));
        if let Some(ref mut evaluator) = self.evaluator {
            evaluator.evaluate(conversation_summary, 1, turn_tokens).await
        } else {
            GoalEvaluation { satisfied: true, reason: "无活跃 Goal".into(), turn_count: 0, tokens_spent: 0 }
        }
    }

    pub fn clear(&mut self, reason: &str) -> Event {
        self.condition = None;
        self.evaluator = None;
        Event::GoalCleared { reason: reason.to_string() }
    }

    pub fn status(&self) -> Option<GoalStatus> {
        self.evaluator.as_ref().map(|e| {
            let mut status = e.status();
            status.subtasks = self.decomposer.all_subtasks().to_vec();
            status
        })
    }

    pub fn update_subtask(&mut self, id: &str, status: SubTaskStatus, note: Option<&str>) -> Option<Event> {
        self.decomposer.update(id, status.clone(), note).map(|_| Event::SubtaskUpdate {
            subtask_id: id.to_string(), status, note: note.map(|s| s.to_string()),
        })
    }

    pub fn is_active(&self) -> bool { self.condition.is_some() }
}

impl Default for GoalManager {
    fn default() -> Self { Self::new() }
}
