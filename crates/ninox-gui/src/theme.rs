//! Materializes the toolkit-neutral `ninox_ui_shared::theme` tokens into
//! `iced::Color` for this crate's widgets. Re-exports everything from the
//! shared module (loading/parsing/file I/O) and shadows only the
//! color-bearing items (`ColorScheme`, `Themes`, `light`/`dark`,
//! `from_variant`) with iced-typed equivalents — a local definition always
//! wins over a glob-imported name of the same identifier, so no ambiguity.

pub use ninox_ui_shared::theme::*;

use ninox_core::ThemeVariant;
use ninox_ui_shared::theme::Rgba;

/// `Rgba` → `iced::Color`. A plain function, not a `From` impl: both types
/// are foreign to this crate, so the orphan rule forbids it.
fn c(v: Rgba) -> iced::Color {
    iced::Color::from_rgba8(v.r, v.g, v.b, v.a as f32 / 255.0)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorScheme {
    pub paper:     iced::Color,
    pub paper_2:   iced::Color,
    pub card:      iced::Color,
    pub ink:       iced::Color,
    pub ink_2:     iced::Color,
    pub faint:     iced::Color,
    pub rule:      iced::Color,
    pub rule_dark: iced::Color,
    pub accent:    iced::Color,
    pub shadow:    iced::Color,
    pub status_working:   iced::Color,
    pub status_pr_open:   iced::Color,
    pub status_ci_failed: iced::Color,
    pub status_review:    iced::Color,
    pub status_mergeable: iced::Color,
    pub status_done:      iced::Color,
    pub cat_pattern:      iced::Color,
    pub cat_decision:     iced::Color,
    pub cat_relationship: iced::Color,
    pub cat_error:        iced::Color,
    pub term_bg:         iced::Color,
    pub term_bar:        iced::Color,
    pub term_bar_border: iced::Color,
    pub term_fg:         iced::Color,
    pub term_ok:         iced::Color,
    pub term_err:        iced::Color,
    pub term_agent:      iced::Color,
    pub term_dim:        iced::Color,
    pub ansi: [iced::Color; 16],
    pub dark: bool,
}

impl From<ninox_ui_shared::theme::ColorScheme> for ColorScheme {
    fn from(s: ninox_ui_shared::theme::ColorScheme) -> Self {
        Self {
            paper: c(s.paper), paper_2: c(s.paper_2), card: c(s.card),
            ink: c(s.ink), ink_2: c(s.ink_2), faint: c(s.faint),
            rule: c(s.rule), rule_dark: c(s.rule_dark), accent: c(s.accent),
            shadow: c(s.shadow),
            status_working: c(s.status_working), status_pr_open: c(s.status_pr_open),
            status_ci_failed: c(s.status_ci_failed), status_review: c(s.status_review),
            status_mergeable: c(s.status_mergeable), status_done: c(s.status_done),
            cat_pattern: c(s.cat_pattern), cat_decision: c(s.cat_decision),
            cat_relationship: c(s.cat_relationship), cat_error: c(s.cat_error),
            term_bg: c(s.term_bg), term_bar: c(s.term_bar),
            term_bar_border: c(s.term_bar_border), term_fg: c(s.term_fg),
            term_ok: c(s.term_ok), term_err: c(s.term_err),
            term_agent: c(s.term_agent), term_dim: c(s.term_dim),
            ansi: s.ansi.map(c),
            dark: s.dark,
        }
    }
}

impl ColorScheme {
    /// Independent of the scheme: v1 chrome is dark-only, so widgets that
    /// inherit the base palette (markdown text, inputs, scrollables) must
    /// agree with `tokens` even when `theme = "light"`.
    pub fn iced_theme() -> iced::Theme {
        use crate::components::shell::tokens;
        iced::Theme::custom(
            "Ninox".into(),
            iced::theme::Palette {
                background: tokens::BG,
                text:       tokens::TEXT,
                primary:    tokens::BLUE,
                success:    tokens::GREEN,
                danger:     tokens::RED,
            },
        )
    }
}

/// Overlays the v1 shell's dark palette onto a shared
/// `ninox_ui_shared::theme::dark()` base — GUI-only (this file, not
/// `ninox_ui_shared`), so `ninox-tui`'s own dark theme is untouched. Hex
/// values are the v1 design tokens (`components::shell::tokens`); only
/// fields with a sensible v1 equivalent are overridden — `cat_*`/`term_*`
/// (brain pinboard categories, the terminal palette) keep their existing
/// values since no board redesigns them.
fn apply_v1_dark_overlay(s: &mut ninox_ui_shared::theme::ColorScheme) {
    let hex = |v: &str| ninox_ui_shared::theme::parse_hex(v).expect("valid hex literal");
    s.paper             = hex("#0b0e14"); // --bg
    s.paper_2            = hex("#12161f"); // --panel
    s.card               = hex("#171c28"); // --panel2
    s.ink                = hex("#e8e9ed"); // --text
    s.ink_2              = hex("#8b92a5"); // --dim
    s.faint              = hex("#5b6478"); // --dim2
    s.rule               = hex("#242a3a"); // --border
    s.rule_dark          = hex("#343c50"); // FleetAtScale.dc.html's .tick/.rail
    s.accent             = hex("#4fd1c5"); // --teal
    s.shadow             = hex("#000000");
    s.status_working     = hex("#4fd1c5"); // --teal
    s.status_pr_open     = hex("#9db4ff"); // badge-type text color
    s.status_ci_failed   = hex("#f0506e"); // --red
    s.status_review      = hex("#e8a33d"); // --amber
    s.status_mergeable   = hex("#4ade80"); // --green
    s.status_done        = hex("#6b7280"); // --gray
}

/// Both loaded palettes, materialized to `iced::Color`.
#[derive(Debug, Clone)]
pub struct Themes {
    pub light: ColorScheme,
    pub dark:  ColorScheme,
}

impl Themes {
    pub fn builtin() -> Self {
        let t = ninox_ui_shared::theme::Themes::builtin();
        let mut dark = t.dark;
        apply_v1_dark_overlay(&mut dark);
        Self { light: t.light.into(), dark: dark.into() }
    }

    pub fn load(theme_file: Option<&str>) -> Self {
        let t = ninox_ui_shared::theme::Themes::load(theme_file);
        let mut dark = t.dark;
        apply_v1_dark_overlay(&mut dark);
        Self { light: t.light.into(), dark: dark.into() }
    }

    pub fn scheme(&self, v: ThemeVariant) -> ColorScheme {
        match v {
            ThemeVariant::Light => self.light,
            ThemeVariant::Dark | ThemeVariant::Ninox => self.dark,
        }
    }
}

#[cfg(test)]
pub fn light() -> ColorScheme {
    ninox_ui_shared::theme::light().into()
}

#[cfg(test)]
pub fn dark() -> ColorScheme {
    let mut s = ninox_ui_shared::theme::dark();
    apply_v1_dark_overlay(&mut s);
    s.into()
}

#[allow(dead_code)]
pub fn from_variant(v: ThemeVariant) -> ColorScheme {
    Themes::builtin().scheme(v)
}

#[cfg(test)]
mod iced_theme_tests {
    use super::*;
    use crate::components::shell::tokens;

    #[test]
    fn base_palette_follows_tokens_not_the_light_scheme() {
        let p = ColorScheme::iced_theme().palette();
        assert_eq!(p.text, tokens::TEXT);
        assert_eq!(p.background, tokens::BG);
        assert_ne!(p.text, light().ink);
    }
}
