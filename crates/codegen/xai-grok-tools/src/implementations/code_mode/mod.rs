//! Code Mode `exec` / `wait` tools.
//!
//! These tools are **not** part of any static toolset configuration. The
//! shell registers them dynamically on the finalized toolset
//! ([`crate::registry::FinalizedToolset::register_code_mode_tools`]) only
//! when the current model declares a Code Mode `tool_mode`, and injects a
//! [`CodeModeHandle`] resource pointing at the session's V8-backed
//! `CodeModeSession`. Without that resource every call fails closed.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use xai_grok_code_mode_protocol::{
    CellId, CodeModeSession, ExecuteRequest, FunctionCallOutputContentItem, RuntimeResponse,
    ToolDefinition as CodeModeToolDefinition, WaitOutcome, parse_exec_source,
};

use crate::types::tool::{ToolKind, ToolNamespace};
use crate::types::tool_metadata::ToolMetadata;

pub use xai_grok_code_mode_protocol::{PUBLIC_TOOL_NAME as EXEC_TOOL_NAME, WAIT_TOOL_NAME};

/// Session-injected handle the `exec`/`wait` tools execute against.
///
/// Registered by the shell as a toolset resource when Code Mode activates.
/// `enabled_tools` is the current nested-tool projection (refreshed by the
/// shell before each turn); `live_cells` tracks cells that yielded and may
/// still be running, so the session can explicitly terminate them on
/// cancel/close.
#[derive(Clone)]
pub struct CodeModeHandle {
    pub session: Arc<dyn CodeModeSession>,
    pub enabled_tools: Arc<parking_lot::RwLock<Vec<CodeModeToolDefinition>>>,
    pub live_cells: Arc<parking_lot::Mutex<std::collections::HashSet<String>>>,
}

impl std::fmt::Debug for CodeModeHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeModeHandle")
            .field("enabled_tools", &self.enabled_tools.read().len())
            .field("live_cells", &self.live_cells.lock().len())
            .finish()
    }
}

impl CodeModeHandle {
    fn track_cell(&self, cell_id: &CellId) {
        self.live_cells.lock().insert(cell_id.as_str().to_string());
    }

    fn untrack_cell(&self, cell_id: &CellId) {
        self.live_cells.lock().remove(cell_id.as_str());
    }
}

async fn code_mode_handle(
    ctx: &xai_tool_runtime::ToolCallContext,
) -> Result<CodeModeHandle, xai_tool_runtime::ToolError> {
    let resources = crate::types::tool_metadata::shared_resources(ctx)?;
    let handle = {
        let res = resources.lock().await;
        res.get::<CodeModeHandle>().cloned()
    };
    handle.ok_or_else(|| {
        xai_tool_runtime::ToolError::custom(
            "code_mode_unavailable",
            "Code Mode is not active for this session (no code-mode runtime is mounted). \
             Use the regular tools instead.",
        )
    })
}

// ---------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------

