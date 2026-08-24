//! Review finding: the `web_search` helper stayed bound to the provider that
//! was active when the session (or its parent) was created, so a session
//! switched onto Codex kept sending Codex-derived queries to the xAI endpoint
//! captured at spawn.
//!
//! The gate is now live session state re-evaluated on every model switch, and
//! it moves both halves together:
//!
//! - the tool definition is dropped from every request built while the gate is
//!   closed, so the model cannot emit a new call; and
//! - the `WebSearchClient` resource is removed from the toolset, so a call the
//!   *previous* provider's model already emitted cannot dispatch either.
//!
//! Both directions are covered here, plus the subagent override direction
//! (xAI parent → Codex child and Codex parent → explicitly pinned xAI child),
//! which resolves through the same session-level gate now that the parent
//! hands over an ungated config plus its provenance pin.

use super::support::*;
use super::*;
use std::sync::Arc;
use xai_grok_sampling_types::ModelProvider;
use xai_grok_tools::implementations::web_search::client::WebSearchClient;

const SEARCH_SLUG: &str = "web-search-gate-test";

fn enabled_web_search_config() -> xai_grok_tools::implementations::WebSearchConfig {
    xai_grok_tools::implementations::WebSearchConfig::Enabled {
        api_key: "xai-search-key".to_owned(),
        base_url: "http://127.0.0.1:9/v1".to_owned(),
        model: SEARCH_SLUG.to_owned(),
        extra_headers: Default::default(),
        alpha_test_key: None,
        allowed_domains: None,
        excluded_domains: None,
    }
}

/// An agent whose toolset really registers `web_search` and really carries a
/// `WebSearchClient` resource — the two things the gate has to move.
async fn agent_with_web_search() -> xai_grok_agent::Agent {
    use xai_grok_tools::computer::local::{LocalFs, LocalTerminalBackend};
    use xai_grok_tools::computer::types::{AsyncFileSystem, TerminalBackend};
    use xai_grok_tools::notification::ToolNotificationHandle;
    use xai_grok_tools::registry::types::{SessionContext, ToolConfig, ToolServerConfig};
    let builder = crate::tools::bridge::ToolBridge::get_builder();
    let fs: std::sync::Arc<dyn AsyncFileSystem> = std::sync::Arc::new(LocalFs);
    let backend: std::sync::Arc<dyn TerminalBackend> =
        std::sync::Arc::new(LocalTerminalBackend::new());
    let ctx = SessionContext {
        backend,
        fs,
        cwd: std::path::PathBuf::from("/tmp"),
        session_folder: std::env::temp_dir().join("grok-web-search-gate"),
        session_env: std::sync::Arc::new(std::collections::HashMap::new()),
        notification_handle: ToolNotificationHandle::noop(),
        owner_session_id: None,
        subagent: None,
        parent_scheduler_handle: None,
        skills: vec![],
        state_path: std::env::temp_dir().join("grok-web-search-gate/state.json"),
        memory_backend: None,
        web_search_config: enabled_web_search_config(),
        web_fetch_config: Default::default(),
        lsp: None,
        image_gen_config: Default::default(),
        video_gen_config: Default::default(),
        app_builder_deployer_config: Default::default(),
        api_key_provider: None,
        auth_provider: None,
        attribution_callback: None,
        system_reminder_tag: xai_grok_tools::reminders::DEFAULT_REMINDER_TAG,
    };
    let config = ToolServerConfig {
        tools: vec![ToolConfig::for_tool::<
            xai_grok_tools::implementations::grok_build::WebSearchTool,
        >()],
        behavior_preset: None,
    };
    let bridge = crate::tools::bridge::ToolBridge::finalize_builder(builder, config, ctx)
        .await
        .expect("finalize_builder should succeed");
    #[allow(clippy::arc_with_non_send_sync)]
    let bridge = std::sync::Arc::new(bridge);
    xai_grok_agent::Agent::new(
        xai_grok_agent::AgentDefinition::default_grok_build(),
        xai_grok_agent::PromptContext::default(),
        String::new(),
        bridge,
        xai_grok_agent::ReminderPolicy::default(),
        xai_grok_agent::CompactionPolicy::default(),
        vec![],
        false,
    )
}

/// A session actor on `spawn_provider` whose web-search helper carries `pin`,
/// wired exactly the way `spawn_session_actor` wires it (including parking the
/// client when the spawn provider already fails the gate).
async fn actor_with_web_search(
    pin: crate::config::AuxModelPin,
    spawn_provider: ModelProvider,
) -> Arc<crate::session::acp_session::SessionActor> {
    let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
    let (persistence_tx, _persistence_rx) = tokio::sync::mpsc::unbounded_channel();
    let (mut actor, _events) =
        create_test_actor_ex(0, 200_000, 80, gateway_tx, persistence_tx).await;
    *actor.agent.borrow_mut() = agent_with_web_search().await;
    let allowed = pin.allows_aux_helper(spawn_provider);
    actor.web_search_pin = pin;
    actor.web_search_provider_allowed = std::cell::Cell::new(allowed);
    let actor = Arc::new(actor);
    if !allowed {
        actor.park_or_restore_web_search_client().await;
    }
    actor
}

