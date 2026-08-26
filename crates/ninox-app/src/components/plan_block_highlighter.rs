//! A `text_editor` `Highlighter` that paints precomputed byte-range spans
//! (from `markdown_blocks::parse_blocks`) instead of re-scanning raw
//! markdown text — used for the Plan panel's per-block widgets, where
//! markdown syntax has already been stripped out of the displayed text and
//! inline styling is carried as `(Range<usize>, InlineStyle)` pairs against
//! the whole block instead.

use std::ops::Range;

use iced::advanced::text::highlighter::{Format, Highlighter};
use iced::Font;

use super::markdown_blocks::InlineStyle;
use crate::style;
use crate::theme::ColorScheme;

#[derive(Clone, PartialEq)]
pub struct BlockHighlighterSettings {
    pub scheme: ColorScheme,
    pub spans: Vec<(Range<usize>, InlineStyle)>,
}

pub struct BlockHighlighter {
    settings: BlockHighlighterSettings,
    current_line: usize,
    byte_offset: usize,
}

impl Highlighter for BlockHighlighter {
    type Settings = BlockHighlighterSettings;
    type Highlight = Format<Font>;
    type Iterator<'a> = std::vec::IntoIter<(Range<usize>, Format<Font>)>;

    fn new(settings: &Self::Settings) -> Self {
        Self { settings: settings.clone(), current_line: 0, byte_offset: 0 }
    }

    fn update(&mut self, new_settings: &Self::Settings) {
        self.settings = new_settings.clone();
    }

    fn change_line(&mut self, line: usize) {
        if line == 0 {
            self.byte_offset = 0;
        }
        self.current_line = line;
    }

    fn highlight_line(&mut self, line: &str) -> Self::Iterator<'_> {
        let start = self.byte_offset;
        let end = start + line.len();
        let mut spans = Vec::new();
        for (range, style) in &self.settings.spans {
            let s = range.start.max(start);
            let e = range.end.min(end);
            if s < e {
                spans.push((s - start..e - start, format_for(*style, &self.settings.scheme)));
            }
        }
        self.byte_offset = end + 1;
        self.current_line += 1;
        spans.into_iter()
    }

    fn current_line(&self) -> usize {
        self.current_line
    }
}

fn format_for(style: InlineStyle, s: &ColorScheme) -> Format<Font> {
    if style.glyph {
        return Format { color: None, font: Some(style::GLYPH) };
    }
    if style.code {
        // No font swap: `Format` has no per-span size, and Spline Sans Mono
        // renders visibly larger than the surrounding sans body text at the
        // same point size — color-only differentiation avoids that
        // mismatch, at the cost of losing the monospace visual cue. Kept
        // distinct from `s.accent` (used for links just below) so the two
        // don't read as the same thing.
        return Format { color: Some(s.ink_2), font: None };
    }
    if style.link {
        return Format { color: Some(s.accent), font: None };
    }
    let font = match (style.bold, style.italic) {
        (true, _) => Some(style::SANS_BOLD),
        (false, true) => Some(style::SANS_ITALIC),
        (false, false) => None,
    };
    Format { color: None, font }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_are_sliced_relative_to_each_line() {
        let s = crate::theme::light();
        let settings = BlockHighlighterSettings {
            scheme: s,
            spans: vec![
                (2..3, InlineStyle { bold: true, ..Default::default() }),
                (11..15, InlineStyle { code: true, ..Default::default() }),
            ],
        };
        let mut h = BlockHighlighter::new(&settings);
        h.change_line(0);

        // "line one" occupies global bytes 0..8, the separator consumes
        // byte 8, so "line two" starts at global byte 9 — span 11..15 is
        // fully inside it (relative 2..6), with no overlap into line one.
        let first: Vec<_> = h.highlight_line("line one").collect();
        assert_eq!(first, vec![(2..3, Format { color: None, font: Some(style::SANS_BOLD) })]);

        let second: Vec<_> = h.highlight_line("line two").collect();
        assert_eq!(second, vec![(2..6, Format { color: Some(s.ink_2), font: None })]);
    }
}
