//! Terminal UI: fleet sidebar + live agent panes composited client-side
//! from ptyd screens (spec §4). The loop owns all I/O; `state` is the pure
//! reducer, `view` the pure renderer, `live` the off-loop pane plumbing.

pub mod backend;
mod brain;
mod brain_flow;
mod fleet;
mod idle;
mod inspector;
mod island_flow;
mod kanban;
mod keys;
mod layout;
mod live;
mod markdown;
mod palette;
mod pane;
pub mod pr_chips;
mod remotes;
#[cfg(debug_assertions)]
mod reply_fixture;
mod report;
mod settings;
mod sources;
mod spawn;
pub mod state;
mod user_requests;
mod worker_types;
mod workspaces_panel;
mod view;

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ninox_ui_shared::editor;
use ninox_ui_shared::prompt_agent::reply::{self as prompt_reply, AnswerSource, PromptPoll};
use ninox_core::{config::AppConfig, events::Engine, store::Store};
use ratatui::layout::Rect;
use ninox_ui_shared::brain_sources::SourceEditForm;
use state::{Action, Level, Modal, PtydState, Row, TuiState, View};

type SpawnHandle = tokio::task::JoinHandle<anyhow::Result<ninox_ui_shared::cli::SpawnedSession>>;
type ResumeHandle = (String, tokio::task::JoinHandle<Result<Option<String>, String>>);
type Term = ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>;

/// Panics on runtime worker threads (pane tasks) must not tear down the
/// screen; they are parked here and surfaced as a notice instead.
static BACKGROUND_PANIC: Mutex<Option<String>> = Mutex::new(None);

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

const REPLY_FIXTURE_VAR: &str = "NINOX_TUI_REPLY_FIXTURE";

/// Seeds the reply panel for headless captures (`reply_fixture`); compiled
/// out of release builds, where it never seeds. Returns whether it did.
fn seed_reply_fixture(st: &mut TuiState, mode: Option<&str>) -> bool {
    #[cfg(debug_assertions)]
    if let Some(mode) = mode {
        reply_fixture::seed(&mut st.fleet.reply, mode, st.now_ms);
        return true;
    }
    let _ = (st, mode);
    false
}

pub async fn run(store: Arc<Store>, port: u16, db_path: PathBuf) -> anyhow::Result<()> {
    let (config, config_warning) = match AppConfig::load_detailed() {
        Ok(cfg) => (cfg, None),
        Err(e) => (AppConfig::default(), Some(e.tui_footer_message())),
    };
    let engine = Engine::new(Arc::clone(&store));
    let mut st = TuiState { now_ms: now_ms(), config_warning, ..Default::default() };
    st.started_ms = st.now_ms;
    seed_reply_fixture(&mut st, std::env::var(REPLY_FIXTURE_VAR).ok().as_deref());
    st.prefix = match config.tui.prefix_byte() {
        Ok(b) => b,
        Err(e) => {
            st.notify(Level::Warn, format!("[tui] {e} — using {}", ninox_core::config::DEFAULT_PREFIX));
            ninox_core::config::DEFAULT_PREFIX_BYTE
        }
    };
    if let Some(note) = prefix_notice(st.prefix) {
        st.deferred_notice = Some((Level::Info, note));
    }
    // Degrade like a failed daemon spawn rather than aborting the TUI —
    // matches run_orchestrate's graceful handling of the same lookup.
    st.daemon_up = match ninox_core::hooks::canonical_exe() {
        Ok(exe) => match ninox_core::daemon::ensure_daemon(port, &exe, &db_path).await {
            ninox_core::daemon::DaemonStatus::AlreadyRunning => {
                // The footer banner (#319's server alerts) shows it too; this
                // puts it in the log and the notice line.
                if let Some(warning) = ninox_ui_shared::server_restart::check_daemon_build(port).await {
                    st.notify(Level::Warn, warning);
                }
                true
            }
            ninox_core::daemon::DaemonStatus::Failed(e) => {
                st.notify(Level::Error, format!("daemon down: {e}"));
                false
            }
            // Spawned but not yet listening (cold start); the tick re-probe
            // flips daemon_up as soon as it binds.
            ninox_core::daemon::DaemonStatus::Starting => {
                st.notify(Level::Info, "daemon starting…");
                false
            }
            _ => true,
        },
        Err(e) => {
            st.notify(Level::Error, format!("daemon down: could not resolve ninox binary: {e}"));
            false
        }
    };
    st.palette = palette::Palette::load(&config);
    st.in_tmux = std::env::var("TMUX").is_ok();
    // Registers this process as a trusted `HumanConsoleIdentity` so the
    // pending-requests screen's `answer_user_request` call verifies —
    // without this, every async answer attempt fails verification. A
    // detached/unusual launch should degrade to "can't answer" rather than
    // refusing to start the TUI at all.
    if let Err(e) = ninox_core::human_console_auth::register_human_console(&store, "ninox-tui") {
        tracing::warn!("failed to register ninox-tui as a human console: {e}");
        st.notify(Level::Error, format!("can't answer user-requests from this TUI: {e}"));
        st.human_console_error = Some(e.to_string());
    }
    st.restore_policy = backend::restore_policy(&config);
    st.reset_cfg = config.orchestrator_reset.clone();
    st.offer_restore(backend::pending_restore(&store));
    // Off the loop: the host may need spawning, and a panic in it (or a
    // slow socket) must not take the UI down with it.
    let start = backend::ptyd_starts_at_launch();
    let ptyd_boot = tokio::spawn(backend::ensure_ptyd(start));

    let mut terminal = enter_terminal()?;
    let res = event_loop(&mut terminal, &mut st, &store, &engine, port, (ptyd_boot, start), db_path).await;
    leave_terminal(&mut terminal)?;
    let host_ours = HOST_FOR_VIEWERS.load(Ordering::SeqCst)
        && ninox_core::runtime::configured_backend() == ninox_core::runtime::Backend::Tmux;
    live::kill_own_viewers(&backend::ptyd_socket(), host_ours).await;
    res
}

/// This TUI spawned the running ptyd host only to show tmux sessions, so
/// it may stop it on exit (see `live::kill_own_viewers`).
static HOST_FOR_VIEWERS: AtomicBool = AtomicBool::new(false);

/// Probed once while raw mode is on: probing again at teardown (raw mode
/// off) would echo the terminal's reply onto the user's shell.
static KEYBOARD_ENHANCED: AtomicBool = AtomicBool::new(false);

fn enable_modes(out: &mut impl Write) -> std::io::Result<()> {
    use crossterm::{event, execute, terminal};
    execute!(out, terminal::EnterAlternateScreen, event::EnableMouseCapture, event::EnableBracketedPaste)?;
    if matches!(terminal::supports_keyboard_enhancement(), Ok(true)) {
        // Lets Shift+Enter, Ctrl+I vs Tab, etc. reach agents distinctly.
        execute!(out, event::PushKeyboardEnhancementFlags(event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES))?;
        KEYBOARD_ENHANCED.store(true, Ordering::SeqCst);
    }
    Ok(())
}

fn disable_modes(out: &mut impl Write) {
    use crossterm::{event, execute, terminal};
    if KEYBOARD_ENHANCED.swap(false, Ordering::SeqCst) {
        let _ = execute!(out, event::PopKeyboardEnhancementFlags);
    }
    let _ = execute!(out, event::DisableBracketedPaste, event::DisableMouseCapture, terminal::LeaveAlternateScreen);
}

fn enter_terminal() -> anyhow::Result<Term> {
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
    // Hook first, so a panic anywhere after raw mode is enabled — including
    // the rest of this setup — still restores the terminal.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().name() != Some("main") {
            if let Ok(mut slot) = BACKGROUND_PANIC.lock() {
                *slot = Some(info.to_string());
            }
            return;
        }
        let _ = disable_raw_mode();
        disable_modes(&mut std::io::stdout());
        hook(info);
    }));
    enable_raw_mode()?;
    let setup = || -> anyhow::Result<_> {
        let mut stdout = std::io::stdout();
        enable_modes(&mut stdout)?;
        Ok(ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(stdout))?)
    };
    match setup() {
        Ok(terminal) => Ok(terminal),
        Err(e) => {
            // Undo raw mode (and the alternate screen, in case it was
            // entered) before surfacing the error — otherwise the user is
            // left in a no-echo TTY with only a blind `reset` to escape.
            let _ = disable_raw_mode();
            disable_modes(&mut std::io::stdout());
            Err(e)
        }
    }
}

fn leave_terminal(terminal: &mut Term) -> anyhow::Result<()> {
    crossterm::terminal::disable_raw_mode()?;
    disable_modes(terminal.backend_mut());
    terminal.show_cursor()?;
    Ok(())
}

/// Viewer panes kept running beyond those on screen, so flicking between
/// sidebar rows doesn't re-attach every time. Arbitrary; each is one idle
/// `tmux attach` client.
const VIEWER_SPARE: usize = 3;
/// Floor between attempts to (re)start a host for viewer panes.
/// Arbitrary; each attempt may spawn `ninox ptyd`.
const VIEWER_HOST_RETRY: std::time::Duration = std::time::Duration::from_secs(10);

/// Same `NINOX_DATA_DIR`-with-fallback resolution `ninox send` itself uses
/// (`cli.rs`'s `Command::Send` arm / `inbox_sessions_dir`) — shared by
/// `Action::SubmitPrompt`'s directly-picked-orchestrator delivery.
fn resolve_sessions_dir() -> PathBuf {
    AppConfig::sessions_dir()
}

/// `interactive_caller`, resolved once: which session (if any) this TUI
/// runs inside doesn't change while it runs, and resolving walks the whole
/// process table.
async fn cached_caller(lp: &mut Loop, store: &Store) -> Option<String> {
    if lp.caller.is_none() {
        lp.caller = Some(interactive_caller(store).await);
    }
    lp.caller.clone().flatten()
}

/// The tracked session this TUI runs inside, if any — `None` is the user
/// at a plain terminal, whom `worker_type_cli` always lets manage types
/// (see `authorize_mutation`); an agent driving `nx` is held to its own
/// type's rules.
async fn interactive_caller(store: &Store) -> Option<String> {
    let pane = ninox_core::runtime::current_private_pane_identity().await.ok().flatten();
    ninox_core::orchestrator_auth::resolve_live_caller_session_id(store, pane.as_ref().map(|p| p.physical_tmux_name.as_str()))
}

/// A freeform message a person typed into the TUI (the ninox prompt to a
/// picked orchestrator, or `t`'s composer), sent as the user unless this TUI
/// runs inside a tracked session.
async fn deliver_interactive(store: &Store, id: &str, message: &str, port: u16) -> anyhow::Result<()> {
    let config = AppConfig::load_or_default();
    let (from, sender_pane) = ninox_core::messaging::resolve_interactive_sender(store).await;
    let acp = ninox_core::acp_client::HttpAcpRouter { port };
    ninox_core::messaging::deliver_message_with_acp(
        store, &resolve_sessions_dir(), id, message, config.send_mechanism(),
        Some(&from), sender_pane.as_ref(), ninox_core::types::MessageKind::Freeform, &acp,
    )
    .await
    .map(|_| ())
}

fn viewer_boot_due(last: Option<std::time::Instant>, now: std::time::Instant) -> bool {
    last.is_none_or(|t| now.duration_since(t) >= VIEWER_HOST_RETRY)
}

/// Viewer size before the board has laid out a pane (matches new tmux
/// sessions' `-x 140 -y 50`).
const VIEWER_DEFAULT_SIZE: (u16, u16) = (140, 50);

struct Loop {
    hub: live::Hub,
    pending_spawns: Vec<SpawnHandle>,
    ptyd_boot: Option<tokio::task::JoinHandle<anyhow::Result<bool>>>,
    /// Whether `ptyd_boot` may start a host (vs. only connect to one).
    ptyd_boot_starts: bool,
    /// `ptyd_boot` was started for viewer panes only.
    ptyd_boot_for_viewers: bool,
    /// When a host was last started for viewer panes; a crashed or failed
    /// one is retried at most every `VIEWER_HOST_RETRY`.
    viewer_boot_at: Option<std::time::Instant>,
    /// Sessions with a viewer started by this TUI, most recent first.
    viewer_lru: Vec<String>,
    last_pane_size: Option<(u16, u16)>,
    /// Last size we asked ptyd for, per pane, so a pane resized by another
    /// client is not fought over every frame.
    requested_sizes: HashMap<String, (u16, u16)>,
    checkpoints_requested: HashSet<String>,
    last_focused: Option<String>,
    db_path: PathBuf,
    /// The session whose report was last requested; selecting another (or
    /// one coming back) gathers a fresh one.
    report_for: Option<String>,
    report_tx: tokio::sync::mpsc::UnboundedSender<(String, report::ReportData)>,
    pending_resumes: Vec<ResumeHandle>,
    brain_tx: tokio::sync::mpsc::UnboundedSender<brain::JobDone>,
    brain_search_tx: tokio::sync::mpsc::UnboundedSender<(String, Vec<String>)>,
    sources_tx: tokio::sync::mpsc::UnboundedSender<sources::SyncDone>,
    /// Kill/remove/reap run off the loop (git worktree removal can take
    /// seconds); results come back here.
    op_tx: tokio::sync::mpsc::UnboundedSender<OpDone>,
    port: u16,
    server_alert_tx: tokio::sync::mpsc::UnboundedSender<Vec<ninox_ui_shared::server_restart::Alert>>,
    ops_in_flight: HashSet<String>,
    /// The session whose delete plan was last requested for `D`'s confirm.
    delete_plan_for: Option<String>,
    delete_plan_tx: tokio::sync::mpsc::UnboundedSender<(String, Result<ninox_core::events::DeletePlan, String>)>,
    /// The session the inspector last showed, to start its Info tab at the
    /// top when the selection moves.
    inspector_for: Option<String>,
    /// `cached_caller`'s answer, once resolved.
    caller: Option<Option<String>>,
    diff_tx: tokio::sync::mpsc::UnboundedSender<(String, Result<Option<String>, String>)>,
    /// Deliveries, polls and clears for the Ninox prompt, drained into
    /// `FleetState::reply`.
    prompt_agent_tx: tokio::sync::mpsc::UnboundedSender<PromptAgentEvent>,
    /// The running reset-from-plan flow for the open `P` dialog. In memory
    /// only; dropping it (cancel, TUI exit) leaves nothing that could fire.
    reset_flow: Option<ninox_ui_shared::reset_from_plan::ResetFlow>,
    reset_tx: tokio::sync::mpsc::UnboundedSender<ResetEvent>,
    /// Tags the running flow's background results.
    reset_gen: u64,
    /// The open flow came from the automatic trigger's notification.
    reset_auto_started: bool,
}

