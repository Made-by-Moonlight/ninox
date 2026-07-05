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

    let mut env: Vec<(&str, &str)> = vec![
        ("NINOX_BIN",    ninox_bin.as_str()),
        ("NINOX_CONFIG", ninox_config.as_str()),
        ("NINOX_BRAIN",  p.catalogue_path.as_str()),
    ];
    for (k, v) in &p.extra_env {
        env.push((k.as_str(), v.as_str()));
    }

    // Prepend ninox bin dir inside the shell command so the `ninox` shim
    // (which points to the current binary) is found first — even after
    // login-shell rc files reorder PATH.
    let ninox_bin_dir = ninox_core::config::AppConfig::ninox_bin_dir();
    let ninox_bin_dir_str = ninox_bin_dir.display().to_string().replace('\'', "'\\''");
    let base_cmd = p.agent.interactive_cmd();
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
    };
    let _ = engine.store.upsert_session(&updated);

    if let Err(e) = pty::start_streaming(engine.clone(), sid.clone(), &sid).await {
        tracing::error!("pty setup failed for {sid}: {e}");
    }

    engine.emit(Event::SessionUpdated(updated));

    // Hidden tmux client attach argv — mirrors NavigateSession's attach flow.
    Some(tmux::attach_args(&sid).await)
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
}
