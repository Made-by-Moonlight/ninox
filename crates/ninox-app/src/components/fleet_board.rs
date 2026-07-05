use iced::{
    widget::{button, column, container, row, scrollable, text, Space},
    Alignment, Background, Border, Element, Length,
};

use crate::app::{App, Message};
use crate::components::filter_bar::filter_bar;
use ninox_core::types::{OrchestratorId, Session, SessionStatus};

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
];

#[cfg(test)]
pub fn filtered_sessions(app: &App) -> Vec<&Session> {
    let q = app.fleet_filter.query.to_lowercase();
    app.sessions.values().filter(|s| {
        q.is_empty()
            || s.name.to_lowercase().contains(&q)
            || s.repo.to_lowercase().contains(&q)
    }).collect()
}

pub fn board_sessions<'a>(
    app: &'a App,
    status: &SessionStatus,
    scope: Option<&str>,
) -> Vec<&'a Session> {
    let q = app.fleet_filter.query.to_lowercase();
    let orch_ids: std::collections::HashSet<&str> =
        app.orchestrators.iter().map(|o| o.id.as_str()).collect();
    let mut sessions: Vec<&Session> = app.sessions.values().filter(|s| {
        &s.status == status
            && !orch_ids.contains(s.id.as_str())
            && scope.is_none_or(|oid| s.orchestrator_id.as_deref() == Some(oid))
            && (q.is_empty()
                || s.name.to_lowercase().contains(&q)
                || s.repo.to_lowercase().contains(&q))
    }).collect();
    sessions.sort_by(|a, b| a.name.cmp(&b.name));
    sessions
}

pub fn attention_count(app: &App) -> usize {
    app.sessions.values().filter(|s| {
        matches!(s.status, SessionStatus::CiFailed | SessionStatus::ReviewPending)
    }).count()
}

/// "Morning observations" / … by local hour (spec §5 folio header).
pub fn folio_title(hour: u32) -> String {
    let period = match hour {
        5..=11  => "Morning",
        12..=16 => "Afternoon",
        17..=21 => "Evening",
        _       => "Night",
    };
    format!("{period} observations")
}

fn tab_chip<'a>(app: &'a App, label: &'a str, msg: Message, is_active: bool) -> Element<'a, Message> {
    let s = &app.scheme;
    let border = Border { color: s.ink, width: 1.5, radius: 2.0.into() };
    button(crate::style::micro_label(label, if is_active { s.card } else { s.ink }))
        .on_press(msg)
        .padding([4, 10])
        .style(crate::style::segment_style(s, is_active, s.ink, None, border, border))
        .into()
}

/// Sessions counted toward the "total" figure — every session that isn't
/// itself an orchestrator's own bookkeeping session.
fn total_session_count(app: &App) -> usize {
    let orch_ids: std::collections::HashSet<&str> =
        app.orchestrators.iter().map(|o| o.id.as_str()).collect();
    app.sessions.values().filter(|w| !orch_ids.contains(w.id.as_str())).count()
}

/// Folio header row: split-weight serif title, "VOL." mono date stamp, the
/// filter field, and a "shown/total" session count. Wraps onto two rows at
/// narrow widths via `folio::folio_scaffold` — see that module for why.
fn folio<'a>(app: &'a App, scope: Option<&'a OrchestratorId>) -> Element<'a, Message> {
    use chrono::{Datelike, Local, Timelike};
    let now = Local::now();
    let title = folio_title(now.hour());
    let month = crate::style::MONTHS[now.month0() as usize];
    let date_label = format!("VOL. I — {} {} {}", now.day(), month, now.year());

    let total = total_session_count(app);
    let shown = COLUMNS.iter()
        .map(|c| board_sessions(app, &c.status, scope.map(|x| x.as_str())).len())
        .sum::<usize>()
        + board_sessions(app, &SessionStatus::Terminated, scope.map(|x| x.as_str())).len();

    crate::components::folio::folio_scaffold(
        app,
        move || {
            let s = &app.scheme;
            // Split the title so the last word is italic ("Morning *observations*").
            let (head, tail) = title.rsplit_once(' ').unwrap_or(("", title.as_str()));
            row![
                text(format!("{head} ")).size(34).font(crate::style::SERIF).color(s.ink),
                text(tail.to_owned()).size(34).font(crate::style::SERIF_ITALIC).color(s.ink),
                Space::new(18, 0),
                text(date_label.clone())
                    .size(10.5)
                    .font(crate::style::MONO)
                    .color(s.faint)
                    .wrapping(iced::widget::text::Wrapping::None),
            ]
            .align_y(Alignment::End)
            .into()
        },
        move || {
            let s = &app.scheme;
            vec![
                filter_bar(app),
                text(format!("{shown}/{total} sessions"))
                    .size(10.5)
                    .font(crate::style::MONO)
                    .color(s.ink_2)
                    .wrapping(iced::widget::text::Wrapping::None)
                    .into(),
            ]
        },
    )
}

