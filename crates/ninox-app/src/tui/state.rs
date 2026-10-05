//! Client-owned UI state (selection, focus, modals, scroll, layout) and the
//! pure key/mouse → `Action` reducer. Shared facts (sessions, PRs, cost,
//! pane metadata) are copied in from the store/ptyd by the loop in `mod.rs`.

use std::collections::{HashMap, HashSet};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ninox_core::types::{ActivityState, PrWatch, Session, SessionStatus};
use ninox_ptyd::PaneInfo;
use ratatui::layout::Rect;

use super::backend::RestoreSummary;
use super::keys;
use super::layout::Layout;
use super::palette::Palette;
use super::pane::PaneView;
use super::report::{ReportButton, ReportSlot};
use super::settings::{Change, Kind, SettingsView};

#[derive(Clone, Debug)]
pub struct Row {
    pub session: Session,
    pub is_orchestrator: bool,
    /// Id of the orchestrator whose group this row belongs to (its own id
    /// for an orchestrator row); `None` for ungrouped/orphan sessions.
    pub group: Option<String>,
}

impl Row {
    pub fn id(&self) -> &str {
        &self.session.id
    }

    pub fn status(&self) -> &'static str {
        crate::status_slug(&self.session.status)
    }

    #[cfg(test)]
    pub fn test_row(id: &str) -> Self {
        Self {
            session: crate::test_fixtures::session(id, None, SessionStatus::Working),
            is_orchestrator: false,
            group: None,
        }
    }
}

/// Rolled-up "does this need me" level, highest wins (herdr's ordering):
/// an idle agent you have not looked at since it went idle is `Done`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Attention {
    Unknown,
    Idle,
    Working,
    Done,
    Blocked,
}

/// Why a session is pinned under "Needs you", most urgent first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Need {
    Blocked,
    CiFailed,
    Interrupted,
    Unread(u64),
    Review,
    Mergeable,
}

impl Need {
    pub fn label(self) -> String {
        match self {
            Self::Blocked => "blocked".into(),
            Self::CiFailed => "CI failed".into(),
            Self::Interrupted => "interrupted".into(),
            Self::Unread(n) => format!("✉ {n}"),
            Self::Review => "review".into(),
            Self::Mergeable => "mergeable".into(),
        }
    }
}

