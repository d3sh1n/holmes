use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HolmesConfig {
    pub agent: AgentConfig,
    pub llm: LlmConfig,
    pub compressor: CompressorConfig,
    pub advisor: AdvisorConfig,
    pub guards: GuardConfig,
    pub memory: MemoryConfig,
    pub skills: SkillsConfig,
    pub recon: ReconConfig,
    pub mcp: McpConfig,
    pub browser: BrowserConfig,
    pub output_dir: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub max_iterations: u32,
    pub no_tool_threshold: u32,
    pub hypothesis_budget: u32,
    pub reflection_threshold: u32,
    pub reflection_cooldown: u32,
    pub stale_threshold: u32,
    pub force_pivot_threshold: u32,
    pub generate_reports: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    pub providers: Vec<ProviderConfig>,
    pub roles: RoleConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    pub api_base: String,
    pub api_key_env: String,
    pub model: String,
    pub format: ApiFormat,
    pub priority: u32,
    pub max_retries: u32,
    pub rpm_limit: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiFormat {
    Openai,
    Anthropic,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleConfig {
    pub attack_agent: String,
    pub supervisor: Option<String>,
    pub compressor: Option<String>,
    pub skill_evolver: Option<String>,
    pub goal_evaluator: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressorConfig {
    pub context_limit: u32,
    pub threshold: f64,
    pub protected_head: usize,
    pub protected_tail_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdvisorConfig {
    pub enabled: bool,
    pub auto_apply_nudge: bool,
    pub auto_apply_suggest: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuardConfig {
    pub immutable_field: bool,
    pub dangerous_command: bool,
    pub repetition: bool,
    pub attack_surface: bool,
    pub evidence_extractor: bool,
    pub skeptic_gate: bool,
    pub failure_tracker: bool,
    pub soft404: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    pub db_path: String,
    pub consolidation_threshold: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillsConfig {
    pub dir: String,
    pub auto_inject: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconConfig {
    pub auto_run: bool,
    pub nmap_top_ports: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConfig {
    pub servers: Vec<McpServerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub transport: McpTransport,
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpTransport {
    Stdio,
    Http,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserConfig {
    pub enabled: bool,
    pub vision: bool,
    pub content_limit: u32,
    pub timeout: u32,
    pub headless: bool,
    pub proxy: Option<String>,
    pub ignore_https_errors: bool,
    pub mcp_command: String,
    pub mcp_args: Vec<String>,
}

impl Default for HolmesConfig {
    fn default() -> Self {
        Self {
            agent: AgentConfig {
                max_iterations: 90,
                no_tool_threshold: 3,
                hypothesis_budget: 8,
                reflection_threshold: 5,
                reflection_cooldown: 3,
                stale_threshold: 8,
                force_pivot_threshold: 15,
                generate_reports: true,
            },
            llm: LlmConfig {
                providers: vec![],
                roles: RoleConfig {
                    attack_agent: "default".into(),
                    supervisor: None,
                    compressor: None,
                    skill_evolver: None,
                    goal_evaluator: None,
                },
            },
            compressor: CompressorConfig {
                context_limit: 128000,
                threshold: 0.75,
                protected_head: 3,
                protected_tail_tokens: 4000,
            },
            advisor: AdvisorConfig {
                enabled: true,
                auto_apply_nudge: true,
                auto_apply_suggest: false,
            },
            guards: GuardConfig {
                immutable_field: true,
                dangerous_command: true,
                repetition: true,
                attack_surface: true,
                evidence_extractor: true,
                skeptic_gate: true,
                failure_tracker: true,
                soft404: true,
            },
            memory: MemoryConfig {
                db_path: "data/memory.db".into(),
                consolidation_threshold: 0.85,
            },
            skills: SkillsConfig {
                dir: "skills".into(),
                auto_inject: true,
            },
            recon: ReconConfig {
                auto_run: false,
                nmap_top_ports: 100,
            },
            mcp: McpConfig { servers: vec![] },
            browser: BrowserConfig {
                enabled: false,
                vision: false,
                content_limit: 5000,
                timeout: 30,
                headless: true,
                proxy: None,
                ignore_https_errors: true,
                mcp_command: "node".into(),
                mcp_args: vec!["browser-mcp/dist/index.js".into()],
            },
            output_dir: "output".into(),
        }
    }
}
