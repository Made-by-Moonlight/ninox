//! The session runtime seam: every operation the engine performs on a live
//! agent pane goes through [`SessionBackend`], so the tmux server and the
//! `ninox-ptyd` PTY host are interchangeable underneath it.
//!
//! Callers use the free functions below rather than a backend directly.
//! They encode the dispatch rules:
//!
//! - **New sessions** go to the backend passed to [`create_session`]
//!   (normally `[runtime] backend` from the config, via
//!   [`configured_backend`]).
//! - **Existing sessions** go to ptyd if ptyd knows the pane, else tmux. No
//!   store column records which backend owns a session, so a live tmux
//!   fleet keeps working after the default flips to ptyd.
//!
//! Test binaries never reach a real ptyd host: unless `NINOX_PTYD_SOCKET`
//! points into the temp dir (a test's own isolated host), ptyd is disabled
//! and everything resolves to tmux's isolated `-L ninox-test` server. The
//! override alone isn't enough because every ptyd pane exports the real
//! socket path — `cargo test` run by a worker would otherwise reach its own
//! host.

pub(crate) mod process;
pub(crate) mod prompt;
pub mod ptyd_backend;
pub mod tmux_backend;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::events::Engine;
use crate::types::SessionId;

pub use ptyd_backend::PtydBackend;
pub use tmux_backend::TmuxBackend;

/// Which runtime hosts newly created sessions (`[runtime] backend`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    /// `ninox-ptyd`: Ninox's own PTY host. Opt-in until the desktop app has
    /// been proven on it.
    Ptyd,
    /// The private `-L ninox` tmux server.
    #[default]
    Tmux,
}

impl Backend {
    pub const ALL: [Backend; 2] = [Backend::Tmux, Backend::Ptyd];

    /// One line on what this runtime does, for the settings card.
    pub fn description(&self) -> &'static str {
        match self {
            Backend::Tmux => "Sessions run on Ninox's private tmux server (`tmux -L ninox`), as they always \
                 have. The terminal view attaches through tmux, which re-renders the agent's output.",
            Backend::Ptyd => "Sessions run on Ninox's own PTY host (`ninox ptyd`) — no tmux. The terminal \
                 view gets the agent's own bytes, so synchronized frames arrive intact, and agents \
                 survive app restarts and upgrades. Applies to new sessions; running ones stay where \
                 they are.",
        }
    }
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Backend::Tmux => "tmux",
            Backend::Ptyd => "ptyd (no tmux)",
        })
    }
}

/// A live pane as reported by its backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSession {
    pub id:         String,
    pub created_ms: i64,
    pub pid:        Option<u32>,
    pub backend:    Backend,
}

/// Whether a session's pane is running, for startup reconciliation. Only
/// `Dead` may turn a session `Interrupted`/`Terminated`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    Live,
    Dead,
    /// The host that may hold the pane (ptyd) can't be reached right now —
    /// mid-upgrade or slow to start. Not evidence the pane is gone.
    Unknown,
}

/// The exact pane that owns the calling process — the binding
/// `orchestrator_auth` persists. For tmux panes this is the tmux session,
/// pane id and server incarnation; for ptyd panes the session id,
/// `ptyd:<id>` and the host's epoch.
pub type PaneIdentity = crate::tmux::TmuxPaneIdentity;

#[async_trait]
pub trait SessionBackend: Send + Sync {
    fn kind(&self) -> Backend;

    /// Whether this backend holds a pane named `id`, alive or not. Decides
    /// dispatch for existing sessions.
    async fn knows(&self, id: &str) -> bool;

    /// Start `cmd` in a login shell in `workspace`, with `env` on top of the
    /// backend's inherited environment. Fails on a duplicate id rather than
    /// replacing a live pane.
    async fn create_session(&self, id: &str, workspace: &str, cmd: &str, env: &[(&str, &str)]) -> Result<()>;

    /// Succeeds when the session does not exist.
    async fn kill_session(&self, id: &str) -> Result<()>;

    /// Whether the pane's process is still running.
    async fn has_session(&self, id: &str) -> bool;

    async fn list_sessions(&self) -> Result<Vec<LiveSession>>;

    /// Pid of the pane's root process (the login shell).
    async fn session_pid(&self, id: &str) -> Option<u32>;

    /// argv that bridges a terminal to the pane (the Iced app's hidden
    /// client, `ninox connect`, `ninox orchestrate`).
    async fn attach_args(&self, id: &str) -> Vec<String>;

    /// Lines of scrollback above the visible screen.
    async fn history_size(&self, id: &str) -> i64;

