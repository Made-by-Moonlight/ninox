use crate::{
    config::{AppConfig, CompletionMode, MachineProfile, OrphanSweepConfig, SessionRetentionConfig},
    events::{Engine, Event},
    github::{split_repo, CheckRun},
    github_graphql::{BranchKey, PrKey},
    hooks,
    lifecycle::{
        binary_update,
        brain_harvest::{self, ClaudeHarvestRunner, HarvestRunner},
        enrichment::EnrichmentCache,
        probe::{is_pid_alive, ProcessListing},
        remote_health::{self, MachineHealthProbe, SshMachineHealthProbe, MAX_BACKOFF_SECS},
        update_check::{self, GenericPackageUpdateSource, UpdateSource},
        usage,
    },
    runtime,
    store::CursorKey,
    types::{
        CIStatus, Comment, GateCheck, GateStatus, MachineHealth, Notification, NotificationKind, PrId,
        Session, SessionFields, SessionKindData, SessionStatus, SuccessCriterion, PR,
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
pub(crate) fn now_millis() -> i64 {
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
    reconcile_dead_session_seen(store, registry, session_id, &ProcessListing::read())
}

/// [`reconcile_dead_session`] against an already-read process table, so a
/// sweep over many sessions reads it once.
fn reconcile_dead_session_seen(
    store:      &crate::store::Store,
    registry:   &crate::harness::HarnessRegistry,
    session_id: &str,
    procs:      &ProcessListing,
) -> anyhow::Result<Option<Session>> {
    let Some(mut live) = store.get_session(session_id)? else { return Ok(None) };
    if live.status.is_terminal() {
        return Ok(None);
    }
    if harness_still_running(&live, registry, procs) {
        log_held_elsewhere(&live);
        return Ok(None);
    }
    let has_resume = registry.session_can_resume(&live, &store.list_sessions()?);
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

/// Loud once per session, quiet on every later re-check.
fn log_held_elsewhere(session: &Session) {
    static WARNED: std::sync::Mutex<Option<std::collections::HashSet<String>>> = std::sync::Mutex::new(None);
    let first = WARNED.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Default::default).insert(session.id.clone());
    if first {
        tracing::error!(
            "session {} has no pane in any host this ninox knows, but its harness process is still running \
             (pid {:?}, conversation {:?}) — likely held by another ninox's ptyd host or tmux server. \
             Leaving it {:?}; it will not be resumed while that process lives.",
            session.id, session.pid, session.claude_session_id, session.status,
        );
    } else {
        tracing::debug!("session {}: harness still running outside any known host", session.id);
    }
}

/// Whether `session`'s harness is still running somewhere this ninox can't
/// see — the guard `reconcile_dead_session` applies; `ninox connect` uses it
/// to explain why it won't reconcile.
pub fn harness_running_elsewhere(session: &Session, registry: &crate::harness::HarnessRegistry) -> bool {
    harness_still_running(session, registry, &ProcessListing::read())
}

fn harness_still_running(session: &Session, registry: &crate::harness::HarnessRegistry, procs: &ProcessListing) -> bool {
    if !matches!(session.kind_data(), SessionKindData::Local) {
        return false;
    }
    let binary = registry.spec(&session.agent_type).binary.unwrap_or_else(|| session.agent_type.clone());
    procs.harness_alive(session.pid, session.claude_session_id.as_deref(), &binary)
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

/// How well a recorded PR's head branch matches the session that owns it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attribution {
    /// Head branch equals the session's branch — the only state that may reap.
    Verified,
    /// A branch isn't known on one side — merge may be recorded, never reaped.
    Unknown,
    /// Proven different branch — someone else's PR.
    Mismatch,
}

/// How long a sync `ninox user-request` answer may sit undelivered before
/// the poller nudges it into the asker's pane. `ask`/`wait` poll once a
/// second and stamp delivery themselves, so anything older than this means
/// no blocking call is left to pick it up. Arbitrary; bump if a slow
/// machine's `wait` legitimately lags more than this.
const SYNC_ANSWER_REDELIVERY_GRACE_MS: i64 = 15_000;

/// How long the retention sweep waits before retrying a row it kept because
/// its worktree couldn't be removed safely. Arbitrary; each retry runs git
/// against the worktree, so keep it well above the sweep's own tick.
const RETAINED_RETRY_MS: i64 = 10 * 60 * 1000;

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

/// Arbitrary, but longer than an in-place restart's pane-less gap (a 3s
/// kill wait plus the relaunch); how long a pid-less session's pane must
/// stay gone before a waiting queue reclaims its slot.
const DEAD_PANE_GRACE_MS: i64 = 15_000;

/// Arbitrary; how often a reconciliation retry may try to start the ptyd
/// host again.
const PREPARE_LIVENESS_EVERY_MS: i64 = 60_000;

/// Caps how long `poll_remote_sessions` waits on a single `MachineProfile`
/// before moving on — machines are polled one at a time on the same 5s tick
/// that also drives local pid liveness, so a slow-but-reachable machine must
/// not be able to stall that tick (and everything queued behind it)
/// indefinitely. Comfortably above `ensure_control_master`'s own
/// `ConnectTimeout=15` so a legitimately-slow-but-live SSH round trip isn't
/// cut off first.
const REMOTE_POLL_TIMEOUT: Duration = Duration::from_secs(20);

pub struct Poller {
    engine:           Arc<Engine>,
    fleet_restorer:   Option<FleetRestorer>,
    queue_launcher:   Option<crate::session_limits::QueueLauncher>,
    plan_advancer:    Option<crate::ghost_plan::PlanAdvancer>,
    /// When `free_slots_held_by_dead_panes` first saw each session's pane
    /// gone; it acts only on a sighting that has lasted.
    dead_pane_seen:   Arc<std::sync::Mutex<HashMap<String, i64>>>,
    /// Sessions startup reconciliation couldn't judge because the ptyd host
    /// didn't answer. Retried on the pid tick; `poll_pids` leaves them alone
    /// meanwhile, since a pid check would burn their resumability.
    unreconciled:     Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// Deferred sessions whose harness was last seen running outside any
    /// known host. One "gone" sighting isn't enough for these: an in-place
    /// restart by the ninox that owns them has a gap between kill and
    /// resume.
    held_elsewhere:   Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
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
    /// Sessions whose already-stamped `merged_at` was re-checked against the
    /// recorded PR's head branch this daemon run, so `repair_misattributed_merges`
    /// costs one lookup per kept-alive merged session rather than one per tick.
    attribution_checked: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// Sessions already warned about a PR head-branch mismatch (warn once).
    attribution_warned:  Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
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
    /// Where to look up the latest published ninox version. `None` (the
    /// default) builds a [`GenericPackageUpdateSource`] for the configured
    /// channel each tick; injectable so tests never hit the network or
    /// shell out — see `poll_update_check`.
    update_source:    Option<Arc<dyn UpdateSource>>,
    /// The last update-check failure message that was logged, so a failure
    /// that repeats every tick (an expired SSO session, say) warns once
    /// per distinct message instead of every 6h.
    last_update_error: Arc<std::sync::Mutex<Option<String>>>,
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
    /// Multiplier on `[github] poll_interval_secs` while the shared token's
    /// budget is draining (1.0 = healthy); set by `note_rate_limit`.
    github_interval_factor: Arc<std::sync::Mutex<f64>>,
    budget_level: Arc<std::sync::Mutex<crate::github_budget::BudgetLevel>>,
    /// Port of a daemon that owns GitHub polling when this poller is the
    /// GUI's in-process one; see `github_deferred_to_daemon`.
    external_daemon_port: Option<u16>,
    /// (unix millis of the last port probe, whether a daemon answered).
    daemon_probe: Arc<std::sync::Mutex<(i64, bool)>>,
    /// `[pr_events]` thresholds, refreshed from config at each GitHub tick
    /// so the sweep functions never read config themselves.
    pr_events_config: Arc<std::sync::Mutex<crate::config::PrEventsConfig>>,
    /// Effective `[worker_completion]` mode, refreshed with `pr_events_config`.
    completion_mode: Arc<std::sync::Mutex<CompletionMode>>,
    /// Last `check_message_stalls` run; it needs minute granularity, not
    /// the 5s liveness tick it rides on.
    last_stall_check: Arc<std::sync::Mutex<i64>>,
    /// Set while a retry pass runs on its own task.
    retry_in_flight: Arc<std::sync::atomic::AtomicBool>,
    /// Last held-message counts emitted, so `Event::HeldMessages` fires on
    /// change only.
    last_held: Arc<std::sync::Mutex<Option<HeldSnapshot>>>,
    /// Attempts to (re)establish a `MachineProfile`'s SSH control
    /// connection — real production code uses `SshMachineHealthProbe`;
    /// tests inject a fake so they never shell out to a real `ssh`.
    health_probe: Arc<dyn MachineHealthProbe>,
    /// Per-`MachineProfile::id` reconnect bookkeeping for
    /// `poll_remote_machine_health` — doubling backoff (mirrors
    /// `rate_limit_backoff_secs`) plus a notify-once dedup flag (mirrors
    /// `enrichment_cache`'s `github_lookup_failed_notified`), just keyed per
    /// machine instead of per rate-limit-window / per-session.
    remote_machine_health: Arc<std::sync::Mutex<HashMap<String, MachineHealthState>>>,
    disk_janitor: Arc<crate::workspace::DiskJanitor>,
}

/// See `Poller::remote_machine_health`.
#[derive(Debug, Clone, Copy, Default)]
struct MachineHealthState {
    backoff_secs: u64,
    next_attempt_at: i64,
    unreachable_notified: bool,
}

impl Poller {
    pub fn new(engine: Arc<Engine>) -> Self {
        Self::new_with_harvest_runner(engine, Arc::new(ClaudeHarvestRunner))
    }

    pub fn new_with_harvest_runner(engine: Arc<Engine>, harvest_runner: Arc<dyn HarvestRunner>) -> Self {
        Self {
            engine,
            fleet_restorer:   None,
            queue_launcher:   None,
            plan_advancer:    None,
            dead_pane_seen:   Arc::new(std::sync::Mutex::new(HashMap::new())),
            unreconciled:     Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            held_elsewhere:   Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            last_liveness_prepare: Arc::new(std::sync::Mutex::new(0)),
            enrichment_cache: Arc::new(std::sync::Mutex::new(HashMap::new())),
            context_cache:    Arc::new(std::sync::Mutex::new(HashMap::new())),
            activity_cache:   Arc::new(std::sync::Mutex::new(HashMap::new())),
            pr_refs_cache:    Arc::new(std::sync::Mutex::new(HashMap::new())),
            attribution_checked: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            attribution_warned:  Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            message_count_cache: Arc::new(std::sync::Mutex::new(None)),
            harvest_runner,
            vault_locks:      Arc::new(std::sync::Mutex::new(HashMap::new())),
            update_source:    None,
            last_update_error: Arc::new(std::sync::Mutex::new(None)),
            last_notified_update: Arc::new(std::sync::Mutex::new(None)),
            rate_limit_pause_until: Arc::new(std::sync::Mutex::new(0)),
            rate_limit_backoff_secs: Arc::new(std::sync::Mutex::new(0)),
            github_interval_factor: Arc::new(std::sync::Mutex::new(1.0)),
            budget_level: Arc::new(std::sync::Mutex::new(crate::github_budget::BudgetLevel::Normal)),
            external_daemon_port: None,
            daemon_probe: Arc::new(std::sync::Mutex::new((0, false))),
            pr_events_config: Arc::new(std::sync::Mutex::new(Default::default())),
            completion_mode: Arc::new(std::sync::Mutex::new(CompletionMode::default())),
            last_stall_check: Arc::new(std::sync::Mutex::new(0)),
            retry_in_flight: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_held: Arc::new(std::sync::Mutex::new(None)),
            health_probe: Arc::new(SshMachineHealthProbe),
            remote_machine_health: Arc::new(std::sync::Mutex::new(HashMap::new())),
            disk_janitor: Arc::new(crate::workspace::DiskJanitor::default()),
        }
    }

    /// Overrides the free-space probe `poll_workspace_disk` uses.
    pub fn with_disk_probe(mut self, probe: crate::workspace::DiskProbe) -> Self {
        self.disk_janitor = Arc::new(crate::workspace::DiskJanitor::with_probe(probe));
        self
    }

    /// Overrides the remote-machine health probe — the dependency-injection
    /// seam `poll_remote_machine_health`'s tests use instead of a real `ssh`.
    pub fn with_machine_health_probe(mut self, probe: Arc<dyn MachineHealthProbe>) -> Self {
        self.health_probe = probe;
        self
    }

    /// Overrides the update-check source — the dependency-injection seam
    /// `poll_update_check`'s tests use instead of the real registry.
    pub fn with_update_source(mut self, source: Arc<dyn UpdateSource>) -> Self {
        self.update_source = Some(source);
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

    /// Test seam: read a machine's current health-probe backoff state, so
    /// `poll_remote_machine_health`'s doubling and dedup bookkeeping is
    /// assertable directly.
    #[cfg(test)]
    fn machine_health_state(&self, id: &str) -> Option<MachineHealthState> {
        self.remote_machine_health.lock().unwrap_or_else(|e| e.into_inner()).get(id).copied()
    }

    /// Test seam: force a machine's `next_attempt_at` into the past, so
    /// repeated backoff doubling is exercisable across several
    /// `poll_remote_machine_health` calls without waiting on a real clock.
    #[cfg(test)]
    fn force_machine_probe_due(&self, id: &str) {
        let mut health = self.remote_machine_health.lock().unwrap_or_else(|e| e.into_inner());
        health.entry(id.to_string()).or_default().next_attempt_at = 0;
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

    /// Inject what starts queued sessions (the app's spawn code). Without
    /// one, this poller leaves the queue alone.
    pub fn with_queue_launcher(mut self, launcher: crate::session_limits::QueueLauncher) -> Self {
        self.queue_launcher = Some(launcher);
        self
    }

    /// Inject what spawns accepted ghost-plan nodes whose parent and
    /// dependencies are ready (the app's spawn code). Without one, accepted
    /// nodes that had to wait stay waiting until the next `ninox plan accept`.
    /// Make this poller leave GitHub polling to a daemon serving `port`
    /// whenever one answers. Used by the GUI's in-process poller so two
    /// pollers never spend the same token; the GUI re-reads the store
    /// instead. Resumes polling on its own if the daemon goes away.
    pub fn with_external_github_daemon(mut self, port: u16) -> Self {
        self.external_daemon_port = Some(port);
        self
    }

    /// True while the daemon named by `with_external_github_daemon` is
    /// reachable. Probed at most every 15s.
    async fn github_deferred_to_daemon(&self) -> bool {
        const PROBE_EVERY_MS: i64 = 15_000;
        let Some(port) = self.external_daemon_port else { return false };
        {
            let probe = self.daemon_probe.lock().unwrap_or_else(|e| e.into_inner());
            if now_millis() - probe.0 < PROBE_EVERY_MS {
                return probe.1;
            }
        }
        let up = crate::daemon::port_in_use(port).await;
        *self.daemon_probe.lock().unwrap_or_else(|e| e.into_inner()) = (now_millis(), up);
        up
    }

    pub fn with_plan_advancer(mut self, advancer: crate::ghost_plan::PlanAdvancer) -> Self {
        self.plan_advancer = Some(advancer);
        self
    }

    async fn advance_ghost_plans(&self) {
        if let Some(advancer) = &self.plan_advancer {
            advancer(now_millis()).await;
        }
    }

    async fn drain_session_queue(&self) {
        let Some(launcher) = &self.queue_launcher else { return };
        self.free_slots_held_by_dead_panes().await;
        crate::session_limits::drain(&self.engine.store, launcher, now_millis()).await;
    }

    /// `poll_pids` can only see a session die through its `pid`, which a
    /// CLI-spawned worker never records, so a crashed one stays `Working`
    /// and would hold a slot forever. Only while this machine is full with
    /// sessions waiting: reconcile its pid-less sessions whose pane is gone,
    /// the same way startup reconciliation does.
    async fn free_slots_held_by_dead_panes(&self) {
        use crate::store::QueueState;
        let store = &self.engine.store;
        let Ok(queue) = store.list_queue() else { return };
        if !queue.iter().any(|e| e.state == QueueState::Queued && e.location.is_none()) {
            return;
        }
        let config = AppConfig::load_or_default();
        let full = config.limits.limit_for(None)
            .is_some_and(|l| store.running_session_count(None).unwrap_or(0) >= l as usize);
        if !full || runtime::configured_backend() == runtime::Backend::Acp {
            return;
        }
        let launching: std::collections::HashSet<&str> = queue.iter()
            .filter(|e| e.state == QueueState::Launching)
            .map(|e| e.id.as_str())
            .collect();
        let unreconciled = self.unreconciled.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let Ok(sessions) = store.list_sessions() else { return };
        let registry = config.registry();
        let now = now_millis();
        let mut still_dead = HashMap::new();
        let seen = self.dead_pane_seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        for s in sessions {
            let candidate = !s.status.is_terminal()
                && s.machine_id.is_none()
                && s.pid.is_none()
                && !crate::worker_type::is_prompt_agent_session(&s)
                && !launching.contains(s.id.as_str())
                && !unreconciled.contains(&s.id);
            if !candidate || crate::runtime::SessionBackend::knows(self.engine.acp.as_ref(), &s.id).await {
                continue;
            }
            if runtime::liveness(&s.id).await != runtime::Liveness::Dead {
                continue;
            }
            // An in-place restart has no pane for a few seconds, and a ptyd
            // host can miss one answer: only a sighting that has lasted
            // counts.
            let first_seen = *seen.get(&s.id).unwrap_or(&now);
            if now - first_seen < DEAD_PANE_GRACE_MS {
                still_dead.insert(s.id.clone(), first_seen);
                continue;
            }
            match reconcile_dead_session(store, &registry, &s.id) {
                Ok(Some(live)) => {
                    tracing::info!("session queue: {} lost its pane; freeing its slot ({:?})", live.id, live.status);
                    self.engine.emit(Event::SessionUpdated(live, SessionFields::STATUS));
                }
                Ok(None) => {}
                Err(e) => tracing::warn!("session queue: reconcile {}: {e}", s.id),
            }
        }
        *self.dead_pane_seen.lock().unwrap_or_else(|e| e.into_inner()) = still_dead;
    }

    async fn apply_restore_policy(&self) {
        let policy = AppConfig::load_or_default().fleet.restore_policy;
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
        let registry = AppConfig::load_or_default().registry();
        let mut deferred = std::collections::HashSet::new();
        let mut host_unanswered = 0usize;
        let mut reconciled = false;
        let procs = std::sync::OnceLock::new();
        for id in ids {
            match check(id.clone()).await {
                Liveness::Live => {
                    self.held_elsewhere.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
                }
                Liveness::Unknown => {
                    host_unanswered += 1;
                    deferred.insert(id);
                }
                Liveness::Dead => {
                    let procs = procs.get_or_init(ProcessListing::read);
                    let held_now = self.engine.store.get_session(&id).ok().flatten()
                        .filter(|s| !s.status.is_terminal() && harness_still_running(s, &registry, procs));
                    let mut held = self.held_elsewhere.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(s) = held_now {
                        log_held_elsewhere(&s);
                        held.insert(id.clone());
                        deferred.insert(id);
                        continue;
                    }
                    // Seen running last time, gone now: wait one more tick
                    // before calling it gone.
                    if held.remove(&id) {
                        deferred.insert(id);
                        continue;
                    }
                    drop(held);
                    match reconcile_dead_session_seen(&self.engine.store, &registry, &id, procs) {
                        Ok(Some(live)) => {
                            reconciled = true;
                            self.engine.emit(Event::SessionUpdated(live, SessionFields::STATUS));
                        }
                        // Its harness appeared between the two looks.
                        Ok(None) => {
                            if self.engine.store.get_session(&id).ok().flatten().is_some_and(|s| !s.status.is_terminal()) {
                                self.held_elsewhere.lock().unwrap_or_else(|e| e.into_inner()).insert(id.clone());
                                deferred.insert(id);
                            }
                        }
                        Err(e) => tracing::warn!("reconcile {id}: {e}"),
                    }
                }
            }
        }
        if host_unanswered > 0 {
            tracing::warn!("ptyd host not answering: {host_unanswered} session(s) left unreconciled, retrying");
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
        // Ticks every 5s only to check whether a GitHub sweep is due; the
        // cadence itself is `[github] poll_interval_secs` stretched by the
        // budget factor, so a changed setting applies without a restart.
        let mut github_interval = tokio::time::interval(Duration::from_secs(5));
        let mut next_github_ms = 0i64;
        // Tick cadence is just how often we check whether any machine is
        // *due* for a probe — the actual pacing of reconnect attempts comes
        // from each profile's own backoff (`next_attempt_at`), not this
        // interval.
        let mut remote_health_interval = tokio::time::interval(Duration::from_secs(5));
        // Releases only happen on merge-to-main, so this doesn't need to be
        // frequent — 6h keeps the once-per-startup check (interval's first
        // tick fires immediately) without hammering the registry or
        // shelling out to `aws codeartifact get-authorization-token` often.
        let mut update_interval = tokio::time::interval(Duration::from_secs(6 * 60 * 60));
        let mut harness_auth_interval = tokio::time::interval(Duration::from_secs(60));
        let mut workspace_disk_interval = tokio::time::interval(Duration::from_secs(30));
        // `tokio::time::interval`'s period can't be changed once created, so
        // (like every other interval here) only the *behavior* at each tick
        // is reloaded fresh from `AppConfig::load()` — a changed
        // `interval_hours` takes effect on the next restart. `.max(1)` guards
        // a misconfigured `0` from ticking in a hot loop.
        let mut orphan_sweep_interval = tokio::time::interval(Duration::from_secs(
            AppConfig::load_or_default().orphan_sweep.interval_hours.max(1) * 60 * 60,
        ));
        // Prevent a missed tick from causing back-to-back polls.
        github_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        usage_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        update_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        remote_health_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        orphan_sweep_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        workspace_disk_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = token.cancelled()      => break,
                _ = pid_interval.tick()    => {
                    self.retry_unreconciled_live().await;
                    self.poll_pids().await;
                    self.poll_remote_sessions().await;
                    self.poll_context_updates().await;
                    self.poll_activity_updates().await;
                    self.poll_message_counts().await;
                    let retention = AppConfig::load()
                        .unwrap_or_default()
                        .session_retention;
                    self.sweep_retired_sessions(&retention).await;
                    self.reap_self_completed_sessions().await;
                    self.drain_session_queue().await;
                    self.retry_unreachable_messages().await;
                    self.emit_held_messages_if_changed();
                    self.check_message_stalls(&AppConfig::load_or_default().message_stalls).await;
                    self.advance_ghost_plans().await;
                }
                _ = usage_interval.tick()  => self.poll_usage().await,
                _ = orphan_sweep_interval.tick() => {
                    let config = AppConfig::load_or_default();
                    self.run_orphan_sweep(&config.orphan_sweep, &config.session_retention).await;
                }
                _ = workspace_disk_interval.tick() => self.poll_workspace_disk(&AppConfig::load_or_default()).await,
                _ = update_interval.tick() => self.poll_update_check().await,
                _ = harness_auth_interval.tick() => self.poll_harness_auth().await,
                _ = remote_health_interval.tick() => self.poll_remote_machine_health().await,
                _ = github_interval.tick() => {
                    if now_millis() < next_github_ms {
                        continue;
                    }
                    if self.github_deferred_to_daemon().await {
                        next_github_ms = now_millis() + 15_000;
                        continue;
                    }
                    let config = AppConfig::load_or_default();
                    if let Some(batch) = &self.engine.github_batch {
                        batch.set_detail_refresh_secs(config.github.effective_detail_refresh_secs());
                    }
                    self.drop_ended_watches();
                    *self.pr_events_config.lock().unwrap_or_else(|e| e.into_inner()) = config.pr_events.clone();
                    let mode = config.completion_mode();
                    *self.completion_mode.lock().unwrap_or_else(|e| e.into_inner()) = mode;
                    self.repair_misattributed_merges().await;
                    if config.pr_watch.enabled {
                        self.poll_github_batched(mode == CompletionMode::ReapOnMerge).await;
                    } else {
                        // Reconciliation first: a session whose PR the poller
                        // hasn't adopted yet has no `pr_number` for `poll_github`
                        // to enrich, so it must run before (not instead of) it.
                        self.poll_pr_reconciliation().await;
                        self.poll_github(mode == CompletionMode::ReapOnMerge).await;
                    }
                    self.check_pr_hangs(&config.pr_events).await;
                    self.check_finish_after_merge(config.worker_completion.idle_after_merge_nudge_mins).await;
                    let factor = *self.github_interval_factor.lock().unwrap_or_else(|e| e.into_inner());
                    let secs = config.github.effective_poll_interval_secs() as f64 * factor;
                    next_github_ms = now_millis() + (secs * 1000.0) as i64;
                }
            }
        }
    }

    // ── Message delivery upkeep ─────────────────────────────────────────────

    /// Retries messages held because their target was unreachable, on its
    /// own task: a delivery can wait up to 30s for a Tier-3 target to go
    /// idle, which must not stall the 5s liveness tick. A message given up
    /// on is surfaced as a notification for its sender.
    async fn retry_unreachable_messages(&self) {
        use std::sync::atomic::Ordering;
        if self.retry_in_flight.swap(true, Ordering::AcqRel) {
            return;
        }
        let engine = self.engine.clone();
        let in_flight = self.retry_in_flight.clone();
        tokio::spawn(async move {
            for outcome in engine.retry_unreachable_messages(now_millis()).await {
                match outcome {
                    crate::messaging::RetryOutcome::Delivered { to, from } => {
                        tracing::info!("held message from {from:?} delivered to {to} on retry");
                    }
                    crate::messaging::RetryOutcome::GaveUp { to, from, error } => {
                        tracing::warn!("held message from {from:?} to {to} dropped: {error}");
                        engine.emit(Event::Notification(Notification {
                            id:         format!("undeliverable-{to}-{}", now_millis()),
                            kind:       NotificationKind::AgentStuck,
                            title:      format!("Message to {to} could not be delivered"),
                            body:       error,
                            session_id: from,
                            created_at: now_millis(),
                        }));
                    }
                }
            }
            in_flight.store(false, Ordering::Release);
        });
    }

    /// Emits `Event::HeldMessages` when the held-message picture changed.
    fn emit_held_messages_if_changed(&self) {
        let (Ok(held), Ok(by_sender)) = (
            self.engine.store.held_message_counts(),
            self.engine.store.unreachable_held_by_sender(),
        ) else {
            return;
        };
        let snapshot = (held, by_sender);
        let mut last = self.last_held.lock().unwrap_or_else(|e| e.into_inner());
        if last.as_ref() == Some(&snapshot) {
            return;
        }
        *last = Some(snapshot.clone());
        self.engine.emit(Event::HeldMessages { held: snapshot.0, by_sender: snapshot.1 });
    }

    /// Nudges a worker that has gone idle owing a message: it has received
    /// something (or just its brief) since it last sent anything, and it has
    /// sat idle for `nudge_after_mins`. Text written only in its own pane
    /// reaches nobody, so this is the usual "read-back that was never sent".
    /// Its orchestrator is told too. Once per idle episode. Workers with an
    /// open PR are left to the PR hang check.
    async fn check_message_stalls(&self, cfg: &crate::config::MessageStallConfig) {
        if cfg.nudge_after_mins == 0 {
            return;
        }
        let now = now_millis();
        {
            let mut last = self.last_stall_check.lock().unwrap_or_else(|e| e.into_inner());
            if now - *last < 60_000 {
                return;
            }
            *last = now;
        }
        let after_ms = cfg.nudge_after_mins as i64 * 60_000;
        let Ok(sessions) = self.engine.store.list_sessions() else { return };
        // Orchestrators (sub-orchestrators included) legitimately sit idle
        // after a worker's report; only workers owe replies.
        let orchestrators: std::collections::HashSet<String> = self.engine.store.list_orchestrators()
            .map(|os| os.into_iter().map(|o| o.id).collect())
            .unwrap_or_default();
        for s in sessions {
            let Some(orch) = s.orchestrator_id.clone() else { continue };
            if orchestrators.contains(&s.id) {
                continue;
            }
            if s.status.is_terminal() || s.merge_handled() || (s.pr_number.is_some() && !s.repo.is_empty()) {
                continue;
            }
            if s.activity != crate::types::ActivityState::Idle {
                continue;
            }
            let Some(idle_since) = s.activity_since else { continue };
            let Ok(clock) = self.engine.store.message_clock(&s.id) else { continue };
            if !message_stall_due(&clock, s.started_at, idle_since, now, after_ms) {
                continue;
            }
            let owed = clock.last_received_at.unwrap_or(0).max(s.started_at);
            match self.engine.store.claim_stall_nudge(&s.id, owed) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => {
                    tracing::warn!("record stall nudge for {}: {e}", s.id);
                    continue;
                }
            }
            let idle_mins = (now - idle_since) / 60_000;
            let nudge = format!(
                "[Ninox] You have been idle for {idle_mins} min and Ninox has not seen you send anything since \
                 your last incoming message. Text in your own pane never reaches your orchestrator: if you \
                 wrote a read-back, report or question, send it with `ninox send {orch} \"...\"`. \
                 If you already sent it, ignore this.",
            );
            if let Err(e) = self.engine.send_to_session_with_kind(&s.id, &nudge, crate::types::MessageKind::Status).await {
                tracing::warn!("send stall nudge to {}: {e}", s.id);
            }
            let note = format!(
                "[Ninox] Worker {} has been idle for {idle_mins} min without messaging anyone since its last \
                 incoming message. It may be waiting on you, or wrote its reply only in its own pane; it was nudged.",
                s.id,
            );
            if let Err(e) = self.engine.send_to_session_with_kind(&orch, &note, crate::types::MessageKind::Status).await {
                tracing::warn!("tell orchestrator {orch} about stalled {}: {e}", s.id);
            }
        }
    }

    // ── Update check ─────────────────────────────────────────────────────────

    /// Checks the configured channel's CodeArtifact generic package for a
    /// newer ninox release than the one running, emitting an
    /// `UpdateAvailable` notification the first time a given newer version
    /// is seen. `ninox-core`'s own `CARGO_PKG_VERSION` is the running
    /// version: every workspace crate is versioned together, so it equals
    /// the UI binary's. Failures are soft — one log line per distinct
    /// message, and an expired SSO session is a prompt, not an error.
    async fn poll_update_check(&self) {
        let current_version = env!("CARGO_PKG_VERSION");
        let channel = crate::config::AppConfig::load_or_default().update.channel;
        if !binary_update::platform_supported(std::env::consts::OS, std::env::consts::ARCH) {
            return;
        }
        let Some(package) = binary_update::generic_package(std::env::consts::OS, channel) else {
            return;
        };
        let source: Arc<dyn UpdateSource> = self
            .update_source
            .clone()
            .unwrap_or_else(|| Arc::new(GenericPackageUpdateSource { channel }));
        let latest = match update_check::check_for_update(source.as_ref(), package, current_version).await {
            Ok(latest) => {
                *self.last_update_error.lock().unwrap_or_else(|e| e.into_inner()) = None;
                crate::server_control::clear_issue("update_check");
                latest
            }
            Err(e) => {
                let (msg, expected) = if binary_update::is_session_error(&e) {
                    ("update check skipped: `aws sso login` needed".to_string(), true)
                } else {
                    (format!("update check against {package} failed: {e}"), false)
                };
                let repeated = {
                    let mut last = self.last_update_error.lock().unwrap_or_else(|e| e.into_inner());
                    let repeated = last.as_deref() == Some(msg.as_str());
                    *last = Some(msg.clone());
                    repeated
                };
                if !repeated {
                    if expected {
                        tracing::info!("{msg}");
                    } else {
                        tracing::warn!("{msg}");
                    }
                }
                crate::server_control::report_issue("update_check", msg);
                None
            }
        };
        if let Some(latest) = latest {
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
                    body:       format!("ninox {latest} is available on the {channel} channel — running {current_version}"),
                    session_id: None,
                    created_at: now_millis(),
                }));
            }
        }
    }

    // ── Remote machine health ────────────────────────────────────────────────

    /// Scan every live local session's pane for its harness's login
    /// failure, so the UIs can say "log in again" and offer to relaunch the
    /// sessions that hit it. Each scan replaces the session's row.
    async fn poll_harness_auth(&self) {
        let store = &self.engine.store;
        let Ok(sessions) = store.list_sessions() else { return };
        let live: std::collections::HashSet<&str> = sessions.iter()
            .filter(|s| !s.status.is_terminal() && s.machine_id.is_none())
            .map(|s| s.id.as_str())
            .collect();
        for session in sessions.iter().filter(|s| live.contains(s.id.as_str())) {
            let Some(auth) = crate::harness_auth::for_harness(&session.agent_type) else { continue };
            let Ok(screen) = runtime::read_screen(&session.id, None, false).await else {
                if !runtime::has_session(&session.id).await {
                    let _ = store.clear_harness_auth(&session.id);
                }
                continue;
            };
            let result = match auth.detect_on_screen(&screen) {
                Some(line) => store.record_harness_auth(&session.id, auth.harness, Some(&session.id), &line),
                None => store.clear_harness_auth(&session.id),
            };
            if let Err(e) = result {
                tracing::warn!("harness auth scan for {}: {e}", session.id);
            }
        }
        for row in store.harness_auth_failures().unwrap_or_default() {
            if let Some(id) = row.session_id.filter(|id| !live.contains(id.as_str())) {
                let _ = store.clear_harness_auth(&id);
            }
        }
    }

    /// Background health probe + reconnect-with-backoff for every enabled
    /// `MachineProfile` — design spec §6. A dead SSH control connection gets
    /// a non-interactive reconnect attempt (`self.health_probe` —
    /// discovery only, never installs or replaces anything on the remote
    /// host, unlike the interactive `machine add`/`machine update` flow),
    /// backing off exponentially (capped ~2 minutes,
    /// `remote_health::next_backoff_secs`) on repeated failure so a
    /// sustained outage doesn't retry every tick.
    ///
    /// Also persists each transition as a `MachineHealth` keyed by
    /// `machine_id` (`Store::set_machine_health`) — the fleet-overview
    /// spec's remote-node connection-status badge reads this through
    /// `Row::machine_health`. This never touches a *session* row, so it
    /// still never reaches into UI selection state (that lives
    /// client-side, not here), satisfying that half of the spec's §6
    /// invariant.
    async fn poll_remote_machine_health(&self) {
        let cfg = AppConfig::load_or_default();
        let now = now_millis();
        let configured_ids: std::collections::HashSet<&str> =
            cfg.remote_machines.machines.iter().map(|m| m.id.as_str()).collect();
        {
            // Drop bookkeeping for any profile no longer configured (e.g.
            // `machine remove`) — same shape as `enrichment_cache`'s own
            // `retain` against live sessions, so this map tracks only
            // currently-configured machines rather than growing unboundedly
            // over every id ever seen by a long-running daemon. The stored
            // `MachineHealth` row is pruned the same way, so a removed
            // profile doesn't leave a stale badge behind. Runs even while
            // `[remote_machines]` is disabled (ahead of the early return
            // below) — otherwise a profile removed during a stretch where
            // remote machines are switched off would leave its persisted
            // health row stranded until (if ever) remote machines are
            // re-enabled.
            let mut health = self.remote_machine_health.lock().unwrap_or_else(|e| e.into_inner());
            let pruned: Vec<String> =
                health.keys().filter(|id| !configured_ids.contains(id.as_str())).cloned().collect();
            health.retain(|id, _| configured_ids.contains(id.as_str()));
            for id in pruned {
                if let Err(err) = self.engine.store.remove_machine_health(&id) {
                    tracing::warn!("failed to prune machine_health for removed profile {id}: {err:#}");
                }
            }
        }
        if !cfg.remote_machines.enabled {
            return;
        }
        let due: Vec<MachineProfile> = {
            let health = self.remote_machine_health.lock().unwrap_or_else(|e| e.into_inner());
            cfg.remote_machines
                .machines
                .into_iter()
                .filter(|m| m.enabled)
                .filter(|m| health.get(&m.id).is_none_or(|s| now >= s.next_attempt_at))
                .collect()
        };

        // Probed concurrently, not one by one: a `ConnectTimeout=15`
        // serialized across N unreachable machines would otherwise block
        // this whole tick — and, since this runs inline in `Poller::start`'s
        // single `select!` loop, every *other* tick too — for up to
        // 15s × N. Bounded to ~15s total instead, the same per-tick
        // worst-case this architecture already accepts for a single slow
        // network call (e.g. `poll_github`'s REST/GraphQL requests), just no
        // longer multiplied by machine count.
        let mut set = tokio::task::JoinSet::new();
        for profile in due {
            let probe = self.health_probe.clone();
            set.spawn(async move {
                let result = probe.reconnect(&profile.ssh_target, &profile.id).await;
                (profile, result)
            });
        }
        while let Some(outcome) = set.join_next().await {
            match outcome {
                Ok((profile, Ok(()))) => self.note_machine_reachable(&profile),
                Ok((profile, Err(e))) => self.note_machine_unreachable(&profile, &e),
                Err(join_err) => tracing::warn!("remote health probe task panicked: {join_err}"),
            }
        }
    }

    /// Reset a profile's backoff state on a successful probe, notifying only
    /// if it was previously flagged unreachable (dedup, same shape as
    /// `clear_github_lookup_failed`'s pairing with `notify_github_lookup_failed`).
    fn note_machine_reachable(&self, profile: &MachineProfile) {
        let was_unreachable = {
            let mut health = self.remote_machine_health.lock().unwrap_or_else(|e| e.into_inner());
            health
                .insert(profile.id.clone(), MachineHealthState::default())
                .is_some_and(|s| s.unreachable_notified)
        };
        if let Err(err) = self.engine.store.set_machine_health(&profile.id, MachineHealth::Connected) {
            tracing::warn!("failed to persist machine_health for {}: {err:#}", profile.id);
        }
        if !was_unreachable {
            return;
        }
        tracing::info!("machine '{}' ({}) reachable again", profile.label, profile.id);
        self.engine.emit(Event::Notification(Notification {
            id:         format!("machine-reachable-{}", profile.id),
            kind:       NotificationKind::MachineReachable,
            title:      format!("Reconnected — {}", profile.label),
            body:       format!("SSH connection to {} is back up.", profile.ssh_target),
            session_id: None,
            created_at: now_millis(),
        }));
    }

    /// Advance a profile's backoff and emit `MachineUnreachable` once per run
    /// of consecutive failures — deduped the same way `notify_github_lookup_failed`
    /// dedupes via its own cache entry's flag.
    fn note_machine_unreachable(&self, profile: &MachineProfile, e: &anyhow::Error) {
        let (already_notified, backoff) = {
            let mut health = self.remote_machine_health.lock().unwrap_or_else(|e| e.into_inner());
            let state = health.entry(profile.id.clone()).or_default();
            let backoff = remote_health::next_backoff_secs(state.backoff_secs);
            state.backoff_secs = backoff;
            state.next_attempt_at = now_millis() + backoff as i64 * 1000;
            let already = state.unreachable_notified;
            state.unreachable_notified = true;
            (already, backoff)
        };
        // A brief blip still doubling its backoff reads as `Reconnecting`;
        // once it's pinned at the cap the outage reads as `Unreachable` —
        // see `remote_health::MAX_BACKOFF_SECS`'s doc comment.
        let health = if backoff >= MAX_BACKOFF_SECS { MachineHealth::Unreachable } else { MachineHealth::Reconnecting };
        if let Err(err) = self.engine.store.set_machine_health(&profile.id, health) {
            tracing::warn!("failed to persist machine_health for {}: {err:#}", profile.id);
        }
        tracing::warn!(
            "machine '{}' ({}) unreachable, retrying in {backoff}s: {e:#}",
            profile.label, profile.id,
        );
        if already_notified {
            return;
        }
        self.engine.emit(Event::Notification(Notification {
            id:         format!("machine-unreachable-{}", profile.id),
            kind:       NotificationKind::MachineUnreachable,
            title:      format!("Machine unreachable — {}", profile.label),
            body:       format!("Lost SSH connection to {} — retrying with backoff.", profile.ssh_target),
            session_id: None,
            created_at: now_millis(),
        }));
    }

    // ── PID liveness ────────────────────────────────────────────────────────

    async fn poll_pids(&self) {
        // Metadata first: a dying worker's last acts (PR create, work
        // request) are processed before the reap below marks it Terminated.
        self.sync_sessions_metadata(&AppConfig::sessions_dir()).await;

        let Ok(sessions) = self.engine.store.list_sessions() else { return };
        let unreconciled = self.unreconciled.lock().unwrap_or_else(|e| e.into_inner()).clone();
        // Every `machine_id: None` session shares this tick's one configured
        // backend, so it's resolved once here rather than per session —
        // `reaction_behavior_for` reloads+reparses the config file on every
        // call, and this loop can run over many sessions each 5-second tick.
        let local_pid_is_local = runtime::reaction_behavior_for(runtime::configured_backend()).pid_is_local();
        let pidless: Vec<&Session> = sessions.iter()
            .filter(|s| s.pid.is_none() && s.machine_id.is_none() && !s.status.is_terminal() && !unreconciled.contains(&s.id))
            .collect();
        if local_pid_is_local && !pidless.is_empty() {
            if let Ok(live) = runtime::list_sessions().await {
                for s in pidless {
                    let host_pid = live.iter().find(|l| l.id == s.id).and_then(|l| l.pid);
                    if crate::lifecycle::pane_pid::adopt_pane_pid(&self.engine.store, s, host_pid) {
                        tracing::info!("recorded pane pid for {}", s.id);
                    }
                }
            }
        }
        let Ok(sessions) = self.engine.store.list_sessions() else { return };
        for mut session in sessions {
            if matches!(session.status, SessionStatus::Done | SessionStatus::Terminated | SessionStatus::Interrupted) {
                continue;
            }
            if unreconciled.contains(&session.id) {
                continue;
            }
            // Dispatches through the session's own `SessionReactions` impl
            // rather than matching `machine_id` directly (see
            // `runtime::reaction_behavior_for_session`). A remote session's
            // pid lives on another machine, so a local `kill(pid, 0)` here
            // would be meaningless (or worse, collide with an unrelated
            // local pid) — per the remote-sessions spec's resolved decision
            // #3, liveness for these comes from `poll_remote_sessions`
            // instead, which copies down the remote machine's own
            // `kill(pid, 0)`-derived status.
            let pid_is_local = match session.kind_data() {
                SessionKindData::Remote { .. } => runtime::reaction_behavior_for_session(&session).pid_is_local(),
                SessionKindData::Local => local_pid_is_local,
            };
            if pid_is_local {
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
    }

    // ── Remote reconciliation ───────────────────────────────────────────────

    /// Per enabled, connected `MachineProfile`, polls that machine's own
    /// `ninox-server` `/api/v1/sessions` over the SSH tunnel and
    /// read→apply→writes the returned rows into the local store, tagged
    /// with `machine_id` — see
    /// `docs/superpowers/specs/2026-10-05-remote-sessions-design.md` §4.
    /// This is what resolves `poll_pids`'s liveness-skip for remote sessions
    /// to a real signal instead of leaving it permanently unknown: a remote
    /// session's `status` already reflects its own machine's
    /// `kill(pid, 0)` check (run by that machine's own poller), so copying
    /// the row down *is* the liveness answer — see the design spec's
    /// resolved decision #3.
    ///
    /// A profile whose SSH `ControlMaster` isn't currently alive is skipped
    /// for this tick, not reconnected: establishing one outside the
    /// interactive `machine add`/`machine update` flow risks a silent hang
    /// on a host-key or passphrase prompt with no terminal to answer it
    /// (see `crate::remote`'s module docs). Reconnection with backoff is
    /// the design spec's §6, explicit follow-on work not attempted here. No
    /// separate `control_master_alive` pre-check is needed to detect that —
    /// `ensure_port_forward` (which `list_remote_session_rows` goes through)
    /// already treats a dead control master as the same clear, specific
    /// error this logs below.
    ///
    /// Each machine gets a bounded [`REMOTE_POLL_TIMEOUT`]: machines are
    /// polled one at a time on this poller's single 5s tick, so one slow —
    /// but technically reachable — machine must not be able to stall local
    /// liveness detection and every other tick behind it indefinitely.
    async fn poll_remote_sessions(&self) {
        let cfg = AppConfig::load_or_default();
        if !cfg.remote_machines.enabled {
            return;
        }
        for profile in cfg.remote_machines.machines.iter().filter(|m| m.enabled).cloned() {
            let backend = crate::runtime::RemoteBackend::for_machine(profile.clone(), cfg.port);
            match tokio::time::timeout(REMOTE_POLL_TIMEOUT, backend.list_remote_session_rows()).await {
                Ok(Ok(sessions)) => self.reconcile_remote_sessions(&profile.id, sessions),
                Ok(Err(e)) => tracing::warn!(
                    "remote reconciliation for machine '{}' ({}) failed: {e}",
                    profile.label, profile.ssh_target,
                ),
                Err(_) => tracing::warn!(
                    "remote reconciliation for machine '{}' ({}) timed out after {:?}",
                    profile.label, profile.ssh_target, REMOTE_POLL_TIMEOUT,
                ),
            }
        }
    }

    /// Writes `sessions` (freshly fetched from `machine_id`'s remote
    /// `ninox-server`) into the local store, tagging each with `machine_id`.
    ///
    /// Adopts the remote's pid-liveness verdict — `Terminated` or
    /// `Interrupted`, the only two outcomes its own poller's `kill(pid, 0)`
    /// check can produce (resolved decision #3) — onto a *currently live*
    /// local row, stamping `terminal_at` in the same write. Nothing else
    /// about `status` is touched by this function:
    ///
    /// - A non-terminal remote status (`Working`, `PrOpen`, …) never changes
    ///   local status at all. Everything beyond raw liveness — PR lifecycle,
    ///   `gate_status`, `cost_usd`, `context_*`, `Done` (a *merge-handling*
    ///   outcome, not a liveness one) — stays under the local orchestrator's
    ///   own polling, which keeps running against `machine_id`-tagged
    ///   sessions too and was never made remote-aware by the remote-sessions
    ///   spec (resolved decision #10). Copying the remote's status
    ///   unconditionally would fight that polling every 5s — relabeling a
    ///   locally-progressed `PrOpen`/`Mergeable`/`Done` back to the remote's
    ///   staler `Working`/`Terminated`.
    /// - An already-terminal local row (`Done`, `Terminated`, or
    ///   `Interrupted`) is never relabeled by a *different* terminal value
    ///   from the remote either — a merge-handled `Done` must stay `Done`,
    ///   and a resumable `Interrupted` (only ever cleared by a user-triggered
    ///   Resume) must not be flattened to `Terminated`.
    ///
    /// A session not already known locally is inserted and announced
    /// (`SessionSpawned`) unless the remote already considers it fully
    /// finished (`Done`/`Terminated`): the remote's own `/api/v1/sessions`
    /// lists every row its store still holds, including ones its retention
    /// sweep hasn't purged yet, so a finished session this store has never
    /// tracked (most often one this store itself already purged) must not be
    /// resurrected only to be swept away again next tick. `Interrupted` is
    /// deliberately *not* excluded here — nothing ever purges it (see
    /// `sweep_retired_sessions`), and it's exactly the actionable,
    /// resumable case this reconciliation exists to surface.
    ///
    /// Ninox session ids are plain name slugs with no per-machine scoping
    /// (e.g. a ticket-named worker), so the *same* id existing on two
    /// different machines — or on a remote machine and this one — is a real
    /// possibility, not a hypothetical. [`Self::update_live_session_row`]
    /// (like every other caller in this file) looks a row up by id alone, so
    /// calling it here unconditionally would let one machine's view of
    /// `some-id` overwrite a same-id row this store already knows belongs to
    /// someone else — a local session flipped `Terminated` by an unrelated
    /// remote one of the same name, or two remote profiles clobbering each
    /// other's status for a shared id. The explicit ownership check below
    /// closes that: a row that already exists under a *different* owner (or
    /// whose ownership can't be confirmed right now) is left untouched and
    /// not inserted either, since the id is already taken.
    fn reconcile_remote_sessions(&self, machine_id: &str, sessions: Vec<Session>) {
        for mut remote in sessions {
            remote.machine_id = Some(machine_id.to_string());
            match self.engine.store.get_session(&remote.id) {
                Ok(None) => {
                    let already_finished = matches!(remote.status, SessionStatus::Done | SessionStatus::Terminated);
                    if !already_finished && self.engine.store.upsert_session(&remote).is_ok() {
                        self.engine.emit(Event::SessionSpawned(remote));
                    }
                }
                Ok(Some(existing)) if existing.machine_id.as_deref() != Some(machine_id) => {
                    tracing::warn!(
                        "remote reconciliation: machine '{machine_id}' reports a session '{}', but the local \
                         store already has a row for that id owned by {:?} — ninox session ids aren't scoped \
                         per machine, so this is treated as an id collision and skipped rather than risking \
                         cross-machine corruption",
                        remote.id, existing.machine_id,
                    );
                }
                Ok(Some(existing)) => {
                    if existing.status.is_terminal() {
                        continue; // never relabel a row this store already finished with
                    }
                    let Some(remote_terminal_status) = (match remote.status {
                        SessionStatus::Terminated | SessionStatus::Interrupted => Some(remote.status.clone()),
                        _ => None, // no pid-death verdict to adopt — leave local status alone
                    }) else {
                        continue;
                    };
                    let expected = remote_terminal_status.clone();
                    let remote_terminal_at = remote.terminal_at;
                    // Re-reads via `update_live_session_row` (so a write from
                    // another local process in the interim, e.g. `ninox
                    // reap`, still wins) and re-checks freshness inside the
                    // closure too: something may have already finalized this
                    // row terminal during the remote fetch's await, in which
                    // case it's left alone rather than relabeled.
                    let written = self.update_live_session_row(&remote, move |row| {
                        if !row.status.is_terminal() {
                            row.status = remote_terminal_status;
                            row.terminal_at = Some(remote_terminal_at.unwrap_or_else(now_millis));
                        }
                    });
                    if let Some(row) = written {
                        if row.status == expected {
                            self.engine.emit(Event::SessionUpdated(row, SessionFields::STATUS | SessionFields::TERMINAL_AT));
                        }
                    }
                }
                Err(e) => tracing::warn!(
                    "remote reconciliation: could not confirm the existing owner of session '{}' before merging \
                     machine '{machine_id}'s view — skipping this tick rather than risk writing over a \
                     different owner's row: {e}",
                    remote.id,
                ),
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
            // Answers are about a question *this* session asked — deliver
            // them even when it has already finished or died, same as work
            // requests above (its next resume still needs to see the answer).
            self.deliver_user_request_answers(&session).await;

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
                    // The URL `gh pr create` printed names the PR's real repo,
                    // which differs from the spawn workspace's when the worker
                    // opened the PR elsewhere (`-R other/repo`, a moved worktree).
                    if let Some((url_repo, _)) = first.url.as_deref().and_then(crate::github::parse_pr_url) {
                        session.repo = url_repo;
                    }
                    // Write through the live row, not the tick-start
                    // snapshot — `deliver_work_requests` awaited above, and
                    // the statusline process can land cost/context at any
                    // moment (see `update_live_session_row`).
                    let Some(written) = self.update_live_session_row(&session, |row| {
                        row.pr_number = session.pr_number;
                        row.repo      = session.repo.clone();
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
        let config = AppConfig::load_or_default();
        if !config.brain_harvest.enabled {
            return;
        }
        let Some(workspace) = session.workspace_path.clone() else { return };
        let workspace_path: PathBuf = workspace.into();
        // The session's write catalogue, so the harvest lands where the
        // worker itself writes, never a read-only source.
        let brain_path: PathBuf = match crate::brain_set::BrainSet::from_session(session, &config)
            .authorize_write(&config, None)
        {
            Ok(member) => member.catalogue.path,
            Err(e) => {
                tracing::warn!("brain harvest skipped for {}: {e}", session.id);
                return;
            }
        };
        let runner       = self.harvest_runner.clone();
        let session_id    = session.id.clone();
        let panic_session_id = session_id.clone();
        let vault_lock   = self.vault_lock(&brain_path);

        let harvest_store = Arc::clone(&self.engine.store);
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
            match runner.run(prompt, workspace_path, brain_path).await {
                Ok(()) => {
                    let _ = harvest_store.clear_harness_auth("harvest:claude-code");
                }
                Err(e) => {
                    tracing::warn!("brain harvest failed for session {session_id}: {e}");
                    crate::harness_auth::record_output_failure(&harvest_store, "claude-code", "harvest", &e.to_string());
                }
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
                match self.engine.send_to_session_with_kind(&orch, &msg, crate::types::MessageKind::RequestForWork).await {
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

    /// Forward every answered-but-undelivered `ninox user-request` to the
    /// session that asked it: async rows immediately, sync rows only once
    /// [`SYNC_ANSWER_REDELIVERY_GRACE_MS`] has passed since the answer —
    /// i.e. the asker's own blocking `ask`/`wait` evidently timed out
    /// before the human answered, and nothing else will ever hand it over.
    /// Mirrors `deliver_work_requests`'s per-row shape for the opposite
    /// direction (human answer → requester, not worker → orchestrator): a
    /// row whose delivery fails (e.g. the requester died before the answer
    /// arrived) is logged and left undelivered rather than marked, so a
    /// later tick — once the requester resumes — still has it to
    /// redeliver; it never aborts the rest of this poll tick.
    async fn deliver_user_request_answers(&self, session: &crate::types::Session) {
        let sync_cutoff = now_millis() - SYNC_ANSWER_REDELIVERY_GRACE_MS;
        let Ok(rows) = self.engine.store.undelivered_answered_user_requests(&session.id, sync_cutoff) else {
            return;
        };
        for row in rows {
            let msg = crate::lifecycle::reactions::format_user_request_answer_reaction(
                &row.id, &row.prompt, row.answer.as_deref().unwrap_or_default(),
            );
            if let Err(e) = self.engine.send_to_session(&session.id, &msg).await {
                tracing::warn!("send user-request answer to {}: {e}", session.id);
                continue;
            }
            if let Err(e) = self.engine.store.mark_user_request_answer_delivered(&row.id, now_millis()) {
                tracing::warn!("mark user-request answer delivered for {}: {e}", row.id);
            }
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
        let mut observed: HashMap<PrKey, crate::github_graphql::PrSnapshot> = HashMap::new();

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
            let attribution = match self.merge_attribution(&session, &pr_status.head_ref) {
                Attribution::Mismatch => {
                    self.warn_attribution_mismatch(&session, pr_number, &pr_status.head_ref);
                    if pr_status.merged { continue; }
                    Attribution::Unknown
                }
                a => a,
            };
            if self.handle_merge_detection_with(&session, pr_number, pr_status.merged, auto_reap, &pr_status.head_sha, attribution).await {
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
            // Only a sweep where every fetch succeeded feeds `pr_events`: an
            // empty list from a failed call would read as "all comments gone".
            let mut complete = true;
            let checks = match gh.get_ci_checks(&owner, &repo, &pr_status.head_sha).await {
                Ok(c)  => c,
                Err(e) => { tracing::warn!("github ci checks: {e}"); complete = false; vec![] }
            };
            let ci = self.ingest_ci(&session, pr_id, &checks, pr_status.mergeable).await;

            // -- Review threads + issue comments (throttled via seen_comment_ids) --
            let threads = match gh.get_review_threads(&owner, &repo, pr_number).await {
                Ok(t)  => t,
                Err(e) => { tracing::warn!("github review threads: {e}"); complete = false; vec![] }
            };
            let issue_comments = match gh.get_issue_comments(&owner, &repo, pr_number).await {
                Ok(c)  => c,
                Err(e) => { tracing::warn!("github issue comments: {e}"); complete = false; vec![] }
            };
            let (has_new, review_reaction_already_sent, has_changes_requested) =
                self.scan_reviews(&session.id, pr_id, &threads, &issue_comments);

            self.apply_status_and_gate(&session, &pr_status, &ci, has_changes_requested);

            self.emit_review_notification(&session, has_new, review_reaction_already_sent);
            if !complete {
                continue;
            }
            observed.insert(
                PrKey { repo: session.repo.clone(), number: pr_number },
                crate::github_graphql::PrSnapshot {
                    closed: pr_status.state == "closed" && !pr_status.merged,
                    status: pr_status,
                    checks,
                    threads,
                    issue_comments,
                    meta: Default::default(),
                },
            );
        }
        self.deliver_pr_events(&observed).await;
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
                crate::server_control::report_issue("github", format!("GitHub fetch failed: {e:#}"));
                self.note_batch_error(&e);
                return;
            }
        };
        crate::server_control::clear_issue("github");
        tracing::info!(
            "github sweep: {} PRs, cost {} points, token remaining {}/{}",
            pr_keys.len(), result.rate_limit.cost, result.rate_limit.remaining, result.rate_limit.limit,
        );
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
            let attribution = match self.merge_attribution(&session, &snap.status.head_ref) {
                Attribution::Mismatch => {
                    self.warn_attribution_mismatch(&session, pr_number, &snap.status.head_ref);
                    if snap.status.merged { continue; }
                    Attribution::Unknown
                }
                a => a,
            };
            if self.handle_merge_detection_with(&session, pr_number, snap.status.merged, auto_reap, &snap.status.head_sha, attribution).await {
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
            let (has_new, already_sent, has_changes_requested) =
                self.scan_reviews(&session.id, pr_id, &snap.threads, &snap.issue_comments);
            self.apply_status_and_gate(&session, &snap.status, &ci, has_changes_requested);
            self.emit_review_notification(&session, has_new, already_sent);
        }

        self.deliver_watch_updates(&watches, &result).await;
        self.deliver_pr_events(&result.prs).await;
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
        use crate::github_budget::{plan, BudgetLevel, GithubBudget};
        // This only ever runs after a successful fetch (the caller returns
        // early on error before reaching here) — reset the Retry-After-less
        // backoff so a fresh outage after a recovery starts doubling over
        // from 120s again, not from wherever the last outage left off.
        *self.rate_limit_backoff_secs.lock().unwrap_or_else(|e| e.into_inner()) = 0;
        let now = now_millis();
        let plan = plan(rate_limit, now);
        *self.github_interval_factor.lock().unwrap_or_else(|e| e.into_inner()) = plan.factor;
        if let Some(until) = plan.pause_until_ms {
            *self.rate_limit_pause_until.lock().unwrap_or_else(|e| e.into_inner()) = until;
        }
        let previous = std::mem::replace(
            &mut *self.budget_level.lock().unwrap_or_else(|e| e.into_inner()),
            plan.level,
        );
        if previous != plan.level {
            match plan.level {
                BudgetLevel::Normal => tracing::info!(
                    "github budget recovered ({} of {} remaining) — polling at normal cadence",
                    rate_limit.remaining, rate_limit.limit,
                ),
                BudgetLevel::BackingOff => tracing::warn!(
                    "github budget low ({} of {} remaining, cost {}) — stretching poll interval x{:.1}",
                    rate_limit.remaining, rate_limit.limit, rate_limit.cost, plan.factor,
                ),
                BudgetLevel::Paused => tracing::warn!(
                    "github budget nearly spent ({} of {} remaining) — pausing polling until {}",
                    rate_limit.remaining, rate_limit.limit, rate_limit.reset_at,
                ),
            }
        }
        if rate_limit.limit > 0 || rate_limit.remaining > 0 {
            let base = AppConfig::load_or_default().github.effective_poll_interval_secs();
            let _ = self.engine.store.save_github_budget(&GithubBudget {
                limit:         rate_limit.limit,
                remaining:     rate_limit.remaining,
                cost:          rate_limit.cost,
                reset_at:      rate_limit.reset_at,
                observed_at:   now,
                level:         plan.level,
                interval_secs: (base as f64 * plan.factor) as u64,
            });
        }
    }

    /// Delete watches whose opener session has ended (`Done`/`Terminated`,
    /// or gone from the store): nobody is waiting on them, yet each still
    /// costs a PR in every sweep. Kept regardless: watches without an
    /// opener, watches opened by an orchestrator (it outlives its workers
    /// and still wants the PR's events), and watches on a PR that a live
    /// session tracks as its own or that another live session also watches.
    fn drop_ended_watches(&self) {
        let Ok(sessions) = self.engine.store.list_sessions() else { return };
        let Ok(watches) = self.engine.store.list_pr_watches() else { return };
        let orchestrators: std::collections::HashSet<String> = self
            .engine
            .store
            .list_orchestrators()
            .unwrap_or_default()
            .into_iter()
            .map(|o| o.id.to_string())
            .collect();
        let live: std::collections::HashSet<&str> = sessions
            .iter()
            .filter(|s| !matches!(s.status, SessionStatus::Done | SessionStatus::Terminated))
            .map(|s| s.id.as_str())
            .collect();
        let tracked: std::collections::HashSet<(String, u64)> = sessions
            .iter()
            .filter(|s| live.contains(s.id.as_str()))
            .filter_map(|s| s.pr_number.map(|n| (s.repo.clone(), n)))
            .collect();
        let opener_is_live = |id: &str| live.contains(id) || orchestrators.contains(id);
        for w in &watches {
            let Some(opener) = w.opener_session_id.as_deref() else { continue };
            if opener_is_live(opener) || tracked.contains(&(w.repo.clone(), w.pr_number)) {
                continue;
            }
            let another_live_watcher = watches.iter().any(|o| {
                o.repo == w.repo
                    && o.pr_number == w.pr_number
                    && o.opener_session_id.as_deref().is_some_and(|id| id != opener && opener_is_live(id))
            });
            if another_live_watcher {
                continue;
            }
            if self.engine.store.delete_pr_watch(&w.repo, w.pr_number, Some(opener)).unwrap_or(0) > 0 {
                tracing::info!("dropped watch on {}#{}: opener {opener} has ended", w.repo, w.pr_number);
            }
        }
    }

    /// Fan the batched snapshots out to the registry's PR watches: merge/close
    /// (terminal, and auto-closing), newly-failing CI and new
    /// CHANGES_REQUESTED review activity.
    ///
    /// Watches are *notification-only*. They deliver a `Notification` (feed +
    /// desktop), plus a terminal reaction to the opener; every other message
    /// to the opener comes from `deliver_pr_events`. Nothing else —
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
                // A worker watching its own PR hears `merged`/`closed` from
                // `deliver_pr_events` instead.
                let opener_owns_it = w.opener_session_id.as_deref()
                    .and_then(|o| self.engine.store.get_session(o).ok().flatten())
                    .is_some_and(|s| s.pr_number == Some(w.pr_number) && s.repo.eq_ignore_ascii_case(&w.repo));
                if let Some(opener) = w.opener_session_id.as_ref().filter(|_| !opener_owns_it) {
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

    /// Every `(subscriber, PR)` pair Ninox delivers PR events for: each
    /// non-finished session's own PR, plus each `ninox open --pr` watch
    /// whose opener is still around. A session watching its own PR collapses
    /// into the one own subscription, so it hears each event once. `None` on
    /// a store error, so a failed read can never prune every cursor.
    fn pr_event_subscriptions(&self) -> Option<std::collections::BTreeMap<CursorKey, PrSubscription>> {
        let sessions = self.engine.store.list_sessions().ok()?;
        let watches = self.engine.store.list_pr_watches().ok()?;
        // Subscriber id → deliverable. A merged worker kept alive still hears
        // about its own merge and its other watches. Interrupted sessions
        // keep their cursor unadvanced, so what happens while they are down
        // arrives on resume.
        let subscribers: HashMap<&str, bool> = sessions.iter()
            .filter(|s| !matches!(s.status, SessionStatus::Done | SessionStatus::Terminated))
            .map(|s| (s.id.as_str(), s.status != SessionStatus::Interrupted))
            .collect();
        let mut subs = std::collections::BTreeMap::new();
        for s in &sessions {
            let (Some(number), Some(&deliverable)) = (s.pr_number, subscribers.get(s.id.as_str())) else { continue };
            if s.repo.is_empty() {
                continue;
            }
            subs.insert(
                CursorKey { subscriber: s.id.clone(), repo: s.repo.to_lowercase(), number },
                PrSubscription { own: true, deliverable },
            );
        }
        for w in watches {
            let Some(opener) = w.opener_session_id.filter(|o| !o.is_empty()) else { continue };
            let Some(&deliverable) = subscribers.get(opener.as_str()) else { continue };
            subs.entry(CursorKey { subscriber: opener, repo: w.repo.to_lowercase(), number: w.pr_number })
                .or_insert(PrSubscription { own: false, deliverable });
        }
        Some(subs)
    }

    /// Caches each observed PR for `ninox pr status`, then runs
    /// [`crate::pr_events::detect`] for every subscription this sweep
    /// observed and delivers what changed. The cursor is persisted before
    /// delivery: if it can't be written nothing is sent, so the next sweep
    /// re-detects rather than double-delivering after a restart.
    async fn deliver_pr_events(&self, observed: &HashMap<PrKey, crate::github_graphql::PrSnapshot>) {
        let cfg = self.pr_events_config.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let now = now_millis();
        // GitHub slugs are case-insensitive; a watch typed `Owner/Repo`
        // and a session on `owner/repo` are one PR.
        let observed: HashMap<(String, u64), &crate::github_graphql::PrSnapshot> = observed.iter()
            .map(|(k, snap)| ((k.repo.to_lowercase(), k.number), snap))
            .collect();
        for ((repo, number), snap) in &observed {
            let view = crate::pr_events::PrStatusView::from_snapshot(repo, *number, snap, now);
            if let Err(e) = self.engine.store.save_pr_status(&view) {
                tracing::warn!("cache pr status {repo}#{number}: {e}");
            }
        }
        let Some(subs) = self.pr_event_subscriptions() else { return };
        let stuck_after_ms = cfg.ci_stuck_after_mins as i64 * 60_000;
        for (key, sub) in &subs {
            if !sub.deliverable {
                continue;
            }
            let Some(snap) = observed.get(&(key.repo.clone(), key.number)) else { continue };
            let mut cursor = self.engine.store.load_pr_event_cursor(key).ok().flatten().unwrap_or_default();
            // Activity on a worker's own PR between its creation and this
            // first sweep is news; on a newly watched PR it is history.
            let report_since = sub.own.then_some(now - OWN_PR_BASELINE_GRACE_MS);
            let events = crate::pr_events::detect_since(&mut cursor, snap, now, stuck_after_ms, report_since);
            if let Err(e) = self.engine.store.save_pr_event_cursor(key, &cursor, now) {
                tracing::warn!("save pr event cursor {key:?}: {e}");
                continue;
            }
            if events.is_empty() {
                continue;
            }
            let finish_hint = sub.own && !self.about_to_be_reaped_at_merge(&key.subscriber);
            let msg = crate::pr_events::format_delivery_with(sub.own, finish_hint, &key.repo, key.number, &events);
            self.engine.emit(Event::PrEvents {
                session_id: key.subscriber.clone(),
                repo:       key.repo.clone(),
                number:     key.number,
                events,
            });
            if let Err(e) = self.engine
                .send_to_session_with_kind(&key.subscriber, &msg, crate::types::MessageKind::PrEvent)
                .await
            {
                tracing::warn!("deliver pr events for {}#{} to {}: {e}", key.repo, key.number, key.subscriber);
            }
        }
        let keep: std::collections::BTreeSet<CursorKey> = subs.into_keys().collect();
        if let Err(e) = self.engine.store.retain_pr_event_cursors(&keep) {
            tracing::warn!("prune pr event cursors: {e}");
        }
        let _ = self.engine.store.prune_pr_status_cache(now - PR_STATUS_CACHE_TTL_MS);
    }

    /// True when `reap_on_merge` is active and the worker's criteria are
    /// only "PR merged", so merge detection removes it without its help.
    fn about_to_be_reaped_at_merge(&self, session_id: &str) -> bool {
        if *self.completion_mode.lock().unwrap_or_else(|e| e.into_inner()) != CompletionMode::ReapOnMerge {
            return false;
        }
        self.engine.store.get_session(session_id).ok().flatten().is_some_and(|s| merge_reaps_worker(&s))
    }

    /// Nudges a worker idle with an open PR that has been quiet too long,
    /// and tells its orchestrator once per quiet spell. Reads only the store
    /// (sessions, cursors, the status cache): no GitHub call.
    async fn check_pr_hangs(&self, cfg: &crate::config::PrEventsConfig) {
        if cfg.hang_nudge_after_mins == 0 {
            return;
        }
        let after_ms = cfg.hang_nudge_after_mins as i64 * 60_000;
        let now = now_millis();
        let Ok(sessions) = self.engine.store.list_sessions() else { return };
        for s in sessions {
            if s.merge_handled() || s.status.is_terminal() || s.repo.is_empty() {
                continue;
            }
            let Some(number) = s.pr_number else { continue };
            if s.activity != crate::types::ActivityState::Idle {
                continue;
            }
            let Some(idle_since) = s.activity_since else { continue };
            let key = CursorKey { subscriber: s.id.clone(), repo: s.repo.to_lowercase(), number };
            let Ok(Some(mut cursor)) = self.engine.store.load_pr_event_cursor(&key) else { continue };
            if !hang_due(&cursor, idle_since, now, after_ms) {
                continue;
            }
            // Only speak for GitHub when Ninox has recently seen the PR, and
            // not while CI is still running (`ci_pending_stuck` covers that).
            let view = self.engine.store.get_pr_status(&key.repo, number).ok().flatten();
            let Some(fresh_view) = view.as_ref().filter(|v| now - v.observed_at <= PR_STATUS_FRESH_MS) else { continue };
            if fresh_view.ci == crate::pr_events::CiVerdict::Pending {
                continue;
            }
            let quiet_mins = (now - cursor.quiet_since()) / 60_000;
            let tell_orchestrator = s.orchestrator_id.clone().filter(|_| !cursor.hang_orch_notified);
            cursor.hang_nudged_episode = Some(idle_since);
            cursor.hang_nudged_at = now;
            cursor.hang_nudges_in_spell += 1;
            if tell_orchestrator.is_some() {
                cursor.hang_orch_notified = true;
            }
            if let Err(e) = self.engine.store.save_pr_event_cursor(&key, &cursor, now) {
                tracing::warn!("save hang state for {}: {e}", s.id);
                continue;
            }
            self.engine.emit(Event::Notification(Notification {
                id:         format!("pr-quiet-{}", s.id),
                kind:       NotificationKind::AgentStuck,
                title:      format!("Idle with a quiet PR — {}", s.name),
                body:       format!("{}#{number}: no PR event for {quiet_mins} min", s.repo),
                session_id: Some(s.id.clone()),
                created_at: now,
            }));
            let nudge = crate::pr_events::format_hang_nudge(view.as_ref(), &s.repo, number, quiet_mins, now);
            if let Err(e) = self.engine.send_to_session_with_kind(&s.id, &nudge, crate::types::MessageKind::Status).await {
                tracing::warn!("send pr hang nudge to {}: {e}", s.id);
            }
            if let Some(orch) = tell_orchestrator {
                let note = crate::pr_events::format_hang_orchestrator(&s.id, &s.repo, number, view.as_ref(), quiet_mins);
                if let Err(e) = self.engine.send_to_session_with_kind(&orch, &note, crate::types::MessageKind::Status).await {
                    tracing::warn!("tell orchestrator {orch} about quiet PR of {}: {e}", s.id);
                }
            }
        }
    }

    /// One reminder to a worker whose PR merged, that has not completed and
    /// has sat idle for `after_mins`: finish with `ninox complete` or say
    /// what is left. The store claim makes it once-only across ticks and
    /// pollers; a failed send is not retried, so a dead pane is not nagged.
    async fn check_finish_after_merge(&self, after_mins: u64) {
        if after_mins == 0 {
            return;
        }
        let after_ms = after_mins as i64 * 60_000;
        let now = now_millis();
        let Ok(sessions) = self.engine.store.list_sessions() else { return };
        for s in sessions {
            if s.merged_at.is_none()
                || s.status.is_terminal()
                || s.status == SessionStatus::AwaitingReview
                || s.self_completed_at.is_some()
                || s.activity != crate::types::ActivityState::Idle
            {
                continue;
            }
            let Some(idle_since) = s.activity_since else { continue };
            // Idle time counts from the merge at the earliest.
            let quiet_since = idle_since.max(s.merged_at.unwrap_or(idle_since));
            if now - quiet_since < after_ms {
                continue;
            }
            match self.engine.store.claim_finish_nudge(&s.id, now) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => {
                    tracing::warn!("claim finish nudge for {}: {e}", s.id);
                    continue;
                }
            }
            self.engine.emit(Event::Notification(Notification {
                id:         format!("merged-idle-{}", s.id),
                kind:       NotificationKind::AgentStuck,
                title:      format!("Idle after merge — {}", s.name),
                body:       format!("reminded to finish with `ninox complete` after {after_mins} min idle"),
                session_id: Some(s.id.clone()),
                created_at: now,
            }));
            let msg = crate::lifecycle::reactions::format_finish_after_merge_nudge(s.pr_number);
            if let Err(e) = self.engine.send_to_session_with_kind(&s.id, &msg, crate::types::MessageKind::Status).await {
                tracing::warn!("send finish-after-merge nudge to {}: {e}", s.id);
            }
        }
    }

    /// The CI block: summarize → upsert → `CiUpdated` emit → newly-failing
    /// transition (via `enrichment_cache`) → `CiFailure` notification, plus
    /// the symmetric newly-ready transition → `PrReadyToMerge` notification.
    /// The worker itself hears about CI from `deliver_pr_events`. Takes already-fetched checks (the
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
        }

        ci
    }

    /// The review-scan block: dedup via `seen_comment_ids`, then
    /// `upsert_comment` and `ReviewComment` emits. Pure w.r.t. the network —
    /// the caller owns the `get_review_threads`/`get_issue_comments`
    /// fetches. Returns `(has_new, review_reaction_already_sent,
    /// has_changes_requested)`.
    fn scan_reviews(
        &self,
        session_id: &str,
        pr_id: PrId,
        threads: &[crate::github::ReviewThread],
        issue_comments: &[Comment],
    ) -> (bool, bool, bool) {
        let has_changes_requested = threads.iter().any(|t| t.state == "CHANGES_REQUESTED");

        let (has_new, review_reaction_already_sent) = {
            let mut cache = self.enrichment_cache.lock().unwrap();
            let state = cache.entry(session_id.to_string()).or_default();
            let mut has_new = false;

            // Persist + emit every displayable comment — CHANGES_REQUESTED
            // and plain COMMENTED reviews (which also covers inline diff
            // comments, tagged COMMENTED by `get_review_threads`) — so the
            // Info panel's Marginalia feed shows the whole conversation.
            // `has_new` stays CHANGES_REQUESTED-only: it
            // drives the reaction/notification path below, which must not
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
                self.engine.emit(Event::ReviewComment { pr_id, comment });
                if is_changes_requested {
                    has_new = true;
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
            (has_new, already_sent)
        };

        (has_new, review_reaction_already_sent, has_changes_requested)
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

    /// The review notification block: a `PrNeedsAttention` notification for
    /// newly-seen CHANGES_REQUESTED comments. The worker itself hears about
    /// reviews from `deliver_pr_events`.
    fn emit_review_notification(&self, session: &Session, has_new: bool, already_sent: bool) {
        if has_new && !already_sent {
            self.engine.emit(Event::Notification(Notification {
                id:         format!("review-{}", session.id),
                kind:       NotificationKind::PrNeedsAttention,
                title:      format!("Review comments — {}", session.name),
                body:       "Changes requested on your PR".to_string(),
                session_id: Some(session.id.clone()),
                created_at: now_millis(),
            }));
        }
    }

    /// Emit a `GithubLookupFailed` notification once per run of consecutive
    /// failures — deduped the same way `ci_reaction_sent` dedupes CI-failure
    /// reactions, via the enrichment cache.
    /// Whether `head_ref` (the recorded PR's head branch) is this session's
    /// own branch. The durable source is the fleet record's branch (the
    /// workspace may already be gone); the live checkout is the fallback.
    fn merge_attribution(&self, session: &crate::types::Session, head_ref: &str) -> Attribution {
        if head_ref.is_empty() {
            return Attribution::Unknown;
        }
        let recorded = self.engine.store.fleet_record(&session.id).ok().flatten().and_then(|r| r.branch);
        let live = session.workspace_path.as_deref().and_then(crate::github::current_branch);
        if head_ref == session.id || recorded.as_deref() == Some(head_ref) || live.as_deref() == Some(head_ref) {
            return Attribution::Verified;
        }
        // Provenance beats branch comparison: a PR this session itself
        // reported creating (the URL `gh pr create` printed) is its own
        // whatever its head branch is called, and one absent from a non-empty
        // report list is someone else's whatever its head branch is called.
        match crate::lifecycle::completion::pr_provenance(session) {
            crate::lifecycle::completion::Provenance::Reported => return Attribution::Verified,
            crate::lifecycle::completion::Provenance::Excluded => return Attribution::Mismatch,
            crate::lifecycle::completion::Provenance::Unknown => {}
        }
        if crate::lifecycle::completion::foreign_head(session, recorded, head_ref) {
            Attribution::Mismatch
        } else {
            Attribution::Unknown
        }
    }

    fn warn_attribution_mismatch(&self, session: &crate::types::Session, pr_number: u64, head_ref: &str) {
        if !self.attribution_warned.lock().unwrap().insert(session.id.clone()) {
            return;
        }
        tracing::warn!(
            "session {} PR attribution uncertain: recorded {}#{pr_number} has head branch \
             `{head_ref}`, which is not the session's branch — ignoring its merge state",
            session.id, session.repo,
        );
        self.engine.emit(Event::Notification(Notification {
            id:         format!("pr-attribution-uncertain-{}", session.id),
            kind:       NotificationKind::GithubLookupFailed,
            title:      format!("PR attribution uncertain — {}", session.name),
            body:       format!("{}#{pr_number} head `{head_ref}` doesn't match this session's branch", session.repo),
            session_id: Some(session.id.clone()),
            created_at: now_millis(),
        }));
    }

    /// Self-heal rows stamped `merged_at` from a PR that isn't theirs: the
    /// recorded PR's head branch must be *proven* different from the
    /// session's branch before anything is cleared, so a genuinely merged
    /// row (matching or unknown branch) is never touched. The cleared row
    /// re-resolves through the normal adoption paths (hook URL, branch lookup).
    async fn repair_misattributed_merges(&self) {
        let Some(gh) = &self.engine.github else { return };
        let Ok(sessions) = self.engine.store.list_sessions() else { return };
        for session in sessions {
            // Nothing left to reap: clearing a terminal row's PR links only
            // destroys history. Interrupted rows are resumable, so a stale
            // merged_at on one can still get it reaped after resume.
            if session.merged_at.is_none()
                || matches!(session.status, SessionStatus::Done | SessionStatus::Terminated)
                || session.terminal_at.is_some()
            {
                continue;
            }
            let Some(pr_number) = session.pr_number else { continue };
            let Some((owner, repo)) = split_repo(&session.repo) else { continue };
            if self.attribution_checked.lock().unwrap().contains(&session.id) {
                continue;
            }
            let status = match gh.get_pr_status(&owner, &repo, pr_number).await {
                Ok(s)  => s,
                Err(_) => continue, // retry next tick
            };
            if self.merge_attribution(&session, &status.head_ref) != Attribution::Mismatch {
                self.attribution_checked.lock().unwrap().insert(session.id.clone());
                continue;
            }
            tracing::warn!(
                "session {} merged_at repaired: recorded {}#{pr_number} (head `{}`) is not this session's PR",
                session.id, session.repo, status.head_ref,
            );
            let healed = self.update_live_session_row(&session, |row| {
                // A re-adoption landing during the lookup's await must survive.
                if row.pr_number == Some(pr_number) && row.merged_at.is_some() {
                    row.merged_at = None;
                    row.pr_number = None;
                    row.pr_id     = None;
                }
            });
            self.attribution_warned.lock().unwrap().remove(&session.id);
            if let Some(row) = healed {
                self.engine.emit(Event::SessionUpdated(
                    row, SessionFields::MERGED_AT | SessionFields::PR_LINK,
                ));
            }
            if let Some(orch) = session.orchestrator_id.clone() {
                let msg = crate::lifecycle::reactions::format_merge_notice_retracted_reaction(&session, pr_number);
                if let Err(e) = self.engine.send_to_session(&orch, &msg).await {
                    tracing::warn!("send merge-notice retraction to orchestrator {orch}: {e}");
                }
            }
        }
    }

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
    #[cfg(test)]
    async fn handle_merge_detection(
        &self,
        session:    &Session,
        pr_number:  u64,
        pr_merged:  bool,
        auto_reap:  bool,
        head_sha:   &str,
    ) -> bool {
        self.handle_merge_detection_with(session, pr_number, pr_merged, auto_reap, head_sha, Attribution::Verified).await
    }

    /// `attribution` gates every destructive consequence of a merge: only a
    /// positive head-branch match (`Verified`) may reap/clean up or advise
    /// `--force`. `Unknown` still records `merged_at` and sends a plain
    /// notice; `Mismatch` is filtered out by the callers before this runs.
    async fn handle_merge_detection_with(
        &self,
        session:    &Session,
        pr_number:  u64,
        pr_merged:  bool,
        auto_reap:  bool,
        head_sha:   &str,
        attribution: Attribution,
    ) -> bool {
        let destructive_ok = attribution == Attribution::Verified;
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
        // Record GitHub's own merged head sha before anything downstream
        // (the keep-alive stamp, or an immediate `cleanup_session`) can read
        // it — cleanup's `commits_since_sha` check depends on this surviving
        // past this tick, however long the worker stays alive afterward.
        // Unconditional, regardless of the auto-reapable gate just below: the
        // sha is a fact about the merge itself, not about whether this
        // worker gets auto-reaped for it.
        if let Err(e) = self.engine.store.set_merged_head_sha(&session.id, head_sha) {
            tracing::warn!("merge-detected session {}: record merged head sha: {e}", session.id);
        }
        // The stamp is the once-only gate for every notice below, however
        // this tick ends: it is persisted before anything is sent, and only
        // the poller whose write moved it from NULL proceeds. A failed write
        // sends nothing, so it can never become a re-notify loop.
        match self.engine.store.claim_merged_at(&session.id, now_millis()) {
            Ok(true) => {}
            Ok(false) => return true,
            Err(e) => {
                tracing::warn!(
                    "merge-detected session {} could not be stamped merged_at, \
                     skipping its notices this tick: {e}",
                    session.id,
                );
                return true;
            }
        }
        // A worker whose resolved criteria carry more than "the PR merged"
        // isn't finished just because it merged — a `Check` item is never
        // independently verifiable, so its presence always disqualifies
        // auto-reap regardless of `PrMerged`; `PrOpened`/`ReportSent`
        // alongside `PrMerged` don't disqualify it, since both are
        // trivially true once the PR merged and the worker is being
        // cleaned up. Empty criteria (an untyped/pre-existing session) is a
        // deliberate back-compat carve-out: today's unconditional behavior.
        let auto_reapable = merge_reaps_worker(session);
        if !auto_reapable && destructive_ok {
            if let Some(row) = self.engine.store.get_session(&session.id).ok().flatten() {
                self.engine.emit(Event::SessionUpdated(row, SessionFields::MERGED_AT));
            }
            if let Some(orch) = session.orchestrator_id.clone() {
                let msg = crate::lifecycle::reactions::format_merged_needs_complete_reaction(session, pr_number);
                if let Err(e) = self.engine.send_to_session(&orch, &msg).await {
                    tracing::warn!("send merged-needs-complete nudge to orchestrator {orch}: {e}");
                }
            }
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
        let keep_alive = !destructive_ok || (!auto_reap && !session.status.is_terminal());
        if !keep_alive {
            // Cleanup runs before any notice: a failure releases the claim
            // and sends nothing, so the next tick retries without spamming,
            // and a success clears the stamp again (a `Done` row never
            // carries `merged_at`; `claim_merged_at` refuses `Done` rows).
            match self.engine.cleanup_session(&session.id).await {
                Ok(()) => {
                    if let Err(e) = self.engine.store.clear_merged_at(&session.id, true) {
                        tracing::warn!("clear merged_at on reaped session {}: {e}", session.id);
                    }
                }
                Err(e) => {
                    tracing::warn!("cleanup_session {}: {e}", session.id);
                    if let Err(e) = self.engine.store.clear_merged_at(&session.id, false) {
                        tracing::warn!("release merged_at claim on {}: {e}", session.id);
                    }
                    return true;
                }
            }
        }
        self.engine.emit(Event::Notification(Notification {
            id:         format!("merged-{}", session.id),
            kind:       NotificationKind::WorkerDone,
            title:      format!("PR merged — {}", session.name),
            body:       format!("#{} merged successfully", pr_number),
            session_id: Some(session.id.clone()),
            created_at: now_millis(),
        }));
        // Code-level completion guarantee for the orchestrator — independent
        // of whether the worker's own agent ever reports back before exiting
        // — UNLESS the worker's own `ninox complete` already told this exact
        // orchestrator about this exact unit of work (its own `started_at`
        // stands in for "this PR/task"; see `Store::has_completion_since`'s
        // doc comment). Sending both would read as two contradictory
        // notifications about the same merge.
        if let Some(orch) = session.orchestrator_id.clone() {
            let already_reported = self.engine.store
                .has_completion_since(&session.id, &orch, session.started_at)
                .unwrap_or(false);
            if already_reported {
                tracing::debug!(
                    "worker {} already sent its own completion to {orch} — suppressing the done reaction",
                    session.id,
                );
            } else {
                let msg = if !destructive_ok {
                    crate::lifecycle::reactions::format_merged_unverified_reaction(session, pr_number)
                } else if keep_alive {
                    crate::lifecycle::reactions::format_worker_done_kept_alive_reaction(session, pr_number)
                } else {
                    crate::lifecycle::reactions::format_worker_done_reaction(session, pr_number)
                };
                if let Err(e) = self.engine.send_to_session(&orch, &msg).await {
                    tracing::warn!("send worker-done reaction to orchestrator {orch}: {e}");
                }
            }
        }
        if keep_alive {
            if let Some(row) = self.engine.store.get_session(&session.id).ok().flatten() {
                self.engine.emit(Event::SessionUpdated(row, SessionFields::MERGED_AT));
            }
        }
        // Remove enrichment state for this session — it's done
        {
            let mut cache = self.enrichment_cache.lock().unwrap();
            cache.remove(&session.id);
        }
        true
    }

    // ── Workspace disk ───────────────────────────────────────────────────────

    /// Frees newly finished sessions' build output, then runs one
    /// rate-limited disk-pressure pass — see `workspace::DiskJanitor`.
    async fn poll_workspace_disk(&self, config: &AppConfig) {
        let store = &self.engine.store;
        self.disk_janitor.free_newly_finished(store, &config.workspaces).await;
        let (steps, notification) = self.disk_janitor.relieve_pressure(
            store, &config.workspaces, &config.orphan_sweep, config.session_retention.retention_millis(), now_millis(),
        ).await;
        if !steps.is_empty() {
            tracing::warn!("disk pressure: {steps:?}");
        }
        if let Some(n) = notification {
            self.engine.emit(Event::Notification(n));
        }
        self.measure_idle_worker_disks(config).await;
    }

    /// Refreshes the cached worktree sizes the Idle view reads, off the
    /// async runtime since it walks the disk.
    async fn measure_idle_worker_disks(&self, config: &AppConfig) {
        let now = now_millis();
        let Ok(report) = crate::idle_workers::load_report(&self.engine.store, &config.idle_workers, now) else { return };
        let workers: Vec<_> = report.workers().cloned().collect();
        // Detached: a slow walk must not hold up the other poller ticks.
        let Some(guard) = crate::idle_workers::MeasureGuard::try_begin() else { return };
        let engine = self.engine.clone();
        let dirs = config.workspaces.build_output_dirs.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            crate::idle_workers::measure_idle_disks(&engine.store, &workers, &dirs, now)
        });
    }

    // ── Orphan sweep ─────────────────────────────────────────────────────────

    /// Scans every repo ninox has spawned into for abandoned
    /// `.claude/worktrees/*` dirs and cleans the safe ones — see
    /// `workspace::sweep_repo_orphans`. `retention` is passed in rather than
    /// loaded here, mirroring `sweep_retired_sessions`'s own pattern so this
    /// stays directly testable without touching a real `AppConfig::load()`.
    /// One repo's failure (deleted from disk, permissions) is logged and
    /// skipped rather than aborting the rest — each repo gets its own
    /// `sweep_repo_orphans` call.
    async fn run_orphan_sweep(&self, cfg: &OrphanSweepConfig, retention: &SessionRetentionConfig) {
        if !cfg.enabled {
            return;
        }
        let repos = match crate::workspace::discover_repo_roots(&self.engine.store) {
            Ok(repos) => repos,
            Err(e) => {
                tracing::warn!("orphan sweep: discover repo roots: {e}");
                return;
            }
        };
        let retention_ms = retention.retention_millis();
        for repo_root in repos {
            let report = match crate::workspace::sweep_repo_orphans(&self.engine.store, &repo_root, retention_ms).await {
                Ok(report) => report,
                Err(e) => {
                    tracing::warn!("orphan sweep {}: {e}", repo_root.display());
                    continue;
                }
            };
            for action in &report.actions {
                match &action.outcome {
                    Some(outcome) if outcome.needs_attention() => {
                        tracing::warn!("orphan sweep {}: needs attention — {}", action.path.display(), outcome.describe());
                    }
                    Some(outcome) => {
                        tracing::info!("orphan sweep {}: {}", action.path.display(), outcome.describe());
                    }
                    None => {}
                }
            }
        }
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
    /// Only terminal rows are ever eligible. A live (or `Interrupted`)
    /// worker with `merged_at` set is not retired: with `[auto_reap]` off it
    /// keeps working after its first merge, and a sweep timed by `merged_at`
    /// destroyed such a worker and its uncommitted work on 31 Oct.
    ///
    /// A row whose worktree cleanup kept (dirty, unpushed, stashed) stays,
    /// and is retried on each pass until the worktree is safe to remove.
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
            if !matches!(session.status, SessionStatus::Done | SessionStatus::Terminated) {
                continue;
            }
            // A restore candidate the user hasn't restored yet (manual
            // policy, or a declined prompt) keeps its row and worktree.
            // Merged ones don't need restoring.
            let record = fleet.get(&session.id);
            let pending_restore = record.is_some_and(|r| crate::fleet::awaits_restore(&session, r));
            if pending_restore && session.merged_at.is_none() {
                continue;
            }
            let expired = match session.terminal_at {
                Some(t) => now.saturating_sub(t) >= retention_ms,
                None    => true,
            };
            if !expired {
                continue;
            }
            let retained_at = record
                .and_then(|r| r.retained_at)
                .filter(|at| session.terminal_at.is_none_or(|t| *at >= t));
            let already_retained = retained_at.is_some();
            if retained_at.is_some_and(|at| now.saturating_sub(at) < RETAINED_RETRY_MS) {
                continue;
            }
            // Delegate to `remove_session` rather than deleting the row
            // directly — it also kills any lingering tmux session and
            // removes the worktree/artifacts, so a session that never went
            // through `handle_merge_detection`'s cleanup still gets it here.
            // `force: false`: this is an automatic sweep, never an explicit
            // user action, so it must never destroy uncommitted or unpushed
            // work — a safety-skipped worktree keeps its row too.
            let kept = match self.engine.remove_session(&session.id, false).await {
                Ok(crate::events::RemoveOutcome::Kept(reason)) => Some(reason),
                Ok(crate::events::RemoveOutcome::Removed) => None,
                Err(e) => {
                    tracing::warn!("purge retired session {}: {e}", session.id);
                    None
                }
            };
            if let (Some(reason), false) = (&kept, already_retained) {
                tracing::warn!("retention kept session {} and its worktree: {reason}", session.id);
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
            if matches!(session.status, SessionStatus::Terminated) && session.merged_at.is_none() && !already_retained {
                self.engine.emit(Event::Notification(Notification {
                    id:         format!("retired-{}", session.id),
                    kind:       NotificationKind::WorkerRetired,
                    title:      format!("Worker retired — {}", session.name),
                    body:       match &kept {
                        Some(reason) => format!("Session ended without a detected PR merge; its worktree was kept ({reason})"),
                        None => "Session cleaned up without a detected PR merge".to_string(),
                    },
                    session_id: Some(session.id.clone()),
                    created_at: now,
                }));
                if let Some(orch) = session.orchestrator_id.clone() {
                    let already_reported = self.engine.store
                        .has_completion_since(&session.id, &orch, session.started_at)
                        .unwrap_or(false);
                    if already_reported {
                        tracing::debug!(
                            "worker {} already sent its own completion to {orch} — suppressing the retired notice",
                            session.id,
                        );
                    } else {
                        let msg = crate::lifecycle::reactions::format_worker_retired_reaction(&session);
                        if let Err(e) = self.engine.send_to_session(&orch, &msg).await {
                            tracing::warn!("send worker-retired reaction to orchestrator {orch}: {e}");
                        }
                    }
                }
            }
        }
    }

    // ── Self-completion reap ────────────────────────────────────────────────

    /// Physically reaps every session `ninox complete` flagged
    /// (`self_completed_at.is_some()`) — `complete` itself never kills its
    /// own pane (it's a child of it), so this sweep is the one place that
    /// actually does. See [`Session::self_completed_at`]'s doc comment.
    async fn reap_self_completed_sessions(&self) {
        let Ok(sessions) = self.engine.store.list_sessions() else { return };
        for session in sessions {
            if session.self_completed_at.is_some() {
                self.reap_one_self_completed_session(&session.id).await;
            }
        }
    }

    /// `cleanup_session` is idempotent (tolerates an already-gone tmux
    /// session/worktree), so the only thing left to guard here is the
    /// marker clear: a session gone by the time we re-read (e.g. a human's
    /// `ninox reap --force` beat this tick to it) is a silent no-op, not an
    /// error — there's nothing left to mark handled.
    async fn reap_one_self_completed_session(&self, session_id: &str) {
        if let Err(e) = self.engine.cleanup_session(session_id).await {
            tracing::warn!("reap self-completed session {session_id}: {e}");
        }
        let Ok(Some(mut row)) = self.engine.store.get_session(session_id) else { return };
        row.self_completed_at = None;
        if self.engine.store.upsert_session(&row).is_ok() {
            self.engine.emit(Event::SessionUpdated(row, SessionFields::SELF_COMPLETED_AT));
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Cached PR state nobody has observed for a week is dropped.
const PR_STATUS_CACHE_TTL_MS: i64 = 7 * 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PrSubscription {
    /// The subscriber's own PR, as opposed to one it watches.
    own:         bool,
    /// False while the subscriber is interrupted: its cursor is kept and
    /// left unadvanced, so it hears what it missed once it is back.
    deliverable: bool,
}

/// A quiet spell earns at most this many worker nudges; the next PR event
/// starts a new spell.
const MAX_HANG_NUDGES_PER_SPELL: u32 = 2;

/// The hang check trusts cached PR state only this recent: older means the
/// sweep isn't seeing GitHub (rate-limited, failing, PR missing).
const PR_STATUS_FRESH_MS: i64 = 10 * 60 * 1000;

/// On first observation of a worker's own PR, comments and reviews this
/// recent are still delivered: they landed between the PR's creation and
/// Ninox's first sweep of it.
const OWN_PR_BASELINE_GRACE_MS: i64 = 10 * 60 * 1000;

/// A worker whose criteria are only "PR merged" (or none) is removed by
/// merge detection under `reap_on_merge`; any `Check` item makes it the
/// worker's own call to finish.
fn merge_reaps_worker(session: &Session) -> bool {
    session.success_criteria.is_empty()
        || (session.success_criteria.iter().any(|c| matches!(c, SuccessCriterion::PrMerged))
            && session.success_criteria.iter().all(|c| !matches!(c, SuccessCriterion::Check(_))))
}

/// One nudge per idle episode, at least `after_ms` apart, at most
/// [`MAX_HANG_NUDGES_PER_SPELL`] per quiet spell, and only after both the
/// worker and its PR have been quiet for `after_ms`.
fn hang_due(cursor: &crate::pr_events::PrCursor, idle_since: i64, now: i64, after_ms: i64) -> bool {
    cursor.initialized
        && !cursor.terminal
        && cursor.hang_nudges_in_spell < MAX_HANG_NUDGES_PER_SPELL
        && now - idle_since >= after_ms
        && now - cursor.quiet_since() >= after_ms
        && cursor.hang_nudged_episode != Some(idle_since)
        && now - cursor.hang_nudged_at >= after_ms
}

type HeldSnapshot = (HashMap<String, (usize, usize)>, HashMap<String, usize>);

/// A worker owes a message when nothing it sent is newer than what it last
/// received (its brief counts as received at `started_at`) and it has been
/// idle `after_ms`. Each owed message is nudged about once: a worker whose
/// sends go unrecorded (an older `ninox` binary) hears it once, not every
/// idle stretch.
fn message_stall_due(clock: &crate::store::MessageClock, started_at: i64, idle_since: i64, now: i64, after_ms: i64) -> bool {
    let last_received = clock.last_received_at.unwrap_or(0).max(started_at);
    let owes = clock.last_sent_at.is_none_or(|sent| sent < last_received);
    owes && now - idle_since >= after_ms && clock.stall_nudged_for != Some(last_received)
}

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
        let _iso = crate::test_isolation::isolate();
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

    /// A pane no host knows is not proof the agent is gone: another ninox's
    /// ptyd host or tmux server may still run it. Resuming it then would run
    /// the same conversation twice.
    #[tokio::test]
    async fn a_session_whose_harness_still_runs_is_never_resumed() {
        let _iso = crate::test_isolation::isolate();
        use crate::runtime::Liveness;
        use crate::store::Store;
        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let conversation = format!("conv-{}-elsewhere", std::process::id());
        let mut s = test_session("elsewhere", "/ws");
        s.claude_session_id = Some(conversation.clone());
        store.upsert_session(&s).unwrap();
        let mut harness = std::process::Command::new("/bin/sh").args(["-c", "sleep 60; :", &conversation]).spawn().unwrap();
        s.pid = Some(harness.id());
        store.upsert_session(&s).unwrap();
        let poller = Poller::new(Engine::new(store.clone()));
        let unknown_to_host = |_: String| -> std::pin::Pin<Box<dyn std::future::Future<Output = Liveness> + Send>> {
            Box::pin(async { Liveness::Dead })
        };

        let reconciled = poller.reconcile_sessions(vec!["elsewhere".into()], &unknown_to_host).await;
        let alive_verdict = store.get_session("elsewhere").unwrap().unwrap();
        let record = store.fleet_record("elsewhere").unwrap();
        poller.poll_pids().await;
        let after_pid_tick = store.get_session("elsewhere").unwrap().unwrap().status;
        harness.kill().unwrap();
        harness.wait().unwrap();

        assert!(!reconciled, "nothing became resumable");
        assert_eq!(alive_verdict.status, SessionStatus::Working);
        assert!(record.as_ref().is_none_or(|r| r.interrupted_at.is_none()), "no interruption recorded: {record:?}");
        assert!(poller.unreconciled.lock().unwrap().contains("elsewhere"), "re-judged on later ticks");
        assert_eq!(after_pid_tick, SessionStatus::Working, "poll_pids leaves a deferred session alone");

        poller.retry_unreconciled(&unknown_to_host).await;
        assert_eq!(
            store.get_session("elsewhere").unwrap().unwrap().status,
            SessionStatus::Working,
            "one sighting of no process may be the owner restarting it in place",
        );
        poller.retry_unreconciled(&unknown_to_host).await;
        assert_eq!(
            store.get_session("elsewhere").unwrap().unwrap().status,
            SessionStatus::Interrupted,
            "resumable once its process is gone on two consecutive checks",
        );
        assert!(poller.unreconciled.lock().unwrap().is_empty());
    }

    /// The limit's queue must not wait forever behind a pid-less session
    /// whose pane is gone; with nobody waiting, nothing is touched.
    #[tokio::test]
    async fn a_blocked_queue_frees_slots_held_by_sessions_whose_pane_is_gone() {
        let iso = crate::test_isolation::isolate();
        use crate::store::{NewQueueEntry, QueueKind, Store};
        crate::config::AppConfig::update(|c| c.limits.set_limit(None, Some(1))).unwrap();
        let store = std::sync::Arc::new(Store::open(iso.root().join("t.db")).unwrap());
        let ghost = test_session("queue-ghost-no-pane", "/ws");
        store.upsert_session(&ghost).unwrap();
        let poller = Poller::new(Engine::new(store.clone()))
            .with_queue_launcher(std::sync::Arc::new(|_| Box::pin(async {})));

        poller.free_slots_held_by_dead_panes().await;
        assert_eq!(store.get_session(&ghost.id).unwrap().unwrap().status, SessionStatus::Working, "no queue, no check");

        let waiting = test_session("queue-waiter", "/ws");
        store.admit_session(NewQueueEntry { kind: QueueKind::Spawn, session: &waiting, payload: "{}" }, Some(1), false, 1).unwrap();
        poller.free_slots_held_by_dead_panes().await;
        assert_eq!(store.get_session(&ghost.id).unwrap().unwrap().status, SessionStatus::Working, "one sighting is not enough");
        // Backdate the first sighting past the grace period.
        poller.dead_pane_seen.lock().unwrap().insert(ghost.id.clone(), now_millis() - DEAD_PANE_GRACE_MS - 1);
        poller.free_slots_held_by_dead_panes().await;
        assert!(store.get_session(&ghost.id).unwrap().unwrap().status.is_terminal());
        assert_eq!(store.running_session_count(None).unwrap(), 0);
    }

    #[tokio::test]
    async fn poll_pids_leaves_interrupted_sessions_alone() {
        let _iso = crate::test_isolation::isolate();
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

    /// Resolved decision #3 of the remote-sessions spec: a remote session's
    /// pid is meaningless to a local `kill(pid, 0)`, so `poll_pids` must
    /// skip that check entirely rather than guess liveness from it.
    #[tokio::test]
    async fn poll_pids_skips_the_local_pid_check_for_a_remote_session() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut s = test_session("remote-1", "/ws");
        s.machine_id = Some("m1".into());
        s.pid = Some(999_999); // a pid that is almost certainly dead (and not this machine's to judge)
        store.upsert_session(&s).unwrap();
        let engine = Engine::new(store.clone());
        let poller = Poller::new(engine);

        poller.poll_pids().await;

        let after = store.get_session("remote-1").unwrap().unwrap();
        assert_eq!(
            after.status, SessionStatus::Working,
            "poll_pids must not terminate a remote session from a local pid check",
        );
    }

    fn remote_test_session(id: &str, status: SessionStatus) -> crate::types::Session {
        let mut s = test_session(id, "/remote/ws");
        s.status = status;
        s.pid = Some(4242); // meaningless locally, but must survive the round trip
        s
    }

    /// A session the remote machine reports that the local store has never
    /// seen before must be inserted (not silently dropped) and tagged with
    /// `machine_id`, and announced as newly spawned with the full row.
    #[tokio::test]
    async fn reconcile_remote_sessions_inserts_a_previously_unknown_session() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let engine = Engine::new(store.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        let remote_row = remote_test_session("remote-new", SessionStatus::Working);
        poller.reconcile_remote_sessions("m1", vec![remote_row]);

        let after = store.get_session("remote-new").unwrap().expect("must be inserted");
        assert_eq!(after.machine_id.as_deref(), Some("m1"));
        assert_eq!(after.status, SessionStatus::Working);

        let events = drain_events(&mut rx);
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], Event::SessionSpawned(s) if s.id == "remote-new" && s.machine_id.as_deref() == Some("m1")),
            "unexpected event: {:?}", events[0],
        );
    }

    /// A terminal session the remote still lists (its own retention sweep
    /// hasn't purged it yet) but the local store has *never tracked* must
    /// not be resurrected — most often this is a session this store already
    /// purged, and re-inserting it would only have it swept away again next
    /// tick, flickering `SessionSpawned` forever.
    #[tokio::test]
    async fn reconcile_remote_sessions_does_not_resurrect_an_unknown_terminal_session() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let engine = Engine::new(store.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        let remote_row = remote_test_session("remote-already-gone", SessionStatus::Terminated);
        poller.reconcile_remote_sessions("m1", vec![remote_row]);

        assert!(store.get_session("remote-already-gone").unwrap().is_none(), "must not be inserted");
        assert!(drain_events(&mut rx).is_empty(), "must not announce a session that was never adopted");
    }

    /// Unlike `Done`/`Terminated`, an `Interrupted` remote session must be
    /// adopted even when this store has never tracked it before — nothing
    /// ever purges `Interrupted` rows (see `sweep_retired_sessions`), and
    /// it's the actionable, resumable case this reconciliation exists to
    /// surface: without this, the user could never discover or `--resume`
    /// it from this machine.
    #[tokio::test]
    async fn reconcile_remote_sessions_adopts_a_previously_unknown_interrupted_session() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let poller = Poller::new(Engine::new(store.clone()));

        let remote_row = remote_test_session("remote-interrupted", SessionStatus::Interrupted);
        poller.reconcile_remote_sessions("m1", vec![remote_row]);

        let after = store.get_session("remote-interrupted").unwrap().expect("Interrupted must be adopted, not skipped like Done/Terminated");
        assert_eq!(after.status, SessionStatus::Interrupted);
        assert_eq!(after.machine_id.as_deref(), Some("m1"));
    }

    /// A non-terminal remote status (`Working`, in this case a stale view
    /// behind the local orchestrator's own GitHub polling) must never
    /// overwrite local status — only the remote's own pid-death verdict
    /// (`Terminated`/`Interrupted`) is ever adopted. Without this, a session
    /// the local orchestrator already advanced to `PrOpen` would flicker back
    /// to `Working` on literally the next 5s tick.
    #[tokio::test]
    async fn reconcile_remote_sessions_never_overwrites_local_status_with_a_non_terminal_remote_view() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut local = test_session("remote-pr-open", "/remote/ws");
        local.machine_id = Some("m1".into());
        local.status = SessionStatus::PrOpen;
        local.pr_number = Some(42);
        store.upsert_session(&local).unwrap();
        let engine = Engine::new(store.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        // The remote's own view hasn't caught up (or never will, if it has
        // no GitHub token configured there) — it still thinks this is Working.
        let remote_row = remote_test_session("remote-pr-open", SessionStatus::Working);
        poller.reconcile_remote_sessions("m1", vec![remote_row]);

        let after = store.get_session("remote-pr-open").unwrap().unwrap();
        assert_eq!(after.status, SessionStatus::PrOpen, "local PR-lifecycle progress must never be reverted by a stale remote view");
        assert!(drain_events(&mut rx).is_empty(), "nothing changed, so nothing should be emitted");
    }

    /// A `Done` row (merge fully handled by the local orchestrator) must
    /// never be relabeled `Terminated` by the remote's own pid-liveness
    /// verdict — `Done` is a *merge-handling* outcome, a different subsystem
    /// decision the remote-sessions spec explicitly keeps out of scope
    /// (resolved decision #10), not something the remote's poller determines.
    #[tokio::test]
    async fn reconcile_remote_sessions_never_relabels_a_done_row_as_terminated() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut local = test_session("remote-done", "/remote/ws");
        local.machine_id = Some("m1".into());
        local.status = SessionStatus::Done;
        store.upsert_session(&local).unwrap();
        let engine = Engine::new(store.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        let remote_row = remote_test_session("remote-done", SessionStatus::Terminated);
        poller.reconcile_remote_sessions("m1", vec![remote_row]);

        let after = store.get_session("remote-done").unwrap().unwrap();
        assert_eq!(after.status, SessionStatus::Done, "a merge-handled Done row must never be relabeled Terminated");
        assert!(drain_events(&mut rx).is_empty());
    }

    /// Symmetric to the `Done` case: a resumable `Interrupted` row must not
    /// be flattened to `Terminated` by a later remote view either — only a
    /// user-triggered Resume clears `Interrupted`.
    #[tokio::test]
    async fn reconcile_remote_sessions_never_flattens_interrupted_to_terminated() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut local = test_session("remote-interrupted-2", "/remote/ws");
        local.machine_id = Some("m1".into());
        local.status = SessionStatus::Interrupted;
        store.upsert_session(&local).unwrap();
        let poller = Poller::new(Engine::new(store.clone()));

        let remote_row = remote_test_session("remote-interrupted-2", SessionStatus::Terminated);
        poller.reconcile_remote_sessions("m1", vec![remote_row]);

        let after = store.get_session("remote-interrupted-2").unwrap().unwrap();
        assert_eq!(after.status, SessionStatus::Interrupted, "only a user-triggered Resume may clear Interrupted");
    }

    /// A session already known locally gets its `status`/`terminal_at`
    /// merged from the remote's fresher view — e.g. the remote's own
    /// `poll_pids` already marked it `Terminated` via its local
    /// `kill(pid, 0)` check, which must flow down here as the real liveness
    /// signal (resolved decision #3) — while every other field stays under
    /// the local orchestrator's own ownership (resolved decision #10): this
    /// must never be a wholesale replace.
    #[tokio::test]
    async fn reconcile_remote_sessions_merges_only_status_and_terminal_at_into_a_known_session() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut local = test_session("remote-known", "/remote/ws");
        local.machine_id = Some("m1".into());
        local.status = SessionStatus::Working;
        // Locally-authoritative fields the orchestrator's own GitHub/usage
        // polling computed for this session — the remote's view of these
        // disagrees (it has none), and must not win.
        local.gate_status = Some(GateStatus { ci: GateCheck::Passing, review: GateCheck::Passing, mergeable: GateCheck::Passing, since: 1 });
        local.cost_usd = 12.5;
        local.pr_number = Some(99);
        store.upsert_session(&local).unwrap();
        let engine = Engine::new(store.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        // The remote's own row has none of those fields set.
        let remote_row = remote_test_session("remote-known", SessionStatus::Terminated);
        poller.reconcile_remote_sessions("m1", vec![remote_row]);

        let after = store.get_session("remote-known").unwrap().unwrap();
        assert_eq!(after.status, SessionStatus::Terminated, "the remote's own liveness check must flow through");
        assert!(after.terminal_at.is_some(), "a terminal row must never be left without a terminal_at stamp");
        assert_eq!(after.machine_id.as_deref(), Some("m1"));
        assert!(after.gate_status.is_some(), "gate_status is orchestrator-owned and must survive reconciliation");
        assert_eq!(after.cost_usd, 12.5, "cost_usd is orchestrator-owned and must survive reconciliation");
        assert_eq!(after.pr_number, Some(99), "pr_number is orchestrator-owned and must survive reconciliation");

        let events = drain_events(&mut rx);
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], Event::SessionUpdated(s, fields) if s.id == "remote-known"
                && s.status == SessionStatus::Terminated
                && *fields == (SessionFields::STATUS | SessionFields::TERMINAL_AT)),
            "unexpected event: {:?}", events[0],
        );
    }

    /// When the remote's own row carries a `terminal_at` (its poller stamped
    /// the real moment the pid died), that stamp — not "now" — must be the
    /// one copied down, so the retention sweep's grace window is measured
    /// from the actual termination time, not from whenever this tick
    /// happened to run.
    #[tokio::test]
    async fn reconcile_remote_sessions_prefers_the_remotes_own_terminal_at_stamp() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut local = test_session("remote-stamped", "/remote/ws");
        local.machine_id = Some("m1".into());
        local.status = SessionStatus::Working;
        store.upsert_session(&local).unwrap();
        let poller = Poller::new(Engine::new(store.clone()));

        let mut remote_row = remote_test_session("remote-stamped", SessionStatus::Terminated);
        remote_row.terminal_at = Some(12_345);
        poller.reconcile_remote_sessions("m1", vec![remote_row]);

        let after = store.get_session("remote-stamped").unwrap().unwrap();
        assert_eq!(after.terminal_at, Some(12_345));
    }

    /// `update_live_session_row`'s terminal guard must still hold for remote
    /// reconciliation: a row some other local writer already finalized as
    /// terminal mid-tick is never resurrected by a stale remote snapshot —
    /// and the guard's own `terminal_at` gap (it reverts `status` alone) must
    /// not leave the row with a terminal status but no stamp at all.
    #[tokio::test]
    async fn reconcile_remote_sessions_never_resurrects_a_row_gone_terminal_meanwhile() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut local = test_session("remote-raced", "/remote/ws");
        local.machine_id = Some("m1".into());
        local.status = SessionStatus::Terminated;
        store.upsert_session(&local).unwrap();
        let poller = Poller::new(Engine::new(store.clone()));

        // The remote fetch was already in flight when something local (e.g.
        // `ninox reap`) finalized the row as Terminated (with no terminal_at
        // of its own — a direct user action's deliberate "purge on sight"
        // marker) — the remote's (now-stale) Working snapshot must not
        // overwrite the status, and `terminal_at` must stay exactly as that
        // other writer left it: an already-terminal row is never touched by
        // this reconciliation at all, not even to "fix" a missing stamp.
        let remote_row = remote_test_session("remote-raced", SessionStatus::Working);
        poller.reconcile_remote_sessions("m1", vec![remote_row]);

        let after = store.get_session("remote-raced").unwrap().unwrap();
        assert_eq!(after.status, SessionStatus::Terminated);
        assert_eq!(after.terminal_at, None, "an already-terminal row must never be touched, not even to backfill terminal_at");
    }

    /// `poll_remote_sessions` reconciles every enabled, connected machine
    /// profile in one tick, each tagged with its own `machine_id` — not just
    /// the first one configured.
    #[tokio::test]
    async fn reconcile_remote_sessions_tags_each_machines_rows_independently() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let poller = Poller::new(Engine::new(store.clone()));

        poller.reconcile_remote_sessions("m1", vec![remote_test_session("from-m1", SessionStatus::Working)]);
        poller.reconcile_remote_sessions("m2", vec![remote_test_session("from-m2", SessionStatus::Working)]);

        assert_eq!(store.get_session("from-m1").unwrap().unwrap().machine_id.as_deref(), Some("m1"));
        assert_eq!(store.get_session("from-m2").unwrap().unwrap().machine_id.as_deref(), Some("m2"));
    }

    /// Ninox session ids are plain name slugs with no per-machine scoping, so
    /// a locally-spawned session can share an id with a session a remote
    /// machine happens to report (a ticket-named worker spawned in both
    /// places, or a machine profile that points back at this same host).
    /// Reconciliation must never let that remote view overwrite a row this
    /// store already knows is a *local* one — a live local worker must not
    /// be flipped `Terminated`, and its untagged `machine_id` must survive.
    #[tokio::test]
    async fn reconcile_remote_sessions_does_not_corrupt_a_local_session_on_id_collision() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut local = test_session("shared-name", "/local/ws");
        local.machine_id = None;
        local.status = SessionStatus::Working;
        store.upsert_session(&local).unwrap();
        let engine = Engine::new(store.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        // A remote machine happens to report a *different* session that
        // just shares this exact id, already terminated.
        let remote_row = remote_test_session("shared-name", SessionStatus::Terminated);
        poller.reconcile_remote_sessions("m1", vec![remote_row]);

        let after = store.get_session("shared-name").unwrap().unwrap();
        assert_eq!(after.status, SessionStatus::Working, "a local session must never be terminated by an unrelated remote one of the same id");
        assert_eq!(after.machine_id, None, "the local session must not be retagged as remote");
        assert!(drain_events(&mut rx).is_empty(), "no event should fire for a skipped id collision");
    }

    /// The same id-collision protection must hold across two *different*
    /// remote machines reporting a session under the same id — whichever
    /// reconciles second must not clobber the first's row.
    #[tokio::test]
    async fn reconcile_remote_sessions_does_not_cross_contaminate_two_machines_with_the_same_id() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let poller = Poller::new(Engine::new(store.clone()));

        poller.reconcile_remote_sessions("m1", vec![remote_test_session("shared-remote-name", SessionStatus::Working)]);
        // m2 reports a session with the exact same id, already terminated.
        poller.reconcile_remote_sessions("m2", vec![remote_test_session("shared-remote-name", SessionStatus::Terminated)]);

        let after = store.get_session("shared-remote-name").unwrap().unwrap();
        assert_eq!(after.machine_id.as_deref(), Some("m1"), "ownership must not transfer to m2");
        assert_eq!(after.status, SessionStatus::Working, "m2's view of a same-id session it doesn't own must not apply");
    }

    /// `poll_remote_sessions` must do nothing when `[remote_machines]` is
    /// disabled or the one configured profile is disabled — no SSH calls, no
    /// store writes.
    #[tokio::test]
    async fn poll_remote_sessions_skips_disabled_config_and_disabled_profiles() {
        use crate::{config::{AppConfig, MachineProfile, RemoteMachinesConfig}, store::Store};

        let config_path = tempfile::tempdir().unwrap().keep().join("ninox-config.toml");
        let _iso = crate::test_isolation::isolate_with_config(&config_path);

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let poller = Poller::new(Engine::new(store.clone()));

        // `remote_machines.enabled = false` entirely (default): must be a no-op.
        poller.poll_remote_sessions().await;
        assert!(store.list_sessions().unwrap().is_empty());

        // Enabled overall, but the one profile disabled: still a no-op.
        AppConfig::update(|cfg| {
            cfg.remote_machines = RemoteMachinesConfig {
                enabled: true,
                machines: vec![MachineProfile {
                    id: "m1".into(),
                    label: "test".into(),
                    ssh_target: "nonexistent.invalid".into(),
                    remote_session: "default".into(),
                    enabled: false,
                }],
            };
        })
        .unwrap();
        poller.poll_remote_sessions().await;
        assert!(store.list_sessions().unwrap().is_empty());
    }

    /// An enabled profile whose SSH `ControlMaster` isn't actually alive
    /// must be skipped for the tick, not reconnected — see `crate::remote`'s
    /// module docs on why reconnecting outside an interactive flow is unsafe.
    #[tokio::test]
    async fn poll_remote_sessions_skips_a_disconnected_profile_without_reconnecting() {
        use crate::{config::{AppConfig, MachineProfile, RemoteMachinesConfig}, store::Store};

        let config_path = tempfile::tempdir().unwrap().keep().join("ninox-config.toml");
        let _iso = crate::test_isolation::isolate_with_config(&config_path);

        AppConfig::update(|cfg| {
            cfg.remote_machines = RemoteMachinesConfig {
                enabled: true,
                machines: vec![MachineProfile {
                    id: "never-connected".into(),
                    label: "test".into(),
                    ssh_target: "nonexistent.invalid".into(),
                    remote_session: "default".into(),
                    enabled: true,
                }],
            };
        })
        .unwrap();

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let poller = Poller::new(Engine::new(store.clone()));

        // No `ControlMaster` was ever established for "never-connected", so
        // this must return quickly (no attempt to reach the tunnel) and
        // leave the store untouched.
        tokio::time::timeout(Duration::from_secs(5), poller.poll_remote_sessions())
            .await
            .expect("must not hang trying to reconnect");
        assert!(store.list_sessions().unwrap().is_empty());
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
        let _iso = crate::test_isolation::isolate();
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
        let _iso = crate::test_isolation::isolate();
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
        let _iso = crate::test_isolation::isolate();
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

    /// `deliver_work_requests`'s own send must be tagged `RequestForWork`,
    /// not the `System` every other poller reaction uses — see Task 13's
    /// brief.
    #[tokio::test]
    async fn work_request_delivery_is_tagged_request_for_work() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let orch_id = format!(
            "orch-workreq-test-{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
        );
        let orch_ws = tempfile::tempdir().unwrap().keep();
        crate::tmux::create_session(&orch_id, orch_ws.to_str().unwrap(), "sleep 30", &[]).await.unwrap();

        let sessions_dir = tempfile::tempdir().unwrap();
        hooks::append_work_request(sessions_dir.path(), "w1", "Migrate the config loader").unwrap();

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut s = test_session("w1", "/ws");
        s.orchestrator_id = Some(orch_id.clone());
        store.upsert_session(&s).unwrap();

        let poller = Poller::new(Engine::new(store.clone()));
        poller.deliver_work_requests(&s, sessions_dir.path()).await;

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let screen = crate::tmux::capture_screen(&orch_id, 0, false).await.unwrap_or_default();
        assert!(screen.contains("[request_for_work]"), "got: {screen:?}");

        crate::tmux::kill_session(&orch_id).await.ok();
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

    /// Fake OpenCode agent that completes the ACP handshake but never
    /// answers a `session/prompt` — `send_keys_detached` returns as soon as
    /// the prompt is accepted (see `events::InProcessAcpRouter::send`), so
    /// this is enough to prove delivery succeeded without waiting out a
    /// full (possibly very long) agent turn.
    fn fake_acp_agent_that_never_replies_to_prompts() -> Vec<String> {
        let script = r#"
read -r _init
printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{},"authMethods":[]}}\n'
read -r _new
printf '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"fake-session-1"}}\n'
while read -r _line; do
  :
