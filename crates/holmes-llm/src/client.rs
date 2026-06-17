use anyhow::{Context, Result};
use holmes_core::config::{ApiFormat, Config, RoleAssignment};
use holmes_core::{LlmResponse, Message, ToolDefinition};
use reqwest::Client as HttpClient;
use std::time::Duration;
use tracing::{debug, error, info, warn};

use crate::anthropic::{AnthropicRequest, AnthropicResponse};
use crate::error_classifier::ClassifiedError;
use crate::provider::{FailoverChain, ProviderState};
use crate::rate_limiter::RateLimiter;
use crate::request::ChatCompletionRequest;
use crate::response::ChatCompletionResponse;

fn safe_truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

pub struct LlmClient {
    http: HttpClient,
    failover: FailoverChain,
    rate_limiter: RateLimiter,
    roles: RoleAssignment,
}

impl LlmClient {
    pub fn new(config: &Config) -> Self {
        let http = HttpClient::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .expect("failed to build HTTP client");

        let failover = FailoverChain::new(config.llm.providers.clone());

        let mut rate_limiter = RateLimiter::new();
        for provider in &config.llm.providers {
            if provider.rpm_limit > 0 {
                rate_limiter.register(provider.name.clone(), provider.rpm_limit, 1);
            }
        }

        Self {
            http,
            failover,
            rate_limiter,
            roles: config.llm.roles.clone(),
        }
    }

    pub async fn chat_completion(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        role: &str,
    ) -> Result<LlmResponse> {
        let role_provider = self.role_provider_name(role);
        let provider = self
            .failover
            .select_for_role(&role_provider)
            .context("no healthy LLM provider available")?;

        self.call_provider(provider, messages, tools).await
    }

    pub async fn chat_completion_oneshot(
        &self,
        system: &str,
        user: &str,
        role: &str,
    ) -> Result<LlmResponse> {
        let messages = vec![Message::system(system), Message::user(user)];
        self.chat_completion(&messages, &[], role).await
    }