    /// ANSI capture of lines `start..=end` relative to the top of the
    /// visible screen (negative = scrollback), as `capture-pane -S -E -e`.
    async fn capture_history(&self, id: &str, start: i64, end: i64) -> Vec<u8>;

    /// Visible screen plus `scrollback` history lines above it, as plain
    /// text or with SGR styling.
    async fn read_screen(&self, id: &str, scrollback: usize, ansi: bool) -> Result<String>;

    /// Type `text` at the agent's prompt and submit it, verifying it left
    /// the input box (see `crate::tmux::send_keys`).
    async fn send_keys(&self, id: &str, text: &str) -> Result<()>;

    /// Best-effort idle-wake nudge (see `crate::tmux::wake_idle_session`).
    async fn wake_idle_session(&self, id: &str) -> Result<()>;

    /// Wait until the pane shows an agent input prompt; `false` on timeout.
    async fn wait_for_input_prompt(&self, id: &str, timeout: Duration) -> bool;

    /// Write raw bytes to the pane as input.
    async fn write_input(&self, id: &str, bytes: &[u8]) -> Result<()>;

    /// Emit the pane's output as `Event::TerminalOutput` and route the
    /// engine's PTY writer for `session_id` to its input (browser WS route).
    async fn start_streaming(&self, engine: Arc<Engine>, session_id: SessionId, id: &str) -> Result<()>;

    /// The pane of this backend that owns the calling process, if any.
    async fn current_pane_identity(&self) -> Result<Option<PaneIdentity>>;

    /// Whether this backend can currently say a pane is absent. A host-based
    /// backend that is unreachable can't.
    async fn answering(&self) -> bool {
        true
    }

    /// Get the backend into a state where it can answer (start its host).
    /// `configured` is whether new sessions go to this backend.
    async fn ensure_answering(&self, _configured: bool) -> Result<()> {
        Ok(())
    }
}

/// Backend selection over a tmux backend and an optional ptyd backend.
pub struct Runtime {
    tmux: Arc<dyn SessionBackend>,
    ptyd: Option<Arc<dyn SessionBackend>>,
}

impl Runtime {
    pub fn new(tmux: Arc<dyn SessionBackend>, ptyd: Option<Arc<dyn SessionBackend>>) -> Self {
        Self { tmux, ptyd }
    }

    /// The process-wide runtime: tmux, plus ptyd wherever it is allowed.
    pub fn current() -> Self {
        let ptyd = ptyd_allowed().then(|| Arc::new(PtydBackend::from_env()) as Arc<dyn SessionBackend>);
        Self::new(Arc::new(TmuxBackend), ptyd)
    }

    /// Backend for a session that may already exist.
    pub async fn for_session(&self, id: &str) -> Arc<dyn SessionBackend> {
        if let Some(ptyd) = &self.ptyd {
            if ptyd.knows(id).await {
                return ptyd.clone();
            }
        }
        self.tmux.clone()
    }

    /// Backend that should host a new session when `backend` is configured.
    pub fn for_new(&self, backend: Backend) -> Arc<dyn SessionBackend> {
        match (backend, &self.ptyd) {
            (Backend::Ptyd, Some(ptyd)) => ptyd.clone(),
            _ => self.tmux.clone(),
        }
    }

    pub async fn kill_session(&self, id: &str) -> Result<()> {
        if let Some(ptyd) = &self.ptyd {
            if ptyd.knows(id).await {
                ptyd.kill_session(id).await?;
                // A legacy tmux pane can share the id (respawned onto ptyd
                // after an upgrade); tmux may not even be installed, so this
                // half is best-effort.
                if let Err(e) = self.tmux.kill_session(id).await {
                    tracing::debug!("tmux kill of {id} after ptyd kill (ignored): {e}");
                }
                return Ok(());
            }
        }
        self.tmux.kill_session(id).await
    }

    /// Live panes across both backends; ptyd wins an id present in both.
    pub async fn list_sessions(&self) -> Result<Vec<LiveSession>> {
        let mut out = match &self.ptyd {
            Some(ptyd) => ptyd.list_sessions().await.unwrap_or_else(|e| {
                tracing::warn!("ptyd list failed (ignored): {e}");
                Vec::new()
            }),
            None => Vec::new(),
        };
        for s in self.tmux.list_sessions().await? {
            if !out.iter().any(|o| o.id == s.id) {
                out.push(s);
            }
        }
        Ok(out)
    }

