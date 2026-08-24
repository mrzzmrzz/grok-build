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

/// Build a session actor whose chat state is an OAuth Codex session on the
/// live-only slug (identity anchor present), as the 401 arm sees it.
async fn actor_on_codex_oauth_session() -> Arc<crate::session::acp_session::SessionActor> {
    let credentials = live_codex_credentials();
    let fingerprint = crate::codex_models::account_fingerprint(&credentials).unwrap();
    let mgr = manager_with_live_codex_slug(fingerprint);
    let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
    let (persistence_tx, _persistence_rx) = tokio::sync::mpsc::unbounded_channel();
    let (mut actor, _events) =
        create_test_actor_ex(0, 200_000, 80, gateway_tx, persistence_tx).await;
    actor.models_manager = mgr;
    let actor = Arc::new(actor);
    let entry = actor.models_manager.models().get(LIVE_SLUG).cloned().unwrap();
    let creds = resolve_credentials(&entry, None);
    let sampler_config = sampling_config_for_model(&entry, creds, None, None, None, None);
    let mut headers = sampler_config.extra_headers.clone();
    crate::codex_auth::set_oauth_identity_anchor(&mut headers, Some(&credentials));
    let mut chat_cfg = actor
        .chat_state_handle
        .get_sampling_config()
        .await
        .expect("chat state has a sampling config");
    chat_cfg.model = LIVE_SLUG.to_owned();
    chat_cfg.extra_headers = headers;
    actor.chat_state_handle.update_sampling_config(chat_cfg);
    actor.invalidate_model_auth_memo();
    actor
}

/// Review finding: a logical request gets ONE Codex 401 recovery. The first
/// 401 forces an OAuth refresh and buys a single replay; a second 401 on the
/// same request fails closed without refreshing again; the next request — a
/// tool continuation after a response landed, or a new prompt — opens a fresh
/// allowance through the same reset the turn loop calls.
#[tokio::test(flavor = "current_thread")]
async fn codex_401_recovery_refreshes_once_per_logical_request() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = actor_on_codex_oauth_session().await;
            let refreshes = std::cell::Cell::new(0u32);
            let refresh = || {
                refreshes.set(refreshes.get() + 1);
                std::future::ready(anyhow::Ok(Some(live_codex_credentials())))
            };

            assert!(
                actor.try_codex_401_recovery_with(refresh).await,
                "the first 401 refreshes and replays once"
            );
            assert_eq!(refreshes.get(), 1);

            assert!(
                !actor.try_codex_401_recovery_with(refresh).await,
                "a second 401 on the same request must fail closed"
            );
            assert_eq!(
                refreshes.get(),
                1,
                "the exhausted request must not refresh again"
            );

            // The terminal path is what a spent request now takes.
            let outcome = actor.handle_sampling_failure(codex_401_error(), 0).await;
            assert!(
                outcome.is_err(),
                "a Codex 401 with no recovery left must surface terminally"
            );

            // A sampler response landing opens the next request (the turn
            // loop resets here alongside `AuthRetrySchedule::reset_on_success`).
            actor.reset_codex_401_recovery();
            assert!(
                actor.try_codex_401_recovery_with(refresh).await,
                "a token that rotates during a tool call must still be \
                 repairable once on the continuation request"
            );
            assert_eq!(refreshes.get(), 2);
            assert!(
                !actor.try_codex_401_recovery_with(refresh).await,
                "that continuation request is bounded to one refresh too"
            );
            assert_eq!(refreshes.get(), 2);
        })
        .await;
}

/// Actor whose image-describe helper is a resolvable catalog model with its
/// own key (tier-1 aux resolution), pinned per `pin`.
async fn actor_with_image_describe_pin(
    pin: crate::config::AuxModelPin,
) -> Arc<crate::session::acp_session::SessionActor> {
    const AUX_SLUG: &str = "image-describe-aux-test";
    let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
    let (persistence_tx, _persistence_rx) = tokio::sync::mpsc::unbounded_channel();
    let (mut actor, _events) =
        create_test_actor_ex(0, 200_000, 80, gateway_tx, persistence_tx).await;
    actor.models_manager.insert_test_entry(
        AUX_SLUG,
        crate::agent::config::ModelEntry {
            info: crate::agent::config::ModelInfo::fallback(AUX_SLUG),
            api_key: Some("aux-key".to_owned()),
            env_key: None,
            auth_provider: None,
            api_base_url: None,
        },
    );
    actor.image_description_model = AUX_SLUG.to_owned();
    actor.image_description_pin = pin;
    Arc::new(actor)
}

