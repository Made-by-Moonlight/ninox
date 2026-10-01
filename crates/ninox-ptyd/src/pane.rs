//! One pane = one process on a PTY the host owns, plus its emulator.
//!
//! Concurrency model, per pane (deliberately boring):
//!
//! - **reader thread** — `poll`s the PTY master (and the host-wide freeze
//!   pipe used by live upgrade), feeds every chunk into the engine under the
//!   pane's state mutex, routes emulator replies to the writer, forwards the
//!   (query-filtered) bytes to raw subscribers, then pokes the pane task.
//!   It never blocks on a client: raw subscriber queues are bounded and a
//!   full queue marks the subscriber lagged (it gets a fresh repaint later
//!   instead of the bytes it missed).
//! - **writer thread** — owns all writes to the PTY, in order: client input,
//!   emulator replies, and the paste/delay/Enter sequence of `Submit`. A
//!   child that stops reading its input can only stall this thread.
//! - **waiter thread** — `waitpid`s the child (or, for panes adopted in a
//!   live upgrade, polls for its disappearance) and marks the pane exited
//!   once the reader has drained the final output.
//! - **pane task** (tokio) — sleeps until poked, then does the periodic work:
//!   force-applies expired synchronized updates, recomputes the screen `seq`
//!   and publishes it to frames subscribers at most every [`FRAME_INTERVAL`]
//!   (only when someone is subscribed), and writes checkpoints.
//!
//! All mutexes are taken through [`lock`], which ignores poisoning: a panic
//! in one pane must never cascade into every later request.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::{mpsc, watch, Notify};

use crate::checkpoint::{self, Checkpoint};
use crate::engine::{AlacrittyEngine, TerminalEngine, DEFAULT_SCROLLBACK};
use crate::filter::QueryFilter;
use crate::protocol::*;

/// Minimum spacing of `ScreenChanged` notifications.
pub const FRAME_INTERVAL: Duration = Duration::from_millis(16);
/// Minimum spacing of checkpoint writes per pane.
pub const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(3);
/// Scrollback lines stored in a checkpoint.
pub const CHECKPOINT_SCROLLBACK: usize = 200;
/// Grace between SIGHUP and SIGKILL on `Kill`.
pub const KILL_GRACE: Duration = Duration::from_secs(2);
/// Gap between a submitted paste and its Enter. Claude Code's input box
/// treats a CR arriving in the same read as the paste as part of it.
pub const SUBMIT_ENTER_DELAY: Duration = Duration::from_millis(80);
/// Raw subscriber queue depth (chunks) before it is marked lagged.
const RAW_QUEUE: usize = 512;
/// Input bytes queued for a pane's writer before further input is refused
/// (a child that stopped reading would otherwise grow the queue without
/// bound). A single message larger than this is still accepted when the
/// queue is empty.
pub const MAX_QUEUED_INPUT: usize = 4 * 1024 * 1024;
/// How long the waiter lets the reader drain output after the child exits.
const EXIT_DRAIN: Duration = Duration::from_millis(500);

pub const MAX_COLS: u16 = 1000;
pub const MAX_ROWS: u16 = 500;

pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// What frames subscribers watch. `watch` gives coalescing for free: a slow
/// subscriber only ever sees the latest value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameSignal {
    pub seq: u64,
    /// `Some(code)` once the process exited.
    pub exited: Option<Option<i32>>,
    /// The pane was killed and forgotten; subscribers should close.
    pub gone: bool,
}

pub enum WriterMsg {
    Bytes(Vec<u8>),
    /// `Submit`: the paste, then (when `enter`) a pause and a separate CR.
    Paste { bytes: Vec<u8>, enter: bool },
    /// Acknowledged once everything queued before it has been written.
    Barrier(std_mpsc::Sender<()>),
}

impl WriterMsg {
    fn queued_len(&self) -> usize {
        match self {
            WriterMsg::Bytes(b) | WriterMsg::Paste { bytes: b, .. } => b.len(),
            WriterMsg::Barrier(_) => 0,
        }
    }
}

/// The writer queue is at [`MAX_QUEUED_INPUT`].
#[derive(Debug)]
pub struct QueueFull;

pub struct RawSub {
    pub tx: mpsc::Sender<Vec<u8>>,
    pub lagged: bool,
}

