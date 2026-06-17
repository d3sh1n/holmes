use holmes_core::config::ProviderConfig;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Instant;
use tokio::sync::Mutex;
use tracing::{info, warn};

pub struct ProviderState {
    pub config: ProviderConfig,
    consecutive_failures: AtomicU32,
    healthy: AtomicBool,
    last_failure: Mutex<Option<Instant>>,
}

impl ProviderState {
    pub fn new(config: ProviderConfig) -> Self {
        Self {
            config,
            consecutive_failures: AtomicU32::new(0),
            healthy: AtomicBool::new(true),
            last_failure: Mutex::new(None),
        }
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    pub fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
        if !self.healthy.load(Ordering::Relaxed) {
            self.healthy.store(true, Ordering::Relaxed);
            info!(provider = %self.config.name, "provider recovered");
        }
    }

    pub async fn record_failure(&self) {
        let count = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
        *self.last_failure.lock().await = Some(Instant::now());
        if count >= self.config.max_retries as u32 {
            self.healthy.store(false, Ordering::Relaxed);
            warn!(provider = %self.config.name, failures = count, "provider marked unhealthy");
        }
    }

    pub fn failure_count(&self) -> u32 {
        self.consecutive_failures.load(Ordering::Relaxed)
    }
}

pub struct FailoverChain {
    providers: Vec<ProviderState>,
}

impl FailoverChain {
    pub fn new(configs: Vec<ProviderConfig>) -> Self {
        let mut configs = configs;
        configs.sort_by_key(|c| c.priority);
        let providers = configs.into_iter().map(ProviderState::new).collect();
        Self { providers }
    }

    pub fn select(&self) -> Option<&ProviderState> {
        self.providers.iter().find(|p| p.is_healthy())
    }

    pub fn select_for_role<'a>(&'a self, role_provider_name: &str) -> Option<&'a ProviderState> {
        self.providers
            .iter()
            .find(|p| p.config.name == role_provider_name && p.is_healthy())
            .or_else(|| self.select())
    }

    pub fn all_providers(&self) -> &[ProviderState] {
        &self.providers
    }

    pub fn any_healthy(&self) -> bool {
        self.providers.iter().any(|p| p.is_healthy())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holmes_core::config::ProviderConfig;

    fn make_provider(name: &str, priority: u8) -> ProviderConfig {
        ProviderConfig {
            name: name.into(),
            base_url: format!("https://{name}.example.com/v1"),
            api_key: "test-key".into(),
            api_key_env: None,
            model: "test-model".into(),
            api_format: Default::default(),
            priority: priority.into(),
            max_retries: 3,
            rpm_limit: 0,
        }
    }

    #[test]
    fn select_returns_highest_priority() {
        let chain = FailoverChain::new(vec![
            make_provider("secondary", 2),
            make_provider("primary", 1),
        ]);
        let selected = chain.select().unwrap();
        assert_eq!(selected.config.name, "primary");
    }

    #[tokio::test]
    async fn failover_on_unhealthy() {
        let chain = FailoverChain::new(vec![
            make_provider("primary", 1),
            make_provider("secondary", 2),
        ]);

        for _ in 0..3 {
            chain.providers[0].record_failure().await;
        }
        assert!(!chain.providers[0].is_healthy());

        let selected = chain.select().unwrap();
        assert_eq!(selected.config.name, "secondary");
    }

    #[test]
    fn recovery_on_success() {
        let state = ProviderState::new(make_provider("test", 1));
        state.consecutive_failures.store(5, Ordering::Relaxed);
        state.healthy.store(false, Ordering::Relaxed);

        state.record_success();
        assert!(state.is_healthy());
        assert_eq!(state.failure_count(), 0);
    }

    #[test]
    fn select_for_role_prefers_named() {
        let chain = FailoverChain::new(vec![
            make_provider("anthropic", 1),
            make_provider("deepseek", 2),
        ]);
        let selected = chain.select_for_role("deepseek").unwrap();
        assert_eq!(selected.config.name, "deepseek");
    }

    #[test]
    fn select_for_role_falls_back() {
        let chain = FailoverChain::new(vec![make_provider("anthropic", 1)]);
        let selected = chain.select_for_role("nonexistent").unwrap();
        assert_eq!(selected.config.name, "anthropic");
    }
}
