//! Workspace validation (spec §5.3 step 1): observe each session's
//! workspace through a [`WorkspaceProbe`] and turn mismatches against the
//! store into [`Anomaly`]s. Anomalies are reported, never repaired by
//! guessing.

use serde::Serialize;
use std::path::Path;

/// What is actually on disk at a session's workspace path.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct WorkspaceObservation {
    pub exists: bool,
    /// Inside a git work tree.
    pub is_git: bool,
    /// Checked-out branch; `None` when detached or not a git work tree.
    pub branch: Option<String>,
    /// Uncommitted changes (tracked or untracked); `None` when unknown.
    pub dirty:  Option<bool>,
}

pub trait WorkspaceProbe: Send + Sync {
    fn observe(&self, path: &Path) -> WorkspaceObservation;
}

/// The real probe: shells out to `git`.
pub struct GitProbe;

impl WorkspaceProbe for GitProbe {
    fn observe(&self, path: &Path) -> WorkspaceObservation {
        if !path.is_dir() {
            return WorkspaceObservation::default();
        }
        let is_git = git(path, &["rev-parse", "--is-inside-work-tree"])
            .is_some_and(|s| s.trim() == "true");
        if !is_git {
            return WorkspaceObservation { exists: true, ..Default::default() };
        }
        WorkspaceObservation {
            exists: true,
            is_git,
            branch: current_branch(path),
            dirty:  git(path, &["status", "--porcelain", "--untracked-files=all"]).map(|s| has_user_changes(&s)),
        }
    }
}

/// Files ninox itself writes into every worker worktree (statusline/hook
/// settings in `spawn_util::ensure_statusline_settings`, seeded skills).
/// They make every worker look dirty, which would bury the real "has
/// uncommitted work" signal in the recovery briefing.
const NINOX_MANAGED_PREFIXES: &[&str] = &[".claude/settings.json", ".claude/settings.local.json", ".claude/skills/"];

fn has_user_changes(porcelain: &str) -> bool {
    porcelain.lines().filter(|l| l.len() > 3).any(|l| {
        let path = l[3..].rsplit(" -> ").next().unwrap_or("").trim_matches('"');
        !NINOX_MANAGED_PREFIXES.iter().any(|p| path.starts_with(p))
    })
}

fn git(path: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").arg("-C").arg(path).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).to_string())
}

