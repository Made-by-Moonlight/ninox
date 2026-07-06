//! Field Notes PR ledger — spec: docs/design-concepts/field-notes-design.md §5-III,
//! mockup `.ledger`/`.prt-row` in docs/design-concepts/03-field-notes.html.

use iced::{
    widget::{button, column, container, row, scrollable, text, Space},
    Alignment, Background, Border, Color, Element, Length,
};

use crate::{
    app::{App, Message},
    style::{hline, heavy_frame, micro_label, stamp, MONO, MONO_MEDIUM, SANS, SERIF, SERIF_ITALIC},
    theme::ColorScheme,
};
use ninox_core::types::{CIStatus, PR};

fn repo_short(repo: &str) -> &str {
    repo.rsplit('/').next().unwrap_or(repo)
}

/// CI stamp, wording adapted from the CI counts: fail → `Failed`,
/// partial → `Running x/y`, complete → `Passed x/y`. No run yet renders as
/// a plain faint em-dash rather than a stamp.
fn ci_badge<'a, M: 'a>(ci: Option<&CIStatus>, s: &ColorScheme) -> Element<'a, M> {
    match ci {
        None => text("—").size(11).color(s.faint).into(),
        Some(c) if c.failing > 0 => stamp("Failed", s.status_ci_failed),
        Some(c) if c.passing < c.total => {
            stamp(&format!("Running {}/{}", c.passing, c.total), s.status_review)
        }
        Some(c) => stamp(&format!("Passed {}/{}", c.passing, c.total), s.status_working),
    }
}

/// Small filled circle — session status indicator in the Session column.
fn status_dot(color: Color) -> Element<'static, Message> {
    container(Space::new(0, 0))
        .width(Length::Fixed(7.0))
        .height(Length::Fixed(7.0))
        .style(move |_| container::Style {
            background: Some(Background::Color(color)),
            border: Border { radius: 3.5.into(), ..Default::default() },
            ..Default::default()
        })
        .into()
}

fn pr_row<'a>(app: &'a App, pr: &'a PR) -> Element<'a, Message> {
    let s = &app.scheme;
    let ci = app.ci_status.get(&pr.id);
    let session = app.sessions.get(&pr.session_id);
    let session_name = session.map(|se| se.name.as_str()).unwrap_or("—");
    let session_color = session.map(|se| s.status_color(&se.status)).unwrap_or(s.faint);
    let session_repo = session.map(|se| repo_short(&se.repo)).unwrap_or("—");
    let cost = session
        .map(|se| format!("${:.2}", se.cost_usd))
        .unwrap_or_else(|| "—".to_string());
    let session_id = pr.session_id.clone();

    // Transparent-until-hover, shared by the navigate row and the open
    // button so the two halves read as one ledger line.
    let row_style = move |_t: &iced::Theme, status: button::Status| button::Style {
        // Normal state stays transparent so the ledger's card background
        // shows through; hover swaps to `paper` so it visibly differs.
        background: matches!(status, button::Status::Hovered)
            .then_some(Background::Color(s.paper)),
        text_color: s.ink,
        border: Border::default(),
        ..Default::default()
    };

    let nav = button(
        row![
            container(
                text(format!("#{}", pr.number)).size(12).font(MONO_MEDIUM).color(s.accent)
            )
            .width(Length::Fixed(70.0)),
            container(
                text(&pr.title)
                    .size(15)
                    .font(SERIF)
                    .color(s.ink)
                    .wrapping(iced::widget::text::Wrapping::None),
            )
            .width(Length::Fill)
            .clip(true),
            container(
                row![
                    status_dot(session_color),
                    Space::new(7, 0),
                    text(session_name).size(11.5).font(SANS).color(s.ink_2),
                ]
                .align_y(Alignment::Center),
            )
            .width(Length::Fixed(150.0)),
            container(text(session_repo).size(10).font(MONO).color(s.faint))
                .width(Length::Fixed(120.0)),
            container(ci_badge(ci, s)).width(Length::Fixed(130.0)),
            container(text(cost).size(11.5).font(MONO).color(s.ink))
                .width(Length::Fixed(70.0))
                .align_x(iced::alignment::Horizontal::Right),
        ]
        .align_y(Alignment::Center)
        .spacing(12)
        .padding([12, 18]),
    )
    .on_press(Message::NavigateSession(session_id))
    .width(Length::Fill)
    .style(row_style);

    // Sibling button (not nested — nested buttons fight over the click):
    // opens the PR in the browser instead of navigating in-app.
    let open = button(
        container(text("↗").size(13).font(MONO_MEDIUM).color(s.accent))
            .width(Length::Fill)
            .align_x(iced::alignment::Horizontal::Center),
    )
    .on_press(Message::OpenUrl(pr.url.clone()))
    .width(Length::Fixed(LINK_COL_WIDTH))
    .padding([12, 0])
    .style(row_style);

    row![nav, open].align_y(Alignment::Center).into()
}

