//! [`SessionBackend`] over the `ninox-ptyd` PTY host.
//!
//! Pane id == ninox session id. Each operation opens its own client
//! connection (a local connect + `Hello`), so nothing here holds state that
//! an engine restart could lose.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use ninox_ptyd::{Event as PtydEvent, PaneInfo, PtydClient, SpawnSpec, SubscribeMode};

use super::prompt::{
    message_stuck_at_prompt, nudge_still_alone_at_prompt, prompt_line_content, IDLE_WAKE_NUDGE,
    PROMPT_POLL_DELAY_MS, SEND_SUBMIT_DELAY_MS, SEND_VERIFY_ATTEMPTS, SEND_VERIFY_DELAY_MS,
};
use super::{Backend, LiveSession, PaneIdentity, SessionBackend};
use crate::events::{Engine, Event};
use crate::types::SessionId;

const CLIENT_NAME: &str = "ninox-engine";
/// Same fixed size tmux panes get (`new-session -x 140 -y 50`); attached
/// clients resize from there.
const DEFAULT_COLS: u16 = 140;
const DEFAULT_ROWS: u16 = 50;
/// How long `create_session` waits for a freshly spawned host to answer.
const HOST_START_TIMEOUT: Duration = Duration::from_secs(10);
/// Matches tmux's `history-limit 100000`, the most `history_size` can report.
const HISTORY_LIMIT: usize = 100_000;

/// Set in every ptyd pane so `ninox` invoked inside it can find its pane.
pub const PANE_ID_ENV: &str = "NINOX_PANE_ID";

/// Removed from the host's inherited environment before a pane starts, so a
/// pane never believes it is a child of whatever agent started the host. The
/// `NINOX_*` session identity and config/brain/data-location vars (and
/// `CLAUDE_CONFIG_DIR`) are included because the host inherits them from
/// whichever pane or test first spawned it, and would otherwise hand them
/// to every pane; callers set their own values, which are applied after
/// removal.
const ENV_REMOVE: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CONFIG_DIR",
    "CODEX_THREAD_ID",
    "TMUX",
    "TMUX_PANE",
    "NINOX_SESSION",
    "NINOX_ORCHESTRATOR_ID",
    "NINOX_CALLER_TYPE",
    "NINOX_CONFIG",
    "NINOX_BRAIN",
    "NINOX_CLAUDE_PROJECTS_DIR",
    PANE_ID_ENV,
];

pub struct PtydBackend {
    socket:    PathBuf,
    ninox_bin: String,
}

impl PtydBackend {
    pub fn new(socket: PathBuf, ninox_bin: String) -> Self {
        Self { socket, ninox_bin }
    }

