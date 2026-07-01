use athene_core::types::NotificationKind;
use iced::{
    widget::{button, column, container, row, scrollable, text, Space},
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
    }
}

fn kind_color(kind: &NotificationKind, s: &ColorScheme) -> Color {
    match kind {
        NotificationKind::CiFailure        => s.status_red,
        NotificationKind::AgentStuck       => s.status_yellow,
        NotificationKind::PrNeedsAttention => s.status_blue,
        NotificationKind::MergeConflict    => s.status_red,
        NotificationKind::WorkerDone       => s.status_grey,
    }
}

pub fn notification_panel<'a>(app: &'a App) -> Element<'a, Message> {
    let s = &app.scheme;

    let header = row![
        text("Notifications").size(12).color(s.text_primary),
        Space::new(Length::Fill, 0),
        button(text("Clear all").size(10).color(s.text_muted))
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
                text("No notifications").size(12).color(s.text_muted),
            )
            .padding([12, 16])
            .into()
        ]
    } else {
        app.notifications.iter().map(|n| {
            let n_id = n.id.clone();
            let sess_id = n.session_id.clone();
            let label_color = kind_color(&n.kind, s);
            let row_content = column![
                row![
                    container(Space::new(0, 0))
                        .width(Length::Fixed(6.0))
                        .height(Length::Fixed(6.0))
                        .style(move |_| container::Style {
                            background: Some(Background::Color(label_color)),
                            border: Border { radius: 3.0.into(), ..Default::default() },
                            ..Default::default()
                        }),
                    Space::new(6, 0),
                    text(kind_label(&n.kind)).size(10).color(label_color),
                    Space::new(Length::Fill, 0),
                    button(text("×").size(12).color(s.text_muted))
                        .on_press(Message::DismissNotification(n_id))
                        .style(|_t, _s| button::Style {
                            background: None,
                            border: Border::default(),
                            ..Default::default()
                        })
                        .padding([0, 4]),
                ]
                .align_y(Alignment::Center),
                text(&n.title).size(12).color(s.text_primary),
                text(&n.body).size(11).color(s.text_secondary),
            ]
            .spacing(2);

            let mut btn = button(row_content)
                .style(move |_t, _s| button::Style {
                    background: Some(Background::Color(s.bg_elevated)),
                    border: Border { color: s.border, width: 1.0, radius: 4.0.into() },
                    ..Default::default()
                })
                .padding([8, 12]);

            if let Some(sid) = sess_id {
                btn = btn.on_press(Message::NavigateNotification(sid));
            }

            container(btn)
                .width(Length::Fill)
                .padding(Padding { top: 0.0, right: 8.0, bottom: 4.0, left: 8.0 })
                .into()
        }).collect()
    };

    container(
        column![
            header,
            scrollable(column(items).spacing(0)).height(Length::Fixed(300.0)),
        ]
    )
    .width(Length::Fill)
    .style(move |_| container::Style {
        background: Some(Background::Color(s.bg_surface)),
        border: Border { color: s.border, width: 1.0, radius: 6.0.into() },
        ..Default::default()
    })
    .into()
}