done
"#;
        vec!["sh".to_string(), "-c".to_string(), script.to_string()]
    }

    fn mk_user_request_row(
        id: &str, requester: &str, status: crate::types::UserRequestStatus,
        answer: Option<&str>,
    ) -> crate::store::UserRequestRow {
        crate::store::UserRequestRow {
            id: id.into(), requester_session_id: requester.into(), prompt: "deploy now?".into(),
            mode: crate::types::UserRequestMode::Async, status,
            answer: answer.map(str::to_string), answered_at: Some(10),
            answer_delivered_at: None, created_at: 5,
        }
    }

    /// An answered async `ninox user-request` must reach the requester
    /// session directly (not via any orchestrator) and only be marked
    /// delivered once the send actually succeeds.
    #[tokio::test]
    async fn deliver_user_request_answers_sends_and_marks_delivered() {
        let _iso = crate::test_isolation::isolate();
        use crate::{runtime::SessionBackend, store::Store};

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut session = test_session("s1", "/ws");
        session.agent_type = "opencode".into();
        store.upsert_session(&session).unwrap();
        store.insert_user_request(&mk_user_request_row(
            "u1", "s1", crate::types::UserRequestStatus::Answered, Some("yes, go ahead"),
        )).unwrap();

        let acp = std::sync::Arc::new(crate::runtime::AcpBackend::with_agent_command(
            fake_acp_agent_that_never_replies_to_prompts(),
        ));
        let workspace = tempfile::tempdir().unwrap().keep();
        acp.create_session("s1", workspace.to_str().unwrap(), "", &[]).await.unwrap();
        let engine = Engine::new_with_acp(store.clone(), acp);
        let poller = Poller::new(engine);

        poller.deliver_user_request_answers(&session).await;

        let row = store.get_user_request("u1").unwrap().unwrap();
        assert!(row.answer_delivered_at.is_some(), "a successfully sent answer must be marked delivered");
    }

    /// A sync `ask` that timed out before the human answered has no blocking
    /// call left to pick the answer up, so once the grace period passes the
    /// poller must deliver it like an async one — but never a sync answer
    /// fresh enough that the asker's own `wait` may still be about to read it.
    #[tokio::test]
    async fn deliver_user_request_answers_redelivers_only_sync_answers_past_the_grace_period() {
        let _iso = crate::test_isolation::isolate();
        use crate::{runtime::SessionBackend, store::Store};

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut session = test_session("s1", "/ws");
        session.agent_type = "opencode".into();
        store.upsert_session(&session).unwrap();
        let sync_row = |id: &str, answered_at: i64| crate::store::UserRequestRow {
            mode: crate::types::UserRequestMode::Sync,
            answered_at: Some(answered_at),
            ..mk_user_request_row(id, "s1", crate::types::UserRequestStatus::Answered, Some("go"))
        };
        store.insert_user_request(&sync_row("timed-out", 10)).unwrap();

        let acp = std::sync::Arc::new(crate::runtime::AcpBackend::with_agent_command(
            fake_acp_agent_that_never_replies_to_prompts(),
        ));
        let workspace = tempfile::tempdir().unwrap().keep();
        acp.create_session("s1", workspace.to_str().unwrap(), "", &[]).await.unwrap();
        let poller = Poller::new(Engine::new_with_acp(store.clone(), acp));

        // Inserted after the ACP agent spawn, which can outlast the 15s grace
        // period on a loaded machine and age this row into redelivery.
        store.insert_user_request(&sync_row("just-answered", now_millis())).unwrap();
        poller.deliver_user_request_answers(&session).await;

        assert!(store.get_user_request("timed-out").unwrap().unwrap().answer_delivered_at.is_some());
        assert!(
            store.get_user_request("just-answered").unwrap().unwrap().answer_delivered_at.is_none(),
            "a fresh sync answer is left for the asker's own blocking `wait` to stamp",
        );
    }

    /// A requester session that died before its answer could be delivered
    /// (no live tmux pane, no ACP connection) must not panic or abort the
    /// poll tick — the failure is logged and the row stays undelivered for
    /// a later retry.
    #[tokio::test]
    async fn deliver_user_request_answers_does_not_panic_for_a_dead_requester() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut session = test_session("s1", "/ws");
        session.status = SessionStatus::Terminated;
        store.upsert_session(&session).unwrap();
        store.insert_user_request(&mk_user_request_row(
            "u1", "s1", crate::types::UserRequestStatus::Answered, Some("yes, go ahead"),
        )).unwrap();

        let poller = Poller::new(Engine::new(store.clone()));

        // No real tmux session named "s1" exists, so delivery fails; this
        // must return normally rather than panicking or propagating.
        poller.deliver_user_request_answers(&session).await;

        let row = store.get_user_request("u1").unwrap().unwrap();
        assert!(row.answer_delivered_at.is_none(), "a failed send must leave the row undelivered for retry");
    }

    /// The ledger row for an extra PR is best-effort at notification time (a
    /// busy store must not kill the alert) — but it must self-heal on later
    /// ticks rather than be lost forever, and healing must not re-notify.
    #[tokio::test]
    async fn extra_pr_ledger_row_backfills_after_notification_without_renotifying() {
        let _iso = crate::test_isolation::isolate();
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
        let _iso = crate::test_isolation::isolate();
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
        let _iso = crate::test_isolation::isolate();
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
        let _iso = crate::test_isolation::isolate();
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
        let _iso = crate::test_isolation::isolate();
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
        let _iso = crate::test_isolation::isolate();
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
                title: "t".into(), number: 99, head_sha: "abc".into(), head_ref: "worker-branch".into(), base_ref: String::new(),
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
        let _iso = crate::test_isolation::isolate();
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
        let _iso = crate::test_isolation::isolate();
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

    /// A session deleted between the tick-start snapshot and the
    /// status/gate write must stay deleted — falling back to the snapshot
    /// there would resurrect the row.
    #[tokio::test]
    async fn poll_github_status_write_does_not_resurrect_a_row_deleted_mid_poll() {
        let _iso = crate::test_isolation::isolate();
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
        let _iso = crate::test_isolation::isolate();
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
        let _iso = crate::test_isolation::isolate();
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
        let _iso = crate::test_isolation::isolate();
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
        let _iso = crate::test_isolation::isolate();
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
        let _iso = crate::test_isolation::isolate();
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
        let _iso = crate::test_isolation::isolate();
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
        let _iso = crate::test_isolation::isolate();
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
        let _iso = crate::test_isolation::isolate();
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
        let _iso = crate::test_isolation::isolate();
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
        let _iso = crate::test_isolation::isolate();
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
            meta:           Default::default(),
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
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut s = test_session("w1", "/ws");
        s.status          = SessionStatus::Working;
        s.orchestrator_id = Some("orch1".into());
        s.repo            = "Owner/repo".into();
        s.pr_number       = Some(7);
        store.upsert_session(&s).unwrap();
        store.set_session_branch("w1", "w1-branch").unwrap();

        let batch = std::sync::Arc::new(FakeBatchApi::default());
        {
            let mut merged = open_snapshot(7);
            merged.status.merged = true;
            merged.status.state  = "closed".into();
            merged.status.head_ref = "w1-branch".into();
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
        let _iso = crate::test_isolation::isolate();
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
        let _iso = crate::test_isolation::isolate();
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
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&open_pr_session("s1", "/ws", 50)).unwrap();

        let batch = std::sync::Arc::new(FakeBatchApi::default());
        {
            let mut result = batch.result.lock().unwrap();
            result.prs.insert(pr_key("Owner/repo", 50), open_snapshot(50));
            result.rate_limit = crate::github_graphql::RateLimitInfo {
                cost: 1, remaining: 50, reset_at: now_millis() + 60_000, ..Default::default()
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
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&open_pr_session("s1", "/ws", 50)).unwrap();

        let batch = std::sync::Arc::new(FakeBatchApi::default());
        {
            let mut result = batch.result.lock().unwrap();
            result.prs.insert(pr_key("Owner/repo", 50), open_snapshot(50));
            result.rate_limit = crate::github_graphql::RateLimitInfo {
                cost: 1, remaining: 4000, reset_at: now_millis() + 60_000, ..Default::default()
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

    fn reading(remaining: u64) -> crate::github_graphql::RateLimitInfo {
        crate::github_graphql::RateLimitInfo {
            cost: 40, remaining, limit: 5000, reset_at: now_millis() + 600_000,
        }
    }

    #[tokio::test]
    async fn draining_budget_stretches_the_interval_then_recovers_and_is_persisted() {
        let _iso = crate::test_isolation::isolate();
        use crate::github_budget::BudgetLevel;
        let store = std::sync::Arc::new(crate::store::Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let poller = Poller::new(batch_engine(store.clone(), std::sync::Arc::new(FakeBatchApi::default())));
        let factor = || *poller.github_interval_factor.lock().unwrap();

        poller.note_rate_limit(&reading(4000));
        assert_eq!(factor(), 1.0);
        assert_eq!(store.get_github_budget().unwrap().unwrap().level, BudgetLevel::Normal);

        poller.note_rate_limit(&reading(1000));
        assert!(factor() > 1.0, "20% left must slow polling");
        assert_eq!(*poller.rate_limit_pause_until.lock().unwrap(), 0, "backing off is not a pause");
        let stored = store.get_github_budget().unwrap().unwrap();
        assert_eq!((stored.level, stored.remaining, stored.limit), (BudgetLevel::BackingOff, 1000, 5000));
        assert!(stored.interval_secs > 60);

        poller.note_rate_limit(&reading(450));
        assert!(*poller.rate_limit_pause_until.lock().unwrap() > now_millis(), "under 10% pauses until reset");
        assert_eq!(store.get_github_budget().unwrap().unwrap().level, BudgetLevel::Paused);

        poller.note_rate_limit(&reading(4800));
        assert_eq!(factor(), 1.0);
        assert_eq!(store.get_github_budget().unwrap().unwrap().level, BudgetLevel::Normal);
    }

    #[tokio::test]
    async fn gui_poller_defers_github_to_a_reachable_daemon_and_resumes_when_it_is_gone() {
        let _iso = crate::test_isolation::isolate();
        let store = std::sync::Arc::new(crate::store::Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let engine = batch_engine(store, std::sync::Arc::new(FakeBatchApi::default()));

        let standalone = Poller::new(engine.clone());
        assert!(!standalone.github_deferred_to_daemon().await, "no daemon configured: poll GitHub");

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let gui = Poller::new(engine).with_external_github_daemon(port);
        assert!(gui.github_deferred_to_daemon().await, "daemon answering: GUI must not poll GitHub");

        drop(listener);
        *gui.daemon_probe.lock().unwrap() = (0, true); // age the cached probe past its 15s window
        assert!(!gui.github_deferred_to_daemon().await, "daemon gone: GUI polls GitHub itself");
    }

    fn watch_for(repo: &str, number: u64, opener: &str) -> crate::types::PrWatch {
        crate::types::PrWatch { opener_session_id: Some(opener.into()), ..watch(repo, number) }
    }

    #[tokio::test]
    async fn watches_of_ended_openers_are_dropped_unless_something_live_still_needs_them() {
        let _iso = crate::test_isolation::isolate();
        let store = std::sync::Arc::new(crate::store::Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut live = test_session("live", "/ws");
        live.status = SessionStatus::Working;
        store.upsert_session(&live).unwrap();
        let mut done = test_session("done", "/ws");
        done.status = SessionStatus::Done;
        store.upsert_session(&done).unwrap();
        // A live session's own PR, also watched by an ended opener.
        let mut owner = test_session("owner", "/ws");
        owner.status = SessionStatus::PrOpen;
        owner.repo = "o/r".into();
        owner.pr_number = Some(4);
        store.upsert_session(&owner).unwrap();

        for w in [
            watch_for("o/r", 1, "done"),    // ended, nobody else: dropped
            watch_for("o/r", 2, "ghost"),   // opener purged from the store: dropped
            watch_for("o/r", 3, "done"),    // ended, but a live session also watches it: kept
            watch_for("o/r", 3, "live"),
            watch_for("o/r", 4, "done"),    // ended, but a live session's own PR: kept
            watch_for("o/r", 5, "live"),    // live opener: kept
            watch("o/r", 6),                // no opener: kept
        ] {
            store.upsert_pr_watch(&w).unwrap();
        }

        Poller::new(batch_engine(store.clone(), std::sync::Arc::new(FakeBatchApi::default()))).drop_ended_watches();

        let left: Vec<(u64, Option<String>)> = store.list_pr_watches().unwrap()
            .into_iter().map(|w| (w.pr_number, w.opener_session_id)).collect();
        assert_eq!(
            left,
            vec![
                (3, Some("done".into())), (3, Some("live".into())), (4, Some("done".into())),
                (5, Some("live".into())), (6, None),
            ],
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
        let _iso = crate::test_isolation::isolate();
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
        let _iso = crate::test_isolation::isolate();
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

    #[tokio::test]
    async fn held_message_counts_are_emitted_only_when_they_change() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::{HeldMessage, Store};
        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let engine = github_engine(store.clone(), std::sync::Arc::new(FakeGithub::default()));
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);
        let held_events = |rx: &mut tokio::sync::broadcast::Receiver<Event>| {
            drain_events(rx).into_iter().filter(|e| matches!(e, Event::HeldMessages { .. })).count()
        };
        poller.emit_held_messages_if_changed();
        assert_eq!(held_events(&mut rx), 1, "the first look always reports");
        poller.emit_held_messages_if_changed();
        assert_eq!(held_events(&mut rx), 0, "unchanged counts stay quiet");
        let msg = HeldMessage { tagged: "t".into(), attributed_from: Some("w1".into()), defanged: "d".into(), kind: crate::types::MessageKind::Status };
        store.hold_unreachable("orch", &msg, "no pane", 1, 2).unwrap();
        poller.emit_held_messages_if_changed();
        let events = drain_events(&mut rx);
        let Some(Event::HeldMessages { held, by_sender }) = events.iter().find(|e| matches!(e, Event::HeldMessages { .. })) else {
            panic!("a change must be reported: {events:?}");
        };
        assert_eq!(held["orch"], (0, 1));
        assert_eq!(by_sender["w1"], 1);
    }

    #[test]
    fn message_stall_due_once_per_owed_message() {
        use crate::store::MessageClock;
        const MIN: i64 = 60_000;
        let now = 1_000 * MIN;
        let idle = now - 40 * MIN;
        let fresh = MessageClock::default();
        assert!(message_stall_due(&fresh, 5, idle, now, 30 * MIN), "never sent since its brief");
        assert!(!message_stall_due(&fresh, 5, now - 10 * MIN, now, 30 * MIN), "not idle long enough");
        let replied = MessageClock { last_sent_at: Some(20), last_received_at: Some(10), ..Default::default() };
        assert!(!message_stall_due(&replied, 5, idle, now, 30 * MIN), "it answered the last message");
        let owes = MessageClock { last_sent_at: Some(20), last_received_at: Some(30), ..Default::default() };
        assert!(message_stall_due(&owes, 5, idle, now, 30 * MIN));
        let nudged = MessageClock { stall_nudged_for: Some(30), ..owes };
        assert!(!message_stall_due(&nudged, 5, idle, now, 30 * MIN), "already nudged about that message");
    }

    #[tokio::test]
    async fn a_worker_idle_without_sending_its_read_back_is_nudged_once_and_its_orchestrator_told() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;
        const MIN: i64 = 60_000;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let now = now_millis();
        store.upsert_session(&test_session("orch", "/ws-o")).unwrap();
        let mut w = test_session("w1", "/ws-w");
        w.orchestrator_id = Some("orch".into());
        w.started_at = now - 60 * MIN;
        w.activity = crate::types::ActivityState::Idle;
        w.activity_since = Some(now - 40 * MIN);
        store.upsert_session(&w).unwrap();
        let mut busy = test_session("w2", "/ws-w2");
        busy.orchestrator_id = Some("orch".into());
        busy.activity = crate::types::ActivityState::Working;
        busy.activity_since = Some(now - 40 * MIN);
        store.upsert_session(&busy).unwrap();

        store.upsert_orchestrator(&crate::types::Orchestrator { id: "sub".into(), name: "sub".into(), created_at: 0 }).unwrap();
        let mut sub = test_session("sub", "/ws-sub");
        sub.orchestrator_id = Some("orch".into());
        sub.started_at = now - 60 * MIN;
        sub.activity = crate::types::ActivityState::Idle;
        sub.activity_since = Some(now - 40 * MIN);
        store.upsert_session(&sub).unwrap();

        let engine = github_engine(store.clone(), std::sync::Arc::new(FakeGithub::default()));
        let poller = Poller::new(engine);
        let cfg = crate::config::MessageStallConfig::default();
        // No pane exists in a test, so each nudge is recorded as a failed
        // attempt: enough to see who was nudged with what.
        let sent = |id: &str| -> Vec<String> {
            store.message_events_to(id, 0).unwrap().into_iter().map(|e| e.text).collect()
        };

        poller.check_message_stalls(&cfg).await;
        assert_eq!(sent("w1").len(), 1, "the idle worker is nudged");
        assert!(sent("w1")[0].contains("ninox send orch"), "{:?}", sent("w1"));
        assert_eq!(sent("orch").len(), 1, "its orchestrator is told");
        assert!(sent("w2").is_empty(), "a working worker is left alone");
        assert!(sent("sub").is_empty(), "a sub-orchestrator idles waiting on its workers, legitimately");

        *poller.last_stall_check.lock().unwrap() = 0;
        poller.check_message_stalls(&cfg).await;
        assert_eq!(sent("w1").len(), 1, "one nudge per owed message, even across idle stretches");

        let mut quiet = test_session("w3", "/ws-w3");
        quiet.orchestrator_id = Some("orch".into());
        quiet.started_at = now - 60 * MIN;
        quiet.activity = crate::types::ActivityState::Idle;
        quiet.activity_since = Some(now - 40 * MIN);
        store.upsert_session(&quiet).unwrap();
        store.note_message_sent("w3", now - 50 * MIN).unwrap();
        *poller.last_stall_check.lock().unwrap() = 0;
        poller.check_message_stalls(&cfg).await;
        assert!(sent("w3").is_empty(), "a worker that sent after its brief owes nothing");
    }

    /// A store last written by 0.37 has no send bookkeeping; opening it must
    /// not make every idle worker look like it owes a reply.
    #[tokio::test]
    async fn a_store_from_before_send_tracking_nudges_nobody_on_first_start() {
        let _iso = crate::test_isolation::isolate();
        const MIN: i64 = 60_000;
        let now = now_millis();
        let path = tempfile::tempdir().unwrap().keep().join("t.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE sessions (
                    id TEXT PRIMARY KEY, orchestrator_id TEXT,
                    name TEXT NOT NULL, repo TEXT NOT NULL,
                    status TEXT NOT NULL, agent_type TEXT NOT NULL,
                    cost_usd REAL NOT NULL DEFAULT 0, started_at INTEGER NOT NULL,
                    pr_number INTEGER, pr_id INTEGER,
                    workspace_path TEXT, pid INTEGER,
                    model TEXT, context_tokens INTEGER, catalogue_path TEXT, context_used_pct REAL,
                    context_total_tokens INTEGER, context_window_size INTEGER, claude_session_id TEXT,
                    summary TEXT, terminal_at INTEGER, gate_status TEXT, merged_at INTEGER,
                    activity TEXT, activity_note TEXT, activity_since INTEGER, machine_id TEXT
                );
                CREATE TABLE orchestrators (id TEXT PRIMARY KEY, name TEXT NOT NULL, created_at INTEGER NOT NULL);
                CREATE TABLE session_messages (session_id TEXT PRIMARY KEY, delivered_count INTEGER NOT NULL);",
            ).unwrap();
            conn.execute("INSERT INTO orchestrators VALUES ('orch', 'orch', 0)", []).unwrap();
            for (id, orch) in [("orch", None), ("w1", Some("orch")), ("w2", Some("orch"))] {
                conn.execute(
                    "INSERT INTO sessions (id, orchestrator_id, name, repo, status, agent_type, started_at,
                                           workspace_path, activity, activity_since)
                     VALUES (?1, ?2, ?1, '', 'working', 'claude-code', ?3, '/ws', 'idle', ?4)",
                    rusqlite::params![id, orch, now - 300 * MIN, now - 270 * MIN],
                ).unwrap();
            }
            conn.execute("INSERT INTO session_messages VALUES ('w1', 3)", []).unwrap();
        }

        let store = std::sync::Arc::new(crate::store::Store::open(&path).unwrap());
        let engine = github_engine(store.clone(), std::sync::Arc::new(FakeGithub::default()));
        let poller = Poller::new(engine);
        poller.check_message_stalls(&crate::config::MessageStallConfig::default()).await;
        for id in ["orch", "w1", "w2"] {
            assert!(store.message_events_to(id, 0).unwrap().is_empty(), "{id} was messaged");
        }

        store.note_message_received("w1", now_millis() + 1).unwrap();
        let mut w1 = store.get_session("w1").unwrap().unwrap();
        w1.activity_since = Some(now_millis() + 1 - 40 * MIN);
        store.upsert_session(&w1).unwrap();
        *poller.last_stall_check.lock().unwrap() = 0;
        poller.check_message_stalls(&crate::config::MessageStallConfig::default()).await;
        assert_eq!(store.message_events_to("w1", 0).unwrap().len(), 1, "a message received after the upgrade is owed again");
    }

    fn notifs(events: &[Event], kind: NotificationKind) -> Vec<Notification> {
        events.iter().filter_map(|e| match e {
            Event::Notification(n) if n.kind == kind => Some(n.clone()),
            _ => None,
        }).collect()
    }

    #[tokio::test]
    async fn watch_on_merged_pr_notifies_opener_and_auto_closes() {
        let _iso = crate::test_isolation::isolate();
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
        let _iso = crate::test_isolation::isolate();
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
        let _iso = crate::test_isolation::isolate();
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
        let _iso = crate::test_isolation::isolate();
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
        let _iso = crate::test_isolation::isolate();
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

    // ── Worker completion mode / finish-after-merge reminder ────────────────

    fn merged_idle_session(id: &str, idle_mins: i64) -> Session {
        let mut s = test_session(id, "/ws");
        s.status = SessionStatus::Mergeable;
        s.orchestrator_id = Some("orch".into());
        s.pr_number = Some(7);
        s.merged_at = Some(now_millis() - 60 * 60_000);
        s.activity = crate::types::ActivityState::Idle;
        s.activity_since = Some(now_millis() - idle_mins * 60_000);
        s
    }

    fn reminders(rx: &mut tokio::sync::broadcast::Receiver<Event>, id: &str) -> usize {
        notifs(&drain_events(rx), NotificationKind::AgentStuck).iter()
            .filter(|n| n.session_id.as_deref() == Some(id) && n.id.starts_with("merged-idle-"))
            .count()
    }

    #[tokio::test]
    async fn finish_reminder_is_sent_once_across_ticks_and_pollers() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&merged_idle_session("w1", 45)).unwrap();
        let (engine_a, engine_b) = (Engine::new(store.clone()), Engine::new(store.clone()));
        let (mut rx_a, mut rx_b) = (engine_a.subscribe(), engine_b.subscribe());
        let (poller_a, poller_b) = (Poller::new(engine_a), Poller::new(engine_b));

        for _ in 0..3 {
            tokio::join!(poller_a.check_finish_after_merge(30), poller_b.check_finish_after_merge(30));
        }
        assert_eq!(reminders(&mut rx_a, "w1") + reminders(&mut rx_b, "w1"), 1, "one reminder, however many ticks and pollers");
        assert!(!store.claim_finish_nudge("w1", 1).unwrap(), "the claim is persisted");
    }

    #[tokio::test]
    async fn finish_reminder_skips_workers_that_should_not_get_one() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut completed = merged_idle_session("completed", 45);
        completed.self_completed_at = Some(now_millis());
        let mut done = merged_idle_session("done", 45);
        done.status = SessionStatus::Done;
        let mut terminated = merged_idle_session("terminated", 45);
        terminated.status = SessionStatus::Terminated;
        let mut unmerged = merged_idle_session("unmerged", 45);
        unmerged.merged_at = None;
        let recent = merged_idle_session("recent", 5);
        let mut idle_before_merge = merged_idle_session("pre-merge-idle", 120);
        idle_before_merge.merged_at = Some(now_millis() - 5 * 60_000);
        let mut review = merged_idle_session("review", 45);
        review.status = SessionStatus::AwaitingReview;
        let mut busy = merged_idle_session("busy", 45);
        busy.activity = crate::types::ActivityState::Working;
        for s in [&completed, &done, &terminated, &unmerged, &recent, &busy, &idle_before_merge, &review] {
            store.upsert_session(s).unwrap();
        }
        let engine = Engine::new(store.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);
        poller.check_finish_after_merge(30).await;
        let got = drain_events(&mut rx);
        for id in ["completed", "done", "terminated", "unmerged", "recent", "busy", "pre-merge-idle", "review"] {
            assert!(!notifs(&got, NotificationKind::AgentStuck).iter().any(|n| n.session_id.as_deref() == Some(id)), "{id} must not be reminded");
            assert!(store.claim_finish_nudge(id, 1).unwrap(), "{id} must not hold a claim");
        }

        store.upsert_session(&merged_idle_session("w1", 45)).unwrap();
        poller.check_finish_after_merge(0).await;
        assert_eq!(reminders(&mut rx, "w1"), 0, "0 disables the reminder");
    }

    #[tokio::test]
    async fn self_mode_never_reaps_at_merge_even_when_the_dod_is_only_pr_merged() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut s = test_session("w1", "/ws");
        s.status = SessionStatus::Mergeable;
        s.orchestrator_id = Some("orch".into());
        s.pr_number = Some(7);
        s.success_criteria = vec![SuccessCriterion::PrMerged];
        store.upsert_session(&s).unwrap();
        let poller = Poller::new(Engine::new(store.clone()));

        // `self` and `manual` both reach the poller as "do not reap at merge".
        let reap_at_merge = |m: CompletionMode| m == CompletionMode::ReapOnMerge;
        assert!(!reap_at_merge(CompletionMode::SelfComplete) && !reap_at_merge(CompletionMode::Manual));
        poller.handle_merge_detection(&s, 7, true, reap_at_merge(CompletionMode::SelfComplete), "abc").await;
        let row = store.get_session("w1").unwrap().unwrap();
        assert_eq!(row.status, SessionStatus::Mergeable, "kept alive");
        assert!(row.merged_at.is_some());
    }

    #[tokio::test]
    async fn reap_on_merge_mode_reaps_a_pr_merged_only_worker() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut s = test_session("w1", "/ws");
        s.status = SessionStatus::Mergeable;
        s.pr_number = Some(7);
        s.success_criteria = vec![SuccessCriterion::PrMerged];
        store.upsert_session(&s).unwrap();
        let poller = Poller::new(Engine::new(store.clone()));
        assert!(merge_reaps_worker(&s));
        *poller.completion_mode.lock().unwrap() = CompletionMode::ReapOnMerge;
        assert!(poller.about_to_be_reaped_at_merge("w1"));
        *poller.completion_mode.lock().unwrap() = CompletionMode::SelfComplete;
        assert!(!poller.about_to_be_reaped_at_merge("w1"), "self mode never reaps at merge");
        *poller.completion_mode.lock().unwrap() = CompletionMode::ReapOnMerge;
        s.success_criteria.push(SuccessCriterion::Check("docs".into()));
        store.upsert_session(&s).unwrap();
        assert!(!poller.about_to_be_reaped_at_merge("w1"), "a Check item leaves the finish to the worker");

        s.success_criteria = vec![SuccessCriterion::PrMerged];
        store.upsert_session(&s).unwrap();
        poller.handle_merge_detection(&s, 7, true, true, "abc").await;
        assert_eq!(store.get_session("w1").unwrap().unwrap().status, SessionStatus::Done);
    }

    // ── PR event delivery (`deliver_pr_events`, `check_pr_hangs`) ───────────

    fn pr_events_for(events: &[Event], session: &str) -> Vec<Vec<crate::pr_events::PrEventKind>> {
        events.iter().filter_map(|e| match e {
            Event::PrEvents { session_id, events, .. } if session_id == session => {
                Some(events.iter().map(|e| e.kind).collect())
            }
            _ => None,
        }).collect()
    }

    fn snapshot_with_check(number: u64, status: &str, conclusion: Option<&str>) -> crate::github_graphql::PrSnapshot {
        let mut snap = open_snapshot(number);
        snap.checks = vec![CheckRun { name: "test".into(), status: status.into(), conclusion: conclusion.map(Into::into) }];
        snap
    }

    fn pr_session(id: &str, repo: &str, number: u64) -> Session {
        let mut s = test_session(id, "/ws");
        s.status    = SessionStatus::PrOpen;
        s.repo      = repo.into();
        s.pr_number = Some(number);
        s.orchestrator_id = Some("orch".into());
        s
    }

    /// The request-count guarantee: however many workers share however few
    /// PRs, one sweep is one `fetch_batch` carrying one key per PR, and the
    /// hang check costs no request at all.
    #[tokio::test]
    async fn sweep_costs_one_request_with_o_prs_keys_regardless_of_worker_count() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        for i in 0..20u64 {
            let id = format!("w{i}");
            store.upsert_session(&pr_session(&id, "o/r", 1 + i % 3)).unwrap();
            // Every worker also watches its own PR and a shared dependency PR.
            store.upsert_pr_watch(&watch_by("o/r", 1 + i % 3, &id)).unwrap();
            store.upsert_pr_watch(&watch_by("o/dep", 9, &id)).unwrap();
        }
        let batch = std::sync::Arc::new(FakeBatchApi::default());
        let engine = batch_engine(store.clone(), batch.clone());
        let poller = Poller::new(engine);

        poller.poll_github_batched(true).await;
        poller.check_pr_hangs(&crate::config::PrEventsConfig::default()).await;

        let calls = batch.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1, "one sweep, one request");
        assert_eq!(calls[0].0.len(), 4, "20 workers on 3 PRs plus 1 shared watch = 4 keys: {:?}", calls[0].0);
    }

    #[tokio::test]
    async fn own_pr_events_are_delivered_once_and_never_replayed_after_restart() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&pr_session("w1", "o/r", 7)).unwrap();
        // Watching its own PR must not double-deliver.
        store.upsert_pr_watch(&watch_by("o/r", 7, "w1")).unwrap();

        let batch = std::sync::Arc::new(FakeBatchApi::default());
        let engine = batch_engine(store.clone(), batch.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine.clone());
        let tick = |snap: crate::github_graphql::PrSnapshot| {
            batch.result.lock().unwrap().prs.insert(pr_key("o/r", 7), snap);
        };

        tick(snapshot_with_check(7, "in_progress", None));
        poller.poll_github_batched(true).await;
        assert!(pr_events_for(&drain_events(&mut rx), "w1").is_empty(), "pending CI is not an event");
        assert!(
            store.get_pr_status("o/r", 7).unwrap().is_some(),
            "every sweep caches PR state for `ninox pr status`",
        );

        // Green on two sweeps in a row before it counts (see `pass_candidate`).
        tick(snapshot_with_check(7, "completed", Some("success")));
        poller.poll_github_batched(true).await;
        assert!(pr_events_for(&drain_events(&mut rx), "w1").is_empty());
        tick(snapshot_with_check(7, "completed", Some("success")));
        poller.poll_github_batched(true).await;
        assert_eq!(
            pr_events_for(&drain_events(&mut rx), "w1"),
            vec![vec![crate::pr_events::PrEventKind::CiPassed]],
            "exactly one delivery, even though w1 also watches its own PR",
        );

        tick(snapshot_with_check(7, "completed", Some("success")));
        poller.poll_github_batched(true).await;
        assert!(pr_events_for(&drain_events(&mut rx), "w1").is_empty(), "no re-delivery");

        // A daemon restart: fresh poller, empty in-memory caches, same store.
        let restarted = Poller::new(engine);
        tick(snapshot_with_check(7, "completed", Some("success")));
        restarted.poll_github_batched(true).await;
        assert!(pr_events_for(&drain_events(&mut rx), "w1").is_empty(), "nothing replays after a restart");

        // A new push that fails is news again.
        let mut pushed = snapshot_with_check(7, "completed", Some("failure"));
        pushed.status.head_sha = "def".into();
        tick(pushed);
        restarted.poll_github_batched(true).await;
        assert_eq!(
            pr_events_for(&drain_events(&mut rx), "w1"),
            vec![vec![crate::pr_events::PrEventKind::CiFailed]],
        );
    }

    #[tokio::test]
    async fn watched_pr_events_go_to_the_opener_and_bot_comments_are_delivered() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&test_session("sess-a", "/ws")).unwrap();
        store.upsert_pr_watch(&watch_by("o/dep", 3, "sess-a")).unwrap();

        let batch = std::sync::Arc::new(FakeBatchApi::default());
        let engine = batch_engine(store.clone(), batch.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        batch.result.lock().unwrap().prs.insert(pr_key("o/dep", 3), open_snapshot(3));
        poller.poll_github_batched(true).await;
        drain_events(&mut rx);

        let mut snap = open_snapshot(3);
        snap.threads = vec![crate::github::ReviewThread {
            id: 900, author: "cursor".into(), body: "Bug: off by one".into(),
            path: Some("src/a.rs".into()), line: Some(12), state: "COMMENTED".into(), created_at: 1,
        }];
        batch.result.lock().unwrap().prs.insert(pr_key("o/dep", 3), snap);
        poller.poll_github_batched(true).await;
        let events = drain_events(&mut rx);
        let delivered: Vec<_> = events.iter().filter_map(|e| match e {
            Event::PrEvents { session_id, events, .. } if session_id == "sess-a" => Some(events.clone()),
            _ => None,
        }).collect();
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0][0].kind, crate::pr_events::PrEventKind::ReviewComment);
        assert!(delivered[0][0].detail.contains("cursor at src/a.rs:12"), "{:?}", delivered[0][0]);
    }

    #[tokio::test]
    async fn interrupted_subscriber_hears_what_it_missed_once_resumed() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&pr_session("w1", "o/r", 7)).unwrap();
        let batch = std::sync::Arc::new(FakeBatchApi::default());
        let engine = batch_engine(store.clone(), batch.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        batch.result.lock().unwrap().prs.insert(pr_key("o/r", 7), snapshot_with_check(7, "in_progress", None));
        poller.poll_github_batched(true).await;

        let mut s = store.get_session("w1").unwrap().unwrap();
        s.status = SessionStatus::Interrupted;
        store.upsert_session(&s).unwrap();
        batch.result.lock().unwrap().prs.insert(pr_key("o/r", 7), snapshot_with_check(7, "completed", Some("failure")));
        poller.poll_github_batched(true).await;
        assert!(pr_events_for(&drain_events(&mut rx), "w1").is_empty(), "nothing is sent to an interrupted session");

        s.status = SessionStatus::PrOpen;
        store.upsert_session(&s).unwrap();
        batch.result.lock().unwrap().prs.insert(pr_key("o/r", 7), snapshot_with_check(7, "completed", Some("failure")));
        poller.poll_github_batched(true).await;
        assert_eq!(
            pr_events_for(&drain_events(&mut rx), "w1"),
            vec![vec![crate::pr_events::PrEventKind::CiFailed]],
        );
    }

    #[tokio::test]
    async fn legacy_rest_path_delivers_pr_events_too() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let repo_dir = init_git_repo("worker-branch", &[("origin", "https://github.com/Owner/repo.git")]);
        let workspace = repo_dir.path().to_string_lossy().to_string();
        let fake = open_pr_fake(50);
        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&open_pr_session("s1", &workspace, 50)).unwrap();
        let engine = github_engine(store.clone(), fake.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        poller.poll_github(true).await;
        drain_events(&mut rx);
        fake.review_threads.lock().unwrap().push(crate::github::ReviewThread {
            id: 501, author: "carol".into(), body: "rename this".into(),
            path: Some("src/x.rs".into()), line: Some(2), state: "COMMENTED".into(), created_at: 1_000,
        });
        poller.poll_github(true).await;
        assert_eq!(
            pr_events_for(&drain_events(&mut rx), "s1"),
            vec![vec![crate::pr_events::PrEventKind::ReviewComment]],
        );
    }

    #[tokio::test]
    async fn hang_nudge_is_rate_limited_and_tells_the_orchestrator_once() {
        let _iso = crate::test_isolation::isolate();
        use crate::pr_events::{CiVerdict, PrStatusView};
        use crate::store::{CursorKey, Store};

        const MIN: i64 = 60_000;
        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let now = now_millis();
        let mut s = pr_session("w1", "o/r", 7);
        s.activity = crate::types::ActivityState::Idle;
        s.activity_since = Some(now - 40 * MIN);
        store.upsert_session(&s).unwrap();
        let key = CursorKey { subscriber: "w1".into(), repo: "o/r".into(), number: 7 };
        let quiet = crate::pr_events::PrCursor { initialized: true, first_seen_at: now - 40 * MIN, ..Default::default() };
        store.save_pr_event_cursor(&key, &quiet, now).unwrap();
        let view = |ci: CiVerdict, observed_at: i64| PrStatusView {
            repo: "o/r".into(), number: 7, ci, observed_at, ..Default::default()
        };

        let batch = std::sync::Arc::new(FakeBatchApi::default());
        let engine = batch_engine(store.clone(), batch.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);
        let cfg = crate::config::PrEventsConfig::default();
        let nudges = |rx: &mut tokio::sync::broadcast::Receiver<Event>| notifs(&drain_events(rx), NotificationKind::AgentStuck).len();

        poller.check_pr_hangs(&cfg).await;
        assert_eq!(nudges(&mut rx), 0, "no cached PR state: Ninox can't speak for GitHub");
        store.save_pr_status(&view(CiVerdict::Passed, now - 20 * MIN)).unwrap();
        poller.check_pr_hangs(&cfg).await;
        assert_eq!(nudges(&mut rx), 0, "stale PR state (the sweep isn't seeing GitHub) never nudges");
        store.save_pr_status(&view(CiVerdict::Pending, now)).unwrap();
        poller.check_pr_hangs(&cfg).await;
        assert_eq!(nudges(&mut rx), 0, "CI still running is not a hang");

        store.save_pr_status(&view(CiVerdict::Passed, now)).unwrap();
        poller.check_pr_hangs(&cfg).await;
        assert_eq!(nudges(&mut rx), 1, "idle 40 min with a quiet, freshly observed PR is nudged");
        let c = store.load_pr_event_cursor(&key).unwrap().unwrap();
        assert!(c.hang_orch_notified, "the orchestrator is told");

        poller.check_pr_hangs(&cfg).await;
        assert_eq!(nudges(&mut rx), 0, "one nudge per idle episode");

        // The worker woke, went idle again 35 min ago: a new episode, but the
        // last nudge was moments ago.
        s.activity_since = Some(now - 35 * MIN);
        store.upsert_session(&s).unwrap();
        poller.check_pr_hangs(&cfg).await;
        assert_eq!(nudges(&mut rx), 0, "nudges are at least N minutes apart");

        let backdate = |store: &Store| {
            let mut c = store.load_pr_event_cursor(&key).unwrap().unwrap();
            c.hang_nudged_at = now - 31 * MIN;
            store.save_pr_event_cursor(&key, &c, now).unwrap();
        };
        backdate(&store);
        poller.check_pr_hangs(&cfg).await;
        assert_eq!(nudges(&mut rx), 1, "a new episode after the rate limit is nudged again");
        assert!(store.load_pr_event_cursor(&key).unwrap().unwrap().hang_orch_notified, "same quiet spell for the orchestrator");

        s.activity_since = Some(now - 32 * MIN);
        store.upsert_session(&s).unwrap();
        backdate(&store);
        poller.check_pr_hangs(&cfg).await;
        assert_eq!(nudges(&mut rx), 0, "at most two nudges per quiet spell");

        // Working workers, and a disabled check, are never nudged.
        let mut busy = s.clone();
        busy.activity = crate::types::ActivityState::Working;
        busy.activity_since = Some(now - 100 * MIN);
        store.upsert_session(&busy).unwrap();
        poller.check_pr_hangs(&cfg).await;
        poller.check_pr_hangs(&crate::config::PrEventsConfig { hang_nudge_after_mins: 0, ..cfg.clone() }).await;
        assert_eq!(nudges(&mut rx), 0);
        assert!(batch.calls.lock().unwrap().is_empty(), "the hang check never calls GitHub");
    }

    #[tokio::test]
    async fn watch_and_session_repos_match_case_insensitively() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&pr_session("w1", "owner/repo", 7)).unwrap();
        store.upsert_pr_watch(&watch_by("Owner/Repo", 7, "w1")).unwrap();
        let batch = std::sync::Arc::new(FakeBatchApi::default());
        let engine = batch_engine(store.clone(), batch.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);
        let tick = |conclusion: &str| {
            let mut r = batch.result.lock().unwrap();
            r.prs.insert(pr_key("owner/repo", 7), snapshot_with_check(7, "completed", Some(conclusion)));
            r.prs.insert(pr_key("Owner/Repo", 7), snapshot_with_check(7, "completed", Some(conclusion)));
        };
        tick("success");
        poller.poll_github_batched(true).await;
        drain_events(&mut rx);
        tick("failure");
        poller.poll_github_batched(true).await;
        assert_eq!(pr_events_for(&drain_events(&mut rx), "w1").len(), 1, "one subscription, one delivery");
        assert!(store.get_pr_status("owner/repo", 7).unwrap().is_some(), "cached under the lowercase slug");
    }

    #[tokio::test]
    async fn a_merged_worker_kept_alive_hears_its_merge_and_keeps_its_watches() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut s = pr_session("w1", "o/r", 7);
        s.merged_at = Some(1);
        store.upsert_session(&s).unwrap();
        store.upsert_pr_watch(&watch_by("o/dep", 3, "w1")).unwrap();
        let batch = std::sync::Arc::new(FakeBatchApi::default());
        let engine = batch_engine(store.clone(), batch.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        batch.result.lock().unwrap().prs.insert(pr_key("o/dep", 3), open_snapshot(3));
        poller.poll_github_batched(true).await;
        drain_events(&mut rx);
        let mut snap = open_snapshot(3);
        snap.issue_comments = vec![Comment { id: 5, pr_id: 3, author: "bob".into(), body: "done?".into(), path: None, line: None, created_at: 1 }];
        batch.result.lock().unwrap().prs.insert(pr_key("o/dep", 3), snap);
        poller.poll_github_batched(true).await;
        assert_eq!(
            pr_events_for(&drain_events(&mut rx), "w1"),
            vec![vec![crate::pr_events::PrEventKind::PrComment]],
            "a kept-alive merged worker still gets its watches",
        );
    }

    #[test]
    fn hang_due_needs_both_the_worker_and_the_pr_quiet() {
        const MIN: i64 = 60_000;
        let now = 1_000 * MIN;
        let mut c = crate::pr_events::PrCursor { initialized: true, first_seen_at: now - 40 * MIN, ..Default::default() };
        assert!(hang_due(&c, now - 40 * MIN, now, 30 * MIN));
        assert!(!hang_due(&c, now - 10 * MIN, now, 30 * MIN), "worker only idle 10 min");
        c.last_event_at = now - 5 * MIN;
        assert!(!hang_due(&c, now - 40 * MIN, now, 30 * MIN), "a PR event 5 min ago");
        c.last_event_at = 0;
        c.terminal = true;
        assert!(!hang_due(&c, now - 40 * MIN, now, 30 * MIN), "a closed PR is not a hang");
        c.terminal = false;
        c.initialized = false;
        assert!(!hang_due(&c, now - 40 * MIN, now, 30 * MIN), "never observed: nothing to say yet");
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
        let _iso = crate::test_isolation::isolate();
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
        let _iso = crate::test_isolation::isolate();
        let engine = bare_engine();
        let mut rx = engine.subscribe();
        let current: semver::Version = env!("CARGO_PKG_VERSION").parse().unwrap();
        let source = Arc::new(FakeUpdateSource(std::sync::Mutex::new(Some(current))));
        let poller = Poller::new(engine).with_update_source(source);

        poller.poll_update_check().await;
        assert_eq!(update_events(&drain_events(&mut rx)), 0);
    }

    struct FailingUpdateSource(String);

    #[async_trait::async_trait]
    impl UpdateSource for FailingUpdateSource {
        async fn latest_version(&self, _package: &str) -> anyhow::Result<Option<semver::Version>> {
            Err(anyhow::anyhow!("{}", self.0))
        }
    }

    #[tokio::test]
    async fn poll_update_check_records_each_distinct_failure_once_and_recovers() {
        let _iso = crate::test_isolation::isolate();
        let poller = Poller::new(bare_engine())
            .with_update_source(Arc::new(FailingUpdateSource("sparse index went away".into())));
        poller.poll_update_check().await;
        let first = poller.last_update_error.lock().unwrap().clone();
        assert!(first.as_deref().is_some_and(|m| m.contains("sparse index went away")), "{first:?}");
        poller.poll_update_check().await;
        assert_eq!(*poller.last_update_error.lock().unwrap(), first, "an identical failure is not a new message");

        let sso = Poller::new(bare_engine()).with_update_source(Arc::new(FailingUpdateSource(
            "aws codeartifact list-package-versions failed — no valid AWS session; run `aws sso login`".into(),
        )));
        sso.poll_update_check().await;
        let msg = sso.last_update_error.lock().unwrap().clone().unwrap();
        assert_eq!(msg, "update check skipped: `aws sso login` needed");

        let ok = poller.with_update_source(Arc::new(FakeUpdateSource(std::sync::Mutex::new(None))));
        ok.poll_update_check().await;
        assert_eq!(*ok.last_update_error.lock().unwrap(), None, "a successful check clears the remembered failure");
    }

    #[tokio::test]
    async fn poll_update_check_no_notification_when_source_has_nothing() {
        let _iso = crate::test_isolation::isolate();
        let engine = bare_engine();
        let mut rx = engine.subscribe();
        let source = Arc::new(FakeUpdateSource(std::sync::Mutex::new(None)));
        let poller = Poller::new(engine).with_update_source(source);

        poller.poll_update_check().await;
        assert_eq!(update_events(&drain_events(&mut rx)), 0);
    }

    // ── Remote machine health ────────────────────────────────────────────────

    /// Records every `(target, profile_id)` pair it's asked to reconnect and
    /// resolves with a caller-configured, swappable-mid-test outcome (same
    /// shape as `FakeUpdateSource`) — never shells out to a real `ssh`.
    struct FakeMachineHealthProbe {
        calls:   std::sync::Mutex<Vec<(String, String)>>,
        succeed: std::sync::Mutex<bool>,
    }

    impl FakeMachineHealthProbe {
        fn new(succeed: bool) -> Self {
            Self { calls: std::sync::Mutex::new(Vec::new()), succeed: std::sync::Mutex::new(succeed) }
        }
        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
        fn set_succeed(&self, succeed: bool) {
            *self.succeed.lock().unwrap() = succeed;
        }
    }

    #[async_trait::async_trait]
    impl MachineHealthProbe for FakeMachineHealthProbe {
        async fn reconnect(&self, target: &str, profile_id: &str) -> anyhow::Result<()> {
            self.calls.lock().unwrap().push((target.to_string(), profile_id.to_string()));
            if *self.succeed.lock().unwrap() {
                Ok(())
            } else {
                anyhow::bail!("simulated unreachable")
            }
        }
    }

    const ONE_MACHINE_CONFIG: &str = r#"
