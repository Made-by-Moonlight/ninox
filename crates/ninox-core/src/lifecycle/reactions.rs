use crate::runtime::{self, SessionReactions};
use crate::types::{CIStatus, Comment, Session};
use crate::worker_type::SuccessCriterion;

/// One line per criterion for the worker-facing "Definition of done" block
/// injected into a spawn's brief (see `cli::run_spawn`).
fn criterion_line(c: &SuccessCriterion) -> String {
    match c {
        SuccessCriterion::PrMerged  => "Your pull request is merged.".to_string(),
        SuccessCriterion::PrOpened  => "You have opened a pull request.".to_string(),
        SuccessCriterion::ReportSent => {
            "You have sent your final report (`ninox complete` *is* that report).".to_string()
        }
        SuccessCriterion::Check(text) => text.clone(),
    }
}

/// The "Definition of done" block appended to a spawned worker's brief:
/// lists its resolved success criteria and makes the worker the one who
/// decides it is finished — `ninox complete` as its last action, which ends
/// the session. Empty `criteria` (an untracked/pre-this-change session
/// falling back to no explicit list) gets the same instruction.
pub fn format_definition_of_done(criteria: &[SuccessCriterion]) -> String {
    let mut msg = String::from("\n\n---\n**Definition of done:**\n");
    if criteria.is_empty() {
        msg.push_str("- Your task, including any post-merge steps in it, is finished.\n");
    } else {
        for c in criteria {
            msg.push_str(&format!("- {}\n", criterion_line(c)));
        }
    }
    msg.push_str(
        "\nYou are done when every item above is met **and** any post-merge steps in your task are \
         finished. Then run `ninox complete --summary \"<what you did>\" [--evidence <url|path>]…` as \
         your **last action**: it reports to your orchestrator and **ends this session**. Don't run it \
         while you still have work to do. If you are blocked, tell your orchestrator instead.",
    );
    msg
}

/// The one-shot reminder to a worker that went idle after its PR merged.
pub fn format_finish_after_merge_nudge(pr_number: Option<u64>) -> String {
    let pr = pr_number.map(|n| format!("#{n} ")).unwrap_or_default();
    format!(
        "[Ninox] Your PR {pr}merged and you have been idle since. If your task, including any \
         post-merge steps in your brief, is complete, finish now with \
         `ninox complete --summary \"<what you did>\"`; it ends your session. If work remains or you \
         are blocked, tell your orchestrator what is left with `ninox send`.",
    )
}

/// Format a CI failure reaction message to send to the agent. Dispatches
/// through the configured backend's [`SessionReactions`] impl (see the
/// limitation noted on [`runtime::reaction_behavior_for`]).
/// Lists each failing check and instructs the agent to fix them.
pub fn format_ci_reaction(session: &Session, ci: &CIStatus, failing_names: &[String]) -> String {
    ci_reaction_from(&*runtime::reaction_behavior_for(runtime::configured_backend()), session, ci, failing_names)
}

pub(crate) fn ci_reaction_from(behavior: &dyn SessionReactions, session: &Session, ci: &CIStatus, failing_names: &[String]) -> String {
    behavior.ci_failing(session, ci, failing_names)
}

/// Format a ready-to-merge reaction message to send to the agent. Mirrors
/// `format_ci_reaction`, but for the opposite transition: CI is green and
/// GitHub reports the PR mergeable. Dispatches through the configured
/// backend's [`SessionReactions`] impl.
pub fn format_ready_to_merge_reaction(session: &Session, ci: &CIStatus) -> String {
    ready_to_merge_reaction_from(&*runtime::reaction_behavior_for(runtime::configured_backend()), session, ci)
}

pub(crate) fn ready_to_merge_reaction_from(behavior: &dyn SessionReactions, session: &Session, ci: &CIStatus) -> String {
    behavior.ready_to_merge(session, ci)
}

