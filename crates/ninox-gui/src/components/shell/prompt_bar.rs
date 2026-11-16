//! The "ninox prompt" — a persistent natural-language input docked under
//! the graph canvas, always visible per the fleet-overview-ui-vision spec.
//! Query/action routing (read queries, spawn requests, plan generation) is
//! a separate, much larger feature and out of scope here: this PR renders
//! a real input field so the layout is honest about what's "always on",
//! but submitting just clears the draft.
//!
//! G1 (round-1 try-out feedback, "the prompt is not very obvious") replaces
//! the original single dim mono line with a real bordered field: the label
//! and keycap hints are plain `text` overlaid on the `text_input` via
//! `stack!`, since `text_input` is a leaf widget and can't host styled
//! children of its own. The `text_input`'s own left/right padding reserves
//! room for that overlay so typed text and the placeholder never run under
//! it.

use iced::{
    widget::{container, pick_list, row, text, text_input},
    Alignment, Background, Border, Color, Element, Length, Padding,
};

use super::tokens;
use crate::app::{App, Message};

/// Left padding on the field's `text_input`, reserving room for the
/// overlaid `❯ Ask ninox → ‹target› ▾` prefix (`PREFIX_WIDTH` plus its own
/// insets, plus the target chip's own fixed width).
const LEFT_PAD: f32 = 230.0;
/// Right padding on the field's `text_input`, reserving room for the
/// overlaid `↑↓ history · / focus · ⏎ send` keycap chips.
const RIGHT_PAD: f32 = 250.0;
const OUTER_PAD: f32 = 12.0;
/// Left/right inset: the same 18px the Fleet and Brain toolbars use, so the
/// field's edges line up with the content above it in either rail state.
const SIDE_INSET: f32 = 18.0;
const FIELD_PAD_V: f32 = 10.0;
const FIELD_TEXT_SIZE: f32 = 12.5;

/// Laid-out height of the bar: iced lines are 1.3× the text size.
pub(crate) const BAR_HEIGHT: f32 = 2.0 * OUTER_PAD + 2.0 * FIELD_PAD_V + FIELD_TEXT_SIZE * 1.3;

/// Stable id for the field's `text_input` — `/` focuses it via
/// `iced::widget::text_input::focus` (see `app.rs`'s `RawKey` handling).
/// `Cmd+K` is reserved for the top-bar search (G3), not this field.
pub fn prompt_input_id() -> text_input::Id {
    text_input::Id::new("ninox-prompt")
}

/// `text_input::Style` has no shadow field (unlike `container::Style`), so
/// the spec's "subtle teal outer glow" is approximated with a thicker teal
/// border rather than a true glow.
fn field_style(status: text_input::Status) -> text_input::Style {
    let focused = status == text_input::Status::Focused;
    text_input::Style {
        background: Background::Color(tokens::BG),
        border: Border {
            color: if focused { tokens::TEAL } else { tokens::BORDER },
            width: if focused { 2.0 } else { 1.0 },
            radius: 8.0.into(),
        },
        icon: tokens::DIM,
        placeholder: tokens::PLACEHOLDER,
        value: tokens::TEXT,
        selection: tokens::VIOLET,
    }
}

/// `key`: the keycap glyph, rendered in `key_font` — neither `style::NERD`
/// (symbols-only, no ASCII) nor `style::GLYPH`/`MONO` alone covers both `/`
/// and `⏎`, so the caller picks the one that actually has the glyph:
/// `MONO` for plain-ASCII `/`, `GLYPH` for `⏎` (round-1 feedback: both
/// rendered as tofu before this).
/// `label`: the small action word after the pill, e.g. "focus"/"send".
/// The `→ ‹target› ▾` chip's closed/hovered/open look (G2). `▾` is
/// `pick_list`'s own built-in handle, drawn in `handle_color`.
fn target_chip_style(status: pick_list::Status) -> pick_list::Style {
    let active = matches!(status, pick_list::Status::Hovered | pick_list::Status::Opened);
    pick_list::Style {
        text_color: tokens::TEXT,
        placeholder_color: tokens::DIM,
        handle_color: tokens::VIOLET,
        background: Background::Color(if active { tokens::PANEL2 } else { Color::TRANSPARENT }),
        border: Border { color: tokens::BORDER, width: 1.0, radius: 10.0.into() },
    }
}

/// G2: who the prompt's text goes to, and the picker to change it —
/// "ninox" (the prompt agent) plus every known orchestrator by name.
/// `Message::NinoxTargetPicked` resolves the picked label back to a
/// `PromptTarget`.
fn target_chip<'a>(app: &'a App) -> Element<'a, Message> {
    let mut options: Vec<String> = vec!["ninox".to_string()];
    options.extend(app.orchestrators.iter().map(|o| o.name.clone()));
    let selected = Some(app.ninox_prompt_target.label().to_string());

    row![
        // "→" through `glyph_font_for`: the mono fonts don't all have it.
        text("→").size(11.5).font(crate::style::glyph_font_for("→")).color(tokens::DIM),
        iced::widget::Space::new(4, 0),
        pick_list(options, selected, Message::NinoxTargetPicked)
            .text_size(11.5)
            .font(crate::style::MONO)
            .padding(Padding { top: 3.0, right: 6.0, bottom: 3.0, left: 8.0 })
            .width(Length::Fixed(96.0))
            .style(|_t, status| target_chip_style(status)),
    ]
    .align_y(Alignment::Center)
    .into()
}