pub struct PaneState {
    pub engine: Box<dyn TerminalEngine>,
    pub seq: u64,
    /// Bytes were fed since the last `settle`.
    pub dirty: bool,
    fingerprint: u64,
    pub last_output_ms: u64,
    pub alive: bool,
    pub exit_code: Option<i32>,
    pub raw_subs: Vec<RawSub>,
    filter: QueryFilter,
}

impl PaneState {
    /// Bring `seq` up to date: bump it iff something visible changed since
    /// the last call. Cheap when nothing was fed.
    pub fn settle(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        let fp = self.engine.fingerprint();
        if fp != self.fingerprint {
            self.fingerprint = fp;
            self.seq += 1;
        }
    }

    pub fn snapshot(&mut self, scrollback: usize) -> ScreenSnapshot {
        self.settle();
        let mut s = self.engine.snapshot(scrollback);
        s.seq = self.seq;
        s
    }
}

/// Immutable description of a pane, also the unit of a live-upgrade manifest.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PaneMeta {
    pub id: PaneId,
    pub pid: u32,
    pub created_ms: u64,
    pub cwd: String,
}

pub struct Pane {
    pub meta: PaneMeta,
    pub master: OwnedFd,
    /// Not our child (inherited in a live upgrade): no `waitpid`, exit code
    /// unknown.
    pub adopted: bool,
    pub state: Mutex<PaneState>,
    writer: std_mpsc::Sender<WriterMsg>,
    queued: Arc<AtomicUsize>,
    pub wake: Notify,
    pub frames: watch::Sender<FrameSignal>,
    pub killed: AtomicBool,
    reader: (Mutex<ReaderState>, Condvar),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderState {
    Running,
    /// Stopped by a live-upgrade freeze; the PTY is still open.
    Frozen,
    /// EOF/EIO: every slave fd closed.
    Eof,
}

/// Where a reader thread should stop: a pipe that becomes readable when the
/// host freezes all readers ahead of a live upgrade.
#[derive(Clone)]
pub struct Freeze(pub Arc<OwnedFd>);

impl Pane {
    pub fn info(&self) -> PaneInfo {
        let mut st = lock(&self.state);
        st.settle();
        let (cols, rows) = st.engine.size();
        PaneInfo {
            pane: self.meta.id.clone(),
            pid: self.meta.pid,
            cols,
            rows,
            alive: st.alive,
            exit_code: st.exit_code,
            created_ms: self.meta.created_ms,
            last_output_ms: st.last_output_ms,
            title: st.engine.title(),
            cwd: self.meta.cwd.clone(),
            seq: st.seq,
            history_size: st.engine.history_size(),
        }
    }

    pub fn is_alive(&self) -> bool {
        lock(&self.state).alive
    }

    pub fn send(&self, msg: WriterMsg) -> Result<(), QueueFull> {
        let len = msg.queued_len();
        self.queued
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |q| {
                (q == 0 || q.saturating_add(len) <= MAX_QUEUED_INPUT).then_some(q + len)
            })
            .map_err(|_| QueueFull)?;
        if self.writer.send(msg).is_err() {
            // The writer thread is gone (pane torn down); nothing will drain.
            self.queued.fetch_sub(len, Ordering::SeqCst);
        }
        Ok(())
    }

    /// Wait (bounded) until everything queued so far reached the PTY.
    pub fn flush_writes(&self, timeout: Duration) {
        let (tx, rx) = std_mpsc::channel();
        let _ = self.send(WriterMsg::Barrier(tx));
        let _ = rx.recv_timeout(timeout);
    }

    /// `Submit`: paste (bracketed when the application asked for it), then
    /// Enter as a separate write after a short delay.
    pub fn submit(&self, text: &str, enter: bool) -> Result<(), QueueFull> {
        let bracketed = lock(&self.state).engine.modes().bracketed_paste;
        let bytes = match (text.is_empty(), bracketed) {
            (true, _) => Vec::new(),
            (false, true) => bracketed_paste(text),
            (false, false) => text.as_bytes().to_vec(),
        };
        // One message, so a full queue can never accept the paste but drop
        // its Enter.
        self.send(WriterMsg::Paste { bytes, enter })
    }

