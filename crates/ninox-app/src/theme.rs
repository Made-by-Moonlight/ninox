use ninox_core::{types::SessionStatus, ThemeVariant};
use iced::{color, Color, Theme};

#[derive(Debug, Clone, Copy)]
pub struct ColorScheme {
    pub bg_base:        Color,
    pub bg_surface:     Color,
    pub bg_elevated:    Color,
    pub bg_sidebar:     Color,
    pub border:         Color,
    pub text_primary:   Color,
    pub text_secondary: Color,
    pub text_muted:     Color,
    pub accent:         Color,
    pub terminal_bg:    Color,
    pub terminal_fg:    Color,
    pub status_green:   Color,
    pub status_blue:    Color,
    pub status_red:     Color,
    pub status_yellow:  Color,
    pub status_purple:  Color,
    pub status_grey:    Color,
    /// 16-entry ANSI palette (0-7 normal, 8-15 bright) used to render
    /// terminal text when a cell requests a named color that isn't
    /// otherwise overridden by the emulator's dynamic color table — see
    /// `terminal::ansi_to_iced`.
    pub ansi: [Color; 16],
}

impl ColorScheme {
    pub fn status_color(&self, status: &SessionStatus) -> Color {
        use SessionStatus::*;
        match status {
            Spawning | Working => self.status_green,
            PrOpen             => self.status_blue,
            CiFailed           => self.status_red,
            ReviewPending      => self.status_yellow,
            Mergeable          => self.status_purple,
            Done | Terminated  => self.status_grey,
        }
    }

    pub fn iced_theme(&self) -> Theme {
        Theme::custom(
            "Ninox".into(),
            iced::theme::Palette {
                background: self.bg_base,
                text:       self.text_primary,
                primary:    self.accent,
                success:    self.status_green,
                danger:     self.status_red,
            },
        )
    }
}

pub fn from_variant(v: ThemeVariant) -> ColorScheme {
    match v {
        ThemeVariant::Light  => light(),
        ThemeVariant::Dark   => dark(),
        ThemeVariant::Ninox => warm_dark(),
    }
}

pub fn light() -> ColorScheme {
    ColorScheme {
        bg_base:        color!(0xeef2fb),
        bg_surface:     color!(0xffffff),
        bg_elevated:    color!(0xf3f6ff),
        bg_sidebar:     color!(0xf5f7ff),
        border:         color!(0x93a7d7, 0.4),
        text_primary:   color!(0x1e2b4a),
        text_secondary: color!(0x4a5c80),
        text_muted:     color!(0x8a9bb8),
        accent:         color!(0x4a6cf7),
        terminal_bg:    color!(0x1e2b4a),
        terminal_fg:    color!(0xe8dcc8),
        status_green:   color!(0x22c55e),
        status_blue:    color!(0x4a6cf7),
        status_red:     color!(0xef4444),
        status_yellow:  color!(0xf59e0b),
        status_purple:  color!(0xa855f7),
        status_grey:    color!(0x94a3b8),
        // terminal_bg is a dark navy (0x1e2b4a), so this palette stays
        // dark-bg tuned even though the surrounding UI is light.
        ansi: [
            color!(0x2a3655), color!(0xef6b6b), color!(0x5fd68a), color!(0xf0c24a),
            color!(0x6f9df7), color!(0xc490f0), color!(0x55d3e0), color!(0xd5dcef),
            color!(0x5a6a92), color!(0xf79a9a), color!(0x8fe8b0), color!(0xf7d97e),
            color!(0x9dbcfa), color!(0xd9b6f5), color!(0x8ce4ee), color!(0xf2f5fd),
        ],
    }
}

pub fn dark() -> ColorScheme {
    ColorScheme {
        bg_base:        color!(0x0d1525),
        bg_surface:     color!(0x131e35),
        bg_elevated:    color!(0x1a2640),
        bg_sidebar:     color!(0x0f1a2e),
        border:         color!(0x3b599b, 0.45),
        text_primary:   color!(0xe2e8f8),
        text_secondary: color!(0x8a9bc5),
        text_muted:     color!(0x4a5a80),
        accent:         color!(0x6b8ef7),
        terminal_bg:    color!(0x0a1020),
        terminal_fg:    color!(0xe2e8f8),
        status_green:   color!(0x4ade80),
        status_blue:    color!(0x60a5fa),
        status_red:     color!(0xf87171),
        status_yellow:  color!(0xfbbf24),
        status_purple:  color!(0xa78bfa),
        status_grey:    color!(0x64748b),
        // Cool navy terminal_bg (0x0a1020).
        ansi: [
            color!(0x1a2233), color!(0xf87171), color!(0x4ade80), color!(0xfbbf24),
            color!(0x60a5fa), color!(0xc084fc), color!(0x22d3ee), color!(0xcbd5e1),
            color!(0x475569), color!(0xfca5a5), color!(0x86efac), color!(0xfde047),
            color!(0x93c5fd), color!(0xd8b4fe), color!(0x67e8f9), color!(0xf1f5f9),
        ],
    }
}

pub fn warm_dark() -> ColorScheme {
    ColorScheme {
        bg_base:        color!(0x1a1714),
        bg_surface:     color!(0x252118),
        bg_elevated:    color!(0x2e2a24),
        bg_sidebar:     color!(0x211e1a),
        border:         color!(0x3d3830, 0.6),
        text_primary:   color!(0xe8e4de),
        text_secondary: color!(0xa09880),
        text_muted:     color!(0x6b6358),
        accent:         color!(0xd4a843),
        terminal_bg:    color!(0x282828),
        terminal_fg:    color!(0xebdbb2),
        status_green:   color!(0x4ade80),
        status_blue:    color!(0x60a5fa),
        status_red:     color!(0xf87171),
        status_yellow:  color!(0xfbbf24),
        status_purple:  color!(0xa78bfa),
        status_grey:    color!(0x6b6358),
        // Gruvbox — the palette this app has always used for warm_dark.
        ansi: [
            color!(0x282828), color!(0xcc241d), color!(0x98971a), color!(0xd79921),
            color!(0x458588), color!(0xb16286), color!(0x689d6a), color!(0xa89984),
            color!(0x928374), color!(0xfb4934), color!(0xb8bb26), color!(0xfabd2f),
            color!(0x83a598), color!(0xd3869b), color!(0x8ec07c), color!(0xebdbb2),
        ],
    }
}
