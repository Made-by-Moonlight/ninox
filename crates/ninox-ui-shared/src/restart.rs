//! `ninox restart <session_id>…` — deliberately restart a *live, healthy*
//! session's process in place, so it picks up tooling-stack updates (a newer
//! `ninox` build, harness version, MCP config, or reseeded skills) installed
//! since it started, without losing its conversation, workspace, or
//! worktree.
//!
//! This is the on-demand counterpart to `ninox fleet restore`: that command
//! only ever acts on *interrupted* sessions (a crash/reboot recovers them),
//! so a live, working session is untouched no matter how stale its binaries
//! are. Before this, the only way to pick up an update was killing the pane
//! by hand and waiting for (or triggering) a fleet restore.
//!
//! Shares the exact relaunch mechanism as fleet restore and the app's
//! per-session "Restart" button (`spawn_util::relaunch_in_place`): kill the
//! pane, make sure the workspace still exists, relaunch under the same
//! session id — `--resume` wherever the harness supports it (conversation
//! intact), a fresh restart otherwise.
//!
//! Restarting the session this CLI is itself running inside is a special
//! case: killing that pane would also kill the process executing this very
//! command before it reaches the relaunch. So a self-restart never runs the
//! kill+relaunch in this process — it re-execs `ninox restart --exec-detached`
//! as a detached child (new process group, the same trick
//! `tui::backend::restore_fleet` uses to survive the TUI's own terminal
//! going away) and returns immediately; the child, no longer part of the
//! pane's process group, survives the kill and finishes the relaunch.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::Args;
use ninox_core::{
    config::{AgentConfig, AppConfig},
    events::Engine,
    harness::PermissionMode,
    store::Store,
    types::{Session, SessionStatus},
};

use crate::fleet::{env_nonempty, now_ms, RESTORE_LOCK};
use crate::spawn_util::{relaunch_in_place, RelaunchRequest};

#[derive(Args, Clone, Debug)]
pub struct RestartArgs {
    /// Session ids to restart
    pub session_ids: Vec<String>,
    /// Restart every currently live session instead of naming ids
    #[arg(long)]
    pub all: bool,
    /// Internal: perform the relaunch directly against exactly one id,
    /// without the self-restart detach check. Used by the detached helper
    /// process a self-restart spawns; never pass this by hand.
    #[arg(long, hide = true)]
    pub exec_detached: bool,
}

/// What actually happened to a session's conversation on a successful
/// restart — a local, 2-variant stand-in for `fleet::RestoreOutcome` (whose
/// other two variants, `AlreadyLive`/`Failed`, belong to the crash-recovery
/// path and can't occur here), so downstream matches are exhaustive without
/// a defensive `unreachable!()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartedVia {
    Resumed,
    Fresh,
}

