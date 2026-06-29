use iced::{
    widget::{button, column, container, row, scrollable, text, Space},
    Alignment, Background, Border, Color, Element, Length,
};

use crate::{
    app::{App, Message, View},
    theme::{
        ACCENT_AMBER, BG_SIDEBAR, BG_SURFACE, BORDER, TEXT_MUTED, TEXT_PRIMARY, TEXT_SECONDARY,
    },
};
use athene_core::types::SessionStatus;

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

fn status_color_for(status: &SessionStatus) -> Color {
    crate::theme::status_color(status)
}

pub fn sidebar(app: &App) -> Element<'_, Message> {
    // Header
    let header = container(
        row![
            text("⬡ Athene")
                .size(14)
                .color(TEXT_PRIMARY),
            Space::new(Length::Fill, 0),
            button(text("+ Spawn").size(12).color(ACCENT_AMBER))
                .on_press(Message::SpawnSession)
                .style(|_theme, _status| button::Style {
                    background: None,
                    text_color: ACCENT_AMBER,
                    border: Border {
                        color: ACCENT_AMBER,
                        width: 1.0,
                        radius: 4.0.into(),
                    },
                    ..Default::default()
                })
                .padding([2, 8]),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .padding([12, 12])
    .width(Length::Fill)
    .style(|_theme| container::Style {
        background: Some(Background::Color(BG_SIDEBAR)),
        border: Border {
            color: BORDER,
            width: 0.0,
            radius: 0.0.into(),
        },
        ..Default::default()
    });

    // Build session list
    let mut items: Vec<Element<Message>> = Vec::new();

    // Orchestrator sections
    for orch in &app.orchestrators {
        let is_expanded = app.sidebar.selected_orchestrator.as_deref() == Some(orch.id.as_str());
        let toggle_icon = if is_expanded { "▼" } else { "▶" };
        let orch_id = orch.id.clone();

        let is_viewing = matches!(&app.view, View::SessionDetail { session_id, .. } if session_id == &orch.id);

        let chevron = button(text(toggle_icon).size(10).color(TEXT_MUTED))
            .on_press(Message::SelectOrchestrator(if is_expanded {
                None
            } else {
                Some(orch_id.clone())
            }))
            .style(|_theme, _status| button::Style {
                background: None,
                border: Border::default(),
                ..Default::default()
            })
            .padding([6, 8]);

        let name_btn = button(text(&orch.name).size(13).color(TEXT_PRIMARY))
            .on_press(Message::NavigateSession(orch_id.clone()))
            .style(move |_theme, _status| button::Style {
                background: if is_viewing { Some(Background::Color(BG_SURFACE)) } else { None },
                text_color: TEXT_PRIMARY,
                border: Border::default(),
                ..Default::default()
            })
            .padding([6, 4])
            .width(Length::Fill);

        let orch_row: Element<Message> = row![chevron, name_btn]
            .align_y(Alignment::Center)
            .width(Length::Fill)
            .into();

        items.push(orch_row);

        if is_expanded {
            let workers: Vec<&athene_core::types::Session> = app
                .sessions
                .values()
                .filter(|s| s.orchestrator_id.as_deref() == Some(orch_id.as_str()))
                .collect();

            for session in workers {
                items.push(worker_row(app, session));
            }
        }
    }

    // Standalone workers (no orchestrator_id)
    let standalone: Vec<&athene_core::types::Session> = app
        .sessions
        .values()
        .filter(|s| s.orchestrator_id.is_none())
        .collect();

    if !standalone.is_empty() {
        if !app.orchestrators.is_empty() {
            // Divider label
            items.push(
                container(text("Workers").size(11).color(TEXT_MUTED))
                    .padding([8u16, 12])
                    .into(),
            );
        }
        for session in standalone {
            items.push(worker_row(app, session));
        }
    }

    let list = scrollable(
        column(items).spacing(1).width(Length::Fill),
    )
    .height(Length::Fill);

    container(column![header, list].spacing(0))
        .width(Length::Fixed(220.0))
        .height(Length::Fill)
        .style(|_theme| container::Style {
            background: Some(Background::Color(BG_SIDEBAR)),
            border: Border {
                color: BORDER,
                width: 0.0,
                radius: 0.0.into(),
            },
            ..Default::default()
        })
        .into()
}

fn worker_row<'a>(app: &'a App, session: &'a athene_core::types::Session) -> Element<'a, Message> {
    let is_selected = app.sidebar.selected_orchestrator.is_none()
        && matches!(&app.view, crate::app::View::SessionDetail { session_id: id, .. } if id == &session.id);

    let color = status_color_for(&session.status);
    let bg = if is_selected {
        Some(Background::Color(BG_SURFACE))
    } else {
        None
    };
    let session_id = session.id.clone();

    button(
        row![
            status_dot(color),
            Space::new(6, 0),
            column![
                text(&session.name).size(12).color(TEXT_PRIMARY),
                text(repo_short(&session.repo)).size(11).color(TEXT_SECONDARY),
            ]
            .spacing(1),
        ]
        .spacing(0)
        .align_y(Alignment::Center),
    )
    .on_press(Message::NavigateSession(session_id))
    .style(move |_theme, _status| button::Style {
        background: bg,
        text_color: TEXT_PRIMARY,
        border: Border::default(),
        ..Default::default()
    })
    .padding([5, 12])
    .width(Length::Fill)
    .into()
}
