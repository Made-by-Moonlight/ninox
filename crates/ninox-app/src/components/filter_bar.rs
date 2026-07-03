use iced::{
    widget::{button, column, row, text, text_input, Space},
    Alignment, Background, Border, Element, Length,
};

use crate::app::{App, Message};
use crate::style::hline;

/// Underlined "⌕ filter the fleet…" field for the folio row.
pub fn filter_bar(app: &App) -> Element<'_, Message> {
    let s = &app.scheme;
    let has_filter = !app.fleet_filter.query.is_empty();
    let input = text_input("filter the fleet…", &app.fleet_filter.query)
        .on_input(Message::FleetFilterQuery)
        .size(12)
        .padding([4, 2])
        .style(move |_t, _st| text_input::Style {
            background: Background::Color(iced::Color::TRANSPARENT),
            border: Border::default(),
            icon: s.faint,
            placeholder: s.faint,
            value: s.ink,
            selection: iced::Color { a: 0.35, ..s.accent },
        });

    let mut field_row = row![text("⌕").size(13).color(s.faint), Space::new(6, 0), input]
        .align_y(Alignment::Center);
    if has_filter {
        field_row = field_row.push(
            button(text("✕").size(10).color(s.faint))
                .on_press(Message::ClearFleetFilter)
                .padding(0)
                .style(|_t, _st| button::Style {
                    background: None,
                    border: Border::default(),
                    ..Default::default()
                }),
        );
    }

    column![field_row, hline(s.ink, 1.5)]
        .width(Length::Fixed(230.0))
        .into()
}
