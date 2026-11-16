use std::path::{Path, PathBuf};

use super::{classify_repo_orphans, decide, CleanupDecision, CleanupPolicy, Ownership, OrphanStatus, SkipReason, WorkspaceFacts};
use crate::store::Store;

/// What `ninox workspaces list`/`show`, the GUI chip/list, and the TUI panel
/// all report for one worktree — purely derived from `OrphanStatus` and
/// `WorkspaceFacts` via [`classify_action`], never by calling `cleanup()`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WorkspaceAction {
    /// Live, not yet past retention, or not a ninox worktree at all.
    None,
    /// `decide()` would proceed. `unmerged_with_content` is the one bucket
    /// that's safe (nothing would be lost — it's fully pushed) but has no
    /// merge behind it yet; `ninox workspaces prune`'s conservative default
    /// withholds it, `--include-unmerged` opts in (see
    /// `sweep_repo_orphans_with_policy`).
    WouldClean { delete_branch: bool, unmerged_with_content: bool },
    /// `decide()` would skip (or an unmarked `LegacyMatch` failed the extra
    /// squeaky-clean check) — needs a human, never auto-pruned.
    NeedsAttention(SkipReason),
}

/// One `.claude/worktrees/<name>` entry's full report row.
pub struct WorkspaceRow {
    pub path: PathBuf,
    /// The session id this worktree is (or was) tied to — from a live/
    /// terminal session row's `workspace_path`, or an ownership marker's
    /// `session_id` once the row itself has expired/vanished. `None` for a
    /// `NotOurs` directory, or a legacy match with no marker to read.
    pub session_id: Option<String>,
    pub status: OrphanStatus,
    pub facts: WorkspaceFacts,
    pub size_bytes: u64,
    pub action: WorkspaceAction,
}

/// Pure: mirrors `sweep_repo_orphans_with_policy`'s own gating exactly (the
/// `LegacyMatch` squeaky-clean check, then `decide()`) so `list`/`show`
/// always agree with what `prune` would actually do.
pub fn classify_action(status: OrphanStatus, facts: &WorkspaceFacts) -> WorkspaceAction {
    let OrphanStatus::Orphan(ownership) = status else {
        return WorkspaceAction::None;
    };
    let legacy_qualifies = !matches!(ownership, Ownership::LegacyMatch) || facts.commits_beyond_base == 0;
    if !legacy_qualifies {
        return WorkspaceAction::NeedsAttention(SkipReason::UnpushedCommits);
    }
    match decide(facts, CleanupPolicy { force: false }) {
        CleanupDecision::Skip(reason) => WorkspaceAction::NeedsAttention(reason),
        CleanupDecision::Proceed { delete_branch } => WorkspaceAction::WouldClean {
            delete_branch,
            unmerged_with_content: !facts.merged && facts.commits_beyond_base > 0,
        },
    }
}

/// Whether `action` is the conservative, merged-or-empty default bucket
/// `ninox workspaces prune` acts on without `--include-unmerged`.
pub fn is_default_prune_candidate(action: WorkspaceAction) -> bool {
    matches!(action, WorkspaceAction::WouldClean { unmerged_with_content: false, .. })
}

/// Whether `action` is the clean/fully-pushed/not-yet-merged bucket
/// `--include-unmerged` additionally opts into.
pub fn is_unmerged_prune_candidate(action: WorkspaceAction) -> bool {
    matches!(action, WorkspaceAction::WouldClean { unmerged_with_content: true, .. })
}

/// Best-effort recursive file-size sum for one worktree directory. A
/// missing path, or any entry this process can't read, is skipped rather
/// than failing the whole walk — one unreadable file in a leftover
/// worktree must not make a report row error out.
pub fn dir_size_bytes(path: &Path) -> u64 {
    if !path.is_dir() {
        return 0;
    }
    walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter_map(|entry| entry.metadata().ok())
        .map(|metadata| metadata.len())
        .sum()
}

