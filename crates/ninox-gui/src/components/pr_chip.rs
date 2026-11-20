//! Clickable PR chips: one pill per PR (`bellhop#1466`) coloured by state,
//! then a `+N` pill for the rest. A PR chip opens the PR in the browser;
//! `+N` opens the session, whose side panel lists every PR. Both are
//! buttons so they capture the press before a card or row around them
//! sees it.

use iced::{
    widget::{button, container, row, text, tooltip},
    Alignment, Background, Border, Color, Element,
};

use ninox_core::{
    pr_events::CiVerdict,
    session_prs::{PrState, SessionPr},
};

use super::shell::tokens;
use crate::{app::Message, style::SANS};

/// Label cap on roomy surfaces (cards); rows pass a smaller one.
pub const CARD_LABEL_CHARS: usize = 20;

/// Failing CI, changes requested, pending, passing, draft, merged and
/// closed each get their own colour; an open PR with no checks known is grey.
pub fn tone(pr: &SessionPr) -> Color {
    match pr.state {
        PrState::Merged => tokens::VIOLET_BADGE_FG,
        PrState::Closed => tokens::DIM2,
        PrState::Open if pr.is_failing() => tokens::RED,
        PrState::Open if pr.changes_requested => tokens::ORANGE,
        PrState::Open if pr.draft => tokens::DIM,
        // No checks known: neither passing nor failing, so no green.
        PrState::Open if pr.ci == CiVerdict::None && pr.total() == 0 => tokens::GRAY,
        PrState::Open if pr.is_pending() => tokens::BLUE,
        PrState::Open => tokens::GREEN,
    }
}

/// How many chips fit, and how many PRs fold into `+N`.
pub fn split(count: usize, max: usize) -> (usize, usize) {
    let shown = count.min(max);
    (shown, count - shown)
}

pub fn label(pr: &SessionPr, max_chars: usize) -> String {
    let full = pr.label();
    if full.chars().count() <= max_chars {
        return full;
    }
    let tail = format!("#{}", pr.number);
    let keep = max_chars.saturating_sub(tail.chars().count() + 1);
    let name: String = full.chars().take(keep).collect();
    format!("{name}…{tail}")
}

pub fn tip(pr: &SessionPr) -> String {
    let mut parts = Vec::new();
    if !pr.title.is_empty() {
        parts.push(pr.title.clone());
    }
    parts.push(pr.state_line());
    if pr.is_open() {
        parts.extend(pr.checks_line());
    }
    format!("{} — click to open", parts.join(" · "))
}

fn pill<'a>(label: String, fg: Color, msg: Message, tip_text: String) -> Element<'a, Message> {
    let body = button(text(label).size(9.5).font(SANS).color(fg).wrapping(iced::widget::text::Wrapping::None))
        .padding([1, 6])
        .on_press(msg)
        .style(move |_t, status| {
            let hovered = matches!(status, button::Status::Hovered);
            button::Style {
                background: Some(Background::Color(Color { a: if hovered { 0.28 } else { 0.16 }, ..fg })),
                text_color: fg,
                border: Border { color: Color { a: 0.45, ..fg }, width: 1.0, radius: 9.0.into() },
                ..Default::default()
            }
        });
    let tip = container(text(tip_text).size(11).font(SANS).color(tokens::TEXT))
        .padding([5, 8])
        .style(|_| container::Style {
            background: Some(Background::Color(tokens::PANEL2)),
            border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
            ..Default::default()
        });
    tooltip(body, tip, tooltip::Position::Bottom).gap(6).into()
}

/// The one chip a cramped row keeps when the full chips don't fit: the
/// number of PRs (`5 PRs`, opens the session's list), or the PR itself
/// (`#1466`, opens it) when there is only one. The lead PR sets the colour.
pub fn count_chip<'a>(session_id: &str, prs: &[SessionPr]) -> Option<Element<'a, Message>> {
    let lead = prs.first()?;
    Some(match prs {
        [only] => pill(format!("#{}", only.number), tone(only), Message::OpenUrl(only.url.clone()), tip(only)),
        _ => pill(
            count_label(prs.len()),
            tone(lead),
            Message::NavigateSession(session_id.to_string()),
            format!("{} PRs — click to see them all", prs.len()),
        ),
    })
}

fn count_label(n: usize) -> String {
    format!("{n} PRs")
}

pub fn count_width_estimate(prs: &[SessionPr]) -> f32 {
    let label = match prs {
        [only] => format!("#{}", only.number),
        _ => count_label(prs.len()),
    };
    label.chars().count() as f32 * 5.6 + 16.0 + 4.0
}

/// Rough pixel width of `chips(.., max, ..)` laid out on one line.
pub fn width_estimate(prs: &[SessionPr], max: usize, label_max: usize) -> f32 {
    const CHAR_PX: f32 = 5.6;
    const PILL_PAD_PX: f32 = 16.0;
    const GAP_PX: f32 = 4.0;
    let (shown, hidden) = split(prs.len(), max);
    let mut px: f32 = prs[..shown].iter().map(|p| label(p, label_max).chars().count() as f32 * CHAR_PX + PILL_PAD_PX + GAP_PX).sum();
    if hidden > 0 {
        px += (format!("+{hidden}").chars().count() as f32) * CHAR_PX + PILL_PAD_PX + GAP_PX;
    }
    px
}

