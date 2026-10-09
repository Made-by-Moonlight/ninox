//! The host process: socket listener, per-connection tasks, request dispatch.
//!
//! Robustness rules this module follows:
//!
//! - Every connection is its own tokio task, and every request that touches
//!   pane state runs in `spawn_blocking`; a panic in either is contained to
//!   that request (the client gets `Internal`) and never reaches the
//!   listener.
//! - Client-controlled data is validated, never unwrapped: malformed frames
//!   close only that connection, malformed headers get `BadRequest`.
//! - A process-wide `flock` on `<socket>.lock` (held for the host's life,
//!   inherited by the successor in a live upgrade) is what decides whether a
//!   host is already running; the socket file itself is only trusted after
//!   the lock is ours.
//! - SIGHUP is ignored so losing the spawning terminal never takes the host
//!   down. SIGTERM behaves like `Shutdown`; so does SIGINT, except in a
//!   takeover host (see [`Signals`]).

use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;

use crate::codec::{read_frame, write_frame, RawFrame};
use crate::engine::{AlacrittyEngine, DEFAULT_SCROLLBACK};
use crate::pane::{self, lock, now_ms, Freeze, Pane, PaneMeta, PaneSetup};
use crate::protocol::*;

pub struct Host {
    pub panes: Mutex<HashMap<PaneId, Arc<Pane>>>,
    /// Serialises spawns so check-then-insert on a pane id is atomic without
    /// holding the map lock across fork/exec.
    spawn_lock: Mutex<()>,
    /// Set (under `spawn_lock`) while a live upgrade snapshots the panes and
    /// for good once it commits: a pane spawned now would be missing from
    /// the successor's manifest.
    handing_off: AtomicBool,
    /// A successor owns the panes (live upgrade committed): whatever stops
    /// this host from now on, it must not kill them.
    released: AtomicBool,
    pub checkpoint_dir: Option<PathBuf>,
    pub epoch_ms: u64,
    pub rt: tokio::runtime::Handle,
    pub freeze: Freeze,
    freeze_tx: Mutex<Option<OwnedFd>>,
    /// The `flock`ed `<socket>.lock` fd, passed to a successor on handoff.
    lock_fd: Mutex<Option<OwnedFd>>,
    shutdown: watch::Sender<Stop>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    Running,
    /// Kill every pane and exit.
    Shutdown,
    /// A successor took over: exit without touching the panes.
    HandedOff,
}

impl Host {
    pub fn new(checkpoint_dir: Option<PathBuf>, epoch_ms: u64) -> Result<Arc<Self>> {
        let (r, w) = pipe_cloexec()?;
        let (shutdown, _) = watch::channel(Stop::Running);
        Ok(Arc::new(Self {
            panes: Mutex::new(HashMap::new()),
            spawn_lock: Mutex::new(()),
            handing_off: AtomicBool::new(false),
            released: AtomicBool::new(false),
            checkpoint_dir,
            epoch_ms,
            rt: tokio::runtime::Handle::current(),
            freeze: Freeze(Arc::new(r)),
            freeze_tx: Mutex::new(Some(w)),
            lock_fd: Mutex::new(None),
            shutdown,
        }))
    }

    pub fn stop(&self, how: Stop) {
        self.shutdown.send_if_modified(|s| {
            if *s == Stop::Running {
                *s = how;
                true
            } else {
                false
            }
        });
    }

    pub fn subscribe_stop(&self) -> watch::Receiver<Stop> {
        self.shutdown.subscribe()
    }

    /// Make every reader thread stop consuming PTY output (live upgrade).
    pub fn freeze_readers(&self) {
        if let Some(w) = lock(&self.freeze_tx).as_ref() {
            unsafe {
                libc::write(w.as_raw_fd(), b"x".as_ptr().cast(), 1);
            }
        }
    }