/// Read-only: one [`WorkspaceRow`] per `.claude/worktrees/*` entry under
/// `repo_root`. Built on [`classify_repo_orphans`] (itself read-only) plus
/// [`classify_action`] and [`dir_size_bytes`] — never calls `cleanup()`.
pub async fn build_workspace_rows(
    store: &Store, repo_root: &Path, retention_ms: i64,
) -> anyhow::Result<Vec<WorkspaceRow>> {
    let classified = classify_repo_orphans(store, repo_root, retention_ms).await?;
    let sessions = store.list_sessions()?;

    Ok(classified
        .into_iter()
        .map(|(path, status, facts)| {
            let path_str = path.to_string_lossy().to_string();
            let session_id = sessions
                .iter()
                .find(|s| s.workspace_path.as_deref() == Some(path_str.as_str()))
                .map(|s| s.id.clone())
                .or_else(|| super::read_marker(&path).map(|m| m.session_id));
            let action = classify_action(status, &facts);
            let size_bytes = dir_size_bytes(&path);
            WorkspaceRow { path, session_id, status, facts, size_bytes, action }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Session, SessionStatus};
    use std::path::Path;
    use std::process::Command;
    use tempfile::{tempdir, TempDir};

    fn facts(overrides: impl FnOnce(&mut WorkspaceFacts)) -> WorkspaceFacts {
        let mut f = WorkspaceFacts {
            dirty: false,
            has_branch: true,
            has_upstream: true,
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

    // ---- classify_action ----

    #[test]
    fn live_not_orphan_yet_and_not_ours_are_all_none() {
        for status in [OrphanStatus::Live, OrphanStatus::NotOrphanYet, OrphanStatus::NotOurs] {
            assert_eq!(classify_action(status, &facts(|_| {})), WorkspaceAction::None);
        }
    }

    #[test]
    fn merged_orphan_would_clean_and_delete_branch() {
        let f = facts(|f| { f.merged = true; f.commits_beyond_base = 5; });
        assert_eq!(
            classify_action(OrphanStatus::Orphan(Ownership::Tracked), &f),
            WorkspaceAction::WouldClean { delete_branch: true, unmerged_with_content: false },
        );
    }

    #[test]
    fn empty_no_upstream_orphan_would_clean_and_delete_branch() {
        let f = facts(|f| { f.has_upstream = false; f.commits_beyond_base = 0; });
        assert_eq!(
            classify_action(OrphanStatus::Orphan(Ownership::Tracked), &f),
            WorkspaceAction::WouldClean { delete_branch: true, unmerged_with_content: false },
        );
    }

    #[test]
    fn clean_pushed_unmerged_orphan_would_clean_but_keep_branch() {
        let f = facts(|f| f.commits_beyond_base = 2);
        assert_eq!(
            classify_action(OrphanStatus::Orphan(Ownership::Tracked), &f),
            WorkspaceAction::WouldClean { delete_branch: false, unmerged_with_content: true },
        );
    }

    #[test]
    fn dirty_orphan_needs_attention() {
        let f = facts(|f| f.dirty = true);
        assert_eq!(
            classify_action(OrphanStatus::Orphan(Ownership::Tracked), &f),
            WorkspaceAction::NeedsAttention(SkipReason::Dirty),
        );
    }

    #[test]
    fn legacy_match_with_commits_beyond_base_needs_attention_even_if_clean() {
        let f = facts(|f| f.commits_beyond_base = 1);
        assert_eq!(
            classify_action(OrphanStatus::Orphan(Ownership::LegacyMatch), &f),
            WorkspaceAction::NeedsAttention(SkipReason::UnpushedCommits),
        );
    }

    #[test]
    fn legacy_match_squeaky_clean_would_clean() {
        let f = facts(|_| {});
        assert_eq!(
            classify_action(OrphanStatus::Orphan(Ownership::LegacyMatch), &f),
            WorkspaceAction::WouldClean { delete_branch: true, unmerged_with_content: false },
        );
    }

    #[test]
    fn candidate_predicates_partition_would_clean_by_merge_state() {
        let merged = WorkspaceAction::WouldClean { delete_branch: true, unmerged_with_content: false };
        let unmerged = WorkspaceAction::WouldClean { delete_branch: false, unmerged_with_content: true };
        let needs_attention = WorkspaceAction::NeedsAttention(SkipReason::Dirty);

        assert!(is_default_prune_candidate(merged));
        assert!(!is_unmerged_prune_candidate(merged));
        assert!(!is_default_prune_candidate(unmerged));
        assert!(is_unmerged_prune_candidate(unmerged));
        assert!(!is_default_prune_candidate(needs_attention));
        assert!(!is_unmerged_prune_candidate(needs_attention));
    }

    // ---- dir_size_bytes ----

    #[test]
    fn dir_size_bytes_sums_file_sizes_recursively() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "12345").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b.txt"), "1234567890").unwrap();

        assert_eq!(dir_size_bytes(dir.path()), 15);
    }

    #[test]
    fn dir_size_bytes_is_zero_for_a_missing_path() {
        let dir = tempdir().unwrap();
        assert_eq!(dir_size_bytes(&dir.path().join("does-not-exist")), 0);
    }

    // ---- build_workspace_rows ----

    fn run_git(dir: &Path, args: &[&str]) {
        let status = Command::new("git").args(args).current_dir(dir).status().unwrap();
        assert!(status.success(), "git {args:?} failed in {dir:?}");
    }

    fn init_repo() -> TempDir {
        let repo = tempdir().unwrap();
        run_git(repo.path(), &["init", "-q", "-b", "main"]);
        run_git(repo.path(), &["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "--allow-empty", "-q", "-m", "init"]);
        repo
    }

    fn test_session(id: &str, status: SessionStatus, workspace_path: &str, terminal_at: Option<i64>) -> Session {
        Session {
            id: id.into(), orchestrator_id: Some("orch".into()), name: "w".into(),
            repo: "r".into(), status, agent_type: "c".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: Some(workspace_path.into()), pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None,
            terminal_at, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None, worker_type: None,
            success_criteria: Vec::new(), completion_outcome: None, self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        }
    }

    const RETENTION_MS: i64 = 2 * 24 * 60 * 60 * 1000;

    #[tokio::test]
    async fn build_workspace_rows_reports_live_and_orphan_rows_without_deleting_anything() {
        let repo = init_repo();
        let live_wt = repo.path().join(".claude").join("worktrees").join("live-one");
        run_git(repo.path(), &["worktree", "add", "-q", "-b", "live-one", live_wt.to_str().unwrap()]);
        std::fs::write(live_wt.join("payload.bin"), vec![0u8; 100]).unwrap();

        let store = Store::open(tempdir().unwrap().keep().join("t.db")).unwrap();
        store.upsert_session(&test_session("live-one", SessionStatus::Working, live_wt.to_str().unwrap(), None)).unwrap();

        let rows = build_workspace_rows(&store, repo.path(), RETENTION_MS).await.unwrap();

        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.session_id.as_deref(), Some("live-one"));
        assert_eq!(row.status, OrphanStatus::Live);
        assert_eq!(row.action, WorkspaceAction::None);
        assert!(row.size_bytes >= 100);
        assert!(live_wt.exists(), "build_workspace_rows must never delete anything");
    }
}
