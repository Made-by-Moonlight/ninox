use iced::{
    widget::{button, container, text, text_input, Space},
    Alignment, Background, Border, Element, Length,
};

use crate::app::{App, Message};

pub fn filter_bar<'a>(app: &'a App) -> Element<'a, Message> {
    let s = &app.scheme;
    let has_filter = !app.fleet_filter.query.is_empty();

    let input = text_input("Filter sessions...", &app.fleet_filter.query)
        .on_input(Message::FleetFilterQuery)
        .padding([6, 10])
        .size(12)
        .width(Length::Fixed(220.0));

    let clear_btn = if has_filter {
        Some(
            button(text("✕").size(11).color(s.text_muted))
                .on_press(Message::ClearFleetFilter)
                .style(|_t, _s| button::Style {
                    background: None,
                    border: Border::default(),
                    ..Default::default()
                })
                .padding([4, 8])
        )
    } else {
        None
    };

    let total = app.sessions.len();
    let shown: usize = {
        let q = app.fleet_filter.query.to_lowercase();
        if q.is_empty() { total } else {
            app.sessions.values().filter(|s| {
                s.name.to_lowercase().contains(&q) || s.repo.to_lowercase().contains(&q)
            }).count()
        }
    };

    let count_label = if has_filter {
        format!("{shown}/{total}")
    } else {
        format!("{total} session{}", if total == 1 { "" } else { "s" })
    };

    let mut row_items: Vec<Element<Message>> = vec![
        input.into(),
    ];
    if let Some(btn) = clear_btn {
        row_items.push(btn.into());
    }
    row_items.push(Space::new(Length::Fill, 0).into());
    row_items.push(text(count_label).size(11).color(s.text_muted).into());

    container(
        iced::widget::row(row_items).align_y(Alignment::Center).spacing(4)
    )
    .padding([8, 16])
    .width(Length::Fill)
    .style(move |_| container::Style {
        background: Some(Background::Color(s.bg_base)),
        border: Border { color: s.border, width: 0.0, radius: 0.0.into() },
        ..Default::default()
    })
    .into()
}
