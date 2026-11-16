use crate::{github::{GitHubClient, GithubApi}, runtime::SessionBackend, store::Store, types::*};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{broadcast, Mutex};

mod delete;
pub use delete::{undeletable_reason, DeleteOutcome, DeletePlan, DeleteTarget, WorktreeState};

#[derive(Debug, Clone)]
pub enum Event {
    OrchestratorSpawned(Orchestrator),
    OrchestratorRemoved(OrchestratorId),
    SessionUpdated(Session, SessionFields),
    SessionSpawned(Session),
    SessionDone(SessionId),
    TerminalOutput { session_id: SessionId, bytes: Vec<u8> },
    /// Rendering stream from an attached tmux client (AttachedClient).
    /// `generation` identifies which `AttachedClient::spawn` call produced
    /// this event — consumers must ignore events whose generation doesn't
    /// match the currently-live client for `session_id` so a stale client
    /// (superseded by a fresh attach) cannot clobber the new one.
    ClientOutput   { session_id: SessionId, generation: u64, bytes: Vec<u8> },
    /// The attached tmux client process exited (detach, kill, server gone).
    /// See `ClientOutput` for why `generation` matters.
    ClientClosed   { session_id: SessionId, generation: u64 },
    CiUpdated      { pr_id: PrId, status: CIStatus },
    PrOpened       { session_id: SessionId, pr: PR },
    /// A PR beyond the session's tracked one (`PrOpened`'s `pr`) was
    /// detected — most often an agent accidentally opening a second PR.
    /// Unlike `PrOpened`, this must never change which PR a session tracks;
    /// it only makes the extra PR visible (e.g. on the Pull Requests
    /// ledger) so a human notices and can clean it up. `PR::session_id`
    /// carries the owning session — no separate field needed.
    ExtraPrDetected(PR),
    ReviewComment  { pr_id: PrId, comment: Comment },
    /// GitHub activity delivered to `session_id`, which owns or watches
    /// `repo#number` (see `crate::pr_events`).
    PrEvents { session_id: SessionId, repo: String, number: u64, events: Vec<crate::pr_events::PrEvent> },
    Notification(Notification),
    /// `count` more messages reached `session_id` since the poller's last
    /// look at `Store::message_delivered_counts`. The UI accumulates these
    /// into the sidebar's unread badge for sessions the user has not opened.
    MessagesDelivered { session_id: SessionId, count: u64 },
    /// Messages held per target, as (waiting for a queued session to start,
    /// unreachable and retrying), and per sender queued on an unreachable
    /// target. Emitted when either changes.
    HeldMessages {
        held:      HashMap<SessionId, (usize, usize)>,
        by_sender: HashMap<SessionId, usize>,
    },
    /// A live structured update from a non-pty backend (`AcpBackend`) — the
    /// counterpart to `TerminalOutput` for a session with no raw terminal
    /// bytes. See `SessionBackend::transcript_history` for the on-load
    /// equivalent of this stream.
    SessionTranscript { session_id: SessionId, entry: crate::runtime::TranscriptEntry },
}

/// Which of an orchestrator's workers [`Engine::reap_workers`] acts on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ReapSelection<'a> {
    /// Exactly these session ids, in this order. Ids that aren't this
    /// orchestrator's workers come back as [`ReapOutcome::NotFound`].
    Ids(&'a [String]),
    /// Every worker of this orchestrator. Anything not already finished
    /// (live or resumable) still needs `force` to actually be reaped.
    All,
    /// Default: only workers that are finished for good (`Done`/
    /// `Terminated`). Deliberately excludes `Interrupted` — see
    /// [`is_finished`].
    Finished,
}

/// What a reap did to one selected worker.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ReapOutcome {
    /// Cleaned up after a worker that had already finished for good.
    Reaped,
    /// Killed a worker that was still running (needs `force`).
    ReapedLive,
    /// Killed a worker whose PR already merged but was kept alive for
    /// post-merge validation (`[auto_reap]` off — see `Session::merged_at`).
    /// Still "live" in the reap sense (needs `force`), but this is the
    /// documented happy-path reap, not an interruption of unfinished work.
    ReapedLiveMerged,
    /// Cleaned up an `Interrupted` worker, giving up the workspace its
    /// resume depended on (needs `force`).
    ReapedResumable,
    /// Still running and `force` wasn't given — nothing was touched.
    SkippedLive,
    /// PR merged and kept alive for validation (`[auto_reap]` off), and
    /// `force` wasn't given — nothing was touched. Distinct from
    /// `SkippedLive` so the report can say "reap when validation's done"
    /// rather than "you'd interrupt work in progress".
    SkippedLiveMerged,
    /// `Interrupted`, i.e. resumable, and `force` wasn't given — nothing was
    /// touched.
    SkippedResumable,
    /// The session was killed and its worktree removed, but the store write
    /// marking it terminal failed. The worker is gone; the record still says
    /// otherwise, so it needs a human (or a retry) to reconcile.
    CleanedButNotRecorded,
    /// Not one of this orchestrator's workers (wrong orchestrator, already
    /// purged, or a typo).
    NotFound,
}

/// Finished for good: no live process to kill, nothing left to resume, and
/// `sweep_retired_sessions` will purge the record once the retention window
/// elapses.
///
/// `Interrupted` is deliberately NOT included even though it has no live
/// process either. It means "resumable" — a `claude_session_id` plus a
/// harness that can `--resume` — and the whole point of the status is that
/// the user can pick that exact conversation back up. Reaping destroys the
/// workspace `--resume` needs (uncommitted work included), and the sweep
/// never purges an `Interrupted` record, so treating it as finished would
/// make a bare `ninox reap` after a reboot — precisely when startup
/// reconciliation has marked every worker `Interrupted` — silently shred
/// resumable work and leave cards on the board forever.
fn is_finished(status: &SessionStatus) -> bool {
    matches!(status, SessionStatus::Done | SessionStatus::Terminated)
}

/// A finished worker with no cleanup recorded since it last finished —
/// what a default `ninox reap` selects. A cleanup stamped before a later
/// `terminal_at` predates a resume, so it doesn't count.
pub fn awaits_reap(session: &Session) -> bool {
    is_finished(&session.status)
        && match (session.cleanup_at, session.terminal_at) {
            (None, _) => true,
            (Some(cleaned), Some(ended)) => ended > cleaned,
            (Some(_), None) => false,
        }
}

pub struct Engine {
    pub store: Arc<Store>,
    tx: broadcast::Sender<Event>,
    pty_writers:  Mutex<HashMap<SessionId, tokio::sync::mpsc::UnboundedSender<Vec<u8>>>>,
    /// Per-session cancellation senders for active FIFO reader tasks.
    /// Sending () to the stored sender stops the running reader immediately.
    stream_cancel: Mutex<HashMap<SessionId, tokio::sync::oneshot::Sender<()>>>,
    /// Optional GitHub API client. None when no token is configured.
    pub github: Option<Arc<dyn GithubApi>>,
    /// Optional GitHub batch GraphQL client. None when no token is configured.
    pub github_batch: Option<Arc<dyn crate::github_graphql::GithubBatchApi>>,
    /// The one `AcpBackend` instance this process holds for its whole
    /// lifetime — see the cross-harness-messaging design spec's "Connection
    /// ownership" section. Unlike tmux/ptyd (reconstructed fresh per call by
    /// `Runtime::current()`, since their actual state lives in an external
    /// server/host process), an ACP connection's state — the live subprocess
    /// and its JSON-RPC pipe — lives only in this `AcpBackend` struct's own
    /// memory, so it must be the same `Arc` for every route/reaction that
    /// touches a given session for that session's whole life.
    pub acp: Arc<crate::runtime::AcpBackend>,
}

impl Engine {
    pub fn new(store: Arc<Store>) -> Arc<Self> {
        let (tx, _) = broadcast::channel(256);
        Arc::new(Self {
            store,
            tx,
            pty_writers:   Mutex::new(HashMap::new()),
            stream_cancel: Mutex::new(HashMap::new()),
            github:        None,
            github_batch:  None,
            acp:           Arc::new(crate::runtime::AcpBackend::new()),
        })
    }

    pub fn new_with_github(store: Arc<Store>, token: String) -> Arc<Self> {
        let (tx, _) = broadcast::channel(256);
        let github_batch = crate::github_graphql::GraphQlClient::new(token.clone()).ok()
            .map(|c| Arc::new(c) as Arc<dyn crate::github_graphql::GithubBatchApi>);
        let github = GitHubClient::new(token).ok()
            .map(|c| Arc::new(c) as Arc<dyn GithubApi>);
        Arc::new(Self {
            store,
            tx,
            pty_writers:   Mutex::new(HashMap::new()),
            stream_cancel: Mutex::new(HashMap::new()),
            github,
            github_batch,
            acp:           Arc::new(crate::runtime::AcpBackend::new()),
        })
    }

    /// Construct an `Engine` with a caller-supplied `GithubApi` — the
    /// dependency-injection seam tests use to drive the GitHub-enrichment
    /// poller against a fake instead of the real network.
    pub fn new_with_github_api(store: Arc<Store>, github: Arc<dyn GithubApi>) -> Arc<Self> {
        let (tx, _) = broadcast::channel(256);
        Arc::new(Self {
            store,
            tx,
            pty_writers:   Mutex::new(HashMap::new()),
            stream_cancel: Mutex::new(HashMap::new()),
            github:        Some(github),
            github_batch:  None,
            acp:           Arc::new(crate::runtime::AcpBackend::new()),
        })
    }

    /// Construct an `Engine` with caller-supplied `GithubApi` and
    /// `GithubBatchApi` — the dependency-injection seam tests use to drive
    /// the GitHub-enrichment poller against fakes instead of the real network.
    pub fn new_with_github_apis(
        store: Arc<Store>,
        github: Arc<dyn GithubApi>,
        batch: Arc<dyn crate::github_graphql::GithubBatchApi>,
    ) -> Arc<Self> {
        let (tx, _) = broadcast::channel(256);
        Arc::new(Self {
            store,
            tx,
            pty_writers:   Mutex::new(HashMap::new()),
            stream_cancel: Mutex::new(HashMap::new()),
            github:        Some(github),
            github_batch:  Some(batch),
            acp:           Arc::new(crate::runtime::AcpBackend::new()),
        })
    }

    /// Construct an `Engine` with a caller-supplied `AcpBackend` — the
    /// dependency-injection seam `ninox-server`'s own route tests use to
    /// drive ACP-connection-ownership behavior against a fake in-memory
    /// agent instead of a real `opencode` binary.
    pub fn new_with_acp(store: Arc<Store>, acp: Arc<crate::runtime::AcpBackend>) -> Arc<Self> {
        let (tx, _) = broadcast::channel(256);
        Arc::new(Self {
            store,
            tx,
            pty_writers:   Mutex::new(HashMap::new()),
            stream_cancel: Mutex::new(HashMap::new()),
            github:        None,
            github_batch:  None,
            acp,
        })
    }

