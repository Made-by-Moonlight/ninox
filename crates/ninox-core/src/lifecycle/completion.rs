//! Support for worker self-completion (`ninox complete`): the worktree
//! safety check that gates it, success-criterion verification, and
//! live-parent resolution for routing its completion message. See
//! `docs/superpowers/plans/2026-10-29-mlops-5417-success-criteria.md`.

use std::path::Path;
use tokio::process::Command;

use crate::github::GithubApi;
use crate::store::Store;
use crate::types::{Session, SuccessCriterion};

/// Bounds the climb in [`resolve_live_parent`] against a corrupt/cyclic
/// `orchestrator_id` chain — mirrors `orchestrator_auth::orchestrator_depth`'s
/// own cycle guard.
const MAX_ANCESTOR_HOPS: u32 = 64;

/// The session's parent (its spawner), or the nearest live ancestor if the
/// direct parent is gone (no session row) or terminal — `ninox complete`'s
/// completion message routes here instead of always `session.orchestrator_id`
/// directly. `None` if there's no parent at all, or every ancestor up the
/// chain is gone/terminal.
pub fn resolve_live_parent(store: &Store, session: &Session) -> Option<String> {
    let mut current = session.orchestrator_id.clone()?;
    for _ in 0..MAX_ANCESTOR_HOPS {
        let row = store.get_session(&current).ok()??;
        if !row.status.is_terminal() {
            return Some(current);
        }
        current = row.orchestrator_id?;
    }
    None
}

/// What the session's own reported PR URLs (the URL `gh pr create` printed)
/// say about its tracked `(repo, number)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Provenance {
    /// The tracked PR is one the session reported creating.
    Reported,
    /// The session reported PR URLs and the tracked PR isn't among them.
    Excluded,
    /// No metadata or no reported URLs: no evidence either way.
    Unknown,
}

pub(crate) fn pr_provenance(session: &Session) -> Provenance {
    let Some(number) = session.pr_number else { return Provenance::Unknown };
    let Ok(meta) = crate::hooks::read_session_metadata(&crate::config::AppConfig::sessions_dir(), &session.id) else {
        return Provenance::Unknown;
    };
    let reported: Vec<(String, u64)> = meta.pr_reports.iter()
        .filter_map(|r| r.url.as_deref().and_then(crate::github::parse_pr_url))
        .collect();
    if reported.is_empty() {
        Provenance::Unknown
    } else if reported.iter().any(|(repo, n)| *n == number && repo.eq_ignore_ascii_case(&session.repo)) {
        Provenance::Reported
    } else {
        Provenance::Excluded
    }
}

/// True only when `head_ref` is provably not this session's PR branch: a
/// session branch is known and none equals `head_ref`. A recorded branch
/// equal to the session id is the spawn default, which workers routinely
/// leave by pushing a ticket-named branch the wrapper never recorded, so it
/// is not evidence; a live checkout's branch is.
pub(crate) fn foreign_head(session: &Session, recorded_branch: Option<String>, head_ref: &str) -> bool {
    if head_ref.is_empty() || head_ref == session.id {
        return false;
    }
    let live = session.workspace_path.as_deref().and_then(crate::github::current_branch);
    let known: Vec<String> = recorded_branch
        .into_iter()
        .filter(|b| !b.is_empty() && *b != session.id)
        .chain(live)
        .collect();
    !known.is_empty() && known.iter().all(|b| b != head_ref)
}

/// Verifies `criteria` against `session`'s current state / GitHub, one
/// result per criterion in input order. `PrOpened` and `ReportSent`/`Check`
/// never touch the network — only `PrMerged` does, and only when a PR is
/// actually tracked.
pub async fn verify_criteria(
    session:  &Session,
    criteria: &[SuccessCriterion],
    github:   &dyn GithubApi,
) -> Vec<(SuccessCriterion, Result<(), String>)> {
    let mut results = Vec::with_capacity(criteria.len());
    for c in criteria {
        let outcome = match c {
            SuccessCriterion::PrOpened => {
                if session.pr_number.is_some() {
                    Ok(())
                } else {
                    Err("no pull request has been opened for this worker".to_string())
                }
            }
            SuccessCriterion::PrMerged => match session.pr_number {
                None => Err("no pull request is tracked for this worker".to_string()),
                Some(pr_number) => {
                    let (owner, repo) = match session.repo.split_once('/') {
                        Some(parts) => parts,
                        None => ("", session.repo.as_str()),
                    };
                    match github.get_pr_status(owner, repo, pr_number).await {
                        Ok(status) if status.merged && match pr_provenance(session) {
                            Provenance::Reported => false,
                            Provenance::Excluded => {
                                let live = session.workspace_path.as_deref().and_then(crate::github::current_branch);
                                !status.head_ref.is_empty() && status.head_ref != session.id && live.as_deref() != Some(status.head_ref.as_str())
                            }
                            Provenance::Unknown => foreign_head(session, None, &status.head_ref),
                        } => Err(format!(
                            "pull request #{pr_number} merged, but its head branch `{}` is not this worker's",
                            status.head_ref,
                        )),
                        Ok(status) if status.merged => Ok(()),
                        Ok(_) => Err(format!("pull request #{pr_number} is not merged")),
                        Err(e) => Err(format!("could not check pull request #{pr_number}: {e}")),
                    }
                }
            },
            SuccessCriterion::ReportSent => Ok(()),
            SuccessCriterion::Check(_) => Ok(()),
        };
        results.push((c.clone(), outcome));
    }
    results
}

