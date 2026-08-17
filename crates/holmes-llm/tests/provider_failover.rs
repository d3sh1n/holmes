//! Failover integration tests: a single `LlmClient` call must survive individual
//! provider failures by failing over, must never re-select an attempted provider,
//! and must classify request-content vs provider-config errors correctly.

mod common;

use common::{
    closed_port, provider, spawn_mock, sse_ok, test_config, Behavior, BUFFERED_OK,
    SSE_DELTA_THEN_ERROR, SSE_OVERLOADED_ERROR, SSE_TRUNCATED,
};
use holmes_core::Message;
use holmes_llm::client::LlmClient;
use holmes_llm::provider::Status;

fn messages() -> Vec<Message> {
    vec![Message::user("ping")]
}

#[tokio::test]
async fn connection_refused_fails_over_to_second_provider() {
    let dead_port = closed_port().await;
    let (up_url, up) = spawn_mock(Behavior::Buffered(BUFFERED_OK.into())).await;

    let config = test_config(
        vec![
            provider("down", &format!("http://127.0.0.1:{dead_port}"), 1),
            provider("up", &up_url, 2),
        ],
        false,
        5_000,
        60_000,
        30_000,
    );
    let client = LlmClient::new(&config);

    let resp = client
        .chat_completion(&messages(), &[], "attack_agent")
        .await
        .expect("call must fail over to the reachable provider");
    assert_eq!(resp.content.as_deref(), Some("buffered-ok"));
    assert_eq!(up.hit_count(), 1);

    let chain = client.failover_chain();
    assert_eq!(chain.providers()[0].status(), Status::CoolingDown);
    assert_eq!(chain.providers()[1].status(), Status::Healthy);
}

#[tokio::test]
async fn stream_interruption_fails_over_and_still_streams_deltas() {
    let (bad_url, bad) = spawn_mock(Behavior::SseChunkError).await;
    let (good_url, good) = spawn_mock(Behavior::Sse(sse_ok("streamed-ok"))).await;

    let config = test_config(
        vec![
            provider("flaky", &bad_url, 1),
            provider("stable", &good_url, 2),
        ],
        true,
        5_000,
        60_000,
        30_000,
    );
    let client = LlmClient::new(&config);

    let mut deltas = String::new();
    let resp = client
        .chat_completion_streaming(&messages(), &[], "attack_agent", &mut |t| {
            deltas.push_str(t)
        })
        .await
        .expect("call must fail over after the stream broke");
    assert_eq!(resp.content.as_deref(), Some("streamed-ok"));
    assert_eq!(deltas, "streamed-ok");
    assert_eq!(bad.hit_count(), 1, "broken stream attempted exactly once");
    assert_eq!(good.hit_count(), 1);
}

#[tokio::test]
async fn failed_attempt_deltas_never_reach_the_ui_callback() {
    // Regression: provider A emits text deltas and then fails (truncated body —
    // no stop reason); the call fails over to provider B. The UI callback must
    // see exactly B's text — no residue of A's partial prefix.
    let (bad_url, bad) = spawn_mock(Behavior::Sse(SSE_TRUNCATED.into())).await;
    let (good_url, good) = spawn_mock(Behavior::Sse(sse_ok("authoritative"))).await;

    let config = test_config(
        vec![
            provider("truncated", &bad_url, 1),
            provider("good", &good_url, 2),
        ],
        true,
        5_000,
        60_000,
        30_000,
    );
    let client = LlmClient::new(&config);

    let mut deltas = String::new();
    let resp = client
        .chat_completion_streaming(&messages(), &[], "attack_agent", &mut |t| {
            deltas.push_str(t)
        })
        .await
        .expect("call must fail over after the truncated stream");
    assert_eq!(resp.content.as_deref(), Some("authoritative"));
    assert_eq!(
        deltas, "authoritative",
        "UI text must match the authoritative response exactly"
    );
    assert!(!deltas.contains("partial"), "no failed-attempt residue");
    assert_eq!(bad.hit_count(), 1);
    assert_eq!(good.hit_count(), 1);
    assert_eq!(
        client.failover_chain().providers()[1].status(),
        Status::Healthy,
        "the winning provider is the one whose text the UI saw"
    );
}

#[tokio::test]
async fn deltas_before_terminal_error_event_are_discarded_on_failover() {
    // Provider A streams a text delta and then a terminal `error` event frame;
    // provider B answers cleanly. A's "leaked-prefix" must never reach the UI.
    let (bad_url, _) = spawn_mock(Behavior::Sse(SSE_DELTA_THEN_ERROR.into())).await;
    let (good_url, _) = spawn_mock(Behavior::Sse(sse_ok("clean-response"))).await;

    let config = test_config(
        vec![
            provider("flaky", &bad_url, 1),
            provider("stable", &good_url, 2),
        ],
        true,
        5_000,
        60_000,
        30_000,
    );
    let client = LlmClient::new(&config);

    let mut deltas = String::new();
    let resp = client
        .chat_completion_streaming(&messages(), &[], "attack_agent", &mut |t| {
            deltas.push_str(t)
        })
        .await
        .expect("SSE error event must fail over");
    assert_eq!(resp.content.as_deref(), Some("clean-response"));
    assert_eq!(deltas, "clean-response");
    assert!(!deltas.contains("leaked-prefix"));
}

