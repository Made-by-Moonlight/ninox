//! Field Notes shared styling: fonts, hard offset shadows, cards, stamps.
//! Spec: docs/design-concepts/field-notes-design.md §2–3.
#![allow(dead_code)] // TODO(field-notes): remove once all views consume these helpers

use iced::font::{Family, Stretch, Style as FontStyle, Weight};
use iced::widget::{container, text, Space};
use iced::{Background, Border, Color, Element, Font, Length, Shadow, Vector};
use ninox_core::types::{CIStatus, SessionStatus};

use crate::theme::ColorScheme;

// ── Typography: three families, three jobs ─────────────────────────────────
pub const SERIF: Font = Font {
    family: Family::Name("Newsreader"),
    weight: Weight::Normal, stretch: Stretch::Normal, style: FontStyle::Normal,
};
pub const SERIF_MEDIUM: Font = Font { weight: Weight::Medium, ..SERIF };
pub const SERIF_ITALIC: Font = Font { style: FontStyle::Italic, ..SERIF };
pub const SERIF_MEDIUM_ITALIC: Font =
    Font { weight: Weight::Medium, style: FontStyle::Italic, ..SERIF };
pub const SANS: Font = Font {
    family: Family::Name("Archivo"),
    weight: Weight::Normal, stretch: Stretch::Normal, style: FontStyle::Normal,
};
pub const SANS_BOLD: Font = Font { weight: Weight::Bold, ..SANS };
pub const MONO: Font = Font {
    family: Family::Name("Spline Sans Mono"),
    weight: Weight::Normal, stretch: Stretch::Normal, style: FontStyle::Normal,
};
pub const MONO_MEDIUM: Font = Font { weight: Weight::Medium, ..MONO };

// ── Hard offset shadows: no blur, ever ─────────────────────────────────────
/// (card, hero, modal) shadow alphas for the active theme.
pub fn shadow_alpha(s: &ColorScheme) -> (f32, f32, f32) {
    // Explicit `dark` flag, not shadow-color sniffing — a theme file can tint
    // `shadow` without breaking mode detection.
    if s.dark { (0.50, 0.55, 0.65) } else { (0.12, 0.18, 0.30) }
}

pub fn hard_shadow(s: &ColorScheme, dx: f32, dy: f32, alpha: f32) -> Shadow {
    Shadow {
        color: Color { a: alpha, ..s.shadow },
        offset: Vector::new(dx, dy),
        blur_radius: 0.0,
    }
}

// ── Object styles ───────────────────────────────────────────────────────────
/// Card: 1px rule-dark border, radius 2, 2×3 offset shadow.
pub fn card_style(s: &ColorScheme) -> container::Style {
    let (card_a, _, _) = shadow_alpha(s);
    container::Style {
        background: Some(Background::Color(s.card)),
        border: Border { color: s.rule_dark, width: 1.0, radius: 2.0.into() },
        shadow: hard_shadow(s, 2.0, 3.0, card_a),
        ..Default::default()
    }
}

/// Hero object: 2px ink border, radius 3, 4×5 offset shadow.
pub fn heavy_frame(s: &ColorScheme) -> container::Style {
    let (_, hero_a, _) = shadow_alpha(s);
    container::Style {
        background: Some(Background::Color(s.card)),
        border: Border { color: s.ink, width: 2.0, radius: 3.0.into() },
        shadow: hard_shadow(s, 4.0, 5.0, hero_a),
        ..Default::default()
    }
}

// ── Rubber stamps ───────────────────────────────────────────────────────────
/// Stamps say a *word*, not the enum name (spec §3).
pub fn stamp_word(status: &SessionStatus) -> &'static str {
    use SessionStatus::*;
    match status {
        Spawning | Working => "Working",
        PrOpen             => "PR Open",
        CiFailed           => "Failed",
        ReviewPending      => "Awaiting",
        Mergeable          => "Ready",
        Done               => "Filed",
        Terminated         => "Closed",
    }
}

