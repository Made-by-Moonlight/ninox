//! Field Notes shared styling: fonts, hard offset shadows, cards, stamps.
//! Spec: docs/design-concepts/field-notes-design.md §2–3.

use iced::font::{Family, Stretch, Style as FontStyle, Weight};
use iced::widget::{button, container, row, text, text_input, Space};
use iced::{Alignment, Background, Border, Color, Element, Font, Length, Shadow, Vector};
use ninox_core::types::{CIStatus, SessionStatus};

use crate::theme::ColorScheme;

/// Full month names, indexed by `chrono`'s `month0()` (0 = January) —
/// shared by fleet_board's and pr_list's folio date labels.
pub const MONTHS: [&str; 12] = [
    "JANUARY", "FEBRUARY", "MARCH", "APRIL", "MAY", "JUNE", "JULY",
    "AUGUST", "SEPTEMBER", "OCTOBER", "NOVEMBER", "DECEMBER",
];

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

/// Font for the app's small set of dingbat glyphs (`⬡ ⌕ ✦ ☰ ⚑`) — none of
/// which are covered by the bundled Newsreader/Archivo/Spline Sans Mono
/// families and render as fallback tofu boxes without this. macOS ships
/// these in Apple Symbols; other platforms fall back to the system default
/// font (best-effort — not verified to cover every glyph there).
#[cfg(target_os = "macos")]
pub const GLYPH: Font = Font::with_name("Apple Symbols");
#[cfg(not(target_os = "macos"))]
pub const GLYPH: Font = Font::DEFAULT;

/// Alternate glyph font for the FEW code points Apple Symbols lacks:
/// verified via CoreText, ✦ (U+2726) exists in Menlo but not Apple
/// Symbols, while ⬡/☰ exist only in Apple Symbols — so glyph sites pick
/// per character. Non-macOS: same unverified-default fallback as GLYPH
/// (tracked in issue #11).
#[cfg(target_os = "macos")]
pub const GLYPH_ALT: Font = Font::with_name("Menlo");
#[cfg(not(target_os = "macos"))]
pub const GLYPH_ALT: Font = Font::DEFAULT;

/// The right glyph font for one of the app's permitted glyphs — per-character,
/// because no single macOS font covers all of them (see GLYPH_ALT).
pub fn glyph_font_for(glyph: &str) -> Font {
    match glyph.chars().next() {
        Some('✦') => GLYPH_ALT,
        _ => GLYPH,
    }
}

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

// ── Segmented toggles ───────────────────────────────────────────────────────
/// `button::Style` for one item of a segmented toggle: `ink`-filled
/// background + `card` text when `active`, transparent otherwise.
/// `inactive_text`/`hover_text` cover per-site hover feedback (brain_panel's
/// mode toggle turns `ink`-colored on hover; others stay fixed), and
/// `border_active`/`border_inactive` cover every border treatment used
/// across the app: an always-on rectangle (fleet_board's scope tabs), a
/// state-colored pill (spawn_modal's agent/catalogue chips), or no border at
/// all (`Border::default()`, for segments living inside a `segmented_frame`).
/// Shared by fleet_board's `tab_chip`, spawn_modal's `chip`, and
/// `toggle_segment` below.
pub fn segment_style<'a>(
    s: &'a ColorScheme,
    active: bool,
    inactive_text: Color,
    hover_text: Option<Color>,
    border_active: Border,
    border_inactive: Border,
) -> impl Fn(&iced::Theme, button::Status) -> button::Style + 'a {
    move |_theme, status| {
        let hovered = matches!(status, button::Status::Hovered);
        button::Style {
            background: Some(Background::Color(if active { s.ink } else { Color::TRANSPARENT })),
            text_color: if active {
                s.card
            } else if hovered {
                hover_text.unwrap_or(inactive_text)
            } else {
                inactive_text
            },
            border: if active { border_active } else { border_inactive },
            ..Default::default()
        }
    }
}

