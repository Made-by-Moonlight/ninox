use crate::{
    config::AppConfig,
    events::{Engine, Event},
    github::{split_repo, CheckRun},
    hooks,
    lifecycle::{
        enrichment::EnrichmentCache,
        probe::is_pid_alive,
        usage,
    },
    types::{
        CIStatus, Comment, Notification, NotificationKind, PrId, SessionStatus, PR,
    },
};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

/// Unix epoch milliseconds "now" — used to stamp `Notification::created_at`.
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub struct Poller {
    engine:           Arc<Engine>,
    enrichment_cache: Arc<std::sync::Mutex<EnrichmentCache>>,
    /// Last-seen `(cost_usd, context_used_pct, context_total_tokens)` per
    /// session, used solely to detect changes written externally by the
    /// `ninox statusline` subcommand — see `poll_context_updates`.
    context_cache:    Arc<std::sync::Mutex<HashMap<String, (f64, Option<f64>, Option<u64>)>>>,
}

impl Poller {
    pub fn new(engine: Arc<Engine>) -> Self {
        Self {
            engine,
            enrichment_cache: Arc::new(std::sync::Mutex::new(HashMap::new())),
            context_cache:    Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    pub async fn start(self, token: CancellationToken) {
        let mut pid_interval    = tokio::time::interval(Duration::from_secs(5));
        let mut usage_interval  = tokio::time::interval(Duration::from_secs(10));
        let mut github_interval = tokio::time::interval(Duration::from_secs(30));
        // Prevent a missed tick from causing back-to-back polls.
        github_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        usage_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = token.cancelled()      => break,
                _ = pid_interval.tick()    => {
                    self.poll_pids().await;
                    self.poll_context_updates().await;
                }
                _ = usage_interval.tick()  => self.poll_usage().await,
                _ = github_interval.tick() => self.poll_github().await,
            }
        }
    }

    // ── PID liveness ────────────────────────────────────────────────────────

    async fn poll_pids(&self) {
        // Metadata first: a dying worker's last acts (PR create, work
        // request) are processed before the reap below marks it Terminated.
        self.sync_sessions_metadata(&AppConfig::sessions_dir()).await;

        let Ok(sessions) = self.engine.store.list_sessions() else { return };
        for mut session in sessions {
            if matches!(session.status, SessionStatus::Done | SessionStatus::Terminated) {
                continue;
            }
            if let Some(pid) = session.pid {
                if !is_pid_alive(pid) {
                    session.status = SessionStatus::Terminated;
                    let _ = self.engine.store.upsert_session(&session);
                    self.engine.emit(Event::SessionUpdated(session));
                }
            }
        }
    }

    // ── Session metadata (wrapper hooks + `ninox request-work`) ────────────

    /// One pass over every non-terminal session's metadata file: adopt the
    /// first reported PR as the session's canonical one, record + notify any
    /// PR opened beyond it, and deliver pending work requests to the
    /// orchestrator. The dir is a parameter so tests can drive this against
    /// a tempdir instead of `AppConfig::sessions_dir()`.
    async fn sync_sessions_metadata(&self, sessions_dir: &std::path::Path) {
        let Ok(sessions) = self.engine.store.list_sessions() else { return };
        for mut session in sessions {
            // Work requests are about *new* work, not this session — deliver
            // them even when the requesting worker has already finished or
            // died (a worker's last act is often "request follow-up, exit").
            self.deliver_work_requests(&session, sessions_dir).await;

            if matches!(session.status, SessionStatus::Done | SessionStatus::Terminated) {
                continue;
            }
            let Ok(meta) = hooks::read_session_metadata(sessions_dir, &session.id) else {
                continue;
            };

            // -- First reported PR becomes the session's tracked PR --
            if session.pr_number.is_none() {
                if let Some(first) = meta.pr_reports.first() {
                    session.pr_number = Some(first.number);
                    session.status    = SessionStatus::PrOpen;
                    let _ = self.engine.store.upsert_session(&session);
                    self.engine.emit(Event::SessionUpdated(session.clone()));
                    tracing::info!(
                        "session {} PR #{} detected via metadata hook",
                        session.id, first.number
                    );
                }
            }

            // -- Every reported PR beyond the tracked one --
            let Some(tracked) = session.pr_number else { continue };

            // Ledger rows first, every tick: a store error at notification
            // time must only defer the row to the next tick, never lose it.
            // Only write when the row id (bare PR number — collides across
            // repos) is free: never steal another session's row.
            for report in meta.pr_reports.iter().filter(|r| r.number != tracked) {
                if let Ok(None) = self.engine.store.get_pr(report.number as i64) {
                    let url = report.url.clone().unwrap_or_else(|| {
                        format!("https://github.com/{}/pull/{}", session.repo, report.number)
                    });
                    let _ = self.engine.store.upsert_pr(&PR {
                        id:         report.number as i64,
                        number:     report.number,
                        title:      String::new(),
                        url,
                        body:       String::new(),
                        session_id: session.id.clone(),
                    });
                }
            }

            // Notifications second, deduped via the poller-owned side file.
            let notified = hooks::read_notified_extra_prs(sessions_dir, &session.id);
            let mut fresh: Vec<(u64, Option<String>)> = Vec::new();
            for report in meta.pr_reports.iter()
                .filter(|r| r.number != tracked && !notified.contains(&r.number))
            {
                self.engine.emit(Event::Notification(Notification {
                    id:         format!("extra-pr-{}-{}", session.id, report.number),
                    kind:       NotificationKind::ExtraPr,
                    title:      format!("Extra PR — {}", session.name),
                    body:       format!("#{} opened beyond tracked #{tracked}", report.number),
                    session_id: Some(session.id.clone()),
                    created_at: now_millis(),
                }));
                fresh.push((report.number, report.url.clone()));
            }
            if fresh.is_empty() {
                continue;
            }
            let numbers: Vec<u64> = fresh.iter().map(|(n, _)| *n).collect();
            if let Err(e) = hooks::mark_extra_prs_notified(sessions_dir, &session.id, &numbers) {
                tracing::warn!("mark extra PRs notified for {}: {e}", session.id);
            }
            if let Some(orch) = session.orchestrator_id.clone() {
                let msg = crate::lifecycle::reactions::format_extra_pr_reaction(
                    &session, tracked, &fresh,
                );
                if let Err(e) = self.engine.send_to_session(&orch, &msg).await {
                    tracing::warn!("send extra-PR reaction to orchestrator {orch}: {e}");
                }
            }
        }
    }

