//! Recovery integration tests: cooling providers are probed half-open after their
//! window, successful probes restore health, failed probes double the window, and a
//! call with every provider unavailable fails deterministically inside its deadline.

mod common;

use common::{provider, spawn_mock, test_config, Behavior, BUFFERED_OK};
use holmes_core::Message;
use holmes_llm::client::LlmClient;
use holmes_llm::provider::Status;
use std::time::{Duration, Instant};

fn messages() -> Vec<Message> {
    vec![Message::user("ping")]
}

fn server_error() -> Behavior {
    Behavior::Error {
        status: 500,
        body: r#"{"error":{"type":"api_error","message":"internal"}}"#.into(),
        retry_after: None,
    }
}

#[tokio::test]
async fn all_providers_unavailable_fails_deterministically_within_deadline() {
    // Both providers rate-limit with a Retry-After far beyond the call deadline.
    let (u1, _) = spawn_mock(Behavior::Error {
        status: 429,
        body: r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#.into(),
        retry_after: Some(60),
    })
    .await;
    let (u2, _) = spawn_mock(Behavior::Error {
        status: 429,
        body: r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#.into(),
        retry_after: Some(60),
    })
    .await;

    let config = test_config(
        vec![provider("p1", &u1, 1), provider("p2", &u2, 2)],
        false,
        5_000,
        300_000,
        500, // call deadline: 500ms
    );
    let client = LlmClient::new(&config);

    let started = Instant::now();
    let err = client
        .chat_completion(&messages(), &[], "attack_agent")
        .await
        .expect_err("every provider unavailable must fail the call");
    let elapsed = started.elapsed();

    assert!(
        err.to_string()
            .contains("no healthy LLM provider available"),
        "unexpected error: {err}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "call must fail deterministically, not wait for recovery: {elapsed:?}"
    );
}

#[tokio::test]
async fn cooled_down_provider_recovers_via_half_open_probe() {
    let (url, server) = spawn_mock(server_error()).await;

    let config = test_config(
        vec![provider("solo", &url, 1)],
        false,
        120,   // cooldown base
        5_000, // cooldown max
        10_000,
    );
    let client = LlmClient::new(&config);

    let err = client
        .chat_completion(&messages(), &[], "attack_agent")
        .await
        .expect_err("failing provider fails the first call");
    assert!(err
        .to_string()
        .contains("no healthy LLM provider available"));
    assert_eq!(
        client.failover_chain().providers()[0].status(),
        Status::CoolingDown
    );

    server.set(Behavior::Buffered(BUFFERED_OK.into()));

    let started = Instant::now();
    let resp = client
        .chat_completion(&messages(), &[], "attack_agent")
        .await
        .expect("second call waits for the half-open probe and succeeds");
    let elapsed = started.elapsed();

    assert_eq!(resp.content.as_deref(), Some("buffered-ok"));
    assert_eq!(
        client.failover_chain().providers()[0].status(),
        Status::Healthy,
        "successful half-open probe must restore health"
    );
    assert!(
        elapsed >= Duration::from_millis(80),
        "the call must have waited for the cooling window ({elapsed:?})"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "the probe must fire at the window end, not later ({elapsed:?})"
    );
    assert_eq!(server.hit_count(), 2);
}

#[tokio::test]
async fn failed_half_open_probe_doubles_the_cooling_window() {
    let (url, _server) = spawn_mock(server_error()).await;

    let config = test_config(vec![provider("solo", &url, 1)], false, 100, 5_000, 10_000);
    let client = LlmClient::new(&config);

    // First failure: window ≈ 100ms (±20% jitter).
    let _ = client
        .chat_completion(&messages(), &[], "attack_agent")
        .await;
    let provider_state = &client.failover_chain().providers()[0];
    let first_until = provider_state
        .cooling_until()
        .expect("cooling after failure");
    let first_window = first_until.saturating_duration_since(Instant::now());
    assert!(
        (Duration::from_millis(60)..=Duration::from_millis(125)).contains(&first_window),
        "first window {first_window:?} should be ~100ms"
    );

    // Wait out the first window; the next call probes half-open and fails again.
    tokio::time::sleep(first_window + Duration::from_millis(30)).await;
    let _ = client
        .chat_completion(&messages(), &[], "attack_agent")
        .await;
    assert_eq!(provider_state.status(), Status::CoolingDown);
    assert_eq!(provider_state.failure_count(), 2);

    let second_window = provider_state
        .cooling_until()
        .unwrap()
        .saturating_duration_since(Instant::now());
    assert!(
        second_window >= Duration::from_millis(150),
        "failed probe must double the window (~200ms), got {second_window:?}"
    );
    assert!(
        second_window <= Duration::from_millis(250),
        "window must stay within jitter bounds, got {second_window:?}"
    );
}

#[tokio::test]
async fn healthy_provider_is_selected_without_waiting_after_other_recovers() {
    // Two providers: p1 fails once and cools down; while it cools, calls keep using
    // p2 immediately. After p1's window elapses it is probed and (still failing)
    // cools again — p2 keeps serving.
    let (u1, _) = spawn_mock(server_error()).await;
    let (u2, up) = spawn_mock(Behavior::Buffered(BUFFERED_OK.into())).await;

    let config = test_config(
        vec![provider("p1", &u1, 1), provider("p2", &u2, 2)],
        false,
        150,
        5_000,
        10_000,
    );
    let client = LlmClient::new(&config);

    // p1 fails → cools; p2 serves.
    let resp = client
        .chat_completion(&messages(), &[], "attack_agent")
        .await
        .expect("p2 serves while p1 cools");
    assert_eq!(resp.content.as_deref(), Some("buffered-ok"));

    // Immediate next call must not wait for p1's probe; p2 answers directly.
    let started = Instant::now();
    let resp = client
        .chat_completion(&messages(), &[], "attack_agent")
        .await
        .expect("healthy provider serves immediately");
    assert_eq!(resp.content.as_deref(), Some("buffered-ok"));
    assert!(
        started.elapsed() < Duration::from_millis(140),
        "healthy provider must answer without waiting for the cooling one"
    );
    assert_eq!(up.hit_count(), 2);
}
