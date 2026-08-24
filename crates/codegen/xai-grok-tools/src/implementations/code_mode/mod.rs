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

/// Estimated token cost of one part, in the same units
/// [`truncate_parts_to_budget`] spends. Kept as one function so the
/// pre-pass in [`budgeted_parts`] and the truncation loop can never drift.
fn part_cost(part: &CodeModePart) -> u64 {
    match part {
        CodeModePart::Image { .. } => xai_token_estimation::IMAGE_TOKEN_ESTIMATE,
        CodeModePart::Text { text } => xai_token_estimation::estimate_tokens(text),
    }
}

/// Total estimated cost of an ordered part list.
fn parts_cost(parts: &[CodeModePart]) -> u64 {
    parts.iter().map(part_cost).sum()
}

/// Longest prefix of `text` whose byte length is at most `max_bytes`, cut on a
/// char boundary. Byte- (not char-) bounded because `estimate_tokens` counts
/// bytes: taking N *chars* of multi-byte text can cost up to 4N tokens' worth
/// of bytes and blow the cap that the caller just computed.
fn clip_to_bytes(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
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
                let keep_bytes =
                    xai_token_estimation::estimate_chars(budget.saturating_sub(used)) as usize;
                let clipped = clip_to_bytes(&text, keep_bytes);
                if !clipped.is_empty() {
                    out.push(CodeModePart::Text {
                        text: clipped.to_string(),
                    });
                }
                return (out, true);
            }
        }
    }
    (out, false)
}

/// The note appended when output had to be dropped. Its own cost counts
/// against the budget (see [`budgeted_parts`]).
fn truncation_marker(max_tokens: usize) -> String {
    format!("[output truncated at the requested budget of {max_tokens} tokens]")
}