impl RestartedVia {
    fn as_str(self) -> &'static str {
        match self {
            Self::Resumed => "resumed",
            Self::Fresh => "fresh",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum RestartResult {
    Restarted(RestartedVia),
    /// Spawned as a detached background process (self-restart).
    Detached,
    NotLive,
    Unknown,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct RestartOutcome {
    pub session_id: String,
    pub result: RestartResult,
}

/// Arbitrary; a cold harness start (auth, MCP servers) can be slow — matches
/// `fleet::DEFAULT_PROMPT_TIMEOUT`.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(90);

pub async fn run_cli(args: RestartArgs, store: Arc<Store>, config: AppConfig, db_path: PathBuf) -> anyhow::Result<()> {
    if args.exec_detached {
        anyhow::ensure!(args.session_ids.len() == 1, "--exec-detached takes exactly one session id");
        let outcome = restart_one(&store, &config, &args.session_ids[0]).await;
        println!("{}", render_outcome(&outcome));
        return Ok(());
    }

    let outcomes = execute(args, store, config, db_path).await?;
    if outcomes.is_empty() {
        println!("nothing to restart");
    }
    for outcome in &outcomes {
        println!("{}", render_outcome(outcome));
    }
    Ok(())
}

/// The batch restart itself, shared by the CLI (`run_cli`) and the desktop
/// app / TUI "restart all" actions — same path, structured outcomes instead
/// of printed lines, so a UI can summarize success/failure instead of
/// scraping stdout.
pub async fn execute(args: RestartArgs, store: Arc<Store>, config: AppConfig, db_path: PathBuf) -> anyhow::Result<Vec<RestartOutcome>> {
    let targets = resolve_targets(&store, &args).await?;
    if targets.is_empty() {
        return Ok(Vec::new());
    }
    let self_id = env_nonempty("NINOX_SESSION");
    let orchestrators: std::collections::HashSet<String> =
        store.list_orchestrators()?.into_iter().map(|o| o.id).collect();
    let ordered = ordered_targets(targets, &orchestrators, self_id.as_deref());

    let mut outcomes = Vec::with_capacity(ordered.len());
    for id in ordered {
        let outcome = if self_id.as_deref() == Some(id.as_str()) {
            spawn_detached_self_restart(&id, &db_path)
        } else {
            restart_one(&store, &config, &id).await
        };
        outcomes.push(outcome);
    }
    Ok(outcomes)
}

/// One-line human summary of a batch restart, for UI toasts/notifications
/// that don't want to render every `render_outcome` line.
/// True if any outcome didn't end in an actual restart — `Failed`, but also
/// `NotLive`/`Unknown` (the session ended or vanished mid-batch before its
/// own turn came up, since a sequential batch can take minutes and
/// `resolve_targets`'s liveness check only ran once, up front). `summarize`
/// and callers deciding whether "the batch had a problem" must agree on
/// this definition, or a UI can report success for a batch that actually
/// skipped sessions silently.
pub fn had_trouble(outcomes: &[RestartOutcome]) -> bool {
    outcomes.iter().any(|o| !matches!(o.result, RestartResult::Restarted(_) | RestartResult::Detached))
}

pub fn summarize(outcomes: &[RestartOutcome]) -> String {
    let total = outcomes.len();
    let mut restarted = 0usize;
    let mut skipped: Vec<&str> = Vec::new();
    let mut failed: Vec<&str> = Vec::new();
    for o in outcomes {
        match &o.result {
            RestartResult::Restarted(_) | RestartResult::Detached => restarted += 1,
            RestartResult::NotLive | RestartResult::Unknown => skipped.push(o.session_id.as_str()),
            RestartResult::Failed(_) => failed.push(o.session_id.as_str()),
        }
    }
    if skipped.is_empty() && failed.is_empty() {
        return format!("restarted {total} agent{}", if total == 1 { "" } else { "s" });
    }
    let mut msg = format!("restarted {restarted}/{total} agent{}", if total == 1 { "" } else { "s" });
    if !skipped.is_empty() {
        msg.push_str(&format!("; no longer live: {}", skipped.join(", ")));
    }
    if !failed.is_empty() {
        msg.push_str(&format!("; failed: {}", failed.join(", ")));
    }
    msg
}

async fn resolve_targets(store: &Store, args: &RestartArgs) -> anyhow::Result<Vec<String>> {
    if args.all {
        anyhow::ensure!(args.session_ids.is_empty(), "pass session ids or --all, not both");
        let mut live = Vec::new();
        for s in store.list_sessions()? {
            if ninox_core::runtime::has_session(&s.id).await {
                live.push(s.id);
            }
        }
        return Ok(live);
    }
    anyhow::ensure!(!args.session_ids.is_empty(), "pass at least one session id, or --all");
    Ok(args.session_ids.clone())
}

/// Workers (and standalone sessions) before orchestrators — the same
/// ordering invariant `fleet::plan_restore` uses, so a worker a request also
/// restarts is live again before its orchestrator comes back. The caller's
/// own session (if targeted) goes last regardless of role: its restart only
/// spawns a detached helper, so ordering it last just means the rest of the
/// batch's results are visible before this pane drops.
fn ordered_targets(ids: Vec<String>, orchestrators: &std::collections::HashSet<String>, self_id: Option<&str>) -> Vec<String> {
    let is_self = |id: &str| Some(id) == self_id;
    let mut ordered: Vec<String> = ids.iter().filter(|id| !orchestrators.contains(*id) && !is_self(id)).cloned().collect();
    ordered.extend(ids.iter().filter(|id| orchestrators.contains(*id) && !is_self(id)).cloned());
    if let Some(s) = self_id {
        if ids.iter().any(|id| id == s) {
            ordered.push(s.to_string());
        }
    }
    ordered
}

async fn restart_one(store: &Arc<Store>, config: &AppConfig, id: &str) -> RestartOutcome {
    let outcome = restart_one_inner(store, config, id).await;
    RestartOutcome { session_id: id.to_string(), result: outcome }
}

async fn restart_one_inner(store: &Arc<Store>, config: &AppConfig, id: &str) -> RestartResult {
    let session = match store.get_session(id) {
        Ok(Some(s)) => s,
        Ok(None) => return RestartResult::Unknown,
        Err(e) => return RestartResult::Failed(e.to_string()),
    };
    if !ninox_core::runtime::has_session(id).await {
        return RestartResult::NotLive;
    }
    // Best-effort: a full fleet restore holds this lock for its whole run
    // (crash recovery can touch many sessions and wait minutes on prompts).
    // Narrow non-blocking check rather than contending for the lock
    // ourselves — this is a single on-demand restart, not a batch, and
    // should fail fast rather than queue behind an unrelated long operation.
    if let Ok(Some((holder, at))) = store.fleet_lock_holder(RESTORE_LOCK) {
        return RestartResult::Failed(format!(
            "a fleet restore is currently running ({holder}, since {}); try again once it finishes",
            ninox_core::fleet::format_local(at)
        ));
    }
    let is_orchestrator = store.is_orchestrator(id).unwrap_or(false);

    let can_resume = session.claude_session_id.is_some()
        && config.registry().can_resume(&relaunch_agent(&session))
        && store.list_sessions().is_ok_and(|all| config.registry().session_can_resume(&session, &all));

    let planned = if can_resume {
        crate::restart::resume_plan(store, &session, is_orchestrator, config)
            .map(|p| (p, session.claude_session_id.clone().expect("checked by can_resume"), RestartedVia::Resumed))
    } else {
        let csid = ninox_core::harness::new_claude_session_id();
        crate::restart::refile_plan(store, &session, is_orchestrator, config, &csid).map(|p| (p, csid, RestartedVia::Fresh))
    };
    let Some((plan, claude_session_id, via)) = planned else {
        return RestartResult::Failed("no workspace recorded, or the harness cannot relaunch it".into());
    };

    let engine = Engine::new(store.clone());
    let note = restart_note(via, &session);
    let launched = relaunch_in_place(
        engine,
        RelaunchRequest {
            // Not `session.status.clone()`: unlike fleet restore (which only
            // ever relaunches an already-Interrupted/Terminated row), this
            // session was live (status `Working`) going in, so reusing it
            // would leave a session whose relaunch failed looking healthy
            // forever. Interrupted makes a failed restart retryable by
            // `ninox fleet restore` instead of silently stuck.
            failure_status: SessionStatus::Interrupted,
            session,
            is_orchestrator,
            plan,
            claude_session_id,
            started_at: now_ms(),
        },
        config,
        false,
    )
    .await;
    if let Err(cause) = launched {
        let message = format!("launch failed: {cause}");
        let _ = store.record_restore(id, now_ms(), "failed", Some(&message));
        return RestartResult::Failed(message);
    }
    if !ninox_core::runtime::wait_for_input_prompt(id, PROMPT_TIMEOUT).await {
        tracing::warn!("restart: {id} showed no input prompt within {PROMPT_TIMEOUT:?}; sending the restart note anyway");
    }
    // `--resume` reloads history but does not itself prompt the harness to
    // take a turn, and a Fresh restart has no idea what it was doing — both
    // need a nudge, the same way `fleet::restore`'s own relaunch does.
    if let Err(e) = ninox_core::messaging::deliver_system_message(
        store, &AppConfig::sessions_dir(), id, &note, config.send_mechanism(),
        ninox_core::types::MessageKind::System,
    )
    .await
    {
        tracing::warn!("restart: delivering restart note to {id}: {e}");
    }
    let _ = store.record_restore(id, now_ms(), via.as_str(), None);
    RestartResult::Restarted(via)
}

/// A user Resume: relaunch `session` under its own id with `--resume`.
/// Returns the attach argv on success. A failed launch leaves a not-running
/// row as it was (still resumable); a running one that was restarted reads
/// as Interrupted rather than live.
pub async fn resume_session(
    engine: Arc<Engine>,
    session: Session,
    is_orchestrator: bool,
    config: &AppConfig,
    stream_pty: bool,
) -> Result<Vec<String>, String> {
    let plan = resume_plan(&engine.store, &session, is_orchestrator, config)
        .ok_or("no workspace or conversation id recorded, or the harness has no resume_args")?;
    let claude_session_id = session.claude_session_id.clone().ok_or("no conversation id recorded")?;
    let failure_status = if session.status.is_terminal() {
        session.status.clone()
    } else {
        SessionStatus::Interrupted
    };
    let req = RelaunchRequest {
        session,
        is_orchestrator,
        plan,
        claude_session_id,
        started_at: now_ms(),
        failure_status,
    };
    relaunch_in_place(engine, req, config, stream_pty)
        .await
        .map_err(|cause| format!("launch failed: {cause}"))
}

#[derive(Debug)]
pub enum ResumeOutcome {
    /// Running again; the attach argv.
    Resumed(Vec<String>),
    /// Over the session limit: waiting in the queue (the message says where).
    Queued(String),
}

/// [`resume_session`] behind the session limit (`session_queue::admit_resume`):
/// resuming a session that isn't running queues when its location is full;
/// restarting a running one is never gated.
pub async fn resume_gated(
    engine: Arc<Engine>,
    session: Session,
    is_orchestrator: bool,
    config: &AppConfig,
    stream_pty: bool,
    no_queue: bool,
) -> Result<ResumeOutcome, String> {
    use crate::session_queue::{admit_resume, release, ResumeGate};
    let id = session.id.clone();
    let store = engine.store.clone();
    let gate = admit_resume(&store, &session, is_orchestrator, no_queue).map_err(|e| e.to_string())?;
    if let ResumeGate::Queued { message } = gate {
        return Ok(ResumeOutcome::Queued(message));
    }
    let result = resume_session(engine, session, is_orchestrator, config, stream_pty).await;
    if gate == ResumeGate::Admitted {
        release(&store, &id);
    }
    result.map(ResumeOutcome::Resumed)
}

/// What a restarted session is told as its first input — distinct from
/// `fleet::briefing`'s crash-recovery notes (those frame it as "you were
/// interrupted"; this is a deliberate, on-demand restart, not a crash).
fn restart_note(via: RestartedVia, session: &Session) -> String {
    match via {
        RestartedVia::Resumed => "[Ninox restart note] You were restarted on purpose, to pick up a tooling-stack \
            update (a newer ninox/harness build, MCP config, or reseeded skills). Your conversation was resumed — \
            continue exactly where you left off.".to_string(),
        RestartedVia::Fresh => {
            let task = session.summary.as_deref().unwrap_or("(no task summary recorded)");
            format!(
                "[Ninox restart note] You were restarted on purpose, to pick up a tooling-stack update, but this \
                 harness could not resume the conversation, so you are starting fresh. Your last known task: {task}. \
                 Your workspace/worktree is unchanged — check `git status`/`git log`/your open PR and continue from \
                 its current state; do not start over."
            )
        }
    }
}

fn render_outcome(o: &RestartOutcome) -> String {
    let RestartOutcome { session_id, result } = o;
    match result {
        RestartResult::Restarted(RestartedVia::Resumed) => format!("{session_id}: resumed (conversation intact)"),
        RestartResult::Restarted(RestartedVia::Fresh) => format!("{session_id}: fresh restart (harness could not resume)"),
        RestartResult::Detached => format!("{session_id}: restarting in background (will drop and reattach)"),
        RestartResult::NotLive => format!(
            "{session_id}: not live — use `ninox fleet restore --only {session_id}` to restore a crashed/interrupted session"
        ),
        RestartResult::Unknown => format!("{session_id}: no such session"),
        RestartResult::Failed(e) => format!("{session_id}: FAILED — {e}"),
    }
}

/// Where a detached self-restart writes, since its stdout/stderr can't land
/// on the pane that is about to die.
fn restart_log(sessions_dir: &Path) -> PathBuf {
    sessions_dir.join("restart.log")
}

/// `ninox restart --exec-detached <id> --db <db_path>` against `exe`, set to
/// survive the pane it was launched from (see the module docs).
fn restart_exec_command(exe: &Path, id: &str, db_path: &Path) -> std::process::Command {
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["restart", "--exec-detached", id, "--db"]).arg(db_path);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0); // survive this pane's death
    }
    cmd
}

