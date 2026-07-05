//! Shared "folio header" scaffold — the split-weight serif title row atop
//! `fleet_board`, `pr_list`, and `brain_panel`. At normal widths it renders
//! as a single baseline row (title cluster, then trailing controls pushed
//! right); below [`NARROW_BREAKPOINT`] it wraps onto two rows (title
//! cluster, then controls right-aligned on their own line) so the trailing
//! mono stats/controls never get crushed into a vertical, letter-stacked
//! column (user-visual-review finding: session count collapsing to 1
//! character wide, date colliding with the filter field).
//!
//! Uses [`iced::widget::responsive`] to pick the layout from the folio's
//! actual available width. `Responsive` always reports `Length::Fill` for
//! its own size (see `iced_widget::lazy::responsive::Widget::size`), so
//! left unconstrained it would fight any `Length::Fill` sibling below it
//! (the board/table body) for vertical space, or otherwise render at an
//! unpredictable height. The wrapping `container` pins an explicit fixed
//! height per mode — decided from the caller's already-tracked
//! `window_width`/`sidebar_width`, which line up with `responsive`'s own
//! `bounds.width` closely enough for this width-only threshold — so the
//! folio always claims exactly the room its current mode needs, never more
//! (stealing space from the body) or less (collapsing to zero height).

use iced::widget::{column, container, responsive, row, Space};
use iced::{Alignment, Element, Length, Padding};

use crate::app::{App, Message};

/// Content width below which the folio wraps onto two rows.
pub const NARROW_BREAKPOINT: f32 = 980.0;

/// Horizontal/vertical padding shared by every folio (28px sides, 22 top /
/// 8 bottom) — baked into the height constants below.
fn folio_padding() -> Padding {
    Padding { top: 22.0, right: 28.0, bottom: 8.0, left: 28.0 }
}

/// Single-row height: the 34px serif title's line box (≈1.3×) plus the
/// folio's vertical padding.
const SINGLE_ROW_HEIGHT: f32 = 34.0 * 1.3 + 22.0 + 8.0;
/// Two-row height: the above, plus a second ~30px controls row and the gap
/// above it.
const TWO_ROW_HEIGHT: f32 = SINGLE_ROW_HEIGHT + 30.0 + 8.0;

/// Builds one folio header. `title` is the leading serif title cluster
/// (already including any date/volume/count stat that must stay glued to
/// it — spec: row 1 is always "title + date"). `controls` are the trailing
/// interactive/stat widgets (filter fields, counts, toggles, buttons) that
/// move to their own right-aligned row once the folio is narrower than
/// `NARROW_BREAKPOINT`; the scaffold spaces and right-aligns them in both
/// modes, so callers just supply the bare elements.
pub fn folio_scaffold<'a>(
    app: &App,
    title: impl Fn() -> Element<'a, Message> + 'a,
    controls: impl Fn() -> Vec<Element<'a, Message>> + 'a,
) -> Element<'a, Message> {
    let content_width = (app.window_width - app.sidebar_width).max(0.0);
    let height = if content_width < NARROW_BREAKPOINT { TWO_ROW_HEIGHT } else { SINGLE_ROW_HEIGHT };

    container(responsive(move |bounds| {
        let mut trailing: Vec<Element<Message>> = vec![Space::new(Length::Fill, 0).into()];
        trailing.extend(controls());

        if bounds.width < NARROW_BREAKPOINT {
            column![
                title(),
                Space::new(0, 8),
                row(trailing).spacing(18).align_y(Alignment::Center),
            ]
            .padding(folio_padding())
            .width(Length::Fill)
            .into()
        } else {
            let mut items = vec![title()];
            items.extend(trailing);
            row(items)
                .spacing(18)
                .align_y(Alignment::End)
                .padding(folio_padding())
                .width(Length::Fill)
                .into()
        }
    }))
    .width(Length::Fill)
    .height(Length::Fixed(height))
    .into()
}
