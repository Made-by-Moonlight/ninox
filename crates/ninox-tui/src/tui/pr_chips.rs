//! Clickable PR chips on fleet and board rows. Line builders add chip
//! spans through [`spans`], which also queues each chip's click target;
//! after the lines are laid out, [`register`] turns the underlined chip
//! spans into screen rects so a left click can hit-test them.

use ninox_core::session_prs::{PrState, SessionPr};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

use super::{palette::Palette, state::{Row, TuiState}};

#[derive(Clone, Debug, PartialEq)]
pub enum ChipTarget {
    /// Open this PR URL in the browser.
    Open(String),
    /// Select this session and show its inspector, which lists every PR.
    More(String),
}

pub fn tone(p: &Palette, pr: &SessionPr) -> Color {
    use ninox_core::pr_events::CiVerdict;
    match pr.state {
        PrState::Merged => p.done,
        PrState::Closed => p.faint,
        PrState::Open if pr.is_failing() => p.ci_failed,
        PrState::Open if pr.changes_requested => p.accent,
        PrState::Open if pr.draft => p.ink_2,
        PrState::Open if pr.ci == CiVerdict::None && pr.total() == 0 => p.faint,
        PrState::Open if pr.is_pending() => p.pr_open,
        PrState::Open => p.working,
    }
}

/// The chip texts and targets for up to `max` PRs, then `+N`.
pub fn chips(session_id: &str, prs: &[SessionPr], max: usize) -> Vec<(String, Option<usize>, ChipTarget)> {
    let shown = prs.len().min(max);
    let mut out: Vec<(String, Option<usize>, ChipTarget)> = prs[..shown]
        .iter()
        .enumerate()
        .map(|(i, pr)| (format!("[{}]", pr.label()), Some(i), ChipTarget::Open(pr.url.clone())))
        .collect();
    if prs.len() > shown {
        out.push((format!("[+{}]", prs.len() - shown), None, ChipTarget::More(session_id.to_string())));
    }
    out
}

/// [`chips`] trimmed from the end until they fit `budget` cells (each chip
/// costs its text plus a leading space).
pub fn fitting(session_id: &str, prs: &[SessionPr], max: usize, budget: usize) -> Vec<(String, Option<usize>, ChipTarget)> {
    let mut out = chips(session_id, prs, max);
    while out.iter().map(|c| c.0.width() + 1).sum::<usize>() > budget && !out.is_empty() {
        out.pop();
    }
    out
}

/// A row whose PRs are drawn as chips shows no plain `PR #n` tag.
pub fn replaces_pr_tag(row: &Row) -> bool {
    row.session.status == ninox_core::types::SessionStatus::PrOpen && !row.prs.is_empty()
}

/// Space-separated chip spans for `row`; each is underlined, and its
/// target is queued on `st` for the next [`register`].
pub fn spans(st: &TuiState, row: &Row, max: usize, extra: Modifier, budget: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    for (text, pr, target) in fitting(row.id(), &row.prs, max, budget) {
        let color = pr.map_or(st.palette.faint, |i| tone(&st.palette, &row.prs[i]));
        out.push(Span::raw(" "));
        out.push(Span::styled(text, Style::default().fg(color).add_modifier(Modifier::UNDERLINED | extra)));
        st.chip_scratch.borrow_mut().push(target);
    }
    out
}