    pub fn resize(&self, cols: u16, rows: u16) -> std::io::Result<()> {
        // Under the state lock, so concurrent resizes leave the PTY and the
        // emulator at the same size.
        let mut st = lock(&self.state);
        set_winsize(self.master.as_raw_fd(), cols, rows)?;
        st.engine.resize(cols, rows);
        st.dirty = true;
        drop(st);
        self.wake.notify_one();
        Ok(())
    }

    /// Feed one chunk of PTY output. Runs on the reader thread.
    fn ingest(&self, bytes: &[u8]) {
        let mut st = lock(&self.state);
        let st = &mut *st;
        st.engine.set_answer_color_queries(st.raw_subs.is_empty());
        if catch_unwind(AssertUnwindSafe(|| st.engine.feed(bytes))).is_err() {
            // An emulator bug must cost this pane its screen state, not the
            // host. Start over with a blank screen of the same size.
            tracing::error!(pane = %self.meta.id, "terminal engine panicked; resetting pane emulator");
            let (cols, rows) = st.engine.size();
            st.engine = Box::new(AlacrittyEngine::new(cols, rows, DEFAULT_SCROLLBACK));
        }
        let replies = st.engine.take_replies();
        if !replies.is_empty() && self.send(WriterMsg::Bytes(replies)).is_err() {
            tracing::debug!(pane = %self.meta.id, "input queue full; dropped terminal query replies");
        }
        st.dirty = true;
        st.last_output_ms = now_ms();
        if !st.raw_subs.is_empty() {
            let mut out = Vec::with_capacity(bytes.len());
            st.filter.filter(bytes, &mut out);
            let mut repaint: Option<Vec<u8>> = None;
            let engine = &st.engine;
            st.raw_subs.retain_mut(|sub| {
                if sub.lagged {
                    // The repaint reflects this chunk too (it was fed above).
                    let r = repaint.get_or_insert_with(|| engine.snapshot(0).to_ansi());
                    return match sub.tx.try_send(r.clone()) {
                        Ok(()) => {
                            sub.lagged = false;
                            true
                        }
                        Err(mpsc::error::TrySendError::Full(_)) => true,
                        Err(mpsc::error::TrySendError::Closed(_)) => false,
                    };
                }
                if out.is_empty() {
                    return !sub.tx.is_closed();
                }
                match sub.tx.try_send(out.clone()) {
                    Ok(()) => true,
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        sub.lagged = true;
                        true
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => false,
                }
            });
        }
    }

    /// Register a raw subscriber; its first message is a full repaint.
    pub fn add_raw_subscriber(&self) -> mpsc::Receiver<Vec<u8>> {
        let (tx, rx) = mpsc::channel(RAW_QUEUE);
        let mut st = lock(&self.state);
        if st.engine.sync_deadline().is_some() {
            // Apply a half-received synchronized update now, so the repaint
            // and the raw bytes that follow line up.
            st.engine.flush_sync();
            st.dirty = true;
        }
        if st.raw_subs.is_empty() {
            st.filter = QueryFilter::default();
        }
        let repaint = st.snapshot(0).to_ansi();
        let _ = tx.try_send(repaint);
        st.raw_subs.push(RawSub { tx, lagged: false });
        rx
    }

    fn mark_exited(&self, code: Option<i32>) {
        let mut st = lock(&self.state);
        if !st.alive {
            return;
        }
        st.alive = false;
        st.exit_code = code;
        drop(st);
        self.frames.send_modify(|f| f.exited = Some(code));
        self.wake.notify_one();
    }

    fn set_reader_state(&self, state: ReaderState) {
        *lock(&self.reader.0) = state;
        self.reader.1.notify_all();
        self.wake.notify_one();
    }

    /// Wait until the reader leaves `Running`; returns its state.
    pub fn wait_reader_stopped(&self, timeout: Duration) -> ReaderState {
        let guard = lock(&self.reader.0);
        let (guard, _) = self
            .reader
            .1
            .wait_timeout_while(guard, timeout, |s| *s == ReaderState::Running)
            .unwrap_or_else(|e| e.into_inner());
        *guard
    }

    pub fn reader_is_done(&self) -> bool {
        *lock(&self.reader.0) == ReaderState::Eof
    }

