//! Frame geometry, computed once per frame from state so drawing, mouse
//! hit-testing and pane resizing all agree.

use ninox_ui_shared::settings_form::SettingsSection;
use ratatui::layout::Rect;

use ninox_overview::islands::IslandKey;

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

/// The header's view tabs, in order; `1`..`6` select them. Labels are kept
/// short deliberately — see `tabs()`'s comment on the width budget. `Types`
/// (not "Worker Types") is the tab-bar label per the fleet-overview-ui-
/// vision-design spec's Navigation section; the `?` help overlay spells the
/// screen's full name out.
pub const TABS: [(View, &str); 6] = [
    (View::Fleet, "Fleet"),
    (View::Board, "Board"),
    (View::Plans, "Plans"),
    (View::Brain, "Brain"),
    (View::WorkerTypes, "Types"),
    (View::Remotes, "Remotes"),
];
/// The header's ` ⚙ s ` entry: a gap, the glyph, the key, a gap.
pub const SETTINGS_BUTTON_W: u16 = 5;
/// Width of the header's `⬡ ninox` mark, its padding and the gap after it.
const BADGE_W: u16 = 10;
/// Room kept for the header's right-aligned `ptyd ● engine ●`, when all
/// tabs fit without borrowing from it — see `tabs()`.
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
    /// The backlinks/related/outlinks panel, carved out of `doc_body`'s
    /// bottom when the selected entry has any and there's room; zero
    /// height otherwise (`doc_body` then keeps the full area).
    pub links: Rect,
    /// `[ a New entry ]` at the search line's right; `[ e Edit ]` and
    /// `[ D Delete ]` atop the reading pane while an entry is selected.
    pub buttons: Vec<(BrainButton, Rect)>,
    pub items: Vec<super::brain::Item>,
    /// `items[list_offset..]` are drawn.
    pub list_offset: usize,
    pub cursor: usize,
    /// The Neighbourhood graph canvas (`Tab` toggles into it) — the whole
    /// body below a one-line breadcrumb; zero height outside graph mode.
    pub graph: Rect,
}

/// The Fleet tab's regions: `tree` is always on (collapsible to a rail);
/// `toolbar` is the one-line `sort: … · view: … · finished: …` + key-hints
/// strip above `main` (the islands grid, or the opened island's full tree);
/// `detail` only appears once an island is open *and* a node in it is
/// selected (mirrors `ninox-gui`'s `fleet_detail::context`), and only if
/// there's room once `tree`/`main`/`prompt` are laid out.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FleetLayout {
    pub tree: Rect,
    pub toolbar: Rect,
    pub main: Rect,
    /// Always-visible 3-row bordered ninox-prompt box, docked under `main`.
    pub prompt: Rect,
    /// The collapsed reply panel's one-line indicator, directly above
    /// `prompt` — submit/clear feedback must never be silent (T3).
    pub reply_strip: Option<Rect>,
    /// The open reply panel, overlaying the bottom of `main` down to the
    /// prompt box.
    pub reply_panel: Option<Rect>,
    pub detail: Option<Rect>,
    /// Each island box's on-screen `Rect` within `main`, in grid (not sort)
    /// order — `None` while an island is open (the main area shows the
    /// opened island's own rataflow tree instead). Only *fully* visible
    /// boxes are included (a box scrolled half on/off `main` is left out
    /// entirely rather than drawn clipped) — this doubles as the candidate
    /// set `fleet::nearest_island_in_direction` picks from, so a selection
    /// is always already fully visible right after a spatial-nav move.
    pub grid: Vec<(IslandKey, Rect)>,
}

/// Cells between adjacent island-grid columns.
const GRID_COL_GAP: u16 = 1;

/// Lays out the islands grid within `area` (mirrors `view.rs`'s
/// `draw_island_box`/`island_rows` exactly — each box is
/// `fleet::island_row_count(..) + 2` rows tall), via `fleet::column_count`/
/// `fleet::column_positions` (the same packing `fleet::scroll_to_show_island`
/// builds on, so a tree-driven selection can never disagree with where the
/// grid actually draws it), scrolled down by `st.fleet.grid_scroll` box-rows
/// and clipped to whole boxes only.
fn fleet_grid(area: Rect, st: &TuiState) -> Vec<(IslandKey, Rect)> {
    if area.width == 0 || area.height == 0 || st.fleet.islands.is_empty() {
        return Vec::new();
    }
    let columns = super::fleet::column_count(area.width);
    let heights = super::fleet::island_grid_heights(&st.fleet.islands, |k| st.fleet.island_fold_expanded(k));
    let positions = super::fleet::column_positions(&heights, columns);
    let col_w = area.width / columns as u16;
    let mut out = Vec::new();
    for (ci, col) in positions.iter().enumerate() {
        let col_area = Rect { x: area.x + ci as u16 * col_w, width: col_w.saturating_sub(GRID_COL_GAP), ..area };
        for &(island, y_off, h) in col {
            let y = col_area.y as i32 + y_off - st.fleet.grid_scroll as i32;
            if y >= col_area.y as i32 && y + h <= col_area.bottom() as i32 {
                out.push((island.key.clone(), Rect { x: col_area.x, y: y as u16, width: col_area.width, height: h as u16 }));
            }
        }
    }
    out
}

