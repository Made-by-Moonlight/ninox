//! The ninox prompt agent (MLOPS-5415): a per-machine singleton session
//! backing the "Ask ninox" prompt. GUI, TUI and the CLI (`ninox prompt
//! ask/clear/status`, see `cli.rs`'s `Command::Prompt` dispatch) all call
//! the functions here directly — never shell out to the CLI binary.
//!
//! Lifecycle: [`ask`] creates the singleton lazily on the first call,
//! reuses it while its pane is alive, and transparently restarts it
//! (preferring `--resume` so a crash never loses the conversation) when its
//! row exists but the pane died. Only [`clear`] discards the conversation
//! on purpose. The session is tagged with the built-in `prompt` worker type
//! (`ninox_core::worker_type::PROMPT_WORKER_TYPE`), which is what
//! `ninox_core::fleet::snapshot::FleetSnapshot::load` keys on to exclude it
//! from fleet/island/Board counts and needs-you — it still answers to
//! `ninox list`/`ninox connect` like any other session.
//!
//! Every function below delegates to an `*_impl` twin that takes the
//! session id and filesystem paths explicitly, so tests never touch the
//! real per-machine singleton id or the real `~/.config/ninox/` paths —
//! only the thin public wrappers hardcode [`PROMPT_SESSION_ID`] and the
//! real `AppConfig` path helpers.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ninox_core::{
    config::{AgentConfig, AppConfig},
    events::Engine,
    harness::{permission_mode, LaunchKind::Interactive},
    messaging, runtime,
    store::Store,
    types::{Session, SessionStatus},
    worker_type::{WorkerTypeSpec, PROMPT_WORKER_TYPE},
};
use serde_json::Value;

pub mod reply;

use crate::spawn_util::{ensure_session_workspace, launch_interactive_session, InteractiveSpawnParams};

/// Re-exported from `ninox-core` (which `fleet::snapshot::FleetSnapshot::
/// load` also needs, and can't depend on this crate for) so callers of this
/// module don't need to reach into `ninox_core::worker_type` themselves.
pub use ninox_core::worker_type::PROMPT_SESSION_ID;

const PROMPT_TIMEOUT: Duration = Duration::from_secs(90);

/// The SQLite-backed lease `ninox fleet restore` uses
/// (`Store::try_acquire_fleet_lock`), reused here to serialize every
/// `ask`/`clear` against the singleton across processes — GUI, TUI and the
/// CLI can all call these concurrently. Without it, two concurrent
/// first-`ask`s racing `store.get_session(id) == None` could both spawn:
/// the loser's `runtime::create_session` fails on the duplicate id, and its
/// failure handler in `launch_interactive_session` then reads the row the
/// winner just created and marks it `Terminated`, mislabeling the winner's
/// live pane dead.
const SINGLETON_LOCK: &str = "prompt-agent";
/// A holder stuck longer than this is presumed crashed/killed before it
/// could release — generous enough to cover a cold harness start
/// ([`PROMPT_TIMEOUT`]'s 90s) plus margin.
const SINGLETON_LOCK_STALE_MS: i64 = 180_000;
/// Covers a Clear waiting out an in-flight answer ([`CLEAR_WAIT_LIMIT`])
/// plus a cold start, so an `ask` queued behind a Clear doesn't give up.
const SINGLETON_LOCK_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(150 + CLEAR_WAIT_LIMIT.as_secs());
const SINGLETON_LOCK_POLL: Duration = Duration::from_millis(200);

/// Blocks until this process holds [`SINGLETON_LOCK`], or errors out after
/// [`SINGLETON_LOCK_ACQUIRE_TIMEOUT`]. Pass the returned holder id to
/// [`Store::release_fleet_lock`] when done — always, including on an `Err`
/// path, via a `let _ =` release rather than a dropped guard, since the
/// release itself is a fallible store call.
async fn acquire_singleton_lock(store: &Store) -> anyhow::Result<String> {
    let holder = Store::new_fleet_lock_holder();
    let deadline = tokio::time::Instant::now() + SINGLETON_LOCK_ACQUIRE_TIMEOUT;
    loop {
        let now = now_ms();
        if store.try_acquire_fleet_lock(SINGLETON_LOCK, &holder, now, now - SINGLETON_LOCK_STALE_MS)? {
            return Ok(holder);
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for another ask/clear on the prompt agent to finish"
        );
        tokio::time::sleep(SINGLETON_LOCK_POLL).await;
    }
}

/// `ninox prompt status` / the nav chip's live state.
#[derive(Debug, Clone, PartialEq)]
pub struct PromptStatus {
    pub session_id: String,
    /// A row exists in the store (it has been asked at least once).
    pub exists: bool,
    /// The pane is actually alive right now.
    pub running: bool,
    pub status: Option<SessionStatus>,
}

pub async fn status(store: &Store) -> anyhow::Result<PromptStatus> {
    status_impl(store, PROMPT_SESSION_ID).await
}

async fn status_impl(store: &Store, id: &str) -> anyhow::Result<PromptStatus> {
    let session = store.get_session(id)?;
    let running = runtime::has_session(id).await;
    Ok(PromptStatus {
        session_id: id.to_string(),
        exists: session.is_some(),
        running,
        status: session.map(|s| s.status),
    })
}

/// Deliver `text` to the prompt agent, starting it if this is the first
/// `ask` ever, or transparently restarting it (preferring `--resume`) if
/// its row exists but the pane died. Returns the session id delivered to.
pub async fn ask(store: Arc<Store>, config: &AppConfig, text: &str) -> anyhow::Result<String> {
    ask_impl(store, config, &AppConfig::sessions_dir(), &AppConfig::prompt_agent_workspace_dir(), PROMPT_SESSION_ID, text).await
}

async fn ask_impl(
    store: Arc<Store>, config: &AppConfig, sessions_dir: &Path, workspace_dir: &Path, id: &str, text: &str,
) -> anyhow::Result<String> {
    anyhow::ensure!(!text.trim().is_empty(), "prompt text must not be empty");
    let holder = acquire_singleton_lock(&store).await?;
    // Re-check under the lock: another process may have finished spawning
    // or restarting it between our caller's decision to call `ask` and this
    // lock actually being granted.
    let result = async {
        match store.get_session(id)? {
            None => spawn_fresh(Arc::clone(&store), config, sessions_dir, workspace_dir, id, text).await?,
            Some(_) if runtime::has_session(id).await => {
                deliver(&store, config, sessions_dir, id, text).await?;
            }
            Some(session) => {
                // `relaunch` already re-primes the role brief itself when it
                // falls back to a fresh start (see its doc comment) — once
                // it returns, the pane is ready for an ordinary delivery
                // either way.
                relaunch(Arc::clone(&store), config, sessions_dir, id, session, true).await?;
                deliver(&store, config, sessions_dir, id, text).await?;
            }
        }
        Ok::<(), anyhow::Error>(())
    }.await;
    let _ = store.release_fleet_lock(SINGLETON_LOCK, &holder);
    result?;
    Ok(id.to_string())
}

/// Reset the prompt agent: once any in-flight answer finishes, a fresh
/// restart (new `claude_session_id`, no `--resume`) that picks up the Ninox
/// type's current harness, model, permissions and brief.
pub async fn clear(store: Arc<Store>, config: &AppConfig) -> anyhow::Result<()> {
    clear_impl(store, config, &AppConfig::sessions_dir(), PROMPT_SESSION_ID).await
}

async fn clear_impl(store: Arc<Store>, config: &AppConfig, sessions_dir: &Path, id: &str) -> anyhow::Result<()> {
    let holder = acquire_singleton_lock(&store).await?;
    let result = async {
        if store.get_session(id)?.is_none() {
            anyhow::bail!("no prompt agent session yet — nothing to clear");
        }
        // The lease stays held while waiting, so a concurrent `ask` queues
        // behind the restart instead of spawning a second agent.
        wait_for_turn_end(&store, id, &holder).await;
        let session = store.get_session(id)?
            .ok_or_else(|| anyhow::anyhow!("the prompt agent session went away while clearing"))?;
        relaunch(Arc::clone(&store), config, sessions_dir, id, session, false).await?;
        Ok(())
    }.await;
    let _ = store.release_fleet_lock(SINGLETON_LOCK, &holder);
    result
}

