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

pub mod acp_backend;
pub(crate) mod process;
pub(crate) mod prompt;
pub mod ptyd_backend;
pub mod remote_backend;
pub mod tmux_backend;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::events::Engine;
use crate::types::{CIStatus, Comment, Session, SessionId, SessionKindData};

pub use acp_backend::{AcpBackend, DetachedSendOutcome, SEND_ACK_WINDOW};
pub use ptyd_backend::PtydBackend;
pub use remote_backend::RemoteBackend;
pub use tmux_backend::TmuxBackend;

/// A session's execution backend. `Tmux`/`Ptyd` are also the two choices for
/// `[runtime] backend` (which runtime hosts newly created *local* sessions);
/// `Remote` is never user-configurable there — it's selected per session via
/// `Session::machine_id`, never written to config (see
/// [`Runtime::for_existing`]). [`Backend::ALL`] stays the two config-selectable
/// values rather than every variant, since the settings UI iterates it to
/// build the `[runtime] backend` picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    /// `ninox-ptyd`: Ninox's own PTY host. Opt-in until the desktop app has
    /// been proven on it.
    Ptyd,
    /// The private `-L ninox` tmux server.
    #[default]
    Tmux,
    /// A direct Agent Client Protocol connection to a harness with ACP
    /// support, native (`opencode acp`) or via an adapter subprocess (Codex's
    /// `codex-acp`, Pi's `pi-acp`) — see
    /// `docs/superpowers/specs/2026-10-21-cross-harness-messaging-design.md`.
    /// Deliberately **not** in [`Backend::ALL`]: unlike Tmux/Ptyd this isn't
    /// a general "where do new sessions go" choice — it only applies to
    /// ACP-capable harnesses, and `Runtime` (the tmux/ptyd dispatcher used
    /// by session creation and `[runtime] backend`) doesn't route to it yet.
    /// That per-harness spawn routing and `ninox-server` connection
    /// ownership is the follow-on this backend unblocks, not part of it.
    Acp,
    /// An SSH-connected remote machine (`Session::machine_id: Some(_)`) —
    /// see `docs/superpowers/specs/2026-10-05-remote-sessions-design.md`.
    /// Phase 1 only: [`RemoteBackend`] has no working transport yet.
    Remote,
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
            Backend::Acp => "Sessions run as a direct Agent Client Protocol connection to the harness's own \
                 process or adapter (OpenCode natively via `opencode acp`; Codex and Pi via their own adapter \
                 subprocess) — no pty, no tmux. Only harnesses with confirmed ACP support use this tier; \
                 everything else stays on tmux/ptyd.",
            Backend::Remote => "Sessions on an SSH-connected remote machine (`ninox machine add`). Not yet \
                 functional — the transport that would reach a remote host's own ptyd doesn't exist yet.",
        }
    }
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Backend::Tmux => "tmux",
            Backend::Ptyd => "ptyd (no tmux)",
            Backend::Acp => "acp (no pty)",
            Backend::Remote => "remote (ssh)",
        })
    }
}

/// One entry of an ACP-backed session's structured transcript — the
/// non-pty counterpart to a tmux/ptyd pane's scrollback. Covers what an ACP
/// `session/update` notification can carry; see
/// [`SessionBackend::transcript_history`] and `Event::SessionTranscript`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum TranscriptEntry {
    UserMessage(String),
    AgentText(String),
    ToolCall { id: String, name: String, status: ToolCallStatus, detail: String },
    TurnStarted,
    TurnEnded,
}

/// An ACP tool call's lifecycle state, straight from its `status` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
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

    /// Structured transcript entries for backends with no raw terminal
    /// (e.g. [`AcpBackend`]) — the non-pty counterpart to `capture_history`.
    /// Pty-based backends have nothing structured to return.
    async fn transcript_history(&self, _id: &str) -> Vec<TranscriptEntry> {
        Vec::new()
    }
}

