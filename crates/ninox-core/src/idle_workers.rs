//! Idle workers: live workers parked on a review, CI run, merge or an answer
//! that still hold a slot and a worktree.
//!
//! Everything here is derived from state Ninox already stores — session
//! rows, the held-message table and the measured-disk cache. Nothing in this
//! module calls GitHub or walks a worktree except [`measure_idle_disks`] and
//! [`clean_idle_build_output`], which are blocking and run off the UI thread.

use crate::config::IdleWorkersConfig;
use crate::messaging::{AcpRouter, DeliveryReceipt, DeliveryStatus};
use crate::store::{DiskMeasurement, Store};
use crate::types::{ActivityState, GateCheck, MessageKind, Session, SessionStatus};
use crate::workspace::{self, FreedBuildOutput};
use anyhow::Result;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::Path;

/// A measurement older than this is re-taken on the next disk tick, even if
/// the worktree root looks untouched.
pub const DISK_REMEASURE_MS: i64 = 30 * 60 * 1000;

/// Worktrees measured per tick, so a fleet of large worktrees is spread
/// over several ticks instead of one long walk. Arbitrary; bump if the
/// numbers lag.
pub const MAX_MEASURED_PER_TICK: usize = 3;

static MEASURING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Held while a measurement pass runs; `None` if one is already in flight.
pub struct MeasureGuard(());

impl MeasureGuard {
    pub fn try_begin() -> Option<Self> {
        (!MEASURING.swap(true, std::sync::atomic::Ordering::SeqCst)).then_some(Self(()))
    }
}

