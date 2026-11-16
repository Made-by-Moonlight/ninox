//! v1 design tokens — the hex values are pulled directly from the design
//! artboards' own CSS custom properties (`Legend.dc.html`, `Main.dc.html`,
//! `FleetAtScale.dc.html`, all under `.claude/design-ref/fleet-overview-v1/`
//! in this repo's parent checkout), not re-derived. GUI-local: the existing
//! `ninox_ui_shared::theme` Field-Notes system is shared with `ninox-tui`
//! and stays untouched (see the shell PR's plan doc for why).

use iced::Color;
use ninox_core::types::SessionStatus;

pub(crate) const fn hex(v: u32) -> Color {
    Color::from_rgb(
        ((v >> 16) & 0xFF) as f32 / 255.0,
        ((v >> 8) & 0xFF) as f32 / 255.0,
        (v & 0xFF) as f32 / 255.0,
    )
}

pub const BG:     Color = hex(0x0b0e14);
pub const PANEL:  Color = hex(0x12161f);
pub const PANEL2: Color = hex(0x171c28);
/// `Main.dc.html`'s nav: link text, hovered link, active tab.
pub const NAV_LINK:   Color = hex(0xa3aabb);
pub const NAV_HOVER:  Color = hex(0x161b26);
pub const NAV_ACTIVE: Color = hex(0x1b2236);
pub const BORDER: Color = hex(0x242a3a);
pub const TEXT:   Color = hex(0xe8e9ed);
pub const DIM:    Color = hex(0x8b92a5);
pub const DIM2:   Color = hex(0x5b6478);
pub const TEAL:   Color = hex(0x4fd1c5);
pub const AMBER:  Color = hex(0xe8a33d);
pub const GREEN:  Color = hex(0x4ade80);
pub const GRAY:   Color = hex(0x6b7280);
pub const RED:    Color = hex(0xf0506e);
pub const VIOLET: Color = hex(0x9b8cff);
/// Brain kind-dot colors not needed by the Fleet/Board tabs
/// (`Brain.dc.html`'s `.kind .dot` rows): Repos, Concepts, Symbols, Hazards.
pub const BLUE:   Color = hex(0x9db4ff);
pub const PINK:   Color = hex(0xf472b6);
pub const CYAN:   Color = hex(0x67e8f9);
pub const ORANGE: Color = hex(0xfb923c);

/// Prompt-bar placeholder text (G1, round-1 try-out feedback) — deliberately
/// brighter than `DIM` so the "always on" prompt reads as a real input at a
/// glance instead of today's dim mono line.
pub const PLACEHOLDER: Color = hex(0x9aa1b2);

/// Badge/chip fills used by the nav's "needs you" pill and similar accents
/// (`Legend.dc.html`'s connection-badge row, `FleetAtScale.dc.html`'s
/// `.nav-count`).
pub const AMBER_BADGE_BG: Color = hex(0x3a2a12);
pub const AMBER_BADGE_FG: Color = hex(0xf3c27a);
pub const VIOLET_BADGE_BG: Color = hex(0x1d1a33);
pub const VIOLET_BADGE_FG: Color = hex(0xc9c2ff);
/// Sources view's LOCAL FOLDER type badge (`BrainSources.dc.html`).
pub const BLUE_BADGE_BG: Color = hex(0x1e2740);

/// Agent status dot color (`Legend.dc.html` "Agent status" row). Working/
/// Spawning share teal; PR-lifecycle statuses map onto the nearest agent
/// status since the Kanban's six columns are a different axis than the
/// Legend's four generic states.
pub fn status_color(status: &SessionStatus) -> Color {
    use SessionStatus::*;
    match status {
        Spawning | Working => TEAL,
        CiFailed | Interrupted => RED,
        ReviewPending | AwaitingReview => AMBER,
        PrOpen | Mergeable => GREEN,
        Done | Terminated | Queued => GRAY,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_colors_cover_every_variant() {
        use SessionStatus::*;
        for s in [Spawning, Working, PrOpen, CiFailed, ReviewPending, Mergeable, Done, Terminated, Interrupted, Queued] {
            // Just exercising every arm — a non-exhaustive match would fail to compile.
            let _ = status_color(&s);
        }
    }

    #[test]
    fn working_is_teal_and_failed_is_red() {
        assert_eq!(status_color(&SessionStatus::Working), TEAL);
        assert_eq!(status_color(&SessionStatus::CiFailed), RED);
    }
}
