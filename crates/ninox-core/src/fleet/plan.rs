//! Restore ordering (spec §5.3): workers (and standalone sessions) first so
//! the fleet is live before its orchestrator wakes, orchestrators last.
//!
//! Skip rules, decided here once:
//! - Any blocking [`Anomaly`] (missing worktree, not a git worktree, no
//!   workspace) keeps a session out — restore never repairs by guessing.
//!   A branch other than the recorded one is only reported.
//! - A worker whose orchestrator is gone for good is skipped
//!   (`OrchestratorGone`): resumed, it would report to nobody. Its
//!   worktree is untouched, so it can still be resumed by hand.
//! - `--only` narrows to the listed ids but never overrides an anomaly.

use super::probe::Anomaly;
use super::snapshot::{FleetMember, FleetSnapshot, Role};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreMode {
    /// `--resume <claude_session_id>`: conversation intact.
    Resume,
    /// The harness can't resume: restart in the workspace and brief it.
    Fresh,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RestoreStep {
    pub session_id: String,
    pub name:       String,
    pub role:       Role,
    pub mode:       RestoreMode,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SkippedSession {
    pub session_id: String,
    pub name:       String,
    pub role:       Role,
    pub anomalies:  Vec<Anomaly>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RestorePlan {
    pub workers:           Vec<RestoreStep>,
    pub orchestrators:     Vec<RestoreStep>,
    pub skipped:           Vec<SkippedSession>,
    /// Live orchestrators whose recovery briefing was never delivered (a
    /// previous restore died between relaunch and briefing).
    pub pending_briefings: Vec<String>,
}

impl RestorePlan {
    pub fn is_empty(&self) -> bool {
        self.workers.is_empty() && self.orchestrators.is_empty() && self.pending_briefings.is_empty()
    }

    pub fn steps(&self) -> impl Iterator<Item = &RestoreStep> {
        self.workers.iter().chain(&self.orchestrators)
    }
}

pub fn plan_restore(snap: &FleetSnapshot, only: &[String]) -> RestorePlan {
    let selected = |id: &str| only.is_empty() || only.iter().any(|o| o == id);
    let mut plan = RestorePlan::default();
    for m in snap.awaiting_restore().filter(|m| selected(m.id())) {
        let blocking: Vec<Anomaly> = m.blocking_anomalies().cloned().collect();
        if !blocking.is_empty() {
            plan.skipped.push(SkippedSession {
                session_id: m.id().to_string(),
                name:       m.session.name.clone(),
                role:       m.role,
                anomalies:  blocking,
            });
            continue;
        }
        let step = step_for(m);
        match m.role {
            Role::Orchestrator => plan.orchestrators.push(step),
            Role::Worker | Role::Standalone => plan.workers.push(step),
        }
    }
    for r in &snap.recoveries {
        if r.briefing_sent_at.is_some() || !selected(&r.orchestrator_id) {
            continue;
        }
        let live = snap.member(&r.orchestrator_id)
            .is_some_and(|m| !m.session.status.is_terminal());
        if live {
            plan.pending_briefings.push(r.orchestrator_id.clone());
        }
    }
    plan
}

/// The snapshot as it would look after `plan` succeeded, for previews
/// (`--dry-run`, `ninox fleet brief` before a restore).
pub fn project_outcomes(snap: &FleetSnapshot, plan: &RestorePlan, now: i64) -> FleetSnapshot {
    let mut projected = snap.clone();
    for step in plan.steps() {
        if let Some(m) = projected.members.iter_mut().find(|m| m.id() == step.session_id) {
            m.record.restored_at = Some(now.max(m.record.interrupted_at.unwrap_or(now)));
            m.record.restore_mode = Some(match step.mode {
                RestoreMode::Resume => super::RestoreOutcome::Resumed,
                RestoreMode::Fresh  => super::RestoreOutcome::Fresh,
            }.as_str().to_string());
            m.session.status = crate::types::SessionStatus::Working;
        }
    }
    projected
}

fn step_for(m: &FleetMember) -> RestoreStep {
    RestoreStep {
        session_id: m.id().to_string(),
        name:       m.session.name.clone(),
        role:       m.role,
        mode:       if m.can_resume { RestoreMode::Resume } else { RestoreMode::Fresh },
    }
}

#[cfg(test)]
mod tests {
    use super::super::snapshot::test_support::*;
    use super::*;
    use crate::store::RecoveryRecord;
    use crate::types::SessionStatus;

    fn fleet() -> FleetSnapshot {
        let o = interrupted(member(session("o", None, SessionStatus::Working, 0), Role::Orchestrator), 10, SessionStatus::Working);
        let a = interrupted(member(session("a", Some("o"), SessionStatus::Working, 2), Role::Worker), 10, SessionStatus::Working);
        let mut b = interrupted(member(session("b", Some("o"), SessionStatus::Working, 1), Role::Worker), 10, SessionStatus::Working);
        b.can_resume = false;
        b.record.task_brief = Some("brief".into());
        let s = interrupted(member(session("s", None, SessionStatus::Working, 3), Role::Standalone), 10, SessionStatus::Working);
        let live = member(session("live", Some("o"), SessionStatus::Working, 4), Role::Worker);
        FleetSnapshot::assemble(vec![o, a, b, s, live], vec![], vec![], 0)
    }

    #[test]
    fn workers_first_then_orchestrators_in_start_order() {
        let plan = plan_restore(&fleet(), &[]);
        let workers: Vec<_> = plan.workers.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(workers, ["b", "a", "s"]);
        assert_eq!(plan.orchestrators.iter().map(|s| s.session_id.as_str()).collect::<Vec<_>>(), ["o"]);
        let order: Vec<_> = plan.steps().map(|s| s.session_id.as_str()).collect();
        assert_eq!(order.last(), Some(&"o"));
        assert!(plan.skipped.is_empty());
    }

    #[test]
    fn harness_without_resume_restarts_fresh() {
        let plan = plan_restore(&fleet(), &[]);
        let b = plan.workers.iter().find(|s| s.session_id == "b").unwrap();
        assert_eq!(b.mode, RestoreMode::Fresh);
        let a = plan.workers.iter().find(|s| s.session_id == "a").unwrap();
        assert_eq!(a.mode, RestoreMode::Resume);
    }

    #[test]
    fn only_narrows_selection() {
        let plan = plan_restore(&fleet(), &["a".to_string()]);
        assert_eq!(plan.steps().count(), 1);
        assert_eq!(plan.workers[0].session_id, "a");
    }

    #[test]
    fn anomalies_skip_even_when_explicitly_selected() {
        let mut snap = fleet();
        let a = snap.members.iter_mut().find(|m| m.id() == "a").unwrap();
        a.workspace.exists = false;
        let snap = FleetSnapshot::assemble(snap.members, vec![], vec![], 0);
        let plan = plan_restore(&snap, &["a".to_string()]);
        assert!(plan.workers.is_empty());
        assert_eq!(plan.skipped.len(), 1);
        assert_eq!(plan.skipped[0].anomalies, vec![Anomaly::WorkspaceMissing { path: "/ws/a".into() }]);
    }

    #[test]
    fn orphaned_worker_is_skipped() {
        let w = interrupted(member(session("w", Some("ghost"), SessionStatus::Working, 1), Role::Worker), 10, SessionStatus::Working);
        let plan = plan_restore(&FleetSnapshot::assemble(vec![w], vec![], vec![], 0), &[]);
        assert!(plan.is_empty());
        assert_eq!(plan.skipped[0].session_id, "w");
    }

    #[test]
    fn nothing_to_do_is_empty_and_rerun_safe() {
        let live = member(session("o", None, SessionStatus::Working, 0), Role::Orchestrator);
        let plan = plan_restore(&FleetSnapshot::assemble(vec![live], vec![], vec![], 0), &[]);
        assert!(plan.is_empty());
    }

    #[test]
    fn projection_marks_planned_steps_restored() {
        let snap = fleet();
        let plan = plan_restore(&snap, &[]);
        let projected = project_outcomes(&snap, &plan, 5);
        let b = projected.member("b").unwrap();
        assert_eq!(b.last_restore_outcome(), Some(super::super::RestoreOutcome::Fresh));
        assert_eq!(projected.member("o").unwrap().last_restore_outcome(), Some(super::super::RestoreOutcome::Resumed));
        assert!(plan_restore(&projected, &[]).steps().next().is_none(), "projection is fully restored");
        assert!(snap.member("b").unwrap().last_restore_outcome().is_none(), "original untouched");
    }

    #[test]
    fn undelivered_briefing_of_live_orchestrator_is_pending() {
        let o = member(session("o", None, SessionStatus::Working, 0), Role::Orchestrator);
        let rec = |sent| RecoveryRecord {
            orchestrator_id: "o".into(), interrupted_at: Some(1), restored_at: 2,
            briefing_sent_at: sent, acked_at: None,
        };
        let snap = FleetSnapshot::assemble(vec![o.clone()], vec![], vec![rec(None)], 0);
        assert_eq!(plan_restore(&snap, &[]).pending_briefings, vec!["o".to_string()]);
        let snap = FleetSnapshot::assemble(vec![o], vec![], vec![rec(Some(3))], 0);
        assert!(plan_restore(&snap, &[]).pending_briefings.is_empty());
    }
}
