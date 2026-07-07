//! Notification panel — journal-margin slips (spec §7): one `card_style`
//! slip per notification, a kind `stamp` mapped to the status tokens, a
//! 12px Archivo message, and a mono timestamp.

use ninox_core::types::NotificationKind;
use iced::{
    widget::{button, column, container, mouse_area, row, scrollable, text, Space},
    Alignment, Background, Border, Color, Element, Length, Padding,
};

use crate::{
    app::{App, Message},
    theme::ColorScheme,
};

fn kind_label(kind: &NotificationKind) -> &'static str {
    match kind {
        NotificationKind::CiFailure        => "CI",
        NotificationKind::AgentStuck       => "Stuck",
        NotificationKind::PrNeedsAttention => "PR",
        NotificationKind::MergeConflict    => "Conflict",
        NotificationKind::WorkerDone       => "Done",
        NotificationKind::WorkRequested    => "Work",
        NotificationKind::ExtraPr          => "Extra PR",
        NotificationKind::GithubLookupFailed => "GitHub",
    }
}

fn kind_color(kind: &NotificationKind, s: &ColorScheme) -> Color {
    match kind {
        NotificationKind::CiFailure        => s.status_ci_failed,
        NotificationKind::AgentStuck       => s.status_review,
        NotificationKind::PrNeedsAttention => s.status_pr_open,
        NotificationKind::MergeConflict    => s.status_ci_failed,
        NotificationKind::WorkerDone       => s.status_done,
        NotificationKind::WorkRequested    => s.status_pr_open,
        NotificationKind::ExtraPr          => s.status_review,
        NotificationKind::GithubLookupFailed => s.status_ci_failed,
    }
}

/// `created_at` (unix millis) as a local `HH:MM` mono timestamp.
fn format_timestamp(created_at_ms: i64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_millis_opt(created_at_ms)
        .single()
        .map(|dt| dt.format("%H:%M").to_string())
        .unwrap_or_default()
}

/// One journal-margin slip: `card_style` frame, a kind stamp/mono
/// timestamp/dismiss row up top, then 12px Archivo title/body below.
/// Pressing the slip (outside the × button) navigates to the
/// notification's session, if any.
fn notification_slip<'a>(app: &'a App, n: &'a ninox_core::types::Notification) -> Element<'a, Message> {
    let s = &app.scheme;
    let n_id = n.id.clone();
    let sess_id = n.session_id.clone();
    let stamp_color = kind_color(&n.kind, s);

    let content = column![
        row![
            crate::style::stamp(kind_label(&n.kind), stamp_color),
            Space::new(Length::Fill, 0),
            text(format_timestamp(n.created_at)).size(9.5).font(crate::style::MONO).color(s.faint),
            Space::new(8, 0),
            button(text("×").size(12).color(s.faint))
                .on_press(Message::DismissNotification(n_id))
                .style(|_t, _s| button::Style {
                    background: None,
                    border: Border::default(),
                    ..Default::default()
                })
                .padding([0, 4]),
        ]
        .align_y(Alignment::Center),
        Space::new(0, 4),
        text(&n.title).size(12).font(crate::style::SANS_BOLD).color(s.ink),
        text(&n.body).size(12).font(crate::style::SANS).color(s.ink_2),
    ]
    .spacing(2);

    let slip = container(content)
        .width(Length::Fill)
        .padding([8, 12])
        .style(move |_| crate::style::card_style(s));

    let slip: Element<Message> = if let Some(sid) = sess_id {
        mouse_area(slip).on_press(Message::NavigateNotification(sid)).into()
    } else {
        slip.into()
    };

    container(slip)
        .width(Length::Fill)
        .padding(Padding { top: 0.0, right: 8.0, bottom: 4.0, left: 8.0 })
        .into()
}

pub fn notification_panel<'a>(app: &'a App) -> Element<'a, Message> {
    let s = &app.scheme;

    let header = row![
        crate::style::micro_label("Notifications", s.ink_2),
        Space::new(Length::Fill, 0),
        button(crate::style::micro_label("Dismiss all", s.faint))
            .on_press(Message::DismissAllNotifications)
            .style(|_t, _s| button::Style {
                background: None,
                border: Border::default(),
                ..Default::default()
            })
            .padding([2, 4]),
    ]
    .align_y(Alignment::Center)
    .padding([8, 12]);

    let items: Vec<Element<Message>> = if app.notifications.is_empty() {
        vec![
            container(
                text("No notifications").size(12).font(crate::style::SERIF_ITALIC).color(s.faint),
            )
            .padding([12, 16])
            .into(),
        ]
    } else {
        app.notifications.iter().map(|n| notification_slip(app, n)).collect()
    };

    container(
        column![
            header,
            scrollable(column(items).spacing(0)).height(Length::Fixed(300.0)),
        ]
    )
    .width(Length::Fill)
    .style(move |_| container::Style {
        background: Some(Background::Color(s.card)),
        border: Border { color: s.rule_dark, width: 1.0, radius: 6.0.into() },
        ..Default::default()
    })
    .into()
}
