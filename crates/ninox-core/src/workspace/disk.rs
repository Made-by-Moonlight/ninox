use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::{
    build_output_candidates, build_output_finished, BuildOutputCandidate, build_output_size, classify_repo_orphans, dir_size,
    discover_repo_roots, free_build_output, live_workspaces, sweep_repo_orphans, OrphanStatus,
};
use crate::config::{OrphanSweepConfig, WorkspacesConfig};
use crate::store::Store;
use crate::types::{Notification, NotificationKind};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DiskUsage {
    pub total_bytes: u64,
    pub avail_bytes: u64,
}

impl DiskUsage {
    pub fn free_percent(&self) -> f64 {
        if self.total_bytes == 0 {
            return 100.0;
        }
        self.avail_bytes as f64 * 100.0 / self.total_bytes as f64
    }
}

pub fn statvfs_usage(path: &Path) -> Option<DiskUsage> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let frsize = st.f_frsize as u64;
    Some(DiskUsage { total_bytes: st.f_blocks as u64 * frsize, avail_bytes: st.f_bavail as u64 * frsize })
}

pub type DiskProbe = Arc<dyn Fn(&Path) -> Option<DiskUsage> + Send + Sync>;

/// Arbitrary; long enough that a disk nothing can relieve doesn't re-run
/// the orphan sweep every tick.
pub const RELIEF_COOLDOWN_MS: i64 = 15 * 60 * 1000;
/// Each pass that frees nothing doubles the relief cooldown, up to
/// `RELIEF_COOLDOWN_MS << 3` (2h). Arbitrary.
const MAX_RELIEF_BACKOFF_SHIFT: u32 = 3;
/// Arbitrary; how often a still-full disk re-raises its notification.
pub const NOTIFY_COOLDOWN_MS: i64 = 6 * 60 * 60 * 1000;

#[derive(Debug, Clone, PartialEq)]
pub enum PressureStep {
    FreedFinishedBuilds { sessions: usize, bytes: u64 },
    SweptOrphans { removed: usize },
    Notified,
}

/// Poller-owned state for freeing finished sessions' build output and
/// relieving disk pressure. Only ever acts on sessions in the store; a live
/// session's worktree is never touched.
pub struct DiskJanitor {
    probe: DiskProbe,
    freed: Mutex<HashSet<String>>,
    last_relief_at: Mutex<Option<i64>>,
    fruitless_passes: Mutex<u32>,
    last_notified_at: Mutex<Option<i64>>,
}

impl Default for DiskJanitor {
    fn default() -> Self {
        Self::with_probe(Arc::new(statvfs_usage))
    }
}

impl DiskJanitor {
    pub fn with_probe(probe: DiskProbe) -> Self {
        Self {
            probe,
            freed: Mutex::new(HashSet::new()),
            last_relief_at: Mutex::new(None),
            fruitless_passes: Mutex::new(0),
            last_notified_at: Mutex::new(None),
        }
    }

    /// Frees the build output of every session that finished since the last
    /// call. A session that goes live again is forgotten, so its next finish
    /// frees again — except a merged worker, which is freed once per merge.
    pub async fn free_newly_finished(&self, store: &Store, ws: &WorkspacesConfig) -> Vec<(String, u64)> {
        if !ws.free_build_output_on_finish {
            return Vec::new();
        }
        let now = crate::lifecycle::poller::now_millis();
        let Some((sessions, orch_ids)) = load(store) else { return Vec::new() };
        let pending: Vec<_> = {
            let mut freed = self.freed.lock().unwrap_or_else(|e| e.into_inner());
            freed.retain(|id| {
                sessions.iter().any(|s| &s.id == id && (s.merged_at.is_some() || build_output_finished(s, now)))
            });
            let orchs: HashSet<&str> = orch_ids.iter().map(String::as_str).collect();
            build_output_candidates(&sessions, &orchs, now).into_iter().filter(|c| freed.insert(c.session_id.clone())).collect()
        };
        let mut out = Vec::new();
        for c in pending {
            if let Some(report) = free_if_still_finished(store, &c, &ws.build_output_dirs).await {
                for (path, bytes) in &report.removed {
                    tracing::info!("freed build output {} ({} for finished session {})", path.display(), gb(*bytes), c.session_id);
                }
                out.push((c.session_id, report.bytes()));
            }
        }
        out
    }

