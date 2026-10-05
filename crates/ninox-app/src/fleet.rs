//! `ninox fleet …` — ordered fleet restore execution (spec §5.3) on top of
//! the pure planning/briefing in `ninox_core::fleet`.
//!
//! Restore: validate → relaunch workers (the app's own Resume path,
//! `spawn_util::relaunch_in_place`) → wait for their input prompts → brief
//! them → relaunch orchestrators → deliver each orchestrator's recovery
//! briefing as its first input → nudge sessions with undelivered inbox
//! messages. Safe to re-run: every attempt is recorded per session, so a
//! second run only retries failures and delivers briefings a crashed run
//! never sent. A store lease keeps two restores from racing.

use crate::spawn_util::{relaunch_in_place, RelaunchRequest};
use clap::Subcommand;
use ninox_core::{
    config::AppConfig,
    events::Engine,
    fleet::{
        self, briefing, plan_restore, project_outcomes, FleetMember, FleetSnapshot, GitProbe,
        RestoreMode, RestoreOutcome, RestorePlan, Role,
    },
    store::Store,
};
use std::{collections::BTreeSet, sync::Arc, time::Duration};

#[derive(Subcommand)]
pub enum FleetAction {
    /// Show interrupted sessions, workspace anomalies, pending request-work
    /// items and recovery state
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Restore interrupted sessions: workers first, then orchestrators,
    /// each orchestrator receiving a recovery briefing as its first input
    Restore {
        /// Print the plan and the briefings it would send; change nothing
        #[arg(long)]
        dry_run: bool,
        /// Don't ask for confirmation
        #[arg(long)]
        yes: bool,
        /// Restrict the restore to these session ids
        #[arg(long, num_args = 1..)]
        only: Vec<String>,
    },
    /// Mark your recovery complete once you have taken stock of the
    /// briefing (used by orchestrator agents)
    Ack {
        /// Orchestrator id (read from NINOX_ORCHESTRATOR_ID if not supplied)
        #[arg(long)]
        orchestrator_id: Option<String>,
    },
    /// Print the recovery briefing a session would receive, without acting
    Brief {
        /// Orchestrator (or worker) session id
        session_id: String,
    },
}

/// Hot-path verbs (agent-invoked) that need only the store; `main.rs` runs
/// these before the tmux-config/wrapper setup.
pub fn is_lightweight(action: &FleetAction) -> bool {
    !matches!(action, FleetAction::Restore { dry_run: false, .. })
}

pub async fn run_cli(action: FleetAction, store: Arc<Store>, config: AppConfig) -> anyhow::Result<()> {
    match action {
        FleetAction::Status { json } => println!("{}", status(&store, &config, json)?),
        FleetAction::Ack { orchestrator_id } => {
            let id = orchestrator_id
                .or_else(|| env_nonempty("NINOX_ORCHESTRATOR_ID"))
                .or_else(|| env_nonempty("NINOX_SESSION"))
                .ok_or_else(|| anyhow::anyhow!(
                    "no orchestrator id — pass --orchestrator-id or run inside an orchestrator session"
                ))?;
            println!("{}", ack(&store, &id, now_ms())?);
        }
        FleetAction::Brief { session_id } => println!("{}", brief(&store, &config, &session_id)?),
        FleetAction::Restore { dry_run, yes, only } => {
            let opts = RestoreOptions { dry_run, only, prompt_timeout: DEFAULT_PROMPT_TIMEOUT };
            let preview = restore(store.clone(), &config, &RestoreOptions { dry_run: true, ..opts.clone() }).await?;
            print!("{}", render_plan(&preview.plan));
            if dry_run {
                for (id, text) in &preview.briefings {
                    println!("\n── briefing for {id} ──\n{text}");
                }
                return Ok(());
            }
            if preview.plan.is_empty() {
                return Ok(());
            }
            if !yes && !confirm("Proceed with the restore?")? {
                println!("aborted");
                return Ok(());
            }
            let report = restore(store, &config, &opts).await?;
            print!("{}", render_report(&report));
        }
    }
    Ok(())
}

