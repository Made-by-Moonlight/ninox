use iced::{
    widget::{button, column, container, row, scrollable, text, Space},
    Alignment, Background, Border, Color, Element, Length,
};

use crate::{app::{App, Message}, theme::ColorScheme};
use athene_core::types::{CIStatus, PR};

fn ci_badge<'a>(ci: Option<&CIStatus>, s: &'a ColorScheme) -> Element<'a, Message> {
    match ci {
        None => text("—").size(11).color(s.text_muted).into(),
        Some(c) if c.failing > 0 => {
            let label = format!("{}/{} CI", c.passing, c.total);
            container(text(label).size(10).color(Color::WHITE))
                .padding([2, 6])
                .style(move |_| container::Style {
                    background: Some(Background::Color(s.status_red)),
                    border: Border { radius: 3.0.into(), ..Default::default() },
                    ..Default::default()
                })
                .into()
        }
        Some(c) => {
            let label = format!("{}/{} CI", c.passing, c.total);
            container(text(label).size(10).color(Color::WHITE))
                .padding([2, 6])
                .style(move |_| container::Style {
                    background: Some(Background::Color(s.status_green)),
                    border: Border { radius: 3.0.into(), ..Default::default() },
                    ..Default::default()
                })
                .into()
        }
    }
}

fn pr_row<'a>(app: &'a App, pr: &'a PR) -> Element<'a, Message> {
    let s = &app.scheme;
    let ci = app.ci_status.get(&pr.id);
    let session_name = app.sessions.get(&pr.session_id)
        .map(|s| s.name.as_str())
        .unwrap_or("—");
    let session_id = pr.session_id.clone();

    button(
        row![
            // PR number
            container(
                text(format!("#{}", pr.number)).size(12).color(s.accent)
            )
            .width(Length::Fixed(56.0)),
            // PR title
            container(
                text(&pr.title).size(12).color(s.text_primary)
            )
            .width(Length::Fill),
            // Session name
            container(
                text(session_name).size(11).color(s.text_secondary)
            )
            .width(Length::Fixed(120.0)),
            // CI badge
            container(ci_badge(ci, s))
                .width(Length::Fixed(80.0))
                .align_x(iced::alignment::Horizontal::Right),
        ]
        .align_y(Alignment::Center)
        .spacing(12)
        .padding([8, 12]),
    )
    .on_press(Message::NavigateSession(session_id))
    .width(Length::Fill)
    .style(move |_t, status| button::Style {
        background: Some(Background::Color(match status {
            button::Status::Hovered => s.bg_elevated,
            _ => s.bg_surface,
        })),
        border: Border { color: s.border, width: 0.0, radius: 0.0.into() },
        ..Default::default()
    })
    .into()
}

pub fn pr_list(app: &App) -> Element<'_, Message> {
    let s = &app.scheme;

    // Sort PRs by number descending
    let mut prs: Vec<&PR> = app.prs.values().collect();
    prs.sort_by_key(|b| std::cmp::Reverse(b.number));

    let back_btn = button(
        text("← Fleet").size(12).color(s.text_secondary)
    )
    .on_press(Message::NavigateFleet { scope: None })
    .style(|_t, _s| button::Style {
        background: None,
        border: Border::default(),
        ..Default::default()
    })
    .padding([4, 0]);

    let header = container(
        row![
            back_btn,
            Space::new(16, 0),
            text("Pull Requests").size(16).color(s.text_primary),
            Space::new(Length::Fill, 0),
            text(format!("{} open", prs.len())).size(12).color(s.text_muted),
        ]
        .align_y(Alignment::Center)
    )
    .padding([12, 20])
    .width(Length::Fill)
    .style(move |_| container::Style {
        background: Some(Background::Color(s.bg_base)),
        border: Border { color: s.border, width: 0.0, radius: 0.0.into() },
        ..Default::default()
    });

    let col_header = container(
        row![
            container(text("#").size(10).color(s.text_muted))
                .width(Length::Fixed(56.0)),
            container(text("Title").size(10).color(s.text_muted))
                .width(Length::Fill),
            container(text("Session").size(10).color(s.text_muted))
                .width(Length::Fixed(120.0)),
            container(text("CI").size(10).color(s.text_muted))
                .width(Length::Fixed(80.0))
                .align_x(iced::alignment::Horizontal::Right),
        ]
        .spacing(12)
        .padding([6, 12])
    )
    .width(Length::Fill)
    .style(move |_| container::Style {
        background: Some(Background::Color(s.bg_elevated)),
        border: Border { color: s.border, width: 0.0, radius: 0.0.into() },
        ..Default::default()
    });

    let rows: Vec<Element<Message>> = if prs.is_empty() {
        vec![
            container(
                text("No pull requests yet.").size(13).color(s.text_muted),
            )
            .padding([40, 20])
            .width(Length::Fill)
            .into()
        ]
    } else {
        prs.iter()
            .flat_map(|pr| {
                let row_elem = pr_row(app, pr);
                let divider = container(Space::new(Length::Fill, 1.0))
                    .width(Length::Fill)
                    .style(move |_| container::Style {
                        background: Some(Background::Color(s.border)),
                        ..Default::default()
                    })
                    .into();
                vec![row_elem, divider]
            })
            .collect()
    };

    column![
        header,
        col_header,
        scrollable(column(rows)).height(Length::Fill),
    ]
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}