/// Results of the reset-from-plan flow's background steps.
enum ResetEvent {
    Sent(u64, Result<(), String>),
    Done(u64, Result<String, String>),
}

/// A finished background op (kill/remove/reap/re-file, or a message send):
/// its id and the notice.
const SERVER_RESTART_OP: &str = "server-restart";

/// How often the footer's server alerts (expired logins, an older build)
/// are re-read from `/api/server/info`.
const SERVER_ALERT_POLL_SECS: u64 = 30;

/// Fetch the server's alerts off the loop; the result lands on
/// `server_alert_rx`.
fn poll_server_alerts(lp: &Loop) {
    let (port, tx) = (lp.port, lp.server_alert_tx.clone());
    tokio::spawn(async move {
        // Nothing listening isn't an alert of ours: the engine-down state
        // is reported elsewhere.
        let alerts = ninox_ui_shared::server_restart::fetch_info(port)
            .await
            .map(|info| ninox_ui_shared::server_restart::alerts_for_this_build(&info))
            .unwrap_or_default();
        let _ = tx.send(alerts);
    });
}

struct OpDone {
    id: String,
    /// The session's cached report/transcript no longer describe it (it was
    /// removed, or re-filed onto a fresh conversation).
    removed: bool,
    result: Result<String, String>,
}

/// A Ninox prompt result, tagged with the `ReplyPanel::generation` it was
/// issued under — drained by `event_loop` into `FleetState::reply`.
enum PromptAgentEvent {
    Delivered { generation: u64, result: Result<AnswerSource, String> },
    Polled { generation: u64, poll: PromptPoll },
    Cleared { generation: u64, result: Result<(), String> },
}

async fn event_loop(
    terminal: &mut Term,
    st: &mut TuiState,
    store: &Arc<Store>,
    engine: &Arc<Engine>,
    port: u16,
    (ptyd_boot, ptyd_boot_starts): (tokio::task::JoinHandle<anyhow::Result<bool>>, bool),
    db_path: PathBuf,
) -> anyhow::Result<()> {
    use futures_util::StreamExt;
    let (hub, mut pane_rx) = live::Hub::new(backend::ptyd_socket());
    let mut transcript_rx = engine.subscribe();
    let (report_tx, mut report_rx) = tokio::sync::mpsc::unbounded_channel();
    let (brain_tx, mut brain_rx) = tokio::sync::mpsc::unbounded_channel();
    let (brain_search_tx, mut brain_search_rx) = tokio::sync::mpsc::unbounded_channel();
    let (sources_tx, mut sources_rx) = tokio::sync::mpsc::unbounded_channel();
    let (op_tx, mut op_rx) = tokio::sync::mpsc::unbounded_channel::<OpDone>();
    let (server_alert_tx, mut server_alert_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<ninox_ui_shared::server_restart::Alert>>();
    let mut server_alert_tick = tokio::time::interval(std::time::Duration::from_secs(SERVER_ALERT_POLL_SECS));
    let (delete_plan_tx, mut delete_plan_rx) = tokio::sync::mpsc::unbounded_channel();
    let (diff_tx, mut diff_rx) = tokio::sync::mpsc::unbounded_channel();
    let (prompt_agent_tx, mut prompt_agent_rx) = tokio::sync::mpsc::unbounded_channel();
    let (reset_tx, mut reset_rx) = tokio::sync::mpsc::unbounded_channel::<ResetEvent>();
    let mut lp = Loop {
        hub,
        pending_spawns: Vec::new(),
        ptyd_boot: Some(ptyd_boot),
        ptyd_boot_starts,
        ptyd_boot_for_viewers: false,
        viewer_boot_at: None,
        viewer_lru: Vec::new(),
        last_pane_size: None,
        requested_sizes: HashMap::new(),
        checkpoints_requested: HashSet::new(),
        last_focused: None,
        db_path,
        report_for: None,
        report_tx,
        pending_resumes: Vec::new(),
        brain_tx,
        brain_search_tx,
        sources_tx,
        op_tx,
        port,
        server_alert_tx,
        ops_in_flight: HashSet::new(),
        delete_plan_for: None,
        delete_plan_tx,
        inspector_for: None,
        caller: None,
        diff_tx,
        prompt_agent_tx,
        reset_flow: None,
        reset_tx,
        reset_gen: 0,
        reset_auto_started: false,
    };
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
    // `None` while a `tmux attach` child owns the terminal; see `perform`.
    let mut events = Some(crossterm::event::EventStream::new());
    refresh(st, store);
    // Agent output only marks the screen dirty; it is drawn at most once per
    // PANE_FRAME, so a busy overview can't flood the terminal. Input and
    // ticks draw immediately.
    let mut dirty = Redraw::Now;
    let mut last_draw = std::time::Instant::now() - PANE_FRAME;
    loop {
        let due = dirty == Redraw::Now || (dirty == Redraw::Paced && last_draw.elapsed() >= PANE_FRAME);
        if due {
            st.now_ms = now_ms();
            st.expire_notice();
            st.mark_displayed_seen();
            let size = terminal.size()?;
            st.layout = layout::compute(Rect::new(0, 0, size.width, size.height), st);
            if let Some(inner) = st.layout.pane_inner {
                lp.last_pane_size = Some((inner.width, inner.height));
            }
            sync_viewers(st, &mut lp);
            sync_panes(st, &mut lp);
            sync_delete_plan(st, &mut lp, engine);
            sync_inspector(st, &mut lp);
            if sync_report(st, store, &mut lp) {
                // A new selection's report starts at the top.
                st.layout = layout::compute(Rect::new(0, 0, size.width, size.height), st);
            }
            resize_focused(st, &st.layout, &mut lp);
            draw_synchronized(terminal, st)?;
            last_draw = std::time::Instant::now();
            // An opened island's marching-ants edges (`island_flow`'s
            // animated `StepEdge`s into a `Working` member) only advance
            // when `tick_opened_animation` runs, which is every draw — so
            // while one is on screen, keep pacing redraws instead of
            // settling to `No` and only animating once a second on the
            // next refresh tick.
            dirty = if opened_island_is_animating(st) || reply_is_thinking(st) { Redraw::Paced } else { Redraw::No };
        }
        let frame_due = tokio::time::sleep_until((last_draw + PANE_FRAME).into());
        tokio::select! {
            _ = frame_due, if dirty == Redraw::Paced => {}
            _ = tick.tick() => {
                refresh(st, store);
                st.reset_cfg = AppConfig::load_or_default().orchestrator_reset;
                sync_reset_flow(st, &mut lp);
                st.sync_notifications(store);
                st.offer_restore(backend::pending_restore(store));
                lp.hub.refresh_list();
                lp.hub.fit_viewers(
                    st.viewers.iter().filter(|(_, v)| v.alive).map(|(s, v)| (s.clone(), v.pane.clone(), v.cols, v.rows)).collect(),
                );
                drain_finished_spawns(st, store, &mut lp.pending_spawns).await;
                drain_finished_resumes(st, &mut lp).await;
                check_ptyd_boot(st, &mut lp).await;
                // Not via tracing: its fmt subscriber writes to stdout,
                // which is the TUI's screen.
                if let Some(msg) = BACKGROUND_PANIC.lock().ok().and_then(|mut s| s.take()) {
                    let first = msg.lines().collect::<Vec<_>>().join(" ");
                    st.notify(Level::Error, format!("background task failed: {first}"));
                }
                // Liveness is re-probed every tick (a local TCP connect is
                // cheap): a slow cold start comes up, or a daemon dies, and
                // the header follows rather than freezing the startup verdict.
                st.daemon_up = ninox_core::daemon::port_in_use(port).await;
                poll_reply(st, store, &lp.prompt_agent_tx);
                dirty = Redraw::Now;
            }
            Some((query, ids)) = brain_search_rx.recv() => {
                st.brain.semantic_done(query, ids);
                dirty = Redraw::Now;
            }
            _ = server_alert_tick.tick() => poll_server_alerts(&lp),
            Some(alerts) = server_alert_rx.recv() => {
                if alerts != st.server_alerts {
                    st.server_alerts = alerts;
                    dirty = Redraw::Now;
                }
            }
            Some(done) = op_rx.recv() => {
                lp.ops_in_flight.remove(&done.id);
                if done.id == SERVER_RESTART_OP {
                    st.server_restarting = false;
                    if done.result.is_ok() {
                        st.server_alerts.clear();
                        poll_server_alerts(&lp);
                    }
                }
                if done.removed {
                    st.reports.remove(&done.id);
                    st.transcripts.remove(&done.id);
                    if lp.report_for.as_deref() == Some(done.id.as_str()) {
                        lp.report_for = None;
                    }
                }
                match done.result {
                    Ok(msg) => st.notify(Level::Info, msg),
                    Err(e) => st.notify(Level::Error, e),
                }
                refresh(st, store);
                lp.hub.refresh_list();
                dirty = Redraw::Now;
            }
            Some(event) = reset_rx.recv() => {
                reset_event(st, &mut lp, event);
                refresh(st, store);
                dirty = Redraw::Now;
            }
            Some((root, plan)) = delete_plan_rx.recv() => {
                if let Some(state::Modal::Delete(d)) = st.modal.as_mut() {
                    if d.root == root && d.plan.is_none() {
                        d.plan = Some(plan);
                    }
                }
                dirty = Redraw::Now;
            }
            Some((id, diff)) = diff_rx.recv() => {
                st.diff.finish(&id, diff.as_ref().map(Option::as_deref).map_err(String::as_str), now_ms());
                dirty = Redraw::Now;
            }
            Some(event) = prompt_agent_rx.recv() => {
                prompt_event(st, store, &lp.prompt_agent_tx, event);
                dirty = Redraw::Now;
            }
            Some(done) = brain_rx.recv() => {
                brain_job_done(st, done).await;
                dirty = Redraw::Now;
            }
            Some(done) = sources_rx.recv() => {
                source_synced(st, done).await;
                dirty = Redraw::Now;
            }
            Some((id, data)) = report_rx.recv() => {
                st.reports.insert(id, report::ReportSlot { data: Some(data), loading: false });
                dirty = Redraw::Now;
            }
            Some(ev) = pane_rx.recv() => {
                apply_pane_event(st, ev);
                // Coalesce a burst (several panes repainting) into one draw.
                while let Ok(ev) = pane_rx.try_recv() {
                    apply_pane_event(st, ev);
                }
                dirty = dirty.max(Redraw::Paced);
            }
            Ok(ninox_core::events::Event::SessionTranscript { session_id, entry }) = transcript_rx.recv() => {
                st.push_transcript_entry(session_id, entry);
                dirty = dirty.max(Redraw::Paced);
            }
            ev = events.get_or_insert_with(crossterm::event::EventStream::new).next() => {
                use crossterm::event::{Event, KeyEventKind};
                st.now_ms = now_ms();
                let action = match ev {
                    None => return Ok(()),
                    Some(Err(e)) => return Err(e.into()),
                    Some(Ok(Event::Key(k))) if k.kind != KeyEventKind::Release => state::handle_key(st, k),
                    Some(Ok(Event::Mouse(m))) => state::handle_mouse(st, m),
                    Some(Ok(Event::Paste(text))) => state::handle_paste(st, &text),
                    Some(Ok(_)) => Action::None,
                };
                if perform(action, terminal, st, store, engine, &mut lp, &mut events, port).await? {
                    return Ok(());
                }
                dirty = Redraw::Now;
            }
        }
    }
}

/// Gather the selected session's report when it newly comes on screen
/// (off the loop: git can take a while on a big repo). Returns whether the
/// report shown changed.
fn sync_report(st: &mut TuiState, store: &Arc<Store>, lp: &mut Loop) -> bool {
    let shown = st.selected_row().filter(|_| st.report_shown()).map(|r| (r.session.clone(), r.is_orchestrator));
    let Some((session, is_orch)) = shown else {
        lp.report_for = None;
        return false;
    };
    if lp.report_for.as_deref() == Some(session.id.as_str()) {
        return false;
    }
    lp.report_for = Some(session.id.clone());
    st.report_scroll = 0;
    st.reports.entry(session.id.clone()).or_default().loading = true;
    let (store, tx) = (Arc::clone(store), lp.report_tx.clone());
    tokio::task::spawn_blocking(move || {
        let data = report::load(&store, &session, is_orch);
        let _ = tx.send((session.id.clone(), data));
    });
    true
}

/// Floor between draws caused by agent output (~30fps). Arbitrary; previews
/// gain nothing from more, and every frame costs the user's terminal.
const PANE_FRAME: std::time::Duration = std::time::Duration::from_millis(33);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Redraw {
    No,
    /// Agent output changed: draw once PANE_FRAME has passed.
    Paced,
    /// Input, a tick or a resize: draw now.
    Now,
}

/// Whether the Fleet tab currently has an island open with at least one
/// `Working` member — the condition under which `island_flow::build_flow`
/// gives an edge `animated = true`, so marching ants are actually on screen
/// and worth pacing redraws for.
/// The reply panel shows its "is thinking" spinner, which only advances
/// when a frame is drawn.
fn reply_is_thinking(st: &TuiState) -> bool {
    st.layout.fleet.as_ref().is_some_and(|f| f.reply_panel.is_some()) && st.fleet.reply.drawer.shows_thinking()
}

fn opened_island_is_animating(st: &TuiState) -> bool {
    if st.view != View::Fleet || matches!(st.focus, state::Focus::Pane | state::Focus::Inspector) {
        return false; // the opened island isn't what's actually on screen
    }
    let Some(key) = &st.fleet.nav.open else { return false };
    st.fleet
        .islands
        .iter()
        .find(|i| &i.key == key)
        .is_some_and(|island| island.members.iter().any(|m| m.status == ninox_overview::status::MemberStatus::Working))
}

/// One frame as a single DEC 2026 synchronized update, so the user's
/// terminal never shows a half-written frame (terminals without 2026 ignore
/// the markers).
fn draw_synchronized(terminal: &mut Term, st: &TuiState) -> anyhow::Result<()> {
    use crossterm::{execute, terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate}};
    execute!(terminal.backend_mut(), BeginSynchronizedUpdate)?;
    let drawn = terminal.draw(|f| view::draw(f, st, &st.layout)).map(|_| ());
    execute!(terminal.backend_mut(), EndSynchronizedUpdate)?;
    Ok(drawn?)
}

