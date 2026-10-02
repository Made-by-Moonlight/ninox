//! Pane I/O off the UI loop. One Frames subscription per visible pane pulls
//! `screen()` on `ScreenChanged` (coalesced through a `Notify`), and a single
//! command task owns the request connection so writes/resizes stay ordered.
//! Everything reports back over one channel; the UI loop never awaits ptyd.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ninox_ptyd::{checkpoint::Checkpoint, Event, PaneInfo, PtydClient, ScreenSnapshot, SubscribeMode};
use tokio::sync::{mpsc, watch, Notify};
use tokio::task::JoinHandle;

const CLIENT_NAME: &str = "ninox-tui";

#[derive(Debug)]
pub enum PaneEvent {
    /// `requested` is the scrollback this snapshot was asked for, so the UI
    /// can tell "history ran out" from "stale reply".
    Screen { pane: String, snap: ScreenSnapshot, requested: usize },
    Exited { pane: String, code: Option<i32> },
    Checkpoint { pane: String, checkpoint: Option<Checkpoint> },
    Panes(Vec<PaneInfo>),
    HostDown(String),
    WriteFailed { pane: String, error: String },
    /// A tmux viewer for `session` could not start.
    ViewerFailed { session: String, error: String },
}

enum Cmd {
    Write { pane: String, bytes: Vec<u8> },
    Resize { pane: String, cols: u16, rows: u16 },
    List,
    StartViewer { session: String, pane: String, cols: u16, rows: u16 },
    Kill { pane: String },
    ReapStaleViewers,
    /// Resize viewer panes `(session, pane, cols, rows)` whose tmux window
    /// changed size since, to the window's size.
    FitViewers(Vec<(String, String, u16, u16)>),
}

struct Sub {
    task: JoinHandle<()>,
    scrollback: watch::Sender<usize>,
}

pub struct Hub {
    socket: PathBuf,
    tx: mpsc::UnboundedSender<PaneEvent>,
    cmd: mpsc::UnboundedSender<Cmd>,
    subs: HashMap<String, Sub>,
}

impl Hub {
    pub fn new(socket: PathBuf) -> (Self, mpsc::UnboundedReceiver<PaneEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (cmd, cmd_rx) = mpsc::unbounded_channel();
        tokio::spawn(command_task(socket.clone(), cmd_rx, tx.clone()));
        (Self { socket, tx, cmd, subs: HashMap::new() }, rx)
    }

    pub fn write(&self, pane: &str, bytes: Vec<u8>) {
        let _ = self.cmd.send(Cmd::Write { pane: pane.to_string(), bytes });
    }

    pub fn resize(&self, pane: &str, cols: u16, rows: u16) {
        let _ = self.cmd.send(Cmd::Resize { pane: pane.to_string(), cols, rows });
    }

    pub fn refresh_list(&self) {
        let _ = self.cmd.send(Cmd::List);
    }

    /// Spawn ptyd pane `pane` running `tmux attach` for legacy `session`.
    /// Ordered with kills on the command connection, so a viewer reopened
    /// right after being retired is never killed by the stale kill.
    pub fn start_viewer(&self, session: &str, pane: &str, cols: u16, rows: u16) {
        let _ = self.cmd.send(Cmd::StartViewer { session: session.to_string(), pane: pane.to_string(), cols, rows });
    }

    /// Keep viewer panes at their tmux window's size (it changes when a
    /// full-size client, e.g. the desktop app, resizes it).
    pub fn fit_viewers(&self, viewers: Vec<(String, String, u16, u16)>) {
        if !viewers.is_empty() {
            let _ = self.cmd.send(Cmd::FitViewers(viewers));
        }
    }

    /// Kill a viewer pane and drop its checkpoint.
    pub fn kill_viewer(&self, pane: &str) {
        let _ = self.cmd.send(Cmd::Kill { pane: pane.to_string() });
    }

    /// Kill the viewer panes (and checkpoints) of TUIs no longer running.
    pub fn reap_stale_viewers(&self) {
        let _ = self.cmd.send(Cmd::ReapStaleViewers);
    }

    pub fn load_checkpoint(&self, pane: &str) {
        let tx = self.tx.clone();
        let pane = pane.to_string();
        tokio::spawn(async move {
            let id = pane.clone();
            let checkpoint = tokio::task::spawn_blocking(move || {
                ninox_ptyd::checkpoint::load(&ninox_ptyd::checkpoint::default_dir(), &id)
            })
            .await
            .ok()
            .flatten();
            let _ = tx.send(PaneEvent::Checkpoint { pane, checkpoint });
        });
    }

