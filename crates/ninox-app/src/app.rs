use std::{collections::{HashMap, VecDeque}, sync::Arc, time::{SystemTime, UNIX_EPOCH}};

use ninox_core::{
    config::{AppConfig, ThemeVariant},
    events::{Engine, Event},
    slugify,
    types::*,
};
use iced::{Element, Subscription, Task, Theme};
use tokio::sync::broadcast;

use crate::{
    components::{session_detail::DetailPanel, spawn_modal::SpawnForm, terminal::TerminalState},
    theme::{from_variant, ColorScheme},
};

const MAX_NOTIFICATIONS: usize = 50;

// ---------------------------------------------------------------------------
// View state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct SidebarState {
    pub selected_orchestrator: Option<OrchestratorId>,
    pub show_theme_popout:     bool,
    pub show_notifications:    bool,
}

#[derive(Debug, Clone, Default)]
pub struct FleetFilter {
    pub query: String,
}

#[derive(Debug, Clone)]
pub enum View {
    FleetBoard { scope: Option<OrchestratorId> },
    SessionDetail { session_id: SessionId, panel: DetailPanel },
    PrList,
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
    pub scheme:             ColorScheme,
    pub active_variant:     ThemeVariant,
    pub orchestrator_root:  std::path::PathBuf,
    pub orchestrator_agent: ninox_core::config::AgentConfig,
    pub orchestrators:      Vec<Orchestrator>,
    pub sessions:        HashMap<SessionId, Session>,
    pub prs:             HashMap<PrId, PR>,
    pub ci_status:       HashMap<PrId, CIStatus>,
    pub review_threads:  HashMap<PrId, Vec<Comment>>,
    pub notifications:   VecDeque<Notification>,
    pub sidebar:         SidebarState,
    pub view:            View,
    pub terminals:       HashMap<SessionId, TerminalState>,
    pub spawn_modal:     Option<SpawnForm>,
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

#[derive(Debug, Clone)]
pub enum Message {
    EngineEvent(Event),
    NavigateFleet { scope: Option<OrchestratorId> },
    NavigateSession(SessionId),
    SelectOrchestrator(Option<OrchestratorId>),
    SpawnSession,
    SpawnFormName(String),
    SpawnFormConfirm,
    SpawnFormCancel,
    SwitchDetailPanel(crate::components::session_detail::DetailPanel),
    RemoveOrchestrator(OrchestratorId),
    RemoveSession(SessionId),
    SwitchTheme(ThemeVariant),
    ToggleThemePopout,
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
    ToggleNotifications,
    DismissNotification(String),
    DismissAllNotifications,
    NavigateNotification(SessionId),
    FleetFilterQuery(String),
    ClearFleetFilter,
    ScrollTerminal { session_id: SessionId, delta: i32 },
    JumpToLatest { session_id: SessionId },
    OpenUrl(String),
    Noop,
}

// ---------------------------------------------------------------------------
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

/// Convert a key event to terminal bytes.
/// `app_cursor`: true when the terminal has APP_CURSOR mode set — arrow keys
/// use `\x1bO[ABCD]` instead of `\x1b[[ABCD]` in that mode.
fn key_to_terminal_bytes(
    key: &iced::keyboard::Key,
    modifiers: iced::keyboard::Modifiers,
    text: Option<&str>,
    app_cursor: bool,
) -> Option<Vec<u8>> {
    use iced::keyboard::key::Named;
    use iced::keyboard::Key;

    // Ctrl+letter → caret notation (Ctrl+A=0x01 … Ctrl+Z=0x1A, Ctrl+[=ESC)
    if modifiers.control() {
        match key {
            Key::Character(c) => {
                if let Some(ch) = c.chars().next() {
                    let b = match ch {
                        'a'..='z' => Some(vec![(ch as u8) - b'a' + 1]),
                        'A'..='Z' => Some(vec![(ch as u8) - b'A' + 1]),
                        '[' => Some(b"\x1b".to_vec()),
                        '\\' => Some(b"\x1c".to_vec()),
                        ']' => Some(b"\x1d".to_vec()),
                        '^' | '6' => Some(b"\x1e".to_vec()),
                        '_' => Some(b"\x1f".to_vec()),
                        _ => None,
                    };
                    if b.is_some() { return b; }
                }
            }
            Key::Named(Named::Enter) => return Some(b"\r".to_vec()),
            _ => {}
        }
    }

    // Arrow keys: mode-sensitive
    let arr = if app_cursor { ("OA","OB","OC","OD") } else { ("[A","[B","[C","[D") };
    let esc = |s: &str| -> Vec<u8> { let mut v = b"\x1b".to_vec(); v.extend(s.as_bytes()); v };

    let bytes: Vec<u8> = match key {
        Key::Named(Named::Enter)      => b"\r".to_vec(),
        Key::Named(Named::Escape)     => b"\x1b".to_vec(),
        Key::Named(Named::Backspace)  => b"\x7f".to_vec(),
        Key::Named(Named::Delete)     => b"\x1b[3~".to_vec(),
        Key::Named(Named::Tab) if modifiers.shift() => b"\x1b[Z".to_vec(),
        Key::Named(Named::Tab)        => b"\t".to_vec(),
        Key::Named(Named::ArrowUp)    => esc(arr.0),
        Key::Named(Named::ArrowDown)  => esc(arr.1),
        Key::Named(Named::ArrowRight) => esc(arr.2),
        Key::Named(Named::ArrowLeft)  => esc(arr.3),
        Key::Named(Named::Home)       => b"\x1b[H".to_vec(),
        Key::Named(Named::End)        => b"\x1b[F".to_vec(),
        Key::Named(Named::PageUp)     => b"\x1b[5~".to_vec(),
        Key::Named(Named::PageDown)   => b"\x1b[6~".to_vec(),
        // Prefer `text` (the actual typed character, shift-resolved) over `c`
        // (the base logical key). On some platforms Key::Character holds the
        // unshifted key, so Shift+backtick would give c="`" instead of "~".
        Key::Character(c)             => text.map(|t| t.as_bytes().to_vec())
                                             .unwrap_or_else(|| c.as_str().as_bytes().to_vec()),
        _ => text.map(|t| t.as_bytes().to_vec()).unwrap_or_default(),
    };
    if bytes.is_empty() { None } else { Some(bytes) }
}

// ---------------------------------------------------------------------------
// Impl
// ---------------------------------------------------------------------------

impl App {
    pub fn new(engine: Arc<Engine>, orchestrator_root: std::path::PathBuf, orchestrator_agent: ninox_core::config::AgentConfig) -> (Self, Task<Message>) {
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
        let scheme = from_variant(config.theme);
        let active_variant = config.theme;

        let mut app = Self {
            engine:             engine.clone(),
            config,
            scheme,
            active_variant,
            orchestrator_root,
            orchestrator_agent,
            orchestrators,
            sessions,
            prs:            HashMap::new(),
            ci_status:      HashMap::new(),
            review_threads: HashMap::new(),
            notifications:  VecDeque::new(),
            sidebar:        SidebarState::default(),
            view:           View::default(),
            terminals:      HashMap::new(),
            spawn_modal:    None,
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

            for session in sessions {
                if matches!(
                    session.status,
                    SessionStatus::Done | SessionStatus::Terminated
                ) {
                    continue;
                }

                if !tmux::has_session(&session.id).await {
                    let mut dead = session.clone();
                    dead.status = SessionStatus::Terminated;
                    let _ = engine.store.upsert_session(&dead);
                    engine.emit(CoreEvent::SessionUpdated(dead));
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
    fn resize_terminals(state: &mut Self) -> Vec<(SessionId, u16, u16)> {
        let (cell_w, cell_h) = crate::components::terminal::cell_size(
            crate::components::terminal::FONT_SIZE,
        );
        let sidebar_w = state.sidebar_width + 5.0; // +5 for drag handle
        let header_h  = 80.0f32;
        let info_w    = state.info_width + 5.0; // +5 for drag handle

        // Background sizing: what any session shows once Split (the default
        // panel) is active — this is also the authoritative size recorded on
        // `state` for sessions that don't have a `TerminalState` yet.
        let bg_cols = ((state.window_width - sidebar_w - info_w).max(200.0) / cell_w) as u16;
        let bg_rows = ((state.window_height - header_h).max(100.0) / cell_h) as u16;
        state.terminal_cols = bg_cols;
        state.terminal_rows = bg_rows;

        // The actively-viewed session uses whatever panel it's actually
        // showing — only Split narrows the width; every other panel uses
        // the full (non-info-panel) width.
        let active = match &state.view {
            View::SessionDetail { session_id, panel: crate::components::session_detail::DetailPanel::Split } => {
                Some((session_id.clone(), bg_cols, bg_rows))
            }
            View::SessionDetail { session_id, .. } => {
                let cols = ((state.window_width - sidebar_w).max(200.0) / cell_w) as u16;
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

    /// Shared mutation logic.
    fn apply(state: &mut Self, message: Message) -> Task<Message> {
        match message {
            Message::EngineEvent(event) => Self::handle_engine_event(state, event),

            Message::NavigateFleet { scope } => {
                state.last_fleet_scope = scope.clone();
                state.view = View::FleetBoard { scope };
                Task::none()
            }

            Message::NavigateSession(id) => {
                state.view = View::SessionDetail {
                    session_id: id.clone(),
                    panel: DetailPanel::default(),
                };
                // `id` is now the active (Split-by-default) session, so reflow
                // every terminal: `id` gets sized for the panel it's about to
                // show, and whatever session was active before (if any) falls
                // back to the background/default-panel size instead of being
                // left at whatever width its old panel happened to need.
                let resized = Self::resize_terminals(state);
                // Capture current terminal dimensions before resetting so start_streaming
                // resizes tmux to match.
                let (cols, rows) = state.terminals.get(&id)
                    .map(|t| {
                        use alacritty_terminal::grid::Dimensions;
                        (
                            t.term.grid().columns() as u16,
                            t.term.grid().screen_lines() as u16,
                        )
                    })
                    .unwrap_or((state.terminal_cols, state.terminal_rows));
                // Reset the terminal grid so capture_pane output is applied to a clean
                // slate. Without this, capture_pane (which has no cursor-home sequence)
                // renders on top of stale cursor state and garbles the output.
                state.terminals.remove(&id);
                let engine = state.engine.clone();
                // Sync tmux for every *other* session resize_terminals just
                // touched — `id`'s own pane is resized below via start_streaming.
                let other_sessions: Vec<(SessionId, u16, u16)> =
                    resized.into_iter().filter(|(sid, ..)| sid != &id).collect();
                Task::future(async move {
                    for (sid, cols, rows) in other_sessions {
                        let _ = ninox_core::tmux::resize_window(&sid, cols, rows).await;
                    }
                    if ninox_core::tmux::has_session(&id).await {
                        if let Err(e) =
                            ninox_core::pty::start_streaming(engine.clone(), id.clone(), &id, cols, rows).await
                        {
                            tracing::warn!("PTY (re)connect for {id}: {e}");
                        }
                    } else {
                        // Session's tmux process is gone — mark it terminated so the
                        // UI shows "Session exited" instead of "Terminal connecting…".
                        if let Ok(Some(mut s)) = engine.store.get_session(&id) {
                            s.status = ninox_core::types::SessionStatus::Terminated;
                            let _ = engine.store.upsert_session(&s);
                            engine.emit(ninox_core::events::Event::SessionUpdated(s));
                        }
                    }
                    Message::Noop
                })
            }

            Message::SelectOrchestrator(id) => {
                state.sidebar.selected_orchestrator = id;
                Task::none()
            }

            Message::SpawnSession => {
                state.spawn_modal = Some(SpawnForm::default());
                Task::none()
            }

            Message::SpawnFormName(v) => {
                if let Some(f) = &mut state.spawn_modal { f.name = v; }
                Task::none()
            }

            Message::SpawnFormCancel => {
                state.spawn_modal = None;
                Task::none()
            }

            Message::SpawnFormConfirm => {
                if let Some(form) = state.spawn_modal.take() {
                    let name = form.name.trim().to_string();
                    if name.is_empty() {
                        return Task::none();
                    }

                    let ts = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis();

                    let slug = slugify(&name);
                    let orch_id = if slug.is_empty() { format!("orch-{ts}") } else { slug };
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
                        agent_type:      state.orchestrator_agent.harness.clone(),
                        cost_usd:        0.0,
                        started_at:      ts as i64,
                        pr_number:       None,
                        pr_id:           None,
                        workspace_path:  Some(ws.clone()),
                        pid:             None,
                    };
                    let _ = state.engine.store.upsert_session(&session);
                    state.sessions.insert(session.id.clone(), session.clone());
                    state.engine.emit(Event::SessionSpawned(session));

                    state.view = View::SessionDetail {
                        session_id: orch.id.clone(),
                        panel:      DetailPanel::Terminal,
                    };

                    let engine     = state.engine.clone();
                    let tmux_id    = orch.id.clone();
                    let sid        = orch.id.clone();
                    let nm         = name;
                    let ts_i64     = ts as i64;
                    let orch_agent = state.orchestrator_agent.clone();
                    let t_cols     = state.terminal_cols;
                    let t_rows     = state.terminal_rows;

                    return Task::future(async move {
                        use ninox_core::{pty, tmux, Event as CoreEvent, Session, SessionStatus};

                        if let Err(e) = tokio::fs::create_dir_all(&ws).await {
                            tracing::error!("mkdir orchestrator workspace {ws}: {e}");
                        }

                        let ninox_bin = std::env::current_exe()
                            .ok()
                            .and_then(|p| p.to_str().map(str::to_string))
                            .unwrap_or_else(|| "ninox".to_string());

                        let ninox_config = ninox_core::config::AppConfig::config_path()
                            .to_string_lossy()
                            .to_string();

                        let env = [
                            ("NINOX_BIN",             ninox_bin.as_str()),
                            ("NINOX_CONFIG",          ninox_config.as_str()),
                            ("NINOX_ORCHESTRATOR_ID", sid.as_str()),
                            ("AO_CALLER_TYPE",         "orchestrator"),
                            ("ATHENE_CALLER_TYPE",     "orchestrator"),
                        ];

                        // Prepend ninox bin dir inside the shell command so the
                        // `ninox` shim (which points to the current binary) is found
                        // first — even after login-shell rc files reorder PATH.
                        let ninox_bin_dir = ninox_core::config::AppConfig::ninox_bin_dir();
                        let ninox_bin_dir_str = ninox_bin_dir.display().to_string()
                            .replace('\'', "'\\''");
                        let base_cmd = orch_agent.interactive_cmd();
                        let launch_cmd = format!(
                            "export PATH='{ninox_bin_dir_str}':\"$PATH\"; {base_cmd}"
                        );
                        if let Err(e) = tmux::create_session(&tmux_id, &ws, &launch_cmd, &env).await {
                            tracing::error!("tmux create failed for {sid}: {e}");
                            return Message::Noop;
                        }

                        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

                        let pid = tmux::list_sessions()
                            .await
                            .ok()
                            .and_then(|ss| ss.into_iter().find(|s| s.id == tmux_id))
                            .and_then(|s| s.pid);

                        let updated = Session {
                            id:              sid.clone(),
                            orchestrator_id: None,
                            name:            nm,
                            repo:            String::new(),
                            status:          SessionStatus::Working,
                            agent_type:      orch_agent.harness.clone(),
                            cost_usd:        0.0,
                            started_at:      ts_i64,
                            pr_number:       None,
                            pr_id:           None,
                            workspace_path:  Some(ws),
                            pid,
                        };
                        let _ = engine.store.upsert_session(&updated);

                        if let Err(e) = pty::start_streaming(engine.clone(), sid.clone(), &tmux_id, t_cols, t_rows).await {
                            tracing::error!("pty setup failed for {sid}: {e}");
                        }

                        engine.emit(CoreEvent::SessionUpdated(updated));
                        Message::Noop
                    });
                }
                Task::none()
            }

            Message::SwitchDetailPanel(new_panel) => {
                if let View::SessionDetail { panel, .. } = &mut state.view {
                    *panel = new_panel;
                }
                // Entering/leaving Split changes how much width the terminal
                // canvas actually has, so the grid must be reflowed to match.
                let resized = Self::resize_terminals(state);
                if resized.is_empty() {
                    return Task::none();
                }
                Task::future(async move {
                    for (sid, cols, rows) in resized {
                        let _ = ninox_core::tmux::resize_window(&sid, cols, rows).await;
                    }
                    Message::Noop
                })
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
                if state.sidebar.selected_orchestrator.as_deref() == Some(id.as_str()) {
                    state.sidebar.selected_orchestrator = None;
                }
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
                let engine = state.engine.clone();
                Task::future(async move {
                    if let Err(e) = engine.remove_session(&id).await {
                        tracing::error!("remove session {id}: {e}");
                    }
                    Message::Noop
                })
            }

            Message::RawKey { key, modifiers, text } => {
                if let View::SessionDetail {
                    session_id,
                    panel: crate::components::session_detail::DetailPanel::Terminal
                        | crate::components::session_detail::DetailPanel::Split,
                } = &state.view {
                    let app_cursor = state.terminals.get(session_id)
                        .map(|t| t.term.mode().contains(
                            alacritty_terminal::term::TermMode::APP_CURSOR
                        ))
                        .unwrap_or(false);
                    let Some(bytes) = key_to_terminal_bytes(&key, modifiers, text.as_deref(), app_cursor)
                        else { return Task::none(); };
                    let session_id = session_id.clone();
                    let engine = state.engine.clone();
                    return Task::future(async move {
                        if let Some(sender) = engine.get_pty_writer(&session_id).await {
                            let _ = sender.send(bytes);
                        }
                        Message::Noop
                    });
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
                if resized.is_empty() {
                    return Task::none();
                }
                Task::future(async move {
                    for (sid, cols, rows) in resized {
                        let _ = ninox_core::tmux::resize_window(&sid, cols, rows).await;
                    }
                    Message::Noop
                })
            }


            Message::ToggleThemePopout => {
                state.sidebar.show_theme_popout = !state.sidebar.show_theme_popout;
                Task::none()
            }

            Message::SwitchTheme(variant) => {
                state.active_variant = variant;
                state.scheme = from_variant(variant);
                state.config.theme = variant;
                state.sidebar.show_theme_popout = false;
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
                    if !resized.is_empty() {
                        return Task::future(async move {
                            for (sid, cols, rows) in resized {
                                let _ = ninox_core::tmux::resize_window(&sid, cols, rows).await;
                            }
                            Message::Noop
                        });
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

                // Remove terminated/done sessions from state and DB — includes
                // both standalone sessions and workers under orchestrators.
                let to_clean: Vec<SessionId> = state.sessions.values()
                    .filter(|s| {
                        matches!(s.status, SessionStatus::Done | SessionStatus::Terminated)
                        && !orch_ids.contains(s.id.as_str())
                    })
                    .map(|s| s.id.clone())
                    .collect();
                for id in &to_clean {
                    state.sessions.remove(id);
                    state.terminals.remove(id);
                }
                let engine_clean = state.engine.clone();
                let to_clean_clone = to_clean.clone();

                // Add genuinely new active sessions (spawned by ninox spawn).
                // PTY streaming is NOT started here — NavigateSession handles that
                // on demand with the correct window dimensions.
                for session in db_sessions {
                    if matches!(session.status, SessionStatus::Done | SessionStatus::Terminated) {
                        continue;
                    }
                    if !state.sessions.contains_key(&session.id) {
                        state.sessions.insert(session.id.clone(), session);
                    }
                }

                Task::future(async move {
                    for id in to_clean_clone {
                        let _ = engine_clean.store.delete_session(&id);
                    }
                    Message::Noop
                })
            }

            Message::NavigatePrList => {
                state.view = View::PrList;
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
                state.view = View::SessionDetail {
                    session_id,
                    panel: crate::components::session_detail::DetailPanel::Terminal,
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

            Message::ScrollTerminal { session_id, delta } => {
                if let Some(term) = state.terminals.get_mut(&session_id) {
                    term.scroll(delta);
                }
                Task::none()
            }

            Message::JumpToLatest { session_id } => {
                if let Some(term) = state.terminals.get_mut(&session_id) {
                    term.scroll_to_bottom();
                }
                Task::none()
            }

            Message::OpenUrl(url) => {
                let _ = std::process::Command::new("open").arg(&url).spawn();
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
            }

            Event::OrchestratorRemoved(id) => {
                state.orchestrators.retain(|o| o.id != id);
                state.sessions.retain(|_, s| s.orchestrator_id.as_deref() != Some(id.as_str()));
            }

            Event::SessionSpawned(session) => {
                state.sessions.insert(session.id.clone(), session);
            }

            Event::SessionUpdated(session) => {
                state.sessions.insert(session.id.clone(), session);
            }

            Event::SessionDone(id) => {
                if let Some(s) = state.sessions.get_mut(&id) {
                    s.status = SessionStatus::Done;
                }
                state.terminals.remove(&id);
            }

            Event::TerminalOutput { session_id, bytes } => {
                let term = state.terminals
                    .entry(session_id)
                    .or_insert_with(|| crate::components::terminal::TerminalState::new(
                        state.terminal_cols, state.terminal_rows, None,
                    ));
                term.process(&bytes);
            }

            Event::CiUpdated { pr_id, status } => {
                state.ci_status.insert(pr_id, status);
            }

            Event::PrOpened { session_id, pr } => {
                if let Some(s) = state.sessions.get_mut(&session_id) {
                    s.pr_number = Some(pr.number);
                    s.pr_id     = Some(pr.id);
                }
                state.prs.insert(pr.id, pr);
            }

            Event::ReviewComment { pr_id, comment } => {
                state.review_threads
                    .entry(pr_id)
                    .or_default()
                    .push(comment);
            }

            // AttachedClient rendering stream: wired into the UI by a later task.
            Event::ClientOutput { .. } => {}
            Event::ClientClosed { .. } => {}

            Event::Notification(n) => {
                let title = n.title.clone();
                let body = n.body.clone();
                std::thread::spawn(move || {
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
        }
        Task::none()
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
            fleet_board::fleet_board,
            pr_list::pr_list,
            session_detail::session_detail,
            sidebar::sidebar,
            spawn_modal::spawn_modal,
        };
        use iced::{Background, Length};

        let bg = state.scheme.bg_base;
        let main: Element<Message> = match &state.view {
            View::FleetBoard { scope } => fleet_board(state, scope.as_ref()),
            View::SessionDetail { session_id, panel } => session_detail(state, session_id, panel),
            View::PrList => pr_list(state),
        };

        let base: Element<Message> = container(
            row![
                sidebar(state),
                App::drag_handle(DragTarget::Sidebar, state.scheme.border),
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

        if let Some(form) = &state.spawn_modal {
            iced::widget::stack![base, spawn_modal(form, &state.scheme)].into()
        } else {
            base
        }
    }

    /// Subscription that drives `Message::EngineEvent` from the engine broadcast channel.
    pub fn subscription(state: &Self) -> Subscription<Message> {
        let mut rx: broadcast::Receiver<Event> = state.engine.subscribe();
        let engine_sub = Subscription::run_with_id(
            "engine-events",
            async_stream::stream! {
                loop {
                    match rx.recv().await {
                        Ok(event)  => yield Message::EngineEvent(event),
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed)    => break,
                    }
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

/// Seeds `~/.config/ninox/orchestrator/` (or the configured root) with the
/// files that orchestrator sessions need: AGENTS.md (canonical, CLAUDE.md
/// symlinks to it), spawn-worker skill, set-agent-config skill, and the
/// subagent-blocker PreToolUse hook.
///
/// AGENTS.md and settings.json are skipped if already present (user-editable).
/// Skill files and the blocker are always overwritten to stay in sync.
pub async fn setup_orchestrator_root(
    root: &std::path::Path,
    ninox_bin: &str,
    config_path: &str,
) -> anyhow::Result<()> {
    use tokio::fs;

    let claude_dir       = root.join(".claude");
    let spawn_skill_dir  = root.join("skills").join("spawn-worker");
    let config_skill_dir = root.join("skills").join("set-agent-config");
    fs::create_dir_all(&claude_dir).await?;
    fs::create_dir_all(&spawn_skill_dir).await?;
    fs::create_dir_all(&config_skill_dir).await?;

    let spawn_skill_path  = spawn_skill_dir.join("SKILL.md");
    let config_skill_path = config_skill_dir.join("SKILL.md");

    // AGENTS.md is canonical; CLAUDE.md symlinks to it.
    let agents_md_path = root.join("AGENTS.md");
    if !agents_md_path.exists() {
        let body = format!(
            "# Ninox Orchestrator\n\n\
             Before doing anything else, read and follow: `{spawn_skill}`\n\n\
             ## Available Skills\n\n\
             - `{spawn_skill}` — spawning worker sessions\n\
             - `{config_skill}` — changing agent harness or model\n",
            spawn_skill  = spawn_skill_path.display(),
            config_skill = config_skill_path.display(),
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
        r#"# Spawn a Worker, Not a Subagent

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
        r#"# Set Ninox Agent Config

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

    // subagent-blocker hook — always overwritten.
    let blocker = r#"#!/usr/bin/env node
const { readFileSync } = require("node:fs");
const callerType = process.env.ATHENE_CALLER_TYPE || process.env.AO_CALLER_TYPE || "";
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
            }
        });
        fs::write(&settings_path, serde_json::to_string_pretty(&settings)?).await?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

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

    fn test_engine() -> Arc<Engine> {
        let s = Arc::new(
            Store::open(tempdir().unwrap().keep().join("t.db")).unwrap(),
        );
        Engine::new(s)
    }

    fn base(engine: Arc<Engine>) -> App {
        App {
            engine,
            config:             AppConfig::default(),
            scheme:             from_variant(ThemeVariant::Dark),
            active_variant:     ThemeVariant::Dark,
            orchestrator_root:  std::path::PathBuf::from("/tmp"),
            orchestrator_agent: ninox_core::config::AgentConfig::default(),
            orchestrators:      vec![],
            sessions:       HashMap::new(),
            prs:            HashMap::new(),
            ci_status:      HashMap::new(),
            review_threads: HashMap::new(),
            notifications:  VecDeque::new(),
            sidebar:        SidebarState::default(),
            view:           View::FleetBoard { scope: None },
            terminals:      HashMap::new(),
            spawn_modal:    None,
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
        };
        let (updated, _) = m.update(Message::EngineEvent(Event::SessionSpawned(s)));
        assert!(updated.sessions.contains_key("s1"));
    }

    #[test]
    fn notifications_capped_at_50() {
        let e = test_engine();
        let mut m = base(e);
        for i in 0..55u32 {
            let (next, _) = m.update(Message::EngineEvent(Event::Notification(Notification {
                id:         i.to_string(),
                kind:       NotificationKind::WorkerDone,
                title:      "t".into(),
                body:       "b".into(),
                session_id: None,
            })));
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
        }).unwrap();
        let engine = Engine::new(store);
        let (app, _task) = App::new(engine, std::path::PathBuf::from("/tmp"), ninox_core::config::AgentConfig::default());
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
        };
        let (m2, _) = m.update(Message::EngineEvent(Event::SessionSpawned(s)));
        m = m2;
        // board_sessions(app, status, scope) returns sessions with that status
        let terminated = board_sessions(&m, &SessionStatus::Terminated, None);
        assert_eq!(terminated.len(), 1);
        assert_eq!(terminated[0].id, "t1");
    }

    #[test]
    fn poll_sessions_removes_done_worker_from_sidebar() {
        let e = test_engine();
        let mut m = base(e);

        // Set up an orchestrator
        let o = Orchestrator { id: "o1".into(), name: "orch".into(), created_at: 0 };
        let _ = m.engine.store.upsert_orchestrator(&o);
        let (next, _) = m.update(Message::EngineEvent(Event::OrchestratorSpawned(o)));
        m = next;

        // Worker session under the orchestrator, already Done
        let worker = Session {
            id: "w1".into(), orchestrator_id: Some("o1".into()), name: "worker".into(),
            repo: "r".into(), status: SessionStatus::Done,
            agent_type: "c".into(), cost_usd: 0.0,
            started_at: 0, pr_number: None, pr_id: None,
            workspace_path: None, pid: None,
        };
        let _ = m.engine.store.upsert_session(&worker);
        let (next, _) = m.update(Message::EngineEvent(Event::SessionSpawned(worker)));
        m = next;
        assert!(m.sessions.contains_key("w1"), "worker should be present before poll");

        // PollSessions should clean up the done worker
        let (next, _) = m.update(Message::PollSessions);
        m = next;
        assert!(!m.sessions.contains_key("w1"), "done worker must be removed by PollSessions");
    }

    #[test]
    fn navigate_pr_list_sets_view() {
        let e = test_engine();
        let m = base(e);
        let (m2, _) = m.update(Message::NavigatePrList);
        assert!(matches!(m2.view, View::PrList));
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
            let (next, _) = m.update(Message::EngineEvent(Event::Notification(Notification {
                id: id.into(), kind: NotificationKind::WorkerDone,
                title: "t".into(), body: "b".into(), session_id: None,
            })));
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
        };
        let (m, _) = m.update(Message::EngineEvent(Event::SessionSpawned(s)));
        let (m2, _) = m.update(Message::NavigateSession("s1".into()));
        let (m3, _) = m2.update(Message::SwitchDetailPanel(DetailPanel::Inspector));
        assert!(matches!(&m3.view, View::SessionDetail { panel: DetailPanel::Inspector, .. }));
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
            };
            let (next, _) = m.update(Message::EngineEvent(Event::SessionSpawned(s)));
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
        };
        let (m, _) = m.update(Message::EngineEvent(Event::SessionSpawned(s)));
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
            };
            let (next, _) = m.update(Message::EngineEvent(Event::SessionSpawned(s)));
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
    fn dismiss_all_clears_notifications() {
        let e = test_engine();
        let mut m = base(e);
        for id in ["a", "b"] {
            let (next, _) = m.update(Message::EngineEvent(Event::Notification(Notification {
                id: id.into(), kind: NotificationKind::WorkerDone,
                title: "t".into(), body: "b".into(), session_id: None,
            })));
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
            };
            let (next, _) = m.update(Message::EngineEvent(Event::SessionSpawned(s)));
            m = next;
        }
        assert_eq!(attention_count(&m), 2); // ci_failed + review_pending
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
            };
            let (next, _) = m.update(Message::EngineEvent(Event::SessionSpawned(s)));
            m = next;
        }
        let (m2, _) = m.update(Message::FleetFilterQuery("auth".into()));
        let sessions = filtered_sessions(&m2);
        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().all(|s| s.name.contains("auth")));
    }
}