/// Subscribe to exactly the panes on screen; queue checkpoint loads for
/// on-screen agents ptyd does not (yet) have live output for.
fn sync_panes(st: &TuiState, lp: &mut Loop) {
    let mut on_screen: Vec<String> = Vec::new();
    let mut wanted: HashMap<String, usize> = HashMap::new();
    if st.view == View::Fleet {
        if let Some(id) = st.selected_id() {
            if let Some(target) = st.pane_target(&id) {
                let page = st.layout.pane_inner.map(|r| r.height as usize).unwrap_or(24);
                let scroll = st.views.get(&target).map(|v| v.scroll).unwrap_or(0);
                wanted.insert(target, if scroll > 0 { scroll + page } else { 0 });
            }
            on_screen.push(id);
        }
    }
    lp.hub.sync(&wanted);
    for id in on_screen {
        let target = st.pane_target(&id).unwrap_or_else(|| id.clone());
        let has_live = st.views.get(&target).is_some_and(|v| v.live.as_ref().is_some_and(pane::has_content));
        if !has_live && lp.checkpoints_requested.insert(id.clone()) {
            lp.hub.load_checkpoint(&id);
        }
    }
}

/// Start viewer panes for the tmux sessions on screen, retire the ones no
/// longer shown (beyond a few spares) or whose tmux client exited, and start
/// a ptyd host for them if none runs.
fn sync_viewers(st: &mut TuiState, lp: &mut Loop) {
    let wanted = st.wanted_viewers();
    let dead: Vec<String> = st.viewers.iter().filter(|(_, v)| !v.alive).map(|(s, _)| s.clone()).collect();
    for session in dead {
        if let Some(v) = st.viewers.remove(&session) {
            lp.hub.kill_viewer(&v.pane);
            st.views.remove(&v.pane);
        }
        lp.viewer_lru.retain(|s| *s != session);
    }
    // A failed start leaves no pane; reopening must spawn afresh.
    lp.viewer_lru.retain(|s| !st.viewer_errors.contains_key(s));
    if st.ptyd != PtydState::Up {
        // Viewers die with the host; start afresh once one answers.
        lp.viewer_lru.clear();
        let now = std::time::Instant::now();
        if !wanted.is_empty() && lp.ptyd_boot.is_none() && viewer_boot_due(lp.viewer_boot_at, now) {
            lp.viewer_boot_at = Some(now);
            lp.ptyd_boot_starts = true;
            lp.ptyd_boot_for_viewers = true;
            lp.ptyd_boot = Some(tokio::spawn(backend::ensure_ptyd(true)));
        }
        return;
    }
    let (cols, rows) = lp.last_pane_size.unwrap_or(VIEWER_DEFAULT_SIZE);
    for session in wanted.iter().rev() {
        if !lp.viewer_lru.contains(session) {
            let pane = ninox_core::runtime::viewer_pane_id(std::process::id(), session);
            lp.hub.start_viewer(session, &pane, cols.max(20), rows.max(5));
        }
        lp.viewer_lru.retain(|s| s != session);
        lp.viewer_lru.insert(0, session.clone());
    }
    while lp.viewer_lru.len() > wanted.len() + VIEWER_SPARE {
        let Some(session) = lp.viewer_lru.pop() else { break };
        let pane = ninox_core::runtime::viewer_pane_id(std::process::id(), &session);
        lp.hub.kill_viewer(&pane);
        st.viewers.remove(&session);
        st.views.remove(&pane);
    }
}

fn resize_focused(st: &TuiState, lay: &layout::Layout, lp: &mut Loop) {
    let focused = match st.view {
        View::Board => st.focused_pane(),
        _ => None,
    };
    if focused != lp.last_focused {
        if let Some(id) = &focused {
            lp.requested_sizes.remove(id);
        }
        lp.last_focused = focused.clone();
    }
    let (Some(id), Some(inner)) = (focused, lay.pane_inner) else { return };
    // A tmux viewer follows its window's size (`live::fit_viewers`) and
    // never sizes the agent's window to this TUI.
    if ninox_core::runtime::is_viewer_pane(&id) {
        return;
    }
    let Some(info) = st.pane_info(&id) else { return };
    if !info.alive || inner.width == 0 || inner.height == 0 {
        return;
    }
    let target = (inner.width, inner.height);
    if (info.cols, info.rows) != target && lp.requested_sizes.get(&id) != Some(&target) {
        lp.hub.resize(&id, target.0, target.1);
        lp.requested_sizes.insert(id, target);
    }
}

fn apply_pane_event(st: &mut TuiState, ev: live::PaneEvent) {
    use live::PaneEvent;
    match ev {
        PaneEvent::Screen { pane, snap, requested } => {
            st.ptyd = PtydState::Up;
            let v = st.views.entry(pane).or_default();
            if requested >= v.scroll && snap.scrollback_len < v.scroll {
                v.scroll = snap.scrollback_len;
            }
            if pane::has_content(&snap) {
                v.checkpoint = None;
            }
            v.live = Some(snap);
        }
        PaneEvent::Exited { pane, code } => {
            if let Some((_, session)) = ninox_core::runtime::parse_viewer_pane(&pane) {
                let session = session.to_string();
                if let Some(v) = st.viewers.get_mut(&session) {
                    v.alive = false;
                }
                st.viewer_errors.insert(session.clone(), "tmux view closed".into());
                if st.selected_id().as_deref() == Some(session.as_str()) && st.focus == state::Focus::Pane {
                    st.notify(Level::Info, format!("{session}: tmux view closed — Enter reopens it"));
                }
                return;
            }
            if let Some(p) = st.panes.get_mut(&pane) {
                p.alive = false;
                p.exit_code = code;
            }
            st.views.entry(pane.clone()).or_default().exited = Some(code);
            let name = st.rows.iter().find(|r| r.id() == pane).map(|r| r.session.name.clone()).unwrap_or(pane);
            let code = code.map(|c| format!(" (code {c})")).unwrap_or_default();
            st.notify(Level::Warn, format!("{name} exited{code}"));
        }
        PaneEvent::Checkpoint { pane, checkpoint } => {
            let v = st.views.entry(pane).or_default();
            if !v.live.as_ref().is_some_and(pane::has_content) {
                v.checkpoint = checkpoint;
            }
        }
        PaneEvent::Panes(list) => {
            st.ptyd = PtydState::Up;
            st.viewers_unavailable = None;
            let own = std::process::id();
            st.panes.clear();
            st.viewers.clear();
            for p in list {
                match ninox_core::runtime::parse_viewer_pane(&p.pane) {
                    Some((pid, session)) if pid == own => {
                        st.viewers.insert(session.to_string(), p);
                    }
                    // Another TUI's viewer (or a stale one being reaped).
                    Some(_) => {}
                    None => {
                        st.panes.insert(p.pane.clone(), p);
                    }
                }
            }
        }
        PaneEvent::HostDown(e) => {
            if st.ptyd == PtydState::Up {
                st.notify(Level::Warn, format!("lost ptyd: {e}"));
                // Its viewers died with it; say so (and offer full-screen)
                // until a restarted host answers.
                if !st.viewers.is_empty() || !st.wanted_viewers().is_empty() {
                    st.viewers_unavailable = Some(format!("lost the host: {e}; retrying"));
                }
            }
            st.ptyd = PtydState::Down(e);
            st.panes.clear();
            st.viewers.clear();
        }
        PaneEvent::WriteFailed { pane, error } => st.notify(Level::Error, format!("{pane}: {error}")),
        PaneEvent::ViewerFailed { session, error } => {
            if st.selected_id().as_deref() == Some(session.as_str()) {
                st.notify(Level::Warn, format!("{session}: tmux view failed: {error}"));
            }
            st.viewer_errors.insert(session, error);
        }
    }
}

async fn check_ptyd_boot(st: &mut TuiState, lp: &mut Loop) {
    if !lp.ptyd_boot.as_ref().is_some_and(|h| h.is_finished()) {
        return;
    }
    let Some(handle) = lp.ptyd_boot.take() else { return };
    let started = lp.ptyd_boot_starts;
    let err = match handle.await {
        Ok(Ok(spawned)) => {
            if spawned && lp.ptyd_boot_for_viewers {
                HOST_FOR_VIEWERS.store(true, Ordering::SeqCst);
            }
            lp.hub.reap_stale_viewers();
            lp.hub.refresh_list();
            return;
        }
        Ok(Err(e)) => e.to_string(),
        Err(_) => "host unavailable in this build".into(),
    };
    st.ptyd = PtydState::Down(err.clone());
    // A connect-only probe (tmux backend) failing is expected; only a host
    // that would not start is news.
    if started {
        st.viewers_unavailable = Some(err.clone());
        st.notify(Level::Warn, format!("ptyd unavailable ({err}) — tmux sessions open full-screen (Ctrl+b d returns)"));
    }
}

async fn drain_finished_resumes(st: &mut TuiState, lp: &mut Loop) {
    let mut still_running = Vec::new();
    for (id, handle) in lp.pending_resumes.drain(..) {
        if !handle.is_finished() {
            still_running.push((id, handle));
            continue;
        }
        match handle.await {
            Ok(Ok(None)) => st.notify(Level::Info, format!("resumed {id}")),
            Ok(Ok(Some(queued))) => st.notify(Level::Info, queued),
            Ok(Err(e)) => st.notify(Level::Error, format!("resume {id} failed: {e}")),
            Err(e) => st.notify(Level::Error, format!("resume task panicked: {e}")),
        }
        // Whatever happened, the next look at it re-reads the session.
        if lp.report_for.as_deref() == Some(id.as_str()) {
            lp.report_for = None;
        }
        lp.hub.refresh_list();
    }
    lp.pending_resumes = still_running;
}

/// Removes finished spawn tasks from `pending`, folding each result into a
/// notice — this is how `Action::Spawn`'s 90s-blocking call surfaces its
/// outcome without freezing the event loop.
async fn drain_finished_spawns(st: &mut TuiState, store: &Store, pending: &mut Vec<SpawnHandle>) {
    let mut still_running = Vec::new();
    for handle in pending.drain(..) {
        if handle.is_finished() {
            match handle.await {
                Ok(Ok(spawned)) => {
                    let headline = match &spawned.queued {
                        Some(queued) => queued.clone(),
                        None => format!("spawned {}", spawned.id),
                    };
                    // One status-line slot: the warnings ride along with the
                    // headline rather than replacing it.
                    match spawned.warnings.is_empty() {
                        true => st.notify(Level::Info, headline),
                        false => st.notify(Level::Warn, format!("{headline} — {}", spawned.warnings.join("; "))),
                    }
                    if let Some(i) = st.rows.iter().position(|r| r.id() == spawned.id) {
                        st.selected = i;
                    }
                }
                Ok(Err(e)) => {
                    ninox_ui_shared::server_restart::note_spawn_failure(store, &e.to_string());
                    st.notify(Level::Error, format!("spawn failed: {e}"));
                }
                Err(e) => st.notify(Level::Error, format!("spawn task panicked: {e}")),
            }
        } else {
            still_running.push(handle);
        }
    }
    *pending = still_running;
}

