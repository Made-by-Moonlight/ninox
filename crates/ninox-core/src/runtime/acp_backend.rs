//! [`SessionBackend`] for a harness speaking the Agent Client Protocol
//! (ACP) natively or via an adapter subprocess over stdio — OpenCode
//! (`opencode acp`, native), Codex (`@agentclientprotocol/codex-acp`) and Pi
//! (`pi-acp@0.0.34`, pinned). See
//! `docs/superpowers/specs/2026-10-21-cross-harness-messaging-design.md`'s
//! "Resolved decisions" section (decision 2): Antigravity is permanently
//! deferred — every available adapter is an unofficial wrapper and several
//! flag Google's ToS as treating third-party access as grounds for account
//! suspension, a disqualifying risk independent of maturity.
//!
//! Unlike a pty backend, there is no raw terminal here: the agent process
//! speaks newline-delimited JSON-RPC 2.0 over its own stdin/stdout
//! (`protocolVersion: 1`). `AcpBackend` implements the full handshake
//! (`initialize` + `session/new`) and one `session/prompt` turn, recording
//! incoming `session/update` notifications as [`TranscriptEntry`] — the
//! non-pty counterpart to a tmux/ptyd pane's scrollback. It does not
//! implement every ACP method: an agent-initiated request this backend
//! doesn't recognize gets an honest JSON-RPC "method not found" reply (so
//! the agent's turn never hangs waiting on us), never silent success.
//!
//! Scope deliberately excluded from this file (see the design doc and the
//! implementation PRs that added it): `ninox-server` owning the connection
//! for a session's full lifetime + CLI routing to it, TUI/GUI rendering of
//! the transcript, per-harness spawn-time routing (nothing yet makes
//! `ninox spawn` produce an ACP-backed session for any harness), and
//! `worker_status::ActivityEvent` live-activity-chip integration — all
//! separately filed follow-ons this backend unblocks, not done here for any
//! of the three harnesses.

use std::collections::HashMap;
use std::path::Path;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{oneshot, Mutex, Notify};

use super::{Backend, LiveSession, PaneIdentity, SessionBackend, SessionReactions, ToolCallStatus, TranscriptEntry};
use crate::events::{Engine, Event};
use crate::types::SessionId;

/// ACP's single, stable major protocol version (see the design doc's
/// resolved decision #1) — distinct from the JSON schema's own finer semver.
const PROTOCOL_VERSION: i64 = 1;

/// How long the `initialize`/`session/new` handshake may take.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long one `session/prompt` turn may run. Generous on purpose — a
/// multi-tool-call agentic turn can run long; arbitrary, bump if real usage
/// needs more.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(600);

/// Set on the spawned agent process's environment; read back by
/// [`AcpBackend::current_pane_identity`] the same way `ptyd_backend`'s
/// `PANE_ID_ENV` lets a pty pane identify itself — a tool call the agent
/// runs inherits this from its parent, so a `ninox` invocation from inside
/// one can find its own session.
const PANE_ID_ENV: &str = "NINOX_ACP_PANE_ID";

type DynWriter = Pin<Box<dyn AsyncWrite + Send>>;
type DynReader = Pin<Box<dyn AsyncRead + Send>>;

/// Default per-harness ACP agent invocations, per the design spec's
/// resolved decision 2. `codex`/`pi` both run through `npx` rather than a
/// globally-installed binary: neither adapter is ninox's own code, so
/// treating it as an external subprocess dependency (the same relationship
/// the remote-sessions design has with `ssh`) avoids requiring a global
/// install step on every machine that spawns one of these harnesses. `pi`
/// pins its adapter's exact version (`0.0.34`): its README itself calls it
/// an MVP-stage adapter and warns of expected breaking changes, unlike
/// `codex`'s adapter, which is Zed's own production ACP path for Codex and
/// is left unpinned.
fn default_agent_commands() -> HashMap<String, Vec<String>> {
    let mut m = HashMap::new();
    m.insert("opencode".to_string(), vec!["opencode".to_string(), "acp".to_string()]);
    m.insert(
        "codex".to_string(),
        vec!["npx".to_string(), "-y".to_string(), "@agentclientprotocol/codex-acp".to_string()],
    );
    m.insert("pi".to_string(), vec!["npx".to_string(), "-y".to_string(), "pi-acp@0.0.34".to_string()]);
    m
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A JSON-RPC error object as the agent sent it (or as we synthesize one for
/// a connection failure).
#[derive(Debug, Clone)]
struct AcpRpcError {
    code:    i64,
    message: String,
}

impl std::fmt::Display for AcpRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "acp error {}: {}", self.code, self.message)
    }
}

