//! A read-only, text-selectable markdown widget.
//!
//! `iced::widget::markdown::view` (used by the brain panel) does not support
//! text selection — nothing in this codebase does except the terminal
//! widget, which only works because its grid is fixed-size monospace.
//! Reusing that approach for proportional, variable-height markdown would
//! mean building real glyph-layout hit-testing from scratch.
//!
//! Instead this wraps `iced::widget::text_editor` — which already implements
//! real selection, keyboard navigation, and clipboard copy — in read-only
//! mode, paired with a hand-rolled line-scanning `Highlighter` for basic
//! markdown styling (headings, bold, italic, inline code, fenced code,
//! links, list markers). This is intentionally not a full CommonMark
//! renderer: no block layout, no distinct heading sizes, no clickable
//! links, no images. See
//! docs/superpowers/specs/2026-08-26-orchestrator-plan-tracking-design.md
//! for the trade-off this was chosen over.
//!
//! Used by the orchestrator Plan panel (`components::plan_panel`) and the
//! Marginalia PR-comment cards (`components::info_panel`) — both need
//! read-only, selectable, lightly-formatted markdown, just routed to
//! different `Message` variants via the `on_action` callback.

use std::ops::Range;

use iced::advanced::text::highlighter::{Format, Highlighter};
use iced::widget::text_editor;
use iced::{Background, Border, Color, Element, Font};

use crate::app::Message;
use crate::style;
use super::shell::tokens;

pub struct MarkdownHighlighter {
    in_fence: bool,
    current_line: usize,
}

impl Highlighter for MarkdownHighlighter {
    type Settings = ();
    type Highlight = Format<Font>;
    type Iterator<'a> = std::vec::IntoIter<(Range<usize>, Format<Font>)>;

    fn new(_settings: &Self::Settings) -> Self {
        Self { in_fence: false, current_line: 0 }
    }

    fn update(&mut self, _new_settings: &Self::Settings) {}

    fn change_line(&mut self, line: usize) {
        if line == 0 {
            self.in_fence = false;
        }
        self.current_line = line;
    }

    fn highlight_line(&mut self, line: &str) -> Self::Iterator<'_> {
        self.current_line += 1;
        highlight_markdown_line(line, &mut self.in_fence).into_iter()
    }

    fn current_line(&self) -> usize {
        self.current_line
    }
}