    /// The host at `ninox_ptyd::socket_path()`, spawned (if needed) from the
    /// running binary.
    pub fn from_env() -> Self {
        let ninox_bin = std::env::current_exe()
            .ok()
            .and_then(|p| p.to_str().map(str::to_string))
            .unwrap_or_else(|| "ninox".to_string());
        Self::new(ninox_ptyd::socket_path(), ninox_bin)
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    fn host_argv(&self) -> Vec<String> {
        vec![self.ninox_bin.clone(), "ptyd".to_string()]
    }

    /// Next to the socket: `<data>/ninox/ptyd.log` for the default socket,
    /// inside the isolated directory for an overridden one.
    fn log_path(&self) -> PathBuf {
        self.socket.parent().unwrap_or_else(|| Path::new(".")).join("ptyd.log")
    }

    /// `None` when no host is listening — every read-side operation treats
    /// that as "pane unknown" rather than an error.
    async fn client(&self) -> Option<PtydClient> {
        if !self.socket.exists() {
            return None;
        }
        match PtydClient::connect(&self.socket, CLIENT_NAME).await {
            Ok(client) => Some(client),
            Err(e) => {
                tracing::debug!("ptyd not reachable at {}: {e}", self.socket.display());
                None
            }
        }
    }

    async fn require_client(&self) -> Result<PtydClient> {
        PtydClient::connect(&self.socket, CLIENT_NAME)
            .await
            .with_context(|| format!("ptyd not reachable at {}", self.socket.display()))
    }

    /// Connect, starting the host first if nothing is listening.
    pub async fn ensure_host(&self) -> Result<PtydClient> {
        if let Some(parent) = self.socket.parent() {
            std::fs::create_dir_all(parent)?;
        }
        PtydClient::connect_or_spawn(
            &self.socket,
            CLIENT_NAME,
            &self.host_argv(),
            Some(self.log_path()),
            HOST_START_TIMEOUT,
        )
        .await
    }

    /// Viewer panes are never sessions, so `knows`/`has_session`/
    /// `session_pid` (all via here) never claim one.
    async fn info(&self, id: &str) -> Option<PaneInfo> {
        if super::is_viewer_pane(id) {
            return None;
        }
        let mut client = self.client().await?;
        match client.info(id).await {
            Ok(info) => info,
            Err(e) => {
                tracing::warn!("ptyd info {id}: {e}");
                None
            }
        }
    }

    async fn pane(&self, id: &str) -> Result<PtydPane> {
        Ok(PtydPane { client: self.require_client().await?, pane: id.to_string() })
    }
}

/// The `SpawnSpec` for a session: argv and env as the tmux path builds them
/// (`$SHELL -l -c <cmd>`, caller env), plus the pane's own identity vars.
pub(crate) fn spawn_spec(
    id:        &str,
    workspace: &str,
    cmd:       &str,
    env:       &[(&str, &str)],
    socket:    &Path,
    shell:     &str,
) -> Result<SpawnSpec> {
    let mut pairs: Vec<(String, String)> = Vec::with_capacity(env.len() + 4);
    for (k, v) in env {
        anyhow::ensure!(!k.contains('='), "env key must not contain '=': {k}");
        pairs.push((k.to_string(), v.to_string()));
    }
    let socket = socket.to_string_lossy().into_owned();
    for (k, v) in [
        (PANE_ID_ENV, id),
        (ninox_ptyd::SOCKET_ENV, socket.as_str()),
        ("TERM", "xterm-256color"),
        ("COLORTERM", "truecolor"),
    ] {
        pairs.retain(|(existing, _)| existing != k);
        pairs.push((k.to_string(), v.to_string()));
    }
    Ok(SpawnSpec {
        pane: id.to_string(),
        argv: vec![shell.to_string(), "-l".into(), "-c".into(), cmd.to_string()],
        cwd: workspace.to_string(),
        env: pairs,
        env_remove: ENV_REMOVE.iter().map(|s| s.to_string()).collect(),
        cols: DEFAULT_COLS,
        rows: DEFAULT_ROWS,
    })
}

/// The host's live panes as sessions, viewer panes excluded.
pub(crate) fn live_sessions(panes: Vec<PaneInfo>) -> Vec<LiveSession> {
    panes
        .into_iter()
        .filter(|p| p.alive && !super::is_viewer_pane(&p.pane))
        .map(|p| LiveSession {
            id:         p.pane,
            created_ms: p.created_ms as i64,
            pid:        Some(p.pid),
            backend:    Backend::Ptyd,
        })
        .collect()
}

/// A TUI viewer pane running `argv` (a `tmux attach`). `TMUX`/`TMUX_PANE`
/// are among the removed vars: a TUI running inside tmux would otherwise
/// make the attach refuse to nest.
pub fn viewer_spawn_spec(pane: &str, argv: Vec<String>, cols: u16, rows: u16) -> SpawnSpec {
    let cwd = dirs::home_dir().filter(|d| d.is_dir()).unwrap_or_else(|| PathBuf::from("/"));
    SpawnSpec {
        pane: pane.to_string(),
        argv,
        cwd: cwd.to_string_lossy().into_owned(),
        env: vec![("TERM".into(), "xterm-256color".into()), ("COLORTERM".into(), "truecolor".into())],
        env_remove: ENV_REMOVE.iter().map(|s| s.to_string()).collect(),
        cols,
        rows,
    }
}

/// The caller's identity in pane `pane` (from `NINOX_PANE_ID`), trusted
/// only when the host confirms the pane is live and the caller actually
/// descends from its root process — the env var alone is forgeable.
/// `descends` must not count descent *through the host*: if the host
/// itself ran under that pane, every other pane would descend from it too
/// (see [`reaches_before`]).
pub(crate) fn identity_for(
    pane:     &str,
    info:     Option<&PaneInfo>,
    epoch_ms: u64,
    descends: impl Fn(u32) -> bool,
) -> Option<PaneIdentity> {
    let info = info.filter(|i| i.alive && i.pane == pane)?;
    descends(info.pid).then(|| PaneIdentity {
        physical_tmux_name: pane.to_string(),
        pane_id:            format!("ptyd:{pane}"),
        pane_pid:           info.pid,
        pane_created_at:    info.created_ms as i64,
        server_epoch:       epoch_ms.to_string(),
    })
}

/// Whether walking up from `pid` reaches `root` before `host_pid`. A pane
/// is a child of the host, so a caller that only reaches `root` by going
/// through the host is in a different pane of a host that was started
/// from inside `root`'s pane.
pub(crate) fn reaches_before(parents: &std::collections::HashMap<u32, u32>, pid: u32, root: u32, host_pid: u32) -> bool {
    use super::process::descends_in;
    // The path up from `pid` to `root` is unique, and passes the host iff
    // both halves of the path meet there.
    descends_in(parents, pid, root) && !(descends_in(parents, pid, host_pid) && descends_in(parents, host_pid, root))
}

/// The pane-level I/O the prompt-aware delivery helpers need, so they can
/// be exercised against a scripted screen.
#[async_trait]
pub(crate) trait PaneIo: Send {
    /// Visible screen as plain text; empty when it can't be read.
    async fn visible_text(&mut self) -> String;
    /// Type `text` as user input (bracketed paste when the pane wants it).
    async fn type_text(&mut self, text: &str) -> Result<()>;
    async fn write(&mut self, bytes: &[u8]) -> Result<()>;
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Timing {
    pub submit_delay: Duration,
    pub verify_delay: Duration,
    pub poll_delay:   Duration,
}

impl Timing {
    pub const DEFAULT: Timing = Timing {
        submit_delay: Duration::from_millis(SEND_SUBMIT_DELAY_MS),
        verify_delay: Duration::from_millis(SEND_VERIFY_DELAY_MS),
        poll_delay:   Duration::from_millis(PROMPT_POLL_DELAY_MS),
    };
}

const ENTER: &[u8] = b"\r";

/// `crate::tmux::send_keys`'s algorithm: type, pause, Enter, then re-send
/// Enter while the message is visibly stuck at the prompt.
pub(crate) async fn send_verified(io: &mut dyn PaneIo, id: &str, text: &str, t: Timing) -> Result<()> {
    io.type_text(text).await?;
    tokio::time::sleep(t.submit_delay).await;
    io.write(ENTER).await?;
    for attempt in 0..=SEND_VERIFY_ATTEMPTS {
        tokio::time::sleep(t.verify_delay).await;
        if !message_stuck_at_prompt(&io.visible_text().await, text) {
            return Ok(());
        }
        if attempt < SEND_VERIFY_ATTEMPTS {
            io.write(ENTER).await?;
        }
    }
    anyhow::bail!(
        "message to {id} is still unsubmitted at its input prompt \
         after {SEND_VERIFY_ATTEMPTS} Enter retries"
    )
}

/// `crate::tmux::wake_idle_session`'s algorithm over a screen snapshot.
pub(crate) async fn wake_idle(io: &mut dyn PaneIo, t: Timing) -> Result<()> {
    match prompt_line_content(&io.visible_text().await) {
        Some("") => {}
        Some(IDLE_WAKE_NUDGE) => return io.write(ENTER).await,
        _ => return Ok(()),
    }
    io.write(IDLE_WAKE_NUDGE.as_bytes()).await?;
    tokio::time::sleep(t.submit_delay).await;
    if !nudge_still_alone_at_prompt(&io.visible_text().await) {
        return Ok(());
    }
    io.write(ENTER).await
}

pub(crate) async fn wait_prompt(io: &mut dyn PaneIo, timeout: Duration, t: Timing) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if prompt_line_content(&io.visible_text().await).is_some() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(t.poll_delay).await;
    }
}

