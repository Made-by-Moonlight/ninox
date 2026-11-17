//! The Fleet tab's model (MLOPS-5350 slice 2): one island per root
//! orchestrator tree, packed into columns, with an opened-island rataflow
//! card tree and detail pane — replaces the old whole-fleet force-directed
//! canvas (#230) and its bundled per-session tool-call drilldown, both
//! removed; the opened island's own card tree (`island_flow.rs`) reuses
//! that canvas's rataflow plumbing, just scoped to one island. Island
//! membership/bucketing/sort/fold-ability come entirely from
//! `ninox_overview::islands`/`nav` (never re-derived here), matching
//! `ninox-gui`'s shipped `fleet_islands`/`fleet_focus`/`fleet_detail`
//! (MLOPS-5339 #253/#254/#255/#257) — key handling lives in `state.rs`'s
//! `fleet_key`, drawing in `view.rs`'s `draw_fleet`.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::time::Instant;

use ninox_core::config::AppConfig;
use ninox_core::fleet::snapshot::{FleetMember, FleetSnapshot, Role};
use ninox_core::store::Store;
use ninox_core::types::{ActivityState, CIStatus, Comment, MachineHealth, PrId, SessionStatus, PR};

use ninox_overview::islands::{build_islands, sort_islands, Island, IslandKey, IslandSort, MachineFilter};
use ninox_overview::nav::FleetNav;
use ninox_overview::status::MemberStatus;
use ratatui::style::Color;
use rataflow::{Flow, StepEdge};

use super::island_flow::{self, OpenedNode};
use super::palette::Palette;
use super::state::Row;

/// The configured nested-orchestration depth cap (root = 0), read fresh each
/// call so a config edit takes effect without a restart — matches every
/// other ad-hoc `AppConfig::load()` call site in this crate. An invalid
/// config.toml falls back to `NestedOrchestrationConfig::default()`'s depth
/// (`AppConfig::load_or_default()` also logs a warning in that case).
pub fn max_orchestrator_depth() -> u32 {
    AppConfig::load_or_default().nested_orchestration.max_depth
}

/// Builds the `FleetSnapshot` `build_islands` needs from the rows the TUI
/// already holds in memory — no extra store/workspace I/O. Mirrors
/// `ninox-gui`'s `fleet_islands::fleet_snapshot`; `build_islands` only reads
/// `members[].session`/`.role`, so every other `FleetMember` field is a
/// cheap default.
pub fn fleet_snapshot(rows: &[Row]) -> FleetSnapshot {
    let members: Vec<FleetMember> = rows
        .iter()
        .map(|r| FleetMember {
            session: r.session.clone(),
            role: if r.is_orchestrator {
                Role::Orchestrator
            } else if r.group.is_some() {
                Role::Worker
            } else {
                Role::Standalone
            },
            record: Default::default(),
            pr: None,
            ci: None,
            pending_inbox: 0,
            can_resume: true,
            workspace: Default::default(),
            anomalies: Vec::new(),
        })
        .collect();
    FleetSnapshot::assemble(members, Vec::new(), Vec::new(), 0)
}

/// Cycles the toolbar's `s` sort control, matching the GUI's three-option
/// segmented control order (Needs you first → Most active → A–Z).
pub fn cycle_sort(mode: IslandSort) -> IslandSort {
    match mode {
        IslandSort::NeedsYouFirst => IslandSort::MostActive,
        IslandSort::MostActive => IslandSort::AToZ,
        IslandSort::AToZ => IslandSort::NeedsYouFirst,
    }
}

/// The toolbar's `sort: <word>` label (`TUIAtScale.dc.html`).
pub fn sort_label(mode: IslandSort) -> &'static str {
    match mode {
        IslandSort::NeedsYouFirst => "needs-you",
        IslandSort::MostActive => "most-active",
        IslandSort::AToZ => "a-z",
    }
}

/// Column count for the islands grid at a given terminal width — 4 at
/// ~200 cols, fewer as it narrows, 1 below ~100 (brief: "4 at about 200
/// cols, fewer when narrow, 1 below about 100"). Vertical scroll only,
/// never horizontal — narrowing never adds a column.
pub fn column_count(width: u16) -> usize {
    match width {
        0..=99 => 1,
        100..=139 => 2,
        140..=179 => 3,
        _ => 4,
    }
}

/// Distributes `islands` into `columns` vertical lists, always placing the
/// next island into the currently-shortest column — by its caller-supplied
/// `(island, rendered_height)` pairs (the exact row count `view.rs`'s
/// `island_box_height` will draw it at, not an approximation), so the grid
/// packs tightly instead of leaving one column much taller than another
/// with a big trailing gap under the short ones. Mirrors `ninox-gui`'s
/// `fleet_islands::pack_columns`, approximating the design's CSS
/// `column-count` masonry layout, which ratatui has no equivalent for.
pub fn pack_columns<'i>(islands: &[(&'i Island, u16)], columns: usize) -> Vec<Vec<&'i Island>> {
    let mut cols: Vec<Vec<&Island>> = vec![Vec::new(); columns.max(1)];
    let mut heights = vec![0u32; columns.max(1)];
    for &(island, height) in islands {
        let idx = heights.iter().enumerate().min_by_key(|(_, h)| **h).map(|(i, _)| i).unwrap_or(0);
        cols[idx].push(island);
        heights[idx] += height as u32 + 1; // + the gutter row after it
    }
    cols
}

/// Rows between vertically-stacked island boxes in the same column —
/// shared by `column_positions` and `layout::fleet_grid` so they can never
/// disagree about where a box sits within its column.
pub const GRID_ROW_GAP: i32 = 1;

/// Each island's actual rendered box height (`island_row_count` plus the
/// top/bottom border `draw_island_box`'s `Borders::ALL` always draws) —
/// the one place that `+ 2` lives, so `layout::fleet_grid` (drawing) and
/// `TuiState::scroll_grid_to_show` (tree-driven scroll-into-view) can never
/// quietly drift onto two different ideas of how tall a box is.
pub fn island_grid_heights(islands: &[Island], fold_expanded: impl Fn(&IslandKey) -> bool) -> Vec<(&Island, u16)> {
    islands.iter().map(|i| (i, island_row_count(i, fold_expanded(&i.key)) as u16 + 2)).collect()
}

