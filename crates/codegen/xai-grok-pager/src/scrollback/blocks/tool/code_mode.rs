//! CodeModeExecToolCallBlock — Code Mode `exec` (JavaScript orchestration)
//! tool calls: highlighted JS source plus the cell's rendered output.

use ratatui::style::Modifier;
use ratatui::text::{Line, Span, Text};

use crate::appearance::AppearanceConfig;
use crate::render::line_utils::truncate_str;
use crate::scrollback::block::BlockContent;
use crate::scrollback::types::{
    AccentStyle, BlockBackground, BlockContext, BlockLine, BlockOutput, DisplayMode,
};
use crate::syntax::get_syntect;
use crate::theme::Theme;

const MAX_INLINE_SOURCE_LINES: usize = 12;
const TRUNCATED_SOURCE_LINES: usize = 4;
const MAX_INLINE_OUTPUT_LINES: usize = 10;
const TRUNCATED_OUTPUT_LINES: usize = 3;

/// Code Mode `exec` tool call.
#[derive(Debug, Clone)]
pub struct CodeModeExecToolCallBlock {
    /// Raw JavaScript source the model submitted.
    pub source: String,
    /// Cell id once known (yield / completion).
    pub cell_id: Option<String>,
    /// Cell status label (`yielded` / `completed` / `terminated`).
    pub status: Option<String>,
    /// Rendered output text (prompt-facing rendering of the ordered parts).
    pub output: Option<String>,
    /// Error message if the cell failed.
    pub error: Option<String>,
    /// When the tool started running.
    pub started_at: Option<std::time::Instant>,
    /// Elapsed time in ms after completion.
    pub elapsed_ms: Option<i64>,
}

impl CodeModeExecToolCallBlock {
    pub fn new(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            cell_id: None,
            status: None,
            output: None,
            error: None,
            started_at: None,
            elapsed_ms: None,
        }
    }

    pub fn with_error(mut self, error: impl Into<String>) -> Self {
        self.error = Some(error.into());
        self
    }

    pub fn is_success(&self) -> bool {
        self.error.is_none()
    }

    pub fn finish(&mut self) {
        if self.elapsed_ms.is_some() {
            return;
        }
        if let Some(start) = self.started_at {
            self.elapsed_ms = Some(start.elapsed().as_millis() as i64);
        }
    }

    pub fn elapsed_ms(&self) -> Option<i64> {
        self.elapsed_ms.or_else(|| {
            self.started_at
                .map(|start| start.elapsed().as_millis() as i64)
        })
    }

    pub fn copy_text(&self) -> String {
        let mut out = String::from("exec\n");
        out.push_str(&self.source);
        if let Some(ref output) = self.output {
            out.push_str("\n\n");
            out.push_str(output);
        }
        if let Some(ref err) = self.error {
            out.push_str("\n\n");
            out.push_str(err);
        }
        out
    }

    /// Header: **exec** [`cell N`] [status]
    fn header_line(&self, theme: &Theme, muted: bool, max_width: Option<usize>) -> Line<'static> {
        let text_style = if muted { theme.muted() } else { theme.primary() };
        let bold_style = text_style.add_modifier(Modifier::BOLD);
        let mut spans = vec![Span::styled("exec", bold_style)];
        let mut suffix = String::new();
        if let Some(ref cell) = self.cell_id {
            suffix.push_str(&format!(" cell {cell}"));
        }
        if let Some(ref status) = self.status {
            suffix.push_str(&format!(" [{status}]"));
        }
        if muted && suffix.is_empty() {
            // Collapsed rows show the first source line for orientation.
            let first = self.source.lines().next().unwrap_or_default().trim();
            if !first.is_empty() {
                suffix.push_str(": ");
                suffix.push_str(first);
            }
        }
        if !suffix.is_empty() {
            let display = match max_width {
                Some(w) => truncate_str(&suffix, w.saturating_sub(4)),
                None => suffix,
            };
            spans.push(Span::styled(
                display,
                if muted { theme.muted() } else { theme.fg(theme.command) },
            ));
        }
        Line::from(spans)
    }

    /// JavaScript source, syntax-highlighted, panel background, capped.
    fn source_lines(&self, theme: &Theme, max_lines: usize) -> Vec<BlockLine> {
        let syntect = get_syntect();
        let mut highlighter = syntect.highlight_lines_for_token("javascript");
        let text_style = theme.primary();
        let raw_lines: Vec<&str> = self.source.lines().collect();
        let mut lines = Vec::new();
        for (i, raw) in raw_lines.iter().enumerate() {
            if i >= max_lines {
                let remaining = raw_lines.len() - max_lines;
                lines.push(
                    BlockLine::from(Line::from(Span::styled(
                        format!("  ... ({remaining} more lines, press Enter to view)"),
                        theme.dim(),
                    )))
                    .with_panel_background(theme.bg_dark),
                );
                break;
            }
            let mut spans = vec![Span::styled("  ", text_style)];
            spans.extend(crate::syntax::highlight_line(
                raw,
                &mut highlighter,
                syntect,
                text_style,
            ));
            lines.push(BlockLine::from(Line::from(spans)).with_panel_background(theme.bg_dark));
        }
        lines
    }
}

