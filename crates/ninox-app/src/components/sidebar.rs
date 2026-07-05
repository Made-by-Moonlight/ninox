use ninox_core::config::ThemeVariant;
use iced::{
    widget::{button, column, container, row, scrollable, text, Space},
    Alignment, Background, Border, Color, Element, Length, Padding,
};

use crate::{
    app::{App, Message, View},
    components::notification_panel::notification_panel,
    style::{hline, micro_label, MONO, SANS_BOLD, SERIF, SERIF_ITALIC, SERIF_MEDIUM},
};

fn repo_short(repo: &str) -> &str {
    repo.rsplit('/').next().unwrap_or(repo)
}

/// "N workers" / "1 worker" — the orchestrator tree row's worker-count label.
fn worker_count_label(count: usize) -> String {
    if count == 1 { "1 worker".to_string() } else { format!("{count} workers") }
}

/// Status dot: filled circle, 1.5px border in the status color.
/// Done/terminated renders hollow (transparent fill).
fn status_dot(color: Color, hollow: bool) -> Element<'static, Message> {
    container(Space::new(0, 0))
        .width(Length::Fixed(8.0))
        .height(Length::Fixed(8.0))
        .style(move |_| container::Style {
            background: (!hollow).then_some(Background::Color(color)),
            border: Border { color, width: 1.5, radius: 4.0.into() },
            ..Default::default()
        })
        .into()
}

/// One table-of-contents row: roman numeral, serif label, dotted leader, key hint.
fn toc_item<'a>(
    app: &'a App,
    numeral: &'a str,
    label: &'a str,
    key: &'a str,
    msg: Message,
    active: bool,
) -> Element<'a, Message> {
    let s = &app.scheme;
    let bar_color = if active { s.accent } else { Color::TRANSPARENT };
    let rn_color = if active { s.accent } else { s.faint };
    let lbl_color = if active { s.ink } else { s.ink_2 };
    let lbl_font = if active { SERIF_MEDIUM } else { SERIF };

    button(
        row![
            container(Space::new(0, 0)).width(3).height(Length::Fixed(18.0)).style(
                move |_| container::Style {
                    background: Some(Background::Color(bar_color)),
                    ..Default::default()
                }
            ),
            Space::new(15, 0),
            text(numeral).size(12).font(SERIF_ITALIC).color(rn_color).width(Length::Fixed(22.0)),
            text(label).size(15).font(lbl_font).color(lbl_color),
            container(
                text("· ".repeat(40)).size(9).color(s.rule_dark)
                    .wrapping(iced::widget::text::Wrapping::None)
            ).width(Length::Fill).height(Length::Fixed(10.0)).clip(true).padding(Padding { top: 6.0, right: 4.0, bottom: 0.0, left: 6.0 }),
            text(key).size(9).font(MONO).color(s.faint),
        ]
        .align_y(Alignment::Center),
    )
    .on_press(msg)
    .padding(Padding { top: 4.0, right: 18.0, bottom: 4.0, left: 0.0 })
    .width(Length::Fill)
    .style(move |_t, status| button::Style {
        background: None,
        text_color: if matches!(status, button::Status::Hovered) { s.ink } else { lbl_color },
        border: Border::default(),
        ..Default::default()
    })
    .into()
}

