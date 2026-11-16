//! Shared styling: fonts, stamps, micro-labels and the underlined input.

use iced::font::{Family, Stretch, Style as FontStyle, Weight};
use iced::widget::{container, text, text_input, Space};
use iced::{Background, Border, Color, Element, Font, Length};
use ninox_core::types::SessionStatus;

// ── Typography: the v1 boards' stacks ─────────────────────────────────────
// UI text is `-apple-system, 'Segoe UI', Inter, system-ui, sans-serif` and
// mono `ui-monospace, 'SF Mono', Menlo, Consolas` (`Main.dc.html`). San
// Francisco can't be loaded by name, so UI text bundles Inter (the stack's
// cross-platform member) and mono uses Menlo where it exists.
pub const SANS: Font = Font {
    family: Family::Name("Inter"),
    weight: Weight::Normal, stretch: Stretch::Normal, style: FontStyle::Normal,
};
pub const SANS_SEMIBOLD: Font = Font { weight: Weight::Semibold, ..SANS };
pub const SANS_BOLD: Font = Font { weight: Weight::Bold, ..SANS };
pub const SANS_ITALIC: Font = Font { style: FontStyle::Italic, ..SANS };
#[cfg(target_os = "macos")]
pub const MONO: Font = Font {
    family: Family::Name("Menlo"),
    weight: Weight::Normal, stretch: Stretch::Normal, style: FontStyle::Normal,
};
// Elsewhere the bundled JetBrains Mono, so mono doesn't depend on which
// monospace faces the system happens to have.
#[cfg(not(target_os = "macos"))]
pub const MONO: Font = Font {
    family: Family::Name("JetBrains Mono"),
    weight: Weight::Normal, stretch: Stretch::Normal, style: FontStyle::Normal,
};
// Neither mono has a 500 face, and a missing weight falls back to another
// family outright.
pub const MONO_MEDIUM: Font = Font { weight: Weight::Bold, ..MONO };

/// Points the generic families at the v1 stacks, so text that asks for them
/// rather than a family by name (iced's markdown: bold and italic spans,
/// code) gets Inter and Menlo too.
pub fn use_v1_generic_families() {
    let mut fonts = iced::advanced::graphics::text::font_system().write().expect("font system lock");
    let db = fonts.raw().db_mut();
    db.set_sans_serif_family("Inter");
    db.set_monospace_family(if cfg!(target_os = "macos") { "Menlo" } else { "JetBrains Mono" });
}

/// Every font file the app registers at startup (`main`). cosmic-text only
/// takes a face of exactly the requested weight and style before falling
/// back to system fonts, so each weight `SANS` is used at has its own static
/// face (Inter 4.1, hinting stripped; OFL-Inter.txt).
pub const BUNDLED_FONTS: &[&[u8]] = &[
    NERD_FONT_BYTES,
    GLYPH_FONT_BYTES,
    include_bytes!("../assets/fonts/Inter-Regular.ttf"),
    include_bytes!("../assets/fonts/Inter-SemiBold.ttf"),
    include_bytes!("../assets/fonts/Inter-Bold.ttf"),
    include_bytes!("../assets/fonts/Inter-Italic.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMono-Bold.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMono-Italic.ttf"),
    include_bytes!("../assets/fonts/JetBrainsMono-BoldItalic.ttf"),
];

/// The app's dingbats (`⌕ ⬡ ✦ ☰ ⚑ ✕ ☐ ☑ …`), none of which Inter covers:
/// "Ninox Symbols", bundled so they render the same on every platform. It
/// merges Noto Sans Symbols (instanced at 400) and Noto Sans Symbols 2,
/// subset to the Arrows, Misc Technical, Geometric Shapes, Misc Symbols,
/// Dingbats and Misc Symbols and Arrows blocks (OFL-NinoxSymbols.txt). No
/// ASCII: text mixing letters with a symbol goes through `glyph_font_for`.
pub const GLYPH: Font = Font::with_name("Ninox Symbols");

pub const GLYPH_FONT_BYTES: &[u8] = include_bytes!("../assets/fonts/NinoxSymbols-Regular.ttf");
pub const NERD_FONT_BYTES: &[u8] = include_bytes!("../assets/fonts/SymbolsNerdFontMono-Regular.ttf");

/// Keycaps and labels mixing a symbol Inter has with ASCII ("⌘K", "↵").
pub const GLYPH_ALT: Font = SANS;