impl Drop for MeasureGuard {
    fn drop(&mut self) {
        MEASURING.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Why a worker is parked, from the strongest signal to the weakest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitingOn {
    /// The PR merged but the worker never finished (`merged_at` is set).
    MergedNotCompleted,
    Blocked { note: Option<String> },
    /// `ninox complete` ran; the orchestrator has to finalize it.
    OrchestratorReview,
    CiFailed,
    CiRunning,
    Mergeable,
    ReviewPending,
    PrOpen,
    QueuedMessages { count: usize },
    Unknown,
}

impl WaitingOn {
    pub fn label(&self) -> String {
        match self {
            Self::MergedNotCompleted => "merged, not completed".into(),
            Self::Blocked { note: Some(n) } if !n.trim().is_empty() => format!("blocked: {}", n.trim()),
            Self::Blocked { .. } => "blocked".into(),
            Self::OrchestratorReview => "orchestrator review".into(),
            Self::CiFailed => "CI failed".into(),
            Self::CiRunning => "CI running".into(),
            Self::Mergeable => "mergeable, awaiting merge".into(),
            Self::ReviewPending | Self::PrOpen => "PR awaiting review".into(),
            Self::QueuedMessages { count } => format!("{count} queued message(s)"),
            Self::Unknown => "unknown".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct IdleWorker {
    pub id:              String,
    pub name:            String,
    pub orchestrator_id: Option<String>,
    pub repo:            String,
    pub pr_number:       Option<u64>,
    pub idle_ms:         Option<i64>,
    pub waiting_on:      WaitingOn,
    pub activity:        ActivityState,
    pub workspace_path:  Option<String>,
    pub disk:            Option<DiskMeasurement>,
}

impl IdleWorker {
    /// Whether "Clean build output" is allowed now: a worker still in a turn
    /// (e.g. merged but compiling) may be writing `target/`.
    pub fn can_clean(&self) -> bool {
        matches!(self.activity, ActivityState::Idle | ActivityState::Blocked) && self.disk.is_some_and(|d| d.build_bytes > 0)
    }

    pub fn disk_age_ms(&self, now: i64) -> Option<i64> {
        self.disk.map(|d| now.saturating_sub(d.measured_at))
    }
}

impl Serialize for DiskMeasurement {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("DiskMeasurement", 3)?;
        st.serialize_field("total_bytes", &self.total_bytes)?;
        st.serialize_field("build_bytes", &self.build_bytes)?;
        st.serialize_field("measured_at", &self.measured_at)?;
        st.end()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct OrchestratorGroup {
    /// `None` collects workers with no orchestrator; they can't be prompted.
    pub orchestrator_id: Option<String>,
    pub workers:         Vec<IdleWorker>,
    /// Epoch ms this orchestrator was last prompted about exactly this set.
    pub prompted_at:     Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct IdleReport {
    pub groups:      Vec<OrchestratorGroup>,
    pub total:       usize,
    pub total_bytes: u64,
    pub build_bytes: u64,
}

impl IdleReport {
    pub fn orchestrators_to_prompt(&self) -> usize {
        self.groups.iter().filter(|g| g.orchestrator_id.is_some()).count()
    }

    /// Groups a prompt would reach now: they have an orchestrator and were
    /// not already prompted about this set inside the cooldown.
    pub fn promptable(&self) -> impl Iterator<Item = &OrchestratorGroup> {
        self.groups.iter().filter(|g| g.orchestrator_id.is_some() && g.prompted_at.is_none())
    }

    pub fn workers(&self) -> impl Iterator<Item = &IdleWorker> {
        self.groups.iter().flat_map(|g| g.workers.iter())
    }
}

fn is_idle(s: &Session, cfg: &IdleWorkersConfig, now: i64) -> bool {
    if s.status.is_terminal() || s.self_completed_at.is_some() {
        return false;
    }
    if s.merged_at.is_some() {
        return true;
    }
    matches!(s.activity, ActivityState::Idle | ActivityState::Blocked)
        && s.activity_since.is_some_and(|t| now.saturating_sub(t) >= cfg.idle_minutes as i64 * 60_000)
}

fn idle_ms(s: &Session, now: i64) -> Option<i64> {
    let since = match s.activity {
        ActivityState::Idle | ActivityState::Blocked => s.activity_since.or(s.merged_at),
        _ => s.merged_at,
    }?;
    Some(now.saturating_sub(since).max(0))
}

pub fn waiting_on(s: &Session, queued_messages: usize) -> WaitingOn {
    if s.merged_at.is_some() {
        return WaitingOn::MergedNotCompleted;
    }
    if s.activity == ActivityState::Blocked {
        return WaitingOn::Blocked { note: s.activity_note.clone() };
    }
    match s.status {
        SessionStatus::AwaitingReview => return WaitingOn::OrchestratorReview,
        SessionStatus::CiFailed => return WaitingOn::CiFailed,
        SessionStatus::Mergeable => return WaitingOn::Mergeable,
        SessionStatus::ReviewPending => return WaitingOn::ReviewPending,
        _ => {}
    }
    if let Some(g) = &s.gate_status {
        if g.ci == GateCheck::Failing {
            return WaitingOn::CiFailed;
        }
        if g.ci == GateCheck::Pending {
            return WaitingOn::CiRunning;
        }
        if g.mergeable == GateCheck::Passing && g.review == GateCheck::Passing && g.ci == GateCheck::Passing {
            return WaitingOn::Mergeable;
        }
        if g.review != GateCheck::Passing && s.pr_number.is_some() {
            return WaitingOn::ReviewPending;
        }
    }
    if s.status == SessionStatus::PrOpen {
        return WaitingOn::PrOpen;
    }
    if queued_messages > 0 {
        return WaitingOn::QueuedMessages { count: queued_messages };
    }
    WaitingOn::Unknown
}

/// Live non-orchestrator workers that are idle, grouped by orchestrator and
/// ordered longest-idle first. Orchestrators (rows in `orchestrator_ids`, or
/// of the Orchestrator worker type) are never listed.
pub fn classify(
    sessions: &[Session], orchestrator_ids: &HashSet<String>, cfg: &IdleWorkersConfig, queued: &HashMap<String, usize>,
    disk: &HashMap<String, DiskMeasurement>, prompts: &HashMap<String, crate::store::IdlePromptRecord>, now: i64,
) -> IdleReport {
    let mut by_orch: std::collections::BTreeMap<Option<String>, Vec<IdleWorker>> = Default::default();
    for s in sessions {
        if orchestrator_ids.contains(&s.id) || s.worker_type.as_deref() == Some("Orchestrator") || !is_idle(s, cfg, now) {
            continue;
        }
        by_orch.entry(s.orchestrator_id.clone()).or_default().push(IdleWorker {
            id:              s.id.clone(),
            name:            s.name.clone(),
            orchestrator_id: s.orchestrator_id.clone(),
            repo:            s.repo.clone(),
            pr_number:       s.pr_number,
            idle_ms:         idle_ms(s, now),
            waiting_on:      waiting_on(s, queued.get(&s.id).copied().unwrap_or(0)),
            activity:        s.activity,
            workspace_path:  s.workspace_path.clone(),
            disk:            disk.get(&s.id).copied(),
        });
    }
    let mut report = IdleReport::default();
    for (orch, mut workers) in by_orch {
        workers.sort_by(|a, b| b.idle_ms.cmp(&a.idle_ms).then_with(|| a.id.cmp(&b.id)));
        report.total += workers.len();
        for w in &workers {
            if let Some(d) = w.disk {
                report.total_bytes += d.total_bytes;
                report.build_bytes += d.build_bytes.min(d.total_bytes);
            }
        }
        let cooldown_ms = cfg.prompt_cooldown_minutes as i64 * 60_000;
        let prompted_at = orch.as_ref().and_then(|o| {
            let rec = prompts.get(o)?;
            (rec.worker_set == worker_set_key(&workers) && now.saturating_sub(rec.prompted_at) < cooldown_ms)
                .then_some(rec.prompted_at)
        });
        report.groups.push(OrchestratorGroup { orchestrator_id: orch, workers, prompted_at });
    }
    report.groups.sort_by(|a, b| b.workers.len().cmp(&a.workers.len()).then_with(|| a.orchestrator_id.cmp(&b.orchestrator_id)));
    report
}

/// Reads the store and builds the report. No GitHub calls, no disk walks.
pub fn load_report(store: &Store, cfg: &IdleWorkersConfig, now: i64) -> Result<IdleReport> {
    let sessions = store.list_sessions()?;
    let orchs: HashSet<String> = store.list_orchestrators()?.into_iter().map(|o| o.id).collect();
    let queued = store.held_message_counts().unwrap_or_default().into_iter().map(|(k, (q, u))| (k, q + u)).collect();
    let disk = store.disk_measurements().unwrap_or_default();
    let prompts = store.idle_prompt_records().unwrap_or_default();
    Ok(classify(&sessions, &orchs, cfg, &queued, &disk, &prompts, now))
}

fn worker_set_key(workers: &[IdleWorker]) -> String {
    let mut ids: Vec<&str> = workers.iter().map(|w| w.id.as_str()).collect();
    ids.sort_unstable();
    ids.join(",")
}

pub fn format_duration(ms: i64) -> String {
    let mins = ms.max(0) / 60_000;
    match mins {
        0 => "<1m".into(),
        1..=59 => format!("{mins}m"),
        60..=1439 => format!("{}h{:02}m", mins / 60, mins % 60),
        _ => format!("{}d{}h", mins / 1440, (mins % 1440) / 60),
    }
}

pub fn format_bytes(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}

pub fn disk_label(w: &IdleWorker, now: i64) -> String {
    match (w.disk, w.disk_age_ms(now)) {
        (Some(d), Some(age)) => format!(
            "{} ({} build output, measured {} ago)",
            format_bytes(d.total_bytes), format_bytes(d.build_bytes), format_duration(age),
        ),
        _ => "not measured yet".into(),
    }
}

/// The message one orchestrator receives about its own idle workers.
pub fn prompt_text(group: &OrchestratorGroup, now: i64) -> String {
    let mut out = format!(
        "{} of your workers are idle and holding slots and disk. Worker, idle time, waiting on, disk:\n",
        group.workers.len(),
    );
    for w in &group.workers {
        out.push_str(&format!(
            "- {} idle {} · {} · {}\n",
            w.id,
            w.idle_ms.map(format_duration).unwrap_or_else(|| "unknown".into()),
            w.waiting_on.label(),
            disk_label(w, now),
        ));
    }
    out.push_str(
        "\nFor each worker: if its work is done or its PR merged, tell the worker to run `ninox complete` \
         (or reap it); if it is waiting on you (a review, an answer, an unblock), act on that now; \
         otherwise reply with what it is waiting for.",
    );
    out
}

#[derive(Debug, Clone)]
pub enum PromptOutcome {
    Sent { orchestrator_id: String, workers: usize, receipt: DeliveryReceipt },
    RateLimited { orchestrator_id: String, prompted_ago_ms: i64 },
    Failed { orchestrator_id: String, error: String },
}

impl PromptOutcome {
    pub fn describe(&self) -> String {
        match self {
            Self::Sent { orchestrator_id, workers, receipt } => {
                let how = match &receipt.status {
                    DeliveryStatus::Delivered => format!("delivered via {}", receipt.mechanism.unwrap_or("unknown")),
                    DeliveryStatus::Queued { .. } => "queued until it starts".to_string(),
                    DeliveryStatus::Unreachable { .. } => "queued, target unreachable (retrying)".to_string(),
                };
                let id = receipt.message_id.map(|i| format!(" (message {i})")).unwrap_or_default();
                format!("{orchestrator_id}: prompted about {workers} worker(s) — {how}{id}")
            }
            Self::RateLimited { orchestrator_id, prompted_ago_ms } => {
                format!("{orchestrator_id}: prompted {} ago, skipped", format_duration(*prompted_ago_ms))
            }
            Self::Failed { orchestrator_id, error } => format!("{orchestrator_id}: failed: {error}"),
        }
    }
}

/// Sends each orchestrator in `report` (or only `only`) its own list through
/// `send`. The cooldown claim lives in the store, so the GUI, TUI and CLI
/// share it: a second call about the same worker set inside the cooldown
/// sends nothing.
pub async fn prompt_orchestrators<F, Fut>(
    store: &Store, report: &IdleReport, only: Option<&str>, cfg: &IdleWorkersConfig, now: i64, send: F,
) -> Vec<PromptOutcome>
where
    F: Fn(String, String) -> Fut,
    Fut: Future<Output = Result<DeliveryReceipt>>,
{
    let cooldown_ms = cfg.prompt_cooldown_minutes as i64 * 60_000;
    let mut out = Vec::new();
    for group in &report.groups {
        let Some(orch) = group.orchestrator_id.as_deref() else { continue };
        if only.is_some_and(|o| o != orch) {
            continue;
        }
        let key = worker_set_key(&group.workers);
        match store.claim_idle_prompt(orch, &key, now, cooldown_ms) {
            Ok(true) => {}
            Ok(false) => {
                let ago = store.idle_prompt_records().ok().and_then(|r| r.get(orch).map(|r| now.saturating_sub(r.prompted_at)));
                out.push(PromptOutcome::RateLimited { orchestrator_id: orch.into(), prompted_ago_ms: ago.unwrap_or(0) });
                continue;
            }
            Err(e) => {
                out.push(PromptOutcome::Failed { orchestrator_id: orch.into(), error: e.to_string() });
                continue;
            }
        }
        match send(orch.to_string(), prompt_text(group, now)).await {
            Ok(receipt) => out.push(PromptOutcome::Sent { orchestrator_id: orch.into(), workers: group.workers.len(), receipt }),
            Err(e) => {
                let _ = store.release_idle_prompt(orch, &key, now);
                out.push(PromptOutcome::Failed { orchestrator_id: orch.into(), error: e.to_string() });
            }
        }
    }
    out
}

/// The delivery path GUI, TUI and CLI all use for the idle prompt: the
/// caller's own attested identity, normal receipts and queuing.
pub async fn send_prompt(store: &Store, acp: &dyn AcpRouter, orchestrator_id: &str, text: &str) -> Result<DeliveryReceipt> {
    let config = crate::config::AppConfig::load_or_default();
    let (from, sender_pane) = crate::messaging::resolve_interactive_sender(store).await;
    crate::messaging::deliver_message_or_hold(
        store, &crate::config::AppConfig::sessions_dir(), orchestrator_id, text, config.send_mechanism(),
        Some(&from), sender_pane.as_ref(), MessageKind::Freeform, acp,
    )
    .await
}

/// Measures the worktrees of `workers` whose cached number is missing or
/// older than [`DISK_REMEASURE_MS`], and drops measurements of sessions that
/// are no longer idle. Blocking; call from `spawn_blocking`.
pub fn measure_idle_disks(store: &Store, workers: &[IdleWorker], build_dirs: &[String], now: i64) {
    let cached = store.disk_measurements().unwrap_or_default();
    let mut stale: Vec<(&IdleWorker, &str)> = workers
        .iter()
        .filter_map(|w| Some((w, w.workspace_path.as_deref()?)))
        .filter(|(w, ws)| workspace::ninox_repo_root(ws, &w.id).is_some() && Path::new(ws).is_dir())
        .filter(|(w, ws)| cached.get(&w.id).is_none_or(|m| needs_remeasure(m.measured_at, now, Path::new(ws))))
        .collect();
    stale.sort_by_key(|(w, _)| cached.get(&w.id).map_or(i64::MIN, |m| m.measured_at));
    for (w, ws) in stale.into_iter().take(MAX_MEASURED_PER_TICK) {
        let m = measure(Path::new(ws), build_dirs, now);
        if let Err(e) = store.put_disk_measurement(&w.id, m) {
            tracing::warn!("idle disk cache write for {}: {e}", w.id);
        }
    }
    let keep: Vec<String> = workers.iter().map(|w| w.id.clone()).collect();
    let _ = store.prune_disk_measurements(&keep);
}

/// Stale after [`DISK_REMEASURE_MS`], or sooner when the worktree root's mtime
/// is newer than the last measurement (entries added or removed there, e.g. a
/// `target/` created or deleted).
fn needs_remeasure(measured_at: i64, now: i64, workspace: &Path) -> bool {
    if now.saturating_sub(measured_at) >= DISK_REMEASURE_MS {
        return true;
    }
    let mtime_ms = std::fs::metadata(workspace)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64);
    mtime_ms.is_some_and(|t| t > measured_at)
}

fn measure(workspace: &Path, build_dirs: &[String], now: i64) -> DiskMeasurement {
    DiskMeasurement {
        total_bytes: workspace::dir_size(workspace),
        build_bytes: workspace::build_output_size(workspace, build_dirs),
        measured_at: now,
    }
}

/// Deletes `build_dirs` (the janitor's set: `target/`, `node_modules/.cache`)
/// from one idle worker's worktree on explicit request, then re-measures it.
/// Same git-ignored / no-tracked-or-untracked-content guards as the janitor;
/// the worker itself being live is the one guard that does not apply, since
/// the caller chose it. Blocking.
pub fn clean_idle_build_output(store: &Store, session_id: &str, build_dirs: &[String], now: i64) -> Result<FreedBuildOutput> {
    let session = store.get_session(session_id)?.ok_or_else(|| anyhow::anyhow!("no session {session_id}"))?;
    if store.list_orchestrators()?.iter().any(|o| o.id == session.id) {
        anyhow::bail!("{session_id} is an orchestrator");
    }
    if session.machine_id.is_some() {
        anyhow::bail!("{session_id} runs on a remote machine");
    }
    let ws = session.workspace_path.as_deref().ok_or_else(|| anyhow::anyhow!("{session_id} has no worktree"))?;
    if workspace::ninox_repo_root(ws, &session.id).is_none() {
        anyhow::bail!("{ws} is not a ninox-managed worktree");
    }
    if !matches!(session.activity, ActivityState::Idle | ActivityState::Blocked) {
        anyhow::bail!("{session_id} is not idle right now ({:?}); its build may still be running", session.activity);
    }
    let canon = |p: &str| std::fs::canonicalize(p).unwrap_or_else(|_| p.into());
    let target = canon(ws);
    if let Some(other) = store.list_sessions()?.iter().find(|o| {
        o.id != session.id
            && !o.status.is_terminal()
            && o.workspace_path.as_deref().is_some_and(|p| canon(p) == target)
    }) {
        anyhow::bail!("{} also uses this worktree and is still live", other.id);
    }
    let freed = workspace::free_build_output(Path::new(ws), build_dirs, &HashSet::new());
    store.put_disk_measurement(session_id, measure(Path::new(ws), build_dirs, now))?;
    Ok(freed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{GateStatus, Orchestrator};
    use crate::messaging::DeliveryStatus;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    const NOW: i64 = 10_000_000_000;
    const MIN: i64 = 60_000;

    fn session(id: &str, orch: Option<&str>) -> Session {
        Session {
            id: id.into(), orchestrator_id: orch.map(Into::into), name: id.into(),
            repo: "r".into(), status: SessionStatus::PrOpen, agent_type: "c".into(), cost_usd: 0.0, started_at: 0,
            pr_number: Some(1), pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: ActivityState::Idle, activity_note: None, activity_since: Some(NOW - 45 * MIN),
            machine_id: None, worker_type: None,
            success_criteria: Vec::new(), completion_outcome: None, self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        }
    }

    fn report(sessions: &[Session], orchs: &[&str]) -> IdleReport {
        let orchs = orchs.iter().map(|s| s.to_string()).collect();
        classify(sessions, &orchs, &IdleWorkersConfig::default(), &HashMap::new(), &HashMap::new(), &HashMap::new(), NOW)
    }

    fn ids(r: &IdleReport) -> Vec<String> {
        r.workers().map(|w| w.id.clone()).collect()
    }

    #[test]
    fn idle_needs_the_threshold_and_a_live_session() {
        let fresh = Session { activity_since: Some(NOW - 5 * MIN), ..session("fresh", Some("o")) };
        let working = Session { activity: ActivityState::Working, ..session("working", Some("o")) };
        let unknown = Session { activity: ActivityState::Unknown, ..session("unknown", Some("o")) };
        let done = Session { status: SessionStatus::Done, ..session("done", Some("o")) };
        let interrupted = Session { status: SessionStatus::Interrupted, ..session("int", Some("o")) };
        let idle = session("idle", Some("o"));
        let blocked = Session { activity: ActivityState::Blocked, ..session("blocked", Some("o")) };
        let r = report(&[fresh, working, unknown, done, interrupted, idle, blocked], &[]);
        let mut got = ids(&r);
        got.sort();
        assert_eq!(got, ["blocked", "idle"]);
    }

    #[test]
    fn merged_and_not_completed_is_idle_whatever_its_activity() {
        let merged = Session { merged_at: Some(NOW - 3 * MIN), activity: ActivityState::Working, ..session("merged", Some("o")) };
        let completed = Session { merged_at: Some(NOW - 3 * MIN), self_completed_at: Some(NOW - MIN), ..session("completed", Some("o")) };
        let r = report(&[merged, completed], &[]);
        assert_eq!(ids(&r), ["merged"]);
        let w = r.workers().next().unwrap();
        assert_eq!(w.waiting_on, WaitingOn::MergedNotCompleted);
        assert_eq!(w.idle_ms, Some(3 * MIN));
    }

    #[test]
    fn orchestrators_are_never_listed() {
        let orch = session("orch-1", None);
        let typed = Session { worker_type: Some("Orchestrator".into()), ..session("sub", Some("orch-1")) };
        let worker = session("w", Some("orch-1"));
        let r = report(&[orch, typed, worker], &["orch-1"]);
        assert_eq!(ids(&r), ["w"]);
    }

    #[test]
    fn waiting_on_comes_from_stored_state() {
        let gate = |ci, review, mergeable| Some(GateStatus { ci, review, mergeable, since: 0 });
        let s = |f: &dyn Fn(&mut Session)| {
            let mut s = session("w", Some("o"));
            f(&mut s);
            waiting_on(&s, 0)
        };
        assert_eq!(s(&|s| s.activity = ActivityState::Blocked), WaitingOn::Blocked { note: None });
        assert_eq!(s(&|s| s.status = SessionStatus::CiFailed), WaitingOn::CiFailed);
        assert_eq!(s(&|s| s.gate_status = gate(GateCheck::Failing, GateCheck::Unknown, GateCheck::Unknown)), WaitingOn::CiFailed);
        assert_eq!(s(&|s| s.gate_status = gate(GateCheck::Pending, GateCheck::Unknown, GateCheck::Unknown)), WaitingOn::CiRunning);
        assert_eq!(s(&|s| s.status = SessionStatus::Mergeable), WaitingOn::Mergeable);
        assert_eq!(s(&|s| s.gate_status = gate(GateCheck::Passing, GateCheck::Passing, GateCheck::Passing)), WaitingOn::Mergeable);
        assert_eq!(s(&|s| s.gate_status = gate(GateCheck::Passing, GateCheck::Pending, GateCheck::Passing)), WaitingOn::ReviewPending);
        assert_eq!(s(&|s| s.status = SessionStatus::AwaitingReview), WaitingOn::OrchestratorReview);
        assert_eq!(s(&|_| {}), WaitingOn::PrOpen);
        let mut bare = session("w", Some("o"));
        bare.status = SessionStatus::Working;
        bare.pr_number = None;
        assert_eq!(waiting_on(&bare, 0), WaitingOn::Unknown);
        assert_eq!(waiting_on(&bare, 2), WaitingOn::QueuedMessages { count: 2 });
        bare.activity_note = Some("need an API key".into());
        bare.activity = ActivityState::Blocked;
        assert_eq!(waiting_on(&bare, 0).label(), "blocked: need an API key");
    }

    #[test]
    fn groups_by_orchestrator_with_totals_from_the_cache() {
        let sessions = [
            session("a1", Some("o1")),
            Session { activity_since: Some(NOW - 90 * MIN), ..session("a2", Some("o1")) },
            session("b1", Some("o2")),
            session("loose", None),
        ];
        let disk: HashMap<String, DiskMeasurement> = [
            ("a1".to_string(), DiskMeasurement { total_bytes: 3_000_000_000, build_bytes: 2_000_000_000, measured_at: NOW - 2 * MIN }),
            ("b1".to_string(), DiskMeasurement { total_bytes: 1_000_000_000, build_bytes: 500_000_000, measured_at: NOW }),
        ]
        .into();
        let r = classify(&sessions, &HashSet::new(), &IdleWorkersConfig::default(), &HashMap::new(), &disk, &HashMap::new(), NOW);
        assert_eq!(r.total, 4);
        assert_eq!(r.orchestrators_to_prompt(), 2);
        assert_eq!(r.total_bytes, 4_000_000_000);
        assert_eq!(r.build_bytes, 2_500_000_000);
        let o1 = r.groups.iter().find(|g| g.orchestrator_id.as_deref() == Some("o1")).unwrap();
        assert_eq!(o1.workers[0].id, "a2", "longest idle first");
        assert_eq!(o1.workers[1].disk_age_ms(NOW), Some(2 * MIN));
        assert_eq!(o1.workers[0].disk, None);
    }

    #[test]
    fn prompt_text_lists_each_worker_with_idle_time_wait_and_disk() {
        let sessions = [
            Session { merged_at: Some(NOW - 2 * MIN), activity: ActivityState::Working, ..session("w-merged", Some("o1")) },
            session("w-open", Some("o1")),
        ];
        let disk = [("w-open".to_string(), DiskMeasurement { total_bytes: 2_500_000_000, build_bytes: 2_000_000_000, measured_at: NOW - 4 * MIN })].into();
        let r = classify(&sessions, &HashSet::new(), &IdleWorkersConfig::default(), &HashMap::new(), &disk, &HashMap::new(), NOW);
        let text = prompt_text(&r.groups[0], NOW);
        assert!(text.starts_with("2 of your workers are idle"));
        assert!(text.contains("- w-merged idle 2m · merged, not completed · not measured yet"), "{text}");
        assert!(text.contains("- w-open idle 45m · PR awaiting review · 2.5 GB (2.0 GB build output, measured 4m ago)"), "{text}");
        assert!(text.contains("`ninox complete`"));
        assert!(!text.contains("{{"));
    }

    fn receipt(to: &str) -> DeliveryReceipt {
        DeliveryReceipt { message_id: Some(1), to: to.into(), mechanism: Some("test"), status: DeliveryStatus::Delivered }
    }

    #[tokio::test]
    async fn two_prompts_inside_the_cooldown_send_one_message_per_orchestrator() {
        let _iso = crate::test_isolation::isolate();
        let store = Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap();
        let sessions = [session("a", Some("o1")), session("b", Some("o2"))];
        let r = report(&sessions, &[]);
        let cfg = IdleWorkersConfig::default();
        let sent = Arc::new(AtomicUsize::new(0));
        let send = |to: String, _text: String| {
            let sent = sent.clone();
            async move {
                sent.fetch_add(1, Ordering::SeqCst);
                Ok(receipt(&to))
            }
        };
        let first = prompt_orchestrators(&store, &r, None, &cfg, NOW, &send).await;
        assert!(first.iter().all(|o| matches!(o, PromptOutcome::Sent { .. })));
        let second = prompt_orchestrators(&store, &r, None, &cfg, NOW + 12 * MIN, &send).await;
        assert!(second.iter().all(|o| matches!(o, PromptOutcome::RateLimited { prompted_ago_ms, .. } if *prompted_ago_ms == 12 * MIN)));
        assert_eq!(sent.load(Ordering::SeqCst), 2);

        let after = prompt_orchestrators(&store, &r, Some("o1"), &cfg, NOW + 31 * MIN, &send).await;
        assert_eq!(after.len(), 1);
        assert!(matches!(after[0], PromptOutcome::Sent { .. }));
        assert_eq!(sent.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_changed_worker_set_is_prompted_again_and_a_failed_send_is_retryable() {
        let _iso = crate::test_isolation::isolate();
        let store = Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap();
        let cfg = IdleWorkersConfig::default();
        let one = report(&[session("a", Some("o1"))], &[]);
        let two = report(&[session("a", Some("o1")), session("b", Some("o1"))], &[]);
        let ok = |to: String, _t: String| async move { Ok(receipt(&to)) };
        let fail = |_to: String, _t: String| async move { Err(anyhow::anyhow!("boom")) };

        assert!(matches!(prompt_orchestrators(&store, &one, None, &cfg, NOW, &fail).await[0], PromptOutcome::Failed { .. }));
        assert!(matches!(prompt_orchestrators(&store, &one, None, &cfg, NOW + MIN, &ok).await[0], PromptOutcome::Sent { .. }));
        assert!(matches!(prompt_orchestrators(&store, &one, None, &cfg, NOW + 2 * MIN, &ok).await[0], PromptOutcome::RateLimited { .. }));
        assert!(matches!(prompt_orchestrators(&store, &two, None, &cfg, NOW + 3 * MIN, &ok).await[0], PromptOutcome::Sent { .. }));
    }

    #[tokio::test]
    async fn workers_without_an_orchestrator_are_never_prompted() {
        let _iso = crate::test_isolation::isolate();
        let store = Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap();
        let r = report(&[session("loose", None)], &[]);
        let ok = |to: String, _t: String| async move { Ok(receipt(&to)) };
        assert!(prompt_orchestrators(&store, &r, None, &IdleWorkersConfig::default(), NOW, ok).await.is_empty());
    }

    #[test]
    fn report_reads_cached_disk_and_prompt_state_from_the_store() {
        let store = Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap();
        store.upsert_orchestrator(&Orchestrator { id: "o1".into(), name: "o1".into(), created_at: 0 }).unwrap();
        store.upsert_session(&session("o1", None)).unwrap();
        store.upsert_session(&session("w1", Some("o1"))).unwrap();
        store.put_disk_measurement("w1", DiskMeasurement { total_bytes: 7, build_bytes: 3, measured_at: NOW - MIN }).unwrap();
        assert!(store.claim_idle_prompt("o1", "w1", NOW - 5 * MIN, 30 * MIN).unwrap());
        let r = load_report(&store, &IdleWorkersConfig::default(), NOW).unwrap();
        assert_eq!(ids(&r), ["w1"]);
        assert_eq!(r.groups[0].workers[0].disk.unwrap().build_bytes, 3);
        assert_eq!(r.groups[0].prompted_at, Some(NOW - 5 * MIN));
        assert_eq!((r.total_bytes, r.build_bytes), (7, 3));
    }

    #[test]
    fn disk_measurements_are_dropped_once_a_worker_is_no_longer_idle() {
        let store = Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap();
        let m = DiskMeasurement { total_bytes: 1, build_bytes: 1, measured_at: NOW };
        store.put_disk_measurement("gone", m).unwrap();
        store.put_disk_measurement("kept", m).unwrap();
        measure_idle_disks(&store, &report(&[session("kept", Some("o"))], &[]).workers().cloned().collect::<Vec<_>>(), &[], NOW);
        let left = store.disk_measurements().unwrap();
        assert!(left.contains_key("kept") && !left.contains_key("gone"));
    }

    fn git(dir: &Path, args: &[&str]) {
        let st = std::process::Command::new("git").args(args).current_dir(dir).status().unwrap();
        assert!(st.success(), "git {args:?}");
    }

    #[test]
    fn clean_removes_only_build_dirs_of_a_live_worker_and_remeasures() {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(repo.path().join(".gitignore"), "target/\n.claude/\n").unwrap();
        std::fs::write(repo.path().join("lib.rs"), "fn a() {}\n").unwrap();
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "-q", "-m", "init"]);
        let wt = repo.path().join(".claude/worktrees/w1");
        git(repo.path(), &["worktree", "add", "-q", "-b", "w1", wt.to_str().unwrap()]);
        std::fs::create_dir_all(wt.join("target/debug")).unwrap();
        std::fs::write(wt.join("target/debug/blob"), vec![1u8; 64 * 1024]).unwrap();
        std::fs::write(wt.join("lib.rs"), "fn dirty() {}\n").unwrap();

        let store = Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap();
        let live = Session { workspace_path: Some(wt.to_string_lossy().into()), status: SessionStatus::Working, activity: ActivityState::Working, ..session("w1", Some("o")) };
        store.upsert_session(&live).unwrap();

        let busy = clean_idle_build_output(&store, "w1", &["target".into()], NOW).unwrap_err().to_string();
        assert!(busy.contains("not idle right now") && wt.join("target").exists(), "{busy}");
        let live = Session { activity: ActivityState::Idle, activity_since: Some(NOW - 45 * MIN), ..live };
        store.upsert_session(&live).unwrap();
        let sharer = Session { id: "w2".into(), workspace_path: live.workspace_path.clone(), status: SessionStatus::Working, ..session("w2", Some("o")) };
        store.upsert_session(&sharer).unwrap();
        let shared = clean_idle_build_output(&store, "w1", &["target".into()], NOW).unwrap_err().to_string();
        assert!(shared.contains("w2 also uses this worktree") && wt.join("target").exists(), "{shared}");
        store.delete_session("w2").unwrap();

        let freed = clean_idle_build_output(&store, "w1", &["target".into()], NOW).unwrap();
        assert!(freed.bytes() >= 64 * 1024);
        assert!(!wt.join("target").exists());
        assert_eq!(std::fs::read_to_string(wt.join("lib.rs")).unwrap(), "fn dirty() {}\n", "tracked files untouched");
        let m = store.disk_measurements().unwrap()["w1"];
        assert_eq!((m.build_bytes, m.measured_at), (0, NOW));

        assert!(clean_idle_build_output(&store, "nope", &["target".into()], NOW).is_err());
        store.upsert_orchestrator(&Orchestrator { id: "w1".into(), name: "w1".into(), created_at: 0 }).unwrap();
        assert!(clean_idle_build_output(&store, "w1", &["target".into()], NOW).unwrap_err().to_string().contains("orchestrator"));
    }

    #[test]
    fn outcome_text_never_echoes_the_message_or_transport_error() {
        let long = "x".repeat(500);
        let sent = |status| PromptOutcome::Sent {
            orchestrator_id: "o1".into(), workers: 2,
            receipt: DeliveryReceipt { message_id: Some(9), to: "o1".into(), mechanism: Some("tmux"), status },
        };
        assert_eq!(sent(DeliveryStatus::Delivered).describe(), "o1: prompted about 2 worker(s) — delivered via tmux (message 9)");
        let held = sent(DeliveryStatus::Unreachable { reason: long.clone() }).describe();
        assert!(held.contains("target unreachable") && !held.contains(&long), "{held}");
        let queued = sent(DeliveryStatus::Queued { reason: long.clone() }).describe();
        assert!(!queued.contains(&long), "{queued}");
        assert_eq!(PromptOutcome::RateLimited { orchestrator_id: "o1".into(), prompted_ago_ms: 12 * MIN }.describe(), "o1: prompted 12m ago, skipped");
    }

    #[test]
    fn the_prompted_marker_expires_with_the_cooldown() {
        let sessions = [session("a", Some("o1"))];
        let rec = |t| [("o1".to_string(), crate::store::IdlePromptRecord { worker_set: "a".into(), prompted_at: t })].into();
        let cfg = IdleWorkersConfig::default();
        let at = |t, now| classify(&sessions, &HashSet::new(), &cfg, &HashMap::new(), &HashMap::new(), &rec(t), now).groups[0].prompted_at;
        assert_eq!(at(NOW - 29 * MIN, NOW), Some(NOW - 29 * MIN));
        assert_eq!(at(NOW - 31 * MIN, NOW), None, "past the cooldown the group can be prompted again");
    }

    #[test]
    fn a_measurement_pass_is_capped_and_deleted_sessions_lose_their_cache() {
        let store = Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap();
        let root = tempfile::tempdir().unwrap();
        let workers: Vec<IdleWorker> = (0..5)
            .map(|i| {
                let ws = root.path().join(format!(".claude/worktrees/w{i}"));
                std::fs::create_dir_all(&ws).unwrap();
                let mut w = report(&[session(&format!("w{i}"), Some("o"))], &[]).workers().next().unwrap().clone();
                w.workspace_path = Some(ws.to_string_lossy().into());
                w
            })
            .collect();
        measure_idle_disks(&store, &workers, &[], NOW);
        assert_eq!(store.disk_measurements().unwrap().len(), MAX_MEASURED_PER_TICK);
        measure_idle_disks(&store, &workers, &[], NOW + MIN);
        assert_eq!(store.disk_measurements().unwrap().len(), 5, "the rest follow on the next tick");
        measure_idle_disks(&store, &[], &[], NOW + 2 * MIN);
        assert!(store.disk_measurements().unwrap().is_empty(), "with nothing idle the cache is pruned");
        store.put_disk_measurement("w0", DiskMeasurement { total_bytes: 1, build_bytes: 0, measured_at: NOW }).unwrap();
        store.upsert_session(&session("w0", Some("o"))).unwrap();
        store.delete_session("w0").unwrap();
        assert!(store.disk_measurements().unwrap().is_empty());
    }

    #[test]
    fn a_fresh_measurement_is_kept_until_it_ages_out_or_the_worktree_root_changes() {
        let dir = tempfile::tempdir().unwrap();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
        assert!(!needs_remeasure(now + 1000, now + 2000, dir.path()), "root untouched since the measurement");
        assert!(needs_remeasure(now - 60 * MIN, now, dir.path()), "aged out");
        assert!(needs_remeasure(0, 1, dir.path()), "root modified after the measurement");
    }

    #[test]
    fn promptable_excludes_groups_prompted_inside_the_cooldown_and_unowned_workers() {
        let sessions = [session("a", Some("o1")), session("b", Some("o2")), session("loose", None)];
        let rec = [("o1".to_string(), crate::store::IdlePromptRecord { worker_set: "a".into(), prompted_at: NOW - 5 * MIN })].into();
        let r = classify(&sessions, &HashSet::new(), &IdleWorkersConfig::default(), &HashMap::new(), &HashMap::new(), &rec, NOW);
        let got: Vec<_> = r.promptable().map(|g| g.orchestrator_id.clone().unwrap()).collect();
        assert_eq!(got, ["o2"]);
    }

    #[test]
    fn can_clean_needs_an_idle_worker_with_measured_build_output() {
        let disk = |b| DiskMeasurement { total_bytes: 10, build_bytes: b, measured_at: NOW };
        let sessions = [Session { merged_at: Some(NOW - MIN), activity: ActivityState::Working, ..session("merged-busy", Some("o")) }, session("idle", Some("o"))];
        let cache = [("merged-busy".to_string(), disk(5)), ("idle".to_string(), disk(5))].into();
        let r = classify(&sessions, &HashSet::new(), &IdleWorkersConfig::default(), &HashMap::new(), &cache, &HashMap::new(), NOW);
        let can = |id: &str| r.workers().find(|w| w.id == id).unwrap().can_clean();
        assert!(!can("merged-busy"), "merged but mid-turn: it may be compiling");
        assert!(can("idle"));
    }

    #[test]
    fn deleting_an_orchestrator_drops_its_idle_bookkeeping() {
        let store = Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap();
        store.upsert_orchestrator(&Orchestrator { id: "o1".into(), name: "o1".into(), created_at: 0 }).unwrap();
        store.upsert_session(&session("o1", None)).unwrap();
        store.upsert_session(&session("w1", Some("o1"))).unwrap();
        store.put_disk_measurement("w1", DiskMeasurement { total_bytes: 1, build_bytes: 0, measured_at: NOW }).unwrap();
        assert!(store.claim_idle_prompt("o1", "w1", NOW, 30 * MIN).unwrap());
        store.delete_orchestrator("o1").unwrap();
        assert!(store.disk_measurements().unwrap().is_empty());
        assert!(store.idle_prompt_records().unwrap().is_empty());
    }

    #[test]
    fn only_one_measurement_pass_runs_at_a_time() {
        let first = MeasureGuard::try_begin().expect("free");
        assert!(MeasureGuard::try_begin().is_none());
        drop(first);
        assert!(MeasureGuard::try_begin().is_some());
    }

    #[test]
    fn durations_and_sizes_are_compact() {
        assert_eq!(format_duration(30_000), "<1m");
        assert_eq!(format_duration(12 * MIN), "12m");
        assert_eq!(format_duration(125 * MIN), "2h05m");
        assert_eq!(format_duration(50 * 60 * MIN), "2d2h");
        assert_eq!(format_bytes(1_500_000_000), "1.5 GB");
    }
}