    /// Restart a reader stopped by a freeze (an aborted live upgrade).
    pub fn restart_reader(self: &Arc<Self>, freeze: Freeze) -> std::io::Result<()> {
        if *lock(&self.reader.0) != ReaderState::Frozen {
            return Ok(());
        }
        let fd = dup_cloexec(self.master.as_raw_fd())?;
        *lock(&self.reader.0) = ReaderState::Running;
        let p = Arc::clone(self);
        std::thread::Builder::new()
            .name(format!("ptyd-read-{}", self.meta.id))
            .spawn(move || reader_loop(p, File::from(fd), freeze))?;
        Ok(())
    }

    /// Bytes that rebuild this pane's screen, scrollback and input modes in
    /// a fresh engine of the same size (live upgrade), at most `max_bytes`
    /// long: the oldest scrollback lines are dropped to fit.
    pub fn replay_bytes(&self, max_bytes: usize) -> Vec<u8> {
        let mut st = lock(&self.state);
        if st.engine.sync_deadline().is_some() {
            st.engine.flush_sync();
            st.dirty = true;
        }
        // The successor's emulator keeps no more than this anyway.
        let snap = st.snapshot(DEFAULT_SCROLLBACK);
        drop(st);
        replay_from_snapshot(&snap, max_bytes)
    }

    pub fn exit_code(&self) -> Option<i32> {
        lock(&self.state).exit_code
    }

    pub fn last_output_ms(&self) -> u64 {
        lock(&self.state).last_output_ms
    }

    /// SIGHUP the process group now and SIGKILL it after [`KILL_GRACE`]
    /// unless it is fully gone by then. Idempotent.
    pub fn kill(self: &Arc<Self>) {
        if self.killed.swap(true, Ordering::SeqCst) {
            return;
        }
        // A reaped leader whose PTY is fully closed has no group left to
        // signal, and its pid may already belong to someone else.
        if self.is_alive() || !self.reader_is_done() {
            signal_group(self.meta.pid, libc::SIGHUP);
        }
        lock(&self.state).raw_subs.clear();
        self.frames.send_modify(|f| f.gone = true);
        self.wake.notify_one();
        let pane = Arc::clone(self);
        std::thread::Builder::new()
            .name(format!("ptyd-kill-{}", pane.meta.pid))
            .spawn(move || {
                std::thread::sleep(KILL_GRACE);
                // Skip only when the leader is reaped *and* nobody holds the
                // slave any more; otherwise stragglers in the group remain.
                if pane.is_alive() || !pane.reader_is_done() {
                    signal_group(pane.meta.pid, libc::SIGKILL);
                }
            })
            .ok();
    }

    /// Final screen, for the checkpoint written at exit/shutdown.
    pub fn checkpoint(&self) -> Checkpoint {
        let screen = lock(&self.state).snapshot(CHECKPOINT_SCROLLBACK);
        Checkpoint { pane: self.meta.id.clone(), saved_ms: now_ms(), screen }
    }
}

/// Wrap `text` in bracketed-paste markers. Every ESC and C1 control is
/// dropped from the text: removing only literal `ESC[201~` markers can be
/// bypassed by nesting one inside another (`ESC[20ESC[201~1~` reassembles
/// into a marker), after which the rest would be read as keystrokes.
pub fn bracketed_paste(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + 12);
    out.extend_from_slice(b"\x1b[200~");
    let mut buf = [0u8; 4];
    for c in text.chars().filter(|&c| c != '\x1b' && !('\u{80}'..='\u{9f}').contains(&c)) {
        out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    }
    out.extend_from_slice(b"\x1b[201~");
    out
}

