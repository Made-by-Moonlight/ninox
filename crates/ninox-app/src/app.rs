use std::{collections::{HashMap, VecDeque}, sync::Arc, time::{SystemTime, UNIX_EPOCH}};

use ninox_core::{
    config::{AppConfig, ThemeVariant},
    events::{Engine, Event},
    slugify,
    types::*,
    BrainEntry, BrainIndex, QueryFilters,
};
use iced::{Element, Subscription, Task, Theme};
use tokio::sync::broadcast;

use crate::{
    components::{
        catalogue_modal::CatalogueForm, session_detail::DetailPanel, spawn_modal::SpawnForm,
        terminal::TerminalState,
    },
    theme::{ColorScheme, Themes},
};

const MAX_NOTIFICATIONS: usize = 50;

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
}

#[derive(Debug, Clone, Default)]
pub struct FleetFilter {
    pub query: String,
}

/// Which of the two brain sub-views is showing (spec §5.IV "two modes").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BrainMode {
    #[default]
    Pinboard,
    Catalogue,
}

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

#[derive(Debug, Clone, Default)]
pub struct BrainViewState {
    pub entries:  Vec<BrainEntry>,
    pub loaded:   bool,
    pub selected: Option<String>,
    /// Entry id under the cursor on the pinboard canvas, if any — drives the
    /// bottom-right hover preview slip. Reset on catalogue/mode switch since
    /// a stale hover from a since-gone view makes no sense; resolution
    /// against `entries` is defensive elsewhere (reindex can also drop the
    /// hovered id without going through either reset path).
    pub hovered:  Option<String>,
    pub filter:   String,
    pub mode:         BrainMode,
    pub open_drawers: std::collections::HashSet<String>,
    /// Parsed markdown body of `selected`, preprocessed so `[[wikilinks]]`
    /// render as clickable `ninox-brain:` links.
    pub markdown: Vec<iced::widget::markdown::Item>,
    /// Entries that link to `selected` ("cited by"), fetched from the index
    /// in the `BrainSelectEntry` handler rather than re-derived from
    /// `entries` on every render. Empty when nothing is selected.
    pub backlinks: Vec<BrainEntry>,
    /// Entries related to `selected` — direct links, then co-citation, then
    /// shared tags (see `BrainIndex::related`) — same fetch-on-select
    /// pattern as `backlinks`.
    pub related: Vec<BrainEntry>,
    /// Pinboard edges as undirected, deduplicated node-index pairs into
    /// `entries`, resolved from `BrainIndex::links_all()` once per data
    /// change (`NavigateBrain` / `BrainReindex` / `BrainSwitchCatalogue`) —
    /// see `App::refresh_brain_edges` and `brain_pinboard::resolve_edges`.
    /// Never re-derived per canvas draw.
    pub edges: Vec<(usize, usize)>,
}

#[derive(Debug, Clone)]
pub enum View {
    FleetBoard { scope: Option<OrchestratorId> },
    SessionDetail { session_id: SessionId, panel: DetailPanel },
    PrList,
    Brain,
    Settings,
}

impl Default for View {
    fn default() -> Self {
        View::FleetBoard { scope: None }
    }
}

// ---------------------------------------------------------------------------
// App model
// ---------------------------------------------------------------------------

pub struct App {
    pub engine:             Arc<Engine>,
    pub config:             AppConfig,
    pub themes:             Themes,
    pub scheme:             ColorScheme,
    pub active_variant:     ThemeVariant,
    pub orchestrator_root:  std::path::PathBuf,
    pub orchestrator_agent: ninox_core::config::AgentConfig,
    pub orchestrators:      Vec<Orchestrator>,
    pub sessions:        HashMap<SessionId, Session>,
    pub brain:           Arc<BrainIndex>,
    pub brain_view:      BrainViewState,
    /// All selectable knowledge-base catalogues (`AppConfig::catalogue_options()`,
    /// snapshotted at startup — see `BrainSwitchCatalogue`).
    pub catalogues:      Vec<ninox_core::config::CatalogueRef>,
    /// Index into `catalogues` for the catalogue `brain` is currently open on.
    pub active_catalogue: usize,
    pub prs:             HashMap<PrId, PR>,
    pub ci_status:       HashMap<PrId, CIStatus>,
    pub review_threads:  HashMap<PrId, Vec<Comment>>,
    /// On-demand `git diff` text per session's workspace (`ensure_diff`).
    /// Absent = not fetched yet (render "Loading…"); `Some(None)` = fetched,
    /// no diff (no workspace recorded, or a clean working tree).
    pub diffs:           HashMap<SessionId, Option<String>>,
    pub notifications:   VecDeque<Notification>,
    /// True while an `ApplyUpdate`-triggered `cargo install` subprocess is
    /// running — disables the "Update now" action so a second click can't
    /// spawn a duplicate install.
    pub update_in_progress: bool,
    /// Result of the on-demand check `ensure_version_check` fires whenever
    /// Settings opens — drives the version line's "up to date"/"update
    /// available" state (spec: Settings panel).
    pub version_check:   VersionCheckState,
    pub sidebar:         SidebarState,
    pub view:            View,
    /// The user's preferred worker panel — global, not per-session: the
    /// last tab chosen in any session detail, applied when opening a
    /// worker (resetting to Split on every navigation was jarring).
    pub worker_panel:    crate::components::session_detail::DetailPanel,
    pub terminals:       HashMap<SessionId, TerminalState>,
    /// One hidden tmux client per on-screen session (the "view"). Dropping
    /// an entry kills the client process; the session itself stays detached
    /// and running.
    clients: HashMap<SessionId, ninox_core::client::AttachedClient>,
    /// Sessions that already burned their one automatic reattach after an
    /// unexpected ClientClosed. Cleared on navigation.
    reattach_attempted: std::collections::HashSet<SessionId>,
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
    /// Add-a-catalogue journal-entry modal, opened from the `+` at the
    /// right edge of the brain view's volume plate. Exclusive with
    /// `spawn_modal` in practice (spawn lives in other views); when both are
    /// somehow set, rendering and Esc both give `spawn_modal` precedence.
    pub catalogue_modal: Option<CatalogueForm>,
    /// Current terminal canvas dimensions, kept in sync by WindowResized.
    /// Used as the source of truth for all start_streaming + TerminalState::new calls.
    pub terminal_cols:   u16,
    pub terminal_rows:   u16,
    pub window_width:    f32,
    pub window_height:   f32,
    pub sidebar_width:   f32,
    pub info_width:      f32,
    pub drag:            Option<DragTarget>,
    pub fleet_filter:    FleetFilter,
    pub last_fleet_scope: Option<OrchestratorId>,
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub enum DragTarget { Sidebar, InfoPanel }

// `Session` grew when statusline context/cost fields were added, and `EngineEvent(Event)`
// wraps it by value; boxing `Event`'s Session-carrying variants would ripple across every
// call site that constructs or matches on `Event::SessionUpdated`/`SessionSpawned` across
// the workspace, which is out of scope here.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum Message {
    EngineEvent(Box<Event>),
    NavigateFleet { scope: Option<OrchestratorId> },
    NavigateSession(SessionId),
    /// Attach argv resolved — spawn the hidden tmux client for this session.
    ClientAttach { session_id: SessionId, argv: Vec<String> },
    /// Toggle an orchestrator's worker list open/closed in the tree.
    SelectOrchestrator(OrchestratorId),
    SpawnSession,
    SpawnFormName(String),
    SpawnFormKind(crate::components::spawn_modal::SpawnKind),
    SpawnFormWorkspace(String),
    SpawnFormHarness(String),
    SpawnFormModel(String),
    SpawnFormCustomModel(String),
    SpawnFormCatalogue(usize),
    SpawnFormConfirm,
    SpawnFormCancel,
    /// Opened from the `+` at the right edge of the brain view's volume
    /// plate (`components::brain_panel::volume_plate`).
    CatalogueModalOpen,
    CatalogueFormName(String),
    CatalogueFormPath(String),
    CatalogueFormConfirm,
    CatalogueFormCancel,
    SwitchDetailPanel(crate::components::session_detail::DetailPanel),
    RemoveOrchestrator(OrchestratorId),
    RemoveSession(SessionId),
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
    WindowResized(iced::Size),
    StartDrag(DragTarget),
    MouseMoved(iced::Point),
    MouseReleased,
    CopyToClipboard(String),
    PollSessions,
    NavigatePrList,
    NavigateBrain,
    /// Opened from the sidebar footer's `Settings ▸` row.
    NavigateSettings,
    /// Flip a harness's enabled flag (inert for the locked-on claude-code).
    SettingsToggleHarness(String),
    /// Workers card — the `[worker]` default `ninox spawn` launches.
    SettingsWorkerHarness(String),
    SettingsWorkerModel(String),
    SettingsWorkerCustomModel(String),
    SettingsWorkerCustomCommit,
    BrainSelectEntry(String),
    /// The pinboard canvas's hovered node changed (including to/from `None`)
    /// — emitted only on change, never on every mouse move.
    BrainHoverEntry(Option<String>),
    BrainFilterQuery(String),
    BrainReindex,
    BrainSetMode(BrainMode),
    BrainToggleDrawer(String),
    BrainLinkClicked(iced::widget::markdown::Url),
    BrainSwitchCatalogue(usize),
    ToggleNotifications,
    DismissNotification(String),
    DismissAllNotifications,
    NavigateNotification(SessionId),
    /// "Update now" pressed on an `UpdateAvailable` notification slip —
    /// kicks off `cargo install ninox --force --locked` as a subprocess.
    ApplyUpdate,
    /// The `cargo install` subprocess from `ApplyUpdate` finished.
    UpdateApplied(Result<(), String>),
    /// "Restart now" pressed on an `UpdateInstalled` notification slip —
    /// relaunches the binary and exits this process.
    RestartApp,
    /// The on-demand registry check `ensure_version_check` fires when
    /// Settings opens resolved: `Some(version)` = that version is newer
    /// than the running one, `None` = already current, `Err` = the check
    /// itself failed (no registry configured, network error, ...).
    VersionCheckResult(Result<Option<String>, String>),
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
    /// `models_cmd` discovery finished for a harness (`None` = failed —
    /// cached so pickers fall through to known_models without retrying).
    ModelListLoaded { harness: String, models: Option<Vec<String>> },
    /// An on-demand `git diff` fetch (`ensure_diff`) resolved for a
    /// session's workspace — `diff: None` means no workspace recorded, or a
    /// clean diff against the default branch, not an error.
    DiffFetched { session_id: SessionId, diff: Option<String> },
    Noop,
}

// ---------------------------------------------------------------------------
// Re-file — respawn a session onto current settings
// ---------------------------------------------------------------------------

/// Everything a Re-file needs, resolved against the CURRENT registry —
/// settings changes apply to new spawns immediately and to existing
/// sessions on Re-file; no silent in-place swaps (spec §V "Updating
/// running sessions"). Pure so it's unit-testable without tmux.
pub struct RefilePlan {
    pub agent:          ninox_core::config::AgentConfig,
    pub base_cmd:       String,
    pub workspace:      String,
    pub catalogue_path: String,
    pub extra_env:      Vec<(String, String)>,
}

/// Decide what a session's status becomes when its tmux pane is found
/// gone at startup. `has_resume_args` is the harness's capability (from
/// `HarnessRegistry::resume_cmd(...).is_some()` against a placeholder id —
/// callers don't have a real command to build yet, just the capability
/// check), not whether resume has ever been attempted.
fn reconciled_status_for_dead_session(
    claude_session_id: &Option<String>,
    has_resume_args:   bool,
) -> SessionStatus {
    if claude_session_id.is_some() && has_resume_args {
        SessionStatus::Interrupted
    } else {
        SessionStatus::Terminated
    }
}

#[cfg(test)]
mod reconciliation_tests {
    use super::*;

    #[test]
    fn session_with_id_and_resumable_harness_becomes_interrupted() {
        assert_eq!(
            reconciled_status_for_dead_session(&Some("uuid-1".into()), true),
            SessionStatus::Interrupted,
        );
    }

    #[test]
    fn legacy_session_without_id_becomes_terminated() {
        assert_eq!(
            reconciled_status_for_dead_session(&None, true),
            SessionStatus::Terminated,
        );
    }

    #[test]
    fn session_under_non_resumable_harness_becomes_terminated_even_with_an_id() {
        assert_eq!(
            reconciled_status_for_dead_session(&Some("uuid-1".into()), false),
            SessionStatus::Terminated,
        );
    }
}