/// One live ACP agent process and the bookkeeping its JSON-RPC pipe needs:
/// the write half (framed as NDJSON), pending-request correlation by id, the
/// accumulated [`TranscriptEntry`] log, and where to live-emit new entries
/// once `start_streaming` has been called.
struct AcpSession {
    /// `None` for an in-memory test double with no real OS process.
    child:         Option<Mutex<Child>>,
    pid:           u32,
    created_ms:    i64,
    writer:        Mutex<DynWriter>,
    next_id:       AtomicI64,
    pending:       Mutex<HashMap<i64, oneshot::Sender<Result<Value, AcpRpcError>>>>,
    acp_session_id: Mutex<Option<String>>,
    transcript:    Mutex<Vec<TranscriptEntry>>,
    turn_in_progress: AtomicBool,
    turn_idle:     Notify,
    /// Serializes entry into [`AcpBackend::prompt_turn`] — ACP has no
    /// notion of two concurrent `session/prompt` turns on one session, so a
    /// second send issued while the first is still running (an ordinary
    /// pattern now that `send_acp_message` no longer blocks its caller for
    /// the whole turn) must queue behind it here, not race it over the wire.
    /// No bound on how many sends can queue up, and no per-waiter timeout —
    /// a hung or slow turn serializes arbitrarily many future ones behind
    /// it with no signal back to their callers (`send_acp_message` still
    /// reports 202 immediately regardless of queue depth). Acceptable for
    /// phase 1; revisit if that turns out to matter in practice.
    turn_lock:     Mutex<()>,
    /// Set as `teardown`'s very first action, before anything else —
    /// checked by `prompt_turn` immediately after acquiring `turn_lock`. A
    /// turn queued behind one `teardown` just failed (via its pending
    /// drain) can otherwise still acquire the lock in the window before
    /// `teardown` finishes killing the child: writing to the connection
    /// would succeed (stdin is still open) while nothing will ever read a
    /// reply (the reader task is already gone by then), hanging for the
    /// full `PROMPT_TIMEOUT`. Since this field is set before the drain that
    /// releases any such waiter, it is guaranteed true by the time one gets
    /// here.
    torn_down:     AtomicBool,
    /// The `u64` is `start_streaming`'s generation token: its cleanup task
    /// only clears this slot if it's still the entry *that call* set, so a
    /// stale cleanup task from a superseded `start_streaming` call (e.g. a
    /// quick reconnect) can't race and null out a newer subscriber's entry.
    /// This mutex is also what serializes `start_streaming`'s own
    /// register-then-stamp-then-write sequence across concurrent callers —
    /// see that method's doc comment.
    live_emit:     Mutex<Option<(Arc<Engine>, SessionId, u64)>>,
    live_emit_generation: AtomicU64,
    reader_task:   Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl AcpSession {
    async fn spawn_over(
        pid:        u32,
        created_ms: i64,
        child:      Option<Child>,
        writer:     DynWriter,
        reader:     DynReader,
    ) -> Arc<Self> {
        let session = Arc::new(Self {
            child: child.map(Mutex::new),
            pid,
            created_ms,
            writer: Mutex::new(writer),
            next_id: AtomicI64::new(1),
            pending: Mutex::new(HashMap::new()),
            acp_session_id: Mutex::new(None),
            transcript: Mutex::new(Vec::new()),
            turn_in_progress: AtomicBool::new(false),
            turn_idle: Notify::new(),
            turn_lock: Mutex::new(()),
            torn_down: AtomicBool::new(false),
            live_emit: Mutex::new(None),
            live_emit_generation: AtomicU64::new(0),
            reader_task: Mutex::new(None),
        });
        let handle = tokio::spawn(run_reader(session.clone(), reader));
        *session.reader_task.lock().await = Some(handle);
        session
    }

    async fn write_line(&self, value: &Value) -> Result<()> {
        let mut line = serde_json::to_string(value)?;
        line.push('\n');
        let mut writer = self.writer.lock().await;
        writer.write_all(line.as_bytes()).await?;
        writer.flush().await?;
        Ok(())
    }

    async fn call(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Err(e) = self.write_line(&request).await {
            self.pending.lock().await.remove(&id);
            return Err(e);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(rpc_err))) => bail!("{method} failed: {rpc_err}"),
            Ok(Err(_)) => bail!("{method}: agent process exited before replying"),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                bail!("{method} timed out after {timeout:?}")
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.write_line(&json!({"jsonrpc": "2.0", "method": method, "params": params})).await
    }

    /// Append a transcript entry and, if a caller has registered interest
    /// via `start_streaming`, emit it live too.
    async fn record(&self, entry: TranscriptEntry) {
        self.transcript.lock().await.push(entry.clone());
        if let Some((engine, session_id, _generation)) = self.live_emit.lock().await.clone() {
            engine.emit(Event::SessionTranscript { session_id, entry });
        }
    }

    async fn is_alive(&self) -> bool {
        match &self.child {
            Some(child) => matches!(child.lock().await.try_wait(), Ok(None)),
            // A test double with no real process is "alive" until killed.
            None => true,
        }
    }

