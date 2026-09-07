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

/// Format a worker-done reaction for the *orchestrator*: fired automatically
/// by the poller when a worker's tracked PR is detected merged. This is a
/// code-level completion guarantee — unlike the notification/desktop alert,
/// it doesn't depend on the worker's own agent voluntarily reporting back
/// (via `ninox send`) before it exits. This wording is for the `[auto_reap]`
/// path, where the worker session is cleaned up at the same moment.
pub fn format_worker_done_reaction(worker: &Session, pr_number: u64) -> String {
    format!(
        "[Ninox] Worker `{id}`'s PR #{pr_number} merged.",
        id = worker.id,
    )
}

/// The merge-detected reaction when `[auto_reap]` is off (the default) and
/// the worker was deliberately kept alive: unlike
/// [`format_worker_done_reaction`], the session and its worktree still
/// exist, so the orchestrator is told it can run post-merge validation in
/// place — and that it now owns the cleanup (`--force` because the session
/// is still live).
pub fn format_worker_done_kept_alive_reaction(worker: &Session, pr_number: u64) -> String {
    format!(
        "{merged} The worker session is still alive with its worktree intact — \
         send it post-merge validation with `ninox send {id} \"...\"` if useful, \
         then reap it with `ninox reap {id} --force` when you're done with it.",
        merged = format_worker_done_reaction(worker, pr_number),
        id = worker.id,
    )
}

/// Format a worker-retired reaction for the *orchestrator*: fired by the
/// retention sweep (`Poller::sweep_retired_sessions`) when a worker session
/// is purged from the store without ever having gone through
/// `handle_merge_detection` — e.g. its process exited on its own, or it was
/// terminated directly, before its PR (if any) was detected merged. Unlike
/// `format_worker_done_reaction`, this does NOT imply the worker's work
/// landed — the orchestrator should check the PR's actual state.
pub fn format_worker_retired_reaction(worker: &Session) -> String {
    match worker.pr_number {
        Some(pr) => format!(
            "[Ninox] Worker `{id}` was cleaned up and its session is gone — \
             its PR #{pr} was not detected as merged, so check its state.",
            id = worker.id,
        ),
        None => format!(
            "[Ninox] Worker `{id}` was cleaned up and its session is gone \
             without ever opening a PR.",
            id = worker.id,
        ),
    }
}

/// The tail every non-terminal watched-PR reaction carries: the agent didn't
/// ask for this PR's CI or reviews as part of its own work — it registered a
/// watch — so the message has to say where the watch came from and how to
/// drop it. (Terminal reactions don't need it: the watch is already gone.)
fn watch_tail(repo: &str, pr_number: u64) -> String {
    format!(
        "\nThis is not your own PR — you registered this watch with `ninox open --pr`. \
         Run `ninox close --pr https://github.com/{repo}/pull/{pr_number}` to stop watching it.",
    )
}

/// A PR this session explicitly watches (`ninox open --pr`) merged or
/// closed. Distinct wording from `format_worker_done_reaction`, which is
/// about the session's OWN PR.
pub fn format_watched_pr_terminal(repo: &str, pr_number: u64, merged: bool) -> String {
    format!(
        "[Ninox] The PR you were watching, {repo}#{pr_number}, was {outcome} — \
         Ninox has stopped watching it.",
        outcome = if merged { "merged" } else { "closed without merging" },
    )
}

/// CI started failing on a watched PR. Mirrors `format_ci_reaction`, but says
/// up front that the PR isn't the agent's own — so the agent chases the right
/// checkout instead of assuming its worktree is broken.
pub fn format_watched_pr_ci(repo: &str, pr_number: u64, ci: &CIStatus, failing_names: &[String]) -> String {
    let mut msg = format!(
        "[Ninox] CI is failing on the PR you are watching, {repo}#{pr_number} \
         ({}/{} checks):\n",
        ci.failing, ci.total,
    );
    for name in failing_names {
        msg.push_str(&format!("  - {name}\n"));
    }
    msg.push_str(&watch_tail(repo, pr_number));
    msg
}

