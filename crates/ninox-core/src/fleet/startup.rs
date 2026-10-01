//! Restore policy at engine startup (spec §5.4).
//!
//! `Poller::start` calls [`on_engine_startup`] once, right after
//! reconciliation:
//! - `manual`: nothing.
//! - `prompt`: when anything awaits restore, sets the pending-restore flag.
//!   A frontend polls [`pending_restore`] and, on the user's yes, runs
//!   `ninox fleet restore --yes`; on no, calls [`dismiss_pending_restore`].
//! - `auto`: the poller invokes its injected restorer
//!   (`Poller::with_fleet_restorer`); without one it degrades to `prompt`.

use super::snapshot::{FleetSnapshot, NoProbe, Role};
use crate::config::RestorePolicy;
use crate::harness::HarnessRegistry;
use crate::store::Store;
use serde::Serialize;

/// `fleet_state` key holding the epoch-ms the pending flag was raised.
pub const PENDING_RESTORE_KEY: &str = "pending_restore_at";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RestoreSummary {
    /// Workers plus standalone sessions awaiting restore.
    pub workers:        usize,
    pub orchestrators:  usize,
    /// Latest interruption among them.
    pub interrupted_at: Option<i64>,
    /// When the pending flag was raised; `None` from [`restore_summary`].
    pub flagged_at:     Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupAction {
    Nothing,
    FlaggedPending(RestoreSummary),
    AutoRestore(RestoreSummary),
}

/// Counts what awaits restore, without touching workspaces. `None` when
/// nothing does.
pub fn restore_summary(store: &Store) -> anyhow::Result<Option<RestoreSummary>> {
    let registry = HarnessRegistry::from_config(&Default::default());
    let snap = FleetSnapshot::load(store, &registry, &NoProbe, &crate::config::AppConfig::sessions_dir(), 0)?;
    Ok(summarize(&snap))
}

pub fn summarize(snap: &FleetSnapshot) -> Option<RestoreSummary> {
    let (mut workers, mut orchestrators, mut latest) = (0, 0, None::<i64>);
    for m in snap.awaiting_restore() {
        match m.role {
            Role::Orchestrator => orchestrators += 1,
            Role::Worker | Role::Standalone => workers += 1,
        }
        latest = latest.max(m.record.interrupted_at);
    }
    (workers + orchestrators > 0).then_some(RestoreSummary {
        workers, orchestrators, interrupted_at: latest, flagged_at: None,
    })
}

/// Pure policy decision.
pub fn decide(policy: RestorePolicy, summary: Option<RestoreSummary>) -> StartupAction {
    match (policy, summary) {
        (_, None) | (RestorePolicy::Manual, _) => StartupAction::Nothing,
        (RestorePolicy::Prompt, Some(s)) => StartupAction::FlaggedPending(s),
        (RestorePolicy::Auto, Some(s)) => StartupAction::AutoRestore(s),
    }
}

/// Apply the policy's store side effects and return what the caller
/// should do next.
pub fn on_engine_startup(store: &Store, policy: RestorePolicy, now: i64) -> StartupAction {
    let summary = match restore_summary(store) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("fleet: restore summary failed: {e}");
            return StartupAction::Nothing;
        }
    };
    let action = decide(policy, summary);
    match &action {
        StartupAction::FlaggedPending(_) => flag_pending_restore(store, now),
        StartupAction::Nothing => {
            let _ = store.fleet_state_delete(PENDING_RESTORE_KEY);
        }
        StartupAction::AutoRestore(_) => {}
    }
    action
}

pub fn flag_pending_restore(store: &Store, now: i64) {
    if let Err(e) = store.fleet_state_set(PENDING_RESTORE_KEY, &now.to_string()) {
        tracing::warn!("fleet: cannot record pending restore: {e}");
    }
}

/// For frontends: `Some` while a `prompt`-policy restore offer is open and
/// there is still something to restore. Cheap enough to call on every UI
/// tick (one store pass, no git).
pub fn pending_restore(store: &Store) -> Option<RestoreSummary> {
    let flagged_at: i64 = store.fleet_state_get(PENDING_RESTORE_KEY).ok()??.parse().ok()?;
    let mut summary = restore_summary(store).ok()??;
    summary.flagged_at = Some(flagged_at);
    Some(summary)
}

/// The user declined the offer (or a restore ran).
pub fn dismiss_pending_restore(store: &Store) {
    if let Err(e) = store.fleet_state_delete(PENDING_RESTORE_KEY) {
        tracing::warn!("fleet: cannot clear pending restore: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::snapshot::test_support::session;
    use crate::types::SessionStatus;

    fn store() -> Store {
        Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap()
    }

    #[test]
    fn decide_matrix() {
        let s = RestoreSummary { workers: 1, orchestrators: 1, interrupted_at: None, flagged_at: None };
        assert_eq!(decide(RestorePolicy::Manual, Some(s.clone())), StartupAction::Nothing);
        assert_eq!(decide(RestorePolicy::Prompt, Some(s.clone())), StartupAction::FlaggedPending(s.clone()));
        assert_eq!(decide(RestorePolicy::Auto, Some(s.clone())), StartupAction::AutoRestore(s));
        assert_eq!(decide(RestorePolicy::Auto, None), StartupAction::Nothing);
    }

    #[test]
    fn prompt_policy_flags_and_pending_restore_reports_counts() {
        let st = store();
        st.upsert_session(&session("o", None, SessionStatus::Interrupted, 0)).unwrap();
        st.upsert_orchestrator(&crate::types::Orchestrator { id: "o".into(), name: "o".into(), created_at: 0 }).unwrap();
        st.upsert_session(&session("w", Some("o"), SessionStatus::Interrupted, 1)).unwrap();
        st.record_interruption("w", 77, &SessionStatus::Working, None).unwrap();

        assert!(pending_restore(&st).is_none());
        assert!(matches!(on_engine_startup(&st, RestorePolicy::Manual, 5), StartupAction::Nothing));
        assert!(pending_restore(&st).is_none());

        let action = on_engine_startup(&st, RestorePolicy::Prompt, 5);
        assert!(matches!(action, StartupAction::FlaggedPending(_)));
        let p = pending_restore(&st).unwrap();
        assert_eq!((p.workers, p.orchestrators, p.interrupted_at, p.flagged_at), (1, 1, Some(77), Some(5)));

        dismiss_pending_restore(&st);
        assert!(pending_restore(&st).is_none());
    }

    #[test]
    fn pending_flag_with_nothing_left_reports_none() {
        let st = store();
        flag_pending_restore(&st, 1);
        assert!(pending_restore(&st).is_none());
        // And a later startup with nothing to restore clears it.
        on_engine_startup(&st, RestorePolicy::Prompt, 2);
        assert!(st.fleet_state_get(PENDING_RESTORE_KEY).unwrap().is_none());
    }
}
