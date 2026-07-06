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

fn can_resume(status: &ninox_core::types::SessionStatus) -> bool {
    matches!(status, ninox_core::types::SessionStatus::Interrupted)
}

// ── Terminal chrome budget ───────────────────────────────────────────────────
//
// `App::resize_terminals` picks a PTY grid size (cols/rows) from the window
// size *before* iced has laid anything out, so it has to predict how much of
// the window the terminal `Canvas` will actually get. If the prediction is
// too generous, the PTY (and alacritty's grid) end up bigger than the canvas
// that renders it, and — because `TerminalWidget::draw` anchors to the
// top-left — the bottom rows (including the live prompt line) clip off the
// bottom of the frame instead of being cut evenly.
//
// These two constants are that prediction, derived from the actual widget
// tree built below (`session_detail`), not eyeballed. iced's default text
// `LineHeight` is `Relative(1.3)` (see `iced_core::text::LineHeight`), so a
// `text(..).size(N)` line is `N * 1.3` px tall; that's the basis for every
// text-derived number here. Border `width` is intentionally excluded —
// iced draws container borders as a stroke over the box's existing bounds,
// it does not add to layout size (see `iced_widget::container::layout`).
//
// Height, top to bottom, above the terminal `Canvas` for *both* the
// `Terminal` and `Split` panels (they share the same `term_stage`/
// `term_frame` chrome — see `content` below):
//   - `header` container:             padding [14, 20] -> 28.0 vertical
//       + content row (max child):    `identity` column is tallest:
//         name text size 28 (28*1.3 = 36.4) + 4.0 spacing
//         + subline text size 10 (10*1.3 = 13.0)          = 53.4
//     header total:                                          81.4
//   - `tabs_block` container:         padding top 10 / bottom 0 -> 10.0
//       + content column:  panel_btn is a button(...) whose own
//         .padding([2, 2]) wraps its inner column (label 15*1.3=19.5
//         + 4.0 Space + 2.0 underline hline = 25.5), so the button lays
//         out at 25.5 + 4.0 = 29.5; plus the full-width 2.0 hline drawn
//         below the row                                       31.5
//     tabs_block total:                                       41.5
//   - `term_stage` container:         padding 16 all sides -> 32.0 vertical
//   - `term_frame`'s `title_bar`:     padding [7, 12] -> 14.0 vertical
//       + content row (max child):    dot 8.0 vs text 9.5*1.3=12.35 -> 12.35
//     title_bar total:                                        26.35
//   - hline between title_bar and the pane:                    1.0
//   ---------------------------------------------------------------------
//   sum:  81.4 + 41.5 + 32.0 + 26.35 + 1.0 = 182.25, rounded up for a small
//   safety margin (font metrics / hinting can round a hair differently than
//   this arithmetic; the canvas is top-anchored, so over-estimating is safe
//   while under-estimating clips the prompt row) to:
pub(crate) const TERM_CHROME_H: f32 = 185.0;

