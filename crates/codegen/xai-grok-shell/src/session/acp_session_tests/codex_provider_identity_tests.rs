//! End-to-end provider identity for live catalog-only Codex models (review
//! finding: a model that exists only in the authenticated Codex catalog must
//! never be reconstructed as an xAI request). The flow under test: inject a
//! Codex-only slug into the live catalog, select it through the model
//! manager, apply the switch to a session, and reconstruct the sampler
//! config — the reconstruction must keep the Codex profile, mount the Codex
//! bearer resolver, target the Codex endpoint, and carry the reserved
//! identity headers.

use super::support::*;
use super::*;
use crate::agent::config::{resolve_credentials, sampling_config_for_model};
use crate::agent::models::{ModelsManager, ModelsManagerBuilder, codex_catalog_entries};
use crate::codex_models::{CodexCatalogModel, CodexModelVisibility, CodexModelsCatalog};
use indexmap::IndexMap;
use std::sync::Arc;

const LIVE_SLUG: &str = "gpt-6-live-only";

fn live_codex_credentials() -> crate::codex_auth::CodexCredentials {
    crate::codex_auth::CodexCredentials {
        access_token: "live-access-token".to_owned(),
        account_id: Some("acct-live".to_owned()),
        chatgpt_user_id: Some("user-live".to_owned()),
        email: Some("live@example.com".to_owned()),
        plan_type: Some("pro".to_owned()),
        is_workspace_account: false,
        account_is_fedramp: false,
    }
}

fn live_catalog_model() -> CodexCatalogModel {
    CodexCatalogModel {
        slug: LIVE_SLUG.to_owned(),
        display_name: "GPT-6 Live Only".to_owned(),
        description: Some("Live catalog-only Codex model".to_owned()),
        priority: 1,
        visibility: CodexModelVisibility::List,
        server_supported_in_api: true,
        supported_in_api: false,
        context_window: Some(353_400),
        raw_context_window: Some(372_000),
        effective_context_window_percent: 95,
        default_reasoning_level: Some("medium".to_owned()),
        supported_reasoning_levels: vec![],
        supports_search_tool: false,
        tool_mode: None,
    }
}

/// Manager whose catalog holds the live Codex-only slug, published through
/// the real fenced path under an injected account fingerprint.
fn manager_with_live_codex_slug(fingerprint: String) -> ModelsManager {
    let cfg = crate::agent::config::Config::default();
    let auth_manager = Arc::new(crate::auth::AuthManager::new(
        std::path::Path::new("/tmp/does-not-exist-codex-e2e"),
        crate::auth::GrokComConfig::default(),
    ));
    let probe_fingerprint = fingerprint.clone();
    let mgr = ModelsManagerBuilder::new(
        None,
        crate::agent::models::resolve_model_catalog(&cfg, None),
        acp::ModelId::new("grok-4.6"),
        auth_manager,
        cfg,
    )
    .codex_account(Arc::new(move || Some(probe_fingerprint.clone())))
    .build();
    let catalog = CodexModelsCatalog::for_test(vec![live_catalog_model()], None, fingerprint.clone());
    assert!(
        mgr.publish_codex_models_for_test(codex_catalog_entries(&catalog), fingerprint),
        "live catalog must publish"
    );
    mgr
}

#[tokio::test(flavor = "current_thread")]
async fn live_codex_only_model_keeps_provider_identity_through_reconstruction() {
    let local = tokio::task::LocalSet::new();
    local.run_until(async {
    let credentials = live_codex_credentials();
    let fingerprint = crate::codex_models::account_fingerprint(&credentials)
        .expect("synthetic credentials have a stable identity");
    let mgr = manager_with_live_codex_slug(fingerprint);

    // Select through the model manager, as the picker does.
    mgr.set_current_model_id(acp::ModelId::new(LIVE_SLUG));
    assert!(
        mgr.available().keys().any(|id| id.0.as_ref() == LIVE_SLUG),
        "the live Codex-only slug must be visible to the picker"
    );

    // Build the switch config the way the production model-switch handler
    // does (entry from the manager catalog, catalog-routed credentials),
    // then simulate the logged-in OAuth state deterministically: production
    // reads the credential file, tests must not.
    let entry = mgr
        .models()
        .get(LIVE_SLUG)
        .cloned()
        .expect("live entry present in merged catalog");
    let creds = resolve_credentials(&entry, None);
    assert!(creds.api_key.is_none(), "no static key for an OAuth entry");
    let mut sampler_config = sampling_config_for_model(&entry, creds, None, None, None, None);
    crate::codex_auth::set_oauth_identity_anchor(
        &mut sampler_config.extra_headers,
        Some(&credentials),
    );

    let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
    let (persistence_tx, _persistence_rx) = tokio::sync::mpsc::unbounded_channel();
    let (mut actor, _events) =
        create_test_actor_ex(0, 200_000, 80, gateway_tx, persistence_tx).await;
    actor.models_manager = mgr;
    let actor = std::sync::Arc::new(actor);

    // Apply the switch to the session, then reconstruct the sampler config
    // exactly as the next turn would.
    actor
        .handle_set_session_model(
            sampler_config,
            /* use_concise */ false,
            /* is_family_switch */ false,
            /* apply_prompt_override */ false,
            /* skip_prompt_rewrite */ false,
            /* auto_compact_threshold_percent */ 80,
        )
        .await
        .expect("model switch applies");
    let reconstructed = actor.reconstruct_full_config().await;

    assert_eq!(reconstructed.model, LIVE_SLUG);
    assert_eq!(
        reconstructed.provider_profile,
        xai_grok_sampling_types::ProviderProfile::CODEX,
        "a live catalog-only Codex model must never fall back to the xAI profile"
    );
    assert_eq!(
        reconstructed.base_url,
        crate::codex_auth::inference_base_url(),
        "inference must target the Codex endpoint"
    );
    assert!(
        reconstructed.bearer_resolver.is_some(),
        "the Codex OAuth bearer resolver must be mounted"
    );
    assert!(
        reconstructed.api_key.is_none(),
        "an OAuth session carries no static key"
    );
    assert!(
        reconstructed.user_id.is_none(),
        "xAI identity must never ride a Codex request"
    );
    assert_eq!(
        reconstructed.extra_headers.get("originator").map(String::as_str),
        Some(crate::codex_auth::CODEX_ORIGINATOR),
        "the Codex wire identity header must be present"
    );
    assert!(
        crate::codex_auth::has_oauth_identity_anchor(&reconstructed.extra_headers),
        "the reserved identity anchor headers must survive reconstruction"
    );
    }).await;
}