fn keycap<'a>(key: &'a str, key_font: iced::Font, label: &'a str) -> Element<'a, Message> {
    row![
        container(text(key).size(10).font(key_font).color(tokens::DIM))
            .padding(Padding { top: 2.0, right: 6.0, bottom: 2.0, left: 6.0 })
            .style(|_| container::Style {
                background: Some(Background::Color(tokens::PANEL2)),
                border: Border { color: tokens::BORDER, width: 1.0, radius: 4.0.into() },
                ..Default::default()
            }),
        iced::widget::Space::new(4, 0),
        text(label).size(10).font(crate::style::MONO).color(tokens::DIM),
    ]
    .align_y(Alignment::Center)
    .into()
}

/// `scope`: the opened island's name, when the Fleet tab has one open
/// (MLOPS-5339 slice 3) — narrows the placeholder to "about `<orchestrator>`
/// — …", matching `FleetFocused.dc.html`. `None` everywhere else. Neither
/// variant repeats "ask ninox"/"Ask ninox" — the `❯ Ask ninox` prefix
/// already says that (round-1 feedback: the two were echoing each other).
pub fn prompt_bar<'a>(app: &'a App, scope: Option<&str>) -> Element<'a, Message> {
    let placeholder = match scope {
        Some(name) => format!("about {name} — \"why is it paused?\""),
        None => "\"what's blocking the webhook rewrite?\" or \"spawn an orchestrator…\"".to_string(),
    };

    let field = text_input(&placeholder, &app.ninox_prompt_draft)
        .id(prompt_input_id())
        .width(Length::Fill)
        .size(FIELD_TEXT_SIZE)
        .font(crate::style::SANS)
        .padding(Padding { top: FIELD_PAD_V, right: RIGHT_PAD, bottom: FIELD_PAD_V, left: LEFT_PAD })
        .on_input(Message::NinoxPromptChanged)
        .on_submit(Message::NinoxPromptSubmitted)
        .style(|_t, status| field_style(status));

    let prefix = container(
        row![
            text("❯").size(14).font(crate::style::NERD).color(tokens::VIOLET),
            iced::widget::Space::new(8, 0),
            text("Ask ninox").size(12.5).font(crate::style::MONO_MEDIUM).color(tokens::TEXT),
            iced::widget::Space::new(10, 0),
            target_chip(app),
        ]
        .align_y(Alignment::Center),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .align_y(Alignment::Center)
    .padding(Padding { top: 0.0, right: 0.0, bottom: 0.0, left: 12.0 });

    let chips = container(
        row![
            keycap("↑↓", crate::style::glyph_font_for("↑"), "history"),
            iced::widget::Space::new(10, 0),
            text("·").size(10).color(tokens::DIM2),
            iced::widget::Space::new(10, 0),
            keycap("/", crate::style::MONO, "focus"),
            iced::widget::Space::new(10, 0),
            text("·").size(10).color(tokens::DIM2),
            iced::widget::Space::new(10, 0),
            keycap("⏎", crate::style::GLYPH, "send"),
        ]
        .align_y(Alignment::Center),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .align_x(Alignment::End)
    .align_y(Alignment::Center)
    .padding(Padding { top: 0.0, right: 10.0, bottom: 0.0, left: 0.0 });

    container(iced::widget::stack![field, prefix, chips])
        .width(Length::Fill)
        .padding(Padding { top: OUTER_PAD, right: SIDE_INSET, bottom: OUTER_PAD, left: SIDE_INSET })
        .style(|_| container::Style {
            background: Some(Background::Color(tokens::PANEL)),
            ..Default::default()
        })
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_is_bordered_and_turns_teal_with_a_wider_ring_when_focused() {
        let idle = field_style(text_input::Status::Active);
        assert_eq!(idle.border.color, tokens::BORDER);
        assert_eq!(idle.border.width, 1.0);
        assert_eq!(idle.border.radius, 8.0.into());
        assert_eq!(idle.background, Background::Color(tokens::BG));
        assert_eq!(idle.placeholder, tokens::PLACEHOLDER);

        let focused = field_style(text_input::Status::Focused);
        assert_eq!(focused.border.color, tokens::TEAL);
        assert_eq!(focused.border.width, 2.0);
    }

    #[test]
    fn hovered_and_disabled_keep_the_idle_border() {
        // Only an actual focus should read as "active" — hovering (or a
        // disabled field, not currently used here but part of the Status
        // enum) must not be mistaken for it.
        for status in [text_input::Status::Hovered, text_input::Status::Disabled] {
            assert_eq!(field_style(status).border.color, tokens::BORDER);
        }
    }
}