pub(crate) fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn confirm(question: &str) -> anyhow::Result<bool> {
    use std::io::{BufRead, IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        anyhow::bail!("not a terminal — pass --yes to restore without confirmation");
    }
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes"))
}

fn snapshot(store: &Store, config: &AppConfig) -> anyhow::Result<FleetSnapshot> {
    FleetSnapshot::load(store, &config.registry(), &GitProbe, &AppConfig::sessions_dir(), now_ms())
}

// ── Process control ─────────────────────────────────────────────────────────
// Every pane-level call restore makes, in one place.
mod backend {
    use std::time::Duration;

    pub async fn is_live(id: &str) -> bool {
        ninox_core::runtime::has_session(id).await
    }

    pub async fn wait_for_prompt(id: &str, timeout: Duration) -> bool {
        ninox_core::runtime::wait_for_input_prompt(id, timeout).await
    }

    pub async fn wake(id: &str) {
        if let Err(e) = ninox_core::runtime::wake_idle_session(id).await {
            tracing::warn!("fleet: wake {id}: {e}");
        }
    }
}

// ── status / ack / brief ────────────────────────────────────────────────────

pub fn status(store: &Store, config: &AppConfig, json: bool) -> anyhow::Result<String> {
    let snap = snapshot(store, config)?;
    let plan = plan_restore(&snap, &[]);
    if json {
        return Ok(serde_json::to_string_pretty(&serde_json::json!({
            "restore_policy":  config.fleet.restore_policy,
            "pending_restore": fleet::pending_restore(store),
            "restore_running": store.fleet_lock_holder(RESTORE_LOCK)?,
            "plan":            plan,
            "snapshot":        snap,
        }))?);
    }
    Ok(render_status(&snap, &plan, config))
}

fn render_status(snap: &FleetSnapshot, plan: &RestorePlan, config: &AppConfig) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let policy = serde_json::to_string(&config.fleet.restore_policy).unwrap_or_default().replace('"', "");
    let _ = writeln!(out, "restore policy: {policy}");
    let groups: Vec<&FleetMember> = snap.members.iter().filter(|m| m.role != Role::Worker).collect();
    for head in groups {
        let _ = writeln!(out, "{}", member_status_line(head, ""));
        if head.role == Role::Orchestrator {
            for w in snap.workers_of(head.id()) {
                let _ = writeln!(out, "{}", member_status_line(w, "  "));
            }
            if let Some(r) = snap.recovery(head.id()) {
                let state = match (r.briefing_sent_at, r.acked_at) {
                    (_, Some(_))    => "acknowledged",
                    (Some(_), None) => "briefed, awaiting `ninox fleet ack`",
                    (None, None)    => "briefing not yet delivered",
                };
                let _ = writeln!(out, "  recovery: {state}");
            }
            let reqs: Vec<_> = snap.requests_for(head.id()).collect();
            if !reqs.is_empty() {
                let _ = writeln!(out, "  open request-work items: {}", reqs.len());
            }
        }
    }
    let orphans: Vec<&FleetMember> = snap.members.iter()
        .filter(|m| m.role == Role::Worker)
        .filter(|m| m.session.orchestrator_id.as_deref().and_then(|o| snap.member(o)).is_none())
        .collect();
    for w in orphans {
        let _ = writeln!(out, "{}", member_status_line(w, ""));
    }
    out.push('\n');
    out.push_str(&render_plan(plan));
    out
}

fn member_status_line(m: &FleetMember, indent: &str) -> String {
    let status = serde_json::to_string(&m.session.status).unwrap_or_default().replace('"', "");
    let role = match m.role {
        Role::Orchestrator => "orchestrator",
        Role::Worker => "worker",
        Role::Standalone => "standalone",
    };
    let mut line = format!("{indent}{} [{role}] {status}", m.id());
    if m.awaiting_restore() {
        line.push_str(if m.can_resume { " — restorable (resume)" } else { " — restorable (fresh restart)" });
    }
    if m.workspace.dirty == Some(true) {
        line.push_str(" — uncommitted changes");
    }
    if m.pending_inbox > 0 {
        line.push_str(&format!(" — {} undelivered inbox", m.pending_inbox));
    }
    for a in &m.anomalies {
        line.push_str(&format!("\n{indent}    ! {}", a.describe()));
    }
    line
}

