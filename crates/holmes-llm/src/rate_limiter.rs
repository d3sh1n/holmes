use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio::time::{Duration, Instant};
use tracing::debug;

pub struct RateLimiter {
    limiters: HashMap<String, ProviderLimiter>,
}

struct ProviderLimiter {
    tokens: tokio::sync::Mutex<TokenBucket>,
    concurrency: Arc<Semaphore>,
}

struct TokenBucket {
    capacity: u32,
    tokens: f64,
    last_refill: Instant,
    refill_rate: f64,
}

impl TokenBucket {
    fn new(requests_per_minute: u32) -> Self {
        Self {
            capacity: requests_per_minute,
            tokens: requests_per_minute as f64,
            last_refill: Instant::now(),
            refill_rate: requests_per_minute as f64 / 60.0,
        }
    }

    fn try_acquire(&mut self) -> Option<Duration> {
        self.refill();
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            None
        } else {
            let wait = (1.0 - self.tokens) / self.refill_rate;
            Some(Duration::from_secs_f64(wait))
        }
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_rate).min(self.capacity as f64);
        self.last_refill = now;
    }
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            limiters: HashMap::new(),
        }
    }

    pub fn register(&mut self, provider: String, requests_per_minute: u32, max_concurrent: u32) {
        self.limiters.insert(
            provider,
            ProviderLimiter {
                tokens: tokio::sync::Mutex::new(TokenBucket::new(requests_per_minute)),
                concurrency: Arc::new(Semaphore::new(max_concurrent as usize)),
            },
        );
    }

    pub async fn acquire(&self, provider: &str) -> Option<RateLimitPermit> {
        let limiter = self.limiters.get(provider)?;

        let concurrency_permit = limiter.concurrency.clone().acquire_owned().await.ok()?;

        loop {
            let wait = {
                let mut bucket = limiter.tokens.lock().await;
                bucket.try_acquire()
            };
            match wait {
                None => break,
                Some(duration) => {
                    debug!(
                        provider,
                        wait_ms = duration.as_millis() as u64,
                        "rate limit wait"
                    );
                    tokio::time::sleep(duration).await;
                }
            }
        }

        Some(RateLimitPermit {
            _permit: concurrency_permit,
        })
    }
}

pub struct RateLimitPermit {
    _permit: tokio::sync::OwnedSemaphorePermit,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn acquire_within_limit() {
        let mut rl = RateLimiter::new();
        rl.register("test".into(), 60, 4);
        let permit = rl.acquire("test").await;
        assert!(permit.is_some());
    }

    #[tokio::test]
    async fn unknown_provider_returns_none() {
        let rl = RateLimiter::new();
        let permit = rl.acquire("nonexistent").await;
        assert!(permit.is_none());
    }

    #[tokio::test]
    async fn concurrency_limit_respected() {
        let mut rl = RateLimiter::new();
        rl.register("test".into(), 600, 2);

        let p1 = rl.acquire("test").await;
        let p2 = rl.acquire("test").await;
        assert!(p1.is_some());
        assert!(p2.is_some());

        let rl = Arc::new(rl);
        let rl2 = rl.clone();
        let handle = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_millis(100), rl2.acquire("test")).await
        });

        let result = handle.await.unwrap();
        assert!(
            result.is_err(),
            "third acquire should timeout due to concurrency limit"
        );

        drop(p1);
        drop(p2);
    }
}
