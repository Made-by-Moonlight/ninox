//! The orchestrator "Plan" panel — a live, rendered, selectable view of an
//! orchestrator's registered goals/plan markdown doc (`ninox plan
//! register`). Spec: docs/superpowers/specs/2026-08-26-orchestrator-plan-
//! tracking-design.md.
//!
//! Each markdown block (heading/paragraph/list item/code block) renders as
//! its own `text_editor` widget, sized per block kind — a single
//! `text_editor` can only have one font size for its whole content, so
//! "real" heading sizes require one widget per block. Markdown syntax is
//! already stripped by `components::markdown_blocks` (no literal `#`/`**`,
//! real ☐/☑ checkboxes), unlike `components::selectable_markdown`'s
//! line-scanner (used for the Marginalia comment cards, where full block
//! rendering isn't worth the complexity).
//!
//! Links can't be made clickable *inside* a `text_editor` (it has no
//! click-through-link mechanism, only plain click/drag/select actions), so
//! each block that contains one or more links surfaces them as a row of
//! clickable chips beneath it instead — routed to `Message::OpenUrl` for
//! web URLs and `Message::OpenInEditor` for local file paths.

use iced::widget::{button, column, container, row, scrollable, text, text_editor, Space};
use iced::{Background, Border, Color, Element, Length};

use crate::app::{App, Message, PlanBlockView};
use crate::components::markdown_blocks::BlockKind;
use crate::components::plan_block_highlighter::{BlockHighlighter, BlockHighlighterSettings};
use crate::theme::ColorScheme;

/// `updated_at` (unix millis) as a local `HH:MM` mono timestamp, matching
/// the notification panel's convention.
fn format_timestamp(updated_at_ms: i64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_millis_opt(updated_at_ms)
        .single()
        .map(|dt| dt.format("%H:%M").to_string())
        .unwrap_or_default()
}

fn is_web_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://") || url.starts_with("mailto:")
}

fn link_chips<'a>(links: &'a [(String, String)], s: &ColorScheme) -> Element<'a, Message> {
    let chips: Vec<Element<Message>> = links
        .iter()
        .map(|(label, url)| {
            let message =
                if is_web_url(url) { Message::OpenUrl(url.clone()) } else { Message::OpenInEditor(url.clone()) };
            // "↗" isn't covered by the bundled Newsreader/Archivo/Spline Sans
            // Mono fonts — same tofu-box problem `style::GLYPH` exists to
            // solve for the app's other dingbats, so route it through that
            // font specifically rather than the label's own MONO font.
            let icon_label = row![
                text("↗").font(crate::style::GLYPH).size(11).color(s.accent),
                text(format!(" {label}")).size(11).font(crate::style::MONO).color(s.accent),
            ];
            button(icon_label)
                .on_press(message)
                .padding(0)
                .style(|_theme, _status| button::Style {
                    background: None,
                    border: Border::default(),
                    ..Default::default()
                })
                .into()
        })
        .collect();
    row(chips).spacing(14).into()
}

/// Font size + base font for a block kind. Headings step down from a large
/// serif H1 to a body-sized H6; paragraphs/list items/code share the body
/// row but differ in font (sans vs. mono).
fn style_for(kind: BlockKind) -> (f32, iced::Font) {
    match kind {
        BlockKind::Heading(1) => (22.0, crate::style::SERIF_MEDIUM),
        BlockKind::Heading(2) => (19.0, crate::style::SERIF_MEDIUM),
        BlockKind::Heading(3) => (17.0, crate::style::SERIF_MEDIUM),
        BlockKind::Heading(_) => (15.0, crate::style::SERIF_MEDIUM),
        BlockKind::Code => (13.0, crate::style::MONO),
        BlockKind::Paragraph | BlockKind::ListItem { .. } => (14.0, crate::style::SANS),
    }
}