    /// Undo [`Self::freeze_readers`] (readers must then be restarted).
    pub fn unfreeze_readers(&self) {
        let mut buf = [0u8; 16];
        // The read end is non-blocking; drain whatever freezes were written.
        while unsafe { libc::read(self.freeze.0.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
    }

    /// Refuse spawns from now on; returns once any spawn in flight has
    /// inserted its pane.
    pub fn begin_handoff(&self) {
        let _guard = lock(&self.spawn_lock);
        self.handing_off.store(true, Ordering::SeqCst);
    }

    /// The handoff was aborted before committing: spawns work again.
    pub fn abort_handoff(&self) {
        self.released.store(false, Ordering::SeqCst);
        self.handing_off.store(false, Ordering::SeqCst);
    }

    /// Set just before the release is sent (cleared again by
    /// [`Self::abort_handoff`] if sending fails), so a `SIGTERM` racing
    /// the commit never kills panes the successor may own.
    pub fn set_released(&self) {
        self.released.store(true, Ordering::SeqCst);
    }

    pub fn set_lock_fd(&self, fd: OwnedFd) {
        *lock(&self.lock_fd) = Some(fd);
    }

    pub fn lock_fd(&self) -> Option<std::os::fd::RawFd> {
        lock(&self.lock_fd).as_ref().map(|f| f.as_raw_fd())
    }

    pub fn pane(&self, id: &str) -> Option<Arc<Pane>> {
        lock(&self.panes).get(id).cloned()
    }

    fn spawn_pane(&self, spec: SpawnSpec) -> Result<u32, ErrorBody> {
        validate_pane_id(&spec.pane)?;
        if spec.argv.is_empty() || spec.argv[0].is_empty() {
            return Err(err(ErrorCode::BadRequest, "argv must not be empty"));
        }
        if !Path::new(&spec.cwd).is_dir() {
            return Err(err(ErrorCode::SpawnFailed, format!("cwd {:?} is not a directory", spec.cwd)));
        }
        let _guard = lock(&self.spawn_lock);
        if self.handing_off.load(Ordering::SeqCst) {
            return Err(err(ErrorCode::Unavailable, "host is handing off to a successor; retry on a new connection"));
        }
        if let Some(existing) = self.pane(&spec.pane) {
            if existing.is_alive() {
                return Err(err(ErrorCode::AlreadyExists, format!("pane {} is running", spec.pane)));
            }
            // A dead pane is replaced by a respawn under the same id.
            let old = lock(&self.panes).remove(&spec.pane);
            if let Some(old) = old {
                old.kill();
            }
        }
        let (cols, rows) = pane::clamp_size(spec.cols, spec.rows);
        let spawn_err = |e: &dyn std::fmt::Display| err(ErrorCode::SpawnFailed, e.to_string());

        let pair = native_pty_system()
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| spawn_err(&e))?;
        let mut cmd = CommandBuilder::new(&spec.argv[0]);
        cmd.args(&spec.argv[1..]);
        cmd.cwd(&spec.cwd);
        for k in &spec.env_remove {
            cmd.env_remove(k);
        }
        // Defaults a terminal would provide; the spec's env can override.
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        let child = pair.slave.spawn_command(cmd).map_err(|e| spawn_err(&e))?;
        let pid = child.process_id().unwrap_or(0);
        // The waiter thread reaps by pid; the portable-pty handle is not
        // needed (dropping it neither kills nor waits).
        drop(child);
        // Our copy of the slave must close, or the reader never sees EOF.
        drop(pair.slave);
        let master = pair
            .master
            .as_raw_fd()
            .ok_or_else(|| err(ErrorCode::SpawnFailed, "pty has no master fd"))
            .and_then(|raw| pane::dup_cloexec(raw).map_err(|e| spawn_err(&e)));
        // portable-pty's master only closes its fd on drop (we never took
        // its writer, whose drop would send EOF to the child).
        drop(pair.master);
        let master = master.inspect_err(|_| abandon_child(pid))?;

        let setup = PaneSetup {
            meta: PaneMeta { id: spec.pane.clone(), pid, created_ms: now_ms(), cwd: spec.cwd.clone() },
            master,
            engine: Box::new(AlacrittyEngine::new(cols, rows, DEFAULT_SCROLLBACK)),
            adopted: false,
            seq: 0,
            last_output_ms: 0,
            alive: true,
            exit_code: None,
        };
        let pane = pane::start_pane(setup, self.freeze.clone(), &self.rt, self.checkpoint_dir.clone())
            .map_err(|e| {
                abandon_child(pid);
                spawn_err(&e)
            })?;
        lock(&self.panes).insert(spec.pane.clone(), pane);
        tracing::info!(pane = %spec.pane, pid, "spawned");
        Ok(pid)
    }

    fn kill_pane(&self, id: &str) {
        let removed = lock(&self.panes).remove(id);
        if let Some(p) = removed {
            p.kill();
            tracing::info!(pane = %id, "killed");
        }
    }

    /// Final checkpoints, then SIGHUP everything, then SIGKILL stragglers.
    pub fn kill_all(&self) {
        let panes: Vec<Arc<Pane>> = lock(&self.panes).drain().map(|(_, p)| p).collect();
        if let Some(dir) = &self.checkpoint_dir {
            for p in &panes {
                if let Err(e) = crate::checkpoint::write(dir, &p.checkpoint()) {
                    tracing::warn!(pane = %p.meta.id, "final checkpoint failed: {e}");
                }
            }
        }
        for p in &panes {
            p.kill();
        }
        let deadline = std::time::Instant::now() + Duration::from_millis(1500);
        while std::time::Instant::now() < deadline && panes.iter().any(|p| p.is_alive()) {
            std::thread::sleep(Duration::from_millis(20));
        }
        for p in &panes {
            if p.is_alive() || !p.reader_is_done() {
                pane::signal_group(p.meta.pid, libc::SIGKILL);
            }
        }
    }

    /// Synchronous request handler (runs in `spawn_blocking`).
    fn handle(&self, req: Request, payload: Vec<u8>) -> (ReplyResult, Vec<u8>) {
        let ok = |r: Reply| (ReplyResult::Ok(r), Vec::new());
        let fail = |e: ErrorBody| (ReplyResult::Err(e), Vec::new());
        let not_found = |id: &str| fail(err(ErrorCode::NotFound, format!("no pane {id}")));
        match req {
            Request::Spawn(spec) => match self.spawn_pane(spec) {
                Ok(pid) => ok(Reply::Spawned { pid }),
                Err(e) => fail(e),
            },
            Request::Kill { pane } => {
                self.kill_pane(&pane);
                ok(Reply::Ok)
            }
            Request::Write { pane } => match self.pane(&pane) {
                Some(p) => {
                    if !payload.is_empty() && p.send(pane::WriterMsg::Bytes(payload)).is_err() {
                        return fail(queue_full(&pane));
                    }
                    ok(Reply::Ok)
                }
                None => not_found(&pane),
            },
            Request::Submit { pane, text, enter } => match self.pane(&pane) {
                Some(p) => match p.submit(&text, enter) {
                    Ok(()) => ok(Reply::Ok),
                    Err(_) => fail(queue_full(&pane)),
                },
                None => not_found(&pane),
            },
            Request::Resize { pane, cols, rows } => match self.pane(&pane) {
                Some(p) => {
                    let (cols, rows) = pane::clamp_size(cols, rows);
                    match p.resize(cols, rows) {
                        Ok(()) => ok(Reply::Ok),
                        // The PTY is gone (process exited); size is moot.
                        Err(_) if !p.is_alive() => ok(Reply::Ok),
                        Err(e) => fail(err(ErrorCode::Internal, format!("resize: {e}"))),
                    }
                }
                None => not_found(&pane),
            },
            Request::List => {
                let panes: Vec<Arc<Pane>> = lock(&self.panes).values().cloned().collect();
                let mut infos: Vec<PaneInfo> = panes.iter().map(|p| p.info()).collect();
                infos.sort_by_key(|i| (i.created_ms, i.pane.clone()));
                ok(Reply::Panes { panes: infos })
            }
            Request::Info { pane } => match self.pane(&pane) {
                Some(p) => ok(Reply::Info { pane: p.info() }),
                None => not_found(&pane),
            },
            Request::Screen { pane, scrollback } => match self.pane(&pane) {
                Some(p) => {
                    let screen = lock(&p.state).snapshot(scrollback);
                    ok(Reply::Screen { screen })
                }
                None => not_found(&pane),
            },
            Request::History { pane, start, end } => match self.pane(&pane) {
                Some(p) => {
                    let mut bytes = lock(&p.state).engine.history_ansi(start, end);
                    // Keep the newest lines if the capture cannot fit a frame.
                    let cap = MAX_FRAME_BYTES - 4096;
                    if bytes.len() > cap {
                        let cut = bytes.len() - cap;
                        let nl = bytes[cut..].iter().position(|&b| b == b'\n').map_or(cut, |i| cut + i + 1);
                        bytes.drain(..nl);
                    }
                    (ReplyResult::Ok(Reply::History), bytes)
                }
                None => not_found(&pane),
            },
            Request::Hello { .. } => fail(err(ErrorCode::BadRequest, "duplicate Hello")),
            // Handled on the async path before dispatch.
            Request::Subscribe { .. } | Request::Shutdown | Request::Handoff => {
                fail(err(ErrorCode::Internal, "unexpected request on blocking path"))
            }
        }
    }
}

pub fn err(code: ErrorCode, message: impl Into<String>) -> ErrorBody {
    ErrorBody { code, message: message.into() }
}

fn queue_full(pane: &str) -> ErrorBody {
    err(ErrorCode::Unavailable, format!("input queue of pane {pane} is full (it is not reading input)"))
}

/// SIGKILL and reap a child we spawned but could not turn into a pane, so
/// it neither keeps running unowned nor lingers as a zombie.
fn abandon_child(pid: u32) {
    if pid <= 1 {
        return;
    }
    pane::signal_group(pid, libc::SIGKILL);
    let mut status = 0;
    while unsafe { libc::waitpid(pid as libc::pid_t, &mut status, 0) } < 0
        && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
    {}
}

fn validate_pane_id(id: &str) -> Result<(), ErrorBody> {
    if id.is_empty() || id.len() > 256 || id.contains(['/', '\0']) || id == "." || id == ".." {
        return Err(err(ErrorCode::BadRequest, format!("invalid pane id {id:?}")));
    }
    Ok(())
}

fn pipe_cloexec() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        bail!("pipe: {}", std::io::Error::last_os_error());
    }
    for fd in fds {
        unsafe {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
    }
    unsafe {
        let fl = libc::fcntl(fds[0], libc::F_GETFL);
        libc::fcntl(fds[0], libc::F_SETFL, fl | libc::O_NONBLOCK);
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

// ---------------------------------------------------------------------------
// Startup: lock, stale socket, bind
// ---------------------------------------------------------------------------

pub fn lock_path(socket: &Path) -> PathBuf {
    let mut s = socket.as_os_str().to_owned();
    s.push(".lock");
    PathBuf::from(s)
}

/// Take the host lock, or fail if another host holds it.
pub fn acquire_lock(socket: &Path) -> Result<OwnedFd> {
    let path = lock_path(socket);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    let fd = OwnedFd::from(file);
    if unsafe { libc::flock(fd.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("another ninox-ptyd is already running (lock {} is held)", path.display());
    }
    Ok(fd)
}

pub fn prepare_socket_dir(socket: &Path) -> Result<()> {
    if let Some(dir) = socket.parent() {
        if !dir.as_os_str().is_empty() && !dir.exists() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
    }
    Ok(())
}

/// Bind `socket`, replacing a stale file. Call only while holding the lock.
pub async fn bind_fresh(socket: &Path) -> Result<UnixListener> {
    if UnixStream::connect(socket).await.is_ok() {
        bail!("another ninox-ptyd answers on {}", socket.display());
    }
    match std::fs::remove_file(socket) {
        Ok(()) => tracing::info!("removed stale socket {}", socket.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("remove stale {}", socket.display())),
    }
    bind_at(socket)
}

/// Binds with a 0177 umask so the socket is never connectable by others,
/// not even between `bind` and a `chmod`. The umask is process-wide, which
/// is fine here: hosts bind before they spawn any pane.
pub fn bind_at(socket: &Path) -> Result<UnixListener> {
    let old = unsafe { libc::umask(0o177) };
    let bound = UnixListener::bind(socket);
    unsafe { libc::umask(old) };
    let listener = bound.with_context(|| format!("bind {}", socket.display()))?;
    let _ = std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600));
    Ok(listener)
}

/// Only our own uid may talk to the host, whatever the socket's mode.
fn peer_is_same_user(stream: &UnixStream) -> bool {
    stream.peer_cred().is_ok_and(|c| c.uid() == unsafe { libc::geteuid() })
}

fn socket_inode(socket: &Path) -> Option<u64> {
    std::fs::symlink_metadata(socket).ok().map(|m| m.ino())
}

/// Ignore SIGHUP (losing the controlling terminal must not kill the host).
pub fn ignore_sighup() {
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
}

/// How the host treats terminal-generated signals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signals {
    /// SIGINT shuts down (and kills every pane), like SIGTERM.
    Default,
    /// `--takeover` typically runs in the user's terminal foreground, where
    /// a stray Ctrl-C, Ctrl-\ or Ctrl-Z would kill or stop a host that
    /// owns a whole fleet: SIGINT, SIGQUIT, SIGTSTP, SIGTTIN and SIGTTOU are
    /// caught and ignored. Caught, not `SIG_IGN`ed, so panes (which inherit
    /// ignored dispositions across exec) still get default handling.
    Takeover,
}

