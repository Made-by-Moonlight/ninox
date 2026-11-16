//! The evidence a long-running ninox process leaves behind: a size-capped
//! log file written off the runtime's threads, a panic hook that logs, the
//! stop signals the headless daemon reports, and a pid marker that exposes
//! deaths no handler can see (SIGKILL, jetsam).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::OnceLock;
use std::time::Duration;

/// The file is renamed to `<name>.1` (replacing the previous one) past this
/// size, so a log holds at most about twice this much.
const MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Lines queued for the writer thread; beyond this a stalled disk drops
/// lines rather than blocking the caller.
const QUEUE_LINES: usize = 4096;

/// Where the daemon serving `db_path` logs: `daemon.log` beside the db, so a
/// scratch `--db` keeps its log out of the real one.
pub fn log_path(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("daemon.log")
}

struct RotatingFile {
    path:    PathBuf,
    file:    Option<File>,
    written: u64,
}

impl RotatingFile {
    fn open(path: PathBuf) -> Self {
        let written = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let mut f = Self { path, file: None, written };
        f.reopen();
        f
    }

    fn reopen(&mut self) {
        self.file = OpenOptions::new().create(true).append(true).open(&self.path).ok();
    }

    fn write(&mut self, buf: &[u8]) {
        if self.written >= MAX_BYTES {
            self.file = None;
            let mut rotated = self.path.clone().into_os_string();
            rotated.push(".1");
            let _ = std::fs::rename(&self.path, rotated);
            self.written = 0;
            self.reopen();
        }
        if let Some(f) = self.file.as_mut() {
            if f.write_all(buf).is_ok() {
                self.written += buf.len() as u64;
            }
        }
    }
}

enum Msg {
    Line(Vec<u8>),
    Flush(SyncSender<()>),
}

struct AsyncLog {
    tx: SyncSender<Msg>,
}

static LOG: OnceLock<AsyncLog> = OnceLock::new();

fn writer_thread(mut file: RotatingFile, rx: Receiver<Msg>) {
    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Line(buf) => file.write(&buf),
            Msg::Flush(done) => {
                let _ = done.send(());
            }
        }
    }
}

/// Start mirroring log output into `path` from a dedicated thread. A file
/// that can't be opened leaves logging as it was; a second call is a no-op.
pub fn init_file(path: &Path) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let (tx, rx) = sync_channel(QUEUE_LINES);
    let file = RotatingFile::open(path.to_path_buf());
    if std::thread::Builder::new().name("ninox-log".into()).spawn(move || writer_thread(file, rx)).is_ok() {
        let _ = LOG.set(AsyncLog { tx });
    }
}

pub fn file_enabled() -> bool {
    LOG.get().is_some()
}

/// Queue `buf` for the file; dropped when the queue is full.
pub fn write(buf: &[u8]) {
    if let Some(log) = LOG.get() {
        let _ = log.tx.try_send(Msg::Line(buf.to_vec()));
    }
}

/// Wait (briefly) until everything queued so far is on disk. Call before
/// `process::exit`, which doesn't give the writer thread a chance to drain.
pub fn flush() {
    let Some(log) = LOG.get() else { return };
    let (done_tx, done_rx) = sync_channel(1);
    if send_within(&log.tx, Msg::Flush(done_tx), Duration::from_secs(2)) {
        let _ = done_rx.recv_timeout(Duration::from_secs(2));
    }
}