/// Up to `max` PR chips then `+N`. `None` when the session has no PRs.
/// `wrap` lets a card's chips flow onto a second line; a sidebar row
/// keeps them on one.
pub fn chips<'a>(session_id: &str, prs: &[SessionPr], max: usize, label_max: usize, wrap: bool) -> Option<Element<'a, Message>> {
    if prs.is_empty() {
        return None;
    }
    let (shown, hidden) = split(prs.len(), max);
    let mut items: Vec<Element<Message>> = prs[..shown]
        .iter()
        .map(|pr| pill(label(pr, label_max), tone(pr), Message::OpenUrl(pr.url.clone()), tip(pr)))
        .collect();
    if hidden > 0 {
        items.push(pill(
            format!("+{hidden}"),
            tokens::DIM,
            Message::NavigateSession(session_id.to_string()),
            format!("{hidden} more — click to see all {} PRs", prs.len()),
        ));
    }
    let r = row(items).spacing(4).align_y(Alignment::Center);
    Some(if wrap { r.wrap().into() } else { r.into() })
}

/// Glyph advance as a fraction of the font size, for budgeting a row's width
/// without measuring text. Conservative (Inter averages ~0.5 em, digit-heavy names ~0.58) so a
/// truncated name errs short rather than being re-wrapped by the layout.
/// Layout runs in logical px and the window zoom scales layout and text
/// together, so only the text size needs to enter the estimate.
const GLYPH_EM: f32 = 0.58;
/// Space between a name and its chips.
const NAME_CHIP_GAP_PX: f32 = 6.0;

pub fn text_px(s: &str, size: f32) -> f32 {
    s.chars().count() as f32 * size * GLYPH_EM
}

/// Truncates to `max_chars` including the ellipsis, so the result never
/// exceeds the budget.
pub fn ellipsize(name: &str, max_chars: usize) -> String {
    if name.chars().count() <= max_chars {
        name.to_string()
    } else {
        let keep = max_chars.saturating_sub(1).max(1);
        format!("{}…", name.chars().take(keep).collect::<String>())
    }
}

/// `name` cut to fit `avail_px` at text `size`.
pub fn fit_name(name: &str, size: f32, avail_px: f32) -> String {
    ellipsize(name, (avail_px / (size * GLYPH_EM)).floor().max(1.0) as usize)
}

/// What a session's PR chips look like in a given width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipPlan {
    /// Up to `max` chips (the rest fold into `+N`), labels cut to `label_chars`.
    Chips { max: usize, label_chars: usize },
    /// The single `N PRs` / `#N` chip.
    Count,
}

impl ChipPlan {
    /// Pixels the chips and their leading gap take.
    pub fn width(self, prs: &[SessionPr]) -> f32 {
        NAME_CHIP_GAP_PX
            + match self {
                ChipPlan::Chips { max, label_chars } => width_estimate(prs, max, label_chars),
                ChipPlan::Count => count_width_estimate(prs),
            }
    }

    pub fn max_chips(self) -> usize {
        match self {
            ChipPlan::Chips { max, .. } => max,
            ChipPlan::Count => 0,
        }
    }
}

/// Picks the richest chips that still leave the whole name in `avail_px`:
/// `max_chips` down to one, then one chip with `short_label` chars, then the
/// count chip. A session with PRs always keeps a clickable chip; when even
/// that doesn't fit, the caller truncates the name instead.
pub fn plan(prs: &[SessionPr], name_px: f32, avail_px: f32, max_chips: usize, label_chars: usize, short_label: Option<usize>) -> ChipPlan {
    let full = (1..=max_chips.max(1)).rev().map(|max| ChipPlan::Chips { max, label_chars });
    let short = short_label.map(|label_chars| ChipPlan::Chips { max: 1, label_chars });
    full.chain(short)
        .find(|p| name_px + p.width(prs) <= avail_px)
        .unwrap_or(ChipPlan::Count)
}

