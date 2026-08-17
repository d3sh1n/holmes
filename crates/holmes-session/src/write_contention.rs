use rand::Rng;
use std::future::Future;
use std::time::Duration;

/// Bounded retry for SQLite write contention (AGT-008).
///
/// Only genuine lock contention (`SQLITE_BUSY` / `SQLITE_LOCKED`) is retried —
/// those resolve once the competing writer commits. Every other database error
/// (constraint violations, corruption, I/O, schema problems) is permanent for
/// this statement and is surfaced immediately instead of being retried until
/// `max_retries` is exhausted, which previously masked permanent failures as
/// "write contention" and delayed the real error by seconds.
#[derive(Clone)]
pub struct WriteContention {
    max_retries: u32,
    base_delay_ms: u64,
    jitter_range_ms: u64,
}

/// Whether a rusqlite error is transient lock contention worth retrying.
pub fn is_busy_or_locked(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(code, _)
            if matches!(
                code.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
    )
}

impl WriteContention {
    pub fn new() -> Self {
        Self {
            max_retries: 15,
            base_delay_ms: 20,
            jitter_range_ms: 130,
        }
    }

    /// Test/tuning constructor: explicit retry budget and delays.
    pub fn with_limits(max_retries: u32, base_delay_ms: u64, jitter_range_ms: u64) -> Self {
        Self {
            max_retries,
            base_delay_ms,
            jitter_range_ms,
        }
    }

    /// Run `f`, retrying only while it fails with BUSY/LOCKED, up to
    /// `max_retries` attempts total. Permanent errors surface on first failure;
    /// contention that outlasts the budget surfaces the last BUSY/LOCKED error.
    pub async fn with_db_retry<F, Fut, T>(&self, f: F) -> Result<T, rusqlite::Error>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<T, rusqlite::Error>>,
    {
        let mut attempt = 0;

        loop {
            match f().await {
                Ok(result) => return Ok(result),
                Err(e) => {
                    if !is_busy_or_locked(&e) {
                        return Err(e);
                    }
                    attempt += 1;
                    if attempt >= self.max_retries {
                        holmes_core::metrics::metrics().count("sqlite.busy_retry_exhausted");
                        tracing::warn!(
                            attempt,
                            max_retries = self.max_retries,
                            error = %e,
                            "sqlite busy/locked retry budget exhausted; surfacing error"
                        );
                        return Err(e);
                    }
                    holmes_core::metrics::metrics().count("sqlite.busy_retry");
                    let delay = {
                        let mut rng = rand::thread_rng();
                        let jitter = rng.gen_range(0..self.jitter_range_ms.max(1));
                        self.base_delay_ms + jitter
                    };
                    tracing::warn!(
                        attempt,
                        max_retries = self.max_retries,
                        delay_ms = delay,
                        error = %e,
                        "sqlite busy/locked; retrying write"
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

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::ffi;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    fn busy_error() -> rusqlite::Error {
        rusqlite::Error::SqliteFailure(
            ffi::Error::new(ffi::SQLITE_BUSY),
            Some("database is locked".into()),
        )
    }

    fn constraint_error() -> rusqlite::Error {
        rusqlite::Error::SqliteFailure(
            ffi::Error::new(ffi::SQLITE_CONSTRAINT),
            Some("FOREIGN KEY constraint failed".into()),
        )
    }

    #[tokio::test]
    async fn busy_is_retried_until_success() {
        let calls = Arc::new(AtomicU32::new(0));
        let contention = WriteContention::with_limits(5, 0, 1);
        let calls_clone = calls.clone();
        let result = contention
            .with_db_retry(move || {
                let calls = calls_clone.clone();
                async move {
                    if calls.fetch_add(1, Ordering::SeqCst) < 2 {
                        Err(busy_error())
                    } else {
                        Ok(42)
                    }
                }
            })
            .await;
        assert_eq!(result.unwrap(), 42);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn busy_retry_budget_is_bounded() {
        let calls = Arc::new(AtomicU32::new(0));
        let contention = WriteContention::with_limits(3, 0, 1);
        let calls_clone = calls.clone();
        let result: Result<(), _> = contention
            .with_db_retry(move || {
                let calls = calls_clone.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Err(busy_error())
                }
            })
            .await;
        assert!(matches!(result, Err(e) if is_busy_or_locked(&e)));
        assert_eq!(calls.load(Ordering::SeqCst), 3, "bounded attempts");
    }

    #[tokio::test]
    async fn non_busy_error_is_not_retried() {
        let calls = Arc::new(AtomicU32::new(0));
        // A generous budget: if the permanent error were retried like before,
        // this would burn all 15 attempts instead of failing fast.
        let contention = WriteContention::with_limits(15, 0, 1);
        let calls_clone = calls.clone();
        let result: Result<(), _> = contention
            .with_db_retry(move || {
                let calls = calls_clone.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Err(constraint_error())
                }
            })
            .await;
        assert!(result.is_err());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "permanent errors must surface on the first failure"
        );
    }
}