/// 1.5px vermilion-bordered banner shown while any session needs attention.
fn attention_banner(app: &App) -> Option<Element<'_, Message>> {
    if attention_count(app) == 0 { return None; }
    let s = &app.scheme;
    let ci = app.sessions.values().filter(|w| matches!(w.status, SessionStatus::CiFailed)).count();
    let review = app.sessions.values().filter(|w| matches!(w.status, SessionStatus::ReviewPending)).count();
    let mut parts = Vec::new();
    if ci > 0 { parts.push(format!("{ci} CI failure{}", if ci == 1 { "" } else { "s" })); }
    if review > 0 { parts.push(format!("{review} review{}", if review == 1 { "" } else { "s" })); }
    Some(
        container(
            row![
                text("⚑").size(13).font(crate::style::GLYPH).color(s.accent),
                Space::new(10, 0),
                text(format!("{} require attention.", parts.join(" and ")))
                    .size(12).font(crate::style::SANS_BOLD).color(s.accent),
            ]
            .align_y(Alignment::Center),
        )
        .padding([8, 14])
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(iced::Color { a: 0.06, ..s.accent })),
            border: Border { color: s.accent, width: 1.5, radius: 2.0.into() },
            ..Default::default()
        })
        .into(),
    )
}

fn session_card<'a>(app: &'a App, session: &'a Session) -> Element<'a, Message> {
    let s = &app.scheme;
    let st_color = s.status_color(&session.status);
    let word = crate::style::stamp_word(&session.status);
    let (card_a, _, _) = crate::style::shadow_alpha(s);
    let repo_line = if session.repo.is_empty() {
        session.id.clone()
    } else {
        session.repo.clone()
    };
    button(
        column![
            text(&session.name).size(16).font(crate::style::SERIF_MEDIUM).color(s.ink),
            Space::new(0, 2),
            text(repo_line).size(9.5).font(crate::style::MONO).color(s.faint),
            Space::new(0, 9),
            crate::style::dotted_rule(s.rule_dark),
            row![
                crate::style::stamp(word, st_color),
                Space::new(Length::Fill, 0),
                text(format!("${:.2}", session.cost_usd))
                    .size(11.5).font(crate::style::MONO_MEDIUM).color(s.ink),
            ]
            .align_y(Alignment::Center),
        ]
        .padding(iced::Padding { top: 12.0, right: 13.0, bottom: 10.0, left: 13.0 }),
    )
    .on_press(Message::NavigateSession(session.id.clone()))
    .width(Length::Fill)
    .style(move |_t, status| {
        let hovered = matches!(status, button::Status::Hovered);
        button::Style {
            background: Some(Background::Color(s.card)),
            text_color: s.ink,
            border: Border { color: s.rule_dark, width: 1.0, radius: 2.0.into() },
            shadow: crate::style::hard_shadow(
                s,
                if hovered { 4.0 } else { 2.0 },
                if hovered { 6.0 } else { 3.0 },
                card_a + if hovered { 0.02 } else { 0.0 },
            ),
        }
    })
    .into()
}

