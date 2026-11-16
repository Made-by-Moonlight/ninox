use std::path::Path;
use std::process::Command;

use super::WorkspaceFacts;
use crate::fleet::probe::has_user_changes;

/// Shells out to `git` to fill in the git-derived fields of
/// [`WorkspaceFacts`]. `base_ref` is the ref to diff against for
/// `commits_beyond_base` — callers resolve it once per repo (via
/// [`resolve_default_base_ref`]) and pass it in. The session/store-derived
/// fields (`merged`, `commits_since_merge`, `interrupted`, `live_session_row`)
/// are left at their conservative defaults; callers set those from what they
/// already know.
///
/// Every safety-relevant field is computed against the worktree's *actual*
/// checked-out branch ([`effective_branch`]), not the `branch` parameter —
/// `branch` is only a fallback used when there's no live worktree left to
/// inspect. A long-lived session can switch branches (a try-out worker
/// reusing one worktree per task-item) or end up detached after `branch` was
/// last recorded, and these facts must describe what's really on disk, the
/// thing that would actually be destroyed, not a label that may no longer
/// match it.
///
/// Every git call is best-effort: a command that fails or can't be parsed
/// falls back to the *unsafe* default for that field (dirty, or a nonzero
/// unpushed/ahead count) rather than silently reporting "safe" — with two
/// exceptions, both representing "there is nothing here to lose": a missing
/// `worktree_path` can't be dirty, and `base_ref: None` is only reached when
/// [`resolve_default_base_ref`] found no base at all, in which case
/// `commits_beyond_base` defaults to an unsafe nonzero sentinel too (an
/// earlier version of this function defaulted it to `0`, which silently
/// judged unpushed local commits "safe" whenever a repo had no `origin/HEAD`
/// — e.g. `git init` + `git remote add` with no fetch yet).
pub fn gather_git_facts(
    repo_root: &Path, worktree_path: &Path, branch: Option<&str>, base_ref: Option<&str>,
) -> WorkspaceFacts {
    // Gitignored files stay out of this check (build output would make every
    // worktree dirty). A stash made here is uncommitted work too, and stashes
    // live in the shared repo, invisible to `status`.
    let dirty = worktree_path.is_dir() && (has_uncommitted_changes(worktree_path) || has_stash_from(worktree_path));

    // Detached only when a live worktree's HEAD genuinely isn't a named
    // branch — a missing worktree (nothing to inspect) falls through to the
    // ordinary named-branch path below, using `branch` as its only source.
    if worktree_path.is_dir() && crate::fleet::probe::current_branch(worktree_path).is_none() {
        // No named branch reaches these commits, so the usual @{u}/base
        // comparisons can't see them at all. Anything on HEAD that no
        // remote ref already has is exactly as unsafe to lose as an
        // unpushed branch commit, so it's folded into the same
        // `commits_beyond_base` signal `decide()` already checks.
        let unreachable = count_unreachable_from_remotes(worktree_path, "HEAD").unwrap_or(1);
        return WorkspaceFacts {
            dirty,
            has_branch: false,
            has_upstream: false,
            unpushed_commit_count: 0,
            commits_beyond_base: unreachable,
            merged: false,
            commits_since_merge: 0,
            interrupted: false,
            live_session_row: false,
        };
    }

    let resolved = effective_branch(worktree_path, branch);
    let has_branch = resolved.as_deref().is_some_and(|b| {
        git_ok(repo_root, &["show-ref", "--verify", "--quiet", &format!("refs/heads/{b}")])
    });

    let has_upstream = has_branch
        && resolved.as_deref().is_some_and(|b| {
            git_ok(repo_root, &["rev-parse", "--abbrev-ref", "--symbolic-full-name", &format!("{b}@{{u}}")])
        });

    let unpushed_commit_count = if has_upstream {
        let b = resolved.as_deref().expect("has_upstream implies has_branch implies resolved.is_some()");
        count_commits(repo_root, &format!("{b}@{{u}}..{b}")).unwrap_or(1)
    } else {
        0
    };

    let commits_beyond_base = match (has_branch, base_ref) {
        (true, Some(base)) => {
            let b = resolved.as_deref().expect("has_branch implies resolved.is_some()");
            count_commits(repo_root, &format!("{base}..{b}")).unwrap_or(1)
        }
        // No base could be resolved at all: unsafe, not 0 — see this
        // function's doc comment.
        (true, None) => 1,
        (false, _) => 0,
    };
    WorkspaceFacts {
        dirty,
        has_branch,
        has_upstream,
        unpushed_commit_count,
        commits_beyond_base,
        merged: false,
        commits_since_merge: 0,
        interrupted: false,
        live_session_row: false,
    }
}

