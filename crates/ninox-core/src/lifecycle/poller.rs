use crate::{
    config::{AppConfig, SessionRetentionConfig},
    events::{Engine, Event},
    github::{split_repo, CheckRun},
    github_graphql::{BranchKey, PrKey},
    hooks,
    lifecycle::{
        brain_harvest::{self, ClaudeHarvestRunner, HarvestRunner},
        enrichment::EnrichmentCache,
        probe::is_pid_alive,
        update_check::{self, CargoRegistryUpdateSource, UpdateSource},
        usage,
    },
    types::{
        CIStatus, Comment, GateCheck, GateStatus, Notification, NotificationKind, PrId, Session,
        SessionFields, SessionStatus, PR,
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
type ActivitySnapshot = (crate::types::ActivityState, Option<String>, Option<i64>);
/// `(repo, head_ref, base_ref)` of a session's open PR.
type PrRefsSnapshot = (String, String, String);

/// Unix epoch milliseconds "now" — used to stamp `Notification::created_at`
/// and `Session::terminal_at`. `pub(crate)` so `events::cleanup_session` can
/// stamp the same clock when it marks a session `Done`.
pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Pure stacked-edge derivation over `(session_id, repo, head_ref,
/// base_ref)` entries: an edge from B to A when B's base branch is A's head
/// branch within the same repo. Returns an entry for *every* input session
/// (empty targets included) so the caller's `set_stacked_deps` clears edges
/// whose branch relationship no longer holds.
pub(crate) fn derive_stacked_edges(
    entries: &[(String, String, String, String)],
) -> Vec<(String, Vec<String>)> {
    entries.iter().map(|(id, repo, _head, base)| {
        let mut targets: Vec<String> = entries.iter()
            .filter(|(other_id, other_repo, other_head, _)| {
                // GitHub repo slugs are case-insensitive, and the same repo
                // can be recorded with different casing depending on whether
                // it was user-typed or parsed from a git remote.
                other_id != id && other_repo.eq_ignore_ascii_case(repo) && other_head == base
            })
            .map(|(other_id, ..)| other_id.clone())
            .collect();
        targets.sort();
        (id.clone(), targets)
    }).collect()
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

/// Decide what a session's status becomes when its tmux pane is found
/// gone at startup. `has_resume_args` is the harness's capability (from
/// `HarnessRegistry::resume_cmd(...).is_some()` against a placeholder id —
/// callers don't have a real command to build yet, just the capability
/// check), not whether resume has ever been attempted.
pub(crate) fn reconciled_status_for_dead_session(
    claude_session_id: &Option<String>,
    has_resume_args:   bool,
) -> SessionStatus {
    if claude_session_id.is_some() && has_resume_args {
        SessionStatus::Interrupted
    } else {
        SessionStatus::Terminated
    }
}

/// Reconcile one session whose tmux session is already known to be gone.
/// Re-reads the live row (the caller has just awaited `tmux::has_session`,
/// so its snapshot is stale), leaves rows that went terminal meanwhile
/// alone, and writes the reconciled status: Interrupted when the harness
/// can `--resume` it, Terminated — stamped with `terminal_at` so the
/// retention sweep gives it a window instead of purging on sight —
/// otherwise. Returns the written row so callers with an `Engine` can emit
/// `SessionUpdated`. Shared by the poller's startup sweep and `ninox
/// connect`: a connect attempt right after a reboot runs before any
/// daemon has reconciled, and must not burn resumability the sweep would
/// have preserved.
pub fn reconcile_dead_session(
    store:      &crate::store::Store,
    registry:   &crate::harness::HarnessRegistry,
    session_id: &str,
) -> anyhow::Result<Option<Session>> {
    let Some(mut live) = store.get_session(session_id)? else { return Ok(None) };
    if live.status.is_terminal() {
        return Ok(None);
    }
    let agent = crate::config::AgentConfig {
        harness: live.agent_type.clone(),
        model:   live.model.clone(),
    };
    let has_resume = registry.resume_cmd(&agent, "placeholder").is_some();
    let last_status = live.status.clone();
    live.status = reconciled_status_for_dead_session(&live.claude_session_id, has_resume);
    if live.status == SessionStatus::Terminated {
        live.terminal_at = Some(now_millis());
    }
    store.upsert_session(&live)?;
    crate::fleet::record_interruption(store, &live.id, live.started_at, &last_status, now_millis());
    if let (SessionStatus::Terminated, Some(at)) = (&live.status, live.terminal_at) {
        if let Err(e) = store.mark_reconciled_terminal(&live.id, at) {
            tracing::warn!("fleet: mark {} reconciled-terminated: {e}", live.id);
        }
    }
    Ok(Some(live))
}

/// Follow a branch the agent created (git wrapper metadata) so restore's
/// workspace check compares against the branch it actually works on.
fn sync_recorded_branch(store: &crate::store::Store, session_id: &str, branch: &str) {
    let recorded = store.fleet_record(session_id).ok().flatten().and_then(|r| r.branch);
    if recorded.as_deref() == Some(branch) {
        return;
    }
    if let Err(e) = store.set_session_branch(session_id, branch) {
        tracing::warn!("fleet: record branch {branch} for {session_id}: {e}");
    }
}

/// Mirror delivered-from-file work requests into the store; only those in
/// `sent` actually reached the orchestrator.
fn record_work_requests(
    store:   &crate::store::Store,
    session: &Session,
    pending: &[hooks::WorkRequest],
    sent:    &std::collections::HashSet<String>,
    now:     i64,
) {
    for request in pending {
        // Requests filed before the table existed have no row; mirror
        // them now so the fleet history is complete.
        let _ = store.insert_work_request(&crate::store::WorkRequestRow {
            id:              request.id.clone(),
            from_session:    session.id.clone(),
            orchestrator_id: session.orchestrator_id.clone(),
            body:            request.description.clone(),
            created_at:      request.requested_at,
            delivered_at:    None,
            resolved_at:     None,
        });
        if !sent.contains(&request.id) {
            continue;
        }
        if let Err(e) = store.mark_work_request_delivered(&request.id, now) {
            tracing::warn!("record work request {} delivered: {e}", request.id);
        }
    }
}

/// Runs a fleet restore; see `Poller::with_fleet_restorer`.
pub type FleetRestorer = Arc<
    dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync,
>;

/// Liveness check used by reconciliation; injectable for tests.
type LivenessCheck<'a> = &'a (dyn Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::runtime::Liveness> + Send>> + Sync);

/// Arbitrary; how often a reconciliation retry may try to start the ptyd
/// host again.
const PREPARE_LIVENESS_EVERY_MS: i64 = 60_000;

pub struct Poller {
    engine:           Arc<Engine>,
    fleet_restorer:   Option<FleetRestorer>,
    /// Sessions startup reconciliation couldn't judge because the ptyd host
    /// didn't answer. Retried on the pid tick; `poll_pids` leaves them alone
    /// meanwhile, since a pid check would burn their resumability.
    unreconciled:     Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    last_liveness_prepare: Arc<std::sync::Mutex<i64>>,
    enrichment_cache: Arc<std::sync::Mutex<EnrichmentCache>>,
    /// Last-seen `(cost_usd, context_used_pct, context_total_tokens)` per
    /// session, used solely to detect changes written externally by the
    /// `ninox statusline` subcommand — see `poll_context_updates`.
    context_cache:    Arc<std::sync::Mutex<HashMap<String, ContextSnapshot>>>,
    /// Last-seen `(activity, activity_note, activity_since)` per session —
    /// same external-writer detection as `context_cache`, but for the
    /// `ninox worker-status` subcommand. See `poll_activity_updates`.
    activity_cache:   Arc<std::sync::Mutex<HashMap<String, ActivitySnapshot>>>,
    /// Last-fetched `(repo, head_ref, base_ref)` per session with an open
    /// PR, fed by both GitHub polling paths and consumed by
    /// `reconcile_stacked_deps` to derive stacked dependency edges.
    pr_refs_cache:    Arc<std::sync::Mutex<HashMap<String, PrRefsSnapshot>>>,
    /// Last `Store::message_delivered_counts` seen per session, so
    /// `poll_message_counts` emits only when a count moves. `None` until the
    /// first tick takes its baseline snapshot.
    message_count_cache: Arc<std::sync::Mutex<Option<HashMap<String, u64>>>>,
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
    /// Where to look up the latest published ninox version. Injectable so
    /// tests never hit the network or shell out — see `poll_update_check`.
    update_source:    Arc<dyn UpdateSource>,
    /// The newest version a `poll_update_check` run has already notified
    /// about, so a steady "still on 0.14.0" state re-notifies only once,
    /// not every tick — same dedup shape as `github_lookup_failed_notified`.
    last_notified_update: Arc<std::sync::Mutex<Option<semver::Version>>>,
    /// Unix millis until which `poll_github_batched` must skip its tick
    /// entirely (0 = no pause). Set by `note_rate_limit` (GraphQL budget
    /// running low) or `note_batch_error` (GitHub answered 403/429). A
    /// paused tick returns before `GithubBatchApi::fetch_batch` is even
    /// called — a skipped tick, never a blocked task — so it costs nothing
    /// beyond the interval's own tick.
    rate_limit_pause_until: Arc<std::sync::Mutex<i64>>,
    /// The backoff duration (seconds) applied by the *last* Retry-After-less
    /// `RateLimitedError`, so consecutive such errors double it instead of
    /// re-pausing for a flat 120s each time (0 = no backoff established
    /// yet). A `RateLimitedError` that carries its own `Retry-After` uses
    /// that value directly and leaves this untouched. Reset to 0 by
    /// `note_rate_limit` on any successful fetch, so a fresh outage after a
    /// recovery starts the doubling over from 120s.
    rate_limit_backoff_secs: Arc<std::sync::Mutex<u64>>,
}

impl Poller {
    pub fn new(engine: Arc<Engine>) -> Self {
        Self::new_with_harvest_runner(engine, Arc::new(ClaudeHarvestRunner))
    }

    pub fn new_with_harvest_runner(engine: Arc<Engine>, harvest_runner: Arc<dyn HarvestRunner>) -> Self {
        Self {
            engine,
            fleet_restorer:   None,
            unreconciled:     Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            last_liveness_prepare: Arc::new(std::sync::Mutex::new(0)),
            enrichment_cache: Arc::new(std::sync::Mutex::new(HashMap::new())),
            context_cache:    Arc::new(std::sync::Mutex::new(HashMap::new())),
            activity_cache:   Arc::new(std::sync::Mutex::new(HashMap::new())),
            pr_refs_cache:    Arc::new(std::sync::Mutex::new(HashMap::new())),
            message_count_cache: Arc::new(std::sync::Mutex::new(None)),
            harvest_runner,
            vault_locks:      Arc::new(std::sync::Mutex::new(HashMap::new())),
            update_source:    Arc::new(CargoRegistryUpdateSource),
            last_notified_update: Arc::new(std::sync::Mutex::new(None)),
            rate_limit_pause_until: Arc::new(std::sync::Mutex::new(0)),
            rate_limit_backoff_secs: Arc::new(std::sync::Mutex::new(0)),
        }
    }

    /// Overrides the update-check source — the dependency-injection seam
    /// `poll_update_check`'s tests use instead of the real registry.
    pub fn with_update_source(mut self, source: Arc<dyn UpdateSource>) -> Self {
        self.update_source = source;
        self
    }

    /// Test seam: force `rate_limit_pause_until` directly, so pause-expiry
    /// behavior (a past pause not skipping a tick) can be exercised without
    /// waiting on a real clock or driving it through `note_rate_limit`/
    /// `note_batch_error`.
    #[cfg(test)]
    fn set_pause_until(&self, t: i64) {
        *self.rate_limit_pause_until.lock().unwrap_or_else(|e| e.into_inner()) = t;
    }

    /// Test seam: read `rate_limit_pause_until` directly, so exponential
    /// backoff (successive Retry-After-less `RateLimitedError`s doubling
    /// the pause) is assertable without waiting on a real clock.
    #[cfg(test)]
    fn pause_until(&self) -> i64 {
        *self.rate_limit_pause_until.lock().unwrap_or_else(|e| e.into_inner())
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

    /// Inject what `[fleet] restore_policy = "auto"` runs after startup
    /// reconciliation (the app's `ninox fleet restore` executor). Without
    /// one, `auto` degrades to `prompt`.
    pub fn with_fleet_restorer(mut self, restorer: FleetRestorer) -> Self {
        self.fleet_restorer = Some(restorer);
        self
    }

    async fn apply_restore_policy(&self) {
        let policy = AppConfig::load().unwrap_or_default().fleet.restore_policy;
        let store = &self.engine.store;
        match crate::fleet::startup::on_engine_startup(store, policy, now_millis()) {
            crate::fleet::StartupAction::AutoRestore(summary) => match &self.fleet_restorer {
                Some(restore) => {
                    tracing::info!(
                        "fleet: auto-restoring {} worker(s) and {} orchestrator(s)",
                        summary.workers, summary.orchestrators,
                    );
                    // Spawned: a restore waits on harness prompts for
                    // minutes and must not hold up the poll loop.
                    tokio::spawn(restore());
                }
                None => crate::fleet::startup::flag_pending_restore(store, now_millis()),
            },
            crate::fleet::StartupAction::FlaggedPending(summary) => tracing::info!(
                "fleet: restore pending ({} worker(s), {} orchestrator(s)) — run `ninox fleet restore`",
                summary.workers, summary.orchestrators,
            ),
            crate::fleet::StartupAction::Nothing => {}
        }
    }

    /// One-shot startup sweep: any non-terminal session whose pane no longer
    /// exists lost it to a host death (reboot). Mark it Interrupted when its
    /// harness can --resume it, Terminated otherwise. Runs before the first
    /// poll_pids tick: poll_pids only checks pid liveness and would mark
    /// these Terminated, destroying resumability.
    ///
    /// "Pane gone" must be a real answer. After a reboot the ptyd host isn't
    /// running yet, so it is started first and a fresh host that doesn't
    /// know a pane means Dead. A host that still doesn't answer (mid-upgrade,
    /// slow start) proves nothing: those sessions are deferred and retried.
    async fn reconcile_dead_sessions(&self) {
        *self.last_liveness_prepare.lock().unwrap_or_else(|e| e.into_inner()) = now_millis();
        crate::runtime::prepare_liveness().await;
        let Ok(sessions) = self.engine.store.list_sessions() else { return };
        let ids = sessions.into_iter().filter(|s| !s.status.is_terminal()).map(|s| s.id).collect();
        self.reconcile_sessions(ids, &|id| Box::pin(async move { crate::runtime::liveness(&id).await })).await;
    }

    /// Reconcile `ids` against `check`; returns whether any was found dead.
    /// Replaces the deferred set with whatever is still unknown.
    async fn reconcile_sessions(&self, ids: Vec<String>, check: LivenessCheck<'_>) -> bool {
        use crate::runtime::Liveness;
        let registry = AppConfig::load().unwrap_or_default().registry();
        let mut deferred = std::collections::HashSet::new();
        let mut reconciled = false;
        for id in ids {
            match check(id.clone()).await {
                Liveness::Live => {}
                Liveness::Unknown => {
                    deferred.insert(id);
                }
                Liveness::Dead => match reconcile_dead_session(&self.engine.store, &registry, &id) {
                    Ok(Some(live)) => {
                        reconciled = true;
                        self.engine.emit(Event::SessionUpdated(live, SessionFields::STATUS));
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!("reconcile {id}: {e}"),
                },
            }
        }
        if !deferred.is_empty() {
            tracing::warn!(
                "ptyd host not answering: {} session(s) left unreconciled, retrying",
                deferred.len(),
            );
        }
        *self.unreconciled.lock().unwrap_or_else(|e| e.into_inner()) = deferred;
        reconciled
    }

    /// Pid-tick retry of the sessions [`reconcile_dead_sessions`] deferred.
    async fn retry_unreconciled(&self, check: LivenessCheck<'_>) {
        let ids: Vec<String> = self.unreconciled.lock().unwrap_or_else(|e| e.into_inner()).iter().cloned().collect();
        if ids.is_empty() {
            return;
        }
        let ids = ids.into_iter()
            .filter(|id| self.engine.store.get_session(id).ok().flatten().is_some_and(|s| !s.status.is_terminal()))
            .collect();
        if self.reconcile_sessions(ids, check).await {
            self.apply_restore_policy().await;
        }
    }

    async fn retry_unreconciled_live(&self) {
        if self.unreconciled.lock().unwrap_or_else(|e| e.into_inner()).is_empty() {
            return;
        }
        let due = {
            let mut last = self.last_liveness_prepare.lock().unwrap_or_else(|e| e.into_inner());
            let due = now_millis() - *last >= PREPARE_LIVENESS_EVERY_MS;
            if due {
                *last = now_millis();
            }
            due
        };
        if due {
            crate::runtime::prepare_liveness().await;
        }
        self.retry_unreconciled(&|id| Box::pin(async move { crate::runtime::liveness(&id).await })).await;
    }

    pub async fn start(self, token: CancellationToken) {
        self.reconcile_dead_sessions().await;
        self.apply_restore_policy().await;

        let mut pid_interval    = tokio::time::interval(Duration::from_secs(5));
        let mut usage_interval  = tokio::time::interval(Duration::from_secs(10));
        let mut github_interval = tokio::time::interval(Duration::from_secs(30));
        // Releases only happen on merge-to-main, so this doesn't need to be
        // frequent — 6h keeps the once-per-startup check (interval's first
        // tick fires immediately) without hammering the registry or
        // shelling out to `aws codeartifact get-authorization-token` often.
        let mut update_interval = tokio::time::interval(Duration::from_secs(6 * 60 * 60));
        // Prevent a missed tick from causing back-to-back polls.
        github_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        usage_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        update_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = token.cancelled()      => break,
                _ = pid_interval.tick()    => {
                    self.retry_unreconciled_live().await;
                    self.poll_pids().await;
                    self.poll_context_updates().await;
                    self.poll_activity_updates().await;
                    self.poll_message_counts().await;
                    let retention = AppConfig::load()
                        .unwrap_or_default()
                        .session_retention;
                    self.sweep_retired_sessions(&retention).await;
                    self.deliver_worker_completions().await;
                }
                _ = usage_interval.tick()  => self.poll_usage().await,
                _ = update_interval.tick() => self.poll_update_check().await,
                _ = github_interval.tick() => {
                    let config = AppConfig::load().unwrap_or_default();
                    if config.pr_watch.enabled {
                        self.poll_github_batched(config.auto_reap.enabled).await;
                    } else {
                        // Reconciliation first: a session whose PR the poller
                        // hasn't adopted yet has no `pr_number` for `poll_github`
                        // to enrich, so it must run before (not instead of) it.
                        self.poll_pr_reconciliation().await;
                        self.poll_github(config.auto_reap.enabled).await;
                    }
                }
            }
        }
    }

    async fn deliver_worker_completions(&self) {
        const RETRY_AFTER_MS: i64 = 30_000;
        let now = now_millis();
        let delivery = match self
            .engine
            .store
            .claim_worker_completion_delivery(now, RETRY_AFTER_MS)
        {
            Ok(Some(delivery)) => delivery,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!("claim worker completion delivery: {error}");
                return;
            }
        };
        let message = format!(
            "Ninox recorded completion for worker `{}`. Receive its canonical final \
             handoff exactly once:\n\n`ninox receive-completion {}`",
            delivery.completion.session_id, delivery.completion.completion_id,
        );
        let error = self
            .engine
            .send_to_session(&delivery.completion.orchestrator_id, &message)
            .await
            .err()
            .map(|error| error.to_string());
        if let Err(record_error) = self.engine.store.finish_worker_completion_delivery_attempt(
            &delivery.completion.completion_id,
            &delivery.attempt_id,
            now_millis(),
            error.as_deref(),
        ) {
            tracing::warn!(
                "record worker completion delivery {} attempt {}: {record_error}",
                delivery.completion.completion_id,
                delivery.attempt,
            );
        }
        if let Some(error) = error {
            tracing::warn!(
                "deliver worker completion {} to orchestrator {}: {error}",
                delivery.completion.completion_id,
                delivery.completion.orchestrator_id,
            );
        }
    }

    // ── Update check ─────────────────────────────────────────────────────────

    /// Checks `update_source` for a newer ninox release than the one
    /// currently running, emitting an `UpdateAvailable` notification the
    /// first time a given newer version is seen. `ninox-core`'s own
    /// `CARGO_PKG_VERSION` is used rather than threading the app crate's
    /// version through: `release.yml` bumps every workspace crate together
    /// in one commit, and `ninox-app`'s `Cargo.toml` pins an exact-version
    /// path dependency on `ninox-core`, so the two can never diverge for a
    /// published build.
    async fn poll_update_check(&self) {
        let current_version = env!("CARGO_PKG_VERSION");
        match update_check::check_for_update(self.update_source.as_ref(), "ninox", current_version).await {
            Ok(Some(latest)) => {
                let already_notified = {
                    let mut last = self.last_notified_update.lock().unwrap_or_else(|e| e.into_inner());
                    let already = last.as_ref() == Some(&latest);
                    *last = Some(latest.clone());
                    already
                };
                if !already_notified {
                    self.engine.emit(Event::Notification(Notification {
                        id:         format!("update-available-{latest}"),
                        kind:       NotificationKind::UpdateAvailable,
                        title:      "Update available".to_string(),
                        body:       format!("ninox {latest} is available — running {current_version}"),
                        session_id: None,
                        created_at: now_millis(),
                    }));
                }
            }
            Ok(None) => {}
            Err(e) => tracing::warn!("update check failed: {e}"),
        }
    }

    // ── PID liveness ────────────────────────────────────────────────────────

    async fn poll_pids(&self) {
        // Metadata first: a dying worker's last acts (PR create, work
        // request) are processed before the reap below marks it Terminated.
        self.sync_sessions_metadata(&AppConfig::sessions_dir()).await;

        let Ok(sessions) = self.engine.store.list_sessions() else { return };
        let unreconciled = self.unreconciled.lock().unwrap_or_else(|e| e.into_inner()).clone();
        for mut session in sessions {
            if matches!(session.status, SessionStatus::Done | SessionStatus::Terminated | SessionStatus::Interrupted) {
                continue;
            }
            if unreconciled.contains(&session.id) {
                continue;
            }
            if let Some(pid) = session.pid {
                if !is_pid_alive(pid) {
                    session.status = SessionStatus::Terminated;
                    session.terminal_at = Some(now_millis());
                    let _ = self.engine.store.upsert_session(&session);
                    self.engine.emit(Event::SessionUpdated(session, SessionFields::STATUS | SessionFields::TERMINAL_AT));
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
            if let Some(branch) = meta.branch.as_deref() {
                sync_recorded_branch(&self.engine.store, &session.id, branch);
            }

            // -- First reported PR becomes the session's tracked PR --
            if session.pr_number.is_none() {
                if let Some(first) = meta.pr_reports.first() {
                    session.pr_number = Some(first.number);
                    session.status    = SessionStatus::PrOpen;
                    // Write through the live row, not the tick-start
                    // snapshot — `deliver_work_requests` awaited above, and
                    // the statusline process can land cost/context at any
                    // moment (see `update_live_session_row`).
                    let Some(written) = self.update_live_session_row(&session, |row| {
                        row.pr_number = session.pr_number;
                        row.status    = session.status.clone();
                    }) else {
                        // Deleted mid-tick — nothing below (extra-PR ledger
                        // rows, notifications, orchestrator messages) should
                        // run for a session that no longer exists either.
                        continue;
                    };
                    self.engine.emit(Event::SessionUpdated(
                        written, SessionFields::PR_LINK | SessionFields::STATUS,
                    ));
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
                    let pr = PR {
                        id:         report.number as i64,
                        number:     report.number,
                        title:      String::new(),
                        url,
                        body:       String::new(),
                        session_id: session.id.clone(),
                    };
                    let _ = self.engine.store.upsert_pr(&pr);
                    // Only reachable once per extra PR (the `get_pr` guard
                    // above stops matching once the row exists) — this must
                    // stay visible to the UI, not just recorded in the
                    // store, or an agent-created duplicate PR is invisible
                    // outside a one-off notification.
                    self.engine.emit(Event::ExtraPrDetected(pr));
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
        let mut sent = std::collections::HashSet::new();
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
                match self.engine.send_to_session(&orch, &msg).await {
                    Ok(()) => { sent.insert(request.id.clone()); }
                    Err(e) => tracing::warn!("send work request to orchestrator {orch}: {e}"),
                }
            }
        }
        // Moved out of the pending set even when the nudge failed — the UI
        // notification is already out, and retrying every tick would spam
        // both channels. The store row keeps `delivered_at` empty instead,
        // so the orchestrator's next recovery briefing carries it.
        let ids: Vec<String> = pending.iter().map(|r| r.id.clone()).collect();
        if let Err(e) = hooks::mark_work_requests_delivered(sessions_dir, &session.id, &ids) {
            tracing::warn!("mark work requests delivered for {}: {e}", session.id);
        }
        record_work_requests(&self.engine.store, session, &pending, &sent, now_millis());
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
            self.engine.emit(Event::SessionUpdated(
                session, SessionFields::COST | SessionFields::CONTEXT | SessionFields::MODEL,
            ));
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
            self.engine.emit(Event::SessionUpdated(
                session, SessionFields::COST | SessionFields::CONTEXT,
            ));
        }
    }

    /// The `ninox worker-status` subcommand (invoked by the worker's own
    /// UserPromptSubmit/Stop hooks, or by the agent explicitly) writes
    /// activity fields directly into the store from a separate short-lived
    /// process — the same external-writer shape as `poll_context_updates`
    /// above, so the same diff-cache re-broadcast, flagged ACTIVITY.
    async fn poll_activity_updates(&self) {
        let Ok(sessions) = self.engine.store.list_sessions() else { return };
        let mut changed = Vec::new();
        {
            let mut cache = self.activity_cache.lock().unwrap();
            for session in sessions {
                let key = (session.activity, session.activity_note.clone(), session.activity_since);
                // Seed silently on first sight — see `poll_context_updates`.
                if let Some(prev) = cache.insert(session.id.clone(), key.clone()) {
                    if prev != key {
                        changed.push(session);
                    }
                }
            }
        }
        for session in changed {
            self.engine.emit(Event::SessionUpdated(session, SessionFields::ACTIVITY));
        }
    }

    /// Emit `MessagesDelivered` for every session whose delivered-message
    /// count moved since the last tick. Both `ninox send` (another process)
    /// and this process's own reactions bump the counter, so polling the
    /// store is the one place that sees them all.
    async fn poll_message_counts(&self) {
        let Ok(counts) = self.engine.store.message_delivered_counts() else { return };
        let mut changed = Vec::new();
        {
            let mut cache = self.message_count_cache.lock().unwrap();
            // The first tick only takes a baseline: everything delivered
            // before this poller started is history, not news. After that a
            // session without a cache entry is genuinely new, and its whole
            // count is news — unlike `poll_context_updates`, whose per-key
            // silent seeding would swallow the first message to every
            // session spawned after startup.
            let Some(cache) = cache.as_mut() else {
                *cache = Some(counts);
                return;
            };
            for (session_id, total) in &counts {
                let prev = cache.insert(session_id.clone(), *total).unwrap_or(0);
                // A counter below its last value means the row was deleted
                // and recreated under the same id (orchestrator ids are
                // user slugs), so everything on it is new.
                let new = if *total < prev { *total } else { total - prev };
                if new > 0 {
                    changed.push((session_id.clone(), new));
                }
            }
            cache.retain(|session_id, _| counts.contains_key(session_id));
        }
        for (session_id, count) in changed {
            self.engine.emit(Event::MessagesDelivered { session_id, count });
        }
    }

    // ── GitHub enrichment ────────────────────────────────────────────────────

    /// Read-modify-write against the *live* session row rather than a
    /// tick-start `list_sessions()` snapshot. The external `ninox
    /// statusline` process writes cost/context straight into the row from
    /// its own process during `poll_github`'s GitHub awaits, and
    /// `upsert_session` is a full-row write — so writing a snapshot back
    /// would revert those fresher fields. Returns the row as written, or
    /// `None` (write skipped) when the session was deleted mid-tick:
    /// resurrecting a deleted row is worse than dropping one update. A
    /// read *error* falls back to the snapshot instead — a missed
    /// statusline write loses less than a skipped status update.
    fn update_live_session_row(
        &self,
        snapshot: &crate::types::Session,
        apply: impl FnOnce(&mut crate::types::Session),
    ) -> Option<crate::types::Session> {
        let mut row = match self.engine.store.get_session(&snapshot.id) {
            Ok(Some(row)) => row,
            Ok(None)      => return None,
            Err(_)        => snapshot.clone(),
        };
        let was_terminal = row.status.is_terminal();
        apply(&mut row);
        // Never resurrect a terminal row to a live status. Every caller
        // derives its new status from the tick-start *snapshot* (e.g.
        // `poll_github`'s `derive_session_status(&session.status, ...)`), so a
        // status that became terminal during this tick's awaits is invisible
        // to that decision. `ninox reap` runs in its own process and is the
        // first writer that can land a `status` write inside that window: it
        // kills the worker, deletes its worktree, and writes `Terminated`,
        // and this closure would then put the row back to `Mergeable`. That
        // row is unrecoverable — `sweep_retired_sessions` only purges
        // `Done`/`Terminated`, and `poll_pids` needs a `pid`, which a
        // CLI-spawned worker never has (`run_spawn` inserts `pid: None`) — so
        // it would sit on the fleet board as live forever, with no session
        // and no worktree behind it.
        if was_terminal && !row.status.is_terminal() {
            row.status = self.engine.store.get_session(&snapshot.id)
                .ok()
                .flatten()
                .map_or(row.status, |fresh| fresh.status);
        }
        let _ = self.engine.store.upsert_session(&row);
        Some(row)
    }

    /// Record the just-fetched PR branch refs for a session (both GitHub
    /// paths call this) so `reconcile_stacked_deps` can derive stacking.
    fn note_pr_refs(&self, session_id: &str, repo: &str, head_ref: &str, base_ref: &str) {
        if head_ref.is_empty() || base_ref.is_empty() {
            return; // REST/GraphQL data missing branch names — nothing to derive
        }
        self.pr_refs_cache.lock().unwrap().insert(
            session_id.to_string(),
            (repo.to_string(), head_ref.to_string(), base_ref.to_string()),
        );
    }

    /// Drop a session's cached PR refs when its PR lookup failed — the last
    /// good tuple would otherwise re-derive its stacked edges forever (the
    /// PR may be closed, retargeted, or gone). Transient failures cost only
    /// a flicker: the edge re-derives on the next successful fetch.
    fn evict_pr_refs(&self, session_id: &str) {
        self.pr_refs_cache.lock().unwrap().remove(session_id);
    }

    /// Re-derive the `stacked` dependency edges from the latest PR branch
    /// refs: session B stacks on session A when B's PR base branch is A's
    /// PR head branch in the same repo. Called at the end of each
    /// *successful* GitHub pass (never from a skipped/rate-limited tick, so
    /// an empty cache after a restart can't mass-clear persisted edges).
    /// Every live session gets a `set_stacked_deps` call — an empty target
    /// list for uncached sessions is what retires edges whose PR vanished.
    /// The store diffs per session, so an unchanged topology writes
    /// nothing; declared edges are never touched.
    fn reconcile_stacked_deps(&self) {
        let Ok(sessions) = self.engine.store.list_sessions() else { return };
        let refs = self.pr_refs_cache.lock().unwrap().clone();
        let live: Vec<&crate::types::Session> = sessions.iter()
            .filter(|s| !s.status.is_terminal())
            .collect();
        let entries: Vec<(String, String, String, String)> = live.iter()
            .filter_map(|s| refs.get(&s.id).map(|(repo, head, base)| {
                (s.id.clone(), repo.clone(), head.clone(), base.clone())
            }))
            .collect();
        let derived = derive_stacked_edges(&entries);
        for session in live {
            let depends_on = derived.iter()
                .find(|(id, _)| *id == session.id)
                .map(|(_, targets)| targets.clone())
                .unwrap_or_default();
            if let Err(e) = self.engine.store.set_stacked_deps(&session.id, &depends_on, now_millis()) {
                tracing::warn!("stacked-deps reconcile failed for {}: {e}", session.id);
            }
        }
    }

    /// `auto_reap` mirrors `[auto_reap].enabled` — passed in by `start()`'s
    /// tick (like `sweep_retired_sessions`'s retention) rather than read
    /// from `AppConfig::load()` here, so tests control it deterministically.
    async fn poll_github(&self, auto_reap: bool) {
        let Some(gh) = &self.engine.github else { return };
        let Ok(sessions) = self.engine.store.list_sessions() else { return };

        for mut session in sessions {
            // Merge-handled sessions are excluded here (`Done`, or a
            // `merged_at` stamp for a worker kept alive after merge — see
            // `Session::merge_handled`). `Terminated` (the worker's own
            // process exited, typically once its PR is merely *open*) and
            // `Interrupted` sessions may still have a PR whose fate hasn't
            // resolved yet, so they must keep being polled or a later merge
            // becomes permanently invisible (no status update, no
            // notification) the instant the process dies.
            if session.merge_handled() {
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
                self.evict_pr_refs(&session.id);
                continue;
            };
            // The GitHub round-trips above can outlive this session's
            // incarnation (a Resume/Re-file replaces it mid-tick) — self-
            // healing or merge-detecting against the stale snapshot below
            // would apply this tick's findings to a successor session that
            // never asked for them. Bail if the live row no longer matches
            // the tick-start snapshot's started_at/pr_number.
            if !self
                .engine
                .store
                .get_session(&session.id)
                .ok()
                .flatten()
                .is_some_and(|current| {
                    current.started_at == session.started_at
                        && current.pr_number == session.pr_number
                })
            {
                continue;
            }
            self.clear_github_lookup_failed(&session.id);
            self.note_pr_refs(&session.id, &resolved_repo, &pr_status.head_ref, &pr_status.base_ref);

            let pr_id: PrId = pr_number as i64;

            // Self-heal (repo/pr_number) and pr_id are persisted together,
            // in one write, before merge detection below — which can
            // `continue` the loop and move the session to `Done`, a status
            // this function never revisits (see the guard at the top of the
            // loop). If pr_id were only set further down (after merge
            // detection), a PR that turns out to already be merged on the
            // very tick it's discovered would leave the session stuck with
            // pr_number set but pr_id permanently None/stale — same
            // flicker-causing inconsistency as the bug this function is
            // otherwise fixing, just for a session that never gets polled
            // again to self-correct.
            if resolved_repo != session.repo
                || Some(pr_number) != session.pr_number
                || session.pr_id != Some(pr_id)
            {
                if resolved_repo != session.repo || Some(pr_number) != session.pr_number {
                    tracing::info!(
                        "session {} repo/PR corrected {}#{:?} -> {resolved_repo}#{pr_number}",
                        session.id, session.repo, session.pr_number,
                    );
                }
                session.repo = resolved_repo;
                session.pr_number = Some(pr_number);
                session.pr_id = Some(pr_id);
                let written = self.update_live_session_row(&session, |row| {
                    row.repo = session.repo.clone();
                    row.pr_number = session.pr_number;
                    row.pr_id = session.pr_id;
                });
                if written.is_some() {
                    // This emit is the *only* channel by which self-healed pr
                    // fields reach the GUI — the status/gate emit further down
                    // deliberately flags only STATUS|GATE (it persists nothing
                    // else). PR_LINK alone restricts the receiving `merge_from`
                    // to exactly the three fields this write just corrected.
                    self.engine.emit(Event::SessionUpdated(
                        session.clone(), SessionFields::PR_LINK,
                    ));
                }
            }
            let Some((owner, repo)) = split_repo(&session.repo) else { continue };

            // -- Merge detection — handle before CI (no point polling CI on merged PR) --
            if self.handle_merge_detection(&session, pr_number, pr_status.merged, auto_reap).await {
                // Skips the gate-computation block below — a merged session's
                // gate_status is intentionally left frozen at its last
                // pre-merge value, not recomputed at the merging tick.
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
            let ci = self.ingest_ci(&session, pr_id, &checks, pr_status.mergeable).await;

            // -- Review threads + issue comments (throttled via seen_comment_ids) --
            let threads = match gh.get_review_threads(&owner, &repo, pr_number).await {
                Ok(t)  => t,
                Err(e) => { tracing::warn!("github review threads: {e}"); vec![] }
            };
            let issue_comments = match gh.get_issue_comments(&owner, &repo, pr_number).await {
                Ok(c)  => c,
                Err(e) => { tracing::warn!("github issue comments: {e}"); vec![] }
            };
            let (has_new, review_reaction_already_sent, new_comments, has_changes_requested) =
                self.scan_reviews(&session.id, pr_id, &threads, &issue_comments);

            self.apply_status_and_gate(&session, &pr_status, &ci, has_changes_requested);

            self.emit_review_reaction(&session, has_new, review_reaction_already_sent, &new_comments).await;
        }
        self.reconcile_stacked_deps();
    }

    // ── Batched GitHub enrichment (behind `[pr_watch] enabled`) ─────────────

    /// One tick of the batched path: collect every PR the app cares about
    /// (session PRs + registry watches) plus every branch still awaiting PR
    /// adoption, fetch them all in a single `GithubBatchApi::fetch_batch`
    /// call, then run the *same* enrichment helpers `poll_github` uses over
    /// the returned snapshots. It replaces both `poll_pr_reconciliation` and
    /// `poll_github` for the tick — `start()` calls one or the other, never
    /// both, so the legacy REST path stays byte-for-byte what it was when the
    /// toggle is off.
    ///
    /// Deliberate simplification vs the legacy path, called out because it is
    /// a behavior change and not an oversight: the legacy cross-repo 404
    /// fallback (`poll_github` re-matching the session's *branch* against
    /// every other configured remote when the recorded repo 404s, then
    /// self-healing `session.repo`/`pr_number`) is NOT replicated here. A
    /// session whose recorded `(repo, number)` alias comes back missing gets
    /// the existing deduped `GithubLookupFailed` notification instead. That
    /// fallback exists for repo/PR-number drift, which self-heals on the
    /// legacy path; anyone actually hitting it can flip `[pr_watch] enabled`
    /// off to get it back.
    /// `auto_reap` mirrors `[auto_reap].enabled`, passed in by `start()`'s
    /// tick — see `poll_github`.
    async fn poll_github_batched(&self, auto_reap: bool) {
        // A skipped tick, never a blocked task: checked before anything
        // else, including `self.engine.github_batch`'s own presence check,
        // so a pause set by `note_rate_limit`/`note_batch_error` costs
        // nothing beyond this one lock+compare every interval tick.
        if now_millis() < *self.rate_limit_pause_until.lock().unwrap_or_else(|e| e.into_inner()) {
            return;
        }
        let Some(batch) = &self.engine.github_batch else { return };
        let Ok(sessions) = self.engine.store.list_sessions() else { return };
        let watches = self.engine.store.list_pr_watches().unwrap_or_default();

        // -- Collect targets --------------------------------------------------
        // Session PRs: same skip rule as poll_github — merge-handled
        // sessions are excluded (see `Session::merge_handled`).
        let mut pr_keys: Vec<PrKey> = Vec::new();
        // Insertion-ordered, deduped: `candidate_repos` sorts `origin` first,
        // and the adoption loop below walks this Vec (not the result HashMap)
        // so a session with several remotes deterministically adopts from the
        // *first* one that has a PR — matching `poll_pr_reconciliation`'s
        // origin-first, break-on-first-match behavior. Dedup is done with a
        // seen-set while building rather than `sort`+`dedup`, which would
        // destroy that origin-first ordering.
        let mut branch_keys: Vec<BranchKey> = Vec::new();
        let mut seen_branch_keys: std::collections::HashSet<BranchKey> = std::collections::HashSet::new();
        // Every session awaiting adoption on a given (repo, branch). Several
        // sessions can share one workspace (and so one key) — all of them must
        // adopt, exactly as the legacy per-session reconciliation loop does.
        let mut branch_owners: HashMap<BranchKey, Vec<String>> = HashMap::new();

        for session in &sessions {
            if session.merge_handled() {
                continue;
            }
            match session.pr_number {
                Some(n) if !session.repo.is_empty() => {
                    pr_keys.push(PrKey { repo: session.repo.clone(), number: n });
                }
                None if !matches!(
                    session.status,
                    SessionStatus::Terminated | SessionStatus::Interrupted
                ) => {
                    // poll_pr_reconciliation equivalent, batched.
                    if let Some(ws) = &session.workspace_path {
                        if let Some(branch) = crate::github::current_branch(ws) {
                            for repo_slug in crate::github::candidate_repos(ws) {
                                let key = BranchKey { repo: repo_slug, branch: branch.clone() };
                                branch_owners.entry(key.clone()).or_default().push(session.id.clone());
                                if seen_branch_keys.insert(key.clone()) {
                                    branch_keys.push(key);
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        for w in &watches {
            pr_keys.push(PrKey { repo: w.repo.clone(), number: w.pr_number });
        }
        pr_keys.sort_by(|a, b| (&a.repo, a.number).cmp(&(&b.repo, b.number)));
        pr_keys.dedup();
        if pr_keys.is_empty() && branch_keys.is_empty() {
            return;
        }

        // -- One batched fetch ------------------------------------------------
        let result = match batch.fetch_batch(&pr_keys, &branch_keys).await {
            Ok(r) => r,
            Err(e) => {
                self.note_batch_error(&e);
                return;
            }
        };
        self.note_rate_limit(&result.rate_limit);

        // -- Branch adoption (replaces poll_pr_reconciliation) ----------------
        // Driven by `branch_keys` (insertion-ordered, origin-first) rather
        // than `result.branch_prs` (a HashMap with arbitrary iteration order),
        // so which remote a multi-remote session adopts from is deterministic.
        for key in &branch_keys {
            let Some(Some(pr_ref)) = result.branch_prs.get(key) else { continue };
            let Some(owner_ids) = branch_owners.get(key) else { continue };
            for session_id in owner_ids {
                let Ok(Some(mut session)) = self.engine.store.get_session(session_id) else { continue };
                if session.pr_number.is_some() {
                    // Already adopted — either via an earlier (higher-priority)
                    // key this tick, which is what makes the first matching
                    // remote win, or before this tick entirely.
                    continue;
                }
                session.pr_number = Some(pr_ref.number);
                session.repo      = key.repo.clone();
                session.status    = SessionStatus::PrOpen;
                if let Some(written) = self.update_live_session_row(&session, |row| {
                    row.pr_number = session.pr_number;
                    row.repo      = session.repo.clone();
                    row.status    = session.status.clone();
                }) {
                    self.engine.emit(Event::SessionUpdated(
                        written,
                        SessionFields::PR_LINK | SessionFields::STATUS,
                    ));
                    tracing::info!(
                        "session {} PR #{} detected via reconciliation ({}, branch {})",
                        session.id, pr_ref.number, key.repo, key.branch,
                    );
                }
            }
        }

        // -- Session enrichment (replaces poll_github's per-session fetches) --
        for mut session in sessions {
            if session.merge_handled() {
                continue;
            }
            let Some(pr_number) = session.pr_number else { continue };
            if session.repo.is_empty() {
                continue;
            }
            let key = PrKey { repo: session.repo.clone(), number: pr_number };
            let Some(snap) = result.prs.get(&key) else {
                self.notify_github_lookup_failed(&session);
                self.evict_pr_refs(&session.id);
                continue;
            };
            self.clear_github_lookup_failed(&session.id);
            self.note_pr_refs(&session.id, &session.repo, &snap.status.head_ref, &snap.status.base_ref);
            let pr_id: PrId = pr_number as i64;
            if session.pr_id != Some(pr_id) {
                session.pr_id = Some(pr_id);
                if self.update_live_session_row(&session, |row| row.pr_id = Some(pr_id)).is_some() {
                    self.engine.emit(Event::SessionUpdated(session.clone(), SessionFields::PR_LINK));
                }
            }
            if self.handle_merge_detection(&session, pr_number, snap.status.merged, auto_reap).await {
                continue;
            }
            let Some((owner, repo)) = split_repo(&session.repo) else { continue };
            let pr = PR {
                id:         pr_id,
                number:     pr_number,
                title:      snap.status.title.clone(),
                url:        format!("https://github.com/{owner}/{repo}/pull/{pr_number}"),
                body:       String::new(),
                session_id: session.id.clone(),
            };
            let _ = self.engine.store.upsert_pr(&pr);
            self.engine.emit(Event::PrOpened { session_id: session.id.clone(), pr });

            let ci = self.ingest_ci(&session, pr_id, &snap.checks, snap.status.mergeable).await;
            let (has_new, already_sent, new_comments, has_changes_requested) =
                self.scan_reviews(&session.id, pr_id, &snap.threads, &snap.issue_comments);
            self.apply_status_and_gate(&session, &snap.status, &ci, has_changes_requested);
            self.emit_review_reaction(&session, has_new, already_sent, &new_comments).await;
        }

        self.deliver_watch_updates(&watches, &result).await;
        self.reconcile_stacked_deps();
    }

    /// A whole batched fetch failed — every target this tick is unobserved.
    /// `RateLimitedError` (403/429 from GitHub's GraphQL endpoint) pauses
    /// `poll_github_batched` for its `Retry-After` hint, or an exponentially
    /// doubling backoff (120s, 240s, 480s, ... capped at 3600s) when GitHub
    /// sent none — consecutive Retry-After-less errors keep doubling
    /// `rate_limit_backoff_secs` instead of re-pausing for a flat 120s
    /// every time, so a sustained outage backs off instead of hammering
    /// GitHub every two minutes. A Retry-After-bearing error uses GitHub's
    /// own value and leaves the stored backoff untouched — it isn't a
    /// signal about the *next* Retry-After-less error's pause. Any other
    /// error (a transient network blip, say) is logged only — polling must
    /// not stall over something that will very likely have cleared up by
    /// the next tick. `{e:#}` logs the full `anyhow` cause chain, not just
    /// the top-level message, so a sustained outage (which otherwise
    /// notifies nobody — watches/sessions simply go quiet) is still
    /// diagnosable from logs alone.
    fn note_batch_error(&self, e: &anyhow::Error) {
        if let Some(rl) = e.downcast_ref::<crate::github_graphql::RateLimitedError>() {
            let pause_until = match rl.retry_after_secs {
                Some(secs) => now_millis() + secs as i64 * 1000,
                None       => {
                    let mut backoff = self.rate_limit_backoff_secs
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    let next = if *backoff == 0 { 120 } else { (*backoff * 2).min(3600) };
                    *backoff = next;
                    now_millis() + next as i64 * 1000
                }
            };
            *self.rate_limit_pause_until.lock().unwrap_or_else(|e| e.into_inner()) = pause_until;
            tracing::warn!("github batch fetch rate limited, pausing until {pause_until}: {e:#}");
        } else {
            tracing::warn!("github batch fetch failed (no pause; retrying next tick): {e:#}");
        }
    }

    /// Record the GraphQL rate-limit budget reported by the last fetch. Once
    /// the remaining budget drops below a small floor, pause
    /// `poll_github_batched` until GitHub's own reset time rather than
    /// grinding the remaining quota to zero. Warns only when newly pausing
    /// (not on every low-budget tick): once paused, `poll_github_batched`'s
    /// own pause check stops this from being called again until the pause
    /// has actually elapsed, so re-warning here would only fire for a pause
    /// that's already expired — i.e. genuinely new.
    fn note_rate_limit(&self, rate_limit: &crate::github_graphql::RateLimitInfo) {
        // This only ever runs after a successful fetch (the caller returns
        // early on error before reaching here) — reset the Retry-After-less
        // backoff so a fresh outage after a recovery starts doubling over
        // from 120s again, not from wherever the last outage left off.
        *self.rate_limit_backoff_secs.lock().unwrap_or_else(|e| e.into_inner()) = 0;
        if rate_limit.remaining >= 100 || rate_limit.reset_at <= now_millis() {
            return;
        }
        let mut pause = self.rate_limit_pause_until.lock().unwrap_or_else(|e| e.into_inner());
        let already_paused = *pause > now_millis();
        *pause = rate_limit.reset_at;
        if !already_paused {
            tracing::warn!(
                "github rate limit low ({} remaining, cost {}) — pausing batched polling until {}",
                rate_limit.remaining, rate_limit.cost, rate_limit.reset_at,
            );
        }
    }

    /// Fan the batched snapshots out to the registry's PR watches: merge/close
    /// (terminal, and auto-closing), newly-failing CI and new
    /// CHANGES_REQUESTED review activity.
    ///
    /// Watches are *notification-only*. They deliver a `Notification` (feed +
    /// desktop) and a tmux reaction to the opener session, and nothing else —
    /// no session status/gate change, no `cleanup_session`, and none of the
    /// session-owned store rows (`upsert_pr`/`upsert_ci_status`/
    /// `upsert_comment`). Lifecycle transitions stay exclusive to
    /// session-attached PRs, which is why this deliberately doesn't route
    /// through `ingest_ci`/`scan_reviews` despite the transition logic
    /// looking alike: those write the session's rows and fire the session's
    /// notifications, neither of which a watch is allowed to touch.
    ///
    /// Dedup rides on the same `enrichment_cache` under a synthetic key
    /// (`watch:{repo}#{number}:{opener}`), so a watch on a PR that is *also*
    /// some session's tracked PR keeps its own independent transition state
    /// instead of stealing/clobbering the session's.
    async fn deliver_watch_updates(
        &self,
        watches: &[crate::types::PrWatch],
        result: &crate::github_graphql::BatchResult,
    ) {
        let mut terminal_prs: Vec<(String, u64)> = Vec::new();
        for w in watches {
            let key = PrKey { repo: w.repo.clone(), number: w.pr_number };
            let cache_key = format!(
                "watch:{}#{}:{}",
                w.repo, w.pr_number, w.opener_session_id.as_deref().unwrap_or(""),
            );
            let Some(snap) = result.prs.get(&key) else {
                // The PR vanished from the batch result (deleted/renamed repo,
                // access revoked, etc.) without an explicit batch error — the
                // watch has no terminal signal to act on, so it stays
                // registered forever unless the user runs `ninox close --pr`.
                // Warn once per run of consecutive misses via the same
                // dedup flag `notify_github_lookup_failed` uses, but log only
                // — no notification event (that stays session-scoped).
                let mut cache = self.enrichment_cache.lock().unwrap();
                let state = cache.entry(cache_key.clone()).or_default();
                if !state.github_lookup_failed_notified {
                    tracing::warn!(
                        "watch {}#{}: PR absent from batch result (deleted/renamed repo?); \
                         watch stays until `ninox close --pr`",
                        w.repo, w.pr_number,
                    );
                    state.github_lookup_failed_notified = true;
                }
                continue;
            };
            {
                let mut cache = self.enrichment_cache.lock().unwrap();
                if let Some(state) = cache.get_mut(&cache_key) {
                    state.github_lookup_failed_notified = false;
                }
            }

            if snap.status.merged || snap.closed {
                self.engine.emit(Event::Notification(Notification {
                    id:         format!("watch-done-{cache_key}"),
                    kind:       NotificationKind::WorkerDone,
                    title:      format!(
                        "Watched PR {} — {}#{}",
                        if snap.status.merged { "merged" } else { "closed" },
                        w.repo, w.pr_number,
                    ),
                    body:       w.pr_url.clone(),
                    session_id: w.opener_session_id.clone(),
                    created_at: now_millis(),
                }));
                if let Some(opener) = &w.opener_session_id {
                    let msg = crate::lifecycle::reactions::format_watched_pr_terminal(
                        &w.repo, w.pr_number, snap.status.merged,
                    );
                    if let Err(e) = self.engine.send_to_session(opener, &msg).await {
                        tracing::warn!("send watched-pr terminal reaction to {opener}: {e}");
                    }
                }
                self.enrichment_cache.lock().unwrap().remove(&cache_key);
                terminal_prs.push((w.repo.clone(), w.pr_number));
                continue;
            }

            // CI transition — the same newly-failing logic as `ingest_ci`,
            // against the watch's own cache entry.
            let ci = summarize_checks(w.pr_number as PrId, &snap.checks);
            let (newly_failing, ci_already_sent) = {
                let mut cache = self.enrichment_cache.lock().unwrap();
                let state = cache.entry(cache_key.clone()).or_default();
                let newly_failing = state.prev_failing.is_none_or(|p| p == 0) && ci.failing > 0;
                state.prev_failing = Some(ci.failing);
                let already = state.ci_reaction_sent;
                if newly_failing && !already {
                    state.ci_reaction_sent = true;
                }
                if ci.failing == 0 {
                    state.ci_reaction_sent = false;
                }
                (newly_failing, already)
            };
            if newly_failing && !ci_already_sent {
                self.engine.emit(Event::Notification(Notification {
                    id:         format!("watch-ci-{cache_key}"),
                    kind:       NotificationKind::CiFailure,
                    title:      format!("Watched PR CI failing — {}#{}", w.repo, w.pr_number),
                    body:       format!("{}/{} checks failing", ci.failing, ci.total),
                    session_id: w.opener_session_id.clone(),
                    created_at: now_millis(),
                }));
                if let Some(opener) = &w.opener_session_id {
                    let failing_names: Vec<String> = snap.checks.iter()
                        .filter(|c| c.conclusion.as_deref() == Some("failure")
                                 || c.conclusion.as_deref() == Some("timed_out"))
                        .map(|c| c.name.clone())
                        .collect();
                    let msg = crate::lifecycle::reactions::format_watched_pr_ci(
                        &w.repo, w.pr_number, &ci, &failing_names,
                    );
                    if let Err(e) = self.engine.send_to_session(opener, &msg).await {
                        tracing::warn!("send watched-pr ci reaction to {opener}: {e}");
                    }
                }
            }

            // Ready-to-merge transition — the same newly-ready logic as
            // `ingest_ci`, against the watch's own cache entry.
            let is_ready = ci.failing == 0 && ci.pending == 0 && snap.status.mergeable == Some(true);
            let (newly_ready, ready_already_sent) = {
                let mut cache = self.enrichment_cache.lock().unwrap();
                let state = cache.entry(cache_key.clone()).or_default();
                let newly_ready = state.prev_ready.is_none_or(|p| !p) && is_ready;
                state.prev_ready = Some(is_ready);
                let already = state.ready_reaction_sent;
                if newly_ready && !already {
                    state.ready_reaction_sent = true;
                }
                if !is_ready {
                    state.ready_reaction_sent = false;
                }
                (newly_ready, already)
            };
            if newly_ready && !ready_already_sent {
                self.engine.emit(Event::Notification(Notification {
                    id:         format!("watch-ready-{cache_key}"),
                    kind:       NotificationKind::PrReadyToMerge,
                    title:      format!("Watched PR ready to merge — {}#{}", w.repo, w.pr_number),
                    body:       format!("{}/{} checks passing, mergeable", ci.passing, ci.total),
                    session_id: w.opener_session_id.clone(),
                    created_at: now_millis(),
                }));
                if let Some(opener) = &w.opener_session_id {
                    let msg = crate::lifecycle::reactions::format_watched_pr_ready(
                        &w.repo, w.pr_number, &ci,
                    );
                    if let Err(e) = self.engine.send_to_session(opener, &msg).await {
                        tracing::warn!("send watched-pr ready reaction to {opener}: {e}");
                    }
                }
            }

            // New CHANGES_REQUESTED review activity — `seen_comment_ids` dedup
            // on the watch's own cache entry (store writes for comments are
            // the session path's job; watches only notify).
            let new_comments: Vec<Comment> = {
                let mut cache = self.enrichment_cache.lock().unwrap();
                let state = cache.entry(cache_key.clone()).or_default();
                snap.threads.iter()
                    .filter(|t| t.state == "CHANGES_REQUESTED")
                    .filter(|t| state.seen_comment_ids.insert(t.id))
                    .map(|t| Comment {
                        id:         t.id,
                        pr_id:      w.pr_number as PrId,
                        author:     t.author.clone(),
                        body:       t.body.clone(),
                        path:       t.path.clone(),
                        line:       t.line,
                        created_at: t.created_at,
                    })
                    .collect()
            };
            if !new_comments.is_empty() {
                self.engine.emit(Event::Notification(Notification {
                    id:         format!("watch-review-{cache_key}"),
                    kind:       NotificationKind::PrNeedsAttention,
                    title:      format!("Watched PR review — {}#{}", w.repo, w.pr_number),
                    body:       "Changes requested".to_string(),
                    session_id: w.opener_session_id.clone(),
                    created_at: now_millis(),
                }));
                if let Some(opener) = &w.opener_session_id {
                    let msg = crate::lifecycle::reactions::format_watched_pr_review(
                        &w.repo, w.pr_number, &new_comments,
                    );
                    if let Err(e) = self.engine.send_to_session(opener, &msg).await {
                        tracing::warn!("send watched-pr review reaction to {opener}: {e}");
                    }
                }
            }
        }

        // Auto-close: every watch on a PR that reached a terminal state goes,
        // whoever opened it — deduped so N openers cost one DELETE.
        terminal_prs.sort();
        terminal_prs.dedup();
        for (repo, number) in terminal_prs {
            if let Err(e) = self.engine.store.delete_pr_watches_for_pr(&repo, number) {
                tracing::warn!("auto-close watches for {repo}#{number}: {e}");
            }
        }
    }

    /// The CI block: summarize → upsert → `CiUpdated` emit → newly-failing
    /// transition (via `enrichment_cache`) → `CiFailure` notification +
    /// `format_ci_reaction` sent into the session's tmux, plus the symmetric
    /// newly-ready transition → `PrReadyToMerge` notification +
    /// `format_ready_to_merge_reaction`. Takes already-fetched checks (the
    /// caller owns the `get_ci_checks` network call) so a non-REST caller can
    /// reuse this. Returns the computed `CIStatus`.
    async fn ingest_ci(
        &self, session: &Session, pr_id: PrId, checks: &[CheckRun], mergeable: Option<bool>,
    ) -> CIStatus {
        let ci = summarize_checks(pr_id, checks);
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
                session, &ci, &failing_names
            );
            if let Err(e) = self.engine.send_to_session(&session.id, &msg).await {
                tracing::warn!("send ci reaction to {}: {e}", session.id);
            }
        }

        // -- Detect the symmetric newly-ready transition --
        let is_ready = ci.failing == 0 && ci.pending == 0 && mergeable == Some(true);
        let (newly_ready, ready_reaction_already_sent) = {
            let mut cache = self.enrichment_cache.lock().unwrap();
            let state = cache.entry(session.id.clone()).or_default();

            let newly_ready = state.prev_ready.is_none_or(|p| !p) && is_ready;
            state.prev_ready = Some(is_ready);

            let already_sent = state.ready_reaction_sent;
            if newly_ready && !already_sent {
                state.ready_reaction_sent = true;
            }
            if !is_ready {
                state.ready_reaction_sent = false;
            }
            (newly_ready, already_sent)
        };

        if newly_ready && !ready_reaction_already_sent {
            self.engine.emit(Event::Notification(Notification {
                id:         format!("ready-{}", session.id),
                kind:       NotificationKind::PrReadyToMerge,
                title:      format!("Ready to merge — {}", session.name),
                body:       format!("{}/{} checks passing, mergeable", ci.passing, ci.total),
                session_id: Some(session.id.clone()),
                created_at: now_millis(),
            }));
            let msg = crate::lifecycle::reactions::format_ready_to_merge_reaction(session, &ci);
            if let Err(e) = self.engine.send_to_session(&session.id, &msg).await {
                tracing::warn!("send ready-to-merge reaction to {}: {e}", session.id);
            }
        }

        ci
    }

    /// The review-scan block: dedup via `seen_comment_ids`, then
    /// `upsert_comment` and `ReviewComment` emits. Pure w.r.t. the network —
    /// the caller owns the `get_review_threads`/`get_issue_comments`
    /// fetches. Returns `(has_new, review_reaction_already_sent,
    /// new_comments, has_changes_requested)`.
    fn scan_reviews(
        &self,
        session_id: &str,
        pr_id: PrId,
        threads: &[crate::github::ReviewThread],
        issue_comments: &[Comment],
    ) -> (bool, bool, Vec<Comment>, bool) {
        let has_changes_requested = threads.iter().any(|t| t.state == "CHANGES_REQUESTED");

        let (has_new, review_reaction_already_sent, new_comments) = {
            let mut cache = self.enrichment_cache.lock().unwrap();
            let state = cache.entry(session_id.to_string()).or_default();
            let mut has_new = false;
            let mut new_comments: Vec<Comment> = Vec::new();

            // Persist + emit every displayable comment — CHANGES_REQUESTED
            // and plain COMMENTED reviews (which also covers inline diff
            // comments, tagged COMMENTED by `get_review_threads`) — so the
            // Info panel's Marginalia feed shows the whole conversation.
            // `has_new`/`new_comments` stay CHANGES_REQUESTED-only: they
            // drive the reaction/notification path below, which must not
            // widen just because the display feed did. A bare
            // empty-body "Comment" review (whose only content is inline
            // comments, already captured separately) is skipped so the
            // feed doesn't show blank entries — but never for
            // CHANGES_REQUESTED, which must keep being captured (and
            // reacted to) exactly as before regardless of body content.
            for thread in threads {
                let is_changes_requested = thread.state == "CHANGES_REQUESTED";
                let is_displayable = is_changes_requested || thread.state == "COMMENTED";
                if !is_displayable || state.seen_comment_ids.contains(&thread.id) {
                    continue;
                }
                if !is_changes_requested && thread.body.trim().is_empty() {
                    continue;
                }
                state.seen_comment_ids.insert(thread.id);
                let comment = Comment {
                    id:         thread.id,
                    pr_id,
                    author:     thread.author.clone(),
                    body:       thread.body.clone(),
                    path:       thread.path.clone(),
                    line:       thread.line,
                    created_at: thread.created_at,
                };
                let _ = self.engine.store.upsert_comment(&comment);
                self.engine.emit(Event::ReviewComment { pr_id, comment: comment.clone() });
                if is_changes_requested {
                    has_new = true;
                    new_comments.push(comment);
                }
            }

            for issue_comment in issue_comments {
                if state.seen_comment_ids.contains(&issue_comment.id) {
                    continue;
                }
                state.seen_comment_ids.insert(issue_comment.id);
                let comment = Comment { pr_id, ..issue_comment.clone() };
                let _ = self.engine.store.upsert_comment(&comment);
                self.engine.emit(Event::ReviewComment { pr_id, comment });
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

        (has_new, review_reaction_already_sent, new_comments, has_changes_requested)
    }

    /// The status/gate write block: derive the new status/gate, write
    /// through the live session row, and emit `SessionUpdated(STATUS |
    /// GATE)` only when something actually changed.
    fn apply_status_and_gate(
        &self,
        session: &Session,
        pr_status: &crate::github::PrStatus,
        ci: &CIStatus,
        has_changes_requested: bool,
    ) {
        // Update session status in DB (after review threads so has_changes_requested is known)
        let new_status = derive_session_status(&session.status, pr_status, ci, has_changes_requested);
        let new_gate = compute_new_gate(
            &session.status, ci, has_changes_requested, pr_status.mergeable,
            session.gate_status.as_ref(), now_millis(),
        );
        if new_status != session.status || new_gate != session.gate_status {
            // `session` is the tick-start snapshot, several GitHub
            // awaits old — write through the live row instead (see
            // `update_live_session_row`), and skip both write and emit
            // if the session was deleted mid-tick.
            if let Some(updated) = self.update_live_session_row(session, |row| {
                row.status = new_status;
                row.gate_status = new_gate;
            }) {
                self.engine.emit(Event::SessionUpdated(
                    updated,
                    SessionFields::STATUS | SessionFields::GATE,
                ));
            }
        }
    }

    /// The review notification/reaction block: `PrNeedsAttention`
    /// notification plus `format_review_reaction` sent into the session's
    /// tmux for newly-seen CHANGES_REQUESTED comments.
    async fn emit_review_reaction(
        &self,
        session: &Session,
        has_new: bool,
        already_sent: bool,
        new_comments: &[Comment],
    ) {
        if has_new && !already_sent {
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
                    session, new_comments
                );
                if let Err(e) = self.engine.send_to_session(&session.id, &msg).await {
                    tracing::warn!("send review reaction to {}: {e}", session.id);
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
                        // Write through the live row, not the tick-start
                        // snapshot — `find_open_pr_for_branch` is a network
                        // await, plenty of time for the statusline process to
                        // land cost/context (see `update_live_session_row`).
                        if let Some(written) = self.update_live_session_row(&session, |row| {
                            row.pr_number = session.pr_number;
                            row.repo      = session.repo.clone();
                            row.status    = session.status.clone();
                        }) {
                            self.engine.emit(Event::SessionUpdated(
                                written, SessionFields::PR_LINK | SessionFields::STATUS,
                            ));
                            tracing::info!(
                                "session {} PR #{} detected via reconciliation ({repo_slug}, branch {branch})",
                                session.id, pr_ref.number,
                            );
                        }
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
    /// Handle a freshly-fetched PR status's merge state for `session`.
    /// Returns `true` when the merge was handled this call (the caller
    /// should skip further CI/review enrichment for the session this tick);
    /// `false` when there's nothing to do (not merged, or already `Done` —
    /// reusing the status transition itself as the one-shot dedup guard, so
    /// a session can never fire this reaction twice).
    ///
    /// Extracted from `poll_github` so it's directly unit-testable without a
    /// live GitHub client — `poll_github` only needs the client to fetch
    /// `pr_status` in the first place; this handles the resulting state
    /// unconditionally.
    async fn handle_merge_detection(
        &self,
        session:    &Session,
        pr_number:  u64,
        pr_merged:  bool,
        auto_reap:  bool,
    ) -> bool {
        if !pr_merged || matches!(session.status, SessionStatus::Done) {
            return false;
        }
        if session.merged_at.is_some() {
            // Merge already handled on an earlier tick with `[auto_reap]`
            // off — the worker was deliberately kept alive, so it keeps
            // surfacing here until it's reaped. Skip enrichment without
            // re-notifying.
            return true;
        }
        // Keep the worker alive for post-merge validation only when
        // `[auto_reap]` is off AND the worker is actually still running.
        // Merge detection deliberately also fires for `Terminated`/
        // `Interrupted` sessions — a worker's process commonly exits while
        // its PR is merely open, then the PR merges later (see the skip
        // guard in `poll_github`). There's no live agent in a dead worker
        // to hand validation to and no reason to preserve its worktree, so
        // those get the same cleanup + plain done-reaction as the auto-reap
        // path regardless of the toggle.
        let keep_alive = !auto_reap && !session.status.is_terminal();
        self.engine.emit(Event::Notification(Notification {
            id:         format!("merged-{}", session.id),
            kind:       NotificationKind::WorkerDone,
            title:      format!("PR merged — {}", session.name),
            body:       format!("#{} merged successfully", pr_number),
            session_id: Some(session.id.clone()),
            created_at: now_millis(),
        }));
        // Code-level completion guarantee for the orchestrator — independent
        // of whether the worker's own agent ever reports back before exiting.
        if let Some(orch) = session.orchestrator_id.clone() {
            let msg = if keep_alive {
                crate::lifecycle::reactions::format_worker_done_kept_alive_reaction(session, pr_number)
            } else {
                crate::lifecycle::reactions::format_worker_done_reaction(session, pr_number)
            };
            if let Err(e) = self.engine.send_to_session(&orch, &msg).await {
                tracing::warn!("send worker-done reaction to orchestrator {orch}: {e}");
            }
        }
        if keep_alive {
            // The stamp is what makes the notification above once-only and
            // drops the session out of GitHub enrichment (see
            // `Session::merged_at`). A failed write is load-bearing here —
            // it's the ONLY dedup for this path — so log it loudly, unlike
            // a cosmetic field write.
            match self.update_live_session_row(session, |row| {
                row.merged_at = Some(now_millis());
            }) {
                Some(row) => self.engine.emit(
                    Event::SessionUpdated(row, SessionFields::MERGED_AT),
                ),
                None => tracing::warn!(
                    "merge-detected session {} could not be stamped merged_at — \
                     the merge may re-notify on the next tick",
                    session.id,
                ),
            }
        } else {
            let worker = match self
                .engine
                .store
                .worker_incarnation_for_snapshot(&session.id, session.started_at)
            {
                Ok(worker) => worker,
                Err(error) => {
                    tracing::warn!("resolve merge capability for {}: {error}", session.id);
                    return false;
                }
            };
            if let Some(worker) = worker {
                if let Err(error) = self.engine.store.retain_worker_after_merge(
                    &session.id,
                    &worker.incarnation_id,
                    session.started_at,
                    pr_number,
                    now_millis(),
                ) {
                    tracing::warn!("retain merged worker {}: {error}", session.id);
                }
            } else if let Err(e) = self.engine.cleanup_session(&session.id).await {
                tracing::warn!("cleanup_session {}: {e}", session.id);
            }
        }
        // Remove enrichment state for this session — it's done
        {
            let mut cache = self.enrichment_cache.lock().unwrap();
            cache.remove(&session.id);
        }
        true
    }

    // ── Retention sweep ──────────────────────────────────────────────────────

    /// Purge `Done`/`Terminated` session records whose terminal state was
    /// reached more than `retention`'s window ago — gives the fleet board a
    /// grace period to show what just completed instead of the record
    /// vanishing the instant the poller marks it done (see
    /// `Session::terminal_at`). Sessions with no `terminal_at` (a terminal
    /// state reached via a direct user action —
    /// `Engine::terminate_session`/`Engine::remove_session` — rather than
    /// this automatic lifecycle path) have no grace period and are purged
    /// on sight, preserving today's immediate disappearance for those
    /// actions. Orchestrator sessions are never purged this way.
    ///
    /// Also reclaims merged-but-kept-alive workers (`merged_at` set with a
    /// still-live status — `[auto_reap]` off) once `merged_at` is older than
    /// the same window. The orchestrator owns their reap, but if it never
    /// comes — the orchestrator is gone, or the worker turned `Interrupted`
    /// at a reboot, a status this sweep otherwise never touches — this
    /// fallback stops a merged worker leaking its row/tmux/worktree forever,
    /// restoring the unconditional cleanup the pre-toggle merge path had.
    async fn sweep_retired_sessions(&self, retention: &SessionRetentionConfig) {
        let Ok(sessions) = self.engine.store.list_sessions() else { return };
        let Ok(orchestrators) = self.engine.store.list_orchestrators() else { return };
        let orch_ids: std::collections::HashSet<&str> =
            orchestrators.iter().map(|o| o.id.as_str()).collect();

        let fleet = self.engine.store.fleet_records().unwrap_or_default();

        let now = now_millis();
        let retention_ms = retention.retention_millis();

        for session in sessions {
            if orch_ids.contains(session.id.as_str()) {
                continue;
            }
            if self.engine.store.is_worker_retained(&session.id).unwrap_or(false) {
                continue;
            }
            // A restore candidate the user hasn't restored yet (manual
            // policy, or a declined prompt) keeps its row and worktree.
            // Merged ones don't need restoring, so the merged fallback
            // below still reclaims them.
            let pending_restore = fleet.get(&session.id)
                .is_some_and(|r| crate::fleet::awaits_restore(&session, r));
            if pending_restore && session.merged_at.is_none() {
                continue;
            }
            let terminal =
                matches!(session.status, SessionStatus::Done | SessionStatus::Terminated);
            // Eligible either as a terminal record (retention grace on
            // `terminal_at`) or as a merged-but-still-live worker (fallback
            // grace on `merged_at`). A live worker with no merge stamp is
            // never swept.
            if !terminal && session.merged_at.is_none() {
                continue;
            }
            let clock = if terminal { session.terminal_at } else { session.merged_at };
            let expired = match clock {
                Some(t) => now.saturating_sub(t) >= retention_ms,
                None    => true,
            };
            if !expired {
                continue;
            }
            // `Done` sessions only ever get there via `handle_merge_detection`
            // (see `poll_github`), which already sent
            // `format_worker_done_reaction` to the orchestrator before
            // transitioning the status — notifying again here would be a
            // duplicate. The same goes for a `merged_at`-stamped session
            // (merge detected with `[auto_reap]` off, worker kept alive,
            // later reaped/terminated): the orchestrator already got the
            // worker-done reaction, and this notice's "was not detected as
            // merged" wording would flatly contradict it. `Terminated`
            // sessions without that stamp (worker process died on its own
            // via `poll_pids`, or a direct `terminate_session`) have never
            // been told anything — this is their one and only chance before
            // the record disappears for good.
            if matches!(session.status, SessionStatus::Terminated) && session.merged_at.is_none() {
                self.engine.emit(Event::Notification(Notification {
                    id:         format!("retired-{}", session.id),
                    kind:       NotificationKind::WorkerRetired,
                    title:      format!("Worker retired — {}", session.name),
                    body:       "Session cleaned up without a detected PR merge".to_string(),
                    session_id: Some(session.id.clone()),
                    created_at: now,
                }));
                if let Some(orch) = session.orchestrator_id.clone() {
                    let msg = crate::lifecycle::reactions::format_worker_retired_reaction(&session);
                    if let Err(e) = self.engine.send_to_session(&orch, &msg).await {
                        tracing::warn!("send worker-retired reaction to orchestrator {orch}: {e}");
                    }
                }
            }
            // Delegate to `remove_session` rather than deleting the row
            // directly — it also kills any lingering tmux session and
            // removes the worktree/artifacts, so a session that never went
            // through `handle_merge_detection`'s cleanup still gets it here.
            if let Err(e) = self.engine.remove_session(&session.id).await {
                tracing::warn!("purge retired session {}: {e}", session.id);
                continue;
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

fn derive_gate_status(
    ci:                    &CIStatus,
    has_changes_requested: bool,
    mergeable:             Option<bool>,
    previous:              Option<&GateStatus>,
    now:                   i64,
) -> GateStatus {
    let ci_check = if ci.failing > 0 {
        GateCheck::Failing
    } else if ci.pending > 0 {
        GateCheck::Pending
    } else {
        GateCheck::Passing
    };
    let review_check = if has_changes_requested { GateCheck::Failing } else { GateCheck::Passing };
    let mergeable_check = match mergeable {
        Some(true)  => GateCheck::Passing,
        Some(false) => GateCheck::Failing,
        None        => GateCheck::Unknown,
    };

    let unchanged = previous.is_some_and(|p| {
        p.ci == ci_check && p.review == review_check && p.mergeable == mergeable_check
    });
    let since = if unchanged { previous.unwrap().since } else { now };

    GateStatus { ci: ci_check, review: review_check, mergeable: mergeable_check, since }
}

/// Wraps `derive_gate_status` with the same terminal guard
/// `derive_session_status` applies to `SessionStatus`: once a session has
/// reached a terminal status (`Done`/`Terminated`/`Interrupted`), its
/// `GateStatus` must never be recomputed — it stays exactly as last
/// observed. `poll_github` deliberately keeps polling `Terminated` and
/// `Interrupted` sessions with an unresolved PR (see the comment on its
/// status guard) purely to catch a late merge; that must not have the side
/// effect of drifting `gate_status` off live CI/review/mergeable data that
/// no longer reflects a session anyone is actively working.
///
/// `current_status` must be the status *entering* this tick — i.e.
/// `session.status` before this tick's `derive_session_status` call — not
/// the freshly derived one, so a session's very last live tick (the one
/// that flips it to terminal) still gets to compute its final gate.
fn compute_new_gate(
    current_status:        &SessionStatus,
    ci:                    &CIStatus,
    has_changes_requested: bool,
    mergeable:             Option<bool>,
    previous_gate:         Option<&GateStatus>,
    now:                   i64,
) -> Option<GateStatus> {
    if matches!(current_status, SessionStatus::Done | SessionStatus::Terminated | SessionStatus::Interrupted) {
        return previous_gate.cloned();
    }
    Some(derive_gate_status(ci, has_changes_requested, mergeable, previous_gate, now))
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
            title: "t".into(), number: 1, head_sha: String::new(), head_ref: String::new(), base_ref: String::new(),
        };
        let ci = CIStatus { pr_id: 1, total: 0, failing: 0, passing: 0, pending: 0 };
        let s  = derive_session_status(&SessionStatus::PrOpen, &pr, &ci, false);
        assert!(matches!(s, SessionStatus::Done));
    }

    #[test]
    fn derive_status_ci_failure_overrides_open() {
        let pr = crate::github::PrStatus {
            merged: false, state: "open".into(), mergeable: Some(true),
            title: "t".into(), number: 1, head_sha: String::new(), head_ref: String::new(), base_ref: String::new(),
        };
        let ci = CIStatus { pr_id: 1, total: 3, failing: 1, passing: 2, pending: 0 };
        let s  = derive_session_status(&SessionStatus::PrOpen, &pr, &ci, false);
        assert!(matches!(s, SessionStatus::CiFailed));
    }

    #[test]
    fn derive_status_all_green_becomes_mergeable() {
        let pr = crate::github::PrStatus {
            merged: false, state: "open".into(), mergeable: Some(true),
            title: "t".into(), number: 1, head_sha: String::new(), head_ref: String::new(), base_ref: String::new(),
        };
        let ci = CIStatus { pr_id: 1, total: 3, failing: 0, passing: 3, pending: 0 };
        let s  = derive_session_status(&SessionStatus::PrOpen, &pr, &ci, false);
        assert!(matches!(s, SessionStatus::Mergeable));
    }

    #[test]
    fn derive_gate_status_maps_raw_signals() {
        let ci = CIStatus { pr_id: 1, total: 3, passing: 1, failing: 1, pending: 1 };
        let gate = derive_gate_status(&ci, true, Some(false), None, 1_000);
        assert!(matches!(gate.ci, GateCheck::Failing));
        assert!(matches!(gate.review, GateCheck::Failing));
        assert!(matches!(gate.mergeable, GateCheck::Failing));
        assert_eq!(gate.since, 1_000, "first observation stamps `since` to `now`");
    }

    #[test]
    fn derive_gate_status_maps_all_passing() {
        let ci = CIStatus { pr_id: 1, total: 2, passing: 2, failing: 0, pending: 0 };
        let gate = derive_gate_status(&ci, false, Some(true), None, 1_000);
        assert!(matches!(gate.ci, GateCheck::Passing));
        assert!(matches!(gate.review, GateCheck::Passing));
        assert!(matches!(gate.mergeable, GateCheck::Passing));
    }

    #[test]
    fn derive_gate_status_maps_pending_ci_and_unknown_mergeable() {
        let ci = CIStatus { pr_id: 1, total: 2, passing: 0, failing: 0, pending: 2 };
        let gate = derive_gate_status(&ci, false, None, None, 1_000);
        assert!(matches!(gate.ci, GateCheck::Pending));
        assert!(matches!(gate.mergeable, GateCheck::Unknown));
    }

    #[test]
    fn derive_gate_status_carries_since_forward_when_unchanged() {
        let ci = CIStatus { pr_id: 1, total: 1, passing: 1, failing: 0, pending: 0 };
        let previous = GateStatus {
            ci: GateCheck::Passing, review: GateCheck::Passing,
            mergeable: GateCheck::Passing, since: 500,
        };
        let gate = derive_gate_status(&ci, false, Some(true), Some(&previous), 9_999);
        assert_eq!(gate.since, 500, "unchanged combination keeps the original `since`");
    }

    #[test]
    fn derive_gate_status_resets_since_when_combination_changes() {
        let ci = CIStatus { pr_id: 1, total: 1, passing: 0, failing: 1, pending: 0 };
        let previous = GateStatus {
            ci: GateCheck::Passing, review: GateCheck::Passing,
            mergeable: GateCheck::Passing, since: 500,
        };
        let gate = derive_gate_status(&ci, false, Some(true), Some(&previous), 9_999);
        assert!(matches!(gate.ci, GateCheck::Failing));
        assert_eq!(gate.since, 9_999, "a changed combination resets `since` to `now`");
    }

    /// The bug this guards: a `Terminated`/`Interrupted` session with an
    /// unresolved PR keeps being polled (see the comment on `poll_github`'s
    /// status guard) purely to catch a late merge. That must not have the
    /// side effect of recomputing `gate_status` from live CI/review/mergeable
    /// data — the gate must stay exactly as last observed, mirroring
    /// `derive_session_status`'s own terminal guard.
    #[test]
    fn compute_new_gate_preserves_terminal_gate_even_when_live_signals_changed() {
        let previous = GateStatus {
            ci: GateCheck::Failing, review: GateCheck::Failing,
            mergeable: GateCheck::Failing, since: 111,
        };
        // Live signals now look all-green — if the terminal guard were
        // missing, `derive_gate_status` would flip the gate to all-Passing
        // and reset `since` to `now`.
        let ci = CIStatus { pr_id: 1, total: 3, passing: 3, failing: 0, pending: 0 };

        for status in [SessionStatus::Terminated, SessionStatus::Interrupted, SessionStatus::Done] {
            let gate = compute_new_gate(&status, &ci, false, Some(true), Some(&previous), 9_999);
            assert_eq!(
                gate, Some(previous.clone()),
                "{status:?} session's gate must be carried forward unchanged, not recomputed",
            );
        }
    }

    #[test]
    fn compute_new_gate_recomputes_for_non_terminal_status() {
        let previous = GateStatus {
            ci: GateCheck::Failing, review: GateCheck::Failing,
            mergeable: GateCheck::Failing, since: 111,
        };
        let ci = CIStatus { pr_id: 1, total: 3, passing: 3, failing: 0, pending: 0 };

        let gate = compute_new_gate(&SessionStatus::PrOpen, &ci, false, Some(true), Some(&previous), 9_999);
        let gate = gate.expect("non-terminal status must produce a fresh gate");
        assert!(matches!(gate.ci, GateCheck::Passing), "must reflect the live all-green signals");
        assert_eq!(gate.since, 9_999, "changed combination resets `since` to `now`");
    }

    #[test]
    fn derive_status_preserves_done() {
        let pr = crate::github::PrStatus {
            merged: false, state: "open".into(), mergeable: Some(true),
            title: "t".into(), number: 1, head_sha: String::new(), head_ref: String::new(), base_ref: String::new(),
        };
        let ci = CIStatus { pr_id: 1, total: 0, failing: 0, passing: 0, pending: 0 };
        let s  = derive_session_status(&SessionStatus::Done, &pr, &ci, false);
        assert!(matches!(s, SessionStatus::Done));
    }

    #[test]
    fn derive_status_preserves_terminated() {
        let pr = crate::github::PrStatus {
            merged: true, state: "closed".into(), mergeable: None,   // merged=true!
            title: "t".into(), number: 1, head_sha: String::new(), head_ref: String::new(), base_ref: String::new(),
        };
        let ci = CIStatus { pr_id: 1, total: 0, failing: 0, passing: 0, pending: 0 };
        let s  = derive_session_status(&SessionStatus::Terminated, &pr, &ci, false);
        assert!(matches!(s, SessionStatus::Terminated));  // must not become Done
    }

    #[test]
    fn derive_status_changes_requested_becomes_review_pending() {
        let pr = crate::github::PrStatus {
            merged: false, state: "open".into(), mergeable: Some(true),
            title: "t".into(), number: 1, head_sha: String::new(), head_ref: String::new(), base_ref: String::new(),
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
            summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
        }
    }

    #[test]
    fn reconcile_marks_the_terminated_row_it_writes_as_a_restore_candidate() {
        use crate::store::Store;
        let store = Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap();
        // No claude_session_id: can't resume, so reconciled to Terminated.
        store.upsert_session(&test_session("dead-1", "/ws")).unwrap();
        let registry = crate::harness::HarnessRegistry::from_config(&Default::default());
        let live = reconcile_dead_session(&store, &registry, "dead-1").unwrap().unwrap();
        assert_eq!(live.status, SessionStatus::Terminated);
        let record = store.fleet_record("dead-1").unwrap().unwrap();
        assert_eq!(record.reconciled_terminal_at, live.terminal_at);
        assert!(crate::fleet::awaits_restore(&live, &record));
    }

    /// An engine starting while ptyd is mid-upgrade must not mark the
    /// sessions it can't see Interrupted; once the host answers they are
    /// judged for real.
    #[tokio::test]
    async fn unreachable_ptyd_defers_reconciliation_instead_of_marking_sessions_dead() {
        use crate::runtime::Liveness;
        use crate::store::Store;
        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        for id in ["unseen", "dead"] {
            store.upsert_session(&test_session(id, "/ws")).unwrap();
        }
        let poller = Poller::new(Engine::new(store.clone()));

        let host_down = |id: String| -> std::pin::Pin<Box<dyn std::future::Future<Output = Liveness> + Send>> {
            Box::pin(async move { if id == "unseen" { Liveness::Unknown } else { Liveness::Dead } })
        };
        poller.reconcile_sessions(vec!["unseen".into(), "dead".into()], &host_down).await;
        assert_eq!(store.get_session("unseen").unwrap().unwrap().status, SessionStatus::Working);
        assert_eq!(store.get_session("dead").unwrap().unwrap().status, SessionStatus::Terminated);
        assert!(poller.unreconciled.lock().unwrap().contains("unseen"));

        // poll_pids must not burn its resumability with a stale pid either.
        let mut s = store.get_session("unseen").unwrap().unwrap();
        s.pid = Some(999_999);
        store.upsert_session(&s).unwrap();
        poller.poll_pids().await;
        assert_eq!(store.get_session("unseen").unwrap().unwrap().status, SessionStatus::Working);

        let host_up_without_it = |_: String| -> std::pin::Pin<Box<dyn std::future::Future<Output = Liveness> + Send>> {
            Box::pin(async { Liveness::Dead })
        };
        poller.retry_unreconciled(&host_up_without_it).await;
        assert_eq!(store.get_session("unseen").unwrap().unwrap().status, SessionStatus::Terminated);
        assert!(poller.unreconciled.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn completion_delivery_attempts_at_most_one_item_per_tick() {
        use crate::{
            store::Store,
            types::{Orchestrator, WorkerCompletionIntent},
        };

        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(root.path().join("t.db")).unwrap());
        store
            .upsert_orchestrator(&Orchestrator {
                id: "orch".into(),
                name: "orch".into(),
                created_at: 1,
            })
            .unwrap();
        let completed_at = now_millis() - 1_000;
        let mut completion_ids = Vec::new();
        for (offset, id) in ["worker-1", "worker-2"].into_iter().enumerate() {
            let mut session = test_session(id, "/ws");
            session.orchestrator_id = Some("orch".into());
            session.started_at = i64::try_from(offset + 1).unwrap();
            store.upsert_session(&session).unwrap();
            let worker = store
                .prepare_worker_incarnation(
                    id,
                    Some("orch"),
                    session.started_at,
                    "/ws",
                    false,
                    3,
                )
                .unwrap();
            assert!(store
                .bind_worker_incarnation(id, &worker.incarnation_id, "/ws", "/ws", None)
                .unwrap());
            let WorkerCompletionIntent::Completed(completion) = store
                .complete_worker_incarnation(
                    id,
                    &worker.incarnation_id,
                    "orch",
                    "canonical summary",
                    completed_at + i64::try_from(offset).unwrap(),
                )
                .unwrap()
            else {
                panic!("first completion must commit");
            };
            completion_ids.push(completion.completion_id);
        }
        let poller = Poller::new(Engine::new(store.clone()));

        poller.deliver_worker_completions().await;

        let still_pending = store
            .claim_worker_completion_delivery(now_millis(), 30_000)
            .unwrap()
            .unwrap();
        assert_eq!(still_pending.completion.completion_id, completion_ids[1]);
        assert_eq!(still_pending.attempt, 1);
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
        assert!(matches!(evt, Event::SessionUpdated(s, _fields) if s.id == "s1" && s.cost_usd > 0.0));
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
            Event::SessionUpdated(s, _fields) if s.id == "s1" && s.context_used_pct == Some(42.0) && s.cost_usd == 3.5
        ));

        // A third tick with no further changes emits nothing.
        poller.poll_context_updates().await;
        assert!(drain_events(&mut rx).is_empty());
    }

    /// Same external-writer story as the statusline test above, but for the
    /// `ninox worker-status` subcommand's activity fields — the change must
    /// re-broadcast flagged ACTIVITY (and only for the touched session), or
    /// the GUI never learns a worker went idle/blocked.
    #[tokio::test]
    async fn poll_activity_updates_emits_activity_flag_only_for_changed_sessions() {
        use crate::store::Store;
        use crate::types::ActivityState;
        use crate::worker_status::{apply_activity, ActivityEvent};

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&test_session("s1", "/ws1")).unwrap();
        store.upsert_session(&test_session("s2", "/ws2")).unwrap();
        let engine = Engine::new(store.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        poller.poll_activity_updates().await;
        assert!(drain_events(&mut rx).is_empty(), "baseline seeding must not emit");

        // Simulate the external hook process writing for s1 only.
        apply_activity(&store, "s1", ActivityEvent::Explicit {
            state: ActivityState::Blocked, note: Some("stuck".into()),
        }, 1_000).unwrap();

        poller.poll_activity_updates().await;
        let events = drain_events(&mut rx);
        assert_eq!(events.len(), 1, "only the changed session should emit");
        match &events[0] {
            Event::SessionUpdated(s, fields) => {
                assert_eq!(s.id, "s1");
                assert_eq!(s.activity, ActivityState::Blocked);
                assert!(fields.contains(SessionFields::ACTIVITY), "must flag ACTIVITY");
            }
            other => panic!("expected SessionUpdated, got {other:?}"),
        }

        poller.poll_activity_updates().await;
        assert!(drain_events(&mut rx).is_empty(), "steady state must not re-emit");
    }

    /// Stacked-edge derivation is pure branch topology: B stacks on A when
    /// B's PR base branch is A's PR head branch, within the same repo.
    #[test]
    fn derive_stacked_edges_matches_base_to_head_within_a_repo() {
        let entries = vec![
            ("a".to_string(), "o/r".to_string(), "feat/a".to_string(), "main".to_string()),
            // Differently-cased slug of the same repo (user-typed vs
            // git-remote-parsed) — GitHub slugs are case-insensitive, so
            // this still edges onto a.
            ("b".to_string(), "O/R".to_string(), "feat/b".to_string(), "feat/a".to_string()),
            // A genuinely different repo — no edge.
            ("c".to_string(), "o/other".to_string(), "feat/c".to_string(), "feat/a".to_string()),
        ];
        let edges = derive_stacked_edges(&entries);
        let of = |id: &str| edges.iter().find(|(s, _)| s == id).map(|(_, t)| t.clone()).unwrap();
        assert_eq!(of("a"), Vec::<String>::new(), "base=main matches nobody");
        assert_eq!(of("b"), vec!["a".to_string()], "repo slug casing must not break the edge");
        assert_eq!(of("c"), Vec::<String>::new(), "cross-repo branch-name collision must not edge");
    }

    #[test]
    fn derive_stacked_edges_never_self_edges_on_degenerate_refs() {
        // A PR whose base equals its own head (degenerate but possible in
        // bad API data) must not produce a self-edge.
        let entries = vec![
            ("a".to_string(), "o/r".to_string(), "same".to_string(), "same".to_string()),
        ];
        let edges = derive_stacked_edges(&entries);
        assert_eq!(edges, vec![("a".to_string(), Vec::new())]);
    }

    /// An external `worker-status` write landing mid-tick must survive the
    /// GitHub pass's read→apply→write — same guarantee the statusline
    /// fields already have.
    #[tokio::test]
    async fn update_live_session_row_does_not_revert_mid_poll_activity_fields() {
        use crate::store::Store;
        use crate::types::ActivityState;
        use crate::worker_status::{apply_activity, ActivityEvent};

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let snapshot = test_session("s1", "/ws1");
        store.upsert_session(&snapshot).unwrap();
        let poller = Poller::new(Engine::new(store.clone()));

        // Tick-start snapshot taken (activity Unknown), then the external
        // hook write lands during the tick's awaits…
        apply_activity(&store, "s1", ActivityEvent::HookPrompt, 1_000).unwrap();

        // …and the GitHub pass writes its status update from the stale snapshot.
        let written = poller.update_live_session_row(&snapshot, |row| {
            row.status = SessionStatus::PrOpen;
        }).expect("row exists");

        assert_eq!(written.activity, ActivityState::Working, "mid-tick activity write must survive");
        let fresh = store.get_session("s1").unwrap().unwrap();
        assert_eq!(fresh.activity, ActivityState::Working);
        assert!(matches!(fresh.status, SessionStatus::PrOpen), "the tick's own write must still land");
    }

    /// End-to-end over the REST path: `poll_github` records each session's
    /// PR branch refs, and `reconcile_stacked_deps` turns "s2's PR is based
    /// on s1's PR branch" into a stacked edge — then drops it once the
    /// branch relationship disappears.
    #[tokio::test]
    async fn poll_github_derives_and_retires_stacked_edges_from_pr_refs() {
        use crate::store::Store;
        use crate::types::DepKind;

        let fake = std::sync::Arc::new(FakeGithub::default());
        let open_pr = |number: u64, head: &str, base: &str| crate::github::PrStatus {
            merged: false, state: "open".into(), mergeable: Some(true),
            title: "t".into(), number, head_sha: "abc".into(),
            head_ref: head.into(), base_ref: base.into(),
        };
        fake.pr_status_ok.lock().unwrap().insert(
            ("Owner".into(), "repo".into(), 1), open_pr(1, "feat/a", "main"),
        );
        fake.pr_status_ok.lock().unwrap().insert(
            ("Owner".into(), "repo".into(), 2), open_pr(2, "feat/b", "feat/a"),
        );

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        for (id, pr) in [("s1", 1u64), ("s2", 2u64)] {
            let mut s = test_session(id, &format!("/ws/{id}"));
            s.repo = "Owner/repo".into();
            s.pr_number = Some(pr);
            store.upsert_session(&s).unwrap();
        }
        let poller = Poller::new(github_engine(store.clone(), fake.clone()));

        // poll_github reconciles at the end of its own pass — no separate call.
        poller.poll_github(true).await;

        let deps = store.deps_for_session("s2").unwrap();
        assert_eq!(deps.len(), 1, "s2's base (feat/a) is s1's head — must edge");
        assert_eq!(deps[0].depends_on, "s1");
        assert_eq!(deps[0].kind, DepKind::Stacked);
        assert!(store.deps_for_session("s1").unwrap().is_empty(), "s1 stacks on nobody");

        // s2's PR is retargeted onto main — the stacked edge must retire.
        fake.pr_status_ok.lock().unwrap().insert(
            ("Owner".into(), "repo".into(), 2), open_pr(2, "feat/b", "main"),
        );
        poller.poll_github(true).await;
        assert!(store.deps_for_session("s2").unwrap().is_empty(), "retargeted PR must drop the edge");

        // Re-establish the edge, then make s2's PR lookup fail (404) — the
        // cached refs must be evicted and the edge retired, not re-derived
        // from the last-good tuple forever.
        fake.pr_status_ok.lock().unwrap().insert(
            ("Owner".into(), "repo".into(), 2), open_pr(2, "feat/b", "feat/a"),
        );
        poller.poll_github(true).await;
        assert_eq!(store.deps_for_session("s2").unwrap().len(), 1);
        fake.pr_status_ok.lock().unwrap().remove(&("Owner".into(), "repo".into(), 2));
        poller.poll_github(true).await;
        assert!(
            store.deps_for_session("s2").unwrap().is_empty(),
            "a failing PR lookup must retire the stacked edge, not freeze it",
        );
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

        // A notification alone is a one-off toast — the UI must also learn
        // about the extra PR itself so it can stay visible on the Pull
        // Requests ledger, not just flash a bell icon and vanish.
        let mut extra_pr_numbers: Vec<u64> = events.iter().filter_map(|e| match e {
            Event::ExtraPrDetected(pr) => Some(pr.number),
            _ => None,
        }).collect();
        extra_pr_numbers.sort();
        assert_eq!(
            extra_pr_numbers, vec![43, 44],
            "ExtraPrDetected must fire for every extra PR, not just the notification",
        );
        let pr_43 = events.iter().find_map(|e| match e {
            Event::ExtraPrDetected(pr) if pr.number == 43 => Some(pr),
            _ => None,
        }).expect("ExtraPrDetected for #43");
        assert_eq!(pr_43.session_id, "s1", "the ledger entry must point back at the owning session");

        // Second tick: nothing new — no duplicate notifications or re-detections.
        poller.sync_sessions_metadata(sessions_dir.path()).await;
        let events = drain_events(&mut rx);
        assert!(
            !events.iter().any(|e| matches!(e, Event::ExtraPrDetected(_))),
            "an extra PR already recorded in the store must not be re-detected on every tick",
        );
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

    #[tokio::test]
    async fn message_counts_first_sighting_is_seeded_silently() {
        use crate::store::Store;
        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.record_message_delivered("orch-1").unwrap();
        let engine = Engine::new(store.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        poller.poll_message_counts().await;

        assert!(
            !drain_events(&mut rx).iter().any(|e| matches!(e, Event::MessagesDelivered { .. })),
            "deliveries that predate the poller must not read as new on startup",
        );
    }

    #[tokio::test]
    async fn message_counts_treat_a_session_first_seen_after_startup_as_all_new() {
        use crate::store::Store;
        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let engine = Engine::new(store.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);
        poller.poll_message_counts().await;
        drain_events(&mut rx);

        store.record_message_delivered("orch-new").unwrap();
        poller.poll_message_counts().await;

        let deltas: Vec<(String, u64)> = drain_events(&mut rx).iter().filter_map(|e| match e {
            Event::MessagesDelivered { session_id, count } => Some((session_id.clone(), *count)),
            _ => None,
        }).collect();
        assert_eq!(deltas, vec![("orch-new".to_string(), 1)], "the first message to a new session is news");
    }

    #[tokio::test]
    async fn message_counts_treat_a_counter_that_restarted_as_all_new() {
        use crate::store::Store;
        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        for _ in 0..5 { store.record_message_delivered("orch-1").unwrap(); }
        let engine = Engine::new(store.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);
        poller.poll_message_counts().await;
        drain_events(&mut rx);

        // Same id removed and recreated between two ticks: the counter row
        // restarts at 1 while the cache still remembers 5.
        store.delete_session("orch-1").unwrap();
        store.record_message_delivered("orch-1").unwrap();
        store.record_message_delivered("orch-1").unwrap();
        poller.poll_message_counts().await;

        let deltas: Vec<(String, u64)> = drain_events(&mut rx).iter().filter_map(|e| match e {
            Event::MessagesDelivered { session_id, count } => Some((session_id.clone(), *count)),
            _ => None,
        }).collect();
        assert_eq!(deltas, vec![("orch-1".to_string(), 2)]);
    }

    #[tokio::test]
    async fn message_counts_forget_a_session_whose_counter_row_is_gone() {
        use crate::store::Store;
        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        for _ in 0..5 { store.record_message_delivered("orch-1").unwrap(); }
        let engine = Engine::new(store.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);
        poller.poll_message_counts().await;
        drain_events(&mut rx);

        store.delete_session("orch-1").unwrap();
        poller.poll_message_counts().await;
        assert!(drain_events(&mut rx).is_empty());

        store.record_message_delivered("orch-1").unwrap();
        poller.poll_message_counts().await;

        let deltas: Vec<(String, u64)> = drain_events(&mut rx).iter().filter_map(|e| match e {
            Event::MessagesDelivered { session_id, count } => Some((session_id.clone(), *count)),
            _ => None,
        }).collect();
        assert_eq!(deltas, vec![("orch-1".to_string(), 1)]);
    }

    #[tokio::test]
    async fn message_counts_emit_once_per_change_with_the_number_of_new_messages() {
        use crate::store::Store;
        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.record_message_delivered("orch-1").unwrap();
        let engine = Engine::new(store.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);
        poller.poll_message_counts().await;
        drain_events(&mut rx);

        store.record_message_delivered("orch-1").unwrap();
        store.record_message_delivered("orch-1").unwrap();
        poller.poll_message_counts().await;

        let events = drain_events(&mut rx);
        let deltas: Vec<(String, u64)> = events.iter().filter_map(|e| match e {
            Event::MessagesDelivered { session_id, count } => Some((session_id.clone(), *count)),
            _ => None,
        }).collect();
        assert_eq!(deltas, vec![("orch-1".to_string(), 2)]);

        poller.poll_message_counts().await;
        assert!(
            !drain_events(&mut rx).iter().any(|e| matches!(e, Event::MessagesDelivered { .. })),
            "an unchanged count must not re-emit",
        );
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

    /// A worker that creates its own branch (`git checkout -b`, recorded by
    /// the git wrapper) must have that branch recorded for restore, not the
    /// one it was spawned on.
    #[tokio::test]
    async fn metadata_sync_records_the_branch_the_agent_created() {
        use crate::store::Store;
        let sessions_dir = tempfile::tempdir().unwrap();
        std::fs::write(sessions_dir.path().join("s1.json"), r#"{"branch": "feat/parser"}"#).unwrap();
        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&test_session("s1", "/ws")).unwrap();
        store.record_spawn_facts("s1", "brief", Some("s1")).unwrap();
        let poller = Poller::new(Engine::new(store.clone()));

        poller.sync_sessions_metadata(sessions_dir.path()).await;

        let r = store.fleet_record("s1").unwrap().unwrap();
        assert_eq!(r.branch.as_deref(), Some("feat/parser"));
        assert_eq!(r.task_brief.as_deref(), Some("brief"));
    }

    /// During a restore workers resume before their orchestrator, so the
    /// nudge to it fails; the request must stay undelivered in the store so
    /// the orchestrator's recovery briefing carries it.
    #[test]
    fn work_requests_that_never_reached_the_orchestrator_stay_undelivered() {
        use crate::store::Store;
        let store = Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap();
        let mut session = test_session("w1", "/ws");
        session.orchestrator_id = Some("o1".into());
        let req = |id: &str| hooks::WorkRequest { id: id.into(), description: format!("do {id}"), requested_at: 5 };
        let sent: std::collections::HashSet<String> = ["ok".to_string()].into();

        record_work_requests(&store, &session, &[req("ok"), req("failed")], &sent, 9);

        let rows = store.open_work_requests(Some("o1")).unwrap();
        let delivered: Vec<_> = rows.iter().map(|r| (r.id.as_str(), r.delivered_at)).collect();
        assert_eq!(delivered, [("failed", None), ("ok", Some(9))]);
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
        assert!(
            !events.iter().any(|e| matches!(e, Event::ExtraPrDetected(pr) if pr.number == 9)),
            "must not emit ExtraPrDetected for a row it didn't (and mustn't) write — that \
             would let s1 clobber the ledger UI's view of another session's PR",
        );

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
        /// When set, `get_review_threads` upserts this row into the store
        /// before returning — simulating the external `ninox statusline`
        /// process landing a cost/context write mid-poll, after
        /// `poll_github` took its `list_sessions()` snapshot but before its
        /// status/gate write.
        mid_review_upsert: std::sync::Mutex<Option<(std::sync::Arc<crate::store::Store>, crate::types::Session)>>,
        /// Same as `mid_review_upsert`, but fired from `get_pr_status` —
        /// i.e. before the *self-heal* write rather than the status/gate
        /// write, which needs its own mid-poll statusline simulation.
        mid_pr_status_upsert: std::sync::Mutex<Option<(std::sync::Arc<crate::store::Store>, crate::types::Session)>>,
        /// When set, `get_review_threads` deletes this session id from the
        /// store before returning — simulating the session being removed
        /// mid-poll, after the snapshot but before the status/gate write.
        mid_review_delete: std::sync::Mutex<Option<(std::sync::Arc<crate::store::Store>, String)>>,
        /// Same statusline simulation as `mid_review_upsert`, but fired from
        /// `find_open_pr_for_branch` — the await inside
        /// `poll_pr_reconciliation`'s adoption loop.
        mid_branch_lookup_upsert: std::sync::Mutex<Option<(std::sync::Arc<crate::store::Store>, crate::types::Session)>>,
        /// Review threads (reviews + inline diff comments) `get_review_threads`
        /// returns — empty by default, same as before comment tests existed.
        review_threads: std::sync::Mutex<Vec<crate::github::ReviewThread>>,
        /// Issue-level conversation comments `get_issue_comments` returns.
        issue_comments: std::sync::Mutex<Vec<crate::types::Comment>>,
    }

    #[async_trait::async_trait]
    impl crate::github::GithubApi for FakeGithub {
        async fn get_pr_status(&self, owner: &str, repo: &str, pr_number: u64) -> anyhow::Result<crate::github::PrStatus> {
            self.calls.lock().unwrap().push((owner.to_string(), repo.to_string(), pr_number));
            if let Some((store, row)) = self.mid_pr_status_upsert.lock().unwrap().take() {
                store.upsert_session(&row).unwrap();
            }
            self.pr_status_ok.lock().unwrap()
                .get(&(owner.to_string(), repo.to_string(), pr_number))
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("404 for {owner}/{repo}#{pr_number}"))
        }
        async fn get_ci_checks(&self, _owner: &str, _repo: &str, _head_sha: &str) -> anyhow::Result<Vec<CheckRun>> {
            Ok(vec![])
        }
        async fn get_review_threads(&self, _owner: &str, _repo: &str, _pr_number: u64) -> anyhow::Result<Vec<crate::github::ReviewThread>> {
            if let Some((store, row)) = self.mid_review_upsert.lock().unwrap().take() {
                store.upsert_session(&row).unwrap();
            }
            if let Some((store, id)) = self.mid_review_delete.lock().unwrap().take() {
                store.delete_session(&id).unwrap();
            }
            Ok(self.review_threads.lock().unwrap().clone())
        }
        async fn get_issue_comments(&self, _owner: &str, _repo: &str, _pr_number: u64) -> anyhow::Result<Vec<crate::types::Comment>> {
            Ok(self.issue_comments.lock().unwrap().clone())
        }
        async fn find_open_pr_for_branch(&self, owner: &str, repo: &str, branch: &str) -> anyhow::Result<Option<crate::github::PrRef>> {
            if let Some((store, row)) = self.mid_branch_lookup_upsert.lock().unwrap().take() {
                store.upsert_session(&row).unwrap();
            }
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

    /// The same stale-upsert race `poll_github`'s writes were fixed for:
    /// reconciliation's adoption write starts from the tick-start snapshot,
    /// but `find_open_pr_for_branch` is a network await during which the
    /// statusline process can land cost/context. The adoption must go
    /// through the live row.
    #[tokio::test]
    async fn poll_pr_reconciliation_does_not_revert_mid_poll_statusline_fields() {
        use crate::store::Store;

        let repo_dir = init_git_repo("worker-branch", &[("origin", "https://github.com/Owner/repo.git")]);
        let workspace = repo_dir.path().to_string_lossy().to_string();

        let fake = std::sync::Arc::new(FakeGithub::default());
        fake.branch_matches.lock().unwrap().insert(
            ("Owner".to_string(), "repo".to_string(), "worker-branch".to_string()),
            crate::github::PrRef { number: 77, url: "https://github.com/Owner/repo/pull/77".into() },
        );

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let session = test_session("s1", &workspace);
        store.upsert_session(&session).unwrap();

        let mut mid_poll = session.clone();
        mid_poll.cost_usd = 7.25;
        mid_poll.context_used_pct = Some(55.0);
        *fake.mid_branch_lookup_upsert.lock().unwrap() = Some((store.clone(), mid_poll));

        let engine = github_engine(store.clone(), fake);
        let poller = Poller::new(engine);

        poller.poll_pr_reconciliation().await;

        let updated = store.get_session("s1").unwrap().unwrap();
        assert_eq!(updated.pr_number, Some(77), "the adoption write itself must still land");
        assert!(matches!(updated.status, SessionStatus::PrOpen));
        assert_eq!(
            updated.cost_usd, 7.25,
            "adoption must not revert cost_usd written mid-poll by the statusline process",
        );
        assert_eq!(updated.context_used_pct, Some(55.0), "mid-poll context fields must survive too");
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
                title: "t".into(), number: 99, head_sha: "abc".into(), head_ref: String::new(), base_ref: String::new(),
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

        poller.poll_github(true).await;

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
                title: "someone else's unrelated PR".into(), number: 50, head_sha: "zzz".into(), head_ref: String::new(), base_ref: String::new(),
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

        poller.poll_github(true).await;

        let updated = store.get_session("s1").unwrap().unwrap();
        assert_eq!(updated.repo, "OwnerA/repoA", "must not adopt the mirror just because it happens to have a same-numbered PR");
        assert_eq!(updated.pr_number, Some(50));

        let calls = fake.calls.lock().unwrap().clone();
        assert!(
            !calls.contains(&("OwnerB".to_string(), "repoB".to_string(), 50)),
            "must never retry the tracked number against another repo's get_pr_status: {calls:?}",
        );
    }

    /// The flicker bug: once `poll_github` discovers a PR, `session.pr_id`
    /// must be persisted to the store — not just carried on the in-memory
    /// `Event::PrOpened` the UI happens to receive. Every other poller
    /// (`poll_usage`, `poll_context_updates`, `sync_sessions_metadata`) does
    /// its own read-modify-write cycle against `list_sessions()` and
    /// re-emits a full `Event::SessionUpdated` snapshot; if `pr_id` was
    /// never written back, that snapshot carries `pr_id: None` and stomps
    /// the UI's in-memory `pr_id` back to `None` on the very next unrelated
    /// tick (e.g. a cost/token update while the agent is still generating
    /// text) — which is exactly what made the session detail view's PR card
    /// flicker between "No PR yet" and the real card.
    #[tokio::test]
    async fn poll_github_persists_pr_id_so_later_polls_dont_regress_it() {
        use crate::store::Store;

        let repo_dir = init_git_repo("worker-branch", &[("origin", "https://github.com/Owner/repo.git")]);
        let workspace = repo_dir.path().to_string_lossy().to_string();

        let fake = std::sync::Arc::new(FakeGithub::default());
        fake.pr_status_ok.lock().unwrap().insert(
            ("Owner".to_string(), "repo".to_string(), 50),
            crate::github::PrStatus {
                merged: false, state: "open".into(), mergeable: Some(true),
                title: "t".into(), number: 50, head_sha: "abc".into(), head_ref: String::new(), base_ref: String::new(),
            },
        );

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut session = test_session("s1", &workspace);
        session.repo = "Owner/repo".into();
        session.pr_number = Some(50);
        store.upsert_session(&session).unwrap();

        let engine = github_engine(store.clone(), fake);
        let poller = Poller::new(engine);

        poller.poll_github(true).await;

        let updated = store.get_session("s1").unwrap().unwrap();
        assert_eq!(
            updated.pr_id, Some(50),
            "pr_id discovered by poll_github must be persisted to the store, or a later \
             read-modify-write by any other poller will re-broadcast pr_id: None and flicker the UI",
        );
    }

    /// End-to-end regression for the `compute_new_gate` terminal guard: a
    /// `Terminated` session with an unresolved PR is still polled by
    /// `poll_github` (see the comment on its status guard, purely to catch a
    /// late merge), but its already-set `gate_status` must survive the tick
    /// unchanged even though the live CI/mergeable signals now disagree with
    /// it — `derive_session_status` already has its own terminal guard for
    /// `status`; this proves `gate_status` gets the same treatment.
    #[tokio::test]
    async fn poll_github_does_not_recompute_gate_for_terminated_session_with_unresolved_pr() {
        use crate::store::Store;

        let repo_dir = init_git_repo("worker-branch", &[("origin", "https://github.com/Owner/repo.git")]);
        let workspace = repo_dir.path().to_string_lossy().to_string();

        let fake = std::sync::Arc::new(FakeGithub::default());
        fake.pr_status_ok.lock().unwrap().insert(
            ("Owner".to_string(), "repo".to_string(), 50),
            crate::github::PrStatus {
                // Not merged — the PR is still unresolved, which is exactly
                // why `poll_github` doesn't skip a `Terminated` session. All
                // live signals (mergeable, and CI/review from FakeGithub's
                // empty checks/threads) are as green as possible, directly
                // contradicting the stale stored gate below.
                merged: false, state: "open".into(), mergeable: Some(true),
                title: "t".into(), number: 50, head_sha: "abc".into(), head_ref: String::new(), base_ref: String::new(),
            },
        );

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut session = test_session("s1", &workspace);
        session.repo = "Owner/repo".into();
        session.pr_number = Some(50);
        session.status = SessionStatus::Terminated;
        let stale_gate = GateStatus {
            ci: GateCheck::Failing, review: GateCheck::Failing,
            mergeable: GateCheck::Failing, since: 111,
        };
        session.gate_status = Some(stale_gate.clone());
        store.upsert_session(&session).unwrap();

        let engine = github_engine(store.clone(), fake);
        let poller = Poller::new(engine);

        poller.poll_github(true).await;

        let updated = store.get_session("s1").unwrap().unwrap();
        assert!(
            matches!(updated.status, SessionStatus::Terminated),
            "status must stay Terminated (derive_session_status's own terminal guard)",
        );
        assert_eq!(
            updated.gate_status, Some(stale_gate),
            "gate_status must be carried forward unchanged for a terminal session, \
             even though live CI/mergeable data disagrees with it",
        );
    }

    /// Guards against a narrower version of the same bug: if the discovered
    /// PR is *already merged* on the very tick it's found, merge detection
    /// short-circuits the rest of the loop iteration and moves the session
    /// to `Done` — a status `poll_github` never revisits (see the guard at
    /// the top of the loop). `pr_id` must therefore be persisted *before*
    /// merge detection runs, not alongside the (skipped) PR-upsert block
    /// further down, or a merged-on-discovery session is stuck with a
    /// correct `pr_number` but a permanently stale/absent `pr_id` — the
    /// exact inconsistency this function otherwise fixes, just for a
    /// session that never gets a second chance to self-correct. Also
    /// exercises this alongside the dual-remote self-heal path, so
    /// `repo`/`pr_number`/`pr_id` are all proven to land in the same write.
    #[tokio::test]
    async fn poll_github_persists_pr_id_when_the_self_healed_pr_is_already_merged() {
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
                merged: true, state: "closed".into(), mergeable: None,
                title: "t".into(), number: 99, head_sha: "abc".into(), head_ref: String::new(), base_ref: String::new(),
            },
        );
        // OwnerA/repoA#50 has no entry — get_pr_status errors, simulating a 404,
        // forcing the branch-match fallback to OwnerB/repoB#99.

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut session = test_session("s1", &workspace);
        session.repo = "OwnerA/repoA".into();
        session.pr_number = Some(50);
        store.upsert_session(&session).unwrap();

        let engine = github_engine(store.clone(), fake);
        let poller = Poller::new(engine);

        poller.poll_github(true).await;

        let updated = store.get_session("s1").unwrap().unwrap();
        assert_eq!(updated.repo, "OwnerB/repoB", "self-heal must still land");
        assert_eq!(updated.pr_number, Some(99), "self-heal must still land");
        assert_eq!(
            updated.pr_id, Some(99),
            "pr_id must be persisted even though the self-healed PR is already merged and \
             merge detection short-circuits the rest of this loop iteration",
        );
        assert!(matches!(updated.status, SessionStatus::Done), "merged PR still transitions to Done");
    }

    /// The stale-upsert race: `poll_github`'s status/gate write starts from
    /// the `list_sessions()` snapshot taken at the top of the tick, but the
    /// external `ninox statusline` process (see `poll_context_updates`)
    /// writes cost/context straight into the same row during the GitHub
    /// awaits in between — and `upsert_session` is a full-row last-writer-
    /// wins write. Persisting the snapshot would silently revert those
    /// fresher fields; the fix re-reads the live row and sets only
    /// status/gate on it. The fake's `get_review_threads` (the last await
    /// before the write) plays the statusline process here.
    #[tokio::test]
    async fn poll_github_status_write_does_not_revert_mid_poll_statusline_fields() {
        use crate::store::Store;

        let repo_dir = init_git_repo("worker-branch", &[("origin", "https://github.com/Owner/repo.git")]);
        let workspace = repo_dir.path().to_string_lossy().to_string();

        let fake = std::sync::Arc::new(FakeGithub::default());
        fake.pr_status_ok.lock().unwrap().insert(
            ("Owner".to_string(), "repo".to_string(), 50),
            crate::github::PrStatus {
                merged: false, state: "open".into(), mergeable: Some(true),
                title: "t".into(), number: 50, head_sha: "abc".into(), head_ref: String::new(), base_ref: String::new(),
            },
        );

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut session = test_session("s1", &workspace);
        session.repo = "Owner/repo".into();
        session.pr_number = Some(50);
        session.pr_id = Some(50); // fully consistent — no self-heal write this tick
        store.upsert_session(&session).unwrap();

        // The statusline write that lands mid-poll: fresher cost/context.
        let mut mid_poll = session.clone();
        mid_poll.cost_usd = 7.25;
        mid_poll.context_used_pct = Some(55.0);
        mid_poll.context_tokens = Some(123);
        *fake.mid_review_upsert.lock().unwrap() = Some((store.clone(), mid_poll));

        let engine = github_engine(store.clone(), fake);
        let poller = Poller::new(engine);

        poller.poll_github(true).await;

        let updated = store.get_session("s1").unwrap().unwrap();
        assert!(
            matches!(updated.status, SessionStatus::Mergeable),
            "the status write itself must still land (open PR, all-green signals): {:?}",
            updated.status,
        );
        assert!(updated.gate_status.is_some(), "the gate write must still land");
        assert_eq!(
            updated.cost_usd, 7.25,
            "persisting status/gate must not revert cost_usd written mid-poll by the statusline process",
        );
        assert_eq!(updated.context_used_pct, Some(55.0), "mid-poll context fields must survive too");
        assert_eq!(updated.context_tokens, Some(123));
    }

    /// Same stale-upsert race as above, one write earlier in the tick: the
    /// *self-heal* write (repo/pr_number/pr_id) also starts from the
    /// snapshot, and the statusline process can land cost/context during
    /// the `get_pr_status` awaits that precede it. The fake's
    /// `get_pr_status` plays the statusline process here.
    #[tokio::test]
    async fn poll_github_self_heal_write_does_not_revert_mid_poll_statusline_fields() {
        use crate::store::Store;

        let repo_dir = init_git_repo("worker-branch", &[("origin", "https://github.com/Owner/repo.git")]);
        let workspace = repo_dir.path().to_string_lossy().to_string();

        let fake = std::sync::Arc::new(FakeGithub::default());
        fake.pr_status_ok.lock().unwrap().insert(
            ("Owner".to_string(), "repo".to_string(), 50),
            crate::github::PrStatus {
                merged: false, state: "open".into(), mergeable: Some(true),
                title: "t".into(), number: 50, head_sha: "abc".into(), head_ref: String::new(), base_ref: String::new(),
            },
        );

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut session = test_session("s1", &workspace);
        session.repo = "Owner/repo".into();
        session.pr_number = Some(50);
        session.pr_id = None; // inconsistent — triggers the self-heal write
        store.upsert_session(&session).unwrap();

        let mut mid_poll = session.clone();
        mid_poll.cost_usd = 7.25;
        mid_poll.context_used_pct = Some(55.0);
        *fake.mid_pr_status_upsert.lock().unwrap() = Some((store.clone(), mid_poll));

        let engine = github_engine(store.clone(), fake);
        let poller = Poller::new(engine);

        poller.poll_github(true).await;

        let updated = store.get_session("s1").unwrap().unwrap();
        assert_eq!(updated.pr_id, Some(50), "the self-heal write itself must still land");
        assert_eq!(
            updated.cost_usd, 7.25,
            "the self-heal write must not revert cost_usd written mid-poll by the statusline process",
        );
        assert_eq!(updated.context_used_pct, Some(55.0), "mid-poll context fields must survive too");
    }

    /// A Resume/Re-file mid-poll (a new incarnation of the same session id,
    /// stamped with a later `started_at`) must not have this tick's
    /// findings — self-heal, merge detection — applied against it: they
    /// were fetched for the incarnation that has since been replaced. The
    /// fake's `get_pr_status` plays the Resume/Re-file here, the same await
    /// window the statusline race above uses.
    #[tokio::test]
    async fn poll_github_skips_self_heal_for_a_session_refiled_mid_poll() {
        use crate::store::Store;

        let repo_dir = init_git_repo("worker-branch", &[("origin", "https://github.com/Owner/repo.git")]);
        let workspace = repo_dir.path().to_string_lossy().to_string();

        let fake = std::sync::Arc::new(FakeGithub::default());
        fake.pr_status_ok.lock().unwrap().insert(
            ("Owner".to_string(), "repo".to_string(), 50),
            crate::github::PrStatus {
                merged: true, state: "closed".into(), mergeable: Some(true),
                title: "t".into(), number: 50, head_sha: "abc".into(),
                head_ref: String::new(), base_ref: String::new(),
            },
        );

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut session = test_session("s1", &workspace);
        session.repo = "Owner/repo".into();
        session.pr_number = Some(50);
        session.pr_id = None;
        store.upsert_session(&session).unwrap();

        // The refiled successor: same id, later started_at, no PR yet.
        let mut refiled = session.clone();
        refiled.started_at += 1;
        refiled.pr_number = None;
        refiled.pr_id = None;
        *fake.mid_pr_status_upsert.lock().unwrap() = Some((store.clone(), refiled));

        let engine = github_engine(store.clone(), fake);
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        poller.poll_github(true).await;

        let updated = store.get_session("s1").unwrap().unwrap();
        assert_eq!(updated.started_at, session.started_at + 1, "the refiled row must survive untouched");
        assert_eq!(updated.pr_id, None, "the stale tick's PR must not be adopted onto the successor");
        assert!(!matches!(updated.status, SessionStatus::Done), "the merged-PR tick must not finish the successor");
        assert!(
            drain_events(&mut rx).iter().all(|e| !matches!(
                e, Event::SessionUpdated(_, _) | Event::Notification(_),
            )),
            "a stale tick must not emit against the refiled successor",
        );
    }

    /// A session deleted between the tick-start snapshot and the
    /// status/gate write must stay deleted — falling back to the snapshot
    /// there would resurrect the row.
    #[tokio::test]
    async fn poll_github_status_write_does_not_resurrect_a_row_deleted_mid_poll() {
        use crate::store::Store;

        let repo_dir = init_git_repo("worker-branch", &[("origin", "https://github.com/Owner/repo.git")]);
        let workspace = repo_dir.path().to_string_lossy().to_string();

        let fake = std::sync::Arc::new(FakeGithub::default());
        fake.pr_status_ok.lock().unwrap().insert(
            ("Owner".to_string(), "repo".to_string(), 50),
            crate::github::PrStatus {
                merged: false, state: "open".into(), mergeable: Some(true),
                title: "t".into(), number: 50, head_sha: "abc".into(), head_ref: String::new(), base_ref: String::new(),
            },
        );

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut session = test_session("s1", &workspace);
        session.repo = "Owner/repo".into();
        session.pr_number = Some(50);
        session.pr_id = Some(50); // fully consistent — no self-heal write this tick
        store.upsert_session(&session).unwrap();

        *fake.mid_review_delete.lock().unwrap() = Some((store.clone(), "s1".to_string()));

        let engine = github_engine(store.clone(), fake);
        let poller = Poller::new(engine);

        poller.poll_github(true).await;

        assert!(
            store.get_session("s1").unwrap().is_none(),
            "a session deleted mid-poll must not be re-inserted by the status/gate write",
        );
    }

    /// A status write must never resurrect a row that reached a terminal
    /// state mid-tick. `poll_github` derives `new_status` from the tick-start
    /// snapshot, so a `Terminated` written during its GitHub awaits is
    /// invisible to that decision — and `ninox reap` runs in its own process,
    /// making it the first writer that can land one there. Writing
    /// `Mergeable` back over it would be unrecoverable: `sweep_retired_sessions`
    /// only purges `Done`/`Terminated`, and `poll_pids` needs a `pid`, which a
    /// CLI-spawned worker never has — so the card would sit on the fleet board
    /// as live forever with no session and no worktree behind it.
    #[tokio::test]
    async fn poll_github_status_write_does_not_resurrect_a_session_reaped_mid_poll() {
        use crate::store::Store;

        let repo_dir = init_git_repo("worker-branch", &[("origin", "https://github.com/Owner/repo.git")]);
        let workspace = repo_dir.path().to_string_lossy().to_string();

        let fake = std::sync::Arc::new(FakeGithub::default());
        fake.pr_status_ok.lock().unwrap().insert(
            ("Owner".to_string(), "repo".to_string(), 50),
            crate::github::PrStatus {
                merged: false, state: "open".into(), mergeable: Some(true),
                title: "t".into(), number: 50, head_sha: "abc".into(), head_ref: String::new(), base_ref: String::new(),
            },
        );

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut session = test_session("s1", &workspace);
        session.repo = "Owner/repo".into();
        session.pr_number = Some(50);
        session.pr_id = Some(50); // fully consistent — no self-heal write this tick
        session.status = SessionStatus::PrOpen;
        store.upsert_session(&session).unwrap();

        // Simulate `ninox reap s1 --force` landing mid-tick: killed, worktree
        // gone, row written Terminated with the countdown started.
        let mut reaped = session.clone();
        reaped.status = SessionStatus::Terminated;
        reaped.terminal_at = Some(1_000);
        *fake.mid_review_upsert.lock().unwrap() = Some((store.clone(), reaped));

        let engine = github_engine(store.clone(), fake);
        let poller = Poller::new(engine);

        poller.poll_github(true).await;

        let after = store.get_session("s1").unwrap().unwrap();
        assert!(
            matches!(after.status, SessionStatus::Terminated),
            "a reaped session must stay Terminated, got {:?} — anything live here is a \
             permanent ghost the sweep and poll_pids can both never clean up", after.status,
        );
        assert_eq!(after.terminal_at, Some(1_000), "the reap's countdown must survive");
    }

    /// Per the `SessionFields` contract (see its doc comment), a producer
    /// must flag exactly the fields it persisted. The status/gate write
    /// persists only `status` and `gate_status` — flagging PR_LINK too (as
    /// it once did) would make the receiving `merge_from` copy this tick's
    /// snapshot of `pr_number`/`pr_id`/`repo` over values a fresher emitter
    /// may have established since.
    #[tokio::test]
    async fn poll_github_status_gate_emit_flags_exactly_status_and_gate() {
        use crate::store::Store;

        let repo_dir = init_git_repo("worker-branch", &[("origin", "https://github.com/Owner/repo.git")]);
        let workspace = repo_dir.path().to_string_lossy().to_string();

        let fake = std::sync::Arc::new(FakeGithub::default());
        fake.pr_status_ok.lock().unwrap().insert(
            ("Owner".to_string(), "repo".to_string(), 50),
            crate::github::PrStatus {
                merged: false, state: "open".into(), mergeable: Some(true),
                title: "t".into(), number: 50, head_sha: "abc".into(), head_ref: String::new(), base_ref: String::new(),
            },
        );

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut session = test_session("s1", &workspace);
        session.repo = "Owner/repo".into();
        session.pr_number = Some(50);
        session.pr_id = Some(50); // fully consistent — nothing to self-heal
        store.upsert_session(&session).unwrap();

        let engine = github_engine(store.clone(), fake);
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        poller.poll_github(true).await;

        let events = drain_events(&mut rx);
        let fields: Vec<SessionFields> = events.iter().filter_map(|e| match e {
            Event::SessionUpdated(s, f) if s.id == "s1" => Some(*f),
            _ => None,
        }).collect();
        assert_eq!(
            fields, vec![SessionFields::STATUS | SessionFields::GATE],
            "with nothing to self-heal, the tick's only SessionUpdated is the status/gate \
             write, flagged for exactly the two fields it persisted — no PR_LINK",
        );
    }

    /// The self-heal write is the only thing that persists corrected
    /// `repo`/`pr_number`/`pr_id`, so it must announce them itself — the
    /// status/gate emit no longer over-flags PR_LINK on its behalf (the
    /// former, accidental delivery channel). The PR_LINK-only flag restricts
    /// the receiving `merge_from` to exactly the fields the self-heal wrote.
    #[tokio::test]
    async fn poll_github_self_heal_emits_pr_link_flagged_update() {
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
                title: "t".into(), number: 99, head_sha: "abc".into(), head_ref: String::new(), base_ref: String::new(),
            },
        );
        // OwnerA/repoA#50 has no entry — get_pr_status errors, simulating a
        // 404, forcing the branch-match fallback (and thus the self-heal).

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut session = test_session("s1", &workspace);
        session.repo = "OwnerA/repoA".into();
        session.pr_number = Some(50);
        store.upsert_session(&session).unwrap();

        let engine = github_engine(store.clone(), fake);
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        poller.poll_github(true).await;

        let events = drain_events(&mut rx);
        let healed = events.iter().find_map(|e| match e {
            Event::SessionUpdated(s, f) if *f == SessionFields::PR_LINK => Some(s.clone()),
            _ => None,
        }).expect(
            "self-heal must emit its own PR_LINK-flagged SessionUpdated — it's the only \
             channel by which the corrected pr fields reach the GUI",
        );
        assert_eq!(healed.repo, "OwnerB/repoB");
        assert_eq!(healed.pr_number, Some(99));
        assert_eq!(healed.pr_id, Some(99));

        for e in &events {
            if let Event::SessionUpdated(_, f) = e {
                if f.contains(SessionFields::STATUS) {
                    assert!(
                        !f.contains(SessionFields::PR_LINK),
                        "the status/gate emit must not over-flag PR_LINK on the self-heal's behalf",
                    );
                }
            }
        }
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

        poller.poll_github(true).await;
        let events = drain_events(&mut rx);
        let failures = |evs: &[Event]| evs.iter().filter(|e| matches!(
            e, Event::Notification(n) if n.kind == crate::types::NotificationKind::GithubLookupFailed
        )).count();
        assert_eq!(failures(&events), 1, "first failure must notify");

        poller.poll_github(true).await;
        let events = drain_events(&mut rx);
        assert_eq!(failures(&events), 0, "repeated failure must not re-notify");

        // Recovery.
        fake.pr_status_ok.lock().unwrap().insert(
            ("OwnerA".to_string(), "repoA".to_string(), 50),
            crate::github::PrStatus {
                merged: false, state: "open".into(), mergeable: Some(true),
                title: "t".into(), number: 50, head_sha: "abc".into(), head_ref: String::new(), base_ref: String::new(),
            },
        );
        poller.poll_github(true).await;

        // Fails again — must notify again, since recovery cleared the flag.
        fake.pr_status_ok.lock().unwrap().clear();
        poller.poll_github(true).await;
        let events = drain_events(&mut rx);
        assert_eq!(failures(&events), 1, "must notify again after recovering and failing anew");
    }

    // ── Comment capture (MLOPS-2455) ─────────────────────────────────────────
    //
    // `get_review_threads`'s only prior consumer was the CHANGES_REQUESTED
    // reaction/notification path, so a plain "Comment" review, an inline
    // diff comment, and the PR's main conversation-tab comments never
    // reached the store or the UI. These tests cover the widened capture
    // that persists/emits all three for display while leaving the
    // CHANGES_REQUESTED-only reaction gating untouched.

    fn open_pr_session(id: &str, workspace: &str, pr_number: u64) -> Session {
        let mut s = test_session(id, workspace);
        s.repo = "Owner/repo".into();
        s.pr_number = Some(pr_number);
        s
    }

    fn open_pr_fake(pr_number: u64) -> std::sync::Arc<FakeGithub> {
        let fake = std::sync::Arc::new(FakeGithub::default());
        fake.pr_status_ok.lock().unwrap().insert(
            ("Owner".to_string(), "repo".to_string(), pr_number),
            crate::github::PrStatus {
                merged: false, state: "open".into(), mergeable: Some(true),
                title: "t".into(), number: pr_number, head_sha: "abc".into(), head_ref: String::new(), base_ref: String::new(),
            },
        );
        fake
    }

    fn comment_events(evs: &[Event]) -> Vec<Comment> {
        evs.iter().filter_map(|e| match e {
            Event::ReviewComment { comment, .. } => Some(comment.clone()),
            _ => None,
        }).collect()
    }

    /// A plain "Comment" review (state COMMENTED, no diff position) is
    /// dropped entirely before this change — only CHANGES_REQUESTED reviews
    /// were converted into `Comment`s.
    #[tokio::test]
    async fn poll_github_captures_commented_review() {
        use crate::store::Store;

        let repo_dir = init_git_repo("worker-branch", &[("origin", "https://github.com/Owner/repo.git")]);
        let workspace = repo_dir.path().to_string_lossy().to_string();

        let fake = open_pr_fake(50);
        fake.review_threads.lock().unwrap().push(crate::github::ReviewThread {
            id: 501, author: "carol".into(), body: "Looks fine overall".into(),
            path: None, line: None, state: "COMMENTED".into(), created_at: 1_000,
        });

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&open_pr_session("s1", &workspace, 50)).unwrap();

        let engine = github_engine(store.clone(), fake);
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);
        poller.poll_github(true).await;

        let persisted = store.list_comments().unwrap();
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].id, 501);
        assert_eq!(persisted[0].body, "Looks fine overall");
        assert_eq!(persisted[0].created_at, 1_000, "created_at must come from the GitHub payload, not be hard-coded 0");

        let emitted = comment_events(&drain_events(&mut rx));
        assert_eq!(emitted.len(), 1, "must emit Event::ReviewComment for display even though it's not CHANGES_REQUESTED");
        assert_eq!(emitted[0].id, 501);
    }

    /// The empty-body filter added for the widened COMMENTED capture must
    /// not narrow the pre-existing CHANGES_REQUESTED path: a "Request
    /// changes" review with no top-level summary (only inline comments)
    /// still has an empty `body`, and must still be captured and still
    /// drive the reaction/notification path exactly as before this change.
    #[tokio::test]
    async fn poll_github_captures_changes_requested_review_with_empty_body() {
        use crate::store::Store;

        let repo_dir = init_git_repo("worker-branch", &[("origin", "https://github.com/Owner/repo.git")]);
        let workspace = repo_dir.path().to_string_lossy().to_string();

        let fake = open_pr_fake(50);
        fake.review_threads.lock().unwrap().push(crate::github::ReviewThread {
            id: 503, author: "frank".into(), body: String::new(),
            path: None, line: None, state: "CHANGES_REQUESTED".into(), created_at: 4_000,
        });

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&open_pr_session("s1", &workspace, 50)).unwrap();

        let engine = github_engine(store.clone(), fake);
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);
        poller.poll_github(true).await;

        let persisted = store.list_comments().unwrap();
        assert_eq!(persisted.len(), 1, "an empty-body CHANGES_REQUESTED review must still be captured");
        assert_eq!(persisted[0].id, 503);

        let events = drain_events(&mut rx);
        assert_eq!(comment_events(&events).len(), 1);
        assert!(
            events.iter().any(|e| matches!(
                e, Event::Notification(n) if n.kind == crate::types::NotificationKind::PrNeedsAttention
            )),
            "an empty-body CHANGES_REQUESTED review must still trigger the reaction/notification path",
        );
    }

    /// An inline diff comment — `get_review_threads` tags these COMMENTED
    /// too (see `github.rs`) — must be captured with its `path`/`line`.
    #[tokio::test]
    async fn poll_github_captures_inline_review_comment() {
        use crate::store::Store;

        let repo_dir = init_git_repo("worker-branch", &[("origin", "https://github.com/Owner/repo.git")]);
        let workspace = repo_dir.path().to_string_lossy().to_string();

        let fake = open_pr_fake(50);
        fake.review_threads.lock().unwrap().push(crate::github::ReviewThread {
            id: 502, author: "dave".into(), body: "nit: rename this".into(),
            path: Some("src/main.rs".into()), line: Some(42),
            state: "COMMENTED".into(), created_at: 2_000,
        });

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&open_pr_session("s1", &workspace, 50)).unwrap();

        let engine = github_engine(store.clone(), fake);
        let poller = Poller::new(engine);
        poller.poll_github(true).await;

        let persisted = store.list_comments().unwrap();
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].path.as_deref(), Some("src/main.rs"));
        assert_eq!(persisted[0].line, Some(42));
    }

    /// Issue-level conversation comments (`GET .../issues/{n}/comments`) are
    /// a separate GitHub endpoint from review threads entirely — before this
    /// change nothing ever called it.
    #[tokio::test]
    async fn poll_github_captures_issue_comment() {
        use crate::store::Store;

        let repo_dir = init_git_repo("worker-branch", &[("origin", "https://github.com/Owner/repo.git")]);
        let workspace = repo_dir.path().to_string_lossy().to_string();

        let fake = open_pr_fake(50);
        fake.issue_comments.lock().unwrap().push(Comment {
            id: 900, pr_id: 0, author: "erin".into(), body: "Great work, merging soon".into(),
            path: None, line: None, created_at: 3_000,
        });

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&open_pr_session("s1", &workspace, 50)).unwrap();

        let engine = github_engine(store.clone(), fake);
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);
        poller.poll_github(true).await;

        let persisted = store.list_comments().unwrap();
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].id, 900);
        assert_eq!(persisted[0].pr_id, 50, "pr_id must be stamped from the resolved PR, not left at the fake's placeholder value");

        let emitted = comment_events(&drain_events(&mut rx));
        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0].author, "erin");
    }

    /// Two ticks of the *same* poller (same `enrichment_cache`) must not
    /// persist or re-emit a comment already seen — `seen_comment_ids` skips
    /// it entirely on the second tick.
    #[tokio::test]
    async fn poll_github_does_not_duplicate_comments_across_repeated_polls() {
        use crate::store::Store;

        let repo_dir = init_git_repo("worker-branch", &[("origin", "https://github.com/Owner/repo.git")]);
        let workspace = repo_dir.path().to_string_lossy().to_string();

        let fake = open_pr_fake(50);
        fake.review_threads.lock().unwrap().push(crate::github::ReviewThread {
            id: 501, author: "carol".into(), body: "Looks fine overall".into(),
            path: None, line: None, state: "COMMENTED".into(), created_at: 1_000,
        });

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&open_pr_session("s1", &workspace, 50)).unwrap();

        let engine = github_engine(store.clone(), fake);
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        poller.poll_github(true).await;
        assert_eq!(comment_events(&drain_events(&mut rx)).len(), 1);

        poller.poll_github(true).await;
        assert_eq!(
            comment_events(&drain_events(&mut rx)).len(), 0,
            "the second tick's in-memory seen_comment_ids must skip an already-captured comment",
        );
        assert_eq!(store.list_comments().unwrap().len(), 1);
    }

    /// A restart resets `enrichment_cache` (and so `seen_comment_ids`) to
    /// empty — the next poll after restart will see the same comment as
    /// "new" again from GitHub's point of view. `upsert_comment`'s INSERT OR
    /// REPLACE (keyed by GitHub's comment id) must absorb that re-fetch
    /// without duplicating the row, so `App::new`'s `list_comments`
    /// hydration never shows a comment twice.
    #[tokio::test]
    async fn poll_github_restart_rehydrates_without_duplicating_comments() {
        use crate::store::Store;

        let repo_dir = init_git_repo("worker-branch", &[("origin", "https://github.com/Owner/repo.git")]);
        let workspace = repo_dir.path().to_string_lossy().to_string();

        let fake = open_pr_fake(50);
        fake.review_threads.lock().unwrap().push(crate::github::ReviewThread {
            id: 501, author: "carol".into(), body: "Looks fine overall".into(),
            path: None, line: None, state: "COMMENTED".into(), created_at: 1_000,
        });

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&open_pr_session("s1", &workspace, 50)).unwrap();

        {
            let engine = github_engine(store.clone(), fake.clone());
            let poller = Poller::new(engine);
            poller.poll_github(true).await;
        }
        assert_eq!(store.list_comments().unwrap().len(), 1);

        // Fresh `Poller`/`Engine` — a fresh `enrichment_cache`, simulating a
        // process restart against the same on-disk store.
        let engine = github_engine(store.clone(), fake);
        let poller = Poller::new(engine);
        poller.poll_github(true).await;

        let persisted = store.list_comments().unwrap();
        assert_eq!(persisted.len(), 1, "restart must not duplicate an already-persisted comment");
        assert_eq!(persisted[0].id, 501);
    }

    // ── Batched GraphQL path (`poll_github_batched`) ─────────────────────────
    //
    // The batched path collapses `poll_pr_reconciliation` + `poll_github`'s
    // per-session REST fan-out into one `fetch_batch` call. These tests drive
    // it through a `GithubBatchApi` fake, so the collection/dedup rules, the
    // branch-adoption half and the enrichment half are all exercised without
    // any network access.

    #[derive(Default)]
    struct FakeBatchApi {
        /// Handed out (by `std::mem::take`) on the *first* `fetch_batch` call;
        /// later calls see an empty `BatchResult`, which is exactly the
        /// "alias missing" shape the lookup-failure dedup test needs.
        result: std::sync::Mutex<crate::github_graphql::BatchResult>,
        /// Queued front-first, ahead of `result` above: a test that wants a
        /// specific `fetch_batch` call to fail (e.g. with a
        /// `RateLimitedError`) pushes one here. Once drained, calls fall
        /// back to the `result`/`mem::take` behavior as before — most tests
        /// never touch this and see no change.
        queued_errors: std::sync::Mutex<std::collections::VecDeque<anyhow::Error>>,
        /// Every `(pr_keys, branch_keys)` pair the poller asked for, in order.
        calls:  std::sync::Mutex<Vec<(Vec<crate::github_graphql::PrKey>, Vec<crate::github_graphql::BranchKey>)>>,
    }

    #[async_trait::async_trait]
    impl crate::github_graphql::GithubBatchApi for FakeBatchApi {
        async fn fetch_batch(
            &self,
            prs: &[crate::github_graphql::PrKey],
            branches: &[crate::github_graphql::BranchKey],
        ) -> anyhow::Result<crate::github_graphql::BatchResult> {
            self.calls.lock().unwrap().push((prs.to_vec(), branches.to_vec()));
            if let Some(err) = self.queued_errors.lock().unwrap().pop_front() {
                return Err(err);
            }
            Ok(std::mem::take(&mut *self.result.lock().unwrap()))
        }
    }

    /// An `Engine` wired to a batch fake (plus an inert REST fake, so the
    /// legacy path would 404 rather than silently satisfying an assertion the
    /// batched path is supposed to satisfy).
    fn batch_engine(
        store: std::sync::Arc<crate::store::Store>,
        batch: std::sync::Arc<FakeBatchApi>,
    ) -> std::sync::Arc<Engine> {
        Engine::new_with_github_apis(
            store,
            std::sync::Arc::new(FakeGithub::default()) as std::sync::Arc<dyn crate::github::GithubApi>,
            batch as std::sync::Arc<dyn crate::github_graphql::GithubBatchApi>,
        )
    }

    fn pr_key(repo: &str, number: u64) -> crate::github_graphql::PrKey {
        crate::github_graphql::PrKey { repo: repo.into(), number }
    }

    fn open_snapshot(number: u64) -> crate::github_graphql::PrSnapshot {
        crate::github_graphql::PrSnapshot {
            status: crate::github::PrStatus {
                merged: false, state: "open".into(), mergeable: Some(true),
                title: "t".into(), number, head_sha: "abc".into(), head_ref: String::new(), base_ref: String::new(),
            },
            closed:         false,
            checks:         vec![],
            threads:        vec![],
            issue_comments: vec![],
        }
    }

    fn watch(repo: &str, pr_number: u64) -> crate::types::PrWatch {
        crate::types::PrWatch {
            repo:              repo.into(),
            pr_number,
            pr_url:            format!("https://github.com/{repo}/pull/{pr_number}"),
            opener_session_id: None,
            created_at:        0,
        }
    }

    /// The whole point of batching: N sessions (and any registry watches)
    /// pointing at the same PR must cost exactly one alias in the query, not
    /// one per row.
    #[tokio::test]
    async fn batched_poll_dedupes_session_and_watch_targets() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        for id in ["s1", "s2"] {
            let mut s = test_session(id, "/ws");
            s.status    = SessionStatus::PrOpen;
            s.repo      = "o/r".into();
            s.pr_number = Some(7);
            store.upsert_session(&s).unwrap();
        }
        // A registry watch on the very same PR — must collapse into the same key.
        store.upsert_pr_watch(&watch("o/r", 7)).unwrap();
        // A second, distinct PR proves dedup isn't just "keep one key".
        store.upsert_pr_watch(&watch("o/r", 9)).unwrap();

        let batch = std::sync::Arc::new(FakeBatchApi::default());
        let engine = batch_engine(store.clone(), batch.clone());
        let poller = Poller::new(engine);

        poller.poll_github_batched(true).await;

        let calls = batch.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1, "one tick must issue exactly one batched fetch");
        assert_eq!(
            calls[0].0,
            vec![pr_key("o/r", 7), pr_key("o/r", 9)],
            "two sessions plus a watch on o/r#7 must contribute a single deduped key",
        );
        assert!(calls[0].1.is_empty(), "sessions that already track a PR contribute no branch keys");
    }

    /// Merge detection must work identically on the batched path: the
    /// snapshot's `merged` flag drives the same `handle_merge_detection`
    /// transition (Done + `terminal_at`) and the same single `WorkerDone`
    /// notification the legacy path produces.
    #[tokio::test]
    async fn batched_poll_marks_merged_session_done_and_notifies_orchestrator() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut s = test_session("w1", "/ws");
        s.status          = SessionStatus::Working;
        s.orchestrator_id = Some("orch1".into());
        s.repo            = "Owner/repo".into();
        s.pr_number       = Some(7);
        store.upsert_session(&s).unwrap();

        let batch = std::sync::Arc::new(FakeBatchApi::default());
        {
            let mut merged = open_snapshot(7);
            merged.status.merged = true;
            merged.status.state  = "closed".into();
            batch.result.lock().unwrap().prs.insert(pr_key("Owner/repo", 7), merged);
        }

        let engine = batch_engine(store.clone(), batch);
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        poller.poll_github_batched(true).await;

        let after = store.get_session("w1").unwrap().unwrap();
        assert!(matches!(after.status, SessionStatus::Done), "a merged PR's session must transition to Done");
        assert!(after.terminal_at.is_some(), "Done via merge detection must stamp terminal_at");
        assert_eq!(after.pr_id, Some(7), "pr_id must be persisted before merge detection ends the session");

        let events = drain_events(&mut rx);
        let merged_notifs = events.iter().filter(|e| matches!(
            e, Event::Notification(n) if n.kind == crate::types::NotificationKind::WorkerDone
        )).count();
        assert_eq!(merged_notifs, 1, "exactly one WorkerDone notification for the merge");
    }

    /// The enrichment half must reuse the exact `ingest_ci`/`scan_reviews`/
    /// `apply_status_and_gate` helpers the legacy path uses — so a snapshot
    /// carrying one failing check and one CHANGES_REQUESTED review yields the
    /// same CI row, comment row, PR row and derived session status.
    #[tokio::test]
    async fn batched_poll_ingests_ci_and_reviews_like_legacy() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&open_pr_session("s1", "/ws", 50)).unwrap();

        let batch = std::sync::Arc::new(FakeBatchApi::default());
        {
            let mut snap = open_snapshot(50);
            snap.checks = vec![
                CheckRun { name: "lint".into(), status: "completed".into(), conclusion: Some("success".into()) },
                CheckRun { name: "test".into(), status: "completed".into(), conclusion: Some("failure".into()) },
            ];
            snap.threads = vec![crate::github::ReviewThread {
                id: 601, author: "alice".into(), body: "please fix".into(),
                path: None, line: None, state: "CHANGES_REQUESTED".into(), created_at: 7_000,
            }];
            snap.issue_comments = vec![Comment {
                id: 900, pr_id: 0, author: "erin".into(), body: "ping".into(),
                path: None, line: None, created_at: 8_000,
            }];
            batch.result.lock().unwrap().prs.insert(pr_key("Owner/repo", 50), snap);
        }

        let engine = batch_engine(store.clone(), batch);
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        poller.poll_github_batched(true).await;

        let events = drain_events(&mut rx);

        let ci = events.iter().find_map(|e| match e {
            Event::CiUpdated { pr_id: 50, status } => Some(status.clone()),
            _ => None,
        }).expect("CiUpdated must be emitted for the batched snapshot's checks");
        assert_eq!((ci.total, ci.passing, ci.failing), (2, 1, 1));

        let mut persisted: Vec<i64> = store.list_comments().unwrap().iter().map(|c| c.id).collect();
        persisted.sort();
        assert_eq!(persisted, vec![601, 900], "review and issue comments must both be captured");
        assert_eq!(
            store.list_comments().unwrap().iter().find(|c| c.id == 900).unwrap().pr_id, 50,
            "pr_id must be stamped from the resolved PR",
        );

        let pr_row = store.get_pr(50).unwrap().expect("the PR row must be upserted from the snapshot");
        assert_eq!(pr_row.url, "https://github.com/Owner/repo/pull/50");
        assert_eq!(pr_row.session_id, "s1");

        let after = store.get_session("s1").unwrap().unwrap();
        assert!(
            matches!(after.status, SessionStatus::CiFailed),
            "a failing check must drive the same derived status as the legacy path, got {:?}", after.status,
        );
        let gate = after.gate_status.expect("gate must be computed on the batched path too");
        assert!(matches!(gate.ci, GateCheck::Failing));
        assert!(matches!(gate.review, GateCheck::Failing));

        assert!(
            events.iter().any(|e| matches!(
                e, Event::Notification(n) if n.kind == crate::types::NotificationKind::PrNeedsAttention
            )),
            "a CHANGES_REQUESTED review must still drive the review reaction path",
        );
    }

    /// Symmetric counterpart to `batched_poll_ingests_ci_and_reviews_like_legacy`:
    /// a session's own PR going all-passing + mergeable must emit
    /// `PrReadyToMerge` exactly once per transition, the same dedup-per-cycle
    /// shape as the newly-failing case.
    #[tokio::test]
    async fn batched_poll_notifies_ready_to_merge_for_own_pr() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&open_pr_session("s1", "/ws", 50)).unwrap();

        let batch = std::sync::Arc::new(FakeBatchApi::default());
        let engine = batch_engine(store.clone(), batch.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        let tick = |conclusion: &str, mergeable: Option<bool>| {
            let mut snap = open_snapshot(50);
            snap.status.mergeable = mergeable;
            snap.checks = vec![CheckRun {
                name: "test".into(), status: "completed".into(), conclusion: Some(conclusion.into()),
            }];
            batch.result.lock().unwrap().prs.insert(pr_key("Owner/repo", 50), snap);
        };

        tick("success", Some(true));
        poller.poll_github_batched(true).await;
        let first = notifs(&drain_events(&mut rx), NotificationKind::PrReadyToMerge);
        assert_eq!(first.len(), 1, "the first all-green + mergeable tick must notify");
        assert_eq!(first[0].session_id.as_deref(), Some("s1"));
        assert_eq!(first[0].body, "1/1 checks passing, mergeable");

        poller.poll_github_batched(true).await;
        assert!(
            notifs(&drain_events(&mut rx), NotificationKind::PrReadyToMerge).is_empty(),
            "a still-ready PR must not re-notify",
        );

        tick("success", Some(false));
        poller.poll_github_batched(true).await;
        assert!(
            notifs(&drain_events(&mut rx), NotificationKind::PrReadyToMerge).is_empty(),
            "green checks but not mergeable (conflicts) must not notify",
        );

        tick("success", Some(true));
        poller.poll_github_batched(true).await;
        assert_eq!(
            notifs(&drain_events(&mut rx), NotificationKind::PrReadyToMerge).len(), 1,
            "becoming ready again after a regression is a fresh transition and must notify",
        );
    }

    /// The batched path subsumes `poll_pr_reconciliation`: a session with no
    /// tracked PR contributes a branch key for every candidate remote, and
    /// whatever open PR comes back is adopted (number, repo, PrOpen).
    #[tokio::test]
    async fn batched_poll_adopts_branch_pr_for_prless_session() {
        use crate::store::Store;

        let repo_dir = init_git_repo("worker-branch", &[("origin", "https://github.com/Owner/repo.git")]);
        let workspace = repo_dir.path().to_string_lossy().to_string();

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&test_session("s1", &workspace)).unwrap();

        let branch_key = crate::github_graphql::BranchKey {
            repo: "Owner/repo".into(), branch: "worker-branch".into(),
        };
        let batch = std::sync::Arc::new(FakeBatchApi::default());
        batch.result.lock().unwrap().branch_prs.insert(
            branch_key.clone(),
            Some(crate::github::PrRef { number: 9, url: "https://github.com/Owner/repo/pull/9".into() }),
        );

        let engine = batch_engine(store.clone(), batch.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        poller.poll_github_batched(true).await;

        let calls = batch.calls.lock().unwrap().clone();
        // The branch-key Vec is built straight from `candidate_repos`, which
        // sorts `origin` first — and the adoption loop walks that Vec rather
        // than the result HashMap, so a multi-remote session deterministically
        // adopts from the first (origin) remote that has a PR, exactly as
        // `poll_pr_reconciliation`'s break-on-first-match loop does.
        assert_eq!(calls[0].1, vec![branch_key], "a PR-less session must contribute its branch key");

        let after = store.get_session("s1").unwrap().unwrap();
        assert_eq!(after.pr_number, Some(9), "the branch's open PR must be adopted");
        assert_eq!(after.repo, "Owner/repo", "adoption must record the repo the PR was found in");
        assert!(matches!(after.status, SessionStatus::PrOpen));

        let events = drain_events(&mut rx);
        assert!(
            events.iter().any(|e| matches!(
                e, Event::SessionUpdated(s, fields)
                    if s.id == "s1"
                    && s.pr_number == Some(9)
                    && fields.contains(SessionFields::PR_LINK)
                    && fields.contains(SessionFields::STATUS)
            )),
            "adoption must broadcast the PR link and status to the UI",
        );
    }

    /// Several sessions can share one workspace (an orchestrator and its
    /// worker on the same worktree, a re-attached session, a split follow-up).
    /// They collapse to a single branch key — one alias in the query — but the
    /// adoption must still fan back out to *every* owner, exactly as the
    /// legacy per-session reconciliation loop does. A last-writer-wins owner
    /// map would leave all but one of them permanently un-adopted.
    #[tokio::test]
    async fn batched_poll_adopts_branch_pr_for_every_session_sharing_a_workspace() {
        use crate::store::Store;

        let repo_dir = init_git_repo("worker-branch", &[("origin", "https://github.com/Owner/repo.git")]);
        let workspace = repo_dir.path().to_string_lossy().to_string();

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&test_session("s1", &workspace)).unwrap();
        store.upsert_session(&test_session("s2", &workspace)).unwrap();

        let branch_key = crate::github_graphql::BranchKey {
            repo: "Owner/repo".into(), branch: "worker-branch".into(),
        };
        let batch = std::sync::Arc::new(FakeBatchApi::default());
        batch.result.lock().unwrap().branch_prs.insert(
            branch_key.clone(),
            Some(crate::github::PrRef { number: 9, url: "https://github.com/Owner/repo/pull/9".into() }),
        );

        let engine = batch_engine(store.clone(), batch.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        poller.poll_github_batched(true).await;

        assert_eq!(
            batch.calls.lock().unwrap()[0].1, vec![branch_key],
            "two sessions on one workspace must still cost exactly one branch alias",
        );

        for id in ["s1", "s2"] {
            let after = store.get_session(id).unwrap().unwrap();
            assert_eq!(after.pr_number, Some(9), "{id} must adopt the branch's PR too");
            assert_eq!(after.repo, "Owner/repo", "{id} must record the repo the PR was found in");
            assert!(matches!(after.status, SessionStatus::PrOpen), "{id} must move to PrOpen");
        }

        let events = drain_events(&mut rx);
        let adopted: std::collections::HashSet<String> = events.iter().filter_map(|e| match e {
            Event::SessionUpdated(s, fields)
                if s.pr_number == Some(9)
                    && fields.contains(SessionFields::PR_LINK)
                    && fields.contains(SessionFields::STATUS) => Some(s.id.clone()),
            _ => None,
        }).collect();
        assert_eq!(
            adopted,
            ["s1".to_string(), "s2".to_string()].into_iter().collect::<std::collections::HashSet<_>>(),
            "both sessions' adoptions must reach the UI",
        );
    }

    /// The batched path deliberately drops the legacy cross-repo 404 fallback
    /// (see `poll_github_batched`'s doc comment): a key missing from the
    /// result map is a lookup failure, notified exactly once per run of
    /// consecutive failures — not once per tick.
    #[tokio::test]
    async fn batched_poll_missing_alias_fires_lookup_failed_once() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&open_pr_session("s1", "/ws", 50)).unwrap();

        // Empty result: the `Owner/repo#50` alias errored out server-side.
        let batch = std::sync::Arc::new(FakeBatchApi::default());
        let engine = batch_engine(store.clone(), batch.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        poller.poll_github_batched(true).await;
        poller.poll_github_batched(true).await;

        assert_eq!(batch.calls.lock().unwrap().len(), 2, "both ticks must have actually fetched");

        let failures = drain_events(&mut rx).iter().filter(|e| matches!(
            e, Event::Notification(n) if n.kind == crate::types::NotificationKind::GithubLookupFailed
        )).count();
        assert_eq!(failures, 1, "a missing alias must notify once, not once per tick");
    }

    // ── Rate-limit floor and Retry-After backoff (`note_rate_limit` /
    //    `note_batch_error` / the pause check atop `poll_github_batched`) ────

    /// A low-but-nonzero remaining budget must pause the *next* tick before
    /// it even calls `fetch_batch` — a skipped tick, not a blocked one.
    #[tokio::test]
    async fn low_rate_limit_remaining_pauses_next_tick() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&open_pr_session("s1", "/ws", 50)).unwrap();

        let batch = std::sync::Arc::new(FakeBatchApi::default());
        {
            let mut result = batch.result.lock().unwrap();
            result.prs.insert(pr_key("Owner/repo", 50), open_snapshot(50));
            result.rate_limit = crate::github_graphql::RateLimitInfo {
                cost: 1, remaining: 50, reset_at: now_millis() + 60_000,
            };
        }

        let engine = batch_engine(store.clone(), batch.clone());
        let poller = Poller::new(engine);

        poller.poll_github_batched(true).await;
        poller.poll_github_batched(true).await;

        assert_eq!(
            batch.calls.lock().unwrap().len(), 1,
            "a low remaining budget must pause the second tick before it fetches",
        );
    }

    /// A healthy remaining budget must never pause — both ticks fetch.
    #[tokio::test]
    async fn healthy_rate_limit_does_not_pause() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&open_pr_session("s1", "/ws", 50)).unwrap();

        let batch = std::sync::Arc::new(FakeBatchApi::default());
        {
            let mut result = batch.result.lock().unwrap();
            result.prs.insert(pr_key("Owner/repo", 50), open_snapshot(50));
            result.rate_limit = crate::github_graphql::RateLimitInfo {
                cost: 1, remaining: 4000, reset_at: now_millis() + 60_000,
            };
        }

        let engine = batch_engine(store.clone(), batch.clone());
        let poller = Poller::new(engine);

        poller.poll_github_batched(true).await;
        poller.poll_github_batched(true).await;

        assert_eq!(
            batch.calls.lock().unwrap().len(), 2,
            "a healthy remaining budget must never pause polling",
        );
    }

    /// A `RateLimitedError` with a `Retry-After` hint pauses for exactly
    /// that long — the next tick must be skipped, not merely retried.
    #[tokio::test]
    async fn rate_limited_error_with_retry_after_pauses() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&open_pr_session("s1", "/ws", 50)).unwrap();

        let batch = std::sync::Arc::new(FakeBatchApi::default());
        batch.queued_errors.lock().unwrap().push_back(
            anyhow::Error::new(crate::github_graphql::RateLimitedError { retry_after_secs: Some(3600) })
        );
        batch.result.lock().unwrap().prs.insert(pr_key("Owner/repo", 50), open_snapshot(50));

        let engine = batch_engine(store.clone(), batch.clone());
        let poller = Poller::new(engine);

        poller.poll_github_batched(true).await; // errors — pauses for 3600s
        poller.poll_github_batched(true).await; // must be skipped

        assert_eq!(
            batch.calls.lock().unwrap().len(), 1,
            "a Retry-After-bearing rate limit error must pause the next tick",
        );
    }

    /// Consecutive `RateLimitedError`s with no `Retry-After` hint must
    /// double the pause each time (120s, then 240s, then 480s) instead of
    /// re-pausing for a flat 120s every tick.
    #[tokio::test]
    async fn rate_limited_error_without_retry_after_backs_off_exponentially() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&open_pr_session("s1", "/ws", 50)).unwrap();

        let batch = std::sync::Arc::new(FakeBatchApi::default());
        let engine = batch_engine(store.clone(), batch.clone());
        let poller = Poller::new(engine);

        let queue_error = || {
            batch.queued_errors.lock().unwrap().push_back(
                anyhow::Error::new(crate::github_graphql::RateLimitedError { retry_after_secs: None })
            );
        };

        let before = now_millis();
        queue_error();
        poller.poll_github_batched(true).await;
        let first_pause = poller.pause_until() - before;
        assert!(
            (110_000..=130_000).contains(&first_pause),
            "the first Retry-After-less rate limit must pause ~120s, got {first_pause}ms",
        );

        // Clear the pause so the next tick actually reaches `fetch_batch`
        // instead of being skipped by the pause it just set.
        poller.set_pause_until(0);
        queue_error();
        poller.poll_github_batched(true).await;
        let second_pause = poller.pause_until() - before;
        assert!(
            second_pause >= 2 * first_pause - 10_000,
            "a second consecutive error must double the pause, got {second_pause}ms after a first of {first_pause}ms",
        );

        poller.set_pause_until(0);
        queue_error();
        poller.poll_github_batched(true).await;
        let third_pause = poller.pause_until() - before;
        assert!(
            third_pause >= 2 * second_pause - 10_000,
            "a third consecutive error must double again, got {third_pause}ms after a second of {second_pause}ms",
        );
    }

    /// A successful fetch resets the stored Retry-After-less backoff, so a
    /// fresh outage after a recovery starts doubling over from 120s again
    /// instead of continuing where a prior outage left off.
    #[tokio::test]
    async fn successful_fetch_resets_rate_limit_backoff() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&open_pr_session("s1", "/ws", 50)).unwrap();

        let batch = std::sync::Arc::new(FakeBatchApi::default());
        let engine = batch_engine(store.clone(), batch.clone());
        let poller = Poller::new(engine);

        let queue_error = || {
            batch.queued_errors.lock().unwrap().push_back(
                anyhow::Error::new(crate::github_graphql::RateLimitedError { retry_after_secs: None })
            );
        };

        // Two consecutive errors double the backoff away from the 120s floor.
        queue_error();
        poller.poll_github_batched(true).await;
        poller.set_pause_until(0);
        queue_error();
        poller.poll_github_batched(true).await;
        let doubled_pause = poller.pause_until();
        poller.set_pause_until(0);
        assert!(
            doubled_pause - now_millis() > 200_000,
            "sanity check: two consecutive errors must have doubled past 120s",
        );

        // A successful fetch in between must reset the stored backoff.
        batch.result.lock().unwrap().prs.insert(pr_key("Owner/repo", 50), open_snapshot(50));
        poller.poll_github_batched(true).await;

        // A fresh error after the success must pause ~120s again, not
        // continue doubling from where the prior outage left off.
        let before = now_millis();
        queue_error();
        poller.poll_github_batched(true).await;
        let fresh_pause = poller.pause_until() - before;
        assert!(
            (110_000..=130_000).contains(&fresh_pause),
            "a fresh error after a success must pause ~120s again, got {fresh_pause}ms",
        );
    }

    /// A pause timestamp already in the past must not skip a tick — the
    /// pause check compares against "now", not merely "is it set".
    #[tokio::test]
    async fn past_pause_does_not_skip_tick() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&open_pr_session("s1", "/ws", 50)).unwrap();

        let batch = std::sync::Arc::new(FakeBatchApi::default());
        batch.result.lock().unwrap().prs.insert(pr_key("Owner/repo", 50), open_snapshot(50));

        let engine = batch_engine(store.clone(), batch.clone());
        let poller = Poller::new(engine);
        poller.set_pause_until(now_millis() - 1_000);

        poller.poll_github_batched(true).await;

        assert_eq!(
            batch.calls.lock().unwrap().len(), 1,
            "a pause timestamp already in the past must not skip the tick",
        );
    }

    // ── Registry watch delivery (`deliver_watch_updates`) ────────────────────
    //
    // Watches are notification-only: they deliver to the *opener* session (and
    // to the UI's notification feed) but never move any session's status/gate
    // and never write the session-owned PR/CI/comment rows. Auto-close drops
    // every watch on a PR once it reaches a terminal state.

    fn watch_by(repo: &str, pr_number: u64, opener: &str) -> crate::types::PrWatch {
        crate::types::PrWatch {
            opener_session_id: Some(opener.into()),
            ..watch(repo, pr_number)
        }
    }

    fn merged_snapshot(number: u64) -> crate::github_graphql::PrSnapshot {
        let mut snap = open_snapshot(number);
        snap.status.merged = true;
        snap.status.state  = "closed".into();
        snap.closed        = true;
        snap
    }

    fn notifs(events: &[Event], kind: NotificationKind) -> Vec<Notification> {
        events.iter().filter_map(|e| match e {
            Event::Notification(n) if n.kind == kind => Some(n.clone()),
            _ => None,
        }).collect()
    }

    #[tokio::test]
    async fn watch_on_merged_pr_notifies_opener_and_auto_closes() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        // The opener exists but tracks no PR of its own — anything that
        // happens to it here could only have come from the watch path.
        store.upsert_session(&test_session("sess-a", "/ws")).unwrap();
        store.upsert_pr_watch(&watch_by("o/r", 7, "sess-a")).unwrap();

        let batch = std::sync::Arc::new(FakeBatchApi::default());
        batch.result.lock().unwrap().prs.insert(pr_key("o/r", 7), merged_snapshot(7));

        let engine = batch_engine(store.clone(), batch);
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        poller.poll_github_batched(true).await;

        let done = notifs(&drain_events(&mut rx), NotificationKind::WorkerDone);
        assert_eq!(done.len(), 1, "a merged watched PR must notify exactly once");
        assert_eq!(done[0].session_id.as_deref(), Some("sess-a"), "the notification belongs to the opener");
        assert!(done[0].title.contains("merged"), "title must say merged, got {:?}", done[0].title);
        assert!(done[0].title.contains("o/r#7"), "title must name the watched PR, got {:?}", done[0].title);
        assert_eq!(done[0].body, "https://github.com/o/r/pull/7", "body carries the watch's URL");

        assert!(
            store.list_pr_watches().unwrap().is_empty(),
            "a merged PR must auto-close its watches",
        );
        assert!(
            store.get_pr(7).unwrap().is_none(),
            "the watch path must not write session-owned PR rows",
        );
    }

    #[tokio::test]
    async fn watch_on_closed_unmerged_pr_also_auto_closes() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&test_session("sess-a", "/ws")).unwrap();
        store.upsert_pr_watch(&watch_by("o/r", 7, "sess-a")).unwrap();

        let batch = std::sync::Arc::new(FakeBatchApi::default());
        {
            let mut snap = open_snapshot(7);
            snap.closed       = true;
            snap.status.state = "closed".into(); // merged stays false
            batch.result.lock().unwrap().prs.insert(pr_key("o/r", 7), snap);
        }

        let engine = batch_engine(store.clone(), batch);
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        poller.poll_github_batched(true).await;

        let done = notifs(&drain_events(&mut rx), NotificationKind::WorkerDone);
        assert_eq!(done.len(), 1, "a closed watched PR is terminal too");
        assert!(done[0].title.contains("closed"), "title must say closed, got {:?}", done[0].title);
        assert!(!done[0].title.contains("merged"), "an unmerged close must not claim a merge");
        assert!(store.list_pr_watches().unwrap().is_empty(), "closing must auto-close the watch");
    }

    #[tokio::test]
    async fn watch_ci_failure_notifies_once_until_recovery() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&test_session("sess-a", "/ws")).unwrap();
        store.upsert_pr_watch(&watch_by("o/r", 7, "sess-a")).unwrap();

        // A CHANGES_REQUESTED review rides along on every snapshot: it must
        // notify exactly once across all four ticks (`seen_comment_ids`
        // dedup on the watch's own cache entry), and never persist a
        // comment row — those belong to the session path.
        let snapshot = |conclusion: &str| {
            let mut snap = open_snapshot(7);
            snap.checks = vec![CheckRun {
                name: "test".into(), status: "completed".into(), conclusion: Some(conclusion.into()),
            }];
            snap.threads = vec![crate::github::ReviewThread {
                id: 701, author: "alice".into(), body: "please fix".into(),
                path: Some("src/lib.rs".into()), line: Some(3),
                state: "CHANGES_REQUESTED".into(), created_at: 5_000,
            }];
            snap
        };

        let batch = std::sync::Arc::new(FakeBatchApi::default());
        let engine = batch_engine(store.clone(), batch.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        let tick = |conclusion: &str| {
            batch.result.lock().unwrap().prs.insert(pr_key("o/r", 7), snapshot(conclusion));
        };

        tick("failure");
        poller.poll_github_batched(true).await;
        let first = drain_events(&mut rx);
        let ci = notifs(&first, NotificationKind::CiFailure);
        assert_eq!(ci.len(), 1, "the first failing tick must notify the opener");
        assert_eq!(ci[0].session_id.as_deref(), Some("sess-a"));
        assert!(ci[0].title.contains("o/r#7"), "title must name the watched PR, got {:?}", ci[0].title);
        assert_eq!(ci[0].body, "1/1 checks failing");
        assert_eq!(
            notifs(&first, NotificationKind::PrNeedsAttention).len(), 1,
            "the new CHANGES_REQUESTED review must notify the opener once",
        );

        tick("failure");
        poller.poll_github_batched(true).await;
        let second = drain_events(&mut rx);
        assert!(
            notifs(&second, NotificationKind::CiFailure).is_empty(),
            "a still-failing PR must not re-notify",
        );
        assert!(
            notifs(&second, NotificationKind::PrNeedsAttention).is_empty(),
            "an already-seen review comment must not re-notify",
        );

        tick("success");
        poller.poll_github_batched(true).await;
        assert!(
            notifs(&drain_events(&mut rx), NotificationKind::CiFailure).is_empty(),
            "a green tick must not notify",
        );

        tick("failure");
        poller.poll_github_batched(true).await;
        assert_eq!(
            notifs(&drain_events(&mut rx), NotificationKind::CiFailure).len(), 1,
            "failing again after recovery is a fresh transition and must notify",
        );

        assert!(
            store.list_comments().unwrap().is_empty(),
            "the watch path must not write session-owned comment rows",
        );
        assert_eq!(
            store.list_pr_watches().unwrap().len(), 1,
            "a non-terminal PR keeps its watch registered",
        );
    }

    /// Symmetric counterpart to `watch_ci_failure_notifies_once_until_recovery`:
    /// an explicit `ninox open --pr` watch going all-passing + mergeable must
    /// notify the opener exactly once per transition.
    #[tokio::test]
    async fn watch_ready_to_merge_notifies_once_until_regression() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&test_session("sess-a", "/ws")).unwrap();
        store.upsert_pr_watch(&watch_by("o/r", 7, "sess-a")).unwrap();

        let batch = std::sync::Arc::new(FakeBatchApi::default());
        let engine = batch_engine(store.clone(), batch.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        let tick = |conclusion: &str, mergeable: Option<bool>| {
            let mut snap = open_snapshot(7);
            snap.status.mergeable = mergeable;
            snap.checks = vec![CheckRun {
                name: "test".into(), status: "completed".into(), conclusion: Some(conclusion.into()),
            }];
            batch.result.lock().unwrap().prs.insert(pr_key("o/r", 7), snap);
        };

        tick("success", Some(true));
        poller.poll_github_batched(true).await;
        let first = notifs(&drain_events(&mut rx), NotificationKind::PrReadyToMerge);
        assert_eq!(first.len(), 1, "the first ready tick must notify the opener");
        assert_eq!(first[0].session_id.as_deref(), Some("sess-a"));
        assert!(first[0].title.contains("o/r#7"), "title must name the watched PR, got {:?}", first[0].title);
        assert_eq!(first[0].body, "1/1 checks passing, mergeable");

        poller.poll_github_batched(true).await;
        assert!(
            notifs(&drain_events(&mut rx), NotificationKind::PrReadyToMerge).is_empty(),
            "a still-ready PR must not re-notify",
        );

        tick("failure", Some(true));
        poller.poll_github_batched(true).await;
        assert!(
            notifs(&drain_events(&mut rx), NotificationKind::PrReadyToMerge).is_empty(),
            "a regression back to failing must not emit a ready notification",
        );

        tick("success", Some(true));
        poller.poll_github_batched(true).await;
        assert_eq!(
            notifs(&drain_events(&mut rx), NotificationKind::PrReadyToMerge).len(), 1,
            "becoming ready again after a regression is a fresh transition and must notify",
        );
    }

    #[tokio::test]
    async fn unowned_watch_emits_events_but_no_session_delivery() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_pr_watch(&watch("o/r", 7)).unwrap(); // opener: None
        assert_eq!(store.list_pr_watches().unwrap().len(), 1);

        let batch = std::sync::Arc::new(FakeBatchApi::default());
        batch.result.lock().unwrap().prs.insert(pr_key("o/r", 7), merged_snapshot(7));

        let engine = batch_engine(store.clone(), batch);
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        poller.poll_github_batched(true).await;

        let done = notifs(&drain_events(&mut rx), NotificationKind::WorkerDone);
        assert_eq!(done.len(), 1, "an unowned watch still reaches the notification feed");
        assert_eq!(done[0].session_id, None, "an unowned watch has no session to attribute to");
        assert!(store.list_pr_watches().unwrap().is_empty(), "auto-close applies to unowned watches too");
    }

    #[tokio::test]
    async fn watch_never_mutates_any_session_status() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        // sess-a's OWN PR is Owner/repo#50 and is wide open; the PR it
        // *watches* (o/r#7) is merged. Lifecycle transitions belong to the
        // session-attached PR only, so sess-a must land where its own open PR
        // puts it (Mergeable) — never Done, never cleaned up.
        store.upsert_session(&open_pr_session("sess-a", "/ws", 50)).unwrap();
        store.upsert_pr_watch(&watch_by("o/r", 7, "sess-a")).unwrap();

        let batch = std::sync::Arc::new(FakeBatchApi::default());
        {
            let mut result = batch.result.lock().unwrap();
            result.prs.insert(pr_key("Owner/repo", 50), open_snapshot(50));
            result.prs.insert(pr_key("o/r", 7), merged_snapshot(7));
        }

        let engine = batch_engine(store.clone(), batch);
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        poller.poll_github_batched(true).await;

        let after = store.get_session("sess-a").unwrap()
            .expect("a watch reaching a terminal PR must never clean up the opener session");
        assert!(
            matches!(after.status, SessionStatus::Mergeable),
            "the opener's status must follow its OWN open PR, not the watched merge; got {:?}", after.status,
        );
        assert!(after.terminal_at.is_none(), "no watch may stamp a session terminal");

        // The watch's own terminal notification still fires, attributed to the opener.
        let done = notifs(&drain_events(&mut rx), NotificationKind::WorkerDone);
        assert_eq!(done.len(), 1, "exactly one terminal notification — the watch's, not a session merge");
        assert!(done[0].title.contains("o/r#7"), "it must be about the watched PR, got {:?}", done[0].title);
        assert!(store.list_pr_watches().unwrap().is_empty());
    }

    /// A watch whose PR key never shows up in the batch result (deleted or
    /// renamed repo, access revoked, etc.) has no terminal signal to act
    /// on — it must stay registered forever (only `ninox close --pr` may
    /// drop it) and must never emit a `Notification` event. The dedup log
    /// warning is a controller-ruled log-only path with no assertable
    /// event, so this only asserts on the registry and the event stream.
    #[tokio::test]
    async fn watch_missing_from_batch_result_stays_registered_and_silent() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&test_session("sess-a", "/ws")).unwrap();
        store.upsert_pr_watch(&watch_by("o/r", 7, "sess-a")).unwrap();

        // Batch result never contains a "o/r"#7 entry — the PR is absent.
        let batch = std::sync::Arc::new(FakeBatchApi::default());

        let engine = batch_engine(store.clone(), batch);
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        poller.poll_github_batched(true).await;
        poller.poll_github_batched(true).await;

        assert!(
            notifs(&drain_events(&mut rx), NotificationKind::WorkerDone).is_empty(),
            "a missing batch entry is not a terminal signal — no WorkerDone",
        );
        assert!(
            notifs(&drain_events(&mut rx), NotificationKind::GithubLookupFailed).is_empty(),
            "the miss is log-only — no notification event, deduped or otherwise",
        );
        assert_eq!(
            store.list_pr_watches().unwrap().len(), 1,
            "the watch must stay registered across repeated misses until `ninox close --pr`",
        );
    }

    // ── Update check ─────────────────────────────────────────────────────────

    /// Returns whatever version is currently set in `0` — swappable mid-test
    /// so "a newer version than the one we already notified about" is
    /// exercisable without a real registry.
    struct FakeUpdateSource(std::sync::Mutex<Option<semver::Version>>);

    #[async_trait::async_trait]
    impl UpdateSource for FakeUpdateSource {
        async fn latest_version(&self, _package: &str) -> anyhow::Result<Option<semver::Version>> {
            Ok(self.0.lock().unwrap().clone())
        }
    }

    fn bare_engine() -> Arc<Engine> {
        use crate::store::Store;
        let store = Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        Engine::new(store)
    }

    fn update_events(evs: &[Event]) -> usize {
        evs.iter().filter(|e| matches!(
            e, Event::Notification(n) if n.kind == crate::types::NotificationKind::UpdateAvailable
        )).count()
    }

    #[tokio::test]
    async fn poll_update_check_notifies_once_then_again_for_a_newer_version() {
        let engine = bare_engine();
        let mut rx = engine.subscribe();
        let source = Arc::new(FakeUpdateSource(std::sync::Mutex::new(Some(semver::Version::new(99, 0, 0)))));
        let poller = Poller::new(engine).with_update_source(source.clone());

        poller.poll_update_check().await;
        assert_eq!(update_events(&drain_events(&mut rx)), 1, "first sighting of a newer version must notify");

        poller.poll_update_check().await;
        assert_eq!(update_events(&drain_events(&mut rx)), 0, "same version again must not re-notify");

        *source.0.lock().unwrap() = Some(semver::Version::new(100, 0, 0));
        poller.poll_update_check().await;
        assert_eq!(update_events(&drain_events(&mut rx)), 1, "an even newer version must notify again");
    }

    #[tokio::test]
    async fn poll_update_check_no_notification_when_already_current() {
        let engine = bare_engine();
        let mut rx = engine.subscribe();
        let current: semver::Version = env!("CARGO_PKG_VERSION").parse().unwrap();
        let source = Arc::new(FakeUpdateSource(std::sync::Mutex::new(Some(current))));
        let poller = Poller::new(engine).with_update_source(source);

        poller.poll_update_check().await;
        assert_eq!(update_events(&drain_events(&mut rx)), 0);
    }

    #[tokio::test]
    async fn poll_update_check_no_notification_when_source_has_nothing() {
        let engine = bare_engine();
        let mut rx = engine.subscribe();
        let source = Arc::new(FakeUpdateSource(std::sync::Mutex::new(None)));
        let poller = Poller::new(engine).with_update_source(source);

        poller.poll_update_check().await;
        assert_eq!(update_events(&drain_events(&mut rx)), 0);
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
    /// The confirmed bug: a worker's process typically exits (`Terminated`)
    /// once its PR is merely *open*, well before that PR merges. Merge
    /// detection must still run for `Terminated` sessions, not just
    /// `Working`/`PrOpen` — otherwise the later merge becomes permanently
    /// invisible the instant the process dies.
    #[tokio::test]
    async fn merge_detection_fires_for_terminated_session() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut s = test_session("w1", "/ws");
        s.status = SessionStatus::Terminated;
        s.orchestrator_id = Some("orch1".into());
        s.pr_number = Some(42);
        store.upsert_session(&s).unwrap();

        let engine = Engine::new(store.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        let handled = poller.handle_merge_detection(&s, 42, true, true).await;
        assert!(handled, "merge detection must run for a Terminated session");

        let after = store.get_session("w1").unwrap().unwrap();
        assert!(matches!(after.status, SessionStatus::Done), "merged session transitions to Done");
        assert!(after.terminal_at.is_some(), "Done via merge detection must stamp terminal_at");

        let events = drain_events(&mut rx);
        let merged_notifs = events.iter().filter(|e| matches!(
            e, Event::Notification(n) if n.kind == crate::types::NotificationKind::WorkerDone
        )).count();
        assert_eq!(merged_notifs, 1, "exactly one WorkerDone notification for the merge");
    }

    /// The orchestrator must receive exactly one worker-done reaction per
    /// merged session — calling merge detection again for an already-`Done`
    /// session (as the next poll tick would, reading the updated status back
    /// from the store) must be a no-op, not a duplicate notification.
    #[tokio::test]
    async fn merge_detection_does_not_fire_twice_for_the_same_session() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut s = test_session("w1", "/ws");
        s.status = SessionStatus::PrOpen;
        s.orchestrator_id = Some("orch1".into());
        s.pr_number = Some(7);
        store.upsert_session(&s).unwrap();

        let engine = Engine::new(store.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        let first = poller.handle_merge_detection(&s, 7, true, true).await;
        assert!(first, "first tick handles the merge");

        // Simulate the next poll tick re-reading the (now Done) session from
        // the store before calling merge detection again.
        let updated = store.get_session("w1").unwrap().unwrap();
        let second = poller.handle_merge_detection(&updated, 7, true, true).await;
        assert!(!second, "an already-Done session must not re-fire merge detection");

        let events = drain_events(&mut rx);
        let merged_notifs = events.iter().filter(|e| matches!(
            e, Event::Notification(n) if n.kind == crate::types::NotificationKind::WorkerDone
        )).count();
        assert_eq!(merged_notifs, 1, "no duplicate WorkerDone notification across ticks");
    }

    /// A kept-alive merged worker (`merged_at` stamped) must drop out of
    /// GitHub enrichment entirely — its PR is merged, so polling it every
    /// tick until the orchestrator reaps the session is pure waste.
    #[tokio::test]
    async fn poll_github_skips_a_merge_stamped_session_entirely() {
        use crate::store::Store;

        let fake = std::sync::Arc::new(FakeGithub::default());
        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut s = test_session("w1", "/ws");
        s.status = SessionStatus::Mergeable;
        s.repo = "Owner/repo".into();
        s.pr_number = Some(7);
        s.merged_at = Some(1_000);
        store.upsert_session(&s).unwrap();

        let engine = github_engine(store.clone(), fake.clone());
        let poller = Poller::new(engine);
        poller.poll_github(false).await;

        assert!(
            fake.calls.lock().unwrap().is_empty(),
            "no GitHub request may be made for a session whose merge is already handled",
        );
    }

    /// With `[auto_reap]` off (the default), a detected merge must NOT clean
    /// the worker up: the session keeps its live status so the orchestrator
    /// can run post-merge validation in it, and the `merged_at` stamp is
    /// what records that the merge was already handled.
    #[tokio::test]
    async fn merge_detection_keeps_worker_alive_when_auto_reap_disabled() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut s = test_session("w1", "/ws");
        s.status = SessionStatus::Mergeable;
        s.orchestrator_id = Some("orch1".into());
        s.pr_number = Some(7);
        store.upsert_session(&s).unwrap();

        let engine = Engine::new(store.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        let handled = poller.handle_merge_detection(&s, 7, true, false).await;
        assert!(handled, "the merge is handled (enrichment skipped) even without cleanup");

        let after = store.get_session("w1").unwrap().unwrap();
        assert!(
            matches!(after.status, SessionStatus::Mergeable),
            "without auto_reap the session must keep its live status, got {:?}",
            after.status,
        );
        assert!(after.merged_at.is_some(), "the merge must be stamped so it's handled exactly once");
        assert!(
            after.terminal_at.is_none(),
            "no terminal_at — the session is alive, not on a retention countdown",
        );

        let events = drain_events(&mut rx);
        let merged_notifs = events.iter().filter(|e| matches!(
            e, Event::Notification(n) if n.kind == crate::types::NotificationKind::WorkerDone
        )).count();
        assert_eq!(merged_notifs, 1, "exactly one WorkerDone notification for the merge");
    }

    /// Even with `[auto_reap]` off, a worker whose process already exited
    /// (`Terminated`) before its PR merged must NOT be treated as kept-alive:
    /// there's no live agent to validate in. It gets cleaned up to `Done`
    /// and the plain done-reaction — never the "still alive" wording, and
    /// never a `merged_at` stamp that would strand a dead row.
    #[tokio::test]
    async fn merge_detection_cleans_up_a_dead_worker_even_with_auto_reap_off() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut s = test_session("w1", "/ws");
        s.status = SessionStatus::Terminated; // process exited while PR was open
        s.orchestrator_id = Some("orch1".into());
        s.pr_number = Some(7);
        store.upsert_session(&s).unwrap();

        let engine = Engine::new(store.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        assert!(poller.handle_merge_detection(&s, 7, true, false).await);

        let after = store.get_session("w1").unwrap().unwrap();
        assert!(
            matches!(after.status, SessionStatus::Done),
            "a dead worker's merge must clean it up regardless of auto_reap, got {:?}",
            after.status,
        );
        assert!(after.merged_at.is_none(), "a dead worker must not be stamped as kept-alive");
        assert!(after.terminal_at.is_some(), "cleanup must stamp terminal_at for the retention sweep");

        // The reaction must be the plain done one — asserting no session is
        // falsely advertised as alive. The kept-alive text contains "alive";
        // the plain one does not.
        let msgs = drain_events(&mut rx);
        let notif = msgs.iter().filter(|e| matches!(
            e, Event::Notification(n) if n.kind == crate::types::NotificationKind::WorkerDone
        )).count();
        assert_eq!(notif, 1, "exactly one WorkerDone notification");
    }

    /// The next tick re-reads the kept-alive session (still a live status,
    /// but `merged_at` stamped) — merge detection must keep returning `true`
    /// so enrichment stays skipped, without re-notifying.
    #[tokio::test]
    async fn merge_detection_does_not_renotify_a_kept_alive_merged_session() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut s = test_session("w1", "/ws");
        s.status = SessionStatus::Mergeable;
        s.orchestrator_id = Some("orch1".into());
        s.pr_number = Some(7);
        store.upsert_session(&s).unwrap();

        let engine = Engine::new(store.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        assert!(poller.handle_merge_detection(&s, 7, true, false).await);

        let updated = store.get_session("w1").unwrap().unwrap();
        let second = poller.handle_merge_detection(&updated, 7, true, false).await;
        assert!(second, "an already-stamped session still skips enrichment");

        let events = drain_events(&mut rx);
        let merged_notifs = events.iter().filter(|e| matches!(
            e, Event::Notification(n) if n.kind == crate::types::NotificationKind::WorkerDone
        )).count();
        assert_eq!(merged_notifs, 1, "no duplicate WorkerDone notification across ticks");
    }

    #[tokio::test]
    async fn handle_merge_detection_is_noop_when_pr_not_merged() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let s = test_session("w1", "/ws");
        store.upsert_session(&s).unwrap();
        let engine = Engine::new(store.clone());
        let poller = Poller::new(engine);

        let handled = poller.handle_merge_detection(&s, 1, false, true).await;
        assert!(!handled);
        assert!(matches!(store.get_session("w1").unwrap().unwrap().status, SessionStatus::Working));
    }

    /// A session past the retention window is purged; one still within the
    /// window survives. Time is injected via `terminal_at` (computed off the
    /// real clock, offset by the retention window) rather than sleeping, and
    /// the retention config is passed in directly rather than read from
    /// `AppConfig::load()` — mirroring `sync_sessions_metadata`'s pattern of
    /// taking its directory as a parameter so tests can control it exactly.
    #[tokio::test]
    async fn sweep_retired_sessions_purges_only_past_the_retention_window() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let retention = SessionRetentionConfig { done_retention_days: 2 };
        let now = now_millis();
        let retention_ms = retention.retention_millis();

        let mut expired = test_session("expired", "/ws");
        expired.status = SessionStatus::Done;
        expired.terminal_at = Some(now - retention_ms - 1_000);
        store.upsert_session(&expired).unwrap();

        let mut fresh = test_session("fresh", "/ws");
        fresh.status = SessionStatus::Done;
        fresh.terminal_at = Some(now - 1_000);
        store.upsert_session(&fresh).unwrap();

        let engine = Engine::new(store.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        poller.sweep_retired_sessions(&retention).await;

        assert!(store.get_session("expired").unwrap().is_none(), "past-retention session must be purged");
        assert!(store.get_session("fresh").unwrap().is_some(), "within-retention session must survive");

        let events = drain_events(&mut rx);
        assert!(events.iter().any(|e| matches!(e, Event::SessionDone(id) if id == "expired")));
    }

    /// Reconciliation stamps an un-resumable dead session `Terminated` with
    /// a `terminal_at`; under a manual restore policy it must still be
    /// there (worktree included) when the user gets round to restoring.
    #[tokio::test]
    async fn sweep_retired_sessions_keeps_unrestored_fleet_candidates() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let long_ago = now_millis() - SessionRetentionConfig::default().retention_millis() - 1_000;
        for id in ["sweep-candidate", "sweep-not-candidate"] {
            let mut s = test_session(id, "/ws");
            s.status = SessionStatus::Terminated;
            s.terminal_at = Some(long_ago);
            store.upsert_session(&s).unwrap();
            store.record_interruption(id, long_ago - 1, &SessionStatus::Working, Some("reboot")).unwrap();
        }
        store.mark_reconciled_terminal("sweep-candidate", long_ago).unwrap();

        let engine = Engine::new(store.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);
        poller.sweep_retired_sessions(&SessionRetentionConfig::default()).await;

        assert!(store.get_session("sweep-candidate").unwrap().is_some(), "awaiting restore: kept");
        assert!(store.get_session("sweep-not-candidate").unwrap().is_none());
        let events = drain_events(&mut rx);
        assert!(!events.iter().any(|e| matches!(
            e, Event::Notification(n) if n.session_id.as_deref() == Some("sweep-candidate")
        )), "no 'retired' notice for a session still awaiting restore");
    }

    /// A session terminated via a direct user action
    /// (`terminate_session`/`remove_session`) never gets `terminal_at`
    /// stamped — those must stay "immediate", so the sweep purges them on
    /// sight rather than holding them for the grace period.
    #[tokio::test]
    async fn sweep_retired_sessions_purges_sessions_with_no_terminal_at_immediately() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut s = test_session("user-killed", "/ws");
        s.status = SessionStatus::Terminated;
        s.terminal_at = None;
        store.upsert_session(&s).unwrap();

        let engine = Engine::new(store.clone());
        let poller = Poller::new(engine);

        poller.sweep_retired_sessions(&SessionRetentionConfig::default()).await;

        assert!(store.get_session("user-killed").unwrap().is_none());
    }

    /// An orchestrator's own session row must never be auto-purged by the
    /// retention sweep, no matter its status or how stale `terminal_at` is.
    #[tokio::test]
    async fn sweep_retired_sessions_never_purges_orchestrator_sessions() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_orchestrator(&crate::types::Orchestrator {
            id: "orch1".into(), name: "orch".into(), created_at: 0,
        }).unwrap();
        let mut s = test_session("orch1", "/ws");
        s.status = SessionStatus::Done;
        s.terminal_at = Some(0); // maximally stale
        store.upsert_session(&s).unwrap();

        let engine = Engine::new(store.clone());
        let poller = Poller::new(engine);

        poller.sweep_retired_sessions(&SessionRetentionConfig::default()).await;

        assert!(store.get_session("orch1").unwrap().is_some(), "orchestrator sessions are never auto-purged");
    }

    /// Active (non-terminal) sessions are left alone by the sweep regardless
    /// of `terminal_at`.
    #[tokio::test]
    async fn sweep_retired_sessions_ignores_non_terminal_sessions() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut s = test_session("still-working", "/ws");
        s.status = SessionStatus::Working;
        s.terminal_at = None;
        store.upsert_session(&s).unwrap();

        let engine = Engine::new(store.clone());
        let poller = Poller::new(engine);

        poller.sweep_retired_sessions(&SessionRetentionConfig::default()).await;

        assert!(store.get_session("still-working").unwrap().is_some());
    }

    /// A session that reaches `Terminated` without ever going through
    /// `handle_merge_detection` (e.g. `poll_pids` reaping a dead process, or
    /// a direct `terminate_session`) must still tell its orchestrator before
    /// its record disappears for good — this is the gap `sweep_retired_sessions`
    /// used to have when it purged sessions via `store.delete_session` directly.
    #[tokio::test]
    async fn sweep_retired_sessions_notifies_orchestrator_for_never_merge_detected_session() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut s = test_session("w1", "/ws");
        s.status = SessionStatus::Terminated; // e.g. poll_pids reaping a dead process
        s.orchestrator_id = Some("orch1".into());
        s.terminal_at = Some(0); // maximally stale, past any retention window
        store.upsert_session(&s).unwrap();

        let engine = Engine::new(store.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        poller.sweep_retired_sessions(&SessionRetentionConfig::default()).await;

        assert!(store.get_session("w1").unwrap().is_none(), "session must still be purged");

        let events = drain_events(&mut rx);
        let retired_notifs = events.iter().filter(|e| matches!(
            e, Event::Notification(n) if n.kind == crate::types::NotificationKind::WorkerRetired
                && n.id == "retired-w1"
        )).count();
        assert_eq!(retired_notifs, 1, "orchestrator must be told its worker was retired");
    }

    /// A session that already went through `handle_merge_detection` (and so
    /// already got `format_worker_done_reaction`) must not be notified again
    /// when the retention sweep later purges it — the merge-detection
    /// notification is the one and only notice the orchestrator needs.
    #[tokio::test]
    async fn sweep_retired_sessions_does_not_double_notify_a_merge_detected_session() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut s = test_session("w1", "/ws");
        s.orchestrator_id = Some("orch1".into());
        s.pr_number = Some(7);
        store.upsert_session(&s).unwrap();

        let engine = Engine::new(store.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        // The merge-detection happy path — this already notifies orch1 and
        // marks the session Done.
        assert!(poller.handle_merge_detection(&s, 7, true, true).await);
        drain_events(&mut rx); // discard the merge-detection's own notification/events

        // Fast-forward past the retention window and let the sweep purge it.
        let mut done = store.get_session("w1").unwrap().unwrap();
        done.terminal_at = Some(0);
        store.upsert_session(&done).unwrap();

        poller.sweep_retired_sessions(&SessionRetentionConfig::default()).await;

        assert!(store.get_session("w1").unwrap().is_none(), "session must still be purged");

        let events = drain_events(&mut rx);
        let retired_notifs = events.iter().filter(|e| matches!(
            e, Event::Notification(n) if n.kind == crate::types::NotificationKind::WorkerRetired
        )).count();
        assert_eq!(retired_notifs, 0, "must not re-notify a session already told about its merge");
    }

    // ── Dead-session reconciliation ─────────────────────────────────────────

    fn test_poller() -> (Poller, std::sync::Arc<crate::store::Store>) {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let engine = Engine::new(store.clone());
        (Poller::new(engine), store)
    }

    #[tokio::test]
    async fn reconcile_marks_dead_session_interrupted_when_resumable() {
        // Session in Working state, a claude_session_id, and no live tmux
        // session behind it (tests never create real tmux sessions on the
        // private socket, so has_session() is false).
        let (poller, store) = test_poller();
        let mut s = test_session("w1", "/ws");
        s.status = SessionStatus::Working;
        s.claude_session_id = Some("abc".into());
        s.agent_type = "claude-code".into(); // default harness: has resume args
        store.upsert_session(&s).unwrap();

        poller.reconcile_dead_sessions().await;

        let after = store.get_session("w1").unwrap().unwrap();
        assert_eq!(after.status, SessionStatus::Interrupted);
    }

    #[tokio::test]
    async fn reconcile_records_the_interruption_for_fleet_restore() {
        let (poller, store) = test_poller();
        let mut s = test_session("w3", "/ws");
        s.status = SessionStatus::PrOpen;
        s.claude_session_id = Some("abc".into());
        s.agent_type = "claude-code".into();
        store.upsert_session(&s).unwrap();

        poller.reconcile_dead_sessions().await;

        let rec = store.fleet_record("w3").unwrap().expect("interruption recorded");
        assert_eq!(rec.last_status, Some(SessionStatus::PrOpen));
        assert!(rec.interrupted_at.is_some());
        assert!(rec.interrupt_cause.is_some());
        assert!(rec.awaiting_restore());
    }

    #[tokio::test]
    async fn reconcile_skips_terminal_sessions() {
        let (poller, store) = test_poller();
        let mut s = test_session("w2", "/ws");
        s.status = SessionStatus::Done;
        store.upsert_session(&s).unwrap();

        poller.reconcile_dead_sessions().await;

        assert_eq!(store.get_session("w2").unwrap().unwrap().status, SessionStatus::Done);
    }

    #[test]
    fn session_with_id_and_resumable_harness_becomes_interrupted() {
        assert_eq!(
            reconciled_status_for_dead_session(&Some("uuid-1".into()), true),
            SessionStatus::Interrupted,
        );
    }

    #[test]
    fn legacy_session_without_id_becomes_terminated() {
        assert_eq!(
            reconciled_status_for_dead_session(&None, true),
            SessionStatus::Terminated,
        );
    }

    #[test]
    fn session_under_non_resumable_harness_becomes_terminated_even_with_an_id() {
        assert_eq!(
            reconciled_status_for_dead_session(&Some("uuid-1".into()), false),
            SessionStatus::Terminated,
        );
    }

    /// A worker kept alive past its merge (`[auto_reap]` off, `merged_at`
    /// stamped) that is later reaped/terminated must be purged WITHOUT the
    /// retired notice — its "PR was not detected as merged" wording would
    /// flatly contradict the worker-done reaction the orchestrator already
    /// received at merge-detection time.
    #[tokio::test]
    async fn sweep_retired_sessions_skips_retired_notice_for_a_merge_stamped_worker() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut s = test_session("w1", "/ws");
        s.status = SessionStatus::Terminated;
        s.orchestrator_id = Some("orch1".into());
        s.pr_number = Some(7);
        s.merged_at = Some(1_000);
        // No terminal_at — a reap is a direct action, purged on sight.
        store.upsert_session(&s).unwrap();

        let engine = Engine::new(store.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        poller.sweep_retired_sessions(&SessionRetentionConfig::default()).await;

        assert!(store.get_session("w1").unwrap().is_none(), "session must still be purged");

        let events = drain_events(&mut rx);
        let retired_notifs = events.iter().filter(|e| matches!(
            e, Event::Notification(n) if n.kind == crate::types::NotificationKind::WorkerRetired
        )).count();
        assert_eq!(
            retired_notifs, 0,
            "no retired notice for a worker whose merge was already announced",
        );
    }

    /// A merged-but-kept-alive worker (live status, `merged_at` set) whose
    /// orchestrator never reaps it must still be reclaimed once `merged_at`
    /// ages past the retention window — otherwise it leaks its row/worktree
    /// forever, losing the guaranteed cleanup the pre-toggle merge path had.
    /// One still inside the window survives.
    #[tokio::test]
    async fn sweep_reclaims_a_kept_alive_merged_worker_past_the_window() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let window = SessionRetentionConfig::default().retention_millis();
        let now = now_millis();

        // Stale: merged well past the window, still parked in a live status.
        let mut stale = test_session("stale", "/ws-stale");
        stale.status = SessionStatus::Mergeable;
        stale.merged_at = Some(now - window - 1);
        store.upsert_session(&stale).unwrap();

        // Fresh: merged just now, still within its validation window.
        let mut fresh = test_session("fresh", "/ws-fresh");
        fresh.status = SessionStatus::Mergeable;
        fresh.merged_at = Some(now);
        store.upsert_session(&fresh).unwrap();

        let engine = Engine::new(store.clone());
        let poller = Poller::new(engine);
        poller.sweep_retired_sessions(&SessionRetentionConfig::default()).await;

        assert!(
            store.get_session("stale").unwrap().is_none(),
            "a kept-alive merged worker past the window must be reclaimed",
        );
        assert!(
            store.get_session("fresh").unwrap().is_some(),
            "a kept-alive merged worker still within its window must survive",
        );
    }
}
