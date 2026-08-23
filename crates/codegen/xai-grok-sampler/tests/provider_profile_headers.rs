//! Provider-profile wire policy: a Codex-profile client sends Bearer +
//! ChatGPT-Account-ID + originator and no `x-grok-*` header at all; the
//! default xAI profile keeps the `x-grok-*` set and never sends Codex
//! account headers, even when its resolver reports account facts.

mod support;

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::http::HeaderMap;
use axum::routing::post;
use tokio::net::TcpListener;
use xai_grok_sampler::{BearerResolver, ResolvedBearerAuth, SamplingClient};
use xai_grok_sampling_types::{
    ApiBackend, ContentPart, ConversationItem, ConversationRequest, ProviderProfile, UserItem,
};

/// Resolver returning a full account snapshot, standing in for the shell's
/// Codex OAuth resolver.
#[derive(Debug)]
struct AccountResolver;

impl BearerResolver for AccountResolver {
    fn current_bearer(&self) -> Option<String> {
        Some("live-token".to_owned())
    }

    fn resolve_auth(&self) -> Option<ResolvedBearerAuth> {
        Some(ResolvedBearerAuth {
            bearer: "live-token".to_owned(),
            account_id: Some("acct-1".to_owned()),
            fedramp: true,
        })
    }
}

async fn spawn_capture_server(path: &'static str) -> (String, Arc<Mutex<Option<HeaderMap>>>) {
    let captured: Arc<Mutex<Option<HeaderMap>>> = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&captured);
    let app = Router::new().route(
        path,
        post(move |headers: HeaderMap| {
            let sink = Arc::clone(&sink);
            async move {
                *sink.lock().unwrap() = Some(headers);
                "{}"
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

fn one_user_request() -> ConversationRequest {
    ConversationRequest {
        items: vec![ConversationItem::User(UserItem {
            content: vec![ContentPart::Text {
                text: Arc::<str>::from("hi"),
            }],
            ..Default::default()
        })],
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_profile_sends_account_headers_and_no_x_grok() {
    let (base_url, captured) = spawn_capture_server("/v1/responses").await;

    let mut cfg = support::test_config(&base_url, "seed-key");
    cfg.api_backend = ApiBackend::Responses;
    cfg.provider_profile = ProviderProfile::CODEX;
    cfg.bearer_resolver = Some(Arc::new(AccountResolver));
    // The shell places `originator` in extra_headers; x-grok-named entries
    // (like its internal codex identity anchors) must never reach the wire.
    cfg.extra_headers
        .insert("originator".into(), "codex_cli_rs".into());
    cfg.extra_headers
        .insert("x-grok-build-codex-auth-anchor".into(), "1".into());
    cfg.extra_headers
        .insert("x-grok-client-mode".into(), "tui".into());
    // Profile-gated built-ins that render as x-grok-* for xAI.
    cfg.client_version = Some("1.2.3".into());
    cfg.deployment_id = Some("dep-1".into());
    cfg.user_id = Some("user-1".into());
    cfg.client_identifier = Some("cli".into());

    let client = SamplingClient::new(cfg).expect("client builds");
    // The canned `{}` body fails typed decode; only the request matters.
    let _ = client.conversation_responses(one_user_request()).await;

    let headers = captured.lock().unwrap().take().expect("request captured");
    assert_eq!(
        headers.get("authorization").unwrap().to_str().unwrap(),
        "Bearer live-token"
    );
    assert_eq!(headers.get("chatgpt-account-id").unwrap(), "acct-1");
    assert_eq!(headers.get("x-openai-fedramp").unwrap(), "true");
    assert_eq!(headers.get("originator").unwrap(), "codex_cli_rs");
    let grok_named: Vec<String> = headers
        .keys()
        .map(|name| name.as_str().to_owned())
        .filter(|name| name.starts_with("x-grok"))
        .collect();
    assert!(
        grok_named.is_empty(),
        "codex requests must carry no x-grok-* headers, found: {grok_named:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn xai_profile_keeps_x_grok_headers_and_no_codex_account_headers() {
    let (base_url, captured) = spawn_capture_server("/v1/responses").await;

    let mut cfg = support::test_config(&base_url, "seed-key");
    cfg.api_backend = ApiBackend::Responses;
    // Default profile is xAI; a resolver with account facts must still not
    // produce Codex account headers on an xAI request.
    cfg.bearer_resolver = Some(Arc::new(AccountResolver));
    cfg.client_version = Some("1.2.3".into());

    let client = SamplingClient::new(cfg).expect("client builds");
    let _ = client.conversation_responses(one_user_request()).await;

    let headers = captured.lock().unwrap().take().expect("request captured");
    assert_eq!(
        headers.get("authorization").unwrap().to_str().unwrap(),
        "Bearer live-token"
    );
    assert!(headers.get("x-grok-client-identifier").is_some());
    assert!(headers.get("x-grok-client-version").is_some());
    assert!(headers.get("x-grok-conv-id").is_some());
    assert!(headers.get("chatgpt-account-id").is_none());
    assert!(headers.get("x-openai-fedramp").is_none());
    assert!(headers.get("originator").is_none());
}
