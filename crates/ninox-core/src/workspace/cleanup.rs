use std::path::Path;

use super::{decide, CleanupDecision, CleanupPolicy, SkipReason, WorkspaceFacts};

/// What one `cleanup()` call actually did (or didn't).
#[derive(Debug, Clone, PartialEq)]
pub struct CleanupOutcome {
    pub worktree_removed: bool,
    /// The worktree existed and `cleanup()` decided to remove it, but the
    /// `git worktree remove` command itself failed (e.g. a locked file
    /// needing `--force --force`) — distinct from `worktree_removed: false`
    /// meaning "there was nothing to remove". A caller must treat this as
    /// needs-attention, not a silent no-op.
    pub worktree_removal_failed: bool,
    pub branch_deleted: Option<String>,
    pub artifacts_removed: bool,
    pub skipped: Option<SkipReason>,
}

impl CleanupOutcome {
    /// Short human string for `Store::set_cleanup_result`, e.g.
    /// `"worktree removed, branch feat deleted"` or `"skipped: dirty"`.
    pub fn describe(&self) -> String {
        if let Some(reason) = self.skipped {
            let why = match reason {
                SkipReason::LiveSessionRow => "live session",
                SkipReason::Interrupted => "interrupted",
                SkipReason::Dirty => "dirty",
                SkipReason::UnpushedCommits => "unpushed commits",
            };
            return format!("skipped: {why}");
        }
        if self.worktree_removal_failed {
            return "needs attention: worktree removal failed".to_string();
        }
        let mut parts = Vec::new();
        if self.worktree_removed {
            parts.push("worktree removed".to_string());
        }
        if let Some(b) = &self.branch_deleted {
            parts.push(format!("branch {b} deleted"));
        }
        if self.artifacts_removed {
            parts.push("artifacts removed".to_string());
        }
        if parts.is_empty() {
            "no-op".to_string()
        } else {
            parts.join(", ")
        }
    }

    /// Whether this outcome should surface as needs-attention to a caller
    /// (e.g. the orphan sweeper's poller logging) rather than a quiet
    /// success — a safety skip, or a removal that was attempted and failed.
    pub fn needs_attention(&self) -> bool {
        self.skipped.is_some() || self.worktree_removal_failed
    }
}

/// The one cleanup function — every worktree teardown path (manual reap,
/// auto-reap-on-merge, the retention sweep, the orphan sweeper) routes
/// through this. `facts` is gathered by the caller (`gather_git_facts` for a
/// live session, or the orphan sweeper's own pass over a directory with no
/// session row) so this function stays pure IO, no decision logic beyond
/// `decide()` itself.
///
/// Idempotent: a worktree directory or branch that's already gone is treated
/// as already-done, not an error — `cleanup()` checks existence before
/// acting rather than swallowing failures from acting on nothing.
///
/// `remove_hook_artifacts` is a caller choice, not a safety rule: `reap`
/// passes `false` to preserve its long-standing deliberate skip (a just-
/// finished worker's hook artifacts can carry a work request or PR report
/// the poller hasn't ingested yet), while every other caller passes `true`.
pub async fn cleanup(
    session_id: &str,
    workspace_path: &str,
    branch: Option<&str>,
    facts: WorkspaceFacts,
    policy: CleanupPolicy,
    remove_hook_artifacts: bool,
    sessions_dir: &Path,
) -> anyhow::Result<CleanupOutcome> {
    let delete_branch = match decide(&facts, policy) {
        CleanupDecision::Skip(reason) => {
            return Ok(CleanupOutcome {
                worktree_removed: false,
                worktree_removal_failed: false,
                branch_deleted: None,
                artifacts_removed: false,
                skipped: Some(reason),
            });
        }
        CleanupDecision::Proceed { delete_branch } => delete_branch,
    };

    let _ = crate::runtime::kill_session(session_id).await;

    let mut worktree_removed = false;
    let mut worktree_removal_failed = false;
    let mut branch_deleted = None;
    if let Some(repo_root) = ninox_repo_root(workspace_path, session_id) {
        match remove_worktree_if_present(&repo_root, workspace_path).await {
            WorktreeRemoval::NothingToRemove => {}
            WorktreeRemoval::Removed => worktree_removed = true,
            WorktreeRemoval::Failed => worktree_removal_failed = true,
        }
        if delete_branch {
            if let Some(b) = branch {
                branch_deleted = delete_branch_if_present(&repo_root, b).await;
            }
        }
        prune_worktrees(&repo_root).await;
    }

    let artifacts_removed = if remove_hook_artifacts {
        let existed = sessions_dir.join(format!("{session_id}.json")).exists();
        crate::hooks::remove_session_artifacts(sessions_dir, session_id);
        existed
    } else {
        false
    };

    Ok(CleanupOutcome { worktree_removed, worktree_removal_failed, branch_deleted, artifacts_removed, skipped: None })
}

