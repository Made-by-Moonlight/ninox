use iced::{
    widget::{button, column, container, row, text, Space},
    Alignment, Background, Border, Color, Element, Length, Padding,
};

use crate::{
    app::{App, DragTarget, Message},
    components::{info_panel::info_panel, inspector_panel::inspector_panel, terminal::{TerminalWidget, FONT_SIZE}},
    theme::ColorScheme,
};

fn repo_short(repo: &str) -> &str {
    repo.rsplit('/').next().unwrap_or(repo)
}

/// Panel tab — italic serif label, accent underline when active, sitting
/// flush above the full-width ink rule drawn by the caller.
fn panel_btn<'a>(app: &'a App, label: &'static str, target: DetailPanel, active: DetailPanel) -> Element<'a, Message> {
    let s = &app.scheme;
    let is_active = target == active;
    button(
        column![
            text(label).size(15)
                .font(if is_active { crate::style::SERIF_MEDIUM_ITALIC } else { crate::style::SERIF_ITALIC })
                .color(if is_active { s.ink } else { s.faint }),
            Space::new(0, 4),
            crate::style::hline(if is_active { s.accent } else { Color::TRANSPARENT }, 2.0),
        ],
    )
    .on_press(Message::SwitchDetailPanel(target))
    .style(|_theme, _status| button::Style { background: None, border: Border::default(), ..Default::default() })
    .padding([2, 2])
    .into()
}

/// The "dark object" terminal frame: title bar (status dot, tmux id/size,
/// status word) over a 1px rule over the terminal canvas, all inside a
/// 2px ink border with a hard offset shadow.
fn term_frame<'a>(
    s: &'a ColorScheme,
    dot_color: Color,
    tmux_line: String,
    status_word: String,
    pane: Element<'a, Message>,
) -> Element<'a, Message> {
    let title_bar = container(
        row![
            container(Space::new(0, 0))
                .width(Length::Fixed(8.0))
                .height(Length::Fixed(8.0))
                .style(move |_theme| container::Style {
                    background: Some(Background::Color(dot_color)),
                    border: Border { radius: 4.0.into(), ..Default::default() },
                    ..Default::default()
                }),
            Space::new(10, 0),
            text(tmux_line).size(9.5).font(crate::style::MONO).color(s.status_done),
            Space::new(Length::Fill, 0),
            text(status_word).size(9.5).font(crate::style::MONO).color(s.status_done),
        ]
        .align_y(Alignment::Center),
    )
    .padding([7, 12])
    .width(Length::Fill)
    .style(move |_theme| container::Style {
        background: Some(Background::Color(s.term_bar)),
        border: Border { color: s.term_bar_border, width: 0.0, radius: 0.0.into() },
        ..Default::default()
    });

    container(
        column![
            title_bar,
            crate::style::hline(s.term_bar_border, 1.0),
            pane,
        ],
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .style(move |_theme| container::Style {
        background: Some(Background::Color(s.term_bg)),
        border: Border { color: s.ink, width: 2.0, radius: 3.0.into() },
        shadow: crate::style::hard_shadow(s, 4.0, 5.0, crate::style::shadow_alpha(s).1),
        ..Default::default()
    })
    .into()
}

/// Wraps the framed terminal with the paper-colored margin it needs to read
/// as an object sitting on the page (otherwise the hard shadow has nowhere
/// to fall).
fn term_stage<'a>(s: &'a ColorScheme, framed: Element<'a, Message>) -> Element<'a, Message> {
    container(framed)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(16)
        .style(move |_theme| container::Style {
            background: Some(Background::Color(s.paper)),
            ..Default::default()
        })
        .into()
}

