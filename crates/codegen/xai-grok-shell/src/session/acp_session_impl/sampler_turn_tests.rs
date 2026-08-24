use xai_grok_sampling_types::{SearchDateBound, ToolOverrides, WebSearchOptions, XSearchOptions};

use super::{
    CLASSIFIER_REQUEST_TOKEN_RESERVE, classifier_request_fits_context,
    codex_visible_tool_definitions, resolve_configured_cutoff,
};

fn tool_definition(
    name: &str,
    parameters: serde_json::Value,
) -> crate::sampling::types::ToolDefinition {
    crate::sampling::types::ToolDefinition::function(name, Some(name), parameters)
}

#[test]
fn codex_tool_surface_hides_grok_dispatchers_when_mcp_is_empty() {
    let defs = codex_visible_tool_definitions(vec![
        tool_definition("read_file", serde_json::json!({"type": "object"})),
        tool_definition(xai_grok_tools::SEARCH_TOOL_NAME, serde_json::json!({})),
        tool_definition(xai_grok_tools::USE_TOOL_NAME, serde_json::json!({})),
    ]);
    let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
    assert_eq!(names, vec!["read_file"]);
}

#[test]
fn codex_tool_surface_exposes_real_mcp_name_and_input_schema() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {"issue_id": {"type": "string"}},
        "required": ["issue_id"],
        "additionalProperties": false
    });
    let defs = codex_visible_tool_definitions(vec![
        tool_definition(xai_grok_tools::SEARCH_TOOL_NAME, serde_json::json!({})),
        tool_definition(xai_grok_tools::USE_TOOL_NAME, serde_json::json!({})),
        tool_definition("linear__get_issue", schema.clone()),
    ]);
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].function.name, "linear__get_issue");
    assert_eq!(defs[0].function.parameters, schema);
}

fn x_cut(to: &str) -> XSearchOptions {
    XSearchOptions {
        date_bound: Some(SearchDateBound::new(None, Some(to.into())).unwrap()),
    }
}

#[test]
fn classifier_request_bound_enforces_its_reserve_with_saturating_arithmetic() {
    let window = 12_000 + CLASSIFIER_REQUEST_TOKEN_RESERVE;
    for (input, context_window, expected) in [
        (12_000, window, true),
        (12_001, window, false),
        (u64::MAX, u64::MAX, false),
    ] {
        assert_eq!(
            classifier_request_fits_context(input, context_window),
            expected
        );
    }
}

#[test]
fn seed_cutoff_is_inherited_without_a_per_turn_update() {
    let seed = ToolOverrides {
        x_search: Some(x_cut("2020-01-01")),
        web_search: None,
    };
    assert_eq!(resolve_configured_cutoff(Some(seed.clone()), None), seed);
}

#[test]
fn non_empty_base_cutoff_wins_per_tool_and_an_empty_one_reverts_to_the_seed() {
    let seed = ToolOverrides {
        x_search: Some(x_cut("2020-01-01")),
        web_search: Some(WebSearchOptions {
            allowed_domains: Some(vec!["x.com".into()]),
            excluded_domains: None,
        }),
    };
    let base = ToolOverrides {
        x_search: Some(x_cut("2019-06-01")),
        web_search: Some(WebSearchOptions {
            allowed_domains: Some(vec![]),
            excluded_domains: None,
        }),
    };
    let got = resolve_configured_cutoff(Some(seed.clone()), Some(&base));
    assert_eq!(got.x_search, Some(x_cut("2019-06-01")));
    assert_eq!(got.web_search, seed.web_search);
}

#[test]
fn inherited_cutoff_agrees_with_the_wire_echo_so_the_two_implementations_cannot_drift() {
    use xai_grok_sampling_types::{HostedTool, apply_tool_overrides};
    let web = WebSearchOptions {
        allowed_domains: Some(vec!["x.com".into()]),
        excluded_domains: None,
    };
    let cases = [
        (
            Some(ToolOverrides {
                x_search: Some(x_cut("2020-01-01")),
                web_search: None,
            }),
            None,
        ),
        (
            Some(ToolOverrides {
                x_search: Some(x_cut("2020-01-01")),
                web_search: Some(web.clone()),
            }),
            Some(ToolOverrides {
                x_search: Some(x_cut("2019-06-01")),
                web_search: None,
            }),
        ),
        (
            None,
            Some(ToolOverrides {
                x_search: Some(x_cut("2018-01-01")),
                web_search: Some(web.clone()),
            }),
        ),
    ];
    for (seed, base) in cases {
        let mut tools = vec![
            HostedTool::WebSearch { options: None },
            HostedTool::XSearch { options: None },
        ];
        apply_tool_overrides(&mut tools, seed.as_ref());
        let wire_echo = apply_tool_overrides(&mut tools, base.as_ref());
        let inherited = resolve_configured_cutoff(seed.clone(), base.as_ref());
        assert_eq!(wire_echo, inherited, "seed={seed:?} base={base:?}");
    }
}

// ---------------------------------------------------------------------------
// Code Mode wire shapes
// ---------------------------------------------------------------------------

fn code_mode_plan(
    transport: xai_grok_sampling_types::CodeModeTransport,
) -> crate::tools::code_mode::CodeModeTurnPlan {
    crate::tools::code_mode::CodeModeTurnPlan {
        model_id: "test-model".to_string(),
        mode: crate::agent::config::ToolMode::CodeMode,
        transport: Some(transport),
        exec_description: "EXEC DESC".to_string(),
        init_error: None,
    }
}

/// Function-envelope transport (xAI): both `exec` and `wait` are plain
/// function tools; `exec` takes `{"source": string}`.
#[test]
fn function_envelope_code_mode_lists_exec_and_wait_function_tools() {
    let specs = super::code_mode_turn_specs(&code_mode_plan(
        xai_grok_sampling_types::CodeModeTransport::FunctionEnvelope,
    ));
    let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["exec", "wait"]);
    let exec = &specs[0];
    assert_eq!(exec.description.as_deref(), Some("EXEC DESC"));
    assert_eq!(
        exec.parameters["required"],
        serde_json::json!(["source"]),
        "{}",
        exec.parameters
    );
    assert_eq!(exec.parameters["properties"]["source"]["type"], "string");
    let wait = &specs[1];
    assert_eq!(wait.parameters["required"], serde_json::json!(["cell_id"]));
}

/// Native custom-grammar transport (Codex): only `wait` stays a function
/// tool; `exec` rides `hosted_tools` as a `ClientCustom` custom tool whose
/// serialized entry is `{"type":"custom","name":"exec",...}`.
#[test]
fn native_custom_grammar_code_mode_moves_exec_to_a_custom_tool_entry() {
    let specs = super::code_mode_turn_specs(&code_mode_plan(
        xai_grok_sampling_types::CodeModeTransport::NativeCustomGrammar,
    ));
    let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["wait"]);

    let hosted = vec![xai_grok_sampling_types::HostedTool::ClientCustom(
        xai_grok_sampling_types::CustomToolSpec {
            name: "exec".to_string(),
            description: Some("EXEC DESC".to_string()),
            format: Default::default(),
        },
    )];
    let entries = xai_grok_sampling_types::conversation::extra_tool_entries(
        &hosted,
        xai_grok_sampling_types::ProviderProfile::CODEX,
    );
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["type"], "custom");
    assert_eq!(entries[0]["name"], "exec");
    assert_eq!(entries[0]["description"], "EXEC DESC");
}
