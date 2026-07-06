//! Shared spawn plumbing used by both the CLI worker path (`main.rs
//! run_spawn`) and the in-app standalone spawn (`app.rs SpawnFormConfirm`):
//! isolated worktree creation, repo-slug detection, tilde expansion, and the
//! interactive tmux+PTY launch sequence shared by both spawn-modal kinds.

use std::sync::Arc;

use ninox_core::{events::Engine, pty, tmux, Event, Session, SessionStatus};

/// The per-kind differences between the spawn-modal launch paths
/// (standalone vs orchestrator). Everything else — env resolution,
/// PATH-prepend launch command, tmux session creation, pid lookup,
/// session persistence, and PTY streaming — is identical and lives in
/// [`spawn_interactive_session`].
pub struct InteractiveSpawnParams {
    /// Session id; also used as the tmux session name.
    pub session_id:      String,
    pub name:            String,
    /// Directory the agent starts in (isolated worktree for standalone,
    /// per-orchestrator subdirectory for orchestrators). Recorded as the
    /// session's `workspace_path`.
    pub workspace:       String,
    /// GitHub `owner/repo` slug ("" when unknown / not applicable).
    pub repo:            String,
    pub orchestrator_id: Option<String>,
    pub agent:           ninox_core::config::AgentConfig,
    /// Resolved interactive launch command (registry-resolved by the
    /// caller: `AppConfig::registry().interactive_cmd(&agent)`). Kept
    /// separate from `agent` so this module needs no registry access;
    /// `agent` remains for session-record stamping (agent_type/model).
    pub base_cmd:        String,
    /// Brain catalogue path, exported as `NINOX_BRAIN` for both kinds.
    pub catalogue_path:  String,
    /// Extra env pairs beyond the shared NINOX_BIN/NINOX_CONFIG/NINOX_BRAIN
    /// (e.g. NINOX_ORCHESTRATOR_ID + caller-type vars for orchestrators).
    pub extra_env:       Vec<(String, String)>,
    pub started_at:      i64,
}

/// Launch an interactive agent session inside tmux and start PTY streaming.
///
/// On success, returns the tmux attach argv for the new session so the
/// caller can emit `Message::ClientAttach` and render it immediately at the
/// right size (the attached-client flow sizes the PTY to the real panel).
/// On tmux failure (e.g. the workspace path is bad) the session is marked
/// `Terminated` in the store, a `SessionUpdated` event is emitted so the UI
/// shows "Session exited" instead of a session stuck in Working forever, and
/// `None` is returned.
pub async fn spawn_interactive_session(
    engine: Arc<Engine>,
    p: InteractiveSpawnParams,
) -> Option<Vec<String>> {
    let sid = p.session_id;

    let ninox_bin = std::env::current_exe()
        .ok()
        .and_then(|x| x.to_str().map(str::to_string))
        .unwrap_or_else(|| "ninox".to_string());
    let ninox_config = ninox_core::config::AppConfig::config_path()
        .to_string_lossy()
        .to_string();
    // Session metadata dir — must be set for both Orchestrator and Standalone
    // kinds, mirroring the CLI worker path (`main.rs run_spawn`). Without
    // these, the gh/git wrapper scripts (`ninox_core::hooks`) have nowhere to
    // record PR/branch metadata for app-spawned sessions, and the usage
    // poller has no session id to attribute cost/token ingestion to.
    let sessions_dir = ninox_core::config::AppConfig::sessions_dir();
    std::fs::create_dir_all(&sessions_dir).ok();
    let sessions_dir_str = sessions_dir.to_string_lossy().to_string();

    let env = interactive_env_vars(
        &ninox_bin, &ninox_config, &p.catalogue_path, &sid, &sessions_dir_str, &p.extra_env,
    );

    // Prepend ninox bin dir inside the shell command so the `ninox` shim
    // (which points to the current binary) is found first — even after
    // login-shell rc files reorder PATH.
    let ninox_bin_dir = ninox_core::config::AppConfig::ninox_bin_dir();
    let ninox_bin_dir_str = ninox_bin_dir.display().to_string().replace('\'', "'\\''");
    let base_cmd = p.base_cmd;
    let launch_cmd = format!("export PATH='{ninox_bin_dir_str}':\"$PATH\"; {base_cmd}");

    if let Err(e) = tmux::create_session(&sid, &p.workspace, &launch_cmd, &env).await {
        tracing::error!("tmux create failed for {sid}: {e}");
        // Surface the failure: without this the optimistically inserted
        // session would sit in Working forever.
        if let Ok(Some(mut s)) = engine.store.get_session(&sid) {
            s.status = SessionStatus::Terminated;
            let _ = engine.store.upsert_session(&s);
            engine.emit(Event::SessionUpdated(s));
        }
        return None;
    }

    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    let pid = tmux::list_sessions()
        .await
        .ok()
        .and_then(|ss| ss.into_iter().find(|s| s.id == sid))
        .and_then(|s| s.pid);

    let updated = Session {
        id:              sid.clone(),
        orchestrator_id: p.orchestrator_id,
        name:            p.name,
        repo:            p.repo,
        status:          SessionStatus::Working,
        agent_type:      p.agent.harness.clone(),
        cost_usd:        0.0,
        started_at:      p.started_at,
        pr_number:       None,
        pr_id:           None,
        workspace_path:  Some(p.workspace),
        pid,
        model:           p.agent.model.clone(),
        context_tokens:  None,
        catalogue_path:  (!p.catalogue_path.is_empty()).then(|| p.catalogue_path.clone()),
    };
    let _ = engine.store.upsert_session(&updated);

    if let Err(e) = pty::start_streaming(engine.clone(), sid.clone(), &sid).await {
        tracing::error!("pty setup failed for {sid}: {e}");
    }

    engine.emit(Event::SessionUpdated(updated));

    // Hidden tmux client attach argv — mirrors NavigateSession's attach flow.
    Some(tmux::attach_args(&sid).await)
}