    /// Before reconciling: start the ptyd host if it should be running. After
    /// a reboot it is not, and only a host that is up can say a pane is gone
    /// — a freshly started one holds no panes, so they all read `Dead`.
    pub async fn prepare_liveness(&self, configured: Backend) {
        if let Some(ptyd) = &self.ptyd {
            if let Err(e) = ptyd.ensure_answering(configured == Backend::Ptyd).await {
                tracing::warn!("ptyd host unavailable for reconciliation: {e}");
            }
        }
    }

    pub async fn liveness(&self, id: &str) -> Liveness {
        if let Some(ptyd) = &self.ptyd {
            if ptyd.knows(id).await {
                return if ptyd.has_session(id).await { Liveness::Live } else { Liveness::Dead };
            }
        }
        if self.tmux.has_session(id).await {
            return Liveness::Live;
        }
        match &self.ptyd {
            // Asked after the lookups, so a host that dropped out mid-check
            // also reads as unknown.
            Some(ptyd) if !ptyd.answering().await => Liveness::Unknown,
            _ => Liveness::Dead,
        }
    }

    pub async fn current_pane_identity(&self) -> Result<Option<PaneIdentity>> {
        if let Some(ptyd) = &self.ptyd {
            if let Some(identity) = ptyd.current_pane_identity().await? {
                return Ok(Some(identity));
            }
        }
        self.tmux.current_pane_identity().await
    }
}

/// Prefix of the TUI's viewer panes: ptyd panes running `tmux attach` so a
/// tmux-backed session can be composited like a ptyd one. They are TUI
/// plumbing, never sessions — every session-facing path skips them.
pub const VIEWER_PANE_PREFIX: &str = "tmux-view:";

pub fn is_viewer_pane(id: &str) -> bool {
    id.starts_with(VIEWER_PANE_PREFIX)
}

/// `tmux-view:<owner pid>:<session>`. The owner pid lets a TUI reap the
/// viewers a crashed TUI left behind without touching those of another TUI
/// that is still running.
pub fn viewer_pane_id(owner_pid: u32, session: &str) -> String {
    format!("{VIEWER_PANE_PREFIX}{owner_pid}:{session}")
}

/// `(owner pid, session id)` of a viewer pane id.
pub fn parse_viewer_pane(id: &str) -> Option<(u32, &str)> {
    let (pid, session) = id.strip_prefix(VIEWER_PANE_PREFIX)?.split_once(':')?;
    Some((pid.parse().ok()?, session)).filter(|(_, s)| !s.is_empty())
}

/// Whether this process may use ptyd at all (see the module docs).
pub fn ptyd_allowed() -> bool {
    ptyd_allowed_for(
        crate::tmux::is_test_binary(),
        std::env::var_os(ninox_ptyd::SOCKET_ENV).map(PathBuf::from).as_deref(),
        &std::env::temp_dir(),
    )
}

fn ptyd_allowed_for(test_binary: bool, socket_override: Option<&Path>, temp_dir: &Path) -> bool {
    !test_binary || socket_override.is_some_and(|socket| socket.starts_with(temp_dir))
}

/// `backend`, unless ptyd is disallowed in this process.
pub fn effective_backend(backend: Backend) -> Backend {
    match backend {
        Backend::Ptyd if !ptyd_allowed() => Backend::Tmux,
        other => other,
    }
}

/// `[runtime] backend` from the live config, as [`effective_backend`].
pub fn configured_backend() -> Backend {
    effective_backend(crate::config::AppConfig::load().unwrap_or_default().runtime.backend)
}

/// Start the ptyd host from `ninox_bin` unless one already answers.
pub async fn ensure_ptyd_host(ninox_bin: &Path) -> Result<()> {
    anyhow::ensure!(ptyd_allowed(), "ptyd is disabled in test binaries without an isolated socket");
    let backend = PtydBackend::new(ninox_ptyd::socket_path(), ninox_bin.to_string_lossy().into_owned());
    backend.ensure_host().await.map(|_| ())
}

/// The root pid of `id`'s pane when ptyd hosts it — the join key for
/// matching Claude Code's session registry when there is no tmux name.
pub async fn ptyd_pane_pid(id: &str) -> Option<u32> {
    let backend = Runtime::current().for_session(id).await;
    if backend.kind() != Backend::Ptyd {
        return None;
    }
    backend.session_pid(id).await
}

/// Create a session on `backend` (normally `config.runtime.backend`).
pub async fn create_session(backend: Backend, id: &str, workspace: &str, cmd: &str, env: &[(&str, &str)]) -> Result<()> {
    Runtime::current().for_new(backend).create_session(id, workspace, cmd, env).await
}

pub async fn kill_session(id: &str) -> Result<()> {
    Runtime::current().kill_session(id).await
}