/// Runs `git -C <workspace> <args>`, returning stdout trimmed on success.
/// `Err` means git itself could not even be invoked against this directory
/// (e.g. it doesn't exist) — the caller must surface this, never treat it
/// as "command ran and reported nothing".
async fn git(workspace: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(args)
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!(
            "git {} failed in {}: {}",
            args.join(" "),
            workspace.display(),
            String::from_utf8_lossy(&output.stderr).trim(),
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// `Ok(None)`: safe to reap. `Ok(Some(reason))`: unsafe, naming why (dirty /
/// N unpushed commits) — `ninox complete` refuses on this unless `--force`.
/// `Err`: git itself couldn't be run against `workspace` (missing/
/// inaccessible worktree) — the caller must surface this as a real error,
/// never fall back to treating it as safe.
pub async fn worktree_unsafe_reason(workspace: &Path) -> anyhow::Result<Option<String>> {
    let status = git(workspace, &["status", "--porcelain"]).await?;
    if !status.is_empty() {
        return Ok(Some("the worktree has uncommitted changes".to_string()));
    }

    // An upstream failing to resolve is not a git failure worth propagating
    // — it just means this branch has none yet (`git rev-parse` exits
    // non-zero), the common no-upstream case this function must still
    // handle below.
    let upstream = git(workspace, &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"]).await.ok();

    let ahead_range = match upstream {
        Some(u) => format!("{u}..HEAD"),
        None => match git(workspace, &["symbolic-ref", "refs/remotes/origin/HEAD"]).await {
            // `refs/remotes/origin/HEAD` is a full ref (e.g.
            // `refs/remotes/origin/main`) — `merge-base`/`rev-list` accept
            // it directly, no need to strip the prefix.
            Ok(default_ref) if !default_ref.is_empty() => {
                let merge_base = git(workspace, &["merge-base", "HEAD", &default_ref]).await?;
                format!("{merge_base}..HEAD")
            }
            // No upstream and no resolvable remote default branch: nothing
            // provably unpushed to flag (documented limitation — matches
            // the spec's own "no upstream, no commits beyond base is
            // clean" example when there's genuinely no base to compare
            // against).
            _ => return Ok(None),
        },
    };

    let ahead = git(workspace, &["rev-list", "--count", &ahead_range]).await?;
    let ahead: u64 = ahead.parse().unwrap_or(0);
    if ahead > 0 {
        return Ok(Some(format!("the worktree has {ahead} unpushed commit(s) ({ahead_range})")));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use tempfile::tempdir;

    fn run(dir: &Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git")
            .args(["-C", dir.to_str().unwrap()])
            .args(args)
            .output()
            .unwrap()
    }

    /// A minimal real git repo with one commit, used as the "remote" a
    /// worktree clones from — `git clone` of a local path sets up `origin`
    /// and `refs/remotes/origin/HEAD` for free, exactly like a real GitHub
    /// remote would.
    fn init_remote_repo() -> std::path::PathBuf {
        let dir = tempdir().unwrap().keep();
        run(&dir, &["init", "-q"]);
        run(&dir, &["config", "user.email", "test@example.com"]);
        run(&dir, &["config", "user.name", "Test"]);
        std::fs::write(dir.join("README.md"), "x").unwrap();
        run(&dir, &["add", "."]);
        run(&dir, &["commit", "-q", "-m", "init"]);
        dir
    }

    /// Clones `remote` into a fresh worktree — HEAD tracks `origin`'s
    /// default branch, clean, with an upstream configured, exactly like a
    /// just-spawned worker's worktree.
    fn clone_worktree(remote: &Path) -> std::path::PathBuf {
        let dir = tempdir().unwrap().keep();
        let out = std::process::Command::new("git")
            .args(["clone", "-q", remote.to_str().unwrap(), dir.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(out.status.success(), "clone failed: {}", String::from_utf8_lossy(&out.stderr));
        run(&dir, &["config", "user.email", "test@example.com"]);
        run(&dir, &["config", "user.name", "Test"]);
        dir
    }

    #[tokio::test]
    async fn clean_pushed_branch_is_safe() {
        let remote = init_remote_repo();
        let worktree = clone_worktree(&remote);
        assert_eq!(super::worktree_unsafe_reason(&worktree).await.unwrap(), None);
    }

    #[tokio::test]
    async fn dirty_tree_is_unsafe() {
        let remote = init_remote_repo();
        let worktree = clone_worktree(&remote);
        std::fs::write(worktree.join("README.md"), "changed").unwrap();
        let reason = super::worktree_unsafe_reason(&worktree).await.unwrap();
        let reason = reason.expect("a dirty tree must be unsafe");
        assert!(reason.to_lowercase().contains("uncommitted") || reason.to_lowercase().contains("dirty"));
    }

    #[tokio::test]
    async fn unpushed_commits_with_upstream_is_unsafe() {
        let remote = init_remote_repo();
        let worktree = clone_worktree(&remote);
        std::fs::write(worktree.join("new.txt"), "x").unwrap();
        run(&worktree, &["add", "."]);
        run(&worktree, &["commit", "-q", "-m", "local work"]);
        let reason = super::worktree_unsafe_reason(&worktree).await.unwrap();
        let reason = reason.expect("an unpushed commit must be unsafe");
        assert!(reason.to_lowercase().contains("unpushed"));
    }

    #[tokio::test]
    async fn no_upstream_no_commits_beyond_base_is_safe() {
        let remote = init_remote_repo();
        let worktree = clone_worktree(&remote);
        run(&worktree, &["checkout", "-q", "-b", "feature-no-commits"]);
        assert_eq!(super::worktree_unsafe_reason(&worktree).await.unwrap(), None);
    }

    #[tokio::test]
    async fn no_upstream_with_commits_beyond_base_is_unsafe() {
        let remote = init_remote_repo();
        let worktree = clone_worktree(&remote);
        run(&worktree, &["checkout", "-q", "-b", "feature-with-commits"]);
        std::fs::write(worktree.join("new.txt"), "x").unwrap();
        run(&worktree, &["add", "."]);
        run(&worktree, &["commit", "-q", "-m", "local work"]);
        let reason = super::worktree_unsafe_reason(&worktree).await.unwrap();
        let reason = reason.expect("a commit beyond the fork point with no upstream must be unsafe");
        assert!(reason.to_lowercase().contains("unpushed"));
    }

    #[tokio::test]
    async fn missing_worktree_directory_errors() {
        let missing = Path::new("/definitely/not/a/real/worktree/dir");
        assert!(super::worktree_unsafe_reason(missing).await.is_err());
    }

    // ── verify_criteria ──────────────────────────────────────────────────

    use crate::types::{Session, SessionStatus, SuccessCriterion};

    fn session(repo: &str, pr_number: Option<u64>) -> Session {
        Session {
            id: "s1".into(), orchestrator_id: None, name: "w".into(),
            repo: repo.into(), status: SessionStatus::Working,
            agent_type: "claude-code".into(), cost_usd: 0.0, started_at: 0,
            pr_number, pr_id: None, workspace_path: None, pid: None,
            model: None, observed_model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None, worker_type: None,
            success_criteria: Vec::new(), completion_outcome: None, self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        }
    }

    #[derive(Default)]
    struct FakeGithub {
        merged: std::sync::Mutex<Option<bool>>,
        calls:  std::sync::Mutex<u32>,
    }

    #[async_trait::async_trait]
    impl crate::github::GithubApi for FakeGithub {
        async fn get_pr_status(&self, _owner: &str, _repo: &str, _pr_number: u64) -> anyhow::Result<crate::github::PrStatus> {
            *self.calls.lock().unwrap() += 1;
            match *self.merged.lock().unwrap() {
                Some(merged) => Ok(crate::github::PrStatus {
                    merged, state: "open".into(), mergeable: None, title: "t".into(),
                    number: 1, head_sha: String::new(), head_ref: String::new(), base_ref: String::new(),
                }),
                None => anyhow::bail!("404"),
            }
        }
        async fn get_ci_checks(&self, _owner: &str, _repo: &str, _head_sha: &str) -> anyhow::Result<Vec<crate::github::CheckRun>> {
            Ok(vec![])
        }
        async fn get_review_threads(&self, _owner: &str, _repo: &str, _pr_number: u64) -> anyhow::Result<Vec<crate::github::ReviewThread>> {
            Ok(vec![])
        }
        async fn get_issue_comments(&self, _owner: &str, _repo: &str, _pr_number: u64) -> anyhow::Result<Vec<crate::types::Comment>> {
            Ok(vec![])
        }
        async fn find_open_pr_for_branch(&self, _owner: &str, _repo: &str, _branch: &str) -> anyhow::Result<Option<crate::github::PrRef>> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn pr_merged_passes_when_github_reports_merged() {
        let _iso = crate::test_isolation::isolate();
        let s = session("o/r", Some(7));
        let gh = FakeGithub { merged: std::sync::Mutex::new(Some(true)), ..Default::default() };
        let results = super::verify_criteria(&s, &[SuccessCriterion::PrMerged], &gh).await;
        assert_eq!(results, vec![(SuccessCriterion::PrMerged, Ok(()))]);
    }

    #[tokio::test]
    async fn pr_merged_fails_when_github_reports_not_merged() {
        let s = session("o/r", Some(7));
        let gh = FakeGithub { merged: std::sync::Mutex::new(Some(false)), ..Default::default() };
        let results = super::verify_criteria(&s, &[SuccessCriterion::PrMerged], &gh).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, SuccessCriterion::PrMerged);
        assert!(results[0].1.is_err());
    }

    #[tokio::test]
    async fn pr_merged_fails_with_no_tracked_pr() {
        let s = session("o/r", None);
        let gh = FakeGithub::default();
        let results = super::verify_criteria(&s, &[SuccessCriterion::PrMerged], &gh).await;
        assert_eq!(results.len(), 1);
        assert!(results[0].1.is_err());
        assert_eq!(*gh.calls.lock().unwrap(), 0, "no PR tracked — must not even query GitHub");
    }

    #[tokio::test]
    async fn pr_opened_passes_with_a_tracked_pr_number_and_no_network_call() {
        let s = session("o/r", Some(7));
        let gh = FakeGithub::default();
        let results = super::verify_criteria(&s, &[SuccessCriterion::PrOpened], &gh).await;
        assert_eq!(results, vec![(SuccessCriterion::PrOpened, Ok(()))]);
        assert_eq!(*gh.calls.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn report_sent_and_check_always_pass() {
        let s = session("o/r", None);
        let gh = FakeGithub::default();
        let results = super::verify_criteria(
            &s, &[SuccessCriterion::ReportSent, SuccessCriterion::Check("x".into())], &gh,
        ).await;
        assert_eq!(results, vec![
            (SuccessCriterion::ReportSent, Ok(())),
            (SuccessCriterion::Check("x".into()), Ok(())),
        ]);
    }

    #[tokio::test]
    async fn verify_criteria_preserves_input_order_and_reports_every_item() {
        let _iso = crate::test_isolation::isolate();
        let s = session("o/r", Some(7));
        let gh = FakeGithub { merged: std::sync::Mutex::new(Some(true)), ..Default::default() };
        let criteria = vec![
            SuccessCriterion::Check("a".into()),
            SuccessCriterion::PrMerged,
            SuccessCriterion::ReportSent,
        ];
        let results = super::verify_criteria(&s, &criteria, &gh).await;
        let kinds: Vec<_> = results.iter().map(|(c, _)| c.clone()).collect();
        assert_eq!(kinds, criteria);
        assert!(results.iter().all(|(_, r)| r.is_ok()));
    }

    // ── resolve_live_parent ──────────────────────────────────────────────

    use crate::store::Store;

    fn test_store() -> Store {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.db");
        std::mem::forget(dir);
        Store::open(path).unwrap()
    }

    fn session_with(id: &str, orchestrator_id: Option<&str>, status: SessionStatus) -> Session {
        let mut s = session("o/r", None);
        s.id = id.into();
        s.orchestrator_id = orchestrator_id.map(String::from);
        s.status = status;
        s
    }

    #[test]
    fn resolve_live_parent_returns_the_direct_parent_when_live() {
        let store = test_store();
        store.upsert_session(&session_with("orch", None, SessionStatus::Working)).unwrap();
        let worker = session_with("worker", Some("orch"), SessionStatus::Working);
        assert_eq!(super::resolve_live_parent(&store, &worker), Some("orch".to_string()));
    }

    #[test]
    fn resolve_live_parent_climbs_past_a_terminated_parent_to_a_live_grandparent() {
        let store = test_store();
        store.upsert_session(&session_with("grandparent", None, SessionStatus::Working)).unwrap();
        store.upsert_session(&session_with("parent", Some("grandparent"), SessionStatus::Terminated)).unwrap();
        let worker = session_with("worker", Some("parent"), SessionStatus::Working);
        assert_eq!(super::resolve_live_parent(&store, &worker), Some("grandparent".to_string()));
    }

    #[test]
    fn resolve_live_parent_returns_none_when_the_whole_chain_is_gone() {
        let store = test_store();
        let worker = session_with("worker", Some("missing-orch"), SessionStatus::Working);
        assert_eq!(super::resolve_live_parent(&store, &worker), None);
    }

    #[test]
    fn resolve_live_parent_returns_none_for_a_root_with_no_orchestrator_id() {
        let store = test_store();
        let root = session_with("root", None, SessionStatus::Working);
        assert_eq!(super::resolve_live_parent(&store, &root), None);
    }
}
