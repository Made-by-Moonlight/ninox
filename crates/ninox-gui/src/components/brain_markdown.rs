//! The Brain reading pane's Markdown view: iced 0.13's `markdown::view`
//! (same layout and sizes), except that an unresolved wikilink
//! (`InternalLink::Unresolved`) is drawn dim and isn't a link. iced's own
//! view gives every link the same colour and has no per-span hook.

use iced::widget::markdown::{Item, Settings, Style, Url};
use iced::widget::{column, container, rich_text, row, scrollable, text};
use iced::{Color, Element, Font, Length};

use crate::components::internal_link::{decode, InternalLink};

fn spans(t: &iced::widget::markdown::Text, style: Style, dim: Color) -> Vec<iced::widget::text::Span<'static, Url>> {
    t.spans(style)
        .iter()
        .cloned()
        .map(|mut span| {
            if span.link.as_ref().is_some_and(|u| matches!(decode(u), Some(InternalLink::Unresolved(_)))) {
                span.link = None;
                span.color = Some(dim);
            }
            span
        })
        .collect()
}

pub fn view<'a>(items: &'a [Item], settings: Settings, style: Style, dim: Color) -> Element<'a, Url> {
    let Settings { text_size, h1_size, h2_size, h3_size, h4_size, h5_size, h6_size, code_size } = settings;
    let spacing = text_size * 0.625;
    let blocks = items.iter().enumerate().map(|(i, item)| -> Element<'a, Url> {
        use iced::widget::markdown::HeadingLevel as H;
        match item {
            Item::Heading(level, heading) => container(rich_text(spans(heading, style, dim)).size(match level {
                H::H1 => h1_size,
                H::H2 => h2_size,
                H::H3 => h3_size,
                H::H4 => h4_size,
                H::H5 => h5_size,
                H::H6 => h6_size,
            }))
            .padding(iced::Padding::default().top(if i > 0 { text_size.0 / 2.0 } else { 0.0 }))
            .into(),
            Item::Paragraph(p) => rich_text(spans(p, style, dim)).size(text_size).into(),
            Item::List { start: None, items } => column(items.iter().map(|items| {
                row![text("•").size(text_size), view(items, settings, style, dim)].spacing(spacing).into()
            }))
            .spacing(spacing)
            .into(),
            Item::List { start: Some(start), items } => column(items.iter().enumerate().map(|(i, items)| {
                row![text(format!("{}.", i as u64 + *start)).size(text_size), view(items, settings, style, dim)]
                    .spacing(spacing)
                    .into()
            }))
            .spacing(spacing)
            .into(),
            Item::CodeBlock(code) => container(
                scrollable(
                    container(rich_text(spans(code, style, dim)).font(Font::MONOSPACE).size(code_size)).padding(spacing.0 / 2.0),
                )
                .direction(scrollable::Direction::Horizontal(
                    scrollable::Scrollbar::default().width(spacing.0 / 2.0).scroller_width(spacing.0 / 2.0),
                )),
            )
            .width(Length::Fill)
            .padding(spacing.0 / 2.0)
            .style(container::dark)
            .into(),
        }
    });
    column(blocks).width(Length::Fill).spacing(text_size).into()
}
