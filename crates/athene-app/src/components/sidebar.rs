use athene_core::config::ThemeVariant;
use iced::{
    widget::{button, column, container, row, scrollable, text, Space},
    Alignment, Background, Border, Color, Element, Length, Padding,
};

use crate::{
    app::{App, Message, View},
    components::notification_panel::notification_panel,
    theme::ColorScheme,
};

fn repo_short(repo: &str) -> &str {
    repo.rsplit('/').next().unwrap_or(repo)
}

fn status_dot(color: Color) -> Element<'static, Message> {
    container(Space::new(0, 0))
        .width(Length::Fixed(7.0))
        .height(Length::Fixed(7.0))
        .style(move |_theme| container::Style {
            background: Some(Background::Color(color)),
            border: Border { color: Color::TRANSPARENT, width: 0.0, radius: 4.0.into() },
            ..Default::default()
        })
        .into()
}

fn theme_swatch(color: Color) -> Element<'static, Message> {
    container(Space::new(0, 0))
        .width(Length::Fixed(12.0))
        .height(Length::Fixed(12.0))
        .style(move |_theme| container::Style {
            background: Some(Background::Color(color)),
            border: Border {
                color: Color { r: 1.0, g: 1.0, b: 1.0, a: 0.12 },
                width: 1.0,
                radius: 3.0.into(),
            },
            ..Default::default()
        })
        .into()
}

