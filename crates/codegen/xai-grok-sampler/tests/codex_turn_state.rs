//! Codex turn-state wire policy (SPEC §8.2/§8.3, tests per §15.4):
//! the client captures `x-codex-turn-state` from Codex response headers
//! into the response-metadata channel, echoes a request-bound turn-state
//! verbatim on Codex requests (including a resubmit after a 401), never
//! sends it under the xAI profile, sends the body-level `prompt_cache_key`
//! it was given, and never sends `previous_response_id`.

mod support;

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use tokio::net::TcpListener;
use xai_grok_sampler::SamplingClient;
use xai_grok_sampling_types::{
    ApiBackend, ContentPart, ConversationItem, ConversationRequest, ProviderProfile, UserItem,
};

/// One captured request: headers + parsed JSON body.
type Captured = (HeaderMap, serde_json::Value);

/// Response script for one incoming request, applied in arrival order; the
/// last entry repeats. `turn_state` is returned as `x-codex-turn-state`.
#[derive(Clone)]
struct ScriptedResponse {
    status: StatusCode,
    turn_state: Option<&'static str>,
}

const OK_NO_STATE: ScriptedResponse = ScriptedResponse {
    status: StatusCode::OK,
    turn_state: None,
};

async fn spawn_scripted_server(
    script: Vec<ScriptedResponse>,
) -> (String, Arc<Mutex<Vec<Captured>>>) {
    let captured: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&captured);
    let app = Router::new().route(
        "/v1/responses",
        post(move |headers: HeaderMap, body: Bytes| {
            let sink = Arc::clone(&sink);
            let script = script.clone();
            async move {
                let n = {
                    let mut sink = sink.lock().unwrap();
                    let body = serde_json::from_slice::<serde_json::Value>(&body)
                        .unwrap_or(serde_json::Value::Null);
                    sink.push((headers, body));
                    sink.len() - 1
                };
                let step = script
                    .get(n)
                    .or_else(|| script.last())
                    .cloned()
                    .unwrap_or(OK_NO_STATE);
                let mut response = axum::response::Response::builder().status(step.status);
                if let Some(state) = step.turn_state {
                    response = response.header("x-codex-turn-state", state);
                }
                // The canned `{}` body fails typed decode on success paths;
                // only the wire-level exchange matters in these tests.
                response.body("{}".to_owned()).unwrap()
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}/v1"), captured)
}

fn codex_client(base_url: &str) -> SamplingClient {
    let mut cfg = support::test_config(base_url, "seed-key");
    cfg.api_backend = ApiBackend::Responses;
    cfg.provider_profile = ProviderProfile::CODEX;
    SamplingClient::new(cfg).expect("client builds")
}

fn xai_client(base_url: &str) -> SamplingClient {
    let mut cfg = support::test_config(base_url, "seed-key");
    cfg.api_backend = ApiBackend::Responses;
    SamplingClient::new(cfg).expect("client builds")
}

fn request_with(turn_state: Option<&str>, prompt_cache_key: Option<&str>) -> ConversationRequest {
    ConversationRequest {
        items: vec![ConversationItem::User(UserItem {
            content: vec![ContentPart::Text {
                text: Arc::<str>::from("hi"),
            }],
            ..Default::default()
        })],
        turn_state: turn_state.map(str::to_owned),
        prompt_cache_key: prompt_cache_key.map(str::to_owned),
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_response_turn_state_lands_in_metadata_and_next_request_echoes_it() {
    let (base_url, captured) = spawn_scripted_server(vec![
        ScriptedResponse {
            status: StatusCode::OK,
            turn_state: Some("ts-bound-1"),
        },
        OK_NO_STATE,
    ])
    .await;
    let client = codex_client(&base_url);

    // First request of the logical prompt: nothing bound yet.
    let (_stream, metadata, _doom) = client
        .conversation_stream_responses(request_with(None, None))
        .await
        .expect("200 stream handshake succeeds");
    let turn_state = metadata
        .expect("metadata extracted from response headers")
        .turn_state;
    assert_eq!(turn_state.as_deref(), Some("ts-bound-1"));

    // Follow-up request of the same logical prompt echoes the captured value.
    let _ = client
        .conversation_stream_responses(request_with(turn_state.as_deref(), None))
        .await;

    let captured = captured.lock().unwrap();
    assert_eq!(captured.len(), 2);
    assert!(
        captured[0].0.get("x-codex-turn-state").is_none(),
        "first request of a prompt has no turn-state to echo"
    );
    assert_eq!(
        captured[1].0.get("x-codex-turn-state").unwrap(),
        "ts-bound-1"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_resubmits_within_a_prompt_reuse_the_same_turn_state() {
    // Continuation + retry: three sends of the same logical prompt (tool
    // continuation, then a 401 that forces a refresh-and-resubmit, then the
    // resubmit) all carry the identical bound turn-state.
    let (base_url, captured) = spawn_scripted_server(vec![
        OK_NO_STATE,
        ScriptedResponse {
            status: StatusCode::UNAUTHORIZED,
            turn_state: None,
        },
        OK_NO_STATE,
    ])
    .await;
    let client = codex_client(&base_url);
    let request = request_with(Some("ts-bound-1"), None);

    // Tool continuation.
    let _ = client.conversation_stream_responses(request.clone()).await;
    // 401 surfaces as an auth error...
    let err = client
        .conversation_stream_responses(request.clone())
        .await
        .err()
        .expect("401 must error");
    assert!(err.is_auth_error(), "unexpected error: {err}");
    // ...and the post-refresh resubmit (same logical prompt, possibly a
    // rebuilt client) still echoes the same turn-state.
    let rebuilt = codex_client(&base_url);
    let _ = rebuilt.conversation_stream_responses(request).await;

    let captured = captured.lock().unwrap();
    assert_eq!(captured.len(), 3);
    for (i, (headers, _)) in captured.iter().enumerate() {
        assert_eq!(
            headers.get("x-codex-turn-state").unwrap(),
            "ts-bound-1",
            "request {i} must reuse the bound turn-state"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn xai_profile_never_sends_turn_state_header() {
    let (base_url, captured) = spawn_scripted_server(vec![OK_NO_STATE]).await;
    let client = xai_client(&base_url);

    // Even a request that (wrongly) carries a turn-state must not put the
    // header on an xAI wire — the client gates by provider profile.
    let _ = client
        .conversation_stream_responses(request_with(Some("ts-should-not-leak"), None))
        .await;

    let captured = captured.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert!(
        captured[0].0.get("x-codex-turn-state").is_none(),
        "xAI requests must never carry x-codex-turn-state"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_body_carries_prompt_cache_key_and_no_previous_response_id() {
    let (base_url, captured) = spawn_scripted_server(vec![OK_NO_STATE, OK_NO_STATE]).await;
    let client = codex_client(&base_url);

    // Session-stable, provider-tagged key (derived by the shell): the body
    // must carry it verbatim on every request of the session.
    let _ = client
        .conversation_stream_responses(request_with(None, Some("codex-sess-1")))
        .await;
    let _ = client
        .conversation_stream_responses(request_with(Some("ts-1"), Some("codex-sess-1")))
        .await;

    let captured = captured.lock().unwrap();
    assert_eq!(captured.len(), 2);
    for (i, (_, body)) in captured.iter().enumerate() {
        assert_eq!(
            body.get("prompt_cache_key").and_then(|v| v.as_str()),
            Some("codex-sess-1"),
            "request {i} must carry the stable session cache key"
        );
        // The key derives from session identity only — never the bearer.
        assert!(
            !body["prompt_cache_key"]
                .as_str()
                .unwrap()
                .contains("seed-key"),
            "prompt_cache_key must not embed credentials"
        );
        // Full-input HTTP mode: previous_response_id is unsupported and
        // must never be sent.
        assert!(
            body.get("previous_response_id")
                .is_none_or(serde_json::Value::is_null),
            "request {i} must not send previous_response_id: {body}"
        );
    }
}