/// Format a review comment reaction message to send to the agent.
/// Lists each new CHANGES_REQUESTED comment. Dispatches through the
/// configured backend's [`SessionReactions`] impl.
pub fn format_review_reaction(session: &Session, comments: &[Comment]) -> String {
    review_reaction_from(&*runtime::reaction_behavior_for(runtime::configured_backend()), session, comments)
}

pub(crate) fn review_reaction_from(behavior: &dyn SessionReactions, session: &Session, comments: &[Comment]) -> String {
    behavior.review_comments(session, comments)
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

/// Format the reaction delivered to a requester session once its `ninox
/// user-request` escalation has been answered by a human. The id is there
/// so the agent can confirm via `ninox user-request status <id>` — pane
/// text alone is keystroke-injectable.
pub fn format_user_request_answer_reaction(id: &str, prompt: &str, answer: &str) -> String {
    format!(
        "[Ninox] user-request {id} answered (confirm with `ninox user-request status {id}`):\n\n\
         Q: {prompt}\n\
         A: {answer}",
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

/// Wraps a worker's own `ninox complete --summary` report for its parent —
/// same convention as `format_work_request_reaction`: this is the worker's
/// own content, not a system-observed event.
pub fn format_completion_reaction(worker: &Session, summary: &str, evidence: &[String]) -> String {
    let mut msg = format!("[Ninox] Worker `{}` completed: {summary}", worker.id);
    if !evidence.is_empty() {
        msg.push_str("\n\nEvidence:\n");
        for e in evidence {
            msg.push_str(&format!("  - {e}\n"));
        }
    }
    msg
}

/// The merge-detected reaction when the worker's resolved criteria aren't
/// `PrMerged`-only-and-verifiable (e.g. an extra `Check` item) — the merge
/// alone doesn't finish the worker, so it's nudged to run `ninox complete`
/// itself rather than auto-reaped. Session and worktree are left untouched.
pub fn format_merged_needs_complete_reaction(worker: &Session, pr_number: u64) -> String {
    format!(
        "[Ninox] Worker `{}`'s PR #{pr_number} merged, but its Definition of done \
         has more to it — run `ninox complete` once the remaining criteria are met.",
        worker.id,
    )
}

/// Merge notice for a PR whose head branch couldn't be proven to be the
/// worker's own: deliberately advises no reap and no `--force`.
pub fn format_merged_unverified_reaction(worker: &Session, pr_number: u64) -> String {
    format!(
        "[Ninox] Worker `{}`'s PR #{pr_number} merged. The PR's branch could not be confirmed \
         as this worker's own, so the worker was left in place — verify before reaping.",
        worker.id,
    )
}

/// Retraction of an earlier merge/reap notice that was attributed to the
/// wrong PR (same number, different repo or branch).
pub fn format_merge_notice_retracted_reaction(worker: &Session, pr_number: u64) -> String {
    format!(
        "[Ninox] Correction: the earlier \"PR #{pr_number} merged\" notice for worker `{}` was \
         attributed to an unrelated PR. The worker is not done — do not reap it.",
        worker.id,
    )
}

/// Format a worker-done reaction for the *orchestrator*: fired automatically
/// by the poller when a worker's tracked PR is detected merged. This is a
/// code-level completion guarantee — unlike the notification/desktop alert,
/// it doesn't depend on the worker's own agent voluntarily reporting back
/// (via `ninox send`) before it exits. This wording is for the `reap_on_merge`
/// path, where the worker session is cleaned up at the same moment.
/// Dispatches through the configured backend's [`SessionReactions`] impl.
pub fn format_worker_done_reaction(worker: &Session, pr_number: u64) -> String {
    worker_done_reaction_from(&*runtime::reaction_behavior_for(runtime::configured_backend()), worker, pr_number)
}

pub(crate) fn worker_done_reaction_from(behavior: &dyn SessionReactions, worker: &Session, pr_number: u64) -> String {
    behavior.worker_done(worker, pr_number)
}

/// The merge-detected reaction when the worker is not reaped at merge (`self`/`manual`) and
/// the worker was deliberately kept alive: unlike
/// [`format_worker_done_reaction`], the session and its worktree still
/// exist, so the orchestrator is told it can run post-merge validation in
/// place — and that it now owns the cleanup (`--force` because the session
/// is still live). Dispatches through the configured backend's
/// [`SessionReactions`] impl.
pub fn format_worker_done_kept_alive_reaction(worker: &Session, pr_number: u64) -> String {
    worker_done_kept_alive_reaction_from(&*runtime::reaction_behavior_for(runtime::configured_backend()), worker, pr_number)
}

pub(crate) fn worker_done_kept_alive_reaction_from(behavior: &dyn SessionReactions, worker: &Session, pr_number: u64) -> String {
    behavior.worker_done_kept_alive(worker, pr_number)
}

/// Format a worker-retired reaction for the *orchestrator*: fired by the
/// retention sweep (`Poller::sweep_retired_sessions`) when a worker session
/// is purged from the store without ever having gone through
/// `handle_merge_detection` — e.g. its process exited on its own, or it was
/// terminated directly, before its PR (if any) was detected merged. Unlike
/// `format_worker_done_reaction`, this does NOT imply the worker's work
/// landed — the orchestrator should check the PR's actual state.
/// Dispatches through the configured backend's [`SessionReactions`] impl.
pub fn format_worker_retired_reaction(worker: &Session) -> String {
    worker_retired_reaction_from(&*runtime::reaction_behavior_for(runtime::configured_backend()), worker)
}

pub(crate) fn worker_retired_reaction_from(behavior: &dyn SessionReactions, worker: &Session) -> String {
    behavior.worker_retired(worker)
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

/// A watched PR became ready to merge. Mirrors `format_watched_pr_ci`, but
/// for the opposite transition — says up front that the PR isn't the
/// agent's own.
pub fn format_watched_pr_ready(repo: &str, pr_number: u64, ci: &CIStatus) -> String {
    format!(
        "[Ninox] The PR you are watching, {repo}#{pr_number}, is ready to merge — \
         all {} checks passing and mergeable.{tail}",
        ci.total,
        tail = watch_tail(repo, pr_number),
    )
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
    use crate::runtime::TmuxBackend;
    use crate::types::{CIStatus, Comment, Session, SessionStatus};

    /// A stub session-kind behavior with every method overridden to a
    /// distinct marker, proving reactions.rs dispatches generically through
    /// `SessionReactions` rather than matching on kind by hand — a future
    /// kind only needs its own impl, never a change to reactions.rs.
    struct StubReactions;
    impl SessionReactions for StubReactions {
        fn ci_failing(&self, _session: &Session, _ci: &CIStatus, _failing_names: &[String]) -> String {
            "stub:ci_failing".into()
        }
        fn ready_to_merge(&self, _session: &Session, _ci: &CIStatus) -> String {
            "stub:ready_to_merge".into()
        }
        fn review_comments(&self, _session: &Session, _comments: &[Comment]) -> String {
            "stub:review_comments".into()
        }
        fn worker_done(&self, _worker: &Session, _pr_number: u64) -> String {
            "stub:worker_done".into()
        }
        fn worker_done_kept_alive(&self, _worker: &Session, _pr_number: u64) -> String {
            "stub:worker_done_kept_alive".into()
        }
        fn worker_retired(&self, _worker: &Session) -> String {
            "stub:worker_retired".into()
        }
    }

    #[test]
    fn reactions_dispatch_generically_to_any_session_reactions_impl() {
        let session = mock_session();
        let ci = CIStatus { pr_id: 7, total: 3, failing: 1, passing: 2, pending: 0 };
        let stub = StubReactions;
        assert_eq!(ci_reaction_from(&stub, &session, &ci, &[]), "stub:ci_failing");
        assert_eq!(ready_to_merge_reaction_from(&stub, &session, &ci), "stub:ready_to_merge");
        assert_eq!(review_reaction_from(&stub, &session, &[]), "stub:review_comments");
        assert_eq!(worker_done_reaction_from(&stub, &session, 1), "stub:worker_done");
        assert_eq!(worker_done_kept_alive_reaction_from(&stub, &session, 1), "stub:worker_done_kept_alive");
        assert_eq!(worker_retired_reaction_from(&stub, &session), "stub:worker_retired");
    }

    /// Proves the public `format_*` functions actually exercise their
    /// `configured_backend()` → `reaction_behavior_for` → trait-method
    /// path, not just the `*_from` helpers tested above. Test binaries are
    /// always forced onto tmux (ptyd is disallowed without an isolated
    /// socket override, see `runtime::ptyd_allowed`), so the public
    /// functions must equal direct dispatch through `TmuxBackend`.
    #[test]
    fn public_format_functions_dispatch_through_the_real_configured_backend() {
        let _iso = crate::test_isolation::isolate();
        let session = mock_session();
        let ci = CIStatus { pr_id: 7, total: 3, failing: 1, passing: 2, pending: 0 };
        let comments = vec![Comment {
            id: 1, pr_id: 7, author: "reviewer".into(),
            body: "Rename this variable".into(),
            path: Some("src/main.rs".into()), line: Some(42),
            created_at: 0,
        }];
        let tmux = TmuxBackend;

        assert_eq!(
            format_ci_reaction(&session, &ci, &["test-unit".to_string()]),
            ci_reaction_from(&tmux, &session, &ci, &["test-unit".to_string()]),
        );
        assert_eq!(format_ready_to_merge_reaction(&session, &ci), ready_to_merge_reaction_from(&tmux, &session, &ci));
        assert_eq!(format_review_reaction(&session, &comments), review_reaction_from(&tmux, &session, &comments));
        assert_eq!(format_worker_done_reaction(&session, 42), worker_done_reaction_from(&tmux, &session, 42));
        assert_eq!(
            format_worker_done_kept_alive_reaction(&session, 42),
            worker_done_kept_alive_reaction_from(&tmux, &session, 42),
        );
        assert_eq!(format_worker_retired_reaction(&session), worker_retired_reaction_from(&tmux, &session));
    }

    fn mock_session() -> Session {
        Session {
            id: "s1".into(), orchestrator_id: None, name: "my-fix".into(),
            repo: "org/repo".into(), status: SessionStatus::CiFailed,
            agent_type: "claude-code".into(), cost_usd: 0.0,
            started_at: 0, pr_number: Some(7), pr_id: Some(7),
            workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
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

    #[test]
    fn ci_reaction_lists_failing_checks() {
        let _iso = crate::test_isolation::isolate();
        let session = mock_session();
        let ci = CIStatus { pr_id: 7, total: 3, failing: 1, passing: 2, pending: 0 };
        let msg = format_ci_reaction(&session, &ci, &["test-unit".to_string()]);
        assert!(msg.contains("1/3 checks"));
        assert!(msg.contains("test-unit"));
        assert!(msg.contains("fix the"));
    }

    #[test]
    fn review_reaction_includes_file_location() {
        let _iso = crate::test_isolation::isolate();
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
    fn definition_of_done_lists_every_criterion() {
        use crate::worker_type::SuccessCriterion;
        let msg = format_definition_of_done(&[
            SuccessCriterion::PrMerged,
            SuccessCriterion::Check("docs updated".into()),
        ]);
        assert!(msg.to_lowercase().contains("merged"));
        assert!(msg.contains("docs updated"));
    }

    fn assert_worker_owns_the_finish(msg: &str) {
        assert!(msg.contains("ninox complete --summary"), "{msg}");
        assert!(msg.contains("**last action**"), "{msg}");
        assert!(msg.contains("**ends this session**"), "{msg}");
        assert!(msg.contains("post-merge steps"), "{msg}");
        assert!(msg.contains("Don't run it while you still have work to do"), "{msg}");
        assert!(msg.contains("tell your orchestrator instead"), "{msg}");
    }

    #[test]
    fn definition_of_done_tells_the_worker_to_run_complete() {
        use crate::worker_type::SuccessCriterion;
        assert_worker_owns_the_finish(&format_definition_of_done(&[SuccessCriterion::ReportSent]));
    }

    #[test]
    fn definition_of_done_handles_empty_criteria() {
        let msg = format_definition_of_done(&[]);
        assert!(!msg.is_empty());
        assert_worker_owns_the_finish(&msg);
    }

    #[test]
    fn kept_alive_notice_says_the_worker_finishes_itself_and_drops_immediate_force() {
        let _iso = crate::test_isolation::isolate();
        let msg = format_worker_done_kept_alive_reaction(&mock_session(), 42);
        assert!(msg.contains("told to finish itself with `ninox complete`"), "{msg}");
        assert!(msg.contains("only if it doesn't, or if you don't need it"), "{msg}");
        assert!(!msg.contains("when you're done with it"), "{msg}");
    }

    #[test]
    fn finish_after_merge_nudge_names_complete_and_the_escape_hatch() {
        let msg = format_finish_after_merge_nudge(Some(42));
        assert!(msg.contains("#42") && msg.contains("ninox complete --summary") && msg.contains("ninox send"));
    }

    #[test]
    fn completion_reaction_names_worker_includes_summary_and_evidence() {
        let worker = mock_session(); // id "s1"
        let msg = format_completion_reaction(&worker, "fixed the thing", &["https://x/pr/1".to_string()]);
        assert!(msg.contains("s1"), "must name the worker session");
        assert!(msg.contains("fixed the thing"));
        assert!(msg.contains("https://x/pr/1"));
    }

    #[test]
    fn completion_reaction_omits_evidence_section_when_empty() {
        let worker = mock_session();
        let msg = format_completion_reaction(&worker, "done", &[]);
        assert!(msg.contains("done"));
        assert!(!msg.to_lowercase().contains("evidence"));
    }

    #[test]
    fn merged_needs_complete_reaction_names_worker_pr_and_says_run_complete() {
        let worker = mock_session(); // id "s1"
        let msg = format_merged_needs_complete_reaction(&worker, 42);
        assert!(msg.contains("s1"), "must name the worker session");
        assert!(msg.contains("#42"), "must name the merged PR");
        assert!(msg.to_lowercase().contains("merged"));
        assert!(msg.contains("ninox complete"));
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
    fn format_user_request_answer_reaction_includes_prompt_and_answer() {
        let msg = format_user_request_answer_reaction("ur-42", "deploy now?", "yes, go ahead");
        assert!(msg.contains("deploy now?"));
        assert!(msg.contains("yes, go ahead"));
        assert!(msg.contains("user-request ur-42 answered"), "{msg}");
        assert!(msg.contains("ninox user-request status ur-42"), "{msg}");
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
        let _iso = crate::test_isolation::isolate();
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
        let _iso = crate::test_isolation::isolate();
        let worker = mock_session(); // id "s1"
        let msg = format_worker_done_reaction(&worker, 42);
        assert!(msg.contains("s1"), "must name the worker session");
        assert!(msg.contains("#42"), "must name the merged PR");
        assert!(msg.to_lowercase().contains("merged"));
    }

    #[test]
    fn worker_retired_reaction_names_worker_and_pr_when_present() {
        let _iso = crate::test_isolation::isolate();
        let worker = mock_session(); // id "s1", pr_number Some(7)
        let msg = format_worker_retired_reaction(&worker);
        assert!(msg.contains("s1"), "must name the worker session");
        assert!(msg.contains("#7"), "must name the tracked PR");
        assert!(!msg.to_lowercase().contains("merged successfully"));
    }

    #[test]
    fn worker_retired_reaction_omits_pr_when_none() {
        let _iso = crate::test_isolation::isolate();
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
        let _iso = crate::test_isolation::isolate();
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
