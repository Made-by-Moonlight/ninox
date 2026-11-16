use std::path::{Path, PathBuf};

use super::{
    cleanup, commits_since, commits_since_sha, gather_git_facts, ninox_repo_root, read_marker, resolve_default_base_ref,
    CleanupOutcome, CleanupPolicy, SkipReason, WorkspaceFacts, WorkspaceMarker,
};
use crate::store::Store;
use crate::types::{Session, SessionStatus};

/// How confident the sweeper is that ninox created a given orphaned
/// worktree. `Tracked` (an ownership marker, or a still-present but expired
/// session row) is trusted the same as a normal cleanup; `LegacyMatch` (no
/// marker, inferred purely from `branch == dir_name`) is trusted only when
/// squeaky clean. `NotOurs` worktrees are never wrapped in `Orphan` — see
/// `OrphanStatus::NotOurs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ownership {
    Tracked,
    LegacyMatch,
    NotOurs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrphanStatus {
    /// A session row exists and isn't finished for good (`Done`/`Terminated`)
    /// — includes `Interrupted`, which is resumable, not an orphan.
    Live,
    /// A session row exists, is finished for good, but hasn't crossed the
    /// retention window yet — the normal retention sweep will reach it.
    NotOrphanYet,
    /// No session row, and nothing (marker or branch-name match) ties this
    /// directory to ninox. Never touched.
    NotOurs,
    Orphan(Ownership),
}

/// What one orphan-sweep pass did (or didn't) to one worktree directory.
pub struct SweepAction {
    pub path: PathBuf,
    pub status: OrphanStatus,
    /// `None` when `status` wasn't `Orphan(..)`, or cleanup itself errored.
    pub outcome: Option<CleanupOutcome>,
}

pub struct SweepReport {
    pub actions: Vec<SweepAction>,
}

/// A marker younger than this is treated as `NotOrphanYet` even with no
/// session row — closes the race between `create_worktree_at` writing the
/// marker and the spawn path inserting the session row a moment later,
/// which would otherwise let a sweep land in between and see a brand-new,
/// clean, zero-commit worktree as an already-expired tracked orphan.
const SPAWN_GRACE_MS: i64 = 5 * 60 * 1000;

/// Named arguments for [`classify_worktree`] — see that function's doc for
/// what each field means; grouped to stay under clippy's argument-count
/// lint, with every field still named at the one call site.
pub struct ClassifyWorktreeArgs<'a> {
    pub dir_name: &'a str,
    pub git_branch: Option<&'a str>,
    pub marker: Option<&'a WorkspaceMarker>,
    pub has_ninox_fingerprint: bool,
    pub session_row: Option<&'a Session>,
    pub restore_pending: bool,
    pub now: i64,
    pub retention_ms: i64,
}

/// Classifies one `.claude/worktrees/<dir_name>` entry given what's already
/// known about it. `session_row` is `Some` only if a session row's
/// `workspace_path` still points at this exact directory (live, or terminal
/// but not yet past the retention window). `has_ninox_fingerprint` is
/// whether the directory contains a file every ninox-created worktree gets
/// at spawn time (`.claude/settings.local.json`, written by
/// `ensure_statusline_settings` — see the caller, `classify_repo_orphans`).
/// `restore_pending` is whether a fleet restore is still waiting on this
/// exact session (`fleet::awaits_restore`) — a restore candidate is never an
/// orphan no matter how long it's been terminal.
///
/// `branch == dir_name` alone is not enough evidence for `LegacyMatch`: a
/// human running `git worktree add .claude/worktrees/<branch> <branch>` by
/// hand (ninox's own documented convention) produces that exact shape too.
/// Requiring the fingerprint as well means a `LegacyMatch` directory is one
/// ninox is known to have touched at some point, just before ownership
/// markers existed or after one was lost — not merely shaped like one.
pub fn classify_worktree(args: ClassifyWorktreeArgs<'_>) -> OrphanStatus {
    let ClassifyWorktreeArgs { dir_name, git_branch, marker, has_ninox_fingerprint, session_row, restore_pending, now, retention_ms } = args;
    if let Some(session) = session_row {
        let finished_for_good = matches!(session.status, SessionStatus::Done | SessionStatus::Terminated);
        if !finished_for_good || restore_pending {
            return OrphanStatus::Live;
        }
        let expired = session.terminal_at.is_some_and(|t| now.saturating_sub(t) >= retention_ms);
        return if expired {
            OrphanStatus::Orphan(Ownership::Tracked)
        } else {
            OrphanStatus::NotOrphanYet
        };
    }

    if let Some(m) = marker.filter(|m| m.session_id == dir_name) {
        return if now.saturating_sub(m.created_at) < SPAWN_GRACE_MS {
            OrphanStatus::NotOrphanYet
        } else {
            OrphanStatus::Orphan(Ownership::Tracked)
        };
    }
    if has_ninox_fingerprint && git_branch == Some(dir_name) {
        return OrphanStatus::Orphan(Ownership::LegacyMatch);
    }
    OrphanStatus::NotOurs
}