/// Returns `true` when the TUI should exit.
#[allow(clippy::too_many_arguments)] // port is only for ACP routing of messages typed into the TUI
async fn perform(
    action: Action,
    terminal: &mut Term,
    st: &mut TuiState,
    store: &Arc<Store>,
    engine: &Arc<Engine>,
    lp: &mut Loop,
    events: &mut Option<crossterm::event::EventStream>,
    port: u16,
) -> anyhow::Result<bool> {
    match action {
        Action::None => {}
        Action::Quit => return Ok(true),
        Action::Write { pane, bytes } => lp.hub.write(&pane, bytes),
        Action::Connect(id) => {
            use ninox_ui_shared::connect::ConnectPlan;
            match backend::legacy_connect(store, &id).await {
                Err(e) => st.notify(Level::Error, format!("connect {id}: {e}")),
                Ok(ConnectPlan::Attach(argv)) => {
                    // The stream's reader thread can still be blocked reading
                    // stdin for a poll that raced this event, and would eat
                    // the first keys meant for tmux. Dropping it wakes and
                    // stops that thread; the loop starts a fresh stream.
                    *events = None;
                    if let Some(note) = attach_suspended(terminal, argv) {
                        st.notify(Level::Warn, note);
                    }
                }
                Ok(ConnectPlan::Dead { id, status }) => {
                    st.notify(Level::Warn, format!("{id} has no live tmux session — {}", ninox_ui_shared::cli::status_slug(&status)));
                }
                Ok(ConnectPlan::NotFound { .. }) => st.notify(Level::Warn, format!("{id} not found")),
            }
            refresh(st, store);
        }
        Action::Spawn(sub) => {
            st.notify(Level::Info, format!("spawning {}…", sub.name));
            let store = Arc::clone(store);
            lp.pending_spawns.push(tokio::spawn(async move {
                let config = AppConfig::load_or_default();
                let brain = ninox_core::brain_set::BrainSet::select(&config, &sub.brain_reads, Some(&sub.brain_write))?;
                let agent = ninox_core::config::AgentConfig { harness: sub.harness, model: sub.model };
                use ninox_ui_shared::spawn_form::SpawnKind;
                match sub.kind {
                    SpawnKind::Orchestrator => {
                        ninox_ui_shared::cli::spawn_orchestrator_common(
                            &store, &config, &sub.name, sub.prompt, None, Some(agent), Some(brain),
                            sub.workspace.as_deref().map(std::path::Path::new),
                        ).await
                    }
                    SpawnKind::Standalone => {
                        ninox_ui_shared::cli::spawn_standalone_worker(
                            &store, &config, sub.name, sub.workspace.unwrap_or_default(), sub.worker_type,
                            agent, brain, sub.prompt.unwrap_or_default(),
                        ).await
                    }
                }
            }));
        }
        Action::Kill(id) => run_op(st, lp, id, false, {
            let engine = Arc::clone(engine);
            move |id| async move {
                match backend::kill_session(&engine, &id).await {
                    Ok(()) => Ok(format!("killed {id}")),
                    Err(e) => Err(format!("kill failed: {e}")),
                }
            }
        }),
        Action::Remove(id) => run_op(st, lp, id, true, {
            let engine = Arc::clone(engine);
            move |id| async move { remove_worker(&engine, &id).await }
        }),
        Action::Delete { plan, force } => run_op(st, lp, plan.root.clone(), true, {
            let engine = Arc::clone(engine);
            move |_| async move {
                let results = engine.execute_delete(&plan, force).await;
                let (summary, failed) = ninox_ui_shared::delete_view::outcome_summary(&results);
                let summary = summary.replace('\n', " ");
                if failed { Err(summary) } else { Ok(summary) }
            }
        }),
        Action::Resume(id) => {
            if lp.pending_resumes.iter().any(|(r, _)| *r == id) || lp.ops_in_flight.contains(&id) {
                st.notify(Level::Info, format!("{id}: still working on the last request"));
                return Ok(false);
            }
            let Some(row) = st.rows.iter().find(|r| r.id() == id) else { return Ok(false) };
            let (session, is_orch) = (row.session.clone(), row.is_orchestrator);
            let engine = Arc::clone(engine);
            lp.pending_resumes.push((id, tokio::spawn(async move { resume(engine, session, is_orch).await })));
        }
        Action::ResumeAll => {
            let targets: Vec<(String, ninox_core::types::Session, bool)> = st
                .rows
                .iter()
                .filter(|r| ninox_overview::board::is_resume_target(&r.session))
                .filter(|r| !lp.pending_resumes.iter().any(|(id, _)| id == r.id()) && !lp.ops_in_flight.contains(r.id()))
                .map(|r| (r.id().to_string(), r.session.clone(), r.is_orchestrator))
                .collect();
            if targets.is_empty() {
                st.notify(Level::Info, "every interrupted session is already resuming");
                return Ok(false);
            }
            st.notify(Level::Info, format!("resuming {} session{}…", targets.len(), if targets.len() == 1 { "" } else { "s" }));
            for (id, session, is_orch) in targets {
                let engine = Arc::clone(engine);
                lp.pending_resumes.push((id, tokio::spawn(async move { resume(engine, session, is_orch).await })));
            }
        }
        Action::Refile(id) => {
            // `run_op` guards `ops_in_flight` itself; a restart in flight
            // lives in `pending_resumes` instead.
            if lp.pending_resumes.iter().any(|(r, _)| *r == id) {
                st.notify(Level::Info, format!("{id}: still working on the last request"));
                return Ok(false);
            }
            let Some(row) = st.rows.iter().find(|r| r.id() == id) else { return Ok(false) };
            let (session, is_orch) = (row.session.clone(), row.is_orchestrator);
            st.notify(Level::Info, format!("re-filing {}…", session.name));
            run_op(st, lp, id, true, {
                let engine = Arc::clone(engine);
                move |id| async move {
                    let config = AppConfig::load_or_default();
                    let req = ninox_ui_shared::restart::refile_request(&engine.store, session, is_orch, &config, now_ms())
                        .ok_or_else(|| format!("re-file {id}: no workspace recorded"))?;
                    match ninox_ui_shared::spawn_util::relaunch_in_place(engine, req, &config, false).await {
                        Ok(_) => Ok(format!("re-filed {id} on a fresh conversation")),
                        Err(cause) => Err(format!("re-file {id}: launch failed: {cause}")),
                    }
                }
            });
        }
        Action::OpenUrl(url) => {
            let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
            spawn_detached(st, opener, &url, format!("opened {url}"));
        }
        Action::OpenInEditor(id) => {
            let Some(path) = st.rows.iter().find(|r| r.id() == id).and_then(|r| r.session.workspace_path.clone()) else {
                st.notify(Level::Warn, format!("{id} has no recorded workspace to open"));
                return Ok(false);
            };
            let choice = AppConfig::load_or_default().editor;
            let program = editor::program(choice);
            if editor::is_terminal(choice) {
                // Neovim has no window of its own: run it on the real
                // terminal, blocking, like the full-screen tmux attach.
                *events = None;
                match open_terminal_editor_suspended(terminal, program, &path) {
                    Some(note) => st.notify(Level::Warn, note),
                    None => st.notify(Level::Info, format!("back from {program}")),
                }
            } else {
                let note = format!("opened {path} in {program}");
                spawn_detached(st, program, &path, note);
            }
        }
        // Not `run_op`: its per-session slot would refuse a second message
        // (losing the already-closed composer's draft) and block kill/remove
        // on a session whose delivery is slow. The op id is never inserted
        // into `ops_in_flight`, so its removal on completion is a no-op.
        Action::SendMessage { id, message } => {
            let store = Arc::clone(store);
            let tx = lp.op_tx.clone();
            tokio::spawn(async move {
                let result = deliver_interactive(&store, &id, &message, port)
                    .await
                    .map(|()| format!("sent to {id}"))
                    .map_err(|e| format!("message to {id} failed: {e}"));
                let _ = tx.send(OpDone { id: format!("{id}:message"), removed: false, result });
            });
        }
        Action::PromptIdle(only) => run_op(st, lp, "idle-prompt".into(), false, {
            let store = Arc::clone(store);
            let report = st.idle.report.clone();
            move |_op| async move {
                let config = AppConfig::load_or_default();
                let now = ninox_ui_shared::idle_cli::now_millis();
                let outcomes = ninox_ui_shared::idle_cli::send_prompts(&store, &report, only.as_deref(), &config, port, now).await;
                if outcomes.is_empty() {
                    return Ok("no orchestrator has idle workers".to_string());
                }
                Ok(outcomes.iter().map(|o| o.describe()).collect::<Vec<_>>().join("; "))
            }
        }),
        Action::CleanIdle(id) => run_op(st, lp, format!("{id}:clean"), false, {
            let store = Arc::clone(store);
            move |_op| async move {
                let dirs = AppConfig::load_or_default().workspaces.build_output_dirs;
                let now = ninox_ui_shared::idle_cli::now_millis();
                tokio::task::spawn_blocking(move || {
                    ninox_core::idle_workers::clean_idle_build_output(&store, &id, &dirs, now)
                        .map(|f| format!("{id}: freed {}", ninox_core::idle_workers::format_bytes(f.bytes())))
                        .map_err(|e| format!("clean {id}: {e}"))
                })
                .await
                .unwrap_or_else(|e| Err(e.to_string()))
            }
        }),
        Action::GhostAccept(plan_id) => run_op(st, lp, format!("ghost:{plan_id}"), false, {
            let store = Arc::clone(store);
            move |_op| async move {
                let config = AppConfig::load_or_default();
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as i64;
                // The TUI is the user: no managed caller.
                ninox_ui_shared::ghost_accept::accept(&store, &config, &plan_id, None, None, now)
                    .await
                    .map(|r| format!("accepted {}: {}", r.plan_id, r.summary()))
                    .map_err(|e| format!("accept {plan_id}: {e:#}"))
            }
        }),
        Action::GhostDiscard(plan_id) => run_op(st, lp, format!("ghost:{plan_id}"), false, {
            let store = Arc::clone(store);
            move |_op| async move {
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as i64;
                ninox_ui_shared::ghost_accept::discard(&store, &plan_id, None, None, now)
                    .map(|r| format!("discarded {}: {}", r.plan_id, r.summary()))
                    .map_err(|e| format!("discard {plan_id}: {e:#}"))
            }
        }),
        Action::Reap(orch_id) => run_op(st, lp, orch_id, false, {
            let engine = Arc::clone(engine);
            move |id| async move {
                match engine.reap_workers(&id, ninox_core::events::ReapSelection::Finished, false).await {
                    Ok(outcomes) => Ok(format!("reaped {} workers", outcomes.len())),
                    Err(e) => Err(format!("reap failed: {e}")),
                }
            }
        }),
        Action::ResetFromPlanStart(id) => {
            use ninox_ui_shared::reset_from_plan::{send_request, ResetFlow, UiPhase};
            if lp.reset_flow.is_some() {
                st.notify(Level::Info, "a reset from plan is already in progress");
                return Ok(false);
            }
            match ResetFlow::prepare(Arc::clone(store), &id) {
                Err(e) => st.notify(Level::Warn, format!("can't reset {id}: {e:#}")),
                Ok((flow, request)) => {
                    let name = st.rows.iter().find(|r| r.id() == id).map(|r| r.session.name.clone()).unwrap_or_else(|| id.clone());
                    lp.reset_flow = Some(flow);
                    lp.reset_auto_started = false;
                    lp.reset_gen = ninox_ui_shared::reset_from_plan::next_generation();
                    let generation = lp.reset_gen;
                    st.modal = Some(state::Modal::ResetFromPlan(state::ResetModal {
                        id: id.clone(), name, phase: UiPhase::Sending, preview: None, error: None, scroll: 0,
                    }));
                    let (store, tx) = (Arc::clone(store), lp.reset_tx.clone());
                    tokio::spawn(async move {
                        let config = AppConfig::load_or_default();
                        let r = send_request(&store, &config, &id, &request).await.map_err(|e| format!("{e:#}"));
                        let _ = tx.send(ResetEvent::Sent(generation, r));
                    });
                }
            }
        }
        Action::ResetFromPlanCancel => {
            if let Some(flow) = lp.reset_flow.take() {
                if lp.reset_auto_started {
                    flow.cancel();
                }
            }
            lp.reset_auto_started = false;
            st.modal = None;
        }
        Action::ResetFromPlanOpenReady(id) => {
            use ninox_ui_shared::reset_from_plan::{ResetFlow, UiPhase};
            if lp.reset_flow.is_some() {
                st.notify(Level::Info, "a reset from plan is already in progress");
                return Ok(false);
            }
            match ninox_ui_shared::reset_from_plan::auto_flow_row(store, &id).ok_or(()) {
                Ok(row) => {
                    let name = st.rows.iter().find(|r| r.id() == id).map(|r| r.session.name.clone()).unwrap_or_else(|| id.clone());
                    lp.reset_flow = Some(ResetFlow::from_auto_row(Arc::clone(store), &row));
                    lp.reset_gen = ninox_ui_shared::reset_from_plan::next_generation();
                    lp.reset_auto_started = true;
                    st.modal = Some(state::Modal::ResetFromPlan(state::ResetModal {
                        id, name, phase: UiPhase::Waiting, preview: None, error: None, scroll: 0,
                    }));
                }
                _ => st.notify(Level::Info, format!("{id} has no automatic reset in progress")),
            }
        }
        Action::ResetFromPlanConfirm => {
            if let (Some(flow), Some(state::Modal::ResetFromPlan(m))) = (lp.reset_flow.take(), st.modal.as_mut()) {
                m.phase = ninox_ui_shared::reset_from_plan::UiPhase::Resetting;
                m.error = None;
                let generation = lp.reset_gen;
                let tx = lp.reset_tx.clone();
                tokio::spawn(async move {
                    let config = AppConfig::load_or_default();
                    let r = flow.confirm(&config).await.map_err(|e| format!("{e:#}"));
                    let _ = tx.send(ResetEvent::Done(generation, r.map(|r| format!("{}: reset onto a fresh conversation from {}", r.orchestrator_id, r.plan_path))));
                });
            }
        }
        Action::RestartServer => {
            st.server_restarting = true;
            let port = lp.port;
            let db_path = lp.db_path.clone();
            let store = Arc::clone(store);
            run_op(st, lp, SERVER_RESTART_OP.to_string(), false, move |_id| async move {
                let target = ninox_ui_shared::server_restart::Target::resolve(port, db_path);
                let ops = ninox_ui_shared::server_restart::LiveOps { store: Some(store) };
                ninox_ui_shared::server_restart::restart(&target, &ops, Default::default(), |_| {})
                    .await
                    .map(|o| o.summary())
                    .map_err(|e| format!("restart server: {e}"))
            });
        }
        Action::RestartAffected(ids) => {
            let db_path = lp.db_path.clone();
            let store = Arc::clone(store);
            run_op(st, lp, "restart-login-errors".to_string(), false, move |_id| {
                ninox_ui_shared::server_restart::restart_sessions(store, db_path, ids)
            });
        }
        Action::RestartAll => run_op(st, lp, "fleet-restart-all".to_string(), false, {
            let store = Arc::clone(store);
            let db_path = lp.db_path.clone();
            move |_id| async move {
                let config = AppConfig::load_or_default();
                let args = ninox_ui_shared::restart::RestartArgs { session_ids: vec![], all: true, exec_detached: false };
                match ninox_ui_shared::restart::execute(args, store, config, db_path).await {
                    Ok(outcomes) if ninox_ui_shared::restart::had_trouble(&outcomes) => Err(ninox_ui_shared::restart::summarize(&outcomes)),
                    Ok(outcomes) => Ok(ninox_ui_shared::restart::summarize(&outcomes)),
                    Err(e) => Err(format!("restart all failed: {e}")),
                }
            }
        }),
        Action::LoadBrain(_) => load_brain(st).await,
        Action::LoadBrainLinks(id) => load_brain_links(st, id).await,
        Action::SearchBrain(query) => {
            st.brain.searching = Some(query.clone());
            let tx = lp.brain_search_tx.clone();
            let path = AppConfig::load_or_default().resolved_brain_path();
            tokio::task::spawn_blocking(move || {
                let ids = semantic_brain_search(&path, &query);
                let _ = tx.send((query, ids));
            });
        }
        Action::EditBrain(id) => {
            let path = AppConfig::load_or_default().resolved_brain_path();
            let file = match brain::source_file(&path, &id) {
                Ok(f) => f,
                Err(e) => {
                    st.notify(Level::Warn, format!("can't edit: {e}"));
                    return Ok(false);
                }
            };
            let before = std::fs::read(&file).ok();
            *events = None;
            let note = edit_suspended(terminal, &file);
            if std::fs::read(&file).ok() == before {
                st.notify(if note.is_some() { Level::Error } else { Level::Info }, note.unwrap_or_else(|| format!("{id} unchanged")));
            } else {
                start_brain_job(st, lp, brain::Job::Reindex { id });
            }
        }
        Action::NewBrain { entry_type, tags } => {
            let path = AppConfig::load_or_default().resolved_brain_path();
            let template = brain::template(&entry_type, &tags);
            let tmp = std::env::temp_dir().join(format!("ninox-brain-new-{}-{}.md", std::process::id(), now_ms()));
            if let Err(e) = std::fs::write(&tmp, &template) {
                st.notify(Level::Error, format!("can't start a new entry: {}: {e}", tmp.display()));
                return Ok(false);
            }
            *events = None;
            let note = edit_suspended(terminal, &tmp);
            let edited = std::fs::read_to_string(&tmp).unwrap_or_default();
            let _ = std::fs::remove_file(&tmp);
            match (note, brain::entry_from_edit(&path, &template, &edited)) {
                (Some(e), _) => st.notify(Level::Error, format!("{e}; nothing was added")),
                (None, Ok(None)) => st.notify(Level::Info, "new entry cancelled (left unchanged)"),
                (None, Err(e)) => st.notify(Level::Warn, e),
                (None, Ok(Some((id, content)))) => {
                    st.brain.cursor = brain::Sel { group: None, id: Some(id.clone()) };
                    start_brain_job(st, lp, brain::Job::Write { id, content });
                }
            }
        }
        Action::DeleteBrain(id) => start_brain_job(st, lp, brain::Job::Delete { id }),
        Action::ReindexBrain => start_brain_job(st, lp, brain::Job::ReindexAll),
        Action::CycleBrainCatalogue => load_brain(st).await,
        Action::LoadBrainSources => load_sources(st).await,
        Action::SaveBrainSource(form) => match sources::save(&form) {
            Ok(name) => {
                st.notify(Level::Info, format!("saved source {name}"));
                after_source_change(st).await;
            }
            Err(e) => st.modal = Some(Modal::SourceForm(sources::SourceForm { form: SourceEditForm { error: Some(e), ..*form }, focus: sources::FormField::Name })),
        },
        Action::ToggleBrainSource(name) => match sources::toggle(&name) {
            Ok(on) => {
                st.notify(Level::Info, format!("{name} {}", if on { "included in search" } else { "excluded from search" }));
                after_source_change(st).await;
            }
            Err(e) => st.notify(Level::Warn, e),
        },
        Action::RemoveBrainSource(name) => match sources::remove(&name) {
            Ok(()) => {
                st.notify(Level::Info, format!("removed source {name}; its remote and cache are untouched"));
                after_source_change(st).await;
            }
            Err(e) => st.notify(Level::Error, e),
        },
        Action::SyncBrainSources(names) => start_source_syncs(st, lp, &names),
        Action::LoadSettings => st.settings.reload(settings::load()),
        Action::SaveSetting(change) => {
            let id = change.id().clone();
            match settings::save(&change) {
                Ok((cfg, msg)) => {
                    st.settings.editing = None;
                    st.settings.error = None;
                    st.settings.reload(Ok(cfg.clone()));
                    let note = apply_live_setting(st, lp, &id, &cfg);
                    st.notify(Level::Info, format!("saved: {msg}{note}"));
                }
                Err(e) => {
                    if st.settings.editing.is_none() {
                        st.notify(Level::Error, e.clone());
                    }
                    st.settings.error = Some(e);
                }
            }
        }
        Action::EditConfig => {
            let path = AppConfig::config_path();
            *events = None;
            let note = edit_suspended(terminal, &path);
            st.settings.reload(settings::load());
            match (note, settings::load()) {
                (Some(e), _) => st.notify(Level::Error, e),
                (None, Ok(cfg)) => {
                    for id in [settings::FieldId::Prefix, settings::FieldId::Colors, settings::FieldId::RestorePolicy] {
                        apply_live_setting(st, lp, &id, &cfg);
                    }
                    st.notify(Level::Info, format!("reloaded {}", path.display()));
                }
                (None, Err(e)) => st.notify(Level::Error, e),
            }
        }
        Action::LoadSpawnForm => match spawn::load() {
            Ok(cfg) => st.spawn_form = spawn::build(&cfg),
            Err(e) => st.notify(Level::Error, e),
        },
        Action::LoadWorkerTypes => st.worker_types.reload(worker_types::load()),
        Action::SaveWorkerType { name, change } => match worker_types::save(store, cached_caller(lp, store).await.as_deref(), &name, &change) {
            Ok((cfg, msg)) => {
                st.worker_types.editing = None;
                st.worker_types.adding_rule = None;
                st.worker_types.error = None;
                st.worker_types.reload(Ok(cfg));
                st.notify(Level::Info, format!("saved: {msg}"));
            }
            Err(e) => {
                if st.worker_types.editing.is_none() {
                    st.notify(Level::Error, e.clone());
                }
                st.worker_types.error = Some(e);
            }
        },
        Action::NewWorkerType(name) => match worker_types::new_type(store, cached_caller(lp, store).await.as_deref(), &name) {
            Ok(cfg) => {
                st.worker_types.editing = None;
                st.worker_types.error = None;
                st.worker_types.reload(Ok(cfg));
                if let Some(i) = st.worker_types.names.iter().position(|n| n == &name) {
                    st.worker_types.select_list_row(i);
                }
                st.notify(Level::Info, format!("added worker type {name}"));
            }
            Err(e) => st.worker_types.error = Some(e),
        },
        Action::DeleteWorkerType(name) => match worker_types::delete_type(store, cached_caller(lp, store).await.as_deref(), &name) {
            Ok(cfg) => {
                st.worker_types.reload(Ok(cfg));
                st.notify(Level::Info, format!("deleted {name}"));
            }
            Err(e) => st.notify(Level::Error, e),
        },
        Action::ResetWorkerType(name) => match worker_types::reset_type(store, cached_caller(lp, store).await.as_deref(), &name) {
            Ok(cfg) => {
                st.worker_types.reload(Ok(cfg));
                st.notify(Level::Info, format!("reset {name} to its built-in default"));
            }
            Err(e) => st.notify(Level::Error, e),
        },
        Action::ExportWorkerType(name) => match worker_types::export_type(&name) {
            Ok(path) => {
                st.worker_types.notice = Some(format!("exported to {}", path.display()));
                st.notify(Level::Info, format!("exported {name} to {}", path.display()));
            }
            Err(e) => st.notify(Level::Error, e),
        },
        Action::ImportWorkerType => {
            let tmp = std::env::temp_dir().join(format!("ninox-worker-type-import-{}-{}.toml", std::process::id(), now_ms()));
            if let Err(e) = std::fs::write(&tmp, worker_types::IMPORT_TEMPLATE) {
                st.notify(Level::Error, format!("can't start an import: {}: {e}", tmp.display()));
                return Ok(false);
            }
            *events = None;
            let note = edit_suspended(terminal, &tmp);
            let edited = std::fs::read_to_string(&tmp).unwrap_or_default();
            let _ = std::fs::remove_file(&tmp);
            match note {
                Some(e) => st.notify(Level::Error, format!("{e}; nothing was imported")),
                None if edited.trim() == worker_types::IMPORT_TEMPLATE.trim() => st.notify(Level::Info, "import cancelled (left unchanged)"),
                None => match worker_types::import_types(store, cached_caller(lp, store).await.as_deref(), &edited) {
                    Ok((cfg, names)) => {
                        st.worker_types.reload(Ok(cfg));
                        if let Some(i) = names.first().and_then(|first| st.worker_types.names.iter().position(|n| n == first)) {
                            st.worker_types.select_list_row(i);
                        }
                        st.notify(Level::Info, format!("imported {}", names.join(", ")));
                    }
                    Err(e) => st.notify(Level::Error, format!("import failed: {e}")),
                },
            }
        }
        Action::AddRemote { host, label } => {
            *events = None;
            match ninox_core::hooks::canonical_exe() {
                Ok(exe) => {
                    let note = machine_add_suspended(terminal, &exe, &host, label.as_deref());
                    refresh(st, store);
                    match note {
                        Some(msg) => st.notify(Level::Error, msg),
                        None => st.notify(Level::Info, format!("added machine {host}")),
                    }
                }
                Err(e) => st.notify(Level::Error, format!("could not resolve ninox binary: {e}")),
            }
        }
        Action::RemoveMachine(id) => match ninox_ui_shared::machine::remove(&id) {
            Ok(()) => {
                refresh(st, store);
                st.notify(Level::Info, format!("removed machine {id}"));
            }
            Err(e) => st.notify(Level::Error, format!("remove machine failed: {e}")),
        },
        Action::Restore => match backend::restore_fleet(&lp.db_path) {
            Ok(msg) => st.notify(Level::Info, msg),
            Err(e) => st.notify(Level::Error, format!("restore failed to start: {e}")),
        },
        Action::DismissRestore => backend::dismiss_pending_restore(store),
        Action::AnswerUserRequest { id, answer } => {
            match store.answer_user_request(&id, &answer) {
                Ok(()) => {
                    st.notify(Level::Info, format!("answered {id}"));
                    refresh(st, store);
                }
                Err(e) => st.notify(Level::Error, format!("answer {id} failed: {e}")),
            }
        }
        Action::CancelUserRequest { id } => {
            match store.mark_user_request_cancelled(&id) {
                Ok(()) => {
                    st.notify(Level::Info, format!("dismissed {id}"));
                    refresh(st, store);
                }
                Err(e) => st.notify(Level::Error, format!("dismiss {id} failed: {e}")),
            }
        }
        Action::Yank(text) => {
            let lines = text.lines().count();
            let mut out = std::io::stdout();
            let _ = write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes()));
            let _ = out.flush();
            let s = if lines == 1 { "" } else { "s" };
            st.notify(Level::Info, format!("copied {lines} line{s} to the clipboard"));
        }
        Action::SubmitPrompt { target, question, message } => {
            let label = match &target {
                fleet::PromptTarget::Agent => "ninox".to_string(),
                fleet::PromptTarget::Orchestrator(id) => {
                    st.rows.iter().find(|r| r.id() == id).map(|r| r.session.name.clone()).unwrap_or_else(|| id.clone())
                }
            };
            let generation = st.fleet.reply.ask(question, label, now_ms());
            let asked_at_ms = st.fleet.reply.drawer.turns.last().map_or(0, |t| t.asked_at_ms);
            let store = Arc::clone(store);
            let tx = lp.prompt_agent_tx.clone();
            tokio::spawn(async move {
                let result = match target {
                    fleet::PromptTarget::Agent => {
                        let config = AppConfig::load_or_default();
                        prompt_reply::deliver_to_agent(store, &config, &message, asked_at_ms).await
                    }
                    fleet::PromptTarget::Orchestrator(id) => {
                        let baseline = prompt_reply::orchestrator_baseline(&id).await;
                        deliver_interactive(&store, &id, &message, port)
                            .await
                            .map(|()| AnswerSource::OrchestratorPane { id, baseline })
                            .map_err(|e| e.to_string())
                    }
                };
                let _ = tx.send(PromptAgentEvent::Delivered { generation, result });
            });
        }
        Action::ClearPromptContext => {
            // Bumped only once the clear succeeds: a failed clear must not
            // strand an in-flight delivery's turn on "thinking…".
            let generation = st.fleet.reply.generation;
            let tx = lp.prompt_agent_tx.clone();
            match &st.fleet.prompt_target {
                fleet::PromptTarget::Agent => {
                    st.fleet.reply.drawer.clear_requested();
                    let store = Arc::clone(store);
                    tokio::spawn(async move {
                        let config = AppConfig::load_or_default();
                        let result = ninox_ui_shared::prompt_agent::clear(store, &config).await.map_err(|e| e.to_string());
                        let _ = tx.send(PromptAgentEvent::Cleared { generation, result });
                    });
                }
                // No clear exists for a picked orchestrator's own session;
                // never clear the unrelated prompt agent on its behalf.
                fleet::PromptTarget::Orchestrator(_) => {
                    let result = Err("clear context isn't available for a picked orchestrator".to_string());
                    let _ = tx.send(PromptAgentEvent::Cleared { generation, result });
                }
            }
        }
    }
    Ok(false)
}