/// The checked-out branch at `path`; `None` when detached or not a repo.
/// Used at spawn time to record a worker's branch.
pub fn current_branch(path: &Path) -> Option<String> {
    let b = git(path, &["symbolic-ref", "--quiet", "--short", "HEAD"])?;
    let b = b.trim();
    (!b.is_empty()).then(|| b.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Anomaly {
    NoWorkspaceRecorded,
    WorkspaceMissing { path: String },
    NotAGitWorktree { path: String },
    /// Informational: the worktree is on a different branch (or detached)
    /// than the last one recorded. Agents switch branches as part of their
    /// work, and the record only follows `git checkout -b`/`switch -c` via
    /// the git wrapper, so it can't be authoritative; the conversation is
    /// keyed to the workspace path, which is checked separately.
    BranchMismatch { expected: String, actual: Option<String> },
    /// The worker's orchestrator no longer exists (or is gone for good), so
    /// a restored worker would report to nobody.
    OrchestratorGone { orchestrator_id: String },
    /// Informational: an orchestrator's workspace is a plain directory that
    /// the resume path recreates; its conversation is keyed by path.
    OrchestratorWorkspaceRecreated { path: String },
    /// Informational: restarting fresh without a recorded task brief means
    /// the worker only gets its state, not its original instructions.
    NoTaskBrief,
}

impl Anomaly {
    /// Blocking anomalies keep the session out of the restore.
    pub fn is_blocking(&self) -> bool {
        !matches!(self, Self::OrchestratorWorkspaceRecreated { .. } | Self::NoTaskBrief | Self::BranchMismatch { .. })
    }

    pub fn describe(&self) -> String {
        match self {
            Self::NoWorkspaceRecorded => "no workspace recorded".into(),
            Self::WorkspaceMissing { path } => format!("workspace `{path}` is missing"),
            Self::NotAGitWorktree { path } => format!("workspace `{path}` is not a git worktree"),
            Self::BranchMismatch { expected, actual: Some(a) } =>
                format!("worktree is on branch `{a}`, expected `{expected}`"),
            Self::BranchMismatch { expected, actual: None } =>
                format!("worktree has a detached HEAD, expected branch `{expected}`"),
            Self::OrchestratorGone { orchestrator_id } =>
                format!("its orchestrator `{orchestrator_id}` no longer exists"),
            Self::OrchestratorWorkspaceRecreated { path } =>
                format!("orchestrator workspace `{path}` is missing and will be recreated"),
            Self::NoTaskBrief => "no task brief was recorded at spawn".into(),
        }
    }
}

/// Workspace checks for one session. `is_orchestrator` relaxes the git
/// checks (orchestrator workspaces are plain directories). Branch checks
/// apply only when a branch was recorded — legacy sessions have none, and
/// guessing one would invent an anomaly.
pub fn validate_workspace(
    workspace:       Option<&str>,
    is_orchestrator: bool,
    recorded_branch: Option<&str>,
    obs:             &WorkspaceObservation,
) -> Vec<Anomaly> {
    let Some(path) = workspace else { return vec![Anomaly::NoWorkspaceRecorded] };
    if !obs.exists {
        return vec![if is_orchestrator {
            Anomaly::OrchestratorWorkspaceRecreated { path: path.into() }
        } else {
            Anomaly::WorkspaceMissing { path: path.into() }
        }];
    }
    if is_orchestrator {
        return Vec::new();
    }
    let Some(expected) = recorded_branch else { return Vec::new() };
    if !obs.is_git {
        return vec![Anomaly::NotAGitWorktree { path: path.into() }];
    }
    if obs.branch.as_deref() != Some(expected) {
        return vec![Anomaly::BranchMismatch { expected: expected.into(), actual: obs.branch.clone() }];
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(exists: bool, is_git: bool, branch: Option<&str>, dirty: Option<bool>) -> WorkspaceObservation {
        WorkspaceObservation { exists, is_git, branch: branch.map(str::to_string), dirty }
    }

    #[test]
    fn healthy_worker_has_no_anomalies() {
        let o = obs(true, true, Some("w1"), Some(true));
        assert!(validate_workspace(Some("/r/w1"), false, Some("w1"), &o).is_empty());
    }

    #[test]
    fn missing_workspace_is_blocking_for_workers_only() {
        let o = WorkspaceObservation::default();
        let w = validate_workspace(Some("/r/w1"), false, Some("w1"), &o);
        assert_eq!(w, vec![Anomaly::WorkspaceMissing { path: "/r/w1".into() }]);
        assert!(w[0].is_blocking());
        let orch = validate_workspace(Some("/o/x"), true, None, &o);
        assert!(!orch[0].is_blocking());
    }

    #[test]
    fn branch_mismatch_and_detached_head() {
        let a = validate_workspace(Some("/r/w1"), false, Some("w1"), &obs(true, true, Some("main"), None));
        assert_eq!(a, vec![Anomaly::BranchMismatch { expected: "w1".into(), actual: Some("main".into()) }]);
        let d = validate_workspace(Some("/r/w1"), false, Some("w1"), &obs(true, true, None, None));
        assert!(d[0].describe().contains("detached"));
        assert!(!a[0].is_blocking() && !d[0].is_blocking(), "a worker that switched branches still restores");
    }

    #[test]
    fn no_recorded_branch_means_no_branch_check() {
        let o = obs(true, false, None, None);
        assert!(validate_workspace(Some("/r/w1"), false, None, &o).is_empty());
    }

    #[test]
    fn non_git_workspace_with_recorded_branch_is_flagged() {
        let o = obs(true, false, None, None);
        assert_eq!(
            validate_workspace(Some("/r/w1"), false, Some("w1"), &o),
            vec![Anomaly::NotAGitWorktree { path: "/r/w1".into() }],
        );
    }

    #[test]
    fn no_workspace_recorded() {
        assert_eq!(
            validate_workspace(None, false, None, &WorkspaceObservation::default()),
            vec![Anomaly::NoWorkspaceRecorded],
        );
    }

    #[test]
    fn git_probe_reads_branch_and_dirty_state() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        let run = |args: &[&str]| {
            assert!(std::process::Command::new("git").arg("-C").arg(p).args(args)
                .output().unwrap().status.success(), "git {args:?}");
        };
        run(&["init", "-q", "-b", "feat-x"]);
        run(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "i"]);
        let clean = GitProbe.observe(p);
        assert_eq!(clean, obs(true, true, Some("feat-x"), Some(false)));
        std::fs::create_dir_all(p.join(".claude/skills/brain")).unwrap();
        std::fs::write(p.join(".claude/settings.json"), "{}").unwrap();
        std::fs::write(p.join(".claude/skills/brain/SKILL.md"), "x").unwrap();
        assert_eq!(GitProbe.observe(p).dirty, Some(false), "ninox-managed files are not user changes");
        std::fs::write(p.join("f"), "x").unwrap();
        assert_eq!(GitProbe.observe(p).dirty, Some(true));
        assert_eq!(current_branch(p).as_deref(), Some("feat-x"));
        assert!(!GitProbe.observe(&p.join("nope")).exists);
    }
}
