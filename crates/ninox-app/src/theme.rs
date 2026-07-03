use ninox_core::{types::SessionStatus, ThemeVariant};
use iced::{color, Color, Theme};

/// Field Notes design tokens — spec: docs/design-concepts/field-notes-design.md §1.
/// The dark theme is the same journal read by lamplight, not a separate design.
#[derive(Debug, Clone, Copy)]
pub struct ColorScheme {
    // surfaces & ink
    pub paper:     Color, // app background
    pub paper_2:   Color, // sidebar, modal header, table header
    pub card:      Color, // cards, panels, modals, reading pane
    pub ink:       Color, // primary text, heavy borders
    pub ink_2:     Color, // secondary text
    pub faint:     Color, // tertiary/metadata text
    pub rule:      Color, // light rules/separators
    pub rule_dark: Color, // stronger rules, input underlines, card borders
    pub accent:    Color, // vermilion
    pub shadow:    Color, // hard-offset shadow base (alpha applied per-use)
    // status
    pub status_working:   Color,
    pub status_pr_open:   Color,
    pub status_ci_failed: Color,
    pub status_review:    Color,
    pub status_mergeable: Color,
    pub status_done:      Color,
    // brain categories beyond the status palette
    pub cat_pattern:      Color,
    pub cat_decision:     Color,
    pub cat_relationship: Color,
    pub cat_error:        Color,
    // terminal — "the dark object" on the page
    pub term_bg:         Color,
    pub term_bar:        Color,
    pub term_bar_border: Color,
    pub term_fg:         Color,
    pub term_ok:         Color,
    pub term_err:        Color,
    pub term_agent:      Color,
    pub term_dim:        Color,
}

impl ColorScheme {
    pub fn status_color(&self, status: &SessionStatus) -> Color {
        use SessionStatus::*;
        match status {
            Spawning | Working => self.status_working,
            PrOpen             => self.status_pr_open,
            CiFailed           => self.status_ci_failed,
            ReviewPending      => self.status_review,
            Mergeable          => self.status_mergeable,
            Done | Terminated  => self.status_done,
        }
    }

    pub fn iced_theme(&self) -> Theme {
        Theme::custom(
            "Ninox".into(),
            iced::theme::Palette {
                background: self.paper,
                text:       self.ink,
                primary:    self.accent,
                success:    self.status_working,
                danger:     self.status_ci_failed,
            },
        )
    }
}

pub fn from_variant(v: ThemeVariant) -> ColorScheme {
    match v {
        ThemeVariant::Light => light(),
        ThemeVariant::Dark  => dark(),
        // "ninox" third theme is not yet designed (spec §7) — lamplight for now.
        ThemeVariant::Ninox => dark(),
    }
}

pub fn light() -> ColorScheme {
    ColorScheme {
        paper:     color!(0xf5f0e4),
        paper_2:   color!(0xefe8d8),
        card:      color!(0xfbf7ee),
        ink:       color!(0x211d16),
        ink_2:     color!(0x5b5344),
        faint:     color!(0x968a72),
        rule:      color!(0xd9cfba),
        rule_dark: color!(0xb7ab90),
        accent:    color!(0xc8451f),
        shadow:    color!(0x211d16),
        status_working:   color!(0x3e7d34),
        status_pr_open:   color!(0x20629e),
        status_ci_failed: color!(0xc8451f),
        status_review:    color!(0xa97913),
        status_mergeable: color!(0x6d4fa3),
        status_done:      color!(0x8b8272),
        cat_pattern:      color!(0xa23f8c),
        cat_decision:     color!(0xc86a1f),
        cat_relationship: color!(0x2a8a80),
        cat_error:        color!(0xb3261e),
        term_bg:         color!(0x23201a),
        term_bar:        color!(0x2c2822),
        term_bar_border: color!(0x3a352c),
        term_fg:         color!(0xece4d0),
        term_ok:         color!(0x8fd37f),
        term_err:        color!(0xf08a72),
        term_agent:      color!(0xf0c069),
        term_dim:        color!(0x7a7260),
    }
}

pub fn dark() -> ColorScheme {
    ColorScheme {
        paper:     color!(0x171410),
        paper_2:   color!(0x1f1b15),
        card:      color!(0x262119),
        ink:       color!(0xece3cd),
        ink_2:     color!(0xb5a98d),
        faint:     color!(0x83775c),
        rule:      color!(0x393227),
        rule_dark: color!(0x4e4534),
        accent:    color!(0xe06038),
        shadow:    color!(0x000000),
        status_working:   color!(0x7cc46a),
        status_pr_open:   color!(0x5ca8e8),
        status_ci_failed: color!(0xe86a4c),
        status_review:    color!(0xd8a83c),
        status_mergeable: color!(0xa184d6),
        status_done:      color!(0x7d7461),
        cat_pattern:      color!(0xc876b4),
        cat_decision:     color!(0xe08a4a),
        cat_relationship: color!(0x4ab0a4),
        cat_error:        color!(0xe0604a),
        term_bg:         color!(0x100d09),
        term_bar:        color!(0x191510),
        term_bar_border: color!(0x2c261d),
        term_fg:         color!(0xece4d0),
        term_ok:         color!(0x8fd37f),
        term_err:        color!(0xf08a72),
        term_agent:      color!(0xf0c069),
        term_dim:        color!(0x7a7260),
    }
}
