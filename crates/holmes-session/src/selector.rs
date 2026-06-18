use holmes_core::session::RuntimeSession;
use holmes_core::workflow::{Workflow, WorkflowError};

/// Selector is an LLM-backed router that chooses the next Workflow
/// based on the current session state.
///
/// It replaces hardcoded supervisor intervention rules with
/// LLM-driven dynamic routing (inspired by OpenRath's Selector pattern).
///
/// Usage:
/// ```ignore
/// let selector = Selector::new();
/// selector.register(Box::new(ReconWorkflow::new(...)));
/// selector.register(Box::new(ExploitWorkflow::new(...)));
///
/// while let Some(name) = selector.select(&session, &llm).await? {
///     let wf = selector.get(&name).unwrap();
///     wf.forward(&mut session).await?;
/// }
/// ```
pub struct Selector {
    workflows: Vec<(String, String, Box<dyn Workflow>)>,
}

impl Selector {
    pub fn new() -> Self {
        Self { workflows: Vec::new() }
    }

    pub fn register(&mut self, workflow: Box<dyn Workflow>) {
        let name = workflow.name().to_string();
        let desc = workflow.description().to_string();
        self.workflows.push((name, desc, workflow));
    }

    pub fn get(&self, name: &str) -> Option<&dyn Workflow> {
        self.workflows.iter().find(|(n, _, _)| n == name).map(|(_, _, w)| w.as_ref())
    }

    pub fn workflow_names(&self) -> Vec<&str> {
        self.workflows.iter().map(|(n, _, _)| n.as_str()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.workflows.is_empty()
    }

    /// Build a selection prompt for the LLM
    pub fn selector_prompt(&self) -> String {
        let mut prompt = String::from(
            "你是一个工作流路由器。根据当前对话状态，选择最合适的下一步。\n\n可用工作流:\n"
        );
        for (name, desc, _) in &self.workflows {
            prompt.push_str(&format!("- {}: {}\n", name, desc));
        }
        prompt.push_str("\n规则:\n- 如果任务已完成，返回 DONE\n- 如果需要向用户提问，返回 CLARIFY\n- 否则选择最相关的工作流\n\n只回复工作流名称或 DONE。不要解释。\n");
        prompt
    }

    /// Let the LLM select the next workflow
    pub async fn select(
        &self,
        session: &RuntimeSession,
        llm: &holmes_llm::client::LlmClient,
    ) -> Result<Option<String>, WorkflowError> {
        if self.workflows.is_empty() {
            return Ok(None);
        }

        let context = self.build_selection_context(session);
        let prompt = format!("{}\n\n当前对话:\n{}", self.selector_prompt(), context);

        match llm.chat_completion_oneshot(&prompt, "选择下一步（只回复名称或DONE）", "attack_agent").await {
            Ok(resp) => {
                let choice = resp.content.unwrap_or_default().trim().to_uppercase();
                if choice == "DONE" || choice.is_empty() {
                    return Ok(None);
                }
                let lower = choice.to_lowercase();
                if self.get(&lower).is_some() {
                    return Ok(Some(lower));
                }
                // Fuzzy match
                for (name, _, _) in &self.workflows {
                    if lower.contains(name) {
                        return Ok(Some(name.clone()));
                    }
                }
                tracing::warn!(choice, "selector returned unrecognized workflow");
                Ok(None)
            }
            Err(e) => {
                tracing::warn!(error = %e, "selector LLM call failed");
                Ok(None)
            }
        }
    }

    fn build_selection_context(&self, session: &RuntimeSession) -> String {
        let mut ctx = String::new();

        // Last user message
        if let Some(last_user) = session.messages.iter().rev().find(|m| {
            matches!(m.role, holmes_core::Role::User)
        }) {
            if let Some(ref content) = last_user.content {
                ctx.push_str(&format!("用户: {}\n", content.chars().take(200).collect::<String>()));
            }
        }

        // Recent tool calls
        let recent: Vec<_> = session.messages.iter().rev()
            .filter(|m| m.tool_calls.is_some())
            .take(3).collect();
        if !recent.is_empty() {
            ctx.push_str("最近工具调用:\n");
            for msg in &recent {
                if let Some(ref calls) = msg.tool_calls {
                    for call in calls {
                        ctx.push_str(&format!("  - {}\n", call.function.name));
                    }
                }
            }
        }

        ctx.push_str(&format!("消息: {}, Tokens: {} in / {} out\n",
            session.message_count(), session.tokens.input, session.tokens.output));

        if !session.context.summary.is_empty() {
            ctx.push_str(&format!("态势: {}\n",
                session.context.summary.chars().take(300).collect::<String>()));
        }

        ctx
    }
}

impl Default for Selector {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    struct TestWf { name: String, desc: String }
    #[async_trait]
    impl Workflow for TestWf {
        fn name(&self) -> &str { &self.name }
        fn description(&self) -> &str { &self.desc }
        async fn forward(&self, _: &mut RuntimeSession) -> Result<(), WorkflowError> { Ok(()) }
    }

    #[test]
    fn test_selector_basics() {
        let mut s = Selector::new();
        s.register(Box::new(TestWf { name: "recon".into(), desc: "信息收集".into() }));
        s.register(Box::new(TestWf { name: "exploit".into(), desc: "漏洞利用".into() }));
        assert_eq!(s.workflow_names().len(), 2);
        assert!(s.get("recon").is_some());
        assert!(s.get("exploit").is_some());
        assert!(s.get("nonexistent").is_none());
    }

    #[test]
    fn test_selector_prompt_contains_workflows() {
        let mut s = Selector::new();
        s.register(Box::new(TestWf { name: "recon".into(), desc: "信息收集".into() }));
        let p = s.selector_prompt();
        assert!(p.contains("recon"));
        assert!(p.contains("信息收集"));
        assert!(p.contains("DONE"));
    }
}
