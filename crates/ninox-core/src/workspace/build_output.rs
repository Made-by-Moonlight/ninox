use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use super::ninox_repo_root;
use crate::types::{ActivityState, Session, SessionStatus};

/// A kept-alive merged worker must have been idle this long before its
/// build output counts as finished — an ended turn can still leave a
/// background build writing `target/`. Arbitrary; bump if that bites.
pub const MERGED_IDLE_GRACE_MS: i64 = 10 * 60 * 1000;

/// Whether a session is past the point of building anything in its
/// worktree again: `Done`/`Terminated`, flagged by `ninox complete`, or
/// merged and idle for `MERGED_IDLE_GRACE_MS`. `Interrupted` is resumable,
/// and merged-but-`Unknown` activity can't be told apart from a worker
/// mid-compile, so neither qualifies.
pub fn build_output_finished(session: &Session, now: i64) -> bool {
    matches!(session.status, SessionStatus::Done | SessionStatus::Terminated)
        || session.self_completed_at.is_some()
        || (session.merged_at.is_some()
            && session.activity == ActivityState::Idle
            && session.activity_since.is_some_and(|t| now.saturating_sub(t) >= MERGED_IDLE_GRACE_MS)
            && !session.status.is_terminal())
}

fn finished_since(session: &Session) -> i64 {
    session.terminal_at.or(session.self_completed_at).or(session.merged_at).unwrap_or(session.started_at)
}

/// A finished session whose ninox-managed worktree may have build output
/// to free.
#[derive(Debug, Clone)]
pub struct BuildOutputCandidate {
    pub session_id: String,
    pub workspace: PathBuf,
    pub finished_since: i64,
}

/// Every finished local worker session in `sessions` that owns a
/// `<repo>/.claude/worktrees/<id>` worktree, oldest finish first. Excludes
/// `orchestrator_ids`, remote sessions, and any worktree a not-finished
/// session (of any orchestrator) also points at — a respawn reusing a path
/// is live, whatever an older row says.
pub fn build_output_candidates(sessions: &[Session], orchestrator_ids: &HashSet<&str>, now: i64) -> Vec<BuildOutputCandidate> {
    let live = live_workspaces(sessions, now);
    let mut out: Vec<BuildOutputCandidate> = sessions
        .iter()
        .filter(|s| !orchestrator_ids.contains(s.id.as_str()) && s.machine_id.is_none() && build_output_finished(s, now))
        .filter_map(|s| {
            let ws = s.workspace_path.as_deref()?;
            ninox_repo_root(ws, &s.id)?;
            let workspace = PathBuf::from(ws);
            (!live.contains(&canonical(&workspace))).then(|| BuildOutputCandidate {
                session_id: s.id.clone(),
                workspace,
                finished_since: finished_since(s),
            })
        })
        .collect();
    out.sort_by_key(|c| c.finished_since);
    out
}

/// Canonical workspace paths of every session that isn't finished — the set
/// build-output freeing must never reach into.
pub fn live_workspaces(sessions: &[Session], now: i64) -> HashSet<PathBuf> {
    sessions
        .iter()
        .filter(|s| !build_output_finished(s, now))
        .filter_map(|s| s.workspace_path.as_deref().map(|p| canonical(Path::new(p))))
        .collect()
}

fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildDirSkip {
    /// Absolute, `..`, or otherwise not a plain relative path.
    InvalidPath,
    /// The dir, or a component on the way to it, is a symlink.
    Symlink,
    NotADirectory,
    /// The workspace isn't the top level of a git worktree, so ignore rules
    /// can't be checked.
    NotAWorktree,
    NotIgnored,
    /// Ignored, but git tracks (force-added) or sees untracked files inside.
    HasGitContent,
    /// A live session's workspace sits inside this directory.
    ContainsLiveWorkspace,
    RemoveFailed,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FreedBuildOutput {
    pub removed: Vec<(PathBuf, u64)>,
    pub skipped: Vec<(PathBuf, BuildDirSkip)>,
}

impl FreedBuildOutput {
    pub fn bytes(&self) -> u64 {
        self.removed.iter().map(|(_, b)| b).sum()
    }
}

/// Deletes each of `dirs` (relative to `workspace`) that git ignores and
/// holds no tracked or untracked-unignored content. Blocking — runs git and
/// walks the tree.
pub fn free_build_output(workspace: &Path, dirs: &[String], live: &HashSet<PathBuf>) -> FreedBuildOutput {
    let mut report = FreedBuildOutput::default();
    let mut toplevel_ok: Option<bool> = None;
    for rel in dirs {
        let full = workspace.join(rel);
        if !is_plain_relative(rel) {
            report.skipped.push((full, BuildDirSkip::InvalidPath));
            continue;
        }
        let meta = match std::fs::symlink_metadata(&full) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.file_type().is_symlink() {
            report.skipped.push((full, BuildDirSkip::Symlink));
            continue;
        }
        if !meta.is_dir() {
            report.skipped.push((full, BuildDirSkip::NotADirectory));
            continue;
        }
        if !canonical(&full).starts_with(canonical(workspace)) {
            report.skipped.push((full, BuildDirSkip::Symlink));
            continue;
        }
        if !*toplevel_ok.get_or_insert_with(|| is_worktree_toplevel(workspace)) {
            report.skipped.push((full, BuildDirSkip::NotAWorktree));
            continue;
        }
        if let Some(reason) = git_blocks_removal(workspace, rel) {
            report.skipped.push((full, reason));
            continue;
        }
        let canon = canonical(&full);
        if live.iter().any(|l| l.starts_with(&canon)) {
            report.skipped.push((full, BuildDirSkip::ContainsLiveWorkspace));
            continue;
        }
        let bytes = dir_size(&full);
        match std::fs::remove_dir_all(&full) {
            Ok(()) => report.removed.push((full, bytes)),
            Err(e) => {
                tracing::warn!("free build output {}: {e}", full.display());
                report.skipped.push((full, BuildDirSkip::RemoveFailed));
            }
        }
    }
    report
}

/// Total size of `dirs` under `workspace` that exist as real directories —
/// a read-only measurement, no git checks.
pub fn build_output_size(workspace: &Path, dirs: &[String]) -> u64 {
    dirs.iter()
        .filter(|rel| is_plain_relative(rel))
        .map(|rel| workspace.join(rel))
        .filter(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()))
        .map(|p| dir_size(&p))
        .sum()
}

fn is_plain_relative(rel: &str) -> bool {
    let p = Path::new(rel);
    !rel.is_empty() && p.components().all(|c| matches!(c, Component::Normal(_)))
}

fn is_worktree_toplevel(workspace: &Path) -> bool {
    let Ok(out) = Command::new("git").arg("-C").arg(workspace).args(["rev-parse", "--show-toplevel"]).output() else {
        return false;
    };
    out.status.success()
        && canonical(Path::new(String::from_utf8_lossy(&out.stdout).trim())) == canonical(workspace)
}

fn git_blocks_removal(workspace: &Path, rel: &str) -> Option<BuildDirSkip> {
    let git = |args: &[&str]| Command::new("git").arg("-C").arg(workspace).args(args).output().ok();
    match git(&["check-ignore", "-q", "--", rel]) {
        Some(o) if o.status.success() => {}
        _ => return Some(BuildDirSkip::NotIgnored),
    }
    let has_output = |args: &[&str]| git(args).is_none_or(|o| !o.status.success() || !o.stdout.is_empty());
    if has_output(&["ls-files", "-z", "--", rel]) || has_output(&["status", "--porcelain", "-uall", "--", rel]) {
        return Some(BuildDirSkip::HasGitContent);
    }
    None
}