/// The manager-first auth-facts lookup must not disturb xAI models: a plain
/// catalog model still reconstructs under the xAI profile.
#[tokio::test(flavor = "current_thread")]
async fn xai_model_reconstruction_stays_on_the_xai_profile() {
    let local = tokio::task::LocalSet::new();
    local.run_until(async {
    let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
    let (persistence_tx, _persistence_rx) = tokio::sync::mpsc::unbounded_channel();
    let (actor, _events) = create_test_actor_ex(0, 200_000, 80, gateway_tx, persistence_tx).await;
    let actor = std::sync::Arc::new(actor);
    let reconstructed = actor.reconstruct_full_config().await;
    assert_eq!(
        reconstructed.provider_profile,
        xai_grok_sampling_types::ProviderProfile::XAI
    );
    assert!(reconstructed.bearer_resolver.is_none());
    }).await;
}

/// A Codex 401 must never enter the xAI session-token recovery: with no
/// OAuth anchor (and no on-disk credential to read in tests), the recovery
/// helper fails closed without touching any xAI store.
#[tokio::test(flavor = "current_thread")]
async fn codex_401_recovery_fails_closed_without_oauth_anchor() {
    let local = tokio::task::LocalSet::new();
    local.run_until(async {
    let credentials = live_codex_credentials();
    let fingerprint = crate::codex_models::account_fingerprint(&credentials).unwrap();
    let mgr = manager_with_live_codex_slug(fingerprint);
    let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
    let (persistence_tx, _persistence_rx) = tokio::sync::mpsc::unbounded_channel();
    let (mut actor, _events) =
        create_test_actor_ex(0, 200_000, 80, gateway_tx, persistence_tx).await;
    actor.models_manager = mgr;
    let actor = std::sync::Arc::new(actor);

    // Explicit-key style Codex session: no identity anchor in the headers.
    let entry = actor.models_manager.models().get(LIVE_SLUG).cloned().unwrap();
    let creds = resolve_credentials(&entry, None);
    let sampler_config = sampling_config_for_model(&entry, creds, None, None, None, None);
    let mut headers = sampler_config.extra_headers.clone();
    crate::codex_auth::set_oauth_identity_anchor(&mut headers, None);
    let mut chat_cfg = actor
        .chat_state_handle
        .get_sampling_config()
        .await
        .expect("chat state has a sampling config");
    chat_cfg.model = LIVE_SLUG.to_owned();
    chat_cfg.extra_headers = headers;
    actor.chat_state_handle.update_sampling_config(chat_cfg);
    actor.invalidate_model_auth_memo();

    let error = xai_grok_sampler::SamplingErrorInfo {
        kind: xai_grok_sampler::SamplingErrorKind::Auth,
        status_code: Some(401),
        message: "unauthorized".to_owned(),
        is_retryable: false,
        retry_after_secs: None,
        should_retry: None,
        error_code: None,
        model_metadata: None,
        empty_response_context: None,
        doom_loop_triggers: None,
        doom_loop_aborted_at_chunk: None,
        credential: xai_grok_sampling_types::SentCredential::Sent,
    };
    let outcome = actor.handle_sampling_failure(error, 0).await;
    assert!(
        outcome.is_err(),
        "an unrecoverable Codex 401 must surface terminally, not enter xAI recovery"
    );
    }).await;
}
