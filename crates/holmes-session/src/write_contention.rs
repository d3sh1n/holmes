use rand::Rng;
use std::future::Future;
use std::time::Duration;

pub struct WriteContention {
    max_retries: u32,
    base_delay_ms: u64,
    jitter_range_ms: u64,
}

impl WriteContention {
    pub fn new() -> Self {
        Self {
            max_retries: 15,
            base_delay_ms: 20,
            jitter_range_ms: 130,
        }
    }

    pub async fn with_retry<F, Fut, T, E>(&self, f: F) -> Result<T, E>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<T, E>>,
        E: std::fmt::Display,
    {
        let mut attempt = 0;
        let mut rng = rand::thread_rng();

        loop {
            match f().await {
                Ok(result) => return Ok(result),
                Err(e) => {
                    attempt += 1;
                    if attempt >= self.max_retries {
                        return Err(e);
                    }
                    let jitter = rng.gen_range(0..self.jitter_range_ms);
                    let delay = self.base_delay_ms + jitter;
                    tracing::warn!(
                        attempt,
                        max_retries = self.max_retries,
                        delay_ms = delay,
                        error = %e,
                        "write contention retry"
                    );
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
            }
        }
    }
}

impl Default for WriteContention {
    fn default() -> Self {
        Self::new()
    }
}