/// The Fleet tab: tree | toolbar + islands/opened-tree + prompt | detail.
/// `collapsed` hides the tree entirely (zero width, the `[` key — starts
/// collapsed on the Fleet tab, since the islands grid already shows every
/// orchestrator) so the centre reclaims its full width, rather than
/// shrinking it to a rail that has no room to show anything useful.
fn fleet(body: Rect, st: &TuiState, collapsed: bool) -> FleetLayout {
    if body.width < MIN_SPLIT_W {
        // Below the width the tree needs to split from the main area, the
        // tree takes priority and everything else is hidden rather than
        // squeezed unreadable.
        return FleetLayout { tree: body, ..Default::default() };
    }
    let tree_w = if collapsed { 0 } else { sidebar_width(body.width) };
    let tree = Rect { width: tree_w, ..body };
    let mut rest = Rect { x: body.x + tree_w, width: body.width - tree_w, ..body };
    let has_detail = st.fleet.nav.open.is_some() && st.fleet.nav.selected.is_some();
    let detail = if has_detail && rest.width >= INSPECTOR_WIDTH + 30 {
        let d = Rect { x: rest.right() - INSPECTOR_WIDTH, width: INSPECTOR_WIDTH, ..rest };
        rest.width -= INSPECTOR_WIDTH;
        Some(d)
    } else {
        None
    };
    let toolbar_h = 1u16.min(rest.height);
    // A 3-row bordered box (top rule + content + bottom rule), not the old
    // single dim line — see FEEDBACK-v1-tryout-round1.md's T2.
    let prompt_h = 3u16.min(rest.height.saturating_sub(toolbar_h));
    // One more row for the collapsed reply indicator (T3) — reserved only
    // while there's one to show, so it never costs space otherwise.
    let reply = &st.fleet.reply.drawer;
    let collapsed = reply.has_content() && !reply.is_open();
    let strip_h = if collapsed { 1u16.min(rest.height.saturating_sub(toolbar_h + prompt_h)) } else { 0 };
    let main_h = rest.height.saturating_sub(toolbar_h + prompt_h + strip_h);
    let main = Rect { y: rest.y + toolbar_h, height: main_h, ..rest };
    let grid = if st.fleet.nav.open.is_none() { fleet_grid(main, st) } else { Vec::new() };
    let prompt_y = rest.bottom().saturating_sub(prompt_h);
    FleetLayout {
        tree,
        toolbar: Rect { height: toolbar_h, ..rest },
        main,
        prompt: Rect { y: prompt_y, height: prompt_h, ..rest },
        reply_strip: (strip_h > 0).then(|| Rect { y: prompt_y.saturating_sub(strip_h), height: strip_h, ..rest }),
        reply_panel: reply.is_open().then(|| reply_panel_height(main_h)).filter(|&h| h > 0).map(|h| Rect {
            y: prompt_y.saturating_sub(h),
            height: h,
            ..rest
        }),
        detail,
        grid,
    }
}

