use holmes_core::types::*;

#[derive(Debug, Clone, Default)]
pub struct TaskDecomposer {
    subtasks: Vec<SubTask>,
}

impl TaskDecomposer {
    pub fn new() -> Self { Self::default() }

    pub fn decompose(&mut self, condition: &str) -> Vec<SubTask> {
        self.subtasks.clear();
        let task_descriptions: Vec<&str> = condition
            .split(&[';', '\n', '，', '、'][..])
            .map(|s| s.trim())
            .filter(|s| !s.is_empty() && s.len() > 3)
            .collect();

        for (i, desc) in task_descriptions.iter().enumerate() {
            self.subtasks.push(SubTask {
                id: format!("subtask-{}", i + 1),
                description: desc.to_string(),
                status: SubTaskStatus::Pending,
                note: None,
            });
        }
        self.subtasks.clone()
    }

    pub fn update(&mut self, subtask_id: &str, status: SubTaskStatus, note: Option<&str>) -> Option<SubTaskUpdate> {
        if let Some(task) = self.subtasks.iter_mut().find(|t| t.id == subtask_id) {
            task.status = status.clone();
            task.note = note.map(|s| s.to_string());
            Some(SubTaskUpdate { subtask_id: subtask_id.to_string(), status, note: note.map(|s| s.to_string()) })
        } else { None }
    }

    pub fn progress(&self) -> (usize, usize) {
        let total = self.subtasks.len();
        let completed = self.subtasks.iter().filter(|t| t.status == SubTaskStatus::Completed).count();
        (completed, total)
    }

    pub fn all_subtasks(&self) -> &[SubTask] { &self.subtasks }
}

#[derive(Debug, Clone)]
pub struct SubTaskUpdate {
    pub subtask_id: String,
    pub status: SubTaskStatus,
    pub note: Option<String>,
}
