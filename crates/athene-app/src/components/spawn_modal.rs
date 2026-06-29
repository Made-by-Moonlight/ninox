use iced::{
    widget::{button, column, container, row, text, text_input, Space},
    Alignment, Background, Border, Color, Element, Length,
};

use crate::{
    app::Message,
    theme::{ACCENT_AMBER, BG_ELEVATED, BG_SURFACE, BORDER, TEXT_MUTED, TEXT_PRIMARY, TEXT_SECONDARY},
};

#[derive(Debug, Clone, Default)]
pub struct SpawnForm {
    pub name:      String,
    pub workspace: String,
}

pub fn spawn_modal(form: &SpawnForm) -> Element<'_, Message> {
    let can_submit =
        !form.name.trim().is_empty() && !form.workspace.trim().is_empty();

    let dialog = container(
        column![
            text("Spawn Orchestrator").size(16).color(TEXT_PRIMARY),
            Space::new(0, 4),
            column![
                text("Name").size(11).color(TEXT_MUTED),
                Space::new(0, 4),
                text_input("e.g. my-feature", &form.name)
                    .on_input(Message::SpawnFormName)
                    .on_submit_maybe(can_submit.then_some(Message::SpawnFormConfirm))
                    .padding(8)
                    .size(13),
            ]
            .spacing(0),
            column![
                text("Workspace").size(11).color(TEXT_MUTED),
                Space::new(0, 4),
                text_input("~/projects/my-repo", &form.workspace)
                    .on_input(Message::SpawnFormWorkspace)
                    .on_submit_maybe(can_submit.then_some(Message::SpawnFormConfirm))
                    .padding(8)
                    .size(13),
            ]
            .spacing(0),
            Space::new(0, 4),
            row![
                button(text("Cancel").size(12).color(TEXT_SECONDARY))
                    .on_press(Message::SpawnFormCancel)
                    .style(|_theme, _status| button::Style {
                        background: None,
                        text_color: TEXT_SECONDARY,
                        border: Border { color: BORDER, width: 1.0, radius: 4.0.into() },
                        ..Default::default()
                    })
                    .padding([5, 12]),
                Space::new(Length::Fill, 0),
                button(
                    text("Spawn")
                        .size(12)
                        .color(if can_submit { Color::WHITE } else { TEXT_MUTED }),
                )
                .on_press_maybe(can_submit.then_some(Message::SpawnFormConfirm))
                .style(move |_theme, _status| button::Style {
                    background: Some(Background::Color(
                        if can_submit { ACCENT_AMBER } else { BG_ELEVATED },
                    )),
                    border: Border { color: BORDER, width: 1.0, radius: 4.0.into() },
                    text_color: if can_submit { Color::WHITE } else { TEXT_MUTED },
                    ..Default::default()
                })
                .padding([5, 16]),
            ]
            .align_y(Alignment::Center),
        ]
        .spacing(12)
        .padding(20),
    )
    .width(Length::Fixed(340.0))
    .style(|_| container::Style {
        background: Some(Background::Color(BG_SURFACE)),
        border: Border { color: BORDER, width: 1.0, radius: 8.0.into() },
        ..Default::default()
    });

    container(dialog)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .style(|_| container::Style {
            background: Some(Background::Color(Color::from_rgba(0.0, 0.0, 0.0, 0.6))),
            ..Default::default()
        })
        .into()
}
