use iced::{
    widget::{button, column, container, row, scrollable, text, Space},
    Alignment, Background, Border, Color, Element, Length,
};

use crate::app::{App, Message};
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
            border: Border { color: Color::TRANSPARENT, width: 0.0, radius: 4.0.into() },
            ..Default::default()
        })
        .into()
}

struct Column {
    label: &'static str,
    status: SessionStatus,
}

const COLUMNS: &[Column] = &[
    Column { label: "Working",   status: SessionStatus::Working },
    Column { label: "PR Open",   status: SessionStatus::PrOpen },
    Column { label: "CI Failed", status: SessionStatus::CiFailed },
    Column { label: "Review",    status: SessionStatus::ReviewPending },
    Column { label: "Mergeable", status: SessionStatus::Mergeable },
    Column { label: "Done",      status: SessionStatus::Done },
    Column { label: "Terminated", status: SessionStatus::Terminated },
];

pub fn board_sessions<'a>(
    app: &'a App,
    status: &SessionStatus,
    scope: Option<&str>,
) -> Vec<&'a Session> {
    let mut sessions: Vec<&Session> = app.sessions
        .values()
        .filter(|s| {
            &s.status == status
                && scope.map_or(true, |oid| {
                    s.orchestrator_id.as_deref() == Some(oid)
                })
        })
        .collect();
    sessions.sort_by(|a, b| a.name.cmp(&b.name));
    sessions
}

fn session_card<'a>(app: &'a App, session: &'a Session) -> Element<'a, Message> {
    let s = &app.scheme;
    let color = s.status_color(&session.status);
    let session_id = session.id.clone();
    let cost = format!("${:.2}", session.cost_usd);

    button(
        column![
            row![
                status_dot(color),
                Space::new(6, 0),
                text(&session.name).size(12).color(s.text_primary),
            ]
            .align_y(Alignment::Center),
            text(repo_short(&session.repo)).size(11).color(s.text_secondary),
            text(cost).size(11).color(s.text_muted),
        ]
        .spacing(3)
        .padding(8),
    )
    .on_press(Message::NavigateSession(session_id))
    .style(move |_theme, _status| button::Style {
        background: Some(Background::Color(s.bg_elevated)),
        text_color: s.text_primary,
        border: Border { color: s.border, width: 1.0, radius: 6.0.into() },
        ..Default::default()
    })
    .width(Length::Fixed(180.0))
    .into()
}

fn kanban_column<'a>(app: &'a App, label: &'static str, cards: Vec<Element<'a, Message>>) -> Element<'a, Message> {
    let s = &app.scheme;
    let count = cards.len();
    let header = container(
        row![
            text(label).size(12).color(s.text_muted),
            Space::new(Length::Fill, 0),
            text(count.to_string()).size(11).color(s.text_muted),
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
        .style(move |_theme| container::Style {
            background: Some(Background::Color(s.bg_surface)),
            border: Border { color: s.border, width: 1.0, radius: 8.0.into() },
            ..Default::default()
        })
        .into()
}

pub fn fleet_board<'a>(app: &'a App, scope: Option<&'a OrchestratorId>) -> Element<'a, Message> {
    let s = &app.scheme;

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
            text(scope_label).size(16).color(s.text_primary),
            Space::new(8, 0),
            text(format!("({} workers)", sessions.len())).size(13).color(s.text_muted),
        ]
        .align_y(Alignment::Center),
    )
    .padding([14, 20])
    .width(Length::Fill);

    let kanban_cols: Vec<Element<Message>> = COLUMNS
        .iter()
        .map(|col| {
            let col_sessions = board_sessions(app, &col.status, scope.map(|s| s.as_str()));
            let cards: Vec<Element<Message>> = col_sessions
                .iter()
                .map(|s| session_card(app, s))
                .collect();
            kanban_column(app, col.label, cards)
        })
        .collect();

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