port = 8080
font_size = 13.0

[remote_machines]
enabled = true

[[remote_machines.machines]]
id = "m1"
label = "box1"
ssh_target = "box1.example"
remote_session = "default"
enabled = true
"#;

    fn machine_notifications(evs: &[Event]) -> Vec<NotificationKind> {
        evs.iter()
            .filter_map(|e| match e {
                Event::Notification(n)
                    if matches!(n.kind, NotificationKind::MachineUnreachable | NotificationKind::MachineReachable) =>
                {
                    Some(n.kind.clone())
                }
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn poll_remote_machine_health_does_nothing_when_remote_machines_disabled() {
        let config_dir = tempfile::tempdir().unwrap();
        let config_path = config_dir.path().join("config.toml");
        std::fs::write(&config_path, "port = 8080\nfont_size = 13.0\n").unwrap();
        let _iso = crate::test_isolation::isolate_with_config(&config_path);

        let probe = Arc::new(FakeMachineHealthProbe::new(true));
        let poller = Poller::new(bare_engine()).with_machine_health_probe(probe.clone());
        poller.poll_remote_machine_health().await;
        assert_eq!(probe.call_count(), 0, "a disabled [remote_machines] section must never be probed");
    }

    #[tokio::test]
    async fn poll_remote_machine_health_skips_disabled_profiles() {
        let config_dir = tempfile::tempdir().unwrap();
        let config_path = config_dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "{ONE_MACHINE_CONFIG}\n[[remote_machines.machines]]\nid = \"m2\"\nlabel = \"box2\"\n\
                 ssh_target = \"box2.example\"\nremote_session = \"default\"\nenabled = false\n"
            ),
        ).unwrap();
        let _iso = crate::test_isolation::isolate_with_config(&config_path);

        let probe = Arc::new(FakeMachineHealthProbe::new(true));
        let poller = Poller::new(bare_engine()).with_machine_health_probe(probe.clone());
        poller.poll_remote_machine_health().await;
        let calls = probe.calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "only the enabled profile must be probed");
        assert_eq!(calls[0], ("box1.example".to_string(), "m1".to_string()));
        drop(calls);
    }

    #[tokio::test]
    async fn poll_remote_machine_health_backs_off_and_notifies_once_then_recovers() {
        let config_dir = tempfile::tempdir().unwrap();
        let config_path = config_dir.path().join("config.toml");
        std::fs::write(&config_path, ONE_MACHINE_CONFIG).unwrap();
        let _iso = crate::test_isolation::isolate_with_config(&config_path);

        let engine = bare_engine();
        let mut rx = engine.subscribe();
        let probe = Arc::new(FakeMachineHealthProbe::new(false));
        let poller = Poller::new(engine).with_machine_health_probe(probe.clone());

        // First failure: backoff starts at 5s (below the cap) → Reconnecting, notifies once.
        poller.poll_remote_machine_health().await;
        let state = poller.machine_health_state("m1").unwrap();
        assert_eq!(state.backoff_secs, 5);
        assert_eq!(poller.engine.store.machine_health().unwrap().get("m1"), Some(&MachineHealth::Reconnecting));
        assert_eq!(machine_notifications(&drain_events(&mut rx)), vec![NotificationKind::MachineUnreachable]);

        // Immediately again: not due yet (backoff hasn't elapsed) — no new probe call.
        poller.poll_remote_machine_health().await;
        assert_eq!(probe.call_count(), 1, "a second tick before the backoff elapses must not re-probe");

        // Force it due: backoff doubles, and the dedup flag suppresses a second notification.
        poller.force_machine_probe_due("m1");
        poller.poll_remote_machine_health().await;
        assert_eq!(poller.machine_health_state("m1").unwrap().backoff_secs, 10);
        assert!(
            machine_notifications(&drain_events(&mut rx)).is_empty(),
            "a second consecutive failure must not re-notify",
        );

        // Drive the backoff up to its cap: health flips from Reconnecting to Unreachable.
        for _ in 0..5 {
            poller.force_machine_probe_due("m1");
            poller.poll_remote_machine_health().await;
        }
        assert_eq!(poller.machine_health_state("m1").unwrap().backoff_secs, 120);
        assert_eq!(poller.engine.store.machine_health().unwrap().get("m1"), Some(&MachineHealth::Unreachable));

        // Recovery: backoff resets, health flips to Connected, and a MachineReachable notification fires.
        probe.set_succeed(true);
        poller.force_machine_probe_due("m1");
        poller.poll_remote_machine_health().await;
        assert_eq!(poller.machine_health_state("m1").unwrap().backoff_secs, 0);
        assert_eq!(poller.engine.store.machine_health().unwrap().get("m1"), Some(&MachineHealth::Connected));
        assert_eq!(machine_notifications(&drain_events(&mut rx)), vec![NotificationKind::MachineReachable]);

        // A further success must not re-notify (no `was_unreachable` flag left to clear).
        poller.force_machine_probe_due("m1");
        poller.poll_remote_machine_health().await;
        assert!(machine_notifications(&drain_events(&mut rx)).is_empty());
    }

    /// A profile's backoff/dedup bookkeeping must not outlive its
    /// `MachineProfile` in config — otherwise a long-running daemon cycling
    /// through `machine add`/`machine remove` would grow `remote_machine_health`
    /// without bound, keyed by every id ever seen rather than only the
    /// currently-configured ones.
    #[tokio::test]
    async fn poll_remote_machine_health_prunes_state_for_a_removed_profile() {
        let config_dir = tempfile::tempdir().unwrap();
        let config_path = config_dir.path().join("config.toml");
        std::fs::write(&config_path, ONE_MACHINE_CONFIG).unwrap();
        let _iso = crate::test_isolation::isolate_with_config(&config_path);

        let probe = Arc::new(FakeMachineHealthProbe::new(false));
        let poller = Poller::new(bare_engine()).with_machine_health_probe(probe);

        poller.poll_remote_machine_health().await;
        assert!(poller.machine_health_state("m1").is_some(), "backoff state must be recorded on failure");
        assert!(poller.engine.store.machine_health().unwrap().contains_key("m1"));

        // `m1` is removed from config entirely (mirrors `machine remove`).
        std::fs::write(&config_path, "port = 8080\nfont_size = 13.0\n\n[remote_machines]\nenabled = true\n").unwrap();
        poller.poll_remote_machine_health().await;
        assert!(poller.machine_health_state("m1").is_none(), "state for a removed profile must be pruned, not kept forever");
        assert!(
            !poller.engine.store.machine_health().unwrap().contains_key("m1"),
            "the stored machine_health row must be pruned too, not just the in-memory backoff state",
        );
    }

    /// Pruning must not be gated on `[remote_machines].enabled`: a profile
    /// removed while remote machines are switched off must not leave its
    /// persisted `machine_health` row stranded until (if ever) remote
    /// machines are re-enabled.
    #[tokio::test]
    async fn poll_remote_machine_health_prunes_a_removed_profile_even_while_disabled() {
        let config_dir = tempfile::tempdir().unwrap();
        let config_path = config_dir.path().join("config.toml");
        std::fs::write(&config_path, ONE_MACHINE_CONFIG).unwrap();
        let _iso = crate::test_isolation::isolate_with_config(&config_path);

        let probe = Arc::new(FakeMachineHealthProbe::new(false));
        let poller = Poller::new(bare_engine()).with_machine_health_probe(probe.clone());

        poller.poll_remote_machine_health().await;
        assert!(poller.engine.store.machine_health().unwrap().contains_key("m1"));

        // `[remote_machines]` is disabled AND `m1` removed in the same edit.
        std::fs::write(&config_path, "port = 8080\nfont_size = 13.0\n\n[remote_machines]\nenabled = false\n").unwrap();
        poller.poll_remote_machine_health().await;
        assert_eq!(probe.call_count(), 1, "disabled must still skip probing");
        assert!(
            !poller.engine.store.machine_health().unwrap().contains_key("m1"),
            "pruning must run even while [remote_machines] is disabled",
        );
    }

    /// Explicit invariant from the design spec's §6: reconnection writes its
    /// own `machine_health` row (see `poll_remote_machine_health_backs_off_and_notifies_once_then_recovers`
    /// for that), but must never touch a *session* row, and therefore never
    /// reaches into UI-selection state either, since that lives client-side
    /// and is no part of the store at all.
    #[tokio::test]
    async fn poll_remote_machine_health_never_touches_session_rows() {
        use crate::store::Store;
        let config_dir = tempfile::tempdir().unwrap();
        let config_path = config_dir.path().join("config.toml");
        std::fs::write(&config_path, ONE_MACHINE_CONFIG).unwrap();
        let _iso = crate::test_isolation::isolate_with_config(&config_path);

        let store = Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&test_session("s1", "/ws")).unwrap();
        let before: Vec<serde_json::Value> =
            store.list_sessions().unwrap().iter().map(|s| serde_json::to_value(s).unwrap()).collect();

        let engine = Engine::new(store.clone());
        let probe = Arc::new(FakeMachineHealthProbe::new(false));
        let poller = Poller::new(engine).with_machine_health_probe(probe);
        poller.poll_remote_machine_health().await;

        let after: Vec<serde_json::Value> =
            store.list_sessions().unwrap().iter().map(|s| serde_json::to_value(s).unwrap()).collect();
        assert_eq!(before, after, "a health-probe tick must never write a session row");
    }

    // ── Brain harvest ────────────────────────────────────────────────────────

    use std::{future::Future, pin::Pin};

    /// Harvests run on a spawned task that shells out to git several times,
    /// which takes seconds on a loaded machine: this bound only catches a
    /// harvest that never comes.
    const HARVEST_WAIT: Duration = Duration::from_secs(30);

    async fn next_harvest<T>(rx: &mut tokio::sync::mpsc::UnboundedReceiver<T>, what: &str) -> T {
        tokio::time::timeout(HARVEST_WAIT, rx.recv())
            .await
            .unwrap_or_else(|_| panic!("{what}: nothing within {HARVEST_WAIT:?}"))
            .unwrap_or_else(|| panic!("{what}: the harvest channel closed"))
    }

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
        // Never the developer's commit signing: it can fail or stall, and a
        // silently missing commit leaves an empty diff, so no harvest at all.
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(["-C", dir.to_str().unwrap(), "-c", "commit.gpgsign=false"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
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
    #[tokio::test]
    async fn metadata_sync_triggers_brain_harvest_exactly_once_on_pr_detection() {
        use crate::store::Store;

        let _iso = crate::test_isolation::isolate_with_config(nonexistent_config_path());

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

        let (prompt, ..) = next_harvest(&mut rx, "harvest should be attempted").await;
        assert!(prompt.contains("src.rs") && prompt.contains("fn main"), "prompt must include the diff");

        // Second tick: pr_number is already Some, so the transition guard
        // must not fire the harvest again.
        poller.sync_sessions_metadata(sessions_dir.path()).await;
        assert!(rx.try_recv().is_err(), "harvest must fire exactly once per session");
    }

    /// A worker whose brain set reads several catalogues must have its
    /// harvest write to the set's write catalogue, not the first read and
    /// not the global default brain path.
    #[tokio::test]
    async fn metadata_sync_brain_harvest_writes_to_session_brain_write() {
        use crate::store::Store;

        let _iso = crate::test_isolation::isolate_with_config(nonexistent_config_path());

        let repo = init_diff_repo("feature-catalogue", Some(("src.rs", "fn main() {}\n")));
        let workspace = repo.to_str().unwrap().to_string();

        let sessions_dir = tempfile::tempdir().unwrap();
        let meta = serde_json::json!({"agentReportedPrNumber": "13"});
        std::fs::write(sessions_dir.path().join("s1.json"), serde_json::to_string(&meta).unwrap()).unwrap();

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut session = test_session("s1", &workspace);
        session.brain_reads = vec!["/custom/other-catalogue".into(), "/custom/brain-catalogue".into()];
        session.brain_write = Some("/custom/brain-catalogue".to_string());
        store.upsert_session(&session).unwrap();
        let engine = Engine::new(store.clone());

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let runner = Arc::new(FakeHarvestRunner { calls: tx, outcome: Ok(()) });
        let poller = Poller::new_with_harvest_runner(engine, runner);

        poller.sync_sessions_metadata(sessions_dir.path()).await;

        let (_, _, brain_path) = next_harvest(&mut rx, "harvest should be attempted").await;
        assert_eq!(
            brain_path, PathBuf::from("/custom/brain-catalogue"),
            "harvest must target the session's own catalogue, not the global default",
        );
    }

    /// `brain_harvest.enabled = false` must suppress the harvest entirely —
    /// PR detection itself proceeds exactly as it would with it enabled.
    #[tokio::test]
    async fn metadata_sync_skips_brain_harvest_when_disabled() {
        use crate::store::Store;

        let config_dir = tempfile::tempdir().unwrap();
        let config_path = config_dir.path().join("config.toml");
        std::fs::write(&config_path, "port = 8080\nfont_size = 13.0\n\n[brain_harvest]\nenabled = false\n").unwrap();
        let _iso = crate::test_isolation::isolate_with_config(&config_path);

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
    }

    /// A session whose branch has no diff against the default branch yet
    /// must not trigger a harvest — nothing worth recording, and no point
    /// invoking an LLM call for it.
    #[tokio::test]
    async fn metadata_sync_skips_brain_harvest_for_trivial_diff() {
        use crate::store::Store;

        let _iso = crate::test_isolation::isolate_with_config(nonexistent_config_path());

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
    }

    /// A failing harvest subprocess must not affect the rest of
    /// `sync_sessions_metadata` — the session still transitions to `PrOpen`
    /// normally, and the failure is swallowed rather than propagated.
    #[tokio::test]
    async fn metadata_sync_survives_a_failing_brain_harvest() {
        use crate::store::Store;

        let _iso = crate::test_isolation::isolate_with_config(nonexistent_config_path());

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

        next_harvest(&mut rx, "the failing harvest must still be attempted").await;
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
    #[tokio::test]
    async fn metadata_sync_logs_a_warning_when_harvest_task_panics() {
        use crate::store::Store;
        use tracing_subscriber::{layer::SubscriberExt, Registry};

        let _iso = crate::test_isolation::isolate_with_config(nonexistent_config_path());

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
        let deadline = std::time::Instant::now() + HARVEST_WAIT;
        while !logs.contains("brain harvest task panicked") {
            assert!(std::time::Instant::now() < deadline, "timed out waiting for the panic to be logged");
            tokio::time::sleep(Duration::from_millis(20)).await;
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
            let calls = self.calls.clone();
            let active = self.active.clone();
            let overlapped = self.overlapped.clone();
            Box::pin(async move {
                if active.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0 {
                    overlapped.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
                active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                let _ = calls.send(());
                Ok(())
            })
        }
    }

    /// Two sessions whose harvests target the same (default) brain vault —
    /// neither sets `brain_write`, so both resolve to
    /// `config.resolved_brain_path()` — must never have their
    /// `HarvestRunner::run` calls (which, in production, both invoke `ninox
    /// brain index`) overlap.
    #[tokio::test]
    async fn concurrent_harvests_to_the_same_vault_do_not_overlap() {
        use crate::store::Store;

        let _iso = crate::test_isolation::isolate_with_config(nonexistent_config_path());

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

        // The runner reports each call once its simulated work is done, so
        // after two reports `overlapped` reflects both full runs.
        for _ in 0..2 {
            next_harvest(&mut rx, "both harvests should run to completion").await;
        }

        assert!(
            !overlapped.load(std::sync::atomic::Ordering::SeqCst),
            "concurrent harvests to the same vault must not run HarvestRunner::run concurrently",
        );
    }

    /// Two syntactically different paths to the same physical vault (here:
    /// a trailing slash) must resolve to the same lock — otherwise two
    /// harvests using differently-spelled `brain_write`s for the same
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
        let _iso = crate::test_isolation::isolate();
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

        let handled = poller.handle_merge_detection(&s, 42, true, true, "abc123").await;
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
        let _iso = crate::test_isolation::isolate();
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

        let first = poller.handle_merge_detection(&s, 7, true, true, "abc123").await;
        assert!(first, "first tick handles the merge");

        // Simulate the next poll tick re-reading the (now Done) session from
        // the store before calling merge detection again.
        let updated = store.get_session("w1").unwrap().unwrap();
        let second = poller.handle_merge_detection(&updated, 7, true, true, "abc123").await;
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
        let _iso = crate::test_isolation::isolate();
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

        let handled = poller.handle_merge_detection(&s, 7, true, false, "abc123").await;
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
        let _iso = crate::test_isolation::isolate();
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

        assert!(poller.handle_merge_detection(&s, 7, true, false, "abc123").await);

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
        let _iso = crate::test_isolation::isolate();
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

        assert!(poller.handle_merge_detection(&s, 7, true, false, "abc123").await);

        let updated = store.get_session("w1").unwrap().unwrap();
        let second = poller.handle_merge_detection(&updated, 7, true, false, "abc123").await;
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

        let handled = poller.handle_merge_detection(&s, 1, false, true, "abc123").await;
        assert!(!handled);
        assert!(matches!(store.get_session("w1").unwrap().unwrap().status, SessionStatus::Working));
    }

    #[tokio::test]
    async fn merge_detection_auto_reaps_when_criteria_are_pr_merged_only() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;
        use crate::types::SuccessCriterion;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut s = test_session("w1", "/ws");
        s.orchestrator_id = Some("orch1".into());
        s.pr_number = Some(7);
        s.success_criteria = vec![SuccessCriterion::PrMerged];
        store.upsert_session(&s).unwrap();

        let engine = Engine::new(store.clone());
        let poller = Poller::new(engine);
        poller.handle_merge_detection(&s, 7, true, true, "abc123").await;

        let after = store.get_session("w1").unwrap().unwrap();
        assert!(matches!(after.status, SessionStatus::Done), "PrMerged-only criteria must still auto-reap");
    }

    #[tokio::test]
    async fn merge_detection_auto_reaps_when_criteria_are_pr_merged_and_report_sent() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;
        use crate::types::SuccessCriterion;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut s = test_session("w1", "/ws");
        s.orchestrator_id = Some("orch1".into());
        s.pr_number = Some(7);
        s.success_criteria = vec![SuccessCriterion::PrMerged, SuccessCriterion::ReportSent];
        store.upsert_session(&s).unwrap();

        let engine = Engine::new(store.clone());
        let poller = Poller::new(engine);
        poller.handle_merge_detection(&s, 7, true, true, "abc123").await;

        let after = store.get_session("w1").unwrap().unwrap();
        assert!(
            matches!(after.status, SessionStatus::Done),
            "PrMerged + ReportSent are both trivially true once merged — still auto-reaps",
        );
    }

    #[tokio::test]
    async fn merge_detection_still_auto_reaps_when_criteria_are_empty() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut s = test_session("w1", "/ws");
        s.orchestrator_id = Some("orch1".into());
        s.pr_number = Some(7);
        assert!(s.success_criteria.is_empty(), "test assumption: untyped/pre-existing session");
        store.upsert_session(&s).unwrap();

        let engine = Engine::new(store.clone());
        let poller = Poller::new(engine);
        poller.handle_merge_detection(&s, 7, true, true, "abc123").await;

        let after = store.get_session("w1").unwrap().unwrap();
        assert!(matches!(after.status, SessionStatus::Done), "back-compat: empty criteria still auto-reaps");
    }

    #[tokio::test]
    async fn merge_detection_nudges_instead_of_reaping_when_criteria_include_a_check_item() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;
        use crate::types::SuccessCriterion;

        // A real tmux session for the orchestrator — `send_to_session` only
        // records a delivered message once it actually lands somewhere, and
        // this test needs to read the nudge text back.
        let orch_id = format!(
            "orch-nudge-test-{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
        );
        let orch_ws = tempfile::tempdir().unwrap().keep();
        crate::tmux::create_session(&orch_id, orch_ws.to_str().unwrap(), "sleep 30", &[]).await.unwrap();

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut s = test_session("w1", "/ws");
        s.orchestrator_id = Some(orch_id.clone());
        s.pr_number = Some(7);
        s.success_criteria = vec![SuccessCriterion::PrMerged, SuccessCriterion::Check("docs updated".into())];
        store.upsert_session(&s).unwrap();

        let engine = Engine::new(store.clone());
        let poller = Poller::new(engine);
        poller.handle_merge_detection(&s, 7, true, true, "abc123").await;

        let after = store.get_session("w1").unwrap().unwrap();
        assert!(matches!(after.status, SessionStatus::Working), "must not be cleaned up");
        assert!(after.merged_at.is_some(), "the nudge must be gated on a persisted merged_at stamp");

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let screen = crate::tmux::capture_screen(&orch_id, 0, false).await.unwrap_or_default();
        assert!(screen.contains("ninox complete"), "orchestrator must get the nudge, got: {screen:?}");

        crate::tmux::kill_session(&orch_id).await.ok();
    }

    #[test]
    fn claim_merged_at_succeeds_once_and_never_overwrites() {
        let _iso = crate::test_isolation::isolate();
        let store = crate::store::Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap();
        store.upsert_session(&test_session("w1", "/ws")).unwrap();

        assert!(store.claim_merged_at("w1", 10).unwrap());
        assert!(!store.claim_merged_at("w1", 20).unwrap());
        assert_eq!(store.get_session("w1").unwrap().unwrap().merged_at, Some(10));
        assert!(!store.claim_merged_at("missing", 10).unwrap());
    }

    fn needs_complete_nudges(store: &crate::store::Store, orch: &str) -> usize {
        store.message_events_to(orch, 0).unwrap()
            .iter().filter(|m| m.text.contains("Definition of done")).count()
    }

    fn check_criteria_session(orch: &str) -> crate::types::Session {
        use crate::types::SuccessCriterion;
        let mut s = test_session("w1", "/ws");
        s.orchestrator_id = Some(orch.into());
        s.pr_number = Some(7);
        s.success_criteria = vec![SuccessCriterion::PrMerged, SuccessCriterion::Check("docs updated".into())];
        s
    }

    /// Every tick re-reads the session from the store, as `poll_github` does.
    #[tokio::test]
    async fn needs_complete_nudge_is_sent_once_across_many_ticks() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&check_criteria_session("orch1")).unwrap();
        let poller = Poller::new(Engine::new(store.clone()));

        for _ in 0..5 {
            let row = store.get_session("w1").unwrap().unwrap();
            assert!(poller.handle_merge_detection(&row, 7, true, true, "abc123").await);
        }

        let after = store.get_session("w1").unwrap().unwrap();
        assert!(after.merged_at.is_some());
        assert!(!after.status.is_terminal(), "the worker must not be reaped, got {:?}", after.status);
        assert_eq!(needs_complete_nudges(&store, "orch1"), 1);
    }

    /// The daemon and the GUI poller both see the merge from a stale
    /// snapshot (`merged_at` still None) in the same tick.
    #[tokio::test]
    async fn two_pollers_on_one_store_send_one_needs_complete_nudge() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let path = tempfile::tempdir().unwrap().keep().join("t.db");
        let store_a = std::sync::Arc::new(Store::open(path.clone()).unwrap());
        let store_b = std::sync::Arc::new(Store::open(path).unwrap());
        let s = check_criteria_session("orch1");
        store_a.upsert_session(&s).unwrap();
        let poller_a = Poller::new(Engine::new(store_a.clone()));
        let poller_b = Poller::new(Engine::new(store_b.clone()));

        let (a, b) = tokio::join!(
            poller_a.handle_merge_detection(&s, 7, true, true, "abc123"),
            poller_b.handle_merge_detection(&s, 7, true, true, "abc123"),
        );
        assert!(a && b);
        assert_eq!(needs_complete_nudges(&store_a, "orch1"), 1);
    }

    /// The same race on the plain done-reaction path.
    #[tokio::test]
    async fn two_pollers_on_one_store_send_one_done_reaction() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let path = tempfile::tempdir().unwrap().keep().join("t.db");
        let store_a = std::sync::Arc::new(Store::open(path.clone()).unwrap());
        let store_b = std::sync::Arc::new(Store::open(path).unwrap());
        let mut s = test_session("w1", "/ws");
        s.orchestrator_id = Some("orch1".into());
        s.status = SessionStatus::Mergeable;
        store_a.upsert_session(&s).unwrap();
        let poller_a = Poller::new(Engine::new(store_a.clone()));
        let poller_b = Poller::new(Engine::new(store_b.clone()));

        // `[auto_reap]` off: kept alive, so the stamp is the only dedup.
        tokio::join!(
            poller_a.handle_merge_detection(&s, 7, true, false, "abc123"),
            poller_b.handle_merge_detection(&s, 7, true, false, "abc123"),
        );
        let merged_msgs = store_a.message_events_to("orch1", 0).unwrap()
            .iter().filter(|m| m.text.contains("merged")).count();
        assert_eq!(merged_msgs, 1);
    }

    /// A second poller holding a pre-merge snapshot arrives after the first
    /// has fully reaped the session: the row is `Done` with no stamp, and it
    /// must not be claimed (and notified) again.
    #[tokio::test]
    async fn a_stale_snapshot_cannot_reclaim_a_reaped_session() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let path = tempfile::tempdir().unwrap().keep().join("t.db");
        let store_a = std::sync::Arc::new(Store::open(path.clone()).unwrap());
        let store_b = std::sync::Arc::new(Store::open(path).unwrap());
        let mut s = test_session("w1", "/ws");
        s.orchestrator_id = Some("orch1".into());
        store_a.upsert_session(&s).unwrap();
        let poller_a = Poller::new(Engine::new(store_a.clone()));
        let poller_b = Poller::new(Engine::new(store_b.clone()));

        poller_a.handle_merge_detection(&s, 7, true, true, "abc123").await;
        let after = store_a.get_session("w1").unwrap().unwrap();
        assert!(matches!(after.status, SessionStatus::Done));
        assert!(after.merged_at.is_none(), "a reaped row carries no stamp");

        poller_b.handle_merge_detection(&s, 7, true, true, "abc123").await;
        let merged_msgs = store_a.message_events_to("orch1", 0).unwrap()
            .iter().filter(|m| m.text.contains("merged")).count();
        assert_eq!(merged_msgs, 1);
    }

    /// A failed stamp must send nothing — otherwise it turns into the spam
    /// loop this gate exists to prevent.
    #[tokio::test]
    async fn needs_complete_nudge_is_not_sent_when_the_stamp_fails() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let path = tempfile::tempdir().unwrap().keep().join("t.db");
        let store = std::sync::Arc::new(Store::open(path.clone()).unwrap());
        let s = check_criteria_session("orch1");
        store.upsert_session(&s).unwrap();
        rusqlite::Connection::open(&path).unwrap()
            .execute_batch("ALTER TABLE sessions RENAME TO sessions_gone").unwrap();
        let poller = Poller::new(Engine::new(store.clone()));

        assert!(poller.handle_merge_detection(&s, 7, true, true, "abc123").await);
        assert_eq!(needs_complete_nudges(&store, "orch1"), 0);
    }

    /// Once the worker's own `ninox complete` already told this exact
    /// orchestrator about this exact unit of work, `handle_merge_detection`'s
    /// code-level done reaction would just be a redundant, confirming echo —
    /// suppress it, but still perform cleanup.
    #[tokio::test]
    async fn merge_detection_suppresses_the_done_reaction_after_a_completion_was_sent() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;
        use crate::types::MessageKind;

        let orch_id = format!(
            "orch-dedup-done-test-{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
        );
        let orch_ws = tempfile::tempdir().unwrap().keep();
        crate::tmux::create_session(&orch_id, orch_ws.to_str().unwrap(), "sleep 30", &[]).await.unwrap();

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut s = test_session("w1", "/ws");
        s.orchestrator_id = Some(orch_id.clone());
        s.started_at = 0;
        store.upsert_session(&s).unwrap();
        store.record_message_event(Some("w1"), &orch_id, "all done here", MessageKind::Completion).unwrap();

        let poller = Poller::new(Engine::new(store.clone()));
        assert!(poller.handle_merge_detection(&s, 7, true, true, "abc123").await);

        let after = store.get_session("w1").unwrap().unwrap();
        assert!(matches!(after.status, SessionStatus::Done), "cleanup must still happen");

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let screen = crate::tmux::capture_screen(&orch_id, 0, false).await.unwrap_or_default();
        assert!(!screen.contains("merged."), "the done reaction must be suppressed, got: {screen:?}");

        crate::tmux::kill_session(&orch_id).await.ok();
    }

    /// The fallback case: with no prior completion from the worker, the
    /// code-level done reaction still fires exactly as before.
    #[tokio::test]
    async fn merge_detection_sends_the_done_reaction_with_no_prior_completion() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let orch_id = format!(
            "orch-dedup-done-fallback-test-{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
        );
        let orch_ws = tempfile::tempdir().unwrap().keep();
        crate::tmux::create_session(&orch_id, orch_ws.to_str().unwrap(), "sleep 30", &[]).await.unwrap();

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut s = test_session("w1", "/ws");
        s.orchestrator_id = Some(orch_id.clone());
        store.upsert_session(&s).unwrap();

        let poller = Poller::new(Engine::new(store.clone()));
        assert!(poller.handle_merge_detection(&s, 7, true, true, "abc123").await);

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let screen = crate::tmux::capture_screen(&orch_id, 0, false).await.unwrap_or_default();
        assert!(screen.contains("merged."), "the done reaction must still fire, got: {screen:?}");

        crate::tmux::kill_session(&orch_id).await.ok();
    }

    /// The dedup check keys off the `message_events.kind` column, never the
    /// message TEXT — a `freeform` message whose body happens to contain the
    /// literal substring `[completion]` (e.g. a human quoting or discussing
    /// one) must never be mistaken for a real completion and suppress the
    /// fallback reaction. Security review on PR #274.
    #[tokio::test]
    async fn a_freeform_message_containing_completion_text_does_not_suppress_the_fallback() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;
        use crate::types::MessageKind;

        let orch_id = format!(
            "orch-dedup-text-spoof-test-{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
        );
        let orch_ws = tempfile::tempdir().unwrap().keep();
        crate::tmux::create_session(&orch_id, orch_ws.to_str().unwrap(), "sleep 30", &[]).await.unwrap();

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut s = test_session("w1", "/ws");
        s.orchestrator_id = Some(orch_id.clone());
        s.started_at = 0;
        store.upsert_session(&s).unwrap();
        store.record_message_event(
            Some("w1"), &orch_id, "[completion] all done here", MessageKind::Freeform,
        ).unwrap();

        let poller = Poller::new(Engine::new(store.clone()));
        assert!(poller.handle_merge_detection(&s, 7, true, true, "abc123").await);

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let screen = crate::tmux::capture_screen(&orch_id, 0, false).await.unwrap_or_default();
        assert!(
            screen.contains("merged."),
            "a freeform message's TEXT must never be mistaken for a real completion, got: {screen:?}"
        );

        crate::tmux::kill_session(&orch_id).await.ok();
    }

    /// A session past the retention window is purged; one still within the
    /// window survives. Time is injected via `terminal_at` (computed off the
    /// real clock, offset by the retention window) rather than sleeping, and
    /// the retention config is passed in directly rather than read from
    /// `AppConfig::load()` — mirroring `sync_sessions_metadata`'s pattern of
    /// taking its directory as a parameter so tests can control it exactly.
    #[tokio::test]
    async fn sweep_retired_sessions_purges_only_past_the_retention_window() {
        let _iso = crate::test_isolation::isolate();
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

    /// One repo root deleted from disk between discovery and sweep (or just
    /// never a real git repo) must not stop the sweep from still reaching
    /// every other repo's orphans.
    #[tokio::test]
    async fn orphan_sweep_tick_logs_and_does_not_touch_other_repos_on_one_repo_error() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;
        use crate::workspace::{write_marker, WorkspaceMarker};

        let repo_a = tempfile::tempdir().unwrap();
        let run_git = |args: &[&str]| {
            let status = std::process::Command::new("git").args(args).current_dir(repo_a.path()).status().unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run_git(&["init", "-q", "-b", "main"]);
        run_git(&["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "--allow-empty", "-q", "-m", "init"]);

        let live_wt = repo_a.path().join(".claude/worktrees/live-one");
        run_git(&["worktree", "add", "-q", "-b", "live-one", live_wt.to_str().unwrap()]);
        let orphan_wt = repo_a.path().join(".claude/worktrees/orphan-one");
        run_git(&["worktree", "add", "-q", "-b", "orphan-one", orphan_wt.to_str().unwrap()]);
        write_marker(&orphan_wt, &WorkspaceMarker {
            session_id: "orphan-one".into(), repo: repo_a.path().to_string_lossy().into(),
            branch: "orphan-one".into(), created_at: 0,
        }).unwrap();

        // A second repo root, registered via a session row, then deleted
        // from disk entirely before the sweep runs.
        let repo_b = tempfile::tempdir().unwrap().keep();
        let ghost_path = repo_b.join(".claude/worktrees/ghost");

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&test_session("live-one", live_wt.to_str().unwrap())).unwrap();
        store.upsert_session(&test_session("ghost", ghost_path.to_str().unwrap())).unwrap();
        std::fs::remove_dir_all(&repo_b).unwrap();

        let engine = Engine::new(store.clone());
        let poller = Poller::new(engine);
        let cfg = crate::config::OrphanSweepConfig { enabled: true, interval_hours: 1 };
        let retention = SessionRetentionConfig::default();

        poller.run_orphan_sweep(&cfg, &retention).await;

        assert!(!orphan_wt.exists(), "repo_a's orphan must still be cleaned despite repo_b being gone");
        assert!(live_wt.exists(), "a live worktree must never be touched");
    }

    #[tokio::test]
    async fn orphan_sweep_tick_is_a_noop_when_disabled() {
        use crate::store::Store;
        use crate::workspace::{write_marker, WorkspaceMarker};

        let repo = tempfile::tempdir().unwrap();
        let run_git = |args: &[&str]| {
            let status = std::process::Command::new("git").args(args).current_dir(repo.path()).status().unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run_git(&["init", "-q", "-b", "main"]);
        run_git(&["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "--allow-empty", "-q", "-m", "init"]);
        let live_wt = repo.path().join(".claude/worktrees/live-one");
        run_git(&["worktree", "add", "-q", "-b", "live-one", live_wt.to_str().unwrap()]);
        let orphan_wt = repo.path().join(".claude/worktrees/orphan-one");
        run_git(&["worktree", "add", "-q", "-b", "orphan-one", orphan_wt.to_str().unwrap()]);
        write_marker(&orphan_wt, &WorkspaceMarker {
            session_id: "orphan-one".into(), repo: repo.path().to_string_lossy().into(),
            branch: "orphan-one".into(), created_at: 0,
        }).unwrap();

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&test_session("live-one", live_wt.to_str().unwrap())).unwrap();

        let engine = Engine::new(store.clone());
        let poller = Poller::new(engine);
        let cfg = crate::config::OrphanSweepConfig { enabled: false, interval_hours: 1 };

        poller.run_orphan_sweep(&cfg, &SessionRetentionConfig::default()).await;

        assert!(orphan_wt.exists(), "a disabled sweep must touch nothing");
    }

    /// Reconciliation stamps an un-resumable dead session `Terminated` with
    /// a `terminal_at`; under a manual restore policy it must still be
    /// there (worktree included) when the user gets round to restoring.
    #[tokio::test]
    async fn sweep_retired_sessions_keeps_unrestored_fleet_candidates() {
        let _iso = crate::test_isolation::isolate();
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
        let _iso = crate::test_isolation::isolate();
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
        let _iso = crate::test_isolation::isolate();
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

    /// The same dedup as `merge_detection_suppresses_the_done_reaction_...`,
    /// for the retired-notice path: a worker that already sent its own
    /// completion must not also get the generic retired notice.
    #[tokio::test]
    async fn sweep_retired_sessions_suppresses_the_retired_notice_after_a_completion_was_sent() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;
        use crate::types::MessageKind;

        let orch_id = format!(
            "orch-dedup-retired-test-{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
        );
        let orch_ws = tempfile::tempdir().unwrap().keep();
        crate::tmux::create_session(&orch_id, orch_ws.to_str().unwrap(), "sleep 30", &[]).await.unwrap();

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut s = test_session("w1", "/ws");
        s.status = SessionStatus::Terminated;
        s.orchestrator_id = Some(orch_id.clone());
        s.terminal_at = Some(0);
        s.started_at = 0;
        store.upsert_session(&s).unwrap();
        store.record_message_event(Some("w1"), &orch_id, "all done here", MessageKind::Completion).unwrap();

        let poller = Poller::new(Engine::new(store.clone()));
        poller.sweep_retired_sessions(&SessionRetentionConfig::default()).await;

        assert!(store.get_session("w1").unwrap().is_none(), "session must still be purged");

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let screen = crate::tmux::capture_screen(&orch_id, 0, false).await.unwrap_or_default();
        assert!(
            !screen.contains("was cleaned up and its session is gone"),
            "the retired notice must be suppressed, got: {screen:?}"
        );

        crate::tmux::kill_session(&orch_id).await.ok();
    }

    /// The fallback case: with no prior completion from the worker, the
    /// retired notice still fires exactly as before.
    #[tokio::test]
    async fn sweep_retired_sessions_sends_the_retired_notice_with_no_prior_completion() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let orch_id = format!(
            "orch-dedup-retired-fallback-test-{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
        );
        let orch_ws = tempfile::tempdir().unwrap().keep();
        crate::tmux::create_session(&orch_id, orch_ws.to_str().unwrap(), "sleep 30", &[]).await.unwrap();

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut s = test_session("w1", "/ws");
        s.status = SessionStatus::Terminated;
        s.orchestrator_id = Some(orch_id.clone());
        s.terminal_at = Some(0);
        store.upsert_session(&s).unwrap();

        let poller = Poller::new(Engine::new(store.clone()));
        poller.sweep_retired_sessions(&SessionRetentionConfig::default()).await;

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let screen = crate::tmux::capture_screen(&orch_id, 0, false).await.unwrap_or_default();
        assert!(
            screen.contains("was cleaned up and its session is gone"),
            "the retired notice must still fire, got: {screen:?}"
        );

        crate::tmux::kill_session(&orch_id).await.ok();
    }

    /// A session that already went through `handle_merge_detection` (and so
    /// already got `format_worker_done_reaction`) must not be notified again
    /// when the retention sweep later purges it — the merge-detection
    /// notification is the one and only notice the orchestrator needs.
    #[tokio::test]
    async fn sweep_retired_sessions_does_not_double_notify_a_merge_detected_session() {
        let _iso = crate::test_isolation::isolate();
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
        assert!(poller.handle_merge_detection(&s, 7, true, true, "abc123").await);
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
        let _iso = crate::test_isolation::isolate();
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
        let _iso = crate::test_isolation::isolate();
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
        let _iso = crate::test_isolation::isolate();
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
        let _iso = crate::test_isolation::isolate();
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

    #[tokio::test]
    async fn reap_self_completed_sessions_cleans_up_a_marked_session() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let repo_dir = tempfile::tempdir().unwrap();
        let run_git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(repo_dir.path())
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run_git(&["init", "-q"]);
        run_git(&["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "--allow-empty", "-q", "-m", "init"]);

        let session_id = "self-completed-1";
        let worktree_path = repo_dir.path().join(".claude/worktrees").join(session_id);
        run_git(&["worktree", "add", "-q", worktree_path.to_str().unwrap()]);
        assert!(worktree_path.exists(), "test setup: worktree must exist before the sweep");

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut s = test_session(session_id, worktree_path.to_str().unwrap());
        s.status = SessionStatus::Done;
        s.self_completed_at = Some(now_millis());
        store.upsert_session(&s).unwrap();

        let engine = Engine::new(store.clone());
        let poller = Poller::new(engine);
        poller.reap_self_completed_sessions().await;

        assert!(!worktree_path.exists(), "the sweep must physically reap a self-completed session");
        let after = store.get_session(session_id).unwrap().unwrap();
        assert_eq!(after.self_completed_at, None, "the marker must be cleared once handled");
        assert_eq!(after.status, SessionStatus::Done);
    }

    #[tokio::test]
    async fn reap_self_completed_sessions_tolerates_an_already_deleted_session() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        // Exercises the per-session reap directly with an id the store has
        // never heard of — the same outcome as a session that vanished
        // (e.g. a human's `ninox reap --force`) between the sweep's own
        // `list_sessions()` snapshot and this call.
        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let engine = Engine::new(store);
        let poller = Poller::new(engine);
        poller.reap_one_self_completed_session("gone-before-sweep").await; // must not panic
    }

    fn git_repo_with_worktree(session_id: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let repo = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git").args(args).current_dir(repo.path()).status().unwrap().success();
            assert!(ok, "git {args:?} failed");
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "--allow-empty", "-q", "-m", "init"]);
        let wt = repo.path().join(".claude/worktrees").join(session_id);
        git(&["worktree", "add", "-q", "-b", session_id, wt.to_str().unwrap()]);
        (repo, wt)
    }

    /// The 31 Oct incident: `[auto_reap]` off, the worker's first PR merged,
    /// and it kept working. A live worker is never retention's to remove,
    /// however old its `merged_at` — even with a clean worktree.
    #[tokio::test]
    async fn sweep_never_retires_a_live_merged_worker_past_the_window() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let (_repo, wt) = git_repo_with_worktree("live-merged");
        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let window = SessionRetentionConfig::default().retention_millis();
        for status in [SessionStatus::Working, SessionStatus::Mergeable, SessionStatus::Interrupted] {
            let mut s = test_session("live-merged", wt.to_str().unwrap());
            s.status = status.clone();
            s.merged_at = Some(now_millis() - window - 1);
            store.upsert_session(&s).unwrap();

            let poller = Poller::new(Engine::new(store.clone()));
            poller.sweep_retired_sessions(&SessionRetentionConfig::default()).await;

            assert!(store.get_session("live-merged").unwrap().is_some(), "{status:?} row must survive retention");
            assert!(wt.exists(), "{status:?} worktree must survive retention");
        }
    }

    /// A terminal row whose worktree holds uncommitted work keeps both: the
    /// row is what keeps the worktree accounted for and visible.
    #[tokio::test]
    async fn sweep_keeps_the_row_when_cleanup_keeps_a_dirty_worktree() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let (_repo, wt) = git_repo_with_worktree("dirty-done");
        std::fs::write(wt.join("wip.rs"), "uncommitted").unwrap();
        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let window = SessionRetentionConfig::default().retention_millis();
        let mut s = test_session("dirty-done", wt.to_str().unwrap());
        s.status = SessionStatus::Terminated;
        s.orchestrator_id = Some("orch1".into());
        s.terminal_at = Some(now_millis() - window - 1);
        store.upsert_session(&s).unwrap();

        let engine = Engine::new(store.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);
        poller.sweep_retired_sessions(&SessionRetentionConfig::default()).await;
        poller.sweep_retired_sessions(&SessionRetentionConfig::default()).await;

        assert!(wt.join("wip.rs").exists(), "the dirty worktree must be kept");
        let row = store.get_session("dirty-done").unwrap().expect("the row of a kept worktree must survive");
        assert_eq!(row.status, SessionStatus::Terminated);
        assert_eq!(row.cleanup_result.as_deref(), Some("skipped: dirty"));
        let retired = drain_events(&mut rx).iter().filter(|e| matches!(
            e, Event::Notification(n) if n.kind == crate::types::NotificationKind::WorkerRetired
        )).count();
        assert_eq!(retired, 1, "the retired notice goes out once, not on every retry");

        std::fs::remove_file(wt.join("wip.rs")).unwrap();
        poller.sweep_retired_sessions(&SessionRetentionConfig::default()).await;
        assert!(store.get_session("dirty-done").unwrap().is_some(), "a kept row waits out the retry backoff");
        store.mark_retained("dirty-done", now_millis() - RETAINED_RETRY_MS - 1).unwrap();
        poller.sweep_retired_sessions(&SessionRetentionConfig::default()).await;
        assert!(store.get_session("dirty-done").unwrap().is_none(), "once clean, the kept row is reclaimed");
        assert!(!wt.exists());
    }

    // ── PR repo/branch attribution (MLOPS-5417) ──────────────────────────

    fn merged_status(number: u64, head_ref: &str) -> crate::github::PrStatus {
        crate::github::PrStatus {
            merged: true, state: "closed".into(), mergeable: None,
            title: "t".into(), number, head_sha: "abc".into(),
            head_ref: head_ref.into(), base_ref: "main".into(),
        }
    }

    fn worker_done_notifs(evs: &[Event]) -> usize {
        evs.iter().filter(|e| matches!(
            e, Event::Notification(n) if n.kind == crate::types::NotificationKind::WorkerDone
        )).count()
    }

    /// A PR whose URL names a different repo than the spawn workspace (the
    /// worker ran `gh pr create -R other/repo`) is recorded against *that* repo.
    #[tokio::test]
    async fn metadata_adoption_records_the_pr_url_repo_not_the_workspace_repo() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let sessions_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            sessions_dir.path().join("s1.json"),
            serde_json::json!({
                "agentReportedPrs": [{"number": "674", "url": "https://github.com/Org/Radisson/pull/674"}],
            }).to_string(),
        ).unwrap();
        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut s = test_session("s1", "/ws");
        s.repo = "Org/bellhop".into();
        store.upsert_session(&s).unwrap();
        let poller = Poller::new(Engine::new(store.clone()));

        poller.sync_sessions_metadata(sessions_dir.path()).await;

        let after = store.get_session("s1").unwrap().unwrap();
        assert_eq!(after.pr_number, Some(674));
        assert_eq!(after.repo, "Org/Radisson");
    }

    /// The incident: recorded PR (bellhop#674) is someone else's merged PR.
    /// With `[auto_reap]` on, the worker must survive, with no merge stamp and
    /// no WorkerDone notice — on both polling paths.
    #[tokio::test]
    async fn head_branch_mismatch_never_reaps_or_notifies_even_with_auto_reap() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        for batched in [false, true] {
            let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
            let mut s = pr_session("w1", "Org/bellhop", 674);
            s.status = SessionStatus::Working;
            store.upsert_session(&s).unwrap();
            store.set_session_branch("w1", "mlops-5607-shared-home-lease-contender").unwrap();

            let fake = std::sync::Arc::new(FakeGithub::default());
            fake.pr_status_ok.lock().unwrap().insert(
                ("Org".into(), "bellhop".into(), 674), merged_status(674, "mlops-4234-userns-sysctl-dev"),
            );
            let batch = std::sync::Arc::new(FakeBatchApi::default());
            let mut snap = open_snapshot(674);
            snap.status = merged_status(674, "mlops-4234-userns-sysctl-dev");
            batch.result.lock().unwrap().prs.insert(pr_key("Org/bellhop", 674), snap);
            let engine = Engine::new_with_github_apis(
                store.clone(),
                fake as std::sync::Arc<dyn crate::github::GithubApi>,
                batch as std::sync::Arc<dyn crate::github_graphql::GithubBatchApi>,
            );
            let mut rx = engine.subscribe();
            let poller = Poller::new(engine);

            if batched { poller.poll_github_batched(true).await } else { poller.poll_github(true).await }

            let after = store.get_session("w1").unwrap().unwrap();
            assert!(after.merged_at.is_none(), "batched={batched}: no merge state");
            assert!(!after.status.is_terminal(), "batched={batched}: worker must not be reaped");
            assert_eq!(worker_done_notifs(&drain_events(&mut rx)), 0, "batched={batched}: no merge notice");
        }
    }

    /// Unknown branch: the merge is recorded and noticed, but never reaps,
    /// even with `[auto_reap]` on and a terminal worker.
    #[tokio::test]
    async fn unknown_branch_records_merge_but_never_cleans_up() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let s = pr_session("w1", "Org/repo", 5);
        store.upsert_session(&s).unwrap();
        let poller = Poller::new(Engine::new(store.clone()));

        let handled = poller.handle_merge_detection_with(&s, 5, true, true, "abc", Attribution::Unknown).await;

        assert!(handled);
        let after = store.get_session("w1").unwrap().unwrap();
        assert!(after.merged_at.is_some(), "merge still recorded");
        assert!(!matches!(after.status, SessionStatus::Done), "no cleanup/reap");
        let msg = crate::lifecycle::reactions::format_merged_unverified_reaction(&s, 5);
        assert!(!msg.contains("--force") && !msg.contains("reap it"));
    }

    /// Existing misattributed row self-heals; a genuinely merged row with a
    /// matching (or unknown) branch is left stamped.
    #[tokio::test]
    async fn repair_clears_misattributed_merge_but_keeps_genuine_ones() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut bad = pr_session("bad", "Org/bellhop", 674);
        bad.merged_at = Some(1);
        store.upsert_session(&bad).unwrap();
        store.set_session_branch("bad", "mlops-5607-lease").unwrap();
        let mut good = pr_session("good", "Org/repo", 8);
        good.merged_at = Some(1);
        store.upsert_session(&good).unwrap();
        store.set_session_branch("good", "feat/good").unwrap();
        let mut unknown = pr_session("unk", "Org/repo", 9);
        unknown.merged_at = Some(1);
        store.upsert_session(&unknown).unwrap();

        let fake = std::sync::Arc::new(FakeGithub::default());
        {
            let mut m = fake.pr_status_ok.lock().unwrap();
            m.insert(("Org".into(), "bellhop".into(), 674), merged_status(674, "mlops-4234-userns"));
            m.insert(("Org".into(), "repo".into(), 8), merged_status(8, "feat/good"));
            m.insert(("Org".into(), "repo".into(), 9), merged_status(9, "feat/other"));
        }
        let poller = Poller::new(github_engine(store.clone(), fake));

        poller.repair_misattributed_merges().await;
        poller.repair_misattributed_merges().await;

        let bad = store.get_session("bad").unwrap().unwrap();
        assert!(bad.merged_at.is_none() && bad.pr_number.is_none() && bad.pr_id.is_none());
        assert!(!bad.status.is_terminal());
        assert!(store.get_session("good").unwrap().unwrap().merged_at.is_some(), "matching branch stays merged");
        assert!(store.get_session("unk").unwrap().unwrap().merged_at.is_some(), "unknown branch stays merged");
    }

    /// A worker on its default `{session_id}` branch, or a PR whose head is
    /// the recorded branch, is Verified; an open PR on a different branch is
    /// still enriched (not skipped) while only its merge handling is blocked.
    #[tokio::test]
    async fn attribution_accepts_session_id_branch_and_open_mismatch_keeps_enrichment() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let s = pr_session("w1", "Org/repo", 5);
        store.upsert_session(&s).unwrap();
        store.set_session_branch("w1", "follow-up").unwrap();
        let poller = Poller::new(Engine::new(store.clone()));
        assert_eq!(poller.merge_attribution(&s, "w1"), Attribution::Verified);
        assert_eq!(poller.merge_attribution(&s, "follow-up"), Attribution::Verified);
        assert_eq!(poller.merge_attribution(&s, "other"), Attribution::Mismatch);

        let mut open = open_snapshot(5);
        open.status.head_ref = "other".into();
        let fake = std::sync::Arc::new(FakeGithub::default());
        fake.pr_status_ok.lock().unwrap().insert(("Org".into(), "repo".into(), 5), open.status.clone());
        let poller = Poller::new(github_engine(store.clone(), fake));
        poller.poll_github(true).await;
        assert!(store.get_session("w1").unwrap().unwrap().pr_id == Some(5), "enrichment still ran");
    }

    /// `ninox complete`'s PrMerged check must not accept a foreign merged PR.
    #[tokio::test]
    async fn complete_rejects_merged_pr_from_a_foreign_branch() {
        let _iso = crate::test_isolation::isolate();
        let repo_dir = init_git_repo("worker-branch", &[("origin", "https://github.com/Org/repo.git")]);
        let mut s = pr_session("w1", "Org/repo", 5);
        s.workspace_path = Some(repo_dir.path().to_string_lossy().to_string());
        let fake = FakeGithub::default();
        fake.pr_status_ok.lock().unwrap().insert(("Org".into(), "repo".into(), 5), merged_status(5, "someone-elses"));
        let r = crate::lifecycle::completion::verify_criteria(&s, &[SuccessCriterion::PrMerged], &fake).await;
        assert!(r[0].1.is_err());
        fake.pr_status_ok.lock().unwrap().insert(("Org".into(), "repo".into(), 5), merged_status(5, "worker-branch"));
        let r = crate::lifecycle::completion::verify_criteria(&s, &[SuccessCriterion::PrMerged], &fake).await;
        assert!(r[0].1.is_ok());
    }

    /// The real incident end to end: recorded branch is the spawn branch, the
    /// worktree is gone, the reported URL names Radisson. bellhop#674 (no
    /// provenance) is repaired; once re-adopted, Radisson#674 merging with a
    /// differently-named head is Verified and handled normally.
    #[tokio::test]
    async fn provenance_verifies_the_real_pr_and_repair_still_clears_the_foreign_one() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let dir = AppConfig::sessions_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("w1.json"),
            serde_json::json!({
                "agentReportedPrs": [{"number": "674", "url": "https://github.com/Org/Radisson/pull/674"}],
            }).to_string(),
        ).unwrap();

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut s = pr_session("w1", "Org/bellhop", 674);
        s.workspace_path = Some("/deleted/worktree".into());
        s.merged_at = Some(1);
        store.upsert_session(&s).unwrap();
        store.set_session_branch("w1", "w1").unwrap();

        let fake = std::sync::Arc::new(FakeGithub::default());
        {
            let mut m = fake.pr_status_ok.lock().unwrap();
            m.insert(("Org".into(), "bellhop".into(), 674), merged_status(674, "mlops-4234-userns-sysctl-dev"));
            m.insert(("Org".into(), "Radisson".into(), 674), merged_status(674, "ethanbrodie/mlops-5607-shared-home-lease-contender"));
        }
        let poller = Poller::new(github_engine(store.clone(), fake));

        poller.repair_misattributed_merges().await;
        let healed = store.get_session("w1").unwrap().unwrap();
        assert!(healed.merged_at.is_none() && healed.pr_number.is_none(), "bellhop#674 has no provenance");

        // Re-adoption from the reported URL.
        poller.sync_sessions_metadata(&dir).await;
        let adopted = store.get_session("w1").unwrap().unwrap();
        assert_eq!((adopted.repo.as_str(), adopted.pr_number), ("Org/Radisson", Some(674)));

        assert_eq!(
            poller.merge_attribution(&adopted, "ethanbrodie/mlops-5607-shared-home-lease-contender"),
            Attribution::Verified,
        );
        poller.poll_github(false).await;
        let merged = store.get_session("w1").unwrap().unwrap();
        assert!(merged.merged_at.is_some(), "genuine merge handled, kept alive with auto_reap off");
    }

    /// The three genuine PRs from the first real-fleet run: terminated rows
    /// whose recorded branch is the stale spawn default, no metadata. They
    /// must not be repaired, and neither may any other terminal row.
    #[tokio::test]
    async fn repair_skips_terminal_rows_with_stale_spawn_default_branch() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let fake = std::sync::Arc::new(FakeGithub::default());
        let cases = [
            ("radisson-uvlock-keyring", "Org/Radisson", 666, "mlops-5577-uvlock-keyring"),
            ("dev-home-hydrate-core-pool", "Org/bellhop", 1160, "fix/mlops-5562-dev-home-jobs-pool"),
            ("session-image-python-venv", "Org/rnd-base-docker-images", 275, "mlops-5561-session-image-python-venv"),
            ("session-docker-network-docs", "Org/bellhop", 658, "mlops-4240-chained-cache-export"),
        ];
        for (id, repo, n, head) in cases {
            let mut s = pr_session(id, repo, n);
            s.status = SessionStatus::Terminated;
            s.terminal_at = Some(2);
            s.merged_at = Some(1);
            store.upsert_session(&s).unwrap();
            store.set_session_branch(id, id).unwrap();
            let (owner, r) = repo.split_once('/').unwrap();
            fake.pr_status_ok.lock().unwrap().insert((owner.into(), r.into(), n), merged_status(n, head));
        }
        let poller = Poller::new(github_engine(store.clone(), fake));

        poller.repair_misattributed_merges().await;

        for (id, _, n, _) in cases {
            let row = store.get_session(id).unwrap().unwrap();
            assert!(row.merged_at.is_some() && row.pr_number == Some(n), "{id} must keep its PR links");
        }
    }

    /// Each terminal signal alone (status, or `terminal_at`) skips repair,
    /// even when the recorded branch would otherwise prove a mismatch.
    #[tokio::test]
    async fn repair_skips_each_terminal_signal_independently() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let fake = std::sync::Arc::new(FakeGithub::default());
        for (id, status, terminal_at, n) in [
            ("t-status", SessionStatus::Terminated, None, 1u64),
            ("t-done", SessionStatus::Done, None, 2),
            ("t-at", SessionStatus::Working, Some(2i64), 3),
        ] {
            let mut s = pr_session(id, "Org/repo", n);
            s.status = status;
            s.terminal_at = terminal_at;
            s.merged_at = Some(1);
            store.upsert_session(&s).unwrap();
            store.set_session_branch(id, "feat/mine").unwrap();
            fake.pr_status_ok.lock().unwrap().insert(("Org".into(), "repo".into(), n), merged_status(n, "feat/other"));
        }
        let poller = Poller::new(github_engine(store.clone(), fake));

        poller.repair_misattributed_merges().await;

        for id in ["t-status", "t-done", "t-at"] {
            assert!(store.get_session(id).unwrap().unwrap().merged_at.is_some(), "{id} must be skipped");
        }
    }

    /// A live worker's stale spawn-default branch plus no metadata is no
    /// evidence of a foreign PR: repair leaves it alone.
    #[tokio::test]
    async fn repair_leaves_live_row_with_stale_spawn_default_branch() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut s = pr_session("w1", "Org/Radisson", 666);
        s.merged_at = Some(1);
        store.upsert_session(&s).unwrap();
        store.set_session_branch("w1", "w1").unwrap();
        let fake = std::sync::Arc::new(FakeGithub::default());
        fake.pr_status_ok.lock().unwrap().insert(("Org".into(), "Radisson".into(), 666), merged_status(666, "mlops-5577-uvlock-keyring"));
        let poller = Poller::new(github_engine(store.clone(), fake));

        poller.repair_misattributed_merges().await;

        assert!(store.get_session("w1").unwrap().unwrap().merged_at.is_some());
    }

    /// bellhop#658: the session reported Radisson#658, so the tracked
    /// bellhop PR is excluded by provenance even though its recorded branch
    /// is only the spawn default.
    #[tokio::test]
    async fn reported_urls_that_exclude_the_tracked_pr_are_a_mismatch() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let dir = AppConfig::sessions_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("session-docker-network-docs.json"),
            serde_json::json!({
                "agentReportedPrs": [{"number": "658", "url": "https://github.com/Org/Radisson/pull/658"}],
            }).to_string(),
        ).unwrap();
        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let s = pr_session("session-docker-network-docs", "Org/bellhop", 658);
        store.upsert_session(&s).unwrap();
        store.set_session_branch(&s.id, &s.id).unwrap();
        let poller = Poller::new(Engine::new(store.clone()));

        assert_eq!(poller.merge_attribution(&s, "mlops-4240-chained-cache-export"), Attribution::Mismatch);
    }

    /// An Interrupted worker is resumable, so a misattributed merge on it is
    /// still repaired.
    #[tokio::test]
    async fn repair_still_clears_interrupted_mismatch_row() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut s = pr_session("w1", "Org/bellhop", 674);
        s.status = SessionStatus::Interrupted;
        s.merged_at = Some(1);
        store.upsert_session(&s).unwrap();
        store.set_session_branch("w1", "mlops-5607-lease").unwrap();
        let fake = std::sync::Arc::new(FakeGithub::default());
        fake.pr_status_ok.lock().unwrap().insert(("Org".into(), "bellhop".into(), 674), merged_status(674, "mlops-4234-userns"));
        let poller = Poller::new(github_engine(store.clone(), fake));

        poller.repair_misattributed_merges().await;

        let row = store.get_session("w1").unwrap().unwrap();
        assert!(row.merged_at.is_none() && row.pr_number.is_none());
    }

    /// A live worker on a ticket-named branch the wrapper never recorded,
    /// no metadata, whose PR genuinely merged: Unknown, so the merge is
    /// recorded and the worker is neither reaped nor told to be.
    #[tokio::test]
    async fn live_worker_with_unrecorded_ticket_branch_records_merge_without_reap() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut s = pr_session("w1", "Org/Radisson", 666);
        s.workspace_path = Some("/deleted/worktree".into());
        store.upsert_session(&s).unwrap();
        store.set_session_branch("w1", "w1").unwrap();
        let fake = std::sync::Arc::new(FakeGithub::default());
        fake.pr_status_ok.lock().unwrap().insert(("Org".into(), "Radisson".into(), 666), merged_status(666, "mlops-5577-uvlock-keyring"));
        let engine = github_engine(store.clone(), fake);
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        assert_eq!(poller.merge_attribution(&s, "mlops-5577-uvlock-keyring"), Attribution::Unknown);
        poller.poll_github(true).await;

        let after = store.get_session("w1").unwrap().unwrap();
        assert!(after.merged_at.is_some(), "merge recorded");
        assert!(!after.status.is_terminal(), "worker must not be reaped");
        assert_eq!(worker_done_notifs(&drain_events(&mut rx)), 1, "plain merge notice sent");
    }

    /// Unknown attribution with a not-auto-reapable worker must not send the
    /// "run `ninox complete`" nudge; it falls to the plain stamped-merge path.
    #[tokio::test]
    async fn unknown_attribution_skips_the_complete_nudge_for_check_criteria() {
        let _iso = crate::test_isolation::isolate();
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut s = pr_session("w1", "Org/repo", 5);
        s.success_criteria = vec![SuccessCriterion::Check("verify".into())];
        store.upsert_session(&s).unwrap();
        let poller = Poller::new(Engine::new(store.clone()));

        poller.handle_merge_detection_with(&s, 5, true, true, "abc", Attribution::Unknown).await;

        assert!(store.get_session("w1").unwrap().unwrap().merged_at.is_some(), "stamped, so no per-tick re-nudge");
    }

    /// `ninox complete` accepts a merged PR the session reported creating even
    /// when its head branch differs from the live checkout.
    #[tokio::test]
    async fn complete_accepts_a_reported_pr_despite_a_different_head_branch() {
        let _iso = crate::test_isolation::isolate();
        let dir = AppConfig::sessions_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("w1.json"),
            serde_json::json!({"agentReportedPrs": [{"number": "5", "url": "https://github.com/Org/repo/pull/5"}]}).to_string(),
        ).unwrap();
        let repo_dir = init_git_repo("spawn-branch", &[("origin", "https://github.com/Org/repo.git")]);
        let mut s = pr_session("w1", "Org/repo", 5);
        s.workspace_path = Some(repo_dir.path().to_string_lossy().to_string());
        let fake = FakeGithub::default();
        fake.pr_status_ok.lock().unwrap().insert(("Org".into(), "repo".into(), 5), merged_status(5, "user/other-name"));
        let r = crate::lifecycle::completion::verify_criteria(&s, &[SuccessCriterion::PrMerged], &fake).await;
        assert!(r[0].1.is_ok());
    }
}