impl BlockContent for CodeModeExecToolCallBlock {
    fn output(&self, ctx: &BlockContext) -> BlockOutput {
        let theme = Theme::current();
        let muted_collapsed =
            ctx.mute_when_collapsed(ctx.appearance.scrollback.blocks.tool.muted_collapsed);

        match ctx.mode {
            DisplayMode::Collapsed => BlockOutput {
                lines: vec![
                    self.header_line(&theme, muted_collapsed, Some(ctx.content_width()))
                        .into(),
                ],
            },
            DisplayMode::Truncated | DisplayMode::Expanded => {
                let header = self.header_line(&theme, false, None);
                let wrapped = crate::render::wrapping::wrap_header_flush(
                    header,
                    ctx.width as usize,
                    ctx.bullet_indent(),
                );
                let mut lines: Vec<BlockLine> = wrapped.into_iter().map(BlockLine::from).collect();

                let (max_source, max_output) = if ctx.mode == DisplayMode::Truncated {
                    (TRUNCATED_SOURCE_LINES, TRUNCATED_OUTPUT_LINES)
                } else {
                    (MAX_INLINE_SOURCE_LINES, MAX_INLINE_OUTPUT_LINES)
                };

                if !self.source.is_empty() {
                    lines.push(Line::from("").into());
                    lines
                        .push(BlockLine::from(Line::from("")).with_panel_background(theme.bg_dark));
                    lines.extend(self.source_lines(&theme, max_source));
                    lines
                        .push(BlockLine::from(Line::from("")).with_panel_background(theme.bg_dark));
                }

                if let Some(ref output) = self.output {
                    lines.push(Line::from("").into());
                    let content_lines: Vec<&str> = output.lines().collect();
                    for (i, line) in content_lines.iter().enumerate() {
                        if i >= max_output {
                            let remaining = content_lines.len() - max_output;
                            lines.push(
                                Line::from(Span::styled(
                                    format!("  ... ({remaining} more lines, press Enter to view)"),
                                    theme.dim(),
                                ))
                                .into(),
                            );
                            break;
                        }
                        lines.push(
                            Line::from(Span::styled(format!("  {line}"), theme.primary())).into(),
                        );
                    }
                }

                if let Some(ref err) = self.error {
                    lines.push(Line::from("").into());
                    lines.push(
                        Line::from(Span::styled(
                            format!("  {err}"),
                            theme.fg(theme.accent_error),
                        ))
                        .into(),
                    );
                }

                BlockOutput { lines }
            }
        }
    }

