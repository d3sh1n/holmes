use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HolmesConfig {
    pub agent: AgentConfig,
    #[serde(default)]
    pub permissions: PermissionConfig,
    pub llm: LlmConfig,
    pub compressor: CompressorConfig,
    #[serde(default)]
    pub learning: LearningConfig,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PermissionConfig {
    #[serde(default)]
    pub mode: PermissionMode,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default)]
    pub disallowed_tools: Vec<String>,
    #[serde(default = "default_true")]
    pub auto_approve_read_only: bool,
}

impl Default for PermissionConfig {
    fn default() -> Self {
        Self {
            mode: PermissionMode::Default,
            allowed_tools: Vec::new(),
            disallowed_tools: Vec::new(),
            auto_approve_read_only: default_true(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Normal Holmes behavior: policy lists and guards decide.
    Default,
    /// Planning-only mode. Tools are blocked so Holmes must reason or ask.
    Plan,
    /// Read-only mode. Only tools marked read-only can run.
    ReadOnly,
    /// Accept edits mode. Holmes can perform file edits without explicit confirmation.
    AcceptEdits,
    /// Non-interactive mode. Policy lists still apply, but Holmes will not ask for approval.
    DontAsk,
    /// Maximum autonomy. Policy lists still apply; security guards remain active.
    Bypass,
}

impl Default for PermissionMode {
    fn default() -> Self {
        Self::Default
    }
}

impl std::str::FromStr for PermissionMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "default" => Ok(PermissionMode::Default),
            "plan" => Ok(PermissionMode::Plan),
            "read-only" | "readonly" => Ok(PermissionMode::ReadOnly),
            "accept-edits" | "acceptedits" => Ok(PermissionMode::AcceptEdits),
            "dont-ask" | "dontask" => Ok(PermissionMode::DontAsk),
            "bypass" => Ok(PermissionMode::Bypass),
            _ => Err("Invalid permission mode. Expected 'plan', 'default', 'read-only', 'accept-edits', 'dont-ask', or 'bypass'.".to_string()),
        }
    }
}

impl std::fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PermissionMode::Default => write!(f, "default"),
            PermissionMode::Plan => write!(f, "plan"),
            PermissionMode::ReadOnly => write!(f, "read-only"),
            PermissionMode::AcceptEdits => write!(f, "accept-edits"),
            PermissionMode::DontAsk => write!(f, "dont-ask"),
            PermissionMode::Bypass => write!(f, "bypass"),
        }
    }
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

fn default_max_retries() -> u32 {
    3
}
fn default_rpm_limit() -> u32 {
    60
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApiFormat {
    Openai,
    Anthropic,
}

impl Default for ApiFormat {
    fn default() -> Self {
        Self::Anthropic
    }
}


#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModel {
    pub model: String,
    pub provider: Option<String>,
}

pub fn resolve_attack_model_provider(
    config: &HolmesConfig,
    override_model: Option<String>,
) -> Option<ResolvedModel> {
    if let Some(model) = override_model {
        return Some(ResolvedModel {
            model,
            provider: None,
        });
    }

    let role = &config.llm.roles.attack_agent;
    config
        .llm
        .providers
        .iter()
        .find(|provider| &provider.name == role)
        .or_else(|| config.llm.providers.iter().min_by_key(|provider| provider.priority))
        .map(|provider| ResolvedModel {
            model: provider.model.clone(),
            provider: Some(provider.name.clone()),
        })
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
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_protect_last_n")]
    pub protect_last_n: usize,
    #[serde(default = "default_target_ratio")]
    pub target_ratio: f64,
    #[serde(default = "default_max_summary_tokens")]
    pub max_summary_tokens: u32,
    #[serde(default = "default_true")]
    pub preserve_tool_groups: bool,
    pub context_limit: u32,
    pub threshold: f64,
    pub protected_head: usize,
    pub protected_tail_tokens: u32,
}

fn default_true() -> bool {
    true
}

fn default_protect_last_n() -> usize {
    20
}

