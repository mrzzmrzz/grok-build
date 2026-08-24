//! Actor-level durable stream recovery (review follow-up): once at least one
//! complete output item has been observed, a clean EOF, a retryable
//! mid-stream transport failure, and an idle timeout all take the same
//! durable-output decision — the completed items reach the caller as a
//! terminal response from exactly one HTTP request, instead of being
//! discarded and re-billed by a retry (or reclassified as a max-token
//! truncation).

mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use axum::Router;
use axum::routing::post;
use futures_util::StreamExt as _;
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use xai_grok_sampler::{RequestId, RetryPolicy, SamplerActor, SamplerConfig};
use xai_grok_sampling_types::{
    ApiBackend, ContentPart, ConversationItem, ConversationRequest, StopReason, UserItem,
};

/// SSE frames for a Responses stream that has completed one whole message
/// output item (`response.created` + `response.output_item.done`) but has NOT
/// seen a terminal `response.completed` yet.
fn created_and_message_done_frames(text: &str) -> Vec<String> {
    vec![
        json!({
            "type": "response.created",
            "sequence_number": 0,
            "response": {
                "id": "resp_durable",
                "object": "response",
                "created_at": 1_234_567_890,
                "model": "gpt-test",
                "status": "in_progress",
                "output": []
            }
        })
        .to_string(),
        json!({
            "type": "response.output_item.done",
            "sequence_number": 1,
            "output_index": 0,
            "item": {
                "type": "message",
                "id": "msg_durable",
                "role": "assistant",
                "status": "completed",
                "content": [{
                    "type": "output_text",
                    "text": text,
                    "annotations": []
                }]
            }
        })
        .to_string(),
    ]
}

fn sse_body(frames: &[String]) -> String {
    frames
        .iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect()
}

fn test_config(addr: SocketAddr, idle_timeout_secs: u64) -> SamplerConfig {
    let mut cfg = support::test_config(&format!("http://{addr}/v1"), "test-key");
    cfg.api_backend = ApiBackend::Responses;
    cfg.max_retries = Some(2);
    cfg.idle_timeout_secs = Some(idle_timeout_secs);
    cfg
}

fn user_request(text: &str) -> ConversationRequest {
    ConversationRequest {
        items: vec![ConversationItem::User(UserItem {
            content: vec![ContentPart::Text {
                text: Arc::<str>::from(text),
            }],
            ..Default::default()
        })],
        ..Default::default()
    }
}

async fn spawn_raw_body_server(
    handler: impl Fn(u32) -> axum::response::Response + Clone + Send + Sync + 'static,
) -> (SocketAddr, Arc<AtomicU32>) {
    let hits = Arc::new(AtomicU32::new(0));
    let hits_handler = Arc::clone(&hits);
    let app = Router::new().route(
        "/v1/responses",
        post(move || {
            let handler = handler.clone();
            let hits = Arc::clone(&hits_handler);
            async move { handler(hits.fetch_add(1, Ordering::SeqCst)) }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (addr, hits)
}

fn event_stream_response(body: axum::body::Body) -> axum::response::Response {
    axum::response::Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .body(body)
        .unwrap()
}

/// done item → mid-body socket error: the body errors after the completed
/// item; the recovered message reaches the caller from one request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_item_then_socket_error_recovers_durably() {
    let (addr, hits) = spawn_raw_body_server(|_| {
        let frames = created_and_message_done_frames("survived the reset");
        // Flush the completed frames first (the delayed error keeps hyper
        // from folding the whole body into one aborted send), then kill the
        // body mid-stream.
        let stream = futures_util::stream::iter(vec![Ok::<_, std::io::Error>(
            axum::body::Bytes::from(sse_body(&frames)),
        )])
        .chain(futures_util::stream::once(async {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "connection reset by peer",
            ))
        }));
        event_stream_response(axum::body::Body::from_stream(stream))
    })
    .await;

    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let handle = SamplerActor::spawn(test_config(addr, 30), RetryPolicy::default(), event_tx);
    let (response, _metrics) = handle
        .submit_and_collect(RequestId::from("req-durable-reset"), user_request("hi"))
        .await
        .expect("durable recovery must complete the turn");

    assert_eq!(response.assistant_text(), "survived the reset");
    assert_eq!(response.stop_reason, Some(StopReason::Stop));
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "recovery must not burn a retry"
    );
}

/// done item → idle timeout: the stream stalls after the completed item;
/// the recovered message reaches the caller instead of an IdleTimeout error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn done_item_then_idle_timeout_recovers_durably() {
    let (addr, hits) = spawn_raw_body_server(|_| {
        let frames = created_and_message_done_frames("survived the stall");
        let stream = futures_util::stream::iter(vec![Ok::<_, std::io::Error>(
            axum::body::Bytes::from(sse_body(&frames)),
        )])
        .chain(futures_util::stream::pending());
        event_stream_response(axum::body::Body::from_stream(stream))
    })
    .await;

    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let handle = SamplerActor::spawn(test_config(addr, 1), RetryPolicy::default(), event_tx);
    let (response, _metrics) = handle
        .submit_and_collect(RequestId::from("req-durable-stall"), user_request("hi"))
        .await
        .expect("durable recovery must complete the turn");

    assert_eq!(response.assistant_text(), "survived the stall");
    assert_eq!(response.stop_reason, Some(StopReason::Stop));
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

/// Message-only recovery on clean EOF: no terminal event ever arrives, but
/// the completed message is a terminal completion (`Stop`) for the caller —
/// not a max-token truncation error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn message_only_clean_eof_recovery_reaches_caller_terminally() {
    let (addr, hits) = spawn_raw_body_server(|_| {
        let frames = created_and_message_done_frames("whole message");
        event_stream_response(axum::body::Body::from(sse_body(&frames)))
    })
    .await;

    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let handle = SamplerActor::spawn(test_config(addr, 30), RetryPolicy::default(), event_tx);
    let (response, _metrics) = handle
        .submit_and_collect(RequestId::from("req-durable-eof"), user_request("hi"))
        .await
        .expect("message-only recovery must reach the caller as a completion");

    assert_eq!(response.assistant_text(), "whole message");
    assert_eq!(response.stop_reason, Some(StopReason::Stop));
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}