#[tokio::test]
async fn truncated_sse_without_stop_reason_is_transient_and_fails_over() {
    let (bad_url, _) = spawn_mock(Behavior::Sse(SSE_TRUNCATED.into())).await;
    let (good_url, _) = spawn_mock(Behavior::Sse(sse_ok("recovered"))).await;

    let config = test_config(
        vec![
            provider("truncated", &bad_url, 1),
            provider("good", &good_url, 2),
        ],
        true,
        5_000,
        60_000,
        30_000,
    );
    let client = LlmClient::new(&config);

    let resp = client
        .chat_completion(&messages(), &[], "attack_agent")
        .await
        .expect("truncated SSE must be classified transient and fail over");
    assert_eq!(resp.content.as_deref(), Some("recovered"));
}

#[tokio::test]
async fn sse_error_event_frame_is_classified_and_fails_over() {
    let (bad_url, _) = spawn_mock(Behavior::Sse(SSE_OVERLOADED_ERROR.into())).await;
    let (good_url, _) = spawn_mock(Behavior::Sse(sse_ok("after-error-event"))).await;

    let config = test_config(
        vec![
            provider("overloaded", &bad_url, 1),
            provider("good", &good_url, 2),
        ],
        true,
        5_000,
        60_000,
        30_000,
    );
    let client = LlmClient::new(&config);

    let resp = client
        .chat_completion(&messages(), &[], "attack_agent")
        .await
        .expect("SSE error event must fail over");
    assert_eq!(resp.content.as_deref(), Some("after-error-event"));
}

#[tokio::test]
async fn same_provider_is_never_reselected_within_one_call() {
    let (url, server) = spawn_mock(Behavior::Error {
        status: 500,
        body: r#"{"error":{"type":"api_error","message":"internal"}}"#.into(),
        retry_after: None,
    })
    .await;

    let config = test_config(
        vec![provider("only", &url, 1)],
        false,
        5_000,
        60_000,
        30_000,
    );
    let client = LlmClient::new(&config);

    let err = client
        .chat_completion(&messages(), &[], "attack_agent")
        .await
        .expect_err("single failing provider must fail the call");
    assert!(
        err.to_string()
            .contains("no healthy LLM provider available"),
        "unexpected error: {err}"
    );
    assert_eq!(
        server.hit_count(),
        1,
        "provider must be attempted exactly once per call"
    );
}

#[tokio::test]
async fn bad_request_400_does_not_affect_provider_health() {
    let (url, server) = spawn_mock(Behavior::Error {
        status: 400,
        body: r#"{"error":{"type":"invalid_request_error","message":"invalid temperature"}}"#
            .into(),
        retry_after: None,
    })
    .await;

    let config = test_config(vec![provider("p", &url, 1)], false, 5_000, 60_000, 30_000);
    let client = LlmClient::new(&config);

    let err = client
        .chat_completion(&messages(), &[], "attack_agent")
        .await
        .expect_err("400 must propagate to the caller");
    assert!(
        err.to_string().contains("invalid temperature"),
        "unexpected error: {err}"
    );

    let chain = client.failover_chain();
    assert_eq!(
        chain.providers()[0].status(),
        Status::Healthy,
        "request-content errors must not touch provider health"
    );
    assert_eq!(chain.providers()[0].failure_count(), 0);

    // still selectable: the next call goes to the same provider
    let _ = client
        .chat_completion(&messages(), &[], "attack_agent")
        .await;
    assert_eq!(server.hit_count(), 2);
}

#[tokio::test]
async fn unauthorized_401_disables_provider_permanently() {
    let (auth_url, auth) = spawn_mock(Behavior::Error {
        status: 401,
        body: r#"{"error":{"type":"authentication_error","message":"invalid api key"}}"#.into(),
        retry_after: None,
    })
    .await;
    let (up_url, up) = spawn_mock(Behavior::Buffered(BUFFERED_OK.into())).await;

    let config = test_config(
        vec![
            provider("bad-key", &auth_url, 1),
            provider("up", &up_url, 2),
        ],
        false,
        100,
        1_000,
        30_000,
    );
    let client = LlmClient::new(&config);

    let resp = client
        .chat_completion(&messages(), &[], "attack_agent")
        .await
        .expect("401 on primary must fail over");
    assert_eq!(resp.content.as_deref(), Some("buffered-ok"));
    assert_eq!(
        client.failover_chain().providers()[0].status(),
        Status::Disabled
    );

    // Well past any cooldown window: the disabled provider is never probed again.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let resp = client
        .chat_completion(&messages(), &[], "attack_agent")
        .await
        .expect("second call must succeed");
    assert_eq!(resp.content.as_deref(), Some("buffered-ok"));
    assert_eq!(
        auth.hit_count(),
        1,
        "disabled provider must never be re-selected"
    );
    assert_eq!(up.hit_count(), 2);
}