/// `None` when the session has no recorded workspace (nothing to respawn
/// into). A worker re-files interactively — its original spawn prompt is
/// not stored — but keeps its orchestrator attachment for the tree.
pub fn refile_plan(
    session: &Session,
    is_orchestrator: bool,
    config: &AppConfig,
    claude_session_id: &str,
) -> Option<RefilePlan> {
    let workspace = session.workspace_path.clone()?;
    let agent = ninox_core::config::AgentConfig {
        harness: session.agent_type.clone(),
        model:   session.model.clone(),
    };
    let base_cmd = config.registry().interactive_cmd(&agent, claude_session_id);
    let catalogue_path = session.catalogue_path.clone()
        .unwrap_or_else(|| config.resolved_brain_path().to_string_lossy().to_string());
    let extra_env = if is_orchestrator {
        vec![
            ("NINOX_ORCHESTRATOR_ID".to_string(), session.id.clone()),
            ("NINOX_CALLER_TYPE".to_string(),     "orchestrator".to_string()),
        ]
    } else {
        Vec::new()
    };
    Some(RefilePlan { agent, base_cmd, workspace, catalogue_path, extra_env })
}

/// Like `refile_plan`, but rebuilds via `resume_cmd` (continuing the
/// existing `claude_session_id`) instead of `interactive_cmd` (starting
/// fresh). `None` when there's no workspace to resume into OR no stored
/// `claude_session_id` OR the harness can't resume (`resume_cmd` returns
/// `None`) — any of which means there's nothing to relaunch into.
pub fn resume_plan(
    session: &Session,
    is_orchestrator: bool,
    config: &AppConfig,
) -> Option<RefilePlan> {
    let workspace = session.workspace_path.clone()?;
    let claude_session_id = session.claude_session_id.as_deref()?;
    let agent = ninox_core::config::AgentConfig {
        harness: session.agent_type.clone(),
        model:   session.model.clone(),
    };
    let base_cmd = config.registry().resume_cmd(&agent, claude_session_id)?;
    let catalogue_path = session.catalogue_path.clone()
        .unwrap_or_else(|| config.resolved_brain_path().to_string_lossy().to_string());
    let extra_env = if is_orchestrator {
        vec![
            ("NINOX_ORCHESTRATOR_ID".to_string(), session.id.clone()),
            ("NINOX_CALLER_TYPE".to_string(),     "orchestrator".to_string()),
        ]
    } else {
        Vec::new()
    };
    Some(RefilePlan { agent, base_cmd, workspace, catalogue_path, extra_env })
}

// ---------------------------------------------------------------------------
// Keyboard → terminal byte conversion (static fn — no captures allowed by listen_with)
// ---------------------------------------------------------------------------