fn highlight_markdown_line(
    line: &str,
    in_fence: &mut bool,
) -> Vec<(Range<usize>, Format<Font>)> {
    let trimmed = line.trim_start();

    if trimmed.starts_with("```") {
        *in_fence = !*in_fence;
        return vec![(0..line.len(), Format { color: Some(tokens::DIM2), font: Some(style::MONO) })];
    }
    if *in_fence {
        return vec![(0..line.len(), Format { color: Some(tokens::DIM), font: Some(style::MONO) })];
    }

    let heading_level = trimmed.bytes().take_while(|&b| b == b'#').count();
    if (1..=6).contains(&heading_level) && trimmed.as_bytes().get(heading_level) == Some(&b' ') {
        return vec![(0..line.len(), Format { color: Some(tokens::TEXT), font: Some(style::SANS_BOLD) })];
    }

    let mut spans = Vec::new();
    let bytes = line.as_bytes();
    let indent = line.len() - trimmed.len();
    let mut i = match list_marker_len(trimmed) {
        Some(marker_len) => {
            spans.push((
                indent..indent + marker_len,
                Format { color: Some(tokens::DIM2), font: None },
            ));
            indent + marker_len
        }
        None => 0,
    };

    while i < bytes.len() {
        match bytes[i] {
            b'`' => {
                if let Some(end) = find_close(line, i + 1, "`") {
                    spans.push((i..end, Format { color: Some(tokens::DIM), font: Some(style::MONO) }));
                    i = end;
                    continue;
                }
            }
            b'*' if bytes.get(i + 1) == Some(&b'*') => {
                if let Some(end) = find_close(line, i + 2, "**") {
                    spans.push((i..end, Format { color: None, font: Some(style::SANS_BOLD) }));
                    i = end;
                    continue;
                }
            }
            b'*' => {
                if let Some(end) = find_close(line, i + 1, "*") {
                    spans.push((i..end, Format { color: None, font: Some(style::SANS_ITALIC) }));
                    i = end;
                    continue;
                }
            }
            b'_' => {
                if let Some(end) = find_close(line, i + 1, "_") {
                    spans.push((i..end, Format { color: None, font: Some(style::SANS_ITALIC) }));
                    i = end;
                    continue;
                }
            }
            b'[' => {
                if let Some(close_bracket) = line[i + 1..].find(']').map(|p| p + i + 1) {
                    if bytes.get(close_bracket + 1) == Some(&b'(') {
                        if let Some(close_paren) =
                            line[close_bracket + 2..].find(')').map(|p| p + close_bracket + 2)
                        {
                            spans.push((
                                i..close_paren + 1,
                                Format { color: Some(tokens::BLUE), font: None },
                            ));
                            i = close_paren + 1;
                            continue;
                        }
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    spans
}

fn find_close(line: &str, from: usize, delim: &str) -> Option<usize> {
    line.get(from..)?.find(delim).map(|p| from + p + delim.len())
}

fn list_marker_len(trimmed: &str) -> Option<usize> {
    for prefix in ["- ", "* ", "+ "] {
        if trimmed.starts_with(prefix) {
            return Some(prefix.len());
        }
    }
    let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0
        && trimmed.as_bytes().get(digits) == Some(&b'.')
        && trimmed.as_bytes().get(digits + 1) == Some(&b' ')
    {
        return Some(digits + 2);
    }
    None
}

/// Renders `content` as read-only, selectable, lightly-styled markdown.
/// `on_action` should route non-edit actions (click/drag/select/scroll)
/// back to a handler that applies them to the same `Content` and drops
/// `Action::Edit(_)` — see `Message::PlanEditorAction`/`apply_comment_action`
/// for the two call sites' handlers; this widget itself has no opinion on
/// which `Message` variant carries the action.
pub fn selectable_markdown<'a>(
    content: &'a text_editor::Content,
    on_action: impl Fn(text_editor::Action) -> Message + 'a,
) -> Element<'a, Message> {
    text_editor(content)
        .highlight_with::<MarkdownHighlighter>((), |highlight, _theme| *highlight)
        .font(style::SANS)
        .size(14)
        .padding(0)
        .style(|_theme, _status| text_editor::Style {
            background: Background::Color(Color::TRANSPARENT),
            border: Border::default(),
            icon: tokens::DIM2,
            placeholder: tokens::DIM2,
            value: tokens::TEXT,
            selection: Color { a: 0.35, ..tokens::TEAL },
        })
        .on_action(on_action)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heading_bolds_the_whole_line() {
        let mut in_fence = false;
        let spans = highlight_markdown_line("## Title", &mut in_fence);
        assert_eq!(spans, vec![(0..8, Format { color: Some(tokens::TEXT), font: Some(style::SANS_BOLD) })]);
        assert!(!in_fence);
    }

    #[test]
    fn bold_span_covers_the_asterisks() {
        let mut in_fence = false;
        let spans = highlight_markdown_line("a **b** c", &mut in_fence);
        assert_eq!(spans, vec![(2..7, Format { color: None, font: Some(style::SANS_BOLD) })]);
    }

    #[test]
    fn italic_underscore_and_asterisk_both_match() {
        let mut in_fence = false;
        assert_eq!(
            highlight_markdown_line("_a_", &mut in_fence),
            vec![(0..3, Format { color: None, font: Some(style::SANS_ITALIC) })],
        );
        assert_eq!(
            highlight_markdown_line("*a*", &mut in_fence),
            vec![(0..3, Format { color: None, font: Some(style::SANS_ITALIC) })],
        );
    }

    #[test]
    fn inline_code_span_uses_mono() {
        let mut in_fence = false;
        let spans = highlight_markdown_line("run `cmd` now", &mut in_fence);
        assert_eq!(spans, vec![(4..9, Format { color: Some(tokens::DIM), font: Some(style::MONO) })]);
    }

    #[test]
    fn link_span_covers_brackets_and_url() {
        let mut in_fence = false;
        let spans = highlight_markdown_line("[text](http://x)", &mut in_fence);
        assert_eq!(spans, vec![(0..16, Format { color: Some(tokens::BLUE), font: None })]);
    }

    #[test]
    fn list_marker_is_dimmed_and_excludes_body() {
        let mut in_fence = false;
        let spans = highlight_markdown_line("- item", &mut in_fence);
        assert_eq!(spans[0], (0..2, Format { color: Some(tokens::DIM2), font: None }));

        let spans = highlight_markdown_line("2. item", &mut in_fence);
        assert_eq!(spans[0], (0..3, Format { color: Some(tokens::DIM2), font: None }));
    }

    #[test]
    fn fenced_code_block_toggles_and_colors_whole_lines_mono() {
        let mut in_fence = false;
        let open = highlight_markdown_line("```rust", &mut in_fence);
        assert!(in_fence);
        assert_eq!(open, vec![(0..7, Format { color: Some(tokens::DIM2), font: Some(style::MONO) })]);

        let body = highlight_markdown_line("let x = 1;", &mut in_fence);
        assert!(in_fence);
        assert_eq!(body, vec![(0..10, Format { color: Some(tokens::DIM), font: Some(style::MONO) })]);

        let close = highlight_markdown_line("```", &mut in_fence);
        assert!(!in_fence);
        assert_eq!(close, vec![(0..3, Format { color: Some(tokens::DIM2), font: Some(style::MONO) })]);
    }

    #[test]
    fn unclosed_delimiters_are_left_unstyled() {
        let mut in_fence = false;
        assert_eq!(highlight_markdown_line("a *b c", &mut in_fence), Vec::new());
    }
}