    /// Forward every pending `ninox request-work` entry for this session to
    /// the UI and the orchestrator, then move it out of the pending set.
    async fn deliver_work_requests(&self, session: &crate::types::Session, sessions_dir: &std::path::Path) {
        let pending = match hooks::read_pending_work_requests(sessions_dir, &session.id) {
            Ok(p) if !p.is_empty() => p,
            _ => return,
        };
        for request in &pending {
            self.engine.emit(Event::Notification(Notification {
                id:         format!("work-request-{}", request.id),
                kind:       NotificationKind::WorkRequested,
                title:      format!("Work requested — {}", session.name),
                body:       request.description.clone(),
                session_id: Some(session.id.clone()),
                created_at: now_millis(),
            }));
            if let Some(orch) = session.orchestrator_id.clone() {
                let msg = crate::lifecycle::reactions::format_work_request_reaction(
                    session, &request.description,
                );
                if let Err(e) = self.engine.send_to_session(&orch, &msg).await {
                    tracing::warn!("send work request to orchestrator {orch}: {e}");
                }
            }
        }
        // Marked delivered even when the tmux nudge failed — the UI
        // notification is already out, and retrying every tick would spam
        // both channels.
        let ids: Vec<String> = pending.iter().map(|r| r.id.clone()).collect();
        if let Err(e) = hooks::mark_work_requests_delivered(sessions_dir, &session.id, &ids) {
            tracing::warn!("mark work requests delivered for {}: {e}", session.id);
        }
    }

    // ── Cost / context-window usage ─────────────────────────────────────────

    /// Ingest cost/token usage for every active session by reading `claude`'s
    /// own transcript for the session's workspace directory (see
    /// `lifecycle::usage`). Sessions without a workspace, or whose transcript
    /// has no usage yet (agent hasn't taken a turn), are left untouched.
    /// Only writes + emits when something actually changed, so this doesn't
    /// spam the store/UI every tick for idle sessions.
    async fn poll_usage(&self) {
        let Ok(sessions) = self.engine.store.list_sessions() else { return };
        for mut session in sessions {
            if matches!(session.status, SessionStatus::Done | SessionStatus::Terminated) {
                continue;
            }
            let Some(workspace) = session.workspace_path.clone() else { continue };
            let Some(snapshot) = usage::ingest_usage_for_workspace(&workspace) else { continue };

            let cost_changed = (session.cost_usd - snapshot.cost_usd).abs() > 1e-9;
            let context_changed = session.context_tokens != Some(snapshot.context_tokens);
            if !cost_changed && !context_changed {
                continue;
            }

            session.cost_usd = snapshot.cost_usd;
            session.context_tokens = Some(snapshot.context_tokens);
            if session.model.is_none() {
                session.model = snapshot.model;
            }
            let _ = self.engine.store.upsert_session(&session);
            self.engine.emit(Event::SessionUpdated(session));
        }
    }