/// Status color for a CI run: any failure → ci_failed, otherwise pending
/// checks → review, otherwise all green → working.
pub fn ci_color(s: &ColorScheme, ci: &CIStatus) -> Color {
    if ci.failing > 0 {
        s.status_ci_failed
    } else if ci.pending > 0 {
        s.status_review
    } else {
        s.status_working
    }
}

/// Uppercase, 8.5px, bold, 1.5px border in the status color.
/// (iced can't rotate widgets — spec §8 accepts an unrotated stamp.)
pub fn stamp<'a, M: 'a>(word: &str, color: Color) -> Element<'a, M> {
    container(text(word.to_uppercase()).size(8.5).font(SANS_BOLD).color(color))
        .padding([2, 6])
        .style(move |_| container::Style {
            border: Border { color, width: 1.5, radius: 2.0.into() },
            ..Default::default()
        })
        .into()
}

// ── Micro-labels & rules ────────────────────────────────────────────────────
/// 9–10px, 700, uppercase (letter-spacing unsupported in iced).
pub fn micro_label<'a>(t: &str, color: Color) -> iced::widget::Text<'a> {
    text(t.to_uppercase()).size(9.5).font(SANS_BOLD).color(color)
}

/// Solid horizontal rule of the given color/thickness.
pub fn hline<'a, M: 'a>(color: Color, height: f32) -> Element<'a, M> {
    container(Space::new(Length::Fill, 0))
        .width(Length::Fill)
        .height(Length::Fixed(height))
        .style(move |_| container::Style {
            background: Some(Background::Color(color)),
            ..Default::default()
        })
        .into()
}

/// Vertical rule of the given color/thickness.
pub fn vline<'a, M: 'a>(color: Color, width: f32) -> Element<'a, M> {
    container(Space::new(0, 0))
        .width(Length::Fixed(width))
        .height(Length::Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(color)),
            ..Default::default()
        })
        .into()
}

/// Dotted rule for soft separations (card footers, comment threads).
pub fn dotted_rule<'a, M: 'a>(color: Color) -> Element<'a, M> {
    container(
        text("· ".repeat(160))
            .size(9)
            .color(color)
            .wrapping(iced::widget::text::Wrapping::None),
    )
    .width(Length::Fill)
    .height(Length::Fixed(8.0))
    .clip(true)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::types::SessionStatus;

    #[test]
    fn stamps_say_a_word_not_the_enum_name() {
        assert_eq!(stamp_word(&SessionStatus::Working),       "Working");
        assert_eq!(stamp_word(&SessionStatus::Spawning),      "Working");
        assert_eq!(stamp_word(&SessionStatus::PrOpen),        "PR Open");
        assert_eq!(stamp_word(&SessionStatus::CiFailed),      "Failed");
        assert_eq!(stamp_word(&SessionStatus::ReviewPending), "Awaiting");
        assert_eq!(stamp_word(&SessionStatus::Mergeable),     "Ready");
        assert_eq!(stamp_word(&SessionStatus::Done),          "Filed");
        assert_eq!(stamp_word(&SessionStatus::Terminated),    "Closed");
    }

    #[test]
    fn hard_shadows_never_blur() {
        let s = crate::theme::light();
        assert_eq!(hard_shadow(&s, 2.0, 3.0, 0.12).blur_radius, 0.0);
    }

    #[test]
    fn shadow_alpha_uses_explicit_dark_flag() {
        assert_eq!(shadow_alpha(&crate::theme::dark()), (0.50, 0.55, 0.65));
        assert_eq!(shadow_alpha(&crate::theme::light()), (0.12, 0.18, 0.30));

        // A theme file tinting dark's shadow color must not flip the
        // detected mode — `dark` is an explicit flag, not inferred from
        // shadow color.
        let mut tinted_dark = crate::theme::dark();
        let table: toml::Table = toml::from_str(r##"shadow = "#1a1208""##).unwrap();
        crate::theme::apply_palette(&mut tinted_dark, &table);
        assert_ne!(tinted_dark.shadow, Color::BLACK);
        assert_eq!(shadow_alpha(&tinted_dark), (0.50, 0.55, 0.65));
    }
}