    /// One disk-pressure pass: if the emptiest repo volume is below
    /// `ws.disk_low_percent` free, frees finished sessions' build output
    /// oldest first, then runs the orphan sweep, then — still low — returns
    /// a notification. Rate-limited by `RELIEF_COOLDOWN_MS` and
    /// `NOTIFY_COOLDOWN_MS`. Independent of `free_build_output_on_finish`;
    /// `disk_low_percent = 0` turns it off.
    pub async fn relieve_pressure(
        &self, store: &Store, ws: &WorkspacesConfig, orphan_sweep: &OrphanSweepConfig, retention_ms: i64, now: i64,
    ) -> (Vec<PressureStep>, Option<Notification>) {
        let mut steps = Vec::new();
        let Ok(repos) = discover_repo_roots(store) else { return (steps, None) };
        let threshold = ws.disk_low_percent;
        let low = |repos: &[PathBuf]| self.lowest(repos).filter(|u| u.free_percent() < threshold);
        if repos.is_empty() || low(&repos).is_none() {
            *self.fruitless_passes.lock().unwrap_or_else(|e| e.into_inner()) = 0;
            return (steps, None);
        }
        let cooldown = {
            let n = *self.fruitless_passes.lock().unwrap_or_else(|e| e.into_inner());
            RELIEF_COOLDOWN_MS << n.min(MAX_RELIEF_BACKOFF_SHIFT)
        };
        {
            let mut last = self.last_relief_at.lock().unwrap_or_else(|e| e.into_inner());
            if last.is_some_and(|t| now.saturating_sub(t) < cooldown) {
                return (steps, None);
            }
            *last = Some(now);
        }

        if let Some((sessions, orch_ids)) = load(store) {
            let orchs: HashSet<&str> = orch_ids.iter().map(String::as_str).collect();
            let (mut count, mut bytes) = (0, 0);
            for c in build_output_candidates(&sessions, &orchs, now) {
                let freed = free_if_still_finished(store, &c, &ws.build_output_dirs).await.map_or(0, |r| r.bytes());
                if freed > 0 {
                    count += 1;
                    bytes += freed;
                    if low(&repos).is_none() {
                        break;
                    }
                }
            }
            steps.push(PressureStep::FreedFinishedBuilds { sessions: count, bytes });
        }
        if low(&repos).is_none() {
            self.record_pass(&steps);
            return (steps, None);
        }

        if orphan_sweep.enabled {
            let mut removed = 0;
            for repo in &repos {
                match sweep_repo_orphans(store, repo, retention_ms).await {
                    Ok(report) => {
                        removed += report.actions.iter().filter(|a| a.outcome.as_ref().is_some_and(|o| o.worktree_removed)).count();
                    }
                    Err(e) => tracing::warn!("disk pressure orphan sweep {}: {e}", repo.display()),
                }
            }
            steps.push(PressureStep::SweptOrphans { removed });
        }
        let Some(usage) = low(&repos) else {
            self.record_pass(&steps);
            return (steps, None);
        };

        {
            let mut last = self.last_notified_at.lock().unwrap_or_else(|e| e.into_inner());
            if last.is_some_and(|t| now.saturating_sub(t) < NOTIFY_COOLDOWN_MS) {
                self.record_pass(&steps);
                return (steps, None);
            }
            *last = Some(now);
        }
        let (live_bytes, orphan_bytes) = self.measure(store, ws, &repos, retention_ms).await;
        steps.push(PressureStep::Notified);
        let full = (100.0 - usage.free_percent()).round() as u64;
        let notification = Notification {
            id: format!("disk-pressure-{now}"),
            kind: NotificationKind::DiskPressure,
            title: format!("Disk is {full}% full"),
            body: format!("{} in live workers' builds, {} in orphaned worktrees", gb(live_bytes), gb(orphan_bytes)),
            session_id: None,
            created_at: now,
        };
        self.record_pass(&steps);
        (steps, Some(notification))
    }

    fn record_pass(&self, steps: &[PressureStep]) {
        let freed = steps.iter().any(|s| match s {
            PressureStep::FreedFinishedBuilds { bytes, .. } => *bytes > 0,
            PressureStep::SweptOrphans { removed } => *removed > 0,
            PressureStep::Notified => false,
        });
        let mut n = self.fruitless_passes.lock().unwrap_or_else(|e| e.into_inner());
        *n = if freed { 0 } else { n.saturating_add(1) };
    }

    fn lowest(&self, repos: &[PathBuf]) -> Option<DiskUsage> {
        lowest_of(&self.probe, repos)
    }