/// Every distinct repo root implied by `{workspace_path}`s ending in
/// `/.claude/worktrees/<name>` across all known session rows.
pub fn discover_repo_roots(store: &Store) -> anyhow::Result<Vec<PathBuf>> {
    let sessions = store.list_sessions()?;
    let mut roots = std::collections::BTreeSet::new();
    for session in &sessions {
        if let Some(workspace_path) = &session.workspace_path {
            if let Some(repo_root) = ninox_repo_root(workspace_path, &session.id) {
                roots.insert(PathBuf::from(repo_root));
            }
        }
    }
    Ok(roots.into_iter().collect())
}

/// Relative path every ninox-created worktree gets at spawn time
/// (`ensure_statusline_settings`), used as the fingerprint a `LegacyMatch`
/// requires on top of a bare branch/dir-name coincidence.
const NINOX_FINGERPRINT_PATH: &str = ".claude/settings.local.json";

/// Read-only: discovers every `.claude/worktrees/*` entry under `repo_root`,
/// classifies each, and gathers its `WorkspaceFacts`. Never calls
/// `cleanup()` and never deletes anything.
pub async fn classify_repo_orphans(
    store: &Store,
    repo_root: &Path,
    retention_ms: i64,
) -> anyhow::Result<Vec<(PathBuf, OrphanStatus, WorkspaceFacts)>> {
    let worktrees_dir = repo_root.join(".claude").join("worktrees");
    let entries = match std::fs::read_dir(&worktrees_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            tracing::warn!("orphan classify {}: read_dir: {e}", worktrees_dir.display());
            return Ok(Vec::new());
        }
    };

    let sessions = store.list_sessions()?;
    let fleet_records = store.fleet_records().unwrap_or_default();
    let now = crate::lifecycle::poller::now_millis();
    let base_ref = resolve_default_base_ref(repo_root);

    let mut results = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(dir_name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        let path_str = path.to_string_lossy().to_string();

        let session_row = sessions.iter().find(|s| s.workspace_path.as_deref() == Some(path_str.as_str()));
        let marker = read_marker(&path);
        let git_branch = crate::fleet::probe::current_branch(&path);
        let has_fingerprint = path.join(NINOX_FINGERPRINT_PATH).exists();
        let restore_pending = session_row.is_some_and(|s| {
            fleet_records.get(dir_name).is_some_and(|r| crate::fleet::awaits_restore(s, r))
        });

        let status = classify_worktree(ClassifyWorktreeArgs {
            dir_name, git_branch: git_branch.as_deref(), marker: marker.as_ref(),
            has_ninox_fingerprint: has_fingerprint, session_row, restore_pending, now, retention_ms,
        });

        // `gather_git_facts` itself always prefers whatever's actually
        // checked out over this hint (it only falls back to it once the
        // worktree is gone) — the recorded branch (marker, else the fleet
        // record) is passed here only for that fallback and to compare
        // against `git_branch` below for the eventual branch-deletion
        // target, which — unlike the safety facts — stays conservative:
        // `sweep_repo_orphans` must never touch a branch other than the one
        // it knows is ours.
        let recorded_branch = marker.as_ref().map(|m| m.branch.clone())
            .or_else(|| fleet_records.get(dir_name).and_then(|r| r.branch.clone()));

        let mut facts = gather_git_facts(repo_root, &path, recorded_branch.as_deref(), base_ref.as_deref());
        facts.live_session_row = matches!(status, OrphanStatus::Live);
        facts.interrupted = session_row.is_some_and(|s| matches!(s.status, SessionStatus::Interrupted));
        // `merged_at` only vouches for the recorded branch's content — if
        // the worktree has since moved on to a different one (a session
        // that kept working before going terminal and expiring here), that
        // merge says nothing about what's actually checked out now.
        let merged_matches_checked_out = session_row.is_some_and(|s| s.merged_at.is_some())
            && git_branch.is_some() && git_branch == recorded_branch;
        facts.merged = merged_matches_checked_out;
        if merged_matches_checked_out {
            if let Some(b) = git_branch.as_deref() {
                // Same preference as the live-session cleanup path: GitHub's
                // own merged head sha over a `merged_at` timestamp, which
                // can only see commits made after the poller's own
                // detection tick, not the real merge moment.
                let merged_head_sha = fleet_records.get(dir_name).and_then(|r| r.merged_head_sha.clone());
                facts.commits_since_merge = merged_head_sha.as_deref()
                    .and_then(|sha| commits_since_sha(repo_root, b, sha))
                    .unwrap_or_else(|| {
                        let since = session_row.and_then(|s| s.merged_at).unwrap_or(now);
                        commits_since(repo_root, b, since)
                    });
            }
        }

        results.push((path, status, facts));
    }
    Ok(results)
}

/// One repo's worth of orphan discovery + action: classifies first, then
/// cleans every `Orphan(..)` entry that qualifies — `LegacyMatch` only when
/// `commits_beyond_base == 0` on top of whatever `decide()` already
/// requires, since an unmarked match is never trusted with the "merged"
/// leniency a `Tracked` orphan gets. Equivalent to
/// `sweep_repo_orphans_with_policy(.., include_unmerged: true)` — the
/// periodic poller's behavior is unchanged by that flag's existence.
pub async fn sweep_repo_orphans(store: &Store, repo_root: &Path, retention_ms: i64) -> anyhow::Result<SweepReport> {
    sweep_repo_orphans_with_policy(store, repo_root, retention_ms, true).await
}