    /// Cancel any running FIFO reader for `session_id` and return a fresh
    /// cancellation receiver for the new reader.  Call this at the top of
    /// every `start_streaming` invocation.
    pub async fn register_stream(
        &self,
        session_id: SessionId,
    ) -> tokio::sync::oneshot::Receiver<()> {
        let mut map = self.stream_cancel.lock().await;
        if let Some(old_tx) = map.remove(&session_id) {
            let _ = old_tx.send(());
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        map.insert(session_id, tx);
        rx
    }

    pub fn emit(&self, event: Event) {
        if let Event::Notification(n) = &event {
            if let Err(e) = self.store.insert_notification(n) {
                tracing::warn!("couldn't persist notification {}: {e}", n.id);
            }
        }
        let _ = self.tx.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    pub async fn register_pty_writer(
        &self,
        session_id: SessionId,
        writer: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    ) {
        self.pty_writers.lock().await.insert(session_id, writer);
    }

    pub async fn get_pty_writer(
        &self,
        session_id: &str,
    ) -> Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>> {
        self.pty_writers.lock().await.get(session_id).cloned()
    }

    /// `Store::sessions_by_orchestrator`'s result, minus any session that is
    /// itself a registered orchestrator. `orchestrator_id` now also points
    /// at a sub-orchestrator's parent (see the nested-orchestration design
    /// spec), so a raw `orchestrator_id == X` filter alone can no longer be
    /// trusted to mean "X's plain workers" — every call site that tears
    /// down, detaches, or reaps "an orchestrator's workers" must use this
    /// instead of the raw store query, or it will kill/remove a live,
    /// independently-tracked sub-orchestrator as if it were a disposable
    /// worker.
    fn plain_workers_of(&self, orchestrator_id: &str) -> anyhow::Result<Vec<Session>> {
        Ok(self.store.sessions_by_orchestrator(orchestrator_id)?
            .into_iter()
            .filter(|s| !self.store.is_orchestrator(&s.id).unwrap_or(false))
            .collect())
    }

    /// Kill all worker sessions belonging to an orchestrator, delete from DB, emit events.
    /// Queued spawns under `orchestrator_id` (and its own, if it is still
    /// queued) never started: removing the orchestrator drops them rather
    /// than letting them start later with no orchestrator to report to.
    fn cancel_queued_under(&self, orchestrator_id: &str) {
        let Ok(queue) = self.store.list_queue() else { return };
        for e in queue {
            if e.id == orchestrator_id || e.session.orchestrator_id.as_deref() == Some(orchestrator_id) {
                if let Ok(true) = self.store.cancel_queued(&e.id) {
                    if self.store.get_session(&e.id).ok().flatten().is_none() {
                        self.emit(Event::SessionDone(e.id));
                    }
                }
            }
        }
    }

    /// Removes the orchestrator and every plain worker that is safe to
    /// lose. Live workers are killed. A worker whose worktree cleanup is
    /// skipped (dirty, unpushed, stashed), and every `Interrupted` one
    /// (resumable), keeps its row and worktree and is detached into a
    /// standalone session instead; those are returned.
    pub async fn remove_orchestrator(&self, orchestrator_id: &str) -> anyhow::Result<Vec<KeptWorker>> {
        self.cancel_queued_under(orchestrator_id);
        let sessions_dir = crate::config::AppConfig::sessions_dir();
        let mut kept = Vec::new();
        let mut removed = Vec::new();
        for session in self.plain_workers_of(orchestrator_id)? {
            let session = self.store.get_session(&session.id)?.unwrap_or(session);
            if session.status == SessionStatus::Interrupted {
                self.store.detach_from_orchestrator(&session.id)?;
                kept.push(KeptWorker { id: session.id, reason: "interrupted (resumable)".into() });
                continue;
            }
            let _ = crate::runtime::kill_session(&session.id).await;
            let _ = self.acp.kill_session(&session.id).await;
            match self.clean_worker_or_keep(&session, &sessions_dir, orchestrator_id).await? {
                Some(reason) => kept.push(KeptWorker { id: session.id, reason }),
                None => removed.push(session.id),
            }
        }
        // Also kill the orchestrator's own tmux session (same id as orchestrator).
        let _ = crate::runtime::kill_session(orchestrator_id).await;
        let _ = self.acp.kill_session(orchestrator_id).await;
        self.store.delete_orchestrator(orchestrator_id)?;
        for id in removed {
            self.emit(Event::SessionDone(id));
        }
        self.emit_kept(&kept);
        self.emit(Event::OrchestratorRemoved(orchestrator_id.to_string()));
        Ok(kept)
    }

    /// [`Self::remove_orchestrator`] that leaves running workers alone: only
    /// ended workers lose their worktree and record, and each live one is
    /// detached into a standalone session. Returns the workers kept.
    pub async fn remove_orchestrator_keeping_live(&self, orchestrator_id: &str) -> anyhow::Result<Vec<KeptWorker>> {
        self.cancel_queued_under(orchestrator_id);
        let sessions_dir = crate::config::AppConfig::sessions_dir();
        let mut kept = Vec::new();
        for session in self.plain_workers_of(orchestrator_id)? {
            // Re-read: a worker resumed since the caller looked is live again.
            let session = self.store.get_session(&session.id)?.unwrap_or(session);
            // Interrupted is resumable (reboot, host death), so it is kept too.
            if !is_finished(&session.status) || crate::runtime::has_session(&session.id).await {
                self.store.detach_from_orchestrator(&session.id)?;
                let reason = if session.status == SessionStatus::Interrupted { "interrupted (resumable)" } else { "still running" };
                kept.push(KeptWorker { id: session.id, reason: reason.into() });
                continue;
            }
            let _ = crate::runtime::kill_session(&session.id).await;
            let _ = self.acp.kill_session(&session.id).await;
            if let Some(reason) = self.clean_worker_or_keep(&session, &sessions_dir, orchestrator_id).await? {
                kept.push(KeptWorker { id: session.id, reason });
                continue;
            }
            self.store.delete_session(&session.id)?;
            self.emit(Event::SessionDone(session.id));
        }
        let _ = crate::runtime::kill_session(orchestrator_id).await;
        let _ = self.acp.kill_session(orchestrator_id).await;
        self.store.delete_orchestrator(orchestrator_id)?;
        self.emit_kept(&kept);
        self.emit(Event::OrchestratorRemoved(orchestrator_id.to_string()));
        Ok(kept)
    }

    /// Re-sends each kept worker's row: a UI that dropped the orchestrator's
    /// workers optimistically puts the detached ones back.
    fn emit_kept(&self, kept: &[KeptWorker]) {
        for k in kept {
            if let Ok(Some(row)) = self.store.get_session(&k.id) {
                self.emit(Event::SessionUpdated(row, SessionFields::ALL));
            }
        }
    }

    /// Non-forced worktree cleanup for a worker whose orchestrator is being
    /// removed (its process already killed). `None` when the worktree is
    /// gone and the row may go with the orchestrator; otherwise the worker
    /// is detached, marked `Terminated` (it was killed), its cleanup result
    /// recorded, and the reason returned.
    async fn clean_worker_or_keep(
        &self, session: &Session, sessions_dir: &std::path::Path, orchestrator_id: &str,
    ) -> anyhow::Result<Option<String>> {
        let cleanup = self.run_cleanup(RunCleanupArgs {
            session,
            policy: crate::workspace::CleanupPolicy { force: false },
            merged: session.merged_at.is_some(),
            interrupted: false,
            live_session_row: false,
            remove_hook_artifacts: true,
        }, sessions_dir).await;
        let reason = match &cleanup {
            Ok(outcome) if !outcome.needs_attention() || !worktree_on_disk(session) => return Ok(None),
            Ok(outcome) => outcome.describe(),
            Err(e) => format!("cleanup failed: {e}"),
        };
        tracing::warn!("remove orchestrator {orchestrator_id}: kept worker {} and its worktree: {reason}", session.id);
        self.store.detach_from_orchestrator(&session.id)?;
        self.mark_reaped_terminal(session)?;
        if let Ok(outcome) = &cleanup {
            self.store.set_cleanup_result(&session.id, outcome)?;
        }
        self.store.mark_retained(&session.id, crate::lifecycle::poller::now_millis())?;
        Ok(Some(reason))
    }

    /// Reap an orchestrator's workers: kill each one's tmux session, remove
    /// its worktree and hook artifacts, and leave the store record in a
    /// terminal state with `terminal_at` stamped so the retention sweep
    /// (`lifecycle::poller::sweep_retired_sessions`) purges it on its normal
    /// schedule. Deliberately NOT `remove_session`: the fleet board keeps the
    /// card for the retention window, and the sweep stays the single place
    /// that deletes a session row.
    ///
    /// Selection (see [`ReapSelection`]) never reaches beyond `orchestrator_id`'s
    /// own workers — an id belonging to another orchestrator (or to no session
    /// at all) comes back as [`ReapOutcome::NotFound`] rather than being
    /// touched.
    ///
    /// `force` only relaxes the not-finished guard: without it, a selected
    /// worker that is still running or still resumable is reported as
    /// [`ReapOutcome::SkippedLive`]/[`ReapOutcome::SkippedResumable`] and
    /// left completely alone.
    pub async fn reap_workers(
        &self,
        orchestrator_id: &str,
        selection:       ReapSelection<'_>,
        force:           bool,
    ) -> anyhow::Result<Vec<(String, ReapOutcome)>> {
        let workers = self.plain_workers_of(orchestrator_id)?;

        // Explicit ids report per-id outcomes (including NotFound) in the
        // order the caller asked for them; the bulk selections report only
        // what they actually found, newest-first ordering left to the store.
        let selected: Vec<Result<Session, String>> = match selection {
            ReapSelection::Ids(ids) => ids
                .iter()
                .map(|id| {
                    workers
                        .iter()
                        .find(|w| w.id == *id)
                        .cloned()
                        .ok_or_else(|| id.clone())
                })
                .collect(),
            ReapSelection::All => workers.into_iter().map(Ok).collect(),
            ReapSelection::Finished => workers
                .into_iter()
                .filter(awaits_reap)
                .map(Ok)
                .collect(),
        };

        let mut outcomes = Vec::with_capacity(selected.len());
        for entry in selected {
            let session = match entry {
                Ok(s)   => s,
                Err(id) => {
                    outcomes.push((id, ReapOutcome::NotFound));
                    continue;
                }
            };
            // Re-read before deciding anything destructive. The worker list
            // was read once up front, and each iteration awaits a tmux kill
            // and a `git worktree remove` — so by the Nth worker the snapshot
            // is seconds old. A `Done`/`Terminated` worker the human has
            // since Re-filed or Resumed is `Working` again, with a fresh
            // worktree and a live agent in it; acting on the snapshot would
            // kill that agent and delete that worktree without `--force`
            // ever being consulted, then report it as "reaped (already
            // finished)". Gone from the store entirely → nothing to reap.
            let session = match self.store.get_session(&session.id) {
                Ok(Some(fresh)) => fresh,
                Ok(None)        => {
                    outcomes.push((session.id, ReapOutcome::NotFound));
                    continue;
                }
                // Read error: fall back to the snapshot rather than skipping,
                // matching `poller::update_live_session_row`'s tradeoff.
                Err(_)          => session,
            };

            // Three cases, and `force` gates the two destructive ones:
            // finished (nothing to lose), still running (kill it), and
            // `Interrupted` (no process, but reaping gives up the workspace
            // its `--resume` needs).
            let resumable = matches!(session.status, SessionStatus::Interrupted);
            let was_live  = !is_finished(&session.status) && !resumable;
            // A live worker whose PR already merged (kept alive for
            // validation, `[auto_reap]` off) reaps like any other live one,
            // but its report line should read as the intended cleanup, not
            // an interruption.
            let merged_alive = was_live && session.merged_at.is_some();
            // An explicitly-named `AwaitingReview` session is the
            // orchestrator finalizing a worker that already met its own
            // criteria and is waiting, live, for exactly this — allowed
            // without `--force` (see `SessionStatus::AwaitingReview`'s doc
            // comment). Bulk selections never get this bypass: `Finished`
            // already excludes it (not `is_finished`), and `All` must still
            // require deliberate `--force` for a session someone may be
            // mid-review on.
            let review_bypass = matches!(selection, ReapSelection::Ids(_))
                && session.status == SessionStatus::AwaitingReview;
            if !force && !review_bypass && (was_live || resumable) {
                let outcome = if resumable {
                    ReapOutcome::SkippedResumable
                } else if merged_alive {
                    ReapOutcome::SkippedLiveMerged
                } else {
                    ReapOutcome::SkippedLive
                };
                outcomes.push((session.id, outcome));
                continue;
            }

            // Unconditional, same reasoning as the other teardown paths in
            // this file: a live ACP connection or tmux pane is never left
            // gated behind the worktree-safety check below.
            let _ = crate::runtime::kill_session(&session.id).await;
            let _ = self.acp.kill_session(&session.id).await;
            // The worktree only — NOT the hook artifacts (remove_hook_artifacts:
            // false below). Those carry work requests and PR reports the
            // poller has not necessarily ingested yet: `deliver_work_requests`
            // runs on its own tick and deliberately fires for sessions that
            // have already finished ("a worker's last act is often 'request
            // follow-up, exit'"), which is exactly the worker an orchestrator
            // reaps first. Deleting them here silently dropped that follow-up
            // request, and dropped an unadopted PR report for good
            // (`poll_pr_reconciliation` skips terminal sessions and needs the
            // worktree we just removed). The retention sweep's `remove_session`
            // clears them once it purges the row, so nothing leaks by waiting.
            let cleanup = match self.run_cleanup(RunCleanupArgs {
                session: &session,
                policy: crate::workspace::CleanupPolicy { force },
                merged: session.merged_at.is_some(),
                interrupted: resumable,
                live_session_row: was_live,
                remove_hook_artifacts: false,
            }, &crate::config::AppConfig::sessions_dir()).await {
                Ok(outcome) => Some(outcome),
                Err(e) => {
                    tracing::warn!("reap {}: workspace cleanup: {e}", session.id);
                    None
                }
            };
            // Reaped on purpose: no later `ninox fleet restore` may bring it
            // back as a fresh restart.
            if let Err(e) = self.store.clear_interruption(&session.id) {
                tracing::warn!("reap {}: clear fleet interruption: {e}", session.id);
            }

            // Only a worker WE just made terminal gets a status write. An
            // already-finished one keeps the status it earned — relabelling a
            // `Terminated` worker `Done` would claim its PR merged, and
            // `sweep_retired_sessions` reads `Done` as "the orchestrator was
            // already told", suppressing the retired notice for a worker
            // whose work never landed — and keeps its `terminal_at` exactly
            // as it is. That second part matters as much as the first: a
            // `None` stamp is load-bearing, marking a terminal state reached
            // by direct user action ("purged on sight", see
            // `sweep_retired_sessions`), so stamping one here would put a
            // card the user just dismissed back on the board for the whole
            // retention window. Reaping must never extend a record's life.
            if was_live || resumable {
                // A live worker was just killed, and a reaped `Interrupted`
                // one can no longer be resumed (its workspace is gone) —
                // `Terminated` is the truth for both, and it's also the only
                // status besides `Done` the retention sweep ever purges.
                //
                // A store error here is reported per worker, never propagated:
                // every kill and removal above is already best-effort, so
                // bailing out of the whole call would throw away the report
                // for workers this loop has ALREADY killed and un-worktree'd,
                // leaving the orchestrator with no idea what happened.
                if let Err(e) = self.mark_reaped_terminal(&session) {
                    tracing::error!("reap {}: status write failed: {e}", session.id);
                    outcomes.push((session.id, ReapOutcome::CleanedButNotRecorded));
                    continue;
                }
            }
            // After the status write: its full-row upsert would otherwise
            // clobber this narrow stamp, which is what `awaits_reap` reads.
            if let Some(outcome) = cleanup {
                if let Err(e) = self.store.set_cleanup_result(&session.id, &outcome) {
                    tracing::warn!("reap {}: record cleanup result: {e}", session.id);
                }
            }
            outcomes.push((session.id, match (was_live, resumable, merged_alive) {
                (true, _, true) => ReapOutcome::ReapedLiveMerged,
                (true, _, _)    => ReapOutcome::ReapedLive,
                (_, true, _)    => ReapOutcome::ReapedResumable,
                _               => ReapOutcome::Reaped,
            }));
        }
        Ok(outcomes)
    }

    /// Flip a just-reaped worker to `Terminated` and start its retention
    /// countdown. Returns the row as written, or `None` when the session was
    /// deleted while the reap was working.
    ///
    /// Re-reads the row immediately before writing rather than reusing the
    /// caller's snapshot, mirroring `poller::update_live_session_row` and for
    /// the same two reasons — `upsert_session` is a FULL-ROW write, and by
    /// this point the reap has awaited a tmux kill and a `git worktree
    /// remove`:
    /// - The retention sweep may have purged the row in that window.
    ///   Writing the snapshot back would resurrect it, putting the card back
    ///   on the fleet board for a fresh retention window and earning the
    ///   orchestrator a second "worker retired" message.
    /// - The metadata/usage pollers, or an out-of-process `ninox statusline`,
    ///   may have written a fresher `pr_number`/`cost_usd`/`context_*`.
    ///   Writing the snapshot back would revert them — and a reverted
    ///   `pr_number` is unrecoverable, since `poll_pr_reconciliation` skips
    ///   `Terminated` sessions and needs the worktree the reap just deleted.
    ///
    /// A read *error* falls back to the snapshot, same as the poller: losing
    /// a statusline write is cheaper than losing the status update itself.
    ///
    /// Re-reading is only half the job: the fresh row may have reached a
    /// terminal state on its own in that window, and then it — not this reap —
    /// owns the outcome. Writing `Terminated` over a `Done` the poller just
    /// set (`handle_merge_detection`, which has already told the orchestrator
    /// the PR merged) would turn a merge into `sweep_retired_sessions`'
    /// "cleaned up without a detected PR merge" notice, contradicting what
    /// the orchestrator was told; re-stamping `terminal_at` over a stamp
    /// `poll_pids` just wrote would restart the retention countdown this very
    /// path exists to protect. So an already-finished fresh row is left
    /// completely alone. `Interrupted` is deliberately not "finished" here:
    /// the worktree its resume needed is gone by now, so `Terminated` really
    /// is the truth for it.
    fn mark_reaped_terminal(&self, snapshot: &Session) -> anyhow::Result<Option<Session>> {
        let mut row = match self.store.get_session(&snapshot.id) {
            Ok(Some(row)) => row,
            Ok(None)      => return Ok(None),
            Err(_)        => snapshot.clone(),
        };
        if is_finished(&row.status) {
            return Ok(None);
        }
        row.status      = SessionStatus::Terminated;
        row.terminal_at = Some(crate::lifecycle::poller::now_millis());
        self.store.upsert_session(&row)?;
        self.emit(Event::SessionUpdated(
            row.clone(), SessionFields::STATUS | SessionFields::TERMINAL_AT,
        ));
        Ok(Some(row))
    }

    /// Kill the tmux session and delete it from the DB entirely.
    ///
    /// `force` is the same override `ninox reap --force` uses: `true` for a
    /// direct, explicit user deletion (nothing left afterward to protect, so
    /// always force); `false` for the retention sweep's automatic purge,
    /// which must never destroy uncommitted or unpushed work. A non-forced
    /// call refuses a live row before killing anything, and when cleanup
    /// keeps the worktree it keeps the row too, so the worktree stays
    /// accounted for and visible ([`RemoveOutcome::Kept`]).
    pub async fn remove_session(&self, session_id: &str, force: bool) -> anyhow::Result<RemoveOutcome> {
        // A queued spawn has nothing to kill or clean up; a queued restore
        // still has its (Interrupted) row, removed below as usual.
        if self.store.cancel_queued(session_id).unwrap_or(false)
            && self.store.get_session(session_id).ok().flatten().is_none()
        {
            self.emit(Event::SessionDone(session_id.to_string()));
            return Ok(RemoveOutcome::Removed);
        }
        let row = self.store.get_session(session_id)?;
        if !force {
            if let Some(live) = row.as_ref().filter(|s| !is_finished(&s.status)) {
                return Ok(RemoveOutcome::Kept(format!("session is {:?}, not finished", live.status)));
            }
        }
        let _ = crate::runtime::kill_session(session_id).await;
        let _ = self.acp.kill_session(session_id).await;
        if let Some(session) = row {
            let cleanup = self.run_cleanup(RunCleanupArgs {
                session: &session,
                policy: crate::workspace::CleanupPolicy { force },
                merged: session.merged_at.is_some(),
                interrupted: matches!(session.status, crate::types::SessionStatus::Interrupted),
                live_session_row: false, // this session is the one being removed, not a bystander
                remove_hook_artifacts: true,
            }, &crate::config::AppConfig::sessions_dir()).await;
            let kept = match &cleanup {
                Ok(outcome) if outcome.needs_attention() && worktree_on_disk(&session) => Some(outcome.describe()),
                Ok(_) => None,
                Err(e) => {
                    tracing::warn!("remove_session {session_id}: workspace cleanup: {e}");
                    Some(format!("cleanup failed: {e}"))
                }
            };
            if let (false, Some(reason)) = (force, kept) {
                if let Ok(outcome) = &cleanup {
                    self.store.set_cleanup_result(session_id, outcome)?;
                }
                self.store.mark_retained(session_id, crate::lifecycle::poller::now_millis())?;
                if let Some(fresh) = self.store.get_session(session_id)? {
                    self.emit(Event::SessionUpdated(fresh, SessionFields::ALL));
                }
                return Ok(RemoveOutcome::Kept(reason));
            }
        } else {
            // No row to read facts from — still clear any hook artifacts an
            // id with no row might still have (matches the pre-unification
            // behavior for this edge case); the kills above already ran.
            crate::hooks::remove_session_artifacts(&crate::config::AppConfig::sessions_dir(), session_id);
        }
        self.store.delete_session(session_id)?;
        self.emit(Event::SessionDone(session_id.to_string()));
        Ok(RemoveOutcome::Removed)
    }

    /// Send a text message to a session (used by poller reactions).
    ///
    /// Delivered by the configured `AppConfig::send_mechanism()` — see
    /// `crate::messaging::deliver_message` for each mechanism and what it
    /// falls back to. Errors surface only when the chosen mechanism AND its
    /// keystroke fallback both fail: no active tmux window, tmux
    /// unavailable, or the message still sitting unsubmitted at the input
    /// prompt after the Enter retries.
    ///
    /// Delivered as `crate::messaging::SYSTEM_SENDER`: every caller here is
    /// a poller reaction generated by ninox's own engine, not a relay of
    /// text from some other session, so it is trusted rather than
    /// unverified.
    pub async fn send_to_session(&self, session_id: &str, message: &str) -> anyhow::Result<()> {
        self.send_to_session_with_kind(session_id, message, crate::types::MessageKind::System).await
    }

    /// One retry pass over messages held because their target was
    /// unreachable; see `messaging::retry_unreachable_messages`.
    pub async fn retry_unreachable_messages(&self, now: i64) -> Vec<crate::messaging::RetryOutcome> {
        let mechanism = crate::config::AppConfig::load_or_default().send_mechanism();
        crate::messaging::retry_unreachable_messages(
            &self.store, &crate::config::AppConfig::sessions_dir(), mechanism,
            &InProcessAcpRouter { acp: self.acp.clone() }, now,
        )
        .await
    }

    /// [`Self::send_to_session`] with an explicit [`crate::types::MessageKind`]
    /// — used by `lifecycle::poller::deliver_work_requests`, whose send is a
    /// `RequestForWork`, not the `System` every other poller reaction sends.
    pub async fn send_to_session_with_kind(
        &self, session_id: &str, message: &str, kind: crate::types::MessageKind,
    ) -> anyhow::Result<()> {
        // `AppConfig::load_or_default()` is a small synchronous TOML read;
        // called directly (not via `spawn_blocking`) here matches the
        // existing convention elsewhere in this codebase (e.g.
        // `main.rs::run_spawn`).
        let mechanism = crate::config::AppConfig::load_or_default().send_mechanism();
        // No `NINOX_DATA_DIR` fallback needed here (unlike the `ninox send`
        // CLI / `run_request_work`): this runs inside the app's own
        // process, which never has that env var set — only sessions the
        // app spawns do.
        crate::messaging::deliver_system_message_with_acp(
            &self.store, &crate::config::AppConfig::sessions_dir(), session_id, message, mechanism,
            kind, &InProcessAcpRouter { acp: self.acp.clone() },
        )
        .await
        .map(|_| ())
    }

    /// Kill the tmux session, mark it Terminated in the DB, and emit SessionUpdated.
    pub async fn terminate_session(&self, session_id: &str) -> anyhow::Result<()> {
        // Best-effort tmux kill (session may already be dead).
        let _ = crate::runtime::kill_session(session_id).await;
        // Best-effort: a no-op if this id never held an ACP connection.
        let _ = self.acp.kill_session(session_id).await;

        if let Some(mut session) = self.store.get_session(session_id)? {
            // Killed on purpose, so not a restore candidate — except an
            // `Interrupted` row, which this leaves as it is (resumable).
            if session.status != SessionStatus::Interrupted {
                self.store.clear_interruption(session_id)?;
            }
            // Never clobber a terminal status — most importantly, never flip
            // a `Done` session back to `Terminated`. `Done` is only ever
            // reached via `handle_merge_detection`, which already notified
            // the orchestrator; `sweep_retired_sessions`'s notification
            // dedup relies on `Done` meaning "already told" (see
            // `lifecycle::poller::sweep_retired_sessions`), so re-marking it
            // `Terminated` here would make the sweep send a second,
            // contradictory notice for a PR that was in fact merged.
            if matches!(
                session.status,
                SessionStatus::Done | SessionStatus::Terminated | SessionStatus::Interrupted
            ) {
                return Ok(());
            }
            session.status = SessionStatus::Terminated;
            self.store.upsert_session(&session)?;
            self.emit(Event::SessionUpdated(session, SessionFields::STATUS));
        }
        Ok(())
    }

    /// Kill the tmux session (best-effort), remove its worktree/artifacts,
    /// and mark it Done in the DB. Called automatically when a PR is
    /// merged. Emits SessionUpdated. Mirrors `remove_session`'s worktree
    /// and artifact cleanup so the record isn't deleted here but still
    /// stops leaking a checked-out worktree and per-session hook files.
    pub async fn cleanup_session(&self, session_id: &str) -> anyhow::Result<()> {
        self.cleanup_session_in(session_id, &crate::config::AppConfig::sessions_dir()).await
    }

    /// `cleanup_session`'s implementation, with the sessions dir as a
    /// parameter so tests can point artifact removal at a tempdir instead
    /// of the real `AppConfig::sessions_dir()`.
    async fn cleanup_session_in(
        &self,
        session_id:   &str,
        sessions_dir: &std::path::Path,
    ) -> anyhow::Result<()> {
        // Unconditional, best-effort: session may already be dead, and
        // never gated by the worktree-safety check `run_cleanup` applies
        // below (same reasoning as every other teardown path in this file).
        let _ = crate::runtime::kill_session(session_id).await;
        let _ = self.acp.kill_session(session_id).await;

        if let Some(mut session) = self.store.get_session(session_id)? {
            // This path only runs once `handle_merge_detection` has just
            // observed the merge, so `merged` is true even before the
            // status write below lands, and this is never a live/Interrupted
            // session being cleaned out from under someone else.
            let outcome = self.run_cleanup(RunCleanupArgs {
                session: &session,
                policy: crate::workspace::CleanupPolicy { force: false },
                merged: true,
                interrupted: false,
                live_session_row: false,
                remove_hook_artifacts: true,
            }, sessions_dir).await?;
            session.status = crate::types::SessionStatus::Done;
            session.terminal_at = Some(crate::lifecycle::poller::now_millis());
            self.store.upsert_session(&session)?;
            // After the full-row upsert above, not before: that upsert's
            // own `cleanup_result`/`cleanup_at` columns come from the
            // `session` snapshot read before `run_cleanup` ran (always
            // `None`), so writing the narrow result first would be
            // immediately overwritten by it (reviewer's Important 4).
            if let Err(e) = self.store.set_cleanup_result(session_id, &outcome) {
                tracing::warn!("cleanup_session {session_id}: record cleanup result: {e}");
            }
            self.emit(Event::SessionUpdated(session, SessionFields::STATUS | SessionFields::TERMINAL_AT));
        }
        Ok(())
    }

    /// The git facts `run_cleanup` judges `session`'s worktree by, and the
    /// branch they describe. Also what a delete preview shows, so the
    /// preview and the cleanup can't disagree about a worktree.
    pub(crate) fn cleanup_facts(&self, session: &Session, merged: bool) -> (crate::workspace::WorkspaceFacts, Option<String>) {
        use crate::workspace;

        let fleet = self.store.fleet_record(&session.id).ok().flatten();
        let branch = fleet.as_ref().and_then(|r| r.branch.clone());
        let merged_head_sha = fleet.as_ref().and_then(|r| r.merged_head_sha.clone());
        let workspace_path = session.workspace_path.clone().unwrap_or_default();
        let workspace_path_ref = std::path::Path::new(&workspace_path);

        // The recorded branch only vouches for itself — if a long-lived
        // session has since switched to a different one (a try-out worker
        // reusing one worktree per task item), `merged` must not bless
        // whatever's actually checked out now. Delete-target branch follows
        // the same preference: whatever the facts below actually describe.
        let effective_branch = workspace::effective_branch(workspace_path_ref, branch.as_deref());
        let merged_matches_effective = merged && effective_branch.as_deref() == branch.as_deref();

        let facts = match workspace::ninox_repo_root(&workspace_path, &session.id) {
            Some(repo_root) => {
                let repo_root = std::path::Path::new(&repo_root);
                let base_ref = workspace::resolve_default_base_ref(repo_root);
                let mut f = workspace::gather_git_facts(
                    repo_root,
                    workspace_path_ref,
                    branch.as_deref(),
                    base_ref.as_deref(),
                );
                f.merged = merged_matches_effective;
                if merged_matches_effective {
                    if let Some(b) = effective_branch.as_deref() {
                        // Prefer GitHub's own merged head sha (set by
                        // `handle_merge_detection` the moment it saw the
                        // merge) — a timestamp comparison can only ever see
                        // commits made after the *poller's own tick*
                        // noticed the merge, so one made in the gap between
                        // the real merge and that tick would predate
                        // `merged_at` and be missed.
                        f.commits_since_merge = merged_head_sha.as_deref()
                            .and_then(|sha| workspace::commits_since_sha(repo_root, b, sha))
                            .unwrap_or_else(|| {
                                // `cleanup_session_in` hardcodes `merged: true`
                                // the instant a fresh merge is detected,
                                // before `session.merged_at` itself is even
                                // written — "now" is the right cutoff there,
                                // since nothing can have landed after a
                                // moment that hasn't happened yet.
                                let since = session.merged_at.unwrap_or_else(crate::lifecycle::poller::now_millis);
                                workspace::commits_since(repo_root, b, since)
                            });
                    }
                }
                f
            }
            None => workspace::WorkspaceFacts {
                dirty: false, has_branch: false, has_upstream: false,
                unpushed_commit_count: 0, commits_beyond_base: 0,
                merged: false, commits_since_merge: 0, interrupted: false, live_session_row: false,
            },
        };
        (facts, effective_branch)
    }

    /// Shared `workspace::cleanup` plumbing for `cleanup_session_in`,
    /// `reap_workers`, `remove_session`, and orchestrator removal: resolves
    /// the session's recorded branch and repo root, gathers real git facts
    /// when the workspace is a ninox worktree, and layers in the
    /// session/store-derived facts the caller already knows (never
    /// re-derived here). Grouped into `RunCleanupArgs` to keep this under
    /// clippy's argument-count lint — each field is still named at every
    /// call site, so nothing is lost in translation.
    async fn run_cleanup(
        &self,
        args: RunCleanupArgs<'_>,
        sessions_dir: &std::path::Path,
    ) -> anyhow::Result<crate::workspace::CleanupOutcome> {
        use crate::workspace;

        let RunCleanupArgs { session, policy, merged, interrupted, live_session_row, remove_hook_artifacts } = args;
        let workspace_path = session.workspace_path.clone().unwrap_or_default();
        let (mut facts, effective_branch) = self.cleanup_facts(session, merged);
        facts.interrupted = interrupted;
        facts.live_session_row = live_session_row;

        workspace::cleanup(
            &session.id, &workspace_path, effective_branch.as_deref(), facts, policy,
            remove_hook_artifacts, sessions_dir,
        ).await
    }
}

/// A skipped cleanup only protects something while the worktree is still on
/// disk; cleanup never deletes a branch it skipped, so a row whose worktree
/// is already gone holds nothing back.
fn worktree_on_disk(session: &Session) -> bool {
    session.workspace_path.as_deref().is_some_and(|p| std::path::Path::new(p).is_dir())
}

/// A worker orchestrator removal left in place, detached, with why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeptWorker {
    pub id:     String,
    pub reason: String,
}

impl std::fmt::Display for KeptWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.id, self.reason)
    }
}