    /// Converge subscriptions on `wanted` (pane id → scrollback lines).
    pub fn sync(&mut self, wanted: &HashMap<String, usize>) {
        self.subs.retain(|id, sub| {
            let keep = wanted.contains_key(id) && !sub.task.is_finished();
            if !keep {
                sub.task.abort();
            }
            keep
        });
        for (id, &scrollback) in wanted {
            match self.subs.get(id) {
                Some(sub) => {
                    sub.scrollback.send_if_modified(|v| {
                        let changed = *v != scrollback;
                        *v = scrollback;
                        changed
                    });
                }
                None => {
                    let (stx, srx) = watch::channel(scrollback);
                    let task = tokio::spawn(subscription_task(self.socket.clone(), id.clone(), self.tx.clone(), srx));
                    self.subs.insert(id.clone(), Sub { task, scrollback: stx });
                }
            }
        }
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        for sub in self.subs.values() {
            sub.task.abort();
        }
    }
}

async fn command_task(socket: PathBuf, mut rx: mpsc::UnboundedReceiver<Cmd>, tx: mpsc::UnboundedSender<PaneEvent>) {
    let mut client: Option<PtydClient> = None;
    while let Some(cmd) = rx.recv().await {
        if client.is_none() {
            match PtydClient::connect(&socket, CLIENT_NAME).await {
                Ok(c) => client = Some(c),
                Err(e) => {
                    let _ = tx.send(PaneEvent::HostDown(e.to_string()));
                    continue;
                }
            }
        }
        let c = client.as_mut().expect("connected above");
        let res = match &cmd {
            Cmd::Write { pane, bytes } => c.write(pane, bytes).await,
            Cmd::Resize { pane, cols, rows } => c.resize(pane, *cols, *rows).await,
            Cmd::List => c.list().await.map(|panes| {
                let _ = tx.send(PaneEvent::Panes(panes));
            }),
            Cmd::StartViewer { session, pane, cols, rows } => {
                if let Err(error) = start_viewer(c, session, pane, *cols, *rows, &tx).await {
                    let _ = tx.send(PaneEvent::ViewerFailed { session: session.clone(), error: format!("{error:#}") });
                }
                Ok(())
            }
            Cmd::Kill { pane } => {
                let res = c.kill(pane).await;
                forget_checkpoint_later(pane.clone());
                res
            }
            Cmd::ReapStaleViewers => reap_stale(c).await,
            Cmd::FitViewers(viewers) => fit_viewers(c, viewers).await,
        };
        if let Err(e) = res {
            // A host-side error keeps the connection; a transport error
            // drops it so the next command reconnects.
            if e.downcast_ref::<ninox_ptyd::client::HostError>().is_none() {
                client = None;
            }
            let _ = tx.send(match cmd {
                Cmd::Write { pane, .. } | Cmd::Resize { pane, .. } => PaneEvent::WriteFailed { pane, error: e.to_string() },
                Cmd::List => PaneEvent::HostDown(e.to_string()),
                Cmd::StartViewer { .. } | Cmd::Kill { .. } | Cmd::ReapStaleViewers | Cmd::FitViewers(_) => continue,
            });
        }
    }
}

async fn start_viewer(
    client: &mut PtydClient,
    session: &str,
    pane: &str,
    cols: u16,
    rows: u16,
    tx: &mpsc::UnboundedSender<PaneEvent>,
) -> anyhow::Result<()> {
    anyhow::ensure!(ninox_core::runtime::has_session(session).await, "no live tmux session");
    let argv = ninox_core::runtime::viewer_attach_args(ninox_core::runtime::attach_args(session).await);
    // At the window's own size, so attaching never resizes the agent.
    let (cols, rows) = ninox_core::runtime::viewer_size(session).await.unwrap_or((cols, rows));
    let spec = ninox_core::runtime::ptyd_backend::viewer_spawn_spec(pane, argv, cols, rows);
    match client.spawn(spec).await {
        Ok(_) => {}
        // Ours from before a host blip: reuse it.
        Err(e) if host_code(&e) == Some(ninox_ptyd::ErrorCode::AlreadyExists) => {}
        Err(e) => return Err(e),
    }
    let _ = tx.send(PaneEvent::Panes(client.list().await?));
    Ok(())
}

async fn fit_viewers(client: &mut PtydClient, viewers: &[(String, String, u16, u16)]) -> anyhow::Result<()> {
    let mut resized = false;
    for (session, pane, cols, rows) in viewers {
        match ninox_core::runtime::viewer_size(session).await {
            Some(size) if size != (*cols, *rows) => {
                client.resize(pane, size.0, size.1).await?;
                resized = true;
            }
            _ => {}
        }
    }
    if resized {
        // Fresh sizes, so the next fit doesn't resize again.
        client.list().await?;
    }
    Ok(())
}