    async fn teardown(&self) {
        // Set before anything else: see this flag's own doc comment on why
        // its ordering relative to the rest of this function is load-bearing.
        self.torn_down.store(true, Ordering::SeqCst);
        if let Some(handle) = self.reader_task.lock().await.take() {
            handle.abort();
        }
        // `abort()` above pre-empts `run_reader`'s own EOF-driven cleanup
        // (its tail, below), which is what would otherwise fail any
        // `call()` still waiting on a reply — so a kill racing an in-flight
        // turn would strand it (and the `turn_lock` it holds) until that
        // call's own timeout fires, up to `PROMPT_TIMEOUT`. Do the same
        // draining here instead, so a kill resolves it promptly.
        for (_, tx) in self.pending.lock().await.drain() {
            let _ = tx.send(Err(AcpRpcError { code: 0, message: "acp session killed".to_string() }));
        }
        self.turn_in_progress.store(false, Ordering::SeqCst);
        self.turn_idle.notify_waiters();
        if let Some(child) = &self.child {
            let _ = child.lock().await.kill().await;
        }
    }
}

/// Reads NDJSON lines from the agent until it closes the pipe, dispatching
/// each to [`handle_incoming`]. Fails any still-pending request once the
/// pipe closes — the agent is gone, so nothing will ever answer them.
async fn run_reader(session: Arc<AcpSession>, reader: DynReader) {
    let mut lines = BufReader::new(reader).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) if line.trim().is_empty() => continue,
            Ok(Some(line)) => match serde_json::from_str::<Value>(&line) {
                Ok(msg) => handle_incoming(&session, msg).await,
                Err(e) => tracing::warn!("acp: malformed line from agent ({e}): {line}"),
            },
            Ok(None) => break,
            Err(e) => {
                tracing::warn!("acp: reading agent stdout: {e}");
                break;
            }
        }
    }
    for (_, tx) in session.pending.lock().await.drain() {
        let _ = tx.send(Err(AcpRpcError { code: 0, message: "agent process exited".to_string() }));
    }
    session.turn_in_progress.store(false, Ordering::SeqCst);
    session.turn_idle.notify_waiters();
}

/// One parsed JSON-RPC line from the agent: either a response to a request
/// we sent (has `id`, no `method`), a notification (`method`, no `id`), or a
/// request from the agent to us (`method` and `id`, both present).
async fn handle_incoming(session: &Arc<AcpSession>, msg: Value) {
    let id = msg.get("id").cloned();
    let Some(method) = msg.get("method").and_then(Value::as_str) else {
        // A response to one of our own requests.
        let Some(id) = id.as_ref().and_then(Value::as_i64) else { return };
        let Some(tx) = session.pending.lock().await.remove(&id) else { return };
        let reply = match msg.get("error") {
            Some(error) => Err(AcpRpcError {
                code:    error.get("code").and_then(Value::as_i64).unwrap_or(0),
                message: error.get("message").and_then(Value::as_str).unwrap_or("unknown error").to_string(),
            }),
            None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
        };
        let _ = tx.send(reply);
        return;
    };

    if method == "session/update" {
        if let Some(params) = msg.get("params") {
            apply_session_update(session, params).await;
        }
        return;
    }

    // Every other agent → client method (permission requests, fs access,
    // terminal control, …) is out of this phase's scope — see the module
    // doc comment. Answer honestly so the agent's turn never hangs waiting
    // on a reply we were never going to send.
    if let Some(id) = id {
        let _ = session
            .write_line(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": format!("Method not found: {method}")},
            }))
            .await;
    }
}

fn chunk_text(update: &Value) -> String {
    update.get("content").and_then(|c| c.get("text")).and_then(Value::as_str).unwrap_or_default().to_string()
}

fn tool_call_detail(update: &Value) -> String {
    let content = update
        .get("content")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("content")?.get("text")?.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    if !content.is_empty() {
        return content;
    }
    update.get("title").and_then(Value::as_str).unwrap_or_default().to_string()
}

fn tool_call_entry(update: &Value) -> TranscriptEntry {
    let id = update.get("toolCallId").and_then(Value::as_str).unwrap_or_default().to_string();
    let name = update
        .get("name")
        .and_then(Value::as_str)
        .or_else(|| update.get("title").and_then(Value::as_str))
        .unwrap_or("tool_call")
        .to_string();
    let status = update
        .get("status")
        .and_then(|s| serde_json::from_value::<ToolCallStatus>(s.clone()).ok())
        .unwrap_or(ToolCallStatus::Pending);
    let detail = tool_call_detail(update);
    TranscriptEntry::ToolCall { id, name, status, detail }
}

