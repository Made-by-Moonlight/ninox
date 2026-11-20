//! Notifications popover (`Notifications.dc.html`): a 400px `PANEL` card
//! anchored under the bell — header with count and "Dismiss all", then one
//! row per notification (tone dot, kind badge, session name, mono time, ×,
//! bold title, dim body, optional primary action) and an empty state.

use ninox_core::types::NotificationKind;
use iced::{
    widget::{button, column, container, mouse_area, row, scrollable, text, Space},
    Alignment, Background, Border, Color, Element, Length, Padding,
};

use super::{
    fleet_focus::badge,
    shell::tokens,
    spawn_modal::{CARD_SELECTED_BG, CARD_SELECTED_BORDER},
};
use crate::app::{App, Message};

const RED_BADGE_BG: Color = tokens::hex(0x3a1822);
const RED_BADGE_FG: Color = tokens::hex(0xf7a1b2);
const POPOVER_BORDER: Color = tokens::hex(0x2f3750);
const PRIMARY_TEXT: Color = tokens::hex(0xbff3ec);
const GHOST_TEXT: Color = tokens::hex(0xa3aabb);

/// The leading dot's colour, by tone.
fn kind_dot_color(kind: &NotificationKind) -> Color {
    use ninox_overview::notifications::{kind_tone, Tone};
    match kind_tone(kind) {
        Tone::Failed    => tokens::RED,
        Tone::Attention => tokens::AMBER,
        Tone::Open      => tokens::BLUE,
        Tone::Done      => tokens::DIM2,
    }
}

/// (background, foreground) of a kind's badge, by tone.
fn kind_badge_colors(kind: &NotificationKind) -> (Color, Color) {
    use ninox_overview::notifications::{kind_tone, Tone};
    match kind_tone(kind) {
        Tone::Failed    => (RED_BADGE_BG, RED_BADGE_FG),
        Tone::Attention => (tokens::AMBER_BADGE_BG, tokens::AMBER_BADGE_FG),
        Tone::Open      => (tokens::BLUE_BADGE_BG, tokens::BLUE),
        Tone::Done      => (tokens::PANEL2, tokens::DIM),
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

/// The popover's small primary button (`Notifications.dc.html`'s
/// `btn-sm btn-primary`); `message: None` renders it disabled.
fn action_button<'a>(label: &'a str, message: Option<Message>) -> Element<'a, Message> {
    let enabled = message.is_some();
    button(text(label).size(11).font(crate::style::SANS_SEMIBOLD))
        .on_press_maybe(message)
        .padding([3, 9])
        .style(move |_theme, _status| button::Style {
            background: Some(Background::Color(if enabled { CARD_SELECTED_BG } else { tokens::PANEL2 })),
            text_color: if enabled { PRIMARY_TEXT } else { tokens::DIM2 },
            border: Border {
                color: if enabled { CARD_SELECTED_BORDER } else { tokens::BORDER },
                width: 1.0,
                radius: 5.0.into(),
            },
            ..Default::default()
        })
        .into()
}

/// The action row for kinds that carry a follow-up action — `None` for
/// everything else. `UpdateAvailable` triggers the `cargo install` subprocess
/// (disabled/relabeled mid-flight via `app.update_in_progress`);
/// `UpdateInstalled` prompts a restart to pick up the new binary;
/// `UpdateFailed` offers an immediate retry — the periodic background check
/// (`poller::poll_update_check`) already recorded this version as notified
/// before the install was attempted, so without this button a failed
/// install wouldn't get another automatic reminder for the same version.
fn notification_action<'a>(app: &'a App, n: &ninox_core::types::Notification) -> Option<Element<'a, Message>> {
    match &n.kind {
        NotificationKind::UpdateAvailable => Some(if app.update_in_progress {
            action_button("Updating…", None)
        } else {
            action_button("Update now", Some(Message::ApplyUpdate))
        }),
        NotificationKind::UpdateInstalled => Some(action_button(
            "Restart now", Some(Message::RestartApp),
        )),
        NotificationKind::UpdateFailed => Some(if app.update_in_progress {
            action_button("Retrying…", None)
        } else {
            action_button("Retry", Some(Message::ApplyUpdate))
        }),
        NotificationKind::FleetRestorePending => Some(action_button(
            "Restore…", Some(Message::RestoreFleetOpen),
        )),
        NotificationKind::DiskPressure => Some(action_button(
            "Review workspaces", Some(Message::WorkspacesModalOpen),
        )),
        NotificationKind::ResetPlanReady => Some(action_button(
            "Review plan…", n.session_id.clone().map(Message::ResetFromPlanOpenReady),
        )),
        _ => None,
    }
}