/// Each column's boxes (via `pack_columns`) with their *virtual*
/// (scroll-independent) top offset and height — the single source both
/// `layout::fleet_grid` (turns these into on-screen `Rect`s, offset by
/// `grid_scroll`) and `scroll_to_show_island` (computes the `grid_scroll`
/// that brings one of them into view) build on, so a selection synced from
/// the tree can never disagree with where the grid actually draws it.
pub fn column_positions<'i>(islands: &[(&'i Island, u16)], columns: usize) -> Vec<Vec<(&'i Island, i32, i32)>> {
    let heights: HashMap<&IslandKey, i32> = islands.iter().map(|&(i, h)| (&i.key, h as i32)).collect();
    pack_columns(islands, columns)
        .into_iter()
        .map(|col| {
            let mut y = 0i32;
            col.into_iter()
                .map(|island| {
                    let h = heights.get(&island.key).copied().unwrap_or(0);
                    let out = (island, y, h);
                    y += h + GRID_ROW_GAP;
                    out
                })
                .collect()
        })
        .collect()
}

/// The `grid_scroll` that brings `target`'s box fully into a viewport
/// `area_height` rows tall, nudging `current_scroll` only as far as needed
/// — used when the tree selection picks an island the grid hasn't scrolled
/// to yet ("selecting a tree row also selects the matching island and
/// scrolls to it").
pub fn scroll_to_show_island(positions: &[Vec<(&Island, i32, i32)>], area_height: u16, current_scroll: u16, target: &IslandKey) -> u16 {
    let Some(&(_, y, h)) = positions.iter().flatten().find(|(i, ..)| &i.key == target) else { return current_scroll };
    let area_height = area_height as i32;
    let scroll = current_scroll as i32;
    if y < scroll {
        y.max(0) as u16
    } else if y + h > scroll + area_height {
        (y + h - area_height).max(0) as u16
    } else {
        current_scroll
    }
}

/// The number of content lines `view.rs`'s `island_rows` will draw for
/// `island` — title, optional remote band, visible member rows, and the
/// fold/empty row — used by both `layout.rs` (to size each box's `Rect`,
/// so hit-testing and spatial navigation agree with what's drawn) and
/// `view.rs` (which must draw exactly this many lines, not recompute a
/// second estimate that could silently drift from the layout).
pub fn island_row_count(island: &Island, fold_expanded: bool) -> usize {
    let others = island.members.iter().filter(|m| Some(&m.id) != island.root.as_ref());
    let (finished, visible): (Vec<_>, Vec<_>) = others.partition(|m| matches!(m.status, MemberStatus::Done | MemberStatus::Stopped));
    let mut lines = 1; // title
    if island.machine.is_some() {
        lines += 1; // remote band
    }
    if fold_expanded {
        lines += visible.len() + finished.len();
    } else {
        lines += visible.len();
        if !finished.is_empty() {
            lines += 1; // fold summary row
        } else if visible.is_empty() {
            lines += 1; // "no workers"
        }
    }
    lines
}

/// A direction for the islands grid's spatial keyboard navigation — the
/// grid stays plain boxes (islands never connect to each other, so rataflow
/// buys nothing there), so it needs its own copy of the same weighted
/// nearest-neighbor idea rataflow's `select_node_in_direction` uses for the
/// opened island's card tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridDirection {
    Up,
    Down,
    Left,
    Right,
}

/// Directional bias weight — matches rataflow's own `DIRECTION_BIAS`, so
/// the grid and the opened island's card tree feel like the same keyboard
/// behavior even though they're two separate implementations.
const GRID_DIRECTION_BIAS: f32 = 2.0;

/// Finds the nearest box in `direction` from `current`, among `boxes`
/// (each `(key, center_x, center_y)`) — candidates in the wrong half-plane
/// (behind the cursor relative to `direction`) are never considered, and
/// among the rest the lowest `distance * (1 + bias * angular_penalty)`
/// wins, so a box roughly in line with the cursor beats one merely closer
/// but far off to the side. `None` when nothing lies in that half-plane
/// (no wrapping) or `current` isn't among `boxes`.
pub fn nearest_island_in_direction(boxes: &[(IslandKey, f32, f32)], current: &IslandKey, direction: GridDirection) -> Option<IslandKey> {
    let (_, cx, cy) = boxes.iter().find(|(k, _, _)| k == current)?;
    let (cx, cy) = (*cx, *cy);
    let mut best: Option<(&IslandKey, f32)> = None;
    for (key, x, y) in boxes {
        if key == current {
            continue;
        }
        let (dx, dy) = (x - cx, y - cy);
        let in_half_plane = match direction {
            GridDirection::Up => dy < -0.01,
            GridDirection::Down => dy > 0.01,
            GridDirection::Left => dx < -0.01,
            GridDirection::Right => dx > 0.01,
        };
        if !in_half_plane {
            continue;
        }
        let dist = (dx * dx + dy * dy).sqrt();
        let (primary, perpendicular) = match direction {
            GridDirection::Up | GridDirection::Down => (dy.abs(), dx.abs()),
            GridDirection::Left | GridDirection::Right => (dx.abs(), dy.abs()),
        };
        let angle_penalty = perpendicular / primary.max(0.001);
        let score = dist * (1.0 + GRID_DIRECTION_BIAS * angle_penalty);
        if best.is_none_or(|(_, s)| score < s) {
            best = Some((key, score));
        }
    }
    best.map(|(k, _)| k.clone())
}

/// Parent → children, scoped to `island.members` — mirrors `ninox-gui`'s
/// `fleet_focus::nodes_for_island`, built directly from `Row` rather than
/// the generic `OverviewNode`/`OverviewEdge` wrapper the GUI's iced widgets
/// need (the TUI renders straight off `Row`, so that layer would be pure
/// overhead here).
pub fn island_children(rows: &[Row], island: &Island) -> HashMap<String, Vec<String>> {
    let member_ids: std::collections::HashSet<&str> = island.members.iter().map(|m| m.id.as_str()).collect();
    let mut children: HashMap<String, Vec<String>> = HashMap::new();
    for row in rows {
        if !member_ids.contains(row.id()) {
            continue;
        }
        let parent = if row.is_orchestrator { row.session.orchestrator_id.clone() } else { row.group.clone() };
        // A root orchestrator's own parent pointer is `None` in real data;
        // never treat a row as its own parent regardless (a self-edge here
        // would make the recursive card-renderer in `view.rs` loop forever).
        if let Some(parent) = parent.filter(|p| member_ids.contains(p.as_str()) && p.as_str() != row.id()) {
            children.entry(parent).or_default().push(row.id().to_string());
        }
    }
    children
}

