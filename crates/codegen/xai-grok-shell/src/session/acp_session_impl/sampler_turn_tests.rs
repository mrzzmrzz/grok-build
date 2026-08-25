use xai_grok_sampling_types::{SearchDateBound, ToolOverrides, WebSearchOptions, XSearchOptions};

use super::{
    CLASSIFIER_REQUEST_TOKEN_RESERVE, McpInitStrategy, classifier_request_fits_context,
    mcp_wait_required, resolve_configured_cutoff, strip_retired_dispatcher_tools,
};

fn tool_definition(
    name: &str,
    parameters: serde_json::Value,
) -> crate::sampling::types::ToolDefinition {
    crate::sampling::types::ToolDefinition::function(name, Some(name), parameters)
}

#[test]
fn tool_surface_hides_retired_dispatchers_when_mcp_is_empty() {
    let defs = strip_retired_dispatcher_tools(vec![
        tool_definition("read_file", serde_json::json!({"type": "object"})),
        tool_definition(xai_grok_tools::SEARCH_TOOL_NAME, serde_json::json!({})),
        tool_definition(xai_grok_tools::USE_TOOL_NAME, serde_json::json!({})),
    ]);
    let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
    assert_eq!(names, vec!["read_file"]);
}

#[test]
fn tool_surface_exposes_real_mcp_name_and_input_schema() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {"issue_id": {"type": "string"}},
        "required": ["issue_id"],
        "additionalProperties": false
    });
    let defs = strip_retired_dispatcher_tools(vec![
        tool_definition(xai_grok_tools::SEARCH_TOOL_NAME, serde_json::json!({})),
        tool_definition(xai_grok_tools::USE_TOOL_NAME, serde_json::json!({})),
        tool_definition("linear__get_issue", schema.clone()),
    ]);
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].function.name, "linear__get_issue");
    assert_eq!(defs[0].function.parameters, schema);
}

/// A Codex session must have its MCP definitions in the turn-1 tools array:
/// gaining them on turn 2 rewrites the cached prompt prefix. xAI sessions
/// keep Progressive's non-blocking first turn.
#[test]
fn progressive_blocks_on_mcp_init_only_for_codex() {
    use xai_grok_sampling_types::ModelProvider;
    for provider in [ModelProvider::Xai, ModelProvider::Codex] {
        assert!(
            mcp_wait_required(McpInitStrategy::Blocking, provider),
            "Blocking always waits ({provider:?})"
        );
    }
    assert!(mcp_wait_required(
        McpInitStrategy::Progressive,
        ModelProvider::Codex
    ));
    assert!(!mcp_wait_required(
        McpInitStrategy::Progressive,
        ModelProvider::Xai
    ));
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
#[cfg(test)]
mod subagent_sampling_gate_tests {
    use super::super::super::support::create_test_actor;
    use super::super::acquire_subagent_sampling_permit;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::Semaphore;

    #[derive(Default)]
    struct ConcurrencyProbe {
        in_flight: AtomicUsize,
        max_in_flight: AtomicUsize,
    }

    impl ConcurrencyProbe {
        fn enter(&self) {
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(now, Ordering::SeqCst);
        }
        fn leave(&self) {
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn subagent_submits_never_exceed_cap_and_excess_queues() {
        const CAP: usize = 3;
        const TURNS: usize = 12;
        let semaphore = Arc::new(Semaphore::new(CAP));
        let probe = Arc::new(ConcurrencyProbe::default());
        let ran = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..TURNS {
            let gate = Some(semaphore.clone());
            let probe = probe.clone();
            let ran = ran.clone();
            handles.push(tokio::spawn(async move {
                let permit = acquire_subagent_sampling_permit(&gate).await;
                assert!(permit.is_some(), "a subagent turn must receive a permit");
                probe.enter();
                tokio::time::sleep(Duration::from_millis(20)).await;
                probe.leave();
                ran.fetch_add(1, Ordering::SeqCst);
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        assert_eq!(
            ran.load(Ordering::SeqCst),
            TURNS,
            "every queued turn ran (queued, not errored)"
        );
        assert!(
            probe.max_in_flight.load(Ordering::SeqCst) <= CAP,
            "in-flight subagent submits exceeded the cap: {} > {CAP}",
            probe.max_in_flight.load(Ordering::SeqCst),
        );
    }

    #[tokio::test]
    async fn cancelled_waiter_releases_without_deadlock() {
        let semaphore = Arc::new(Semaphore::new(1));
        let gate = Some(semaphore.clone());
        let held = acquire_subagent_sampling_permit(&gate).await;
        assert!(held.is_some());

        let waiter = tokio::spawn({
            let gate = gate.clone();
            async move {
                let _permit = acquire_subagent_sampling_permit(&gate).await;
                std::future::pending::<()>().await;
            }
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            semaphore.available_permits(),
            0,
            "slot stays held while the second turn queues"
        );
        waiter.abort();
        let _ = waiter.await;

        drop(held);
        let next = tokio::time::timeout(
            Duration::from_millis(200),
            acquire_subagent_sampling_permit(&gate),
        )
        .await
        .expect("a permit must be free once the held one is released");
        assert!(next.is_some(), "the cancelled waiter did not leak the slot");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn submit_holds_permit_for_subagent_not_main() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let saturated = Arc::new(Semaphore::new(1));
                let _held = saturated.clone().acquire_owned().await.unwrap();

                let (gw_tx, _gw_rx) = tokio::sync::mpsc::unbounded_channel();
                let (p_tx, _p_rx) = tokio::sync::mpsc::unbounded_channel();
                let mut subagent = create_test_actor(0, 200_000, 80, gw_tx, p_tx).await;
                subagent.sampling_gate = Some(saturated.clone());
                let subagent = Arc::new(subagent);

                let queued = tokio::time::timeout(
                    Duration::from_millis(150),
                    subagent.submit_turn_request(Default::default()),
                )
                .await;
                assert!(
                    queued.is_err(),
                    "a subagent submit must queue behind the drained gate, never reaching the sampler"
                );

                let (gw_tx, _gw_rx) = tokio::sync::mpsc::unbounded_channel();
                let (p_tx, _p_rx) = tokio::sync::mpsc::unbounded_channel();
                let main = create_test_actor(0, 200_000, 80, gw_tx, p_tx).await;
                assert!(main.sampling_gate.is_none());
                let main = Arc::new(main);

                let ran = tokio::time::timeout(
                    Duration::from_millis(150),
                    main.submit_turn_request(Default::default()),
                )
                .await;
                assert!(
                    ran.is_ok(),
                    "the main session must reach the sampler even while the gate is drained"
                );
            })
            .await;
    }
}
