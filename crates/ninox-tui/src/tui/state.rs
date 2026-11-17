//! Client-owned UI state (selection, focus, modals, scroll, layout) and the
//! pure key/mouse → `Action` reducer. Shared facts (sessions, PRs, cost,
//! pane metadata) are copied in from the store/ptyd by the loop in `mod.rs`.

use std::collections::{HashMap, HashSet};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ninox_core::runtime::TranscriptEntry;
use ninox_core::store::UserRequestRow;
use ninox_core::types::{ActivityState, MachineHealth, PrWatch, Session, SessionStatus};
use ninox_overview::islands::IslandKey;
use ninox_ptyd::PaneInfo;
use ratatui::layout::Rect;

use super::backend::RestoreSummary;
use super::kanban::KanbanView;
use super::keys;
use super::layout::Layout;
use super::palette::Palette;
use super::pane::PaneView;
use super::remotes::RemotesView;
use super::report::{ReportButton, ReportSlot};
use super::settings::{Change, Kind, SettingsView};
use super::spawn;
use super::worker_types;
use super::workspaces_panel::{WorkspaceRowView, WorkspacesPanelState};

#[derive(Clone, Debug)]
pub struct Row {
    pub session: Session,
    pub is_orchestrator: bool,
    /// Id of the orchestrator whose group this row belongs to (its own id
    /// for an orchestrator row); `None` for ungrouped/orphan sessions.
    pub group: Option<String>,
    /// `session.machine_id`'s last-observed connection health (see
    /// `Store::machine_health`) — `None` for a local session or one whose
    /// machine has no recorded health yet. Flows into the Fleet tab's
    /// island model (`FleetState::sync`) for the remote-island connection
    /// badge.
    pub machine_health: Option<MachineHealth>,
    /// 1-based place in its location's spawn queue while `Queued`.
    pub queue_position: Option<usize>,
    /// Every PR the session is responsible for, open first; filled by `refresh`.
    pub prs: Vec<ninox_core::session_prs::SessionPr>,
}

impl Row {
    pub fn id(&self) -> &str {
        &self.session.id
    }

    pub fn status(&self) -> &'static str {
        ninox_ui_shared::cli::status_slug(&self.session.status)
    }

    #[cfg(test)]
    pub fn test_row(id: &str) -> Self {
        Self {
            session: ninox_ui_shared::cli::test_fixtures::session(id, None, SessionStatus::Working),
            is_orchestrator: false,
            group: None,
            machine_health: None,
            queue_position: None, prs: Vec::new(),
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

/// Why a session is pinned under "Needs you", most urgent first — exactly
/// `ninox_overview::status::classify_member`'s `MemberStatus::NeedsYou`
/// conditions (blocked activity, CI failure, or a pending user-request —
/// the last not yet wired up anywhere, see that module's doc comment), so
/// the pinned block, the header's "N need you" count and an island's `!n`
/// tag (`Island::needs_attention`, built from the same classification)
/// never disagree. A mergeable/review-pending/unread session is "waiting
/// on you to act," not "needs you" — `MemberStatus::WaitingOnPr`, a
/// different bucket entirely.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Need {
    Blocked,
    CiFailed,
}

impl Need {
    pub fn label(self) -> String {
        match self {
            Self::Blocked => "blocked".into(),
            Self::CiFailed => "CI failed".into(),
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
    /// Remove a worker type's `[[worker_types]]` config entry (by name).
    DeleteWorkerType(String),
    /// Drop a customized built-in's `[[worker_types]]` entry (by name).
    ResetWorkerType(String),
    /// Restart every live worker and orchestrator session (fleet-wide, not
    /// scoped to the selected row) — same batch path as `ninox restart --all`.
    RestartAll,
    /// Restart the background server (`ninox server restart`).
    RestartServer,
    /// Relaunch the sessions that hit a harness login failure.
    RestartAffected(Vec<String>),
    /// Relaunch every interrupted session (the Board's Resume all).
    ResumeAll,
    /// Relaunch one session in place, resuming its conversation (the GUI's
    /// Restart/Resume button).
    Restart(String),
    /// Kill one session and respawn it under the same id on a fresh
    /// conversation (the GUI's Re-file button).
    Refile(String),
    /// Forget a saved machine profile (by id); the remote machine itself is
    /// untouched.
    RemoveMachine(String),
    /// Forget a brain source's config entry (by name); its remote and local
    /// cache are untouched.
    RemoveBrainSource(String),
    /// Accept a ghost plan's undecided nodes (by plan id).
    GhostAccept(String),
    /// Discard a ghost plan's undecided nodes (by plan id).
    GhostDiscard(String),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum View {
    #[default]
    Fleet,
    Board,
    Plans,
    Brain,
    WorkerTypes,
    Remotes,
    UserRequests,
    /// Reached with `7`; not a header tab (the tab bar has no width left).
    Idle,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Focus {
    #[default]
    Sidebar,
    Pane,
    /// The inspector beside the pane (Tab from the pane, via the prefix).
    Inspector,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Normal,
    Prefix,
    Scroll,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AddRemoteModal {
    pub host: String,
    pub label: String,
    pub on_label_field: bool,
    /// Set when Enter was pressed with an empty Host; cleared on the next edit.
    pub error: Option<String>,
}

/// `t`'s one-line composer, addressed to the session selected when it
/// opened (by id, so a selection change behind the modal can't redirect
/// the draft).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageComposer {
    pub id: String,
    pub name: String,
    pub draft: String,
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
    Delete(DeleteConfirm),
    Spawn,
    AddRemote(AddRemoteModal),
    Goto(Goto),
    Help,
    Message(MessageComposer),
    /// `N`'s list; the cursor counts from the newest.
    Notifications(usize),
    Restore(RestoreSummary),
    /// Settings, moved off the tab bar in v1 (see the fleet-overview-ui-
    /// vision-design spec's Navigation section): opened by `s`, rendered as
    /// an overlay over whatever view was active, reading/writing the same
    /// persistent `TuiState::settings` field the tab used to.
    Settings,
    /// Brain › Sources' add (`a`) / edit (`e`) form.
    SourceForm(super::sources::SourceForm),
}

/// `D`'s confirm: what deleting `root` removes, read off the loop (it runs
/// git in every worktree), the same plan and wording as the GUI's modal.
#[derive(Clone, Debug, PartialEq)]
pub struct DeleteConfirm {
    pub root:     String,
    pub plan:     Option<Result<ninox_core::events::DeletePlan, String>>,
    /// The "Also delete N worktrees with uncommitted or unpushed work"
    /// override, off until `f`.
    pub force:    bool,
    pub expanded: bool,
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
use super::fleet::{FleetFocus, FleetState, GridDirection, PromptTarget};

pub struct TuiState {
    pub rows: Vec<Row>,
    /// Header counter, e.g. "6/6 workers running locally · 3 orchestrators · 2 queued".
    pub limits_headline: String,
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
    pub inspector_tab: super::inspector::Tab,
    /// The Info tab's scroll offset, in pre-wrapped lines.
    pub inspector_scroll: u16,
    /// The help modal's scroll offset; back to 0 whenever it closes.
    pub help_scroll: u16,
    pub diff: super::inspector::DiffView,
    /// The Fleet tab's tree, collapsed to a rail (`[` key). Starts collapsed
    /// on the Fleet tab — now that the islands grid shows every orchestrator
    /// at a glance, the tree doesn't need to be the default hierarchy view.
    pub tree_collapsed: bool,
    pub kanban: KanbanView,
    pub remotes: RemotesView,
    /// Recomputed each `refresh()` tick from `AppConfig.remote_machines`
    /// and `Store::machine_health()` (the latter was already read every
    /// tick for `build_rows`; this adds one `AppConfig::load()` per tick).
    pub remote_rows: Vec<super::remotes::RemoteRow>,
    /// The Remotes tab's `W` panel (MLOPS-5418): leftover agent worktrees.
    /// `workspace_rows` is only recomputed while the panel is open — unlike
    /// `remote_rows`, building it shells out to `git` per worktree, real
    /// cost not worth paying on every `refresh()` tick while the panel is
    /// closed.
    pub workspaces: WorkspacesPanelState,
    pub workspace_rows: Vec<WorkspaceRowView>,
    pub prefix: u8,
    pub daemon_up: bool,
    pub ptyd: PtydState,
    pub panes: HashMap<String, PaneInfo>,
    pub views: HashMap<String, PaneView>,
    pub pr_watches: Vec<PrWatch>,
    /// `brain_set::session_brains_line` per session id, recomputed each
    /// refresh — the view never sees `AppConfig`.
    pub brain_lines: std::collections::HashMap<String, String>,
    /// How many `pr_watches` had no matching session the last time `refresh`
    /// checked — a watch registered via `ninox open --pr` for a PR no known
    /// session owns. With the PR list removed, there is no per-node home
    /// for a watch like this; it surfaces as a one-time notice instead of
    /// vanishing silently. Tracked so the notice fires only when this
    /// count changes, not on every refresh tick.
    pub orphan_watch_count: usize,
    pub msg_counts: HashMap<String, u64>,
    pub msg_seen: HashMap<String, u64>,
    /// Messages held for each target: (waiting under the session limit,
    /// unreachable and being retried).
    pub held: HashMap<String, (usize, usize)>,
    /// Messages each sender has queued on an unreachable target.
    pub queued_by_sender: HashMap<String, usize>,
    pub seen_ms: HashMap<String, i64>,
    pub started_ms: i64,
    pub now_ms: i64,
    pub notice: Option<Notice>,
    /// Undismissed notifications from the store, oldest first, capped at
    /// `MAX_NOTIFICATIONS` (the GUI panel's cap); `N` lists them.
    pub notifications: std::collections::VecDeque<ninox_core::types::Notification>,
    /// Highest store `seq` already shown; later rows toast, the backlog on
    /// the first sync doesn't.
    pub notif_cursor: Option<i64>,
    /// Dismissals made in the modal, written to the store on the next sync.
    pub dismissed_ids: Vec<String>,
    pub dismissed_all: Option<i64>,
    pub brain: BrainView,
    pub fleet: FleetState,
    pub settings: SettingsView,
    pub worker_types: super::worker_types::WorkerTypesView,
    /// The Spawn modal's fields — persistent so `Action::LoadSpawnForm`
    /// (fired on `n`) can replace it with a fresh build from the live
    /// config, mirroring `settings`/`worker_types`'s reload pattern.
    pub spawn_form: super::spawn::SpawnForm,
    /// Geometry of the last frame, for mouse hit-testing and paging.
    pub layout: Layout,
    /// Screen rects of the PR chips drawn last frame, rebuilt every `draw`.
    pub chip_hits: std::cell::RefCell<Vec<(ratatui::layout::Rect, super::pr_chips::ChipTarget)>>,
    /// Click targets of chip spans built since the last `pr_chips::register`.
    pub chip_scratch: std::cell::RefCell<Vec<super::pr_chips::ChipTarget>>,
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
    /// Structured transcript entries from `Event::SessionTranscript`, for
    /// ACP-native sessions with no raw terminal (see the cross-harness-
    /// messaging design's "Rendering implications" section). Populated
    /// live by `push_transcript_entry`; a session with no entries here
    /// renders its ordinary pty view instead.
    pub transcripts: HashMap<String, Vec<TranscriptEntry>>,
    /// Set once at startup when `config.toml` fails to parse — unlike
    /// `notice`/`deferred_notice`, this never expires: it stays in the
    /// footer (taking priority over both) until the file is fixed and the
    /// TUI restarts. See MLOPS-5457.
    pub config_warning: Option<String>,
    /// Every open `ninox user-request` fleet-wide; refreshed each tick
    /// regardless of which tab is on screen, since the footer badge needs
    /// it even when the pending-requests screen itself isn't open.
    pub pending_user_requests: Vec<UserRequestRow>,
    /// Ghost plans for Fleet and the Plans tab (`ghost_view::load`).
    pub ghost_plans: Vec<ninox_core::ghost_plan::PlanWithNodes>,
    /// The Plans tab's selected plan, by id: the list re-sorts on every
    /// refresh, so a row index would drift onto another plan.
    pub plans_sel: Option<String>,
    /// Whether this process is itself running inside a tmux server — gates
    /// the pending-requests screen's keystroke-injection caveat.
    pub in_tmux: bool,
    pub user_requests_selected: usize,
    pub idle: super::idle::IdleView,
    /// `Some` while the inline answer-input sub-mode (`async` rows only) is
    /// capturing keys; `None` is plain list navigation.
    pub user_requests_draft: Option<UserRequestDraft>,
    /// Why this process failed to register as a human console, if it did —
    /// every answer attempt will fail verification, so the requests screen
    /// says so for as long as the TUI runs rather than in a transient notice.
    pub human_console_error: Option<String>,
    /// A server restart is in flight; the footer says so until it finishes.
    pub server_restarting: bool,
    /// What the server reports needs the user (an expired login, an older
    /// build), each ending in "then Restart server". Refreshed periodically.
    pub server_alerts: Vec<ninox_ui_shared::server_restart::Alert>,
}

/// An in-progress answer, bound to the request it was opened on by id —
/// `pending_user_requests` is replaced every tick and can reorder under an
/// open draft, so its index can't identify the target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserRequestDraft {
    pub id:  String,
    pub buf: String,
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
            limits_headline: String::new(),
            selected: 0,
            selected_pinned: false,
            view: View::Fleet,
            focus: Focus::Sidebar,
            mode: Mode::Normal,
            modal: None,
            zoom: false,
            inspector: false,
            inspector_tab: Default::default(),
            inspector_scroll: 0,
            help_scroll: 0,
            diff: Default::default(),
            tree_collapsed: true,
            kanban: Default::default(),
            remotes: Default::default(),
            remote_rows: Vec::new(),
            workspaces: Default::default(),
            workspace_rows: Vec::new(),
            prefix: ninox_core::config::DEFAULT_PREFIX_BYTE,
            daemon_up: false,
            ptyd: PtydState::Unknown,
            panes: HashMap::new(),
            views: HashMap::new(),
            pr_watches: Vec::new(),
            brain_lines: Default::default(),
            orphan_watch_count: 0,
            msg_counts: HashMap::new(),
            msg_seen: HashMap::new(),
            held: HashMap::new(),
            queued_by_sender: HashMap::new(),
            seen_ms: HashMap::new(),
            started_ms: 0,
            now_ms: 0,
            notice: None,
            notifications: Default::default(),
            notif_cursor: None,
            dismissed_ids: Vec::new(),
            dismissed_all: None,
            brain: BrainView::default(),
            fleet: FleetState::default(),
            settings: SettingsView::default(),
            worker_types: Default::default(),
            spawn_form: Default::default(),
            layout: Layout::default(),
            chip_hits: Default::default(),
            chip_scratch: Default::default(),
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
            transcripts: HashMap::new(),
            config_warning: None,
            pending_user_requests: Vec::new(),
            idle: Default::default(),
            ghost_plans: Vec::new(),
            plans_sel: None,
            in_tmux: false,
            user_requests_selected: 0,
            user_requests_draft: None,
            human_console_error: None,
            server_restarting: false,
            server_alerts: Vec::new(),
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
    /// Remove an ended worker (an orchestrator goes through `Delete`).
    Remove(String),
    /// Run a confirmed delete plan, forcing worktrees it showed at risk
    /// only when `force` is set.
    Delete { plan: ninox_core::events::DeletePlan, force: bool },
    Reap(String),
    /// Relaunch an ended session under its own id, `--resume`d.
    Resume(String),
    OpenUrl(String),
    /// Idle screen: prompt one orchestrator (`Some`) or each with idle workers.
    PromptIdle(Option<String>),
    /// Idle screen: free one worker's build output.
    CleanIdle(String),
    /// Open a session's workspace in the configured editor (`e`). Fire-and-
    /// forget for a GUI editor; suspends the TUI for Neovim.
    OpenInEditor(String),
    LoadSpawnForm,
    Spawn(spawn::SpawnSubmission),
    Write { pane: String, bytes: Vec<u8> },
    LoadBrain(String),
    /// Fetch backlinks/related/outlinks for the entry now selected in the
    /// Brain tab (fired whenever the selection changes).
    LoadBrainLinks(String),
    /// Manual full reindex (`R`) — not tied to one entry.
    ReindexBrain,
    /// Stepped `active_catalogue` (`c`) — reload from the new scope.
    CycleBrainCatalogue,
    /// (Re)load Brain › Sources: the configured sources and their status.
    LoadBrainSources,
    /// Save the add/edit form through the guarded config path.
    SaveBrainSource(Box<ninox_ui_shared::brain_sources::SourceEditForm>),
    ToggleBrainSource(String),
    RemoveBrainSource(String),
    /// Accept / discard a ghost plan as the user (confirmed first).
    GhostAccept(String),
    GhostDiscard(String),
    /// Sync these sources (by name) off the loop.
    SyncBrainSources(Vec<String>),
    /// Semantic (embedding) search for the query, merged after the text hits.
    SearchBrain(String),
    /// Suspend the TUI for `$VISUAL`/`$EDITOR` on a brain entry's markdown,
    /// then reindex it off the loop.
    EditBrain(String),
    /// Edit a new-entry template (type, tags) and add it if filled in.
    NewBrain { entry_type: String, tags: Vec<String> },
    DeleteBrain(String),
    LoadSettings,
    /// Read → apply → write one settings change.
    SaveSetting(Change),
    /// Suspend the TUI and open `config.toml` in `$VISUAL`/`$EDITOR`.
    EditConfig,
    LoadWorkerTypes,
    /// Read → apply → write one worker-type field change (by name).
    SaveWorkerType { name: String, change: worker_types::Change },
    NewWorkerType(String),
    DeleteWorkerType(String),
    ResetWorkerType(String),
    ExportWorkerType(String),
    /// Suspend the TUI, edit a `[[worker_types]]` TOML snippet, then parse
    /// and add it.
    ImportWorkerType,
    Restore,
    /// Kill and respawn a session on a fresh conversation, same id.
    Refile(String),
    Yank(String),
    /// Restart every live worker and orchestrator session — same batch path
    /// as `ninox restart --all`.
    RestartAll,
    /// Submits an `async` pending request's typed answer — the only path
    /// in `ninox-tui` that leads to `Store::answer_user_request`; built
    /// only by `user_requests::user_requests_key`'s draft-submit branch.
    AnswerUserRequest { id: String, answer: String },
    /// Dismiss a pending request whose requester session is independently
    /// confirmed terminal — the only path to
    /// `Store::mark_user_request_cancelled`; built only by
    /// `user_requests::user_requests_key`'s `x` branch, and only once
    /// `user_requests::requester_is_stale` confirms the requester.
    CancelUserRequest { id: String },
    RestartServer,
    RestartAffected(Vec<String>),
    /// Resume every interrupted session but the prompt agent.
    ResumeAll,
    /// Submit the ninox prompt's text to `target` (the prompt agent, or a
    /// directly `ctrl+t`-picked orchestrator). `message` already carries
    /// the `[scope: X]` prefix for a scoped (opened-island) prompt;
    /// `question` is the text as typed, for the reply panel.
    SubmitPrompt { target: PromptTarget, question: String, message: String },
    /// Deliver `message` to session `id` as a freeform message from the user
    /// (or the session this TUI runs inside) — the composer `t` opens.
    SendMessage { id: String, message: String },
    /// `ctrl+l` while the prompt or its reply panel is focused.
    ClearPromptContext,
    /// Suspend the TUI and run `ninox machine add <host> [--label <label>]`
    /// in the foreground, same as attach — `machine add`'s SSH/install/
    /// session-picker prompts run on the real terminal.
    AddRemote { host: String, label: Option<String> },
    /// Forget a saved machine profile (by id); synchronous, no SSH involved.
    RemoveMachine(String),
}

pub const NOTICE_MS: i64 = 5_000;
pub const MAX_NOTIFICATIONS: usize = 50;

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
        ("l / c", "open the agent (tmux sessions too, in the pane)"),
        ("j k / arrows", "move through the tree; ! jumps to Needs you"),
        ("space", "fold / unfold an orchestrator's workers"),
        ("x  (or click ✕)", "kill a live agent / remove an ended one; asks first"),
        ("D", "delete; an orchestrator takes everything under it; asks first"),
        ("", "removing an orchestrator removes its workers too"),
        ("r · t · O · R", "resume · message · open the PR · reap finished workers"),
        ("Ctrl+r · F", "restart in place · re-file on a fresh conversation; ask first"),
        ("Ctrl+Shift+R", "restart every live agent (or a in Ctrl+r's confirm)"),
        ("n · g · N", "spawn orchestrator · go to · notifications"),
        ("z · i · [", "zoom · inspector (prefix tab focuses) · hide/show the tree"),
        ("a", "attach full-screen (tmux: Ctrl+b d returns)"),
        ("e", "open the workspace in the configured editor"),
        ("1-6  (b s w)", "tabs: fleet · board · plans · brain · worker types · remotes"),
        ("d / q", "detach (agents keep running)"),
    ]),
    ("Fleet's islands (bare keys, no pane attached)", &[
        ("tab / shift-tab", "cycle focus: tree · grid/island · detail"),
        ("→ · ←", "cross between tree and centre (← from the centre's left edge)"),
        ("↵", "open the tree selection's island, focus moves to it"),
        ("s · m · M", "cycle sort · toggle density · cycle the machine filter"),
        ("f · Ctrl+f", "fold / unfold finished workers: this island · every island"),
        ("c", "connect to the highlighted session's live terminal"),
        ("/", "ninox prompt — sends to the selected node's orchestrator"),
        ("esc", "collapse the reply panel, then close the detail panel, then the island"),
    ]),
    ("Ninox prompt reply panel", &[
        ("/", "focus the prompt; reopens the panel with its history"),
        ("PgUp · PgDn", "scroll the reply, also while typing"),
        ("esc (in the prompt)", "hand the keyboard to the open panel"),
        ("j k · g G", "scroll a line · jump to the top / the end"),
        ("Ctrl+l", "clear context: empties the conversation"),
        ("o", "open a picked orchestrator's pane"),
    ]),
    ("Brain tab (bare keys)", &[
        ("j k · ↵ / l · h", "move · read or fold · up to the group"),
        ("space · t", "fold a group · group by tag / type / flat"),
        ("e · a · D", "edit in $EDITOR · new entry · delete (asks first)"),
        ("/", "search; matching groups unfold"),
        ("[ / ]", "cycle the Kind filter back / forward"),
        ("r · T", "cycle the Repo filter · cycle the Tag filter"),
        ("s · x", "cycle sort (Recent/A–Z/Most-linked) · clear filters"),
        ("L", "focus the Cited-by/Related/Links-to panel"),
        ("j k · ↵ · esc", "move the focused link · jump to it · back to the entry"),
        ("R · c", "reindex the whole brain now (elsewhere, R reaps finished workers) · cycle catalogue (All sources / each configured one)"),
        ("tab", "graph; s cycles Neighbourhood/Clusters; arrows walk/move, ↵ re-centres or filters by repo, o opens in list"),
        ("S", "Sources: the local folders, GitHub wikis and S3 prefixes the brain searches"),
    ]),
    ("Brain › Sources (after S)", &[
        ("j k · ↵", "move · show / hide the source's details"),
        ("a · e · d", "add · edit · remove (asks first; the remote is never touched)"),
        ("t", "include in search on / off"),
        ("s · S", "sync this source now · sync every source"),
        ("esc / q", "close the details, then back to the brain list"),
    ]),
    ("Worker types tab (bare keys)", &[
        ("tab", "switch focus between the type list and its form"),
        ("j k · ↵/space", "move · open or change a field"),
        ("n · D · R", "new type · delete · reset a customized built-in (ask first)"),
        ("x · i  (a · x on a rule)", "export · import  (add · delete a permission rule)"),
    ]),
    ("Board tab (bare keys)", &[
        ("h l", "move between status columns"),
        ("j k", "move between cards in the column"),
        ("↵ / c", "open the selected session"),
        ("/ · o · A", "filter · cycle orchestrator scope · resume all interrupted (asks first)"),
    ]),
    ("Settings (press s, or click ⚙ in the header)", &[
        ("j k", "move between sections · between fields"),
        ("→ · ↵ · tab", "from the section bar into its fields · tab goes back"),
        ("↵/space · ←/→ · e", "change a field · cycle a choice · open config.toml in $EDITOR"),
        ("esc", "cancel editing, back to the section bar, then close"),
    ]),
    ("Idle workers (not a tab — press 7)", &[
        ("j k · ↵", "move · open the worker"),
        ("m · o · x", "message · open its PR · delete (asks first)"),
        ("c", "clean its build output (target/, node_modules/.cache)"),
        ("p · P", "prompt its orchestrator · prompt every orchestrator with idle workers"),
        ("esc / q", "back"),
    ]),
    ("Pending requests (bare key: u)", &[
        ("j k", "move"),
        ("↵ on a sync row", "read-only note — answer from ninox-gui instead"),
        ("↵ on an async row", "open the draft input; ↵ again sends it, esc cancels"),
        ("u / esc / q", "back"),
    ]),
    ("Mouse", &[
        ("click", "tabs, sidebar, footer and pane buttons go to ninox"),
        ("right-click a row", "kill / remove it (asks first)"),
        ("drag / double-click", "select and copy (Shift+drag over mouse apps)"),
        ("wheel", "scrolls the pane, sidebar or brain under it"),
    ]),
];

impl TuiState {
    /// Row of the selected plan; the first row when none is selected or it
    /// has left the list.
    pub fn plans_sel_index(&self) -> usize {
        self.plans_sel.as_deref().and_then(|id| self.ghost_plans.iter().position(|p| p.plan.id == id)).unwrap_or(0)
    }

    /// The island `f` folds: the open one, else the grid's highlighted one,
    /// else the one owning the tree selection.
    pub fn focused_island(&self) -> Option<ninox_overview::islands::IslandKey> {
        self.fleet.nav.open.clone().or_else(|| self.fleet.nav.selected_island.clone()).or_else(|| {
            let id = self.selected_id()?;
            ninox_overview::nav::owning_island(&self.fleet.islands, &id)
        })
    }

    /// Keeps a new notification for `N`'s list and toasts its title.
    pub fn push_notification(&mut self, n: ninox_core::types::Notification) {
        use ninox_overview::notifications::{kind_tone, Tone};
        let level = match kind_tone(&n.kind) {
            Tone::Failed => Level::Error,
            Tone::Attention => Level::Warn,
            Tone::Open | Tone::Done => Level::Info,
        };
        // A background arrival never hides a more severe notice the user
        // may not have read yet; it's still in the list.
        let rank = |l: Level| match l {
            Level::Info => 0,
            Level::Warn => 1,
            Level::Error => 2,
        };
        let showing = self.notice.as_ref().filter(|n| n.expires_ms > self.now_ms).map(|n| rank(n.level));
        if showing.is_none_or(|r| r <= rank(level)) {
            self.notify(level, n.title.clone());
        }
        self.notifications.push_back(n);
        // The open list's cursor counts from the newest: keep it on the
        // same notification.
        if let Some(Modal::Notifications(cur)) = &mut self.modal {
            *cur += 1;
        }
        while self.notifications.len() > MAX_NOTIFICATIONS {
            self.notifications.pop_front();
        }
        if let Some(Modal::Notifications(cur)) = &mut self.modal {
            *cur = (*cur).min(self.notifications.len().saturating_sub(1));
        }
    }

    /// Flushes modal dismissals to the store, then shows rows past the
    /// cursor (toasting them unless this is the first sync) and drops ones
    /// dismissed elsewhere. Returns whether anything changed.
    pub fn sync_notifications(&mut self, store: &ninox_core::store::Store) -> bool {
        if let Some(through) = self.dismissed_all.take() {
            if let Err(e) = store.dismiss_notifications_through(through) {
                tracing::warn!("couldn't persist dismiss-all: {e}");
            }
        }
        for id in std::mem::take(&mut self.dismissed_ids) {
            if let Err(e) = store.dismiss_notification(&id) {
                tracing::warn!("couldn't persist dismissal of {id}: {e}");
            }
        }
        let Ok(rows) = store.undismissed_notifications() else { return false };
        let quiet = self.notif_cursor.is_none();
        let cursor = self.notif_cursor.unwrap_or(0);
        let before = self.notifications.len();
        let live: std::collections::HashSet<&str> = rows.iter().map(|r| r.notification.id.as_str()).collect();
        self.notifications.retain(|n| live.contains(n.id.as_str()));
        let mut changed = self.notifications.len() != before;
        for r in rows.iter().filter(|r| r.seq > cursor) {
            changed = true;
            if quiet {
                self.notifications.retain(|n| n.id != r.notification.id);
                self.notifications.push_back(r.notification.clone());
                while self.notifications.len() > MAX_NOTIFICATIONS {
                    self.notifications.pop_front();
                }
            } else {
                self.notifications.retain(|n| n.id != r.notification.id);
                self.push_notification(r.notification.clone());
            }
        }
        self.notif_cursor = Some(rows.last().map_or(cursor, |r| r.seq).max(cursor));
        if let Some(Modal::Notifications(cur)) = &mut self.modal {
            *cur = (*cur).min(self.notifications.len().saturating_sub(1));
        }
        changed
    }

    /// The `i`th notification counting from the newest.
    pub fn notification_at(&self, i: usize) -> Option<&ninox_core::types::Notification> {
        self.notifications.iter().rev().nth(i)
    }

    /// Every session a server alert says hit a harness login failure.
    pub fn login_error_sessions(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.server_alerts.iter().flat_map(|a| a.sessions.iter().cloned()).collect();
        ids.sort();
        ids.dedup();
        ids
    }

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

    /// Record a live `Event::SessionTranscript` entry, trimming the oldest
    /// once a session's buffer passes [`ninox_ui_shared::transcript::MAX_ENTRIES`].
    pub fn push_transcript_entry(&mut self, session_id: String, entry: TranscriptEntry) {
        ninox_ui_shared::transcript::push(self.transcripts.entry(session_id).or_default(), entry);
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
            View::Fleet => self.selected_row().filter(|r| needs(r)).map(|r| r.id().to_string()).into_iter().collect(),
            _ => Vec::new(),
        }
    }

    pub fn modes_of(&self, id: &str) -> ninox_ptyd::Modes {
        self.views.get(id).and_then(|v| v.live.as_ref()).map(|s| s.modes).unwrap_or_default()
    }

    /// Held-message lines for the inspector: what is waiting for this
    /// session, and what it sent that is still waiting on its target.
    pub fn held_lines(&self, id: &str) -> Vec<(&'static str, String)> {
        let plural = |n: usize| if n == 1 { "" } else { "s" };
        let mut out = Vec::new();
        if let Some(&(queued, unreachable)) = self.held.get(id) {
            if unreachable > 0 {
                out.push(("undelivered", format!("{unreachable} message{} held (unreachable, retrying)", plural(unreachable))));
            }
            if queued > 0 {
                out.push(("held", format!("{queued} message{} waiting for it to start", plural(queued))));
            }
        }
        if let Some(&n) = self.queued_by_sender.get(id).filter(|n| **n > 0) {
            out.push(("sent", format!("{n} message{} queued (target unreachable)", plural(n))));
        }
        out
    }

    /// Records fresh held counts, returning a notice for each target that
    /// just became unreachable.
    pub fn apply_held(&mut self, held: HashMap<String, (usize, usize)>, by_sender: HashMap<String, usize>) -> Vec<String> {
        let notes = held.iter()
            .filter(|(id, (_, unreachable))| *unreachable > 0 && self.held.get(*id).is_none_or(|(_, before)| *before == 0))
            .map(|(id, _)| format!("message to {id} queued: target unreachable, retrying"))
            .collect();
        self.held = held;
        self.queued_by_sender = by_sender;
        notes
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

    /// Whether (and why) a session belongs under "Needs you" — a terminal
    /// status (including `Interrupted`, terminal per `is_terminal()`) wins
    /// over everything else, same as `classify_member`'s own precedence.
    pub fn needs_you(&self, r: &Row) -> Option<Need> {
        let s = &r.session;
        if s.status.is_terminal() {
            return None;
        }
        if s.activity == ActivityState::Blocked {
            return Some(Need::Blocked);
        }
        if s.status == SessionStatus::CiFailed {
            return Some(Need::CiFailed);
        }
        None
    }

    /// Rows pinned under "Needs you", most urgent reason first.
    pub fn needs_rows(&self) -> Vec<usize> {
        let rank = |n: Need| match n {
            Need::Blocked => 0,
            Need::CiFailed => 1,
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
                    self.sync_fleet_selection();
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
            View::Fleet => self.selected_id().into_iter().collect(),
            _ => Vec::new(),
        };
        for id in ids {
            if let Some(&n) = self.msg_counts.get(&id) {
                self.msg_seen.insert(id.clone(), n);
            }
            self.seen_ms.insert(id, self.now_ms);
        }
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

    /// Replace the Remotes list from a fresh config read, clamping the
    /// selection to the new length (a machine can be removed from config
    /// while its row is selected).
    pub fn set_remote_rows(&mut self, rows: Vec<super::remotes::RemoteRow>) {
        self.remote_rows = rows;
        self.remotes.selected = self.remotes.selected.min(self.remote_rows.len().saturating_sub(1));
    }

    pub fn set_workspace_rows(&mut self, rows: Vec<WorkspaceRowView>) {
        self.workspace_rows = rows;
        self.workspaces.selected = self.workspaces.selected.min(self.workspace_rows.len().saturating_sub(1));
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
        // Only force the Fleet selection to follow the tree cursor when the
        // exact previously-selected session actually vanished (so whatever
        // `fleet.nav.selected` pointed at is meaningless now anyway) — a
        // routine data refresh that doesn't touch the selected row must
        // never re-populate a selection the user just cleared with Esc
        // (`fleet.nav.clear_selection`), or the Fleet tab's "Esc first
        // clears the detail panel" stage would be silently undone within a
        // second by the next refresh tick.
        let selected_row_vanished = matches!(kept, Some(None));
        self.selected = kept.flatten().unwrap_or(self.selected).min(self.rows.len().saturating_sub(1));
        if selected_row_vanished {
            self.sync_fleet_selection();
        }
        // A scope whose orchestrator is gone would leave the Board empty
        // with no chip lit.
        if self.kanban.scope.as_ref().is_some_and(|s| !self.rows.iter().any(|r| r.is_orchestrator && r.id() == s)) {
            self.kanban.scope = None;
        }
        let kanban_rows_in_col =
            super::kanban::column_rows(&self.rows, super::kanban::COLUMNS[self.kanban.col].status.clone(), &self.kanban).len();
        self.kanban.row = self.kanban.row.min(kanban_rows_in_col.saturating_sub(1));
        // The selection now falls on a different agent: keys typed at the
        // pane must not reach it.
        if let Some(None) = kept {
            if matches!(self.focus, Focus::Pane | Focus::Inspector) || self.mode == Mode::Scroll {
                self.focus = Focus::Sidebar;
                self.mode = Mode::Normal;
                self.zoom = false;
                notes.push((Level::Info, format!("{} is gone — back to the sidebar", keep.unwrap_or_default())));
            }
        }
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
            self.sync_fleet_selection();
        }
    }

    /// The Fleet tab's island selection/detail panel follow ordinary row
    /// navigation — the tree is the primary way to move, the islands model
    /// mirrors it, not the reverse. Every place that sets `self.selected`
    /// outside `select()` itself must call this too (final review
    /// Important #3 — `select()` alone left Goto, the Board's Enter,
    /// `toggle_collapse`'s reveal, and the initial
    /// selection on a fresh `apply_rows` out of step with the islands).
    fn sync_fleet_selection(&mut self) {
        if self.view != View::Fleet {
            return;
        }
        let Some(id) = self.rows.get(self.selected).map(|r| r.id().to_string()) else { return };
        self.fleet.nav.select(id.clone());
        // "Selecting a tree row also selects the matching island and scrolls
        // to it" — only while no island is open (an open island's grid
        // isn't on screen to scroll, and `nav.selected_island` there tracks
        // the *opened* island, not whatever the tree cursor passes through).
        if self.fleet.nav.open.is_none() {
            if let Some(key) = ninox_overview::nav::owning_island(&self.fleet.islands, &id) {
                self.fleet.nav.selected_island = Some(key.clone());
                self.scroll_grid_to_show(&key);
            }
        }
    }

    /// Mirrors `fleet.nav.selected` into the tree cursor (`self.selected`) —
    /// the reverse of `sync_fleet_selection` — so bare commands that read
    /// the tree cursor (`c`, `r`, `O`, `R`, `x`, …) keep acting on whatever
    /// is actually highlighted once focus has moved off the tree (the
    /// islands grid's opened card tree, or an island just opened from the
    /// grid). A plain index-only mirror, not `select()` — `select()`'s own
    /// `sync_fleet_selection` would immediately overwrite the island/
    /// grid-scroll state this nav-driven selection just set.
    fn sync_tree_cursor_to_nav_selection(&mut self) {
        if let Some(id) = self.fleet.nav.selected.clone() {
            if let Some(idx) = self.rows.iter().position(|r| r.id() == id) {
                self.selected = idx;
                self.selected_pinned = false;
            }
        }
    }

    /// Adjusts `fleet.grid_scroll` so `key`'s box is fully visible, using
    /// the previous frame's `layout.fleet.main` for the column count/height
    /// (one frame stale, same as `fleet_main_arrow`'s spatial nav) — a
    /// no-op before the first frame has ever been laid out.
    fn scroll_grid_to_show(&mut self, key: &IslandKey) {
        let Some(fl) = &self.layout.fleet else { return };
        let (width, height) = (fl.main.width, fl.main.height);
        if width == 0 || height == 0 {
            return;
        }
        let columns = super::fleet::column_count(width);
        let heights = super::fleet::island_grid_heights(&self.fleet.islands, |k| self.fleet.island_fold_expanded(k));
        let positions = super::fleet::column_positions(&heights, columns);
        self.fleet.grid_scroll = super::fleet::scroll_to_show_island(&positions, height, self.fleet.grid_scroll, key);
    }

    /// Open the selected agent in the pane. A tmux session shows through a
    /// viewer pane; only when none can run does it attach full-screen.
    fn open_selected(&mut self) -> Action {
        let Some(id) = self.selected_id() else { return Action::None };
        self.view = View::Fleet;
        self.sync_fleet_selection();
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
        self.view = View::Fleet;
        self.focus = Focus::Sidebar;
        self.mode = Mode::Normal;
        self.zoom = false;
        self.mouse_grab = None;
    }

    /// Whether the fleet pane shows the selected session's report (it
    /// has no live process to show or type into).
    pub fn report_shown(&self) -> bool {
        self.view == View::Fleet && self.selected_row().is_some_and(|r| super::report::shows_report(self, r))
    }

    /// `x`: remove an ended session, kill a live one; both ask first.
    fn ask_delete(&mut self) {
        let Some(id) = self.selected_id() else { return };
        self.ask_delete_id(id);
    }

    pub(super) fn ask_delete_id(&mut self, id: String) {
        if let Some(reason) = ninox_core::events::undeletable_reason(&id) {
            self.notify(Level::Warn, reason);
            return;
        }
        self.modal = Some(Modal::Delete(DeleteConfirm { root: id, plan: None, force: false, expanded: false }));
    }

    fn ask_remove_or_kill(&mut self) {
        let Some(r) = self.selected_row() else { return };
        let id = r.id().to_string();
        let ended = super::report::shows_report(self, r);
        // Removing an orchestrator takes its whole tree, sub-orchestrators
        // included: one path for that, `D`'s.
        if ended && self.is_orchestrator_id(&id) {
            self.modal = Some(Modal::Delete(DeleteConfirm { root: id, plan: None, force: false, expanded: false }));
            return;
        }
        self.modal = Some(Modal::Confirm(if ended { Pending::Remove(id) } else { Pending::Kill(id) }));
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

    /// The confirm modal offers `a`: remove everything, live workers too.
    /// What the confirm's third button (`a` or a click) does, when it has one.
    pub fn confirm_all_action(&self, pending: &Pending) -> Option<Action> {
        self.confirm_all_label(pending)?;
        match pending {
            Pending::Restart(_) => Some(Action::RestartAll),
            _ => None,
        }
    }

    /// The confirm's third button (`a`), when it has one.
    pub fn confirm_all_label(&self, pending: &Pending) -> Option<&'static str> {
        match pending {
            // Restart-all's own key, ctrl+shift+r, is indistinguishable from
            // ctrl+r in terminals without the kitty keyboard protocol.
            Pending::Restart(_) if self.live_count() > 0 => Some("Restart all"),
            _ => None,
        }
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
            Pending::Remove(id) => (format!("Remove {}?", name(id)), "deletes its record and worktree, unless the worktree has unsaved or unpushed work; the branch stays".into()),
            Pending::Reap(id) => (format!("Reap the finished workers of {}?", name(id)), "removes every ended worker in the group".into()),
            Pending::DeleteBrain(id) => {
                (format!("Delete brain entry {}?", self.brain.name_of(id)), "deletes its markdown file, reindexes, and syncs the removal to any remote".into())
            }
            Pending::DeleteWorkerType(name) => (
                format!("Delete worker type {name}?"),
                "removes its [[worker_types]] config entry".into(),
            ),
            Pending::ResetWorkerType(name) => (
                format!("Reset {name} to its built-in default?"),
                "drops your customization of it from config.toml".into(),
            ),
            Pending::ResumeAll => {
                let n = ninox_overview::board::resume_all_targets(self.rows.iter().map(|r| &r.session)).len();
                (
                    format!("Resume all {n} interrupted session{}?", plural(n)),
                    "relaunches each in place; conversations resume where the harness supports it".into(),
                )
            }
            Pending::RestartServer => (
                "Restart the ninox server?".into(),
                "running sessions keep running; the server reloads the current build and the credentials it captured".into(),
            ),
            Pending::RestartAffected(ids) => (
                format!("Relaunch {} session{} that hit a login error?", ids.len(), plural(ids.len())),
                "log in first; each relaunches in place, resuming its conversation where possible".into(),
            ),
            Pending::RestartAll => {
                let n = self.live_count();
                (
                    format!("Restart all {n} live agent{}?", plural(n)),
                    "interrupts every running session at once to pick up tooling updates; conversations resume where possible".into(),
                )
            }
            Pending::Restart(id) => (
                format!("Restart {}?", name(id)),
                "relaunches it in place; the conversation resumes where the harness supports it".into(),
            ),
            Pending::Refile(id) => (
                format!("Re-file {}?", name(id)),
                "kills it and starts a fresh conversation in the same worktree; the old one is not resumed".into(),
            ),
            Pending::RemoveMachine(id) => {
                let label = self.remote_rows.iter().find(|r| &r.id == id).map(|r| r.label.clone()).unwrap_or_else(|| id.clone());
                (format!("Remove machine {label}?"), "forgets the saved connection; the remote machine itself is untouched".into())
            }
            Pending::RemoveBrainSource(name) => (
                format!("Remove brain source {name}?"),
                "forgets it in config.toml; the remote and its local cache are not touched".into(),
            ),
            Pending::GhostAccept(id) => match self.ghost_plans.iter().find(|p| &p.plan.id == id) {
                Some(plan) => (ninox_ui_shared::ghost_view::confirm_text(plan), format!("accepts {id} and spawns its sessions; dependents wait for their dependencies")),
                None => (format!("Accept {id}?"), "spawns its sessions".into()),
            },
            Pending::GhostDiscard(id) => match self.ghost_plans.iter().find(|p| &p.plan.id == id) {
                Some(plan) => (ninox_ui_shared::ghost_view::discard_confirm_text(plan), format!("discards {id}'s undecided nodes; nothing is spawned")),
                None => (format!("Discard {id}?"), "nothing is spawned".into()),
            },
        }
    }

    /// Switch to a header tab.
    pub fn switch_view(&mut self, view: View) -> Action {
        self.mode = Mode::Normal;
        self.selection = None;
        match view {
            View::Fleet => {
                self.view = View::Fleet;
                Action::None
            }
            View::Board => {
                self.view = View::Board;
                Action::None
            }
            View::Plans => {
                self.view = View::Plans;
                Action::None
            }
            View::Brain => {
                self.view = View::Brain;
                Action::LoadBrain(self.brain.query.clone())
            }
            View::WorkerTypes => {
                self.view = View::WorkerTypes;
                Action::LoadWorkerTypes
            }
            View::Remotes => {
                self.view = View::Remotes;
                Action::None
            }
            View::UserRequests => {
                self.view = View::UserRequests;
                Action::None
            }
            View::Idle => {
                self.view = View::Idle;
                Action::None
            }
        }
    }
}

/// `1`..`6` select the header tabs.
fn tab_key(key: &KeyEvent) -> Option<View> {
    if key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
        return None;
    }
    let KeyCode::Char(c @ '1'..='7') = key.code else { return None };
    match c {
        '7' => Some(View::Idle),
        _ => Some(super::layout::TABS[(c as u8 - b'1') as usize].0),
    }
}

