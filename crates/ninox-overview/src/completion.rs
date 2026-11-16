//! A session's "Definition of done" and "Outcome", as plain display data —
//! shared by the GUI's and TUI's detail views.

use ninox_core::types::{CompletionOutcome, Session, SessionStatus};
use ninox_core::worker_type::SuccessCriterion;

/// Whether `criterion` already reads as met, purely from `session`'s own
/// fields — `ReportSent`/`Check` are attested, not independently checkable,
/// so both are only "done" once `ninox complete` has actually stored an
/// outcome.
pub fn criterion_done(session: &Session, criterion: &SuccessCriterion) -> bool {
    match criterion {
        SuccessCriterion::PrOpened => session.pr_number.is_some(),
        SuccessCriterion::PrMerged => session.merged_at.is_some() || session.status == SessionStatus::Done,
        SuccessCriterion::ReportSent | SuccessCriterion::Check(_) => session.completion_outcome.is_some(),
    }
}

pub fn criterion_label(criterion: &SuccessCriterion) -> String {
    match criterion {
        SuccessCriterion::PrMerged => "Pull request merged".to_string(),
        SuccessCriterion::PrOpened => "Pull request opened".to_string(),
        SuccessCriterion::ReportSent => "Final report sent".to_string(),
        SuccessCriterion::Check(text) => text.clone(),
    }
}

/// `(done, label)` per resolved success criterion, in order.
pub fn definition_of_done(session: &Session) -> Vec<(bool, String)> {
    session.success_criteria.iter().map(|c| (criterion_done(session, c), criterion_label(c))).collect()
}

/// The outcome's summary first, then each evidence entry.
pub fn outcome_lines(outcome: &CompletionOutcome) -> Vec<String> {
    let mut lines = vec![outcome.summary.clone()];
    lines.extend(outcome.evidence.iter().cloned());
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::types::ActivityState;

    fn session_with_criteria(success_criteria: Vec<SuccessCriterion>, completion_outcome: Option<CompletionOutcome>) -> Session {
        Session {
            id: "s".into(), orchestrator_id: None, name: "s-name".into(), repo: "o/r".into(),
            status: SessionStatus::Working, agent_type: "claude-code".into(), cost_usd: 0.0,
            started_at: 0, pr_number: None, pr_id: None, workspace_path: None,
            pid: None, model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None, context_used_pct: None,
            context_total_tokens: None, context_window_size: None, claude_session_id: None, summary: None,
            terminal_at: None, gate_status: None, merged_at: None, activity: ActivityState::Idle,
            activity_note: None, activity_since: None, machine_id: None, worker_type: None,
            success_criteria, completion_outcome, self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        }
    }

    #[test]
    fn unmet_criteria_are_not_done() {
        let session = session_with_criteria(vec![SuccessCriterion::PrMerged, SuccessCriterion::Check("docs updated".into())], None);
        let lines = definition_of_done(&session);
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|(done, _)| !done), "neither criterion is met yet");
        let labels: Vec<&str> = lines.iter().map(|(_, l)| l.as_str()).collect();
        assert!(labels.iter().any(|l| l.to_lowercase().contains("merged")));
        assert!(labels.contains(&"docs updated"));
    }

    #[test]
    fn pr_opened_and_merged_read_from_session_state() {
        let mut session = session_with_criteria(vec![SuccessCriterion::PrOpened, SuccessCriterion::PrMerged], None);
        session.pr_number = Some(7);
        session.merged_at = Some(123);
        assert!(definition_of_done(&session).iter().all(|(done, _)| *done), "both criteria are satisfied by session state");
    }

    #[test]
    fn report_sent_is_done_only_once_an_outcome_is_stored() {
        let mut session = session_with_criteria(vec![SuccessCriterion::ReportSent], None);
        assert!(!definition_of_done(&session)[0].0);
        session.completion_outcome = Some(CompletionOutcome { summary: "done".into(), evidence: vec![], completed_at: 0 });
        assert!(definition_of_done(&session)[0].0);
    }

    #[test]
    fn outcome_lines_put_the_summary_before_the_evidence() {
        let outcome = CompletionOutcome { summary: "all done here".into(), evidence: vec!["https://example.com/pr".into()], completed_at: 0 };
        assert_eq!(outcome_lines(&outcome), vec!["all done here".to_string(), "https://example.com/pr".to_string()]);
    }
}
