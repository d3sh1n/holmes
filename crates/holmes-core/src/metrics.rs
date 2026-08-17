//! Lightweight in-process metrics registry (AGT-015).
//!
//! Deliberately dependency-free: the agent is a single-process CLI, so the core
//! reliability metrics the remediation plan calls for (turn latency percentiles,
//! provider failover/recovery rates, tool timeout counts, recovery dispositions,
//! verified-finish ratio, memory recall hit rate, subagent cost) are kept as
//! counters and bounded latency samples in a global registry, updated at the same
//! code points that emit the structured tracing events.
//!
//! Naming: metric names are dotted lowercase (`turn.duration_ms`,
//! `provider.recovered`). This is distinct from structured tracing events
//! (CamelCase `event = "..."` field) and persisted session events (snake_case
//! `event_type` column) — see `docs/observability.md` for the full catalog.
//!
//! The registry is pull-based: call [`snapshot`] to read a consistent, JSON-
//! serializable view. Ratios the plan lists (failover rate, verified-finish
//! ratio, recall hit rate, restart recovery rate) are derived from the counters
//! by the reader, keeping the write path trivially cheap.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Maximum latency samples retained per metric. Older samples are dropped
/// FIFO, so percentiles describe the recent window of a long-running process
/// instead of its whole lifetime (and memory stays bounded).
const MAX_SAMPLES: usize = 4096;

#[derive(Default)]
struct Inner {
    counters: BTreeMap<String, u64>,
    /// Latency/quantity samples in milliseconds (or the unit implied by the
    /// metric name suffix), insertion-ordered.
    samples: BTreeMap<String, VecDeque<u64>>,
}

/// In-process metrics registry. All methods are cheap and thread-safe; none
/// of them allocate on the hot path beyond a map lookup.
pub struct MetricsRegistry {
    inner: Mutex<Inner>,
}

impl MetricsRegistry {
    /// Increment counter `name` by one.
    pub fn count(&self, name: &str) {
        self.count_by(name, 1);
    }

    /// Increment counter `name` by `n`.
    pub fn count_by(&self, name: &str, n: u64) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        *inner.counters.entry(name.to_string()).or_insert(0) += n;
    }

    /// Record one latency/quantity sample (milliseconds).
    pub fn record_ms(&self, name: &str, ms: u64) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let series = inner.samples.entry(name.to_string()).or_default();
        if series.len() >= MAX_SAMPLES {
            series.pop_front();
        }
        series.push_back(ms);
    }

    /// Record one duration sample.
    pub fn record_duration(&self, name: &str, duration: Duration) {
        self.record_ms(name, duration.as_millis() as u64);
    }

    /// A consistent snapshot of every counter and per-metric percentiles.
    pub fn snapshot(&self) -> MetricsSnapshot {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let timings = inner
            .samples
            .iter()
            .map(|(name, series)| (name.clone(), TimingSummary::from_samples(series)))
            .collect();
        MetricsSnapshot {
            counters: inner.counters.clone(),
            timings,
        }
    }

    /// Current value of one counter (0 when never recorded). Test/inspection helper.
    pub fn counter(&self, name: &str) -> u64 {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.counters.get(name).copied().unwrap_or(0)
    }

    /// Drop everything. Intended for tests that assert exact counter values.
    pub fn reset(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        *inner = Inner::default();
    }
}

/// Process-wide registry. Instrumentation sites call `metrics().count(...)`
/// directly; the handle never fails and never blocks meaningfully.
pub fn metrics() -> &'static MetricsRegistry {
    static REGISTRY: OnceLock<MetricsRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| MetricsRegistry {
        inner: Mutex::new(Inner::default()),
    })
}

/// Serializable view of all metrics at one instant.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MetricsSnapshot {
    pub counters: BTreeMap<String, u64>,
    pub timings: BTreeMap<String, TimingSummary>,
}

/// Percentile summary of one sample series (nearest-rank over retained samples).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TimingSummary {
    pub count: u64,
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub p99_ms: u64,
    pub max_ms: u64,
}

impl TimingSummary {
    fn from_samples(samples: &VecDeque<u64>) -> Self {
        if samples.is_empty() {
            return Self::default();
        }
        let mut sorted: Vec<u64> = samples.iter().copied().collect();
        sorted.sort_unstable();
        let percentile = |p: f64| -> u64 {
            // Nearest-rank: p50 of [a] is a, p95 of 100 samples is index 94.
            let rank = ((sorted.len() as f64) * p).ceil() as usize;
            sorted[rank.max(1).min(sorted.len()) - 1]
        };
        Self {
            count: sorted.len() as u64,
            p50_ms: percentile(0.50),
            p95_ms: percentile(0.95),
            p99_ms: percentile(0.99),
            max_ms: *sorted.last().unwrap_or(&0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_accumulate() {
        let registry = MetricsRegistry {
            inner: Mutex::new(Inner::default()),
        };
        registry.count("turn.completed");
        registry.count("turn.completed");
        registry.count_by("llm.call.success", 3);
        let snapshot = registry.snapshot();
        assert_eq!(snapshot.counters["turn.completed"], 2);
        assert_eq!(snapshot.counters["llm.call.success"], 3);
    }

    #[test]
    fn percentiles_are_nearest_rank() {
        let registry = MetricsRegistry {
            inner: Mutex::new(Inner::default()),
        };
        for ms in 1..=100u64 {
            registry.record_ms("turn.duration_ms", ms);
        }
        let summary = registry.snapshot().timings["turn.duration_ms"];
        assert_eq!(summary.count, 100);
        assert_eq!(summary.p50_ms, 50);
        assert_eq!(summary.p95_ms, 95);
        assert_eq!(summary.p99_ms, 99);
        assert_eq!(summary.max_ms, 100);
    }

    #[test]
    fn single_sample_series_reports_that_sample() {
        let summary = TimingSummary::from_samples(&VecDeque::from([42]));
        assert_eq!(summary.p50_ms, 42);
        assert_eq!(summary.p99_ms, 42);
        assert_eq!(summary.max_ms, 42);
    }

    #[test]
    fn samples_are_bounded_fifo() {
        let registry = MetricsRegistry {
            inner: Mutex::new(Inner::default()),
        };
        for ms in 0..(MAX_SAMPLES as u64 + 10) {
            registry.record_ms("provider.downtime_ms", ms);
        }
        let summary = registry.snapshot().timings["provider.downtime_ms"];
        assert_eq!(summary.count, MAX_SAMPLES as u64);
        // The oldest 10 samples were dropped; the max is the newest sample.
        assert_eq!(summary.max_ms, MAX_SAMPLES as u64 + 9);
    }

    #[test]
    fn snapshot_serializes_to_json() {
        let registry = MetricsRegistry {
            inner: Mutex::new(Inner::default()),
        };
        registry.count("task.recovered");
        registry.record_ms("turn.duration_ms", 123);
        let json = serde_json::to_value(registry.snapshot()).expect("serialize");
        assert_eq!(json["counters"]["task.recovered"], 1);
        assert_eq!(json["timings"]["turn.duration_ms"]["p50_ms"], 123);
    }
}
