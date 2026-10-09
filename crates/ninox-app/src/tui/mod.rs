//! Terminal UI: fleet sidebar + live agent panes composited client-side
//! from ptyd screens (spec §4). The loop owns all I/O; `state` is the pure
//! reducer, `view` the pure renderer, `live` the off-loop pane plumbing.

pub mod backend;
mod brain;
mod keys;
mod layout;
mod live;
mod palette;
mod pane;
mod prs;
mod report;
mod settings;
pub mod state;
mod view;

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::editor;
use ninox_core::{config::AppConfig, events::Engine, store::Store};
use ratatui::layout::Rect;
use state::{Action, Level, PtydState, Row, TuiState, View};

type SpawnHandle = tokio::task::JoinHandle<anyhow::Result<crate::SpawnedOrchestrator>>;
type ResumeHandle = (String, tokio::task::JoinHandle<Result<(), String>>);
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

pub async fn run(store: Arc<Store>, port: u16, db_path: PathBuf) -> anyhow::Result<()> {
    let config = AppConfig::load().unwrap_or_default();
    let engine = Engine::new(Arc::clone(&store));
    let mut st = TuiState { now_ms: now_ms(), ..Default::default() };
    st.started_ms = st.now_ms;
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
    st.restore_policy = backend::restore_policy(&config);
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
    /// Kill/remove/reap run off the loop (git worktree removal can take
    /// seconds); results come back here.
    op_tx: tokio::sync::mpsc::UnboundedSender<OpDone>,
    ops_in_flight: HashSet<String>,
    /// The orchestrator whose workers' uncommitted changes were last
    /// gathered for its remove confirm.
    uncommitted_for: Option<String>,
    uncommitted_tx: tokio::sync::mpsc::UnboundedSender<HashMap<String, usize>>,
}

