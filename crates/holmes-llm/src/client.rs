use anyhow::{anyhow, Result};
use holmes_core::config::{Config, RoleAssignment};
use holmes_core::{LlmResponse, Message, ToolDefinition};
use reqwest::Client as HttpClient;
use std::collections::HashSet;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use crate::anthropic::{AnthropicRequest, AnthropicResponse};
use crate::error_classifier::{parse_retry_after, ClassifiedError, FailureClass};
use crate::provider::{FailoverChain, ProviderState};
use crate::rate_limiter::RateLimiter;

use holmes_core::truncate_str as safe_truncate;

fn anthropic_messages_url(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    }
}

pub struct LlmClient {
    http: HttpClient,
    failover: FailoverChain,
    rate_limiter: RateLimiter,
    roles: RoleAssignment,
    stream: bool,
    thinking_budget: u32,
    call_deadline: Duration,
}

impl LlmClient {
    pub fn new(config: &Config) -> Self {
        let http = HttpClient::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .expect("failed to build HTTP client");

        let failover = FailoverChain::new(
            config.llm.providers.clone(),
            Duration::from_millis(config.llm.provider_cooldown_base_ms),
            Duration::from_millis(config.llm.provider_cooldown_max_ms),
        );

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
            stream: config.llm.stream,
            thinking_budget: config.llm.thinking_budget,
            call_deadline: Duration::from_millis(config.llm.call_deadline_ms),
        }
    }

    /// Handle on the provider chain, for health assertions in integration tests.
    #[doc(hidden)]
    pub fn failover_chain(&self) -> &FailoverChain {
        &self.failover
    }

    pub async fn chat_completion(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        role: &str,
    ) -> Result<LlmResponse> {
        self.chat_completion_streaming(messages, tools, role, &mut |_| {})
            .await
    }

    /// Like `chat_completion`, but invokes `on_text` with the assistant text of the
    /// *winning* provider attempt (only meaningful when `llm.stream` is enabled;
    /// otherwise the callback simply never fires and the buffered path is used).
    ///
    /// Deltas are buffered per attempt and committed to `on_text` only once the
    /// attempt has fully succeeded — a failed attempt that triggers failover never
    /// leaks its partial output to the callback, so displayed text always matches
    /// the authoritative response.
    ///
    /// One call drives the failover state machine:
    /// - each provider is attempted at most once (`attempted` set — a failed provider is
    ///   never re-selected within the same call);
    /// - a terminal transient failure cools the provider down and fails over to the next
    ///   selectable one (highest priority, or the role's provider when selectable);
    /// - when every remaining provider is cooling, the call waits for the nearest
    ///   half-open probe point — bounded by `llm.call_deadline_ms`, after which the call
    ///   fails deterministically;
    /// - the absolute call deadline covers the WHOLE call lifecycle (P1-01): provider
    ///   selection, half-open backoff sleeps, rate-limiter queueing, connect, read and
    ///   streaming — queueing time is charged against the same budget as the request;
    /// - request-content errors (400, context overflow) propagate immediately without
    ///   touching provider health; provider config errors (401/403/billing/unknown
    ///   model) disable the provider for the rest of the process lifetime;
    /// - caller cancellation simply drops the future — the in-flight HTTP request is
    ///   aborted on drop and nothing is recorded.
    pub async fn chat_completion_streaming(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        role: &str,
        on_text: &mut (dyn FnMut(&str) + Send),
    ) -> Result<LlmResponse> {
        let role_provider = self.role_provider_name(role);
        let deadline = Instant::now() + self.call_deadline;
        let mut attempted: HashSet<String> = HashSet::new();
        let mut last_error: Option<String> = None;
        // Provider used by the previous attempt in this call; reported as `from`
        // in failover events.
        let mut previous_provider: Option<String> = None;

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                warn!(
                    attempted = ?attempted,
                    last_error = last_error.as_deref().unwrap_or("none"),
                    event = "LlmCallDeadlineExceeded",
                    "LLM call deadline exceeded"
                );
                break;
            }

            let Some(provider) = self.failover.select_for_role(&role_provider, &attempted) else {
                match self.failover.next_probe_at(&attempted) {
                    Some(wake) => {
                        let wait = wake.saturating_duration_since(Instant::now());
                        if wait >= remaining {
                            warn!(
                                wait_ms = wait.as_millis() as u64,
                                remaining_ms = remaining.as_millis() as u64,
                                event = "LlmCallDeadlineExceeded",
                                "all LLM providers cooling; recovery past call deadline"
                            );
                            break;
                        }
                        info!(
                            wait_ms = wait.as_millis() as u64,
                            event = "LlmAwaitingHalfOpen",
                            "all LLM providers attempted or cooling; waiting for half-open probe"
                        );
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    None => break,
                }
            };

            attempted.insert(provider.config.name.clone());
            if attempted.len() > 1 {
                holmes_core::metrics::metrics().count("llm.failover.started");
                info!(
                    from = previous_provider.as_deref().unwrap_or("none"),
                    to = %provider.config.name,
                    attempted = ?attempted,
                    event = "ProviderFailoverStarted",
                    "failing over to next LLM provider"
                );
            }

            match self
                .attempt_provider(provider, messages, tools, &mut *on_text, remaining)
                .await
            {
                Ok(llm_response) => {
                    provider.record_success();
                    holmes_core::metrics::metrics().count("llm.call.success");
                    if attempted.len() == 1 {
                        holmes_core::metrics::metrics().count("llm.call.first_try_success");
                    } else {
                        holmes_core::metrics::metrics().count("llm.failover.completed");
                        info!(
                            provider = %provider.config.name,
                            attempts = attempted.len(),
                            event = "ProviderFailoverCompleted",
                            "LLM call succeeded after failover"
                        );
                    }
                    if let Some(usage) = &llm_response.usage {
                        debug!(
                            prompt_tokens = usage.prompt_tokens,
                            completion_tokens = usage.completion_tokens,
                            "LLM usage"
                        );
                    }
                    return Ok(llm_response);
                }
                Err(classified) => {
                    let class = classified.failure_class();
                    holmes_core::metrics::metrics().count("llm.provider_attempt_failed");
                    warn!(
                        provider = %provider.config.name,
                        reason = ?classified.reason,
                        status = classified.status_code,
                        class = ?class,
                        event = "ProviderAttemptFailed",
                        "LLM provider attempt failed"
                    );
                    last_error = Some(format!("{}: {}", provider.config.name, classified.message));
                    match class {
                        // The request content itself is rejected: failover is pointless
                        // and provider health is unaffected — propagate directly.
                        FailureClass::RequestContent => {
                            return Err(anyhow!(
                                "LLM error ({}): {}{}",
                                provider.config.name,
                                classified
                                    .status_code
                                    .map(|s| format!("{s} "))
                                    .unwrap_or_default(),
                                classified.message
                            ));
                        }
                        FailureClass::ProviderConfig => {
                            provider.record_failure(class, classified.reason, None);
                        }
                        FailureClass::Transient => {
                            provider.record_failure(
                                class,
                                classified.reason,
                                classified.retry_after,
                            );
                        }
                    }
                    previous_provider = Some(provider.config.name.clone());
                }
            }
        }

        holmes_core::metrics::metrics().count("llm.call.failed");
        if attempted.len() > 1 {
            holmes_core::metrics::metrics().count("llm.failover.failed");
            warn!(
                attempted = ?attempted,
                last_error = last_error.as_deref().unwrap_or("none"),
                event = "ProviderFailoverFailed",
                "LLM call failed after exhausting provider failover"
            );
        }
        Err(anyhow!(
            "no healthy LLM provider available: {}",
            last_error.unwrap_or_else(|| "no selectable provider".into())
        ))
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

    /// One attempt against one provider: queue at the rate limiter, build the
    /// request, send it and classify the outcome. The buffered and streaming (SSE)
    /// wire paths share this classification. The WHOLE attempt — rate-limiter
    /// queueing included — is bounded by `remaining` (the rest of the call
    /// deadline), so a provider whose bucket is empty fails over instead of
    /// parking the call outside the deadline (P1-01).
    async fn attempt_provider(
        &self,
        provider: &ProviderState,
        messages: &[Message],
        tools: &[ToolDefinition],
        on_text: &mut (dyn FnMut(&str) + Send),
        remaining: Duration,
    ) -> Result<LlmResponse, ClassifiedError> {
        let attempt = self.attempt_queued(provider, messages, tools, on_text);
        match tokio::time::timeout(remaining, attempt).await {
            Ok(result) => result,
            Err(_) => Err(ClassifiedError::timeout(format!(
                "attempt (incl. rate-limit queue) exceeded call deadline ({}ms remaining)",
                remaining.as_millis()
            ))),
        }
    }

    /// The body of one provider attempt, run under the caller's `remaining` budget.
    async fn attempt_queued(
        &self,
        provider: &ProviderState,
        messages: &[Message],
        tools: &[ToolDefinition],
        on_text: &mut (dyn FnMut(&str) + Send),
    ) -> Result<LlmResponse, ClassifiedError> {
        let _permit = self.rate_limiter.acquire(&provider.config.name).await;

        let url = anthropic_messages_url(&provider.config.base_url);
        let req = AnthropicRequest::from_messages(&provider.config.model, messages, tools);
        let mut request_body = serde_json::to_value(&req).map_err(|e| {
            ClassifiedError::from_sse_error(
                "invalid_request_error",
                format!("serializing Anthropic request: {e}"),
            )
        })?;
        if self.stream {
            // Opt-in SSE streaming wire path (buffered path is unchanged when off).
            request_body["stream"] = serde_json::Value::Bool(true);
        }
        if self.thinking_budget > 0 {
            // Enable extended thinking; ensure max_tokens exceeds the thinking budget.
            request_body["thinking"] = serde_json::json!({
                "type": "enabled",
                "budget_tokens": self.thinking_budget,
            });
            let need = self.thinking_budget + 4096;
            if request_body["max_tokens"].as_u64().unwrap_or(0) < need as u64 {
                request_body["max_tokens"] = serde_json::json!(need);
            }
            // Extended thinking requires temperature unset.
            if let Some(obj) = request_body.as_object_mut() {
                obj.remove("temperature");
            }
        }

        debug!(provider = %provider.config.name, model = %provider.config.model, configured_format = ?provider.config.api_format, wire_protocol = "anthropic", "LLM request");

        self.execute_attempt(provider, &url, &request_body, on_text)
            .await
    }

    async fn execute_attempt(
        &self,
        provider: &ProviderState,
        url: &str,
        request_body: &serde_json::Value,
        on_text: &mut (dyn FnMut(&str) + Send),
    ) -> Result<LlmResponse, ClassifiedError> {
        let result = self
            .http
            .post(url)
            .header("Content-Type", "application/json")
            .header("x-api-key", &provider.config.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(request_body)
            .send()
            .await;

        let resp = match result {
            Ok(resp) => resp,
            Err(e) if e.is_timeout() => {
                return Err(ClassifiedError::timeout(e.to_string()));
            }
            Err(e) => {
                return Err(ClassifiedError::connection(e.to_string()));
            }
        };

        let status = resp.status().as_u16();
        if status != 200 {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(parse_retry_after);
            let body = resp.text().await.unwrap_or_default();
            return Err(
                ClassifiedError::from_status_and_body(status, &body).with_retry_after(retry_after)
            );
        }

        if !self.stream {
            let body = resp
                .text()
                .await
                .map_err(|e| ClassifiedError::stream_interrupted(format!("reading body: {e}")))?;
            return serde_json::from_str::<AnthropicResponse>(&body)
                .map(|parsed| parsed.into_llm_response())
                .map_err(|e| {
                    ClassifiedError::invalid_response(format!(
                        "parsing Anthropic response: {} — body: {}",
                        e,
                        safe_truncate(&body, 200)
                    ))
                });
        }

        // Read the SSE body incrementally, accumulating text deltas into a
        // per-attempt buffer. Deltas are committed to `on_text` only after the
        // attempt is fully validated (stop reason present, no terminal error
        // frame): if the stream breaks mid-flight and the call fails over to
        // another provider, the dead attempt's partial output never reaches
        // the UI, so what was displayed always matches the authoritative
        // response. The full body is still accumulated to build the
        // authoritative LlmResponse (tool calls, stop reason, usage).
        use futures::StreamExt;
        let mut stream = resp.bytes_stream();
        let mut body = String::new();
        let mut pending = String::new();
        let mut deltas = String::new();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    let s = String::from_utf8_lossy(&bytes);
                    body.push_str(&s);
                    pending.push_str(&s);
                    while let Some(nl) = pending.find('\n') {
                        let line: String = pending.drain(..=nl).collect();
                        if let Some(t) = crate::anthropic::sse_line_text_delta(&line) {
                            deltas.push_str(&t);
                        }
                    }
                }
                Err(e) => {
                    return Err(ClassifiedError::stream_interrupted(format!(
                        "stream read error: {e}"
                    )));
                }
            }
        }
        if !pending.trim().is_empty() {
            if let Some(t) = crate::anthropic::sse_line_text_delta(&pending) {
                deltas.push_str(&t);
            }
        }

        // A terminal `error` event frame mid-stream is a classified provider failure,
        // not a successful completion.
        if let Some((error_type, message)) = crate::anthropic::sse_error_event(&body) {
            return Err(ClassifiedError::from_sse_error(&error_type, message));
        }

        // The stream must end with a stop reason (`message_delta`); a body without one
        // was truncated (connection dropped, gateway EOF) — a transient failure.
        let llm_response = crate::anthropic::parse_sse_response(&body, &mut |_| {});
        if llm_response.finish_reason.is_none() {
            return Err(ClassifiedError::stream_interrupted(
                "SSE stream ended without a stop reason (truncated response)",
            ));
        }
        // The attempt succeeded: commit this attempt's buffered deltas to the UI.
        if !deltas.is_empty() {
            on_text(&deltas);
        }
        Ok(llm_response)
    }

    fn role_provider_name(&self, role: &str) -> String {
        match role {
            "attack_agent" => self.roles.attack_agent.clone(),
            "supervisor" => self.roles.supervisor.clone(),
            "compressor" => self.roles.compressor.clone(),
            "skill_evolver" => self.roles.skill_evolver.clone(),
            // The completion verifier is an independent audit boundary (P0-02): it
            // routes through its own provider mapping instead of silently falling
            // back to the agent's provider. When `goal_evaluator` is unconfigured
            // (empty), provider selection falls back to priority order.
            "goal_evaluator" => self.roles.goal_evaluator.clone(),
            _ => self.roles.attack_agent.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::anthropic_messages_url;

    #[test]
    fn anthropic_url_does_not_duplicate_v1_segment() {
        assert_eq!(
            anthropic_messages_url("https://api.anthropic.com"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            anthropic_messages_url("https://gateway.example.test/v1"),
            "https://gateway.example.test/v1/messages"
        );
    }
}