fn host_code(e: &anyhow::Error) -> Option<ninox_ptyd::ErrorCode> {
    e.downcast_ref::<ninox_ptyd::client::HostError>().map(|h| h.0.code)
}

/// A viewer is stale when the TUI that owns it is gone. Ours are never
/// stale: this TUI may already have started some by the time it reaps (one
/// left by a dead process that had our pid is simply adopted).
fn is_stale_viewer(pane: &str, own_pid: u32, alive: impl Fn(u32) -> bool) -> bool {
    ninox_core::runtime::parse_viewer_pane(pane).is_some_and(|(pid, _)| pid != own_pid && !alive(pid))
}

async fn reap_stale(c: &mut PtydClient) -> anyhow::Result<()> {
    let own = std::process::id();
    let alive = ninox_core::lifecycle::probe::is_pid_alive;
    for p in c.list().await? {
        if is_stale_viewer(&p.pane, own, alive) {
            let _ = c.kill(&p.pane).await;
        }
    }
    // Checkpoints the host wrote for viewers that are already gone.
    let dir = ninox_ptyd::checkpoint::default_dir();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(rest) = name.strip_prefix("tmux-view_") else { continue };
            let pid = rest.split('_').next().and_then(|p| p.parse::<u32>().ok());
            if pid.is_some_and(|pid| pid != own && !alive(pid)) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    Ok(())
}

/// The host writes a final checkpoint as a killed pane exits, after the
/// kill reply; removing it a moment later catches that write.
fn forget_checkpoint_later(pane: String) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        ninox_ptyd::checkpoint::remove(&ninox_ptyd::checkpoint::default_dir(), &pane);
    });
}

