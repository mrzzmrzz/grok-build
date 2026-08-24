//! Codex turn-state lifecycle and provider-aware prompt-cache affinity.
//!
//! Pure policy functions, kept free of actor state so the lifecycle rules
//! (SPEC §8.2/§8.3) are unit-testable:
//!
//! * `x-codex-turn-state` is an opaque token a Codex response returns; every
//!   follow-up request of the same *logical prompt* (retry, tool
//!   continuation, sampler-client rebuild, 401-refresh resubmit) must echo
//!   it verbatim. A new user prompt or a model/provider switch drops it.
//!   It is never persisted with the session, and each session actor owns
//!   its own slot, so concurrent prompts (subagents) cannot cross-bind.
//! * `prompt_cache_key` is a longer-lived routing affinity derived from the
//!   stable session identity — constant for the whole session, never
//!   containing credentials or prompt content, and provider-distinguished
//!   so a provider switch never reuses the other provider's cache key.

use xai_grok_sampling_types::{ConversationRequest, ModelProvider};

/// Derive the sticky prompt-cache routing key for a session.
///
/// * xAI: the bare session id — the pre-existing behavior
///   (`parent_cached_request` side-calls; main turns fall back to
///   `x_grok_conv_id` on the wire when the field stays unset).
/// * Codex: the session id under a provider prefix, so an xAI session that
///   switches to Codex (or back) never reuses the other provider's key.
///
/// The key is a pure function of provider + session id: stable within a
/// session, and free of bearers and user prompt content by construction.
pub(crate) fn derive_prompt_cache_key(provider: ModelProvider, session_id: &str) -> String {
    match provider {
        ModelProvider::Xai => session_id.to_string(),
        ModelProvider::Codex => format!("codex-{session_id}"),
    }
}

/// Stamp turn affinity onto an outgoing main-turn request.
///
/// Codex: bind the stored turn-state (if any) and pin `prompt_cache_key`
/// to the session-derived key. xAI: strip any turn-state (an xAI request
/// must never carry `x-codex-turn-state`; the sampler client also gates
/// the header by profile) and leave `prompt_cache_key` untouched so the
/// existing conv-id fallback behavior is preserved.
pub(crate) fn apply_turn_affinity(
    request: &mut ConversationRequest,
    provider: ModelProvider,
    session_id: &str,
    turn_state: Option<String>,
) {
    match provider {
        ModelProvider::Codex => {
            request.turn_state = turn_state.filter(|s| !s.is_empty());
            request.prompt_cache_key = Some(derive_prompt_cache_key(provider, session_id));
        }
        ModelProvider::Xai => {
            request.turn_state = None;
        }
    }
}

/// Fold a response-observed turn-state into the logical prompt's slot.
/// Latest non-empty observation wins: the first successful response binds
/// the state, and a continuation response that returns a rotated token
/// rebinds so the next request echoes the freshest value.
pub(crate) fn observe_turn_state(slot: &mut Option<String>, observed: String) {
    if !observed.is_empty() {
        *slot = Some(observed);
    }
}

/// Drop the turn-state binding. Called when a new user prompt starts a new
/// logical prompt, and on any model switch (Codex -> xAI must not leak the
/// token; a different Codex model's binding is stale too).
pub(crate) fn clear_turn_state(slot: &mut Option<String>) {
    slot.take();
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_grok_sampling_types::ConversationItem;

    fn request() -> ConversationRequest {
        ConversationRequest {
            items: vec![ConversationItem::user("hi")],
            ..Default::default()
        }
    }

    #[test]
    fn codex_cache_key_is_stable_within_a_session() {
        let a = derive_prompt_cache_key(ModelProvider::Codex, "sess-1");
        let b = derive_prompt_cache_key(ModelProvider::Codex, "sess-1");
        assert_eq!(a, b, "same session must derive the same key");
    }

    #[test]
    fn cache_key_differs_across_providers_for_the_same_session() {
        let xai = derive_prompt_cache_key(ModelProvider::Xai, "sess-1");
        let codex = derive_prompt_cache_key(ModelProvider::Codex, "sess-1");
        assert_ne!(
            xai, codex,
            "a provider switch must not reuse the other provider's cache affinity"
        );
        // xAI keeps the historical bare-session-id key.
        assert_eq!(xai, "sess-1");
    }

    #[test]
    fn cache_key_carries_only_provider_tag_and_session_identity() {
        // No bearer / prompt content can enter: the key is exactly the
        // provider prefix plus the session id.
        let key = derive_prompt_cache_key(ModelProvider::Codex, "sess-1");
        assert_eq!(key, "codex-sess-1");
    }

    #[test]
    fn codex_apply_binds_turn_state_and_cache_key() {
        let mut req = request();
        apply_turn_affinity(
            &mut req,
            ModelProvider::Codex,
            "sess-1",
            Some("ts-abc".to_owned()),
        );
        assert_eq!(req.turn_state.as_deref(), Some("ts-abc"));
        assert_eq!(req.prompt_cache_key.as_deref(), Some("codex-sess-1"));
    }

    #[test]
    fn codex_apply_without_bound_state_sends_none() {
        let mut req = request();
        apply_turn_affinity(&mut req, ModelProvider::Codex, "sess-1", None);
        assert_eq!(req.turn_state, None);
        assert_eq!(req.prompt_cache_key.as_deref(), Some("codex-sess-1"));
    }

    #[test]
    fn xai_apply_strips_turn_state_and_leaves_cache_key_alone() {
        let mut req = request();
        req.turn_state = Some("stale-from-somewhere".to_owned());
        apply_turn_affinity(
            &mut req,
            ModelProvider::Xai,
            "sess-1",
            Some("ts-abc".to_owned()),
        );
        assert_eq!(req.turn_state, None, "xAI requests never carry turn-state");
        assert_eq!(
            req.prompt_cache_key, None,
            "xAI main turns keep the conv-id fallback (field unset)"
        );
    }

    #[test]
    fn observe_binds_first_success_and_rebinds_on_rotation() {
        let mut slot = None;
        observe_turn_state(&mut slot, "ts-1".to_owned());
        assert_eq!(slot.as_deref(), Some("ts-1"));
        // Retry / continuation echoing the same value is a no-op rebind.
        observe_turn_state(&mut slot, "ts-1".to_owned());
        assert_eq!(slot.as_deref(), Some("ts-1"));
        // A rotated token from a later response of the same prompt wins.
        observe_turn_state(&mut slot, "ts-2".to_owned());
        assert_eq!(slot.as_deref(), Some("ts-2"));
    }

    #[test]
    fn observe_ignores_empty_header_values() {
        let mut slot = Some("ts-1".to_owned());
        observe_turn_state(&mut slot, String::new());
        assert_eq!(slot.as_deref(), Some("ts-1"));
    }

    #[test]
    fn new_prompt_and_model_switch_clear_the_binding() {
        let mut slot = Some("ts-1".to_owned());
        clear_turn_state(&mut slot);
        assert_eq!(slot, None);
        // Idempotent: queue-accept + turn-start double clear is fine.
        clear_turn_state(&mut slot);
        assert_eq!(slot, None);
    }
}