pub fn sidebar(app: &App) -> Element<'_, Message> {
    let s = &app.scheme;

    #[cfg(target_os = "macos")]
    let masthead_padding = Padding { top: 40.0, right: 18.0, bottom: 14.0, left: 18.0 };
    #[cfg(not(target_os = "macos"))]
    let masthead_padding = Padding { top: 20.0, right: 18.0, bottom: 14.0, left: 18.0 };

    // ── 1. Masthead ──────────────────────────────────────────────────────────
    let masthead = container(
        column![
            row![
                text("Nin").size(27).font(SERIF_MEDIUM).color(s.ink),
                text("ox").size(27).font(SERIF_ITALIC).color(s.ink),
                text(" ⬡").size(20).font(crate::style::GLYPH).color(s.ink),
            ]
            .align_y(Alignment::End),
            Space::new(0, 6),
            micro_label("Fleet Field Journal", s.ink_2).size(9.0),
        ],
    )
    .padding(masthead_padding)
    .width(Length::Fill);

    // ── 2. Table-of-contents nav ─────────────────────────────────────────────
    let on_fleet = matches!(app.view, View::FleetBoard { .. });
    let on_session = matches!(app.view, View::SessionDetail { .. });
    let on_prs = matches!(app.view, View::PrList);
    let on_brain = matches!(app.view, View::Brain);
    let toc = column![
        toc_item(app, "I.", "Fleet board", "1", Message::NavigateFleet { scope: None }, on_fleet),
        toc_item(app, "II.", "Session", "2", Message::NavigateLastSession, on_session),
        toc_item(app, "III.", "Pull requests", "3", Message::NavigatePrList, on_prs),
        toc_item(app, "IV.", "Brain", "4", Message::NavigateBrain, on_brain),
    ]
    .padding(Padding { top: 10.0, right: 0.0, bottom: 10.0, left: 0.0 });

    // ── 3. Action row: Alerts (badge) · + Spawn ─────────────────────────────
    let unread = app.notifications.len();
    let alerts_label: Element<Message> = if unread > 0 {
        row![
            micro_label("Alerts", s.ink_2).size(10.0),
            Space::new(6, 0),
            container(text(unread.min(99).to_string()).size(8).font(SANS_BOLD).color(s.card))
                .padding([1, 4])
                .style(move |_| container::Style {
                    background: Some(Background::Color(s.accent)),
                    border: Border { radius: 7.0.into(), ..Default::default() },
                    ..Default::default()
                }),
        ]
        .align_y(Alignment::Center)
        .into()
    } else {
        micro_label("Alerts", s.ink_2).size(10.0).into()
    };

    let action_btn_style = move |_t: &iced::Theme, status: button::Status| button::Style {
        background: matches!(status, button::Status::Hovered)
            .then_some(Background::Color(s.card)),
        text_color: s.ink_2,
        border: Border::default(),
        ..Default::default()
    };
    let actions = row![
        button(container(alerts_label).center_x(Length::Fill))
            .on_press(Message::ToggleNotifications)
            .style(action_btn_style)
            .padding([9, 4])
            .width(Length::Fill),
        container(Space::new(0, 0)).width(1).height(Length::Fixed(30.0)).style(
            move |_| container::Style {
                background: Some(Background::Color(s.rule_dark)),
                ..Default::default()
            }
        ),
        button(container(micro_label("+ Spawn", s.accent).size(10.0)).center_x(Length::Fill))
            .on_press(Message::SpawnSession)
            .style(action_btn_style)
            .padding([9, 4])
            .width(Length::Fill),
    ]
    .align_y(Alignment::Center);

    // ── 4. Session tree ──────────────────────────────────────────────────────
    let mut items: Vec<Element<Message>> = Vec::new();
    if !app.orchestrators.is_empty() {
        items.push(
            container(text("Orchestrators").size(13).font(SERIF_ITALIC).color(s.faint))
                .padding(Padding { top: 12.0, right: 18.0, bottom: 4.0, left: 18.0 })
                .into(),
        );
    }
    for orch in &app.orchestrators {
        let is_expanded = app.sidebar.selected_orchestrator.as_deref() == Some(orch.id.as_str());
        let worker_count = app
            .sessions
            .values()
            .filter(|w| w.orchestrator_id.as_deref() == Some(orch.id.as_str()))
            .count();
        items.push(tree_row(
            app,
            &orch.id,
            &orch.name,
            &worker_count_label(worker_count),
            app.sessions.get(&orch.id).map(|se| &se.status),
            true,  // bold
            false, // not indented
            Some(if is_expanded { None } else { Some(orch.id.clone()) }), // chevron toggle target
            Some(Message::RemoveOrchestrator(orch.id.clone())),
        ));
        if is_expanded {
            let mut workers: Vec<_> = app
                .sessions
                .values()
                .filter(|w| w.orchestrator_id.as_deref() == Some(orch.id.as_str()))
                .collect();
            workers.sort_by(|a, b| a.name.cmp(&b.name));
            for w in workers {
                items.push(tree_row(
                    app, &w.id, &w.name, repo_short(&w.repo),
                    Some(&w.status), false, true, None,
                    Some(Message::RemoveSession(w.id.clone())),
                ));
            }
        }
    }
    let mut standalone: Vec<_> = app
        .sessions
        .values()
        .filter(|w| {
            w.orchestrator_id.is_none() && !app.orchestrators.iter().any(|o| o.id == w.id)
        })
        .collect();
    standalone.sort_by(|a, b| a.name.cmp(&b.name));
    if !standalone.is_empty() {
        items.push(
            container(text("Standalone").size(13).font(SERIF_ITALIC).color(s.faint))
                .padding(Padding { top: 12.0, right: 18.0, bottom: 4.0, left: 18.0 })
                .into(),
        );
    }
    for w in standalone {
        items.push(tree_row(
            app, &w.id, &w.name, repo_short(&w.repo),
            Some(&w.status), false, false, None,
            Some(Message::RemoveSession(w.id.clone())),
        ));
    }
    let list = scrollable(column(items).width(Length::Fill)).height(Length::Fill);

    // ── 5. Footer: theme dots ────────────────────────────────────────────────
    let footer = theme_dots_footer(app);

    let mut col_items: Vec<Element<Message>> = vec![
        masthead.into(),
        hline(s.rule_dark, 1.0),
        toc.into(),
        hline(s.rule_dark, 1.0),
        actions.into(),
        hline(s.rule_dark, 1.0),
    ];
    if app.sidebar.show_notifications {
        col_items.push(notification_panel(app));
    }
    col_items.push(list.into());
    col_items.push(hline(s.rule_dark, 1.0));
    col_items.push(footer);

    // Sidebar edge is a structural 2px ink border (right side only — iced borders
    // are uniform, so draw the edge as a separate vertical line).
    row![
        container(column(col_items))
            .width(Length::Fixed(app.sidebar_width - 2.0))
            .height(Length::Fill)
            .style(move |_| container::Style {
                background: Some(Background::Color(s.paper_2)),
                ..Default::default()
            }),
        container(Space::new(0, 0)).width(2).height(Length::Fill).style(move |_| {
            container::Style { background: Some(Background::Color(s.ink)), ..Default::default() }
        }),
    ]
    .into()
}