fn global_event_handler(
    event: iced::Event,
    status: iced::event::Status,
    _id: iced::window::Id,
) -> Option<Message> {
    // Window resize — always handle regardless of captured status.
    if let iced::Event::Window(iced::window::Event::Resized(size)) = &event {
        return Some(Message::WindowResized(*size));
    }
    // Mouse tracking for drag-resize handles.
    if let iced::Event::Mouse(iced::mouse::Event::CursorMoved { position }) = event {
        return Some(Message::MouseMoved(position));
    }
    if let iced::Event::Mouse(iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)) = event {
        return Some(Message::MouseReleased);
    }
    // Keyboard — only handle Ignored events (not already captured by a widget).
    if status == iced::event::Status::Captured {
        return None;
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
        orchestrator_agent: ninox_core::config::AgentConfig,
        brain: Arc<BrainIndex>,
    ) -> (Self, Task<Message>) {
        // Synchronously load persisted state from the DB so the UI isn't empty
        // on startup.
        let orchestrators = engine.store.list_orchestrators().unwrap_or_default();
        let sessions: HashMap<SessionId, Session> = engine
            .store
            .list_sessions()
            .unwrap_or_default()
            .into_iter()
            .map(|s| (s.id.clone(), s))
            .collect();

        let config = AppConfig::load().unwrap_or_default();

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
        let catalogues = config.catalogue_options();

        let mut app = Self {
            engine:             engine.clone(),
            config,
            themes,
            scheme,
            active_variant,
            orchestrator_root,
            orchestrator_agent,
            orchestrators,
            sessions,
            brain,
            brain_view:     BrainViewState::default(),
            catalogues,
            active_catalogue: 0,
            prs:            HashMap::new(),
            ci_status:      HashMap::new(),
            review_threads: HashMap::new(),
            diffs:          HashMap::new(),
            notifications:  VecDeque::new(),
            update_in_progress: false,
            version_check:  VersionCheckState::NotChecked,
            sidebar:        SidebarState::default(),
            view:           View::default(),
            worker_panel:   Default::default(),
            terminals:      HashMap::new(),
            clients:        HashMap::new(),
            reattach_attempted: std::collections::HashSet::new(),
            next_client_generation: 0,
            model_lists:    HashMap::new(),
            settings:       Default::default(),
            spawn_modal:    None,
            catalogue_modal: None,
            // Placeholders — corrected below by resize_terminals() using the
            // real default window size, so this never drifts out of sync with
            // main.rs's iced::window::Settings::default() (1024x768).
            terminal_cols:  140,
            terminal_rows:  50,
            window_width:   1024.0,
            window_height:  768.0,
            sidebar_width:  220.0,
            info_width:     300.0,
            drag:            None,
            fleet_filter:    FleetFilter::default(),
            last_fleet_scope: None,
        };
        Self::resize_terminals(&mut app);

        // Asynchronously mark dead sessions as Terminated.
        // PTY streaming is NOT started here — we stream on demand when the user
        // navigates to a session (NavigateSession).  Eagerly streaming at startup
        // with the wrong default dimensions (140×50) creates competing FIFO readers
        // that race with NavigateSession and re-populate state.terminals with
        // wrong-dimension content, causing the garbled-terminal bug.
        let task = Task::future(async move {
            use ninox_core::{tmux, Event as CoreEvent, SessionStatus};

            let sessions = match engine.store.list_sessions() {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("restore: list_sessions: {e}");
                    return Message::Noop;
                }
            };

            let registry = AppConfig::load().unwrap_or_default().registry();

            for session in sessions {
                if matches!(
                    session.status,
                    SessionStatus::Done | SessionStatus::Terminated | SessionStatus::Interrupted
                ) {
                    continue;
                }

                if !tmux::has_session(&session.id).await {
                    let agent = ninox_core::config::AgentConfig {
                        harness: session.agent_type.clone(),
                        model:   session.model.clone(),
                    };
                    let has_resume_args = registry.resume_cmd(&agent, "placeholder").is_some();
                    let mut dead = session.clone();
                    dead.status = reconciled_status_for_dead_session(
                        &session.claude_session_id, has_resume_args,
                    );
                    let _ = engine.store.upsert_session(&dead);
                    engine.emit(CoreEvent::SessionUpdated(dead, SessionFields::STATUS));
                }
            }

            Message::Noop
        });

        (app, task)
    }

    /// Iced-compatible mutable update — passed to `iced::application()`.
    pub fn iced_update(state: &mut Self, message: Message) -> Task<Message> {
        Self::apply(state, message)
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
            let models = crate::models::run_models_cmd(cmd).await;
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

    /// Kicks off an on-demand registry check for Settings' version line.
    /// Always re-runs on every `NavigateSettings` (unlike `ensure_models`'s
    /// cache-on-first-success) — the whole point is a fresh answer each
    /// time a user opens Settings to check, not a possibly-hours-stale one.
    fn ensure_version_check(state: &mut App) -> Task<Message> {
        state.version_check = VersionCheckState::Checking;
        Task::future(async move {
            use ninox_core::lifecycle::update_check::{CargoRegistryUpdateSource, check_for_update};
            let result = check_for_update(&CargoRegistryUpdateSource, "ninox", NINOX_VERSION)
                .await
                .map(|latest| latest.map(|v| v.to_string()))
                .map_err(|e| e.to_string());
            Message::VersionCheckResult(result)
        })
    }

    fn resize_terminals(state: &mut Self) -> Vec<(SessionId, u16, u16)> {
        use crate::components::session_detail::{TERM_CHROME_H, TERM_CHROME_W};

        let (cell_w, cell_h) = crate::components::terminal::cell_size(
            crate::components::terminal::FONT_SIZE,
        );
        let sidebar_w = state.sidebar_width + 5.0; // +5 for drag handle
        let info_w    = state.info_width + 5.0; // +5 for drag handle

        // Background sizing: what any session shows once Split (the default
        // panel) is active — this is also the authoritative size recorded on
        // `state` for sessions that don't have a `TerminalState` yet.
        // `TERM_CHROME_W`/`TERM_CHROME_H` are the header/tabs/term-frame
        // chrome that sits around the terminal `Canvas` in both the
        // `Terminal` and `Split` panels — see their doc comment in
        // `session_detail.rs` for the pixel-by-pixel derivation.
        let bg_cols = ((state.window_width - sidebar_w - info_w - TERM_CHROME_W).max(200.0) / cell_w) as u16;
        let bg_rows = ((state.window_height - TERM_CHROME_H).max(100.0) / cell_h) as u16;
        state.terminal_cols = bg_cols;
        state.terminal_rows = bg_rows;

        // The actively-viewed session uses whatever panel it's actually
        // showing — only Split narrows the width; every other panel uses
        // the full (non-info-panel) width. Orchestrator sessions render
        // terminal-only at full width REGARDLESS of the stored panel (see
        // `session_detail`'s `effective_panel`), so their sizing must match
        // or tmux draws the session at Split width and dot-fills the rest.
        let active = match &state.view {
            View::SessionDetail { session_id, panel: crate::components::session_detail::DetailPanel::Split }
                if !state.orchestrators.iter().any(|o| &o.id == session_id) =>
            {
                Some((session_id.clone(), bg_cols, bg_rows))
            }
            View::SessionDetail { session_id, .. } => {
                let cols = ((state.window_width - sidebar_w - TERM_CHROME_W).max(200.0) / cell_w) as u16;
                Some((session_id.clone(), cols, bg_rows))
            }
            _ => None,
        };

        let session_ids: Vec<SessionId> = state.terminals.keys().cloned().collect();
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
    /// `NavigateBrain`'s initial load, `BrainReindex`, and
    /// `BrainSwitchCatalogue` — never per canvas draw. A DB error is
    /// tolerated: warn and leave the pinboard edge-less rather than panic.
    fn refresh_brain_edges(state: &mut Self) {
        match state.brain.links_all() {
            Ok(links) => {
                state.brain_view.edges = crate::components::brain_pinboard::resolve_edges(
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

    /// Refetches `backlinks`/`related` for `brain_view.selected` from the
    /// index. No-ops when nothing is selected. Shared by `BrainSelectEntry`
    /// (fresh selection) and `BrainReindex` (index may have changed under a
    /// still-selected entry) — markdown handling stays with the select path
    /// since reindex never changes which entry is selected, only its graph.
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
    }

    /// Shared mutation logic.
    fn apply(state: &mut Self, message: Message) -> Task<Message> {
        match message {
            Message::EngineEvent(event) => Self::handle_engine_event(state, *event),

            Message::NavigateFleet { scope } => {
                state.last_fleet_scope = scope.clone();
                state.view = View::FleetBoard { scope };
                // Off-screen sessions keep running detached; drop their view clients.
                state.clients.clear();
                Task::none()
            }

            Message::NavigateSession(id) => {
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
                state.view = View::SessionDetail {
                    session_id: id.clone(),
                    panel: state.worker_panel,
                };
                // Drop every client that is no longer on screen — the tmux
                // sessions stay detached and running.
                state.clients.retain(|sid, _| sid == &id);
                state.reattach_attempted.clear();
                // Fresh view: kill any previous client + emulator for this
                // session; attach repaints the whole screen into clean state.
                state.clients.remove(&id);
                state.terminals.remove(&id);
                Self::resize_terminals(state);

                let diff_task = if state.worker_panel == DetailPanel::Diff {
                    Self::ensure_diff(state, &id)
                } else {
                    Task::none()
                };

                let engine = state.engine.clone();
                let attach_task = Task::future(async move {
                    if !ninox_core::tmux::has_session(&id).await {
                        if let Ok(Some(mut s)) = engine.store.get_session(&id) {
                            s.status = ninox_core::types::SessionStatus::Terminated;
                            let _ = engine.store.upsert_session(&s);
                            engine.emit(ninox_core::events::Event::SessionUpdated(
                                s, ninox_core::types::SessionFields::STATUS,
                            ));
                        }
                        return Message::Noop;
                    }
                    // Keep the pipe-pane tap alive for the WS route/monitoring.
                    if let Err(e) = ninox_core::pty::start_streaming(engine.clone(), id.clone(), &id).await {
                        tracing::warn!("pipe-pane tap for {id}: {e}");
                    }
                    let argv = ninox_core::tmux::attach_args(&id).await;
                    Message::ClientAttach { session_id: id, argv }
                });
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
                match ninox_core::client::AttachedClient::spawn(
                    state.engine.clone(), session_id.clone(), argv, cols, rows, generation,
                ) {
                    Ok(client) => {
                        // Fresh emulator wired to the client so query replies
                        // (DSR/DA/kitty) flow back to tmux.
                        state.terminals.insert(
                            session_id.clone(),
                            crate::components::terminal::TerminalState::new(
                                cols, rows, Some(client.input_sender()),
                            ),
                        );
                        state.clients.insert(session_id.clone(), client);
                        // The client was spawned at the background size; the
                        // active panel may want a different one — reflow and
                        // push the real size to the client PTY.
                        let resized = Self::resize_terminals(state);
                        if let Some((_, c, r)) = resized.iter().find(|(sid, ..)| sid == &session_id) {
                            if let Some(client) = state.clients.get(&session_id) {
                                client.resize(*c, *r);
                            }
                        }
                    }
                    Err(e) => tracing::error!("attach client for {session_id}: {e}"),
                }
                Task::none()
            }

            Message::SelectOrchestrator(id) => {
                if !state.sidebar.expanded_orchestrators.remove(&id) {
                    state.sidebar.expanded_orchestrators.insert(id);
                }
                Task::none()
            }

            Message::SpawnSession => {
                // Preselect the remembered `[orchestrator]` agent when its
                // harness is still enabled; otherwise the first enabled
                // harness (claude-code is locked on, so there's always one).
                let enabled = state.config.registry().enabled_names();
                let (harness, model) = if enabled.contains(&state.orchestrator_agent.harness) {
                    (state.orchestrator_agent.harness.clone(), state.orchestrator_agent.model.clone())
                } else {
                    (enabled.first().cloned().unwrap_or_else(|| "claude-code".into()), None)
                };
                let task = Self::ensure_models(state, &harness);
                state.spawn_modal = Some(SpawnForm { harness, model, ..SpawnForm::default() });
                task
            }

            Message::SpawnFormName(v) => {
                if let Some(f) = &mut state.spawn_modal { f.name = v; f.error = None; }
                Task::none()
            }

            Message::SpawnFormKind(kind) => {
                if let Some(f) = &mut state.spawn_modal { f.kind = kind; f.error = None; }
                Task::none()
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
                    f.model = None;
                    f.custom_model = None;
                    f.error = None;
                }
                task
            }

            Message::SpawnFormModel(v) => {
                if let Some(f) = &mut state.spawn_modal {
                    if v == crate::models::CUSTOM_SENTINEL {
                        f.custom_model = Some(String::new());
                    } else {
                        f.custom_model = None;
                        f.model = Some(v);
                    }
                    f.error = None;
                }
                Task::none()
            }

            Message::SpawnFormCustomModel(v) => {
                if let Some(f) = &mut state.spawn_modal { f.custom_model = Some(v); f.error = None; }
                Task::none()
            }

            Message::SpawnFormCatalogue(idx) => {
                if let Some(f) = &mut state.spawn_modal { f.catalogue_idx = idx; f.error = None; }
                Task::none()
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

                let expanded = crate::spawn_util::expand_tilde(&path_input);
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

                state.config.brain.catalogues.push(ninox_core::config::CatalogueRef {
                    name: name.clone(),
                    path: path.clone(),
                });
                if let Err(e) = state.config.save() {
                    tracing::warn!("failed to save config after adding catalogue '{name}': {e}");
                }
                state.catalogues = state.config.catalogue_options();
                let idx = state
                    .catalogues
                    .iter()
                    .position(|c| c.path == path)
                    .unwrap_or_else(|| state.catalogues.len().saturating_sub(1));

                state.catalogue_modal = None;
                // Reuse BrainSwitchCatalogue's handler logic to open the
                // index for real and refresh the view.
                App::apply(state, Message::BrainSwitchCatalogue(idx))
            }

            Message::SpawnFormConfirm => {
                use crate::components::spawn_modal::SpawnKind;

                let Some(form) = state.spawn_modal.clone() else { return Task::none(); };
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
                let spec = registry.spec(&form.harness);
                let agent = ninox_core::config::AgentConfig {
                    harness: form.harness.clone(),
                    model:   crate::components::spawn_modal::effective_model(&form, &spec),
                };
                let claude_session_id = ninox_core::harness::new_claude_session_id();
                let base_cmd = registry.interactive_cmd(&agent, &claude_session_id);
                let catalogue = state.config.catalogue_options()
                    .into_iter()
                    .nth(form.catalogue_idx)
                    .unwrap_or_else(|| ninox_core::config::CatalogueRef {
                        name: "default".to_string(),
                        path: state.config.resolved_brain_path(),
                    });
                let catalogue_path = catalogue.path.to_string_lossy().to_string();

                match form.kind {
                    // ── Standalone path: an interactive, unattached session in
                    // a user-supplied workspace. Mirrors the orchestrator flow
                    // (interactive agent in tmux + PTY streaming) but with no
                    // Orchestrator record, and with the same worktree isolation
                    // + repo detection the CLI worker path uses (shared via
                    // crate::spawn_util).
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

                        let mut workspace = crate::spawn_util::expand_tilde(&workspace_input);
                        if !std::path::Path::new(&workspace).exists() {
                            // The path doesn't exist yet — if it's nested
                            // under a git repo (e.g. this project's own
                            // `<repo>/.claude/worktrees/<branch>` convention),
                            // treat it as a request for a brand-new worktree
                            // rather than rejecting outright.
                            let ws_path = std::path::Path::new(&workspace).to_path_buf();
                            let repo_root = ws_path.parent().and_then(crate::spawn_util::find_repo_root);

                            match repo_root {
                                Some(root) => {
                                    let branch = ws_path
                                        .file_name()
                                        .and_then(|n| n.to_str())
                                        .unwrap_or(&workspace)
                                        .to_string();
                                    match crate::spawn_util::create_worktree_at(&root, &ws_path, &branch) {
                                        Ok(created) => workspace = created,
                                        Err(e) => {
                                            if let Some(f) = &mut state.spawn_modal {
                                                f.error = Some(format!(
                                                    "failed to create worktree at {workspace}: {e}"
                                                ));
                                            }
                                            return Task::none();
                                        }
                                    }
                                }
                                None => {
                                    // Bad path typed, no enclosing git repo —
                                    // keep the modal open rather than
                                    // optimistically spawning a session that
                                    // can never launch.
                                    if let Some(f) = &mut state.spawn_modal {
                                        f.error = Some(format!("workspace {workspace} does not exist"));
                                    }
                                    return Task::none();
                                }
                            }
                        }

                        let ts = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis();
                        let slug = slugify(&name);
                        let sid = if slug.is_empty() { format!("session-{ts}") } else { slug };

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
                        state.spawn_modal = None;

                        // "[orchestrator] stays only as the Spawn modal's
                        // remembered preselection" — remember the confirmed
                        // agent choice for the next spawn (either kind).
                        // Skipped when unchanged so routine spawns don't
                        // touch config.toml on disk.
                        if state.config.orchestrator.harness != agent.harness
                            || state.config.orchestrator.model != agent.model
                        {
                            state.orchestrator_agent = agent.clone();
                            state.config.orchestrator = agent.clone();
                            if let Err(e) = state.config.save() {
                                tracing::warn!("failed to save remembered agent preselection: {e}");
                            }
                        }

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
                            catalogue_path:  Some(catalogue_path.clone()),
                            context_used_pct: None, context_total_tokens: None, context_window_size: None,
                            claude_session_id: Some(claude_session_id.clone()),
                            summary:         None,
                            terminal_at:         None, gate_status: None,
                        };
                        let _ = state.engine.store.upsert_session(&session);
                        state.sessions.insert(session.id.clone(), session.clone());
                        state.engine.emit(Event::SessionSpawned(session));

                        state.view = View::SessionDetail {
                            session_id: sid.clone(),
                            panel:      DetailPanel::Terminal,
                        };

                        let engine = state.engine.clone();
                        let nm     = name;
                        let ts_i64 = ts as i64;

                        Task::future(async move {
                            // Isolate the session on its own branch/worktree when
                            // the workspace is a git repo; otherwise work in the
                            // directory itself (same fallback as run_spawn).
                            let effective_ws =
                                match crate::spawn_util::create_worker_worktree(&workspace, &sid).await {
                                    Ok(path) => path,
                                    Err(e) => {
                                        tracing::warn!(
                                            "worktree creation failed for {sid}, using shared workspace: {e}"
                                        );
                                        workspace.clone()
                                    }
                                };
                            if let Err(e) = crate::spawn_util::seed_worker_brain_skill(&effective_ws).await {
                                tracing::warn!("failed to seed brain skill for {sid}: {e}");
                            }

                            // Repo slug from the base workspace's git remote so
                            // poll_github can talk to the right owner/repo.
                            let repo = crate::spawn_util::repo_from_workspace(&workspace)
                                .unwrap_or_default();

                            // No NINOX_ORCHESTRATOR_ID and no caller-type vars:
                            // this session is unattached and reports to no one.
                            let attach_sid = sid.clone();
                            let attach = crate::spawn_util::spawn_interactive_session(
                                engine,
                                crate::spawn_util::InteractiveSpawnParams {
                                    session_id:      sid,
                                    name:            nm,
                                    workspace:       effective_ws,
                                    repo,
                                    orchestrator_id: None,
                                    agent,
                                    base_cmd,
                                    catalogue_path,
                                    extra_env:       Vec::new(),
                                    started_at:      ts_i64,
                                    claude_session_id,
                                    failure_status:  ninox_core::SessionStatus::Terminated,
                                    summary:         None,
                                },
                            )
                            .await;
                            match attach {
                                Some(argv) => Message::ClientAttach { session_id: attach_sid, argv },
                                None => Message::Noop,
                            }
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
                        state.spawn_modal = None;

                        // Remember the confirmed agent choice (see the
                        // standalone arm) — `[orchestrator]` is the Spawn
                        // modal's remembered preselection.
                        if state.config.orchestrator.harness != agent.harness
                            || state.config.orchestrator.model != agent.model
                        {
                            state.orchestrator_agent = agent.clone();
                            state.config.orchestrator = agent.clone();
                            if let Err(e) = state.config.save() {
                                tracing::warn!("failed to save remembered agent preselection: {e}");
                            }
                        }

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
                            catalogue_path:  Some(catalogue_path.clone()),
                            context_used_pct: None, context_total_tokens: None, context_window_size: None,
                            claude_session_id: Some(claude_session_id.clone()),
                            summary:         None,
                            terminal_at:         None, gate_status: None,
                        };
                        let _ = state.engine.store.upsert_session(&session);
                        state.sessions.insert(session.id.clone(), session.clone());
                        state.engine.emit(Event::SessionSpawned(session));

                        state.view = View::SessionDetail {
                            session_id: orch.id.clone(),
                            panel:      DetailPanel::Terminal,
                        };

                        let engine            = state.engine.clone();
                        let sid               = orch.id.clone();
                        let nm                = name;
                        let ts_i64            = ts as i64;
                        let orch_agent        = agent;

                        Task::future(async move {
                            if let Err(e) = tokio::fs::create_dir_all(&ws).await {
                                tracing::error!("mkdir orchestrator workspace {ws}: {e}");
                            }

                            // Orchestrator sessions get the caller-type vars and
                            // their own id so spawned workers can report back.
                            let extra_env = vec![
                                ("NINOX_ORCHESTRATOR_ID".to_string(), sid.clone()),
                                ("NINOX_CALLER_TYPE".to_string(),     "orchestrator".to_string()),
                            ];

                            let attach_sid = sid.clone();
                            let attach = crate::spawn_util::spawn_interactive_session(
                                engine,
                                crate::spawn_util::InteractiveSpawnParams {
                                    session_id:      sid,
                                    name:            nm,
                                    workspace:       ws,
                                    repo:            String::new(),
                                    orchestrator_id: None,
                                    agent:           orch_agent,
                                    base_cmd,
                                    catalogue_path,
                                    extra_env,
                                    started_at:      ts_i64,
                                    claude_session_id,
                                    failure_status:  ninox_core::SessionStatus::Terminated,
                                    summary:         None,
                                },
                            )
                            .await;
                            match attach {
                                Some(argv) => Message::ClientAttach { session_id: attach_sid, argv },
                                None => Message::Noop,
                            }
                        })
                    }
                }
            }

            Message::SwitchDetailPanel(new_panel) => {
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
                    _ => Task::none(),
                }
            }

            Message::RemoveOrchestrator(id) => {
                // Navigate away if we're viewing this orchestrator.
                if matches!(&state.view, View::SessionDetail { session_id, .. } if session_id == &id)
                    || matches!(&state.view, View::FleetBoard { scope: Some(s) } if s == &id)
                {
                    state.view = View::FleetBoard { scope: None };
                }
                // Remove the orchestrator, its workers, and its own session from in-memory state.
                state.orchestrators.retain(|o| o.id != id);
                state.sessions.retain(|k, s| {
                    k != &id && s.orchestrator_id.as_deref() != Some(id.as_str())
                });
                state.terminals.remove(&id);
                state.diffs.remove(&id);
                // Drop clients for the orchestrator itself and any worker
                // sessions removed above — only surviving sessions keep theirs.
                state.clients.retain(|sid, _| state.sessions.contains_key(sid));
                // Same for diffs: the orchestrator's own removal is handled
                // above, this catches its removed workers.
                state.diffs.retain(|sid, _| state.sessions.contains_key(sid));
                state.sidebar.expanded_orchestrators.remove(&id);
                let engine = state.engine.clone();
                Task::future(async move {
                    if let Err(e) = engine.remove_orchestrator(&id).await {
                        tracing::error!("remove orchestrator {id}: {e}");
                    }
                    Message::Noop
                })
            }

            Message::RemoveSession(id) => {
                if matches!(&state.view, View::SessionDetail { session_id, .. } if session_id == &id) {
                    state.view = View::FleetBoard { scope: None };
                }
                state.sessions.remove(&id);
                state.terminals.remove(&id);
                state.clients.remove(&id);
                state.diffs.remove(&id);
                let engine = state.engine.clone();
                Task::future(async move {
                    if let Err(e) = engine.remove_session(&id).await {
                        tracing::error!("remove session {id}: {e}");
                    }
                    Message::Noop
                })
            }

            Message::RefileSession(id) => {
                let Some(session) = state.sessions.get(&id).cloned() else { return Task::none(); };
                let is_orch = state.orchestrators.iter().any(|o| o.id == id);
                let claude_session_id = ninox_core::harness::new_claude_session_id();
                let Some(plan) = refile_plan(&session, is_orch, &state.config, &claude_session_id) else {
                    tracing::warn!("refile {id}: no workspace recorded, cannot respawn");
                    return Task::none();
                };
                // Drop the live client/grid first so the old PTY doesn't
                // fight the respawn for the same tmux session name.
                state.clients.remove(&id);
                state.terminals.remove(&id);
                // Re-file respawns the same id onto a fresh workspace/branch
                // — the cached diff is for the OLD incarnation and would
                // otherwise flash stale content until the next fetch.
                state.diffs.remove(&id);

                let ts = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64;
                let engine  = state.engine.clone();
                let name    = session.name.clone();
                let repo    = session.repo.clone();
                let orch_id = session.orchestrator_id.clone();
                let summary = session.summary.clone();
                Task::future(async move {
                    // Ignore kill errors — a Terminated husk has no tmux
                    // session, and Re-file on one "just spawns". The
                    // respawn upserts the record back to Working with cost
                    // 0; the usage poller re-ingests real spend from the
                    // workspace transcript.
                    let _ = ninox_core::tmux::kill_session(&id).await;
                    let attach = crate::spawn_util::spawn_interactive_session(
                        engine,
                        crate::spawn_util::InteractiveSpawnParams {
                            session_id:      id.clone(),
                            name,
                            workspace:       plan.workspace,
                            repo,
                            orchestrator_id: orch_id,
                            agent:           plan.agent,
                            base_cmd:        plan.base_cmd,
                            catalogue_path:  plan.catalogue_path,
                            extra_env:       plan.extra_env,
                            started_at:      ts,
                            claude_session_id,
                            failure_status:  ninox_core::SessionStatus::Terminated,
                            summary,
                        },
                    )
                    .await;
                    match attach {
                        Some(argv) => Message::ClientAttach { session_id: id, argv },
                        None       => Message::Noop,
                    }
                })
            }

            Message::ResumeSession(id) => {
                let Some(session) = state.sessions.get(&id).cloned() else { return Task::none(); };
                let is_orch = state.orchestrators.iter().any(|o| o.id == id);
                let Some(plan) = resume_plan(&session, is_orch, &state.config) else {
                    tracing::warn!("resume {id}: no workspace/claude_session_id/resume-capable harness, cannot resume");
                    return Task::none();
                };
                let Some(claude_session_id) = session.claude_session_id.clone() else {
                    return Task::none(); // unreachable: resume_plan already required this
                };
                state.clients.remove(&id);
                state.terminals.remove(&id);
                // Same id, relaunched — the pane died and may come back onto
                // a different pty/branch state; drop the cached diff so the
                // Diff panel refetches instead of showing the pre-resume text.
                state.diffs.remove(&id);

                let ts = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64;
                let engine  = state.engine.clone();
                let name    = session.name.clone();
                let repo    = session.repo.clone();
                let orch_id = session.orchestrator_id.clone();
                let summary = session.summary.clone();
                Task::future(async move {
                    let _ = ninox_core::tmux::kill_session(&id).await;
                    let attach = crate::spawn_util::spawn_interactive_session(
                        engine,
                        crate::spawn_util::InteractiveSpawnParams {
                            session_id:      id.clone(),
                            name,
                            workspace:       plan.workspace,
                            repo,
                            orchestrator_id: orch_id,
                            agent:           plan.agent,
                            base_cmd:        plan.base_cmd,
                            catalogue_path:  plan.catalogue_path,
                            extra_env:       plan.extra_env,
                            started_at:      ts,
                            claude_session_id,
                            failure_status:  ninox_core::SessionStatus::Interrupted,
                            summary,
                        },
                    )
                    .await;
                    match attach {
                        Some(argv) => Message::ClientAttach { session_id: id, argv },
                        None       => Message::Noop,
                    }
                })
            }

            Message::ResumeAllSessions => {
                let ids: Vec<SessionId> = state.sessions.values()
                    .filter(|s| matches!(s.status, ninox_core::SessionStatus::Interrupted))
                    .map(|s| s.id.clone())
                    .collect();
                Task::batch(ids.into_iter().map(|id| {
                    Task::done(Message::ResumeSession(id))
                }))
            }

            Message::RawKey { key, modifiers, text } => {
                // Esc closes the spawn modal from anywhere. Checked first —
                // both can't be open in practice (spawn lives in other
                // views), but if they somehow were, spawn takes precedence
                // here just like it does in `iced_view`'s stacking order.
                if state.spawn_modal.is_some() {
                    if matches!(key, iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape)) {
                        state.spawn_modal = None;
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

                let terminal_capturing = matches!(
                    &state.view,
                    View::SessionDetail { panel, .. }
                        if matches!(panel, DetailPanel::Terminal | DetailPanel::Split)
                );
                if !terminal_capturing && !modifiers.command() && !modifiers.control() && !modifiers.alt() {
                    if let iced::keyboard::Key::Character(c) = &key {
                        match c.as_str() {
                            "1" => return App::apply(state, Message::NavigateFleet { scope: None }),
                            "2" => return App::apply(state, Message::NavigatePrList),
                            "3" => return App::apply(state, Message::NavigateBrain),
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

                if let View::SessionDetail {
                    session_id,
                    panel: crate::components::session_detail::DetailPanel::Terminal
                        | crate::components::session_detail::DetailPanel::Split,
                } = &state.view {
                    let session_id = session_id.clone();
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
                Task::none()
            }


            Message::SwitchTheme(variant) => {
                state.active_variant = variant;
                state.scheme = state.themes.scheme(variant);
                state.config.theme = variant;
                for term in state.terminals.values_mut() {
                    term.cache.clear();
                }
                if let Err(e) = state.config.save() {
                    tracing::error!("failed to save theme config: {e}");
                }
                Task::none()
            }

            Message::StartDrag(target) => {
                state.drag = Some(target);
                Task::none()
            }

            Message::MouseMoved(position) => {
                match state.drag {
                    Some(DragTarget::Sidebar) => {
                        state.sidebar_width = position.x.clamp(150.0, 400.0);
                        // Reflow the grid locally for live visual feedback; the
                        // backing tmux pane is synced once on MouseReleased
                        // rather than on every drag frame.
                        Self::resize_terminals(state);
                    }
                    Some(DragTarget::InfoPanel) => {
                        let available = state.window_width - state.sidebar_width - 10.0;
                        state.info_width = (state.window_width - position.x).clamp(200.0, available.max(200.0));
                        Self::resize_terminals(state);
                    }
                    None => {}
                }
                Task::none()
            }

            Message::MouseReleased => {
                let was_dragging = state.drag.is_some();
                state.drag = None;
                if was_dragging {
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

            Message::PollSessions => {
                let db_sessions   = state.engine.store.list_sessions().unwrap_or_default();
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

                // Add genuinely new sessions (spawned by `ninox spawn`, or a
                // completed session not yet tracked in `state`) — including
                // Done/Terminated ones, which stay visible until the sweep
                // above purges their store record. PTY streaming is NOT
                // started here — NavigateSession handles that on demand with
                // the correct window dimensions.
                for session in db_sessions {
                    if !state.sessions.contains_key(&session.id) {
                        state.sessions.insert(session.id.clone(), session);
                    }
                }

                // Refresh the diff for whatever session is on the Diff panel
                // right now — a live session's diff changes as the worker
                // commits, so this is the "keeps updating" tick for it.
                match &state.view {
                    View::SessionDetail { session_id, panel: DetailPanel::Diff } => {
                        Self::ensure_diff(state, &session_id.clone())
                    }
                    _ => Task::none(),
                }
            }

            Message::NavigatePrList => {
                state.view = View::PrList;
                // No session is on screen in the PR list; drop all view clients.
                state.clients.clear();
                Task::none()
            }

            Message::NavigateBrain => {
                if !state.brain_view.loaded {
                    // `None`: the GUI brain panel is keyword-only for now — wiring it to the
                    // same embedder the server constructs at startup is a natural follow-up,
                    // deliberately out of scope for the CLI/HTTP-focused semantic search spec.
                    match state.brain.query("", None, QueryFilters::default()) {
                        Ok(entries) => {
                            state.brain_view.entries = entries;
                            state.brain_view.loaded = true;
                            Self::refresh_brain_edges(state);
                        }
                        Err(e) => tracing::error!("brain query: {e}"),
                    }
                }
                state.view = View::Brain;
                Task::none()
            }

            Message::NavigateSettings => {
                state.view = View::Settings;
                // Kick model discovery for the worker default's harness so
                // the Workers card's picker has live options when it opens.
                let models_task = Self::ensure_models(state, &state.config.worker.harness.clone());
                let version_task = Self::ensure_version_check(state);
                Task::batch(vec![models_task, version_task])
            }

            Message::SettingsToggleHarness(name) => {
                // claude-code is the locked-on default — inert by design.
                if name == "claude-code" {
                    return Task::none();
                }
                // Write the FULL effective spec with `enabled` flipped —
                // config entries replace builtin specs wholesale, so a bare
                // `{ enabled: true }` would wipe the builtin's args.
                let mut spec = state.config.registry().spec(&name);
                spec.enabled = !spec.enabled;
                state.config.harnesses.insert(name.clone(), spec);
                if let Err(e) = state.config.save() {
                    tracing::warn!("failed to save config after toggling harness {name}: {e}");
                }
                Task::none()
            }

            Message::SettingsWorkerHarness(h) => {
                // pick_list fires on re-selecting the current value — don't
                // wipe (and re-save) the model for a no-op selection.
                if state.config.worker.harness == h {
                    return Task::none();
                }
                let task = Self::ensure_models(state, &h);
                state.config.worker.harness = h;
                // Clear the model — ids from one harness must not leak into
                // another's launch command.
                state.config.worker.model = None;
                state.settings.worker_custom = None;
                if let Err(e) = state.config.save() {
                    tracing::warn!("failed to save worker harness: {e}");
                }
                task
            }

            Message::SettingsWorkerModel(v) => {
                if v == crate::models::CUSTOM_SENTINEL {
                    state.settings.worker_custom =
                        Some(state.config.worker.model.clone().unwrap_or_default());
                    return Task::none();
                }
                state.settings.worker_custom = None;
                state.config.worker.model = Some(v);
                if let Err(e) = state.config.save() {
                    tracing::warn!("failed to save worker model: {e}");
                }
                Task::none()
            }

            Message::SettingsWorkerCustomModel(v) => {
                state.settings.worker_custom = Some(v);
                Task::none()
            }

            Message::SettingsWorkerCustomCommit => {
                if let Some(v) = state.settings.worker_custom.take() {
                    let t = v.trim();
                    state.config.worker.model = (!t.is_empty()).then(|| t.to_string());
                    if let Err(e) = state.config.save() {
                        tracing::warn!("failed to save worker model: {e}");
                    }
                }
                Task::none()
            }

            Message::BrainSelectEntry(id) => {
                if let Some(e) = state.brain_view.entries.iter().find(|e| e.id == id) {
                    state.brain_view.markdown = iced::widget::markdown::parse(
                        &crate::components::brain_panel::preprocess_wikilinks(&e.body),
                    ).collect();
                    state.brain_view.open_drawers.insert(e.entry_type.clone());
                    state.brain_view.mode = BrainMode::Catalogue;
                }
                state.brain_view.selected = Some(id);
                Self::refresh_selection_graph(state);
                Task::none()
            }

            Message::BrainHoverEntry(id) => {
                state.brain_view.hovered = id;
                Task::none()
            }

            Message::BrainFilterQuery(query) => {
                state.brain_view.filter = query;
                Task::none()
            }

            Message::BrainReindex => {
                // `None`: the GUI brain panel is keyword-only for now — wiring it to the
                // same embedder the server constructs at startup is a natural follow-up,
                // deliberately out of scope for the CLI/HTTP-focused semantic search spec.
                match state.brain.rebuild(None) {
                    Ok(stats) => {
                        tracing::info!("brain reindexed: {} entries", stats.indexed);
                        // `None`: the GUI brain panel is keyword-only for now — wiring it to the
                        // same embedder the server constructs at startup is a natural follow-up,
                        // deliberately out of scope for the CLI/HTTP-focused semantic search spec.
                        match state.brain.query("", None, QueryFilters::default()) {
                            Ok(entries) => {
                                state.brain_view.entries = entries;
                                Self::refresh_brain_edges(state);
                                // The selected entry may have been renamed or
                                // deleted by whatever triggered the reindex —
                                // if it no longer resolves, clear the pane
                                // instead of showing a ghost selection.
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
                                }
                                Self::refresh_selection_graph(state);
                            }
                            Err(e) => tracing::error!("brain query after reindex: {e}"),
                        }
                        state.brain_view.loaded = true;
                    }
                    Err(e) => tracing::error!("brain rebuild: {e}"),
                }
                Task::none()
            }

            Message::BrainSetMode(m) => {
                state.brain_view.mode = m;
                state.brain_view.hovered = None;
                Task::none()
            }

            Message::BrainToggleDrawer(cat) => {
                if !state.brain_view.open_drawers.remove(&cat) {
                    state.brain_view.open_drawers.insert(cat);
                }
                Task::none()
            }

            Message::BrainLinkClicked(url) => {
                if url.scheme() == "ninox-brain" {
                    // `url.path()` returns the raw, still percent-encoded
                    // opaque part (this is a cannot-be-a-base URL, so `url`
                    // never decodes it for us) — reverse the encoding
                    // `preprocess_wikilinks` applied before matching.
                    let link = crate::components::brain_panel::percent_decode_wikilink_target(url.path());
                    if let Some(e) =
                        crate::components::brain_panel::resolve_link(&state.brain_view.entries, &link)
                    {
                        let id = e.id.clone();
                        return App::apply(state, Message::BrainSelectEntry(id));
                    }
                } else {
                    return App::apply(state, Message::OpenUrl(url.to_string()));
                }
                Task::none()
            }

            // `App.brain` is an `Arc<BrainIndex>` shared with the server at startup —
            // this replaces the app's own Arc with a fresh index over the chosen
            // catalogue's path. It does NOT affect the server's copy (acceptable:
            // catalogue viewing is app-side; the server's brain stays on the
            // catalogue it was started with).
            Message::BrainSwitchCatalogue(idx) => {
                if let Some(cat) = state.catalogues.get(idx).cloned() {
                    match BrainIndex::open(&cat.path) {
                        Ok(new_brain) => {
                            state.brain = Arc::new(new_brain);
                            state.active_catalogue = idx;
                            state.brain_view.selected = None;
                            state.brain_view.hovered = None;
                            state.brain_view.markdown.clear();
                            state.brain_view.open_drawers.clear();
                            state.brain_view.backlinks.clear();
                            state.brain_view.related.clear();
                            state.brain_view.edges.clear();
                            state.brain_view.loaded = false;
                            // `None`: the GUI brain panel is keyword-only for now — wiring it to the
                            // same embedder the server constructs at startup is a natural follow-up,
                            // deliberately out of scope for the CLI/HTTP-focused semantic search spec.
                            match state.brain.query("", None, QueryFilters::default()) {
                                Ok(entries) => {
                                    state.brain_view.entries = entries;
                                    state.brain_view.loaded = true;
                                    Self::refresh_brain_edges(state);
                                }
                                Err(e) => tracing::error!("brain query after catalogue switch: {e}"),
                            }
                        }
                        Err(e) => tracing::warn!("open catalogue '{}': {e}", cat.name),
                    }
                }
                Task::none()
            }

            Message::ToggleNotifications => {
                state.sidebar.show_notifications = !state.sidebar.show_notifications;
                Task::none()
            }

            Message::DismissNotification(id) => {
                state.notifications.retain(|n| n.id != id);
                Task::none()
            }

            Message::DismissAllNotifications => {
                state.notifications.clear();
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

            Message::ApplyUpdate => {
                if state.update_in_progress {
                    return Task::none();
                }
                state.update_in_progress = true;
                Task::future(async move {
                    use ninox_core::lifecycle::update_check::{CargoInstallInstaller, UpdateInstaller};
                    let result = CargoInstallInstaller.install("ninox").await.map_err(|e| e.to_string());
                    Message::UpdateApplied(result)
                })
            }

            Message::UpdateApplied(result) => {
                state.update_in_progress = false;
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
                    // A `local` scroll (drag-select auto-scroll) must never
                    // reach the PTY — under a mouse-mode TUI `encode_wheel`
                    // would scroll the inner app instead of extending the
                    // selection into ninox's scrollback.
                    let pty_bytes =
                        if local { None } else { crate::input::encode_wheel(delta, 0, 0, &mode) };
                    if let Some(bytes) = pty_bytes {
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
                            let total = ninox_core::tmux::history_size(&sid).await;
                            let end = from - 1; // next line above cache
                            let start = (from - FETCH_CHUNK).max(-total);
                            if end < -total || total == 0 {
                                return Message::HistoryFetched {
                                    session_id: sid, bytes: Vec::new(),
                                    fetched_to: from, top_reached: true,
                                };
                            }
                            let bytes = ninox_core::tmux::capture_history(&sid, start, end).await;
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

            Message::ModelListLoaded { harness, models } => {
                state.model_lists.insert(harness, models);
                Task::none()
            }

            Message::DiffFetched { session_id, diff } => {
                state.diffs.insert(session_id, diff);
                Task::none()
            }

            Message::Noop => Task::none(),
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
                state.sessions.retain(|_, s| s.orchestrator_id.as_deref() != Some(id.as_str()));
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
                state.terminals.remove(&id);
                // A done session is definitionally not viewable.
                state.clients.remove(&id);
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
                state.terminals.remove(&session_id);
                state.diffs.remove(&session_id);
                // One automatic reattach for unexpected deaths (tmux server
                // restart); repeated failures fall through to the
                // "Terminal connecting…" placeholder.
                if viewing && state.reattach_attempted.insert(session_id.clone()) {
                    return Task::future(async move {
                        if !ninox_core::tmux::has_session(&session_id).await {
                            return Message::Noop;
                        }
                        let argv = ninox_core::tmux::attach_args(&session_id).await;
                        Message::ClientAttach { session_id, argv }
                    });
                }
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
                state.review_threads
                    .entry(pr_id)
                    .or_default()
                    .push(comment);
                Task::none()
            }

            Event::Notification(n) => {
                Self::push_notification(state, n);
                Task::none()
            }
        }
    }

    /// Fires the native macOS notification (via `notify-rust`) and appends
    /// to the in-app panel, capped at `MAX_NOTIFICATIONS` — shared by
    /// engine-sourced notifications (`Event::Notification`, e.g. the
    /// periodic update check) and ones this process constructs directly for
    /// an on-demand action's outcome (e.g. `UpdateApplied`).
    fn push_notification(state: &mut Self, n: Notification) {
        let title = n.title.clone();
        let body = n.body.clone();
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
        state.notifications.push_back(n);
        if state.notifications.len() > MAX_NOTIFICATIONS {
            state.notifications.pop_front();
        }
    }

    /// A 5px drag handle strip between resizable panels.
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
        .on_press(Message::StartDrag(target))
        .into()
    }

    /// View — sidebar + fleet board or session detail.
    pub fn iced_view(state: &Self) -> Element<'_, Message> {
        use iced::widget::{container, row};
        use crate::components::{
            brain_panel::brain_panel,
            catalogue_modal::catalogue_modal,
            fleet_board::fleet_board,
            pr_list::pr_list,
            session_detail::session_detail,
            settings_panel::settings_panel,
            sidebar::sidebar,
            spawn_modal::spawn_modal,
        };
        use iced::{Background, Length};

        let bg = state.scheme.paper;
        let main: Element<Message> = match &state.view {
            View::FleetBoard { scope } => fleet_board(state, scope.as_ref()),
            View::SessionDetail { session_id, panel } => session_detail(state, session_id, panel),
            View::PrList => pr_list(state),
            View::Brain => brain_panel(state),
            View::Settings => settings_panel(state),
        };

        let base: Element<Message> = container(
            row![
                sidebar(state),
                App::drag_handle(DragTarget::Sidebar, state.scheme.rule_dark),
                main,
            ].height(Length::Fill),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .style(move |_theme| container::Style {
            background: Some(Background::Color(bg)),
            ..Default::default()
        })
        .into();

        // Spawn wins on top if both were somehow set — exclusive in
        // practice (spawn lives in other views).
        if let Some(form) = &state.spawn_modal {
            iced::widget::stack![base, spawn_modal(state, form)].into()
        } else if let Some(form) = &state.catalogue_modal {
            iced::widget::stack![base, catalogue_modal(state, form)].into()
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
async fn next_coalesced_event(
    rx: &mut broadcast::Receiver<Event>,
    pending: &mut Option<Event>,
) -> Option<Event> {
    let event = if let Some(event) = pending.take() {
        event
    } else {
        loop {
            match rx.recv().await {
                Ok(event) => break event,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    };

    // Extend `bytes` in place rather than re-cloning the merged-so-far
    // buffer on every iteration — a bursty repaint can queue dozens of
    // chunks, and cloning each time would copy O(chunks²) bytes instead
    // of O(total bytes).
    let Event::ClientOutput { session_id, generation, mut bytes } = event else {
        return Some(event);
    };
    loop {
        match rx.try_recv() {
            Ok(Event::ClientOutput { session_id: sid2, generation: gen2, bytes: more })
                if sid2 == session_id && gen2 == generation =>
            {
                bytes.extend(more);
            }
            Ok(other) => {
                *pending = Some(other);
                break;
            }
            Err(_) => break,
        }
    }

    Some(Event::ClientOutput { session_id, generation, bytes })
}

impl App {
    /// Subscription that drives `Message::EngineEvent` from the engine broadcast channel.
    pub fn subscription(state: &Self) -> Subscription<Message> {
        let mut rx: broadcast::Receiver<Event> = state.engine.subscribe();
        let mut pending: Option<Event> = None;
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

        Subscription::batch([engine_sub, keyboard_sub, poll_sub])
    }

    /// Theme accessor for the iced `.theme()` builder.
    pub fn theme(state: &Self) -> Theme {
        state.scheme.iced_theme()
    }
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

/// Seeds `~/.config/ninox/orchestrator/` (or the configured root) with the
/// files that orchestrator sessions need: AGENTS.md (canonical, CLAUDE.md
/// symlinks to it), spawn-worker skill, set-agent-config skill, brain skill,
/// and the subagent-blocker PreToolUse hook.
///
/// AGENTS.md and settings.json are skipped if already present (user-editable).
/// Skill files and the blocker are always overwritten to stay in sync.
pub async fn setup_orchestrator_root(
    root: &std::path::Path,
    ninox_bin: &str,
    config_path: &str,
) -> anyhow::Result<()> {
    use tokio::fs;

    let claude_dir        = root.join(".claude");
    let claude_skills_dir = claude_dir.join("skills");
    let spawn_skill_dir   = claude_skills_dir.join("spawn-worker");
    let config_skill_dir  = claude_skills_dir.join("set-agent-config");
    let brain_skill_dir   = claude_skills_dir.join("brain");
    fs::create_dir_all(&claude_dir).await?;
    fs::create_dir_all(&spawn_skill_dir).await?;
    fs::create_dir_all(&config_skill_dir).await?;
    fs::create_dir_all(&brain_skill_dir).await?;

    let spawn_skill_path  = spawn_skill_dir.join("SKILL.md");
    let config_skill_path = config_skill_dir.join("SKILL.md");
    let brain_skill_path  = brain_skill_dir.join("SKILL.md");

    // AGENTS.md is canonical; CLAUDE.md symlinks to it.
    let agents_md_path = root.join("AGENTS.md");
    if !agents_md_path.exists() {
        let body = format!(
            "# Ninox Orchestrator\n\n\
             Before doing anything else, read and follow: `{spawn_skill}`\n\n\
             ## Available Skills\n\n\
             - `{spawn_skill}` — spawning worker sessions\n\
             - `{config_skill}` — changing agent harness or model\n\
             - `{brain_skill}` — reading and writing the shared knowledge brain\n",
            spawn_skill  = spawn_skill_path.display(),
            config_skill = config_skill_path.display(),
            brain_skill  = brain_skill_path.display(),
        );
        fs::write(&agents_md_path, body).await?;
    }
    let claude_md_path = root.join("CLAUDE.md");
    if !claude_md_path.exists() {
        #[cfg(unix)]
        tokio::fs::symlink("AGENTS.md", &claude_md_path).await?;
        #[cfg(not(unix))]
        {
            let body = fs::read_to_string(&agents_md_path).await?;
            fs::write(&claude_md_path, body).await?;
        }
    }

    // spawn-worker skill — always overwritten.
    let spawn_skill_content = format!(
        r#"---
name: spawn-worker
description: Use before starting any implementation task as a Ninox orchestrator — spawn a worker session instead of doing the work yourself.
---

# Spawn a Worker, Not a Subagent

You are a **Ninox orchestrator agent**. You coordinate — you do not implement.

## Your Role

- Spawn worker sessions for all implementation tasks
- Monitor worker progress; direct workers when they get stuck
- Never implement code, run tests, or create PRs yourself

## Spawning Workers

Name workers after the ticket or task so they are easy to reference:

```bash
{ninox_bin} spawn \
  --name "ath-123-auth-fix" \
  --prompt "Complete task description with acceptance criteria, repo path, and branch" \
  --workspace /absolute/path/to/repo
```

`--name` becomes the session ID. Names are slugified automatically (`"ATH-123 auth"` → `"ath-123-auth"`).
Omitting `--name` generates a timestamp ID (`worker-…`).

`NINOX_ORCHESTRATOR_ID` is set in your environment and picked up automatically.
Each spawn prints the session ID (`spawned ath-123-auth-fix`) — use it to send follow-ups.

## Messaging Workers (Orchestrator → Worker)

Send instructions or follow-ups to a worker using its session ID:

```bash
{ninox_bin} send ath-123-auth-fix "Focus on the token refresh path first"
```

## Work Requests (Worker → Orchestrator)

Workers are scoped to one task and one PR. When a worker discovers additional
work, it runs `{ninox_bin} request-work "<description>"` and Ninox forwards
the request to you as a `[Ninox] Worker … requested additional work` message.

When one arrives: decide whether the work is worth doing, and if so
spawn a new worker for it with `{ninox_bin} spawn`. **Never** tell a worker to widen
its own task or PR — extra scope always gets its own worker. Ninox will also
warn you (`[Ninox] Worker … opened N PRs beyond its tracked PR`) if a worker
opens extra PRs anyway; review each extra PR and either close it or hand it
to a dedicated worker.

## The Rule

**Never use the Agent tool for implementation work.** All implementation goes
through `{ninox_bin} spawn`. Read-only Explore/Plan agents are permitted.

| Thought | Reality |
|---|---|
| "The task is small" | Size doesn't matter. Workers handle small tasks fine. |
| "I'm already mid-context" | Offload work to preserve orchestrator context. |
| "It's just a push/PR" | Pushes need auth wiring subagents don't have. |
| "The Agent tool is easier" | It's always easier. That's why this rule exists. |
"#,
        ninox_bin = ninox_bin,
    );
    fs::write(&spawn_skill_path, spawn_skill_content).await?;

    // set-agent-config skill — always overwritten.
    let config_skill_content = format!(
        r#"---
name: set-agent-config
description: Use when the user asks to change the orchestrator's or worker's agent harness or model.
---

# Set Ninox Agent Config

Use this skill when the user asks to change the agent harness or model.

## Config file

```
{config_path}
```

## Format

```toml
[orchestrator]
harness = "claude-code"   # claude-code | codex | aider | opencode
model = "model-name"      # omit to use the harness default

[worker]
harness = "claude-code"
model = "model-name"
```

Use the Edit tool to update the relevant field. Changes take effect on the next spawn.
"#,
        config_path = config_path,
    );
    fs::write(&config_skill_path, config_skill_content).await?;

    // brain skill — always overwritten.
    let brain_skill_content = format!(
        r#"---
name: brain
description: Read and write Ninox's shared knowledge brain. Use before exploring unfamiliar code (query first) and as soon as you learn something worth keeping — write it down, don't wait until the end.
---

# Read and Write the Brain

The brain is Ninox's persistent, shared knowledge store. As you explore
codebases you discover things — where a type is defined, how two repos
relate, why a decision was made. Without a place to put that, every new
session starts cold. Write it down so the next orchestrator doesn't have to
rediscover it.

Your session's brain is already resolved — these commands act on it with no
extra configuration.

## 1. Query first

`brain query` blends keyword and semantic matches automatically — no new
syntax needed. Before writing a new entry, check whether one already exists:

```bash
{ninox_bin} brain query "<name or concept>"
```

Narrow with filters:

```bash
{ninox_bin} brain query "<text>" --entry-type repo
{ninox_bin} brain query "<text>" --tag auth
```

If a relevant entry exists, update it instead of creating a duplicate.

## 2. Write a fact

Create or update a Markdown file under the section that fits, then rebuild
the index:

```
repos/          where repositories live, their purpose, entry points
symbols/        where types, functions, and modules are defined
concepts/       domain terminology and mental models
patterns/       conventions and recurring implementation shapes
decisions/      why something was built a certain way (ADRs)
architecture/   how the system is structured — components, data flows
relationships/  how repos, services, and teams connect
errors/         known failure modes and how to resolve them
```

Each file needs YAML frontmatter followed by Markdown body:

```markdown
---
type: repo
name: my-crate
tags: [auth, core]
repos: [my-crate]
updated: 2026-07-06
---

# my-crate

Entry point: `src/main.rs`
Build: `cargo build`

Facts, not prose. Link related entries with `[[other-entry]]`.
```

Then rebuild the index so the write becomes queryable:

```bash
{ninox_bin} brain index
```

## 3. Read for context

At the start of work in unfamiliar territory, query before exploring:

```bash
{ninox_bin} brain query "" --entry-type architecture
{ninox_bin} brain query "" --entry-type repo
{ninox_bin} brain show <path-from-a-query-result>
```

## The Rule

**Before exploring anything unfamiliar, query first.** As soon as you learn
something a future session would want to know — don't wait until the end
of your session — write it down and index it. A stale or empty brain is no
better than no brain at all.
"#,
        ninox_bin = ninox_bin,
    );
    fs::write(&brain_skill_path, brain_skill_content).await?;

    // subagent-blocker hook — always overwritten.
    let blocker = r#"#!/usr/bin/env node
const { readFileSync } = require("node:fs");
const callerType = process.env.NINOX_CALLER_TYPE || "";
if (callerType !== "orchestrator") process.exit(0);
let raw = "";
try { raw = readFileSync(0, "utf-8"); } catch { process.exit(0); }
let payload;
try { payload = JSON.parse(raw || "{}"); } catch { process.exit(0); }
const toolName = typeof payload.tool_name === "string" ? payload.tool_name : "";
if (toolName !== "Task" && toolName !== "Agent") process.exit(0);
const sub = (payload.tool_input?.subagent_type || "").toLowerCase();
if (sub === "explore" || sub === "plan") process.exit(0);
process.stdout.write(JSON.stringify({
  hookSpecificOutput: {
    hookEventName: "PreToolUse",
    permissionDecision: "deny",
    permissionDecisionReason: "Use `${NINOX_BIN:-ninox} spawn` instead of native subagents.",
  },
}) + "\n");
process.exit(0);
"#;
    fs::write(claude_dir.join("subagent-blocker.cjs"), blocker).await?;

    let settings_path = claude_dir.join("settings.json");
    if !settings_path.exists() {
        let settings = serde_json::json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Task|Agent",
                    "hooks": [{"type": "command", "command": "node .claude/subagent-blocker.cjs", "timeout": 2000}]
                }]
            },
            "statusLine": {
                "type": "command",
                "command": format!("'{}' statusline", ninox_bin.replace('\'', "'\\''")),
                "refreshInterval": 20
            }
        });
        fs::write(&settings_path, serde_json::to_string_pretty(&settings)?).await?;
    }

    Ok(())
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
        let task = Self::apply(&mut state, message);
        (state, task)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::{events::Engine, store::Store};
    use tempfile::tempdir;

    #[tokio::test]
    async fn coalesces_buffered_client_output_for_the_same_generation() {
        // Send three chunks before ever polling — all three are already
        // sitting in the channel by the time `next_coalesced_event` looks,
        // so one call must return them merged into a single event rather
        // than requiring three separate (and three separately rendered)
        // frames.
        let (tx, mut rx) = broadcast::channel(16);
        let mut pending = None;
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
        let mut pending = None;
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
        let mut pending = None;
        tx.send(Event::ClientOutput { session_id: "s1".into(), generation: 1, bytes: b"a".to_vec() }).unwrap();
        tx.send(Event::ClientOutput { session_id: "s2".into(), generation: 1, bytes: b"b".to_vec() }).unwrap();

        let first = next_coalesced_event(&mut rx, &mut pending).await.unwrap();
        assert!(matches!(&first, Event::ClientOutput { session_id, bytes, .. } if session_id == "s1" && bytes == b"a"));

        let second = next_coalesced_event(&mut rx, &mut pending).await.unwrap();
        assert!(matches!(&second, Event::ClientOutput { session_id, bytes, .. } if session_id == "s2" && bytes == b"b"));
    }

    fn pr_session(pr_number: Option<u64>, pr_id: Option<i64>, repo: &str) -> Session {
        Session {
            id: "s1".into(), orchestrator_id: None, name: "w".into(),
            repo: repo.into(), status: ninox_core::types::SessionStatus::PrOpen,
            agent_type: "claude-code".into(), cost_usd: 0.0, started_at: 0,
            pr_number, pr_id, workspace_path: None, pid: None,
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None,
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
            themes:             Themes::builtin(),
            scheme:             crate::theme::from_variant(ThemeVariant::Dark),
            active_variant:     ThemeVariant::Dark,
            orchestrator_root:  std::path::PathBuf::from("/tmp"),
            orchestrator_agent: ninox_core::config::AgentConfig::default(),
            orchestrators:      vec![],
            sessions:       HashMap::new(),
            brain,
            brain_view:     BrainViewState::default(),
            catalogues:      vec![ninox_core::config::CatalogueRef {
                name: "default".to_string(),
                path: std::path::PathBuf::new(),
            }],
            active_catalogue: 0,
            prs:            HashMap::new(),
            ci_status:      HashMap::new(),
            review_threads: HashMap::new(),
            diffs:          HashMap::new(),
            notifications:  VecDeque::new(),
            update_in_progress: false,
            version_check:  VersionCheckState::NotChecked,
            sidebar:        SidebarState::default(),
            view:           View::FleetBoard { scope: None },
            worker_panel:   Default::default(),
            terminals:      HashMap::new(),
            clients:        HashMap::new(),
            reattach_attempted: std::collections::HashSet::new(),
            next_client_generation: 0,
            model_lists:    HashMap::new(),
            settings:       Default::default(),
            spawn_modal:    None,
            catalogue_modal: None,
            terminal_cols:  140,
            terminal_rows:  50,
            window_width:   0.0,
            window_height:  0.0,
            sidebar_width:  0.0,
            info_width:     0.0,
            drag:            None,
            fleet_filter:    FleetFilter::default(),
            last_fleet_scope: None,
        }
    }

    fn refile_session(id: &str) -> Session {
        Session {
            id: id.into(), orchestrator_id: None, name: id.into(), repo: String::new(),
            status: SessionStatus::Terminated, agent_type: "claude-code".into(),
            cost_usd: 0.0, started_at: 0, pr_number: None, pr_id: None,
            workspace_path: Some("/tmp/ws".into()), pid: None,
            model: Some("claude-opus-4-8".into()), context_tokens: None,
            catalogue_path: Some("/brains/b".into()),
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None,
        }
    }

    #[test]
    fn refile_plan_resolves_agent_through_current_registry() {
        let mut cfg = ninox_core::config::AppConfig::default();
        // simulate a spec change since the session was filed
        cfg.harnesses.insert("claude-code".to_string(), ninox_core::harness::HarnessSpec {
            enabled: true,
            binary:  Some("claude-nightly".into()),
            interactive_args: vec!["--model".into(), "{model}".into()],
            ..Default::default()
        });
        let session = refile_session("s1");
        let plan = refile_plan(&session, false, &cfg, "fresh-uuid").expect("plan");
        assert_eq!(plan.base_cmd, "claude-nightly --model 'claude-opus-4-8'");
        assert_eq!(plan.workspace, "/tmp/ws");
        assert_eq!(plan.catalogue_path, "/brains/b");
        assert!(plan.extra_env.is_empty());
        assert_eq!(plan.agent.harness, "claude-code");
        assert_eq!(plan.agent.model.as_deref(), Some("claude-opus-4-8"));
    }

    #[test]
    fn refile_plan_orchestrator_gets_caller_env_and_no_workspace_means_no_plan() {
        let cfg = ninox_core::config::AppConfig::default();
        let mut session = refile_session("o1");
        session.catalogue_path = None;
        let plan = refile_plan(&session, true, &cfg, "fresh-uuid").expect("plan");
        assert!(plan.extra_env.iter().any(|(k, v)| k == "NINOX_ORCHESTRATOR_ID" && v == "o1"));
        assert!(plan.extra_env.iter().any(|(k, _)| k == "NINOX_CALLER_TYPE"));
        // The legacy ATHENE_/AO_ transition names are gone.
        assert!(!plan.extra_env.iter().any(|(k, _)| k == "ATHENE_CALLER_TYPE" || k == "AO_CALLER_TYPE"));
        // no recorded catalogue → default brain path
        assert!(!plan.catalogue_path.is_empty());

        session.workspace_path = None;
        assert!(refile_plan(&session, true, &cfg, "fresh-uuid").is_none());
    }

    #[test]
    fn refile_plan_uses_the_freshly_generated_id_not_the_stale_one() {
        let cfg = ninox_core::config::AppConfig::default();
        let mut session = refile_session("s1");
        session.claude_session_id = Some("stale-id-from-before".into());
        let plan = refile_plan(&session, false, &cfg, "brand-new-id").expect("plan");
        assert!(plan.base_cmd.contains("brand-new-id"));
        assert!(!plan.base_cmd.contains("stale-id-from-before"));
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
    fn resume_plan_builds_a_resume_command_from_the_stored_id() {
        let cfg = ninox_core::config::AppConfig::default();
        let mut session = refile_session("s1");
        session.claude_session_id = Some("stored-uuid".into());
        let plan = resume_plan(&session, false, &cfg).expect("plan");
        assert!(plan.base_cmd.contains("--resume"));
        assert!(plan.base_cmd.contains("stored-uuid"));
        assert_eq!(plan.workspace, "/tmp/ws");
    }

    #[test]
    fn resume_plan_none_without_a_stored_claude_session_id() {
        let cfg = ninox_core::config::AppConfig::default();
        let session = refile_session("s1"); // claude_session_id: None by default in this fixture
        assert!(resume_plan(&session, false, &cfg).is_none());
    }

    #[test]
    fn resume_message_relaunches_and_attaches() {
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

    #[test]
    fn resume_message_drops_the_cached_diff_from_before_the_pane_died() {
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

    #[test]
    fn navigate_settings_switches_view() {
        let m = base(test_engine());
        let (m, _) = m.update(Message::NavigateSettings);
        assert!(matches!(m.view, View::Settings));
        // and back out via the TOC
        let (m, _) = m.update(Message::NavigateFleet { scope: None });
        assert!(matches!(m.view, View::FleetBoard { .. }));
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
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None,
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
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None, gate_status: None,
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

    #[test]
    fn notifications_capped_at_50() {
        let e = test_engine();
        let mut m = base(e);
        for i in 0..55u32 {
            let (next, _) = m.update(Message::EngineEvent(Box::new(Event::Notification(Notification {
                id:         i.to_string(),
                kind:       NotificationKind::WorkerDone,
                title:      "t".into(),
                body:       "b".into(),
                session_id: None,
                created_at: 0,
            }))));
            m = next;
        }
        assert_eq!(m.notifications.len(), 50);
    }

    #[test]
    fn spawn_form_confirm_inserts_orchestrator_and_navigates() {
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
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None,
        };
        let (next, _) = m.update(Message::EngineEvent(Box::new(Event::SessionSpawned(s))));
        m = next;

        let (next, _) = m.update(Message::NavigateSession("sess-a".into()));
        m = next;
        assert!(matches!(&m.view, View::SessionDetail { panel: DetailPanel::Split, .. }));

        let (next, _) = m.update(Message::SwitchDetailPanel(DetailPanel::Terminal));
        m = next;
        let (next, _) = m.update(Message::NavigateFleet { scope: None });
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
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None,
        }).unwrap();
        let engine = Engine::new(store);
        let brain = Arc::new(BrainIndex::open(tempdir().unwrap().keep()).unwrap());
        let (app, _task) = App::new(engine, std::path::PathBuf::from("/tmp"), ninox_core::config::AgentConfig::default(), brain);
        assert_eq!(app.orchestrators.len(), 1);
        assert_eq!(app.sessions.len(), 1);
        assert!(app.sessions.contains_key("s1"));
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
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None,
        };
        let (m2, _) = m.update(Message::EngineEvent(Box::new(Event::SessionSpawned(s))));
        m = m2;
        // board_sessions(app, status, scope) returns sessions with that status
        let terminated = board_sessions(&m, &SessionStatus::Terminated, None);
        assert_eq!(terminated.len(), 1);
        assert_eq!(terminated[0].id, "t1");
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
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: Some(0), gate_status: None,
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
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None,
        });

        let (next, _) = m.update(Message::PollSessions);
        m = next;
        assert!(m.sessions.contains_key("o1"), "orchestrator sessions must never be auto-dropped");
    }

    #[test]
    fn poll_sessions_drops_the_cached_diff_once_the_session_is_purged_from_the_store() {
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
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: Some(0), gate_status: None,
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
    fn remove_session_drops_the_cached_diff() {
        let e = test_engine();
        let mut m = base(e);
        let s = refile_session("s1");
        m.sessions.insert("s1".into(), s);
        m.diffs.insert("s1".into(), Some("stale diff text".into()));
        let (m, _) = m.update(Message::RemoveSession("s1".into()));
        assert!(!m.diffs.contains_key("s1"));
    }

    #[test]
    fn remove_orchestrator_drops_cached_diffs_for_it_and_its_workers() {
        let e = test_engine();
        let mut m = base(e);
        let o = Orchestrator { id: "o1".into(), name: "orch".into(), created_at: 0 };
        let worker = Session {
            id: "w1".into(), orchestrator_id: Some("o1".into()), name: "worker".into(),
            repo: "r".into(), status: SessionStatus::Working,
            agent_type: "c".into(), cost_usd: 0.0,
            started_at: 0, pr_number: None, pr_id: None,
            workspace_path: None, pid: None,
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None, gate_status: None,
        };
        let (m2, _) = m.update(Message::EngineEvent(Box::new(Event::OrchestratorSpawned(o))));
        m = m2;
        let (m2, _) = m.update(Message::EngineEvent(Box::new(Event::SessionSpawned(worker))));
        m = m2;
        m.diffs.insert("o1".into(), Some("stale orch diff".into()));
        m.diffs.insert("w1".into(), Some("stale worker diff".into()));

        let (m, _) = m.update(Message::RemoveOrchestrator("o1".into()));
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
    fn navigate_pr_list_sets_view() {
        let e = test_engine();
        let m = base(e);
        let (m2, _) = m.update(Message::NavigatePrList);
        assert!(matches!(m2.view, View::PrList));
    }

    #[test]
    fn navigate_notification_routes_through_the_session_attach_path() {
        // NavigateNotification used to set state.view directly and never
        // attach a client or create a TerminalState, permanently stranding
        // the panel at "Terminal connecting…". It must now delegate to the
        // same attach path as NavigateSession — reattach_attempted.clear()
        // and the Terminal/Split client-retention side effects are unique
        // to that path, so seeing them here proves delegation happened.
        use crate::components::session_detail::DetailPanel;
        let e = test_engine();
        let mut m = base(e);
        m.reattach_attempted.insert("s1".into());
        m.sidebar.show_notifications = true;

        let (m2, _) = m.update(Message::NavigateNotification("s1".into()));

        assert!(!m2.sidebar.show_notifications);
        assert!(
            matches!(&m2.view, View::SessionDetail { session_id, panel: DetailPanel::Terminal }
                if session_id == "s1"),
            "must land on the session's Terminal panel"
        );
        assert!(
            !m2.reattach_attempted.contains("s1"),
            "must go through NavigateSession's attach path (which clears reattach_attempted), \
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

    #[test]
    fn brain_reindex_reloads_entries_from_disk() {
        let brain_dir = tempdir().unwrap().keep();
        let brain = Arc::new(BrainIndex::open(&brain_dir).unwrap());

        let e = test_engine();
        let m = base_with_brain(e, brain);
        let (m2, _) = m.update(Message::NavigateBrain);
        assert!(m2.brain_view.entries.is_empty());

        // A file appears on disk after the initial load (e.g. hand-edited outside the app).
        std::fs::create_dir_all(brain_dir.join("repos")).unwrap();
        std::fs::write(brain_dir.join("repos").join("ninox.md"), "Ninox repo notes.").unwrap();

        let (m3, _) = m2.update(Message::BrainReindex);
        assert_eq!(m3.brain_view.entries.len(), 1);
        assert_eq!(m3.brain_view.entries[0].id, "repos/ninox.md");
    }

    #[test]
    fn selecting_entry_opens_catalogue_and_drawer() {
        let brain_dir = tempdir().unwrap().keep();
        std::fs::create_dir_all(brain_dir.join("symbols")).unwrap();
        std::fs::write(brain_dir.join("symbols").join("x.md"), "---\nname: X\n---\nbody").unwrap();
        let brain = Arc::new(BrainIndex::open(&brain_dir).unwrap());
        brain.rebuild(None).unwrap();

        let e = test_engine();
        let app = base_with_brain(e, brain);
        let (app, _) = app.update(Message::NavigateBrain);
        let (app, _) = app.update(Message::BrainSetMode(BrainMode::Pinboard));
        let (app, _) = app.update(Message::BrainSelectEntry("symbols/x.md".into()));
        assert_eq!(app.brain_view.mode, BrainMode::Catalogue);
        assert!(app.brain_view.open_drawers.contains("symbols"));
        assert_eq!(app.brain_view.selected.as_deref(), Some("symbols/x.md"));
    }

    #[test]
    fn hover_entry_sets_and_clears_hovered() {
        let e = test_engine();
        let m = base(e);
        assert_eq!(m.brain_view.hovered, None);
        let (m2, _) = m.update(Message::BrainHoverEntry(Some("symbols/x.md".into())));
        assert_eq!(m2.brain_view.hovered.as_deref(), Some("symbols/x.md"));
        let (m3, _) = m2.update(Message::BrainHoverEntry(None));
        assert_eq!(m3.brain_view.hovered, None);
    }

    #[test]
    fn switching_mode_clears_hovered() {
        let e = test_engine();
        let m = base(e);
        let (m2, _) = m.update(Message::BrainHoverEntry(Some("symbols/x.md".into())));
        assert_eq!(m2.brain_view.hovered.as_deref(), Some("symbols/x.md"));
        let (m3, _) = m2.update(Message::BrainSetMode(BrainMode::Catalogue));
        assert_eq!(m3.brain_view.hovered, None);
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
            ninox_core::config::CatalogueRef { name: "default".into(), path: dir_a.clone() },
            ninox_core::config::CatalogueRef { name: "second".into(), path: dir_b.clone() },
        ];
        let (app, _) = app.update(Message::NavigateBrain);
        let (app, _) = app.update(Message::BrainSelectEntry("symbols/a.md".into()));
        assert_eq!(app.brain_view.selected.as_deref(), Some("symbols/a.md"));
        let (app, _) = app.update(Message::BrainHoverEntry(Some("symbols/a.md".into())));
        assert_eq!(app.brain_view.hovered.as_deref(), Some("symbols/a.md"));

        let (app, _) = app.update(Message::BrainSwitchCatalogue(1));
        assert_eq!(app.active_catalogue, 1);
        assert_eq!(app.brain_view.selected, None);
        assert_eq!(app.brain_view.hovered, None);
        assert!(app.brain_view.markdown.is_empty());
        assert!(app.brain_view.open_drawers.is_empty());
        assert_eq!(app.brain_view.entries.len(), 1);
        assert_eq!(app.brain_view.entries[0].id, "concepts/b.md");
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

    #[test]
    fn reindex_refreshes_backlinks_and_related_for_the_selected_entry() {
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

        let (m4, _) = m3.update(Message::BrainReindex);
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

    #[test]
    fn reindex_clears_selection_if_the_selected_entry_disappears() {
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

        let (m4, _) = m3.update(Message::BrainReindex);
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
            ninox_core::config::CatalogueRef { name: "default".into(), path: dir_a.clone() },
            ninox_core::config::CatalogueRef { name: "second".into(), path: dir_b.clone() },
        ];
        let (app, _) = app.update(Message::NavigateBrain);
        assert_eq!(
            app.brain_view.edges.len(),
            1,
            "alice<->bob's mutual link should resolve to one edge in catalogue A"
        );

        let (app, _) = app.update(Message::BrainSwitchCatalogue(1));
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

    #[test]
    fn dismiss_notification_removes_by_id() {
        let e = test_engine();
        let mut m = base(e);
        for id in ["n1", "n2", "n3"] {
            let (next, _) = m.update(Message::EngineEvent(Box::new(Event::Notification(Notification {
                id: id.into(), kind: NotificationKind::WorkerDone,
                title: "t".into(), body: "b".into(), session_id: None,
                created_at: 0,
            }))));
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
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None,
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
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None, gate_status: None,
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
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None,
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
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None,
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
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None,
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

        let e = test_engine();
        let mut m = base(e);
        m.window_width  = 1200.0;
        m.window_height = 800.0;
        m.sidebar_width = 220.0;
        m.info_width    = 300.0;

        let (cell_w, cell_h) = crate::components::terminal::cell_size(
            crate::components::terminal::FONT_SIZE,
        );
        let sidebar_w = m.sidebar_width + 5.0;
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
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None,
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
            let (next, _) = m.update(Message::EngineEvent(Box::new(Event::Notification(Notification {
                id: id.into(), kind: NotificationKind::WorkerDone,
                title: "t".into(), body: "b".into(), session_id: None,
                created_at: 0,
            }))));
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
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None,
            };
            let (next, _) = m.update(Message::EngineEvent(Box::new(Event::SessionSpawned(s))));
            m = next;
        }
        assert_eq!(attention_count(&m), 2); // ci_failed + review_pending
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
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None,
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
        let m = base(test_engine());
        let m = press(m, "2");
        assert!(matches!(m.view, View::PrList));
        let m = press(m, "3");
        assert!(matches!(m.view, View::Brain));
        let m = press(m, "1");
        assert!(matches!(m.view, View::FleetBoard { .. }));
    }

    /// Serializes tests that mutate process-global env vars (`NINOX_CONFIG`)
    /// against each other — `cargo test` runs test fns on parallel threads,
    /// so without this guard one test's env mutation could leak into
    /// another's read.
    static ENV_TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Set `key=value` for the duration of `f`, restoring the prior value
    /// (or unsetting it) afterward. Serialized via `ENV_TEST_GUARD` since
    /// env vars are process-global state shared across parallel test
    /// threads. Mirrors `ninox_core::config::tests::with_env_override`.
    fn with_env_override<T>(
        key: &str,
        value: impl AsRef<std::ffi::OsStr>,
        f: impl FnOnce() -> T,
    ) -> T {
        let _guard = ENV_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var(key).ok();
        std::env::set_var(key, value);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));

        match prior {
            Some(v) => std::env::set_var(key, v),
            None    => std::env::remove_var(key),
        }
        result.unwrap()
    }

    #[test]
    fn t_toggles_light_dark() {
        // `Message::SwitchTheme` calls `state.config.save()`, which writes to
        // `AppConfig::config_path()`. Redirect that to a tempfile so this
        // test never touches the real user config
        // (e.g. `~/Library/Application Support/ninox/config.toml`).
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("t_toggles_light_dark_config.toml");

        with_env_override("NINOX_CONFIG", &config_path, || {
            let m = base(test_engine());
            let before = m.active_variant;
            let m = press(m, "t");
            assert_ne!(m.active_variant, before);
            let m = press(m, "t");
            assert_eq!(m.active_variant, before);
        });
    }

    #[test]
    fn toggling_a_harness_enables_it_and_persists() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("toggle_harness_config.toml");
        with_env_override("NINOX_CONFIG", &config_path, || {
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
        });
    }

    #[test]
    fn claude_code_toggle_is_inert() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("claude_toggle_config.toml");
        with_env_override("NINOX_CONFIG", &config_path, || {
            let m = base(test_engine());
            let (m, _) = m.update(Message::SettingsToggleHarness("claude-code".into()));
            assert!(m.config.registry().enabled_names().contains(&"claude-code".to_string()));
            assert!(m.config.harnesses.is_empty(), "inert toggle must not write config");
        });
    }

    #[test]
    fn worker_harness_change_persists_and_clears_model() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("worker_harness_config.toml");
        with_env_override("NINOX_CONFIG", &config_path, || {
            let m = base(test_engine());
            let (m, _) = m.update(Message::SettingsWorkerModel("claude-haiku-4-5".into()));
            let (m, _) = m.update(Message::SettingsToggleHarness("codex".into()));
            let (m, _) = m.update(Message::SettingsWorkerHarness("codex".into()));
            assert_eq!(m.config.worker.harness, "codex");
            assert!(m.config.worker.model.is_none(), "stale model must not leak across harnesses");
            let loaded = ninox_core::config::AppConfig::load().unwrap();
            assert_eq!(loaded.worker.harness, "codex");
            assert!(loaded.worker.model.is_none());
        });
    }

    #[test]
    fn worker_model_pick_persists_and_custom_commits() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("worker_model_config.toml");
        with_env_override("NINOX_CONFIG", &config_path, || {
            let m = base(test_engine());
            let (m, _) = m.update(Message::SettingsWorkerModel("claude-haiku-4-5".into()));
            assert_eq!(m.config.worker.model.as_deref(), Some("claude-haiku-4-5"));

            // custom… opens the input (seeded with the current model)
            // without touching config
            let (m, _) = m.update(Message::SettingsWorkerModel(crate::models::CUSTOM_SENTINEL.into()));
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
            let (m, _) = m.update(Message::SettingsWorkerModel(crate::models::CUSTOM_SENTINEL.into()));
            let (m, _) = m.update(Message::SettingsWorkerCustomModel("   ".into()));
            let (m, _) = m.update(Message::SettingsWorkerCustomCommit);
            assert!(m.config.worker.model.is_none());
        });
    }

    #[test]
    fn spawn_confirm_remembers_agent_preselection() {
        // Confirming a spawn with a CHANGED agent choice persists it as the
        // `[orchestrator]` remembered preselection; redirect config writes
        // to a tempfile so the real user config is never touched.
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("remembered_preselection_config.toml");

        with_env_override("NINOX_CONFIG", &config_path, || {
            let mut m = base(test_engine());
            let (next, _) = m.update(Message::SpawnSession);
            m = next;
            let (next, _) = m.update(Message::SpawnFormName("remember-me".into()));
            m = next;
            let (next, _) = m.update(Message::SpawnFormModel("claude-haiku-4-5".into()));
            m = next;
            let (next, _) = m.update(Message::SpawnFormConfirm);
            m = next;

            assert_eq!(m.orchestrator_agent.model.as_deref(), Some("claude-haiku-4-5"));
            let loaded = ninox_core::config::AppConfig::load().unwrap();
            assert_eq!(loaded.orchestrator.harness, "claude-code");
            assert_eq!(loaded.orchestrator.model.as_deref(), Some("claude-haiku-4-5"));

            // Reopening the modal preselects the remembered choice.
            let (m, _) = m.update(Message::SpawnSession);
            assert_eq!(m.spawn_modal.as_ref().unwrap().model.as_deref(), Some("claude-haiku-4-5"));
        });
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

        with_env_override("NINOX_CONFIG", &config_path, || {
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
            assert_eq!(m.active_catalogue, idx, "active catalogue switches to the new one");
            assert!(
                catalogue_dir.join(".index.db").exists(),
                "brain index initialized on disk"
            );

            let saved = std::fs::read_to_string(&config_path).unwrap();
            assert!(saved.contains("ninox-dev"), "catalogue persisted to config.toml");
        });
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
        let (next, _) = m.update(Message::SpawnFormCatalogue(0));
        m = next;

        let f = m.spawn_modal.as_ref().unwrap();
        assert_eq!(f.name, "theme-tokens");
        assert_eq!(f.kind, SpawnKind::Standalone);
        assert_eq!(f.workspace, "~/proj/ninox");
        assert_eq!(f.harness, "codex");
        assert_eq!(f.model.as_deref(), Some("gpt-4o"));
        assert_eq!(f.catalogue_idx, 0);
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
        let (next, _) = m.update(Message::SpawnFormModel(crate::models::CUSTOM_SENTINEL.into()));
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

    #[test]
    fn standalone_without_workspace_cannot_confirm() {
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
        assert!(matches!(m.view, View::FleetBoard { .. }));
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
        use crate::components::spawn_modal::SpawnKind;

        let repo = init_git_repo();
        let missing = repo.join(".claude").join("worktrees").join("feat-existing");
        let missing_str = missing.to_string_lossy().to_string();

        // Pre-create the branch (and worktree), then remove just the
        // worktree registration so the path is missing again but the
        // branch survives — exercising the "already exists" fallback.
        crate::spawn_util::create_worktree_at(&repo, &missing, "feat-existing").unwrap();
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
        use crate::components::spawn_modal::SpawnKind;

        // The branch derived from the second missing path's final component
        // ("dup-branch") is already checked out in another worktree in the
        // same repo, so `create_worktree_at`'s fallback (checkout without
        // `-b`) fails too — a real git error create_worktree_at can surface.
        let repo = init_git_repo();
        crate::spawn_util::create_worktree_at(&repo, &repo.join("first"), "dup-branch").unwrap();
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
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None,
        };
        let (next, _) = m.update(Message::EngineEvent(Box::new(Event::SessionSpawned(s))));
        m = next;
        let (next, _) = m.update(Message::NavigateSession("sess-a".into()));
        m = next;

        let m = press(m, "1"); // must go to the terminal, not switch views
        assert!(matches!(m.view, View::SessionDetail { .. }));
    }

    #[test]
    fn spawn_modal_swallows_number_keys_without_navigating() {
        let m = base(test_engine());
        let (m, _) = m.update(Message::NavigatePrList);
        assert!(matches!(m.view, View::PrList));

        let (m, _) = m.update(Message::SpawnSession);
        assert!(m.spawn_modal.is_some());

        // While the modal is open, "1" must not fall through to the
        // fleet-board navigation shortcut — the modal swallows every key
        // except Escape.
        let m = press(m, "1");
        assert!(m.spawn_modal.is_some(), "modal must remain open");
        assert!(
            matches!(m.view, View::PrList),
            "\"1\" must not navigate while the spawn modal is open"
        );
    }

    #[tokio::test]
    async fn spawn_skill_teaches_work_request_handling() {
        let root = tempdir().unwrap().keep();
        setup_orchestrator_root(&root, "ninox", "/cfg.toml").await.unwrap();

        let skill = std::fs::read_to_string(
            root.join(".claude").join("skills").join("spawn-worker").join("SKILL.md"),
        ).unwrap();
        assert!(skill.starts_with("---\n"), "skill must start with YAML frontmatter");
        assert!(skill.contains("name: spawn-worker"));
        assert!(skill.contains("description:"));
        assert!(
            skill.contains("request-work"),
            "skill must explain the worker→orchestrator work-request channel"
        );
        assert!(
            skill.contains("spawn a new worker") || skill.contains("spawn a dedicated worker"),
            "skill must tell the orchestrator to spawn a worker for requested work"
        );
        assert!(
            skill.to_lowercase().contains("never") && skill.to_lowercase().contains("widen"),
            "skill must forbid widening an existing worker's scope"
        );
    }

    #[tokio::test]
    async fn set_agent_config_skill_has_frontmatter() {
        let root = tempdir().unwrap().keep();
        setup_orchestrator_root(&root, "ninox", "/cfg.toml").await.unwrap();

        let skill = std::fs::read_to_string(
            root.join(".claude").join("skills").join("set-agent-config").join("SKILL.md"),
        ).unwrap();
        assert!(skill.starts_with("---\n"), "skill must start with YAML frontmatter");
        assert!(skill.contains("name: set-agent-config"));
        assert!(skill.contains("description:"));
    }

    #[tokio::test]
    async fn setup_orchestrator_root_seeds_brain_skill() {
        let root = tempdir().unwrap().keep();
        setup_orchestrator_root(&root, "ninox", "/cfg.toml").await.unwrap();

        let skill_path = root.join(".claude").join("skills").join("brain").join("SKILL.md");
        let skill = std::fs::read_to_string(&skill_path).unwrap();
        assert!(skill.starts_with("---\n"), "skill must start with YAML frontmatter");
        assert!(skill.contains("name: brain"));
        assert!(skill.contains("description:"));
        assert!(skill.contains("ninox brain query"));
        assert!(skill.contains("ninox brain index"));
        assert!(skill.contains("ninox brain show"));
        assert!(skill.contains("blends keyword and semantic matches"));

        let agents_md = std::fs::read_to_string(root.join("AGENTS.md")).unwrap();
        assert!(
            agents_md.contains(&skill_path.display().to_string()),
            "AGENTS.md should point orchestrators at the brain skill"
        );
    }

    #[tokio::test]
    async fn setup_orchestrator_root_configures_statusline() {
        let root = tempdir().unwrap().keep();
        setup_orchestrator_root(&root, "/path/to/ninox", "/cfg.toml").await.unwrap();

        let settings: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(root.join(".claude").join("settings.json")).unwrap(),
        ).unwrap();
        assert_eq!(settings["statusLine"]["type"], "command");
        assert_eq!(settings["statusLine"]["command"], "'/path/to/ninox' statusline");
        assert_eq!(settings["statusLine"]["refreshInterval"], 20);
        // The existing subagent-blocker hook must still be present.
        assert!(settings["hooks"]["PreToolUse"].is_array());
    }

    #[tokio::test]
    async fn setup_orchestrator_root_never_overwrites_existing_settings_json() {
        let root = tempdir().unwrap().keep();
        let claude_dir = root.join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        std::fs::write(claude_dir.join("settings.json"), r#"{"userCustom": true}"#).unwrap();

        setup_orchestrator_root(&root, "ninox", "/cfg.toml").await.unwrap();

        let contents = std::fs::read_to_string(claude_dir.join("settings.json")).unwrap();
        assert_eq!(contents, r#"{"userCustom": true}"#, "pre-existing settings.json must be left byte-for-byte alone");
    }
}
