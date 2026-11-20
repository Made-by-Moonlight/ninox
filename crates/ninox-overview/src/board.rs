//! Which sessions the Board shows, and where — shared by the GUI's
//! `fleet_board.rs` and the TUI's Board tab.

use ninox_core::types::{Session, SessionStatus};

/// Whether a session with `status` is shown under the column headed
/// `column`: its own status, plus `Terminated` under Done and
/// `Interrupted` and `Queued` under Working (a session cut off mid-task, or
/// not started yet, belongs with the ones still working, not the ones that
/// finished).
pub fn column_absorbs(column: &SessionStatus, status: &SessionStatus) -> bool {
    status == column
        || (*column == SessionStatus::Done && *status == SessionStatus::Terminated)
        || (*column == SessionStatus::Working && matches!(status, SessionStatus::Interrupted | SessionStatus::Queued))
}

/// Whether `session` is a Board card: not an orchestrator's own session,
/// not the prompt agent, inside `scope` (an orchestrator id; `None` is
/// all), and matching `query` on name or repo (case-insensitive; empty
/// matches everything).
pub fn on_board(session: &Session, is_orchestrator: bool, scope: Option<&str>, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    !is_orchestrator
        && !ninox_core::worker_type::is_prompt_agent_session(session)
        && scope.is_none_or(|oid| session.orchestrator_id.as_deref() == Some(oid))
        && (q.is_empty() || session.name.to_lowercase().contains(&q) || session.repo.to_lowercase().contains(&q))
}

/// Whether "Resume all" relaunches `session`: interrupted, and not the
/// prompt agent, which restarts through its own path.
pub fn is_resume_target(session: &Session) -> bool {
    session.status == SessionStatus::Interrupted && !ninox_core::worker_type::is_prompt_agent_session(session)
}

pub fn resume_all_targets<'a>(sessions: impl IntoIterator<Item = &'a Session>) -> Vec<&'a Session> {
    sessions.into_iter().filter(|s| is_resume_target(s)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::types::ActivityState;

    fn session(id: &str, orch: Option<&str>, status: SessionStatus) -> Session {
        Session {
            id: id.into(), orchestrator_id: orch.map(str::to_string), name: format!("{id}-name"),
            repo: "acme/widgets".into(), status, agent_type: "claude-code".into(), cost_usd: 0.0,
            started_at: 0, pr_number: None, pr_id: None, workspace_path: None, pid: None, model: None, observed_model: None,
            context_tokens: None, brain_reads: Vec::new(), brain_write: None, context_used_pct: None, context_total_tokens: None,
            context_window_size: None, claude_session_id: None, summary: None, terminal_at: None,
            gate_status: None, merged_at: None, activity: ActivityState::default(), activity_note: None,
            activity_since: None, machine_id: None, worker_type: None, success_criteria: Vec::new(),
            completion_outcome: None, self_completed_at: None, cleanup_result: None, cleanup_at: None,
        }
    }

    fn prompt_agent(status: SessionStatus) -> Session {
        Session {
            worker_type: Some(ninox_core::worker_type::PROMPT_WORKER_TYPE.into()),
            ..session(ninox_core::worker_type::PROMPT_SESSION_ID, None, status)
        }
    }

    #[test]
    fn columns_fold_terminated_into_done_and_interrupted_into_working() {
        use SessionStatus::*;
        assert!(column_absorbs(&Done, &Terminated) && column_absorbs(&Working, &Interrupted) && column_absorbs(&PrOpen, &PrOpen));
        assert!(!column_absorbs(&Working, &Terminated) && !column_absorbs(&Done, &Interrupted));
    }

    #[test]
    fn the_board_filters_by_scope_and_query_and_skips_orchestrators_and_the_prompt_agent() {
        let w = session("w1", Some("o1"), SessionStatus::Working);
        assert!(on_board(&w, false, None, ""));
        assert!(!on_board(&w, true, None, ""), "an orchestrator's own session");
        assert!(on_board(&w, false, Some("o1"), "") && !on_board(&w, false, Some("o2"), ""));
        assert!(on_board(&w, false, None, "W1-N") && on_board(&w, false, None, " widgets ") && !on_board(&w, false, None, "gadgets"));
        assert!(!on_board(&prompt_agent(SessionStatus::Working), false, None, ""));
    }

    #[test]
    fn resume_all_targets_only_interrupted_sessions_and_never_the_prompt_agent() {
        let sessions = [
            session("a", None, SessionStatus::Interrupted),
            session("b", None, SessionStatus::Working),
            prompt_agent(SessionStatus::Interrupted),
        ];
        assert_eq!(resume_all_targets(&sessions).iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), vec!["a"]);
    }
}