/// How a session kind wants its own PR lifecycle presented to the agent —
/// the counterpart to [`SessionBackend`] for session-kind behavior rather
/// than session execution. Each `SessionBackend` implementor (tmux, ptyd, a
/// future remote kind) provides one, colocated with its backend impl. Every
/// method defaults to today's one-size-fits-all wording (see
/// `crate::lifecycle::reactions`), so a kind that wants no different text
/// needs zero overrides.
///
/// Known limitation (see [`reaction_behavior_for`]): resolution by default
/// picks the *globally configured* backend, not the backend actually hosting
/// a given session — fine for every PR-lifecycle-wording method, since all
/// of them stay all-default today, but already wrong for
/// [`Self::pid_is_local`], which [`RemoteBackend`] overrides — that one
/// method must go through [`reaction_behavior_for_session`] instead, which
/// resolves by the session's own `machine_id`.
pub trait SessionReactions: Send + Sync {
    /// CI is failing on the session's own PR.
    fn ci_failing(&self, _session: &Session, ci: &CIStatus, failing_names: &[String]) -> String {
        let mut msg = format!(
            "[Ninox] CI is failing on your PR ({}/{} checks). Please fix the following:\n",
            ci.failing, ci.total
        );
        for name in failing_names {
            msg.push_str(&format!("  - {name}\n"));
        }
        msg.push_str("\nRun the failing checks locally, fix the issues, and push your changes.");
        msg
    }

    /// The session's own PR is green and mergeable.
    fn ready_to_merge(&self, _session: &Session, ci: &CIStatus) -> String {
        format!(
            "[Ninox] Your PR is ready to merge — all {} checks passing and mergeable.",
            ci.total,
        )
    }

    /// New CHANGES_REQUESTED review activity on the session's own PR.
    fn review_comments(&self, _session: &Session, comments: &[Comment]) -> String {
        let mut msg = format!(
            "[Ninox] Your PR has {} new review comment{}. Please address them:\n",
            comments.len(),
            if comments.len() == 1 { "" } else { "s" }
        );
        for c in comments {
            let location = match (&c.path, c.line) {
                (Some(p), Some(l)) => format!("{p}:{l}"),
                (Some(p), None)    => p.clone(),
                _                  => "general".to_string(),
            };
            msg.push_str(&format!("\n[{}] {}: {}\n", location, c.author, c.body));
        }
        msg.push_str("\nAddress each comment, push your changes, and respond to the reviewer.");
        msg
    }

    /// The worker's tracked PR merged and Ninox cleaned it up (`reap_on_merge`).
    fn worker_done(&self, worker: &Session, pr_number: u64) -> String {
        format!("[Ninox] Worker `{id}`'s PR #{pr_number} merged.", id = worker.id)
    }

    /// The worker's tracked PR merged but it was not reaped, so the session
    /// and its worktree still exist and the worker has been told to finish
    /// itself with `ninox complete`.
    fn worker_done_kept_alive(&self, worker: &Session, pr_number: u64) -> String {
        format!(
            "{merged} The worker session is still alive with its worktree intact — \
             send it post-merge validation with `ninox send {id} \"...\"` if useful. \
             The worker has been told to finish itself with `ninox complete`. Reap it yourself \
             (`ninox reap {id} --force`) only if it doesn't, or if you don't need it.",
            merged = self.worker_done(worker, pr_number),
            id = worker.id,
        )
    }

    /// The worker session was purged by the retention sweep without ever
    /// going through merge detection.
    fn worker_retired(&self, worker: &Session) -> String {
        match worker.pr_number {
            Some(pr) => format!(
                "[Ninox] Worker `{id}` was cleaned up and its session is gone — \
                 its PR #{pr} was not detected as merged, so check its state.",
                id = worker.id,
            ),
            None => format!(
                "[Ninox] Worker `{id}` was cleaned up and its session is gone \
                 without ever opening a PR.",
                id = worker.id,
            ),
        }
    }

    /// Whether this session kind's process id lives on *this* machine, i.e.
    /// whether `Poller::poll_pids`'s local `kill(pid, 0)` check can judge its
    /// liveness. Defaults to `true` (every current kind's pid is local);
    /// [`RemoteBackend`] overrides it to `false` since a remote session's pid
    /// belongs to another machine entirely (resolved decision #3 of the
    /// remote-sessions design spec).
    fn pid_is_local(&self) -> bool {
        true
    }
}

/// The [`SessionReactions`] implementor for `backend` (falling back to tmux
/// when ptyd is disallowed in this process, same as [`effective_backend`]).
///
/// Takes a `Backend`, not a `&Session`, so it can only answer "what does the
/// *configured* backend do", not "what does *this session's* kind do" — the
/// two coincide for every method except [`SessionReactions::pid_is_local`],
/// including for a session left running on a backend the config no longer
/// points at. See [`reaction_behavior_for_session`] for the one case that
/// now needs genuine per-session resolution.
pub fn reaction_behavior_for(backend: Backend) -> Arc<dyn SessionReactions> {
    match effective_backend(backend) {
        Backend::Tmux => Arc::new(TmuxBackend),
        Backend::Ptyd => Arc::new(PtydBackend::from_env()),
        // Unreachable via `effective_backend` today (nothing routes new
        // sessions to `Backend::Acp` yet — see its doc comment), but a
        // hand-edited config could set `[runtime] backend = "acp"` and
        // reach this arm, so it must answer, not panic.
        Backend::Acp => Arc::new(AcpBackend::new()),
        // `[runtime] backend` never actually holds `Remote` (see the
        // `Backend` doc comment) — this arm exists only so the match stays
        // exhaustive if a config file is hand-edited to that value. There is
        // no `machine_id` to resolve in this position (see `Runtime::for_new`),
        // so this always stays unresolved, same as before this phase.
        Backend::Remote => Arc::new(RemoteBackend::unresolved()),
    }
}

