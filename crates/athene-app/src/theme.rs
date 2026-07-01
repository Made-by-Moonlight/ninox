use athene_core::{types::SessionStatus, ThemeVariant};
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
            "Athene".into(),
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
        ThemeVariant::Athene => warm_dark(),
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
    }
}