/// One item of a borderless segmented toggle meant to sit inside
/// `segmented_frame` (mockup `.bm`): micro-label typography, `ink`-filled +
/// `card` text when active, `ink`-colored text on hover otherwise. Shared by
/// brain_panel's Pinboard/Catalogue mode toggle and spawn_modal's Entry-type
/// toggle.
pub fn toggle_segment<'a, M: Clone + 'a>(s: &'a ColorScheme, label: &str, active: bool, on_press: M) -> Element<'a, M> {
    button(micro_label(label, if active { s.card } else { s.ink_2 }))
        .on_press(on_press)
        .padding([5, 14])
        .style(segment_style(s, active, s.ink_2, Some(s.ink), Border::default(), Border::default()))
        .into()
}

/// Same as `toggle_segment`, for labels that lead with one of the app's
/// dingbat glyphs (e.g. `"✦ Pinboard"`). Newsreader/Archivo don't cover
/// `⬡ ⌕ ✦ ☰ ⚑`, so the glyph renders separately in `GLYPH` while the rest of
/// the label keeps the normal micro-label (Archivo) treatment.
pub fn toggle_segment_glyph<'a, M: Clone + 'a>(
    s: &'a ColorScheme,
    glyph: &str,
    label: &str,
    active: bool,
    on_press: M,
) -> Element<'a, M> {
    let color = if active { s.card } else { s.ink_2 };
    button(
        row![
            text(glyph.to_string()).size(9.5).font(glyph_font_for(glyph)).color(color),
            Space::new(5, 0),
            micro_label(label, color),
        ]
        .align_y(Alignment::Center),
    )
    .on_press(on_press)
    .padding([5, 14])
    .style(segment_style(s, active, s.ink_2, Some(s.ink), Border::default(), Border::default()))
    .into()
}

/// The bordered, hard-shadowed frame around a `toggle_segment` row, with an
/// `ink` `vline` between each segment (mockup `.brain-mode`). Shared by
/// brain_panel's mode toggle and spawn_modal's Entry-type toggle.
pub fn segmented_frame<'a, M: 'a>(s: &'a ColorScheme, segments: Vec<Element<'a, M>>) -> Element<'a, M> {
    let (card_a, _, _) = shadow_alpha(s);
    let mut children: Vec<Element<M>> = Vec::with_capacity(segments.len() * 2);
    for (i, seg) in segments.into_iter().enumerate() {
        if i > 0 {
            children.push(vline(s.ink, 1.5));
        }
        children.push(seg);
    }
    container(row(children).height(Length::Shrink))
        .style(move |_theme| container::Style {
            border: Border { color: s.ink, width: 1.5, radius: 2.0.into() },
            shadow: hard_shadow(s, 2.0, 2.0, card_a),
            ..Default::default()
        })
        .into()
}

// ── Underlined text inputs ──────────────────────────────────────────────────
/// Style for the app's underlined text inputs: transparent background, no
/// border (the underline is a separate `hline`), `faint` icon/placeholder,
/// `ink` value, and a 35%-alpha `accent` selection. Shared by the fleet/brain
/// filter fields and the spawn modal's name/workspace fields.
pub fn underlined_input_style(s: &ColorScheme) -> impl Fn(&iced::Theme, text_input::Status) -> text_input::Style + '_ {
    move |_theme, _status| text_input::Style {
        background: Background::Color(Color::TRANSPARENT),
        border: Border::default(),
        icon: s.faint,
        placeholder: s.faint,
        value: s.ink,
        selection: Color { a: 0.35, ..s.accent },
    }
}

/// Shared serif underlined input for journal-entry modal "Name" fields
/// (spawn modal's session name, catalogue modal's catalogue name) — 16px
/// Newsreader, `underlined_input_style` background/selection treatment.
pub fn serif_underlined_input<'a>(
    s: &'a ColorScheme,
    placeholder: &'a str,
    value: &'a str,
) -> text_input::TextInput<'a, crate::app::Message> {
    text_input(placeholder, value)
        .font(SERIF)
        .size(16)
        .padding([4, 2])
        .style(underlined_input_style(s))
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