/// The tmux env for an app-spawned interactive session (Orchestrator or
/// Standalone): the shared NINOX_BIN/NINOX_CONFIG/NINOX_BRAIN vars, session
/// metadata attribution (`NINOX_SESSION`/`NINOX_DATA_DIR`, plus the legacy
/// `ATHENE_*` aliases — consumed by the gh/git wrapper scripts in
/// `ninox_core::hooks` and by the usage poller's cost/token ingestion),
/// plus any kind-specific extra vars.
///
/// Factored out from [`spawn_interactive_session`] for unit testing: this is
/// the fix for the gap where app-spawned sessions never set the session
/// attribution vars (only the CLI worker path in `main.rs::run_spawn` did),
/// making PR/branch metadata capture and cost attribution silently
/// invisible for orchestrators and standalone spawns.
fn interactive_env_vars<'a>(
    ninox_bin:      &'a str,
    ninox_config:   &'a str,
    catalogue_path: &'a str,
    session_id:     &'a str,
    sessions_dir:   &'a str,
    extra_env:      &'a [(String, String)],
) -> Vec<(&'a str, &'a str)> {
    let mut env: Vec<(&str, &str)> = vec![
        ("NINOX_BIN",       ninox_bin),
        ("NINOX_CONFIG",    ninox_config),
        ("NINOX_BRAIN",     catalogue_path),
        ("NINOX_SESSION",   session_id),
        ("NINOX_DATA_DIR",  sessions_dir),
        // Legacy names: wrapper scripts installed by an older ninox read
        // these; kept until those installs are gone.
        ("ATHENE_SESSION",  session_id),
        ("ATHENE_DATA_DIR", sessions_dir),
    ];
    for (k, v) in extra_env {
        env.push((k.as_str(), v.as_str()));
    }
    env
}

/// Expand a leading `~` or `~/` in a user-supplied path to the home directory.
/// Returns the input unchanged when it doesn't start with `~` (or when the
/// home directory can't be resolved).
pub fn expand_tilde(path: &str) -> String {
    if path == "~" {
        if let Some(home) = dirs::home_dir() {
            return home.to_string_lossy().to_string();
        }
    } else if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).to_string_lossy().to_string();
        }
    }
    path.to_string()
}