/// One session-tree row: status dot + name + mono repo slug; active = card bg
/// + vermilion left bar; × remove button.
#[allow(clippy::too_many_arguments)]
fn tree_row<'a>(
    app: &'a App,
    id: &str,
    name: &'a str,
    right: &str,
    status: Option<&ninox_core::types::SessionStatus>,
    bold: bool,
    indented: bool,
    chevron_toggle: Option<Option<ninox_core::types::OrchestratorId>>,
    remove: Option<Message>,
) -> Element<'a, Message> {
    let s = &app.scheme;
    let is_active = matches!(&app.view, View::SessionDetail { session_id, .. } if session_id == id);
    let dot: Element<Message> = match status {
        Some(st) => status_dot(
            s.status_color(st),
            matches!(st, ninox_core::types::SessionStatus::Done
                        | ninox_core::types::SessionStatus::Terminated),
        ),
        None => Space::new(8, 0).into(),
    };
    let name_font = if bold { SANS_BOLD } else { crate::style::SANS };
    let left_pad = if indented { 38.0 } else { 18.0 };

    // Row-level hover/active background: shared by the navigate button and
    // (so the whole row still reads as one control) the chevron/× buttons,
    // which otherwise sit transparent. All three are SIBLING buttons — an
    // iced `button` swallows presses on any button nested inside it, which
    // is why the chevron/× used to be dead when they lived inside the
    // navigate button's content.
    let row_bg = move |hovered: bool| {
        (is_active || hovered).then_some(Background::Color(s.card))
    };

    let navigate = button(
        row![
            container(Space::new(0, 0)).width(3).height(Length::Fixed(20.0)).style(move |_| {
                container::Style {
                    background: Some(Background::Color(if is_active { s.accent } else { Color::TRANSPARENT })),
                    ..Default::default()
                }
            }),
            Space::new(left_pad - 3.0, 0),
            dot,
            Space::new(9, 0),
            text(name.to_owned()).size(12.5).font(name_font).color(if is_active || bold { s.ink } else { s.ink_2 }),
            Space::new(Length::Fill, 0),
            text(right.to_owned()).size(10).font(MONO).color(s.faint),
        ]
        .align_y(Alignment::Center),
    )
    .on_press(Message::NavigateSession(id.to_owned()))
    .style(move |_t, status| button::Style {
        background: row_bg(matches!(status, button::Status::Hovered)),
        text_color: s.ink_2,
        border: Border::default(),
        ..Default::default()
    })
    .padding(Padding { top: 3.0, right: 0.0, bottom: 3.0, left: 0.0 })
    .width(Length::Fill);

    let mut row_items: Vec<Element<Message>> = vec![navigate.into()];

    if let Some(toggle_target) = chevron_toggle {
        row_items.push(Space::new(4, 0).into());
        row_items.push(
            button(text(if toggle_target.is_none() { "▾" } else { "▸" }).size(9).color(s.faint))
                .on_press(Message::SelectOrchestrator(toggle_target))
                .style(move |_t, status| button::Style {
                    background: row_bg(matches!(status, button::Status::Hovered)),
                    border: Border::default(),
                    ..Default::default()
                })
                .padding([2, 4])
                .into(),
        );
    }
    if let Some(remove_msg) = remove {
        row_items.push(
            button(text("×").size(12).color(s.faint))
                .on_press(remove_msg)
                .style(move |_t, status| button::Style {
                    background: row_bg(matches!(status, button::Status::Hovered)),
                    border: Border::default(),
                    ..Default::default()
                })
                .padding([2, 6])
                .into(),
        );
    }

    container(row(row_items).align_y(Alignment::Center))
        .padding(Padding { top: 0.0, right: 10.0, bottom: 0.0, left: 0.0 })
        .width(Length::Fill)
        .into()
}

