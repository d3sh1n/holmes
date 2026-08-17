//! P1-01 acceptance: the absolute call deadline (`llm.call_deadline_ms`) covers the
//! whole call lifecycle — a silent provider and rate-limiter queueing must both end
//! within the deadline instead of parking the caller.

mod common;

use std::time::{Duration, Instant};

use common::{provider, spawn_mock, test_config, Behavior, BUFFERED_OK};
use holmes_core::Message;
use holmes_llm::client::LlmClient;

fn messages() -> Vec<Message> {
    vec![Message::user("ping")]
}

#[tokio::test]
async fn silent_provider_fails_within_call_deadline() {
    let (url, mock) = spawn_mock(Behavior::Silent).await;
    // 400ms call deadline; cooldown far above it so no half-open retry is attempted.
    let config = test_config(vec![provider("hung", &url, 1)], false, 60_000, 60_000, 400);
    let client = LlmClient::new(&config);

    let start = Instant::now();
    let err = client
        .chat_completion(&messages(), &[], "attack_agent")
        .await
        .expect_err("silent provider must not hang the call");
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_secs(5),
        "call bounded by deadline, took {elapsed:?}"
    );
    assert!(elapsed >= Duration::from_millis(350));
    assert_eq!(mock.hit_count(), 1, "exactly one provider attempt");
    assert!(
        err.to_string()
            .contains("no healthy LLM provider available"),
        "got: {err}"
    );
}

#[tokio::test]
async fn rate_limiter_queue_time_is_charged_against_call_deadline() {
    let (url, _mock) = spawn_mock(Behavior::Buffered(BUFFERED_OK.into())).await;
    // rpm_limit=1: the first call drains the single token; the refill takes ~60s.
    let mut limited = provider("limited", &url, 1);
    limited.rpm_limit = 1;
    let config = test_config(vec![limited], false, 60_000, 60_000, 500);
    let client = LlmClient::new(&config);

    client
        .chat_completion(&messages(), &[], "attack_agent")
        .await
        .expect("first call consumes the only rate-limit token");

    // The second call must queue at the rate limiter — and the queue wait is part
    // of the 500ms call deadline, so it fails fast instead of waiting ~60s.
    let start = Instant::now();
    let err = client
        .chat_completion(&messages(), &[], "attack_agent")
        .await
        .expect_err("queued call must hit the call deadline");
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_secs(5),
        "rate-limit queue bounded by call deadline, took {elapsed:?}"
    );
    assert!(
        err.to_string()
            .contains("no healthy LLM provider available"),
        "got: {err}"
    );
}

#[tokio::test]
async fn dropped_call_leaves_no_background_provider_request() {
    // Caller cancellation drops the future; the in-flight HTTP request is aborted
    // on drop, so no provider request lingers behind the cancelled call.
    let (url, mock) = spawn_mock(Behavior::Silent).await;
    let config = test_config(
        vec![provider("hung", &url, 1)],
        false,
        60_000,
        60_000,
        60_000,
    );
    let client = std::sync::Arc::new(LlmClient::new(&config));

    let start = Instant::now();
    let cancelled = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .chat_completion(&messages(), &[], "attack_agent")
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    cancelled.abort();
    let _ = cancelled.await;

    // The aborted task is gone; nothing keeps polling the mock afterwards. A quiet
    // period must show zero additional hits beyond the single in-flight attempt.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        mock.hit_count(),
        1,
        "no retry after the caller dropped the call"
    );
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "took {:?}",
        start.elapsed()
    );
}