fn prompt_event(st: &mut TuiState, store: &Arc<Store>, tx: &PromptTx, event: PromptAgentEvent) {
    let reply = &mut st.fleet.reply;
    match event {
        PromptAgentEvent::Delivered { generation, result } => {
            if generation != reply.generation {
                return;
            }
            let Some(turn) = reply.drawer.latest_mut() else { return };
            match result {
                Ok(source) => {
                    turn.source = source;
                    poll_reply(st, store, tx);
                }
                Err(message) => turn.fail(message, now_ms()),
            }
        }
        PromptAgentEvent::Polled { generation, poll } => {
            if generation != reply.generation {
                return;
            }
            let had_answer = reply.drawer.turns.last().is_some_and(|t| !t.answer.is_empty());
            if reply.drawer.apply_poll(poll, now_ms()) && !had_answer {
                reply.reopen();
            }
        }
        PromptAgentEvent::Cleared { generation, result } => {
            if generation != reply.generation {
                return;
            }
            match result {
                Ok(()) => reply.cleared(),
                Err(message) => reply.drawer.clear_failed(message),
            }
        }
    }
}

type PromptTx = tokio::sync::mpsc::UnboundedSender<PromptAgentEvent>;

/// Re-reads the latest turn's answer, the prompt agent's context figure and
/// recent message events (relays), off the loop and one read at a time.
/// Transcript turns keep polling after their turn ends: the agent resumes
/// to report a relayed answer.
fn poll_reply(st: &mut TuiState, store: &Arc<Store>, tx: &PromptTx) {
    let reply = &mut st.fleet.reply;
    let Some(turn) = reply.drawer.turns.last() else { return };
    if reply.drawer.poll_in_flight || !turn.wants_poll(now_ms()) {
        return;
    }
    let (source, asked_at_ms, generation) = (turn.source.clone(), turn.asked_at_ms, reply.generation);
    reply.drawer.poll_in_flight = true;
    let (store, tx) = (Arc::clone(store), tx.clone());
    tokio::spawn(async move {
        let poll = prompt_reply::poll(store, source, asked_at_ms).await;
        let _ = tx.send(PromptAgentEvent::Polled { generation, poll });
    });
}

