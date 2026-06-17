use holmes_core::types::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTypeConfig {
    pub agent_type: AgentType,
    pub description: &'static str,
    pub default_model: &'static str,
    pub default_max_turns: u32,
    pub read_only: bool,
    pub isolation: Option<&'static str>,
}

pub const BUILTIN_AGENTS: &[AgentTypeConfig] = &[
    AgentTypeConfig {
        agent_type: AgentType::Scout,
        description: "快速侦察 — 端口扫描、子域名枚举、代码搜索、信息收集。只读操作，低成本模型。",
        default_model: "haiku",
        default_max_turns: 10,
        read_only: true,
        isolation: None,
    },
    AgentTypeConfig {
        agent_type: AgentType::Analyst,
        description: "深度分析 — 代码审计、漏洞分析、逆向分析、攻击面分析。只读操作，标准模型。",
        default_model: "sonnet",
        default_max_turns: 20,
        read_only: true,
        isolation: None,
    },
    AgentTypeConfig {
        agent_type: AgentType::Operative,
        description: "行动执行 — 漏洞利用、权限提升、横向移动、Payload 构造。全工具访问。",
        default_model: "sonnet",
        default_max_turns: 30,
        read_only: false,
        isolation: None,
    },
    AgentTypeConfig {
        agent_type: AgentType::Ghost,
        description: "隐蔽行动 — 高风险操作，沙箱隔离。exploit 测试、内网扫描、提权尝试。",
        default_model: "sonnet",
        default_max_turns: 25,
        read_only: false,
        isolation: Some("worktree"),
    },
    AgentTypeConfig {
        agent_type: AgentType::Chronicler,
        description: "记录整理 — 生成报告、整理发现、更新画报。只读对话记录 + 文件写入。",
        default_model: "haiku",
        default_max_turns: 8,
        read_only: false,
        isolation: None,
    },
];

impl AgentTypeConfig {
    pub fn find(agent_type: &AgentType) -> &'static AgentTypeConfig {
        BUILTIN_AGENTS
            .iter()
            .find(|a| a.agent_type == *agent_type)
            .expect("all AgentType variants must be present in BUILTIN_AGENTS")
    }
}