/// The row-level half of `restart::resume_plan` (a workspace and a
/// conversation id); whether the harness can resume needs the config, so
/// that half is still checked when the restart runs.
pub fn row_can_restart(row: &Row) -> bool {
    row.session.workspace_path.is_some() && row.session.claude_session_id.is_some()
}

pub fn handle_key(st: &mut TuiState, key: KeyEvent) -> Action {
    let before = st.fleet.nav.selected_island.clone();
    let action = route_key(st, key);
    if st.view == View::Fleet && st.fleet.nav.selected_island != before {
        reveal_selected_island(st);
    }
    action
}

/// The open reply panel overlays the bottom of the grid; a keyboard move
/// (grid arrows or the tree) onto an island under it collapses the panel so
/// the selection shows.
fn reveal_selected_island(st: &mut TuiState) {
    if !st.fleet.reply.is_open() {
        return;
    }
    let lay = super::layout::compute(st.layout.header.union(st.layout.footer), st);
    let Some(fl) = lay.fleet else { return };
    let (Some(panel), Some(selected)) = (fl.reply_panel, &st.fleet.nav.selected_island) else { return };
    if fl.grid.iter().any(|(k, r)| k == selected && r.bottom() > panel.y) {
        st.fleet.reply.collapse();
    }
}

fn route_key(st: &mut TuiState, key: KeyEvent) -> Action {
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
                (View::Fleet, Focus::Pane, Some(id)) => Action::Write { pane: id, bytes: vec![byte] },
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
    // Typing into the Board's filter ends with the Board, however it was left.
    if st.view != View::Board {
        st.kanban.editing_query = false;
    }
    if st.focus == Focus::Inspector {
        if st.inspector && st.layout.inspector.is_some() {
            return inspector_key(st, key);
        }
        st.focus = Focus::Pane;
    }
    let typing = (st.view == View::Board && st.kanban.editing_query)
        || (st.view == View::Brain && st.brain.editing_query)
        || (st.view == View::Fleet && st.fleet.prompt_editing)
        || (st.view == View::WorkerTypes && st.worker_types.editing.is_some())
        || (st.view == View::UserRequests && st.user_requests_draft.is_some());
    if st.view != View::Fleet && !typing {
        if let Some(v) = tab_key(&key) {
            return st.switch_view(v);
        }
    }
    match st.view {
        View::Board => return board_key(st, key),
        View::Remotes => return remotes_key(st, key),
        View::Plans => return plans_key(st, key),
        View::Brain => return brain_key(st, key),
        View::WorkerTypes => return worker_types_key(st, key),
        View::UserRequests => return super::user_requests::user_requests_key(st, key),
        View::Idle => return super::idle::idle_key(st, key),
        // A live pane attached within Fleet still owns bare keys (typed
        // input), same as old `View::Board` did.
        View::Fleet if st.focus == Focus::Pane => {}
        // Fleet-specific keys take priority; anything else (ordinary
        // session management) falls through to the ordinary bare-key path
        // below, same as it always has — see `fleet_key`'s doc comment.
        View::Fleet => {
            if let Some(action) = fleet_key(st, key) {
                return action;
            }
        }
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
        Focus::Inspector => inspector_key(st, key),
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

/// The new offset for a scroll key over `0..=max`, or `None` if `code`
/// isn't one.
fn scrolled(cur: u16, code: KeyCode, max: u16, page: u16) -> Option<u16> {
    Some(match code {
        KeyCode::Char('j') | KeyCode::Down => cur.saturating_add(1),
        KeyCode::Char('k') | KeyCode::Up => cur.saturating_sub(1),
        KeyCode::PageDown | KeyCode::Char(' ') => cur.saturating_add(page),
        KeyCode::PageUp => cur.saturating_sub(page),
        KeyCode::Char('g') | KeyCode::Home => 0,
        KeyCode::Char('G') | KeyCode::End => max,
        _ => return None,
    }
    .min(max))
}

/// The focused inspector: ←/→ (h/l) switch Info/Diff; Info scrolls; Diff
/// walks its file list, ↵ opens a file's diff to scroll, `r` refetches.
/// Esc peels back one layer (open file → list → the pane). Anything else,
/// and any Ctrl/Alt chord (Ctrl+r restarts), is an ordinary command.
fn inspector_key(st: &mut TuiState, key: KeyEvent) -> Action {
    use super::inspector::Tab;
    if key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
        return command(st, key);
    }
    let max = st.layout.inspector_max_scroll;
    let page = (st.layout.inspector.map_or(20, |r| r.height) / 2).max(1);
    let file_open = st.inspector_tab == Tab::Diff && st.diff.open.is_some();
    match key.code {
        KeyCode::Esc | KeyCode::Backspace | KeyCode::Char('h') | KeyCode::Left if file_open => {
            st.diff.open = None;
            st.diff.scroll = 0;
        }
        KeyCode::Esc => st.focus = Focus::Pane,
        KeyCode::Char('h') | KeyCode::Left => st.inspector_tab = Tab::Info,
        KeyCode::Char('l') | KeyCode::Right => st.inspector_tab = Tab::Diff,
        // Never resume here: in the inspector `r` means refetch, and the
        // Info tab has nothing to refetch.
        KeyCode::Char('r') => {
            if st.inspector_tab == Tab::Diff {
                st.diff.fetched_ms = i64::MIN / 2;
            }
        }
        code if st.inspector_tab == Tab::Info && scrolled(0, code, 0, 0).is_some() => {
            st.inspector_scroll = scrolled(st.inspector_scroll, code, max, page).unwrap_or_default();
        }
        code if file_open && scrolled(0, code, 0, 0).is_some() => {
            st.diff.scroll = scrolled(st.diff.scroll, code, max, page).unwrap_or_default();
        }
        KeyCode::Enter if st.inspector_tab == Tab::Diff && !st.diff.files.is_empty() => {
            st.diff.open = Some(st.diff.selected);
            st.diff.scroll = 0;
        }
        code if st.inspector_tab == Tab::Diff && scrolled(0, code, 0, 0).is_some() => {
            let last = st.diff.files.len().saturating_sub(1) as u16;
            st.diff.selected = scrolled(st.diff.selected as u16, code, last, page).unwrap_or_default() as usize;
        }
        _ => return command(st, key),
    }
    Action::None
}

/// Keys on a focused report: nothing behind it takes input, so they are
/// ninox commands (bare), with j/k scrolling the report.
fn report_key(st: &mut TuiState, key: KeyEvent) -> Action {
    let max = st.layout.report_max_scroll;
    match key.code {
        KeyCode::Esc | KeyCode::Char('h') | KeyCode::Left => st.focus = Focus::Sidebar,
        code if scrolled(0, code, 0, 0).is_some() => {
            st.report_scroll = scrolled(st.report_scroll, code, max, st.page() as u16 / 2).unwrap_or_default();
        }
        KeyCode::Enter => {}
        _ if key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {}
        _ => return command(st, key),
    }
    Action::None
}

/// Prefix commands; also what bare keys mean while the sidebar has focus.
fn command(st: &mut TuiState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Char('d') | KeyCode::Char('q') => return Action::Quit,
        KeyCode::Char('h') | KeyCode::Left => {
            st.view = View::Fleet;
            st.focus = Focus::Sidebar;
            st.zoom = false;
        }
        KeyCode::Char('l') | KeyCode::Right => {
            st.view = View::Fleet;
            if st.selected_row().is_some() {
                st.focus = Focus::Pane;
            }
        }
        KeyCode::Char('j') | KeyCode::Down => st.step_selection(1),
        KeyCode::Char('k') | KeyCode::Up => st.step_selection(-1),
        KeyCode::Char('!') => {
            if st.select_most_urgent() {
                st.view = View::Fleet;
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
            st.view = View::Fleet;
            st.focus = match st.focus {
                Focus::Sidebar if st.selected_row().is_some() => Focus::Pane,
                Focus::Pane if st.inspector && st.layout.inspector.is_some() => Focus::Inspector,
                _ => Focus::Sidebar,
            };
        }
        KeyCode::Char('z') => {
            st.view = View::Fleet;
            st.zoom = !st.zoom;
            if st.zoom && st.selected_row().is_some() {
                st.focus = Focus::Pane;
            }
        }
        KeyCode::Char('g') => st.modal = Some(Modal::Goto(Goto::default())),
        KeyCode::Char('n') => {
            st.modal = Some(Modal::Spawn);
            return Action::LoadSpawnForm;
        }
        KeyCode::Char('x') => st.ask_remove_or_kill(),
        KeyCode::Char('D') => st.ask_delete(),
        KeyCode::Char(c @ ('r' | 'R'))
            if key.modifiers.contains(KeyModifiers::CONTROL) && (c == 'R' || key.modifiers.contains(KeyModifiers::SHIFT)) =>
        {
            if st.live_count() == 0 {
                st.notify(Level::Info, "no live agents to restart");
            } else {
                st.modal = Some(Modal::Confirm(Pending::RestartAll));
            }
        }
        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => match st.selected_row() {
            Some(row) if row_can_restart(row) => st.modal = Some(Modal::Confirm(Pending::Restart(row.id().to_string()))),
            Some(row) => {
                let name = row.session.name.clone();
                st.notify(Level::Info, format!("{name} can't be restarted: no workspace or conversation id recorded"));
            }
            None => {}
        },
        KeyCode::Char('F') => match st.selected_row() {
            Some(row) if row.session.workspace_path.is_some() => {
                st.modal = Some(Modal::Confirm(Pending::Refile(row.id().to_string())));
            }
            Some(_) => st.notify(Level::Info, "no recorded workspace to re-file into"),
            None => {}
        },
        KeyCode::Char('r') => return st.resume_selected(),
        KeyCode::Char('O') => return st.open_selected_pr(),
        KeyCode::Char('N') => st.modal = Some(Modal::Notifications(0)),
        KeyCode::Char('t') if !key.modifiers.contains(KeyModifiers::CONTROL) => match st.selected_row() {
            Some(row) => {
                let (id, name) = (row.id().to_string(), row.session.name.clone());
                st.modal = Some(Modal::Message(MessageComposer { id, name, draft: String::new() }));
            }
            None => st.notify(Level::Info, "select a session to message"),
        },
        KeyCode::Char('R') => match st.selected_row().and_then(|r| r.group.clone()) {
            Some(orch) => st.modal = Some(Modal::Confirm(Pending::Reap(orch))),
            None => st.notify(Level::Info, "no orchestrator owns this row — nothing to reap"),
        },
        KeyCode::Char('?') => st.modal = Some(Modal::Help),
        // On the Fleet tab's graph/tree side, `[` collapses the tree to a
        // rail (spec: "Collapsible tree"). Gated on `Focus::Pane` rather
        // than `focused_pane().is_none()` — that check is `Some` for any
        // ptyd-backed row whether or not it's attached, so in a real fleet
        // it was almost never `None` and `[` almost never collapsed (final
        // review Important #2). The older "jump straight into scroll mode"
        // meaning now applies only once the pane itself already has focus
        // (see the arm below) or on a non-Fleet view.
        KeyCode::Char('[') if st.view == View::Fleet && st.focus == Focus::Sidebar => {
            st.tree_collapsed = !st.tree_collapsed;
            if st.tree_collapsed {
                // The tree just vanished out from under whatever had focus.
                if st.fleet.main_focus == FleetFocus::Tree {
                    st.fleet.main_focus = FleetFocus::Main;
                }
            } else {
                // "'[' again restores the tree, with focus returning to it."
                st.fleet.main_focus = FleetFocus::Tree;
            }
        }
        KeyCode::Char('[') => match st.focused_pane() {
            Some(_) => {
                st.view = View::Fleet;
                st.focus = Focus::Pane;
                st.mode = Mode::Scroll;
                st.scroll_by_wheel = false;
            }
            None => st.notify(Level::Info, "scroll mode needs a live pane"),
        },
        KeyCode::Char('i') => {
            st.view = View::Fleet;
            st.inspector = !st.inspector;
            if !st.inspector && st.focus == Focus::Inspector {
                st.focus = Focus::Pane;
            }
        }
        KeyCode::Char('u') => return st.switch_view(View::UserRequests),
        KeyCode::Char('b') => {
            st.view = View::Brain;
            return Action::LoadBrain(st.brain.query.clone());
        }
        KeyCode::Char('s') => return open_settings(st),
        KeyCode::Char('w') => {
            st.view = View::WorkerTypes;
            return Action::LoadWorkerTypes;
        }
        KeyCode::Char('c') | KeyCode::Enter => {
            return st.open_selected();
        }
        KeyCode::Char('a') => {
            if let Some(id) = st.selected_id() {
                return Action::Connect(id);
            }
        }
        KeyCode::Char('e') => match st.selected_row() {
            Some(row) if row.session.workspace_path.is_some() => return Action::OpenInEditor(row.id().to_string()),
            Some(_) => st.notify(Level::Info, "no recorded workspace to open in an editor"),
            None => {}
        },
        KeyCode::Char('1'..='7') => {
            if let Some(v) = tab_key(&key) {
                return st.switch_view(v);
            }
        }
        _ => {}
    }
    Action::None
}

/// `h`/`l` move between non-empty kanban columns (`j`/`k` step rows); Enter
/// or `c` attaches the selected card's session, the same as Fleet's `c`.
fn board_key(st: &mut TuiState, key: KeyEvent) -> Action {
    if st.kanban.editing_query {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc | KeyCode::Enter => st.kanban.editing_query = false,
            KeyCode::Backspace => {
                st.kanban.query.pop();
            }
            KeyCode::Char('u') if ctrl => st.kanban.query.clear(),
            KeyCode::Char(c) if !ctrl => st.kanban.query.push(c),
            _ => {}
        }
        st.kanban.row = 0;
        return Action::None;
    }
    let mut populated = [false; 6];
    for (i, col) in super::kanban::COLUMNS.iter().enumerate() {
        populated[i] = !super::kanban::column_rows(&st.rows, col.status.clone(), &st.kanban).is_empty();
    }
    let rows_in_col = super::kanban::column_rows(&st.rows, super::kanban::COLUMNS[st.kanban.col].status.clone(), &st.kanban).len();
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') if !st.kanban.query.is_empty() => {
            st.kanban.query.clear();
            st.kanban.row = 0;
        }
        KeyCode::Esc | KeyCode::Char('q') => st.view = View::Fleet,
        KeyCode::Char('/') => st.kanban.editing_query = true,
        KeyCode::Char('o') => {
            let scopes = super::kanban::scopes(&st.rows);
            st.kanban.cycle_scope(&scopes);
        }
        KeyCode::Char('A') => {
            if ninox_overview::board::resume_all_targets(st.rows.iter().map(|r| &r.session)).is_empty() {
                st.notify(Level::Info, "nothing interrupted to resume");
            } else {
                st.modal = Some(Modal::Confirm(Pending::ResumeAll));
            }
        }
        KeyCode::Char('h') | KeyCode::Left => st.kanban.move_col_among(-1, &populated),
        KeyCode::Char('l') | KeyCode::Right => st.kanban.move_col_among(1, &populated),
        KeyCode::Char('j') | KeyCode::Down => st.kanban.move_row(1, rows_in_col),
        KeyCode::Char('k') | KeyCode::Up => st.kanban.move_row(-1, rows_in_col),
        KeyCode::Enter | KeyCode::Char('c') => {
            let status = super::kanban::COLUMNS[st.kanban.col].status.clone();
            let id = super::kanban::column_rows(&st.rows, status, &st.kanban).get(st.kanban.row).map(|r| r.id().to_string());
            if let Some(id) = id {
                if let Some(idx) = st.rows.iter().position(|r| r.id() == id) {
                    st.selected = idx;
                    return st.open_selected();
                }
            }
        }
        _ => {}
    }
    Action::None
}

/// `j`/`k` move through the configured machines; `a` opens the add-remote
/// modal and `d` confirms removing the selected one — both drive the same
/// `ninox machine add`/`remove` flow the CLI uses (see `machine.rs`).
fn remotes_key(st: &mut TuiState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') if st.workspaces.open => {
            st.workspaces.open = false;
            st.workspaces.preview = false;
        }
        KeyCode::Esc | KeyCode::Char('q') => st.view = View::Fleet,
        KeyCode::Char('W') => {
            st.workspaces.open = !st.workspaces.open;
            st.workspaces.selected = 0;
            st.workspaces.preview = false;
        }
        KeyCode::Char('p') if st.workspaces.open => {
            st.workspaces.preview = !st.workspaces.preview;
        }
        KeyCode::Char('j') | KeyCode::Down if st.workspaces.open => {
            st.workspaces.move_sel(1, st.workspace_rows.len());
        }
        KeyCode::Char('k') | KeyCode::Up if st.workspaces.open => {
            st.workspaces.move_sel(-1, st.workspace_rows.len());
        }
        KeyCode::Char('j') | KeyCode::Down => st.remotes.move_sel(1, st.remote_rows.len()),
        KeyCode::Char('k') | KeyCode::Up => st.remotes.move_sel(-1, st.remote_rows.len()),
        KeyCode::Char('a') => st.modal = Some(Modal::AddRemote(AddRemoteModal::default())),
        KeyCode::Char('d') => {
            if let Some(row) = st.remote_rows.get(st.remotes.selected) {
                st.modal = Some(Modal::Confirm(Pending::RemoveMachine(row.id.clone())));
            }
        }
        _ => {}
    }
    Action::None
}

