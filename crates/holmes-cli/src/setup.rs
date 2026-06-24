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
    models_path: &'static str, // Preferred API path for listing models.
}

const PROVIDERS: &[ProviderTemplate] = &[
    ProviderTemplate {
        name: "anthropic",
        display: "Anthropic (Claude)",
        base_url: "https://api.anthropic.com",
        api_format: ApiFormat::Anthropic,
        default_model: "claude-sonnet-4-6",
        env_var: "ANTHROPIC_API_KEY",
        models_path: "/v1/models",
    },
    ProviderTemplate {
        name: "custom",
        display: "Custom Anthropic-compatible endpoint",
        base_url: "",
        api_format: ApiFormat::Anthropic,
        default_model: "",
        env_var: "HOLMES_API_KEY",
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
    println!("Choose an Anthropic-native provider or configure an Anthropic-compatible endpoint.");
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
    let api_key = if let Ok(key) = std::env::var(provider.env_var) {
        if !key.is_empty() {
            println!(
                "✓ Found {} in environment ({}...)",
                provider.env_var,
                &key[..8.min(key.len())]
            );
            key
        } else {
            prompt_api_key(provider)?
        }
    } else {
        prompt_api_key(provider)?
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
    println!(
        "Set environment variable {} or enter it now.",
        provider.env_var
    );
    print!("API Key (input hidden): ");
    io::stdout().flush()?;

    let key = rpassword::read_password()?;
    if key.trim().is_empty() {
        anyhow::bail!(
            "API key is required. Set {} and re-run setup.",
            provider.env_var
        );
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
            let error = live_models
                .as_ref()
                .err()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "no models returned".into());
            if looks_like_auth_failure(&error) {
                anyhow::bail!(
                    "Could not fetch models because authentication failed. Check the API key and rerun setup.\n{}",
                    error
                );
            }
            println!("Could not fetch models from API ({}).", error);
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
    let models = live_models
        .as_ref()
        .map(|v| v.as_slice())
        .unwrap_or(curated_vec.as_slice());

    loop {
        print!(
            "Select model [1-{}, or type a custom model name]: ",
            models.len()
        );
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

fn looks_like_auth_failure(error: &str) -> bool {
    error.contains("HTTP 401")
        || error.contains("HTTP 403")
        || error.to_lowercase().contains("authentication")
        || error.to_lowercase().contains("invalid api key")
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
    println!(
        "Default model for {}: {}",
        provider.display, provider.default_model
    );
    print!("Press Enter to accept, or type a different model: ");
    io::stdout().flush()?;
    let mut m = String::new();
    io::stdin().read_line(&mut m)?;
    let m = m.trim().to_string();
    if m.is_empty() {
        Ok(provider.default_model.to_string())
    } else {
        Ok(m)
    }
}

/// Fetch available models from a provider model-list endpoint when one exists.
fn fetch_models(
    provider: &ProviderTemplate,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<String>, String> {
    if api_key.is_empty() {
        return Err("API key is required before fetching models".into());
    }

    let urls = model_list_urls(provider, base_url);
    if urls.is_empty() {
        return Err("no model-list endpoint candidates for this provider".into());
    }

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("http client error: {}", e))?;

    let mut errors = Vec::new();
    for url in urls {
        let mut req = client.get(&url);
        match provider.api_format {
            ApiFormat::Openai => {
                req = req.header("Authorization", format!("Bearer {}", api_key));
            }
            ApiFormat::Anthropic => {
                req = req
                    .header("x-api-key", api_key)
                    .header("anthropic-version", "2023-06-01");
            }
        }

        let resp = match req.send() {
            Ok(resp) => resp,
            Err(error) => {
                errors.push(format!("{url}: request failed: {error}"));
                continue;
            }
        };
        let status = resp.status();
        let text = match resp.text() {
            Ok(text) => text,
            Err(error) => {
                errors.push(format!("{url}: failed to read response: {error}"));
                continue;
            }
        };

        if !status.is_success() {
            errors.push(format!(
                "{}: HTTP {} {}",
                url,
                status.as_u16(),
                truncate_for_setup(&text, 180)
            ));
            continue;
        }

        let body: serde_json::Value = match serde_json::from_str(&text) {
            Ok(body) => body,
            Err(error) => {
                errors.push(format!(
                    "{}: parse failed: {}; body: {}",
                    url,
                    error,
                    truncate_for_setup(&text, 180)
                ));
                continue;
            }
        };

        let models = extract_model_ids(&body);
        if models.is_empty() {
            errors.push(format!("{url}: no models in API response"));
            continue;
        }

        return Ok(sort_models(models, provider.default_model));
    }

    Err(errors.join("; "))
}

fn model_list_urls(provider: &ProviderTemplate, base_url: &str) -> Vec<String> {
    let base = base_url.trim_end_matches('/');
    let mut urls = Vec::new();

    if !provider.models_path.is_empty() {
        push_unique(&mut urls, join_model_path(base, provider.models_path));
    }

    if base.ends_with("/v1") {
        push_unique(&mut urls, format!("{base}/models"));
    } else {
        push_unique(&mut urls, format!("{base}/v1/models"));
        push_unique(&mut urls, format!("{base}/models"));
    }

    if let Some(parent) = parent_base_url(base) {
        push_unique(&mut urls, format!("{parent}/v1/models"));
        push_unique(&mut urls, format!("{parent}/models"));
    }

    urls
}

fn join_model_path(base: &str, path: &str) -> String {
    let path = path.trim_start_matches('/');
    if base.ends_with("/v1") && path.starts_with("v1/") {
        format!("{base}/{}", path.trim_start_matches("v1/"))
    } else {
        format!("{base}/{path}")
    }
}

fn parent_base_url(base: &str) -> Option<&str> {
    let (_, after_scheme) = base.split_once("://")?;
    after_scheme.find('/')?;
    base.rsplit_once('/').map(|(parent, _)| parent)
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.iter().any(|existing| existing == &value) {
        values.push(value);
    }
}

fn extract_model_ids(body: &serde_json::Value) -> Vec<String> {
    let arrays = [
        body.get("data"),
        body.get("models"),
        body.as_array().map(|_| body),
    ];

    let mut models = Vec::new();
    for array in arrays
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_array())
    {
        for item in array {
            if let Some(id) = item.as_str() {
                models.push(id.to_string());
                continue;
            }
            for key in ["id", "name", "model"] {
                if let Some(id) = item.get(key).and_then(|value| value.as_str()) {
                    models.push(id.to_string());
                    break;
                }
            }
        }
    }

    models
        .into_iter()
        .filter(|id| !id.trim().is_empty())
        .filter(|id| is_chat_model(id))
        .collect()
}

fn sort_models(models: Vec<String>, default_model: &str) -> Vec<String> {
    let mut sorted = models;
    sorted.sort();
    sorted.dedup();
    if !default_model.is_empty() {
        if let Some(pos) = sorted.iter().position(|m| m == default_model) {
            let default = sorted.remove(pos);
            sorted.insert(0, default);
        }
    }
    sorted
}

fn truncate_for_setup(content: &str, max_bytes: usize) -> &str {
    if content.len() <= max_bytes {
        return content;
    }
    let mut end = max_bytes;
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    &content[..end]
}

/// Filter to likely chat-capable models (filter out embedding, tts, dall-e, etc.)
fn is_chat_model(id: &str) -> bool {
    let lower = id.to_lowercase();
    // Exclude known non-chat model types
    if lower.contains("embedding")
        || lower.contains("tts")
        || lower.contains("dall-e")
        || lower.contains("whisper")
        || lower.contains("moderation")
        || lower.contains("babbage")
        || lower.contains("davinci")
        || lower.contains("audio")
        || lower.contains("vision")
    {
        return false;
    }
    // Must look like a chat model
    lower.contains("gpt")
        || lower.contains("claude")
        || lower.contains("o1")
        || lower.contains("o3")
        || lower.contains("o4")
        || lower.contains("deepseek")
        || lower.contains("llama")
        || lower.contains("mixtral")
        || lower.contains("gemma")
        || lower.contains("mistral")
        || lower.contains("qwen")
        || lower.contains("command")
        || lower.contains("gemini")
        || lower.contains("sonnet")
        || lower.contains("opus")
        || lower.contains("haiku")
        || lower.contains("fable")
        || lower.contains("nova")
        || lower.contains("yi-")
        || lower.contains("dbrx")
        || lower.contains("reka")
        || lower.contains("r1")
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
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_provider_requires_api_key_env_name() {
        let custom = PROVIDERS
            .iter()
            .find(|provider| provider.name == "custom")
            .expect("custom provider");

        assert_eq!(custom.api_format, ApiFormat::Anthropic);
        assert_eq!(custom.env_var, "HOLMES_API_KEY");
    }

    #[test]
    fn anthropic_provider_has_model_list_endpoint() {
        let anthropic = PROVIDERS
            .iter()
            .find(|provider| provider.name == "anthropic")
            .expect("anthropic provider");

        assert_eq!(
            model_list_urls(anthropic, "https://api.anthropic.com"),
            vec![
                "https://api.anthropic.com/v1/models".to_string(),
                "https://api.anthropic.com/models".to_string(),
            ]
        );
    }

    #[test]
    fn custom_provider_tries_nested_and_parent_model_endpoints() {
        let custom = PROVIDERS
            .iter()
            .find(|provider| provider.name == "custom")
            .expect("custom provider");

        assert_eq!(
            model_list_urls(custom, "https://api.deepseek.com/anthropic"),
            vec![
                "https://api.deepseek.com/anthropic/v1/models".to_string(),
                "https://api.deepseek.com/anthropic/models".to_string(),
                "https://api.deepseek.com/v1/models".to_string(),
                "https://api.deepseek.com/models".to_string(),
            ]
        );
    }

    #[test]
    fn extracts_models_from_common_response_shapes() {
        let body = serde_json::json!({
            "data": [
                {"id": "claude-sonnet-4-6"},
                {"name": "deepseek-v4-pro"},
                "custom-model"
            ]
        });

        assert_eq!(
            sort_models(extract_model_ids(&body), "deepseek-v4-pro"),
            vec![
                "deepseek-v4-pro".to_string(),
                "claude-sonnet-4-6".to_string(),
                "custom-model".to_string(),
            ]
        );
    }

    #[test]
    fn auth_errors_are_not_silently_accepted_during_setup() {
        assert!(looks_like_auth_failure(
            "https://api.example.test/v1/models: HTTP 401 invalid api key"
        ));
        assert!(looks_like_auth_failure("Authentication Fails"));
        assert!(!looks_like_auth_failure(
            "https://api.example.test/v1/models: HTTP 404 not found"
        ));
    }
}