/// See [`Pane::replay_bytes`].
fn replay_from_snapshot(snap: &ScreenSnapshot, max_bytes: usize) -> Vec<u8> {
    let mut out = Vec::new();
    if snap.modes.alt_screen {
        out.extend_from_slice(b"\x1b[?1049h");
    }
    let mut body = Vec::new();
    let mut line_starts = Vec::with_capacity(snap.lines.len());
    for (i, line) in snap.lines.iter().enumerate() {
        if i > 0 {
            body.extend_from_slice(b"\r\n");
        }
        line_starts.push(body.len());
        crate::render::write_line_sgr(&mut body, line);
    }
    let suffix = replay_suffix(snap);
    let budget = max_bytes.saturating_sub(out.len() + suffix.len());
    // Visible rows are always kept; scrollback goes oldest-first.
    let visible_start = line_starts.len().saturating_sub(snap.rows as usize);
    let keep_from = line_starts[..visible_start]
        .iter()
        .copied()
        .find(|&start| body.len() - start <= budget)
        .unwrap_or_else(|| line_starts.get(visible_start).copied().unwrap_or(0));
    out.extend_from_slice(&body[keep_from..]);
    out.extend_from_slice(&suffix);
    out
}

/// Input modes, title and cursor, applied after the replayed lines.
fn replay_suffix(snap: &ScreenSnapshot) -> Vec<u8> {
    let mut out = Vec::new();
    let m = snap.modes;
    if m.app_cursor {
        out.extend_from_slice(b"\x1b[?1h");
    }
    if m.bracketed_paste {
        out.extend_from_slice(b"\x1b[?2004h");
    }
    if m.mouse_reporting {
        out.extend_from_slice(b"\x1b[?1002h");
    }
    if m.sgr_mouse {
        out.extend_from_slice(b"\x1b[?1006h");
    }
    if m.kitty_keyboard != 0 {
        out.extend_from_slice(format!("\x1b[>{}u", m.kitty_keyboard).as_bytes());
    }
    if let Some(t) = &snap.title {
        let clean: String = t.chars().filter(|c| !c.is_control()).collect();
        out.extend_from_slice(format!("\x1b]2;{clean}\x07").as_bytes());
    }
    out.extend_from_slice(format!("\x1b[{};{}H", snap.cursor.row + 1, snap.cursor.col + 1).as_bytes());
    if !snap.cursor.visible {
        out.extend_from_slice(b"\x1b[?25l");
    }
    out
}

pub fn signal_group(pid: u32, sig: libc::c_int) {
    if pid > 1 {
        // The child is a session (and process group) leader: portable-pty
        // calls setsid() before exec, so pgid == pid.
        unsafe {
            libc::killpg(pid as libc::pid_t, sig);
        }
    }
}

pub fn set_winsize(fd: RawFd, cols: u16, rows: u16) -> std::io::Result<()> {
    let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ as _, &ws as *const _) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

pub fn dup_cloexec(fd: RawFd) -> std::io::Result<OwnedFd> {
    let new = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if new < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(new) })
}

pub fn clamp_size(cols: u16, rows: u16) -> (u16, u16) {
    (cols.clamp(2, MAX_COLS), rows.clamp(1, MAX_ROWS))
}

/// Inputs to [`start_pane`] shared by fresh spawns and adopted panes.
pub struct PaneSetup {
    pub meta: PaneMeta,
    pub master: OwnedFd,
    pub engine: Box<dyn TerminalEngine>,
    pub adopted: bool,
    /// Initial state carried across a live upgrade.
    pub seq: u64,
    pub last_output_ms: u64,
    pub alive: bool,
    pub exit_code: Option<i32>,
}

/// Wire up threads and the pane task for a pane whose process already runs.
pub fn start_pane(
    setup: PaneSetup,
    freeze: Freeze,
    rt: &tokio::runtime::Handle,
    checkpoint_dir: Option<PathBuf>,
) -> std::io::Result<Arc<Pane>> {
    prepare_pane(setup)?.start(freeze, rt, checkpoint_dir)
}

/// A pane whose state exists but whose threads do not run yet: nothing
/// reads or writes its PTY. A live-upgrade successor holds its panes like
/// this until the old host has released them.
pub struct PendingPane {
    pane: Arc<Pane>,
    reader_fd: OwnedFd,
    writer_fd: OwnedFd,
    wrx: std_mpsc::Receiver<WriterMsg>,
    queued: Arc<AtomicUsize>,
    alive: bool,
}

