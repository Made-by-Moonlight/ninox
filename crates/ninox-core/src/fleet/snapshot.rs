//! [`FleetSnapshot`]: everything restore and the briefings need, read from
//! the store (plus a workspace probe) in one pass.

use super::probe::{validate_workspace, Anomaly, WorkspaceObservation, WorkspaceProbe};
use super::RestoreOutcome;
use crate::harness::HarnessRegistry;
use crate::store::{FleetRecord, RecoveryRecord, Store, UserRequestRow, WorkRequestRow};
use crate::types::{CIStatus, Session, SessionStatus, PR};
use anyhow::Result;
use serde::Serialize;
use std::collections::HashSet;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Orchestrator,
    Worker,
    /// No orchestrator and not one itself (a spawn-modal standalone session).
    Standalone,
}

#[derive(Debug, Clone, Serialize)]
pub struct FleetMember {
    pub session:       Session,
    pub role:          Role,
    pub record:        FleetRecord,
    pub pr:            Option<PR>,
    pub ci:            Option<CIStatus>,
    pub pending_inbox: usize,
    /// The harness can `--resume` and a conversation id is recorded.
    pub can_resume:    bool,
    pub workspace:     WorkspaceObservation,
    /// Only computed for members awaiting restore.
    pub anomalies:     Vec<Anomaly>,
}

impl FleetMember {
    pub fn id(&self) -> &str {
        &self.session.id
    }

    pub fn awaiting_restore(&self) -> bool {
        awaits_restore(&self.session, &self.record)
    }

    pub fn blocking_anomalies(&self) -> impl Iterator<Item = &Anomaly> {
        self.anomalies.iter().filter(|a| a.is_blocking())
    }

    /// The lifecycle status before reconciliation rewrote it, when the
    /// current one is the reconciled `Interrupted`/`Terminated`.
    pub fn effective_status(&self) -> &SessionStatus {
        match (&self.session.status, &self.record.last_status) {
            (SessionStatus::Interrupted | SessionStatus::Terminated, Some(last)) => last,
            (s, _) => s,
        }
    }

    pub fn last_restore_outcome(&self) -> Option<RestoreOutcome> {
        self.record.restored_for_current_interruption()
            .then(|| self.record.restore_mode.as_deref().and_then(RestoreOutcome::parse))
            .flatten()
    }
}