/// Accept loop and lifecycle, shared by fresh starts and takeovers. Returns
/// once the host stopped; the lock fd is released when dropped by the caller.
pub async fn serve(host: Arc<Host>, listener: UnixListener, socket: PathBuf, signals: Signals) -> Result<()> {
    use tokio::signal::unix::{signal, SignalKind};
    let inode = socket_inode(&socket);
    let mut stop = host.subscribe_stop();
    let mut sigterm = signal(SignalKind::terminate()).ok();
    let mut sigint = signal(SignalKind::interrupt()).ok();
    if signals == Signals::Takeover {
        for sig in [libc::SIGQUIT, libc::SIGTSTP, libc::SIGTTIN, libc::SIGTTOU] {
            if let Ok(mut s) = signal(SignalKind::from_raw(sig)) {
                tokio::spawn(async move {
                    while s.recv().await.is_some() {
                        tracing::info!(signal = sig, "ignoring job-control signal (takeover host)");
                    }
                });
            }
        }
    }
    tracing::info!(socket = %socket.display(), pid = std::process::id(), "ninox-ptyd listening");
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) if !peer_is_same_user(&stream) => {
                    tracing::warn!("rejected a connection from another user");
                }
                Ok((stream, _)) => {
                    let host = Arc::clone(&host);
                    tokio::spawn(async move { serve_conn(host, stream).await });
                }
                Err(e) => {
                    tracing::warn!("accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            },
            _ = stop.changed() => break,
            _ = recv_signal(&mut sigterm) => host.stop(Stop::Shutdown),
            _ = recv_signal(&mut sigint) => match signals {
                Signals::Default => host.stop(Stop::Shutdown),
                Signals::Takeover => tracing::info!("ignoring SIGINT (takeover host); stop it with SIGTERM or `ninox ptyd stop`"),
            },
        }
    }
    let how = *stop.borrow();
    drop(listener);
    match how {
        _ if how == Stop::HandedOff || host.released.load(Ordering::SeqCst) => {
            tracing::info!("handed off to successor; exiting without touching panes");
        }
        _ => {
            let h = Arc::clone(&host);
            let _ = tokio::task::spawn_blocking(move || h.kill_all()).await;
            // Only remove the socket if it is still ours (a successor may
            // have renamed its own over it).
            if inode.is_some() && socket_inode(&socket) == inode {
                let _ = std::fs::remove_file(&socket);
            }
            tracing::info!("ninox-ptyd stopped");
        }
    }
    Ok(())
}