pub fn prepare_pane(setup: PaneSetup) -> std::io::Result<PendingPane> {
    let reader_fd = dup_cloexec(setup.master.as_raw_fd())?;
    let writer_fd = dup_cloexec(setup.master.as_raw_fd())?;
    let (wtx, wrx) = std_mpsc::channel();
    let queued = Arc::new(AtomicUsize::new(0));
    let (frames, _) = watch::channel(FrameSignal {
        seq: setup.seq,
        exited: if setup.alive { None } else { Some(setup.exit_code) },
        gone: false,
    });
    let pane = Arc::new(Pane {
        meta: setup.meta,
        master: setup.master,
        adopted: setup.adopted,
        state: Mutex::new(PaneState {
            engine: setup.engine,
            seq: setup.seq,
            dirty: true,
            fingerprint: 0,
            last_output_ms: setup.last_output_ms,
            alive: setup.alive,
            exit_code: setup.exit_code,
            raw_subs: Vec::new(),
            filter: QueryFilter::default(),
        }),
        writer: wtx,
        queued: Arc::clone(&queued),
        wake: Notify::new(),
        frames,
        killed: AtomicBool::new(false),
        reader: (Mutex::new(ReaderState::Running), Condvar::new()),
    });
    // Prime the fingerprint without bumping seq: carried-over state is
    // already at `setup.seq`.
    {
        let mut st = lock(&pane.state);
        st.fingerprint = st.engine.fingerprint();
        st.dirty = false;
    }
    Ok(PendingPane { pane, reader_fd, writer_fd, wrx, queued, alive: setup.alive })
}

impl PendingPane {
    pub fn id(&self) -> &str {
        &self.pane.meta.id
    }

    pub fn start(self, freeze: Freeze, rt: &tokio::runtime::Handle, checkpoint_dir: Option<PathBuf>) -> std::io::Result<Arc<Pane>> {
        let Self { pane, reader_fd, writer_fd, wrx, queued, alive } = self;
        let id = pane.meta.id.clone();
        let p = Arc::clone(&pane);
        std::thread::Builder::new()
            .name(format!("ptyd-read-{id}"))
            .spawn(move || reader_loop(p, File::from(reader_fd), freeze))?;
        std::thread::Builder::new()
            .name(format!("ptyd-write-{id}"))
            .spawn(move || writer_loop(wrx, File::from(writer_fd), queued))?;
        if alive {
            let p = Arc::clone(&pane);
            std::thread::Builder::new()
                .name(format!("ptyd-wait-{id}"))
                .spawn(move || waiter_loop(p))?;
        }
        rt.spawn(pane_task(Arc::clone(&pane), checkpoint_dir));
        Ok(pane)
    }
}

fn reader_loop(pane: Arc<Pane>, mut file: File, freeze: Freeze) {
    let mut buf = vec![0u8; 64 * 1024];
    let fd = file.as_raw_fd();
    loop {
        let mut fds = [
            libc::pollfd { fd, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: freeze.0.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        ];
        let r = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if r < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if fds[1].revents != 0 {
            // Live upgrade: stop consuming so unread output stays in the
            // kernel buffer for the successor host. Not EOF: the process is
            // still running.
            pane.set_reader_state(ReaderState::Frozen);
            return;
        }
        if fds[0].revents & libc::POLLNVAL != 0 {
            break;
        }
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                pane.ingest(&buf[..n]);
                pane.wake.notify_one();
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted || e.kind() == std::io::ErrorKind::WouldBlock => {}
            // EIO: every slave fd is closed (the process tree exited).
            Err(_) => break,
        }
    }
    pane.set_reader_state(ReaderState::Eof);
}

fn writer_loop(rx: std_mpsc::Receiver<WriterMsg>, mut file: File, queued: Arc<AtomicUsize>) {
    let mut write = |b: &[u8]| {
        if let Err(e) = file.write_all(b) {
            tracing::debug!("pty write failed: {e}");
        }
    };
    for msg in rx {
        let len = msg.queued_len();
        match msg {
            WriterMsg::Bytes(b) => write(&b),
            WriterMsg::Paste { bytes, enter } => {
                if !bytes.is_empty() {
                    write(&bytes);
                    if enter {
                        std::thread::sleep(SUBMIT_ENTER_DELAY);
                    }
                }
                if enter {
                    write(b"\r");
                }
            }
            WriterMsg::Barrier(ack) => {
                let _ = ack.send(());
            }
        }
        queued.fetch_sub(len, Ordering::SeqCst);
    }
}

