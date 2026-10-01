//! Chrome colours. By default ([`Palette::terminal`]) only the host
//! terminal's default fg/bg and its 16 ANSI colours are used, so the TUI
//! inherits whatever palette the user's terminal is themed with. With
//! `[tui] colors = "field-notes"` the Field Notes tokens
//! (docs/design-concepts/field-notes-design.md §1) are painted in RGB from
//! the same `theme::Themes` the desktop app uses. Agent panes always keep
//! the colours the agent emits.

use ratatui::style::Color;

use crate::theme::ColorScheme;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub paper: Color,
    pub paper_2: Color,
    pub card: Color,
    pub ink: Color,
    pub ink_2: Color,
    pub faint: Color,
    pub rule: Color,
    pub accent: Color,
    pub working: Color,
    pub pr_open: Color,
    pub ci_failed: Color,
    pub review: Color,
    pub mergeable: Color,
    pub done: Color,
}

impl Default for Palette {
    fn default() -> Self {
        Self::terminal()
    }
}

impl Palette {
    /// The configured variant (and theme file), in truecolor when the
    /// terminal advertises it via `COLORTERM`, else the nearest xterm-256
    /// colours. Call before the alternate screen is up: a bad theme file
    /// warns through tracing, which writes to stdout.
    pub fn load(config: &ninox_core::config::AppConfig) -> Self {
        if config.tui.colors == ninox_core::config::TuiColors::Terminal {
            return Self::terminal();
        }
        let scheme = crate::theme::Themes::load(config.theme_file.as_deref()).scheme(config.theme);
        let truecolor = std::env::var("COLORTERM").is_ok_and(|v| matches!(v.as_str(), "truecolor" | "24bit"));
        Self::from_scheme(&scheme, truecolor)
    }

    #[cfg(test)]
    pub fn of(variant: ninox_core::ThemeVariant, truecolor: bool) -> Self {
        Self::from_scheme(&crate::theme::Themes::builtin().scheme(variant), truecolor)
    }

    /// The host terminal's palette: `Reset` for surfaces and body text (its
    /// own default bg/fg), bright black for muted text, rules and the
    /// selection, and named ANSI colours for accent and status.
    pub fn terminal() -> Self {
        Self {
            paper: Color::Reset,
            paper_2: Color::Reset,
            card: Color::DarkGray,
            ink: Color::Reset,
            ink_2: Color::Reset,
            faint: Color::DarkGray,
            rule: Color::DarkGray,
            accent: Color::LightRed,
            working: Color::Green,
            pr_open: Color::Blue,
            ci_failed: Color::Red,
            review: Color::Yellow,
            mergeable: Color::Cyan,
            done: Color::DarkGray,
        }
    }

    pub fn from_scheme(s: &ColorScheme, truecolor: bool) -> Self {
        let c = |c: iced::Color| {
            let (r, g, b) = ((c.r * 255.0).round() as u8, (c.g * 255.0).round() as u8, (c.b * 255.0).round() as u8);
            if truecolor {
                Color::Rgb(r, g, b)
            } else {
                Color::Indexed(xterm256(r, g, b))
            }
        };
        Self {
            paper: c(s.paper),
            paper_2: c(s.paper_2),
            card: c(s.card),
            ink: c(s.ink),
            ink_2: c(s.ink_2),
            faint: c(s.faint),
            rule: c(s.rule),
            accent: c(s.accent),
            working: c(s.status_working),
            pr_open: c(s.status_pr_open),
            ci_failed: c(s.status_ci_failed),
            review: c(s.status_review),
            mergeable: c(s.status_mergeable),
            done: c(s.status_done),
        }
    }
}

/// Nearest xterm-256 colour: the 6×6×6 cube (16–231) or the grey ramp
/// (232–255), by squared RGB distance. The 16 system colours are skipped —
/// terminals remap them.
pub fn xterm256(r: u8, g: u8, b: u8) -> u8 {
    const LEVELS: [i32; 6] = [0, 95, 135, 175, 215, 255];
    let nearest = |v: u8| -> usize {
        let v = v as i32;
        (0..6).min_by_key(|&i| (LEVELS[i] - v).abs()).unwrap_or(0)
    };
    let dist = |(r2, g2, b2): (i32, i32, i32)| {
        let (dr, dg, db) = (r as i32 - r2, g as i32 - g2, b as i32 - b2);
        dr * dr + dg * dg + db * db
    };
    let (ri, gi, bi) = (nearest(r), nearest(g), nearest(b));
    let cube = (16 + 36 * ri + 6 * gi + bi) as u8;
    let cube_d = dist((LEVELS[ri], LEVELS[gi], LEVELS[bi]));
    let avg = (r as i32 + g as i32 + b as i32) / 3;
    let gi = ((avg - 8).max(0) / 10).min(23);
    let grey = 8 + 10 * gi;
    if dist((grey, grey, grey)) < cube_d {
        232 + gi as u8
    } else {
        cube
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::ThemeVariant;

    #[test]
    fn default_palette_uses_only_the_terminals_own_colours() {
        let p = Palette::default();
        for c in [p.paper, p.paper_2, p.card, p.ink, p.ink_2, p.faint, p.rule, p.accent, p.working, p.pr_open, p.ci_failed, p.review, p.mergeable, p.done] {
            assert!(!matches!(c, Color::Rgb(..) | Color::Indexed(_)), "{c:?} is not a terminal palette colour");
        }
        let mut config = ninox_core::config::AppConfig::default();
        assert_eq!(Palette::load(&config), Palette::terminal());
        config.tui.colors = ninox_core::config::TuiColors::FieldNotes;
        assert!(matches!(Palette::load(&config).accent, Color::Rgb(..) | Color::Indexed(_)));
    }

    #[test]
    fn variants_map_to_their_field_notes_tokens() {
        let dark = Palette::of(ThemeVariant::Dark, true);
        assert_eq!(dark.accent, Color::Rgb(0xe0, 0x60, 0x38));
        assert_eq!(dark.paper, Color::Rgb(0x17, 0x14, 0x10));
        let light = Palette::of(ThemeVariant::Light, true);
        assert_eq!(light.accent, Color::Rgb(0xc8, 0x45, 0x1f));
        assert_eq!(light.ink, Color::Rgb(0x21, 0x1d, 0x16));
        assert_eq!(Palette::of(ThemeVariant::Ninox, true), dark);
    }

    #[test]
    fn without_truecolor_every_token_is_an_xterm_256_index() {
        let p = Palette::of(ThemeVariant::Dark, false);
        for c in [p.paper, p.ink, p.accent, p.faint, p.working, p.ci_failed] {
            assert!(matches!(c, Color::Indexed(16..=255)), "{c:?}");
        }
    }

    #[test]
    fn xterm256_picks_cube_and_grey_entries() {
        assert_eq!(xterm256(0, 0, 0), 16);
        assert_eq!(xterm256(255, 255, 255), 231);
        assert_eq!(xterm256(0xe0, 0x60, 0x38), 167, "vermilion lands on the warm red cube cell");
        assert_eq!(xterm256(0x17, 0x14, 0x10), 233, "dark paper is a near-black grey");
    }
}
