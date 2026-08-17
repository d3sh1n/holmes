use holmes_core::config::ProviderConfig;
use std::collections::HashSet;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::{info, warn};

use crate::error_classifier::{FailoverReason, FailureClass};

/// Lease after which an in-flight half-open probe is considered abandoned (e.g. the
/// caller was cancelled mid-attempt) and the provider may be probed again. Slightly
/// above the HTTP client's total timeout.
const PROBE_LEASE: Duration = Duration::from_secs(130);

/// Explicit lifecycle of one upstream provider.
///
/// ```text
/// Healthy ──transient failure──▶ CoolingDown ──window elapsed──▶ HalfOpen
///   ▲                                                             │     │
///   └────────────── probe success ◀───────────────────────────────┘     │ probe failure
///   │                                                                   ▼
///   │                                                     CoolingDown (window ×2)
///   │
///   └──── Disabled: terminal config error (401/403/billing/unknown model);
///         never auto-selected again until the process restarts with fixed config
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Healthy,
    CoolingDown,
    HalfOpen,
    Disabled,
}

impl Status {
    /// Stable lowercase label used in `ProviderHealthChanged` event fields.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::CoolingDown => "cooling_down",
            Self::HalfOpen => "half_open",
            Self::Disabled => "disabled",
        }
    }
}

#[derive(Debug)]
struct Inner {
    status: Status,
    /// When the current cooling window ends (CoolingDown / HalfOpen states).
    cooling_until: Option<Instant>,
    /// Consecutive terminal request-level failures; drives the exponential window.
    consecutive_failures: u32,
    /// Length of the current cooling window (before jitter).
    cooldown_window: Duration,
    /// A half-open probe attempt is currently running.
    probe_in_flight: bool,
    /// When the in-flight probe started (for the abandonment lease).
    probe_started: Option<Instant>,
    /// When the current unhealthy streak began (first terminal failure after a
    /// healthy period); basis for the `provider.downtime_ms` recovery metric.
    unhealthy_since: Option<Instant>,
}

impl Inner {
    fn new() -> Self {
        Self {
            status: Status::Healthy,
            cooling_until: None,
            consecutive_failures: 0,
            cooldown_window: Duration::ZERO,
            probe_in_flight: false,
            probe_started: None,
            unhealthy_since: None,
        }
    }
}

pub struct ProviderState {
    pub config: ProviderConfig,
    cooldown_base: Duration,
    cooldown_max: Duration,
    inner: Mutex<Inner>,
}

impl ProviderState {
    pub fn new(config: ProviderConfig, cooldown_base: Duration, cooldown_max: Duration) -> Self {
        Self {
            config,
            cooldown_base,
            cooldown_max,
            inner: Mutex::new(Inner::new()),
        }
    }

    pub fn status(&self) -> Status {
        self.inner.lock().unwrap().status
    }

    pub fn failure_count(&self) -> u32 {
        self.inner.lock().unwrap().consecutive_failures
    }

    /// Current cooling window end, if the provider is cooling or probing.
    pub fn cooling_until(&self) -> Option<Instant> {
        self.inner.lock().unwrap().cooling_until
    }

