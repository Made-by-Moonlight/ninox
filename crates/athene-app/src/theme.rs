use iced::{color, Color, Theme};

pub const BG_BASE: Color = color!(0x1a1714);
pub const BG_SURFACE: Color = color!(0x252118);
pub const BG_ELEVATED: Color = color!(0x2e2a24);
pub const BG_SIDEBAR: Color = color!(0x211e1a);
pub const BORDER: Color = color!(0x3d3830, 0.6);
pub const TEXT_PRIMARY: Color = color!(0xe8e4de);
pub const TEXT_SECONDARY: Color = color!(0xa09880);
pub const TEXT_MUTED: Color = color!(0x6b6358);
pub const ACCENT_AMBER: Color = color!(0xd4a843);
pub const STATUS_GREEN: Color = color!(0x4ade80);
pub const STATUS_BLUE: Color = color!(0x60a5fa);
pub const STATUS_RED: Color = color!(0xf87171);
pub const STATUS_YELLOW: Color = color!(0xfbbf24);
pub const STATUS_PURPLE: Color = color!(0xa78bfa);
pub const STATUS_GREY: Color = color!(0x6b6358);

pub fn athene_theme() -> Theme {
    Theme::custom(
        "Athene".into(),
        iced::theme::Palette {
            background: BG_BASE,
            text: TEXT_PRIMARY,
            primary: ACCENT_AMBER,
            success: STATUS_GREEN,
            danger: STATUS_RED,
        },
    )
}

pub fn status_color(status: &athene_core::types::SessionStatus) -> Color {
    use athene_core::types::SessionStatus::*;
    match status {
        Spawning | Working => STATUS_GREEN,
        PrOpen => STATUS_BLUE,
        CiFailed => STATUS_RED,
        ReviewPending => STATUS_YELLOW,
        Mergeable => STATUS_PURPLE,
        Done | Terminated => STATUS_GREY,
    }
}
