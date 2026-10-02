//! `ninox pane attach <id>`: bridge the calling terminal to a pane.
//!
//! Puts stdin in raw mode, subscribes in `Raw` mode (full repaint first, then
//! live bytes → stdout), forwards stdin → `Write`, and propagates the calling
//! terminal's size (initially and on SIGWINCH) via `Resize`. Restores the
//! terminal on every exit path.
//!
//! The calling "terminal" may itself be a PTY owned by another program (the
//! Iced app runs this bridge on a PTY it reads from). That works the same
//! way, with two allowances: the size is read from whichever of
//! stdin/stdout is a terminal, and it is also polled once a second, because
//! a PTY without this process as its controlling terminal never delivers
//! SIGWINCH.
//!
//! With a detach chord configured and a real TTY, the bridge switches the
//! outer terminal to its alternate screen while attached (like tmux), so the
//! user's shell screen comes back on detach.
//!
//! Losing the host is not immediately the end: a live upgrade drops every
//! connection, so the bridge reconnects for up to [`RECONNECT_WINDOW`] and
//! carries on (with a fresh repaint) if a host with the same epoch, i.e.
//! the successor, still has the pane.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use crate::client::{PtydClient, Subscription};
use crate::protocol::{Event, SubscribeMode};

#[derive(Debug, Clone, Default)]
pub struct AttachOptions {
    /// Two-byte detach chord: `prefix` then `key`. `None` disables detaching
    /// (the Iced app embeds the bridge and closes it by killing the child).
    pub detach: Option<(u8, u8)>,
    /// Skip raw mode / size propagation when stdin is not a TTY.
    pub force_no_tty: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachEnd {
    Detached,
    PaneExited(Option<i32>),
    HostGone,
}

/// Resets the outer terminal's input modes the application may have turned
/// on through the raw stream, so the user's shell is usable after detach.
/// How long the bridge keeps trying to reach a successor host.
pub const RECONNECT_WINDOW: Duration = Duration::from_secs(5);

const RESET_MODES: &[u8] = b"\x1b[?2026l\x1b[0m\x1b[?1l\x1b[?2004l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1004l\x1b[<u\x1b[?25h";

pub async fn attach(socket: &Path, pane: &str, opts: AttachOptions) -> anyhow::Result<AttachEnd> {
    let tty = !opts.force_no_tty && is_tty(0);
    let mut control = PtydClient::connect(socket, "pane-attach").await?;
    let epoch = control.host_identity().1;
    // Fail with NotFound before touching the terminal.
    if control.info(pane).await?.is_none() {
        return Err(anyhow::Error::new(crate::client::HostError(crate::protocol::ErrorBody {
            code: crate::protocol::ErrorCode::NotFound,
            message: format!("no pane {pane}"),
        })));
    }

    let alt_screen = tty && opts.detach.is_some();
    let guard = if tty { Some(TerminalGuard::enter(alt_screen)?) } else { None };

    // Size first, so the repaint is rendered at our dimensions.
    let mut last_size = if tty { terminal_size() } else { None };
    if let Some((c, r)) = last_size {
        control.resize(pane, c, r).await?;
    }

    let mut sub = PtydClient::subscribe(socket, pane, SubscribeMode::Raw).await?;
    let (stdin_rx, stdin_stop) = spawn_stdin_reader()?;
    let mut stdin_rx = stdin_rx;
    let mut winch = if tty {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()).ok()
    } else {
        None
    };
    let mut size_poll = tokio::time::interval(Duration::from_secs(1));
    let mut stdout = tokio::io::stdout();
    let mut chord = ChordState::default();

    let end = loop {
        tokio::select! {
            ev = sub.next() => match ev {
                Ok(Some((Event::Output { .. }, bytes))) => {
                    if stdout.write_all(&bytes).await.is_err() || stdout.flush().await.is_err() {
                        // Our terminal is gone; nothing left to bridge to.
                        break AttachEnd::Detached;
                    }
                }
                Ok(Some((Event::Exited { code, .. }, _))) => break AttachEnd::PaneExited(code),
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => match reconnect(socket, pane, epoch, last_size).await {
                    Some((c, s)) => (control, sub) = (c, s),
                    None => break AttachEnd::HostGone,
                },
            },
            input = stdin_rx.recv() => match input {
                Some(bytes) => {
                    let (forward, detach) = chord.process(opts.detach, &bytes);
                    if !forward.is_empty() && control.write(pane, &forward).await.is_err() {
                        match reconnect(socket, pane, epoch, last_size).await {
                            Some((mut c, s)) => {
                                let _ = c.write(pane, &forward).await;
                                (control, sub) = (c, s);
                            }
                            None => break AttachEnd::HostGone,
                        }
                    }
                    if detach {
                        break AttachEnd::Detached;
                    }
                }
                // stdin closed (EOF): nothing more to forward, but keep
                // showing output when stdin was never interactive.
                None => {
                    if tty {
                        break AttachEnd::Detached;
                    }
                    stdin_rx = mpsc::channel(1).1;
                }
            },
            _ = recv_winch(&mut winch) => {
                if let Some(size) = terminal_size() {
                    last_size = Some(size);
                    if control.resize(pane, size.0, size.1).await.is_err() {
                        match reconnect(socket, pane, epoch, last_size).await {
                            Some((c, s)) => (control, sub) = (c, s),
                            None => break AttachEnd::HostGone,
                        }
                    }
                }
            }
            _ = size_poll.tick(), if tty => {
                let size = terminal_size();
                if size.is_some() && size != last_size {
                    last_size = size;
                    if let Some((c, r)) = size {
                        if control.resize(pane, c, r).await.is_err() {
                            match reconnect(socket, pane, epoch, last_size).await {
                                Some((c, s)) => (control, sub) = (c, s),
                                None => break AttachEnd::HostGone,
                            }
                        }
                    }
                }
            }
        }
    };
    stdin_stop.stop();
    if tty {
        let _ = stdout.write_all(RESET_MODES).await;
        let _ = stdout.flush().await;
    }
    drop(guard);
    Ok(end)
}