fn ledger_column<'a>(
    app: &'a App,
    label: &'static str,
    cards: Vec<Element<'a, Message>>,
    first: bool,
) -> Element<'a, Message> {
    let s = &app.scheme;
    let count = cards.len();
    let head = column![
        row![
            text(label).size(16.5).font(crate::style::SERIF_MEDIUM_ITALIC).color(s.ink),
            Space::new(Length::Fill, 0),
            text(format!("№ {count}")).size(10).font(crate::style::MONO).color(s.faint),
        ]
        .align_y(Alignment::End),
        Space::new(0, 8),
        crate::style::hline(s.ink, 2.0),
    ];
    let body = scrollable(column(cards).spacing(12).padding(iced::Padding {
        top: 12.0, right: 2.0, bottom: 4.0, left: 0.0,
    }))
    .height(Length::Fill);

    let inner = column![head, body].width(Length::Fixed(220.0));
    if first {
        container(inner).padding(iced::Padding { top: 0.0, right: 12.0, bottom: 0.0, left: 0.0 }).into()
    } else {
        row![
            crate::style::vline(s.rule, 1.0),
            container(inner).padding([0, 12]),
        ]
        .into()
    }
}

pub fn fleet_board<'a>(app: &'a App, scope: Option<&'a OrchestratorId>) -> Element<'a, Message> {
    let mut sections: Vec<Element<Message>> = Vec::new();
    sections.push(folio(app, scope));

    if let Some(banner) = attention_banner(app) {
        sections.push(
            container(banner)
                .padding(iced::Padding { top: 0.0, right: 28.0, bottom: 0.0, left: 28.0 })
                .width(Length::Fill)
                .into(),
        );
    }

    // Orchestrator scope chips — only render when there are orchestrators.
    if !app.orchestrators.is_empty() {
        let mut chips: Vec<Element<Message>> = Vec::new();
        chips.push(tab_chip(app, "All", Message::NavigateFleet { scope: None }, scope.is_none()));
        for orch in &app.orchestrators {
            let is_active = scope.map(|id| id == &orch.id).unwrap_or(false);
            chips.push(tab_chip(
                app,
                &orch.name,
                Message::NavigateFleet { scope: Some(orch.id.clone()) },
                is_active,
            ));
        }
        sections.push(
            container(row(chips).spacing(8))
                .padding(iced::Padding { top: 0.0, right: 28.0, bottom: 10.0, left: 28.0 })
                .width(Length::Fill)
                .into(),
        );
    }

    if total_session_count(app) == 0 {
        let s = &app.scheme;
        sections.push(
            container(
                text("No sessions in the field.")
                    .size(15)
                    .font(crate::style::SERIF_ITALIC)
                    .color(s.faint),
            )
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .width(Length::Fill)
            .height(Length::Fill)
            .into(),
        );
    } else {
        let ledger_cols: Vec<Element<Message>> = COLUMNS
            .iter()
            .enumerate()
            .map(|(i, col)| {
                let mut col_sessions = board_sessions(app, &col.status, scope.map(|s| s.as_str()));
                if col.status == SessionStatus::Done {
                    col_sessions.extend(board_sessions(app, &SessionStatus::Terminated, scope.map(|s| s.as_str())));
                }
                let cards: Vec<Element<Message>> = col_sessions
                    .iter()
                    .map(|s| session_card(app, s))
                    .collect();
                ledger_column(app, col.label, cards, i == 0)
            })
            .collect();

        let board = scrollable::Scrollable::with_direction(
            row(ledger_cols),
            scrollable::Direction::Horizontal(scrollable::Scrollbar::default()),
        )
        .width(Length::Fill)
        .height(Length::Fill);

        sections.push(
            container(board)
                .padding(iced::Padding { top: 16.0, right: 28.0, bottom: 16.0, left: 28.0 })
                .width(Length::Fill)
                .height(Length::Fill)
                .into(),
        );
    }

    column(sections)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folio_title_follows_time_of_day() {
        assert_eq!(folio_title(6),  "Morning observations");
        assert_eq!(folio_title(13), "Afternoon observations");
        assert_eq!(folio_title(19), "Evening observations");
        assert_eq!(folio_title(2),  "Night observations");
    }
}