/// Review finding: a Codex session must not ship its images to the xAI
/// describe helper unless the user explicitly pinned that model. `None` sends
/// the caller back to the session model, which is provider-consistent.
#[tokio::test(flavor = "current_thread")]
async fn image_describe_helper_is_provider_gated_by_the_pin() {
    use xai_grok_sampling_types::ModelProvider;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let unpinned =
                actor_with_image_describe_pin(crate::config::AuxModelPin::Unpinned).await;
            assert!(
                unpinned
                    .resolve_image_describe_sampler_config(ModelProvider::Xai)
                    .await
                    .is_some(),
                "control: an xAI session still resolves the describe helper"
            );
            assert!(
                unpinned
                    .resolve_image_describe_sampler_config(ModelProvider::Codex)
                    .await
                    .is_none(),
                "an unpinned xAI helper must not describe a Codex session's images"
            );

            let pinned = actor_with_image_describe_pin(crate::config::AuxModelPin::Pinned(
                "image-describe-aux-test".to_owned(),
            ))
            .await;
            assert!(
                pinned
                    .resolve_image_describe_sampler_config(ModelProvider::Codex)
                    .await
                    .is_some(),
                "an explicit pin is the user's cross-provider consent"
            );
        })
        .await;
}

/// A real `handle_set_session_model` onto a live Codex model must, in one
/// step, close every cross-provider aux gate the session owns:
///
/// - the monotonic Codex provenance latch is set SYNCHRONOUSLY, so an
///   already-created `PromptTraceContext` is revoked at its next upload
///   boundary rather than at the next actor round-trip (review finding 5);
/// - the `web_search` gate is re-evaluated against the new provider, so the
///   helper resolved from the spawn provider stops serving the session
///   (review finding 1); and
/// - the Auto-mode classifier refuses a non-locally-pinned aux model, with a
///   remote-settings slug counting as unpinned (review finding 2).
#[tokio::test(flavor = "current_thread")]
async fn switching_to_codex_closes_every_cross_provider_aux_gate() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let credentials = live_codex_credentials();
            let fingerprint = crate::codex_models::account_fingerprint(&credentials)
                .expect("synthetic credentials have a stable identity");
            let mgr = manager_with_live_codex_slug(fingerprint);
            mgr.set_current_model_id(acp::ModelId::new(LIVE_SLUG));
            let entry = mgr
                .models()
                .get(LIVE_SLUG)
                .cloned()
                .expect("live entry present in merged catalog");
            let creds = resolve_credentials(&entry, None);
            let mut sampler_config =
                sampling_config_for_model(&entry, creds, None, None, None, None);
            crate::codex_auth::set_oauth_identity_anchor(
                &mut sampler_config.extra_headers,
                Some(&credentials),
            );

            let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
            let (persistence_tx, _persistence_rx) = tokio::sync::mpsc::unbounded_channel();
            let (mut actor, _events) =
                create_test_actor_ex(0, 200_000, 80, gateway_tx, persistence_tx).await;
            actor.models_manager = mgr;
            // The session started on xAI with an unpinned search helper.
            actor.web_search_pin = crate::config::AuxModelPin::Unpinned;
            actor.web_search_provider_allowed = std::cell::Cell::new(true);
            let actor = Arc::new(actor);

            // Control: before the switch every gate is open.
            assert!(!actor.chat_state_handle.ever_used_codex_now());
            assert!(actor.web_search_provider_allowed.get());
            assert!(
                actor
                    .auto_classifier_aux_allowed(&crate::config::AuxModelPin::Unpinned)
                    .await,
                "an xAI session may use the xAI classifier helper"
            );

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

            assert!(
                actor.chat_state_handle.ever_used_codex_now(),
                "the provenance latch must be readable synchronously right after \
                 the switch — that is what revokes an in-flight trace context"
            );
            assert!(
                !actor.web_search_provider_allowed.get(),
                "the xAI web-search helper captured at spawn must stop serving a \
                 Codex session"
            );
            assert!(
                !actor
                    .prepare_tool_definitions_inner()
                    .await
                    .iter()
                    .any(|td| td.function.name == "web_search"),
                "and it must be absent from the very next request's tool list"
            );
            assert!(
                !actor
                    .auto_classifier_aux_allowed(&crate::config::AuxModelPin::Unpinned)
                    .await,
                "an unpinned classifier helper must not serve a Codex session"
            );
            assert!(
                !actor
                    .auto_classifier_aux_allowed(&crate::config::AuxModelPin::Remote(
                        "remote-classifier".to_owned()
                    ))
                    .await,
                "a remote-settings classifier slug is service config, not consent"
            );
            assert!(
                actor
                    .auto_classifier_aux_allowed(&crate::config::AuxModelPin::Pinned(
                        "local-classifier".to_owned()
                    ))
                    .await,
                "a local pin IS the user's cross-provider consent"
            );
        })
        .await;
}

fn codex_401_error() -> xai_grok_sampler::SamplingErrorInfo {
    xai_grok_sampler::SamplingErrorInfo {
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
    }
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

    let outcome = actor.handle_sampling_failure(codex_401_error(), 0).await;
    assert!(
        outcome.is_err(),
        "an unrecoverable Codex 401 must surface terminally, not enter xAI recovery"
    );
    }).await;
}