    async fn measure(&self, store: &Store, ws: &WorkspacesConfig, repos: &[PathBuf], retention_ms: i64) -> (u64, u64) {
        let mut orphan_dirs = Vec::new();
        for repo in repos {
            if let Ok(entries) = classify_repo_orphans(store, repo, retention_ms).await {
                orphan_dirs.extend(entries.into_iter().filter(|(_, s, _)| matches!(s, OrphanStatus::Orphan(_))).map(|(p, _, _)| p));
            }
        }
        let live: Vec<PathBuf> = load(store)
            .map(|(sessions, _)| {
                sessions
                    .iter()
                    .filter(|s| s.machine_id.is_none() && !build_output_finished(s, crate::lifecycle::poller::now_millis()))
                    .filter_map(|s| s.workspace_path.as_deref().map(PathBuf::from))
                    .collect()
            })
            .unwrap_or_default();
        let dirs = ws.build_output_dirs.clone();
        tokio::task::spawn_blocking(move || {
            let live_bytes = live.iter().map(|w| build_output_size(w, &dirs)).sum();
            let orphan_bytes = orphan_dirs.iter().map(|d| dir_size(d)).sum();
            (live_bytes, orphan_bytes)
        })
        .await
        .unwrap_or((0, 0))
    }
}

/// Re-reads the store right before deleting, so a session that resumed — or
/// a live row now sharing the path — since the candidate list was built is
/// left alone.
async fn free_if_still_finished(
    store: &Store, candidate: &BuildOutputCandidate, dirs: &[String],
) -> Option<super::FreedBuildOutput> {
    let now = crate::lifecycle::poller::now_millis();
    let (sessions, orch_ids) = load(store)?;
    let orchs: HashSet<&str> = orch_ids.iter().map(String::as_str).collect();
    let still = build_output_candidates(&sessions, &orchs, now)
        .into_iter()
        .any(|c| c.session_id == candidate.session_id && c.workspace == candidate.workspace);
    if !still {
        return None;
    }
    let live = live_workspaces(&sessions, now);
    let (workspace, dirs) = (candidate.workspace.clone(), dirs.to_vec());
    tokio::task::spawn_blocking(move || free_build_output(&workspace, &dirs, &live)).await.ok()
}

fn lowest_of(probe: &DiskProbe, repos: &[PathBuf]) -> Option<DiskUsage> {
    repos.iter().filter_map(|r| probe(r)).min_by(|a, b| a.free_percent().total_cmp(&b.free_percent()))
}

fn load(store: &Store) -> Option<(Vec<crate::types::Session>, Vec<String>)> {
    let sessions = store.list_sessions().ok()?;
    let orchs = store.list_orchestrators().ok()?.into_iter().map(|o| o.id).collect();
    Some((sessions, orchs))
}