/// Panel selection — which view is active in session detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DetailPanel {
    Terminal,
    #[default]
    Split,
    Info,
    Inspector,
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
            text("Session not found").size(14).color(s.faint),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(20)
        .into();
    };

    let color = s.status_color(&session.status);
    let cost = format!("${:.4}", session.cost_usd);

    // ── PR + CI + comments (fed to the header's PR stamp and the info pane) ────
    let pr = session.pr_id.and_then(|id| app.prs.get(&id));
    let ci = pr.and_then(|p| app.ci_status.get(&p.id));
    let comments = pr
        .and_then(|p| app.review_threads.get(&p.id))
        .map(|v| v.as_slice())
        .unwrap_or(&[]);
    let ci_color = ci.map(|c| {
        if c.failing > 0 {
            s.status_ci_failed
        } else if c.pending > 0 {
            s.status_review
        } else {
            s.status_working
        }
    });

    // ── Header ────────────────────────────────────────────────────────────────
    let status_dot = container(Space::new(0, 0))
        .width(Length::Fixed(10.0))
        .height(Length::Fixed(10.0))
        .style(move |_theme| container::Style {
            background: Some(Background::Color(color)),
            border: Border { color: Color::TRANSPARENT, width: 0.0, radius: 5.0.into() },
            ..Default::default()
        });

    let back_scope = app.last_fleet_scope.clone();
    let back_btn = button(text("←").size(15).font(crate::style::SANS).color(s.ink))
        .on_press(Message::NavigateFleet { scope: back_scope })
        .width(Length::Fixed(30.0))
        .height(Length::Fixed(30.0))
        .style(move |_theme, _status| button::Style {
            background: Some(Background::Color(s.card)),
            text_color: s.ink,
            border: Border { color: s.ink, width: 1.5, radius: 2.0.into() },
            shadow: crate::style::hard_shadow(s, 2.0, 2.0, crate::style::shadow_alpha(s).0),
        });

    let orch_name = session.orchestrator_id.as_deref()
        .and_then(|oid| app.orchestrators.iter().find(|o| o.id == oid))
        .map(|o| o.name.as_str());
    let mut subline_parts: Vec<String> = Vec::new();
    if !session.repo.is_empty() {
        subline_parts.push(repo_short(&session.repo).to_string());
    }
    if let Some(name) = orch_name {
        subline_parts.push(format!("worker of {name}"));
    }
    let subline = subline_parts.join(" · ");

    let identity = column![
        text(&session.name).size(28).font(crate::style::SERIF_MEDIUM).color(s.ink),
        text(subline).size(10).font(crate::style::MONO).color(s.faint),
    ]
    .spacing(4);

    let is_orchestrator = app.orchestrators.iter().any(|o| o.id == session_id);

    let pr_stamp: Element<Message> = match session.pr_number {
        Some(n) => crate::style::stamp(&format!("PR #{n}"), ci_color.unwrap_or(s.status_pr_open)),
        None => Space::new(0, 0).into(),
    };

    let kill_btn: Element<Message> = if !is_orchestrator {
        let sid = session_id.to_string();
        button(crate::style::micro_label("Kill", s.accent).size(10.0))
            .on_press(Message::RemoveSession(sid))
            .padding([6, 16])
            .style(move |_theme, status| {
                let hovered = matches!(status, button::Status::Hovered);
                button::Style {
                    background: hovered.then_some(Background::Color(s.accent)),
                    text_color: if hovered { s.card } else { s.accent },
                    border: Border { color: s.accent, width: 1.5, radius: 2.0.into() },
                    shadow: crate::style::hard_shadow(s, 2.0, 2.0, crate::style::shadow_alpha(s).0),
                }
            })
            .into()
    } else {
        Space::new(0, 0).into()
    };

    let header = container(
        row![
            back_btn,
            Space::new(16, 0),
            status_dot,
            Space::new(10, 0),
            identity,
            Space::new(Length::Fill, 0),
            pr_stamp,
            Space::new(14, 0),
            text(cost).size(13).font(crate::style::MONO).color(s.ink_2),
            Space::new(14, 0),
            kill_btn,
        ]
        .align_y(Alignment::Center),
    )
    .padding([14, 20])
    .width(Length::Fill)
    .style(move |_theme| container::Style {
        background: Some(Background::Color(s.card)),
        border: Border { color: s.rule_dark, width: 1.0, radius: 0.0.into() },
        ..Default::default()
    });

    // ── Panel tabs ────────────────────────────────────────────────────────────
    let tabs_block: Element<Message> = if is_orchestrator {
        Space::new(0, 0).into()
    } else {
        container(
            column![
                row![
                    panel_btn(app, "Terminal", DetailPanel::Terminal, *panel),
                    panel_btn(app, "Split", DetailPanel::Split, *panel),
                    panel_btn(app, "Info", DetailPanel::Info, *panel),
                    panel_btn(app, "Inspector", DetailPanel::Inspector, *panel),
                ]
                .spacing(22)
                .align_y(Alignment::Center),
                crate::style::hline(s.ink, 2.0),
            ],
        )
        .padding(Padding { top: 10.0, right: 28.0, bottom: 0.0, left: 28.0 })
        .width(Length::Fill)
        .style(move |_theme| container::Style {
            background: Some(Background::Color(s.card)),
            ..Default::default()
        })
        .into()
    };

    // ── Terminal pane ─────────────────────────────────────────────────────────
    let terminal_bg = s.term_bg;
    let session_ids: Vec<String> = app.sessions.keys().cloned().collect();
    let terminal_pane: Element<Message> = if let Some(term_state) = app.terminals.get(session_id) {
        iced::widget::Canvas::new(TerminalWidget {
            state:        term_state,
            session_id:   session_id.to_string(),
            font_size:    FONT_SIZE,
            terminal_bg:  s.term_bg,
            terminal_fg:  s.term_fg,
            cursor_color: s.accent,
            session_ids,
        })
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    } else {
        use ninox_core::types::SessionStatus;
        let placeholder = match session.status {
            SessionStatus::Terminated | SessionStatus::Done => "Session exited",
            _ => "Terminal connecting…",
        };
        container(
            text(placeholder).size(13).color(s.faint),
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

    let tmux_line = format!("tmux · {} · {}×{}", session.id, app.terminal_cols, app.terminal_rows);
    let status_word = crate::style::stamp_word(&session.status).to_lowercase();

    // ── Info pane ─────────────────────────────────────────────────────────────
    let info_width = app.info_width;
    let info_pane: Element<Message> = container(
        info_panel(session, pr, ci, comments, s),
    )
    .width(Length::Fixed(info_width))
    .height(Length::Fill)
    .style(move |_theme| container::Style {
        background: Some(Background::Color(s.card)),
        border: Border { color: s.rule_dark, width: 1.0, radius: 0.0.into() },
        ..Default::default()
    })
    .into();

    // ── Panel routing ─────────────────────────────────────────────────────────
    let effective_panel = if is_orchestrator { &DetailPanel::Terminal } else { panel };
    let content: Element<Message> = match effective_panel {
        DetailPanel::Terminal => {
            term_stage(s, term_frame(s, color, tmux_line, status_word, terminal_pane))
        }
        DetailPanel::Split => row![
            term_stage(s, term_frame(s, color, tmux_line, status_word, terminal_pane)),
            App::drag_handle(DragTarget::InfoPanel, s.rule_dark),
            info_pane,
        ]
        .height(Length::Fill)
        .into(),
        DetailPanel::Info => info_pane,
        DetailPanel::Inspector => inspector_panel(app, session),
    };

    column![header, tabs_block, content]
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}