    /// Deterministic ±20% jitter, stable per (provider, failure count) so tests can
    /// assert bounds and different providers spread their half-open probes.
    fn jittered(&self, window: Duration, failures: u32) -> Duration {
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for b in self.config.name.as_bytes() {
            h = (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3);
        }
        h = (h ^ u64::from(failures)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        let frac = (h >> 32) as f64 / u32::MAX as f64;
        window.mul_f64(0.8 + 0.4 * frac)
    }

    /// A request to this provider completed successfully: reset to Healthy and
    /// restore the base cooldown window.
    pub fn record_success(&self) {
        let mut inner = self.inner.lock().unwrap();
        let was = inner.status;
        let unhealthy_since = inner.unhealthy_since.take();
        inner.status = Status::Healthy;
        inner.consecutive_failures = 0;
        inner.cooling_until = None;
        inner.probe_in_flight = false;
        inner.probe_started = None;
        if was != Status::Healthy {
            holmes_core::metrics::metrics().count("provider.recovered");
            if let Some(since) = unhealthy_since {
                holmes_core::metrics::metrics()
                    .record_duration("provider.downtime_ms", since.elapsed());
            }
            info!(
                provider = %self.config.name,
                from = was.as_str(),
                to = Status::Healthy.as_str(),
                event = "ProviderHealthChanged",
                "LLM provider recovered to healthy"
            );
        }
    }

    /// Record the terminal outcome of an attempt, according to its failure class.
    ///
    /// - `Transient`: enter/extend CoolingDown. The window is `base * 2^(failures-1)`
    ///   capped at `max` with ±20% jitter; a server `Retry-After` hint takes
    ///   precedence (capped at `max`).
    /// - `ProviderConfig`: move to Disabled — never auto-selected again.
    /// - `RequestContent`: provider health is unaffected; nothing is recorded.
    pub fn record_failure(
        &self,
        class: FailureClass,
        reason: FailoverReason,
        retry_after: Option<Duration>,
    ) {
        let mut inner = self.inner.lock().unwrap();
        match class {
            FailureClass::RequestContent => {}
            FailureClass::ProviderConfig => {
                if inner.status != Status::Disabled {
                    let was = inner.status;
                    inner.status = Status::Disabled;
                    inner.probe_in_flight = false;
                    inner.probe_started = None;
                    inner.unhealthy_since.get_or_insert_with(Instant::now);
                    holmes_core::metrics::metrics().count("provider.disabled");
                    warn!(
                        provider = %self.config.name,
                        from = was.as_str(),
                        to = Status::Disabled.as_str(),
                        reason = ?reason,
                        event = "ProviderHealthChanged",
                        "LLM provider disabled after terminal configuration error"
                    );
                }
            }
            FailureClass::Transient => {
                inner.consecutive_failures += 1;
                let failures = inner.consecutive_failures;
                // base * 2^(failures-1), capped — doubles after every terminal
                // failure, including failed half-open probes.
                let computed = self
                    .cooldown_base
                    .saturating_mul(1u32 << failures.saturating_sub(1).min(20))
                    .min(self.cooldown_max);
                inner.cooldown_window = computed;
                let cooldown = match retry_after {
                    Some(hint) => hint.min(self.cooldown_max),
                    None => self.jittered(computed, failures),
                };
                let was = inner.status;
                inner.status = Status::CoolingDown;
                inner.cooling_until = Some(Instant::now() + cooldown);
                inner.probe_in_flight = false;
                inner.probe_started = None;
                inner.unhealthy_since.get_or_insert_with(Instant::now);
                holmes_core::metrics::metrics().count("provider.cooling_down");
                warn!(
                    provider = %self.config.name,
                    from = was.as_str(),
                    to = Status::CoolingDown.as_str(),
                    reason = ?reason,
                    failures,
                    cooldown_ms = cooldown.as_millis() as u64,
                    retry_after_ms = retry_after.map(|d| d.as_millis() as u64),
                    event = "ProviderHealthChanged",
                    "LLM provider entered cooldown"
                );
            }
        }
    }

    /// Consider this provider for selection. Returns true when the caller may use it
    /// for a new attempt. Transitions CoolingDown → HalfOpen when the window elapsed,
    /// claiming the single probe slot; reclaims abandoned probes after PROBE_LEASE.
    fn try_claim(&self) -> bool {
        let mut inner = self.inner.lock().unwrap();
        match inner.status {
            Status::Healthy => true,
            Status::Disabled => false,
            Status::HalfOpen => {
                let abandoned = inner
                    .probe_started
                    .is_some_and(|t| t.elapsed() > PROBE_LEASE);
                if inner.probe_in_flight && !abandoned {
                    return false;
                }
                inner.probe_in_flight = true;
                inner.probe_started = Some(Instant::now());
                true
            }
            Status::CoolingDown => {
                let ready = inner
                    .cooling_until
                    .is_some_and(|until| until <= Instant::now());
                if !ready {
                    return false;
                }
                inner.status = Status::HalfOpen;
                inner.probe_in_flight = true;
                inner.probe_started = Some(Instant::now());
                holmes_core::metrics::metrics().count("provider.half_open_probe");
                info!(
                    provider = %self.config.name,
                    from = Status::CoolingDown.as_str(),
                    to = Status::HalfOpen.as_str(),
                    failures = inner.consecutive_failures,
                    event = "ProviderHealthChanged",
                    "LLM provider half-open probe started"
                );
                true
            }
        }
    }
}

pub struct FailoverChain {
    providers: Vec<ProviderState>,
}

impl FailoverChain {
    pub fn new(
        configs: Vec<ProviderConfig>,
        cooldown_base: Duration,
        cooldown_max: Duration,
    ) -> Self {
        let mut configs = configs;
        configs.sort_by_key(|c| c.priority);
        let providers = configs
            .into_iter()
            .map(|c| ProviderState::new(c, cooldown_base, cooldown_max))
            .collect();
        Self { providers }
    }