pub fn render_plan(plan: &RestorePlan) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    if plan.is_empty() && plan.skipped.is_empty() {
        out.push_str("nothing to restore\n");
        return out;
    }
    let mode = |m: RestoreMode| match m {
        RestoreMode::Resume => "resume",
        RestoreMode::Fresh => "fresh restart + briefing",
    };
    let mut n = 0;
    for step in plan.steps() {
        n += 1;
        let _ = writeln!(out, "{n}. {} ({}) — {}", step.session_id, step.name, mode(step.mode));
    }
    for id in &plan.pending_briefings {
        let _ = writeln!(out, "- deliver undelivered recovery briefing to {id}");
    }
    for s in &plan.skipped {
        let why: Vec<String> = s.anomalies.iter().map(|a| a.describe()).collect();
        let _ = writeln!(out, "skip {}: {}", s.session_id, why.join("; "));
    }
    out
}

pub fn ack(store: &Store, orchestrator_id: &str, now: i64) -> anyhow::Result<String> {
    Ok(match fleet::ack(store, orchestrator_id, now)? {
        fleet::AckOutcome::NoRecovery => format!("no recovery in progress for {orchestrator_id}"),
        fleet::AckOutcome::Acked { resolved_requests } => format!(
            "recovery of {orchestrator_id} acknowledged ({resolved_requests} delivered request-work item(s) resolved)"
        ),
    })
}

/// The briefing as the store currently implies it; for a session that
/// still awaits restore, as it would read after a successful restore.
pub fn brief(store: &Store, config: &AppConfig, session_id: &str) -> anyhow::Result<String> {
    let snap = snapshot(store, config)?;
    let plan = plan_restore(&snap, &[]);
    let snap = project_outcomes(&snap, &plan, now_ms());
    let member = snap.member(session_id).ok_or_else(|| anyhow::anyhow!("no session {session_id}"))?;
    let text = if member.role == Role::Orchestrator {
        briefing::orchestrator_briefing(&snap, session_id, &fleet::format_local)
    } else {
        briefing::worker_briefing(&snap, session_id, &fleet::format_local)
    };
    text.ok_or_else(|| anyhow::anyhow!("{session_id} has no recovery briefing (it was not interrupted)"))
}

// ── restore ─────────────────────────────────────────────────────────────────

pub(crate) const RESTORE_LOCK: &str = "restore";
/// Arbitrary; bump if a slow machine's restores legitimately take longer.
const RESTORE_LEASE_MS: i64 = 30 * 60 * 1000;
/// Arbitrary; a cold harness start (auth, MCP servers) can be slow.
pub const DEFAULT_PROMPT_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Clone)]
pub struct RestoreOptions {
    pub dry_run:        bool,
    pub only:           Vec<String>,
    pub prompt_timeout: Duration,
}

#[derive(Default)]
pub struct RestoreReport {
    pub plan:      RestorePlan,
    pub outcomes:  Vec<(String, RestoreOutcome, Option<String>)>,
    /// `(session id, briefing text)` — sent, or (dry run) would be sent.
    pub briefings: Vec<(String, String)>,
    pub briefing_failures: Vec<(String, String)>,
    pub nudged:    Vec<String>,
}

struct RestoreLease<'a> {
    store:  &'a Store,
    holder: String,
}

fn lease_holder_prefix() -> String {
    format!("pid-{}-", std::process::id())
}

/// For exit paths that skip destructors: drop any restore lease this
/// process holds, so the next restore isn't refused until it goes stale.
pub fn release_own_restore_lease(store: &Store) {
    if let Ok(Some((holder, _))) = store.fleet_lock_holder(RESTORE_LOCK) {
        if holder.starts_with(&lease_holder_prefix()) {
            let _ = store.release_fleet_lock(RESTORE_LOCK, &holder);
        }
    }
}

