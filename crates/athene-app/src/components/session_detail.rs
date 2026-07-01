use iced::{
    widget::{button, column, container, row, text, Space},
    Alignment, Background, Border, Color, Element, Length,
};

use crate::{
    app::{App, DragTarget, Message},
    components::{info_panel::info_panel, inspector_panel::inspector_panel, terminal::TerminalWidget},
};

fn repo_short(repo: &str) -> &str {
    repo.rsplit('/').next().unwrap_or(repo)
}

fn panel_btn<'a>(app: &'a App, label: &'static str, target: DetailPanel, active: DetailPanel) -> Element<'a, Message> {
    let s = &app.scheme;
    let is_active = target == active;
    button(
        text(label).size(11).color(if is_active { Color::WHITE } else { s.text_secondary }),
    )
    .on_press(Message::SwitchDetailPanel(target))
    .padding([3, 8])
    .style(move |_theme, _status| button::Style {
        background: if is_active {
            Some(Background::Color(s.accent))
        } else {
            Some(Background::Color(s.bg_elevated))
        },
        border: Border { color: s.border, width: 1.0, radius: 3.0.into() },
        text_color: if is_active { Color::WHITE } else { s.text_secondary },
        ..Default::default()
    })
    .into()
}

/// Panel selection — which view is active in session detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailPanel {
    Terminal,
    Split,
    Info,
    Inspector,
}

impl Default for DetailPanel {
    fn default() -> Self {
        DetailPanel::Split
    }
}

/// Session detail view — header + panel toggle + terminal canvas.
pub fn session_detail<'a>(
    app: &'a App,
    session_id: &str,
    panel: &DetailPanel,
) -> Element<'a, Message> {
    let s = &app.scheme;

    let Some(session) = app.sessions.get(session_id) else {
        return container(
            text("Session not found").size(14).color(s.text_muted),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(20)
        .into();
    };

    let color = s.status_color(&session.status);
    let cost = format!("${:.4}", session.cost_usd);
    let pr_label = session.pr_number.map(|n| format!("PR #{n}")).unwrap_or_default();

    // ── Header ────────────────────────────────────────────────────────────────
    let status_dot = container(Space::new(0, 0))
        .width(Length::Fixed(8.0))
        .height(Length::Fixed(8.0))
        .style(move |_theme| container::Style {
            background: Some(Background::Color(color)),
            border: Border { color: Color::TRANSPARENT, width: 0.0, radius: 4.0.into() },
            ..Default::default()
        });

    let back_btn = button(text("← Fleet").size(12).color(s.text_secondary))
        .on_press(Message::NavigateFleet { scope: None })
        .style(|_theme, _status| button::Style {
            background: None,
            text_color: s.text_secondary,
            ..Default::default()
        });

    let is_orchestrator = app.orchestrators.iter().any(|o| o.id == session_id);
    let panel_toggles = if is_orchestrator {
        row![].align_y(Alignment::Center)
    } else {
        row![
            panel_btn(app, "Terminal", DetailPanel::Terminal, *panel),
            Space::new(4, 0),
            panel_btn(app, "Split", DetailPanel::Split, *panel),
            Space::new(4, 0),
            panel_btn(app, "Info", DetailPanel::Info, *panel),
            Space::new(4, 0),
            panel_btn(app, "Inspector", DetailPanel::Inspector, *panel),
        ]
        .align_y(Alignment::Center)
    };

    let kill_color = iced::Color::from_rgb8(0xcc, 0x24, 0x1d);
    let kill_btn: Element<Message> = if !is_orchestrator {
        let sid = session_id.to_string();
        button(text("Kill").size(11).color(kill_color))
            .on_press(Message::RemoveSession(sid))
            .style(move |_theme, _status| button::Style {
                background: None,
                border: Border { color: kill_color, width: 1.0, radius: 3.0.into() },
                text_color: kill_color,
                ..Default::default()
            })
            .padding([3, 8])
            .into()
    } else {
        Space::new(0, 0).into()
    };

    let header = container(
        row![
            back_btn,
            Space::new(16, 0),
            text(&session.name).size(14).color(s.text_primary),
            Space::new(8, 0),
            text("·").size(14).color(s.text_muted),
            Space::new(8, 0),
            text(repo_short(&session.repo)).size(13).color(s.accent),
            Space::new(Length::Fill, 0),
            panel_toggles,
            Space::new(Length::Fill, 0),
            kill_btn,
            Space::new(12, 0),
            status_dot,
            Space::new(6, 0),
            text(cost).size(12).color(s.text_muted),
            if !pr_label.is_empty() {
                Element::from(
                    row![
                        Space::new(12, 0),
                        text(pr_label.clone()).size(12).color(s.text_secondary),
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
    .style(move |_theme| container::Style {
        background: Some(Background::Color(s.bg_surface)),
        border: Border { color: s.border, width: 1.0, radius: 0.0.into() },
        ..Default::default()
    });

    // ── Terminal pane ─────────────────────────────────────────────────────────
    let terminal_bg = s.terminal_bg;
    let session_ids: Vec<String> = app.sessions.keys().cloned().collect();
    let terminal_pane: Element<Message> = if let Some(term_state) = app.terminals.get(session_id) {
        iced::widget::Canvas::new(TerminalWidget {
            state:        term_state,
            session_id:   session_id.to_string(),
            font_size:    13.0,
            terminal_bg:  s.terminal_bg,
            terminal_fg:  s.terminal_fg,
            cursor_color: s.accent,
            session_ids,
        })
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    } else {
        use athene_core::types::SessionStatus;
        let placeholder = match session.status {
            SessionStatus::Terminated | SessionStatus::Done => "Session exited",
            _ => "Terminal connecting…",
        };
        container(
            text(placeholder).size(13).color(s.text_muted),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .style(move |_theme| container::Style {
            background: Some(Background::Color(terminal_bg)),
            ..Default::default()
        })
        .into()
    };

    // ── Info pane ─────────────────────────────────────────────────────────────
    let pr = session.pr_id.and_then(|id| app.prs.get(&id));
    let ci = pr.and_then(|p| app.ci_status.get(&p.id));
    let comments = pr
        .and_then(|p| app.review_threads.get(&p.id))
        .map(|v| v.as_slice())
        .unwrap_or(&[]);

    let info_width = app.info_width;
    let info_pane: Element<Message> = container(
        info_panel(session, pr, ci, comments, s),
    )
    .width(Length::Fixed(info_width))
    .height(Length::Fill)
    .style(move |_theme| container::Style {
        background: Some(Background::Color(s.bg_surface)),
        border: Border { color: s.border, width: 1.0, radius: 0.0.into() },
        ..Default::default()
    })
    .into();

    // ── Panel routing ─────────────────────────────────────────────────────────
    let effective_panel = if is_orchestrator { &DetailPanel::Terminal } else { panel };
    let content: Element<Message> = match effective_panel {
        DetailPanel::Terminal => container(terminal_pane)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(move |_theme| container::Style {
                background: Some(Background::Color(terminal_bg)),
                ..Default::default()
            })
            .into(),
        DetailPanel::Split => row![
            container(terminal_pane)
                .width(Length::Fill)
                .height(Length::Fill)
                .style(move |_theme| container::Style {
                    background: Some(Background::Color(terminal_bg)),
                    ..Default::default()
                }),
            App::drag_handle(DragTarget::InfoPanel, s.border),
            info_pane,
        ]
        .height(Length::Fill)
        .into(),
        DetailPanel::Info => info_pane,
        DetailPanel::Inspector => inspector_panel(app, session).into(),
    };

    column![header, content]
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}
