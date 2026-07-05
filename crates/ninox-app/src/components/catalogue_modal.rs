//! Add-a-catalogue modal — a "journal entry" over a dimmed backdrop, opened
//! from the `+` at the right edge of the brain view's volume plate.
//! Spec: docs/design-concepts/field-notes-design.md §"Adding a catalogue".

use iced::{
    widget::{button, column, container, row, text, text_input, Space},
    Alignment, Background, Border, Color, Element, Length,
};

use crate::{
    app::{App, Message},
    style::{self, hard_shadow, hline, micro_label, shadow_alpha, MONO, SANS_BOLD, SERIF, SERIF_ITALIC},
};

// ---------------------------------------------------------------------------
// Form state
// ---------------------------------------------------------------------------

/// Journal-entry form state for filing a new brain catalogue (mirrors
/// `spawn_modal::SpawnForm`'s shape).
#[derive(Debug, Clone, Default)]
pub struct CatalogueForm {
    pub name: String,
    pub path: String,
    /// Human-readable refusal reason from the last confirm attempt, if any.
    /// Cleared whenever the user edits any field. Rendered above the footer.
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

pub fn catalogue_modal<'a>(state: &'a App, form: &'a CatalogueForm) -> Element<'a, Message> {
    let s = &state.scheme;
    let can_submit = !form.name.trim().is_empty() && !form.path.trim().is_empty();

    // ── Header: journal-entry title strip — mirrors spawn_modal's header
    // structure exactly (paper_2 container, same padding, serif + serif
    // italic title pair) minus the entry counter, which has no analogue
    // here. ──────────────────────────────────────────────────────────────
    let header = container(
        row![
            text("File a new ").size(23).font(SERIF).color(s.ink),
            text("catalogue").size(23).font(SERIF_ITALIC).color(s.ink),
        ]
        .align_y(Alignment::Center),
    )
    .padding([18, 22])
    .width(Length::Fill)
    .style(move |_theme| container::Style {
        background: Some(Background::Color(s.paper_2)),
        ..Default::default()
    });

    // ── Name: serif underlined input (shared helper — also used by
    // spawn_modal's session-name field). ────────────────────────────────────
    let name_field = column![
        micro_label("Name", s.ink_2),
        Space::new(0, 6),
        style::serif_underlined_input(s, "e.g. research-notes", &form.name)
            .on_input(Message::CatalogueFormName)
            .on_submit_maybe(can_submit.then_some(Message::CatalogueFormConfirm)),
        hline(s.rule_dark, 1.5),
    ]
    .spacing(4);

    // ── Path: mono underlined input; `~` is expanded at confirm time
    // (crate::spawn_util::expand_tilde), the directory created and the
    // brain index initialized if missing. ───────────────────────────────────
    let path_field = column![
        micro_label("Path", s.ink_2),
        Space::new(0, 6),
        text_input("~/brains/research", &form.path)
            .on_input(Message::CatalogueFormPath)
            .on_submit_maybe(can_submit.then_some(Message::CatalogueFormConfirm))
            .font(MONO)
            .size(13)
            .padding([6, 2])
            .style(style::underlined_input_style(s)),
        hline(s.rule_dark, 1.5),
    ]
    .spacing(4);

    // ── Footer: ghost Cancel + accent primary File ⬡ (mirrors spawn_modal's
    // Cancel/Spawn button pair) ───────────────────────────────────────────
    let cancel_button = button(text("Cancel").size(11).font(SANS_BOLD).color(s.ink_2))
        .on_press(Message::CatalogueFormCancel)
        .padding([9, 18])
        .style(move |_theme, status| button::Style {
            background: None,
            text_color: s.ink_2,
            border: Border {
                color: if status == button::Status::Hovered { s.ink } else { s.rule_dark },
                width: 1.5,
                radius: 2.0.into(),
            },
            ..Default::default()
        });

    let file_label_color = if can_submit { s.card } else { s.faint };
    let file_button = button(
        row![
            text("FILE ").size(12).font(SANS_BOLD).color(file_label_color),
            text("⬡").size(12).font(style::GLYPH).color(file_label_color),
        ]
        .align_y(Alignment::Center),
    )
    .on_press_maybe(can_submit.then_some(Message::CatalogueFormConfirm))
    .padding([9, 20])
    .style(move |_theme, status| {
        let hovered = can_submit && status == button::Status::Hovered;
        let offset = if hovered { 4.0 } else { 3.0 };
        let (card_a, _, _) = shadow_alpha(s);
        button::Style {
            background: Some(Background::Color(if can_submit { s.accent } else { s.card })),
            text_color: if can_submit { s.card } else { s.faint },
            border: Border { color: s.rule_dark, width: 1.0, radius: 2.0.into() },
            shadow: hard_shadow(s, offset, offset, card_a),
        }
    });

    let footer = row![Space::new(Length::Fill, 0), cancel_button, file_button]
        .spacing(12)
        .align_y(Alignment::Center);

    // ── Guard-refusal feedback (mirrors spawn_modal's error line) ───────────
    let error_line: Option<Element<Message>> = form.error.as_ref().map(|msg| {
        row![
            text("⚑").size(11).font(style::GLYPH).color(s.accent),
            Space::new(6, 0),
            text(msg.clone()).size(11).font(SANS_BOLD).color(s.accent),
        ]
        .align_y(Alignment::Center)
        .into()
    });

    let mut body = column![name_field, Space::new(0, 18), path_field]
        .padding([20, 24])
        .spacing(0);
    if let Some(err) = error_line {
        body = body.push(Space::new(0, 14)).push(err);
    }
    body = body.push(Space::new(0, 22)).push(footer);

    let modal = container(column![header, hline(s.ink, 2.0), body])
        .width(Length::Fixed(420.0))
        .style(move |_theme| {
            let mut frame = style::heavy_frame(s);
            let (_, _, modal_a) = shadow_alpha(s);
            frame.shadow = hard_shadow(s, 8.0, 10.0, modal_a);
            frame
        });

    let backdrop_alpha = if s.dark { 0.55 } else { 0.45 };
    container(modal)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .style(move |_theme| container::Style {
            background: Some(Background::Color(Color { a: backdrop_alpha, ..s.shadow })),
            ..Default::default()
        })
        .into()
}
