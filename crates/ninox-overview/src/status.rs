//! Member status classification for the island model. `has_pending_user_request`
//! is a direct-to-the-human-user signal — e.g. the not-yet-shipped
//! MLOPS-5184 "ninox user-request" feature (an agent asking the user for
//! sign-off). It is NOT `FleetSnapshot.open_requests` (`ninox request-work`
//! items): those go from a worker to its *orchestrator* and are routine
//! orchestrator business, not something that needs the human user — see
//! `islands.rs`'s `Island::pending_request` for that signal instead. This
//! module has no knowledge of work requests at all.

use ninox_core::types::{ActivityState, Session, SessionStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberStatus { Working, NeedsYou, WaitingOnPr, Done, Stopped, Queued }

pub fn classify_member(session: &Session, has_pending_user_request: bool) -> MemberStatus {
    if session.status == SessionStatus::Done || session.merged_at.is_some() {
        return MemberStatus::Done;
    }
    if session.status.is_terminal() {
        return MemberStatus::Stopped;
    }
    if session.status == SessionStatus::Queued {
        return MemberStatus::Queued;
    }
    if session.activity == ActivityState::Blocked
        || session.status == SessionStatus::CiFailed
        || session.status == SessionStatus::AwaitingReview
        || has_pending_user_request
    {
        return MemberStatus::NeedsYou;
    }
    if matches!(session.status, SessionStatus::PrOpen | SessionStatus::ReviewPending | SessionStatus::Mergeable) {
        return MemberStatus::WaitingOnPr;
    }
    MemberStatus::Working
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::types::{ActivityState, Session, SessionStatus};

    fn session(status: SessionStatus) -> Session {
        Session {
            id: "s".into(), orchestrator_id: None, name: "s-name".into(),
            repo: "o/r".into(), status, agent_type: "claude-code".into(), cost_usd: 0.0,
            started_at: 0, pr_number: None, pr_id: None,
            workspace_path: Some("/ws/s".into()), pid: None, model: None, observed_model: None,
            context_tokens: None, brain_reads: Vec::new(), brain_write: None, context_used_pct: None,
            context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: ActivityState::default(), activity_note: None, activity_since: None,
            machine_id: None, worker_type: None,
            success_criteria: Vec::new(), completion_outcome: None, self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        }
    }

    #[test]
    fn spawning_or_working_with_non_blocked_activity_is_working() {
        assert_eq!(classify_member(&session(SessionStatus::Spawning), false), MemberStatus::Working);
        assert_eq!(classify_member(&session(SessionStatus::Working), false), MemberStatus::Working);
    }

    #[test]
    fn blocked_activity_overrides_status_to_needs_you() {
        let mut s = session(SessionStatus::Working);
        s.activity = ActivityState::Blocked;
        assert_eq!(classify_member(&s, false), MemberStatus::NeedsYou);
    }

    #[test]
    fn ci_failed_status_is_needs_you_even_if_activity_is_working() {
        let mut s = session(SessionStatus::CiFailed);
        s.activity = ActivityState::Working;
        assert_eq!(classify_member(&s, false), MemberStatus::NeedsYou);
    }

    #[test]
    fn a_pending_user_request_makes_an_otherwise_working_session_needs_you() {
        assert_eq!(classify_member(&session(SessionStatus::Working), true), MemberStatus::NeedsYou);
    }

    #[test]
    fn pr_open_review_pending_and_mergeable_are_waiting_on_pr() {
        for status in [SessionStatus::PrOpen, SessionStatus::ReviewPending, SessionStatus::Mergeable] {
            assert_eq!(classify_member(&session(status.clone()), false), MemberStatus::WaitingOnPr, "{status:?}");
        }
    }

    #[test]
    fn done_status_is_done() {
        assert_eq!(classify_member(&session(SessionStatus::Done), false), MemberStatus::Done);
    }

    #[test]
    fn merged_at_set_is_done_even_if_status_lags_behind() {
        let mut s = session(SessionStatus::Working);
        s.merged_at = Some(123);
        assert_eq!(classify_member(&s, false), MemberStatus::Done);
    }

    #[test]
    fn terminated_and_interrupted_without_a_merge_are_stopped_not_done() {
        assert_eq!(classify_member(&session(SessionStatus::Terminated), false), MemberStatus::Stopped);
        assert_eq!(classify_member(&session(SessionStatus::Interrupted), false), MemberStatus::Stopped);
    }

    #[test]
    fn a_queued_session_is_queued_not_working() {
        assert_eq!(classify_member(&session(SessionStatus::Queued), false), MemberStatus::Queued);
    }

    #[test]
    fn awaiting_review_is_needs_you() {
        assert_eq!(classify_member(&session(SessionStatus::AwaitingReview), false), MemberStatus::NeedsYou);
    }

    #[test]
    fn terminal_status_wins_over_a_pending_user_request_or_blocked_activity() {
        let mut s = session(SessionStatus::Terminated);
        s.activity = ActivityState::Blocked;
        assert_eq!(classify_member(&s, true), MemberStatus::Stopped);
    }
}
