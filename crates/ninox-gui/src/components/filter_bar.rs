use iced::{
    widget::{button, column, row, text, text_input, Space},
    Alignment, Border, Element, Length,
};

use crate::app::{App, Message};
use super::shell::tokens;
use crate::style::hline;

/// Underlined "⌕ filter the fleet…" field for the folio row.
pub fn filter_bar(app: &App) -> Element<'_, Message> {
    let has_filter = !app.fleet_filter.query.is_empty();
    let input = text_input("filter the fleet…", &app.fleet_filter.query)
        .on_input(Message::FleetFilterQuery)
        .size(12)
        .padding([4, 2])
        .style(crate::style::v1_underlined_input_style());

    let mut field_row = row![
        text("⌕").size(13).font(crate::style::GLYPH).color(tokens::DIM2),
        Space::new(6, 0),
        input
    ]
        .align_y(Alignment::Center);
    if has_filter {
        field_row = field_row.push(
            button(text("✕").size(10).color(tokens::DIM2).shaping(iced::widget::text::Shaping::Advanced))
                .on_press(Message::ClearFleetFilter)
                .padding(0)
                .style(|_t, _st| button::Style {
                    background: None,
                    border: Border::default(),
                    ..Default::default()
                }),
        );
    }

    column![field_row, hline(tokens::BORDER, 1.0)]
        .width(Length::Fixed(230.0))
        .into()
}