/// Like [`sweep_repo_orphans`], but when `include_unmerged` is `false` also
/// withholds the one bucket `decide()` alone would otherwise let through
/// despite having no merge behind it: a clean, fully-pushed branch with
/// real commits beyond base that just hasn't been merged yet (`decide()`'s
/// `Proceed { delete_branch: false }` case — safe to touch because nothing
/// would be lost, but still "unmerged work", which `ninox workspaces
/// prune`'s conservative default (merged-or-empty only) holds back unless
/// the caller opts in with `--include-unmerged`). That bucket's
/// `SweepAction::outcome` is `None` when withheld — distinct from both a
/// cleaned action and a genuine safety skip, since it was never attempted
/// this run rather than refused.
pub async fn sweep_repo_orphans_with_policy(
    store: &Store, repo_root: &Path, retention_ms: i64, include_unmerged: bool,
) -> anyhow::Result<SweepReport> {
    let classified = classify_repo_orphans(store, repo_root, retention_ms).await?;
    let sessions_dir = crate::config::AppConfig::sessions_dir();
    let fleet_records = store.fleet_records().unwrap_or_default();

    let mut actions = Vec::with_capacity(classified.len());
    for (path, status, facts) in classified {
        let outcome = match status {
            OrphanStatus::Orphan(ownership) => {
                let legacy_qualifies = !matches!(ownership, Ownership::LegacyMatch) || facts.commits_beyond_base == 0;
                let unmerged_with_content = !facts.merged && facts.commits_beyond_base > 0;
                if legacy_qualifies && !(unmerged_with_content && !include_unmerged) {
                    // Same recorded-branch preference as classify_repo_orphans:
                    // never the directory's current HEAD for anything other
                    // than a LegacyMatch, whose branch *is* `dir_name` by
                    // construction of that match.
                    let dir_name = path.file_name().and_then(|n| n.to_str());
                    let branch = match ownership {
                        Ownership::LegacyMatch => dir_name.map(str::to_string),
                        _ => {
                            let recorded = read_marker(&path).map(|m| m.branch)
                                .or_else(|| dir_name.and_then(|n| fleet_records.get(n).and_then(|r| r.branch.clone())));
                            // Only delete the branch we recorded as ours
                            // when it's still what's actually checked out —
                            // if HEAD has moved on since (the worktree kept
                            // working before going terminal and expiring
                            // here), the recorded name is no longer
                            // something we're sure is safe to touch, so
                            // leave branch deletion alone and only remove
                            // the worktree.
                            recorded.filter(|r| crate::fleet::probe::current_branch(&path).as_deref() == Some(r.as_str()))
                        }
                    };
                    clean_one_orphan(&path, branch, facts, &sessions_dir).await
                } else if !legacy_qualifies {
                    // A LegacyMatch that failed the extra squeaky-clean check
                    // is needs-attention, not silently skipped — it only
                    // reaches here because `facts` already says it's safe by
                    // every OTHER measure, so leaving `outcome: None` would
                    // make it indistinguishable from "never even considered".
                    Some(CleanupOutcome {
                        worktree_removed: false,
                        worktree_removal_failed: false,
                        branch_deleted: None,
                        artifacts_removed: false,
                        skipped: Some(SkipReason::UnpushedCommits),
                    })
                } else {
                    // Eligible (legacy_qualifies) but unmerged-with-content
                    // and the caller didn't opt into that bucket this run —
                    // not a safety skip, just not attempted.
                    None
                }
            }
            OrphanStatus::Live | OrphanStatus::NotOrphanYet | OrphanStatus::NotOurs => None,
        };
        actions.push(SweepAction { path, status, outcome });
    }
    Ok(SweepReport { actions })
}