impl<'a> RestoreLease<'a> {
    fn acquire(store: &'a Store) -> anyhow::Result<Self> {
        let now = now_ms();
        let holder = format!("{}{now}", lease_holder_prefix());
        if !store.try_acquire_fleet_lock(RESTORE_LOCK, &holder, now, now - RESTORE_LEASE_MS)? {
            let held = store.fleet_lock_holder(RESTORE_LOCK)?
                .map(|(h, at)| format!(" ({h}, since {})", fleet::format_local(at)))
                .unwrap_or_default();
            anyhow::bail!("another fleet restore is already running{held}");
        }
        Ok(Self { store, holder })
    }
}

impl Drop for RestoreLease<'_> {
    fn drop(&mut self) {
        let _ = self.store.release_fleet_lock(RESTORE_LOCK, &self.holder);
    }
}

pub async fn restore(store: Arc<Store>, config: &AppConfig, opts: &RestoreOptions) -> anyhow::Result<RestoreReport> {
    if opts.dry_run {
        let snap = snapshot(&store, config)?;
        let plan = plan_restore(&snap, &opts.only);
        let projected = project_outcomes(&snap, &plan, now_ms());
        let briefings = briefing_targets(&plan, &projected)
            .into_iter()
            .chain(plan.workers.iter().map(|s| s.session_id.clone()))
            .filter_map(|id| briefing_for(&projected, &id).map(|t| (id, t)))
            .collect();
        return Ok(RestoreReport { plan, briefings, ..Default::default() });
    }

    let _lease = RestoreLease::acquire(&store)?;
    let snap = snapshot(&store, config)?;
    let plan = plan_restore(&snap, &opts.only);
    let mut report = RestoreReport { plan: plan.clone(), ..Default::default() };
    let engine = Engine::new(store.clone());

    // Workers first, so the fleet is live before its orchestrator wakes.
    let mut live_workers = Vec::new();
    for step in &plan.workers {
        let Some(m) = snap.member(&step.session_id) else { continue };
        let (outcome, note) = relaunch(&engine, config, m, step.mode).await;
        record(&store, &mut report, &step.session_id, outcome, note);
        if outcome != RestoreOutcome::Failed {
            live_workers.push(step.session_id.clone());
        }
    }
    wait_for_prompts(&live_workers, opts.prompt_timeout).await;

    let snap = snapshot(&store, config)?;
    let mut briefed = BTreeSet::new();
    for id in &live_workers {
        deliver_briefing(&store, config, &snap, id, &mut report, &mut briefed).await;
    }

    let mut live_orchs = Vec::new();
    for step in &plan.orchestrators {
        let Some(m) = snap.member(&step.session_id) else { continue };
        let (outcome, note) = relaunch(&engine, config, m, step.mode).await;
        record(&store, &mut report, &step.session_id, outcome, note);
        if outcome != RestoreOutcome::Failed {
            live_orchs.push(step.session_id.clone());
        }
    }
    wait_for_prompts(&live_orchs, opts.prompt_timeout).await;

    let snap = snapshot(&store, config)?;
    let now = now_ms();
    for orch in briefing_targets(&plan, &snap) {
        if !plan.pending_briefings.contains(&orch) {
            store.begin_recovery(&orch, snap.interruption_of(&orch).map(|(t, _)| t), now)?;
        }
        if deliver_briefing(&store, config, &snap, &orch, &mut report, &mut briefed).await {
            let sent_at = now_ms();
            store.mark_briefing_sent(&orch, sent_at)?;
            store.mark_listed_work_requests_delivered(&orch, snap.taken_at, sent_at)?;
        }
    }

    // Inbox redelivery: inbox files are only ever written for sessions
    // whose hooks drain them, so a briefed session drains at the end of its
    // first turn; anything not briefed needs an explicit nudge.
    for id in live_workers.iter().chain(&live_orchs) {
        let pending = snap.member(id).is_some_and(|m| m.pending_inbox > 0);
        if pending && !briefed.contains(id) {
            backend::wake(id).await;
            report.nudged.push(id.clone());
        }
    }

    if fleet::startup::restore_summary(&store).ok().flatten().is_none() {
        fleet::dismiss_pending_restore(&store);
    }
    Ok(report)
}

