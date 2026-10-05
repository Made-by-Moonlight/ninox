//! Frame geometry, computed once per frame from state so drawing, mouse
//! hit-testing and pane resizing all agree.

use ratatui::layout::Rect;
use ratatui::widgets::{Block, Borders};

use super::report::{self, ReportButton};
use super::state::{Focus, Modal, SideItem, TuiState, View};

/// Sidebar width bounds; it takes 30% of the body in between.
pub const SIDEBAR_MIN: u16 = 28;
pub const SIDEBAR_MAX: u16 = 44;
/// Below this body width the sidebar and the pane take turns full-width
/// (whichever has focus) instead of squeezing side by side.
pub const MIN_SPLIT_W: u16 = 64;
/// Cells of a group header's fold chevron (`▾ `); a click there folds.
pub const CHEVRON_W: u16 = 2;
/// Pane header row plus the rule under it.
pub const PANE_HEADER_H: u16 = 2;
pub const INSPECTOR_WIDTH: u16 = 42;
const MIN_TILE_W: u16 = 28;
const MIN_TILE_H: u16 = 7;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tile {
    /// Index into `TuiState::overview_rows()`.
    pub index: usize,
    pub outer: Rect,
    pub inner: Rect,
}

/// The header's view tabs, in order; `1`..`5` select them.
pub const TABS: [(View, &str); 5] = [
    (View::Board, "Fleet"),
    (View::Overview, "Overview"),
    (View::PrWatches, "PRs"),
    (View::Brain, "Brain"),
    (View::Settings, "Settings"),
];
/// Width of the header's `⬡ ninox` mark, its padding and the gap after it.
const BADGE_W: u16 = 10;
/// Room kept for the header's right-aligned `ptyd ● engine ●`; at 80
/// columns this is what lets all five tabs fit.
const STATUS_W: u16 = 16;
/// The pane title's clickable "back to the fleet" button.
pub const BACK_LABEL: &str = " ◀ fleet ";
/// Pane title buttons after `◀ fleet`, for a session with a live process:
/// kill it (asks first) and zoom. They are what works when no prefix chord
/// reaches the TUI (macOS can claim Ctrl-Space).
pub const KILL_LABEL: &str = " ✕ kill ";
pub const ZOOM_LABEL: &str = " ⤢ zoom ";
/// Cells kept for the session name beside the pane title buttons.
const PANE_TITLE_MIN: u16 = 16;
/// The selected sidebar row's `✕` (kill/remove) at its right edge: a gap
/// cell and the glyph.
pub const ROW_CLOSE_W: u16 = 2;
/// Settings: the label column, before the values.
pub const SETTINGS_LABEL_W: u16 = 36;
const SETTINGS_LIST_MAX: u16 = 80;
const SETTINGS_HELP_MIN: u16 = 34;
pub const OPEN_CONFIG_LABEL: &str = "[ e Open in $EDITOR ]";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tab {
    pub view: View,
    /// The digit that selects it.
    pub key: char,
    pub label: &'static str,
    pub rect: Rect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrainButton {
    New,
    Edit,
    Delete,
}

impl BrainButton {
    pub fn text(self) -> &'static str {
        match self {
            Self::New => "[ a New entry ]",
            Self::Edit => "[ e Edit ]",
            Self::Delete => "[ D Delete ]",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BrainLayout {
    pub search: Rect,
    pub list: Rect,
    /// The reading pane; `doc_body` is its scrolling text under the button
    /// row.
    pub doc: Rect,
    pub doc_body: Rect,
    /// `[ a New entry ]` at the search line's right; `[ e Edit ]` and
    /// `[ D Delete ]` atop the reading pane while an entry is selected.
    pub buttons: Vec<(BrainButton, Rect)>,
    pub items: Vec<super::brain::Item>,
    /// `items[list_offset..]` are drawn.
    pub list_offset: usize,
    pub cursor: usize,
}

pub const PR_OPEN_LABEL: &str = "[ ↗ Open ]";
/// A PR line's `#number` column.
pub const PR_NUMBER_W: u16 = 7;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PrLayout {
    /// Counts and whether PR watching is on.
    pub summary: Rect,
    pub list: Rect,
    pub lines: Vec<super::prs::Line>,
    /// `lines[offset..]` are drawn.
    pub offset: usize,
    /// Per drawn PR (`rows` index): its `#number`, its `[ ↗ Open ]` (both
    /// open it) and its session name (shows the session).
    pub numbers: Vec<(usize, Rect)>,
    pub opens: Vec<(usize, Rect)>,
    pub sessions: Vec<(usize, Rect)>,
    /// Width of the session column; 0 when there is no room for it.
    pub session_w: u16,
}

/// One line of the settings list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsLine {
    Heading(&'static str),
    Gap,
    /// `settings.fields[i]`.
    Field(usize),
    /// The refusal of the last change, under its field.
    Error,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SettingsLayout {
    /// The file path line; `open` is its "open in $EDITOR" button.
    pub path: Rect,
    pub open: Rect,
    pub list: Rect,
    pub lines: Vec<SettingsLine>,
    /// `lines[offset..]` are drawn.
    pub offset: usize,
    /// Where the value column starts; a click from here on changes it.
    pub value_x: u16,
    /// The selected field's description.
    pub help: Rect,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Layout {
    pub header: Rect,
    pub tabs: Vec<Tab>,
    /// Where the header's own text (agent counts) starts, after the tabs.
    pub header_text_x: u16,
    pub body: Rect,
    pub footer: Rect,
    pub sidebar: Option<Rect>,
    /// The pinned "Needs you" block at the top of the sidebar, when any
    /// session needs attention; `needs_items` are its lines.
    pub sidebar_needs: Option<Rect>,
    pub needs_items: Vec<SideItem>,
    /// The fleet tree below it; `sidebar_items[sidebar_offset..]` are drawn.
    pub sidebar_list: Option<Rect>,
    pub sidebar_items: Vec<SideItem>,
    pub sidebar_offset: usize,
    pub pane: Option<Rect>,
    pub pane_inner: Option<Rect>,
    pub pane_back: Option<Rect>,
    pub pane_kill: Option<Rect>,
    pub pane_zoom: Option<Rect>,
    /// The selected sidebar row's `✕`.
    pub sidebar_close: Option<Rect>,
    pub settings: Option<SettingsLayout>,
    pub brain: Option<BrainLayout>,
    pub prs: Option<PrLayout>,
    pub inspector: Option<Rect>,
    pub tiles: Vec<Tile>,
    pub overview_cols: u16,
    /// An ended session's report in the pane: its buttons, the clickable
    /// PR lines, the scrolling body under the pinned head, and how far
    /// that body can scroll.
    pub report_buttons: Vec<(ReportButton, Rect)>,
    pub report_links: Vec<Rect>,
    pub report_body: Option<Rect>,
    pub report_max_scroll: u16,
    /// The confirm modal's `[ y Yes ]` and `[ n No ]`.
    pub modal_buttons: Option<(Rect, Rect)>,
    /// Its `[ a Remove all ]`, when offered.
    pub modal_all: Option<Rect>,
}

pub const YES_LABEL: &str = "[ y Yes ]";
pub const NO_LABEL: &str = "[ n No ]";
pub const ALL_LABEL: &str = "[ a Remove all ]";

/// Geometry of the confirm modal for a question `text_w` cells wide: the
/// box, then its Yes and No buttons. Content rows are the question, a
/// detail line, a gap and the buttons.
pub fn confirm_rects(area: Rect, text_w: u16, with_all: bool) -> (Rect, Rect, Rect, Option<Rect>) {
    let all_w = if with_all { ALL_LABEL.len() + 2 } else { 0 };
    let buttons_w = (YES_LABEL.len() + 2 + NO_LABEL.len() + all_w) as u16;
    let w = (text_w.max(buttons_w) + 4).min(area.width);
    let h = 6u16.min(area.height);
    let r = Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h };
    let (cx, cy, cw) = (r.x + 2, r.y + 4, w.saturating_sub(4));
    let clip = |x: u16, len: u16| Rect { x: x.min(cx + cw), y: cy, width: len.min((cx + cw).saturating_sub(x)), height: u16::from(cy + 1 < r.bottom()) };
    let yes = clip(cx, YES_LABEL.len() as u16);
    let no = clip(cx + YES_LABEL.len() as u16 + 2, NO_LABEL.len() as u16);
    let all = with_all.then(|| clip(no.x + NO_LABEL.len() as u16 + 2, ALL_LABEL.len() as u16));
    (r, yes, no, all)
}

pub fn compute(area: Rect, st: &TuiState) -> Layout {
    let mut lay = Layout {
        header: Rect { height: area.height.min(1), ..area },
        footer: Rect { y: area.bottom().saturating_sub(1), height: area.height.min(1), ..area },
        ..Default::default()
    };
    // A blank row of breathing room under the header/tab bar, above the
    // footer; every view's content shares this top margin.
    let body = Rect { y: area.y + 2, height: area.height.saturating_sub(3), ..area };
    lay.body = body;
    tabs(&mut lay);
    match st.view {
        View::Board => board(&mut lay, body, st),
        View::Overview => overview(&mut lay, body, st),
        View::Brain => lay.brain = Some(brain(body, st)),
        View::Settings => lay.settings = Some(settings(body, st)),
        View::PrWatches => lay.prs = Some(prs(body, st)),
    }
    if let Some(Modal::Confirm(pending)) = &st.modal {
        let (q, detail) = st.confirm_text(pending);
        let tw = unicode_width::UnicodeWidthStr::width(q.as_str()).max(unicode_width::UnicodeWidthStr::width(detail.as_str())) as u16;
        let (_, yes, no, all) = confirm_rects(area, tw, st.confirm_offers_all(pending));
        lay.modal_buttons = Some((yes, no));
        lay.modal_all = all;
    }
    lay
}

fn tabs(lay: &mut Layout) {
    let h = lay.header;
    let mut x = h.x + BADGE_W;
    let limit = h.right().saturating_sub(STATUS_W);
    for (key, (view, label)) in ('1'..).zip(TABS) {
        // " 1 Fleet "
        let w = label.chars().count() as u16 + 4;
        if h.height == 0 || x + w > limit {
            break;
        }
        lay.tabs.push(Tab { view, key, label, rect: Rect { x, y: h.y, width: w, height: 1 } });
        x += w + 1;
    }
    lay.header_text_x = x.min(h.right());
}

fn brain(body: Rect, st: &TuiState) -> BrainLayout {
    let pad = Rect { x: body.x + 1, width: body.width.saturating_sub(2), ..body };
    let search = Rect { height: pad.height.min(1), ..pad };
    let gap = pad.height.min(2);
    let rest = Rect { y: pad.y + gap, height: pad.height - gap, ..pad };
    let list_w = (rest.width / 3).clamp(20, 44).min(rest.width);
    let list = Rect { width: list_w, ..rest };
    // A rule column and a blank one sit between the list and the doc.
    let doc = Rect { x: rest.x + list_w + 3, width: rest.width.saturating_sub(list_w + 3), ..rest };
    let mut buttons = Vec::new();
    let new_w = BrainButton::New.text().len() as u16;
    if search.height > 0 && search.width >= new_w + 30 {
        buttons.push((BrainButton::New, Rect { x: search.right() - new_w, width: new_w, ..search }));
    }
    let b = &st.brain;
    let has_entry = b.error.is_none() && b.selected_entry().is_some();
    let head = if has_entry && doc.height > 2 { 2 } else { 0 };
    if head > 0 {
        let mut x = doc.x;
        for button in [BrainButton::Edit, BrainButton::Delete] {
            let w = button.text().len() as u16;
            if x + w > doc.right() {
                break;
            }
            buttons.push((button, Rect { x, y: doc.y, width: w, height: 1 }));
            x += w + 2;
        }
    }
    let doc_body = Rect { y: doc.y + head, height: doc.height - head, ..doc };
    let items = b.items();
    let cursor = b.cursor_index(&items);
    let h = list.height as usize;
    let list_offset = cursor.saturating_sub(h.saturating_sub(1));
    BrainLayout { search, list, doc, doc_body, buttons, items, list_offset, cursor }
}

fn prs(body: Rect, st: &TuiState) -> PrLayout {
    use super::prs::Line;
    let pad = Rect { x: body.x + 1, width: body.width.saturating_sub(2), ..body };
    let summary = Rect { height: pad.height.min(1), ..pad };
    let gap = pad.height.min(2);
    let list = Rect { y: pad.y + gap, height: pad.height - gap, ..pad };
    let lines = super::prs::lines(&st.prs.rows);
    let sel = st.prs.selected();
    let at = lines.iter().position(|l| *l == Line::Pr(sel)).unwrap_or(0);
    let h = list.height as usize;
    // Keep the selected PR's repo header in view when it fits.
    let header = lines[..at].iter().rposition(|l| matches!(l, Line::Repo { .. })).unwrap_or(0);
    let mut offset = at.saturating_sub(h.saturating_sub(1));
    if header < offset && at - header < h {
        offset = header;
    }
    let open_w = unicode_width::UnicodeWidthStr::width(PR_OPEN_LABEL) as u16;
    let show_open = list.width >= 40;
    let session_w = if list.width >= 70 { (list.width / 5).clamp(12, 28) } else { 0 };
    let (mut numbers, mut opens, mut sessions) = (Vec::new(), Vec::new(), Vec::new());
    for (k, line) in lines.iter().enumerate().skip(offset).take(h) {
        let Line::Pr(i) = *line else { continue };
        let y = list.y + (k - offset) as u16;
        let row = Rect { y, height: 1, ..list };
        numbers.push((i, Rect { x: row.x + 2, width: PR_NUMBER_W.min(row.width.saturating_sub(2)), ..row }));
        let mut right = row.right();
        if show_open && st.prs.rows[i].url.is_some() {
            right -= open_w;
            opens.push((i, Rect { x: right, width: open_w, ..row }));
        }
        if session_w > 0 && st.prs.rows[i].session.is_some() {
            sessions.push((i, Rect { x: right.saturating_sub(session_w + 2), width: session_w, ..row }));
        }
    }
    PrLayout { summary, list, lines, offset, numbers, opens, sessions, session_w }
}

pub fn sidebar_width(body_w: u16) -> u16 {
    (body_w * 3 / 10).clamp(SIDEBAR_MIN, SIDEBAR_MAX)
}

fn board(lay: &mut Layout, body: Rect, st: &TuiState) {
    let mut rest = body;
    let split = body.width >= MIN_SPLIT_W;
    let show_sidebar = !st.zoom && (split || st.focus == Focus::Sidebar || st.selected_row().is_none());
    if show_sidebar {
        let w = if split { sidebar_width(body.width) } else { body.width };
        let side = Rect { width: w, ..body };
        // 1-cell padding each side; in a split the last column is the rule.
        let content = Rect { x: side.x + 1, width: w.saturating_sub(if split { 3 } else { 2 }), ..side };
        sidebar(lay, content, st);
        lay.sidebar = Some(side);
        if !split {
            return;
        }
        rest = Rect { x: body.x + w, width: body.width - w, ..body };
    }
    if st.inspector && !st.zoom && rest.width >= INSPECTOR_WIDTH + 30 {
        let insp = Rect { x: rest.right() - INSPECTOR_WIDTH, width: INSPECTOR_WIDTH, ..rest };
        lay.inspector = Some(insp);
        rest.width -= INSPECTOR_WIDTH;
    }
    if rest.width >= 3 && rest.height > PANE_HEADER_H {
        lay.pane = Some(rest);
        lay.pane_inner = Some(Rect {
            x: rest.x + 1,
            y: rest.y + PANE_HEADER_H,
            width: rest.width - 2,
            height: rest.height - PANE_HEADER_H,
        });
        let w = BACK_LABEL.chars().count() as u16;
        if rest.width >= w + 2 {
            lay.pane_back = Some(Rect { x: rest.x + 1, y: rest.y, width: w, height: 1 });
        }
        let (kw, zw) = (KILL_LABEL.chars().count() as u16, ZOOM_LABEL.chars().count() as u16);
        let live = st.selected_row().is_some_and(|r| !report::shows_report(st, r));
        if live && rest.width >= w + 1 + kw + 1 + zw + PANE_TITLE_MIN {
            let kill = Rect { x: rest.x + w + 1, y: rest.y, width: kw, height: 1 };
            lay.pane_kill = Some(kill);
            lay.pane_zoom = Some(Rect { x: kill.right() + 1, width: zw, ..kill });
        }
        if let (Some(inner), Some(r)) = (lay.pane_inner, st.selected_row().filter(|r| report::shows_report(st, r))) {
            report_geometry(lay, inner, st, r);
        }
    }
}

/// The report's pinned head (with its buttons) and scrolling body.
fn report_geometry(lay: &mut Layout, inner: Rect, st: &TuiState, r: &super::state::Row) {
    let plan = report::plan(st, r, inner.width);
    if report::BUTTON_ROW < inner.height {
        let y = inner.y + report::BUTTON_ROW;
        lay.report_buttons = plan.buttons.iter().map(|&(b, x, w)| (b, Rect { x: inner.x + x, y, width: w, height: 1 })).collect();
    }
    let head = report::HEAD_H.min(inner.height);
    let body = Rect { y: inner.y + head, height: inner.height - head, ..inner };
    lay.report_max_scroll = (plan.body.len() as u16).saturating_sub(body.height);
    let scroll = st.report_scroll.min(lay.report_max_scroll) as usize;
    lay.report_links = plan
        .link_lines
        .iter()
        .filter(|&&i| i >= scroll && i - scroll < body.height as usize)
        .map(|&i| Rect { y: body.y + (i - scroll) as u16, height: 1, ..body })
        .collect();
    lay.report_body = Some(body);
}

/// Splits the sidebar into the pinned "Needs you" block (at most half the
/// height, cut short with a "+n more" line) and the scrolling fleet tree.
fn sidebar(lay: &mut Layout, area: Rect, st: &TuiState) {
    let mut list = area;
    let needs = st.needs_rows();
    let budget = (area.height / 2) as usize;
    if !needs.is_empty() && budget >= 3 {
        let mut items = vec![SideItem::Heading(super::state::Section::NeedsYou)];
        // Heading + rows + gap, or heading + rows + "+n more" + gap.
        let fits = needs.len() + 2 <= budget;
        let shown = if fits { needs.len() } else { budget.saturating_sub(3).max(1) };
        items.extend(needs.iter().take(shown).map(|&row| SideItem::Session { row, pinned: true }));
        if shown < needs.len() {
            items.push(SideItem::More(needs.len() - shown));
        }
        items.push(SideItem::Gap);
        let h = (items.len() as u16).min(area.height);
        lay.sidebar_needs = Some(Rect { height: h, ..area });
        lay.needs_items = items;
        list = Rect { y: area.y + h, height: area.height - h, ..area };
    }
    let items = st.sidebar_items();
    let sel = items.iter().position(|it| *it == SideItem::Session { row: st.selected, pinned: false }).unwrap_or(0);
    let h = list.height.max(1) as usize;
    lay.sidebar_offset = sel.saturating_sub(h - 1);
    lay.sidebar_items = items;
    lay.sidebar_list = Some(list);
    let pinned = st.cursor_pinned();
    let here = SideItem::Session { row: st.selected, pinned };
    let y = match (pinned, lay.sidebar_needs) {
        (true, Some(n)) => lay.needs_items.iter().position(|it| *it == here).map(|k| n.y + k as u16).filter(|&y| y < n.bottom()),
        _ => sel.checked_sub(lay.sidebar_offset).filter(|&k| k < list.height as usize).map(|k| list.y + k as u16),
    };
    if let Some(y) = y.filter(|_| area.width >= 20 && !st.rows.is_empty()) {
        lay.sidebar_close = Some(Rect { x: area.right() - ROW_CLOSE_W, y, width: ROW_CLOSE_W, height: 1 });
    }
}

/// The settings lines: fields grouped under their section headings, with
/// the last refusal under the selected field.
pub fn settings_lines(st: &TuiState) -> Vec<SettingsLine> {
    let v = &st.settings;
    let mut out = Vec::new();
    let mut section = None;
    for (i, f) in v.fields.iter().enumerate() {
        if section != Some(f.section) {
            if section.is_some() {
                out.push(SettingsLine::Gap);
            }
            out.push(SettingsLine::Heading(f.section));
            section = Some(f.section);
        }
        out.push(SettingsLine::Field(i));
        if i == v.selected && v.error.is_some() {
            out.push(SettingsLine::Error);
        }
    }
    out
}

/// Path line with its button, a gap (or the parse error), then the list;
/// the selected field's description sits right of the list when there is
/// room, else under it.
fn settings(body: Rect, st: &TuiState) -> SettingsLayout {
    let pad = Rect { x: body.x + 1, width: body.width.saturating_sub(2), ..body };
    let path = Rect { height: pad.height.min(1), ..pad };
    let bw = (OPEN_CONFIG_LABEL.len() as u16).min(path.width);
    let open = Rect { x: path.right() - bw, width: bw, ..path };
    let top = pad.height.min(2);
    let rest = Rect { y: pad.y + top, height: pad.height - top, ..pad };
    let list_w = rest.width.min(SETTINGS_LIST_MAX);
    let (list, help) = if rest.width >= list_w + 3 + SETTINGS_HELP_MIN {
        let help = Rect { x: rest.x + list_w + 3, width: rest.width - list_w - 3, ..rest };
        (Rect { width: list_w, ..rest }, help)
    } else {
        let hh = rest.height.min(if rest.height >= 12 { 5 } else { 0 });
        let gap = u16::from(hh > 0);
        let list = Rect { width: list_w, height: rest.height.saturating_sub(hh + gap), ..rest };
        (list, Rect { y: list.bottom() + gap, height: hh, ..rest })
    };
    let lines = settings_lines(st);
    let sel = lines.iter().position(|l| *l == SettingsLine::Field(st.settings.selected)).unwrap_or(0);
    let h = list.height.max(1) as usize;
    // One more line under the selection, so its error stays in view.
    let offset = (sel + 2).saturating_sub(h).min(lines.len().saturating_sub(h));
    let value_x = (list.x + 2 + SETTINGS_LABEL_W).min(list.right());
    SettingsLayout { path, open, list, lines, offset, value_x, help }
}

fn overview(lay: &mut Layout, body: Rect, st: &TuiState) {
    let n = st.overview_rows().len();
    if n == 0 || body.width < MIN_TILE_W || body.height < MIN_TILE_H {
        lay.overview_cols = 1;
        return;
    }
    let max_cols = (body.width / MIN_TILE_W).max(1) as usize;
    let max_rows = (body.height / MIN_TILE_H).max(1) as usize;
    let mut cols = (n as f64).sqrt().ceil() as usize;
    // Wide terminals favour more columns than rows.
    while cols < max_cols && n.div_ceil(cols) > max_rows {
        cols += 1;
    }
    let cols = cols.clamp(1, max_cols);
    let rows = n.div_ceil(cols).clamp(1, max_rows);
    let per_page = cols * rows;
    let page = st.overview_sel.min(n - 1) / per_page;
    let first = page * per_page;
    let shown = (n - first).min(per_page);
    let shown_rows = shown.div_ceil(cols);
    lay.overview_cols = cols as u16;
    let tw = body.width / cols as u16;
    let th = body.height / shown_rows as u16;
    // A cell of breathing room between tiles, when there's slack to spare.
    let gap_x = u16::from(tw > MIN_TILE_W);
    let gap_y = u16::from(th > MIN_TILE_H);
    for k in 0..shown {
        let (c, r) = ((k % cols) as u16, (k / cols) as u16);
        let last_col = c as usize == cols - 1;
        let last_row = r as usize == shown_rows - 1;
        let w = (if last_col { body.width - tw * c } else { tw }) - if last_col { 0 } else { gap_x };
        let h = (if last_row { body.height - th * r } else { th }) - if last_row { 0 } else { gap_y };
        let outer = Rect { x: body.x + tw * c, y: body.y + th * r, width: w, height: h };
        let inner = Block::default().borders(Borders::ALL).inner(outer);
        let inner = Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner };
        lay.tiles.push(Tile { index: first + k, outer, inner });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::state::{Row, TuiState};

    fn st(n: usize) -> TuiState {
        TuiState { rows: (0..n).map(|i| Row::test_row(&format!("s{i}"))).collect(), ..Default::default() }
    }

    #[test]
    fn board_splits_sidebar_and_pane() {
        let s = st(3);
        let lay = compute(Rect::new(0, 0, 120, 40), &s);
        let side = lay.sidebar.unwrap();
        let pane = lay.pane.unwrap();
        assert_eq!(side.width, 36);
        assert_eq!(pane.x, 36);
        assert_eq!(pane.right(), 120);
        // Under the header's breathing room, then the pane's header row and
        // rule, padded a cell either side.
        assert_eq!(lay.pane_inner.unwrap(), Rect::new(37, 4, 82, 35));
        assert_eq!(compute(Rect::new(0, 0, 300, 40), &s).sidebar.unwrap().width, SIDEBAR_MAX);
    }

    #[test]
    fn narrow_terminals_show_sidebar_or_pane_full_width_by_focus() {
        let mut s = st(3);
        let lay = compute(Rect::new(0, 0, 40, 10), &s);
        assert_eq!(lay.sidebar.unwrap().width, 40);
        assert!(lay.pane.is_none());
        s.focus = Focus::Pane;
        let lay = compute(Rect::new(0, 0, 40, 10), &s);
        assert!(lay.sidebar.is_none());
        assert_eq!(lay.pane.unwrap().width, 40);
        assert!(lay.pane_back.is_some(), "◀ fleet is the way back");
    }

    #[test]
    fn needs_you_is_pinned_above_the_scrolling_tree() {
        let mut s = st(40);
        for r in s.rows.iter_mut().step_by(4) {
            r.session.activity = ninox_core::types::ActivityState::Blocked;
        }
        s.selected = 39;
        let lay = compute(Rect::new(0, 0, 120, 24), &s);
        let (needs, list) = (lay.sidebar_needs.unwrap(), lay.sidebar_list.unwrap());
        assert_eq!(needs.y, lay.body.y);
        assert_eq!(list.y, needs.bottom());
        assert!(needs.height <= lay.body.height / 2);
        assert!(matches!(lay.needs_items.iter().rev().nth(1), Some(SideItem::More(_))), "{:?}", lay.needs_items);
        assert!(lay.sidebar_offset > 0, "the tree scrolls to the selection while needs stay put");
    }

    #[test]
    fn zoom_hides_sidebar_and_inspector() {
        let mut s = st(3);
        s.zoom = true;
        s.inspector = true;
        let lay = compute(Rect::new(0, 0, 120, 40), &s);
        assert!(lay.sidebar.is_none() && lay.inspector.is_none());
        assert_eq!(lay.pane.unwrap().width, 120);
    }

    #[test]
    fn inspector_takes_the_right_edge() {
        let mut s = st(1);
        s.inspector = true;
        let lay = compute(Rect::new(0, 0, 160, 40), &s);
        assert_eq!(lay.inspector.unwrap().right(), 160);
        assert_eq!(lay.pane.unwrap().right(), 160 - INSPECTOR_WIDTH);
    }

    #[test]
    fn sidebar_scrolls_to_keep_selection_visible() {
        let mut s = st(50);
        s.selected = 45;
        let lay = compute(Rect::new(0, 0, 120, 20), &s);
        let h = lay.sidebar_list.unwrap().height as usize;
        assert!(lay.sidebar_offset <= 45 && 45 < lay.sidebar_offset + h);
    }

    #[test]
    fn overview_grid_tiles_cover_the_body_without_overlap() {
        let mut s = st(5);
        s.view = View::Overview;
        let lay = compute(Rect::new(0, 0, 120, 40), &s);
        assert_eq!(lay.tiles.len(), 5);
        assert_eq!(lay.overview_cols, 3);
        for (i, a) in lay.tiles.iter().enumerate() {
            assert!(a.outer.width >= MIN_TILE_W && a.outer.height >= MIN_TILE_H);
            for b in &lay.tiles[i + 1..] {
                assert!(a.outer.intersection(b.outer).is_empty());
            }
        }
    }

    #[test]
    fn overview_survives_a_selection_past_a_shrunken_list() {
        let mut s = st(3);
        s.view = View::Overview;
        s.overview_sel = 39;
        let lay = compute(Rect::new(0, 0, 90, 30), &s);
        assert_eq!(lay.tiles.len(), 3);
    }

    #[test]
    fn header_tabs_are_laid_out_in_order_without_overlap() {
        let lay = compute(Rect::new(0, 0, 120, 40), &st(1));
        let views: Vec<View> = lay.tabs.iter().map(|t| t.view).collect();
        assert_eq!(views, [View::Board, View::Overview, View::PrWatches, View::Brain, View::Settings]);
        for w in lay.tabs.windows(2) {
            assert!(w[0].rect.right() < w[1].rect.x);
        }
        assert!(lay.header_text_x >= lay.tabs.last().unwrap().rect.right());
        let narrow = compute(Rect::new(0, 0, 50, 10), &st(1));
        assert!(narrow.tabs.len() < 5, "tabs that do not fit are dropped, not overlapped");
        assert!(narrow.tabs.iter().all(|t| t.rect.right() <= 50 - STATUS_W));
    }

    #[test]
    fn pane_title_has_a_back_button() {
        let lay = compute(Rect::new(0, 0, 120, 40), &st(1));
        let pane = lay.pane.unwrap();
        assert_eq!(lay.pane_back, Some(Rect::new(pane.x + 1, pane.y, 9, 1)));
    }

    #[test]
    fn overview_pages_when_too_many_agents() {
        let mut s = st(40);
        s.view = View::Overview;
        s.overview_sel = 39;
        let lay = compute(Rect::new(0, 0, 90, 30), &s);
        assert!(lay.tiles.len() < 40);
        assert!(lay.tiles.iter().any(|t| t.index == 39));
    }
}