/// Needs a restore: `Interrupted` (resumable), or still the `Terminated`
/// row reconciliation wrote for a harness that can't resume (restarts
/// fresh). A failed attempt stays eligible so re-running restore retries it.
pub fn awaits_restore(session: &Session, record: &FleetRecord) -> bool {
    let failed_last_time = record.restore_mode.as_deref() == Some(RestoreOutcome::Failed.as_str());
    match session.status {
        SessionStatus::Interrupted => true,
        SessionStatus::Terminated => {
            record.interrupted_at.is_some()
                && record.reconciled_terminal_at.is_some()
                && record.reconciled_terminal_at == session.terminal_at
                && (record.awaiting_restore() || failed_last_time)
        }
        _ => false,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FleetSnapshot {
    pub taken_at:           i64,
    pub members:            Vec<FleetMember>,
    pub open_requests:      Vec<WorkRequestRow>,
    pub open_user_requests: Vec<UserRequestRow>,
    pub recoveries:         Vec<RecoveryRecord>,
}

/// A probe that observes nothing, for callers that only need counts.
pub struct NoProbe;

impl WorkspaceProbe for NoProbe {
    fn observe(&self, _: &Path) -> WorkspaceObservation {
        WorkspaceObservation { exists: true, ..Default::default() }
    }
}

impl FleetSnapshot {
    pub fn load(
        store:        &Store,
        registry:     &HarnessRegistry,
        probe:        &dyn WorkspaceProbe,
        sessions_dir: &Path,
        now:          i64,
    ) -> Result<Self> {
        let sessions = store.list_sessions()?;
        let orchestrators: HashSet<String> = store.list_orchestrators()?.into_iter().map(|o| o.id).collect();
        let mut records = store.fleet_records()?;
        let mut members = Vec::with_capacity(sessions.len());
        for session in &sessions {
            let session = session.clone();
            // The prompt-agent singleton (MLOPS-5415) is never a fleet
            // member: no island, no Board row, no needs-you contribution.
            // It still lives in the store and answers to `ninox
            // list`/`ninox connect` directly. See
            // `worker_type::is_prompt_agent_session`'s doc comment for why
            // this isn't just the `prompt` worker-type tag.
            if crate::worker_type::is_prompt_agent_session(&session) {
                continue;
            }
            let role = role_of(&session, &orchestrators);
            let record = records.remove(&session.id).unwrap_or_default();
            let pr = session.pr_id.map(|id| store.get_pr(id)).transpose()?.flatten();
            let ci = session.pr_id.map(|id| store.get_ci_status(id)).transpose()?.flatten();
            let pending_inbox = crate::inbox::pending_count(sessions_dir, &session.id);
            let can_resume = session.claude_session_id.is_some()
                && registry.session_can_resume(&session, &sessions);
            let workspace = session.workspace_path.as_deref()
                .map(|p| probe.observe(Path::new(p)))
                .unwrap_or_default();
            members.push(FleetMember {
                session, role, record, pr, ci, pending_inbox, can_resume, workspace,
                anomalies: Vec::new(),
            });
        }
        let mut snapshot = Self::assemble(members, store.open_work_requests(None)?, store.list_recoveries()?, now);
        snapshot.open_user_requests = store.unresolved_user_requests_for_briefing(None)?;
        Ok(snapshot)
    }

    /// Pure tail of [`load`](Self::load): orders members and computes
    /// anomalies. Tests build snapshots through this directly.
    pub fn assemble(
        mut members:   Vec<FleetMember>,
        open_requests: Vec<WorkRequestRow>,
        recoveries:    Vec<RecoveryRecord>,
        taken_at:      i64,
    ) -> Self {
        members.sort_by(|a, b| a.session.started_at.cmp(&b.session.started_at).then(a.id().cmp(b.id())));
        let anomalies: Vec<Vec<Anomaly>> = members.iter().map(|m| compute_anomalies(m, &members)).collect();
        for (m, a) in members.iter_mut().zip(anomalies) {
            m.anomalies = a;
        }
        Self { taken_at, members, open_requests, open_user_requests: Vec::new(), recoveries }
    }

    pub fn member(&self, id: &str) -> Option<&FleetMember> {
        self.members.iter().find(|m| m.id() == id)
    }

    pub fn workers_of<'a>(&'a self, orchestrator_id: &'a str) -> impl Iterator<Item = &'a FleetMember> + 'a {
        self.members.iter().filter(move |m| {
            m.role == Role::Worker && m.session.orchestrator_id.as_deref() == Some(orchestrator_id)
        })
    }

    pub fn awaiting_restore(&self) -> impl Iterator<Item = &FleetMember> {
        self.members.iter().filter(|m| m.awaiting_restore())
    }

    pub fn recovery(&self, orchestrator_id: &str) -> Option<&RecoveryRecord> {
        self.recoveries.iter().find(|r| r.orchestrator_id == orchestrator_id)
    }

    pub fn requests_for<'a>(&'a self, orchestrator_id: &'a str) -> impl Iterator<Item = &'a WorkRequestRow> + 'a {
        self.open_requests.iter().filter(move |r| r.orchestrator_id.as_deref() == Some(orchestrator_id))
    }

    /// `user_requests` belong to whichever session asked — worker or
    /// orchestrator — so this filters on `requester_session_id`, unlike
    /// `requests_for`'s `orchestrator_id`.
    pub fn user_requests_for<'a>(&'a self, session_id: &'a str) -> impl Iterator<Item = &'a UserRequestRow> + 'a {
        self.open_user_requests.iter().filter(move |r| r.requester_session_id == session_id)
    }

    /// Latest interruption across an orchestrator and its workers, counting
    /// only interruptions still awaiting restore or handled by the current
    /// restore — an old, long-recovered one is not news.
    pub fn interruption_of(&self, orchestrator_id: &str) -> Option<(i64, Option<String>)> {
        self.member(orchestrator_id).into_iter()
            .chain(self.workers_of(orchestrator_id))
            .filter(|m| m.awaiting_restore() || m.record.restored_for_current_interruption())
            .filter_map(|m| m.record.interrupted_at.map(|t| (t, m.record.interrupt_cause.clone())))
            .max_by_key(|(t, _)| *t)
    }
}

fn role_of(session: &Session, orchestrators: &HashSet<String>) -> Role {
    if orchestrators.contains(&session.id) {
        Role::Orchestrator
    } else if session.orchestrator_id.is_some() {
        Role::Worker
    } else {
        Role::Standalone
    }
}

