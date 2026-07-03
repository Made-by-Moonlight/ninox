//! Shared spawn plumbing used by both the CLI worker path (`main.rs
//! run_spawn`) and the in-app standalone spawn (`app.rs SpawnFormConfirm`):
//! isolated worktree creation, repo-slug detection, and tilde expansion.

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
