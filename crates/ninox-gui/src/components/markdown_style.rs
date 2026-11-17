//! Shared `iced::widget::markdown::Style` for chip-styled inline code.

use iced::widget::markdown::{Highlight, Style};
use iced::{Background, Border, Color, Padding};

use super::shell::tokens;

/// iced paints the inline-code chip `inline_code_padding` wider than the
/// span's layout box, so horizontal padding wider than a space (~3.6px at
/// 13px Inter, counting the 1px border) paints over the spaces around the
/// chip and glues it to the neighbouring words. Vertical padding doesn't
/// affect layout, so it can stay generous.
pub const INLINE_CODE_PADDING: Padding = Padding { top: 1.0, right: 1.0, bottom: 1.0, left: 1.0 };

pub fn inline_code_style(link_color: Color, radius: f32) -> Style {
    Style {
        inline_code_highlight: Highlight {
            background: Background::Color(tokens::PANEL2),
            border: Border { color: tokens::BORDER, width: 1.0, radius: radius.into() },
        },
        inline_code_padding: INLINE_CODE_PADDING,
        inline_code_color: tokens::TEXT,
        link_color,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A 13px Inter space is ~3.6px; the painted chip eats padding + 1px border per side.
    const _: () = assert!(INLINE_CODE_PADDING.left + 1.0 < 3.6 && INLINE_CODE_PADDING.right + 1.0 < 3.6);

    #[test]
    fn inline_code_keeps_surrounding_spaces_in_spans() {
        let items: Vec<_> =
            iced::widget::markdown::parse("No plan stored. `ninox plan` rejected it with `x`: done").collect();
        let style = inline_code_style(tokens::BLUE, 3.0);
        let iced::widget::markdown::Item::Paragraph(paragraph) = &items[0] else {
            panic!("expected a paragraph");
        };
        let spans = paragraph.spans(style);
        let joined: String = spans.iter().map(|s| s.text.as_ref()).collect();
        assert_eq!(joined, "No plan stored. ninox plan rejected it with x: done");
    }
}
