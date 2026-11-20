//! The context meter: a short bar of how full an orchestrator's context
//! window is, the reset threshold marked on it, tinted as it nears the
//! threshold. No v1 design artifact covers it, so it takes the PR chips'
//! look: a mono label, hairline border, the app's own status colours.

use iced::{
    widget::{container, row, stack, text, tooltip, Space},
    Alignment, Background, Border, Element, Length,
};
use ninox_ui_shared::context_meter::{ContextMeter, MeterTint};

use super::shell::tokens;
use crate::app::Message;

const BAR_HEIGHT: f32 = 6.0;

pub fn tint_color(tint: MeterTint) -> iced::Color {
    match tint {
        MeterTint::Normal => tokens::TEAL,
        MeterTint::Near => tokens::AMBER,
        MeterTint::Over => tokens::RED,
    }
}

/// `bar_px` wide bar plus its percentage, with the detail on hover.
pub fn context_meter<'a>(m: ContextMeter, bar_px: f32, text_size: f32) -> Element<'a, Message> {
    let color = tint_color(m.tint);
    let unknown = m.pct.is_none();

    let track = container(Space::new(Length::Fixed(bar_px), Length::Fixed(BAR_HEIGHT))).style(|_| container::Style {
        background: Some(Background::Color(tokens::PANEL2)),
        border: Border { color: tokens::BORDER, width: 1.0, radius: 3.0.into() },
        ..Default::default()
    });
    let fill = container(Space::new(Length::Fixed((bar_px * m.fill()).max(0.0)), Length::Fixed(BAR_HEIGHT))).style(move |_| {
        container::Style {
            background: Some(Background::Color(color)),
            border: Border { radius: 3.0.into(), ..Default::default() },
            ..Default::default()
        }
    });
    let mut layers: Vec<Element<'a, Message>> = vec![track.into(), fill.into()];
    if let Some(at) = m.threshold_at() {
        // A tick taller than the bar, so it reads as a mark, not a fill edge.
        let tick = container(Space::new(Length::Fixed(1.5), Length::Fixed(BAR_HEIGHT + 4.0)))
            .style(|_| container::Style { background: Some(Background::Color(tokens::TEXT)), ..Default::default() });
        layers.push(
            row![Space::new(Length::Fixed((bar_px * at - 0.75).max(0.0)), Length::Fixed(0.0)), tick]
                .align_y(Alignment::Center)
                .height(Length::Fixed(BAR_HEIGHT))
                .into(),
        );
    }
    let bar = stack(layers).width(Length::Fixed(bar_px)).height(Length::Fixed(BAR_HEIGHT));

    let label = text(m.label())
        .size(text_size)
        .font(crate::style::MONO)
        .color(if unknown { tokens::DIM2 } else if m.tint == MeterTint::Normal { tokens::DIM } else { color })
        .wrapping(iced::widget::text::Wrapping::None);
    let body = row![bar, label].spacing(6).align_y(Alignment::Center);

    tooltip(
        body,
        container(text(m.detail()).size(11).font(crate::style::SANS).color(tokens::DIM))
            .padding([8, 10])
            .style(|_| container::Style {
                background: Some(Background::Color(tokens::PANEL2)),
                border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
                ..Default::default()
            }),
        tooltip::Position::Bottom,
    )
    .gap(6)
    .into()
}

/// Approximate width of the meter, for the island head's room budget.
pub fn meter_px(bar_px: f32, text_size: f32) -> f32 {
    bar_px + 6.0 + text_size * 0.62 * 3.0
}
