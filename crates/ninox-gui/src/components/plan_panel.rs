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

use iced::widget::{button, column, container, row, scrollable, text, text_editor, tooltip, Space};
use iced::{Background, Border, Color, Element, Length};

use crate::app::{App, Message, PlanBlockView};
use crate::components::markdown_blocks::BlockKind;
use crate::components::plan_block_highlighter::{BlockHighlighter, BlockHighlighterSettings};
use super::shell::tokens;

/// "updated Nov 4 17:29" for a real timestamp; "updated —" for `None`, so a
/// zero/unset time never renders as an epoch date.
pub fn updated_label(updated_ms: Option<i64>) -> String {
    use chrono::{Local, TimeZone};
    let when = updated_ms
        .filter(|ms| *ms > 0)
        .and_then(|ms| Local.timestamp_millis_opt(ms).single())
        .map(|dt| dt.format("%b %-d %H:%M").to_string());
    format!("updated {}", when.as_deref().unwrap_or("—"))
}

fn is_web_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://") || url.starts_with("mailto:")
}

fn link_chips<'a>(links: &'a [(String, String)], editor_is_terminal: bool) -> Element<'a, Message> {
    let chips: Vec<Element<Message>> = links
        .iter()
        .map(|(label, url)| {
            let web = is_web_url(url);
            // A local-file link is just as useless to click as the "Open in
            // editor" button when Neovim is selected (see session_detail.rs):
            // the desktop app has nowhere to run it.
            let can_open = web || !editor_is_terminal;
            let color = if can_open { tokens::TEAL } else { tokens::DIM2 };
            let message = if web { Message::OpenUrl(url.clone()) } else { Message::OpenInEditor(url.clone()) };
            // "↗" goes through `style::GLYPH` rather than the label's own
            // MONO font, which off macOS is the system monospace and may
            // not cover it.
            let icon_label = row![
                text("↗").font(crate::style::GLYPH).size(11).color(color),
                text(format!(" {label}")).size(11).font(crate::style::MONO).color(color),
            ];
            button(icon_label)
                .on_press_maybe(can_open.then_some(message))
                .padding(0)
                .style(|_theme, _status| button::Style {
                    background: None,
                    border: Border::default(),
                    ..Default::default()
                })
                .into()
        })
        .collect();
    // `.wrap()`: a plain `Row` squeezes later chips once they overflow, and
    // their labels then break one glyph per line.
    row(chips).spacing(14).wrap().into()
}

/// Font size + base font for a block kind. Headings step down from a large
/// sans H1 to a body-sized H6; paragraphs/list items/code share the body
/// row but differ in font (sans vs. mono).
pub(super) fn style_for(kind: BlockKind) -> (f32, iced::Font) {
    match kind {
        BlockKind::Heading(1) => (22.0, crate::style::SANS_SEMIBOLD),
        BlockKind::Heading(2) => (19.0, crate::style::SANS_SEMIBOLD),
        BlockKind::Heading(3) => (17.0, crate::style::SANS_SEMIBOLD),
        BlockKind::Heading(_) => (15.0, crate::style::SANS_SEMIBOLD),
        BlockKind::Code => (13.0, crate::style::MONO),
        BlockKind::Paragraph | BlockKind::ListItem { .. } => (14.0, crate::style::SANS),
    }
}