/// Run a kill/remove/reap for `id` off the loop; one at a time per id.
fn run_op<F, Fut>(st: &mut TuiState, lp: &mut Loop, id: String, removed: bool, op: F)
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Result<String, String>> + Send + 'static,
{
    if !lp.ops_in_flight.insert(id.clone()) {
        st.notify(Level::Info, format!("{id}: still working on the last request"));
        return;
    }
    let tx = lp.op_tx.clone();
    let fut = op(id.clone());
    tokio::spawn(async move {
        let result = fut.await;
        let _ = tx.send(OpDone { id, removed, result });
    });
}

/// Starts the inspector's Info tab at the top for a new selection, and
/// (re)fetches the selected session's diff off the loop while the Diff tab
/// is showing — on selection, then every `DIFF_REFRESH_MS` (a live session
/// keeps committing). Same `compute_diff` as the GUI's Diff panel.
fn sync_inspector(st: &mut TuiState, lp: &mut Loop) {
    let Some(row) = st.selected_row().filter(|_| st.layout.inspector.is_some()) else { return };
    let (id, remote, workspace) = (row.id().to_string(), row.session.machine_id.is_some(), row.session.workspace_path.clone());
    if lp.inspector_for.as_deref() != Some(id.as_str()) {
        st.inspector_scroll = 0;
        lp.inspector_for = Some(id.clone());
    }
    if st.inspector_tab != inspector::Tab::Diff || remote {
        return;
    }
    let Some(workspace) = workspace else { return };
    if !st.diff.due(&id, now_ms()) {
        return;
    }
    st.diff.begin(&id);
    let tx = lp.diff_tx.clone();
    tokio::spawn(async move {
        // `compute_diff` reads a git failure as "no diff"; the one failure
        // worth naming is a worktree that's gone (merged and cleaned up).
        let path = std::path::Path::new(&workspace);
        let diff = if path.is_dir() {
            Ok(ninox_core::lifecycle::brain_harvest::compute_diff(path).await)
        } else {
            Err(format!("{workspace} no longer exists"))
        };
        let _ = tx.send((id, diff));
    });
}

/// Polls the running reset-from-plan flow into the open dialog (once a second: a file read
/// and two store reads, and drops a flow
/// whose dialog is gone.
fn sync_reset_flow(st: &mut TuiState, lp: &mut Loop) {
    let Some(state::Modal::ResetFromPlan(m)) = st.modal.as_mut() else {
        lp.reset_flow = None;
        return;
    };
    if !m.phase.pollable() {
        return;
    }
    if let Some(flow) = lp.reset_flow.as_mut() {
        m.phase = m.phase.after_poll(flow.poll());
        m.preview = flow.preview().map(|p| (p.path, p.content));
    }
}

fn reset_event(st: &mut TuiState, lp: &mut Loop, event: ResetEvent) {
    use ninox_ui_shared::reset_from_plan::UiPhase;
    let (ResetEvent::Sent(generation, _) | ResetEvent::Done(generation, _)) = &event;
    if *generation != lp.reset_gen {
        return;
    }
    let Some(state::Modal::ResetFromPlan(m)) = st.modal.as_mut() else {
        if let ResetEvent::Done(_, r) = event {
            match r {
                Ok(msg) => st.notify(Level::Info, msg),
                Err(e) => st.notify(Level::Error, e),
            }
        }
        return;
    };
    match event {
        ResetEvent::Sent(_, Ok(())) if m.phase == UiPhase::Sending => m.phase = UiPhase::Waiting,
        ResetEvent::Sent(_, Ok(())) => {}
        ResetEvent::Sent(_, Err(e)) => {
            m.error = Some(e);
            m.phase = UiPhase::SendFailed;
        }
        ResetEvent::Done(_, Ok(msg)) => {
            st.modal = None;
            lp.reset_flow = None;
            st.notify(Level::Info, msg);
        }
        ResetEvent::Done(_, Err(e)) => {
            m.error = Some(e);
            m.phase = UiPhase::ResetFailed;
        }
    }
}

/// Reads the plan for an open `D` confirm off the loop, once per opening.
fn sync_delete_plan(st: &TuiState, lp: &mut Loop, engine: &Arc<Engine>) {
    let open = match &st.modal {
        Some(state::Modal::Delete(d)) if d.plan.is_none() => Some(d.root.clone()),
        _ => None,
    };
    if open == lp.delete_plan_for {
        return;
    }
    lp.delete_plan_for = open.clone();
    let Some(root) = open else { return };
    let (engine, tx) = (Arc::clone(engine), lp.delete_plan_tx.clone());
    tokio::task::spawn_blocking(move || {
        let plan = engine.plan_delete(&root).map_err(|e| e.to_string());
        let _ = tx.send((root, plan));
    });
}

/// Opens the catalogue the Brain tab's `active_catalogue` names: one
/// catalogue's own index (`Some(idx)`), or every enabled catalogue merged
/// ("All sources", `None`) via `MergedBrainIndex`. The sole integration
/// point between `BrainView`'s catalogue-cycling state and an actually
/// open index — `load_brain`/`load_brain_links` both build a scope through
/// this instead of hardcoding `resolved_brain_path()`.
fn open_brain_scope(options: &[ninox_core::config::CatalogueRef], active: Option<usize>) -> anyhow::Result<ninox_core::BrainScope> {
    use ninox_core::{BrainIndex, BrainScope, MergedBrainIndex};
    match active {
        Some(idx) => {
            let catalogue = options.get(idx).ok_or_else(|| anyhow::anyhow!("catalogue index {idx} out of range"))?;
            Ok(BrainScope::Single(std::sync::Arc::new(BrainIndex::open(&catalogue.path)?)))
        }
        None => Ok(BrainScope::All(std::sync::Arc::new(MergedBrainIndex::open(options)?))),
    }
}

/// Reload the brain list for `query`, keeping the cursor on its entry.
/// Every entry: the Brain tab searches client-side, so the list is always
/// the whole brain.
///
/// Entries plus the resolved wikilink pairs (`BrainIndex::links_all()`),
/// loaded together so `set_edges` has both without a second index open.
type BrainLoad = (Vec<ninox_core::BrainEntry>, Vec<(String, String)>);

async fn load_brain(st: &mut TuiState) {
    st.brain.set_catalogues(AppConfig::load_or_default().catalogue_options());
    let options = st.brain.catalogue_options.clone();
    let active = st.brain.active_catalogue;
    let res = tokio::task::spawn_blocking(move || -> anyhow::Result<BrainLoad> {
        let scope = open_brain_scope(&options, active)?;
        let entries = scope.query("", None, ninox_core::QueryFilters::default())?;
        let links = scope.links_all()?;
        Ok((entries, links))
    })
    .await;
    match res {
        Ok(Ok((entries, links))) => {
            st.brain.entries = entries;
            st.brain.set_edges(&links);
            st.brain.error = None;
            st.brain.reveal_cursor();
        }
        Ok(Err(e)) => st.brain.error = Some(format!("brain unavailable: {e}")),
        Err(e) => st.brain.error = Some(format!("brain query failed: {e}")),
    }
    if let Some(id) = st.brain.selected_entry().map(|e| e.id.clone()) {
        load_brain_links(st, id).await;
    }
}

/// Backlinks ("Cited by"), related entries, and outlinks ("Links to") for
/// one entry, in that order — what `BrainView::set_links` applies.
type BrainLinks = (Vec<ninox_core::BrainEntry>, Vec<ninox_core::BrainEntry>, Vec<ninox_core::BrainEntry>);

/// Fetch `id`'s backlinks/related/outlinks and apply them if it's still
/// selected by the time the fetch returns (`set_links` guards the race).
/// Fired whenever the Brain tab's selection changes — `load_brain`'s
/// initial load, and `brain_key`'s post-dispatch selection-change check.
async fn load_brain_links(st: &mut TuiState, id: String) {
    let options = st.brain.catalogue_options.clone();
    let active = st.brain.active_catalogue;
    let for_task = id.clone();
    let res = tokio::task::spawn_blocking(move || -> anyhow::Result<BrainLinks> {
        let scope = open_brain_scope(&options, active)?;
        Ok((scope.backlinks(&for_task)?, scope.related(&for_task, 6)?, scope.outlinks(&for_task)?))
    })
    .await;
    match res {
        Ok(Ok((backlinks, related, outlinks))) => st.brain.set_links(&id, backlinks, related, outlinks),
        _ => st.brain.set_links(&id, Vec::new(), Vec::new(), Vec::new()),
    }
}

/// The brain's own hybrid search (keyword + embeddings, as `ninox brain
/// query` runs it) for `query`, as ranked ids. Blocking: the first call
/// loads the embedding model. Empty when no model is available — the text
/// search already covers keywords.
fn semantic_brain_search(path: &std::path::Path, query: &str) -> Vec<String> {
    use ninox_core::embeddings::{Embedder, FastEmbedEmbedder};
    static EMBEDDER: std::sync::OnceLock<Option<FastEmbedEmbedder>> = std::sync::OnceLock::new();
    let Some(embedder) = EMBEDDER.get_or_init(|| FastEmbedEmbedder::try_new_with_progress(false).ok()) else { return Vec::new() };
    let Ok(index) = ninox_core::BrainIndex::open(path) else { return Vec::new() };
    index
        .query(query, Some(embedder as &dyn Embedder), ninox_core::QueryFilters::default())
        .map(|entries| entries.into_iter().map(|e| e.id).collect())
        .unwrap_or_default()
}

/// Index a brain change off the loop (the rebuild may embed); the result
/// comes back through `brain_tx` and reloads the list.
fn start_brain_job(st: &mut TuiState, lp: &Loop, job: brain::Job) {
    let config = AppConfig::load_or_default();
    let fallback = config.resolved_brain_path();
    let paths = brain::brain_job_paths(&st.brain.catalogue_options, st.brain.active_catalogue, &job, &fallback);
    let job = brain::unqualify_job_id(job, st.brain.active_catalogue);
    let what = match &job {
        brain::Job::ReindexAll if paths.len() > 1 => format!("reindexing {} catalogues", paths.len()),
        brain::Job::ReindexAll => "reindexing the brain".to_string(),
        _ => format!("indexing {}", job.id()),
    };
    st.brain.indexing = Some(what.clone());
    st.notify(Level::Info, format!("{what}…"));
    let tx = lp.brain_tx.clone();
    tokio::spawn(async move {
        let mut summaries = Vec::with_capacity(paths.len());
        for path in paths {
            match brain::run_job(path, config.clone(), job.clone(), true).await {
                Ok(summary) => summaries.push(summary),
                Err(e) => {
                    let _ = tx.send(brain::JobDone { job, result: Err(e) });
                    return;
                }
            }
        }
        let result = Ok(summaries.join(" · "));
        let _ = tx.send(brain::JobDone { job, result });
    });
}

async fn brain_job_done(st: &mut TuiState, done: brain::JobDone) {
    st.brain.indexing = None;
    if matches!(done.job, brain::Job::Delete { .. }) && done.result.is_ok() {
        st.brain.open = false;
    }
    match done.result {
        Ok(msg) => st.notify(Level::Info, msg),
        Err(e) => st.notify(Level::Error, e),
    }
    load_brain(st).await;
}