/// Record the screen rect of every chip span in `lines`, which start at
/// `area`'s top-left. A line wider than `area` wraps, so its chips are
/// skipped. A mismatch between chip spans and queued targets (a builder
/// ran without a register) drops the batch.
pub fn register(st: &TuiState, lines: &[Line<'_>], area: Rect, wrap: bool) {
    let targets: Vec<ChipTarget> = std::mem::take(&mut *st.chip_scratch.borrow_mut());
    let is_chip = |s: &Span| s.style.add_modifier.contains(Modifier::UNDERLINED);
    let chip_spans = lines.iter().flat_map(|l| &l.spans).filter(|s| is_chip(s)).count();
    if chip_spans != targets.len() {
        return;
    }
    let mut next = targets.into_iter();
    let mut y = area.y;
    let mut hits = st.chip_hits.borrow_mut();
    for line in lines {
        let width: usize = line.spans.iter().map(|s| s.content.width()).sum();
        let rows = if wrap && area.width > 0 { width.div_ceil(area.width as usize).max(1) } else { 1 };
        let mut x = area.x;
        for s in &line.spans {
            let w = s.content.width() as u16;
            if is_chip(s) {
                let target = next.next();
                let fits = rows == 1 && y < area.y + area.height && x + w <= area.x + area.width;
                if let (true, Some(target)) = (fits, target) {
                    hits.push((Rect { x, y, width: w, height: 1 }, target));
                }
            }
            x = x.saturating_add(w);
        }
        y = y.saturating_add(rows as u16);
    }
}

pub fn hit(st: &TuiState, x: u16, y: u16) -> Option<ChipTarget> {
    st.chip_hits
        .borrow()
        .iter()
        .find(|(r, _)| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height)
        .map(|(_, t)| t.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::pr_events::CiVerdict;

    pub fn pr(repo: &str, n: u64, state: PrState) -> SessionPr {
        SessionPr {
            repo: repo.into(), number: n, url: format!("https://github.com/{repo}/pull/{n}"), title: String::new(),
            state, draft: false, ci: CiVerdict::Passed, passing: 1, failing: 0, pending: 0,
            changes_requested: false, approved: false, tracked: false,
        }
    }

    #[test]
    fn chips_cap_then_fold_the_rest_into_a_session_target() {
        let prs = [pr("o/a", 1, PrState::Open), pr("o/a", 2, PrState::Open), pr("o/a", 3, PrState::Merged)];
        let c = chips("w1", &prs, 1);
        assert_eq!(c.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(), vec!["[a#1]", "[+2]"]);
        assert_eq!(c[0].2, ChipTarget::Open("https://github.com/o/a/pull/1".into()));
        assert_eq!(c[1].2, ChipTarget::More("w1".into()));
        assert_eq!(chips("w1", &prs, 3).len(), 3);
    }

    #[test]
    fn fitting_drops_chips_from_the_end_until_they_fit() {
        let prs = [pr("o/bellhop", 1466, PrState::Open), pr("o/bellhop", 1473, PrState::Open)];
        let texts = |b| fitting("w", &prs, 1, b).into_iter().map(|c| c.0).collect::<Vec<_>>();
        assert_eq!(texts(40), vec!["[bellhop#1466]", "[+1]"]);
        assert_eq!(texts(16), vec!["[bellhop#1466]"]);
        assert!(texts(10).is_empty());
    }

    #[test]
    fn register_maps_chip_spans_to_screen_rects_and_hit_finds_them() {
        let st = TuiState::default();
        let mut row = Row::test_row("w1");
        row.prs = vec![pr("o/bellhop", 1466, PrState::Open), pr("o/bellhop", 1473, PrState::Open)];
        let mut spans = vec![Span::raw("name")];
        spans.extend(super::spans(&st, &row, 1, Modifier::empty(), 40));
        let lines = vec![Line::from("header"), Line::from(spans)];
        register(&st, &lines, Rect { x: 10, y: 5, width: 40, height: 4 }, false);
        // "name" 4 cells, then a space, then "[bellhop#1466]" (14 cells) at x=15, y=6.
        assert_eq!(hit(&st, 15, 6), Some(ChipTarget::Open("https://github.com/o/bellhop/pull/1466".into())));
        assert_eq!(hit(&st, 28, 6), Some(ChipTarget::Open("https://github.com/o/bellhop/pull/1466".into())));
        assert_eq!(hit(&st, 30, 6), Some(ChipTarget::More("w1".into())));
        assert_eq!(hit(&st, 12, 6), None, "the name is not a chip");
        assert_eq!(hit(&st, 15, 5), None, "another line");
        st.chip_hits.borrow_mut().clear();
    }

    #[test]
    fn a_wrapped_line_registers_no_chips_and_a_stray_builder_drops_the_batch() {
        let st = TuiState::default();
        let mut row = Row::test_row("w1");
        row.prs = vec![pr("o/a", 1, PrState::Open)];
        let long = Line::from(
            std::iter::once(Span::raw("x".repeat(30))).chain(super::spans(&st, &row, 1, Modifier::empty(), 40)).collect::<Vec<_>>(),
        );
        register(&st, &[long], Rect { x: 0, y: 0, width: 20, height: 4 }, true);
        assert!(st.chip_hits.borrow().is_empty());

        super::spans(&st, &row, 1, Modifier::empty(), 40);
        register(&st, &[Line::from("no chips here")], Rect { x: 0, y: 0, width: 20, height: 1 }, false);
        assert!(st.chip_hits.borrow().is_empty());
        assert!(st.chip_scratch.borrow().is_empty(), "register always drains the queue");
    }

    #[test]
    fn tones_separate_failing_from_changes_requested() {
        let p = Palette::terminal();
        let mut failing = pr("o/a", 1, PrState::Open);
        failing.failing = 1;
        let mut changes = pr("o/a", 2, PrState::Open);
        changes.changes_requested = true;
        assert_ne!(tone(&p, &failing), tone(&p, &changes));
        assert_ne!(tone(&p, &changes), tone(&p, &pr("o/a", 3, PrState::Open)));
    }
}