fn spawn_detached_self_restart(id: &str, db_path: &Path) -> RestartOutcome {
    let result = (|| -> anyhow::Result<()> {
        let exe = ninox_core::hooks::canonical_exe()?;
        let log_path = restart_log(&AppConfig::sessions_dir());
        if let Some(dir) = log_path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
        let stderr = log.try_clone()?;
        restart_exec_command(&exe, id, db_path)
            .stdin(std::process::Stdio::null())
            .stdout(log)
            .stderr(stderr)
            .spawn()?;
        Ok(())
    })();
    match result {
        Ok(()) => RestartOutcome { session_id: id.to_string(), result: RestartResult::Detached },
        Err(e) => RestartOutcome { session_id: id.to_string(), result: RestartResult::Failed(format!("spawn detached restart: {e}")) },
    }
}

// ---------------------------------------------------------------------------
// Re-file — respawn a session onto current settings
// ---------------------------------------------------------------------------

/// Everything a Re-file needs, resolved against the CURRENT registry —
/// settings changes apply to new spawns immediately and to existing
/// sessions on Re-file; no silent in-place swaps (spec §V "Updating
/// running sessions"). Pure so it's unit-testable without tmux.
pub struct RefilePlan {
    pub agent:          AgentConfig,
    pub base_cmd:       String,
    pub workspace:      String,
    pub brain:          Option<ninox_core::brain_set::BrainSet>,
    pub extra_env:      Vec<(String, String)>,
    /// The mode `base_cmd` was built with; recorded on the row again at
    /// launch so the next relaunch reads the same value.
    pub permission_mode: PermissionMode,
}

/// The permission mode any relaunch of `session` must use — see
/// [`ninox_core::harness::relaunch_permission_mode`].
pub fn session_permission_mode(store: &Store, session: &Session, is_orchestrator: bool, config: &AppConfig) -> PermissionMode {
    let recorded = store.permission_mode(&session.id).unwrap_or_else(|e| {
        tracing::warn!("permission mode for {}: {e}", session.id);
        None
    });
    let types = config.worker_type_registry();
    let worker_type = session.worker_type.as_deref().and_then(|t| types.get(t));
    ninox_core::harness::relaunch_permission_mode(recorded, session, is_orchestrator, worker_type)
}

/// Whether `session` could be resumed: a workspace, a conversation id, and
/// a harness that supports `--resume`. What [`resume_plan`] requires,
/// without needing the store.
pub fn can_resume(session: &Session, config: &AppConfig) -> bool {
    session.workspace_path.is_some()
        && session.claude_session_id.is_some()
        && config.registry().can_resume(&relaunch_agent(session))
}

/// A relaunch (re-file / resume / restart / restore) keeps the harness
/// and model the session was started with — a `[orchestrator]`/`[worker]`
/// model change applies to new spawns only.
fn relaunch_agent(session: &Session) -> AgentConfig {
    ninox_core::model_resolution::relaunch_agent(&session.agent_type, session.model.as_deref())
}

