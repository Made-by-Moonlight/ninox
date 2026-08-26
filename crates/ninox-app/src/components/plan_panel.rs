//! The orchestrator "Plan" panel — a live, rendered, selectable view of an
//! orchestrator's registered goals/plan markdown doc (`ninox plan
//! register`). Spec: docs/superpowers/specs/2026-08-26-orchestrator-plan-
//! tracking-design.md.

use iced::widget::{button, column, container, row, scrollable, text, Space};
use iced::{Background, Border, Element, Length};

use crate::app::{App, Message};
use crate::components::selectable_markdown::selectable_markdown;
use crate::theme::ColorScheme;

/// `updated_at` (unix millis) as a local `HH:MM` mono timestamp, matching
/// the notification panel's convention.
fn format_timestamp(updated_at_ms: i64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_millis_opt(updated_at_ms)
        .single()
        .map(|dt| dt.format("%H:%M").to_string())
        .unwrap_or_default()
}

pub fn plan_pane<'a>(app: &'a App, orchestrator_id: &str, s: &'a ColorScheme) -> Element<'a, Message> {
    let doc = app.plan_docs.get(orchestrator_id);
    let registration = app.engine.store.get_orchestrator_plan(orchestrator_id).ok().flatten();

    let header: Element<Message> = match (&registration, doc.and_then(|d| d.error.as_ref())) {
        (None, _) => Space::new(0, 0).into(),
        (Some(reg), error) => {
            let mut items = vec![
                text(reg.file_path.clone())
                    .size(12)
                    .font(crate::style::MONO)
                    .color(s.ink_2)
                    .into(),
                Space::new(10, 0).into(),
                button(text("Open").size(12).color(s.accent))
                    .on_press(Message::OpenUrl(reg.file_path.clone()))
                    .style(|_theme, _status| button::Style {
                        background: None,
                        border: Border::default(),
                        ..Default::default()
                    })
                    .padding(0)
                    .into(),
                Space::new(Length::Fill, 0).into(),
                text(format!("updated {}", format_timestamp(reg.updated_at)))
                    .size(11)
                    .font(crate::style::MONO)
                    .color(s.faint)
                    .into(),
            ];
            if let Some(error) = error {
                items.insert(
                    1,
                    text(format!(" — unreadable: {error}"))
                        .size(11)
                        .color(s.status_ci_failed)
                        .into(),
                );
            }
            row(items).align_y(iced::Alignment::Center).into()
        }
    };

    let body: Element<Message> = match (&registration, doc) {
        (None, _) => container(
            column![
                text("No plan doc registered").size(14).font(crate::style::SERIF_ITALIC).color(s.faint),
                Space::new(0, 6),
                text("The orchestrator can run `ninox plan register <file>`.")
                    .size(12)
                    .font(crate::style::MONO)
                    .color(s.faint),
            ]
            .align_x(iced::Alignment::Center),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .into(),
        (Some(_), Some(doc)) if doc.error.is_some() => container(
            text("Registered plan doc is missing or unreadable").size(13).color(s.faint),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .into(),
        (Some(_), Some(doc)) => scrollable(
            container(selectable_markdown(orchestrator_id.to_string(), &doc.content, s))
                .width(Length::Fill)
                .padding(16),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .into(),
        (Some(_), None) => container(text("Loading plan…").size(13).color(s.faint))
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into(),
    };

    container(
        column![
            container(header).padding([10, 16]).width(Length::Fill).style(move |_theme| {
                iced::widget::container::Style {
                    background: Some(Background::Color(s.paper_2)),
                    border: Border { color: s.rule, width: 1.0, radius: 0.0.into() },
                    ..Default::default()
                }
            }),
            body,
        ]
        .width(Length::Fill)
        .height(Length::Fill),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .style(move |_theme| iced::widget::container::Style {
        background: Some(Background::Color(s.paper)),
        ..Default::default()
    })
    .into()
}
