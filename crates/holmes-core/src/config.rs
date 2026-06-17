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
    /// Backend HTTP base URL.
    #[serde(alias = "api_base")]
    pub base_url: String,
    /// Resolved API key (after env-var lookup).
    #[serde(default)]
    pub api_key: String,
    /// Optional environment variable to read the key from at runtime.
    #[serde(default)]
    pub api_key_env: Option<String>,
    pub model: String,
    /// API wire format (`openai` | `anthropic`).
    #[serde(default, alias = "format")]
    pub api_format: ApiFormat,
    #[serde(default)]
    pub priority: u32,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    #[serde(default = "default_rpm_limit")]
    pub rpm_limit: u32,
}

fn default_max_retries() -> u32 { 3 }
fn default_rpm_limit() -> u32 { 60 }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApiFormat {
    Openai,
    Anthropic,
}

impl Default for ApiFormat {
    fn default() -> Self { Self::Openai }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleConfig {
    pub attack_agent: String,
    #[serde(default)]
    pub supervisor: String,
    #[serde(default)]
    pub compressor: String,
    #[serde(default)]
    pub skill_evolver: String,
    #[serde(default)]
    pub goal_evaluator: String,
}

/// Alias for compatibility with apeiron-core call sites that imported `RoleAssignment`.
pub type RoleAssignment = RoleConfig;

/// Alias for compatibility with apeiron-core call sites that imported `Config`.
pub type Config = HolmesConfig;

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
    /// Window size for the repetition guard (number of recent calls to track).
    #[serde(default = "default_repetition_window")]
    pub repetition_window: usize,
}

fn default_repetition_window() -> usize { 10 }

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
    pub content_limit: usize,
    pub timeout: u32,
    pub headless: bool,
    pub proxy: Option<String>,
    pub ignore_https_errors: bool,
    pub mcp_command: String,
    pub mcp_args: Vec<String>,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            vision: false,
            content_limit: 5000,
            timeout: 30,
            headless: true,
            proxy: None,
            ignore_https_errors: true,
            mcp_command: "node".into(),
            mcp_args: vec!["browser-mcp/dist/index.js".into()],
        }
    }
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
                    supervisor: String::new(),
                    compressor: String::new(),
                    skill_evolver: String::new(),
                    goal_evaluator: String::new(),
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
                repetition_window: 10,
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
