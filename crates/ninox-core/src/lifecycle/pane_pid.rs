//! Recording a session's pane root pid, the value `orchestrator_auth` binds a
//! runtime identity to. CLI spawns insert the row before the pane exists, so
//! the pid is filled afterwards, and the poller backfills rows that lack one.

use crate::store::Store;
use crate::types::Session;

/// Fills `session`'s NULL pid with `host_pid`, the pid the pane host reports
/// for the live pane named exactly `session.id`. Never touches a remote
/// session, a terminal one, or a row that already records a pid.
pub(crate) fn adopt_pane_pid(store: &Store, session: &Session, host_pid: Option<u32>) -> bool {
    let Some(pid) = host_pid.filter(|p| *p != 0) else { return false };
    if session.machine_id.is_some() || session.pid.is_some() || session.status.is_terminal() {
        return false;
    }
    store.fill_session_pid_if_null(&session.id, pid).unwrap_or_else(|e| {
        tracing::warn!("record pane pid for {}: {e}", session.id);
        false
    })
}

/// Reads `id`'s live pane pid from its backend and records it.
pub async fn record_pane_pid(store: &Store, id: &str) -> bool {
    let Ok(Some(session)) = store.get_session(id) else { return false };
    if session.machine_id.is_some() || session.pid.is_some() {
        return false;
    }
    adopt_pane_pid(store, &session, crate::runtime::session_pid(id).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SessionStatus;

    fn row(id: &str) -> Session {
        Session {
            id: id.into(), orchestrator_id: None, name: id.into(),
            repo: String::new(), status: SessionStatus::Working,
            agent_type: "claude-code".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: Some("/ws".into()), pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None, gate_status: None,
            merged_at: None, activity: Default::default(), activity_note: None,
            activity_since: None, machine_id: None, worker_type: None,
            success_criteria: Vec::new(), completion_outcome: None, self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        }
    }

    #[test]
    fn fills_a_null_pid_and_never_overwrites() {
        let _iso = crate::test_isolation::isolate();
        let store = Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap();
        let s = row("w1");
        store.upsert_session(&s).unwrap();
        assert!(adopt_pane_pid(&store, &s, Some(4242)));
        assert_eq!(store.get_session("w1").unwrap().unwrap().pid, Some(4242));
        let filled = store.get_session("w1").unwrap().unwrap();
        assert!(!adopt_pane_pid(&store, &filled, Some(7)));
        assert!(!store.fill_session_pid_if_null("w1", 7).unwrap());
        assert_eq!(store.get_session("w1").unwrap().unwrap().pid, Some(4242));
    }

    #[test]
    fn skips_remote_terminal_and_missing_pane() {
        let _iso = crate::test_isolation::isolate();
        let store = Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap();
        let mut remote = row("r1");
        remote.machine_id = Some("m".into());
        let mut dead = row("d1");
        dead.status = SessionStatus::Terminated;
        let live = row("l1");
        for s in [&remote, &dead, &live] {
            store.upsert_session(s).unwrap();
        }
        assert!(!adopt_pane_pid(&store, &remote, Some(1234)));
        assert!(!adopt_pane_pid(&store, &dead, Some(1234)));
        assert!(!adopt_pane_pid(&store, &live, None));
        assert!(!adopt_pane_pid(&store, &live, Some(0)));
        for id in ["r1", "d1", "l1"] {
            assert_eq!(store.get_session(id).unwrap().unwrap().pid, None);
        }
    }
}