/// Read `git remote get-url origin` from the workspace and parse it as a
/// GitHub slug (`owner/repo`). Returns `None` if git fails or the URL is not
/// a recognisable GitHub remote.
pub fn repo_from_workspace(workspace: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["-C", workspace, "remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    ninox_core::github::split_repo(&url).map(|(o, r)| format!("{o}/{r}"))
}

/// Create an isolated git worktree for a session at
/// `{repo}/.claude/worktrees/{session_id}` on a new branch `{session_id}`.
///
/// Returns the worktree path on success. If the branch name already exists
/// (e.g. a previous run with the same name), the existing branch is checked
/// out rather than creating a new one.
pub async fn create_worker_worktree(repo: &str, session_id: &str) -> anyhow::Result<String> {
    use anyhow::Context as _;
    use tokio::process::Command;

    let worktree_path = std::path::Path::new(repo)
        .join(".claude")
        .join("worktrees")
        .join(session_id);
    let worktree_str = worktree_path.to_string_lossy().to_string();

    // Attempt 1: create a fresh branch named after the session.
    let out = Command::new("git")
        .args(["-C", repo, "worktree", "add", &worktree_str, "-b", session_id])
        .output()
        .await
        .context("git worktree add")?;

    if out.status.success() {
        return Ok(worktree_str);
    }

    // Attempt 2: branch already exists — check it out without -b.
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("already exists") {
        let out2 = Command::new("git")
            .args(["-C", repo, "worktree", "add", &worktree_str, session_id])
            .output()
            .await
            .context("git worktree add (existing branch)")?;
        if out2.status.success() {
            return Ok(worktree_str);
        }
        anyhow::bail!("{}", String::from_utf8_lossy(&out2.stderr).trim());
    }

    anyhow::bail!("{}", stderr.trim());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_tilde_leaves_absolute_paths_alone() {
        assert_eq!(expand_tilde("/tmp/foo"), "/tmp/foo");
        assert_eq!(expand_tilde("relative/path"), "relative/path");
    }

    #[test]
    fn expand_tilde_expands_home_prefix() {
        let home = dirs::home_dir().expect("home dir").to_string_lossy().to_string();
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("~/proj"), format!("{home}/proj"));
        // A bare "~user" form is not supported — passed through untouched.
        assert_eq!(expand_tilde("~other/proj"), "~other/proj");
    }

    /// Regression test for the gap where app-spawned sessions (both
    /// Orchestrator and Standalone kinds) never set `ATHENE_SESSION`/
    /// `ATHENE_DATA_DIR`, unlike the CLI worker path (`main.rs::run_spawn`,
    /// see `worker_env_vars` there). Without these, the gh/git wrapper
    /// scripts can't record PR/branch metadata and the usage poller can't
    /// attribute a workspace's cost/token ingestion back to a session id.
    #[test]
    fn interactive_env_vars_includes_session_attribution() {
        let extra = vec![("NINOX_ORCHESTRATOR_ID".to_string(), "orch-1".to_string())];
        let env = interactive_env_vars(
            "/usr/local/bin/ninox", "/cfg/config.toml", "/brain", "sess-1", "/data/sessions", &extra,
        );
        assert!(env.contains(&("NINOX_SESSION", "sess-1")));
        assert!(env.contains(&("NINOX_DATA_DIR", "/data/sessions")));
        // Legacy ATHENE_* names still exported for wrapper scripts installed
        // by an older ninox.
        assert!(env.contains(&("ATHENE_SESSION", "sess-1")));
        assert!(env.contains(&("ATHENE_DATA_DIR", "/data/sessions")));
        assert!(env.contains(&("NINOX_ORCHESTRATOR_ID", "orch-1")));
    }
}

#[cfg(test)]
mod persistence_probe {
    use super::*;
    use ninox_core::{config::AgentConfig, store::Store};
    use std::sync::Arc;
    use tempfile::tempdir;

