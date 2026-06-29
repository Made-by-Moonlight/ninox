use iced::{
    widget::{button, column, container, row, scrollable, text, Space},
    Alignment, Background, Border, Color, Element, Length,
};

use crate::{
    app::{App, Message},
    theme::{
        BG_ELEVATED, BG_SURFACE, BORDER, TEXT_MUTED, TEXT_PRIMARY, TEXT_SECONDARY,
    },
};
use athene_core::types::{OrchestratorId, Session, SessionStatus};

fn repo_short(repo: &str) -> &str {
    repo.rsplit('/').next().unwrap_or(repo)
}

fn status_dot(color: Color) -> Element<'static, Message> {
    container(Space::new(0, 0))
        .width(Length::Fixed(8.0))
        .height(Length::Fixed(8.0))
        .style(move |_theme| container::Style {
            background: Some(Background::Color(color)),
            border: Border {
                color: Color::TRANSPARENT,
                width: 0.0,
                radius: 4.0.into(),
            },
            ..Default::default()
        })
        .into()
}

struct Column {
    label: &'static str,
    status: SessionStatus,
}

const COLUMNS: &[Column] = &[
    Column { label: "Working",    status: SessionStatus::Working },
    Column { label: "PR Open",    status: SessionStatus::PrOpen },
    Column { label: "CI Failed",  status: SessionStatus::CiFailed },
    Column { label: "Review",     status: SessionStatus::ReviewPending },
    Column { label: "Mergeable",  status: SessionStatus::Mergeable },
    Column { label: "Done",       status: SessionStatus::Done },
];

fn session_card<'a>(session: &'a Session) -> Element<'a, Message> {
    let color = crate::theme::status_color(&session.status);
    let session_id = session.id.clone();
    let cost = format!("${:.2}", session.cost_usd);

    button(
        column![
            row![
                status_dot(color),
                Space::new(6, 0),
                text(&session.name).size(12).color(TEXT_PRIMARY),
            ]
            .align_y(Alignment::Center),
            text(repo_short(&session.repo))
                .size(11)
                .color(TEXT_SECONDARY),
            text(cost).size(11).color(TEXT_MUTED),
        ]
        .spacing(3)
        .padding(8),
    )
    .on_press(Message::NavigateSession(session_id))
    .style(|_theme, _status| button::Style {
        background: Some(Background::Color(BG_ELEVATED)),
        text_color: TEXT_PRIMARY,
        border: Border {
            color: BORDER,
            width: 1.0,
            radius: 6.0.into(),
        },
        ..Default::default()
    })
    .width(Length::Fixed(180.0))
    .into()
}

fn kanban_column<'a>(label: &'static str, cards: Vec<Element<'a, Message>>) -> Element<'a, Message> {
    let count = cards.len();
    let header = container(
        row![
            text(label).size(12).color(TEXT_MUTED),
            Space::new(Length::Fill, 0),
            text(count.to_string()).size(11).color(TEXT_MUTED),
        ]
        .align_y(Alignment::Center),
    )
    .padding([8, 10])
    .width(Length::Fixed(200.0));

    let body = scrollable(
        column(cards).spacing(6).padding(10u16),
    )
    .height(Length::Fill);

    container(column![header, body])
        .width(Length::Fixed(200.0))
        .height(Length::Fill)
        .style(|_theme| container::Style {
            background: Some(Background::Color(BG_SURFACE)),
            border: Border {
                color: BORDER,
                width: 1.0,
                radius: 8.0.into(),
            },
            ..Default::default()
        })
        .into()
}

pub fn fleet_board<'a>(app: &'a App, scope: Option<&'a OrchestratorId>) -> Element<'a, Message> {
    // Filter sessions by scope
    let sessions: Vec<&Session> = app
        .sessions
        .values()
        .filter(|s| match scope {
            Some(orch_id) => s.orchestrator_id.as_ref() == Some(orch_id),
            None => true,
        })
        .collect();

    let scope_label = scope
        .and_then(|id| app.orchestrators.iter().find(|o| &o.id == id))
        .map(|o| o.name.as_str())
        .unwrap_or("All workers");

    let header = container(
        row![
            text(scope_label).size(16).color(TEXT_PRIMARY),
            Space::new(8, 0),
            text(format!("({} workers)", sessions.len()))
                .size(13)
                .color(TEXT_MUTED),
        ]
        .align_y(Alignment::Center),
    )
    .padding([14, 20])
    .width(Length::Fill);

    // Build kanban columns
    let kanban_cols: Vec<Element<Message>> = COLUMNS
        .iter()
        .map(|col| {
            let cards: Vec<Element<Message>> = sessions
                .iter()
                .filter(|s| s.status == col.status)
                .map(|s| session_card(s))
                .collect();
            kanban_column(col.label, cards)
        })
        .collect();

    // Use with_direction directly: calling scrollable() defaults to vertical and
    // validates immediately, panicking because the row's height is Fill (from
    // kanban columns with height(Fill)).
    let board = scrollable::Scrollable::with_direction(
        row(kanban_cols).spacing(12).padding(20u16),
        scrollable::Direction::Horizontal(scrollable::Scrollbar::default()),
    )
    .width(Length::Fill);

    column![header, board]
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}
