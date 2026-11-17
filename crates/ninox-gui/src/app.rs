use std::{collections::{HashMap, VecDeque}, sync::Arc, time::{Instant, SystemTime, UNIX_EPOCH}};

use ninox_core::{
    config::{AppConfig, EditorChoice, SendMechanism, ThemeVariant},
    events::{Engine, Event},
    slugify,
    store::UserRequestRow,
    types::*,
    BrainEntry, BrainIndex, MergedBrainIndex, QueryFilters,
};
use iced::{Element, Subscription, Task, Theme};
use tokio::sync::broadcast;

use ninox_ui_shared::restart::resume_plan;

use crate::{
    components::{
        catalogue_modal::CatalogueForm, fleet_islands::build_fleet_islands,
        session_detail::DetailPanel, spawn_modal::SpawnForm, terminal::TerminalState,
        workspaces_modal::WorkspacesModalState,
    },
    theme::{ColorScheme, Themes},
};

const MAX_NOTIFICATIONS: usize = 50;

/// The nav bar's pending-requests badge: its count text and hover hint, or
/// `None` when nothing is pending — shared by `components::shell::nav`.
pub(crate) fn pending_requests_badge(n: usize) -> Option<(String, String)> {
    (n > 0).then(|| {
        let count = if n > 99 { "99+".to_string() } else { n.to_string() };
        (count, format!("{n} pending user request{} — click to show", if n == 1 { "" } else { "s" }))
    })
}

/// The running binary's own version — shown in Settings and used as the
/// baseline for `ensure_version_check`. This crate's `CARGO_PKG_VERSION`,
/// not `ninox-core`'s, since it's specifically "what build is this person
/// actually running" (the two are guaranteed equal for a published
/// release — see `poller::poll_update_check`'s doc comment — but this is
/// the more literally correct source for a user-facing version display).
pub const NINOX_VERSION: &str = env!("CARGO_PKG_VERSION");

// ---------------------------------------------------------------------------
// View state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct SidebarState {
    /// Orchestrators whose worker lists are expanded in the tree. A set,
    /// not a single id — expanding one orchestrator must not collapse the
    /// others (user directive).
    pub expanded_orchestrators: std::collections::HashSet<OrchestratorId>,
    pub show_notifications:    bool,
    /// The nav bar's pending-requests dropdown (`components::shell::nav`'s
    /// badge toggles this) — mirrors `show_notifications`'s own dropdown.
    pub show_pending_requests: bool,
    /// The one pending-request row whose freeform answer input is revealed
    /// (click-to-reveal — see `pending_requests_panel`). `None` = all rows
    /// collapsed to their label/quick-actions.
    pub expanded_pending_request: Option<String>,
    /// Draft text for `expanded_pending_request`'s freeform input. Cleared
    /// whenever the expanded row changes or an answer is submitted.
    pub pending_request_draft:    String,
}

#[derive(Debug, Clone, Default)]
pub struct FleetFilter {
    pub query: String,
}

/// The top nav's global search box (G3) — `open` tracks whether the
/// dropdown is showing (set on typing or `⌘K`; cleared on opening a hit,
/// navigating, Esc, a press outside it, or the input losing focus).
/// `cursor` is the selected hit's flat index across all three result groups
/// combined (`GlobalSearchResults::flat`'s ordering), `None` meaning
/// nothing's been arrowed to yet. `results` is computed once per draft
/// change/focus (`GlobalSearchChanged`/`GlobalSearchFocus`), never inside
/// the view — `view()` re-runs on every `update()` regardless of what
/// changed (engine events, the 3s poll), and `search_brain`'s underlying
/// `BrainIndex::query("")` is an unbounded, unlimited full-entry-body scan,
/// so recomputing it there would re-run that query on every tick for as
/// long as the dropdown stayed open (found in review of PR #279).
#[derive(Debug, Clone, Default)]
pub struct GlobalSearchState {
    pub draft:   String,
    pub open:    bool,
    pub cursor:  Option<usize>,
    pub results: crate::components::global_search::GlobalSearchResults,
    /// Pointer over the field / the dropdown — a press elsewhere closes it.
    pub hover_field:    bool,
    pub hover_dropdown: bool,
}

impl GlobalSearchState {
    /// Every dismissal route except opening a hit keeps `draft`, so `⌘K`
    /// reopens on the same query.
    pub fn dismiss(&mut self) {
        self.open = false;
        self.cursor = None;
        self.hover_dropdown = false;
        // On a narrow top bar the field leaves the layout on close, so it
        // never gets the `on_exit` that would clear this.
        self.hover_field = false;
    }
}

/// `App.brain`'s backing enum — one catalogue's own index, or the union of
/// every enabled catalogue ("All sources") — moved to `ninox_core::brain`
/// so the TUI's catalogue picker shares it instead of reimplementing the
/// same `Single`/`All` delegation.
pub use ninox_core::brain::{BrainScope, BrainScopeId};

/// Which of the Brain tab's three views is showing (`Brain.dc.html`'s
/// List/Graph/Sources segmented control).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BrainMode {
    #[default]
    List,
    Graph,
    Sources,
}

/// Entry-list sort order (`Brain.dc.html`'s Recent/A-Z/Most-linked segmented
/// control). Lives in `ninox_core::brain` so the TUI's Brain tab shares it.
pub use ninox_core::brain::BrainSort;

/// On-demand registry check triggered when Settings opens (`ensure_version_check`)
/// — independent of the periodic background check in `ninox_core::lifecycle::poller`,
/// so opening Settings always shows a fresh answer rather than whatever the
/// last multi-hour poll happened to see.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum VersionCheckState {
    #[default]
    NotChecked,
    Checking,
    UpToDate,
    UpdateAvailable(String),
    /// `ApplyUpdate` finished successfully — set directly by
    /// `Message::UpdateApplied` (not by a fresh `ensure_version_check`)
    /// since the running process is still the old binary until restarted,
    /// so re-checking the registry would just report `UpdateAvailable`
    /// again for the exact version that was just installed.
    Installed,
    Failed,
}

/// One orchestrator's registered goals/plan doc, as last read from disk.
/// Keyed by `orchestrator_id` on `App::plan_docs` — see
/// `docs/superpowers/specs/2026-08-26-orchestrator-plan-tracking-design.md`.
/// Not `Clone`/`Debug`-derived: `text_editor::Content` supports neither.
#[derive(Default)]
pub struct PlanDocState {
    pub file_path:  Option<String>,
    pub blocks:     Vec<PlanBlockView>,
    last_mtime:     Option<std::time::SystemTime>,
    /// Set when the row exists but the file itself is missing/unreadable —
    /// distinct from "nothing registered" so the panel can say so.
    pub error:      Option<String>,
}

/// One rendered markdown block (heading/paragraph/list-item/code) — the
/// displayed text already has bullet/checkbox prefixes applied and
/// markdown syntax stripped (see `components::markdown_blocks`), paired
/// with a `text_editor::Content` so it's independently selectable.
pub struct PlanBlockView {
    pub kind:    crate::components::markdown_blocks::BlockKind,
    pub content: iced::widget::text_editor::Content,
    pub spans:   Vec<(std::ops::Range<usize>, crate::components::markdown_blocks::InlineStyle)>,
    pub links:   Vec<(String, String)>,
}

#[derive(Debug, Default)]
pub struct BrainViewState {
    pub entries:  Vec<BrainEntry>,
    pub loaded:   bool,
    pub selected: Option<String>,
    pub filter:   String,
    pub mode:     BrainMode,
    /// Active Kinds-column selection (`ninox_core::brain::normalize_kind`
    /// value); `None` is "All entries".
    pub kind_filter: Option<String>,
    /// Active Repo chip filter, single-select.
    pub repo_filter: Option<String>,
    /// Active Tag chip filter, single-select.
    pub tag_filter:  Option<String>,
    pub sort:        BrainSort,
    /// When `entries` was last (re)loaded — toolbar's "indexed X ago".
    /// Session-only, like the rest of this view's derived state.
    pub last_indexed: Option<std::time::Instant>,
    /// The in-place editor's draft, for an existing entry or a new one.
    pub editor: Option<crate::components::brain_editor::BrainEditor>,
    /// The entry id awaiting the inline delete confirm.
    pub delete_confirm: Option<String>,
    /// What the running write/delete job is doing, while it runs.
    pub job_running: Option<String>,
    /// The last write/delete job's outcome.
    pub notice: Option<Result<String, String>>,
    /// Parsed markdown body of `selected`, preprocessed so `[[wikilinks]]`
    /// render as clickable brain links (`internal_link`).
    pub markdown: Vec<iced::widget::markdown::Item>,
    /// Entries that link to `selected` ("cited by"), fetched from the index
    /// in the `BrainSelectEntry` handler rather than re-derived from
    /// `entries` on every render. Empty when nothing is selected.
    pub backlinks: Vec<BrainEntry>,
    /// Entries related to `selected` — direct links, then co-citation, then
    /// shared tags (see `BrainIndex::related`) — same fetch-on-select
    /// pattern as `backlinks`.
    pub related: Vec<BrainEntry>,
    /// Resolved targets of `selected`'s own links ("links to"), via
    /// `BrainIndex::outlinks` — same fetch-on-select pattern as `backlinks`.
    pub outlinks: Vec<BrainEntry>,
    /// Graph edges as undirected, deduplicated node-index pairs into
    /// `entries`, resolved from `BrainIndex::links_all()` once per data
    /// change (`NavigateBrain` / `BrainReindex` / `BrainSwitchCatalogue`) —
    /// see `App::refresh_brain_edges`. Used by the List view's "Most
    /// linked" sort (`brain_list::visible_entries`) and, from PR2, the
    /// Graph view. Never re-derived per render.
    pub edges: Vec<(usize, usize)>,
    /// Graph view state: scope, filters, computed layout, canvas cache.
    pub graph: crate::components::brain_graph::BrainGraphState,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub enum View {
    /// The v1 shell's default tab — island/tree fleet view. Placeholder
    /// centre content until the Fleet-islands PR (MLOPS-5339 PR 2).
    #[default]
    Fleet,
    /// The existing Kanban (Working/PR Open/CI Failed/Review/Mergeable/
    /// Done), restyled — the "Board" nav tab. Was `View::FleetBoard`.
    Board { scope: Option<OrchestratorId> },
    SessionDetail { session_id: SessionId, panel: DetailPanel },
    Brain,
    /// Placeholder centre content — no GUI implementation yet.
    Plans,
    /// Placeholder centre content — no GUI implementation yet.
    WorkerTypes,
    /// Placeholder centre content — no GUI implementation yet.
    Remotes,
    /// Idle live workers grouped by orchestrator — see `components::idle_view`.
    Idle,
    Settings,
}

// ---------------------------------------------------------------------------
// App model
// ---------------------------------------------------------------------------

/// UI zoom bounds and step for the Cmd/Ctrl +/-/0 shortcuts. `zoom` feeds
/// Iced's native `scale_factor` (see `main.rs`), so it scales the whole UI.
const ZOOM_MIN:  f64 = 0.5;
const ZOOM_MAX:  f64 = 3.0;
const ZOOM_STEP: f64 = 0.1;

/// Whether a click on `key` is the second of a double-click, recording it
/// in `last` either way. iced's `mouse_area` hands every click the same
/// message, so the distinction is made here against wall-clock time.
fn completes_double_click<K: PartialEq + Clone>(last: &mut Option<(K, Instant)>, key: &K) -> bool {
    const DOUBLE_CLICK_WINDOW: std::time::Duration = std::time::Duration::from_millis(400);
    let now = Instant::now();
    let is_double = last.as_ref().is_some_and(|(prev, at)| prev == key && now.duration_since(*at) < DOUBLE_CLICK_WINDOW);
    *last = Some((key.clone(), now));
    is_double
}

pub struct App {
    pub engine:             Arc<Engine>,
    pub config:             AppConfig,
    /// Set once at startup when `config.toml` fails to parse — stays until
    /// the file is fixed and the app restarts (no live file-watching). See
    /// MLOPS-5457.
    pub config_warning:     Option<String>,
    pub themes:             Themes,
    pub scheme:             ColorScheme,
    pub active_variant:     ThemeVariant,
    pub orchestrator_root:  std::path::PathBuf,
    /// Where the store's sqlite file lives — threaded in at startup (mirrors
    /// `tui::run`'s own `db_path` param) so `restart::execute` has it for the
    /// rare self-restart branch; unused for every other action.
    pub db_path:            std::path::PathBuf,
    pub orchestrators:      Vec<Orchestrator>,
    pub sessions:        HashMap<SessionId, Session>,
    /// 1-based place of each queued session in its location's spawn queue.
    pub queue_positions: HashMap<SessionId, usize>,
    /// Header counter, e.g. "6/6 workers running locally · 3 orchestrators · 2 queued".
    pub limits_headline: String,
    pub brain:           BrainScope,
    pub brain_view:      BrainViewState,
    /// All selectable knowledge-base catalogues (`AppConfig::catalogue_options()`,
    /// snapshotted at startup — see `BrainSwitchCatalogue`).
    pub catalogues:      Vec<ninox_core::config::CatalogueRef>,
    /// Index into `catalogues` for the catalogue `brain` is currently open
    /// on, or `None` when `brain` is `BrainScope::All` ("All sources").
    pub active_catalogue: Option<usize>,
    pub prs:             HashMap<PrId, PR>,
    pub ci_status:       HashMap<PrId, CIStatus>,
    /// Every PR each session is responsible for (tracked, watched, extra),
    /// open first; refreshed on the store poll.
    pub session_prs:     HashMap<SessionId, Vec<ninox_core::session_prs::SessionPr>>,
    /// Latest GitHub rate-limit reading the poller stored, for the budget readout.
    pub github_budget:   Option<ninox_core::github_budget::GithubBudget>,
    store_poll_tick:     u32,
    pub review_threads:  HashMap<PrId, Vec<Comment>>,
    /// Read-only `text_editor` buffer per comment id, so Marginalia comment
    /// bodies can be selected and copied. iced needs the buffer to outlive
    /// the view, so it cannot be built on the fly while rendering — instead
    /// `ensure_comment_editors` hydrates a PR's entries lazily, whenever its
    /// session's Marginalia panel (Split/Info) might have something new to
    /// show it. Absent = not built yet (the Marginalia card renders blank
    /// for that comment until then).
    pub comment_editors: HashMap<i64, iced::widget::text_editor::Content>,
    /// On-demand `git diff` text per session's workspace (`ensure_diff`).
    /// Absent = not fetched yet (render "Loading…"); `Some(None)` = fetched,
    /// no diff (no workspace recorded, or a clean working tree).
    pub diffs:           HashMap<SessionId, Option<String>>,
    /// Registered goals/plan doc per orchestrator, refreshed by
    /// `ensure_plan` — see `PlanDocState`.
    pub plan_docs:       HashMap<OrchestratorId, PlanDocState>,
    pub notifications:   VecDeque<Notification>,
    /// Highest store `seq` already shown; only rows past it raise a desktop
    /// alert, so the backlog at startup never does.
    notif_cursor:        i64,
    #[cfg(test)]
    alerted:             Vec<String>,
    /// Every open (`pending`) `ninox user-request`, fleet-wide — populated
    /// purely by polling `Store::open_user_requests(None)` on each
    /// `PollSessions` tick (no event source for this; see the persistent
    /// Pending Requests panel). Rendered unconditionally in the sidebar.
    pub pending_user_requests: Vec<UserRequestRow>,
    /// Ghost plans (`ninox plan propose`), re-read on the db-poll tick.
    pub ghost: crate::components::ghost_plans::GhostUi,
    pub orch_plans: crate::components::orchestrator_plans::OrchestratorPlansUi,
    /// Why this process failed to register as a human console, if it did —
    /// every answer attempt will fail verification, so the Pending Requests
    /// panel shows it for the app's whole lifetime. Set once at launch.
    pub human_console_error: Option<String>,
    /// The last failed answer/dismiss from the Pending Requests panel,
    /// shown there until the next one succeeds.
    pub user_request_error: Option<String>,
    /// Messages delivered to each session since the user last had it open
    /// (`Event::MessagesDelivered`). Drives the sidebar's per-row badge;
    /// cleared by `NavigateSession`.
    pub unread_messages: HashMap<SessionId, u64>,
    /// See `Event::HeldMessages`.
    pub held_messages: HashMap<SessionId, (usize, usize)>,
    pub queued_by_sender: HashMap<SessionId, usize>,
    /// True while an `ApplyUpdate`-triggered `ninox update` subprocess is
    /// running — disables the "Update now" action so a second click can't
    /// spawn a duplicate install.
    pub update_in_progress: bool,
    /// Result of the on-demand check `ensure_version_check` fires whenever
    /// Settings opens — drives the version line's "up to date"/"update
    /// available" state (spec: Settings panel).
    pub version_check:   VersionCheckState,
    /// True while a `ConfirmRestartAll`-triggered batch restart is running —
    /// disables the Fleet card's action and keeps a second click from
    /// spawning a duplicate batch.
    pub restart_all_in_progress: bool,
    pub server:          ServerPanel,
    pub sidebar:         SidebarState,
    pub view:            View,
    /// The user's preferred worker panel — global, not per-session: the
    /// last tab chosen in any session detail, applied when opening a
    /// worker (resetting to Split on every navigation was jarring).
    pub worker_panel:    crate::components::session_detail::DetailPanel,
    pub terminals:       HashMap<SessionId, TerminalState>,
    /// ACP-native sessions' live transcripts; a session listed here shows
    /// its transcript instead of a terminal.
    pub transcripts:     HashMap<SessionId, Vec<ninox_core::runtime::TranscriptEntry>>,
    /// One hidden tmux client per on-screen session (the "view"). Dropping
    /// an entry kills the client process; the session itself stays detached
    /// and running.
    clients: HashMap<SessionId, ninox_core::client::AttachedClient>,
    /// Where each viewed session's terminal attach stands: the retry count,
    /// the pending prepare/retry task, and the failure shown with Reconnect.
    pub attach: HashMap<SessionId, crate::attach::AttachProgress>,
    next_attach_token: u64,
    /// Clients seen exited on the previous reap tick, so a close event that
    /// is merely in flight gets one tick to arrive before one is synthesised.
    reap_pending: HashMap<SessionId, u64>,
    /// Monotonically increasing identity handed to every spawned
    /// `AttachedClient`. Lets `Event::ClientOutput`/`ClientClosed` handlers
    /// tell a stale client's events (from one that lost a concurrent-attach
    /// race, or was superseded by a fresh NavigateSession) apart from the
    /// currently-live client for the same session_id.
    next_client_generation: u64,
    /// Per-app-run `models_cmd` discovery cache, keyed by harness name.
    /// `Some(None)` = attempted and failed (pickers fall through to the
    /// spec's known_models); absent = not attempted yet.
    pub model_lists:     HashMap<String, Option<Vec<String>>>,
    /// Settings-view UI state (custom-model input for the Workers card).
    pub settings:        crate::components::settings_panel::SettingsState,
    pub spawn_modal:     Option<SpawnForm>,
    /// The Spawn dialog's multi-line "Initial prompt" — kept beside the form
    /// because `text_editor::Content` isn't `Clone`.
    pub spawn_prompt:    iced::widget::text_editor::Content,
    /// The `?` keyboard reference is showing.
    pub help_open: bool,
    /// Add-a-catalogue journal-entry modal, opened from the `+` at the
    /// right edge of the brain view's volume plate. Exclusive with
    /// `spawn_modal` in practice (spawn lives in other views); when both are
    /// somehow set, rendering and Esc both give `spawn_modal` precedence.
    pub catalogue_modal: Option<CatalogueForm>,
    /// Sources view's docked 380px edit panel (Add or Edit) — `None` when
    /// closed. Unlike `catalogue_modal` this isn't a backdrop-dimmed modal;
    /// it renders inline alongside the card grid, matching
    /// `BrainSources.dc.html`'s layout.
    pub sources_edit: Option<ninox_ui_shared::brain_sources::SourceEditForm>,
    /// Per-catalogue live status for the Sources view, keyed by path —
    /// computed on demand (opening the view, after Save/Remove/Sync now),
    /// never kept fresh automatically (no background scheduler).
    pub source_status: HashMap<std::path::PathBuf, ninox_ui_shared::brain_sources::SourceStatus>,
    /// Session-only "recent sync activity" log (`BrainSources.dc.html`'s
    /// plain mono list), newest first, capped so it can't grow unbounded
    /// across a long-running session.
    pub source_sync_log: VecDeque<String>,
    /// "Restart all agents" confirmation modal (Fleet card, Settings) is
    /// open. A plain bool rather than `Option<Form>` like the modals above —
    /// there's no form data to carry; the live count it displays is read
    /// straight from `sessions` at render time.
    pub restart_all_confirm: bool,
    /// The "Restore fleet" modal, while open.
    pub restore_modal: Option<crate::components::restore_modal::RestoreModal>,
    /// The Fleet toolbar's "Clean up finished" confirm, while open.
    pub reap_modal: Option<crate::components::reap_modal::ReapModal>,
    pub delete_modal: Option<crate::components::delete_modal::DeleteModal>,
    /// Current terminal canvas dimensions, kept in sync by WindowResized.
    /// Used as the source of truth for all start_streaming + TerminalState::new calls.
    pub terminal_cols:   u16,
    pub terminal_rows:   u16,
    pub window_width:    f32,
    /// How much of the nav bar's left edge the macOS traffic lights take
    /// (0 in full screen), as `titlebar::place_traffic_lights` last measured
    /// it, in this app's logical px; the nav brand clears it.
    pub traffic_lights_end: Option<f32>,
    pub window_height:   f32,
    pub sidebar_width:   f32,
    pub info_width:      f32,
    /// Whether the session page's terminal owns the keyboard. The page
    /// opens unfocused; a click on the terminal, `c` or Connect focuses it,
    /// `Ctrl+]` or a click elsewhere releases it.
    pub terminal_focused: bool,
    /// Set by the terminal canvas's own click so the app-wide
    /// `MousePressed` for that same click (delivered after it) doesn't
    /// treat it as a click outside the terminal.
    pub terminal_click_marker: bool,
    pub session_more_open: bool,
    pub poll_tick: u32,
    pub session_more_hover: bool,
    /// The session page's pending inline confirm (Restart or Kill).
    pub session_confirm: Option<crate::components::session_detail::Confirmable>,
    pub workspace_facts: HashMap<SessionId, ninox_core::workspace::WorkspaceFacts>,
    pub session_activity: HashMap<SessionId, Vec<ninox_core::types::MessageEvent>>,
    pub drag:            Option<DragTarget>,
    pub fleet_filter:    FleetFilter,
    pub last_fleet_scope: Option<OrchestratorId>,
    pub global_search:   GlobalSearchState,
    /// Global UI zoom factor fed to Iced's native `scale_factor`. Driven by
    /// the Cmd/Ctrl +/-/0 shortcuts, clamped to `[ZOOM_MIN, ZOOM_MAX]`, and
    /// mirrored into `config.zoom` so it persists across restarts.
    pub zoom:            f64,
    /// Draft text for the always-on "ninox prompt" dock. Query/action
    /// routing isn't wired up yet (see `Message::NinoxPromptSubmitted`) —
    /// this only tracks what's currently typed.
    pub ninox_prompt_draft: String,
    /// Fleet tab toolbar/selection state (MLOPS-5339 slice 2) — the island
    /// grid itself is recomputed from `sessions`/`orchestrators` on every
    /// render via `ninox_overview::islands::build_islands`, never cached
    /// here.
    pub fleet_sort:            ninox_overview::islands::IslandSort,
    pub fleet_machine_filter:  FleetMachineFilter,
    /// Card density toggle. `true` ("Compact", the FleetAtScale default)
    /// folds each island's done/stopped members into one row by default;
    /// `false` ("Full cards") shows them unfolded by default. Either way
    /// `fleet_expanded_folds` flips a single island's fold relative to this
    /// baseline (see `Message::FleetToggleFold`).
    pub fleet_compact:         bool,
    pub fleet_expanded_folds:  std::collections::HashSet<ninox_overview::islands::IslandKey>,
    /// Fleet tab nav/selection state (MLOPS-5339 slice 3) — which island is
    /// selected/opened, the current node selection within an opened island,
    /// and the left session tree's collapsed flag. A single
    /// `ninox_overview::nav::FleetNav` rather than separate fields so
    /// "Esc preserves the selection" etc. come from its own tested behavior
    /// instead of being re-implemented here.
    pub fleet_nav: ninox_overview::nav::FleetNav,
    /// Back/forward history of user-initiated navigations.
    pub nav: crate::nav_history::NavHistory,
    /// Set by engine-driven view changes so `apply_settled` records nothing.
    nav_auto: bool,
    /// The last island card clicked and when, used only to detect a
    /// double-click (opens the island) vs a single click (just selects it)
    /// in `Message::FleetSelectIsland` — iced hands every click the same
    /// message, so the distinction is made here against wall-clock time
    /// rather than in the view.
    pub fleet_last_island_click: Option<(ninox_overview::islands::IslandKey, Instant)>,
    /// Same as `fleet_last_island_click`, for session cards in an opened
    /// island: a double-click opens the session view.
    pub fleet_last_node_click: Option<(ninox_overview::node::NodeId, Instant)>,
    /// The detail panel's "Send message" composer (slice 4): the node it's
    /// addressed to, and its draft text. `None` = composer closed. Only one
    /// composer can be open at a time, same as `spawn_modal`/
    /// `catalogue_modal`.
    pub fleet_message_draft: Option<(ninox_overview::node::NodeId, String)>,
    /// Worker Types tab state (list + the currently-edited draft). Not
    /// populated at construction — `NavigateWorkerTypes` refreshes it from
    /// disk on first visit, same as `brain_view`/`catalogues` are seeded
    /// from config rather than carried from `App::new`.
    pub worker_types: crate::components::worker_types::WorkerTypesState,
    /// Remotes tab state (machine list + remove-confirm/add-remote flow).
    /// Not populated at construction — `NavigateRemotes` refreshes it from
    /// disk on first visit, same as `worker_types`.
    pub remotes: crate::components::remotes::RemotesState,
    pub idle: crate::components::idle_view::IdleState,
    /// Who the ninox prompt's text goes to (G2) — defaults to the prompt
    /// agent, overridable via the `→ ninox ▾` target chip's picker.
    pub ninox_prompt_target: crate::prompt_agent::PromptTarget,
    /// The reply drawer (`PromptReply.dc.html`): the prompt conversation,
    /// expanded or collapsed to one line above the prompt bar.
    pub ninox_reply_drawer: crate::prompt_agent::ReplyDrawer,
    /// Drawer answers in "Select" mode, keyed by `Turn::asked_at_ms`.
    pub ninox_turn_select: HashMap<i64, iced::widget::text_editor::Content>,
    /// Frame counter for the drawer's "is thinking" animation; advances
    /// only while [`App::wants_thinking_tick`].
    pub thinking_tick: u64,
    /// macOS Reduce Motion, re-read on each ask: the thinking placeholder
    /// and status dot hold still.
    pub reduce_motion: bool,
    /// Monotonic counter bumped on every submit/clear — a delivery result
    /// or poll tick carries the generation it was issued for, so a stale
    /// one (superseded by a clear or a newer submit) can never clobber the
    /// current turn. Same pattern as `next_client_generation`.
    pub ninox_prompt_generation: u64,
    /// Submitted prompts, oldest first, for Up/Down recall.
    pub ninox_prompt_history: Vec<String>,
    /// Position in `ninox_prompt_history` while recalling (`None` = not
    /// currently recalling — the field holds a fresh, unsubmitted draft).
    pub ninox_prompt_history_cursor: Option<usize>,
    /// The in-progress draft at the moment Up first started recall, so
    /// Down can restore it once the cursor reaches the newest history
    /// entry again, instead of leaving the last-recalled entry behind.
    pub ninox_prompt_draft_before_recall: Option<String>,
    /// `(count, total_bytes)` of leftover worktrees across every repo ninox
    /// has spawned into — drives the nav chip (see `shell::nav`), hidden
    /// entirely when `.0 == 0`. Refreshed on the same background tick that
    /// updates `sessions` (`Message::PollSessions`), throttled by
    /// `workspaces_last_refresh` since a full cross-repo `git status` pass
    /// is too heavy to repeat on every 3s tick.
    pub workspaces_summary: (usize, u64),
    workspaces_last_refresh: Option<Instant>,
    /// Leftover-workspaces modal, opened from the nav chip. Same `None` =
    /// closed shape as `spawn_modal`/`catalogue_modal`.
    pub workspaces_modal: Option<WorkspacesModalState>,
}

/// The Fleet tab toolbar's Machine filter, shared with the TUI.
pub use ninox_overview::islands::MachineFilter as FleetMachineFilter;

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub enum DragTarget { InfoPanel }

// `Session` grew when statusline context/cost fields were added, and `EngineEvent(Event)`
// wraps it by value; boxing `Event`'s Session-carrying variants would ripple across every
// call site that constructs or matches on `Event::SessionUpdated`/`SessionSpawned` across
// the workspace, which is out of scope here.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum Message {
    EngineEvent(Box<Event>),
    /// Was `NavigateFleet` — renamed when the v1 shell introduced a new
    /// `Fleet` tab, freeing `NavigateFleet` for it (below).
    NavigateBoard { scope: Option<OrchestratorId> },
    /// The v1 shell's default tab (island/tree fleet view).
    NavigateFleet,
    NavigatePlans,
    NavigateWorkerTypes,
    NavigateRemotes,
    NavigateIdle,
    /// Idle tab: send one orchestrator (`Some`) or each orchestrator with
    /// idle workers (`None`) its own list.
    IdlePrompt(Option<String>),
    IdlePromptDone(Vec<String>),
    /// Idle tab: free one worker's build output (`target/` etc.).
    IdleClean(String),
    IdleCleanDone(Result<String, String>),
    /// Remotes tab: "Remove" was clicked on a row — asks for confirm before
    /// `ninox_ui_shared::machine::remove` actually runs.
    RemoteRemoveRequest(String),
    RemoteRemoveConfirm,
    RemoteRemoveCancel,
    /// Remotes tab: opens the "Add remote" modal with a blank form.
    RemoteAddOpen,
    RemoteAddHostChanged(String),
    RemoteAddLabelChanged(String),
    /// Closes the modal, killing the embedded `machine add` subprocess (if
    /// one was spawned) by dropping its `AttachedClient`.
    RemoteAddCancel,
    /// Validates the form, then spawns the embedded `machine add`
    /// subprocess on success.
    RemoteAddSubmit,
    /// Dismisses a finished add's frozen terminal + result (the modal's
    /// "Close" button) — distinct from `RemoteAddCancel`, which aborts a
    /// still-running one.
    RemoteAddClose,
    /// Worker Types tab: select an existing type by canonical name, loading
    /// its effective (builtin-defaults + overlay) spec into the form.
    WorkerTypeSelect(String),
    /// Worker Types tab: "+ New type" — blanks the form for a fresh draft.
    WorkerTypeNewDraft,
    /// Worker Types tab: the Name field, editable only while drafting a new
    /// type (an existing type's name is immutable once created).
    WorkerTypeNameChanged(String),
    WorkerTypeBriefAction(iced::widget::text_editor::Action),
    WorkerTypeHarnessChanged(String),
    WorkerTypeModelChanged(String),
    WorkerTypeUseWhenChanged(String),
    /// Worker Types tab: edits the pending rule text for the allow/deny/ask
    /// chip row's "add" input — not yet committed to the list.
    WorkerTypeAllowDraftChanged(String),
    WorkerTypeDenyDraftChanged(String),
    WorkerTypeAskDraftChanged(String),
    /// Commits the pending draft (comma-splittable, so pasting "A, B" adds
    /// both at once) into the rule list as one or more chips.
    WorkerTypeAllowRuleAdd,
    WorkerTypeDenyRuleAdd,
    WorkerTypeAskRuleAdd,
    /// Removes one chip from the rule list by index.
    WorkerTypeAllowRuleRemove(usize),
    WorkerTypeDenyRuleRemove(usize),
    WorkerTypeAskRuleRemove(usize),
    /// Toggles one name (or `"*"`) in the can-spawn multi-select — `"*"` and
    /// named entries are mutually exclusive (see
    /// `worker_types::toggle_can_spawn`).
    WorkerTypeCanSpawnToggle(String),
    WorkerTypeToggleCanManageTypes,
    /// Worker Types tab: `create` (drafting) or `edit` (existing) the
    /// current form, passing `caller_session_id: None` — the GUI is always
    /// "the user" by `worker_type_cli::authorize_mutation`'s own contract.
    WorkerTypeSave,
    WorkerTypeReset,
    WorkerTypeDelete,
    WorkerTypeExport,
    WorkerTypeImportPathChanged(String),
    WorkerTypeImport,
    /// Collapse/expand the v1 shell's left session tree (`[` or the rail
    /// button).
    ToggleTreeCollapsed,
    /// Fleet tab: an island card was clicked. Only highlights it — a second
    /// click within the double-click window (checked in `App::apply`
    /// against `fleet_last_island_click`) promotes this to `FleetOpenIsland`
    /// instead.
    FleetSelectIsland(ninox_overview::islands::IslandKey),
    /// A click on empty Fleet board space: deselects the card.
    FleetClearBoardSelection,
    /// Fleet tab: open an island into the full-card view (double-click/
    /// Enter on a card, or clicking an orchestrator row in the tree) —
    /// switches to the Fleet tab if needed.
    FleetOpenIsland(ninox_overview::islands::IslandKey),
    /// Fleet tab: return from an opened island to the grid, keeping it
    /// selected (Esc, or the breadcrumb's "Fleet" link).
    FleetCloseIsland,
    /// Click a worker (or standalone session) row in the tree — opens its
    /// owning island with that node selected, switching to the Fleet tab.
    FleetOpenNode(ninox_overview::node::NodeId),
    /// Select a card inside an opened island's full-card view. Highlights
    /// it only — the detail panel/bottom action row are slice 4.
    FleetSelectNode(ninox_overview::node::NodeId),
    /// A session card in an opened island was clicked: `FleetSelectNode`,
    /// or `NavigateSession` when it's the second click of a double-click.
    FleetNodeCardPressed(ninox_overview::node::NodeId),
    /// A session row inside a Fleet island card was clicked:
    /// `FleetSelectIsland` for its island (the row's `mouse_area` captures
    /// the press before the card's own), or `NavigateSession` when it's
    /// the second click of a double-click on the same row.
    FleetIslandMemberPressed { island: ninox_overview::islands::IslandKey, id: ninox_overview::node::NodeId },
    /// Fleet tab toolbar: Machine filter chip pressed.
    FleetFilterMachine(FleetMachineFilter),
    /// Fleet tab toolbar: Sort chip pressed.
    FleetSetSort(ninox_overview::islands::IslandSort),
    /// Fleet tab toolbar: Full cards / Compact chip pressed (`true` = Compact).
    FleetSetCompact(bool),
    /// Fleet tab: click on an island's folded `▸ N done · M stopped` row (or
    /// its unfolded equivalent) — flips that one island's fold state
    /// relative to `fleet_compact`'s baseline.
    FleetToggleFold(ninox_overview::islands::IslandKey),
    /// Detail panel / action row (slice 4): "Send message" pressed for
    /// `id` — opens the composer, replacing any other node's in-progress
    /// draft.
    FleetSendMessageOpen(ninox_overview::node::NodeId),
    /// The open composer's draft text changed.
    FleetSendMessageChanged(String),
    /// The open composer was dismissed without sending.
    FleetSendMessageCancel,
    /// Submit the open composer's draft — delivers it via
    /// `ninox_core::messaging::deliver_message` and closes the composer.
    FleetSendMessageSubmit,
    /// The always-on ninox-prompt draft changed.
    NinoxPromptChanged(String),
    /// Enter pressed in the ninox prompt (G2) — resolves delivery via
    /// `prompt_agent::resolve_delivery`, pushes history, opens the reply
    /// drawer on a new turn, and dispatches the actual send.
    NinoxPromptSubmitted,
    /// One step of the reply drawer's "is thinking" animation.
    ThinkingTick,
    /// `/` pressed globally (G1) — focuses the ninox prompt's `text_input`.
    /// `Cmd+K` is reserved for the top-bar search (G3), not this field.
    /// Esc-to-blur needs no handler: `iced`'s `text_input` already
    /// unfocuses itself on Escape.
    FocusNinoxPrompt,
    /// `Cmd+F` pressed while the Brain tab is active — focuses the Brain
    /// List view's search field (see `brain_list::search_input_id`'s doc
    /// comment for the full shortcut map).
    BrainFocusSearch,
    /// The target chip's picker chose a label (G2) — "ninox" resolves to
    /// `PromptTarget::PromptAgent`, anything else to the orchestrator of
    /// that name.
    NinoxTargetPicked(String),
    /// A submit's delivery (or a live poll) resolved.
    /// `generation` is ignored if stale (see `App::ninox_prompt_generation`);
    /// `Ok` carries where the answer will be read from.
    NinoxPromptDelivered { generation: u64, result: Result<crate::prompt_agent::AnswerSource, String> },
    /// A poll of the latest turn's answer, the prompt agent's context
    /// figure and the store's recent message events (for relays).
    NinoxPromptPolled { generation: u64, poll: crate::prompt_agent::PromptPoll },
    /// `⌘⇧K`, the reply drawer's "Clear context" button, or the target
    /// chip menu's equivalent (G2) — calls `prompt_agent::clear`.
    NinoxPromptClear,
    /// `prompt_agent::clear`'s result (same generation-gating as a reply).
    NinoxPromptCleared { generation: u64, target_label: String, result: Result<(), String> },
    /// The drawer's "Collapse" button or Esc.
    NinoxDrawerCollapse,
    NinoxTurnSelectToggle(i64),
    NinoxTurnSelectAction(i64, iced::widget::text_editor::Action),
    /// The collapsed one-line indicator was clicked.
    NinoxDrawerReopen,
    /// Esc that a focused `text_input` captured (it unfocuses itself, and
    /// `RawKey` never sees it) — still closes search / collapses the drawer.
    EscapeCaptured,
    /// Any left mouse press, captured or not — closes the global search
    /// when the pointer isn't over it.
    MousePressed,
    /// The window lost OS focus.
    WindowUnfocused,
    /// The pointer entered (`true`) or left the search field + dropdown.
    GlobalSearchHover(crate::components::global_search::SearchPart, bool),
    /// Up/Down in the ninox prompt recall history (G2).
    NinoxPromptHistoryUp,
    NinoxPromptHistoryDown,
    /// Global Up/Down reached `RawKey` (every `text_input` ignores arrow
    /// keys even while focused, unlike other characters — see the
    /// `RawKey` handler) with a check of which widget (if any) is
    /// currently focused, via `iced::advanced::widget::operate` +
    /// `operation::focusable::find_focused`. Only resolves to history
    /// recall if that's the ninox prompt itself, so Up/Down typed into
    /// some other focused field is never hijacked.
    NinoxPromptHistoryFocusChecked(bool, iced::advanced::widget::Id),
    /// `⌘K`, global (G3) — focuses the top-bar search field and opens its
    /// dropdown (showing "recent" — really just the first few of
    /// everything — even before anything's typed).
    GlobalSearchFocus,
    /// The global search field's draft changed — always (re)opens the
    /// dropdown and resets the cursor (typing exits any prior arrow-key
    /// selection, same convention as G2's prompt history recall).
    GlobalSearchChanged(String),
    /// Enter in the global search field — opens the cursored hit, or the
    /// first hit if nothing's been arrowed to yet.
    GlobalSearchEnterPressed,
    /// Global `↑`/`↓` reached `RawKey` (every `text_input` ignores arrow
    /// keys even while focused) with a check of which widget is currently
    /// focused, via `iced::advanced::widget::operate` +
    /// `operation::focusable::find_focused` — only moves the search
    /// dropdown's cursor if that's the search field itself, same pattern as
    /// G2's `NinoxPromptHistoryFocusChecked`.
    GlobalSearchCursorFocusChecked(bool, iced::advanced::widget::Id),
    /// A hit (from the dropdown, or Enter) was opened — dispatches the
    /// right navigation message(s) for its kind and closes the dropdown.
    GlobalSearchOpenHit(crate::components::global_search::GlobalSearchHit),
    NavigateSession(SessionId),
    /// Attach argv resolved — spawn the hidden tmux client for this session.
    ClientAttach { session_id: SessionId, argv: Vec<String> },
    /// Result of looking up the pane and its attach command for attempt `token`.
    AttachPrepared { session_id: SessionId, token: u64, outcome: crate::attach::Prepared },
    /// Backoff elapsed for attempt `token`.
    AttachRetry { session_id: SessionId, token: u64 },
    /// The first-frame budget of client `generation` ran out.
    AttachWatchdog { session_id: SessionId, generation: u64 },
    /// Notice attach children that exited without their `ClientClosed`
    /// being delivered.
    AttachReap,
    /// The Reconnect button of a view whose attach gave up.
    ReconnectTerminal(SessionId),
    /// Toggle an orchestrator's worker list open/closed in the tree.
    SelectOrchestrator(OrchestratorId),
    SpawnSession,
    SpawnFormName(String),
    SpawnFormKind(crate::components::spawn_modal::SpawnKind),
    SpawnFormWorkspace(String),
    SpawnFormHarness(String),
    SpawnFormModel(String),
    SpawnFormCustomModel(String),
    SpawnFormToggleBrain(String),
    SpawnFormBrainWrite(String),
    /// The Type picker chose a worker type (MLOPS-5416, G4) — always one of
    /// `worker_type_registry().user_facing_names()`, never a hidden type.
    SpawnFormWorkerType(String),
    SpawnFormPrompt(iced::widget::text_editor::Action),
    SpawnFormConfirm,
    SpawnFormCancel,
    /// A dialog-spawned session's launch finished: attach if it started, then
    /// deliver `first_message` (the initial prompt as composed for its kind).
    SpawnLaunched { session_id: SessionId, argv: Result<Vec<String>, String>, first_message: Option<String>, spawner: Option<String> },
    /// A spawn/resume/re-file never started; `cause` is the one-line reason.
    SessionLaunchFailed { session_id: SessionId, action: &'static str, cause: String },
    SpawnPromptDelivered { session_id: SessionId, result: Result<(), String> },
    /// Opened from the `+` at the right edge of the brain view's volume
    /// plate (`components::brain_panel::volume_plate`).
    CatalogueModalOpen,
    CatalogueFormName(String),
    CatalogueFormPath(String),
    CatalogueFormConfirm,
    CatalogueFormCancel,
    /// Nav-bar "N leftover workspaces" chip clicked.
    WorkspacesModalOpen,
    WorkspacesModalClose,
    /// A fresh row load completed — both the periodic background refresh
    /// and the modal's own open/post-prune reloads funnel through this one
    /// message (see `App::workspaces_rows_task`).
    WorkspacesRowsLoaded(Vec<crate::components::workspaces_modal::WorkspaceRowView>),
    /// First click previews what would be pruned; a second click (while
    /// already previewing) confirms and actually prunes.
    WorkspacesPruneClicked,
    WorkspacesPruneDone(Result<usize, String>),
    SwitchDetailPanel(crate::components::session_detail::DetailPanel),
    /// A non-edit `text_editor::Action` (click/drag/select/scroll) from one
    /// of the read-only per-block widgets in the Plan panel — applied to
    /// `plan_docs[orchestrator_id].blocks[block].content` so
    /// selection/copy work; `Action::Edit(_)` is deliberately dropped.
    PlanEditorAction {
        orchestrator_id: OrchestratorId,
        block: usize,
        action: iced::widget::text_editor::Action,
    },
    /// Stop a session's agent and mark it terminal. Its row and worktree
    /// stay; removing them is `DeleteAsk`'s job.
    KillSession(SessionId),
    /// Open the delete confirm for a session (and, for an orchestrator,
    /// everything under it).
    DeleteAsk(SessionId),
    DeletePlanned { root: SessionId, plan: Result<ninox_core::events::DeletePlan, String> },
    DeleteToggleForce(bool),
    DeleteToggleExpanded,
    DeleteConfirm,
    DeleteDone(Vec<(SessionId, ninox_core::events::DeleteOutcome)>),
    DeleteClose,
    /// Session page: give the terminal the keyboard (Connect, `c`),
    /// switching to the Terminal tab if the current tab has none.
    TerminalFocus,
    /// The terminal canvas was clicked: focus it.
    TerminalClicked,
    /// `Ctrl+]`: hand the keyboard back to the app.
    TerminalRelease,
    SessionMoreToggle,
    SessionMoreHover(bool),
    SessionMenuPick(crate::components::session_detail::SessionAction),
    SessionConfirmAsk(crate::components::session_detail::Confirmable),
    SessionConfirmCancel,
    SessionConfirmGo,
    /// Esc on the session page: back to the Fleet tab with this session's
    /// island open and the session selected.
    SessionBack,
    /// Run `ninox connect <id>` in a terminal window of its own.
    PopOutSession(SessionId),
    SessionFactsLoaded(SessionId, Option<ninox_core::workspace::WorkspaceFacts>),
    /// Kill the tmux session and respawn the same name/workspace with the
    /// CURRENT registry settings (session header, beside Kill). On a
    /// Terminated husk this "just spawns".
    RefileSession(SessionId),
    /// Relaunch an `Interrupted` session by continuing its stored
    /// `claude_session_id` (tmux pane gone, but the Claude session id is
    /// still resumable — see `resume_plan`). Unlike `RefileSession`, this
    /// reuses the existing id instead of minting a fresh one.
    ResumeSession(SessionId),
    /// Fan out to one `Message::ResumeSession` per currently `Interrupted`
    /// session — the Fleet board's bulk "Resume all (N)" control.
    ResumeAllSessions,
    SwitchTheme(ThemeVariant),
    // Raw key event from the global subscription — bytes are computed in the handler
    // where we have access to the terminal mode (APP_CURSOR changes arrow sequences).
    RawKey {
        key:       iced::keyboard::Key,
        modifiers: iced::keyboard::Modifiers,
        text:      Option<String>,
    },
    /// Pre-encoded bytes for a session's pane — the terminal widget's
    /// Option/Cmd+click cursor move (`terminal::cursor_move_bytes`).
    TerminalWrite { session_id: SessionId, bytes: Vec<u8> },
    WindowResized(iced::Size),
    /// `titlebar::place_traffic_lights`'s result, in window points.
    TrafficLightsPlaced(Option<f32>),
    /// AppKit re-tiles the title bar when the window regains focus.
    WindowFocused,
    StartDrag(DragTarget),
    MouseMoved(iced::Point),
    MouseReleased,
    CopyToClipboard(String),
    /// A `text_editor` interaction inside a Marginalia comment card, keyed by
    /// `Comment::id`. Selection/scroll actions are applied; edit actions are
    /// dropped, which is what keeps the comment bodies read-only.
    CommentAction(i64, iced::widget::text_editor::Action),
    PollSessions,
    /// Mouse back button / ⌘[ / ⌘← — step back through navigation history.
    NavBack,
    NavForward,
    NavigateBrain,
    /// Opened from the v1 nav bar's gear icon.
    NavigateSettings,
    /// Flip a harness's enabled flag (inert for the locked-on claude-code).
    SettingsToggleHarness(String),
    /// Workers card — the `[worker]` default `ninox spawn` launches.
    SettingsWorkerHarness(String),
    /// External editor for the "Open in editor" action (`AppConfig::editor`).
    SettingsEditor(EditorChoice),
    SettingsWorkerModel(String),
    SettingsWorkerCustomModel(String),
    SettingsWorkerCustomCommit,
    /// Choose how orchestrator↔worker messages are delivered
    /// (`[messaging].mechanism` — see `ninox_core::config::SendMechanism`).
    /// Replaces the `SettingsToggleInboxMessaging` bool: the inbox is now
    /// one of three mutually exclusive mechanisms rather than on/off.
    SettingsSetSendMechanism(SendMechanism),
    SettingsSetRuntimeBackend(ninox_core::runtime::Backend),
    /// Flip the consolidated PR-watching toggle (`[pr_watch].enabled`,
    /// default on — see `ninox_core::config::PrWatchConfig`).
    SettingsTogglePrWatch,
    /// Session-limits field edited (location: `None` = local).
    SettingsLimitInput(Option<String>, String),
    /// Session-limits field submitted: parse and save.
    SettingsLimitCommit(Option<String>),
    SettingsLimitToggleUnlimited(Option<String>),
    SettingsSection(crate::components::settings_panel::SettingsSection),
    /// Validate and save one shared settings field (`settings_form::apply`).
    SettingsSet { id: ninox_ui_shared::settings_form::FieldId, text: String },
    SettingsDraft { id: ninox_ui_shared::settings_form::FieldId, text: String },
    SettingsCommit(ninox_ui_shared::settings_form::FieldId),
    SettingsRevert(ninox_ui_shared::settings_form::FieldId),
    BrainSelectEntry(String),
    BrainFilterQuery(String),
    BrainReindex,
    BrainSetMode(BrainMode),
    BrainShowInGraph(String),
    /// Kinds-column row clicked; `None` is "All entries".
    BrainSetKindFilter(Option<String>),
    /// Repo chip clicked; re-clicking the active repo clears it.
    BrainSetRepoFilter(Option<String>),
    /// Tag chip clicked; re-clicking the active tag clears it.
    BrainSetTagFilter(Option<String>),
    BrainSetSort(BrainSort),
    /// Scope segmented control (Neighbourhood/Clusters/Everything).
    BrainGraphSetScope(crate::components::brain_graph::GraphScope),
    /// Hops segmented control, 1-3.
    BrainGraphSetHops(u8),
    /// A node clicked: re-centres Neighbourhood on it (or, from any other
    /// scope, switches to Neighbourhood centred on it — "click focuses").
    BrainGraphFocusNode(usize),
    /// A node double-clicked: opens it in List ("double-click opens in
    /// List").
    BrainGraphOpenInList(usize),
    /// A cluster bubble clicked: scopes Everything down to just that
    /// repo's entries ("clicking a bubble opens it").
    BrainGraphOpenCluster(String),
    BrainGraphHover(Option<usize>),
    BrainGraphHoverCluster(Option<String>),
    BrainGraphToggleKind(String),
    /// Repo chip clicked (Everything scope); re-clicking the active repo
    /// clears it, same convention as the List view's repo chips.
    BrainGraphSetRepoFilter(Option<String>),
    BrainGraphToggleHideUnlinked,
    BrainGraphToggleFreezeLayout,
    BrainGraphZoom(f32),
    BrainGraphPan(f32, f32),
    BrainGraphFit,
    /// The Neighbourhood empty state's search text.
    BrainGraphSearch(String),
    /// Collapse or expand the "Whole brain" minimap (remembered in config).
    BrainGraphToggleMinimap,
    BrainGraphColorBy(crate::components::brain_graph::ColorBy),
    /// An off-thread layout finished (see `App::spawn_graph_layout`).
    BrainGraphLaidOut { key: u64, nodes: Vec<crate::components::brain_graph::GraphNode> },
    /// Esc inside the Graph view: Neighbourhood/Everything -> Clusters,
    /// already-Clusters is a no-op (brief: "Esc goes back to clusters").
    BrainGraphEsc,
    /// Recomputes `source_status` for every catalogue — triggered on
    /// entering the Sources view and after Save/Remove/Sync now, never on
    /// a timer (no background scheduler).
    SourcesRefreshStatus,
    SourcesOpenAdd,
    SourcesOpenEdit(String),
    SourcesFormName(String),
    SourcesFormSetType(ninox_ui_shared::brain_sources::SourceType),
    SourcesFormLocation(String),
    SourcesFormRegion(String),
    SourcesFormEndpoint(String),
    SourcesFormCachePath(String),
    SourcesFormCadence(String),
    SourcesFormAccess(String),
    SourcesFormCancel,
    SourcesFormSave,
    SourcesToggleEnabled(String),
    SourcesRemove(String),
    SourcesSyncNow(String),
    SourcesSyncAll,
    /// The async sync kicked off by `SourcesSyncNow` for `name`/`path`
    /// finished. Carries `name` too since the activity log reads better
    /// with the source's name than its filesystem path.
    SourcesSynced { name: String, path: std::path::PathBuf, result: Result<ninox_core::brain_sync::SyncReport, String> },
    BrainLinkClicked(iced::widget::markdown::Url),
    BrainEditStart(String),
    /// Start a new entry of this kind in the active catalogue.
    BrainNewStart(String),
    BrainEditName(String),
    /// A new entry's kind (`normalize_kind` key), from the badge-slot picker.
    BrainEditKind(String),
    /// ⌘S, even while a text field has focus: saves the open Brain editor.
    SaveShortcut,
    BrainEditTagDraft(String),
    BrainEditTagAdd,
    BrainEditTagRemove(String),
    BrainEditRepoDraft(String),
    BrainEditRepoAdd,
    BrainEditRepoRemove(String),
    BrainEditBody(iced::widget::text_editor::Action),
    BrainEditSave,
    BrainEditCancel,
    /// Select a ghost plan (`node_id: None`) or one of its nodes.
    GhostSelect { plan_id: String, node_id: Option<String> },
    GhostAcceptRequest(String),
    GhostDiscardRequest(String),
    GhostConfirmed,
    GhostConfirmCancel,
    GhostOpenInPlans(String),
    PlansSelectOrchestratorPlan(OrchestratorId),
    PlansToggleShowEnded,
    PlansOpenOrchestrator(OrchestratorId),
    GhostDecisionDone { plan_id: String, result: Result<String, String> },
    BrainDeleteRequest(String),
    BrainDeleteCancel,
    BrainDeleteConfirm,
    /// `select` is the entry to show once the brain reloads.
    /// `editor`: the serial of the draft that started the job, if any.
    BrainJobDone { job: ninox_ui_shared::brain_edit::Job, select: Option<String>, editor: Option<u64>, result: Result<String, String> },
    /// `Some(idx)` switches to `catalogues[idx]`; `None` switches to "All
    /// sources" (every enabled catalogue, merged).
    BrainSwitchCatalogue(Option<usize>),
    /// The background remote freshen kicked off by `BrainSwitchCatalogue`
    /// resolved: `open_synced` pulled (or confirmed fresh) every catalogue
    /// in scope and reopened its index/indexes. Dropped if the user has
    /// switched away from `scope_id` while the pull was in flight.
    BrainFreshened { scope_id: BrainScopeId, brain: BrainScope },
    /// The async remote-sync + rebuild kicked off by `BrainReindex`
    /// finished for `scope_id`. `Ok(n)` = entries indexed (summed across
    /// catalogues for "All sources"); `Err` = the local rebuild itself
    /// failed (remote failures degrade inside the task and still arrive
    /// here as `Ok`).
    BrainReindexed { scope_id: BrainScopeId, result: Result<usize, String> },
    /// Opens/closes the Fleet tab's needs-you-badge notifications dropdown.
    ToggleNotifications,
    /// Opens/closes the nav bar's pending-user-requests dropdown
    /// (`components::shell::nav`'s badge).
    TogglePendingRequests,
    DismissNotification(String),
    DismissAllNotifications,
    NavigateNotification(SessionId),
    /// Click-to-reveal a pending user-request's freeform answer input —
    /// toggles `sidebar.expanded_pending_request` and clears the draft.
    TogglePendingRequestExpanded(String),
    /// The expanded pending-request row's freeform input changed.
    PendingRequestDraftChanged(String),
    /// Submit an answer for a pending `ninox user-request` — fired by the
    /// Pending Requests panel's Approve/Deny buttons (sync rows) or its
    /// freeform input's submit. The panel's handler here is the only call
    /// site of `Store::answer_user_request` in this crate.
    AnswerUserRequest { id: String, answer: String },
    /// Dismiss a pending `ninox user-request` whose requester session is
    /// independently confirmed terminal — fired only by the Pending
    /// Requests panel's stale-row dismiss button. Cancels via
    /// `Store::mark_user_request_cancelled`, never answers.
    CancelUserRequest(String),
    /// "Update now" pressed on an `UpdateAvailable` notification slip —
    /// runs `ninox update` (the configured channel) as a subprocess.
    ApplyUpdate,
    /// The `ninox update` subprocess from `ApplyUpdate` finished.
    UpdateApplied(Result<(), String>),
    /// "Restart now" pressed on an `UpdateInstalled` notification slip —
    /// relaunches the binary and exits this process.
    RestartApp,
    /// The on-demand registry check `ensure_version_check` fires when
    /// Settings opens resolved: `Some(version)` = that version is newer
    /// than the running one, `None` = already current, `Err` = the check
    /// itself failed (no registry configured, network error, ...).
    VersionCheckResult(Result<Option<String>, String>),
    /// "Restart all agents" pressed on the Fleet card — opens the
    /// confirmation modal rather than firing immediately.
    RequestRestartAll,
    /// Re-read `/api/server/info` (periodically, and after a restart).
    ServerInfoTick,
    ServerInfoLoaded(Option<ninox_core::server_control::ServerInfo>),
    /// "Restart server" pressed — shows the inline confirm.
    RequestRestartServer,
    CancelRestartServer,
    ConfirmRestartServer,
    ServerRestartDone(Result<String, String>),
    /// From the top-bar dot or an alert banner: go to Settings with the
    /// restart confirm open.
    OpenServerRestart,
    /// Relaunch the sessions an alert says hit a harness login failure.
    RestartAffectedSessions(Vec<String>),
    AffectedSessionsRestarted(Result<String, String>),
    RestoreFleetOpen,
    /// The plan, and whether a restore already holds the lease.
    RestoreFleetPlanned(Result<ninox_core::fleet::RestorePlan, String>, Option<String>),
    RestoreFleetToggle(String),
    RestoreFleetRequest(crate::components::restore_modal::RestoreScope),
    RestoreFleetCancelConfirm,
    RestoreFleetConfirm,
    RestoreFleetStarted(Result<String, String>),
    RestoreFleetClose,
    ReapFinishedOpen,
    ReapFinishedConfirm,
    /// Ids of the workers a confirmed clean-up reaped, and the first
    /// orchestrator that failed, if any.
    ReapFinishedDone { reaped: Vec<String>, error: Option<String> },
    ReapFinishedClose,
    /// The restart-all confirmation modal's Cancel, or Esc.
    CancelRestartAll,
    /// The restart-all confirmation modal's "Restart all" — fires
    /// `restart::execute(..., all: true, ...)` against every live session.
    ConfirmRestartAll,
    /// The batch restart kicked off by `ConfirmRestartAll` finished.
    RestartAllApplied(Result<Vec<ninox_ui_shared::restart::RestartOutcome>, String>),
    FleetFilterQuery(String),
    ClearFleetFilter,
    /// Scroll a terminal by `delta` lines (positive = up into history).
    /// `local: false` (wheel) forwards to the PTY when the inner app has
    /// mouse mode enabled; `local: true` (drag-select auto-scroll) always
    /// moves ninox's own scrollback so selections can extend into history
    /// even under a mouse-mode TUI.
    ScrollTerminal { session_id: SessionId, delta: i32, local: bool },
    JumpToLatest { session_id: SessionId },
    /// A chunk of tmux pane history came back from `capture-pane` for a
    /// scrolled-back terminal.
    HistoryFetched {
        session_id: SessionId,
        bytes: Vec<u8>,
        fetched_to: i64,
        top_reached: bool,
    },
    OpenUrl(String),
    /// Open a worker's workspace directory in the configured editor
    /// (`AppConfig::editor`). Fire-and-forget, like `OpenUrl`.
    OpenInEditor(String),
    /// `models_cmd` discovery finished for a harness (`None` = failed —
    /// cached so pickers fall through to known_models without retrying).
    ModelListLoaded { harness: String, models: Option<Vec<String>> },
    /// An on-demand `git diff` fetch (`ensure_diff`) resolved for a
    /// session's workspace — `diff: None` means no workspace recorded, or a
    /// clean diff against the default branch, not an error.
    DiffFetched { session_id: SessionId, diff: Option<String> },
    Noop,
    ToggleHelp,
}

/// Whether `Message::PollSessions` should overwrite a tracked session's
/// status with the one the store now holds.
///
/// True whenever the store holds a *different terminal* status than app
/// memory. Those are the changes the app can't hear about any other way: an
/// out-of-process mutation (today, `ninox reap`) writes the store directly
/// and its `Event`s never reach this process.
///
/// Both directions matter, and the second is easy to miss:
/// - live → terminal: a worker was killed out from under us.
/// - terminal → terminal: force-reaping an `Interrupted` worker writes
///   `Terminated`, because its worktree is gone and it can no longer be
///   resumed. Ignoring that leaves the card sitting in the Working column
///   (`fleet_board::session_matches_column`), counted as interrupted, and —
///   worst of all — still picked up by `Message::ResumeAllSessions`, which
///   filters on the in-memory status and would silently resurrect a session
///   the orchestrator deliberately reaped.
///
/// A *live* status from the store is never adopted: terminal → live is the
/// app's own transition (Resume/Re-file), so a store snapshot read
/// mid-respawn must not be able to undo it.
fn adopts_terminal_status(known: &SessionStatus, from_store: &SessionStatus) -> bool {
    from_store.is_terminal() && from_store != known
}

/// Mirrors the store's sessions into `known`, touching only what differs:
/// untouched sessions are neither cloned nor written. Returns how many
/// sessions were inserted or changed.
fn sync_db_sessions(known: &mut HashMap<SessionId, Session>, db_sessions: Vec<Session>) -> usize {
    let mut changed = 0;
    for session in db_sessions {
        let replace = match known.get_mut(&session.id) {
            None => true,
            Some(k) if adopts_terminal_status(&k.status, &session.status) => true,
            // Entering or leaving the spawn queue happens in another process
            // (the poller's launcher) and emits no event here.
            Some(k) if k.status != session.status
                && (k.status == SessionStatus::Queued || session.status == SessionStatus::Queued) => true,
            // PR link/status written by a daemon's GitHub sweep: its Engine
            // events never reach this process.
            Some(k) => {
                if adopts_pr_progress(k, &session) {
                    changed += 1;
                }
                false
            }
        };
        if replace {
            known.insert(session.id.clone(), session);
            changed += 1;
        }
    }
    changed
}

/// Copies what a GitHub sweep in *another process* (the daemon) wrote to a
/// session's row into the app's copy: the PR link, and the PR-lifecycle
/// status. Only moves among the five PR-lifecycle statuses are adopted, so
/// this can never touch the Spawning/Queued/Interrupted/terminal
/// transitions the app drives itself.
fn adopts_pr_progress(known: &mut Session, stored: &Session) -> bool {
    use SessionStatus::*;
    let in_flow = |s: &SessionStatus| matches!(s, Working | PrOpen | CiFailed | ReviewPending | Mergeable);
    let mut changed = false;
    if stored.pr_number.is_some()
        && (known.pr_number != stored.pr_number || known.pr_id != stored.pr_id || known.repo != stored.repo)
    {
        known.pr_number = stored.pr_number;
        known.pr_id = stored.pr_id;
        known.repo = stored.repo.clone();
        changed = true;
    }
    if known.status != stored.status && in_flow(&known.status) && in_flow(&stored.status) {
        known.status = stored.status.clone();
        changed = true;
    }
    if known.gate_status != stored.gate_status || (stored.merged_at.is_some() && known.merged_at != stored.merged_at) {
        known.gate_status = stored.gate_status.clone();
        known.merged_at = stored.merged_at.or(known.merged_at);
        changed = true;
    }
    changed
}

/// An estimate of how many sessions "restart all" would target — the Fleet
/// card's count and the confirm modal's question both read this, so they
/// can't drift from each other. Based on the cached store status, not a
/// live tmux probe, so it can disagree with `restart::execute`'s own
/// `resolve_targets` (which checks `runtime::has_session` at fire time) if
/// a session's pane died since the last poll; `summarize`'s result after
/// the batch runs is the authoritative count.
pub(crate) fn live_session_count(sessions: &HashMap<SessionId, Session>) -> usize {
    sessions.values().filter(|s| !s.status.is_terminal()).count()
}

/// Apply a `text_editor` action to a Marginalia comment buffer while keeping
/// it read-only: selection, navigation and scrolling are applied, edits are
/// dropped. Copy is handled inside the widget itself (it writes the clipboard
/// directly and emits no action), so filtering edits here does not break
/// Cmd+C — which is the whole point of rendering comments as editors.
fn apply_comment_action(
    content: &mut iced::widget::text_editor::Content,
    action: iced::widget::text_editor::Action,
) {
    if !action.is_edit() {
        content.perform(action);
    }
}

#[cfg(test)]
mod comment_editor_tests {
    use iced::widget::text_editor::{Action, Content, Edit, Motion};

    use super::apply_comment_action;

    #[test]
    fn edits_never_mutate_a_comment_buffer() {
        let mut content = Content::with_text("hello");
        for edit in [
            Edit::Insert('x'),
            Edit::Backspace,
            Edit::Delete,
            Edit::Enter,
            Edit::Paste(std::sync::Arc::new("pasted".to_string())),
        ] {
            apply_comment_action(&mut content, Action::Edit(edit));
        }
        // `Content::text()` always terminates with a newline.
        assert_eq!(
            content.text(),
            "hello\n",
            "comment cards are read-only; no edit action may change the body"
        );
    }

    #[test]
    fn selection_actions_are_applied() {
        let mut content = Content::with_text("hello");
        assert!(content.selection().is_none(), "nothing selected initially");

        apply_comment_action(&mut content, Action::SelectAll);
        assert_eq!(
            content.selection().as_deref(),
            Some("hello"),
            "select-all must reach the buffer, otherwise text isn't selectable"
        );

        // Selecting must not have altered the text itself.
        assert_eq!(content.text(), "hello\n");
    }

    #[test]
    fn navigation_is_applied_and_clears_selection() {
        let mut content = Content::with_text("hello");
        apply_comment_action(&mut content, Action::SelectAll);
        assert!(content.selection().is_some());

        apply_comment_action(&mut content, Action::Move(Motion::DocumentEnd));
        assert!(
            content.selection().is_none(),
            "a plain move collapses the selection"
        );
        assert_eq!(content.text(), "hello\n");
    }
}

#[cfg(test)]
mod poll_adoption_tests {
    use super::*;

    /// The gap `ninox reap` exposed: a CLI process writes the store directly
    /// and its `Event`s never reach the running app, so without adopting the
    /// store's terminal status on poll a reaped worker keeps rendering as
    /// live on the fleet board for the whole retention window.
    #[test]
    fn adopts_a_terminal_status_the_store_has_and_app_memory_does_not() {
        assert!(adopts_terminal_status(&SessionStatus::Working, &SessionStatus::Terminated));
        assert!(adopts_terminal_status(&SessionStatus::PrOpen,  &SessionStatus::Done));
        assert!(adopts_terminal_status(&SessionStatus::CiFailed, &SessionStatus::Interrupted));
    }

    /// Terminal → live is the app's OWN transition (Resume/Re-file). A store
    /// snapshot read mid-respawn must never undo it, or the poll tick would
    /// fight the spawn it just started.
    #[test]
    fn never_reverts_a_session_the_app_just_respawned() {
        assert!(!adopts_terminal_status(&SessionStatus::Terminated, &SessionStatus::Working));
        assert!(!adopts_terminal_status(&SessionStatus::Interrupted, &SessionStatus::Working));
    }

    #[test]
    fn ignores_live_to_live_churn() {
        // CI flipping, a review landing — all of that already arrives as
        // events; re-adopting it here would add a second, racier path.
        assert!(!adopts_terminal_status(&SessionStatus::Working, &SessionStatus::PrOpen));
    }

    #[test]
    fn ignores_a_status_that_has_not_changed() {
        for s in [SessionStatus::Done, SessionStatus::Terminated, SessionStatus::Interrupted] {
            assert!(!adopts_terminal_status(&s, &s), "{s:?} → {s:?} is not a change");
        }
    }

    /// Force-reaping an `Interrupted` worker writes `Terminated` out of
    /// process. Missing that transition leaves the card in the Working
    /// column, counted as interrupted, and still selected by
    /// `Message::ResumeAllSessions` — which filters on the in-memory status
    /// and would resurrect a session the orchestrator deliberately reaped.
    #[test]
    fn adopts_a_force_reaped_interrupted_worker_becoming_terminated() {
        assert!(adopts_terminal_status(&SessionStatus::Interrupted, &SessionStatus::Terminated));
    }
}

// ---------------------------------------------------------------------------
// Keyboard → terminal byte conversion (static fn — no captures allowed by listen_with)
// ---------------------------------------------------------------------------

/// Messages the engine or a timer sends, never the user. Anything they do to
/// the view must not enter the navigation history.
fn is_automatic(message: &Message) -> bool {
    matches!(message, Message::EngineEvent(_) | Message::PollSessions)
}

/// ⌘[ / ⌘] always navigate (the logo key, not `command()`: that is Ctrl off
/// macOS, which would shadow the terminal's Ctrl+] release chord); ⌘← / ⌘→ only while nothing (text input or
/// terminal) wants the arrow keys.
fn history_chord(
    key: &iced::keyboard::Key,
    modifiers: iced::keyboard::Modifiers,
    arrows_free: bool,
) -> Option<Message> {
    use iced::keyboard::{key::Named, Key};
    if !modifiers.logo() || modifiers.control() || modifiers.shift() || modifiers.alt() {
        return None;
    }
    match key {
        Key::Character(c) if c.as_str() == "[" => Some(Message::NavBack),
        Key::Character(c) if c.as_str() == "]" => Some(Message::NavForward),
        Key::Named(Named::ArrowLeft) if arrows_free => Some(Message::NavBack),
        Key::Named(Named::ArrowRight) if arrows_free => Some(Message::NavForward),
        _ => None,
    }
}

fn global_event_handler(
    event: iced::Event,
    status: iced::event::Status,
    _id: iced::window::Id,
) -> Option<Message> {
    // Window resize — always handle regardless of captured status.
    if let iced::Event::Window(iced::window::Event::Resized(size)) = &event {
        return Some(Message::WindowResized(*size));
    }
    if let iced::Event::Mouse(iced::mouse::Event::ButtonPressed(button)) = &event {
        match button {
            iced::mouse::Button::Back => return Some(Message::NavBack),
            iced::mouse::Button::Forward => return Some(Message::NavForward),
            _ => {}
        }
    }
    // Mouse tracking for drag-resize handles.
    if let iced::Event::Mouse(iced::mouse::Event::CursorMoved { position }) = event {
        return Some(Message::MouseMoved(position));
    }
    if let iced::Event::Mouse(iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)) = event {
        return Some(Message::MouseReleased);
    }
    if let iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)) = event {
        return Some(Message::MousePressed);
    }
    if let iced::Event::Window(iced::window::Event::Unfocused) = event {
        return Some(Message::WindowUnfocused);
    }
    if let iced::Event::Window(iced::window::Event::Focused) = event {
        return Some(Message::WindowFocused);
    }
    // Keyboard — only handle Ignored events (not already captured by a
    // widget), except Esc: a focused `text_input` captures it to unfocus
    // itself, but it must still close search / collapse the drawer.
    if status == iced::event::Status::Captured {
        if let iced::Event::Keyboard(iced::keyboard::Event::KeyPressed { key, modifiers, .. }) = &event {
            if let Some(nav @ (Message::NavBack | Message::NavForward)) = history_chord(key, *modifiers, false) {
                return Some(nav);
            }
        }
        if let iced::Event::Keyboard(iced::keyboard::Event::KeyPressed { key: iced::keyboard::Key::Character(c), modifiers, .. }) = &event {
            if modifiers.command() && c.as_str().eq_ignore_ascii_case("s") {
                return Some(Message::SaveShortcut);
            }
        }
        return matches!(
            event,
            iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
                key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape), ..
            })
        ).then_some(Message::EscapeCaptured);
    }
    let iced::Event::Keyboard(
        iced::keyboard::Event::KeyPressed { key, modifiers, text, .. }
    ) = event else {
        return None;
    };
    Some(Message::RawKey {
        key,
        modifiers,
        text: text.map(|t| t.as_str().to_string()),
    })
}

// ---------------------------------------------------------------------------
// Impl
// ---------------------------------------------------------------------------

impl App {
    pub fn new(
        engine: Arc<Engine>,
        orchestrator_root: std::path::PathBuf,
        brain: Arc<BrainIndex>,
        db_path: std::path::PathBuf,
    ) -> (Self, Task<Message>) {
        // Synchronously load persisted state from the DB so the UI isn't empty
        // on startup.
        let orchestrators = engine.store.list_orchestrators().unwrap_or_default();
        let queue = engine.store.list_queue().unwrap_or_default();
        let mut sessions = engine.store.list_sessions().unwrap_or_default();
        ninox_core::session_limits::overlay_queue(&mut sessions, &queue);
        let sessions: HashMap<SessionId, Session> = sessions
            .into_iter()
            .map(|s| (s.id.clone(), s))
            .collect();
        let session_prs = ninox_core::session_prs::for_sessions(&engine.store, sessions.values());

        // Comments already fetched by a previous run — grouped by PR so a
        // restart doesn't blank out the Marginalia feed until the next poll.
        // `list_comments` already orders by `created_at`, so each group's
        // insertion order is already the display order.
        //
        // `comment_editors` is intentionally NOT populated here: shaping a
        // `text_editor::Content` per comment is CPU-bound text layout, and
        // doing it for every historical comment across every PR blocks
        // `App::new` — which iced calls before the window is created — for
        // as long as it takes to shape all of them. With enough review
        // history that's long enough that no window ever appears
        // (MLOPS-5188). Editors are hydrated lazily per PR instead, by
        // `ensure_comment_editors`, only when a session with that PR
        // actually needs them shown.
        let mut review_threads: HashMap<PrId, Vec<Comment>> = HashMap::new();
        for comment in engine.store.list_comments().unwrap_or_default() {
            review_threads.entry(comment.pr_id).or_default().push(comment);
        }
        let comment_editors: HashMap<i64, iced::widget::text_editor::Content> = HashMap::new();

        // `mut`: the zoom clamp below writes the sanitized value back so
        // disk and runtime stay in sync.
        let (mut config, config_warning) = match AppConfig::load_detailed() {
            Ok(cfg) => (cfg, None),
            Err(e) => (AppConfig::default(), Some(e.banner_message())),
        };

        // First run: seed a complete, editable default theme file so users
        // have a working example to customize rather than a blank slate.
        if let Some(themes_dir) = AppConfig::config_path().parent().map(|p| p.join("themes")) {
            if let Err(e) = crate::theme::ensure_default_theme_file(&themes_dir) {
                tracing::warn!("failed to write default theme file: {e}");
            }
        }
        let themes = Themes::load(config.theme_file.as_deref());
        let scheme = themes.scheme(config.theme);
        let active_variant = config.theme;
        // Restore the persisted zoom, guarding against a hand-edited config
        // with an out-of-range value. Write the clamped value back into
        // `config` so a later `config.save()` (e.g. a theme change) persists
        // the normalized zoom rather than the stale out-of-range one — keeping
        // disk and runtime in sync.
        let zoom = config.zoom.clamp(ZOOM_MIN, ZOOM_MAX);
        config.zoom = zoom;
        let catalogues = config.catalogue_options();
        // "All sources" is the default whenever there's more than one
        // enabled catalogue to merge — `BrainIndex::open` is a cheap
        // connection-open-plus-schema-pragma (no file walking; `rebuild()`
        // is what's expensive), so opening a handful of them here doesn't
        // touch the startup-time budget MLOPS-5338 protects. With zero or
        // one enabled catalogue this is exactly the caller's single `brain`,
        // unchanged.
        let (initial_brain, active_catalogue) =
            if catalogues.iter().filter(|c| c.enabled).count() > 1 {
                match MergedBrainIndex::open(&catalogues) {
                    Ok(merged) => (BrainScope::All(Arc::new(merged)), None),
                    Err(e) => {
                        tracing::warn!("merged brain at startup: {e}");
                        (BrainScope::Single(brain), Some(0))
                    }
                }
            } else {
                (BrainScope::Single(brain), Some(0))
            };
        // Restore persisted sidebar geometry; clamp width to the same range
        // the live drag enforces so a hand-edited config can't wedge layout.
        // Write the clamped value straight back into `config` so a later
        // `config.save()` persists the same width the UI renders, rather than
        // the raw out-of-range value (disk and runtime stay in sync).
        config.sidebar_width = config.sidebar_width.clamp(150.0, 400.0);
        let sidebar_width_init = config.sidebar_width;

        let mut app = Self {
            engine:             engine.clone(),
            config,
            config_warning,
            themes,
            scheme,
            active_variant,
            orchestrator_root,
            db_path,
            orchestrators,
            sessions,
            queue_positions: HashMap::new(),
            limits_headline: String::new(),
            brain:          initial_brain,
            brain_view:     BrainViewState::default(),
            catalogues,
            active_catalogue,
            prs:            HashMap::new(),
            ci_status:      HashMap::new(),
            session_prs,
            github_budget:  None,
            store_poll_tick: 0,
            review_threads,
            comment_editors,
            diffs:          HashMap::new(),
            plan_docs:      HashMap::new(),
            notifications:  VecDeque::new(),
            notif_cursor:   0,
            #[cfg(test)]
            alerted:        Vec::new(),
            pending_user_requests: engine.store.open_user_requests(None).unwrap_or_default(),
            orch_plans: Default::default(),
            ghost: crate::components::ghost_plans::GhostUi {
                plans: ninox_ui_shared::ghost_view::load(&engine.store, now_ms_i64()),
                ..Default::default()
            },
            human_console_error: None,
            user_request_error: None,
            unread_messages: HashMap::new(),
            held_messages: HashMap::new(),
            queued_by_sender: HashMap::new(),
            update_in_progress: false,
            version_check:  VersionCheckState::NotChecked,
            restart_all_in_progress: false,
            server: ServerPanel::default(),
            sidebar:        SidebarState::default(),
            view:           View::default(),
            worker_panel:   Default::default(),
            terminals:      HashMap::new(),
            transcripts:    HashMap::new(),
            clients:        HashMap::new(),
            attach: HashMap::new(),
            next_attach_token: 0,
            reap_pending: HashMap::new(),
            next_client_generation: 0,
            model_lists:    HashMap::new(),
            settings:       Default::default(),
            spawn_modal:    None,
            spawn_prompt:   iced::widget::text_editor::Content::new(),
            help_open:      false,
            catalogue_modal: None,
            sources_edit: None,
            source_status: HashMap::new(),
            source_sync_log: VecDeque::new(),
            restart_all_confirm: false,
            restore_modal: None,
            reap_modal: None,
            delete_modal: None,
            // Placeholders — corrected below by resize_terminals() using the
            // real default window size, so this never drifts out of sync with
            // main.rs's iced::window::Settings::default() (1024x768).
            terminal_cols:  140,
            terminal_rows:  50,
            // The default 1024×768 window in iced's zoomed logical px, until
            // the first resize reports the real size.
            window_width:   1024.0 / zoom as f32,
            traffic_lights_end: None,
            window_height:  768.0 / zoom as f32,
            // Restore the persisted sidebar width, clamped to the live drag
            // range so a stale/edited config can't wedge the layout.
            sidebar_width:  sidebar_width_init,
            info_width:     crate::components::session_detail::SIDE_PANEL_WIDTH,
            terminal_focused: false,
            terminal_click_marker: false,
            session_more_open: false,
            poll_tick: 0,
            session_more_hover: false,
            session_confirm: None,
            workspace_facts: HashMap::new(),
            session_activity: HashMap::new(),
            drag:            None,
            fleet_filter:    FleetFilter::default(),
            last_fleet_scope: None,
            global_search:   GlobalSearchState::default(),
            zoom,
            ninox_prompt_draft: String::new(),
            fleet_sort:            ninox_overview::islands::IslandSort::NeedsYouFirst,
            fleet_machine_filter:  FleetMachineFilter::default(),
            fleet_compact:         true,
            fleet_expanded_folds:  std::collections::HashSet::new(),
            fleet_nav:             ninox_overview::nav::FleetNav::default(),
            nav:                   Default::default(),
            nav_auto:              false,
            fleet_last_island_click: None,
            fleet_last_node_click: None,
            fleet_message_draft: None,
            worker_types: crate::components::worker_types::WorkerTypesState::default(),
            remotes: crate::components::remotes::RemotesState::default(),
            idle: crate::components::idle_view::IdleState::default(),
            ninox_prompt_target: crate::prompt_agent::PromptTarget::default(),
            ninox_reply_drawer: crate::prompt_agent::ReplyDrawer::default(),
            ninox_turn_select: HashMap::new(),
            thinking_tick: 0,
            reduce_motion: crate::motion::reduce_motion(),
            ninox_prompt_generation: 0,
            ninox_prompt_history: Vec::new(),
            ninox_prompt_history_cursor: None,
            ninox_prompt_draft_before_recall: None,
            workspaces_summary: (0, 0),
            workspaces_last_refresh: None,
            workspaces_modal: None,
        };

        Self::resize_terminals(&mut app);

        // After a reboot or crash under the `prompt` restore policy the
        // engine leaves a restore offer open; surface it like the TUI's
        // startup modal does.
        app.notif_cursor = app.engine.store.max_notification_seq().unwrap_or(0);
        Self::sync_notifications(&mut app, false);
        if let Some(summary) = ninox_core::fleet::startup::pending_restore(&app.engine.store) {
            app.notifications.push_front(restore_offer(&summary));
        }

        // Dead-session reconciliation now runs in `Poller::start`
        // (`reconcile_dead_sessions`) so every host of the poller — GUI and
        // the headless daemon alike — gets it, not just this startup task.
        let task = Self::place_traffic_lights(&app);
        (app, task)
    }

    /// Iced-compatible mutable update — passed to `iced::application()`.
    pub fn iced_update(state: &mut Self, message: Message) -> Task<Message> {
        let task = Self::apply_settled(state, message);
        match state.brain_view.graph.layout_job.take() {
            Some(job) => Task::batch([task, Self::spawn_graph_layout(job)]),
            None => task,
        }
    }

    /// `apply`, then drops a ghost confirm left armed on a view the user has
    /// left: it is only answerable where it was shown, and an unseen one
    /// would keep claiming Esc.
    fn apply_settled(state: &mut Self, message: Message) -> Task<Message> {
        let before = std::mem::discriminant(&state.view);
        let from = Self::location(state);
        let records = !is_automatic(&message) && !matches!(message, Message::NavBack | Message::NavForward);
        state.nav_auto = false;
        let task = Self::apply(state, message);
        if std::mem::discriminant(&state.view) != before {
            state.ghost.confirm = None;
        }
        if records && !state.nav_auto && !from.same_place(&Self::location(state)) {
            state.nav.push(from);
        }
        state.nav_auto = false;
        task
    }

    fn location(state: &Self) -> crate::nav_history::Location {
        crate::nav_history::Location {
            view: state.view.clone(),
            open: state.fleet_nav.open.clone(),
            selected: state.fleet_nav.selected.clone(),
            selected_island: state.fleet_nav.selected_island.clone(),
        }
    }

    fn navigate_history(state: &mut Self, backwards: bool) -> Task<Message> {
        if state.spawn_modal.is_some() || state.catalogue_modal.is_some() || state.restart_all_confirm
            || state.workspaces_modal.is_some() || state.delete_modal.is_some()
        {
            return Task::none();
        }
        let current = Self::location(state);
        let mut nav = std::mem::take(&mut state.nav);
        let exists = |id: &str| state.sessions.contains_key(id) || state.orchestrators.iter().any(|o| o.id == id);
        let is_stale = |loc: &crate::nav_history::Location| match &loc.view {
            View::SessionDetail { session_id, .. } => !exists(session_id),
            View::Board { scope: Some(s) } => !exists(s),
            _ => {
                let gone = |k: &Option<ninox_overview::islands::IslandKey>| matches!(k, Some(ninox_overview::islands::IslandKey::Root(id)) if !exists(id));
                gone(&loc.open)
            }
        };
        let stepped = if backwards { nav.back(current, is_stale) } else { nav.forward(current, is_stale) };
        state.nav = nav;
        let Some((target, skipped)) = stepped else { return Task::none() };
        if skipped > 0 {
            let now = chrono::Utc::now().timestamp_millis();
            Self::push_notification(state, Notification {
                id: format!("nav-skipped-{now}"),
                kind: NotificationKind::WorkerRetired,
                title: "Skipped a session that no longer exists".into(),
                body: format!("{skipped} history entr{} pointed at a deleted session.", if skipped == 1 { "y" } else { "ies" }),
                session_id: None,
                created_at: now,
            });
        }
        Self::restore_location(state, target)
    }

    fn restore_location(state: &mut Self, target: crate::nav_history::Location) -> Task<Message> {
        state.global_search.dismiss();
        state.ghost.selected = None;
        state.ghost.confirm = None;
        state.fleet_message_draft = None;
        state.terminal_focused = false;
        state.session_more_open = false;
        state.session_confirm = None;
        // Leaving a session view releases its attach, as the tab handlers do.
        state.clients.clear();
        state.attach.clear();
        let task = match target.view {
            View::SessionDetail { session_id, panel } => {
                let task = Self::apply(state, Message::NavigateSession(session_id));
                let switch = Self::apply(state, Message::SwitchDetailPanel(panel));
                Task::batch([task, switch])
            }
            View::Board { scope } => Self::apply(state, Message::NavigateBoard { scope }),
            View::Plans => Self::apply(state, Message::NavigatePlans),
            View::Brain => Self::apply(state, Message::NavigateBrain),
            View::WorkerTypes => Self::apply(state, Message::NavigateWorkerTypes),
            View::Remotes => Self::apply(state, Message::NavigateRemotes),
            View::Idle => Self::apply(state, Message::NavigateIdle),
            View::Settings => Self::apply(state, Message::NavigateSettings),
            View::Fleet => {
                state.view = View::Fleet;
                Task::none()
            }
        };
        let exists = |id: &str| state.sessions.contains_key(id) || state.orchestrators.iter().any(|o| o.id == id);
        let island_gone = |k: &Option<ninox_overview::islands::IslandKey>| matches!(k, Some(ninox_overview::islands::IslandKey::Root(id)) if !exists(id));
        let selected = target.selected.filter(|id| exists(id));
        let selected_island = target.selected_island.filter(|k| !island_gone(&Some(k.clone())));
        state.fleet_nav.open = target.open;
        state.fleet_nav.selected = selected;
        state.fleet_nav.selected_island = selected_island;
        task
    }

    /// Lays out a big graph on a blocking thread; the UI keeps drawing.
    fn spawn_graph_layout(job: crate::components::brain_graph::LayoutJob) -> Task<Message> {
        let key = job.key;
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || crate::components::brain_graph::layout_seeded(&job.node_ids, &job.edges, &job.seeds))
                    .await
                    .unwrap_or_default()
            },
            move |nodes| Message::BrainGraphLaidOut { key, nodes },
        )
    }

    /// Recompute terminal cols/rows from the current window, sidebar, and
    /// (when the Split panel is active) info-panel width, then push the new
    /// size into every live `TerminalState`'s grid so rendering reflows to
    /// fit the actual visible canvas area instead of being clipped.
    ///
    /// `DetailPanel::default()` is Split, so any session — active or
    /// backgrounded — shows the Split panel unless it's the one currently
    /// being viewed *and* the user has switched it to a different panel.
    /// Background sessions are therefore always sized as if Split were
    /// showing (the width they'll actually get next time they're
    /// navigated to); only the active session is sized for whatever panel
    /// it's actually displaying right now. Without this distinction, the
    /// active session's own panel choice (e.g. switching to the full-width
    /// Terminal tab) would leak into every other open session's real tmux
    /// pane, resizing sessions the user isn't even looking at.
    ///
    /// Returns `(session_id, cols, rows)` for every session whose backing
    /// tmux pane needs resizing to match — callers decide whether/when to
    /// do that (e.g. immediately for a window resize, or once at
    /// drag-release rather than every frame).
    /// Kick off `models_cmd` discovery for a harness (cached per app run —
    /// including failures, which fall through to known_models). Marks the
    /// harness in-flight immediately (as a `None` entry — pickers fall
    /// through to known_models until the result lands) so a second trigger
    /// before completion doesn't spawn a duplicate subprocess.
    /// Persist a config change read → apply → write against the file, so an
    /// edit made meanwhile (e.g. in `nx`'s settings) is never clobbered by
    /// this process's stale copy; `state.config` becomes the saved result.
    /// If the file can't be read back, the change only applies in memory.
    /// The Spawn dialog's admission under the session limit. `Ok(true)`:
    /// start now (a slot is reserved; release it once the launch finishes).
    /// `Ok(false)`: queued — the dialog closes and the session shows as
    /// queued. `Err`: a message for the dialog.
    fn admit_spawn(
        state: &mut App,
        skeleton: Session,
        payload: ninox_ui_shared::session_queue::QueuedLaunch,
    ) -> Result<bool, String> {
        use ninox_ui_shared::session_queue::{admit, Admit};
        match admit(&state.engine.store, ninox_core::store::QueueKind::Spawn, &skeleton, &payload, false) {
            Ok(Admit::Admitted) => Ok(true),
            Ok(Admit::Queued { ahead, .. }) => {
                state.spawn_modal = None;
                let mut queued = skeleton;
                queued.status = SessionStatus::Queued;
                state.queue_positions.insert(queued.id.clone(), ahead + 1);
                state.sessions.insert(queued.id.clone(), queued);
                Ok(false)
            }
            Err(e) => Err(e.to_string()),
        }
    }

    /// `worker_type_cli` writes config.toml itself; pick its write up so a
    /// Clear context or a first ask launches the Ninox type as just edited.
    fn reload_config_after_worker_type_change(state: &mut App) {
        match AppConfig::load() {
            Ok(cfg) => state.config = cfg,
            Err(e) => tracing::warn!("failed to reload config after a worker type change: {e}"),
        }
    }

    fn persist_config<R>(state: &mut App, what: &str, f: impl Fn(&mut AppConfig) -> R) -> R {
        match AppConfig::update(&f) {
            Ok((saved, r)) => {
                state.config = saved;
                r
            }
            Err(e) => {
                tracing::warn!("failed to save config ({what}): {e}");
                f(&mut state.config)
            }
        }
    }

    /// Spawn `argv` as the embedded `machine add` subprocess, wired to a
    /// fresh `TerminalState`/`AttachedClient` keyed by
    /// `remotes::MACHINE_ADD_SESSION_ID` — the same insertion shape
    /// `Message::ClientAttach`'s success path uses for a real session,
    /// minus the tmux-attach argv wrapping and panel reflow those don't
    /// apply here (this PTY has no backing ninox session or detail panel).
    /// Called directly by `Message::RemoteAddSubmit` and by tests with a
    /// trivial argv.
    fn spawn_machine_add(state: &mut App, argv: Vec<String>) -> Result<(), String> {
        let (cols, rows) = crate::components::remotes::machine_add_cols_rows();
        let generation = state.next_client_generation;
        state.next_client_generation += 1;
        let client = ninox_core::client::AttachedClient::spawn(
            state.engine.clone(),
            crate::components::remotes::MACHINE_ADD_SESSION_ID.to_string(),
            argv,
            cols,
            rows,
            generation,
        )
        .map_err(|e| e.to_string())?;
        state.terminals.insert(
            crate::components::remotes::MACHINE_ADD_SESSION_ID.to_string(),
            crate::components::terminal::TerminalState::new(cols, rows, Some(client.input_sender())),
        );
        state.clients.insert(crate::components::remotes::MACHINE_ADD_SESSION_ID.to_string(), client);
        if let Some(form) = &mut state.remotes.add {
            form.running = true;
        }
        Ok(())
    }

    /// Resolve a pending "Add remote" flow once its embedded `machine add`
    /// subprocess is gone (either it exited, or the client was dropped by
    /// navigating away mid-add) — closes the modal and sets a success/
    /// failure notice. Success is inferred from whether `AppConfig` now has
    /// a profile for the submitted host, since neither `AttachedClient` nor
    /// `Event::ClientClosed` carries the subprocess's exit code today.
    fn resolve_machine_add(state: &mut App) {
        let Some(form) = &mut state.remotes.add else { return };
        if form.finished.is_some() {
            return; // already resolved — don't re-derive or double-refresh
        }
        let added = AppConfig::load()
            .map(|cfg| cfg.remote_machines.machines.iter().any(|m| m.ssh_target == form.host))
            .unwrap_or(false);
        let result = if added {
            Ok(format!("added {}", form.host))
        } else {
            Err(format!(
                "machine add for {} did not complete — run `ninox machine add {}` directly for details",
                form.host, form.host
            ))
        };
        form.finished = Some(result.clone());
        state.remotes.notice = Some(result);
        Self::refresh_remotes(state);
    }

    /// Reload the Remotes tab's `RemotesState::rows` from disk — the same
    /// join (`ninox_ui_shared::machine::collect_rows`) the TUI's own
    /// `refresh` uses, run fresh so an add/remove made elsewhere (e.g. `nx`,
    /// or `ninox machine add`'s own subprocess) is reflected immediately.
    /// Rebuilds the Idle tab's report from the store: session rows plus the
    /// cached disk numbers and prompt log — no GitHub call, no disk walk.
    fn refresh_idle(state: &mut App) {
        let now = now_ms_i64();
        match ninox_core::idle_workers::load_report(&state.engine.store, &state.config.idle_workers, now) {
            Ok(report) => {
                state.idle.report = report;
                state.idle.as_of = now;
            }
            Err(e) => tracing::warn!("idle report: {e}"),
        }
    }

    fn refresh_remotes(state: &mut App) {
        let cfg = AppConfig::load().unwrap_or_default();
        let health = state.engine.store.machine_health().unwrap_or_default();
        state.remotes.rows = ninox_ui_shared::machine::collect_rows(&cfg, &health);
        state.remotes.monitoring_enabled = cfg.remote_machines.enabled;
    }

    fn ensure_models(state: &mut App, harness: &str) -> Task<Message> {
        if state.model_lists.contains_key(harness) {
            return Task::none();
        }
        let Some(cmd) = state.config.registry().spec(harness).models_cmd else {
            return Task::none();
        };
        state.model_lists.insert(harness.to_string(), None);
        let h = harness.to_string();
        Task::future(async move {
            let models = ninox_ui_shared::models::run_models_cmd(cmd).await;
            Message::ModelListLoaded { harness: h, models }
        })
    }

    /// Kick off an on-demand `git diff` fetch for `session_id`'s workspace.
    /// Unlike `ensure_models`, this always re-runs rather than caching on
    /// first success: a live, in-progress session's diff changes as the
    /// worker commits, so callers re-issue this every time the Diff panel
    /// is opened or the session poll ticks while it's on screen.
    fn ensure_diff(state: &mut App, session_id: &str) -> Task<Message> {
        let Some(workspace) = state.sessions.get(session_id).and_then(|s| s.workspace_path.clone()) else {
            state.diffs.insert(session_id.to_string(), None);
            return Task::none();
        };
        let sid = session_id.to_string();
        Task::future(async move {
            let diff = ninox_core::lifecycle::brain_harvest::compute_diff(
                std::path::Path::new(&workspace),
            )
            .await;
            Message::DiffFetched { session_id: sid, diff }
        })
    }

    /// Clean/dirty and commits-ahead for the Split panel's Workspace field.
    /// Shells out to git, so it runs off the UI thread.
    fn ensure_workspace_facts(state: &App, session_id: &str) -> Task<Message> {
        let Some(session) = state.sessions.get(session_id) else { return Task::none() };
        let Some(workspace) = session.workspace_path.clone() else { return Task::none() };
        let branch = crate::components::session_detail::branch_from_workspace(session).map(str::to_string);
        let sid = session_id.to_string();
        Task::future(async move {
            let facts = tokio::task::spawn_blocking(move || {
                let path = std::path::Path::new(&workspace);
                if !path.is_dir() {
                    return None;
                }
                let base = ninox_core::workspace::resolve_default_base_ref(path);
                Some(ninox_core::workspace::gather_git_facts(path, path, branch.as_deref(), base.as_deref()))
            })
            .await
            .ok()
            .flatten();
            Message::SessionFactsLoaded(sid, facts)
        })
    }

    /// Typed messages to or from `session_id`, for the Split panel's
    /// Activity field. The table is capped at 200 rows, so this is a cheap
    /// read.
    fn load_session_activity(state: &mut App, session_id: &str) {
        let events = state.engine.store.recent_message_events(0).unwrap_or_default();
        let mine = events
            .into_iter()
            .filter(|e| e.to_id == session_id || e.from_id.as_deref() == Some(session_id))
            .collect();
        state.session_activity.insert(session_id.to_string(), mine);
    }

    /// Resize every terminal to the current layout and sync the backing panes.
    fn reflow(state: &mut App) {
        for (sid, cols, rows) in Self::resize_terminals(state) {
            if let Some(client) = state.clients.get(&sid) {
                client.resize(cols, rows);
            }
        }
    }

    /// Drops a session `Engine::execute_delete` removed from every
    /// in-memory view of it, leaving a page that showed it.
    fn forget_deleted_session(state: &mut App, id: &str) {
        if matches!(&state.view, View::SessionDetail { session_id, .. } if session_id == id)
            || matches!(&state.view, View::Board { scope: Some(s) } if s == id)
        {
            state.view = View::Fleet;
            state.nav_auto = true;
        }
        if state.fleet_nav.selected.as_deref() == Some(id) {
            state.fleet_nav.clear_selection();
        }
        let island = ninox_overview::islands::IslandKey::Root(id.to_string());
        if state.fleet_nav.open.as_ref() == Some(&island) {
            state.fleet_nav.close_island();
            state.nav_auto = true;
        }
        if state.fleet_nav.selected_island.as_ref() == Some(&island) {
            state.fleet_nav.selected_island = None;
        }
        if state.fleet_message_draft.as_ref().is_some_and(|(target, _)| target == id) {
            state.fleet_message_draft = None;
        }
        state.orchestrators.retain(|o| o.id != id);
        state.sidebar.expanded_orchestrators.remove(id);
        state.sessions.remove(id);
        state.transcripts.remove(id);
        state.terminals.remove(id);
        state.clients.remove(id);
        state.diffs.remove(id);
        state.plan_docs.remove(id);
    }

    /// Start attach attempt `attempt` for the viewed session. Idempotent:
    /// whatever an earlier attempt left — its attach child, its pending
    /// prepare, its retry timer — is torn down first, so one view never has
    /// two attach children or two attempts racing.
    fn begin_attach(state: &mut App, id: &SessionId, attempt: u32) -> Task<Message> {
        state.clients.remove(id);
        state.terminals.remove(id);
        let token = state.next_attach_token;
        state.next_attach_token += 1;
        let engine = state.engine.clone();
        let session_id = id.clone();
        let budget = crate::attach::budget(Self::is_remote(state, id));
        let (task, handle) = Task::future(async move {
            let outcome = prepare_attach(engine, &session_id, budget).await;
            Message::AttachPrepared { session_id, token, outcome }
        })
        .abortable();
        tracing::info!(session = %id, attempt, "terminal attach starting");
        state.attach.insert(id.clone(), crate::attach::AttachProgress::new(token, attempt, Some(handle)));
        task
    }

    fn is_remote(state: &App, id: &SessionId) -> bool {
        state.sessions.get(id).is_some_and(|s| s.machine_id.is_some())
    }

    /// Schedule the next attempt after `attempt` failures, or give up and
    /// leave `reason` for the view to show with a Reconnect button.
    fn retry_or_fail(state: &mut App, id: &SessionId, attempt: u32, reason: String) -> Task<Message> {
        let token = state.next_attach_token;
        state.next_attach_token += 1;
        if attempt > crate::attach::MAX_ATTEMPTS {
            tracing::warn!(session = %id, attempt, %reason, "terminal attach gave up");
            let mut p = crate::attach::AttachProgress::new(token, attempt, None);
            p.failure = Some(reason);
            state.attach.insert(id.clone(), p);
            return Task::none();
        }
        let delay = crate::attach::backoff(attempt);
        tracing::warn!(session = %id, attempt, ?delay, %reason, "terminal attach failed; retrying");
        let session_id = id.clone();
        let (task, handle) = Task::future(async move {
            tokio::time::sleep(delay).await;
            Message::AttachRetry { session_id, token }
        })
        .abortable();
        state.attach.insert(id.clone(), crate::attach::AttachProgress::new(token, attempt, Some(handle)));
        task
    }

    /// Every way onto a session page starts from the same clean state: the
    /// terminal unfocused, no menu or confirm armed from another session,
    /// and a panel this session actually offers.
    fn enter_session_page(state: &mut App, session_id: &str, panel: DetailPanel) {
        let is_orchestrator = state.orchestrators.iter().any(|o| o.id == session_id);
        state.view = View::SessionDetail {
            session_id: session_id.to_string(),
            panel: crate::components::session_detail::effective_panel(panel, is_orchestrator),
        };
        state.terminal_focused = false;
        state.terminal_click_marker = false;
        state.session_more_open = false;
        state.session_more_hover = false;
        state.session_confirm = None;
    }

    /// The open session shows an ACP transcript: there's no pty to type into.
    fn shows_transcript(state: &App) -> bool {
        matches!(&state.view, View::SessionDetail { session_id, .. }
            if state.transcripts.get(session_id).is_some_and(|e| !e.is_empty()))
    }

    fn set_terminal_focus(state: &mut App, focused: bool) {
        let focused = focused && !Self::shows_transcript(state);
        if focused {
            if let View::SessionDetail { session_id, .. } = &state.view {
                let session_id = session_id.clone();
                Self::reclaim_pane(state, &session_id);
            }
        }
        if state.terminal_focused != focused {
            state.terminal_focused = focused;
            Self::reflow(state);
        }
    }

    /// Takes `session_id`'s shared ptyd pane back to this view's size when
    /// another client resized it last. Only that session's bridge is
    /// signalled: the user may be working in another session elsewhere.
    fn reclaim_pane(state: &mut App, session_id: &str) {
        if !state.terminals.get(session_id).is_some_and(|t| t.follows_another_client()) {
            return;
        }
        Self::reflow(state);
        if let Some(client) = state.clients.get(session_id) {
            client.reclaim_size();
        }
    }

    /// Lazily shapes a `text_editor::Content` for every comment already
    /// loaded into `review_threads[pr_id]` that doesn't have one yet, for the
    /// PR attached to `session_id` — same signature shape as `ensure_diff`/
    /// `ensure_plan` (take the entity id, resolve what's needed internally).
    /// Called whenever the Marginalia panel (Split/Info) might have
    /// something new to render for this session: on navigating to it (see
    /// `NavigateSession`), on switching into Split/Info (see
    /// `SwitchDetailPanel`), and when a PR attaches to an already-open
    /// session (see `Event::PrOpened`) — deliberately never called for every
    /// PR at once (that's what made `App::new` block the window from
    /// appearing; see its comment on `review_threads`).
    /// Drops the detail panel's "Send message" draft when the selection
    /// moves to a different node than the one it's addressed to — a draft
    /// left open for a node that's no longer selected is stale (the panel
    /// stops showing it entirely, see `fleet_detail::action_row`'s own
    /// `id == selected` guard) and must not silently resurface, either by
    /// swallowing the next Escape (it isn't the composer the user now
    /// sees) or by reappearing verbatim if that old node is reselected
    /// later (code-review findings).
    /// The node the Fleet action row currently acts on: the opened island's
    /// selected node, or the selected board card's root orchestrator.
    fn fleet_action_target(state: &App) -> Option<String> {
        if !matches!(state.view, View::Fleet) {
            return None;
        }
        match &state.fleet_nav.open {
            Some(_) => state.fleet_nav.selected.clone(),
            None => match &state.fleet_nav.selected_island {
                Some(ninox_overview::islands::IslandKey::Root(id)) => Some(id.clone()),
                _ => None,
            },
        }
    }

    fn discard_draft_for_a_different_node(state: &mut App, newly_selected: &str) {
        if state.fleet_message_draft.as_ref().is_some_and(|(target, _)| target != newly_selected) {
            state.fleet_message_draft = None;
        }
    }

    fn ensure_comment_editors(state: &mut App, session_id: &str) {
        let Some(pr_id) = state.sessions.get(session_id).and_then(|s| s.pr_id) else { return };
        let Some(thread) = state.review_threads.get(&pr_id) else { return };
        for comment in thread {
            state.comment_editors.entry(comment.id).or_insert_with(|| {
                iced::widget::text_editor::Content::with_text(&comment.body)
            });
        }
    }

    /// Refreshes `state.plan_docs[orchestrator_id]` from the store + disk,
    /// synchronously — no async round-trip, since this is a local sqlite
    /// lookup plus a small markdown file read, not a subprocess. Called
    /// on-demand when the Plan panel is opened, and on every `PollSessions`
    /// tick while it stays open (no file-watcher; see the design doc).
    /// Skips the disk read entirely when the file's mtime hasn't moved.
    fn ensure_plan(state: &mut App, orchestrator_id: &str) -> Task<Message> {
        let Ok(Some(registration)) = state.engine.store.get_orchestrator_plan(orchestrator_id)
        else {
            state.plan_docs.remove(orchestrator_id);
            return Task::none();
        };
        let mtime = std::fs::metadata(&registration.file_path).and_then(|m| m.modified()).ok();
        let doc = state.plan_docs.entry(orchestrator_id.to_string()).or_default();
        let unchanged = mtime.is_some()
            && mtime == doc.last_mtime
            && doc.file_path.as_deref() == Some(registration.file_path.as_str());
        if unchanged {
            return Task::none();
        }
        doc.file_path = Some(registration.file_path.clone());
        doc.last_mtime = mtime;
        match std::fs::read_to_string(&registration.file_path) {
            Ok(text) => {
                use crate::components::markdown_blocks::{parse_blocks, BlockKind};
                doc.blocks = parse_blocks(&text)
                    .into_iter()
                    .map(|block| {
                        let is_checkbox = matches!(block.kind, BlockKind::ListItem { checked: Some(_), .. });
                        let prefix = match block.kind {
                            BlockKind::ListItem { checked: Some(true), .. } => "☑ ".to_string(),
                            BlockKind::ListItem { checked: Some(false), .. } => "☐ ".to_string(),
                            BlockKind::ListItem { ordinal: Some(n), checked: None } => format!("{n}. "),
                            BlockKind::ListItem { ordinal: None, checked: None } => "• ".to_string(),
                            _ => String::new(),
                        };
                        let display_text = format!("{prefix}{}", block.text);
                        let offset = prefix.len();
                        let mut spans: Vec<_> = block
                            .spans
                            .into_iter()
                            .map(|(range, style)| (range.start + offset..range.end + offset, style))
                            .collect();
                        if is_checkbox {
                            // Inter doesn't cover ☐/☑ — route them through
                            // the app's dingbat font instead of the list
                            // item's normal (sans) font.
                            use crate::components::markdown_blocks::InlineStyle;
                            spans.push((0..offset, InlineStyle { glyph: true, ..Default::default() }));
                        }
                        PlanBlockView {
                            kind: block.kind,
                            content: iced::widget::text_editor::Content::with_text(&display_text),
                            spans,
                            links: block.links,
                        }
                    })
                    .collect();
                doc.error = None;
            }
            Err(error) => {
                doc.error = Some(error.to_string());
            }
        }
        Task::none()
    }

    /// The opened island's orchestrator name, if the Fleet tab has one open
    /// — scopes a prompt-agent delivery (G2's `resolve_delivery`), same
    /// source `shell::shell` uses for the prompt bar's placeholder.
    fn ninox_prompt_scope(state: &App) -> Option<String> {
        let key = state.fleet_nav.open.as_ref()?;
        crate::components::fleet_islands::build_fleet_islands(state)
            .into_iter()
            .find(|i| &i.key == key)
            .map(|i| i.name)
    }

    /// Re-reads the latest turn's answer, the prompt agent's context figure
    /// and recent message events (relays) — on the existing 3s
    /// `PollSessions` tick and right after a delivery lands. A no-op until
    /// the latest turn has been delivered. Transcript turns keep polling
    /// after their turn ends: the agent resumes to report a relayed answer.
    fn poll_ninox_prompt(state: &mut App) -> Task<Message> {
        let Some(turn) = state.ninox_reply_drawer.turns.last() else { return Task::none() };
        if state.ninox_reply_drawer.poll_in_flight || !turn.wants_poll(chrono::Utc::now().timestamp_millis()) {
            return Task::none();
        }
        let source = turn.source.clone();
        let asked_at_ms = turn.asked_at_ms;
        let generation = state.ninox_prompt_generation;
        let store = state.engine.store.clone();
        state.ninox_reply_drawer.poll_in_flight = true;
        Task::future(async move {
            let poll = ninox_ui_shared::prompt_agent::reply::poll(store, source, asked_at_ms).await;
            Message::NinoxPromptPolled { generation, poll }
        })
    }

    /// Anything with its own Esc handling that a captured Esc from one of
    /// its inputs must not reach past.
    fn modal_open(state: &App) -> bool {
        state.spawn_modal.is_some() || state.catalogue_modal.is_some()
            || state.restart_all_confirm || state.workspaces_modal.is_some()
            || (matches!(state.view, View::Remotes) && (state.remotes.add.is_some() || state.remotes.remove_confirm.is_some()))
            || state.fleet_message_draft.is_some()
    }

    /// A terminal pane owns the keyboard: Esc must reach the agent (or the
    /// `machine add` subprocess), so it never collapses the drawer there.
    fn terminal_owns_keys(state: &App) -> bool {
        (state.terminal_focused && !Self::shows_transcript(state)
            && matches!(&state.view, View::SessionDetail { panel, .. } if panel.shows_terminal()))
            || (matches!(state.view, View::Remotes)
            && state.remotes.add.as_ref().is_some_and(|f| f.running && f.finished.is_none()))
    }

    /// Esc's first job, ahead of every view-level Esc: close the global
    /// search, else collapse an open reply drawer. Returns whether it did.
    fn dismiss_on_escape(state: &mut App) -> bool {
        if state.global_search.open {
            state.global_search.dismiss();
            true
        } else if state.sidebar.show_notifications {
            state.sidebar.show_notifications = false;
            true
        } else if state.ninox_reply_drawer.is_open() && !Self::terminal_owns_keys(state) {
            state.ninox_reply_drawer.collapse();
            true
        } else if matches!(state.view, View::Brain) && state.brain_view.editor.is_some() {
            state.brain_view.editor = None;
            true
        } else if matches!(state.view, View::Brain) && state.brain_view.delete_confirm.is_some() {
            state.brain_view.delete_confirm = None;
            true
        } else if state.ghost.confirm.is_some() && !Self::terminal_owns_keys(state) {
            state.ghost.confirm = None;
            true
        } else if state.ghost.selected.is_some() && matches!(state.view, View::Fleet | View::Plans) {
            state.ghost.selected = None;
            true
        } else {
            false
        }
    }

    /// Re-parses the latest turn's answer to markdown, linking known
    /// sessions and PRs.
    fn reparse_latest_answer(state: &mut App) {
        let (sessions, prs) = crate::components::shell::reply_drawer::link_terms(state);
        if let Some(turn) = state.ninox_reply_drawer.latest_mut() {
            let linked = crate::components::shell::reply_drawer::linkify(&turn.answer, &sessions, &prs);
            turn.rendered = iced::widget::markdown::parse(&crate::components::markdown_prepass::prepare(&linked)).collect();
            if let Some(content) = state.ninox_turn_select.get_mut(&turn.asked_at_ms) {
                *content = iced::widget::text_editor::Content::with_text(&turn.answer);
            }
        }
    }

    /// Kicks off an on-demand registry check for Settings' version line.
    /// Always re-runs on every `NavigateSettings` (unlike `ensure_models`'s
    /// cache-on-first-success) — the whole point is a fresh answer each
    /// time a user opens Settings to check, not a possibly-hours-stale one.
    fn ensure_version_check(state: &mut App) -> Task<Message> {
        state.version_check = VersionCheckState::Checking;
        Task::future(async move {
            use ninox_core::lifecycle::{binary_update, update_check::{GenericPackageUpdateSource, check_for_update}};
            let channel = ninox_core::config::AppConfig::load_or_default().update.channel;
            let supported = binary_update::platform_supported(std::env::consts::OS, std::env::consts::ARCH);
            let result = match binary_update::generic_package(std::env::consts::OS, channel).filter(|_| supported) {
                Some(package) => check_for_update(&GenericPackageUpdateSource { channel }, package, NINOX_VERSION)
                    .await
                    .map(|latest| latest.map(|v| v.to_string()))
                    .map_err(|e| e.to_string()),
                None => Ok(None),
            };
            Message::VersionCheckResult(result)
        })
    }

    /// Centres the macOS traffic lights on the nav bar (in window points,
    /// hence the zoom) and measures where they end.
    fn place_traffic_lights(state: &Self) -> Task<Message> {
        let bar_height = f64::from(crate::components::shell::nav::NAV_HEIGHT) * state.zoom;
        iced::window::get_oldest().then(move |id| match id {
            Some(id) => iced::window::run_with_handle(id, move |handle| crate::titlebar::place_traffic_lights(handle, bar_height))
                .map(Message::TrafficLightsPlaced),
            None => Task::none(),
        })
    }

    fn resize_terminals(state: &mut Self) -> Vec<(SessionId, u16, u16)> {
        use crate::components::session_detail::{TERM_CHROME_H, TERM_CHROME_W};

        let (cell_w, cell_h) = crate::components::terminal::cell_size(
            crate::components::terminal::FONT_SIZE,
        );
        // The v1 shell's left tree: a 44px rail when collapsed, ~238px
        // expanded (see `components::shell::tree`) — replaces the old
        // sidebar's width/hidden-reveal-rail pair.
        let sidebar_w = if state.fleet_nav.tree_collapsed {
            crate::components::shell::tree::RAIL_WIDTH
        } else {
            crate::components::shell::tree::EXPANDED_WIDTH
        };
        let info_w    = state.info_width + 5.0; // +5 for drag handle

        // Background sizing: what any session shows once Split (the default
        // panel) is active — this is also the authoritative size recorded on
        // `state` for sessions that don't have a `TerminalState` yet.
        // `TERM_CHROME_W`/`TERM_CHROME_H` are the header/tabs/term-frame
        // chrome that sits around the terminal `Canvas` in both the
        // `Terminal` and `Split` panels — see their doc comment in
        // `session_detail.rs` for the pixel-by-pixel derivation.
        let bg_cols = ((state.window_width - sidebar_w - info_w - TERM_CHROME_W).max(200.0) / cell_w) as u16;
        // Optional shell rows that push the session page down.
        let extra_h = if state.config_warning.is_some() { crate::components::config_banner::BANNER_HEIGHT } else { 0.0 }
            + if crate::components::config_banner::server_banner_shown(state) {
                crate::components::config_banner::BANNER_HEIGHT
            } else {
                0.0
            }
            + if crate::components::shell::reply_drawer::indicator_shown(state) {
                crate::components::shell::reply_drawer::INDICATOR_HEIGHT
            } else {
                0.0
            };
        let bg_rows = ((state.window_height - TERM_CHROME_H - extra_h).max(100.0) / cell_h) as u16;
        state.terminal_cols = bg_cols;
        state.terminal_rows = bg_rows;

        // The actively-viewed session uses whatever panel it's actually
        // showing. Split (workers) and Plan (orchestrators — same
        // terminal+side-pane layout, see `session_detail`'s `DetailPanel::Plan`
        // arm) narrow the terminal to leave room for the side pane; every
        // other panel uses the full (non-side-pane) width. A worker session
        // can still carry a stale `Plan` value in its stored panel (the
        // sticky global `worker_panel` can leak it over from an
        // orchestrator) — `session_detail` falls back to rendering that as
        // Split, so it must be sized the same here too, regardless of
        // whether `session_id` is actually an orchestrator.
        let active = match &state.view {
            View::SessionDetail { session_id, panel: DetailPanel::Split }
                if !state.orchestrators.iter().any(|o| &o.id == session_id) =>
            {
                Some((session_id.clone(), bg_cols, bg_rows))
            }
            View::SessionDetail { session_id, panel: DetailPanel::Plan } => {
                Some((session_id.clone(), bg_cols, bg_rows))
            }
            View::SessionDetail { session_id, .. } => {
                let cols = ((state.window_width - sidebar_w - TERM_CHROME_W).max(200.0) / cell_w) as u16;
                Some((session_id.clone(), cols, bg_rows))
            }
            _ => None,
        };

        // The embedded `machine add` terminal (`remotes::MACHINE_ADD_SESSION_ID`)
        // has no backing session and keeps its own fixed PTY size
        // (`remotes::machine_add_cols_rows`) set once at spawn time — it
        // must never be reflowed to the session-detail background size a
        // plain window resize computes here.
        let session_ids: Vec<SessionId> = state.terminals.keys()
            .filter(|sid| sid.as_str() != crate::components::remotes::MACHINE_ADD_SESSION_ID)
            .cloned()
            .collect();
        let mut resized = Vec::with_capacity(session_ids.len());
        for sid in session_ids {
            let (cols, rows) = match &active {
                Some((active_id, cols, rows)) if active_id == &sid => (*cols, *rows),
                _ => (bg_cols, bg_rows),
            };
            if let Some(term) = state.terminals.get_mut(&sid) {
                term.resize(cols, rows);
            }
            resized.push((sid, cols, rows));
        }
        resized
    }

    /// Re-derive pinboard edges (node-index pairs into `brain_view.entries`)
    /// from the index's resolved link graph. Called once per data change —
    /// `NavigateBrain`'s initial load and every `reload_brain_entries`
    /// (reindex, catalogue switch, background freshen) — never per canvas
    /// draw. A DB error is tolerated: warn and leave the pinboard edge-less
    /// rather than panic.
    fn refresh_brain_edges(state: &mut Self) {
        match state.brain.links_all() {
            Ok(links) => {
                state.brain_view.edges = ninox_core::brain::resolve_edges(
                    &state.brain_view.entries,
                    &links,
                );
            }
            Err(e) => {
                tracing::warn!("brain links_all: {e}");
                state.brain_view.edges.clear();
            }
        }
    }

    /// Recomputes the Graph view's nodes/edges/clusters from the current
    /// scope and filters, and clears the canvas cache so the next draw
    /// regenerates the static layer exactly once for the new data — never
    /// per frame, matching the brief's "the simulation stops after it
    /// settles". Called by every Graph-affecting message, and by
    /// `reload_brain_entries`/`NavigateBrain` so switching catalogues or
    /// reindexing doesn't leave a stale layout over an entry set that no
    /// longer exists.
    ///
    /// "Freeze layout" (`freeze_layout`) is a hard stop: once a layout
    /// exists, nothing here recomputes it (including a focus/filter change)
    /// until it's turned back off, which then catches up in one go. A
    /// partial reseed-only-new-nodes version would read more naturally for
    /// some cases, but risks silently-stale positions for nodes that
    /// should've moved; a flat freeze is at least never surprising about
    /// what it does. A scope switch is exempt from the freeze even so — the
    /// old nodes belong to a different subgraph, not stale positions within
    /// the same one, so showing them under the new scope would be wrong
    /// rather than merely frozen.
    fn refresh_brain_graph(state: &mut Self) {
        use crate::components::brain_graph as graph;

        let g = &mut state.brain_view.graph;
        if !g.kinds_initialized && !state.brain_view.entries.is_empty() {
            g.visible_kinds = graph::default_visible_kinds(&state.brain_view.entries);
            g.kinds_initialized = true;
        }
        if g.minimap.is_empty() && !state.brain_view.entries.is_empty() {
            g.minimap = graph::cluster_by_repo(&state.brain_view.entries);
        }
        let scope_changed = g.last_scope != Some(g.scope);
        g.last_scope = Some(g.scope);
        if g.freeze_layout && !scope_changed && !g.nodes.is_empty() && g.scope != graph::GraphScope::Clusters {
            return;
        }

        if g.scope == graph::GraphScope::Clusters {
            // A layout still running for the scope we left must not land here.
            g.pending_layout = None;
            g.fit_when_laid_out = false;
            g.clusters = g.minimap.clone();
            g.nodes.clear();
            g.edges.clear();
            g.spatial = None;
            g.hidden_neighbourhood_count = 0;
        } else {
            let ids = if g.scope == graph::GraphScope::Neighbourhood {
                match g.focus {
                    Some(focus) => {
                        let (ids, dropped) = graph::neighbourhood_capped(focus, &state.brain_view.edges, g.hops);
                        g.hidden_neighbourhood_count = dropped;
                        ids
                    }
                    None => {
                        g.hidden_neighbourhood_count = 0;
                        Vec::new()
                    }
                }
            } else {
                g.hidden_neighbourhood_count = 0;
                graph::visible_node_ids(
                    &state.brain_view.entries,
                    &state.brain_view.edges,
                    graph::GraphFilters {
                        scope: g.scope,
                        focus: g.focus,
                        hops: g.hops,
                        hide_unlinked: g.hide_unlinked,
                        visible_kinds: &g.visible_kinds,
                        repo_filter: g.repo_filter.as_deref(),
                    },
                )
            };
            g.clusters.clear();
            let key = graph::layout_key(&ids, &state.brain_view.edges);
            if let Some(nodes) = g.cached_layout(key) {
                g.pending_layout = None;
                g.fit_when_laid_out = false;
                Self::show_graph_layout(g, nodes, &state.brain_view.edges);
            } else if ids.len() < graph::ASYNC_LAYOUT_MIN_NODES {
                let nodes = graph::layout(&ids, &state.brain_view.edges, &state.brain_view.entries);
                g.remember_layout(key, nodes.clone());
                g.pending_layout = None;
                g.fit_when_laid_out = false;
                Self::show_graph_layout(g, nodes, &state.brain_view.edges);
            } else {
                let set: std::collections::HashSet<usize> = ids.iter().copied().collect();
                let edges = state.brain_view.edges.iter().copied().filter(|(a, b)| set.contains(a) && set.contains(b)).collect();
                let seeds = ids.iter().map(|&i| state.brain_view.entries.get(i).map(|e| e.id.clone()).unwrap_or_default()).collect();
                g.pending_layout = Some(key);
                g.layout_job = Some(graph::LayoutJob { key, node_ids: ids, edges, seeds });
                g.nodes.clear();
                g.edges.clear();
                g.spatial = None;
            }
        }
        g.cache.clear();
    }

    /// Neighbourhood needs a centre: the List selection, else the
    /// most-linked entry. Stays `None` only for a brain with no links, which
    /// the view shows as an empty state with a search.
    fn ensure_graph_focus(state: &mut Self) {
        let g = &mut state.brain_view.graph;
        if g.scope != crate::components::brain_graph::GraphScope::Neighbourhood || g.focus.is_some() {
            return;
        }
        let selected = state.brain_view.selected.as_ref().and_then(|id| state.brain_view.entries.iter().position(|e| &e.id == id));
        g.focus = selected.or_else(|| crate::components::brain_graph::most_linked(&state.brain_view.edges));
        if g.graph_selected.is_none() {
            g.graph_selected = g.focus;
        }
    }

    fn show_graph_layout(g: &mut crate::components::brain_graph::BrainGraphState, nodes: Vec<crate::components::brain_graph::GraphNode>, edges: &[(usize, usize)]) {
        use crate::components::brain_graph as graph;
        g.edges = graph::layout_edges(&nodes, edges);
        g.nodes = nodes;
        g.spatial = Some(graph::SpatialIndex::build(&g.nodes, 0.15));
    }

    /// Re-frames the Graph canvas's pan/zoom around whatever `refresh_brain_graph`
    /// just computed (brief: "+/−/fit buttons"). Called on scope changes —
    /// entering a new view should start framed sensibly — and by the
    /// explicit Fit button; deliberately NOT called from every
    /// `refresh_brain_graph` (a kind/repo filter toggle shouldn't yank the
    /// view the user is already looking at out from under them).
    fn fit_brain_graph(state: &mut Self) {
        use crate::components::brain_graph as graph;
        let g = &mut state.brain_view.graph;
        if g.pending_layout.is_some() && g.scope != graph::GraphScope::Clusters {
            g.fit_when_laid_out = true;
            return;
        }
        let top = graph::SCOPE_OVERLAY_RESERVE_PX
            + if g.hidden_neighbourhood_count > 0 { graph::SCOPE_HINT_RESERVE_PX } else { 0.0 };
        let (zoom, pan_x, pan_y) = if g.scope == graph::GraphScope::Clusters {
            let items: Vec<(f32, f32, f32)> = g.clusters.iter().map(|b| (b.x, b.y, b.radius)).collect();
            graph::fit_transform_reserving(&items, graph::FitReserve { top, bottom: 0.0 })
        } else {
            // Keep clear of the minimap in the bottom-right while it's open.
            let bottom = if state.config.brain_minimap_collapsed { 0.0 } else { graph::MINIMAP_RESERVE_PX };
            graph::fit_transform_for_nodes_reserving(&g.nodes, &g.edges, g.focus, graph::FitReserve { top, bottom })
        };
        g.zoom = zoom;
        g.pan_x = pan_x;
        g.pan_y = pan_y;
        g.cache.clear();
    }

    /// Refetches `backlinks`/`related`/`outlinks` for `brain_view.selected`
    /// from the index. No-ops when nothing is selected. Shared by
    /// `BrainSelectEntry` (fresh selection) and the entry-set reload path
    /// (index may have changed under a still-selected entry). The reading
    /// pane's markdown is NOT handled here: the select path parses it from
    /// the clicked entry, and `reload_brain_entries` re-parses it from the
    /// re-read entry set — since a background pull can change a selected
    /// entry's body, not just its graph.
    fn refresh_selection_graph(state: &mut Self) {
        let Some(id) = state.brain_view.selected.clone() else {
            return;
        };
        match state.brain.backlinks(&id) {
            Ok(backs) => state.brain_view.backlinks = backs,
            Err(e) => {
                tracing::warn!("brain backlinks({id}): {e}");
                state.brain_view.backlinks = Vec::new();
            }
        }
        match state.brain.related(&id, 6) {
            Ok(rel) => state.brain_view.related = rel,
            Err(e) => {
                tracing::warn!("brain related({id}): {e}");
                state.brain_view.related = Vec::new();
            }
        }
        match state.brain.outlinks(&id) {
            Ok(outs) => state.brain_view.outlinks = outs,
            Err(e) => {
                tracing::warn!("brain outlinks({id}): {e}");
                state.brain_view.outlinks = Vec::new();
            }
        }
    }

    /// Reload `brain_view.entries` from `state.brain` and re-derive
    /// everything downstream of the entry set: pinboard edges, ghost-
    /// selection cleanup, and the selection graph. Shared by
    /// `BrainSwitchCatalogue` (fresh local open), `BrainReindexed` (async
    /// rebuild landed), and `BrainFreshened` (background pull landed) so
    /// all three agree on what "the entry set changed" means.
    /// Catalogue roots a brain write/delete job runs against: the active
    /// catalogue, or under "All sources" the one the entry's id names.
    pub(crate) fn brain_job_paths(state: &Self, job: &ninox_ui_shared::brain_edit::Job) -> Vec<std::path::PathBuf> {
        let fallback = state.config.resolved_brain_path();
        ninox_ui_shared::brain_edit::brain_job_paths(&state.catalogues, state.active_catalogue, job, &fallback)
    }

    /// Runs `job` off the UI thread; `BrainJobDone` reloads the brain and
    /// then selects `select`. No embedding, like `BrainReindex`.
    fn start_brain_job(
        state: &mut Self,
        job: ninox_ui_shared::brain_edit::Job,
        select: Option<String>,
    ) -> Task<Message> {
        let paths = Self::brain_job_paths(state, &job);
        let job = ninox_ui_shared::brain_edit::unqualify_job_id(job, state.active_catalogue);
        Self::start_brain_job_at(state, paths, job, select, None)
    }

    /// [`Self::start_brain_job`] with the catalogue roots and raw id already
    /// resolved (an editor's save, pinned to the catalogue it opened in).
    fn start_brain_job_at(
        state: &mut Self,
        paths: Vec<std::path::PathBuf>,
        job: ninox_ui_shared::brain_edit::Job,
        select: Option<String>,
        editor: Option<u64>,
    ) -> Task<Message> {
        state.brain_view.job_running = Some(format!("{} {}", job.verb(), job.id()));
        let config = state.config.clone();
        Task::future(async move {
            let mut summaries = Vec::with_capacity(paths.len());
            for path in paths {
                match ninox_ui_shared::brain_edit::run_job(path, config.clone(), job.clone(), false).await {
                    Ok(summary) => summaries.push(summary),
                    Err(e) => return Message::BrainJobDone { job, select, editor, result: Err(e) },
                }
            }
            Message::BrainJobDone { job, select, editor, result: Ok(summaries.join(" · ")) }
        })
    }

    fn reload_brain_entries(state: &mut Self) {
        // `None`: the GUI brain panel is keyword-only for now — wiring it to the
        // same embedder the server constructs at startup is a natural follow-up,
        // deliberately out of scope for the CLI/HTTP-focused semantic search spec.
        match state.brain.query("", None, QueryFilters::default()) {
            Ok(entries) => {
                state.brain_view.entries = entries;
                Self::refresh_brain_edges(state);
                state.brain_view.last_indexed = Some(std::time::Instant::now());
                // The selected entry may have been renamed or deleted by
                // whatever changed the entry set — if it no longer resolves,
                // clear the pane instead of showing a ghost selection.
                let ghost = state
                    .brain_view
                    .selected
                    .as_ref()
                    .is_some_and(|id| !state.brain_view.entries.iter().any(|e| &e.id == id));
                if ghost {
                    state.brain_view.selected = None;
                    state.brain_view.markdown = Vec::new();
                    state.brain_view.backlinks = Vec::new();
                    state.brain_view.related = Vec::new();
                    state.brain_view.outlinks = Vec::new();
                } else if let Some(id) = state.brain_view.selected.clone() {
                    // The entry set was just re-read, and whatever changed
                    // it (background pull, reindex) may have changed the
                    // selected entry's BODY too — re-render the reading
                    // pane from the fresh copy instead of keeping the
                    // parse from selection time.
                    if let Some(e) = state.brain_view.entries.iter().find(|e| e.id == id) {
                        let body = crate::components::brain_panel::strip_leading_h1(&e.body);
                        let body = crate::components::brain_panel::insert_soft_breaks(&body);
                        state.brain_view.markdown = iced::widget::markdown::parse(
                            &crate::components::markdown_prepass::prepare(
                                &crate::components::brain_panel::preprocess_wikilinks_resolving(&body, |t| {
                                    crate::components::brain_panel::resolve_link(&state.brain_view.entries, t, Some(&e.source)).is_some()
                                }),
                            ),
                        ).collect();
                    }
                }
                Self::refresh_selection_graph(state);
                state.brain_view.loaded = true;

                // `BrainGraphState`'s focus/selection/node positions are
                // entry *indices*, not ids — fragile across a reload that
                // rebuilds `entries` from scratch (a removed or reordered
                // entry would otherwise leave them pointing at the wrong
                // row, or past the end). Reset and let a fresh selection
                // re-focus it; `refresh_brain_graph` also overrides
                // `freeze_layout` here since the old layout is for an
                // entry set that may no longer exist.
                state.brain_view.graph.focus = None;
                state.brain_view.graph.graph_selected = None;
                state.brain_view.graph.hovered = None;
                state.brain_view.graph.hovered_cluster = None;
                state.brain_view.graph.freeze_layout = false;
                state.brain_view.graph.minimap.clear();
                Self::ensure_graph_focus(state);
                Self::refresh_brain_graph(state);
            }
            Err(e) => tracing::error!("brain entries reload: {e}"),
        }
    }

    /// Background freshen for a (possibly remote-backed) catalogue — the
    /// pull-before-read front door the CLI (`open_synced`) and server
    /// (`freshen`) already use, delivered via `BrainFreshened`. Cheap to
    /// fire repeatedly: `pull_if_stale` is TTL-gated, and a catalogue with
    /// no remote degrades to a plain local open. Shared by catalogue
    /// switch and opening the Brain view.
    fn freshen_brain_task(config: AppConfig, path: std::path::PathBuf) -> Task<Message> {
        Task::future(async move {
            // First open of a config-declared remote catalogue materializes
            // its .sync.toml — the CLI does this in run_brain; without it a
            // fresh clone never learns its remote.
            if let Err(e) = ninox_core::brain_sync::ensure_sync_toml(&config, &path) {
                tracing::warn!("brain: failed to materialize .sync.toml: {e}");
            }
            match ninox_core::brain_sync::open_synced(&path, None).await {
                Ok(brain) => Message::BrainFreshened {
                    scope_id: BrainScopeId::Single(path),
                    brain: BrainScope::Single(Arc::new(brain)),
                },
                // A hard Err from open_synced means the LOCAL open failed
                // (remote failures degrade inside it) — a catalogue that
                // can't open locally has already surfaced its error on the
                // synchronous path, so drop this one quietly.
                Err(e) => {
                    tracing::warn!("brain freshen: {e}");
                    Message::Noop
                }
            }
        })
    }

    /// `freshen_brain_task`'s "All sources" twin: freshens every *enabled*
    /// catalogue (same materialize-then-`open_synced` front door, per
    /// catalogue) and merges the results. A catalogue that fails to freshen
    /// is logged and dropped from the merge rather than failing the whole
    /// background refresh.
    fn freshen_all_brain_task(
        config: AppConfig,
        catalogues: Vec<ninox_core::config::CatalogueRef>,
    ) -> Task<Message> {
        Task::future(async move {
            let mut sources = Vec::new();
            for cat in catalogues.iter().filter(|c| c.enabled) {
                if let Err(e) = ninox_core::brain_sync::ensure_sync_toml(&config, &cat.path) {
                    tracing::warn!("brain: failed to materialize .sync.toml for '{}': {e}", cat.name);
                }
                match ninox_core::brain_sync::open_synced(&cat.path, None).await {
                    Ok(brain) => sources.push(ninox_core::MergedSource {
                        name: cat.name.clone(),
                        index: Arc::new(brain),
                    }),
                    Err(e) => tracing::warn!("brain freshen: skipping catalogue '{}': {e}", cat.name),
                }
            }
            let brain = BrainScope::All(Arc::new(MergedBrainIndex::from_sources(sources)));
            let scope_id = brain.identity();
            Message::BrainFreshened { scope_id, brain }
        })
    }

    /// Freshens whatever scope is currently active — the single-catalogue
    /// or "All sources" twin of the two tasks above, picked by what
    /// `state.brain` is. Shared by `BrainSwitchCatalogue` and opening the
    /// Brain view.
    fn freshen_active_brain_task(state: &Self) -> Task<Message> {
        match &state.brain {
            BrainScope::Single(b) => Self::freshen_brain_task(state.config.clone(), b.path().to_path_buf()),
            BrainScope::All(_) => Self::freshen_all_brain_task(state.config.clone(), state.catalogues.clone()),
        }
    }

    /// Loads one fresh `WorkspaceRow` list across every repo ninox has
    /// spawned into — read-only (never calls `cleanup()`/prune). Shared by
    /// the periodic nav-chip refresh, opening the modal, and reloading
    /// after a prune.
    fn workspaces_rows_task(state: &Self) -> Task<Message> {
        let store = state.engine.store.clone();
        Task::future(async move {
            let retention_ms = AppConfig::load_or_default().session_retention.retention_millis();
            let roots = ninox_core::workspace::discover_repo_roots(&store).unwrap_or_default();
            let mut rows = Vec::new();
            for root in &roots {
                match ninox_core::workspace::build_workspace_rows(&store, root, retention_ms).await {
                    Ok(r) => rows.extend(r.into_iter().map(crate::components::workspaces_modal::WorkspaceRowView::from)),
                    Err(e) => tracing::warn!("workspaces rows {}: {e}", root.display()),
                }
            }
            Message::WorkspacesRowsLoaded(rows)
        })
    }

    /// Shared mutation logic.
    fn apply(state: &mut Self, message: Message) -> Task<Message> {
        match message {
            Message::EngineEvent(event) => Self::handle_engine_event(state, *event),

            Message::NavBack => Self::navigate_history(state, true),
            Message::NavForward => Self::navigate_history(state, false),

            Message::NavigateBoard { scope } => {
                state.global_search.dismiss();
                state.last_fleet_scope = scope.clone();
                state.view = View::Board { scope };
                // Off-screen sessions keep running detached; drop their view clients.
                state.clients.clear();
                state.attach.clear();
                Task::none()
            }

            Message::NavigateFleet => {
                state.global_search.dismiss();
                state.view = View::Fleet;
                state.fleet_nav.open = None;
                state.fleet_nav.selected = None;
                state.fleet_nav.selected_island = None;
                state.fleet_message_draft = None;
                state.ghost.selected = None;
                state.ghost.confirm = None;
                state.clients.clear();
                state.attach.clear();
                Task::none()
            }
            Message::NavigatePlans => {
                state.global_search.dismiss();
                state.view = View::Plans;
                state.clients.clear();
                state.attach.clear();
                state.orch_plans.reload(&state.engine.store);
                match state.orch_plans.selected.clone() {
                    Some(id) => Self::ensure_plan(state, &id),
                    None => Task::none(),
                }
            }
            Message::PlansSelectOrchestratorPlan(id) => {
                state.ghost.selected = None;
                state.ghost.confirm = None;
                let ui = &mut state.orch_plans;
                ui.selected = if ui.selected.as_ref() == Some(&id) { None } else { Some(id.clone()) };
                if state.orch_plans.selected.is_some() {
                    Self::ensure_plan(state, &id)
                } else {
                    Task::none()
                }
            }
            Message::PlansToggleShowEnded => {
                state.orch_plans.show_ended = !state.orch_plans.show_ended;
                state.orch_plans.reload(&state.engine.store);
                Task::none()
            }
            Message::PlansOpenOrchestrator(id) => {
                let navigate = Self::apply(state, Message::NavigateSession(id));
                let switch = Self::apply(state, Message::SwitchDetailPanel(crate::components::session_detail::DetailPanel::Plan));
                Task::batch([navigate, switch])
            }
            Message::NavigateWorkerTypes => {
                state.global_search.dismiss();
                state.view = View::WorkerTypes;
                state.clients.clear();
                state.attach.clear();
                state.worker_types.notice = None;
                state.worker_types.refresh(true);
                Task::none()
            }
            Message::WorkerTypeSelect(name) => {
                if let Some(entry) = state.worker_types.entries.iter().find(|e| e.spec.name == name) {
                    state.worker_types.form = crate::components::worker_types::WorkerTypeForm::from_entry(entry);
                    state.worker_types.error = None;
                    state.worker_types.notice = None;
                }
                Task::none()
            }
            Message::WorkerTypeNewDraft => {
                state.worker_types.form = crate::components::worker_types::WorkerTypeForm::blank();
                state.worker_types.error = None;
                state.worker_types.notice = None;
                Task::none()
            }
            Message::WorkerTypeNameChanged(name) => {
                state.worker_types.form.name = name;
                Task::none()
            }
            Message::WorkerTypeBriefAction(action) => {
                state.worker_types.form.brief.perform(action);
                Task::none()
            }
            Message::WorkerTypeHarnessChanged(harness) => {
                state.worker_types.form.harness = harness;
                Task::none()
            }
            Message::WorkerTypeUseWhenChanged(text) => {
                state.worker_types.form.use_when = text;
                Task::none()
            }
            Message::WorkerTypeModelChanged(model) => {
                state.worker_types.form.model = model;
                Task::none()
            }
            Message::WorkerTypeAllowDraftChanged(v) => {
                state.worker_types.form.allow_draft = v;
                Task::none()
            }
            Message::WorkerTypeDenyDraftChanged(v) => {
                state.worker_types.form.deny_draft = v;
                Task::none()
            }
            Message::WorkerTypeAskDraftChanged(v) => {
                state.worker_types.form.ask_draft = v;
                Task::none()
            }
            Message::WorkerTypeAllowRuleAdd => {
                let form = &mut state.worker_types.form;
                crate::components::worker_types::add_rule(&mut form.allow, &mut form.allow_draft);
                Task::none()
            }
            Message::WorkerTypeDenyRuleAdd => {
                let form = &mut state.worker_types.form;
                crate::components::worker_types::add_rule(&mut form.deny, &mut form.deny_draft);
                Task::none()
            }
            Message::WorkerTypeAskRuleAdd => {
                let form = &mut state.worker_types.form;
                crate::components::worker_types::add_rule(&mut form.ask, &mut form.ask_draft);
                Task::none()
            }
            Message::WorkerTypeAllowRuleRemove(i) => {
                if i < state.worker_types.form.allow.len() { state.worker_types.form.allow.remove(i); }
                Task::none()
            }
            Message::WorkerTypeDenyRuleRemove(i) => {
                if i < state.worker_types.form.deny.len() { state.worker_types.form.deny.remove(i); }
                Task::none()
            }
            Message::WorkerTypeAskRuleRemove(i) => {
                if i < state.worker_types.form.ask.len() { state.worker_types.form.ask.remove(i); }
                Task::none()
            }
            Message::WorkerTypeCanSpawnToggle(name) => {
                ninox_core::worker_type_cli::toggle_can_spawn(&mut state.worker_types.form.can_spawn, &name);
                Task::none()
            }
            Message::WorkerTypeToggleCanManageTypes => {
                state.worker_types.form.can_manage_types = !state.worker_types.form.can_manage_types;
                Task::none()
            }
            Message::WorkerTypeSave => {
                match crate::components::worker_types::save(&state.engine.store, &state.worker_types.form) {
                    Ok(spec) => {
                        state.worker_types.error = None;
                        state.worker_types.notice = Some(format!("saved '{}'", spec.name));
                        state.worker_types.form.original_name = Some(spec.name.clone());
                        state.worker_types.refresh(true);
                        Self::reload_config_after_worker_type_change(state);
                    }
                    Err(e) => {
                        state.worker_types.error = Some(e.to_string());
                        state.worker_types.notice = None;
                    }
                }
                Task::none()
            }
            Message::WorkerTypeReset => {
                let Some(name) = state.worker_types.form.original_name.clone() else { return Task::none() };
                match ninox_core::worker_type_cli::reset(&state.engine.store, None, &name) {
                    Ok(_) => {
                        state.worker_types.error = None;
                        state.worker_types.notice = Some(format!("reset '{name}' to its compiled-in default"));
                        state.worker_types.refresh(true);
                        Self::reload_config_after_worker_type_change(state);
                    }
                    Err(e) => {
                        state.worker_types.error = Some(e.to_string());
                        state.worker_types.notice = None;
                    }
                }
                Task::none()
            }
            Message::WorkerTypeDelete => {
                let Some(name) = state.worker_types.form.original_name.clone() else { return Task::none() };
                match ninox_core::worker_type_cli::delete(&state.engine.store, None, &name) {
                    Ok(()) => {
                        state.worker_types.error = None;
                        state.worker_types.notice = Some(format!("deleted '{name}'"));
                        state.worker_types.refresh(false);
                        Self::reload_config_after_worker_type_change(state);
                    }
                    Err(e) => {
                        state.worker_types.error = Some(e.to_string());
                        state.worker_types.notice = None;
                    }
                }
                Task::none()
            }
            Message::WorkerTypeExport => {
                let Some(name) = state.worker_types.form.original_name.clone() else { return Task::none() };
                match crate::components::worker_types::export_to_file(&name) {
                    Ok(path) => {
                        state.worker_types.error = None;
                        state.worker_types.notice = Some(format!("exported to {}", path.display()));
                    }
                    Err(e) => {
                        state.worker_types.error = Some(e.to_string());
                        state.worker_types.notice = None;
                    }
                }
                Task::none()
            }
            Message::WorkerTypeImportPathChanged(path) => {
                state.worker_types.import_path = path;
                Task::none()
            }
            Message::WorkerTypeImport => {
                let path = state.worker_types.import_path.trim().to_string();
                let result = std::fs::read_to_string(&path)
                    .map_err(|e| anyhow::anyhow!("could not read {path}: {e}"))
                    .and_then(|src| ninox_core::worker_type_cli::import(&state.engine.store, None, &src));
                match result {
                    Ok(names) => {
                        state.worker_types.error = None;
                        state.worker_types.notice = Some(format!("imported: {}", names.join(", ")));
                        state.worker_types.refresh(true);
                        Self::reload_config_after_worker_type_change(state);
                    }
                    Err(e) => {
                        state.worker_types.error = Some(e.to_string());
                        state.worker_types.notice = None;
                    }
                }
                Task::none()
            }
            Message::NavigateRemotes => {
                state.global_search.dismiss();
                state.view = View::Remotes;
                state.clients.clear();
                state.attach.clear();
                // Only a *running, unfinished* add can have been orphaned by
                // the `clients.clear()` above — a form the user hasn't
                // submitted yet (never spawned anything) or one already
                // resolved must not be re-derived here.
                if state.remotes.add.as_ref().is_some_and(|f| f.running && f.finished.is_none())
                    && !state.clients.contains_key(crate::components::remotes::MACHINE_ADD_SESSION_ID)
                {
                    Self::resolve_machine_add(state);
                }
                Self::refresh_remotes(state);
                Task::none()
            }
            Message::NavigateIdle => {
                state.global_search.dismiss();
                state.view = View::Idle;
                state.clients.clear();
                state.attach.clear();
                state.idle.notice = None;
                Self::refresh_idle(state);
                Task::none()
            }
            Message::IdlePrompt(only) => {
                if state.idle.busy {
                    return Task::none();
                }
                state.idle.busy = true;
                let store = state.engine.store.clone();
                let config = state.config.clone();
                let report = state.idle.report.clone();
                Task::future(async move {
                    let outcomes = ninox_ui_shared::idle_cli::send_prompts(
                        &store, &report, only.as_deref(), &config, config.port, now_ms_i64(),
                    )
                    .await;
                    Message::IdlePromptDone(outcomes.iter().map(|o| o.describe()).collect())
                })
            }
            Message::IdlePromptDone(lines) => {
                state.idle.busy = false;
                state.idle.notice = Some(Ok(if lines.is_empty() { "No orchestrator has idle workers.".into() } else { lines.join("\n") }));
                Self::refresh_idle(state);
                Task::none()
            }
            Message::IdleClean(id) => {
                if state.idle.busy {
                    return Task::none();
                }
                state.idle.busy = true;
                let store = state.engine.store.clone();
                let dirs = state.config.workspaces.build_output_dirs.clone();
                Task::future(async move {
                    let res = tokio::task::spawn_blocking(move || {
                        ninox_core::idle_workers::clean_idle_build_output(&store, &id, &dirs, now_ms_i64())
                            .map(|f| format!("{id}: freed {}", ninox_core::idle_workers::format_bytes(f.bytes())))
                            .map_err(|e| e.to_string())
                    })
                    .await
                    .unwrap_or_else(|e| Err(e.to_string()));
                    Message::IdleCleanDone(res)
                })
            }
            Message::IdleCleanDone(res) => {
                state.idle.busy = false;
                state.idle.notice = Some(res);
                Self::refresh_idle(state);
                Task::none()
            }
            Message::RemoteRemoveRequest(id) => {
                state.remotes.remove_confirm = Some(id);
                state.remotes.notice = None;
                Task::none()
            }
            Message::RemoteRemoveCancel => {
                state.remotes.remove_confirm = None;
                Task::none()
            }
            Message::RemoteRemoveConfirm => {
                if let Some(id) = state.remotes.remove_confirm.take() {
                    state.remotes.notice = Some(
                        ninox_ui_shared::machine::remove(&id)
                            .map(|()| format!("removed {id}"))
                            .map_err(|e| e.to_string()),
                    );
                    Self::refresh_remotes(state);
                }
                Task::none()
            }
            Message::RemoteAddOpen => {
                state.remotes.add = Some(crate::components::remotes::AddRemoteForm::default());
                state.remotes.notice = None;
                Task::none()
            }
            Message::RemoteAddHostChanged(v) => {
                if let Some(form) = &mut state.remotes.add {
                    form.host = v;
                    form.error = None;
                }
                Task::none()
            }
            Message::RemoteAddLabelChanged(v) => {
                if let Some(form) = &mut state.remotes.add {
                    form.label = v;
                }
                Task::none()
            }
            Message::RemoteAddCancel => {
                state.clients.remove(crate::components::remotes::MACHINE_ADD_SESSION_ID);
                state.terminals.remove(crate::components::remotes::MACHINE_ADD_SESSION_ID);
                state.remotes.add = None;
                Task::none()
            }
            Message::RemoteAddSubmit => {
                let Some(form) = &mut state.remotes.add else { return Task::none() };
                let host = form.host.trim().to_string();
                if host.is_empty() {
                    form.error = Some("Host is required".into());
                    return Task::none();
                }
                // Mirrors `ninox_ui_shared::machine::run_add`'s own
                // duplicate-target check (reloaded fresh rather than reusing
                // `state.remotes.rows`, which may be stale) — without this,
                // submitting an already-tracked host spawns a `machine add`
                // that fails with this exact message, and the post-exit
                // `AppConfig` check would then find the pre-existing profile
                // and wrongly report success.
                let already_tracked = AppConfig::load()
                    .map(|cfg| cfg.remote_machines.machines.iter().any(|m| m.ssh_target == host))
                    .unwrap_or(false);
                if already_tracked {
                    form.error = Some(format!(
                        "a machine profile already tracks {host} — remove it first (`ninox machine remove <id>`) \
                         to re-add it differently"
                    ));
                    return Task::none();
                }
                // Stored trimmed so `resolve_machine_add`'s later
                // `ssh_target == form.host` comparison can't miss a match
                // over leading/trailing whitespace the user typed.
                form.host = host.clone();
                let exe = match ninox_core::hooks::canonical_exe() {
                    Ok(exe) => exe,
                    Err(e) => {
                        form.error = Some(format!("could not resolve ninox binary: {e}"));
                        return Task::none();
                    }
                };
                let label = (!form.label.trim().is_empty()).then(|| form.label.trim().to_string());
                let argv = crate::components::remotes::machine_add_argv(&exe, &host, label.as_deref());
                if let Err(e) = Self::spawn_machine_add(state, argv) {
                    if let Some(form) = &mut state.remotes.add {
                        form.error = Some(e);
                    }
                }
                Task::none()
            }
            Message::RemoteAddClose => {
                state.clients.remove(crate::components::remotes::MACHINE_ADD_SESSION_ID);
                state.terminals.remove(crate::components::remotes::MACHINE_ADD_SESSION_ID);
                state.remotes.add = None;
                Task::none()
            }
            Message::ToggleTreeCollapsed => {
                state.fleet_nav.toggle_tree();
                // The rail's width changed, so reflow terminals and sync the
                // backing panes (same as a completed resize drag).
                let resized = Self::resize_terminals(state);
                for (sid, cols, rows) in resized {
                    if let Some(client) = state.clients.get(&sid) {
                        client.resize(cols, rows);
                    }
                }
                Task::none()
            }
            Message::FleetSelectIsland(key) => {
                state.ghost.selected = None;
                state.ghost.confirm = None;
                if completes_double_click(&mut state.fleet_last_island_click, &key) {
                    let islands = build_fleet_islands(state);
                    state.fleet_nav.open_island(key, &islands);
                } else {
                    if state.fleet_nav.selected_island.as_ref() != Some(&key) {
                        state.fleet_message_draft = None;
                    }
                    state.fleet_nav.selected_island = Some(key);
                }
                Task::none()
            }
            Message::FleetClearBoardSelection => {
                if state.fleet_nav.open.is_none() {
                    state.fleet_nav.selected_island = None;
                    state.fleet_message_draft = None;
                }
                Task::none()
            }
            Message::FleetOpenIsland(key) => {
                state.ghost.selected = None;
                state.ghost.confirm = None;
                state.view = View::Fleet;
                let islands = build_fleet_islands(state);
                state.fleet_nav.open_island(key, &islands);
                Task::none()
            }
            Message::FleetCloseIsland => {
                state.ghost.selected = None;
                state.ghost.confirm = None;
                state.fleet_nav.close_island();
                // Without this, a draft left open for a worker in this
                // island resurfaces verbatim if the same worker is ever
                // reselected later (code-review finding).
                state.fleet_message_draft = None;
                Task::none()
            }
            Message::FleetOpenNode(id) => {
                state.ghost.selected = None;
                state.ghost.confirm = None;
                state.global_search.dismiss();
                state.view = View::Fleet;
                let islands = build_fleet_islands(state);
                state.fleet_nav.open_node(id.clone(), &islands);
                Self::discard_draft_for_a_different_node(state, &id);
                // The detail panel's PR section needs this the moment the
                // node is selected, not gated behind a panel-tab switch like
                // `NavigateSession`'s Diff — the Fleet detail panel has no
                // tabs.
                Self::ensure_diff(state, &id)
            }
            Message::FleetNodeCardPressed(id) => {
                if completes_double_click(&mut state.fleet_last_node_click, &id) {
                    Self::apply(state, Message::NavigateSession(id))
                } else {
                    Self::apply(state, Message::FleetSelectNode(id))
                }
            }
            Message::FleetIslandMemberPressed { island, id } => {
                if completes_double_click(&mut state.fleet_last_node_click, &id) {
                    Self::apply(state, Message::NavigateSession(id))
                } else {
                    Self::apply(state, Message::FleetSelectIsland(island))
                }
            }
            Message::FleetSelectNode(id) => {
                state.ghost.selected = None;
                state.ghost.confirm = None;
                state.fleet_nav.select(id.clone());
                Self::discard_draft_for_a_different_node(state, &id);
                Self::ensure_diff(state, &id)
            }
            Message::FleetFilterMachine(filter) => {
                state.fleet_machine_filter = filter;
                Task::none()
            }
            Message::FleetSetSort(sort) => {
                state.fleet_sort = sort;
                Task::none()
            }
            Message::FleetSetCompact(compact) => {
                state.fleet_compact = compact;
                Task::none()
            }
            Message::FleetToggleFold(key) => {
                if !state.fleet_expanded_folds.remove(&key) {
                    state.fleet_expanded_folds.insert(key);
                }
                Task::none()
            }
            Message::FleetSendMessageOpen(id) => {
                state.fleet_message_draft = Some((id, String::new()));
                if matches!(state.view, View::SessionDetail { .. }) {
                    Self::set_terminal_focus(state, false);
                    return iced::widget::text_input::focus(crate::components::session_detail::composer_input_id());
                }
                Task::none()
            }
            Message::FleetSendMessageChanged(text) => {
                if let Some((_, draft)) = state.fleet_message_draft.as_mut() {
                    *draft = text;
                }
                Task::none()
            }
            Message::FleetSendMessageCancel => {
                state.fleet_message_draft = None;
                Task::none()
            }
            Message::FleetSendMessageSubmit => {
                let Some((id, text)) = state.fleet_message_draft.take() else { return Task::none(); };
                if text.trim().is_empty() {
                    return Task::none();
                }
                let store = state.engine.store.clone();
                let sessions_dir = ninox_core::config::AppConfig::sessions_dir();
                let mechanism = state.config.send_mechanism();
                Task::future(async move {
                    let (from, sender_pane) = ninox_core::messaging::resolve_interactive_sender(&store).await;
                    if let Err(e) = ninox_core::messaging::deliver_message(
                        &store, &sessions_dir, &id, &text, mechanism, Some(&from), sender_pane.as_ref(),
                        ninox_core::types::MessageKind::Freeform,
                    ).await {
                        tracing::error!("send message to {id}: {e}");
                    }
                    Message::Noop
                })
            }
            Message::NinoxPromptChanged(text) => {
                // Typing exits history recall — the field now holds fresh,
                // unsubmitted text, not a recalled entry anymore.
                state.ninox_prompt_history_cursor = None;
                state.ninox_prompt_draft_before_recall = None;
                state.ninox_prompt_draft = text;
                Task::none()
            }
            Message::ThinkingTick => {
                state.thinking_tick = state.thinking_tick.wrapping_add(1);
                Task::none()
            }
            Message::NinoxPromptSubmitted => {
                let draft = state.ninox_prompt_draft.trim().to_string();
                if draft.is_empty() {
                    return Task::none();
                }
                state.ninox_prompt_history_cursor = None;
                state.ninox_prompt_draft_before_recall = None;
                if state.ninox_prompt_history.last() != Some(&draft) {
                    state.ninox_prompt_history.push(draft.clone());
                }
                state.ninox_prompt_draft.clear();
                state.reduce_motion = crate::motion::reduce_motion();

                let scope = Self::ninox_prompt_scope(state);
                let delivery = crate::prompt_agent::resolve_delivery(
                    &state.ninox_prompt_target, scope.as_deref(), &draft,
                );
                let target_label = state.ninox_prompt_target.label().to_string();

                state.ninox_prompt_generation += 1;
                let generation = state.ninox_prompt_generation;
                state.ninox_reply_drawer.ask(draft, target_label, chrono::Utc::now().timestamp_millis());

                match delivery {
                    crate::prompt_agent::PromptDelivery::ToPromptAgent { text } => {
                        let store = state.engine.store.clone();
                        let config = state.config.clone();
                        let asked_at_ms = state.ninox_reply_drawer.turns.last().map_or(0, |t| t.asked_at_ms);
                        Task::future(async move {
                            let result = ninox_ui_shared::prompt_agent::reply::deliver_to_agent(store, &config, &text, asked_at_ms).await;
                            Message::NinoxPromptDelivered { generation, result }
                        })
                    }
                    crate::prompt_agent::PromptDelivery::ToOrchestrator { id, text } => {
                        let store = state.engine.store.clone();
                        let sessions_dir = ninox_core::config::AppConfig::sessions_dir();
                        let mechanism = state.config.send_mechanism();
                        Task::future(async move {
                            let baseline = ninox_ui_shared::prompt_agent::reply::orchestrator_baseline(&id).await;
                            let (from, sender_pane) = ninox_core::messaging::resolve_interactive_sender(&store).await;
                            let result = ninox_core::messaging::deliver_message(
                                &store, &sessions_dir, &id, &text, mechanism, Some(&from), sender_pane.as_ref(),
                                ninox_core::types::MessageKind::Freeform,
                            ).await
                                .map(|_| crate::prompt_agent::AnswerSource::OrchestratorPane { id, baseline })
                                .map_err(|e| e.to_string());
                            Message::NinoxPromptDelivered { generation, result }
                        })
                    }
                }
            }
            Message::NinoxPromptDelivered { generation, result } => {
                if generation != state.ninox_prompt_generation {
                    return Task::none(); // superseded by a clear or a newer submit
                }
                let Some(turn) = state.ninox_reply_drawer.latest_mut() else { return Task::none() };
                match result {
                    Ok(source) => {
                        turn.source = source;
                        Self::poll_ninox_prompt(state)
                    }
                    Err(message) => {
                        turn.fail(message, chrono::Utc::now().timestamp_millis());
                        Task::none()
                    }
                }
            }
            Message::NinoxPromptPolled { generation, poll } => {
                if generation != state.ninox_prompt_generation {
                    return Task::none();
                }
                if state.ninox_reply_drawer.apply_poll(poll, chrono::Utc::now().timestamp_millis()) {
                    Self::reparse_latest_answer(state);
                }
                Task::none()
            }
            Message::NinoxTargetPicked(label) => {
                state.ninox_prompt_target = if label == "ninox" {
                    crate::prompt_agent::PromptTarget::PromptAgent
                } else {
                    match state.orchestrators.iter().find(|o| o.name == label) {
                        Some(o) => crate::prompt_agent::PromptTarget::Orchestrator {
                            id: o.id.clone(), name: o.name.clone(),
                        },
                        None => crate::prompt_agent::PromptTarget::PromptAgent,
                    }
                };
                Task::none()
            }
            Message::NinoxPromptClear => {
                let target_label = state.ninox_prompt_target.label().to_string();
                // Bumped only once the clear succeeds: a failed clear must
                // not strand an in-flight delivery's turn on "thinking…".
                let generation = state.ninox_prompt_generation;
                match &state.ninox_prompt_target {
                    crate::prompt_agent::PromptTarget::PromptAgent => {
                        state.ninox_reply_drawer.clear_requested();
                        let store = state.engine.store.clone();
                        let config = state.config.clone();
                        Task::future(async move {
                            let result = ninox_ui_shared::prompt_agent::clear(store, &config).await.map_err(|e| e.to_string());
                            Message::NinoxPromptCleared { generation, target_label, result }
                        })
                    }
                    // No "clear context" operation exists for a directly-
                    // picked orchestrator's own session — never silently
                    // clear the unrelated prompt-agent singleton instead and
                    // claim success on its behalf (round-2 review: this
                    // previously always called `prompt_agent::clear` here,
                    // and the strip lied about whose context was reset).
                    crate::prompt_agent::PromptTarget::Orchestrator { .. } => {
                        Task::done(Message::NinoxPromptCleared {
                            generation, target_label,
                            result: Err("clear context isn't available for a picked orchestrator".to_string()),
                        })
                    }
                }
            }
            Message::NinoxPromptCleared { generation, target_label, result } => {
                if generation != state.ninox_prompt_generation {
                    return Task::none();
                }
                match result {
                    Ok(()) => {
                        state.ninox_prompt_generation += 1;
                        state.ninox_reply_drawer.clear();
                        state.ninox_turn_select.clear();
                    }
                    Err(message) => state.ninox_reply_drawer.clear_failed(format!("{target_label}: {message}")),
                }
                Task::none()
            }
            Message::NinoxDrawerCollapse => {
                state.ninox_reply_drawer.collapse();
                Task::none()
            }
            Message::NinoxTurnSelectToggle(asked_at_ms) => {
                if state.ninox_turn_select.remove(&asked_at_ms).is_none() {
                    if let Some(turn) = state.ninox_reply_drawer.turns.iter().find(|t| t.asked_at_ms == asked_at_ms) {
                        state
                            .ninox_turn_select
                            .insert(asked_at_ms, iced::widget::text_editor::Content::with_text(&turn.answer));
                    }
                }
                Task::none()
            }
            Message::NinoxTurnSelectAction(asked_at_ms, action) => {
                if let Some(content) = state.ninox_turn_select.get_mut(&asked_at_ms) {
                    apply_comment_action(content, action);
                }
                Task::none()
            }
            Message::NinoxDrawerReopen => {
                state.ninox_reply_drawer.reopen();
                Task::none()
            }
            Message::NinoxPromptHistoryUp => {
                if state.ninox_prompt_history.is_empty() {
                    return Task::none();
                }
                let next = match state.ninox_prompt_history_cursor {
                    None => {
                        state.ninox_prompt_draft_before_recall = Some(state.ninox_prompt_draft.clone());
                        state.ninox_prompt_history.len() - 1
                    }
                    Some(0) => 0,
                    Some(c) => c - 1,
                };
                state.ninox_prompt_history_cursor = Some(next);
                state.ninox_prompt_draft = state.ninox_prompt_history[next].clone();
                Task::none()
            }
            Message::NinoxPromptHistoryDown => {
                let Some(c) = state.ninox_prompt_history_cursor else { return Task::none(); };
                if c + 1 >= state.ninox_prompt_history.len() {
                    state.ninox_prompt_history_cursor = None;
                    state.ninox_prompt_draft = state.ninox_prompt_draft_before_recall.take().unwrap_or_default();
                } else {
                    state.ninox_prompt_history_cursor = Some(c + 1);
                    state.ninox_prompt_draft = state.ninox_prompt_history[c + 1].clone();
                }
                Task::none()
            }
            Message::FocusNinoxPrompt => {
                state.global_search.dismiss();
                iced::widget::text_input::focus(crate::components::shell::prompt_bar::prompt_input_id())
            }
            Message::NinoxPromptHistoryFocusChecked(going_up, focused_id) => {
                let prompt_id: iced::advanced::widget::Id =
                    crate::components::shell::prompt_bar::prompt_input_id().into();
                if focused_id != prompt_id {
                    return Task::none(); // Up/Down typed into some other field — not ours
                }
                App::apply(state, if going_up { Message::NinoxPromptHistoryUp } else { Message::NinoxPromptHistoryDown })
            }

            Message::BrainFocusSearch => {
                iced::widget::text_input::focus(crate::components::brain_list::search_input_id())
            }

            Message::EscapeCaptured => {
                if !Self::modal_open(state) {
                    Self::dismiss_on_escape(state);
                }
                Task::none()
            }
            Message::MousePressed => {
                let search = &mut state.global_search;
                if search.open && !search.hover_field && !search.hover_dropdown {
                    search.dismiss();
                }
                if state.terminal_click_marker {
                    state.terminal_click_marker = false;
                } else {
                    Self::set_terminal_focus(state, false);
                }
                if state.session_more_open && !state.session_more_hover {
                    state.session_more_open = false;
                }
                Task::none()
            }
            Message::WindowUnfocused => {
                state.global_search.dismiss();
                Task::none()
            }
            Message::GlobalSearchHover(part, hovered) => {
                match part {
                    crate::components::global_search::SearchPart::Field => state.global_search.hover_field = hovered,
                    crate::components::global_search::SearchPart::Dropdown => state.global_search.hover_dropdown = hovered,
                }
                Task::none()
            }
            Message::GlobalSearchFocus => {
                state.global_search.open = true;
                state.global_search.results = crate::components::global_search::run_search(state, &state.global_search.draft);
                iced::widget::text_input::focus(crate::components::global_search::search_input_id())
            }

            Message::GlobalSearchChanged(text) => {
                state.global_search.draft = text;
                state.global_search.open = true;
                state.global_search.cursor = None;
                // Computed once here, not in the view — `view()` re-runs on
                // every `update()` regardless of what changed (engine
                // events, the 3s poll), and `search_brain`'s
                // `BrainIndex::query` is an unbounded, unlimited scan (found
                // in review of PR #279).
                state.global_search.results = crate::components::global_search::run_search(state, &state.global_search.draft);
                Task::none()
            }

            Message::GlobalSearchEnterPressed => {
                match crate::components::global_search::hit_at(&state.global_search.results, state.global_search.cursor) {
                    Some(hit) => App::apply(state, Message::GlobalSearchOpenHit(hit)),
                    None => Task::none(),
                }
            }

            Message::GlobalSearchCursorFocusChecked(going_up, focused_id) => {
                if focused_id != crate::components::global_search::search_input_id().into() {
                    return Task::none();
                }
                let len = state.global_search.results.len();
                if len == 0 {
                    return Task::none();
                }
                // Clamp a stale cursor into range first (the live result
                // set can shrink between two arrow presses with no retype
                // in between) so both directions clamp symmetrically,
                // rather than only the increment branch re-checking `len`
                // (review of PR #279).
                let clamped = state.global_search.cursor.map(|c| c.min(len - 1));
                state.global_search.cursor = Some(match clamped {
                    None => if going_up { len - 1 } else { 0 },
                    Some(c) if going_up => c.saturating_sub(1),
                    Some(c) => (c + 1).min(len - 1),
                });
                Task::none()
            }

            Message::GlobalSearchOpenHit(hit) => {
                state.global_search.open = false;
                state.global_search.cursor = None;
                state.global_search.draft.clear();
                state.global_search.results = Default::default();
                match hit {
                    crate::components::global_search::GlobalSearchHit::Session { id, .. } => {
                        App::apply(state, Message::FleetOpenNode(id))
                    }
                    crate::components::global_search::GlobalSearchHit::Plan { orchestrator_id, .. } => {
                        let navigate = App::apply(state, Message::NavigateSession(orchestrator_id));
                        let switch = App::apply(
                            state,
                            Message::SwitchDetailPanel(crate::components::session_detail::DetailPanel::Plan),
                        );
                        Task::batch([navigate, switch])
                    }
                    crate::components::global_search::GlobalSearchHit::Brain { id, .. } => {
                        let navigate = App::apply(state, Message::NavigateBrain);
                        let select = App::apply(state, Message::BrainSelectEntry(id));
                        Task::batch([navigate, select])
                    }
                }
            }

            Message::NavigateSession(id) => {
                // The prompt-agent singleton (MLOPS-5415) has no session
                // page or terminal of its own — a richer reply drawer is
                // designed (PromptReply.dc.html) but pending user review,
                // not built yet. Guarded here, the single choke point every
                // navigation path (fleet, search, notifications) already
                // routes through, so no call site needs its own check.
                if !opens_session_page(state, &id) {
                    return Task::none();
                }
                state.global_search.dismiss();
                // Selecting an orchestrator auto-expands its workers in the
                // sidebar tree; selecting one of its workers keeps it open.
                if state.orchestrators.iter().any(|o| o.id == id) {
                    state.sidebar.expanded_orchestrators.insert(id.clone());
                } else if let Some(orch_id) = state
                    .sessions
                    .get(&id)
                    .and_then(|w| w.orchestrator_id.clone())
                {
                    state.sidebar.expanded_orchestrators.insert(orch_id);
                }
                Self::enter_session_page(state, &id, state.worker_panel);
                Self::load_session_activity(state, &id);
                state.unread_messages.remove(&id);
                // Drop every client that is no longer on screen — the tmux
                // sessions stay detached and running — and anything still
                // trying to attach one.
                state.clients.retain(|sid, _| sid == &id);
                state.attach.retain(|sid, _| sid == &id);
                // Fresh view: kill any previous client + emulator for this
                // session; attach repaints the whole screen into clean state.
                state.clients.remove(&id);
                state.terminals.remove(&id);
                state.attach.remove(&id);
                Self::resize_terminals(state);
                if matches!(state.worker_panel, DetailPanel::Split | DetailPanel::Info) {
                    Self::ensure_comment_editors(state, &id);
                }

                // Always fetched: the Diff tab shows its `+N −M` count on
                // every tab, not just once Diff is open.
                let diff_task = Task::batch([Self::ensure_diff(state, &id), Self::ensure_workspace_facts(state, &id)]);

                let attach_task = Self::begin_attach(state, &id, 0);
                Task::batch(vec![diff_task, attach_task])
            }

            Message::ClientAttach { session_id, argv } => {
                // Only attach if the user is still looking at this session.
                let viewing = matches!(&state.view,
                    View::SessionDetail { session_id: sid, .. } if sid == &session_id);
                if !viewing { return Task::none(); }
                // A live client already exists for this session — a
                // concurrent attach (e.g. a re-navigate that fired its own
                // ClientAttach) already won the race. Spawning another
                // would orphan two AttachedClients pointed at the same
                // session_id, one of which is stray.
                if state.clients.contains_key(&session_id) { return Task::none(); }

                let (cols, rows) = (state.terminal_cols, state.terminal_rows);
                let generation = state.next_client_generation;
                state.next_client_generation += 1;
                let argv = ninox_core::runtime::embedded_attach_args(argv);
                match ninox_core::client::AttachedClient::spawn(
                    state.engine.clone(), session_id.clone(), argv, cols, rows, generation,
                ) {
                    Ok(client) => {
                        tracing::info!(session = %session_id, generation, "terminal attach child spawned");
                        // Fresh emulator wired to the client so query replies
                        // (DSR/DA/kitty) flow back to tmux.
                        state.terminals.insert(
                            session_id.clone(),
                            crate::components::terminal::TerminalState::new(
                                cols, rows, Some(client.input_sender()),
                            ),
                        );
                        state.clients.insert(session_id.clone(), client);
                        let token = state.next_attach_token;
                        state.next_attach_token += 1;
                        state.attach
                            .entry(session_id.clone())
                            .or_insert_with(|| crate::attach::AttachProgress::new(token, 0, None))
                            .spawned();
                        // The client was spawned at the background size; the
                        // active panel may want a different one — reflow and
                        // push the real size to the client PTY.
                        let resized = Self::resize_terminals(state);
                        if let Some((_, c, r)) = resized.iter().find(|(sid, ..)| sid == &session_id) {
                            if let Some(client) = state.clients.get(&session_id) {
                                client.resize(*c, *r);
                            }
                        }
                        let budget = crate::attach::budget(Self::is_remote(state, &session_id));
                        Task::future(async move {
                            tokio::time::sleep(budget).await;
                            Message::AttachWatchdog { session_id, generation }
                        })
                    }
                    Err(e) => {
                        tracing::error!(session = %session_id, "attach client spawn failed: {e}");
                        let attempt = state.attach.get(&session_id).map_or(0, |p| p.attempt) + 1;
                        Self::retry_or_fail(state, &session_id, attempt, format!("could not start the attach process: {e}"))
                    }
                }
            }

            Message::AttachPrepared { session_id, token, outcome } => {
                let Some(p) = state.attach.get(&session_id).filter(|p| p.token == token) else { return Task::none() };
                let attempt = p.attempt;
                match outcome {
                    crate::attach::Prepared::Ready(argv) => Self::apply(state, Message::ClientAttach { session_id, argv }),
                    crate::attach::Prepared::Gone => {
                        tracing::info!(session = %session_id, "terminal attach: pane is gone");
                        if !state.clients.contains_key(&session_id) {
                            state.attach.remove(&session_id);
                        }
                        Task::none()
                    }
                    crate::attach::Prepared::Unavailable(reason) => {
                        Self::retry_or_fail(state, &session_id, attempt + 1, reason)
                    }
                }
            }

            Message::AttachRetry { session_id, token } => {
                let viewing = matches!(&state.view,
                    View::SessionDetail { session_id: sid, .. } if sid == &session_id);
                let Some(p) = state.attach.get(&session_id).filter(|p| p.token == token) else { return Task::none() };
                if !viewing || state.clients.contains_key(&session_id) { return Task::none(); }
                let attempt = p.attempt;
                Self::begin_attach(state, &session_id, attempt)
            }

            Message::AttachWatchdog { session_id, generation } => {
                let current = state.clients.get(&session_id).is_some_and(|c| c.generation == generation);
                // The client's own flag, not `ClientOutput`, which can be dropped.
                let silent = state.clients.get(&session_id).is_some_and(|c| !c.has_output())
                    && state.attach.get(&session_id).is_some_and(|p| !p.got_output);
                if !current || !silent { return Task::none(); }
                tracing::warn!(session = %session_id, generation, "terminal attach produced no output within the first-frame budget; killing it");
                state.clients.remove(&session_id);
                state.terminals.remove(&session_id);
                let attempt = state.attach.get(&session_id).map_or(0, |p| p.attempt) + 1;
                Self::retry_or_fail(state, &session_id, attempt, "no output from the pane host (is ptyd busy?)".to_string())
            }

            Message::AttachReap => {
                for (sid, c) in &state.clients {
                    if c.has_output() {
                        if let Some(p) = state.attach.get_mut(sid) {
                            p.got_output = true;
                        }
                    }
                }
                let exited: HashMap<SessionId, u64> = state.clients.iter()
                    .filter(|(_, c)| c.has_exited())
                    .map(|(sid, c)| (sid.clone(), c.generation))
                    .collect();
                let dead: Vec<(SessionId, u64)> = exited.iter()
                    .filter(|(sid, generation)| state.reap_pending.get(*sid) == Some(*generation))
                    .map(|(sid, generation)| (sid.clone(), *generation))
                    .collect();
                state.reap_pending = exited;
                let tasks: Vec<Task<Message>> = dead.into_iter().map(|(session_id, generation)| {
                    tracing::warn!(session = %session_id, generation, "terminal attach child exited without a delivered close event");
                    Self::apply(state, Message::EngineEvent(Box::new(Event::ClientClosed { session_id, generation })))
                }).collect();
                Task::batch(tasks)
            }

            Message::ReconnectTerminal(id) => {
                let viewing = matches!(&state.view,
                    View::SessionDetail { session_id: sid, .. } if sid == &id);
                if !viewing { return Task::none(); }
                tracing::info!(session = %id, "terminal reconnect requested");
                Self::begin_attach(state, &id, 0)
            }

            Message::SelectOrchestrator(id) => {
                if !state.sidebar.expanded_orchestrators.remove(&id) {
                    state.sidebar.expanded_orchestrators.insert(id);
                }
                Task::none()
            }

            Message::SpawnSession => {
                // Preselect the resolved default for the dialog's kind and
                // type (on an enabled harness — see `resolve_dialog_agent`).
                let choices = ninox_ui_shared::spawn_form::brain_choices(&state.config);
                let brain_reads: Vec<String> = choices.iter().filter(|c| c.selected).map(|c| c.name.clone()).collect();
                let brain_write = ninox_ui_shared::spawn_form::reconcile_write(&choices, &brain_reads, "default");
                let mut form = SpawnForm { brain_reads, brain_write, ..SpawnForm::default() };
                form.sync_to_resolved(&state.config);
                let task = Self::ensure_models(state, &form.harness);
                state.spawn_modal = Some(form);
                state.spawn_prompt = iced::widget::text_editor::Content::new();
                task
            }

            Message::SpawnFormName(v) => {
                if let Some(f) = &mut state.spawn_modal { f.name = v; f.error = None; }
                Task::none()
            }

            Message::SpawnFormKind(kind) => {
                let Some(f) = &mut state.spawn_modal else { return Task::none() };
                f.kind = kind;
                f.error = None;
                f.sync_to_resolved(&state.config);
                let harness = f.harness.clone();
                Self::ensure_models(state, &harness)
            }

            Message::SpawnFormWorkspace(v) => {
                if let Some(f) = &mut state.spawn_modal { f.workspace = v; f.error = None; }
                Task::none()
            }

            Message::SpawnFormHarness(h) => {
                // Re-clicking the already-selected chip must not wipe the
                // model choice (iced buttons fire on every press).
                if state.spawn_modal.as_ref().is_some_and(|f| f.harness == h) {
                    return Task::none();
                }
                let task = Self::ensure_models(state, &h);
                if let Some(f) = &mut state.spawn_modal {
                    f.harness = h;
                    f.harness_picked = true;
                    f.model_picked = false;
                    f.error = None;
                    f.sync_to_resolved(&state.config);
                }
                task
            }

            Message::SpawnFormModel(v) => {
                if let Some(f) = &mut state.spawn_modal {
                    if v == ninox_ui_shared::models::CUSTOM_SENTINEL {
                        f.custom_model = Some(String::new());
                    } else {
                        f.custom_model = None;
                        f.model = Some(v);
                    }
                    f.error = None;
                    f.model_picked = true;
                }
                Task::none()
            }

            Message::SpawnFormCustomModel(v) => {
                if let Some(f) = &mut state.spawn_modal { f.custom_model = Some(v); f.error = None; f.model_picked = true; }
                Task::none()
            }

            Message::SpawnFormToggleBrain(name) => {
                let choices = ninox_ui_shared::spawn_form::brain_choices(&state.config);
                if let Some(f) = &mut state.spawn_modal {
                    if let Some(pos) = f.brain_reads.iter().position(|n| n == &name) {
                        // A session always reads something.
                        if f.brain_reads.len() > 1 {
                            f.brain_reads.remove(pos);
                        }
                    } else {
                        f.brain_reads.push(name);
                        let order: Vec<&str> = choices.iter().map(|c| c.name.as_str()).collect();
                        f.brain_reads.sort_by_key(|n| order.iter().position(|o| o == n).unwrap_or(usize::MAX));
                    }
                    f.brain_write = ninox_ui_shared::spawn_form::reconcile_write(&choices, &f.brain_reads, &f.brain_write);
                    f.error = None;
                }
                Task::none()
            }

            Message::SpawnFormBrainWrite(name) => {
                if let Some(f) = &mut state.spawn_modal { f.brain_write = name; f.error = None; }
                Task::none()
            }

            Message::SpawnFormWorkerType(name) => {
                let Some(f) = &mut state.spawn_modal else { return Task::none() };
                f.worker_type = name;
                f.error = None;
                f.sync_to_resolved(&state.config);
                let harness = f.harness.clone();
                Self::ensure_models(state, &harness)
            }

            Message::SpawnFormCancel => {
                state.spawn_modal = None;
                Task::none()
            }

            Message::CatalogueModalOpen => {
                state.catalogue_modal = Some(CatalogueForm::default());
                Task::none()
            }

            Message::CatalogueFormName(v) => {
                if let Some(f) = &mut state.catalogue_modal { f.name = v; f.error = None; }
                Task::none()
            }

            Message::CatalogueFormPath(v) => {
                if let Some(f) = &mut state.catalogue_modal { f.path = v; f.error = None; }
                Task::none()
            }

            Message::CatalogueFormCancel => {
                state.catalogue_modal = None;
                Task::none()
            }

            Message::WorkspacesModalOpen => {
                state.workspaces_modal = Some(WorkspacesModalState { loading: true, ..Default::default() });
                Self::workspaces_rows_task(state)
            }

            Message::WorkspacesModalClose => {
                state.workspaces_modal = None;
                Task::none()
            }

            Message::WorkspacesRowsLoaded(rows) => {
                state.workspaces_summary = crate::components::workspaces_modal::leftover_summary(&rows);
                if let Some(modal) = &mut state.workspaces_modal {
                    modal.rows = rows;
                    modal.loading = false;
                }
                Task::none()
            }

            Message::WorkspacesPruneClicked => {
                let Some(modal) = &mut state.workspaces_modal else { return Task::none(); };
                if !modal.confirming_prune {
                    modal.confirming_prune = true;
                    return Task::none();
                }
                modal.confirming_prune = false;
                modal.loading = true;
                modal.status = Some("Pruning…".to_string());
                let store = state.engine.store.clone();
                Task::future(async move {
                    let retention_ms = AppConfig::load_or_default().session_retention.retention_millis();
                    let roots = ninox_core::workspace::discover_repo_roots(&store).unwrap_or_default();
                    let mut pruned = 0usize;
                    let mut had_error = false;
                    for root in &roots {
                        // `include_unmerged: false` — this minimal surface
                        // mirrors the CLI's conservative default only; the
                        // `--include-unmerged` lever stays CLI-only.
                        match ninox_core::workspace::sweep_repo_orphans_with_policy(&store, root, retention_ms, false).await {
                            Ok(report) => {
                                pruned += report.actions.iter()
                                    .filter(|a| a.outcome.as_ref().is_some_and(|o| o.worktree_removed))
                                    .count();
                            }
                            Err(e) => {
                                tracing::warn!("workspaces prune {}: {e}", root.display());
                                had_error = true;
                            }
                        }
                    }
                    Message::WorkspacesPruneDone(if had_error {
                        Err("prune failed for one or more repos — see log".to_string())
                    } else {
                        Ok(pruned)
                    })
                })
            }

            Message::WorkspacesPruneDone(result) => {
                let message = match &result {
                    Ok(n) => format!("Pruned {n} worktree(s)."),
                    Err(e) => e.clone(),
                };
                if let Some(modal) = &mut state.workspaces_modal {
                    modal.status = Some(message);
                }
                Self::workspaces_rows_task(state)
            }

            // Guard order mirrors the design spec: empty name, duplicate
            // name (vs `catalogue_options()`), empty path, path exists but
            // isn't a directory, then create/open failure. Each guard sets
            // `form.error` and returns with the modal still open and
            // `config.brain.catalogues` untouched.
            Message::CatalogueFormConfirm => {
                let Some(form) = state.catalogue_modal.clone() else { return Task::none(); };

                let name = form.name.trim().to_string();
                if name.is_empty() {
                    if let Some(f) = &mut state.catalogue_modal {
                        f.error = Some("give this catalogue a name".to_string());
                    }
                    return Task::none();
                }

                if state.config.catalogue_options().iter().any(|c| c.name == name) {
                    if let Some(f) = &mut state.catalogue_modal {
                        f.error = Some(format!("a catalogue named {name} already exists"));
                    }
                    return Task::none();
                }

                let path_input = form.path.trim().to_string();
                if path_input.is_empty() {
                    if let Some(f) = &mut state.catalogue_modal {
                        f.error = Some("path is required".to_string());
                    }
                    return Task::none();
                }

                let expanded = ninox_ui_shared::spawn_util::expand_tilde(&path_input);
                let path = std::path::PathBuf::from(&expanded);
                if path.exists() && !path.is_dir() {
                    if let Some(f) = &mut state.catalogue_modal {
                        f.error = Some(format!("{expanded} exists but isn't a directory"));
                    }
                    return Task::none();
                }

                if let Err(e) = std::fs::create_dir_all(&path) {
                    if let Some(f) = &mut state.catalogue_modal {
                        f.error = Some(format!("couldn't create {expanded}: {e}"));
                    }
                    return Task::none();
                }

                // Initialize the index — creates .index.db + .gitignore.
                if let Err(e) = BrainIndex::open(&path) {
                    if let Some(f) = &mut state.catalogue_modal {
                        f.error = Some(format!("couldn't initialize brain index: {e}"));
                    }
                    return Task::none();
                }

                Self::persist_config(state, "add catalogue", |c| {
                    c.brain.catalogues.push(ninox_core::config::CatalogueRef {
                        name: name.clone(),
                        path: path.clone(),
                        remote: None,
                        endpoint: None,
                        region: None,
                        cache_ttl_secs: None,
                        enabled: true,
                        cadence: "manual".to_string(),
                        access: "read_write".to_string(),
                    })
                });
                state.catalogues = state.config.catalogue_options();
                let idx = state
                    .catalogues
                    .iter()
                    .position(|c| c.path == path)
                    .unwrap_or_else(|| state.catalogues.len().saturating_sub(1));

                state.catalogue_modal = None;
                // Reuse BrainSwitchCatalogue's handler logic to open the
                // index for real and refresh the view.
                App::apply(state, Message::BrainSwitchCatalogue(Some(idx)))
            }

            Message::SpawnFormPrompt(action) => {
                state.spawn_prompt.perform(action);
                if let Some(f) = &mut state.spawn_modal {
                    f.error = None;
                }
                Task::none()
            }

            Message::SpawnLaunched { session_id, argv, first_message, spawner } => {
                let argv = match argv {
                    Ok(argv) => argv,
                    Err(cause) => return App::apply(state, Message::SessionLaunchFailed { session_id, action: "spawn", cause }),
                };
                let attach = App::apply(state, Message::ClientAttach { session_id: session_id.clone(), argv });
                let Some(message) = first_message else { return attach };
                let store = state.engine.store.clone();
                let config = state.config.clone();
                let deliver = Task::future(async move {
                    let result = ninox_ui_shared::cli::deliver_initial_prompt(&store, &config, &session_id, &message, spawner.as_deref())
                        .await
                        .map(|warning| if let Some(w) = warning { tracing::warn!("{w}") });
                    Message::SpawnPromptDelivered { session_id, result }
                });
                Task::batch([attach, deliver])
            }

            Message::SessionLaunchFailed { session_id, action, cause } => {
                tracing::warn!("{action} {session_id} failed: {cause}");
                let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64;
                Self::push_notification(state, Notification {
                    id:         format!("launch-failed-{session_id}-{ts}"),
                    kind:       NotificationKind::LaunchFailed,
                    title:      format!("{session_id}: {action} failed"),
                    body:       cause,
                    session_id: Some(session_id),
                    created_at: ts,
                });
                Task::none()
            }

            Message::SpawnPromptDelivered { session_id, result } => {
                if let Err(e) = result {
                    tracing::warn!("{e}");
                    let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64;
                    Self::push_notification(state, Notification {
                        id:         format!("initial-prompt-{session_id}-{ts}"),
                        kind:       NotificationKind::InitialPromptFailed,
                        title:      format!("{session_id} didn't get its initial prompt"),
                        body:       e,
                        session_id: Some(session_id),
                        created_at: ts,
                    });
                }
                Task::none()
            }

            Message::SpawnFormConfirm => {
                use crate::components::spawn_modal::SpawnKind;

                let Some(form) = state.spawn_modal.clone() else { return Task::none(); };
                let prompt = state.spawn_prompt.text().trim_end().to_string();
                let name = form.name.trim().to_string();
                if name.is_empty() {
                    // Only reachable via keyboard submit — the SPAWN button is
                    // disabled while the name is empty.
                    if let Some(f) = &mut state.spawn_modal {
                        f.error = Some("give this entry a name".to_string());
                    }
                    return Task::none();
                }

                // Both kinds share the agent (harness + model) and brain catalogue.
                let registry = state.config.registry();
                let agent = form.resolved(&state.config).agent;
                let claude_session_id = ninox_core::harness::new_claude_session_id();
                let permission_mode = ninox_core::harness::permission_mode(
                    ninox_core::harness::LaunchKind::Interactive,
                    state.config.worker_type_registry().get(&form.worker_type),
                );
                let base_cmd = registry.interactive_cmd(&agent, permission_mode, &claude_session_id);
                let brain = match ninox_core::brain_set::BrainSet::select(&state.config, &form.brain_reads, Some(&form.brain_write)) {
                    Ok(b) => b,
                    Err(e) => {
                        if let Some(f) = &mut state.spawn_modal { f.error = Some(e.to_string()); }
                        return Task::none();
                    }
                };

                match form.kind {
                    // ── Standalone path: an interactive, unattached session in
                    // a user-supplied workspace. Mirrors the orchestrator flow
                    // (interactive agent in tmux + PTY streaming) but with no
                    // Orchestrator record, and with the same worktree isolation
                    // + repo detection the CLI worker path uses (shared via
                    // ninox_ui_shared::spawn_util).
                    SpawnKind::Standalone => {
                        let workspace_input = form.workspace.trim().to_string();
                        if workspace_input.is_empty() {
                            // No workspace — keep the modal open (the UI also
                            // disables confirm, but SpawnFormConfirm must not
                            // silently proceed).
                            if let Some(f) = &mut state.spawn_modal {
                                f.error = Some("workspace is required for a standalone session".to_string());
                            }
                            return Task::none();
                        }

                        let workspace = match ninox_ui_shared::spawn_util::resolve_standalone_workspace(
                            &workspace_input,
                            state.config.send_mechanism() == SendMechanism::Inbox,
                            &state.config,
                        ) {
                            Ok(ws) => ws,
                            Err(e) => {
                                if let Some(f) = &mut state.spawn_modal {
                                    f.error = Some(e);
                                }
                                return Task::none();
                            }
                        };

                        let ts = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis();
                        let slug = slugify(&name);
                        let sid = if slug.is_empty() { format!("session-{ts}") } else { slug };

                        if ninox_core::messaging::is_reserved_session_id(&sid) {
                            // See `messaging::is_reserved_session_id`.
                            if let Some(f) = &mut state.spawn_modal {
                                f.error = Some(format!("'{sid}' is reserved for ninox itself — pick another name"));
                            }
                            return Task::none();
                        }

                        if state.sessions.contains_key(&sid) {
                            // Slugified name collides with an existing session
                            // id — upserting would silently overwrite that
                            // session's stored record, and the subsequent
                            // tmux-create (same session name) would then fail
                            // and mark the *hijacked* record Terminated. Keep
                            // the modal open so the user can rename instead.
                            if let Some(f) = &mut state.spawn_modal {
                                f.error = Some(format!(
                                    "a session named {sid} already exists — pick another name"
                                ));
                            }
                            return Task::none();
                        }
                        let Some((criteria, first_message)) = ninox_ui_shared::cli::standalone_initial_prompt(
                            &state.config, &sid, &form.worker_type, prompt.clone(),
                        ) else {
                            // The registry changed while the dialog was open.
                            if let Some(f) = &mut state.spawn_modal {
                                f.error = Some(format!("worker type {} is no longer available — pick another", form.worker_type));
                            }
                            return Task::none();
                        };
                        let queued_launch = ninox_ui_shared::session_queue::StandaloneLaunch {
                            id: sid.clone(),
                            name: name.clone(),
                            workspace_input: workspace.clone(),
                            worker_type: form.worker_type.clone(),
                            agent: agent.clone(),
                            catalogue_path: brain.write_str(),
                            brain: Some(brain.clone()),
                            prompt: first_message.clone(),
                            task_brief: prompt.clone(),
                            success_criteria: criteria.clone(),
                            started_at: ts as i64,
                            isolate_worktree: true,
                        };
                        match Self::admit_spawn(state, queued_launch.skeleton(), ninox_ui_shared::session_queue::QueuedLaunch::Standalone(queued_launch)) {
                            Err(e) => {
                                if let Some(f) = &mut state.spawn_modal {
                                    f.error = Some(e);
                                }
                                return Task::none();
                            }
                            Ok(false) => return Task::none(),
                            Ok(true) => {}
                        }
                        state.spawn_modal = None;

                        let session = Session {
                            id:              sid.clone(),
                            orchestrator_id: None,
                            name:            name.clone(),
                            repo:            String::new(),
                            status:          SessionStatus::Working,
                            agent_type:      agent.harness.clone(),
                            cost_usd:        0.0,
                            started_at:      ts as i64,
                            pr_number:       None,
                            pr_id:           None,
                            workspace_path:  Some(workspace.clone()),
                            pid:             None,
                            model:           agent.model.clone(),
                            context_tokens:  None,
                            brain_reads:     brain.reads.iter().map(|r| r.to_string_lossy().into_owned()).collect(),
                            brain_write:     Some(brain.write_str()),
                            context_used_pct: None, context_total_tokens: None, context_window_size: None,
                            claude_session_id: Some(claude_session_id.clone()),
                            summary:         None,
                            terminal_at:         None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: Some(form.worker_type.clone()),
            success_criteria: criteria,
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
                        let _ = state.engine.store.upsert_session(&session);
                        state.sessions.insert(session.id.clone(), session.clone());
                        state.engine.emit(Event::SessionSpawned(session));

                        Self::enter_session_page(state, &sid, DetailPanel::Terminal);

                        let engine = state.engine.clone();
                        let nm     = name;
                        let ts_i64 = ts as i64;
                        let config = state.config.clone();
                        let task_brief = prompt;
                        let spawner: Option<String> = None;
                        let seed_type = form.worker_type.clone();

                        Task::future(async move {
                            // Isolate the session on its own branch/worktree when
                            // the workspace is a git repo; otherwise work in the
                            // directory itself (same fallback as run_spawn).
                            let effective_ws =
                                match ninox_ui_shared::spawn_util::create_worker_worktree(
                                    &workspace, &sid,
                                    config.send_mechanism() == SendMechanism::Inbox,
                                    &config,
                                ).await {
                                    Ok(path) => path,
                                    Err(e) => {
                                        tracing::warn!(
                                            "worktree creation failed for {sid}, using shared workspace: {e}"
                                        );
                                        workspace.clone()
                                    }
                                };
                            ninox_ui_shared::spawn_util::seed_worker_skills(&effective_ws, &config, &agent.harness).await;
                            if let Err(e) = ninox_ui_shared::spawn_util::seed_permissions_for_type_name(
                                std::path::Path::new(&effective_ws), &agent.harness, Some(&seed_type), &config,
                            ) {
                                ninox_ui_shared::session_queue::release(&engine.store, &sid);
                                return Message::SpawnLaunched { session_id: sid, argv: Err(format!("{e:#}")), first_message, spawner };
                            }
                            // What a fleet restore re-briefs this session with if
                            // its conversation can't be resumed.
                            let branch = ninox_core::fleet::probe::current_branch(std::path::Path::new(&effective_ws));
                            if let Err(e) = engine.store.record_spawn_facts(&sid, &task_brief, branch.as_deref()) {
                                tracing::warn!("record spawn facts for {sid}: {e}");
                            }

                            // Repo slug from the base workspace's git remote so
                            // poll_github can talk to the right owner/repo.
                            let repo = ninox_ui_shared::spawn_util::repo_from_workspace(&workspace)
                                .unwrap_or_default();

                            // No NINOX_ORCHESTRATOR_ID and no caller-type vars:
                            // this session is unattached and reports to no one.
                            let attach_sid = sid.clone();
                            let store = engine.store.clone();
                            let attach = ninox_ui_shared::spawn_util::spawn_interactive_session(
                                engine,
                                ninox_ui_shared::spawn_util::InteractiveSpawnParams {
                                    session_id:      sid,
                                    name:            nm,
                                    workspace:       effective_ws,
                                    repo,
                                    orchestrator_id: None,
                                    agent,
                                    base_cmd,
                                    brain: Some(brain),
                                    extra_env:       Vec::new(),
                                    started_at:      ts_i64,
                                    claude_session_id,
                                    failure_status:  ninox_core::SessionStatus::Terminated,
                                    summary:         None,
                                    permission_mode,
                                },
                            )
                            .await;
                            ninox_ui_shared::session_queue::release(&store, &attach_sid);
                            Message::SpawnLaunched { session_id: attach_sid, argv: attach, first_message, spawner }
                        })
                    }

                    // ── Orchestrator path: existing flow, stamped with the
                    // selected agent preset + brain catalogue.
                    SpawnKind::Orchestrator => {
                        let ts = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis();

                        let slug = slugify(&name);
                        let orch_id = if slug.is_empty() { format!("orch-{ts}") } else { slug };

                        if ninox_core::messaging::is_reserved_session_id(&orch_id) {
                            // See `messaging::is_reserved_session_id`.
                            if let Some(f) = &mut state.spawn_modal {
                                f.error = Some(format!("'{orch_id}' is reserved for ninox itself — pick another name"));
                            }
                            return Task::none();
                        }

                        if state.sessions.contains_key(&orch_id)
                            || state.orchestrators.iter().any(|o| o.id == orch_id)
                        {
                            // Same hazard as the standalone path: a duplicate
                            // name would upsert over an existing session/
                            // orchestrator record, then fail tmux-create and
                            // mark the hijacked record Terminated. Keep the
                            // modal open so the user can rename instead.
                            if let Some(f) = &mut state.spawn_modal {
                                f.error = Some(format!(
                                    "a session named {orch_id} already exists — pick another name"
                                ));
                            }
                            return Task::none();
                        }
                        let queued_launch = ninox_ui_shared::session_queue::OrchestratorLaunch {
                            id: orch_id.clone(),
                            name: name.clone(),
                            prompt: (!prompt.trim().is_empty()).then(|| prompt.clone()),
                            parent_orchestrator_id: None,
                            agent: agent.clone(),
                            catalogue_path: brain.write_str(),
                            brain: Some(brain.clone()),
                            spawner: std::env::var("NINOX_ORCHESTRATOR_ID").ok().filter(|s| !s.is_empty()),
                            started_at: ts as i64,
                        };
                        match Self::admit_spawn(state, queued_launch.skeleton(), ninox_ui_shared::session_queue::QueuedLaunch::Orchestrator(queued_launch)) {
                            Err(e) => {
                                if let Some(f) = &mut state.spawn_modal {
                                    f.error = Some(e);
                                }
                                return Task::none();
                            }
                            Ok(false) => return Task::none(),
                            Ok(true) => {}
                        }
                        state.spawn_modal = None;

                        let orch = Orchestrator {
                            id:         orch_id,
                            name:       name.clone(),
                            created_at: ts as i64,
                        };

                        // Each orchestrator gets its own subdirectory under the root.
                        // AGENTS.md/CLAUDE.md and hooks live in the root and are inherited.
                        let ws = state.orchestrator_root
                            .join(&orch.id)
                            .to_string_lossy()
                            .to_string();

                        let _ = state.engine.store.upsert_orchestrator(&orch);
                        state.orchestrators.push(orch.clone());
                        state.engine.emit(Event::OrchestratorSpawned(orch.clone()));

                        let session = Session {
                            id:              orch.id.clone(),
                            orchestrator_id: None,
                            name:            name.clone(),
                            repo:            String::new(),
                            status:          SessionStatus::Working,
                            agent_type:      agent.harness.clone(),
                            cost_usd:        0.0,
                            started_at:      ts as i64,
                            pr_number:       None,
                            pr_id:           None,
                            workspace_path:  Some(ws.clone()),
                            pid:             None,
                            model:           agent.model.clone(),
                            context_tokens:  None,
                            brain_reads:     brain.reads.iter().map(|r| r.to_string_lossy().into_owned()).collect(),
                            brain_write:     Some(brain.write_str()),
                            context_used_pct: None, context_total_tokens: None, context_window_size: None,
                            claude_session_id: Some(claude_session_id.clone()),
                            summary:         None,
                            terminal_at:         None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            // An orchestrator's type is "Orchestrator" by definition — it's
            // what `can_spawn` resolution keys on — so the Type field (G4)
            // offers nothing for this kind and `form.worker_type` isn't
            // even rendered; recorded implicitly instead (round-2 review on
            // PR #282).
            worker_type: Some("Orchestrator".to_string()),
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
                        let _ = state.engine.store.upsert_session(&session);
                        state.sessions.insert(session.id.clone(), session.clone());
                        state.engine.emit(Event::SessionSpawned(session));

                        Self::enter_session_page(state, &orch.id.clone(), DetailPanel::Terminal);

                        let engine            = state.engine.clone();
                        let sid               = orch.id.clone();
                        let nm                = name;
                        let ts_i64            = ts as i64;
                        let orch_agent        = agent;
                        let spawner           = std::env::var("NINOX_ORCHESTRATOR_ID").ok().filter(|s| !s.is_empty());
                        let config            = state.config.clone();
                        let first_message     = (!prompt.trim().is_empty())
                            .then(|| ninox_ui_shared::cli::orchestrator_initial_brief(&sid, &prompt, spawner.as_deref()));

                        Task::future(async move {
                            if let Err(e) = tokio::fs::create_dir_all(&ws).await {
                                tracing::error!("mkdir orchestrator workspace {ws}: {e}");
                            }
                            if let Err(e) = ninox_ui_shared::spawn_util::seed_permissions_for_type_name(
                                std::path::Path::new(&ws), &orch_agent.harness, Some("Orchestrator"), &config,
                            ) {
                                ninox_ui_shared::session_queue::release(&engine.store, &sid);
                                return Message::SpawnLaunched { session_id: sid, argv: Err(format!("{e:#}")), first_message, spawner };
                            }

                            // Orchestrator sessions get the caller-type vars and
                            // their own id so spawned workers can report back.
                            let extra_env = vec![
                                ("NINOX_ORCHESTRATOR_ID".to_string(), sid.clone()),
                                ("NINOX_CALLER_TYPE".to_string(),     "orchestrator".to_string()),
                            ];

                            let attach_sid = sid.clone();
                            let store = engine.store.clone();
                            let attach = ninox_ui_shared::spawn_util::spawn_interactive_session(
                                engine,
                                ninox_ui_shared::spawn_util::InteractiveSpawnParams {
                                    session_id:      sid,
                                    name:            nm,
                                    workspace:       ws,
                                    repo:            String::new(),
                                    orchestrator_id: None,
                                    agent:           orch_agent,
                                    base_cmd,
                                    brain: Some(brain),
                                    extra_env,
                                    started_at:      ts_i64,
                                    claude_session_id,
                                    failure_status:  ninox_core::SessionStatus::Terminated,
                                    summary:         None,
                                    permission_mode,
                                },
                            )
                            .await;
                            ninox_ui_shared::session_queue::release(&store, &attach_sid);
                            Message::SpawnLaunched { session_id: attach_sid, argv: attach, first_message, spawner }
                        })
                    }
                }
            }

            Message::SwitchDetailPanel(new_panel) => {
                state.session_more_open = false;
                state.session_confirm = None;
                if !new_panel.shows_terminal() {
                    state.terminal_focused = false;
                }
                let session_id = if let View::SessionDetail { session_id, panel } = &mut state.view {
                    *panel = new_panel;
                    state.worker_panel = new_panel;
                    Some(session_id.clone())
                } else {
                    None
                };
                // Entering/leaving Split changes how much width the terminal
                // canvas actually has, so the grid must be reflowed to match.
                let resized = Self::resize_terminals(state);
                for (sid, cols, rows) in resized {
                    if let Some(client) = state.clients.get(&sid) {
                        client.resize(cols, rows);
                    }
                }
                match (new_panel, session_id) {
                    (DetailPanel::Diff, Some(sid)) => Self::ensure_diff(state, &sid),
                    (DetailPanel::Plan, Some(sid)) => Self::ensure_plan(state, &sid),
                    (DetailPanel::Split, Some(sid)) => {
                        Self::ensure_comment_editors(state, &sid);
                        Self::load_session_activity(state, &sid);
                        Self::ensure_workspace_facts(state, &sid)
                    }
                    (DetailPanel::Info, Some(sid)) => {
                        Self::ensure_comment_editors(state, &sid);
                        Task::none()
                    }
                    _ => Task::none(),
                }
            }

            Message::PlanEditorAction { orchestrator_id, block, action } => {
                if !action.is_edit() {
                    if let Some(doc) = state.plan_docs.get_mut(&orchestrator_id) {
                        if let Some(b) = doc.blocks.get_mut(block) {
                            b.content.perform(action);
                        }
                    }
                }
                Task::none()
            }

            Message::KillSession(id) => {
                state.clients.remove(&id);
                state.terminals.remove(&id);
                state.attach.remove(&id);
                let engine = state.engine.clone();
                Task::future(async move {
                    if let Err(e) = engine.terminate_session(&id).await {
                        tracing::error!("kill session {id}: {e}");
                    }
                    Message::Noop
                })
            }

            Message::TerminalFocus => {
                if Self::shows_transcript(state) {
                    return Task::none();
                }
                let View::SessionDetail { panel, .. } = &state.view else { return Task::none() };
                let shows_terminal = panel.shows_terminal();
                state.session_more_open = false;
                let switch = if shows_terminal {
                    Task::none()
                } else {
                    App::apply(state, Message::SwitchDetailPanel(DetailPanel::Terminal))
                };
                Self::set_terminal_focus(state, true);
                switch
            }
            Message::TerminalClicked => {
                state.terminal_click_marker = true;
                Self::set_terminal_focus(state, true);
                Task::none()
            }
            Message::TerminalRelease => {
                Self::set_terminal_focus(state, false);
                Task::none()
            }
            Message::SessionMoreToggle => {
                state.session_more_open = !state.session_more_open;
                Task::none()
            }
            Message::SessionMoreHover(hovered) => {
                state.session_more_hover = hovered;
                Task::none()
            }
            Message::SessionMenuPick(action) => {
                state.session_more_open = false;
                state.session_more_hover = false;
                let View::SessionDetail { session_id, .. } = &state.view else { return Task::none() };
                let workspace = state.sessions.get(session_id).and_then(|s| s.workspace_path.clone());
                match crate::components::session_detail::action_message(action, session_id, workspace.as_deref()) {
                    Some(msg) => App::apply(state, msg),
                    None => Task::none(),
                }
            }
            Message::SessionConfirmAsk(what) => {
                state.session_more_open = false;
                state.session_confirm = Some(what);
                Task::none()
            }
            Message::SessionConfirmCancel => {
                state.session_confirm = None;
                Task::none()
            }
            Message::SessionConfirmGo => {
                let Some(what) = state.session_confirm.take() else { return Task::none() };
                let View::SessionDetail { session_id, .. } = &state.view else { return Task::none() };
                let msg = crate::components::session_detail::confirmed_message(what, session_id);
                App::apply(state, msg)
            }
            Message::SessionBack => {
                let View::SessionDetail { session_id, .. } = &state.view else { return Task::none() };
                let id = session_id.clone();
                state.terminal_focused = false;
                state.session_more_open = false;
                state.session_confirm = None;
                state.clients.clear();
                state.attach.clear();
                App::apply(state, Message::FleetOpenNode(id))
            }
            Message::PopOutSession(id) => {
                pop_out(&id);
                Task::none()
            }
            Message::SessionFactsLoaded(id, facts) => {
                match facts {
                    Some(f) => { state.workspace_facts.insert(id, f); }
                    None => { state.workspace_facts.remove(&id); }
                }
                Task::none()
            }

            Message::RefileSession(id) => {
                let Some(session) = state.sessions.get(&id).cloned() else { return Task::none(); };
                let is_orch = state.orchestrators.iter().any(|o| o.id == id);
                let ts = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64;
                let Some(req) = ninox_ui_shared::restart::refile_request(&state.engine.store, session, is_orch, &state.config, ts) else {
                    tracing::warn!("refile {id}: no workspace recorded, cannot respawn");
                    return Task::none();
                };
                // Drop the live client/grid first so the old PTY doesn't
                // fight the respawn for the same tmux session name.
                state.clients.remove(&id);
                state.terminals.remove(&id);
                state.attach.remove(&id);
                // Re-file respawns the same id onto a fresh workspace/branch
                // — the cached diff is for the OLD incarnation and would
                // otherwise flash stale content until the next fetch.
                state.diffs.remove(&id);

                let engine = state.engine.clone();
                let config = state.config.clone();
                Task::future(async move {
                    let attach = ninox_ui_shared::spawn_util::relaunch_in_place(engine, req, &config, true).await;
                    match attach {
                        Ok(argv) => Message::ClientAttach { session_id: id, argv },
                        Err(cause) => Message::SessionLaunchFailed { session_id: id, action: "re-file", cause },
                    }
                })
            }

            Message::ResumeSession(id) => {
                let Some(session) = state.sessions.get(&id).cloned() else { return Task::none(); };
                let is_orch = state.orchestrators.iter().any(|o| o.id == id);
                let Some(plan) = resume_plan(&state.engine.store, &session, is_orch, &state.config) else {
                    tracing::warn!("resume {id}: no workspace/claude_session_id/resume-capable harness, cannot resume");
                    return Task::none();
                };
                let Some(claude_session_id) = session.claude_session_id.clone() else {
                    return Task::none(); // unreachable: resume_plan already required this
                };
                // Resuming a session that isn't running adds one, so it goes
                // through the session limit; restarting a live one doesn't.
                use ninox_ui_shared::session_queue::{admit_resume, ResumeGate};
                let gate = match admit_resume(&state.engine.store, &session, is_orch, false) {
                    Ok(gate) => gate,
                    Err(e) => {
                        tracing::warn!("resume {id}: {e}");
                        return Task::none();
                    }
                };
                if let ResumeGate::Queued { message } = &gate {
                    tracing::info!("{message}");
                    if let Some(s) = state.sessions.get_mut(&id) {
                        s.status = SessionStatus::Queued;
                    }
                    return Task::none();
                }
                let reserved = gate == ResumeGate::Admitted;
                state.clients.remove(&id);
                state.terminals.remove(&id);
                state.attach.remove(&id);
                // Same id, relaunched — the pane died and may come back onto
                // a different pty/branch state; drop the cached diff so the
                // Diff panel refetches instead of showing the pre-resume text.
                state.diffs.remove(&id);

                let ts = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64;
                let engine = state.engine.clone();
                let config = state.config.clone();
                Task::future(async move {
                    let store = engine.store.clone();
                    let attach = ninox_ui_shared::spawn_util::relaunch_in_place(
                        engine,
                        ninox_ui_shared::spawn_util::RelaunchRequest {
                            session,
                            is_orchestrator: is_orch,
                            plan,
                            claude_session_id,
                            started_at:      ts,
                            failure_status:  ninox_core::SessionStatus::Interrupted,
                        },
                        &config,
                        true,
                    )
                    .await;
                    if reserved {
                        ninox_ui_shared::session_queue::release(&store, &id);
                    }
                    match attach {
                        Ok(argv) => Message::ClientAttach { session_id: id, argv },
                        Err(cause) => Message::SessionLaunchFailed { session_id: id, action: "resume", cause },
                    }
                })
            }

            Message::ResumeAllSessions => {
                // Never the prompt-agent singleton — its displayed count
                // (`fleet_board::interrupted_count`) already excludes it, so
                // resuming it here too would silently resume N+1 sessions
                // for a button labeled "Resume all (N)", through the
                // generic relaunch path rather than
                // `ninox_ui_shared::prompt_agent`'s own restart handling.
                let ids: Vec<SessionId> = ninox_overview::board::resume_all_targets(state.sessions.values())
                    .into_iter()
                    .map(|s| s.id.clone())
                    .collect();
                Task::batch(ids.into_iter().map(|id| {
                    Task::done(Message::ResumeSession(id))
                }))
            }

            Message::RawKey { key, modifiers, text } => {
                // The help overlay sits above everything, so it takes every
                // key while open; only Esc or `?` closes it.
                if state.help_open {
                    let closes = matches!(&key, iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape))
                        || matches!(&key, iced::keyboard::Key::Character(c) if c.as_str() == "?");
                    if closes {
                        state.help_open = false;
                    }
                    return Task::none();
                }
                if let Some(nav) = history_chord(&key, modifiers, !Self::terminal_owns_keys(state)) {
                    // The chord arrives as a RawKey; the history step itself must not be recorded.
                    state.nav_auto = true;
                    return Self::apply(state, nav);
                }
                // Esc closes the spawn modal from anywhere; ⌘⏎ submits it
                // (G4) — both checked first, both can't be open in practice
                // (spawn lives in other views), but if they somehow were,
                // spawn takes precedence here just like it does in
                // `iced_view`'s stacking order. This `RawKey` branch and the
                // Name field's own `on_submit_maybe` can never both fire for
                // the same keypress: `text_input` always reports `Status::
                // Captured` for Enter while focused (any modifiers), and
                // `global_event_handler` only emits `RawKey` for `Status::
                // Ignored` events — so ⌘⏎ while the field has focus is
                // consumed there, and this branch only runs when no input
                // widget is capturing the key (round-2 review, PR #282).
                if state.spawn_modal.is_some() {
                    if matches!(key, iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape)) {
                        state.spawn_modal = None;
                    } else if modifiers.command()
                        && matches!(key, iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter))
                    {
                        return App::apply(state, Message::SpawnFormConfirm);
                    }
                    return Task::none();
                }

                // Esc closes the add-catalogue modal (Brain view's volume
                // plate `+`) at the same precedence level.
                if state.catalogue_modal.is_some() {
                    if matches!(key, iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape)) {
                        state.catalogue_modal = None;
                    }
                    return Task::none();
                }

                // Esc closes the restart-all confirm modal (Settings' Fleet
                // card) at the same precedence level.
                if state.restart_all_confirm {
                    if matches!(key, iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape)) {
                        state.restart_all_confirm = false;
                    }
                    return Task::none();
                }

                if let Some(modal) = &state.reap_modal {
                    if matches!(key, iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape)) && !modal.running {
                        state.reap_modal = None;
                    }
                    return Task::none();
                }

                // Esc backs out of the restore confirm first, then closes.
                if let Some(modal) = state.restore_modal.as_mut() {
                    if matches!(key, iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape))
                        && modal.confirm.take().is_none()
                    {
                        state.restore_modal = None;
                    }
                    return Task::none();
                }
                if state.delete_modal.is_some() {
                    if matches!(key, iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape)) {
                        return App::apply(state, Message::DeleteClose);
                    }
                    return Task::none();
                }

                // Esc closes the leftover-workspaces modal (nav chip) at the
                // same precedence level.
                if state.workspaces_modal.is_some() {
                    if matches!(key, iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape)) {
                        state.workspaces_modal = None;
                    }
                    return Task::none();
                }

                if matches!(key, iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape))
                    && Self::dismiss_on_escape(state)
                {
                    return Task::none();
                }

                // The Remotes tab's embedded `machine add` terminal has
                // focus: forward real keystrokes to its PTY (same
                // `encode_key`/paste path `View::SessionDetail`'s terminal
                // panel uses below) rather than swallowing them — the SSH
                // host-key prompt, install offer, and session picker are
                // answered by typing into this exact pane. Esc still closes
                // it (killing the subprocess), same as `RemoteAddCancel`.
                if matches!(state.view, View::Remotes)
                    && state.remotes.add.as_ref().is_some_and(|f| f.running && f.finished.is_none())
                {
                    let session_id = crate::components::remotes::MACHINE_ADD_SESSION_ID;
                    if matches!(key, iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape)) {
                        state.clients.remove(session_id);
                        state.terminals.remove(session_id);
                        state.remotes.add = None;
                        return Task::none();
                    }
                    let mode = state.terminals.get(session_id)
                        .map(|t| *t.term.mode())
                        .unwrap_or_else(alacritty_terminal::term::TermMode::empty);
                    let is_paste = matches!(&key, iced::keyboard::Key::Character(c)
                            if c.as_str().eq_ignore_ascii_case("v"))
                        && (modifiers.logo() || (modifiers.control() && modifiers.shift()));
                    if is_paste {
                        if let Ok(mut cb) = arboard::Clipboard::new() {
                            if let Ok(pasted) = cb.get_text() {
                                let payload = crate::input::encode_paste(&pasted, &mode);
                                if let Some(client) = state.clients.get(session_id) {
                                    client.write(payload);
                                }
                            }
                        }
                        return Task::none();
                    }
                    if let Some(bytes) = crate::input::encode_key(&key, modifiers, text.as_deref(), &mode) {
                        if let Some(client) = state.clients.get(session_id) {
                            client.write(bytes);
                        }
                    }
                    return Task::none();
                }

                // Esc closes the Remotes tab's add-remote modal while it's
                // a plain host/label form (not yet running) or showing the
                // frozen result of a finished add, and clears a pending
                // remove-confirm row, at the same precedence level.
                if matches!(state.view, View::Remotes)
                    && (state.remotes.add.is_some() || state.remotes.remove_confirm.is_some())
                {
                    if matches!(key, iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape)) {
                        state.clients.remove(crate::components::remotes::MACHINE_ADD_SESSION_ID);
                        state.terminals.remove(crate::components::remotes::MACHINE_ADD_SESSION_ID);
                        state.remotes.add = None;
                        state.remotes.remove_confirm = None;
                    }
                    return Task::none();
                }

                // Session page, terminal not focused: Esc unwinds the inline
                // confirm, the More menu, the composer, then goes back; `c`
                // hands the keyboard to the terminal. Focused, both keys
                // belong to the agent.
                if let View::SessionDetail { session_id, .. } = &state.view {
                    if !state.terminal_focused {
                        let sid = session_id.clone();
                        if matches!(key, iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape)) {
                            if state.session_confirm.take().is_some() {
                                return Task::none();
                            }
                            if state.session_more_open {
                                state.session_more_open = false;
                                return Task::none();
                            }
                            if state.fleet_message_draft.as_ref().is_some_and(|(target, _)| target == &sid) {
                                state.fleet_message_draft = None;
                                return Task::none();
                            }
                            return App::apply(state, Message::SessionBack);
                        }
                        if !modifiers.command() && !modifiers.control() && !modifiers.alt()
                            && matches!(&key, iced::keyboard::Key::Character(c) if c.as_str() == "c")
                        {
                            return App::apply(state, Message::TerminalFocus);
                        }
                    }
                }

                // Esc dismisses the detail panel's "Send message" composer
                // at the same precedence level as the modals above — it
                // behaves like a lightweight modal within the action row,
                // and without this check the island-selection Esc branch
                // below would clear the selection out from under it instead
                // of just cancelling the compose. Gated on the draft still
                // matching the current selection (not just `is_some()`) —
                // belt-and-suspenders with `discard_draft_for_a_different_
                // node` so an orphaned draft from a node that's no longer
                // selected can never eat the Esc meant to clear the real
                // selection (code-review finding).
                let composer_matches_selection = !Self::terminal_owns_keys(state)
                    && state.fleet_message_draft.as_ref()
                        .is_some_and(|(target, _)| Some(target.as_str()) == Self::fleet_action_target(state).as_deref());
                if composer_matches_selection {
                    if matches!(key, iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape)) {
                        state.fleet_message_draft = None;
                    }
                    return Task::none();
                }

                // Esc on an opened island is two stages (slice 4 brief):
                // the first clears the node selection (closing the detail
                // panel/action row), the second returns to the Fleet tab,
                // keeping it selected (`FleetNav::close_island`). Unlike the
                // modal checks above, only Escape itself is intercepted here
                // — this is a sub-view of the Fleet tab, not a modal, so
                // every other key (nav digits, etc.) still falls through.
                if matches!(state.view, View::Fleet) && state.fleet_nav.open.is_some()
                    && matches!(key, iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape))
                {
                    if state.fleet_nav.selected.is_some() {
                        state.fleet_nav.clear_selection();
                    } else {
                        state.fleet_nav.close_island();
                    }
                    return Task::none();
                }

                // Esc on the board (no island open) deselects the card and
                // hides its action row.
                if matches!(state.view, View::Fleet) && state.fleet_nav.open.is_none()
                    && state.fleet_nav.selected_island.is_some()
                    && matches!(key, iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape))
                {
                    state.fleet_nav.selected_island = None;
                    return Task::none();
                }

                // Esc on the Graph view goes back to Clusters (brief).
                if matches!(state.view, View::Brain) && state.brain_view.mode == BrainMode::Graph
                    && matches!(key, iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape))
                {
                    return App::apply(state, Message::BrainGraphEsc);
                }

                // Arrow keys walk links on the Graph view (brief) — the
                // neighbour of the focused node most aligned with the
                // pressed direction becomes the new focus.
                if matches!(state.view, View::Brain) && state.brain_view.mode == BrainMode::Graph {
                    use iced::keyboard::key::Named;
                    let dir = match key {
                        iced::keyboard::Key::Named(Named::ArrowRight) => Some((1.0, 0.0)),
                        iced::keyboard::Key::Named(Named::ArrowLeft) => Some((-1.0, 0.0)),
                        iced::keyboard::Key::Named(Named::ArrowUp) => Some((0.0, -1.0)),
                        iced::keyboard::Key::Named(Named::ArrowDown) => Some((0.0, 1.0)),
                        _ => None,
                    };
                    if let Some(dir) = dir {
                        if let Some(focus) = state.brain_view.graph.focus {
                            if let Some(target) = crate::components::brain_graph::walk_direction(
                                &state.brain_view.graph.nodes,
                                &state.brain_view.graph.edges,
                                focus,
                                dir,
                            ) {
                                return App::apply(state, Message::BrainGraphFocusNode(target));
                            }
                        }
                        return Task::none();
                    }
                }

                // Cmd/Ctrl+B toggles the tree rail from any view (IDE
                // convention), same as a bare `[`. `command()` maps to Cmd on
                // macOS / Ctrl elsewhere, so it never shadows tmux's Ctrl-b
                // prefix inside a terminal pane. Handled before the
                // terminal-capture path below so it works while a session
                // terminal is focused.
                if modifiers.command()
                    && matches!(&key, iced::keyboard::Key::Character(c)
                        if c.as_str().eq_ignore_ascii_case("b"))
                {
                    return App::apply(state, Message::ToggleTreeCollapsed);
                }

                // Cmd+F focuses the Brain List search field, but only while
                // the Brain tab is actually showing — unlike `/` (G1, any
                // view) and `Cmd+B`, this one names a specific field that
                // doesn't exist anywhere else yet.
                if modifiers.command()
                    && matches!(&key, iced::keyboard::Key::Character(c)
                        if c.as_str().eq_ignore_ascii_case("f"))
                    && matches!(state.view, View::Brain)
                {
                    return App::apply(state, Message::BrainFocusSearch);
                }

                if modifiers.command()
                    && matches!(&key, iced::keyboard::Key::Character(c) if c.as_str().eq_ignore_ascii_case("s"))
                {
                    return App::apply(state, Message::SaveShortcut);
                }

                // `e` edits the selected entry (List mode, nothing being typed).
                if !modifiers.command() && !modifiers.control() && !modifiers.alt()
                    && matches!(&key, iced::keyboard::Key::Character(c) if c.as_str() == "e")
                    && matches!(state.view, View::Brain)
                    && state.brain_view.mode == BrainMode::List
                    && state.brain_view.editor.is_none()
                {
                    if let Some(id) = state.brain_view.selected.clone() {
                        return App::apply(state, Message::BrainEditStart(id));
                    }
                }

                // Cmd/Ctrl+Shift+K clears the ninox prompt's context (G2),
                // same precedence tier as Cmd+B above — works from any view,
                // not just while the prompt happens to be focused. Checked
                // before plain `⌘K` below since both share the `K` key.
                if modifiers.command() && modifiers.shift()
                    && matches!(&key, iced::keyboard::Key::Character(c)
                        if c.as_str().eq_ignore_ascii_case("k"))
                {
                    return App::apply(state, Message::NinoxPromptClear);
                }

                // `⌘K` (no Shift) focuses the top-bar global search (G3),
                // any view — same precedence tier as `Cmd+B` above.
                // Shortcut map: `/` → the ninox prompt, `⌘K` → global
                // search, `⌘⇧K` → clear the ninox prompt's context, `⌘F` →
                // the Brain tab's own search.
                if modifiers.command() && !modifiers.shift()
                    && matches!(&key, iced::keyboard::Key::Character(c)
                        if c.as_str().eq_ignore_ascii_case("k"))
                {
                    return App::apply(state, Message::GlobalSearchFocus);
                }

                let terminal_capturing = state.terminal_focused && !Self::shows_transcript(state) && matches!(
                    &state.view,
                    View::SessionDetail { panel, .. } if panel.shows_terminal()
                );
                if !terminal_capturing && !modifiers.command() && !modifiers.control() && !modifiers.alt() {
                    // Global `↑`/`↓` recall the ninox prompt's history (G2)
                    // or move the search dropdown's cursor (G3), depending
                    // on which field is actually focused — every
                    // `text_input` ignores arrow keys even while focused
                    // (unlike other characters), so reaching `RawKey` at all
                    // proves nothing about which field (if any) has focus;
                    // `find_focused` is the real guard. One `operate` call,
                    // not two: each of G2's and G3's own handlers (`NinoxPrompt
                    // HistoryFocusChecked`/`GlobalSearchCursorFocusChecked`)
                    // already no-ops when the resolved id isn't theirs, so
                    // mapping to both from the same lookup is just as safe
                    // as a single dedicated one, and avoids the first match
                    // always winning if these were two separate early-return
                    // blocks.
                    if let iced::keyboard::Key::Named(named) = &key {
                        let going_up = match named {
                            iced::keyboard::key::Named::ArrowUp => Some(true),
                            iced::keyboard::key::Named::ArrowDown => Some(false),
                            _ => None,
                        };
                        if let Some(going_up) = going_up {
                            return iced::advanced::widget::operate(
                                iced::advanced::widget::operation::focusable::find_focused(),
                            )
                            .map(move |id| {
                                if id == crate::components::global_search::search_input_id().into() {
                                    Message::GlobalSearchCursorFocusChecked(going_up, id)
                                } else {
                                    Message::NinoxPromptHistoryFocusChecked(going_up, id)
                                }
                            });
                        }
                    }
                    if matches!(
                        key,
                        iced::keyboard::Key::Named(iced::keyboard::key::Named::Delete | iced::keyboard::key::Named::Backspace)
                    ) && matches!(state.view, View::Fleet)
                    {
                        let mut selected = Self::fleet_action_target(state);
                        if state.fleet_nav.open.is_none() {
                            let islands = build_fleet_islands(state);
                            let visible = state.fleet_nav.selected_island.as_ref()
                                .is_some_and(|key| islands.iter().any(|i| &i.key == key));
                            if !visible {
                                selected = None;
                            }
                        }
                        if let Some(id) = selected.filter(|id| state.sessions.contains_key(id) || state.orchestrators.iter().any(|o| &o.id == id)) {
                            return App::apply(state, Message::DeleteAsk(id));
                        }
                        return Task::none();
                    }
                    // Enter opens the selected island on the Fleet tab, same
                    // as double-clicking its card.
                    if matches!(key, iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter))
                        && matches!(state.view, View::Fleet) && state.fleet_nav.open.is_none()
                    {
                        if let Some(selected) = state.fleet_nav.selected_island.clone() {
                            let islands = build_fleet_islands(state);
                            state.fleet_nav.open_island(selected, &islands);
                        }
                        return Task::none();
                    }
                    if let iced::keyboard::Key::Character(c) = &key {
                        match c.as_str() {
                            "1" => return App::apply(state, Message::NavigateFleet),
                            "2" => return App::apply(state, Message::NavigateBoard { scope: None }),
                            "3" => return App::apply(state, Message::NavigatePlans),
                            "4" => return App::apply(state, Message::NavigateBrain),
                            "5" => return App::apply(state, Message::NavigateWorkerTypes),
                            "6" => return App::apply(state, Message::NavigateRemotes),
                            "7" => return App::apply(state, Message::NavigateIdle),
                            // Reaching here at all means no text_input is
                            // currently focused (`global_event_handler`
                            // drops anything `Status::Captured`), so `/`
                            // never steals keys from an already-focused
                            // field (G1).
                            "/" => return App::apply(state, Message::FocusNinoxPrompt),
                            "?" => return App::apply(state, Message::ToggleHelp),
                            "[" => return App::apply(state, Message::ToggleTreeCollapsed),
                            "t" => {
                                let next = match state.active_variant {
                                    ThemeVariant::Dark | ThemeVariant::Ninox => ThemeVariant::Light,
                                    ThemeVariant::Light => ThemeVariant::Dark,
                                };
                                return App::apply(state, Message::SwitchTheme(next));
                            }
                            _ => {}
                        }
                    }
                    return Task::none();
                }

                // UI zoom: Cmd (macOS) / Ctrl (elsewhere) with +/-/0, like
                // macOS Terminal.app. `modifiers.command()` is logo on macOS
                // and control on other platforms. Intercepted BEFORE the
                // terminal encode path below so the terminal panel does not
                // swallow these keys. '+' and '=' both zoom in (on most
                // layouts '+' is Shift+'='); '-' zooms out; '0' resets. The
                // level is mirrored into config and persisted across restarts.
                if modifiers.command() {
                    if let iced::keyboard::Key::Character(c) = &key {
                        let new_zoom = match c.as_str() {
                            "+" | "=" => Some((state.zoom + ZOOM_STEP).min(ZOOM_MAX)),
                            "-" | "_" => Some((state.zoom - ZOOM_STEP).max(ZOOM_MIN)),
                            "0"       => Some(1.0),
                            _         => None,
                        };
                        if let Some(z) = new_zoom {
                            // Snap to the nearest 0.1 to keep repeated steps
                            // free of binary-float drift (1.0 → 1.1 → 1.2 …).
                            let z = (z * 10.0).round() / 10.0;
                            if (z - state.zoom).abs() > f64::EPSILON {
                                // iced reports sizes in zoomed logical px but
                                // sends no resize on a zoom change, so the
                                // stored size follows the zoom here.
                                let ratio = (state.zoom / z) as f32;
                                state.window_width *= ratio;
                                state.window_height *= ratio;
                                state.zoom = z;
                                Self::persist_config(state, "zoom", |c| c.zoom = z);
                                for (sid, cols, rows) in Self::resize_terminals(state) {
                                    if let Some(client) = state.clients.get(&sid) {
                                        client.resize(cols, rows);
                                    }
                                }
                                return Self::place_traffic_lights(state);
                            }
                            return Task::none();
                        }
                    }
                }

                if let View::SessionDetail { session_id, panel } = &state.view {
                    if !state.terminal_focused || !panel.shows_terminal() {
                        return Task::none();
                    }
                    let session_id = session_id.clone();
                    // `Ctrl+]` is never sent to the agent: it's the one key
                    // that always releases the terminal, as in the TUI.
                    if modifiers.control()
                        && matches!(&key, iced::keyboard::Key::Character(c) if c.as_str() == "]")
                    {
                        return App::apply(state, Message::TerminalRelease);
                    }
                    if modifiers.logo() {
                        use iced::keyboard::key::Named;
                        let page = i32::from(state.terminal_rows.max(1));
                        let delta = match &key {
                            iced::keyboard::Key::Named(Named::ArrowUp) => Some(page),
                            iced::keyboard::Key::Named(Named::ArrowDown) => Some(-page),
                            _ => None,
                        };
                        if let Some(delta) = delta {
                            return App::apply(state, Message::ScrollTerminal { session_id, delta, local: false });
                        }
                    }
                    // Typing makes this the active client again, as tmux's
                    // `window-size latest` does.
                    Self::reclaim_pane(state, &session_id);
                    let mode = state.terminals.get(&session_id)
                        .map(|t| *t.term.mode())
                        .unwrap_or_else(alacritty_terminal::term::TermMode::empty);

                    // Paste: Cmd+V (macOS) / Ctrl+Shift+V.
                    let is_paste = matches!(&key, iced::keyboard::Key::Character(c)
                            if c.as_str().eq_ignore_ascii_case("v"))
                        && (modifiers.logo() || (modifiers.control() && modifiers.shift()));
                    if is_paste {
                        if let Ok(mut cb) = arboard::Clipboard::new() {
                            if let Ok(pasted) = cb.get_text() {
                                let payload = crate::input::encode_paste(&pasted, &mode);
                                if let Some(client) = state.clients.get(&session_id) {
                                    client.write(payload);
                                }
                            }
                        }
                        return Task::none();
                    }
                    let Some(bytes) = crate::input::encode_key(&key, modifiers, text.as_deref(), &mode)
                        else { return Task::none(); };
                    if let Some(client) = state.clients.get(&session_id) {
                        client.write(bytes);
                    }
                    return Task::none();
                }
                Task::none()
            }

            Message::TerminalWrite { session_id, bytes } => {
                if let Some(client) = state.clients.get(&session_id) {
                    client.write(bytes);
                }
                Task::none()
            }

            Message::WindowResized(size) => {
                // iced_winit converts Resized to logical pixels before emitting,
                // so size.width/height are already in logical (device-independent) pixels.
                state.window_width  = size.width;
                state.window_height = size.height;

                // Keep the authoritative terminal size up to date so new sessions
                // spawned after this resize use the correct dimensions.
                let resized = Self::resize_terminals(state);
                for (sid, cols, rows) in resized {
                    if let Some(client) = state.clients.get(&sid) {
                        client.resize(cols, rows);
                    }
                }
                // Again once a full-screen exit has finished animating: AppKit
                // re-tiles the title bar after the resize that started it.
                let settle = Task::perform(tokio::time::sleep(std::time::Duration::from_millis(700)), |()| Message::WindowFocused);
                Task::batch([Self::place_traffic_lights(state), settle])
            }

            Message::TrafficLightsPlaced(end) => {
                state.traffic_lights_end = end.map(|points| points / state.zoom as f32);
                Task::none()
            }

            Message::WindowFocused => {
                // Coming back to the window makes the open session's view
                // the active client again, as typing into it does.
                if let View::SessionDetail { session_id, .. } = &state.view {
                    let session_id = session_id.clone();
                    Self::reclaim_pane(state, &session_id);
                }
                Self::place_traffic_lights(state)
            }


            Message::SwitchTheme(variant) => {
                state.active_variant = variant;
                state.scheme = state.themes.scheme(variant);
                for term in state.terminals.values_mut() {
                    term.cache.clear();
                }
                Self::persist_config(state, "theme", |c| c.theme = variant);
                Task::none()
            }

            Message::StartDrag(target) => {
                state.drag = Some(target);
                Task::none()
            }

            Message::MouseMoved(position) => {
                match state.drag {
                    Some(DragTarget::InfoPanel) => {
                        let tree_w = if state.fleet_nav.tree_collapsed {
                            crate::components::shell::tree::RAIL_WIDTH
                        } else {
                            crate::components::shell::tree::EXPANDED_WIDTH
                        };
                        let available = state.window_width - tree_w - 10.0;
                        state.info_width = (state.window_width - position.x).clamp(200.0, available.max(200.0));
                        Self::resize_terminals(state);
                    }
                    None => {}
                }
                Task::none()
            }

            Message::MouseReleased => {
                let target = state.drag.take();
                if target.is_some() {
                    // InfoPanel width is session-local and intentionally not
                    // persisted (there's no standing per-drag-target
                    // persistence left now that the sidebar's own drag-resize
                    // is gone with the old shell).
                    let resized = Self::resize_terminals(state);
                    for (sid, cols, rows) in resized {
                        if let Some(client) = state.clients.get(&sid) {
                            client.resize(cols, rows);
                        }
                    }
                }
                Task::none()
            }

            Message::CopyToClipboard(text) => {
                if let Ok(mut cb) = arboard::Clipboard::new() {
                    let _ = cb.set_text(text);
                }
                Task::none()
            }

            // See `apply_comment_action`: edits are dropped so the cards stay
            // read-only while still being selectable and copyable.
            Message::CommentAction(comment_id, action) => {
                if let Some(content) = state.comment_editors.get_mut(&comment_id) {
                    apply_comment_action(content, action);
                }
                Task::none()
            }

            Message::PollSessions => {
                // Read before sessions: every request's asker existed before
                // it asked, so this order never shows a just-spawned asker as
                // missing (which the panel treats as stale).
                state.pending_user_requests = state.engine.store.open_user_requests(None).unwrap_or_default();
                Self::sync_notifications(state, true);
                state.ghost.plans = ninox_ui_shared::ghost_view::load(&state.engine.store, now_ms_i64());
                // Health changes land in the store from the poller with no
                // event of their own, so the Remotes tab re-reads on this
                // same 3s tick while it's showing.
                if matches!(state.view, View::Remotes) {
                    Self::refresh_remotes(state);
                }
                // Not gated on the view: the nav badge counts idle workers.
                Self::refresh_idle(state);
                let mut facts_task = Task::none();
                if let View::SessionDetail { session_id, panel } = &state.view {
                    let (sid, split) = (session_id.clone(), *panel == DetailPanel::Split);
                    Self::load_session_activity(state, &sid);
                    // Git shell-outs: every 5th tick (15s) is fresh enough
                    // to follow the agent's commits.
                    state.poll_tick = state.poll_tick.wrapping_add(1);
                    if split && state.poll_tick.is_multiple_of(5) {
                        facts_task = Self::ensure_workspace_facts(state, &sid);
                    }
                }
                let queue_view = ninox_ui_shared::session_queue::queue_view(&state.engine.store, &AppConfig::load_or_default());
                let (db_sessions, positions, limits) = match queue_view {
                    Ok(v) => (v.sessions, v.positions, v.limits),
                    Err(e) => {
                        tracing::warn!("poll sessions: {e}");
                        (state.engine.store.list_sessions().unwrap_or_default(), HashMap::new(), Vec::new())
                    }
                };
                state.queue_positions = positions;
                state.limits_headline = ninox_ui_shared::session_queue::limits_headline(&limits);
                let db_orchestrators = state.engine.store.list_orchestrators().unwrap_or_default();

                for o in db_orchestrators {
                    if !state.orchestrators.iter().any(|existing| existing.id == o.id) {
                        state.orchestrators.push(o);
                    }
                }

                // Collect IDs of orchestrators for orphan detection.
                let orch_ids: std::collections::HashSet<&str> =
                    state.orchestrators.iter().map(|o| o.id.as_str()).collect();

                // A completed (Done/Terminated) session lingers on the board
                // for a configurable grace period after finishing — see
                // `ninox_core::lifecycle::poller::Poller::sweep_retired_sessions`,
                // which is the sole place that actually deletes the store
                // record once that window elapses. This handler must not
                // race that retention logic by deleting eagerly on status
                // alone; it only mirrors the store, dropping from `state`
                // whatever has genuinely vanished from it (already purged by
                // the sweep, or by a direct user removal).
                let db_ids: std::collections::HashSet<&str> =
                    db_sessions.iter().map(|s| s.id.as_str()).collect();
                let to_clean: Vec<SessionId> = state.sessions.keys()
                    .filter(|id| !db_ids.contains(id.as_str()) && !orch_ids.contains(id.as_str()))
                    .cloned()
                    .collect();
                for id in &to_clean {
                    state.sessions.remove(id);
                    state.terminals.remove(id);
                    state.diffs.remove(id);
                }
                // Also drops a buffer a late transcript entry recreated after removal.
                state.transcripts.retain(|sid, _| state.sessions.contains_key(sid));

                // Add genuinely new sessions (spawned by `ninox spawn`, or a
                // completed session not yet tracked in `state`) — including
                // Done/Terminated ones, which stay visible until the sweep
                // above purges their store record. PTY streaming is NOT
                // started here — NavigateSession handles that on demand with
                // the correct window dimensions.
                //
                // For sessions already tracked, adopt a terminal status the
                // store has but app memory doesn't. Everything else in this
                // process learns about status changes from `Event`s, but an
                // out-of-process mutation (`ninox reap` runs in its own CLI
                // process, whose `Engine` has no subscribers here) emits into
                // the void — without this, a reaped worker would keep
                // rendering as live on the fleet board for the whole
                // retention window. Deliberately narrow: only terminal
                // statuses, and only in the live → terminal direction, so
                // this can never fight the app's own in-flight updates or
                // resurrect a session the app just respawned.
                let changed = sync_db_sessions(&mut state.sessions, db_sessions);
                if changed > 0 {
                    tracing::debug!(changed, "poll sessions: adopted store changes");
                }

                // Same reason for the PR rows themselves: PR/CI/review data a
                // daemon wrote since the last tick. Comments are the bulky
                // read, so they ride every 5th tick (15s).
                for pr in state.engine.store.list_prs().unwrap_or_default() {
                    state.prs.insert(pr.id, pr);
                }
                for ci in state.engine.store.list_ci_statuses().unwrap_or_default() {
                    state.ci_status.insert(ci.pr_id, ci);
                }
                state.session_prs = ninox_core::session_prs::for_sessions(&state.engine.store, state.sessions.values());
                state.github_budget = state.engine.store.get_github_budget().ok().flatten();
                state.store_poll_tick = state.store_poll_tick.wrapping_add(1);
                if state.store_poll_tick.is_multiple_of(5) {
                    for comment in state.engine.store.list_comments().unwrap_or_default() {
                        let thread = state.review_threads.entry(comment.pr_id).or_default();
                        if !thread.iter().any(|c| c.id == comment.id) {
                            thread.push(comment);
                            thread.sort_by_key(|c| c.created_at);
                        }
                    }
                }

                // Refresh the diff for whatever session is on the Diff panel
                // right now — a live session's diff changes as the worker
                // commits, so this is the "keeps updating" tick for it.
                // Same idea for the Plan panel: no file-watcher exists in
                // this codebase, so this 3s tick doubles as the plan doc's
                // freshness bar too (see docs/superpowers/specs/2026-08-26-
                // orchestrator-plan-tracking-design.md).
                let panel_task = match &state.view {
                    View::SessionDetail { session_id, panel: DetailPanel::Diff } => {
                        Self::ensure_diff(state, &session_id.clone())
                    }
                    View::SessionDetail { session_id, panel: DetailPanel::Plan } => {
                        Self::ensure_plan(state, &session_id.clone())
                    }
                    View::Plans => {
                        state.orch_plans.reload(&state.engine.store);
                        match state.orch_plans.selected.clone() {
                            Some(id) => Self::ensure_plan(state, &id),
                            None => Task::none(),
                        }
                    }
                    _ => Task::none(),
                };

                // A full cross-repo workspaces scan shells out `git status`/
                // `rev-list` per worktree — too heavy to repeat on every 3s
                // tick, so it's throttled to roughly once every 30s here
                // rather than given its own subscription.
                const WORKSPACES_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
                let due = state.workspaces_last_refresh
                    .is_none_or(|last| last.elapsed() >= WORKSPACES_REFRESH_INTERVAL);
                let mut tasks = vec![panel_task, Self::poll_ninox_prompt(state), facts_task];
                if due {
                    state.workspaces_last_refresh = Some(Instant::now());
                    tasks.push(Self::workspaces_rows_task(state));
                }
                Task::batch(tasks)
            }

            Message::NavigateBrain => {
                state.global_search.dismiss();
                if !state.brain_view.loaded {
                    // `None`: the GUI brain panel is keyword-only for now — wiring it to the
                    // same embedder the server constructs at startup is a natural follow-up,
                    // deliberately out of scope for the CLI/HTTP-focused semantic search spec.
                    match state.brain.query("", None, QueryFilters::default()) {
                        Ok(entries) => {
                            state.brain_view.entries = entries;
                            state.brain_view.loaded = true;
                            state.brain_view.last_indexed = Some(std::time::Instant::now());
                            Self::refresh_brain_edges(state);
                            Self::refresh_brain_graph(state);
                        }
                        Err(e) => tracing::error!("brain query: {e}"),
                    }
                }
                state.view = View::Brain;
                // Opening the view freshens the ACTIVE scope in the
                // background — switch and reindex got this in the remote-
                // brains follow-up, but the default catalogue (never
                // switched to) otherwise only freshens when something hits
                // the in-process server, so its first Brain visit showed
                // the local snapshot.
                Self::freshen_active_brain_task(state)
            }

            Message::NavigateSettings => {
                state.global_search.dismiss();
                // Reopening shows what's saved now, not text left from last time.
                state.settings.drafts.clear();
                state.settings.errors.clear();
                state.view = View::Settings;
                // Kick model discovery for the worker default's harness so
                // the Workers card's picker has live options when it opens.
                let models_task = Self::ensure_models(state, &state.config.worker.harness.clone());
                let orch_models_task = Self::ensure_models(state, &state.config.orchestrator.harness.clone());
                let version_task = Self::ensure_version_check(state);
                Task::batch(vec![models_task, orch_models_task, version_task])
            }

            Message::SettingsToggleHarness(name) => {
                Self::persist_config(state, "toggle harness", |c| c.toggle_harness(&name));
                Task::none()
            }

            Message::SettingsWorkerHarness(h) => {
                // pick_list fires on re-selecting the current value — don't
                // wipe (and re-save) the model for a no-op selection.
                if !Self::persist_config(state, "worker harness", |c| c.worker.set_harness(&h)) {
                    return Task::none();
                }
                state.settings.worker_custom = None;
                Self::ensure_models(state, &h)
            }

            Message::SettingsEditor(editor) => {
                Self::persist_config(state, "editor", |c| c.editor = editor);
                Task::none()
            }

            Message::SettingsWorkerModel(v) => {
                if v == ninox_ui_shared::models::CUSTOM_SENTINEL {
                    state.settings.worker_custom =
                        Some(state.config.worker.model.clone().unwrap_or_default());
                    return Task::none();
                }
                state.settings.worker_custom = None;
                Self::persist_config(state, "worker model", |c| c.worker.model = Some(v.clone()));
                Task::none()
            }

            Message::SettingsWorkerCustomModel(v) => {
                state.settings.worker_custom = Some(v);
                Task::none()
            }

            Message::SettingsWorkerCustomCommit => {
                if let Some(v) = state.settings.worker_custom.take() {
                    let model = Some(v.trim().to_string()).filter(|t| !t.is_empty());
                    Self::persist_config(state, "worker model", |c| c.worker.model = model.clone());
                }
                Task::none()
            }

            // Session limits go through the shared settings form, so they parse
            // (a number, `unlimited` or `0`) and refuse exactly as the TUI does.
            Message::SettingsLimitInput(machine, v) => {
                App::apply(state, Message::SettingsDraft { id: ninox_ui_shared::settings_form::FieldId::SessionLimit(machine), text: v })
            }

            Message::SettingsLimitCommit(machine) => {
                App::apply(state, Message::SettingsCommit(ninox_ui_shared::settings_form::FieldId::SessionLimit(machine)))
            }

            Message::SettingsLimitToggleUnlimited(machine) => {
                use ninox_ui_shared::settings_form::FieldId;
                state.settings.take_draft(&FieldId::SessionLimit(machine.clone()));
                state.settings.set_result(FieldId::SessionLimit(machine.clone()), Ok(()));
                let now_unlimited = state.config.limits.limit_for(machine.as_deref()).is_none();
                let text = if now_unlimited { "off" } else { "on" };
                App::apply(state, Message::SettingsSet { id: FieldId::SessionLimitUnlimited(machine), text: text.into() })
            }

            Message::SettingsSetSendMechanism(mechanism) => {
                Self::persist_config(state, "send mechanism", |c| c.set_send_mechanism(mechanism));
                Task::none()
            }

            Message::SettingsSetRuntimeBackend(backend) => {
                Self::persist_config(state, "session runtime", |c| c.runtime.backend = backend);
                if backend != ninox_core::runtime::Backend::Ptyd {
                    return Task::none();
                }
                // Start the host now so the first new session doesn't pay for it.
                Task::future(async move {
                    if let Ok(exe) = std::env::current_exe() {
                        if let Err(e) = ninox_core::runtime::ensure_ptyd_host(&exe).await {
                            tracing::warn!("could not start the ptyd host: {e}");
                        }
                    }
                    Message::Noop
                })
            }

            Message::SettingsTogglePrWatch => {
                Self::persist_config(state, "PR watch", |c| c.pr_watch.enabled = !c.pr_watch.enabled);
                Task::none()
            }

            Message::SettingsSection(section) => {
                state.settings.section = section;
                Task::none()
            }

            Message::SettingsDraft { id, text } => {
                // Typed back to the saved value: no draft, so a later change
                // made elsewhere is shown rather than masked. The custom-model
                // box is the exception — its draft is what keeps it open.
                let saved = ninox_ui_shared::settings_form::fields(&state.config)
                    .into_iter()
                    .find(|f| f.id == id)
                    .map(|f| f.value);
                if id != ninox_ui_shared::settings_form::FieldId::OrchModel && saved.as_deref() == Some(text.as_str()) {
                    state.settings.take_draft(&id);
                    state.settings.set_result(id, Ok(()));
                } else {
                    state.settings.set_draft(id, text);
                }
                Task::none()
            }

            Message::SettingsRevert(id) => {
                state.settings.take_draft(&id);
                state.settings.set_result(id, Ok(()));
                Task::none()
            }

            Message::SettingsCommit(id) => {
                let text = state.settings.take_draft(&id).unwrap_or_default();
                App::apply(state, Message::SettingsSet { id, text })
            }

            Message::SettingsSet { id, text } => {
                use ninox_ui_shared::settings_form::{apply, Change, FieldId};
                let change = Change::Set { id: id.clone(), text: text.clone() };
                // Read → apply → write against the file, so a change made
                // elsewhere since this panel loaded is judged on what's on disk.
                // A refused value leaves the file untouched.
                let result = match ninox_core::config::AppConfig::update(|c| apply(c, &change)) {
                    Ok((saved, r)) => {
                        state.config = saved;
                        r.map(|_| ())
                    }
                    Err(e) => Err(format!("could not save {}: {e}", ninox_core::config::AppConfig::config_path().display())),
                };
                if result.is_ok() {
                    state.settings.take_draft(&id);
                } else {
                    state.settings.set_draft(id.clone(), text);
                }
                state.settings.set_result(id.clone(), result);
                if id == FieldId::OrchHarness {
                    // Switching the harness clears its model; drop a half-typed custom id too.
                    state.settings.take_draft(&FieldId::OrchModel);
                    return Self::ensure_models(state, &state.config.orchestrator.harness.clone());
                }
                Task::none()
            }

            Message::BrainSelectEntry(id) => {
                if state.brain_view.delete_confirm.as_deref().is_some_and(|c| c != id) {
                    state.brain_view.delete_confirm = None;
                }
                if let Some(e) = state.brain_view.entries.iter().find(|e| e.id == id) {
                    let body = crate::components::brain_panel::strip_leading_h1(&e.body);
                    let body = crate::components::brain_panel::insert_soft_breaks(&body);
                    state.brain_view.markdown = iced::widget::markdown::parse(
                        &crate::components::markdown_prepass::prepare(
                            &crate::components::brain_panel::preprocess_wikilinks_resolving(&body, |t| {
                                crate::components::brain_panel::resolve_link(&state.brain_view.entries, t, Some(&e.source)).is_some()
                            }),
                        ),
                    ).collect();
                    // Retarget the Kinds-column filter only when it would
                    // otherwise hide the selection (a wikilink, a backlink
                    // chip, or a future Graph double-click landing on an
                    // entry outside the active kind). "All entries"
                    // (`None`) already shows every kind, and a selection
                    // that already matches the active kind needs no
                    // change — narrowing either of those on every click
                    // would silently yank the user out of "All entries"
                    // just for reading a row (code-review finding).
                    let kind = ninox_core::brain::normalize_kind(&e.entry_type);
                    if state.brain_view.kind_filter.as_deref().is_some_and(|k| k != kind) {
                        state.brain_view.kind_filter = Some(kind);
                    }
                    state.brain_view.mode = BrainMode::List;
                }
                state.brain_view.selected = Some(id);
                Self::refresh_selection_graph(state);
                Task::none()
            }

            Message::BrainFilterQuery(query) => {
                state.brain_view.filter = query;
                Task::none()
            }

            Message::BrainReindex => {
                // Freshen from the remote BEFORE rebuilding, matching the
                // CLI's `ninox brain index` (sync → rebuild) and the server's
                // POST /index — previously this button rebuilt local-only, so
                // teammates' pushes stayed invisible to the GUI. Runs as a
                // Task so the network wait never blocks the UI thread;
                // completion (and the entry reload) lands via
                // `BrainReindexed`. A brain with no remote skips straight to
                // the rebuild, behaving exactly as before. "All sources"
                // does this for every enabled catalogue before the one
                // summed `rebuild_all`.
                let brain = state.brain.clone();
                let config = state.config.clone();
                let scope_id = state.brain.identity();
                let sync_paths: Vec<std::path::PathBuf> = match &state.brain {
                    BrainScope::Single(b) => vec![b.path().to_path_buf()],
                    BrainScope::All(_) => {
                        state.catalogues.iter().filter(|c| c.enabled).map(|c| c.path.clone()).collect()
                    }
                };
                Task::future(async move {
                    for path in &sync_paths {
                        // First open of a config-declared remote catalogue
                        // materializes its .sync.toml (a local-fs write only) —
                        // the CLI does this in run_brain; without it a freshly
                        // cloned catalogue never learns its remote.
                        if let Err(e) = ninox_core::brain_sync::ensure_sync_toml(&config, path) {
                            tracing::warn!("brain: failed to materialize .sync.toml: {e}");
                        }
                        // Full sync (pull → resolve → push), not just
                        // pull_if_stale: reindex is the GUI's explicit "get in
                        // sync" gesture, same contract as the CLI Index action.
                        // Any remote failure degrades to local-only reindexing.
                        match ninox_core::brain_sync::BrainSync::for_brain(path).await {
                            Ok(None) => {}
                            Ok(Some(sync)) => {
                                if let Err(e) = sync.sync().await {
                                    tracing::warn!("brain sync failed (continuing with local reindex): {e}");
                                }
                            }
                            Err(e) => tracing::warn!("brain remote unavailable (continuing local-only): {e}"),
                        }
                    }
                    // `None`: the GUI brain panel is keyword-only for now — wiring it to the
                    // same embedder the server constructs at startup is a natural follow-up,
                    // deliberately out of scope for the CLI/HTTP-focused semantic search spec.
                    let result = brain.rebuild(None).map(|s| s.indexed).map_err(|e| e.to_string());
                    Message::BrainReindexed { scope_id, result }
                })
            }

            Message::BrainReindexed { scope_id, result } => {
                // Race guard: the user may have switched catalogues/scope
                // while the sync + rebuild ran — the rebuilt index belongs
                // to the old scope, so leave the current view alone.
                if state.brain.identity() != scope_id {
                    return Task::none();
                }
                match result {
                    Ok(indexed) => {
                        tracing::info!("brain reindexed: {indexed} entries");
                        Self::reload_brain_entries(state);
                    }
                    Err(e) => tracing::error!("brain rebuild: {e}"),
                }
                Task::none()
            }

            Message::BrainSetMode(m) => {
                state.brain_view.mode = m;
                state.brain_view.editor = None;
                state.brain_view.delete_confirm = None;
                if m == BrainMode::Graph {
                    // The List selection carries over as the graph's focus.
                    if let Some(index) = state.brain_view.selected.as_ref().and_then(|id| state.brain_view.entries.iter().position(|e| &e.id == id)) {
                        state.brain_view.graph.focus = Some(index);
                        state.brain_view.graph.graph_selected = Some(index);
                    }
                    Self::ensure_graph_focus(state);
                    Self::refresh_brain_graph(state);
                    Self::fit_brain_graph(state);
                }
                if m == BrainMode::Sources {
                    return App::apply(state, Message::SourcesRefreshStatus);
                }
                Task::none()
            }

            Message::BrainShowInGraph(id) => {
                state.brain_view.mode = BrainMode::Graph;
                if let Some(index) = state.brain_view.entries.iter().position(|e| e.id == id) {
                    let g = &mut state.brain_view.graph;
                    g.scope = crate::components::brain_graph::GraphScope::Neighbourhood;
                    g.focus = Some(index);
                    g.graph_selected = Some(index);
                    Self::refresh_brain_graph(state);
                    Self::fit_brain_graph(state);
                }
                Task::none()
            }

            Message::BrainSetKindFilter(kind) => {
                state.brain_view.kind_filter = kind;
                // The Repo/Tag chip lists are scoped to the active kind
                // (`brain_list::kinds_column`) and re-derive on every
                // render, so a repo/tag chosen under the old kind may not
                // even be offered under the new one — reset both rather
                // than risk a stale filter silently emptying the list
                // (code-review finding).
                state.brain_view.repo_filter = None;
                state.brain_view.tag_filter = None;
                Task::none()
            }

            Message::BrainSetRepoFilter(repo) => {
                state.brain_view.repo_filter =
                    if state.brain_view.repo_filter == repo { None } else { repo };
                Task::none()
            }

            Message::BrainSetTagFilter(tag) => {
                state.brain_view.tag_filter =
                    if state.brain_view.tag_filter == tag { None } else { tag };
                Task::none()
            }

            Message::BrainSetSort(sort) => {
                state.brain_view.sort = sort;
                Task::none()
            }

            Message::BrainGraphSetScope(scope) => {
                state.brain_view.graph.scope = scope;
                Self::ensure_graph_focus(state);
                Self::refresh_brain_graph(state);
                Self::fit_brain_graph(state);
                Task::none()
            }

            Message::BrainGraphSetHops(hops) => {
                state.brain_view.graph.hops = hops.clamp(1, 3);
                Self::refresh_brain_graph(state);
                Task::none()
            }

            Message::BrainGraphFocusNode(entry) => {
                state.brain_view.graph.graph_selected = Some(entry);
                if state.brain_view.graph.scope == crate::components::brain_graph::GraphScope::Neighbourhood {
                    let first_focus = state.brain_view.graph.focus.is_none();
                    state.brain_view.graph.focus = Some(entry);
                    Self::refresh_brain_graph(state);
                    if first_focus {
                        Self::fit_brain_graph(state);
                    }
                }
                Task::none()
            }

            Message::BrainGraphColorBy(by) => {
                state.brain_view.graph.color_by = by;
                state.brain_view.graph.cache.clear();
                Task::none()
            }

            Message::BrainGraphToggleMinimap => {
                Self::persist_config(state, "brain minimap", |c| c.brain_minimap_collapsed = !c.brain_minimap_collapsed);
                // The free canvas area changed: refit into it.
                Self::fit_brain_graph(state);
                Task::none()
            }

            Message::BrainGraphSearch(text) => {
                state.brain_view.graph.search = text;
                Task::none()
            }

            Message::BrainGraphOpenInList(entry) => {
                if let Some(id) = state.brain_view.entries.get(entry).map(|e| e.id.clone()) {
                    return App::apply(state, Message::BrainSelectEntry(id));
                }
                Task::none()
            }

            Message::BrainGraphOpenCluster(repo) => {
                state.brain_view.graph.scope = crate::components::brain_graph::GraphScope::Everything;
                state.brain_view.graph.repo_filter = Some(repo);
                state.brain_view.graph.focus = None;
                Self::refresh_brain_graph(state);
                Self::fit_brain_graph(state);
                Task::none()
            }

            Message::BrainGraphHover(entry) => {
                if state.brain_view.graph.hovered != entry {
                    state.brain_view.graph.hovered = entry;
                    // Hover changes which edges are lit and which labels show.
                    state.brain_view.graph.cache.clear();
                }
                Task::none()
            }

            Message::BrainGraphHoverCluster(label) => {
                state.brain_view.graph.hovered_cluster = label;
                Task::none()
            }

            Message::BrainGraphToggleKind(kind) => {
                let kinds = &mut state.brain_view.graph.visible_kinds;
                if !kinds.remove(&kind) {
                    kinds.insert(kind);
                }
                Self::refresh_brain_graph(state);
                Task::none()
            }

            Message::BrainGraphSetRepoFilter(repo) => {
                state.brain_view.graph.repo_filter =
                    if state.brain_view.graph.repo_filter == repo { None } else { repo };
                Self::refresh_brain_graph(state);
                Task::none()
            }

            Message::BrainGraphToggleHideUnlinked => {
                state.brain_view.graph.hide_unlinked = !state.brain_view.graph.hide_unlinked;
                Self::refresh_brain_graph(state);
                Task::none()
            }

            Message::BrainGraphToggleFreezeLayout => {
                state.brain_view.graph.freeze_layout = !state.brain_view.graph.freeze_layout;
                Self::refresh_brain_graph(state);
                Task::none()
            }

            Message::BrainGraphZoom(delta) => {
                let g = &mut state.brain_view.graph;
                let new_zoom = (g.zoom + delta).clamp(0.1, 5.0);
                if (new_zoom - g.zoom).abs() > f32::EPSILON {
                    g.zoom = new_zoom;
                    g.cache.clear();
                }
                Task::none()
            }

            Message::BrainGraphPan(dx, dy) => {
                let g = &mut state.brain_view.graph;
                g.pan_x += dx;
                g.pan_y += dy;
                g.cache.clear();
                Task::none()
            }

            Message::BrainGraphLaidOut { key, nodes } => {
                let g = &mut state.brain_view.graph;
                if g.pending_layout != Some(key) {
                    return Task::none(); // superseded by a newer graph
                }
                g.pending_layout = None;
                // Jobs only run for 200+ nodes: an empty result is a failed
                // layout, which must not be cached as that graph's answer.
                if !nodes.is_empty() {
                    g.remember_layout(key, nodes.clone());
                }
                Self::show_graph_layout(g, nodes, &state.brain_view.edges);
                if std::mem::take(&mut g.fit_when_laid_out) {
                    Self::fit_brain_graph(state);
                }
                state.brain_view.graph.cache.clear();
                Task::none()
            }

            Message::BrainGraphFit => {
                Self::fit_brain_graph(state);
                Task::none()
            }

            Message::BrainGraphEsc => {
                state.brain_view.graph.scope = crate::components::brain_graph::GraphScope::Clusters;
                state.brain_view.graph.focus = None;
                state.brain_view.graph.repo_filter = None;
                Self::refresh_brain_graph(state);
                Self::fit_brain_graph(state);
                Task::none()
            }

            Message::SourcesRefreshStatus => {
                for c in state.catalogues.clone() {
                    let loaded = ninox_ui_shared::brain_sources::load_status(&c);
                    let entry = state.source_status.entry(c.path.clone()).or_default();
                    entry.entry_count = loaded.entry_count;
                    entry.remote_status = loaded.remote_status;
                }
                Task::none()
            }

            Message::SourcesOpenAdd => {
                state.sources_edit = Some(ninox_ui_shared::brain_sources::SourceEditForm::default());
                Task::none()
            }

            Message::SourcesOpenEdit(name) => {
                if let Some(c) = state.catalogues.iter().find(|c| c.name == name) {
                    state.sources_edit = Some(ninox_ui_shared::brain_sources::form_from_catalogue(c));
                }
                Task::none()
            }

            Message::SourcesFormName(v) => {
                if let Some(f) = &mut state.sources_edit {
                    f.name = v;
                    f.error = None;
                }
                Task::none()
            }

            Message::SourcesFormSetType(t) => {
                if let Some(f) = &mut state.sources_edit {
                    f.source_type = t;
                    f.error = None;
                }
                Task::none()
            }

            Message::SourcesFormLocation(v) => {
                if let Some(f) = &mut state.sources_edit {
                    f.location = ninox_ui_shared::brain_sources::apply_masked_edit(&f.location, &v);
                    f.error = None;
                }
                Task::none()
            }

            Message::SourcesFormRegion(v) => {
                if let Some(f) = &mut state.sources_edit {
                    f.region = v;
                    f.error = None;
                }
                Task::none()
            }

            Message::SourcesFormEndpoint(v) => {
                if let Some(f) = &mut state.sources_edit {
                    f.endpoint = ninox_ui_shared::brain_sources::apply_masked_edit(&f.endpoint, &v);
                    f.error = None;
                }
                Task::none()
            }

            Message::SourcesFormCachePath(v) => {
                if let Some(f) = &mut state.sources_edit {
                    f.cache_path = v;
                    f.error = None;
                }
                Task::none()
            }

            Message::SourcesFormCadence(v) => {
                if let Some(f) = &mut state.sources_edit {
                    f.cadence = v;
                }
                Task::none()
            }

            Message::SourcesFormAccess(v) => {
                if let Some(f) = &mut state.sources_edit {
                    f.access = v;
                }
                Task::none()
            }

            Message::SourcesFormCancel => {
                state.sources_edit = None;
                Task::none()
            }

            Message::SourcesFormSave => {
                let Some(form) = state.sources_edit.clone() else { return Task::none() };

                let catalogue = match ninox_ui_shared::brain_sources::prepare_save(&form, &state.config.catalogue_options()) {
                    Ok(c) => c,
                    Err(e) => {
                        if let Some(f) = &mut state.sources_edit {
                            f.error = Some(e);
                        }
                        return Task::none();
                    }
                };

                let editing = form.editing.clone();
                Self::persist_config(state, "save source", |c| {
                    ninox_ui_shared::brain_sources::apply_save(c, editing.as_deref(), &catalogue)
                });
                state.catalogues = state.config.catalogue_options();
                state.sources_edit = None;
                App::apply(state, Message::SourcesRefreshStatus)
            }

            Message::SourcesToggleEnabled(name) => {
                Self::persist_config(state, "toggle source enabled", |c| ninox_ui_shared::brain_sources::apply_toggle(c, &name));
                state.catalogues = state.config.catalogue_options();
                Task::none()
            }

            Message::SourcesRemove(name) => {
                // Pops the config reference only — never touches the
                // remote, and (narrowest reading of that same brief line)
                // leaves any local `.sync.toml`/cache files alone too.
                let removed_path = state.catalogues.iter().find(|c| c.name == name).map(|c| c.path.clone());
                Self::persist_config(state, "remove source", |c| ninox_ui_shared::brain_sources::apply_remove(c, &name));
                state.catalogues = state.config.catalogue_options();
                if let Some(path) = removed_path {
                    state.source_status.remove(&path);
                }
                Task::none()
            }

            Message::SourcesSyncNow(name) => {
                let Some(catalogue) = state.catalogues.iter().find(|c| c.name == name).cloned() else {
                    return Task::none();
                };
                state.source_status.entry(catalogue.path.clone()).or_default().syncing = true;

                let config = state.config.clone();
                let path = catalogue.path.clone();
                Task::future(async move {
                    let result = ninox_ui_shared::brain_sources::sync_source(&config, &path).await;
                    Message::SourcesSynced { name, path, result }
                })
            }

            Message::SourcesSyncAll => {
                let tasks: Vec<Task<Message>> =
                    state.catalogues.iter().map(|c| c.name.clone()).collect::<Vec<_>>().into_iter().map(|name| App::apply(state, Message::SourcesSyncNow(name))).collect();
                Task::batch(tasks)
            }

            Message::SourcesSynced { name, path, result } => {
                let status = state.source_status.entry(path.clone()).or_default();
                status.syncing = false;
                match result {
                    Ok(report) => {
                        status.last_error = None;
                        state.source_sync_log.push_front(ninox_ui_shared::brain_sources::sync_log_line(&name, &report));
                        const MAX_LOG_LINES: usize = 50;
                        state.source_sync_log.truncate(MAX_LOG_LINES);
                        // Rebuild just this one catalogue's local index if
                        // it's part of the currently open scope, same as
                        // BrainReindexed — not the whole scope, which in
                        // "All sources" mode would otherwise mean a full
                        // rebuild of every other catalogue on every sync.
                        if state.brain.includes(&path) {
                            if let Err(e) = state.brain.rebuild_source(&path, None) {
                                tracing::error!("brain rebuild after source sync: {e}");
                            } else {
                                Self::reload_brain_entries(state);
                            }
                        }
                    }
                    Err(e) => {
                        status.last_error = Some(e);
                    }
                }
                App::apply(state, Message::SourcesRefreshStatus)
            }

            Message::BrainEditStart(id) => {
                let probe = ninox_ui_shared::brain_edit::Job::Reindex { id: id.clone() };
                let root = Self::brain_job_paths(state, &probe).remove(0);
                let raw = ninox_ui_shared::brain_edit::unqualify_job_id(probe, state.active_catalogue);
                state.brain_view.delete_confirm = None;
                state.brain_view.notice = None;
                match crate::components::brain_editor::BrainEditor::open_existing(&root, raw.id(), id) {
                    Ok(editor) => {
                        state.brain_view.editor = Some(editor);
                        iced::widget::text_input::focus(crate::components::brain_edit_view::name_input_id())
                    }
                    Err(e) => {
                        state.brain_view.notice = Some(Err(e));
                        Task::none()
                    }
                }
            }

            Message::BrainNewStart(kind) => {
                if state.active_catalogue.is_none() {
                    state.brain_view.notice =
                        Some(Err("pick a catalogue to add an entry to — All sources has no single home for it".into()));
                    return Task::none();
                }
                state.brain_view.delete_confirm = None;
                state.brain_view.notice = None;
                let root = Self::brain_job_paths(state, &ninox_ui_shared::brain_edit::Job::ReindexAll).remove(0);
                state.brain_view.editor = Some(crate::components::brain_editor::BrainEditor::new_entry(kind, root));
                iced::widget::text_input::focus(crate::components::brain_edit_view::name_input_id())
            }

            Message::BrainEditKind(kind) => {
                if let Some(ed) = state.brain_view.editor.as_mut() {
                    if let crate::components::brain_editor::EditTarget::New { kind: k } = &mut ed.target {
                        *k = kind;
                    }
                }
                Task::none()
            }

            Message::SaveShortcut => {
                if matches!(state.view, View::Brain) && state.brain_view.editor.is_some() && !Self::modal_open(state) {
                    return App::apply(state, Message::BrainEditSave);
                }
                Task::none()
            }

            Message::BrainEditName(name) => {
                if let Some(ed) = state.brain_view.editor.as_mut() {
                    ed.name = name;
                }
                Task::none()
            }

            Message::BrainEditTagDraft(draft) => {
                if let Some(ed) = state.brain_view.editor.as_mut() {
                    ed.tag_draft = draft;
                }
                Task::none()
            }

            Message::BrainEditTagAdd => {
                if let Some(ed) = state.brain_view.editor.as_mut() {
                    let draft = std::mem::take(&mut ed.tag_draft);
                    crate::components::brain_editor::BrainEditor::add_values(&mut ed.tags, &draft);
                }
                Task::none()
            }

            Message::BrainEditTagRemove(tag) => {
                if let Some(ed) = state.brain_view.editor.as_mut() {
                    ed.tags.retain(|t| *t != tag);
                }
                Task::none()
            }

            Message::BrainEditRepoDraft(draft) => {
                if let Some(ed) = state.brain_view.editor.as_mut() {
                    ed.repo_draft = draft;
                }
                Task::none()
            }

            Message::BrainEditRepoAdd => {
                if let Some(ed) = state.brain_view.editor.as_mut() {
                    let draft = std::mem::take(&mut ed.repo_draft);
                    crate::components::brain_editor::BrainEditor::add_values(&mut ed.repos, &draft);
                }
                Task::none()
            }

            Message::BrainEditRepoRemove(repo) => {
                if let Some(ed) = state.brain_view.editor.as_mut() {
                    ed.repos.retain(|r| *r != repo);
                }
                Task::none()
            }

            Message::BrainEditBody(action) => {
                if let Some(ed) = state.brain_view.editor.as_mut() {
                    ed.body.perform(action);
                }
                Task::none()
            }

            Message::BrainEditSave => {
                if state.brain_view.job_running.is_some() {
                    return Task::none();
                }
                let Some(ed) = state.brain_view.editor.as_ref() else { return Task::none() };
                match ed.to_job() {
                    Ok(job) => {
                        let select = Some(match &ed.target {
                            crate::components::brain_editor::EditTarget::Existing { id, .. } => id.clone(),
                            crate::components::brain_editor::EditTarget::New { .. } => job.id().to_string(),
                        });
                        let (root, serial) = (ed.root.clone(), ed.serial);
                        Self::start_brain_job_at(state, vec![root], job, select, Some(serial))
                    }
                    Err(e) => {
                        if let Some(ed) = state.brain_view.editor.as_mut() {
                            ed.error = Some(e);
                        }
                        Task::none()
                    }
                }
            }

            Message::BrainEditCancel => {
                state.brain_view.editor = None;
                Task::none()
            }

            Message::GhostSelect { plan_id, node_id } => {
                if state.ghost.selected.as_ref().is_none_or(|s| s.plan_id != plan_id) {
                    state.ghost.confirm = None;
                }
                state.orch_plans.selected = None;
                state.ghost.selected = Some(crate::components::ghost_plans::GhostSelection { plan_id, node_id });
                // The ghost detail panel replaces the real one, so nothing
                // real may stay selected for Fleet keys to act on unseen.
                if matches!(state.view, View::Fleet) {
                    state.fleet_nav.clear_selection();
                    state.fleet_message_draft = None;
                    if state.fleet_nav.open.is_none() {
                        state.fleet_nav.selected_island = None;
                    }
                }
                Task::none()
            }

            Message::GhostAcceptRequest(plan_id) => {
                state.ghost.confirm = Some(crate::components::ghost_plans::GhostConfirm::Accept(plan_id));
                Task::none()
            }

            Message::GhostDiscardRequest(plan_id) => {
                state.ghost.confirm = Some(crate::components::ghost_plans::GhostConfirm::Discard(plan_id));
                Task::none()
            }

            Message::GhostConfirmCancel => {
                state.ghost.confirm = None;
                Task::none()
            }

            Message::GhostOpenInPlans(plan_id) => {
                state.global_search.dismiss();
                state.view = View::Plans;
                state.clients.clear();
                state.attach.clear();
                state.orch_plans.selected = None;
                state.orch_plans.reload(&state.engine.store);
                state.ghost.selected = Some(crate::components::ghost_plans::GhostSelection { plan_id, node_id: None });
                Task::none()
            }

            Message::GhostConfirmed => {
                use crate::components::ghost_plans::GhostConfirm;
                if state.ghost.busy.is_some() {
                    return Task::none();
                }
                let Some(confirm) = state.ghost.confirm.take() else { return Task::none() };
                let (plan_id, accept) = match confirm {
                    GhostConfirm::Accept(id) => (id, true),
                    GhostConfirm::Discard(id) => (id, false),
                };
                state.ghost.busy = Some(plan_id.clone());
                state.ghost.notice = None;
                let store = state.engine.store.clone();
                let config = AppConfig::load_or_default();
                Task::future(async move {
                    // The GUI is the user: no managed caller.
                    let result = if accept {
                        ninox_ui_shared::ghost_accept::accept(&store, &config, &plan_id, None, None, now_ms_i64()).await
                    } else {
                        ninox_ui_shared::ghost_accept::discard(&store, &plan_id, None, None, now_ms_i64())
                    };
                    let result = result
                        .map(|r| format!("{} {}: {}", if accept { "Accepted" } else { "Discarded" }, r.plan_id, r.summary()))
                        .map_err(|e| format!("{e:#}"));
                    Message::GhostDecisionDone { plan_id, result }
                })
            }

            Message::GhostDecisionDone { plan_id, result } => {
                if state.ghost.busy.as_deref() == Some(plan_id.as_str()) {
                    state.ghost.busy = None;
                }
                state.ghost.notice = Some(match result {
                    Ok(msg) => msg,
                    Err(e) => format!("{plan_id}: {e}"),
                });
                state.ghost.plans = ninox_ui_shared::ghost_view::load(&state.engine.store, now_ms_i64());
                Task::none()
            }

            Message::BrainDeleteRequest(id) => {
                state.brain_view.notice = None;
                state.brain_view.delete_confirm = Some(id);
                Task::none()
            }

            Message::BrainDeleteCancel => {
                state.brain_view.delete_confirm = None;
                Task::none()
            }

            Message::BrainDeleteConfirm => {
                if state.brain_view.job_running.is_some() {
                    return Task::none();
                }
                let Some(id) = state.brain_view.delete_confirm.take() else { return Task::none() };
                Self::start_brain_job(state, ninox_ui_shared::brain_edit::Job::Delete { id }, None)
            }

            Message::BrainJobDone { job, select, editor, result } => {
                state.brain_view.job_running = None;
                // Only the draft that started this job is closed or told why it failed.
                let own_editor = editor.is_some() && state.brain_view.editor.as_ref().map(|e| e.serial) == editor;
                match result {
                    Ok(msg) => {
                        if own_editor {
                            state.brain_view.editor = None;
                        }
                        state.brain_view.notice = Some(Ok(msg));
                        if matches!(job, ninox_ui_shared::brain_edit::Job::Delete { .. }) {
                            state.brain_view.selected = None;
                        }
                        Self::reload_brain_entries(state);
                        if let Some(id) = select {
                            return App::apply(state, Message::BrainSelectEntry(id));
                        }
                    }
                    Err(e) => match state.brain_view.editor.as_mut().filter(|_| own_editor) {
                        Some(ed) => ed.error = Some(e),
                        None => state.brain_view.notice = Some(Err(e)),
                    },
                }
                Task::none()
            }

            Message::BrainLinkClicked(url) => {
                use crate::components::internal_link::{decode, is_internal, InternalLink};
                match decode(&url) {
                    Some(InternalLink::Brain(link)) => {
                        let same_source = state
                            .brain_view
                            .selected
                            .as_deref()
                            .and_then(|id| state.brain_view.entries.iter().find(|e| e.id == id))
                            .map(|e| e.source.as_str());
                        if let Some(e) = crate::components::brain_panel::resolve_link(
                            &state.brain_view.entries,
                            &link,
                            same_source,
                        ) {
                            let id = e.id.clone();
                            return App::apply(state, Message::BrainSelectEntry(id));
                        }
                        Task::none()
                    }
                    Some(InternalLink::Session(_) | InternalLink::Unresolved(_)) => Task::none(),
                    None if is_internal(&url) => Task::none(),
                    None => App::apply(state, Message::OpenUrl(url.to_string())),
                }
            }

            // `App.brain` is a `BrainScope` shared with the server at startup —
            // this replaces the app's own scope with a fresh one over the chosen
            // catalogue's path (or the merge of every enabled catalogue for "All
            // sources"). It does NOT affect the server's copy (acceptable:
            // catalogue viewing is app-side; the server's brain stays on the
            // catalogue it was started with).
            Message::BrainSwitchCatalogue(idx) => {
                state.brain_view.editor = None;
                state.brain_view.delete_confirm = None;
                let opened = match idx {
                    Some(idx) => state
                        .catalogues
                        .get(idx)
                        .cloned()
                        .and_then(|cat| match BrainIndex::open(&cat.path) {
                            Ok(b) => Some(BrainScope::Single(Arc::new(b))),
                            Err(e) => {
                                tracing::warn!("open catalogue '{}': {e}", cat.name);
                                None
                            }
                        }),
                    None => match MergedBrainIndex::open(&state.catalogues) {
                        Ok(m) => Some(BrainScope::All(Arc::new(m))),
                        Err(e) => {
                            tracing::warn!("open merged brain: {e}");
                            None
                        }
                    },
                };
                let Some(new_brain) = opened else { return Task::none() };

                state.brain = new_brain;
                state.active_catalogue = idx;
                state.brain_view.selected = None;
                state.brain_view.markdown.clear();
                state.brain_view.kind_filter = None;
                state.brain_view.repo_filter = None;
                state.brain_view.tag_filter = None;
                state.brain_view.backlinks.clear();
                state.brain_view.related.clear();
                state.brain_view.outlinks.clear();
                state.brain_view.edges.clear();
                state.brain_view.loaded = false;
                Self::reload_brain_entries(state);
                // The local open above keeps the switch instant; every
                // remote-backed catalogue in scope then freshens in the
                // background (read-safe pull_if_stale + rebuild, the same
                // front door as the CLI's query/show) and lands via
                // `BrainFreshened` — previously the GUI never pulled on
                // switch at all, so teammates' pushes stayed invisible
                // until a CLI/server touch on the same directory.
                Self::freshen_active_brain_task(state)
            }

            Message::BrainFreshened { scope_id, brain } => {
                // Race guard: the pull ran in the background, so apply the
                // fresh index only if the user is still on this scope.
                // Comparing against the ACTIVE scope's identity (swapped on
                // every switch) means A→B→A while A's pull was in flight
                // still matches — and that fresh A index is exactly what we
                // want.
                if state.brain.identity() == scope_id {
                    state.brain = brain;
                    Self::reload_brain_entries(state);
                }
                Task::none()
            }

            Message::ToggleNotifications => {
                state.sidebar.show_notifications = !state.sidebar.show_notifications;
                if state.sidebar.show_notifications {
                    state.sidebar.show_pending_requests = false;
                }
                Task::none()
            }

            Message::TogglePendingRequests => {
                state.sidebar.show_pending_requests = !state.sidebar.show_pending_requests;
                if state.sidebar.show_pending_requests {
                    state.sidebar.show_notifications = false;
                }
                Task::none()
            }

            Message::DismissNotification(id) => {
                let declines_restore = state
                    .notifications
                    .iter()
                    .any(|n| n.id == id && n.kind == NotificationKind::FleetRestorePending);
                if declines_restore {
                    ninox_core::fleet::startup::dismiss_pending_restore(&state.engine.store);
                }
                state.notifications.retain(|n| n.id != id);
                if let Err(e) = state.engine.store.dismiss_notification(&id) {
                    tracing::warn!("couldn't persist dismissal of {id}: {e}");
                }
                Task::none()
            }

            Message::DismissAllNotifications => {
                if state.notifications.iter().any(|n| n.kind == NotificationKind::FleetRestorePending) {
                    ninox_core::fleet::startup::dismiss_pending_restore(&state.engine.store);
                }
                state.notifications.clear();
                if let Err(e) = state.engine.store.dismiss_notifications_through(state.notif_cursor) {
                    tracing::warn!("couldn't persist dismiss-all: {e}");
                }
                Task::none()
            }

            Message::NavigateNotification(session_id) => {
                state.sidebar.show_notifications = false;
                // Route through the same attach path as NavigateSession —
                // this view previously set state.view directly and never
                // attached a client or created a TerminalState, permanently
                // stranding the panel at "Terminal connecting…".
                let task = Self::apply(state, Message::NavigateSession(session_id));
                if let View::SessionDetail { panel, .. } = &mut state.view {
                    *panel = crate::components::session_detail::DetailPanel::Terminal;
                }
                task
            }

            Message::TogglePendingRequestExpanded(id) => {
                state.sidebar.expanded_pending_request =
                    if state.sidebar.expanded_pending_request.as_deref() == Some(id.as_str()) {
                        None
                    } else {
                        Some(id)
                    };
                state.sidebar.pending_request_draft.clear();
                Task::none()
            }

            Message::PendingRequestDraftChanged(text) => {
                state.sidebar.pending_request_draft = text;
                Task::none()
            }

            Message::AnswerUserRequest { id, answer } => {
                // An empty answer would permanently close the request with
                // nothing in it.
                if answer.trim().is_empty() {
                    return Task::none();
                }
                if let Err(e) = state.engine.store.answer_user_request(&id, &answer) {
                    tracing::warn!("failed to answer user request {id}: {e}");
                    state.user_request_error = Some(format!("answer {id} failed: {e}"));
                    return Task::none();
                }
                state.user_request_error = None;
                state.pending_user_requests.retain(|r| r.id != id);
                if state.sidebar.expanded_pending_request.as_deref() == Some(id.as_str()) {
                    state.sidebar.expanded_pending_request = None;
                    state.sidebar.pending_request_draft.clear();
                }
                Task::none()
            }

            Message::CancelUserRequest(id) => {
                if let Err(e) = state.engine.store.mark_user_request_cancelled(&id) {
                    tracing::warn!("failed to cancel user request {id}: {e}");
                    state.user_request_error = Some(format!("dismiss {id} failed: {e}"));
                    return Task::none();
                }
                state.user_request_error = None;
                state.pending_user_requests.retain(|r| r.id != id);
                if state.sidebar.expanded_pending_request.as_deref() == Some(id.as_str()) {
                    state.sidebar.expanded_pending_request = None;
                    state.sidebar.pending_request_draft.clear();
                }
                Task::none()
            }

            Message::ApplyUpdate => {
                if state.update_in_progress {
                    return Task::none();
                }
                state.update_in_progress = true;
                Task::future(async move {
                    use ninox_core::lifecycle::update_check::{SelfUpdateInstaller, UpdateInstaller};
                    let result = SelfUpdateInstaller.install("ninox").await.map_err(|e| e.to_string());
                    Message::UpdateApplied(result)
                })
            }

            Message::UpdateApplied(result) => {
                state.update_in_progress = false;
                for n in state.notifications.iter().filter(|n| n.kind == NotificationKind::UpdateAvailable) {
                    if let Err(e) = state.engine.store.dismiss_notification(&n.id) {
                        tracing::warn!("couldn't persist dismissal of {}: {e}", n.id);
                    }
                }
                state.notifications.retain(|n| n.kind != NotificationKind::UpdateAvailable);
                let ts = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64;
                match result {
                    Ok(()) => {
                        state.version_check = VersionCheckState::Installed;
                        Self::push_notification(state, Notification {
                            id:         format!("update-installed-{ts}"),
                            kind:       NotificationKind::UpdateInstalled,
                            title:      "Update installed".to_string(),
                            body:       "Restart ninox to use the new version.".to_string(),
                            session_id: None,
                            created_at: ts,
                        });
                    }
                    Err(e) => {
                        tracing::error!("update install failed: {e}");
                        state.version_check = VersionCheckState::Failed;
                        Self::push_notification(state, Notification {
                            id:         format!("update-failed-{ts}"),
                            kind:       NotificationKind::UpdateFailed,
                            title:      "Update failed".to_string(),
                            body:       e,
                            session_id: None,
                            created_at: ts,
                        });
                    }
                }
                Task::none()
            }

            Message::RestartApp => {
                match std::env::current_exe() {
                    Ok(exe) => {
                        if let Err(e) = std::process::Command::new(exe).spawn() {
                            tracing::warn!("restart: failed to spawn new instance: {e}");
                        }
                    }
                    Err(e) => tracing::warn!("restart: couldn't resolve current_exe: {e}"),
                }
                iced::exit()
            }

            Message::VersionCheckResult(result) => {
                state.version_check = match result {
                    Ok(Some(latest)) => VersionCheckState::UpdateAvailable(latest),
                    Ok(None)         => VersionCheckState::UpToDate,
                    Err(e) => {
                        tracing::warn!("settings version check failed: {e}");
                        VersionCheckState::Failed
                    }
                };
                Task::none()
            }

            Message::DeleteAsk(id) => {
                if state.delete_modal.is_some() || ninox_core::events::undeletable_reason(&id).is_some() {
                    return Task::none();
                }
                state.delete_modal = Some(crate::components::delete_modal::DeleteModal::new(id.clone()));
                let engine = state.engine.clone();
                Task::future(async move {
                    let root = id.clone();
                    let plan = tokio::task::spawn_blocking(move || engine.plan_delete(&id).map_err(|e| e.to_string()))
                        .await
                        .unwrap_or_else(|e| Err(e.to_string()));
                    Message::DeletePlanned { root, plan }
                })
            }

            Message::DeletePlanned { root, plan } => {
                if let Some(modal) = state.delete_modal.as_mut().filter(|m| m.root == root && m.plan.is_none()) {
                    modal.plan = Some(plan);
                }
                Task::none()
            }

            Message::DeleteToggleForce(on) => {
                if let Some(modal) = state.delete_modal.as_mut().filter(|m| m.submitted.is_none()) {
                    modal.force_worktrees = on;
                }
                Task::none()
            }

            Message::DeleteToggleExpanded => {
                if let Some(modal) = state.delete_modal.as_mut() {
                    modal.expanded = !modal.expanded;
                }
                Task::none()
            }

            Message::DeleteConfirm => {
                let Some(modal) = state.delete_modal.as_mut() else { return Task::none() };
                if modal.submitted.is_some() {
                    return Task::none();
                }
                let Some(plan) = modal.ready_plan().cloned() else { return Task::none() };
                let force = modal.force_worktrees;
                modal.submitted = Some((plan.targets.iter().map(|t| t.id.clone()).collect(), force));
                let engine = state.engine.clone();
                Task::future(async move { Message::DeleteDone(engine.execute_delete(&plan, force).await) })
            }

            Message::DeleteDone(results) => {
                use ninox_core::events::DeleteOutcome;
                for (id, outcome) in &results {
                    match outcome {
                        DeleteOutcome::Deleted => Self::forget_deleted_session(state, id),
                        DeleteOutcome::KeptWorktree(_) | DeleteOutcome::Failed(_) => {
                            if let Ok(Some(fresh)) = state.engine.store.get_session(id) {
                                state.sessions.insert(id.clone(), fresh);
                            }
                        }
                    }
                }
                let all_deleted = results.iter().all(|(_, o)| *o == DeleteOutcome::Deleted);
                match state.delete_modal.as_mut() {
                    Some(_) if all_deleted => state.delete_modal = None,
                    Some(modal) => modal.results = Some(results),
                    None => {}
                }
                Task::none()
            }

            Message::DeleteClose => {
                if !state.delete_modal.as_ref().is_some_and(|m| m.running()) {
                    state.delete_modal = None;
                }
                Task::none()
            }

            Message::ReapFinishedOpen => {
                state.reap_modal = Some(crate::components::reap_modal::ReapModal::open(&state.sessions, &state.orchestrators));
                Task::none()
            }

            Message::ReapFinishedConfirm => {
                let Some(modal) = state.reap_modal.as_mut() else { return Task::none() };
                if modal.running || modal.outcome.is_some() {
                    return Task::none();
                }
                modal.running = true;
                let batches = modal.by_orchestrator();
                let engine = state.engine.clone();
                Task::future(async move {
                    use ninox_core::events::{ReapOutcome, ReapSelection};
                    let (mut reaped, mut error) = (Vec::new(), None);
                    for (orch, ids) in batches {
                        // Only what was confirmed, and only if it's still
                        // finished and uncleaned: `Ids` would otherwise let an
                        // `AwaitingReview` worker through without force.
                        let ids: Vec<String> = ids
                            .into_iter()
                            .filter(|id| matches!(engine.store.get_session(id), Ok(Some(s)) if ninox_core::events::awaits_reap(&s)))
                            .collect();
                        if ids.is_empty() {
                            continue;
                        }
                        match engine.reap_workers(&orch, ReapSelection::Ids(&ids), false).await {
                            Ok(outcomes) => reaped.extend(
                                outcomes.into_iter().filter(|(_, o)| matches!(o, ReapOutcome::Reaped)).map(|(id, _)| id),
                            ),
                            Err(e) => {
                                error.get_or_insert(format!("{orch}: {e}"));
                            }
                        }
                    }
                    Message::ReapFinishedDone { reaped, error }
                })
            }

            Message::ReapFinishedDone { reaped, error } => {
                for id in &reaped {
                    if let Ok(Some(fresh)) = state.engine.store.get_session(id) {
                        state.sessions.insert(id.clone(), fresh);
                    }
                }
                if let Some(modal) = state.reap_modal.as_mut() {
                    modal.running = false;
                    let done = crate::components::reap_modal::outcome_text(reaped.len());
                    modal.outcome = Some(match error {
                        None => Ok(done),
                        Some(e) => Err(format!("{e}. {done}")),
                    });
                }
                Task::none()
            }

            Message::ReapFinishedClose => {
                if !state.reap_modal.as_ref().is_some_and(|m| m.running) {
                    state.reap_modal = None;
                }
                Task::none()
            }

            Message::RestoreFleetOpen => {
                state.restore_modal = Some(Default::default());
                let store = state.engine.store.clone();
                let config = state.config.clone();
                let plan = ninox_core::test_isolation::propagate(move || {
                    let plan = ninox_ui_shared::fleet::restore_plan(&store, &config).map_err(|e| e.to_string());
                    (plan, ninox_ui_shared::fleet::restore_running(&store))
                });
                Task::perform(
                    async move { tokio::task::spawn_blocking(plan).await.unwrap_or_else(|e| (Err(e.to_string()), None)) },
                    |(plan, running)| Message::RestoreFleetPlanned(plan, running),
                )
            }

            Message::RestoreFleetPlanned(plan, running) => {
                if state.restore_modal.is_some() {
                    let mut modal = crate::components::restore_modal::RestoreModal::loaded(plan);
                    modal.running = running;
                    state.restore_modal = Some(modal);
                }
                Task::none()
            }

            Message::RestoreFleetToggle(id) => {
                if let Some(modal) = state.restore_modal.as_mut() {
                    modal.toggle(&id);
                    modal.confirm = None;
                }
                Task::none()
            }

            Message::RestoreFleetRequest(scope) => {
                if let Some(modal) = state.restore_modal.as_mut() {
                    modal.request(scope);
                }
                Task::none()
            }

            Message::RestoreFleetCancelConfirm => {
                if let Some(modal) = state.restore_modal.as_mut() {
                    modal.confirm = None;
                }
                Task::none()
            }

            Message::RestoreFleetConfirm => {
                let Some(modal) = state.restore_modal.as_mut() else { return Task::none() };
                let Some(scope) = modal.confirm.take() else { return Task::none() };
                // Another restore may have started since the plan loaded.
                if let Some(since) = ninox_ui_shared::fleet::restore_running(&state.engine.store) {
                    modal.running = Some(since);
                    return Task::none();
                }
                let only = modal.only(scope);
                // This instance's own store and config: an isolated window
                // can only ever restore its own scratch fleet.
                let db_path = state.db_path.clone();
                let config_path = AppConfig::config_path();
                Task::perform(
                    async move { ninox_ui_shared::fleet::spawn_detached_restore(&db_path, &config_path, &only) },
                    Message::RestoreFleetStarted,
                )
            }

            Message::RestoreFleetStarted(result) => {
                if result.is_ok() {
                    state.notifications.retain(|n| n.kind != NotificationKind::FleetRestorePending);
                }
                if let Some(modal) = state.restore_modal.as_mut() {
                    modal.outcome = Some(result);
                }
                Task::none()
            }

            Message::RestoreFleetClose => {
                state.restore_modal = None;
                Task::none()
            }

            Message::ServerInfoTick => {
                let port = state.server.port;
                if port == 0 {
                    return Task::none();
                }
                Task::future(async move {
                    Message::ServerInfoLoaded(ninox_ui_shared::server_restart::fetch_info(port).await)
                })
            }
            Message::ServerInfoLoaded(info) => {
                let stale = info.as_ref().and_then(|i| ninox_ui_shared::server_restart::stale_daemon_warning(
                    i, env!("CARGO_PKG_VERSION"), ninox_core::server_control::BUILD_HASH,
                ));
                // Logged once per build seen, not on every poll; the banner
                // stays up from `alerts` below.
                let seen = state.server.info.as_ref().map(|i| (&i.version, &i.build));
                if let Some(warning) = stale.filter(|_| seen != info.as_ref().map(|i| (&i.version, &i.build))) {
                    tracing::warn!("{warning}");
                }
                state.server.alerts = info.as_ref().map(ninox_ui_shared::server_restart::alerts_for_this_build).unwrap_or_default();
                state.server.info = info;
                Task::none()
            }
            Message::RequestRestartServer => {
                state.server.confirming = !state.server.restarting;
                state.server.result = None;
                Task::none()
            }
            Message::CancelRestartServer => {
                state.server.confirming = false;
                Task::none()
            }
            Message::ConfirmRestartServer => {
                state.server.confirming = false;
                if state.server.restarting {
                    return Task::none();
                }
                if state.server.port == 0 {
                    state.server.result = Some(Err("this app doesn't know its server's port".into()));
                    return Task::none();
                }
                state.server.restarting = true;
                let (port, db_path) = (state.server.port, state.db_path.clone());
                let store = state.engine.store.clone();
                Task::future(async move {
                    let target = ninox_ui_shared::server_restart::Target::resolve(port, db_path);
                    let ops = ninox_ui_shared::server_restart::LiveOps { store: Some(store) };
                    let result = ninox_ui_shared::server_restart::restart(&target, &ops, Default::default(), |_| {})
                        .await
                        .map(|o| o.summary())
                        .map_err(|e| e.to_string());
                    Message::ServerRestartDone(result)
                })
            }
            Message::ServerRestartDone(result) => {
                state.server.restarting = false;
                state.server.result = Some(result);
                Task::done(Message::ServerInfoTick)
            }
            Message::OpenServerRestart => Task::done(Message::NavigateSettings)
                .chain(Task::done(Message::SettingsSection(crate::components::settings_panel::SettingsSection::Engine)))
                .chain(Task::done(Message::RequestRestartServer)),
            Message::RestartAffectedSessions(ids) => {
                if state.server.restarting_sessions || ids.is_empty() {
                    return Task::none();
                }
                state.server.restarting_sessions = true;
                let store = state.engine.store.clone();
                let db_path = state.db_path.clone();
                Task::future(async move {
                    Message::AffectedSessionsRestarted(
                        ninox_ui_shared::server_restart::restart_sessions(store, db_path, ids).await,
                    )
                })
            }
            Message::AffectedSessionsRestarted(result) => {
                state.server.restarting_sessions = false;
                state.server.result = Some(result);
                Task::done(Message::ServerInfoTick)
            }

            Message::RequestRestartAll => {
                if live_session_count(&state.sessions) > 0 {
                    state.restart_all_confirm = true;
                }
                Task::none()
            }
            Message::CancelRestartAll => {
                state.restart_all_confirm = false;
                Task::none()
            }
            Message::ConfirmRestartAll => {
                state.restart_all_confirm = false;
                if state.restart_all_in_progress {
                    return Task::none();
                }
                state.restart_all_in_progress = true;
                let store = state.engine.store.clone();
                let config = state.config.clone();
                let db_path = state.db_path.clone();
                Task::future(async move {
                    let args = ninox_ui_shared::restart::RestartArgs { session_ids: vec![], all: true, exec_detached: false };
                    let result = ninox_ui_shared::restart::execute(args, store, config, db_path).await.map_err(|e| e.to_string());
                    Message::RestartAllApplied(result)
                })
            }
            Message::RestartAllApplied(result) => {
                state.restart_all_in_progress = false;
                let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64;
                let (kind, title, body) = match result {
                    Ok(outcomes) => {
                        let failed = ninox_ui_shared::restart::had_trouble(&outcomes);
                        let body = ninox_ui_shared::restart::summarize(&outcomes);
                        if failed {
                            (NotificationKind::RestartAllFailed, "Restart all: some agents failed".to_string(), body)
                        } else {
                            (NotificationKind::RestartAllCompleted, "Restart all agents".to_string(), body)
                        }
                    }
                    Err(e) => {
                        tracing::error!("restart all failed: {e}");
                        (NotificationKind::RestartAllFailed, "Restart all agents failed".to_string(), e)
                    }
                };
                Self::push_notification(state, Notification {
                    id: format!("restart-all-{ts}"),
                    kind,
                    title,
                    body,
                    session_id: None,
                    created_at: ts,
                });
                Task::none()
            }

            Message::FleetFilterQuery(q) => {
                state.fleet_filter.query = q;
                Task::none()
            }
            Message::ClearFleetFilter => {
                state.fleet_filter = FleetFilter::default();
                Task::none()
            }

            Message::ScrollTerminal { session_id, delta, local } => {
                if let Some(term) = state.terminals.get_mut(&session_id) {
                    let mode = *term.term.mode();
                    if let Some(bytes) = crate::input::scroll_pty_bytes(local, delta, &mode) {
                        if let Some(client) = state.clients.get(&session_id) {
                            for _ in 0..delta.unsigned_abs() { client.write(bytes.clone()); }
                        }
                    } else if term.scroll(delta) {
                        // Cache edge hit while more history may exist —
                        // fetch the next chunk from tmux (the source of
                        // truth for scrollback; the live grid holds none).
                        term.scrollback.fetch_pending = true;
                        let from = term.scrollback.fetched_to; // 0 on first fetch
                        let sid = session_id.clone();
                        return Task::future(async move {
                            use crate::components::scrollback::FETCH_CHUNK;
                            let total = ninox_core::runtime::history_size(&sid).await;
                            let end = from - 1; // next line above cache
                            let start = (from - FETCH_CHUNK).max(-total);
                            if end < -total || total == 0 {
                                return Message::HistoryFetched {
                                    session_id: sid, bytes: Vec::new(),
                                    fetched_to: from, top_reached: true,
                                };
                            }
                            let bytes = ninox_core::runtime::capture_history(&sid, start, end).await;
                            Message::HistoryFetched {
                                session_id: sid, bytes,
                                fetched_to: start, top_reached: start <= -total,
                            }
                        });
                    }
                }
                Task::none()
            }

            Message::JumpToLatest { session_id } => {
                if let Some(term) = state.terminals.get_mut(&session_id) {
                    term.scroll_to_bottom();
                }
                Task::none()
            }

            Message::HistoryFetched { session_id, bytes, fetched_to, top_reached } => {
                if let Some(term) = state.terminals.get_mut(&session_id) {
                    use alacritty_terminal::grid::Dimensions;
                    let cols = term.term.grid().columns() as u16;
                    let lines = crate::components::scrollback::parse_capture(&bytes, cols);
                    term.scrollback.absorb(lines, fetched_to, top_reached);
                    term.cache.clear();
                }
                Task::none()
            }

            Message::OpenUrl(url) => {
                let _ = std::process::Command::new(open_url_program()).arg(&url).spawn();
                Task::none()
            }

            Message::OpenInEditor(path) => {
                // Fire-and-forget, exactly like OpenUrl: no error surfacing.
                // If the configured editor isn't on PATH this silently
                // no-ops, which is the accepted product behaviour. Neovim is
                // scoped out here (see `more_menu_items` in session_detail.rs): the
                // desktop app has no attached terminal for it to run in, so
                // the button is disabled rather than spawning a useless
                // detached process.
                if !ninox_ui_shared::editor::is_terminal(state.config.editor) {
                    let _ = std::process::Command::new(ninox_ui_shared::editor::program(state.config.editor))
                        .arg(&path)
                        .spawn();
                }
                Task::none()
            }

            Message::ModelListLoaded { harness, models } => {
                state.model_lists.insert(harness, models);
                Task::none()
            }

            Message::DiffFetched { session_id, diff } => {
                state.diffs.insert(session_id, diff);
                Task::none()
            }

            Message::Noop => Task::none(),

            Message::ToggleHelp => {
                state.help_open = !state.help_open;
                Task::none()
            }
        }
    }

    fn handle_engine_event(state: &mut Self, event: Event) -> Task<Message> {
        match event {
            Event::OrchestratorSpawned(orch) => {
                if !state.orchestrators.iter().any(|o| o.id == orch.id) {
                    state.orchestrators.push(orch);
                }
                Task::none()
            }

            Event::OrchestratorRemoved(id) => {
                state.orchestrators.retain(|o| o.id != id);
                let sessions = &mut state.sessions;
                state.unread_messages.retain(|sid, _| {
                    sid != &id
                        && sessions.get(sid).and_then(|s| s.orchestrator_id.as_deref()) != Some(id.as_str())
                });
                sessions.retain(|_, s| s.orchestrator_id.as_deref() != Some(id.as_str()));
                state.transcripts.retain(|sid, _| state.sessions.contains_key(sid));
                Task::none()
            }

            Event::SessionSpawned(session) => {
                state.sessions.insert(session.id.clone(), session);
                Task::none()
            }

            Event::SessionUpdated(incoming, fields) => {
                match state.sessions.get_mut(&incoming.id) {
                    Some(existing) => existing.merge_from(&incoming, fields),
                    None => { state.sessions.insert(incoming.id.clone(), incoming); }
                }
                Task::none()
            }

            Event::SessionDone(id) => {
                if let Some(s) = state.sessions.get_mut(&id) {
                    s.status = SessionStatus::Done;
                }
                state.unread_messages.remove(&id);
                state.terminals.remove(&id);
                // A done session is definitionally not viewable.
                state.clients.remove(&id);
                state.attach.remove(&id);
                state.diffs.remove(&id);
                Task::none()
            }

            Event::TerminalOutput { .. } => {
                // Raw pane tap — consumed by the browser WS route, not the app.
                Task::none()
            }

            Event::ClientOutput { session_id, generation, bytes } => {
                // A stale client (superseded by a fresh attach) may still
                // have a reader thread draining its last buffered output —
                // only apply it if it's still the client on record.
                let current = state.clients.get(&session_id).map(|c| c.generation);
                if current != Some(generation) {
                    return Task::none();
                }
                if let Some(p) = state.attach.get_mut(&session_id) {
                    if !p.got_output {
                        tracing::info!(session = %session_id, generation, "terminal attach first output");
                    }
                    p.push_output(&bytes[bytes.len().saturating_sub(512)..]);
                }
                if let Some(term) = state.terminals.get_mut(&session_id) {
                    term.process(&bytes);
                }
                Task::none()
            }

            Event::ClientClosed { session_id, generation } => {
                // Ignore a close from a client generation that is no longer
                // the one on record — e.g. a deliberately dropped old
                // client (NavigateSession re-click) whose ClientClosed
                // arrives after a fresh client has already been attached.
                // Acting on it would strand the new client by removing it
                // and its terminal out from under the view.
                let current = state.clients.get(&session_id).map(|c| c.generation);
                if current != Some(generation) {
                    return Task::none();
                }

                let viewing = matches!(&state.view,
                    View::SessionDetail { session_id: sid, .. } if sid == &session_id);
                state.clients.remove(&session_id);
                state.diffs.remove(&session_id);
                if session_id == crate::components::remotes::MACHINE_ADD_SESSION_ID {
                    // Unlike every other closed client, this one's terminal
                    // is kept: `resolve_machine_add` freezes it on screen
                    // alongside the inferred result until the user closes
                    // it (`Message::RemoteAddClose`), rather than it
                    // vanishing with the generic `terminals.remove` below.
                    Self::resolve_machine_add(state);
                } else {
                    state.terminals.remove(&session_id);
                }
                if viewing && session_id != crate::components::remotes::MACHINE_ADD_SESSION_ID {
                    let (attempt, reason) = match state.attach.get(&session_id) {
                        Some(p) => (if p.died_fast() { p.attempt + 1 } else { 1 }, p.last_line()),
                        None => (1, None),
                    };
                    tracing::warn!(session = %session_id, generation, attempt, ?reason, "terminal attach child closed");
                    return Self::retry_or_fail(
                        state, &session_id, attempt,
                        reason.unwrap_or_else(|| "the terminal connection closed".to_string()),
                    );
                }
                state.attach.remove(&session_id);
                Task::none()
            }

            Event::CiUpdated { pr_id, status } => {
                state.ci_status.insert(pr_id, status);
                Task::none()
            }

            Event::PrOpened { session_id, pr } => {
                if let Some(s) = state.sessions.get_mut(&session_id) {
                    s.pr_number = Some(pr.number);
                    s.pr_id     = Some(pr.id);
                }
                state.prs.insert(pr.id, pr);
                // The session may already be the one on screen (its PR just
                // opened while the user was looking at it) — without this,
                // review_threads[pr_id] (if already loaded, e.g. from a prior
                // run that touched the same PR) would render Marginalia
                // cards with blank bodies until the user navigates away and
                // back.
                Self::ensure_comment_editors(state, &session_id);
                Task::none()
            }

            // Never touches the owning session's tracked pr_number/pr_id —
            // this is a PR beyond the tracked one (typically an agent
            // opening a duplicate by mistake); it must surface on the Pull
            // Requests ledger without displacing what the session actually
            // tracks.
            Event::ExtraPrDetected(pr) => {
                state.prs.insert(pr.id, pr);
                Task::none()
            }

            Event::ReviewComment { pr_id, comment } => {
                let thread = state.review_threads.entry(pr_id).or_default();
                // The poller re-emits on every poll that still sees a comment
                // in its GitHub response (only *new* ids are skipped there);
                // guard here too so a repeated emit doesn't duplicate the
                // in-memory feed the way it would upsert-replace the DB row.
                if !thread.iter().any(|c| c.id == comment.id) {
                    state.comment_editors.insert(
                        comment.id,
                        iced::widget::text_editor::Content::with_text(&comment.body),
                    );
                    thread.push(comment);
                    thread.sort_by_key(|c| c.created_at);
                }
                Task::none()
            }

            // Delivered to the session itself; the review feed and
            // notifications above already show the same activity here.
            Event::PrEvents { .. } => Task::none(),

            // `Engine::emit` already persisted it; the store read raises the alert.
            Event::Notification(_) => {
                Self::sync_notifications(state, true);
                Task::none()
            }

            Event::HeldMessages { held, by_sender } => {
                state.held_messages = held;
                state.queued_by_sender = by_sender;
                Task::none()
            }

            Event::MessagesDelivered { session_id, count } => {
                let on_screen = matches!(&state.view,
                    View::SessionDetail { session_id: sid, .. } if sid == &session_id);
                if !on_screen {
                    *state.unread_messages.entry(session_id).or_insert(0) += count;
                }
                Task::none()
            }

            // TUI/GUI rendering of the structured transcript is a separate,
            // already-filed follow-on (see the cross-harness-messaging
            // design's "Rendering implications" section) — this event exists
            // so that follow-on has something to subscribe to.
            Event::SessionTranscript { session_id, entry } => {
                ninox_ui_shared::transcript::push(state.transcripts.entry(session_id).or_default(), entry);
                Task::none()
            }
        }
    }

    /// Records a notification this process constructs directly (e.g. an
    /// on-demand action's outcome) in the store, where the panel reads it
    /// back like any engine-sourced one.
    fn push_notification(state: &mut Self, n: Notification) {
        if let Err(e) = state.engine.store.insert_notification(&n) {
            tracing::warn!("couldn't persist notification {}: {e}", n.id);
        }
        Self::sync_notifications(state, true);
    }

    /// Rebuilds the panel from the store's undismissed rows (keeping the
    /// store-less fleet-restore offer) and, when `alert`, posts a desktop
    /// notification for each row past `notif_cursor` — once, then advances it.
    fn sync_notifications(state: &mut Self, alert: bool) {
        let rows = match state.engine.store.undismissed_notifications() {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!("couldn't read notifications: {e}");
                return;
            }
        };
        let cursor = state.notif_cursor;
        for r in rows.iter().filter(|r| r.seq > cursor) {
            #[cfg(not(test))]
            if alert {
                Self::post_desktop_notification(r.notification.title.clone(), r.notification.body.clone());
            }
            #[cfg(test)]
            if alert {
                state.alerted.push(r.notification.id.clone());
            }
        }
        if let Some(max) = rows.last().map(|r| r.seq) {
            state.notif_cursor = state.notif_cursor.max(max);
        }
        let restore: Vec<_> = state.notifications.iter().filter(|n| n.kind == NotificationKind::FleetRestorePending).cloned().collect();
        let skip = rows.len().saturating_sub(MAX_NOTIFICATIONS);
        state.notifications = restore.into_iter().chain(rows.into_iter().skip(skip).map(|r| r.notification)).collect();
    }

    #[cfg(not(test))]
    fn post_desktop_notification(title: String, body: String) {
        std::thread::spawn(move || {
            // Pin the sending application before the first notification:
            // notify-rust's macOS backend otherwise resolves the sender
            // lazily via the AppleScript `get id of application
            // "use_default"`, and macOS answers an unknown app name with a
            // blocking "Where is use_default?" application picker.
            #[cfg(target_os = "macos")]
            {
                static PIN_SENDER: std::sync::Once = std::sync::Once::new();
                PIN_SENDER.call_once(|| {
                    let exe = std::env::current_exe().unwrap_or_default();
                    let _ = notify_rust::set_application(notification_sender_bundle_id(&exe));
                });
            }
            let _ = notify_rust::Notification::new()
                .summary(&title)
                .body(&body)
                .show();
        });
    }

    /// A 5px drag handle strip between resizable panels. Shows a
    /// horizontal-resize cursor on hover so the divider reads as draggable.
    pub fn drag_handle<'a>(target: DragTarget, border: iced::Color) -> Element<'a, Message> {
        use iced::widget::{container, mouse_area, Space};
        use iced::{Background, Length};
        mouse_area(
            container(Space::new(5, 0))
                .height(Length::Fill)
                .style(move |_theme| container::Style {
                    background: Some(Background::Color(border)),
                    ..Default::default()
                }),
        )
        .interaction(iced::mouse::Interaction::ResizingHorizontally)
        .on_press(Message::StartDrag(target))
        .into()
    }

    /// View — the v1 shell (nav/tree/detail-panel/bottom-bar) wrapping
    /// whichever tab (or session detail/settings) is active in its centre
    /// column. Replaces the old sidebar+content chrome entirely — see
    /// `docs/superpowers/plans/2026-10-27-v1-gui-shell.md`. The nav bar
    /// (always rendered, regardless of tab or `sidebar_hidden`) carries the
    /// pending-user-requests badge — see `components::shell::nav::nav`.
    pub fn iced_view(state: &Self) -> Element<'_, Message> {
        use crate::components::{
            brain_panel::brain_panel,
            catalogue_modal::catalogue_modal,
            fleet_board::fleet_board,
            fleet_focus::fleet_focus,
            fleet_islands::fleet_islands,
            remotes::{self, remotes_view},
            restart_all_modal::restart_all_modal,
            session_detail::session_detail,
            settings_panel::settings_panel,
            shell::shell,
            spawn_modal::spawn_modal,
            worker_types::worker_types_view,
        };

        // Computed once here (only on the Fleet tab) and threaded through to
        // both the main content and the session tree, rather than each
        // independently querying the store and rebuilding the island graph
        // — see `shell::shell`'s doc comment.
        let fleet_islands_data = matches!(state.view, View::Fleet).then(|| build_fleet_islands(state));

        // Likewise computed once (only when an island is actually open) and
        // threaded to both `fleet_focus` (the opened island's own cards)
        // and `shell` (the detail panel's breadcrumb) — see `shell::shell`'s
        // doc comment.
        let open_island_edges: Option<Vec<ninox_overview::edge::OverviewEdge>> = fleet_islands_data.as_deref()
            .zip(state.fleet_nav.open.as_ref())
            .and_then(|(islands, key)| islands.iter().find(|i| &i.key == key))
            .map(|island| crate::components::fleet_focus::nodes_for_island(state, island).1);

        let main: Element<Message> = match &state.view {
            View::Fleet => match &state.fleet_nav.open {
                Some(key) => fleet_focus(
                    state,
                    fleet_islands_data.as_deref().unwrap_or_default(),
                    key,
                    open_island_edges.as_deref().unwrap_or_default(),
                ),
                None => fleet_islands(state, fleet_islands_data.as_deref().unwrap_or_default()),
            },
            View::Board { scope } => fleet_board(state, scope.as_ref()),
            View::SessionDetail { session_id, panel } => session_detail(state, session_id, panel),
            View::Plans => crate::components::ghost_plans::plans_view(state),
            View::Brain => brain_panel(state),
            View::WorkerTypes => worker_types_view(state),
            View::Remotes => remotes_view(state),
            View::Idle => crate::components::idle_view::idle_view(state),
            View::Settings => settings_panel(state),
        };

        let base: Element<Message> = shell(state, main, fleet_islands_data.as_deref(), open_island_edges.as_deref());

        // Spawn wins on top if both were somehow set — exclusive in
        // practice (spawn lives in other views).
        if state.help_open {
            iced::widget::stack![base, crate::components::help_overlay::help_overlay()].into()
        } else if let Some(form) = &state.spawn_modal {
            iced::widget::stack![base, spawn_modal(state, form)].into()
        } else if let Some(form) = &state.catalogue_modal {
            iced::widget::stack![base, catalogue_modal(state, form)].into()
        } else if state.restart_all_confirm {
            iced::widget::stack![base, restart_all_modal(state)].into()
        } else if let Some(modal) = &state.restore_modal {
            iced::widget::stack![base, crate::components::restore_modal::restore_modal(modal)].into()
        } else if let Some(modal) = &state.reap_modal {
            iced::widget::stack![base, crate::components::reap_modal::reap_modal(modal)].into()
        } else if let Some(modal) = &state.delete_modal {
            iced::widget::stack![base, crate::components::delete_modal::delete_modal(modal)].into()
        } else if let Some(modal) = &state.workspaces_modal {
            iced::widget::stack![base, crate::components::workspaces_modal::workspaces_modal(state, modal)].into()
        } else if let (true, Some(form)) = (matches!(state.view, View::Remotes), &state.remotes.add) {
            // Gated on `View::Remotes`: unlike every other modal here, this
            // one's state can survive navigating away (a running subprocess
            // keeps producing output, and a finished one's frozen result is
            // worth returning to) — but it must only ever be drawn over the
            // Remotes tab itself, not left stacked over whatever view the
            // user clicked into afterward.
            iced::widget::stack![base, remotes::add_remote_modal(state, form)].into()
        } else if state.sidebar.show_notifications {
            use iced::{widget::{container, mouse_area}, Length};
            let dropdown = container(
                iced::widget::opaque(
                    container(crate::components::notification_panel::notification_panel(state))
                        .width(Length::Fixed(400.0)),
                ),
            )
            .align_right(Length::Fill)
            .align_top(Length::Fill)
            .padding(iced::Padding {
                top: 50.0,
                right: crate::components::shell::nav::bell_right_inset(state),
                bottom: 0.0,
                left: 0.0,
            });
            let backdrop = mouse_area(container(iced::widget::Space::new(Length::Fill, Length::Fill)))
                .on_press(Message::ToggleNotifications);
            iced::widget::stack![base, backdrop, dropdown].into()
        } else if state.sidebar.show_pending_requests {
            use iced::{widget::container, Length};
            let dropdown = container(
                container(crate::components::pending_requests_panel::pending_requests_panel(state))
                    .width(Length::Fixed(320.0)),
            )
            .align_right(Length::Fill)
            .align_top(Length::Fill)
            .padding(iced::Padding { top: 50.0, right: 12.0, bottom: 0.0, left: 0.0 });
            iced::widget::stack![base, dropdown].into()
        } else if let Some(panel) = crate::components::global_search::dropdown_overlay(state) {
            use iced::{widget::container, Length};
            // Fixed-offset approximation of "under the search field", same
            // approach the notifications dropdown above takes for its bell
            // icon — the field's exact position shifts with window width,
            // and iced has no "anchor to this other widget" overlay
            // primitive to do this precisely.
            let dropdown = container(panel)
                .align_right(Length::Fill)
                .align_top(Length::Fill)
                .padding(iced::Padding { top: 50.0, right: 150.0, bottom: 0.0, left: 0.0 });
            iced::widget::stack![base, dropdown].into()
        } else {
            base
        }
    }
}

/// Pull one `Event` off `rx`, merging any `ClientOutput` chunks already
/// sitting in the channel for the same client generation into it.
/// `pending` carries a lookahead event across calls: a `try_recv` that
/// turns out not to match can't be put back on the channel, so it's
/// stashed here and returned as-is on the following call instead of being
/// dropped or clobbering the event already in hand.
///
/// The PTY reader thread (`AttachedClient::spawn`) emits one `ClientOutput`
/// per raw `read()`, capped at 8KB — a single full-screen repaint (a tmux
/// status refresh, a busy TUI redrawing) routinely arrives as many of these
/// back-to-back. Rendering each as its own frame is what surfaces as a
/// flickering, "darting" cursor mid-repaint: the view briefly shows the
/// cursor at each intermediate position the repaint passes through instead
/// of settling directly on the final one. This only merges chunks that are
/// already buffered by the time we look — it never waits for more to
/// arrive, so it adds no latency to genuinely paced output (e.g. a spinner).
/// Chunks from a different generation are never merged together, matching
/// the same stale-vs-current-client distinction `ClientOutput`'s handler
/// already relies on elsewhere.
/// Whether `id` has a pane to attach to, and how. Bounded as a whole: each
/// ptyd request can take 15 s to time out and this makes several, so a stalled
/// host would otherwise hold the view at "connecting" for the sum.
async fn prepare_attach(engine: std::sync::Arc<ninox_core::events::Engine>, id: &SessionId, budget: std::time::Duration) -> crate::attach::Prepared {
    use crate::attach::Prepared;
    let lookup = async {
        match ninox_core::runtime::liveness(id).await {
            ninox_core::runtime::Liveness::Live => {}
            ninox_core::runtime::Liveness::Unknown => return Prepared::Unavailable("the pane host is not answering".to_string()),
            ninox_core::runtime::Liveness::Dead => {
                // Same rule as the poller: Interrupted when the harness can
                // resume it, re-reading the row.
                let registry = ninox_core::config::AppConfig::load_or_default().registry();
                if let Ok(Some(s)) = ninox_core::lifecycle::poller::reconcile_dead_session(&engine.store, &registry, id) {
                    engine.emit(ninox_core::events::Event::SessionUpdated(s, ninox_core::types::SessionFields::STATUS));
                }
                return Prepared::Gone;
            }
        }
        Prepared::Ready(ninox_core::runtime::attach_args(id).await)
    };
    let prepared = match tokio::time::timeout(budget, lookup).await {
        Ok(p) => p,
        Err(_) => return Prepared::Unavailable(format!("the pane host did not answer within {}s", budget.as_secs())),
    };
    if matches!(prepared, Prepared::Ready(_)) {
        // Keep the pipe-pane tap alive for the WS route/monitoring — off the
        // attach's critical path, since the view doesn't need it.
        let id = id.clone();
        tokio::spawn(async move {
            let tap = ninox_core::pty::start_streaming(engine, id.clone(), &id);
            match tokio::time::timeout(budget, tap).await {
                Ok(Err(e)) => tracing::warn!("pipe-pane tap for {id}: {e}"),
                Err(_) => tracing::warn!("pipe-pane tap for {id}: timed out"),
                Ok(Ok(())) => {}
            }
        });
    }
    prepared
}

/// Collapses a drained burst of engine events so iced's bounded subscription
/// channel sees one message per thing that changed, not one per emission.
///
/// Only *adjacent* events merge, so the relative order of everything else is
/// untouched: consecutive `ClientOutput` chunks for one session+generation
/// concatenate, and within a run of `SessionUpdated` events, repeats for the
/// same session fold into the first one's slot (see [`fold_session_update`]).
fn coalesce_events(batch: impl IntoIterator<Item = Event>) -> std::collections::VecDeque<Event> {
    let mut out: Vec<Event> = Vec::new();
    for event in batch {
        match event {
            Event::ClientOutput { session_id, generation, bytes } => {
                if let Some(Event::ClientOutput { session_id: sid, generation: gen, bytes: acc }) = out.last_mut() {
                    if *sid == session_id && *gen == generation {
                        acc.extend(bytes);
                        continue;
                    }
                }
                out.push(Event::ClientOutput { session_id, generation, bytes });
            }
            Event::SessionUpdated(incoming, fields) => {
                let run_start = out.iter().rposition(|e| !matches!(e, Event::SessionUpdated(..))).map_or(0, |i| i + 1);
                let slot = out[run_start..].iter_mut().find_map(|e| match e {
                    Event::SessionUpdated(known, known_fields) if known.id == incoming.id => Some((known, known_fields)),
                    _ => None,
                });
                match slot {
                    Some((known, known_fields)) => fold_session_update(known, known_fields, &incoming, fields),
                    None => out.push(Event::SessionUpdated(incoming, fields)),
                }
            }
            other => out.push(other),
        }
    }
    out.into()
}

/// Folds a later `SessionUpdated(incoming, fields)` into an earlier one for
/// the same session. Overlaying only `fields` keeps the earlier snapshot's
/// values for fields the later emitter was not authoritative for, so applying
/// the folded event equals applying both in order.
fn fold_session_update(known: &mut Session, known_fields: &mut SessionFields, incoming: &Session, fields: SessionFields) {
    known.merge_from(incoming, fields);
    *known_fields = *known_fields | fields;
}

/// Next event for iced. Every call first moves whatever is buffered on the
/// broadcast channel into `pending` and re-coalesces it, so a long backlog
/// being handed out one message at a time never leaves fresh events sitting
/// in the (bounded, lag-dropping) channel. It never waits for more events
/// while `pending` is non-empty, so this adds no latency to paced output.
async fn next_coalesced_event(
    rx: &mut broadcast::Receiver<Event>,
    pending: &mut std::collections::VecDeque<Event>,
) -> Option<Event> {
    let mut batch = Vec::new();
    if pending.is_empty() {
        batch.push(loop {
            match rx.recv().await {
                Ok(event) => break event,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(dropped = n, "GUI engine-event receiver lagged; events were dropped");
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        });
    }
    loop {
        match rx.try_recv() {
            Ok(event) => batch.push(event),
            Err(broadcast::error::TryRecvError::Lagged(n)) => {
                tracing::warn!(dropped = n, "GUI engine-event receiver lagged; events were dropped");
            }
            Err(_) => break,
        }
    }
    if !batch.is_empty() {
        *pending = coalesce_events(pending.drain(..).chain(batch));
    }
    pending.pop_front()
}

impl App {
    /// Subscription that drives `Message::EngineEvent` from the engine broadcast channel.
    pub fn subscription(state: &Self) -> Subscription<Message> {
        let mut rx: broadcast::Receiver<Event> = state.engine.subscribe();
        let mut pending = std::collections::VecDeque::new();
        let engine_sub = Subscription::run_with_id(
            "engine-events",
            async_stream::stream! {
                while let Some(event) = next_coalesced_event(&mut rx, &mut pending).await {
                    yield Message::EngineEvent(Box::new(event));
                }
            },
        );

        // Global keyboard subscription for the active terminal.
        // listen_with takes a fn pointer (no captures), so we emit RawKey / WindowResized
        // for all Ignored key events and route to the active session in the handler.
        let keyboard_sub = iced::event::listen_with(global_event_handler);

        let poll_sub = Subscription::run_with_id(
            "db-poll",
            async_stream::stream! {
                loop {
                    tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
                    yield Message::PollSessions;
                }
            },
        );

        // Starts with a tick so alerts show from launch, not 30s in.
        let server_info_sub = Subscription::run_with_id(
            "server-info-poll",
            async_stream::stream! {
                loop {
                    yield Message::ServerInfoTick;
                    tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
                }
            },
        );

        let reap_sub = iced::time::every(std::time::Duration::from_secs(2)).map(|_| Message::AttachReap);
        let mut subs = vec![engine_sub, keyboard_sub, poll_sub, server_info_sub, reap_sub];
        if Self::wants_thinking_tick(state) {
            subs.push(iced::time::every(crate::motion::TICK).map(|_| Message::ThinkingTick));
        }
        Subscription::batch(subs)
    }

    /// The thinking animation's clock runs only while its placeholder is on
    /// screen and allowed to move — no wakeups otherwise.
    pub fn wants_thinking_tick(state: &Self) -> bool {
        state.ninox_reply_drawer.shows_thinking() && !state.reduce_motion
    }

    /// Theme accessor for the iced `.theme()` builder.
    pub fn theme(_state: &Self) -> Theme {
        ColorScheme::iced_theme()
    }
}

/// The Settings Fleet card's server row, the top-bar dot and the alert
/// banner all read this.
#[derive(Debug, Default, Clone)]
pub struct ServerPanel {
    /// The port this app's server lives on: `--port`, else the config's.
    /// Zero until `main` sets it, which polls nothing.
    pub port:       u16,
    /// `None` until the first poll, or when nothing answers on the port.
    pub info:       Option<ninox_core::server_control::ServerInfo>,
    pub alerts:     Vec<ninox_ui_shared::server_restart::Alert>,
    pub confirming: bool,
    pub restarting: bool,
    pub restarting_sessions: bool,
    /// The last restart's summary or error, shown under the button.
    pub result:     Option<Result<String, String>>,
}

// ---------------------------------------------------------------------------
// Orchestrator root setup
// ---------------------------------------------------------------------------

/// The platform command that opens a URL in the default browser.
fn open_url_program() -> &'static str {
    #[cfg(target_os = "macos")]
    { "open" }
    #[cfg(not(target_os = "macos"))]
    { "xdg-open" }
}

/// Whether `id` gets a session page. The prompt agent never does: it has no
/// terminal of its own, and its replies live in the prompt drawer.
fn opens_session_page(state: &App, id: &str) -> bool {
    id != ninox_core::worker_type::PROMPT_SESSION_ID
        && !state.sessions.get(id).is_some_and(ninox_core::worker_type::is_prompt_agent_session)
}

const KITTY_APP: &str = "/Applications/kitty.app";

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Pop out's launch: kitty (the user's terminal) when it's installed,
/// otherwise a `.command` script, which macOS opens in the default
/// terminal. Returns `(program, args, script body to write first)`.
fn pop_out_plan(kitty_installed: bool, exe: &str, session_id: &str, script: &str) -> (String, Vec<String>, Option<String>) {
    if kitty_installed {
        let args = ["-na", "kitty", "--args", exe, "connect", session_id].map(String::from).to_vec();
        return ("open".into(), args, None);
    }
    let body = format!("#!/bin/sh\nexec {} connect {}\n", sh_quote(exe), sh_quote(session_id));
    ("open".into(), vec![script.to_string()], Some(body))
}

/// User-initiated, so taking focus here is expected.
fn pop_out(session_id: &str) {
    let exe = match ninox_core::hooks::canonical_exe() {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(e) => {
            tracing::warn!("pop out {session_id}: cannot resolve the ninox binary: {e}");
            return;
        }
    };
    let script = std::env::temp_dir().join(format!("ninox-connect-{session_id}.command"));
    let script_str = script.to_string_lossy().into_owned();
    let (program, args, body) = pop_out_plan(std::path::Path::new(KITTY_APP).exists(), &exe, session_id, &script_str);
    if let Some(body) = body {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::write(&script, body)
            .and_then(|()| std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)))
        {
            tracing::warn!("pop out {session_id}: writing {script_str}: {e}");
            return;
        }
    }
    if let Err(e) = std::process::Command::new(program).args(args).spawn() {
        tracing::warn!("pop out {session_id}: {e}");
    }
}

/// Best browser URL for a session's tracked PR: the recorded PR's own URL
/// when GitHub enrichment has produced one, otherwise constructed from the
/// session's repo slug (works before enrichment / without a token). `None`
/// when the session has no PR, or no repo to build a URL from.
pub fn pr_url_for_session(
    prs: &HashMap<PrId, PR>,
    session: &Session,
) -> Option<String> {
    let number = session.pr_number?;
    // `prs` is keyed by bare PR number, which collides across repos — only
    // trust a row recorded for this very session.
    if let Some(pr) = session.pr_id
        .and_then(|id| prs.get(&id))
        .filter(|pr| pr.session_id == session.id)
    {
        return Some(pr.url.clone());
    }
    if session.repo.is_empty() {
        return None;
    }
    Some(format!("https://github.com/{}/pull/{number}", session.repo))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Bundle identifier notifications are attributed to. Running from inside
/// Ninox.app, use our own identifier (must match `[package.metadata.bundle]
/// identifier` in Cargo.toml); from a bare binary, borrow Terminal's — it
/// always exists, whereas an identifier unknown to Launch Services makes
/// notifications silently drop.
#[cfg(target_os = "macos")]
fn notification_sender_bundle_id(exe: &std::path::Path) -> &'static str {
    if exe.components().any(|c| c.as_os_str() == "Ninox.app") {
        "com.madebymoonlight.ninox"
    } else {
        "com.apple.Terminal"
    }
}

#[cfg(test)]
impl App {
    pub fn update(self, message: Message) -> (Self, Task<Message>) {
        let mut state = self;
        let task = Self::apply_settled(&mut state, message);
        (state, task)
    }
}

/// The startup offer to restore an interrupted fleet.
fn restore_offer(summary: &ninox_core::fleet::startup::RestoreSummary) -> Notification {
    let plural = |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
    Notification {
        id: "fleet-restore-pending".into(),
        kind: NotificationKind::FleetRestorePending,
        title: "Fleet was interrupted".into(),
        body: format!(
            "{} and {} are waiting to be restored.",
            plural(summary.workers, "worker"),
            plural(summary.orchestrators, "orchestrator"),
        ),
        session_id: None,
        created_at: summary.flagged_at.unwrap_or_else(|| chrono::Utc::now().timestamp_millis()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::{events::Engine, store::Store};
    use tempfile::tempdir;

    #[test]
    fn config_warning_is_set_when_the_file_is_invalid() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "not = [valid").unwrap();
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(&path);
            let warning = ninox_core::config::AppConfig::load_detailed()
                .err()
                .map(|e| e.banner_message());
            assert!(warning.is_some());
            assert!(warning.unwrap().contains("Running with defaults"));
        }
    }

    #[tokio::test]
    async fn coalesces_buffered_client_output_for_the_same_generation() {
        // Send three chunks before ever polling — all three are already
        // sitting in the channel by the time `next_coalesced_event` looks,
        // so one call must return them merged into a single event rather
        // than requiring three separate (and three separately rendered)
        // frames.
        let (tx, mut rx) = broadcast::channel(16);
        let mut pending = std::collections::VecDeque::new();
        tx.send(Event::ClientOutput { session_id: "s1".into(), generation: 1, bytes: b"foo".to_vec() }).unwrap();
        tx.send(Event::ClientOutput { session_id: "s1".into(), generation: 1, bytes: b"bar".to_vec() }).unwrap();
        tx.send(Event::ClientOutput { session_id: "s1".into(), generation: 1, bytes: b"baz".to_vec() }).unwrap();

        match next_coalesced_event(&mut rx, &mut pending).await {
            Some(Event::ClientOutput { bytes, .. }) => assert_eq!(bytes, b"foobarbaz".to_vec()),
            other => panic!("expected merged ClientOutput, got {other:?}"),
        }

        drop(tx);
        assert!(next_coalesced_event(&mut rx, &mut pending).await.is_none());
    }

    #[tokio::test]
    async fn does_not_merge_client_output_across_different_generations() {
        // A stale client's trailing output must never be glued onto a
        // fresh client's — same reasoning as the `current != Some(generation)`
        // guard in the `ClientOutput` handler.
        let (tx, mut rx) = broadcast::channel(16);
        let mut pending = std::collections::VecDeque::new();
        tx.send(Event::ClientOutput { session_id: "s1".into(), generation: 1, bytes: b"old".to_vec() }).unwrap();
        tx.send(Event::ClientOutput { session_id: "s1".into(), generation: 2, bytes: b"new".to_vec() }).unwrap();

        let first = next_coalesced_event(&mut rx, &mut pending).await.unwrap();
        assert!(matches!(&first, Event::ClientOutput { generation: 1, bytes, .. } if bytes == b"old"));

        let second = next_coalesced_event(&mut rx, &mut pending).await.unwrap();
        assert!(matches!(&second, Event::ClientOutput { generation: 2, bytes, .. } if bytes == b"new"));
    }

    #[tokio::test]
    async fn does_not_merge_client_output_across_different_sessions() {
        let (tx, mut rx) = broadcast::channel(16);
        let mut pending = std::collections::VecDeque::new();
        tx.send(Event::ClientOutput { session_id: "s1".into(), generation: 1, bytes: b"a".to_vec() }).unwrap();
        tx.send(Event::ClientOutput { session_id: "s2".into(), generation: 1, bytes: b"b".to_vec() }).unwrap();

        let first = next_coalesced_event(&mut rx, &mut pending).await.unwrap();
        assert!(matches!(&first, Event::ClientOutput { session_id, bytes, .. } if session_id == "s1" && bytes == b"a"));

        let second = next_coalesced_event(&mut rx, &mut pending).await.unwrap();
        assert!(matches!(&second, Event::ClientOutput { session_id, bytes, .. } if session_id == "s2" && bytes == b"b"));
    }

    #[test]
    fn folds_repeated_session_updates_per_id_and_keeps_order() {
        let mut a1 = live_session("a");
        a1.cost_usd = 1.0;
        let mut b1 = live_session("b");
        b1.cost_usd = 5.0;
        let mut a2 = live_session("a");
        a2.cost_usd = 2.0;
        a2.status = SessionStatus::Done;
        let out = coalesce_events([
            Event::SessionUpdated(a1, SessionFields::COST | SessionFields::STATUS),
            Event::SessionUpdated(b1, SessionFields::COST),
            Event::SessionUpdated(a2, SessionFields::COST),
        ]);
        assert_eq!(out.len(), 2);
        match (&out[0], &out[1]) {
            (Event::SessionUpdated(a, af), Event::SessionUpdated(b, _)) => {
                assert_eq!((a.id.as_str(), b.id.as_str()), ("a", "b"));
                assert_eq!(a.cost_usd, 2.0, "later cost wins");
                assert_eq!(a.status, live_session("a").status, "status comes from the earlier emitter, which owned it");
                assert!(af.contains(SessionFields::STATUS | SessionFields::COST));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn session_updates_do_not_fold_across_other_events() {
        let out = coalesce_events([
            Event::SessionUpdated(live_session("a"), SessionFields::COST),
            Event::SessionDone("a".into()),
            Event::SessionUpdated(live_session("a"), SessionFields::COST),
        ]);
        assert_eq!(out.len(), 3);
    }

    #[tokio::test]
    async fn drains_a_buffered_burst_into_one_event_per_session() {
        let (tx, mut rx) = broadcast::channel(256);
        let mut pending = std::collections::VecDeque::new();
        for _ in 0..50 {
            for id in ["a", "b", "c"] {
                tx.send(Event::SessionUpdated(live_session(id), SessionFields::ACTIVITY)).unwrap();
            }
        }
        let mut seen = Vec::new();
        for _ in 0..3 {
            match next_coalesced_event(&mut rx, &mut pending).await {
                Some(Event::SessionUpdated(s, _)) => seen.push(s.id),
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(seen, ["a", "b", "c"]);
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn events_arriving_during_a_backlog_leave_the_channel() {
        let (tx, mut rx) = broadcast::channel(4);
        let mut pending = std::collections::VecDeque::new();
        for id in ["a", "b", "c"] {
            tx.send(Event::SessionUpdated(live_session(id), SessionFields::COST)).unwrap();
        }
        assert!(matches!(next_coalesced_event(&mut rx, &mut pending).await, Some(Event::SessionUpdated(s, _)) if s.id == "a"));
        for i in 0..3 {
            tx.send(Event::SessionDone(format!("d{i}"))).unwrap();
        }
        assert!(matches!(next_coalesced_event(&mut rx, &mut pending).await, Some(Event::SessionUpdated(s, _)) if s.id == "b"));
        assert!(matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)), "fresh events must be pulled off the ring");
        assert_eq!(pending.len(), 4);
    }

    #[test]
    fn sync_db_sessions_reports_only_real_changes() {
        let mut known: HashMap<SessionId, Session> = HashMap::new();
        let s = live_session("s1");
        assert_eq!(sync_db_sessions(&mut known, vec![s.clone()]), 1, "new session");
        assert_eq!(sync_db_sessions(&mut known, vec![s.clone()]), 0, "identical store row is a no-op");
        let mut with_pr = s.clone();
        with_pr.pr_number = Some(9);
        with_pr.pr_id = Some(9);
        with_pr.repo = "o/r".into();
        assert_eq!(sync_db_sessions(&mut known, vec![with_pr]), 1, "daemon-written PR link adopted");
        assert_eq!(known["s1"].pr_number, Some(9));
    }

    fn pr_session(pr_number: Option<u64>, pr_id: Option<i64>, repo: &str) -> Session {
        Session {
            id: "s1".into(), orchestrator_id: None, name: "w".into(),
            repo: repo.into(), status: ninox_core::types::SessionStatus::PrOpen,
            agent_type: "claude-code".into(), cost_usd: 0.0, started_at: 0,
            pr_number, pr_id, workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn notification_sender_is_ninox_when_bundled_else_terminal() {
        use std::path::Path;
        assert_eq!(
            notification_sender_bundle_id(Path::new(
                "/Applications/Ninox.app/Contents/MacOS/ninox-app"
            )),
            "com.madebymoonlight.ninox",
        );
        assert_eq!(
            notification_sender_bundle_id(Path::new("/Users/me/ninox/target/debug/ninox")),
            "com.apple.Terminal",
        );
    }

    #[test]
    fn pr_url_for_session_prefers_the_recorded_pr_url() {
        let mut prs = HashMap::new();
        prs.insert(7i64, ninox_core::types::PR {
            id: 7, number: 7, title: "t".into(),
            url: "https://github.com/org/repo/pull/7".into(),
            body: String::new(), session_id: "s1".into(),
        });
        let session = pr_session(Some(7), Some(7), "org/repo");
        assert_eq!(
            pr_url_for_session(&prs, &session).as_deref(),
            Some("https://github.com/org/repo/pull/7"),
        );
    }

    /// Before the poller's GitHub enrichment has run (or with no token at
    /// all) there is no PR record — the link must still work, constructed
    /// from the session's repo slug.
    #[test]
    fn pr_url_for_session_falls_back_to_repo_slug() {
        let prs = HashMap::new();
        let session = pr_session(Some(9), None, "org/repo");
        assert_eq!(
            pr_url_for_session(&prs, &session).as_deref(),
            Some("https://github.com/org/repo/pull/9"),
        );
    }

    /// `prs` is keyed by bare PR number, which collides across repos — a
    /// row recorded for another session must not serve its URL here; the
    /// repo-slug fallback is the correct answer.
    #[test]
    fn pr_url_for_session_ignores_another_sessions_colliding_pr_row() {
        let mut prs = HashMap::new();
        prs.insert(5i64, ninox_core::types::PR {
            id: 5, number: 5, title: "other".into(),
            url: "https://github.com/org/other-repo/pull/5".into(),
            body: String::new(), session_id: "someone-else".into(),
        });
        let session = pr_session(Some(5), Some(5), "org/repo");
        assert_eq!(
            pr_url_for_session(&prs, &session).as_deref(),
            Some("https://github.com/org/repo/pull/5"),
        );
    }

    #[test]
    fn pr_url_for_session_is_none_without_pr_or_repo() {
        let prs = HashMap::new();
        assert_eq!(pr_url_for_session(&prs, &pr_session(None, None, "org/repo")), None);
        assert_eq!(pr_url_for_session(&prs, &pr_session(Some(9), None, "")), None);
    }

    /// The browser-open command must exist per platform — `open` is
    /// macOS-only; Linux needs `xdg-open` (the README supports both).
    #[test]
    fn open_url_program_matches_platform() {
        #[cfg(target_os = "macos")]
        assert_eq!(open_url_program(), "open");
        #[cfg(target_os = "linux")]
        assert_eq!(open_url_program(), "xdg-open");
    }

    fn test_engine() -> Arc<Engine> {
        let s = Arc::new(
            Store::open(tempdir().unwrap().keep().join("t.db")).unwrap(),
        );
        Engine::new(s)
    }

    fn base(engine: Arc<Engine>) -> App {
        let brain = Arc::new(BrainIndex::open(tempdir().unwrap().keep()).unwrap());
        base_with_brain(engine, brain)
    }

    fn base_with_brain(engine: Arc<Engine>, brain: Arc<BrainIndex>) -> App {
        App {
            engine,
            config:             AppConfig::default(),
            config_warning:     None,
            themes:             Themes::builtin(),
            scheme:             crate::theme::from_variant(ThemeVariant::Dark),
            active_variant:     ThemeVariant::Dark,
            orchestrator_root:  std::path::PathBuf::from("/tmp"),
            db_path:            std::path::PathBuf::from("/tmp/t.db"),
            orchestrators:      vec![],
            sessions:       HashMap::new(),
            queue_positions: HashMap::new(),
            limits_headline: String::new(),
            brain:          BrainScope::Single(brain),
            brain_view:     BrainViewState::default(),
            catalogues:      vec![ninox_core::config::CatalogueRef {
                name: "default".to_string(),
                path: std::path::PathBuf::new(),
                remote: None,
                endpoint: None,
                region: None,
                cache_ttl_secs: None,
                enabled: true,
                cadence: "manual".to_string(),
                access: "read_write".to_string(),
            }],
            active_catalogue: Some(0),
            prs:            HashMap::new(),
            ci_status:      HashMap::new(),
            session_prs:    HashMap::new(),
            github_budget:  None,
            store_poll_tick: 0,
            review_threads: HashMap::new(),
            comment_editors: HashMap::new(),
            diffs:          HashMap::new(),
            plan_docs:      HashMap::new(),
            notifications:  VecDeque::new(),
            notif_cursor:   0,
            alerted:        Vec::new(),
            pending_user_requests: Vec::new(),
            orch_plans: Default::default(),
            ghost: Default::default(),
            human_console_error: None,
            user_request_error: None,
            unread_messages: HashMap::new(),
            held_messages: HashMap::new(),
            queued_by_sender: HashMap::new(),
            update_in_progress: false,
            version_check:  VersionCheckState::NotChecked,
            restart_all_in_progress: false,
            server: ServerPanel::default(),
            sidebar:        SidebarState::default(),
            view:           View::default(),
            worker_panel:   Default::default(),
            terminals:      HashMap::new(),
            transcripts:    HashMap::new(),
            clients:        HashMap::new(),
            attach: HashMap::new(),
            next_attach_token: 0,
            reap_pending: HashMap::new(),
            next_client_generation: 0,
            model_lists:    HashMap::new(),
            settings:       Default::default(),
            spawn_modal:    None,
            spawn_prompt:   iced::widget::text_editor::Content::new(),
            help_open:      false,
            catalogue_modal: None,
            sources_edit: None,
            source_status: HashMap::new(),
            source_sync_log: VecDeque::new(),
            restart_all_confirm: false,
            restore_modal: None,
            reap_modal: None,
            delete_modal: None,
            terminal_cols:  140,
            terminal_rows:  50,
            window_width:   0.0,
            traffic_lights_end: None,
            window_height:  0.0,
            sidebar_width:  0.0,
            info_width:     0.0,
            terminal_focused: false,
            terminal_click_marker: false,
            session_more_open: false,
            poll_tick: 0,
            session_more_hover: false,
            session_confirm: None,
            workspace_facts: HashMap::new(),
            session_activity: HashMap::new(),
            drag:            None,
            fleet_filter:    FleetFilter::default(),
            last_fleet_scope: None,
            global_search:   GlobalSearchState::default(),
            zoom:            1.0,
            ninox_prompt_draft: String::new(),
            fleet_sort:            ninox_overview::islands::IslandSort::NeedsYouFirst,
            fleet_machine_filter:  FleetMachineFilter::default(),
            fleet_compact:         true,
            fleet_expanded_folds:  std::collections::HashSet::new(),
            fleet_nav:             ninox_overview::nav::FleetNav::default(),
            nav:                   Default::default(),
            nav_auto:              false,
            fleet_last_island_click: None,
            fleet_last_node_click: None,
            fleet_message_draft: None,
            worker_types: crate::components::worker_types::WorkerTypesState::default(),
            remotes: crate::components::remotes::RemotesState::default(),
            idle: crate::components::idle_view::IdleState::default(),
            ninox_prompt_target: crate::prompt_agent::PromptTarget::default(),
            ninox_reply_drawer: crate::prompt_agent::ReplyDrawer::default(),
            ninox_turn_select: HashMap::new(),
            thinking_tick: 0,
            reduce_motion: false,
            ninox_prompt_generation: 0,
            ninox_prompt_history: Vec::new(),
            ninox_prompt_history_cursor: None,
            ninox_prompt_draft_before_recall: None,
            workspaces_summary: (0, 0),
            workspaces_last_refresh: None,
            workspaces_modal: None,
        }
    }

    #[test]
    fn answer_user_request_message_calls_store_and_clears_pending_entry() {
        let e = test_engine();
        ninox_core::human_console_auth::register_human_console(&e.store, "test").unwrap();
        e.store.insert_user_request(&UserRequestRow {
            id: "ur-1".into(),
            requester_session_id: "s1".into(),
            prompt: "merge now?".into(),
            mode: UserRequestMode::Sync,
            status: UserRequestStatus::Pending,
            answer: None,
            answered_at: None,
            answer_delivered_at: None,
            created_at: 0,
        }).unwrap();
        let mut m = base(e.clone());
        m.pending_user_requests = e.store.open_user_requests(None).unwrap();
        assert!(m.pending_user_requests.iter().any(|r| r.id == "ur-1"));

        let (m, _) = m.update(Message::AnswerUserRequest { id: "ur-1".into(), answer: "approved".into() });

        let row = e.store.get_user_request("ur-1").unwrap().expect("row still exists");
        assert_eq!(row.status, UserRequestStatus::Answered);
        assert_eq!(row.answer.as_deref(), Some("approved"));
        assert!(!m.pending_user_requests.iter().any(|r| r.id == "ur-1"));
    }

    #[test]
    fn cancel_user_request_message_cancels_never_answers() {
        let e = test_engine();
        ninox_core::human_console_auth::register_human_console(&e.store, "test").unwrap();
        e.store.insert_user_request(&UserRequestRow {
            id: "ur-stale".into(),
            requester_session_id: "s1".into(),
            prompt: "merge now?".into(),
            mode: UserRequestMode::Async,
            status: UserRequestStatus::Pending,
            answer: None,
            answered_at: None,
            answer_delivered_at: None,
            created_at: 0,
        }).unwrap();
        let mut m = base(e.clone());
        m.pending_user_requests = e.store.open_user_requests(None).unwrap();

        let (m, _) = m.update(Message::CancelUserRequest("ur-stale".into()));

        let row = e.store.get_user_request("ur-stale").unwrap().expect("row still exists");
        assert_eq!(row.status, UserRequestStatus::Cancelled);
        assert_eq!(row.answer, None, "dismissal must never answer the request");
        assert!(!m.pending_user_requests.iter().any(|r| r.id == "ur-stale"));
    }

    fn stale_user_request(id: &str, requester: &str) -> UserRequestRow {
        UserRequestRow {
            id: id.into(),
            requester_session_id: requester.into(),
            prompt: "merge now?".into(),
            mode: UserRequestMode::Async,
            status: UserRequestStatus::Pending,
            answer: None,
            answered_at: None,
            answer_delivered_at: None,
            created_at: 0,
        }
    }

    #[test]
    fn stale_row_is_shown_with_a_dismiss_badge_not_hidden() {
        use crate::components::pending_requests_panel::{pending_requests_panel, requester_is_stale};
        let e = test_engine();
        let mut m = base(e);
        let mut dead = refile_session("s1");
        dead.status = SessionStatus::Terminated;
        m.sessions.insert("s1".into(), dead);
        m.pending_user_requests = vec![stale_user_request("ur-1", "s1")];

        // Rendering must not panic, and the row the panel iterates over
        // must still include the stale one — nothing filters it out before
        // the human explicitly dismisses it.
        let _ = pending_requests_panel(&m);
        assert_eq!(m.pending_user_requests.len(), 1, "a stale row must still be present, not hidden");
        assert!(requester_is_stale(&m, &m.pending_user_requests[0]), "its requester session must register as independently confirmed terminal");
    }

    #[test]
    fn a_pending_row_for_a_live_session_has_no_dismiss_affordance() {
        use crate::components::pending_requests_panel::requester_is_stale;
        let e = test_engine();
        let mut m = base(e);
        let mut alive = refile_session("s1");
        alive.status = SessionStatus::Working;
        m.sessions.insert("s1".into(), alive);
        m.pending_user_requests = vec![stale_user_request("ur-1", "s1")];

        assert!(
            !requester_is_stale(&m, &m.pending_user_requests[0]),
            "a live requester must gate off the dismiss affordance",
        );

        // A requester purged from the store entirely can never receive an
        // answer either — the row must be dismissable, not stuck forever.
        m.sessions.clear();
        assert!(requester_is_stale(&m, &m.pending_user_requests[0]));
    }

    #[test]
    fn an_empty_answer_is_never_submitted() {
        let e = test_engine();
        ninox_core::human_console_auth::register_human_console(&e.store, "test").unwrap();
        e.store.insert_user_request(&stale_user_request("ur-1", "s1")).unwrap();
        let mut m = base(e.clone());
        m.pending_user_requests = e.store.open_user_requests(None).unwrap();

        for blank in ["", "   "] {
            let (next, _) = m.update(Message::AnswerUserRequest { id: "ur-1".into(), answer: blank.into() });
            m = next;
        }

        assert_eq!(e.store.get_user_request("ur-1").unwrap().unwrap().status, UserRequestStatus::Pending);
        assert_eq!(m.pending_user_requests.len(), 1);
    }

    #[test]
    fn a_failed_answer_is_surfaced_in_the_panel_until_the_next_success() {
        let e = test_engine();
        e.store.insert_user_request(&stale_user_request("ur-1", "s1")).unwrap();
        let mut m = base(e.clone());
        m.pending_user_requests = e.store.open_user_requests(None).unwrap();

        // No console registered: the store refuses the answer.
        let (m, _) = m.update(Message::AnswerUserRequest { id: "ur-1".into(), answer: "yes".into() });
        let err = m.user_request_error.clone().expect("a refused answer must not fail silently");
        assert!(err.contains("ur-1"), "{err}");
        let _ = crate::components::pending_requests_panel::pending_requests_panel(&m);

        ninox_core::human_console_auth::register_human_console(&e.store, "test").unwrap();
        let (m, _) = m.update(Message::AnswerUserRequest { id: "ur-1".into(), answer: "yes".into() });
        assert!(m.user_request_error.is_none());
    }

    #[test]
    fn pending_requests_badge_always_renders_in_the_nav_bar() {
        assert_eq!(pending_requests_badge(0), None);
        assert_eq!(
            pending_requests_badge(1),
            Some(("1".to_string(), "1 pending user request — click to show".to_string())),
        );
        assert_eq!(pending_requests_badge(150).unwrap().0, "99+");

        // The nav bar (and its pending-requests badge) renders on every
        // view — no `sidebar_hidden`/collapsed-tree state can hide it.
        let mut m = base(test_engine());
        m.pending_user_requests = vec![stale_user_request("ur-1", "s1")];
        let _ = App::iced_view(&m);
    }

    /// A worker row (attached to an orchestrator). Set `orchestrator_id`
    /// to `None` for a standalone session.
    fn refile_session(id: &str) -> Session {
        Session {
            id: id.into(), orchestrator_id: Some("orch".into()), name: id.into(), repo: String::new(),
            status: SessionStatus::Terminated, agent_type: "claude-code".into(),
            cost_usd: 0.0, started_at: 0, pr_number: None, pr_id: None,
            workspace_path: Some("/tmp/ws".into()), pid: None,
            model: Some("claude-opus-4-8".into()), context_tokens: None,
            brain_reads: Vec::new(), brain_write: Some("/brains/b".into()),
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        }
    }

    #[test]
    fn refile_message_drops_client_state_and_keeps_session() {
        let e = test_engine();
        let mut m = base(e);
        let s = refile_session("s1");
        m.sessions.insert("s1".into(), s);
        m.terminals.insert("s1".into(), TerminalState::new(80, 24, None));
        let (m, _) = m.update(Message::RefileSession("s1".into()));
        // The stale grid is dropped so the respawned PTY starts clean; the
        // session record itself stays (the respawn upserts it to Working).
        assert!(!m.terminals.contains_key("s1"));
        assert!(m.sessions.contains_key("s1"));
    }

    #[test]
    fn refile_message_drops_the_cached_diff_from_the_prior_incarnation() {
        let e = test_engine();
        let mut m = base(e);
        let s = refile_session("s1");
        m.sessions.insert("s1".into(), s);
        m.diffs.insert("s1".into(), Some("stale diff text".into()));
        let (m, _) = m.update(Message::RefileSession("s1".into()));
        assert!(!m.diffs.contains_key("s1"), "re-file must not leave the old workspace's diff cached");
    }

    #[test]
    fn resume_message_relaunches_and_attaches() {
        let _iso = ninox_core::test_isolation::isolate();
        let e = test_engine();
        let mut m = base(e);
        let mut s = refile_session("s1");
        s.status = SessionStatus::Interrupted;
        s.claude_session_id = Some("stored-uuid".into());
        m.sessions.insert("s1".into(), s.clone());
        m.engine.store.upsert_session(&s).unwrap();
        m.terminals.insert("s1".into(), TerminalState::new(80, 24, None));
        let (m, _) = m.update(Message::ResumeSession("s1".into()));
        // Mirrors `refile_message_drops_client_state_and_keeps_session`:
        // the stale grid is dropped so the respawned PTY starts clean; the
        // session record itself stays (the respawn upserts it to Working).
        // Actually executing the returned `Task` needs a real async
        // executor driving a real tmux server — that end-to-end behavior is
        // covered by `resume_message_keeps_status_interrupted_when_tmux_create_fails`
        // below; this test only checks the synchronous state `update()`
        // itself mutates, same as its Re-file twin.
        assert!(!m.terminals.contains_key("s1"));
        assert!(m.sessions.contains_key("s1"));
    }

    fn daemon_note(id: &str) -> Notification {
        Notification {
            id: id.into(),
            kind: NotificationKind::CiFailure,
            title: format!("t-{id}"),
            body: String::new(),
            session_id: None,
            // Store assigns the current time, so retention pruning cannot
            // discard this fixture before the notification is polled.
            created_at: 0,
        }
    }

    #[test]
    fn a_notification_the_daemon_stored_shows_on_the_next_poll_and_alerts_once() {
        let _iso = ninox_core::test_isolation::isolate();
        let m = base(test_engine());
        m.engine.store.insert_notification(&daemon_note("a")).unwrap();
        let (m, _) = m.update(Message::PollSessions);
        assert_eq!(m.notifications.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(), ["a"]);
        assert_eq!(m.alerted, ["a"]);
        let (m, _) = m.update(Message::PollSessions);
        assert_eq!(m.alerted, ["a"], "a re-read doesn't alert again");
        assert_eq!(m.notifications.len(), 1, "nor duplicate the row");
    }

    #[test]
    fn a_repeat_of_a_still_showing_notification_alerts_again_without_duplicating() {
        let _iso = ninox_core::test_isolation::isolate();
        let m = base(test_engine());
        m.engine.store.insert_notification(&daemon_note("ci-s1")).unwrap();
        let (m, _) = m.update(Message::PollSessions);
        m.engine.store.insert_notification(&daemon_note("ci-s1")).unwrap();
        let (m, _) = m.update(Message::PollSessions);
        assert_eq!(m.alerted, ["ci-s1", "ci-s1"]);
        assert_eq!(m.notifications.len(), 1);
    }

    #[test]
    fn an_applied_update_doesnt_bring_the_update_available_row_back() {
        let _iso = ninox_core::test_isolation::isolate();
        let m = base(test_engine());
        let mut n = daemon_note("update-available-1.2.3");
        n.kind = NotificationKind::UpdateAvailable;
        m.engine.store.insert_notification(&n).unwrap();
        let (m, _) = m.update(Message::PollSessions);
        let (m, _) = m.update(Message::UpdateApplied(Ok(())));
        let (m, _) = m.update(Message::PollSessions);
        assert!(m.notifications.iter().all(|n| n.kind != NotificationKind::UpdateAvailable));
        assert!(m.notifications.iter().any(|n| n.kind == NotificationKind::UpdateInstalled));
    }

    #[test]
    fn the_backlog_at_startup_fills_the_panel_without_alerting() {
        let _iso = ninox_core::test_isolation::isolate();
        let e = test_engine();
        e.store.insert_notification(&daemon_note("old")).unwrap();
        let mut m = base(e);
        App::sync_notifications(&mut m, false);
        assert_eq!(m.notifications.len(), 1);
        assert!(m.alerted.is_empty());
        let (m, _) = m.update(Message::PollSessions);
        assert!(m.alerted.is_empty(), "the backlog stays quiet on later ticks too");
        m.engine.store.insert_notification(&daemon_note("new")).unwrap();
        let (m, _) = m.update(Message::PollSessions);
        assert_eq!(m.alerted, ["new"]);
    }

    #[test]
    fn dismissals_persist_and_the_same_condition_recurring_alerts_again() {
        let _iso = ninox_core::test_isolation::isolate();
        let m = base(test_engine());
        m.engine.store.insert_notification(&daemon_note("a")).unwrap();
        m.engine.store.insert_notification(&daemon_note("b")).unwrap();
        let (m, _) = m.update(Message::PollSessions);
        let (m, _) = m.update(Message::DismissNotification("a".into()));
        let (m, _) = m.update(Message::PollSessions);
        assert_eq!(m.notifications.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(), ["b"]);
        m.engine.store.insert_notification(&daemon_note("a")).unwrap();
        let (m, _) = m.update(Message::PollSessions);
        assert_eq!(m.alerted, ["a", "b", "a"]);
        let (m, _) = m.update(Message::DismissAllNotifications);
        let (m, _) = m.update(Message::PollSessions);
        assert!(m.notifications.is_empty());
        assert!(m.engine.store.undismissed_notifications().unwrap().is_empty());
    }

    #[test]
    fn with_no_daemon_the_guis_own_emit_is_shown_once() {
        let _iso = ninox_core::test_isolation::isolate();
        let m = base(test_engine());
        let n = daemon_note("a");
        m.engine.emit(Event::Notification(n.clone()));
        let (m, _) = m.update(Message::EngineEvent(Box::new(Event::Notification(n))));
        let (m, _) = m.update(Message::PollSessions);
        assert_eq!(m.notifications.len(), 1);
        assert_eq!(m.alerted, ["a"]);
    }

    #[test]
    fn resume_at_the_session_limit_queues_instead_of_tearing_down() {
        let _iso = ninox_core::test_isolation::isolate();
        AppConfig::update(|c| c.limits.set_limit(None, Some(1))).unwrap();
        let e = test_engine();
        let mut m = base(e);
        let mut busy = refile_session("busy");
        busy.status = SessionStatus::Working;
        m.engine.store.upsert_session(&busy).unwrap();
        let mut s = refile_session("s1");
        s.status = SessionStatus::Interrupted;
        s.claude_session_id = Some("stored-uuid".into());
        m.sessions.insert("s1".into(), s.clone());
        m.engine.store.upsert_session(&s).unwrap();
        m.terminals.insert("s1".into(), TerminalState::new(80, 24, None));
        let (m, _) = m.update(Message::ResumeSession("s1".into()));
        assert!(m.engine.store.is_waiting_in_queue("s1").unwrap());
        assert_eq!(m.sessions["s1"].status, SessionStatus::Queued);
        assert!(m.terminals.contains_key("s1"), "nothing torn down for a queued resume");
    }

    #[test]
    fn resume_message_drops_the_cached_diff_from_before_the_pane_died() {
        let _iso = ninox_core::test_isolation::isolate();
        let e = test_engine();
        let mut m = base(e);
        let mut s = refile_session("s1");
        s.status = SessionStatus::Interrupted;
        s.claude_session_id = Some("stored-uuid".into());
        m.sessions.insert("s1".into(), s.clone());
        m.engine.store.upsert_session(&s).unwrap();
        m.diffs.insert("s1".into(), Some("stale diff text".into()));
        let (m, _) = m.update(Message::ResumeSession("s1".into()));
        assert!(!m.diffs.contains_key("s1"), "resume must not leave the pre-resume diff cached");
    }

    #[tokio::test]
    async fn resume_message_keeps_status_interrupted_when_tmux_create_fails() {
        let _iso = ninox_core::test_isolation::isolate();
        // Driving `Message::ResumeSession`'s `Task` to completion needs a
        // real async executor — no other test in this file does that for a
        // `Task<Message>` (the closest sibling, `stale_client_closed_...`,
        // drives real tmux calls directly, not the `Task` an `update()` call
        // returns), so this uses `iced_runtime::task::into_stream` +
        // `futures::StreamExt` to run it for real, exactly the way iced's
        // own runtime would.
        //
        // Triggering a real `tmux::create_session` failure here can't reuse
        // the collision trick `spawn_util::spawn_failure_uses_the_caller_supplied_failure_status`
        // uses (pre-create a live session under the same id so `new-session`
        // collides with tmux's own "duplicate session" error): unlike that
        // test, which calls `spawn_interactive_session` directly, the real
        // `Message::ResumeSession` handler kills any live tmux session under
        // the same id *first* (`ninox_core::tmux::kill_session`) before
        // creating the new one — so a pre-created collision would just get
        // killed and the respawn would succeed. And a bad `-c <workspace>`
        // path (the brief's literal suggestion) doesn't reliably fail
        // `tmux new-session` on this machine's tmux either (3.6a returns
        // exit 0 and only the spawned shell dies asynchronously — the same
        // finding Task 5 made). Instead, a NUL byte embedded in the
        // workspace path makes the underlying `Command::spawn` fail
        // deterministically — Rust validates argv for interior NULs before
        // ever exec'ing tmux — independent of tmux version and unaffected
        // by the preceding `kill_session` call (whose own args don't
        // contain the workspace path).
        use futures::StreamExt;

        let e = test_engine();
        let mut m = base(e);
        let mut s = refile_session("s1");
        s.status = SessionStatus::Interrupted;
        s.claude_session_id = Some("stored-uuid".into());
        s.workspace_path = Some("/definitely/does/not/exist/ever\0".into());
        m.sessions.insert("s1".into(), s.clone());
        m.engine.store.upsert_session(&s).unwrap();

        let (m, task) = m.update(Message::ResumeSession("s1".into()));
        let mut stream = iced_runtime::task::into_stream(task)
            .expect("ResumeSession must produce a real Task, not Task::none()");
        // `Task::future` yields exactly one `Action::Output(message)`; drive
        // it to completion and ignore the resulting `Message` (Noop on the
        // failure path this test forces).
        let _ = stream.next().await;

        let session = m.engine.store.get_session("s1").unwrap().unwrap();
        assert!(
            matches!(session.status, SessionStatus::Interrupted),
            "a failed Resume must stay Interrupted (retryable), not fall back to Terminated",
        );
    }

    #[tokio::test]
    async fn resume_message_relaunches_a_terminated_orchestrator_end_to_end() {
        let _iso = ninox_core::test_isolation::isolate();
        // `resume_message_relaunches_and_attaches` above only checks the
        // synchronous state `update()` mutates; nothing else in this file
        // drives `Message::ResumeSession`'s `Task` to completion for a
        // Terminated *orchestrator* (as opposed to
        // `resume_message_keeps_status_interrupted_when_tmux_create_fails`,
        // which forces the failure path for a plain worker session). This
        // exercises the success path end-to-end: a persisted orchestrator
        // whose session is Terminated with a dead pid but a live workspace +
        // stored `claude_session_id` — Resume must relaunch it (ClientAttach)
        // and land on `Working`, not silently no-op.
        use futures::StreamExt;

        let e = test_engine();
        let mut m = base(e);
        // Harness override: run a harmless long-lived command instead of the
        // real `claude` binary so the pane survives the attach handshake.
        m.config.harnesses.insert("claude-code".to_string(), ninox_core::harness::HarnessSpec {
            enabled: true,
            binary: Some("sleep".into()),
            resume_args: vec!["30".into()],
            ..Default::default()
        });
        let ws = tempdir().unwrap().keep();
        let sid = "orch-resume-reboot-test";
        let orch = ninox_core::types::Orchestrator {
            id: sid.into(), name: "O".into(), created_at: 0,
        };
        m.engine.store.upsert_orchestrator(&orch).unwrap();
        m.orchestrators.push(orch);
        let mut s = refile_session(sid);
        s.status = SessionStatus::Terminated;
        s.claude_session_id = Some("stored-uuid".into());
        s.workspace_path = Some(ws.to_string_lossy().to_string());
        s.pid = Some(u32::MAX); // pane pid from before the reboot — dead
        s.terminal_at = Some(1);
        m.engine.store.upsert_session(&s).unwrap();
        m.sessions.insert(sid.into(), s);

        let (m, task) = m.update(Message::ResumeSession(sid.into()));
        let mut stream = iced_runtime::task::into_stream(task)
            .expect("ResumeSession must produce a real Task, not Task::none()");
        let out = stream.next().await;
        // Clean up the test-socket tmux session regardless of outcome.
        let _ = ninox_core::tmux::kill_session(sid).await;
        let attached = matches!(
            &out,
            Some(iced_runtime::Action::Output(Message::ClientAttach { session_id, .. }))
                if session_id == sid
        );
        assert!(
            attached,
            "resume of a terminated orchestrator must relaunch and attach; got {:?}",
            out.map(|a| match a {
                iced_runtime::Action::Output(msg) => format!("{msg:?}").chars().take(200).collect::<String>(),
                _ => "non-output action".into(),
            })
        );
        let session = m.engine.store.get_session(sid).unwrap().unwrap();
        assert!(matches!(session.status, SessionStatus::Working));
    }

    #[test]
    fn sources_location_edits_keep_a_masked_secret() {
        let mut m = base(test_engine());
        m.sources_edit = Some(ninox_ui_shared::brain_sources::SourceEditForm {
            location: "https://bob:hunter2@github.com/acme/repo/wiki".into(),
            ..Default::default()
        });
        let shown = ninox_ui_shared::brain_sources::redact_credentials(&m.sources_edit.as_ref().unwrap().location);
        assert!(!shown.contains("hunter2"), "{shown}");
        let (m, _) = m.update(Message::SourcesFormLocation(shown.replace("acme/repo", "acme/docs")));
        assert_eq!(m.sources_edit.as_ref().unwrap().location, "https://bob:hunter2@github.com/acme/docs/wiki");
    }

    #[test]
    fn sources_endpoint_edits_keep_a_masked_secret() {
        let mut m = base(test_engine());
        m.sources_edit = Some(ninox_ui_shared::brain_sources::SourceEditForm {
            endpoint: "https://key:sekrit@minio.local".into(),
            ..Default::default()
        });
        let shown = ninox_ui_shared::brain_sources::redact_credentials(&m.sources_edit.as_ref().unwrap().endpoint);
        let (m, _) = m.update(Message::SourcesFormEndpoint(format!("{shown}:9000")));
        assert_eq!(m.sources_edit.as_ref().unwrap().endpoint, "https://key:sekrit@minio.local:9000");
    }

    #[test]
    fn reap_finished_confirm_runs_once_and_refreshes_the_reaped_rows() {
        use crate::components::reap_modal::tests::worker;
        let mut m = base(test_engine());
        let w = worker("w1", "o1", SessionStatus::Done);
        m.orchestrators = vec![Orchestrator { id: "o1".into(), name: "o1".into(), created_at: 0 }];
        m.sessions.insert(w.id.clone(), w.clone());

        let (m, _) = m.update(Message::ReapFinishedOpen);
        assert_eq!(m.reap_modal.as_ref().unwrap().workers.len(), 1);
        let (m, _) = m.update(Message::ReapFinishedConfirm);
        assert!(m.reap_modal.as_ref().unwrap().running);
        let (m, _) = m.update(Message::ReapFinishedClose);
        assert!(m.reap_modal.is_some(), "a running clean-up can't be dismissed");

        let mut cleaned = w;
        cleaned.cleanup_at = Some(5);
        m.engine.store.upsert_session(&cleaned).unwrap();
        let (m, _) = m.update(Message::ReapFinishedDone { reaped: vec!["w1".into()], error: None });
        assert_eq!(m.sessions["w1"].cleanup_at, Some(5), "the row is re-read from the store");
        let modal = m.reap_modal.as_ref().unwrap();
        assert!(!modal.running);
        assert_eq!(modal.outcome, Some(Ok("Cleaned up 1 finished worker.".into())));
        assert!(crate::components::reap_modal::reapable(&m.sessions, &m.orchestrators).is_empty());
        let (m, _) = m.update(Message::ReapFinishedClose);
        assert!(m.reap_modal.is_none());
    }

    #[test]
    fn reap_finished_failure_is_reported() {
        let m = base(test_engine());
        let (m, _) = m.update(Message::ReapFinishedOpen);
        let (m, _) = m.update(Message::ReapFinishedDone { reaped: vec!["w9".into()], error: Some("o1: store locked".into()) });
        assert_eq!(
            m.reap_modal.unwrap().outcome,
            Some(Err("o1: store locked. Cleaned up 1 finished worker.".into())),
            "workers reaped before the failure are still reported",
        );
    }

    #[test]
    fn acp_transcript_entries_accumulate_per_session_and_survive_done() {
        use ninox_core::runtime::TranscriptEntry;
        let m = base(test_engine());
        let ev = |id: &str, e: TranscriptEntry| Message::EngineEvent(Box::new(Event::SessionTranscript { session_id: id.into(), entry: e }));
        let (m, _) = m.update(ev("s1", TranscriptEntry::UserMessage("hi".into())));
        let (m, _) = m.update(ev("s1", TranscriptEntry::TurnStarted));
        let (m, _) = m.update(ev("s2", TranscriptEntry::AgentText("other".into())));
        assert_eq!(m.transcripts["s1"], vec![TranscriptEntry::UserMessage("hi".into()), TranscriptEntry::TurnStarted]);
        assert_eq!(m.transcripts["s2"].len(), 1);
        let (m, _) = m.update(Message::EngineEvent(Box::new(Event::SessionDone("s1".into()))));
        assert_eq!(m.transcripts["s1"].len(), 2, "an ended session's transcript stays readable, as in the TUI");
    }

    #[test]
    fn an_acp_session_never_captures_the_keyboard() {
        use ninox_core::runtime::TranscriptEntry;
        let mut m = on_session_page(DetailPanel::Terminal);
        m.transcripts.insert("w1".into(), vec![TranscriptEntry::AgentText("hi".into())]);
        let mut m = m;
        m.view = View::SessionDetail { session_id: "w1".into(), panel: DetailPanel::Info };
        let (m, _) = m.update(Message::TerminalFocus);
        assert!(!m.terminal_focused, "c must not focus a pty that doesn't exist");
        assert!(matches!(m.view, View::SessionDetail { panel: DetailPanel::Info, .. }), "c is a no-op, not a panel switch");
        let mut m = m;
        m.view = View::SessionDetail { session_id: "w1".into(), panel: DetailPanel::Terminal };
        let (mut m, _) = m.update(Message::TerminalClicked);
        assert!(!m.terminal_focused);
        // Focus set before the first entry arrived doesn't capture keys either.
        m.terminal_focused = true;
        assert!(!App::terminal_owns_keys(&m));
    }

    #[test]
    fn removing_an_orchestrator_drops_its_workers_transcripts() {
        use ninox_core::runtime::TranscriptEntry;
        let mut m = fleet_test_app("o1", "w1");
        m.transcripts.insert("w1".into(), vec![TranscriptEntry::TurnStarted]);
        let (m, _) = m.update(Message::EngineEvent(Box::new(Event::OrchestratorRemoved("o1".into()))));
        assert!(!m.transcripts.contains_key("w1"));
    }

    fn brain_with(files: &[(&str, &str)]) -> App {
        let dir = tempdir().unwrap().keep();
        for (rel, body) in files {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        let brain = Arc::new(BrainIndex::open(&dir).unwrap());
        brain.rebuild(None).unwrap();
        let (m, _) = base_with_brain(test_engine(), brain).update(Message::NavigateBrain);
        m
    }

    fn linked_brain() -> App {
        brain_with(&[
            ("concepts/hub.md", "---\nname: Hub\n---\n[[a]] [[b]] [[c]]\n"),
            ("concepts/a.md", "---\nname: a\n---\n[[hub]]\n"),
            ("concepts/b.md", "---\nname: b\n---\nbody\n"),
            ("concepts/c.md", "---\nname: c\n---\nbody\n"),
            ("concepts/lonely.md", "---\nname: Lonely\n---\nbody\n"),
        ])
    }

    #[test]
    fn opening_graph_focuses_the_list_selection() {
        let _iso = ninox_core::test_isolation::isolate();
        let m = linked_brain();
        let (m, _) = m.update(Message::BrainSelectEntry("concepts/a.md".into()));
        let (m, _) = m.update(Message::BrainSetMode(BrainMode::Graph));
        let g = &m.brain_view.graph;
        let a = m.brain_view.entries.iter().position(|e| e.id == "concepts/a.md");
        assert_eq!(g.focus, a);
        assert!(!g.nodes.is_empty(), "the neighbourhood is drawn, not '0 of N shown'");
    }

    #[test]
    fn with_no_selection_graph_focuses_the_most_linked_entry() {
        let _iso = ninox_core::test_isolation::isolate();
        let mut m = linked_brain();
        m.brain_view.selected = None;
        let (m, _) = m.update(Message::BrainSetMode(BrainMode::Graph));
        let hub = m.brain_view.entries.iter().position(|e| e.id == "concepts/hub.md");
        assert_eq!(m.brain_view.graph.focus, hub);
        assert!(m.brain_view.graph.nodes.len() >= 4);
    }

    #[test]
    fn a_brain_without_links_shows_the_pick_an_entry_state() {
        let _iso = ninox_core::test_isolation::isolate();
        let mut m = brain_with(&[("concepts/x.md", "---\nname: X\n---\nbody\n"), ("concepts/y.md", "---\nname: Y\n---\nbody\n")]);
        m.brain_view.selected = None;
        let (m, _) = m.update(Message::BrainSetMode(BrainMode::Graph));
        assert_eq!(m.brain_view.graph.focus, None);
        let (m, _) = m.update(Message::BrainGraphSearch("y".into()));
        let hits = crate::components::brain_graph::search_entries(&m.brain_view.entries, &m.brain_view.graph.search);
        assert_eq!(hits.len(), 1);
        let (m, _) = m.update(Message::BrainGraphFocusNode(hits[0]));
        assert_eq!(m.brain_view.graph.focus, Some(hits[0]), "picking an entry centres the graph on it");
    }

    #[test]
    fn an_off_thread_layout_lands_only_if_still_wanted() {
        use crate::components::brain_graph::GraphNode;
        let _iso = ninox_core::test_isolation::isolate();
        let mut m = linked_brain();
        m.brain_view.graph.pending_layout = Some(7);
        m.brain_view.graph.fit_when_laid_out = true;
        let stale = vec![GraphNode { entry: 0, x: 0.0, y: 0.0, degree: 0 }];
        let (mut m, _) = m.update(Message::BrainGraphLaidOut { key: 6, nodes: stale.clone() });
        assert_eq!(m.brain_view.graph.pending_layout, Some(7), "a superseded result is dropped");
        m.brain_view.graph.scope = crate::components::brain_graph::GraphScope::Everything;
        let (m, _) = m.update(Message::BrainGraphLaidOut { key: 7, nodes: stale.clone() });
        let g = &m.brain_view.graph;
        assert_eq!((g.pending_layout, g.nodes.len(), g.fit_when_laid_out), (None, 1, false));
        assert_eq!(g.cached_layout(7), Some(stale), "and cached for next time");
    }

    #[test]
    fn switching_to_clusters_cancels_a_pending_layout() {
        use crate::components::brain_graph::{GraphNode, GraphScope};
        let _iso = ninox_core::test_isolation::isolate();
        let mut m = linked_brain();
        m.brain_view.graph.pending_layout = Some(9);
        m.brain_view.graph.fit_when_laid_out = true;
        let (m, _) = m.update(Message::BrainGraphSetScope(GraphScope::Clusters));
        assert_eq!((m.brain_view.graph.pending_layout, m.brain_view.graph.fit_when_laid_out), (None, false));
        let (m, _) = m.update(Message::BrainGraphLaidOut { key: 9, nodes: vec![GraphNode { entry: 0, x: 0.0, y: 0.0, degree: 0 }] });
        assert!(m.brain_view.graph.nodes.is_empty(), "the late result doesn't land in Clusters");
        assert!(!m.brain_view.graph.clusters.is_empty());
    }

    #[test]
    fn a_failed_off_thread_layout_is_not_cached() {
        let _iso = ninox_core::test_isolation::isolate();
        let mut m = linked_brain();
        m.brain_view.graph.scope = crate::components::brain_graph::GraphScope::Everything;
        m.brain_view.graph.pending_layout = Some(5);
        let (m, _) = m.update(Message::BrainGraphLaidOut { key: 5, nodes: Vec::new() });
        assert_eq!(m.brain_view.graph.cached_layout(5), None);
    }

    #[test]
    fn the_minimap_collapse_is_remembered_and_colour_by_switches() {
        use crate::components::brain_graph::ColorBy;
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("minimap.toml");
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(&config_path);
            let m = linked_brain();
            let (m, _) = m.update(Message::BrainGraphToggleMinimap);
            assert!(m.config.brain_minimap_collapsed);
            assert!(ninox_core::config::AppConfig::load().unwrap().brain_minimap_collapsed, "persisted");
            let (m, _) = m.update(Message::BrainGraphColorBy(ColorBy::Repo));
            assert_eq!(m.brain_view.graph.color_by, ColorBy::Repo);
        }
    }

    #[test]
    fn navigate_settings_switches_view() {
        let m = base(test_engine());
        let (m, _) = m.update(Message::NavigateSettings);
        assert!(matches!(m.view, View::Settings));
        // and back out via the TOC
        let (m, _) = m.update(Message::NavigateBoard { scope: None });
        assert!(matches!(m.view, View::Board { .. }));
    }

    #[test]
    fn model_list_loaded_populates_cache() {
        let m = base(test_engine());
        let (m, _) = m.update(Message::ModelListLoaded {
            harness: "opencode".into(),
            models:  Some(vec!["a".into()]),
        });
        assert_eq!(m.model_lists.get("opencode"), Some(&Some(vec!["a".to_string()])));
        // A failed discovery is cached too — pickers fall through to
        // known_models without retrying every render.
        let (m, _) = m.update(Message::ModelListLoaded { harness: "aider".into(), models: None });
        assert_eq!(m.model_lists.get("aider"), Some(&None));
    }

    #[test]
    fn session_spawned_inserts() {
        let e = test_engine();
        let m = base(e);
        let s = Session {
            id:              "s1".into(),
            orchestrator_id: None,
            name:            "w".into(),
            repo:            "r".into(),
            status:          SessionStatus::Working,
            agent_type:      "c".into(),
            cost_usd:        0.0,
            started_at:      0,
            pr_number:       None,
            pr_id:           None,
            workspace_path:  None,
            pid:             None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        let (updated, _) = m.update(Message::EngineEvent(Box::new(Event::SessionSpawned(s))));
        assert!(updated.sessions.contains_key("s1"));
    }

    /// Two producers racing `SessionUpdated` for the same session must merge
    /// their flagged fields rather than one wholesale-replacing the other's
    /// work — a stale rebroadcast (e.g. a poller tick that read the row
    /// before another actor's write landed) must not stomp fields it isn't
    /// flagged to own.
    #[test]
    fn session_updated_merges_instead_of_replacing() {
        let e = test_engine();
        let mut app = base(e);
        let mut session = Session {
            id: "s1".into(), orchestrator_id: None, name: "w".into(),
            repo: "r1".into(), status: SessionStatus::Working,
            agent_type: "claude-code".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        let (updated, _) = app.update(Message::EngineEvent(Box::new(
            Event::SessionSpawned(session.clone()),
        )));
        app = updated;

        // Actor A: authoritative for PR_LINK + STATUS, from a fresh read.
        session.status = SessionStatus::PrOpen;
        session.pr_number = Some(7);
        let (after_a, _) = app.update(Message::EngineEvent(Box::new(
            Event::SessionUpdated(session.clone(), SessionFields::STATUS | SessionFields::PR_LINK),
        )));
        app = after_a;

        // Actor B: authoritative for COST only, from a *stale* read that
        // still has pr_number: None — must not stomp A's PR_LINK write.
        let mut stale = session.clone();
        stale.pr_number = None;
        stale.status = SessionStatus::Working;
        stale.cost_usd = 3.25;
        let (after_b, _) = app.update(Message::EngineEvent(Box::new(
            Event::SessionUpdated(stale, SessionFields::COST),
        )));

        let final_session = after_b.sessions.get("s1").unwrap();
        assert_eq!(final_session.cost_usd, 3.25, "B's flagged field must apply");
        assert_eq!(final_session.pr_number, Some(7), "A's pr_number must survive B's stale rebroadcast");
        assert!(matches!(final_session.status, SessionStatus::PrOpen), "A's status must survive B's stale rebroadcast");
    }

    /// An agent that accidentally opens a second PR must not have it
    /// silently vanish — it needs to land in `state.prs` so the Pull
    /// Requests ledger can show it — but it must never displace the
    /// session's actually-tracked PR (`ExtraPrDetected` is deliberately
    /// distinct from `PrOpened` for exactly this reason).
    #[test]
    fn extra_pr_detected_populates_prs_without_touching_tracked_pr() {
        let e = test_engine();
        let mut m = base(e);
        let (next, _) = m.update(Message::EngineEvent(Box::new(Event::SessionSpawned(
            pr_session(Some(42), Some(42), "org/repo"),
        ))));
        m = next;

        let extra = ninox_core::types::PR {
            id: 43, number: 43, title: String::new(),
            url: "https://github.com/org/repo/pull/43".into(),
            body: String::new(), session_id: "s1".into(),
        };
        let (updated, _) = m.update(Message::EngineEvent(Box::new(Event::ExtraPrDetected(extra))));

        assert!(updated.prs.contains_key(&43), "extra PR must be visible to the ledger");
        let session = updated.sessions.get("s1").unwrap();
        assert_eq!(session.pr_number, Some(42), "tracked PR must not change");
        assert_eq!(session.pr_id, Some(42), "tracked PR must not change");
    }

    fn messages_delivered(session_id: &str, count: u64) -> Message {
        Message::EngineEvent(Box::new(Event::MessagesDelivered { session_id: session_id.into(), count }))
    }

    #[test]
    fn messages_delivered_to_a_session_not_on_screen_accumulate_as_unread() {
        let m = base(test_engine());
        let (m, _) = m.update(messages_delivered("orch-1", 1));
        let (m, _) = m.update(messages_delivered("orch-1", 2));
        assert_eq!(m.unread_messages.get("orch-1"), Some(&3));
    }

    #[test]
    fn messages_delivered_to_the_session_on_screen_are_not_unread() {
        let mut m = base(test_engine());
        m.view = View::SessionDetail { session_id: "orch-1".into(), panel: DetailPanel::Split };
        let (m, _) = m.update(messages_delivered("orch-1", 1));
        assert_eq!(m.unread_messages.get("orch-1"), None);
    }

    #[test]
    fn opening_a_session_clears_its_unread_messages() {
        let m = base(test_engine());
        let (m, _) = m.update(messages_delivered("orch-1", 4));
        let (m, _) = m.update(messages_delivered("orch-2", 1));
        let (m, _) = m.update(Message::NavigateSession("orch-1".into()));
        assert_eq!(m.unread_messages.get("orch-1"), None);
        assert_eq!(m.unread_messages.get("orch-2"), Some(&1), "other sessions keep their badge");
    }

    #[test]
    fn removing_an_orchestrator_clears_unread_for_it_and_its_workers() {
        let mut m = base(test_engine());
        m.orchestrators.push(Orchestrator { id: "o1".into(), name: "orch".into(), created_at: 0 });
        let mut w = Session {
            id: "w1".into(), orchestrator_id: Some("o1".into()), name: "w".into(),
            repo: "r".into(), status: SessionStatus::Working,
            agent_type: "c".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        m.sessions.insert("w1".into(), w.clone());
        w.id = "other".into();
        w.orchestrator_id = None;
        m.sessions.insert("other".into(), w);
        let (m, _) = m.update(messages_delivered("o1", 1));
        let (m, _) = m.update(messages_delivered("w1", 1));
        let (m, _) = m.update(messages_delivered("other", 1));

        let (m, _) = m.update(Message::EngineEvent(Box::new(Event::OrchestratorRemoved("o1".into()))));

        assert_eq!(m.unread_messages.get("o1"), None);
        assert_eq!(m.unread_messages.get("w1"), None);
        assert_eq!(m.unread_messages.get("other"), Some(&1));
    }

    #[test]
    fn a_finished_session_drops_its_unread_messages() {
        let m = base(test_engine());
        let (m, _) = m.update(messages_delivered("w1", 2));
        let (m, _) = m.update(Message::EngineEvent(Box::new(Event::SessionDone("w1".into()))));
        assert_eq!(m.unread_messages.get("w1"), None);
    }

    #[test]
    fn notifications_capped_at_50() {
        let _iso = ninox_core::test_isolation::isolate();
        let e = test_engine();
        let mut m = base(e);
        for i in 0..55u32 {
            let n = Notification {
                id:         i.to_string(),
                kind:       NotificationKind::WorkerDone,
                title:      "t".into(),
                body:       "b".into(),
                session_id: None,
                created_at: 0,
            };
            m.engine.store.insert_notification(&n).unwrap();
            let (next, _) = m.update(Message::EngineEvent(Box::new(Event::Notification(n))));
            m = next;
        }
        assert_eq!(m.notifications.len(), 50);
    }

    #[test]
    fn spawn_form_confirm_inserts_orchestrator_and_navigates() {
        let _iso = ninox_core::test_isolation::isolate();
        let e = test_engine();
        let mut m = base(e);
        let (next, _) = m.update(Message::SpawnSession);
        m = next;
        assert!(m.spawn_modal.is_some());

        let (next, _) = m.update(Message::SpawnFormName("my-feature".into()));
        m = next;
        let (next, _) = m.update(Message::SpawnFormConfirm);
        m = next;

        assert!(m.spawn_modal.is_none());
        assert_eq!(m.orchestrators.len(), 1);
        assert_eq!(m.orchestrators[0].name, "my-feature");

        // A session with the orchestrator's ID must exist for the terminal view
        let orch_id = &m.orchestrators[0].id;
        assert!(m.sessions.contains_key(orch_id));

        // View should be the session detail for that orchestrator
        assert!(matches!(&m.view, View::SessionDetail { session_id, .. } if session_id == orch_id));

        // The newly spawned orchestrator's session must be remembered as the
        // last-visited session so NavigateLastSession can return to it.
    }

    #[test]
    fn orchestrator_spawn_persists_a_claude_session_id() {
        let _iso = ninox_core::test_isolation::isolate();
        let e = test_engine();
        let mut m = base(e);
        let (next, _) = m.update(Message::SpawnSession);
        m = next;
        let (next, _) = m.update(Message::SpawnFormName("my-feature".into()));
        m = next;
        let (next, _) = m.update(Message::SpawnFormConfirm);
        m = next;

        let orch_id = &m.orchestrators[0].id;
        let session = m.sessions.get(orch_id).expect("session recorded optimistically");
        assert!(
            session.claude_session_id.is_some(),
            "a fresh UUID must be recorded at spawn time",
        );
    }

    #[test]
    fn opening_worker_uses_global_preferred_panel() {
        use crate::components::session_detail::DetailPanel;
        let e = test_engine();
        let mut m = base(e);
        let s = Session {
            id: "sess-a".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: SessionStatus::Working,
            agent_type: "c".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        let (next, _) = m.update(Message::EngineEvent(Box::new(Event::SessionSpawned(s))));
        m = next;

        let (next, _) = m.update(Message::NavigateSession("sess-a".into()));
        m = next;
        assert!(matches!(&m.view, View::SessionDetail { panel: DetailPanel::Split, .. }));

        let (next, _) = m.update(Message::SwitchDetailPanel(DetailPanel::Terminal));
        m = next;
        let (next, _) = m.update(Message::NavigateBoard { scope: None });
        m = next;
        let (next, _) = m.update(Message::NavigateSession("sess-a".into()));
        m = next;
        assert!(
            matches!(&m.view, View::SessionDetail { panel: DetailPanel::Terminal, .. }),
            "opening a worker must use the global preferred panel, not reset to Split"
        );
    }

    #[test]
    fn spawn_form_cancel_clears_modal() {
        let e = test_engine();
        let mut m = base(e);
        let (next, _) = m.update(Message::SpawnSession);
        m = next;
        let (next, _) = m.update(Message::SpawnFormCancel);
        m = next;
        assert!(m.spawn_modal.is_none());
        assert!(m.orchestrators.is_empty());
    }

    #[test]
    fn new_loads_sessions_and_orchestrators_from_db() {
        let _iso = ninox_core::test_isolation::isolate();
        let store = Arc::new(
            Store::open(tempdir().unwrap().keep().join("t.db")).unwrap(),
        );
        store.upsert_orchestrator(&Orchestrator {
            id: "o1".into(), name: "test-orch".into(), created_at: 0,
        }).unwrap();
        store.upsert_session(&Session {
            id: "s1".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: SessionStatus::Working,
            agent_type: "c".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        }).unwrap();
        let engine = Engine::new(store);
        let brain = Arc::new(BrainIndex::open(tempdir().unwrap().keep()).unwrap());
        let (app, _task) = App::new(engine, std::path::PathBuf::from("/tmp"), brain, std::path::PathBuf::from("/tmp/t.db"));
        assert_eq!(app.orchestrators.len(), 1);
        assert_eq!(app.sessions.len(), 1);
        assert!(app.sessions.contains_key("s1"));
    }

    fn comment(id: i64, pr_id: i64, body: &str) -> Comment {
        Comment {
            id, pr_id, author: "a".into(), body: body.into(),
            path: None, line: None, created_at: id,
        }
    }

    /// Shaping a `text_editor::Content` per persisted comment used to run
    /// synchronously inside `App::new`, which iced calls before the window
    /// is ever created — with enough historical comments this blocked the
    /// window from appearing at all (MLOPS-5188). `review_threads` (plain
    /// data, no shaping) still loads eagerly; `comment_editors` must now
    /// stay empty until a session's PR is actually viewed.
    #[test]
    fn new_loads_review_threads_but_does_not_eagerly_shape_comment_editors() {
        let _iso = ninox_core::test_isolation::isolate();
        let store = Arc::new(Store::open(tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_comment_for_test(&comment(1, 100, "first")).unwrap();
        store.upsert_comment_for_test(&comment(2, 100, "second")).unwrap();
        store.upsert_comment_for_test(&comment(3, 200, "third")).unwrap();
        let engine = Engine::new(store);
        let brain = Arc::new(BrainIndex::open(tempdir().unwrap().keep()).unwrap());
        let (app, _task) = App::new(engine, std::path::PathBuf::from("/tmp"), brain, std::path::PathBuf::from("/tmp/t.db"));

        assert!(app.comment_editors.is_empty());
        assert_eq!(app.review_threads.get(&100).map(Vec::len), Some(2));
        assert_eq!(app.review_threads.get(&200).map(Vec::len), Some(1));
    }

    /// User decision on PR #273: the prompt agent (MLOPS-5415) gets no
    /// session page or terminal of its own — `NavigateSession` is the
    /// single choke point every navigation path (fleet, search,
    /// notifications) already routes through, so guarding it here covers
    /// all of them without a per-call-site check.
    #[test]
    fn navigate_session_to_the_prompt_agent_is_a_noop() {
        let e = test_engine();
        let mut app = base(e);
        app.sessions.insert(
            ninox_core::worker_type::PROMPT_SESSION_ID.into(),
            refile_session(ninox_core::worker_type::PROMPT_SESSION_ID),
        );
        let (app, task) = app.update(Message::NavigateSession(ninox_core::worker_type::PROMPT_SESSION_ID.into()));

        assert!(
            matches!(app.view, View::Fleet),
            "navigating to the prompt agent must never change the view, got {:?}", app.view,
        );
        assert!(iced_runtime::task::into_stream(task).is_none());
    }

    /// The deferred counterpart: navigating into a session hydrates editors
    /// for exactly the PR thread about to be shown, lazily, the same way
    /// `ensure_diff`/`ensure_plan` hydrate their on-demand panels.
    #[test]
    fn navigate_session_lazily_builds_comment_editors_for_its_pr() {
        let e = test_engine();
        let mut app = base(e);
        app.sessions.insert("s1".into(), pr_session(Some(7), Some(100), "org/repo"));
        app.review_threads.insert(100, vec![comment(1, 100, "first"), comment(2, 100, "second")]);
        app.review_threads.insert(200, vec![comment(3, 200, "other pr")]);

        let (app, _task) = app.update(Message::NavigateSession("s1".into()));

        assert!(app.comment_editors.contains_key(&1));
        assert!(app.comment_editors.contains_key(&2));
        assert!(!app.comment_editors.contains_key(&3));
    }

    /// Terminal/Diff/Plan/Inspector never render the Marginalia panel (only
    /// Split and Info do — see `session_detail`'s panel routing), so
    /// navigating there shouldn't pay the shaping cost either — switching
    /// into Split later is what hydrates them, mirroring `ensure_diff`'s own
    /// panel-gated call in `SwitchDetailPanel`.
    #[test]
    fn navigate_session_on_a_non_marginalia_panel_skips_shaping_comment_editors() {
        let e = test_engine();
        let mut app = base(e);
        app.worker_panel = DetailPanel::Terminal;
        app.sessions.insert("s1".into(), pr_session(Some(7), Some(100), "org/repo"));
        app.review_threads.insert(100, vec![comment(1, 100, "first")]);

        let (app, _task) = app.update(Message::NavigateSession("s1".into()));
        assert!(app.comment_editors.is_empty());

        let (app, _task) = app.update(Message::SwitchDetailPanel(DetailPanel::Split));
        assert!(app.comment_editors.contains_key(&1));
    }

    /// A PR can attach to a session (`Event::PrOpened`) while that session's
    /// Marginalia panel is already on screen — e.g. `review_threads[pr_id]`
    /// was already loaded (another session touched the same PR earlier) by
    /// the time this one's `pr_id` gets set. Without hydrating here too, the
    /// comment cards would render with blank bodies until the user
    /// navigated away and back (MLOPS-5188 review finding).
    #[test]
    fn pr_opened_hydrates_comment_editors_for_the_session_already_in_view() {
        let e = test_engine();
        let mut app = base(e);
        app.sessions.insert("s1".into(), pr_session(None, None, "org/repo"));
        app.view = View::SessionDetail { session_id: "s1".into(), panel: DetailPanel::Split };
        app.review_threads.insert(100, vec![comment(1, 100, "first")]);

        let pr = ninox_core::types::PR {
            id: 100, number: 7, title: "t".into(),
            url: "https://github.com/org/repo/pull/7".into(),
            body: String::new(), session_id: "s1".into(),
        };
        let (app, _task) = app.update(Message::EngineEvent(Box::new(
            Event::PrOpened { session_id: "s1".into(), pr },
        )));

        assert!(app.comment_editors.contains_key(&1));
    }

    #[test]
    fn terminated_session_visible_in_board() {
        use crate::components::fleet_board::board_sessions;
        let e = test_engine();
        let mut m = base(e);
        let s = Session {
            id: "t1".into(), orchestrator_id: None, name: "ended".into(),
            repo: "r".into(), status: SessionStatus::Terminated,
            agent_type: "c".into(), cost_usd: 0.42,
            started_at: 0, pr_number: None, pr_id: None,
            workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        let (m2, _) = m.update(Message::EngineEvent(Box::new(Event::SessionSpawned(s))));
        m = m2;
        // board_sessions(app, status, scope) returns sessions with that status
        let terminated = board_sessions(&m, &SessionStatus::Terminated, None);
        assert_eq!(terminated.len(), 1);
        assert_eq!(terminated[0].id, "t1");
    }

    #[test]
    fn adopts_pr_progress_follows_the_daemons_pr_link_and_lifecycle_only() {
        let mut known = live_session("s1");
        known.status = SessionStatus::Working;
        let mut stored = known.clone();
        stored.repo = "o/r".into();
        stored.pr_number = Some(7);
        stored.pr_id = Some(7);
        stored.status = SessionStatus::CiFailed;
        assert!(adopts_pr_progress(&mut known, &stored));
        assert_eq!((known.pr_number, known.status.clone()), (Some(7), SessionStatus::CiFailed));
        assert!(!adopts_pr_progress(&mut known, &stored), "already in sync");
        stored.merged_at = Some(99);
        assert!(adopts_pr_progress(&mut known, &stored));
        assert_eq!(known.merged_at, Some(99));

        // The app's own transitions are never overwritten by a store read.
        known.status = SessionStatus::Interrupted;
        stored.status = SessionStatus::Working;
        adopts_pr_progress(&mut known, &stored);
        assert_eq!(known.status, SessionStatus::Interrupted);
        known.status = SessionStatus::Working;
        stored.status = SessionStatus::Done;
        adopts_pr_progress(&mut known, &stored);
        assert_eq!(known.status, SessionStatus::Working);
    }

    /// A Done worker must survive `PollSessions` while its store record is
    /// still within the retention grace period — the old behavior (instant
    /// removal on status alone) gave the fleet board no window to show what
    /// just completed. Only once the record is actually gone from the store
    /// (here simulated directly, standing in for
    /// `Poller::sweep_retired_sessions` having purged it) must the board
    /// drop it.
    #[test]
    fn poll_sessions_keeps_done_worker_until_purged_from_store() {
        let _iso = ninox_core::test_isolation::isolate();
        let e = test_engine();
        let mut m = base(e);

        // Set up an orchestrator
        let o = Orchestrator { id: "o1".into(), name: "orch".into(), created_at: 0 };
        let _ = m.engine.store.upsert_orchestrator(&o);
        let (next, _) = m.update(Message::EngineEvent(Box::new(Event::OrchestratorSpawned(o))));
        m = next;

        // Worker session under the orchestrator, already Done
        let worker = Session {
            id: "w1".into(), orchestrator_id: Some("o1".into()), name: "worker".into(),
            repo: "r".into(), status: SessionStatus::Done,
            agent_type: "c".into(), cost_usd: 0.0,
            started_at: 0, pr_number: None, pr_id: None,
            workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: Some(0), gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        let _ = m.engine.store.upsert_session(&worker);
        let (next, _) = m.update(Message::EngineEvent(Box::new(Event::SessionSpawned(worker))));
        m = next;
        assert!(m.sessions.contains_key("w1"), "worker should be present before poll");

        // Still present after a poll tick — the record is still in the
        // store, so the board must not drop it just because it's Done.
        let (next, _) = m.update(Message::PollSessions);
        m = next;
        assert!(
            m.sessions.contains_key("w1"),
            "Done worker must survive PollSessions while its retention window hasn't elapsed",
        );

        // Once the retention sweep has actually purged the record...
        m.engine.store.delete_session("w1").unwrap();
        let (next, _) = m.update(Message::PollSessions);
        m = next;
        assert!(
            !m.sessions.contains_key("w1"),
            "a session purged from the store must disappear from the board on the next poll",
        );
    }

    /// An orchestrator's own session (tracked in `state.sessions` alongside
    /// its workers) must never be dropped from the board by `PollSessions`
    /// even if it's briefly absent from a given `list_sessions()` read —
    /// mirrors the retention sweep's own orchestrator exemption.
    #[test]
    fn poll_sessions_never_drops_orchestrator_sessions() {
        let _iso = ninox_core::test_isolation::isolate();
        let e = test_engine();
        let mut m = base(e);

        let o = Orchestrator { id: "o1".into(), name: "orch".into(), created_at: 0 };
        let _ = m.engine.store.upsert_orchestrator(&o);
        let (next, _) = m.update(Message::EngineEvent(Box::new(Event::OrchestratorSpawned(o))));
        m = next;

        // The orchestrator's own session row is untracked in the sessions
        // table (e.g. not yet written) — PollSessions must not evict it from
        // `state.sessions` just because `db_sessions` doesn't contain it.
        m.sessions.insert("o1".into(), Session {
            id: "o1".into(), orchestrator_id: None, name: "orch".into(),
            repo: "r".into(), status: SessionStatus::Working,
            agent_type: "c".into(), cost_usd: 0.0,
            started_at: 0, pr_number: None, pr_id: None,
            workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        });

        let (next, _) = m.update(Message::PollSessions);
        m = next;
        assert!(m.sessions.contains_key("o1"), "orchestrator sessions must never be auto-dropped");
    }

    #[test]
    fn poll_sessions_drops_the_cached_diff_once_the_session_is_purged_from_the_store() {
        let _iso = ninox_core::test_isolation::isolate();
        // Mirrors `poll_sessions_keeps_done_worker_until_purged_from_store`:
        // PollSessions only evicts `state` for what's genuinely gone from
        // the store (the retention sweep's job, simulated here via a direct
        // delete), not on status alone.
        let e = test_engine();
        let mut m = base(e);
        let s = Session {
            id: "w1".into(), orchestrator_id: None, name: "worker".into(),
            repo: "r".into(), status: SessionStatus::Done,
            agent_type: "c".into(), cost_usd: 0.0,
            started_at: 0, pr_number: None, pr_id: None,
            workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: Some(0), gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        let _ = m.engine.store.upsert_session(&s);
        let (next, _) = m.update(Message::EngineEvent(Box::new(Event::SessionSpawned(s))));
        m = next;
        m.diffs.insert("w1".into(), Some("stale diff text".into()));

        m.engine.store.delete_session("w1").unwrap();
        let (next, _) = m.update(Message::PollSessions);
        m = next;
        assert!(!m.diffs.contains_key("w1"), "a session purged from the store must not leak its cached diff");
    }

    #[test]
    fn kill_keeps_the_session_on_the_board() {
        let m = fleet_test_app("o1", "w1");
        let (m, _) = m.update(Message::KillSession("w1".into()));
        assert!(m.sessions.contains_key("w1"), "only Delete… removes a row");
    }

    #[test]
    fn deleting_an_orchestrator_drops_cached_diffs_for_it_and_its_workers() {
        let e = test_engine();
        let mut m = base(e);
        let o = Orchestrator { id: "o1".into(), name: "orch".into(), created_at: 0 };
        let worker = Session {
            id: "w1".into(), orchestrator_id: Some("o1".into()), name: "worker".into(),
            repo: "r".into(), status: SessionStatus::Working,
            agent_type: "c".into(), cost_usd: 0.0,
            started_at: 0, pr_number: None, pr_id: None,
            workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        let (m2, _) = m.update(Message::EngineEvent(Box::new(Event::OrchestratorSpawned(o))));
        m = m2;
        let (m2, _) = m.update(Message::EngineEvent(Box::new(Event::SessionSpawned(worker))));
        m = m2;
        m.diffs.insert("o1".into(), Some("stale orch diff".into()));
        m.diffs.insert("w1".into(), Some("stale worker diff".into()));

        let deleted = ninox_core::events::DeleteOutcome::Deleted;
        let (m, _) = m.update(Message::DeleteDone(vec![("w1".into(), deleted.clone()), ("o1".into(), deleted)]));
        assert!(!m.diffs.contains_key("o1"));
        assert!(!m.diffs.contains_key("w1"));
    }

    #[test]
    fn session_done_event_drops_the_cached_diff() {
        let e = test_engine();
        let mut m = base(e);
        let s = refile_session("s1");
        m.sessions.insert("s1".into(), s);
        m.diffs.insert("s1".into(), Some("stale diff text".into()));
        let (m, _) = m.update(Message::EngineEvent(Box::new(Event::SessionDone("s1".into()))));
        assert!(!m.diffs.contains_key("s1"));
    }

    #[test]
    fn navigate_plans_sets_view() {
        let e = test_engine();
        let m = base(e);
        let (m2, _) = m.update(Message::NavigatePlans);
        assert!(matches!(m2.view, View::Plans));
    }

    #[test]
    fn navigate_fleet_sets_view() {
        let e = test_engine();
        let m = base(e);
        // Start from a non-default view so this isn't trivially true.
        let (m, _) = m.update(Message::NavigateBoard { scope: None });
        let (m2, _) = m.update(Message::NavigateFleet);
        assert!(matches!(m2.view, View::Fleet));
    }

    #[test]
    fn navigate_worker_types_sets_view() {
        let _iso = ninox_core::test_isolation::isolate();
        let e = test_engine();
        let m = base(e);
        let (m2, _) = m.update(Message::NavigateWorkerTypes);
        assert!(matches!(m2.view, View::WorkerTypes));
    }

    // ── Worker Types tab ────────────────────────────────────────────────

    #[test]
    fn navigate_worker_types_refreshes_the_list_and_selects_an_entry() {
        let dir = tempdir().unwrap();
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(dir.path().join("config.toml"));
            let m = base(test_engine());
            let (m, _) = m.update(Message::NavigateWorkerTypes);
            assert!(!m.worker_types.entries.is_empty(), "built-ins always exist");
            assert!(m.worker_types.form.original_name.is_some());
        }
    }

    #[test]
    fn worker_type_new_draft_blanks_the_form() {
        let dir = tempdir().unwrap();
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(dir.path().join("config.toml"));
            let m = base(test_engine());
            let (m, _) = m.update(Message::NavigateWorkerTypes);
            let (m, _) = m.update(Message::WorkerTypeNewDraft);
            assert!(m.worker_types.form.is_new);
            assert!(m.worker_types.form.original_name.is_none());
            assert!(m.worker_types.form.name.is_empty());
        }
    }

    #[test]
    fn worker_type_select_loads_the_chosen_entrys_spec() {
        let dir = tempdir().unwrap();
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(dir.path().join("config.toml"));
            let m = base(test_engine());
            let (m, _) = m.update(Message::NavigateWorkerTypes);
            let (m, _) = m.update(Message::WorkerTypeSelect("Researcher".into()));
            assert_eq!(m.worker_types.form.original_name.as_deref(), Some("Researcher"));
            assert!(!m.worker_types.form.is_new);
            assert!(m.worker_types.form.built_in);
        }
    }

    #[test]
    fn worker_type_save_creates_a_new_type_and_refreshes_the_list() {
        let dir = tempdir().unwrap();
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(dir.path().join("config.toml"));
            let m = base(test_engine());
            let (m, _) = m.update(Message::NavigateWorkerTypes);
            let (m, _) = m.update(Message::WorkerTypeNewDraft);
            let (mut m, _) = m.update(Message::WorkerTypeNameChanged("GuiType".into()));
            m.worker_types.form.brief = iced::widget::text_editor::Content::with_text("Do it: {prompt}");
            let (m, _) = m.update(Message::WorkerTypeSave);
            assert!(m.worker_types.error.is_none(), "{:?}", m.worker_types.error);
            assert!(m.worker_types.notice.is_some());
            assert!(m.worker_types.entries.iter().any(|e| e.spec.name == "GuiType"));
            assert!(!m.worker_types.form.is_new, "the saved type becomes the new selection");
        }
    }

    #[test]
    fn worker_type_save_failure_sets_error_and_keeps_the_draft() {
        let dir = tempdir().unwrap();
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(dir.path().join("config.toml"));
            let m = base(test_engine());
            let (m, _) = m.update(Message::NavigateWorkerTypes);
            let (m, _) = m.update(Message::WorkerTypeNewDraft);
            // A blank name is rejected by `worker_type_cli::create`.
            let (m, _) = m.update(Message::WorkerTypeSave);
            assert!(m.worker_types.error.is_some());
            assert!(m.worker_types.form.is_new, "a failed save must not clear the draft");
        }
    }

    #[test]
    fn worker_type_brief_action_edits_the_draft_content() {
        use iced::widget::text_editor::{Action, Edit};
        let dir = tempdir().unwrap();
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(dir.path().join("config.toml"));
            let m = base(test_engine());
            let (m, _) = m.update(Message::NavigateWorkerTypes);
            let (m, _) = m.update(Message::WorkerTypeNewDraft);
            let (m, _) = m.update(Message::WorkerTypeBriefAction(Action::Edit(Edit::Insert('x'))));
            assert_eq!(m.worker_types.form.brief.text(), "x\n");
        }
    }

    #[test]
    fn worker_type_toggle_can_manage_types() {
        let dir = tempdir().unwrap();
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(dir.path().join("config.toml"));
            let m = base(test_engine());
            let (m, _) = m.update(Message::NavigateWorkerTypes);
            assert!(!m.worker_types.form.can_manage_types);
            let (m, _) = m.update(Message::WorkerTypeToggleCanManageTypes);
            assert!(m.worker_types.form.can_manage_types);
        }
    }

    #[test]
    fn worker_type_select_clears_a_stale_notice_from_a_previous_action() {
        let dir = tempdir().unwrap();
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(dir.path().join("config.toml"));
            let m = base(test_engine());
            let (m, _) = m.update(Message::NavigateWorkerTypes);
            let (mut m, _) = m.update(Message::WorkerTypeSelect("Researcher".into()));
            m.worker_types.notice = Some("exported to /tmp/x.toml".into());
            let (m, _) = m.update(Message::WorkerTypeSelect("Evaluator".into()));
            assert!(m.worker_types.notice.is_none());
        }
    }

    #[test]
    fn worker_type_save_failure_clears_a_stale_success_notice() {
        let dir = tempdir().unwrap();
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(dir.path().join("config.toml"));
            let m = base(test_engine());
            let (m, _) = m.update(Message::NavigateWorkerTypes);
            let (mut m, _) = m.update(Message::WorkerTypeNewDraft);
            m.worker_types.notice = Some("saved 'Something'".into());
            // No name set: `save` refuses an empty type name, exercising the Err branch.
            let (m, _) = m.update(Message::WorkerTypeSave);
            assert!(m.worker_types.error.is_some());
            assert!(m.worker_types.notice.is_none());
        }
    }

    #[test]
    fn worker_type_delete_removes_a_custom_type() {
        let dir = tempdir().unwrap();
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(dir.path().join("config.toml"));
            let m = base(test_engine());
            let (m, _) = m.update(Message::NavigateWorkerTypes);
            let (m, _) = m.update(Message::WorkerTypeNewDraft);
            let (m, _) = m.update(Message::WorkerTypeNameChanged("Throwaway".into()));
            let (m, _) = m.update(Message::WorkerTypeSave);
            assert!(m.worker_types.entries.iter().any(|e| e.spec.name == "Throwaway"));

            let (m, _) = m.update(Message::WorkerTypeDelete);
            assert!(m.worker_types.error.is_none(), "{:?}", m.worker_types.error);
            assert!(!m.worker_types.entries.iter().any(|e| e.spec.name == "Throwaway"));
        }
    }

    #[test]
    fn worker_type_delete_refuses_a_builtin() {
        let dir = tempdir().unwrap();
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(dir.path().join("config.toml"));
            let m = base(test_engine());
            let (m, _) = m.update(Message::NavigateWorkerTypes);
            let (m, _) = m.update(Message::WorkerTypeSelect("Researcher".into()));
            let (m, _) = m.update(Message::WorkerTypeDelete);
            assert!(m.worker_types.error.is_some());
            assert!(m.worker_types.entries.iter().any(|e| e.spec.name == "Researcher"), "a refused delete must not remove the builtin");
        }
    }

    #[test]
    fn worker_type_export_writes_a_file_and_sets_a_notice() {
        let dir = tempdir().unwrap();
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(dir.path().join("config.toml"));
            let m = base(test_engine());
            let (m, _) = m.update(Message::NavigateWorkerTypes);
            let (m, _) = m.update(Message::WorkerTypeSelect("Evaluator".into()));
            let (m, _) = m.update(Message::WorkerTypeExport);
            assert!(m.worker_types.error.is_none(), "{:?}", m.worker_types.error);
            let notice = m.worker_types.notice.expect("export sets a notice with the path");
            assert!(notice.contains("Evaluator") || notice.to_lowercase().contains("evaluator"), "{notice}");
        }
    }

    #[test]
    fn worker_type_import_round_trips_an_export() {
        let dir = tempdir().unwrap();
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(dir.path().join("config.toml"));
            let m = base(test_engine());
            let (m, _) = m.update(Message::NavigateWorkerTypes);
            let (m, _) = m.update(Message::WorkerTypeNewDraft);
            let (m, _) = m.update(Message::WorkerTypeNameChanged("Exportable".into()));
            let (m, _) = m.update(Message::WorkerTypeSave);
            let (m, _) = m.update(Message::WorkerTypeExport);
            let path = ninox_core::config::AppConfig::worker_type_exports_dir().join("exportable.toml");
            assert!(path.exists(), "{}", path.display());

            let (m, _) = m.update(Message::WorkerTypeDelete);
            assert!(!m.worker_types.entries.iter().any(|e| e.spec.name == "Exportable"));

            let (m, _) = m.update(Message::WorkerTypeImportPathChanged(path.display().to_string()));
            let (m, _) = m.update(Message::WorkerTypeImport);
            assert!(m.worker_types.error.is_none(), "{:?}", m.worker_types.error);
            assert!(m.worker_types.entries.iter().any(|e| e.spec.name == "Exportable"));
        }
    }

    #[test]
    fn navigate_remotes_sets_view() {
        let _iso = ninox_core::test_isolation::isolate();
        let e = test_engine();
        let m = base(e);
        let (m2, _) = m.update(Message::NavigateRemotes);
        assert!(matches!(m2.view, View::Remotes));
    }

    /// Renders `workspaces_modal` with 0/1/several rows — a debug build's
    /// `scrollable` validates its content's size hint at construction time
    /// (`iced_widget::scrollable::Scrollable::validate`), so simply calling
    /// the view function here is the whole test: if any row's content ever
    /// resolves to a `Fill` height, this panics exactly as a live debug
    /// build would.
    #[test]
    fn workspaces_modal_renders_at_several_row_counts_without_panicking() {
        use crate::components::workspaces_modal::{workspaces_modal, WorkspaceRowView, WorkspacesModalState};
        use ninox_core::workspace::{Ownership, OrphanStatus, SkipReason, WorkspaceAction, WorkspaceFacts};
        use std::path::PathBuf;

        fn facts() -> WorkspaceFacts {
            WorkspaceFacts {
                dirty: false, has_branch: true, has_upstream: true,
                unpushed_commit_count: 0, commits_beyond_base: 0,
                merged: true, commits_since_merge: 0,
                interrupted: false, live_session_row: false,
            }
        }

        fn row(i: usize, with_session: bool) -> WorkspaceRowView {
            WorkspaceRowView {
                path: PathBuf::from(format!("/repo/.claude/worktrees/w{i}")),
                session_id: with_session.then(|| format!("w{i}")),
                status: OrphanStatus::Orphan(Ownership::Tracked),
                facts: facts(),
                size_bytes: 1024 * (i as u64 + 1),
                action: if i.is_multiple_of(2) {
                    WorkspaceAction::WouldClean { delete_branch: true, unmerged_with_content: false }
                } else {
                    WorkspaceAction::NeedsAttention(SkipReason::Dirty)
                },
            }
        }

        let app = base(test_engine());
        for &n in &[0usize, 1, 5, 20] {
            for with_session in [false, true] {
                let modal = WorkspacesModalState {
                    rows: (0..n).map(|i| row(i, with_session)).collect(),
                    ..Default::default()
                };
                let _ = workspaces_modal(&app, &modal);
            }
        }
    }

    #[test]
    fn toggle_tree_collapsed_flips_state() {
        let e = test_engine();
        let m = base(e);
        assert!(m.fleet_nav.tree_collapsed);
        let (m, _) = m.update(Message::ToggleTreeCollapsed);
        assert!(!m.fleet_nav.tree_collapsed);
        let (m, _) = m.update(Message::ToggleTreeCollapsed);
        assert!(m.fleet_nav.tree_collapsed);
    }

    /// An orchestrator with one worker, wired into `m.orchestrators`/
    /// `m.sessions` (not the store — `build_fleet_islands` never hits it)
    /// so `build_fleet_islands` resolves a real `IslandKey::Root(orch_id)`.
    fn fleet_test_app(orch_id: &str, worker_id: &str) -> App {
        let e = test_engine();
        let mut m = base(e);
        m.orchestrators.push(ninox_core::types::Orchestrator {
            id: orch_id.into(), name: "Orch".into(), created_at: 0,
        });
        let mut orch_session = refile_session(orch_id);
        orch_session.orchestrator_id = None;
        orch_session.status = SessionStatus::Working;
        m.sessions.insert(orch_id.into(), orch_session);
        let mut worker = refile_session(worker_id);
        worker.orchestrator_id = Some(orch_id.into());
        worker.status = SessionStatus::Working;
        m.sessions.insert(worker_id.into(), worker);
        m
    }

    /// A pending top-level plan of three orchestrators, proposed by the
    /// prompt agent and loaded into `m.ghost`.
    fn with_ghost_plan(mut m: App) -> (App, String) {
        use ninox_core::ghost_plan::{Proposal, ProposalSection};
        let section = |id: &str| ProposalSection {
            id: id.into(), parent_id: None, worker_type: Some("Orchestrator".into()), name: None,
            brief: format!("own {id}"), dependencies: vec![], await_done: false, workspace: None, model: None,
            brain_reads: vec![], brain_write: None, linear: None,
        };
        let p = Proposal { parent_id: None, title: Some("Q4".into()), sections: vec![section("alpha"), section("beta"), section("gamma")] };
        let v = ninox_core::ghost_plan::prepare_proposal(&m.engine.store, &m.config, &p, None).unwrap();
        let id = ninox_core::ghost_plan::new_plan_id(now_ms_i64());
        m.engine.store.insert_ghost_plan(&id, ninox_core::worker_type::PROMPT_SESSION_ID, &v, now_ms_i64(), now_ms_i64() + 1_000_000).unwrap();
        m.ghost.plans = ninox_ui_shared::ghost_view::load(&m.engine.store, now_ms_i64());
        (m, id)
    }

    #[test]
    fn ghost_accept_asks_for_an_inline_confirm_and_esc_cancels_it() {
        let _iso = ninox_core::test_isolation::isolate();
        let (m, id) = with_ghost_plan(fleet_test_app("o1", "w1"));
        let (m, _) = m.update(Message::GhostSelect { plan_id: id.clone(), node_id: None });
        let (m, _) = m.update(Message::GhostAcceptRequest(id.clone()));
        assert_eq!(m.ghost.confirm, Some(crate::components::ghost_plans::GhostConfirm::Accept(id.clone())));
        let plan = m.ghost.plan(&id).unwrap();
        assert_eq!(ninox_ui_shared::ghost_view::confirm_text(plan), "Spawn 3 sessions?");
        let (m, _) = m.update(esc());
        assert_eq!(m.ghost.confirm, None);
        assert!(m.ghost.selected.is_some(), "the first Esc only drops the confirm");
        let stored = m.engine.store.get_ghost_plan(&id).unwrap().unwrap();
        assert!(stored.nodes.iter().all(|n| n.state == ninox_core::ghost_plan::NodeState::Ghost), "nothing decided without the confirm");
    }

    #[test]
    fn leaving_the_view_drops_an_armed_ghost_confirm() {
        let _iso = ninox_core::test_isolation::isolate();
        let (m, id) = with_ghost_plan(fleet_test_app("o1", "w1"));
        let (m, _) = m.update(Message::NavigatePlans);
        let (m, _) = m.update(Message::GhostAcceptRequest(id.clone()));
        let (m, _) = m.update(Message::NavigateFleet);
        assert_eq!(m.ghost.confirm, None);
    }

    fn root_key(id: &str) -> ninox_overview::islands::IslandKey {
        ninox_overview::islands::IslandKey::Root(id.into())
    }

    #[test]
    fn navigate_fleet_returns_to_the_root_board_even_from_the_fleet_tab() {
        let _iso = ninox_core::test_isolation::isolate();
        let m = fleet_test_app("o1", "w1");
        let (m, _) = m.update(Message::FleetOpenNode("w1".into()));
        assert!(m.fleet_nav.open.is_some() && m.fleet_nav.selected.is_some());
        let (m, _) = m.update(Message::FleetSendMessageOpen("w1".into()));
        let (m, _) = m.update(Message::NavigateFleet);
        assert!(matches!(m.view, View::Fleet));
        assert_eq!(m.fleet_nav.open, None);
        assert_eq!(m.fleet_nav.selected, None);
        assert_eq!(m.fleet_nav.selected_island, None);
        assert_eq!(m.fleet_message_draft, None);
    }

    #[test]
    fn navigate_fleet_from_a_session_view_lands_on_the_root_board() {
        let _iso = ninox_core::test_isolation::isolate();
        let m = fleet_test_app("o1", "w1");
        let (m, _) = m.update(Message::FleetOpenIsland(root_key("o1")));
        let (m, _) = m.update(Message::NavigateSession("w1".into()));
        let (m, _) = m.update(Message::NavigateFleet);
        assert!(matches!(m.view, View::Fleet));
        assert_eq!(m.fleet_nav.open, None);
        assert_eq!(m.fleet_nav.selected_island, None);
    }

    #[test]
    fn a_selected_board_card_is_the_action_target_until_esc_or_empty_click() {
        let _iso = ninox_core::test_isolation::isolate();
        let key = |k: iced::keyboard::Key| Message::RawKey { key: k, modifiers: iced::keyboard::Modifiers::empty(), text: None };
        let esc = || key(iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape));
        let m = fleet_test_app("o1", "w1");
        let (m, _) = m.update(Message::FleetSelectIsland(root_key("o1")));
        assert_eq!(App::fleet_action_target(&m).as_deref(), Some("o1"));
        let (m, _) = m.update(esc());
        assert_eq!(App::fleet_action_target(&m), None);
        let mut m = m;
        m.fleet_nav.selected_island = Some(root_key("o1"));
        let (m, _) = m.update(Message::FleetClearBoardSelection);
        assert_eq!(App::fleet_action_target(&m), None);
    }

    #[test]
    fn esc_with_the_message_composer_open_on_a_card_cancels_only_the_composer() {
        let _iso = ninox_core::test_isolation::isolate();
        let m = fleet_test_app("o1", "w1");
        let (m, _) = m.update(Message::FleetSelectIsland(root_key("o1")));
        let (m, _) = m.update(Message::FleetSendMessageOpen("o1".into()));
        let esc = Message::RawKey { key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape), modifiers: iced::keyboard::Modifiers::empty(), text: None };
        let (m, _) = m.update(esc);
        assert_eq!(m.fleet_message_draft, None);
        assert_eq!(App::fleet_action_target(&m).as_deref(), Some("o1"));
    }

    #[test]
    fn selecting_a_different_card_drops_a_stale_composer() {
        let _iso = ninox_core::test_isolation::isolate();
        let m = fleet_test_app("o1", "w1");
        let (m, _) = m.update(Message::FleetSelectIsland(root_key("o1")));
        let (m, _) = m.update(Message::FleetSendMessageOpen("o1".into()));
        let (m, _) = m.update(Message::FleetSelectIsland(root_key("o2")));
        assert_eq!(m.fleet_message_draft, None);
    }

    #[test]
    fn a_board_card_selection_is_not_an_action_target_off_the_fleet_tab() {
        let _iso = ninox_core::test_isolation::isolate();
        let m = fleet_test_app("o1", "w1");
        let (m, _) = m.update(Message::FleetSelectIsland(root_key("o1")));
        let (m, _) = m.update(Message::NavigatePlans);
        assert_eq!(App::fleet_action_target(&m), None);
    }

    #[test]
    fn selecting_a_ghost_on_fleet_drops_the_real_selection() {
        let _iso = ninox_core::test_isolation::isolate();
        let (m, id) = with_ghost_plan(fleet_test_app("o1", "w1"));
        let (m, _) = m.update(Message::FleetSelectNode("o1".into()));
        assert!(m.fleet_nav.selected.is_some());
        let (m, _) = m.update(Message::GhostSelect { plan_id: id, node_id: None });
        assert!(m.fleet_nav.selected.is_none());
    }

    #[test]
    fn ghost_confirm_runs_the_decision_once() {
        let _iso = ninox_core::test_isolation::isolate();
        let (m, id) = with_ghost_plan(fleet_test_app("o1", "w1"));
        let (m, _) = m.update(Message::GhostDiscardRequest(id.clone()));
        let (m, _) = m.update(Message::GhostConfirmed);
        assert_eq!(m.ghost.busy.as_deref(), Some(id.as_str()));
        assert_eq!(m.ghost.confirm, None);
        let (m, _) = m.update(Message::GhostConfirmed);
        assert_eq!(m.ghost.busy.as_deref(), Some(id.as_str()), "a second confirm with nothing armed is a no-op");
        let (m, _) = m.update(Message::GhostDecisionDone { plan_id: id.clone(), result: Ok("Discarded".into()) });
        assert_eq!(m.ghost.busy, None);
        assert_eq!(m.ghost.notice.as_deref(), Some("Discarded"));
    }

    #[test]
    fn open_in_plans_switches_tab_and_selecting_a_session_drops_the_ghost() {
        let _iso = ninox_core::test_isolation::isolate();
        let (m, id) = with_ghost_plan(fleet_test_app("o1", "w1"));
        let (m, _) = m.update(Message::GhostOpenInPlans(id.clone()));
        assert!(matches!(m.view, View::Plans));
        assert_eq!(m.ghost.selected.as_ref().map(|s| s.plan_id.as_str()), Some(id.as_str()));
        let (m, _) = m.update(Message::NavigateFleet);
        let (m, _) = m.update(Message::FleetSelectNode("w1".into()));
        assert!(m.ghost.selected.is_none());
    }

    fn with_registered_plan(m: App, dir: &std::path::Path) -> App {
        let file = dir.join("plan.md");
        std::fs::write(&file, "# Goals\n\n- one\n").unwrap();
        m.engine.store.upsert_session(&m.sessions["o1"]).unwrap();
        m.engine.store.upsert_orchestrator(&m.orchestrators[0]).unwrap();
        m.engine.store.register_orchestrator_plan("o1", file.to_str().unwrap(), 1).unwrap();
        m
    }


    #[test]
    fn plans_tab_lists_selects_and_renders_a_registered_plan_without_reload() {
        let _iso = ninox_core::test_isolation::isolate();
        let dir = tempfile::tempdir().unwrap();
        let m = with_registered_plan(fleet_test_app("o1", "w1"), dir.path());
        let (m, _) = m.update(Message::NavigatePlans);
        assert_eq!(m.orch_plans.entries.len(), 1);
        let (m, _) = m.update(Message::PlansSelectOrchestratorPlan("o1".into()));
        assert_eq!(m.orch_plans.selected.as_deref(), Some("o1"));
        assert!(!m.plan_docs["o1"].blocks.is_empty());
        let (m, _) = m.update(Message::PlansSelectOrchestratorPlan("o1".into()));
        assert!(m.orch_plans.selected.is_none(), "selecting the open card again closes it");
    }

    #[test]
    fn plans_tab_picks_up_a_plan_registered_after_it_opened() {
        let _iso = ninox_core::test_isolation::isolate();
        let dir = tempfile::tempdir().unwrap();
        let m = fleet_test_app("o1", "w1");
        let (m, _) = m.update(Message::NavigatePlans);
        assert!(m.orch_plans.entries.is_empty());
        let m = with_registered_plan(m, dir.path());
        let (m, _) = m.update(Message::PollSessions);
        assert_eq!(m.orch_plans.entries.len(), 1);
    }

    #[test]
    fn ended_orchestrator_plans_hide_until_toggled_and_selection_follows() {
        let _iso = ninox_core::test_isolation::isolate();
        let dir = tempfile::tempdir().unwrap();
        let m = with_registered_plan(fleet_test_app("o1", "w1"), dir.path());
        let (m, _) = m.update(Message::NavigatePlans);
        let (m, _) = m.update(Message::PlansSelectOrchestratorPlan("o1".into()));
        let mut ended = m.sessions["o1"].clone();
        ended.status = SessionStatus::Terminated;
        m.engine.store.upsert_session(&ended).unwrap();
        let (m, _) = m.update(Message::PollSessions);
        assert!(m.orch_plans.selected.is_none(), "an ended orchestrator's plan is hidden, so is its selection");
        let (m, _) = m.update(Message::PlansToggleShowEnded);
        assert_eq!(ninox_ui_shared::orchestrator_plans::visible(&m.orch_plans.entries, m.orch_plans.show_ended).len(), 1);
    }

    #[test]
    fn ghost_and_orchestrator_plan_selections_are_exclusive() {
        let _iso = ninox_core::test_isolation::isolate();
        let dir = tempfile::tempdir().unwrap();
        let (m, id) = with_ghost_plan(fleet_test_app("o1", "w1"));
        let m = with_registered_plan(m, dir.path());
        let (m, _) = m.update(Message::NavigatePlans);
        let (m, _) = m.update(Message::PlansSelectOrchestratorPlan("o1".into()));
        let (m, _) = m.update(Message::GhostSelect { plan_id: id, node_id: None });
        assert!(m.orch_plans.selected.is_none() && m.ghost.selected.is_some());
        let (m, _) = m.update(Message::PlansSelectOrchestratorPlan("o1".into()));
        assert!(m.orch_plans.selected.is_some() && m.ghost.selected.is_none());
    }

    #[test]
    fn open_in_plans_clears_an_orchestrator_selection_and_loads_the_list() {
        let _iso = ninox_core::test_isolation::isolate();
        let dir = tempfile::tempdir().unwrap();
        let (m, id) = with_ghost_plan(fleet_test_app("o1", "w1"));
        let mut m = with_registered_plan(m, dir.path());
        m.orch_plans.selected = Some("o1".into());
        assert!(m.orch_plans.entries.is_empty());
        let (m, _) = m.update(Message::GhostOpenInPlans(id));
        assert!(m.orch_plans.selected.is_none());
        assert_eq!(m.orch_plans.entries.len(), 1);
    }

    #[test]
    fn selection_drops_when_the_registration_is_removed() {
        let _iso = ninox_core::test_isolation::isolate();
        let dir = tempfile::tempdir().unwrap();
        let m = with_registered_plan(fleet_test_app("o1", "w1"), dir.path());
        let (m, _) = m.update(Message::NavigatePlans);
        let (m, _) = m.update(Message::PlansSelectOrchestratorPlan("o1".into()));
        m.engine.store.unregister_orchestrator_plan("o1").unwrap();
        let (m, _) = m.update(Message::PollSessions);
        assert!(m.orch_plans.selected.is_none());
    }

    #[test]
    fn open_orchestrator_from_plans_navigates_to_its_plan_panel() {
        let _iso = ninox_core::test_isolation::isolate();
        let dir = tempfile::tempdir().unwrap();
        let m = with_registered_plan(fleet_test_app("o1", "w1"), dir.path());
        let (m, _) = m.update(Message::NavigatePlans);
        let (m, _) = m.update(Message::PlansOpenOrchestrator("o1".into()));
        assert!(matches!(&m.view, View::SessionDetail { session_id, panel: DetailPanel::Plan } if session_id == "o1"));
    }

    #[test]
    fn top_level_ghost_plans_are_fleet_islands() {
        let _iso = ninox_core::test_isolation::isolate();
        let (m, id) = with_ghost_plan(fleet_test_app("o1", "w1"));
        let islands = ninox_ui_shared::ghost_view::ghost_islands(&m.ghost.plans);
        assert_eq!(islands.iter().map(|p| p.plan.id.clone()).collect::<Vec<_>>(), vec![id]);
    }

    fn key_press(key: iced::keyboard::Key, modifiers: iced::keyboard::Modifiers) -> Message {
        let text = match &key {
            iced::keyboard::Key::Character(c) => Some(c.to_string()),
            _ => None,
        };
        Message::RawKey { key, modifiers, text }
    }

    fn esc() -> Message {
        key_press(iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape), iced::keyboard::Modifiers::empty())
    }

    fn delete_plan_for_o1(dirty_worker: bool) -> ninox_core::events::DeletePlan {
        use ninox_core::events::{DeletePlan, DeleteTarget, WorktreeState};
        let target = |id: &str, is_orchestrator: bool| DeleteTarget {
            id: id.into(), name: id.into(), is_orchestrator, running: true, resumable: false,
            depth: usize::from(!is_orchestrator), tree_index: usize::from(!is_orchestrator),
            worktree: (!is_orchestrator).then(|| WorktreeState {
                path: format!("/r/.claude/worktrees/{id}"), uncommitted: dirty_worker, stash: false, unpushed: false,
            }),
        };
        DeletePlan { root: "o1".into(), targets: vec![target("w1", false), target("o1", true)] }
    }

    fn with_delete_confirm_open(m: App, dirty_worker: bool) -> App {
        let (m, _) = m.update(Message::DeleteAsk("o1".into()));
        let (m, _) = m.update(Message::DeletePlanned { root: "o1".into(), plan: Ok(delete_plan_for_o1(dirty_worker)) });
        m
    }

    #[test]
    fn delete_confirms_the_planned_ids_children_first() {
        let m = with_delete_confirm_open(fleet_test_app("o1", "w1"), false);
        assert!(m.delete_modal.as_ref().unwrap().submitted.is_none(), "nothing runs before Delete is pressed");
        let (m, _) = m.update(Message::DeleteConfirm);
        let modal = m.delete_modal.as_ref().unwrap();
        assert_eq!(modal.submitted, Some((vec!["w1".to_string(), "o1".to_string()], false)));
        assert!(modal.running());
        let (m, _) = m.update(esc());
        assert!(m.delete_modal.is_some(), "a running delete can't be dismissed");
    }

    #[test]
    fn delete_passes_the_worktree_override_only_when_ticked() {
        let m = with_delete_confirm_open(fleet_test_app("o1", "w1"), true);
        let (m, _) = m.update(Message::DeleteToggleForce(true));
        let (m, _) = m.update(Message::DeleteConfirm);
        assert_eq!(m.delete_modal.unwrap().submitted.map(|(_, force)| force), Some(true));
    }

    #[test]
    fn escape_cancels_the_delete_confirm() {
        let m = with_delete_confirm_open(fleet_test_app("o1", "w1"), false);
        let (m, _) = m.update(esc());
        assert!(m.delete_modal.is_none());
        assert!(m.sessions.contains_key("o1") && m.sessions.contains_key("w1"));
    }

    #[test]
    fn delete_done_drops_deleted_sessions_and_reports_the_rest() {
        use ninox_core::events::DeleteOutcome;
        let m = with_delete_confirm_open(fleet_test_app("o1", "w1"), true);
        let (m, _) = m.update(Message::DeleteConfirm);
        let (m, _) = m.update(Message::DeleteDone(vec![
            ("w1".into(), DeleteOutcome::KeptWorktree("skipped: dirty".into())),
            ("o1".into(), DeleteOutcome::Deleted),
        ]));
        assert!(!m.sessions.contains_key("o1"));
        assert!(!m.orchestrators.iter().any(|o| o.id == "o1"));
        assert!(m.delete_modal.as_ref().unwrap().results.is_some(), "a kept worktree is reported, not closed silently");

        let (m, _) = fleet_test_app("o1", "w1").update(Message::FleetOpenIsland(ninox_overview::islands::IslandKey::Root("o1".into())));
        let m = with_delete_confirm_open(m, false);
        let (m, _) = m.update(Message::DeleteConfirm);
        let (m, _) = m.update(Message::DeleteDone(vec![("w1".into(), DeleteOutcome::Deleted), ("o1".into(), DeleteOutcome::Deleted)]));
        assert!(m.delete_modal.is_none(), "a clean delete just closes");
        assert_eq!(m.fleet_nav.open, None, "the deleted orchestrator's island isn't left open");
        assert!(m.sessions.is_empty());
    }

    #[test]
    fn delete_key_on_a_selected_fleet_card_opens_the_confirm() {
        let m = fleet_test_app("o1", "w1");
        let island = ninox_overview::islands::IslandKey::Root("o1".into());
        let (m, _) = m.update(Message::FleetOpenIsland(island));
        let (m, _) = m.update(Message::FleetSelectNode("w1".into()));
        let (m, _) = m.update(key_press(iced::keyboard::Key::Named(iced::keyboard::key::Named::Delete), iced::keyboard::Modifiers::empty()));
        assert_eq!(m.delete_modal.as_ref().map(|d| d.root.as_str()), Some("w1"));
    }

    #[test]
    fn the_built_in_session_has_no_delete() {
        let m = fleet_test_app("o1", "w1");
        let (m, _) = m.update(Message::DeleteAsk(ninox_core::worker_type::PROMPT_SESSION_ID.into()));
        assert!(m.delete_modal.is_none());
    }

    fn on_session_page(panel: DetailPanel) -> App {
        let mut m = fleet_test_app("o1", "w1");
        m.view = View::SessionDetail { session_id: "w1".into(), panel };
        m
    }

    fn cmd() -> iced::keyboard::Modifiers { iced::keyboard::Modifiers::LOGO }
    fn named(n: iced::keyboard::key::Named) -> iced::keyboard::Key { iced::keyboard::Key::Named(n) }
    fn ch(c: &str) -> iced::keyboard::Key { iced::keyboard::Key::Character(c.into()) }

    #[test]
    fn mouse_side_buttons_map_to_history_even_over_a_capturing_widget() {
        use iced::{event::Status, mouse::{Button, Event as M}, window::Id, Event};
        for status in [Status::Ignored, Status::Captured] {
            let back = global_event_handler(Event::Mouse(M::ButtonPressed(Button::Back)), status, Id::unique());
            let fwd = global_event_handler(Event::Mouse(M::ButtonPressed(Button::Forward)), status, Id::unique());
            assert!(matches!(back, Some(Message::NavBack)));
            assert!(matches!(fwd, Some(Message::NavForward)));
        }
    }

    #[test]
    fn bracket_chords_navigate_even_when_a_widget_captured_the_key() {
        use iced::{event::Status, keyboard::{Event as K, Modifiers}, window::Id, Event};
        let ev = |c: &str| Event::Keyboard(K::KeyPressed {
            key: ch(c), modified_key: ch(c), physical_key: iced::keyboard::key::Physical::Unidentified(iced::keyboard::key::NativeCode::Unidentified),
            location: iced::keyboard::Location::Standard, modifiers: Modifiers::LOGO, text: None,
        });
        assert!(matches!(global_event_handler(ev("["), Status::Captured, Id::unique()), Some(Message::NavBack)));
        assert!(matches!(global_event_handler(ev("]"), Status::Captured, Id::unique()), Some(Message::NavForward)));
    }

    #[test]
    fn history_chords_map_and_arrows_yield_to_the_terminal() {
        use iced::keyboard::key::Named::{ArrowLeft, ArrowRight};
        assert!(matches!(history_chord(&ch("["), cmd(), false), Some(Message::NavBack)));
        assert!(matches!(history_chord(&ch("]"), cmd(), false), Some(Message::NavForward)));
        assert!(matches!(history_chord(&named(ArrowLeft), cmd(), true), Some(Message::NavBack)));
        assert!(matches!(history_chord(&named(ArrowRight), cmd(), true), Some(Message::NavForward)));
        assert!(history_chord(&named(ArrowLeft), cmd(), false).is_none());
        assert!(history_chord(&ch("["), iced::keyboard::Modifiers::empty(), true).is_none());
    }

    #[test]
    fn cmd_left_is_not_intercepted_while_the_terminal_has_focus() {
        let mut m = on_session_page(DetailPanel::Terminal);
        m.nav.push(crate::nav_history::Location { view: View::Brain, open: None, selected: None, selected_island: None });
        m.terminal_focused = true;
        let (m, _) = m.update(key_press(named(iced::keyboard::key::Named::ArrowLeft), cmd()));
        assert!(matches!(m.view, View::SessionDetail { .. }), "the terminal keeps ⌘←");
        let (m, _) = m.update(key_press(ch("["), cmd()));
        assert!(matches!(m.view, View::Brain), "⌘[ still navigates from the terminal");
    }

    #[test]
    fn fleet_to_orchestrator_to_worker_then_back_back_forward() {
        let m = fleet_test_app("o1", "w1");
        let (m, _) = m.update(Message::FleetOpenIsland(root_key("o1")));
        let (m, _) = m.update(Message::NavigateSession("w1".into()));
        assert!(matches!(&m.view, View::SessionDetail { session_id, .. } if session_id == "w1"));
        let (m, _) = m.update(Message::NavBack);
        assert!(matches!(m.view, View::Fleet));
        assert_eq!(m.fleet_nav.open, Some(root_key("o1")));
        let (m, _) = m.update(Message::NavBack);
        assert!(matches!(m.view, View::Fleet));
        assert_eq!(m.fleet_nav.open, None);
        assert!(!m.nav.can_back());
        let (m, _) = m.update(Message::NavForward);
        assert_eq!(m.fleet_nav.open, Some(root_key("o1")));
        let (m, _) = m.update(Message::NavForward);
        assert!(matches!(&m.view, View::SessionDetail { session_id, .. } if session_id == "w1"));
        assert!(m.clients.is_empty() || m.clients.contains_key("w1"));
    }

    #[test]
    fn selection_only_changes_do_not_create_history() {
        let m = fleet_test_app("o1", "w1");
        let (m, _) = m.update(Message::FleetOpenIsland(root_key("o1")));
        let depth_marker = m.nav.can_back();
        let (m, _) = m.update(Message::FleetSelectNode("w1".into()));
        let (m, _) = m.update(Message::FleetSelectNode("o1".into()));
        assert_eq!(m.nav.can_back(), depth_marker);
        let (m, _) = m.update(Message::NavBack);
        assert_eq!(m.fleet_nav.open, None, "one Back leaves the island, not each card");
    }

    #[test]
    fn keyboard_back_then_forward_round_trips_without_corrupting_history() {
        let m = fleet_test_app("o1", "w1");
        let (m, _) = m.update(Message::NavigateBrain);
        let (m, _) = m.update(Message::NavigatePlans);
        let (m, _) = m.update(key_press(ch("["), cmd()));
        assert!(matches!(m.view, View::Brain));
        let (m, _) = m.update(key_press(ch("["), cmd()));
        assert!(matches!(m.view, View::Fleet));
        let (m, _) = m.update(key_press(ch("]"), cmd()));
        assert!(matches!(m.view, View::Brain));
        let (m, _) = m.update(key_press(ch("]"), cmd()));
        assert!(matches!(m.view, View::Plans));
    }

    #[test]
    fn history_is_inert_while_a_modal_is_open() {
        let mut m = fleet_test_app("o1", "w1");
        m.restart_all_confirm = true;
        let (m, _) = m.update(Message::NavigateBrain);
        let (m, _) = m.update(Message::NavBack);
        assert!(matches!(m.view, View::Brain));
    }

    #[test]
    fn switching_the_session_panel_is_a_history_step() {
        let m = on_session_page(DetailPanel::Terminal);
        let (m, _) = m.update(Message::SwitchDetailPanel(DetailPanel::Plan));
        let (m, _) = m.update(Message::NavBack);
        assert!(matches!(m.view, View::SessionDetail { panel: DetailPanel::Terminal, .. }));
    }

    #[test]
    fn engine_driven_view_changes_push_nothing() {
        let _iso = ninox_core::test_isolation::isolate();
        let m = on_session_page(DetailPanel::Terminal);
        let (m, _) = m.update(Message::PollSessions);
        assert!(!m.nav.can_back());
        let (m, _) = m.update(Message::DeleteDone(vec![("w1".into(), ninox_core::events::DeleteOutcome::Deleted)]));
        assert!(matches!(m.view, View::Fleet), "deleting the viewed session moves off it");
        assert!(!m.nav.can_back(), "an automatic move never enters history");
    }

    #[test]
    fn back_to_a_place_whose_selected_worker_vanished_still_goes_there() {
        let m = fleet_test_app("o1", "w1");
        let (m, _) = m.update(Message::FleetOpenIsland(root_key("o1")));
        let (m, _) = m.update(Message::FleetSelectNode("w1".into()));
        let (mut m, _) = m.update(Message::NavigateBrain);
        m.sessions.remove("w1");
        let (m, _) = m.update(Message::NavBack);
        assert_eq!(m.fleet_nav.open, Some(root_key("o1")));
        assert_eq!(m.fleet_nav.selected, None, "the dead selection is dropped, the place kept");
        assert!(m.notifications.is_empty());
    }

    #[test]
    fn back_skips_a_deleted_session_and_says_so() {
        let m = fleet_test_app("o1", "w1");
        let (m, _) = m.update(Message::NavigateSession("w1".into()));
        let (mut m, _) = m.update(Message::NavigateBrain);
        m.sessions.remove("w1");
        let before = m.notifications.len();
        let (m, _) = m.update(Message::NavBack);
        assert!(matches!(m.view, View::Fleet), "went past the dead session");
        assert_eq!(m.notifications.len(), before + 1);
    }


    #[test]
    fn the_prompt_agent_never_gets_a_session_page() {
        let mut m = fleet_test_app("o1", "w1");
        let mut prompt = refile_session(ninox_core::worker_type::PROMPT_SESSION_ID);
        prompt.worker_type = Some(ninox_core::worker_type::PROMPT_WORKER_TYPE.to_string());
        m.sessions.insert(prompt.id.clone(), prompt);
        assert!(!opens_session_page(&m, ninox_core::worker_type::PROMPT_SESSION_ID));
        let (m, _) = m.update(Message::NavigateSession(ninox_core::worker_type::PROMPT_SESSION_ID.into()));
        assert!(matches!(m.view, View::Fleet), "got {:?}", m.view);

        let mut m = m;
        let mut one_shot = refile_session("ask-1");
        one_shot.worker_type = Some(ninox_core::worker_type::PROMPT_WORKER_TYPE.to_string());
        m.sessions.insert("ask-1".into(), one_shot);
        assert!(opens_session_page(&m, "ask-1"), "an ordinary prompt-type spawn keeps its page");
    }

    #[test]
    fn the_session_page_opens_with_the_terminal_unfocused() {
        let mut m = fleet_test_app("o1", "w1");
        m.terminal_focused = true;
        m.session_more_open = true;
        m.session_confirm = Some(crate::components::session_detail::Confirmable::Kill);
        let (m, _) = m.update(Message::NavigateSession("w1".into()));
        assert!(matches!(&m.view, View::SessionDetail { session_id, .. } if session_id == "w1"));
        assert!(!m.terminal_focused);
        assert!(!m.session_more_open);
        assert!(m.session_confirm.is_none());
    }

    #[test]
    fn c_focuses_the_terminal_then_keys_belong_to_it_until_ctrl_bracket() {
        use iced::keyboard::{Key, Modifiers};
        let m = on_session_page(DetailPanel::Split);
        let (m, _) = m.update(key_press(Key::Character("c".into()), Modifiers::empty()));
        assert!(m.terminal_focused);

        let (m, _) = m.update(esc());
        assert!(matches!(m.view, View::SessionDetail { .. }), "focused, Esc is the agent's");
        assert!(m.terminal_focused);
        let (m, _) = m.update(key_press(Key::Character("1".into()), Modifiers::empty()));
        assert!(matches!(m.view, View::SessionDetail { .. }), "focused, digits are the agent's");

        let (m, _) = m.update(key_press(Key::Character("]".into()), Modifiers::CTRL));
        assert!(!m.terminal_focused);
        assert!(matches!(m.view, View::SessionDetail { .. }));
    }

    #[test]
    fn esc_on_an_unfocused_session_page_goes_back_to_its_island() {
        let (m, _) = on_session_page(DetailPanel::Terminal).update(esc());
        assert!(matches!(m.view, View::Fleet));
        assert_eq!(m.fleet_nav.selected.as_deref(), Some("w1"));
        assert_eq!(m.fleet_nav.open, Some(ninox_overview::islands::IslandKey::Root("o1".into())));
    }

    #[test]
    fn connect_from_a_tab_without_a_terminal_switches_to_terminal() {
        let (m, _) = on_session_page(DetailPanel::Info).update(Message::TerminalFocus);
        assert!(matches!(m.view, View::SessionDetail { panel: DetailPanel::Terminal, .. }));
        assert!(m.terminal_focused);
    }

    #[test]
    fn leaving_the_terminal_tabs_releases_the_keyboard() {
        let mut m = on_session_page(DetailPanel::Split);
        m.terminal_focused = true;
        let (m, _) = m.update(Message::SwitchDetailPanel(DetailPanel::Terminal));
        assert!(m.terminal_focused, "Split → Terminal keeps focus");
        let (m, _) = m.update(Message::SwitchDetailPanel(DetailPanel::Diff));
        assert!(!m.terminal_focused);
    }

    #[test]
    fn a_terminal_click_survives_its_own_mouse_press_but_a_click_elsewhere_releases() {
        let m = on_session_page(DetailPanel::Terminal);
        let (m, _) = m.update(Message::TerminalClicked);
        let (m, _) = m.update(Message::MousePressed);
        assert!(m.terminal_focused);
        let (m, _) = m.update(Message::MousePressed);
        assert!(!m.terminal_focused);
    }

    #[test]
    fn kill_and_restart_fire_only_on_confirm() {
        use crate::components::session_detail::Confirmable;
        let m = on_session_page(DetailPanel::Split);
        let (m, _) = m.update(Message::SessionConfirmAsk(Confirmable::Kill));
        assert_eq!(m.session_confirm, Some(Confirmable::Kill));
        assert!(m.sessions.contains_key("w1"));

        let (m, _) = m.update(esc());
        assert!(m.session_confirm.is_none(), "Esc cancels");
        assert!(matches!(m.view, View::SessionDetail { .. }), "and doesn't also go back");

        let (m, _) = m.update(Message::SessionConfirmAsk(Confirmable::Restart));
        let (m, _) = m.update(Message::SessionConfirmCancel);
        assert!(m.session_confirm.is_none());

        let (m, _) = m.update(Message::SessionConfirmAsk(Confirmable::Kill));
        let (m, _) = m.update(Message::SessionConfirmGo);
        assert!(m.sessions.contains_key("w1"), "Kill stops the agent; only Delete… removes the row");
        assert!(m.session_confirm.is_none());
    }

    #[test]
    fn the_more_menu_closes_on_an_outside_click_or_a_pick() {
        let m = on_session_page(DetailPanel::Split);
        let (m, _) = m.update(Message::SessionMoreToggle);
        assert!(m.session_more_open);
        let (m, _) = m.update(Message::MousePressed);
        assert!(!m.session_more_open, "outside click");

        let (m, _) = m.update(Message::SessionMoreToggle);
        let (m, _) = m.update(Message::SessionMoreHover(true));
        let (m, _) = m.update(Message::MousePressed);
        assert!(m.session_more_open, "a click on the menu itself keeps it open");

        let (m, task) = m.update(Message::SessionMenuPick(crate::components::session_detail::SessionAction::Refile));
        assert!(!m.session_more_open);
        assert!(iced_runtime::task::into_stream(task).is_some(), "re-file runs");

        let (m, _) = m.update(Message::SessionMoreToggle);
        let (m, _) = m.update(esc());
        assert!(!m.session_more_open, "Esc closes the menu");
        assert!(matches!(m.view, View::SessionDetail { .. }));
    }

    #[test]
    fn send_message_opens_the_composer_and_esc_cancels_it() {
        let mut m = on_session_page(DetailPanel::Split);
        m.terminal_focused = true;
        let (m, _) = m.update(Message::FleetSendMessageOpen("w1".into()));
        assert!(!m.terminal_focused);
        assert!(m.fleet_message_draft.as_ref().is_some_and(|(id, _)| id == "w1"));
        let (m, _) = m.update(esc());
        assert!(m.fleet_message_draft.is_none());
        assert!(matches!(m.view, View::SessionDetail { .. }));
    }

    #[test]
    fn an_open_composer_never_eats_a_focused_terminals_keys() {
        let mut m = on_session_page(DetailPanel::Split);
        m.fleet_nav.selected = Some("w1".into());
        m.fleet_message_draft = Some(("w1".into(), "half-typed".into()));
        m.terminal_focused = true;
        let (m, _) = m.update(esc());
        assert!(m.fleet_message_draft.is_some(), "Esc went to the agent, not the composer");
        assert!(m.terminal_focused);
    }

    #[test]
    fn an_armed_confirm_never_carries_over_to_another_session_page() {
        use crate::components::session_detail::Confirmable;
        let mut m = fleet_test_app("o1", "w1");
        m.sessions.insert("w2".into(), refile_session("w2"));
        m.view = View::SessionDetail { session_id: "w1".into(), panel: DetailPanel::Split };
        m.session_confirm = Some(Confirmable::Kill);
        m.session_more_open = true;
        m.terminal_focused = true;
        App::enter_session_page(&mut m, "w2", DetailPanel::Terminal);
        assert!(m.session_confirm.is_none());
        assert!(!m.session_more_open);
        assert!(!m.terminal_focused);
    }

    #[test]
    fn an_orchestrator_page_stores_the_panel_it_actually_shows() {
        let mut m = fleet_test_app("o1", "w1");
        m.worker_panel = DetailPanel::Info;
        let (m, _) = m.update(Message::NavigateSession("o1".into()));
        assert!(matches!(m.view, View::SessionDetail { panel: DetailPanel::Terminal, .. }), "{:?}", m.view);
    }

    #[test]
    fn the_config_banner_row_is_counted_in_the_terminal_height() {
        let mut m = on_session_page(DetailPanel::Terminal);
        m.window_width = 1440.0;
        m.window_height = 944.0;
        App::resize_terminals(&mut m);
        let rows = m.terminal_rows;
        m.config_warning = Some("bad config".into());
        App::resize_terminals(&mut m);
        assert!(m.terminal_rows < rows, "{} vs {rows}", m.terminal_rows);
    }

    #[test]
    fn pop_out_prefers_kitty_and_falls_back_to_a_command_script() {
        let (program, args, body) = pop_out_plan(true, "/bin/ninox", "w1", "/tmp/x.command");
        assert_eq!(program, "open");
        assert_eq!(args, ["-na", "kitty", "--args", "/bin/ninox", "connect", "w1"]);
        assert!(body.is_none());

        let (program, args, body) = pop_out_plan(false, "/Apps/My Ninox/ninox", "it's", "/tmp/x.command");
        assert_eq!(program, "open");
        assert_eq!(args, ["/tmp/x.command"]);
        assert_eq!(body.as_deref(), Some("#!/bin/sh\nexec '/Apps/My Ninox/ninox' connect 'it'\\''s'\n"));
    }

    /// The prompt-agent singleton (MLOPS-5415) lives in `app.sessions` like
    /// any other session (so "Open pane" keeps working), but
    /// `fleet_islands::fleet_snapshot` rebuilds its own `FleetSnapshot` from
    /// `app.sessions` directly rather than via `ninox_core::fleet::snapshot
    /// ::FleetSnapshot::load` — which already excludes it — so it needs its
    /// own copy of that exclusion. Without it, the prompt agent showed up
    /// as an "Unparented sessions" island member in the GUI (round-1
    /// try-out feedback on PR #273).
    #[test]
    fn build_fleet_islands_never_includes_the_prompt_agent_session() {
        let mut m = fleet_test_app("o1", "w1");
        let mut prompt = refile_session(ninox_core::worker_type::PROMPT_SESSION_ID);
        prompt.orchestrator_id = None;
        prompt.status = SessionStatus::Working;
        prompt.worker_type = Some(ninox_core::worker_type::PROMPT_WORKER_TYPE.to_string());
        m.sessions.insert(ninox_core::worker_type::PROMPT_SESSION_ID.into(), prompt);

        let islands = build_fleet_islands(&m);
        for island in &islands {
            assert!(
                island.members.iter().all(|member| member.id != ninox_core::worker_type::PROMPT_SESSION_ID),
                "the prompt-agent singleton must never be a fleet-island member",
            );
        }
    }

    fn raw_key(chord: &crate::components::help_overlay::Chord) -> Message {
        use crate::components::help_overlay::Key as K;
        use iced::keyboard::{key::Named, Key, Modifiers};
        let (key, text) = match chord.key {
            K::Char(c) => (Key::Character(c.into()), Some(c.to_string())),
            K::Escape => (Key::Named(Named::Escape), None),
            K::Enter => (Key::Named(Named::Enter), None),
            K::Up => (Key::Named(Named::ArrowUp), None),
            K::Down => (Key::Named(Named::ArrowDown), None),
            K::Left => (Key::Named(Named::ArrowLeft), None),
            K::Right => (Key::Named(Named::ArrowRight), None),
        };
        let mut modifiers = Modifiers::empty();
        if chord.cmd {
            modifiers |= Modifiers::COMMAND;
        }
        if chord.shift {
            modifiers |= Modifiers::SHIFT;
        }
        if chord.ctrl {
            modifiers |= Modifiers::CTRL;
        }
        Message::RawKey { key, modifiers, text }
    }

    /// Everything a listed shortcut can visibly change without returning a
    /// task.
    fn key_fingerprint(m: &App) -> String {
        format!(
            "{:?}|{:?}|{}|{:?}|{:?}|{:?}|{:?}|{:?}|{}|{}",
            m.view,
            m.fleet_nav,
            m.zoom,
            m.active_variant,
            m.sidebar,
            m.brain_view.mode,
            m.brain_view.graph.focus,
            m.spawn_modal,
            m.help_open,
            m.terminal_focused,
        )
    }

    /// Apps in which a shortcut of `scope` is expected to act; a chord
    /// passes if it does something in at least one of them.
    fn key_setups(scope: crate::components::help_overlay::Scope) -> Vec<App> {
        use crate::components::help_overlay::Scope;
        let island = ninox_overview::islands::IslandKey::Root("o1".into());
        let selected = || fleet_test_app("o1", "w1").update(Message::FleetSelectIsland(island.clone())).0;
        let opened = || selected().update(Message::FleetSelectIsland(island.clone())).0;
        let on_brain = |mut m: App| {
            m.view = View::Brain;
            m
        };
        match scope {
            Scope::Anywhere => {
                let mut zoomed = base(test_engine());
                zoomed.zoom = 1.2;
                vec![fleet_test_app("o1", "w1"), on_brain(base(test_engine())), opened(), zoomed]
            }
            Scope::Fleet => vec![selected(), opened()],
            Scope::Session => {
                let on_page = || {
                    let mut m = fleet_test_app("o1", "w1");
                    m.view = View::SessionDetail { session_id: "w1".into(), panel: DetailPanel::Terminal };
                    m
                };
                let mut focused = on_page();
                focused.terminal_focused = true;
                vec![on_page(), focused]
            }
            Scope::Brain => vec![on_brain(base(test_engine()))],
            Scope::BrainGraph => {
                let dir = tempdir().unwrap().keep();
                // A hub with neighbours all round it, so every arrow has
                // somewhere to walk.
                let spokes: Vec<String> = (1..=8).map(|i| format!("n{i}")).collect();
                let links: String = spokes.iter().map(|n| format!("[[{n}]] ")).collect();
                std::fs::write(dir.join("a.md"), format!("---\nname: a\n---\n{links}\n")).unwrap();
                for n in &spokes {
                    std::fs::write(dir.join(format!("{n}.md")), format!("---\nname: {n}\n---\n[[a]]\n")).unwrap();
                }
                let brain = Arc::new(BrainIndex::open(&dir).unwrap());
                brain.rebuild(None).unwrap();
                let (m, _) = base_with_brain(test_engine(), brain).update(Message::NavigateBrain);
                vec![m.update(Message::BrainShowInGraph("a.md".into())).0]
            }
            Scope::Spawn => vec![base(test_engine()).update(Message::SpawnSession).0],
        }
    }

    /// The overlay can't list a dead key: every chord in it, replayed
    /// through the real `RawKey` handler where its section says it applies,
    /// returns a task or changes visible state.
    #[test]
    fn every_listed_shortcut_does_something() {
        let _iso = ninox_core::test_isolation::isolate();
        for section in crate::components::help_overlay::SECTIONS {
            for shortcut in section.shortcuts {
                for chord in shortcut.chords {
                    let acted = key_setups(section.scope).into_iter().any(|m| {
                        let before = key_fingerprint(&m);
                        let (m, task) = m.update(raw_key(chord));
                        iced_runtime::task::into_stream(task).is_some() || key_fingerprint(&m) != before
                    });
                    assert!(acted, "{} › {}: {chord:?} does nothing", section.scope.title(), shortcut.action);
                }
            }
        }
    }

    #[test]
    fn question_mark_opens_help_and_esc_or_question_mark_closes_it() {
        use crate::components::help_overlay::{Chord, Key};
        let q = Chord { key: Key::Char("?"), cmd: false, shift: false, ctrl: false };
        let esc = Chord { key: Key::Escape, cmd: false, shift: false, ctrl: false };
        let (m, _) = base(test_engine()).update(raw_key(&q));
        assert!(m.help_open);
        let (m, _) = m.update(raw_key(&esc));
        assert!(!m.help_open);
        let (m, _) = m.update(raw_key(&q));
        let (m, _) = m.update(raw_key(&q));
        assert!(!m.help_open);
    }

    #[test]
    fn open_help_swallows_other_keys() {
        use crate::components::help_overlay::{Chord, Key};
        let (m, _) = base(test_engine()).update(Message::ToggleHelp);
        let (m, _) = m.update(raw_key(&Chord { key: Key::Char("4"), cmd: false, shift: false, ctrl: false }));
        assert!(m.help_open);
        assert!(matches!(m.view, View::Fleet));
    }

    #[test]
    fn bracket_toggles_the_tree() {
        use crate::components::help_overlay::{Chord, Key};
        let bracket = Chord { key: Key::Char("["), cmd: false, shift: false, ctrl: false };
        let m = base(test_engine());
        let before = m.fleet_nav.tree_collapsed;
        let (m, _) = m.update(raw_key(&bracket));
        assert_eq!(m.fleet_nav.tree_collapsed, !before);
        let (m, _) = m.update(raw_key(&bracket));
        assert_eq!(m.fleet_nav.tree_collapsed, before);
    }

    /// A store holding an interrupted orchestrator and worker, flagged as
    /// awaiting a restore the way an engine start after a reboot leaves it.
    fn interrupted_store() -> (Arc<Store>, std::path::PathBuf) {
        let dir = tempdir().unwrap().keep();
        let store = Arc::new(Store::open(dir.join("t.db")).unwrap());
        let ws = dir.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        for (id, orch) in [("o1", None), ("w1", Some("o1"))] {
            let mut s = refile_session(id);
            s.orchestrator_id = orch.map(str::to_string);
            s.status = SessionStatus::Interrupted;
            s.workspace_path = Some(ws.to_string_lossy().into_owned());
            s.claude_session_id = Some(format!("uuid-{id}"));
            store.upsert_session(&s).unwrap();
            store.record_interruption(id, 10, &SessionStatus::Working, Some("reboot")).unwrap();
        }
        store.upsert_orchestrator(&Orchestrator { id: "o1".into(), name: "o".into(), created_at: 0 }).unwrap();
        ninox_core::fleet::startup::flag_pending_restore(&store, 20);
        (store, dir)
    }

    fn app_on(store: Arc<Store>, dir: &std::path::Path) -> App {
        let brain = Arc::new(BrainIndex::open(tempdir().unwrap().keep()).unwrap());
        App::new(
            Engine::new(store),
            std::path::PathBuf::from("/tmp"),
            brain,
            dir.join("t.db"),
        )
        .0
    }

    #[test]
    fn an_interrupted_fleet_is_offered_for_restore_at_startup() {
        let _iso = ninox_core::test_isolation::isolate();
        let (store, dir) = interrupted_store();
        let m = app_on(store, &dir);
        let offer = m
            .notifications
            .iter()
            .find(|n| n.kind == NotificationKind::FleetRestorePending)
            .expect("restore offer");
        assert!(offer.body.contains("1 worker") && offer.body.contains("1 orchestrator"), "{}", offer.body);
    }

    #[test]
    fn dismissing_the_restore_offer_clears_the_pending_flag() {
        let _iso = ninox_core::test_isolation::isolate();
        let (store, dir) = interrupted_store();
        let m = app_on(store.clone(), &dir);
        let id = m.notifications.iter().find(|n| n.kind == NotificationKind::FleetRestorePending).unwrap().id.clone();
        let (m, _) = m.update(Message::DismissNotification(id));
        assert!(!m.notifications.iter().any(|n| n.kind == NotificationKind::FleetRestorePending));
        assert!(ninox_core::fleet::startup::pending_restore(&store).is_none());
    }

    #[test]
    fn confirming_while_another_restore_runs_starts_nothing() {
        use crate::components::restore_modal::{RestoreModal, RestoreScope};
        let _iso = ninox_core::test_isolation::isolate();
        let (store, dir) = interrupted_store();
        let mut m = app_on(store.clone(), &dir);
        let now = chrono::Utc::now().timestamp_millis();
        assert!(store.try_acquire_fleet_lock("restore", "pid-other-1", now, now - 1).unwrap());
        let mut modal = RestoreModal::loaded(Ok(ninox_core::fleet::RestorePlan {
            pending_briefings: vec!["o1".into()],
            ..Default::default()
        }));
        modal.confirm = Some(RestoreScope::All);
        m.restore_modal = Some(modal);
        let (m, task) = m.update(Message::RestoreFleetConfirm);
        assert!(iced_runtime::task::into_stream(task).is_none());
        assert!(m.restore_modal.as_ref().unwrap().running.is_some());
    }

    #[test]
    fn dismissing_all_notifications_also_declines_the_restore() {
        let _iso = ninox_core::test_isolation::isolate();
        let (store, dir) = interrupted_store();
        let m = app_on(store.clone(), &dir);
        let (_m, _) = m.update(Message::DismissAllNotifications);
        assert!(ninox_core::fleet::startup::pending_restore(&store).is_none());
    }

    #[tokio::test]
    async fn opening_restore_loads_the_plan_and_confirming_starts_a_restore() {
        use crate::components::restore_modal::RestoreScope;
        let _iso = ninox_core::test_isolation::isolate();
        let (store, dir) = interrupted_store();
        let m = app_on(store, &dir);
        let (m, task) = m.update(Message::RestoreFleetOpen);
        assert!(m.restore_modal.as_ref().is_some_and(|r| r.plan.is_none()), "loading");
        let (m, _) = m.update(output_of(task).await);
        let modal = m.restore_modal.as_ref().unwrap();
        assert!(modal.selected.contains("w1") && modal.selected.contains("o1"), "{:?}", modal.selected);
        assert_eq!(modal.running, None);

        let (m, _) = m.update(Message::RestoreFleetToggle("o1".into()));
        let (m, task) = m.update(Message::RestoreFleetConfirm);
        assert!(iced_runtime::task::into_stream(task).is_none(), "nothing starts without the confirm step");
        let (m, _) = m.update(Message::RestoreFleetRequest(RestoreScope::Selected));
        assert_eq!(m.restore_modal.as_ref().unwrap().confirm, Some(RestoreScope::Selected));
        // The returned task would spawn the detached restore; it is never run here.
        let (m, task) = m.update(Message::RestoreFleetConfirm);
        assert!(iced_runtime::task::into_stream(task).is_some());
        assert_eq!(m.restore_modal.as_ref().unwrap().confirm, None);

        let (m, _) = m.update(Message::RestoreFleetStarted(Ok("restoring fleet in the background".into())));
        assert!(m.restore_modal.as_ref().unwrap().outcome.as_ref().is_some_and(|o| o.is_ok()));
        assert!(!m.notifications.iter().any(|n| n.kind == NotificationKind::FleetRestorePending));
        let (m, _) = m.update(Message::RestoreFleetClose);
        assert!(m.restore_modal.is_none());
    }

    #[test]
    fn esc_backs_out_of_the_restore_confirm_then_closes_the_modal() {
        use crate::components::restore_modal::{RestoreModal, RestoreScope};
        let esc = Message::RawKey {
            key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape),
            modifiers: iced::keyboard::Modifiers::empty(),
            text: None,
        };
        let mut m = base(test_engine());
        let mut modal = RestoreModal::loaded(Ok(Default::default()));
        modal.confirm = Some(RestoreScope::All);
        m.restore_modal = Some(modal);
        let (m, _) = m.update(esc.clone());
        assert!(m.restore_modal.as_ref().is_some_and(|r| r.confirm.is_none()));
        let (m, _) = m.update(esc);
        assert!(m.restore_modal.is_none());
    }

    #[test]
    fn fleet_select_island_single_click_only_selects_not_opens() {
        let m = fleet_test_app("o1", "w1");
        let key = ninox_overview::islands::IslandKey::Root("o1".into());
        let (m, _) = m.update(Message::FleetSelectIsland(key.clone()));
        assert_eq!(m.fleet_nav.selected_island, Some(key));
        assert_eq!(m.fleet_nav.open, None);
    }

    #[test]
    fn fleet_select_island_double_click_opens_it() {
        let m = fleet_test_app("o1", "w1");
        let key = ninox_overview::islands::IslandKey::Root("o1".into());
        let (m, _) = m.update(Message::FleetSelectIsland(key.clone()));
        let (m, _) = m.update(Message::FleetSelectIsland(key.clone()));
        assert_eq!(m.fleet_nav.open, Some(key));
    }

    #[test]
    fn fleet_island_member_row_single_click_selects_its_island() {
        let m = fleet_test_app("o1", "w1");
        let island = ninox_overview::islands::IslandKey::Root("o1".into());
        let (m, _) = m.update(Message::FleetIslandMemberPressed { island: island.clone(), id: "w1".into() });
        assert_eq!(m.fleet_nav.selected_island, Some(island));
        assert_eq!(m.fleet_nav.open, None);
    }

    #[test]
    fn fleet_island_member_row_double_click_opens_the_session_view() {
        let m = fleet_test_app("o1", "w1");
        let island = ninox_overview::islands::IslandKey::Root("o1".into());
        let press = || Message::FleetIslandMemberPressed { island: island.clone(), id: "w1".into() };
        let (m, _) = m.update(press());
        let (m, _) = m.update(press());
        assert!(matches!(&m.view, View::SessionDetail { session_id, .. } if session_id == "w1"), "got {:?}", m.view);
    }

    #[test]
    fn fleet_node_card_single_click_only_selects() {
        let m = fleet_test_app("o1", "w1");
        let (m, _) = m.update(Message::FleetNodeCardPressed("o1".into()));
        assert_eq!(m.fleet_nav.selected.as_deref(), Some("o1"));
        assert!(matches!(m.view, View::Fleet), "got {:?}", m.view);
    }

    #[test]
    fn fleet_node_card_double_click_opens_the_session_view() {
        for id in ["o1", "w1"] {
            let m = fleet_test_app("o1", "w1");
            let (m, _) = m.update(Message::FleetNodeCardPressed(id.into()));
            let (m, _) = m.update(Message::FleetNodeCardPressed(id.into()));
            assert!(matches!(&m.view, View::SessionDetail { session_id, .. } if session_id == id), "got {:?}", m.view);
        }
    }

    #[test]
    fn fleet_node_card_clicks_on_two_different_cards_do_not_open() {
        let m = fleet_test_app("o1", "w1");
        let (m, _) = m.update(Message::FleetNodeCardPressed("o1".into()));
        let (m, _) = m.update(Message::FleetNodeCardPressed("w1".into()));
        assert_eq!(m.fleet_nav.selected.as_deref(), Some("w1"));
        assert!(matches!(m.view, View::Fleet), "got {:?}", m.view);
    }

    #[test]
    fn fleet_select_island_two_clicks_on_different_islands_does_not_open() {
        let m = fleet_test_app("o1", "w1");
        let a = ninox_overview::islands::IslandKey::Root("o1".into());
        let b = ninox_overview::islands::IslandKey::Unparented;
        let (m, _) = m.update(Message::FleetSelectIsland(a));
        let (m, _) = m.update(Message::FleetSelectIsland(b.clone()));
        assert_eq!(m.fleet_nav.selected_island, Some(b));
        assert_eq!(m.fleet_nav.open, None);
    }

    #[test]
    fn fleet_open_island_switches_to_the_fleet_tab_and_opens_it() {
        let mut m = fleet_test_app("o1", "w1");
        m.view = View::Brain;
        let key = ninox_overview::islands::IslandKey::Root("o1".into());
        let (m, _) = m.update(Message::FleetOpenIsland(key.clone()));
        assert!(matches!(m.view, View::Fleet));
        assert_eq!(m.fleet_nav.open, Some(key));
    }

    #[test]
    fn fleet_open_node_opens_the_owning_island_with_the_node_selected() {
        let m = fleet_test_app("o1", "w1");
        let (m, _) = m.update(Message::FleetOpenNode("w1".into()));
        assert!(matches!(m.view, View::Fleet));
        assert_eq!(m.fleet_nav.open, Some(ninox_overview::islands::IslandKey::Root("o1".into())));
        assert_eq!(m.fleet_nav.selected.as_deref(), Some("w1"));
    }

    #[test]
    fn fleet_select_node_only_highlights_it() {
        let m = fleet_test_app("o1", "w1");
        let (m, _) = m.update(Message::FleetSelectNode("w1".into()));
        assert_eq!(m.fleet_nav.selected.as_deref(), Some("w1"));
        assert_eq!(m.fleet_nav.open, None);
    }

    #[test]
    fn fleet_send_message_open_sets_the_draft_for_that_node() {
        let m = fleet_test_app("o1", "w1");
        let (m, _) = m.update(Message::FleetSendMessageOpen("w1".into()));
        assert_eq!(m.fleet_message_draft, Some(("w1".to_string(), String::new())));
    }

    #[test]
    fn fleet_send_message_changed_updates_the_open_drafts_text() {
        let m = fleet_test_app("o1", "w1");
        let (m, _) = m.update(Message::FleetSendMessageOpen("w1".into()));
        let (m, _) = m.update(Message::FleetSendMessageChanged("hello".into()));
        assert_eq!(m.fleet_message_draft, Some(("w1".to_string(), "hello".to_string())));
    }

    #[test]
    fn fleet_send_message_cancel_clears_the_draft() {
        let m = fleet_test_app("o1", "w1");
        let (m, _) = m.update(Message::FleetSendMessageOpen("w1".into()));
        let (m, _) = m.update(Message::FleetSendMessageCancel);
        assert_eq!(m.fleet_message_draft, None);
    }

    #[test]
    fn fleet_send_message_submit_clears_the_draft() {
        let _iso = ninox_core::test_isolation::isolate();
        let m = fleet_test_app("o1", "w1");
        let (m, _) = m.update(Message::FleetSendMessageOpen("w1".into()));
        let (m, _) = m.update(Message::FleetSendMessageChanged("hello".into()));
        let (m, _) = m.update(Message::FleetSendMessageSubmit);
        assert_eq!(m.fleet_message_draft, None);
    }

    #[test]
    fn fleet_send_message_submit_with_empty_text_is_a_noop_task_but_still_closes() {
        let m = fleet_test_app("o1", "w1");
        let (m, _) = m.update(Message::FleetSendMessageOpen("w1".into()));
        let (m, _) = m.update(Message::FleetSendMessageSubmit);
        assert_eq!(m.fleet_message_draft, None);
    }

    #[test]
    fn deleting_a_session_clears_a_matching_fleet_selection_and_draft() {
        let m = fleet_test_app("o1", "w1");
        let (m, _) = m.update(Message::FleetSelectNode("w1".into()));
        let (m, _) = m.update(Message::FleetSendMessageOpen("w1".into()));
        let (m, _) = m.update(Message::DeleteDone(vec![("w1".into(), ninox_core::events::DeleteOutcome::Deleted)]));
        assert_eq!(m.fleet_nav.selected, None);
        assert_eq!(m.fleet_message_draft, None);
    }

    #[test]
    fn selecting_a_different_node_discards_a_stale_draft() {
        let m = fleet_test_app("o1", "w1");
        let (m, _) = m.update(Message::FleetSendMessageOpen("o1".into()));
        let (m, _) = m.update(Message::FleetSelectNode("w1".into()));
        assert_eq!(m.fleet_message_draft, None);
    }

    #[test]
    fn reselecting_the_same_node_keeps_its_own_draft() {
        let m = fleet_test_app("o1", "w1");
        let (m, _) = m.update(Message::FleetSendMessageOpen("w1".into()));
        let (m, _) = m.update(Message::FleetSendMessageChanged("hi".into()));
        let (m, _) = m.update(Message::FleetSelectNode("w1".into()));
        assert_eq!(m.fleet_message_draft, Some(("w1".to_string(), "hi".to_string())));
    }

    #[test]
    fn closing_the_island_clears_a_stale_draft() {
        let m = fleet_test_app("o1", "w1");
        let key = ninox_overview::islands::IslandKey::Root("o1".into());
        let (m, _) = m.update(Message::FleetOpenIsland(key));
        let (m, _) = m.update(Message::FleetSendMessageOpen("w1".into()));
        let (m, _) = m.update(Message::FleetCloseIsland);
        assert_eq!(m.fleet_message_draft, None);
    }

    #[test]
    fn esc_falls_through_to_clearing_selection_when_the_draft_no_longer_matches_it() {
        let m = fleet_test_app("o1", "w1");
        let key = ninox_overview::islands::IslandKey::Root("o1".into());
        let (m, _) = m.update(Message::FleetOpenIsland(key));
        // A draft addressed to a node that is not the current selection —
        // shouldn't normally happen given `discard_draft_for_a_different_
        // node`, but the Esc handler must not rely on that alone.
        let mut m = m;
        m.fleet_message_draft = Some(("some-other-node".to_string(), "draft".to_string()));

        let (m, _) = m.update(escape_key());
        assert_eq!(m.fleet_nav.selected, None, "Esc still clears the real selection");
        assert_eq!(
            m.fleet_message_draft,
            Some(("some-other-node".to_string(), "draft".to_string())),
            "a draft that doesn't match the selection is untouched by this Esc",
        );
    }

    fn escape_key() -> Message {
        Message::RawKey {
            key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape),
            modifiers: iced::keyboard::Modifiers::default(),
            text: None,
        }
    }

    #[test]
    fn esc_first_clears_the_selection_then_a_second_esc_closes_the_island() {
        let m = fleet_test_app("o1", "w1");
        let key = ninox_overview::islands::IslandKey::Root("o1".into());
        let (m, _) = m.update(Message::FleetOpenIsland(key.clone()));
        // Opening an island auto-selects its root (`FleetNav::open_island`)
        // — the detail panel/action row are showing, so the first Esc has
        // something to clear.
        assert_eq!(m.fleet_nav.selected.as_deref(), Some("o1"));

        let (m, _) = m.update(escape_key());
        assert_eq!(m.fleet_nav.selected, None, "first Esc clears the selection");
        assert_eq!(m.fleet_nav.open, Some(key.clone()), "first Esc keeps the island open");

        let (m, _) = m.update(escape_key());
        assert_eq!(m.fleet_nav.open, None, "second Esc returns to Fleet");
        assert_eq!(m.fleet_nav.selected_island, Some(key));
    }

    #[test]
    fn esc_closes_an_opened_island_immediately_when_nothing_is_selected() {
        let m = fleet_test_app("o1", "w1");
        let key = ninox_overview::islands::IslandKey::Root("o1".into());
        let (mut m, _) = m.update(Message::FleetOpenIsland(key.clone()));
        m.fleet_nav.clear_selection();

        let (m, _) = m.update(escape_key());
        assert_eq!(m.fleet_nav.open, None);
        assert_eq!(m.fleet_nav.selected_island, Some(key));
    }

    #[test]
    fn esc_cancels_the_send_message_composer_without_touching_the_selection() {
        let m = fleet_test_app("o1", "w1");
        let key = ninox_overview::islands::IslandKey::Root("o1".into());
        let (m, _) = m.update(Message::FleetOpenIsland(key.clone()));
        let (m, _) = m.update(Message::FleetSendMessageOpen("o1".into()));
        assert!(m.fleet_message_draft.is_some());

        let (m, _) = m.update(escape_key());
        assert_eq!(m.fleet_message_draft, None, "first Esc cancels the composer");
        assert_eq!(m.fleet_nav.selected.as_deref(), Some("o1"), "selection untouched");
        assert_eq!(m.fleet_nav.open, Some(key), "island stays open");
    }

    #[test]
    fn esc_on_the_fleet_grid_with_nothing_open_is_a_noop() {
        // No island open — Esc must fall through untouched (digit shortcuts
        // etc. still work), not be swallowed like the modal-close checks
        // above, and must not disturb any nav state.
        let m = fleet_test_app("o1", "w1");
        let before = m.fleet_nav.clone();
        let (m, _) = m.update(Message::RawKey {
            key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape),
            modifiers: iced::keyboard::Modifiers::default(),
            text: None,
        });
        assert_eq!(m.fleet_nav, before);
    }

    #[test]
    fn enter_opens_the_selected_island_on_the_fleet_tab() {
        let m = fleet_test_app("o1", "w1");
        let key = ninox_overview::islands::IslandKey::Root("o1".into());
        let (m, _) = m.update(Message::FleetSelectIsland(key.clone()));
        let (m, _) = m.update(Message::RawKey {
            key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter),
            modifiers: iced::keyboard::Modifiers::default(),
            text: None,
        });
        assert_eq!(m.fleet_nav.open, Some(key));
    }

    #[test]
    fn ninox_prompt_input_and_submit() {
        let e = test_engine();
        let m = base(e);
        let (m, _) = m.update(Message::NinoxPromptChanged("spawn a thing".into()));
        assert_eq!(m.ninox_prompt_draft, "spawn a thing");
        let (m, _) = m.update(Message::NinoxPromptSubmitted);
        assert!(m.ninox_prompt_draft.is_empty());
    }

    /// Submitting opens the drawer with the question as a pending turn
    /// before the delivery task has even run.
    #[test]
    fn submit_opens_the_drawer_immediately_and_records_history() {
        let m = base(test_engine());
        let (m, _) = m.update(Message::NinoxPromptChanged("spawn a thing".into()));
        let (m, task) = m.update(Message::NinoxPromptSubmitted);
        assert!(m.ninox_reply_drawer.is_open());
        let turn = &m.ninox_reply_drawer.turns[0];
        assert_eq!((turn.question.as_str(), turn.target_label.as_str()), ("spawn a thing", "ninox"));
        assert_eq!(turn.state, crate::prompt_agent::AnswerState::Pending);
        assert_eq!(m.ninox_prompt_history, vec!["spawn a thing".to_string()]);
        assert!(iced_runtime::task::into_stream(task).is_some(), "submit must dispatch a real delivery task");
    }

    #[test]
    fn submitting_blank_text_is_a_noop() {
        let m = base(test_engine());
        let (m, _) = m.update(Message::NinoxPromptChanged("   ".into()));
        let (m, task) = m.update(Message::NinoxPromptSubmitted);
        assert!(!m.ninox_reply_drawer.has_content());
        assert!(m.ninox_prompt_history.is_empty());
        assert!(iced_runtime::task::into_stream(task).is_none());
    }

    /// G2 spec item 6: a picked orchestrator's delivery task must target
    /// that orchestrator's session id (no real tmux session exists here, so
    /// delivery fails — the routing is what's under test).
    #[tokio::test]
    async fn submit_to_a_picked_orchestrator_targets_its_session_id() {
        let _iso = ninox_core::test_isolation::isolate();
        let mut m = fleet_test_app("o1", "w1");
        m.ninox_prompt_target = crate::prompt_agent::PromptTarget::Orchestrator { id: "o1".into(), name: "Orch".into() };
        let (m, _) = m.update(Message::NinoxPromptChanged("status?".into()));
        let (m, task) = m.update(Message::NinoxPromptSubmitted);
        assert_eq!(m.ninox_reply_drawer.turns[0].target_label, "Orch");

        match output_of(task).await {
            Message::NinoxPromptDelivered { generation, result } => {
                assert_eq!(generation, m.ninox_prompt_generation);
                assert!(result.is_err(), "no real session is running in this test");
            }
            other => panic!("expected NinoxPromptDelivered, got {other:?}"),
        }
    }

    fn submitted(m: App, text: &str) -> App {
        let (m, _) = m.update(Message::NinoxPromptChanged(text.into()));
        let (m, _) = m.update(Message::NinoxPromptSubmitted);
        m
    }

    fn transcript_poll(blocks: &[&str], ended: bool) -> crate::prompt_agent::PromptPoll {
        crate::prompt_agent::PromptPoll {
            answer: ninox_ui_shared::prompt_agent::reply::AnswerUpdate::Transcript(
                ninox_ui_shared::prompt_agent::TranscriptDelta {
                    text_blocks: blocks.iter().map(|b| b.to_string()).collect(),
                    ended: Some(ended),
                },
                Default::default(),
            ),
            context_pct: Some(18.0),
            events: vec![],
        }
    }

    /// Delivery → polled transcript answer (parsed to markdown in update)
    /// → stale results from a superseded generation are dropped.
    #[test]
    fn delivered_and_polled_answers_land_on_the_latest_turn_and_stale_ones_are_dropped() {
        let m = submitted(base(test_engine()), "hi");
        let generation = m.ninox_prompt_generation;
        let (m, _) = m.update(Message::NinoxPromptDelivered {
            generation,
            result: Ok(crate::prompt_agent::AnswerSource::Transcript(Default::default())),
        });
        let (m, _) = m.update(Message::NinoxPromptPolled { generation, poll: transcript_poll(&["**4**"], true) });
        let turn = &m.ninox_reply_drawer.turns[0];
        assert_eq!(turn.answer, "**4**");
        assert!(!turn.rendered.is_empty(), "the answer is parsed in update, not in view");
        assert_eq!(m.ninox_reply_drawer.context_pct, Some(18.0));

        let m = submitted(m, "again");
        let (m, _) = m.update(Message::NinoxPromptPolled { generation, poll: transcript_poll(&["late"], true) });
        assert_eq!(m.ninox_reply_drawer.turns[1].answer, "", "a stale poll must not land on the new turn");
        assert_eq!(m.ninox_reply_drawer.turns[0].answer, "**4**");
    }

    #[test]
    fn a_failed_delivery_shows_on_its_turn() {
        let m = submitted(base(test_engine()), "hi");
        let generation = m.ninox_prompt_generation;
        let (m, _) = m.update(Message::NinoxPromptDelivered { generation, result: Err("boom".into()) });
        assert_eq!(m.ninox_reply_drawer.turns[0].error.as_deref(), Some("boom"));
        assert_eq!(m.ninox_reply_drawer.turns[0].state, crate::prompt_agent::AnswerState::Failed);
    }

    #[test]
    fn answers_link_known_sessions() {
        let mut m = fleet_test_app("o1", "w1");
        m.sessions.get_mut("w1").unwrap().name = "bellhop-worker".into();
        let m = submitted(m, "who?");
        let generation = m.ninox_prompt_generation;
        let (m, _) = m.update(Message::NinoxPromptDelivered {
            generation,
            result: Ok(crate::prompt_agent::AnswerSource::Transcript(Default::default())),
        });
        let (m, _) = m.update(Message::NinoxPromptPolled {
            generation, poll: transcript_poll(&["It's bellhop-worker."], true),
        });
        let linked = crate::components::shell::reply_drawer::linkify(
            &m.ninox_reply_drawer.turns[0].answer,
            &crate::components::shell::reply_drawer::link_terms(&m).0,
            &[],
        );
        assert!(linked.contains("](https://ninox.invalid/session/w1)"), "{linked}");
    }

    #[test]
    fn select_toggle_swaps_in_a_read_only_copy_of_the_answer_and_back() {
        use iced::widget::text_editor::{Action, Edit};
        let m = submitted(fleet_test_app("o1", "w1"), "who?");
        let generation = m.ninox_prompt_generation;
        let (m, _) = m.update(Message::NinoxPromptDelivered {
            generation,
            result: Ok(crate::prompt_agent::AnswerSource::Transcript(Default::default())),
        });
        let (m, _) = m.update(Message::NinoxPromptPolled { generation, poll: transcript_poll(&["Run `x` now."], true) });
        let at = m.ninox_reply_drawer.turns[0].asked_at_ms;

        let (m, _) = m.update(Message::NinoxTurnSelectToggle(at));
        assert_eq!(m.ninox_turn_select[&at].text(), "Run `x` now.\n");
        let (m, _) = m.update(Message::NinoxTurnSelectAction(at, Action::Edit(Edit::Insert('!'))));
        assert_eq!(m.ninox_turn_select[&at].text(), "Run `x` now.\n", "edits are dropped");
        let (m, _) = m.update(Message::NinoxPromptPolled { generation, poll: transcript_poll(&["Run `x` now.", "More."], false) });
        assert!(m.ninox_turn_select[&at].text().contains("More."), "a growing answer refreshes the selectable copy");
        let (m, _) = m.update(Message::NinoxTurnSelectToggle(at));
        assert!(m.ninox_turn_select.is_empty(), "second toggle returns to the formatted view");
    }

    #[test]
    fn collapse_button_and_esc_fold_the_drawer_and_the_indicator_reopens_it() {
        let m = submitted(base(test_engine()), "hi");
        let (m, _) = m.update(Message::NinoxDrawerCollapse);
        assert!(!m.ninox_reply_drawer.is_open());
        let (m, _) = m.update(Message::NinoxDrawerReopen);
        assert!(m.ninox_reply_drawer.is_open());

        let (m, _) = m.update(Message::EscapeCaptured);
        assert!(!m.ninox_reply_drawer.is_open(), "Esc from the focused prompt collapses it");
        let (m, _) = m.update(Message::NinoxDrawerReopen);
        let (m, _) = m.update(Message::RawKey {
            key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape),
            modifiers: iced::keyboard::Modifiers::default(),
            text: None,
        });
        assert!(!m.ninox_reply_drawer.is_open(), "an uncaptured Esc collapses it too");
        assert_eq!(m.ninox_reply_drawer.turns.len(), 1, "collapsing keeps history");
    }

    #[test]
    fn asking_again_reopens_a_collapsed_drawer_with_history_intact() {
        let m = submitted(base(test_engine()), "one");
        let (m, _) = m.update(Message::NinoxDrawerCollapse);
        let m = submitted(m, "two");
        assert!(m.ninox_reply_drawer.is_open());
        assert_eq!(m.ninox_reply_drawer.turns.len(), 2);
    }

    #[test]
    fn esc_closes_the_search_before_collapsing_the_drawer() {
        let mut m = submitted(base(test_engine()), "hi");
        m.global_search.open = true;
        let (m, _) = m.update(Message::EscapeCaptured);
        assert!(!m.global_search.open);
        assert!(m.ninox_reply_drawer.is_open());
    }

    #[test]
    fn captured_esc_inside_a_modal_leaves_the_drawer_alone() {
        let mut m = submitted(base(test_engine()), "hi");
        m.restart_all_confirm = true;
        let (m, _) = m.update(Message::EscapeCaptured);
        assert!(m.ninox_reply_drawer.is_open());
    }

    #[test]
    fn clear_context_empties_the_drawer_on_success_and_keeps_it_on_error() {
        let m = submitted(base(test_engine()), "hi");
        let (m, _) = m.update(Message::NinoxPromptClear);
        let generation = m.ninox_prompt_generation;

        let (m, _) = m.update(Message::NinoxPromptCleared {
            generation, target_label: "ninox".into(), result: Err("nope".into()),
        });
        assert_eq!(m.ninox_reply_drawer.turns.len(), 1);
        assert_eq!(m.ninox_reply_drawer.notice, Some(crate::prompt_agent::Notice::Error("ninox: nope".into())));

        let (m, _) = m.update(Message::NinoxPromptCleared { generation, target_label: "ninox".into(), result: Ok(()) });
        assert!(m.ninox_reply_drawer.turns.is_empty());
        assert_eq!(m.ninox_reply_drawer.notice, Some(crate::prompt_agent::Notice::ContextCleared));
        assert!(m.ninox_reply_drawer.is_open());
    }

    #[test]
    fn esc_in_a_session_terminal_reaches_the_terminal_not_the_drawer() {
        let mut m = submitted(base(test_engine()), "hi");
        m.view = View::SessionDetail { session_id: "w1".into(), panel: DetailPanel::Terminal };
        m.terminal_focused = true;
        let (m, _) = m.update(Message::EscapeCaptured);
        assert!(m.ninox_reply_drawer.is_open(), "the terminal canvas's captured Esc is the agent's");
        let (m, _) = m.update(Message::RawKey {
            key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape),
            modifiers: iced::keyboard::Modifiers::default(),
            text: None,
        });
        assert!(m.ninox_reply_drawer.is_open());
    }

    #[test]
    fn captured_esc_in_the_fleet_composer_leaves_the_drawer_alone() {
        let mut m = submitted(base(test_engine()), "hi");
        m.fleet_message_draft = Some(("w1".into(), "draft".into()));
        let (m, _) = m.update(Message::EscapeCaptured);
        assert!(m.ninox_reply_drawer.is_open());
    }

    #[test]
    fn a_failed_clear_never_strands_an_in_flight_delivery() {
        let m = submitted(base(test_engine()), "hi");
        let generation = m.ninox_prompt_generation;
        let (m, _) = m.update(Message::NinoxPromptClear);
        let (m, _) = m.update(Message::NinoxPromptCleared { generation, target_label: "Orch".into(), result: Err("n/a".into()) });
        let (m, _) = m.update(Message::NinoxPromptDelivered {
            generation,
            result: Ok(crate::prompt_agent::AnswerSource::Transcript(Default::default())),
        });
        assert_eq!(
            m.ninox_reply_drawer.turns[0].source,
            crate::prompt_agent::AnswerSource::Transcript(Default::default()),
        );
    }

    /// ↑/↓ with the prompt focused recall earlier questions.
    #[test]
    fn history_recall_walks_previous_questions() {
        let m = submitted(submitted(base(test_engine()), "first"), "second");
        let prompt_id: iced::advanced::widget::Id = crate::components::shell::prompt_bar::prompt_input_id().into();
        let (m, _) = m.update(Message::NinoxPromptHistoryFocusChecked(true, prompt_id.clone()));
        assert_eq!(m.ninox_prompt_draft, "second");
        let (m, _) = m.update(Message::NinoxPromptHistoryFocusChecked(true, prompt_id.clone()));
        assert_eq!(m.ninox_prompt_draft, "first");
        let (m, _) = m.update(Message::NinoxPromptHistoryFocusChecked(false, prompt_id));
        assert_eq!(m.ninox_prompt_draft, "second");
    }

    fn open_search(m: App, query: &str) -> App {
        let (m, _) = m.update(Message::GlobalSearchFocus);
        let (m, _) = m.update(Message::GlobalSearchChanged(query.into()));
        assert!(m.global_search.open);
        m
    }

    fn assert_dismissed_keeping_query(m: &App, query: &str) {
        assert!(!m.global_search.open);
        assert_eq!(m.global_search.draft, query, "dismissal keeps the query for the next open");
    }

    #[test]
    fn search_closes_on_a_press_outside_it() {
        let m = open_search(base(test_engine()), "w1");
        let (m, _) = m.update(Message::GlobalSearchHover(crate::components::global_search::SearchPart::Field, true));
        let (m, _) = m.update(Message::MousePressed);
        assert!(m.global_search.open, "a press on the field keeps it open");
        let (m, _) = m.update(Message::GlobalSearchHover(crate::components::global_search::SearchPart::Field, false));
        let (m, _) = m.update(Message::GlobalSearchHover(crate::components::global_search::SearchPart::Dropdown, true));
        let (m, _) = m.update(Message::MousePressed);
        assert!(m.global_search.open, "a press on a result keeps it open so the result can fire");
        let (m, _) = m.update(Message::GlobalSearchHover(crate::components::global_search::SearchPart::Dropdown, false));
        let (m, _) = m.update(Message::MousePressed);
        assert_dismissed_keeping_query(&m, "w1");
    }

    #[test]
    fn search_closes_when_its_input_loses_focus() {
        let m = open_search(base(test_engine()), "w1");
        let (m, _) = m.update(Message::WindowUnfocused);
        assert_dismissed_keeping_query(&m, "w1");

        let m = open_search(m, "w1");
        let (m, _) = m.update(Message::FocusNinoxPrompt);
        assert_dismissed_keeping_query(&m, "w1");
    }

    #[test]
    fn search_closes_on_esc() {
        let m = open_search(base(test_engine()), "w1");
        let (m, _) = m.update(Message::EscapeCaptured);
        assert_dismissed_keeping_query(&m, "w1");
        let m = open_search(m, "w1");
        let (m, _) = m.update(Message::RawKey {
            key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape),
            modifiers: iced::keyboard::Modifiers::default(),
            text: None,
        });
        assert_dismissed_keeping_query(&m, "w1");
    }

    #[test]
    fn search_closes_on_every_tab_switch() {
        let _iso = ninox_core::test_isolation::isolate();
        let tabs = [
            Message::NavigateFleet,
            Message::NavigateBoard { scope: None },
            Message::NavigatePlans,
            Message::NavigateBrain,
            Message::NavigateWorkerTypes,
            Message::NavigateRemotes,
            Message::NavigateIdle,
            Message::NavigateSettings,
        ];
        let mut m = base(test_engine());
        for tab in tabs {
            m = open_search(m, "w1");
            let label = format!("{tab:?}");
            let (next, _) = m.update(tab);
            assert!(!next.global_search.open, "{label} must close the search");
            assert_eq!(next.global_search.draft, "w1");
            m = next;
        }
    }

    #[test]
    fn reopening_search_restores_the_kept_query() {
        let m = open_search(base(test_engine()), "w1");
        let (m, _) = m.update(Message::EscapeCaptured);
        let (m, _) = m.update(Message::GlobalSearchFocus);
        assert!(m.global_search.open);
        assert_eq!(m.global_search.draft, "w1");
    }

    /// Round-2 review: clearing context while a picked orchestrator (not
    /// the default prompt agent) is the target must never silently clear
    /// the unrelated prompt-agent singleton and claim success on the
    /// orchestrator's behalf.
    #[tokio::test]
    async fn clear_context_is_unavailable_for_a_picked_orchestrator() {
        let mut m = fleet_test_app("o1", "w1");
        m.ninox_prompt_target = crate::prompt_agent::PromptTarget::Orchestrator { id: "o1".into(), name: "Orch".into() };
        let (m, task) = m.update(Message::NinoxPromptClear);

        match output_of(task).await {
            Message::NinoxPromptCleared { generation, target_label, result } => {
                assert_eq!(generation, m.ninox_prompt_generation);
                assert_eq!(target_label, "Orch");
                assert!(result.is_err(), "no clear operation exists for a picked orchestrator");
            }
            other => panic!("expected NinoxPromptCleared, got {other:?}"),
        }
    }

    #[test]
    fn target_picker_resolves_ninox_to_the_prompt_agent_and_a_name_to_its_orchestrator() {
        let m = fleet_test_app("o1", "w1");
        let (m, _) = m.update(Message::NinoxTargetPicked("Orch".into()));
        assert_eq!(
            m.ninox_prompt_target,
            crate::prompt_agent::PromptTarget::Orchestrator { id: "o1".into(), name: "Orch".into() },
        );

        let (m, _) = m.update(Message::NinoxTargetPicked("ninox".into()));
        assert_eq!(m.ninox_prompt_target, crate::prompt_agent::PromptTarget::PromptAgent);
    }

    #[test]
    fn target_picker_falls_back_to_the_prompt_agent_for_an_unknown_name() {
        let m = base(test_engine());
        let (m, _) = m.update(Message::NinoxTargetPicked("does-not-exist".into()));
        assert_eq!(m.ninox_prompt_target, crate::prompt_agent::PromptTarget::PromptAgent);
    }

    /// Up/Down recall must only fire when the ninox prompt itself is the
    /// focused widget — simulated here by driving
    /// `NinoxPromptHistoryFocusChecked` directly, the message
    /// `find_focused`'s result maps into.
    #[test]
    fn history_recall_up_and_down_restore_the_in_progress_draft() {
        let mut m = base(test_engine());
        m.ninox_prompt_history = vec!["first".into(), "second".into()];
        m.ninox_prompt_draft = "typing a new one".into();

        let prompt_id: iced::advanced::widget::Id = crate::components::shell::prompt_bar::prompt_input_id().into();
        let (m, _) = m.update(Message::NinoxPromptHistoryFocusChecked(true, prompt_id.clone()));
        assert_eq!(m.ninox_prompt_draft, "second", "Up recalls the most recent entry first");

        let (m, _) = m.update(Message::NinoxPromptHistoryFocusChecked(true, prompt_id.clone()));
        assert_eq!(m.ninox_prompt_draft, "first", "a second Up goes further back");

        let (m, _) = m.update(Message::NinoxPromptHistoryFocusChecked(false, prompt_id.clone()));
        assert_eq!(m.ninox_prompt_draft, "second", "Down moves forward again");

        let (m, _) = m.update(Message::NinoxPromptHistoryFocusChecked(false, prompt_id));
        assert_eq!(m.ninox_prompt_draft, "typing a new one", "Down past the newest entry restores the in-progress draft");
        assert_eq!(m.ninox_prompt_history_cursor, None);
    }

    #[test]
    fn history_recall_is_ignored_when_a_different_widget_is_focused() {
        let mut m = base(test_engine());
        m.ninox_prompt_history = vec!["first".into()];
        m.ninox_prompt_draft = "untouched".into();

        let other_id = iced::advanced::widget::Id::new("some-other-field");
        let (m, _) = m.update(Message::NinoxPromptHistoryFocusChecked(true, other_id));
        assert_eq!(m.ninox_prompt_draft, "untouched");
        assert_eq!(m.ninox_prompt_history_cursor, None);
    }

    #[test]
    fn typing_exits_history_recall() {
        let mut m = base(test_engine());
        m.ninox_prompt_history = vec!["first".into()];
        let prompt_id: iced::advanced::widget::Id = crate::components::shell::prompt_bar::prompt_input_id().into();
        let (m, _) = m.update(Message::NinoxPromptHistoryFocusChecked(true, prompt_id));
        assert_eq!(m.ninox_prompt_history_cursor, Some(0));

        let (m, _) = m.update(Message::NinoxPromptChanged("something else".into()));
        assert_eq!(m.ninox_prompt_history_cursor, None);
    }

    /// G1: `/` focuses the ninox prompt from any view. Reaching `RawKey` at
    /// all already proves no other `text_input` is focused —
    /// `global_event_handler` drops anything `Status::Captured`, and a
    /// focused `text_input` captures every character key including `/`.
    #[test]
    fn slash_focuses_the_ninox_prompt() {
        let m = base(test_engine());
        let (_m, task) = m.update(Message::RawKey {
            key:       iced::keyboard::Key::Character("/".into()),
            modifiers: iced::keyboard::Modifiers::default(),
            text:      Some("/".to_string()),
        });
        assert!(
            iced_runtime::task::into_stream(task).is_some(),
            "/ must dispatch a focus command, not no-op"
        );
    }

    /// `Cmd+K` focuses the top-bar global search (G3), any view — it must
    /// never also focus the ninox prompt.
    #[test]
    fn cmd_k_focuses_global_search_and_opens_its_dropdown() {
        let m = base(test_engine());
        let (m, task) = m.update(Message::RawKey {
            key:       iced::keyboard::Key::Character("k".into()),
            modifiers: iced::keyboard::Modifiers::COMMAND,
            text:      Some("k".to_string()),
        });
        assert!(m.global_search.open, "Cmd+K must open the search dropdown immediately");
        assert!(
            iced_runtime::task::into_stream(task).is_some(),
            "Cmd+K must dispatch a focus command for the search field"
        );
    }

    #[test]
    fn global_search_changed_opens_the_dropdown_and_resets_the_cursor() {
        let mut m = base(test_engine());
        m.global_search.cursor = Some(2);
        let (m, _) = m.update(Message::GlobalSearchChanged("fleet".into()));
        assert_eq!(m.global_search.draft, "fleet");
        assert!(m.global_search.open);
        assert_eq!(m.global_search.cursor, None);
    }

    /// `Session` hits open the Fleet island (`FleetOpenNode`), and opening
    /// any hit closes the dropdown and clears the draft.
    #[test]
    fn global_search_open_hit_session_opens_its_fleet_island_and_closes_the_dropdown() {
        let mut m = fleet_test_app("o1", "w1");
        m.global_search.draft = "w1".into();
        m.global_search.open = true;

        let (m, _) = m.update(Message::GlobalSearchOpenHit(
            crate::components::global_search::GlobalSearchHit::Session { id: "w1".into(), label: "w1".into() },
        ));

        assert!(matches!(m.view, View::Fleet));
        assert_eq!(m.fleet_nav.selected.as_deref(), Some("w1"));
        assert!(!m.global_search.open);
        assert!(m.global_search.draft.is_empty());
    }

    /// Enter opens the first hit when nothing's been arrowed to yet —
    /// reads the cached `results` (populated by `GlobalSearchChanged`), not
    /// a fresh `run_search` call.
    #[test]
    fn global_search_enter_opens_the_first_hit_with_no_cursor() {
        let m = fleet_test_app("o1", "w1");
        let (m, _) = m.update(Message::GlobalSearchChanged("w1".into()));
        let (m, _) = m.update(Message::GlobalSearchEnterPressed);

        assert!(matches!(m.view, View::Fleet), "Enter should have opened the session's Fleet island");
        assert_eq!(m.fleet_nav.selected.as_deref(), Some("w1"));
    }

    /// The top bar's right edge (`+ Spawn`, the settings dot) and every
    /// label in it, laid out at `width`.
    fn nav_layout(m: &App, width: f32) -> (f32, f32, f32, f32) {
        use iced::advanced::layout::{Limits, Node};
        use iced::advanced::widget::Tree;
        use iced::{Font, Pixels, Size};
        fn tallest_leaf(node: &Node) -> f32 {
            if node.children().is_empty() {
                return node.size().height;
            }
            node.children().iter().map(tallest_leaf).fold(0.0, f32::max)
        }
        let bar = crate::components::shell::nav::nav(m);
        crate::style::load_bundled_fonts();
        let renderer = iced::Renderer::Secondary(iced_tiny_skia::Renderer::new(Font::default(), Pixels(16.0)));
        let mut tree = Tree::new(bar.as_widget());
        let node = bar.as_widget().layout(&mut tree, &renderer, &Limits::new(Size::ZERO, Size::new(width, f32::INFINITY)));
        // container → column → [the bar's row, its bottom rule].
        let column = &node.children()[0];
        let row = &column.children()[0];
        let children = row.children();
        let last = children.last().unwrap().bounds();
        // `+ Spawn` sits two before the settings dot (a 6px spacer between),
        // or four when the workspaces chip and its spacer sit between them;
        // one more when the server dot shows, and one for the bell.
        let chip = m.workspaces_summary.0 > 0;
        let dot = !crate::components::shell::nav::numbers_only(m);
        let spawn = children[children.len() - 1 - usize::from(dot) - if chip { 5 } else { 3 }].bounds();
        (column.bounds().x + row.bounds().x + last.x + last.width, tallest_leaf(row), spawn.width, last.width)
    }

    #[test]
    fn the_brand_clears_the_traffic_lights_by_measurement() {
        use crate::components::shell::nav::leading_inset;
        let mut m = base(test_engine());
        let unmeasured = leading_inset(&m);
        m.traffic_lights_end = Some(68.0);
        assert_eq!(leading_inset(&m), 84.0, "16px past where the zoom button ends");
        m.traffic_lights_end = Some(0.0);
        assert_eq!(leading_inset(&m), 16.0, "full screen hides the lights, so no room is kept for them");
        m.traffic_lights_end = None;
        assert_eq!(leading_inset(&m), unmeasured);

        let (m, _) = m.update(Message::TrafficLightsPlaced(Some(77.0)));
        assert_eq!(m.traffic_lights_end, Some(77.0 / m.zoom as f32), "window points become logical px");
    }

    /// At ~745px the search field pushed `+ Spawn` and the settings dot out
    /// of the window.
    #[test]
    fn the_top_bar_never_pushes_spawn_or_settings_out_of_the_window() {
        for (search_open, leftovers) in [(false, 0), (true, 0), (false, 3), (true, 3)] {
            for width in (480..=1400).step_by(20).map(|w| w as f32) {
                let mut m = base(test_engine());
                m.window_width = width;
                m.global_search.open = search_open;
                m.workspaces_summary = (leftovers, 2 * 1024 * 1024 * 1024);
                let (right, tallest, spawn_w, settings_w) = nav_layout(&m, width);
                assert!(right <= width + 0.5, "at {width}px (search open: {search_open}, leftovers: {leftovers}) the bar ends at {right}");
                // Squeezed to nothing is how they used to leave the window.
                assert!(spawn_w > 50.0, "at {width}px (search open: {search_open}, leftovers: {leftovers}) + Spawn is {spawn_w}px wide");
                assert!(settings_w > 20.0, "at {width}px (search open: {search_open}) the settings dot is {settings_w}px wide");
                assert!(tallest < 30.0, "at {width}px (search open: {search_open}) a label wrapped: {tallest}px");
            }
        }
    }

    /// On a narrow bar the field leaves the layout when search closes, so its
    /// mouse area never reports the pointer leaving; a stale `hover_field`
    /// would then swallow every click-outside dismissal.
    #[test]
    fn dismissing_search_forgets_the_hovered_field() {
        let mut m = base(test_engine());
        m.global_search.open = true;
        m.global_search.hover_field = true;
        m.global_search.dismiss();
        let (m, _) = m.update(Message::GlobalSearchFocus);
        let (m, _) = m.update(Message::MousePressed);
        assert!(!m.global_search.open, "a click outside must close search");
    }

    #[test]
    fn global_search_enter_with_no_hits_is_a_noop() {
        let m = base(test_engine());
        let (m, _) = m.update(Message::GlobalSearchChanged("nothing-matches-anything".into()));
        let (m, task) = m.update(Message::GlobalSearchEnterPressed);
        assert!(iced_runtime::task::into_stream(task).is_none());
        assert!(matches!(m.view, View::Fleet), "no hit to open — view must not change");
    }

    /// Review of PR #279: `⌘K` then clicking a nav tab (without picking a
    /// result) left the dropdown stacked on top of every subsequent view
    /// indefinitely — `text_input` swallows Escape internally, so there's
    /// no way to close it that way either. Every `Navigate*`/`FleetOpenNode`
    /// handler now closes it as its first action.
    #[test]
    fn navigating_anywhere_closes_the_search_dropdown() {
        let mut m = base(test_engine());
        m.global_search.open = true;
        let (m, _) = m.update(Message::NavigateBrain);
        assert!(!m.global_search.open, "navigating away must close a dropdown left open");
    }

    /// `Plan` hits navigate to the owning orchestrator's own Plan detail
    /// panel — `View::Plans` lists ghost plans, not plan docs.
    #[test]
    fn global_search_open_hit_plan_navigates_to_the_orchestrators_plan_panel() {
        let m = fleet_test_app("o1", "w1");
        let (m, _) = m.update(Message::GlobalSearchOpenHit(
            crate::components::global_search::GlobalSearchHit::Plan { orchestrator_id: "o1".into(), label: "Orch".into() },
        ));
        assert!(matches!(
            &m.view,
            View::SessionDetail { session_id, panel: DetailPanel::Plan } if session_id == "o1"
        ));
    }

    /// `Brain` hits switch to the Brain tab and select the entry.
    #[test]
    fn global_search_open_hit_brain_navigates_and_selects_the_entry() {
        let brain_dir = tempdir().unwrap().keep();
        std::fs::write(brain_dir.join("alice.md"), "# Alice\n").unwrap();
        let brain = Arc::new(BrainIndex::open(&brain_dir).unwrap());
        let e = test_engine();
        let m = base_with_brain(e, brain);

        let (m, _) = m.update(Message::GlobalSearchOpenHit(
            crate::components::global_search::GlobalSearchHit::Brain {
                id: "alice.md".into(), label: "Alice".into(), kind: "Notes".into(),
            },
        ));

        assert!(matches!(m.view, View::Brain));
        assert_eq!(m.brain_view.selected.as_deref(), Some("alice.md"));
    }

    #[test]
    fn clicking_a_brain_wikilink_selects_the_linked_entry() {
        use crate::components::internal_link::{encode, InternalLink};
        let brain_dir = tempdir().unwrap().keep();
        std::fs::create_dir_all(brain_dir.join("concepts")).unwrap();
        std::fs::write(brain_dir.join("concepts/frame alignment.md"), "---\nname: frame alignment\n---\nbody\n").unwrap();
        let brain = Arc::new(BrainIndex::open(&brain_dir).unwrap());
        brain.rebuild(None).unwrap();
        let mut m = base_with_brain(test_engine(), brain.clone());
        m.brain_view.entries = brain.query("", None, Default::default()).unwrap();

        let url = iced::widget::markdown::Url::parse(&encode(&InternalLink::Brain("frame alignment".into()))).unwrap();
        let (m, _) = m.update(Message::BrainLinkClicked(url));

        assert_eq!(m.brain_view.selected.as_deref(), Some("concepts/frame alignment.md"));
    }

    fn catalogue_at(name: &str, path: &std::path::Path) -> ninox_core::config::CatalogueRef {
        ninox_core::config::CatalogueRef {
            name: name.to_string(),
            path: path.to_path_buf(),
            remote: None,
            endpoint: None,
            region: None,
            cache_ttl_secs: None,
            enabled: true,
            cadence: "manual".to_string(),
            access: "read_write".to_string(),
        }
    }

    /// One indexed single catalogue holding `concepts/a.md`, active.
    fn app_with_one_entry() -> (App, std::path::PathBuf) {
        let dir = tempdir().unwrap().keep();
        std::fs::create_dir_all(dir.join("concepts")).unwrap();
        std::fs::write(dir.join("concepts/a.md"), "---\nname: A\ntype: concepts\nkeep: me\n---\n\nbody\n").unwrap();
        let brain = Arc::new(BrainIndex::open(&dir).unwrap());
        brain.rebuild(None).unwrap();
        let mut m = base_with_brain(test_engine(), brain);
        m.catalogues = vec![catalogue_at("default", &dir)];
        m.active_catalogue = Some(0);
        let (m, _) = m.update(Message::NavigateBrain);
        (m, dir)
    }

    #[tokio::test]
    async fn editing_an_entry_saves_it_reindexes_and_keeps_it_selected() {
        let _iso = ninox_core::test_isolation::isolate();
        let (m, dir) = app_with_one_entry();
        let (m, _) = m.update(Message::BrainSelectEntry("concepts/a.md".into()));
        let (m, _) = m.update(Message::BrainEditStart("concepts/a.md".into()));
        assert_eq!(m.brain_view.editor.as_ref().map(|e| e.name.as_str()), Some("A"));

        let (m, _) = m.update(Message::BrainEditName("Renamed".into()));
        let (m, _) = m.update(Message::BrainEditTagDraft("x, y".into()));
        let (m, _) = m.update(Message::BrainEditTagAdd);
        let (m, _) = m.update(Message::BrainEditTagRemove("x".into()));
        let (m, task) = m.update(Message::BrainEditSave);
        assert!(m.brain_view.job_running.is_some());
        let done = output_of(task).await;
        assert!(matches!(&done, Message::BrainJobDone { result: Ok(_), .. }), "{done:?}");
        let (m, _) = m.update(done);

        assert!(m.brain_view.editor.is_none());
        assert!(m.brain_view.job_running.is_none());
        assert_eq!(m.brain_view.selected.as_deref(), Some("concepts/a.md"));
        let e = m.brain_view.entries.iter().find(|e| e.id == "concepts/a.md").unwrap();
        assert_eq!((e.name.as_str(), e.tags.clone()), ("Renamed", vec!["y".to_string()]));
        let file = std::fs::read_to_string(dir.join("concepts/a.md")).unwrap();
        assert!(file.contains("keep: me"), "{file}");
        assert!(matches!(&m.brain_view.notice, Some(Ok(msg)) if msg.starts_with("saved concepts/a.md")));
    }

    #[tokio::test]
    async fn a_new_entry_is_written_and_selected() {
        let _iso = ninox_core::test_isolation::isolate();
        let (m, dir) = app_with_one_entry();
        let (m, _) = m.update(Message::BrainNewStart("patterns".into()));
        let (m, _) = m.update(Message::BrainEditName("Frame Alignment".into()));
        let (m, task) = m.update(Message::BrainEditSave);
        let (m, _) = m.update(output_of(task).await);

        assert!(dir.join("patterns/frame-alignment.md").is_file());
        assert_eq!(m.brain_view.selected.as_deref(), Some("patterns/frame-alignment.md"));
        assert!(m.brain_view.editor.is_none());
    }

    #[test]
    fn a_new_entry_is_refused_under_all_sources() {
        let _iso = ninox_core::test_isolation::isolate();
        let (mut m, _dir) = app_with_one_entry();
        m.active_catalogue = None;
        let (m, task) = m.update(Message::BrainNewStart("concepts".into()));
        assert!(m.brain_view.editor.is_none());
        assert!(matches!(&m.brain_view.notice, Some(Err(_))));
        assert!(iced_runtime::task::into_stream(task).is_none());
    }

    #[test]
    fn editing_under_all_sources_reads_the_entrys_own_catalogue() {
        let _iso = ninox_core::test_isolation::isolate();
        let (mut m, _dir) = app_with_one_entry();
        let other = tempdir().unwrap().keep();
        std::fs::create_dir_all(other.join("concepts")).unwrap();
        std::fs::write(other.join("concepts/a.md"), "---\nname: From beta\n---\n\nb\n").unwrap();
        m.catalogues.push(catalogue_at("beta", &other));
        m.active_catalogue = None;

        let (m, _) = m.update(Message::BrainEditStart("beta:concepts/a.md".into()));

        let ed = m.brain_view.editor.as_ref().expect("editor opened");
        assert_eq!(ed.name, "From beta");
    }

    #[test]
    fn repo_and_body_edits_reach_the_draft_and_cancel_drops_it() {
        let _iso = ninox_core::test_isolation::isolate();
        let (m, _dir) = app_with_one_entry();
        let (m, _) = m.update(Message::BrainEditStart("concepts/a.md".into()));
        let (m, _) = m.update(Message::BrainEditRepoDraft("ninox, bellhop".into()));
        let (m, _) = m.update(Message::BrainEditRepoAdd);
        let (m, _) = m.update(Message::BrainEditRepoRemove("bellhop".into()));
        let (m, _) = m.update(Message::BrainEditBody(iced::widget::text_editor::Action::SelectAll));
        let (m, _) = m.update(Message::BrainEditBody(iced::widget::text_editor::Action::Edit(
            iced::widget::text_editor::Edit::Paste(Arc::new("replaced".into())),
        )));
        let ed = m.brain_view.editor.as_ref().unwrap();
        assert_eq!((ed.repos.clone(), ed.repo_draft.as_str()), (vec!["ninox".to_string()], ""));
        assert_eq!(ed.body.text().trim_end(), "replaced");

        let (m, _) = m.update(Message::BrainEditCancel);
        assert!(m.brain_view.editor.is_none());
    }

    #[test]
    fn e_edits_the_selected_entry_and_esc_cancels() {
        let _iso = ninox_core::test_isolation::isolate();
        let (m, _dir) = app_with_one_entry();
        let (m, _) = m.update(Message::BrainSelectEntry("concepts/a.md".into()));
        let key = |k: iced::keyboard::Key, modifiers| Message::RawKey { key: k, modifiers, text: None };
        let (m, _) = m.update(key(iced::keyboard::Key::Character("e".into()), iced::keyboard::Modifiers::empty()));
        assert!(m.brain_view.editor.is_some(), "e opens the editor");
        let (m, _) = m.update(key(iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape), iced::keyboard::Modifiers::empty()));
        assert!(m.brain_view.editor.is_none(), "esc cancels");
    }

    #[tokio::test]
    async fn cmd_s_saves_the_open_editor() {
        let _iso = ninox_core::test_isolation::isolate();
        let (m, _dir) = app_with_one_entry();
        let (m, _) = m.update(Message::BrainEditStart("concepts/a.md".into()));
        let (m, _) = m.update(Message::BrainEditName("Via shortcut".into()));
        let (m, task) = m.update(Message::SaveShortcut);
        assert!(m.brain_view.job_running.is_some());
        let (m, _) = m.update(output_of(task).await);
        assert!(m.brain_view.entries.iter().any(|e| e.name == "Via shortcut"));
    }

    #[tokio::test]
    async fn a_new_entrys_kind_picks_its_folder() {
        let _iso = ninox_core::test_isolation::isolate();
        let (m, dir) = app_with_one_entry();
        let (m, _) = m.update(Message::BrainNewStart("patterns".into()));
        let (m, _) = m.update(Message::BrainEditKind("hazards".into()));
        let (m, _) = m.update(Message::BrainEditName("Hot path".into()));
        let (m, task) = m.update(Message::BrainEditSave);
        let (_m, _) = m.update(output_of(task).await);
        assert!(dir.join("hazards/hot-path.md").is_file());
    }

    #[tokio::test]
    async fn delete_asks_first_then_removes_the_file() {
        let _iso = ninox_core::test_isolation::isolate();
        let (m, dir) = app_with_one_entry();
        let (m, _) = m.update(Message::BrainSelectEntry("concepts/a.md".into()));
        let (m, _) = m.update(Message::BrainDeleteRequest("concepts/a.md".into()));
        let (m, _) = m.update(Message::BrainDeleteCancel);
        assert!(m.brain_view.delete_confirm.is_none() && dir.join("concepts/a.md").is_file());

        let (m, _) = m.update(Message::BrainDeleteRequest("concepts/a.md".into()));
        let (m, task) = m.update(Message::BrainDeleteConfirm);
        let (m, _) = m.update(output_of(task).await);
        assert!(!dir.join("concepts/a.md").exists());
        assert!(m.brain_view.selected.is_none());
        assert!(!m.brain_view.entries.iter().any(|e| e.id == "concepts/a.md"));
    }

    #[test]
    fn unresolved_wikilinks_are_marked_and_never_followed() {
        use crate::components::internal_link::{decode, InternalLink};
        let md = crate::components::brain_panel::preprocess_wikilinks_resolving("see [[a]] and [[nowhere]]", |t| t == "a");
        let urls: Vec<InternalLink> = iced::widget::markdown::parse(&md)
            .filter_map(|item| match item {
                iced::widget::markdown::Item::Paragraph(t) => Some(t),
                _ => None,
            })
            .flat_map(|t| t.spans(iced::widget::markdown::Style::from_palette(iced::Theme::Dark.palette())).to_vec())
            .filter_map(|span| span.link.as_ref().and_then(decode))
            .collect();
        assert_eq!(urls, [InternalLink::Brain("a".into()), InternalLink::Unresolved("nowhere".into())]);

        let _iso = ninox_core::test_isolation::isolate();
        let (m, _dir) = app_with_one_entry();
        let url = iced::widget::markdown::Url::parse(&crate::components::internal_link::encode(&InternalLink::Unresolved("nowhere".into()))).unwrap();
        let (m, task) = m.update(Message::BrainLinkClicked(url));
        assert!(m.brain_view.selected.is_none());
        assert!(iced_runtime::task::into_stream(task).is_none(), "nothing is opened");
    }

    #[test]
    fn switching_catalogue_or_mode_while_editing_drops_the_draft() {
        let _iso = ninox_core::test_isolation::isolate();
        let (mut m, _dir) = app_with_one_entry();
        let other = tempdir().unwrap().keep();
        m.catalogues.push(catalogue_at("personal", &other));
        let (m, _) = m.update(Message::BrainEditStart("concepts/a.md".into()));
        let (m, _) = m.update(Message::BrainSwitchCatalogue(Some(1)));
        assert!(m.brain_view.editor.is_none(), "a save can't be redirected to another catalogue");
        assert!(!other.join("concepts/a.md").exists());

        let (m, _) = m.update(Message::BrainSwitchCatalogue(Some(0)));
        let (m, _) = m.update(Message::BrainDeleteRequest("concepts/a.md".into()));
        let (m, _) = m.update(Message::BrainSetMode(BrainMode::Graph));
        assert!(m.brain_view.delete_confirm.is_none());
    }

    #[test]
    fn an_editor_is_pinned_to_the_catalogue_it_opened_in() {
        let _iso = ninox_core::test_isolation::isolate();
        let (m, dir) = app_with_one_entry();
        let (m, _) = m.update(Message::BrainEditStart("concepts/a.md".into()));
        assert_eq!(m.brain_view.editor.as_ref().map(|e| e.root.clone()), Some(dir));
    }

    #[test]
    fn a_job_only_closes_the_draft_that_started_it() {
        let _iso = ninox_core::test_isolation::isolate();
        let (m, _dir) = app_with_one_entry();
        let (m, _) = m.update(Message::BrainEditStart("concepts/a.md".into()));
        let stale = m.brain_view.editor.as_ref().unwrap().serial + 1000;
        let job = ninox_ui_shared::brain_edit::Job::Delete { id: "concepts/b.md".into() };
        let (m, _) = m.update(Message::BrainJobDone { job: job.clone(), select: None, editor: Some(stale), result: Ok("deleted".into()) });
        assert!(m.brain_view.editor.is_some(), "another job's success leaves this draft open");
        let (m, _) = m.update(Message::BrainJobDone { job, select: None, editor: None, result: Err("boom".into()) });
        assert!(m.brain_view.editor.as_ref().unwrap().error.is_none(), "and its failure isn't pinned on this draft");
        assert!(matches!(&m.brain_view.notice, Some(Err(e)) if e == "boom"));
    }

    #[test]
    fn cmd_s_under_a_modal_never_saves_the_brain_draft() {
        let _iso = ninox_core::test_isolation::isolate();
        let (m, _dir) = app_with_one_entry();
        let (m, _) = m.update(Message::BrainEditStart("concepts/a.md".into()));
        let (m, _) = m.update(Message::SpawnSession);
        let (m, _) = m.update(Message::SaveShortcut);
        assert!(m.brain_view.job_running.is_none());
    }

    #[test]
    fn selecting_another_entry_drops_a_pending_delete_confirm() {
        let _iso = ninox_core::test_isolation::isolate();
        let (m, _dir) = app_with_one_entry();
        let (m, _) = m.update(Message::BrainDeleteRequest("concepts/a.md".into()));
        let (m, _) = m.update(Message::BrainSelectEntry("concepts/other.md".into()));
        assert!(m.brain_view.delete_confirm.is_none());
    }

    #[tokio::test]
    async fn a_colon_in_an_id_is_kept_with_a_single_catalogue() {
        let _iso = ninox_core::test_isolation::isolate();
        let (m, dir) = app_with_one_entry();
        std::fs::create_dir_all(dir.join("notes")).unwrap();
        std::fs::write(dir.join("notes/a:b.md"), "---\nname: AB\n---\n\nx\n").unwrap();
        std::fs::write(dir.join("b.md"), "---\nname: B\n---\n\ny\n").unwrap();
        let (m, _) = m.update(Message::BrainDeleteRequest("notes/a:b.md".into()));
        let (m, task) = m.update(Message::BrainDeleteConfirm);
        let (_m, _) = m.update(output_of(task).await);
        assert!(!dir.join("notes/a:b.md").exists());
        assert!(dir.join("b.md").exists(), "the wrong file must never be deleted");
    }

    #[test]
    fn a_save_that_fails_validation_keeps_the_draft_and_shows_why() {
        let _iso = ninox_core::test_isolation::isolate();
        let (m, _dir) = app_with_one_entry();
        let (m, _) = m.update(Message::BrainEditStart("concepts/a.md".into()));
        let (m, _) = m.update(Message::BrainEditName(String::new()));
        let (m, task) = m.update(Message::BrainEditSave);
        assert!(iced_runtime::task::into_stream(task).is_none());
        let ed = m.brain_view.editor.as_ref().expect("draft kept");
        assert!(ed.error.as_deref().is_some_and(|e| e.contains("name")));
        assert!(m.brain_view.job_running.is_none());
    }

    #[test]
    fn a_failed_job_keeps_the_draft_with_its_error() {
        let _iso = ninox_core::test_isolation::isolate();
        let (m, _dir) = app_with_one_entry();
        let (m, _) = m.update(Message::BrainEditStart("concepts/a.md".into()));
        let job = ninox_ui_shared::brain_edit::Job::Save { id: "concepts/a.md".into(), content: String::new() };
        let serial = m.brain_view.editor.as_ref().map(|e| e.serial);
        let (m, _) = m.update(Message::BrainJobDone { job, select: None, editor: serial, result: Err("disk full".into()) });
        assert_eq!(m.brain_view.editor.as_ref().and_then(|e| e.error.as_deref()), Some("disk full"));
        assert!(m.brain_view.job_running.is_none());
    }

    #[tokio::test]
    async fn deleting_asks_first_then_removes_the_entry_and_clears_the_selection() {
        let _iso = ninox_core::test_isolation::isolate();
        let (m, dir) = app_with_one_entry();
        let (m, _) = m.update(Message::BrainSelectEntry("concepts/a.md".into()));
        let (m, _) = m.update(Message::BrainDeleteRequest("concepts/a.md".into()));
        assert_eq!(m.brain_view.delete_confirm.as_deref(), Some("concepts/a.md"));
        let (m, _) = m.update(Message::BrainDeleteCancel);
        assert!(m.brain_view.delete_confirm.is_none());
        assert!(dir.join("concepts/a.md").is_file());

        let (m, _) = m.update(Message::BrainDeleteRequest("concepts/a.md".into()));
        let (m, task) = m.update(Message::BrainDeleteConfirm);
        let (m, _) = m.update(output_of(task).await);

        assert!(!dir.join("concepts/a.md").exists());
        assert!(m.brain_view.delete_confirm.is_none());
        assert_eq!(m.brain_view.selected, None);
        assert!(m.brain_view.entries.is_empty());
    }

    #[test]
    fn global_search_cursor_is_ignored_when_a_different_widget_is_focused() {
        let mut m = base(test_engine());
        m.global_search.draft = "x".into();
        let other_id: iced::advanced::widget::Id = iced::advanced::widget::Id::new("some-other-field");
        let (m, _) = m.update(Message::GlobalSearchCursorFocusChecked(true, other_id));
        assert_eq!(m.global_search.cursor, None, "arrow keys must not move the cursor unless the search field is focused");
    }

    #[test]
    fn global_search_cursor_moves_within_bounds_when_the_field_is_focused() {
        let m = fleet_test_app("o1", "w1");
        let (m, _) = m.update(Message::GlobalSearchChanged("w1".into()));
        let id: iced::advanced::widget::Id = crate::components::global_search::search_input_id().into();

        let (m, _) = m.update(Message::GlobalSearchCursorFocusChecked(true, id.clone()));
        assert_eq!(m.global_search.cursor, Some(0), "only one hit — Up from None lands on it");

        let (m, _) = m.update(Message::GlobalSearchCursorFocusChecked(true, id.clone()));
        assert_eq!(m.global_search.cursor, Some(0), "Up at the top stays clamped, no wraparound");

        let (m, _) = m.update(Message::GlobalSearchCursorFocusChecked(false, id));
        assert_eq!(m.global_search.cursor, Some(0), "Down at the bottom stays clamped, no wraparound");
    }

    /// Review of PR #279: a stale cursor past the current result count (the
    /// live result set can shrink between two arrow presses with no retype
    /// in between) must clamp symmetrically in both directions, not just on
    /// the increment branch.
    #[test]
    fn global_search_cursor_clamps_a_stale_out_of_range_value_in_both_directions() {
        let m = fleet_test_app("o1", "w1");
        let (mut m, _) = m.update(Message::GlobalSearchChanged("w1".into()));
        m.global_search.cursor = Some(99); // stale — only one hit exists
        let id: iced::advanced::widget::Id = crate::components::global_search::search_input_id().into();

        let (m, _) = m.update(Message::GlobalSearchCursorFocusChecked(true, id));
        assert_eq!(m.global_search.cursor, Some(0), "an out-of-range cursor must clamp into range, not panic or stay stale");
    }

    /// `Cmd+F` focuses the Brain List search field, but only while the
    /// Brain tab is actually showing.
    #[test]
    fn cmd_f_focuses_brain_search_only_on_the_brain_tab() {
        let mut m = base(test_engine());
        m.view = View::Brain;
        let (_m, task) = m.update(Message::RawKey {
            key:       iced::keyboard::Key::Character("f".into()),
            modifiers: iced::keyboard::Modifiers::COMMAND,
            text:      Some("f".to_string()),
        });
        assert!(
            iced_runtime::task::into_stream(task).is_some(),
            "Cmd+F must dispatch a focus command on the Brain tab, not no-op"
        );

        let m = base(test_engine());
        assert!(matches!(m.view, View::Fleet));
        let (_m, task) = m.update(Message::RawKey {
            key:       iced::keyboard::Key::Character("f".into()),
            modifiers: iced::keyboard::Modifiers::COMMAND,
            text:      Some("f".to_string()),
        });
        assert!(
            iced_runtime::task::into_stream(task).is_none(),
            "Cmd+F must be a no-op off the Brain tab"
        );
    }

    /// G1 spec item 5: a layout test that the field's border and label
    /// render. The field, the `❯ Ask ninox` prefix and the `/`/`⏎` keycap
    /// chips are three layered `stack!` children — this guards that all
    /// three are still actually present (e.g. a future refactor collapsing
    /// the overlay back into a single widget would silently drop the
    /// label/border styling this item exists to add).
    #[test]
    fn prompt_bar_layers_the_field_prefix_and_chips() {
        use crate::components::shell::prompt_bar;
        use iced::advanced::layout::Limits;
        use iced::advanced::widget::Tree;
        use iced::{Font, Pixels, Size};

        let m = base(test_engine());
        let bar = prompt_bar::prompt_bar(&m, None);
        crate::style::load_bundled_fonts();
        let renderer = iced::Renderer::Secondary(iced_tiny_skia::Renderer::new(
            Font::default(),
            Pixels(16.0),
        ));
        let mut tree = Tree::new(bar.as_widget());
        let limits = Limits::new(Size::ZERO, Size::new(900.0, 100.0));
        let node = bar.as_widget().layout(&mut tree, &renderer, &limits);

        // `container(stack![field, prefix, chips])` — the outer container
        // has exactly one child (the stack), which in turn has the three
        // layers.
        let stack_children = node.children()[0].children();
        assert_eq!(
            stack_children.len(),
            3,
            "expected the field, the prefix label, and the keycap chips"
        );
    }

    #[test]
    fn navigate_notification_routes_through_the_session_attach_path() {
        // NavigateNotification used to set state.view directly and never
        // attach a client or create a TerminalState, permanently stranding
        // the panel at "Terminal connecting…". It must now delegate to the
        // same attach path as NavigateSession — beginning an attach attempt
        // is unique to that path, so seeing it here proves delegation happened.
        use crate::components::session_detail::DetailPanel;
        let e = test_engine();
        let mut m = base(e);
        m.sidebar.show_notifications = true;

        let (m2, _) = m.update(Message::NavigateNotification("s1".into()));

        assert!(!m2.sidebar.show_notifications);
        assert!(
            matches!(&m2.view, View::SessionDetail { session_id, panel: DetailPanel::Terminal }
                if session_id == "s1"),
            "must land on the session's Terminal panel"
        );
        assert!(
            m2.attach.contains_key("s1"),
            "must go through NavigateSession's attach path (which begins an attach attempt), \
             not set the view directly"
        );
    }

    #[test]
    fn navigate_brain_sets_view_and_loads_entries() {
        let brain_dir = tempdir().unwrap().keep();
        std::fs::create_dir_all(brain_dir.join("concepts")).unwrap();
        std::fs::write(
            brain_dir.join("concepts").join("note.md"),
            "---\nname: Note\n---\nSome body text.",
        )
        .unwrap();
        let brain = Arc::new(BrainIndex::open(&brain_dir).unwrap());
        brain.rebuild(None).unwrap();

        let e = test_engine();
        let m = base_with_brain(e, brain);
        assert!(!m.brain_view.loaded);

        let (m2, _) = m.update(Message::NavigateBrain);
        assert!(matches!(m2.view, View::Brain));
        assert!(m2.brain_view.loaded);
        assert_eq!(m2.brain_view.entries.len(), 1);
        assert_eq!(m2.brain_view.entries[0].name, "Note");
    }

    #[test]
    fn brain_select_entry_sets_selected() {
        let e = test_engine();
        let m = base(e);
        let (m2, _) = m.update(Message::BrainSelectEntry("concepts/note.md".into()));
        assert_eq!(m2.brain_view.selected.as_deref(), Some("concepts/note.md"));
    }

    #[test]
    fn brain_filter_query_sets_filter() {
        let e = test_engine();
        let m = base(e);
        let (m2, _) = m.update(Message::BrainFilterQuery("rust".into()));
        assert_eq!(m2.brain_view.filter, "rust");
    }

    /// Drive a `Task::future` to its single output `Message` — the async
    /// handlers (reindex, catalogue freshen) deliver their results through
    /// a follow-up message rather than mutating state directly.
    async fn output_of(task: Task<Message>) -> Message {
        use futures::StreamExt;
        let mut stream = iced_runtime::task::into_stream(task)
            .expect("expected a real Task, not Task::none()");
        match stream.next().await {
            Some(iced_runtime::Action::Output(msg)) => msg,
            _ => panic!("Task must yield exactly one Action::Output"),
        }
    }

    #[tokio::test]
    async fn brain_reindex_reloads_entries_from_disk() {
        let brain_dir = tempdir().unwrap().keep();
        let brain = Arc::new(BrainIndex::open(&brain_dir).unwrap());

        let e = test_engine();
        let m = base_with_brain(e, brain);
        let (m2, _) = m.update(Message::NavigateBrain);
        assert!(m2.brain_view.entries.is_empty());

        // A file appears on disk after the initial load (e.g. hand-edited outside the app).
        std::fs::create_dir_all(brain_dir.join("repos")).unwrap();
        std::fs::write(brain_dir.join("repos").join("ninox.md"), "Ninox repo notes.").unwrap();

        // Reindex runs sync + rebuild in a Task (no remote here, so the sync
        // step is a no-op) and reloads entries when `BrainReindexed` lands.
        let (m3, task) = m2.update(Message::BrainReindex);
        let done = output_of(task).await;
        assert!(
            matches!(&done, Message::BrainReindexed { result: Ok(1), .. }),
            "expected BrainReindexed with 1 indexed entry, got {done:?}"
        );
        let (m3, _) = m3.update(done);
        assert_eq!(m3.brain_view.entries.len(), 1);
        assert_eq!(m3.brain_view.entries[0].id, "repos/ninox.md");
    }

    #[test]
    fn stale_brain_reindexed_for_a_switched_away_catalogue_is_ignored() {
        // The rebuild belongs to whatever brain was active when BrainReindex
        // fired; if the user switched catalogues while it ran, its completion
        // must not reload the (unrelated) current catalogue's view.
        let e = test_engine();
        let m = base(e);
        let other = tempdir().unwrap().keep();
        let (m2, _) =
            m.update(Message::BrainReindexed { scope_id: BrainScopeId::Single(other), result: Ok(5) });
        assert!(
            !m2.brain_view.loaded,
            "a completion for a no-longer-active catalogue must be dropped, not applied"
        );
        assert!(m2.brain_view.entries.is_empty());
    }

    #[test]
    fn selecting_entry_opens_list_mode() {
        let brain_dir = tempdir().unwrap().keep();
        std::fs::create_dir_all(brain_dir.join("symbols")).unwrap();
        std::fs::write(brain_dir.join("symbols").join("x.md"), "---\nname: X\n---\nbody").unwrap();
        let brain = Arc::new(BrainIndex::open(&brain_dir).unwrap());
        brain.rebuild(None).unwrap();

        let e = test_engine();
        let app = base_with_brain(e, brain);
        let (app, _) = app.update(Message::NavigateBrain);
        let (app, _) = app.update(Message::BrainSetMode(BrainMode::Graph));
        let (app, _) = app.update(Message::BrainSelectEntry("symbols/x.md".into()));
        assert_eq!(app.brain_view.mode, BrainMode::List);
        assert_eq!(app.brain_view.selected.as_deref(), Some("symbols/x.md"));
    }

    /// Selecting an entry already visible under the active kind filter (or
    /// under "All entries", `kind_filter == None`) must not narrow it —
    /// otherwise reading any row while browsing "All entries" would
    /// silently yank the user into that row's own kind (code-review
    /// finding on an earlier version of this handler).
    #[test]
    fn selecting_a_visible_entry_does_not_change_the_kind_filter() {
        let brain_dir = tempdir().unwrap().keep();
        std::fs::create_dir_all(brain_dir.join("symbols")).unwrap();
        std::fs::write(brain_dir.join("symbols").join("x.md"), "---\nname: X\n---\nbody").unwrap();
        let brain = Arc::new(BrainIndex::open(&brain_dir).unwrap());
        brain.rebuild(None).unwrap();

        let e = test_engine();
        let app = base_with_brain(e, brain);
        let (app, _) = app.update(Message::NavigateBrain);
        assert_eq!(app.brain_view.kind_filter, None);
        let (app, _) = app.update(Message::BrainSelectEntry("symbols/x.md".into()));
        assert_eq!(
            app.brain_view.kind_filter, None,
            "selecting a row visible under \"All entries\" must not narrow the Kinds filter"
        );

        let (app, _) = app.update(Message::BrainSetKindFilter(Some("symbols".to_string())));
        let (app, _) = app.update(Message::BrainSelectEntry("symbols/x.md".into()));
        assert_eq!(
            app.brain_view.kind_filter.as_deref(), Some("symbols"),
            "selecting a row already matching the active kind filter must leave it unchanged"
        );
    }

    /// Selecting an entry that the active kind filter would otherwise hide
    /// (a wikilink, a backlink chip, or a future Graph double-click landing
    /// outside the current kind) retargets the filter so the selection is
    /// actually visible in the list.
    #[test]
    fn selecting_a_hidden_entry_retargets_the_kind_filter() {
        let brain_dir = tempdir().unwrap().keep();
        std::fs::create_dir_all(brain_dir.join("symbols")).unwrap();
        std::fs::write(brain_dir.join("symbols").join("x.md"), "---\nname: X\n---\nbody").unwrap();
        let brain = Arc::new(BrainIndex::open(&brain_dir).unwrap());
        brain.rebuild(None).unwrap();

        let e = test_engine();
        let mut app = base_with_brain(e, brain);
        app.brain_view.kind_filter = Some("errors".to_string());
        let (app, _) = app.update(Message::NavigateBrain);
        let (app, _) = app.update(Message::BrainSelectEntry("symbols/x.md".into()));
        assert_eq!(app.brain_view.kind_filter.as_deref(), Some("symbols"));
    }

    #[test]
    fn set_kind_repo_tag_filters_and_sort() {
        let e = test_engine();
        let m = base(e);
        let (m, _) = m.update(Message::BrainSetKindFilter(Some("decisions".to_string())));
        assert_eq!(m.brain_view.kind_filter.as_deref(), Some("decisions"));

        let (m, _) = m.update(Message::BrainSetRepoFilter(Some("ninox".to_string())));
        assert_eq!(m.brain_view.repo_filter.as_deref(), Some("ninox"));
        // Re-clicking the active repo clears it.
        let (m, _) = m.update(Message::BrainSetRepoFilter(Some("ninox".to_string())));
        assert_eq!(m.brain_view.repo_filter, None);

        let (m, _) = m.update(Message::BrainSetTagFilter(Some("lifecycle".to_string())));
        assert_eq!(m.brain_view.tag_filter.as_deref(), Some("lifecycle"));
        let (m, _) = m.update(Message::BrainSetTagFilter(Some("lifecycle".to_string())));
        assert_eq!(m.brain_view.tag_filter, None);

        let (m, _) = m.update(Message::BrainSetSort(BrainSort::Az));
        assert_eq!(m.brain_view.sort, BrainSort::Az);
    }

    /// Changing the Kinds-column selection resets the Repo/Tag chip
    /// filters — they're scoped to the active kind and re-derived on every
    /// render, so a value chosen under the old kind may not even be a
    /// valid chip under the new one.
    #[test]
    fn changing_kind_filter_resets_repo_and_tag_filters() {
        let e = test_engine();
        let m = base(e);
        let (m, _) = m.update(Message::BrainSetKindFilter(Some("decisions".to_string())));
        let (m, _) = m.update(Message::BrainSetRepoFilter(Some("ninox".to_string())));
        let (m, _) = m.update(Message::BrainSetTagFilter(Some("lifecycle".to_string())));
        assert_eq!(m.brain_view.repo_filter.as_deref(), Some("ninox"));
        assert_eq!(m.brain_view.tag_filter.as_deref(), Some("lifecycle"));

        let (m, _) = m.update(Message::BrainSetKindFilter(Some("errors".to_string())));
        assert_eq!(m.brain_view.kind_filter.as_deref(), Some("errors"));
        assert_eq!(m.brain_view.repo_filter, None);
        assert_eq!(m.brain_view.tag_filter, None);
    }

    #[test]
    fn switching_catalogue_resets_selection_and_active_index() {
        let dir_a = tempdir().unwrap().keep();
        std::fs::create_dir_all(dir_a.join("symbols")).unwrap();
        std::fs::write(dir_a.join("symbols").join("a.md"), "a body").unwrap();
        let dir_b = tempdir().unwrap().keep();
        std::fs::create_dir_all(dir_b.join("concepts")).unwrap();
        std::fs::write(dir_b.join("concepts").join("b.md"), "b body").unwrap();

        let brain_a = Arc::new(BrainIndex::open(&dir_a).unwrap());
        brain_a.rebuild(None).unwrap();
        // Seed dir_b's index too — `BrainSwitchCatalogue` opens a fresh
        // `BrainIndex` over the target path but does not rebuild it (matching
        // `NavigateBrain`'s lazy-load semantics), so the catalogue being
        // switched to must already be indexed on disk.
        BrainIndex::open(&dir_b).unwrap().rebuild(None).unwrap();

        let e = test_engine();
        let mut app = base_with_brain(e, brain_a);
        app.catalogues = vec![
            ninox_core::config::CatalogueRef { name: "default".into(), path: dir_a.clone(), remote: None, endpoint: None, region: None, cache_ttl_secs: None, enabled: true, cadence: "manual".to_string(), access: "read_write".to_string() },
            ninox_core::config::CatalogueRef { name: "second".into(), path: dir_b.clone(), remote: None, endpoint: None, region: None, cache_ttl_secs: None, enabled: true, cadence: "manual".to_string(), access: "read_write".to_string() },
        ];
        let (app, _) = app.update(Message::NavigateBrain);
        let (app, _) = app.update(Message::BrainSelectEntry("symbols/a.md".into()));
        assert_eq!(app.brain_view.selected.as_deref(), Some("symbols/a.md"));

        let (app, _) = app.update(Message::BrainSwitchCatalogue(Some(1)));
        assert_eq!(app.active_catalogue, Some(1));
        assert_eq!(app.brain_view.selected, None);
        assert!(app.brain_view.markdown.is_empty());
        assert_eq!(app.brain_view.kind_filter, None);
        assert_eq!(app.brain_view.repo_filter, None);
        assert_eq!(app.brain_view.tag_filter, None);
        assert_eq!(app.brain_view.entries.len(), 1);
        assert_eq!(app.brain_view.entries[0].id, "concepts/b.md");
    }

    #[tokio::test]
    async fn opening_the_brain_view_freshens_the_active_catalogue() {
        // Switch and reindex freshen, but the DEFAULT catalogue is never
        // switched to — without a freshen on NavigateBrain its first visit
        // shows whatever local snapshot the last CLI/server touch left.
        let dir = tempdir().unwrap().keep();
        std::fs::create_dir_all(dir.join("concepts")).unwrap();
        std::fs::write(dir.join("concepts").join("a.md"), "a body").unwrap();
        BrainIndex::open(&dir).unwrap().rebuild(None).unwrap();

        let e = test_engine();
        let app = base_with_brain(e, Arc::new(BrainIndex::open(&dir).unwrap()));

        let (app, task) = app.update(Message::NavigateBrain);
        assert!(matches!(app.view, View::Brain));
        let freshened = output_of(task).await;
        assert!(
            matches!(&freshened, Message::BrainFreshened { scope_id, .. } if scope_id == &BrainScopeId::Single(dir.clone())),
            "opening the Brain view must kick off a background freshen for the active catalogue, got {freshened:?}"
        );
        let (app, _) = app.update(freshened);
        assert!(app.brain_view.loaded);
        assert_eq!(app.brain_view.entries.len(), 1);
        assert_eq!(app.brain_view.entries[0].id, "concepts/a.md");
    }

    #[tokio::test]
    async fn switching_catalogue_freshens_in_the_background() {
        // The switch itself is synchronous (instant UI on the local copy),
        // but it must ALSO return a Task that re-opens the catalogue through
        // `open_synced` — the pull-before-read front door the CLI and server
        // already use. Without a remote configured, open_synced degrades to
        // a plain local open, so the whole flow is exercisable offline.
        let dir_a = tempdir().unwrap().keep();
        let dir_b = tempdir().unwrap().keep();
        std::fs::create_dir_all(dir_b.join("concepts")).unwrap();
        std::fs::write(dir_b.join("concepts").join("b.md"), "b body").unwrap();
        BrainIndex::open(&dir_b).unwrap().rebuild(None).unwrap();

        let e = test_engine();
        let mut app = base_with_brain(e, Arc::new(BrainIndex::open(&dir_a).unwrap()));
        app.catalogues = vec![
            ninox_core::config::CatalogueRef { name: "default".into(), path: dir_a.clone(), remote: None, endpoint: None, region: None, cache_ttl_secs: None, enabled: true, cadence: "manual".to_string(), access: "read_write".to_string() },
            ninox_core::config::CatalogueRef { name: "second".into(), path: dir_b.clone(), remote: None, endpoint: None, region: None, cache_ttl_secs: None, enabled: true, cadence: "manual".to_string(), access: "read_write".to_string() },
        ];

        let (app, task) = app.update(Message::BrainSwitchCatalogue(Some(1)));
        let freshened = output_of(task).await;
        assert!(
            matches!(&freshened, Message::BrainFreshened { scope_id, .. } if scope_id == &BrainScopeId::Single(dir_b.clone())),
            "switch must kick off a background freshen for the target catalogue, got {freshened:?}"
        );
        let (app, _) = app.update(freshened);
        assert_eq!(app.active_catalogue, Some(1));
        assert!(app.brain_view.loaded);
        assert_eq!(app.brain_view.entries.len(), 1);
        assert_eq!(app.brain_view.entries[0].id, "concepts/b.md");
    }

    #[test]
    fn switching_to_all_sources_merges_every_enabled_catalogue_and_stamps_source() {
        let dir_a = tempdir().unwrap().keep();
        std::fs::create_dir_all(dir_a.join("concepts")).unwrap();
        std::fs::write(dir_a.join("concepts").join("a.md"), "a body").unwrap();
        BrainIndex::open(&dir_a).unwrap().rebuild(None).unwrap();

        let dir_b = tempdir().unwrap().keep();
        std::fs::create_dir_all(dir_b.join("concepts")).unwrap();
        std::fs::write(dir_b.join("concepts").join("b.md"), "b body").unwrap();
        BrainIndex::open(&dir_b).unwrap().rebuild(None).unwrap();

        let e = test_engine();
        let mut app = base_with_brain(e, Arc::new(BrainIndex::open(&dir_a).unwrap()));
        app.catalogues = vec![
            ninox_core::config::CatalogueRef { name: "alpha".into(), path: dir_a, remote: None, endpoint: None, region: None, cache_ttl_secs: None, enabled: true, cadence: "manual".to_string(), access: "read_write".to_string() },
            ninox_core::config::CatalogueRef { name: "beta".into(), path: dir_b, remote: None, endpoint: None, region: None, cache_ttl_secs: None, enabled: true, cadence: "manual".to_string(), access: "read_write".to_string() },
        ];

        let (app, _) = app.update(Message::BrainSwitchCatalogue(None));
        assert_eq!(app.active_catalogue, None);
        assert!(app.brain.is_all());
        assert_eq!(app.brain_view.entries.len(), 2);
        let sources: std::collections::HashSet<&str> =
            app.brain_view.entries.iter().map(|e| e.source.as_str()).collect();
        assert_eq!(sources, std::collections::HashSet::from(["alpha", "beta"]));
        assert!(app.brain_view.entries.iter().all(|e| e.id.contains(':')), "ids are qualified in All-sources mode");
    }

    #[test]
    fn all_sources_excludes_disabled_catalogues() {
        let dir_a = tempdir().unwrap().keep();
        std::fs::create_dir_all(dir_a.join("concepts")).unwrap();
        std::fs::write(dir_a.join("concepts").join("a.md"), "a body").unwrap();
        BrainIndex::open(&dir_a).unwrap().rebuild(None).unwrap();

        let dir_b = tempdir().unwrap().keep();
        std::fs::create_dir_all(dir_b.join("concepts")).unwrap();
        std::fs::write(dir_b.join("concepts").join("b.md"), "b body").unwrap();
        BrainIndex::open(&dir_b).unwrap().rebuild(None).unwrap();

        let e = test_engine();
        let mut app = base_with_brain(e, Arc::new(BrainIndex::open(&dir_a).unwrap()));
        app.catalogues = vec![
            ninox_core::config::CatalogueRef { name: "alpha".into(), path: dir_a, remote: None, endpoint: None, region: None, cache_ttl_secs: None, enabled: true, cadence: "manual".to_string(), access: "read_write".to_string() },
            ninox_core::config::CatalogueRef { name: "beta".into(), path: dir_b, remote: None, endpoint: None, region: None, cache_ttl_secs: None, enabled: false, cadence: "manual".to_string(), access: "read_write".to_string() },
        ];

        let (app, _) = app.update(Message::BrainSwitchCatalogue(None));
        assert_eq!(app.brain_view.entries.len(), 1, "the disabled catalogue must never appear under All sources");
        assert_eq!(app.brain_view.entries[0].source, "alpha");
    }

    #[tokio::test]
    async fn all_sources_freshen_in_the_background_covers_every_enabled_catalogue() {
        let dir_a = tempdir().unwrap().keep();
        let dir_b = tempdir().unwrap().keep();
        std::fs::create_dir_all(dir_b.join("concepts")).unwrap();
        std::fs::write(dir_b.join("concepts").join("b.md"), "b body").unwrap();
        BrainIndex::open(&dir_b).unwrap().rebuild(None).unwrap();

        let e = test_engine();
        let mut app = base_with_brain(e, Arc::new(BrainIndex::open(&dir_a).unwrap()));
        app.catalogues = vec![
            ninox_core::config::CatalogueRef { name: "alpha".into(), path: dir_a, remote: None, endpoint: None, region: None, cache_ttl_secs: None, enabled: true, cadence: "manual".to_string(), access: "read_write".to_string() },
            ninox_core::config::CatalogueRef { name: "beta".into(), path: dir_b, remote: None, endpoint: None, region: None, cache_ttl_secs: None, enabled: true, cadence: "manual".to_string(), access: "read_write".to_string() },
        ];

        let (app, task) = app.update(Message::BrainSwitchCatalogue(None));
        let freshened = output_of(task).await;
        assert!(
            matches!(&freshened, Message::BrainFreshened { scope_id: BrainScopeId::All(paths), brain: BrainScope::All(m) } if paths.len() == 2 && m.sources().len() == 2),
            "the All-sources freshen must cover every enabled catalogue, got {freshened:?}"
        );
        let (app, _) = app.update(freshened);
        assert_eq!(app.brain_view.entries.len(), 1);
        assert_eq!(app.brain_view.entries[0].source, "beta");
    }

    #[test]
    fn a_stale_all_sources_freshen_for_a_different_catalogue_set_is_ignored() {
        // The race guard must distinguish "All sources over {alpha}" from
        // "All sources over {alpha, beta}" -- a slow freshen kicked off
        // before `beta` was enabled must not clobber the current, larger
        // merge just because both are generically "All sources" (the bug:
        // BrainScopeId::All used to be a bare unit variant, so any two All-
        // sources freshens compared equal regardless of which catalogues
        // they actually covered).
        let dir_a = tempdir().unwrap().keep();
        std::fs::create_dir_all(dir_a.join("concepts")).unwrap();
        std::fs::write(dir_a.join("concepts").join("a.md"), "a body").unwrap();
        BrainIndex::open(&dir_a).unwrap().rebuild(None).unwrap();

        let dir_b = tempdir().unwrap().keep();
        std::fs::create_dir_all(dir_b.join("concepts")).unwrap();
        std::fs::write(dir_b.join("concepts").join("b.md"), "b body").unwrap();
        BrainIndex::open(&dir_b).unwrap().rebuild(None).unwrap();

        let e = test_engine();
        let mut app = base_with_brain(e, Arc::new(BrainIndex::open(&dir_a).unwrap()));
        app.catalogues = vec![
            ninox_core::config::CatalogueRef { name: "alpha".into(), path: dir_a.clone(), remote: None, endpoint: None, region: None, cache_ttl_secs: None, enabled: true, cadence: "manual".to_string(), access: "read_write".to_string() },
            ninox_core::config::CatalogueRef { name: "beta".into(), path: dir_b, remote: None, endpoint: None, region: None, cache_ttl_secs: None, enabled: true, cadence: "manual".to_string(), access: "read_write".to_string() },
        ];
        let (app, _) = app.update(Message::BrainSwitchCatalogue(None));
        assert_eq!(app.brain_view.entries.len(), 2, "both catalogues are in scope");

        // A freshen that only covers `alpha` (e.g. kicked off before `beta`
        // was added) lands late.
        let stale_merge = MergedBrainIndex::from_sources(vec![ninox_core::MergedSource {
            name: "alpha".into(),
            index: Arc::new(BrainIndex::open(&dir_a).unwrap()),
        }]);
        let stale_scope_id = BrainScope::All(Arc::new(stale_merge)).identity();
        let (app, _) = app.update(Message::BrainFreshened {
            scope_id: stale_scope_id,
            brain: BrainScope::All(Arc::new(MergedBrainIndex::from_sources(vec![]))),
        });

        assert_eq!(
            app.brain_view.entries.len(),
            2,
            "a freshen for a different (smaller) catalogue set must not replace the current merge"
        );
    }

    #[test]
    fn sources_synced_in_all_mode_rebuilds_only_the_catalogue_that_synced() {
        // SourcesSynced must rebuild just the one catalogue whose sync
        // completed, not every source in "All sources" mode — otherwise a
        // file freshly written to disk under `beta` (not yet reindexed)
        // would leak into view the moment `alpha`'s unrelated sync lands,
        // and SourcesSyncAll fanning out one completion per catalogue would
        // turn N syncs into N full rebuilds each (O(N^2), found in review).
        let dir_a = tempdir().unwrap().keep();
        std::fs::create_dir_all(dir_a.join("concepts")).unwrap();
        std::fs::write(dir_a.join("concepts").join("a.md"), "a body").unwrap();
        BrainIndex::open(&dir_a).unwrap().rebuild(None).unwrap();

        let dir_b = tempdir().unwrap().keep();
        std::fs::create_dir_all(dir_b.join("concepts")).unwrap();
        std::fs::write(dir_b.join("concepts").join("b.md"), "b body").unwrap();
        BrainIndex::open(&dir_b).unwrap().rebuild(None).unwrap();

        let e = test_engine();
        let mut app = base_with_brain(e, Arc::new(BrainIndex::open(&dir_a).unwrap()));
        app.catalogues = vec![
            ninox_core::config::CatalogueRef { name: "alpha".into(), path: dir_a.clone(), remote: None, endpoint: None, region: None, cache_ttl_secs: None, enabled: true, cadence: "manual".to_string(), access: "read_write".to_string() },
            ninox_core::config::CatalogueRef { name: "beta".into(), path: dir_b.clone(), remote: None, endpoint: None, region: None, cache_ttl_secs: None, enabled: true, cadence: "manual".to_string(), access: "read_write".to_string() },
        ];
        let (app, _) = app.update(Message::BrainSwitchCatalogue(None));
        assert_eq!(app.brain_view.entries.len(), 2);

        // A new file lands under `beta` on disk, but `beta`'s own
        // `.index.db` hasn't been rebuilt to pick it up yet.
        std::fs::write(dir_b.join("concepts").join("new-from-disk.md"), "new").unwrap();

        let ok = |pulled| {
            Ok(ninox_core::brain_sync::SyncReport {
                checked: true,
                pulled,
                pushed: 0,
                deleted_local: 0,
                deleted_remote: 0,
                conflicts: vec![],
            })
        };

        let (app, _) =
            app.update(Message::SourcesSynced { name: "alpha".into(), path: dir_a, result: ok(0) });
        assert_eq!(
            app.brain_view.entries.len(),
            2,
            "alpha's sync completing must not pull in beta's unrelated on-disk change"
        );

        let (app, _) =
            app.update(Message::SourcesSynced { name: "beta".into(), path: dir_b, result: ok(1) });
        assert_eq!(app.brain_view.entries.len(), 3, "beta's own sync completing does rebuild beta");
    }

    #[test]
    fn brain_freshened_for_the_active_catalogue_swaps_the_index_and_reloads() {
        let dir = tempdir().unwrap().keep();
        let e = test_engine();
        let mut app = base_with_brain(e, Arc::new(BrainIndex::open(&dir).unwrap()));
        app.catalogues[0].path = dir.clone();
        let (app, _) = app.update(Message::NavigateBrain);
        assert!(app.brain_view.entries.is_empty());

        // The background pull landed new entries and rebuilt the index —
        // simulate its delivery with a fresh index over the same directory.
        std::fs::create_dir_all(dir.join("repos")).unwrap();
        std::fs::write(dir.join("repos").join("pulled.md"), "pulled from remote").unwrap();
        let fresh = Arc::new(BrainIndex::open(&dir).unwrap());
        fresh.rebuild(None).unwrap();

        let (app, _) = app.update(Message::BrainFreshened {
            scope_id: BrainScopeId::Single(dir),
            brain: BrainScope::Single(fresh.clone()),
        });
        let BrainScope::Single(active) = &app.brain else { panic!("expected a single-catalogue scope") };
        assert!(Arc::ptr_eq(active, &fresh), "the freshened index must replace the active one");
        assert_eq!(app.brain_view.entries.len(), 1);
        assert_eq!(app.brain_view.entries[0].id, "repos/pulled.md");
    }

    #[test]
    fn stale_brain_freshened_for_a_switched_away_catalogue_is_ignored() {
        // The user switched again while the pull was in flight — the late
        // result belongs to a catalogue that's no longer on screen and must
        // not clobber the one that is.
        let dir_a = tempdir().unwrap().keep();
        let dir_b = tempdir().unwrap().keep();
        std::fs::create_dir_all(dir_b.join("concepts")).unwrap();
        std::fs::write(dir_b.join("concepts").join("b.md"), "b body").unwrap();
        let stale = Arc::new(BrainIndex::open(&dir_b).unwrap());
        stale.rebuild(None).unwrap();

        let e = test_engine();
        let active = Arc::new(BrainIndex::open(&dir_a).unwrap());
        let mut app = base_with_brain(e, active.clone());
        app.catalogues[0].path = dir_a.clone();
        let (app, _) = app.update(Message::NavigateBrain);

        let (app, _) = app.update(Message::BrainFreshened {
            scope_id: BrainScopeId::Single(dir_b),
            brain: BrainScope::Single(stale),
        });
        let BrainScope::Single(still_active) = &app.brain else { panic!("expected a single-catalogue scope") };
        assert!(Arc::ptr_eq(still_active, &active), "a stale freshen must not replace the active index");
        assert!(app.brain_view.entries.is_empty(), "dir_b's entries must not leak into dir_a's view");
    }

    #[test]
    fn navigate_brain_populates_pinboard_edges_from_the_index() {
        let brain_dir = tempdir().unwrap().keep();
        std::fs::create_dir_all(brain_dir.join("people")).unwrap();
        std::fs::write(
            brain_dir.join("people").join("alice.md"),
            "---\nname: Alice\n---\nManages [[bob]].",
        )
        .unwrap();
        std::fs::write(
            brain_dir.join("people").join("bob.md"),
            "---\nname: Bob\n---\nReports to [[alice]].",
        )
        .unwrap();
        let brain = Arc::new(BrainIndex::open(&brain_dir).unwrap());
        brain.rebuild(None).unwrap();

        let e = test_engine();
        let m = base_with_brain(e, brain);
        assert!(m.brain_view.edges.is_empty());

        let (m2, _) = m.update(Message::NavigateBrain);
        // Alice <-> Bob's mutual link resolves to a single undirected
        // node-index edge, computed once from `links_all()` rather than
        // re-parsed per pinboard draw.
        assert_eq!(m2.brain_view.edges.len(), 1);
        let (a, b) = m2.brain_view.edges[0];
        let ids: Vec<&str> =
            [a, b].iter().map(|&i| m2.brain_view.entries[i].id.as_str()).collect();
        assert!(ids.contains(&"people/alice.md"));
        assert!(ids.contains(&"people/bob.md"));
    }

    #[test]
    fn selecting_entry_populates_backlinks_and_related_from_the_index() {
        let brain_dir = tempdir().unwrap().keep();
        std::fs::create_dir_all(brain_dir.join("people")).unwrap();
        std::fs::write(
            brain_dir.join("people").join("alice.md"),
            "---\nname: Alice\n---\nManages [[bob]].",
        )
        .unwrap();
        std::fs::write(brain_dir.join("people").join("bob.md"), "---\nname: Bob\n---\nbody").unwrap();
        let brain = Arc::new(BrainIndex::open(&brain_dir).unwrap());
        brain.rebuild(None).unwrap();

        let e = test_engine();
        let m = base_with_brain(e, brain);
        let (m2, _) = m.update(Message::NavigateBrain);
        assert!(m2.brain_view.backlinks.is_empty());
        assert!(m2.brain_view.related.is_empty());

        let (m3, _) = m2.update(Message::BrainSelectEntry("people/bob.md".into()));
        assert_eq!(m3.brain_view.backlinks.len(), 1);
        assert_eq!(m3.brain_view.backlinks[0].id, "people/alice.md");
        assert!(
            m3.brain_view.related.iter().any(|e| e.id == "people/alice.md"),
            "alice directly links to bob, so alice should rank in bob's related list: {:?}",
            m3.brain_view.related.iter().map(|e| &e.id).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn reindex_rerenders_the_reading_pane_for_the_selected_entry() {
        // The reading pane parsed markdown at selection time and kept it
        // through entry-set reloads — fine when only the graph could change
        // underneath a selection, wrong once background pulls can change
        // the selected entry's body itself.
        let brain_dir = tempdir().unwrap().keep();
        std::fs::create_dir_all(brain_dir.join("concepts")).unwrap();
        std::fs::write(
            brain_dir.join("concepts").join("a.md"),
            "---\nname: a\n---\none paragraph",
        )
        .unwrap();
        let brain = Arc::new(BrainIndex::open(&brain_dir).unwrap());
        brain.rebuild(None).unwrap();

        let e = test_engine();
        let m = base_with_brain(e, brain);
        let (m2, _) = m.update(Message::NavigateBrain);
        let (m3, _) = m2.update(Message::BrainSelectEntry("concepts/a.md".into()));
        let blocks_before = m3.brain_view.markdown.len();
        assert!(blocks_before > 0, "sanity: selection rendered something");

        // The entry's body changes on disk (e.g. a teammate's pull landing)
        // while it stays selected.
        std::fs::write(
            brain_dir.join("concepts").join("a.md"),
            "---\nname: a\n---\nfirst paragraph\n\nsecond paragraph\n\nthird paragraph",
        )
        .unwrap();

        let (m4, task) = m3.update(Message::BrainReindex);
        let (m4, _) = m4.update(output_of(task).await);
        assert_eq!(m4.brain_view.selected.as_deref(), Some("concepts/a.md"));
        assert!(
            m4.brain_view.markdown.len() > blocks_before,
            "the reading pane must re-render from the re-read body ({} blocks) instead of \
             keeping the selection-time parse ({} blocks)",
            m4.brain_view.markdown.len(), blocks_before,
        );
    }

    #[tokio::test]
    async fn reindex_refreshes_backlinks_and_related_for_the_selected_entry() {
        // BrainReindex used to refetch entries + edges but leave the reading
        // pane's backlinks/related as stale snapshots from selection time --
        // if the on-disk citation graph changed underneath the selection,
        // the chips kept pointing at a graph that no longer existed.
        let brain_dir = tempdir().unwrap().keep();
        std::fs::create_dir_all(brain_dir.join("people")).unwrap();
        std::fs::write(
            brain_dir.join("people").join("alice.md"),
            "---\nname: Alice\n---\nManages [[bob]].",
        )
        .unwrap();
        std::fs::write(
            brain_dir.join("people").join("bob.md"),
            "---\nname: Bob\n---\nReports to [[alice]].",
        )
        .unwrap();
        let brain = Arc::new(BrainIndex::open(&brain_dir).unwrap());
        brain.rebuild(None).unwrap();

        let e = test_engine();
        let m = base_with_brain(e, brain);
        let (m2, _) = m.update(Message::NavigateBrain);
        let (m3, _) = m2.update(Message::BrainSelectEntry("people/alice.md".into()));
        assert_eq!(
            m3.brain_view.backlinks.len(),
            1,
            "bob cites alice, so alice's backlinks should start populated"
        );
        assert!(m3.brain_view.related.iter().any(|e| e.id == "people/bob.md"));

        // Bob (the only entry citing/cited-by alice) is deleted on disk
        // (e.g. removed outside the app) -- alice stays selected, but her
        // backlinks/related must reflect the new, bob-less graph after
        // reindex rather than the stale selection-time snapshot.
        std::fs::remove_file(brain_dir.join("people").join("bob.md")).unwrap();

        let (m4, task) = m3.update(Message::BrainReindex);
        let (m4, _) = m4.update(output_of(task).await);
        assert_eq!(m4.brain_view.selected.as_deref(), Some("people/alice.md"));
        assert!(
            m4.brain_view.backlinks.is_empty(),
            "backlinks must be refreshed after reindex, not left stale: {:?}",
            m4.brain_view.backlinks.iter().map(|e| &e.id).collect::<Vec<_>>()
        );
        assert!(
            m4.brain_view.related.is_empty(),
            "bob is gone, so alice's related list must be refreshed to drop it, not left stale: {:?}",
            m4.brain_view.related.iter().map(|e| &e.id).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn reindex_clears_selection_if_the_selected_entry_disappears() {
        // If the selected entry's file is deleted (or renamed) before a
        // reindex, `selected` must be cleared so the reading pane falls back
        // to its empty state instead of showing a ghost entry with stale
        // markdown/backlinks/related from before the deletion.
        let brain_dir = tempdir().unwrap().keep();
        std::fs::create_dir_all(brain_dir.join("people")).unwrap();
        std::fs::write(
            brain_dir.join("people").join("alice.md"),
            "---\nname: Alice\n---\nManages [[bob]].",
        )
        .unwrap();
        std::fs::write(
            brain_dir.join("people").join("bob.md"),
            "---\nname: Bob\n---\nReports to [[alice]].",
        )
        .unwrap();
        let brain = Arc::new(BrainIndex::open(&brain_dir).unwrap());
        brain.rebuild(None).unwrap();

        let e = test_engine();
        let m = base_with_brain(e, brain);
        let (m2, _) = m.update(Message::NavigateBrain);
        let (m3, _) = m2.update(Message::BrainSelectEntry("people/alice.md".into()));
        assert_eq!(m3.brain_view.selected.as_deref(), Some("people/alice.md"));
        assert!(!m3.brain_view.markdown.is_empty());
        assert!(!m3.brain_view.backlinks.is_empty());

        std::fs::remove_file(brain_dir.join("people").join("alice.md")).unwrap();

        let (m4, task) = m3.update(Message::BrainReindex);
        let (m4, _) = m4.update(output_of(task).await);
        assert_eq!(
            m4.brain_view.selected, None,
            "selection must be cleared once the selected entry no longer resolves"
        );
        assert!(m4.brain_view.markdown.is_empty());
        assert!(m4.brain_view.backlinks.is_empty());
        assert!(m4.brain_view.related.is_empty());
    }

    #[test]
    fn switching_catalogue_clears_and_repopulates_pinboard_edges() {
        let dir_a = tempdir().unwrap().keep();
        std::fs::create_dir_all(dir_a.join("people")).unwrap();
        std::fs::write(dir_a.join("people").join("alice.md"), "Sees [[bob]].").unwrap();
        std::fs::write(dir_a.join("people").join("bob.md"), "Sees [[alice]].").unwrap();

        let dir_b = tempdir().unwrap().keep();
        std::fs::create_dir_all(dir_b.join("people")).unwrap();
        std::fs::write(dir_b.join("people").join("carol.md"), "No links here.").unwrap();

        let brain_a = Arc::new(BrainIndex::open(&dir_a).unwrap());
        brain_a.rebuild(None).unwrap();
        BrainIndex::open(&dir_b).unwrap().rebuild(None).unwrap();

        let e = test_engine();
        let mut app = base_with_brain(e, brain_a);
        app.catalogues = vec![
            ninox_core::config::CatalogueRef { name: "default".into(), path: dir_a.clone(), remote: None, endpoint: None, region: None, cache_ttl_secs: None, enabled: true, cadence: "manual".to_string(), access: "read_write".to_string() },
            ninox_core::config::CatalogueRef { name: "second".into(), path: dir_b.clone(), remote: None, endpoint: None, region: None, cache_ttl_secs: None, enabled: true, cadence: "manual".to_string(), access: "read_write".to_string() },
        ];
        let (app, _) = app.update(Message::NavigateBrain);
        assert_eq!(
            app.brain_view.edges.len(),
            1,
            "alice<->bob's mutual link should resolve to one edge in catalogue A"
        );

        let (app, _) = app.update(Message::BrainSwitchCatalogue(Some(1)));
        assert!(
            app.brain_view.edges.is_empty(),
            "catalogue B has no links -- edges must be cleared, not left over from A"
        );
    }

    #[test]
    fn toggle_notifications_flips_show_flag() {
        let e = test_engine();
        let m = base(e);
        assert!(!m.sidebar.show_notifications);
        let (m2, _) = m.update(Message::ToggleNotifications);
        assert!(m2.sidebar.show_notifications);
        let (m3, _) = m2.update(Message::ToggleNotifications);
        assert!(!m3.sidebar.show_notifications);
    }

    fn notif(i: usize) -> Notification {
        Notification {
            id: format!("n{i}"),
            kind: NotificationKind::LaunchFailed,
            title: "t".into(),
            body: "b".into(),
            session_id: None,
            created_at: 0,
        }
    }

    #[test]
    fn escape_closes_the_notifications_panel() {
        let mut m = base(test_engine());
        m.sidebar.show_notifications = true;
        let esc = Message::RawKey {
            key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape),
            modifiers: iced::keyboard::Modifiers::empty(),
            text: None,
        };
        let (m, _) = m.update(esc);
        assert!(!m.sidebar.show_notifications);
    }

    #[test]
    fn notifications_and_pending_requests_dropdowns_are_exclusive() {
        let m = base(test_engine());
        let (m, _) = m.update(Message::TogglePendingRequests);
        let (m, _) = m.update(Message::ToggleNotifications);
        assert!(m.sidebar.show_notifications && !m.sidebar.show_pending_requests);
        let (m, _) = m.update(Message::TogglePendingRequests);
        assert!(m.sidebar.show_pending_requests && !m.sidebar.show_notifications);
    }

    #[test]
    fn the_top_bar_with_unread_notifications_stays_inside_the_window() {
        for width in (480..=1400).step_by(20).map(|w| w as f32) {
            let mut m = base(test_engine());
            m.window_width = width;
            m.notifications = (0..120).map(notif).collect();
            let (right, _, spawn_w, settings_w) = nav_layout(&m, width);
            assert!(right <= width + 0.5, "at {width}px the bar ends at {right}");
            assert!(spawn_w > 50.0 && settings_w > 20.0, "at {width}px spawn {spawn_w} settings {settings_w}");
        }
    }

    #[test]
    fn toggle_pending_requests_flips_show_flag() {
        let e = test_engine();
        let m = base(e);
        assert!(!m.sidebar.show_pending_requests);
        let (m2, _) = m.update(Message::TogglePendingRequests);
        assert!(m2.sidebar.show_pending_requests);
        let (m3, _) = m2.update(Message::TogglePendingRequests);
        assert!(!m3.sidebar.show_pending_requests);
    }

    #[test]
    fn dismiss_notification_removes_by_id() {
        let _iso = ninox_core::test_isolation::isolate();
        let e = test_engine();
        let mut m = base(e);
        for id in ["n1", "n2", "n3"] {
            let n = Notification {
                id: id.into(), kind: NotificationKind::WorkerDone,
                title: "t".into(), body: "b".into(), session_id: None,
                created_at: 0,
            };
            m.engine.store.insert_notification(&n).unwrap();
            let (next, _) = m.update(Message::EngineEvent(Box::new(Event::Notification(n))));
            m = next;
        }
        assert_eq!(m.notifications.len(), 3);
        let (m2, _) = m.update(Message::DismissNotification("n2".into()));
        assert_eq!(m2.notifications.len(), 2);
        assert!(!m2.notifications.iter().any(|n| n.id == "n2"));
    }

    #[test]
    fn switch_to_inspector_panel() {
        use crate::components::session_detail::DetailPanel;
        let e = test_engine();
        let m = base(e);
        let s = Session {
            id: "s1".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: SessionStatus::Working,
            agent_type: "claude-code".into(), cost_usd: 1.23,
            started_at: 0, pr_number: Some(42), pr_id: None,
            workspace_path: Some("/tmp/w".into()), pid: Some(1234),
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        let (m, _) = m.update(Message::EngineEvent(Box::new(Event::SessionSpawned(s))));
        let (m2, _) = m.update(Message::NavigateSession("s1".into()));
        let (m3, _) = m2.update(Message::SwitchDetailPanel(DetailPanel::Inspector));
        assert!(matches!(&m3.view, View::SessionDetail { panel: DetailPanel::Inspector, .. }));
    }

    #[test]
    fn switching_to_diff_panel_without_a_workspace_marks_no_diff_synchronously() {
        use crate::components::session_detail::DetailPanel;
        let e = test_engine();
        let m = base(e);
        let s = Session {
            id: "s1".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: SessionStatus::Working,
            agent_type: "claude-code".into(), cost_usd: 1.23,
            started_at: 0, pr_number: None, pr_id: None,
            workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        let (m, _) = m.update(Message::EngineEvent(Box::new(Event::SessionSpawned(s))));
        let (m2, _) = m.update(Message::NavigateSession("s1".into()));
        let (m3, _) = m2.update(Message::SwitchDetailPanel(DetailPanel::Diff));
        assert!(matches!(&m3.view, View::SessionDetail { panel: DetailPanel::Diff, .. }));
        assert_eq!(m3.diffs.get("s1"), Some(&None));
    }

    #[test]
    fn diff_fetched_stores_the_result_by_session_id() {
        let e = test_engine();
        let m = base(e);
        let (m, _) = m.update(Message::DiffFetched {
            session_id: "s1".into(),
            diff: Some("diff --git a/x b/x\n+hello\n".into()),
        });
        assert_eq!(
            m.diffs.get("s1"),
            Some(&Some("diff --git a/x b/x\n+hello\n".to_string())),
        );
    }

    #[test]
    fn navigating_away_reverts_the_outgoing_session_to_background_width() {
        use crate::components::session_detail::DetailPanel;
        use alacritty_terminal::grid::Dimensions;
        let e = test_engine();
        let mut m = base(e);
        m.window_width  = 1200.0;
        m.window_height = 800.0;
        m.sidebar_width = 220.0;
        m.info_width    = 300.0;

        for id in ["s1", "s2"] {
            let s = Session {
                id: id.into(), orchestrator_id: None, name: "w".into(),
                repo: "r".into(), status: SessionStatus::Working,
                agent_type: "claude-code".into(), cost_usd: 0.0,
                started_at: 0, pr_number: None, pr_id: None,
                workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
            let (next, _) = m.update(Message::EngineEvent(Box::new(Event::SessionSpawned(s))));
            m = next;
        }

        // s1 becomes active and switches off Split to the full-width Terminal
        // panel, so its terminal is wider than the Split-assumed background size.
        let (m, _) = m.update(Message::NavigateSession("s1".into()));
        let background_cols = m.terminal_cols;
        let (mut m, _) = m.update(Message::SwitchDetailPanel(DetailPanel::Terminal));
        m.terminals.insert(
            "s1".into(),
            crate::components::terminal::TerminalState::new(background_cols, m.terminal_rows, None),
        );
        let (m, _) = m.update(Message::SwitchDetailPanel(DetailPanel::Terminal));
        let wide_cols = m.terminals.get("s1").unwrap().term.grid().columns();
        assert!(wide_cols > background_cols as usize);

        // Navigating away to s2 must revert s1 — now backgrounded — back to
        // the Split-assumed width instead of leaving it at its old, wider
        // active-panel size (which would corrupt s1's real tmux pane size
        // for as long as it sits in the background).
        let (m2, _) = m.update(Message::NavigateSession("s2".into()));
        let s1_term = m2.terminals.get("s1").unwrap();
        assert_eq!(
            s1_term.term.grid().columns(),
            background_cols as usize,
            "a session that just became backgrounded must be resized back to the \
             Split-assumed width, not left at its old active-panel size"
        );
    }

    #[test]
    fn switch_to_split_panel_resizes_terminal_to_fit_narrower_area() {
        use crate::components::session_detail::DetailPanel;
        use alacritty_terminal::grid::Dimensions;
        let e = test_engine();
        let mut m = base(e);
        m.window_width  = 1200.0;
        m.window_height = 800.0;
        m.sidebar_width = 220.0;
        m.info_width    = 300.0;

        let s = Session {
            id: "s1".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: SessionStatus::Working,
            agent_type: "claude-code".into(), cost_usd: 0.0,
            started_at: 0, pr_number: None, pr_id: None,
            workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        let (m, _) = m.update(Message::EngineEvent(Box::new(Event::SessionSpawned(s))));
        // NavigateSession defaults to the Split panel, so switch to Terminal
        // first to establish the full-width baseline, then attach a terminal
        // directly rather than relying on the async PTY task.
        let (m, _) = m.update(Message::NavigateSession("s1".into()));
        let (mut m, _) = m.update(Message::SwitchDetailPanel(DetailPanel::Terminal));
        m.terminals.insert(
            "s1".into(),
            crate::components::terminal::TerminalState::new(m.terminal_cols, m.terminal_rows, None),
        );
        // Re-apply Terminal now that a terminal exists, so it's sized as the
        // active (full-width) session rather than left at its initial size.
        let (m, _) = m.update(Message::SwitchDetailPanel(DetailPanel::Terminal));
        let full_width_cols = m.terminals.get("s1").unwrap().term.grid().columns();

        let (m2, _) = m.update(Message::SwitchDetailPanel(DetailPanel::Split));

        let term = m2.terminals.get("s1").unwrap();
        assert!(
            term.term.grid().columns() < full_width_cols,
            "opening the Split panel should narrow the terminal grid to fit the remaining space"
        );
        assert_eq!(term.term.grid().columns(), m2.terminal_cols as usize);
    }

    /// Regression test: `resize_terminals` used to treat every orchestrator
    /// session as terminal-only-at-full-width regardless of panel, which
    /// predates the `Plan` tab. Once an orchestrator can show `Plan`
    /// (terminal + plan pane side by side, same layout as `Split`), its
    /// terminal must narrow exactly like a worker's `Split` panel does —
    /// otherwise the PTY renders at full width while the plan pane visually
    /// overlaps it.
    #[test]
    fn opening_the_plan_panel_narrows_an_orchestrators_terminal() {
        use crate::components::session_detail::DetailPanel;
        use alacritty_terminal::grid::Dimensions;
        let e = test_engine();
        let mut m = base(e);
        m.window_width  = 1200.0;
        m.window_height = 800.0;
        m.sidebar_width = 220.0;
        m.info_width    = 300.0;

        let o = Orchestrator { id: "orch1".into(), name: "orch".into(), created_at: 0 };
        let _ = m.engine.store.upsert_orchestrator(&o);
        let (m, _) = m.update(Message::EngineEvent(Box::new(Event::OrchestratorSpawned(o))));

        let (m, _) = m.update(Message::NavigateSession("orch1".into()));
        let (mut m, _) = m.update(Message::SwitchDetailPanel(DetailPanel::Terminal));
        m.terminals.insert(
            "orch1".into(),
            crate::components::terminal::TerminalState::new(m.terminal_cols, m.terminal_rows, None),
        );
        let (m, _) = m.update(Message::SwitchDetailPanel(DetailPanel::Terminal));
        let full_width_cols = m.terminals.get("orch1").unwrap().term.grid().columns();

        let (m2, _) = m.update(Message::SwitchDetailPanel(DetailPanel::Plan));

        let term = m2.terminals.get("orch1").unwrap();
        assert!(
            term.term.grid().columns() < full_width_cols,
            "opening the Plan panel should narrow an orchestrator's terminal grid, \
             not leave it full-width under the plan pane"
        );
        assert_eq!(term.term.grid().columns(), m2.terminal_cols as usize);
    }

    #[test]
    fn plan_editor_action_drops_edits_but_applies_non_edit_actions() {
        use iced::widget::text_editor::{Action, Edit};

        use crate::components::markdown_blocks::BlockKind;

        let e = test_engine();
        let mut m = base(e);
        m.plan_docs.insert(
            "orch".into(),
            PlanDocState {
                file_path: Some("/plan.md".into()),
                blocks: vec![PlanBlockView {
                    kind: BlockKind::Paragraph,
                    content: iced::widget::text_editor::Content::with_text("hello"),
                    spans: Vec::new(),
                    links: Vec::new(),
                }],
                ..Default::default()
            },
        );
        let before = m.plan_docs.get("orch").unwrap().blocks[0].content.text();

        let (m, _) = m.update(Message::PlanEditorAction {
            orchestrator_id: "orch".into(),
            block: 0,
            action: Action::Edit(Edit::Insert('x')),
        });
        assert_eq!(
            m.plan_docs.get("orch").unwrap().blocks[0].content.text(),
            before,
            "an Edit action must never mutate a read-only plan doc"
        );

        let (m, _) = m.update(Message::PlanEditorAction {
            orchestrator_id: "orch".into(),
            block: 0,
            action: Action::SelectAll,
        });
        assert_eq!(m.plan_docs.get("orch").unwrap().blocks[0].content.text(), before);
    }

    #[test]
    fn active_sessions_non_split_panel_does_not_widen_background_sessions() {
        use crate::components::session_detail::DetailPanel;
        use alacritty_terminal::grid::Dimensions;
        let e = test_engine();
        let mut m = base(e);
        m.window_width  = 1200.0;
        m.window_height = 800.0;
        m.sidebar_width = 220.0;
        m.info_width    = 300.0;

        for id in ["s1", "s2"] {
            let s = Session {
                id: id.into(), orchestrator_id: None, name: "w".into(),
                repo: "r".into(), status: SessionStatus::Working,
                agent_type: "claude-code".into(), cost_usd: 0.0,
                started_at: 0, pr_number: None, pr_id: None,
                workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
            let (next, _) = m.update(Message::EngineEvent(Box::new(Event::SessionSpawned(s))));
            m = next;
        }

        // s1 becomes the active session; DetailPanel::default() is Split, so
        // its terminal (and the still-terminal-less s2's authoritative size)
        // both start out at the narrow, Split-assumed background width.
        let (mut m, _) = m.update(Message::NavigateSession("s1".into()));
        let background_cols = m.terminal_cols;
        m.terminals.insert(
            "s1".into(),
            crate::components::terminal::TerminalState::new(background_cols, m.terminal_rows, None),
        );
        // s2 is backgrounded at the same Split-assumed width every session
        // not currently in view is expected to sit at.
        m.terminals.insert(
            "s2".into(),
            crate::components::terminal::TerminalState::new(background_cols, m.terminal_rows, None),
        );

        // s1 switches away from Split to a full-width panel while it's the
        // one being viewed — this must widen s1 only, not backgrounded s2,
        // which will show Split (the default) again next time it's viewed.
        let (m2, _) = m.update(Message::SwitchDetailPanel(DetailPanel::Terminal));

        let s1_term = m2.terminals.get("s1").unwrap();
        let s2_term = m2.terminals.get("s2").unwrap();
        assert!(
            s1_term.term.grid().columns() > background_cols as usize,
            "the actively-viewed session should widen when it switches off the Split panel"
        );
        assert_eq!(
            s2_term.term.grid().columns(),
            background_cols as usize,
            "a backgrounded session must stay at the Split-assumed width — it isn't shown by \
             the panel change and will default back to Split next time it's navigated to"
        );
    }

    #[test]
    fn resize_terminals_budgets_the_real_session_detail_chrome() {
        // Guards against the PTY grid being sized larger than the terminal
        // `Canvas` actually rendered by `session_detail` — if this drifts
        // from the widget tree again, the bottom rows (including the live
        // prompt line) clip off-screen instead of the grid staying in sync.
        // Expected dims are derived from the same `TERM_CHROME_*` constants
        // `resize_terminals` uses, not re-typed magic numbers, so this test
        // fails the moment the two go out of sync with each other.
        use crate::components::session_detail::{TERM_CHROME_H, TERM_CHROME_W};

        use crate::components::shell::tree;

        let e = test_engine();
        let mut m = base(e);
        m.window_width   = 1200.0;
        m.window_height  = 800.0;
        m.fleet_nav.tree_collapsed = false;
        m.info_width     = 300.0;

        let (cell_w, cell_h) = crate::components::terminal::cell_size(
            crate::components::terminal::FONT_SIZE,
        );
        let sidebar_w = tree::EXPANDED_WIDTH;
        let info_w    = m.info_width + 5.0;

        let expected_bg_cols = ((m.window_width - sidebar_w - info_w - TERM_CHROME_W) / cell_w) as u16;
        let expected_bg_rows = ((m.window_height - TERM_CHROME_H) / cell_h) as u16;
        let expected_full_cols = ((m.window_width - sidebar_w - TERM_CHROME_W) / cell_w) as u16;

        App::resize_terminals(&mut m);
        assert_eq!(m.terminal_cols, expected_bg_cols);
        assert_eq!(m.terminal_rows, expected_bg_rows);

        let s = Session {
            id: "s1".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: SessionStatus::Working,
            agent_type: "claude-code".into(), cost_usd: 0.0,
            started_at: 0, pr_number: None, pr_id: None,
            workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        let (m, _) = m.update(Message::EngineEvent(Box::new(Event::SessionSpawned(s))));
        let (m, _) = m.update(Message::NavigateSession("s1".into()));
        let mut m = m;
        m.terminals.insert(
            "s1".into(),
            crate::components::terminal::TerminalState::new(m.terminal_cols, m.terminal_rows, None),
        );
        let (m, _) = m.update(Message::SwitchDetailPanel(crate::components::session_detail::DetailPanel::Terminal));
        use alacritty_terminal::grid::Dimensions;
        assert_eq!(
            m.terminals.get("s1").unwrap().term.grid().columns(),
            expected_full_cols as usize,
            "full-width Terminal panel must subtract TERM_CHROME_W, not just the sidebar"
        );
    }

    #[test]
    fn dismiss_all_clears_notifications() {
        let e = test_engine();
        let mut m = base(e);
        for id in ["a", "b"] {
            let n = Notification {
                id: id.into(), kind: NotificationKind::WorkerDone,
                title: "t".into(), body: "b".into(), session_id: None,
                created_at: 0,
            };
            m.engine.store.insert_notification(&n).unwrap();
            let (next, _) = m.update(Message::EngineEvent(Box::new(Event::Notification(n))));
            m = next;
        }
        let (m2, _) = m.update(Message::DismissAllNotifications);
        assert!(m2.notifications.is_empty());
    }

    #[test]
    fn attention_count_detects_ci_failures() {
        use crate::components::fleet_board::attention_count;
        let e = test_engine();
        let mut m = base(e);
        for (id, status) in [
            ("s1", SessionStatus::CiFailed),
            ("s2", SessionStatus::ReviewPending),
            ("s3", SessionStatus::Working),
        ] {
            let s = Session {
                id: id.into(), orchestrator_id: None, name: id.into(),
                repo: "r".into(), status,
                agent_type: "c".into(), cost_usd: 0.0,
                started_at: 0, pr_number: None, pr_id: None,
                workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
            let (next, _) = m.update(Message::EngineEvent(Box::new(Event::SessionSpawned(s))));
            m = next;
        }
        assert_eq!(attention_count(&m), 2); // ci_failed + review_pending
    }

    /// The prompt-agent singleton (MLOPS-5415) must never contribute to the
    /// Board's counts, same as it's excluded from Fleet islands — round-2
    /// review on PR #273 caught `attention_count`/`interrupted_count`/
    /// `total_session_count`/`column_sessions` all missing this.
    #[test]
    fn board_counts_exclude_the_prompt_agent_session() {
        use crate::components::fleet_board::{attention_count, column_sessions, interrupted_count, total_session_count};
        let mut m = base(test_engine());
        let mut prompt = refile_session(ninox_core::worker_type::PROMPT_SESSION_ID);
        prompt.orchestrator_id = None;
        prompt.status = SessionStatus::CiFailed;
        prompt.worker_type = Some(ninox_core::worker_type::PROMPT_WORKER_TYPE.to_string());
        m.sessions.insert(ninox_core::worker_type::PROMPT_SESSION_ID.into(), prompt);

        assert_eq!(attention_count(&m), 0, "CiFailed prompt-agent session must not count as needing attention");
        assert_eq!(total_session_count(&m), 0, "prompt-agent session must not count toward the Board total");
        assert!(
            column_sessions(&m, &SessionStatus::CiFailed, None).is_empty(),
            "prompt-agent session must not appear in any Board column",
        );

        m.sessions.get_mut(ninox_core::worker_type::PROMPT_SESSION_ID).unwrap().status = SessionStatus::Interrupted;
        assert_eq!(interrupted_count(&m), 0, "Interrupted prompt-agent session must not count toward Resume all");
    }

    /// Same exclusion, the sidebar tree's "Standalone" section — round-2
    /// review on PR #273 caught this site too.
    #[test]
    fn standalone_sidebar_section_excludes_the_prompt_agent_session() {
        use crate::components::shell::tree::standalone_sessions;
        let mut m = base(test_engine());
        let mut prompt = refile_session(ninox_core::worker_type::PROMPT_SESSION_ID);
        prompt.orchestrator_id = None;
        prompt.worker_type = Some(ninox_core::worker_type::PROMPT_WORKER_TYPE.to_string());
        m.sessions.insert(ninox_core::worker_type::PROMPT_SESSION_ID.into(), prompt);

        assert_eq!(standalone_sessions(&m).count(), 0);
    }

    /// `Message::ResumeAllSessions` must never resume the prompt-agent
    /// singleton even if it's `Interrupted` — round-2 review on PR #273:
    /// its displayed count already excludes it, so resuming it too would
    /// silently act on N+1 sessions for a button labeled "Resume all (N)".
    #[tokio::test]
    async fn resume_all_sessions_excludes_the_prompt_agent() {
        use futures::StreamExt;
        let mut m = base(test_engine());
        let mut prompt = refile_session(ninox_core::worker_type::PROMPT_SESSION_ID);
        prompt.orchestrator_id = None;
        prompt.status = SessionStatus::Interrupted;
        prompt.worker_type = Some(ninox_core::worker_type::PROMPT_WORKER_TYPE.to_string());
        m.sessions.insert(ninox_core::worker_type::PROMPT_SESSION_ID.into(), prompt);
        let mut ordinary = refile_session("s1");
        ordinary.status = SessionStatus::Interrupted;
        m.sessions.insert("s1".into(), ordinary);

        let (_m, task) = m.update(Message::ResumeAllSessions);
        let mut stream = iced_runtime::task::into_stream(task)
            .expect("at least the ordinary interrupted session must dispatch a resume");
        let mut messages = Vec::new();
        while let Some(iced_runtime::Action::Output(msg)) = stream.next().await {
            messages.push(msg);
        }
        assert_eq!(messages.len(), 1, "only the ordinary interrupted session should be resumed");
        assert!(matches!(&messages[0], Message::ResumeSession(id) if id == "s1"));
    }

    mod attach_state {
        use super::*;
        use crate::attach::{Prepared, MAX_ATTEMPTS};

        fn apply(m: &mut App, msg: Message) {
            let _ = App::apply(m, msg);
        }

        fn viewing_s1() -> App {
            let mut m = base(test_engine());
            apply(&mut m, Message::NavigateSession("s1".into()));
            m
        }

        fn token(m: &App) -> u64 {
            m.attach["s1"].token
        }

        fn sleeper() -> Vec<String> {
            vec!["sleep".into(), "300".into()]
        }

        fn fail_prepare(m: &mut App) {
            let t = token(m);
            apply(m, Message::AttachPrepared {
                session_id: "s1".into(), token: t, outcome: Prepared::Unavailable("host busy".into()),
            });
        }

        #[tokio::test]
        async fn a_spawned_client_is_not_torn_down_by_a_leftover_timer() {
            let mut m = viewing_s1();
            assert!(m.attach["s1"].has_pending());
            apply(&mut m, Message::ClientAttach { session_id: "s1".into(), argv: sleeper() });
            assert!(!m.attach["s1"].has_pending(), "spawning must cancel the prepare/retry task it replaces");
        }

        #[tokio::test]
        async fn kill_resume_and_refile_paths_drop_attach_progress() {
            let mut m = viewing_s1();
            apply(&mut m, Message::KillSession("s1".into()));
            assert!(!m.attach.contains_key("s1"));
        }

        #[tokio::test]
        async fn navigating_begins_an_attach_attempt() {
            let m = viewing_s1();
            assert_eq!(m.attach["s1"].attempt, 0);
            assert!(m.attach["s1"].failure.is_none());
        }

        #[tokio::test]
        async fn an_unanswering_host_is_retried_then_reported_with_a_reason() {
            let mut m = viewing_s1();
            for want in 1..=MAX_ATTEMPTS {
                fail_prepare(&mut m);
                assert_eq!(m.attach["s1"].attempt, want);
                assert!(m.attach["s1"].failure.is_none(), "still retrying after {want}");
                let t = token(&m);
                apply(&mut m, Message::AttachRetry { session_id: "s1".into(), token: t });
                assert_eq!(m.attach["s1"].attempt, want, "a retry keeps the count");
            }
            fail_prepare(&mut m);
            assert_eq!(m.attach["s1"].failure.as_deref(), Some("host busy"));
        }

        #[tokio::test]
        async fn a_failed_spawn_is_retried_not_swallowed() {
            let mut m = viewing_s1();
            apply(&mut m, Message::ClientAttach { session_id: "s1".into(), argv: vec!["/nonexistent/ninox-attach".into()] });
            assert!(m.clients.is_empty());
            assert_eq!(m.attach["s1"].attempt, 1);
        }

        #[tokio::test]
        async fn a_pane_that_is_gone_stops_the_attempt_without_an_error() {
            let mut m = viewing_s1();
            let t = token(&m);
            apply(&mut m, Message::AttachPrepared { session_id: "s1".into(), token: t, outcome: Prepared::Gone });
            assert!(!m.attach.contains_key("s1"));
        }

        #[tokio::test]
        async fn a_superseded_attempts_results_are_ignored() {
            let mut m = viewing_s1();
            let stale = token(&m);
            apply(&mut m, Message::ReconnectTerminal("s1".into()));
            assert_ne!(token(&m), stale);
            apply(&mut m, Message::AttachPrepared {
                session_id: "s1".into(), token: stale, outcome: Prepared::Unavailable("old".into()),
            });
            assert_eq!(m.attach["s1"].attempt, 0);
            apply(&mut m, Message::AttachRetry { session_id: "s1".into(), token: stale });
            assert_eq!(m.attach["s1"].attempt, 0);
        }

        #[tokio::test]
        async fn reconnect_and_retry_never_leave_two_attach_children() {
            let mut m = viewing_s1();
            apply(&mut m, Message::ClientAttach { session_id: "s1".into(), argv: sleeper() });
            let first = m.clients["s1"].generation;
            // A second attach racing in is refused while one is live.
            apply(&mut m, Message::ClientAttach { session_id: "s1".into(), argv: sleeper() });
            assert_eq!(m.clients.len(), 1);
            assert_eq!(m.clients["s1"].generation, first);
            // Reconnect tears the live child down before starting over.
            apply(&mut m, Message::ReconnectTerminal("s1".into()));
            assert!(m.clients.is_empty() && !m.terminals.contains_key("s1"));
            apply(&mut m, Message::ReconnectTerminal("s1".into()));
            let t = token(&m);
            apply(&mut m, Message::AttachPrepared { session_id: "s1".into(), token: t, outcome: Prepared::Ready(sleeper()) });
            apply(&mut m, Message::AttachRetry { session_id: "s1".into(), token: t });
            assert_eq!(m.clients.len(), 1);
        }

        #[tokio::test]
        async fn an_attach_that_dies_at_once_counts_against_the_retry_budget() {
            let mut m = viewing_s1();
            for want in 1..=3 {
                apply(&mut m, Message::ClientAttach { session_id: "s1".into(), argv: sleeper() });
                let generation = m.clients["s1"].generation;
                m.attach.get_mut("s1").unwrap().push_output(b"\x1b[0m");
                apply(&mut m, Message::EngineEvent(Box::new(Event::ClientClosed { session_id: "s1".into(), generation })));
                assert_eq!(m.attach["s1"].attempt, want, "a quick death must not reset the count");
            }
        }

        #[tokio::test]
        async fn an_attach_that_stayed_up_starts_the_count_over() {
            let mut m = viewing_s1();
            m.attach.get_mut("s1").unwrap().attempt = 4;
            apply(&mut m, Message::ClientAttach { session_id: "s1".into(), argv: sleeper() });
            let generation = m.clients["s1"].generation;
            let p = m.attach.get_mut("s1").unwrap();
            p.push_output(b"hello");
            p.spawned_at = Some(std::time::Instant::now() - std::time::Duration::from_secs(60));
            apply(&mut m, Message::EngineEvent(Box::new(Event::ClientClosed { session_id: "s1".into(), generation })));
            assert_eq!(m.attach["s1"].attempt, 1);
        }

        #[tokio::test]
        async fn the_childs_last_words_become_the_failure_reason() {
            let mut m = viewing_s1();
            m.attach.get_mut("s1").unwrap().attempt = MAX_ATTEMPTS;
            apply(&mut m, Message::ClientAttach { session_id: "s1".into(), argv: sleeper() });
            let generation = m.clients["s1"].generation;
            m.attach.get_mut("s1").unwrap().push_output(b"Error: ninox-ptyd request timed out after 15s\r\n");
            apply(&mut m, Message::EngineEvent(Box::new(Event::ClientClosed { session_id: "s1".into(), generation })));
            assert_eq!(m.attach["s1"].failure.as_deref(), Some("Error: ninox-ptyd request timed out after 15s"));
        }

        #[tokio::test]
        async fn a_silent_attach_is_killed_when_its_first_frame_budget_runs_out() {
            let mut m = viewing_s1();
            apply(&mut m, Message::ClientAttach { session_id: "s1".into(), argv: sleeper() });
            let generation = m.clients["s1"].generation;
            apply(&mut m, Message::AttachWatchdog { session_id: "s1".into(), generation });
            assert!(m.clients.is_empty() && !m.terminals.contains_key("s1"));
            assert_eq!(m.attach["s1"].attempt, 1);
        }

        #[tokio::test]
        async fn the_watchdog_leaves_an_attach_that_printed_alone() {
            let mut m = viewing_s1();
            apply(&mut m, Message::ClientAttach { session_id: "s1".into(), argv: sleeper() });
            let generation = m.clients["s1"].generation;
            m.attach.get_mut("s1").unwrap().push_output(b"frame");
            apply(&mut m, Message::AttachWatchdog { session_id: "s1".into(), generation });
            assert_eq!(m.clients.len(), 1);
        }

        #[tokio::test]
        async fn an_exit_whose_close_event_was_lost_is_noticed() {
            let mut m = viewing_s1();
            apply(&mut m, Message::ClientAttach { session_id: "s1".into(), argv: vec!["true".into()] });
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !m.clients["s1"].has_exited() {
                assert!(std::time::Instant::now() < deadline, "child never exited");
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            apply(&mut m, Message::AttachReap);
            assert!(!m.clients.is_empty(), "an exit gets one tick for its close event to arrive");
            apply(&mut m, Message::AttachReap);
            assert!(m.clients.is_empty(), "the dead client must be dropped");
            assert_eq!(m.attach["s1"].attempt, 1);
        }

        #[tokio::test]
        async fn leaving_a_view_releases_its_child_and_pending_work() {
            let mut m = viewing_s1();
            apply(&mut m, Message::ClientAttach { session_id: "s1".into(), argv: sleeper() });
            apply(&mut m, Message::NavigateSession("s2".into()));
            assert!(!m.clients.contains_key("s1"));
            assert!(!m.attach.contains_key("s1"));
            assert!(m.attach.contains_key("s2"));
        }

        #[tokio::test]
        async fn switching_views_repeatedly_does_not_grow_clients_or_progress() {
            let mut m = viewing_s1();
            for n in 0..30 {
                let id = format!("s{}", n % 3);
                apply(&mut m, Message::NavigateSession(id.clone()));
                apply(&mut m, Message::ClientAttach { session_id: id, argv: sleeper() });
                assert!(m.clients.len() <= 1 && m.attach.len() <= 1, "n={n} clients={:?} attach={:?}", m.clients.keys().collect::<Vec<_>>(), m.attach.keys().collect::<Vec<_>>());
            }
        }
    }

    #[tokio::test]
    async fn stale_client_closed_does_not_strand_a_freshly_reattached_session() {
        // Reproduces the critical bug: NavigateSession re-clicked on the
        // currently-viewed session drops the OLD AttachedClient (killing
        // its process) and attaches a fresh one. The OLD client's reader
        // thread still surfaces exactly one ClientClosed once the killed
        // process actually exits — carrying the OLD generation. Without
        // generation tagging, that stale event would remove the NEW
        // client + terminal out from under the view, and after the
        // one-shot reattach budget burns, strand it at "Terminal
        // connecting…" permanently.
        fn tmux_available() -> bool {
            std::process::Command::new("tmux").args(["-V"]).output()
                .map(|o| o.status.success()).unwrap_or(false)
        }
        if !tmux_available() { return; }

        let e = test_engine();
        let m = base(e);
        let sid = format!(
            "app-gen-test-{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis()
        );
        ninox_core::tmux::create_session(&sid, "/tmp", "sleep 30", &[]).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // Navigate to the session and attach the first (OLD) client.
        let (mut m, _) = m.update(Message::NavigateSession(sid.clone()));
        let argv = ninox_core::tmux::attach_args(&sid).await;
        let (m2, _) = m.update(Message::ClientAttach { session_id: sid.clone(), argv });
        m = m2;
        assert!(m.clients.contains_key(&sid), "first attach must succeed");
        let old_generation = m.clients.get(&sid).unwrap().generation;

        // Re-click the same session: NavigateSession drops the OLD client
        // synchronously (dropping AttachedClient kills its process) and
        // clears the terminal, mirroring the real re-click flow.
        let (m3, _) = m.update(Message::NavigateSession(sid.clone()));
        m = m3;
        assert!(!m.clients.contains_key(&sid), "NavigateSession must drop the old client");

        // The fresh (NEW) client attaches — different generation.
        let argv2 = ninox_core::tmux::attach_args(&sid).await;
        let (m4, _) = m.update(Message::ClientAttach { session_id: sid.clone(), argv: argv2 });
        m = m4;
        assert!(m.clients.contains_key(&sid), "second attach must succeed");
        let new_generation = m.clients.get(&sid).unwrap().generation;
        assert_ne!(old_generation, new_generation, "the two attaches must not share a generation");

        // The OLD client's reader thread now surfaces its terminal
        // ClientClosed, tagged with the OLD generation. This must be
        // ignored — the currently-live client must survive intact.
        let (m5, _) = m.update(Message::EngineEvent(Box::new(Event::ClientClosed {
            session_id: sid.clone(),
            generation: old_generation,
        })));
        m = m5;

        assert!(m.clients.contains_key(&sid), "a stale ClientClosed must not remove the current client");
        assert!(m.terminals.contains_key(&sid), "the terminal entry must remain intact");
        assert_eq!(
            m.clients.get(&sid).unwrap().generation, new_generation,
            "the surviving client must still be the new generation"
        );

        ninox_core::tmux::kill_session(&sid).await.unwrap();
    }

    #[test]
    fn fleet_filter_matches_session_name() {
        use crate::components::fleet_board::filtered_sessions;
        let e = test_engine();
        let mut m = base(e);
        for (id, name) in [("s1", "auth-fix"), ("s2", "payment-bug"), ("s3", "auth-refactor")] {
            let s = Session {
                id: id.into(), orchestrator_id: None, name: name.into(),
                repo: "r".into(), status: SessionStatus::Working,
                agent_type: "c".into(), cost_usd: 0.0,
                started_at: 0, pr_number: None, pr_id: None,
                workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
            let (next, _) = m.update(Message::EngineEvent(Box::new(Event::SessionSpawned(s))));
            m = next;
        }
        let (m2, _) = m.update(Message::FleetFilterQuery("auth".into()));
        let sessions = filtered_sessions(&m2);
        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().all(|s| s.name.contains("auth")));
    }

    fn press(app: App, ch: &str) -> App {
        let (next, _) = app.update(Message::RawKey {
            key:       iced::keyboard::Key::Character(ch.into()),
            modifiers: iced::keyboard::Modifiers::default(),
            text:      Some(ch.to_string()),
        });
        next
    }

    #[test]
    fn number_keys_switch_views() {
        let _iso = ninox_core::test_isolation::isolate();
        let m = base(test_engine());
        let m = press(m, "2");
        assert!(matches!(m.view, View::Board { .. }));
        let m = press(m, "4");
        assert!(matches!(m.view, View::Brain));
        let m = press(m, "3");
        assert!(matches!(m.view, View::Plans));
        let m = press(m, "5");
        assert!(matches!(m.view, View::WorkerTypes));
        let m = press(m, "6");
        assert!(matches!(m.view, View::Remotes));
        let m = press(m, "1");
        assert!(matches!(m.view, View::Fleet));
    }

    /// The zoom modifier: Cmd (⌘, `LOGO`) on macOS, Ctrl elsewhere — the
    /// combination `Modifiers::command()` reports as pressed.
    fn zoom_mods() -> iced::keyboard::Modifiers {
        #[cfg(target_os = "macos")]
        { iced::keyboard::Modifiers::LOGO }
        #[cfg(not(target_os = "macos"))]
        { iced::keyboard::Modifiers::CTRL }
    }

    /// Press `ch` with the platform zoom modifier held.
    fn press_cmd(app: App, ch: &str) -> App {
        let (next, _) = app.update(Message::RawKey {
            key:       iced::keyboard::Key::Character(ch.into()),
            modifiers: zoom_mods(),
            text:      Some(ch.to_string()),
        });
        next
    }

    fn approx(a: f64, b: f64) -> bool { (a - b).abs() < 1e-9 }

    #[test]
    fn cmd_zoom_steps_and_persists() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("zoom_config.toml");
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(&config_path);
            let m = base(test_engine());
            assert!(approx(m.zoom, 1.0));
            // '+' zooms in by one step.
            let m = press_cmd(m, "+");
            assert!(approx(m.zoom, 1.1), "got {}", m.zoom);
            // '=' also zooms in (on many layouts '+' is Shift+'=').
            let m = press_cmd(m, "=");
            assert!(approx(m.zoom, 1.2), "got {}", m.zoom);
            // Mirrored into config and persisted to disk.
            assert!(approx(m.config.zoom, 1.2));
            let loaded = ninox_core::config::AppConfig::load().unwrap();
            assert!(approx(loaded.zoom, 1.2), "persisted {}", loaded.zoom);
            // '-' zooms out; '0' resets.
            let m = press_cmd(m, "-");
            assert!(approx(m.zoom, 1.1), "got {}", m.zoom);
            let m = press_cmd(m, "0");
            assert!(approx(m.zoom, 1.0), "got {}", m.zoom);
            assert!(approx(ninox_core::config::AppConfig::load().unwrap().zoom, 1.0));
        }
    }

    #[test]
    fn cmd_zoom_clamps_to_range() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("zoom_clamp_config.toml");
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(&config_path);
            let mut m = base(test_engine());
            for _ in 0..40 { m = press_cmd(m, "-"); }
            assert!(approx(m.zoom, ZOOM_MIN), "min clamp got {}", m.zoom);
            for _ in 0..60 { m = press_cmd(m, "+"); }
            assert!(approx(m.zoom, ZOOM_MAX), "max clamp got {}", m.zoom);
        }
    }

    #[test]
    fn zoom_keys_survive_focused_terminal() {
        // With a terminal/split panel focused the terminal normally
        // swallows keystrokes; zoom must be intercepted first.
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("zoom_terminal_config.toml");
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(&config_path);
            let mut m = base(test_engine());
            m.view = View::SessionDetail {
                session_id: "s1".into(),
                panel: crate::components::session_detail::DetailPanel::Terminal,
            };
            let m = press_cmd(m, "+");
            assert!(approx(m.zoom, 1.1), "terminal swallowed zoom key: {}", m.zoom);
        }
    }

    /// Regression test: the `Message::RawKey` handler's `terminal_capturing`
    /// gate listed only `DetailPanel::Terminal | DetailPanel::Split`, so a
    /// bare keystroke with the Plan tab open fell through to the "no
    /// terminal focused" branch and got hijacked as a `1`/`2`/`3`/`t`
    /// navigation/theme shortcut instead of reaching the terminal — the
    /// reported "can't type in the terminal with Plan open" bug.
    /// `DetailPanel::Plan` must capture bare keys exactly like `Split` does.
    #[test]
    fn plan_panel_keeps_capturing_bare_keys_for_the_terminal() {
        let mut m = base(test_engine());
        m.view = View::SessionDetail {
            session_id: "orch1".into(),
            panel: crate::components::session_detail::DetailPanel::Plan,
        };
        m.terminal_focused = true;
        let m = press(m, "1");
        assert!(
            matches!(
                m.view,
                View::SessionDetail {
                    panel: crate::components::session_detail::DetailPanel::Plan,
                    ..
                }
            ),
            "a bare key with the Plan tab open must not be hijacked by the \
             navigation-shortcut branch: {:?}",
            m.view
        );
    }

    /// Complements `plan_panel_keeps_capturing_bare_keys_for_the_terminal`:
    /// that test only proves the `terminal_capturing` gate (guarding the
    /// bare 1/2/3/t shortcuts) includes `DetailPanel::Plan` — it can't tell
    /// "the key reached the terminal" apart from "the key was silently
    /// dropped", since `state.view` is unaffected either way. This exercises
    /// the actual PTY-write gate end to end against a real tmux pane.
    #[tokio::test]
    async fn raw_key_reaches_the_pty_when_the_plan_tab_is_open() {
        fn tmux_available() -> bool {
            std::process::Command::new("tmux").args(["-V"]).output()
                .map(|o| o.status.success()).unwrap_or(false)
        }
        if !tmux_available() { return; }

        let e = test_engine();
        let m = base(e);
        let sid = format!(
            "plan-rawkey-test-{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis()
        );
        ninox_core::tmux::create_session(&sid, "/tmp", "cat", &[]).await.unwrap();
        // The pane runs `$SHELL -l -c cat`, and the user's login profile can
        // swallow typed-ahead input: only send once `cat` itself is up.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let cat_running = match ninox_core::runtime::session_pid(&sid).await {
                Some(pid) => {
                    let pid = pid.to_string();
                    let comm = std::process::Command::new("ps").args(["-o", "comm=", "-p", &pid]).output();
                    comm.is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim().ends_with("cat"))
                        || std::process::Command::new("pgrep").args(["-x", "-P", &pid, "cat"]).status().is_ok_and(|s| s.success())
                }
                None => false,
            };
            if cat_running {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "cat never started in {sid} within 30s");
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let (mut m, _) = m.update(Message::NavigateSession(sid.clone()));
        let argv = ninox_core::tmux::attach_args(&sid).await;
        let (m2, _) = m.update(Message::ClientAttach { session_id: sid.clone(), argv });
        m = m2;
        assert!(m.clients.contains_key(&sid), "attach must succeed");

        m.view = View::SessionDetail {
            session_id: sid.clone(),
            panel: crate::components::session_detail::DetailPanel::Plan,
        };
        m.terminal_focused = true;

        // Bound (not discarded): dropping the returned `App` would drop its
        // `AttachedClient`, killing the PTY attach before the byte we just
        // queued has a chance to land. Keep it alive past the capture below.
        let (_m, _) = m.update(Message::RawKey {
            key:       iced::keyboard::Key::Character("z".into()),
            modifiers: iced::keyboard::Modifiers::default(),
            text:      Some("z".into()),
        });

        // Real tmux on a loaded machine: wait for the echo rather than a
        // fixed beat; the bound only catches a key that never arrives.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let text = loop {
            let text = String::from_utf8_lossy(&ninox_core::tmux::capture_history(&sid, 0, 5).await).into_owned();
            if text.contains('z') || std::time::Instant::now() >= deadline {
                break text;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        };
        ninox_core::tmux::kill_session(&sid).await.unwrap();
        assert!(
            text.contains('z'),
            "a keystroke with the Plan tab open must reach the PTY within 30s: {text:?}"
        );
    }

    #[test]
    fn out_of_range_config_zoom_normalized_on_load() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("zoom_load_config.toml");
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(&config_path);
            // Persist a config with an out-of-range zoom, as a hand-edit would.
            let cfg = ninox_core::config::AppConfig { zoom: 99.0, ..Default::default() };
            cfg.save().unwrap();

            let brain = Arc::new(BrainIndex::open(tempdir().unwrap().keep()).unwrap());
            let (app, _task) = App::new(
                test_engine(),
                std::path::PathBuf::from("/tmp"),
                brain,
                std::path::PathBuf::from("/tmp/t.db"),
            );
            // Both the runtime value AND the in-memory config are clamped, so
            // a later config.save() (e.g. a theme change) cannot re-persist
            // the invalid value — disk and runtime stay in sync.
            assert!(approx(app.zoom, ZOOM_MAX), "runtime zoom {}", app.zoom);
            assert!(approx(app.config.zoom, ZOOM_MAX), "config zoom {}", app.config.zoom);
        }
    }


    #[test]
    fn t_toggles_light_dark() {
        // `Message::SwitchTheme` calls `state.config.save()`, which writes to
        // `AppConfig::config_path()`. Redirect that to a tempfile so this
        // test never touches the real user config
        // (e.g. `~/Library/Application Support/ninox/config.toml`).
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("t_toggles_light_dark_config.toml");

        {
            let _iso = ninox_core::test_isolation::isolate_with_config(&config_path);
            let m = base(test_engine());
            let before = m.active_variant;
            let m = press(m, "t");
            assert_ne!(m.active_variant, before);
            let m = press(m, "t");
            assert_eq!(m.active_variant, before);
        }
    }

    #[test]
    fn cmd_b_toggles_the_tree() {
        let m = base(test_engine());
        let before = m.fleet_nav.tree_collapsed;
        let (m, _) = m.update(Message::RawKey {
            key:       iced::keyboard::Key::Character("b".into()),
            modifiers: iced::keyboard::Modifiers::COMMAND,
            text:      Some("b".to_string()),
        });
        assert_eq!(m.fleet_nav.tree_collapsed, !before);
    }

    #[test]
    fn toggling_the_tree_reflows_an_open_terminal() {
        use crate::components::session_detail::DetailPanel;
        use alacritty_terminal::grid::Dimensions;
        let mut m = base(test_engine());
        m.window_width = 1200.0;
        m.window_height = 800.0;
        let o = Orchestrator { id: "orch1".into(), name: "orch".into(), created_at: 0 };
        let _ = m.engine.store.upsert_orchestrator(&o);
        let (m, _) = m.update(Message::EngineEvent(Box::new(Event::OrchestratorSpawned(o))));
        let (m, _) = m.update(Message::NavigateSession("orch1".into()));
        let (mut m, _) = m.update(Message::SwitchDetailPanel(DetailPanel::Terminal));
        m.terminals.insert(
            "orch1".into(),
            crate::components::terminal::TerminalState::new(m.terminal_cols, m.terminal_rows, None),
        );
        let (m, _) = m.update(Message::SwitchDetailPanel(DetailPanel::Terminal));
        assert!(m.fleet_nav.tree_collapsed);
        let before = m.terminals.get("orch1").unwrap().term.grid().columns();

        let (m, _) = m.update(Message::ToggleTreeCollapsed);

        let after = m.terminals.get("orch1").unwrap().term.grid().columns();
        assert!(after < before, "expanding the rail must narrow the terminal ({before} -> {after})");
    }

    /// iced reports sizes in zoomed logical px, but a zoom change sends no
    /// resize event, so the stored size has to follow the zoom itself.
    #[test]
    fn zooming_rescales_the_window_size_layout_uses() {
        let dir = tempdir().unwrap();
        let _iso = ninox_core::test_isolation::isolate_with_config(dir.path().join("config.toml"));
        let mut m = base(test_engine());
        m.window_width = 1300.0;
        m.window_height = 800.0;
        let zoom_in = Message::RawKey {
            key:       iced::keyboard::Key::Character("=".into()),
            modifiers: iced::keyboard::Modifiers::COMMAND,
            text:      Some("=".to_string()),
        };
        let (m, _) = m.update(zoom_in);
        assert!(m.zoom > 1.0);
        assert!((m.window_width - 1300.0 / m.zoom as f32).abs() < 0.5, "{}", m.window_width);
        assert!((m.window_height - 800.0 / m.zoom as f32).abs() < 0.5, "{}", m.window_height);

        let reset = Message::RawKey {
            key:       iced::keyboard::Key::Character("0".into()),
            modifiers: iced::keyboard::Modifiers::COMMAND,
            text:      Some("0".to_string()),
        };
        let (m, _) = m.update(reset);
        assert!((m.window_width - 1300.0).abs() < 0.5, "{}", m.window_width);
    }

    /// iced lays out in zoomed logical px; until the first resize the window
    /// is its default 1024×768 physical size seen through the saved zoom.
    #[test]
    fn startup_window_size_accounts_for_the_saved_zoom() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("zoom_config.toml");
        let _iso = ninox_core::test_isolation::isolate_with_config(&config_path);
        ninox_core::config::AppConfig { zoom: 1.3, ..ninox_core::config::AppConfig::default() }.save().unwrap();
        let store = Arc::new(Store::open(dir.path().join("t.db")).unwrap());
        let brain = Arc::new(BrainIndex::open(tempdir().unwrap().keep()).unwrap());
        let (app, _) = App::new(
            Engine::new(store),
            std::path::PathBuf::from("/tmp"),
            brain,
            std::path::PathBuf::from("/tmp/t.db"),
        );
        assert!((app.window_width - 1024.0 / 1.3).abs() < 0.5, "{}", app.window_width);
        assert!((app.window_height - 768.0 / 1.3).abs() < 0.5, "{}", app.window_height);
    }

    #[test]
    fn mouse_released_without_a_drag_is_a_no_op() {
        // The v1 shell dropped the sidebar's drag-to-resize handle — only
        // `DragTarget::InfoPanel` remains, and it's intentionally not
        // persisted (session-local). This just guards that releasing with
        // no drag in flight doesn't panic or resize anything unexpectedly.
        let m = base(test_engine());
        assert!(m.drag.is_none());
        let (m, _) = m.update(Message::MouseReleased);
        assert!(m.drag.is_none());
    }

    #[test]
    fn startup_restores_persisted_sidebar_geometry_clamped() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("restore_sidebar_config.toml");
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(&config_path);
            // An over-range width must clamp into the 150–400 drag band.
            let cfg = ninox_core::config::AppConfig {
                sidebar_width: 999.0,
                ..ninox_core::config::AppConfig::default()
            };
            cfg.save().unwrap();

            let store = Arc::new(Store::open(dir.path().join("t.db")).unwrap());
            let engine = Engine::new(store);
            let brain = Arc::new(BrainIndex::open(tempdir().unwrap().keep()).unwrap());
            let (app, _task) = App::new(
                engine,
                std::path::PathBuf::from("/tmp"),
                brain,
                std::path::PathBuf::from("/tmp/t.db"),
            );
            assert_eq!(app.sidebar_width, 400.0);
            // The in-memory config that a later `config.save()` writes back
            // must hold the clamped width too — not the raw out-of-range
            // 999.0 — so disk never drifts from what the UI renders.
            assert_eq!(app.config.sidebar_width, 400.0);
        }
    }

    #[test]
    fn startup_clamps_below_range_width() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("restore_sidebar_low_config.toml");
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(&config_path);
            let cfg = ninox_core::config::AppConfig {
                sidebar_width: 10.0,
                ..ninox_core::config::AppConfig::default()
            };
            cfg.save().unwrap();

            let store = Arc::new(Store::open(dir.path().join("t.db")).unwrap());
            let engine = Engine::new(store);
            let brain = Arc::new(BrainIndex::open(tempdir().unwrap().keep()).unwrap());
            let (app, _task) = App::new(
                engine,
                std::path::PathBuf::from("/tmp"),
                brain,
                std::path::PathBuf::from("/tmp/t.db"),
            );
            assert_eq!(app.sidebar_width, 150.0);
            assert_eq!(app.config.sidebar_width, 150.0);
        }
    }

    #[test]
    fn settings_fields_validate_like_the_tui_and_persist() {
        use ninox_ui_shared::settings_form::FieldId;
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("settings_fields.toml");
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(&config_path);
            let m = base(test_engine());

            let (m, _) = m.update(Message::SettingsDraft { id: FieldId::Port, text: "70000".into() });
            let (m, _) = m.update(Message::SettingsCommit(FieldId::Port));
            assert!(m.settings.error(&FieldId::Port).is_some_and(|e| e.contains("not a port")));
            assert_eq!(m.settings.draft(&FieldId::Port), Some("70000"), "a refused value stays editable");

            let (m, _) = m.update(Message::SettingsDraft { id: FieldId::Port, text: "9123".into() });
            let (m, _) = m.update(Message::SettingsCommit(FieldId::Port));
            assert_eq!((m.config.port, m.settings.error(&FieldId::Port)), (9123, None));
            assert_eq!(m.settings.draft(&FieldId::Port), None);

            let (m, _) = m.update(Message::SettingsSet { id: FieldId::Prefix, text: "Ctrl+b".into() });
            assert!(m.settings.error(&FieldId::Prefix).is_some(), "tmux's own prefix is refused, as in the TUI");
            let (m, _) = m.update(Message::SettingsSet { id: FieldId::Prefix, text: "Ctrl+g".into() });
            let (m, _) = m.update(Message::SettingsSet { id: FieldId::RestorePolicy, text: "auto".into() });
            let (m, _) = m.update(Message::SettingsSet { id: FieldId::WorkerCompletion, text: "manual".into() });
            let (m, _) = m.update(Message::SettingsSet { id: FieldId::RetentionDays, text: "7".into() });
            let (m, _) = m.update(Message::SettingsSet { id: FieldId::OrchModel, text: "two words".into() });
            assert!(m.settings.error(&FieldId::OrchModel).is_some());

            let loaded = ninox_core::config::AppConfig::load().unwrap();
            assert_eq!(loaded.port, 9123);
            assert_eq!(loaded.tui.prefix, "Ctrl+g");
            assert_eq!(loaded.fleet.restore_policy, ninox_core::config::RestorePolicy::Auto);
            assert_eq!(loaded.completion_mode(), ninox_core::config::CompletionMode::Manual);
            assert_eq!(loaded.session_retention.done_retention_days, 7);
            assert_eq!(m.config.tui.prefix, "Ctrl+g");
        }
    }

    #[test]
    fn switching_the_orchestrator_harness_updates_the_spawn_preselection() {
        use ninox_ui_shared::settings_form::FieldId;
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("settings_orch.toml");
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(&config_path);
            let m = base(test_engine());
            let (m, _) = m.update(Message::SettingsToggleHarness("codex".into()));
            let (m, _) = m.update(Message::SettingsDraft { id: FieldId::OrchModel, text: "opus-x".into() });
            let (m, _) = m.update(Message::SettingsSet { id: FieldId::OrchHarness, text: "codex".into() });
            assert_eq!(m.config.orchestrator.harness, "codex");
            assert_eq!(m.settings.draft(&FieldId::OrchModel), None, "a custom model typed for the old harness is dropped");
            let (m, _) = m.update(Message::SettingsSet { id: FieldId::OrchHarness, text: "not-a-harness".into() });
            assert!(m.settings.error(&FieldId::OrchHarness).is_some());
            assert_eq!(m.config.orchestrator.harness, "codex");
        }
    }

    #[test]
    fn settings_changes_are_judged_against_the_file_on_disk() {
        use ninox_ui_shared::settings_form::FieldId;
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("settings_disk.toml");
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(&config_path);
            let m = base(test_engine());
            let (mut m, _) = m.update(Message::SettingsToggleHarness("codex".into()));
            // Disabled elsewhere (the TUI, an editor) while this panel is open.
            let mut on_disk = ninox_core::config::AppConfig::load().unwrap();
            on_disk.toggle_harness("codex");
            on_disk.save().unwrap();
            assert!(m.config.registry().enabled_names().contains(&"codex".to_string()), "panel still thinks it's on");

            m = m.update(Message::SettingsSet { id: FieldId::OrchHarness, text: "codex".into() }).0;
            assert!(m.settings.error(&FieldId::OrchHarness).is_some(), "refused, not silently dropped");
            assert_eq!(ninox_core::config::AppConfig::load().unwrap().orchestrator.harness, "claude-code");
            assert!(!m.config.registry().enabled_names().contains(&"codex".to_string()), "the panel now shows what's on disk");

            std::fs::write(&config_path, "port = [nope").unwrap();
            let (m, _) = m.update(Message::SettingsSet { id: FieldId::Port, text: "9000".into() });
            assert!(m.settings.error(&FieldId::Port).is_some_and(|e| e.contains("could not save")), "{:?}", m.settings.errors);
            assert_eq!(std::fs::read_to_string(&config_path).unwrap(), "port = [nope", "a broken file is left alone");
        }
    }

    #[test]
    fn settings_drafts_never_mask_the_saved_value() {
        use ninox_ui_shared::settings_form::FieldId;
        let m = base(test_engine());
        let saved = m.config.port.to_string();
        let (m, _) = m.update(Message::SettingsDraft { id: FieldId::Port, text: "1".into() });
        let (m, _) = m.update(Message::SettingsDraft { id: FieldId::Port, text: saved });
        assert_eq!(m.settings.draft(&FieldId::Port), None, "typed back to the saved value");

        let (m, _) = m.update(Message::SettingsDraft { id: FieldId::OrchModel, text: String::new() });
        assert_eq!(m.settings.draft(&FieldId::OrchModel), Some(""), "custom mode stays open even when empty");
        let (m, _) = m.update(Message::SettingsRevert(FieldId::OrchModel));
        assert_eq!(m.settings.draft(&FieldId::OrchModel), None, "Cancel leaves custom mode");

        let (m, _) = m.update(Message::SettingsDraft { id: FieldId::RetentionDays, text: "x".into() });
        let (m, _) = m.update(Message::NavigateSettings);
        assert!(m.settings.drafts.is_empty());
    }

    #[test]
    fn session_limits_parse_like_the_tui_and_refuse_inline() {
        use ninox_ui_shared::settings_form::FieldId;
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("settings_limits.toml");
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(&config_path);
            let limit = |m: &App| m.config.limits.limit_for(None);
            let commit = |m: App, text: &str| {
                let (m, _) = m.update(Message::SettingsLimitInput(None, text.into()));
                m.update(Message::SettingsLimitCommit(None)).0
            };
            let m = commit(base(test_engine()), "4");
            assert_eq!(limit(&m), Some(4));

            let m = commit(m, "lots");
            let id = FieldId::SessionLimit(None);
            assert!(m.settings.error(&id).is_some_and(|e| e.contains("not a whole number")), "refused inline, not dropped");
            assert_eq!(m.settings.draft(&id), Some("lots"), "the typed text stays to fix");
            assert_eq!(limit(&m), Some(4));

            let m = commit(m, "Unlimited");
            assert_eq!((limit(&m), m.settings.error(&id)), (None, None));
            let (m, _) = m.update(Message::SettingsLimitToggleUnlimited(None));
            assert_eq!(limit(&m), Some(6), "switching Unlimited off starts from a cap");
            let m = commit(m, "0");
            assert_eq!(limit(&m), None, "0 means unlimited");
            assert_eq!(ninox_core::config::AppConfig::load().unwrap().limits.limit_for(None), None);
        }
    }

    #[test]
    fn settings_drafts_revert_and_sections_switch() {
        use crate::components::settings_panel::SettingsSection;
        use ninox_ui_shared::settings_form::FieldId;
        let m = base(test_engine());
        let (m, _) = m.update(Message::SettingsSection(SettingsSection::Fleet));
        assert_eq!(m.settings.section, SettingsSection::Fleet);
        let (m, _) = m.update(Message::SettingsDraft { id: FieldId::RetentionDays, text: "x".into() });
        let (m, _) = m.update(Message::SettingsRevert(FieldId::RetentionDays));
        assert_eq!(m.settings.draft(&FieldId::RetentionDays), None);
    }

    #[test]
    fn toggling_a_harness_enables_it_and_persists() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("toggle_harness_config.toml");
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(&config_path);
            let m = base(test_engine());
            assert!(!m.config.registry().enabled_names().contains(&"codex".to_string()));
            let (m, _) = m.update(Message::SettingsToggleHarness("codex".into()));
            assert!(m.config.registry().enabled_names().contains(&"codex".to_string()));
            // persisted: a fresh load sees it too, with the builtin's args
            // intact (the toggle writes the full effective spec)
            let loaded = ninox_core::config::AppConfig::load().unwrap();
            assert!(loaded.registry().enabled_names().contains(&"codex".to_string()));
            assert!(loaded.registry().spec("codex").worker_args.is_some());
            // toggling back disables
            let (m, _) = m.update(Message::SettingsToggleHarness("codex".into()));
            assert!(!m.config.registry().enabled_names().contains(&"codex".to_string()));
        }
    }

    #[test]
    fn choosing_the_session_runtime_persists_it() {
        use ninox_core::runtime::Backend;
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("runtime_backend_config.toml");
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(&config_path);
            let m = base(test_engine());
            assert_eq!(m.config.runtime.backend, Backend::Tmux);
            // Tmux needs no host, so this stays a pure config write.
            let (m, _) = m.update(Message::SettingsSetRuntimeBackend(Backend::Tmux));
            assert_eq!(m.config.runtime.backend, Backend::Tmux);
            let mut m = m;
            m.config.runtime.backend = Backend::Ptyd;
            m.config.save().unwrap();
            assert_eq!(ninox_core::config::AppConfig::load().unwrap().runtime.backend, Backend::Ptyd);
            let (m, _) = m.update(Message::SettingsSetRuntimeBackend(Backend::Tmux));
            assert_eq!(m.config.runtime.backend, Backend::Tmux);
            assert_eq!(ninox_core::config::AppConfig::load().unwrap().runtime.backend, Backend::Tmux);
        }
    }

    #[test]
    fn settings_changes_keep_edits_made_on_disk_meanwhile() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("concurrent_edit_config.toml");
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(&config_path);
            let m = base(test_engine());
            // `nx`'s settings form writes the file while the app is open.
            let mut on_disk = ninox_core::config::AppConfig::load().unwrap();
            on_disk.tui.prefix = "C-a".into();
            on_disk.save().unwrap();

            let (m, _) = m.update(Message::SettingsTogglePrWatch);
            let (m, _) = m.update(Message::SettingsSetRuntimeBackend(ninox_core::runtime::Backend::Tmux));
            let loaded = ninox_core::config::AppConfig::load().unwrap();
            assert!(!loaded.pr_watch.enabled, "the toggle flips the default-on watcher off");
            assert_eq!(loaded.tui.prefix, "C-a", "the app must not write back its stale copy");
            assert_eq!(m.config.tui.prefix, "C-a", "the app adopts what it saved");
        }
    }

    #[test]
    fn claude_code_toggle_is_inert() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("claude_toggle_config.toml");
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(&config_path);
            let m = base(test_engine());
            let (m, _) = m.update(Message::SettingsToggleHarness("claude-code".into()));
            assert!(m.config.registry().enabled_names().contains(&"claude-code".to_string()));
            assert!(m.config.harnesses.is_empty(), "inert toggle must not write config");
        }
    }

    #[test]
    fn worker_harness_change_persists_and_clears_model() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("worker_harness_config.toml");
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(&config_path);
            let m = base(test_engine());
            let (m, _) = m.update(Message::SettingsWorkerModel("claude-haiku-4-5".into()));
            let (m, _) = m.update(Message::SettingsToggleHarness("codex".into()));
            let (m, _) = m.update(Message::SettingsWorkerHarness("codex".into()));
            assert_eq!(m.config.worker.harness, "codex");
            assert!(m.config.worker.model.is_none(), "stale model must not leak across harnesses");
            let loaded = ninox_core::config::AppConfig::load().unwrap();
            assert_eq!(loaded.worker.harness, "codex");
            assert!(loaded.worker.model.is_none());
        }
    }

    #[test]
    fn worker_model_pick_persists_and_custom_commits() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("worker_model_config.toml");
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(&config_path);
            let m = base(test_engine());
            let (m, _) = m.update(Message::SettingsWorkerModel("claude-haiku-4-5".into()));
            assert_eq!(m.config.worker.model.as_deref(), Some("claude-haiku-4-5"));

            // custom… opens the input (seeded with the current model)
            // without touching config
            let (m, _) = m.update(Message::SettingsWorkerModel(ninox_ui_shared::models::CUSTOM_SENTINEL.into()));
            assert_eq!(m.settings.worker_custom.as_deref(), Some("claude-haiku-4-5"));
            assert_eq!(m.config.worker.model.as_deref(), Some("claude-haiku-4-5"));

            // typing + commit writes and closes custom mode
            let (m, _) = m.update(Message::SettingsWorkerCustomModel("my-model".into()));
            let (m, _) = m.update(Message::SettingsWorkerCustomCommit);
            assert_eq!(m.config.worker.model.as_deref(), Some("my-model"));
            assert!(m.settings.worker_custom.is_none());
            let loaded = ninox_core::config::AppConfig::load().unwrap();
            assert_eq!(loaded.worker.model.as_deref(), Some("my-model"));

            // committing a blank custom clears the model (harness default)
            let (m, _) = m.update(Message::SettingsWorkerModel(ninox_ui_shared::models::CUSTOM_SENTINEL.into()));
            let (m, _) = m.update(Message::SettingsWorkerCustomModel("   ".into()));
            let (m, _) = m.update(Message::SettingsWorkerCustomCommit);
            assert!(m.config.worker.model.is_none());
        }
    }

    #[test]
    fn spawn_confirm_never_writes_the_pick_into_config() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("no_writeback_config.toml");
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(&config_path);
            let mut m = base(test_engine());
            for msg in [
                Message::SpawnSession,
                Message::SpawnFormName("pick-once".into()),
                Message::SpawnFormModel("claude-haiku-4-5".into()),
                Message::SpawnFormConfirm,
            ] {
                m = m.update(msg).0;
            }
            assert!(!config_path.exists() || ninox_core::config::AppConfig::load().unwrap().orchestrator.model.is_none());
            let (m, _) = m.update(Message::SpawnSession);
            assert_eq!(
                m.spawn_modal.as_ref().unwrap().model.as_deref(), Some("claude-opus-5-5"),
                "the next dialog starts from the resolved default again, not the last pick",
            );
        }
    }

    #[test]
    fn spawn_dialog_preselects_the_resolved_model_per_kind_and_type() {
        use crate::components::spawn_modal::SpawnKind;
        let mut m = base(test_engine());
        m.config.orchestrator.model = Some("orch-m".into());
        m.config.worker.model = Some("worker-m".into());
        m.config.worker_types.push(ninox_core::worker_type::WorkerTypeSpec {
            name: "Researcher".into(), model: Some("research-m".into()), ..Default::default()
        });
        let (m, _) = m.update(Message::SpawnSession);
        assert_eq!(m.spawn_modal.as_ref().unwrap().model.as_deref(), Some("orch-m"));
        let (m, _) = m.update(Message::SpawnFormKind(SpawnKind::Standalone));
        assert_eq!(m.spawn_modal.as_ref().unwrap().model.as_deref(), Some("worker-m"));
        let (m, _) = m.update(Message::SpawnFormWorkerType("Researcher".into()));
        assert_eq!(m.spawn_modal.as_ref().unwrap().model.as_deref(), Some("research-m"));
        let line = crate::components::spawn_modal::effective_model_line(&m.spawn_modal.as_ref().unwrap().resolved(&m.config));
        assert_eq!(line, "Model: research-m (from type)");
        let (m, _) = m.update(Message::SpawnFormModel("picked".into()));
        let (m, _) = m.update(Message::SpawnFormWorkerType("Evaluator".into()));
        let form = m.spawn_modal.as_ref().unwrap();
        assert_eq!(form.model.as_deref(), Some("picked"), "a user pick survives a type change");
        assert_eq!(form.resolved(&m.config).source, ninox_core::model_resolution::ModelSource::Explicit);
    }

    #[test]
    fn spawn_dialog_harness_change_shows_that_harness_resolved_model() {
        let mut m = base(test_engine());
        m.config.harnesses.insert("codex".into(), ninox_core::harness::HarnessSpec {
            enabled: true, worker_args: Some(vec![]), ..Default::default()
        });
        m.config.orchestrator.model = Some("orch-m".into());
        let (m, _) = m.update(Message::SpawnSession);
        let (m, _) = m.update(Message::SpawnFormHarness("codex".into()));
        let form = m.spawn_modal.as_ref().unwrap();
        assert_eq!(form.model, None, "[orchestrator].model is for claude-code");
        assert_eq!(form.resolved(&m.config).agent.model, None);
        let (m, _) = m.update(Message::SpawnFormHarness("claude-code".into()));
        let form = m.spawn_modal.as_ref().unwrap();
        assert_eq!(form.model.as_deref(), Some("orch-m"));
        assert_eq!(form.resolved(&m.config).agent.model.as_deref(), Some("orch-m"));
    }

    #[test]
    fn spawn_dialog_never_preselects_a_disabled_type_harness() {
        use crate::components::spawn_modal::SpawnKind;
        let mut m = base(test_engine());
        m.config.worker.model = Some("worker-m".into());
        m.config.worker_types.push(ninox_core::worker_type::WorkerTypeSpec {
            name: "Codexer".into(), harness: "codex".into(), model: Some("gpt-5".into()), ..Default::default()
        });
        let (m, _) = m.update(Message::SpawnSession);
        let (m, _) = m.update(Message::SpawnFormKind(SpawnKind::Standalone));
        let (m, _) = m.update(Message::SpawnFormWorkerType("Codexer".into()));
        let form = m.spawn_modal.as_ref().unwrap();
        assert_eq!(form.harness, "claude-code");
        assert_eq!(form.resolved(&m.config).agent.harness, "claude-code");
        assert_eq!(form.model.as_deref(), Some("worker-m"));
    }

    #[test]
    fn spawn_dialog_blank_custom_model_keeps_the_picked_model() {
        let m = base(test_engine());
        let (m, _) = m.update(Message::SpawnSession);
        let (m, _) = m.update(Message::SpawnFormModel("claude-haiku-4-5".into()));
        let (m, _) = m.update(Message::SpawnFormModel(ninox_ui_shared::models::CUSTOM_SENTINEL.into()));
        let form = m.spawn_modal.as_ref().unwrap();
        assert_eq!(form.resolved(&m.config).agent.model.as_deref(), Some("claude-haiku-4-5"));
    }

    #[test]
    fn esc_closes_spawn_modal() {
        let m = base(test_engine());
        let (m, _) = m.update(Message::SpawnSession);
        assert!(m.spawn_modal.is_some());
        let (m, _) = m.update(Message::RawKey {
            key:       iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape),
            modifiers: iced::keyboard::Modifiers::default(),
            text:      None,
        });
        assert!(m.spawn_modal.is_none());
    }

    // ── Remotes tab ────────────────────────────────────────────────────
    // `navigate_remotes_sets_view` already exists above, alongside the
    // other bare Navigate* view-switch tests.

    #[test]
    fn navigating_to_remotes_populates_rows_from_config() {
        let dir = tempdir().unwrap();
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(dir.path().join("config.toml"));
            ninox_core::config::AppConfig::update(|c| {
                c.remote_machines.machines = vec![ninox_core::config::MachineProfile {
                    id: "a".into(), label: "A".into(), ssh_target: "a@host".into(),
                    remote_session: "default".into(), enabled: true,
                }];
                Some(())
            }).unwrap();

            let m = base(test_engine());
            let (m, _) = m.update(Message::NavigateRemotes);

            assert!(matches!(m.view, View::Remotes));
            assert_eq!(m.remotes.rows.len(), 1);
            assert_eq!(m.remotes.rows[0].id, "a");
        }
    }

    fn idle_engine_with_workers() -> Arc<Engine> {
        use ninox_core::types::{ActivityState, SessionStatus};
        let engine = test_engine();
        let now = now_ms_i64();
        engine.store.upsert_orchestrator(&Orchestrator { id: "o1".into(), name: "o1".into(), created_at: 0 }).unwrap();
        for (id, orch) in [("o1", None), ("w-idle", Some("o1")), ("w-busy", Some("o1"))] {
            let mut s = pr_session(Some(7), None, "acme/app");
            s.id = id.into();
            s.name = id.into();
            s.orchestrator_id = orch.map(Into::into);
            s.status = SessionStatus::PrOpen;
            s.activity = if id == "w-busy" { ActivityState::Working } else { ActivityState::Idle };
            s.activity_since = Some(now - 90 * 60_000);
            engine.store.upsert_session(&s).unwrap();
        }
        engine
    }

    #[test]
    fn idle_tab_lists_idle_workers_never_orchestrators_and_key_7_opens_it() {
        let _iso = ninox_core::test_isolation::isolate();
        let (m, _) = base(idle_engine_with_workers()).update(Message::NavigateIdle);
        assert!(matches!(m.view, View::Idle));
        let ids: Vec<_> = m.idle.report.workers().map(|w| w.id.clone()).collect();
        assert_eq!(ids, ["w-idle"]);
        assert_eq!(m.idle.report.orchestrators_to_prompt(), 1);
        let _ = crate::components::idle_view::idle_view(&m);
    }

    #[test]
    fn idle_prompt_is_rate_limited_across_repeated_clicks() {
        let _iso = ninox_core::test_isolation::isolate();
        let (m, _) = base(idle_engine_with_workers()).update(Message::NavigateIdle);
        let store = m.engine.store.clone();
        let now = now_ms_i64();
        let cooldown = m.config.idle_workers.prompt_cooldown_minutes as i64 * 60_000;
        assert!(store.claim_idle_prompt("o1", "w-idle", now, cooldown).unwrap(), "the first click claims the send");
        let (m, _) = m.update(Message::IdlePromptDone(vec!["o1: prompted".into()]));
        assert!(!m.idle.busy);
        assert!(m.idle.report.groups[0].prompted_at.is_some(), "the group shows when it was prompted");
        assert!(!store.claim_idle_prompt("o1", "w-idle", now + 60_000, cooldown).unwrap(), "a second click inside the cooldown sends nothing");
    }

    #[test]
    fn remote_remove_confirm_removes_profile_and_sets_success_notice() {
        let dir = tempdir().unwrap();
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(dir.path().join("config.toml"));
            ninox_core::config::AppConfig::update(|c| {
                c.remote_machines.machines = vec![ninox_core::config::MachineProfile {
                    id: "a".into(), label: "A".into(), ssh_target: "a@host".into(),
                    remote_session: "default".into(), enabled: true,
                }];
                Some(())
            }).unwrap();

            let (m, _) = base(test_engine()).update(Message::NavigateRemotes);
            let (m, _) = m.update(Message::RemoteRemoveRequest("a".into()));
            assert_eq!(m.remotes.remove_confirm, Some("a".into()));
            let (m, _) = m.update(Message::RemoteRemoveConfirm);
            assert!(m.remotes.remove_confirm.is_none());
            assert!(m.remotes.rows.is_empty());
            assert!(matches!(m.remotes.notice, Some(Ok(_))));
        }
    }

    /// Health is written to the store by the poller with no event of its
    /// own, so the tab must pick it up on the periodic `PollSessions` tick
    /// — otherwise a fake host would sit on "checking" until the user
    /// navigates away and back.
    #[test]
    fn poll_tick_refreshes_remotes_health_while_the_tab_is_showing() {
        let dir = tempdir().unwrap();
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(dir.path().join("config.toml"));
            ninox_core::config::AppConfig::update(|c| {
                c.remote_machines.enabled = true;
                c.remote_machines.machines = vec![ninox_core::config::MachineProfile {
                    id: "a".into(), label: "A".into(), ssh_target: "a@host".into(),
                    remote_session: "default".into(), enabled: true,
                }];
                Some(())
            }).unwrap();

            let (m, _) = base(test_engine()).update(Message::NavigateRemotes);
            assert!(m.remotes.monitoring_enabled);
            assert_eq!(m.remotes.rows[0].health, None);

            m.engine.store.set_machine_health("a", ninox_core::types::MachineHealth::Unreachable).unwrap();
            let (m, _) = m.update(Message::PollSessions);
            assert_eq!(m.remotes.rows[0].health, Some(ninox_core::types::MachineHealth::Unreachable));
        }
    }

    #[test]
    fn remote_remove_confirm_missing_id_sets_failure_notice() {
        let dir = tempdir().unwrap();
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(dir.path().join("config.toml"));
            let (m, _) = base(test_engine()).update(Message::NavigateRemotes);
            let (m, _) = m.update(Message::RemoteRemoveRequest("ghost".into()));
            let (m, _) = m.update(Message::RemoteRemoveConfirm);
            assert!(matches!(m.remotes.notice, Some(Err(_))));
        }
    }

    #[test]
    fn remote_add_submit_rejects_blank_host() {
        let (m, _) = base(test_engine()).update(Message::RemoteAddOpen);
        let (m, _) = m.update(Message::RemoteAddHostChanged("   ".into()));
        let (m, _) = m.update(Message::RemoteAddSubmit);
        let form = m.remotes.add.as_ref().unwrap();
        assert!(form.error.is_some());
        assert!(!form.running);
        assert!(!m.clients.contains_key(crate::components::remotes::MACHINE_ADD_SESSION_ID));
    }

    // Deliberately NOT testing the successful-submit path through
    // `Message::RemoteAddSubmit` itself (as opposed to calling
    // `App::spawn_machine_add` directly, as the tests below do): it
    // resolves `canonical_exe()` to this very test binary and would spawn
    // it with argv `["machine", "add", <host>]`. libtest's own arg parser
    // treats bare positional args as substring filters, so that child
    // process would re-run every test whose name contains "machine" or
    // "add" — including this file's own `*machine_add*` tests — recursing
    // into a grandchild of itself. Validation paths that return before
    // reaching `canonical_exe()` (blank host above, duplicate host below)
    // are safe and tested directly; the trimmed-host fix in
    // `RemoteAddSubmit` (storing the trimmed value back into `form.host`
    // before it's used for argv or the later `AppConfig` comparison) is
    // verified by inspection instead.
    #[test]
    fn remote_add_submit_rejects_an_already_tracked_host() {
        let dir = tempdir().unwrap();
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(dir.path().join("config.toml"));
            ninox_core::config::AppConfig::update(|c| {
                c.remote_machines.machines.push(ninox_core::config::MachineProfile {
                    id: "a".into(), label: "a".into(), ssh_target: "a@host".into(),
                    remote_session: "default".into(), enabled: true,
                });
                Some(())
            }).unwrap();

            let (m, _) = base(test_engine()).update(Message::RemoteAddOpen);
            let (m, _) = m.update(Message::RemoteAddHostChanged("a@host".into()));
            let (m, _) = m.update(Message::RemoteAddSubmit);
            let form = m.remotes.add.as_ref().unwrap();
            assert!(
                form.error.as_deref().is_some_and(|e| e.contains("already tracks")),
                "expected an already-tracked error, got {:?}", form.error
            );
            assert!(!form.running);
            assert!(!m.clients.contains_key(crate::components::remotes::MACHINE_ADD_SESSION_ID));
        }
    }

    #[test]
    fn machine_add_cols_rows_exactly_fills_the_modal_canvas() {
        let (cell_w, cell_h) = crate::components::terminal::cell_size(crate::components::terminal::FONT_SIZE);
        let (canvas_w, canvas_h) = crate::components::remotes::MACHINE_ADD_CANVAS_SIZE;
        let (cols, rows) = crate::components::remotes::machine_add_cols_rows();
        assert_eq!(cols, (canvas_w / cell_w) as u16);
        assert_eq!(rows, (canvas_h / cell_h) as u16);
        // Sanity bound: large enough to show a real SSH prompt/session
        // picker without immediately wrapping or clipping.
        assert!(cols >= 60, "cols too narrow for a real prompt: {cols}");
        assert!(rows >= 15, "rows too short to show a session picker: {rows}");
    }

    #[tokio::test]
    async fn spawn_machine_add_uses_the_modal_canvas_size_not_the_session_detail_background_size() {
        let e = test_engine();
        let mut m = base(e);
        // Pick background sizes far from the modal canvas's own fixed size
        // so a regression (falling back to `state.terminal_cols/rows`)
        // can't coincidentally match.
        m.terminal_cols = 9;
        m.terminal_rows = 9;
        m.remotes.add = Some(crate::components::remotes::AddRemoteForm::default());
        App::spawn_machine_add(&mut m, vec!["sh".into(), "-c".into(), "sleep 1".into()]).unwrap();
        let (expected_cols, expected_rows) = crate::components::remotes::machine_add_cols_rows();
        let (cols, rows) = m.terminals.get(crate::components::remotes::MACHINE_ADD_SESSION_ID)
            .unwrap().grid_size();
        assert_eq!((cols, rows), (expected_cols, expected_rows));
    }

    /// End-to-end proof that a real keystroke, typed while the embedded
    /// `machine add` terminal is running, actually reaches the PTY — not
    /// just that `state.view`/`state.remotes.add` look right (state alone
    /// can't distinguish "forwarded" from "silently dropped"). `cat`'s PTY
    /// line discipline echoes input back immediately, so no shell prompt or
    /// command submission is needed to observe it.
    #[tokio::test]
    async fn raw_key_reaches_the_machine_add_pty_while_running() {
        let e = test_engine();
        let mut m = base(e);
        let mut rx = m.engine.subscribe();
        m.view = View::Remotes;
        m.remotes.add = Some(crate::components::remotes::AddRemoteForm::default());
        App::spawn_machine_add(&mut m, vec!["cat".into()]).unwrap();

        let (m, _) = m.update(Message::RawKey {
            key:       iced::keyboard::Key::Character("z".into()),
            modifiers: iced::keyboard::Modifiers::default(),
            text:      Some("z".into()),
        });

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut seen = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() { break; }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(ninox_core::events::Event::ClientOutput { session_id, bytes, .. }))
                    if session_id == crate::components::remotes::MACHINE_ADD_SESSION_ID =>
                {
                    seen.extend(bytes);
                    if seen.contains(&b'z') { break; }
                }
                Ok(Ok(_)) => {}
                _ => break,
            }
        }
        assert!(
            seen.contains(&b'z'),
            "keystroke must reach the machine-add PTY, got {:?}", String::from_utf8_lossy(&seen)
        );
        drop(m); // keep the client (and its PTY) alive until the read above completes
    }

    #[tokio::test]
    async fn resize_terminals_never_touches_the_machine_add_pty() {
        let e = test_engine();
        let mut m = base(e);
        m.remotes.add = Some(crate::components::remotes::AddRemoteForm::default());
        App::spawn_machine_add(&mut m, vec!["sh".into(), "-c".into(), "sleep 1".into()]).unwrap();
        let (before_cols, before_rows) = m.terminals.get(crate::components::remotes::MACHINE_ADD_SESSION_ID)
            .unwrap().grid_size();

        m.window_width = 1600.0;
        m.window_height = 900.0;
        App::resize_terminals(&mut m);

        let (after_cols, after_rows) = m.terminals.get(crate::components::remotes::MACHINE_ADD_SESSION_ID)
            .unwrap().grid_size();
        assert_eq!((before_cols, before_rows), (after_cols, after_rows));
    }

    #[tokio::test]
    async fn spawn_machine_add_inserts_client_and_terminal() {
        let e = test_engine();
        let mut m = base(e);
        m.remotes.add = Some(crate::components::remotes::AddRemoteForm::default());
        let argv = vec!["sh".to_string(), "-c".to_string(), "sleep 1".to_string()];
        let result = App::spawn_machine_add(&mut m, argv);
        assert!(result.is_ok());
        assert!(m.clients.contains_key(crate::components::remotes::MACHINE_ADD_SESSION_ID));
        assert!(m.terminals.contains_key(crate::components::remotes::MACHINE_ADD_SESSION_ID));
        assert!(m.remotes.add.unwrap().running);
    }

    #[tokio::test]
    async fn machine_add_exit_freezes_the_pane_with_a_success_result() {
        let dir = tempdir().unwrap();
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(dir.path().join("config.toml"));
            let mut m = base(test_engine());
            m.remotes.add = Some(crate::components::remotes::AddRemoteForm {
                host: "a@host".into(),
                ..Default::default()
            });
            ninox_core::config::AppConfig::update(|c| {
                c.remote_machines.machines.push(ninox_core::config::MachineProfile {
                    id: "a".into(), label: "a".into(), ssh_target: "a@host".into(),
                    remote_session: "default".into(), enabled: true,
                });
                Some(())
            }).unwrap();
            App::spawn_machine_add(&mut m, vec!["sh".into(), "-c".into(), "exit 0".into()]).unwrap();
            let generation = m.clients.get(crate::components::remotes::MACHINE_ADD_SESSION_ID).unwrap().generation;

            let (m, _) = m.update(Message::EngineEvent(Box::new(
                ninox_core::events::Event::ClientClosed {
                    session_id: crate::components::remotes::MACHINE_ADD_SESSION_ID.into(),
                    generation,
                },
            )));

            // The modal stays open on its frozen terminal + result (TUI
            // parity: `machine_add_suspended` keeps its output on screen
            // too) — only an explicit `RemoteAddClose`/Esc clears `add`.
            assert!(matches!(m.remotes.add.as_ref().unwrap().finished, Some(Ok(_))));
            assert!(!m.clients.contains_key(crate::components::remotes::MACHINE_ADD_SESSION_ID));
            assert!(m.terminals.contains_key(crate::components::remotes::MACHINE_ADD_SESSION_ID));
            assert!(matches!(m.remotes.notice, Some(Ok(_))));
            assert_eq!(m.remotes.rows.len(), 1);

            let (m, _) = m.update(Message::RemoteAddClose);
            assert!(m.remotes.add.is_none());
            assert!(!m.terminals.contains_key(crate::components::remotes::MACHINE_ADD_SESSION_ID));
        }
    }

    #[tokio::test]
    async fn machine_add_exit_without_a_matching_profile_freezes_with_a_failure_result() {
        let dir = tempdir().unwrap();
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(dir.path().join("config.toml"));
            let mut m = base(test_engine());
            m.remotes.add = Some(crate::components::remotes::AddRemoteForm {
                host: "a@host".into(),
                ..Default::default()
            });
            App::spawn_machine_add(&mut m, vec!["sh".into(), "-c".into(), "exit 1".into()]).unwrap();
            let generation = m.clients.get(crate::components::remotes::MACHINE_ADD_SESSION_ID).unwrap().generation;

            let (m, _) = m.update(Message::EngineEvent(Box::new(
                ninox_core::events::Event::ClientClosed {
                    session_id: crate::components::remotes::MACHINE_ADD_SESSION_ID.into(),
                    generation,
                },
            )));

            assert!(matches!(m.remotes.add.as_ref().unwrap().finished, Some(Err(_))));
            assert!(m.terminals.contains_key(crate::components::remotes::MACHINE_ADD_SESSION_ID));
            assert!(matches!(m.remotes.notice, Some(Err(_))));
        }
    }

    #[tokio::test]
    async fn navigating_away_and_back_resolves_an_orphaned_add() {
        let dir = tempdir().unwrap();
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(dir.path().join("config.toml"));
            let mut m = base(test_engine());
            m.remotes.add = Some(crate::components::remotes::AddRemoteForm {
                host: "a@host".into(),
                ..Default::default()
            });
            App::spawn_machine_add(&mut m, vec!["sh".into(), "-c".into(), "sleep 5".into()]).unwrap();

            // Simulate what every other Navigate* handler's existing
            // `state.clients.clear()` already does when the user switches
            // away from Remotes mid-add: the client (and its process, via
            // Drop) is gone, but `state.terminals` (never cleared by
            // `clients.clear()`) and `state.remotes.add` are untouched
            // until something resolves it.
            m.clients.remove(crate::components::remotes::MACHINE_ADD_SESSION_ID);

            let (m, _) = m.update(Message::NavigateRemotes);

            assert!(matches!(m.remotes.add.as_ref().unwrap().finished, Some(Err(_))));
            assert!(m.terminals.contains_key(crate::components::remotes::MACHINE_ADD_SESSION_ID));
            assert!(matches!(m.remotes.notice, Some(Err(_))));
        }
    }

    // ── Add-a-catalogue modal ────────────────────────────────────────────

    #[test]
    fn catalogue_modal_open_sets_a_blank_form() {
        let m = base(test_engine());
        let (m, _) = m.update(Message::CatalogueModalOpen);
        let form = m.catalogue_modal.as_ref().expect("modal opens");
        assert!(form.name.is_empty());
        assert!(form.path.is_empty());
        assert!(form.error.is_none());
    }

    #[test]
    fn catalogue_form_field_edits_update_state_and_clear_error() {
        let m = base(test_engine());
        let (mut m, _) = m.update(Message::CatalogueModalOpen);
        m.catalogue_modal.as_mut().unwrap().error = Some("stale".to_string());
        let (m, _) = m.update(Message::CatalogueFormName("ninox-dev".into()));
        assert_eq!(m.catalogue_modal.as_ref().unwrap().name, "ninox-dev");
        assert!(m.catalogue_modal.as_ref().unwrap().error.is_none());

        let mut m = m;
        m.catalogue_modal.as_mut().unwrap().error = Some("stale again".to_string());
        let (m, _) = m.update(Message::CatalogueFormPath("~/brains/dev".into()));
        assert_eq!(m.catalogue_modal.as_ref().unwrap().path, "~/brains/dev");
        assert!(m.catalogue_modal.as_ref().unwrap().error.is_none());
    }

    #[test]
    fn brain_list_reading_pane_does_not_panic_with_nothing_selected() {
        // Regression: the "Nothing selected" placeholder used
        // `center_y(Length::Fill)`, which made it a `Length::Fill`-height
        // child of the reading pane's `scrollable` — iced's own
        // `Scrollable::validate()` panics on exactly that ("scrollable
        // content must not fill its vertical scrolling axis"), so simply
        // opening Brain/List with nothing selected (e.g. an empty brain)
        // crashed the whole app. `brain_view.selected` is `None` by
        // default, so just rendering the view is the regression check.
        let mut m = base(test_engine());
        m.view = View::Brain;
        m.brain_view.mode = BrainMode::List;
        let _ = crate::components::brain_panel::brain_panel(&m);
    }

    #[test]
    fn catalogue_form_cancel_closes_modal() {
        let m = base(test_engine());
        let (m, _) = m.update(Message::CatalogueModalOpen);
        assert!(m.catalogue_modal.is_some());
        let (m, _) = m.update(Message::CatalogueFormCancel);
        assert!(m.catalogue_modal.is_none());
    }

    #[test]
    fn esc_closes_catalogue_modal() {
        let m = base(test_engine());
        let (m, _) = m.update(Message::CatalogueModalOpen);
        assert!(m.catalogue_modal.is_some());
        let (m, _) = m.update(Message::RawKey {
            key:       iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape),
            modifiers: iced::keyboard::Modifiers::default(),
            text:      None,
        });
        assert!(m.catalogue_modal.is_none());
    }

    fn live_session(id: &str) -> Session {
        Session { status: SessionStatus::Working, ..refile_session(id) }
    }

    #[test]
    fn request_restart_all_noops_without_live_agents() {
        let m = base(test_engine());
        let (m, _) = m.update(Message::RequestRestartAll);
        assert!(!m.restart_all_confirm, "nothing live — no point confirming");
    }

    fn server_info(version: &str, issues: &[&str]) -> ninox_core::server_control::ServerInfo {
        ninox_core::server_control::ServerInfo {
            version: version.into(),
            build: ninox_core::server_control::BUILD_HASH.into(),
            pid: 4242,
            port: 45678,
            db_path: "/scratch/t.db".into(),
            in_process: false,
            issues: issues.iter().map(|m| ninox_core::server_control::ServiceIssue {
                source: "update_check".into(), message: (*m).into(), sessions: Vec::new(),
            }).collect(),
        }
    }

    #[test]
    fn restart_server_confirms_inline_before_running() {
        let mut m = base(test_engine());
        m.server.port = 45678;
        let (m, _) = m.update(Message::RequestRestartServer);
        assert!(m.server.confirming && !m.server.restarting);
        let (m, _) = m.update(Message::CancelRestartServer);
        assert!(!m.server.confirming && !m.server.restarting, "cancel runs nothing");
        let (m, _) = m.update(Message::RequestRestartServer);
        let (m, _) = m.update(Message::ConfirmRestartServer);
        assert!(m.server.restarting && !m.server.confirming);
        assert!(crate::components::config_banner::server_banner_shown(&m), "non-blocking restarting banner");
        let (m, _) = m.update(Message::RequestRestartServer);
        assert!(!m.server.confirming, "no second confirm while one runs");
        let (m, _) = m.update(Message::ServerRestartDone(Ok("server restarted: a → b".into())));
        assert!(!m.server.restarting);
        assert_eq!(m.server.result, Some(Ok("server restarted: a → b".into())));
    }

    #[test]
    fn server_info_with_an_expired_login_raises_an_alert() {
        let m = base(test_engine());
        let info = server_info(NINOX_VERSION, &["update check failed: credential-provider exited with exit status: 254"]);
        let (m, _) = m.update(Message::ServerInfoLoaded(Some(info)));
        assert_eq!(m.server.alerts.len(), 1);
        assert!(m.server.alerts[0].line().contains("aws sso login"));
        assert!(crate::components::config_banner::server_banner_shown(&m));
        let (color, tip) = crate::components::shell::nav::server_dot_state(&m.server);
        assert_eq!(color, crate::components::shell::tokens::AMBER);
        assert!(tip.contains("aws sso login"), "{tip}");

        let (m, _) = m.update(Message::ServerInfoLoaded(Some(server_info(NINOX_VERSION, &[]))));
        assert!(m.server.alerts.is_empty() && !crate::components::config_banner::server_banner_shown(&m));
        assert_eq!(crate::components::shell::nav::server_dot_state(&m.server).0, crate::components::shell::tokens::GREEN);
    }

    #[test]
    fn a_harness_login_failure_offers_restarting_the_affected_sessions() {
        let m = base(test_engine());
        let mut info = server_info(NINOX_VERSION, &[]);
        info.issues.push(ninox_core::server_control::ServiceIssue {
            source: "harness_auth:claude-code".into(),
            message: "Invalid API key · Please run /login".into(),
            sessions: vec!["w1".into()],
        });
        let (m, _) = m.update(Message::ServerInfoLoaded(Some(info)));
        assert_eq!(m.server.alerts[0].line(), "Claude login expired: log in again (`claude /login`), then Restart server");
        assert_eq!(m.server.alerts[0].sessions, ["w1"]);
        let (m, _) = m.update(Message::RestartAffectedSessions(vec!["w1".into()]));
        assert!(m.server.restarting_sessions);
        let (m, _) = m.update(Message::RestartAffectedSessions(vec!["w1".into()]));
        assert!(m.server.restarting_sessions, "no second batch while one runs");
        let (m, _) = m.update(Message::AffectedSessionsRestarted(Ok("restarted w1".into())));
        assert!(!m.server.restarting_sessions);
    }

    #[test]
    fn an_older_server_build_offers_a_restart_and_no_server_is_red() {
        let m = base(test_engine());
        let (m, _) = m.update(Message::ServerInfoLoaded(Some(server_info("0.0.1", &[]))));
        assert!(m.server.alerts[0].title.contains("0.0.1"));
        let (m, _) = m.update(Message::ServerInfoLoaded(None));
        assert_eq!(crate::components::shell::nav::server_dot_state(&m.server).0, crate::components::shell::tokens::RED);
    }

    #[test]
    fn request_restart_all_opens_the_confirm_modal_with_a_live_agent() {
        let mut m = base(test_engine());
        m.sessions.insert("s1".into(), live_session("s1"));
        let (m, _) = m.update(Message::RequestRestartAll);
        assert!(m.restart_all_confirm);
    }

    #[test]
    fn cancel_and_esc_both_close_the_restart_all_modal() {
        let mut m = base(test_engine());
        m.sessions.insert("s1".into(), live_session("s1"));

        let (m, _) = m.update(Message::RequestRestartAll);
        assert!(m.restart_all_confirm);
        let (m, _) = m.update(Message::CancelRestartAll);
        assert!(!m.restart_all_confirm);

        let (m, _) = m.update(Message::RequestRestartAll);
        let (m, _) = m.update(Message::RawKey {
            key:       iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape),
            modifiers: iced::keyboard::Modifiers::default(),
            text:      None,
        });
        assert!(!m.restart_all_confirm);
    }

    #[test]
    fn confirm_restart_all_closes_the_modal_and_marks_in_progress() {
        let mut m = base(test_engine());
        m.sessions.insert("s1".into(), live_session("s1"));
        let (m, _) = m.update(Message::RequestRestartAll);
        let (m, _) = m.update(Message::ConfirmRestartAll);
        assert!(!m.restart_all_confirm);
        assert!(m.restart_all_in_progress);

        // A second confirm while one is already running must not kick off
        // a duplicate batch — same guard `ApplyUpdate` uses.
        let (m, _) = m.update(Message::ConfirmRestartAll);
        assert!(m.restart_all_in_progress);
    }

    #[test]
    fn restart_all_applied_pushes_a_completed_notification_on_a_clean_sweep() {
        let m = base(test_engine());
        let (m, _) = m.update(Message::ConfirmRestartAll);
        let outcomes = vec![ninox_ui_shared::restart::RestartOutcome {
            session_id: "w1".into(),
            result: ninox_ui_shared::restart::RestartResult::Restarted(ninox_ui_shared::restart::RestartedVia::Resumed),
        }];
        let (m, _) = m.update(Message::RestartAllApplied(Ok(outcomes)));
        assert!(!m.restart_all_in_progress);
        let n = m.notifications.front().expect("a notification was pushed");
        assert_eq!(n.kind, NotificationKind::RestartAllCompleted);
        assert_eq!(n.body, "restarted 1 agent");
    }

    #[test]
    fn restart_all_applied_pushes_a_failed_notification_on_partial_failure() {
        let m = base(test_engine());
        let (m, _) = m.update(Message::ConfirmRestartAll);
        let outcomes = vec![
            ninox_ui_shared::restart::RestartOutcome {
                session_id: "w1".into(),
                result: ninox_ui_shared::restart::RestartResult::Restarted(ninox_ui_shared::restart::RestartedVia::Resumed),
            },
            ninox_ui_shared::restart::RestartOutcome {
                session_id: "w2".into(),
                result: ninox_ui_shared::restart::RestartResult::Failed("launch failed".into()),
            },
        ];
        let (m, _) = m.update(Message::RestartAllApplied(Ok(outcomes)));
        let n = m.notifications.front().expect("a notification was pushed");
        assert_eq!(n.kind, NotificationKind::RestartAllFailed);
        assert_eq!(n.body, "restarted 1/2 agents; failed: w2");
    }

    #[test]
    fn restart_all_applied_pushes_a_failed_notification_on_error() {
        let m = base(test_engine());
        let (m, _) = m.update(Message::ConfirmRestartAll);
        let (m, _) = m.update(Message::RestartAllApplied(Err("db unavailable".into())));
        assert!(!m.restart_all_in_progress);
        let n = m.notifications.front().expect("a notification was pushed");
        assert_eq!(n.kind, NotificationKind::RestartAllFailed);
        assert_eq!(n.body, "db unavailable");
    }

    #[test]
    fn confirm_guard_empty_name_keeps_modal_open_and_config_untouched() {
        let m = base(test_engine());
        let (m, _) = m.update(Message::CatalogueModalOpen);
        let before = m.config.brain.catalogues.clone();
        let (m, _) = m.update(Message::CatalogueFormConfirm);
        let form = m.catalogue_modal.as_ref().expect("modal stays open");
        assert!(form.error.as_deref().unwrap().contains("name"), "{:?}", form.error);
        assert_eq!(m.config.brain.catalogues, before);
    }

    #[test]
    fn confirm_guard_duplicate_name_keeps_modal_open_and_config_untouched() {
        let _iso = ninox_core::test_isolation::isolate();
        let m = base(test_engine());
        let (m, _) = m.update(Message::CatalogueModalOpen);
        // "default" is always present via `catalogue_options()`.
        let (m, _) = m.update(Message::CatalogueFormName("default".into()));
        let (m, _) = m.update(Message::CatalogueFormPath("/tmp/whatever-not-touched".into()));
        let before = m.config.brain.catalogues.clone();
        let (m, _) = m.update(Message::CatalogueFormConfirm);
        let form = m.catalogue_modal.as_ref().expect("modal stays open");
        assert!(form.error.as_deref().unwrap().contains("already exists"), "{:?}", form.error);
        assert_eq!(m.config.brain.catalogues, before);
    }

    #[test]
    fn confirm_guard_empty_path_keeps_modal_open_and_config_untouched() {
        let _iso = ninox_core::test_isolation::isolate();
        let m = base(test_engine());
        let (m, _) = m.update(Message::CatalogueModalOpen);
        let (m, _) = m.update(Message::CatalogueFormName("ninox-dev".into()));
        let before = m.config.brain.catalogues.clone();
        let (m, _) = m.update(Message::CatalogueFormConfirm);
        let form = m.catalogue_modal.as_ref().expect("modal stays open");
        assert!(form.error.as_deref().unwrap().contains("path"), "{:?}", form.error);
        assert_eq!(m.config.brain.catalogues, before);
    }

    #[test]
    fn confirm_guard_path_exists_but_isnt_a_directory_keeps_modal_open_and_config_untouched() {
        let _iso = ninox_core::test_isolation::isolate();
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("not-a-dir");
        std::fs::write(&file_path, b"nope").unwrap();

        let m = base(test_engine());
        let (m, _) = m.update(Message::CatalogueModalOpen);
        let (m, _) = m.update(Message::CatalogueFormName("ninox-dev".into()));
        let (m, _) = m.update(Message::CatalogueFormPath(file_path.to_string_lossy().to_string()));
        let before = m.config.brain.catalogues.clone();
        let (m, _) = m.update(Message::CatalogueFormConfirm);
        let form = m.catalogue_modal.as_ref().expect("modal stays open");
        assert!(form.error.as_deref().unwrap().contains("directory"), "{:?}", form.error);
        assert_eq!(m.config.brain.catalogues, before);
    }

    #[test]
    fn confirm_happy_path_files_a_new_catalogue_and_switches_to_it() {
        // `CatalogueFormConfirm` calls `state.config.save()`, which writes
        // to `AppConfig::config_path()`. Redirect that to a tempfile so
        // this test never touches the real user config (see
        // `t_toggles_light_dark` for the same pattern).
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("catalogue_happy_path_config.toml");
        let catalogue_dir = dir.path().join("ninox-dev-brain");

        {
            let _iso = ninox_core::test_isolation::isolate_with_config(&config_path);
            let m = base(test_engine());
            let (m, _) = m.update(Message::CatalogueModalOpen);
            let (m, _) = m.update(Message::CatalogueFormName("ninox-dev".into()));
            let (m, _) = m.update(Message::CatalogueFormPath(
                catalogue_dir.to_string_lossy().to_string(),
            ));
            let (m, _) = m.update(Message::CatalogueFormConfirm);

            assert!(m.catalogue_modal.is_none(), "modal closes on success");
            assert!(
                m.config
                    .brain
                    .catalogues
                    .iter()
                    .any(|c| c.name == "ninox-dev" && c.path == catalogue_dir),
                "config gains the new catalogue entry: {:?}",
                m.config.brain.catalogues
            );
            assert!(
                m.catalogues.iter().any(|c| c.name == "ninox-dev"),
                "state.catalogues refreshed from config.catalogue_options()"
            );
            let idx = m.catalogues.iter().position(|c| c.name == "ninox-dev").unwrap();
            assert_eq!(m.active_catalogue, Some(idx), "active catalogue switches to the new one");
            assert!(
                catalogue_dir.join(".index.db").exists(),
                "brain index initialized on disk"
            );

            let saved = std::fs::read_to_string(&config_path).unwrap();
            assert!(saved.contains("ninox-dev"), "catalogue persisted to config.toml");
        }
    }

    #[test]
    fn spawn_form_field_messages_update_state() {
        use crate::components::spawn_modal::SpawnKind;

        let mut m = base(test_engine());
        let (next, _) = m.update(Message::SpawnSession);
        m = next;
        let (next, _) = m.update(Message::SpawnFormName("theme-tokens".into()));
        m = next;
        let (next, _) = m.update(Message::SpawnFormKind(SpawnKind::Standalone));
        m = next;
        let (next, _) = m.update(Message::SpawnFormWorkspace("~/proj/ninox".into()));
        m = next;
        let (next, _) = m.update(Message::SpawnFormHarness("codex".into()));
        m = next;
        let (next, _) = m.update(Message::SpawnFormModel("gpt-4o".into()));
        m = next;
        let (next, _) = m.update(Message::SpawnFormToggleBrain("default".into()));
        m = next;

        let f = m.spawn_modal.as_ref().unwrap();
        assert_eq!(f.name, "theme-tokens");
        assert_eq!(f.kind, SpawnKind::Standalone);
        assert_eq!(f.workspace, "~/proj/ninox");
        assert_eq!(f.harness, "codex");
        assert_eq!(f.model.as_deref(), Some("gpt-4o"));
        assert_eq!(f.brain_reads, vec!["default"], "the last brain can't be deselected");
        assert_eq!(f.brain_write, "default");
    }

    #[test]
    fn spawn_form_harness_switch_resets_model_and_custom_opens_input() {
        let mut m = base(test_engine());
        let (next, _) = m.update(Message::SpawnSession);
        m = next;
        let (next, _) = m.update(Message::SpawnFormModel("claude-haiku-4-5".into()));
        m = next;
        assert_eq!(m.spawn_modal.as_ref().unwrap().model.as_deref(), Some("claude-haiku-4-5"));

        // custom… flips the picker into free-text mode without clobbering
        // the last picked model (blank custom falls back to it on confirm).
        let (next, _) = m.update(Message::SpawnFormModel(ninox_ui_shared::models::CUSTOM_SENTINEL.into()));
        m = next;
        assert!(m.spawn_modal.as_ref().unwrap().custom_model.is_some());
        let (next, _) = m.update(Message::SpawnFormCustomModel("my-model".into()));
        m = next;
        assert_eq!(m.spawn_modal.as_ref().unwrap().custom_model.as_deref(), Some("my-model"));

        // Switching harness clears model state — stale model ids from one
        // harness must not leak into another's launch command.
        let (next, _) = m.update(Message::SpawnFormHarness("codex".into()));
        m = next;
        let f = m.spawn_modal.as_ref().unwrap();
        assert_eq!(f.harness, "codex");
        assert!(f.model.is_none());
        assert!(f.custom_model.is_none());
    }

    #[test]
    fn orchestrator_confirm_unaffected_by_workspace_field() {
        let _iso = ninox_core::test_isolation::isolate();
        use crate::components::spawn_modal::SpawnKind;

        let mut m = base(test_engine());
        let (next, _) = m.update(Message::SpawnSession);
        m = next;
        let (next, _) = m.update(Message::SpawnFormName("theme-tokens".into()));
        m = next;
        // Type a workspace while on Standalone, then flip back to
        // Orchestrator — the stale workspace text must not block or alter
        // the orchestrator spawn (its workspace always derives from the
        // orchestrator root).
        let (next, _) = m.update(Message::SpawnFormKind(SpawnKind::Standalone));
        m = next;
        let (next, _) = m.update(Message::SpawnFormWorkspace("/does/not/matter".into()));
        m = next;
        let (next, _) = m.update(Message::SpawnFormKind(SpawnKind::Orchestrator));
        m = next;
        let (next, _) = m.update(Message::SpawnFormConfirm);
        m = next;

        assert!(m.spawn_modal.is_none());
        assert_eq!(m.orchestrators.len(), 1);
        let sess = m.sessions.get("theme-tokens").expect("orchestrator session created");
        assert_eq!(
            sess.workspace_path.as_deref(),
            Some(std::path::Path::new("/tmp/theme-tokens").to_str().unwrap()),
            "workspace comes from the orchestrator root, not the form field",
        );
    }

    /// G4 (MLOPS-5416's Type field): the chosen worker type is recorded on
    /// the spawned Session, for both kinds — previously always hardcoded
    /// `None`.
    #[test]
    fn spawn_confirm_records_the_chosen_worker_type() {
        let _iso = ninox_core::test_isolation::isolate();
        let mut m = base(test_engine());
        let (next, _) = m.update(Message::SpawnSession);
        m = next;
        assert_eq!(m.spawn_modal.as_ref().unwrap().worker_type, "Implementor", "default worker type");
        let (next, _) = m.update(Message::SpawnFormName("theme-tokens".into()));
        m = next;
        let (next, _) = m.update(Message::SpawnFormConfirm);
        m = next;

        // Round-2 review on PR #282: an orchestrator's type is
        // "Orchestrator" by definition (the Type field isn't even offered
        // for this kind — see spawn_modal's conditional rendering), never
        // whatever the form's own (inapplicable) worker_type field holds.
        let sess = m.sessions.get("theme-tokens").expect("orchestrator session created");
        assert_eq!(sess.worker_type.as_deref(), Some("Orchestrator"));
    }

    /// A Standalone spawn's chosen Type *is* recorded — only the
    /// Orchestrator kind has an implicit, unpickable type.
    #[test]
    fn standalone_spawn_confirm_records_the_chosen_worker_type() {
        let _iso = ninox_core::test_isolation::isolate();
        use crate::components::spawn_modal::SpawnKind;

        // An already-existing, non-git directory — the confirm path only
        // special-cases a *missing* workspace (auto-creating a worktree
        // when it's nested under a git repo); an existing one proceeds
        // directly, so the session is inserted synchronously and this test
        // needs no git fixture.
        let workspace = tempdir().unwrap().keep();

        let mut m = base(test_engine());
        let (next, _) = m.update(Message::SpawnSession);
        m = next;
        let (next, _) = m.update(Message::SpawnFormKind(SpawnKind::Standalone));
        m = next;
        let (next, _) = m.update(Message::SpawnFormName("theme-tokens".into()));
        m = next;
        let (next, _) = m.update(Message::SpawnFormWorkspace(workspace.to_string_lossy().to_string()));
        m = next;
        let (next, _) = m.update(Message::SpawnFormWorkerType("Researcher".into()));
        m = next;
        let (next, _) = m.update(Message::SpawnFormConfirm);
        m = next;

        let sess = m.sessions.get("theme-tokens").expect("standalone session created");
        assert_eq!(sess.worker_type.as_deref(), Some("Researcher"));
    }

    #[test]
    fn standalone_spawn_records_the_types_success_criteria() {
        let _iso = ninox_core::test_isolation::isolate();
        use crate::components::spawn_modal::SpawnKind;
        let workspace = tempdir().unwrap().keep();
        let m = base(test_engine());
        let expected = ninox_ui_shared::cli::standalone_initial_prompt(&m.config, "x", "Implementor", String::new()).unwrap().0;

        let (m, _) = m.update(Message::SpawnSession);
        let (m, _) = m.update(Message::SpawnFormKind(SpawnKind::Standalone));
        let (m, _) = m.update(Message::SpawnFormName("theme-tokens".into()));
        let (m, _) = m.update(Message::SpawnFormWorkspace(workspace.to_string_lossy().to_string()));
        let (m, _) = m.update(Message::SpawnFormWorkerType("Implementor".into()));
        let (m, _) = m.update(Message::SpawnFormConfirm);

        assert_eq!(m.sessions["theme-tokens"].success_criteria, expected, "same criteria the TUI's spawn records");
    }

    #[test]
    fn the_initial_prompt_is_multi_line_and_resets_on_reopen() {
        use iced::widget::text_editor::{Action, Edit};
        let m = base(test_engine());
        let (mut m, _) = m.update(Message::SpawnSession);
        for action in [Action::Edit(Edit::Insert('a')), Action::Edit(Edit::Enter), Action::Edit(Edit::Insert('b'))] {
            m = m.update(Message::SpawnFormPrompt(action)).0;
        }
        assert_eq!(m.spawn_prompt.text().trim_end(), "a\nb");
        let (m, _) = m.update(Message::SpawnFormCancel);
        let (m, _) = m.update(Message::SpawnSession);
        assert_eq!(m.spawn_prompt.text().trim(), "");
    }

    #[test]
    fn a_failed_initial_prompt_raises_a_notification() {
        let m = base(test_engine());
        let (m, _) = m.update(Message::SpawnPromptDelivered { session_id: "solo".into(), result: Ok(()) });
        assert!(m.notifications.is_empty());
        let (m, _) = m.update(Message::SpawnPromptDelivered { session_id: "solo".into(), result: Err("socket gone".into()) });
        let n = m.notifications.back().expect("a notification");
        assert_eq!(n.kind, NotificationKind::InitialPromptFailed);
        assert_eq!((n.session_id.as_deref(), n.body.as_str()), (Some("solo"), "socket gone"));
    }

    #[test]
    fn the_thinking_clock_runs_only_while_the_placeholder_is_on_screen() {
        let mut m = base(test_engine());
        assert!(!App::wants_thinking_tick(&m), "idle: no wakeups");
        m.ninox_reply_drawer.ask("q".into(), "ninox".into(), 0);
        assert!(App::wants_thinking_tick(&m), "pending");
        m.ninox_reply_drawer.latest_mut().unwrap().state = crate::prompt_agent::AnswerState::Streaming;
        assert!(App::wants_thinking_tick(&m), "streaming, no text yet");
        m.reduce_motion = true;
        assert!(!App::wants_thinking_tick(&m), "reduce motion holds still");
        m.reduce_motion = false;
        m.ninox_reply_drawer.latest_mut().unwrap().answer = "words".into();
        assert!(!App::wants_thinking_tick(&m), "text is streaming in");
        m.ninox_reply_drawer.latest_mut().unwrap().answer.clear();
        m.ninox_reply_drawer.collapse();
        assert!(!App::wants_thinking_tick(&m), "collapsed drawer");

        let (m, _) = m.update(Message::ThinkingTick);
        let (m, _) = m.update(Message::ThinkingTick);
        assert_eq!(m.thinking_tick, 2);
    }

    #[test]
    fn a_launch_that_failed_never_attaches() {
        let m = base(test_engine());
        let (m, _) = m.update(Message::SpawnLaunched {
            session_id: "solo".into(), argv: Err("ninox-ptyd failed to start: bind /x/ptyd.sock: path must be shorter than SUN_LEN".into()),
            first_message: Some("hi".into()), spawner: None,
        });
        assert!(!m.clients.contains_key("solo"));
        let n = m.notifications.back().expect("a failed launch is surfaced, not silent");
        assert_eq!(n.kind, NotificationKind::LaunchFailed);
        assert_eq!(n.session_id.as_deref(), Some("solo"));
        assert!(n.body.ends_with("path must be shorter than SUN_LEN"), "{}", n.body);
    }

    /// The Type picker's options must come from `user_facing_names()`, not
    /// `names()` — a hidden type (the prompt agent) must never be
    /// selectable from the Spawn dialog.
    #[test]
    fn spawn_dialog_worker_types_never_include_a_hidden_type() {
        let m = base(test_engine());
        let names = m.config.worker_type_registry().user_facing_names();
        assert!(
            !names.iter().any(|n| n == ninox_core::worker_type::PROMPT_WORKER_TYPE),
            "the prompt agent's worker type must never be user-selectable",
        );
    }

    /// `⌘⏎` submits the spawn dialog from anywhere, not just via the Name
    /// field's own plain-Enter submit (G4 — previously not wired at all).
    #[test]
    fn cmd_enter_submits_the_spawn_dialog() {
        let _iso = ninox_core::test_isolation::isolate();
        let mut m = base(test_engine());
        let (next, _) = m.update(Message::SpawnSession);
        m = next;
        let (next, _) = m.update(Message::SpawnFormName("theme-tokens".into()));
        m = next;

        let (m, _) = m.update(Message::RawKey {
            key:       iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter),
            modifiers: iced::keyboard::Modifiers::COMMAND,
            text:      None,
        });

        assert!(m.spawn_modal.is_none(), "⌘⏎ must confirm and close the dialog");
        assert!(m.sessions.contains_key("theme-tokens"));
    }

    #[test]
    fn standalone_without_workspace_cannot_confirm() {
        let _iso = ninox_core::test_isolation::isolate();
        use crate::components::spawn_modal::SpawnKind;

        let mut m = base(test_engine());
        let (next, _) = m.update(Message::SpawnSession);
        m = next;
        let (next, _) = m.update(Message::SpawnFormKind(SpawnKind::Standalone));
        m = next;
        let (next, _) = m.update(Message::SpawnFormName("solo-a".into()));
        m = next;
        let (next, _) = m.update(Message::SpawnFormConfirm);
        m = next;

        // No workspace supplied — confirm must no-op and leave the modal
        // open so the user can fill it in, with an inline error explaining why.
        assert!(m.sessions.is_empty());
        let form = m.spawn_modal.as_ref().expect("modal stays open");
        let err = form.error.as_deref().expect("guard refusal must surface an error");
        assert!(err.contains("workspace is required"), "unexpected error message: {err}");
    }

    #[test]
    fn standalone_confirm_creates_unattached_session_without_orchestrator() {
        let _iso = ninox_core::test_isolation::isolate();
        use crate::components::spawn_modal::SpawnKind;

        // Confirm-time validation requires the workspace path to exist.
        let ws = tempdir().unwrap().keep();
        let ws_str = ws.to_string_lossy().to_string();

        let mut m = base(test_engine());
        let (next, _) = m.update(Message::SpawnSession);
        m = next;
        let (next, _) = m.update(Message::SpawnFormKind(SpawnKind::Standalone));
        m = next;
        let (next, _) = m.update(Message::SpawnFormName("solo-a".into()));
        m = next;
        let (next, _) = m.update(Message::SpawnFormWorkspace(ws_str.clone()));
        m = next;
        let (next, _) = m.update(Message::SpawnFormConfirm);
        m = next;

        assert!(m.spawn_modal.is_none());
        assert!(m.orchestrators.is_empty(), "standalone spawn must not create an orchestrator");
        let sess = m.sessions.get("solo-a").expect("standalone session created");
        assert!(sess.orchestrator_id.is_none());
        assert_eq!(sess.workspace_path.as_deref(), Some(ws_str.as_str()));
        assert!(matches!(&m.view, View::SessionDetail { session_id, .. } if session_id == "solo-a"));
    }

    #[test]
    fn standalone_confirm_with_nonexistent_workspace_keeps_modal_open() {
        let _iso = ninox_core::test_isolation::isolate();
        use crate::components::spawn_modal::SpawnKind;

        let mut m = base(test_engine());
        let (next, _) = m.update(Message::SpawnSession);
        m = next;
        let (next, _) = m.update(Message::SpawnFormKind(SpawnKind::Standalone));
        m = next;
        let (next, _) = m.update(Message::SpawnFormName("solo-bad".into()));
        m = next;
        let (next, _) = m.update(Message::SpawnFormWorkspace(
            "/definitely/not/a/real/workspace/path".into(),
        ));
        m = next;
        let (next, _) = m.update(Message::SpawnFormConfirm);
        m = next;

        // Bad path — confirm must no-op, keep the modal open for correction,
        // and never optimistically create a session.
        assert!(m.sessions.is_empty());
        assert!(m.orchestrators.is_empty());
        assert!(matches!(m.view, View::Fleet));
        let form = m.spawn_modal.as_ref().expect("modal stays open");
        let err = form.error.as_deref().expect("guard refusal must surface an error");
        assert!(err.contains("does not exist"), "unexpected error message: {err}");
    }

    /// Minimal real git repo, mirroring `spawn_util::tests::init_git_repo` —
    /// `create_worktree_at` shells out to real `git`.
    fn init_git_repo() -> std::path::PathBuf {
        let dir = tempdir().unwrap().keep();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", dir.to_str().unwrap()])
                .args(args)
                .output()
                .unwrap()
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        std::fs::write(dir.join("README.md"), "x").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);
        dir
    }

    #[test]
    fn standalone_confirm_with_missing_path_under_git_repo_auto_creates_worktree() {
        let _iso = ninox_core::test_isolation::isolate();
        use crate::components::spawn_modal::SpawnKind;

        let repo = init_git_repo();
        let missing = repo.join(".claude").join("worktrees").join("feat-new-thing");
        let missing_str = missing.to_string_lossy().to_string();
        assert!(!missing.exists(), "sanity: the typed path must not exist yet");

        let mut m = base(test_engine());
        let (next, _) = m.update(Message::SpawnSession);
        m = next;
        let (next, _) = m.update(Message::SpawnFormKind(SpawnKind::Standalone));
        m = next;
        let (next, _) = m.update(Message::SpawnFormName("solo-autocreate".into()));
        m = next;
        let (next, _) = m.update(Message::SpawnFormWorkspace(missing_str.clone()));
        m = next;
        let (next, _) = m.update(Message::SpawnFormConfirm);
        m = next;

        assert!(
            m.spawn_modal.is_none(),
            "auto-created worktree must let spawn proceed, got error: {:?}",
            m.spawn_modal.as_ref().and_then(|f| f.error.clone())
        );
        let sess = m.sessions.get("solo-autocreate").expect("session created against the auto-created worktree");
        assert_eq!(sess.workspace_path.as_deref(), Some(missing_str.as_str()));
        assert!(missing.join(".git").exists(), "the typed path must now be a real git worktree");

        let out = std::process::Command::new("git")
            .args(["-C", &missing_str, "branch", "--show-current"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "feat-new-thing",
            "branch name must be derived from the missing path's final component"
        );
    }

    #[test]
    fn standalone_confirm_with_missing_path_reuses_existing_branch() {
        let _iso = ninox_core::test_isolation::isolate();
        use crate::components::spawn_modal::SpawnKind;

        let repo = init_git_repo();
        let missing = repo.join(".claude").join("worktrees").join("feat-existing");
        let missing_str = missing.to_string_lossy().to_string();

        // Pre-create the branch (and worktree), then remove just the
        // worktree registration so the path is missing again but the
        // branch survives — exercising the "already exists" fallback.
        ninox_ui_shared::spawn_util::create_worktree_at(&repo, &missing, "feat-existing", false, &ninox_core::config::AppConfig::default()).unwrap();
        std::process::Command::new("git")
            .args(["-C", &repo.to_string_lossy(), "worktree", "remove", "--force", &missing_str])
            .output()
            .unwrap();
        assert!(!missing.exists(), "sanity: worktree dir removed but branch remains");

        let mut m = base(test_engine());
        let (next, _) = m.update(Message::SpawnSession);
        m = next;
        let (next, _) = m.update(Message::SpawnFormKind(SpawnKind::Standalone));
        m = next;
        let (next, _) = m.update(Message::SpawnFormName("solo-reuse".into()));
        m = next;
        let (next, _) = m.update(Message::SpawnFormWorkspace(missing_str.clone()));
        m = next;
        let (next, _) = m.update(Message::SpawnFormConfirm);
        m = next;

        assert!(
            m.spawn_modal.is_none(),
            "re-checking out an existing branch must let spawn proceed, got error: {:?}",
            m.spawn_modal.as_ref().and_then(|f| f.error.clone())
        );
        assert!(m.sessions.contains_key("solo-reuse"));
        let out = std::process::Command::new("git")
            .args(["-C", &missing_str, "branch", "--show-current"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "feat-existing");
    }

    #[test]
    fn standalone_confirm_surfaces_the_git_error_when_worktree_creation_fails() {
        let _iso = ninox_core::test_isolation::isolate();
        use crate::components::spawn_modal::SpawnKind;

        // The branch derived from the second missing path's final component
        // ("dup-branch") is already checked out in another worktree in the
        // same repo, so `create_worktree_at`'s fallback (checkout without
        // `-b`) fails too — a real git error create_worktree_at can surface.
        let repo = init_git_repo();
        ninox_ui_shared::spawn_util::create_worktree_at(&repo, &repo.join("first"), "dup-branch", false, &ninox_core::config::AppConfig::default()).unwrap();
        let missing = repo.join(".claude").join("worktrees").join("dup-branch");
        let missing_str = missing.to_string_lossy().to_string();

        let mut m = base(test_engine());
        let (next, _) = m.update(Message::SpawnSession);
        m = next;
        let (next, _) = m.update(Message::SpawnFormKind(SpawnKind::Standalone));
        m = next;
        let (next, _) = m.update(Message::SpawnFormName("solo-conflict".into()));
        m = next;
        let (next, _) = m.update(Message::SpawnFormWorkspace(missing_str.clone()));
        m = next;
        let (next, _) = m.update(Message::SpawnFormConfirm);
        m = next;

        assert!(m.sessions.is_empty(), "a failed auto-create must not spawn a session");
        assert!(!missing.exists(), "a failed auto-create must not leave a half-created worktree dir");
        let form = m.spawn_modal.as_ref().expect("modal stays open on worktree-creation failure");
        let err = form.error.as_deref().expect("guard refusal must surface an error");
        assert!(err.contains("failed to create worktree"), "unexpected error message: {err}");
    }

    #[test]
    fn standalone_confirm_with_duplicate_name_keeps_modal_open_and_original_untouched() {
        let _iso = ninox_core::test_isolation::isolate();
        use crate::components::spawn_modal::SpawnKind;

        // Two separate workspaces so any difference between the original and
        // a would-be overwrite is easy to see.
        let ws1 = tempdir().unwrap().keep();
        let ws1_str = ws1.to_string_lossy().to_string();
        let ws2 = tempdir().unwrap().keep();
        let ws2_str = ws2.to_string_lossy().to_string();

        let mut m = base(test_engine());
        let (next, _) = m.update(Message::SpawnSession);
        m = next;
        let (next, _) = m.update(Message::SpawnFormKind(SpawnKind::Standalone));
        m = next;
        let (next, _) = m.update(Message::SpawnFormName("solo-a".into()));
        m = next;
        let (next, _) = m.update(Message::SpawnFormWorkspace(ws1_str.clone()));
        m = next;
        let (next, _) = m.update(Message::SpawnFormConfirm);
        m = next;
        assert!(m.spawn_modal.is_none(), "first spawn with a unique name must succeed");
        let original = m.sessions.get("solo-a").cloned().expect("original session created");
        assert_eq!(original.workspace_path.as_deref(), Some(ws1_str.as_str()));

        // Spawn again with the exact same name — slugify("solo-a") collides
        // with the existing session id.
        let (next, _) = m.update(Message::SpawnSession);
        m = next;
        let (next, _) = m.update(Message::SpawnFormKind(SpawnKind::Standalone));
        m = next;
        let (next, _) = m.update(Message::SpawnFormName("solo-a".into()));
        m = next;
        let (next, _) = m.update(Message::SpawnFormWorkspace(ws2_str.clone()));
        m = next;
        let (next, _) = m.update(Message::SpawnFormConfirm);
        m = next;

        let form = m.spawn_modal.as_ref().expect(
            "a name colliding with an existing session id must keep the modal open, not overwrite it"
        );
        let err = form.error.as_deref().expect("guard refusal must surface an error");
        assert!(err.contains("solo-a"), "unexpected error message: {err}");
        assert!(err.contains("already exists"), "unexpected error message: {err}");
        assert_eq!(m.sessions.len(), 1, "the colliding spawn must not create/overwrite any session");
        let unchanged = m.sessions.get("solo-a").expect("original session must still exist");
        assert_eq!(
            unchanged.workspace_path.as_deref(),
            Some(ws1_str.as_str()),
            "the original session's record must be untouched by the rejected duplicate spawn"
        );
    }

    #[test]
    fn orchestrator_confirm_with_duplicate_name_keeps_modal_open_and_original_untouched() {
        let _iso = ninox_core::test_isolation::isolate();
        let mut m = base(test_engine());
        let (next, _) = m.update(Message::SpawnSession);
        m = next;
        let (next, _) = m.update(Message::SpawnFormName("orch-a".into()));
        m = next;
        let (next, _) = m.update(Message::SpawnFormConfirm);
        m = next;
        assert!(m.spawn_modal.is_none(), "first orchestrator spawn with a unique name must succeed");
        assert_eq!(m.orchestrators.len(), 1);
        let original = m.sessions.get("orch-a").cloned().expect("original orchestrator session created");

        // Spawn a second orchestrator with the exact same name.
        let (next, _) = m.update(Message::SpawnSession);
        m = next;
        let (next, _) = m.update(Message::SpawnFormName("orch-a".into()));
        m = next;
        let (next, _) = m.update(Message::SpawnFormConfirm);
        m = next;

        let form = m.spawn_modal.as_ref().expect(
            "a name colliding with an existing orchestrator id must keep the modal open"
        );
        let err = form.error.as_deref().expect("guard refusal must surface an error");
        assert!(err.contains("orch-a"), "unexpected error message: {err}");
        assert!(err.contains("already exists"), "unexpected error message: {err}");
        assert_eq!(m.orchestrators.len(), 1, "no second orchestrator record should be created");
        assert_eq!(m.sessions.len(), 1, "the colliding spawn must not create/overwrite any session");
        let unchanged = m.sessions.get("orch-a").expect("original orchestrator session must still exist");
        assert_eq!(
            unchanged.started_at, original.started_at,
            "the original orchestrator session's record must be untouched by the rejected duplicate spawn"
        );
    }

    #[test]
    fn editing_spawn_form_name_clears_stale_guard_error() {
        let _iso = ninox_core::test_isolation::isolate();
        use crate::components::spawn_modal::SpawnKind;

        let mut m = base(test_engine());
        let (next, _) = m.update(Message::SpawnSession);
        m = next;
        let (next, _) = m.update(Message::SpawnFormKind(SpawnKind::Standalone));
        m = next;
        let (next, _) = m.update(Message::SpawnFormName("solo-a".into()));
        m = next;
        // No workspace — confirm is blocked and sets an inline error.
        let (next, _) = m.update(Message::SpawnFormConfirm);
        m = next;
        assert!(m.spawn_modal.as_ref().unwrap().error.is_some(), "guard refusal must set an error");

        // Editing any field clears the stale error, even before it's fixed.
        let (next, _) = m.update(Message::SpawnFormName("solo-b".into()));
        m = next;
        assert!(
            m.spawn_modal.as_ref().unwrap().error.is_none(),
            "editing a field must clear the stale guard error"
        );
    }

    #[test]
    fn shortcuts_do_not_fire_in_terminal_view() {
        let e = test_engine();
        let mut m = base(e);
        let s = Session {
            id: "sess-a".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: SessionStatus::Working,
            agent_type: "c".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        let (next, _) = m.update(Message::EngineEvent(Box::new(Event::SessionSpawned(s))));
        m = next;
        let (next, _) = m.update(Message::NavigateSession("sess-a".into()));
        m = next;
        m.terminal_focused = true;

        let m = press(m, "1"); // must go to the terminal, not switch views
        assert!(matches!(m.view, View::SessionDetail { .. }));
    }

    #[test]
    fn spawn_modal_swallows_number_keys_without_navigating() {
        let m = base(test_engine());
        let (m, _) = m.update(Message::NavigateBoard { scope: None });
        assert!(matches!(m.view, View::Board { .. }));

        let (m, _) = m.update(Message::SpawnSession);
        assert!(m.spawn_modal.is_some());

        // While the modal is open, "1" must not fall through to the Fleet
        // navigation shortcut — the modal swallows every key except Escape.
        let m = press(m, "1");
        assert!(m.spawn_modal.is_some(), "modal must remain open");
        assert!(
            matches!(m.view, View::Board { .. }),
            "\"1\" must not navigate while the spawn modal is open"
        );
    }

    /// PR chips on every surface that shows a session, rendered offscreen
    /// through iced's software renderer with in-process events (no window,
    /// no desktop input). Set `NINOX_SHOT_DIR` to also write PNGs.
    mod pr_chip_surfaces {
        use super::*;
        use iced::advanced::{clipboard, renderer::Style};
        use iced_runtime::{user_interface::Cache, UserInterface};
        use iced::{mouse, Event, Font, Pixels, Point, Rectangle, Size, Theme};
        use ninox_core::pr_events::CiVerdict;
        use ninox_core::session_prs::{PrState, SessionPr};
        use ninox_core::types::{Orchestrator, SessionStatus};

        const SIZE: Size = Size { width: 1280.0, height: 800.0 };
        /// The Split view's terminal is a fixed 140 columns, so its side panel needs a wider window.
        const WIDE: Size = Size { width: 1800.0, height: 900.0 };

        #[allow(clippy::too_many_arguments)]
        fn pr(repo: &str, n: u64, title: &str, state: PrState, ci: CiVerdict, passing: u32, failing: u32, pending: u32) -> SessionPr {
            SessionPr {
                repo: repo.into(), number: n, url: format!("https://github.com/{repo}/pull/{n}"), title: title.into(),
                state, draft: false, ci, passing, failing, pending, changes_requested: false, approved: false, tracked: false,
            }
        }

        fn worker(id: &str, name: &str, status: SessionStatus, prs: Vec<SessionPr>) -> (Session, Vec<SessionPr>) {
            let mut s = pr_session(prs.first().map(|p| p.number), None, "org/bellhop");
            s.id = id.into();
            s.name = name.into();
            s.status = status;
            s.orchestrator_id = Some("ninox".into());
            s.summary = Some("rolling out spokes across regions".into());
            (s, prs)
        }

        fn seeded(view: View) -> App {
            let mut app = base(test_engine());
            let mut orch = pr_session(None, None, "org/ninox");
            orch.id = "ninox".into();
            orch.name = "ninox".into();
            orch.status = SessionStatus::Working;
            app.orchestrators = vec![Orchestrator { id: "ninox".into(), name: "ninox".into(), created_at: 0 }];
            app.sessions.insert("ninox".into(), orch);
            let mut changes = pr("org/manifests", 988, "Enable eus2 spokes", PrState::Open, CiVerdict::Passed, 12, 0, 0);
            changes.changes_requested = true;
            let mut draft = pr("org/manifests", 990, "Draft: cleanup", PrState::Open, CiVerdict::Passed, 4, 0, 0);
            draft.draft = true;
            let many = vec![
                pr("org/bellhop", 1466, "Spoke sizing", PrState::Open, CiVerdict::Pending, 28, 0, 25),
                pr("org/bellhop", 1473, "Add eus2 routes", PrState::Open, CiVerdict::Failed, 9, 3, 0),
                changes,
                draft,
                pr("org/manifests", 989, "Enable euc1 spokes", PrState::Merged, CiVerdict::Passed, 12, 0, 0),
            ];
            let few = vec![pr("org/bellhop", 412, "Rebase the parser", PrState::Open, CiVerdict::Passed, 4, 0, 0)];
            for (s, mut prs) in [
                worker("w-spokes", "mlops-5202-euc1-eus2-spokes", SessionStatus::PrOpen, many),
                worker("w-parser", "w-parser", SessionStatus::PrOpen, few),
                worker("w-none", "w-no-pr-yet", SessionStatus::Working, vec![]),
            ] {
                ninox_core::session_prs::order(&mut prs);
                app.session_prs.insert(s.id.clone(), prs);
                app.sessions.insert(s.id.clone(), s);
            }
            app.view = view;
            app.info_width = 360.0;
            app
        }

        /// Builds the full window view and runs `f` against its UI and renderer.
        fn with_ui<R>(app: &App, size: Size, f: impl FnOnce(&mut UserInterface<'_, Message, Theme, iced::Renderer>, &mut iced::Renderer) -> R) -> R {
            crate::style::load_bundled_fonts();
            let mut renderer = iced::Renderer::Secondary(iced_tiny_skia::Renderer::new(Font::default(), Pixels(16.0)));
            let mut ui = UserInterface::build(App::iced_view(app), size, Cache::default(), &mut renderer);
            f(&mut ui, &mut renderer)
        }

        fn click(ui: &mut UserInterface<'_, Message, Theme, iced::Renderer>, renderer: &mut iced::Renderer, at: Point) -> Vec<Message> {
            let mut out = Vec::new();
            let events = [
                Event::Mouse(mouse::Event::CursorMoved { position: at }),
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
            ];
            ui.update(&events, mouse::Cursor::Available(at), renderer, &mut clipboard::Null, &mut out);
            out
        }

        /// Every message a click anywhere in the window produces, by point.
        fn sweep(app: &App, size: Size) -> Vec<(Point, Vec<Message>)> {
            with_ui(app, size, |ui, renderer| {
                let mut hits = Vec::new();
                let mut y = 40.0;
                while y < size.height {
                    let mut x = 4.0;
                    while x < size.width {
                        let at = Point::new(x, y);
                        let msgs = click(ui, renderer, at);
                        if !msgs.is_empty() {
                            hits.push((at, msgs));
                        }
                        x += 3.0;
                    }
                    y += 3.0;
                }
                hits
            })
        }

        fn shot(app: &App, size: Size, name: &str) {
            let Ok(dir) = std::env::var("NINOX_SHOT_DIR") else { return };
            with_ui(app, size, |ui, renderer| {
                ui.draw(renderer, &Theme::Dark, &Style { text_color: crate::components::shell::tokens::TEXT }, mouse::Cursor::Unavailable);
                let (w, h) = (size.width as u32, size.height as u32);
                let mut pixels = tiny_skia::Pixmap::new(w, h).unwrap();
                let mut mask = tiny_skia::Mask::new(w, h).unwrap();
                let iced::Renderer::Secondary(r) = renderer else { panic!("software renderer expected") };
                r.draw(
                    &mut pixels.as_mut(),
                    &mut mask,
                    &iced_tiny_skia::graphics::Viewport::with_physical_size(Size::new(w, h), 1.0),
                    &[Rectangle::with_size(size)],
                    crate::components::shell::tokens::BG,
                    &[] as &[&str],
                );
                let path = std::path::Path::new(&dir).join(format!("{name}.png"));
                // The software renderer writes BGRA (what the window compositor expects).
                let mut rgba = pixels.data().to_vec();
                for i in (0..rgba.len()).step_by(4) {
                    rgba.swap(i, i + 2);
                }
                image::save_buffer(&path, &rgba, w, h, image::ColorType::Rgba8).unwrap();
            });
        }

        fn urls(hits: &[(Point, Vec<Message>)]) -> std::collections::BTreeSet<String> {
            hits.iter()
                .flat_map(|(_, m)| m)
                .filter_map(|m| match m { Message::OpenUrl(u) => Some(u.clone()), _ => None })
                .collect()
        }

        /// A chip click opens the PR and does nothing else: no point in the
        /// window yields both an `OpenUrl` and a navigation/selection.
        fn assert_chip_clicks_do_not_also_select(hits: &[(Point, Vec<Message>)]) {
            for (at, msgs) in hits {
                if msgs.iter().any(|m| matches!(m, Message::OpenUrl(_))) {
                    assert_eq!(msgs.len(), 1, "a click at {at:?} on a chip also fired {msgs:?}");
                }
            }
        }

        #[test]
        fn board_card_chips_open_the_pr_and_do_not_navigate_the_card() {
            let app = seeded(View::Board { scope: None });
            shot(&app, SIZE, "board");
            let hits = sweep(&app, SIZE);
            let opened = urls(&hits);
            assert!(opened.contains("https://github.com/org/bellhop/pull/1466"), "{opened:?}");
            assert!(opened.contains("https://github.com/org/bellhop/pull/412"), "{opened:?}");
            assert!(hits.iter().any(|(_, m)| m.iter().any(|m| matches!(m, Message::NavigateSession(id) if id == "w-spokes"))), "the card itself still navigates");
            assert_chip_clicks_do_not_also_select(&hits);
        }

        #[test]
        fn board_card_plus_n_chip_opens_the_session_side_panel_listing() {
            let app = seeded(View::Board { scope: None });
            let hits = sweep(&app, SIZE);
            let nav_count = hits.iter().filter(|(_, m)| matches!(m.as_slice(), [Message::NavigateSession(id)] if id == "w-spokes")).count();
            assert!(nav_count > 0);
        }

        #[test]
        fn sidebar_row_chips_open_the_pr_and_do_not_select_the_row() {
            let mut app = seeded(View::Fleet);
            app.fleet_nav.tree_collapsed = false;
            app.sidebar.expanded_orchestrators.insert("ninox".into());
            shot(&app, SIZE, "fleet-islands-and-sidebar");
            let hits = sweep(&app, SIZE);
            let sidebar_hits: Vec<_> = hits.iter().filter(|(p, _)| p.x < crate::components::shell::tree::EXPANDED_WIDTH).cloned().collect();
            let opened = urls(&sidebar_hits);
            assert!(!opened.is_empty(), "no PR chip in the sidebar: {sidebar_hits:?}");
            assert_chip_clicks_do_not_also_select(&sidebar_hits);
            assert!(
                sidebar_hits.iter().any(|(_, m)| matches!(m.as_slice(), [Message::NavigateSession(id)] if id == "w-spokes")),
                "the long-named multi-PR row still shows a clickable count chip: {sidebar_hits:?}",
            );
            assert!(
                sidebar_hits.iter().any(|(_, m)| m.iter().all(|m| !matches!(m, Message::OpenUrl(_))) && !m.is_empty()),
                "the row itself still selects",
            );
        }

        #[test]
        fn fleet_island_rows_and_graph_cards_carry_chips() {
            let app = seeded(View::Fleet);
            let hits = sweep(&app, SIZE);
            let in_main = hits;
            assert!(urls(&in_main).contains("https://github.com/org/bellhop/pull/1473"), "island row chip missing: the lead (failing) PR comes first");
            assert_chip_clicks_do_not_also_select(&in_main);

            let mut focus = seeded(View::Fleet);
            let islands = crate::components::fleet_islands::build_fleet_islands(&focus);
            focus.fleet_nav.open_island(ninox_overview::islands::IslandKey::Root("ninox".into()), &islands);
            shot(&focus, SIZE, "fleet-graph");
            let hits = sweep(&focus, SIZE);
            assert!(urls(&hits).contains("https://github.com/org/bellhop/pull/1466"), "graph card chip missing");
            assert_chip_clicks_do_not_also_select(&hits);
        }

        #[test]
        fn session_side_panel_lists_every_pr_open_first() {
            let app = seeded(View::SessionDetail { session_id: "w-spokes".into(), panel: DetailPanel::Split });
            shot(&app, WIDE, "session-detail");
            let opened = urls(&sweep(&app, WIDE));
            for n in [1466, 1473, 989] {
                assert!(opened.iter().any(|u| u.ends_with(&format!("/pull/{n}"))), "PR {n} not clickable in the side panel: {opened:?}");
            }
        }
    }

}

fn now_ms_i64() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as i64
}