/// `tool_call_update` is folded into a fresh [`TranscriptEntry::ToolCall`]
/// entry rather than mutating an earlier one in place, keeping the
/// transcript a simple append-only log (same shape as `capture_history`'s
/// contract). A renderer that wants "latest status per tool call id" can
/// fold the log by `id` itself — a cheap transform the already-filed
/// TUI/GUI rendering follow-on can make, not something this backend needs
/// to precompute.
async fn apply_session_update(session: &Arc<AcpSession>, params: &Value) {
    let Some(update) = params.get("update") else { return };
    let Some(kind) = update.get("sessionUpdate").and_then(Value::as_str) else { return };

    let entry = match kind {
        "user_message_chunk" => Some(TranscriptEntry::UserMessage(chunk_text(update))),
        "agent_message_chunk" => Some(TranscriptEntry::AgentText(chunk_text(update))),
        "tool_call" | "tool_call_update" => Some(tool_call_entry(update)),
        other => {
            tracing::debug!("acp: ignoring unhandled session/update kind {other:?} (out of this phase's scope)");
            None
        }
    };

    if let Some(entry) = entry {
        session.record(entry).await;
    }
}

fn render_entry(entry: &TranscriptEntry) -> String {
    match entry {
        TranscriptEntry::UserMessage(text) => format!("[user] {text}"),
        TranscriptEntry::AgentText(text) => format!("[agent] {text}"),
        TranscriptEntry::ToolCall { name, status, detail, .. } => format!("[tool:{name}] {status:?}: {detail}"),
        TranscriptEntry::TurnStarted => "--- turn started ---".to_string(),
        TranscriptEntry::TurnEnded => "--- turn ended ---".to_string(),
    }
}

fn render_transcript_lines(entries: &[TranscriptEntry]) -> Vec<String> {
    entries.iter().map(render_entry).collect()
}

async fn handshake(session: &AcpSession, workspace: &str) -> Result<()> {
    let init = session
        .call(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false},
            }),
            HANDSHAKE_TIMEOUT,
        )
        .await
        .context("acp initialize handshake failed")?;
    if let Some(negotiated) = init.get("protocolVersion").and_then(Value::as_i64) {
        if negotiated != PROTOCOL_VERSION {
            tracing::warn!("acp agent negotiated protocolVersion {negotiated}, expected {PROTOCOL_VERSION}");
        }
    }

    let new_session = session
        .call("session/new", json!({"cwd": workspace, "mcpServers": []}), HANDSHAKE_TIMEOUT)
        .await
        .context("acp session/new failed")?;
    let acp_session_id = new_session
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("acp session/new response missing sessionId"))?
        .to_string();
    *session.acp_session_id.lock().await = Some(acp_session_id);
    Ok(())
}

/// A slot in [`AcpBackend`]'s session map: `Reserved` closes the race
/// between `create_session`'s existence check and its eventual insert (the
/// spawn + handshake in between spans several `.await` points) — a second
/// concurrent `create_session` for the same id sees the reservation and
/// fails immediately, rather than both racing to spawn an agent process
/// and the loser's `AcpSession` (and its live subprocess) getting silently
/// dropped from the map by the winner's `insert` with nothing to tear it
/// down. The same "never write across an `.await` without reserving first"
/// shape as this crate's `Store` read→apply→write convention.
///
/// `Killing` closes a second race: a `kill_session` call that lands while a
/// `create_session` is still mid-flight (`Reserved`, subprocess possibly
/// already spawned) must not just remove the entry and report success — the
/// in-flight `create_session` would then unconditionally re-insert
/// `Ready(session)` once its handshake finishes, resurrecting a session the
/// caller already believes is dead. `kill_session` instead tombstones a
/// `Reserved` slot as `Killing`; `create_session`'s completion checks for
/// that tombstone before promoting to `Ready` and tears the new session down
/// instead if it finds one.
enum SessionSlot {
    Reserved,
    Ready(Arc<AcpSession>),
    Killing,
}

/// How [`AcpBackend::command_for`] resolves the agent command to launch —
/// one or the other, never both at once (a single field rather than two
/// independently-settable ones, so the two states stay mutually exclusive
/// by construction).
enum CommandSource {
    /// The real per-harness table ([`default_agent_commands`] by default).
    PerHarness(HashMap<String, Vec<String>>),
    /// Test/override seam: used for *every* harness name regardless of
    /// which one `create_session` is called with — lets a test point at one
    /// fake agent without needing to care which harness name a call site
    /// passes (the shape every existing `AcpBackend` test already relies
    /// on).
    Override(Vec<String>),
}

/// [`SessionBackend`] for an ACP agent subprocess — OpenCode natively, Codex
/// and Pi via their adapter (or, in tests, an in-memory double driving the
/// same protocol over a duplex pipe — see [`AcpSession::spawn_over`]).
pub struct AcpBackend {
    commands: CommandSource,
    sessions: Mutex<HashMap<String, SessionSlot>>,
}

