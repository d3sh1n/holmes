#[derive(Debug, Clone)]
pub struct GoalCondition {
    pub raw: String,
    pub criteria: Vec<Criterion>,
    pub stop_clause: Option<StopClause>,
}

#[derive(Debug, Clone)]
pub enum Criterion {
    Check { description: String, command: Option<String> },
    Count { description: String, target: u32 },
    FileCheck { path: String, should_exist: bool, should_contain: Option<String> },
    Statement { text: String },
}

#[derive(Debug, Clone)]
pub struct StopClause {
    pub max_turns: u32,
}

impl GoalCondition {
    pub fn parse(raw: &str) -> Self {
        let mut criteria = Vec::new();
        let mut stop_clause = None;

        if let Some(cap) = raw.find("stop after") {
            if let Some(turns_str) = raw[cap..].split_whitespace().nth(2) {
                if let Ok(turns) = turns_str.trim_end_matches(|c: char| !c.is_ascii_digit()).parse() {
                    stop_clause = Some(StopClause { max_turns: turns });
                }
            }
        }

        let main_part = if let Some(cap) = raw.find(" or stop after") {
            &raw[..cap]
        } else {
            raw
        };

        for part in main_part.split(&[';', '\n'][..]) {
            let trimmed = part.trim().trim_matches(|c: char| c == ',' || c == '。' || c == '.');
            if trimmed.is_empty() { continue; }
            criteria.push(Criterion::Statement { text: trimmed.to_string() });
        }

        GoalCondition { raw: raw.to_string(), criteria, stop_clause }
    }

    pub fn to_evaluator_prompt(&self) -> String {
        let criteria_text: Vec<String> = self.criteria.iter().enumerate().map(|(i, c)| match c {
            Criterion::Statement { text } => format!("{}. {}", i + 1, text),
            Criterion::Check { description, .. } => format!("{}. {}", i + 1, description),
            Criterion::Count { description, target } => format!("{}. {} (目标: {})", i + 1, description, target),
            Criterion::FileCheck { path, should_exist, .. } => {
                if *should_exist { format!("{}. 文件 {} 应该存在", i + 1, path) }
                else { format!("{}. 文件 {} 应该不存在", i + 1, path) }
            }
        }).collect();

        format!(
            "完成条件:\n{}\n\n请根据对话记录判断以上条件是否全部满足。只回答 YES 或 NO，然后给出简短理由。",
            criteria_text.join("\n")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple() {
        let cond = GoalCondition::parse("发现所有 SQL 注入漏洞并生成报告");
        assert_eq!(cond.criteria.len(), 1);
    }

    #[test]
    fn test_parse_with_stop() {
        let cond = GoalCondition::parse("审计 src/ 下所有文件 or stop after 20 turns");
        assert_eq!(cond.criteria.len(), 1);
        assert!(cond.stop_clause.is_some());
        assert_eq!(cond.stop_clause.unwrap().max_turns, 20);
    }

    #[test]
    fn test_parse_multiple() {
        let cond = GoalCondition::parse("完成信息收集\n发现所有漏洞\n生成最终报告");
        assert_eq!(cond.criteria.len(), 3);
    }
}