/// The detail pane's cached PR state for the currently selected node: the
/// PR itself, its CI counts, and its Linear-filtered review comments.
#[derive(Debug, Clone, Default)]
pub struct DetailPr {
    pub pr_id: PrId,
    pub pr: Option<PR>,
    pub ci: Option<CIStatus>,
    pub comments: Vec<Comment>,
}

/// Fixed badge color for PR/review tags (the design's `#9db4ff` badge
/// blue) — not themed through `Palette`: the host terminal's own ANSI blue
/// (`p.pr_open`) renders too dark to read against a dark background, and
/// `ninox-gui`'s matching chip (`CHIP_OPEN_FG`) is a fixed color too, not
/// themed either.
const PR_TAG_COLOR: Color = Color::Rgb(0x9d, 0xb4, 0xff);

/// A member's tag text/color, read from its `Session` — `MemberStatus` only
/// carries the coarse bucket (`NeedsYou` covers both a CI failure and
/// blocked activity); the design calls for the specific cause plus the PR
/// number. Mirrors `ninox-gui`'s `fleet_islands::member_chip`. `None` for
/// `Working`/`Done`/`Stopped` — no tag in the design.
pub fn member_tag(status: MemberStatus, row: &Row, p: &Palette) -> Option<(String, Color)> {
    if matches!(status, MemberStatus::Done | MemberStatus::Stopped) {
        return None;
    }
    match row.session.status {
        SessionStatus::Queued => Some((
            format!(" {}", ninox_ui_shared::session_queue::queued_badge(row.queue_position)),
            p.faint,
        )),
        SessionStatus::CiFailed => Some((" ✗CI".into(), p.ci_failed)),
        SessionStatus::Mergeable => Some((" ✓merge".into(), p.mergeable)),
        SessionStatus::PrOpen => {
            let label = ninox_core::session_prs::summary(&row.prs)
                .or_else(|| row.session.pr_number.map(|n| format!("PR #{n}")))
                .unwrap_or_else(|| "PR".into());
            Some((format!(" {label}"), PR_TAG_COLOR))
        }
        SessionStatus::ReviewPending => Some((" review".into(), PR_TAG_COLOR)),
        SessionStatus::Interrupted => Some((" paused".into(), p.review)),
        _ if row.session.activity == ActivityState::Blocked => Some((" !you".into(), p.accent)),
        _ => None,
    }
}

pub fn member_status_color(status: MemberStatus, p: &Palette) -> Color {
    match status {
        MemberStatus::Working => p.working,
        MemberStatus::NeedsYou => p.accent,
        MemberStatus::WaitingOnPr => p.faint,
        MemberStatus::Done => p.done,
        MemberStatus::Stopped => p.faint,
        MemberStatus::Queued => p.faint,
    }
}

/// One line of the Fleet tab's own tree (`TUIAtScale.dc.html`'s "SESSIONS"
/// panel) — a pinned Needs-you block, then sessions grouped by machine
/// (local first, unlabeled unless a remote is also present), each
/// orchestrator group folding its done/terminated workers into one summary
/// row when `!fold_expanded` (shared with the islands grid's own fold
/// toggle — one mental model, not two).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeRow {
    NeedsYouHeading,
    /// Pinned copy of `rows[idx]` under the needs-you heading.
    Needs(usize),
    /// `None` is "this machine" — only shown at all once a remote session
    /// is present, so the common single-machine fleet has no heading.
    MachineHeading(Option<String>),
    Orchestrator(usize),
    Worker(usize),
    Standalone(usize),
    Folded { done: usize, stopped: usize },
}

/// Builds the Fleet tab's tree rows directly from `rows`, in their existing
/// order — a single linear pass, same shape as `TuiState::sidebar_items`,
/// just grouped by machine first and folding finished workers.
/// `fold_expanded` answers per orchestrator group (its island's root id;
/// `None` for rows outside any group).
pub fn tree_rows(rows: &[Row], needs: &[usize], fold_expanded: &dyn Fn(Option<&str>) -> bool) -> Vec<TreeRow> {
    let mut out = Vec::new();
    if !needs.is_empty() {
        out.push(TreeRow::NeedsYouHeading);
        out.extend(needs.iter().map(|&i| TreeRow::Needs(i)));
    }
    let any_remote = rows.iter().any(|r| r.machine_health.is_some());
    let mut current_machine: Option<Option<String>> = None;
    let mut current_group: Option<String> = None;
    let (mut done, mut stopped) = (0usize, 0usize);

    for (i, r) in rows.iter().enumerate() {
        let machine_key = r.session.machine_id.clone();
        if current_machine.as_ref() != Some(&machine_key) {
            if done + stopped > 0 && !fold_expanded(current_group.as_deref()) {
                out.push(TreeRow::Folded { done, stopped });
            }
            (done, stopped) = (0, 0);
            current_group = None;
            if any_remote {
                out.push(TreeRow::MachineHeading(machine_key.clone()));
            }
            current_machine = Some(machine_key);
        }
        match &r.group {
            Some(g) => {
                if current_group.as_deref() != Some(g.as_str()) {
                    if done + stopped > 0 && !fold_expanded(current_group.as_deref()) {
                        out.push(TreeRow::Folded { done, stopped });
                    }
                    (done, stopped) = (0, 0);
                    current_group = Some(g.clone());
                }
                if r.is_orchestrator {
                    out.push(TreeRow::Orchestrator(i));
                } else {
                    match r.session.status {
                        SessionStatus::Done => {
                            done += 1;
                            if fold_expanded(Some(g.as_str())) {
                                out.push(TreeRow::Worker(i));
                            }
                        }
                        SessionStatus::Terminated => {
                            stopped += 1;
                            if fold_expanded(Some(g.as_str())) {
                                out.push(TreeRow::Worker(i));
                            }
                        }
                        _ => out.push(TreeRow::Worker(i)),
                    }
                }
            }
            None => {
                if done + stopped > 0 && !fold_expanded(current_group.as_deref()) {
                    out.push(TreeRow::Folded { done, stopped });
                }
                (done, stopped) = (0, 0);
                current_group = None;
                out.push(TreeRow::Standalone(i));
            }
        }
    }
    if done + stopped > 0 && !fold_expanded(current_group.as_deref()) {
        out.push(TreeRow::Folded { done, stopped });
    }
    out
}