impl AcpBackend {
    pub fn new() -> Self {
        Self { commands: CommandSource::PerHarness(default_agent_commands()), sessions: Mutex::new(HashMap::new()) }
    }

    /// Overrides the agent binary + argv for every harness (default: the
    /// per-harness table in [`default_agent_commands`]) — the seam tests use
    /// to point at a fake agent instead of a real one, independent of
    /// whichever harness name `create_session` is called with.
    pub fn with_agent_command(agent_command: Vec<String>) -> Self {
        Self { commands: CommandSource::Override(agent_command), sessions: Mutex::new(HashMap::new()) }
    }

    /// Overrides the full per-harness command table (unlike
    /// [`Self::with_agent_command`], still dispatches by harness name) — the
    /// seam tests use to prove `create_session` actually launches a
    /// different subprocess per harness, not just that the real table is
    /// correct.
    pub fn with_agent_commands(agent_commands: HashMap<String, Vec<String>>) -> Self {
        Self { commands: CommandSource::PerHarness(agent_commands), sessions: Mutex::new(HashMap::new()) }
    }

    /// Resolves the agent command to launch for `harness` (the value
    /// [`SessionBackend::create_session`]'s `cmd` parameter carries for this
    /// backend — see that impl's doc comment). An empty `harness` defaults
    /// to `"opencode"`, preserving every pre-existing call site's exact
    /// behavior unmodified (per-harness spawn routing doesn't thread a real
    /// harness name through yet — see the module doc comment). An unknown,
    /// non-empty harness is a real error, not a silent fallback: a caller
    /// that explicitly named a harness this process has no adapter command
    /// for made a configuration mistake worth surfacing, not papering over.
    fn command_for(&self, harness: &str) -> Result<Vec<String>> {
        match &self.commands {
            CommandSource::Override(cmd) => Ok(cmd.clone()),
            CommandSource::PerHarness(table) => {
                let key = if harness.is_empty() { "opencode" } else { harness };
                table.get(key).cloned().ok_or_else(|| anyhow!("no ACP agent command registered for harness {key:?}"))
            }
        }
    }

    /// The live session for `id`, or `None` for an unknown id *or* one still
    /// `Reserved` (a `create_session` in flight has nothing ready yet).
    async fn ready_session(&self, id: &str) -> Option<Arc<AcpSession>> {
        match self.sessions.lock().await.get(id) {
            Some(SessionSlot::Ready(session)) => Some(session.clone()),
            _ => None,
        }
    }

    async fn require_session(&self, id: &str) -> Result<Arc<AcpSession>> {
        self.ready_session(id).await.ok_or_else(|| anyhow!("no acp session named {id}"))
    }

    /// The actual spawn + handshake, isolated so `create_session` can hold
    /// the `Reserved` slot for exactly this span and clean it up on any
    /// error path without duplicating that bookkeeping per failure branch.
    async fn spawn_and_handshake(
        &self,
        id:        &str,
        workspace: &str,
        harness:   &str,
        env:       &[(&str, &str)],
    ) -> Result<Arc<AcpSession>> {
        anyhow::ensure!(Path::new(workspace).is_dir(), "workspace directory does not exist: {workspace}");

        let agent_command = self.command_for(harness)?;
        let [binary, args @ ..] = agent_command.as_slice() else {
            bail!("acp agent command is empty");
        };
        let mut command = Command::new(binary);
        command
            .args(args)
            .current_dir(workspace)
            .envs(env.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())))
            .env(PANE_ID_ENV, id)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        let mut child =
            command.spawn().with_context(|| format!("failed to spawn acp agent {agent_command:?}"))?;
        let pid = child.id().ok_or_else(|| anyhow!("acp agent for {id} exited immediately after spawn"))?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("acp agent for {id} has no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("acp agent for {id} has no stdout"))?;

        let session =
            AcpSession::spawn_over(pid, now_ms(), Some(child), Box::pin(stdin) as DynWriter, Box::pin(stdout) as DynReader)
                .await;

        if let Err(e) = handshake(&session, workspace).await {
            session.teardown().await;
            return Err(e);
        }
        Ok(session)
    }

    /// `send_keys`/`write_input`'s shared path: a full ACP prompt turn.
    /// Records `UserMessage`/`TurnStarted`/`TurnEnded` transcript entries
    /// around the call regardless of outcome, so a failed or cancelled turn
    /// still leaves an honest record.
    async fn prompt_turn(&self, id: &str, text: &str) -> Result<()> {
        let session = self.require_session(id).await?;
        let acp_session_id = session
            .acp_session_id
            .lock()
            .await
            .clone()
            .ok_or_else(|| anyhow!("acp session {id} has not completed its handshake"))?;
        let _turn_guard = session.turn_lock.lock().await;
        // A waiter that queued behind a turn `teardown` just failed can
        // reach here before `teardown` finishes killing the child — see
        // `torn_down`'s doc comment for why checking it right here, right
        // after acquiring the lock, closes that window.
        if session.torn_down.load(Ordering::SeqCst) {
            bail!("acp session {id} was killed");
        }

        session.record(TranscriptEntry::UserMessage(text.to_string())).await;
        session.record(TranscriptEntry::TurnStarted).await;
        session.turn_in_progress.store(true, Ordering::SeqCst);

        let result = session
            .call(
                "session/prompt",
                json!({"sessionId": acp_session_id, "prompt": [{"type": "text", "text": text}]}),
                PROMPT_TIMEOUT,
            )
            .await;

        session.turn_in_progress.store(false, Ordering::SeqCst);
        session.turn_idle.notify_waiters();
        session.record(TranscriptEntry::TurnEnded).await;
        result.map(|_| ())
    }
}