/// A finished kill/remove/reap: which session it was for and the notice.
struct OpDone {
    id: String,
    removed: bool,
    result: Result<String, String>,
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
    let (report_tx, mut report_rx) = tokio::sync::mpsc::unbounded_channel();
    let (brain_tx, mut brain_rx) = tokio::sync::mpsc::unbounded_channel();
    let (brain_search_tx, mut brain_search_rx) = tokio::sync::mpsc::unbounded_channel();
    let (op_tx, mut op_rx) = tokio::sync::mpsc::unbounded_channel::<OpDone>();
    let (uncommitted_tx, mut uncommitted_rx) = tokio::sync::mpsc::unbounded_channel();
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
        op_tx,
        ops_in_flight: HashSet::new(),
        uncommitted_for: None,
        uncommitted_tx,
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
            sync_uncommitted(st, &mut lp);
            if sync_report(st, store, &mut lp) {
                // A new selection's report starts at the top.
                st.layout = layout::compute(Rect::new(0, 0, size.width, size.height), st);
            }
            resize_focused(st, &st.layout, &mut lp);
            draw_synchronized(terminal, st)?;
            last_draw = std::time::Instant::now();
            dirty = Redraw::No;
        }
        let frame_due = tokio::time::sleep_until((last_draw + PANE_FRAME).into());
        tokio::select! {
            _ = frame_due, if dirty == Redraw::Paced => {}
            _ = tick.tick() => {
                refresh(st, store);
                st.offer_restore(backend::pending_restore(store));
                lp.hub.refresh_list();
                lp.hub.fit_viewers(
                    st.viewers.iter().filter(|(_, v)| v.alive).map(|(s, v)| (s.clone(), v.pane.clone(), v.cols, v.rows)).collect(),
                );
                drain_finished_spawns(st, &mut lp.pending_spawns).await;
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
                dirty = Redraw::Now;
            }
            Some((query, ids)) = brain_search_rx.recv() => {
                st.brain.semantic_done(query, ids);
                dirty = Redraw::Now;
            }
            Some(done) = op_rx.recv() => {
                lp.ops_in_flight.remove(&done.id);
                if done.removed {
                    st.reports.remove(&done.id);
                }
                match done.result {
                    Ok(msg) => st.notify(Level::Info, msg),
                    Err(e) => st.notify(Level::Error, e),
                }
                refresh(st, store);
                lp.hub.refresh_list();
                dirty = Redraw::Now;
            }
            Some(counts) = uncommitted_rx.recv() => {
                st.uncommitted = counts;
                dirty = Redraw::Now;
            }
            Some(done) = brain_rx.recv() => {
                brain_job_done(st, done).await;
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
                if perform(action, terminal, st, store, engine, &mut lp, &mut events).await? {
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
    match st.view {
        View::Board => {
            if let Some(id) = st.selected_id() {
                if let Some(target) = st.pane_target(&id) {
                    let page = st.layout.pane_inner.map(|r| r.height as usize).unwrap_or(24);
                    let scroll = st.views.get(&target).map(|v| v.scroll).unwrap_or(0);
                    wanted.insert(target, if scroll > 0 { scroll + page } else { 0 });
                }
                on_screen.push(id);
            }
        }
        View::Overview => {
            let rows = st.overview_rows();
            for t in &st.layout.tiles {
                if let Some(r) = rows.get(t.index).and_then(|&i| st.rows.get(i)) {
                    if let Some(target) = st.pane_target(r.id()) {
                        wanted.insert(target, 0);
                    }
                    on_screen.push(r.id().to_string());
                }
            }
        }
        _ => {}
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
            st.clamp_overview_sel();
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
            st.clamp_overview_sel();
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
            Ok(Ok(())) => st.notify(Level::Info, format!("resumed {id}")),
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
async fn drain_finished_spawns(st: &mut TuiState, pending: &mut Vec<SpawnHandle>) {
    let mut still_running = Vec::new();
    for handle in pending.drain(..) {
        if handle.is_finished() {
            match handle.await {
                Ok(Ok(spawned)) => {
                    st.notify(Level::Info, format!("spawned {}", spawned.id));
                    if let Some(i) = st.rows.iter().position(|r| r.id() == spawned.id) {
                        st.selected = i;
                    }
                }
                Ok(Err(e)) => st.notify(Level::Error, format!("spawn failed: {e}")),
                Err(e) => st.notify(Level::Error, format!("spawn task panicked: {e}")),
            }
        } else {
            still_running.push(handle);
        }
    }
    *pending = still_running;
}

/// Returns `true` when the TUI should exit.
async fn perform(
    action: Action,
    terminal: &mut Term,
    st: &mut TuiState,
    store: &Arc<Store>,
    engine: &Arc<Engine>,
    lp: &mut Loop,
    events: &mut Option<crossterm::event::EventStream>,
) -> anyhow::Result<bool> {
    match action {
        Action::None => {}
        Action::Quit => return Ok(true),
        Action::Write { pane, bytes } => lp.hub.write(&pane, bytes),
        Action::Connect(id) => {
            use crate::connect::ConnectPlan;
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
                    st.notify(Level::Warn, format!("{id} has no live tmux session — {}", crate::status_slug(&status)));
                }
                Ok(ConnectPlan::NotFound { .. }) => st.notify(Level::Warn, format!("{id} not found")),
            }
            refresh(st, store);
        }
        Action::Spawn { name, prompt } => {
            st.notify(Level::Info, format!("spawning {name}…"));
            let store = Arc::clone(store);
            lp.pending_spawns.push(tokio::spawn(async move {
                let config = AppConfig::load().unwrap_or_default();
                crate::spawn_orchestrator_common(&store, &config, &name, prompt).await
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
        Action::Remove(id) => {
            let is_orch = st.is_orchestrator_id(&id);
            run_op(st, lp, id, true, {
                let engine = Arc::clone(engine);
                move |id| async move {
                    if !is_orch {
                        return match engine.remove_session(&id).await {
                            Ok(()) => Ok(format!("removed {id} (its branch is kept)")),
                            Err(e) => Err(format!("remove failed: {e}")),
                        };
                    }
                    match engine.remove_orchestrator_keeping_live(&id).await {
                        Ok(kept) if kept.is_empty() => Ok(format!("removed {id} (branches are kept)")),
                        Ok(kept) => Ok(format!("removed {id}; {} live or resumable worker{} kept: {}", kept.len(), if kept.len() == 1 { "" } else { "s" }, kept.join(", "))),
                        Err(e) => Err(format!("remove failed: {e}")),
                    }
                }
            });
        }
        Action::RemoveAll(id) => run_op(st, lp, id, true, {
            let engine = Arc::clone(engine);
            move |id| async move {
                match engine.remove_orchestrator(&id).await {
                    Ok(()) => Ok(format!("removed {id} and all its workers (branches are kept)")),
                    Err(e) => Err(format!("remove failed: {e}")),
                }
            }
        }),
        Action::Resume(id) => {
            if lp.pending_resumes.iter().any(|(r, _)| *r == id) {
                st.notify(Level::Info, format!("{id} is already resuming"));
                return Ok(false);
            }
            let Some(row) = st.rows.iter().find(|r| r.id() == id) else { return Ok(false) };
            let (session, is_orch) = (row.session.clone(), row.is_orchestrator);
            let engine = Arc::clone(engine);
            lp.pending_resumes.push((id, tokio::spawn(async move { resume(engine, session, is_orch).await })));
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
            let choice = AppConfig::load().unwrap_or_default().editor;
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
        Action::Reap(orch_id) => run_op(st, lp, orch_id, false, {
            let engine = Arc::clone(engine);
            move |id| async move {
                match engine.reap_workers(&id, ninox_core::events::ReapSelection::Finished, false).await {
                    Ok(outcomes) => Ok(format!("reaped {} workers", outcomes.len())),
                    Err(e) => Err(format!("reap failed: {e}")),
                }
            }
        }),
        Action::RestartAll => run_op(st, lp, "fleet-restart-all".to_string(), false, {
            let store = Arc::clone(store);
            let db_path = lp.db_path.clone();
            move |_id| async move {
                let config = AppConfig::load().unwrap_or_default();
                let args = crate::restart::RestartArgs { session_ids: vec![], all: true, exec_detached: false };
                match crate::restart::execute(args, store, config, db_path).await {
                    Ok(outcomes) if crate::restart::had_trouble(&outcomes) => Err(crate::restart::summarize(&outcomes)),
                    Ok(outcomes) => Ok(crate::restart::summarize(&outcomes)),
                    Err(e) => Err(format!("restart all failed: {e}")),
                }
            }
        }),
        Action::LoadBrain(_) => load_brain(st).await,
        Action::SearchBrain(query) => {
            st.brain.searching = Some(query.clone());
            let tx = lp.brain_search_tx.clone();
            let path = AppConfig::load().unwrap_or_default().resolved_brain_path();
            tokio::task::spawn_blocking(move || {
                let ids = semantic_brain_search(&path, &query);
                let _ = tx.send((query, ids));
            });
        }
        Action::EditBrain(id) => {
            let path = AppConfig::load().unwrap_or_default().resolved_brain_path();
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
            let path = AppConfig::load().unwrap_or_default().resolved_brain_path();
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
        Action::LoadSettings => st.settings.reload(settings::load()),
        Action::LoadPrs => {
            st.prs.watching = Some(AppConfig::load().unwrap_or_default().pr_watch.enabled);
            refresh(st, store);
        }
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
        Action::Restore => match backend::restore_fleet(&lp.db_path) {
            Ok(msg) => st.notify(Level::Info, msg),
            Err(e) => st.notify(Level::Error, format!("restore failed to start: {e}")),
        },
        Action::DismissRestore => backend::dismiss_pending_restore(store),
        Action::Yank(text) => {
            let lines = text.lines().count();
            let mut out = std::io::stdout();
            let _ = write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes()));
            let _ = out.flush();
            let s = if lines == 1 { "" } else { "s" };
            st.notify(Level::Info, format!("copied {lines} line{s} to the clipboard"));
        }
    }
    Ok(false)
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

/// While an orchestrator's remove confirm is open, count its workers'
/// uncommitted changes off the loop so the modal can say what is at stake.
fn sync_uncommitted(st: &mut TuiState, lp: &mut Loop) {
    let open = match &st.modal {
        Some(state::Modal::Confirm(state::Pending::Remove(id))) if st.is_orchestrator_id(id) => Some(id.clone()),
        _ => None,
    };
    if open == lp.uncommitted_for {
        return;
    }
    lp.uncommitted_for = open.clone();
    st.uncommitted.clear();
    let Some(orch) = open else { return };
    let workspaces: Vec<(String, Option<String>)> = st
        .rows
        .iter()
        .filter(|r| !r.is_orchestrator && r.group.as_deref() == Some(orch.as_str()))
        .map(|r| (r.session.id.clone(), r.session.workspace_path.clone()))
        .collect();
    let tx = lp.uncommitted_tx.clone();
    tokio::task::spawn_blocking(move || {
        let counts = workspaces
            .into_iter()
            .map(|(id, ws)| (id, ws.map_or(0, |ws| report::uncommitted(std::path::Path::new(&ws)))))
            .collect();
        let _ = tx.send(counts);
    });
}

/// Reload the brain list for `query`, keeping the cursor on its entry.
/// Every entry: the Brain tab searches client-side, so the list is always
/// the whole brain.
async fn load_brain(st: &mut TuiState) {
    let path = AppConfig::load().unwrap_or_default().resolved_brain_path();
    let res = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<ninox_core::BrainEntry>> {
        let index = ninox_core::BrainIndex::open(&path)?;
        index.query("", None, ninox_core::QueryFilters::default())
    })
    .await;
    match res {
        Ok(Ok(entries)) => {
            st.brain.entries = entries;
            st.brain.error = None;
            st.brain.reveal_cursor();
        }
        Ok(Err(e)) => st.brain.error = Some(format!("brain unavailable: {e}")),
        Err(e) => st.brain.error = Some(format!("brain query failed: {e}")),
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
    let config = AppConfig::load().unwrap_or_default();
    let path = config.resolved_brain_path();
    st.brain.indexing = Some(format!("indexing {}", job.id()));
    st.notify(Level::Info, format!("indexing {}…", job.id()));
    let tx = lp.brain_tx.clone();
    tokio::spawn(async move {
        let result = brain::run_job(path, config, job.clone(), true).await;
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

/// The desktop app's Resume: relaunch under the same id with `--resume`.
async fn resume(engine: Arc<Engine>, session: ninox_core::types::Session, is_orch: bool) -> Result<(), String> {
    let config = AppConfig::load().unwrap_or_default();
    let plan = crate::app::resume_plan(&session, is_orch, &config)
        .ok_or("no workspace or conversation id recorded, or the harness has no resume_args")?;
    let claude_session_id = session.claude_session_id.clone().ok_or("no conversation id recorded")?;
    // A failed launch leaves the row as it was (still resumable).
    let failure_status = session.status.clone();
    let req = crate::spawn_util::RelaunchRequest {
        session,
        is_orchestrator: is_orch,
        plan,
        claude_session_id,
        started_at: now_ms(),
        failure_status,
    };
    match crate::spawn_util::relaunch_in_place(engine, req, &config, false).await {
        Some(_) => Ok(()),
        None => Err("launch failed (see the ninox log)".into()),
    }
}

/// The startup hint when the prefix may never arrive: Ctrl-Space on macOS
/// is usually the input-source shortcut.
fn prefix_notice(prefix: u8) -> Option<String> {
    (cfg!(target_os = "macos") && prefix == 0x00).then(|| {
        "Ctrl+Space may never reach nx on macOS (input sources) — Ctrl+] always returns to the fleet; change the prefix in Settings (5)".into()
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

fn refresh(st: &mut TuiState, store: &Store) {
    let (Ok(sessions), Ok(orchs)) = (store.list_sessions(), store.list_orchestrators()) else {
        return;
    };
    st.apply_rows(build_rows(crate::group_sessions(sessions, orchs)));
    st.pr_watches = store.list_pr_watches().unwrap_or_default();
    let owned: Vec<_> = st
        .rows
        .iter()
        .filter(|r| r.session.pr_number.is_some())
        .map(|r| (r.session.clone(), r.session.pr_id.and_then(|id| store.get_pr(id).ok().flatten())))
        .collect();
    st.prs.rows = prs::collect(&owned, &st.pr_watches);
    if let Ok(counts) = store.message_delivered_counts() {
        for (id, n) in &counts {
            // Messages delivered before this TUI first saw a session are
            // not "unread" here.
            st.msg_seen.entry(id.clone()).or_insert(*n);
        }
        st.msg_counts = counts;
    }
}

fn build_rows(groups: Vec<(Option<ninox_core::types::Orchestrator>, Vec<ninox_core::types::Session>)>) -> Vec<Row> {
    let mut rows = Vec::new();
    for (orch, members) in groups {
        for s in members {
            rows.push(Row {
                is_orchestrator: orch.as_ref().is_some_and(|o| o.id == s.id),
                group: orch.as_ref().map(|o| o.id.clone()),
                session: s,
            });
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn losing_ptyd_clamps_the_overview_selection() {
        use crate::test_fixtures::session;
        use ninox_core::types::SessionStatus;
        let mut st = TuiState {
            rows: (0..3).map(|i| Row {
                session: session(&format!("s{i}"), None, SessionStatus::Terminated),
                is_orchestrator: false, group: None,
            }).collect(),
            ..Default::default()
        };
        apply_pane_event(&mut st, live::PaneEvent::Panes(
            (0..3).map(|i| ninox_ptyd::PaneInfo {
                pane: format!("s{i}"), pid: 1, cols: 80, rows: 24, alive: true, exit_code: None,
                created_ms: 0, last_output_ms: 0, title: None, cwd: "/".into(), seq: 0, history_size: 0,
            }).collect(),
        ));
        st.overview_sel = 2;
        apply_pane_event(&mut st, live::PaneEvent::HostDown("gone".into()));
        assert_eq!(st.overview_rows().len(), 0);
        assert_eq!(st.overview_sel, 0);
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
        use crate::test_fixtures::session;
        use ninox_core::types::SessionStatus;
        let mut st = TuiState {
            rows: vec![Row { session: session("w1", None, SessionStatus::Working), is_orchestrator: false, group: None }],
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
        use crate::test_fixtures::session;
        use ninox_core::types::SessionStatus;
        let orch = ninox_core::types::Orchestrator { id: "o".into(), name: "o".into(), created_at: 0 };
        let groups = vec![(
            Some(orch),
            vec![session("o", None, SessionStatus::Working), session("w", Some("o"), SessionStatus::Working)],
        )];
        let rows = build_rows(groups);
        assert!(rows[0].is_orchestrator);
        assert!(!rows[1].is_orchestrator);
        assert_eq!(rows[1].group.as_deref(), Some("o"));
    }
}