async fn recv_signal(sig: &mut Option<tokio::signal::unix::Signal>) {
    match sig {
        Some(s) => {
            s.recv().await;
        }
        None => std::future::pending().await,
    }
}

// ---------------------------------------------------------------------------
// Connections
// ---------------------------------------------------------------------------

async fn send(wr: &mut OwnedWriteHalf, frame: &HostFrame, payload: &[u8]) -> std::io::Result<()> {
    write_frame(wr, frame, payload).await
}

async fn reply(wr: &mut OwnedWriteHalf, id: u64, result: ReplyResult, payload: &[u8]) -> std::io::Result<()> {
    let frame = HostFrame::Reply { id, result };
    match write_frame(wr, &frame, payload).await {
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
            // Too large to encode: tell the client instead of dropping it.
            let frame = HostFrame::Reply {
                id,
                result: ReplyResult::Err(err(ErrorCode::BadRequest, format!("reply too large: {e}"))),
            };
            write_frame(wr, &frame, &[]).await
        }
        r => r,
    }
}

/// Parse a header, salvaging the id for an error reply when the request
/// itself does not parse (unknown variant from a newer client, bad fields).
fn parse_client_frame(raw: &RawFrame) -> Result<ClientFrame, (u64, String)> {
    match serde_json::from_slice::<ClientFrame>(&raw.header) {
        Ok(f) => Ok(f),
        Err(e) => {
            let id = serde_json::from_slice::<serde_json::Value>(&raw.header)
                .ok()
                .and_then(|v| v.get("id").and_then(|i| i.as_u64()))
                .unwrap_or(0);
            Err((id, e.to_string()))
        }
    }
}