struct PtydPane {
    client: PtydClient,
    pane:   String,
}

#[async_trait]
impl PaneIo for PtydPane {
    async fn visible_text(&mut self) -> String {
        self.client
            .screen(&self.pane, 0)
            .await
            .map(|s| s.to_plain_text())
            .unwrap_or_default()
    }
    async fn type_text(&mut self, text: &str) -> Result<()> {
        self.client.submit(&self.pane, text, false).await
    }
    async fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.client.write(&self.pane, bytes).await
    }
}

#[async_trait]
impl SessionBackend for PtydBackend {
    fn kind(&self) -> Backend {
        Backend::Ptyd
    }

    async fn knows(&self, id: &str) -> bool {
        self.info(id).await.is_some()
    }

    async fn create_session(&self, id: &str, workspace: &str, cmd: &str, env: &[(&str, &str)]) -> Result<()> {
        anyhow::ensure!(
            Path::new(workspace).is_dir(),
            "workspace directory does not exist: {workspace}"
        );
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        let spec = spawn_spec(id, workspace, cmd, env, &self.socket, &shell)?;
        let mut client = self.ensure_host().await?;
        client.spawn(spec).await.map(|_| ())
    }

    async fn kill_session(&self, id: &str) -> Result<()> {
        match self.client().await {
            Some(mut client) => client.kill(id).await,
            None => Ok(()),
        }
    }

    async fn has_session(&self, id: &str) -> bool {
        self.info(id).await.is_some_and(|i| i.alive)
    }

    async fn list_sessions(&self) -> Result<Vec<LiveSession>> {
        let Some(mut client) = self.client().await else { return Ok(Vec::new()) };
        Ok(live_sessions(client.list().await?))
    }

    async fn session_pid(&self, id: &str) -> Option<u32> {
        self.info(id).await.filter(|i| i.alive).map(|i| i.pid)
    }

    async fn attach_args(&self, id: &str) -> Vec<String> {
        vec![self.ninox_bin.clone(), "pane".into(), "attach".into(), id.to_string()]
    }