/// Brain › Sources' rows: every configured source and its entry count /
/// remote status (opening each index, so off the loop).
async fn load_sources(st: &mut TuiState) {
    let catalogues = AppConfig::load_or_default().catalogue_options();
    let cats = catalogues.clone();
    let loaded = tokio::task::spawn_blocking(move || {
        cats.iter().map(|c| (c.path.clone(), ninox_ui_shared::brain_sources::load_status(c))).collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    st.brain.sources.set_catalogues(catalogues, loaded);
}

/// A source was added, edited, toggled or removed: the Sources rows and
/// the brain list (whose "All sources" scope and `c` cycle follow config)
/// both reload.
async fn after_source_change(st: &mut TuiState) {
    load_sources(st).await;
    load_brain(st).await;
}

fn start_source_syncs(st: &mut TuiState, lp: &Loop, names: &[String]) {
    let started = st.brain.sources.start_sync(names);
    if started.is_empty() {
        return;
    }
    st.notify(Level::Info, match started.as_slice() {
        [(name, _)] => format!("syncing {name}…"),
        many => format!("syncing {} sources…", many.len()),
    });
    let config = AppConfig::load_or_default();
    for (name, path) in started {
        let tx = lp.sources_tx.clone();
        let config = config.clone();
        tokio::spawn(async move {
            let result = ninox_ui_shared::brain_sources::sync_source(&config, &path).await;
            // Pulled entries are only searchable once the index is rebuilt.
            if result.is_ok() {
                let p = path.clone();
                let rebuilt = tokio::task::spawn_blocking(move || ninox_core::BrainIndex::open(&p).and_then(|idx| idx.rebuild(None))).await;
                if let Ok(Err(e)) = rebuilt {
                    tracing::warn!("sources: rebuild after sync of {name}: {e}");
                }
            }
            let _ = tx.send(sources::SyncDone { name, path, result });
        });
    }
}

async fn source_synced(st: &mut TuiState, done: sources::SyncDone) {
    let clock = report::fmt_clock(now_ms(), now_ms());
    match st.brain.sources.sync_done(&done, &clock) {
        Ok(msg) => st.notify(Level::Info, msg),
        Err(e) => st.notify(Level::Error, e),
    }
    if done.result.is_ok() {
        after_source_change(st).await;
    }
}

/// The desktop app's Resume: relaunch under the same id with `--resume`.
/// Resuming a session that isn't running counts against the session limit
/// and may be queued instead (`Ok(Some(message))`).
async fn resume(engine: Arc<Engine>, session: ninox_core::types::Session, is_orch: bool) -> Result<Option<String>, String> {
    let config = AppConfig::load_or_default();
    match ninox_ui_shared::restart::resume_gated(engine, session, is_orch, &config, false, false).await? {
        ninox_ui_shared::restart::ResumeOutcome::Resumed(_) => Ok(None),
        ninox_ui_shared::restart::ResumeOutcome::Queued(message) => Ok(Some(message)),
    }
}

/// The startup hint when the prefix may never arrive: Ctrl-Space on macOS
/// is usually the input-source shortcut.
fn prefix_notice(prefix: u8) -> Option<String> {
    (cfg!(target_os = "macos") && prefix == 0x00).then(|| {
        "Ctrl+Space may never reach nx on macOS (input sources) — Ctrl+] always returns to the fleet; change the prefix in Settings (s)".into()
    })
}

/// Make a saved setting take effect in this TUI where it can; otherwise say
/// when it will.
fn apply_live_setting(st: &mut TuiState, lp: &mut Loop, id: &settings::FieldId, cfg: &AppConfig) -> &'static str {
    use settings::FieldId;
    match id {
        FieldId::Prefix => {
            if let Ok(b) = cfg.tui.prefix_byte() {
                st.prefix = b;
            }
            ""
        }
        FieldId::Colors | FieldId::Theme => {
            st.palette = palette::Palette::load(cfg);
            ""
        }
        FieldId::RestorePolicy => {
            st.restore_policy = backend::restore_policy(cfg);
            ""
        }
        FieldId::OrchestratorResetMode | FieldId::OrchestratorResetPct => {
            st.reset_cfg = cfg.orchestrator_reset.clone();
            ""
        }
        FieldId::RuntimeBackend => {
            // Start the host now so the first new session doesn't wait for it.
            if cfg.runtime.backend == ninox_core::runtime::Backend::Ptyd && st.ptyd != PtydState::Up && lp.ptyd_boot.is_none() {
                lp.ptyd_boot_starts = true;
                lp.ptyd_boot_for_viewers = false;
                lp.ptyd_boot = Some(tokio::spawn(backend::ensure_ptyd(true)));
            }
            if cfg.runtime.backend == ninox_core::runtime::Backend::Ptyd {
                // The host now runs this TUI's new sessions too.
                HOST_FOR_VIEWERS.store(false, Ordering::SeqCst);
            }
            " (new sessions)"
        }
        FieldId::Port => " (next engine start)",
        _ => "",
    }
}

/// Spawn `program arg` detached (stdio silenced, not awaited inline),
/// notifying `st` with `on_success` if it starts or the spawn error
/// otherwise. Shared by `Action::OpenUrl` and the GUI-editor branch of
/// `Action::OpenInEditor` — both fire-and-forget a program on a path/URL
/// the same way.
fn spawn_detached(st: &mut TuiState, program: &str, arg: &str, on_success: String) {
    let child = tokio::process::Command::new(program)
        .arg(arg)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match child {
        Ok(mut c) => {
            tokio::spawn(async move { c.wait().await });
            st.notify(Level::Info, on_success);
        }
        Err(e) => st.notify(Level::Error, format!("{program} {arg}: {e}")),
    }
}

/// Pulled out of the `suspended` closures below so it's testable without
/// actually spawning anything.
fn status_note(label: &str, result: std::io::Result<std::process::ExitStatus>) -> Option<String> {
    match result {
        Ok(s) if s.success() => None,
        Ok(s) => Some(format!("{label} exited {s}")),
        Err(e) => Some(format!("could not run {label}: {e}")),
    }
}

/// Suspend the TUI while `$VISUAL`/`$EDITOR` (else `vi`) edits `path`, like
/// the full-screen tmux attach. `Some` describes a failure.
fn edit_suspended(terminal: &mut Term, path: &std::path::Path) -> Option<String> {
    let editor = ["VISUAL", "EDITOR"].iter().find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty())).unwrap_or_else(|| "vi".into());
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    suspended(terminal, || {
        // Through sh so an editor setting with arguments (`code -w`) works.
        status_note(&editor, std::process::Command::new("sh").arg("-c").arg(format!("{editor} \"$1\"")).arg("sh").arg(path).status())
    })
}

/// Suspend the TUI to run the configured editor's `program` on `path`, like
/// the full-screen tmux attach — for Neovim, which has no window of its own
/// to open detached into. `Some` describes a failure.
fn open_terminal_editor_suspended(terminal: &mut Term, program: &str, path: &str) -> Option<String> {
    suspended(terminal, || status_note(program, std::process::Command::new(program).arg(path).status()))
}

/// Leave the TUI's terminal modes, run `child` on the real terminal, and
/// restore. Every failure, the terminal's included, comes back as a note
/// rather than ending the TUI.
fn suspended(terminal: &mut Term, child: impl FnOnce() -> Option<String>) -> Option<String> {
    let mut notes = Vec::new();
    if let Err(e) = crossterm::terminal::disable_raw_mode() {
        notes.push(format!("could not leave raw mode: {e}"));
    }
    disable_modes(terminal.backend_mut());
    notes.extend(child());
    let restore = (|| -> std::io::Result<()> {
        crossterm::terminal::enable_raw_mode()?;
        enable_modes(terminal.backend_mut())?;
        terminal.clear()?;
        while crossterm::event::poll(std::time::Duration::ZERO)? {
            crossterm::event::read()?;
        }
        Ok(())
    })();
    if let Err(e) = restore {
        notes.push(format!("could not restore the terminal: {e}"));
    }
    (!notes.is_empty()).then(|| notes.join("; "))
}

fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Run tmux attach as a *child* (not exec — we come back) on the real
/// terminal. `Some` describes a failure.
fn attach_suspended(terminal: &mut Term, argv: Vec<String>) -> Option<String> {
    suspended(terminal, || {
        // A TUI run inside tmux would otherwise have the attach refuse to nest;
        // ninox's server is a separate socket, so nesting is what the user asked.
        status_note("tmux attach", std::process::Command::new(&argv[0]).args(&argv[1..]).env_remove("TMUX").status())
    })
}

/// Run `ninox machine add <host> [--label <label>]` in the foreground on the
/// real terminal, like `attach_suspended`. `exe` is resolved the same way
/// every other re-exec in this crate resolves itself (`canonical_exe`, not a
/// bare `ninox` off `$PATH`) so a test build never shells out to a different
/// installed version; host/label are passed as separate argv entries, never
/// interpolated into a shell string. Waits for Enter before restoring the
/// TUI so the install/SSH/session-picker prompts' final output is readable.
fn machine_add_suspended(terminal: &mut Term, exe: &std::path::Path, host: &str, label: Option<&str>) -> Option<String> {
    suspended(terminal, || {
        let mut cmd = std::process::Command::new(exe);
        cmd.args(["machine", "add", host]);
        if let Some(label) = label {
            cmd.args(["--label", label]);
        }
        let note = match cmd.status() {
            Ok(s) if s.success() => None,
            Ok(s) => Some(format!("machine add failed (exit {})", s.code().map_or_else(|| "?".to_string(), |c| c.to_string()))),
            Err(e) => Some(format!("could not run ninox machine add: {e}")),
        };
        println!("\nPress Enter to return to ninox…");
        let mut discard = String::new();
        let _ = std::io::stdin().read_line(&mut discard);
        note
    })
}

fn refresh(st: &mut TuiState, store: &Store) {
    // Read before sessions: every request's asker existed before it asked,
    // so this order never shows a just-spawned asker as missing (= stale).
    let pending_user_requests = store.open_user_requests(None);
    let config = AppConfig::load_or_default();
    let Ok(view) = ninox_ui_shared::session_queue::queue_view(store, &config) else {
        return;
    };
    let ninox_ui_shared::session_queue::QueueView { sessions, orchestrators: orchs, positions, limits } = view;
    st.limits_headline = ninox_ui_shared::session_queue::limits_headline(&limits);
    let machine_health = store.machine_health().unwrap_or_default();
    st.set_remote_rows(crate::tui::remotes::collect(&config, &machine_health));
    if st.workspaces.open {
        st.set_workspace_rows(crate::tui::workspaces_panel::collect(store, crate::tui::workspaces_panel::retention_millis()));
    }
    st.brain_lines = sessions.iter().map(|s| (s.id.clone(), ninox_core::brain_set::session_brains_line(s, &config))).collect();
    let mut new_rows = build_rows(ninox_ui_shared::cli::group_sessions(sessions, orchs), &machine_health, &positions);
    let mut session_prs = ninox_core::session_prs::for_sessions(store, new_rows.iter().map(|r| &r.session));
    for r in &mut new_rows {
        r.prs = session_prs.remove(r.id()).unwrap_or_default();
    }
    st.apply_rows(new_rows);
    st.fleet.sync(&st.rows, &machine_health);
    st.fleet.sync_detail_pr(&st.rows, store);
    st.pr_watches = store.list_pr_watches().unwrap_or_default();
    st.pending_user_requests = pending_user_requests.unwrap_or_default();
    let idle_now = ninox_ui_shared::idle_cli::now_millis();
    if let Ok(report) = ninox_core::idle_workers::load_report(store, &config.idle_workers, idle_now) {
        st.idle.set_report(report, idle_now);
    }
    st.ghost_plans = ninox_ui_shared::ghost_view::load(store, st.now_ms);
    let orphans = state::orphan_watch_count(&st.rows, &st.pr_watches);
    if orphans != st.orphan_watch_count && orphans > 0 {
        st.notify(Level::Info, format!("{orphans} PR watch{} not shown on any session — see `ninox open --pr`", if orphans == 1 { "" } else { "es" }));
    }
    st.orphan_watch_count = orphans;
    if let Ok(counts) = store.message_delivered_counts() {
        for (id, n) in &counts {
            // Messages delivered before this TUI first saw a session are
            // not "unread" here.
            st.msg_seen.entry(id.clone()).or_insert(*n);
        }
        st.msg_counts = counts;
    }
    if let (Ok(held), Ok(by_sender)) = (store.held_message_counts(), store.unreachable_held_by_sender()) {
        for note in st.apply_held(held, by_sender) {
            st.notify(Level::Warn, note);
        }
    }
}

fn build_rows(
    groups: Vec<(Option<ninox_core::types::Orchestrator>, Vec<ninox_core::types::Session>)>,
    machine_health: &std::collections::HashMap<String, ninox_core::types::MachineHealth>,
    queue_positions: &std::collections::HashMap<String, usize>,
) -> Vec<Row> {
    let mut rows = Vec::new();
    for (orch, members) in groups {
        for s in members {
            // This is the TUI's single row-construction choke point —
            // every fleet/island/Board/count/selection/connect surface is
            // built from `rows`, so excluding the prompt-agent singleton
            // here keeps it out of all of them at once.
            if ninox_core::worker_type::is_prompt_agent_session(&s) {
                continue;
            }
            let machine_health = match s.kind_data() {
                ninox_core::types::SessionKindData::Remote { machine_id } => machine_health.get(&machine_id).copied(),
                ninox_core::types::SessionKindData::Local => None,
            };
            rows.push(Row {
                is_orchestrator: orch.as_ref().is_some_and(|o| o.id == s.id),
                group: orch.as_ref().map(|o| o.id.clone()),
                queue_position: queue_positions.get(&s.id).copied(),
                prs: Vec::new(),
                session: s,
                machine_health,
            });
        }
    }
    rows
}

/// The GUI's Delete… for one worker: it ends even if its row still reads
/// live, and a worktree with unsaved or unpushed work is kept.
async fn remove_worker(engine: &Arc<Engine>, id: &str) -> Result<String, String> {
    use ninox_core::events::DeleteOutcome;
    let plan = engine.plan_delete(id).map_err(|e| format!("remove failed: {e}"))?;
    let results = engine.execute_delete(&plan, false).await;
    match results.iter().find(|(r, _)| r == id).map(|(_, o)| o) {
        Some(DeleteOutcome::Deleted) => Ok(format!("removed {id} (its branch is kept)")),
        Some(DeleteOutcome::KeptWorktree(why)) => Ok(format!("kept {id} and its worktree: {why}")),
        Some(DeleteOutcome::Failed(e)) => Err(format!("remove failed: {e}")),
        None => Err(format!("remove failed: {id} was not in its own plan")),
    }
}

