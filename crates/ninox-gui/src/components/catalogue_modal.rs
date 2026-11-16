//! Add-a-catalogue modal — opened from the `+` at the right edge of the
//! brain view's volume plate. v1 flat dark tokens, mirroring
//! `spawn_modal`'s frame, fields and footer (`Spawn.dc.html`); no artifact
//! draws this dialog on its own.

use iced::{
    widget::{button, column, container, row, text, text_input, Space},
    Alignment, Background, Border, Color, Element, Length, Padding,
};

use super::{
    shell::tokens,
    spawn_modal::{field_label, field_style, CARD_SELECTED_BG, CARD_SELECTED_BORDER},
};
use crate::{
    app::{App, Message},
    style::{MONO, SANS},
};

// ---------------------------------------------------------------------------
// Form state
// ---------------------------------------------------------------------------

/// Form state for filing a new brain catalogue (mirrors
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

pub fn catalogue_modal<'a>(_state: &'a App, form: &'a CatalogueForm) -> Element<'a, Message> {
    let can_submit = !form.name.trim().is_empty() && !form.path.trim().is_empty();

    let header = container(text("File a new catalogue").size(16).font(SANS).color(tokens::TEXT))
        .padding(Padding { top: 16.0, right: 20.0, bottom: 16.0, left: 20.0 })
        .width(Length::Fill);

    let name_field = column![
        field_label("Name"),
        Space::new(0, 6),
        text_input("e.g. research-notes", &form.name)
            .on_input(Message::CatalogueFormName)
            .on_submit_maybe(can_submit.then_some(Message::CatalogueFormConfirm))
            .size(13)
            .font(SANS)
            .padding(Padding { top: 9.0, right: 11.0, bottom: 9.0, left: 11.0 })
            .style(|_t, status| field_style(status)),
    ]
    .spacing(0);

    // `~` is expanded at confirm time (`ninox_ui_shared::spawn_util::expand_tilde`),
    // the directory created and the brain index initialized if missing.
    let path_field = column![
        field_label("Path"),
        Space::new(0, 6),
        text_input("~/brains/research", &form.path)
            .on_input(Message::CatalogueFormPath)
            .on_submit_maybe(can_submit.then_some(Message::CatalogueFormConfirm))
            .font(MONO)
            .size(13)
            .padding(Padding { top: 9.0, right: 11.0, bottom: 9.0, left: 11.0 })
            .style(|_t, status| field_style(status)),
    ]
    .spacing(0);

    let cancel_button = button(text("Cancel").size(12).font(SANS).color(tokens::DIM))
        .on_press(Message::CatalogueFormCancel)
        .padding(Padding { top: 8.0, right: 16.0, bottom: 8.0, left: 16.0 })
        .style(|_t, status| {
            let hovered = matches!(status, button::Status::Hovered);
            button::Style {
                background: Some(Background::Color(if hovered { tokens::PANEL2 } else { Color::TRANSPARENT })),
                text_color: tokens::DIM,
                border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
                ..Default::default()
            }
        });

    let spawn_teal_text = Color { r: 0xbf as f32 / 255.0, g: 0xf3 as f32 / 255.0, b: 0xec as f32 / 255.0, a: 1.0 };
    let file_button = button(text("File").size(12).font(SANS))
        .on_press_maybe(can_submit.then_some(Message::CatalogueFormConfirm))
        .padding(Padding { top: 8.0, right: 16.0, bottom: 8.0, left: 16.0 })
        .style(move |_t, status| {
            let hovered = can_submit && matches!(status, button::Status::Hovered);
            button::Style {
                background: Some(Background::Color(if can_submit { CARD_SELECTED_BG } else { tokens::PANEL2 })),
                text_color: if can_submit { spawn_teal_text } else { tokens::DIM2 },
                border: Border {
                    color: if can_submit { CARD_SELECTED_BORDER } else { tokens::BORDER },
                    width: if hovered { 1.5 } else { 1.0 },
                    radius: 6.0.into(),
                },
                ..Default::default()
            }
        });

    let footer = container(
        row![Space::new(Length::Fill, 0), cancel_button, Space::new(8, 0), file_button]
            .align_y(Alignment::Center),
    )
    .padding(Padding { top: 14.0, right: 20.0, bottom: 14.0, left: 20.0 })
    .width(Length::Fill)
    .style(|_| container::Style {
        background: Some(Background::Color(tokens::PANEL2)),
        ..Default::default()
    });

    let error_line: Option<Element<Message>> = form
        .error
        .as_ref()
        .map(|msg| text(msg.clone()).size(11.5).font(SANS).color(tokens::RED).into());

    let mut body = column![name_field, Space::new(0, 18), path_field]
        .padding(Padding { top: 18.0, right: 20.0, bottom: 18.0, left: 20.0 })
        .spacing(0);
    if let Some(err) = error_line {
        body = body.push(Space::new(0, 14)).push(err);
    }

    let header_rule = container(Space::new(Length::Fill, 1)).style(|_| container::Style {
        background: Some(Background::Color(tokens::BORDER)),
        ..Default::default()
    });

    let modal = container(column![header, header_rule, body, footer])
        .width(Length::Fixed(420.0))
        .style(|_| container::Style {
            background: Some(Background::Color(tokens::PANEL)),
            border: Border { color: Color { r: 0x2f as f32 / 255.0, g: 0x37 as f32 / 255.0, b: 0x50 as f32 / 255.0, a: 1.0 }, width: 1.0, radius: 12.0.into() },
            shadow: iced::Shadow { color: Color { a: 0.6, ..Color::BLACK }, offset: iced::Vector::new(0.0, 24.0), blur_radius: 60.0 },
            ..Default::default()
        });

    container(modal)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .style(|_| container::Style {
            background: Some(Background::Color(Color { r: 5.0 / 255.0, g: 7.0 / 255.0, b: 11.0 / 255.0, a: 0.66 })),
            ..Default::default()
        })
        .into()
}