/// `SyncSender` has no `send_timeout`: poll `try_send` until `limit`.
fn send_within(tx: &SyncSender<Msg>, mut msg: Msg, limit: Duration) -> bool {
    let deadline = std::time::Instant::now() + limit;
    loop {
        match tx.try_send(msg) {
            Ok(()) => return true,
            Err(TrySendError::Full(m)) if std::time::Instant::now() < deadline => msg = m,
            Err(_) => return false,
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Whether this process's stderr is the file at `path` — a daemon started
/// by `spawn_headless` has its stderr pointed at `daemon.log`, so teeing to
/// both would print every line twice.
pub fn stderr_is(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else { return false };
    // SAFETY: an all-zero `stat` is a valid out-parameter for fstat.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: fstat on fd 2 with a valid out-pointer.
    if unsafe { libc::fstat(2, &mut st) } != 0 {
        return false;
    }
    st.st_ino as u64 == meta.ino() && st.st_dev as u64 == meta.dev()
}

/// Log panics (message and location) before the default hook runs, so a
/// panic on any thread reaches the log file and not just a stderr nobody
/// kept.
pub fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let payload = info.payload();
        let msg = payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "non-string payload".to_string());
        let at = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
        tracing::error!("panic in thread '{}' at {at}: {msg}", thread.name().unwrap_or("<unnamed>"));
        flush();
        prev(info);
    }));
}

/// The stop signals the headless daemon reports. tokio doesn't expose the
/// sender's pid, so these name the signal only.
pub struct StopSignals {
    int:         tokio::signal::unix::Signal,
    term:        tokio::signal::unix::Signal,
    quit:        tokio::signal::unix::Signal,
    hup:         tokio::signal::unix::Signal,
    hup_ignored: bool,
}

impl StopSignals {
    /// Must run inside a tokio runtime. Registering replaces a `SIG_IGN`
    /// disposition, so whether SIGHUP was ignored (`nohup`) is read first
    /// and keeps being honoured.
    pub fn install() -> std::io::Result<Self> {
        use tokio::signal::unix::{signal, SignalKind};
        let hup_ignored = sighup_ignored();
        Ok(Self {
            int: signal(SignalKind::interrupt())?,
            term: signal(SignalKind::terminate())?,
            quit: signal(SignalKind::quit())?,
            hup: signal(SignalKind::hangup())?,
            hup_ignored,
        })
    }

    /// Resolves with the name of the first signal that should stop the
    /// daemon. An ignored SIGHUP is logged and otherwise left alone.
    pub async fn stop(&mut self) -> &'static str {
        loop {
            tokio::select! {
                _ = self.int.recv() => return "SIGINT",
                _ = self.term.recv() => return "SIGTERM",
                _ = self.quit.recv() => return "SIGQUIT",
                _ = self.hup.recv() => {
                    if self.hup_ignored {
                        tracing::warn!("SIGHUP received — ignored (started under nohup)");
                    } else {
                        return "SIGHUP";
                    }
                }
            }
        }
    }
}

fn sighup_ignored() -> bool {
    // SAFETY: querying the disposition (null new action) with a valid out-pointer.
    unsafe {
        let mut old: libc::sigaction = std::mem::zeroed();
        libc::sigaction(libc::SIGHUP, std::ptr::null(), &mut old) == 0 && old.sa_sigaction == libc::SIG_IGN
    }
}

/// What a previous run beside this db left in `daemon.pid`.
#[derive(Debug, PartialEq, Eq)]
pub enum PriorRun {
    /// No marker: a first run, or the last one exited with a logged reason.
    Clean,
    /// A marker whose process is gone: it ended without logging an exit.
    Vanished(u32),
    /// Another daemon on this db is alive; its marker is left alone.
    Live(u32),
}

fn marker_path(db_path: &Path) -> PathBuf {
    log_path(db_path).with_file_name("daemon.pid")
}