/// A ghost plan for TUI tests: `ids` as top-level orchestrator nodes.
#[cfg(test)]
pub(crate) fn ghost_fixture(
    id: &str,
    ids: &[&str],
    status: ninox_core::ghost_plan::PlanStatus,
    created_at: i64,
) -> ninox_core::ghost_plan::PlanWithNodes {
    use ninox_core::ghost_plan::{NodeRow, NodeState, PlanRow, PlanWithNodes};
    PlanWithNodes {
        plan: PlanRow {
            id: id.into(),
            proposer: ninox_core::worker_type::PROMPT_SESSION_ID.into(),
            created_at,
            expires_at: created_at + 1_000_000,
            status,
            target_parent: None,
            title: Some("Q4 objectives".into()),
            decided_by: None,
            decided_at: None,
        },
        nodes: ids
            .iter()
            .enumerate()
            .map(|(i, n)| NodeRow {
                plan_id: id.into(),
                node_id: n.to_string(),
                parent_node_id: None,
                worker_type: "Orchestrator".into(),
                name: n.to_string(),
                brief: format!("Own the {n} objective."),
                workspace: None,
                model: None,
                brain: None,
                dependencies: vec![],
                await_done: false,
                linear_ref: None,
                state: NodeState::Ghost,
                session_id: None,
                error: None,
                updated_at: 0,
                seq: i as i64,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spawn_warnings_land_on_the_status_line_not_stderr() {
        let _iso = ninox_core::test_isolation::isolate();
        let store = Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap();
        let mut st = TuiState::default();
        let handle: SpawnHandle = tokio::spawn(async {
            Ok(ninox_ui_shared::cli::SpawnedSession {
                id: "w1".into(),
                workspace: String::new(),
                queued: None,
                warnings: vec!["w1 is still starting up — sent the prompt anyway".into()],
            })
        });
        while !handle.is_finished() {
            tokio::task::yield_now().await;
        }
        let mut pending = vec![handle];
        drain_finished_spawns(&mut st, &store, &mut pending).await;

        let notice = st.notice.expect("a status-line notice");
        assert_eq!(notice.level, Level::Warn);
        assert_eq!(notice.text, "spawned w1 — w1 is still starting up — sent the prompt anyway");
    }

    /// A row still reading `Working` after its pane died (what puts Remove
    /// on screen) must still go: a non-forced `remove_session` refused it.
    #[tokio::test]
    async fn remove_takes_a_worker_whose_row_still_reads_live() {
        let _iso = ninox_core::test_isolation::isolate();
        let store = Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut worker = ninox_core::types::Session {
            id: "w1".into(), orchestrator_id: None, name: "w1".into(), repo: "r".into(),
            status: ninox_core::types::SessionStatus::Working, agent_type: "claude-code".into(),
            cost_usd: 0.0, started_at: 0, pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, observed_model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None, machine_id: None,
            worker_type: None, success_criteria: Vec::new(), completion_outcome: None,
            self_completed_at: None, cleanup_result: None, cleanup_at: None,
        };
        store.upsert_session(&worker).unwrap();
        worker.id = "w2".into();
        worker.status = ninox_core::types::SessionStatus::Interrupted;
        store.upsert_session(&worker).unwrap();
        let engine = Engine::new(store.clone());

        for id in ["w1", "w2"] {
            assert_eq!(remove_worker(&engine, id).await, Ok(format!("removed {id} (its branch is kept)")));
            assert!(store.get_session(id).unwrap().is_none(), "{id} must be gone");
        }
    }

    /// Run by `cargo test --release`; in a debug build the fixture is meant
    /// to seed, which `reply_fixture`'s own tests cover.
    #[cfg(not(debug_assertions))]
    #[test]
    fn reply_fixture_is_compiled_out_of_release_builds() {
        let mut st = TuiState::default();
        assert!(!seed_reply_fixture(&mut st, Some("open")));
        assert!(!st.fleet.reply.drawer.has_content());
    }

    #[cfg(debug_assertions)]
    #[test]
    fn redraws_are_paced_only_while_the_thinking_spinner_is_on_screen() {
        let mut st = TuiState { view: View::Fleet, ..TuiState::default() };
        st.layout = layout::compute(Rect::new(0, 0, 120, 40), &st);
        assert!(!reply_is_thinking(&st), "no reply panel");

        seed_reply_fixture(&mut st, Some("thinking"));
        st.layout = layout::compute(Rect::new(0, 0, 120, 40), &st);
        assert!(reply_is_thinking(&st));

        st.fleet.reply.drawer.latest_mut().unwrap().answer = "first words".into();
        assert!(!reply_is_thinking(&st), "answer text is streaming in");
    }

    #[cfg(debug_assertions)]
    #[test]
    fn reply_fixture_seeds_in_debug_builds() {
        let mut st = TuiState::default();
        assert!(seed_reply_fixture(&mut st, Some("open")));
        assert!(st.fleet.reply.is_open());
        assert!(!seed_reply_fixture(&mut TuiState::default(), None));
    }

    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (input, want) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foobar", "Zm9vYmFy")] {
            assert_eq!(base64(input.as_bytes()), want);
        }
    }

    /// `status_note` is the pure boundary `edit_suspended` / `attach_suspended`
    /// / `open_terminal_editor_suspended` all funnel their child process's
    /// result through — exercised here without spawning anything, including
    /// nvim crashing or not existing (the two failure modes the "open in
    /// editor" acceptance criteria call out for terminal restoration).
    #[test]
    fn status_note_reports_a_clean_exit_as_none() {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status_note("nvim", Ok(std::process::ExitStatus::from_raw(0))), None);
    }

    #[test]
    fn status_note_describes_a_nonzero_exit() {
        use std::os::unix::process::ExitStatusExt;
        let note = status_note("nvim", Ok(std::process::ExitStatus::from_raw(1 << 8))).unwrap();
        assert!(note.contains("nvim") && note.contains("exited"), "{note}");
    }

    #[test]
    fn status_note_describes_a_missing_binary() {
        let err = std::io::Error::from(std::io::ErrorKind::NotFound);
        let note = status_note("nvim", Err(err)).unwrap();
        assert!(note.contains("nvim") && note.contains("could not run"), "{note}");
    }

    #[test]
    fn the_macos_hint_only_shows_for_ctrl_space() {
        assert_eq!(prefix_notice(0x00).is_some(), cfg!(target_os = "macos"));
        assert!(prefix_notice(0x1c).is_none());
        assert!(prefix_notice(0x00).is_none_or(|n| n.contains("Ctrl+]")));
    }

    fn info(pane: &str) -> ninox_ptyd::PaneInfo {
        ninox_ptyd::PaneInfo {
            pane: pane.into(), pid: 1, cols: 80, rows: 24, alive: true, exit_code: None,
            created_ms: 0, last_output_ms: 0, title: None, cwd: "/".into(), seq: 0, history_size: 0,
        }
    }

    #[test]
    fn a_viewer_host_is_retried_with_backoff() {
        let t0 = std::time::Instant::now();
        assert!(viewer_boot_due(None, t0));
        assert!(!viewer_boot_due(Some(t0), t0 + std::time::Duration::from_secs(3)));
        assert!(viewer_boot_due(Some(t0), t0 + VIEWER_HOST_RETRY));
    }

    #[test]
    fn losing_the_viewer_host_surfaces_instead_of_starting_forever() {
        use ninox_ui_shared::cli::test_fixtures::session;
        use ninox_core::types::SessionStatus;
        let mut st = TuiState {
            rows: vec![Row {
                session: session("w1", None, SessionStatus::Working),
                is_orchestrator: false, group: None, machine_health: None, queue_position: None, prs: Vec::new(),
            }],
            ..Default::default()
        };
        apply_pane_event(&mut st, live::PaneEvent::HostDown("never ran".into()));
        assert!(st.viewers_unavailable.is_none(), "no host yet is not an outage: the TUI starts one for viewers");

        let mine = ninox_core::runtime::viewer_pane_id(std::process::id(), "w1");
        apply_pane_event(&mut st, live::PaneEvent::Panes(vec![info(&mine)]));
        apply_pane_event(&mut st, live::PaneEvent::HostDown("crashed".into()));
        assert!(st.viewers_unavailable.as_deref().is_some_and(|e| e.contains("crashed")));
        apply_pane_event(&mut st, live::PaneEvent::Panes(vec![]));
        assert!(st.viewers_unavailable.is_none(), "a restarted host clears it");
    }

    #[test]
    fn viewer_panes_are_kept_apart_from_session_panes() {
        let mut st = TuiState::default();
        let mine = ninox_core::runtime::viewer_pane_id(std::process::id(), "w1");
        let other_tui = ninox_core::runtime::viewer_pane_id(std::process::id().wrapping_add(1), "w1");
        apply_pane_event(&mut st, live::PaneEvent::Panes(vec![info("s0"), info(&mine), info(&other_tui)]));
        assert_eq!(st.panes.keys().collect::<Vec<_>>(), ["s0"], "viewers are never sessions");
        assert_eq!(st.viewers.get("w1").map(|v| v.pane.as_str()), Some(mine.as_str()));
        assert_eq!(st.pane_info(&mine).map(|p| p.pane.as_str()), Some(mine.as_str()));
        assert!(st.pane_info(&other_tui).is_none(), "another TUI's viewer is ignored");

        apply_pane_event(&mut st, live::PaneEvent::Exited { pane: mine, code: Some(0) });
        assert!(!st.viewers["w1"].alive);
        assert!(st.viewer_errors.contains_key("w1"), "a closed view waits for the user to reopen it");
        assert!(st.notice.is_none(), "an off-screen viewer closing is not news");
    }

    #[test]
    fn rows_mark_orchestrators_and_their_group() {
        use ninox_ui_shared::cli::test_fixtures::session;
        use ninox_core::types::SessionStatus;
        let orch = ninox_core::types::Orchestrator { id: "o".into(), name: "o".into(), created_at: 0 };
        let groups = vec![(
            Some(orch),
            vec![session("o", None, SessionStatus::Working), session("w", Some("o"), SessionStatus::Working)],
        )];
        let rows = build_rows(groups, &std::collections::HashMap::new(), &std::collections::HashMap::new());
        assert!(rows[0].is_orchestrator);
        assert!(!rows[1].is_orchestrator);
        assert_eq!(rows[1].group.as_deref(), Some("o"));
    }

    #[test]
    fn rows_never_include_the_prompt_agent_singleton() {
        use ninox_ui_shared::cli::test_fixtures::session;
        use ninox_core::types::SessionStatus;
        let mut prompt_agent = session(ninox_core::worker_type::PROMPT_SESSION_ID, None, SessionStatus::Working);
        prompt_agent.worker_type = Some(ninox_core::worker_type::PROMPT_WORKER_TYPE.into());
        let ordinary = session("w", None, SessionStatus::Working);
        let groups = vec![(None, vec![prompt_agent, ordinary])];

        let rows = build_rows(groups, &std::collections::HashMap::new(), &std::collections::HashMap::new());

        assert_eq!(rows.iter().map(|r| r.id()).collect::<Vec<_>>(), ["w"], "the prompt-agent singleton must never surface as a fleet row");
    }

    #[test]
    fn an_ordinary_spawn_type_prompt_session_is_not_mistaken_for_the_singleton() {
        use ninox_ui_shared::cli::test_fixtures::session;
        use ninox_core::types::SessionStatus;
        let mut one_shot = session("some-other-id", None, SessionStatus::Working);
        one_shot.worker_type = Some(ninox_core::worker_type::PROMPT_WORKER_TYPE.into());
        let groups = vec![(None, vec![one_shot])];

        let rows = build_rows(groups, &std::collections::HashMap::new(), &std::collections::HashMap::new());

        assert_eq!(rows.iter().map(|r| r.id()).collect::<Vec<_>>(), ["some-other-id"], "only the reserved id+type pair is the singleton, not any `--type prompt` spawn");
    }

    #[test]
    fn rows_pick_up_their_session_machine_health_by_machine_id() {
        use ninox_ui_shared::cli::test_fixtures::session;
        use ninox_core::types::{MachineHealth, SessionStatus};
        let mut remote = session("r", None, SessionStatus::Working);
        remote.machine_id = Some("m1".into());
        let local = session("l", None, SessionStatus::Working);
        let groups = vec![(None, vec![remote, local])];
        let mut health = std::collections::HashMap::new();
        health.insert("m1".to_string(), MachineHealth::Reconnecting);

        let rows = build_rows(groups, &health, &std::collections::HashMap::new());
        assert_eq!(rows[0].machine_health, Some(MachineHealth::Reconnecting));
        assert_eq!(rows[1].machine_health, None, "a local session (no machine_id) must never pick up a badge");
    }

    fn catalogue(name: &str, dir: &tempfile::TempDir) -> ninox_core::config::CatalogueRef {
        ninox_core::config::CatalogueRef {
            name: name.to_string(),
            path: dir.path().to_path_buf(),
            remote: None,
            endpoint: None,
            region: None,
            cache_ttl_secs: None,
            enabled: true,
            cadence: "manual".to_string(),
            access: "read_write".to_string(),
            write_rules_override: None,
        }
    }

    fn write_entry(dir: &tempfile::TempDir, rel: &str, body: &str) {
        let path = dir.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn open_brain_scope_opens_the_selected_single_catalogue() {
        let a = tempfile::tempdir().unwrap();
        write_entry(&a, "concepts/widget.md", "# Widget");
        ninox_core::BrainIndex::open(a.path()).unwrap().rebuild(None).unwrap();
        let b = tempfile::tempdir().unwrap();
        write_entry(&b, "concepts/gadget.md", "# Gadget");
        ninox_core::BrainIndex::open(b.path()).unwrap().rebuild(None).unwrap();

        let options = vec![catalogue("alpha", &a), catalogue("beta", &b)];
        let scope = open_brain_scope(&options, Some(1)).unwrap();
        let entries = scope.query("", None, ninox_core::QueryFilters::default()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "gadget");
    }

    #[test]
    fn open_brain_scope_merges_all_sources_when_none() {
        let a = tempfile::tempdir().unwrap();
        write_entry(&a, "concepts/widget.md", "# Widget");
        ninox_core::BrainIndex::open(a.path()).unwrap().rebuild(None).unwrap();
        let b = tempfile::tempdir().unwrap();
        write_entry(&b, "concepts/gadget.md", "# Gadget");
        ninox_core::BrainIndex::open(b.path()).unwrap().rebuild(None).unwrap();

        let options = vec![catalogue("alpha", &a), catalogue("beta", &b)];
        let scope = open_brain_scope(&options, None).unwrap();
        assert!(scope.is_all());
        let entries = scope.query("", None, ninox_core::QueryFilters::default()).unwrap();
        assert_eq!(entries.len(), 2);
    }
}