/// `None` when the session has no recorded workspace (nothing to respawn
/// into). A worker re-files interactively — its original spawn prompt is
/// not stored — but keeps its orchestrator attachment for the tree.
/// Keeps the session's own model (see `relaunch_agent`).
pub fn refile_plan(
    store: &Store,
    session: &Session,
    is_orchestrator: bool,
    config: &AppConfig,
    claude_session_id: &str,
) -> Option<RefilePlan> {
    let workspace = session.workspace_path.clone()?;
    let agent = relaunch_agent(session);
    let permission_mode = session_permission_mode(store, session, is_orchestrator, config);
    let base_cmd = config.registry().interactive_cmd(&agent, permission_mode, claude_session_id);
    let brain = Some(ninox_core::brain_set::BrainSet::from_session(session, config));
    let extra_env = if is_orchestrator {
        vec![
            ("NINOX_ORCHESTRATOR_ID".to_string(), session.id.clone()),
            ("NINOX_CALLER_TYPE".to_string(),     "orchestrator".to_string()),
        ]
    } else {
        Vec::new()
    };
    Some(RefilePlan { agent, base_cmd, workspace, brain, extra_env, permission_mode })
}

/// Re-file as a [`relaunch_in_place`] request: the same id on a freshly
/// minted conversation, `Terminated` if the launch fails (nothing worth
/// keeping resumable). `None` when there's no workspace to respawn into.
pub fn refile_request(store: &Store, session: Session, is_orchestrator: bool, config: &AppConfig, started_at: i64) -> Option<RelaunchRequest> {
    let claude_session_id = ninox_core::harness::new_claude_session_id();
    let plan = refile_plan(store, &session, is_orchestrator, config, &claude_session_id)?;
    Some(RelaunchRequest {
        session,
        is_orchestrator,
        plan,
        claude_session_id,
        started_at,
        failure_status: SessionStatus::Terminated,
    })
}