/// Empty-state tab — no content to navigate, but it must still own its own
/// bare keys so a pane attached elsewhere never receives them (final
/// review Critical #1: `View::Plans` previously fell through to
/// `pane_key`, typing into a backgrounded agent with nothing on screen to
/// show it).
fn plans_key(st: &mut TuiState, key: KeyEvent) -> Action {
    let last = st.ghost_plans.len().saturating_sub(1);
    let at = st.plans_sel_index();
    let selected = st.ghost_plans.get(at).filter(|p| p.plan.status == ninox_core::ghost_plan::PlanStatus::Pending).map(|p| p.plan.id.clone());
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => st.view = View::Fleet,
        KeyCode::Char('j') | KeyCode::Down => st.plans_sel = st.ghost_plans.get((at + 1).min(last)).map(|p| p.plan.id.clone()),
        KeyCode::Char('k') | KeyCode::Up => st.plans_sel = st.ghost_plans.get(at.saturating_sub(1)).map(|p| p.plan.id.clone()),
        KeyCode::Char('a') => {
            if let Some(id) = selected {
                st.modal = Some(Modal::Confirm(Pending::GhostAccept(id)));
            }
        }
        KeyCode::Char('x') | KeyCode::Char('s') => {
            if let Some(id) = selected {
                st.modal = Some(Modal::Confirm(Pending::GhostDiscard(id)));
            }
        }
        _ => {}
    }
    Action::None
}

/// Fleet-tab-specific keys only — island open/close, sort/density/fold
/// toggles, the ninox prompt. `None` means "not one of these," so the
/// caller falls through to the same bare-key session management
/// (`command()`, reached via the `Focus::Sidebar` path right below this in
/// `handle_key`) that the tree always has — the tree, not the islands grid,
/// is the primary navigation surface; `TuiState::select()`'s
/// `sync_fleet_selection` keeps `fleet.nav.selected` (and so the detail
/// panel) following it. `Enter` is the one key that acts on the tree
/// selection itself, opening its owning island and moving focus to the
/// centre (mirrors the brief's "click an orchestrator in the tree" /
/// "clicking a worker in the tree opens its orchestrator with that worker
/// selected" — both are exactly `FleetNav::open_node`).
/// Rows the grid scrolls per arrow-key step off its edge — arbitrary, bump
/// if a real fleet makes it feel too slow or too jumpy.
const GRID_SCROLL_STEP: u16 = 4;

/// `Tab`/`Shift-Tab`'s cycle order: tree → main → detail (only while the
/// detail panel is actually showing — an island open with a node
/// selected), wrapping in either direction. A `main_focus` not in `order`
/// (e.g. `Detail` after the panel it was reached through just closed) lands
/// back on `Tree` rather than panicking on a missing position.
fn next_fleet_focus(current: FleetFocus, forward: bool, has_detail: bool) -> FleetFocus {
    use FleetFocus::{Detail, Main, Tree};
    let order: &[FleetFocus] = if has_detail { &[Tree, Main, Detail] } else { &[Tree, Main] };
    let idx = order.iter().position(|&f| f == current).unwrap_or(0) as isize;
    let len = order.len() as isize;
    let next = (idx + if forward { 1 } else { -1 }).rem_euclid(len);
    order[next as usize]
}

fn fleet_key(st: &mut TuiState, key: KeyEvent) -> Option<Action> {
    if st.fleet.prompt_editing {
        return Some(fleet_prompt_key(st, key));
    }
    if let Some(action) = reply_panel_key(st, key) {
        return Some(action);
    }
    match key.code {
        KeyCode::Tab | KeyCode::BackTab => {
            let has_detail = st.fleet.nav.open.is_some() && st.fleet.nav.selected.is_some();
            st.fleet.main_focus = next_fleet_focus(st.fleet.main_focus, key.code == KeyCode::Tab, has_detail);
            Some(Action::None)
        }
        // Esc's two stages (nav.rs doc comment): first clears the node
        // selection (closes the detail panel, keeps the island open), a
        // second Esc closes the island back to the Fleet grid (with it
        // still selected there). Either stage can drop the detail panel
        // `main_focus` was on, so fall back to `Main` rather than leaving
        // focus parked on a panel no longer showing.
        KeyCode::Esc if st.fleet.nav.open.is_some() => {
            if st.fleet.nav.selected.is_some() {
                st.fleet.nav.clear_selection();
            } else {
                st.fleet.nav.close_island();
            }
            if st.fleet.main_focus == FleetFocus::Detail {
                st.fleet.main_focus = FleetFocus::Main;
            }
            Some(Action::None)
        }
        // "Open pane" on the reply panel — connects to a picked
        // orchestrator only; the prompt-agent singleton is never navigable
        // from here.
        KeyCode::Char('o') if st.fleet.reply.drawer.has_content() && st.fleet.reply.orchestrator_id().is_some() => {
            st.fleet.reply.orchestrator_id().map(|id| Action::Connect(id.to_string()))
        }
        KeyCode::Char('t') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            cycle_prompt_target(st);
            Some(Action::None)
        }
        // The design footer's "←→ tree/graph": → moves off the tree into
        // the centre pane; ← back the other way only happens from the
        // centre once it's at its own left edge (`fleet_main_arrow`), not
        // here — the tree is already the leftmost pane.
        KeyCode::Right if st.fleet.main_focus == FleetFocus::Tree => {
            st.fleet.main_focus = FleetFocus::Main;
            Some(Action::None)
        }
        KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right if st.fleet.main_focus == FleetFocus::Main => {
            fleet_main_arrow(st, key.code);
            Some(Action::None)
        }
        KeyCode::Char('m') => {
            st.fleet.compact = !st.fleet.compact;
            Some(Action::None)
        }
        KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            st.fleet.toggle_fold_all();
            Some(Action::None)
        }
        KeyCode::Char('f') => {
            match st.focused_island() {
                Some(k) => st.fleet.toggle_island_fold(&k),
                None => st.notify(Level::Info, "select an island to fold — ctrl+f folds them all"),
            }
            Some(Action::None)
        }
        KeyCode::Char('M') => {
            if st.fleet.machines.is_empty() {
                st.notify(Level::Info, "every island is on this machine");
            } else {
                st.fleet.cycle_machine_filter();
            }
            Some(Action::None)
        }
        KeyCode::Char('s') => {
            st.fleet.sort = super::fleet::cycle_sort(st.fleet.sort);
            Some(Action::None)
        }
        KeyCode::Char('/') => {
            st.fleet.prompt_editing = true;
            st.fleet.reply.focused = false;
            st.fleet.reply.reopen();
            st.fleet.prompt_target = PromptTarget::Agent;
            st.fleet.prompt_history_pos = None;
            Some(Action::None)
        }
        KeyCode::Enter => {
            // With the grid focused and an island highlighted (but not open
            // yet), Enter opens that highlighted island rather than
            // whatever the tree cursor happens to sit on. Either path moves
            // focus to the centre, so an Enter from the tree lands the user
            // straight on the island it just opened.
            if st.fleet.main_focus == FleetFocus::Main && st.fleet.nav.open.is_none() {
                if let Some(key) = st.fleet.nav.selected_island.clone() {
                    st.fleet.nav.open_island(key, &st.fleet.islands);
                    st.fleet.main_focus = FleetFocus::Main;
                    st.sync_tree_cursor_to_nav_selection();
                    return Some(Action::None);
                }
            }
            let id = st.selected_id()?;
            st.fleet.nav.open_node(id, &st.fleet.islands);
            st.fleet.main_focus = FleetFocus::Main;
            Some(Action::None)
        }
        _ => None,
    }
}

/// Arrow keys while `main_focus == Main`: spatial navigation, either among
/// the islands grid's boxes (`fleet::nearest_island_in_direction`, scrolling
/// the grid when a direction has no visible candidate) or — once an island
/// is open — among its own rataflow card tree (`FlowAction::SelectUp/Down/
/// Left/Right`, rataflow's own spatial navigation, reused as-is). Either
/// way, `Left` with nothing further left to land on crosses back to the
/// tree (the design footer's "←→ tree/graph" — the centre's own left edge
/// is the other half of that crossing, `Right` from the tree being the
/// first half in `fleet_key`).
fn fleet_main_arrow(st: &mut TuiState, code: KeyCode) {
    if st.fleet.nav.open.is_some() {
        let action = match code {
            KeyCode::Up => rataflow::FlowAction::SelectUp,
            KeyCode::Down => rataflow::FlowAction::SelectDown,
            KeyCode::Left => rataflow::FlowAction::SelectLeft,
            KeyCode::Right => rataflow::FlowAction::SelectRight,
            _ => return,
        };
        let before = st.fleet.opened_flow.borrow().as_ref().and_then(|f| f.first_selected_node_id());
        let selected = {
            let mut flow = st.fleet.opened_flow.borrow_mut();
            let Some(flow) = flow.as_mut() else { return };
            flow.apply(action);
            flow.first_selected_node_id()
        };
        if code == KeyCode::Left && selected == before {
            st.fleet.main_focus = FleetFocus::Tree;
            return;
        }
        if let Some(id) = selected {
            st.fleet.nav.select(id);
            st.sync_tree_cursor_to_nav_selection();
        }
        return;
    }
    let direction = match code {
        KeyCode::Up => GridDirection::Up,
        KeyCode::Down => GridDirection::Down,
        KeyCode::Left => GridDirection::Left,
        KeyCode::Right => GridDirection::Right,
        _ => return,
    };
    let Some(fl) = &st.layout.fleet else { return };
    let boxes: Vec<(ninox_overview::islands::IslandKey, f32, f32)> =
        fl.grid.iter().map(|(k, r)| (k.clone(), r.x as f32 + r.width as f32 / 2.0, r.y as f32 + r.height as f32 / 2.0)).collect();
    let current = st.fleet.nav.selected_island.clone().or_else(|| boxes.first().map(|(k, ..)| k.clone()));
    let Some(current) = current else { return };
    if !boxes.iter().any(|(k, ..)| k == &current) {
        // The current selection isn't in the visible set (e.g. scrolled
        // out, or nothing selected yet) — land on the first visible box
        // rather than failing to move at all.
        if let Some((k, ..)) = boxes.first() {
            st.fleet.nav.selected_island = Some(k.clone());
        }
        return;
    }
    match super::fleet::nearest_island_in_direction(&boxes, &current, direction) {
        Some(next) => st.fleet.nav.selected_island = Some(next),
        None => match direction {
            GridDirection::Down => st.fleet.grid_scroll = st.fleet.grid_scroll.saturating_add(GRID_SCROLL_STEP),
            GridDirection::Up => st.fleet.grid_scroll = st.fleet.grid_scroll.saturating_sub(GRID_SCROLL_STEP),
            GridDirection::Left => st.fleet.main_focus = FleetFocus::Tree,
            GridDirection::Right => {}
        },
    }
}

/// Translates a crossterm mouse button/kind into rataflow's own abstract
/// types by hand, rather than leaning on its `From<crossterm::...>` impls —
/// `ninox-tui` depends on the plain `crossterm` crate directly while
/// rataflow converts from `ratatui`'s re-exported one; nothing guarantees
/// cargo resolves both to the exact same crate instance.
fn to_rataflow_button(b: MouseButton) -> rataflow::MouseButton {
    match b {
        MouseButton::Left => rataflow::MouseButton::Left,
        MouseButton::Right => rataflow::MouseButton::Right,
        MouseButton::Middle => rataflow::MouseButton::Middle,
    }
}

fn to_rataflow_mouse(ev: &MouseEvent, column: u16, row: u16) -> Option<rataflow::MouseEvent> {
    let kind = match ev.kind {
        MouseEventKind::Down(b) => rataflow::MouseEventKind::Down(to_rataflow_button(b)),
        MouseEventKind::Up(b) => rataflow::MouseEventKind::Up(to_rataflow_button(b)),
        MouseEventKind::Drag(b) => rataflow::MouseEventKind::Drag(to_rataflow_button(b)),
        MouseEventKind::Moved => rataflow::MouseEventKind::Moved,
        _ => return None, // scroll is handled separately (pan, never zoom) — see `fleet_mouse`.
    };
    Some(rataflow::MouseEvent::new(kind, column, row))
}

/// Mouse handling for the Fleet tab's islands grid and opened-island card
/// tree (`Focus::Sidebar` only — a live attached pane keeps its own mouse
/// handling, further down in `handle_mouse`). A click selects (and focuses
/// `Main`); a double-click on the already-selected grid box opens it;
/// inside an opened island, clicks/drags forward to the rataflow `Flow`
/// itself (it already does its own hit-testing and emits `NodeClicked`);
/// the wheel always scrolls/pans, never zooms — cards must stay readable at
/// a fixed 1.0 zoom (see `island_flow::build_flow`).
fn fleet_mouse(st: &mut TuiState, ev: &MouseEvent, lay: &Layout, wheel: Option<i64>, double: bool) -> Action {
    let (x, y) = (ev.column, ev.row);
    let Some(fl) = lay.fleet.clone() else { return Action::None };

    // The open reply panel sits over the grid: it takes its own wheel and
    // swallows clicks rather than letting them reach a hidden island.
    if fl.reply_panel.is_some_and(|r| contains(r, x, y)) {
        if let Some(d) = wheel {
            st.fleet.reply.scroll_by(d as i32);
        }
        return Action::None;
    }

    if st.fleet.nav.open.is_some() {
        if !contains(fl.main, x, y) {
            return Action::None;
        }
        if let Some(d) = wheel {
            if let Some(flow) = st.fleet.opened_flow.borrow_mut().as_mut() {
                flow.pan(0.0, -(d as f64));
            }
            return Action::None;
        }
        let Some(local) = to_rataflow_mouse(ev, x - fl.main.x, y - fl.main.y) else { return Action::None };
        let clicked = {
            let mut flow = st.fleet.opened_flow.borrow_mut();
            let Some(flow) = flow.as_mut() else { return Action::None };
            flow.handle_mouse_event(local)
                .into_events()
                .find_map(|e| if let rataflow::FlowEvent::NodeClicked { node_id } = e { Some(node_id) } else { None })
        };
        if let Some(id) = clicked {
            st.fleet.nav.select(id);
            st.fleet.main_focus = FleetFocus::Main;
            st.sync_tree_cursor_to_nav_selection();
        }
        return Action::None;
    }

    if !contains(fl.main, x, y) {
        return Action::None;
    }
    if let Some(d) = wheel {
        if d < 0 {
            st.fleet.grid_scroll = st.fleet.grid_scroll.saturating_add(GRID_SCROLL_STEP);
        } else {
            st.fleet.grid_scroll = st.fleet.grid_scroll.saturating_sub(GRID_SCROLL_STEP);
        }
        return Action::None;
    }
    if matches!(ev.kind, MouseEventKind::Down(MouseButton::Left)) {
        if let Some((key, _)) = fl.grid.iter().find(|(_, r)| contains(*r, x, y)) {
            let same = st.fleet.nav.selected_island.as_ref() == Some(key);
            st.fleet.nav.selected_island = Some(key.clone());
            st.fleet.main_focus = FleetFocus::Main;
            if double && same {
                st.fleet.nav.open_island(key.clone(), &st.fleet.islands);
                st.sync_tree_cursor_to_nav_selection();
            }
        }
    }
    Action::None
}

/// The reply panel's own keys, ahead of the grid's: while it's focused,
/// j/k/PgUp/PgDn/g/G scroll it and esc collapses it; while it's open but
/// unfocused, esc still collapses it before closing an island (one layer
/// peels at a time). Returns `None` for keys it leaves to the grid.
fn reply_panel_key(st: &mut TuiState, key: KeyEvent) -> Option<Action> {
    let reply = &mut st.fleet.reply;
    if !reply.is_open() {
        reply.focused = false;
        return None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let page = reply_page(&st.layout);
    match key.code {
        KeyCode::Esc => reply.collapse(),
        KeyCode::PageUp => reply.scroll_by(page),
        KeyCode::PageDown => reply.scroll_by(-page),
        _ if !reply.focused => return None,
        KeyCode::Char('k') | KeyCode::Up => reply.scroll_by(1),
        KeyCode::Char('j') | KeyCode::Down => reply.scroll_by(-1),
        KeyCode::Char('g') => reply.scroll_to_top(),
        KeyCode::Char('G') => reply.scroll_to_bottom(),
        KeyCode::Char('l') if ctrl => return Some(Action::ClearPromptContext),
        KeyCode::Tab | KeyCode::BackTab => {
            reply.focused = false;
            return None;
        }
        _ => return None,
    }
    Some(Action::None)
}

/// A page of the reply panel: its height less the border, header and a
/// row of overlap.
fn reply_page(layout: &super::layout::Layout) -> i32 {
    layout.fleet.as_ref().and_then(|f| f.reply_panel).map_or(8, |r| r.height.saturating_sub(4).max(1) as i32)
}

fn fleet_prompt_key(st: &mut TuiState, key: KeyEvent) -> Action {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Esc => {
            st.fleet.prompt_editing = false;
            st.fleet.prompt_input.clear();
            st.fleet.prompt_history_pos = None;
            st.fleet.reply.focused = st.fleet.reply.is_open();
            Action::None
        }
        KeyCode::PageUp | KeyCode::PageDown => {
            let page = reply_page(&st.layout);
            st.fleet.reply.scroll_by(if key.code == KeyCode::PageUp { page } else { -page });
            Action::None
        }
        KeyCode::Char('t') if ctrl => {
            cycle_prompt_target(st);
            Action::None
        }
        KeyCode::Char('l') if ctrl => Action::ClearPromptContext,
        KeyCode::Up => {
            recall_older_prompt(st);
            Action::None
        }
        KeyCode::Down => {
            recall_newer_prompt(st);
            Action::None
        }
        KeyCode::Enter => {
            st.fleet.prompt_editing = false;
            let text = std::mem::take(&mut st.fleet.prompt_input).trim().to_string();
            st.fleet.prompt_history_pos = None;
            if text.is_empty() {
                return Action::None;
            }
            st.fleet.prompt_history.push(text.clone());
            let message = match st.fleet.nav.open.as_ref().and_then(|k| st.fleet.islands.iter().find(|i| &i.key == k)) {
                Some(island) => format!("[scope: {}] {text}", island.name),
                None => text.clone(),
            };
            Action::SubmitPrompt { target: st.fleet.prompt_target.clone(), question: text, message }
        }
        KeyCode::Backspace => {
            st.fleet.prompt_input.pop();
            Action::None
        }
        KeyCode::Char(c) if !ctrl => {
            st.fleet.prompt_input.push(c);
            Action::None
        }
        _ => Action::None,
    }
}

/// `ctrl+t`: the prompt agent, then each orchestrator (in row order), then
/// back to the agent. A picked orchestrator that's vanished by the next
/// cycle (removed/ended) is treated as not found, same as cycling past it.
fn cycle_prompt_target(st: &mut TuiState) {
    let orchestrators: Vec<String> = st.rows.iter().filter(|r| r.is_orchestrator).map(|r| r.id().to_string()).collect();
    st.fleet.prompt_target = match &st.fleet.prompt_target {
        PromptTarget::Agent => orchestrators.into_iter().next().map_or(PromptTarget::Agent, PromptTarget::Orchestrator),
        PromptTarget::Orchestrator(current) => {
            let next = orchestrators.iter().position(|id| id == current).and_then(|i| orchestrators.get(i + 1).cloned());
            next.map_or(PromptTarget::Agent, PromptTarget::Orchestrator)
        }
    };
}

/// Display name for the prompt's current target, for the chip and the
/// strip's headline.
pub(crate) fn prompt_target_label(st: &TuiState) -> String {
    match &st.fleet.prompt_target {
        PromptTarget::Agent => "ninox".to_string(),
        PromptTarget::Orchestrator(id) => st.rows.iter().find(|r| r.id() == id).map(|r| r.session.name.clone()).unwrap_or_else(|| id.clone()),
    }
}

fn recall_older_prompt(st: &mut TuiState) {
    if st.fleet.prompt_history.is_empty() {
        return;
    }
    let pos = st.fleet.prompt_history_pos.unwrap_or(st.fleet.prompt_history.len()).saturating_sub(1);
    st.fleet.prompt_history_pos = Some(pos);
    st.fleet.prompt_input = st.fleet.prompt_history[pos].clone();
}

fn recall_newer_prompt(st: &mut TuiState) {
    match st.fleet.prompt_history_pos {
        Some(pos) if pos + 1 < st.fleet.prompt_history.len() => {
            st.fleet.prompt_history_pos = Some(pos + 1);
            st.fleet.prompt_input = st.fleet.prompt_history[pos + 1].clone();
        }
        Some(_) => {
            st.fleet.prompt_history_pos = None;
            st.fleet.prompt_input.clear();
        }
        None => {}
    }
}