/// The prompt agent's output since `cursor` (a byte offset into its pane
/// text previously returned by this function) — same source as `ninox
/// read`. Returns the new suffix and the cursor to pass next time.
pub async fn reply_since(cursor: usize) -> anyhow::Result<(String, usize)> {
    let screen = runtime::read_screen(PROMPT_SESSION_ID, None, false).await?;
    Ok(new_output_since(&screen, cursor))
}

/// Pure tail of [`reply_since`] — unit-testable without a live pane. A
/// cursor past the current screen (e.g. a `/clear` shrank it) is clamped to
/// the whole screen rather than panicking on an out-of-bounds slice.
fn new_output_since(screen: &str, cursor: usize) -> (String, usize) {
    let suffix = screen.get(cursor..).unwrap_or(screen);
    (suffix.to_string(), screen.len())
}

/// Where the drawer's transcript read for the current answer left off.
/// `path` is `None` until the agent's transcript exists (a first-ever `ask`
/// spawns the agent, and with it the file).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TranscriptCursor {
    pub path: Option<PathBuf>,
    pub offset: u64,
}

/// Assistant output appended to the transcript since a [`TranscriptCursor`].
/// `ended`: whether the last assistant entry read closed its turn (any
/// `stop_reason` but `tool_use`/`pause_turn`, which Claude Code continues
/// from); `None` when no assistant entry was read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TranscriptDelta {
    pub text_blocks: Vec<String>,
    pub ended: Option<bool>,
}

/// The prompt agent's Claude Code transcript directory. Claude Code's
/// on-screen rendering strips Markdown (fences, emphasis, link targets), so
/// the GUI drawer reads answers from here instead of [`reply_since`]'s pane
/// text — only possible for a claude-code agent with a known workspace.
pub fn transcript_dir_for(session: &Session) -> Option<PathBuf> {
    if session.agent_type != "claude-code" {
        return None;
    }
    let workspace = session.workspace_path.as_deref()?;
    Some(
        ninox_core::lifecycle::usage::claude_projects_dir()
            .join(ninox_core::lifecycle::usage::claude_project_slug(workspace)),
    )
}

/// `claude_session_id` is the row's spawn-time id, which `/clear` doesn't
/// update even though Claude Code then writes a fresh transcript — so a
/// different top-level file modified after both the id's file and the ask
/// wins. Only top-level `*.jsonl` files count: subagent transcripts live in
/// subdirectories or (older Claude Code) as top-level `agent-*.jsonl`.
pub fn pick_transcript(
    dir: &Path,
    claude_session_id: Option<&str>,
    asked_at: SystemTime,
) -> Option<PathBuf> {
    let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let newest = std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .filter(|p| {
            !p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("agent-"))
        })
        .filter_map(|p| mtime(&p).map(|t| (t, p)))
        .max_by_key(|(t, _)| *t);
    let id_file = claude_session_id
        .map(|id| dir.join(format!("{id}.jsonl")))
        .filter(|p| p.is_file());
    match (id_file, newest) {
        (Some(id_file), Some((newest_at, newest))) => {
            let rotated = newest != id_file
                && mtime(&id_file).is_some_and(|t| newest_at > t)
                && newest_at >= asked_at;
            Some(if rotated { newest } else { id_file })
        }
        (Some(id_file), None) => Some(id_file),
        (None, newest) => newest.map(|(_, p)| p),
    }
}

/// Reads whatever the transcript gained since `cursor`, restarting at the
/// top when [`pick_transcript`] now resolves to a different file. Entries
/// stamped before `asked_at_ms` are skipped, so a restart never replays an
/// earlier turn's answer. Unreadable or missing files read as "nothing new".
pub fn transcript_since(
    dir: &Path,
    claude_session_id: Option<&str>,
    cursor: &TranscriptCursor,
    asked_at_ms: i64,
) -> (TranscriptDelta, TranscriptCursor) {
    let asked_at = UNIX_EPOCH + Duration::from_millis(asked_at_ms.max(0) as u64);
    let Some(path) = pick_transcript(dir, claude_session_id, asked_at) else {
        return (TranscriptDelta::default(), cursor.clone());
    };
    let offset = if cursor.path.as_deref() == Some(path.as_path()) {
        cursor.offset
    } else {
        0
    };
    let Some(appended) = read_from(&path, offset) else {
        return (TranscriptDelta::default(), cursor.clone());
    };
    // Complete lines only, measured in raw bytes: lossy decoding can change
    // the length, and the cursor must stay a byte offset into the file.
    let complete = appended.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    let (delta, _) = parse_transcript_chunk(&String::from_utf8_lossy(&appended[..complete]), asked_at_ms);
    (
        delta,
        TranscriptCursor {
            path: Some(path),
            offset: offset + complete as u64,
        },
    )
}

/// The file's bytes from `offset` on, without re-reading what came before
/// (a live transcript reaches megabytes). An offset past the end — the
/// file was replaced — reads nothing.
fn read_from(path: &Path, offset: u64) -> Option<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    if offset > file.metadata().ok()?.len() {
        return Some(Vec::new());
    }
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut appended = Vec::new();
    file.read_to_end(&mut appended).ok()?;
    Some(appended)
}

/// Where a new turn's reads start: the end of the transcript the turn
/// would read right now, so earlier turns are never re-parsed.
pub fn transcript_baseline(dir: &Path, claude_session_id: Option<&str>, asked_at_ms: i64) -> TranscriptCursor {
    let asked_at = UNIX_EPOCH + Duration::from_millis(asked_at_ms.max(0) as u64);
    match pick_transcript(dir, claude_session_id, asked_at) {
        Some(path) => {
            let offset = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            TranscriptCursor { path: Some(path), offset }
        }
        None => TranscriptCursor::default(),
    }
}

/// Parses complete lines only, returning how many bytes were consumed so a
/// half-written trailing line is re-read next time.
fn parse_transcript_chunk(chunk: &str, asked_at_ms: i64) -> (TranscriptDelta, usize) {
    let consumed = chunk.rfind('\n').map_or(0, |i| i + 1);
    let mut delta = TranscriptDelta::default();
    for line in chunk[..consumed].lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if entry.get("type").and_then(Value::as_str) != Some("assistant")
            || entry.get("isSidechain").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        let stamped_before_ask = entry
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .is_some_and(|t| t.timestamp_millis() < asked_at_ms);
        if stamped_before_ask {
            continue;
        }
        let message = entry.get("message");
        let stop_reason = message
            .and_then(|m| m.get("stop_reason"))
            .and_then(Value::as_str);
        delta.ended = Some(stop_reason.is_some_and(|r| r != "tool_use" && r != "pause_turn"));
        let blocks = message
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array);
        for block in blocks.into_iter().flatten() {
            if block.get("type").and_then(Value::as_str) == Some("text") {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    delta.text_blocks.push(text.to_string());
                }
            }
        }
    }
    (delta, consumed)
}
/// Whether the transcript's last real turn entry leaves an answer pending:
/// a user message (or tool result) not yet answered, or an assistant entry
/// that continues (`tool_use`/`pause_turn`, or no `stop_reason` yet while it
/// streams). Slash-command echoes, meta entries and sidechains don't count.
fn tail_has_pending_turn(tail: &str) -> bool {
    for line in tail.lines().rev() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else { continue };
        if entry.get("isSidechain").and_then(Value::as_bool) == Some(true)
            || entry.get("isMeta").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        match entry.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                let stop = entry.get("message").and_then(|m| m.get("stop_reason")).and_then(Value::as_str);
                return !stop.is_some_and(|r| r != "tool_use" && r != "pause_turn");
            }
            Some("user") => {
                let content = entry.get("message").and_then(|m| m.get("content"));
                let text = match content {
                    Some(Value::String(t)) => t.clone(),
                    Some(Value::Array(blocks)) => blocks.iter()
                        .filter_map(|b| b.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => String::new(),
                };
                if text.contains("<command-name>") || text.contains("<local-command-stdout>") || text.starts_with("[Request interrupted") {
                    return false;
                }
                return true;
            }
            _ => continue,
        }
    }
    false
}