/// Apply the advertised budget as a **hard** cap: the estimated cost of the
/// returned parts is never greater than `max_tokens`.
///
/// The truncation marker is part of the output, so it is paid for out of the
/// same budget rather than appended on top of an already-exhausted one (which
/// is how a small budget used to return *more* than it asked for). Order of
/// operations:
///
/// 1. Everything fits ⇒ return it untouched, no marker (a budget that is not
///    exceeded must behave exactly as before).
/// 2. Otherwise reserve the marker's cost, clip the content to what is left,
///    and append the marker: content + marker ≤ `max_tokens`.
/// 3. The marker alone does not fit ⇒ return only the prefix of it that does
///    (nothing at all when the budget rounds down to zero tokens).
fn budgeted_parts(
    content_items: Vec<FunctionCallOutputContentItem>,
    max_tokens: usize,
) -> Vec<CodeModePart> {
    let parts = output_parts(content_items);
    let budget = max_tokens as u64;
    if parts_cost(&parts) <= budget {
        return parts;
    }
    let marker = truncation_marker(max_tokens);
    let marker_cost = xai_token_estimation::estimate_tokens(&marker);
    if marker_cost > budget {
        let keep_bytes = xai_token_estimation::estimate_chars(budget) as usize;
        let clipped = clip_to_bytes(&marker, keep_bytes);
        if clipped.is_empty() {
            return Vec::new();
        }
        return vec![CodeModePart::Text {
            text: clipped.to_string(),
        }];
    }
    let (mut parts, _) =
        truncate_parts_to_budget(parts, budget.saturating_sub(marker_cost) as usize);
    parts.push(CodeModePart::Text { text: marker });
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

    fn text_item(text: &str) -> FunctionCallOutputContentItem {
        FunctionCallOutputContentItem::InputText {
            text: text.to_string(),
        }
    }

    /// The advertised budget is a hard cap: the truncation marker is paid for
    /// out of the same budget, so no `max_tokens` ever returns more than it
    /// asked for. Boundaries: 0, "smaller than the marker", "exactly the
    /// marker", and "exactly fits" (no marker at all).
    #[test]
    fn budgeted_parts_never_exceeds_the_requested_budget() {
        let big = || vec![text_item(&"y".repeat(4000))]; // ~1000 tokens

        // 0 tokens: nothing at all can be returned, not even the marker.
        let parts = budgeted_parts(big(), 0);
        assert_eq!(parts_cost(&parts), 0, "{parts:?}");
        assert!(parts.is_empty(), "{parts:?}");

        // Tiny budgets: only a prefix of the marker survives, and it fits.
        for budget in [1usize, 2, 5, 12] {
            let parts = budgeted_parts(big(), budget);
            assert!(
                parts_cost(&parts) <= budget as u64,
                "budget {budget} overflowed: {parts:?}"
            );
            assert_eq!(parts.len(), 1, "budget {budget}: {parts:?}");
            let CodeModePart::Text { text } = &parts[0] else {
                panic!("budget {budget} must yield text: {parts:?}");
            };
            assert!(
                truncation_marker(budget).starts_with(text.as_str()),
                "budget {budget} must return a prefix of the marker, got {text:?}"
            );
        }

        // Exactly the marker's cost: the marker is returned whole and no
        // content rides along with it.
        let marker_only = truncation_marker(0);
        let exact = xai_token_estimation::estimate_tokens(&marker_only) as usize;
        let parts = budgeted_parts(big(), exact);
        assert!(parts_cost(&parts) <= exact as u64, "{parts:?}");

        // Room for content + marker: both are present and the total still fits.
        let parts = budgeted_parts(big(), 100);
        assert!(parts_cost(&parts) <= 100, "{parts:?}");
        assert!(parts.len() >= 2, "{parts:?}");
        assert!(
            matches!(parts.last(), Some(CodeModePart::Text { text })
                if text == &truncation_marker(100)),
            "{parts:?}"
        );

        // Exactly fits: untouched, and no marker is appended.
        let parts = budgeted_parts(vec![text_item(&"z".repeat(40))], 10);
        assert_eq!(parts.len(), 1, "{parts:?}");
        assert!(matches!(&parts[0], CodeModePart::Text { text } if text.len() == 40));

        // Under budget: untouched, no marker.
        let parts = budgeted_parts(vec![text_item("short")], 20_000);
        assert_eq!(parts.len(), 1, "{parts:?}");
        assert!(matches!(&parts[0], CodeModePart::Text { text } if text == "short"));
    }

    /// A clip lands on a char boundary and stays inside the *byte* budget the
    /// estimator counts in — taking N chars of multi-byte text would cost up
    /// to 4N bytes and break the cap.
    #[test]
    fn multibyte_text_is_clipped_within_the_byte_budget() {
        // 40 chars x 3 bytes = 120 bytes ≈ 30 tokens.
        let text = "。".repeat(40);
        let (kept, truncated) = truncate_parts_to_budget(vec![CodeModePart::Text { text }], 4);
        assert!(truncated);
        assert!(parts_cost(&kept) <= 4, "{kept:?}");
        let CodeModePart::Text { text } = &kept[0] else {
            panic!("expected text: {kept:?}");
        };
        // 16 bytes of budget, clipped down to the 15-byte char boundary.
        assert_eq!(text.len(), 15);
        assert!(text.chars().all(|c| c == '。'));
    }

    /// The exec pragma budget applies to the real cell output, as a hard cap.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exec_pragma_budget_truncates_real_output() {
        async fn exec_with_budget(budget: usize) -> CodeModeCallOutput {
            let handle = test_handle();
            xai_tool_runtime::Tool::run(
                &ExecTool,
                ctx_with(Some(handle)),
                ExecToolInput {
                    source: format!(
                        "// @exec: {{\"max_output_tokens\": {budget}}}\ntext('a'.repeat(400));"
                    ),
                },
            )
            .await
            .expect("exec should complete")
        }

        // Room for content + marker: both land, and the whole result fits.
        let output = exec_with_budget(50).await;
        assert_eq!(output.status, CodeModeCellStatus::Completed);
        let rendered = output.to_prompt_format();
        assert!(
            rendered.contains("[output truncated at the requested budget of 50 tokens]"),
            "{rendered}"
        );
        assert!(parts_cost(&output.parts) <= 50, "{:?}", output.parts);
        let text_len: usize = output
            .parts
            .iter()
            .filter_map(|p| match p {
                CodeModePart::Text { text } if text.starts_with('a') => Some(text.len()),
                _ => None,
            })
            .sum();
        assert!(text_len > 0, "content should survive a 50-token budget");

        // A budget too small for the marker itself returns only a prefix of
        // it — never more than the caller asked for (maintenance finding).
        let output = exec_with_budget(2).await;
        assert!(parts_cost(&output.parts) <= 2, "{:?}", output.parts);
        assert!(
            output.parts.iter().all(|p| matches!(
                p,
                CodeModePart::Text { text } if truncation_marker(2).starts_with(text.as_str())
            )),
            "{:?}",
            output.parts
        );
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