fn block_widget<'a>(
    orchestrator_id: &str,
    index: usize,
    block: &'a PlanBlockView,
    editor_is_terminal: bool,
) -> Element<'a, Message> {
    let (size, font) = style_for(block.kind);
    let settings = BlockHighlighterSettings { spans: block.spans.clone() };
    let orchestrator_id = orchestrator_id.to_string();

    let body_color = if matches!(block.kind, BlockKind::Heading(_)) { tokens::TEXT } else { tokens::hex(0xd6d9e0) };
    let editor: Element<Message> = text_editor(&block.content)
        .highlight_with::<BlockHighlighter>(settings, |highlight, _theme| *highlight)
        .font(font)
        .size(size)
        .padding(0)
        .style(move |_theme, _status| text_editor::Style {
            background: Background::Color(Color::TRANSPARENT),
            border: Border::default(),
            icon: tokens::DIM2,
            placeholder: tokens::DIM2,
            value: body_color,
            selection: Color { a: 0.35, ..tokens::TEAL },
        })
        .on_action(move |action| Message::PlanEditorAction {
            orchestrator_id: orchestrator_id.clone(),
            block: index,
            action,
        })
        .into();

    let editor: Element<Message> = if block.kind == BlockKind::Code {
        container(editor)
            .width(Length::Fill)
            .padding([10, 12])
            .style(|_| container::Style {
                background: Some(Background::Color(tokens::PANEL2)),
                border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
                ..Default::default()
            })
            .into()
    } else {
        editor
    };
    if block.links.is_empty() {
        editor
    } else {
        column![editor, container(link_chips(&block.links, editor_is_terminal)).padding(iced::Padding { left: 26.0, ..iced::Padding::ZERO })]
            .spacing(6)
            .into()
    }
}