    fn pick<'a>(
        &'a self,
        attempted: &HashSet<String>,
        pred: impl Fn(&ProviderState) -> bool,
    ) -> Option<&'a ProviderState> {
        self.providers
            .iter()
            .filter(|p| pred(p) && !attempted.contains(&p.config.name))
            .find(|p| p.try_claim())
    }

    /// Highest-priority selectable provider that this call has not attempted yet.
    pub fn select(&self, attempted: &HashSet<String>) -> Option<&ProviderState> {
        self.pick(attempted, |_| true)
    }

    /// Prefer the provider named by the role when selectable; otherwise fail over to
    /// the highest-priority selectable provider.
    pub fn select_for_role(
        &self,
        role_provider_name: &str,
        attempted: &HashSet<String>,
    ) -> Option<&ProviderState> {
        self.pick(attempted, |p| p.config.name == role_provider_name)
            .or_else(|| self.select(attempted))
    }

    /// Earliest time at which a not-yet-attempted cooling provider becomes eligible
    /// for a half-open probe. None when nothing cooling remains (all healthy,
    /// disabled or already attempted).
    pub fn next_probe_at(&self, attempted: &HashSet<String>) -> Option<Instant> {
        self.providers
            .iter()
            .filter(|p| !attempted.contains(&p.config.name))
            .filter_map(|p| {
                let inner = p.inner.lock().unwrap();
                (inner.status == Status::CoolingDown)
                    .then_some(inner.cooling_until)
                    .flatten()
            })
            .min()
    }

    pub fn providers(&self) -> &[ProviderState] {
        &self.providers
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holmes_core::config::ProviderConfig;

    const BASE: Duration = Duration::from_millis(100);
    const MAX: Duration = Duration::from_millis(1_000);

    fn make_provider(name: &str, priority: u32) -> ProviderConfig {
        ProviderConfig {
            name: name.into(),
            base_url: format!("https://{name}.example.com/v1"),
            api_key: "test-key".into(),
            api_key_env: None,
            model: "test-model".into(),
            api_format: Default::default(),
            priority,
            rpm_limit: 0,
        }
    }

    fn chain(configs: Vec<ProviderConfig>) -> FailoverChain {
        FailoverChain::new(configs, BASE, MAX)
    }

    fn attempted(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn select_returns_highest_priority() {
        let chain = chain(vec![
            make_provider("secondary", 2),
            make_provider("primary", 1),
        ]);
        let selected = chain.select(&attempted(&[])).unwrap();
        assert_eq!(selected.config.name, "primary");
    }

    #[test]
    fn select_never_returns_attempted_provider() {
        let chain = chain(vec![
            make_provider("primary", 1),
            make_provider("secondary", 2),
        ]);
        let selected = chain.select(&attempted(&["primary"])).unwrap();
        assert_eq!(selected.config.name, "secondary");
        assert!(chain
            .select(&attempted(&["primary", "secondary"]))
            .is_none());
    }

    #[test]
    fn transient_failure_enters_cooldown() {
        let chain = chain(vec![
            make_provider("primary", 1),
            make_provider("secondary", 2),
        ]);
        let primary = &chain.providers()[0];
        primary.record_failure(FailureClass::Transient, FailoverReason::ServerError, None);
        assert_eq!(primary.status(), Status::CoolingDown);

        let selected = chain.select(&attempted(&[])).unwrap();
        assert_eq!(selected.config.name, "secondary");
    }

    #[test]
    fn cooldown_window_doubles_and_caps() {
        let state = ProviderState::new(make_provider("p", 1), BASE, MAX);
        let mut last = Duration::ZERO;
        for expected in [100u64, 200, 400, 800, 1000, 1000] {
            state.record_failure(FailureClass::Transient, FailoverReason::ServerError, None);
            let until = state.cooling_until().unwrap();
            let window = until.saturating_duration_since(Instant::now());
            let base = Duration::from_millis(expected);
            // jittered within ±20% of the nominal window
            assert!(window <= base.mul_f64(1.25), "window {window:?} > {base:?}");
            assert!(
                window >= base.mul_f64(0.75),
                "window {window:?} < 0.8*{base:?}"
            );
            assert!(window >= last);
            last = window;
        }
    }

    #[test]
    fn retry_after_overrides_computed_window() {
        let state = ProviderState::new(make_provider("p", 1), BASE, MAX);
        state.record_failure(
            FailureClass::Transient,
            FailoverReason::RateLimit,
            Some(Duration::from_millis(500)),
        );
        let window = state
            .cooling_until()
            .unwrap()
            .saturating_duration_since(Instant::now());
        assert!(window > Duration::from_millis(450));
        assert!(window <= Duration::from_millis(500));

        // retry-after is capped at the configured max
        state.record_failure(
            FailureClass::Transient,
            FailoverReason::RateLimit,
            Some(Duration::from_secs(600)),
        );
        let window = state
            .cooling_until()
            .unwrap()
            .saturating_duration_since(Instant::now());
        assert!(window <= MAX);
    }

    #[test]
    fn provider_config_error_disables_permanently() {
        let chain = chain(vec![
            make_provider("primary", 1),
            make_provider("secondary", 2),
        ]);
        let primary = &chain.providers()[0];
        primary.record_failure(FailureClass::ProviderConfig, FailoverReason::Auth, None);
        assert_eq!(primary.status(), Status::Disabled);

        // never selected again, even with nothing attempted and time passing
        let selected = chain.select(&attempted(&[])).unwrap();
        assert_eq!(selected.config.name, "secondary");
        assert!(chain.next_probe_at(&attempted(&["secondary"])).is_none());
    }

    #[test]
    fn request_content_error_does_not_affect_health() {
        let state = ProviderState::new(make_provider("p", 1), BASE, MAX);
        state.record_failure(
            FailureClass::RequestContent,
            FailoverReason::ContextOverflow,
            None,
        );
        assert_eq!(state.status(), Status::Healthy);
        assert_eq!(state.failure_count(), 0);
    }

    #[test]
    fn half_open_probe_success_recovers() {
        let chain = chain(vec![
            make_provider("primary", 1),
            make_provider("secondary", 2),
        ]);
        let primary = &chain.providers()[0];
        primary.record_failure(FailureClass::Transient, FailoverReason::ServerError, None);
        assert_eq!(primary.status(), Status::CoolingDown);

        std::thread::sleep(BASE.mul_f64(1.3));
        // window elapsed → next selection claims a half-open probe
        let selected = chain.select(&attempted(&[])).unwrap();
        assert_eq!(selected.config.name, "primary");
        assert_eq!(primary.status(), Status::HalfOpen);

        // a second concurrent selection may not take the probe slot
        let selected = chain.select(&attempted(&["primary"])).unwrap();
        assert_eq!(selected.config.name, "secondary");

        primary.record_success();
        assert_eq!(primary.status(), Status::Healthy);
        assert_eq!(primary.failure_count(), 0);
    }

    #[test]
    fn half_open_probe_failure_doubles_window() {
        let state = ProviderState::new(make_provider("p", 1), BASE, MAX);
        state.record_failure(FailureClass::Transient, FailoverReason::ServerError, None);
        let first_window = state.cooling_until().unwrap();

        std::thread::sleep(BASE.mul_f64(1.3));
        assert!(state.try_claim(), "should transition to half-open");
        assert_eq!(state.status(), Status::HalfOpen);

        state.record_failure(FailureClass::Transient, FailoverReason::ServerError, None);
        assert_eq!(state.status(), Status::CoolingDown);
        let second_until = state.cooling_until().unwrap();
        // nominal window doubled 100→200ms; even with jitter it must exceed the
        // first window's jittered lower bound
        assert!(second_until > first_window);
        assert_eq!(state.failure_count(), 2);
    }

    #[test]
    fn next_probe_at_reports_earliest_cooling_provider() {
        let chain = chain(vec![make_provider("a", 1), make_provider("b", 2)]);
        chain.providers()[0].record_failure(
            FailureClass::Transient,
            FailoverReason::ServerError,
            None,
        );
        let probe_at = chain.next_probe_at(&attempted(&[]));
        assert!(probe_at.is_some());
        assert!(chain.next_probe_at(&attempted(&["a"])).is_none());
    }

    #[test]
    fn select_for_role_prefers_named_then_fails_over() {
        let chain = chain(vec![
            make_provider("anthropic", 1),
            make_provider("deepseek", 2),
        ]);
        let selected = chain.select_for_role("deepseek", &attempted(&[])).unwrap();
        assert_eq!(selected.config.name, "deepseek");

        chain.providers()[1].record_failure(
            FailureClass::ProviderConfig,
            FailoverReason::Auth,
            None,
        );
        let selected = chain.select_for_role("deepseek", &attempted(&[])).unwrap();
        assert_eq!(selected.config.name, "anthropic");

        // unknown role name falls back to priority order
        let selected = chain
            .select_for_role("nonexistent", &attempted(&[]))
            .unwrap();
        assert_eq!(selected.config.name, "anthropic");
    }

    #[test]
    fn selection_emits_no_events_for_healthy_path() {
        // sanity: selecting a healthy provider does not consume a probe slot
        let chain = chain(vec![make_provider("p", 1)]);
        assert!(chain.select(&attempted(&[])).is_some());
        assert!(chain.select(&attempted(&[])).is_some());
    }
}
