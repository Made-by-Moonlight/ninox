use iced::{
    widget::{button, column, container, row, text, text_input, Space},
    Alignment, Background, Border, Color, Element, Length,
};

use crate::{app::Message, theme::ColorScheme};

#[derive(Debug, Clone, Default)]
pub struct SpawnForm {
    pub name:      String,
    pub workspace: String,
}

pub fn spawn_modal<'a>(form: &'a SpawnForm, s: &'a ColorScheme) -> Element<'a, Message> {
    let can_submit = !form.name.trim().is_empty();

    let dialog = container(
        column![
            text("Spawn Orchestrator").size(16).color(s.text_primary),
            Space::new(0, 4),
            column![
                text("Name").size(11).color(s.text_muted),
                Space::new(0, 4),
                text_input("e.g. my-feature", &form.name)
                    .on_input(Message::SpawnFormName)
                    .on_submit_maybe(can_submit.then_some(Message::SpawnFormConfirm))
                    .padding(8)
                    .size(13),
            ]
            .spacing(0),
            Space::new(0, 4),
            row![
                button(text("Cancel").size(12).color(s.text_secondary))
                    .on_press(Message::SpawnFormCancel)
                    .style(move |_theme, _status| button::Style {
                        background: None,
                        text_color: s.text_secondary,
                        border: Border { color: s.border, width: 1.0, radius: 4.0.into() },
                        ..Default::default()
                    })
                    .padding([5, 12]),
                Space::new(Length::Fill, 0),
                button(
                    text("Spawn")
                        .size(12)
                        .color(if can_submit { Color::WHITE } else { s.text_muted }),
                )
                .on_press_maybe(can_submit.then_some(Message::SpawnFormConfirm))
                .style(move |_theme, _status| button::Style {
                    background: Some(Background::Color(
                        if can_submit { s.accent } else { s.bg_elevated },
                    )),
                    border: Border { color: s.border, width: 1.0, radius: 4.0.into() },
                    text_color: if can_submit { Color::WHITE } else { s.text_muted },
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
    .style(move |_| container::Style {
        background: Some(Background::Color(s.bg_surface)),
        border: Border { color: s.border, width: 1.0, radius: 8.0.into() },
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