fn default_target_ratio() -> f64 {
    0.25
}

fn default_max_summary_tokens() -> u32 {
    12_000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdvisorConfig {
    pub enabled: bool,
    pub auto_apply_nudge: bool,
    pub auto_apply_suggest: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearningConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub background: bool,
    #[serde(default = "default_review_interval_turns")]
    pub review_interval_turns: u32,
    #[serde(default = "default_max_learning_candidates")]
    pub max_candidates_per_turn: usize,
    #[serde(default)]
    pub memory_write_approval: bool,
    #[serde(default = "default_true")]
    pub skill_write_approval: bool,
    #[serde(default = "default_true")]
    pub rule_write_approval: bool,
}

impl Default for LearningConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            background: default_true(),
            review_interval_turns: default_review_interval_turns(),
            max_candidates_per_turn: default_max_learning_candidates(),
            memory_write_approval: false,
            skill_write_approval: default_true(),
            rule_write_approval: default_true(),
        }
    }
}

fn default_review_interval_turns() -> u32 {
    1
}

fn default_max_learning_candidates() -> usize {
    5
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
    #[serde(default = "default_true")]
    pub read_state_seeding: bool,
    /// Window size for the repetition guard (number of recent calls to track).
    #[serde(default = "default_repetition_window")]
    pub repetition_window: usize,
}

fn default_repetition_window() -> usize {
    10
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
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_headless")]
    pub headless: bool,
    #[serde(default)]
    pub vision: bool,
    #[serde(default = "default_content_limit")]
    pub content_limit: usize,
    #[serde(default = "default_timeout")]
    pub timeout: u32,
    #[serde(default)]
    pub proxy: Option<String>,
    #[serde(default = "default_ignore_https")]
    pub ignore_https_errors: bool,
    #[serde(default)]
    pub executable_path: Option<String>,
    #[serde(default)]
    pub extra_launch_args: Vec<String>,
    #[serde(default)]
    pub screenshot_dir: Option<String>,
}