/// `with_header: false` drops the file-name/Open/updated row, for the Plans
/// tab, whose own header already carries them (`Plans.dc.html`).
pub fn plan_pane<'a>(app: &'a App, orchestrator_id: &str, with_header: bool) -> Element<'a, Message> {
    let doc = app.plan_docs.get(orchestrator_id);
    let registration = app.engine.store.get_orchestrator_plan(orchestrator_id).ok().flatten();
    // See session_detail.rs: Neovim has nowhere to run from the desktop app.
    let editor_is_terminal = ninox_ui_shared::editor::is_terminal(app.config.editor);

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
                    .color(tokens::DIM)
                    .wrapping(iced::widget::text::Wrapping::None)
                    .into(),
                Space::new(10, 0).into(),
                {
                    let color = if editor_is_terminal { tokens::DIM2 } else { tokens::TEAL };
                    let open_btn = button(text("Open").size(12).color(color))
                        .on_press_maybe((!editor_is_terminal).then(|| Message::OpenInEditor(reg.file_path.clone())))
                        .style(|_theme, _status| button::Style {
                            background: None,
                            border: Border::default(),
                            ..Default::default()
                        })
                        .padding(0);
                    if editor_is_terminal {
                        tooltip(
                            open_btn,
                            container(
                                text("Neovim is a terminal editor — open this file from the nx TUI's e key instead")
                                    .size(11)
                                    .font(crate::style::SANS)
                                    .color(tokens::DIM),
                            )
                            .width(Length::Fixed(220.0))
                            .padding([10, 12])
                            .style(|_theme| container::Style {
                                background: Some(Background::Color(tokens::PANEL2)),
                                border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
                                ..Default::default()
                            }),
                            tooltip::Position::Top,
                        )
                        .gap(6)
                        .into()
                    } else {
                        open_btn.into()
                    }
                },
                Space::new(Length::Fill, 0).into(),
                text(updated_label(ninox_ui_shared::orchestrator_plans::updated_ms(&reg.file_path, reg.updated_at)))
                    .size(11)
                    .font(crate::style::MONO)
                    .color(tokens::DIM2)
                    .wrapping(iced::widget::text::Wrapping::None)
                    .into(),
            ];
            if let Some(error) = error {
                items.insert(
                    1,
                    text(format!(" — unreadable: {error}"))
                        .size(11)
                        .color(tokens::RED)
                        .into(),
                );
            }
            row(items).align_y(iced::Alignment::Center).into()
        }
    };

    let body: Element<Message> = match (&registration, doc) {
        (None, _) => container(
            column![
                text("No plan doc registered").size(14).font(crate::style::SANS_ITALIC).color(tokens::DIM2),
                Space::new(0, 6),
                text("The orchestrator can run `ninox plan register <file>`.")
                    .size(12)
                    .font(crate::style::MONO)
                    .color(tokens::DIM2),
            ]
            .align_x(iced::Alignment::Center),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .into(),
        (Some(_), Some(doc)) if doc.error.is_some() => container(
            column![
                text("Registered plan doc is missing or unreadable").size(13).color(tokens::DIM2),
                text(doc.error.clone().unwrap_or_default()).size(11).font(crate::style::MONO).color(tokens::RED),
            ]
            .spacing(6)
            .align_x(iced::Alignment::Center),
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
                .map(|(i, block)| block_widget(orchestrator_id, i, block, editor_is_terminal))
                .collect();
            scrollable(container(column(blocks).spacing(10).max_width(860)).width(Length::Fill).padding(if with_header { [16, 16] } else { [22, 28] }))
                .width(Length::Fill)
                .height(Length::Fill)
                .into()
        }
        (Some(_), None) => container(text("Loading plan…").size(13).color(tokens::DIM2))
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into(),
    };

    let header: Element<Message> = container(header)
        .padding([10, 16])
        .width(Length::Fill)
        .style(|_theme| iced::widget::container::Style {
            background: Some(Background::Color(tokens::PANEL)),
            border: Border { color: tokens::BORDER, width: 1.0, radius: 0.0.into() },
            ..Default::default()
        })
        .into();
    let mut pane = column![].width(Length::Fill).height(Length::Fill);
    if with_header {
        pane = pane.push(header);
    }
    container(
        pane.push(body),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .style(|_theme| iced::widget::container::Style {
        background: Some(Background::Color(tokens::BG)),
        ..Default::default()
    })
    .into()
}

#[cfg(test)]
mod tests {
    use super::{link_chips, updated_label};
    use iced::advanced::layout::{Limits, Node};
    use iced::advanced::widget::Tree;
    use iced::{Font, Pixels, Size};

    fn make_links(labels: &[&str]) -> Vec<(String, String)> {
        labels.iter().map(|l| (l.to_string(), format!("https://example.com/{l}"))).collect()
    }

    fn lay_out(links: &[(String, String)], width: f32) -> Node {
        crate::style::load_bundled_fonts();
        let renderer = iced::Renderer::Secondary(iced_tiny_skia::Renderer::new(Font::default(), Pixels(16.0)));
        let element = link_chips(links, false);
        let mut tree = Tree::new(element.as_widget());
        element.as_widget().layout(&mut tree, &renderer, &Limits::new(Size::ZERO, Size::new(width, f32::INFINITY)))
    }

    #[test]
    fn link_chips_wrap_whole_chips_instead_of_squeezing_later_ones() {
        let labels = ["gossip_cert_issuer.rs", "render_corrosion_config.rs", "gossip_cert_rotation.rs"];
        let links = make_links(&labels);
        let natural: Vec<Node> = links.iter().map(|l| lay_out(std::slice::from_ref(l), f32::INFINITY)).collect();

        let node = lay_out(&links, 300.0);
        let children = node.children();
        assert_eq!(children.len(), labels.len());
        for ((label, natural), child) in labels.iter().zip(&natural).zip(children) {
            assert!(
                (child.size().width - natural.size().width).abs() < 0.5,
                "chip {label:?} shrunk: natural={}, actual={}",
                natural.size().width,
                child.size().width,
            );
            assert!((child.size().height - natural.size().height).abs() < 0.5, "chip {label:?} wrapped its label");
        }
        let rows: std::collections::BTreeSet<i32> = children.iter().map(|c| (c.bounds().y * 10.0).round() as i32).collect();
        assert!(rows.len() > 1, "expected chips on multiple lines");
    }

    #[test]
    fn an_overlong_chip_wraps_within_the_panel_not_per_glyph() {
        let long = "crates/ninox-core/src/very/deeply/nested/module_with_an_extremely_long_name/another_remarkably_long_file_name.rs";
        let links = make_links(&[long]);
        let line = lay_out(&make_links(&["a.rs"]), 300.0).size().height;

        let node = lay_out(&links, 300.0);
        assert!(node.size().width <= 300.5, "chip overflows the panel: {}", node.size().width);
        assert!(node.size().height <= line * 6.0, "label wraps far too often: {} vs line {line}", node.size().height);
    }

    #[test]
    fn a_zero_or_missing_timestamp_is_not_rendered_as_a_time() {
        assert_eq!(updated_label(Some(0)), "updated —");
        assert_eq!(updated_label(None), "updated —");
        assert!(updated_label(Some(1_793_813_000_000)).starts_with("updated Nov"));
    }
}