    // ── Statusline-sourced cost/context updates (external writer) ──────────

    /// The `ninox statusline` subcommand (invoked by Claude Code's own
    /// `statusLine` hook — see `lifecycle::statusline`) writes cost/context
    /// fields directly into the store from a separate short-lived process.
    /// Unlike every other poll method, this data doesn't arrive via a
    /// read-modify-write cycle this poller drives, so there's nothing to
    /// diff against except a cache of the last-seen values. Detects
    /// external changes and re-broadcasts them as `SessionUpdated` so the
    /// GUI picks them up.
    async fn poll_context_updates(&self) {
        let Ok(sessions) = self.engine.store.list_sessions() else { return };
        let mut changed = Vec::new();
        {
            let mut cache = self.context_cache.lock().unwrap();
            for session in sessions {
                let key = (session.cost_usd, session.context_used_pct, session.context_total_tokens);
                // `None` means this session has never been cached — seed it
                // silently rather than treating "no prior state" as a change
                // (that would spam an event for every session on startup).
                if let Some(prev) = cache.insert(session.id.clone(), key) {
                    if prev != key {
                        changed.push(session);
                    }
                }
            }
        }
        for session in changed {
            self.engine.emit(Event::SessionUpdated(session));
        }
    }

    // ── GitHub enrichment ────────────────────────────────────────────────────