/// The symbols Inter covers, so text made of them and ASCII can stay in
/// `SANS` under Basic shaping (no per-glyph fallback).
pub const INTER_SYMBOLS: &str = "·×–—•…›←↑→↓↗↵⇧−⌘⏎▶▼○●✓✗";

/// Terminal-style iconography (e.g. `❯`). Shares the "Symbols Nerd Font
/// Mono" family already bundled and used by `components::terminal`.
pub const NERD: Font = Font {
    family: Family::Name("Symbols Nerd Font Mono"),
    weight: Weight::Normal, stretch: Stretch::Normal, style: FontStyle::Normal,
};

/// The font for `text`, rendered with Basic shaping: `SANS` when Inter
/// covers every character (ASCII included, so "⌘K" stays one font),
/// otherwise the symbol font.
pub fn glyph_font_for(text: &str) -> Font {
    if text.chars().all(|c| c.is_ascii() || INTER_SYMBOLS.contains(c)) { GLYPH_ALT } else { GLYPH }
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
        Interrupted        => "Interrupted",
        AwaitingReview     => "For Review",
        Queued             => "Queued",
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

// ── Underlined text inputs ──────────────────────────────────────────────────
/// Underlined-field style: transparent background, no border (the underline is a separate `hline`), `DIM2`
/// icon/placeholder, `TEXT` value, 35%-alpha `TEAL` selection.
pub fn v1_underlined_input_style() -> impl Fn(&iced::Theme, text_input::Status) -> text_input::Style {
    use crate::components::shell::tokens;
    |_theme, _status| text_input::Style {
        background: Background::Color(Color::TRANSPARENT),
        border: Border::default(),
        icon: tokens::DIM2,
        placeholder: tokens::DIM2,
        value: tokens::TEXT,
        selection: Color { a: 0.35, ..tokens::TEAL },
    }
}

/// Registers the app's fonts with the process-wide font system, once, as
/// `main` does. Every headless layout test calls it first: otherwise its
/// measurements depend on whether another test happened to load them yet.
#[cfg(test)]
pub(crate) fn load_bundled_fonts() {
    static LOAD: std::sync::Once = std::sync::Once::new();
    LOAD.call_once(|| {
        {
            let mut fonts = iced::advanced::graphics::text::font_system().write().unwrap();
            for bytes in BUNDLED_FONTS {
                fonts.load_font(std::borrow::Cow::Borrowed(bytes));
            }
        }
        use_v1_generic_families();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each glyph `text` shapes to in `font`, as the renderer resolves it:
    /// (glyph id, face postscript name, weight, italic).
    fn shape(text: &str, font: Font, basic: bool) -> Vec<(u16, String, u16, bool)> {
        use iced::advanced::graphics::text::cosmic_text::{fontdb, Buffer, Metrics, Shaping};
        load_bundled_fonts();
        let mut fonts = iced::advanced::graphics::text::font_system().write().unwrap();
        let raw = fonts.raw();
        let mut buffer = Buffer::new(raw, Metrics::new(20.0, 26.0));
        let attrs = iced::advanced::graphics::text::to_attributes(font);
        // iced's `text` shapes Basic (no per-glyph fallback) unless a site
        // asks for Advanced.
        let shaping = if basic { Shaping::Basic } else { Shaping::Advanced };
        buffer.set_text(raw, text, attrs, shaping);
        buffer.shape_until_scroll(raw, false);
        let glyphs: Vec<_> = buffer.layout_runs().flat_map(|run| run.glyphs.iter().map(|g| (g.glyph_id, g.font_id))).collect();
        glyphs
            .into_iter()
            .map(|(glyph, id)| {
                let face = raw.db().face(id).expect("face");
                (glyph, face.post_script_name.clone(), face.weight.0, face.style != fontdb::Style::Normal)
            })
            .collect()
    }

    /// cosmic-text only takes a face of exactly the requested weight and
    /// style before falling back to system fonts, so each of these needs its
    /// own bundled Inter face. Markdown asks for the generic sans-serif.
    #[test]
    fn sans_text_shapes_in_inter_at_its_own_weight() {
        let markdown_bold = Font { weight: Weight::Bold, ..Font::default() };
        let markdown_italic = Font { style: FontStyle::Italic, ..Font::default() };
        for (label, font, weight, italic) in [
            ("SANS", SANS, 400, false),
            ("SANS_SEMIBOLD", SANS_SEMIBOLD, 600, false),
            ("SANS_BOLD", SANS_BOLD, 700, false),
            ("SANS_ITALIC", SANS_ITALIC, 400, true),
            ("markdown bold", markdown_bold, 700, false),
            ("markdown italic", markdown_italic, 400, true),
        ] {
            let (_, name, got_weight, got_italic) = shape("Hamburgefonstiv", font, true).remove(0);
            assert!(name.starts_with("Inter"), "{label} shaped through {name}");
            assert_eq!((got_weight, got_italic), (weight, italic), "{label} shaped through {name}");
        }
    }

    /// Menlo on macOS (the board's `ui-monospace` stack), the bundled
    /// JetBrains Mono elsewhere. Neither has a 500 face, and outside the
    /// generic monospace family cosmic-text won't round a missing weight to
    /// a near one, so every mono weight in use must exist.
    #[test]
    fn mono_text_shapes_in_the_platform_mono() {
        let expected = if cfg!(target_os = "macos") { "Menlo" } else { "JetBrainsMono" };
        for (label, font) in [("MONO", MONO), ("MONO_MEDIUM", MONO_MEDIUM), ("markdown code", Font::MONOSPACE)] {
            let (_, name, ..) = shape("fn main()", font, true).remove(0);
            assert!(name.starts_with(expected), "{label} shaped through {name}");
        }
    }

    /// Everything rendered through `GLYPH` or `glyph_font_for`, shaped Basic
    /// as their call sites do — including keycaps that mix a symbol with
    /// ASCII ("⌘K"), where every glyph must come from the one font.
    #[test]
    fn glyph_font_text_renders_as_real_glyphs() {
        for text in ["⌕", "⬡", "✦", "☰", "⚑", "✕", "☐", "☑", "⏎", "↗"] {
            for (glyph, name, ..) in shape(text, GLYPH, true) {
                assert!(glyph != 0, "{text:?} shaped to .notdef in {name}");
            }
        }
        for text in ["⌘K", "⌘⇧K", "↑↓", "→", "↵", "esc", "⌘", "✕", "✦", "⌕"] {
            for (glyph, name, ..) in shape(text, glyph_font_for(text), true) {
                assert!(glyph != 0, "{text:?} via glyph_font_for shaped to .notdef in {name}");
            }
        }
    }

    /// Every symbol the app renders in SANS text with iced's default Basic
    /// shaping. Basic shaping never falls back, so each must be a real Inter
    /// glyph; a symbol Inter lacks belongs in `ADVANCED_SYMBOLS` instead,
    /// with its call site switched to `Shaping::Advanced`.
    const BASIC_SYMBOLS: &str = INTER_SYMBOLS;

    /// Symbols Inter lacks, rendered at `Shaping::Advanced` sites
    /// (`fleet_islands`' fold chevron, the `✕` on chips and toggles,
    /// `fleet_focus`' tool note, `info_panel`'s pending check).
    const ADVANCED_SYMBOLS: &str = "▸✕⚒◌";

    #[test]
    fn sans_symbols_render_as_real_glyphs() {
        for symbol in BASIC_SYMBOLS.chars() {
            let (glyph, name, ..) = shape(&symbol.to_string(), SANS, true).remove(0);
            assert!(glyph != 0 && name.starts_with("Inter"), "{symbol:?} shaped to glyph {glyph} in {name}");
        }
        for symbol in ADVANCED_SYMBOLS.chars() {
            let (glyph, name, ..) = shape(&symbol.to_string(), SANS, false).remove(0);
            assert!(glyph != 0, "{symbol:?} has no fallback glyph (shaped to .notdef in {name})");
        }
    }

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
        assert_eq!(stamp_word(&SessionStatus::Interrupted),   "Interrupted");
    }

    #[test]
    fn glyph_font_for_keeps_inter_covered_text_in_one_font() {
        assert_eq!(glyph_font_for("⌘K"), GLYPH_ALT);
        assert_eq!(glyph_font_for("↑↓"), GLYPH_ALT);
        assert_eq!(glyph_font_for("✦"), GLYPH);
        assert_eq!(glyph_font_for("✕"), GLYPH);
        assert_eq!(glyph_font_for("⬡"), GLYPH);
    }
}