/// Input for `exec`. Over the function-envelope transport the model sends
/// `{"source": "...js..."}`; over the native custom-grammar transport the
/// shell wraps the raw text into the same shape before dispatch.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ExecToolInput {
    /// Raw JavaScript source text (optionally starting with a
    /// `// @exec: {...}` pragma line).
    #[serde(alias = "raw")]
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WaitToolInput {
    /// Identifies the running `exec` cell to resume.
    pub cell_id: String,
    /// How long to wait for more output before yielding again (ms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yield_time_ms: Option<u64>,
    /// When true, stop the running cell instead of waiting for output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminate: Option<bool>,
    /// Token budget for the new output this wait call returns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// One ordered output part from a code-mode cell.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CodeModePart {
    Text {
        text: String,
    },
    Image {
        image_url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CodeModeCellStatus {
    /// The cell is still running; resume it with `wait`.
    Yielded,
    /// The cell finished (successfully or with `error_text`).
    Completed,
    /// The cell was explicitly terminated.
    Terminated,
}

/// Result of one `exec`/`wait` call, preserving the cell's ordered output
/// parts so text/image interleaving survives to the model.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq)]
pub struct CodeModeCallOutput {
    pub cell_id: String,
    pub status: CodeModeCellStatus,
    pub parts: Vec<CodeModePart>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_text: Option<String>,
}

impl CodeModeCallOutput {
    pub fn is_error(&self) -> bool {
        self.error_text.is_some()
    }

    /// Prompt-facing rendering: ordered text parts, image placeholders, and
    /// yield/termination/error trailers.
    pub fn to_prompt_format(&self) -> String {
        let mut sections: Vec<String> = Vec::new();
        for part in &self.parts {
            match part {
                CodeModePart::Text { text } => sections.push(text.clone()),
                CodeModePart::Image { .. } => sections
                    .push("[Image output is included inline in this tool result]".to_string()),
            }
        }
        let trailer = self.trailer_text();
        if !trailer.is_empty() {
            sections.push(trailer);
        }
        if sections.is_empty() {
            sections.push("(no output)".to_string());
        }
        sections.join("\n")
    }

    /// The status/error trailer rendered after the ordered parts (may be
    /// empty). Shared with the shell's ordered-parts tool-result path so both
    /// renderings can never disagree.
    pub fn trailer_text(&self) -> String {
        let mut sections: Vec<String> = Vec::new();
        match self.status {
            CodeModeCellStatus::Yielded => sections.push(format!(
                "Script running with cell ID {}. Call `wait` with this cell_id to get more \
                 output, or `wait` with terminate: true to stop it.",
                self.cell_id
            )),
            CodeModeCellStatus::Terminated => {
                sections.push(format!("Cell {} was terminated.", self.cell_id));
            }
            CodeModeCellStatus::Completed => {}
        }
        if let Some(error_text) = &self.error_text {
            sections.push(format!("Error: {error_text}"));
        }
        sections.join("\n")
    }
}

impl xai_tool_runtime::ToolOutput for CodeModeCallOutput {
    fn model_output(&self) -> Vec<xai_tool_runtime::ContentBlock> {
        let mut blocks = Vec::new();
        for part in &self.parts {
            match part {
                CodeModePart::Text { text } => {
                    blocks.push(xai_tool_runtime::ContentBlock::Text { text: text.clone() });
                }
                CodeModePart::Image { image_url, .. } => {
                    if let Some((mime_type, data)) = parse_data_url(image_url) {
                        blocks.push(xai_tool_runtime::ContentBlock::Image {
                            mime_type,
                            data,
                            media_id: None,
                            filename: None,
                            path: None,
                            metadata: Default::default(),
                        });
                    } else {
                        blocks.push(xai_tool_runtime::ContentBlock::Text {
                            text: format!("[unsupported image url: {image_url}]"),
                        });
                    }
                }
            }
        }
        if blocks.is_empty() {
            blocks.push(xai_tool_runtime::ContentBlock::Text {
                text: self.to_prompt_format(),
            });
        }
        blocks
    }
}

/// Split a `data:<mime>;base64,<data>` URL. Returns `None` for anything else.
pub fn parse_data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (mime, data) = rest.split_once(";base64,")?;
    if mime.is_empty() || data.is_empty() {
        return None;
    }
    Some((mime.to_string(), data.to_string()))
}

fn output_parts(content_items: Vec<FunctionCallOutputContentItem>) -> Vec<CodeModePart> {
    content_items
        .into_iter()
        .map(|item| match item {
            FunctionCallOutputContentItem::InputText { text } => CodeModePart::Text { text },
            FunctionCallOutputContentItem::InputImage { image_url, detail } => {
                CodeModePart::Image {
                    image_url,
                    detail: detail.map(|d| {
                        serde_json::to_value(d)
                            .ok()
                            .and_then(|v| v.as_str().map(str::to_owned))
                            .unwrap_or_else(|| "auto".to_string())
                    }),
                }
            }
        })
        .collect()
}

/// Enforce the advertised per-call output token budget (`exec`'s
/// `max_output_tokens` / `wait`'s `max_tokens`) over the ordered parts.
/// Text is estimated at [`xai_token_estimation::BYTES_PER_TOKEN`] bytes per
/// token and clipped at a char boundary; images cost
/// [`xai_token_estimation::IMAGE_TOKEN_ESTIMATE`] each. Returns whether
/// anything was dropped.
fn truncate_parts_to_budget(
    parts: Vec<CodeModePart>,
    max_tokens: usize,
) -> (Vec<CodeModePart>, bool) {
    let budget = max_tokens as u64;
    let mut used = 0u64;
    let mut out = Vec::new();
    let mut parts_iter = parts.into_iter();
    for part in parts_iter.by_ref() {
        match part {
            CodeModePart::Image { image_url, detail } => {
                let cost = xai_token_estimation::IMAGE_TOKEN_ESTIMATE;
                if used.saturating_add(cost) > budget {
                    return (out, true);
                }
                used += cost;
                out.push(CodeModePart::Image { image_url, detail });
            }
            CodeModePart::Text { text } => {
                let cost = xai_token_estimation::estimate_tokens(&text);
                if used.saturating_add(cost) <= budget {
                    used += cost;
                    out.push(CodeModePart::Text { text });
                    continue;
                }
                let keep_chars =
                    xai_token_estimation::estimate_chars(budget.saturating_sub(used)) as usize;
                let clipped: String = text.chars().take(keep_chars).collect();
                if !clipped.is_empty() {
                    out.push(CodeModePart::Text { text: clipped });
                }
                return (out, true);
            }
        }
    }
    (out, false)
}

fn budgeted_parts(
    content_items: Vec<FunctionCallOutputContentItem>,
    max_tokens: usize,
) -> Vec<CodeModePart> {
    let (mut parts, truncated) = truncate_parts_to_budget(output_parts(content_items), max_tokens);
    if truncated {
        parts.push(CodeModePart::Text {
            text: format!(
                "[output truncated at the requested budget of {max_tokens} tokens]"
            ),
        });
    }
    parts
}

fn call_output(
    handle: &CodeModeHandle,
    response: RuntimeResponse,
    max_tokens: usize,
) -> CodeModeCallOutput {
    match response {
        RuntimeResponse::Yielded {
            cell_id,
            content_items,
        } => {
            handle.track_cell(&cell_id);
            CodeModeCallOutput {
                cell_id: cell_id.as_str().to_string(),
                status: CodeModeCellStatus::Yielded,
                parts: budgeted_parts(content_items, max_tokens),
                error_text: None,
            }
        }
        RuntimeResponse::Terminated {
            cell_id,
            content_items,
        } => {
            handle.untrack_cell(&cell_id);
            CodeModeCallOutput {
                cell_id: cell_id.as_str().to_string(),
                status: CodeModeCellStatus::Terminated,
                parts: budgeted_parts(content_items, max_tokens),
                error_text: None,
            }
        }
        RuntimeResponse::Result {
            cell_id,
            content_items,
            error_text,
        } => {
            handle.untrack_cell(&cell_id);
            CodeModeCallOutput {
                cell_id: cell_id.as_str().to_string(),
                status: CodeModeCellStatus::Completed,
                parts: budgeted_parts(content_items, max_tokens),
                error_text,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct ExecTool;

impl ToolMetadata for ExecTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Other
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        // The model-facing description is generated per turn by the shell via
        // `xai_grok_code_mode_protocol::build_exec_tool_description`, which
        // folds in the current nested-tool projection; this static text is
        // only the registry fallback.
        "Run JavaScript code to orchestrate/compose tool calls in a sandboxed V8 isolate."
    }

    fn is_read_only(&self) -> bool {
        // The JS sandbox has no direct I/O; every side effect goes through a
        // nested tool call, which re-enters the permission and plan-mode
        // gates individually.
        true
    }
}

impl xai_tool_runtime::Tool for ExecTool {
    type Args = ExecToolInput;
    type Output = CodeModeCallOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new(EXEC_TOOL_NAME).expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            EXEC_TOOL_NAME,
            self.sanitized_description_template(),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: true,
            ..Default::default()
        }
    }

    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: ExecToolInput,
    ) -> Result<CodeModeCallOutput, xai_tool_runtime::ToolError> {
        let handle = code_mode_handle(&ctx).await?;
        let parsed = parse_exec_source(&input.source)
            .map_err(xai_tool_runtime::ToolError::invalid_arguments)?;
        let max_output_tokens = parsed
            .max_output_tokens
            .unwrap_or(xai_grok_code_mode_protocol::DEFAULT_MAX_OUTPUT_TOKENS_PER_EXEC_CALL);
        let request = ExecuteRequest {
            tool_call_id: ctx.call_id.as_str().to_string(),
            enabled_tools: handle.enabled_tools.read().clone(),
            source: parsed.code,
            yield_time_ms: parsed.yield_time_ms,
            max_output_tokens: parsed.max_output_tokens,
        };
        let started = handle
            .session
            .execute(request)
            .await
            .map_err(|error| xai_tool_runtime::ToolError::custom("code_mode_exec", error))?;
        // Track before awaiting the first response: if this future is dropped
        // by a turn cancel, the session's cell-termination sweep must still
        // see the cell.
        handle.track_cell(&started.cell_id);
        let response = started
            .initial_response()
            .await
            .map_err(|error| xai_tool_runtime::ToolError::custom("code_mode_exec", error))?;
        Ok(call_output(&handle, response, max_output_tokens))
    }
}