    async fn call_provider(
        &self,
        provider: &ProviderState,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<LlmResponse> {
        let _permit = self.rate_limiter.acquire(&provider.config.name).await;

        let is_anthropic = provider.config.api_format == ApiFormat::Anthropic;

        let (url, request_body) = if is_anthropic {
            let url = format!(
                "{}/v1/messages",
                provider.config.base_url.trim_end_matches('/')
            );
            let req = AnthropicRequest::from_messages(&provider.config.model, messages, tools);
            let body = serde_json::to_value(&req).context("serializing Anthropic request")?;
            (url, body)
        } else {
            let url = format!(
                "{}/chat/completions",
                provider.config.base_url.trim_end_matches('/')
            );
            let req = ChatCompletionRequest::new(&provider.config.model, messages, tools);
            let body = serde_json::to_value(&req).context("serializing OpenAI request")?;
            (url, body)
        };

        debug!(provider = %provider.config.name, model = %provider.config.model, format = ?provider.config.api_format, "LLM request");

        let mut last_error = None;
        let max_retries = provider.config.max_retries;

        for attempt in 0..=max_retries {
            if attempt > 0 {
                let backoff = Duration::from_millis(500 * 2u64.pow(attempt as u32 - 1));
                debug!(attempt, backoff_ms = backoff.as_millis() as u64, "retrying");
                tokio::time::sleep(backoff).await;
            }

            let mut req_builder = self
                .http
                .post(&url)
                .header("Content-Type", "application/json");

            if is_anthropic {
                req_builder = req_builder
                    .header("x-api-key", &provider.config.api_key)
                    .header("anthropic-version", "2023-06-01");
            } else {
                req_builder = req_builder.header(
                    "Authorization",
                    format!("Bearer {}", provider.config.api_key),
                );
            }

            let result = req_builder.json(&request_body).send().await;

            match result {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    if status == 200 {
                        let body = resp.text().await.context("reading response body")?;

                        let parse_result = if is_anthropic {
                            serde_json::from_str::<AnthropicResponse>(&body)
                                .map(|parsed| parsed.into_llm_response())
                                .map_err(|e| {
                                    format!(
                                        "parsing Anthropic response: {} — body: {}",
                                        e,
                                        safe_truncate(&body, 200)
                                    )
                                })
                        } else {
                            serde_json::from_str::<ChatCompletionResponse>(&body)
                                .map_err(|e| {
                                    format!(
                                        "parsing LLM response: {} — body: {}",
                                        e,
                                        safe_truncate(&body, 200)
                                    )
                                })
                                .and_then(|parsed| {
                                    parsed
                                        .into_llm_response()
                                        .ok_or_else(|| "empty choices in LLM response".to_string())
                                })
                        };

                        match parse_result {
                            Ok(llm_response) => {
                                provider.record_success();

                                if let Some(usage) = &llm_response.usage {
                                    debug!(
                                        prompt_tokens = usage.prompt_tokens,
                                        completion_tokens = usage.completion_tokens,
                                        "LLM usage"
                                    );
                                }

                                return Ok(llm_response);
                            }
                            Err(parse_err) => {
                                // JSON parse failure on a 200 response — retryable
                                warn!(
                                    provider = %provider.config.name,
                                    attempt,
                                    error = %parse_err,
                                    "response parse failed, retrying"
                                );
                                last_error = Some(parse_err);
                                continue;
                            }
                        }
                    }

                    let body = resp.text().await.unwrap_or_default();
                    let classified = ClassifiedError::from_status_and_body(status, &body);

                    if classified.should_compress {
                        provider.record_failure().await;
                        anyhow::bail!("context overflow: {}", classified.message);
                    }

                    if !classified.retryable {
                        provider.record_failure().await;
                        if classified.should_fallback && self.failover.any_healthy() {
                            warn!(
                                provider = %provider.config.name,
                                status,
                                reason = ?classified.reason,
                                "non-retryable error, attempting failover"
                            );
                            if let Some(next) = self.failover.select() {
                                if next.config.name != provider.config.name {
                                    return Box::pin(self.call_provider(next, messages, tools))
                                        .await;
                                }
                            }
                        }
                        anyhow::bail!(
                            "LLM error ({}): {} {}",
                            provider.config.name,
                            status,
                            classified.message
                        );
                    }

                    warn!(
                        provider = %provider.config.name,
                        status,
                        attempt,
                        reason = ?classified.reason,
                        "retryable error"
                    );
                    last_error = Some(format!("{} {}", status, classified.message));
                    provider.record_failure().await;
                }
                Err(e) => {
                    if e.is_timeout() {
                        warn!(provider = %provider.config.name, attempt, "request timeout");
                        last_error = Some("timeout".into());
                        provider.record_failure().await;
                    } else {
                        error!(provider = %provider.config.name, error = %e, "connection error");
                        provider.record_failure().await;
                        last_error = Some(e.to_string());
                    }
                }
            }
        }

        if self.failover.any_healthy() {
            if let Some(next) = self.failover.select() {
                if next.config.name != provider.config.name {
                    info!(
                        from = %provider.config.name,
                        to = %next.config.name,
                        "failover after exhausting retries"
                    );
                    return Box::pin(self.call_provider(next, messages, tools)).await;
                }
            }
        }

        anyhow::bail!(
            "LLM request failed after {} retries: {}",
            max_retries,
            last_error.unwrap_or_else(|| "unknown error".into())
        )
    }

    fn role_provider_name(&self, role: &str) -> String {
        match role {
            "attack_agent" => self.roles.attack_agent.clone(),
            "supervisor" => self.roles.supervisor.clone(),
            "compressor" => self.roles.compressor.clone(),
            "skill_evolver" => self.roles.skill_evolver.clone(),
            _ => self.roles.attack_agent.clone(),
        }
    }
}