/// Which pane the Fleet tab's bare arrow keys/Enter act on — `Tab`/
/// `Shift-Tab` cycle through them (`Detail` only while the detail panel is
/// actually showing, per `fleet_key`'s doc comment). `Tree` is the default
/// so `j`/`k`/arrows keep moving the left tree exactly as before this
/// slice; `Main` hands arrows to the islands grid's spatial navigation (or,
/// once an island is open, to its rataflow card tree); `Detail` is reached
/// only via `Tab` (no arrow-key behavior of its own yet).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FleetFocus {
    #[default]
    Tree,
    Main,
    Detail,
}

/// Who the ninox prompt sends to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PromptTarget {
    #[default]
    Agent,
    /// A directly-picked orchestrator, by session id.
    Orchestrator(String),
}

pub type ReplyDrawer = ninox_ui_shared::prompt_agent::reply::ReplyDrawer<()>;

/// The Ninox prompt's reply panel (`PromptReply.dc.html`): the shared
/// conversation state plus the TUI's own focus and scroll.
#[derive(Debug, Default)]
pub struct ReplyPanel {
    pub drawer: ReplyDrawer,
    /// The panel owns j/k/PgUp/PgDn/g/G/esc instead of the grid.
    pub focused: bool,
    /// The conversation row pinned to the top of the panel; `None` sits
    /// the latest question at the top (or the end, if the latest turn fits),
    /// so a long answer reads from its start as it streams in.
    pub scroll: Option<u16>,
    /// The top row and the furthest it can go, as last drawn — rendering
    /// only gets `&TuiState`, and both depend on the drawn width.
    pub drawn_top: Cell<u16>,
    pub max_scroll: Cell<u16>,
    /// Bumped by every ask and successful clear; a delivery or poll result
    /// from an older generation is dropped.
    pub generation: u64,
}

impl ReplyPanel {
    pub fn is_open(&self) -> bool {
        self.drawer.is_open()
    }

    pub fn ask(&mut self, question: String, target_label: String, now_ms: i64) -> u64 {
        self.drawer.ask(question, target_label, now_ms);
        self.generation += 1;
        self.scroll = None;
        self.generation
    }

    pub fn collapse(&mut self) {
        self.drawer.collapse();
        self.focused = false;
    }

    pub fn reopen(&mut self) {
        self.drawer.reopen();
        self.scroll = None;
    }

    pub fn cleared(&mut self) {
        self.drawer.clear();
        self.generation += 1;
        self.scroll = None;
    }

    /// Positive scrolls back towards older turns, clamped to what was drawn.
    pub fn scroll_by(&mut self, rows: i32) {
        let top = self.drawn_top.get() as i32 - rows;
        self.set_top(top.clamp(0, self.max_scroll.get() as i32) as u16);
    }

    pub fn scroll_to_top(&mut self) {
        self.set_top(0);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.set_top(self.max_scroll.get());
    }

    /// `drawn_top` moves with it so a second key before the next draw
    /// scrolls from here, not from the stale frame.
    fn set_top(&mut self, top: u16) {
        self.scroll = Some(top);
        self.drawn_top.set(top);
    }

    /// The top row to draw: `scroll` clamped to `max`, else the latest
    /// question (`latest_start`) or the end, whichever is higher up.
    pub fn top_row(&self, latest_start: u16, max: u16) -> u16 {
        self.scroll.unwrap_or(latest_start).min(max)
    }