/// One line of the sidebar. Built by `TuiState::sidebar_items`, laid out
/// once per frame, and shared by drawing and mouse hit-testing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SideItem {
    Heading(Section),
    Gap,
    /// A session (`rows[row]`); `pinned` is its copy under "Needs you".
    /// An orchestrator's own row is its group's header.
    Session { row: usize, pinned: bool },
    /// Header for a group whose orchestrator has no session row.
    Group(String),
    /// "+n more" under a "Needs you" list cut short by the sidebar height.
    More(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    NeedsYou,
    Standalone,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Ptyd { alive: bool },
    /// Not known to ptyd: a tmux-backed session (or a dead one).
    Legacy,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Pending {
    Kill(String),
    /// Delete an ended session's record and worktree.
    Remove(String),
    Reap(String),
    /// Delete a brain entry's markdown file (by entry id) and reindex.
    DeleteBrain(String),
    /// Restart every live worker and orchestrator session (fleet-wide, not
    /// scoped to the selected row) — same batch path as `ninox restart --all`.
    RestartAll,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum View {
    #[default]
    Board,
    Overview,
    PrWatches,
    Brain,
    Settings,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Focus {
    #[default]
    Sidebar,
    Pane,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Normal,
    Prefix,
    Scroll,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SpawnModal {
    pub name: String,
    pub prompt: String,
    pub on_prompt_field: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GotoFilter {
    #[default]
    All,
    Blocked,
    Done,
    Working,
    Idle,
}

impl GotoFilter {
    const ORDER: [GotoFilter; 5] = [Self::All, Self::Blocked, Self::Done, Self::Working, Self::Idle];

    pub fn default_order() -> &'static [GotoFilter] {
        &Self::ORDER
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Working => "working",
            Self::Idle => "idle",
        }
    }

    fn cycle(self, back: bool) -> Self {
        let i = Self::ORDER.iter().position(|f| *f == self).unwrap_or(0);
        let n = Self::ORDER.len();
        Self::ORDER[if back { (i + n - 1) % n } else { (i + 1) % n }]
    }

    fn admits(self, a: Attention) -> bool {
        match self {
            Self::All => true,
            Self::Blocked => a == Attention::Blocked,
            Self::Done => a == Attention::Done,
            Self::Working => a == Attention::Working,
            Self::Idle => matches!(a, Attention::Idle | Attention::Done),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Goto {
    pub query: String,
    pub filter: GotoFilter,
    pub selected: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Modal {
    Confirm(Pending),
    Spawn(SpawnModal),
    Goto(Goto),
    Help,
    Restore(RestoreSummary),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Notice {
    pub text: String,
    pub level: Level,
    pub expires_ms: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PtydState {
    #[default]
    Unknown,
    Up,
    Down(String),
}

pub use super::brain::BrainView;

pub struct TuiState {
    pub rows: Vec<Row>,
    pub selected: usize,
    /// The cursor is on `selected`'s pinned copy under "Needs you" rather
    /// than its place in the tree; j/k walk the pinned block then the tree.
    pub selected_pinned: bool,
    pub view: View,
    pub focus: Focus,
    pub mode: Mode,
    pub modal: Option<Modal>,
    pub zoom: bool,
    pub inspector: bool,
    pub overview_sel: usize,
    pub prefix: u8,
    pub daemon_up: bool,
    pub ptyd: PtydState,
    pub panes: HashMap<String, PaneInfo>,
    pub views: HashMap<String, PaneView>,
    pub pr_watches: Vec<PrWatch>,
    pub prs: super::prs::PrsView,
    pub msg_counts: HashMap<String, u64>,
    pub msg_seen: HashMap<String, u64>,
    pub seen_ms: HashMap<String, i64>,
    pub started_ms: i64,
    pub now_ms: i64,
    pub notice: Option<Notice>,
    pub brain: BrainView,
    pub settings: SettingsView,
    /// Geometry of the last frame, for mouse hit-testing and paging.
    pub layout: Layout,
    /// Wheel-entered scroll mode leaves itself when scrolled back to live.
    pub scroll_by_wheel: bool,
    pub restore_policy: ninox_core::config::RestorePolicy,
    /// `flagged_at` of the pending-restore flag already offered, so an open
    /// offer (or a restore still running) isn't offered again.
    pub restore_offered: Option<Option<i64>>,
    /// This TUI's viewer panes (ptyd panes running `tmux attach`), keyed by
    /// the tmux-backed session each shows.
    pub viewers: HashMap<String, PaneInfo>,
    /// Why a session's viewer is not running (start failed, or the tmux
    /// client exited); cleared when the user reopens the row.
    pub viewer_errors: HashMap<String, String>,
    /// Set when viewers can't run at all (no ptyd host): opening a tmux row
    /// falls back to a full-screen attach.
    pub viewers_unavailable: Option<String>,
    pub selection: Option<Selection>,
    /// A press forwarded to the agent: its drags and release follow it there
    /// even outside the pane.
    pub mouse_grab: Option<String>,
    pub last_click: Option<Click>,
    /// Shown once the notice slot is free.
    pub deferred_notice: Option<(Level, String)>,
    pub palette: Palette,
    /// Orchestrator groups folded in the sidebar (client-side only).
    pub collapsed: HashSet<String>,
    /// Reports for sessions with no live process, by session id.
    pub reports: HashMap<String, ReportSlot>,
    /// Lines the report body is scrolled down.
    pub report_scroll: u16,
    /// Uncommitted-change counts by worker id, gathered off the loop while
    /// an orchestrator's remove confirm is open.
    pub uncommitted: HashMap<String, usize>,
}

/// What removing an orchestrator would touch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemoveScope {
    pub ended: usize,
    pub live: usize,
    /// Ended / live workers with uncommitted changes; `None` until checked.
    pub dirty_ended: Option<usize>,
    pub dirty_live: Option<usize>,
}

/// Drag-selection in a pane's viewport, `(row, col)` relative to its inner
/// area.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    pub pane: String,
    pub anchor: (u16, u16),
    pub cursor: (u16, u16),
    /// Released: stays highlighted until the next click or key.
    pub done: bool,
}

impl Selection {
    pub fn dragged(&self) -> bool {
        self.anchor != self.cursor
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Click {
    pub x: u16,
    pub y: u16,
    pub ms: i64,
}

pub const DOUBLE_CLICK_MS: i64 = 400;

/// Always leaves an agent pane, whatever the prefix: `Ctrl+]` (GS). The prefix
/// alone is not enough — macOS can claim Ctrl-Space for input sources, and
/// then nothing would get the user out. Claude Code binds nothing to it.
pub const ESCAPE_BYTE: u8 = 0x1d;

impl Default for TuiState {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            selected: 0,
            selected_pinned: false,
            view: View::Board,
            focus: Focus::Sidebar,
            mode: Mode::Normal,
            modal: None,
            zoom: false,
            inspector: false,
            overview_sel: 0,
            prefix: ninox_core::config::DEFAULT_PREFIX_BYTE,
            daemon_up: false,
            ptyd: PtydState::Unknown,
            panes: HashMap::new(),
            views: HashMap::new(),
            pr_watches: Vec::new(),
            prs: Default::default(),
            msg_counts: HashMap::new(),
            msg_seen: HashMap::new(),
            seen_ms: HashMap::new(),
            started_ms: 0,
            now_ms: 0,
            notice: None,
            brain: BrainView::default(),
            settings: SettingsView::default(),
            layout: Layout::default(),
            scroll_by_wheel: false,
            restore_policy: Default::default(),
            restore_offered: None,
            viewers: HashMap::new(),
            viewer_errors: HashMap::new(),
            viewers_unavailable: None,
            selection: None,
            mouse_grab: None,
            last_click: None,
            deferred_notice: None,
            palette: Palette::default(),
            collapsed: HashSet::new(),
            reports: HashMap::new(),
            report_scroll: 0,
            uncommitted: HashMap::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    None,
    /// Declined the restore prompt: clear the engine's pending-restore flag.
    DismissRestore,
    Quit,
    /// Full-screen attach, suspending the TUI: explicit (prefix a), or for
    /// a tmux session when no viewer pane can run.
    Connect(String),
    Kill(String),
    /// Remove an ended session; for an orchestrator, its ended workers too
    /// (live ones keep running, detached).
    Remove(String),
    /// Remove an orchestrator and every worker, stopping the live ones.
    RemoveAll(String),
    Reap(String),
    /// Relaunch an ended session under its own id, `--resume`d.
    Resume(String),
    OpenUrl(String),
    Spawn { name: String, prompt: Option<String> },
    Write { pane: String, bytes: Vec<u8> },
    LoadBrain(String),
    /// Semantic (embedding) search for the query, merged after the text hits.
    SearchBrain(String),
    /// Suspend the TUI for `$VISUAL`/`$EDITOR` on a brain entry's markdown,
    /// then reindex it off the loop.
    EditBrain(String),
    /// Edit a new-entry template (type, tags) and add it if filled in.
    NewBrain { entry_type: String, tags: Vec<String> },
    DeleteBrain(String),
    LoadSettings,
    /// Read `[pr_watch] enabled` and refresh the PR list.
    LoadPrs,
    /// Read → apply → write one settings change.
    SaveSetting(Change),
    /// Suspend the TUI and open `config.toml` in `$VISUAL`/`$EDITOR`.
    EditConfig,
    Restore,
    Yank(String),
    /// Restart every live worker and orchestrator session — same batch path
    /// as `ninox restart --all`.
    RestartAll,
}

pub const NOTICE_MS: i64 = 5_000;

/// The help overlay, grouped by where you are: `Ctrl+]` first, since it is
/// the one chord that works whatever the prefix and whatever macOS takes.
pub const HELP: &[(&str, &[(&str, &str)])] = &[
    ("Inside an agent pane (typing goes to the agent)", &[
        ("Ctrl+]", "back to the fleet list (always works); then the keys below"),
        ("click ◀ fleet · ✕ kill · ⤢ zoom", "pane header buttons, always handled by ninox"),
        ("prefix + key", "any fleet-list key below without leaving the pane"),
        ("prefix prefix / prefix Ctrl+]", "type the prefix / Ctrl+] into the agent"),
    ]),
    ("From the fleet list (bare keys)", &[
        ("↵ / l / c", "open the agent (tmux sessions too, in the pane)"),
        ("j k / arrows", "move; ! jumps to Needs you"),
        ("space", "fold / unfold an orchestrator's workers"),
        ("x  (or click ✕)", "kill a live agent / remove an ended one; asks first"),
        ("", "removing an orchestrator removes its workers too"),
        ("r · O · R", "resume · open the PR · reap finished workers"),
        ("Ctrl+R", "restart every live agent (tooling update); asks first"),
        ("n · g · o", "spawn orchestrator · go to · overview grid"),
        ("z · i · [", "zoom · inspector · scroll mode (j/k, g/G, y copies)"),
        ("a", "attach full-screen (tmux: Ctrl+b d returns)"),
        ("1-5  (p b s)", "tabs: fleet · overview · PRs · brain · settings"),
        ("d / q", "detach (agents keep running)"),
    ]),
    ("PRs tab (bare keys)", &[
        ("j k · ↵ / o · click #n or ↗ Open", "move · open the PR in the browser"),
        ("s  (or click the session)", "show the PR's session on the fleet tab"),
    ]),
    ("Brain tab (bare keys)", &[
        ("j k · ↵ / l · h", "move · read or fold · up to the group"),
        ("space · t", "fold a group · group by tag / type / flat"),
        ("e · a · D", "edit in $EDITOR · new entry · delete (asks first)"),
        ("/", "search; matching groups unfold"),
    ]),
    ("Mouse", &[
        ("click", "tabs, sidebar, footer and pane buttons go to ninox"),
        ("right-click a row", "kill / remove it (asks first)"),
        ("drag / double-click", "select and copy (Shift+drag over mouse apps)"),
        ("wheel", "scrolls the pane, sidebar, brain or settings under it"),
    ]),
];

impl TuiState {
    pub fn notify(&mut self, level: Level, text: impl Into<String>) {
        let ttl = if level == Level::Error { NOTICE_MS * 2 } else { NOTICE_MS };
        self.notice = Some(Notice { text: text.into(), level, expires_ms: self.now_ms + ttl });
    }

    pub fn expire_notice(&mut self) {
        if self.notice.as_ref().is_some_and(|n| n.expires_ms <= self.now_ms) {
            self.notice = None;
        }
        if self.notice.is_none() {
            if let Some((level, text)) = self.deferred_notice.take() {
                self.notice = Some(Notice { text, level, expires_ms: self.now_ms + NOTICE_MS * 3 });
            }
        }
    }

    pub fn selected_row(&self) -> Option<&Row> {
        self.rows.get(self.selected)
    }

    pub fn selected_id(&self) -> Option<String> {
        self.selected_row().map(|r| r.session.id.clone())
    }

    pub fn backend_of(&self, id: &str) -> Backend {
        match self.panes.get(id) {
            Some(p) => Backend::Ptyd { alive: p.alive },
            None => Backend::Legacy,
        }
    }

    /// The ptyd pane that renders session `id` and takes its input: its own
    /// pane, or for a tmux session the viewer pane attached to it.
    pub fn pane_target(&self, id: &str) -> Option<String> {
        match self.backend_of(id) {
            Backend::Ptyd { .. } => Some(id.to_string()),
            Backend::Legacy => self.viewers.get(id).filter(|v| v.alive).map(|v| v.pane.clone()),
        }
    }

    /// Host metadata for a ptyd pane id (session or viewer).
    pub fn pane_info(&self, pane: &str) -> Option<&PaneInfo> {
        match ninox_core::runtime::parse_viewer_pane(pane) {
            Some((_, session)) => self.viewers.get(session).filter(|v| v.pane == pane),
            None => self.panes.get(pane),
        }
    }

    /// The pane shown in the board's main area, when one renders there.
    pub fn focused_pane(&self) -> Option<String> {
        self.pane_target(self.selected_row()?.id())
    }

    /// Sessions on screen that need a tmux viewer pane started.
    pub fn wanted_viewers(&self) -> Vec<String> {
        let needs = |r: &Row| {
            self.backend_of(r.id()) == Backend::Legacy
                && !r.session.status.is_terminal()
                && !self.viewer_errors.contains_key(r.id())
        };
        match self.view {
            View::Board => self.selected_row().filter(|r| needs(r)).map(|r| r.id().to_string()).into_iter().collect(),
            View::Overview => {
                let rows = self.overview_rows();
                self.layout
                    .tiles
                    .iter()
                    .filter_map(|t| rows.get(t.index).and_then(|&i| self.rows.get(i)))
                    .filter(|r| needs(r))
                    .map(|r| r.id().to_string())
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    pub fn modes_of(&self, id: &str) -> ninox_ptyd::Modes {
        self.views.get(id).and_then(|v| v.live.as_ref()).map(|s| s.modes).unwrap_or_default()
    }

    pub fn unread(&self, id: &str) -> u64 {
        let total = self.msg_counts.get(id).copied().unwrap_or(0);
        total.saturating_sub(self.msg_seen.get(id).copied().unwrap_or(total))
    }

    pub fn attention(&self, row: &Row) -> Attention {
        let s = &row.session;
        match s.activity {
            ActivityState::Blocked => Attention::Blocked,
            ActivityState::Working => Attention::Working,
            ActivityState::Idle => {
                let seen = self.seen_ms.get(&s.id).copied().unwrap_or(self.started_ms);
                if s.activity_since.is_some_and(|t| t > seen) {
                    Attention::Done
                } else {
                    Attention::Idle
                }
            }
            ActivityState::Unknown => match s.status {
                SessionStatus::Spawning | SessionStatus::Working => Attention::Working,
                _ => Attention::Unknown,
            },
        }
    }

    /// Highest attention across an orchestrator's group (itself included).
    pub fn rollup(&self, group: &str) -> Attention {
        self.rows
            .iter()
            .filter(|r| r.group.as_deref() == Some(group))
            .map(|r| self.attention(r))
            .max()
            .unwrap_or(Attention::Unknown)
    }

    /// Whether (and why) a session belongs under "Needs you".
    pub fn needs_you(&self, r: &Row) -> Option<Need> {
        let s = &r.session;
        if s.status == SessionStatus::Interrupted {
            return Some(Need::Interrupted);
        }
        if s.status.is_terminal() {
            return None;
        }
        if s.activity == ActivityState::Blocked {
            return Some(Need::Blocked);
        }
        if s.status == SessionStatus::CiFailed {
            return Some(Need::CiFailed);
        }
        let unread = self.unread(r.id());
        if unread > 0 {
            return Some(Need::Unread(unread));
        }
        match s.status {
            SessionStatus::ReviewPending => Some(Need::Review),
            SessionStatus::Mergeable => Some(Need::Mergeable),
            _ => None,
        }
    }

    /// Rows pinned under "Needs you", most urgent reason first.
    pub fn needs_rows(&self) -> Vec<usize> {
        let rank = |n: Need| match n {
            Need::Blocked => 0,
            Need::CiFailed => 1,
            Need::Interrupted => 2,
            Need::Unread(_) => 3,
            Need::Review => 4,
            Need::Mergeable => 5,
        };
        let mut v: Vec<(usize, usize)> =
            self.rows.iter().enumerate().filter_map(|(i, r)| self.needs_you(r).map(|n| (rank(n), i))).collect();
        v.sort();
        v.into_iter().map(|(_, i)| i).collect()
    }

    /// The fleet tree: orchestrator groups (workers hidden when folded),
    /// then standalone sessions under their own heading.
    pub fn sidebar_items(&self) -> Vec<SideItem> {
        let mut out = Vec::new();
        let mut group: Option<&str> = None;
        let mut standalone = false;
        let any_group = self.rows.iter().any(|r| r.group.is_some());
        for (i, r) in self.rows.iter().enumerate() {
            match r.group.as_deref() {
                Some(g) => {
                    if group != Some(g) {
                        if !out.is_empty() {
                            out.push(SideItem::Gap);
                        }
                        if !r.is_orchestrator {
                            out.push(SideItem::Group(g.to_string()));
                        }
                        group = Some(g);
                    }
                    if r.is_orchestrator || !self.collapsed.contains(g) {
                        out.push(SideItem::Session { row: i, pinned: false });
                    }
                }
                None => {
                    if !standalone && any_group {
                        if !out.is_empty() {
                            out.push(SideItem::Gap);
                        }
                        out.push(SideItem::Heading(Section::Standalone));
                    }
                    standalone = true;
                    group = None;
                    out.push(SideItem::Session { row: i, pinned: false });
                }
            }
        }
        out
    }

    /// Rows j/k step through: the tree in sidebar order, folded workers out.
    pub fn nav_rows(&self) -> Vec<usize> {
        self.sidebar_items()
            .into_iter()
            .filter_map(|it| match it {
                SideItem::Session { row, pinned: false } => Some(row),
                _ => None,
            })
            .collect()
    }

    /// Pinned "Needs you" rows the keyboard can reach: those the last frame
    /// showed (the block is cut short on short terminals), or all of them
    /// before the first frame.
    fn nav_pinned(&self) -> Vec<usize> {
        let shown: Vec<usize> = self
            .layout
            .needs_items
            .iter()
            .filter_map(|it| match it {
                SideItem::Session { row, pinned: true } => Some(*row),
                _ => None,
            })
            .collect();
        let needs = self.needs_rows();
        if shown.is_empty() { needs } else { shown.into_iter().filter(|r| needs.contains(r)).collect() }
    }

    /// The full keyboard order: pinned block first, then the tree.
    fn nav_entries(&self) -> Vec<(usize, bool)> {
        let mut nav: Vec<(usize, bool)> = self.nav_pinned().into_iter().map(|r| (r, true)).collect();
        nav.extend(self.nav_rows().into_iter().map(|r| (r, false)));
        nav
    }

    fn step_selection(&mut self, delta: i64) {
        let nav = self.nav_entries();
        if nav.is_empty() {
            return;
        }
        let pinned = self.cursor_pinned();
        let pos = nav
            .iter()
            .position(|&e| e == (self.selected, pinned))
            .or_else(|| nav.iter().position(|&(r, p)| !p && r == self.selected))
            .unwrap_or(0) as i64;
        let next = (pos + delta).clamp(0, nav.len() as i64 - 1) as usize;
        let (row, pinned) = nav[next];
        self.select(row);
        self.selected_pinned = pinned;
    }

    /// Whether the cursor sits on the pinned copy — only while the selected
    /// session still needs you; otherwise the tree row carries it.
    pub fn cursor_pinned(&self) -> bool {
        self.selected_pinned && self.needs_rows().contains(&self.selected)
    }

    /// Jump to the most urgent "Needs you" row.
    pub fn select_most_urgent(&mut self) -> bool {
        let Some(&row) = self.nav_pinned().first() else { return false };
        self.select(row);
        self.selected_pinned = true;
        true
    }

    /// Fold or unfold the selected row's orchestrator group.
    pub fn toggle_collapse(&mut self, group: &str) {
        if !self.collapsed.remove(group) {
            self.collapsed.insert(group.to_string());
            let hidden = self.selected_row().is_some_and(|r| !r.is_orchestrator && r.group.as_deref() == Some(group));
            if hidden {
                if let Some(i) = self.rows.iter().position(|r| r.is_orchestrator && r.group.as_deref() == Some(group)) {
                    self.selected = i;
                }
            }
        }
    }

    /// Unfold the group hiding `rows[idx]`, so a selection is never invisible.
    fn reveal(&mut self, idx: usize) {
        if let Some(g) = self.rows.get(idx).filter(|r| !r.is_orchestrator).and_then(|r| r.group.clone()) {
            self.collapsed.remove(&g);
        }
    }

    /// Mark what is on screen as seen: clears unread and Done badges.
    pub fn mark_displayed_seen(&mut self) {
        let ids: Vec<String> = match self.view {
            View::Board => self.selected_id().into_iter().collect(),
            _ => Vec::new(),
        };
        for id in ids {
            if let Some(&n) = self.msg_counts.get(&id) {
                self.msg_seen.insert(id.clone(), n);
            }
            self.seen_ms.insert(id, self.now_ms);
        }
    }

    /// Row indices shown in the overview grid: live agents first-class.
    pub fn overview_rows(&self) -> Vec<usize> {
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, r)| !r.session.status.is_terminal() || self.panes.contains_key(r.id()))
            .map(|(i, _)| i)
            .collect()
    }

    pub fn goto_matches(&self, g: &Goto) -> Vec<usize> {
        let words: Vec<String> = g.query.to_lowercase().split_whitespace().map(str::to_string).collect();
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, r)| g.filter.admits(self.attention(r)))
            .filter(|(_, r)| {
                let s = &r.session;
                let hay = format!(
                    "{} {} {} {} {} {}",
                    s.id,
                    s.name,
                    s.repo,
                    r.status(),
                    s.summary.as_deref().unwrap_or(""),
                    s.pr_number.map(|n| format!("#{n}")).unwrap_or_default()
                )
                .to_lowercase();
                words.iter().all(|w| hay.contains(w))
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Replace the rows from a fresh store read, carrying the selection by
    /// id and returning notifications for transitions worth surfacing.
    pub fn apply_rows(&mut self, rows: Vec<Row>) {
        let keep = self.selected_id();
        let prev: HashMap<String, Session> =
            self.rows.drain(..).map(|r| (r.session.id.clone(), r.session)).collect();
        let mut notes = Vec::new();
        if !prev.is_empty() {
            for r in &rows {
                let Some(old) = prev.get(r.id()) else { continue };
                let s = &r.session;
                let name = &s.name;
                if s.activity == ActivityState::Blocked && old.activity != ActivityState::Blocked {
                    let note = s.activity_note.as_deref().map(|n| format!(": {n}")).unwrap_or_default();
                    notes.push((Level::Warn, format!("{name} is blocked{note}")));
                }
                if s.status != old.status {
                    let pr = s.pr_number.map(|n| format!(" #{n}")).unwrap_or_default();
                    match s.status {
                        SessionStatus::CiFailed => notes.push((Level::Error, format!("CI failed on {name}{pr}"))),
                        SessionStatus::Mergeable => notes.push((Level::Info, format!("{name}{pr} is mergeable"))),
                        SessionStatus::Done => notes.push((Level::Info, format!("{name} done"))),
                        SessionStatus::PrOpen if old.status == SessionStatus::Working => {
                            notes.push((Level::Info, format!("{name} opened PR{pr}")))
                        }
                        _ => {}
                    }
                }
            }
        }
        self.rows = rows;
        let kept = keep.as_deref().map(|id| self.rows.iter().position(|r| r.id() == id));
        self.selected = kept.flatten().unwrap_or(self.selected).min(self.rows.len().saturating_sub(1));
        // The selection now falls on a different agent: keys typed at the
        // pane must not reach it.
        if let Some(None) = kept {
            if self.focus == Focus::Pane || self.mode == Mode::Scroll {
                self.focus = Focus::Sidebar;
                self.mode = Mode::Normal;
                self.zoom = false;
                notes.push((Level::Info, format!("{} is gone — back to the sidebar", keep.unwrap_or_default())));
            }
        }
        self.clamp_overview_sel();
        if let Some((level, text)) = notes.pop() {
            let more = if notes.is_empty() { String::new() } else { format!(" (+{} more)", notes.len()) };
            self.notify(level, format!("{text}{more}"));
        }
    }

    /// Offer a pending fleet restore once per flag. Called every tick: a
    /// daemon cold-started by this TUI raises the flag only after its
    /// poller has reconciled, well after the TUI is up.
    pub fn offer_restore(&mut self, pending: Option<RestoreSummary>) {
        use ninox_core::config::RestorePolicy;
        let Some(summary) = pending else { return };
        if self.restore_offered == Some(summary.flagged_at) {
            return;
        }
        match self.restore_policy {
            RestorePolicy::Prompt => {
                if self.modal.is_some() {
                    return;
                }
                self.modal = Some(Modal::Restore(summary.clone()));
            }
            // The engine restores on boot; the TUI only reports it.
            RestorePolicy::Auto => self.notify(
                Level::Info,
                format!("restoring fleet ({} workers, {} orchestrators)…", summary.workers, summary.orchestrators),
            ),
            RestorePolicy::Manual => {}
        }
        self.restore_offered = Some(summary.flagged_at);
    }

    /// The overview list shrinks with `rows` and with `panes` (an exited
    /// pane's terminal session drops out).
    pub fn clamp_overview_sel(&mut self) {
        self.overview_sel = self.overview_sel.min(self.overview_rows().len().saturating_sub(1));
    }

    fn page(&self) -> usize {
        self.layout.pane_inner.map(|r| r.height as usize).unwrap_or(20).max(2)
    }

    fn set_scroll(&mut self, id: &str, scroll: usize) {
        let v = self.views.entry(id.to_string()).or_default();
        v.scroll = scroll;
    }

    fn scroll_of(&self, id: &str) -> usize {
        self.views.get(id).map(|v| v.scroll).unwrap_or(0)
    }

    fn select(&mut self, idx: usize) {
        if !self.rows.is_empty() {
            self.selected_pinned = false;
            self.selected = idx.min(self.rows.len() - 1);
            self.reveal(self.selected);
            self.mode = if self.mode == Mode::Scroll { Mode::Normal } else { self.mode };
        }
    }

    /// Open the selected agent in the pane. A tmux session shows through a
    /// viewer pane; only when none can run does it attach full-screen.
    fn open_selected(&mut self) -> Action {
        let Some(id) = self.selected_id() else { return Action::None };
        self.view = View::Board;
        self.reveal(self.selected);
        let legacy_live = self.backend_of(&id) == Backend::Legacy && !self.selected_row().is_some_and(|r| r.session.status.is_terminal());
        if legacy_live && self.viewers_unavailable.is_some() {
            return Action::Connect(id);
        }
        // Reopening retries a viewer that failed or was detached.
        self.viewer_errors.remove(&id);
        self.focus = Focus::Pane;
        Action::None
    }

    fn leave_pane(&mut self) {
        self.view = View::Board;
        self.focus = Focus::Sidebar;
        self.mode = Mode::Normal;
        self.zoom = false;
        self.mouse_grab = None;
    }

    /// Whether the board's pane shows the selected session's report (it
    /// has no live process to show or type into).
    pub fn report_shown(&self) -> bool {
        self.view == View::Board && self.selected_row().is_some_and(|r| super::report::shows_report(self, r))
    }

    /// `x`: remove an ended session, kill a live one; both ask first.
    fn ask_remove_or_kill(&mut self) {
        let Some(r) = self.selected_row() else { return };
        let id = r.id().to_string();
        let pending = if super::report::shows_report(self, r) { Pending::Remove(id) } else { Pending::Kill(id) };
        self.modal = Some(Modal::Confirm(pending));
    }

    fn resume_selected(&mut self) -> Action {
        let Some(r) = self.selected_row() else { return Action::None };
        let (id, name) = (r.id().to_string(), r.session.name.clone());
        if !super::report::shows_report(self, r) {
            self.notify(Level::Info, format!("{name} is still running"));
            return Action::None;
        }
        match self.reports.get(&id).and_then(|s| s.data.as_ref()) {
            Some(d) if d.resumable => {
                self.notify(Level::Info, format!("resuming {name}…"));
                Action::Resume(id)
            }
            Some(_) => {
                self.notify(Level::Info, format!("{name} can't be resumed: no workspace or conversation id, or the harness has no resume_args"));
                Action::None
            }
            None => {
                self.notify(Level::Info, "still reading the session — try again in a moment");
                Action::None
            }
        }
    }

    fn open_selected_pr(&mut self) -> Action {
        let Some(id) = self.selected_id() else { return Action::None };
        match super::report::pr_url(self, &id) {
            Some(url) => Action::OpenUrl(url),
            None => {
                self.notify(Level::Info, "no PR link for this session");
                Action::None
            }
        }
    }

    pub fn report_button(&mut self, b: ReportButton) -> Action {
        match b {
            ReportButton::Remove => {
                self.ask_remove_or_kill();
                Action::None
            }
            ReportButton::Resume => self.resume_selected(),
            ReportButton::OpenPr => self.open_selected_pr(),
        }
    }

    pub fn is_orchestrator_id(&self, id: &str) -> bool {
        self.rows.iter().any(|r| r.is_orchestrator && r.id() == id)
    }

    pub fn remove_scope(&self, orch: &str) -> RemoveScope {
        let workers: Vec<&Row> = self.rows.iter().filter(|r| !r.is_orchestrator && r.group.as_deref() == Some(orch)).collect();
        // Matches `Engine::remove_orchestrator_keeping_live`: only Done /
        // Terminated workers with no live pane go; Interrupted ones are
        // resumable and are kept with the live ones.
        let finished = |r: &Row| matches!(r.session.status, SessionStatus::Done | SessionStatus::Terminated);
        let (ended, live): (Vec<&Row>, Vec<&Row>) =
            workers.into_iter().partition(|r| finished(r) && super::report::shows_report(self, r));
        let dirty = |rows: &[&Row]| -> Option<usize> {
            rows.iter().map(|r| self.uncommitted.get(r.id()).map(|&n| usize::from(n > 0))).sum()
        };
        RemoveScope { ended: ended.len(), live: live.len(), dirty_ended: dirty(&ended), dirty_live: dirty(&live) }
    }

    /// The confirm modal offers `a`: remove everything, live workers too.
    pub fn confirm_offers_all(&self, pending: &Pending) -> bool {
        matches!(pending, Pending::Remove(id) if self.is_orchestrator_id(id) && self.remove_scope(id).live > 0)
    }

    /// How many rows "restart all" would actually target — same definition
    /// `restart --all` uses (anything not `is_terminal()`), checked against
    /// the row cache rather than a live tmux probe, so it's only an
    /// estimate for display; `restart::execute` re-checks liveness itself.
    pub fn live_count(&self) -> usize {
        self.rows.iter().filter(|r| !r.session.status.is_terminal()).count()
    }

    /// The confirm modal's question and a line on what it does.
    pub fn confirm_text(&self, pending: &Pending) -> (String, String) {
        let name = |id: &str| self.rows.iter().find(|r| r.id() == id).map(|r| r.session.name.clone()).unwrap_or_else(|| id.to_string());
        let plural = |n: usize| if n == 1 { "" } else { "s" };
        match pending {
            Pending::Kill(id) if self.is_orchestrator_id(id) => {
                (format!("Kill orchestrator {}?", name(id)), "stops the orchestrator; its workers keep running".into())
            }
            Pending::Kill(id) => (format!("Kill {}?", name(id)), "stops the agent; its worktree and branch stay".into()),
            Pending::Remove(id) if self.is_orchestrator_id(id) => {
                let sc = self.remove_scope(id);
                let dirty = |n: Option<usize>, who: &str| match n {
                    None => format!("{who} may have uncommitted work"),
                    Some(0) => String::new(),
                    Some(n) => format!("{n} {who} with uncommitted changes"),
                };
                let q = format!("Remove {} and its {} ended worker{}?", name(id), sc.ended, plural(sc.ended));
                let mut parts = Vec::new();
                if sc.ended > 0 {
                    parts.push(dirty(sc.dirty_ended, "ended"));
                }
                if sc.live > 0 {
                    parts.push(format!("{} live or resumable worker{} kept — a removes them too", sc.live, plural(sc.live)));
                    parts.push(dirty(sc.dirty_live, "live"));
                }
                parts.retain(|p| !p.is_empty());
                parts.push("branches stay".into());
                (q, parts.join("; "))
            }
            Pending::Remove(id) => (format!("Remove {}?", name(id)), "deletes its record and worktree; the branch stays".into()),
            Pending::Reap(id) => (format!("Reap the finished workers of {}?", name(id)), "removes every ended worker in the group".into()),
            Pending::DeleteBrain(id) => {
                (format!("Delete brain entry {}?", self.brain.name_of(id)), "deletes its markdown file, reindexes, and syncs the removal to any remote".into())
            }
            Pending::RestartAll => {
                let n = self.live_count();
                (
                    format!("Restart all {n} live agent{}?", plural(n)),
                    "interrupts every running session at once to pick up tooling updates; conversations resume where possible".into(),
                )
            }
        }
    }

    /// Switch to a header tab.
    pub fn switch_view(&mut self, view: View) -> Action {
        self.mode = Mode::Normal;
        self.selection = None;
        match view {
            View::Board => {
                self.view = View::Board;
                Action::None
            }
            View::Overview => {
                if self.view != View::Overview {
                    let rows = self.overview_rows();
                    self.overview_sel = rows.iter().position(|&i| i == self.selected).unwrap_or(0);
                }
                self.view = View::Overview;
                Action::None
            }
            View::PrWatches => {
                self.view = View::PrWatches;
                Action::LoadPrs
            }
            View::Brain => {
                self.view = View::Brain;
                Action::LoadBrain(self.brain.query.clone())
            }
            View::Settings => {
                self.view = View::Settings;
                Action::LoadSettings
            }
        }
    }
}

/// `1`..`5` select the header tabs.
fn tab_key(key: &KeyEvent) -> Option<View> {
    if key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
        return None;
    }
    let KeyCode::Char(c @ '1'..='5') = key.code else { return None };
    Some(super::layout::TABS[(c as u8 - b'1') as usize].0)
}

pub fn handle_key(st: &mut TuiState, key: KeyEvent) -> Action {
    if st.modal.is_some() {
        return handle_modal_key(st, key);
    }
    if st.selection.as_ref().is_some_and(|s| s.done) {
        st.selection = None;
    }
    let escape = st.prefix != ESCAPE_BYTE && keys::is_prefix(&key, ESCAPE_BYTE);
    if st.mode == Mode::Prefix {
        st.mode = Mode::Normal;
        if keys::is_prefix(&key, st.prefix) || escape {
            let byte = if escape { ESCAPE_BYTE } else { st.prefix };
            return match (st.view, st.focus, st.focused_pane()) {
                (View::Board, Focus::Pane, Some(id)) => Action::Write { pane: id, bytes: vec![byte] },
                _ => Action::None,
            };
        }
        return command(st, key);
    }
    if escape {
        st.leave_pane();
        return Action::None;
    }
    if keys::is_prefix(&key, st.prefix) {
        st.mode = Mode::Prefix;
        return Action::None;
    }
    let typing = (st.view == View::Brain && st.brain.editing_query) || (st.view == View::Settings && st.settings.editing.is_some());
    if st.view != View::Board && !typing {
        if let Some(v) = tab_key(&key) {
            return st.switch_view(v);
        }
    }
    match st.view {
        View::Board => {}
        View::Overview => return overview_key(st, key),
        View::PrWatches => return pr_key(st, key),
        View::Brain => return brain_key(st, key),
        View::Settings => return settings_key(st, key),
    }
    if st.mode == Mode::Scroll {
        return scroll_key(st, key);
    }
    match st.focus {
        Focus::Sidebar => match key.code {
            KeyCode::Char('q') => Action::Quit,
            KeyCode::Esc => {
                st.zoom = false;
                Action::None
            }
            KeyCode::Enter => st.open_selected(),
            _ => command(st, key),
        },
        Focus::Pane => pane_key(st, key),
    }
}

fn pane_key(st: &mut TuiState, key: KeyEvent) -> Action {
    let Some(id) = st.selected_id() else {
        st.focus = Focus::Sidebar;
        return Action::None;
    };
    if let Some(pane) = st.pane_target(&id).filter(|p| st.pane_info(p).is_some_and(|i| i.alive)) {
        let Some(bytes) = keys::key_bytes(&key, &st.modes_of(&pane)) else { return Action::None };
        if st.scroll_of(&pane) > 0 {
            st.set_scroll(&pane, 0);
        }
        return Action::Write { pane, bytes };
    }
    if st.report_shown() {
        return report_key(st, key);
    }
    match st.backend_of(&id) {
        Backend::Ptyd { .. } => {
            if key.code == KeyCode::Esc {
                st.focus = Focus::Sidebar;
            } else {
                st.notify(Level::Info, format!("{id} has exited — Ctrl+] then x removes it"));
            }
            Action::None
        }
        Backend::Legacy => match key.code {
            KeyCode::Enter => st.open_selected(),
            KeyCode::Esc => {
                st.focus = Focus::Sidebar;
                Action::None
            }
            _ => {
                let why = st.viewer_errors.get(&id).cloned().unwrap_or_else(|| "starting the tmux view…".into());
                st.notify(Level::Info, format!("{id}: {why} — Enter retries; Ctrl+] then a attaches full-screen"));
                Action::None
            }
        },
    }
}

/// Keys on a focused report: nothing behind it takes input, so they are
/// ninox commands (bare), with j/k scrolling the report.
fn report_key(st: &mut TuiState, key: KeyEvent) -> Action {
    let max = st.layout.report_max_scroll;
    match key.code {
        KeyCode::Esc | KeyCode::Char('h') | KeyCode::Left => st.focus = Focus::Sidebar,
        KeyCode::Char('j') | KeyCode::Down => st.report_scroll = (st.report_scroll + 1).min(max),
        KeyCode::Char('k') | KeyCode::Up => st.report_scroll = st.report_scroll.saturating_sub(1),
        KeyCode::PageDown | KeyCode::Char(' ') => st.report_scroll = (st.report_scroll + st.page() as u16 / 2).min(max),
        KeyCode::PageUp => st.report_scroll = st.report_scroll.saturating_sub(st.page() as u16 / 2),
        KeyCode::Char('g') | KeyCode::Home => st.report_scroll = 0,
        KeyCode::Char('G') | KeyCode::End => st.report_scroll = max,
        KeyCode::Enter => {}
        _ if key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {}
        _ => return command(st, key),
    }
    Action::None
}

/// Prefix commands; also what bare keys mean while the sidebar has focus.
fn command(st: &mut TuiState, key: KeyEvent) -> Action {
    let overview = st.view == View::Overview;
    match key.code {
        KeyCode::Char('d') | KeyCode::Char('q') => return Action::Quit,
        KeyCode::Char('h') | KeyCode::Left if overview => move_tile(st, -1, 0),
        KeyCode::Char('l') | KeyCode::Right if overview => move_tile(st, 1, 0),
        KeyCode::Char('j') | KeyCode::Down if overview => move_tile(st, 0, 1),
        KeyCode::Char('k') | KeyCode::Up if overview => move_tile(st, 0, -1),
        KeyCode::Char('h') | KeyCode::Left => {
            st.view = View::Board;
            st.focus = Focus::Sidebar;
            st.zoom = false;
        }
        KeyCode::Char('l') | KeyCode::Right => {
            st.view = View::Board;
            if st.selected_row().is_some() {
                st.focus = Focus::Pane;
            }
        }
        KeyCode::Char('j') | KeyCode::Down => st.step_selection(1),
        KeyCode::Char('k') | KeyCode::Up => st.step_selection(-1),
        KeyCode::Char('!') => {
            if st.select_most_urgent() {
                st.view = View::Board;
                st.focus = Focus::Sidebar;
            } else {
                st.notify(Level::Info, "nothing needs you");
            }
        }
        KeyCode::Char(' ') => {
            if let Some(g) = st.selected_row().and_then(|r| r.group.clone()) {
                st.toggle_collapse(&g);
            }
        }
        KeyCode::Tab => {
            st.view = View::Board;
            st.focus = match st.focus {
                Focus::Sidebar if st.selected_row().is_some() => Focus::Pane,
                _ => Focus::Sidebar,
            };
        }
        KeyCode::Char('z') => {
            st.view = View::Board;
            st.zoom = !st.zoom;
            if st.zoom && st.selected_row().is_some() {
                st.focus = Focus::Pane;
            }
        }
        KeyCode::Char('g') => st.modal = Some(Modal::Goto(Goto::default())),
        KeyCode::Char('o') => {
            if overview {
                st.view = View::Board;
            } else {
                st.view = View::Overview;
                let rows = st.overview_rows();
                st.overview_sel = rows.iter().position(|&i| i == st.selected).unwrap_or(0);
            }
        }
        KeyCode::Char('n') => st.modal = Some(Modal::Spawn(SpawnModal::default())),
        KeyCode::Char('x') => st.ask_remove_or_kill(),
        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if st.live_count() == 0 {
                st.notify(Level::Info, "no live agents to restart");
            } else {
                st.modal = Some(Modal::Confirm(Pending::RestartAll));
            }
        }
        KeyCode::Char('r') => return st.resume_selected(),
        KeyCode::Char('O') => return st.open_selected_pr(),
        KeyCode::Char('R') => match st.selected_row().and_then(|r| r.group.clone()) {
            Some(orch) => st.modal = Some(Modal::Confirm(Pending::Reap(orch))),
            None => st.notify(Level::Info, "no orchestrator owns this row — nothing to reap"),
        },
        KeyCode::Char('?') => st.modal = Some(Modal::Help),
        KeyCode::Char('[') => match st.focused_pane() {
            Some(_) => {
                st.view = View::Board;
                st.focus = Focus::Pane;
                st.mode = Mode::Scroll;
                st.scroll_by_wheel = false;
            }
            None => st.notify(Level::Info, "scroll mode needs a live pane"),
        },
        KeyCode::Char('i') => {
            st.view = View::Board;
            st.inspector = !st.inspector;
        }
        KeyCode::Char('p') => return st.switch_view(View::PrWatches),
        KeyCode::Char('b') => {
            st.view = View::Brain;
            return Action::LoadBrain(st.brain.query.clone());
        }
        KeyCode::Char('s') => {
            st.view = View::Settings;
            return Action::LoadSettings;
        }
        KeyCode::Char('c') | KeyCode::Enter => {
            if overview {
                return open_tile(st);
            }
            return st.open_selected();
        }
        KeyCode::Char('a') => {
            if let Some(id) = st.selected_id() {
                return Action::Connect(id);
            }
        }
        KeyCode::Char('1'..='5') => {
            if let Some(v) = tab_key(&key) {
                return st.switch_view(v);
            }
        }
        _ => {}
    }
    Action::None
}

/// `h`/`l` just step through `overview_rows()` in order. `j`/`k` instead
/// walk the grid a group's header rows may have shifted out of the simple
/// `index ± cols` arithmetic (`super::layout::overview_grid`), landing on
/// the closest column in the row above/below.
fn move_tile(st: &mut TuiState, dx: i32, dy: i32) {
    let n = st.overview_rows().len();
    if n == 0 {
        return;
    }
    let cur = st.overview_sel.min(n - 1);
    if dy == 0 {
        st.overview_sel = (cur as i32 + dx).clamp(0, n as i32 - 1) as usize;
        return;
    }
    let cols = st.layout.overview_cols.max(1) as usize;
    let grid = super::layout::overview_grid(st, cols);
    let (row, col) = grid.coords[cur];
    let Some(target_row) = row.checked_add_signed(dy as i16) else { return };
    if let Some(best) =
        (0..n).filter(|&i| grid.coords[i].0 == target_row).min_by_key(|&i| (grid.coords[i].1 as i32 - col as i32).abs())
    {
        st.overview_sel = best;
    }
}

fn open_tile(st: &mut TuiState) -> Action {
    if let Some(&row) = st.overview_rows().get(st.overview_sel) {
        st.selected = row;
        st.view = View::Board;
        return st.open_selected();
    }
    Action::None
}

fn overview_key(st: &mut TuiState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc | KeyCode::Char('o') => {
            st.view = View::Board;
            Action::None
        }
        KeyCode::Enter => open_tile(st),
        _ => command(st, key),
    }
}

fn scroll_key(st: &mut TuiState, key: KeyEvent) -> Action {
    let Some(id) = st.focused_pane() else {
        st.mode = Mode::Normal;
        return Action::None;
    };
    let cur = st.scroll_of(&id);
    let half = st.page() / 2;
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let next = match key.code {
        KeyCode::Char('q') | KeyCode::Esc => {
            st.mode = Mode::Normal;
            0
        }
        KeyCode::Char('k') | KeyCode::Up => cur + 1,
        KeyCode::Char('j') | KeyCode::Down => cur.saturating_sub(1),
        KeyCode::Char('u') if ctrl => cur + half,
        KeyCode::Char('d') if ctrl => cur.saturating_sub(half),
        KeyCode::PageUp | KeyCode::Char('b') => cur + st.page(),
        KeyCode::PageDown | KeyCode::Char('f') | KeyCode::Char(' ') => cur.saturating_sub(st.page()),
        KeyCode::Char('g') | KeyCode::Home => usize::MAX / 4,
        KeyCode::Char('G') | KeyCode::End => 0,
        KeyCode::Char('y') => {
            let text = st.views.get(&id).map(|v| v.window_text(st.page() as u16)).unwrap_or_default();
            return Action::Yank(text);
        }
        _ => cur,
    };
    st.set_scroll(&id, next);
    Action::None
}

fn brain_key(st: &mut TuiState, key: KeyEvent) -> Action {
    if !st.brain.editing_query && key.code == KeyCode::Char('?') {
        st.modal = Some(Modal::Help);
        return Action::None;
    }
    let page = st.page() as i64;
    let b = &mut st.brain;
    if b.editing_query {
        // Filters live as you type; Enter keeps the filter and adds
        // semantic matches; Esc drops the search.
        let edited = match key.code {
            KeyCode::Esc => {
                b.editing_query = false;
                b.query.clear();
                true
            }
            KeyCode::Enter => {
                b.editing_query = false;
                return if b.query.trim().is_empty() { Action::None } else { Action::SearchBrain(b.query.clone()) };
            }
            KeyCode::Backspace => b.query.pop().is_some(),
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                b.query.clear();
                true
            }
            KeyCode::Char(c) => {
                b.query.push(c);
                true
            }
            KeyCode::Down => {
                b.editing_query = false;
                false
            }
            _ => false,
        };
        if edited {
            b.cursor = Default::default();
            b.last_index = 0;
            b.scroll = 0;
        }
        return Action::None;
    }
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') if b.open => b.open = false,
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('b') => st.view = View::Board,
        KeyCode::Char('/') => {
            b.editing_query = true;
            b.open = false;
        }
        KeyCode::Char('e') => return brain_edit(st),
        KeyCode::Char('a') => return brain_new(st),
        KeyCode::Char('D') => brain_delete(st),
        KeyCode::Char('t') => {
            b.cycle_grouping();
            b.open = false;
        }
        KeyCode::Char(' ') => b.toggle_fold(),
        KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => match b.selected_item() {
            Some(super::brain::Item::Group { folded, .. }) if folded || key.code == KeyCode::Enter => b.toggle_fold(),
            Some(super::brain::Item::Group { .. }) => {}
            Some(_) => {
                b.open = true;
                b.scroll = 0;
            }
            None => {}
        },
        KeyCode::Char('j') | KeyCode::Down if b.open => b.scroll = b.scroll.saturating_add(1),
        KeyCode::Char('k') | KeyCode::Up if b.open => b.scroll = b.scroll.saturating_sub(1),
        KeyCode::PageDown if b.open => b.scroll = b.scroll.saturating_add(10),
        KeyCode::PageUp if b.open => b.scroll = b.scroll.saturating_sub(10),
        KeyCode::Char('h') if b.open => b.open = false,
        // From an entry, back up to its group's header.
        KeyCode::Char('h') | KeyCode::Left => {
            if let Some(super::brain::Item::Entry { group: Some(g), .. }) = b.selected_item() {
                b.cursor = super::brain::Sel { group: Some(g), id: None };
            }
        }
        KeyCode::Char('j') | KeyCode::Down => b.move_cursor(1),
        KeyCode::Char('k') | KeyCode::Up => b.move_cursor(-1),
        KeyCode::PageDown => b.move_cursor(page),
        KeyCode::PageUp => b.move_cursor(-page),
        KeyCode::Char('g') | KeyCode::Home => b.move_cursor(i64::MIN / 2),
        KeyCode::Char('G') | KeyCode::End => b.move_cursor(i64::MAX / 2),
        _ => {}
    }
    Action::None
}

fn open_pr(st: &mut TuiState) -> Action {
    match st.prs.selected_row().map(|r| (r.url.clone(), r.number)) {
        Some((Some(url), _)) => Action::OpenUrl(url),
        Some((None, n)) => {
            st.notify(Level::Warn, format!("#{n} has no URL to open"));
            Action::None
        }
        None => Action::None,
    }
}

/// Jump to the selected PR's session on the fleet tab.
fn show_pr_session(st: &mut TuiState) -> Action {
    let Some(id) = st.prs.selected_row().and_then(|r| r.session.clone()) else {
        st.notify(Level::Info, "this PR is watched from outside any session".to_string());
        return Action::None;
    };
    match st.rows.iter().position(|r| r.id() == id) {
        Some(i) => {
            st.view = View::Board;
            st.focus = Focus::Sidebar;
            st.select(i);
        }
        None => st.notify(Level::Warn, format!("session {id} is gone")),
    }
    Action::None
}

fn pr_key(st: &mut TuiState, key: KeyEvent) -> Action {
    let page = st.page() as i64;
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('p') => st.view = View::Board,
        KeyCode::Char('?') => st.modal = Some(Modal::Help),
        KeyCode::Char('j') | KeyCode::Down => st.prs.move_cursor(1),
        KeyCode::Char('k') | KeyCode::Up => st.prs.move_cursor(-1),
        KeyCode::PageDown => st.prs.move_cursor(page),
        KeyCode::PageUp => st.prs.move_cursor(-page),
        KeyCode::Char('g') | KeyCode::Home => st.prs.move_cursor(i64::MIN / 2),
        KeyCode::Char('G') | KeyCode::End => st.prs.move_cursor(i64::MAX / 2),
        KeyCode::Enter | KeyCode::Char('o') | KeyCode::Char('O') => return open_pr(st),
        KeyCode::Char('s') => return show_pr_session(st),
        _ => {}
    }
    Action::None
}

fn pr_mouse(st: &mut TuiState, ev: &MouseEvent, lay: &Layout, wheel: Option<i64>) -> Action {
    let Some(pl) = lay.prs.clone() else { return Action::None };
    let (x, y) = (ev.column, ev.row);
    if let Some(d) = wheel {
        if contains(pl.list, x, y) {
            st.prs.move_cursor(-d.signum());
        }
        return Action::None;
    }
    if !matches!(ev.kind, MouseEventKind::Down(MouseButton::Left)) || !contains(pl.list, x, y) {
        return Action::None;
    }
    let Some(super::prs::Line::Pr(i)) = pl.lines.get(pl.offset + (y - pl.list.y) as usize).cloned() else { return Action::None };
    st.prs.select(i);
    let hit = |rects: &[(usize, Rect)]| rects.iter().any(|(r, rect)| *r == i && contains(*rect, x, y));
    if hit(&pl.numbers) || hit(&pl.opens) {
        return open_pr(st);
    }
    if hit(&pl.sessions) {
        return show_pr_session(st);
    }
    Action::None
}

fn brain_edit(st: &mut TuiState) -> Action {
    match st.brain.selected_entry() {
        Some(e) => Action::EditBrain(e.id.clone()),
        None => {
            st.notify(Level::Info, "select an entry to edit (a adds a new one)".to_string());
            Action::None
        }
    }
}

fn brain_new(st: &mut TuiState) -> Action {
    let (entry_type, tags) = st.brain.template_defaults();
    Action::NewBrain { entry_type, tags }
}

fn brain_delete(st: &mut TuiState) {
    if let Some(id) = st.brain.selected_entry().map(|e| e.id.clone()) {
        st.modal = Some(Modal::Confirm(Pending::DeleteBrain(id)));
    }
}

/// Activate the selected setting: toggle or step a choice, open the inline
/// editor on a text field, or run the action.
pub fn activate_setting(st: &mut TuiState) -> Action {
    let Some(f) = st.settings.selected_field().cloned() else { return Action::None };
    st.settings.error = None;
    match f.kind {
        Kind::Toggle | Kind::Choice => Action::SaveSetting(Change::Step { id: f.id, back: false }),
        Kind::Text => {
            st.settings.editing = Some(f.value);
            Action::None
        }
        Kind::Action => Action::EditConfig,
    }
}

fn step_setting(st: &mut TuiState, back: bool) -> Action {
    let Some(id) = st.settings.selected_field().filter(|f| !f.options.is_empty()).map(|f| f.id.clone()) else {
        return Action::None;
    };
    st.settings.error = None;
    Action::SaveSetting(Change::Step { id, back })
}

fn move_setting(st: &mut TuiState, delta: i64) {
    let last = st.settings.fields.len().saturating_sub(1) as i64;
    st.settings.selected = (st.settings.selected as i64 + delta).clamp(0, last) as usize;
    st.settings.error = None;
}

fn settings_key(st: &mut TuiState, key: KeyEvent) -> Action {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if let Some(buf) = st.settings.editing.as_mut() {
        match key.code {
            KeyCode::Esc => {
                st.settings.editing = None;
                st.settings.error = None;
            }
            KeyCode::Enter => {
                let text = buf.clone();
                let Some(f) = st.settings.selected_field() else { return Action::None };
                return Action::SaveSetting(Change::Set { id: f.id.clone(), text });
            }
            KeyCode::Backspace => {
                buf.pop();
            }
            KeyCode::Char('u') if ctrl => buf.clear(),
            KeyCode::Char(c) if !ctrl => buf.push(c),
            _ => {}
        }
        return Action::None;
    }
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('s') => st.view = View::Board,
        KeyCode::Char('j') | KeyCode::Down => move_setting(st, 1),
        KeyCode::Char('k') | KeyCode::Up => move_setting(st, -1),
        KeyCode::PageDown => move_setting(st, 10),
        KeyCode::PageUp => move_setting(st, -10),
        KeyCode::Char('g') | KeyCode::Home => move_setting(st, i64::MIN / 2),
        KeyCode::Char('G') | KeyCode::End => move_setting(st, i64::MAX / 2),
        KeyCode::Enter | KeyCode::Char(' ') => return activate_setting(st),
        KeyCode::Char('l') | KeyCode::Right => return step_setting(st, false),
        KeyCode::Char('h') | KeyCode::Left => return step_setting(st, true),
        KeyCode::Char('e') => return Action::EditConfig,
        KeyCode::Char('?') => st.modal = Some(Modal::Help),
        _ => {}
    }
    Action::None
}

fn confirmed(pending: Pending) -> Action {
    match pending {
        Pending::Kill(id) => Action::Kill(id),
        Pending::Remove(id) => Action::Remove(id),
        Pending::Reap(id) => Action::Reap(id),
        Pending::DeleteBrain(id) => Action::DeleteBrain(id),
        Pending::RestartAll => Action::RestartAll,
    }
}

fn handle_modal_key(st: &mut TuiState, key: KeyEvent) -> Action {
    let Some(modal) = st.modal.take() else { return Action::None };
    match modal {
        Modal::Help => Action::None,
        Modal::Confirm(pending) => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => confirmed(pending),
            KeyCode::Char('a') if st.confirm_offers_all(&pending) => match pending {
                Pending::Remove(id) => Action::RemoveAll(id),
                _ => Action::None,
            },
            KeyCode::Char('n') | KeyCode::Esc => Action::None,
            _ => {
                st.modal = Some(Modal::Confirm(pending));
                Action::None
            }
        },
        Modal::Restore(summary) => match key.code {
            KeyCode::Char('y') | KeyCode::Enter => Action::Restore,
            KeyCode::Char('n') | KeyCode::Esc => Action::DismissRestore,
            _ => {
                st.modal = Some(Modal::Restore(summary));
                Action::None
            }
        },
        Modal::Spawn(mut m) => {
            let action = spawn_modal_key(&mut m, key);
            if action.is_none_and_open() {
                st.modal = Some(Modal::Spawn(m));
            }
            action.into_action()
        }
        Modal::Goto(mut g) => {
            let matches = st.goto_matches(&g);
            match key.code {
                KeyCode::Esc => return Action::None,
                KeyCode::Enter => {
                    if let Some(&row) = matches.get(g.selected) {
                        st.selected = row;
                        return st.open_selected();
                    }
                }
                KeyCode::Tab => {
                    g.filter = g.filter.cycle(false);
                    g.selected = 0;
                }
                KeyCode::BackTab => {
                    g.filter = g.filter.cycle(true);
                    g.selected = 0;
                }
                KeyCode::Down => g.selected = (g.selected + 1).min(matches.len().saturating_sub(1)),
                KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    g.selected = (g.selected + 1).min(matches.len().saturating_sub(1))
                }
                KeyCode::Up => g.selected = g.selected.saturating_sub(1),
                KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    g.selected = g.selected.saturating_sub(1)
                }
                KeyCode::Backspace => {
                    g.query.pop();
                    g.selected = 0;
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    g.query.push(c);
                    g.selected = 0;
                }
                _ => {}
            }
            st.modal = Some(Modal::Goto(g));
            Action::None
        }
    }
}

enum SpawnOutcome {
    Open,
    Cancel,
    Submit(Action),
}

impl SpawnOutcome {
    fn is_none_and_open(&self) -> bool {
        matches!(self, Self::Open)
    }
    fn into_action(self) -> Action {
        match self {
            Self::Submit(a) => a,
            Self::Open | Self::Cancel => Action::None,
        }
    }
}

fn spawn_modal_key(modal: &mut SpawnModal, key: KeyEvent) -> SpawnOutcome {
    match key.code {
        KeyCode::Esc => SpawnOutcome::Cancel,
        KeyCode::Enter | KeyCode::Tab if !modal.on_prompt_field => {
            modal.on_prompt_field = true;
            SpawnOutcome::Open
        }
        KeyCode::BackTab | KeyCode::Tab => {
            modal.on_prompt_field = !modal.on_prompt_field;
            SpawnOutcome::Open
        }
        KeyCode::Enter => {
            if modal.name.trim().is_empty() {
                return SpawnOutcome::Open;
            }
            let m = std::mem::take(modal);
            SpawnOutcome::Submit(Action::Spawn {
                name: m.name,
                prompt: Some(m.prompt).filter(|p| !p.trim().is_empty()),
            })
        }
        KeyCode::Backspace => {
            if modal.on_prompt_field {
                modal.prompt.pop();
            } else {
                modal.name.pop();
            }
            SpawnOutcome::Open
        }
        KeyCode::Char(c) => {
            if modal.on_prompt_field {
                modal.prompt.push(c);
            } else {
                modal.name.push(c);
            }
            SpawnOutcome::Open
        }
        _ => SpawnOutcome::Open,
    }
}

fn contains(r: Rect, x: u16, y: u16) -> bool {
    x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
}

/// Mouse routing, herdr-style: the header (tabs), footer, sidebar and the
/// pane's `◀ fleet` button always belong to ninox, whatever the agent asked
/// for; only events inside the pane's inner area can reach the agent, in
/// pane-local coordinates.
pub fn handle_mouse(st: &mut TuiState, ev: MouseEvent) -> Action {
    let (x, y) = (ev.column, ev.row);
    if st.modal.is_some() {
        let left = matches!(ev.kind, MouseEventKind::Down(MouseButton::Left));
        if matches!(ev.kind, MouseEventKind::Down(_)) && st.modal == Some(Modal::Help) {
            st.modal = None;
        }
        if let (true, Some(all), Some(Modal::Confirm(Pending::Remove(id)))) = (left, st.layout.modal_all, &st.modal) {
            if contains(all, x, y) {
                let id = id.clone();
                st.modal = None;
                return Action::RemoveAll(id);
            }
        }
        if let (true, Some((yes, no))) = (left, st.layout.modal_buttons) {
            if contains(yes, x, y) {
                if let Some(Modal::Confirm(pending)) = st.modal.take() {
                    return confirmed(pending);
                }
            } else if contains(no, x, y) {
                st.modal = None;
            }
        }
        return Action::None;
    }
    let lay = st.layout.clone();
    if let Some(action) = continue_gesture(st, &ev, &lay) {
        return action;
    }
    let left_down = matches!(ev.kind, MouseEventKind::Down(MouseButton::Left));
    let double = left_down && register_click(st, x, y);
    if left_down {
        st.selection = None;
    }
    if contains(lay.header, x, y) {
        if left_down {
            if let Some(t) = lay.tabs.iter().find(|t| contains(t.rect, x, y)) {
                return st.switch_view(t.view);
            }
        }
        return Action::None;
    }
    if contains(lay.footer, x, y) {
        if left_down && st.view == View::Board && st.focus == Focus::Pane {
            st.leave_pane();
        }
        return Action::None;
    }
    let wheel = match ev.kind {
        MouseEventKind::ScrollUp => Some(3i64),
        MouseEventKind::ScrollDown => Some(-3i64),
        _ => None,
    };
    match st.view {
        View::Overview => {
            if left_down {
                if let Some(pos) = lay.tiles.iter().position(|t| contains(t.outer, x, y)) {
                    st.overview_sel = lay.tiles[pos].index;
                    return open_tile(st);
                }
            }
            return Action::None;
        }
        View::Brain => return brain_mouse(st, &ev, &lay, wheel),
        View::Settings => return settings_mouse(st, &ev, &lay, wheel),
        View::PrWatches => return pr_mouse(st, &ev, &lay, wheel),
        View::Board => {}
    }
    if lay.sidebar.or(lay.sidebar_list).is_some_and(|r| contains(r, x, y)) {
        if left_down && lay.sidebar_close.is_some_and(|r| contains(r, x, y)) {
            st.focus = Focus::Sidebar;
            st.ask_remove_or_kill();
            return Action::None;
        }
        match (ev.kind, wheel) {
            (MouseEventKind::Down(MouseButton::Right), _) => {
                let hit = match (lay.sidebar_needs, lay.sidebar_list) {
                    (Some(n), _) if contains(n, x, y) => lay.needs_items.get((y - n.y) as usize),
                    (_, Some(l)) if contains(l, x, y) => lay.sidebar_items.get(lay.sidebar_offset + (y - l.y) as usize),
                    _ => None,
                };
                if let Some(SideItem::Session { row, pinned }) = hit {
                    let (row, pinned) = (*row, *pinned);
                    st.focus = Focus::Sidebar;
                    st.select(row);
                    st.selected_pinned = pinned;
                    st.ask_remove_or_kill();
                }
            }
            (MouseEventKind::Down(MouseButton::Left), _) => {
                st.focus = Focus::Sidebar;
                let hit = match (lay.sidebar_needs, lay.sidebar_list) {
                    (Some(n), _) if contains(n, x, y) => lay.needs_items.get((y - n.y) as usize).map(|it| (it, n)),
                    (_, Some(l)) if contains(l, x, y) => {
                        lay.sidebar_items.get(lay.sidebar_offset + (y - l.y) as usize).map(|it| (it, l))
                    }
                    _ => None,
                };
                match hit {
                    Some((SideItem::Session { row, pinned }, area)) => {
                        let (row, pinned) = (*row, *pinned);
                        let Some(r) = st.rows.get(row) else { return Action::None };
                        // The chevron cell of a group header folds it.
                        if r.is_orchestrator && x < area.x + super::layout::CHEVRON_W {
                            let g = r.id().to_string();
                            st.toggle_collapse(&g);
                            return Action::None;
                        }
                        let same = row == st.selected;
                        st.select(row);
                        st.selected_pinned = pinned;
                        if double && same {
                            return st.open_selected();
                        }
                    }
                    Some((SideItem::Group(g), _)) => {
                        let g = g.clone();
                        st.toggle_collapse(&g);
                    }
                    _ => {}
                }
            }
            (_, Some(d)) => st.step_selection(-d.signum()),
            _ => {}
        }
        return Action::None;
    }
    if left_down && lay.pane_back.is_some_and(|r| contains(r, x, y)) {
        st.leave_pane();
        return Action::None;
    }
    if left_down && lay.pane_kill.is_some_and(|r| contains(r, x, y)) {
        if let Some(id) = st.selected_id() {
            st.modal = Some(Modal::Confirm(Pending::Kill(id)));
        }
        return Action::None;
    }
    if left_down && lay.pane_zoom.is_some_and(|r| contains(r, x, y)) {
        st.zoom = !st.zoom;
        if st.zoom {
            st.focus = Focus::Pane;
        }
        return Action::None;
    }
    if let Some(body) = lay.report_body.filter(|_| st.report_shown()) {
        let in_pane = lay.pane.is_some_and(|o| contains(o, x, y));
        if let Some(d) = wheel.filter(|_| in_pane) {
            st.report_scroll = (st.report_scroll as i64 - d).clamp(0, lay.report_max_scroll as i64) as u16;
            return Action::None;
        }
        if left_down && in_pane {
            st.focus = Focus::Pane;
            if let Some((b, _)) = lay.report_buttons.iter().find(|(_, r)| contains(*r, x, y)) {
                return st.report_button(*b);
            }
            if lay.report_links.iter().any(|r| contains(*r, x, y)) && contains(body, x, y) {
                return st.open_selected_pr();
            }
        }
        return Action::None;
    }
    let Some(inner) = lay.pane_inner.filter(|r| contains(*r, x, y)) else {
        if left_down && lay.pane.is_some_and(|o| contains(o, x, y)) && st.selected_row().is_some() {
            st.focus = Focus::Pane;
        }
        return Action::None;
    };
    let Some(id) = st.focused_pane() else {
        if left_down && st.selected_row().is_some() {
            return st.open_selected();
        }
        return Action::None;
    };
    let (col, row) = (x - inner.x, y - inner.y);
    let modes = st.modes_of(&id);
    let scrolled = st.scroll_of(&id) > 0;
    if matches!(ev.kind, MouseEventKind::Down(_)) {
        st.focus = Focus::Pane;
    }
    if let Some(d) = wheel {
        if modes.mouse_reporting && !scrolled {
            return keys::mouse_bytes(&ev, col, row, &modes).map(|bytes| Action::Write { pane: id, bytes }).unwrap_or(Action::None);
        }
        let next = (st.scroll_of(&id) as i64 + d).max(0) as usize;
        st.set_scroll(&id, next);
        if next > 0 && st.mode != Mode::Scroll {
            st.mode = Mode::Scroll;
            st.scroll_by_wheel = true;
        } else if next == 0 && st.mode == Mode::Scroll && st.scroll_by_wheel {
            st.mode = Mode::Normal;
        }
        return Action::None;
    }
    // Shift opts out of the agent's mouse capture to select instead
    // (terminals that keep Shift+drag for their own selection never send it).
    let captured = modes.mouse_reporting && !scrolled && !ev.modifiers.contains(KeyModifiers::SHIFT);
    if captured {
        let Some(bytes) = keys::mouse_bytes(&ev, col, row, &modes) else { return Action::None };
        if matches!(ev.kind, MouseEventKind::Down(_)) {
            st.mouse_grab = Some(id.clone());
        }
        return Action::Write { pane: id, bytes };
    }
    if left_down {
        if double {
            let word = st.views.get(&id).and_then(|v| v.word_at(inner.height, row, col));
            if let Some((c0, c1)) = word {
                let text = st.views.get(&id).map(|v| v.selection_text(inner.height, (row, c0), (row, c1))).unwrap_or_default();
                st.selection = Some(Selection { pane: id, anchor: (row, c0), cursor: (row, c1), done: true });
                return Action::Yank(text);
            }
        }
        st.selection = Some(Selection { pane: id, anchor: (row, col), cursor: (row, col), done: false });
    }
    Action::None
}

/// Drags and releases of a gesture in progress: a press forwarded to the
/// agent, or a selection being dragged. Positions are clamped to the pane.
fn continue_gesture(st: &mut TuiState, ev: &MouseEvent, lay: &Layout) -> Option<Action> {
    if !matches!(ev.kind, MouseEventKind::Drag(_) | MouseEventKind::Up(_)) {
        return None;
    }
    let clamp = |inner: Rect| {
        (
            ev.row.saturating_sub(inner.y).min(inner.height.saturating_sub(1)),
            ev.column.saturating_sub(inner.x).min(inner.width.saturating_sub(1)),
        )
    };
    if let Some(pane) = st.mouse_grab.clone() {
        if matches!(ev.kind, MouseEventKind::Up(_)) {
            st.mouse_grab = None;
        }
        let Some(inner) = lay.pane_inner else { return Some(Action::None) };
        let (row, col) = clamp(inner);
        let bytes = keys::mouse_bytes(ev, col, row, &st.modes_of(&pane));
        return Some(bytes.map(|bytes| Action::Write { pane, bytes }).unwrap_or(Action::None));
    }
    let inner = lay.pane_inner?;
    let sel = st.selection.as_mut().filter(|s| !s.done)?;
    let cell = clamp(inner);
    match ev.kind {
        MouseEventKind::Drag(MouseButton::Left) => {
            sel.cursor = cell;
            Some(Action::None)
        }
        MouseEventKind::Up(MouseButton::Left) => {
            sel.cursor = cell;
            if !sel.dragged() {
                st.selection = None;
                return Some(Action::None);
            }
            sel.done = true;
            let (pane, a, b) = (sel.pane.clone(), sel.anchor, sel.cursor);
            let text = st.views.get(&pane).map(|v| v.selection_text(inner.height, a, b)).unwrap_or_default();
            Some(if text.is_empty() { Action::None } else { Action::Yank(text) })
        }
        _ => None,
    }
}

/// Records a left press; `true` when it completes a double-click.
fn register_click(st: &mut TuiState, x: u16, y: u16) -> bool {
    let now = st.now_ms;
    let double = st.last_click.is_some_and(|c| c.x == x && c.y == y && now - c.ms <= DOUBLE_CLICK_MS);
    st.last_click = if double { None } else { Some(Click { x, y, ms: now }) };
    double
}

/// Settings rows: a click on the value changes it (as Enter does), a click
/// on the label selects, and a second click on the selected row changes it.
fn settings_mouse(st: &mut TuiState, ev: &MouseEvent, lay: &Layout, wheel: Option<i64>) -> Action {
    let Some(sl) = lay.settings.clone() else { return Action::None };
    let (x, y) = (ev.column, ev.row);
    if let Some(d) = wheel {
        if st.settings.editing.is_none() {
            move_setting(st, -d.signum());
        }
        return Action::None;
    }
    if !matches!(ev.kind, MouseEventKind::Down(MouseButton::Left)) {
        return Action::None;
    }
    if contains(sl.open, x, y) {
        return Action::EditConfig;
    }
    if !contains(sl.list, x, y) {
        return Action::None;
    }
    let Some(super::layout::SettingsLine::Field(i)) = sl.lines.get(sl.offset + (y - sl.list.y) as usize).copied() else {
        return Action::None;
    };
    let again = i == st.settings.selected;
    if st.settings.editing.is_some() {
        if again {
            return Action::None;
        }
        st.settings.editing = None;
    }
    st.settings.selected = i;
    st.settings.error = None;
    if again || x >= sl.value_x {
        return activate_setting(st);
    }
    Action::None
}

fn brain_mouse(st: &mut TuiState, ev: &MouseEvent, lay: &Layout, wheel: Option<i64>) -> Action {
    use super::layout::BrainButton;
    let Some(bl) = lay.brain.clone() else { return Action::None };
    let (x, y) = (ev.column, ev.row);
    let left = matches!(ev.kind, MouseEventKind::Down(MouseButton::Left));
    if left {
        if let Some((button, _)) = bl.buttons.iter().find(|(_, r)| contains(*r, x, y)) {
            st.brain.editing_query = false;
            return match button {
                BrainButton::New => brain_new(st),
                BrainButton::Edit => brain_edit(st),
                BrainButton::Delete => {
                    brain_delete(st);
                    Action::None
                }
            };
        }
    }
    let b = &mut st.brain;
    match (ev.kind, wheel) {
        (MouseEventKind::Down(MouseButton::Left), _) if contains(bl.search, x, y) => {
            b.editing_query = true;
            b.open = false;
        }
        (MouseEventKind::Down(MouseButton::Left), _) if contains(bl.list, x, y) => {
            let i = bl.list_offset + (y - bl.list.y) as usize;
            if let Some(item) = bl.items.get(i) {
                b.set_cursor(&bl.items, i);
                b.open = false;
                b.editing_query = false;
                if matches!(item, super::brain::Item::Group { .. }) && x < bl.list.x + super::layout::CHEVRON_W {
                    b.toggle_fold();
                }
            }
        }
        (MouseEventKind::Down(MouseButton::Left), _) if contains(bl.doc, x, y) && b.selected_entry().is_some() => {
            b.open = true;
            b.editing_query = false;
        }
        (_, Some(d)) if contains(bl.doc, x, y) => b.scroll = (b.scroll as i64 - d).clamp(0, u16::MAX as i64) as u16,
        (_, Some(d)) if contains(bl.list, x, y) => b.move_cursor(-d.signum()),
        _ => {}
    }
    Action::None
}

/// Paste goes to the focused live pane, bracketed when the app asked.
pub fn handle_paste(st: &mut TuiState, text: &str) -> Action {
    if let Some(Modal::Spawn(m)) = st.modal.as_mut() {
        let field = if m.on_prompt_field { &mut m.prompt } else { &mut m.name };
        field.push_str(text);
        return Action::None;
    }
    if st.modal.is_some() || st.view != View::Board || st.focus != Focus::Pane {
        return Action::None;
    }
    match st.focused_pane() {
        Some(id) if st.pane_info(&id).is_some_and(|p| p.alive) => {
            let bytes = keys::paste_bytes(text, &st.modes_of(&id));
            Action::Write { pane: id, bytes }
        }
        _ => Action::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::settings::FieldId;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn code(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }
    /// The platform's default prefix chord (`TuiState::default().prefix`).
    fn prefix() -> KeyEvent {
        let c = if ninox_core::config::DEFAULT_PREFIX_BYTE == 0x1c { '\\' } else { ' ' };
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }
    fn state_with_rows(n: usize) -> TuiState {
        TuiState { rows: (0..n).map(|i| Row::test_row(&format!("s{i}"))).collect(), ..Default::default() }
    }
    fn pane_info(id: &str, alive: bool) -> PaneInfo {
        PaneInfo {
            pane: id.into(),
            pid: 1,
            cols: 80,
            rows: 24,
            alive,
            exit_code: None,
            created_ms: 0,
            last_output_ms: 0,
            title: None,
            cwd: "/".into(),
            seq: 0, history_size: 0,
        }
    }
    fn with_live_pane(st: &mut TuiState, id: &str) {
        st.panes.insert(id.into(), pane_info(id, true));
    }

    #[test]
    fn j_and_k_move_selection_within_bounds() {
        let mut st = state_with_rows(3);
        handle_key(&mut st, key('j'));
        assert_eq!(st.selected, 1);
        handle_key(&mut st, key('k'));
        handle_key(&mut st, key('k'));
        assert_eq!(st.selected, 0);
        handle_key(&mut st, key('j'));
        handle_key(&mut st, key('j'));
        handle_key(&mut st, key('j'));
        assert_eq!(st.selected, 2);
    }

    /// `o` with workers `w0`..`w{n}`, then standalone `solo`.
    fn grouped(n: usize) -> TuiState {
        let mut rows = vec![Row { is_orchestrator: true, group: Some("o".into()), ..Row::test_row("o") }];
        rows.extend((0..n).map(|i| Row { group: Some("o".into()), ..Row::test_row(&format!("w{i}")) }));
        rows.push(Row::test_row("solo"));
        TuiState { rows, ..Default::default() }
    }

    #[test]
    fn space_folds_a_group_and_j_k_skip_its_hidden_workers() {
        let mut st = grouped(2);
        handle_key(&mut st, key('j'));
        assert_eq!(st.selected_id().as_deref(), Some("w0"));
        handle_key(&mut st, key(' '));
        assert!(st.collapsed.contains("o"));
        assert_eq!(st.selected_id().as_deref(), Some("o"), "selection climbs out of the folded group");
        handle_key(&mut st, key('j'));
        assert_eq!(st.selected_id().as_deref(), Some("solo"));
        assert!(!st.sidebar_items().contains(&SideItem::Session { row: 1, pinned: false }));
        handle_key(&mut st, key('k'));
        handle_key(&mut st, key(' '));
        assert!(st.collapsed.is_empty());
    }

    #[test]
    fn opening_a_folded_worker_unfolds_its_group() {
        let mut st = grouped(2);
        st.collapsed.insert("o".into());
        st.modal = Some(Modal::Goto(Goto { query: "w1".into(), ..Default::default() }));
        handle_key(&mut st, code(KeyCode::Enter));
        assert_eq!(st.selected_id().as_deref(), Some("w1"));
        assert!(st.collapsed.is_empty());
    }

    #[test]
    fn removing_an_orchestrator_says_it_takes_its_workers() {
        let mut st = grouped(2);
        for r in &mut st.rows {
            r.session.status = SessionStatus::Terminated;
        }
        let (q, what) = st.confirm_text(&Pending::Remove("o".into()));
        assert!(q.ends_with("and its 2 ended workers?"), "{q}");
        assert!(what.contains("may have uncommitted work"), "unchecked worktrees are not called clean: {what}");
        assert!(!st.confirm_offers_all(&Pending::Remove("o".into())), "nothing live to opt into stopping");
        st.uncommitted.insert("w0".into(), 3);
        st.uncommitted.insert("w1".into(), 0);
        let (_, what) = st.confirm_text(&Pending::Remove("o".into()));
        assert!(what.contains("1 ended with uncommitted changes"), "{what}");
        let (q, _) = st.confirm_text(&Pending::Remove("w0".into()));
        assert!(!q.contains("workers"), "{q}");
    }

    #[test]
    fn removing_an_orchestrator_keeps_live_workers_unless_asked() {
        let mut st = grouped(2);
        st.rows[0].session.status = SessionStatus::Terminated;
        st.rows[1].session.status = SessionStatus::Terminated;
        let pending = Pending::Remove("o".into());
        let (q, what) = st.confirm_text(&pending);
        assert!(q.ends_with("and its 1 ended worker?"), "{q}");
        assert!(what.contains("1 live or resumable worker kept"), "{what}");
        assert!(what.contains("live may have uncommitted work"), "{what}");

        st.modal = Some(Modal::Confirm(pending.clone()));
        assert_eq!(handle_key(&mut st, key('y')), Action::Remove("o".into()), "y keeps the live worker");
        st.modal = Some(Modal::Confirm(pending.clone()));
        assert_eq!(handle_key(&mut st, key('a')), Action::RemoveAll("o".into()));

        st.modal = Some(Modal::Confirm(pending));
        st.layout = crate::tui::layout::compute(Rect::new(0, 0, 160, 40), &st);
        let all = st.layout.modal_all.expect("an `a` button when workers are live");
        let (_, no) = st.layout.modal_buttons.unwrap();
        assert!(all.x > no.right());
        assert_eq!(handle_mouse(&mut st, down(all.x + 1, all.y)), Action::RemoveAll("o".into()));
    }

    fn needs_state() -> TuiState {
        let mut st = grouped(3);
        st.rows[1].session.status = SessionStatus::Mergeable;
        st.rows[2].session.activity = ActivityState::Blocked;
        st.rows[4].session.status = SessionStatus::Interrupted;
        st
    }

    #[test]
    fn keyboard_walks_the_needs_you_block_then_the_tree() {
        let mut st = needs_state();
        let pinned = st.needs_rows();
        assert!(!pinned.is_empty());
        st.focus = Focus::Sidebar;
        let first_tree = st.nav_rows()[0];
        st.select(first_tree);
        handle_key(&mut st, code(KeyCode::Up));
        assert!(st.cursor_pinned(), "k from the top of the tree enters Needs you");
        assert_eq!(st.selected, *pinned.last().unwrap());
        for _ in 0..pinned.len() {
            handle_key(&mut st, code(KeyCode::Up));
        }
        assert_eq!((st.selected, st.cursor_pinned()), (pinned[0], true), "clamps at the most urgent row");
        for _ in 0..pinned.len() {
            handle_key(&mut st, code(KeyCode::Down));
        }
        assert!(!st.cursor_pinned(), "j past the block lands in the tree");
        assert_eq!(st.selected, first_tree);
    }

    #[test]
    fn bang_jumps_to_the_most_urgent_session_even_from_a_pane() {
        let mut st = needs_state();
        let urgent = st.needs_rows()[0];
        st.select(st.nav_rows()[0]);
        st.focus = Focus::Pane;
        st.mode = Mode::Prefix;
        handle_key(&mut st, key('!'));
        assert_eq!((st.selected, st.cursor_pinned(), st.focus), (urgent, true, Focus::Sidebar));
    }

    #[test]
    fn needs_you_ranks_blocked_first_and_skips_ended_sessions() {
        let mut st = grouped(3);
        st.rows[1].session.status = SessionStatus::Mergeable;
        st.rows[2].session.activity = ActivityState::Blocked;
        st.rows[3].session.status = SessionStatus::Terminated;
        st.rows[3].session.activity = ActivityState::Blocked;
        st.rows[4].session.status = SessionStatus::Interrupted;
        assert_eq!(st.needs_rows(), vec![2, 4, 1]);
        assert_eq!(st.needs_you(&st.rows[4]), Some(Need::Interrupted));
    }

    #[test]
    fn clicking_a_group_chevron_folds_it_and_a_pinned_row_selects_its_session() {
        let mut st = grouped(2);
        st.rows[2].session.activity = ActivityState::Blocked;
        st.layout = crate::tui::layout::compute(Rect::new(0, 0, 100, 30), &st);
        let needs = st.layout.sidebar_needs.unwrap();
        handle_mouse(&mut st, down(needs.x + 4, needs.y + 1));
        assert_eq!(st.selected_id().as_deref(), Some("w1"), "the pinned copy selects the real row");
        let list = st.layout.sidebar_list.unwrap();
        let header = st.layout.sidebar_items.iter().position(|i| *i == SideItem::Session { row: 0, pinned: false }).unwrap();
        handle_mouse(&mut st, down(list.x, list.y + header as u16));
        assert!(st.collapsed.contains("o"));
        assert_eq!(st.selected_id().as_deref(), Some("o"), "a folded-away selection climbs to the header");
    }

    fn viewer_info(session: &str, alive: bool) -> PaneInfo {
        pane_info(&ninox_core::runtime::viewer_pane_id(std::process::id(), session), alive)
    }

    #[test]
    fn enter_on_a_tmux_row_opens_it_in_the_pane_and_wants_a_viewer() {
        let mut st = state_with_rows(2);
        st.selected = 1;
        assert_eq!(handle_key(&mut st, code(KeyCode::Enter)), Action::None, "no full-screen attach");
        assert_eq!(st.focus, Focus::Pane);
        assert_eq!(st.wanted_viewers(), ["s1"]);
        assert_eq!(st.focused_pane(), None, "nothing to type into until the viewer is up");

        // Once the viewer pane exists, keys go to it like any ptyd pane.
        st.viewers.insert("s1".into(), viewer_info("s1", true));
        let viewer = ninox_core::runtime::viewer_pane_id(std::process::id(), "s1");
        assert_eq!(st.focused_pane().as_deref(), Some(viewer.as_str()));
        assert_eq!(handle_key(&mut st, key('q')), Action::Write { pane: viewer.clone(), bytes: b"q".to_vec() });
        assert_eq!(handle_paste(&mut st, "hi"), Action::Write { pane: viewer, bytes: b"hi".to_vec() });
    }

    #[test]
    fn tmux_rows_fall_back_to_full_screen_only_without_ptyd() {
        let mut st = state_with_rows(1);
        st.viewers_unavailable = Some("no host".into());
        assert_eq!(handle_key(&mut st, code(KeyCode::Enter)), Action::Connect("s0".into()));
        st.viewers_unavailable = None;
        assert_eq!(handle_key(&mut st, code(KeyCode::Enter)), Action::None);
        // Explicit full-screen attach stays one chord away.
        handle_key(&mut st, prefix());
        assert_eq!(handle_key(&mut st, key('a')), Action::Connect("s0".into()));
    }

    #[test]
    fn a_closed_viewer_is_not_respawned_until_the_row_is_reopened() {
        let mut st = state_with_rows(1);
        st.viewer_errors.insert("s0".into(), "tmux view closed".into());
        assert!(st.wanted_viewers().is_empty());
        handle_key(&mut st, code(KeyCode::Enter));
        assert_eq!(st.wanted_viewers(), ["s0"]);
        st.rows[0].session.status = SessionStatus::Terminated;
        assert!(st.wanted_viewers().is_empty(), "a dead session gets no viewer");
    }

    #[test]
    fn overview_wants_viewers_for_visible_tmux_tiles_only() {
        use crate::tui::layout::Tile;
        let mut st = state_with_rows(3);
        with_live_pane(&mut st, "s0");
        st.view = View::Overview;
        st.layout.tiles = vec![
            Tile { index: 0, outer: Rect::default(), inner: Rect::default() },
            Tile { index: 1, outer: Rect::default(), inner: Rect::default() },
        ];
        assert_eq!(st.wanted_viewers(), ["s1"], "s0 is ptyd-backed; s2 is off screen");
    }

    #[test]
    fn escape_chord_leaves_the_pane_whatever_the_prefix() {
        let mut st = state_with_rows(1);
        with_live_pane(&mut st, "s0");
        st.focus = Focus::Pane;
        st.zoom = true;
        let ctrl_bracket = KeyEvent::new(KeyCode::Char(']'), KeyModifiers::CONTROL);
        assert_eq!(handle_key(&mut st, ctrl_bracket), Action::None);
        assert_eq!((st.focus, st.zoom), (Focus::Sidebar, false));

        // From scroll mode and other views too.
        st.focus = Focus::Pane;
        st.mode = Mode::Scroll;
        handle_key(&mut st, ctrl_bracket);
        assert_eq!((st.focus, st.mode), (Focus::Sidebar, Mode::Normal));
        st.view = View::Brain;
        handle_key(&mut st, ctrl_bracket);
        assert_eq!(st.view, View::Board);

        // prefix Ctrl+] types a literal Ctrl+] into the agent.
        st.focus = Focus::Pane;
        handle_key(&mut st, prefix());
        assert_eq!(handle_key(&mut st, ctrl_bracket), Action::Write { pane: "s0".into(), bytes: vec![0x1d] });

        // When Ctrl+] *is* the prefix it behaves as the prefix.
        st.prefix = ESCAPE_BYTE;
        assert_eq!(handle_key(&mut st, ctrl_bracket), Action::None);
        assert_eq!(st.mode, Mode::Prefix);
    }

    #[test]
    fn number_keys_switch_tabs_from_the_sidebar_and_after_the_prefix() {
        let mut st = state_with_rows(1);
        with_live_pane(&mut st, "s0");
        assert_eq!(handle_key(&mut st, key('4')), Action::LoadBrain(String::new()));
        assert_eq!(st.view, View::Brain);
        assert_eq!(handle_key(&mut st, key('5')), Action::LoadSettings);
        assert_eq!(handle_key(&mut st, key('2')), Action::None);
        assert_eq!(st.view, View::Overview);
        assert_eq!(handle_key(&mut st, key('3')), Action::LoadPrs);
        assert_eq!(st.view, View::PrWatches);
        handle_key(&mut st, key('1'));
        assert_eq!(st.view, View::Board);

        // In a pane digits are the agent's; prefix 4 still switches.
        st.focus = Focus::Pane;
        assert_eq!(handle_key(&mut st, key('4')), Action::Write { pane: "s0".into(), bytes: b"4".to_vec() });
        handle_key(&mut st, prefix());
        assert_eq!(handle_key(&mut st, key('4')), Action::LoadBrain(String::new()));

        // Typing a brain query keeps its digits.
        handle_key(&mut st, key('/'));
        handle_key(&mut st, key('2'));
        assert_eq!((st.view, st.brain.query.as_str()), (View::Brain, "2"));
    }

    #[test]
    fn enter_on_a_ptyd_row_focuses_its_pane_and_keys_flow_to_it() {
        let mut st = state_with_rows(1);
        with_live_pane(&mut st, "s0");
        assert_eq!(handle_key(&mut st, code(KeyCode::Enter)), Action::None);
        assert_eq!(st.focus, Focus::Pane);
        // Bare keys that are sidebar commands now type into the agent.
        assert_eq!(handle_key(&mut st, key('q')), Action::Write { pane: "s0".into(), bytes: b"q".to_vec() });
        assert_eq!(handle_key(&mut st, code(KeyCode::Enter)), Action::Write { pane: "s0".into(), bytes: b"\r".to_vec() });
    }

    #[test]
    fn prefix_commands_work_from_the_pane_and_prefix_twice_sends_it() {
        let mut st = state_with_rows(2);
        with_live_pane(&mut st, "s0");
        st.focus = Focus::Pane;
        assert_eq!(handle_key(&mut st, prefix()), Action::None);
        assert_eq!(st.mode, Mode::Prefix);
        assert_eq!(handle_key(&mut st, prefix()), Action::Write { pane: "s0".into(), bytes: vec![st.prefix] });
        assert_eq!(st.mode, Mode::Normal);

        handle_key(&mut st, prefix());
        handle_key(&mut st, key('j'));
        assert_eq!(st.selected, 1, "prefix j moves to the next agent");
        handle_key(&mut st, prefix());
        assert_eq!(handle_key(&mut st, key('d')), Action::Quit, "prefix d detaches");
    }

    #[test]
    fn configurable_prefix() {
        let mut st = state_with_rows(1);
        st.prefix = 7;
        handle_key(&mut st, KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL));
        assert_eq!(st.mode, Mode::Prefix);
        handle_key(&mut st, key('?'));
        assert_eq!(st.modal, Some(Modal::Help));
    }

    #[test]
    fn kill_requires_confirmation() {
        let mut st = state_with_rows(1);
        assert_eq!(handle_key(&mut st, key('x')), Action::None);
        assert!(matches!(st.modal, Some(Modal::Confirm(Pending::Kill(ref id))) if id == "s0"));
        assert_eq!(handle_key(&mut st, key('z')), Action::None, "other keys keep the confirm open");
        assert!(st.modal.is_some());
        assert_eq!(handle_key(&mut st, key('y')), Action::Kill("s0".into()));
        assert!(st.modal.is_none());
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn restart_all_requires_confirmation() {
        let mut st = state_with_rows(2);
        assert_eq!(handle_key(&mut st, ctrl('r')), Action::None);
        assert_eq!(st.modal, Some(Modal::Confirm(Pending::RestartAll)));
        assert_eq!(handle_key(&mut st, key('n')), Action::None, "n cancels");
        assert!(st.modal.is_none());

        assert_eq!(handle_key(&mut st, ctrl('r')), Action::None);
        assert_eq!(handle_key(&mut st, key('y')), Action::RestartAll);
        assert!(st.modal.is_none());
    }

    #[test]
    fn restart_all_with_no_live_agents_skips_the_confirm() {
        let mut st = state_with_rows(1);
        st.rows[0].session.status = SessionStatus::Done;
        assert_eq!(handle_key(&mut st, ctrl('r')), Action::None);
        assert!(st.modal.is_none(), "nothing live — no point confirming");
    }

    #[test]
    fn live_count_excludes_terminal_statuses() {
        let mut st = state_with_rows(2);
        assert_eq!(st.live_count(), 2);
        st.rows[0].session.status = SessionStatus::Done;
        assert_eq!(st.live_count(), 1);
    }

    #[test]
    fn restart_all_confirm_text_names_the_live_count() {
        let mut st = state_with_rows(3);
        st.rows[0].session.status = SessionStatus::Terminated;
        let (q, _) = st.confirm_text(&Pending::RestartAll);
        assert_eq!(q, "Restart all 2 live agents?");
    }

    #[test]
    fn reap_targets_the_selected_rows_own_group() {
        let mut st = state_with_rows(3);
        st.rows[0].is_orchestrator = true;
        st.rows[0].group = Some("s0".into());
        st.rows[1].group = Some("gone-orch".into());
        st.rows[2].group = None;

        let reap = KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT);
        st.selected = 1;
        handle_key(&mut st, reap);
        assert!(matches!(st.modal, Some(Modal::Confirm(Pending::Reap(ref id))) if id == "gone-orch"));
        st.modal = None;

        st.selected = 2;
        handle_key(&mut st, reap);
        assert!(st.modal.is_none(), "an ungrouped row has no orchestrator to reap for");
    }

    /// `s0` ended (done, in a tmux-less store row) with a cached report.
    fn ended(resumable: bool, pr: bool) -> TuiState {
        use crate::tui::report::{PrFacts, ReportData, ReportSlot};
        let mut st = state_with_rows(2);
        st.rows[0].session.status = SessionStatus::Done;
        let data = ReportData {
            resumable,
            pr: pr.then(|| PrFacts { number: 7, url: Some("https://github.com/o/r/pull/7".into()), ..Default::default() }),
            ..Default::default()
        };
        if pr {
            st.rows[0].session.pr_number = Some(7);
        }
        st.reports.insert("s0".into(), ReportSlot { data: Some(data), loading: false });
        st.layout = crate::tui::layout::compute(Rect::new(0, 0, 120, 40), &st);
        st
    }

    #[test]
    fn x_removes_an_ended_session_and_kills_a_live_one() {
        let mut st = ended(false, false);
        handle_key(&mut st, key('x'));
        assert_eq!(st.modal, Some(Modal::Confirm(Pending::Remove("s0".into()))));
        assert_eq!(handle_key(&mut st, key('y')), Action::Remove("s0".into()));
        st.selected = 1;
        handle_key(&mut st, key('x'));
        assert_eq!(st.modal, Some(Modal::Confirm(Pending::Kill("s1".into()))));
    }

    #[test]
    fn bare_keys_on_an_ended_sessions_report_are_commands_not_input() {
        let mut st = ended(true, true);
        st.panes.insert("s0".into(), pane_info("s0", false));
        st.focus = Focus::Pane;
        assert!(st.report_shown());
        assert_eq!(handle_key(&mut st, key('r')), Action::Resume("s0".into()));
        assert_eq!(handle_key(&mut st, KeyEvent::new(KeyCode::Char('O'), KeyModifiers::SHIFT)), Action::OpenUrl("https://github.com/o/r/pull/7".into()));
        assert_eq!(handle_key(&mut st, key('x')), Action::None);
        assert_eq!(st.modal, Some(Modal::Confirm(Pending::Remove("s0".into()))), "x asks, never types");
        st.modal = None;
        st.layout.report_max_scroll = 3;
        handle_key(&mut st, key('j'));
        handle_key(&mut st, key('j'));
        assert_eq!((st.report_scroll, st.selected), (2, 0), "j scrolls the report, not the sidebar");
        handle_key(&mut st, code(KeyCode::Esc));
        assert_eq!(st.focus, Focus::Sidebar);
        // A session that can't be resumed says why instead.
        let mut st = ended(false, false);
        assert_eq!(handle_key(&mut st, key('r')), Action::None);
        assert!(st.notice.as_ref().is_some_and(|n| n.text.contains("can't be resumed")));
    }

    #[test]
    fn report_buttons_and_pr_link_are_clickable() {
        use crate::tui::report::ReportButton;
        let mut st = ended(true, true);
        let buttons = st.layout.report_buttons.clone();
        assert_eq!(buttons.iter().map(|(b, _)| *b).collect::<Vec<_>>(), [ReportButton::Remove, ReportButton::Resume, ReportButton::OpenPr]);
        for w in buttons.windows(2) {
            assert!(w[0].1.right() < w[1].1.x, "buttons do not overlap");
        }
        let at = |b: ReportButton| buttons.iter().find(|(x, _)| *x == b).unwrap().1;
        let r = at(ReportButton::Resume);
        assert_eq!(handle_mouse(&mut st, down(r.x + 1, r.y)), Action::Resume("s0".into()));
        assert_eq!(st.focus, Focus::Pane);
        let o = at(ReportButton::OpenPr);
        assert_eq!(handle_mouse(&mut st, down(o.right() - 1, o.y)), Action::OpenUrl("https://github.com/o/r/pull/7".into()));
        let link = st.layout.report_links[0];
        assert_eq!(handle_mouse(&mut st, down(link.x + 3, link.y)), Action::OpenUrl("https://github.com/o/r/pull/7".into()));
        let x = at(ReportButton::Remove);
        handle_mouse(&mut st, down(x.x, x.y));
        assert_eq!(st.modal, Some(Modal::Confirm(Pending::Remove("s0".into()))));
    }

    #[test]
    fn confirm_modal_has_clickable_yes_and_no() {
        let mut st = ended(false, false);
        handle_key(&mut st, key('x'));
        st.layout = crate::tui::layout::compute(Rect::new(0, 0, 120, 40), &st);
        let (yes, no) = st.layout.modal_buttons.unwrap();
        assert!(yes.width > 0 && no.x > yes.right());
        assert_eq!(handle_mouse(&mut st, down(no.x, no.y)), Action::None);
        assert!(st.modal.is_none(), "No closes it");
        handle_key(&mut st, key('x'));
        assert_eq!(handle_mouse(&mut st, down(yes.x + 2, yes.y)), Action::Remove("s0".into()));
        assert!(st.modal.is_none());
    }

    #[test]
    fn wheel_scrolls_the_report_within_its_bounds() {
        let mut st = ended(false, false);
        st.layout.report_max_scroll = 4;
        let body = st.layout.report_body.unwrap();
        handle_mouse(&mut st, mouse(MouseEventKind::ScrollDown, body.x + 2, body.y + 1));
        handle_mouse(&mut st, mouse(MouseEventKind::ScrollDown, body.x + 2, body.y + 1));
        assert_eq!(st.report_scroll, 4);
        handle_mouse(&mut st, mouse(MouseEventKind::ScrollUp, body.x + 2, body.y + 1));
        assert_eq!(st.report_scroll, 1);
    }

    #[test]
    fn q_quits_from_the_sidebar_unless_confirming() {
        let mut st = state_with_rows(1);
        handle_key(&mut st, key('x'));
        assert_eq!(handle_key(&mut st, key('n')), Action::None);
        assert!(st.modal.is_none());
        assert_eq!(handle_key(&mut st, key('q')), Action::Quit);
    }

    #[test]
    fn spawn_modal_collects_name_then_prompt_then_spawns() {
        let mut st = state_with_rows(0);
        handle_key(&mut st, key('n'));
        assert!(matches!(st.modal, Some(Modal::Spawn(_))));
        for c in "demo".chars() {
            handle_key(&mut st, key(c));
        }
        handle_key(&mut st, code(KeyCode::Enter));
        for c in "do x".chars() {
            handle_key(&mut st, key(c));
        }
        let a = handle_key(&mut st, code(KeyCode::Enter));
        assert_eq!(a, Action::Spawn { name: "demo".into(), prompt: Some("do x".into()) });
        assert!(st.modal.is_none());
    }

    #[test]
    fn spawn_modal_esc_cancels_and_empty_name_keeps_it_open() {
        let mut st = state_with_rows(0);
        st.modal = Some(Modal::Spawn(SpawnModal::default()));
        handle_key(&mut st, code(KeyCode::Enter));
        for c in "do x".chars() {
            handle_key(&mut st, key(c));
        }
        assert_eq!(handle_key(&mut st, code(KeyCode::Enter)), Action::None);
        let Some(Modal::Spawn(m)) = &st.modal else { panic!("modal must stay open on empty name") };
        assert_eq!(m.prompt, "do x");
        assert_eq!(handle_key(&mut st, code(KeyCode::Esc)), Action::None);
        assert!(st.modal.is_none());
    }

    #[test]
    fn empty_prompt_becomes_none() {
        let mut st = state_with_rows(0);
        st.modal = Some(Modal::Spawn(SpawnModal::default()));
        for c in "demo".chars() {
            handle_key(&mut st, key(c));
        }
        handle_key(&mut st, code(KeyCode::Enter));
        assert_eq!(handle_key(&mut st, code(KeyCode::Enter)), Action::Spawn { name: "demo".into(), prompt: None });
    }

    #[test]
    fn goto_filters_by_state_and_query() {
        let mut st = state_with_rows(3);
        st.rows[0].session.activity = ActivityState::Blocked;
        st.rows[1].session.activity = ActivityState::Working;
        st.rows[2].session.activity = ActivityState::Idle;
        st.rows[2].session.name = "fix-login".into();
        handle_key(&mut st, key('g'));
        handle_key(&mut st, code(KeyCode::Tab));
        let Some(Modal::Goto(g)) = st.modal.clone() else { panic!() };
        assert_eq!(g.filter, GotoFilter::Blocked);
        assert_eq!(st.goto_matches(&g), vec![0]);
        handle_key(&mut st, code(KeyCode::BackTab));
        for c in "login".chars() {
            handle_key(&mut st, key(c));
        }
        let Some(Modal::Goto(g)) = st.modal.clone() else { panic!() };
        assert_eq!(st.goto_matches(&g), vec![2]);
        assert_eq!(handle_key(&mut st, code(KeyCode::Enter)), Action::None);
        assert_eq!((st.selected, st.focus), (2, Focus::Pane));
    }

    #[test]
    fn idle_since_last_look_is_done_until_seen() {
        let mut st = state_with_rows(1);
        st.started_ms = 100;
        st.rows[0].session.activity = ActivityState::Idle;
        st.rows[0].session.activity_since = Some(50);
        assert_eq!(st.attention(&st.rows[0]), Attention::Idle);
        st.rows[0].session.activity_since = Some(200);
        assert_eq!(st.attention(&st.rows[0]), Attention::Done);
        st.now_ms = 300;
        st.mark_displayed_seen();
        assert_eq!(st.attention(&st.rows[0]), Attention::Idle);
    }

    #[test]
    fn rollup_takes_the_most_urgent_member() {
        let mut st = state_with_rows(3);
        for r in &mut st.rows {
            r.group = Some("s0".into());
        }
        st.rows[1].session.activity = ActivityState::Blocked;
        assert_eq!(st.rollup("s0"), Attention::Blocked);
    }

    #[test]
    fn unread_counts_messages_since_last_seen() {
        let mut st = state_with_rows(2);
        st.msg_counts.insert("s1".into(), 5);
        st.msg_seen.insert("s1".into(), 2);
        assert_eq!(st.unread("s1"), 3);
        assert_eq!(st.unread("s0"), 0);
        st.selected = 1;
        st.mark_displayed_seen();
        assert_eq!(st.unread("s1"), 0);
    }

    #[test]
    fn transitions_raise_notices_and_selection_follows_id() {
        let mut st = state_with_rows(2);
        st.selected = 1;
        let mut rows: Vec<Row> = vec![Row::test_row("new"), Row::test_row("s0"), Row::test_row("s1")];
        rows[2].session.activity = ActivityState::Blocked;
        rows[2].session.activity_note = Some("needs creds".into());
        st.apply_rows(rows);
        assert_eq!(st.selected_id().as_deref(), Some("s1"));
        let n = st.notice.clone().expect("blocked transition notifies");
        assert!(n.text.contains("blocked: needs creds"), "{}", n.text);
        st.now_ms = n.expires_ms;
        st.expire_notice();
        assert!(st.notice.is_none());
    }

    #[test]
    fn scroll_mode_keys_and_exit() {
        let mut st = state_with_rows(1);
        with_live_pane(&mut st, "s0");
        handle_key(&mut st, prefix());
        handle_key(&mut st, key('['));
        assert_eq!(st.mode, Mode::Scroll);
        handle_key(&mut st, key('k'));
        handle_key(&mut st, key('k'));
        assert_eq!(st.views["s0"].scroll, 2);
        handle_key(&mut st, key('j'));
        assert_eq!(st.views["s0"].scroll, 1);
        assert!(matches!(handle_key(&mut st, key('y')), Action::Yank(_)));
        handle_key(&mut st, key('q'));
        assert_eq!(st.mode, Mode::Normal);
        assert_eq!(st.views["s0"].scroll, 0);
    }

    #[test]
    fn restore_offer_appears_when_the_flag_is_raised_after_startup_and_only_once() {
        let mut st = state_with_rows(1);
        st.restore_policy = ninox_core::config::RestorePolicy::Prompt;
        let summary = RestoreSummary { workers: 1, orchestrators: 1, interrupted_at: None, flagged_at: Some(7) };
        st.offer_restore(None);
        assert!(st.modal.is_none(), "cold-started daemon hasn't reconciled yet");

        st.modal = Some(Modal::Spawn(SpawnModal::default()));
        st.offer_restore(Some(summary.clone()));
        assert!(matches!(st.modal, Some(Modal::Spawn(_))), "an open modal isn't clobbered");
        st.modal = None;

        st.offer_restore(Some(summary.clone()));
        assert!(matches!(st.modal, Some(Modal::Restore(_))));
        st.modal = None;
        st.offer_restore(Some(summary.clone()));
        assert!(st.modal.is_none(), "the same flag is offered once (a restore may still be running)");

        st.offer_restore(Some(RestoreSummary { flagged_at: Some(9), ..summary }));
        assert!(matches!(st.modal, Some(Modal::Restore(_))), "a newer interruption is offered again");
    }

    #[test]
    fn focused_agent_vanishing_returns_focus_to_the_sidebar() {
        let mut st = state_with_rows(3);
        for i in 0..3 {
            with_live_pane(&mut st, &format!("s{i}"));
        }
        st.selected = 1;
        st.focus = Focus::Pane;
        let rows: Vec<Row> = ["s0", "s2"].iter().map(|id| Row::test_row(id)).collect();
        st.apply_rows(rows);
        assert_eq!(st.focus, Focus::Sidebar);
        assert!(
            !matches!(handle_key(&mut st, key('a')), Action::Write { .. }),
            "keys meant for the vanished agent must not reach its neighbour",
        );

        // A selection that survives keeps pane focus.
        st.selected = 1;
        st.focus = Focus::Pane;
        st.apply_rows(["s0", "s2"].iter().map(|id| Row::test_row(id)).collect());
        assert_eq!((st.focus, st.selected_id().as_deref()), (Focus::Pane, Some("s2")));
    }

    #[test]
    fn typing_while_scrolled_snaps_back_to_live() {
        let mut st = state_with_rows(1);
        with_live_pane(&mut st, "s0");
        st.focus = Focus::Pane;
        st.views.entry("s0".into()).or_default().scroll = 5;
        assert!(matches!(handle_key(&mut st, key('a')), Action::Write { .. }));
        assert_eq!(st.views["s0"].scroll, 0);
    }

    #[test]
    fn overview_moves_by_grid_and_opens() {
        let mut st = state_with_rows(4);
        for i in 0..4 {
            with_live_pane(&mut st, &format!("s{i}"));
        }
        st.layout.overview_cols = 2;
        handle_key(&mut st, key('o'));
        assert_eq!(st.view, View::Overview);
        handle_key(&mut st, key('j'));
        assert_eq!(st.overview_sel, 2);
        handle_key(&mut st, key('l'));
        assert_eq!(st.overview_sel, 3);
        handle_key(&mut st, code(KeyCode::Enter));
        assert_eq!(st.view, View::Board);
        assert_eq!(st.selected, 3);
        assert_eq!(st.focus, Focus::Pane);
    }

    #[test]
    fn overview_j_k_cross_a_group_boundary_by_grid_not_flat_index() {
        // cols=2: o(0,0) w0(0,1) / w1(1,0) / solo(2,0) — the group boundary
        // pushes solo onto its own row instead of sharing w1's row, so a
        // flat `index ± cols` step would miss it (and panic on nothing: it
        // would just silently stick).
        let mut st = grouped(2);
        st.layout.overview_cols = 2;
        handle_key(&mut st, key('o'));
        st.overview_sel = 2; // w1
        handle_key(&mut st, key('j'));
        assert_eq!(st.overview_sel, 3, "down from w1 lands on solo, not off the grid");
        handle_key(&mut st, key('k'));
        assert_eq!(st.overview_sel, 2, "and back up to w1");
    }

    #[test]
    fn mouse_click_selects_sidebar_rows_and_focuses_panes() {
        let mut st = state_with_rows(3);
        with_live_pane(&mut st, "s1");
        st.layout.sidebar_list = Some(Rect::new(0, 2, 20, 10));
        st.layout.sidebar_items = st.sidebar_items();
        st.layout.pane = Some(Rect::new(20, 1, 60, 20));
        st.layout.pane_inner = Some(Rect::new(21, 2, 58, 18));
        let click = |x, y| MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: x, row: y, modifiers: KeyModifiers::NONE };
        handle_mouse(&mut st, click(3, 3));
        assert_eq!(st.selected, 1);
        assert_eq!(st.focus, Focus::Sidebar);
        assert_eq!(handle_mouse(&mut st, click(30, 5)), Action::None);
        assert_eq!(st.focus, Focus::Pane);

        // Wheel scrolls locally when the app has no mouse reporting...
        let wheel = MouseEvent { kind: MouseEventKind::ScrollUp, column: 30, row: 5, modifiers: KeyModifiers::NONE };
        handle_mouse(&mut st, wheel);
        assert_eq!(st.views["s1"].scroll, 3);
        assert_eq!(st.mode, Mode::Scroll);
        handle_mouse(&mut st, MouseEvent { kind: MouseEventKind::ScrollDown, ..wheel });
        assert_eq!(st.mode, Mode::Normal);

        // ...and is forwarded (pane-relative, SGR) when it does.
        let mut v = PaneView { live: Some(crate::tui::pane::tests::snap(&["x"], 0, 4)), ..Default::default() };
        v.live.as_mut().unwrap().modes = ninox_ptyd::Modes { mouse_reporting: true, sgr_mouse: true, ..Default::default() };
        st.views.insert("s1".into(), v);
        assert_eq!(handle_mouse(&mut st, wheel), Action::Write { pane: "s1".into(), bytes: b"\x1b[<64;10;4M".to_vec() });
    }

    fn mouse(kind: MouseEventKind, x: u16, y: u16) -> MouseEvent {
        MouseEvent { kind, column: x, row: y, modifiers: KeyModifiers::NONE }
    }
    fn down(x: u16, y: u16) -> MouseEvent {
        mouse(MouseEventKind::Down(MouseButton::Left), x, y)
    }

    /// Board laid out at 100x30 with a live pane `s1` focused whose agent
    /// has SGR mouse reporting on.
    fn board_with_mouse_app() -> TuiState {
        let mut st = state_with_rows(3);
        with_live_pane(&mut st, "s1");
        st.selected = 1;
        st.focus = Focus::Pane;
        let mut v = PaneView { live: Some(crate::tui::pane::tests::snap(&["hello world", "line two"], 0, 60)), ..Default::default() };
        v.live.as_mut().unwrap().modes = ninox_ptyd::Modes { mouse_reporting: true, sgr_mouse: true, ..Default::default() };
        st.views.insert("s1".into(), v);
        st.layout = crate::tui::layout::compute(Rect::new(0, 0, 100, 30), &st);
        st
    }

    #[test]
    fn chrome_clicks_reach_ninox_even_when_the_agent_captures_the_mouse() {
        let mut st = board_with_mouse_app();
        let lay = st.layout.clone();
        let list = lay.sidebar_list.unwrap();
        assert_eq!(handle_mouse(&mut st, down(list.x + 2, list.y)), Action::None, "sidebar click is not forwarded");
        assert_eq!((st.focus, st.selected), (Focus::Sidebar, 0));

        st.focus = Focus::Pane;
        let side = lay.sidebar.unwrap();
        assert_eq!(handle_mouse(&mut st, down(side.x + 1, side.y)), Action::None, "the sidebar title too");
        assert_eq!(st.focus, Focus::Sidebar);

        st.focus = Focus::Pane;
        let back = lay.pane_back.unwrap();
        assert_eq!(handle_mouse(&mut st, down(back.x + 2, back.y)), Action::None);
        assert_eq!(st.focus, Focus::Sidebar, "◀ fleet returns to the sidebar");

        st.focus = Focus::Pane;
        assert_eq!(handle_mouse(&mut st, down(50, lay.footer.y)), Action::None);
        assert_eq!(st.focus, Focus::Sidebar, "a footer click leaves the pane");

        st.focus = Focus::Pane;
        let brain = lay.tabs.iter().find(|t| t.view == View::Brain).unwrap().rect;
        assert_eq!(handle_mouse(&mut st, down(brain.x, brain.y)), Action::LoadBrain(String::new()));
        assert_eq!(st.view, View::Brain, "header tabs switch views from a focused pane");
        let fleet = lay.tabs[0].rect;
        handle_mouse(&mut st, down(fleet.x + 1, fleet.y));
        assert_eq!(st.view, View::Board);
    }

    #[test]
    fn pane_clicks_are_forwarded_in_pane_local_cells_and_drags_follow_the_grab() {
        let mut st = board_with_mouse_app();
        let inner = st.layout.pane_inner.unwrap();
        // Cell (4, 2) of the pane is SGR 1-based (5, 3).
        assert_eq!(
            handle_mouse(&mut st, down(inner.x + 4, inner.y + 2)),
            Action::Write { pane: "s1".into(), bytes: b"\x1b[<0;5;3M".to_vec() }
        );
        assert_eq!(st.mouse_grab.as_deref(), Some("s1"));
        // A drag over the sidebar stays with the agent, clamped to its edge.
        let a = handle_mouse(&mut st, mouse(MouseEventKind::Drag(MouseButton::Left), 0, inner.y + 2));
        assert_eq!(a, Action::Write { pane: "s1".into(), bytes: b"\x1b[<32;1;3M".to_vec() });
        let a = handle_mouse(&mut st, mouse(MouseEventKind::Up(MouseButton::Left), 0, inner.y + 2));
        assert_eq!(a, Action::Write { pane: "s1".into(), bytes: b"\x1b[<0;1;3m".to_vec() });
        assert_eq!(st.mouse_grab, None);
    }

    #[test]
    fn drag_selects_and_copies_and_shift_overrides_capture() {
        let mut st = board_with_mouse_app();
        let inner = st.layout.pane_inner.unwrap();
        let shift = |kind, x, y| MouseEvent { kind, column: x, row: y, modifiers: KeyModifiers::SHIFT };
        assert_eq!(handle_mouse(&mut st, shift(MouseEventKind::Down(MouseButton::Left), inner.x + 6, inner.y)), Action::None);
        handle_mouse(&mut st, shift(MouseEventKind::Drag(MouseButton::Left), inner.x + 3, inner.y + 1));
        let a = handle_mouse(&mut st, shift(MouseEventKind::Up(MouseButton::Left), inner.x + 3, inner.y + 1));
        assert_eq!(a, Action::Yank("world\nline".into()));
        assert!(st.selection.as_ref().is_some_and(|s| s.done), "kept highlighted");
        handle_key(&mut st, key('x'));
        assert_eq!(st.selection, None, "any key clears it");

        // Without mouse reporting a plain drag selects; a click alone does not.
        st.views.get_mut("s1").unwrap().live.as_mut().unwrap().modes = ninox_ptyd::Modes::default();
        handle_mouse(&mut st, down(inner.x, inner.y));
        assert_eq!(handle_mouse(&mut st, mouse(MouseEventKind::Up(MouseButton::Left), inner.x, inner.y)), Action::None);
        assert_eq!(st.selection, None);
        st.now_ms += 10_000;
        handle_mouse(&mut st, down(inner.x, inner.y));
        let a = handle_mouse(&mut st, mouse(MouseEventKind::Up(MouseButton::Left), inner.x + 4, inner.y));
        assert_eq!(a, Action::Yank("hello".into()));
    }

    #[test]
    fn double_click_copies_the_word() {
        let mut st = board_with_mouse_app();
        st.views.get_mut("s1").unwrap().live.as_mut().unwrap().modes = ninox_ptyd::Modes::default();
        let inner = st.layout.pane_inner.unwrap();
        st.now_ms = 1_000;
        handle_mouse(&mut st, down(inner.x + 7, inner.y));
        handle_mouse(&mut st, mouse(MouseEventKind::Up(MouseButton::Left), inner.x + 7, inner.y));
        st.now_ms = 1_200;
        assert_eq!(handle_mouse(&mut st, down(inner.x + 7, inner.y)), Action::Yank("world".into()));
        st.now_ms = 5_000;
        assert_eq!(handle_mouse(&mut st, down(inner.x + 7, inner.y)), Action::None, "too slow for a double-click");
    }

    #[test]
    fn sidebar_wheel_moves_through_the_list() {
        let mut st = board_with_mouse_app();
        let list = st.layout.sidebar_list.unwrap();
        handle_mouse(&mut st, mouse(MouseEventKind::ScrollDown, list.x, list.y));
        assert_eq!(st.selected, 2);
        handle_mouse(&mut st, mouse(MouseEventKind::ScrollUp, list.x, list.y));
        assert_eq!(st.selected, 1);
    }

    fn brain_state(grouping: crate::tui::brain::Grouping) -> TuiState {
        let mut st = state_with_rows(0);
        st.view = View::Brain;
        st.brain = crate::tui::brain::tests::sample();
        for e in &mut st.brain.entries {
            e.body = "body\n".repeat(40);
        }
        st.brain.grouping = grouping;
        st.layout = crate::tui::layout::compute(Rect::new(0, 0, 100, 30), &st);
        st
    }

    fn selected_id(st: &TuiState) -> Option<String> {
        st.brain.selected_entry().map(|e| e.id.clone())
    }

    #[test]
    fn brain_is_mouse_driven() {
        let mut st = brain_state(crate::tui::brain::Grouping::Flat);
        let bl = st.layout.brain.clone().unwrap();
        handle_mouse(&mut st, down(bl.list.x + 1, bl.list.y + 2));
        assert_eq!(selected_id(&st).as_deref(), Some("errors/socket.md"));
        handle_mouse(&mut st, mouse(MouseEventKind::ScrollUp, bl.list.x, bl.list.y));
        assert_eq!(selected_id(&st).as_deref(), Some("concepts/loose.md"));
        handle_mouse(&mut st, mouse(MouseEventKind::ScrollDown, bl.doc.x, bl.doc.y + 3));
        handle_mouse(&mut st, mouse(MouseEventKind::ScrollDown, bl.doc.x, bl.doc.y + 3));
        assert_eq!(st.brain.scroll, 6, "wheel over the doc scrolls it");
        handle_mouse(&mut st, down(bl.doc_body.x + 1, bl.doc_body.y + 1));
        assert!(st.brain.open);
        handle_mouse(&mut st, down(bl.search.x + 3, bl.search.y));
        assert!(st.brain.editing_query && !st.brain.open);
        handle_key(&mut st, key('t'));
        assert_eq!(st.brain.query, "t");
    }

    #[test]
    fn brain_tree_folds_by_chevron_and_its_buttons_act() {
        use crate::tui::layout::BrainButton;
        let mut st = brain_state(crate::tui::brain::Grouping::Tag);
        let bl = st.layout.brain.clone().unwrap();
        // Line 4 is the `▾ tmux  (2)` header: a click on its name selects it,
        // one on the chevron folds it.
        handle_mouse(&mut st, down(bl.list.x + 4, bl.list.y + 4));
        assert_eq!(st.brain.cursor.group.as_deref(), Some("tmux"));
        assert_eq!(st.brain.items().len(), 9, "selecting a header doesn't fold it");
        handle_mouse(&mut st, down(bl.list.x, bl.list.y + 4));
        assert_eq!(st.brain.items().len(), 7, "the chevron folds it");
        st.layout = crate::tui::layout::compute(Rect::new(0, 0, 100, 30), &st);
        let bl = st.layout.brain.clone().unwrap();
        assert!(!bl.buttons.iter().any(|(b, _)| *b == BrainButton::Edit), "no Edit on a header");
        let (_, new) = *bl.buttons.iter().find(|(b, _)| *b == BrainButton::New).unwrap();
        assert_eq!(
            handle_mouse(&mut st, down(new.x + 1, new.y)),
            Action::NewBrain { entry_type: "concepts".into(), tags: vec!["tmux".into()] },
            "a new entry is pre-tagged with the group it's added from"
        );

        handle_mouse(&mut st, down(bl.list.x + 4, bl.list.y + 3));
        assert_eq!(selected_id(&st).as_deref(), Some("patterns/Alpha.md"));
        st.layout = crate::tui::layout::compute(Rect::new(0, 0, 100, 30), &st);
        let bl = st.layout.brain.clone().unwrap();
        let rect = |b: BrainButton| bl.buttons.iter().find(|(k, _)| *k == b).unwrap().1;
        assert_eq!(handle_mouse(&mut st, down(rect(BrainButton::Edit).x, bl.doc.y)), Action::EditBrain("patterns/Alpha.md".into()));
        assert_eq!(handle_mouse(&mut st, down(rect(BrainButton::Delete).x + 2, bl.doc.y)), Action::None);
        assert_eq!(st.modal, Some(Modal::Confirm(Pending::DeleteBrain("patterns/Alpha.md".into()))));
        let (q, _) = st.confirm_text(&Pending::DeleteBrain("patterns/Alpha.md".into()));
        assert_eq!(q, "Delete brain entry Alpha?");
        st.layout = crate::tui::layout::compute(Rect::new(0, 0, 100, 30), &st);
        let (yes, _) = st.layout.modal_buttons.unwrap();
        assert_eq!(handle_mouse(&mut st, down(yes.x + 1, yes.y)), Action::DeleteBrain("patterns/Alpha.md".into()));
    }

    #[test]
    fn brain_keys_are_bare() {
        use crate::tui::brain::Grouping;
        let mut st = brain_state(Grouping::Tag);
        assert_eq!(handle_key(&mut st, key('e')), Action::None, "nothing to edit on a header");
        handle_key(&mut st, key('j'));
        assert_eq!(handle_key(&mut st, key('e')), Action::EditBrain("concepts/ptyd.md".into()));
        assert_eq!(
            handle_key(&mut st, key('a')),
            Action::NewBrain { entry_type: "concepts".into(), tags: vec!["runtime".into()] }
        );
        handle_key(&mut st, key('D'));
        assert_eq!(st.modal, Some(Modal::Confirm(Pending::DeleteBrain("concepts/ptyd.md".into()))));
        assert_eq!(handle_key(&mut st, key('n')), Action::None);
        assert!(st.modal.is_none());
        handle_key(&mut st, key('h'));
        assert_eq!(st.brain.cursor.id, None, "h goes up to the group header");
        handle_key(&mut st, code(KeyCode::Enter));
        assert!(st.brain.is_folded("runtime"), "Enter on a header folds");
        handle_key(&mut st, key(' '));
        assert!(!st.brain.is_folded("runtime"), "space unfolds");
        handle_key(&mut st, key('l'));
        handle_key(&mut st, key('j'));
        handle_key(&mut st, key('l'));
        assert!(st.brain.open);
        handle_key(&mut st, key('j'));
        assert_eq!(st.brain.scroll, 1, "j scrolls the open entry");
        handle_key(&mut st, code(KeyCode::Esc));
        assert!(!st.brain.open);
        handle_key(&mut st, key('t'));
        assert_eq!(st.brain.grouping, Grouping::Type);
        assert_eq!(selected_id(&st).as_deref(), Some("concepts/ptyd.md"), "regrouping keeps the entry");
        handle_key(&mut st, key('G'));
        assert_eq!(selected_id(&st).as_deref(), Some("patterns/Alpha.md"));
        assert_eq!(handle_key(&mut st, key('4')), Action::LoadBrain(String::new()), "digits still switch tabs");
    }

    fn prs_state() -> TuiState {
        let mut st = state_with_rows(4);
        st.rows[1].session.id = "w1".into();
        st.prs.rows = crate::tui::prs::tests::sample();
        st.view = View::PrWatches;
        st.layout = crate::tui::layout::compute(Rect::new(0, 0, 120, 30), &st);
        st
    }

    #[test]
    fn prs_are_navigable_and_open_in_the_browser() {
        let mut st = prs_state();
        assert_eq!(st.prs.selected_row().unwrap().number, 7);
        assert_eq!(handle_key(&mut st, code(KeyCode::Enter)), Action::OpenUrl("https://github.com/acme/api/pull/7".into()));
        handle_key(&mut st, key('j'));
        handle_key(&mut st, key('j'));
        assert_eq!(st.prs.selected_row().unwrap().number, 142, "j skips repo headers");
        assert_eq!(handle_key(&mut st, key('o')), Action::OpenUrl("https://github.com/acme/web/pull/142".into()));
        handle_key(&mut st, key('G'));
        assert_eq!(st.prs.selected_row().unwrap().number, 9);
        assert_eq!(handle_key(&mut st, key('s')), Action::None);
        assert_eq!(st.view, View::PrWatches, "a watch from outside ninox has no session to show");
        handle_key(&mut st, key('k'));
        handle_key(&mut st, key('s'));
        assert_eq!((st.view, st.selected_id().as_deref()), (View::Board, Some("w1")), "s shows the PR's session");
        assert_eq!(handle_key(&mut st, key('p')), Action::LoadPrs);
        handle_key(&mut st, code(KeyCode::Esc));
        assert_eq!(st.view, View::Board);
    }

    #[test]
    fn pr_rows_hit_test_number_open_and_session() {
        let mut st = prs_state();
        let pl = st.layout.prs.clone().unwrap();
        let rect = |rects: &[(usize, Rect)], i: usize| rects.iter().find(|(j, _)| *j == i).unwrap().1;
        // rows[2] is acme/web#142 on line 5 (after a header, a gap, a header and #150).
        let num = rect(&pl.numbers, 2);
        assert_eq!(num.y, pl.list.y + 5);
        let url = Action::OpenUrl("https://github.com/acme/web/pull/142".into());
        assert_eq!(handle_mouse(&mut st, down(num.x + 1, num.y)), url);
        assert_eq!(st.prs.selected_row().unwrap().number, 142);
        let open = rect(&pl.opens, 1);
        assert_eq!(handle_mouse(&mut st, down(open.x + 2, open.y)), Action::OpenUrl("https://github.com/acme/web/pull/150".into()));
        assert_eq!(handle_mouse(&mut st, down(pl.list.x + 30, pl.list.y)), Action::None, "a repo header is not a row");
        assert_eq!(st.prs.selected_row().unwrap().number, 150);
        handle_mouse(&mut st, down(pl.list.x + 30, pl.list.y + 1));
        assert_eq!(st.prs.selected_row().unwrap().number, 7, "a click elsewhere on a row selects it");
        handle_mouse(&mut st, mouse(MouseEventKind::ScrollDown, pl.list.x, pl.list.y));
        assert_eq!(st.prs.selected_row().unwrap().number, 150);
        let sess = rect(&pl.sessions, 2);
        handle_mouse(&mut st, down(sess.x, sess.y));
        assert_eq!((st.view, st.selected_id().as_deref()), (View::Board, Some("w1")));
    }

    #[test]
    fn paste_is_bracketed_per_pane_mode() {
        let mut st = state_with_rows(1);
        with_live_pane(&mut st, "s0");
        st.focus = Focus::Pane;
        assert_eq!(handle_paste(&mut st, "hi"), Action::Write { pane: "s0".into(), bytes: b"hi".to_vec() });
        st.focus = Focus::Sidebar;
        assert_eq!(handle_paste(&mut st, "hi"), Action::None);
    }

    #[test]
    fn restore_modal_confirms() {
        let mut st = state_with_rows(0);
        st.modal = Some(Modal::Restore(RestoreSummary { workers: 2, orchestrators: 1, interrupted_at: None, flagged_at: None }));
        assert_eq!(handle_key(&mut st, key('?')), Action::None);
        assert!(st.modal.is_some());
        assert_eq!(handle_key(&mut st, key('y')), Action::Restore);
    }

    #[test]
    fn brain_query_editing() {
        let mut st = state_with_rows(0);
        assert_eq!(handle_key(&mut st, key('b')), Action::LoadBrain(String::new()));
        assert_eq!(st.view, View::Brain);
        handle_key(&mut st, key('/'));
        for c in "tmux".chars() {
            handle_key(&mut st, key(c));
        }
        assert_eq!(st.brain.query, "tmux", "the filter applies as you type");
        assert_eq!(handle_key(&mut st, code(KeyCode::Enter)), Action::SearchBrain("tmux".into()), "Enter adds semantic matches");
        assert_eq!(st.brain.query, "tmux", "and keeps the filter");
        handle_key(&mut st, key('/'));
        handle_key(&mut st, code(KeyCode::Esc));
        assert!(st.brain.query.is_empty(), "Esc while searching clears it");
        handle_key(&mut st, code(KeyCode::Esc));
        assert_eq!(st.view, View::Board);
    }

    #[test]
    fn pane_header_buttons_kill_and_zoom_without_any_prefix() {
        let mut st = board_with_mouse_app();
        let (kill, zoom) = (st.layout.pane_kill.unwrap(), st.layout.pane_zoom.unwrap());
        assert!(kill.x > st.layout.pane_back.unwrap().x && zoom.x > kill.right(), "after ◀ fleet");
        assert_eq!(handle_mouse(&mut st, down(kill.x + 1, kill.y)), Action::None, "never forwarded to the agent");
        assert_eq!(st.modal, Some(Modal::Confirm(Pending::Kill("s1".into()))));
        assert_eq!(handle_key(&mut st, key('y')), Action::Kill("s1".into()));
        handle_mouse(&mut st, down(zoom.x + 1, zoom.y));
        assert!(st.zoom);
        st.layout = crate::tui::layout::compute(Rect::new(0, 0, 100, 30), &st);
        let zoom = st.layout.pane_zoom.unwrap();
        handle_mouse(&mut st, down(zoom.x + 1, zoom.y));
        assert!(!st.zoom);
    }

    #[test]
    fn an_ended_session_has_no_kill_button_its_report_has_remove() {
        let st = ended(false, false);
        assert!(st.layout.pane_back.is_some());
        assert!(st.layout.pane_kill.is_none() && st.layout.pane_zoom.is_none());
    }

    #[test]
    fn killing_an_orchestrator_says_its_workers_keep_running() {
        let st = grouped(2);
        let (q, what) = st.confirm_text(&Pending::Kill("o".into()));
        assert_eq!(q, "Kill orchestrator o?");
        assert!(what.contains("workers keep running"), "{what}");
    }

    #[test]
    fn c_bracket_then_bare_x_kills_or_removes_an_orchestrator_row() {
        let mut st = grouped(2);
        with_live_pane(&mut st, "o");
        st.focus = Focus::Pane;
        handle_key(&mut st, KeyEvent::new(KeyCode::Char(']'), KeyModifiers::CONTROL));
        assert_eq!(st.focus, Focus::Sidebar);
        handle_key(&mut st, key('x'));
        assert_eq!(st.modal, Some(Modal::Confirm(Pending::Kill("o".into()))));
        st.modal = None;

        st.panes.clear();
        st.rows[0].session.status = SessionStatus::Terminated;
        handle_key(&mut st, key('x'));
        assert_eq!(st.modal, Some(Modal::Confirm(Pending::Remove("o".into()))));
        assert_eq!(handle_key(&mut st, key('y')), Action::Remove("o".into()), "the engine removes its workers with it");
    }

    #[test]
    fn the_selected_sidebar_row_has_a_close_button_and_right_click_asks_too() {
        let mut st = grouped(2);
        st.layout = crate::tui::layout::compute(Rect::new(0, 0, 120, 30), &st);
        let close = st.layout.sidebar_close.unwrap();
        let list = st.layout.sidebar_list.unwrap();
        assert_eq!(close.y, list.y, "on the selected (first) row");
        assert_eq!(close.right(), st.layout.sidebar.unwrap().right() - 2, "at the row's right edge, before the rule");
        handle_mouse(&mut st, down(close.x + 1, close.y));
        assert_eq!(st.modal, Some(Modal::Confirm(Pending::Kill("o".into()))));
        st.modal = None;

        let right = mouse(MouseEventKind::Down(MouseButton::Right), list.x + 3, list.y + 2);
        handle_mouse(&mut st, right);
        assert_eq!(st.selected_id().as_deref(), Some("w1"));
        assert_eq!(st.modal, Some(Modal::Confirm(Pending::Kill("w1".into()))));
        st.modal = None;
        st.layout = crate::tui::layout::compute(Rect::new(0, 0, 120, 30), &st);
        assert_eq!(st.layout.sidebar_close.unwrap().y, list.y + 2, "follows the selection");
    }

    fn settings_state() -> TuiState {
        let mut st = state_with_rows(1);
        st.view = View::Settings;
        st.settings.reload(Ok(ninox_core::config::AppConfig::default()));
        st.layout = crate::tui::layout::compute(Rect::new(0, 0, 140, 50), &st);
        st
    }

    fn select_setting(st: &mut TuiState, id: FieldId) {
        st.settings.selected = st.settings.fields.iter().position(|f| f.id == id).unwrap();
    }

    #[test]
    fn settings_keys_step_choices_and_edit_text_inline() {
        let mut st = settings_state();
        assert_eq!(st.settings.selected_field().unwrap().id, FieldId::RuntimeBackend);
        let step = |back| Action::SaveSetting(Change::Step { id: FieldId::RuntimeBackend, back });
        assert_eq!(handle_key(&mut st, code(KeyCode::Enter)), step(false));
        assert_eq!(handle_key(&mut st, key(' ')), step(false));
        assert_eq!(handle_key(&mut st, code(KeyCode::Left)), step(true));
        handle_key(&mut st, key('j'));
        assert_eq!(st.settings.selected_field().unwrap().id, FieldId::Prefix);

        assert_eq!(handle_key(&mut st, code(KeyCode::Enter)), Action::None);
        assert_eq!(st.settings.editing.as_deref(), Some(ninox_core::config::DEFAULT_PREFIX));
        handle_key(&mut st, KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        for c in "Ctrl+g5".chars() {
            assert_eq!(handle_key(&mut st, key(c)), Action::None, "digits type, they don't switch tabs");
        }
        assert_eq!(st.view, View::Settings);
        handle_key(&mut st, code(KeyCode::Backspace));
        assert_eq!(
            handle_key(&mut st, code(KeyCode::Enter)),
            Action::SaveSetting(Change::Set { id: FieldId::Prefix, text: "Ctrl+g".into() })
        );
        assert!(st.settings.editing.is_some(), "stays open until the save succeeds");
        handle_key(&mut st, code(KeyCode::Esc));
        assert!(st.settings.editing.is_none());
        assert_eq!(st.view, View::Settings, "esc only closed the editor");

        select_setting(&mut st, FieldId::OpenFile);
        assert_eq!(handle_key(&mut st, code(KeyCode::Enter)), Action::EditConfig);
        assert_eq!(handle_key(&mut st, key('e')), Action::EditConfig);
        assert_eq!(handle_key(&mut st, key('G')), Action::None);
        assert_eq!(st.settings.selected, st.settings.fields.len() - 1);
        handle_key(&mut st, code(KeyCode::Esc));
        assert_eq!(st.view, View::Board);
    }

    #[test]
    fn settings_rows_are_clickable() {
        let mut st = settings_state();
        let sl = st.layout.settings.clone().unwrap();
        let line_of = |id: &FieldId, st: &TuiState| {
            let i = st.settings.fields.iter().position(|f| &f.id == id).unwrap();
            let k = sl.lines.iter().position(|l| *l == crate::tui::layout::SettingsLine::Field(i)).unwrap();
            sl.list.y + (k - sl.offset) as u16
        };
        let y = line_of(&FieldId::PrWatch, &st);
        assert_eq!(handle_mouse(&mut st, down(sl.list.x + 3, y)), Action::None, "a label click selects");
        assert_eq!(st.settings.selected_field().unwrap().id, FieldId::PrWatch);
        assert_eq!(
            handle_mouse(&mut st, down(sl.list.x + 3, y)),
            Action::SaveSetting(Change::Step { id: FieldId::PrWatch, back: false }),
            "a second click toggles"
        );
        let y = line_of(&FieldId::Port, &st);
        assert_eq!(handle_mouse(&mut st, down(sl.value_x + 1, y)), Action::None);
        assert_eq!(st.settings.editing.as_deref(), Some("8080"), "a value click opens the editor at once");
        assert_eq!(handle_mouse(&mut st, down(sl.open.x + 1, sl.open.y)), Action::EditConfig);
        let wheel = mouse(MouseEventKind::ScrollDown, sl.list.x + 3, sl.list.y);
        st.settings.editing = None;
        let before = st.settings.selected;
        handle_mouse(&mut st, wheel);
        assert_eq!(st.settings.selected, before + 1);
    }
}