/// Like `refile_plan`, but rebuilds via `resume_cmd` (continuing the
/// existing `claude_session_id`) instead of `interactive_cmd` (starting
/// fresh). `None` when there's no workspace to resume into OR no stored
/// `claude_session_id` OR the harness can't resume (`resume_cmd` returns
/// `None`) — any of which means there's nothing to relaunch into.
///
/// Keeps the session's own model (see `relaunch_agent`).
pub fn resume_plan(
    store: &Store,
    session: &Session,
    is_orchestrator: bool,
    config: &AppConfig,
) -> Option<RefilePlan> {
    let workspace = session.workspace_path.clone()?;
    let claude_session_id = session.claude_session_id.as_deref()?;
    let agent = relaunch_agent(session);
    if !config.registry().session_can_resume(session, &store.list_sessions().ok()?) {
        return None;
    }
    let permission_mode = session_permission_mode(store, session, is_orchestrator, config);
    let base_cmd = config.registry().resume_cmd(&agent, permission_mode, claude_session_id)?;
    let brain = Some(ninox_core::brain_set::BrainSet::from_session(session, config));
    let extra_env = if is_orchestrator {
        vec![
            ("NINOX_ORCHESTRATOR_ID".to_string(), session.id.clone()),
            ("NINOX_CALLER_TYPE".to_string(),     "orchestrator".to_string()),
        ]
    } else {
        Vec::new()
    };
    Some(RefilePlan { agent, base_cmd, workspace, brain, extra_env, permission_mode })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::store::Store;
    use ninox_core::types::{Orchestrator, Session, SessionStatus};
    use std::collections::HashSet;

    /// A worker row (attached to an orchestrator). Set `orchestrator_id`
    /// to `None` for a standalone session.
    fn refile_session(id: &str) -> Session {
        Session {
            id: id.into(), orchestrator_id: Some("orch".into()), name: id.into(), repo: String::new(),
            status: SessionStatus::Terminated, agent_type: "claude-code".into(),
            cost_usd: 0.0, started_at: 0, pr_number: None, pr_id: None,
            workspace_path: Some("/tmp/ws".into()), pid: None,
            model: Some("claude-opus-4-8".into()), context_tokens: None,
            brain_reads: Vec::new(), brain_write: Some("/brains/b".into()),
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        }
    }

    fn st() -> Store {
        Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap()
    }

    const SKIP: &str = ninox_core::harness::CLAUDE_SKIP_PERMISSIONS_FLAG;

    /// A recorded launch mode is what every relaunch uses, so a session
    /// spawned without the bypass flag can never pick it up by resuming.
    #[test]
    fn resume_and_refile_reuse_the_recorded_launch_mode() {
        let _iso = ninox_core::test_isolation::isolate();
        let cfg = ninox_core::config::AppConfig::default();
        let store = st();
        let mut session = refile_session("w1");
        session.claude_session_id = Some("c1".into());
        store.upsert_session(&session).unwrap();
        for (mode, want_skip) in [(PermissionMode::Standard, false), (PermissionMode::Bypass, true)] {
            store.set_permission_mode("w1", mode).unwrap();
            let resume = resume_plan(&store, &session, false, &cfg).unwrap();
            let refile = refile_plan(&store, &session, false, &cfg, "u").unwrap();
            for plan in [&resume, &refile] {
                assert_eq!(plan.permission_mode, mode);
                assert_eq!(plan.base_cmd.contains(SKIP), want_skip, "{mode:?}: {}", plan.base_cmd);
            }
        }
    }

    #[test]
    fn a_permission_enforced_worker_type_resumes_without_the_skip_flag() {
        let _iso = ninox_core::test_isolation::isolate();
        let mut cfg = ninox_core::config::AppConfig::default();
        let mut scoped = ninox_core::worker_type::WorkerTypeSpec { name: "Scoped".into(), ..Default::default() };
        scoped.permissions.enabled = true;
        cfg.worker_types.push(scoped);
        let store = st();
        let mut session = refile_session("w1");
        session.claude_session_id = Some("c1".into());
        session.worker_type = Some("Scoped".into());
        store.upsert_session(&session).unwrap();
        // Even a stale bypass record (e.g. enforcement switched on after
        // the spawn) can't widen it.
        store.set_permission_mode("w1", PermissionMode::Bypass).unwrap();
        let resume = resume_plan(&store, &session, false, &cfg).unwrap();
        assert!(resume.base_cmd.contains("--resume") && !resume.base_cmd.contains(SKIP), "{}", resume.base_cmd);
        assert!(!refile_plan(&store, &session, false, &cfg, "u").unwrap().base_cmd.contains(SKIP));
    }

    /// Rows launched before modes were recorded: an orchestrator, a
    /// standalone session and the prompt agent resume like they launched
    /// (no skip); only an orchestrator-attached `ninox spawn` worker keeps it.
    #[test]
    fn unrecorded_rows_infer_the_mode_their_launch_used() {
        let _iso = ninox_core::test_isolation::isolate();
        let cfg = ninox_core::config::AppConfig::default();
        let store = st();
        let mut session = refile_session("x");
        session.claude_session_id = Some("c1".into());
        let skip = |s: &Session, is_orch: bool| resume_plan(&store, s, is_orch, &cfg).unwrap().base_cmd.contains(SKIP);
        assert!(skip(&session, false), "attached worker");
        assert!(!skip(&session, true), "orchestrator");
        session.orchestrator_id = None;
        assert!(!skip(&session, false), "standalone");
        session.id = ninox_core::worker_type::PROMPT_SESSION_ID.into();
        session.worker_type = Some(ninox_core::worker_type::PROMPT_WORKER_TYPE.into());
        session.orchestrator_id = Some("orch".into());
        assert!(!skip(&session, false), "prompt agent");
    }

    #[test]
    fn refile_and_resume_keep_the_brain_set() {
        let cfg = ninox_core::config::AppConfig::default();
        let mut session = refile_session("s1");
        session.brain_reads = vec!["/brains/a".into(), "/brains/b".into()];
        session.claude_session_id = Some("c1".into());
        let want = ninox_core::brain_set::BrainSet::from_session(&session, &cfg);
        assert_eq!(want.reads.len(), 2);
        assert_eq!(want.write, std::path::PathBuf::from("/brains/b"));
        assert_eq!(refile_plan(&st(), &session, false, &cfg, "u").unwrap().brain.as_ref(), Some(&want));
        assert_eq!(resume_plan(&st(), &session, false, &cfg).unwrap().brain.as_ref(), Some(&want));
    }

    #[test]
    fn refile_plan_resolves_agent_through_current_registry() {
        let mut cfg = ninox_core::config::AppConfig::default();
        // simulate a spec change since the session was filed
        cfg.harnesses.insert("claude-code".to_string(), ninox_core::harness::HarnessSpec {
            enabled: true,
            binary:  Some("claude-nightly".into()),
            interactive_args: vec!["--model".into(), "{model}".into()],
            ..Default::default()
        });
        let session = refile_session("s1");
        let plan = refile_plan(&st(), &session, false, &cfg, "fresh-uuid").expect("plan");
        assert_eq!(plan.base_cmd, "claude-nightly --model 'claude-opus-4-8' --settings '{\"crossSessionInbound\":\"accept\"}'");
        assert_eq!(plan.workspace, "/tmp/ws");
        assert_eq!(plan.brain, Some(ninox_core::brain_set::BrainSet::single("/brains/b")));
        assert!(plan.extra_env.is_empty());
        assert_eq!(plan.agent.harness, "claude-code");
        assert_eq!(plan.agent.model.as_deref(), Some("claude-opus-4-8"));
    }

    #[test]
    fn refile_plan_keeps_the_session_model_whatever_the_config_says() {
        // A `[orchestrator]`/`[worker]` model change applies to new spawns
        // only; a relaunched session keeps the model it started with.
        let mut cfg = ninox_core::config::AppConfig::default();
        cfg.orchestrator.model = Some("orch-model".into());
        cfg.worker.model       = Some("worker-model".into());
        let mut session = refile_session("s1"); // row: claude-opus-4-8
        for is_orch in [true, false] {
            let plan = refile_plan(&st(), &session, is_orch, &cfg, "fresh-uuid").expect("plan");
            assert_eq!(plan.agent.model.as_deref(), Some("claude-opus-4-8"));
            assert!(plan.base_cmd.contains("claude-opus-4-8"));
            assert!(!plan.base_cmd.contains("orch-model") && !plan.base_cmd.contains("worker-model"));
        }
        session.orchestrator_id = None;
        let plan = refile_plan(&st(), &session, false, &cfg, "u").expect("plan");
        assert_eq!(plan.agent.model.as_deref(), Some("claude-opus-4-8"));
    }

    #[test]
    fn refile_plan_row_without_a_model_stays_on_the_harness_default() {
        let mut cfg = ninox_core::config::AppConfig::default();
        cfg.worker.model = Some("worker-model".into());
        let mut session = refile_session("s1");
        session.model = None;
        let plan = refile_plan(&st(), &session, false, &cfg, "u").expect("plan");
        assert_eq!(plan.agent.model, None);
        assert!(!plan.base_cmd.contains("--model"));
    }

    #[test]
    fn refile_plan_orchestrator_gets_caller_env_and_no_workspace_means_no_plan() {
        let _iso = ninox_core::test_isolation::isolate();
        let cfg = ninox_core::config::AppConfig::default();
        let mut session = refile_session("o1");
        session.brain_write = None;
        let plan = refile_plan(&st(), &session, true, &cfg, "fresh-uuid").expect("plan");
        assert!(plan.extra_env.iter().any(|(k, v)| k == "NINOX_ORCHESTRATOR_ID" && v == "o1"));
        assert!(plan.extra_env.iter().any(|(k, _)| k == "NINOX_CALLER_TYPE"));
        // The legacy ATHENE_/AO_ transition names are gone.
        assert!(!plan.extra_env.iter().any(|(k, _)| k == "ATHENE_CALLER_TYPE" || k == "AO_CALLER_TYPE"));
        // no recorded catalogue → the default set
        assert_eq!(plan.brain, Some(ninox_core::brain_set::BrainSet::default_for(&cfg)));

        session.workspace_path = None;
        assert!(refile_plan(&st(), &session, true, &cfg, "fresh-uuid").is_none());
    }

    #[test]
    fn refile_plan_uses_the_freshly_generated_id_not_the_stale_one() {
        let cfg = ninox_core::config::AppConfig::default();
        let mut session = refile_session("s1");
        session.claude_session_id = Some("stale-id-from-before".into());
        let plan = refile_plan(&st(), &session, false, &cfg, "brand-new-id").expect("plan");
        assert!(plan.base_cmd.contains("brand-new-id"));
        assert!(!plan.base_cmd.contains("stale-id-from-before"));
    }

    #[test]
    fn refile_request_relaunches_the_same_id_on_a_fresh_conversation() {
        let _iso = ninox_core::test_isolation::isolate();
        let cfg = ninox_core::config::AppConfig::default();
        let mut session = refile_session("s1");
        session.claude_session_id = Some("stale-id".into());
        let req = refile_request(&st(), session.clone(), false, &cfg, 42).expect("request");
        assert_eq!(req.session.id, "s1");
        assert_ne!(req.claude_session_id, "stale-id");
        assert!(req.plan.base_cmd.contains(&req.claude_session_id));
        assert_eq!(req.failure_status, SessionStatus::Terminated);
        assert_eq!(req.started_at, 42);
        session.workspace_path = None;
        assert!(refile_request(&st(), session, false, &cfg, 42).is_none());
    }

    #[test]
    fn resume_plan_builds_a_resume_command_from_the_stored_id() {
        let cfg = ninox_core::config::AppConfig::default();
        let mut session = refile_session("s1");
        session.claude_session_id = Some("stored-uuid".into());
        let plan = resume_plan(&st(), &session, false, &cfg).expect("plan");
        assert!(plan.base_cmd.contains("--resume"));
        assert!(plan.base_cmd.contains("stored-uuid"));
        assert_eq!(plan.workspace, "/tmp/ws");
    }

    #[test]
    fn resume_plan_keeps_the_session_model_whatever_the_config_says() {
        let mut cfg = ninox_core::config::AppConfig::default();
        cfg.orchestrator.model = Some("claude-fable-5-1".into());
        cfg.worker.model       = Some("claude-fable-5-1".into());
        let mut session = refile_session("o1"); // row: claude-opus-4-8
        session.claude_session_id = Some("stored-uuid".into());
        for is_orch in [true, false] {
            let plan = resume_plan(&st(), &session, is_orch, &cfg).expect("plan");
            assert_eq!(plan.agent.model.as_deref(), Some("claude-opus-4-8"));
            assert!(plan.base_cmd.contains("--resume"));
            assert!(!plan.base_cmd.contains("claude-fable-5-1"));
        }
    }

    #[test]
    fn resume_plan_none_without_a_stored_claude_session_id() {
        let cfg = ninox_core::config::AppConfig::default();
        let session = refile_session("s1"); // claude_session_id: None by default in this fixture
        assert!(resume_plan(&st(), &session, false, &cfg).is_none());
    }

    #[test]
    fn exec_command_survives_the_pane_and_carries_the_db_path() {
        let cmd = restart_exec_command(Path::new("/bin/ninox"), "sess-1", Path::new("/tmp/sandbox/ninox.db"));
        let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args, ["restart", "--exec-detached", "sess-1", "--db", "/tmp/sandbox/ninox.db"]);
        assert_eq!(restart_log(Path::new("/tmp/sandbox")), Path::new("/tmp/sandbox/restart.log"));
    }

    #[test]
    fn ordered_targets_puts_workers_before_orchestrators_and_self_last() {
        let orchestrators: HashSet<String> = ["o".to_string()].into_iter().collect();
        let ids = vec!["o".to_string(), "w1".to_string(), "self".to_string(), "w2".to_string()];
        let out = ordered_targets(ids, &orchestrators, Some("self"));
        assert_eq!(out, vec!["w1", "w2", "o", "self"]);
    }

    #[test]
    fn ordered_targets_without_self_in_the_request_appends_nothing() {
        let orchestrators: HashSet<String> = ["o".to_string()].into_iter().collect();
        let ids = vec!["o".to_string(), "w".to_string()];
        let out = ordered_targets(ids, &orchestrators, Some("not-requested"));
        assert_eq!(out, vec!["w", "o"]);
    }

    #[test]
    fn summarize_reports_a_clean_sweep() {
        let outcomes = vec![
            RestartOutcome { session_id: "w1".into(), result: RestartResult::Restarted(RestartedVia::Resumed) },
            RestartOutcome { session_id: "w2".into(), result: RestartResult::Restarted(RestartedVia::Fresh) },
        ];
        assert_eq!(summarize(&outcomes), "restarted 2 agents");
    }

    #[test]
    fn summarize_names_the_failures() {
        let outcomes = vec![
            RestartOutcome { session_id: "w1".into(), result: RestartResult::Restarted(RestartedVia::Resumed) },
            RestartOutcome { session_id: "w2".into(), result: RestartResult::Failed("launch failed".into()) },
        ];
        assert_eq!(summarize(&outcomes), "restarted 1/2 agents; failed: w2");
    }

    /// A session that was live when `resolve_targets` ran can end (or
    /// vanish from the store) before its own turn in a long sequential
    /// batch — `NotLive`/`Unknown` must not be folded into the success
    /// count the way a plain "restarted N agents" would imply.
    #[test]
    fn summarize_reports_sessions_that_stopped_being_live_mid_batch() {
        let outcomes = vec![
            RestartOutcome { session_id: "w1".into(), result: RestartResult::Restarted(RestartedVia::Resumed) },
            RestartOutcome { session_id: "w2".into(), result: RestartResult::NotLive },
            RestartOutcome { session_id: "w3".into(), result: RestartResult::Unknown },
        ];
        assert_eq!(summarize(&outcomes), "restarted 1/3 agents; no longer live: w2, w3");
    }

    #[test]
    fn had_trouble_is_false_only_when_every_outcome_actually_restarted() {
        let clean = vec![
            RestartOutcome { session_id: "w1".into(), result: RestartResult::Restarted(RestartedVia::Resumed) },
            RestartOutcome { session_id: "w2".into(), result: RestartResult::Detached },
        ];
        assert!(!had_trouble(&clean));

        for result in [RestartResult::NotLive, RestartResult::Unknown, RestartResult::Failed("x".into())] {
            assert!(had_trouble(&[RestartOutcome { session_id: "w".into(), result }]));
        }
    }

    #[test]
    fn summarize_handles_an_empty_batch() {
        assert_eq!(summarize(&[]), "restarted 0 agents");
    }

    #[tokio::test]
    async fn execute_restarts_every_live_session_and_skips_ended_ones() {
        let _iso = ninox_core::test_isolation::isolate();
        let st = store();
        let ws = tempfile::tempdir().unwrap();
        let wsp = ws.path().to_str().unwrap();
        let mut config = AppConfig::default();
        config.harnesses.insert("restart-fake-execute".into(), ninox_core::harness::HarnessSpec {
            enabled: true,
            binary: Some("sh".into()),
            resume_args: vec!["-c".into(), "'printf \"❯ \\n\"; exec cat'".into(), "x".into(), "{session_id}".into()],
            ..Default::default()
        });
        let live_id = format!("restart-execute-live-{}", std::process::id());
        let mut live = session(&live_id, None, SessionStatus::Working, wsp);
        live.agent_type = "restart-fake-execute".into();
        st.upsert_session(&live).unwrap();
        st.upsert_session(&session("restart-execute-done", None, SessionStatus::Done, wsp)).unwrap();

        ninox_core::runtime::create_session(
            ninox_core::runtime::configured_backend(), &live_id, wsp, "sh -c 'printf \"❯ \\n\"; exec cat'", &[],
        ).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;

        let args = RestartArgs { session_ids: vec![], all: true, exec_detached: false };
        let outcomes = execute(args, st.clone(), config, PathBuf::from("/tmp/unused.db")).await.unwrap();
        let _ = ninox_core::tmux::kill_session(&live_id).await;

        assert_eq!(outcomes.len(), 1, "the Done session was never live, so --all never names it: {outcomes:?}");
        assert_eq!(outcomes[0].session_id, live_id);
        assert_eq!(outcomes[0].result, RestartResult::Restarted(RestartedVia::Resumed));
    }

    fn session(id: &str, orch: Option<&str>, status: SessionStatus, ws: &str) -> Session {
        Session {
            id: id.into(), orchestrator_id: orch.map(str::to_string), name: id.into(),
            repo: String::new(), status, agent_type: "claude-code".into(), cost_usd: 0.0,
            started_at: 1, pr_number: None, pr_id: None, workspace_path: Some(ws.into()),
            pid: None, model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: Some(format!("uuid-{id}")), summary: Some(format!("task {id}")),
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        }
    }

    fn store() -> Arc<Store> {
        Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap())
    }

    #[tokio::test]
    async fn restart_one_reports_unknown_and_not_live() {
        let st = store();
        let ws = tempfile::tempdir().unwrap();
        let wsp = ws.path().to_str().unwrap();
        st.upsert_session(&session("not-live", None, SessionStatus::Working, wsp)).unwrap();

        let out = restart_one(&st, &AppConfig::default(), "no-such-session").await;
        assert_eq!(out.result, RestartResult::Unknown);

        let out = restart_one(&st, &AppConfig::default(), "not-live").await;
        assert_eq!(out.result, RestartResult::NotLive);
    }

    /// End to end on the real tmux backend with a stand-in harness that
    /// draws a `❯` prompt and echoes input: a *live* session is killed and
    /// relaunched resumed, under its own id, picking the relaunch command up
    /// fresh each time (what makes a newer harness binary take effect).
    #[tokio::test]
    async fn restart_one_kills_and_resumes_a_live_session() {
        let _iso = ninox_core::test_isolation::isolate();
        let st = store();
        let ws = tempfile::tempdir().unwrap();
        let w_id = format!("restart-e2e-w-{}", std::process::id());
        let mut config = AppConfig::default();
        config.harnesses.insert("restart-fake".into(), ninox_core::harness::HarnessSpec {
            enabled: true,
            binary: Some("sh".into()),
            resume_args: vec!["-c".into(), "'printf \"❯ \\n\"; exec cat'".into(), "x".into(), "{session_id}".into()],
            ..Default::default()
        });
        config.worker.harness = "restart-fake".into();
        let wsp = ws.path().to_str().unwrap();
        let o_id = format!("restart-e2e-o-{}", std::process::id());
        let mut w = session(&w_id, Some(&o_id), SessionStatus::Working, wsp);
        w.agent_type = "restart-fake".into();
        st.upsert_session(&w).unwrap();
        st.upsert_orchestrator(&Orchestrator { id: o_id.clone(), name: "o".into(), created_at: 0 }).unwrap();

        // Bring the pane up first — relaunch_in_place expects a live-or-dead
        // pane to kill, same as the app's Resume button does.
        ninox_core::runtime::create_session(
            ninox_core::runtime::configured_backend(), &w_id, wsp, "sh -c 'printf \"❯ \\n\"; exec cat'", &[],
        ).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(ninox_core::runtime::has_session(&w_id).await, "precondition: pane is live");

        let out = restart_one(&st, &config, &w_id).await;

        let live = ninox_core::runtime::has_session(&w_id).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let screen = ninox_core::runtime::read_screen(&w_id, Some(50), false).await.unwrap_or_default();
        let _ = ninox_core::tmux::kill_session(&w_id).await;

        assert_eq!(out.result, RestartResult::Restarted(RestartedVia::Resumed), "{out:?}");
        assert!(live, "session is live again after restart");
        assert!(screen.contains("[Ninox restart note]"), "restart note was delivered to the resumed pane: {screen}");
    }

    /// A resume goes through `relaunch_in_place`, which every relaunch path
    /// shares: an enforced type's rules are (re)seeded into the workspace
    /// and the row keeps `Standard`, even if the rules file was deleted.
    #[tokio::test]
    async fn a_restarted_enforced_type_session_is_reseeded_and_stays_standard() {
        let _iso = ninox_core::test_isolation::isolate();
        let st = store();
        let ws = tempfile::tempdir().unwrap();
        let w_id = format!("restart-perm-w-{}", std::process::id());
        let mut config = AppConfig::default();
        config.harnesses.insert("restart-fake".into(), ninox_core::harness::HarnessSpec {
            enabled: true,
            binary: Some("sh".into()),
            resume_args: vec!["-c".into(), "'printf \"❯ \\n\"; exec cat'".into(), "x".into(), "{session_id}".into()],
            ..Default::default()
        });
        let mut scoped = ninox_core::worker_type::WorkerTypeSpec { name: "Scoped".into(), ..Default::default() };
        scoped.permissions.enabled = true;
        scoped.permissions.allow = vec!["Read".into()];
        config.worker_types.push(scoped);
        let wsp = ws.path().to_str().unwrap();
        let mut w = session(&w_id, Some("orch"), SessionStatus::Working, wsp);
        w.agent_type = "restart-fake".into();
        w.worker_type = Some("Scoped".into());
        st.upsert_session(&w).unwrap();
        st.set_permission_mode(&w_id, PermissionMode::Bypass).unwrap();
        ninox_core::runtime::create_session(
            ninox_core::runtime::configured_backend(), &w_id, wsp, "sh -c 'printf \"❯ \\n\"; exec cat'", &[],
        ).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;

        let out = restart_one(&st, &config, &w_id).await;
        let _ = ninox_core::tmux::kill_session(&w_id).await;

        assert_eq!(out.result, RestartResult::Restarted(RestartedVia::Resumed), "{out:?}");
        assert_eq!(st.permission_mode(&w_id).unwrap(), Some(PermissionMode::Standard));
        let local: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(ws.path().join(".claude/settings.local.json")).expect("seeded"),
        ).unwrap();
        assert_eq!(local["permissions"]["allow"], serde_json::json!(["Read"]));
    }

    /// Same shape, but the configured harness has no `resume_args` — the
    /// restart degrades to a fresh start and the note repeats the task
    /// summary instead of just saying "continue".
    #[tokio::test]
    async fn restart_one_degrades_to_fresh_and_rebriefs_when_the_harness_cannot_resume() {
        let _iso = ninox_core::test_isolation::isolate();
        let st = store();
        let ws = tempfile::tempdir().unwrap();
        let w_id = format!("restart-e2e-fresh-{}", std::process::id());
        let mut config = AppConfig::default();
        config.harnesses.insert("restart-fake-fresh".into(), ninox_core::harness::HarnessSpec {
            enabled: true,
            binary: Some("sh".into()),
            interactive_args: vec!["-c".into(), "'printf \"❯ \\n\"; exec cat'".into()],
            resume_args: vec![],
            ..Default::default()
        });
        let wsp = ws.path().to_str().unwrap();
        let o_id = format!("restart-e2e-fresh-o-{}", std::process::id());
        let mut w = session(&w_id, Some(&o_id), SessionStatus::Working, wsp);
        w.agent_type = "restart-fake-fresh".into();
        w.summary = Some("do the fresh-restart thing".into());
        st.upsert_session(&w).unwrap();
        st.upsert_orchestrator(&Orchestrator { id: o_id.clone(), name: "o".into(), created_at: 0 }).unwrap();

        ninox_core::runtime::create_session(
            ninox_core::runtime::configured_backend(), &w_id, wsp, "sh -c 'printf \"❯ \\n\"; exec cat'", &[],
        ).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;

        let out = restart_one(&st, &config, &w_id).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let screen = ninox_core::runtime::read_screen(&w_id, Some(50), false).await.unwrap_or_default();
        let _ = ninox_core::tmux::kill_session(&w_id).await;

        assert_eq!(out.result, RestartResult::Restarted(RestartedVia::Fresh), "{out:?}");
        assert!(screen.contains("starting fresh"), "{screen}");
        assert!(screen.contains("do the fresh-restart thing"), "{screen}");
    }

    #[tokio::test]
    async fn restart_one_records_a_fleet_history_entry_on_success() {
        let _iso = ninox_core::test_isolation::isolate();
        let st = store();
        let ws = tempfile::tempdir().unwrap();
        let w_id = format!("restart-record-{}", std::process::id());
        let mut config = AppConfig::default();
        config.harnesses.insert("restart-fake-record".into(), ninox_core::harness::HarnessSpec {
            enabled: true,
            binary: Some("sh".into()),
            resume_args: vec!["-c".into(), "'printf \"❯ \\n\"; exec cat'".into(), "x".into(), "{session_id}".into()],
            ..Default::default()
        });
        let wsp = ws.path().to_str().unwrap();
        let mut w = session(&w_id, None, SessionStatus::Working, wsp);
        w.agent_type = "restart-fake-record".into();
        st.upsert_session(&w).unwrap();

        ninox_core::runtime::create_session(
            ninox_core::runtime::configured_backend(), &w_id, wsp, "sh -c 'printf \"❯ \\n\"; exec cat'", &[],
        ).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;

        let out = restart_one(&st, &config, &w_id).await;
        let _ = ninox_core::tmux::kill_session(&w_id).await;

        assert_eq!(out.result, RestartResult::Restarted(RestartedVia::Resumed), "{out:?}");
        let record = st.fleet_record(&w_id).unwrap().expect("a restore record was written");
        assert_eq!(record.restore_mode.as_deref(), Some("resumed"));
        assert!(record.restored_at.is_some());
    }

    #[tokio::test]
    async fn restart_one_refuses_while_a_fleet_restore_lease_is_held() {
        let _iso = ninox_core::test_isolation::isolate();
        let st = store();
        let ws = tempfile::tempdir().unwrap();
        let wsp = ws.path().to_str().unwrap();
        let id = format!("restart-locked-{}", std::process::id());
        st.upsert_session(&session(&id, None, SessionStatus::Working, wsp)).unwrap();
        ninox_core::runtime::create_session(
            ninox_core::runtime::configured_backend(), &id, wsp, "sh -c 'exec cat'", &[],
        ).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(st.try_acquire_fleet_lock(RESTORE_LOCK, "someone-else", now_ms(), 0).unwrap());

        let out = restart_one(&st, &AppConfig::default(), &id).await;
        let _ = ninox_core::tmux::kill_session(&id).await;

        match out.result {
            RestartResult::Failed(e) => assert!(e.contains("fleet restore is currently running"), "{e}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// A relaunch that fails (bad workspace path) must not leave the row
    /// looking healthy — `Working` would never become eligible for a later
    /// `ninox fleet restore` retry, so it has to flip to `Interrupted`
    /// instead of keeping whatever status the live session had going in.
    #[tokio::test]
    async fn restart_one_marks_a_failed_relaunch_interrupted_and_records_it() {
        let _iso = ninox_core::test_isolation::isolate();
        let st = store();
        let w_id = format!("restart-fail-{}", std::process::id());
        let ws = tempfile::tempdir().unwrap();
        let wsp = ws.path().to_str().unwrap();
        let mut config = AppConfig::default();
        config.harnesses.insert("restart-fake-fail".into(), ninox_core::harness::HarnessSpec {
            enabled: true,
            binary: Some("sh".into()),
            resume_args: vec!["-c".into(), "'printf \"❯ \\n\"; exec cat'".into(), "x".into(), "{session_id}".into()],
            ..Default::default()
        });
        let mut w = session(&w_id, None, SessionStatus::Working, wsp);
        w.agent_type = "restart-fake-fail".into();
        st.upsert_session(&w).unwrap();

        // Bring up the pane at a real workspace first (so `has_session`
        // sees it live), then poison the *stored* workspace path with an
        // embedded NUL — `Command::spawn` rejects argv containing one
        // deterministically, independent of tmux version (see
        // `app.rs::resume_message_keeps_status_interrupted_when_tmux_create_fails`).
        ninox_core::runtime::create_session(
            ninox_core::runtime::configured_backend(), &w_id, wsp, "sh -c 'printf \"❯ \\n\"; exec cat'", &[],
        ).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut poisoned = w.clone();
        poisoned.workspace_path = Some("/definitely/does/not/exist/ever\0".into());
        st.upsert_session(&poisoned).unwrap();

        let out = restart_one(&st, &config, &w_id).await;
        let _ = ninox_core::tmux::kill_session(&w_id).await;

        match out.result {
            RestartResult::Failed(_) => {}
            other => panic!("expected Failed, got {other:?}"),
        }
        assert_eq!(st.get_session(&w_id).unwrap().unwrap().status, SessionStatus::Interrupted);
        let record = st.fleet_record(&w_id).unwrap().expect("a failed restore was recorded");
        assert_eq!(record.restore_mode.as_deref(), Some("failed"));
    }
}