/// Allocated bytes under `path`, not following symlinks.
pub fn dir_size(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    walkdir::WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .flatten()
        .filter_map(|e| e.metadata().ok())
        .filter(|m| !m.is_dir())
        .map(|m| m.blocks() * 512)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    fn run_git(dir: &Path, args: &[&str]) {
        let status = Command::new("git").args(args).current_dir(dir).status().unwrap();
        assert!(status.success(), "git {args:?} failed in {dir:?}");
    }

    fn repo_with_worktree(name: &str) -> (TempDir, PathBuf) {
        let repo = tempfile::tempdir().unwrap();
        run_git(repo.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(repo.path().join(".gitignore"), "target/\nnode_modules/\n.claude/\n").unwrap();
        std::fs::create_dir_all(repo.path().join("src")).unwrap();
        std::fs::write(repo.path().join("src/lib.rs"), "fn a() {}\n").unwrap();
        run_git(repo.path(), &["add", "."]);
        run_git(repo.path(), &["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "-q", "-m", "init"]);
        let wt = repo.path().join(".claude/worktrees").join(name);
        run_git(repo.path(), &["worktree", "add", "-q", "-b", name, wt.to_str().unwrap()]);
        (repo, wt)
    }

    fn fill(dir: &Path) {
        std::fs::create_dir_all(dir.join("debug")).unwrap();
        std::fs::write(dir.join("debug/blob"), vec![7u8; 64 * 1024]).unwrap();
    }

    fn dirs() -> Vec<String> {
        vec!["target".into(), "node_modules/.cache".into()]
    }

    #[test]
    fn removes_ignored_build_dirs_and_keeps_tracked_and_untracked_source() {
        let (_repo, wt) = repo_with_worktree("w1");
        fill(&wt.join("target"));
        fill(&wt.join("node_modules/.cache"));
        std::fs::write(wt.join("src/new.rs"), "untracked\n").unwrap();
        std::fs::write(wt.join("src/lib.rs"), "modified\n").unwrap();

        let report = free_build_output(&wt, &dirs(), &HashSet::new());

        assert!(!wt.join("target").exists());
        assert!(!wt.join("node_modules/.cache").exists());
        assert!(wt.join("node_modules").exists(), "only the configured subdir goes");
        assert_eq!(std::fs::read_to_string(wt.join("src/lib.rs")).unwrap(), "modified\n");
        assert!(wt.join("src/new.rs").exists());
        assert_eq!(report.removed.len(), 2);
        assert!(report.bytes() >= 2 * 64 * 1024);
    }

    #[test]
    fn a_configured_dir_that_is_not_gitignored_is_kept() {
        let (_repo, wt) = repo_with_worktree("w1");
        fill(&wt.join("build"));
        let report = free_build_output(&wt, &["build".into(), "src".into()], &HashSet::new());
        assert!(wt.join("build/debug/blob").exists());
        assert!(wt.join("src/lib.rs").exists());
        assert!(report.removed.is_empty());
        assert!(report.skipped.iter().all(|(_, r)| *r == BuildDirSkip::NotIgnored));
    }

    #[test]
    fn an_ignored_dir_holding_force_added_tracked_files_is_kept() {
        let (_repo, wt) = repo_with_worktree("w1");
        fill(&wt.join("target"));
        run_git(&wt, &["add", "-f", "target/debug/blob"]);
        let report = free_build_output(&wt, &dirs(), &HashSet::new());
        assert!(wt.join("target/debug/blob").exists());
        assert!(report.removed.is_empty());
        assert!(matches!(report.skipped.as_slice(), [(_, BuildDirSkip::NotIgnored | BuildDirSkip::HasGitContent)]));
    }

    #[test]
    fn escaping_absolute_and_symlinked_paths_are_refused() {
        let (repo, wt) = repo_with_worktree("w1");
        let outside = repo.path().join("target");
        fill(&outside);
        std::os::unix::fs::symlink(&outside, wt.join("target")).unwrap();
        let abs = outside.to_string_lossy().to_string();
        std::fs::create_dir_all(outside.join("nested/.cache")).unwrap();
        std::os::unix::fs::symlink(&outside, wt.join("node_modules")).unwrap();
        let dirs = ["../../../target".into(), abs, "target".into(), "node_modules/nested/.cache".into()];
        let report = free_build_output(&wt, &dirs, &HashSet::new());
        assert!(outside.join("debug/blob").exists());
        assert!(outside.join("nested/.cache").exists());
        assert!(report.removed.is_empty());
        let reasons: Vec<_> = report.skipped.iter().map(|(_, r)| *r).collect();
        assert_eq!(reasons, vec![BuildDirSkip::InvalidPath, BuildDirSkip::InvalidPath, BuildDirSkip::Symlink, BuildDirSkip::Symlink]);
    }

    #[test]
    fn a_plain_directory_that_is_not_a_worktree_top_level_is_untouched() {
        let dir = tempfile::tempdir().unwrap();
        fill(&dir.path().join("target"));
        let report = free_build_output(dir.path(), &dirs(), &HashSet::new());
        assert!(dir.path().join("target/debug/blob").exists());
        assert_eq!(report.skipped, vec![(dir.path().join("target"), BuildDirSkip::NotAWorktree)]);
    }

    fn session(id: &str, ws: &str, status: SessionStatus) -> Session {
        Session {
            id: id.into(), orchestrator_id: Some("orch".into()), name: id.into(),
            repo: "r".into(), status, agent_type: "c".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: Some(ws.into()), pid: None,
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

    #[test]
    fn finished_states_are_done_terminated_self_completed_and_merged_idle() {
        let ws = "/r/.claude/worktrees/x";
        let now = 100 * MERGED_IDLE_GRACE_MS;
        assert!(build_output_finished(&session("x", ws, SessionStatus::Done), now));
        assert!(build_output_finished(&session("x", ws, SessionStatus::Terminated), now));
        assert!(!build_output_finished(&session("x", ws, SessionStatus::Working), now));
        assert!(!build_output_finished(&session("x", ws, SessionStatus::Interrupted), now));

        let mut s = session("x", ws, SessionStatus::Working);
        s.self_completed_at = Some(1);
        assert!(build_output_finished(&s, now));

        let mut s = session("x", ws, SessionStatus::Working);
        s.merged_at = Some(1);
        s.activity_since = Some(0);
        assert!(!build_output_finished(&s, now), "merged but activity unknown may still be compiling");
        s.activity = ActivityState::Working;
        assert!(!build_output_finished(&s, now));
        s.activity = ActivityState::Idle;
        assert!(build_output_finished(&s, now));
        s.activity_since = Some(now - MERGED_IDLE_GRACE_MS + 1);
        assert!(!build_output_finished(&s, now), "a turn that only just ended may have a background build running");
    }

    #[test]
    fn candidates_skip_orchestrators_remote_main_checkouts_and_reused_live_paths_oldest_first() {
        let mut newer = session("newer", "/r/.claude/worktrees/newer", SessionStatus::Done);
        newer.terminal_at = Some(200);
        let mut older = session("older", "/r/.claude/worktrees/older", SessionStatus::Terminated);
        older.terminal_at = Some(100);
        let orch = session("orch", "/r/.claude/worktrees/orch", SessionStatus::Done);
        let mut remote = session("remote", "/r/.claude/worktrees/remote", SessionStatus::Done);
        remote.machine_id = Some("m".into());
        let main_checkout = session("main", "/r", SessionStatus::Done);
        let stale = session("reused", "/r/.claude/worktrees/reused", SessionStatus::Done);
        let mut live = session("reused-live", "/r/.claude/worktrees/reused", SessionStatus::Working);
        live.orchestrator_id = Some("other-orch".into());

        let sessions = vec![newer, older, orch, remote, main_checkout, stale, live];
        let orchs: HashSet<&str> = ["orch"].into_iter().collect();
        let ids: Vec<_> = build_output_candidates(&sessions, &orchs, 0).into_iter().map(|c| c.session_id).collect();
        assert_eq!(ids, vec!["older", "newer"]);
    }
}