/// Kill this TUI's viewer panes; called as the TUI exits. With
/// `shutdown_if_idle` (this TUI started the host just for viewers), also
/// stop the host when nothing but those viewers ran on it.
pub async fn kill_own_viewers(socket: &std::path::Path, shutdown_if_idle: bool) {
    let own = std::process::id();
    let work = async {
        let mut c = PtydClient::connect(socket, CLIENT_NAME).await.ok()?;
        let mine: Vec<String> = c
            .list()
            .await
            .ok()?
            .into_iter()
            .map(|p| p.pane)
            .filter(|p| ninox_core::runtime::parse_viewer_pane(p).is_some_and(|(pid, _)| pid == own))
            .collect();
        for pane in &mine {
            let _ = c.kill(pane).await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        let dir = ninox_ptyd::checkpoint::default_dir();
        for pane in &mine {
            ninox_ptyd::checkpoint::remove(&dir, pane);
        }
        if shutdown_if_idle && only_own_viewers(&c.list().await.ok()?, own) {
            let _ = c.shutdown().await;
        }
        Some(())
    };
    let _ = tokio::time::timeout(Duration::from_secs(3), work).await;
}

/// Whether `panes` holds nothing but `own`'s viewers (dead or alive):
/// stopping that host loses no agent and no other TUI's view.
fn only_own_viewers(panes: &[PaneInfo], own: u32) -> bool {
    panes.iter().all(|p| ninox_core::runtime::parse_viewer_pane(&p.pane).is_some_and(|(pid, _)| pid == own))
}

struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// tmux re-encodes an agent's synchronized frame as several partial
/// updates, so a viewer pane snapshotted mid-burst shows a torn frame. Wait
/// for output to go quiet this long (bounded by VIEWER_SETTLE_MAX, so a
/// constantly busy agent still updates) before pulling. Both arbitrary —
/// tune if tmux sessions still tear or feel laggy.
const VIEWER_SETTLE: Duration = Duration::from_millis(12);
const VIEWER_SETTLE_MAX: Duration = Duration::from_millis(100);

/// Returns once `dirty` has stayed silent for `quiet`, or `max` has passed.
async fn settle_output(dirty: &Notify, quiet: Duration, max: Duration) {
    let deadline = tokio::time::Instant::now() + max;
    loop {
        let window = (tokio::time::Instant::now() + quiet).min(deadline);
        tokio::select! {
            _ = dirty.notified() => {
                if tokio::time::Instant::now() >= deadline {
                    return;
                }
            }
            _ = tokio::time::sleep_until(window) => return,
        }
    }
}

async fn subscription_task(
    socket: PathBuf,
    pane: String,
    tx: mpsc::UnboundedSender<PaneEvent>,
    mut scrollback: watch::Receiver<usize>,
) {
    loop {
        match subscribe_once(&socket, &pane, &tx, &mut scrollback).await {
            Ok(()) => return,
            // Pane not (yet) known or host restarting: retry quietly — no
            // tracing here, its subscriber writes to the TUI's stdout.
            Err(_) => tokio::time::sleep(Duration::from_secs(1)).await,
        }
    }
}

/// `Ok` when the host closed the stream (pane gone); `Err` to retry.
async fn subscribe_once(
    socket: &std::path::Path,
    pane: &str,
    tx: &mpsc::UnboundedSender<PaneEvent>,
    scrollback: &mut watch::Receiver<usize>,
) -> anyhow::Result<()> {
    let mut sub = PtydClient::subscribe(socket, pane, SubscribeMode::Frames).await?;
    let mut client = PtydClient::connect(socket, CLIENT_NAME).await?;
    let dirty = Arc::new(Notify::new());
    let (done_tx, mut done_rx) = tokio::sync::oneshot::channel::<anyhow::Result<()>>();
    let _reader = {
        let dirty = Arc::clone(&dirty);
        let tx = tx.clone();
        let pane = pane.to_string();
        AbortOnDrop(tokio::spawn(async move {
            let res = loop {
                match sub.next().await {
                    Ok(Some((Event::ScreenChanged { .. }, _))) => dirty.notify_one(),
                    Ok(Some((Event::Exited { code, .. }, _))) => {
                        let _ = tx.send(PaneEvent::Exited { pane: pane.clone(), code });
                        dirty.notify_one();
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => break Ok(()),
                    Err(e) => break Err(e),
                }
            };
            let _ = done_tx.send(res);
        }))
    };
    dirty.notify_one();
    let settle = if ninox_core::runtime::is_viewer_pane(pane) { Some(VIEWER_SETTLE) } else { None };
    loop {
        tokio::select! {
            _ = dirty.notified() => {
                if let Some(quiet) = settle {
                    settle_output(&dirty, quiet, VIEWER_SETTLE_MAX).await;
                }
            }
            changed = scrollback.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
            }
            res = &mut done_rx => return res.unwrap_or(Ok(())),
        }
        let requested = *scrollback.borrow_and_update();
        let snap = client.screen(pane, requested).await?;
        if tx.send(PaneEvent::Screen { pane: pane.to_string(), snap, requested }).is_err() {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_host_is_only_idle_when_nothing_but_our_viewers_run_on_it() {
        let own = std::process::id();
        let pane = |id: &str| PaneInfo {
            pane: id.into(), pid: 1, cols: 80, rows: 24, alive: false, exit_code: None,
            created_ms: 0, last_output_ms: 0, title: None, cwd: "/".into(), seq: 0, history_size: 0,
        };
        let mine = pane(&ninox_core::runtime::viewer_pane_id(own, "w1"));
        assert!(only_own_viewers(&[], own));
        assert!(only_own_viewers(std::slice::from_ref(&mine), own));
        assert!(!only_own_viewers(&[mine.clone(), pane("agent-1")], own), "an agent session runs on it");
        let other_tui = pane(&ninox_core::runtime::viewer_pane_id(own.wrapping_add(1), "w1"));
        assert!(!only_own_viewers(&[mine, other_tui], own), "another TUI is viewing through it");
    }

    use super::*;

    #[tokio::test]
    async fn settle_waits_for_quiet_but_is_bounded() {
        let dirty = Arc::new(Notify::new());
        let start = tokio::time::Instant::now();
        settle_output(&dirty, Duration::from_millis(12), Duration::from_millis(100)).await;
        let quiet = start.elapsed();
        assert!(quiet >= Duration::from_millis(12) && quiet < Duration::from_millis(90), "quiet output settles after one window: {quiet:?}");

        let noisy = Arc::clone(&dirty);
        let spam = tokio::spawn(async move {
            loop {
                noisy.notify_one();
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        let start = tokio::time::Instant::now();
        settle_output(&dirty, Duration::from_millis(12), Duration::from_millis(100)).await;
        spam.abort();
        let waited = start.elapsed();
        assert!(waited >= Duration::from_millis(100) && waited < Duration::from_millis(400), "busy output is capped: {waited:?}");
    }

    #[test]
    fn only_viewers_of_dead_tuis_are_stale() {
        let alive = |pid: u32| pid == 200;
        assert!(is_stale_viewer("tmux-view:100:w", 1, alive), "owner gone");
        assert!(!is_stale_viewer("tmux-view:200:w", 1, alive), "another TUI is still running");
        assert!(!is_stale_viewer("tmux-view:1:w", 1, alive), "ours, possibly just started");
        assert!(!is_stale_viewer("w", 1, alive), "sessions are never reaped");
    }
}