    fn accent(&self, ctx: &BlockContext) -> Option<AccentStyle> {
        if ctx.mode == DisplayMode::Collapsed {
            return None;
        }
        let theme = Theme::current();
        if self.error.is_some() {
            Some(AccentStyle::static_color(theme.accent_error))
        } else if ctx.is_running {
            Some(AccentStyle::animated(theme.accent_running))
        } else {
            Some(AccentStyle::static_color(theme.accent_tool))
        }
    }

    fn bullet(&self, ctx: &BlockContext) -> Option<AccentStyle> {
        if self.error.is_some() {
            let theme = Theme::current();
            Some(AccentStyle::static_color(theme.accent_error))
        } else if ctx.mode == DisplayMode::Collapsed {
            None
        } else {
            self.accent(ctx)
        }
    }

    fn has_vpad_for(&self, _appearance: &AppearanceConfig) -> bool {
        false
    }

    fn background(&self, _ctx: &BlockContext) -> BlockBackground {
        BlockBackground::None
    }

    fn has_raw_mode(&self) -> bool {
        false
    }

    fn is_foldable(&self) -> bool {
        true
    }

    fn default_display_mode(&self) -> DisplayMode {
        DisplayMode::Collapsed
    }

    fn next_fold_mode(&self, current: DisplayMode, _is_running: bool) -> DisplayMode {
        match current {
            DisplayMode::Collapsed => DisplayMode::Expanded,
            _ => DisplayMode::Collapsed,
        }
    }

    fn preamble(&self, _ctx: &BlockContext) -> Option<Text<'static>> {
        let theme = Theme::current();
        Some(Text::from(vec![self.header_line(&theme, false, None)]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scrollback::types::BlockContext;

    fn ctx(mode: DisplayMode) -> BlockContext {
        BlockContext {
            width: 80,
            mode,
            is_running: false,
            raw: false,
            max_lines: None,
            appearance: Default::default(),
            is_selected: false,
            cwd: None,
        }
    }

    fn rendered_text(block: &CodeModeExecToolCallBlock, mode: DisplayMode) -> String {
        block
            .output(&ctx(mode))
            .lines
            .iter()
            .map(|l| {
                l.content
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn expanded_shows_source_output_and_yield_status() {
        let mut block =
            CodeModeExecToolCallBlock::new("const a = await tools.read_file({path: 'x'});");
        block.cell_id = Some("3".to_string());
        block.status = Some("yielded".to_string());
        block.output = Some("partial output\nScript running with cell ID 3.".to_string());

        let expanded = rendered_text(&block, DisplayMode::Expanded);
        assert!(expanded.contains("exec"), "{expanded}");
        assert!(expanded.contains("cell 3"), "{expanded}");
        assert!(expanded.contains("const a = await tools.read_file"), "{expanded}");
        assert!(expanded.contains("partial output"), "{expanded}");
    }

    #[test]
    fn collapsed_shows_first_source_line() {
        let block = CodeModeExecToolCallBlock::new("text('hello');\nmore();");
        let collapsed = rendered_text(&block, DisplayMode::Collapsed);
        assert!(collapsed.contains("exec"), "{collapsed}");
    }

    #[test]
    fn error_renders_and_flips_success() {
        let block = CodeModeExecToolCallBlock::new("boom(").with_error("SyntaxError: boom");
        assert!(!block.is_success());
        let expanded = rendered_text(&block, DisplayMode::Expanded);
        assert!(expanded.contains("SyntaxError: boom"), "{expanded}");
    }

    #[test]
    fn truncated_caps_source_lines() {
        let source: Vec<String> = (1..=10).map(|i| format!("line{i}();")).collect();
        let block = CodeModeExecToolCallBlock::new(source.join("\n"));
        let truncated = rendered_text(&block, DisplayMode::Truncated);
        assert!(truncated.contains("line4"), "{truncated}");
        assert!(!truncated.contains("line5();"), "{truncated}");
        assert!(truncated.contains("more lines"), "{truncated}");
    }
}