/// [`SessionReactions`] for a *specific* session, keyed off its own
/// `machine_id` rather than the globally configured backend — unlike
/// [`reaction_behavior_for`], this answers "what does *this session's* kind
/// do". Needed because a remote-hosted session's pid lives on another
/// machine regardless of what `[runtime] backend` is configured to, so
/// `Poller::poll_pids` must resolve [`SessionReactions::pid_is_local`] by the
/// session's own kind, not the process-wide default. Doesn't need the full
/// machine-profile resolution [`resolve_remote_backend`] does (an unresolved
/// [`RemoteBackend`] already answers `pid_is_local` correctly, since that
/// method doesn't depend on which machine) — falls back to
/// [`reaction_behavior_for`]'s configured-backend resolution when
/// `machine_id` is absent.
///
/// This intentionally resolves less precisely than [`Runtime::for_existing`]
/// (which does call [`resolve_remote_backend`]) — correct only as long as
/// every [`SessionReactions`] method a remote session's kind might override
/// answers identically regardless of *which* machine it's on. A future
/// method that needs genuine per-machine behavior must resolve through
/// [`resolve_remote_backend`] here too, or the two machine_id-dispatch paths
/// will silently diverge.
pub fn reaction_behavior_for_session(session: &Session) -> Arc<dyn SessionReactions> {
    match session.kind_data() {
        SessionKindData::Remote { .. } => Arc::new(RemoteBackend::unresolved()),
        SessionKindData::Local => reaction_behavior_for(configured_backend()),
    }
}

/// Resolves `machine_id` against the live config's `remote_machines`
/// profiles, baking the matching enabled [`MachineProfile`] — and today's
/// simplifying same-port assumption (see `RemoteBackend`'s own docs) — into
/// a fresh [`RemoteBackend`]. Falls back to
/// [`RemoteBackend::unresolved`]'s same honest "not yet implemented"-style
/// behavior when nothing matches: either a `machine_id` left over after
/// `ninox machine remove`, or a profile the user disabled — `enabled: false`
/// exists precisely so a paused machine stays unreachable (see
/// `MachineProfile`'s own doc comment), so this must check it, not just find
/// by id.
fn resolve_remote_backend(machine_id: &str) -> RemoteBackend {
    let cfg = crate::config::AppConfig::load_or_default();
    match cfg.remote_machines.machines.into_iter().find(|m| m.id == machine_id && m.enabled) {
        Some(profile) => RemoteBackend::for_machine(profile, cfg.port),
        None => RemoteBackend::unresolved(),
    }
}

/// Backend selection over a tmux backend and an optional ptyd backend.
pub struct Runtime {
    tmux:   Arc<dyn SessionBackend>,
    ptyd:   Option<Arc<dyn SessionBackend>>,
    remote: Arc<dyn SessionBackend>,
}