/// How long [`AcpBackend::send_keys_detached`] waits for an early result
/// before detaching — short enough that neither an HTTP caller nor the
/// poller's own loop blocks on a turn that can legitimately run for up to
/// [`PROMPT_TIMEOUT`] (600s), long enough to still catch (and report) the
/// fast failures — a missing handshake, a dead connection — rather than
/// only ever the slow ones.
pub const SEND_ACK_WINDOW: Duration = Duration::from_secs(3);

/// What [`AcpBackend::send_keys_detached`] learned within its `ack_window`.
pub enum DetachedSendOutcome {
    /// The turn already completed successfully.
    Delivered,
    /// The turn already completed, but failed.
    Failed(anyhow::Error),
    /// Still running past the window — left to finish in the background;
    /// any later failure is only logged, never reported back to the caller
    /// that triggered it.
    StillRunning,
}

impl AcpBackend {
    /// Runs a `session/prompt` turn for `id` without blocking the caller
    /// for its full duration (up to [`PROMPT_TIMEOUT`], 600s): waits up to
    /// `ack_window` for an early result, then detaches and lets the turn
    /// keep running against the connection this backend already holds,
    /// logging a late failure instead of reporting it — the same
    /// fire-and-forget semantics every other delivery mechanism in
    /// `messaging.rs` already uses. Shared by `ninox-server`'s
    /// `send_acp_message` route and `events.rs`'s `InProcessAcpRouter`, so
    /// neither an HTTP caller nor the poller's own reaction loop ever
    /// blocks on a turn that can legitimately run for minutes.
    pub async fn send_keys_detached(
        self:       Arc<Self>,
        id:         String,
        text:       String,
        ack_window: Duration,
    ) -> DetachedSendOutcome {
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let result = self.send_keys(&id, &text).await;
            if let Err(ref err) = result {
                tracing::warn!("acp send to {id} failed: {err}");
            }
            let _ = tx.send(result);
        });
        match tokio::time::timeout(ack_window, rx).await {
            Ok(Ok(Ok(()))) => DetachedSendOutcome::Delivered,
            Ok(Ok(Err(err))) => DetachedSendOutcome::Failed(err),
            Ok(Err(_)) | Err(_) => DetachedSendOutcome::StillRunning,
        }
    }
}

impl Default for AcpBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SessionBackend for AcpBackend {
    fn kind(&self) -> Backend {
        Backend::Acp
    }

    async fn knows(&self, id: &str) -> bool {
        self.sessions.lock().await.contains_key(id)
    }

    /// Launches the agent in ACP mode and completes its `initialize` +
    /// `session/new` handshake. `cmd`/`env`'s usual "argv this pane runs"
    /// meaning doesn't apply here: `cmd` is repurposed as the *harness name*
    /// to look up in [`Self::command_for`] (e.g. `"opencode"`, `"codex"`,
    /// `"pi"`) rather than a literal argv — an empty string defaults to
    /// `"opencode"`, since nothing yet threads a real harness name through
    /// from a spawn path (per-harness spawn-time routing is a separate,
    /// not-yet-implemented follow-on — see the module doc comment). `env` is
    /// still applied on top of the spawned process's environment, matching
    /// every other backend's `create_session` contract.
    ///
    /// Reserves `id` in the session map before the spawn + handshake (which
    /// spans several `.await` points) so a concurrent second call for the
    /// same id fails fast instead of racing to spawn a second agent process
    /// — see [`SessionSlot`]. On success, only promotes the reservation to
    /// `Ready` if it's still `Reserved`: a concurrent `kill_session` may have
    /// tombstoned it as `Killing` while the spawn/handshake was in flight, in
    /// which case the newly-spawned session is torn down instead of being
    /// resurrected.
    async fn create_session(&self, id: &str, workspace: &str, cmd: &str, env: &[(&str, &str)]) -> Result<()> {
        {
            let mut sessions = self.sessions.lock().await;
            anyhow::ensure!(!sessions.contains_key(id), "acp session {id} already exists");
            sessions.insert(id.to_string(), SessionSlot::Reserved);
        }

        match self.spawn_and_handshake(id, workspace, cmd, env).await {
            Ok(session) => {
                let mut sessions = self.sessions.lock().await;
                match sessions.get(id) {
                    Some(SessionSlot::Reserved) => {
                        sessions.insert(id.to_string(), SessionSlot::Ready(session));
                        Ok(())
                    }
                    _ => {
                        // Killed (tombstoned `Killing`) while we were
                        // spawning — never resurrect a session the caller
                        // already believes is dead.
                        sessions.remove(id);
                        drop(sessions);
                        session.teardown().await;
                        bail!("acp session {id} was killed while starting")
                    }
                }
            }
            Err(e) => {
                self.sessions.lock().await.remove(id);
                Err(e)
            }
        }
    }

