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
    models_path: &'static str, // API path for listing models
}

const PROVIDERS: &[ProviderTemplate] = &[
    ProviderTemplate {
        name: "anthropic",
        display: "Anthropic (Claude)",
        base_url: "https://api.anthropic.com",
        api_format: ApiFormat::Anthropic,
        default_model: "claude-sonnet-4-6",
        env_var: "ANTHROPIC_API_KEY",
        models_path: "",
    },
    ProviderTemplate {
        name: "openai",
        display: "OpenAI (GPT-4o, o4, etc.)",
        base_url: "https://api.openai.com/v1",
        api_format: ApiFormat::Openai,
        default_model: "gpt-4o",
        env_var: "OPENAI_API_KEY",
        models_path: "/models",
    },
    ProviderTemplate {
        name: "deepseek",
        display: "DeepSeek",
        base_url: "https://api.deepseek.com/v1",
        api_format: ApiFormat::Openai,
        default_model: "deepseek-chat",
        env_var: "DEEPSEEK_API_KEY",
        models_path: "/models",
    },
    ProviderTemplate {
        name: "openrouter",
        display: "OpenRouter (multi-provider aggregator)",
        base_url: "https://openrouter.ai/api/v1",
        api_format: ApiFormat::Openai,
        default_model: "anthropic/claude-sonnet-4",
        env_var: "OPENROUTER_API_KEY",
        models_path: "/models",
    },
    ProviderTemplate {
        name: "groq",
        display: "Groq (fast inference)",
        base_url: "https://api.groq.com/openai/v1",
        api_format: ApiFormat::Openai,
        default_model: "llama-3.3-70b-versatile",
        env_var: "GROQ_API_KEY",
        models_path: "/models",
    },
    ProviderTemplate {
        name: "custom",
        display: "Custom endpoint (enter URL manually)",
        base_url: "",
        api_format: ApiFormat::Openai,
        default_model: "",
        env_var: "",
        models_path: "",
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
        String::new()
    } else {
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

    // Step 4: Auto-discover models or pick from curated list
    println!();
    let model = select_model(provider, &base_url, &api_key)?;

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

/// Try to fetch available models from the provider's API.
/// Falls back to a curated list if the API call fails.
fn select_model(
    provider: &ProviderTemplate,
    base_url: &str,
    api_key: &str,
) -> anyhow::Result<String> {
    // Try live API fetch first
    let live_models = fetch_models(provider, base_url, api_key);

    match &live_models {
        Ok(models) if !models.is_empty() => {
            println!("✓ Fetched {} available models from API:", models.len());
            print_model_list(models, provider);
        }
        _ => {
            println!("Could not fetch models from API ({}).",
                live_models.as_ref().err().map(|e| e.to_string()).unwrap_or_else(|| "no models returned".into()));
            let curated = curated_models(provider);
            if curated.is_empty() {
                // Last resort: free text input
                return prompt_free_text_model(provider);
            }
            println!("Using curated model list:");
            print_model_list(&curated, provider);
        }
    }

    // Interactive picker
    let curated_vec = curated_models(provider);
    let models = live_models.as_ref().map(|v| v.as_slice()).unwrap_or(curated_vec.as_slice());

    loop {
        print!("Select model [1-{}, or type a custom model name]: ", models.len());
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let trimmed = input.trim();

        if trimmed.is_empty() {
            continue;
        }

        if let Ok(n) = trimmed.parse::<usize>() {
            if n >= 1 && n <= models.len() {
                return Ok(models[n - 1].clone());
            }
        }

        // Free text model name
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
}

fn print_model_list(models: &[String], _provider: &ProviderTemplate) {
    for (i, m) in models.iter().enumerate() {
        let marker = if i == 0 { " ← default" } else { "" };
        println!("  {}. {}{}", i + 1, m, marker);
    }
    println!();
}

fn prompt_free_text_model(provider: &ProviderTemplate) -> anyhow::Result<String> {
    if provider.name == "custom" {
        print!("Enter model name: ");
        io::stdout().flush()?;
        let mut m = String::new();
        io::stdin().read_line(&mut m)?;
        let m = m.trim().to_string();
        if m.is_empty() {
            anyhow::bail!("Model name is required.");
        }
        return Ok(m);
    }
    println!("Default model for {}: {}", provider.display, provider.default_model);
    print!("Press Enter to accept, or type a different model: ");
    io::stdout().flush()?;
    let mut m = String::new();
    io::stdin().read_line(&mut m)?;
    let m = m.trim().to_string();
    if m.is_empty() { Ok(provider.default_model.to_string()) } else { Ok(m) }
}

/// Fetch available models from the provider's /models endpoint.
/// Works with OpenAI-compatible APIs.
fn fetch_models(
    provider: &ProviderTemplate,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<String>, String> {
    if provider.models_path.is_empty() || api_key.is_empty() {
        return Err("no models endpoint for this provider".into());
    }

    let url = format!("{}{}", base_url.trim_end_matches('/'), provider.models_path);
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("http client error: {}", e))?;

    let mut req = client.get(&url);
    match provider.api_format {
        ApiFormat::Openai => {
            req = req.header("Authorization", format!("Bearer {}", api_key));
        }
        ApiFormat::Anthropic => {
            req = req.header("x-api-key", api_key)
                     .header("anthropic-version", "2023-06-01");
        }
    }

    let resp = req.send().map_err(|e| format!("request failed: {}", e))?;
    let body: serde_json::Value = resp.json().map_err(|e| format!("parse failed: {}", e))?;

    let models: Vec<String> = body["data"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m["id"].as_str())
                .map(|s| s.to_string())
                .filter(|id| is_chat_model(id))
                .collect()
        })
        .unwrap_or_default();

    if models.is_empty() {
        return Err("no models in API response".into());
    }

    // Sort: put the default model first, then sort the rest
    let mut sorted = models;
    if let Some(pos) = sorted.iter().position(|m| m == provider.default_model) {
        let default = sorted.remove(pos);
        sorted.sort();
        sorted.insert(0, default);
    } else {
        sorted.sort();
    }

    Ok(sorted)
}

/// Filter to likely chat-capable models (filter out embedding, tts, dall-e, etc.)
fn is_chat_model(id: &str) -> bool {
    let lower = id.to_lowercase();
    // Exclude known non-chat model types
    if lower.contains("embedding") || lower.contains("tts") || lower.contains("dall-e")
        || lower.contains("whisper") || lower.contains("moderation")
        || lower.contains("babbage") || lower.contains("davinci")
        || lower.contains("audio") || lower.contains("vision")
    {
        return false;
    }
    // Must look like a chat model
    lower.contains("gpt") || lower.contains("claude") || lower.contains("o1") || lower.contains("o3")
        || lower.contains("o4") || lower.contains("deepseek") || lower.contains("llama")
        || lower.contains("mixtral") || lower.contains("gemma") || lower.contains("mistral")
        || lower.contains("qwen") || lower.contains("command") || lower.contains("gemini")
        || lower.contains("sonnet") || lower.contains("opus") || lower.contains("haiku")
        || lower.contains("fable") || lower.contains("nova") || lower.contains("yi-")
        || lower.contains("dbrx") || lower.contains("reka") || lower.contains("r1")
        || true // if no filter matches, still include it (custom providers may have unknown names)
}

/// Curated model lists per provider, used when API fetch fails.
fn curated_models(provider: &ProviderTemplate) -> Vec<String> {
    match provider.name {
        "anthropic" => vec![
            "claude-sonnet-4-6".into(),
            "claude-opus-4-8".into(),
            "claude-haiku-4-5".into(),
            "claude-fable-5".into(),
        ],
        "openai" => vec![
            "gpt-4o".into(),
            "gpt-4o-mini".into(),
            "gpt-4.1".into(),
            "o4-mini".into(),
            "o3".into(),
        ],
        "deepseek" => vec![
            "deepseek-chat".into(),
            "deepseek-reasoner".into(),
        ],
        "openrouter" => vec![
            "anthropic/claude-sonnet-4".into(),
            "anthropic/claude-opus-4".into(),
            "openai/gpt-4o".into(),
            "google/gemini-2.5-pro".into(),
            "meta-llama/llama-4-maverick".into(),
            "deepseek/deepseek-chat".into(),
            "qwen/qwen3-235b".into(),
        ],
        "groq" => vec![
            "llama-3.3-70b-versatile".into(),
            "llama-4-maverick-17b".into(),
            "deepseek-r1-distill-llama-70b".into(),
            "mixtral-8x7b-32768".into(),
            "gemma2-9b-it".into(),
        ],
        _ => vec![],
    }
}