impl Runtime {
    pub fn new(tmux: Arc<dyn SessionBackend>, ptyd: Option<Arc<dyn SessionBackend>>) -> Self {
        Self { tmux, ptyd, remote: Arc::new(RemoteBackend::unresolved()) }
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

    /// Backend for a session carrying `machine_id`: a [`RemoteBackend`]
    /// resolved against that machine's own `MachineProfile` when set (see
    /// [`resolve_remote_backend`]) — most methods still error "remote
    /// transport not yet implemented" per the remote-sessions spec's
    /// resolved decision #2, but `list_sessions` now actually reaches the
    /// machine over the SSH tunnel. Otherwise today's local tmux/ptyd
    /// dispatch via [`Self::for_session`]. This is the session-kind routing
    /// point, so it takes `kind` as a [`SessionKindData`] (the single source
    /// of truth for local-vs-remote decisions — see `Session::kind_data`)
    /// rather than a raw `machine_id`.
    pub async fn for_existing(&self, id: &str, kind: &SessionKindData) -> Arc<dyn SessionBackend> {
        match kind {
            SessionKindData::Remote { machine_id } => {
                let machine_id = machine_id.clone();
                let backend = tokio::task::spawn_blocking(crate::test_isolation::propagate(move || resolve_remote_backend(&machine_id)))
                    .await
                    .unwrap_or_else(|_| RemoteBackend::unresolved());
                Arc::new(backend)
            }
            SessionKindData::Local => self.for_session(id).await,
        }
    }

    /// Backend that should host a new session when `backend` is configured.
    pub fn for_new(&self, backend: Backend) -> Arc<dyn SessionBackend> {
        match (backend, &self.ptyd) {
            (Backend::Ptyd, Some(ptyd)) => ptyd.clone(),
            // `[runtime] backend` never legitimately holds `Remote` (see the
            // `Backend` doc comment) — routing to `self.remote` rather than
            // falling into the `_` arm below keeps this consistent with
            // `reaction_behavior_for`, so a hand-edited `backend = "remote"`
            // gets the honest "not yet implemented" error instead of
            // silently creating a local tmux session under the wrong name.
            (Backend::Remote, _) => self.remote.clone(),
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

/// The ptyd socket: `NINOX_PTYD_SOCKET`, else inside a scratch config's
/// directory (so a scratch instance never reaches the user's ptyd host),
/// else `ninox_ptyd::socket_path()`. A scratch path too long for a unix
/// socket falls back to one per scratch root under the temp dir.
pub fn ptyd_socket_path() -> PathBuf {
    if std::env::var_os(ninox_ptyd::SOCKET_ENV).is_some() {
        return ninox_ptyd::socket_path();
    }
    match crate::test_isolation::scratch_config_root() {
        Some(root) => scratch_socket(&root, &std::env::temp_dir()),
        None => ninox_ptyd::socket_path(),
    }
}

fn scratch_socket(root: &Path, temp_dir: &Path) -> PathBuf {
    let socket = root.join("ptyd.sock");
    if socket.as_os_str().len() <= ninox_ptyd::max_socket_path_len() {
        return socket;
    }
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    root.hash(&mut h);
    temp_dir.join(format!("ninox-ptyd-{:016x}.sock", h.finish()))
}

/// Where the ptyd host keeps pane checkpoints, following
/// [`ptyd_socket_path`]'s choice.
pub fn ptyd_checkpoint_dir() -> PathBuf {
    if std::env::var_os(ninox_ptyd::SOCKET_ENV).is_some() {
        return ninox_ptyd::checkpoint::default_dir();
    }
    match crate::test_isolation::scratch_config_root() {
        Some(root) => root.join("ptyd-checkpoints"),
        None => ninox_ptyd::checkpoint::default_dir(),
    }
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
    effective_backend(crate::config::AppConfig::load_or_default().runtime.backend)
}

/// Start the ptyd host from `ninox_bin` unless one already answers.
pub async fn ensure_ptyd_host(ninox_bin: &Path) -> Result<()> {
    anyhow::ensure!(ptyd_allowed(), "ptyd is disabled in test binaries without an isolated socket");
    let backend = PtydBackend::new(ptyd_socket_path(), ninox_bin.to_string_lossy().into_owned());
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

/// See [`Runtime::for_existing`].
pub async fn backend_for(id: &str, kind: &SessionKindData) -> Arc<dyn SessionBackend> {
    Runtime::current().for_existing(id, kind).await
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
/// which closes it by killing the child (so the detach chord must not
/// apply) and emulates its output (so it follows the pane's size).
pub fn embedded_attach_args(mut argv: Vec<String>) -> Vec<String> {
    let is_pane_attach = argv.get(1).is_some_and(|a| a == "pane") && argv.get(2).is_some_and(|a| a == "attach");
    if is_pane_attach {
        for flag in ["--follow-size", "--no-detach"] {
            if !argv.iter().any(|a| a == flag) {
                argv.insert(3, flag.into());
            }
        }
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

/// The calling process's own pid followed by each ancestor's, nearest
/// first — one `ps` walk, for callers checking many candidate roots at once.
pub fn process_ancestor_pids() -> Vec<u32> {
    crate::tmux::process_ancestor_pids()
}

/// Identity of the calling process itself, for any process (not only ones in a tmux pane).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub created_at: i64,
}

pub fn current_process_identity() -> ProcessIdentity {
    let pid = std::process::id();
    ProcessIdentity {
        pid,
        created_at: crate::tmux::process_started_at(pid),
    }
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