/// Draws `plan`; `None` when the session has no PRs.
pub fn render<'a>(session_id: &str, prs: &[SessionPr], plan: ChipPlan, wrap: bool) -> Option<Element<'a, Message>> {
    match plan {
        ChipPlan::Chips { max, label_chars } => chips(session_id, prs, max, label_chars, wrap),
        ChipPlan::Count => count_chip(session_id, prs),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(state: PrState) -> SessionPr {
        SessionPr {
            repo: "o/bellhop".into(), number: 1466, url: "https://github.com/o/bellhop/pull/1466".into(),
            title: "Spoke sizing".into(), state, draft: false, ci: CiVerdict::Passed,
            passing: 3, failing: 0, pending: 0, changes_requested: false, approved: false, tracked: true,
        }
    }

    #[test]
    fn every_state_has_its_own_colour() {
        let mut failing = pr(PrState::Open);
        failing.ci = CiVerdict::Failed;
        failing.failing = 1;
        let mut changes = pr(PrState::Open);
        changes.changes_requested = true;
        let mut pending = pr(PrState::Open);
        pending.ci = CiVerdict::Pending;
        pending.pending = 1;
        let mut draft = pr(PrState::Open);
        draft.draft = true;
        let tones = [
            tone(&failing), tone(&changes), tone(&pending), tone(&pr(PrState::Open)),
            tone(&draft), tone(&pr(PrState::Merged)), tone(&pr(PrState::Closed)),
        ];
        for (i, a) in tones.iter().enumerate() {
            for b in &tones[i + 1..] {
                assert_ne!(a, b, "{tones:?}");
            }
        }
        assert_eq!(tone(&changes), tokens::ORANGE);
        assert_eq!(tone(&failing), tokens::RED);
    }

    #[test]
    fn failing_ci_outranks_changes_requested() {
        let mut p = pr(PrState::Open);
        p.changes_requested = true;
        p.failing = 2;
        assert_eq!(tone(&p), tokens::RED);
    }

    #[test]
    fn split_folds_the_rest_into_a_count() {
        assert_eq!(split(1, 3), (1, 0));
        assert_eq!(split(3, 3), (3, 0));
        assert_eq!(split(5, 3), (3, 2));
        assert_eq!(split(4, 2), (2, 2));
    }

    #[test]
    fn a_cramped_row_keeps_a_count_chip_or_the_single_pr() {
        let one = [pr(PrState::Open)];
        let many = [pr(PrState::Open), pr(PrState::Merged), pr(PrState::Closed)];
        assert!(count_chip("s", &one).is_some() && count_chip("s", &many).is_some());
        assert!(count_chip("s", &[]).is_none());
        assert!(count_width_estimate(&many) < width_estimate(&many, 2, CARD_LABEL_CHARS));
    }

    #[test]
    fn long_labels_keep_the_number() {
        let mut p = pr(PrState::Open);
        p.repo = "o/a-very-long-repository-name".into();
        let l = label(&p, CARD_LABEL_CHARS);
        assert!(l.chars().count() <= CARD_LABEL_CHARS && l.ends_with("#1466") && l.contains('…'), "{l}");
        assert_eq!(label(&pr(PrState::Open), CARD_LABEL_CHARS), "bellhop#1466");
        assert_eq!(label(&pr(PrState::Open), 9), "bel…#1466");
    }

    #[test]
    fn tooltip_carries_title_state_and_checks() {
        assert_eq!(tip(&pr(PrState::Open)), "Spoke sizing · open · 3/3 checks passing — click to open");
        assert_eq!(tip(&pr(PrState::Merged)), "Spoke sizing · merged — click to open");
    }

    #[test]
    fn no_prs_no_chips_and_overflow_adds_one_more_chip() {
        assert!(chips("s", &[], 3, CARD_LABEL_CHARS, true).is_none());
        assert!(chips("s", &[pr(PrState::Open)], 3, CARD_LABEL_CHARS, true).is_some());
    }

    fn prs(n: usize) -> Vec<SessionPr> {
        (0..n).map(|i| SessionPr { number: 1000 + i as u64, ..pr(PrState::Open) }).collect()
    }

    #[test]
    fn plan_folds_chips_before_the_name_gives_way() {
        let three = prs(3);
        let name_px = text_px("mlops-5578-free-nodes-by-size", 11.0);
        let wide = name_px + 400.0;
        assert_eq!(plan(&three, name_px, wide, 2, 14, Some(8)), ChipPlan::Chips { max: 2, label_chars: 14 });
        let one = name_px + ChipPlan::Chips { max: 1, label_chars: 14 }.width(&three);
        assert_eq!(plan(&three, name_px, one, 2, 14, Some(8)), ChipPlan::Chips { max: 1, label_chars: 14 });
        let short = name_px + ChipPlan::Chips { max: 1, label_chars: 8 }.width(&three);
        assert_eq!(plan(&three, name_px, short, 2, 14, Some(8)), ChipPlan::Chips { max: 1, label_chars: 8 });
        assert_eq!(plan(&three, name_px, name_px, 2, 14, Some(8)), ChipPlan::Count);
        assert_eq!(plan(&three, name_px, 0.0, 2, 14, None), ChipPlan::Count);
    }

    #[test]
    fn fit_name_never_exceeds_its_budget() {
        for avail in [40.0, 95.0, 160.0, 400.0] {
            let shown = fit_name("mlops-5870-ray-migrated-source-sweeper", 11.0, avail);
            assert!(shown.chars().count() == 1 || text_px(&shown, 11.0) <= avail, "{shown:?} in {avail}");
        }
        assert_eq!(fit_name("w-parser", 11.0, 400.0), "w-parser");
    }
}