fn block_widget<'a>(
    orchestrator_id: &str,
    index: usize,
    block: &'a PlanBlockView,
    s: &'a ColorScheme,
) -> Element<'a, Message> {
    let (size, font) = style_for(block.kind);
    let scheme = *s;
    let settings = BlockHighlighterSettings { scheme, spans: block.spans.clone() };
    let orchestrator_id = orchestrator_id.to_string();

    let editor: Element<Message> = text_editor(&block.content)
        .highlight_with::<BlockHighlighter>(settings, |highlight, _theme| *highlight)
        .font(font)
        .size(size)
        .padding(0)
        .style(move |_theme, _status| text_editor::Style {
            background: Background::Color(Color::TRANSPARENT),
            border: Border::default(),
            icon: scheme.faint,
            placeholder: scheme.faint,
            value: scheme.ink,
            selection: Color { a: 0.35, ..scheme.accent },
        })
        .on_action(move |action| Message::PlanEditorAction {
            orchestrator_id: orchestrator_id.clone(),
            block: index,
            action,
        })
        .into();

    if block.links.is_empty() {
        editor
    } else {
        column![editor, link_chips(&block.links, s)].spacing(6).into()
    }
}

pub fn plan_pane<'a>(app: &'a App, orchestrator_id: &str, s: &'a ColorScheme) -> Element<'a, Message> {
    let doc = app.plan_docs.get(orchestrator_id);
    let registration = app.engine.store.get_orchestrator_plan(orchestrator_id).ok().flatten();

    let header: Element<Message> = match (&registration, doc.and_then(|d| d.error.as_ref())) {
        (None, _) => Space::new(0, 0).into(),
        (Some(reg), error) => {
            // The panel is narrow (it shares width with the terminal), so
            // the full absolute path doesn't fit alongside the timestamp —
            // show just the file name and rely on the path itself being
            // visible via "Open" (the target app's own title bar/tab).
            let file_name = std::path::Path::new(&reg.file_path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| reg.file_path.clone());
            let mut items = vec![
                text(file_name)
                    .size(12)
                    .font(crate::style::MONO)
                    .color(s.ink_2)
                    .wrapping(iced::widget::text::Wrapping::None)
                    .into(),
                Space::new(10, 0).into(),
                button(text("Open").size(12).color(s.accent))
                    .on_press(Message::OpenInEditor(reg.file_path.clone()))
                    .style(|_theme, _status| button::Style {
                        background: None,
                        border: Border::default(),
                        ..Default::default()
                    })
                    .padding(0)
                    .into(),
                Space::new(Length::Fill, 0).into(),
                text(format!("updated {}", format_timestamp(reg.updated_at)))
                    .size(11)
                    .font(crate::style::MONO)
                    .color(s.faint)
                    .wrapping(iced::widget::text::Wrapping::None)
                    .into(),
            ];
            if let Some(error) = error {
                items.insert(
                    1,
                    text(format!(" — unreadable: {error}"))
                        .size(11)
                        .color(s.status_ci_failed)
                        .into(),
                );
            }
            row(items).align_y(iced::Alignment::Center).into()
        }
    };

    let body: Element<Message> = match (&registration, doc) {
        (None, _) => container(
            column![
                text("No plan doc registered").size(14).font(crate::style::SERIF_ITALIC).color(s.faint),
                Space::new(0, 6),
                text("The orchestrator can run `ninox plan register <file>`.")
                    .size(12)
                    .font(crate::style::MONO)
                    .color(s.faint),
            ]
            .align_x(iced::Alignment::Center),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .into(),
        (Some(_), Some(doc)) if doc.error.is_some() => container(
            text("Registered plan doc is missing or unreadable").size(13).color(s.faint),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .into(),
        (Some(_), Some(doc)) => {
            let blocks: Vec<Element<Message>> = doc
                .blocks
                .iter()
                .enumerate()
                .map(|(i, block)| block_widget(orchestrator_id, i, block, s))
                .collect();
            scrollable(container(column(blocks).spacing(10)).width(Length::Fill).padding(16))
                .width(Length::Fill)
                .height(Length::Fill)
                .into()
        }
        (Some(_), None) => container(text("Loading plan…").size(13).color(s.faint))
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into(),
    };

    container(
        column![
            container(header).padding([10, 16]).width(Length::Fill).style(move |_theme| {
                iced::widget::container::Style {
                    background: Some(Background::Color(s.paper_2)),
                    border: Border { color: s.rule, width: 1.0, radius: 0.0.into() },
                    ..Default::default()
                }
            }),
            body,
        ]
        .width(Length::Fill)
        .height(Length::Fill),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .style(move |_theme| iced::widget::container::Style {
        background: Some(Background::Color(s.paper)),
        ..Default::default()
    })
    .into()
}