#[derive(Debug, Default)]
pub struct WaitTool;

impl ToolMetadata for WaitTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Other
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        xai_grok_code_mode_protocol::build_wait_tool_description()
    }

    fn is_read_only(&self) -> bool {
        true
    }
}

impl xai_tool_runtime::Tool for WaitTool {
    type Args = WaitToolInput;
    type Output = CodeModeCallOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new(WAIT_TOOL_NAME).expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            WAIT_TOOL_NAME,
            self.sanitized_description_template(),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: true,
            ..Default::default()
        }
    }

    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: WaitToolInput,
    ) -> Result<CodeModeCallOutput, xai_tool_runtime::ToolError> {
        let handle = code_mode_handle(&ctx).await?;
        let cell_id = CellId::new(input.cell_id.clone());
        let outcome = if input.terminate.unwrap_or(false) {
            handle.session.terminate(cell_id).await
        } else {
            handle
                .session
                .wait(xai_grok_code_mode_protocol::WaitRequest {
                    cell_id,
                    yield_time_ms: input
                        .yield_time_ms
                        .unwrap_or(xai_grok_code_mode_protocol::DEFAULT_WAIT_YIELD_TIME_MS),
                })
                .await
        }
        .map_err(|error| xai_tool_runtime::ToolError::custom("code_mode_wait", error))?;
        let response = match outcome {
            WaitOutcome::LiveCell(response) | WaitOutcome::MissingCell(response) => response,
        };
        let max_tokens = input
            .max_tokens
            .map(|v| v as usize)
            .unwrap_or(xai_grok_code_mode_protocol::DEFAULT_MAX_OUTPUT_TOKENS_PER_EXEC_CALL);
        Ok(call_output(&handle, response, max_tokens))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn data_url_parses_mime_and_payload() {
        assert_eq!(
            parse_data_url("data:image/png;base64,QUJD"),
            Some(("image/png".to_string(), "QUJD".to_string()))
        );
        assert_eq!(parse_data_url("https://example.com/x.png"), None);
        assert_eq!(parse_data_url("data:;base64,QUJD"), None);
    }

    #[test]
    fn yielded_output_renders_cell_resume_guidance() {
        let output = CodeModeCallOutput {
            cell_id: "7".to_string(),
            status: CodeModeCellStatus::Yielded,
            parts: vec![CodeModePart::Text {
                text: "partial".to_string(),
            }],
            error_text: None,
        };
        let prompt = output.to_prompt_format();
        assert!(prompt.starts_with("partial\n"));
        assert!(prompt.contains("Script running with cell ID 7"));
        assert!(!output.is_error());
    }

    #[test]
    fn completed_output_with_error_is_an_error() {
        let output = CodeModeCallOutput {
            cell_id: "1".to_string(),
            status: CodeModeCellStatus::Completed,
            parts: Vec::new(),
            error_text: Some("boom".to_string()),
        };
        assert!(output.is_error());
        assert!(output.to_prompt_format().contains("Error: boom"));
    }

    #[test]
    fn exec_input_accepts_raw_alias_for_native_transport() {
        let input: ExecToolInput =
            serde_json::from_value(serde_json::json!({"raw": "text('hi')"})).unwrap();
        assert_eq!(input.source, "text('hi')");
    }

    /// Finding-12 boundary: the advertised token budget clips text at the
    /// estimated boundary and drops whole images that no longer fit.
    #[test]
    fn output_budget_truncates_text_and_drops_images()  {
        let parts = vec![
            CodeModePart::Text {
                text: "x".repeat(40), // ~10 tokens
            },
            CodeModePart::Text {
                text: "y".repeat(4000), // ~1000 tokens
            },
            CodeModePart::Image {
                image_url: "data:image/png;base64,QUJD".to_string(),
                detail: None,
            },
        ];
        let (kept, truncated) = truncate_parts_to_budget(parts.clone(), 12);
        assert!(truncated);
        assert_eq!(kept.len(), 2);
        assert!(matches!(&kept[0], CodeModePart::Text { text } if text.len() == 40));
        assert!(matches!(&kept[1], CodeModePart::Text { text } if text.len() == 8));

        // An image that does not fit the remaining budget is dropped whole.
        let (kept, truncated) = truncate_parts_to_budget(
            vec![CodeModePart::Image {
                image_url: "data:image/png;base64,QUJD".to_string(),
                detail: None,
            }],
            10,
        );
        assert!(truncated);
        assert!(kept.is_empty());

        // Under budget: everything passes through untouched.
        let (kept, truncated) = truncate_parts_to_budget(parts, 20_000);
        assert!(!truncated);
        assert_eq!(kept.len(), 3);
    }

    /// The exec pragma budget applies to the real cell output.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exec_pragma_budget_truncates_real_output() {
        let handle = test_handle();
        let output = xai_tool_runtime::Tool::run(
            &ExecTool,
            ctx_with(Some(handle)),
            ExecToolInput {
                source: "// @exec: {\"max_output_tokens\": 2}\ntext('a'.repeat(400));"
                    .to_string(),
            },
        )
        .await
        .expect("exec should complete");
        assert_eq!(output.status, CodeModeCellStatus::Completed);
        let rendered = output.to_prompt_format();
        assert!(
            rendered.contains("[output truncated at the requested budget of 2 tokens]"),
            "{rendered}"
        );
        let text_len: usize = output
            .parts
            .iter()
            .filter_map(|p| match p {
                CodeModePart::Text { text } if text.starts_with('a') => Some(text.len()),
                _ => None,
            })
            .sum();
        assert!(text_len <= 8, "clipped to the 2-token budget, got {text_len}");
    }

    fn test_handle() -> CodeModeHandle {
        let session: Arc<dyn CodeModeSession> =
            Arc::new(xai_grok_code_mode::InProcessCodeModeSession::new());
        CodeModeHandle {
            session,
            enabled_tools: Arc::new(parking_lot::RwLock::new(Vec::new())),
            live_cells: Arc::new(parking_lot::Mutex::new(Default::default())),
        }
    }

    fn ctx_with(handle: Option<CodeModeHandle>) -> xai_tool_runtime::ToolCallContext {
        let mut resources = crate::types::resources::Resources::new();
        if let Some(handle) = handle {
            resources.insert(handle);
        }
        let mut ctx = xai_tool_runtime::ToolCallContext::default();
        ctx.extensions.insert(resources.into_shared());
        ctx
    }

    /// Fail closed: without a mounted [`CodeModeHandle`] every exec call errs.
    #[tokio::test]
    async fn exec_fails_closed_without_a_mounted_runtime() {
        let err = xai_tool_runtime::Tool::run(
            &ExecTool,
            ctx_with(None),
            ExecToolInput {
                source: "text('hi')".to_string(),
            },
        )
        .await
        .expect_err("exec without a handle must fail");
        assert!(err.to_string().contains("Code Mode is not active"), "{err}");
    }

    /// exec → completed roundtrip through a real in-process V8 session.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exec_roundtrip_produces_ordered_text_parts() {
        let handle = test_handle();
        let output = xai_tool_runtime::Tool::run(
            &ExecTool,
            ctx_with(Some(handle.clone())),
            ExecToolInput {
                source: "text('one'); text('two');".to_string(),
            },
        )
        .await
        .expect("exec should complete");
        assert_eq!(output.status, CodeModeCellStatus::Completed);
        assert_eq!(
            output.parts,
            vec![
                CodeModePart::Text {
                    text: "one".to_string()
                },
                CodeModePart::Text {
                    text: "two".to_string()
                },
            ]
        );
        assert!(output.error_text.is_none());
        assert!(
            handle.live_cells.lock().is_empty(),
            "completed cells must be untracked"
        );
    }

    /// exec yields on a long-running script, then `wait` with terminate stops
    /// the cell and clears the live-cell tracking.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn yielded_cell_waits_and_terminates() {
        let handle = test_handle();
        let output = xai_tool_runtime::Tool::run(
            &ExecTool,
            ctx_with(Some(handle.clone())),
            ExecToolInput {
                source: "// @exec: {\"yield_time_ms\": 1}\ntext('early'); await new Promise(() => {});"
                    .to_string(),
            },
        )
        .await
        .expect("exec should yield");
        assert_eq!(output.status, CodeModeCellStatus::Yielded);
        assert!(
            output.to_prompt_format().contains("Script running with cell ID"),
            "{}",
            output.to_prompt_format()
        );
        let cell_id = output.cell_id.clone();
        assert!(handle.live_cells.lock().contains(&cell_id));

        let wait_output = xai_tool_runtime::Tool::run(
            &WaitTool,
            ctx_with(Some(handle.clone())),
            WaitToolInput {
                cell_id: cell_id.clone(),
                yield_time_ms: None,
                terminate: Some(true),
                max_tokens: None,
            },
        )
        .await
        .expect("terminate should succeed");
        assert_eq!(wait_output.status, CodeModeCellStatus::Terminated);
        assert!(
            !handle.live_cells.lock().contains(&cell_id),
            "terminated cells must be untracked"
        );
    }

    /// `wait` on an unknown cell surfaces the runtime's missing-cell result
    /// as an error output instead of failing the dispatch.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wait_on_missing_cell_reports_error_output() {
        let handle = test_handle();
        let output = xai_tool_runtime::Tool::run(
            &WaitTool,
            ctx_with(Some(handle)),
            WaitToolInput {
                cell_id: "999".to_string(),
                yield_time_ms: Some(10),
                terminate: None,
                max_tokens: None,
            },
        )
        .await
        .expect("wait dispatch should succeed");
        assert!(output.is_error());
        assert!(
            output.error_text.as_deref().unwrap_or("").contains("not found"),
            "{:?}",
            output.error_text
        );
    }

    /// Once the session is shut down (model switch / session close), a stale
    /// handle fails closed instead of executing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stale_handle_after_shutdown_fails_closed() {
        let session = Arc::new(xai_grok_code_mode::InProcessCodeModeSession::new());
        let handle = CodeModeHandle {
            session: session.clone(),
            enabled_tools: Arc::new(parking_lot::RwLock::new(Vec::new())),
            live_cells: Arc::new(parking_lot::Mutex::new(Default::default())),
        };
        session.shutdown().await.expect("shutdown");
        let err = xai_tool_runtime::Tool::run(
            &ExecTool,
            ctx_with(Some(handle)),
            ExecToolInput {
                source: "text('late')".to_string(),
            },
        )
        .await
        .expect_err("stale handle must fail closed");
        assert!(err.to_string().contains("shutting down"), "{err}");
    }
}