/// Footer: "THEME" microlabel + one dot per variant; selected dot ringed in accent.
fn theme_dots_footer(app: &App) -> Element<'_, Message> {
    let s = &app.scheme;
    let mut dots = row![].spacing(6).align_y(Alignment::Center);
    for variant in [ThemeVariant::Light, ThemeVariant::Dark, ThemeVariant::Ninox] {
        let selected = app.active_variant == variant;
        let fill = match variant {
            ThemeVariant::Light => crate::theme::light().paper,
            ThemeVariant::Dark | ThemeVariant::Ninox => crate::theme::dark().paper,
        };
        dots = dots.push(
            button(
                container(Space::new(0, 0)).width(14).height(Length::Fixed(14.0)).style(
                    move |_| container::Style {
                        background: Some(Background::Color(fill)),
                        border: Border {
                            color: if selected { s.accent } else { s.ink },
                            width: if selected { 2.0 } else { 1.5 },
                            radius: 7.0.into(),
                        },
                        ..Default::default()
                    },
                ),
            )
            .on_press(Message::SwitchTheme(variant))
            .style(|_t, _st| button::Style { background: None, border: Border::default(), ..Default::default() })
            .padding(0),
        );
    }
    container(
        row![
            micro_label("Theme", s.ink_2).size(10.0),
            Space::new(Length::Fill, 0),
            dots,
        ]
        .align_y(Alignment::Center),
    )
    .padding([12, 18])
    .width(Length::Fill)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_count_label_pluralizes_correctly() {
        assert_eq!(worker_count_label(0), "0 workers");
        assert_eq!(worker_count_label(1), "1 worker");
        assert_eq!(worker_count_label(2), "2 workers");
        assert_eq!(worker_count_label(11), "11 workers");
    }
}