// Width chrome affecting the terminal canvas: only `term_stage`'s own
// padding narrows it (16 left + 16 right = 32.0) — everything else in the
// tree (header, tabs, title_bar) is either full-width or lays out
// vertically above the canvas, so it doesn't reduce the canvas's width.
// Sidebar/info-panel widths are already subtracted separately by the
// caller (`App::resize_terminals`), since those aren't part of this chrome.
pub(crate) const TERM_CHROME_W: f32 = 32.0;

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
    let ci_color = ci.map(|c| crate::style::ci_color(s, c));

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
        Some(n) => {
            let stamp = crate::style::stamp(&format!("PR #{n}"), ci_color.unwrap_or(s.status_pr_open));
            // Clickable when a browser URL is resolvable — opens the PR.
            match crate::app::pr_url_for_session(&app.prs, session) {
                Some(url) => iced::widget::mouse_area(stamp)
                    .interaction(iced::mouse::Interaction::Pointer)
                    .on_press(Message::OpenUrl(url))
                    .into(),
                None => stamp,
            }
        }
        None => Space::new(0, 0).into(),
    };

    // Re-file: kill + respawn the same name/workspace with the CURRENT
    // registry settings. Rendered for ALL sessions (orchestrators too —
    // this also covers respawning over a Terminated husk, which "just
    // spawns"). Disabled (faint, no press) when the session has no
    // recorded workspace to respawn into — the handler would refuse
    // anyway, but a clickable button that silently does nothing reads
    // as broken.
    let refile_btn: Element<Message> = {
        let can_refile = session.workspace_path.is_some();
        let sid = session_id.to_string();
        let label_color = if can_refile { s.ink_2 } else { s.faint };
        button(crate::style::micro_label("Re-file", label_color).size(10.0))
            .on_press_maybe(can_refile.then_some(Message::RefileSession(sid)))
            .padding([6, 16])
            .style(move |_theme, status| {
                let hovered = can_refile && matches!(status, button::Status::Hovered);
                button::Style {
                    background: hovered.then_some(Background::Color(s.ink)),
                    text_color: if hovered { s.card } else { label_color },
                    border: Border {
                        color: if can_refile { s.ink_2 } else { s.rule_dark },
                        width: 1.5,
                        radius: 2.0.into(),
                    },
                    shadow: crate::style::hard_shadow(s, 2.0, 2.0, crate::style::shadow_alpha(s).0),
                }
            })
            .into()
    };

    let resume_btn: Element<Message> = if can_resume(&session.status) {
        let sid = session_id.to_string();
        button(crate::style::micro_label("Resume", s.status_review).size(10.0))
            .on_press(Message::ResumeSession(sid))
            .padding([6, 16])
            .style(move |_theme, status| {
                let hovered = matches!(status, button::Status::Hovered);
                button::Style {
                    background: hovered.then_some(Background::Color(s.status_review)),
                    text_color: if hovered { s.card } else { s.status_review },
                    border: Border { color: s.status_review, width: 1.5, radius: 2.0.into() },
                    shadow: crate::style::hard_shadow(s, 2.0, 2.0, crate::style::shadow_alpha(s).0),
                }
            })
            .into()
    } else {
        Space::new(0, 0).into()
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
            refile_btn,
            Space::new(10, 0),
            resume_btn,
            Space::new(10, 0),
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
        let canvas = iced::widget::Canvas::new(TerminalWidget {
            state:        term_state,
            session_id:   session_id.to_string(),
            font_size:    FONT_SIZE,
            terminal_bg:  s.term_bg,
            terminal_fg:  s.term_fg,
            cursor_color: s.accent,
            ansi:         s.ansi,
            session_ids,
        })
        .width(Length::Fill)
        .height(Length::Fill);

        // Scrolled up into history — surface a floating button to jump back
        // down to the live output, since new output won't auto-scroll into
        // view while the user is reading scrollback.
        let jump_to_latest: Option<Element<Message>> = term_state.is_scrolled_back().then(|| {
            container(
                button(text("↓ Jump to latest").size(12).color(Color::WHITE))
                    .on_press(Message::JumpToLatest { session_id: session_id.to_string() })
                    .padding([6, 14])
                    .style(move |_theme, _status| button::Style {
                        background: Some(Background::Color(s.accent)),
                        border: Border { color: Color::TRANSPARENT, width: 0.0, radius: 999.0.into() },
                        text_color: Color::WHITE,
                        ..Default::default()
                    }),
            )
            .center_x(Length::Fill)
            .align_bottom(Length::Fill)
            .padding(iced::Padding::default().bottom(16))
            .into()
        });

        iced::widget::Stack::new()
            .width(Length::Fill)
            .height(Length::Fill)
            .push(canvas)
            .push_maybe(jump_to_latest)
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

    let (grid_cols, grid_rows) = app
        .terminals
        .get(session_id)
        .map(|t| t.grid_size())
        .unwrap_or((app.terminal_cols, app.terminal_rows));
    let tmux_line = format!("tmux · {} · {}×{}", session.id, grid_cols, grid_rows);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn can_resume_only_when_interrupted() {
        use ninox_core::types::SessionStatus;
        assert!(can_resume(&SessionStatus::Interrupted));
        assert!(!can_resume(&SessionStatus::Working));
        assert!(!can_resume(&SessionStatus::Terminated));
        assert!(!can_resume(&SessionStatus::Done));
    }
}