/// Width of the trailing open-in-browser column — shared with the header so
/// the Cost column stays aligned.
const LINK_COL_WIDTH: f32 = 42.0;

pub fn pr_list(app: &App) -> Element<'_, Message> {
    use chrono::{Datelike, Local};
    let s = &app.scheme;

    // Sort PRs by number descending
    let mut prs: Vec<&PR> = app.prs.values().collect();
    prs.sort_by_key(|b| std::cmp::Reverse(b.number));

    let now = Local::now();
    let month = crate::style::MONTHS[now.month0() as usize];
    let ledger_label = format!("LEDGER — {} {} {}", now.day(), month, now.year());
    let open_count = prs.len();

    let folio = crate::components::folio::folio_scaffold(
        app,
        move || {
            let s = &app.scheme;
            row![
                text("Pull ").size(30).font(SERIF).color(s.ink),
                text("requests").size(30).font(SERIF_ITALIC).color(s.ink),
                Space::new(18, 0),
                text(ledger_label.clone())
                    .size(10.5)
                    .font(MONO)
                    .color(s.faint)
                    .wrapping(iced::widget::text::Wrapping::None),
            ]
            .align_y(Alignment::End)
            .into()
        },
        move || {
            let s = &app.scheme;
            vec![
                text(format!("{open_count} open"))
                    .size(10.5)
                    .font(MONO)
                    .color(s.ink_2)
                    .wrapping(iced::widget::text::Wrapping::None)
                    .into(),
            ]
        },
    );

    // Two-part structure mirrors `pr_row` (padded cells filling, then the
    // open-in-browser column) so the Cost header aligns with its values.
    let col_header = container(
        row![
            container(
                row![
                    container(micro_label("№", s.ink_2)).width(Length::Fixed(70.0)),
                    container(micro_label("Title", s.ink_2)).width(Length::Fill),
                    container(micro_label("Session", s.ink_2)).width(Length::Fixed(150.0)),
                    container(micro_label("Repo", s.ink_2)).width(Length::Fixed(120.0)),
                    container(micro_label("CI", s.ink_2)).width(Length::Fixed(130.0)),
                    container(micro_label("Cost", s.ink_2))
                        .width(Length::Fixed(70.0))
                        .align_x(iced::alignment::Horizontal::Right),
                ]
                .spacing(12)
                .padding([12, 18]),
            )
            .width(Length::Fill),
            Space::new(LINK_COL_WIDTH, 0),
        ],
    )
    .width(Length::Fill)
    .style(move |_| container::Style {
        background: Some(Background::Color(s.paper_2)),
        ..Default::default()
    });

    let rows: Vec<Element<Message>> = if prs.is_empty() {
        vec![
            container(
                text("No pull requests on file.")
                    .size(15)
                    .font(SERIF_ITALIC)
                    .color(s.faint),
            )
            .padding([40, 20])
            .width(Length::Fill)
            .into(),
        ]
    } else {
        let mut items: Vec<Element<Message>> = Vec::new();
        for (i, pr) in prs.iter().enumerate() {
            if i > 0 {
                items.push(hline(s.rule, 1.0));
            }
            items.push(pr_row(app, pr));
        }
        items
    };

    let table = container(column![
        col_header,
        hline(s.ink, 2.0),
        scrollable(column(rows)).height(Length::Fill),
    ])
    .width(Length::Fill)
    .height(Length::Fill)
    .style(move |_| heavy_frame(s));

    let table_wrapper = container(table)
        .padding([14, 28])
        .width(Length::Fill)
        .height(Length::Fill);

    column![folio, table_wrapper]
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}
