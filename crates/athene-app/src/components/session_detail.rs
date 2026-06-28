use iced::{
    widget::{button, column, container, row, text, Space},
    Alignment, Background, Border, Element, Length,
};

use crate::{
    app::{App, Message},
    components::terminal::TerminalWidget,
    theme::{
        ACCENT_AMBER, BG_ELEVATED, BG_SURFACE, BORDER, TEXT_MUTED, TEXT_PRIMARY, TEXT_SECONDARY,
    },
};

fn repo_short(repo: &str) -> &str {
    repo.rsplit('/').next().unwrap_or(repo)
}

/// Panel selection — which view is active in session detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailPanel {
    Terminal,
    Info,
}

impl Default for DetailPanel {
    fn default() -> Self {
        DetailPanel::Terminal
    }
}

/// Session detail view — header + panel toggle + terminal canvas.
pub fn session_detail<'a>(
    app: &'a App,
    session_id: &str,
    panel: &DetailPanel,
) -> Element<'a, Message> {
    let Some(session) = app.sessions.get(session_id) else {
        return container(
            text("Session not found")
                .size(14)
                .color(TEXT_MUTED),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(20)
        .into();
    };

    let color = crate::theme::status_color(&session.status);
    let cost = format!("${:.4}", session.cost_usd);
    let pr_label = session
        .pr_number
        .map(|n| format!("PR #{n}"))
        .unwrap_or_default();

    // --- Header bar ---
    let status_dot = container(Space::new(0, 0))
        .width(Length::Fixed(8.0))
        .height(Length::Fixed(8.0))
        .style(move |_theme| container::Style {
            background: Some(Background::Color(color)),
            border: Border {
                color: iced::Color::TRANSPARENT,
                width: 0.0,
                radius: 4.0.into(),
            },
            ..Default::default()
        });

    let back_btn = button(
        text("← Fleet")
            .size(12)
            .color(TEXT_SECONDARY),
    )
    .on_press(Message::NavigateFleet { scope: None })
    .style(|_theme, _status| button::Style {
        background: None,
        text_color: TEXT_SECONDARY,
        ..Default::default()
    });

    let header = container(
        row![
            back_btn,
            Space::new(16, 0),
            text(&session.name).size(14).color(TEXT_PRIMARY),
            Space::new(8, 0),
            text("·").size(14).color(TEXT_MUTED),
            Space::new(8, 0),
            text(repo_short(&session.repo))
                .size(13)
                .color(ACCENT_AMBER),
            Space::new(Length::Fill, 0),
            status_dot,
            Space::new(6, 0),
            text(cost).size(12).color(TEXT_MUTED),
            if !pr_label.is_empty() {
                Element::from(
                    row![
                        Space::new(12, 0),
                        text(pr_label.clone()).size(12).color(TEXT_SECONDARY),
                    ]
                    .align_y(Alignment::Center),
                )
            } else {
                Space::new(0, 0).into()
            },
        ]
        .align_y(Alignment::Center),
    )
    .padding([10, 16])
    .width(Length::Fill)
    .style(|_theme| container::Style {
        background: Some(Background::Color(BG_SURFACE)),
        border: Border {
            color: BORDER,
            width: 1.0,
            radius: 0.0.into(),
        },
        ..Default::default()
    });

    // --- Terminal canvas or placeholder ---
    let terminal_pane: Element<Message> = if let Some(term_state) = app.terminals.get(session_id) {
        iced::widget::Canvas::new(TerminalWidget {
            state: term_state,
            font_size: 13.0,
            session_id: session_id.to_string(),
        })
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    } else {
        container(
            text("Terminal connecting…")
                .size(13)
                .color(TEXT_MUTED),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .style(|_theme| container::Style {
            background: Some(Background::Color(BG_ELEVATED)),
            ..Default::default()
        })
        .into()
    };

    // --- Info stub (for future Task 12) ---
    let info_pane: Element<Message> = container(
        text("Info panel — Task 12")
            .size(13)
            .color(TEXT_MUTED),
    )
    .width(Length::FillPortion(1))
    .height(Length::Fill)
    .center_x(Length::Fill)
    .center_y(Length::Fill)
    .style(|_theme| container::Style {
        background: Some(Background::Color(BG_SURFACE)),
        border: Border {
            color: BORDER,
            width: 1.0,
            radius: 0.0.into(),
        },
        ..Default::default()
    })
    .into();

    // Panel routing: Terminal = full terminal, Info = full info, default = split 2/3 + 1/3.
    let content: Element<Message> = match panel {
        DetailPanel::Terminal => container(terminal_pane)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_theme| container::Style {
                background: Some(Background::Color(iced::Color::from_rgb8(0x28, 0x28, 0x28))),
                ..Default::default()
            })
            .into(),
        DetailPanel::Info => info_pane,
    };

    column![header, content]
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}