enum WorktreeRemoval {
    NothingToRemove,
    Removed,
    Failed,
}

/// Only acts on paths matching the `.claude/worktrees/{session_id}`
/// pattern — the single implementation of the check previously inline in
/// `events::remove_worker_worktree`. `pub(crate)` so callers that need to
/// `gather_git_facts` *before* calling `cleanup()` (e.g. `reap_workers`) can
/// derive the same `repo_root` without re-deriving the suffix check.
pub(crate) fn ninox_repo_root(workspace_path: &str, session_id: &str) -> Option<String> {
    let suffix = format!("/.claude/worktrees/{session_id}");
    let repo_root = workspace_path.strip_suffix(&suffix)?;
    (!repo_root.is_empty()).then(|| repo_root.to_string())
}

async fn remove_worktree_if_present(repo_root: &str, workspace_path: &str) -> WorktreeRemoval {
    if !Path::new(workspace_path).exists() {
        return WorktreeRemoval::NothingToRemove;
    }
    let ok = tokio::process::Command::new("git")
        .args(["-C", repo_root, "worktree", "remove", "--force", workspace_path])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false);
    if ok {
        WorktreeRemoval::Removed
    } else {
        WorktreeRemoval::Failed
    }
}

async fn delete_branch_if_present(repo_root: &str, branch: &str) -> Option<String> {
    let exists = tokio::process::Command::new("git")
        .args(["-C", repo_root, "show-ref", "--verify", "--quiet", &format!("refs/heads/{branch}")])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !exists {
        return None;
    }
    let deleted = tokio::process::Command::new("git")
        .args(["-C", repo_root, "branch", "-D", branch])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false);
    deleted.then(|| branch.to_string())
}