/// The board's drawer takes half the window; in a terminal that leaves too
/// little conversation on short screens, so 60% of `main`, at least 8 rows.
fn reply_panel_height(main_h: u16) -> u16 {
    (main_h * 3 / 5).max(8).min(main_h)
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

/// One row of the settings page's section bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsBarLine {
    Heading(&'static str),
    Gap,
    Section(SettingsSection),
}

/// The bar's rows: each group's heading over its sections, a gap between groups.
pub fn settings_bar_lines() -> Vec<SettingsBarLine> {
    let mut out = Vec::new();
    for (i, (group, sections)) in SettingsSection::GROUPS.iter().enumerate() {
        if i > 0 {
            out.push(SettingsBarLine::Gap);
        }
        out.push(SettingsBarLine::Heading(group));
        out.extend(sections.iter().map(|s| SettingsBarLine::Section(*s)));
    }
    out
}

pub const SETTINGS_CLOSE_LABEL: &str = " ✕ close ";
const SETTINGS_BAR_W: u16 = 18;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SettingsLayout {
    /// The version and file path line; `open` is its "open in $EDITOR" button.
    pub path: Rect,
    pub open: Rect,
    /// The left section bar; `bar_lines[bar_offset..]` are drawn.
    pub bar: Rect,
    pub bar_lines: Vec<SettingsBarLine>,
    pub bar_offset: usize,
    /// The selected section's title (and, under it, its blurb), with a
    /// `close` button at the right of the title row.
    pub title: Rect,
    pub close: Rect,
    pub list: Rect,
    pub lines: Vec<SettingsLine>,
    /// `lines[offset..]` are drawn.
    pub offset: usize,
    /// Where the value column starts; a click from here on changes it.
    pub value_x: u16,
    pub label_w: u16,
    /// The selected field's description.
    pub help: Rect,
    pub hint: Rect,
}

/// Where the settings page's content goes inside a `screen`-sized terminal:
/// the full-screen modal's border and padding removed.
pub fn settings_area(screen: Rect) -> Rect {
    Rect { x: screen.x + 2, y: screen.y + 1, width: screen.width.saturating_sub(4), height: screen.height.saturating_sub(2) }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Layout {
    pub header: Rect,
    pub tabs: Vec<Tab>,
    /// The header's `⚙ s` entry, right after the tabs, when it fits.
    pub settings_button: Option<Rect>,
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
    pub fleet: Option<FleetLayout>,
    pub worker_types: Option<WorkerTypesLayout>,
    pub kanban: Option<KanbanLayout>,
    pub remotes: Option<Rect>,
    pub inspector: Option<Rect>,
    /// How far the inspector's Info tab, or an open file's diff, can scroll.
    pub inspector_max_scroll: u16,
    /// How far the help modal's key list scrolls (0: it fits), and half
    /// its box's height (PgUp/PgDn).
    pub help_max_scroll: u16,
    pub help_page: u16,
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

/// Geometry of the confirm modal for a question `text_w` cells wide: the
/// box, then its Yes and No buttons. Content rows are the question, a
/// detail line, a gap and the buttons.
/// `all_label`: the third button's label (e.g. "Remove all"), drawn as
/// `[ a <label> ]`, when the confirm has one.
pub fn confirm_rects(area: Rect, text_w: u16, all_label: Option<&str>) -> (Rect, Rect, Rect, Option<Rect>) {
    let all_len = all_label.map_or(0, |l| "[ a  ]".len() + unicode_width::UnicodeWidthStr::width(l));
    let all_w = if all_label.is_some() { all_len + 2 } else { 0 };
    let buttons_w = (YES_LABEL.len() + 2 + NO_LABEL.len() + all_w) as u16;
    let w = (text_w.max(buttons_w) + 4).min(area.width);
    let h = 6u16.min(area.height);
    let r = Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h };
    let (cx, cy, cw) = (r.x + 2, r.y + 4, w.saturating_sub(4));
    let clip = |x: u16, len: u16| Rect { x: x.min(cx + cw), y: cy, width: len.min((cx + cw).saturating_sub(x)), height: u16::from(cy + 1 < r.bottom()) };
    let yes = clip(cx, YES_LABEL.len() as u16);
    let no = clip(cx + YES_LABEL.len() as u16 + 2, NO_LABEL.len() as u16);
    let all = all_label.map(|_| clip(no.x + NO_LABEL.len() as u16 + 2, all_len as u16));
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
        View::Fleet if matches!(st.focus, Focus::Pane | Focus::Inspector) => fleet_pane(&mut lay, body, st),
        View::Fleet => lay.fleet = Some(fleet(body, st, st.tree_collapsed)),
        View::Board => lay.kanban = Some(kanban(body)),
        View::Plans => {}
        View::Brain => lay.brain = Some(brain(body, st)),
        View::WorkerTypes => lay.worker_types = Some(worker_types(body, st)),
        View::Remotes => lay.remotes = Some(body),
        // Renders straight off `lay.body`; no dedicated layout geometry.
        View::UserRequests | View::Idle => {}
    }
    if let Some(Modal::Help) = &st.modal {
        // `draw_modal`'s box: the lines plus its two border rows, as tall as
        // the screen allows.
        let n = super::view::help_line_count() as u16;
        let inner = (n + 2).min(area.height).saturating_sub(2);
        lay.help_max_scroll = n.saturating_sub(inner);
        lay.help_page = (inner / 2).max(1);
    }
    if let Some(Modal::Confirm(pending)) = &st.modal {
        let (q, detail) = st.confirm_text(pending);
        let tw = unicode_width::UnicodeWidthStr::width(q.as_str()).max(unicode_width::UnicodeWidthStr::width(detail.as_str())) as u16;
        let (_, yes, no, all) = confirm_rects(area, tw, st.confirm_all_label(pending));
        lay.modal_buttons = Some((yes, no));
        lay.modal_all = all;
    }
    lay
}

fn tabs(lay: &mut Layout) {
    let h = lay.header;
    let mut x = h.x + BADGE_W;
    // STATUS_W only has room for 5 of TABS' 6 entries at common widths
    // (e.g. 80 columns) — reserving it unconditionally silently dropped the
    // last tab (`6 Remotes`) even though its keybinding still worked. A
    // dropped tab loses the user's only on-screen reminder the view
    // exists; the status strip this reserves room for already degrades
    // gracefully on its own when squeezed (see `draw_header`'s `right`/
    // `health` fallback), so prefer shrinking that over dropping a tab:
    // only reserve STATUS_W when every tab fits without it.
    let all_tabs_w: u16 =
        TABS.iter().map(|(_, label)| label.chars().count() as u16 + 4).sum::<u16>() + (TABS.len() as u16 - 1);
    let limit = if x + all_tabs_w <= h.right() { h.right() } else { h.right().saturating_sub(STATUS_W) };
    for (key, (view, label)) in ('1'..).zip(TABS) {
        // " 1 Fleet "
        let w = label.chars().count() as u16 + 4;
        if h.height == 0 || x + w > limit {
            break;
        }
        lay.tabs.push(Tab { view, key, label, rect: Rect { x, y: h.y, width: w, height: 1 } });
        x += w + 1;
    }
    if h.height > 0 && x + SETTINGS_BUTTON_W <= h.right() {
        lay.settings_button = Some(Rect { x, y: h.y, width: SETTINGS_BUTTON_W, height: 1 });
        x += SETTINGS_BUTTON_W + 1;
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
    let doc_body_full = Rect { y: doc.y + head, height: doc.height - head, ..doc };
    // One header row per non-empty section, plus its entries, capped so
    // the links panel never crowds out the body it's attached to.
    const LINKS_MAX_H: u16 = 8;
    const DOC_BODY_MIN_H: u16 = 4;
    let link_rows = b.link_rows();
    let link_sections = {
        let mut seen: Option<&str> = None;
        link_rows.iter().filter(|(label, _)| { let is_new = seen != Some(*label); seen = Some(*label); is_new }).count()
    };
    let link_rows_needed = (link_rows.len() + link_sections) as u16;
    let links_h = if has_entry && link_rows_needed > 0 && doc_body_full.height > DOC_BODY_MIN_H {
        link_rows_needed.min(LINKS_MAX_H).min(doc_body_full.height - DOC_BODY_MIN_H)
    } else {
        0
    };
    let links = Rect { y: doc_body_full.bottom().saturating_sub(links_h), height: links_h, ..doc_body_full };
    let doc_body = Rect { height: doc_body_full.height - links_h, ..doc_body_full };
    let items = b.items();
    let cursor = b.cursor_index(&items);
    let h = list.height as usize;
    let list_offset = cursor.saturating_sub(h.saturating_sub(1));
    let graph = if st.brain.graph_mode { Rect { y: pad.y + gap, height: pad.height.saturating_sub(gap), ..pad } } else { Rect::default() };
    BrainLayout { search, list, doc, doc_body, links, buttons, items, list_offset, cursor, graph }
}

pub fn sidebar_width(body_w: u16) -> u16 {
    (body_w * 3 / 10).clamp(SIDEBAR_MIN, SIDEBAR_MAX)
}

/// The pane-attach region within the Fleet tab (`Focus::Pane`, reached by
/// pressing `c` on a focused session — `graph_key`'s existing "connect"
/// key, not a new binding): sidebar/tree plus the live pane, same geometry
/// the pre-v1 "Fleet" tab used.
fn fleet_pane(lay: &mut Layout, body: Rect, st: &TuiState) {
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
    // A diff needs more room than the Info tab's key/value column.
    let insp_w = match st.inspector_tab {
        super::inspector::Tab::Info => INSPECTOR_WIDTH,
        super::inspector::Tab::Diff => INSPECTOR_WIDTH.max((u32::from(rest.width) * 45 / 100) as u16),
    };
    if st.inspector && rest.width >= insp_w + 30 {
        let insp = Rect { x: rest.right() - insp_w, width: insp_w, ..rest };
        lay.inspector = Some(insp);
        let body = super::inspector::body(insp);
        let n = super::inspector::lines(st, body.width as usize).len() as u16;
        lay.inspector_max_scroll = n.saturating_sub(body.height);
        rest.width -= insp_w;
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

/// Cells between adjacent kanban columns.
const KANBAN_GAP: u16 = 1;

/// The Board tab: one region per `kanban::COLUMNS` entry, in order, equal
/// width, separated by `KANBAN_GAP` — restyled Kanban per the spec's "v1
/// replaces the shell" section ("unchanged in function").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KanbanLayout {
    /// Filter, scope chips, Resume all and the shown/total count.
    pub header: Rect,
    pub columns: [Rect; 6],
}

fn kanban(body: Rect) -> KanbanLayout {
    // Inset one cell each side, level with the columns' own borders.
    let header = Rect { x: body.x + 1, width: body.width.saturating_sub(2), height: body.height.min(1), ..body };
    let gap = u16::from(body.height > 2);
    let body = Rect { y: body.y + header.height + gap, height: body.height.saturating_sub(header.height + gap), ..body };
    let n = super::kanban::COLUMNS.len() as u16;
    let col_w = ((body.width + KANBAN_GAP) / n).saturating_sub(KANBAN_GAP).max(1);
    let mut columns = [Rect::default(); 6];
    for (i, col) in columns.iter_mut().enumerate() {
        let x = body.x + i as u16 * (col_w + KANBAN_GAP);
        let is_last = i as u16 + 1 == n;
        let w = if is_last { body.right().saturating_sub(x) } else { col_w };
        *col = Rect { x, width: w, ..body };
    }
    KanbanLayout { header, columns }
}

/// The section's field lines. Fields that the shared form files under
/// different headings (default models, say) keep them as sub-headings; the
/// last refusal sits under the selected field.
pub fn settings_lines(st: &TuiState) -> Vec<SettingsLine> {
    let v = &st.settings;
    let visible = v.section_fields();
    let headed = visible.windows(2).any(|w| v.fields[w[0]].section != v.fields[w[1]].section);
    let mut out = Vec::new();
    let mut section = None;
    for i in visible {
        let f = &v.fields[i];
        if headed && section != Some(f.section) {
            if section.is_some() {
                out.push(SettingsLine::Gap);
            }
            out.push(SettingsLine::Heading(f.section));
        }
        section = Some(f.section);
        out.push(SettingsLine::Field(i));
        if i == v.selected && v.error.is_some() {
            out.push(SettingsLine::Error);
        }
    }
    out
}

/// The page: a path line (and the load error under it), the section bar on
/// the left, and on the right the section's title, blurb and fields, with
/// the selected field's description beside them when there is room, else
/// under; a key-hint row closes the page.
pub(super) fn settings(body: Rect, st: &TuiState) -> SettingsLayout {
    let path = Rect { height: body.height.min(1), ..body };
    let bw = (OPEN_CONFIG_LABEL.len() as u16).min(path.width);
    let open = Rect { x: path.right() - bw, width: bw, ..path };
    let top = body.height.min(2);
    let hint_h = u16::from(body.height > top + 3);
    let content = Rect { y: body.y + top, height: body.height - top - hint_h, ..body };
    let hint = Rect { y: content.bottom(), height: hint_h, ..body };

    let bar_w = SETTINGS_BAR_W.min(content.width / 3);
    let bar = Rect { width: bar_w, ..content };
    let bar_lines = settings_bar_lines();
    let sel_bar = bar_lines.iter().position(|l| *l == SettingsBarLine::Section(st.settings.section)).unwrap_or(0);
    let bar_offset = scroll_offset(sel_bar, 0, bar_lines.len(), bar.height.max(1) as usize);

    let right = Rect { x: content.x + bar_w + 3, width: content.width.saturating_sub(bar_w + 3), ..content };
    let title = Rect { height: right.height.min(2), ..right };
    let cw = (SETTINGS_CLOSE_LABEL.chars().count() as u16).min(right.width);
    let close = Rect { x: right.right() - cw, width: cw, height: right.height.min(1), ..right };
    let rest = Rect { y: right.y + right.height.min(3), height: right.height.saturating_sub(3), ..right };

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
    let offset = scroll_offset(sel, 2, lines.len(), h);
    let label_w = SETTINGS_LABEL_W.min(list.width.saturating_sub(2 + 20)).max(12);
    let value_x = (list.x + 2 + label_w).min(list.right());
    SettingsLayout { path, open, bar, bar_lines, bar_offset, title, close, list, lines, offset, value_x, label_w, help, hint }
}

/// Scroll offset for `selected` so it — plus `lookahead` rows after it, so
/// e.g. an error line directly under it stays in view — is visible in a
/// `page`-tall viewport over `total` rows. Shared by `settings()` (which
/// renders an error inline under its selection, `lookahead: 2`) and
/// `worker_types()`'s two lists (whose error/help render in a separate
/// panel instead, `lookahead: 1`).
fn scroll_offset(selected: usize, lookahead: usize, total: usize, page: usize) -> usize {
    let page = page.max(1);
    (selected + lookahead).saturating_sub(page).min(total.saturating_sub(page))
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkerTypesLayout {
    /// Left column: every registry name plus a trailing "+ New type" row.
    pub names: Rect,
    /// `names` rows actually drawn start at this index into
    /// `worker_types.names` (len() itself is the "+ New type" row).
    pub names_offset: usize,
    /// Right column: the selected (or draft) type's field form.
    pub fields: Rect,
    /// `fields` rows actually drawn start at this index into
    /// `worker_types.fields`.
    pub fields_offset: usize,
    /// Where the value column starts; a click from here on changes it.
    pub value_x: u16,
    /// The selected field's description, or the last error.
    pub help: Rect,
}

fn worker_types(body: Rect, st: &TuiState) -> WorkerTypesLayout {
    let pad = Rect { x: body.x + 1, width: body.width.saturating_sub(2), ..body };
    let names_w = pad.width.min(28);
    let split = pad.width >= names_w + 3 + 24;
    let (names, rest) = if split {
        (Rect { width: names_w, ..pad }, Rect { x: pad.x + names_w + 2, width: pad.width.saturating_sub(names_w + 2), ..pad })
    } else {
        let nh = pad.height.min(6);
        let gap = u16::from(pad.height > nh);
        (Rect { height: nh, ..pad }, Rect { y: pad.y + nh + gap, height: pad.height.saturating_sub(nh + gap), ..pad })
    };
    let total_names = st.worker_types.names.len() + 1;
    let names_page = names.height.max(1) as usize;
    let names_offset = scroll_offset(st.worker_types.list_selected, 1, total_names, names_page);

    let help_h = rest.height.min(5);
    let gap2 = u16::from(rest.height > help_h);
    let fields = Rect { height: rest.height.saturating_sub(help_h + gap2), ..rest };
    let help = Rect { y: fields.bottom() + gap2, height: help_h, ..rest };
    let total_fields = st.worker_types.fields.len();
    let fields_page = fields.height.max(1) as usize;
    let fields_offset = scroll_offset(st.worker_types.field_selected, 1, total_fields, fields_page);
    let value_x = (fields.x + 2 + SETTINGS_LABEL_W).min(fields.right());
    WorkerTypesLayout { names, names_offset, fields, fields_offset, value_x, help }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::state::{Row, TuiState};

    fn st(n: usize) -> TuiState {
        TuiState { rows: (0..n).map(|i| Row::test_row(&format!("s{i}"))).collect(), ..Default::default() }
    }

    #[test]
    fn scroll_offset_keeps_the_selection_and_its_lookahead_in_view() {
        assert_eq!(scroll_offset(0, 2, 10, 5), 0, "selection near the top needs no scroll");
        assert_eq!(scroll_offset(4, 2, 10, 5), 1, "one more row past the selection must stay visible");
        assert_eq!(scroll_offset(9, 1, 10, 5), 5, "never scrolls past the last page");
        assert_eq!(scroll_offset(0, 1, 3, 10), 0, "a page taller than the content never scrolls");
    }

    #[test]
    fn fleet_pane_splits_sidebar_and_pane() {
        let s = st(3);
        let mut lay = Layout::default();
        fleet_pane(&mut lay, Rect::new(0, 2, 120, 37), &s);
        let side = lay.sidebar.unwrap();
        let pane = lay.pane.unwrap();
        assert_eq!(side.width, 36);
        assert_eq!(pane.x, 36);
        assert_eq!(pane.right(), 120);
        // The pane's header row and rule, padded a cell either side.
        assert_eq!(lay.pane_inner.unwrap(), Rect::new(37, 4, 82, 35));
        let mut wide = Layout::default();
        fleet_pane(&mut wide, Rect::new(0, 2, 300, 37), &s);
        assert_eq!(wide.sidebar.unwrap().width, SIDEBAR_MAX);
    }

    #[test]
    fn fleet_view_computes_pane_layout_only_once_a_pane_is_attached() {
        let mut s = st(3);
        let fleet_lay = compute(Rect::new(0, 0, 120, 40), &s);
        assert!(fleet_lay.fleet.is_some(), "no pane attached yet — Fleet shows the islands view");
        assert!(fleet_lay.pane.is_none());

        s.focus = Focus::Pane;
        let pane_lay = compute(Rect::new(0, 0, 120, 40), &s);
        assert!(pane_lay.pane.is_some(), "`c` attached a pane — Fleet shows it instead of the islands view");
        assert!(pane_lay.fleet.is_none());
    }

    #[test]
    fn narrow_terminals_show_sidebar_or_pane_full_width_by_focus() {
        let mut s = st(3);
        let mut lay = Layout::default();
        fleet_pane(&mut lay, Rect::new(0, 2, 40, 7), &s);
        assert_eq!(lay.sidebar.unwrap().width, 40);
        assert!(lay.pane.is_none());
        s.focus = Focus::Pane;
        let mut lay = Layout::default();
        fleet_pane(&mut lay, Rect::new(0, 2, 40, 7), &s);
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
        let body = Rect::new(0, 2, 120, 21);
        let mut lay = Layout { body, ..Default::default() };
        fleet_pane(&mut lay, body, &s);
        let (needs, list) = (lay.sidebar_needs.unwrap(), lay.sidebar_list.unwrap());
        assert_eq!(needs.y, lay.body.y);
        assert_eq!(list.y, needs.bottom());
        assert!(needs.height <= lay.body.height / 2);
        assert!(matches!(lay.needs_items.iter().rev().nth(1), Some(SideItem::More(_))), "{:?}", lay.needs_items);
        assert!(lay.sidebar_offset > 0, "the tree scrolls to the selection while needs stay put");
    }

    #[test]
    fn zoom_hides_the_sidebar_but_keeps_an_opened_inspector() {
        let mut s = st(3);
        s.zoom = true;
        let mut lay = Layout::default();
        fleet_pane(&mut lay, Rect::new(0, 2, 120, 37), &s);
        assert!(lay.sidebar.is_none() && lay.inspector.is_none());
        assert_eq!(lay.pane.unwrap().width, 120);
        s.inspector = true;
        let mut lay = Layout::default();
        fleet_pane(&mut lay, Rect::new(0, 2, 120, 37), &s);
        assert!(lay.sidebar.is_none());
        assert_eq!(lay.inspector.unwrap().right(), 120, "the brief: the inspector sits beside a zoomed pane too");
        assert_eq!(lay.pane.unwrap().width, 120 - INSPECTOR_WIDTH);
    }

    #[test]
    fn the_diff_tab_widens_the_inspector_and_inspector_focus_keeps_the_pane_layout() {
        let mut s = st(1);
        s.inspector = true;
        s.inspector_tab = crate::tui::inspector::Tab::Diff;
        let mut lay = Layout::default();
        fleet_pane(&mut lay, Rect::new(0, 2, 200, 37), &s);
        assert!(lay.inspector.unwrap().width > INSPECTOR_WIDTH);
        s.view = View::Fleet;
        s.focus = Focus::Inspector;
        assert!(compute(Rect::new(0, 0, 200, 40), &s).inspector.is_some());
        assert!(compute(Rect::new(0, 0, 3000, 40), &s).inspector.is_some(), "45% of a very wide body doesn't overflow");
    }

    #[test]
    fn inspector_takes_the_right_edge() {
        let mut s = st(1);
        s.inspector = true;
        let mut lay = Layout::default();
        fleet_pane(&mut lay, Rect::new(0, 2, 160, 37), &s);
        assert_eq!(lay.inspector.unwrap().right(), 160);
        assert_eq!(lay.pane.unwrap().right(), 160 - INSPECTOR_WIDTH);
    }

    #[test]
    fn sidebar_scrolls_to_keep_selection_visible() {
        let mut s = st(50);
        s.selected = 45;
        let mut lay = Layout::default();
        fleet_pane(&mut lay, Rect::new(0, 2, 120, 17), &s);
        let h = lay.sidebar_list.unwrap().height as usize;
        assert!(lay.sidebar_offset <= 45 && 45 < lay.sidebar_offset + h);
    }

    #[test]
    fn pane_title_has_a_back_button() {
        let mut lay = Layout::default();
        fleet_pane(&mut lay, Rect::new(0, 2, 120, 37), &st(1));
        let pane = lay.pane.unwrap();
        assert_eq!(lay.pane_back, Some(Rect::new(pane.x + 1, pane.y, 9, 1)));
    }

    #[test]
    fn kanban_lays_out_six_non_overlapping_columns_spanning_the_body() {
        let body = Rect::new(0, 0, 120, 30);
        let kl = kanban(body);
        for (i, w) in kl.columns.windows(2).enumerate() {
            assert!(w[0].right() < w[1].x, "column {i} overlaps the next");
        }
        assert_eq!(kl.columns[0].x, body.x);
        assert_eq!(kl.columns[5].right(), body.right());
    }

    #[test]
    fn all_six_tabs_render_at_80_columns() {
        // Regression: STATUS_W's reserved budget only had room for 5 of a
        // 6-tab bar at common widths — a dropped tab loses its on-screen
        // reminder even though the digit keybinding still works. Verify the
        // budget arithmetic directly, then confirm `tabs()` renders all 6.
        // `tabs()` only reserves STATUS_W when the tabs don't already fit
        // without it (`x + all_tabs_w <= h.right()`) — so the budget that
        // actually matters is against the full width, not width - STATUS_W.
        let all_tabs_w: u16 = TABS.iter().map(|(_, label)| label.chars().count() as u16 + 4).sum::<u16>() + (TABS.len() as u16 - 1);
        assert!(
            BADGE_W + all_tabs_w <= 80,
            "the six v1 tab labels must fit at 80 columns (got {all_tabs_w} cells of tabs)"
        );
        for w in [80, 85, 89, 90] {
            let lay = compute(Rect::new(0, 0, w, 40), &st(1));
            assert_eq!(lay.tabs.len(), 6, "all six tabs must render at {w} columns");
        }
    }

    #[test]
    fn header_tabs_are_laid_out_in_order_without_overlap() {
        let lay = compute(Rect::new(0, 0, 120, 40), &st(1));
        let views: Vec<View> = lay.tabs.iter().map(|t| t.view).collect();
        assert_eq!(views, [View::Fleet, View::Board, View::Plans, View::Brain, View::WorkerTypes, View::Remotes]);
        for w in lay.tabs.windows(2) {
            assert!(w[0].rect.right() < w[1].rect.x);
        }
        assert!(lay.header_text_x >= lay.tabs.last().unwrap().rect.right());
        let narrow = compute(Rect::new(0, 0, 50, 10), &st(1));
        assert!(narrow.tabs.len() < 5, "tabs that do not fit are dropped, not overlapped");
        assert!(narrow.tabs.iter().all(|t| t.rect.right() <= 50 - STATUS_W));
    }

    #[test]
    fn fleet_tree_collapses_to_fully_hidden() {
        let body = Rect::new(0, 0, 120, 30);
        let expanded = fleet(body, &TuiState::default(), false);
        assert_eq!(expanded.tree.width, sidebar_width(body.width));
        let collapsed = fleet(body, &TuiState::default(), true);
        assert_eq!(collapsed.tree.width, 0, "collapsed hides the tree entirely, not just to a rail");
        assert_eq!(collapsed.main.width, body.width, "the centre reclaims the tree's full width");
        assert!(collapsed.main.width > expanded.main.width);
    }

    #[test]
    fn fleet_layout_reserves_a_toolbar_line_above_the_prompt() {
        let body = Rect::new(0, 0, 80, 30);
        let fl = fleet(body, &TuiState::default(), false);
        assert_eq!(fl.toolbar.height, 1);
        assert!(fl.main.y > fl.toolbar.y, "main area must sit below the toolbar");
        assert!(fl.prompt.y > fl.main.y, "prompt must sit below the main area");
        assert_eq!(fl.toolbar.height + fl.main.height + fl.prompt.height, body.height, "the tree column has full body height, so toolbar/main/prompt must fill it exactly");
        assert_eq!(fl.tree.width + fl.main.width, body.width, "tree + main must span the body width with no detail panel");
    }

    /// T2 (FEEDBACK-v1-tryout-round1.md): the dim one-line prompt became a
    /// 3-row bordered box. "The grid loses 3 rows; the 120x40 layout must
    /// still work" — confirmed at both ticket acceptance-criterion sizes.
    #[test]
    fn fleet_layout_reserves_three_rows_for_the_bordered_prompt_box() {
        for (w, h) in [(120, 40), (220, 60)] {
            let body = Rect::new(0, 0, w, h);
            let fl = fleet(body, &TuiState::default(), false);
            assert_eq!(fl.prompt.height, 3, "prompt box must be 3 rows at {w}x{h}");
            assert!(fl.main.height > 0, "main area must still have room at {w}x{h}");
            assert!(fl.prompt.width <= body.width, "prompt box must never exceed the body width (no horizontal scroll) at {w}x{h}");
        }
    }

    #[test]
    fn fleet_layout_below_min_split_width_shows_only_the_tree() {
        let body = Rect::new(0, 0, MIN_SPLIT_W - 1, 30);
        let fl = fleet(body, &TuiState::default(), false);
        assert_eq!(fl.tree, body);
        assert_eq!(fl.main, Rect::default());
        assert!(fl.detail.is_none());
    }

    #[test]
    fn fleet_layout_shows_the_detail_panel_only_once_an_island_is_open_and_a_node_selected() {
        let mut st = TuiState { rows: vec![Row::test_row("o")], ..Default::default() };
        st.rows[0].is_orchestrator = true;
        st.rows[0].group = Some("o".into());
        let body = Rect::new(0, 0, 160, 30);

        assert!(fleet(body, &st, false).detail.is_none(), "no island open, no detail panel");

        st.fleet.sync(&st.rows.clone(), &std::collections::HashMap::new());
        st.fleet.nav.open_node("o", &st.fleet.islands);
        assert!(fleet(body, &st, false).detail.is_some(), "an open island with a selected node and room must show the detail panel");

        st.fleet.nav.clear_selection();
        assert!(fleet(body, &st, false).detail.is_none(), "open but nothing selected — no detail panel");
    }
}