/// The worktree's actual checked-out branch, falling back to `branch` only
/// when there's no live worktree left to inspect (e.g. a retry after an
/// earlier run already removed it). Exposed so a caller can run further
/// branch-specific queries (e.g. [`commits_since`]) against the exact branch
/// `gather_git_facts` based its facts on, and so it can pass [`cleanup`](
/// super::cleanup) the same branch those facts actually describe instead of
/// a possibly-stale recorded one.
pub fn effective_branch(worktree_path: &Path, branch: Option<&str>) -> Option<String> {
    if worktree_path.is_dir() {
        crate::fleet::probe::current_branch(worktree_path)
    } else {
        branch.map(str::to_string)
    }
}

/// Commits on `branch` newer than `since_ms` (unix epoch milliseconds) —
/// used to confirm a `merged` branch has picked up no follow-up work the
/// merge never covered. Unresolvable defaults to an unsafe nonzero sentinel,
/// consistent with this module's other "can't verify → unsafe" defaults.
///
/// Prefer [`commits_since_sha`] when the PR's merged head sha is known: a
/// timestamp comparison can only ever see commits made after the *poller's
/// own tick* detected the merge, not the real merge moment — a commit made
/// in that gap predates `since_ms` and would be missed.
pub fn commits_since(repo_root: &Path, branch: &str, since_ms: i64) -> u32 {
    git_output(repo_root, &["rev-list", "--count", branch, &format!("--since=@{}", since_ms / 1000)])
        .and_then(|out| out.trim().parse().ok())
        .unwrap_or(1)
}

/// Commits on `branch` not reachable from `sha` — GitHub's own record of the
/// PR's exact merged head, immune to how late the poller got to detecting
/// it. `None` when `sha` isn't a valid, present object in this repo (e.g. a
/// squash merge whose pre-squash head was never fetched locally) — callers
/// fall back to [`commits_since`] in that case.
pub fn commits_since_sha(repo_root: &Path, branch: &str, sha: &str) -> Option<u32> {
    if !git_ok(repo_root, &["cat-file", "-e", &format!("{sha}^{{commit}}")]) {
        return None;
    }
    count_commits(repo_root, &format!("{sha}..{branch}"))
}

/// `git -C repo_root rev-parse --abbrev-ref origin/HEAD`, trimmed; falls
/// back to a local `main`/`master`/`v1` branch if `origin/HEAD` was never
/// set (common for a repo with a remote added but never fetched, or a
/// local-only sandbox) — any of these still gives `gather_git_facts` a real
/// ref to diff against, rather than leaving `commits_beyond_base` to its own
/// unsafe "nothing resolvable" default. `None` only when none of these
/// exist.
pub fn resolve_default_base_ref(repo_root: &Path) -> Option<String> {
    if let Some(out) = git_output(repo_root, &["rev-parse", "--abbrev-ref", "origin/HEAD"]) {
        let trimmed = out.trim();
        if !trimmed.is_empty() && trimmed != "origin/HEAD" {
            return Some(trimmed.to_string());
        }
    }
    ["main", "master", "v1"]
        .into_iter()
        .find(|candidate| git_ok(repo_root, &["show-ref", "--verify", "--quiet", &format!("refs/heads/{candidate}")]))
        .map(str::to_string)
}

fn git_output(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).to_string())
}

fn git_ok(dir: &Path, args: &[&str]) -> bool {
    Command::new("git").arg("-C").arg(dir).args(args).output().map(|o| o.status.success()).unwrap_or(false)
}

fn count_commits(dir: &Path, range: &str) -> Option<u32> {
    git_output(dir, &["rev-list", "--count", range])?.trim().parse().ok()
}

/// Uncommitted changes, gitignored files aside. Unreadable counts as yes.
pub(crate) fn has_uncommitted_changes(worktree_path: &Path) -> bool {
    git_output(worktree_path, &["status", "--porcelain", "--untracked-files=all"])
        .map(|out| has_user_changes(&out))
        .unwrap_or(true)
}