    /// A `Reserved` slot (a `create_session` is mid-flight) is tombstoned as
    /// `Killing` rather than removed outright, so that in-flight
    /// `create_session`'s completion can see it and tear its new session
    /// down instead of re-inserting `Ready` — see [`SessionSlot`].
    async fn kill_session(&self, id: &str) -> Result<()> {
        let mut sessions = self.sessions.lock().await;
        match sessions.get_mut(id) {
            None => return Ok(()),
            Some(slot @ SessionSlot::Reserved) => {
                *slot = SessionSlot::Killing;
                return Ok(());
            }
            Some(SessionSlot::Killing) => return Ok(()),
            Some(SessionSlot::Ready(_)) => {}
        }
        let Some(SessionSlot::Ready(session)) = sessions.remove(id) else { unreachable!() };
        drop(sessions);
        session.teardown().await;
        Ok(())
    }

    async fn has_session(&self, id: &str) -> bool {
        match self.ready_session(id).await {
            Some(session) => session.is_alive().await,
            None => false,
        }
    }

    async fn list_sessions(&self) -> Result<Vec<LiveSession>> {
        let sessions = self.sessions.lock().await;
        let mut out = Vec::with_capacity(sessions.len());
        for (id, slot) in sessions.iter() {
            let SessionSlot::Ready(session) = slot else { continue };
            let pid = session.is_alive().await.then_some(session.pid);
            out.push(LiveSession { id: id.clone(), created_ms: session.created_ms, pid, backend: Backend::Acp });
        }
        Ok(out)
    }

    async fn session_pid(&self, id: &str) -> Option<u32> {
        self.ready_session(id).await.map(|s| s.pid)
    }

    /// No concrete command exists yet for "follow this session's structured
    /// transcript read-only" — that CLI surface belongs to the already-filed
    /// TUI/GUI rendering follow-on this backend unblocks. Returning an empty
    /// argv is the honest answer today (same "non-committal, never
    /// fabricated" contract `RemoteBackend` uses for its own not-yet-built
    /// pieces), not a tmux attach that would be meaningless here anyway —
    /// there is no raw terminal to attach to.
    async fn attach_args(&self, _id: &str) -> Vec<String> {
        Vec::new()
    }

    async fn history_size(&self, id: &str) -> i64 {
        match self.ready_session(id).await {
            Some(session) => session.transcript.lock().await.len() as i64,
            None => 0,
        }
    }

    async fn capture_history(&self, id: &str, start: i64, end: i64) -> Vec<u8> {
        let Some(session) = self.ready_session(id).await else { return Vec::new() };
        // Rendered directly under the lock rather than cloning the whole
        // (unbounded, append-only) entry log out first — avoids doubling
        // the allocation for a session with a long transcript.
        let lines = render_transcript_lines(&session.transcript.lock().await);
        if lines.is_empty() {
            return Vec::new();
        }
        let len = lines.len() as i64;
        let s = start.clamp(0, len - 1) as usize;
        let e = end.clamp(0, len - 1) as usize;
        if s > e {
            return Vec::new();
        }
        lines[s..=e].join("\n").into_bytes()
    }

    /// ACP has no fixed-size "visible screen" the way a pty does — the
    /// whole rendered transcript is returned every time, `scrollback`/`ansi`
    /// ignored, same as `capture_history`'s contract for this backend.
    async fn read_screen(&self, id: &str, _scrollback: usize, _ansi: bool) -> Result<String> {
        let session = self.require_session(id).await?;
        let transcript = session.transcript.lock().await;
        Ok(render_transcript_lines(&transcript).join("\n"))
    }

    /// Maps directly onto an ACP `session/prompt` turn — this is the "send
    /// a message" path the rest of the codebase already calls.
    async fn send_keys(&self, id: &str, text: &str) -> Result<()> {
        self.prompt_turn(id, text).await
    }

    /// ACP has no pty-idle-nudge concept to replicate.
    async fn wake_idle_session(&self, _id: &str) -> Result<()> {
        Ok(())
    }