/// Reach the successor after a live upgrade: same epoch (a restarted host
/// has none of the old panes) and the pane still known. Re-applies `size`
/// and resubscribes, which starts with a full repaint.
async fn reconnect(socket: &Path, pane: &str, epoch: u64, size: Option<(u16, u16)>) -> Option<(PtydClient, Subscription)> {
    let deadline = tokio::time::Instant::now() + RECONNECT_WINDOW;
    loop {
        if let Ok(mut c) = PtydClient::connect(socket, "pane-attach").await {
            if c.host_identity().1 != epoch {
                return None;
            }
            match c.info(pane).await {
                Ok(Some(_)) => {
                    if let Some((cols, rows)) = size {
                        let _ = c.resize(pane, cols, rows).await;
                    }
                    if let Ok(sub) = PtydClient::subscribe(socket, pane, SubscribeMode::Raw).await {
                        return Some((c, sub));
                    }
                }
                Ok(None) => return None,
                Err(_) => {}
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn recv_winch(sig: &mut Option<tokio::signal::unix::Signal>) {
    match sig {
        Some(s) => {
            s.recv().await;
        }
        None => std::future::pending().await,
    }
}

#[derive(Default)]
struct ChordState {
    pending_prefix: bool,
}

impl ChordState {
    /// Returns the bytes to forward and whether the detach chord completed.
    /// `prefix prefix` sends one literal prefix byte; `prefix <other>` sends
    /// both bytes.
    fn process(&mut self, chord: Option<(u8, u8)>, input: &[u8]) -> (Vec<u8>, bool) {
        let Some((prefix, key)) = chord else { return (input.to_vec(), false) };
        let mut out = Vec::with_capacity(input.len());
        for &b in input {
            if self.pending_prefix {
                self.pending_prefix = false;
                if b == key {
                    return (out, true);
                }
                out.push(prefix);
                if b != prefix {
                    out.push(b);
                }
            } else if b == prefix {
                self.pending_prefix = true;
            } else {
                out.push(b);
            }
        }
        (out, false)
    }
}

fn is_tty(fd: i32) -> bool {
    unsafe { libc::isatty(fd) == 1 }
}

fn terminal_size() -> Option<(u16, u16)> {
    for fd in [0, 1, 2] {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ as _, &mut ws) } == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
            return Some((ws.ws_col, ws.ws_row));
        }
    }
    None
}

/// Raw mode (and optionally the alternate screen) for the life of the value;
/// restored on drop, including during unwinding.
struct TerminalGuard {
    saved: libc::termios,
    alt_screen: bool,
}

impl TerminalGuard {
    fn enter(alt_screen: bool) -> Result<Self> {
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(0, &mut saved) } != 0 {
            return Err(std::io::Error::last_os_error()).context("tcgetattr");
        }
        let mut raw = saved;
        unsafe { libc::cfmakeraw(&mut raw) };
        if unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw) } != 0 {
            return Err(std::io::Error::last_os_error()).context("tcsetattr");
        }
        if alt_screen {
            write_stdout(b"\x1b[?1049h");
        }
        Ok(Self { saved, alt_screen })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.alt_screen {
            write_stdout(b"\x1b[?1049l");
        }
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &self.saved);
        }
    }
}

fn write_stdout(bytes: &[u8]) {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(bytes);
    let _ = out.flush();
}

/// Stops the stdin thread by making its poll wake up.
struct StdinStop(OwnedFd);

impl StdinStop {
    fn stop(&self) {
        unsafe {
            libc::write(self.0.as_raw_fd(), b"x".as_ptr().cast(), 1);
        }
    }
}

/// Read stdin on a plain thread that `poll`s stdin and a stop pipe, so it
/// can be stopped without consuming a keystroke that belongs to whatever
/// runs after the bridge (tokio's stdin would leave a thread blocked in
/// `read`).
fn spawn_stdin_reader() -> Result<(mpsc::Receiver<Vec<u8>>, StdinStop)> {
    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("pipe");
    }
    let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    let (tx, rx) = mpsc::channel(64);
    std::thread::Builder::new().name("attach-stdin".into()).spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            let mut p = [
                libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: r.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            ];
            let n = unsafe { libc::poll(p.as_mut_ptr(), 2, -1) };
            if n < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return;
            }
            if p[1].revents != 0 {
                return;
            }
            if p[0].revents == 0 {
                continue;
            }
            let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
            if n < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return;
            }
            if n == 0 {
                return;
            }
            if tx.blocking_send(buf[..n as usize].to_vec()).is_err() {
                return;
            }
        }
    })?;
    Ok((rx, StdinStop(w)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detach_chord() {
        let chord = Some((0u8, b'd'));
        let mut s = ChordState::default();
        assert_eq!(s.process(chord, b"ab"), (b"ab".to_vec(), false));
        assert_eq!(s.process(chord, b"x\0"), (b"x".to_vec(), false));
        assert_eq!(s.process(chord, b"d"), (Vec::new(), true));
        let mut s = ChordState::default();
        assert_eq!(s.process(chord, b"\0\0q"), (b"\0q".to_vec(), false));
        assert_eq!(s.process(chord, b"\0z"), (b"\0z".to_vec(), false));
        assert_eq!(s.process(None, b"\0d"), (b"\0d".to_vec(), false));
    }
}