pub fn sidebar(app: &App) -> Element<'_, Message> {
    let s = &app.scheme;

    // On macOS with fullsize_content_view the traffic lights sit at the top-left
    // (~y=28px). Extra top padding clears them so content doesn't clip under.
    #[cfg(target_os = "macos")]
    let header_padding = Padding { top: 36.0, right: 12.0, bottom: 12.0, left: 12.0 };
    #[cfg(not(target_os = "macos"))]
    let header_padding = Padding { top: 12.0, right: 12.0, bottom: 12.0, left: 12.0 };

    // ── Header ────────────────────────────────────────────────────────────────
    let unread = app.notifications.len();
    let bell_label = if unread > 0 {
        format!("🔔 {}", unread.min(99))
    } else {
        "🔔".to_string()
    };

    let pr_count = app.prs.len();
    let prs_label = if pr_count > 0 {
        format!("PRs ({})", pr_count)
    } else {
        "PRs".to_string()
    };

    let header = container(
        row![
            text("⬡ Athene").size(13).color(s.text_primary),
            Space::new(Length::Fill, 0),
            button(text(bell_label.clone()).size(11).color(
                if unread > 0 { s.accent } else { s.text_muted }
            ))
            .on_press(Message::ToggleNotifications)
            .style(move |_theme, _status| button::Style {
                background: None,
                text_color: if unread > 0 { s.accent } else { s.text_muted },
                border: Border { color: Color::TRANSPARENT, width: 0.0, radius: 4.0.into() },
                ..Default::default()
            })
            .padding([2, 4]),
            button(text(prs_label.clone()).size(11).color(s.text_secondary))
                .on_press(Message::NavigatePrList)
                .style(move |_theme, _status| button::Style {
                    background: None,
                    text_color: s.text_secondary,
                    border: Border { color: s.border, width: 1.0, radius: 4.0.into() },
                    ..Default::default()
                })
                .padding([2, 8]),
            button(text("+ Spawn").size(11).color(s.accent))
                .on_press(Message::SpawnSession)
                .style(move |_theme, _status| button::Style {
                    background: None,
                    text_color: s.accent,
                    border: Border { color: s.accent, width: 1.0, radius: 4.0.into() },
                    ..Default::default()
                })
                .padding([2, 8]),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .padding(header_padding)
    .width(Length::Fill)
    .style(move |_theme| container::Style {
        background: Some(Background::Color(s.bg_sidebar)),
        border: Border { color: s.border, width: 0.0, radius: 0.0.into() },
        ..Default::default()
    });

    // ── Session list ──────────────────────────────────────────────────────────
    let mut items: Vec<Element<Message>> = Vec::new();

    for orch in &app.orchestrators {
        let is_expanded = app.sidebar.selected_orchestrator.as_deref() == Some(orch.id.as_str());
        let toggle_icon = if is_expanded { "▼" } else { "▶" };
        let orch_id = orch.id.clone();
        let is_viewing = matches!(&app.view, View::SessionDetail { session_id, .. } if session_id == &orch.id);

        let chevron = button(text(toggle_icon).size(10).color(s.text_muted))
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

        let name_btn = button(text(&orch.name).size(13).color(s.text_primary))
            .on_press(Message::NavigateSession(orch_id.clone()))
            .style(move |_theme, _status| button::Style {
                background: if is_viewing { Some(Background::Color(s.bg_surface)) } else { None },
                text_color: s.text_primary,
                border: Border::default(),
                ..Default::default()
            })
            .padding([6, 4])
            .width(Length::Fill);

        let remove_id = orch.id.clone();
        let remove_btn = button(text("×").size(12).color(s.text_muted))
            .on_press(Message::RemoveOrchestrator(remove_id))
            .style(|_theme, _status| button::Style {
                background: None,
                border: Border::default(),
                ..Default::default()
            })
            .padding([6, 8]);

        let orch_row: Element<Message> = row![chevron, name_btn, remove_btn]
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

    // Standalone workers (no orchestrator, and not an orchestrator session itself)
    let standalone: Vec<&athene_core::types::Session> = app
        .sessions
        .values()
        .filter(|s| {
            s.orchestrator_id.is_none()
                && !app.orchestrators.iter().any(|o| o.id == s.id)
        })
        .collect();

    for session in standalone {
        items.push(standalone_row(app, session));
    }

    let list = scrollable(
        column(items).spacing(1).width(Length::Fill),
    )
    .height(Length::Fill);

    // ── Footer: theme popout ──────────────────────────────────────────────────
    let footer = theme_footer(app, s);

    // ── Notification panel (conditional overlay between header and list) ──────
    let mut col_items: Vec<Element<Message>> = vec![header.into()];
    if app.sidebar.show_notifications {
        col_items.push(notification_panel(app));
    }
    col_items.push(list.into());
    col_items.push(footer.into());

    container(column(col_items).spacing(0))
        .width(Length::Fixed(app.sidebar_width))
        .height(Length::Fill)
        .style(move |_theme| container::Style {
            background: Some(Background::Color(s.bg_sidebar)),
            border: Border { color: s.border, width: 1.0, radius: 0.0.into() },
            ..Default::default()
        })
        .into()
}

fn worker_row<'a>(app: &'a App, session: &'a athene_core::types::Session) -> Element<'a, Message> {
    let s = &app.scheme;
    let is_selected = matches!(
        &app.view,
        View::SessionDetail { session_id: id, .. } if id == &session.id
    );

    let color = s.status_color(&session.status);
    let bg = if is_selected { Some(Background::Color(s.bg_surface)) } else { None };
    let session_id = session.id.clone();

    button(
        row![
            status_dot(color),
            Space::new(6, 0),
            column![
                text(&session.name).size(12).color(s.text_primary),
                text(repo_short(&session.repo)).size(10).color(s.text_secondary),
            ]
            .spacing(1),
        ]
        .spacing(0)
        .align_y(Alignment::Center),
    )
    .on_press(Message::NavigateSession(session_id))
    .style(move |_theme, _status| button::Style {
        background: bg,
        text_color: s.text_primary,
        border: Border::default(),
        ..Default::default()
    })
    .padding(iced::Padding { top: 5.0, right: 12.0, bottom: 5.0, left: 26.0 })
    .width(Length::Fill)
    .into()
}

fn standalone_row<'a>(app: &'a App, session: &'a athene_core::types::Session) -> Element<'a, Message> {
    let s = &app.scheme;
    let is_selected = matches!(
        &app.view,
        View::SessionDetail { session_id: id, .. } if id == &session.id
    );

    let color = s.status_color(&session.status);
    let bg = if is_selected { Some(Background::Color(s.bg_surface)) } else { None };
    let nav_id = session.id.clone();
    let remove_id = session.id.clone();

    let content_btn = button(
        row![
            status_dot(color),
            Space::new(6, 0),
            column![
                text(&session.name).size(12).color(s.text_primary),
                text(repo_short(&session.repo)).size(10).color(s.text_secondary),
            ]
            .spacing(1),
        ]
        .spacing(0)
        .align_y(Alignment::Center),
    )
    .on_press(Message::NavigateSession(nav_id))
    .style(move |_theme, _status| button::Style {
        background: bg,
        text_color: s.text_primary,
        border: Border::default(),
        ..Default::default()
    })
    .padding(iced::Padding { top: 5.0, right: 4.0, bottom: 5.0, left: 26.0 })
    .width(Length::Fill);

    let remove_btn = button(text("×").size(12).color(s.text_muted))
        .on_press(Message::RemoveSession(remove_id))
        .style(|_theme, _status| button::Style {
            background: None,
            border: Border::default(),
            ..Default::default()
        })
        .padding([6, 8]);

    row![content_btn, remove_btn]
        .align_y(Alignment::Center)
        .width(Length::Fill)
        .into()
}

fn theme_footer<'a>(app: &'a App, s: &'a ColorScheme) -> Element<'a, Message> {
    let mut col_items: Vec<Element<'a, Message>> = Vec::new();

    if app.sidebar.show_theme_popout {
        for variant in [ThemeVariant::Light, ThemeVariant::Dark] {
            let is_active = app.active_variant == variant;
            let label = match variant {
                ThemeVariant::Light  => "Light",
                ThemeVariant::Dark   => "Dark",
                ThemeVariant::Athene => "Athene",
            };
            let swatch_color = match variant {
                ThemeVariant::Light  => crate::theme::light().bg_base,
                ThemeVariant::Dark   => crate::theme::dark().bg_base,
                ThemeVariant::Athene => crate::theme::warm_dark().bg_base,
            };

            let check: Element<Message> = if is_active {
                text("✓").size(11).color(s.accent).into()
            } else {
                Space::new(0, 0).into()
            };

            let option_btn = button(
                row![
                    theme_swatch(swatch_color),
                    Space::new(6, 0),
                    text(label).size(12).color(if is_active { s.text_primary } else { s.text_secondary }),
                    Space::new(Length::Fill, 0),
                    check,
                ]
                .align_y(Alignment::Center),
            )
            .on_press(Message::SwitchTheme(variant))
            .style(move |_theme, _status| button::Style {
                background: if is_active {
                    Some(Background::Color(Color { a: 0.1, ..s.accent }))
                } else {
                    None
                },
                border: Border { color: Color::TRANSPARENT, width: 0.0, radius: 6.0.into() },
                text_color: s.text_primary,
                ..Default::default()
            })
            .padding([6, 10])
            .width(Length::Fill);

            col_items.push(option_btn.into());
        }

        col_items.push(
            container(Space::new(Length::Fill, 1))
                .width(Length::Fill)
                .style(move |_theme| container::Style {
                    background: Some(Background::Color(s.border)),
                    ..Default::default()
                })
                .into(),
        );
    }

    let variant_label = match app.active_variant {
        ThemeVariant::Light  => "Light",
        ThemeVariant::Dark   => "Dark",
        ThemeVariant::Athene => "Athene",
    };
    let arrow = if app.sidebar.show_theme_popout { "↓" } else { "↑" };

    let trigger = button(
        row![
            text("Theme").size(11).color(s.text_muted),
            Space::new(Length::Fill, 0),
            text(format!("{variant_label} {arrow}")).size(11).color(s.text_primary),
        ]
        .align_y(Alignment::Center),
    )
    .on_press(Message::ToggleThemePopout)
    .style(|_theme, _status| button::Style {
        background: None,
        border: Border::default(),
        text_color: s.text_primary,
        ..Default::default()
    })
    .padding([4, 0])
    .width(Length::Fill);

    col_items.push(trigger.into());

    container(column(col_items).spacing(2))
        .padding([8, 12])
        .width(Length::Fill)
        .style(move |_theme| container::Style {
            background: Some(Background::Color(s.bg_sidebar)),
            border: Border { color: s.border, width: 1.0, radius: 0.0.into() },
            ..Default::default()
        })
        .into()
}