fn default_headless() -> bool {
    true
}
fn default_content_limit() -> usize {
    5000
}
fn default_timeout() -> u32 {
    30
}
fn default_ignore_https() -> bool {
    true
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            headless: true,
            vision: false,
            content_limit: 5000,
            timeout: 30,
            proxy: None,
            ignore_https_errors: true,
            executable_path: None,
            extra_launch_args: Vec::new(),
            screenshot_dir: None,
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
            permissions: PermissionConfig::default(),
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
                enabled: default_true(),
                protect_last_n: default_protect_last_n(),
                target_ratio: default_target_ratio(),
                max_summary_tokens: default_max_summary_tokens(),
                preserve_tool_groups: default_true(),
                context_limit: 128000,
                threshold: 0.75,
                protected_head: 3,
                protected_tail_tokens: 4000,
            },
            learning: LearningConfig::default(),
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
                read_state_seeding: true,
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
                executable_path: None,
                extra_launch_args: Vec::new(),
                screenshot_dir: None,
            },
            output_dir: "output".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn browser_config_serde_round_trip_new_fields() {
        let cfg = BrowserConfig {
            enabled: true,
            headless: false,
            vision: false,
            content_limit: 7000,
            timeout: 45,
            proxy: Some("http://127.0.0.1:8080".into()),
            ignore_https_errors: true,
            executable_path: Some("/usr/bin/chromium".into()),
            extra_launch_args: vec!["--lang=en".into()],
            screenshot_dir: None,
        };
        let yaml = serde_json::to_string(&cfg).unwrap();
        let back: BrowserConfig = serde_json::from_str(&yaml).unwrap();
        assert!(back.enabled);
        assert_eq!(back.executable_path.as_deref(), Some("/usr/bin/chromium"));
        assert_eq!(back.extra_launch_args, vec!["--lang=en".to_string()]);
    }

    #[test]
    fn browser_config_defaults_include_new_fields() {
        let cfg = BrowserConfig::default();
        assert!(!cfg.enabled);
        assert!(cfg.executable_path.is_none());
        assert!(cfg.extra_launch_args.is_empty());
        assert!(cfg.screenshot_dir.is_none());
    }

    #[test]
    fn browser_config_legacy_yaml_without_new_fields_loads() {
        let json = r#"{"enabled":false,"headless":true,"vision":false,"content_limit":5000,"timeout":30,"ignore_https_errors":true}"#;
        let cfg: BrowserConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.executable_path.is_none());
        assert!(cfg.extra_launch_args.is_empty());
    }

    #[test]
    fn resolve_attack_model_provider_prefers_override_then_role_provider() {
        let mut config = HolmesConfig::default();
        config.llm.roles.attack_agent = "main".into();
        config.llm.providers.push(ProviderConfig {
            name: "main".into(),
            base_url: "http://localhost".into(),
            api_key: String::new(),
            api_key_env: None,
            model: "role-model".into(),
            api_format: ApiFormat::Anthropic,
            priority: 0,
            max_retries: 3,
            rpm_limit: 60,
        });

        let override_resolved = resolve_attack_model_provider(&config, Some("override".into()));
        assert_eq!(
            override_resolved
                .as_ref()
                .map(|resolved| resolved.model.as_str()),
            Some("override")
        );
        assert_eq!(
            override_resolved.and_then(|resolved| resolved.provider),
            None
        );

        let role_resolved = resolve_attack_model_provider(&config, None);
        assert_eq!(
            role_resolved
                .as_ref()
                .map(|resolved| resolved.model.as_str()),
            Some("role-model")
        );
        assert_eq!(
            role_resolved.and_then(|resolved| resolved.provider),
            Some("main".into())
        );
    }

    #[test]
    fn resolve_attack_model_provider_falls_back_to_lowest_priority_provider() {
        let mut config = HolmesConfig::default();
        config.llm.roles.attack_agent = "missing".into();
        config.llm.providers = vec![
            ProviderConfig {
                name: "listed-first".into(),
                base_url: "http://localhost".into(),
                api_key: String::new(),
                api_key_env: None,
                model: "listed-first-model".into(),
                api_format: ApiFormat::Anthropic,
                priority: 20,
                max_retries: 3,
                rpm_limit: 60,
            },
            ProviderConfig {
                name: "highest-priority".into(),
                base_url: "http://localhost".into(),
                api_key: String::new(),
                api_key_env: None,
                model: "highest-priority-model".into(),
                api_format: ApiFormat::Anthropic,
                priority: 1,
                max_retries: 3,
                rpm_limit: 60,
            },
        ];

        let resolved = resolve_attack_model_provider(&config, None).expect("provider selected");

        assert_eq!(resolved.model, "highest-priority-model");
        assert_eq!(resolved.provider.as_deref(), Some("highest-priority"));
    }

    #[test]
    fn compressor_defaults_enable_static_compaction() {
        let compressor = HolmesConfig::default().compressor;

        assert!(compressor.enabled);
        assert_eq!(compressor.protect_last_n, 20);
        assert_eq!(compressor.target_ratio, 0.25);
        assert_eq!(compressor.max_summary_tokens, 12_000);
        assert!(compressor.preserve_tool_groups);
    }

    #[test]
    fn learning_defaults_enable_memory_review_with_approval_gates() {
        let learning = HolmesConfig::default().learning;

        assert!(learning.enabled);
        assert!(learning.background);
        assert_eq!(learning.review_interval_turns, 1);
        assert_eq!(learning.max_candidates_per_turn, 5);
        assert!(!learning.memory_write_approval);
        assert!(learning.skill_write_approval);
        assert!(learning.rule_write_approval);
    }

    #[test]
    fn permission_defaults_match_interactive_agent_mode() {
        let permissions = HolmesConfig::default().permissions;

        assert_eq!(permissions.mode, PermissionMode::Default);
        assert!(permissions.allowed_tools.is_empty());
        assert!(permissions.disallowed_tools.is_empty());
        assert!(permissions.auto_approve_read_only);
    }
}