/// One notification row: tone dot, then a badge/session/timestamp/dismiss
/// line, the bold title, the dim body and (for kinds that have one) a primary
/// action. Pressing the row (outside the × button and any action button)
/// navigates to the notification's session, if any.
fn notification_row<'a>(app: &'a App, n: &'a ninox_core::types::Notification) -> Element<'a, Message> {
    let n_id = n.id.clone();
    let (badge_bg, badge_fg) = kind_badge_colors(&n.kind);

    let mut top = row![badge(ninox_overview::notifications::kind_label(&n.kind).to_uppercase(), badge_bg, badge_fg)]
        .spacing(8)
        .align_y(Alignment::Center);
    if let Some(name) = n.session_id.as_ref().and_then(|id| app.sessions.get(id)).map(|s| s.name.clone()) {
        let name = if name.chars().count() > 28 { format!("{}…", name.chars().take(27).collect::<String>()) } else { name };
        top = top.push(text(name).size(11).font(crate::style::MONO).color(tokens::DIM).wrapping(iced::widget::text::Wrapping::None));
    }
    let top = top
        .push(Space::new(Length::Fill, 0))
        .push(text(format_timestamp(n.created_at)).size(11).font(crate::style::MONO).color(tokens::DIM2))
        .push(
            button(text("×").size(12).color(GHOST_TEXT))
                .on_press(Message::DismissNotification(n_id))
                .style(|_t, _s| button::Style { background: None, border: Border::default(), ..Default::default() })
                .padding([0, 4]),
        );

    let mut content = column![
        top,
        Space::new(0, 6),
        text(&n.title).size(12).font(crate::style::SANS_BOLD).color(tokens::TEXT),
        Space::new(0, 2),
        text(&n.body).size(12).font(crate::style::SANS).color(tokens::DIM),
    ];
    if let Some(action) = notification_action(app, n) {
        content = content.push(Space::new(0, 6)).push(action);
    }

    let dot = container(Space::new(7, 7)).style({
        let color = kind_dot_color(&n.kind);
        move |_| container::Style {
            background: Some(Background::Color(color)),
            border: Border { radius: 3.5.into(), ..Default::default() },
            ..Default::default()
        }
    });
    let body = row![container(dot).padding(Padding { top: 6.0, ..Padding::ZERO }), content.width(Length::Fill)].spacing(12);

    let framed = container(column![
        container(body).width(Length::Fill).padding([12, 16]),
        crate::style::hline(tokens::BORDER, 1.0),
    ]);
    match n.session_id.clone() {
        Some(sid) => mouse_area(framed).on_press(Message::NavigateNotification(sid)).into(),
        None => framed.into(),
    }
}

fn empty_state<'a>() -> Element<'a, Message> {
    container(
        column![
            text("No notifications").size(13).font(crate::style::SANS_SEMIBOLD).color(tokens::hex(0xc8ccd6)),
            text("CI failures, reviews, idle workers and updates show up here.").size(12).font(crate::style::SANS).color(tokens::DIM),
        ]
        .spacing(4)
        .align_x(Alignment::Center),
    )
    .width(Length::Fill)
    .center_x(Length::Fill)
    .padding([34, 16])
    .into()
}

pub fn notification_panel<'a>(app: &'a App) -> Element<'a, Message> {
    let header = container(
        row![
            crate::style::micro_label("Notifications", tokens::DIM2),
            text(app.notifications.len().to_string()).size(11.5).font(crate::style::SANS).color(tokens::DIM),
            Space::new(Length::Fill, 0),
            button(text("Dismiss all").size(11).font(crate::style::SANS_SEMIBOLD).color(GHOST_TEXT))
                .on_press(Message::DismissAllNotifications)
                .style(|_t, status| button::Style {
                    background: matches!(status, button::Status::Hovered).then_some(Background::Color(tokens::PANEL2)),
                    border: Border { radius: 5.0.into(), ..Default::default() },
                    ..Default::default()
                })
                .padding([3, 9]),
        ]
        .spacing(10)
        .align_y(Alignment::Center),
    )
    .padding([12, 16]);

    let list: Element<Message> = if app.notifications.is_empty() {
        empty_state()
    } else {
        scrollable(column(app.notifications.iter().map(|n| notification_row(app, n)).collect::<Vec<_>>()))
            .height(Length::Shrink)
            .into()
    };

    container(column![header, crate::style::hline(tokens::BORDER, 1.0), container(list).max_height(620.0)])
        .width(Length::Fill)
        .style(|_| container::Style {
            background: Some(Background::Color(tokens::PANEL)),
            border: Border { color: POPOVER_BORDER, width: 1.0, radius: 10.0.into() },
            shadow: iced::Shadow { color: Color { a: 0.55, ..Color::BLACK }, offset: iced::Vector::new(0.0, 18.0), blur_radius: 48.0 },
            ..Default::default()
        })
        .into()
}
