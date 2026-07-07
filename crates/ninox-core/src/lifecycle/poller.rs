use crate::{
    config::AppConfig,
    events::{Engine, Event},
    github::{split_repo, CheckRun},
    hooks,
    lifecycle::{
        brain_harvest::{self, ClaudeHarvestRunner, HarvestRunner},
        enrichment::EnrichmentCache,
        probe::is_pid_alive,
        usage,
    },
    types::{
        CIStatus, Comment, Notification, NotificationKind, PrId, Session, SessionStatus, PR,
    },
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio_util::sync::CancellationToken;

/// Last-seen `(cost_usd, context_used_pct, context_total_tokens)` snapshot per session.
/// Used by `poll_context_updates` to detect external changes.
type ContextSnapshot = (f64, Option<f64>, Option<u64>);

/// Unix epoch milliseconds "now" — used to stamp `Notification::created_at`.
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Best-effort extraction of a human-readable message from a
/// `JoinError::into_panic()` payload — used so a panicking `HarvestRunner`
/// is diagnosable in logs instead of silently swallowed.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

pub struct Poller {
    engine:           Arc<Engine>,
    enrichment_cache: Arc<std::sync::Mutex<EnrichmentCache>>,
    /// Last-seen `(cost_usd, context_used_pct, context_total_tokens)` per
    /// session, used solely to detect changes written externally by the
    /// `ninox statusline` subcommand — see `poll_context_updates`.
    context_cache:    Arc<std::sync::Mutex<HashMap<String, ContextSnapshot>>>,
    /// Runs the brain-harvest subprocess (real `claude -p` in production).
    /// Injectable so tests can fake success/failure without spawning a real
    /// process — see `sync_sessions_metadata`'s `trigger_brain_harvest`.
    harvest_runner:   Arc<dyn HarvestRunner>,
    /// One lock per resolved brain vault path, created on first use. Held
    /// across a harvest's `HarvestRunner::run` call so two sessions whose
    /// harvests target the same vault (the common case: both on the global
    /// default catalogue) never run their `claude -p` — and its `ninox
    /// brain index` — concurrently against it. Never pruned: the number of
    /// distinct vault paths in play is bounded by the number of configured
    /// catalogues, not by session count.
    vault_locks:      Arc<std::sync::Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>>,
}

impl Poller {
    pub fn new(engine: Arc<Engine>) -> Self {
        Self::new_with_harvest_runner(engine, Arc::new(ClaudeHarvestRunner))
    }

    pub fn new_with_harvest_runner(engine: Arc<Engine>, harvest_runner: Arc<dyn HarvestRunner>) -> Self {
        Self {
            engine,
            enrichment_cache: Arc::new(std::sync::Mutex::new(HashMap::new())),
            context_cache:    Arc::new(std::sync::Mutex::new(HashMap::new())),
            harvest_runner,
            vault_locks:      Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// The (created-on-first-use) lock for a given vault path — see
    /// `vault_locks`.
    fn vault_lock(&self, path: &Path) -> Arc<tokio::sync::Mutex<()>> {
        // Canonicalize so two syntactically different paths to the same
        // physical vault (trailing slash, symlink) share one lock. Falls
        // back to the raw path when it doesn't exist yet (e.g. a vault
        // that hasn't been written to before) — that harvest still gets
        // its own lock, just not deduplicated against a not-yet-existing
        // twin, which can't race with anything yet either.
        let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let mut locks = self.vault_locks.lock().unwrap_or_else(|e| e.into_inner());
        locks
            .entry(key)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
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
                _ = github_interval.tick() => {
                    // Reconciliation first: a session whose PR the poller
                    // hasn't adopted yet has no `pr_number` for `poll_github`
                    // to enrich, so it must run before (not instead of) it.
                    self.poll_pr_reconciliation().await;
                    self.poll_github().await;
                }
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
            if matches!(session.status, SessionStatus::Done | SessionStatus::Terminated | SessionStatus::Interrupted) {
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

            if matches!(session.status, SessionStatus::Done | SessionStatus::Terminated | SessionStatus::Interrupted) {
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
                    self.trigger_brain_harvest(&session).await;
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

    /// Fire a background brain-harvest attempt for a session whose PR was
    /// just detected. Reuses the caller's `pr_number.is_none()` guard as its
    /// only dedup — this is called from exactly one call site, itself
    /// guaranteed to fire once per session lifetime, so no second dedup
    /// layer is needed here.
    ///
    /// Diff computation (a couple of local `git` subprocess calls) AND the
    /// `claude -p` subprocess itself both run inside a single `tokio::spawn`
    /// — nothing about harvesting, however slow, may stall this poll tick's
    /// processing of other sessions. Any failure (disabled config, no
    /// workspace, trivial diff, or the subprocess itself failing) is logged
    /// at most and never propagates back into `sync_sessions_metadata`. A
    /// second, supervising spawn awaits the harvest task purely to log a
    /// panic that would otherwise be silent.
    async fn trigger_brain_harvest(&self, session: &Session) {
        let config = AppConfig::load().unwrap_or_default();
        if !config.brain_harvest.enabled {
            return;
        }
        let Some(workspace) = session.workspace_path.clone() else { return };
        let workspace_path: PathBuf = workspace.into();
        // Prefer the session's own catalogue — set from that worker's
        // `NINOX_BRAIN` at spawn time (see `main.rs::run_spawn`,
        // `spawn_util::interactive_env_vars`) — over the global default, so
        // the harvest writes to the same vault the worker itself thinks
        // with, not always the default catalogue.
        let brain_path: PathBuf = session.catalogue_path.clone()
            .map(PathBuf::from)
            .unwrap_or_else(|| config.resolved_brain_path());
        let runner       = self.harvest_runner.clone();
        let session_id    = session.id.clone();
        let panic_session_id = session_id.clone();
        let vault_lock   = self.vault_lock(&brain_path);

        let handle = tokio::spawn(async move {
            let Some(diff) = brain_harvest::compute_nontrivial_diff(&workspace_path).await else {
                tracing::info!("brain harvest skipped for {session_id}: no non-trivial diff");
                return;
            };
            let prompt = brain_harvest::build_harvest_prompt(&session_id, &diff);

            // Serialize concurrent harvests that share a vault — two
            // `claude -p` subprocesses running `ninox brain index` against
            // the same vault at once can race on the index write.
            let _guard = vault_lock.lock().await;
            if let Err(e) = runner.run(prompt, workspace_path, brain_path).await {
                tracing::warn!("brain harvest failed for session {session_id}: {e}");
            }
        });

        tokio::spawn(async move {
            if let Err(join_err) = handle.await {
                if join_err.is_panic() {
                    let payload = join_err.into_panic();
                    tracing::warn!(
                        "brain harvest task panicked for session {panic_session_id}: {}",
                        panic_message(payload.as_ref()),
                    );
                } else {
                    tracing::warn!("brain harvest task for session {panic_session_id} was cancelled");
                }
            }
        });
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
            if matches!(session.status, SessionStatus::Done | SessionStatus::Terminated | SessionStatus::Interrupted) {
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

        for mut session in sessions {
            if matches!(session.status, SessionStatus::Done | SessionStatus::Terminated | SessionStatus::Interrupted) {
                continue;
            }
            let Some(pr_number) = session.pr_number else { continue };

            // -- PR state — try the repo on record first (the common case,
            // no extra requests, and the only case where reusing the same
            // numeric `pr_number` is valid: it was recorded *for that repo
            // specifically*). If that 404s, DO NOT retry the same
            // `pr_number` against another remote's repo — PR numbers are a
            // per-repository sequence with no cross-repo relationship, so a
            // different repo (e.g. an internal mirror) can easily have some
            // unrelated PR at that same number. Instead, match on the
            // session's actual branch, the same way `poll_pr_reconciliation`
            // does, and adopt whatever PR number that repo's branch match
            // actually has. --
            let mut found: Option<(String, u64, crate::github::PrStatus)> = None;
            let mut last_err: Option<anyhow::Error> = None;
            let mut attempted = false;
            if !session.repo.is_empty() {
                if let Some((owner, repo)) = split_repo(&session.repo) {
                    attempted = true;
                    match gh.get_pr_status(&owner, &repo, pr_number).await {
                        Ok(s)  => found = Some((session.repo.clone(), pr_number, s)),
                        Err(e) => last_err = Some(e),
                    }
                }
            }
            if found.is_none() {
                if let Some(workspace) = session.workspace_path.clone() {
                    if let Some(branch) = crate::github::current_branch(&workspace) {
                        for repo_slug in crate::github::candidate_repos(&workspace) {
                            if repo_slug == session.repo {
                                continue; // already tried above
                            }
                            let Some((owner, repo)) = split_repo(&repo_slug) else { continue };
                            attempted = true;
                            let pr_ref = match gh.find_open_pr_for_branch(&owner, &repo, &branch).await {
                                Ok(Some(r)) => r,
                                Ok(None)    => continue,
                                Err(e)      => { last_err = Some(e); continue; }
                            };
                            match gh.get_pr_status(&owner, &repo, pr_ref.number).await {
                                Ok(s)  => { found = Some((repo_slug, pr_ref.number, s)); break; }
                                Err(e) => { last_err = Some(e); continue; }
                            }
                        }
                    }
                }
            }
            if !attempted {
                continue; // nothing parseable to check — same as pre-fallback behavior
            }
            let Some((resolved_repo, pr_number, pr_status)) = found else {
                if let Some(e) = last_err {
                    tracing::warn!("github pr status for {}: {e}", session.id);
                }
                self.notify_github_lookup_failed(&session);
                continue;
            };
            self.clear_github_lookup_failed(&session.id);

            // Self-heal: the PR was found against a different remote (and
            // possibly a different PR number in that remote's own sequence)
            // than the one on record — persist it so future ticks go
            // straight there instead of re-discovering it via fallback
            // every time.
            if resolved_repo != session.repo || Some(pr_number) != session.pr_number {
                tracing::info!(
                    "session {} repo/PR corrected {}#{:?} -> {resolved_repo}#{pr_number}",
                    session.id, session.repo, session.pr_number,
                );
                session.repo = resolved_repo;
                session.pr_number = Some(pr_number);
                let _ = self.engine.store.upsert_session(&session);
            }
            let Some((owner, repo)) = split_repo(&session.repo) else { continue };

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

    /// Emit a `GithubLookupFailed` notification once per run of consecutive
    /// failures — deduped the same way `ci_reaction_sent` dedupes CI-failure
    /// reactions, via the enrichment cache.
    fn notify_github_lookup_failed(&self, session: &crate::types::Session) {
        let already_notified = {
            let mut cache = self.enrichment_cache.lock().unwrap();
            let state = cache.entry(session.id.clone()).or_default();
            let already = state.github_lookup_failed_notified;
            state.github_lookup_failed_notified = true;
            already
        };
        if already_notified {
            return;
        }
        self.engine.emit(Event::Notification(Notification {
            id:         format!("github-lookup-failed-{}", session.id),
            kind:       NotificationKind::GithubLookupFailed,
            title:      format!("GitHub lookup failing — {}", session.name),
            body:       "PR status/CI/review polling failed against every configured remote"
                .to_string(),
            session_id: Some(session.id.clone()),
            created_at: now_millis(),
        }));
    }

    /// Clear the dedup flag once a lookup succeeds again — the next failure
    /// (if any) gets its own fresh notification.
    fn clear_github_lookup_failed(&self, session_id: &str) {
        let mut cache = self.enrichment_cache.lock().unwrap();
        if let Some(state) = cache.get_mut(session_id) {
            state.github_lookup_failed_notified = false;
        }
    }

    // ── PR reconciliation (active fallback) ─────────────────────────────────

    /// For every non-terminal session that has no tracked PR yet, actively
    /// check whether a PR already exists for its branch — independent of
    /// whether the wrapped `gh pr create` ever ran (a manual `git push` +
    /// PR opened via the GitHub web UI, `hub`, a shell alias, or the wrapper
    /// simply missing an unrecognized `gh` invocation shape all leave
    /// `pr_number` at `None` forever without this). Tries every configured
    /// remote, not just `origin`, for the same dual-remote reason as
    /// `poll_github`'s fallback.
    async fn poll_pr_reconciliation(&self) {
        let Some(gh) = &self.engine.github else { return };
        let Ok(sessions) = self.engine.store.list_sessions() else { return };

        for mut session in sessions {
            if session.pr_number.is_some() {
                continue;
            }
            if matches!(session.status, SessionStatus::Done | SessionStatus::Terminated | SessionStatus::Interrupted) {
                continue;
            }
            let Some(workspace) = session.workspace_path.clone() else { continue };
            let Some(branch) = crate::github::current_branch(&workspace) else { continue };

            for repo_slug in crate::github::candidate_repos(&workspace) {
                let Some((owner, repo)) = split_repo(&repo_slug) else { continue };
                match gh.find_open_pr_for_branch(&owner, &repo, &branch).await {
                    Ok(Some(pr_ref)) => {
                        session.pr_number = Some(pr_ref.number);
                        session.repo      = repo_slug.clone();
                        session.status    = SessionStatus::PrOpen;
                        let _ = self.engine.store.upsert_session(&session);
                        self.engine.emit(Event::SessionUpdated(session.clone()));
                        tracing::info!(
                            "session {} PR #{} detected via reconciliation ({repo_slug}, branch {branch})",
                            session.id, pr_ref.number,
                        );
                        break;
                    }
                    Ok(None) => continue,
                    Err(e) => {
                        tracing::warn!("reconciliation lookup {repo_slug} branch {branch}: {e}");
                        continue;
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
    if matches!(current, SessionStatus::Done | SessionStatus::Terminated | SessionStatus::Interrupted) {
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
            claude_session_id: None,
        }
    }

    #[tokio::test]
    async fn poll_pids_leaves_interrupted_sessions_alone() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut s = test_session("interrupted-1", "/ws");
        s.status = SessionStatus::Interrupted;
        s.pid = Some(999_999); // a pid that is almost certainly dead
        store.upsert_session(&s).unwrap();
        let engine = Engine::new(store.clone());
        let poller = Poller::new(engine);

        poller.poll_pids().await;

        let after = store.get_session("interrupted-1").unwrap().unwrap();
        assert!(
            matches!(after.status, SessionStatus::Interrupted),
            "poll_pids must not re-terminate an Interrupted session just because its stale pid is dead",
        );
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

    // ── GithubApi fake — drives poll_github/poll_pr_reconciliation with no
    // network access, so the dual-remote fallback and reconciliation logic
    // can be exercised deterministically. ───────────────────────────────────

    #[derive(Default)]
    struct FakeGithub {
        /// Keyed by (owner, repo, pr_number) — PR numbers are per-repo, so a
        /// fake that ignored the number couldn't catch a cross-repo number
        /// collision bug.
        pr_status_ok:   std::sync::Mutex<HashMap<(String, String, u64), crate::github::PrStatus>>,
        branch_matches: std::sync::Mutex<HashMap<(String, String, String), crate::github::PrRef>>,
        /// (owner, repo, pr_number) triples `get_pr_status` was actually called with, in order.
        calls: std::sync::Mutex<Vec<(String, String, u64)>>,
    }

    #[async_trait::async_trait]
    impl crate::github::GithubApi for FakeGithub {
        async fn get_pr_status(&self, owner: &str, repo: &str, pr_number: u64) -> anyhow::Result<crate::github::PrStatus> {
            self.calls.lock().unwrap().push((owner.to_string(), repo.to_string(), pr_number));
            self.pr_status_ok.lock().unwrap()
                .get(&(owner.to_string(), repo.to_string(), pr_number))
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("404 for {owner}/{repo}#{pr_number}"))
        }
        async fn get_ci_checks(&self, _owner: &str, _repo: &str, _head_sha: &str) -> anyhow::Result<Vec<CheckRun>> {
            Ok(vec![])
        }
        async fn get_review_threads(&self, _owner: &str, _repo: &str, _pr_number: u64) -> anyhow::Result<Vec<crate::github::ReviewThread>> {
            Ok(vec![])
        }
        async fn find_open_pr_for_branch(&self, owner: &str, repo: &str, branch: &str) -> anyhow::Result<Option<crate::github::PrRef>> {
            Ok(self.branch_matches.lock().unwrap()
                .get(&(owner.to_string(), repo.to_string(), branch.to_string()))
                .cloned())
        }
    }

    fn init_git_repo(branch: &str, remotes: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().to_string_lossy().to_string();
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(["-C", &workspace]).args(args).status().unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        run(&["commit", "--allow-empty", "-q", "-m", "init"]);
        run(&["checkout", "-q", "-b", branch]);
        for (name, url) in remotes {
            run(&["remote", "add", name, url]);
        }
        dir
    }

    fn github_engine(store: std::sync::Arc<crate::store::Store>, gh: std::sync::Arc<FakeGithub>) -> std::sync::Arc<Engine> {
        Engine::new_with_github_api(store, gh as std::sync::Arc<dyn crate::github::GithubApi>)
    }

    /// The core acceptance criterion: a PR opened without the wrapped `gh pr
    /// create` ever running (no metadata file at all) must still end up
    /// adopted, purely from an active branch lookup against a configured
    /// remote.
    #[tokio::test]
    async fn poll_pr_reconciliation_adopts_pr_found_without_wrapper_metadata() {
        use crate::store::Store;

        let repo_dir = init_git_repo("worker-branch", &[("origin", "https://github.com/Owner/repo.git")]);
        let workspace = repo_dir.path().to_string_lossy().to_string();

        let fake = std::sync::Arc::new(FakeGithub::default());
        fake.branch_matches.lock().unwrap().insert(
            ("Owner".to_string(), "repo".to_string(), "worker-branch".to_string()),
            crate::github::PrRef { number: 77, url: "https://github.com/Owner/repo/pull/77".into() },
        );

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&test_session("s1", &workspace)).unwrap();
        let engine = github_engine(store.clone(), fake);
        let poller = Poller::new(engine);

        poller.poll_pr_reconciliation().await;

        let session = store.get_session("s1").unwrap().unwrap();
        assert_eq!(session.pr_number, Some(77), "no wrapper metadata was ever written — reconciliation must still find it");
        assert!(matches!(session.status, SessionStatus::PrOpen));
        assert_eq!(session.repo, "Owner/repo");
    }

    #[tokio::test]
    async fn poll_pr_reconciliation_leaves_sessions_with_no_matching_pr_untouched() {
        use crate::store::Store;

        let repo_dir = init_git_repo("worker-branch", &[("origin", "https://github.com/Owner/repo.git")]);
        let workspace = repo_dir.path().to_string_lossy().to_string();

        let fake = std::sync::Arc::new(FakeGithub::default()); // no branch matches configured

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&test_session("s1", &workspace)).unwrap();
        let engine = github_engine(store.clone(), fake);
        let poller = Poller::new(engine);

        poller.poll_pr_reconciliation().await;

        let session = store.get_session("s1").unwrap().unwrap();
        assert_eq!(session.pr_number, None);
    }

    /// The dual-remote gap from the bug report: the session's recorded repo
    /// (`origin`) 404s, but the PR actually lives against a second
    /// configured remote (an internal mirror) — `poll_github` must fall
    /// back to it instead of silently stalling, and self-heal `session.repo`
    /// (and `session.pr_number`, since PR numbers are per-repo) so later
    /// ticks go straight there. The mirror's real PR is deliberately given a
    /// *different* number (99, not the tracked 50) — a repo whose branch
    /// match is only found by matching the branch, not by coincidentally
    /// reusing the same numeric `pr_number`.
    #[tokio::test]
    async fn poll_github_falls_back_to_other_remote_when_recorded_repo_404s() {
        use crate::store::Store;

        let repo_dir = init_git_repo("worker-branch", &[
            ("origin", "https://github.com/OwnerA/repoA.git"),
            ("mirror", "https://github.com/OwnerB/repoB.git"),
        ]);
        let workspace = repo_dir.path().to_string_lossy().to_string();

        let fake = std::sync::Arc::new(FakeGithub::default());
        fake.branch_matches.lock().unwrap().insert(
            ("OwnerB".to_string(), "repoB".to_string(), "worker-branch".to_string()),
            crate::github::PrRef { number: 99, url: "https://github.com/OwnerB/repoB/pull/99".into() },
        );
        fake.pr_status_ok.lock().unwrap().insert(
            ("OwnerB".to_string(), "repoB".to_string(), 99),
            crate::github::PrStatus {
                merged: false, state: "open".into(), mergeable: Some(true),
                title: "t".into(), number: 99, head_sha: "abc".into(),
            },
        );
        // OwnerA/repoA#50 has no entry — get_pr_status errors, simulating a 404.

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut session = test_session("s1", &workspace);
        session.repo = "OwnerA/repoA".into();
        session.pr_number = Some(50);
        store.upsert_session(&session).unwrap();

        let engine = github_engine(store.clone(), fake.clone());
        let poller = Poller::new(engine);

        poller.poll_github().await;

        let updated = store.get_session("s1").unwrap().unwrap();
        assert_eq!(updated.repo, "OwnerB/repoB", "session.repo must self-heal to the remote that actually has the PR");
        assert_eq!(updated.pr_number, Some(99), "must adopt the mirror's own PR number, not reuse the tracked repo's number");

        let calls = fake.calls.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec![("OwnerA".to_string(), "repoA".to_string(), 50), ("OwnerB".to_string(), "repoB".to_string(), 99)],
            "the repo on record must be tried first (by its own number), the mirror only as a branch-matched fallback",
        );
    }

    /// The bug this guards against: PR numbers are a per-repository
    /// sequence with no cross-repo relationship. If the recorded repo 404s,
    /// a *different*, unrelated repo can easily have some PR at the exact
    /// same number purely by coincidence. Blindly retrying the tracked
    /// number against that repo would silently adopt the wrong PR. Since
    /// that unrelated PR's head branch doesn't match this session's branch,
    /// the branch-matching fallback must not adopt it — even though a
    /// `get_pr_status` for that same number would have "succeeded".
    #[tokio::test]
    async fn poll_github_does_not_adopt_an_unrelated_pr_that_shares_the_tracked_number() {
        use crate::store::Store;

        let repo_dir = init_git_repo("worker-branch", &[
            ("origin", "https://github.com/OwnerA/repoA.git"),
            ("mirror", "https://github.com/OwnerB/repoB.git"),
        ]);
        let workspace = repo_dir.path().to_string_lossy().to_string();

        let fake = std::sync::Arc::new(FakeGithub::default());
        // OwnerB/repoB happens to have *some* PR #50 too, but it's unrelated
        // — its branch doesn't match, so no `branch_matches` entry for it.
        fake.pr_status_ok.lock().unwrap().insert(
            ("OwnerB".to_string(), "repoB".to_string(), 50),
            crate::github::PrStatus {
                merged: false, state: "open".into(), mergeable: Some(true),
                title: "someone else's unrelated PR".into(), number: 50, head_sha: "zzz".into(),
            },
        );
        // OwnerA/repoA#50 has no entry — get_pr_status errors, simulating a 404.

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut session = test_session("s1", &workspace);
        session.repo = "OwnerA/repoA".into();
        session.pr_number = Some(50);
        store.upsert_session(&session).unwrap();

        let engine = github_engine(store.clone(), fake.clone());
        let poller = Poller::new(engine);

        poller.poll_github().await;

        let updated = store.get_session("s1").unwrap().unwrap();
        assert_eq!(updated.repo, "OwnerA/repoA", "must not adopt the mirror just because it happens to have a same-numbered PR");
        assert_eq!(updated.pr_number, Some(50));

        let calls = fake.calls.lock().unwrap().clone();
        assert!(
            !calls.contains(&("OwnerB".to_string(), "repoB".to_string(), 50)),
            "must never retry the tracked number against another repo's get_pr_status: {calls:?}",
        );
    }

    /// When every configured remote fails, that must be visible — not just
    /// a `tracing::warn!` — but deduped so it doesn't spam every tick, and
    /// re-armed once the session recovers and then fails again.
    #[tokio::test]
    async fn poll_github_notifies_once_when_every_remote_fails_and_rearms_after_recovery() {
        use crate::store::Store;

        let repo_dir = init_git_repo("worker-branch", &[("origin", "https://github.com/OwnerA/repoA.git")]);
        let workspace = repo_dir.path().to_string_lossy().to_string();

        let fake = std::sync::Arc::new(FakeGithub::default()); // always 404s

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut session = test_session("s1", &workspace);
        session.repo = "OwnerA/repoA".into();
        session.pr_number = Some(50);
        store.upsert_session(&session).unwrap();

        let engine = github_engine(store.clone(), fake.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        poller.poll_github().await;
        let events = drain_events(&mut rx);
        let failures = |evs: &[Event]| evs.iter().filter(|e| matches!(
            e, Event::Notification(n) if n.kind == crate::types::NotificationKind::GithubLookupFailed
        )).count();
        assert_eq!(failures(&events), 1, "first failure must notify");

        poller.poll_github().await;
        let events = drain_events(&mut rx);
        assert_eq!(failures(&events), 0, "repeated failure must not re-notify");

        // Recovery.
        fake.pr_status_ok.lock().unwrap().insert(
            ("OwnerA".to_string(), "repoA".to_string(), 50),
            crate::github::PrStatus {
                merged: false, state: "open".into(), mergeable: Some(true),
                title: "t".into(), number: 50, head_sha: "abc".into(),
            },
        );
        poller.poll_github().await;

        // Fails again — must notify again, since recovery cleared the flag.
        fake.pr_status_ok.lock().unwrap().clear();
        poller.poll_github().await;
        let events = drain_events(&mut rx);
        assert_eq!(failures(&events), 1, "must notify again after recovering and failing anew");
    }

    // ── Brain harvest ────────────────────────────────────────────────────────

    use std::{future::Future, pin::Pin};

    /// Records every call it receives on `calls` and resolves with a
    /// caller-configured outcome — never spawns a real process or touches
    /// the network.
    struct FakeHarvestRunner {
        calls:   tokio::sync::mpsc::UnboundedSender<(String, PathBuf, PathBuf)>,
        outcome: Result<(), String>,
    }

    impl HarvestRunner for FakeHarvestRunner {
        fn run(
            &self,
            prompt:     String,
            workspace:  PathBuf,
            brain_path: PathBuf,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>> {
            let _ = self.calls.send((prompt, workspace, brain_path));
            let outcome = self.outcome.clone();
            Box::pin(async move {
                outcome.map_err(|e| anyhow::anyhow!(e))
            })
        }
    }

    /// A repo on an explicit `main` branch (so default-branch detection is
    /// deterministic regardless of the machine's `init.defaultBranch`),
    /// checked out onto `feature_branch` with an optional extra commit —
    /// this is the diff `compute_nontrivial_diff` sees.
    fn init_diff_repo(feature_branch: &str, extra_file: Option<(&str, &str)>) -> std::path::PathBuf {
        let dir = tempfile::tempdir().unwrap().keep();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", dir.to_str().unwrap()])
                .args(args)
                .output()
                .unwrap()
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        std::fs::write(dir.join("README.md"), "x").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);
        run(&["checkout", "-q", "-b", feature_branch]);
        if let Some((name, contents)) = extra_file {
            std::fs::write(dir.join(name), contents).unwrap();
            run(&["add", name]);
            run(&["commit", "-q", "-m", "feature work"]);
        }
        dir
    }

    /// Point `NINOX_CONFIG` at a path that doesn't exist, so `AppConfig::load()`
    /// falls back to `AppConfig::default()` (brain harvest enabled) rather
    /// than risking a real config file on the machine running the tests.
    fn nonexistent_config_path() -> std::path::PathBuf {
        tempfile::tempdir().unwrap().keep().join("nonexistent-ninox-config.toml")
    }

    /// A worker session whose PR was just detected, with a real non-trivial
    /// diff on its branch, triggers exactly one background harvest attempt —
    /// and never a second one on a later tick, since `pr_number.is_none()`
    /// has already flipped.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn metadata_sync_triggers_brain_harvest_exactly_once_on_pr_detection() {
        use crate::{config::ENV_TEST_GUARD, store::Store};

        let _guard = ENV_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var("NINOX_CONFIG").ok();
        std::env::set_var("NINOX_CONFIG", nonexistent_config_path());

        let repo = init_diff_repo("feature-1", Some(("src.rs", "fn main() {}\n")));
        let workspace = repo.to_str().unwrap().to_string();

        let sessions_dir = tempfile::tempdir().unwrap();
        let meta = serde_json::json!({"agentReportedPrNumber": "9"});
        std::fs::write(sessions_dir.path().join("s1.json"), serde_json::to_string(&meta).unwrap()).unwrap();

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&test_session("s1", &workspace)).unwrap();
        let engine = Engine::new(store.clone());

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let runner = Arc::new(FakeHarvestRunner { calls: tx, outcome: Ok(()) });
        let poller = Poller::new_with_harvest_runner(engine, runner);

        poller.sync_sessions_metadata(sessions_dir.path()).await;

        let (prompt, ..) = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("harvest should be attempted")
            .expect("channel should not be closed");
        assert!(prompt.contains("src.rs") && prompt.contains("fn main"), "prompt must include the diff");

        // Second tick: pr_number is already Some, so the transition guard
        // must not fire the harvest again.
        poller.sync_sessions_metadata(sessions_dir.path()).await;
        assert!(rx.try_recv().is_err(), "harvest must fire exactly once per session");

        match prior {
            Some(v) => std::env::set_var("NINOX_CONFIG", v),
            None    => std::env::remove_var("NINOX_CONFIG"),
        }
    }

    /// A worker spawned against a non-default catalogue (`session.catalogue_path`,
    /// set from that worker's own `NINOX_BRAIN` at spawn time — see
    /// `ninox_app::main::run_spawn`) must have its harvest write to that same
    /// catalogue, not silently fall back to the global default brain path.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn metadata_sync_brain_harvest_prefers_session_catalogue_path_over_default() {
        use crate::{config::ENV_TEST_GUARD, store::Store};

        let _guard = ENV_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var("NINOX_CONFIG").ok();
        std::env::set_var("NINOX_CONFIG", nonexistent_config_path());

        let repo = init_diff_repo("feature-catalogue", Some(("src.rs", "fn main() {}\n")));
        let workspace = repo.to_str().unwrap().to_string();

        let sessions_dir = tempfile::tempdir().unwrap();
        let meta = serde_json::json!({"agentReportedPrNumber": "13"});
        std::fs::write(sessions_dir.path().join("s1.json"), serde_json::to_string(&meta).unwrap()).unwrap();

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut session = test_session("s1", &workspace);
        session.catalogue_path = Some("/custom/brain-catalogue".to_string());
        store.upsert_session(&session).unwrap();
        let engine = Engine::new(store.clone());

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let runner = Arc::new(FakeHarvestRunner { calls: tx, outcome: Ok(()) });
        let poller = Poller::new_with_harvest_runner(engine, runner);

        poller.sync_sessions_metadata(sessions_dir.path()).await;

        let (_, _, brain_path) = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("harvest should be attempted")
            .expect("channel should not be closed");
        assert_eq!(
            brain_path, PathBuf::from("/custom/brain-catalogue"),
            "harvest must target the session's own catalogue, not the global default",
        );

        match prior {
            Some(v) => std::env::set_var("NINOX_CONFIG", v),
            None    => std::env::remove_var("NINOX_CONFIG"),
        }
    }

    /// `brain_harvest.enabled = false` must suppress the harvest entirely —
    /// PR detection itself proceeds exactly as it would with it enabled.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn metadata_sync_skips_brain_harvest_when_disabled() {
        use crate::{config::ENV_TEST_GUARD, store::Store};

        let _guard = ENV_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let config_dir = tempfile::tempdir().unwrap();
        let config_path = config_dir.path().join("config.toml");
        std::fs::write(&config_path, "port = 8080\nfont_size = 13.0\n\n[brain_harvest]\nenabled = false\n").unwrap();
        let prior = std::env::var("NINOX_CONFIG").ok();
        std::env::set_var("NINOX_CONFIG", &config_path);

        let repo = init_diff_repo("feature-2", Some(("src.rs", "fn main() {}\n")));
        let workspace = repo.to_str().unwrap().to_string();

        let sessions_dir = tempfile::tempdir().unwrap();
        let meta = serde_json::json!({"agentReportedPrNumber": "10"});
        std::fs::write(sessions_dir.path().join("s1.json"), serde_json::to_string(&meta).unwrap()).unwrap();

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&test_session("s1", &workspace)).unwrap();
        let engine = Engine::new(store.clone());

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let runner = Arc::new(FakeHarvestRunner { calls: tx, outcome: Ok(()) });
        let poller = Poller::new_with_harvest_runner(engine, runner);

        poller.sync_sessions_metadata(sessions_dir.path()).await;

        assert!(rx.try_recv().is_err(), "harvest must not fire when brain_harvest.enabled = false");
        let session = store.get_session("s1").unwrap().unwrap();
        assert_eq!(session.pr_number, Some(10), "PR detection must be unaffected by the disabled harvest");
        assert!(matches!(session.status, SessionStatus::PrOpen));

        match prior {
            Some(v) => std::env::set_var("NINOX_CONFIG", v),
            None    => std::env::remove_var("NINOX_CONFIG"),
        }
    }

    /// A session whose branch has no diff against the default branch yet
    /// must not trigger a harvest — nothing worth recording, and no point
    /// invoking an LLM call for it.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn metadata_sync_skips_brain_harvest_for_trivial_diff() {
        use crate::{config::ENV_TEST_GUARD, store::Store};

        let _guard = ENV_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var("NINOX_CONFIG").ok();
        std::env::set_var("NINOX_CONFIG", nonexistent_config_path());

        // No extra commit — the feature branch is identical to main.
        let repo = init_diff_repo("feature-3", None);
        let workspace = repo.to_str().unwrap().to_string();

        let sessions_dir = tempfile::tempdir().unwrap();
        let meta = serde_json::json!({"agentReportedPrNumber": "11"});
        std::fs::write(sessions_dir.path().join("s1.json"), serde_json::to_string(&meta).unwrap()).unwrap();

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&test_session("s1", &workspace)).unwrap();
        let engine = Engine::new(store.clone());

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let runner = Arc::new(FakeHarvestRunner { calls: tx, outcome: Ok(()) });
        let poller = Poller::new_with_harvest_runner(engine, runner);

        poller.sync_sessions_metadata(sessions_dir.path()).await;

        assert!(rx.try_recv().is_err(), "harvest must not fire for an empty diff");

        match prior {
            Some(v) => std::env::set_var("NINOX_CONFIG", v),
            None    => std::env::remove_var("NINOX_CONFIG"),
        }
    }

    /// A failing harvest subprocess must not affect the rest of
    /// `sync_sessions_metadata` — the session still transitions to `PrOpen`
    /// normally, and the failure is swallowed rather than propagated.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn metadata_sync_survives_a_failing_brain_harvest() {
        use crate::{config::ENV_TEST_GUARD, store::Store};

        let _guard = ENV_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var("NINOX_CONFIG").ok();
        std::env::set_var("NINOX_CONFIG", nonexistent_config_path());

        let repo = init_diff_repo("feature-4", Some(("src.rs", "fn main() {}\n")));
        let workspace = repo.to_str().unwrap().to_string();

        let sessions_dir = tempfile::tempdir().unwrap();
        let meta = serde_json::json!({"agentReportedPrNumber": "12"});
        std::fs::write(sessions_dir.path().join("s1.json"), serde_json::to_string(&meta).unwrap()).unwrap();

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&test_session("s1", &workspace)).unwrap();
        let engine = Engine::new(store.clone());

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let runner = Arc::new(FakeHarvestRunner {
            calls:   tx,
            outcome: Err("simulated claude -p failure".to_string()),
        });
        let poller = Poller::new_with_harvest_runner(engine, runner);

        poller.sync_sessions_metadata(sessions_dir.path()).await;

        let session = store.get_session("s1").unwrap().unwrap();
        assert_eq!(session.pr_number, Some(12), "PR detection must succeed regardless of harvest outcome");
        assert!(matches!(session.status, SessionStatus::PrOpen));

        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("the failing harvest must still be attempted")
            .expect("channel should not be closed");

        match prior {
            Some(v) => std::env::set_var("NINOX_CONFIG", v),
            None    => std::env::remove_var("NINOX_CONFIG"),
        }
    }

    /// Captures every `tracing` event's formatted `message` field so tests
    /// can assert on log output without a real logging backend.
    #[derive(Clone, Default)]
    struct CapturedLogs(Arc<std::sync::Mutex<Vec<String>>>);

    impl CapturedLogs {
        fn contains(&self, needle: &str) -> bool {
            self.0.lock().unwrap().iter().any(|m| m.contains(needle))
        }
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CapturedLogs {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
            struct Visitor(String);
            impl tracing::field::Visit for Visitor {
                fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                    if field.name() == "message" {
                        self.0 = format!("{value:?}");
                    }
                }
            }
            let mut visitor = Visitor(String::new());
            event.record(&mut visitor);
            self.0.lock().unwrap().push(visitor.0);
        }
    }

    /// A `HarvestRunner` whose returned future panics as soon as it's
    /// polled — stands in for a bug inside the real harvest subprocess
    /// plumbing, to prove a panic is logged rather than silently lost.
    struct PanickingHarvestRunner;

    impl HarvestRunner for PanickingHarvestRunner {
        fn run(
            &self,
            _prompt: String,
            _workspace: PathBuf,
            _brain_path: PathBuf,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>> {
            Box::pin(async move { panic!("simulated harvest panic") })
        }
    }

    /// A panicking `HarvestRunner` must produce a logged warning — the
    /// `tokio::spawn` `JoinHandle` is otherwise discarded and a panic would
    /// be completely silent (see `trigger_brain_harvest`'s supervising
    /// spawn).
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn metadata_sync_logs_a_warning_when_harvest_task_panics() {
        use crate::{config::ENV_TEST_GUARD, store::Store};
        use tracing_subscriber::{layer::SubscriberExt, Registry};

        let _guard = ENV_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var("NINOX_CONFIG").ok();
        std::env::set_var("NINOX_CONFIG", nonexistent_config_path());

        let logs = CapturedLogs::default();
        let _log_guard = tracing::subscriber::set_default(Registry::default().with(logs.clone()));

        let repo = init_diff_repo("feature-panic", Some(("src.rs", "fn main() {}\n")));
        let workspace = repo.to_str().unwrap().to_string();

        let sessions_dir = tempfile::tempdir().unwrap();
        let meta = serde_json::json!({"agentReportedPrNumber": "20"});
        std::fs::write(sessions_dir.path().join("s1.json"), serde_json::to_string(&meta).unwrap()).unwrap();

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&test_session("s1", &workspace)).unwrap();
        let engine = Engine::new(store.clone());

        let poller = Poller::new_with_harvest_runner(engine, Arc::new(PanickingHarvestRunner));
        poller.sync_sessions_metadata(sessions_dir.path()).await;

        // The harvest + its supervising task are detached spawns; poll
        // until the panic has been caught and logged, bounded so a
        // regression fails the test instead of hanging.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !logs.contains("brain harvest task panicked") {
            assert!(std::time::Instant::now() < deadline, "timed out waiting for the panic to be logged");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        match prior {
            Some(v) => std::env::set_var("NINOX_CONFIG", v),
            None    => std::env::remove_var("NINOX_CONFIG"),
        }
    }

    /// Records whether it ever observed two overlapping `run()` calls —
    /// proves the per-vault lock actually serializes concurrent harvests
    /// targeting the same brain path, rather than merely happening not to
    /// race in this particular run.
    struct OverlapDetectingHarvestRunner {
        calls:      tokio::sync::mpsc::UnboundedSender<()>,
        active:     Arc<std::sync::atomic::AtomicUsize>,
        overlapped: Arc<std::sync::atomic::AtomicBool>,
    }

    impl HarvestRunner for OverlapDetectingHarvestRunner {
        fn run(
            &self,
            _prompt: String,
            _workspace: PathBuf,
            _brain_path: PathBuf,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>> {
            let _ = self.calls.send(());
            let active = self.active.clone();
            let overlapped = self.overlapped.clone();
            Box::pin(async move {
                if active.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0 {
                    overlapped.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
                active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            })
        }
    }

    /// Two sessions whose harvests target the same (default) brain vault —
    /// neither sets `catalogue_path`, so both resolve to
    /// `config.resolved_brain_path()` — must never have their
    /// `HarvestRunner::run` calls (which, in production, both invoke `ninox
    /// brain index`) overlap.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn concurrent_harvests_to_the_same_vault_do_not_overlap() {
        use crate::{config::ENV_TEST_GUARD, store::Store};

        let _guard = ENV_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var("NINOX_CONFIG").ok();
        std::env::set_var("NINOX_CONFIG", nonexistent_config_path());

        let repo1 = init_diff_repo("feature-vault-1", Some(("a.rs", "fn a() {}\n")));
        let repo2 = init_diff_repo("feature-vault-2", Some(("b.rs", "fn b() {}\n")));

        let sessions_dir = tempfile::tempdir().unwrap();
        for (id, pr) in [("s1", "30"), ("s2", "31")] {
            let meta = serde_json::json!({"agentReportedPrNumber": pr});
            std::fs::write(sessions_dir.path().join(format!("{id}.json")), serde_json::to_string(&meta).unwrap()).unwrap();
        }

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&test_session("s1", repo1.to_str().unwrap())).unwrap();
        store.upsert_session(&test_session("s2", repo2.to_str().unwrap())).unwrap();
        let engine = Engine::new(store.clone());

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let overlapped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let runner = Arc::new(OverlapDetectingHarvestRunner {
            calls: tx, active: active.clone(), overlapped: overlapped.clone(),
        });
        let poller = Poller::new_with_harvest_runner(engine, runner);

        poller.sync_sessions_metadata(sessions_dir.path()).await;

        for _ in 0..2 {
            tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("both harvests should be attempted")
                .expect("channel should not be closed");
        }
        // Let the (lock-serialized) second call finish its simulated work
        // so `overlapped` reflects the full run.
        tokio::time::sleep(Duration::from_millis(150)).await;

        assert!(
            !overlapped.load(std::sync::atomic::Ordering::SeqCst),
            "concurrent harvests to the same vault must not run HarvestRunner::run concurrently",
        );

        match prior {
            Some(v) => std::env::set_var("NINOX_CONFIG", v),
            None    => std::env::remove_var("NINOX_CONFIG"),
        }
    }

    /// Two syntactically different paths to the same physical vault (here:
    /// a trailing slash) must resolve to the same lock — otherwise two
    /// harvests using differently-spelled `catalogue_path`s for the same
    /// vault could still run `ninox brain index` concurrently, silently
    /// defeating the point of the lock.
    #[test]
    fn vault_lock_treats_equivalent_paths_as_the_same_vault() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let engine = Engine::new(store);
        let poller = Poller::new_with_harvest_runner(engine, Arc::new(ClaudeHarvestRunner));

        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().to_path_buf();
        let with_trailing_slash = PathBuf::from(format!("{}/", canonical.display()));

        let lock_a = poller.vault_lock(&canonical);
        let lock_b = poller.vault_lock(&with_trailing_slash);

        assert!(
            Arc::ptr_eq(&lock_a, &lock_b),
            "syntactically different paths to the same physical vault must share one lock",
        );
    }
}