/// See [`Runtime::prepare_liveness`].
pub async fn prepare_liveness() {
    Runtime::current().prepare_liveness(configured_backend()).await
}

pub async fn liveness(id: &str) -> Liveness {
    Runtime::current().liveness(id).await
}

pub async fn has_session(id: &str) -> bool {
    Runtime::current().for_session(id).await.has_session(id).await
}

pub async fn list_sessions() -> Result<Vec<LiveSession>> {
    Runtime::current().list_sessions().await
}

pub async fn session_pid(id: &str) -> Option<u32> {
    Runtime::current().for_session(id).await.session_pid(id).await
}

pub async fn attach_args(id: &str) -> Vec<String> {
    Runtime::current().for_session(id).await.attach_args(id).await
}

/// `attach_args` for a bridge embedded in another program (the Iced app),
/// which closes it by killing the child: the detach chord must not apply.
pub fn embedded_attach_args(mut argv: Vec<String>) -> Vec<String> {
    let is_pane_attach = argv.get(1).is_some_and(|a| a == "pane") && argv.get(2).is_some_and(|a| a == "attach");
    if is_pane_attach && !argv.iter().any(|a| a == "--no-detach") {
        argv.insert(3, "--no-detach".into());
    }
    argv
}

/// `attach_args` for a TUI viewer pane: a tmux client flagged
/// `ignore-size`, so while any other client (the desktop app, `ninox
/// connect`) is attached the viewer never resizes the agent's window. The
/// caller sizes the viewer to the window ([`viewer_size`]) so a viewer that
/// is the only client leaves it alone too.
pub fn viewer_attach_args(mut argv: Vec<String>) -> Vec<String> {
    if argv.first().is_some_and(|a| a == "tmux") && argv.iter().any(|a| a == "attach-session") {
        argv.extend(["-f".to_string(), "ignore-size".to_string()]);
    }
    argv
}

/// The size a viewer pane for tmux session `id` should have: its window's.
pub async fn viewer_size(id: &str) -> Option<(u16, u16)> {
    crate::tmux::window_size(id).await
}

pub async fn history_size(id: &str) -> i64 {
    Runtime::current().for_session(id).await.history_size(id).await
}

pub async fn capture_history(id: &str, start: i64, end: i64) -> Vec<u8> {
    Runtime::current().for_session(id).await.capture_history(id, start, end).await
}

/// `ninox read`: the visible screen, or the last `lines` lines including
/// scrollback, trailing blank lines trimmed.
pub async fn read_screen(id: &str, lines: Option<usize>, ansi: bool) -> Result<String> {
    let backend = Runtime::current().for_session(id).await;
    anyhow::ensure!(backend.has_session(id).await, "no live session named {id}");
    let raw = backend.read_screen(id, lines.unwrap_or(0), ansi).await?;
    Ok(tail_lines(&raw, lines))
}

pub async fn send_keys(id: &str, text: &str) -> Result<()> {
    Runtime::current().for_session(id).await.send_keys(id, text).await
}

pub async fn wake_idle_session(id: &str) -> Result<()> {
    Runtime::current().for_session(id).await.wake_idle_session(id).await
}

pub async fn wait_for_input_prompt(id: &str, timeout: Duration) -> bool {
    Runtime::current().for_session(id).await.wait_for_input_prompt(id, timeout).await
}

pub async fn write_input(id: &str, bytes: &[u8]) -> Result<()> {
    Runtime::current().for_session(id).await.write_input(id, bytes).await
}

pub async fn start_streaming(engine: Arc<Engine>, session_id: SessionId, id: &str) -> Result<()> {
    Runtime::current().for_session(id).await.start_streaming(engine, session_id, id).await
}

/// The private pane (ptyd or tmux) that owns the calling process.
pub async fn current_private_pane_identity() -> Result<Option<PaneIdentity>> {
    Runtime::current().current_pane_identity().await
}

/// Whether the calling process descends from `root_pid` — backend-agnostic,
/// since both runtimes report the pane's root pid.
pub fn caller_descends_from(root_pid: u32) -> bool {
    crate::tmux::caller_descends_from(root_pid)
}

/// Drop trailing blank lines, then keep the last `lines` (all when `None`).
fn tail_lines(raw: &str, lines: Option<usize>) -> String {
    let all: Vec<&str> = raw.lines().collect();
    let end = all.iter().rposition(|l| !l.trim().is_empty()).map_or(0, |i| i + 1);
    let start = lines.map_or(0, |n| end.saturating_sub(n));
    all[start..end].join("\n")
}

#[cfg(test)]
pub(crate) mod mock;

#[cfg(test)]
mod tests;
