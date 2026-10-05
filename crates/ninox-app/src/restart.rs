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

    let targets = resolve_targets(&store, &args).await?;
    if targets.is_empty() {
        println!("nothing to restart");
        return Ok(());
    }
    let self_id = env_nonempty("NINOX_SESSION");
    let orchestrators: std::collections::HashSet<String> =
        store.list_orchestrators()?.into_iter().map(|o| o.id).collect();
    let ordered = ordered_targets(targets, &orchestrators, self_id.as_deref());

    for id in ordered {
        let outcome = if self_id.as_deref() == Some(id.as_str()) {
            spawn_detached_self_restart(&id, &db_path)
        } else {
            restart_one(&store, &config, &id).await
        };
        println!("{}", render_outcome(&outcome));
    }
    Ok(())
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

    let agent = AgentConfig { harness: session.agent_type.clone(), model: session.model.clone() };
    let can_resume = session.claude_session_id.is_some() && config.registry().resume_cmd(&agent, "placeholder").is_some();

    let planned = if can_resume {
        crate::app::resume_plan(&session, is_orchestrator, config)
            .map(|p| (p, session.claude_session_id.clone().expect("checked by can_resume"), RestartedVia::Resumed))
    } else {
        let csid = ninox_core::harness::new_claude_session_id();
        crate::app::refile_plan(&session, is_orchestrator, config, &csid).map(|p| (p, csid, RestartedVia::Fresh))
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
    let Some(_) = launched else {
        let _ = store.record_restore(id, now_ms(), "failed", Some("launch failed"));
        return RestartResult::Failed("launch failed (see the ninox log)".into());
    };
    if !ninox_core::runtime::wait_for_input_prompt(id, PROMPT_TIMEOUT).await {
        tracing::warn!("restart: {id} showed no input prompt within {PROMPT_TIMEOUT:?}; sending the restart note anyway");
    }
    // `--resume` reloads history but does not itself prompt the harness to
    // take a turn, and a Fresh restart has no idea what it was doing — both
    // need a nudge, the same way `fleet::restore`'s own relaunch does.
    if let Err(e) = ninox_core::messaging::deliver_message(
        store, &AppConfig::sessions_dir(), id, &note, config.send_mechanism(),
        Some(ninox_core::messaging::SYSTEM_SENDER),
    )
    .await
    {
        tracing::warn!("restart: delivering restart note to {id}: {e}");
    }
    let _ = store.record_restore(id, now_ms(), via.as_str(), None);
    RestartResult::Restarted(via)
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

#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::store::Store;
    use ninox_core::types::{Orchestrator, Session, SessionStatus};
    use std::collections::HashSet;

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

    fn session(id: &str, orch: Option<&str>, status: SessionStatus, ws: &str) -> Session {
        Session {
            id: id.into(), orchestrator_id: orch.map(str::to_string), name: id.into(),
            repo: String::new(), status, agent_type: "claude-code".into(), cost_usd: 0.0,
            started_at: 1, pr_number: None, pr_id: None, workspace_path: Some(ws.into()),
            pid: None, model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: Some(format!("uuid-{id}")), summary: Some(format!("task {id}")),
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
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

    /// Same shape, but the configured harness has no `resume_args` — the
    /// restart degrades to a fresh start and the note repeats the task
    /// summary instead of just saying "continue".
    #[tokio::test]
    async fn restart_one_degrades_to_fresh_and_rebriefs_when_the_harness_cannot_resume() {
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