/// New CHANGES_REQUESTED review activity on a watched PR. Mirrors
/// `format_review_reaction`, with the same location-prefixed comment list.
pub fn format_watched_pr_review(repo: &str, pr_number: u64, comments: &[Comment]) -> String {
    let mut msg = format!(
        "[Ninox] The PR you are watching, {repo}#{pr_number}, has {} new review comment{}:\n",
        comments.len(),
        if comments.len() == 1 { "" } else { "s" },
    );
    for c in comments {
        let location = match (&c.path, c.line) {
            (Some(p), Some(l)) => format!("{p}:{l}"),
            (Some(p), None)    => p.clone(),
            _                  => "general".to_string(),
        };
        msg.push_str(&format!("\n[{}] {}: {}\n", location, c.author, c.body));
    }
    msg.push_str(&watch_tail(repo, pr_number));
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
            summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
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
    fn worker_done_kept_alive_reaction_hands_cleanup_to_the_orchestrator() {
        let worker = mock_session(); // id "s1"
        let msg = format_worker_done_kept_alive_reaction(&worker, 42);
        assert!(msg.contains("s1"), "must name the worker session");
        assert!(msg.contains("#42"), "must name the merged PR");
        assert!(msg.to_lowercase().contains("merged"));
        assert!(
            msg.to_lowercase().contains("alive"),
            "must say the worker survived its merge — the orchestrator can validate in place",
        );
        assert!(
            msg.contains("ninox reap s1 --force"),
            "must give the exact reap command — the session is live, so a bare reap would skip it",
        );
    }

    #[test]
    fn worker_done_reaction_names_worker_and_merged_pr() {
        let worker = mock_session(); // id "s1"
        let msg = format_worker_done_reaction(&worker, 42);
        assert!(msg.contains("s1"), "must name the worker session");
        assert!(msg.contains("#42"), "must name the merged PR");
        assert!(msg.to_lowercase().contains("merged"));
    }

    #[test]
    fn worker_retired_reaction_names_worker_and_pr_when_present() {
        let worker = mock_session(); // id "s1", pr_number Some(7)
        let msg = format_worker_retired_reaction(&worker);
        assert!(msg.contains("s1"), "must name the worker session");
        assert!(msg.contains("#7"), "must name the tracked PR");
        assert!(!msg.to_lowercase().contains("merged successfully"));
    }

    #[test]
    fn worker_retired_reaction_omits_pr_when_none() {
        let mut worker = mock_session();
        worker.pr_number = None;
        let msg = format_worker_retired_reaction(&worker);
        assert!(msg.contains("s1"));
        assert!(msg.contains("without ever opening a PR"));
    }

    #[test]
    fn watched_pr_terminal_distinguishes_merge_from_close() {
        let merged = format_watched_pr_terminal("o/r", 7, true);
        assert!(merged.contains("o/r#7"), "must name the watched PR");
        assert!(merged.to_lowercase().contains("merged"));

        let closed = format_watched_pr_terminal("o/r", 7, false);
        assert!(closed.contains("o/r#7"));
        assert!(closed.to_lowercase().contains("closed"));
        assert!(!closed.to_lowercase().contains(" merged"), "an unmerged close must not claim a merge");
    }

    #[test]
    fn watched_pr_ci_lists_checks_and_says_how_to_stop_watching() {
        let ci = CIStatus { pr_id: 7, total: 3, failing: 1, passing: 2, pending: 0 };
        let msg = format_watched_pr_ci("o/r", 7, &ci, &["test-unit".to_string()]);
        assert!(msg.contains("o/r#7"), "must name the watched PR");
        assert!(msg.contains("1/3 checks"));
        assert!(msg.contains("test-unit"));
        assert!(msg.contains("ninox open --pr"), "must remind the agent where the watch came from");
        assert!(
            msg.contains("ninox close --pr https://github.com/o/r/pull/7"),
            "must say exactly how to stop watching, got {msg:?}",
        );
    }

    #[test]
    fn watched_pr_review_includes_comments_and_says_how_to_stop_watching() {
        let comments = vec![Comment {
            id: 1, pr_id: 7, author: "reviewer".into(),
            body: "Rename this variable".into(),
            path: Some("src/main.rs".into()), line: Some(42),
            created_at: 0,
        }];
        let msg = format_watched_pr_review("o/r", 7, &comments);
        assert!(msg.contains("o/r#7"), "must name the watched PR");
        assert!(msg.contains("src/main.rs:42"));
        assert!(msg.contains("Rename this variable"));
        assert!(msg.contains("reviewer"));
        assert!(msg.contains("ninox open --pr"));
        assert!(msg.contains("ninox close --pr https://github.com/o/r/pull/7"));
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