/// Orchestrators that get a briefing: restored ones, live ones owning a
/// worker this run restored, and live ones a crashed run never briefed.
fn briefing_targets(plan: &RestorePlan, snap: &FleetSnapshot) -> Vec<String> {
    let mut targets: BTreeSet<String> = plan.orchestrators.iter().map(|s| s.session_id.clone()).collect();
    targets.extend(plan.pending_briefings.iter().cloned());
    for step in &plan.workers {
        if let Some(o) = snap.member(&step.session_id).and_then(|m| m.session.orchestrator_id.clone()) {
            if snap.member(&o).is_some_and(|m| m.role == Role::Orchestrator && !m.session.status.is_terminal()) {
                targets.insert(o);
            }
        }
    }
    // Only orchestrators that are actually live can take input.
    targets.into_iter()
        .filter(|o| snap.member(o).is_some_and(|m| !m.session.status.is_terminal()))
        .collect()
}

fn briefing_for(snap: &FleetSnapshot, id: &str) -> Option<String> {
    match snap.member(id)?.role {
        Role::Orchestrator => briefing::orchestrator_briefing(snap, id, &fleet::format_local),
        Role::Worker | Role::Standalone => briefing::worker_briefing(snap, id, &fleet::format_local),
    }
}

async fn deliver_briefing(
    store:   &Store,
    config:  &AppConfig,
    snap:    &FleetSnapshot,
    id:      &str,
    report:  &mut RestoreReport,
    briefed: &mut BTreeSet<String>,
) -> bool {
    let Some(text) = briefing_for(snap, id) else { return false };
    let sent = ninox_core::messaging::deliver_message(
        store, &AppConfig::sessions_dir(), id, &text, config.send_mechanism(),
    )
    .await;
    match sent {
        Ok(()) => {
            briefed.insert(id.to_string());
            report.briefings.push((id.to_string(), text));
            true
        }
        Err(e) => {
            tracing::warn!("fleet: briefing {id}: {e}");
            report.briefing_failures.push((id.to_string(), e.to_string()));
            false
        }
    }
}

fn record(store: &Store, report: &mut RestoreReport, id: &str, outcome: RestoreOutcome, note: Option<String>) {
    if let Err(e) = store.record_restore(id, now_ms(), outcome.as_str(), note.as_deref()) {
        tracing::warn!("fleet: record restore of {id}: {e}");
    }
    report.outcomes.push((id.to_string(), outcome, note));
}

async fn wait_for_prompts(ids: &[String], timeout: Duration) {
    let waits = ids.iter().map(|id| async move {
        if !backend::wait_for_prompt(id, timeout).await {
            tracing::warn!("fleet: {id} showed no input prompt within {timeout:?}; briefing it anyway");
        }
    });
    futures_util::future::join_all(waits).await;
}

async fn relaunch(
    engine: &Arc<Engine>,
    config: &AppConfig,
    m:      &FleetMember,
    mode:   RestoreMode,
) -> (RestoreOutcome, Option<String>) {
    if backend::is_live(m.id()).await {
        return (RestoreOutcome::AlreadyLive, None);
    }
    let is_orch = m.role == Role::Orchestrator;
    let planned = match mode {
        RestoreMode::Resume => m.session.claude_session_id.clone().and_then(|csid| {
            crate::app::resume_plan(&m.session, is_orch, config).map(|p| (p, csid, RestoreOutcome::Resumed))
        }),
        RestoreMode::Fresh => {
            let csid = ninox_core::harness::new_claude_session_id();
            crate::app::refile_plan(&m.session, is_orch, config, &csid).map(|p| (p, csid, RestoreOutcome::Fresh))
        }
    };
    let Some((plan, claude_session_id, outcome)) = planned else {
        return (RestoreOutcome::Failed, Some("no workspace recorded or the harness cannot relaunch it".into()));
    };
    let launched = relaunch_in_place(
        engine.clone(),
        RelaunchRequest {
            session: m.session.clone(),
            is_orchestrator: is_orch,
            plan,
            claude_session_id,
            started_at: now_ms(),
            // Keep the row restorable (Interrupted, or the reconciled
            // Terminated of a fresh-restart candidate) so a re-run retries.
            failure_status: m.session.status.clone(),
        },
        config,
        false,
    )
    .await;
    match launched {
        Some(_) => (outcome, None),
        None => (RestoreOutcome::Failed, Some("launch failed (see the ninox log)".into())),
    }
}