async fn clean_one_orphan(path: &Path, branch: Option<String>, facts: WorkspaceFacts, sessions_dir: &Path) -> Option<CleanupOutcome> {
    let dir_name = path.file_name()?.to_str()?;
    let path_str = path.to_string_lossy().to_string();

    match cleanup(dir_name, &path_str, branch.as_deref(), facts, CleanupPolicy { force: false }, true, sessions_dir).await {
        Ok(outcome) => Some(outcome),
        Err(e) => {
            tracing::warn!("orphan sweep cleanup {}: {e}", path.display());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::write_marker;
    use crate::store::Store;
    use crate::types::{Session, SessionStatus};
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

    fn init_repo() -> TempDir {
        let repo = tempdir().unwrap();
        run_git(repo.path(), &["init", "-q", "-b", "main"]);
        run_git(repo.path(), &["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "--allow-empty", "-q", "-m", "init"]);
        repo
    }

    /// Mirrors a real ninox worktree's shape, including the
    /// `.claude/settings.local.json` fingerprint every ninox spawn seeds
    /// (`ensure_statusline_settings`) — required since Critical 3's fix for
    /// a `LegacyMatch` to ever be considered at all. Tests that need a
    /// directory ninox did *not* create use a raw `run_git` call instead.
    fn add_worktree_branch(repo: &Path, branch: &str) -> std::path::PathBuf {
        let wt = repo.join(".claude").join("worktrees").join(branch);
        run_git(repo, &["worktree", "add", "-q", "-b", branch, wt.to_str().unwrap()]);
        std::fs::create_dir_all(wt.join(".claude")).unwrap();
        std::fs::write(wt.join(NINOX_FINGERPRINT_PATH), "{}").unwrap();
        wt
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

    // ---- classify_worktree (pure) ----

    #[test]
    fn live_session_row_is_live() {
        let session = test_session("s1", SessionStatus::Working, "/r/.claude/worktrees/s1", None);
        let status = classify_worktree(ClassifyWorktreeArgs { dir_name: "s1", git_branch: None, marker: None, has_ninox_fingerprint: false, session_row: Some(&session), restore_pending: false, now: 10_000, retention_ms: RETENTION_MS });
        assert_eq!(status, OrphanStatus::Live);
    }

    #[test]
    fn interrupted_session_row_is_live() {
        let session = test_session("s1", SessionStatus::Interrupted, "/r/.claude/worktrees/s1", None);
        let status = classify_worktree(ClassifyWorktreeArgs { dir_name: "s1", git_branch: None, marker: None, has_ninox_fingerprint: false, session_row: Some(&session), restore_pending: false, now: 10_000, retention_ms: RETENTION_MS });
        assert_eq!(status, OrphanStatus::Live, "Interrupted is resumable, not an orphan");
    }

    #[test]
    fn terminal_row_within_retention_is_not_orphan_yet() {
        let now = 10_000_000;
        let session = test_session("s1", SessionStatus::Done, "/r/.claude/worktrees/s1", Some(now - 1_000));
        let status = classify_worktree(ClassifyWorktreeArgs { dir_name: "s1", git_branch: None, marker: None, has_ninox_fingerprint: false, session_row: Some(&session), restore_pending: false, now, retention_ms: RETENTION_MS });
        assert_eq!(status, OrphanStatus::NotOrphanYet);
    }

    #[test]
    fn terminal_row_past_retention_is_tracked_orphan() {
        let now = 10_000_000_000;
        let session = test_session("s1", SessionStatus::Done, "/r/.claude/worktrees/s1", Some(0));
        let status = classify_worktree(ClassifyWorktreeArgs { dir_name: "s1", git_branch: None, marker: None, has_ninox_fingerprint: false, session_row: Some(&session), restore_pending: false, now, retention_ms: RETENTION_MS });
        assert_eq!(status, OrphanStatus::Orphan(Ownership::Tracked));
    }

    /// Reviewer's Important 9: a restore candidate is never an orphan, no
    /// matter how far past retention its terminal_at is.
    #[test]
    fn terminal_row_past_retention_but_restore_pending_is_live() {
        let now = 10_000_000_000;
        let session = test_session("s1", SessionStatus::Terminated, "/r/.claude/worktrees/s1", Some(0));
        let status = classify_worktree(ClassifyWorktreeArgs { dir_name: "s1", git_branch: None, marker: None, has_ninox_fingerprint: false, session_row: Some(&session), restore_pending: true, now, retention_ms: RETENTION_MS });
        assert_eq!(status, OrphanStatus::Live);
    }

    #[test]
    fn no_row_with_marker_is_tracked_orphan_once_past_the_spawn_grace_period() {
        let marker = WorkspaceMarker { session_id: "s1".into(), repo: "/r".into(), branch: "s1".into(), created_at: 0 };
        let status = classify_worktree(ClassifyWorktreeArgs { dir_name: "s1", git_branch: None, marker: Some(&marker), has_ninox_fingerprint: false, session_row: None, restore_pending: false, now: 10_000_000, retention_ms: RETENTION_MS });
        assert_eq!(status, OrphanStatus::Orphan(Ownership::Tracked));
    }

    /// Reviewer's Important 8: closes the race between `create_worktree_at`
    /// writing the marker and the spawn path inserting the session row.
    #[test]
    fn no_row_with_a_very_recent_marker_is_not_orphan_yet() {
        let marker = WorkspaceMarker { session_id: "s1".into(), repo: "/r".into(), branch: "s1".into(), created_at: 10_000 };
        let status = classify_worktree(ClassifyWorktreeArgs { dir_name: "s1", git_branch: None, marker: Some(&marker), has_ninox_fingerprint: false, session_row: None, restore_pending: false, now: 15_000, retention_ms: RETENTION_MS });
        assert_eq!(status, OrphanStatus::NotOrphanYet, "a session row insert may just be a moment away");
    }

    #[test]
    fn no_row_no_marker_branch_matches_dirname_but_no_fingerprint_is_not_ours() {
        // Reviewer's Critical 3: a human's own `git worktree add
        // .claude/worktrees/<branch> <branch>` produces this exact shape —
        // branch == dir_name alone must never be enough.
        let status = classify_worktree(ClassifyWorktreeArgs { dir_name: "s1", git_branch: Some("s1"), marker: None, has_ninox_fingerprint: false, session_row: None, restore_pending: false, now: 10_000, retention_ms: RETENTION_MS });
        assert_eq!(status, OrphanStatus::NotOurs);
    }

    #[test]
    fn no_row_no_marker_branch_matches_dirname_with_fingerprint_is_legacy_orphan() {
        let status = classify_worktree(ClassifyWorktreeArgs { dir_name: "s1", git_branch: Some("s1"), marker: None, has_ninox_fingerprint: true, session_row: None, restore_pending: false, now: 10_000, retention_ms: RETENTION_MS });
        assert_eq!(status, OrphanStatus::Orphan(Ownership::LegacyMatch));
    }

    #[test]
    fn no_row_no_marker_branch_mismatch_is_not_ours() {
        let status = classify_worktree(ClassifyWorktreeArgs { dir_name: "s1", git_branch: Some("some-human-branch"), marker: None, has_ninox_fingerprint: true, session_row: None, restore_pending: false, now: 10_000, retention_ms: RETENTION_MS });
        assert_eq!(status, OrphanStatus::NotOurs);
    }

    #[test]
    fn no_row_no_marker_no_branch_is_not_ours() {
        let status = classify_worktree(ClassifyWorktreeArgs { dir_name: "s1", git_branch: None, marker: None, has_ninox_fingerprint: false, session_row: None, restore_pending: false, now: 10_000, retention_ms: RETENTION_MS });
        assert_eq!(status, OrphanStatus::NotOurs);
    }

    // ---- classify_repo_orphans (read-only) ----

    #[tokio::test]
    async fn classify_never_deletes_a_safe_orphan() {
        let repo = init_repo();
        let wt = add_worktree_branch(repo.path(), "orphan-clean");
        let marker = WorkspaceMarker { session_id: "orphan-clean".into(), repo: repo.path().to_string_lossy().into(), branch: "orphan-clean".into(), created_at: 0 };
        write_marker(&wt, &marker).unwrap();
        let store = Store::open(tempdir().unwrap().keep().join("t.db")).unwrap();

        let results = classify_repo_orphans(&store, repo.path(), RETENTION_MS).await.unwrap();

        assert!(wt.exists(), "classify must never delete anything");
        let (_path, status, facts) = results.iter().find(|(p, _, _)| p == &wt).unwrap();
        assert_eq!(*status, OrphanStatus::Orphan(Ownership::Tracked));
        assert!(!facts.dirty);
    }

    #[tokio::test]
    async fn classify_reports_facts_for_every_entry_including_live_and_not_ours() {
        let repo = init_repo();
        let live_wt = add_worktree_branch(repo.path(), "live-one");
        let orphan_wt = add_worktree_branch(repo.path(), "orphan-one");
        let marker = WorkspaceMarker { session_id: "orphan-one".into(), repo: repo.path().to_string_lossy().into(), branch: "orphan-one".into(), created_at: 0 };
        write_marker(&orphan_wt, &marker).unwrap();
        let not_ours_wt = repo.path().join(".claude").join("worktrees").join("human-dir");
        run_git(repo.path(), &["worktree", "add", "-q", not_ours_wt.to_str().unwrap()]);

        let store = Store::open(tempdir().unwrap().keep().join("t.db")).unwrap();
        let live = test_session("live-one", SessionStatus::Working, live_wt.to_str().unwrap(), None);
        store.upsert_session(&live).unwrap();

        let results = classify_repo_orphans(&store, repo.path(), RETENTION_MS).await.unwrap();

        assert_eq!(results.len(), 3);
        let status_of = |p: &std::path::Path| results.iter().find(|(path, _, _)| path == p).unwrap().1;
        assert_eq!(status_of(&live_wt), OrphanStatus::Live);
        assert_eq!(status_of(&orphan_wt), OrphanStatus::Orphan(Ownership::Tracked));
        // No ninox fingerprint (created via a raw `git worktree add`, not
        // the `add_worktree_branch` helper) — branch == dir_name alone is
        // never enough (Critical 3), so this must always be NotOurs, not
        // just "maybe LegacyMatch".
        assert_eq!(status_of(&not_ours_wt), OrphanStatus::NotOurs);
    }

    // ---- sweep_repo_orphans ----

    #[tokio::test]
    async fn clean_pushed_tracked_orphan_is_auto_cleaned() {
        let _iso = crate::test_isolation::isolate();
        let repo = init_repo();
        let wt = add_worktree_branch(repo.path(), "tracked-clean");
        let marker = WorkspaceMarker { session_id: "tracked-clean".into(), repo: repo.path().to_string_lossy().into(), branch: "tracked-clean".into(), created_at: 0 };
        write_marker(&wt, &marker).unwrap();
        let store = Store::open(tempdir().unwrap().keep().join("t.db")).unwrap();

        let report = sweep_repo_orphans(&store, repo.path(), RETENTION_MS).await.unwrap();

        assert!(!wt.exists(), "a clean, no-upstream-empty Tracked orphan must be auto-cleaned");
        let action = report.actions.iter().find(|a| a.path == wt).unwrap();
        assert!(action.outcome.as_ref().is_some_and(|o| o.worktree_removed));
    }

    /// A worker reusing one worktree across several tasks can go terminal
    /// and expire here with `merged_at` set from its first task's PR while
    /// HEAD has since moved on to a different, unpushed branch — the merge
    /// must not vouch for that branch, and the recorded (now-stale) one
    /// must never be deleted once HEAD has moved off it.
    #[tokio::test]
    async fn tracked_orphan_whose_head_switched_branches_after_merge_is_needs_attention_not_deleted() {
        let _iso = crate::test_isolation::isolate();
        let repo = init_repo();
        let wt = add_worktree_branch(repo.path(), "task-a");
        let marker = WorkspaceMarker { session_id: "tryout-orphan".into(), repo: repo.path().to_string_lossy().into(), branch: "task-a".into(), created_at: 0 };
        write_marker(&wt, &marker).unwrap();
        run_git(&wt, &["checkout", "-q", "-b", "task-b"]);
        run_git(&wt, &["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "--allow-empty", "-q", "-m", "task b work"]);

        let store = Store::open(tempdir().unwrap().keep().join("t.db")).unwrap();
        let mut session = test_session("tryout-orphan", SessionStatus::Terminated, &wt.to_string_lossy(), Some(0));
        session.merged_at = Some(1_000);
        store.upsert_session(&session).unwrap();

        let report = sweep_repo_orphans(&store, repo.path(), RETENTION_MS).await.unwrap();

        assert!(wt.exists(), "task-b's unpushed commit must block removal even though task-a is recorded merged");
        let action = report.actions.iter().find(|a| a.path == wt).unwrap();
        let outcome = action.outcome.as_ref().unwrap();
        assert!(!outcome.worktree_removed);
        assert_eq!(outcome.branch_deleted, None, "must never delete task-a either, since HEAD has moved off it");
    }

    #[tokio::test]
    async fn dirty_tracked_orphan_is_needs_attention_not_deleted() {
        let _iso = crate::test_isolation::isolate();
        let repo = init_repo();
        let wt = add_worktree_branch(repo.path(), "tracked-dirty");
        let marker = WorkspaceMarker { session_id: "tracked-dirty".into(), repo: repo.path().to_string_lossy().into(), branch: "tracked-dirty".into(), created_at: 0 };
        write_marker(&wt, &marker).unwrap();
        std::fs::write(wt.join("scratch.txt"), "wip").unwrap();
        let store = Store::open(tempdir().unwrap().keep().join("t.db")).unwrap();

        let report = sweep_repo_orphans(&store, repo.path(), RETENTION_MS).await.unwrap();

        assert!(wt.exists(), "dirty orphan must never be auto-deleted");
        let action = report.actions.iter().find(|a| a.path == wt).unwrap();
        assert_eq!(action.outcome.as_ref().unwrap().skipped, Some(SkipReason::Dirty));
    }

    #[tokio::test]
    async fn legacy_orphan_with_commits_beyond_base_is_needs_attention_even_if_pushed() {
        let _iso = crate::test_isolation::isolate();
        let repo = init_repo();
        let remote = tempdir().unwrap();
        run_git(remote.path(), &["init", "-q", "--bare", "-b", "main"]);
        run_git(repo.path(), &["remote", "add", "origin", remote.path().to_str().unwrap()]);
        run_git(repo.path(), &["push", "-q", "origin", "main"]);
        run_git(repo.path(), &["remote", "set-head", "origin", "-a"]);
        let wt = add_worktree_branch(repo.path(), "legacy-ahead");
        run_git(repo.path(), &["push", "-q", "-u", "origin", "legacy-ahead"]);
        run_git(&wt, &["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "--allow-empty", "-q", "-m", "extra"]);
        run_git(repo.path(), &["push", "-q", "origin", "legacy-ahead"]);
        let store = Store::open(tempdir().unwrap().keep().join("t.db")).unwrap();

        let report = sweep_repo_orphans(&store, repo.path(), RETENTION_MS).await.unwrap();

        assert!(wt.exists(), "a legacy (unmarked) orphan with commits beyond base must never be auto-cleaned, even pushed");
        let action = report.actions.iter().find(|a| a.path == wt).unwrap();
        let outcome = action.outcome.as_ref().expect("a failed legacy check must surface as needs-attention, not a silent None");
        assert!(!outcome.worktree_removed);
        assert!(outcome.needs_attention(), "reviewer's Important 10: this must never look like a plain no-op");
    }

    #[tokio::test]
    async fn legacy_orphan_squeaky_clean_is_auto_cleaned() {
        let _iso = crate::test_isolation::isolate();
        let repo = init_repo();
        let wt = add_worktree_branch(repo.path(), "legacy-clean");
        let store = Store::open(tempdir().unwrap().keep().join("t.db")).unwrap();

        let report = sweep_repo_orphans(&store, repo.path(), RETENTION_MS).await.unwrap();

        assert!(!wt.exists(), "a squeaky-clean legacy orphan (no marker, branch==dirname, no commits beyond base) must be auto-cleaned");
        let action = report.actions.iter().find(|a| a.path == wt).unwrap();
        assert_eq!(action.status, OrphanStatus::Orphan(Ownership::LegacyMatch));
    }

    /// A clean, fully-pushed branch with real commits beyond base but no
    /// merge yet is exactly the bucket `decide()` alone would let through
    /// (`Proceed { delete_branch: false }`) — `sweep_repo_orphans_with_policy`
    /// must withhold it by default (`include_unmerged: false`), the
    /// conservative default `ninox workspaces prune` wants.
    #[tokio::test]
    async fn unmerged_clean_pushed_orphan_is_excluded_when_include_unmerged_is_false() {
        let _iso = crate::test_isolation::isolate();
        let repo = init_repo();
        let remote = tempdir().unwrap();
        run_git(remote.path(), &["init", "-q", "--bare", "-b", "main"]);
        run_git(repo.path(), &["remote", "add", "origin", remote.path().to_str().unwrap()]);
        run_git(repo.path(), &["push", "-q", "origin", "main"]);
        run_git(repo.path(), &["remote", "set-head", "origin", "-a"]);
        let wt = add_worktree_branch(repo.path(), "tracked-unmerged");
        let marker = WorkspaceMarker { session_id: "tracked-unmerged".into(), repo: repo.path().to_string_lossy().into(), branch: "tracked-unmerged".into(), created_at: 0 };
        write_marker(&wt, &marker).unwrap();
        run_git(&wt, &["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "--allow-empty", "-q", "-m", "real work"]);
        run_git(repo.path(), &["push", "-q", "-u", "origin", "tracked-unmerged"]);
        let store = Store::open(tempdir().unwrap().keep().join("t.db")).unwrap();

        let report = sweep_repo_orphans_with_policy(&store, repo.path(), RETENTION_MS, false).await.unwrap();

        assert!(wt.exists(), "unmerged content must survive without --include-unmerged");
        let action = report.actions.iter().find(|a| a.path == wt).unwrap();
        assert!(action.outcome.is_none(), "withheld by policy, not a safety skip");
    }

    #[tokio::test]
    async fn unmerged_clean_pushed_orphan_is_cleaned_when_include_unmerged_is_true() {
        let _iso = crate::test_isolation::isolate();
        let repo = init_repo();
        let remote = tempdir().unwrap();
        run_git(remote.path(), &["init", "-q", "--bare", "-b", "main"]);
        run_git(repo.path(), &["remote", "add", "origin", remote.path().to_str().unwrap()]);
        run_git(repo.path(), &["push", "-q", "origin", "main"]);
        run_git(repo.path(), &["remote", "set-head", "origin", "-a"]);
        let wt = add_worktree_branch(repo.path(), "tracked-unmerged-2");
        let marker = WorkspaceMarker { session_id: "tracked-unmerged-2".into(), repo: repo.path().to_string_lossy().into(), branch: "tracked-unmerged-2".into(), created_at: 0 };
        write_marker(&wt, &marker).unwrap();
        run_git(&wt, &["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "--allow-empty", "-q", "-m", "real work"]);
        run_git(repo.path(), &["push", "-q", "-u", "origin", "tracked-unmerged-2"]);
        let store = Store::open(tempdir().unwrap().keep().join("t.db")).unwrap();

        let report = sweep_repo_orphans_with_policy(&store, repo.path(), RETENTION_MS, true).await.unwrap();

        assert!(!wt.exists(), "worktree is removed once opted in");
        assert!(branch_exists(repo.path(), "tracked-unmerged-2"), "branch stays — never merged");
        let action = report.actions.iter().find(|a| a.path == wt).unwrap();
        let outcome = action.outcome.as_ref().unwrap();
        assert!(outcome.worktree_removed);
        assert_eq!(outcome.branch_deleted, None);
    }

    #[tokio::test]
    async fn not_ours_directory_is_never_touched() {
        let _iso = crate::test_isolation::isolate();
        let repo = init_repo();
        let wt = repo.path().join(".claude").join("worktrees").join("human-dir");
        run_git(repo.path(), &["worktree", "add", "-q", "-b", "a-persons-feature", wt.to_str().unwrap()]);
        let store = Store::open(tempdir().unwrap().keep().join("t.db")).unwrap();

        let report = sweep_repo_orphans(&store, repo.path(), RETENTION_MS).await.unwrap();

        assert!(wt.exists());
        let action = report.actions.iter().find(|a| a.path == wt).unwrap();
        assert_eq!(action.status, OrphanStatus::NotOurs);
        assert!(action.outcome.is_none());
    }

    #[tokio::test]
    async fn interrupted_session_row_is_never_swept() {
        let _iso = crate::test_isolation::isolate();
        let repo = init_repo();
        let wt = add_worktree_branch(repo.path(), "interrupted-one");
        let store = Store::open(tempdir().unwrap().keep().join("t.db")).unwrap();
        let session = test_session("interrupted-one", SessionStatus::Interrupted, wt.to_str().unwrap(), None);
        store.upsert_session(&session).unwrap();

        let report = sweep_repo_orphans(&store, repo.path(), RETENTION_MS).await.unwrap();

        assert!(wt.exists());
        let action = report.actions.iter().find(|a| a.path == wt).unwrap();
        assert_eq!(action.status, OrphanStatus::Live);
    }

    #[tokio::test]
    async fn sweep_never_runs_against_a_real_default_appconfig_or_home_dir() {
        let _iso = crate::test_isolation::isolate();
        // Documents the isolation constraint: both functions take an explicit
        // repo_root and Store, never resolving AppConfig::load() or a real
        // home-directory path themselves.
        let repo = init_repo();
        let store = Store::open(tempdir().unwrap().keep().join("t.db")).unwrap();
        let report = sweep_repo_orphans(&store, repo.path(), RETENTION_MS).await.unwrap();
        assert!(report.actions.is_empty(), "an empty .claude/worktrees dir yields no actions");
    }

    // ---- sample report (for the PR, not a stable CLI contract) ----

    #[tokio::test]
    async fn sample_run_prints_a_list_style_report() {
        let _iso = crate::test_isolation::isolate();
        let repo_a = init_repo();
        let live_wt = add_worktree_branch(repo_a.path(), "live-worker");
        let clean_orphan_wt = add_worktree_branch(repo_a.path(), "clean-orphan");
        write_marker(&clean_orphan_wt, &WorkspaceMarker {
            session_id: "clean-orphan".into(), repo: repo_a.path().to_string_lossy().into(),
            branch: "clean-orphan".into(), created_at: 0,
        }).unwrap();
        let dirty_orphan_wt = add_worktree_branch(repo_a.path(), "dirty-orphan");
        write_marker(&dirty_orphan_wt, &WorkspaceMarker {
            session_id: "dirty-orphan".into(), repo: repo_a.path().to_string_lossy().into(),
            branch: "dirty-orphan".into(), created_at: 0,
        }).unwrap();
        std::fs::write(dirty_orphan_wt.join("scratch.txt"), "wip").unwrap();
        let not_ours_wt = repo_a.path().join(".claude").join("worktrees").join("someones-checkout");
        run_git(repo_a.path(), &["worktree", "add", "-q", "-b", "a-persons-feature", not_ours_wt.to_str().unwrap()]);

        let repo_b = init_repo();
        let legacy_clean_wt = add_worktree_branch(repo_b.path(), "legacy-clean");

        let store = Store::open(tempdir().unwrap().keep().join("t.db")).unwrap();
        store.upsert_session(&test_session("live-worker", SessionStatus::Working, live_wt.to_str().unwrap(), None)).unwrap();

        println!("PATH                                                STATUS                   ACTION");
        for repo in [repo_a.path(), repo_b.path()] {
            let report = sweep_repo_orphans(&store, repo, RETENTION_MS).await.unwrap();
            for action in &report.actions {
                let status = match action.status {
                    OrphanStatus::Live => "live".to_string(),
                    OrphanStatus::NotOrphanYet => "not-orphan-yet".to_string(),
                    OrphanStatus::NotOurs => "not-ours".to_string(),
                    OrphanStatus::Orphan(Ownership::Tracked) => "orphan(tracked)".to_string(),
                    OrphanStatus::Orphan(Ownership::LegacyMatch) => "orphan(legacy)".to_string(),
                    OrphanStatus::Orphan(Ownership::NotOurs) => "orphan(not-ours)".to_string(),
                };
                let taken = action.outcome.as_ref().map(|o| o.describe()).unwrap_or_else(|| "untouched".to_string());
                println!("{:<52}{:<25}{}", action.path.display().to_string(), status, taken);
            }
        }

        // Not a format assertion — the point of this test is the captured
        // stdout above; this just proves every expected directory was
        // visited and classified as intended.
        assert!(!clean_orphan_wt.exists(), "clean tracked orphan auto-cleaned");
        assert!(dirty_orphan_wt.exists(), "dirty tracked orphan left alone");
        assert!(not_ours_wt.exists(), "not-ours directory never touched");
        assert!(live_wt.exists(), "live worker never touched");
        assert!(!legacy_clean_wt.exists(), "squeaky-clean legacy match auto-cleaned");
    }

    // ---- discover_repo_roots ----

    #[test]
    fn discovers_distinct_repo_roots_from_workspace_paths() {
        let store = Store::open(tempdir().unwrap().keep().join("t.db")).unwrap();
        store.upsert_session(&test_session("a1", SessionStatus::Working, "/repos/a/.claude/worktrees/a1", None)).unwrap();
        store.upsert_session(&test_session("a2", SessionStatus::Working, "/repos/a/.claude/worktrees/a2", None)).unwrap();
        store.upsert_session(&test_session("b1", SessionStatus::Working, "/repos/b/.claude/worktrees/b1", None)).unwrap();
        store.upsert_session(&test_session("o1", SessionStatus::Working, "/orchestrators/o1", None)).unwrap();

        let roots = discover_repo_roots(&store).unwrap();

        assert_eq!(roots, vec![std::path::PathBuf::from("/repos/a"), std::path::PathBuf::from("/repos/b")]);
    }
}
