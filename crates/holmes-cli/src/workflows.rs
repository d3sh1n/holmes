use async_trait::async_trait;
use holmes_core::session::RuntimeSession;
use holmes_core::tool_types::*;
use holmes_core::workflow::{Workflow, WorkflowError};
use holmes_guards::GuardChain;
use holmes_llm::client::LlmClient;
use holmes_tools::ToolRegistry;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Run the core LLM ↔ Tool execution loop for one turn.
/// Shared by all built-in workflows.
async fn run_agent_turn(
    session: &mut RuntimeSession,
    stage_prompt: &str,
    llm: &LlmClient,
    registry: &ToolRegistry,
    guards: &Arc<Mutex<GuardChain>>,
) -> Result<(), WorkflowError> {
    session
        .messages
        .push(Message::user(format!("[当前阶段] {}", stage_prompt)));

    let tools = registry.definitions();
    let max_iterations = 10;
    let budget = IterationBudget::new(max_iterations);

    loop {
        if !budget.consume() {
            session
                .messages
                .push(Message::user("[阶段完成] 已达到本阶段最大轮次。"));
            return Ok(());
        }

        let response = llm
            .chat_completion(&session.messages, &tools, "attack_agent")
            .await
            .map_err(|e| WorkflowError::Llm(e.to_string()))?;

        let assistant_msg = response.to_message();
        session.messages.push(assistant_msg);

        if let Some(ref usage) = response.usage {
            session.tokens.input += usage.prompt_tokens as u64;
            session.tokens.output += usage.completion_tokens as u64;
        }

        if response.tool_calls.is_empty() {
            return Ok(());
        }

        let results = if registry.can_parallelize(&response.tool_calls) {
            let mut r = Vec::new();
            for call in &response.tool_calls {
                r.push(registry.execute(call).await);
            }
            r
        } else {
            let mut r = Vec::new();
            for call in &response.tool_calls {
                let verdict = {
                    let state = holmes_core::state::AttackState::new(
                        "unknown".into(),
                        String::new(),
                        String::new(),
                        String::new(),
                        vec![],
                    );
                    guards.lock().await.run_pre(call, &state).await
                };
                let result = if !verdict.allowed {
                    ToolResult::blocked(&call.id, verdict.guidance)
                } else {
                    registry.execute(call).await
                };
                r.push(result);
            }
            r
        };

        for result in &results {
            session.messages.push(result.to_message());
        }
    }
}

// ============================================================
// Built-in Workflows
// ============================================================

pub struct ReconWorkflow {
    pub llm: Arc<LlmClient>,
    pub registry: Arc<ToolRegistry>,
    pub guards: Arc<Mutex<GuardChain>>,
}

#[async_trait]
impl Workflow for ReconWorkflow {
    fn name(&self) -> &str {
        "recon"
    }
    fn description(&self) -> &str {
        "信息收集：端口扫描、子域名枚举、目录爆破、技术栈识别。当需要了解目标时使用。"
    }
    async fn forward(&self, session: &mut RuntimeSession) -> Result<(), WorkflowError> {
        run_agent_turn(
            session,
            "你正在进行信息收集阶段。使用工具扫描目标，了解攻击面。",
            &self.llm,
            &self.registry,
            &self.guards,
        )
        .await
    }
}

pub struct AnalysisWorkflow {
    pub llm: Arc<LlmClient>,
    pub registry: Arc<ToolRegistry>,
    pub guards: Arc<Mutex<GuardChain>>,
}

#[async_trait]
impl Workflow for AnalysisWorkflow {
    fn name(&self) -> &str {
        "analysis"
    }
    fn description(&self) -> &str {
        "深度分析：代码审计、漏洞分析、攻击面分析、逆向分析。当需要深入理解时使用。"
    }
    async fn forward(&self, session: &mut RuntimeSession) -> Result<(), WorkflowError> {
        run_agent_turn(
            session,
            "你正在进行分析阶段。深入分析目标，识别漏洞和弱点。",
            &self.llm,
            &self.registry,
            &self.guards,
        )
        .await
    }
}

pub struct ExploitWorkflow {
    pub llm: Arc<LlmClient>,
    pub registry: Arc<ToolRegistry>,
    pub guards: Arc<Mutex<GuardChain>>,
}

#[async_trait]
impl Workflow for ExploitWorkflow {
    fn name(&self) -> &str {
        "exploit"
    }
    fn description(&self) -> &str {
        "漏洞利用：漏洞利用、权限提升、横向移动、Payload构造。当需要执行攻击时使用。"
    }
    async fn forward(&self, session: &mut RuntimeSession) -> Result<(), WorkflowError> {
        run_agent_turn(
            session,
            "你正在进行漏洞利用阶段。利用已发现的漏洞，获取访问权限。",
            &self.llm,
            &self.registry,
            &self.guards,
        )
        .await
    }
}

pub struct ReportWorkflow {
    pub llm: Arc<LlmClient>,
    pub registry: Arc<ToolRegistry>,
    pub guards: Arc<Mutex<GuardChain>>,
}

#[async_trait]
impl Workflow for ReportWorkflow {
    fn name(&self) -> &str {
        "report"
    }
    fn description(&self) -> &str {
        "报告生成：生成渗透测试报告、整理发现、更新画报。当需要输出结果时使用。"
    }
    async fn forward(&self, session: &mut RuntimeSession) -> Result<(), WorkflowError> {
        run_agent_turn(
            session,
            "你正在生成报告。整理所有发现，生成结构化报告。",
            &self.llm,
            &self.registry,
            &self.guards,
        )
        .await
    }
}

pub struct ChatWorkflow {
    pub llm: Arc<LlmClient>,
    pub registry: Arc<ToolRegistry>,
    pub guards: Arc<Mutex<GuardChain>>,
}

#[async_trait]
impl Workflow for ChatWorkflow {
    fn name(&self) -> &str {
        "chat"
    }
    fn description(&self) -> &str {
        "对话模式：通用问答、任务澄清、计划制定。默认工作流，处理用户的各种请求。"
    }
    async fn forward(&self, session: &mut RuntimeSession) -> Result<(), WorkflowError> {
        run_agent_turn(
            session,
            "你是 Holmes。回答用户的问题，如果需要使用工具就使用。",
            &self.llm,
            &self.registry,
            &self.guards,
        )
        .await
    }
}

/// Create all built-in workflows
pub fn create_builtin_workflows(
    llm: Arc<LlmClient>,
    registry: Arc<ToolRegistry>,
    guards: Arc<Mutex<GuardChain>>,
) -> Vec<Box<dyn Workflow>> {
    vec![
        Box::new(ChatWorkflow {
            llm: llm.clone(),
            registry: registry.clone(),
            guards: guards.clone(),
        }),
        Box::new(ReconWorkflow {
            llm: llm.clone(),
            registry: registry.clone(),
            guards: guards.clone(),
        }),
        Box::new(AnalysisWorkflow {
            llm: llm.clone(),
            registry: registry.clone(),
            guards: guards.clone(),
        }),
        Box::new(ExploitWorkflow {
            llm: llm.clone(),
            registry: registry.clone(),
            guards: guards.clone(),
        }),
        Box::new(ReportWorkflow {
            llm,
            registry,
            guards,
        }),
    ]
}
