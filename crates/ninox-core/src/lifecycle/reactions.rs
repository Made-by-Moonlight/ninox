use crate::types::{CIStatus, Comment, Session};

/// Format a CI failure reaction message to send to the agent.
/// Lists each failing check and instructs the agent to fix them.
pub fn format_ci_reaction(_session: &Session, ci: &CIStatus, failing_names: &[String]) -> String {
    let mut msg = format!(
        "[Ninox] CI is failing on your PR ({}/{} checks). Please fix the following:\n",
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
        "[Ninox] Your PR has {} new review comment{}. Please address them:\n",
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

/// Format a work-request reaction for the *orchestrator*: a worker found
/// additional work outside its task and wants a new worker spawned for it.
pub fn format_work_request_reaction(worker: &Session, description: &str) -> String {
    format!(
        "[Ninox] Worker `{id}` requested additional work it discovered outside its task:\n\n\
         {description}\n\n\
         If this should be done, spawn a new worker for it (`ninox spawn`). \
         Do not ask the requesting worker to widen its own PR — one worker, one task, one PR.",
        id = worker.id,
    )
}

/// Format an extra-PR reaction for the *orchestrator*: a worker opened PRs
/// beyond the one its session tracks. `extras` is every untracked (number,
/// url) pair, oldest first.
pub fn format_extra_pr_reaction(
    worker:     &Session,
    tracked_pr: u64,
    extras:     &[(u64, Option<String>)],
) -> String {
    let mut msg = format!(
        "[Ninox] Worker `{id}` opened {count} PR{plural} beyond its tracked PR #{tracked_pr}:\n",
        id     = worker.id,
        count  = extras.len(),
        plural = if extras.len() == 1 { "" } else { "s" },
    );
    for (number, url) in extras {
        match url {
            Some(u) => msg.push_str(&format!("  - #{number} ({u})\n")),
            None    => msg.push_str(&format!("  - #{number}\n")),
        }
    }
    msg.push_str(
        "\nOne worker, one PR — Ninox only tracks the first. Review each extra PR and \
         either close it or hand it to a dedicated worker so CI and reviews are tracked.",
    );
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
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
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
    fn work_request_reaction_tells_orchestrator_to_spawn() {
        let worker = mock_session();
        let msg = format_work_request_reaction(&worker, "Migrate the config loader to TOML");
        assert!(msg.contains("s1"), "must name the requesting worker");
        assert!(msg.contains("Migrate the config loader to TOML"));
        assert!(msg.contains("ninox spawn"), "must point at the spawn path, not the worker");
        assert!(msg.to_lowercase().contains("do not"), "must forbid widening the worker's scope");
    }

    #[test]
    fn extra_pr_reaction_names_every_extra_pr_and_the_tracked_one() {
        let worker = mock_session(); // tracked PR #7
        let extras = vec![
            (9u64,  Some("https://github.com/org/repo/pull/9".to_string())),
            (11u64, None),
        ];
        let msg = format_extra_pr_reaction(&worker, 7, &extras);
        assert!(msg.contains("#7"), "must name the tracked PR");
        assert!(msg.contains("#9") && msg.contains("#11"), "must list every extra PR");
        assert!(msg.contains("https://github.com/org/repo/pull/9"));
        assert!(msg.contains("s1"));
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