    async fn history_size(&self, id: &str) -> i64 {
        let Some(mut client) = self.client().await else { return 0 };
        client.info(id).await.ok().flatten().map_or(0, |p| p.history_size.min(HISTORY_LIMIT) as i64)
    }

    async fn capture_history(&self, id: &str, start: i64, end: i64) -> Vec<u8> {
        let Some(mut client) = self.client().await else { return Vec::new() };
        client.history(id, start, end).await.unwrap_or_default()
    }

    async fn read_screen(&self, id: &str, scrollback: usize, ansi: bool) -> Result<String> {
        let mut client = self.require_client().await?;
        if ansi {
            let rows = client.info(id).await?.map_or(DEFAULT_ROWS, |i| i.rows);
            let bytes = client.history(id, -(scrollback as i64), i64::from(rows) - 1).await?;
            return Ok(String::from_utf8_lossy(&bytes).into_owned());
        }
        Ok(client.screen(id, scrollback).await?.to_plain_text())
    }

    async fn send_keys(&self, id: &str, text: &str) -> Result<()> {
        send_verified(&mut self.pane(id).await?, id, text, Timing::DEFAULT).await
    }

    async fn wake_idle_session(&self, id: &str) -> Result<()> {
        wake_idle(&mut self.pane(id).await?, Timing::DEFAULT).await
    }

    async fn wait_for_input_prompt(&self, id: &str, timeout: Duration) -> bool {
        match self.pane(id).await {
            Ok(mut pane) => wait_prompt(&mut pane, timeout, Timing::DEFAULT).await,
            Err(_) => false,
        }
    }

    async fn write_input(&self, id: &str, bytes: &[u8]) -> Result<()> {
        self.require_client().await?.write(id, bytes).await
    }

    async fn start_streaming(&self, engine: Arc<Engine>, session_id: SessionId, id: &str) -> Result<()> {
        let mut cancel_rx = engine.register_stream(session_id.clone()).await;
        let mut sub = PtydClient::subscribe(&self.socket, id, SubscribeMode::Raw).await?;

        let engine_out = engine.clone();
        let sid_out = session_id.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = &mut cancel_rx => break,
                    next = sub.next() => match next {
                        Ok(Some((PtydEvent::Output { .. }, bytes))) => {
                            engine_out.emit(Event::TerminalOutput { session_id: sid_out.clone(), bytes });
                        }
                        Ok(Some((PtydEvent::Exited { .. }, _))) | Ok(None) => break,
                        Ok(Some(_)) => {}
                        Err(e) => {
                            tracing::warn!("ptyd stream {sid_out}: {e}");
                            break;
                        }
                    },
                }
            }
            tracing::info!("PTY stream ended for {sid_out}");
        });

        let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        engine.register_pty_writer(session_id, input_tx).await;
        let socket = self.socket.clone();
        let pane = id.to_string();
        tokio::spawn(async move {
            let mut client: Option<PtydClient> = None;
            while let Some(bytes) = input_rx.recv().await {
                // One reconnect per write: the host may have restarted
                // (re-adopting panes) since the last keystroke.
                for _ in 0..2 {
                    if client.is_none() {
                        client = PtydClient::connect(&socket, CLIENT_NAME).await.ok();
                    }
                    let Some(c) = client.as_mut() else { break };
                    match c.write(&pane, &bytes).await {
                        Ok(()) => break,
                        Err(e) => {
                            tracing::debug!("ptyd write {pane}: {e}");
                            client = None;
                        }
                    }
                }
            }
        });
        Ok(())
    }

    /// No socket means no host has run here, so no ptyd pane exists. A
    /// socket nobody answers on is either stale or a host mid-takeover.
    async fn answering(&self) -> bool {
        !self.socket.exists() || self.client().await.is_some()
    }

    /// Starts the host when ptyd is configured or a socket suggests one
    /// should exist; `connect_or_spawn` never starts a second beside a host
    /// that answers, and waits out one that is slow to come up.
    async fn ensure_answering(&self, configured: bool) -> Result<()> {
        if configured || self.socket.exists() {
            self.ensure_host().await?;
        }
        Ok(())
    }

    async fn current_pane_identity(&self) -> Result<Option<PaneIdentity>> {
        let Some(pane) = std::env::var(PANE_ID_ENV).ok().filter(|s| !s.is_empty()) else {
            return Ok(None);
        };
        let Some(mut client) = self.client().await else { return Ok(None) };
        let (host_pid, epoch_ms) = client.host_identity();
        let info = client.info(&pane).await?;
        let descends = |root| reaches_before(&super::process::parent_map(), std::process::id(), root, host_pid);
        Ok(identity_for(&pane, info.as_ref(), epoch_ms, descends))
    }
}

#[cfg(test)]
mod tests;
