use crate::types::{CIStatus, Comment, Session};

/// Format a CI failure reaction message to send to the agent.
/// Lists each failing check and instructs the agent to fix them.
pub fn format_ci_reaction(_session: &Session, ci: &CIStatus, failing_names: &[String]) -> String {
    let mut msg = format!(
        "[Athene] CI is failing on your PR ({}/{} checks). Please fix the following:\n",
        ci.failing, ci.total
    );
    for name in failing_names {
        msg.push_str(&format!("  - {name}\n"));
    }
    msg.push_str("\nRun the failing checks locally, fix the issues, and push your changes.");
    msg
}

/// Format a review comment reaction message to send to the agent.
/// Lists each new CHANGES_REQUESTED comment.
pub fn format_review_reaction(_session: &Session, comments: &[Comment]) -> String {
    let mut msg = format!(
        "[Athene] Your PR has {} new review comment{}. Please address them:\n",
        comments.len(),
        if comments.len() == 1 { "" } else { "s" }
    );
    for c in comments {
        let location = match (&c.path, c.line) {
            (Some(p), Some(l)) => format!("{p}:{l}"),
            (Some(p), None)    => p.clone(),
            _                  => "general".to_string(),
        };
        msg.push_str(&format!("\n[{}] {}: {}\n", location, c.author, c.body));
    }
    msg.push_str("\nAddress each comment, push your changes, and respond to the reviewer.");
    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{CIStatus, Comment, Session, SessionStatus};

    fn mock_session() -> Session {
        Session {
            id: "s1".into(), orchestrator_id: None, name: "my-fix".into(),
            repo: "org/repo".into(), status: SessionStatus::CiFailed,
            agent_type: "claude-code".into(), cost_usd: 0.0,
            started_at: 0, pr_number: Some(7), pr_id: Some(7),
            workspace_path: None, pid: None,
        }
    }

    #[test]
    fn ci_reaction_lists_failing_checks() {
        let session = mock_session();
        let ci = CIStatus { pr_id: 7, total: 3, failing: 1, passing: 2, pending: 0 };
        let msg = format_ci_reaction(&session, &ci, &["test-unit".to_string()]);
        assert!(msg.contains("1/3 checks"));
        assert!(msg.contains("test-unit"));
        assert!(msg.contains("fix the"));
    }

    #[test]
    fn review_reaction_includes_file_location() {
        let session = mock_session();
        let comments = vec![Comment {
            id: 1, pr_id: 7, author: "reviewer".into(),
            body: "Rename this variable".into(),
            path: Some("src/main.rs".into()), line: Some(42),
            created_at: 0,
        }];
        let msg = format_review_reaction(&session, &comments);
        assert!(msg.contains("src/main.rs:42"));
        assert!(msg.contains("Rename this variable"));
        assert!(msg.contains("reviewer"));
    }

    #[test]
    fn review_reaction_handles_general_comment() {
        let session = mock_session();
        let comments = vec![Comment {
            id: 2, pr_id: 7, author: "bot".into(),
            body: "Please add tests".into(),
            path: None, line: None, created_at: 0,
        }];
        let msg = format_review_reaction(&session, &comments);
        assert!(msg.contains("[general]"));
        assert!(msg.contains("Please add tests"));
    }
}