/// Record this process as the running daemon, reporting what the previous
/// marker says about the run before it.
pub fn claim_marker(db_path: &Path) -> PriorRun {
    let path = marker_path(db_path);
    let own = std::process::id();
    let prior = match std::fs::read_to_string(&path).ok().and_then(|s| s.trim().parse::<u32>().ok()) {
        None => PriorRun::Clean,
        Some(pid) if pid == own => PriorRun::Clean,
        Some(pid) if crate::lifecycle::probe::is_pid_alive(pid) => PriorRun::Live(pid),
        Some(pid) => PriorRun::Vanished(pid),
    };
    if !matches!(prior, PriorRun::Live(_)) {
        // `create_new` after clearing the stale file: of two daemons starting
        // together, only one creates it and the other sees a live owner.
        if matches!(prior, PriorRun::Vanished(_)) {
            let _ = std::fs::remove_file(&path);
        }
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                let _ = writeln!(f, "{own}");
            }
            Err(_) => {
                if let Some(pid) = read_marker(&path).filter(|p| *p != own && crate::lifecycle::probe::is_pid_alive(*p)) {
                    return PriorRun::Live(pid);
                }
            }
        }
    }
    prior
}

fn read_marker(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok().and_then(|s| s.trim().parse().ok())
}

/// A daemon that lost the port race to `pid` gives it the marker it holds,
/// so the daemon that is actually serving stays covered.
pub fn hand_over_marker(db_path: &Path, pid: u32) {
    let path = marker_path(db_path);
    if read_marker(&path) == Some(std::process::id()) {
        let _ = std::fs::write(path, format!("{pid}\n"));
    }
}

/// Remove the marker if it is still ours: a clean, logged exit.
pub fn release_marker(db_path: &Path) {
    let path = marker_path(db_path);
    let ours = std::fs::read_to_string(&path).ok().and_then(|s| s.trim().parse::<u32>().ok()) == Some(std::process::id());
    if ours {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_lives_beside_the_db() {
        assert_eq!(log_path(Path::new("/x/data/ninox.db")), Path::new("/x/data/daemon.log"));
        assert_eq!(log_path(Path::new("ninox.db")), Path::new("./daemon.log"));
    }

    #[test]
    fn the_file_rotates_instead_of_growing_without_bound() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.log");
        let mut file = RotatingFile::open(path.clone());
        let line = vec![b'x'; 1024 * 1024];
        for _ in 0..6 {
            file.write(&line);
        }
        assert!(std::fs::metadata(&path).unwrap().len() <= MAX_BYTES + line.len() as u64);
        assert!(dir.path().join("daemon.log.1").exists());
    }

    #[test]
    fn a_marker_of_a_dead_process_is_reported_once_and_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("ninox.db");
        assert_eq!(claim_marker(&db), PriorRun::Clean);
        release_marker(&db);
        assert!(!marker_path(&db).exists());

        // A pid that cannot be alive: above any kernel's pid_max.
        std::fs::write(marker_path(&db), "2000000000\n").unwrap();
        assert_eq!(claim_marker(&db), PriorRun::Vanished(2_000_000_000));
        assert_eq!(std::fs::read_to_string(marker_path(&db)).unwrap().trim(), std::process::id().to_string());
    }

    #[test]
    fn a_daemon_that_lost_the_port_hands_its_marker_to_the_winner() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("ninox.db");
        assert_eq!(claim_marker(&db), PriorRun::Clean);
        hand_over_marker(&db, 4242);
        release_marker(&db);
        assert_eq!(std::fs::read_to_string(marker_path(&db)).unwrap().trim(), "4242");
    }

    #[test]
    fn a_live_foreign_marker_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("ninox.db");
        let parent = std::os::unix::process::parent_id();
        std::fs::write(marker_path(&db), format!("{parent}\n")).unwrap();
        assert_eq!(claim_marker(&db), PriorRun::Live(parent));
        release_marker(&db);
        assert!(marker_path(&db).exists(), "not ours to remove");
    }

    #[tokio::test]
    async fn sigterm_is_named() {
        let mut signals = StopSignals::install().unwrap();
        // SAFETY: raising a signal this test has a handler for.
        unsafe { libc::raise(libc::SIGTERM) };
        assert_eq!(tokio::time::timeout(Duration::from_secs(5), signals.stop()).await.unwrap(), "SIGTERM");
    }
}