async fn prune_worktrees(repo_root: &str) {
    let _ = tokio::process::Command::new("git")
        .args(["-C", repo_root, "worktree", "prune"])
        .output()
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;
    use tempfile::{tempdir, TempDir};

    fn run_git(dir: &Path, args: &[&str]) {
        let status = Command::new("git").args(args).current_dir(dir).status().unwrap();
        assert!(status.success(), "git {args:?} failed in {dir:?}");
    }

    fn branch_exists(repo: &Path, branch: &str) -> bool {
        Command::new("git")
            .args(["-C", repo.to_str().unwrap(), "show-ref", "--verify", "--quiet", &format!("refs/heads/{branch}")])
            .status()
            .unwrap()
            .success()
    }

    fn facts(overrides: impl FnOnce(&mut WorkspaceFacts)) -> WorkspaceFacts {
        let mut f = WorkspaceFacts {
            dirty: false,
            has_branch: true,
            has_upstream: false,
            unpushed_commit_count: 0,
            commits_beyond_base: 0,
            merged: false,
            commits_since_merge: 0,
            interrupted: false,
            live_session_row: false,
        };
        overrides(&mut f);
        f
    }

    /// `repo/.claude/worktrees/<session_id>` on a fresh branch `<session_id>`
    /// — the exact shape `create_worktree_at` produces in production.
    fn repo_with_worktree(session_id: &str) -> (TempDir, std::path::PathBuf) {
        let repo = tempdir().unwrap();
        run_git(repo.path(), &["init", "-q", "-b", "main"]);
        run_git(repo.path(), &["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "--allow-empty", "-q", "-m", "init"]);
        let worktree_path = repo.path().join(".claude/worktrees").join(session_id);
        run_git(repo.path(), &["worktree", "add", "-q", "-b", session_id, worktree_path.to_str().unwrap()]);
        (repo, worktree_path)
    }

    #[tokio::test]
    async fn proceed_removes_worktree_and_deletes_merged_branch() {
        let session_id = "s1";
        let (repo, worktree_path) = repo_with_worktree(session_id);
        let sessions_dir = tempdir().unwrap();

        let outcome = cleanup(
            session_id,
            worktree_path.to_str().unwrap(),
            Some(session_id),
            facts(|f| f.merged = true),
            CleanupPolicy { force: false },
            true,
            sessions_dir.path(),
        )
        .await
        .unwrap();

        assert!(!worktree_path.exists());
        assert!(!branch_exists(repo.path(), session_id));
        assert!(outcome.worktree_removed);
        assert_eq!(outcome.branch_deleted, Some(session_id.to_string()));
        assert_eq!(outcome.skipped, None);
    }

    #[tokio::test]
    async fn proceed_keeps_unmerged_branch_with_commits() {
        let session_id = "s2";
        let (repo, worktree_path) = repo_with_worktree(session_id);
        let sessions_dir = tempdir().unwrap();

        let outcome = cleanup(
            session_id,
            worktree_path.to_str().unwrap(),
            Some(session_id),
            facts(|f| { f.has_upstream = true; f.commits_beyond_base = 1; }),
            CleanupPolicy { force: false },
            true,
            sessions_dir.path(),
        )
        .await
        .unwrap();

        assert!(!worktree_path.exists());
        assert!(branch_exists(repo.path(), session_id));
        assert!(outcome.worktree_removed);
        assert_eq!(outcome.branch_deleted, None);
    }

    #[tokio::test]
    async fn skip_dirty_without_force_leaves_everything() {
        let session_id = "s3";
        let (repo, worktree_path) = repo_with_worktree(session_id);
        let sessions_dir = tempdir().unwrap();

        let outcome = cleanup(
            session_id,
            worktree_path.to_str().unwrap(),
            Some(session_id),
            facts(|f| f.dirty = true),
            CleanupPolicy { force: false },
            true,
            sessions_dir.path(),
        )
        .await
        .unwrap();

        assert!(worktree_path.exists());
        assert!(branch_exists(repo.path(), session_id));
        assert!(!outcome.worktree_removed);
        assert_eq!(outcome.skipped, Some(SkipReason::Dirty));
    }

    #[tokio::test]
    async fn force_removes_dirty_worktree() {
        let session_id = "s4";
        let (_repo, worktree_path) = repo_with_worktree(session_id);
        let sessions_dir = tempdir().unwrap();

        let outcome = cleanup(
            session_id,
            worktree_path.to_str().unwrap(),
            Some(session_id),
            facts(|f| f.dirty = true),
            CleanupPolicy { force: true },
            true,
            sessions_dir.path(),
        )
        .await
        .unwrap();

        assert!(!worktree_path.exists());
        assert!(outcome.worktree_removed);
        assert_eq!(outcome.skipped, None);
    }

    #[tokio::test]
    async fn idempotent_second_call_on_already_removed_worktree_is_a_noop_ok() {
        let session_id = "s5";
        let (_repo, worktree_path) = repo_with_worktree(session_id);
        let sessions_dir = tempdir().unwrap();
        let f = facts(|f| f.merged = true);

        cleanup(session_id, worktree_path.to_str().unwrap(), Some(session_id), f, CleanupPolicy { force: false }, true, sessions_dir.path())
            .await
            .unwrap();
        assert!(!worktree_path.exists());

        let second = cleanup(session_id, worktree_path.to_str().unwrap(), Some(session_id), f, CleanupPolicy { force: false }, true, sessions_dir.path())
            .await
            .unwrap();

        assert!(!second.worktree_removed);
        assert_eq!(second.branch_deleted, None);
    }

    #[tokio::test]
    async fn removes_artifacts_alongside_worktree() {
        let session_id = "s6";
        let (_repo, worktree_path) = repo_with_worktree(session_id);
        let sessions_dir = tempdir().unwrap();
        std::fs::write(sessions_dir.path().join(format!("{session_id}.json")), "{}").unwrap();

        let outcome = cleanup(
            session_id,
            worktree_path.to_str().unwrap(),
            Some(session_id),
            facts(|f| f.merged = true),
            CleanupPolicy { force: false },
            true,
            sessions_dir.path(),
        )
        .await
        .unwrap();

        assert!(!sessions_dir.path().join(format!("{session_id}.json")).exists());
        assert!(outcome.artifacts_removed);
    }

    #[tokio::test]
    async fn remove_hook_artifacts_false_preserves_them() {
        let session_id = "s8";
        let (_repo, worktree_path) = repo_with_worktree(session_id);
        let sessions_dir = tempdir().unwrap();
        std::fs::write(sessions_dir.path().join(format!("{session_id}.json")), "{}").unwrap();

        let outcome = cleanup(
            session_id,
            worktree_path.to_str().unwrap(),
            Some(session_id),
            facts(|f| f.merged = true),
            CleanupPolicy { force: false },
            false,
            sessions_dir.path(),
        )
        .await
        .unwrap();

        assert!(!worktree_path.exists(), "worktree removal is unaffected by remove_hook_artifacts");
        assert!(sessions_dir.path().join(format!("{session_id}.json")).exists(), "hook artifacts must survive");
        assert!(!outcome.artifacts_removed);
    }

    #[tokio::test]
    async fn non_ninox_path_is_never_touched() {
        let session_id = "s7";
        let outside = tempdir().unwrap();
        let sessions_dir = tempdir().unwrap();

        let outcome = cleanup(
            session_id,
            outside.path().to_str().unwrap(),
            Some(session_id),
            facts(|f| f.merged = true),
            CleanupPolicy { force: false },
            true,
            sessions_dir.path(),
        )
        .await
        .unwrap();

        assert!(outside.path().exists());
        assert!(!outcome.worktree_removed);
    }

    /// Reviewer's Important 5: a `git worktree remove` that fails (e.g. a
    /// locked file needing `--force --force`) must surface as a failure, not
    /// be reported as a successful removal. Simulated by pointing `cleanup`
    /// at a plain directory in the right place that was never actually
    /// registered as a worktree of the repo — `git worktree remove` refuses
    /// it, while the directory itself survives, same shape as a real failure.
    #[tokio::test]
    async fn worktree_removal_failure_is_reported_not_hidden() {
        let session_id = "s9";
        let (repo, _real_wt) = repo_with_worktree("placeholder");
        let fake_wt = repo.path().join(".claude/worktrees").join(session_id);
        std::fs::create_dir_all(&fake_wt).unwrap();
        let sessions_dir = tempdir().unwrap();

        let outcome = cleanup(
            session_id,
            fake_wt.to_str().unwrap(),
            Some(session_id),
            facts(|f| f.merged = true),
            CleanupPolicy { force: false },
            true,
            sessions_dir.path(),
        )
        .await
        .unwrap();

        assert!(fake_wt.exists(), "a failed removal must leave the directory in place");
        assert!(!outcome.worktree_removed);
        assert!(outcome.worktree_removal_failed, "a real git failure must not look like a no-op");
        assert!(outcome.needs_attention());
    }
}