    /// Uses the session's actual turn-idle state — an explicit signal ACP
    /// gives us — rather than a pty backend's screen-scrape heuristic.
    async fn wait_for_input_prompt(&self, id: &str, timeout: Duration) -> bool {
        let Ok(session) = self.require_session(id).await else { return false };
        let idle = session.turn_idle.notified();
        if !session.turn_in_progress.load(Ordering::SeqCst) {
            return true;
        }
        tokio::select! {
            _ = idle => !session.turn_in_progress.load(Ordering::SeqCst),
            _ = tokio::time::sleep(timeout) => false,
        }
    }

    /// Forwards as UTF-8 text through the same path as `send_keys`, with
    /// one special case: a literal `0x03` (Ctrl-C) byte maps to ACP's
    /// `session/cancel` notification — a real cancel RPC where pty backends
    /// only have a raw interrupt byte.
    async fn write_input(&self, id: &str, bytes: &[u8]) -> Result<()> {
        if bytes == [0x03] {
            let session = self.require_session(id).await?;
            let acp_session_id = session
                .acp_session_id
                .lock()
                .await
                .clone()
                .ok_or_else(|| anyhow!("acp session {id} has not completed its handshake"))?;
            return session.notify("session/cancel", json!({"sessionId": acp_session_id})).await;
        }
        self.prompt_turn(id, &String::from_utf8_lossy(bytes)).await
    }

    /// Arranges for future `session/update`-derived transcript entries to
    /// also emit `Event::SessionTranscript`. The JSON-RPC reader loop
    /// already runs from `create_session`, so there is no separate stream
    /// to start — only where to deliver it. No pty writer is registered:
    /// there is no raw terminal for a browser terminal widget to write to.
    ///
    /// Tags this call's `live_emit` entry with a fresh generation so its
    /// cleanup task only clears the slot if a later `start_streaming` call
    /// (e.g. a quick reconnect) hasn't already replaced it — see the
    /// `live_emit` field doc on [`AcpSession`].
    ///
    /// `live_emit`'s own lock is held across `register_stream` (whose
    /// internal lock decides cancellation order between concurrent callers)
    /// *and* the generation stamp + write. Without that, two genuinely
    /// concurrent `start_streaming` calls for the same session could
    /// interleave such that the older call's write lands *after* the newer
    /// call's write — at which point the older call's now-stale cleanup task
    /// (woken by the newer call's `register_stream` cancelling it) would see
    /// its own generation as current and clear it, silently killing live
    /// delivery for both callers. Holding the lock across the whole sequence
    /// serializes "register, then stamp, then write" as one atomic step, so
    /// a cleanup task can only ever observe a write that happened-before it.
    async fn start_streaming(&self, engine: Arc<Engine>, session_id: SessionId, id: &str) -> Result<()> {
        let session = self.require_session(id).await?;

        let mut live_emit = session.live_emit.lock().await;
        let cancel_rx = engine.register_stream(session_id.clone()).await;
        let generation = session.live_emit_generation.fetch_add(1, Ordering::SeqCst) + 1;
        *live_emit = Some((engine, session_id, generation));
        drop(live_emit);

        let session = session.clone();
        tokio::spawn(async move {
            let _ = cancel_rx.await;
            let mut live_emit = session.live_emit.lock().await;
            if matches!(&*live_emit, Some((_, _, g)) if *g == generation) {
                *live_emit = None;
            }
        });
        Ok(())
    }

    /// Whether the calling process is a tool-call child of one of this
    /// backend's own agent processes (mirrors `ptyd_backend`'s
    /// `PANE_ID_ENV` identity check — see [`PANE_ID_ENV`]'s doc comment).
    async fn current_pane_identity(&self) -> Result<Option<PaneIdentity>> {
        let Some(id) = std::env::var(PANE_ID_ENV).ok().filter(|s| !s.is_empty()) else { return Ok(None) };
        let Some(session) = self.ready_session(&id).await else { return Ok(None) };
        let parents = super::process::parent_map();
        if !super::process::descends_in(&parents, std::process::id(), session.pid) {
            return Ok(None);
        }
        Ok(Some(PaneIdentity {
            physical_tmux_name: format!("acp:{id}"),
            pane_id:            format!("acp:{id}"),
            pane_pid:           session.pid,
            pane_created_at:    session.created_ms,
            // No persistent host spans multiple panes the way a tmux
            // server or the ptyd host does — each agent process is its own
            // "epoch", so a respawn (new pid) is unambiguously a new one.
            server_epoch:       session.pid.to_string(),
        }))
    }

    async fn transcript_history(&self, id: &str) -> Vec<TranscriptEntry> {
        match self.ready_session(id).await {
            Some(session) => session.transcript.lock().await.clone(),
            None => Vec::new(),
        }
    }
}

/// ACP sessions react exactly like every other kind today — no overrides.
impl SessionReactions for AcpBackend {}

#[cfg(test)]
mod tests;