    /// End-to-end persistence probe — NOT part of the suite (`--ignored`).
    /// Spawns a REAL interactive agent session through the exact app code
    /// path, attaches+drops a client like the app quitting, then this test
    /// process exits. The controller verifies from outside that the tmux
    /// session survives. Cleanup is the controller's job (kill-session).
    #[tokio::test]
    #[ignore]
    async fn spawn_probe() {
        let store = Arc::new(Store::open(tempdir().unwrap().keep().join("p.db")).unwrap());
        let engine = ninox_core::events::Engine::new(store);
        let ws = tempdir().unwrap().keep().to_string_lossy().to_string();

        let attach = spawn_interactive_session(
            engine.clone(),
            InteractiveSpawnParams {
                session_id:      "fnd-probe".into(),
                name:            "fnd-probe".into(),
                workspace:       ws,
                repo:            String::new(),
                orchestrator_id: None,
                agent:           AgentConfig::default(),
                base_cmd:        ninox_core::config::AppConfig::default()
                                     .registry()
                                     .interactive_cmd(&AgentConfig::default()),
                catalogue_path:  String::new(),
                extra_env:       Vec::new(),
                started_at:      0,
            },
        )
        .await;
        let argv = attach.expect("tmux create must succeed");

        // Mirror the app: attach a hidden client, then drop it (Cmd-Q path).
        let client = ninox_core::client::AttachedClient::spawn(
            engine, "fnd-probe".into(), argv, 140, 40, 0,
        )
        .expect("attach");
        tokio::time::sleep(std::time::Duration::from_secs(4)).await;
        drop(client);
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        // test process exits here — the "app" is gone.
    }

    /// End-to-end proof that the cost/context tracking fix works against a
    /// REAL `claude` session, driven exactly the way the app drives one:
    /// spawn via `spawn_interactive_session` (the code path that used to be
    /// missing `ATHENE_SESSION`/`ATHENE_DATA_DIR`), send one cheap prompt,
    /// then assert the usage poller's ingestion (`ninox_core::lifecycle::
    /// usage::ingest_usage_for_workspace`) picks up a non-zero cost and
    /// context-token count from the real `~/.claude/projects/...` transcript
    /// `claude` itself writes. NOT part of the suite (`--ignored`) — costs a
    /// small amount of real API usage and needs the `claude` CLI installed.
    #[tokio::test]
    #[ignore]
    async fn usage_ingestion_probe() {
        let store = Arc::new(Store::open(tempdir().unwrap().keep().join("p.db")).unwrap());
        let engine = ninox_core::events::Engine::new(store.clone());
        let ws = tempdir().unwrap().keep().to_string_lossy().to_string();
        let sid = "fnd-usage-probe".to_string();

        let attach = spawn_interactive_session(
            engine.clone(),
            InteractiveSpawnParams {
                session_id:      sid.clone(),
                name:            sid.clone(),
                workspace:       ws.clone(),
                repo:            String::new(),
                orchestrator_id: None,
                agent:           AgentConfig::default(),
                base_cmd:        ninox_core::config::AppConfig::default()
                                     .registry()
                                     .interactive_cmd(&AgentConfig::default()),
                catalogue_path:  String::new(),
                extra_env:       Vec::new(),
                started_at:      0,
            },
        )
        .await;
        let argv = attach.expect("tmux create must succeed");
        let client = ninox_core::client::AttachedClient::spawn(
            engine.clone(), sid.clone(), argv, 140, 40, 0,
        )
        .expect("attach");

        // Give the TUI a moment to finish booting before sending input — the
        // welcome screen + MCP-auth check can take a while under load, and
        // input sent too early is silently dropped.
        tokio::time::sleep(std::time::Duration::from_secs(15)).await;
        engine
            .send_to_session(&sid, "Reply with exactly one word: OK")
            .await
            .expect("send prompt");

        // Poll until the transcript shows up with non-zero usage, or time out.
        let mut snapshot = None;
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            if let Some(s) = ninox_core::lifecycle::usage::ingest_usage_for_workspace(&ws) {
                snapshot = Some(s);
                break;
            }
        }

        drop(client);

        let snapshot = snapshot.expect("usage should be ingested from the real claude transcript");
        assert!(snapshot.cost_usd > 0.0, "cost_usd must be non-zero once claude has taken a turn");
        assert!(snapshot.context_tokens > 0, "context_tokens must be non-zero once claude has taken a turn");
    }
}