fn compute_anomalies(m: &FleetMember, all: &[FleetMember]) -> Vec<Anomaly> {
    if !m.awaiting_restore() {
        return Vec::new();
    }
    let is_orch = m.role == Role::Orchestrator;
    let mut out = validate_workspace(
        m.session.workspace_path.as_deref(), is_orch, m.record.branch.as_deref(), &m.workspace,
    );
    let reads_claude_settings = m.session.agent_type == crate::harness::CLAUDE_CODE_HARNESS;
    if let Some(detail) = m.workspace.cross_session_override.as_ref().filter(|_| reads_claude_settings) {
        out.push(Anomaly::CrossSessionInboundOverridden { detail: detail.clone() });
    }
    if m.role == Role::Worker {
        let orch_id = m.session.orchestrator_id.as_deref().unwrap_or_default();
        let orch_alive = all.iter().find(|o| o.id() == orch_id)
            .is_some_and(|o| !o.session.status.is_terminal() || o.awaiting_restore());
        if !orch_alive {
            out.push(Anomaly::OrchestratorGone { orchestrator_id: orch_id.to_string() });
        }
        if !m.can_resume && m.record.task_brief.is_none() {
            out.push(Anomaly::NoTaskBrief);
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub fn session(id: &str, orch: Option<&str>, status: SessionStatus, started_at: i64) -> Session {
        Session {
            id: id.into(), orchestrator_id: orch.map(str::to_string), name: format!("{id}-name"),
            repo: "o/r".into(), status, agent_type: "claude-code".into(), cost_usd: 0.0,
            started_at, pr_number: None, pr_id: None,
            workspace_path: Some(format!("/ws/{id}")), pid: None, model: None, observed_model: None,
            context_tokens: None, brain_reads: Vec::new(), brain_write: None, context_used_pct: None,
            context_total_tokens: None, context_window_size: None,
            claude_session_id: Some(format!("uuid-{id}")), summary: Some(format!("task of {id}")),
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        }
    }

    pub fn member(session: Session, role: Role) -> FleetMember {
        FleetMember {
            session, role, record: FleetRecord::default(), pr: None, ci: None,
            pending_inbox: 0, can_resume: true,
            workspace: WorkspaceObservation { exists: true, is_git: true, branch: None, dirty: Some(false), ..Default::default() },
            anomalies: Vec::new(),
        }
    }

    pub fn interrupted(mut m: FleetMember, at: i64, last: SessionStatus) -> FleetMember {
        m.session.status = SessionStatus::Interrupted;
        m.record.interrupted_at = Some(at);
        m.record.last_status = Some(last);
        m.record.interrupt_cause = Some("reboot".into());
        m
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    #[test]
    fn interrupted_and_fresh_restart_candidates_await_restore() {
        let m = interrupted(member(session("a", Some("o"), SessionStatus::Working, 1), Role::Worker), 10, SessionStatus::Working);
        assert!(m.awaiting_restore());

        let mut t = member(session("b", Some("o"), SessionStatus::Terminated, 1), Role::Worker);
        assert!(!t.awaiting_restore(), "a plain Terminated session is gone for good");
        t.record.interrupted_at = Some(10);
        t.session.terminal_at = Some(11);
        t.record.reconciled_terminal_at = Some(11);
        assert!(t.awaiting_restore(), "Terminated by reconciliation restarts fresh");
        t.record.restored_at = Some(20);
        t.record.restore_mode = Some("fresh".into());
        assert!(!t.awaiting_restore());
        t.record.restore_mode = Some("failed".into());
        assert!(t.awaiting_restore(), "a failed attempt is retried");
    }

    #[test]
    fn a_session_that_came_back_and_then_ended_is_not_a_restore_candidate() {
        // Resumed by hand (no restore stamp) after an interruption, then
        // finished or killed: `interrupted_at` is still set.
        let mut m = member(session("w", Some("o"), SessionStatus::Terminated, 1), Role::Worker);
        m.record.interrupted_at = Some(10);
        m.record.last_status = Some(SessionStatus::Working);
        assert!(!m.awaiting_restore(), "never reconciled to Terminated");
        m.session.terminal_at = Some(500);
        assert!(!m.awaiting_restore(), "a later death stamps a terminal_at of its own");
        m.record.reconciled_terminal_at = Some(20);
        assert!(!m.awaiting_restore(), "a re-stamped terminal_at is not the reconciled row");
        m.session.terminal_at = None;
        assert!(!m.awaiting_restore(), "user terminate after a relaunch leaves terminal_at empty");
    }

    #[test]
    fn effective_status_prefers_pre_reconciliation_status() {
        let m = interrupted(member(session("a", Some("o"), SessionStatus::Working, 1), Role::Worker), 10, SessionStatus::PrOpen);
        assert_eq!(m.effective_status(), &SessionStatus::PrOpen);
    }

    #[test]
    fn worker_of_vanished_orchestrator_is_flagged() {
        let w = interrupted(member(session("w", Some("gone"), SessionStatus::Working, 1), Role::Worker), 10, SessionStatus::Working);
        let snap = FleetSnapshot::assemble(vec![w], vec![], vec![], 0);
        assert_eq!(
            snap.members[0].anomalies,
            vec![Anomaly::OrchestratorGone { orchestrator_id: "gone".into() }],
        );
    }

    #[test]
    fn worker_of_interrupted_orchestrator_is_fine() {
        let o = interrupted(member(session("o", None, SessionStatus::Working, 0), Role::Orchestrator), 10, SessionStatus::Working);
        let w = interrupted(member(session("w", Some("o"), SessionStatus::Working, 1), Role::Worker), 10, SessionStatus::Working);
        let snap = FleetSnapshot::assemble(vec![w, o], vec![], vec![], 0);
        assert!(snap.members.iter().all(|m| m.anomalies.is_empty()));
        assert_eq!(snap.members[0].id(), "o", "sorted by start time");
        assert_eq!(snap.workers_of("o").count(), 1);
    }

    #[test]
    fn an_overridden_cross_session_setting_is_an_informational_anomaly() {
        let o = member(session("o", None, SessionStatus::Working, 0), Role::Orchestrator);
        let mut w = interrupted(member(session("w", Some("o"), SessionStatus::Working, 1), Role::Worker), 10, SessionStatus::Working);
        w.workspace.cross_session_override = Some("refuse in /w/.claude/settings.local.json".into());
        let snap = FleetSnapshot::assemble(vec![o, w], vec![], vec![], 0);
        let w = snap.member("w").unwrap();
        assert_eq!(w.anomalies, vec![Anomaly::CrossSessionInboundOverridden { detail: "refuse in /w/.claude/settings.local.json".into() }]);
        assert_eq!(w.blocking_anomalies().count(), 0);
    }

    #[test]
    fn a_non_claude_session_gets_no_cross_session_anomaly() {
        let o = member(session("o", None, SessionStatus::Working, 0), Role::Orchestrator);
        let mut w = interrupted(member(session("w", Some("o"), SessionStatus::Working, 1), Role::Worker), 10, SessionStatus::Working);
        w.session.agent_type = "codex".into();
        w.workspace.cross_session_override = Some("refuse in /w/.claude/settings.local.json".into());
        let snap = FleetSnapshot::assemble(vec![o, w], vec![], vec![], 0);
        assert!(snap.member("w").unwrap().anomalies.is_empty());
    }

    #[test]
    fn fresh_worker_without_brief_gets_informational_anomaly() {
        let o = member(session("o", None, SessionStatus::Working, 0), Role::Orchestrator);
        let mut w = interrupted(member(session("w", Some("o"), SessionStatus::Working, 1), Role::Worker), 10, SessionStatus::Working);
        w.can_resume = false;
        let snap = FleetSnapshot::assemble(vec![o, w], vec![], vec![], 0);
        let w = snap.member("w").unwrap();
        assert_eq!(w.anomalies, vec![Anomaly::NoTaskBrief]);
        assert_eq!(w.blocking_anomalies().count(), 0);
    }

    #[test]
    fn interruption_of_ignores_long_recovered_members() {
        let o = member(session("o", None, SessionStatus::Working, 0), Role::Orchestrator);
        let mut old = member(session("old", Some("o"), SessionStatus::Working, 1), Role::Worker);
        old.record.interrupted_at = Some(5);
        old.record.restored_at = Some(6);
        let mut fresh = member(session("new", Some("o"), SessionStatus::Working, 2), Role::Worker);
        fresh.record.interrupted_at = Some(50);
        fresh.record.restored_at = Some(60);
        fresh.record.interrupt_cause = Some("reboot".into());
        let snap = FleetSnapshot::assemble(vec![o, old, fresh], vec![], vec![], 0);
        // `old` was restored after its interruption too; both count — the
        // latest wins.
        assert_eq!(snap.interruption_of("o"), Some((50, Some("reboot".into()))));
    }

    #[test]
    fn load_reads_store_rows() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db")).unwrap();
        let o = session("o", None, SessionStatus::Interrupted, 0);
        store.upsert_session(&o).unwrap();
        store.upsert_orchestrator(&crate::types::Orchestrator { id: "o".into(), name: "o".into(), created_at: 0 }).unwrap();
        let mut w = session("w", Some("o"), SessionStatus::Interrupted, 1);
        w.pr_id = Some(9);
        w.pr_number = Some(41);
        store.upsert_session(&w).unwrap();
        store.upsert_pr(&PR { id: 9, number: 41, title: "t".into(), url: "u".into(), body: "".into(), session_id: "w".into() }).unwrap();
        store.upsert_ci_status(&CIStatus { pr_id: 9, total: 2, passing: 2, failing: 0, pending: 0 }).unwrap();
        store.record_spawn_facts("w", "the brief", Some("w")).unwrap();
        store.record_interruption("w", 10, &SessionStatus::PrOpen, Some("reboot")).unwrap();
        crate::inbox::write_message(dir.path(), "o", "hello", None).unwrap();
        store.insert_user_request(&UserRequestRow {
            id: "ur-1".into(), requester_session_id: "w".into(), prompt: "ok to proceed?".into(),
            mode: crate::types::UserRequestMode::Sync, status: crate::types::UserRequestStatus::Pending,
            answer: None, answered_at: None, answer_delivered_at: None, created_at: 5,
        }).unwrap();
        store.insert_user_request(&UserRequestRow {
            id: "ur-2".into(), requester_session_id: "w".into(), prompt: "which env?".into(),
            mode: crate::types::UserRequestMode::Sync, status: crate::types::UserRequestStatus::Answered,
            answer: Some("staging".into()), answered_at: Some(6), answer_delivered_at: None, created_at: 6,
        }).unwrap();
        store.insert_user_request(&UserRequestRow {
            id: "ur-3".into(), requester_session_id: "w".into(), prompt: "already handled".into(),
            mode: crate::types::UserRequestMode::Sync, status: crate::types::UserRequestStatus::Answered,
            answer: Some("done".into()), answered_at: Some(7), answer_delivered_at: Some(8), created_at: 7,
        }).unwrap();

        let registry = HarnessRegistry::from_config(&Default::default());
        let snap = FleetSnapshot::load(&store, &registry, &NoProbe, dir.path(), 99).unwrap();
        let o = snap.member("o").unwrap();
        assert_eq!(o.role, Role::Orchestrator);
        assert_eq!(o.pending_inbox, 1);
        let w = snap.member("w").unwrap();
        assert_eq!(w.role, Role::Worker);
        assert!(w.can_resume);
        assert_eq!(w.pr.as_ref().unwrap().number, 41);
        assert_eq!(w.ci.as_ref().unwrap().passing, 2);
        assert_eq!(w.record.task_brief.as_deref(), Some("the brief"));
        assert_eq!(snap.awaiting_restore().count(), 2);
        let ur_ids: Vec<_> = snap.user_requests_for("w").map(|r| r.id.as_str()).collect();
        assert_eq!(ur_ids, vec!["ur-1", "ur-2"], "pending and answered-undelivered surface; delivered does not");
    }

    #[test]
    fn load_excludes_the_prompt_agent_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db")).unwrap();
        let mut p = session(crate::worker_type::PROMPT_SESSION_ID, None, SessionStatus::Working, 0);
        p.worker_type = Some(crate::worker_type::PROMPT_WORKER_TYPE.to_string());
        store.upsert_session(&p).unwrap();
        store.upsert_session(&session("plain", None, SessionStatus::Working, 1)).unwrap();

        let registry = HarnessRegistry::from_config(&Default::default());
        let snap = FleetSnapshot::load(&store, &registry, &NoProbe, dir.path(), 99).unwrap();

        assert!(snap.member(crate::worker_type::PROMPT_SESSION_ID).is_none(), "prompt-agent session must not be a fleet member");
        assert!(snap.member("plain").is_some());
        assert_eq!(snap.members.len(), 1);
    }

    #[test]
    fn load_does_not_exclude_an_ordinary_spawn_sharing_the_prompt_worker_type() {
        // `ninox spawn --type prompt --name foo` (a different id) must stay
        // a normal fleet member — only the singleton's reserved id is hidden.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db")).unwrap();
        let mut w = session("foo", None, SessionStatus::Working, 0);
        w.worker_type = Some(crate::worker_type::PROMPT_WORKER_TYPE.to_string());
        store.upsert_session(&w).unwrap();

        let registry = HarnessRegistry::from_config(&Default::default());
        let snap = FleetSnapshot::load(&store, &registry, &NoProbe, dir.path(), 99).unwrap();

        assert!(snap.member("foo").is_some(), "a non-singleton 'prompt'-typed spawn must still be a fleet member");
    }
}
