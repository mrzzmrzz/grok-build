//! Live Codex catalog → model-manager entries.
//!
//! Single mapping site from [`CodexCatalogModel`] to [`ModelEntry`], plus the
//! merge rule that folds live Codex entries into the resolved catalog without
//! ever displacing an xAI entry (user `[model.*]` overrides are applied later
//! by `resolve_model_list`, so they still win field-by-field).

use super::*;
use crate::codex_models::{CodexCatalogModel, CodexModelsCatalog};
use crate::sampling::ApiBackend;
use std::num::NonZeroU64;
use xai_grok_sampling_types::ModelProvider;

/// Agent type driven by Codex models (bundled builtin definition).
const CODEX_AGENT_TYPE: &str = "codex";

/// Map a live Codex catalog to manager entries, honoring the catalog's own
/// visibility (`list`-visible models only) and its priority order.
pub(crate) fn codex_catalog_entries(catalog: &CodexModelsCatalog) -> IndexMap<String, ModelEntry> {
    codex_entries_from_models(catalog.list_visible_models())
}

/// Core of [`codex_catalog_entries`], parameterized for unit tests; keeps the
/// visibility filter as a second line of defense.
fn codex_entries_from_models<'a>(
    models: impl IntoIterator<Item = &'a CodexCatalogModel>,
) -> IndexMap<String, ModelEntry> {
    let base_url = crate::codex_auth::inference_base_url();
    models
        .into_iter()
        .filter(|model| model.is_visible())
        .map(|model| (model.slug.clone(), codex_model_entry(model, &base_url)))
        .collect()
}

/// The single `CodexCatalogModel` → `ModelEntry` mapping.
///
/// Capabilities are intentionally conservative: no compaction or
/// backend-search declarations until those are implemented for the Codex
/// transport, and `supported_in_api: false` because this transport is
/// OAuth-only (visibility is provider-aware, see `ModelEntry::visible_for`).
/// `tool_mode` is the exception: Code Mode is implemented for the Codex
/// transport, so the catalog's declaration is honored (unknown values warn
/// and fail closed to Classic).
fn codex_model_entry(model: &CodexCatalogModel, base_url: &str) -> ModelEntry {
    let mut info = config::ModelInfo::fallback(&model.slug);
    info.id = Some(model.slug.clone());
    info.model_family = Some("codex".to_owned());
    info.base_url = base_url.to_owned();
    info.name = Some(model.display_name.clone());
    info.description = model.description.clone();
    info.api_backend = ApiBackend::Responses;
    info.agent_type = CODEX_AGENT_TYPE.to_owned();
    info.supported_in_api = false;
    if let Some(context_window) = model.context_window.and_then(NonZeroU64::new) {
        info.context_window = context_window;
    }
    info.reasoning_efforts = codex_reasoning_efforts(model);
    info.tool_mode = codex_tool_mode(model);
    ModelEntry {
        info,
        api_key: None,
        env_key: None,
        auth_provider: None,
        api_base_url: None,
    }
}

/// Parse the live catalog's `tool_mode` string into the local capability.
/// Known wire values map 1:1; anything else warns once per model and fails
/// closed to `None` (⇒ Classic), so an unrecognized future mode can never
/// grant Code Mode by accident.
fn codex_tool_mode(model: &CodexCatalogModel) -> Option<config::ToolMode> {
    match model.tool_mode.as_deref() {
        None => None,
        Some("classic") => Some(config::ToolMode::Classic),
        Some("code_mode") => Some(config::ToolMode::CodeMode),
        Some("code_mode_only") => Some(config::ToolMode::CodeModeOnly),
        Some(other) => {
            tracing::warn!(
                model = %model.slug,
                tool_mode = %other,
                "Codex catalog declares a tool_mode this build does not know; \
                 failing closed to classic"
            );
            None
        }
    }
}

/// Map the catalog's reasoning levels onto the local effort menu. Levels this
/// build has no wire value for are skipped with one warning per model.
fn codex_reasoning_efforts(model: &CodexCatalogModel) -> Vec<ReasoningEffortOption> {
    let default_level = model.default_reasoning_level.as_deref().map(str::trim);
    let mut unknown: Vec<String> = Vec::new();
    let mut options = Vec::with_capacity(model.supported_reasoning_levels.len());
    for level in &model.supported_reasoning_levels {
        let effort = level.effort.trim();
        let Ok(value) = effort.parse::<ReasoningEffort>() else {
            unknown.push(effort.to_owned());
            continue;
        };
        let id = value.as_str().to_owned();
        let label = humanize_effort_label(&id);
        options.push(ReasoningEffortOption {
            id,
            value,
            label,
            description: Some(level.description.trim().to_owned())
                .filter(|description| !description.is_empty()),
            default: default_level.is_some_and(|d| d.eq_ignore_ascii_case(effort)),
        });
    }
    if !unknown.is_empty() {
        tracing::warn!(
            model = %model.slug,
            levels = ?unknown,
            "Codex catalog offers reasoning levels this build does not support; skipping them"
        );
    }
    options
}