/// Whether any stash entry was made on the worktree's branch, or on its
/// detached HEAD. Unreadable counts as yes.
pub(crate) fn has_stash_from(worktree_path: &Path) -> bool {
    let Some(list) = git_output(worktree_path, &["stash", "list", "--format=%gs%x00%P"]) else { return true };
    if list.trim().is_empty() {
        return false;
    }
    let branch = crate::fleet::probe::current_branch(worktree_path);
    let head = git_output(worktree_path, &["rev-parse", "HEAD"]).map(|h| h.trim().to_string());
    list.lines().any(|line| {
        let (subject, parents) = line.split_once('\0').unwrap_or((line, ""));
        match &branch {
            Some(b) => subject.starts_with(&format!("WIP on {b}:")) || subject.starts_with(&format!("On {b}:")),
            None => head.as_deref().is_some_and(|h| parents.split_whitespace().next() == Some(h)),
        }
    })
}

fn count_unreachable_from_remotes(dir: &Path, rev: &str) -> Option<u32> {
    git_output(dir, &["rev-list", "--count", rev, "--not", "--remotes"])?.trim().parse().ok()
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

    fn init_repo() -> TempDir {
        let repo = tempdir().unwrap();
        run_git(repo.path(), &["init", "-q", "-b", "main"]);
        run_git(repo.path(), &["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "--allow-empty", "-q", "-m", "init"]);
        repo
    }

    fn add_worktree_branch(repo: &Path, branch: &str) -> TempDir {
        let wt = tempdir().unwrap();
        run_git(repo, &["worktree", "add", "-q", "-b", branch, wt.path().to_str().unwrap()]);
        wt
    }

    #[test]
    fn clean_pushed_branch_has_no_unsafe_facts() {
        let repo = init_repo();
        let remote = tempdir().unwrap();
        run_git(remote.path(), &["init", "-q", "--bare"]);
        run_git(repo.path(), &["remote", "add", "origin", remote.path().to_str().unwrap()]);
        run_git(repo.path(), &["push", "-q", "origin", "main"]);
        let wt = add_worktree_branch(repo.path(), "feature");
        run_git(repo.path(), &["push", "-q", "-u", "origin", "feature"]);

        let facts = gather_git_facts(repo.path(), wt.path(), Some("feature"), None);
        assert!(!facts.dirty);
        assert!(facts.has_branch);
        assert!(facts.has_upstream);
        assert_eq!(facts.unpushed_commit_count, 0);
    }

    #[test]
    fn dirty_worktree_is_detected() {
        let repo = init_repo();
        let wt = add_worktree_branch(repo.path(), "feature");
        std::fs::write(wt.path().join("untracked.txt"), "x").unwrap();

        let facts = gather_git_facts(repo.path(), wt.path(), Some("feature"), None);
        assert!(facts.dirty);
    }

    #[test]
    fn gitignored_files_do_not_count_as_dirty() {
        let repo = init_repo();
        std::fs::write(repo.path().join(".gitignore"), "target/\n").unwrap();
        run_git(repo.path(), &["add", ".gitignore"]);
        run_git(repo.path(), &["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "-q", "-m", "ignore"]);
        let wt = add_worktree_branch(repo.path(), "feature");
        std::fs::create_dir_all(wt.path().join("target")).unwrap();
        std::fs::write(wt.path().join("target/out"), "x").unwrap();

        assert!(!gather_git_facts(repo.path(), wt.path(), Some("feature"), None).dirty);
    }

    /// `git status` can't see a stash, and stashes live in the shared repo:
    /// work stashed in this worktree must still block a non-forced removal.
    #[test]
    fn a_stash_made_on_the_worktree_branch_counts_as_dirty() {
        let repo = init_repo();
        std::fs::write(repo.path().join("f.txt"), "a").unwrap();
        run_git(repo.path(), &["add", "f.txt"]);
        run_git(repo.path(), &["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "-q", "-m", "f"]);
        let other = add_worktree_branch(repo.path(), "other");
        let wt = add_worktree_branch(repo.path(), "feature");

        std::fs::write(wt.path().join("f.txt"), "wip").unwrap();
        run_git(wt.path(), &["-c", "user.email=t@t.com", "-c", "user.name=t", "stash", "-q"]);
        let facts = gather_git_facts(repo.path(), wt.path(), Some("feature"), None);
        assert!(facts.dirty, "a stash-only worktree must count as dirty");
        assert!(
            !gather_git_facts(repo.path(), other.path(), Some("other"), None).dirty,
            "another branch's stash is not this worktree's work",
        );
    }

    #[test]
    fn ninox_managed_files_do_not_count_as_dirty() {
        let repo = init_repo();
        let wt = add_worktree_branch(repo.path(), "feature");
        std::fs::create_dir_all(wt.path().join(".claude")).unwrap();
        std::fs::write(wt.path().join(".claude/settings.json"), "{}").unwrap();

        let facts = gather_git_facts(repo.path(), wt.path(), Some("feature"), None);
        assert!(!facts.dirty);
    }

    #[test]
    fn unpushed_commit_is_detected() {
        let repo = init_repo();
        let remote = tempdir().unwrap();
        run_git(remote.path(), &["init", "-q", "--bare"]);
        run_git(repo.path(), &["remote", "add", "origin", remote.path().to_str().unwrap()]);
        let wt = add_worktree_branch(repo.path(), "feature");
        run_git(repo.path(), &["push", "-q", "-u", "origin", "feature"]);
        run_git(wt.path(), &["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "--allow-empty", "-q", "-m", "extra"]);

        let facts = gather_git_facts(repo.path(), wt.path(), Some("feature"), None);
        assert!(facts.has_upstream);
        assert_eq!(facts.unpushed_commit_count, 1);
    }

    #[test]
    fn no_upstream_branch_reports_commits_beyond_base() {
        let repo = init_repo();
        let wt = add_worktree_branch(repo.path(), "feature");
        run_git(wt.path(), &["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "--allow-empty", "-q", "-m", "c1"]);
        run_git(wt.path(), &["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "--allow-empty", "-q", "-m", "c2"]);

        let facts = gather_git_facts(repo.path(), wt.path(), Some("feature"), Some("main"));
        assert!(!facts.has_upstream);
        assert_eq!(facts.commits_beyond_base, 2);
    }

    #[test]
    fn resolve_default_base_ref_reads_origin_head() {
        let repo = init_repo();
        let remote = tempdir().unwrap();
        run_git(remote.path(), &["init", "-q", "--bare", "-b", "main"]);
        run_git(repo.path(), &["remote", "add", "origin", remote.path().to_str().unwrap()]);
        run_git(repo.path(), &["push", "-q", "origin", "main"]);
        run_git(repo.path(), &["remote", "set-head", "origin", "-a"]);

        let base = resolve_default_base_ref(repo.path());
        assert_eq!(base.as_deref(), Some("origin/main"));
    }

    #[test]
    fn resolve_default_base_ref_falls_back_to_local_main_without_origin_head() {
        // init_repo() creates a local "main" branch and nothing else — no
        // origin at all. A repo in this shape (e.g. `git init` + `git
        // remote add` with no fetch yet, or a local-only sandbox) must still
        // resolve a usable base, since the fallback-to-0 this replaces was
        // letting unpushed commits through as "safe" (see policy_decides_* /
        // the fix for the reviewer's Critical 1).
        let repo = init_repo();
        assert_eq!(resolve_default_base_ref(repo.path()), Some("main".to_string()));
    }

    #[test]
    fn resolve_default_base_ref_none_when_nothing_resolvable() {
        let repo = tempdir().unwrap();
        run_git(repo.path(), &["init", "-q", "-b", "feature-only"]);
        run_git(repo.path(), &["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "--allow-empty", "-q", "-m", "init"]);
        assert_eq!(resolve_default_base_ref(repo.path()), None);
    }

    #[test]
    fn gather_git_facts_commits_beyond_base_is_unsafe_sentinel_without_a_resolvable_base() {
        // No origin, no local main/master/v1 — nothing to compare against.
        // Must default to "unsafe" (nonzero), never 0, or an unpushed,
        // never-backed-up commit on this branch would be judged safe to
        // delete (reviewer's Critical 1).
        let repo = tempdir().unwrap();
        run_git(repo.path(), &["init", "-q", "-b", "feature-only"]);
        run_git(repo.path(), &["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "--allow-empty", "-q", "-m", "init"]);
        let wt = add_worktree_branch(repo.path(), "orphan-branch");
        run_git(wt.path(), &["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "--allow-empty", "-q", "-m", "precious work"]);

        assert_eq!(resolve_default_base_ref(repo.path()), None, "sanity: no base resolvable here");
        let facts = gather_git_facts(repo.path(), wt.path(), Some("orphan-branch"), None);
        assert!(facts.commits_beyond_base > 0, "no resolvable base must default to unsafe, not 0");
    }

    #[test]
    fn gather_git_facts_uses_the_worktree_actual_branch_over_a_stale_recorded_one() {
        // A long-lived session can switch branches after `branch` was last
        // recorded (a try-out worker reusing one worktree per task item) —
        // facts must reflect what's really checked out, not the old label.
        let repo = init_repo();
        let wt = add_worktree_branch(repo.path(), "task-a");
        run_git(wt.path(), &["checkout", "-q", "-b", "task-b"]);
        run_git(wt.path(), &["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "--allow-empty", "-q", "-m", "task b work"]);

        let facts = gather_git_facts(repo.path(), wt.path(), Some("task-a"), Some("main"));
        assert!(facts.has_branch, "the actually-checked-out branch (task-b) exists");
        assert_eq!(facts.commits_beyond_base, 1, "must count task-b's commit, not task-a's (0)");
    }

    #[test]
    fn gather_git_facts_detached_head_is_unsafe_when_unreachable_from_any_remote() {
        let repo = init_repo();
        let wt = add_worktree_branch(repo.path(), "feature");
        run_git(wt.path(), &["checkout", "-q", "--detach"]);
        run_git(wt.path(), &["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "--allow-empty", "-q", "-m", "orphaned work"]);

        let facts = gather_git_facts(repo.path(), wt.path(), Some("feature"), Some("main"));
        assert!(!facts.has_branch, "detached HEAD has no named branch");
        assert!(facts.commits_beyond_base > 0, "a detached commit unreachable from any remote must read as unsafe");
    }

    #[test]
    fn commits_since_counts_only_commits_after_the_given_timestamp() {
        // Explicit commit dates, not wall-clock timing: `--since` has only
        // second resolution, so a wall-clock-derived cutoff can land in the
        // same second as a commit made moments earlier and flake.
        let repo = init_repo(); // "init" commit at whatever `git commit` dates it
        let wt = add_worktree_branch(repo.path(), "feature");
        let commit_at = 2_000_000_000i64; // 2033-05-18, far past any `init_repo()` commit
        let status = Command::new("git")
            .args(["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "--allow-empty", "-q", "-m", "after"])
            .env("GIT_COMMITTER_DATE", commit_at.to_string())
            .env("GIT_AUTHOR_DATE", commit_at.to_string())
            .current_dir(wt.path())
            .status()
            .unwrap();
        assert!(status.success());

        assert_eq!(commits_since(repo.path(), "feature", (commit_at - 60) * 1000), 1, "the commit made after `since` must be counted");
        assert_eq!(commits_since(repo.path(), "feature", (commit_at + 60) * 1000), 0, "nothing was committed after this later cutoff");
    }

    #[test]
    fn commits_since_sha_counts_commits_not_reachable_from_the_given_sha_even_with_a_misleading_timestamp() {
        // Simulates the race `commits_since` (timestamp-based) can't see:
        // a commit lands in the gap between the real merge and the
        // poller's detection tick, backdated (as a real clock race would
        // produce) to before the `merged_at` stamp a timestamp check would
        // compare against. The sha-based check doesn't care about any
        // timestamp at all, so it still catches it.
        let repo = init_repo();
        let wt = add_worktree_branch(repo.path(), "feature");
        let merged_head = git_output(wt.path(), &["rev-parse", "HEAD"]).unwrap().trim().to_string();

        let status = Command::new("git")
            .args(["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "--allow-empty", "-q", "-m", "race-window commit"])
            .env("GIT_COMMITTER_DATE", "1000000000")
            .env("GIT_AUTHOR_DATE", "1000000000")
            .current_dir(wt.path())
            .status()
            .unwrap();
        assert!(status.success());

        assert_eq!(commits_since_sha(repo.path(), "feature", &merged_head), Some(1));
    }

    #[test]
    fn commits_since_sha_is_none_for_an_unresolvable_sha() {
        let repo = init_repo();
        let wt = add_worktree_branch(repo.path(), "feature");
        assert_eq!(commits_since_sha(repo.path(), "feature", "0000000000000000000000000000000000000000"), None);
        let _ = wt;
    }

    #[test]
    fn gather_git_facts_dirty_is_false_when_worktree_directory_is_missing() {
        // The idempotent-cleanup case: a worktree directory already removed
        // by a previous run. There's nothing left to have uncommitted
        // changes, so this must not report dirty (which would permanently
        // block a retry from ever finishing the branch deletion).
        let repo = init_repo();
        let missing = repo.path().join(".claude/worktrees/already-gone");
        let facts = gather_git_facts(repo.path(), &missing, Some("already-gone"), None);
        assert!(!facts.dirty, "a missing worktree directory has nothing to be dirty");
    }
}
