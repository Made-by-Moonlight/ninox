//! Restart-all confirmation, opened from Settings › Fleet
//! (`Message::RequestRestartAll`). A single yes/no decision, in the same v1
//! dialog shape as `restore_modal`.

use iced::{
    widget::{column, container, row, text, Space},
    Alignment, Background, Border, Color, Element, Length,
};

use super::restore_modal::modal_button;
use super::shell::tokens;
use crate::{
    app::{live_session_count, App, Message},
    style::SANS,
};

pub fn restart_all_modal(state: &App) -> Element<'_, Message> {
    let n = live_session_count(&state.sessions);
    let plural = if n == 1 { "" } else { "s" };

    let header = container(text("Restart all agents").size(15).font(SANS).color(tokens::TEXT))
        .padding([16, 20])
        .width(Length::Fill)
        .style(|_| container::Style { background: Some(Background::Color(tokens::PANEL2)), ..Default::default() });

    let body = column![
        text(format!("Restart all {n} live agent{plural}?")).size(13).font(SANS).color(tokens::TEXT),
        text(
            "Interrupts every running session at once so each picks up tooling-stack updates \
             (a newer ninox/harness build, MCP config, or reseeded skills). Conversations resume \
             where the harness supports it; sessions that can't resume restart fresh.",
        )
        .size(11.5)
        .font(SANS)
        .color(tokens::DIM),
    ]
    .spacing(8);

    let footer = row![
        Space::new(Length::Fill, 0),
        modal_button("Cancel".into(), tokens::TEXT, false, Some(Message::CancelRestartAll)),
        modal_button("Restart all".into(), tokens::TEAL, true, Some(Message::ConfirmRestartAll)),
    ]
    .spacing(10)
    .align_y(Alignment::Center);

    let card = container(column![header, container(body).padding([16, 20]), container(footer).padding([12, 20])])
        .width(Length::Fixed(460.0))
        .style(|_| container::Style {
            background: Some(Background::Color(tokens::PANEL)),
            border: Border { color: tokens::BORDER, width: 1.0, radius: 8.0.into() },
            ..Default::default()
        });

    container(card)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .style(|_| container::Style { background: Some(Background::Color(Color { a: 0.6, ..Color::BLACK })), ..Default::default() })
        .into()
}