/// What [`Engine::remove_session`] did with the row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoveOutcome {
    Removed,
    /// Left in place, with why: a non-forced call found the session live,
    /// or its worktree cleanup was skipped or failed.
    Kept(String),
}

/// Named arguments for [`Engine::run_cleanup`] — see that method's doc for why.
struct RunCleanupArgs<'a> {
    session:               &'a Session,
    policy:                crate::workspace::CleanupPolicy,
    merged:                bool,
    interrupted:           bool,
    live_session_row:      bool,
    remove_hook_artifacts: bool,
}

/// [`crate::messaging::AcpRouter`] for `send_to_session`'s poller reactions,
/// which already run inside this same process (`ninox --headless` is both
/// the poller and `ninox-server` — see `daemon.rs`'s module doc comment) and
/// so can call straight into the held `AcpBackend`, with no HTTP hop. This
/// is the in-process counterpart to `acp_client`'s `HttpAcpRouter`, which a
/// separate, short-lived CLI process must use instead since it never holds
/// the connection itself.
struct InProcessAcpRouter {
    acp: Arc<crate::runtime::AcpBackend>,
}

#[async_trait::async_trait]
impl crate::messaging::AcpRouter for InProcessAcpRouter {
    /// Goes through [`crate::runtime::AcpBackend::send_keys_detached`]
    /// rather than awaiting a full turn inline: `send_to_session` is called
    /// from `lifecycle::poller`'s reaction loop as a plain inline `.await`,
    /// with no per-call `tokio::spawn` of its own, so blocking here for up
    /// to `PROMPT_TIMEOUT` (600s) would stall that entire poll tick — and
    /// every other session's reactions due that same tick — behind one
    /// slow agent turn.
    async fn send(&self, session_id: &str, message: &str) -> anyhow::Result<Option<()>> {
        if !self.acp.knows(session_id).await {
            return Ok(None);
        }
        use crate::runtime::DetachedSendOutcome;
        match self.acp.clone().send_keys_detached(session_id.to_string(), message.to_string(), crate::runtime::SEND_ACK_WINDOW).await {
            DetachedSendOutcome::Delivered | DetachedSendOutcome::StillRunning => Ok(Some(())),
            DetachedSendOutcome::Failed(err) => Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use tempfile::tempdir;

    #[tokio::test]
    async fn emit_received_by_subscriber() {
        let store = Arc::new(Store::open(tempdir().unwrap().keep().join("t.db")).unwrap());
        let engine = Engine::new(store);
        let mut rx = engine.subscribe();
        engine.emit(Event::SessionDone("s1".into()));
        let event = rx.recv().await.unwrap();
        assert!(matches!(event, Event::SessionDone(id) if id == "s1"));
    }

    #[tokio::test]
    async fn terminate_emits_session_updated() {
        let store = Arc::new(Store::open(tempdir().unwrap().keep().join("t.db")).unwrap());
        let session = crate::types::Session {
            id: "s1".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: crate::types::SessionStatus::Working,
            agent_type: "c".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
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
        };
        store.upsert_session(&session).unwrap();
        let engine = Engine::new(store);
        let mut rx = engine.subscribe();

        engine.terminate_session("s1").await.unwrap();

        let evt = rx.recv().await.unwrap();
        if let Event::SessionUpdated(s, _fields) = evt {
            assert!(matches!(s.status, crate::types::SessionStatus::Terminated));
            assert_eq!(
                s.terminal_at, None,
                "user-initiated terminate_session must not stamp terminal_at — \
                 that's reserved for the automatic lifecycle path, so this \
                 session is purged on sight rather than held for the \
                 retention grace period",
            );
        } else {
            panic!("expected SessionUpdated");
        }
    }

    pub(super) fn bare_session(id: &str, orchestrator_id: Option<&str>) -> crate::types::Session {
        crate::types::Session {
            id: id.to_string(), orchestrator_id: orchestrator_id.map(str::to_string), name: id.to_string(),
            repo: "r".into(), status: crate::types::SessionStatus::Working,
            agent_type: "c".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        }
    }

    /// `remove_orchestrator`'s worker list comes from `Store::
    /// sessions_by_orchestrator`, a pure `orchestrator_id == root` filter
    /// with no orchestrator/worker distinction — see the nested-
    /// orchestration design spec's lineage section, which reuses
    /// `orchestrator_id` for a sub-orchestrator's parent pointer too. A
    /// sub-orchestrator found this way must be left alone entirely (not
    /// killed, not reported as a torn-down worker), not swept up as if it
    /// were a plain worker.
    #[tokio::test]
    async fn remove_orchestrator_does_not_tear_down_a_sub_orchestrator() {
        let _iso = crate::test_isolation::isolate();
        let store = Arc::new(Store::open(tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_orchestrator(&crate::types::Orchestrator { id: "root".into(), name: "root".into(), created_at: 0 }).unwrap();
        store.upsert_session(&bare_session("root", None)).unwrap();
        store.upsert_session(&bare_session("worker-a", Some("root"))).unwrap();
        store.upsert_orchestrator(&crate::types::Orchestrator { id: "sub1".into(), name: "sub1".into(), created_at: 0 }).unwrap();
        store.upsert_session(&bare_session("sub1", Some("root"))).unwrap();

        let engine = Engine::new(store.clone());
        let mut rx = engine.subscribe();
        engine.remove_orchestrator("root").await.unwrap();

        let mut done_ids = Vec::new();
        while let Ok(evt) = rx.try_recv() {
            if let Event::SessionDone(id) = evt {
                done_ids.push(id);
            }
        }
        assert!(done_ids.contains(&"worker-a".to_string()), "a plain worker must still be torn down");
        assert!(
            !done_ids.contains(&"sub1".to_string()),
            "a sub-orchestrator must not be reported as a torn-down worker"
        );
        assert!(
            store.get_session("sub1").unwrap().is_some(),
            "a sub-orchestrator's session row must survive its parent's removal"
        );
    }

    /// Fake `opencode acp` agent that completes the handshake but then
    /// never answers a `session/prompt` at all — simulating a turn that
    /// legitimately runs long (up to `AcpBackend`'s `PROMPT_TIMEOUT`, 600s).
    fn fake_agent_that_never_replies_to_prompts() -> Vec<String> {
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

    /// Regression test for `InProcessAcpRouter::send` blocking
    /// `send_to_session` (and therefore the poller's own reaction loop,
    /// which calls it as a plain inline `.await` with no per-call
    /// `tokio::spawn`) for a full ACP turn. Bounding the wait at 5s makes a
    /// regression fail fast rather than hang the test suite for the full
    /// 600s `PROMPT_TIMEOUT`.
    #[tokio::test]
    async fn send_to_session_does_not_block_the_caller_for_the_whole_acp_turn() {
        let _iso = crate::test_isolation::isolate();
        let store = Arc::new(Store::open(tempdir().unwrap().keep().join("t.db")).unwrap());
        let session = crate::types::Session {
            id: "s1".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: crate::types::SessionStatus::Working,
            agent_type: "opencode".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
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
        };
        store.upsert_session(&session).unwrap();

        let acp = Arc::new(crate::runtime::AcpBackend::with_agent_command(fake_agent_that_never_replies_to_prompts()));
        let workspace = tempdir().unwrap().keep();
        acp.create_session("s1", workspace.to_str().unwrap(), "", &[]).await.unwrap();

        let engine = Engine::new_with_acp(store, acp);

        tokio::time::timeout(std::time::Duration::from_secs(5), engine.send_to_session("s1", "hello"))
            .await
            .expect("send_to_session must not block the poller's own loop for the full ACP turn")
            .unwrap();
    }

    /// A `Done` session (reached via `cleanup_session`, which already
    /// notified the orchestrator about the merge) must never be flipped back
    /// to `Terminated` by a later `terminate_session` call (e.g. the `DELETE
    /// /sessions/:id` route firing on a stale client, or a race with the
    /// poller). `sweep_retired_sessions`'s notification dedup relies on
    /// `Done` meaning "already told" — reopening it as `Terminated` would
    /// make the sweep send a second, contradictory notification claiming the
    /// PR was never detected merged.
    #[tokio::test]
    async fn terminate_session_does_not_reopen_a_done_session() {
        let store = Arc::new(Store::open(tempdir().unwrap().keep().join("t.db")).unwrap());
        let session = crate::types::Session {
            id: "s1".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: crate::types::SessionStatus::Done,
            agent_type: "c".into(), cost_usd: 0.0, started_at: 0,
            pr_number: Some(1), pr_id: Some(1), workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: Some(1_000), gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        store.upsert_session(&session).unwrap();
        let engine = Engine::new(Arc::clone(&store));
        let mut rx = engine.subscribe();

        engine.terminate_session("s1").await.unwrap();

        assert!(
            rx.try_recv().is_err(),
            "a no-op terminate_session on a Done session must not emit SessionUpdated"
        );
        let after = store.get_session("s1").unwrap().unwrap();
        assert!(matches!(after.status, crate::types::SessionStatus::Done), "status must stay Done");
        assert_eq!(after.terminal_at, Some(1_000), "terminal_at must be untouched");
    }

    fn awaiting_review_session(id: &str, orchestrator_id: &str) -> Session {
        Session {
            id: id.into(), orchestrator_id: Some(orchestrator_id.into()), name: id.into(),
            repo: String::new(), status: crate::types::SessionStatus::AwaitingReview,
            agent_type: "claude-code".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None, worker_type: None,
            success_criteria: Vec::new(), completion_outcome: None, self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        }
    }

    #[tokio::test]
    async fn reap_workers_reaps_an_explicitly_named_awaiting_review_session_without_force() {
        let _iso = crate::test_isolation::isolate();
        let store = Arc::new(Store::open(tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&awaiting_review_session("w1", "orch1")).unwrap();
        let engine = Engine::new(Arc::clone(&store));

        let ids = vec!["w1".to_string()];
        let outcomes = engine.reap_workers("orch1", ReapSelection::Ids(&ids), false).await.unwrap();

        assert_eq!(outcomes.len(), 1);
        assert!(
            !matches!(outcomes[0].1, ReapOutcome::SkippedLive),
            "an explicitly-named AwaitingReview session must not need --force, got {:?}", outcomes[0].1,
        );
    }

    #[tokio::test]
    async fn reap_workers_finished_selection_still_excludes_awaiting_review() {
        let store = Arc::new(Store::open(tempdir().unwrap().keep().join("t.db")).unwrap());
        store.upsert_session(&awaiting_review_session("w1", "orch1")).unwrap();
        let engine = Engine::new(Arc::clone(&store));

        let outcomes = engine.reap_workers("orch1", ReapSelection::Finished, false).await.unwrap();

        assert!(outcomes.is_empty(), "the default bulk reap must not touch an AwaitingReview session");
        let still_there = store.get_session("w1").unwrap().unwrap();
        assert_eq!(still_there.status, crate::types::SessionStatus::AwaitingReview);
    }

    #[tokio::test]
    async fn cleanup_session_sets_done_status() {
        let _iso = crate::test_isolation::isolate();
        let store = Arc::new(
            Store::open(tempdir().unwrap().keep().join("t.db")).unwrap()
        );
        let session = crate::types::Session {
            id: "s1".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: crate::types::SessionStatus::PrOpen,
            agent_type: "c".into(), cost_usd: 0.0, started_at: 0,
            pr_number: Some(1), pr_id: Some(1),
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
        };
        store.upsert_session(&session).unwrap();
        let engine = Engine::new(Arc::clone(&store));
        let mut rx = engine.subscribe();

        engine.cleanup_session("s1").await.unwrap();

        let evt = rx.recv().await.unwrap();
        if let Event::SessionUpdated(s, _fields) = evt {
            assert!(matches!(s.status, crate::types::SessionStatus::Done));
            assert!(
                s.terminal_at.is_some(),
                "cleanup_session must stamp terminal_at so the retention sweep can hold this \
                 record for the fleet board's grace window instead of purging it on sight",
            );
        } else {
            panic!("expected SessionUpdated");
        }
    }

    /// Reviewer's Important 4: `cleanup_session_in`'s full-row `upsert_session`
    /// (status/terminal_at) must not wipe the narrow `cleanup_result`/
    /// `cleanup_at` write — whichever store write runs second is the one
    /// that sticks, and the upsert's own snapshot predates `run_cleanup`.
    #[tokio::test]
    async fn cleanup_session_persists_cleanup_result() {
        let _iso = crate::test_isolation::isolate();
        let store = Arc::new(Store::open(tempdir().unwrap().keep().join("t.db")).unwrap());
        let session = crate::types::Session {
            id: "s1".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: crate::types::SessionStatus::PrOpen,
            agent_type: "c".into(), cost_usd: 0.0, started_at: 0,
            pr_number: Some(1), pr_id: Some(1),
            workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(), completion_outcome: None, self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        store.upsert_session(&session).unwrap();
        let engine = Engine::new(Arc::clone(&store));

        engine.cleanup_session("s1").await.unwrap();

        let after = store.get_session("s1").unwrap().unwrap();
        assert!(after.cleanup_result.is_some(), "cleanup_result must survive cleanup_session's own status upsert");
        assert!(after.cleanup_at.is_some());
    }

    #[tokio::test]
    async fn cleanup_session_removes_worktree_and_artifacts() {
        // Real git repo + real worktree so we exercise the same
        // `workspace::cleanup` path `remove_session` uses, not a mock.
        let repo_dir = tempdir().unwrap();
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

        let session_id = "s1";
        let worktree_path = repo_dir.path().join(".claude/worktrees").join(session_id);
        run_git(&["worktree", "add", "-q", worktree_path.to_str().unwrap()]);
        assert!(worktree_path.exists(), "test setup: worktree must exist before cleanup");

        let sessions_dir = tempdir().unwrap();
        let artifact_path = sessions_dir.path().join(format!("{session_id}.json"));
        std::fs::write(&artifact_path, "{}").unwrap();

        let store = Arc::new(Store::open(tempdir().unwrap().keep().join("t.db")).unwrap());
        let session = crate::types::Session {
            id: session_id.into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: crate::types::SessionStatus::PrOpen,
            agent_type: "c".into(), cost_usd: 0.0, started_at: 0,
            pr_number: Some(1), pr_id: Some(1),
            workspace_path: Some(worktree_path.to_string_lossy().to_string()),
            pid: None,
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
        };
        store.upsert_session(&session).unwrap();
        let engine = Engine::new(Arc::clone(&store));

        engine.cleanup_session_in(session_id, sessions_dir.path()).await.unwrap();

        assert!(!worktree_path.exists(), "cleanup_session must remove the worktree, like remove_session does");
        assert!(!artifact_path.exists(), "cleanup_session must remove session artifacts, like remove_session does");
        let after = store.get_session(session_id).unwrap().unwrap();
        assert!(matches!(after.status, crate::types::SessionStatus::Done));
    }

    /// The retention sweep's automatic purge (`force: false`) must leave a
    /// dirty worktree alone, and its row with it: a deleted row left the
    /// kept worktree unaccounted for.
    #[tokio::test]
    async fn remove_session_with_force_false_keeps_a_dirty_worktree_and_its_row() {
        let _iso = crate::test_isolation::isolate();
        let repo_dir = tempdir().unwrap();
        let run_git = |args: &[&str]| {
            let status = std::process::Command::new("git").args(args).current_dir(repo_dir.path()).status().unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run_git(&["init", "-q"]);
        run_git(&["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "--allow-empty", "-q", "-m", "init"]);

        let session_id = "dirty-1";
        let worktree_path = repo_dir.path().join(".claude/worktrees").join(session_id);
        run_git(&["worktree", "add", "-q", "-b", session_id, worktree_path.to_str().unwrap()]);
        std::fs::write(worktree_path.join("scratch.txt"), "uncommitted work").unwrap();

        let store = Arc::new(Store::open(tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut session = worker(session_id, "orch-1", crate::types::SessionStatus::Terminated);
        session.workspace_path = Some(worktree_path.to_string_lossy().to_string());
        store.upsert_session(&session).unwrap();
        let engine = Engine::new(Arc::clone(&store));

        let outcome = engine.remove_session(session_id, false).await.unwrap();

        assert_eq!(outcome, RemoveOutcome::Kept("skipped: dirty".into()));
        assert!(worktree_path.exists(), "a non-forced remove_session must never delete a dirty worktree");
        let row = store.get_session(session_id).unwrap().expect("the row of a kept worktree must survive");
        assert_eq!(row.cleanup_result.as_deref(), Some("skipped: dirty"));
        assert!(store.fleet_record(session_id).unwrap().unwrap().retained_at.is_some());
    }

    /// A worktree already gone from disk leaves nothing for the row to
    /// protect (cleanup never deletes a branch it skipped), so a skipped
    /// cleanup doesn't pin the row forever.
    #[tokio::test]
    async fn remove_session_with_force_false_removes_a_row_whose_worktree_is_gone() {
        let _iso = crate::test_isolation::isolate();
        let (repo, wt) = repo_with_worker_worktree("gone-1");
        let git_wt = |args: &[&str]| {
            assert!(std::process::Command::new("git").args(args).current_dir(&wt).status().unwrap().success());
        };
        git_wt(&["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "--allow-empty", "-q", "-m", "unpushed"]);
        assert!(std::process::Command::new("git").args(["worktree", "remove", "--force", wt.to_str().unwrap()])
            .current_dir(repo.path()).status().unwrap().success());
        let store = Arc::new(Store::open(tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut session = worker("gone-1", "orch-1", crate::types::SessionStatus::Terminated);
        session.workspace_path = Some(wt.to_string_lossy().into_owned());
        store.upsert_session(&session).unwrap();
        let engine = Engine::new(Arc::clone(&store));

        assert_eq!(engine.remove_session("gone-1", false).await.unwrap(), RemoveOutcome::Removed);
        assert!(store.get_session("gone-1").unwrap().is_none());
        let branch = std::process::Command::new("git").args(["show-ref", "--verify", "--quiet", "refs/heads/gone-1"])
            .current_dir(repo.path()).status().unwrap();
        assert!(branch.success(), "the unpushed branch itself is kept");
    }

    /// Defence in depth behind the sweep's own terminal-only filter: a
    /// non-forced removal never kills or cleans a live session.
    #[tokio::test]
    async fn remove_session_with_force_false_refuses_a_live_session() {
        let _iso = crate::test_isolation::isolate();
        let store = Arc::new(Store::open(tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut session = worker("live-1", "orch-1", crate::types::SessionStatus::Working);
        session.merged_at = Some(1_000);
        store.upsert_session(&session).unwrap();
        let engine = Engine::new(Arc::clone(&store));

        let outcome = engine.remove_session("live-1", false).await.unwrap();

        assert!(matches!(outcome, RemoveOutcome::Kept(_)), "{outcome:?}");
        assert!(store.get_session("live-1").unwrap().is_some());
    }

    #[tokio::test]
    async fn remove_session_with_force_true_deletes_a_dirty_worktree() {
        let _iso = crate::test_isolation::isolate();
        let repo_dir = tempdir().unwrap();
        let run_git = |args: &[&str]| {
            let status = std::process::Command::new("git").args(args).current_dir(repo_dir.path()).status().unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run_git(&["init", "-q"]);
        run_git(&["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "--allow-empty", "-q", "-m", "init"]);

        let session_id = "dirty-2";
        let worktree_path = repo_dir.path().join(".claude/worktrees").join(session_id);
        run_git(&["worktree", "add", "-q", "-b", session_id, worktree_path.to_str().unwrap()]);
        std::fs::write(worktree_path.join("scratch.txt"), "uncommitted work").unwrap();

        let store = Arc::new(Store::open(tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut session = worker(session_id, "orch-1", crate::types::SessionStatus::Terminated);
        session.workspace_path = Some(worktree_path.to_string_lossy().to_string());
        store.upsert_session(&session).unwrap();
        let engine = Engine::new(Arc::clone(&store));

        engine.remove_session(session_id, true).await.unwrap();

        assert!(!worktree_path.exists(), "an explicit, forced remove_session (user-initiated) still deletes a dirty worktree");
    }

    /// A try-out worker reuses one worktree across several tasks, checking
    /// out a new branch per item without the fleet record ever naming
    /// anything but the first one. `merged_at` gets stamped once that first
    /// branch's PR merges and never changes afterward — it must not vouch
    /// for whatever branch is actually checked out by the time this session
    /// is torn down.
    #[tokio::test]
    async fn remove_session_skips_when_head_switched_branches_after_an_earlier_merge() {
        let _iso = crate::test_isolation::isolate();
        let repo_dir = tempdir().unwrap();
        let run_git = |args: &[&str]| {
            let status = std::process::Command::new("git").args(args).current_dir(repo_dir.path()).status().unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run_git(&["init", "-q", "-b", "main"]);
        run_git(&["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "--allow-empty", "-q", "-m", "init"]);

        let session_id = "tryout-1";
        let worktree_path = repo_dir.path().join(".claude/worktrees").join(session_id);
        run_git(&["worktree", "add", "-q", "-b", "task-a", worktree_path.to_str().unwrap()]);

        let run_git_wt = |args: &[&str]| {
            let status = std::process::Command::new("git").args(args).current_dir(&worktree_path).status().unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        // task-a's PR merges; the worker then moves on to a new branch for
        // the next item, in the same worktree, with real unpushed work.
        run_git_wt(&["checkout", "-q", "-b", "task-b"]);
        run_git_wt(&["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "--allow-empty", "-q", "-m", "task b work"]);

        let store = Arc::new(Store::open(tempdir().unwrap().keep().join("t.db")).unwrap());
        store.record_spawn_facts(session_id, "brief", Some("task-a")).unwrap();
        let mut session = worker(session_id, "orch-1", crate::types::SessionStatus::Terminated);
        session.workspace_path = Some(worktree_path.to_string_lossy().to_string());
        session.merged_at = Some(1_000);
        store.upsert_session(&session).unwrap();
        let engine = Engine::new(Arc::clone(&store));

        engine.remove_session(session_id, false).await.unwrap();

        assert!(worktree_path.exists(), "task-b's unpushed commit must block removal even though task-a is recorded merged");
    }

    /// `merged_at` only proves the PR's head landed, not anything committed
    /// to the same branch afterward — kept-alive post-merge validation, or
    /// follow-up work in a long-lived session.
    #[tokio::test]
    async fn remove_session_skips_when_commits_land_on_the_recorded_branch_after_its_merge() {
        let _iso = crate::test_isolation::isolate();
        let repo_dir = tempdir().unwrap();
        let run_git = |args: &[&str]| {
            let status = std::process::Command::new("git").args(args).current_dir(repo_dir.path()).status().unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run_git(&["init", "-q", "-b", "main"]);
        run_git(&["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "--allow-empty", "-q", "-m", "init"]);

        let session_id = "kept-alive-1";
        let worktree_path = repo_dir.path().join(".claude/worktrees").join(session_id);
        run_git(&["worktree", "add", "-q", "-b", "feature", worktree_path.to_str().unwrap()]);

        let merged_at = crate::lifecycle::poller::now_millis() - 5_000;

        let run_git_wt = |args: &[&str]| {
            let status = std::process::Command::new("git").args(args).current_dir(&worktree_path).status().unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run_git_wt(&["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "--allow-empty", "-q", "-m", "post-merge follow-up"]);

        let store = Arc::new(Store::open(tempdir().unwrap().keep().join("t.db")).unwrap());
        store.record_spawn_facts(session_id, "brief", Some("feature")).unwrap();
        let mut session = worker(session_id, "orch-1", crate::types::SessionStatus::Terminated);
        session.workspace_path = Some(worktree_path.to_string_lossy().to_string());
        session.merged_at = Some(merged_at);
        store.upsert_session(&session).unwrap();
        let engine = Engine::new(Arc::clone(&store));

        engine.remove_session(session_id, false).await.unwrap();

        assert!(worktree_path.exists(), "a commit landed on the merged branch after merged_at — the merge never covered it");
    }

    /// A commit that lands in the real gap between the actual merge and the
    /// poller's detection tick gets a timestamp before `merged_at` (stamped
    /// at detection), so a timestamp-only check would misread it as
    /// pre-merge. GitHub's own merged head sha doesn't care about
    /// timestamps at all, so it still catches it.
    #[tokio::test]
    async fn remove_session_skips_a_commit_backdated_into_the_merge_detection_race_window() {
        let _iso = crate::test_isolation::isolate();
        let repo_dir = tempdir().unwrap();
        let run_git = |args: &[&str]| {
            let status = std::process::Command::new("git").args(args).current_dir(repo_dir.path()).status().unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run_git(&["init", "-q", "-b", "main"]);
        run_git(&["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "--allow-empty", "-q", "-m", "init"]);

        let session_id = "race-window-1";
        let worktree_path = repo_dir.path().join(".claude/worktrees").join(session_id);
        run_git(&["worktree", "add", "-q", "-b", "feature", worktree_path.to_str().unwrap()]);
        let merged_head = String::from_utf8(
            std::process::Command::new("git").args(["rev-parse", "HEAD"]).current_dir(&worktree_path).output().unwrap().stdout,
        ).unwrap().trim().to_string();

        // A commit made for real after the merge, but dated as if it
        // predated it — exactly what a clock race between the merge and the
        // poller's detection tick produces.
        let status = std::process::Command::new("git")
            .args(["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "--allow-empty", "-q", "-m", "race-window work"])
            .env("GIT_COMMITTER_DATE", "1000000000")
            .env("GIT_AUTHOR_DATE", "1000000000")
            .current_dir(&worktree_path)
            .status()
            .unwrap();
        assert!(status.success());

        let store = Arc::new(Store::open(tempdir().unwrap().keep().join("t.db")).unwrap());
        store.record_spawn_facts(session_id, "brief", Some("feature")).unwrap();
        store.set_merged_head_sha(session_id, &merged_head).unwrap();
        let mut session = worker(session_id, "orch-1", crate::types::SessionStatus::Terminated);
        session.workspace_path = Some(worktree_path.to_string_lossy().to_string());
        session.merged_at = Some(2_000_000_000_000); // long after the backdated commit's fake timestamp
        store.upsert_session(&session).unwrap();
        let engine = Engine::new(Arc::clone(&store));

        engine.remove_session(session_id, false).await.unwrap();

        assert!(worktree_path.exists(), "the sha-based check must still catch a commit whose backdated timestamp predates merged_at");
    }

    /// A detached HEAD's commits aren't reachable from any named branch, so
    /// the ordinary branch-based checks never see them — they must still
    /// block removal when nothing remote has them either.
    #[tokio::test]
    async fn remove_session_skips_a_detached_head_with_an_unreachable_commit() {
        let _iso = crate::test_isolation::isolate();
        let repo_dir = tempdir().unwrap();
        let run_git = |args: &[&str]| {
            let status = std::process::Command::new("git").args(args).current_dir(repo_dir.path()).status().unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run_git(&["init", "-q", "-b", "main"]);
        run_git(&["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "--allow-empty", "-q", "-m", "init"]);

        let session_id = "detached-1";
        let worktree_path = repo_dir.path().join(".claude/worktrees").join(session_id);
        run_git(&["worktree", "add", "-q", "-b", "feature", worktree_path.to_str().unwrap()]);

        let run_git_wt = |args: &[&str]| {
            let status = std::process::Command::new("git").args(args).current_dir(&worktree_path).status().unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run_git_wt(&["checkout", "-q", "--detach"]);
        run_git_wt(&["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "--allow-empty", "-q", "-m", "orphaned work"]);

        let store = Arc::new(Store::open(tempdir().unwrap().keep().join("t.db")).unwrap());
        store.record_spawn_facts(session_id, "brief", Some("feature")).unwrap();
        let mut session = worker(session_id, "orch-1", crate::types::SessionStatus::Terminated);
        session.workspace_path = Some(worktree_path.to_string_lossy().to_string());
        store.upsert_session(&session).unwrap();
        let engine = Engine::new(Arc::clone(&store));

        engine.remove_session(session_id, false).await.unwrap();

        assert!(worktree_path.exists(), "a detached HEAD with a commit unreachable from any ref must block removal");
    }

    pub(super) fn repo_with_worker_worktree(session_id: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let repo = tempdir().unwrap();
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

    /// Removing an orchestrator used to force-clean every worker. A dirty
    /// worker and an `Interrupted` one now keep their worktree and row,
    /// detached; a clean finished one still goes.
    #[tokio::test]
    async fn removing_an_orchestrator_keeps_dirty_and_interrupted_workers() {
        let _iso = crate::test_isolation::isolate();
        use crate::types::SessionStatus::*;
        for keeping_live in [false, true] {
            let (_r1, dirty_wt) = repo_with_worker_worktree("dirty-1");
            std::fs::write(dirty_wt.join("wip.rs"), "uncommitted").unwrap();
            let (_r2, intr_wt) = repo_with_worker_worktree("intr-1");
            let (_r3, clean_wt) = repo_with_worker_worktree("clean-1");
            let with_ws = |mut s: Session, wt: &std::path::Path| {
                s.workspace_path = Some(wt.to_string_lossy().into_owned());
                s
            };
            let mut orch = worker("orch-1", "orch-1", Working);
            orch.orchestrator_id = None;
            // `remove_orchestrator_keeping_live` keeps a live worker as it
            // is, so it sees the dirty worker already finished.
            let dirty_status = if keeping_live { Terminated } else { Working };
            let (store, engine) = reap_fixture(&[
                orch,
                with_ws(worker("dirty-1", "orch-1", dirty_status), &dirty_wt),
                with_ws(worker("intr-1", "orch-1", Interrupted), &intr_wt),
                with_ws(worker("clean-1", "orch-1", Done), &clean_wt),
            ]);
            store.upsert_orchestrator(&crate::types::Orchestrator { id: "orch-1".into(), name: "o".into(), created_at: 0 }).unwrap();

            let kept = if keeping_live {
                engine.remove_orchestrator_keeping_live("orch-1").await.unwrap()
            } else {
                engine.remove_orchestrator("orch-1").await.unwrap()
            };

            let mut ids: Vec<&str> = kept.iter().map(|k| k.id.as_str()).collect();
            ids.sort();
            assert_eq!(ids, ["dirty-1", "intr-1"], "keeping_live={keeping_live}: {kept:?}");
            assert!(dirty_wt.join("wip.rs").exists(), "the dirty worktree must survive");
            assert!(intr_wt.exists(), "the Interrupted worker's worktree must survive");
            let dirty = store.get_session("dirty-1").unwrap().expect("the dirty worker's row must survive");
            assert_eq!(dirty.orchestrator_id, None);
            assert_eq!(dirty.status, Terminated, "it was killed");
            assert_eq!(dirty.cleanup_result.as_deref(), Some("skipped: dirty"));
            let intr = store.get_session("intr-1").unwrap().expect("the Interrupted worker's row must survive");
            assert_eq!(intr.status, Interrupted, "still resumable");
            assert!(store.get_session("clean-1").unwrap().is_none());
            assert!(!clean_wt.exists(), "a clean finished worker is still removed");
        }
    }

    /// The GUI's "Clean up finished" (#317): only a cleanly finished worker
    /// loses its worktree. A live merged one, a dirty `Terminated` one and an
    /// `Interrupted` one keep theirs, and every row stays.
    #[tokio::test]
    async fn clean_up_finished_touches_only_cleanly_finished_workers() {
        let _iso = crate::test_isolation::isolate();
        use crate::types::SessionStatus::*;
        let (_r1, live_wt) = repo_with_worker_worktree("live-merged");
        let (_r2, dirty_wt) = repo_with_worker_worktree("dirty-term");
        std::fs::write(dirty_wt.join("wip.rs"), "uncommitted").unwrap();
        let (_r3, intr_wt) = repo_with_worker_worktree("intr");
        let (_r4, clean_wt) = repo_with_worker_worktree("clean-done");
        let with_ws = |mut s: Session, wt: &std::path::Path| {
            s.workspace_path = Some(wt.to_string_lossy().into_owned());
            s
        };
        let mut live = with_ws(worker("live-merged", "orch-1", Working), &live_wt);
        live.merged_at = Some(1_000);
        let (store, engine) = reap_fixture(&[
            live,
            with_ws(worker("dirty-term", "orch-1", Terminated), &dirty_wt),
            with_ws(worker("intr", "orch-1", Interrupted), &intr_wt),
            with_ws(worker("clean-done", "orch-1", Done), &clean_wt),
        ]);

        engine.reap_workers("orch-1", ReapSelection::Finished, false).await.unwrap();

        assert!(live_wt.exists() && intr_wt.exists() && dirty_wt.join("wip.rs").exists());
        assert!(!clean_wt.exists());
        assert_eq!(store.get_session("live-merged").unwrap().unwrap().status, Working);
        assert_eq!(store.get_session("intr").unwrap().unwrap().status, Interrupted);
        assert_eq!(store.get_session("dirty-term").unwrap().unwrap().cleanup_result.as_deref(), Some("skipped: dirty"));
        assert!(store.get_session("clean-done").unwrap().is_some(), "reap leaves rows to the retention sweep");
    }

    #[tokio::test]
    async fn removing_an_orchestrator_keeping_live_detaches_running_and_resumable_workers() {
        let _iso = crate::test_isolation::isolate();
        use crate::types::SessionStatus::*;
        let mut orch = worker("orch-1", "orch-1", Terminated);
        orch.orchestrator_id = None;
        let (store, engine) = reap_fixture(&[
            orch,
            worker("done-1", "orch-1", Done),
            worker("intr-1", "orch-1", Interrupted),
            worker("live-1", "orch-1", Working),
        ]);
        store.upsert_orchestrator(&crate::types::Orchestrator { id: "orch-1".into(), name: "o".into(), created_at: 0 }).unwrap();

        let kept = engine.remove_orchestrator_keeping_live("orch-1").await.unwrap();

        let kept: Vec<&str> = kept.iter().map(|k| k.id.as_str()).collect();
        assert_eq!(kept, ["intr-1", "live-1"]);
        for id in ["intr-1", "live-1"] {
            let s = store.get_session(id).unwrap().expect("live and resumable workers keep their records");
            assert_eq!(s.orchestrator_id, None, "detached, so the orchestrator delete can't take {id}");
        }
        assert_eq!(store.get_session("intr-1").unwrap().unwrap().status, Interrupted, "still resumable");
        for gone in ["orch-1", "done-1"] {
            assert!(store.get_session(gone).unwrap().is_none(), "{gone} should be removed");
        }
        assert!(store.list_orchestrators().unwrap().is_empty());
    }

    // ── Reap ────────────────────────────────────────────────────────────────

    fn worker(id: &str, orchestrator: &str, status: crate::types::SessionStatus) -> Session {
        Session {
            id: id.into(), orchestrator_id: Some(orchestrator.into()), name: id.into(),
            repo: "r".into(), status, agent_type: "claude-code".into(),
            cost_usd: 0.0, started_at: 0, pr_number: None, pr_id: None,
            workspace_path: None, pid: None, model: None, context_tokens: None,
            brain_reads: Vec::new(), brain_write: None, context_used_pct: None, context_total_tokens: None,
            context_window_size: None, claude_session_id: None, summary: None,
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

    /// Store + engine seeded with `workers`.
    fn reap_fixture(workers: &[Session]) -> (Arc<Store>, Arc<Engine>) {
        let store = Arc::new(Store::open(tempdir().unwrap().keep().join("t.db")).unwrap());
        for w in workers {
            store.upsert_session(w).unwrap();
        }
        let engine = Engine::new(Arc::clone(&store));
        (store, engine)
    }

    #[tokio::test]
    async fn reap_default_selects_only_finished_workers() {
        let _iso = crate::test_isolation::isolate();
        use crate::types::SessionStatus::*;
        let (store, engine) = reap_fixture(&[
            worker("done-1",  "orch-1", Done),
            worker("term-1",  "orch-1", Terminated),
            worker("intr-1",  "orch-1", Interrupted),
            worker("live-1",  "orch-1", Working),
            worker("live-2",  "orch-1", PrOpen),
        ]);

        let outcomes = engine
            .reap_workers("orch-1", ReapSelection::Finished, false)
            .await
            .unwrap();

        // Compared as a set: `list_sessions` orders by `started_at DESC` and
        // every fixture worker shares `started_at: 0`, so SQLite gives no
        // guarantee about tie order — asserting a sequence here would be
        // fragile rather than meaningful.
        let mut got: Vec<(String, ReapOutcome)> = outcomes.clone();
        got.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(got, vec![
            ("done-1".to_string(), ReapOutcome::Reaped),
            ("term-1".to_string(), ReapOutcome::Reaped),
        ], "only the finished-for-good workers are selected: {outcomes:?}");
        // Live and resumable workers aren't merely skipped — the default
        // selection never picks them up at all.
        assert!(!outcomes.iter().any(|(id, _)| id == "live-1" || id == "live-2" || id == "intr-1"));
        assert!(
            matches!(store.get_session("live-1").unwrap().unwrap().status, Working),
            "an unselected live worker must be left untouched",
        );
    }

    #[tokio::test]
    async fn a_reaped_worker_no_longer_awaits_reap() {
        let _iso = crate::test_isolation::isolate();
        use crate::types::SessionStatus::*;
        let (store, engine) = reap_fixture(&[worker("done-1", "orch-1", Done), worker("intr-1", "orch-1", Interrupted)]);
        assert!(awaits_reap(&store.get_session("done-1").unwrap().unwrap()));
        assert!(!awaits_reap(&store.get_session("intr-1").unwrap().unwrap()), "interrupted is resumable, not finished");

        engine.reap_workers("orch-1", ReapSelection::Finished, false).await.unwrap();

        let after = store.get_session("done-1").unwrap().unwrap();
        assert!(after.cleanup_at.is_some() && !awaits_reap(&after));
        assert!(matches!(after.status, Done), "reaping keeps the status the worker earned");

        let again = engine.reap_workers("orch-1", ReapSelection::Finished, false).await.unwrap();
        assert!(again.is_empty(), "a repeat reap leaves the recorded cleanup alone: {again:?}");
        assert_eq!(store.get_session("done-1").unwrap().unwrap().cleanup_at, after.cleanup_at);
    }

    #[test]
    fn a_cleanup_before_the_latest_finish_no_longer_counts() {
        use crate::types::SessionStatus::*;
        let mut w = worker("w", "o", Done);
        w.cleanup_at = Some(10);
        w.terminal_at = Some(5);
        assert!(!awaits_reap(&w));
        w.terminal_at = Some(20);
        assert!(awaits_reap(&w), "resumed and finished again after its last cleanup");
        w.terminal_at = None;
        assert!(!awaits_reap(&w));
    }

    /// A kept-alive merged worker (live status, `merged_at` set) reaps like
    /// any other live worker but reports the merged-specific outcomes, so the
    /// CLI can tell the orchestrator this was the documented cleanup rather
    /// than an interruption of unfinished work.
    #[tokio::test]
    async fn reap_reports_merged_specific_outcomes_for_a_kept_alive_worker() {
        let _iso = crate::test_isolation::isolate();
        use crate::types::SessionStatus::*;
        let mut merged = worker("merged-1", "orch-1", Mergeable);
        merged.merged_at = Some(1_000);
        let (store, engine) = reap_fixture(&[merged]);

        // Without --force it's skipped, but with the merged-aware wording.
        let skipped = engine
            .reap_workers("orch-1", ReapSelection::Ids(&["merged-1".to_string()]), false)
            .await
            .unwrap();
        assert_eq!(skipped, vec![("merged-1".to_string(), ReapOutcome::SkippedLiveMerged)]);
        assert!(
            matches!(store.get_session("merged-1").unwrap().unwrap().status, Mergeable),
            "a skipped merged worker must be left untouched",
        );

        // With --force it's reaped, reported as the merged happy path.
        let reaped = engine
            .reap_workers("orch-1", ReapSelection::Ids(&["merged-1".to_string()]), true)
            .await
            .unwrap();
        assert_eq!(reaped, vec![("merged-1".to_string(), ReapOutcome::ReapedLiveMerged)]);
        assert!(
            matches!(store.get_session("merged-1").unwrap().unwrap().status, Terminated),
            "a force-reaped merged worker is terminated like any live one",
        );
    }

    /// Regression test for the reboot footgun: startup reconciliation marks
    /// every worker with a resumable transcript `Interrupted`, so if a bare
    /// `ninox reap` treated that as finished it would shred every resume
    /// workspace (uncommitted work included) in one command — and leave the
    /// cards on the board forever, since `sweep_retired_sessions` only ever
    /// purges `Done`/`Terminated`.
    #[tokio::test]
    async fn reap_never_touches_a_resumable_worker_by_default() {
        use crate::types::SessionStatus::*;
        let (store, engine) = reap_fixture(&[worker("intr-1", "orch-1", Interrupted)]);

        let outcomes = engine
            .reap_workers("orch-1", ReapSelection::Finished, false)
            .await
            .unwrap();

        assert!(outcomes.is_empty(), "an Interrupted worker is not finished: {outcomes:?}");
        let after = store.get_session("intr-1").unwrap().unwrap();
        assert!(matches!(after.status, Interrupted), "must stay resumable");
        assert_eq!(after.terminal_at, None, "must not start the retention countdown");
    }

    #[tokio::test]
    async fn reap_needs_force_for_a_resumable_worker_and_then_terminates_it() {
        let _iso = crate::test_isolation::isolate();
        use crate::types::SessionStatus::*;
        let (store, engine) = reap_fixture(&[worker("intr-1", "orch-1", Interrupted)]);

        let skipped = engine
            .reap_workers("orch-1", ReapSelection::All, false)
            .await
            .unwrap();
        assert_eq!(skipped, vec![("intr-1".to_string(), ReapOutcome::SkippedResumable)]);
        assert!(matches!(store.get_session("intr-1").unwrap().unwrap().status, Interrupted));

        let reaped = engine
            .reap_workers("orch-1", ReapSelection::All, true)
            .await
            .unwrap();
        assert_eq!(reaped, vec![("intr-1".to_string(), ReapOutcome::ReapedResumable)]);
        let after = store.get_session("intr-1").unwrap().unwrap();
        assert!(
            matches!(after.status, Terminated),
            "a reaped worker can no longer be resumed, and only Done/Terminated are ever \
             purged by the retention sweep — leaving it Interrupted would strand the card \
             on the board forever, got {:?}", after.status,
        );
        assert!(after.terminal_at.is_some());
    }

    #[tokio::test]
    async fn reap_keeps_the_record_for_the_retention_sweep_to_purge() {
        let _iso = crate::test_isolation::isolate();
        use crate::types::SessionStatus::*;
        // The record must survive the reap — the fleet board keeps the card,
        // and `sweep_retired_sessions` stays the only thing that deletes a row.
        let mut w = worker("term-1", "orch-1", Terminated);
        w.terminal_at = Some(1_000);
        let (store, engine) = reap_fixture(&[w]);

        engine.reap_workers("orch-1", ReapSelection::Finished, false).await.unwrap();

        let after = store.get_session("term-1").unwrap().expect("record must not be deleted");
        assert_eq!(
            after.terminal_at, Some(1_000),
            "re-reaping must not extend a card's stay on the board",
        );
    }

    /// A `terminal_at` of `None` on an already-terminal row is load-bearing:
    /// `sweep_retired_sessions` documents it as a terminal state reached by
    /// direct user action (`terminate_session`/`remove_session`) and purges
    /// it on sight. Stamping one here would resurrect a card the user just
    /// dismissed for the whole retention window — a cleanup command must
    /// never make a record live LONGER.
    #[tokio::test]
    async fn reap_never_stamps_a_deliberately_unstamped_terminal_worker() {
        let _iso = crate::test_isolation::isolate();
        use crate::types::SessionStatus::*;
        let w = worker("term-1", "orch-1", Terminated); // terminal_at: None
        let (store, engine) = reap_fixture(&[w]);

        engine.reap_workers("orch-1", ReapSelection::Finished, false).await.unwrap();

        assert_eq!(
            store.get_session("term-1").unwrap().unwrap().terminal_at, None,
            "must stay purge-on-sight, not gain a 2-day grace window",
        );
    }

    /// The row is re-read immediately before the status write (mirroring
    /// `poller::update_live_session_row`), because `upsert_session` is a
    /// full-row write and the reap awaits a tmux kill plus a `git worktree
    /// remove` first. If the retention sweep purges the row in that window,
    /// writing the snapshot back would resurrect the card AND earn the
    /// orchestrator a second "worker retired" message when it expires again.
    #[tokio::test]
    async fn reaping_never_resurrects_a_row_deleted_mid_reap() {
        use crate::types::SessionStatus::*;
        let (store, engine) = reap_fixture(&[worker("live-1", "orch-1", Working)]);
        let snapshot = store.get_session("live-1").unwrap().unwrap();

        // Stand in for the sweep landing between the snapshot and the write.
        store.delete_session("live-1").unwrap();

        let written = engine.mark_reaped_terminal(&snapshot).unwrap();

        assert!(written.is_none(), "must report the row as gone rather than writing it");
        assert!(
            store.get_session("live-1").unwrap().is_none(),
            "a deleted row must stay deleted — resurrecting it puts the card back on the board",
        );
    }

    /// `Done` means "PR merged, orchestrator already notified" everywhere
    /// else in the codebase (`sweep_retired_sessions` dedups on it), so a
    /// reap must never relabel a `Terminated` worker as `Done` — that would
    /// claim work landed when it didn't AND swallow the retired notice.
    #[tokio::test]
    async fn reap_preserves_the_status_a_finished_worker_earned() {
        let _iso = crate::test_isolation::isolate();
        use crate::types::SessionStatus::*;
        let (store, engine) = reap_fixture(&[
            worker("done-1", "orch-1", Done),
            worker("term-1", "orch-1", Terminated),
        ]);

        engine.reap_workers("orch-1", ReapSelection::Finished, false).await.unwrap();

        assert!(matches!(store.get_session("done-1").unwrap().unwrap().status, Done));
        assert!(matches!(store.get_session("term-1").unwrap().unwrap().status, Terminated));
    }

    #[tokio::test]
    async fn reap_skips_a_live_worker_without_force() {
        use crate::types::SessionStatus::*;
        let (store, engine) = reap_fixture(&[worker("live-1", "orch-1", Working)]);

        let outcomes = engine
            .reap_workers("orch-1", ReapSelection::All, false)
            .await
            .unwrap();

        assert_eq!(outcomes, vec![("live-1".to_string(), ReapOutcome::SkippedLive)]);
        let after = store.get_session("live-1").unwrap().unwrap();
        assert!(matches!(after.status, Working), "status must be untouched");
        assert_eq!(after.terminal_at, None, "no retention countdown for an untouched worker");
    }

    #[tokio::test]
    async fn reap_kills_a_live_worker_with_force_and_marks_it_terminated() {
        let _iso = crate::test_isolation::isolate();
        use crate::types::SessionStatus::*;
        let (store, engine) = reap_fixture(&[worker("live-1", "orch-1", Working)]);

        let outcomes = engine
            .reap_workers("orch-1", ReapSelection::All, true)
            .await
            .unwrap();

        assert_eq!(outcomes, vec![("live-1".to_string(), ReapOutcome::ReapedLive)]);
        let after = store.get_session("live-1").unwrap().unwrap();
        assert!(matches!(after.status, Terminated), "a killed worker is Terminated, not Done");
        assert!(after.terminal_at.is_some());
    }

    /// The containment guarantee: an orchestrator can only ever reap its own
    /// workers, so a named id belonging to someone else is reported missing
    /// rather than acted on.
    #[tokio::test]
    async fn reap_refuses_ids_outside_the_callers_own_workers() {
        let _iso = crate::test_isolation::isolate();
        use crate::types::SessionStatus::*;
        let mut standalone = worker("standalone", "orch-1", Terminated);
        standalone.orchestrator_id = None;
        let (store, engine) = reap_fixture(&[
            worker("mine",     "orch-1", Terminated),
            worker("theirs",   "orch-2", Terminated),
            standalone,
        ]);

        let ids = vec!["mine".to_string(), "theirs".to_string(), "standalone".to_string(), "ghost".to_string()];
        let outcomes = engine
            .reap_workers("orch-1", ReapSelection::Ids(&ids), true)
            .await
            .unwrap();

        assert_eq!(outcomes, vec![
            ("mine".to_string(),       ReapOutcome::Reaped),
            ("theirs".to_string(),     ReapOutcome::NotFound),
            ("standalone".to_string(), ReapOutcome::NotFound),
            ("ghost".to_string(),      ReapOutcome::NotFound),
        ]);
        assert_eq!(
            store.get_session("theirs").unwrap().unwrap().terminal_at, None,
            "another orchestrator's worker must not be touched",
        );
        assert_eq!(
            store.get_session("standalone").unwrap().unwrap().terminal_at, None,
            "an unattached session must not be touched",
        );
    }

    /// Explicit ids reach a live worker too (with `force`), where the
    /// default `Finished` selection would never have picked it up.
    #[tokio::test]
    async fn reap_by_id_can_target_a_live_worker() {
        let _iso = crate::test_isolation::isolate();
        use crate::types::SessionStatus::*;
        let (_store, engine) = reap_fixture(&[worker("live-1", "orch-1", Working)]);
        let ids = vec!["live-1".to_string()];

        let without_force = engine
            .reap_workers("orch-1", ReapSelection::Ids(&ids), false)
            .await
            .unwrap();
        assert_eq!(without_force, vec![("live-1".to_string(), ReapOutcome::SkippedLive)]);

        let with_force = engine
            .reap_workers("orch-1", ReapSelection::Ids(&ids), true)
            .await
            .unwrap();
        assert_eq!(with_force, vec![("live-1".to_string(), ReapOutcome::ReapedLive)]);
    }

    /// A worker whose PR merges *during* the reap's awaits is finished by the
    /// poller, not by the reap. Overwriting that `Done` with `Terminated`
    /// would turn a merge the orchestrator was already notified about into the
    /// sweep's "cleaned up without a detected PR merge" notice — a direct
    /// contradiction of what it was told.
    #[tokio::test]
    async fn reaping_never_clobbers_a_done_reached_during_the_reap() {
        use crate::types::SessionStatus::*;
        let (store, engine) = reap_fixture(&[worker("live-1", "orch-1", Working)]);
        let snapshot = store.get_session("live-1").unwrap().unwrap();

        // Stand in for `handle_merge_detection`/`cleanup_session` landing
        // mid-reap.
        let mut merged = snapshot.clone();
        merged.status      = Done;
        merged.pr_number   = Some(9);
        merged.terminal_at = Some(1_000);
        store.upsert_session(&merged).unwrap();

        let written = engine.mark_reaped_terminal(&snapshot).unwrap();

        assert!(written.is_none(), "the poller owns this outcome, not the reap");
        let after = store.get_session("live-1").unwrap().unwrap();
        assert!(matches!(after.status, Done), "a merge must not become Terminated");
        assert_eq!(after.terminal_at, Some(1_000), "the countdown must not restart");
    }

    /// `poll_pids` reaping a dead process stamps `terminal_at` itself. Landing
    /// mid-reap, that stamp wins — re-stamping would restart the retention
    /// countdown this whole re-read path exists to protect.
    #[tokio::test]
    async fn reaping_never_restarts_a_countdown_started_during_the_reap() {
        use crate::types::SessionStatus::*;
        let (store, engine) = reap_fixture(&[worker("live-1", "orch-1", Working)]);
        let snapshot = store.get_session("live-1").unwrap().unwrap();

        let mut died = snapshot.clone();
        died.status      = Terminated;
        died.terminal_at = Some(2_000);
        store.upsert_session(&died).unwrap();

        engine.mark_reaped_terminal(&snapshot).unwrap();

        assert_eq!(
            store.get_session("live-1").unwrap().unwrap().terminal_at, Some(2_000),
            "an existing stamp must survive",
        );
    }

    /// The one non-finished status that can show up on the fresh read: the
    /// worktree its `--resume` depended on is gone by now, so the reap really
    /// does own this outcome.
    #[tokio::test]
    async fn reaping_still_terminates_a_row_found_interrupted_on_the_fresh_read() {
        use crate::types::SessionStatus::*;
        let (store, engine) = reap_fixture(&[worker("live-1", "orch-1", Working)]);
        let snapshot = store.get_session("live-1").unwrap().unwrap();

        let mut interrupted = snapshot.clone();
        interrupted.status = Interrupted;
        store.upsert_session(&interrupted).unwrap();

        let written = engine.mark_reaped_terminal(&snapshot).unwrap();

        assert!(written.is_some(), "Interrupted is not finished — the reap still writes");
        let after = store.get_session("live-1").unwrap().unwrap();
        assert!(matches!(after.status, Terminated));
        assert!(after.terminal_at.is_some(), "the reap started this countdown");
    }

    /// The other half of re-reading before the write: a PR the worker opened
    /// (or cost/context a `ninox statusline` recorded) during the reap's
    /// awaits must survive. Reverting `pr_number` would be unrecoverable —
    /// `poll_pr_reconciliation` skips `Terminated` sessions and needs the
    /// worktree the reap just deleted.
    #[tokio::test]
    async fn reaping_preserves_fields_written_while_the_reap_was_working() {
        use crate::types::SessionStatus::*;
        let (store, engine) = reap_fixture(&[worker("live-1", "orch-1", Working)]);
        let snapshot = store.get_session("live-1").unwrap().unwrap();
        assert_eq!(snapshot.pr_number, None, "sanity: nothing recorded yet");

        // Stand in for `sync_sessions_metadata` adopting the worker's freshly
        // created PR, and a statusline write, mid-reap.
        let mut fresher = snapshot.clone();
        fresher.pr_number = Some(42);
        fresher.pr_id     = Some(42);
        fresher.cost_usd  = 3.5;
        store.upsert_session(&fresher).unwrap();

        engine.mark_reaped_terminal(&snapshot).unwrap();

        let after = store.get_session("live-1").unwrap().unwrap();
        assert!(matches!(after.status, Terminated), "the reap still owns status");
        assert_eq!(after.pr_number, Some(42), "a PR opened mid-reap must not be reverted");
        assert_eq!(after.pr_id, Some(42));
        assert_eq!(after.cost_usd, 3.5, "cost recorded mid-reap must not be reverted");
    }

    #[tokio::test]
    async fn reap_removes_the_workers_worktree() {
        let _iso = crate::test_isolation::isolate();
        // Real git repo + real worktree, mirroring
        // `cleanup_session_removes_worktree_and_artifacts` — reclaiming the
        // worktree is the whole point of reaping.
        //
        // Note what this does NOT assert: that reap leaves hook artifacts
        // alone for the poller to consume. `remove_session_artifacts` resolves
        // its directory from `AppConfig::sessions_dir()`, which has no test
        // override, so the only honest way to observe that would be writing
        // into the developer's real config dir. An earlier version of this
        // test wrote requests into a private tempdir reap never looks at,
        // which made the assertion pass no matter what reap did. That
        // behaviour is enforced by reap passing `remove_hook_artifacts:
        // false` to `workspace::cleanup` — see the comment at that call
        // site in `reap_workers`.
        let repo_dir = tempdir().unwrap();
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

        let session_id = "reap-wt";
        let worktree_path = repo_dir.path().join(".claude/worktrees").join(session_id);
        run_git(&["worktree", "add", "-q", worktree_path.to_str().unwrap()]);
        assert!(worktree_path.exists(), "test setup: worktree must exist before the reap");

        let mut w = worker(session_id, "orch-1", crate::types::SessionStatus::Terminated);
        w.workspace_path = Some(worktree_path.to_string_lossy().to_string());
        let (_store, engine) = reap_fixture(&[w]);

        engine
            .reap_workers("orch-1", ReapSelection::Finished, false)
            .await
            .unwrap();

        assert!(!worktree_path.exists(), "reap must remove the worker's worktree");
    }

    /// Reaping (or killing) a restore candidate dismisses it: a later
    /// `ninox fleet restore` must not bring back a worker the orchestrator
    /// deliberately cleaned up.
    #[tokio::test]
    async fn reap_and_terminate_dismiss_pending_fleet_restores() {
        let _iso = crate::test_isolation::isolate();
        use crate::types::SessionStatus::*;
        let mut reconciled = worker("reconciled", "orch-1", Terminated);
        reconciled.terminal_at = Some(50);
        let (store, engine) = reap_fixture(&[
            worker("interrupted", "orch-1", Interrupted),
            reconciled.clone(),
            Session { id: "killed".into(), ..reconciled.clone() },
        ]);
        for id in ["interrupted", "reconciled", "killed"] {
            store.record_interruption(id, 10, &Working, Some("reboot")).unwrap();
        }
        store.mark_reconciled_terminal("reconciled", 50).unwrap();
        store.mark_reconciled_terminal("killed", 50).unwrap();
        let awaiting = |id: &str| {
            let s = store.get_session(id).unwrap().unwrap();
            crate::fleet::awaits_restore(&s, &store.fleet_record(id).unwrap().unwrap_or_default())
        };
        assert!(awaiting("interrupted") && awaiting("reconciled") && awaiting("killed"));

        let ids = ["interrupted".to_string(), "reconciled".to_string()];
        engine
            .reap_workers("orch-1", ReapSelection::Ids(&ids), true)
            .await
            .unwrap();
        engine.terminate_session("killed").await.unwrap();

        assert!(!awaiting("interrupted"), "a reaped Interrupted worker is gone for good");
        assert!(!awaiting("reconciled"), "a reaped fresh-restart candidate is gone for good");
        assert!(!awaiting("killed"), "a killed fresh-restart candidate is gone for good");
    }

    // The non-ninox-path guard this used to cover is now
    // `workspace::cleanup`'s own responsibility — see
    // `workspace::cleanup::tests::non_ninox_path_is_never_touched`.
}