fn gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ActivityState, Session, SessionStatus};
    use crate::workspace::{write_marker, WorkspaceMarker};
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    fn run_git(dir: &Path, args: &[&str]) {
        let status = Command::new("git").args(args).current_dir(dir).status().unwrap();
        assert!(status.success(), "git {args:?} failed in {dir:?}");
    }

    fn repo() -> TempDir {
        let repo = tempfile::tempdir().unwrap();
        run_git(repo.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(repo.path().join(".gitignore"), "target/\n.claude/\n").unwrap();
        std::fs::write(repo.path().join("lib.rs"), "fn a() {}\n").unwrap();
        run_git(repo.path(), &["add", "."]);
        run_git(repo.path(), &["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "-q", "-m", "init"]);
        repo
    }

    fn worktree(repo: &Path, id: &str) -> PathBuf {
        let wt = repo.join(".claude/worktrees").join(id);
        run_git(repo, &["worktree", "add", "-q", "-b", id, wt.to_str().unwrap()]);
        std::fs::create_dir_all(wt.join("target/debug")).unwrap();
        std::fs::write(wt.join("target/debug/blob"), vec![1u8; 32 * 1024]).unwrap();
        wt
    }

    fn session(id: &str, wt: &Path, status: SessionStatus) -> Session {
        Session {
            id: id.into(), orchestrator_id: Some("orch".into()), name: id.into(),
            repo: "r".into(), status, agent_type: "c".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: Some(wt.to_string_lossy().into()), pid: None,
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

    fn store() -> Store {
        Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap()
    }

    fn ws() -> WorkspacesConfig {
        WorkspacesConfig { build_output_dirs: vec!["target".into()], ..Default::default() }
    }

    /// Free-space probe reporting 2% free for the first `low_calls` calls,
    /// then 50%.
    fn probe(low_calls: usize) -> DiskProbe {
        let n = Arc::new(AtomicUsize::new(0));
        Arc::new(move |_| {
            let free = if n.fetch_add(1, Ordering::SeqCst) < low_calls { 2 } else { 50 };
            Some(DiskUsage { total_bytes: 100, avail_bytes: free })
        })
    }

    #[tokio::test]
    async fn every_finished_transition_frees_build_output_and_a_live_session_is_untouched() {
        let _iso = crate::test_isolation::isolate();
        let repo = repo();
        let store = store();

        let done = worktree(repo.path(), "done");
        store.upsert_session(&session("done", &done, SessionStatus::Done)).unwrap();
        let terminated = worktree(repo.path(), "terminated");
        store.upsert_session(&session("terminated", &terminated, SessionStatus::Terminated)).unwrap();
        let completed = worktree(repo.path(), "completed");
        let mut s = session("completed", &completed, SessionStatus::Working);
        s.self_completed_at = Some(1);
        store.upsert_session(&s).unwrap();
        let merged = worktree(repo.path(), "merged");
        let mut s = session("merged", &merged, SessionStatus::Working);
        s.merged_at = Some(1);
        s.activity = ActivityState::Idle;
        s.activity_since = Some(0);
        store.upsert_session(&s).unwrap();
        let live = worktree(repo.path(), "live");
        let mut s = session("live", &live, SessionStatus::Working);
        s.orchestrator_id = Some("another-orchestrator".into());
        store.upsert_session(&s).unwrap();

        std::fs::write(done.join("lib.rs"), "uncommitted\n").unwrap();
        std::fs::write(done.join("new.rs"), "untracked\n").unwrap();

        let janitor = DiskJanitor::with_probe(probe(0));
        let mut freed: Vec<_> = janitor.free_newly_finished(&store, &ws()).await.into_iter().map(|(id, _)| id).collect();
        freed.sort();
        assert_eq!(freed, vec!["completed", "done", "merged", "terminated"]);
        for wt in [&done, &terminated, &completed, &merged] {
            assert!(!wt.join("target").exists(), "{} kept its build output", wt.display());
            assert!(wt.join("lib.rs").exists(), "the worktree itself stays");
        }
        assert_eq!(std::fs::read_to_string(done.join("lib.rs")).unwrap(), "uncommitted\n");
        assert!(done.join("new.rs").exists());
        assert!(live.join("target/debug/blob").exists(), "a live session's build dir is never touched");

        std::fs::create_dir_all(done.join("target")).unwrap();
        assert!(janitor.free_newly_finished(&store, &ws()).await.is_empty(), "each finish is handled once");
    }

    #[tokio::test]
    async fn a_merged_worker_that_resumes_is_not_freed_again_on_its_next_idle() {
        let _iso = crate::test_isolation::isolate();
        let repo = repo();
        let store = store();
        let merged = worktree(repo.path(), "merged");
        let mut s = session("merged", &merged, SessionStatus::Working);
        s.merged_at = Some(1);
        s.activity = ActivityState::Idle;
        s.activity_since = Some(0);
        store.upsert_session(&s).unwrap();

        let janitor = DiskJanitor::with_probe(probe(0));
        assert_eq!(janitor.free_newly_finished(&store, &ws()).await.len(), 1);

        s.activity = ActivityState::Working;
        store.upsert_session(&s).unwrap();
        assert!(janitor.free_newly_finished(&store, &ws()).await.is_empty());
        std::fs::create_dir_all(merged.join("target/debug")).unwrap();
        s.activity = ActivityState::Idle;
        store.upsert_session(&s).unwrap();
        assert!(janitor.free_newly_finished(&store, &ws()).await.is_empty());
        assert!(merged.join("target/debug").exists(), "the resumed turn's build survives");
    }

    #[tokio::test]
    async fn freeing_on_finish_can_be_turned_off() {
        let _iso = crate::test_isolation::isolate();
        let repo = repo();
        let store = store();
        let done = worktree(repo.path(), "done");
        store.upsert_session(&session("done", &done, SessionStatus::Done)).unwrap();
        let cfg = WorkspacesConfig { free_build_output_on_finish: false, ..ws() };
        assert!(DiskJanitor::with_probe(probe(0)).free_newly_finished(&store, &cfg).await.is_empty());
        assert!(done.join("target").exists());
    }

    /// One finished session, one live session, one orphan marked ours.
    fn pressure_fixture(repo: &Path, store: &Store) -> (PathBuf, PathBuf, PathBuf) {
        let finished = worktree(repo, "finished");
        let mut s = session("finished", &finished, SessionStatus::Done);
        s.terminal_at = Some(crate::lifecycle::poller::now_millis());
        store.upsert_session(&s).unwrap();
        let live = worktree(repo, "live");
        store.upsert_session(&session("live", &live, SessionStatus::Working)).unwrap();
        let orphan = worktree(repo, "orphan");
        write_marker(&orphan, &WorkspaceMarker {
            session_id: "orphan".into(), repo: repo.to_string_lossy().into(), branch: "orphan".into(), created_at: 0,
        }).unwrap();
        (finished, live, orphan)
    }

    const RETENTION_MS: i64 = 2 * 24 * 60 * 60 * 1000;

    #[tokio::test]
    async fn pressure_frees_finished_builds_then_sweeps_orphans_then_notifies() {
        let _iso = crate::test_isolation::isolate();
        let repo = repo();
        let store = store();
        let (finished, live, orphan) = pressure_fixture(repo.path(), &store);

        let janitor = DiskJanitor::with_probe(probe(usize::MAX));
        let (steps, n) = janitor.relieve_pressure(&store, &ws(), &OrphanSweepConfig::default(), RETENTION_MS, 1_000).await;

        assert!(matches!(steps.as_slice(), [
            PressureStep::FreedFinishedBuilds { sessions: 1, .. },
            PressureStep::SweptOrphans { removed: 1 },
            PressureStep::Notified,
        ]), "{steps:?}");
        assert!(!finished.join("target").exists());
        assert!(finished.exists(), "freeing build output keeps the worktree");
        assert!(!orphan.exists());
        assert!(live.join("target/debug/blob").exists());
        let n = n.expect("still low after relief");
        assert_eq!(n.kind, NotificationKind::DiskPressure);
        assert_eq!(n.title, "Disk is 98% full");
        assert_eq!(n.body, "0.0 GB in live workers' builds, 0.0 GB in orphaned worktrees");
    }

    #[tokio::test]
    async fn pressure_stops_once_freeing_finished_builds_is_enough() {
        let _iso = crate::test_isolation::isolate();
        let repo = repo();
        let store = store();
        let (finished, _live, orphan) = pressure_fixture(repo.path(), &store);

        let janitor = DiskJanitor::with_probe(probe(1));
        let (steps, n) = janitor.relieve_pressure(&store, &ws(), &OrphanSweepConfig::default(), RETENTION_MS, 1_000).await;

        assert!(matches!(steps.as_slice(), [PressureStep::FreedFinishedBuilds { sessions: 1, .. }]), "{steps:?}");
        assert!(n.is_none());
        assert!(!finished.join("target").exists());
        assert!(orphan.exists(), "the sweep never ran");
    }

    #[tokio::test]
    async fn no_pressure_touches_nothing() {
        let _iso = crate::test_isolation::isolate();
        let repo = repo();
        let store = store();
        let (finished, _live, orphan) = pressure_fixture(repo.path(), &store);

        let janitor = DiskJanitor::with_probe(probe(0));
        let (steps, n) = janitor.relieve_pressure(&store, &ws(), &OrphanSweepConfig::default(), RETENTION_MS, 1_000).await;

        assert!(steps.is_empty() && n.is_none());
        assert!(finished.join("target").exists());
        assert!(orphan.exists());
    }

    #[tokio::test]
    async fn pressure_relief_and_its_notification_are_rate_limited() {
        let _iso = crate::test_isolation::isolate();
        let repo = repo();
        let store = store();
        pressure_fixture(repo.path(), &store);
        let janitor = DiskJanitor::with_probe(probe(usize::MAX));
        let cfg = OrphanSweepConfig::default();
        let t0 = 1_000;

        let (_, n) = janitor.relieve_pressure(&store, &ws(), &cfg, RETENTION_MS, t0).await;
        assert!(n.is_some());

        let (steps, n) = janitor.relieve_pressure(&store, &ws(), &cfg, RETENTION_MS, t0 + 1).await;
        assert!(steps.is_empty() && n.is_none(), "inside the relief cooldown nothing runs");

        let (steps, n) = janitor.relieve_pressure(&store, &ws(), &cfg, RETENTION_MS, t0 + RELIEF_COOLDOWN_MS).await;
        assert_eq!(steps.len(), 2, "relief runs again: {steps:?}");
        assert!(n.is_none(), "but the notification is still cooling down");

        let (_, n) = janitor.relieve_pressure(&store, &ws(), &cfg, RETENTION_MS, t0 + NOTIFY_COOLDOWN_MS).await;
        assert!(n.is_some(), "a still-full disk re-notifies after the cooldown");
    }
}