async fn serve_conn(host: Arc<Host>, stream: UnixStream) {
    let (mut rd, mut wr) = stream.into_split();

    // Handshake.
    let Ok(Some(first)) = read_frame(&mut rd).await else { return };
    match parse_client_frame(&first) {
        Ok(ClientFrame { id, request: Request::Hello { version, client } }) => {
            if version != PROTOCOL_VERSION {
                let e = err(
                    ErrorCode::VersionMismatch,
                    format!("host speaks protocol {PROTOCOL_VERSION}, client {client:?} speaks {version}"),
                );
                let _ = reply(&mut wr, id, ReplyResult::Err(e), &[]).await;
                return;
            }
            let hello = Reply::Hello { version: PROTOCOL_VERSION, host_pid: std::process::id(), epoch_ms: host.epoch_ms };
            if reply(&mut wr, id, ReplyResult::Ok(hello), &[]).await.is_err() {
                return;
            }
        }
        Ok(ClientFrame { id, .. }) | Err((id, _)) => {
            let _ = reply(&mut wr, id, ReplyResult::Err(err(ErrorCode::BadRequest, "first request must be Hello")), &[]).await;
            return;
        }
    }

    loop {
        let raw = match read_frame(&mut rd).await {
            Ok(Some(f)) => f,
            // EOF, or a broken/oversized frame: the stream cannot be resynced.
            Ok(None) | Err(_) => return,
        };
        let frame = match parse_client_frame(&raw) {
            Ok(f) => f,
            Err((id, msg)) => {
                if reply(&mut wr, id, ReplyResult::Err(err(ErrorCode::BadRequest, msg)), &[]).await.is_err() {
                    return;
                }
                continue;
            }
        };
        let id = frame.id;
        match frame.request {
            Request::Subscribe { pane, mode } => {
                let Some(p) = host.pane(&pane) else {
                    let e = err(ErrorCode::NotFound, format!("no pane {pane}"));
                    let _ = reply(&mut wr, id, ReplyResult::Err(e), &[]).await;
                    continue;
                };
                run_subscription(p, mode, id, rd, wr).await;
                return;
            }
            Request::Shutdown => {
                let _ = reply(&mut wr, id, ReplyResult::Ok(Reply::Ok), &[]).await;
                host.stop(Stop::Shutdown);
                return;
            }
            Request::Handoff => {
                // Takes over the stream; on success the host stops.
                let stream = match rd.reunite(wr) {
                    Ok(s) => s,
                    Err(_) => return,
                };
                crate::handoff::serve_handoff(Arc::clone(&host), stream, id).await;
                return;
            }
            request => {
                let h = Arc::clone(&host);
                let joined = tokio::task::spawn_blocking(move || h.handle(request, raw.payload)).await;
                let (result, payload) = match joined {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::error!("request handler panicked: {e}");
                        (ReplyResult::Err(err(ErrorCode::Internal, "request handler panicked")), Vec::new())
                    }
                };
                if reply(&mut wr, id, result, &payload).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// Event-stream mode for one connection. Ends when the client goes away,
/// the pane is killed, or (raw mode) the pane exits.
async fn run_subscription(p: Arc<Pane>, mode: SubscribeMode, id: u64, mut rd: OwnedReadHalf, mut wr: OwnedWriteHalf) {
    let pane_id = p.meta.id.clone();
    let mut frames = p.frames.subscribe();
    let mut raw_rx = match mode {
        SubscribeMode::Raw => Some(p.add_raw_subscriber()),
        SubscribeMode::Frames => None,
    };
    if reply(&mut wr, id, ReplyResult::Ok(Reply::Ok), &[]).await.is_err() {
        return;
    }
    // Detects the client closing its end; anything it sends is ignored.
    let mut client_gone = Box::pin(async move {
        let mut buf = [0u8; 256];
        loop {
            match tokio::io::AsyncReadExt::read(&mut rd, &mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
        }
    });

    let output = |pane: &str| HostFrame::Event(Event::Output { pane: pane.to_string() });
    let exited = |pane: &str, code| HostFrame::Event(Event::Exited { pane: pane.to_string(), code });

    match raw_rx.as_mut() {
        Some(rx) => {
            let mut sig = *frames.borrow_and_update();
            loop {
                if sig.gone {
                    return;
                }
                if let Some(code) = sig.exited {
                    // Deliver whatever output is still queued, then the exit.
                    while let Ok(bytes) = rx.try_recv() {
                        if send(&mut wr, &output(&pane_id), &bytes).await.is_err() {
                            return;
                        }
                    }
                    let _ = send(&mut wr, &exited(&pane_id, code), &[]).await;
                    return;
                }
                tokio::select! {
                    msg = rx.recv() => match msg {
                        Some(bytes) => {
                            if send(&mut wr, &output(&pane_id), &bytes).await.is_err() {
                                return;
                            }
                        }
                        None => return,
                    },
                    changed = frames.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        sig = *frames.borrow_and_update();
                    }
                    _ = &mut client_gone => return,
                }
            }
        }
        None => {
            // Current state first, so the client can pull an initial Screen.
            let seq = {
                let mut st = lock(&p.state);
                st.settle();
                st.seq
            };
            if send(&mut wr, &HostFrame::Event(Event::ScreenChanged { pane: pane_id.clone(), seq }), &[]).await.is_err() {
                return;
            }
            let mut last_seq = seq;
            let mut exit_sent = false;
            let mut sig = *frames.borrow_and_update();
            loop {
                if sig.gone {
                    return;
                }
                if sig.seq > last_seq {
                    last_seq = sig.seq;
                    let ev = HostFrame::Event(Event::ScreenChanged { pane: pane_id.clone(), seq: sig.seq });
                    if send(&mut wr, &ev, &[]).await.is_err() {
                        return;
                    }
                }
                if let (Some(code), false) = (sig.exited, exit_sent) {
                    exit_sent = true;
                    if send(&mut wr, &exited(&pane_id, code), &[]).await.is_err() {
                        return;
                    }
                }
                tokio::select! {
                    changed = frames.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        sig = *frames.borrow_and_update();
                    }
                    _ = &mut client_gone => return,
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

pub async fn run(socket: PathBuf, checkpoint_dir: Option<PathBuf>, signals: Signals) -> Result<()> {
    ignore_sighup();
    prepare_socket_dir(&socket)?;
    let lock_fd = acquire_lock(&socket)?;
    let listener = bind_fresh(&socket).await?;
    let host = Host::new(checkpoint_dir, now_ms())?;
    host.set_lock_fd(lock_fd);
    serve(host, listener, socket, signals).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(pane: &str) -> SpawnSpec {
        SpawnSpec {
            pane: pane.into(),
            argv: vec!["/bin/sh".into(), "-c".into(), "exit 0".into()],
            cwd: "/".into(),
            env: vec![],
            env_remove: vec![],
            cols: 80,
            rows: 24,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn spawn_is_refused_while_handing_off() {
        let host = Host::new(None, 1).unwrap();
        host.begin_handoff();
        let e = host.spawn_pane(spec("late")).unwrap_err();
        assert_eq!(e.code, ErrorCode::Unavailable);
        assert!(host.pane("late").is_none());

        host.abort_handoff();
        assert!(host.spawn_pane(spec("late")).is_ok(), "an aborted handoff re-enables spawns");
        host.kill_all();
    }
}