    /// The picked orchestrator the latest turn went to, for "o open pane".
    pub fn orchestrator_id(&self) -> Option<&str> {
        match &self.drawer.turns.last()?.source {
            ninox_ui_shared::prompt_agent::reply::AnswerSource::OrchestratorPane { id, .. } => Some(id),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub struct FleetState {
    /// Island open/select/node-select bookkeeping — shared with `ninox-gui`.
    pub nav: FleetNav,
    pub sort: IslandSort,
    /// Density toggle (`m`): compact rows (default, matches the design
    /// board) vs. full rows with an extra activity-note line per working
    /// member.
    pub compact: bool,
    /// The finished-workers fold every island starts from (`ctrl+f` flips
    /// it, clearing per-island flips): folded (default) vs. expanded.
    pub fold_expanded: bool,
    /// Islands whose fold is the opposite of `fold_expanded` (`f` on one),
    /// like the GUI's `fleet_expanded_folds`.
    pub fold_flipped: HashSet<IslandKey>,
    /// Rebuilt from `rows`/`machine_health` on every `sync` call (the TUI's
    /// 1s refresh tick), sorted per `sort` and narrowed to `machine_filter`.
    pub islands: Vec<Island>,
    /// Every island before `machine_filter`, so `M` can re-filter at once.
    all_islands: Vec<Island>,
    /// `M` cycles All → Local → each remote machine.
    pub machine_filter: MachineFilter,
    pub machines: Vec<String>,
    /// `Tab` toggle between the tree and the main area (grid or opened
    /// island) owning bare arrow keys.
    pub main_focus: FleetFocus,
    /// Rows the islands grid has scrolled down, keeping the
    /// `nav.selected_island` box visible after spatial navigation moves it
    /// off-screen.
    pub grid_scroll: u16,
    pub prompt_input: String,
    pub prompt_editing: bool,
    /// Who the prompt sends to (`ctrl+t` cycles): the prompt agent by
    /// default, or a directly-picked orchestrator.
    pub prompt_target: PromptTarget,
    /// Past submissions, oldest first; `↑`/`↓` recall them.
    pub prompt_history: Vec<String>,
    /// Index into `prompt_history` while recalling; `None` means the input
    /// is a fresh (unsent) draft, not a history entry.
    pub prompt_history_pos: Option<usize>,
    /// The prompt conversation, over the bottom of the Fleet view or
    /// collapsed to one line above the prompt box — never silent, per the
    /// "it did not reply" complaint (FEEDBACK-v1-tryout-round1.md's T3).
    pub reply: ReplyPanel,
    /// The detail pane's PR/CI/comments for `nav.selected`, refreshed every
    /// `sync_detail_pr` call (the TUI's 1s refresh tick) — `None` whenever
    /// nothing is selected or the selection has no PR yet.
    pub detail_pr: Option<DetailPr>,
    /// The opened island's own rataflow card tree (`island_flow::build_flow`),
    /// rebuilt every render by `ensure_opened_flow` (content can change live,
    /// and the wrap width can change with the terminal) — `RefCell` because
    /// rendering only ever gets `&TuiState`, never `&mut`.
    pub opened_flow: RefCell<Option<Flow<OpenedNode, StepEdge>>>,
    /// Which island `opened_flow` was last built for — a change means the
    /// next `ensure_opened_flow` must reset the viewport instead of carrying
    /// over the previous (unrelated) island's scroll position.
    opened_flow_key: RefCell<Option<IslandKey>>,
    /// When `tick_opened_animation` last ran — `None` until its first call.
    last_opened_animation_tick: Cell<Option<Instant>>,
}

impl Default for FleetState {
    fn default() -> Self {
        Self {
            nav: FleetNav::default(),
            sort: IslandSort::NeedsYouFirst,
            compact: true,
            fold_expanded: false,
            fold_flipped: HashSet::new(),
            islands: Vec::new(),
            all_islands: Vec::new(),
            machine_filter: MachineFilter::default(),
            machines: Vec::new(),
            main_focus: FleetFocus::default(),
            grid_scroll: 0,
            prompt_input: String::new(),
            prompt_editing: false,
            prompt_target: PromptTarget::default(),
            prompt_history: Vec::new(),
            prompt_history_pos: None,
            reply: ReplyPanel::default(),
            detail_pr: None,
            opened_flow: RefCell::new(None),
            opened_flow_key: RefCell::new(None),
            last_opened_animation_tick: Cell::new(None),
        }
    }
}

impl FleetState {
    /// Rebuilds `islands` from the fleet's current `rows`/`machine_health` —
    /// called once per refresh tick, same cadence the old canvas's
    /// `sync_fleet` ran at.
    pub fn sync(&mut self, rows: &[Row], machine_health: &HashMap<String, MachineHealth>) {
        let snapshot = fleet_snapshot(rows);
        let mut islands = build_islands(&snapshot, machine_health);
        sort_islands(&mut islands, self.sort);
        self.all_islands = islands;
        self.machines = ninox_overview::islands::machine_ids(&self.all_islands);
        if let MachineFilter::Machine(id) = &self.machine_filter {
            if !self.machines.contains(id) {
                self.machine_filter = MachineFilter::All;
            }
        }
        self.apply_machine_filter();
    }

    fn apply_machine_filter(&mut self) {
        self.islands = self.all_islands.iter().filter(|i| self.machine_filter.matches(i)).cloned().collect();
        let shown = |k: &IslandKey| self.islands.iter().any(|i| &i.key == k);
        if self.nav.open.as_ref().is_some_and(|k| !shown(k)) {
            self.nav.close_island();
            self.nav.clear_selection();
            if self.main_focus == FleetFocus::Detail {
                self.main_focus = FleetFocus::Main;
            }
        }
        if self.nav.selected_island.as_ref().is_some_and(|k| !shown(k)) {
            self.nav.selected_island = self.islands.first().map(|i| i.key.clone());
        }
    }

    pub fn cycle_machine_filter(&mut self) {
        self.machine_filter = self.machine_filter.next(&self.machines);
        self.grid_scroll = 0;
        self.apply_machine_filter();
    }

    /// Whether `key`'s finished workers show (vs. fold into one row).
    pub fn island_fold_expanded(&self, key: &IslandKey) -> bool {
        self.fold_expanded ^ self.fold_flipped.contains(key)
    }

    pub fn toggle_island_fold(&mut self, key: &IslandKey) {
        if !self.fold_flipped.remove(key) {
            self.fold_flipped.insert(key.clone());
        }
    }

    /// `ctrl+f`: flip every island's fold to the same new state.
    pub fn toggle_fold_all(&mut self) {
        self.fold_expanded = !self.fold_expanded;
        self.fold_flipped.clear();
    }

    /// Rebuilds the opened island's rataflow card tree, if one is open —
    /// called every render (content can change live, and `available_width`
    /// tracks the terminal, so a plain data refresh is never enough). The
    /// previous frame's viewport/selection carry over as long as the same
    /// island is still open; switching to a different one resets to the
    /// top-left instead of keeping an unrelated scroll position.
    pub fn ensure_opened_flow(&self, rows: &[Row], palette: &Palette, available_width: f64) {
        let Some(key) = self.nav.open.clone() else {
            *self.opened_flow.borrow_mut() = None;
            *self.opened_flow_key.borrow_mut() = None;
            return;
        };
        let Some(island) = self.islands.iter().find(|i| i.key == key) else {
            return;
        };
        let Some(mut flow) = island_flow::build_flow(rows, island, self.island_fold_expanded(&key), palette, available_width) else {
            return;
        };
        let same_island = self.opened_flow_key.borrow().as_ref() == Some(&key);
        if same_island {
            if let Some(old) = self.opened_flow.borrow().as_ref() {
                island_flow::carry_over_viewport_and_selection(old, &mut flow);
            }
        }
        if let Some(id) = self.nav.selected.as_deref() {
            if flow.node(id).is_some() {
                flow.select_node(id);
            }
        }
        *self.opened_flow.borrow_mut() = Some(flow);
        *self.opened_flow_key.borrow_mut() = Some(key);
    }

    /// Advances the opened island's marching-ants clock by real elapsed
    /// time since the last call — called on every draw while an island is
    /// open (not just the 1s refresh tick, which is too coarse to look
    /// animated).
    pub fn tick_opened_animation(&self) {
        let now = Instant::now();
        let elapsed = self.last_opened_animation_tick.get().map(|t| now.duration_since(t)).unwrap_or_default();
        self.last_opened_animation_tick.set(Some(now));
        if let Some(flow) = self.opened_flow.borrow_mut().as_mut() {
            flow.tick_animation(elapsed);
        }
    }

    /// Refreshes the detail pane's PR/CI/comments for the current selection
    /// — three small indexed `Store` reads, cheap enough to run every
    /// refresh tick (same tier of cost as `refresh()`'s other per-tick store
    /// calls, e.g. `list_pr_watches`).
    pub fn sync_detail_pr(&mut self, rows: &[Row], store: &Store) {
        let pr_id = self
            .nav
            .selected
            .as_deref()
            .and_then(|id| rows.iter().find(|r| r.id() == id))
            .and_then(|r| r.session.pr_id);
        self.detail_pr = pr_id.map(|pr_id| {
            let pr = store.get_pr(pr_id).ok().flatten();
            let ci = store.get_ci_status(pr_id).ok().flatten();
            // The shared GUI filter (#257's `fleet_detail::context`) — not
            // reimplemented here — so the TUI and GUI detail panes always
            // agree on what counts as a Linear bookkeeping comment.
            let raw_comments = store.list_comments_for_pr(pr_id).unwrap_or_default();
            let comments = ninox_overview::comments::visible_comments(&raw_comments).into_iter().cloned().collect();
            DetailPr { pr_id, pr, ci, comments }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_tag_names_the_lead_pr_and_the_rest() {
        use ninox_core::session_prs::{PrState, SessionPr};
        let pr = |n, state| SessionPr {
            repo: "o/bellhop".into(), number: n, url: String::new(), title: String::new(), state,
            draft: false, ci: ninox_core::pr_events::CiVerdict::None, passing: 0, failing: 0, pending: 0,
            changes_requested: false, approved: false, tracked: false,
        };
        let mut r = row("w1", Some("acme"), false, SessionStatus::PrOpen);
        let p = Palette::terminal();
        assert_eq!(member_tag(MemberStatus::WaitingOnPr, &r, &p).unwrap().0, " PR #42");
        r.prs = vec![pr(1466, PrState::Open), pr(1473, PrState::Open), pr(900, PrState::Merged)];
        assert_eq!(member_tag(MemberStatus::WaitingOnPr, &r, &p).unwrap().0, " PR bellhop#1466 +2");
    }

    use ninox_core::types::SessionStatus;
    use ninox_overview::islands::{IslandKey, IslandMember};

    fn row(id: &str, group: Option<&str>, is_orchestrator: bool, status: SessionStatus) -> Row {
        Row {
            session: ninox_ui_shared::cli::test_fixtures::session(id, group, status),
            is_orchestrator,
            // Real `build_rows` data sets an orchestrator's own `Row.group`
            // to its own id (its `session.orchestrator_id`, the `group`
            // param here, stays `None` for a root) — `tree_rows` reads
            // `Row.group`, not `session.orchestrator_id`, to find an
            // orchestrator's row at all.
            group: if is_orchestrator { Some(id.to_string()) } else { group.map(String::from) },
            machine_health: None, queue_position: None, prs: Vec::new(),
        }
    }

    #[test]
    fn fleet_snapshot_assigns_roles_from_row_shape() {
        let rows = vec![
            row("acme", None, true, SessionStatus::Working),
            row("w1", Some("acme"), false, SessionStatus::Working),
            row("solo", None, false, SessionStatus::Working),
        ];
        let snap = fleet_snapshot(&rows);
        let role_of = |id: &str| snap.members.iter().find(|m| m.id() == id).unwrap().role;
        assert_eq!(role_of("acme"), Role::Orchestrator);
        assert_eq!(role_of("w1"), Role::Worker);
        assert_eq!(role_of("solo"), Role::Standalone);
    }

    #[test]
    fn sync_builds_one_island_per_orchestrator_tree() {
        let rows = vec![row("acme", None, true, SessionStatus::Working), row("w1", Some("acme"), false, SessionStatus::Working)];
        let mut fleet = FleetState::default();
        fleet.sync(&rows, &HashMap::new());
        assert_eq!(fleet.islands.len(), 1);
        assert_eq!(fleet.islands[0].key, IslandKey::Root("acme".into()));
    }

    #[test]
    fn column_count_matches_the_briefs_breakpoints() {
        assert_eq!(column_count(200), 4);
        assert_eq!(column_count(120), 2);
        assert_eq!(column_count(60), 1);
        assert_eq!(column_count(99), 1);
        assert_eq!(column_count(100), 2);
    }

    #[test]
    fn pack_columns_balances_by_rendered_height_not_island_count() {
        let big = Island { key: IslandKey::Root("big".into()), name: "big".into(), root: Some("big".into()), members: vec![IslandMember { id: "big".into(), status: ninox_overview::status::MemberStatus::Working }; 10], summary: Default::default(), machine: None, pending_request: false };
        let small = Island { key: IslandKey::Root("small".into()), name: "small".into(), root: Some("small".into()), members: vec![], summary: Default::default(), machine: None, pending_request: false };
        let small2 = Island { key: IslandKey::Root("small2".into()), name: "small2".into(), root: Some("small2".into()), members: vec![], summary: Default::default(), machine: None, pending_request: false };
        let islands = [(&big, 14u16), (&small, 2u16), (&small2, 2u16)];
        let cols = pack_columns(&islands, 2);
        assert_eq!(cols.iter().map(Vec::len).sum::<usize>(), 3);
        // The big island should have both small ones packed into the other column.
        assert!(cols.iter().any(|c| c.len() == 2));
    }

    #[test]
    fn island_row_count_folds_finished_members_into_one_line() {
        let island = Island {
            key: IslandKey::Root("acme".into()),
            name: "acme".into(),
            root: Some("acme".into()),
            members: vec![
                IslandMember { id: "acme".into(), status: MemberStatus::Working },
                IslandMember { id: "w1".into(), status: MemberStatus::Working },
                IslandMember { id: "w2".into(), status: MemberStatus::Done },
                IslandMember { id: "w3".into(), status: MemberStatus::Stopped },
            ],
            summary: Default::default(),
            machine: None,
            pending_request: false,
        };
        // title + 1 visible + 1 fold row = 3, regardless of the 2 finished members.
        assert_eq!(island_row_count(&island, false), 3);
        // expanded: title + 1 visible + 2 finished, no fold row.
        assert_eq!(island_row_count(&island, true), 4);
    }

    fn grid_key(id: &str) -> IslandKey {
        IslandKey::Root(id.into())
    }

    /// A 2×2 grid, boxes at (0,0) (10,0) (0,10) (10,10) — a-b on the top
    /// row, c-d on the bottom, matching a two-column packed layout.
    fn grid_2x2() -> Vec<(IslandKey, f32, f32)> {
        vec![(grid_key("a"), 0.0, 0.0), (grid_key("b"), 10.0, 0.0), (grid_key("c"), 0.0, 10.0), (grid_key("d"), 10.0, 10.0)]
    }

    #[test]
    fn nearest_island_moves_down_within_a_column() {
        let boxes = grid_2x2();
        assert_eq!(nearest_island_in_direction(&boxes, &grid_key("a"), GridDirection::Down), Some(grid_key("c")));
    }

    #[test]
    fn nearest_island_moves_right_within_a_row() {
        let boxes = grid_2x2();
        assert_eq!(nearest_island_in_direction(&boxes, &grid_key("a"), GridDirection::Right), Some(grid_key("b")));
    }

    #[test]
    fn nearest_island_is_none_past_the_grid_edge() {
        let boxes = grid_2x2();
        assert_eq!(nearest_island_in_direction(&boxes, &grid_key("a"), GridDirection::Up), None);
        assert_eq!(nearest_island_in_direction(&boxes, &grid_key("a"), GridDirection::Left), None);
    }

    #[test]
    fn nearest_island_prefers_the_nearer_slightly_off_axis_box_over_a_much_farther_aligned_one() {
        // b is close but slightly off to the side; c is perfectly aligned
        // but much farther away. The direction bias should not override a
        // large enough distance gap.
        let boxes = vec![(grid_key("a"), 0.0, 0.0), (grid_key("b"), 1.0, 2.0), (grid_key("c"), 0.0, 12.0)];
        assert_eq!(nearest_island_in_direction(&boxes, &grid_key("a"), GridDirection::Down), Some(grid_key("b")), "the much nearer candidate wins despite being slightly off axis");
    }

    #[test]
    fn nearest_island_prefers_the_aligned_box_once_distances_are_comparable() {
        // Here b and c are close enough in raw distance that the
        // direction bias should tip the result to the better-aligned c.
        let boxes = vec![(grid_key("a"), 0.0, 0.0), (grid_key("b"), 1.0, 2.0), (grid_key("c"), 0.0, 3.0)];
        assert_eq!(nearest_island_in_direction(&boxes, &grid_key("a"), GridDirection::Down), Some(grid_key("c")), "aligned c wins once b's off-axis penalty outweighs its small distance edge");
    }

    #[test]
    fn nearest_island_returns_none_for_an_unknown_current_key() {
        let boxes = grid_2x2();
        assert_eq!(nearest_island_in_direction(&boxes, &grid_key("nowhere"), GridDirection::Down), None);
    }

    fn bare_island(id: &str) -> Island {
        Island { key: grid_key(id), name: id.into(), root: Some(id.into()), members: vec![], summary: Default::default(), machine: None, pending_request: false }
    }

    #[test]
    fn column_positions_stacks_one_columns_boxes_with_a_row_gap() {
        let (a, b) = (bare_island("a"), bare_island("b"));
        let heights = [(&a, 5u16), (&b, 3u16)];
        let positions = column_positions(&heights, 1);
        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0], vec![(&a, 0, 5), (&b, 6, 3)], "b starts after a's 5 rows plus the 1-row gap");
    }

    #[test]
    fn scroll_to_show_island_only_scrolls_as_far_as_needed() {
        let (a, b, c) = (bare_island("a"), bare_island("b"), bare_island("c"));
        let heights = [(&a, 5u16), (&b, 5u16), (&c, 5u16)];
        let positions = column_positions(&heights, 1);
        // a (0..5), b (6..11), c (12..17) in one column.
        assert_eq!(scroll_to_show_island(&positions, 10, 0, &grid_key("a")), 0, "already visible — no scroll");
        assert_eq!(scroll_to_show_island(&positions, 10, 0, &grid_key("c")), 7, "scrolls just enough to bring c's bottom into view");
        assert_eq!(scroll_to_show_island(&positions, 10, 15, &grid_key("a")), 0, "scrolls back up to a's top, not just into view");
        assert_eq!(scroll_to_show_island(&positions, 10, 6, &grid_key("b")), 6, "already fully visible mid-scroll — unchanged");
    }

    #[test]
    fn scroll_to_show_island_is_a_no_op_for_an_unknown_key() {
        let a = bare_island("a");
        let heights = [(&a, 5u16)];
        let positions = column_positions(&heights, 1);
        assert_eq!(scroll_to_show_island(&positions, 10, 3, &grid_key("nowhere")), 3);
    }

    #[test]
    fn cycle_sort_goes_needs_you_most_active_a_to_z_and_back() {
        assert_eq!(cycle_sort(IslandSort::NeedsYouFirst), IslandSort::MostActive);
        assert_eq!(cycle_sort(IslandSort::MostActive), IslandSort::AToZ);
        assert_eq!(cycle_sort(IslandSort::AToZ), IslandSort::NeedsYouFirst);
    }

    #[test]
    fn island_children_only_follows_parents_inside_the_same_island() {
        let rows = vec![
            row("root", None, true, SessionStatus::Working),
            row("mid", Some("root"), true, SessionStatus::Working),
            row("leaf", Some("mid"), false, SessionStatus::Working),
            row("outsider", Some("someone-else"), false, SessionStatus::Working),
        ];
        let mut fleet = FleetState::default();
        fleet.sync(&rows, &HashMap::new());
        let island = fleet.islands.iter().find(|i| i.key == IslandKey::Root("root".into())).unwrap();
        let children = island_children(&rows, island);
        assert_eq!(children.get("root"), Some(&vec!["mid".to_string()]));
        assert_eq!(children.get("mid"), Some(&vec!["leaf".to_string()]));
        assert!(!children.contains_key("someone-else"), "a parent outside this island must not appear");
    }

    #[test]
    fn island_children_never_makes_a_root_its_own_parent() {
        // A test fixture (or a stale DB row) can set a root orchestrator's
        // `orchestrator_id` to its own id — real production data never
        // does, but `island_children` must not trust that and loop forever
        // when something upstream does.
        let rows = vec![row("acme", Some("acme"), true, SessionStatus::Working)];
        let mut fleet = FleetState::default();
        fleet.sync(&rows, &HashMap::new());
        let island = fleet.islands.iter().find(|i| i.key == IslandKey::Root("acme".into())).unwrap();
        let children = island_children(&rows, island);
        assert!(children.get("acme").is_none_or(Vec::is_empty), "{children:?}");
    }

    #[test]
    fn sync_detail_pr_is_none_without_a_pr_on_the_selected_session() {
        let store = Store::open(":memory:").unwrap();
        let rows = vec![row("w1", Some("acme"), false, SessionStatus::Working)];
        let mut fleet = FleetState::default();
        fleet.nav.select("w1");
        fleet.sync_detail_pr(&rows, &store);
        assert!(fleet.detail_pr.is_none());
    }

    #[test]
    fn sync_detail_pr_loads_comments_for_the_selected_sessions_pr() {
        let store = Store::open(":memory:").unwrap();
        store
            .upsert_comment_for_test(&Comment { id: 1, pr_id: 42, author: "octocat".into(), body: "lgtm".into(), path: None, line: None, created_at: 0 })
            .unwrap();
        store
            .upsert_comment_for_test(&Comment { id: 2, pr_id: 99, author: "other".into(), body: "unrelated".into(), path: None, line: None, created_at: 0 })
            .unwrap();

        let mut w1 = ninox_ui_shared::cli::test_fixtures::session("w1", Some("acme"), SessionStatus::PrOpen);
        w1.pr_id = Some(42);
        let rows = vec![Row { session: w1, is_orchestrator: false, group: Some("acme".into()), machine_health: None , queue_position: None, prs: Vec::new()}];

        let mut fleet = FleetState::default();
        fleet.nav.select("w1");
        fleet.sync_detail_pr(&rows, &store);

        let detail = fleet.detail_pr.expect("a pr-bearing selection must populate detail_pr");
        assert_eq!(detail.pr_id, 42);
        assert_eq!(detail.comments.len(), 1);
    }

    #[test]
    fn sync_detail_pr_filters_out_linear_bookkeeping_comments() {
        // Must go through the shared `ninox_overview::comments::visible_comments`
        // filter (#257), not a TUI-local reimplementation, so the TUI and
        // GUI detail panes always agree on what's a Linear comment — and
        // the count (`detail.comments.len()`) must agree with the list,
        // since both read from the same already-filtered field.
        let store = Store::open(":memory:").unwrap();
        store
            .upsert_comment_for_test(&Comment { id: 1, pr_id: 42, author: "linear-code".into(), body: "linear-linkback".into(), path: None, line: None, created_at: 0 })
            .unwrap();
        store
            .upsert_comment_for_test(&Comment { id: 2, pr_id: 42, author: "octocat".into(), body: "looks good".into(), path: None, line: None, created_at: 1 })
            .unwrap();

        let mut w1 = ninox_ui_shared::cli::test_fixtures::session("w1", Some("acme"), SessionStatus::PrOpen);
        w1.pr_id = Some(42);
        let rows = vec![Row { session: w1, is_orchestrator: false, group: Some("acme".into()), machine_health: None , queue_position: None, prs: Vec::new()}];

        let mut fleet = FleetState::default();
        fleet.nav.select("w1");
        fleet.sync_detail_pr(&rows, &store);

        let detail = fleet.detail_pr.expect("a pr-bearing selection must populate detail_pr");
        assert_eq!(detail.comments.len(), 1, "the linear-code comment must be filtered out of both the list and its count: {:?}", detail.comments);
        assert_eq!(detail.comments[0].author, "octocat");
        assert_eq!(detail.comments[0].author, "octocat");
        assert!(detail.pr.is_none(), "no row recorded in the prs table — get_pr legitimately returns None");
    }

    #[test]
    fn tree_rows_pins_needs_you_above_the_rest() {
        let rows = vec![row("acme", None, true, SessionStatus::Working), row("w1", Some("acme"), false, SessionStatus::Working)];
        let out = tree_rows(&rows, &[1], &|_| false);
        assert_eq!(out[0], TreeRow::NeedsYouHeading);
        assert_eq!(out[1], TreeRow::Needs(1));
    }

    #[test]
    fn tree_rows_has_no_needs_you_heading_when_nothing_needs_you() {
        let rows = vec![row("acme", None, true, SessionStatus::Working)];
        let out = tree_rows(&rows, &[], &|_| false);
        assert!(!out.contains(&TreeRow::NeedsYouHeading));
    }

    #[test]
    fn tree_rows_has_no_machine_heading_when_everything_is_local() {
        let rows = vec![row("acme", None, true, SessionStatus::Working), row("w1", Some("acme"), false, SessionStatus::Working)];
        let out = tree_rows(&rows, &[], &|_| false);
        assert!(!out.iter().any(|r| matches!(r, TreeRow::MachineHeading(_))));
    }

    #[test]
    fn tree_rows_shows_this_machine_once_a_remote_session_exists() {
        let mut remote = row("remote-acme", None, true, SessionStatus::Working);
        remote.machine_health = Some(MachineHealth::Connected);
        remote.session.machine_id = Some("m1".into());
        let rows = vec![row("acme", None, true, SessionStatus::Working), remote];
        let out = tree_rows(&rows, &[], &|_| false);
        assert_eq!(out[0], TreeRow::MachineHeading(None), "local machine heads the tree first");
        assert!(out.contains(&TreeRow::MachineHeading(Some("m1".into()))));
    }

    #[test]
    fn tree_rows_folds_done_and_terminated_workers_into_one_summary() {
        let rows = vec![
            row("acme", None, true, SessionStatus::Working),
            row("w1", Some("acme"), false, SessionStatus::Working),
            row("w2", Some("acme"), false, SessionStatus::Done),
            row("w3", Some("acme"), false, SessionStatus::Terminated),
        ];
        let out = tree_rows(&rows, &[], &|_| false);
        assert_eq!(out, vec![TreeRow::Orchestrator(0), TreeRow::Worker(1), TreeRow::Folded { done: 1, stopped: 1 }]);
    }

    #[test]
    fn tree_rows_shows_every_row_when_fold_expanded() {
        let rows = vec![row("acme", None, true, SessionStatus::Working), row("w2", Some("acme"), false, SessionStatus::Done)];
        let out = tree_rows(&rows, &[], &|_| true);
        assert_eq!(out, vec![TreeRow::Orchestrator(0), TreeRow::Worker(1)]);
    }

    #[test]
    fn tree_rows_standalone_sessions_flush_any_pending_fold_first() {
        let rows = vec![
            row("acme", None, true, SessionStatus::Working),
            row("w2", Some("acme"), false, SessionStatus::Done),
            row("solo", None, false, SessionStatus::Working),
        ];
        let out = tree_rows(&rows, &[], &|_| false);
        assert_eq!(out, vec![TreeRow::Orchestrator(0), TreeRow::Folded { done: 1, stopped: 0 }, TreeRow::Standalone(2)]);
    }
}
