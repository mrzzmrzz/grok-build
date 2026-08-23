// Derived from OpenAI Codex code-mode at commit
// 2be648ba4a6c159a3d80b1c07e7323cbd5efef8f (Apache-2.0).

use pretty_assertions::assert_eq;
use xai_grok_code_mode::ExecuteRequest;
use xai_grok_code_mode::InProcessCodeModeSession;
use xai_grok_code_mode::RuntimeResponse;
use xai_grok_code_mode::V8JitMode;
use xai_grok_code_mode::initialize_v8;

#[tokio::test]
async fn code_mode_runs_with_jit_disabled() {
    initialize_v8(V8JitMode::Disabled).expect("initialize V8 without JIT");

    let service = InProcessCodeModeSession::new();
    let started = service
        .execute(ExecuteRequest {
            tool_call_id: "call_1".to_string(),
            enabled_tools: Vec::new(),
            source: "21 * 2;".to_string(),
            yield_time_ms: None,
            max_output_tokens: None,
        })
        .await
        .expect("start code-mode cell");
    let cell_id = started.cell_id.clone();
    let response = started
        .initial_response()
        .await
        .expect("execute code-mode cell");

    assert_eq!(
        response,
        RuntimeResponse::Result {
            cell_id,
            content_items: Vec::new(),
            error_text: None,
        }
    );
    assert_eq!(
        initialize_v8(V8JitMode::Enabled),
        Err("V8 was already initialized with JIT disabled".to_string())
    );
}