/// `"xhigh"` → `"Xhigh"`; effort ids are canonical ASCII.
fn humanize_effort_label(id: &str) -> String {
    let mut label = id.to_owned();
    label[..1].make_ascii_uppercase();
    label
}

/// Fold live Codex entries into an already-resolved base catalog.
///
/// A Codex entry may replace only another Codex entry (the bundled offline
/// fallback is not user configuration); a key held by an xAI entry is left
/// untouched and the Codex entry is dropped with a warning. `[model.*]`
/// overrides are layered on afterwards by `resolve_model_list`, so explicit
/// user configuration always wins over anything merged here.
pub(crate) fn merge_codex_catalog_entries(
    resolved: &mut IndexMap<String, ModelEntry>,
    codex: &IndexMap<String, ModelEntry>,
) {
    for (key, entry) in codex {
        match resolved.get(key) {
            Some(existing) if existing.info.provider() != ModelProvider::Codex => {
                tracing::warn!(
                    model_key = %key,
                    "live Codex catalog entry collides with an existing non-Codex entry; skipping it"
                );
            }
            _ => {
                resolved.insert(key.clone(), entry.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex_models::{CodexModelVisibility, CodexReasoningLevel};

    fn catalog_model(slug: &str) -> CodexCatalogModel {
        CodexCatalogModel {
            slug: slug.to_owned(),
            display_name: format!("{slug} display"),
            description: Some("Live Codex model".to_owned()),
            priority: 1,
            visibility: CodexModelVisibility::List,
            server_supported_in_api: true,
            supported_in_api: false,
            context_window: Some(353_400),
            raw_context_window: Some(372_000),
            auto_compact_token_limit: None,
            comp_hash: None,
            effective_context_window_percent: 95,
            default_reasoning_level: Some("medium".to_owned()),
            supported_reasoning_levels: vec![
                CodexReasoningLevel {
                    effort: "low".to_owned(),
                    description: "Fast".to_owned(),
                },
                CodexReasoningLevel {
                    effort: "medium".to_owned(),
                    description: String::new(),
                },
            ],
            supports_search_tool: true,
            tool_mode: None,
        }
    }

    /// Live-catalog Code Mode chain: the wire `tool_mode` string reaches
    /// `ModelInfo.tool_mode` at the single mapping site, resolves through the
    /// catalog capability lookup, and selects the native custom-grammar exec
    /// transport for the Codex provider profile (the plan → turn-spec /
    /// hosted-tool link is covered by the sampler-turn wire-shape tests).
    #[test]
    fn live_catalog_tool_mode_reaches_the_effective_code_mode_chain() {
        for (wire, expected) in [
            (None, None),
            (Some("classic"), Some(config::ToolMode::Classic)),
            (Some("code_mode"), Some(config::ToolMode::CodeMode)),
            (Some("code_mode_only"), Some(config::ToolMode::CodeModeOnly)),
            // Unknown value warns and fails closed.
            (Some("hologram_mode"), None),
        ] {
            let mut model = catalog_model("gpt-6-live");
            model.tool_mode = wire.map(str::to_owned);
            let entries = codex_entries_from_models([&model]);
            let info = entries["gpt-6-live"].info();
            assert_eq!(info.tool_mode, expected, "wire tool_mode {wire:?}");

            let effective = config::model_tool_mode(&entries, "gpt-6-live");
            assert_eq!(
                effective,
                expected.unwrap_or(config::ToolMode::Classic),
                "effective mode for wire {wire:?}"
            );
            if effective.is_code_mode() {
                let transport =
                    xai_grok_sampling_types::ProviderProfile::for_provider(info.provider())
                        .code_mode_transport;
                assert_eq!(
                    transport,
                    xai_grok_sampling_types::CodeModeTransport::NativeCustomGrammar,
                    "Codex code mode must ride the native custom-grammar transport"
                );
            }
        }
    }

    #[test]
    fn mapping_produces_conservative_codex_entry() {
        let entries = codex_entries_from_models([&catalog_model("gpt-6-live")]);
        let entry = &entries["gpt-6-live"];
        let info = entry.info();

        assert_eq!(info.provider(), ModelProvider::Codex);
        assert_eq!(info.model_family.as_deref(), Some("codex"));
        assert_eq!(info.model, "gpt-6-live");
        assert_eq!(info.id.as_deref(), Some("gpt-6-live"));
        assert_eq!(info.name.as_deref(), Some("gpt-6-live display"));
        assert_eq!(info.description.as_deref(), Some("Live Codex model"));
        assert_eq!(info.api_backend, ApiBackend::Responses);
        assert_eq!(info.agent_type, CODEX_AGENT_TYPE);
        assert_eq!(info.base_url, crate::codex_auth::inference_base_url());
        assert_eq!(info.context_window.get(), 353_400);
        assert!(!info.supported_in_api, "transport is OAuth-only");
        assert!(!info.hidden);

        // Capabilities the Codex transport does not implement yet must not
        // be declared, even when the wire model advertises search support.
        assert!(!info.supports_backend_search);
        assert!(info.compactions_remaining.is_none());
        assert!(info.compaction_at_tokens.is_none());
        assert!(info.stream_tool_calls.is_none());

        // No static credential: the session layer attaches the OAuth bearer.
        assert!(entry.api_key.is_none());
        assert!(entry.env_key.is_none());
        assert!(entry.auth_provider.is_none());

        let efforts = &info.reasoning_efforts;
        assert_eq!(efforts.len(), 2);
        assert_eq!(efforts[0].value, ReasoningEffort::Low);
        assert_eq!(efforts[0].label, "Low");
        assert_eq!(efforts[0].description.as_deref(), Some("Fast"));
        assert!(!efforts[0].default);
        assert_eq!(efforts[1].value, ReasoningEffort::Medium);
        assert!(
            efforts[1].default,
            "default_reasoning_level marks the option"
        );
        assert_eq!(efforts[1].description, None);
    }

    #[test]
    fn unknown_reasoning_levels_are_skipped() {
        let mut model = catalog_model("gpt-6-live");
        model.supported_reasoning_levels.push(CodexReasoningLevel {
            effort: "ultra".to_owned(),
            description: "Not a local wire value".to_owned(),
        });
        let entries = codex_entries_from_models([&model]);
        let efforts = &entries["gpt-6-live"].info().reasoning_efforts;
        assert_eq!(efforts.len(), 2, "unknown level must be dropped");
        assert!(efforts.iter().all(|option| option.id != "ultra"));
    }

    #[test]
    fn non_list_visibility_is_excluded() {
        let mut hidden = catalog_model("hidden-model");
        hidden.visibility = CodexModelVisibility::Hide;
        let mut unspecified = catalog_model("unspecified-model");
        unspecified.visibility = CodexModelVisibility::None;
        let listed = catalog_model("listed-model");

        let entries = codex_entries_from_models([&hidden, &unspecified, &listed]);
        assert_eq!(entries.keys().collect::<Vec<_>>(), ["listed-model"]);
    }

    #[test]
    fn merge_never_replaces_a_non_codex_entry() {
        let cfg = config::Config::default();
        let mut resolved = resolve_model_catalog(&cfg, None);
        let xai_before = serde_json::to_string(&resolved["grok-4.6"]).unwrap();

        let mut colliding = catalog_model("grok-4.6");
        colliding.display_name = "Impostor".to_owned();
        let codex = codex_entries_from_models([&colliding, &catalog_model("gpt-6-live")]);
        merge_codex_catalog_entries(&mut resolved, &codex);

        assert_eq!(
            serde_json::to_string(&resolved["grok-4.6"]).unwrap(),
            xai_before,
            "an xAI entry must never be overwritten by the Codex catalog"
        );
        assert!(resolved.contains_key("gpt-6-live"));
    }

    #[test]
    fn live_entry_overrides_the_bundled_codex_fallback() {
        let cfg = config::Config::default();
        let fallback_key = "gpt-5.6-sol";
        {
            let resolved = resolve_model_catalog(&cfg, None);
            assert_eq!(
                resolved[fallback_key].info().provider(),
                ModelProvider::Codex,
                "precondition: bundled fallback is a Codex entry"
            );
        }

        let mut live = catalog_model(fallback_key);
        live.context_window = Some(400_000);
        let codex = codex_entries_from_models([&live]);
        let resolved = resolve_model_catalog_with_codex(&cfg, None, Some(&codex));
        assert_eq!(
            resolved[fallback_key].info().context_window.get(),
            400_000,
            "live catalog capability fields must replace the offline fallback"
        );
    }

    #[test]
    fn user_model_override_still_applies_on_top_of_live_entry() {
        let raw: toml::Value = toml::from_str(
            r#"
                [model.gpt-6-live]
                api_key = "sk-explicit"
            "#,
        )
        .unwrap();
        let cfg = config::Config::new_from_toml_cfg(&raw).unwrap();
        let codex = codex_entries_from_models([&catalog_model("gpt-6-live")]);
        let resolved = resolve_model_catalog_with_codex(&cfg, None, Some(&codex));

        let entry = &resolved["gpt-6-live"];
        assert_eq!(
            entry.api_key.as_deref(),
            Some("sk-explicit"),
            "explicit user credential must survive the live merge"
        );
        assert_eq!(
            entry.info().name.as_deref(),
            Some("gpt-6-live display"),
            "unset override fields inherit the live metadata"
        );
        assert!(
            entry.visible_for(config::AuthVisibility::new(false, false)),
            "a Codex entry with its own credential needs no OAuth login"
        );
    }
}
