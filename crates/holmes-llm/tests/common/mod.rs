//! Shared mock Anthropic HTTP server + config builders for the failover/recovery
//! integration tests. Each test binary compiles this module and uses only a subset
//! of the helpers, hence the crate-level dead-code allowance.
#![allow(dead_code)]

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::Response;
use axum::routing::post;
use axum::Router;
use holmes_core::config::{Config, ProviderConfig};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;

pub const BUFFERED_OK: &str = r#"{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"buffered-ok"}],"stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":5}}"#;

pub fn sse_ok(text: &str) -> String {
    r#"event: message_start
data: {"type":"message_start","message":{"usage":{"input_tokens":10}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"__TEXT__"}}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}

event: message_stop
data: {"type":"message_stop"}

"#
    .replace("__TEXT__", text)
}

/// SSE that starts a message and emits one delta but never a stop reason (truncated).
pub const SSE_TRUNCATED: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10}}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n";

/// SSE carrying a terminal `error` event frame (overloaded).
pub const SSE_OVERLOADED_ERROR: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10}}}\n\nevent: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n";

/// SSE that emits a text delta and THEN a terminal `error` event frame: the
/// attempt produced visible partial output before failing.
pub const SSE_DELTA_THEN_ERROR: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10}}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"leaked-prefix\"}}\n\nevent: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n";

#[derive(Clone)]
pub enum Behavior {
    /// 200 with a buffered Anthropic JSON body.
    Buffered(String),
    /// 200 text/event-stream with a complete SSE body.
    Sse(String),
    /// 200 SSE: yields one chunk then fails the body stream mid-flight.
    SseChunkError,
    /// Accepts the request and never responds (hung provider).
    Silent,
    /// Non-200 with a JSON error body and optional Retry-After seconds.
    Error {
        status: u16,
        body: String,
        retry_after: Option<u64>,
    },
}

pub struct Mock {
    pub hits: AtomicUsize,
    pub behavior: Mutex<Behavior>,
}

impl Mock {
    pub fn new(behavior: Behavior) -> Arc<Self> {
        Arc::new(Self {
            hits: AtomicUsize::new(0),
            behavior: Mutex::new(behavior),
        })
    }

    pub fn set(&self, behavior: Behavior) {
        *self.behavior.lock().unwrap() = behavior;
    }

    pub fn hit_count(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

async fn handle(State(mock): State<Arc<Mock>>, _body: String) -> Response {
    mock.hits.fetch_add(1, Ordering::SeqCst);
    let behavior = mock.behavior.lock().unwrap().clone();
    match behavior {
        Behavior::Buffered(body) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap(),
        Behavior::Sse(body) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from(body))
            .unwrap(),
        Behavior::SseChunkError => {
            let stream = futures::stream::iter(vec![
                Ok::<_, std::io::Error>(axum::body::Bytes::from_static(
                    b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n",
                )),
                Err(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "boom")),
            ]);
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/event-stream")
                .body(Body::from_stream(stream))
                .unwrap()
        }
        Behavior::Silent => {
            // A hung provider: the response body stream never yields and never ends.
            let stream = futures::stream::pending::<Result<axum::body::Bytes, std::io::Error>>();
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from_stream(stream))
                .unwrap()
        }
        Behavior::Error {
            status,
            body,
            retry_after,
        } => {
            let mut response = Response::builder()
                .status(StatusCode::from_u16(status).unwrap())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap();
            if let Some(secs) = retry_after {
                response
                    .headers_mut()
                    .insert("retry-after", secs.to_string().parse().unwrap());
            }
            response
        }
    }
}

/// Spawn a mock server on an ephemeral localhost port; returns its base URL and the
/// shared mock state.
pub async fn spawn_mock(behavior: Behavior) -> (String, Arc<Mock>) {
    let mock = Mock::new(behavior);
    let app = Router::new()
        .route("/v1/messages", post(handle))
        .with_state(mock.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://127.0.0.1:{port}"), mock)
}

/// A port with nothing listening on it (listener dropped immediately).
pub async fn closed_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

pub fn provider(name: &str, base_url: &str, priority: u32) -> ProviderConfig {
    ProviderConfig {
        name: name.into(),
        base_url: base_url.into(),
        api_key: "test-key".into(),
        api_key_env: None,
        model: "test-model".into(),
        api_format: Default::default(),
        priority,
        rpm_limit: 0,
    }
}

pub fn test_config(
    providers: Vec<ProviderConfig>,
    stream: bool,
    cooldown_base_ms: u64,
    cooldown_max_ms: u64,
    call_deadline_ms: u64,
) -> Config {
    let mut config = Config::default();
    config.llm.stream = stream;
    config.llm.provider_cooldown_base_ms = cooldown_base_ms;
    config.llm.provider_cooldown_max_ms = cooldown_max_ms;
    config.llm.call_deadline_ms = call_deadline_ms;
    if let Some(first) = providers.first() {
        config.llm.roles.attack_agent = first.name.clone();
    }
    config.llm.providers = providers;
    config
}