fn render_report(r: &RestoreReport) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for (id, outcome, note) in &r.outcomes {
        let _ = writeln!(out, "{id}: {}{}", outcome.as_str(), note.as_ref().map(|n| format!(" — {n}")).unwrap_or_default());
    }
    for (id, _) in &r.briefings {
        let _ = writeln!(out, "briefed {id}");
    }
    for (id, e) in &r.briefing_failures {
        let _ = writeln!(out, "briefing {id} FAILED: {e} (re-run `ninox fleet restore` to retry)");
    }
    for id in &r.nudged {
        let _ = writeln!(out, "nudged {id} to drain its inbox");
    }
    out
}

/// What `[fleet] restore_policy = "auto"` runs from `Poller::start`.
pub fn auto_restorer(store: Arc<Store>) -> ninox_core::lifecycle::poller::FleetRestorer {
    Arc::new(move || {
        let store = store.clone();
        Box::pin(async move {
            let config = AppConfig::load().unwrap_or_default();
            let opts = RestoreOptions { dry_run: false, only: Vec::new(), prompt_timeout: DEFAULT_PROMPT_TIMEOUT };
            match restore(store, &config, &opts).await {
                Ok(report) => tracing::info!("fleet: auto-restore finished:\n{}", render_report(&report)),
                Err(e) => tracing::warn!("fleet: auto-restore: {e}"),
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::types::{Orchestrator, Session, SessionStatus};

    fn store() -> Arc<Store> {
        Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap())
    }

    fn session(id: &str, orch: Option<&str>, status: SessionStatus, ws: &str) -> Session {
        Session {
            id: id.into(), orchestrator_id: orch.map(str::to_string), name: id.into(),
            repo: String::new(), status, agent_type: "claude-code".into(), cost_usd: 0.0,
            started_at: 1, pr_number: None, pr_id: None, workspace_path: Some(ws.into()),
            pid: None, model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: Some(format!("uuid-{id}")), summary: Some(format!("task {id}")),
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
        }
    }

    fn seed(store: &Store, ws: &std::path::Path) {
        let ws = ws.to_str().unwrap();
        store.upsert_session(&session("fleet-test-o", None, SessionStatus::Interrupted, ws)).unwrap();
        store.upsert_orchestrator(&Orchestrator { id: "fleet-test-o".into(), name: "o".into(), created_at: 0 }).unwrap();
        store.upsert_session(&session("fleet-test-w", Some("fleet-test-o"), SessionStatus::Interrupted, ws)).unwrap();
        store.record_interruption("fleet-test-w", 10, &SessionStatus::Working, Some("reboot")).unwrap();
        store.record_interruption("fleet-test-o", 10, &SessionStatus::Working, Some("reboot")).unwrap();
    }

    #[tokio::test]
    async fn dry_run_plans_workers_first_and_previews_briefings_without_writing() {
        let st = store();
        let ws = tempfile::tempdir().unwrap();
        seed(&st, ws.path());
        let opts = RestoreOptions { dry_run: true, only: vec![], prompt_timeout: Duration::from_millis(1) };
        let report = restore(st.clone(), &AppConfig::default(), &opts).await.unwrap();
        let order: Vec<_> = report.plan.steps().map(|s| s.session_id.as_str()).collect();
        assert_eq!(order, ["fleet-test-w", "fleet-test-o"]);
        let orch_brief = &report.briefings.iter().find(|(id, _)| id == "fleet-test-o").unwrap().1;
        assert!(orch_brief.contains("`fleet-test-w` was resumed and is continuing"), "{orch_brief}");
        assert!(report.briefings.iter().any(|(id, _)| id == "fleet-test-w"));
        // Nothing recorded, no lease left behind.
        assert!(st.fleet_record("fleet-test-w").unwrap().unwrap().restored_at.is_none());
        assert!(st.fleet_lock_holder(RESTORE_LOCK).unwrap().is_none());
        assert_eq!(st.get_session("fleet-test-w").unwrap().unwrap().status, SessionStatus::Interrupted);
    }

    #[tokio::test]
    async fn concurrent_restore_is_refused_while_the_lease_is_held() {
        let st = store();
        let now = now_ms();
        assert!(st.try_acquire_fleet_lock(RESTORE_LOCK, "someone-else", now, 0).unwrap());
        let opts = RestoreOptions { dry_run: false, only: vec![], prompt_timeout: Duration::from_millis(1) };
        let err = restore(st.clone(), &AppConfig::default(), &opts).await.err().unwrap();
        assert!(err.to_string().contains("already running"), "{err}");
        assert_eq!(st.fleet_lock_holder(RESTORE_LOCK).unwrap().unwrap().0, "someone-else");
    }

    #[test]
    fn own_lease_is_released_by_hand_but_never_anothers() {
        let st = store();
        let lease = RestoreLease::acquire(&st).unwrap();
        std::mem::forget(lease); // as `process::exit` would
        release_own_restore_lease(&st);
        assert!(st.fleet_lock_holder(RESTORE_LOCK).unwrap().is_none());

        assert!(st.try_acquire_fleet_lock(RESTORE_LOCK, "pid-1-5", now_ms(), 0).unwrap());
        release_own_restore_lease(&st);
        assert!(st.fleet_lock_holder(RESTORE_LOCK).unwrap().is_some());
    }

    #[tokio::test]
    async fn real_restore_with_nothing_to_do_is_a_noop_and_releases_the_lease() {
        let st = store();
        let opts = RestoreOptions { dry_run: false, only: vec![], prompt_timeout: Duration::from_millis(1) };
        let report = restore(st.clone(), &AppConfig::default(), &opts).await.unwrap();
        assert!(report.plan.is_empty() && report.outcomes.is_empty());
        assert!(st.fleet_lock_holder(RESTORE_LOCK).unwrap().is_none());
    }

    #[test]
    fn brief_previews_an_unrestored_orchestrator_and_errors_for_unknown_ids() {
        let st = store();
        let ws = tempfile::tempdir().unwrap();
        seed(&st, ws.path());
        let text = brief(&st, &AppConfig::default(), "fleet-test-o").unwrap();
        assert!(text.starts_with(briefing::ORCHESTRATOR_HEADER));
        assert!(text.contains("ninox fleet ack"));
        assert!(brief(&st, &AppConfig::default(), "nope").is_err());
    }

    #[test]
    fn ack_messages() {
        let st = store();
        assert!(ack(&st, "o", 1).unwrap().contains("no recovery in progress"));
        st.begin_recovery("o", None, 1).unwrap();
        assert!(ack(&st, "o", 2).unwrap().contains("acknowledged"));
    }

    #[test]
    fn status_lists_restorable_sessions_and_the_plan() {
        let st = store();
        let ws = tempfile::tempdir().unwrap();
        seed(&st, ws.path());
        let text = status(&st, &AppConfig::default(), false).unwrap();
        assert!(text.contains("restore policy: manual"));
        assert!(text.contains("fleet-test-o [orchestrator] interrupted — restorable (resume)"), "{text}");
        assert!(text.contains("  fleet-test-w [worker] interrupted — restorable (resume)"), "{text}");
        assert!(text.contains("1. fleet-test-w"));
        let json: serde_json::Value = serde_json::from_str(&status(&st, &AppConfig::default(), true).unwrap()).unwrap();
        assert_eq!(json["restore_policy"], "manual");
        assert_eq!(json["plan"]["orchestrators"][0]["session_id"], "fleet-test-o");
    }

    /// End to end on the real tmux backend with a stand-in harness that
    /// draws a `❯` prompt and echoes input: workers before orchestrators,
    /// briefing delivered and recorded, and a re-run does nothing.
    #[tokio::test]
    async fn restore_relaunches_in_order_briefs_and_is_idempotent() {
        let st = store();
        let ws = tempfile::tempdir().unwrap();
        let w_id = format!("fleet-e2e-w-{}", std::process::id());
        let o_id = format!("fleet-e2e-o-{}", std::process::id());
        let mut config = AppConfig::default();
        config.harnesses.insert("fleet-fake".into(), ninox_core::harness::HarnessSpec {
            enabled: true,
            binary: Some("sh".into()),
            resume_args: vec!["-c".into(), "'printf \"❯ \\n\"; exec cat'".into(), "x".into(), "{session_id}".into()],
            ..Default::default()
        });
        config.orchestrator.harness = "fleet-fake".into();
        config.worker.harness = "fleet-fake".into();
        let wsp = ws.path().to_str().unwrap();
        let mut o = session(&o_id, None, SessionStatus::Interrupted, wsp);
        o.agent_type = "fleet-fake".into();
        let mut w = session(&w_id, Some(&o_id), SessionStatus::Interrupted, wsp);
        w.agent_type = "fleet-fake".into();
        st.upsert_session(&o).unwrap();
        st.upsert_orchestrator(&Orchestrator { id: o_id.clone(), name: "o".into(), created_at: 0 }).unwrap();
        st.upsert_session(&w).unwrap();
        st.record_interruption(&w_id, 10, &SessionStatus::Working, Some("reboot")).unwrap();
        st.record_interruption(&o_id, 10, &SessionStatus::Working, Some("reboot")).unwrap();

        let opts = RestoreOptions { dry_run: false, only: vec![], prompt_timeout: Duration::from_secs(10) };
        let report = restore(st.clone(), &config, &opts).await.unwrap();

        let live_w = backend::is_live(&w_id).await;
        let live_o = backend::is_live(&o_id).await;
        let _ = ninox_core::tmux::kill_session(&w_id).await;
        let _ = ninox_core::tmux::kill_session(&o_id).await;

        let outcomes: Vec<_> = report.outcomes.iter().map(|(id, o, _)| (id.clone(), *o)).collect();
        assert_eq!(outcomes, vec![(w_id.clone(), RestoreOutcome::Resumed), (o_id.clone(), RestoreOutcome::Resumed)]);
        assert!(live_w && live_o);
        assert!(report.briefing_failures.is_empty(), "{:?}", report.briefing_failures);
        let o_brief = &report.briefings.iter().find(|(id, _)| *id == o_id).expect("orchestrator briefed").1;
        assert!(o_brief.contains(&format!("`{w_id}` was resumed and is continuing")), "{o_brief}");
        assert!(st.recovery(&o_id).unwrap().unwrap().briefing_sent_at.is_some());
        assert_eq!(st.get_session(&w_id).unwrap().unwrap().status, SessionStatus::Working);

        // Re-run: everything was handled, so nothing is relaunched.
        let again = restore(st.clone(), &config, &RestoreOptions { prompt_timeout: Duration::from_millis(1), ..opts }).await.unwrap();
        assert!(again.outcomes.is_empty() && again.plan.is_empty(), "{:?}", again.plan);
    }

    #[test]
    fn lightweight_verbs() {
        assert!(is_lightweight(&FleetAction::Status { json: false }));
        assert!(is_lightweight(&FleetAction::Restore { dry_run: true, yes: false, only: vec![] }));
        assert!(!is_lightweight(&FleetAction::Restore { dry_run: false, yes: true, only: vec![] }));
    }
}
