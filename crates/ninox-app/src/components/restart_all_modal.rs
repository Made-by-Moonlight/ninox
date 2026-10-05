//! Restart-all confirmation modal — a dimmed-backdrop overlay opened from
//! the Fleet card in Settings (`Message::RequestRestartAll`), mirroring
//! `catalogue_modal`'s header/body/footer shape. No form fields: restarting
//! every live agent at once is a single yes/no decision, not something
//! requiring a text field to get wrong.

use iced::{
    widget::{button, column, container, row, text, Space},
    Alignment, Background, Border, Color, Element, Length,
};

use crate::{
    app::{live_session_count, App, Message},
    style::{self, hard_shadow, hline, shadow_alpha, MONO, SANS_BOLD, SERIF, SERIF_ITALIC},
};

pub fn restart_all_modal(state: &App) -> Element<'_, Message> {
    let s = &state.scheme;
    let n = live_session_count(&state.sessions);
    let plural = if n == 1 { "" } else { "s" };

    let header = container(
        row![
            text("Restart all ").size(23).font(SERIF).color(s.ink),
            text("agents").size(23).font(SERIF_ITALIC).color(s.ink),
        ]
        .align_y(Alignment::Center),
    )
    .padding([18, 22])
    .width(Length::Fill)
    .style(move |_theme| container::Style {
        background: Some(Background::Color(s.paper_2)),
        ..Default::default()
    });

    let question = text(format!("Restart all {n} live agent{plural}?")).size(14).font(SERIF).color(s.ink);
    let detail = text(
        "Interrupts every running session at once so each picks up tooling-stack updates \
         (a newer ninox/harness build, MCP config, or reseeded skills). Conversations resume \
         where the harness supports it; sessions that can't resume restart fresh.",
    )
    .size(11)
    .font(MONO)
    .color(s.faint);

    let cancel_button = button(text("Cancel").size(11).font(SANS_BOLD).color(s.ink_2))
        .on_press(Message::CancelRestartAll)
        .padding([9, 18])
        .style(move |_theme, status| button::Style {
            background: None,
            text_color: s.ink_2,
            border: Border {
                color: if status == button::Status::Hovered { s.ink } else { s.rule_dark },
                width: 1.5,
                radius: 2.0.into(),
            },
            ..Default::default()
        });

    let confirm_button = button(
        row![
            text("RESTART ALL ").size(12).font(SANS_BOLD).color(s.card),
            text("⬡").size(12).font(style::GLYPH).color(s.card),
        ]
        .align_y(Alignment::Center),
    )
    .on_press(Message::ConfirmRestartAll)
    .padding([9, 20])
    .style(move |_theme, status| {
        let hovered = status == button::Status::Hovered;
        let offset = if hovered { 4.0 } else { 3.0 };
        let (card_a, _, _) = shadow_alpha(s);
        button::Style {
            background: Some(Background::Color(s.accent)),
            text_color: s.card,
            border: Border { color: s.rule_dark, width: 1.0, radius: 2.0.into() },
            shadow: hard_shadow(s, offset, offset, card_a),
        }
    });

    let footer = row![Space::new(Length::Fill, 0), cancel_button, confirm_button]
        .spacing(12)
        .align_y(Alignment::Center);

    let body = column![question, Space::new(0, 10), detail, Space::new(0, 22), footer]
        .padding([20, 24])
        .spacing(0);

    let modal = container(column![header, hline(s.ink, 2.0), body])
        .width(Length::Fixed(440.0))
        .style(move |_theme| {
            let mut frame = style::heavy_frame(s);
            let (_, _, modal_a) = shadow_alpha(s);
            frame.shadow = hard_shadow(s, 8.0, 10.0, modal_a);
            frame
        });

    let backdrop_alpha = if s.dark { 0.55 } else { 0.45 };
    container(modal)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .style(move |_theme| container::Style {
            background: Some(Background::Color(Color { a: backdrop_alpha, ..s.shadow })),
            ..Default::default()
        })
        .into()
}