fn waiter_loop(pane: Arc<Pane>) {
    let pid = pane.meta.pid as libc::pid_t;
    let code = if pane.adopted {
        // Not our child: poll for the process (and the PTY) to go away.
        loop {
            std::thread::sleep(Duration::from_millis(500));
            let gone = unsafe { libc::kill(pid, 0) } != 0
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
            if gone || pane.reader_is_done() {
                break None;
            }
        }
    } else {
        loop {
            let mut status: libc::c_int = 0;
            let r = unsafe { libc::waitpid(pid, &mut status, 0) };
            if r == pid {
                if libc::WIFEXITED(status) {
                    break Some(libc::WEXITSTATUS(status));
                }
                if libc::WIFSIGNALED(status) {
                    break Some(128 + libc::WTERMSIG(status));
                }
                continue;
            }
            if r < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            // ECHILD: someone else reaped it; the code is unknowable.
            break None;
        }
    };
    let deadline = Instant::now() + EXIT_DRAIN;
    while Instant::now() < deadline {
        // A frozen reader (live upgrade in progress) is not draining.
        if pane.wait_reader_stopped(deadline - Instant::now()) != ReaderState::Running {
            break;
        }
    }
    pane.mark_exited(code);
}

async fn pane_task(pane: Arc<Pane>, checkpoint_dir: Option<PathBuf>) {
    let mut last_emit = Instant::now() - FRAME_INTERVAL;
    let mut last_ckpt: Option<Instant> = None;
    let mut ckpt_seq: Option<u64> = None;
    let mut final_ckpt_done = false;
    let mut timer: Option<Instant> = None;
    loop {
        match timer {
            Some(t) => {
                tokio::select! {
                    _ = pane.wake.notified() => {}
                    _ = tokio::time::sleep_until(t.into()) => {}
                }
            }
            None => pane.wake.notified().await,
        }
        timer = None;
        if pane.killed.load(Ordering::SeqCst) {
            return;
        }
        let presenting = pane.frames.receiver_count() > 0;
        if presenting {
            // Coalesce bursts: at most one notification per FRAME_INTERVAL.
            let since = last_emit.elapsed();
            if since < FRAME_INTERVAL {
                tokio::time::sleep(FRAME_INTERVAL - since).await;
            }
        }

        let now = Instant::now();
        let ckpt_due = checkpoint_dir.is_some() && last_ckpt.is_none_or(|t| now.duration_since(t) >= CHECKPOINT_INTERVAL);
        let mut write_ckpt: Option<Checkpoint> = None;
        let mut final_write = false;
        {
            let mut st = lock(&pane.state);
            if let Some(deadline) = st.engine.sync_deadline() {
                if now >= deadline {
                    st.engine.flush_sync();
                    st.dirty = true;
                } else {
                    timer = Some(deadline);
                }
            }
            if presenting || ckpt_due || !st.alive {
                st.settle();
            }
            let seq = st.seq;
            if presenting && pane.frames.borrow().seq != seq {
                pane.frames.send_modify(|f| f.seq = seq);
                last_emit = Instant::now();
            }
            if checkpoint_dir.is_some() {
                let changed = ckpt_seq != Some(seq) || st.dirty;
                if !st.alive && !final_ckpt_done {
                    final_write = true;
                } else if changed && ckpt_due {
                    // Settled above, so `seq` is current.
                    if ckpt_seq != Some(seq) {
                        let screen = {
                            let mut s = st.engine.snapshot(CHECKPOINT_SCROLLBACK);
                            s.seq = seq;
                            s
                        };
                        write_ckpt = Some(Checkpoint { pane: pane.meta.id.clone(), saved_ms: now_ms(), screen });
                    }
                } else if changed {
                    let due_at = last_ckpt.map(|t| t + CHECKPOINT_INTERVAL).unwrap_or(now);
                    timer = Some(timer.map_or(due_at, |t| t.min(due_at)));
                }
            }
        }
        if final_write {
            final_ckpt_done = true;
            write_ckpt = Some(pane.checkpoint());
        }
        if let (Some(dir), Some(ck)) = (checkpoint_dir.as_ref(), write_ckpt) {
            ckpt_seq = Some(ck.screen.seq);
            last_ckpt = Some(Instant::now());
            let dir = dir.clone();
            let _ = tokio::task::spawn_blocking(move || {
                if let Err(e) = checkpoint::write(&dir, &ck) {
                    tracing::warn!(pane = %ck.pane, "checkpoint write failed: {e}");
                }
            })
            .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pane whose "PTY" is one end of a socketpair; the other end is
    /// returned so the test decides when input drains.
    pub(crate) fn socket_pane() -> (Arc<Pane>, std::os::unix::net::UnixStream) {
        let (master, peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut fds = [0 as libc::c_int; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let freeze = Freeze(Arc::new(unsafe { OwnedFd::from_raw_fd(fds[0]) }));
        std::mem::forget(unsafe { OwnedFd::from_raw_fd(fds[1]) });
        let setup = PaneSetup {
            meta: PaneMeta { id: "t".into(), pid: 0, created_ms: 0, cwd: "/".into() },
            master: OwnedFd::from(master),
            engine: Box::new(AlacrittyEngine::new(80, 24, 100)),
            adopted: true,
            seq: 0,
            last_output_ms: 0,
            alive: false,
            exit_code: None,
        };
        (start_pane(setup, freeze, &tokio::runtime::Handle::current(), None).unwrap(), peer)
    }

    #[tokio::test]
    async fn input_queue_is_bounded_and_drains() {
        let (p, mut peer) = socket_pane();
        // An empty queue takes even an oversized message; the writer then
        // blocks on it because nobody reads the other end.
        assert!(p.send(WriterMsg::Bytes(vec![b'x'; MAX_QUEUED_INPUT + 1])).is_ok());
        assert!(p.send(WriterMsg::Bytes(b"y".to_vec())).is_err());
        assert!(p.submit("z", true).is_err());

        std::thread::spawn(move || std::io::copy(&mut peer, &mut std::io::sink()));
        let deadline = Instant::now() + Duration::from_secs(10);
        while p.send(WriterMsg::Bytes(b"y".to_vec())).is_err() {
            assert!(Instant::now() < deadline, "queue never drained");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn replay_drops_oldest_scrollback_to_fit_and_keeps_the_screen() {
        let mut engine = AlacrittyEngine::new(80, 24, DEFAULT_SCROLLBACK);
        for i in 0..5000 {
            engine.feed(format!("\x1b[31mline-{i:05}\x1b[0m {}\r\n", "x".repeat(60)).as_bytes());
        }
        engine.feed(b"\x1b[?2004hprompt");
        let snap = engine.snapshot(DEFAULT_SCROLLBACK);
        let full = replay_from_snapshot(&snap, usize::MAX);
        let max = full.len() / 4;
        let capped = replay_from_snapshot(&snap, max);
        assert!(capped.len() <= max, "{} > {max}", capped.len());

        let mut fresh = AlacrittyEngine::new(80, 24, DEFAULT_SCROLLBACK);
        fresh.feed(&capped);
        let rebuilt = fresh.snapshot(0);
        assert_eq!(rebuilt.to_plain_text(), engine.snapshot(0).to_plain_text(), "visible screen intact");
        assert!(rebuilt.modes.bracketed_paste);
        let history = fresh.history_size();
        assert!(history > 100 && history < 4900, "kept the newest part of the scrollback ({history} lines)");
        assert!(String::from_utf8_lossy(&capped).contains("line-04975"));
        assert!(!String::from_utf8_lossy(&capped).contains("line-00000"));
    }

    #[test]
    fn bracketed_paste_cannot_be_escaped() {
        for exploit in ["hi\x1b[20\x1b[201~1~INJECT", "a\x1b[201~b", "x\u{9b}201~y", "\x1b\x1b[201~[201~"] {
            let out = bracketed_paste(exploit);
            let inner = &out[6..out.len() - 6];
            assert!(!inner.contains(&0x1b), "{exploit:?} left an ESC in {inner:?}");
            assert!(!String::from_utf8_lossy(inner).contains('\u{9b}'), "{exploit:?} left a C1 CSI");
            assert!(out.starts_with(b"\x1b[200~") && out.ends_with(b"\x1b[201~"));
        }
        assert_eq!(bracketed_paste("héllo\n✓"), "\x1b[200~héllo\n✓\x1b[201~".as_bytes());
    }
}
