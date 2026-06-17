use holmes_core::types::*;
use holmes_session::db::{CreateSessionParams, SessionDB};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::protocol::DispatchTask;
use crate::types::*;

pub struct SubAgentDispatcher {
    session_db: Arc<SessionDB>,
    active_agents: Arc<Mutex<HashMap<String, SubAgentHandle>>>,
}

impl SubAgentDispatcher {
    pub fn new(session_db: Arc<SessionDB>) -> Self {
        Self {
            session_db,
            active_agents: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn dispatch(
        &self,
        parent_session_id: &str,
        agent_type: AgentType,
        task: DispatchTask,
    ) -> Result<SubAgentHandle, String> {
        let config = AgentTypeConfig::find(&agent_type);
        let sub_session_id = uuid::Uuid::new_v4().to_string();

        let title_snippet: String = task.task.chars().take(30).collect();
        self.session_db
            .create_session(CreateSessionParams {
                id: Some(sub_session_id.clone()),
                title: Some(format!("{}-{}", agent_type_str(&agent_type), title_snippet)),
                mode: Some(SessionMode::Pentest),
                model: Some(config.default_model.to_string()),
                system_prompt: Some(build_sub_agent_prompt(&agent_type)),
                parent_session_id: Some(parent_session_id.to_string()),
                fork_point: None,
                source: Some("subagent".into()),
                tags: vec![agent_type_str(&agent_type).to_string()],
            })
            .await
            .map_err(|e| e.to_string())?;

        let handle = SubAgentHandle {
            sub_session_id: sub_session_id.clone(),
            agent_type: agent_type.clone(),
            status: SubAgentStatus::Running,
        };
        self.active_agents
            .lock()
            .await
            .insert(sub_session_id, handle.clone());
        Ok(handle)
    }

    pub async fn dispatch_parallel(
        &self,
        parent_session_id: &str,
        tasks: Vec<(AgentType, DispatchTask)>,
    ) -> Result<Vec<SubAgentHandle>, String> {
        let mut handles = Vec::new();
        for (agent_type, task) in tasks {
            let handle = self.dispatch(parent_session_id, agent_type, task).await?;
            handles.push(handle);
        }
        Ok(handles)
    }

    pub async fn active_agents_list(&self) -> Vec<SubAgentHandle> {
        self.active_agents.lock().await.values().cloned().collect()
    }

    pub async fn cancel(&self, handle: &SubAgentHandle) -> Result<(), String> {
        let mut agents = self.active_agents.lock().await;
        if let Some(h) = agents.get_mut(&handle.sub_session_id) {
            h.status = SubAgentStatus::Cancelled;
        }
        Ok(())
    }
}

fn agent_type_str(at: &AgentType) -> &'static str {
    match at {
        AgentType::Scout => "scout",
        AgentType::Analyst => "analyst",
        AgentType::Operative => "operative",
        AgentType::Ghost => "ghost",
        AgentType::Chronicler => "chronicler",
    }
}

fn build_sub_agent_prompt(agent_type: &AgentType) -> String {
    match agent_type {
        AgentType::Scout => "你是 Holmes 的侦察员（Scout）。快速收集信息：端口扫描、子域名枚举、目录爆破、代码搜索。只读操作，快速返回结构化结果。".into(),
        AgentType::Analyst => "你是 Holmes 的分析师（Analyst）。深度分析：代码审计、漏洞分析、逆向分析、攻击面分析。只读操作，返回详细的结构化发现。".into(),
        AgentType::Operative => "你是 Holmes 的行动员（Operative）。执行操作：漏洞利用、权限提升、横向移动、Payload 构造。可以使用所有工具。".into(),
        AgentType::Ghost => "你是 Holmes 的幽灵（Ghost）。在隔离环境中执行高风险操作。操作完成后不留痕迹。".into(),
        AgentType::Chronicler => "你是 Holmes 的记录员（Chronicler）。生成报告、整理发现、更新画报。可以读取对话记录和写入文件。".into(),
    }
}