    async fn poll_github(&self) {
        let Some(gh) = &self.engine.github else { return };
        let Ok(sessions) = self.engine.store.list_sessions() else { return };

        for session in sessions {
            if matches!(session.status, SessionStatus::Done | SessionStatus::Terminated) {
                continue;
            }
            let Some(pr_number) = session.pr_number else { continue };
            let Some((owner, repo)) = split_repo(&session.repo) else { continue };

            // -- PR state --
            let pr_status = match gh.get_pr_status(&owner, &repo, pr_number).await {
                Ok(s)  => s,
                Err(e) => { tracing::warn!("github pr status: {e}"); continue }
            };

            let pr_id: PrId = pr_number as i64;

            // -- Merge detection — handle before CI (no point polling CI on merged PR) --
            if pr_status.merged && !matches!(session.status, SessionStatus::Done) {
                self.engine.emit(Event::Notification(Notification {
                    id:         format!("merged-{}", session.id),
                    kind:       NotificationKind::WorkerDone,
                    title:      format!("PR merged — {}", session.name),
                    body:       format!("#{} merged successfully", pr_number),
                    session_id: Some(session.id.clone()),
                    created_at: now_millis(),
                }));
                if let Err(e) = self.engine.cleanup_session(&session.id).await {
                    tracing::warn!("cleanup_session {}: {e}", session.id);
                }
                // Remove enrichment state for this session — it's done
                {
                    let mut cache = self.enrichment_cache.lock().unwrap();
                    cache.remove(&session.id);
                }
                continue; // skip further enrichment for this session
            }

            // Upsert PR record — only when not merged (merged sessions stay Done after cleanup)
            {
                let pr = PR {
                    id:         pr_id,
                    number:     pr_number,
                    title:      pr_status.title.clone(),
                    url:        format!("https://github.com/{owner}/{repo}/pull/{pr_number}"),
                    body:       String::new(),
                    session_id: session.id.clone(),
                };
                let _ = self.engine.store.upsert_pr(&pr);
                self.engine.emit(Event::PrOpened { session_id: session.id.clone(), pr });
            }

            // -- CI checks --
            let checks = match gh.get_ci_checks(&owner, &repo, &pr_status.head_sha).await {
                Ok(c)  => c,
                Err(e) => { tracing::warn!("github ci checks: {e}"); vec![] }
            };
            let ci = summarize_checks(pr_id, &checks);
            let _ = self.engine.store.upsert_ci_status(&ci);
            self.engine.emit(Event::CiUpdated { pr_id, status: ci.clone() });

            // -- Detect CI transition and update session status --
            let (newly_failing, ci_reaction_already_sent) = {
                let mut cache = self.enrichment_cache.lock().unwrap();
                let state = cache.entry(session.id.clone()).or_default();

                let newly_failing = state.prev_failing.is_none_or(|p| p == 0)
                    && ci.failing > 0;
                state.prev_failing = Some(ci.failing);

                let already_sent = state.ci_reaction_sent;
                if newly_failing && !already_sent {
                    state.ci_reaction_sent = true;
                }
                if ci.failing == 0 {
                    state.ci_reaction_sent = false;
                }
                (newly_failing, already_sent)
            };

            if newly_failing && !ci_reaction_already_sent {
                self.engine.emit(Event::Notification(Notification {
                    id:         format!("ci-{}", session.id),
                    kind:       NotificationKind::CiFailure,
                    title:      format!("CI failing — {}", session.name),
                    body:       format!("{}/{} checks failing", ci.failing, ci.total),
                    session_id: Some(session.id.clone()),
                    created_at: now_millis(),
                }));
                // Send reaction to the agent in the tmux session
                let failing_names: Vec<String> = checks.iter()
                    .filter(|c| c.conclusion.as_deref() == Some("failure")
                             || c.conclusion.as_deref() == Some("timed_out"))
                    .map(|c| c.name.clone())
                    .collect();
                let msg = crate::lifecycle::reactions::format_ci_reaction(
                    &session, &ci, &failing_names
                );
                if let Err(e) = self.engine.send_to_session(&session.id, &msg).await {
                    tracing::warn!("send ci reaction to {}: {e}", session.id);
                }
            }

            // -- Review threads (throttled via seen_comment_ids) --
            let threads = match gh.get_review_threads(&owner, &repo, pr_number).await {
                Ok(t)  => t,
                Err(e) => { tracing::warn!("github review threads: {e}"); vec![] }
            };

            let has_changes_requested = threads.iter().any(|t| t.state == "CHANGES_REQUESTED");

            let (has_new, review_reaction_already_sent, new_comments) = {
                let mut cache = self.enrichment_cache.lock().unwrap();
                let state = cache.entry(session.id.clone()).or_default();
                let mut has_new = false;
                let mut new_comments: Vec<Comment> = Vec::new();

                for thread in &threads {
                    if thread.state == "CHANGES_REQUESTED"
                        && !state.seen_comment_ids.contains(&thread.id)
                    {
                        state.seen_comment_ids.insert(thread.id);
                        has_new = true;
                        let comment = Comment {
                            id:         thread.id,
                            pr_id,
                            author:     thread.author.clone(),
                            body:       thread.body.clone(),
                            path:       thread.path.clone(),
                            line:       thread.line,
                            created_at: 0,
                        };
                        let _ = self.engine.store.upsert_comment(&comment);
                        self.engine.emit(Event::ReviewComment { pr_id, comment: comment.clone() });
                        new_comments.push(comment);
                    }
                }

                let already_sent = state.review_reaction_sent;
                if has_new && !already_sent {
                    state.review_reaction_sent = true;
                }
                // Reset when all CHANGES_REQUESTED are resolved
                if !has_changes_requested {
                    state.review_reaction_sent = false;
                }
                (has_new, already_sent, new_comments)
            };

            // Update session status in DB (after review threads so has_changes_requested is known)
            let new_status = derive_session_status(&session.status, &pr_status, &ci, has_changes_requested);
            let mut updated = session.clone();
            updated.status = new_status;
            if updated.status != session.status {
                let _ = self.engine.store.upsert_session(&updated);
                self.engine.emit(Event::SessionUpdated(updated.clone()));
            }

            if has_new && !review_reaction_already_sent {
                self.engine.emit(Event::Notification(Notification {
                    id:         format!("review-{}", session.id),
                    kind:       NotificationKind::PrNeedsAttention,
                    title:      format!("Review comments — {}", session.name),
                    body:       "Changes requested on your PR".to_string(),
                    session_id: Some(session.id.clone()),
                    created_at: now_millis(),
                }));
                if !new_comments.is_empty() {
                    let msg = crate::lifecycle::reactions::format_review_reaction(
                        &session, &new_comments
                    );
                    if let Err(e) = self.engine.send_to_session(&session.id, &msg).await {
                        tracing::warn!("send review reaction to {}: {e}", session.id);
                    }
                }
            }
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn summarize_checks(pr_id: PrId, checks: &[CheckRun]) -> CIStatus {
    let total   = checks.len() as u32;
    let failing = checks.iter().filter(|c| {
        c.conclusion.as_deref() == Some("failure")
            || c.conclusion.as_deref() == Some("timed_out")
    }).count() as u32;
    let passing = checks.iter().filter(|c| {
        c.conclusion.as_deref() == Some("success")
    }).count() as u32;
    let pending = total - failing - passing;
    CIStatus { pr_id, total, failing, passing, pending }
}

fn derive_session_status(
    current:               &SessionStatus,
    pr_status:             &crate::github::PrStatus,
    ci:                    &CIStatus,
    has_changes_requested: bool,
) -> SessionStatus {
    // Terminal states are never overwritten.
    if matches!(current, SessionStatus::Done | SessionStatus::Terminated) {
        return current.clone();
    }
    if pr_status.merged {
        return SessionStatus::Done;
    }
    if ci.failing > 0 {
        return SessionStatus::CiFailed;
    }
    if has_changes_requested {
        return SessionStatus::ReviewPending;
    }
    if pr_status.mergeable == Some(true) && ci.failing == 0 && ci.pending == 0 {
        return SessionStatus::Mergeable;
    }
    SessionStatus::PrOpen
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SessionStatus;

    #[test]
    fn summarize_checks_counts_failures() {
        let checks = vec![
            CheckRun { name: "lint".into(), status: "completed".into(), conclusion: Some("success".into()) },
            CheckRun { name: "test".into(), status: "completed".into(), conclusion: Some("failure".into()) },
            CheckRun { name: "build".into(), status: "in_progress".into(), conclusion: None },
        ];
        let ci = summarize_checks(1, &checks);
        assert_eq!(ci.total,   3);
        assert_eq!(ci.passing, 1);
        assert_eq!(ci.failing, 1);
        assert_eq!(ci.pending, 1);
    }

    #[test]
    fn derive_status_merged_becomes_done() {
        let pr = crate::github::PrStatus {
            merged: true, state: "closed".into(), mergeable: None,
            title: "t".into(), number: 1, head_sha: String::new(),
        };
        let ci = CIStatus { pr_id: 1, total: 0, failing: 0, passing: 0, pending: 0 };
        let s  = derive_session_status(&SessionStatus::PrOpen, &pr, &ci, false);
        assert!(matches!(s, SessionStatus::Done));
    }

    #[test]
    fn derive_status_ci_failure_overrides_open() {
        let pr = crate::github::PrStatus {
            merged: false, state: "open".into(), mergeable: Some(true),
            title: "t".into(), number: 1, head_sha: String::new(),
        };
        let ci = CIStatus { pr_id: 1, total: 3, failing: 1, passing: 2, pending: 0 };
        let s  = derive_session_status(&SessionStatus::PrOpen, &pr, &ci, false);
        assert!(matches!(s, SessionStatus::CiFailed));
    }

    #[test]
    fn derive_status_all_green_becomes_mergeable() {
        let pr = crate::github::PrStatus {
            merged: false, state: "open".into(), mergeable: Some(true),
            title: "t".into(), number: 1, head_sha: String::new(),
        };
        let ci = CIStatus { pr_id: 1, total: 3, failing: 0, passing: 3, pending: 0 };
        let s  = derive_session_status(&SessionStatus::PrOpen, &pr, &ci, false);
        assert!(matches!(s, SessionStatus::Mergeable));
    }

    #[test]
    fn derive_status_preserves_done() {
        let pr = crate::github::PrStatus {
            merged: false, state: "open".into(), mergeable: Some(true),
            title: "t".into(), number: 1, head_sha: String::new(),
        };
        let ci = CIStatus { pr_id: 1, total: 0, failing: 0, passing: 0, pending: 0 };
        let s  = derive_session_status(&SessionStatus::Done, &pr, &ci, false);
        assert!(matches!(s, SessionStatus::Done));
    }

    #[test]
    fn derive_status_preserves_terminated() {
        let pr = crate::github::PrStatus {
            merged: true, state: "closed".into(), mergeable: None,   // merged=true!
            title: "t".into(), number: 1, head_sha: String::new(),
        };
        let ci = CIStatus { pr_id: 1, total: 0, failing: 0, passing: 0, pending: 0 };
        let s  = derive_session_status(&SessionStatus::Terminated, &pr, &ci, false);
        assert!(matches!(s, SessionStatus::Terminated));  // must not become Done
    }

    #[test]
    fn derive_status_changes_requested_becomes_review_pending() {
        let pr = crate::github::PrStatus {
            merged: false, state: "open".into(), mergeable: Some(true),
            title: "t".into(), number: 1, head_sha: String::new(),
        };
        let ci = CIStatus { pr_id: 1, total: 3, failing: 0, passing: 3, pending: 0 };
        let s  = derive_session_status(&SessionStatus::PrOpen, &pr, &ci, true);
        assert!(matches!(s, SessionStatus::ReviewPending));
    }

    fn test_session(id: &str, workspace: &str) -> crate::types::Session {
        crate::types::Session {
            id: id.into(), orchestrator_id: None, name: id.into(),
            repo: String::new(), status: SessionStatus::Working,
            agent_type: "claude-code".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None,
            workspace_path: Some(workspace.into()), pid: None,
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
        }
    }

    /// End-to-end (within-process) proof that the poller closes the gap
    /// documented in `lifecycle::usage`: given a workspace whose `claude`
    /// transcript directory has usage recorded, `poll_usage` writes the
    /// derived cost/context/model back into the store and emits
    /// `SessionUpdated` — the exact path the UI's $0.0000 / missing-tokens
    /// symptom traces back to when this ingestion doesn't happen.
    // The `ENV_TEST_GUARD` mutex is intentionally held across the `.await`
    // points below — it serializes access to the process-global
    // `NINOX_CLAUDE_PROJECTS_DIR` env var against other tests (in this file
    // and in `lifecycle::usage`) for this single-threaded `#[tokio::test]`,
    // and must stay held for the env var's entire lifetime, not just around
    // the sync portions.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn poll_usage_ingests_transcript_into_store_and_emits_update() {
        use crate::{lifecycle::usage::{claude_project_slug, ENV_TEST_GUARD}, store::Store};
        use std::io::Write;

        let _guard = ENV_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let projects_dir = tempfile::tempdir().unwrap();
        let workspace = "/tmp/poller-usage-probe-workspace";
        let project_dir = projects_dir.path().join(claude_project_slug(workspace));
        std::fs::create_dir_all(&project_dir).unwrap();
        let mut f = std::fs::File::create(project_dir.join("s.jsonl")).unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","timestamp":"2026-07-05T13:00:00.000Z","message":{{"model":"claude-fable-5","usage":{{"input_tokens":2,"output_tokens":300,"cache_creation_input_tokens":500,"cache_read_input_tokens":45000}}}}}}"#
        ).unwrap();
        drop(f);

        let prior = std::env::var("NINOX_CLAUDE_PROJECTS_DIR").ok();
        std::env::set_var("NINOX_CLAUDE_PROJECTS_DIR", projects_dir.path());

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&test_session("s1", workspace)).unwrap();
        let engine = Engine::new(store.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        poller.poll_usage().await;

        match prior {
            Some(v) => std::env::set_var("NINOX_CLAUDE_PROJECTS_DIR", v),
            None    => std::env::remove_var("NINOX_CLAUDE_PROJECTS_DIR"),
        }

        let updated = store.get_session("s1").unwrap().unwrap();
        assert!(updated.cost_usd > 0.0, "cost_usd should be ingested, not 0.0000");
        assert_eq!(updated.context_tokens, Some(2 + 500 + 45000));
        assert_eq!(updated.model.as_deref(), Some("claude-fable-5"));

        let evt = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("SessionUpdated should be emitted")
            .unwrap();
        assert!(matches!(evt, Event::SessionUpdated(s) if s.id == "s1" && s.cost_usd > 0.0));
    }

    /// The `ninox statusline` subcommand (a separate short-lived process)
    /// writes cost/context fields directly into the store — outside any
    /// read-modify-write cycle this poller drives. This proves the diff
    /// cache detects that external write and re-broadcasts it, and that an
    /// untouched session generates no spurious event.
    #[tokio::test]
    async fn poll_context_updates_emits_only_for_changed_sessions() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut s1 = test_session("s1", "/ws1");
        let s2 = test_session("s2", "/ws2");
        store.upsert_session(&s1).unwrap();
        store.upsert_session(&s2).unwrap();
        let engine = Engine::new(store.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        // First tick establishes the baseline — nothing to diff against yet,
        // so it must not emit for sessions that already exist with no prior
        // cached state.
        poller.poll_context_updates().await;
        let baseline_events = drain_events(&mut rx);
        assert!(baseline_events.is_empty(), "no prior cached state means no change to report");

        // Simulate the statusline hook writing directly into the store for s1 only.
        s1.context_used_pct = Some(42.0);
        s1.cost_usd = 3.5;
        store.upsert_session(&s1).unwrap();

        poller.poll_context_updates().await;
        let events = drain_events(&mut rx);
        assert_eq!(events.len(), 1, "only the changed session should emit");
        assert!(matches!(
            &events[0],
            Event::SessionUpdated(s) if s.id == "s1" && s.context_used_pct == Some(42.0) && s.cost_usd == 3.5
        ));

        // A third tick with no further changes emits nothing.
        poller.poll_context_updates().await;
        assert!(drain_events(&mut rx).is_empty());
    }

    /// Drain every event currently buffered on the receiver.
    fn drain_events(rx: &mut tokio::sync::broadcast::Receiver<Event>) -> Vec<Event> {
        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        events
    }

    /// A worker that opened three PRs: the first becomes the session's
    /// tracked PR, every later one is recorded in the store and raised as an
    /// ExtraPr notification — and only once, however often the poller ticks.
    #[tokio::test]
    async fn metadata_sync_adopts_first_pr_and_flags_every_extra_once() {
        use crate::store::Store;

        let sessions_dir = tempfile::tempdir().unwrap();
        let meta = serde_json::json!({
            "agentReportedPrNumber": "44",
            "agentReportedPrUrl": "https://github.com/org/repo/pull/44",
            "agentReportedPrs": [
                {"number": "42", "url": "https://github.com/org/repo/pull/42"},
                {"number": "43", "url": "https://github.com/org/repo/pull/43"},
                {"number": "44", "url": "https://github.com/org/repo/pull/44"},
            ],
        });
        std::fs::write(
            sessions_dir.path().join("s1.json"),
            serde_json::to_string(&meta).unwrap(),
        ).unwrap();

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&test_session("s1", "/ws")).unwrap();
        let engine = Engine::new(store.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        poller.sync_sessions_metadata(sessions_dir.path()).await;

        let session = store.get_session("s1").unwrap().unwrap();
        assert_eq!(session.pr_number, Some(42), "first PR is the canonical one");
        assert!(matches!(session.status, SessionStatus::PrOpen));
        assert!(store.get_pr(43).unwrap().is_some(), "extra PR #43 recorded");
        assert!(store.get_pr(44).unwrap().is_some(), "extra PR #44 recorded");
        assert_eq!(
            store.get_pr(43).unwrap().unwrap().url,
            "https://github.com/org/repo/pull/43",
        );

        let events = drain_events(&mut rx);
        let extra_notifs: Vec<_> = events.iter().filter(|e| matches!(
            e, Event::Notification(n) if n.kind == crate::types::NotificationKind::ExtraPr
        )).collect();
        assert_eq!(extra_notifs.len(), 2, "one ExtraPr notification per extra PR");

        // Second tick: nothing new — no duplicate notifications.
        poller.sync_sessions_metadata(sessions_dir.path()).await;
        let events = drain_events(&mut rx);
        assert!(
            !events.iter().any(|e| matches!(e, Event::Notification(_))),
            "extra PRs must not be re-notified on every tick",
        );
    }

    /// A single reported PR (the normal case) adopts it with no extra-PR
    /// noise — the pre-existing first-PR-detection behavior.
    #[tokio::test]
    async fn metadata_sync_single_pr_has_no_extra_notifications() {
        use crate::store::Store;

        let sessions_dir = tempfile::tempdir().unwrap();
        let meta = serde_json::json!({
            "agentReportedPrNumber": "5",
            "agentReportedPrUrl": "https://github.com/org/repo/pull/5",
        });
        std::fs::write(
            sessions_dir.path().join("s1.json"),
            serde_json::to_string(&meta).unwrap(),
        ).unwrap();

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&test_session("s1", "/ws")).unwrap();
        let engine = Engine::new(store.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        poller.sync_sessions_metadata(sessions_dir.path()).await;

        let session = store.get_session("s1").unwrap().unwrap();
        assert_eq!(session.pr_number, Some(5));
        assert!(matches!(session.status, SessionStatus::PrOpen));
        let events = drain_events(&mut rx);
        assert!(!events.iter().any(|e| matches!(e, Event::Notification(_))));
    }

    /// Work requests recorded by `ninox request-work` surface exactly one
    /// WorkRequested notification each, then are marked delivered.
    #[tokio::test]
    async fn metadata_sync_delivers_work_requests_exactly_once() {
        use crate::store::Store;

        let sessions_dir = tempfile::tempdir().unwrap();
        hooks::append_work_request(sessions_dir.path(), "s1", "Migrate the config loader").unwrap();

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&test_session("s1", "/ws")).unwrap();
        let engine = Engine::new(store.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        poller.sync_sessions_metadata(sessions_dir.path()).await;

        let events = drain_events(&mut rx);
        let notif = events.iter().find_map(|e| match e {
            Event::Notification(n) if n.kind == crate::types::NotificationKind::WorkRequested => Some(n),
            _ => None,
        }).expect("WorkRequested notification emitted");
        assert!(notif.body.contains("Migrate the config loader"));
        assert_eq!(notif.session_id.as_deref(), Some("s1"));

        assert!(
            hooks::read_pending_work_requests(sessions_dir.path(), "s1").unwrap().is_empty(),
            "delivered requests must leave the pending set",
        );

        poller.sync_sessions_metadata(sessions_dir.path()).await;
        let events = drain_events(&mut rx);
        assert!(
            !events.iter().any(|e| matches!(e, Event::Notification(_))),
            "delivered work requests must not fire again",
        );
    }

    /// A worker can request work and exit before the next tick — the request
    /// must still reach the orchestrator, not die with the session.
    #[tokio::test]
    async fn metadata_sync_delivers_work_requests_from_terminated_sessions() {
        use crate::store::Store;

        let sessions_dir = tempfile::tempdir().unwrap();
        hooks::append_work_request(sessions_dir.path(), "s1", "Follow-up refactor").unwrap();

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut session = test_session("s1", "/ws");
        session.status = SessionStatus::Terminated;
        store.upsert_session(&session).unwrap();
        let engine = Engine::new(store.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        poller.sync_sessions_metadata(sessions_dir.path()).await;

        let events = drain_events(&mut rx);
        assert!(
            events.iter().any(|e| matches!(
                e, Event::Notification(n) if n.kind == crate::types::NotificationKind::WorkRequested
            )),
            "work requests outlive their session",
        );
    }

    /// The ledger row for an extra PR is best-effort at notification time (a
    /// busy store must not kill the alert) — but it must self-heal on later
    /// ticks rather than be lost forever, and healing must not re-notify.
    #[tokio::test]
    async fn extra_pr_ledger_row_backfills_after_notification_without_renotifying() {
        use crate::store::Store;

        let sessions_dir = tempfile::tempdir().unwrap();
        let meta = serde_json::json!({
            "agentReportedPrs": [
                {"number": "7", "url": "https://github.com/org/repo/pull/7"},
                {"number": "9", "url": "https://github.com/org/repo/pull/9"},
            ],
        });
        std::fs::write(
            sessions_dir.path().join("s1.json"),
            serde_json::to_string(&meta).unwrap(),
        ).unwrap();
        // Simulate "notified previously, but the row write failed that tick".
        hooks::mark_extra_prs_notified(sessions_dir.path(), "s1", &[9]).unwrap();

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&test_session("s1", "/ws")).unwrap();
        let engine = Engine::new(store.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        poller.sync_sessions_metadata(sessions_dir.path()).await;

        assert!(
            store.get_pr(9).unwrap().is_some(),
            "already-notified extra PR must still get its ledger row backfilled",
        );
        let events = drain_events(&mut rx);
        assert!(
            !events.iter().any(|e| matches!(
                e, Event::Notification(n) if n.kind == crate::types::NotificationKind::ExtraPr
            )),
            "backfilling the row must not re-notify",
        );
    }

    /// Extra-PR dedup must not be fooled by an unrelated session in another
    /// repo already owning the `prs` row for that number (prs.id is the bare
    /// PR number, which collides across repos) — and must not steal that row.
    #[tokio::test]
    async fn extra_pr_detection_survives_cross_repo_pr_number_collision() {
        use crate::store::Store;

        let sessions_dir = tempfile::tempdir().unwrap();
        let meta = serde_json::json!({
            "agentReportedPrs": [
                {"number": "7", "url": "https://github.com/org/repo-a/pull/7"},
                {"number": "9", "url": "https://github.com/org/repo-a/pull/9"},
            ],
        });
        std::fs::write(
            sessions_dir.path().join("s1.json"),
            serde_json::to_string(&meta).unwrap(),
        ).unwrap();

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&test_session("s1", "/ws")).unwrap();
        // Another repo's session already tracks its own PR #9.
        let other = PR {
            id: 9, number: 9, title: "other repo's PR".into(),
            url: "https://github.com/org/repo-b/pull/9".into(),
            body: String::new(), session_id: "other".into(),
        };
        store.upsert_pr(&other).unwrap();

        let engine = Engine::new(store.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        poller.sync_sessions_metadata(sessions_dir.path()).await;

        let events = drain_events(&mut rx);
        assert!(
            events.iter().any(|e| matches!(
                e, Event::Notification(n) if n.kind == crate::types::NotificationKind::ExtraPr
            )),
            "the collision must not suppress the extra-PR alert",
        );
        let row = store.get_pr(9).unwrap().unwrap();
        assert_eq!(row.session_id, "other", "the other repo's row must not be stolen");
        assert_eq!(row.url, "https://github.com/org/repo-b/pull/9");

        poller.sync_sessions_metadata(sessions_dir.path()).await;
        let events = drain_events(&mut rx);
        assert!(
            !events.iter().any(|e| matches!(e, Event::Notification(_))),
            "dedup must hold across ticks even without a prs row of our own",
        );
    }

    #[tokio::test]
    async fn poll_usage_leaves_sessions_without_workspace_or_usage_untouched() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut no_ws = test_session("no-ws", "/does/not/matter");
        no_ws.workspace_path = None;
        store.upsert_session(&no_ws).unwrap();
        let engine = Engine::new(store.clone());
        let poller = Poller::new(engine);

        poller.poll_usage().await;

        let unchanged = store.get_session("no-ws").unwrap().unwrap();
        assert_eq!(unchanged.cost_usd, 0.0);
        assert_eq!(unchanged.context_tokens, None);
    }
}