async fn web_search_client_installed(actor: &crate::session::acp_session::SessionActor) -> bool {
    let bridge = actor.agent.borrow().tool_bridge().clone();
    bridge
        .toolset()
        .get_resource_cloned::<WebSearchClient>()
        .await
        .is_some()
}

async fn web_search_tool_offered(actor: &crate::session::acp_session::SessionActor) -> bool {
    actor
        .prepare_tool_definitions_inner()
        .await
        .iter()
        .any(|td| td.function.name == "web_search")
}

/// Both halves of the gate, asserted together.
async fn assert_web_search(
    actor: &crate::session::acp_session::SessionActor,
    expected: bool,
    context: &str,
) {
    assert_eq!(
        web_search_tool_offered(actor).await,
        expected,
        "{context}: tool definition offered to the model"
    );
    assert_eq!(
        web_search_client_installed(actor).await,
        expected,
        "{context}: WebSearchClient available to dispatch"
    );
}

/// Scenario 1 of the finding: an UNPINNED xAI session switched to Codex must
/// lose the helper it captured at spawn — and get it back on the way home.
#[tokio::test(flavor = "current_thread")]
async fn unpinned_session_loses_web_search_when_switched_to_codex_and_regains_it() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor =
                actor_with_web_search(crate::config::AuxModelPin::Unpinned, ModelProvider::Xai)
                    .await;
            assert_web_search(&actor, true, "control: unpinned xAI session at spawn").await;

            actor
                .apply_web_search_provider_gate(ModelProvider::Codex)
                .await;
            assert_web_search(
                &actor,
                false,
                "after switching to Codex, the xAI helper captured at spawn",
            )
            .await;

            // Reversible: the gate follows the effective provider, it does not
            // latch. The restored client is the SAME one that was parked.
            actor
                .apply_web_search_provider_gate(ModelProvider::Xai)
                .await;
            assert_web_search(&actor, true, "after switching back to xAI").await;
        })
        .await;
}

/// A user-explicit pin IS cross-provider consent, so the same switch keeps the
/// helper. (A remote-settings pin is not — see `AuxModelPin::Remote`.)
#[tokio::test(flavor = "current_thread")]
async fn explicitly_pinned_session_keeps_web_search_across_a_codex_switch() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = actor_with_web_search(
                crate::config::AuxModelPin::Pinned(SEARCH_SLUG.to_owned()),
                ModelProvider::Xai,
            )
            .await;
            actor
                .apply_web_search_provider_gate(ModelProvider::Codex)
                .await;
            assert_web_search(&actor, true, "an explicit pin is the user's consent").await;

            // A remote-settings pin names the same model but is not consent.
            let remote = actor_with_web_search(
                crate::config::AuxModelPin::Remote(SEARCH_SLUG.to_owned()),
                ModelProvider::Xai,
            )
            .await;
            remote
                .apply_web_search_provider_gate(ModelProvider::Codex)
                .await;
            assert_web_search(
                &remote,
                false,
                "a remote pin is service config, not consent",
            )
            .await;
        })
        .await;
}

/// Scenario 2 of the finding, both directions. A child's provider comes from
/// its own effective sampling config, so the gate that runs at child spawn is
/// the same one tested above — parametrized by the child's provider, not the
/// parent's.
#[tokio::test(flavor = "current_thread")]
async fn child_override_gates_web_search_on_the_child_provider_in_both_directions() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            // xAI parent, Codex child via a model override, unpinned helper:
            // the child must NOT inherit the parent's enabled xAI helper.
            let codex_child =
                actor_with_web_search(crate::config::AuxModelPin::Unpinned, ModelProvider::Codex)
                    .await;
            assert_web_search(
                &codex_child,
                false,
                "a Codex child of an xAI parent must not inherit the xAI helper",
            )
            .await;

            // Codex parent, explicitly pinned xAI child: previously disabled
            // by the parent-side snapshot; the child's own provider allows it.
            let xai_child = actor_with_web_search(
                crate::config::AuxModelPin::Pinned(SEARCH_SLUG.to_owned()),
                ModelProvider::Xai,
            )
            .await;
            assert_web_search(
                &xai_child,
                true,
                "an xAI child of a Codex parent must keep its own helper",
            )
            .await;

            // ...and an unpinned xAI child of a Codex parent too: the parent's
            // provider never enters the decision.
            let plain_xai_child =
                actor_with_web_search(crate::config::AuxModelPin::Unpinned, ModelProvider::Xai)
                    .await;
            assert_web_search(
                &plain_xai_child,
                true,
                "an xAI child is provider-consistent regardless of the parent",
            )
            .await;
        })
        .await;
}

/// The gate is idempotent: repeating the same provider neither double-parks
/// nor loses the parked client.
#[tokio::test(flavor = "current_thread")]
async fn repeated_gate_application_is_idempotent() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor =
                actor_with_web_search(crate::config::AuxModelPin::Unpinned, ModelProvider::Xai)
                    .await;
            for _ in 0..3 {
                actor
                    .apply_web_search_provider_gate(ModelProvider::Codex)
                    .await;
            }
            assert_web_search(&actor, false, "repeated close").await;
            for _ in 0..3 {
                actor
                    .apply_web_search_provider_gate(ModelProvider::Xai)
                    .await;
            }
            assert_web_search(&actor, true, "repeated reopen").await;
        })
        .await;
}