/// A pending turn whose transcript hasn't moved for this long is treated
/// as abandoned (a crashed or interrupted turn), not one to wait for.
/// Arbitrary; long enough for a slow tool call, bump if Clear cuts answers.
const TURN_STALL: Duration = Duration::from_secs(90);

/// Whether the prompt agent is mid-answer, read from its Claude Code
/// transcript. `false` for a harness without one (nothing to wait on).
pub fn turn_in_flight(session: &Session) -> bool {
    let Some(dir) = transcript_dir_for(session) else { return false };
    let Some(path) = pick_transcript(&dir, session.claude_session_id.as_deref(), UNIX_EPOCH) else { return false };
    let fresh = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .is_ok_and(|t| t.elapsed().unwrap_or_default() < TURN_STALL);
    if !fresh {
        return false;
    }
    let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let Some(tail) = read_from(&path, len.saturating_sub(256 * 1024)) else { return false };
    tail_has_pending_turn(&String::from_utf8_lossy(&tail))
}

/// Longest Clear waits for an in-flight answer before restarting anyway.
/// Arbitrary.
const CLEAR_WAIT_LIMIT: Duration = Duration::from_secs(300);

/// Waits while the agent is mid-answer, renewing `holder`'s lease so it
/// never goes stale and lets a concurrent `ask` in. Stops early once the
/// pane is gone.
async fn wait_for_turn_end(store: &Store, id: &str, holder: &str) {
    let started = std::time::Instant::now();
    while started.elapsed() < CLEAR_WAIT_LIMIT {
        let Ok(Some(session)) = store.get_session(id) else { return };
        if !runtime::has_session(id).await || !turn_in_flight(&session) {
            return;
        }
        let now = now_ms();
        let _ = store.try_acquire_fleet_lock(SINGLETON_LOCK, holder, now, now - SINGLETON_LOCK_STALE_MS);
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// The user-facing worker types and when to use each, for the brief — read
/// fresh at every start so a new or edited type is proposed correctly.
fn worker_type_guide(config: &AppConfig) -> String {
    let registry = config.worker_type_registry();
    ninox_core::worker_type::type_guide(&registry, &registry.user_facing_names())
}

fn prompt_spec(config: &AppConfig) -> anyhow::Result<WorkerTypeSpec> {
    config
        .worker_type_registry()
        .get(PROMPT_WORKER_TYPE)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("the built-in '{PROMPT_WORKER_TYPE}' worker type is missing from the registry"))
}

/// Seed/merge `spec`'s permission profile into `<workspace>/.claude/
/// settings.json`'s `permissions.allow`/`permissions.deny` arrays. Unlike a
/// typical `ninox spawn --type`, this session launches via `interactive_cmd`
/// — never `--dangerously-skip-permissions` — so Claude Code's own
/// permission check is live and these rules are genuinely enforced, not
/// merely descriptive data (and `deny` rules additionally hold even under a
/// future skip-permissions launch, per Claude Code's own precedence).
/// The `allow`/`deny`/`ask` lists are set to exactly the spec's (plus
/// `extra_deny`), so removing a rule from the Ninox type really revokes it
/// on the next start; every other key in an existing file is preserved.
/// Runs on every spawn/restart.
fn seed_permissions_settings(workspace_dir: &Path, spec: &WorkerTypeSpec, extra_deny: &[String]) -> anyhow::Result<()> {
    let mut rules = ninox_core::worker_type::seeded_permissions(&spec.permissions);
    rules.deny.extend(extra_deny.iter().cloned());
    ninox_core::claude_settings::set_permission_rules(&workspace_dir.join(".claude").join("settings.json"), &rules)
}

/// The live config file and its directory (the prompt agent's own
/// workspace's parent), wherever `NINOX_CONFIG` put them.
fn resolved_path_denies() -> Vec<String> {
    let config = AppConfig::config_path();
    let mut paths = vec![config.as_path()];
    if let Some(dir) = config.parent() {
        paths.push(dir);
    }
    ninox_core::worker_type::protected_path_bash_denies(&paths)
}

async fn deliver(store: &Store, config: &AppConfig, sessions_dir: &Path, id: &str, message: &str) -> anyhow::Result<()> {
    let (from, sender_pane) = messaging::resolve_interactive_sender(store).await;
    messaging::deliver_message(store, sessions_dir, id, message, config.send_mechanism(), Some(&from), sender_pane.as_ref(), ninox_core::types::MessageKind::Freeform)
        .await
        .map(|_| ())
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64
}

/// First-ever `ask`: no row exists yet. Spawns the singleton under `id` in
/// its own scratch workspace (never a git worktree — see
/// [`WorkerTypeSpec`]'s "no file edits outside its own scratch dir"
/// requirement) and delivers the role brief composed with `first_question`
/// as its very first message.
async fn spawn_fresh(
    store: Arc<Store>, config: &AppConfig, sessions_dir: &Path, workspace_dir: &Path, id: &str, first_question: &str,
) -> anyhow::Result<()> {
    let spec = prompt_spec(config)?;
    let agent = ninox_core::model_resolution::effective_agent(
        &ninox_core::model_resolution::SpawnModelRequest::worker(Some(&spec)), config,
    ).agent;
    config.registry().ensure_permissions_enforceable(&agent.harness, Some(&spec))?;
    std::fs::create_dir_all(workspace_dir)?;
    ninox_core::trust::seed_workspace_trust(workspace_dir)?;
    seed_permissions_settings(workspace_dir, &spec, &resolved_path_denies())?;
    let workspace = workspace_dir.to_string_lossy().to_string();

    let claude_session_id = ninox_core::harness::new_claude_session_id();
    let permission_mode = permission_mode(Interactive, Some(&spec));
    let base_cmd = config.registry().interactive_cmd(&agent, permission_mode, &claude_session_id);
    let brain = Some(ninox_core::brain_set::BrainSet::default_for(config));

    let engine = Engine::new(store.clone());
    let launched = launch_interactive_session(engine, InteractiveSpawnParams {
        session_id: id.to_string(),
        name: "Ninox prompt".to_string(),
        workspace,
        repo: String::new(),
        orchestrator_id: None,
        agent,
        base_cmd,
        brain,
        extra_env: Vec::new(),
        started_at: now_ms(),
        claude_session_id,
        failure_status: SessionStatus::Terminated,
        summary: Some("Ninox prompt agent".to_string()),
        permission_mode,
    }, true).await;
    launched.map_err(|cause| anyhow::anyhow!("failed to spawn the prompt agent: {cause}"))?;

    // `InteractiveSpawnParams` has no `worker_type` field (it only ever
    // carries a prior row's forward, and there is none yet) — stamp it now
    // that the row exists, so it's immediately excluded from fleet/island/
    // Board counts (see `fleet::snapshot::FleetSnapshot::load`).
    if let Some(mut s) = store.get_session(id)? {
        s.worker_type = Some(PROMPT_WORKER_TYPE.to_string());
        store.upsert_session(&s)?;
    }

    runtime::wait_for_input_prompt(id, PROMPT_TIMEOUT).await;
    deliver(&store, config, sessions_dir, id, &spec.compose_prompt_with_guide(first_question, &worker_type_guide(config))?).await
}

/// Relaunch an existing row under its own id: `prefer_resume` picks
/// `--resume` with the stored `claude_session_id` when the harness
/// supports it (a crash recovery — conversation survives), otherwise (or
/// when `prefer_resume` is `false`, i.e. a deliberate `clear`) starts fresh
/// with a freshly minted id. A fresh start has no conversation at all —
/// including the role brief, which was only ever delivered as the first
/// chat message, not a persistent system prompt — so this re-primes it
/// immediately rather than leaving the agent blind to its own role until
/// the next `ask`. Returns whether it actually resumed.
struct RelaunchCmd {
    agent: AgentConfig,
    base_cmd: String,
    claude_session_id: String,
    resumed: bool,
    permission_mode: ninox_core::harness::PermissionMode,
}

/// The command a relaunch runs. Same mode function and inputs as
/// `spawn_fresh`, so a crash-resume never runs wider than the first launch.
/// A resume keeps the conversation and the model it runs on; a fresh start
/// (Clear context, or a harness that can't resume) picks up the Ninox
/// type's current harness/model.
fn relaunch_cmd(config: &AppConfig, spec: &WorkerTypeSpec, session: &Session, prefer_resume: bool) -> RelaunchCmd {
    let permission_mode = permission_mode(Interactive, Some(spec));
    let registry = config.registry();
    let row_agent = ninox_core::model_resolution::relaunch_agent(
        &session.agent_type, session.model.as_deref(), ninox_core::model_resolution::Role::Worker, Some(spec), config,
    );
    match session.claude_session_id.clone().filter(|_| prefer_resume && registry.can_resume(&row_agent)) {
        Some(claude_session_id) => RelaunchCmd {
            base_cmd: registry.resume_cmd(&row_agent, permission_mode, &claude_session_id).expect("checked by can_resume"),
            agent: row_agent,
            claude_session_id,
            resumed: true,
            permission_mode,
        },
        None => {
            let agent = ninox_core::model_resolution::effective_agent(
                &ninox_core::model_resolution::SpawnModelRequest::worker(Some(spec)), config,
            ).agent;
            let claude_session_id = ninox_core::harness::new_claude_session_id();
            RelaunchCmd {
                base_cmd: registry.interactive_cmd(&agent, permission_mode, &claude_session_id),
                agent,
                claude_session_id,
                resumed: false,
                permission_mode,
            }
        }
    }
}

async fn relaunch(
    store: Arc<Store>, config: &AppConfig, sessions_dir: &Path, id: &str, session: Session, prefer_resume: bool,
) -> anyhow::Result<bool> {
    let _ = runtime::kill_session(id).await;
    let workspace = session.workspace_path.clone()
        .unwrap_or_else(|| AppConfig::prompt_agent_workspace_dir().to_string_lossy().to_string());
    // `is_orchestrator: true` only selects this helper's plain-directory
    // recreation path (never worktree recreation) if the dir went missing
    // — the prompt agent's scratch dir carries no git semantics either way.
    ensure_session_workspace(&workspace, id, true, config, &session.agent_type).await?;
    // Re-seed on every restart too: heals a workspace whose settings.json
    // was recreated from scratch above, and re-applies the profile's rules
    // if the registry's `prompt` spec changed since the last launch.
    let spec = prompt_spec(config)?;
    config.registry().ensure_permissions_enforceable(&session.agent_type, Some(&spec))?;
    seed_permissions_settings(Path::new(&workspace), &spec, &resolved_path_denies())?;

    let RelaunchCmd { agent, base_cmd, claude_session_id, resumed: can_resume, permission_mode } =
        relaunch_cmd(config, &spec, &session, prefer_resume);
    let brain = Some(ninox_core::brain_set::BrainSet::from_session(&session, config));

    let engine = Engine::new(store.clone());
    let launched = launch_interactive_session(engine, InteractiveSpawnParams {
        session_id: id.to_string(),
        name: session.name.clone(),
        workspace,
        repo: String::new(),
        orchestrator_id: None,
        agent,
        base_cmd,
        brain,
        extra_env: Vec::new(),
        started_at: now_ms(),
        claude_session_id,
        failure_status: SessionStatus::Terminated,
        summary: session.summary.clone(),
        permission_mode,
    }, true).await;
    launched.map_err(|cause| anyhow::anyhow!("failed to restart the prompt agent: {cause}"))?;

    if let Some(mut s) = store.get_session(id)? {
        s.worker_type = Some(PROMPT_WORKER_TYPE.to_string());
        store.upsert_session(&s)?;
    }

    runtime::wait_for_input_prompt(id, PROMPT_TIMEOUT).await;
    if !can_resume {
        let spec = prompt_spec(config)?;
        let reprime = spec.compose_prompt_with_guide("(reconnected after a restart — no pending question yet)", &worker_type_guide(config))?;
        deliver(&store, config, sessions_dir, id, &reprime).await?;
    }
    Ok(can_resume)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::harness::HarnessSpec;

    mod transcript {
        use super::super::*;
        use std::time::{Duration, SystemTime};

        fn assistant(text: &str, stop: &str, ts: &str) -> String {
            serde_json::json!({
                "type": "assistant", "timestamp": ts,
                "message": {"stop_reason": stop, "content": [{"type": "text", "text": text}]},
            })
            .to_string()
                + "\n"
        }

        fn tool_use(ts: &str) -> String {
            serde_json::json!({
                "type": "assistant", "timestamp": ts,
                "message": {"stop_reason": "tool_use", "content": [{"type": "tool_use", "name": "Bash", "input": {}}]},
            }).to_string() + "\n"
        }

        fn set_mtime(path: &Path, at: SystemTime) {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(at)
                .unwrap();
        }

        const T0: &str = "2026-10-30T10:00:00.000Z";
        const T1: &str = "2026-10-30T10:05:00.000Z";

        fn ms(ts: &str) -> i64 {
            chrono::DateTime::parse_from_rfc3339(ts)
                .unwrap()
                .timestamp_millis()
        }

        #[test]
        fn parse_collects_text_blocks_and_end_turn() {
            let chunk = tool_use(T1)
                + &assistant("first", "tool_use", T1)
                + &assistant("**done**", "end_turn", T1);
            let (delta, consumed) = parse_transcript_chunk(&chunk, 0);
            assert_eq!(
                delta.text_blocks,
                vec!["first".to_string(), "**done**".to_string()]
            );
            assert_eq!(delta.ended, Some(true));
            assert_eq!(consumed, chunk.len());
        }

        #[test]
        fn parse_leaves_a_partial_trailing_line_for_the_next_read() {
            let full = assistant("whole", "tool_use", T1);
            let chunk = full.clone() + "{\"type\":\"assist";
            let (delta, consumed) = parse_transcript_chunk(&chunk, 0);
            assert_eq!(delta.text_blocks, vec!["whole".to_string()]);
            assert_eq!(delta.ended, Some(false));
            assert_eq!(consumed, full.len());
        }

        #[test]
        fn parse_skips_sidechain_entries() {
            let side = serde_json::json!({
                "type": "assistant", "isSidechain": true, "timestamp": T1,
                "message": {"stop_reason": "end_turn", "content": [{"type": "text", "text": "subagent chatter"}]},
            }).to_string() + "\n";
            let (delta, _) = parse_transcript_chunk(&side, 0);
            assert!(delta.text_blocks.is_empty());
            assert_eq!(delta.ended, None);
        }

        #[test]
        fn parse_ignores_entries_older_than_the_ask() {
            let chunk =
                assistant("old answer", "end_turn", T0) + &assistant("new answer", "end_turn", T1);
            let (delta, _) = parse_transcript_chunk(&chunk, ms(T1) - 1);
            assert_eq!(delta.text_blocks, vec!["new answer".to_string()]);
        }

        #[test]
        fn pick_prefers_the_rows_session_id_over_a_newer_unrelated_file() {
            let dir = tempfile::tempdir().unwrap();
            let id_file = dir.path().join("abc.jsonl");
            let other = dir.path().join("zzz.jsonl");
            std::fs::write(&id_file, "").unwrap();
            std::fs::write(&other, "").unwrap();
            let asked = SystemTime::now();
            set_mtime(&id_file, asked - Duration::from_secs(60));
            set_mtime(&other, asked - Duration::from_secs(30));
            assert_eq!(
                pick_transcript(dir.path(), Some("abc"), asked),
                Some(id_file)
            );
        }

        #[test]
        fn pick_follows_a_rotated_transcript_written_after_the_ask() {
            let dir = tempfile::tempdir().unwrap();
            let id_file = dir.path().join("abc.jsonl");
            let rotated = dir.path().join("def.jsonl");
            std::fs::write(&id_file, "").unwrap();
            std::fs::write(&rotated, "").unwrap();
            let asked = SystemTime::now() - Duration::from_secs(10);
            set_mtime(&id_file, asked - Duration::from_secs(60));
            set_mtime(&rotated, asked + Duration::from_secs(5));
            assert_eq!(
                pick_transcript(dir.path(), Some("abc"), asked),
                Some(rotated)
            );
        }

        #[test]
        fn pick_falls_back_to_the_newest_top_level_file_without_an_id() {
            let dir = tempfile::tempdir().unwrap();
            let old = dir.path().join("old.jsonl");
            let new = dir.path().join("new.jsonl");
            let sub = dir.path().join("new").join("subagents");
            std::fs::create_dir_all(&sub).unwrap();
            std::fs::write(sub.join("agent-1.jsonl"), "").unwrap();
            std::fs::write(dir.path().join("agent-2.jsonl"), "").unwrap();
            std::fs::write(&old, "").unwrap();
            std::fs::write(&new, "").unwrap();
            let now = SystemTime::now();
            set_mtime(&old, now - Duration::from_secs(60));
            set_mtime(&new, now - Duration::from_secs(30));
            set_mtime(&dir.path().join("agent-2.jsonl"), now);
            assert_eq!(pick_transcript(dir.path(), None, now), Some(new));
        }

        #[test]
        fn transcript_since_advances_the_cursor_and_restarts_on_a_new_file() {
            let _iso = ninox_core::test_isolation::isolate();
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("abc.jsonl");
            std::fs::write(&file, assistant("one", "tool_use", T1)).unwrap();
            let asked_ms = ms(T1) - 1;
            let (delta, cursor) = transcript_since(
                dir.path(),
                Some("abc"),
                &TranscriptCursor::default(),
                asked_ms,
            );
            assert_eq!(delta.text_blocks, vec!["one".to_string()]);
            assert_eq!(cursor.path.as_deref(), Some(file.as_path()));

            let mut f = std::fs::File::options().append(true).open(&file).unwrap();
            std::io::Write::write_all(&mut f, assistant("two", "end_turn", T1).as_bytes()).unwrap();
            let (delta, cursor2) = transcript_since(dir.path(), Some("abc"), &cursor, asked_ms);
            assert_eq!(delta.text_blocks, vec!["two".to_string()]);
            assert_eq!(delta.ended, Some(true));

            let (delta, _) = transcript_since(dir.path(), Some("abc"), &cursor2, asked_ms);
            assert_eq!(
                delta,
                TranscriptDelta::default(),
                "nothing new since the last read"
            );
        }

        #[test]
        fn non_tool_stop_reasons_end_the_turn() {
            for (reason, ended) in [("stop_sequence", true), ("max_tokens", true), ("refusal", true), ("tool_use", false), ("pause_turn", false)] {
                let (delta, _) = parse_transcript_chunk(&assistant("x", reason, T1), 0);
                assert_eq!(delta.ended, Some(ended), "{reason}");
            }
        }

        #[test]
        fn baseline_skips_everything_already_in_the_transcript() {
            let _iso = ninox_core::test_isolation::isolate();
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("abc.jsonl");
            std::fs::write(&file, assistant("earlier turn", "end_turn", T1)).unwrap();
            let cursor = transcript_baseline(dir.path(), Some("abc"), ms(T0));
            assert_eq!(cursor.offset, std::fs::metadata(&file).unwrap().len());
            let (delta, _) = transcript_since(dir.path(), Some("abc"), &cursor, ms(T0));
            assert!(delta.text_blocks.is_empty());
        }

        #[test]
        fn invalid_utf8_never_skews_the_byte_cursor() {
            let _iso = ninox_core::test_isolation::isolate();
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("abc.jsonl");
            let mut bytes = b"\xff\xfe not json\n".to_vec();
            bytes.extend(assistant("ok", "end_turn", T1).as_bytes());
            std::fs::write(&file, &bytes).unwrap();
            let (delta, cursor) = transcript_since(dir.path(), Some("abc"), &TranscriptCursor::default(), 0);
            assert_eq!(delta.text_blocks, vec!["ok".to_string()]);
            assert_eq!(cursor.offset, bytes.len() as u64);
        }

        #[test]
        fn transcript_dir_is_none_for_a_non_claude_harness() {
            let _iso = ninox_core::test_isolation::isolate();
            let mut s = super::session_with(PROMPT_SESSION_ID);
            s.agent_type = "codex".into();
            s.workspace_path = Some("/tmp/ws".into());
            assert_eq!(transcript_dir_for(&s), None);
            s.agent_type = "claude-code".into();
            assert!(transcript_dir_for(&s).unwrap().ends_with("-tmp-ws"));
        }
    }

    #[test]
    fn new_output_since_returns_only_the_suffix() {
        assert_eq!(new_output_since("hello world", 5), (" world".to_string(), 11));
        assert_eq!(new_output_since("hello", 0), ("hello".to_string(), 5));
        assert_eq!(new_output_since("hello", 5), (String::new(), 5));
    }

    #[test]
    fn new_output_since_clamps_a_cursor_past_a_shrunk_screen() {
        // e.g. a `/clear` wiped the pane between two reply_since calls.
        assert_eq!(new_output_since("hi", 50), ("hi".to_string(), 2));
    }

    /// The MLOPS-5416 regression: a crash-resume used to launch with
    /// `--dangerously-skip-permissions`, leaving only the deny rules in force.
    #[test]
    fn relaunch_with_prefer_resume_never_skips_permissions() {
        let config = AppConfig::default();
        let spec = prompt_spec(&config).unwrap();
        let agent = AgentConfig { harness: "claude-code".into(), model: Some("m".into()) };
        let mut session = crate::session_queue::base_session(
            ninox_core::worker_type::PROMPT_SESSION_ID, "Ninox prompt", &agent, 0,
        );
        session.worker_type = Some(PROMPT_WORKER_TYPE.into());
        session.claude_session_id = Some("conv-1".into());
        let skip = ninox_core::harness::CLAUDE_SKIP_PERMISSIONS_FLAG;

        let resumed = relaunch_cmd(&config, &spec, &session, true);
        assert!(resumed.resumed);
        assert_eq!(resumed.base_cmd, "claude --resume 'conv-1' --model 'm' --settings '{\"crossSessionInbound\":\"accept\"}'");
        assert_eq!(resumed.permission_mode, ninox_core::harness::PermissionMode::Standard);

        let fresh = relaunch_cmd(&config, &spec, &session, false);
        assert!(!fresh.resumed && !fresh.base_cmd.contains(skip), "{}", fresh.base_cmd);
    }

    /// A user config overriding the `prompt` type with enforcement off still
    /// can't make the prompt agent resume with the bypass flag: it is an
    /// interactive session either way.
    #[test]
    fn relaunch_never_skips_even_if_the_prompt_type_is_unenforced() {
        let config = AppConfig::default();
        let mut spec = prompt_spec(&config).unwrap();
        spec.permissions.enabled = false;
        let agent = AgentConfig { harness: "claude-code".into(), model: None };
        let mut session = crate::session_queue::base_session("p", "p", &agent, 0);
        session.claude_session_id = Some("conv-1".into());
        let cmd = relaunch_cmd(&config, &spec, &session, true).base_cmd;
        assert!(!cmd.contains(ninox_core::harness::CLAUDE_SKIP_PERMISSIONS_FLAG), "{cmd}");
    }

    #[test]
    fn seed_permissions_settings_adds_the_resolved_path_denies() {
        let dir = tempfile::tempdir().unwrap();
        let spec = WorkerTypeSpec::default();
        let extra = ninox_core::worker_type::protected_path_bash_denies(&[Path::new("/x/ninox")]);
        seed_permissions_settings(dir.path(), &spec, &extra).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(dir.path().join(".claude/settings.json")).unwrap()).unwrap();
        assert!(v["permissions"]["deny"].as_array().unwrap().contains(&Value::String("Bash(rm */x/ninox*)".into())));
    }

    #[test]
    fn seed_permissions_settings_writes_the_deny_and_allow_rules() {
        let dir = tempfile::tempdir().unwrap();
        let spec = WorkerTypeSpec {
            permissions: ninox_core::worker_type::WorkerPermissionsConfig {
                enabled: true,
                allow: vec!["Bash(ninox list:*)".into()],
                deny: vec!["Bash(ninox spawn:*)".into(), "Bash(ninox reap:*)".into()],
                ask: vec![],
            },
            ..WorkerTypeSpec::default()
        };
        seed_permissions_settings(dir.path(), &spec, &[]).unwrap();
        let raw = std::fs::read_to_string(dir.path().join(".claude/settings.json")).unwrap();
        let v: Value = serde_json::from_str(&raw).unwrap();
        let deny = v["permissions"]["deny"].as_array().unwrap();
        assert!(deny.contains(&Value::String("Bash(ninox spawn:*)".into())));
        assert!(deny.contains(&Value::String("Bash(ninox reap:*)".into())));
        let allow = v["permissions"]["allow"].as_array().unwrap();
        assert!(allow.contains(&Value::String("Bash(ninox list:*)".into())));
    }

    #[test]
    fn seed_permissions_settings_sets_exact_rules_and_keeps_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
        std::fs::write(
            dir.path().join(".claude/settings.json"),
            serde_json::json!({
                "permissions": {"allow": ["Bash(*)"], "deny": ["Bash(rm:*)"], "defaultMode": "default"},
                "statusLine": {"type": "command", "command": "something unrelated"},
            }).to_string(),
        ).unwrap();
        let spec = WorkerTypeSpec {
            permissions: ninox_core::worker_type::WorkerPermissionsConfig {
                enabled: true,
                allow: vec!["Bash(ninox list:*)".into()],
                deny: vec!["Bash(ninox spawn:*)".into()],
                ask: vec!["Bash(ninox send:*)".into()],
            },
            ..WorkerTypeSpec::default()
        };
        seed_permissions_settings(dir.path(), &spec, &[]).unwrap();
        seed_permissions_settings(dir.path(), &spec, &[]).unwrap();

        let raw = std::fs::read_to_string(dir.path().join(".claude/settings.json")).unwrap();
        let v: Value = serde_json::from_str(&raw).unwrap();
        // A rule dropped from the Ninox type is really revoked.
        assert_eq!(v["permissions"]["allow"], serde_json::json!(["Bash(ninox list:*)"]));
        let mut want_deny = vec!["Bash(ninox spawn:*)".to_string()];
        want_deny.extend(ninox_core::worker_type::settings_self_protection_denies());
        assert_eq!(v["permissions"]["deny"], serde_json::json!(want_deny), "the type's denies plus self-protection");
        assert_eq!(v["permissions"]["ask"], serde_json::json!(["Bash(ninox send:*)"]));
        assert_eq!(v["permissions"]["defaultMode"], "default");
        assert_eq!(v["statusLine"]["command"], "something unrelated");
    }

    fn store() -> Arc<Store> {
        Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap())
    }

    #[tokio::test]
    async fn status_reports_absent_when_never_asked() {
        let st = status_impl(&store(), "prompt-agent-test-absent").await.unwrap();
        assert!(!st.exists);
        assert!(!st.running);
        assert_eq!(st.status, None);
    }

    #[tokio::test]
    async fn clear_without_a_session_errors_clearly() {
        let err = clear_impl(store(), &AppConfig::default(), &std::env::temp_dir(), "prompt-agent-test-clear-none")
            .await.unwrap_err();
        assert!(err.to_string().contains("nothing to clear"), "{err}");
    }

    #[tokio::test]
    async fn ask_rejects_blank_text() {
        let err = ask_impl(
            store(), &AppConfig::default(), &std::env::temp_dir(), &std::env::temp_dir(),
            "prompt-agent-test-blank", "   ",
        ).await.unwrap_err();
        assert!(err.to_string().contains("empty"), "{err}");
    }

    /// The pane's recent text once it shows `needle`, or whatever it shows
    /// after the (arbitrary) deadline. The built-in brief is long, so the
    /// text after it can take a moment to arrive.
    async fn screen_with(id: &str, needle: &str) -> String {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            let screen = ninox_core::runtime::read_screen(id, Some(300), false).await.unwrap_or_default();
            if screen.contains(needle) || std::time::Instant::now() > deadline {
                return screen;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }

    /// The built-in brief's opening words — the first thing a delivered
    /// brief puts on screen.
    const BRIEF_OPENING: &str = "You are the Ninox prompt agent";

    /// A stand-in harness that draws a `❯` prompt and echoes input, mirroring
    /// `restart.rs`'s own end-to-end test pattern — no real `claude` binary
    /// needed.
    /// Stands in for claude-code itself: the Ninox type always runs there.
    fn fake_harness_config(_label: &str) -> AppConfig {
        let mut cfg = AppConfig::default();
        cfg.harnesses.insert("claude-code".to_string(), HarnessSpec {
            enabled: true,
            binary: Some("sh".into()),
            // `-icanon`: the delivered brief is one line longer than the
            // tty's 1024-byte canonical input buffer, which would block
            // everything typed after it.
            interactive_args: vec!["-c".into(), "'printf \"❯ \\n\"; stty -icanon; exec cat'".into()],
            resume_args: vec!["-c".into(), "'printf \"❯ resumed\\n\"; stty -icanon; exec cat'".into()],
            ..Default::default()
        });
        cfg.upsert_worker_type(WorkerTypeSpec {
            name: PROMPT_WORKER_TYPE.into(),
            brief_addendum: Some("role brief:".into()),
            ..WorkerTypeSpec::default()
        });
        cfg
    }

    #[tokio::test]
    async fn a_failed_spawn_reports_its_cause_not_just_a_log_pointer() {
        let _iso = ninox_core::test_isolation::isolate();
        let id = format!("prompt-agent-test-spawn-fail-{}", std::process::id());
        let sessions_dir = tempfile::tempdir().unwrap();
        let workspace_dir = tempfile::tempdir().unwrap();
        // A live pane under the same id makes `create_session` fail
        // deterministically ("duplicate session").
        ninox_core::tmux::create_session(&id, &workspace_dir.path().to_string_lossy(), "sleep 30", &[]).await.unwrap();

        let err = ask_impl(store(), &fake_harness_config("prompt-fake-spawn-fail"), sessions_dir.path(), workspace_dir.path(), &id, "hi")
            .await
            .unwrap_err()
            .to_string();
        let _ = ninox_core::tmux::kill_session(&id).await;
        let cause = err.strip_prefix("failed to spawn the prompt agent: ").unwrap_or_else(|| panic!("{err}"));
        assert!(!cause.is_empty() && !cause.contains("see the ninox log"), "{err}");
    }

    #[tokio::test]
    async fn ask_spawns_the_singleton_on_first_call_and_delivers_the_role_brief() {
        let _iso = ninox_core::test_isolation::isolate();
        let id = format!("prompt-agent-test-spawn-{}", std::process::id());
        let st = store();
        let sessions_dir = tempfile::tempdir().unwrap();
        let workspace_dir = tempfile::tempdir().unwrap();
        let config = fake_harness_config("prompt-fake-spawn");

        let returned = ask_impl(st.clone(), &config, sessions_dir.path(), workspace_dir.path(), &id, "what's blocking X?")
            .await.unwrap();
        assert_eq!(returned, id);
        let screen = screen_with(&id, BRIEF_OPENING).await;
        let _ = ninox_core::runtime::kill_session(&id).await;

        let session = st.get_session(&id).unwrap().expect("session row created");
        assert_eq!(session.worker_type.as_deref(), Some(PROMPT_WORKER_TYPE));
        assert!(screen.contains(BRIEF_OPENING), "{screen}");
    }

    #[tokio::test]
    async fn fresh_prompt_agent_runs_the_worker_setting_then_its_own_type_model() {
        let _iso = ninox_core::test_isolation::isolate();
        let echo = "'printf \"❯ \\n\"; echo \"argv:$*\"; stty -icanon; exec cat'";
        for (type_model, want) in [(None, "--model worker-m"), (Some("ninox-m"), "--model ninox-m")] {
            let id = format!("prompt-agent-test-model-{}-{}", type_model.is_some(), std::process::id());
            let mut config = fake_harness_config("prompt-fake-model");
            config.harnesses.get_mut("claude-code").unwrap().interactive_args =
                vec!["-c".into(), echo.into(), "x".into(), "--model".into(), "{model}".into()];
            config.worker = AgentConfig { harness: "claude-code".into(), model: Some("worker-m".into()) };
            let mut spec = config.worker_type_registry().get(PROMPT_WORKER_TYPE).unwrap().clone();
            spec.model = type_model.map(Into::into);
            config.upsert_worker_type(spec);
            let sessions_dir = tempfile::tempdir().unwrap();
            let workspace_dir = tempfile::tempdir().unwrap();
            ask_impl(store(), &config, sessions_dir.path(), workspace_dir.path(), &id, "hi").await.unwrap();
            // The argv line is printed first; on a narrow pane the long brief
            // that follows wraps far enough to push it out of a short read.
            let screen = screen_with(&id, want).await;
            let _ = ninox_core::runtime::kill_session(&id).await;
            assert!(screen.contains(want), "want {want}: {screen}");
        }
    }

    /// Mirrors `orchestrator_auth`'s own `session_with` test fixture: a live
    /// (non-terminal) session whose recorded root pid is `std::process::
    /// id()`, so `resolve_live_caller_session_id`'s ancestry fallback finds
    /// it (this test process IS, for the test's purposes, that session).
    fn session_with(id: &str) -> ninox_core::types::Session {
        ninox_core::types::Session {
            id: id.to_string(), orchestrator_id: None, name: id.to_string(), repo: String::new(),
            status: SessionStatus::Working, agent_type: "claude-code".to_string(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: Some(std::process::id()),
            model: None, observed_model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None, worker_type: None,
            success_criteria: Vec::new(), completion_outcome: None, self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        }
    }

    /// FEEDBACK-v1-tryout-round1.md's T3 follow-up: a prompt-originated send
    /// must read as from the user, not "could not be verified" — the common
    /// case, a bare test/CI process with no managed ninox ancestor.
    #[tokio::test]
    async fn deliver_attributes_a_caller_with_no_managed_ancestor_to_the_human_sender() {
        let _iso = ninox_core::test_isolation::isolate();
        let id = format!("prompt-agent-test-deliver-human-{}", std::process::id());
        let st = store();
        let sessions_dir = tempfile::tempdir().unwrap();
        let workspace_dir = tempfile::tempdir().unwrap();
        let config = fake_harness_config("prompt-fake-deliver-human");
        ask_impl(st.clone(), &config, sessions_dir.path(), workspace_dir.path(), &id, "hello").await.unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;

        deliver(&st, &config, sessions_dir.path(), &id, "what's blocking the rewrite?").await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let screen = ninox_core::runtime::read_screen(&id, Some(50), false).await.unwrap_or_default();
        let _ = ninox_core::runtime::kill_session(&id).await;

        assert!(screen.contains("[ninox: from the user, via the ninox prompt]"), "{screen}");
    }

    /// The security fix itself: a worker that stripped its own pane-id env
    /// var (so the pane-identity fast path misses, same as a bare human)
    /// must NOT be stamped as the trusted user just because of that —
    /// `resolve_live_caller_session_id`'s ancestry fallback must still find
    /// it and attribute to its real session instead.
    #[tokio::test]
    async fn deliver_does_not_stamp_human_sender_when_a_live_session_is_an_ancestor() {
        let _iso = ninox_core::test_isolation::isolate();
        let id = format!("prompt-agent-test-deliver-ancestor-{}", std::process::id());
        let st = store();
        let sessions_dir = tempfile::tempdir().unwrap();
        let workspace_dir = tempfile::tempdir().unwrap();
        let config = fake_harness_config("prompt-fake-deliver-ancestor");
        ask_impl(st.clone(), &config, sessions_dir.path(), workspace_dir.path(), &id, "hello").await.unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        st.upsert_session(&session_with("impersonating-worker")).unwrap();

        deliver(&st, &config, sessions_dir.path(), &id, "sneaky message").await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let screen = ninox_core::runtime::read_screen(&id, Some(50), false).await.unwrap_or_default();
        let _ = ninox_core::runtime::kill_session(&id).await;

        // The pane also carries the real "hello" role-brief delivered
        // before `impersonating-worker` was registered — that one
        // genuinely has no managed ancestor yet, so it correctly is
        // attributed to the human sender; only the *second* delivery, sent
        // after registering an ancestor session, must not be.
        let sneaky_line = screen.lines().find(|l| l.contains("sneaky message")).unwrap_or_default();
        assert!(!sneaky_line.contains("from the user, via the ninox prompt"), "{screen}");
    }

    #[tokio::test]
    async fn ask_seeds_every_deny_rule_from_the_real_prompt_worker_type_into_claude_settings() {
        let _iso = ninox_core::test_isolation::isolate();
        let id = format!("prompt-agent-test-deny-seed-{}", std::process::id());
        let st = store();
        let sessions_dir = tempfile::tempdir().unwrap();
        let workspace_dir = tempfile::tempdir().unwrap();
        // The real, registered `prompt` worker type (its actual deny list),
        // just routed at the fake shell instead of a real `claude` binary.
        let mut config = AppConfig::default();
        config.harnesses.insert("claude-code".to_string(), HarnessSpec {
            enabled: true,
            binary: Some("sh".into()),
            interactive_args: vec!["-c".into(), "'printf \"❯ \\n\"; stty -icanon; exec cat'".into()],
            ..Default::default()
        });
        let mut expected_deny = prompt_spec(&config).unwrap().permissions.deny;
        // The agent must not be able to rewrite the file its rules live in.
        expected_deny.extend(ninox_core::worker_type::settings_self_protection_denies());
        assert!(!expected_deny.is_empty(), "precondition: the real prompt type has deny rules");

        ask_impl(st.clone(), &config, sessions_dir.path(), workspace_dir.path(), &id, "hello").await.unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        let _ = ninox_core::runtime::kill_session(&id).await;

        let raw = std::fs::read_to_string(workspace_dir.path().join(".claude/settings.json")).unwrap();
        let v: Value = serde_json::from_str(&raw).unwrap();
        let deny: Vec<String> = v["permissions"]["deny"].as_array().unwrap()
            .iter().map(|r| r.as_str().unwrap().to_string()).collect();
        for rule in &expected_deny {
            assert!(deny.contains(rule), "missing deny rule {rule:?} in {deny:?}");
        }
    }

    #[tokio::test]
    async fn ask_reuses_the_live_singleton_instead_of_respawning() {
        let _iso = ninox_core::test_isolation::isolate();
        let id = format!("prompt-agent-test-reuse-{}", std::process::id());
        let st = store();
        let sessions_dir = tempfile::tempdir().unwrap();
        let workspace_dir = tempfile::tempdir().unwrap();
        let config = fake_harness_config("prompt-fake-reuse");

        ask_impl(st.clone(), &config, sessions_dir.path(), workspace_dir.path(), &id, "first question").await.unwrap();
        let screen_after_first = screen_with(&id, "first question").await;
        let first_pid = ninox_core::runtime::session_pid(&id).await;
        let role_brief_count_after_first = screen_after_first.matches(BRIEF_OPENING).count();
        assert!(role_brief_count_after_first > 0, "{screen_after_first}");

        ask_impl(st.clone(), &config, sessions_dir.path(), workspace_dir.path(), &id, "second question").await.unwrap();
        let screen = screen_with(&id, "second question").await;
        let second_pid = ninox_core::runtime::session_pid(&id).await;
        let _ = ninox_core::runtime::kill_session(&id).await;

        assert_eq!(first_pid, second_pid, "the same pane must be reused, not respawned");
        assert!(screen.contains("second question"), "{screen}");
        // The role brief is only ever composed once, for the very first
        // delivery to a freshly spawned agent — a reused `ask` must not add
        // any further occurrences of it.
        assert_eq!(
            screen.matches(BRIEF_OPENING).count(), role_brief_count_after_first,
            "a reused ask must not re-deliver the role brief: {screen}"
        );
    }

    #[tokio::test]
    async fn ask_restarts_a_dead_singleton_preferring_resume() {
        let _iso = ninox_core::test_isolation::isolate();
        let id = format!("prompt-agent-test-restart-{}", std::process::id());
        let st = store();
        let sessions_dir = tempfile::tempdir().unwrap();
        let workspace_dir = tempfile::tempdir().unwrap();
        let config = fake_harness_config("prompt-fake-restart");

        ask_impl(st.clone(), &config, sessions_dir.path(), workspace_dir.path(), &id, "first question").await.unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        ninox_core::runtime::kill_session(&id).await.unwrap();
        assert!(!ninox_core::runtime::has_session(&id).await, "precondition: pane is dead");

        ask_impl(st.clone(), &config, sessions_dir.path(), workspace_dir.path(), &id, "after the crash").await.unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        let alive = ninox_core::runtime::has_session(&id).await;
        let screen = ninox_core::runtime::read_screen(&id, Some(50), false).await.unwrap_or_default();
        let _ = ninox_core::runtime::kill_session(&id).await;

        assert!(alive, "session is live again after the transparent restart");
        // The fake harness's resume_args print a distinct "resumed" banner —
        // confirms `--resume` was chosen over a fresh `interactive_args` start.
        assert!(screen.contains("resumed"), "restart must prefer --resume: {screen}");
        assert!(screen.contains("after the crash"), "{screen}");
    }

    #[tokio::test]
    async fn ask_restarts_fresh_and_reprimes_the_role_brief_when_the_harness_cannot_resume() {
        let _iso = ninox_core::test_isolation::isolate();
        let id = format!("prompt-agent-test-restart-fresh-{}", std::process::id());
        let st = store();
        let sessions_dir = tempfile::tempdir().unwrap();
        let workspace_dir = tempfile::tempdir().unwrap();
        let mut config = fake_harness_config("prompt-fake-no-resume");
        // No `resume_args` at all: `relaunch` must fall back to a fresh
        // start even though `ask` always passes `prefer_resume: true`.
        config.harnesses.get_mut("claude-code").unwrap().resume_args = vec![];

        ask_impl(st.clone(), &config, sessions_dir.path(), workspace_dir.path(), &id, "first question").await.unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        ninox_core::runtime::kill_session(&id).await.unwrap();

        ask_impl(st.clone(), &config, sessions_dir.path(), workspace_dir.path(), &id, "after the crash").await.unwrap();
        let screen = screen_with(&id, "after the crash").await;
        let _ = ninox_core::runtime::kill_session(&id).await;

        // A fresh start has no conversation at all, so `relaunch` must
        // re-deliver the role brief itself before the caller's own question
        // — otherwise the agent would be blind to its own role.
        assert!(screen.contains(BRIEF_OPENING), "fresh restart must re-prime the role brief: {screen}");
        assert!(screen.contains("after the crash"), "{screen}");
    }

    #[tokio::test]
    async fn concurrent_first_asks_do_not_race_into_a_double_spawn() {
        let _iso = ninox_core::test_isolation::isolate();
        let id = format!("prompt-agent-test-concurrent-{}", std::process::id());
        let st = store();
        let sessions_dir = tempfile::tempdir().unwrap();
        let workspace_dir = tempfile::tempdir().unwrap();
        let config = fake_harness_config("prompt-fake-concurrent");

        let (r1, r2) = tokio::join!(
            ask_impl(st.clone(), &config, sessions_dir.path(), workspace_dir.path(), &id, "question one"),
            ask_impl(st.clone(), &config, sessions_dir.path(), workspace_dir.path(), &id, "question two"),
        );
        r1.unwrap();
        r2.unwrap();
        let session = st.get_session(&id).unwrap().expect("session row created");
        let alive = ninox_core::runtime::has_session(&id).await;
        let _ = ninox_core::runtime::kill_session(&id).await;

        assert!(alive, "the singleton must end up live, not clobbered Terminated by a losing racer");
        assert_ne!(session.status, SessionStatus::Terminated);
    }

    #[tokio::test]
    async fn singleton_lock_is_exclusive_between_same_process_callers() {
        let st = store();
        let first = acquire_singleton_lock(&st).await.unwrap();
        // `biased` polls the second acquire exactly once before the ready
        // arm: that first poll runs its claim attempt synchronously, so it
        // completes only if the claim (wrongly) succeeded.
        tokio::select! {
            biased;
            second = acquire_singleton_lock(&st) => panic!("second caller claimed a held lock: {second:?}"),
            _ = std::future::ready(()) => {}
        }
        assert_eq!(st.fleet_lock_holder(SINGLETON_LOCK).unwrap().unwrap().0, first);

        st.release_fleet_lock(SINGLETON_LOCK, &first).unwrap();
        let next = acquire_singleton_lock(&st).await.unwrap();
        assert_ne!(next, first);
        st.release_fleet_lock(SINGLETON_LOCK, &next).unwrap();
    }

    #[tokio::test]
    async fn clear_restarts_fresh_on_the_ninox_types_current_model() {
        let _iso = ninox_core::test_isolation::isolate();
        let id = format!("prompt-agent-test-clear-{}", std::process::id());
        let st = store();
        let sessions_dir = tempfile::tempdir().unwrap();
        let workspace_dir = tempfile::tempdir().unwrap();
        let mut config = fake_harness_config("claude-code");
        config.harnesses.insert("claude-code".to_string(), HarnessSpec {
            enabled: true,
            binary: Some("sh".into()),
            interactive_args: vec![
                "-c".into(), "'printf \"❯ \\n\"; echo \"argv:$*\"; stty -icanon; exec cat'".into(), "x".into(),
                "--model".into(), "{model}".into(),
            ],
            ..Default::default()
        });
        let set_model = |config: &mut AppConfig, model: &str| {
            let mut spec = config.worker_type_registry().get(PROMPT_WORKER_TYPE).unwrap().clone();
            spec.harness = "claude-code".into();
            spec.model = Some(model.into());
            config.upsert_worker_type(spec);
        };
        set_model(&mut config, "model-a");

        ask_impl(st.clone(), &config, sessions_dir.path(), workspace_dir.path(), &id, "hello").await.unwrap();
        let screen = screen_with(&id, "argv:--model model-a").await;
        let pid_before = ninox_core::runtime::session_pid(&id).await;
        assert!(screen.contains("argv:--model model-a"), "{screen}");

        set_model(&mut config, "model-b");
        clear_impl(st.clone(), &config, sessions_dir.path(), &id).await.unwrap();
        let screen = screen_with(&id, BRIEF_OPENING).await;
        let pid_after = ninox_core::runtime::session_pid(&id).await;
        let row = st.get_session(&id).unwrap().unwrap();
        let _ = ninox_core::runtime::kill_session(&id).await;

        assert_ne!(pid_before, pid_after, "Clear context restarts the agent");
        assert!(screen.contains("argv:--model model-b"), "{screen}");
        assert_eq!(row.model.as_deref(), Some("model-b"));
        assert_eq!(row.worker_type.as_deref(), Some(PROMPT_WORKER_TYPE));
        assert!(screen.contains(BRIEF_OPENING), "the restart re-primes the brief: {screen}");
    }

    #[test]
    fn a_pending_turn_is_one_the_transcript_has_not_answered_yet() {
        let user = r#"{"type":"user","message":{"role":"user","content":"what's blocking X?"}}"#;
        let streaming = r#"{"type":"assistant","message":{"stop_reason":null,"content":[{"type":"text","text":"Look"}]}}"#;
        let tool = r#"{"type":"assistant","message":{"stop_reason":"tool_use","content":[]}}"#;
        let tool_result = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"ok"}]}}"#;
        let done = r#"{"type":"assistant","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"Done"}]}}"#;
        let clear_echo = r#"{"type":"user","message":{"role":"user","content":"<command-name>/clear</command-name>"}}"#;
        let meta = r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"caveat"}}"#;
        let sidechain = r#"{"type":"user","isSidechain":true,"message":{"role":"user","content":"sub"}}"#;
        let interrupted = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#;
        assert!(!tail_has_pending_turn(&[user, streaming, interrupted].join("\n")));
        let join = |lines: &[&str]| lines.join("\n");
        assert!(tail_has_pending_turn(&join(&[done, user])));
        assert!(tail_has_pending_turn(&join(&[user, streaming])));
        assert!(tail_has_pending_turn(&join(&[user, tool])));
        assert!(tail_has_pending_turn(&join(&[user, tool, tool_result])));
        assert!(!tail_has_pending_turn(&join(&[user, tool, tool_result, done])));
        assert!(!tail_has_pending_turn(&join(&[done, clear_echo])));
        assert!(!tail_has_pending_turn(&join(&[done, meta, sidechain])));
        assert!(!tail_has_pending_turn(""));
    }
}
