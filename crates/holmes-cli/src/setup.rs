use holmes_core::config::{ApiFormat, HolmesConfig, ProviderConfig};
use std::io::{self, Write};
use std::path::PathBuf;

/// Built-in provider registry for setup wizard.
struct ProviderTemplate {
    name: &'static str,
    display: &'static str,
    base_url: &'static str,
    api_format: ApiFormat,
    default_model: &'static str,
    env_var: &'static str,
}

const PROVIDERS: &[ProviderTemplate] = &[
    ProviderTemplate {
        name: "anthropic",
        display: "Anthropic (Claude)",
        base_url: "https://api.anthropic.com",
        api_format: ApiFormat::Anthropic,
        default_model: "claude-sonnet-4-6",
        env_var: "ANTHROPIC_API_KEY",
    },
    ProviderTemplate {
        name: "openai",
        display: "OpenAI (GPT-4o, o4, etc.)",
        base_url: "https://api.openai.com/v1",
        api_format: ApiFormat::Openai,
        default_model: "gpt-4o",
        env_var: "OPENAI_API_KEY",
    },
    ProviderTemplate {
        name: "deepseek",
        display: "DeepSeek",
        base_url: "https://api.deepseek.com/v1",
        api_format: ApiFormat::Openai,
        default_model: "deepseek-chat",
        env_var: "DEEPSEEK_API_KEY",
    },
    ProviderTemplate {
        name: "openrouter",
        display: "OpenRouter (multi-provider aggregator)",
        base_url: "https://openrouter.ai/api/v1",
        api_format: ApiFormat::Openai,
        default_model: "anthropic/claude-sonnet-4",
        env_var: "OPENROUTER_API_KEY",
    },
    ProviderTemplate {
        name: "groq",
        display: "Groq (fast inference)",
        base_url: "https://api.groq.com/openai/v1",
        api_format: ApiFormat::Openai,
        default_model: "llama-3.3-70b-versatile",
        env_var: "GROQ_API_KEY",
    },
    ProviderTemplate {
        name: "custom",
        display: "Custom endpoint (enter URL manually)",
        base_url: "",
        api_format: ApiFormat::Openai,
        default_model: "",
        env_var: "",
    },
];

/// Run the interactive setup wizard.
pub fn run_setup(data_dir: &PathBuf) -> anyhow::Result<()> {
    println!("╔══════════════════════════════════════════════╗");
    println!("║  Holmes Setup — LLM Provider Configuration   ║");
    println!("╚══════════════════════════════════════════════╝");
    println!();
    println!("Holmes needs an LLM provider to function.");
    println!("Choose one of the supported providers below, or configure a custom endpoint.");
    println!();

    // Step 1: Pick provider
    println!("Available providers:");
    for (i, p) in PROVIDERS.iter().enumerate() {
        println!("  {}. {}", i + 1, p.display);
    }
    println!();

    let choice: usize = loop {
        print!("Select provider [1-{}]: ", PROVIDERS.len());
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        match input.trim().parse::<usize>() {
            Ok(n) if n >= 1 && n <= PROVIDERS.len() => break n - 1,
            _ => println!("Invalid selection. Choose 1-{}.", PROVIDERS.len()),
        }
    };

    let provider = &PROVIDERS[choice];

    // Step 2: Get API key
    println!();
    let api_key = if provider.name == "custom" {
        String::new() // custom endpoint may not need an API key
    } else {
        // Try auto-detect from env
        if let Ok(key) = std::env::var(provider.env_var) {
            if !key.is_empty() {
                println!("✓ Found {} in environment ({}...)", provider.env_var, &key[..8.min(key.len())]);
                key
            } else {
                prompt_api_key(provider)?
            }
        } else {
            prompt_api_key(provider)?
        }
    };

    // Step 3: Get base URL (for custom)
    let base_url = if provider.name == "custom" {
        print!("Enter API base URL (e.g. https://api.example.com/v1): ");
        io::stdout().flush()?;
        let mut url = String::new();
        io::stdin().read_line(&mut url)?;
        let url = url.trim().to_string();
        if url.is_empty() {
            anyhow::bail!("Base URL is required for custom endpoints.");
        }
        url
    } else {
        provider.base_url.to_string()
    };

    // Step 4: Get model name
    println!();
    let model = if provider.name == "custom" {
        print!("Enter model name: ");
        io::stdout().flush()?;
        let mut m = String::new();
        io::stdin().read_line(&mut m)?;
        let m = m.trim().to_string();
        if m.is_empty() {
            anyhow::bail!("Model name is required.");
        }
        m
    } else {
        println!("Default model for {}: {}", provider.display, provider.default_model);
        print!("Press Enter to accept, or type a different model: ");
        io::stdout().flush()?;
        let mut m = String::new();
        io::stdin().read_line(&mut m)?;
        let m = m.trim().to_string();
        if m.is_empty() { provider.default_model.to_string() } else { m }
    };

    // Step 5: Build and save config
    let config = HolmesConfig {
        llm: holmes_core::config::LlmConfig {
            providers: vec![ProviderConfig {
                name: provider.name.to_string(),
                base_url,
                api_key,
                api_key_env: Some(provider.env_var.to_string()),
                model,
                api_format: provider.api_format.clone(),
                priority: 0,
                max_retries: 3,
                rpm_limit: 50,
            }],
            roles: holmes_core::config::RoleConfig {
                attack_agent: provider.name.to_string(),
                supervisor: provider.name.to_string(),
                compressor: provider.name.to_string(),
                skill_evolver: provider.name.to_string(),
                goal_evaluator: provider.name.to_string(),
            },
        },
        ..HolmesConfig::default()
    };

    std::fs::create_dir_all(data_dir)?;
    let config_path = data_dir.join("config.yaml");
    let yaml = serde_yaml::to_string(&config)?;
    std::fs::write(&config_path, yaml)?;

    println!();
    println!("✓ Configuration saved to {}", config_path.display());
    println!();
    println!("Setup complete! Run 'holmes' to start.");
    Ok(())
}

fn prompt_api_key(provider: &ProviderTemplate) -> anyhow::Result<String> {
    println!("{} requires an API key.", provider.display);
    println!("Set environment variable {} or enter it now.", provider.env_var);
    print!("API Key (input hidden): ");
    io::stdout().flush()?;

    let key = rpassword::read_password()?;
    if key.trim().is_empty() {
        anyhow::bail!("API key is required. Set {} and re-run setup.", provider.env_var);
    }
    Ok(key.trim().to_string())
}