/// A `PrWatch` (`ninox open --pr`) with no matching session — see
/// `TuiState::orphan_watch_count`'s doc comment for why this is tracked.
pub(crate) fn orphan_watch_count(rows: &[Row], watches: &[PrWatch]) -> usize {
    watches
        .iter()
        .filter(|w| {
            !rows.iter().any(|r| {
                r.session.pr_number == Some(w.pr_number) && r.session.repo.eq_ignore_ascii_case(&w.repo)
            })
        })
        .count()
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
    if st.brain.sources.open {
        return brain_sources_key(st, key);
    }
    if !st.brain.editing_query && key.code == KeyCode::Char('S') {
        st.brain.sources.open = true;
        return Action::LoadBrainSources;
    }
    if !st.brain.editing_query && key.code == KeyCode::Tab {
        st.brain.toggle_graph_mode();
        return Action::None;
    }
    if st.brain.graph_mode {
        return brain_graph_key(st, key);
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
    let before_id = b.selected_entry().map(|e| e.id.clone());
    match key.code {
        // Link-focus mode (`L`) takes over j/k/Enter/Esc while active, on
        // top of everything below — checked first so it doesn't fall
        // through to the plain list/doc bindings those keys otherwise have.
        KeyCode::Char('j') | KeyCode::Down if b.link_focus.is_some() => b.move_link_focus(1),
        KeyCode::Char('k') | KeyCode::Up if b.link_focus.is_some() => b.move_link_focus(-1),
        KeyCode::Enter if b.link_focus.is_some() => b.jump_to_focused_link(),
        KeyCode::Esc | KeyCode::Char('h') if b.link_focus.is_some() => b.exit_link_focus(),
        KeyCode::Char('L') => b.enter_link_focus(),
        KeyCode::Esc | KeyCode::Char('q') if b.open => b.open = false,
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('b') => st.view = View::Fleet,
        KeyCode::Char('/') => {
            b.editing_query = true;
            b.open = false;
        }
        KeyCode::Char('e') => return brain_edit(st),
        KeyCode::Char('a') => return brain_new(st),
        KeyCode::Char('D') => brain_delete(st),
        KeyCode::Char('R') => return Action::ReindexBrain,
        KeyCode::Char('c') => {
            b.cycle_catalogue();
            return Action::CycleBrainCatalogue;
        }
        KeyCode::Char('t') => {
            b.cycle_grouping();
            b.open = false;
        }
        KeyCode::Char('[') => {
            b.cycle_kind_filter(false);
            b.open = false;
        }
        KeyCode::Char(']') => {
            b.cycle_kind_filter(true);
            b.open = false;
        }
        KeyCode::Char('s') => {
            b.cycle_sort();
            b.open = false;
        }
        KeyCode::Char('x') => {
            b.clear_filters();
            b.open = false;
        }
        KeyCode::Char('r') => {
            b.cycle_repo_filter(true);
            b.open = false;
        }
        KeyCode::Char('T') => {
            b.cycle_tag_filter(true);
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
    // Whatever just changed the selection — list navigation, a filter/sort
    // cycle narrowing it out, or a link jump — fetch that entry's
    // backlinks/related/outlinks fresh rather than leaving the previous
    // entry's stale.
    match st.brain.selected_entry().map(|e| e.id.clone()) {
        Some(id) if Some(&id) != before_id.as_ref() => Action::LoadBrainLinks(id),
        _ => Action::None,
    }
}

/// Keys while the Brain tab's Neighbourhood graph (`Tab`) is showing:
/// arrows walk the graph (rataflow's own spatial selection), `Enter`
/// re-centres on the selected entry, `o` opens it in the list, `Esc`
/// leaves graph mode. Acts on `graph_flow` as it stood after the last
/// render — the same "arrow keys move rataflow's own selection" pattern
/// `fleet_main_arrow` already uses for the opened island.
fn brain_graph_key(st: &mut TuiState, key: KeyEvent) -> Action {
    if key.code == KeyCode::Esc {
        st.brain.graph_mode = false;
        return Action::None;
    }
    if key.code == KeyCode::Char('s') {
        st.brain.cycle_graph_scope();
        return Action::None;
    }
    match st.brain.graph_scope {
        super::brain::GraphScope::Neighbourhood => brain_neighbourhood_key(st, key),
        super::brain::GraphScope::Clusters => brain_clusters_key(st, key),
    }
    Action::None
}

fn brain_neighbourhood_key(st: &mut TuiState, key: KeyEvent) {
    match key.code {
        KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right => {
            let action = match key.code {
                KeyCode::Up => rataflow::FlowAction::SelectUp,
                KeyCode::Down => rataflow::FlowAction::SelectDown,
                KeyCode::Left => rataflow::FlowAction::SelectLeft,
                KeyCode::Right => rataflow::FlowAction::SelectRight,
                _ => unreachable!(),
            };
            st.brain.apply_graph_action(action);
        }
        KeyCode::Enter => {
            if let Some(id) = st.brain.graph_selected_id() {
                st.brain.refocus_graph(&id);
            }
        }
        KeyCode::Char('o') => {
            if let Some(id) = st.brain.graph_selected_id() {
                if id != super::brain_flow::MORE_NODE_ID {
                    st.brain.graph_mode = false;
                    st.brain.select_entry(&id);
                }
            }
        }
        _ => {}
    }
}

/// Clusters scope has no rataflow canvas of its own — just a ranked table,
/// so `j`/`k`/arrows move a plain row cursor and `Enter` jumps straight to
/// the List view filtered to that repo (reusing the same `repo_filter` the
/// List's own `r` key sets), rather than trying to express "a graph scoped
/// to one repo" as a second kind of graph.
fn brain_clusters_key(st: &mut TuiState, key: KeyEvent) {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => st.brain.move_clusters_cursor(-1),
        KeyCode::Down | KeyCode::Char('j') => st.brain.move_clusters_cursor(1),
        KeyCode::Enter => {
            if let Some(repo) = st.brain.selected_cluster_repo() {
                st.brain.repo_filter = Some(repo);
                st.brain.graph_mode = false;
            }
        }
        _ => {}
    }
}

/// Keys while Brain › Sources (`S`) is showing. `esc` closes the detail
/// pane first, then the sub-view; `S` here syncs every source.
fn brain_sources_key(st: &mut TuiState, key: KeyEvent) -> Action {
    use super::sources::{is_builtin, SourceForm, BUILTIN_NOTE};
    let page = st.page() as i64;
    let v = &mut st.brain.sources;
    let selected = v.selected().cloned();
    let builtin = selected.as_ref().is_some_and(is_builtin);
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => v.move_cursor(1),
        KeyCode::Char('k') | KeyCode::Up => v.move_cursor(-1),
        KeyCode::PageDown => v.move_cursor(page),
        KeyCode::PageUp => v.move_cursor(-page),
        KeyCode::Char('g') | KeyCode::Home => v.move_cursor(i64::MIN / 2),
        KeyCode::Char('G') | KeyCode::End => v.move_cursor(i64::MAX / 2),
        KeyCode::Enter => v.detail = !v.detail,
        KeyCode::Esc if v.detail => v.detail = false,
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('b') => {
            v.open = false;
            v.detail = false;
        }
        KeyCode::Char('a') => st.modal = Some(Modal::SourceForm(SourceForm::add())),
        KeyCode::Char('s') => return selected.map_or(Action::None, |c| Action::SyncBrainSources(vec![c.name])),
        KeyCode::Char('S') => return Action::SyncBrainSources(v.catalogues.iter().map(|c| c.name.clone()).collect()),
        KeyCode::Char('e' | 't' | 'd') if builtin => st.notify(Level::Info, BUILTIN_NOTE.to_string()),
        KeyCode::Char('e') => {
            if let Some(c) = &selected {
                st.modal = Some(Modal::SourceForm(SourceForm::edit(c)));
            }
        }
        KeyCode::Char('t') => return selected.map_or(Action::None, |c| Action::ToggleBrainSource(c.name)),
        KeyCode::Char('d') => {
            if let Some(c) = selected {
                st.modal = Some(Modal::Confirm(Pending::RemoveBrainSource(c.name)));
            }
        }
        _ => {}
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
        Kind::Action if f.id == super::settings::FieldId::RestartServer => {
            st.modal = Some(Modal::Confirm(Pending::RestartServer));
            Action::None
        }
        Kind::Action if f.id == super::settings::FieldId::RestartAffected => {
            let ids = st.login_error_sessions();
            if ids.is_empty() {
                st.notify(Level::Info, "no session has a login error");
            } else {
                st.modal = Some(Modal::Confirm(Pending::RestartAffected(ids)));
            }
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
    st.settings.move_field(delta);
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
    if !st.settings.in_fields {
        match key.code {
            // Closing is decided by `handle_modal_key`; the page is a modal
            // overlay, so `st.view` stays whatever was showing underneath.
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('s') => {}
            KeyCode::Char('j') | KeyCode::Down => st.settings.move_section(1),
            KeyCode::Char('k') | KeyCode::Up => st.settings.move_section(-1),
            KeyCode::Char('g') | KeyCode::Home | KeyCode::PageUp => st.settings.move_section(i64::MIN / 2),
            KeyCode::Char('G') | KeyCode::End | KeyCode::PageDown => st.settings.move_section(i64::MAX / 2),
            KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Char('l') | KeyCode::Right | KeyCode::Tab => enter_setting_fields(st),
            KeyCode::Char('e') => return Action::EditConfig,
            KeyCode::Char('?') => st.modal = Some(Modal::Help),
            _ => {}
        }
        return Action::None;
    }
    match key.code {
        KeyCode::Esc | KeyCode::Tab | KeyCode::BackTab => st.settings.in_fields = false,
        KeyCode::Char('q') | KeyCode::Char('s') => {}
        KeyCode::Char('j') | KeyCode::Down => move_setting(st, 1),
        KeyCode::Char('k') | KeyCode::Up => move_setting(st, -1),
        KeyCode::PageDown => move_setting(st, 10),
        KeyCode::PageUp => move_setting(st, -10),
        KeyCode::Char('g') | KeyCode::Home => move_setting(st, i64::MIN / 2),
        KeyCode::Char('G') | KeyCode::End => move_setting(st, i64::MAX / 2),
        KeyCode::Enter | KeyCode::Char(' ') => return activate_setting(st),
        KeyCode::Char('l') | KeyCode::Right => return step_setting(st, false),
        // A field with nothing to cycle has no use for ←: it goes back to the bar.
        KeyCode::Char('h') | KeyCode::Left if st.settings.selected_field().is_some_and(|f| f.options.is_empty()) => st.settings.in_fields = false,
        KeyCode::Char('h') | KeyCode::Left => return step_setting(st, true),
        KeyCode::Char('e') => return Action::EditConfig,
        KeyCode::Char('?') => st.modal = Some(Modal::Help),
        _ => {}
    }
    Action::None
}

pub fn open_settings(st: &mut TuiState) -> Action {
    st.settings.in_fields = false;
    st.settings.editing = None;
    st.settings.error = None;
    st.modal = Some(Modal::Settings);
    Action::LoadSettings
}

fn enter_setting_fields(st: &mut TuiState) {
    st.settings.in_fields = !st.settings.section_fields().is_empty();
}

fn confirmed(pending: Pending) -> Action {
    match pending {
        Pending::Kill(id) => Action::Kill(id),
        Pending::Remove(id) => Action::Remove(id),
        Pending::Reap(id) => Action::Reap(id),
        Pending::DeleteBrain(id) => Action::DeleteBrain(id),
        Pending::DeleteWorkerType(name) => Action::DeleteWorkerType(name),
        Pending::ResetWorkerType(name) => Action::ResetWorkerType(name),
        Pending::RestartAll => Action::RestartAll,
        Pending::RestartServer => Action::RestartServer,
        Pending::RestartAffected(ids) => Action::RestartAffected(ids),
        Pending::ResumeAll => Action::ResumeAll,
        Pending::Restart(id) => Action::Resume(id),
        Pending::Refile(id) => Action::Refile(id),
        Pending::RemoveMachine(id) => Action::RemoveMachine(id),
        Pending::RemoveBrainSource(name) => Action::RemoveBrainSource(name),
        Pending::GhostAccept(id) => Action::GhostAccept(id),
        Pending::GhostDiscard(id) => Action::GhostDiscard(id),
    }
}

fn handle_modal_key(st: &mut TuiState, key: KeyEvent) -> Action {
    let Some(modal) = st.modal.take() else { return Action::None };
    match modal {
        Modal::Help => {
            let max = st.layout.help_max_scroll;
            let at = st.help_scroll.min(max);
            // Nothing to scroll: every key closes it, as it always has.
            match scrolled(at, key.code, max, st.layout.help_page.max(1)).filter(|_| max > 0) {
                Some(at) => {
                    st.help_scroll = at;
                    st.modal = Some(Modal::Help);
                }
                None => st.help_scroll = 0,
            }
            Action::None
        }
        Modal::Settings => {
            // Esc closes only from the section bar (from the fields it goes
            // back to the bar), and not mid-edit; `settings_key` doesn't
            // say whether to keep the modal open, so that's decided up front.
            let closing = st.settings.editing.is_none()
                && (matches!(key.code, KeyCode::Char('q') | KeyCode::Char('s')) || (key.code == KeyCode::Esc && !st.settings.in_fields));
            let action = settings_key(st, key);
            // Keep the page unless the key closed it or opened Help / a confirm over it.
            if !closing && st.modal.is_none() {
                st.modal = Some(Modal::Settings);
            }
            action
        }
        Modal::Delete(mut d) => {
            let ready = d.plan.as_ref().and_then(|p| p.as_ref().ok());
            match key.code {
                KeyCode::Char('n') | KeyCode::Esc => return Action::None,
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    if let Some(plan) = ready {
                        return Action::Delete { plan: plan.clone(), force: d.force };
                    }
                }
                KeyCode::Char('f') if ready.is_some_and(|p| !p.at_risk().is_empty()) => d.force = !d.force,
                KeyCode::Char('e') | KeyCode::Char(' ') => d.expanded = !d.expanded,
                _ => {}
            }
            st.modal = Some(Modal::Delete(d));
            Action::None
        }
        Modal::Confirm(pending) => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => confirmed(pending),
            KeyCode::Char('a') if st.confirm_all_action(&pending).is_some() => st.confirm_all_action(&pending).unwrap_or(Action::None),
            KeyCode::Char('n') | KeyCode::Esc => Action::None,
            _ => {
                st.modal = Some(Modal::Confirm(pending));
                Action::None
            }
        },
        Modal::Notifications(mut cur) => {
            let last = st.notifications.len().saturating_sub(1);
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('N') => return Action::None,
                KeyCode::Char('j') | KeyCode::Down => cur = (cur + 1).min(last),
                KeyCode::Char('k') | KeyCode::Up => cur = cur.saturating_sub(1),
                KeyCode::Char('d') => {
                    if let Some(id) = st.notification_at(cur).map(|n| n.id.clone()) {
                        st.notifications.retain(|n| n.id != id);
                        st.dismissed_ids.push(id);
                    }
                    cur = cur.min(st.notifications.len().saturating_sub(1));
                }
                KeyCode::Char('D') => {
                    st.notifications.clear();
                    st.dismissed_all = Some(st.notif_cursor.unwrap_or(0));
                    cur = 0;
                }
                KeyCode::Enter if st.notification_at(cur).is_some_and(|n| n.kind == ninox_core::types::NotificationKind::DiskPressure) => {
                    st.view = View::Remotes;
                    st.workspaces.open = true;
                    st.workspaces.selected = 0;
                    st.workspaces.preview = true;
                    return Action::None;
                }
                KeyCode::Enter => {
                    if let Some(id) = st.notification_at(cur).and_then(|n| n.session_id.clone()) {
                        match st.rows.iter().position(|r| r.id() == id) {
                            Some(idx) => {
                                st.view = View::Fleet;
                                st.selected = idx;
                                return st.open_selected();
                            }
                            None => st.notify(Level::Info, format!("{id} is no longer in the fleet")),
                        }
                    }
                }
                _ => {}
            }
            st.modal = Some(Modal::Notifications(cur));
            Action::None
        }
        Modal::Message(mut m) => {
            match key.code {
                KeyCode::Esc => return Action::None,
                KeyCode::Enter if !m.draft.trim().is_empty() => {
                    return Action::SendMessage { id: m.id, message: m.draft.trim().to_string() };
                }
                KeyCode::Backspace => {
                    m.draft.pop();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => m.draft.push(c),
                _ => {}
            }
            st.modal = Some(Modal::Message(m));
            Action::None
        }
        Modal::Restore(summary) => match key.code {
            KeyCode::Char('y') | KeyCode::Enter => Action::Restore,
            KeyCode::Char('n') | KeyCode::Esc => Action::DismissRestore,
            _ => {
                st.modal = Some(Modal::Restore(summary));
                Action::None
            }
        },
        Modal::Spawn => {
            let outcome = spawn_modal_key(st, key);
            if outcome.is_none_and_open() {
                st.modal = Some(Modal::Spawn);
            }
            outcome.into_action()
        }
        Modal::SourceForm(mut form) => {
            use super::sources::{form_key, FormOutcome};
            match form_key(&mut form, key, &st.brain.sources.catalogues) {
                FormOutcome::Open => st.modal = Some(Modal::SourceForm(form)),
                FormOutcome::Cancel => {}
                FormOutcome::Submit => return Action::SaveBrainSource(Box::new(form.form)),
            }
            Action::None
        }
        Modal::AddRemote(mut m) => {
            let outcome = add_remote_modal_key(&mut m, key);
            if outcome.is_none_and_open() {
                st.modal = Some(Modal::AddRemote(m));
            }
            outcome.into_action()
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

/// Tab/Shift-Tab move between fields; Left/Right step a focused Choice
/// field (not h/l — those letters must stay typeable into Name/Workspace/
/// Prompt); `ctrl+enter` validates and submits; a plain Enter on the Prompt
/// field inserts a newline rather than submitting; Esc cancels.
fn spawn_modal_key(st: &mut TuiState, key: KeyEvent) -> SpawnOutcome {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let form = &mut st.spawn_form;
    match key.code {
        KeyCode::Esc => SpawnOutcome::Cancel,
        KeyCode::Enter if ctrl => match spawn::try_submit(form) {
            Ok(sub) => SpawnOutcome::Submit(Action::Spawn(sub)),
            Err(e) => {
                form.error = Some(e);
                SpawnOutcome::Open
            }
        },
        KeyCode::Tab => {
            form.focus = spawn::next_focus(form, false);
            SpawnOutcome::Open
        }
        KeyCode::BackTab => {
            form.focus = spawn::next_focus(form, true);
            SpawnOutcome::Open
        }
        KeyCode::Left => {
            spawn::step(form, true);
            SpawnOutcome::Open
        }
        KeyCode::Right => {
            spawn::step(form, false);
            SpawnOutcome::Open
        }
        KeyCode::Enter => {
            if form.fields[form.focus].id == spawn::FieldId::Prompt {
                spawn::push_char(form, '\n');
            }
            SpawnOutcome::Open
        }
        KeyCode::Backspace => {
            spawn::backspace(form);
            SpawnOutcome::Open
        }
        KeyCode::Char(' ') if form.fields[form.focus].id == spawn::FieldId::Brains => {
            spawn::toggle_brain(form);
            SpawnOutcome::Open
        }
        KeyCode::Char(c) if !ctrl => {
            spawn::push_char(form, c);
            SpawnOutcome::Open
        }
        _ => SpawnOutcome::Open,
    }
}

enum AddRemoteOutcome {
    Open,
    Cancel,
    Submit(Action),
}

impl AddRemoteOutcome {
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

/// Tab/Shift-Tab swap the two fields; Enter submits from either one (unlike
/// the Spawn modal, there's no "advance then submit" staging — just Host and
/// an optional Label). An empty Host sets `error` and keeps the modal open.
fn add_remote_modal_key(modal: &mut AddRemoteModal, key: KeyEvent) -> AddRemoteOutcome {
    match key.code {
        KeyCode::Esc => AddRemoteOutcome::Cancel,
        KeyCode::Tab | KeyCode::BackTab => {
            modal.on_label_field = !modal.on_label_field;
            AddRemoteOutcome::Open
        }
        KeyCode::Enter => {
            let host = modal.host.trim().to_string();
            if host.is_empty() {
                modal.error = Some("host is required".into());
                return AddRemoteOutcome::Open;
            }
            let label = modal.label.trim().to_string();
            AddRemoteOutcome::Submit(Action::AddRemote { host, label: (!label.is_empty()).then_some(label) })
        }
        KeyCode::Backspace => {
            modal.error = None;
            if modal.on_label_field {
                modal.label.pop();
            } else {
                modal.host.pop();
            }
            AddRemoteOutcome::Open
        }
        KeyCode::Char(c) => {
            modal.error = None;
            if modal.on_label_field {
                modal.label.push(c);
            } else {
                modal.host.push(c);
            }
            AddRemoteOutcome::Open
        }
        _ => AddRemoteOutcome::Open,
    }
}

fn contains(r: Rect, x: u16, y: u16) -> bool {
    x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
}

/// Mouse routing, herdr-style: the header (tabs), footer, sidebar and the
/// pane's `◀ fleet` button always belong to ninox, whatever the agent asked
/// for; only events inside the pane's inner area can reach the agent, in
/// pane-local coordinates.
/// A click on a PR chip never reaches the row under it: a PR chip opens
/// the PR, `+N` selects the session and opens it with its inspector.
fn chip_click(st: &mut TuiState, target: super::pr_chips::ChipTarget) -> Action {
    use super::pr_chips::ChipTarget;
    match target {
        ChipTarget::Open(url) => Action::OpenUrl(url),
        ChipTarget::More(id) => {
            let Some(row) = st.rows.iter().position(|r| r.id() == id) else { return Action::None };
            st.select(row);
            st.fleet.nav.select(id);
            st.inspector = true;
            st.open_selected()
        }
    }
}

pub fn handle_mouse(st: &mut TuiState, ev: MouseEvent) -> Action {
    let (x, y) = (ev.column, ev.row);
    if st.modal.is_some() {
        let left = matches!(ev.kind, MouseEventKind::Down(MouseButton::Left));
        if matches!(st.modal, Some(Modal::Settings)) {
            return settings_mouse(st, &ev);
        }
        // Help has no controls: any click closes it.
        if matches!(ev.kind, MouseEventKind::Down(_)) && matches!(st.modal, Some(Modal::Help)) {
            st.modal = None;
            st.help_scroll = 0;
        }
        if let (Some(Modal::Help), MouseEventKind::ScrollDown | MouseEventKind::ScrollUp) = (&st.modal, ev.kind) {
            let max = st.layout.help_max_scroll;
            let at = st.help_scroll.min(max);
            st.help_scroll = if ev.kind == MouseEventKind::ScrollDown { at.saturating_add(3).min(max) } else { at.saturating_sub(3) };
            return Action::None;
        }
        if let (true, Some(all), Some(Modal::Confirm(pending))) = (left, st.layout.modal_all, &st.modal) {
            if let Some(action) = st.confirm_all_action(pending).filter(|_| contains(all, x, y)) {
                st.modal = None;
                return action;
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
    if left_down {
        if let Some(target) = super::pr_chips::hit(st, x, y) {
            return chip_click(st, target);
        }
    }
    let double = left_down && register_click(st, x, y);
    if left_down {
        st.selection = None;
    }
    if contains(lay.header, x, y) {
        if left_down {
            if lay.settings_button.is_some_and(|b| contains(b, x, y)) {
                return open_settings(st);
            }
            if let Some(t) = lay.tabs.iter().find(|t| contains(t.rect, x, y)) {
                return st.switch_view(t.view);
            }
        }
        return Action::None;
    }
    if contains(lay.footer, x, y) {
        if left_down && st.view == View::Fleet && matches!(st.focus, Focus::Pane | Focus::Inspector) {
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
        View::Brain => return brain_mouse(st, &ev, &lay, wheel),
        View::WorkerTypes => return worker_types_mouse(st, &ev, &lay, wheel),
        View::Fleet if !matches!(st.focus, Focus::Pane | Focus::Inspector) => return fleet_mouse(st, &ev, &lay, wheel, double),
        // No dedicated mouse handler yet (keyboard-only, same as its
        // predecessor `View::Graph` returning `Action::None` here).
        View::UserRequests | View::Idle => return Action::None,
        // Board/Plans/Remotes have no dedicated mouse handler yet (each
        // has keyboard-only navigation so far).
        View::Board | View::Plans | View::Remotes | View::Fleet => {}
    }
    if lay.sidebar.or(lay.sidebar_list).is_some_and(|r| contains(r, x, y)) {
        if left_down && lay.sidebar_close.is_some_and(|r| contains(r, x, y)) {
            // Not a focus reset to Sidebar here: a Confirm modal intercepts
            // all input next regardless of focus, and on Fleet resetting it
            // would silently swap the pane-split layout for the graph
            // canvas out from under the pending confirmation.
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
                    st.select(row);
                    st.selected_pinned = pinned;
                    st.ask_remove_or_kill();
                }
            }
            (MouseEventKind::Down(MouseButton::Left), _) => {
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
                        // No focus reset to Sidebar here (see the
                        // close-button and right-click arms above):
                        // reselecting a session must not swap the
                        // pane-split layout for the full graph canvas
                        // out from under the click.
                    }
                    other => {
                        // Anything else clicked in the sidebar — a group
                        // header, a heading/gap/"+n more" row, or blank
                        // space — is a "look at the sidebar" gesture rather
                        // than a row reselect, so this does return focus
                        // there.
                        st.focus = Focus::Sidebar;
                        if let Some((SideItem::Group(g), _)) = other {
                            let g = g.clone();
                            st.toggle_collapse(&g);
                        }
                    }
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
fn move_worker_type_list(st: &mut TuiState, delta: i64) {
    let last = st.worker_types.names.len() as i64; // the trailing "+ New type" row
    let next = (st.worker_types.list_selected as i64 + delta).clamp(0, last) as usize;
    st.worker_types.select_list_row(next);
}

fn move_worker_type_field(st: &mut TuiState, delta: i64) {
    let last = st.worker_types.fields.len().saturating_sub(1) as i64;
    st.worker_types.field_selected = (st.worker_types.field_selected as i64 + delta).clamp(0, last) as usize;
    st.worker_types.error = None;
}

fn activate_worker_type_field(st: &mut TuiState) -> Action {
    if st.worker_types.is_new_row() {
        // Only the Name field (index 0) means anything before the type
        // exists — activating any other field here would open a blank
        // text buffer whose Enter still fires NewWorkerType, creating the
        // type under whatever text was typed into the wrong field.
        if st.worker_types.field_selected == 0 {
            st.worker_types.editing = Some(String::new());
        } else {
            st.worker_types.error = Some("name the new type first".into());
        }
        return Action::None;
    }
    let Some(name) = st.worker_types.selected_name().map(str::to_string) else { return Action::None };
    let Some(f) = st.worker_types.selected_field().cloned() else { return Action::None };
    st.worker_types.error = None;
    match f.kind {
        Kind::Toggle | Kind::Choice => Action::SaveWorkerType { name, change: worker_types::Change::Step { id: f.id, back: false } },
        Kind::Text => {
            st.worker_types.editing = Some(f.value);
            Action::None
        }
        Kind::Action => Action::None,
    }
}

fn step_worker_type_field(st: &mut TuiState, back: bool) -> Action {
    let Some(name) = st.worker_types.selected_name().map(str::to_string) else { return Action::None };
    let Some(id) = st.worker_types.selected_field().filter(|f| !f.options.is_empty()).map(|f| f.id.clone()) else {
        return Action::None;
    };
    st.worker_types.error = None;
    Action::SaveWorkerType { name, change: worker_types::Change::Step { id, back } }
}

/// The selected form row's rule list and index, if it's a rule row.
fn rule_row(st: &TuiState) -> Option<(worker_types::RuleList, Option<usize>)> {
    match st.worker_types.selected_field()?.id {
        worker_types::FieldId::Rule(list, at) => Some((list, at)),
        _ => None,
    }
}

fn worker_types_key(st: &mut TuiState, key: KeyEvent) -> Action {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if let Some(buf) = st.worker_types.editing.as_mut() {
        match key.code {
            KeyCode::Esc => {
                st.worker_types.editing = None;
                st.worker_types.adding_rule = None;
                st.worker_types.error = None;
            }
            KeyCode::Enter => {
                let text = buf.clone();
                if st.worker_types.is_new_row() {
                    return Action::NewWorkerType(text);
                }
                let Some(name) = st.worker_types.selected_name().map(str::to_string) else { return Action::None };
                // Cleared only once the save lands (`perform`), so a refused
                // add can be fixed and retried as an add, not an edit.
                if let Some(list) = st.worker_types.adding_rule {
                    return Action::SaveWorkerType { name, change: worker_types::Change::AddRule { list, text } };
                }
                if let Some(f) = st.worker_types.selected_field() {
                    if let worker_types::FieldId::Rule(list, Some(index)) = f.id {
                        let rule = f.value.clone();
                        return Action::SaveWorkerType { name, change: worker_types::Change::EditRule { list, index, rule, text } };
                    }
                }
                let Some(f) = st.worker_types.selected_field().cloned() else { return Action::None };
                return Action::SaveWorkerType { name, change: worker_types::Change::Set { id: f.id, text } };
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
        KeyCode::Esc | KeyCode::Char('q') => st.view = View::Fleet,
        KeyCode::Tab | KeyCode::BackTab => st.worker_types.on_list = !st.worker_types.on_list,
        KeyCode::Char('j') | KeyCode::Down if st.worker_types.on_list => move_worker_type_list(st, 1),
        KeyCode::Char('k') | KeyCode::Up if st.worker_types.on_list => move_worker_type_list(st, -1),
        KeyCode::Char('j') | KeyCode::Down => move_worker_type_field(st, 1),
        KeyCode::Char('k') | KeyCode::Up => move_worker_type_field(st, -1),
        KeyCode::Enter | KeyCode::Char(' ') if st.worker_types.on_list => st.worker_types.on_list = false,
        KeyCode::Enter | KeyCode::Char(' ') => return activate_worker_type_field(st),
        KeyCode::Char('l') | KeyCode::Right if !st.worker_types.on_list => return step_worker_type_field(st, false),
        KeyCode::Char('h') | KeyCode::Left if !st.worker_types.on_list => return step_worker_type_field(st, true),
        KeyCode::Char('n') => {
            st.worker_types.select_list_row(st.worker_types.names.len());
            st.worker_types.on_list = false;
            st.worker_types.editing = Some(String::new());
        }
        KeyCode::Char('D') => match st.worker_types.selected_name().map(str::to_string) {
            Some(name) if st.worker_types.builtin.contains(&name) => {
                st.worker_types.error = Some(format!("{name} is built in and can't be deleted; R resets a customized one"));
            }
            Some(name) => st.modal = Some(Modal::Confirm(Pending::DeleteWorkerType(name))),
            None => {}
        },
        // On a rule row `a`/`x` add/delete a rule; elsewhere `x` exports.
        KeyCode::Char('a') | KeyCode::Char('x') if !st.worker_types.on_list && rule_row(st).is_some() => {
            let (Some(name), Some((list, at))) = (st.worker_types.selected_name().map(str::to_string), rule_row(st)) else {
                return Action::None;
            };
            st.worker_types.error = None;
            match (key.code, at) {
                (KeyCode::Char('a'), _) => {
                    st.worker_types.adding_rule = Some(list);
                    st.worker_types.editing = Some(String::new());
                }
                (_, Some(index)) => {
                    let rule = st.worker_types.selected_field().map(|f| f.value.clone()).unwrap_or_default();
                    return Action::SaveWorkerType { name, change: worker_types::Change::RemoveRule { list, index, rule } };
                }
                _ => st.worker_types.error = Some("no rule here to delete".into()),
            }
        }
        KeyCode::Char('x') => {
            if let Some(name) = st.worker_types.selected_name().map(str::to_string) {
                return Action::ExportWorkerType(name);
            }
        }
        KeyCode::Char('R') => match st.worker_types.selected_name().map(str::to_string) {
            Some(name) if st.worker_types.resettable(&name) => st.modal = Some(Modal::Confirm(Pending::ResetWorkerType(name))),
            Some(name) => st.worker_types.error = Some(format!("only a customized built-in can be reset; {name} isn't one")),
            None => {}
        },
        KeyCode::Char('i') => return Action::ImportWorkerType,
        KeyCode::Char('?') => st.modal = Some(Modal::Help),
        _ => {}
    }
    Action::None
}

/// The settings page: a click on a section selects it, a click on a field
/// selects it and a click on its value (or a second click on it) changes it,
/// as Enter does; the wheel moves within the pane it is over.
fn settings_mouse(st: &mut TuiState, ev: &MouseEvent) -> Action {
    let (x, y) = (ev.column, ev.row);
    let sl = super::layout::settings(super::layout::settings_area(st.layout.header.union(st.layout.footer)), st);
    let wheel = match ev.kind {
        MouseEventKind::ScrollUp => Some(-1i64),
        MouseEventKind::ScrollDown => Some(1),
        _ => None,
    };
    if let Some(d) = wheel {
        if st.settings.editing.is_none() {
            if contains(sl.bar, x, y) {
                st.settings.move_section(d);
            } else if contains(sl.list, x, y) {
                st.settings.move_field(d);
            }
        }
        return Action::None;
    }
    if !matches!(ev.kind, MouseEventKind::Down(MouseButton::Left)) {
        return Action::None;
    }
    if contains(sl.close, x, y) {
        st.modal = None;
        return Action::None;
    }
    if contains(sl.open, x, y) {
        st.settings.editing = None;
        st.settings.error = None;
        return Action::EditConfig;
    }
    if contains(sl.bar, x, y) {
        if let Some(super::layout::SettingsBarLine::Section(s)) = sl.bar_lines.get(sl.bar_offset + (y - sl.bar.y) as usize) {
            st.settings.select_section(*s);
            st.settings.in_fields = false;
        }
        return Action::None;
    }
    if !contains(sl.list, x, y) {
        return Action::None;
    }
    let Some(super::layout::SettingsLine::Field(i)) = sl.lines.get(sl.offset + (y - sl.list.y) as usize).copied() else {
        return Action::None;
    };
    let again = i == st.settings.selected && st.settings.in_fields;
    if st.settings.editing.is_some() {
        if again {
            return Action::None;
        }
        st.settings.editing = None;
    }
    st.settings.in_fields = true;
    st.settings.selected = i;
    st.settings.error = None;
    if again || x >= sl.value_x {
        return activate_setting(st);
    }
    Action::None
}

fn worker_types_mouse(st: &mut TuiState, ev: &MouseEvent, lay: &Layout, wheel: Option<i64>) -> Action {
    let Some(wl) = lay.worker_types.clone() else { return Action::None };
    let (x, y) = (ev.column, ev.row);
    if let Some(d) = wheel {
        if st.worker_types.editing.is_none() {
            if contains(wl.names, x, y) {
                move_worker_type_list(st, -d.signum());
            } else {
                move_worker_type_field(st, -d.signum());
            }
        }
        return Action::None;
    }
    if !matches!(ev.kind, MouseEventKind::Down(MouseButton::Left)) {
        return Action::None;
    }
    if contains(wl.names, x, y) {
        let i = wl.names_offset + (y - wl.names.y) as usize;
        if i <= st.worker_types.names.len() {
            st.worker_types.editing = None;
            st.worker_types.select_list_row(i);
            st.worker_types.on_list = false;
        }
        return Action::None;
    }
    if !contains(wl.fields, x, y) {
        return Action::None;
    }
    let i = wl.fields_offset + (y - wl.fields.y) as usize;
    if i >= st.worker_types.fields.len() {
        return Action::None;
    }
    let again = i == st.worker_types.field_selected && !st.worker_types.on_list;
    if st.worker_types.editing.is_some() {
        if again {
            return Action::None;
        }
        st.worker_types.editing = None;
    }
    st.worker_types.on_list = false;
    st.worker_types.field_selected = i;
    st.worker_types.error = None;
    if again || x >= wl.value_x {
        return activate_worker_type_field(st);
    }
    Action::None
}

fn brain_mouse(st: &mut TuiState, ev: &MouseEvent, lay: &Layout, wheel: Option<i64>) -> Action {
    use super::layout::BrainButton;
    // Brain › Sources is keyboard-only; the list's hit areas sit under it.
    if st.brain.sources.open {
        return Action::None;
    }
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
    if matches!(st.modal, Some(Modal::Spawn)) {
        spawn::paste(&mut st.spawn_form, text);
        return Action::None;
    }
    if let Some(Modal::AddRemote(m)) = st.modal.as_mut() {
        m.error = None;
        let field = if m.on_label_field { &mut m.label } else { &mut m.host };
        field.push_str(text);
        return Action::None;
    }
    if let Some(Modal::Message(m)) = st.modal.as_mut() {
        m.draft.push_str(&text.replace("\r\n", "\n"));
        return Action::None;
    }
    if st.modal.is_some() || st.view != View::Fleet || st.focus != Focus::Pane {
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
    use crate::tui::worker_types::FieldId as WtFieldId;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ninox_core::brain::BrainSort;
    use ninox_overview::islands::IslandSort;

    #[test]
    fn plans_tab_a_and_x_ask_before_accepting_or_discarding() {
        use ninox_core::ghost_plan::PlanStatus;
        let mut st = TuiState { view: View::Plans, ..Default::default() };
        st.ghost_plans = vec![
            crate::tui::ghost_fixture("gp-1", &["alpha", "beta"], PlanStatus::Pending, 1),
            crate::tui::ghost_fixture("gp-2", &["old"], PlanStatus::Expired, 0),
        ];
        assert_eq!(plans_key(&mut st, key('a')), Action::None);
        assert_eq!(st.modal, Some(Modal::Confirm(Pending::GhostAccept("gp-1".into()))));
        assert_eq!(st.confirm_text(&Pending::GhostAccept("gp-1".into())).0, "Spawn 2 sessions?");
        assert_eq!(handle_modal_key(&mut st, key('n')), Action::None, "n cancels without accepting");
        plans_key(&mut st, key('a'));
        assert_eq!(handle_modal_key(&mut st, key('y')), Action::GhostAccept("gp-1".into()));

        for k in ['x', 's'] {
            plans_key(&mut st, key(k));
            assert_eq!(st.modal.take(), Some(Modal::Confirm(Pending::GhostDiscard("gp-1".into()))));
        }

        plans_key(&mut st, key('j'));
        assert_eq!(st.plans_sel_index(), 1);
        plans_key(&mut st, key('a'));
        assert_eq!(st.modal, None, "an expired plan can't be accepted");
    }

    #[test]
    fn plans_selection_follows_its_plan_when_the_list_reorders() {
        use ninox_core::ghost_plan::PlanStatus;
        let mut st = TuiState { view: View::Plans, ..Default::default() };
        st.ghost_plans = vec![
            crate::tui::ghost_fixture("gp-a", &["alpha"], PlanStatus::Pending, 2),
            crate::tui::ghost_fixture("gp-b", &["beta"], PlanStatus::Pending, 1),
        ];
        plans_key(&mut st, key('j'));
        st.ghost_plans.insert(0, crate::tui::ghost_fixture("gp-c", &["gamma"], PlanStatus::Pending, 3));
        assert_eq!(st.plans_sel_index(), 2);
        plans_key(&mut st, key('a'));
        assert_eq!(st.modal, Some(Modal::Confirm(Pending::GhostAccept("gp-b".into()))));
    }

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
    fn enter_opens_the_tree_selections_owning_island() {
        let mut st = TuiState { rows: vec![orchestrator_row("acme"), worker_row("w1", "acme")], view: View::Fleet, ..Default::default() };
        st.fleet.sync(&st.rows, &HashMap::new());
        st.selected = 1; // w1

        handle_key(&mut st, code(KeyCode::Enter));

        assert_eq!(st.fleet.nav.open, Some(ninox_overview::islands::IslandKey::Root("acme".into())));
        assert_eq!(st.fleet.nav.selected.as_deref(), Some("w1"));
        assert_eq!(st.fleet.main_focus, FleetFocus::Main, "Enter opens the island and moves focus to the centre");
    }

    #[test]
    fn esc_first_clears_selection_then_closes_the_island() {
        let mut st = TuiState { rows: vec![orchestrator_row("acme")], view: View::Fleet, ..Default::default() };
        st.fleet.sync(&st.rows, &HashMap::new());
        st.selected = 0;
        handle_key(&mut st, code(KeyCode::Enter));
        assert!(st.fleet.nav.open.is_some());
        assert!(st.fleet.nav.selected.is_some());

        handle_key(&mut st, code(KeyCode::Esc));
        assert!(st.fleet.nav.open.is_some(), "first esc only clears the selection");
        assert!(st.fleet.nav.selected.is_none());

        handle_key(&mut st, code(KeyCode::Esc));
        assert!(st.fleet.nav.open.is_none(), "second esc closes the island");
        assert_eq!(
            st.fleet.nav.selected_island,
            Some(ninox_overview::islands::IslandKey::Root("acme".into())),
            "the island stays selected on the Fleet grid"
        );
    }

    #[test]
    fn esc_drops_a_stranded_detail_focus_back_to_main() {
        let mut st = TuiState { rows: vec![orchestrator_row("acme")], view: View::Fleet, ..Default::default() };
        st.fleet.sync(&st.rows, &HashMap::new());
        st.selected = 0;
        handle_key(&mut st, code(KeyCode::Enter));
        st.fleet.main_focus = FleetFocus::Detail;

        handle_key(&mut st, code(KeyCode::Esc));
        assert_eq!(st.fleet.main_focus, FleetFocus::Main, "the detail panel esc just closed can't still hold focus");
    }

    #[test]
    fn tab_cycles_tree_main_and_back_without_a_detail_panel() {
        let mut st = TuiState { rows: vec![Row::test_row("a")], view: View::Fleet, ..Default::default() };
        assert_eq!(st.fleet.main_focus, FleetFocus::Tree);
        handle_key(&mut st, code(KeyCode::Tab));
        assert_eq!(st.fleet.main_focus, FleetFocus::Main, "no detail panel open — tab skips straight back to tree next");
        handle_key(&mut st, code(KeyCode::Tab));
        assert_eq!(st.fleet.main_focus, FleetFocus::Tree);
    }

    #[test]
    fn tab_cycles_through_detail_once_a_node_is_selected_in_an_open_island() {
        let mut st = TuiState { rows: vec![orchestrator_row("acme")], view: View::Fleet, ..Default::default() };
        st.fleet.sync(&st.rows, &HashMap::new());
        st.selected = 0;
        handle_key(&mut st, code(KeyCode::Enter)); // opens the island, focus -> Main, node selected
        assert_eq!(st.fleet.main_focus, FleetFocus::Main);

        handle_key(&mut st, code(KeyCode::Tab));
        assert_eq!(st.fleet.main_focus, FleetFocus::Detail, "a node is selected in the open island — detail is in the cycle");
        handle_key(&mut st, code(KeyCode::Tab));
        assert_eq!(st.fleet.main_focus, FleetFocus::Tree, "wraps back to the tree");
    }

    #[test]
    fn shift_tab_cycles_backward() {
        let mut st = TuiState { rows: vec![orchestrator_row("acme")], view: View::Fleet, ..Default::default() };
        st.fleet.sync(&st.rows, &HashMap::new());
        st.selected = 0;
        handle_key(&mut st, code(KeyCode::Enter));
        assert_eq!(st.fleet.main_focus, FleetFocus::Main);

        handle_key(&mut st, code(KeyCode::BackTab));
        assert_eq!(st.fleet.main_focus, FleetFocus::Tree, "shift-tab from main goes backward, to tree");
        handle_key(&mut st, code(KeyCode::BackTab));
        assert_eq!(st.fleet.main_focus, FleetFocus::Detail, "backward from tree wraps to the last entry, detail");
    }

    #[test]
    fn right_arrow_on_the_tree_crosses_focus_to_main() {
        let mut st = TuiState { rows: vec![Row::test_row("a")], view: View::Fleet, ..Default::default() };
        assert_eq!(st.fleet.main_focus, FleetFocus::Tree);
        handle_key(&mut st, code(KeyCode::Right));
        assert_eq!(st.fleet.main_focus, FleetFocus::Main);
    }

    #[test]
    fn left_arrow_at_the_grids_leftmost_column_crosses_focus_back_to_tree() {
        let mut st = TuiState { rows: vec![orchestrator_row("acme"), orchestrator_row("other")], view: View::Fleet, ..Default::default() };
        st.fleet.sync(&st.rows, &HashMap::new());
        st.fleet.main_focus = FleetFocus::Main;
        st.layout.fleet = Some(crate::tui::layout::FleetLayout {
            grid: vec![
                (ninox_overview::islands::IslandKey::Root("acme".into()), Rect::new(0, 0, 10, 5)),
                (ninox_overview::islands::IslandKey::Root("other".into()), Rect::new(11, 0, 10, 5)),
            ],
            ..Default::default()
        });
        st.fleet.nav.selected_island = Some(ninox_overview::islands::IslandKey::Root("acme".into()));

        handle_key(&mut st, code(KeyCode::Left));
        assert_eq!(st.fleet.main_focus, FleetFocus::Tree, "already the leftmost column — left crosses back to the tree");
    }

    #[test]
    fn left_arrow_inside_the_opened_island_crosses_to_tree_only_at_its_own_left_edge() {
        let mut st = TuiState { rows: vec![orchestrator_row("acme"), worker_row("w1", "acme")], view: View::Fleet, ..Default::default() };
        st.fleet.sync(&st.rows, &HashMap::new());
        st.selected = 0;
        handle_key(&mut st, code(KeyCode::Enter)); // opens acme, focus -> Main, "acme" selected
        assert_eq!(st.fleet.main_focus, FleetFocus::Main);
        // Real card positions only exist once something has rendered
        // (`view::draw_fleet_opened` calls this every frame) — build them
        // here so `fleet_main_arrow`'s rataflow branch has a flow to act on.
        st.fleet.ensure_opened_flow(&st.rows, &Palette::default(), 200.0);

        handle_key(&mut st, code(KeyCode::Left));
        assert_eq!(st.fleet.main_focus, FleetFocus::Tree, "the root card has nothing to its left in the opened tree");
    }

    #[test]
    fn opening_an_island_from_the_grid_moves_the_tree_cursor_to_match() {
        // Regression: bare commands ('c', 'r', 'O', 'R', 'x', …) all read
        // the tree cursor (`st.selected`), not `fleet.nav.selected` — if
        // the grid-focused Enter path opens an island the tree cursor
        // isn't already sitting on, those commands would silently act on
        // whatever unrelated row the tree cursor was last left on.
        let mut st = TuiState { rows: vec![orchestrator_row("acme"), orchestrator_row("other")], view: View::Fleet, ..Default::default() };
        st.fleet.sync(&st.rows, &HashMap::new());
        st.selected = 1; // tree cursor on "other" — not the island about to open
        st.fleet.main_focus = FleetFocus::Main;
        st.fleet.nav.selected_island = Some(ninox_overview::islands::IslandKey::Root("acme".into()));

        handle_key(&mut st, code(KeyCode::Enter));

        assert_eq!(st.fleet.nav.open, Some(ninox_overview::islands::IslandKey::Root("acme".into())));
        assert_eq!(st.selected_id(), Some("acme".to_string()), "the tree cursor must follow the island actually opened, not stay on the old row");
    }

    #[test]
    fn arrow_nav_inside_an_opened_island_moves_the_tree_cursor_to_match() {
        let mut st = TuiState { rows: vec![orchestrator_row("acme"), worker_row("w1", "acme")], view: View::Fleet, ..Default::default() };
        st.fleet.sync(&st.rows, &HashMap::new());
        st.selected = 0;
        handle_key(&mut st, code(KeyCode::Enter)); // opens acme, tree cursor + nav.selected both "acme"
        st.fleet.ensure_opened_flow(&st.rows, &Palette::default(), 200.0);

        handle_key(&mut st, code(KeyCode::Down)); // selects w1 inside the opened card tree

        assert_eq!(st.fleet.nav.selected.as_deref(), Some("w1"));
        assert_eq!(st.selected_id(), Some("w1".to_string()), "the tree cursor must follow the card tree's own selection so 'c'/'r'/'O' act on the right row");
    }

    #[test]
    fn selecting_a_tree_row_also_selects_its_owning_island() {
        let mut st = TuiState { rows: vec![orchestrator_row("acme"), orchestrator_row("other")], view: View::Fleet, ..Default::default() };
        st.fleet.sync(&st.rows, &HashMap::new());
        st.selected = 1; // "other"

        handle_key(&mut st, key('j')); // step_selection within the tree

        let selected_id = st.fleet.nav.selected.clone();
        let owner = selected_id.and_then(|id| ninox_overview::nav::owning_island(&st.fleet.islands, &id));
        assert_eq!(st.fleet.nav.selected_island, owner, "the grid selection follows whichever row the tree cursor lands on");
    }

    #[test]
    fn m_toggles_the_fleet_density_view() {
        let mut st = TuiState { rows: vec![Row::test_row("a")], view: View::Fleet, ..Default::default() };
        assert!(st.fleet.compact, "compact is the default, matching TUIAtScale.dc.html");
        handle_key(&mut st, key('m'));
        assert!(!st.fleet.compact);
        handle_key(&mut st, key('m'));
        assert!(st.fleet.compact);
    }

    #[test]
    fn f_folds_the_focused_island_and_ctrl_f_folds_every_island() {
        use ninox_core::types::SessionStatus as S;
        let mut st = TuiState { view: View::Fleet, ..Default::default() };
        for (id, orch) in [("o1", "o1"), ("w1", "o1"), ("o2", "o2")] {
            let mut r = Row::test_row(id);
            r.session = ninox_core::types::Session { orchestrator_id: (id != orch).then(|| orch.to_string()), ..r.session };
            r.is_orchestrator = id == orch;
            r.group = Some(orch.into());
            r.session.status = S::Working;
            st.rows.push(r);
        }
        st.fleet.sync(&st.rows.clone(), &std::collections::HashMap::new());
        let k1 = ninox_overview::islands::IslandKey::Root("o1".into());
        let k2 = ninox_overview::islands::IslandKey::Root("o2".into());
        assert!(!st.fleet.fold_expanded, "folded is the default, matching TUIAtScale.dc.html");
        st.fleet.nav.selected_island = Some(k1.clone());
        handle_key(&mut st, key('f'));
        assert!(st.fleet.island_fold_expanded(&k1) && !st.fleet.island_fold_expanded(&k2), "only the focused island");
        handle_key(&mut st, KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
        assert!(st.fleet.island_fold_expanded(&k1) && st.fleet.island_fold_expanded(&k2), "every island, to the same state");
        handle_key(&mut st, KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
        assert!(!st.fleet.island_fold_expanded(&k1) && !st.fleet.island_fold_expanded(&k2));
    }

    #[test]
    fn capital_m_cycles_the_machine_filter_and_hides_other_machines_islands() {
        let mut st = TuiState { view: View::Fleet, ..Default::default() };
        for (id, machine) in [("local", None), ("far", Some("box"))] {
            let mut r = Row::test_row(id);
            r.is_orchestrator = true;
            r.group = Some(id.into());
            r.session.machine_id = machine.map(str::to_string);
            st.rows.push(r);
        }
        st.fleet.sync(&st.rows.clone(), &std::collections::HashMap::new());
        let names = |st: &TuiState| st.fleet.islands.iter().map(|i| i.name.clone()).collect::<Vec<_>>();
        assert_eq!(st.fleet.islands.len(), 2);
        st.fleet.nav.open_island(ninox_overview::islands::IslandKey::Root("far".into()), &st.fleet.islands);
        st.fleet.main_focus = FleetFocus::Detail;
        handle_key(&mut st, KeyEvent::new(KeyCode::Char('M'), KeyModifiers::SHIFT));
        assert_eq!(st.fleet.main_focus, FleetFocus::Main, "focus doesn't stay on a detail panel that closed");
        assert_eq!(st.fleet.machine_filter, ninox_overview::islands::MachineFilter::Local);
        assert_eq!(st.fleet.islands.len(), 1, "{:?}", names(&st));
        assert!(st.fleet.nav.open.is_none(), "the opened island was filtered away");
        handle_key(&mut st, KeyEvent::new(KeyCode::Char('M'), KeyModifiers::SHIFT));
        assert_eq!(st.fleet.machine_filter, ninox_overview::islands::MachineFilter::Machine("box".into()));
        handle_key(&mut st, KeyEvent::new(KeyCode::Char('M'), KeyModifiers::SHIFT));
        assert_eq!((st.fleet.machine_filter.clone(), st.fleet.islands.len()), (ninox_overview::islands::MachineFilter::All, 2));
    }

    #[test]
    fn s_cycles_the_island_sort() {
        let mut st = TuiState { rows: vec![Row::test_row("a")], view: View::Fleet, ..Default::default() };
        assert_eq!(st.fleet.sort, IslandSort::NeedsYouFirst);
        handle_key(&mut st, key('s'));
        assert_eq!(st.fleet.sort, IslandSort::MostActive);
        handle_key(&mut st, key('s'));
        assert_eq!(st.fleet.sort, IslandSort::AToZ);
        handle_key(&mut st, key('s'));
        assert_eq!(st.fleet.sort, IslandSort::NeedsYouFirst);
    }

    #[test]
    fn plans_tab_never_types_into_a_backgrounded_pane() {
        // Final review Critical #1: switching to Plans while a pane is
        // attached (Focus::Pane) must not leave bare keys falling through
        // to `pane_key`, typing into a live agent with nothing on screen
        // to show it.
        let mut st = state_with_rows(1);
        with_live_pane(&mut st, "s0");
        st.focus = Focus::Pane;
        st.view = View::Plans;
        assert_eq!(handle_key(&mut st, key('y')), Action::None, "must not type into the backgrounded pane");
        assert_eq!(handle_key(&mut st, code(KeyCode::Enter)), Action::None);
    }

    #[test]
    fn bracket_toggles_the_fleet_tree_rail() {
        let mut st = TuiState { rows: vec![Row::test_row("a")], view: View::Fleet, ..Default::default() };
        assert!(st.tree_collapsed, "the tree starts collapsed on the Fleet tab — the islands already show every orchestrator");
        handle_key(&mut st, key('['));
        assert!(!st.tree_collapsed);
        handle_key(&mut st, key('['));
        assert!(st.tree_collapsed);
    }

    #[test]
    fn bracket_collapses_even_with_a_runnable_pane_selected() {
        // Final review Important #2: `focused_pane()` is `Some` for any
        // ptyd-backed row whether or not it's actually attached, so a
        // real fleet (almost always ptyd-backed) could never collapse the
        // tree — `[` always fell into the old scroll-jump instead.
        let mut st = state_with_rows(1);
        st.tree_collapsed = false;
        with_live_pane(&mut st, "s0");
        assert_eq!(st.focus, Focus::Sidebar, "not attached to the pane");
        handle_key(&mut st, key('['));
        assert!(st.tree_collapsed, "must collapse — the pane isn't what's on screen");
        assert_eq!(st.mode, Mode::Normal, "must not have entered scroll mode");
    }

    #[test]
    fn typing_a_digit_into_the_fleet_prompt_does_not_switch_tabs() {
        let mut st = TuiState { rows: vec![Row::test_row("a")], view: View::Fleet, ..Default::default() };
        st.fleet.prompt_editing = true;

        handle_key(&mut st, key('2'));

        assert_eq!(st.view, View::Fleet, "a digit typed into the prompt must not be read as a tab switch");
        assert_eq!(st.fleet.prompt_input, "2");
        assert!(st.fleet.prompt_editing);
    }

    fn orchestrator_row(id: &str) -> Row {
        Row { is_orchestrator: true, group: Some(id.to_string()), ..Row::test_row(id) }
    }

    fn worker_row(id: &str, group: &str) -> Row {
        let mut row = Row { is_orchestrator: false, group: Some(group.to_string()), ..Row::test_row(id) };
        // Real `build_rows` data always keeps `session.orchestrator_id` in
        // step with `group` — the island model (`fleet::fleet_snapshot`)
        // reads the former, not the latter, so this fixture must too.
        row.session.orchestrator_id = Some(group.to_string());
        row
    }

    #[test]
    fn prompt_defaults_to_the_agent_and_ctrl_t_cycles_through_orchestrators_and_back() {
        let mut st = TuiState { rows: vec![orchestrator_row("acme"), orchestrator_row("beta")], ..Default::default() };
        assert_eq!(st.fleet.prompt_target, PromptTarget::Agent);
        let ctrl_t = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL);

        handle_key(&mut st, ctrl_t);
        assert_eq!(st.fleet.prompt_target, PromptTarget::Orchestrator("acme".into()));
        handle_key(&mut st, ctrl_t);
        assert_eq!(st.fleet.prompt_target, PromptTarget::Orchestrator("beta".into()));
        handle_key(&mut st, ctrl_t);
        assert_eq!(st.fleet.prompt_target, PromptTarget::Agent, "cycling past the last orchestrator returns to the agent");
    }

    #[test]
    fn ctrl_t_cycles_while_the_prompt_is_focused_too() {
        let mut st = TuiState { rows: vec![orchestrator_row("acme")], ..Default::default() };
        st.fleet.prompt_editing = true;
        handle_key(&mut st, KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert_eq!(st.fleet.prompt_target, PromptTarget::Orchestrator("acme".into()));
        assert!(st.fleet.prompt_editing, "cycling the target must not close the prompt");
    }

    #[test]
    fn opening_the_prompt_resets_the_target_to_the_agent() {
        let mut st = TuiState { rows: vec![orchestrator_row("acme")], view: View::Fleet, ..Default::default() };
        st.fleet.prompt_target = PromptTarget::Orchestrator("acme".into());
        handle_key(&mut st, key('/'));
        assert_eq!(st.fleet.prompt_target, PromptTarget::Agent);
    }

    #[test]
    fn fleet_prompt_enter_submits_to_the_current_target_and_clears_the_input() {
        let mut st = TuiState { rows: vec![orchestrator_row("acme")], view: View::Fleet, ..Default::default() };
        st.fleet.prompt_editing = true;
        st.fleet.prompt_target = PromptTarget::Orchestrator("acme".into());
        st.fleet.prompt_input = "what's blocking the rewrite?".to_string();

        let action = handle_key(&mut st, code(KeyCode::Enter));

        assert_eq!(action, Action::SubmitPrompt { target: PromptTarget::Orchestrator("acme".into()), question: "what's blocking the rewrite?".into(), message: "what's blocking the rewrite?".into() });
        assert!(st.fleet.prompt_input.is_empty());
        assert!(!st.fleet.prompt_editing);
        assert_eq!(st.fleet.prompt_history, vec!["what's blocking the rewrite?".to_string()]);
    }

    #[test]
    fn fleet_prompt_enter_defaults_to_the_agent() {
        let mut st = TuiState { view: View::Fleet, ..Default::default() };
        st.fleet.prompt_editing = true;
        st.fleet.prompt_input = "spawn an orchestrator".to_string();

        let action = handle_key(&mut st, code(KeyCode::Enter));

        assert_eq!(action, Action::SubmitPrompt { target: PromptTarget::Agent, question: "spawn an orchestrator".into(), message: "spawn an orchestrator".into() });
    }

    #[test]
    fn fleet_prompt_enter_prefixes_the_scope_when_an_island_is_open() {
        let mut st = TuiState { rows: vec![orchestrator_row("acme")], view: View::Fleet, ..Default::default() };
        st.fleet.sync(&st.rows, &HashMap::new());
        st.selected = 0;
        handle_key(&mut st, code(KeyCode::Enter)); // opens acme's island
        assert!(st.fleet.nav.open.is_some(), "the island must actually be open for this test to mean anything");
        st.fleet.prompt_editing = true;
        st.fleet.prompt_input = "what's blocking this?".to_string();

        let action = handle_key(&mut st, code(KeyCode::Enter));

        assert_eq!(action, Action::SubmitPrompt { target: PromptTarget::Agent, question: "what's blocking this?".into(), message: "[scope: acme] what's blocking this?".into() });
    }

    #[test]
    fn fleet_prompt_enter_with_blank_input_does_nothing() {
        let mut st = TuiState { rows: vec![orchestrator_row("acme")], view: View::Fleet, ..Default::default() };
        st.fleet.prompt_editing = true;
        st.fleet.prompt_input = "   ".to_string();

        let action = handle_key(&mut st, code(KeyCode::Enter));

        assert_eq!(action, Action::None);
        assert!(st.fleet.prompt_history.is_empty());
    }

    #[test]
    fn prompt_history_up_down_recalls_past_submissions_and_restores_the_draft() {
        let mut st = TuiState { view: View::Fleet, ..Default::default() };
        st.fleet.prompt_editing = true;
        for text in ["first", "second"] {
            st.fleet.prompt_input = text.to_string();
            handle_key(&mut st, code(KeyCode::Enter));
            st.fleet.prompt_editing = true;
        }
        st.fleet.prompt_input = "draft not yet sent".to_string();

        handle_key(&mut st, code(KeyCode::Up));
        assert_eq!(st.fleet.prompt_input, "second", "most recent entry first");
        handle_key(&mut st, code(KeyCode::Up));
        assert_eq!(st.fleet.prompt_input, "first");
        handle_key(&mut st, code(KeyCode::Up));
        assert_eq!(st.fleet.prompt_input, "first", "stops at the oldest entry");
        handle_key(&mut st, code(KeyCode::Down));
        assert_eq!(st.fleet.prompt_input, "second");
        handle_key(&mut st, code(KeyCode::Down));
        assert_eq!(st.fleet.prompt_input, "", "past the newest entry returns to an empty draft");
    }

    #[test]
    fn ctrl_l_while_focused_clears_context() {
        let mut st = TuiState { view: View::Fleet, ..Default::default() };
        st.fleet.prompt_editing = true;
        let action = handle_key(&mut st, KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL));
        assert_eq!(action, Action::ClearPromptContext);
    }

    use ninox_ui_shared::prompt_agent::reply::{AnswerSource, AnswerState};

    fn answered(st: &mut TuiState, question: &str, answer: &str, source: AnswerSource) {
        st.fleet.reply.ask(question.into(), "ninox".into(), 1);
        let turn = st.fleet.reply.drawer.latest_mut().unwrap();
        turn.source = source;
        turn.answer = answer.into();
        turn.state = AnswerState::Done;
        turn.ended_at_ms = Some(2);
    }

    fn agent_source() -> AnswerSource {
        AnswerSource::Transcript(Default::default())
    }

    #[test]
    fn reply_panel_opens_on_ask_collapses_and_reopens_with_history() {
        let mut st = TuiState { view: View::Fleet, ..Default::default() };
        assert!(!st.fleet.reply.is_open());
        answered(&mut st, "one", "first answer", agent_source());
        assert!(st.fleet.reply.is_open());

        st.fleet.prompt_editing = true;
        handle_key(&mut st, code(KeyCode::Esc));
        assert!(!st.fleet.prompt_editing);
        assert!(st.fleet.reply.focused, "esc out of the input hands the keyboard to the open panel");
        assert!(st.fleet.reply.is_open());

        handle_key(&mut st, code(KeyCode::Esc));
        assert!(!st.fleet.reply.is_open(), "a second esc collapses it");
        assert!(!st.fleet.reply.focused);
        assert!(st.fleet.reply.drawer.has_content(), "collapsed still shows the one-line indicator");

        handle_key(&mut st, key('/'));
        assert!(st.fleet.reply.is_open(), "focusing the prompt reopens the panel");
        assert!(st.fleet.prompt_editing);
        assert_eq!(st.fleet.reply.drawer.turns[0].answer, "first answer", "history survives the round trip");
    }

    #[test]
    fn reply_panel_stacks_turns_and_clear_empties_them() {
        let mut st = TuiState { view: View::Fleet, ..Default::default() };
        answered(&mut st, "one", "a", agent_source());
        let first = st.fleet.reply.generation;
        answered(&mut st, "two", "b", agent_source());
        assert_eq!(st.fleet.reply.drawer.turns.iter().map(|t| t.question.as_str()).collect::<Vec<_>>(), vec!["one", "two"]);
        assert!(st.fleet.reply.generation > first, "each ask outdates the previous turn's reads");

        st.fleet.reply.collapse();
        st.fleet.reply.cleared();
        assert!(st.fleet.reply.drawer.turns.is_empty());
        assert!(st.fleet.reply.is_open(), "clearing shows the cleared notice");
        assert_eq!(st.fleet.reply.drawer.notice, Some(ninox_ui_shared::prompt_agent::reply::Notice::ContextCleared));
    }

    #[test]
    fn esc_collapses_an_open_reply_panel_before_closing_an_island() {
        let mut st = TuiState { rows: vec![orchestrator_row("acme")], view: View::Fleet, ..Default::default() };
        st.fleet.sync(&st.rows, &HashMap::new());
        st.selected = 0;
        handle_key(&mut st, code(KeyCode::Enter));
        assert!(st.fleet.nav.open.is_some());
        answered(&mut st, "q", "a", agent_source());

        handle_key(&mut st, code(KeyCode::Esc));
        assert!(!st.fleet.reply.is_open());
        assert!(st.fleet.nav.open.is_some(), "one layer peels at a time");
    }

    #[test]
    fn focused_reply_panel_scrolls_with_j_k_and_pages_within_bounds() {
        let mut st = TuiState { view: View::Fleet, ..Default::default() };
        answered(&mut st, "q", "a", agent_source());
        st.fleet.reply.focused = true;
        st.fleet.reply.max_scroll.set(10);
        st.fleet.reply.drawn_top.set(10);

        handle_key(&mut st, key('k'));
        handle_key(&mut st, key('k'));
        assert_eq!(st.fleet.reply.scroll, Some(8));
        handle_key(&mut st, key('j'));
        assert_eq!(st.fleet.reply.scroll, Some(9));
        handle_key(&mut st, code(KeyCode::PageUp));
        assert_eq!(st.fleet.reply.scroll, Some(1));
        handle_key(&mut st, code(KeyCode::PageUp));
        assert_eq!(st.fleet.reply.scroll, Some(0), "clamped to the first row");
        handle_key(&mut st, key('G'));
        assert_eq!(st.fleet.reply.scroll, Some(10));
        handle_key(&mut st, key('g'));
        assert_eq!(st.fleet.reply.scroll, Some(0));
        handle_key(&mut st, code(KeyCode::PageDown));
        handle_key(&mut st, code(KeyCode::PageDown));
        assert_eq!(st.fleet.reply.scroll, Some(10), "never past the last row");
        st.fleet.reply.scroll = None;

        st.fleet.reply.focused = false;
        handle_key(&mut st, key('k'));
        assert_eq!(st.fleet.reply.scroll, None, "k belongs to the grid when the panel isn't focused");
    }

    #[test]
    fn ctrl_l_from_the_focused_reply_panel_clears_context() {
        let mut st = TuiState { view: View::Fleet, ..Default::default() };
        answered(&mut st, "q", "a", agent_source());
        st.fleet.reply.focused = true;
        let action = handle_key(&mut st, KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL));
        assert_eq!(action, Action::ClearPromptContext);
    }

    #[test]
    fn page_keys_scroll_the_reply_while_typing() {
        let mut st = TuiState { view: View::Fleet, ..Default::default() };
        answered(&mut st, "q", "a", agent_source());
        st.fleet.reply.max_scroll.set(20);
        st.fleet.reply.drawn_top.set(20);
        st.fleet.prompt_editing = true;
        st.fleet.prompt_input = "draft".into();
        handle_key(&mut st, code(KeyCode::PageUp));
        assert!(st.fleet.reply.scroll.is_some_and(|t| t < 20));
        assert_eq!(st.fleet.prompt_input, "draft", "scrolling never touches the draft");
    }

    #[test]
    fn o_opens_a_picked_orchestrators_pane_from_the_reply() {
        let mut st = TuiState { view: View::Fleet, ..Default::default() };
        answered(&mut st, "q", "a", AnswerSource::OrchestratorPane { id: "acme".into(), baseline: String::new() });
        assert_eq!(handle_key(&mut st, key('o')), Action::Connect("acme".into()));
    }

    #[test]
    fn o_never_opens_the_prompt_agent_singleton() {
        let mut st = TuiState { view: View::Fleet, ..Default::default() };
        answered(&mut st, "q", "a", agent_source());
        let action = handle_key(&mut st, key('o'));
        assert_ne!(action, Action::Connect(ninox_core::worker_type::PROMPT_SESSION_ID.into()));
    }

    fn watch(opener: Option<&str>, repo: &str, pr_number: u64) -> PrWatch {
        PrWatch {
            repo: repo.into(),
            pr_number,
            pr_url: format!("https://github.com/{repo}/pull/{pr_number}"),
            opener_session_id: opener.map(Into::into),
            created_at: 0,
        }
    }

    #[test]
    fn orphan_watch_count_matches_by_repo_and_pr_number_not_opener_liveness() {
        let mut owning = Row::test_row("w1");
        owning.session.repo = "acme/web".into();
        owning.session.pr_number = Some(1);
        let rows = vec![owning];

        assert_eq!(
            orphan_watch_count(&rows, &[watch(Some("gone"), "acme/web", 1)]),
            0,
            "a live session already shows this PR, even though its opener is gone"
        );
        assert_eq!(
            orphan_watch_count(&rows, &[watch(Some("w1"), "acme/web", 2)]),
            1,
            "no session shows PR #2, even though the opener session is alive"
        );
        assert_eq!(
            orphan_watch_count(&rows, &[watch(Some("w1"), "other/repo", 1)]),
            1,
            "different repo, same number — not a match"
        );
        assert_eq!(
            orphan_watch_count(&rows, &[watch(Some("gone"), "acme/web", 1), watch(Some("w1"), "acme/web", 2)]),
            1
        );
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
        // Final review Important #3: `toggle_collapse`'s reveal writes
        // `st.selected` directly — the graph must still follow it.
        assert_eq!(st.fleet.nav.selected.as_deref(), Some("o"));
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
        st.rows[1].session.status = SessionStatus::CiFailed;
        st.rows[2].session.activity = ActivityState::Blocked;
        st.rows[3].session.status = SessionStatus::Terminated;
        st.rows[3].session.activity = ActivityState::Blocked;
        assert_eq!(st.needs_rows(), vec![2, 1], "blocked outranks CI-failed; a terminated+blocked row is skipped entirely");
    }

    #[test]
    fn needs_you_matches_member_status_needs_you_exactly_not_mergeable_review_or_unread() {
        // Regression: the pinned block and the header's "N need you" count
        // both read `needs_rows()`, so if it ever drifted from
        // `ninox_overview::status::classify_member`'s `MemberStatus::NeedsYou`
        // (blocked activity or CI failure only), a mergeable/review-pending/
        // unread-message row would show up there and in an island's `!n`
        // tag disagreeing about which rows actually need the user.
        let mut st = grouped(5);
        st.rows[1].session.status = SessionStatus::Mergeable;
        st.rows[2].session.status = SessionStatus::ReviewPending;
        st.msg_counts.insert("w2".into(), 3); // an unread message, no other signal
        st.rows[4].session.status = SessionStatus::Interrupted; // terminal — never needs-you
        st.rows[5].session.activity = ActivityState::Blocked;
        assert_eq!(st.needs_rows(), vec![5], "only the blocked row qualifies");
        assert_eq!(st.needs_you(&st.rows[1]), None, "mergeable is waiting-on-pr, not needs-you");
        assert_eq!(st.needs_you(&st.rows[2]), None, "review-pending is waiting-on-pr, not needs-you");
        assert_eq!(st.needs_you(&st.rows[4]), None, "a terminal status wins over everything, same as classify_member");
    }

    #[test]
    fn clicking_a_group_chevron_folds_it_and_a_pinned_row_selects_its_session() {
        let mut st = grouped(2);
        st.focus = Focus::Pane;
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

    #[test]
    fn clicking_a_different_sidebar_row_keeps_the_pane_split_view() {
        let mut st = state_with_rows(2);
        st.selected = 0;
        st.focus = Focus::Pane;
        st.layout = crate::tui::layout::compute(Rect::new(0, 0, 120, 37), &st);
        let list = st.layout.sidebar_list.expect("a wide terminal splits sidebar + pane even while pane-focused");
        let header = st.layout.sidebar_items.iter().position(|i| *i == SideItem::Session { row: 1, pinned: false }).unwrap();
        handle_mouse(&mut st, down(list.x, list.y + header as u16));
        assert_eq!(st.selected, 1);
        assert_eq!(st.focus, Focus::Pane, "selecting from the sidebar must not tear down the pane-split view");
    }

    fn viewer_info(session: &str, alive: bool) -> PaneInfo {
        pane_info(&ninox_core::runtime::viewer_pane_id(std::process::id(), session), alive)
    }

    #[test]
    fn c_on_a_tmux_row_opens_it_in_the_pane_and_wants_a_viewer() {
        // `c` is Fleet's pane-attach key — `Enter` now opens the tree
        // selection's island instead (see `enter_opens_the_tree_selections_
        // owning_island`), so attaching moved off bare Enter onto `c`.
        let mut st = state_with_rows(2);
        st.selected = 1;
        assert_eq!(handle_key(&mut st, key('c')), Action::None, "no full-screen attach");
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
        assert_eq!(handle_key(&mut st, key('c')), Action::Connect("s0".into()));
        st.viewers_unavailable = None;
        assert_eq!(handle_key(&mut st, key('c')), Action::None);
        // Explicit full-screen attach stays one chord away.
        handle_key(&mut st, prefix());
        assert_eq!(handle_key(&mut st, key('a')), Action::Connect("s0".into()));
    }

    #[test]
    fn a_closed_viewer_is_not_respawned_until_the_row_is_reopened() {
        let mut st = state_with_rows(1);
        st.viewer_errors.insert("s0".into(), "tmux view closed".into());
        assert!(st.wanted_viewers().is_empty());
        handle_key(&mut st, key('c'));
        assert_eq!(st.wanted_viewers(), ["s0"]);
        st.rows[0].session.status = SessionStatus::Terminated;
        assert!(st.wanted_viewers().is_empty(), "a dead session gets no viewer");
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
        assert_eq!(st.view, View::Fleet);

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
        assert_eq!(handle_key(&mut st, key('5')), Action::LoadWorkerTypes);
        assert_eq!(handle_key(&mut st, key('2')), Action::None);
        assert_eq!(st.view, View::Board);
        assert_eq!(handle_key(&mut st, key('3')), Action::None);
        assert_eq!(st.view, View::Plans);
        handle_key(&mut st, key('1'));
        assert_eq!(st.view, View::Fleet);

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
    fn c_on_a_ptyd_row_focuses_its_pane_and_keys_flow_to_it() {
        let mut st = state_with_rows(1);
        with_live_pane(&mut st, "s0");
        assert_eq!(handle_key(&mut st, key('c')), Action::None);
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
    fn settings_opens_as_a_modal_not_a_tab() {
        let mut st = state_with_rows(1);
        let before = st.view;
        handle_key(&mut st, prefix());
        let action = handle_key(&mut st, key('s'));
        assert_eq!(st.modal, Some(Modal::Settings), "s opens the settings modal");
        assert_eq!(st.view, before, "settings no longer steals the view");
        assert_eq!(action, Action::LoadSettings);
        // Esc with no field being edited closes it.
        handle_key(&mut st, code(KeyCode::Esc));
        assert_eq!(st.modal, None);
    }

    fn settings_page() -> TuiState {
        let _iso = ninox_core::test_isolation::isolate();
        let mut st = state_with_rows(1);
        st.settings.reload(Ok(Default::default()));
        open_settings(&mut st);
        st.layout = crate::tui::layout::compute(Rect::new(0, 0, 80, 24), &st);
        st
    }

    fn settings_hit(st: &TuiState, pred: impl Fn(&crate::tui::layout::SettingsBarLine) -> bool) -> (u16, u16) {
        let sl = crate::tui::layout::settings(crate::tui::layout::settings_area(Rect::new(0, 0, 80, 24)), st);
        let k = sl.bar_lines.iter().position(pred).expect("bar row");
        (sl.bar.x + 3, sl.bar.y + (k - sl.bar_offset) as u16)
    }

    #[test]
    fn settings_arrows_move_the_section_bar_until_a_field_pane_is_entered() {
        use ninox_ui_shared::settings_form::SettingsSection as S;
        let mut st = settings_page();
        assert_eq!((st.settings.section, st.settings.in_fields), (S::Appearance, false));
        handle_key(&mut st, code(KeyCode::Down));
        assert_eq!(st.settings.section, S::Agents);
        assert_eq!(st.settings.selected_field().map(|f| f.id.clone()), Some(ninox_ui_shared::settings_form::FieldId::OrchHarness), "a section's first field is selected");
        for _ in 0..20 {
            handle_key(&mut st, code(KeyCode::Down));
        }
        assert_eq!(st.settings.section, S::Version, "the bar stops at its last section");
        handle_key(&mut st, code(KeyCode::Home));
        assert_eq!(st.settings.section, S::Appearance);
    }

    #[test]
    fn settings_tab_and_enter_move_between_the_bar_and_the_fields_and_esc_steps_back() {
        use ninox_ui_shared::settings_form::SettingsSection as S;
        let mut st = settings_page();
        handle_key(&mut st, code(KeyCode::Down));
        handle_key(&mut st, code(KeyCode::Tab));
        assert!(st.settings.in_fields);
        handle_key(&mut st, code(KeyCode::Down));
        assert_eq!(st.settings.selected_field().map(|f| f.label.as_str()), Some("Orchestrator model"));
        handle_key(&mut st, code(KeyCode::Down));
        assert_eq!(st.settings.section, S::Agents, "↓ in the fields never changes section");
        handle_key(&mut st, code(KeyCode::Esc));
        assert!(!st.settings.in_fields);
        assert_eq!(st.modal, Some(Modal::Settings), "esc from the fields only goes back to the bar");
        handle_key(&mut st, code(KeyCode::Right));
        assert!(st.settings.in_fields, "→ enters the fields");
        handle_key(&mut st, code(KeyCode::BackTab));
        assert!(!st.settings.in_fields);
        handle_key(&mut st, code(KeyCode::Esc));
        assert_eq!(st.modal, None, "esc from the bar closes");
    }

    #[test]
    fn settings_left_on_a_field_with_nothing_to_cycle_returns_to_the_bar() {
        use ninox_ui_shared::settings_form::SettingsSection as S;
        let mut st = settings_page();
        st.settings.select_section(S::Engine);
        handle_key(&mut st, code(KeyCode::Tab));
        assert_eq!(st.settings.selected_field().map(|f| f.label.as_str()), Some("Port"));
        handle_key(&mut st, code(KeyCode::Left));
        assert!(!st.settings.in_fields);
    }

    #[test]
    fn settings_esc_mid_edit_cancels_the_edit_not_the_page() {
        use ninox_ui_shared::settings_form::SettingsSection as S;
        let mut st = settings_page();
        st.settings.select_section(S::Engine);
        handle_key(&mut st, code(KeyCode::Tab));
        handle_key(&mut st, code(KeyCode::Enter));
        assert!(st.settings.editing.is_some());
        handle_key(&mut st, code(KeyCode::Esc));
        assert!(st.settings.editing.is_none());
        assert!(st.settings.in_fields && st.modal == Some(Modal::Settings));
    }

    #[test]
    fn settings_question_mark_and_restart_server_open_their_modals() {
        use ninox_ui_shared::settings_form::SettingsSection as S;
        let mut st = settings_page();
        handle_key(&mut st, key('?'));
        assert_eq!(st.modal, Some(Modal::Help));
        st.modal = Some(Modal::Settings);
        st.settings.select_section(S::Engine);
        handle_key(&mut st, code(KeyCode::Tab));
        handle_key(&mut st, code(KeyCode::Down));
        handle_key(&mut st, code(KeyCode::Enter));
        assert!(matches!(st.modal, Some(Modal::Confirm(Pending::RestartServer))), "{:?}", st.modal);
    }

    #[test]
    fn opening_the_editor_by_mouse_mid_edit_drops_the_draft() {
        use ninox_ui_shared::settings_form::SettingsSection as S;
        let mut st = settings_page();
        st.settings.select_section(S::Engine);
        handle_key(&mut st, code(KeyCode::Tab));
        handle_key(&mut st, code(KeyCode::Enter));
        let sl = crate::tui::layout::settings(crate::tui::layout::settings_area(Rect::new(0, 0, 80, 24)), &st);
        assert_eq!(handle_mouse(&mut st, down(sl.open.x + 1, sl.open.y)), Action::EditConfig);
        assert!(st.settings.editing.is_none());
    }

    #[test]
    fn closing_by_mouse_mid_edit_drops_the_edit() {
        use ninox_ui_shared::settings_form::SettingsSection as S;
        let mut st = settings_page();
        st.settings.select_section(S::Engine);
        handle_key(&mut st, code(KeyCode::Tab));
        handle_key(&mut st, code(KeyCode::Enter));
        let sl = crate::tui::layout::settings(crate::tui::layout::settings_area(Rect::new(0, 0, 80, 24)), &st);
        handle_mouse(&mut st, down(sl.close.x + 1, sl.close.y));
        open_settings(&mut st);
        assert!(st.settings.editing.is_none());
    }

    #[test]
    fn settings_reopens_on_the_bar() {
        let mut st = settings_page();
        handle_key(&mut st, code(KeyCode::Tab));
        handle_key(&mut st, key('q'));
        assert_eq!(st.modal, None);
        open_settings(&mut st);
        assert!(!st.settings.in_fields);
    }

    #[test]
    fn clicking_a_section_selects_it_and_a_field_value_changes_it() {
        use ninox_ui_shared::settings_form::SettingsSection as S;
        let mut st = settings_page();
        let (x, y) = settings_hit(&st, |l| *l == crate::tui::layout::SettingsBarLine::Section(S::Runtime));
        assert_eq!(handle_mouse(&mut st, down(x, y)), Action::None);
        assert_eq!(st.settings.section, S::Runtime);
        let sl = crate::tui::layout::settings(crate::tui::layout::settings_area(Rect::new(0, 0, 80, 24)), &st);
        let row = sl.lines.iter().position(|l| matches!(l, crate::tui::layout::SettingsLine::Field(_))).unwrap() as u16;
        let label = handle_mouse(&mut st, down(sl.list.x + 3, sl.list.y + row));
        assert_eq!((label, st.settings.in_fields), (Action::None, true), "a click on the label only selects");
        let action = handle_mouse(&mut st, down(sl.value_x, sl.list.y + row));
        assert!(matches!(action, Action::SaveSetting(Change::Step { back: false, .. })), "{action:?}");
    }

    #[test]
    fn clicking_close_or_the_header_entry_closes_and_opens_the_page() {
        let mut st = settings_page();
        let sl = crate::tui::layout::settings(crate::tui::layout::settings_area(Rect::new(0, 0, 80, 24)), &st);
        handle_mouse(&mut st, down(sl.close.x + 1, sl.close.y));
        assert_eq!(st.modal, None);
        let b = st.layout.settings_button.expect("the header has a settings entry at 80 columns");
        assert_eq!(handle_mouse(&mut st, down(b.x + 1, b.y)), Action::LoadSettings);
        assert_eq!(st.modal, Some(Modal::Settings));
    }

    #[test]
    fn settings_wheel_scrolls_the_bar_and_the_fields_separately() {
        use ninox_ui_shared::settings_form::SettingsSection as S;
        let mut st = settings_page();
        let sl = crate::tui::layout::settings(crate::tui::layout::settings_area(Rect::new(0, 0, 80, 24)), &st);
        let wheel = |x, y| MouseEvent { kind: MouseEventKind::ScrollDown, column: x, row: y, modifiers: KeyModifiers::NONE };
        handle_mouse(&mut st, wheel(sl.bar.x + 2, sl.bar.y + 1));
        assert_eq!(st.settings.section, S::Agents);
        let before = st.settings.selected;
        handle_mouse(&mut st, wheel(sl.list.x + 2, sl.list.y));
        assert!(st.settings.selected > before);
    }

    #[test]
    fn closing_settings_returns_to_the_view_it_was_opened_from() {
        let mut st = state_with_rows(1);
        st.view = View::Brain;
        handle_key(&mut st, prefix());
        handle_key(&mut st, key('s'));
        assert_eq!(st.modal, Some(Modal::Settings));
        handle_key(&mut st, code(KeyCode::Esc));
        assert_eq!(st.modal, None);
        assert_eq!(st.view, View::Brain, "closing settings must not jump to Fleet");
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

    fn delete_plan(dirty: bool) -> ninox_core::events::DeletePlan {
        use ninox_core::events::{DeletePlan, DeleteTarget, WorktreeState};
        let target = |id: &str, orch: bool, depth: usize, tree_index: usize| DeleteTarget {
            id: id.into(), name: id.into(), is_orchestrator: orch, running: true, resumable: false, depth, tree_index,
            worktree: (!orch).then(|| WorktreeState { path: format!("/r/.claude/worktrees/{id}"), uncommitted: dirty, stash: false, unpushed: false }),
        };
        DeletePlan { root: "s0".into(), targets: vec![target("w1", false, 1, 1), target("s0", true, 0, 0)] }
    }

    fn shift_d() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('D'), KeyModifiers::SHIFT)
    }

    fn delete_confirm(st: &TuiState) -> &DeleteConfirm {
        match &st.modal {
            Some(Modal::Delete(d)) => d,
            other => panic!("expected the delete confirm, got {other:?}"),
        }
    }

    #[test]
    fn d_opens_the_delete_confirm_and_y_runs_the_planned_ids_unforced() {
        let mut st = state_with_rows(1);
        assert_eq!(handle_key(&mut st, shift_d()), Action::None);
        assert_eq!(delete_confirm(&st).root, "s0");
        assert_eq!(handle_key(&mut st, key('y')), Action::None, "nothing runs before the plan is read");
        if let Some(Modal::Delete(d)) = st.modal.as_mut() {
            d.plan = Some(Ok(delete_plan(false)));
        }
        assert_eq!(handle_key(&mut st, key('f')), Action::None);
        assert!(!delete_confirm(&st).force, "no override to offer when nothing is at risk");
        let Action::Delete { plan, force } = handle_key(&mut st, key('y')) else { panic!("expected a delete") };
        let ids: Vec<&str> = plan.targets.iter().map(|t| t.id.as_str()).collect();
        assert_eq!((ids, force), (vec!["w1", "s0"], false));
        assert!(st.modal.is_none());
    }

    #[test]
    fn f_ticks_the_worktree_override_for_a_risky_plan() {
        let mut st = state_with_rows(1);
        handle_key(&mut st, shift_d());
        if let Some(Modal::Delete(d)) = st.modal.as_mut() {
            d.plan = Some(Ok(delete_plan(true)));
        }
        handle_key(&mut st, key('f'));
        assert!(matches!(handle_key(&mut st, key('y')), Action::Delete { force: true, .. }));
    }

    #[test]
    fn esc_cancels_the_delete_confirm() {
        let mut st = state_with_rows(1);
        handle_key(&mut st, shift_d());
        handle_key(&mut st, key('e'));
        assert!(delete_confirm(&st).expanded);
        assert_eq!(handle_key(&mut st, code(KeyCode::Esc)), Action::None);
        assert!(st.modal.is_none());
    }

    #[test]
    fn the_built_in_session_has_no_delete() {
        let mut st = TuiState { rows: vec![Row::test_row(ninox_core::worker_type::PROMPT_SESSION_ID)], ..Default::default() };
        handle_key(&mut st, shift_d());
        assert!(st.modal.is_none());
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn ctrl_shift_r() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('R'), KeyModifiers::CONTROL | KeyModifiers::SHIFT)
    }

    #[test]
    fn restart_all_requires_confirmation() {
        let mut st = state_with_rows(2);
        assert_eq!(handle_key(&mut st, ctrl_shift_r()), Action::None);
        assert_eq!(st.modal, Some(Modal::Confirm(Pending::RestartAll)));
        assert_eq!(handle_key(&mut st, key('n')), Action::None, "n cancels");
        assert!(st.modal.is_none());

        assert_eq!(handle_key(&mut st, ctrl_shift_r()), Action::None);
        assert_eq!(handle_key(&mut st, key('y')), Action::RestartAll);
        assert!(st.modal.is_none());
    }

    #[test]
    fn restart_server_from_settings_needs_a_confirm() {
        let mut st = state_with_rows(1);
        st.settings.fields = super::super::settings::fields(&ninox_core::config::AppConfig::default());
        st.settings.selected = st.settings.fields.iter()
            .position(|f| f.id == super::super::settings::FieldId::RestartServer)
            .expect("Settings lists Restart server");
        assert_eq!(activate_setting(&mut st), Action::None);
        assert_eq!(st.modal, Some(Modal::Confirm(Pending::RestartServer)));
        assert_eq!(handle_key(&mut st, key('n')), Action::None, "n cancels");
        assert!(st.modal.is_none());

        activate_setting(&mut st);
        assert_eq!(handle_key(&mut st, key('y')), Action::RestartServer);
        let (q, detail) = st.confirm_text(&Pending::RestartServer);
        assert!(st.login_error_sessions().is_empty());
        assert!(q.contains("Restart the ninox server"), "{q}");
        assert!(detail.contains("sessions keep running"), "{detail}");
    }

    #[test]
    fn sessions_with_a_login_error_relaunch_after_a_confirm() {
        let mut st = state_with_rows(1);
        st.settings.fields = super::super::settings::fields(&ninox_core::config::AppConfig::default());
        st.settings.selected = st.settings.fields.iter()
            .position(|f| f.id == super::super::settings::FieldId::RestartAffected)
            .unwrap();
        assert_eq!(activate_setting(&mut st), Action::None);
        assert!(st.modal.is_none(), "nothing to relaunch without a login error");

        let issue = ninox_core::server_control::ServiceIssue {
            source: "harness_auth:claude-code".into(),
            message: "Invalid API key".into(),
            sessions: vec!["w2".into(), "w1".into()],
        };
        st.server_alerts = vec![ninox_ui_shared::server_restart::classify(&issue).unwrap()];
        activate_setting(&mut st);
        let ids = vec!["w1".to_string(), "w2".to_string()];
        assert_eq!(st.modal, Some(Modal::Confirm(Pending::RestartAffected(ids.clone()))));
        assert_eq!(handle_key(&mut st, key('y')), Action::RestartAffected(ids));
    }

    #[test]
    fn restart_all_with_no_live_agents_skips_the_confirm() {
        let mut st = state_with_rows(1);
        st.rows[0].session.status = SessionStatus::Done;
        assert_eq!(handle_key(&mut st, ctrl_shift_r()), Action::None);
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
    fn e_opens_the_selected_sessions_workspace_or_says_why_not() {
        let mut st = state_with_rows(1);
        assert_eq!(handle_key(&mut st, key('e')), Action::None, "no recorded workspace yet");
        assert!(st.notice.as_ref().is_some_and(|n| n.text.contains("no recorded workspace")));
        st.rows[0].session.workspace_path = Some("/work/s0".into());
        assert_eq!(handle_key(&mut st, key('e')), Action::OpenInEditor("s0".into()));
    }

    #[test]
    fn t_opens_a_message_composer_for_the_selected_session_and_enter_sends_it() {
        let mut st = state_with_rows(2);
        st.selected = 1;
        assert_eq!(handle_key(&mut st, key('t')), Action::None);
        assert!(matches!(&st.modal, Some(Modal::Message(m)) if m.id == "s1" && m.draft.is_empty()), "{:?}", st.modal);
        for c in "hi there".chars() {
            handle_key(&mut st, key(c));
        }
        handle_key(&mut st, code(KeyCode::Backspace));
        assert_eq!(handle_key(&mut st, code(KeyCode::Enter)), Action::SendMessage { id: "s1".into(), message: "hi ther".into() });
        assert_eq!(st.modal, None);
    }

    #[test]
    fn the_message_composer_takes_command_letters_as_text_and_esc_cancels() {
        let mut st = state_with_rows(1);
        handle_key(&mut st, key('t'));
        for c in "qxtR ".chars() {
            assert_eq!(handle_key(&mut st, KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)), Action::None);
        }
        assert!(matches!(&st.modal, Some(Modal::Message(m)) if m.draft == "qxtR "), "{:?}", st.modal);
        let mut blank = state_with_rows(1);
        handle_key(&mut blank, key('t'));
        handle_key(&mut blank, key(' '));
        assert_eq!(handle_key(&mut blank, code(KeyCode::Enter)), Action::None, "a blank draft never sends");
        assert!(matches!(blank.modal, Some(Modal::Message(_))), "and stays open for typing");
        assert_eq!(handle_key(&mut st, code(KeyCode::Esc)), Action::None);
        assert_eq!(st.modal, None);
    }

    #[test]
    fn the_message_composer_accepts_pastes_and_option_typed_characters() {
        let mut st = state_with_rows(1);
        handle_key(&mut st, key('t'));
        assert_eq!(handle_paste(&mut st, "see the log:\nE0599"), Action::None);
        handle_key(&mut st, KeyEvent::new(KeyCode::Char('@'), KeyModifiers::ALT));
        assert_eq!(
            handle_key(&mut st, code(KeyCode::Enter)),
            Action::SendMessage { id: "s0".into(), message: "see the log:\nE0599@".into() },
        );
    }

    fn restartable(n: usize) -> TuiState {
        let mut st = state_with_rows(n);
        for r in &mut st.rows {
            r.session.workspace_path = Some(format!("/work/{}", r.session.id));
            r.session.claude_session_id = Some(format!("conv-{}", r.session.id));
        }
        st
    }

    #[test]
    fn ctrl_r_without_a_conversation_to_resume_says_why_instead_of_confirming() {
        let mut st = state_with_rows(1);
        handle_key(&mut st, ctrl('r'));
        assert_eq!(st.modal, None);
        assert!(st.notice.as_ref().is_some_and(|n| n.text.contains("can't be restarted")), "{:?}", st.notice);
    }

    #[test]
    fn clicking_restart_all_in_the_restart_confirm_restarts_all() {
        let mut st = restartable(2);
        handle_key(&mut st, ctrl('r'));
        st.layout = crate::tui::layout::compute(Rect::new(0, 0, 160, 40), &st);
        let all = st.layout.modal_all.expect("a Restart all button");
        assert_eq!(handle_mouse(&mut st, down(all.x + 1, all.y)), Action::RestartAll);
        assert_eq!(st.modal, None);
    }

    #[test]
    fn ctrl_r_restarts_just_the_selected_session_after_a_confirm() {
        let mut st = restartable(2);
        st.selected = 1;
        handle_key(&mut st, ctrl('r'));
        assert_eq!(st.modal, Some(Modal::Confirm(Pending::Restart("s1".into()))));
        assert_eq!(handle_key(&mut st, key('y')), Action::Resume("s1".into()));
        handle_key(&mut st, ctrl('r'));
        assert_eq!(handle_key(&mut st, key('n')), Action::None);
        assert_eq!(st.modal, None);
    }

    #[test]
    fn the_restart_confirm_offers_restart_all_for_terminals_without_ctrl_shift_r() {
        let mut st = restartable(2);
        handle_key(&mut st, ctrl('r'));
        assert_eq!(st.confirm_all_label(&Pending::Restart("s0".into())), Some("Restart all"));
        assert_eq!(handle_key(&mut st, key('a')), Action::RestartAll);
    }

    #[test]
    fn ctrl_shift_r_asks_to_restart_every_live_agent() {
        let mut st = state_with_rows(2);
        handle_key(&mut st, ctrl_shift_r());
        assert_eq!(st.modal, Some(Modal::Confirm(Pending::RestartAll)));
        assert_eq!(handle_key(&mut st, key('y')), Action::RestartAll);
    }

    #[test]
    fn shift_f_re_files_the_selection_after_a_confirm_or_says_why_not() {
        let mut st = state_with_rows(1);
        handle_key(&mut st, KeyEvent::new(KeyCode::Char('F'), KeyModifiers::SHIFT));
        assert_eq!(st.modal, None, "no recorded workspace to respawn into");
        assert!(st.notice.as_ref().is_some_and(|n| n.text.contains("no recorded workspace")), "{:?}", st.notice);
        st.rows[0].session.workspace_path = Some("/work/s0".into());
        handle_key(&mut st, KeyEvent::new(KeyCode::Char('F'), KeyModifiers::SHIFT));
        assert_eq!(st.modal, Some(Modal::Confirm(Pending::Refile("s0".into()))));
        assert_eq!(st.confirm_all_label(&Pending::Refile("s0".into())), None);
        assert_eq!(handle_key(&mut st, key('y')), Action::Refile("s0".into()));
    }

    fn inspecting() -> TuiState {
        let mut st = state_with_rows(1);
        st.view = View::Fleet;
        st.inspector = true;
        st.focus = Focus::Pane;
        st.layout.inspector = Some(Rect::new(80, 2, 42, 30));
        st
    }

    #[test]
    fn prefix_tab_from_the_pane_reaches_the_inspector_then_the_sidebar() {
        let mut st = inspecting();
        handle_key(&mut st, prefix());
        handle_key(&mut st, code(KeyCode::Tab));
        assert_eq!(st.focus, Focus::Inspector);
        handle_key(&mut st, code(KeyCode::Tab));
        assert_eq!(st.focus, Focus::Sidebar);
        st.layout.inspector = None;
        st.focus = Focus::Pane;
        handle_key(&mut st, prefix());
        handle_key(&mut st, code(KeyCode::Tab));
        assert_eq!(st.focus, Focus::Sidebar, "no inspector showing: the old pane ↔ sidebar toggle");
    }

    #[test]
    fn the_focused_inspector_switches_tabs_and_scrolls_its_info_within_bounds() {
        let mut st = inspecting();
        st.focus = Focus::Inspector;
        st.layout.inspector_max_scroll = 3;
        for _ in 0..5 {
            handle_key(&mut st, key('j'));
        }
        assert_eq!(st.inspector_scroll, 3);
        handle_key(&mut st, key('g'));
        assert_eq!(st.inspector_scroll, 0);
        handle_key(&mut st, key('G'));
        assert_eq!(st.inspector_scroll, 3);
        handle_key(&mut st, key('l'));
        assert_eq!(st.inspector_tab, crate::tui::inspector::Tab::Diff);
        handle_key(&mut st, code(KeyCode::Left));
        assert_eq!(st.inspector_tab, crate::tui::inspector::Tab::Info);
        assert_eq!(handle_key(&mut st, key('q')), Action::Quit, "anything else is still a command");
    }

    #[test]
    fn the_diff_tab_walks_files_opens_one_and_esc_peels_back_out() {
        let mut st = inspecting();
        st.focus = Focus::Inspector;
        st.inspector_tab = crate::tui::inspector::Tab::Diff;
        st.diff.begin("s0");
        st.diff.finish("s0", Ok(Some("diff --git a/x b/x\n@@ -1 +1 @@\n-a\n+b\ndiff --git a/y b/y\n")), 1);
        handle_key(&mut st, key('j'));
        handle_key(&mut st, key('j'));
        assert_eq!(st.diff.selected, 1, "clamped to the last file");
        handle_key(&mut st, key('k'));
        handle_key(&mut st, code(KeyCode::Enter));
        assert_eq!(st.diff.open, Some(0));
        st.layout.inspector_max_scroll = 2;
        handle_key(&mut st, key('j'));
        handle_key(&mut st, code(KeyCode::PageDown));
        assert_eq!(st.diff.scroll, 2);
        handle_key(&mut st, code(KeyCode::Esc));
        assert_eq!((st.diff.open, st.diff.scroll), (None, 0));
        handle_key(&mut st, key('r'));
        assert!(st.diff.due("s0", 2), "r refetches now");
        handle_key(&mut st, code(KeyCode::Esc));
        assert_eq!(st.focus, Focus::Pane);
    }

    #[test]
    fn chords_in_the_inspector_stay_commands_and_r_never_resumes() {
        let mut st = inspecting();
        for r in &mut st.rows {
            r.session.workspace_path = Some("/work/s0".into());
            r.session.claude_session_id = Some("conv".into());
        }
        st.focus = Focus::Inspector;
        st.inspector_tab = crate::tui::inspector::Tab::Diff;
        handle_key(&mut st, ctrl('r'));
        assert_eq!(st.modal, Some(Modal::Confirm(Pending::Restart("s0".into()))), "ctrl+r is still restart");
        st.modal = None;
        st.inspector_tab = crate::tui::inspector::Tab::Info;
        assert_eq!(handle_key(&mut st, key('r')), Action::None, "r on Info doesn't resume");
    }

    #[test]
    fn hiding_the_inspector_hands_focus_back_to_the_pane() {
        let mut st = inspecting();
        st.focus = Focus::Inspector;
        handle_key(&mut st, key('i'));
        assert!(!st.inspector);
        assert_eq!(st.focus, Focus::Pane);
    }

    fn note(id: &str, session: Option<&str>, kind: ninox_core::types::NotificationKind) -> ninox_core::types::Notification {
        ninox_core::types::Notification {
            id: id.into(),
            kind,
            title: format!("title {id}"),
            body: format!("body {id}"),
            session_id: session.map(str::to_string),
            created_at: 0,
        }
    }

    #[test]
    fn new_notifications_toast_by_tone_and_the_list_is_capped_oldest_first_out() {
        use ninox_core::types::NotificationKind as K;
        let mut st = state_with_rows(1);
        st.push_notification(note("ci", None, K::CiFailure));
        assert!(st.notice.as_ref().is_some_and(|n| n.text.contains("title ci") && n.level == Level::Error), "{:?}", st.notice);
        for i in 0..MAX_NOTIFICATIONS {
            st.push_notification(note(&format!("n{i}"), None, K::WorkerDone));
        }
        assert_eq!(st.notifications.len(), MAX_NOTIFICATIONS);
        assert!(st.notifications.iter().all(|n| n.id != "ci"), "the oldest one dropped");
        assert_eq!(st.notice.as_ref().map(|n| n.level), Some(Level::Error), "the Info arrivals didn't hide the error toast");
    }

    #[test]
    fn store_notifications_fill_the_list_quietly_at_first_then_toast_new_ones_once() {
        use ninox_core::types::NotificationKind as K;
        let _iso = ninox_core::test_isolation::isolate();
        let store = ninox_core::store::Store::open(":memory:").unwrap();
        store.insert_notification(&note("old", None, K::CiFailure)).unwrap();
        let mut st = state_with_rows(1);
        assert!(st.sync_notifications(&store));
        assert_eq!(st.notifications.len(), 1);
        assert!(st.notice.is_none(), "the backlog doesn't toast");
        assert!(!st.sync_notifications(&store), "a re-read changes nothing");
        store.insert_notification(&note("new", None, K::WorkerDone)).unwrap();
        assert!(st.sync_notifications(&store));
        assert!(st.notice.as_ref().is_some_and(|n| n.text.contains("title new")));
        assert_eq!(st.notifications.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(), ["old", "new"]);
        assert!(!st.sync_notifications(&store));
    }

    #[test]
    fn modal_dismissals_persist_to_the_store_and_dismissals_elsewhere_drop_rows() {
        use ninox_core::types::NotificationKind as K;
        let _iso = ninox_core::test_isolation::isolate();
        let store = ninox_core::store::Store::open(":memory:").unwrap();
        for id in ["a", "b", "c"] {
            store.insert_notification(&note(id, None, K::WorkerDone)).unwrap();
        }
        let mut st = state_with_rows(1);
        st.sync_notifications(&store);
        store.dismiss_notification("a").unwrap();
        assert!(st.sync_notifications(&store));
        assert_eq!(st.notifications.len(), 2);
        handle_key(&mut st, key('N'));
        handle_key(&mut st, key('d'));
        st.sync_notifications(&store);
        assert_eq!(store.undismissed_notifications().unwrap().len(), 1);
        handle_key(&mut st, key('D'));
        st.sync_notifications(&store);
        assert!(store.undismissed_notifications().unwrap().is_empty());
        assert!(st.notifications.is_empty());
    }

    #[test]
    fn shift_n_lists_newest_first_and_d_dismisses_one_and_shift_d_all() {
        use ninox_core::types::NotificationKind as K;
        let mut st = state_with_rows(1);
        for id in ["a", "b", "c"] {
            st.push_notification(note(id, None, K::WorkerDone));
        }
        handle_key(&mut st, KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT));
        assert_eq!(st.modal, Some(Modal::Notifications(0)));
        assert_eq!(st.notification_at(0).map(|n| n.id.as_str()), Some("c"), "newest first");
        handle_key(&mut st, key('j'));
        handle_key(&mut st, key('j'));
        handle_key(&mut st, key('j'));
        assert_eq!(st.modal, Some(Modal::Notifications(2)), "clamped to the last");
        handle_key(&mut st, key('d'));
        assert_eq!(st.notifications.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(), vec!["b", "c"], "the oldest, under the cursor, went");
        assert_eq!(st.modal, Some(Modal::Notifications(1)), "the cursor stays in range");
        handle_key(&mut st, KeyEvent::new(KeyCode::Char('D'), KeyModifiers::SHIFT));
        assert!(st.notifications.is_empty());
        assert_eq!(st.modal, Some(Modal::Notifications(0)), "stays open, now empty");
        handle_key(&mut st, code(KeyCode::Esc));
        assert_eq!(st.modal, None);
    }

    #[test]
    fn enter_on_a_disk_pressure_notification_opens_the_workspaces_prune_preview() {
        use ninox_core::types::NotificationKind as K;
        let mut st = state_with_rows(1);
        st.push_notification(note("disk", None, K::DiskPressure));
        handle_key(&mut st, KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT));
        handle_key(&mut st, code(KeyCode::Enter));
        assert_eq!(st.modal, None);
        assert_eq!(st.view, View::Remotes);
        assert!(st.workspaces.open && st.workspaces.preview);
    }

    #[test]
    fn an_arrival_while_the_list_is_open_keeps_the_cursor_on_the_same_notification() {
        use ninox_core::types::NotificationKind as K;
        let mut st = state_with_rows(1);
        st.push_notification(note("ci", None, K::CiFailure));
        handle_key(&mut st, KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT));
        st.push_notification(note("done", None, K::WorkerDone));
        let Some(Modal::Notifications(cur)) = st.modal.clone() else { panic!("{:?}", st.modal) };
        assert_eq!(st.notification_at(cur).map(|n| n.id.as_str()), Some("ci"));
        handle_key(&mut st, key('d'));
        assert_eq!(st.notifications.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(), vec!["done"]);
    }

    #[test]
    fn a_background_notification_never_hides_a_more_severe_notice_still_showing() {
        use ninox_core::types::NotificationKind as K;
        let mut st = state_with_rows(1);
        st.notify(Level::Error, "spawn failed: no such repo");
        st.push_notification(note("done", None, K::WorkerDone));
        assert_eq!(st.notice.as_ref().map(|n| n.text.as_str()), Some("spawn failed: no such repo"));
        st.push_notification(note("ci", None, K::CiFailure));
        assert_eq!(st.notice.as_ref().map(|n| n.text.as_str()), Some("title ci"), "an equally severe one replaces it");
        assert_eq!(st.notifications.len(), 2, "both are still listed");
    }

    #[test]
    fn enter_on_a_notification_goes_to_its_session_and_does_nothing_without_one() {
        use ninox_core::types::NotificationKind as K;
        let mut st = state_with_rows(2);
        st.push_notification(note("s", Some("s1"), K::AgentStuck));
        st.push_notification(note("u", None, K::UpdateAvailable));
        handle_key(&mut st, KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT));
        assert_eq!(handle_key(&mut st, code(KeyCode::Enter)), Action::None);
        assert_eq!(st.modal, Some(Modal::Notifications(0)), "no session to go to: stay");
        handle_key(&mut st, key('j'));
        handle_key(&mut st, code(KeyCode::Enter));
        assert_eq!(st.modal, None);
        assert_eq!(st.focus, Focus::Pane, "the same open path as Goto's Enter");
        assert_eq!((st.view, st.selected_id().as_deref()), (View::Fleet, Some("s1")));

        st.push_notification(note("gone", Some("reaped"), K::WorkerDone));
        st.focus = Focus::Sidebar;
        handle_key(&mut st, KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT));
        assert_eq!(handle_key(&mut st, code(KeyCode::Enter)), Action::None);
        assert_eq!(st.modal, Some(Modal::Notifications(0)), "stays open");
        assert!(st.notice.as_ref().is_some_and(|n| n.text.contains("no longer in the fleet")), "{:?}", st.notice);
    }

    #[test]
    fn the_help_modal_scrolls_with_j_k_and_any_other_key_still_closes_it() {
        let mut st = state_with_rows(1);
        handle_key(&mut st, key('?'));
        assert_eq!(st.modal, Some(Modal::Help));
        st.layout.help_max_scroll = 2;
        for _ in 0..4 {
            handle_key(&mut st, key('j'));
        }
        assert_eq!((st.modal.clone(), st.help_scroll), (Some(Modal::Help), 2), "clamped, still open");
        handle_key(&mut st, code(KeyCode::Home));
        assert_eq!(st.help_scroll, 0);
        handle_key(&mut st, key('x'));
        assert_eq!(st.modal, None);
        handle_key(&mut st, key('?'));
        assert_eq!(st.help_scroll, 0, "reopens at the top");
    }

    #[test]
    fn help_that_fits_still_closes_on_any_key_and_a_click_or_wheel_behaves() {
        let mut st = state_with_rows(1);
        handle_key(&mut st, key('?'));
        handle_key(&mut st, key('j'));
        assert_eq!(st.modal, None, "nothing to scroll: j closes it like any other key");

        handle_key(&mut st, key('?'));
        st.layout.help_max_scroll = 10;
        handle_mouse(&mut st, mouse(MouseEventKind::ScrollDown, 5, 5));
        assert_eq!((st.modal.clone(), st.help_scroll), (Some(Modal::Help), 3), "the wheel scrolls help");
        handle_mouse(&mut st, down(5, 5));
        assert_eq!((st.modal.clone(), st.help_scroll), (None, 0), "a click closes it, back at the top");

        handle_key(&mut st, key('?'));
        st.help_scroll = 20;
        st.layout.help_max_scroll = 5;
        handle_key(&mut st, key('k'));
        assert_eq!(st.help_scroll, 4, "an offset left over from a shorter screen counts from the new max");
    }

    #[test]
    fn the_board_filters_as_you_type_cycles_scope_and_asks_before_resuming_all() {
        use ninox_core::types::SessionStatus as S;
        let mut st = state_with_rows(3);
        st.view = View::Board;
        st.rows[0].is_orchestrator = true;
        st.rows[1].session.orchestrator_id = Some("s0".into());
        handle_key(&mut st, key('/'));
        for c in "s1".chars() {
            handle_key(&mut st, key(c));
        }
        assert_eq!(st.view, View::Board, "typing '1' must not switch tabs");
        assert_eq!(st.kanban.query, "s1");
        handle_key(&mut st, code(KeyCode::Enter));
        assert!(!st.kanban.editing_query);
        handle_key(&mut st, code(KeyCode::Esc));
        assert_eq!((st.kanban.query.as_str(), st.view), ("", View::Board), "Esc clears the filter first");
        handle_key(&mut st, key('o'));
        assert_eq!(st.kanban.scope.as_deref(), Some("s0"));
        handle_key(&mut st, key('o'));
        assert_eq!(st.kanban.scope, None);

        handle_key(&mut st, KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT));
        assert_eq!(st.modal, None, "nothing interrupted");
        st.rows[2].session.status = S::Interrupted;
        handle_key(&mut st, KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT));
        assert_eq!(st.modal, Some(Modal::Confirm(Pending::ResumeAll)));
        assert!(st.confirm_text(&Pending::ResumeAll).0.contains("Resume all 1 interrupted session?"));
        assert_eq!(handle_key(&mut st, key('y')), Action::ResumeAll);
    }

    #[test]
    fn a_vanished_scope_resets_and_leaving_the_board_ends_filter_typing() {
        let mut st = state_with_rows(2);
        st.rows[0].is_orchestrator = true;
        st.view = View::Board;
        handle_key(&mut st, key('o'));
        assert_eq!(st.kanban.scope.as_deref(), Some("s0"));
        let rows: Vec<Row> = st.rows.iter().skip(1).cloned().collect();
        st.apply_rows(rows);
        assert_eq!(st.kanban.scope, None, "its orchestrator is gone");

        handle_key(&mut st, key('/'));
        handle_key(&mut st, prefix());
        handle_key(&mut st, key('h'));
        assert_eq!(st.view, View::Fleet);
        handle_key(&mut st, key('j'));
        st.view = View::Board;
        handle_key(&mut st, key('2'));
        assert!(!st.kanban.editing_query && st.kanban.query.is_empty(), "keys on the Board are commands again");
    }

    #[test]
    fn t_with_nothing_selected_says_why_instead_of_opening() {
        let mut st = state_with_rows(0);
        handle_key(&mut st, key('t'));
        assert_eq!(st.modal, None);
        assert!(st.notice.as_ref().is_some_and(|n| n.text.contains("select a session")), "{:?}", st.notice);
    }

    #[test]
    fn prefix_t_opens_the_composer_from_inside_an_attached_pane() {
        let mut st = state_with_rows(1);
        st.panes.insert("s0".into(), pane_info("s0", true));
        st.focus = Focus::Pane;
        assert!(matches!(handle_key(&mut st, key('t')), Action::Write { .. }), "a bare t is typing into the agent");
        handle_key(&mut st, prefix());
        handle_key(&mut st, key('t'));
        assert!(matches!(&st.modal, Some(Modal::Message(m)) if m.id == "s0"), "{:?}", st.modal);
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
        st.focus = Focus::Pane;
        st.layout = crate::tui::layout::compute(Rect::new(0, 0, 120, 40), &st);
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

    fn grid_with_open_reply() -> TuiState {
        let mut rows = Vec::new();
        for i in 0..12 {
            let id = format!("o{i}");
            rows.push(orchestrator_row(&id));
            for w in 0..3 {
                rows.push(worker_row(&format!("{id}-w{w}"), &id));
            }
        }
        let mut st = TuiState { rows, view: View::Fleet, ..Default::default() };
        st.fleet.sync(&st.rows, &HashMap::new());
        st.fleet.main_focus = FleetFocus::Main;
        st.fleet.reply.ask("q".into(), "ninox".into(), 1);
        st.layout = crate::tui::layout::compute(Rect::new(0, 0, 120, 40), &st);
        st.fleet.nav.selected_island = st.layout.fleet.as_ref().and_then(|f| f.grid.first()).map(|(k, _)| k.clone());
        st
    }

    #[test]
    fn moving_the_grid_selection_under_the_open_reply_collapses_it() {
        let mut st = grid_with_open_reply();
        let panel = st.layout.fleet.as_ref().and_then(|f| f.reply_panel).expect("panel open");
        let mut collapsed_at = None;
        for _ in 0..12 {
            handle_key(&mut st, code(KeyCode::Down));
            let fl = st.layout.fleet.clone().unwrap();
            let rect = fl.grid.iter().find(|(k, _)| Some(k) == st.fleet.nav.selected_island.as_ref()).map(|(_, r)| *r);
            if !st.fleet.reply.is_open() {
                collapsed_at = rect;
                break;
            }
            assert!(rect.is_none_or(|r| r.bottom() <= panel.y), "a selection under the panel must collapse it: {rect:?} vs {panel:?}");
            st.layout = crate::tui::layout::compute(Rect::new(0, 0, 120, 40), &st);
        }
        let rect = collapsed_at.expect("some Down lands under the panel and collapses it");
        assert!(rect.bottom() > panel.y, "collapsed only once the selection was under the panel");
        assert!(st.fleet.reply.drawer.has_content(), "collapsing keeps the history");
    }

    #[test]
    fn keys_that_dont_move_the_selection_leave_the_reply_open() {
        let mut st = grid_with_open_reply();
        let under = st.layout.fleet.as_ref().and_then(|f| {
            let panel = f.reply_panel?;
            f.grid.iter().find(|(_, r)| r.bottom() > panel.y).map(|(k, _)| k.clone())
        });
        st.fleet.nav.selected_island = under;
        handle_key(&mut st, key('m'));
        assert!(st.fleet.reply.is_open(), "only a selection move reveals; a reply landing over the selection stays up");
    }

    #[test]
    fn the_open_reply_panel_takes_the_wheel_and_swallows_clicks() {
        let mut st = TuiState { rows: vec![orchestrator_row("acme")], view: View::Fleet, ..Default::default() };
        st.fleet.sync(&st.rows, &HashMap::new());
        st.fleet.reply.ask("q".into(), "ninox".into(), 1);
        st.layout = crate::tui::layout::compute(Rect::new(0, 0, 120, 40), &st);
        let panel = st.layout.fleet.as_ref().and_then(|f| f.reply_panel).expect("panel laid out");
        st.fleet.reply.max_scroll.set(10);
        st.fleet.reply.drawn_top.set(10);
        let selected = st.fleet.nav.selected_island.clone();

        handle_mouse(&mut st, mouse(MouseEventKind::ScrollUp, panel.x + 2, panel.y + 2));
        assert_eq!(st.fleet.reply.scroll, Some(7), "wheel up scrolls back through the reply");
        handle_mouse(&mut st, down(panel.x + 2, panel.y + 2));
        handle_mouse(&mut st, down(panel.x + 2, panel.y + 2));
        assert!(st.fleet.nav.open.is_none(), "a double-click on the panel never opens an island under it");
        assert_eq!(st.fleet.nav.selected_island, selected);
    }

    #[test]
    fn wheel_scrolls_the_report_within_its_bounds() {
        let mut st = ended(false, false);
        st.focus = Focus::Pane;
        st.layout = crate::tui::layout::compute(Rect::new(0, 0, 120, 40), &st);
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

    fn ctrl_enter() -> KeyEvent {
        KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL)
    }

    fn type_into_focused(st: &mut TuiState, text: &str) {
        for c in text.chars() {
            handle_key(st, key(c));
        }
    }

    #[test]
    fn spawn_modal_opens_on_n() {
        let mut st = state_with_rows(0);
        assert_eq!(handle_key(&mut st, key('n')), Action::LoadSpawnForm);
        assert!(matches!(st.modal, Some(Modal::Spawn)));
        assert_eq!(st.spawn_form.focus, 0);
        assert_eq!(st.spawn_form.fields[0].id, spawn::FieldId::Kind);
    }

    #[test]
    fn ctrl_enter_with_empty_name_keeps_modal_open_with_error() {
        let mut st = state_with_rows(0);
        st.modal = Some(Modal::Spawn);
        assert_eq!(handle_key(&mut st, ctrl_enter()), Action::None);
        assert!(matches!(st.modal, Some(Modal::Spawn)));
        assert_eq!(st.spawn_form.error.as_deref(), Some("name is required"));
    }

    #[test]
    fn ctrl_enter_standalone_with_empty_workspace_keeps_modal_open_with_error() {
        let mut st = state_with_rows(0);
        st.modal = Some(Modal::Spawn);
        st.spawn_form.focus = st.spawn_form.fields.iter().position(|f| f.id == spawn::FieldId::Name).unwrap();
        type_into_focused(&mut st, "demo");
        st.spawn_form.focus = st.spawn_form.fields.iter().position(|f| f.id == spawn::FieldId::Kind).unwrap();
        handle_key(&mut st, code(KeyCode::Right)); // -> Standalone
        assert_eq!(handle_key(&mut st, ctrl_enter()), Action::None);
        assert!(matches!(st.modal, Some(Modal::Spawn)));
        assert_eq!(st.spawn_form.error.as_deref(), Some("workspace is required for a standalone session"));
    }

    #[test]
    fn plain_enter_on_prompt_field_inserts_newline_not_submit() {
        let mut st = state_with_rows(0);
        st.modal = Some(Modal::Spawn);
        st.spawn_form.focus = st.spawn_form.fields.iter().position(|f| f.id == spawn::FieldId::Prompt).unwrap();
        type_into_focused(&mut st, "line1");
        assert_eq!(handle_key(&mut st, code(KeyCode::Enter)), Action::None);
        type_into_focused(&mut st, "line2");
        assert!(matches!(st.modal, Some(Modal::Spawn)));
        assert_eq!(spawn::try_submit(&st.spawn_form), Err("name is required".to_string()));
        let prompt = &st.spawn_form.fields.iter().find(|f| f.id == spawn::FieldId::Prompt).unwrap().value;
        assert_eq!(prompt, "line1\nline2");
    }

    #[test]
    fn ctrl_enter_orchestrator_submits_with_workspace_and_type_none() {
        let mut st = state_with_rows(0);
        st.modal = Some(Modal::Spawn);
        st.spawn_form.focus = st.spawn_form.fields.iter().position(|f| f.id == spawn::FieldId::Name).unwrap();
        type_into_focused(&mut st, "demo");
        let a = handle_key(&mut st, ctrl_enter());
        let Action::Spawn(sub) = a else { panic!("expected Action::Spawn, got {a:?}") };
        assert_eq!(sub.kind, ninox_ui_shared::spawn_form::SpawnKind::Orchestrator);
        assert_eq!(sub.name, "demo");
        assert_eq!(sub.workspace, None);
        assert_eq!(sub.worker_type, None);
        assert!(st.modal.is_none());
    }

    #[test]
    fn ctrl_enter_standalone_submits_with_workspace_and_type() {
        let mut st = state_with_rows(0);
        st.modal = Some(Modal::Spawn);
        st.spawn_form.focus = st.spawn_form.fields.iter().position(|f| f.id == spawn::FieldId::Name).unwrap();
        type_into_focused(&mut st, "demo");
        st.spawn_form.focus = st.spawn_form.fields.iter().position(|f| f.id == spawn::FieldId::Kind).unwrap();
        handle_key(&mut st, code(KeyCode::Right)); // -> Standalone
        st.spawn_form.focus = st.spawn_form.fields.iter().position(|f| f.id == spawn::FieldId::Workspace).unwrap();
        type_into_focused(&mut st, "~/x");
        let a = handle_key(&mut st, ctrl_enter());
        let Action::Spawn(sub) = a else { panic!("expected Action::Spawn, got {a:?}") };
        assert_eq!(sub.kind, ninox_ui_shared::spawn_form::SpawnKind::Standalone);
        assert_eq!(sub.workspace.as_deref(), Some("~/x"));
        assert_eq!(sub.worker_type.as_deref(), Some("Implementor"));
    }

    #[test]
    fn spawn_modal_esc_cancels() {
        let mut st = state_with_rows(0);
        st.modal = Some(Modal::Spawn);
        assert_eq!(handle_key(&mut st, code(KeyCode::Esc)), Action::None);
        assert!(st.modal.is_none());
    }

    #[test]
    fn tab_and_shift_tab_cycle_focus_through_visible_fields_only() {
        let mut st = state_with_rows(0);
        st.modal = Some(Modal::Spawn);
        st.spawn_form.focus = st.spawn_form.fields.iter().position(|f| f.id == spawn::FieldId::Name).unwrap();
        handle_key(&mut st, code(KeyCode::Tab));
        // Orchestrator kind hides Workspace/WorkerType — Tab from Name lands on Harness.
        assert_eq!(st.spawn_form.fields[st.spawn_form.focus].id, spawn::FieldId::Harness);
        handle_key(&mut st, code(KeyCode::BackTab));
        assert_eq!(st.spawn_form.fields[st.spawn_form.focus].id, spawn::FieldId::Name);
    }

    fn remote_row(id: &str) -> crate::tui::remotes::RemoteRow {
        crate::tui::remotes::RemoteRow {
            id: id.into(),
            label: id.into(),
            ssh_target: format!("user@{id}"),
            remote_session: "default".into(),
            enabled: true,
            health: None,
        }
    }

    #[test]
    fn a_opens_the_add_remote_modal_from_the_empty_and_populated_remotes_tab() {
        let mut st = TuiState { view: View::Remotes, ..Default::default() };
        handle_key(&mut st, key('a'));
        assert!(matches!(st.modal, Some(Modal::AddRemote(_))), "empty state: a must open the form");

        let mut st = TuiState { view: View::Remotes, ..Default::default() };
        st.set_remote_rows(vec![remote_row("m0")]);
        handle_key(&mut st, key('a'));
        assert!(matches!(st.modal, Some(Modal::AddRemote(_))), "populated list: a must open the form too");
    }

    #[test]
    fn add_remote_modal_tab_switches_fields_and_enter_submits_host_and_label() {
        let mut st = TuiState { view: View::Remotes, ..Default::default() };
        handle_key(&mut st, key('a'));
        for c in "box.example.com".chars() {
            handle_key(&mut st, key(c));
        }
        handle_key(&mut st, code(KeyCode::Tab));
        let Some(Modal::AddRemote(m)) = &st.modal else { panic!("modal must stay open") };
        assert!(m.on_label_field, "tab moves focus onto the label field");
        for c in "box".chars() {
            handle_key(&mut st, key(c));
        }
        let action = handle_key(&mut st, code(KeyCode::Enter));
        assert_eq!(action, Action::AddRemote { host: "box.example.com".into(), label: Some("box".into()) });
        assert!(st.modal.is_none());
    }

    #[test]
    fn add_remote_modal_submits_from_either_field_with_no_advance_staging() {
        // Unlike Spawn, Enter on the first field submits directly rather than
        // just advancing focus — matches the brief's "Tab/Shift-Tab move
        // between fields. Enter submits."
        let mut st = TuiState { view: View::Remotes, ..Default::default() };
        handle_key(&mut st, key('a'));
        for c in "host1".chars() {
            handle_key(&mut st, key(c));
        }
        let action = handle_key(&mut st, code(KeyCode::Enter));
        assert_eq!(action, Action::AddRemote { host: "host1".into(), label: None });
    }

    #[test]
    fn add_remote_modal_empty_host_shows_inline_error_and_does_not_submit() {
        let mut st = TuiState { view: View::Remotes, ..Default::default() };
        handle_key(&mut st, key('a'));
        let action = handle_key(&mut st, code(KeyCode::Enter));
        assert_eq!(action, Action::None);
        let Some(Modal::AddRemote(m)) = &st.modal else { panic!("modal must stay open on empty host") };
        assert!(m.error.is_some());
        // Typing again clears the error rather than leaving it stuck.
        handle_key(&mut st, key('h'));
        let Some(Modal::AddRemote(m)) = &st.modal else { panic!("modal must stay open") };
        assert!(m.error.is_none());
    }

    #[test]
    fn add_remote_modal_esc_cancels() {
        let mut st = TuiState { view: View::Remotes, ..Default::default() };
        handle_key(&mut st, key('a'));
        for c in "host1".chars() {
            handle_key(&mut st, key(c));
        }
        assert_eq!(handle_key(&mut st, code(KeyCode::Esc)), Action::None);
        assert!(st.modal.is_none());
    }

    #[test]
    fn w_toggles_the_workspaces_panel_and_esc_closes_it_without_leaving_the_remotes_tab() {
        let mut st = TuiState { view: View::Remotes, ..Default::default() };
        assert!(!st.workspaces.open);

        handle_key(&mut st, key('W'));
        assert!(st.workspaces.open);

        handle_key(&mut st, code(KeyCode::Esc));
        assert!(!st.workspaces.open, "Esc closes the panel first");
        assert_eq!(st.view, View::Remotes, "Esc must not also leave the Remotes tab");

        handle_key(&mut st, code(KeyCode::Esc));
        assert_eq!(st.view, View::Fleet, "a second Esc behaves like the ordinary tab-exit");
    }

    #[test]
    fn p_toggles_the_prune_preview_only_while_the_panel_is_open() {
        let mut st = TuiState { view: View::Remotes, ..Default::default() };

        handle_key(&mut st, key('p'));
        assert!(!st.workspaces.preview, "p does nothing while the panel is closed");

        handle_key(&mut st, key('W'));
        handle_key(&mut st, key('p'));
        assert!(st.workspaces.preview);
        handle_key(&mut st, key('p'));
        assert!(!st.workspaces.preview, "a second p toggles it back off");

        handle_key(&mut st, key('p'));
        handle_key(&mut st, key('W'));
        assert!(!st.workspaces.preview, "closing the panel always resets the preview");
    }

    #[test]
    fn typing_in_the_add_remote_host_field_never_switches_tabs_or_scrolls_the_list() {
        let mut st = TuiState { view: View::Remotes, ..Default::default() };
        st.set_remote_rows(vec![remote_row("m0"), remote_row("m1")]);
        handle_key(&mut st, key('a'));
        // '1' would normally be a tab-switch key, and 'j'/'k' move the list
        // selection — both must go into the Host field instead while the
        // modal owns input.
        for c in "1jk".chars() {
            handle_key(&mut st, key(c));
        }
        assert_eq!(st.view, View::Remotes, "typing in the modal must not switch tabs");
        assert_eq!(st.remotes.selected, 0, "typing in the modal must not move the list selection");
        let Some(Modal::AddRemote(m)) = &st.modal else { panic!("modal must still be open") };
        assert_eq!(m.host, "1jk");
    }

    #[test]
    fn d_on_the_remotes_tab_confirms_then_removes_the_selected_machine() {
        let mut st = TuiState { view: View::Remotes, ..Default::default() };
        st.set_remote_rows(vec![remote_row("m0"), remote_row("m1")]);
        st.remotes.selected = 1;
        handle_key(&mut st, key('d'));
        assert!(matches!(st.modal, Some(Modal::Confirm(Pending::RemoveMachine(ref id))) if id == "m1"));
        let action = handle_key(&mut st, key('y'));
        assert_eq!(action, Action::RemoveMachine("m1".into()));
        assert!(st.modal.is_none());
    }

    #[test]
    fn d_on_an_empty_remotes_tab_does_nothing() {
        let mut st = TuiState { view: View::Remotes, ..Default::default() };
        assert_eq!(handle_key(&mut st, key('d')), Action::None);
        assert!(st.modal.is_none());
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
        // Final review Important #3: Goto's Enter writes `st.selected`
        // directly — the graph/detail panel must still follow it.
        assert_eq!(st.fleet.nav.selected.as_deref(), Some("s2"));
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
    fn held_messages_show_on_target_and_sender_and_notify_once() {
        let mut st = state_with_rows(2);
        let held = HashMap::from([("s1".to_string(), (1, 2))]);
        let by_sender = HashMap::from([("s0".to_string(), 2)]);
        let notes = st.apply_held(held.clone(), by_sender.clone());
        assert_eq!(notes, vec!["message to s1 queued: target unreachable, retrying".to_string()]);
        assert!(st.apply_held(held, by_sender).is_empty(), "notified once, not every refresh");
        let target: Vec<String> = st.held_lines("s1").into_iter().map(|(k, v)| format!("{k}: {v}")).collect();
        assert_eq!(target, vec![
            "undelivered: 2 messages held (unreachable, retrying)".to_string(),
            "held: 1 message waiting for it to start".to_string(),
        ]);
        assert_eq!(st.held_lines("s0"), vec![("sent", "2 messages queued (target unreachable)".to_string())]);
        st.apply_held(HashMap::new(), HashMap::new());
        assert!(st.held_lines("s1").is_empty(), "cleared once delivered");
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
    fn fleet_tree_selection_drives_the_canvas_detail_panel() {
        // `apply_rows` deliberately does *not* auto-populate `fleet.nav.selected`
        // on a plain startup/data refresh any more (see `apply_rows`'s own
        // comment) — doing so unconditionally silently undid Esc's
        // "clear the detail-pane selection" stage within a second, the
        // next time the routine 1s refresh tick ran. Nothing is selected
        // until the user actually navigates or opens an island.
        let mut st = TuiState::default();
        st.apply_rows(vec![Row::test_row("s0"), Row::test_row("s1")]);
        assert_eq!(st.fleet.nav.selected, None, "startup alone must not populate a selection");

        handle_key(&mut st, key('j'));
        assert_eq!(st.selected_id().as_deref(), Some("s1"));
        assert_eq!(st.fleet.nav.selected.as_deref(), Some("s1"), "ordinary tree movement drives the canvas");
    }

    #[test]
    fn esc_cleared_fleet_selection_survives_a_routine_refresh() {
        // Regression: `apply_rows` used to unconditionally re-sync
        // `fleet.nav.selected` from the tree cursor on every call — since
        // `refresh()` calls it every ~1s tick regardless of user input,
        // that silently undid Esc's "clear the detail-pane selection"
        // stage (`fleet.nav.clear_selection`) within a second, even though
        // the tree cursor itself never moved.
        let mut st = TuiState { rows: vec![orchestrator_row("acme")], view: View::Fleet, ..Default::default() };
        st.fleet.sync(&st.rows, &HashMap::new());
        st.selected = 0;
        handle_key(&mut st, code(KeyCode::Enter));
        assert!(st.fleet.nav.selected.is_some(), "opening the island selects its root");

        handle_key(&mut st, code(KeyCode::Esc));
        assert_eq!(st.fleet.nav.selected, None, "esc clears the selection");

        // A routine data refresh (same rows, selected row unchanged) must
        // not resurrect the cleared selection.
        st.apply_rows(vec![orchestrator_row("acme")]);
        assert_eq!(st.fleet.nav.selected, None, "a routine refresh must not undo Esc's clear");
        assert!(st.fleet.nav.open.is_some(), "the island itself must still be open");
    }

    #[test]
    fn apply_rows_still_resyncs_the_selection_when_the_selected_row_vanishes() {
        let mut st = TuiState { rows: vec![orchestrator_row("acme"), worker_row("w1", "acme")], view: View::Fleet, ..Default::default() };
        st.select(1); // w1
        assert_eq!(st.fleet.nav.selected.as_deref(), Some("w1"));

        // w1 is gone from the next refresh — the stale selection must not
        // silently point at a vanished row forever.
        st.apply_rows(vec![orchestrator_row("acme")]);
        assert_eq!(st.fleet.nav.selected.as_deref(), Some("acme"), "selection follows the remapped tree cursor once its own row vanishes");
    }

    #[test]
    fn set_remote_rows_clamps_the_selection_when_the_list_shrinks() {
        use crate::tui::remotes::RemoteRow;
        let row = |id: &str| RemoteRow {
            id: id.into(),
            label: id.into(),
            ssh_target: String::new(),
            remote_session: "default".into(),
            enabled: true,
            health: None,
        };
        let mut st = TuiState::default();
        st.set_remote_rows(vec![row("m0"), row("m1"), row("m2")]);
        st.remotes.selected = 2; // last machine
        st.set_remote_rows(vec![row("m0")]); // a machine was removed from config
        assert_eq!(st.remotes.selected, 0, "a stale selection must not point past the end of the shrunken list");
    }

    #[test]
    fn apply_rows_clamps_the_kanban_row_when_the_selected_column_shrinks() {
        use ninox_core::types::SessionStatus as S;
        let working = |id: &str| Row {
            session: ninox_ui_shared::cli::test_fixtures::session(id, None, S::Working),
            is_orchestrator: false,
            group: None,
            machine_health: None, queue_position: None, prs: Vec::new(),
        };
        let mut st = TuiState::default();
        st.apply_rows(vec![working("s0"), working("s1"), working("s2")]);
        st.kanban.col = 0; // Working
        st.kanban.row = 2; // last card
        st.apply_rows(vec![working("s0")]); // the column shrinks to one card
        assert_eq!(st.kanban.row, 0, "a stale row index must not point past the end of the shrunken column");
    }

    #[test]
    fn scroll_mode_keys_and_exit() {
        let mut st = state_with_rows(1);
        with_live_pane(&mut st, "s0");
        // `[` collapses the Fleet tree instead when the pane isn't the
        // thing on screen (Focus::Sidebar) — see
        // `bracket_collapses_even_with_a_runnable_pane_selected`. Scroll
        // mode is reached once the pane itself has focus.
        st.focus = Focus::Pane;
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

        st.modal = Some(Modal::Spawn);
        st.offer_restore(Some(summary.clone()));
        assert!(matches!(st.modal, Some(Modal::Spawn)), "an open modal isn't clobbered");
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
    fn mouse_click_selects_sidebar_rows_and_focuses_panes() {
        // This split sidebar+pane layout is only ever computed for
        // `Focus::Pane` now (`layout::compute` gives the Fleet tab's
        // `Focus::Sidebar` the islands grid instead, with its own mouse
        // handling in `fleet_mouse`) — matching that is what makes this
        // generic sidebar/pane click/wheel/SGR-forwarding logic reachable
        // at all.
        let mut st = state_with_rows(3);
        st.focus = Focus::Pane;
        with_live_pane(&mut st, "s1");
        st.layout.sidebar_list = Some(Rect::new(0, 2, 20, 10));
        st.layout.sidebar_items = st.sidebar_items();
        st.layout.pane = Some(Rect::new(20, 1, 60, 20));
        st.layout.pane_inner = Some(Rect::new(21, 2, 58, 18));
        let click = |x, y| MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: x, row: y, modifiers: KeyModifiers::NONE };
        handle_mouse(&mut st, click(3, 3));
        assert_eq!(st.selected, 1);
        assert_eq!(st.focus, Focus::Pane, "clicking a row doesn't by itself reset focus off the pane");
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
        assert_eq!((st.focus, st.selected), (Focus::Pane, 0), "reselecting from the sidebar keeps the pane-split view");

        st.focus = Focus::Pane;
        let side = lay.sidebar.unwrap();
        assert_eq!(handle_mouse(&mut st, down(side.x + 1, side.y)), Action::None, "via the outer sidebar rect too");
        assert_eq!(st.focus, Focus::Pane, "still the same row reselect, still keeps the pane-split view");

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
        assert_eq!(st.view, View::Fleet);
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

    #[test]
    fn bracket_keys_cycle_kind_filter_s_cycles_sort_x_clears() {
        use crate::tui::brain::Grouping;
        let mut st = brain_state(Grouping::Tag);
        assert_eq!(handle_key(&mut st, key(']')), Action::None);
        assert_eq!(st.brain.kind_filter.as_deref(), Some("concepts"));
        handle_key(&mut st, key(']'));
        assert_eq!(st.brain.kind_filter.as_deref(), Some("errors"));
        handle_key(&mut st, key('['));
        assert_eq!(st.brain.kind_filter.as_deref(), Some("concepts"));
        handle_key(&mut st, key('s'));
        assert_eq!(st.brain.sort, BrainSort::Az);
        handle_key(&mut st, key('s'));
        assert_eq!(st.brain.sort, BrainSort::MostLinked);
        handle_key(&mut st, key('x'));
        assert_eq!(st.brain.kind_filter, None);
    }

    #[test]
    fn r_cycles_repo_filter_and_shift_t_cycles_tag_filter() {
        use crate::tui::brain::Grouping;
        let mut st = brain_state(Grouping::Tag);
        st.brain.entries[0].repos = vec!["ninox".to_string()]; // ptyd
        assert_eq!(handle_key(&mut st, key('r')), Action::None);
        assert_eq!(st.brain.repo_filter.as_deref(), Some("ninox"));
        handle_key(&mut st, key('r'));
        assert_eq!(st.brain.repo_filter, None, "wraps back to All");
        handle_key(&mut st, key('T'));
        assert_eq!(st.brain.tag_filter.as_deref(), Some("tmux"), "highest count first");
    }

    #[test]
    fn c_cycles_the_brain_catalogue_and_reloads() {
        use crate::tui::brain::Grouping;
        let mut st = brain_state(Grouping::Tag);
        st.brain.set_catalogues(vec![
            ninox_core::config::CatalogueRef {
                name: "default".to_string(),
                path: std::path::PathBuf::from("/brains/default"),
                remote: None,
                endpoint: None,
                region: None,
                cache_ttl_secs: None,
                enabled: true,
                cadence: "manual".to_string(),
                access: "read_write".to_string(),
            },
            ninox_core::config::CatalogueRef {
                name: "team".to_string(),
                path: std::path::PathBuf::from("/brains/team"),
                remote: None,
                endpoint: None,
                region: None,
                cache_ttl_secs: None,
                enabled: true,
                cadence: "manual".to_string(),
                access: "read_write".to_string(),
            },
        ]);
        assert_eq!(st.brain.active_catalogue, None);
        assert_eq!(handle_key(&mut st, key('c')), Action::CycleBrainCatalogue);
        assert_eq!(st.brain.active_catalogue, Some(0));
        handle_key(&mut st, key('c'));
        assert_eq!(st.brain.active_catalogue, Some(1));
    }

    #[test]
    fn l_enters_link_focus_and_enter_jumps_to_the_focused_link() {
        use crate::tui::brain::Grouping;
        let mut st = brain_state(Grouping::Tag);
        handle_key(&mut st, key('j')); // off the "runtime" header, onto ptyd
        assert_eq!(selected_id(&st).as_deref(), Some("concepts/ptyd.md"));
        // L with no links loaded does nothing.
        assert_eq!(handle_key(&mut st, key('L')), Action::None);
        assert_eq!(st.brain.link_focus, None);
        st.brain.outlinks = vec![crate::tui::brain::tests::sample().entries[3].clone()]; // patterns/Alpha.md
        handle_key(&mut st, key('L'));
        assert_eq!(st.brain.link_focus, Some(0));
        assert_eq!(handle_key(&mut st, key('j')), Action::None, "j moves link focus, not the list, while link-focused");
        assert_eq!(st.brain.link_focus, Some(0), "clamped: only one row");
        assert_eq!(handle_key(&mut st, code(KeyCode::Enter)), Action::LoadBrainLinks("patterns/Alpha.md".into()));
        assert_eq!(selected_id(&st).as_deref(), Some("patterns/Alpha.md"), "Enter jumped to the focused link");
        assert_eq!(st.brain.link_focus, None, "jump exits link focus");
    }

    #[test]
    fn esc_exits_link_focus_without_closing_the_entry_or_changing_selection() {
        use crate::tui::brain::Grouping;
        let mut st = brain_state(Grouping::Tag);
        handle_key(&mut st, key('j'));
        st.brain.backlinks = vec![crate::tui::brain::tests::sample().entries[3].clone()];
        handle_key(&mut st, code(KeyCode::Enter)); // open the reading pane
        assert!(st.brain.open);
        handle_key(&mut st, key('L'));
        assert_eq!(st.brain.link_focus, Some(0));
        handle_key(&mut st, code(KeyCode::Esc));
        assert_eq!(st.brain.link_focus, None);
        assert!(st.brain.open, "esc only backs out of link focus, not the whole reading pane");
        assert_eq!(selected_id(&st).as_deref(), Some("concepts/ptyd.md"));
    }

    #[test]
    fn tab_toggles_brain_graph_mode_and_seeds_focus_from_selection() {
        use crate::tui::brain::Grouping;
        let mut st = brain_state(Grouping::Tag);
        handle_key(&mut st, key('j')); // onto ptyd
        assert_eq!(selected_id(&st).as_deref(), Some("concepts/ptyd.md"));
        assert_eq!(handle_key(&mut st, code(KeyCode::Tab)), Action::None);
        assert!(st.brain.graph_mode);
        assert_eq!(st.brain.graph_focus.as_deref(), Some("concepts/ptyd.md"));
        handle_key(&mut st, code(KeyCode::Tab));
        assert!(!st.brain.graph_mode);
    }

    #[test]
    fn graph_mode_enter_refocuses_and_o_opens_in_list() {
        use crate::tui::brain::Grouping;
        let mut st = brain_state(Grouping::Tag);
        handle_key(&mut st, key('j'));
        st.brain.set_edges(&[("concepts/ptyd.md".to_string(), "errors/socket.md".to_string())]);
        handle_key(&mut st, code(KeyCode::Tab));
        assert!(st.brain.graph_mode);

        st.brain.ensure_graph_flow();
        st.brain.graph_flow_mut().as_mut().unwrap().select_node("errors/socket.md");
        assert_eq!(handle_key(&mut st, code(KeyCode::Enter)), Action::None);
        assert_eq!(st.brain.graph_focus.as_deref(), Some("errors/socket.md"), "Enter refocuses on the selected node");

        st.brain.ensure_graph_flow();
        st.brain.graph_flow_mut().as_mut().unwrap().select_node("concepts/ptyd.md");
        handle_key(&mut st, key('o'));
        assert!(!st.brain.graph_mode, "o leaves graph mode");
        assert_eq!(selected_id(&st).as_deref(), Some("concepts/ptyd.md"), "o selects the node in the list");
    }

    #[test]
    fn esc_exits_graph_mode() {
        use crate::tui::brain::Grouping;
        let mut st = brain_state(Grouping::Tag);
        handle_key(&mut st, code(KeyCode::Tab));
        assert!(st.brain.graph_mode);
        handle_key(&mut st, code(KeyCode::Esc));
        assert!(!st.brain.graph_mode);
    }

    #[test]
    fn s_cycles_graph_scope_to_clusters_and_back() {
        use crate::tui::brain::{Grouping, GraphScope};
        let mut st = brain_state(Grouping::Tag);
        handle_key(&mut st, code(KeyCode::Tab));
        assert_eq!(st.brain.graph_scope, GraphScope::Neighbourhood);
        handle_key(&mut st, key('s'));
        assert_eq!(st.brain.graph_scope, GraphScope::Clusters);
        handle_key(&mut st, key('s'));
        assert_eq!(st.brain.graph_scope, GraphScope::Neighbourhood);
    }

    #[test]
    fn clusters_scope_moves_the_cursor_and_enter_filters_the_list_by_repo() {
        use crate::tui::brain::{Grouping, GraphScope};
        let mut st = brain_state(Grouping::Tag);
        st.brain.entries[0].repos = vec!["ninox".to_string()]; // ptyd
        st.brain.entries[2].repos = vec!["bellhop".to_string()]; // socket
        handle_key(&mut st, code(KeyCode::Tab));
        handle_key(&mut st, key('s'));
        assert_eq!(st.brain.graph_scope, GraphScope::Clusters);
        assert_eq!(st.brain.clusters_cursor, 0);
        handle_key(&mut st, code(KeyCode::Down));
        assert_eq!(st.brain.clusters_cursor, 1);
        let expected_repo = st.brain.clusters()[1].label.clone();
        handle_key(&mut st, code(KeyCode::Enter));
        assert!(!st.brain.graph_mode, "Enter leaves graph mode");
        assert_eq!(st.brain.repo_filter, Some(expected_repo));
    }

    #[test]
    fn shift_r_triggers_a_full_reindex() {
        use crate::tui::brain::Grouping;
        let mut st = brain_state(Grouping::Tag);
        assert_eq!(handle_key(&mut st, key('R')), Action::ReindexBrain);
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
        assert_eq!(st.view, View::Fleet);
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
        let mut st = ended(false, false);
        st.focus = Focus::Pane;
        st.layout = crate::tui::layout::compute(Rect::new(0, 0, 120, 40), &st);
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
    fn c_bracket_then_bare_x_kills_or_deletes_an_orchestrator_row() {
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
        assert!(
            matches!(&st.modal, Some(Modal::Delete(d)) if d.root == "o"),
            "removing an orchestrator is D's delete, which takes its whole tree: {:?}", st.modal,
        );
    }

    #[test]
    fn the_selected_sidebar_row_has_a_close_button_and_right_click_asks_too() {
        let mut st = grouped(2);
        st.focus = Focus::Pane;
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

    fn worker_types_state() -> TuiState {
        let mut st = state_with_rows(1);
        st.view = View::WorkerTypes;
        st.worker_types.reload(Ok(ninox_core::config::AppConfig::default()));
        st.layout = crate::tui::layout::compute(Rect::new(0, 0, 140, 50), &st);
        st
    }

    fn select_worker_type_field(st: &mut TuiState, id: WtFieldId) {
        st.worker_types.field_selected = st.worker_types.fields.iter().position(|f| f.id == id).unwrap();
        st.worker_types.on_list = false;
    }

    #[test]
    fn tab_toggles_focus_between_list_and_form() {
        let mut st = worker_types_state();
        assert!(st.worker_types.on_list);
        handle_key(&mut st, code(KeyCode::Tab));
        assert!(!st.worker_types.on_list);
        handle_key(&mut st, code(KeyCode::BackTab));
        assert!(st.worker_types.on_list);
    }

    #[test]
    fn list_navigation_selects_a_name_and_the_trailing_new_row() {
        let mut st = worker_types_state();
        assert_eq!(st.worker_types.selected_name(), Some("Evaluator"));
        handle_key(&mut st, key('j'));
        assert_eq!(st.worker_types.selected_name(), Some("Implementor"));
        handle_key(&mut st, code(KeyCode::Up));
        assert_eq!(st.worker_types.selected_name(), Some("Evaluator"), "clamped at the top");
        for _ in 0..10 {
            handle_key(&mut st, key('j'));
        }
        assert!(st.worker_types.is_new_row(), "clamped at the trailing + New type row");
        assert_eq!(st.worker_types.selected_name(), None);
    }

    #[test]
    fn field_stepping_and_inline_text_edit_emit_save_actions() {
        let mut st = worker_types_state();
        handle_key(&mut st, key('j')); // select "Implementor" (2nd name)
        handle_key(&mut st, code(KeyCode::Tab)); // move focus onto the form
        select_worker_type_field(&mut st, WtFieldId::Harness);
        let step = |back| Action::SaveWorkerType { name: "Implementor".into(), change: worker_types::Change::Step { id: WtFieldId::Harness, back } };
        assert_eq!(handle_key(&mut st, code(KeyCode::Enter)), step(false));
        assert_eq!(handle_key(&mut st, code(KeyCode::Left)), step(true));

        select_worker_type_field(&mut st, WtFieldId::Model);
        assert_eq!(handle_key(&mut st, code(KeyCode::Enter)), Action::None);
        assert_eq!(st.worker_types.editing.as_deref(), Some(""));
        for c in "o3".chars() {
            handle_key(&mut st, key(c));
        }
        assert_eq!(
            handle_key(&mut st, code(KeyCode::Enter)),
            Action::SaveWorkerType { name: "Implementor".into(), change: worker_types::Change::Set { id: WtFieldId::Model, text: "o3".into() } }
        );
        assert!(st.worker_types.editing.is_some(), "stays open until the save succeeds");
        handle_key(&mut st, code(KeyCode::Esc));
        assert!(st.worker_types.editing.is_none());
        assert_eq!(st.view, View::WorkerTypes, "esc only closed the editor");
        handle_key(&mut st, code(KeyCode::Esc));
        assert_eq!(st.view, View::Fleet);
    }

    #[test]
    fn activating_a_non_name_field_on_the_new_row_does_not_open_an_editor() {
        let mut st = worker_types_state();
        handle_key(&mut st, key('n'));
        handle_key(&mut st, code(KeyCode::Esc)); // cancel the Name editor, stay on the new row
        assert!(st.worker_types.is_new_row());
        st.worker_types.field_selected = 3; // Default harness
        assert_eq!(handle_key(&mut st, code(KeyCode::Enter)), Action::None);
        assert!(st.worker_types.editing.is_none(), "only the Name field opens an editor before the type exists");
        assert!(st.worker_types.error.is_some());
    }

    #[test]
    fn n_opens_a_blank_draft_on_the_trailing_row_and_enter_creates_it() {
        let mut st = worker_types_state();
        handle_key(&mut st, key('n'));
        assert!(st.worker_types.is_new_row());
        assert!(!st.worker_types.on_list);
        assert_eq!(st.worker_types.editing.as_deref(), Some(""));
        for c in "Tester".chars() {
            handle_key(&mut st, key(c));
        }
        assert_eq!(handle_key(&mut st, code(KeyCode::Enter)), Action::NewWorkerType("Tester".into()));
    }

    /// `worker_types_state` with the form focused on the first row matching `id`.
    fn on_field(id: impl Fn(&WtFieldId) -> bool) -> TuiState {
        let mut st = worker_types_state();
        st.worker_types.on_list = false;
        st.worker_types.field_selected = st.worker_types.fields.iter().position(|f| id(&f.id)).expect("such a row");
        st
    }

    #[test]
    fn on_a_rule_row_a_types_a_new_rule_and_x_deletes_that_rule() {
        use crate::tui::worker_types::{Change, RuleList};
        let mut st = on_field(|id| matches!(id, WtFieldId::Rule(RuleList::Deny, _)));
        let name = st.worker_types.selected_name().unwrap().to_string();
        assert_eq!(handle_key(&mut st, key('a')), Action::None);
        assert_eq!(st.worker_types.editing.as_deref(), Some(""));
        for c in "Bash(rm:*)".chars() {
            handle_key(&mut st, key(c));
        }
        assert_eq!(
            handle_key(&mut st, code(KeyCode::Enter)),
            Action::SaveWorkerType { name: name.clone(), change: Change::AddRule { list: RuleList::Deny, text: "Bash(rm:*)".into() } },
        );
        assert_eq!(st.worker_types.adding_rule, Some(RuleList::Deny), "still an add until the save lands");

        let mut st = worker_types_state();
        let mut cfg = ninox_core::config::AppConfig::default();
        let mut spec = cfg.worker_type_registry().get(&name).cloned().unwrap();
        spec.permissions.deny = vec!["Read".into(), "Bash(rm:*)".into()];
        cfg.upsert_worker_type(spec);
        st.worker_types.reload(Ok(cfg));
        st.worker_types.on_list = false;
        st.worker_types.field_selected = st.worker_types.fields.iter().position(|f| f.id == WtFieldId::Rule(RuleList::Deny, Some(1))).unwrap();
        assert_eq!(
            handle_key(&mut st, key('x')),
            Action::SaveWorkerType { name: name.clone(), change: Change::RemoveRule { list: RuleList::Deny, index: 1, rule: "Bash(rm:*)".into() } },
        );
        st.worker_types.field_selected = 0;
        assert_eq!(handle_key(&mut st, key('x')), Action::ExportWorkerType(name), "off a rule row x still exports");
    }

    #[test]
    fn can_spawn_options_and_can_manage_types_toggle_with_enter() {
        use crate::tui::worker_types::Change;
        let mut st = on_field(|id| *id == WtFieldId::CanSpawnOption("Researcher".into()));
        let name = st.worker_types.selected_name().unwrap().to_string();
        assert_eq!(
            handle_key(&mut st, code(KeyCode::Enter)),
            Action::SaveWorkerType { name: name.clone(), change: Change::Step { id: WtFieldId::CanSpawnOption("Researcher".into()), back: false } },
        );
        let mut st = on_field(|id| *id == WtFieldId::CanManageTypes);
        assert_eq!(handle_key(&mut st, key(' ')), Action::SaveWorkerType { name, change: Change::Step { id: WtFieldId::CanManageTypes, back: false } });
    }

    #[test]
    fn capital_r_resets_only_a_customized_built_in_after_a_confirm() {
        let mut st = worker_types_state();
        let name = st.worker_types.selected_name().unwrap().to_string();
        handle_key(&mut st, KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT));
        assert_eq!(st.modal, None, "an unmodified built-in has nothing to reset");
        assert!(st.worker_types.error.as_deref().is_some_and(|e| e.contains("only a customized built-in")));
        let mut cfg = ninox_core::config::AppConfig::default();
        let mut spec = cfg.worker_type_registry().get(&name).cloned().unwrap();
        spec.model = Some("o3".into());
        cfg.upsert_worker_type(spec);
        st.worker_types.reload(Ok(cfg));
        handle_key(&mut st, KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT));
        assert_eq!(st.modal, Some(Modal::Confirm(Pending::ResetWorkerType(name.clone()))));
        assert_eq!(handle_modal_key(&mut st, key('y')), Action::ResetWorkerType(name));
    }

    #[test]
    fn capital_d_asks_before_deleting_a_custom_type_refuses_a_built_in_and_x_i_export_import() {
        let mut st = worker_types_state();
        handle_key(&mut st, key('j')); // Implementor
        handle_key(&mut st, key('D'));
        assert_eq!(st.modal, None, "a built-in is reset, never deleted");
        assert!(st.worker_types.error.as_deref().is_some_and(|e| e.contains("R resets")), "{:?}", st.worker_types.error);

        let mut cfg = ninox_core::config::AppConfig::default();
        cfg.upsert_worker_type(ninox_core::worker_type::WorkerTypeSpec { name: "Tester".into(), ..Default::default() });
        st.worker_types.reload(Ok(cfg));
        st.worker_types.select_list_row(st.worker_types.names.iter().position(|n| n == "Tester").unwrap());
        handle_key(&mut st, key('D'));
        assert_eq!(st.modal, Some(Modal::Confirm(Pending::DeleteWorkerType("Tester".into()))));
        assert_eq!(handle_modal_key(&mut st, key('y')), Action::DeleteWorkerType("Tester".into()));
        st.worker_types.select_list_row(st.worker_types.names.iter().position(|n| n == "Implementor").unwrap());

        assert_eq!(handle_key(&mut st, key('x')), Action::ExportWorkerType("Implementor".into()));
        assert_eq!(handle_key(&mut st, key('i')), Action::ImportWorkerType);
    }

    #[test]
    fn worker_type_rows_are_clickable_and_a_value_click_opens_the_editor() {
        let mut st = worker_types_state();
        let wl = st.layout.worker_types.clone().unwrap();
        let y = wl.names.y + 1; // "Implementor", alphabetically after Evaluator
        assert_eq!(handle_mouse(&mut st, down(wl.names.x + 1, y)), Action::None);
        assert_eq!(st.worker_types.selected_name(), Some("Implementor"));
        assert!(!st.worker_types.on_list, "clicking a name also moves focus onto its form");

        let field_y = |st: &TuiState, id: WtFieldId| {
            let i = st.worker_types.fields.iter().position(|f| f.id == id).unwrap();
            wl.fields.y + (i - wl.fields_offset) as u16
        };
        let y = field_y(&st, WtFieldId::Model);
        assert_eq!(handle_mouse(&mut st, down(wl.value_x + 1, y)), Action::None);
        assert_eq!(st.worker_types.editing.as_deref(), Some(""), "a value click opens the editor at once");
    }

    #[test]
    fn push_transcript_entry_accumulates_per_session_and_trims_the_oldest() {
        let mut st = TuiState::default();
        st.push_transcript_entry("s1".into(), TranscriptEntry::UserMessage("hi".into()));
        st.push_transcript_entry("s1".into(), TranscriptEntry::TurnStarted);
        st.push_transcript_entry("s2".into(), TranscriptEntry::AgentText("unrelated".into()));
        assert_eq!(st.transcripts["s1"], vec![TranscriptEntry::UserMessage("hi".into()), TranscriptEntry::TurnStarted]);
        assert_eq!(st.transcripts["s2"], vec![TranscriptEntry::AgentText("unrelated".into())]);

        for _ in 0..ninox_ui_shared::transcript::MAX_ENTRIES + 10 {
            st.push_transcript_entry("s1".into(), TranscriptEntry::TurnEnded);
        }
        assert_eq!(st.transcripts["s1"].len(), ninox_ui_shared::transcript::MAX_ENTRIES, "oldest entries must be trimmed, not grow unbounded");
    }

    fn sources_state() -> TuiState {
        let mut st = brain_state(crate::tui::brain::Grouping::Tag);
        let open = handle_key(&mut st, key('S'));
        assert_eq!(open, Action::LoadBrainSources);
        st.brain.sources = crate::tui::sources::tests::sample();
        st
    }

    #[test]
    fn shift_s_opens_sources_and_esc_or_q_return_to_the_brain_list() {
        let mut st = brain_state(crate::tui::brain::Grouping::Tag);
        assert_eq!(handle_key(&mut st, key('S')), Action::LoadBrainSources);
        assert!(st.brain.sources.open);
        assert_eq!(handle_key(&mut st, code(KeyCode::Tab)), Action::None);
        assert!(!st.brain.graph_mode, "Tab doesn't reach the graph toggle underneath");
        handle_key(&mut st, code(KeyCode::Esc));
        assert!(!st.brain.sources.open);
        assert_eq!(st.view, View::Brain, "esc leaves the sub-view, not the tab");
        handle_key(&mut st, key('S'));
        handle_key(&mut st, key('q'));
        assert!(!st.brain.sources.open);
        assert_eq!(st.view, View::Brain);
        // The brain list's own keys are untouched once it's closed.
        assert_eq!(handle_key(&mut st, key('R')), Action::ReindexBrain);
    }

    #[test]
    fn typing_a_capital_s_into_the_brain_search_does_not_open_sources() {
        let mut st = brain_state(crate::tui::brain::Grouping::Tag);
        handle_key(&mut st, key('/'));
        handle_key(&mut st, key('S'));
        assert_eq!(st.brain.query, "S");
        assert!(!st.brain.sources.open);
    }

    #[test]
    fn c_does_not_cycle_the_catalogue_inside_sources() {
        let mut st = sources_state();
        let before = st.brain.active_catalogue;
        assert_eq!(handle_key(&mut st, key('c')), Action::None);
        assert_eq!(st.brain.active_catalogue, before);
    }

    #[test]
    fn sources_keys_move_show_details_and_sync() {
        let mut st = sources_state();
        handle_key(&mut st, key('j'));
        assert_eq!(st.brain.sources.selected().unwrap().name, "team-mlops");
        handle_key(&mut st, code(KeyCode::Enter));
        assert!(st.brain.sources.detail);
        handle_key(&mut st, code(KeyCode::Esc));
        assert!(!st.brain.sources.detail && st.brain.sources.open, "esc closes the details first");
        assert_eq!(handle_key(&mut st, key('s')), Action::SyncBrainSources(vec!["team-mlops".into()]));
        assert_eq!(
            handle_key(&mut st, key('S')),
            Action::SyncBrainSources(vec!["default".into(), "team-mlops".into(), "research-archive".into()])
        );
        handle_key(&mut st, key('G'));
        assert_eq!(st.brain.sources.selected().unwrap().name, "research-archive");
        handle_key(&mut st, key('g'));
        assert_eq!(st.brain.sources.cursor, 0);
    }

    #[test]
    fn sources_keys_toggle_edit_and_remove_with_a_confirm() {
        let mut st = sources_state();
        handle_key(&mut st, key('j'));
        assert_eq!(handle_key(&mut st, key('t')), Action::ToggleBrainSource("team-mlops".into()));
        handle_key(&mut st, key('e'));
        assert!(matches!(&st.modal, Some(Modal::SourceForm(f)) if f.form.editing.as_deref() == Some("team-mlops")));
        assert_eq!(handle_key(&mut st, code(KeyCode::Esc)), Action::None);
        assert_eq!(st.modal, None);
        handle_key(&mut st, key('d'));
        assert_eq!(st.modal, Some(Modal::Confirm(Pending::RemoveBrainSource("team-mlops".into()))));
        assert_eq!(handle_key(&mut st, key('n')), Action::None);
        handle_key(&mut st, key('d'));
        assert_eq!(handle_key(&mut st, key('y')), Action::RemoveBrainSource("team-mlops".into()));
    }

    #[test]
    fn the_builtin_default_source_cannot_be_edited_toggled_or_removed() {
        let mut st = sources_state();
        assert_eq!(st.brain.sources.selected().unwrap().name, "default");
        for k in ['e', 't', 'd'] {
            assert_eq!(handle_key(&mut st, key(k)), Action::None);
            assert_eq!(st.modal, None);
            assert_eq!(st.notice.as_ref().map(|n| n.text.as_str()), Some(crate::tui::sources::BUILTIN_NOTE));
        }
        assert_eq!(handle_key(&mut st, key('s')), Action::SyncBrainSources(vec!["default".into()]), "it can still be synced");
    }

    #[test]
    fn the_add_form_submits_a_validated_source_and_keeps_errors_in_the_form() {
        let mut st = sources_state();
        handle_key(&mut st, key('a'));
        assert!(matches!(&st.modal, Some(Modal::SourceForm(f)) if f.form.editing.is_none()));
        assert_eq!(handle_key(&mut st, code(KeyCode::Enter)), Action::None);
        assert!(matches!(&st.modal, Some(Modal::SourceForm(f)) if f.form.error.is_some()), "an invalid form stays open with its error");
        for c in "team-mlops".chars() {
            handle_key(&mut st, key(c));
        }
        handle_key(&mut st, code(KeyCode::Tab));
        handle_key(&mut st, code(KeyCode::Tab));
        for c in "/tmp/t".chars() {
            handle_key(&mut st, key(c));
        }
        assert_eq!(handle_key(&mut st, code(KeyCode::Enter)), Action::None, "the name is taken");
        handle_key(&mut st, code(KeyCode::BackTab));
        handle_key(&mut st, code(KeyCode::BackTab));
        handle_key(&mut st, key('2'));
        match handle_key(&mut st, code(KeyCode::Enter)) {
            Action::SaveBrainSource(form) => {
                assert_eq!(form.name, "team-mlops2");
                assert_eq!(form.location, "/tmp/t");
            }
            other => panic!("expected a save, got {other:?}"),
        }
        assert_eq!(st.modal, None);
    }

    #[test]
    fn help_lists_s_for_sources_and_the_sub_view_keys() {
        let brain = HELP.iter().find(|(g, _)| *g == "Brain tab (bare keys)").unwrap().1;
        assert!(brain.iter().any(|(k, v)| *k == "S" && v.starts_with("Sources")));
        let sources = HELP.iter().find(|(g, _)| *g == "Brain › Sources (after S)").unwrap().1;
        assert!(sources.iter().any(|(k, v)| *k == "s · S" && v.contains("sync every source")));
        assert!(sources.iter().any(|(k, _)| *k == "a · e · d"));
    }
}
