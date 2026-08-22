use crate::{github::{GitHubClient, GithubApi}, store::Store, types::*};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{broadcast, Mutex};

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
    Notification(Notification),
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
        })
    }

    pub fn new_with_github(store: Arc<Store>, token: String) -> Arc<Self> {
        let (tx, _) = broadcast::channel(256);
        let github = GitHubClient::new(token).ok()
            .map(|c| Arc::new(c) as Arc<dyn GithubApi>);
        Arc::new(Self {
            store,
            tx,
            pty_writers:   Mutex::new(HashMap::new()),
            stream_cancel: Mutex::new(HashMap::new()),
            github,
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

    /// Kill all worker sessions belonging to an orchestrator, delete from DB, emit events.
    pub async fn remove_orchestrator(&self, orchestrator_id: &str) -> anyhow::Result<()> {
        let workers = self.store.sessions_by_orchestrator(orchestrator_id)?;
        let sessions_dir = crate::config::AppConfig::sessions_dir();
        let mut claims = Vec::with_capacity(workers.len());
        for session in &workers {
            let claim = match self
                .store
                .claim_worker_cleanup_snapshot_recoverable(&session.id, session.started_at)
            {
                Ok(claim) => claim,
                Err(error) => {
                    self.abort_recoverable_cleanup_claims(&claims);
                    return Err(error);
                }
            };
            if claim.is_none() {
                let already_released = match self
                    .store
                    .released_worker_for_cleanup_snapshot(&session.id, session.started_at)
                {
                    Ok(worker) => worker.is_some(),
                    Err(error) => {
                        self.abort_recoverable_cleanup_claims(&claims);
                        return Err(error);
                    }
                };
                if !already_released {
                    self.abort_recoverable_cleanup_claims(&claims);
                    anyhow::bail!(
                        "orchestrator worker {} changed while cleanup was being claimed",
                        session.id
                    );
                }
            }
            claims.push(claim);
        }
        for claim in claims.iter().flatten() {
            if let Err(error) = self.stop_recoverable_cleanup_claim(claim).await {
                self.abort_recoverable_cleanup_claims(&claims);
                return Err(error);
            }
        }
        for (index, (session, cleanup_claim)) in workers.iter().zip(&claims).enumerate() {
            if let Some(claim) = cleanup_claim.as_ref().map(|claim| &claim.worker) {
                remove_worktree_and_artifacts(
                    &self.store,
                    &session.id,
                    session.workspace_path.as_deref(),
                    &sessions_dir,
                    RecoveryMetadata::Remove,
                    Some(claim),
                )
                .await;
            }
            if let Err(error) = self.store.delete_session(&session.id) {
                if let Some(cleanup_claim) = cleanup_claim {
                    let claim = &cleanup_claim.worker;
                    let completed = self.store.complete_worker_claim(
                        &session.id,
                        &claim.incarnation_id,
                        crate::types::WorkerIncarnationState::CleanupClaimed,
                    );
                    if !matches!(completed, Ok(true)) {
                        let _ = self.store.abort_worker_cleanup_snapshot(cleanup_claim);
                    }
                }
                self.abort_recoverable_cleanup_claims(&claims[index + 1..]);
                return Err(error);
            }
            if let Some(claim) = cleanup_claim.as_ref().map(|claim| &claim.worker) {
                let _ = self.store.complete_worker_claim(
                    &session.id,
                    &claim.incarnation_id,
                    crate::types::WorkerIncarnationState::CleanupClaimed,
                );
            }
            self.emit(Event::SessionDone(session.id.clone()));
        }
        // Also kill the orchestrator's own tmux session (same id as orchestrator).
        let _ = crate::tmux::kill_session(orchestrator_id).await;
        self.store.delete_orchestrator(orchestrator_id)?;
        self.emit(Event::OrchestratorRemoved(orchestrator_id.to_string()));
        Ok(())
    }

    fn abort_recoverable_cleanup_claims(
        &self,
        claims: &[Option<crate::store::WorkerCleanupSnapshotClaim>],
    ) {
        for claim in claims.iter().flatten() {
            match self.store.abort_worker_cleanup_snapshot(claim) {
                Ok(true) => {}
                Ok(false) => tracing::error!(
                    "cleanup {}: exact claim changed before abort",
                    claim.worker.session_id
                ),
                Err(error) => tracing::error!(
                    "cleanup {}: exact claim abort failed: {error}",
                    claim.worker.session_id
                ),
            }
        }
    }

    async fn stop_recoverable_cleanup_claim(
        &self,
        claim: &crate::store::WorkerCleanupSnapshotClaim,
    ) -> anyhow::Result<()> {
        let stop_result = if claim.require_legacy_runtime_absence {
            async {
                let runtime = self
                    .store
                    .legacy_worker_runtime(&claim.worker.session_id)?
                    .ok_or_else(|| anyhow::anyhow!("adopted legacy runtime metadata changed"))?;
                anyhow::ensure!(
                    runtime.incarnation_id == claim.worker.incarnation_id,
                    "adopted legacy runtime belongs to another worker incarnation"
                );
                anyhow::ensure!(
                    crate::tmux::exact_private_session(&runtime.physical_tmux_name)
                        .await?
                        .is_none(),
                    "adopted legacy worker runtime is still live"
                );
                Ok(())
            }
            .await
        } else {
            crate::workers::stop_exact_runtime(&self.store, &claim.worker).await
        };
        let Err(stop_error) = stop_result else {
            return Ok(());
        };
        match self.store.abort_worker_cleanup_snapshot(claim) {
            Ok(true) => Err(stop_error),
            Ok(false) => Err(anyhow::anyhow!(
                "{stop_error}; exact cleanup claim changed before abort"
            )),
            Err(abort_error) => Err(anyhow::anyhow!(
                "{stop_error}; exact cleanup claim abort failed: {abort_error}"
            )),
        }
    }

    /// Kill the tmux session and delete it from the DB entirely.
    pub async fn remove_session(&self, session_id: &str) -> anyhow::Result<()> {
        let Some(session) = self.store.get_session(session_id)? else {
            return Ok(());
        };
        let claim = self
            .store
            .claim_worker_cleanup_snapshot_recoverable(session_id, session.started_at)?;
        let Some(claim) = claim else {
            let already_released = self
                .store
                .released_worker_for_cleanup_snapshot(session_id, session.started_at)?
                .is_some();
            if already_released {
                self.store.delete_session(session_id)?;
                self.emit(Event::SessionDone(session_id.to_string()));
            }
            return Ok(());
        };
        self.stop_recoverable_cleanup_claim(&claim).await?;
        let claim = &claim.worker;
        remove_worktree_and_artifacts(
            &self.store,
            session_id,
            session.workspace_path.as_deref(),
            &crate::config::AppConfig::sessions_dir(),
            RecoveryMetadata::Remove,
            Some(claim),
        )
        .await;
        self.store.delete_session(session_id)?;
        let _ = self.store.complete_worker_claim(
            session_id,
            &claim.incarnation_id,
            crate::types::WorkerIncarnationState::CleanupClaimed,
        );
        self.emit(Event::SessionDone(session_id.to_string()));
        Ok(())
    }

    /// Send a text message to a session (used by poller reactions).
    ///
    /// Gated by `AppConfig.inbox_messaging.enabled` (opt-in, default off —
    /// see `crate::messaging::deliver_message`):
    /// - off: unchanged — the message is injected as keyboard input, verified
    ///   via `tmux::send_keys` (delivery-verified + Enter-retried, PR #69).
    ///   Errors if the session has no active tmux window, tmux is
    ///   unavailable, or the message is still sitting unsubmitted at the
    ///   input prompt after the Enter retries.
    /// - on: the message is written durably to the session's file-based
    ///   inbox and a best-effort idle-wake nudge is sent; errors only if the
    ///   inbox write itself fails.
    pub async fn send_to_session(&self, session_id: &str, message: &str) -> anyhow::Result<()> {
        // `AppConfig::load()` is a small synchronous TOML read; called
        // directly (not via `spawn_blocking`) here matches the existing
        // convention elsewhere in this codebase (e.g. `main.rs::run_spawn`).
        let inbox_enabled = crate::config::AppConfig::load()
            .map(|c| c.inbox_messaging.enabled)
            .unwrap_or(false);
        // No `NINOX_DATA_DIR` fallback needed here (unlike the `ninox send`
        // CLI / `run_request_work`): this runs inside the app's own
        // process, which never has that env var set — only sessions the
        // app spawns do.
        crate::messaging::deliver_message(
            &self.store, &crate::config::AppConfig::sessions_dir(), session_id, message, inbox_enabled,
        )
        .await
    }

    /// Kill the tmux session, mark it Terminated in the DB, and emit SessionUpdated.
    pub async fn terminate_session(&self, session_id: &str) -> anyhow::Result<()> {
        // Best-effort tmux kill (session may already be dead).
        let _ = crate::tmux::kill_session(session_id).await;

        if let Some(mut session) = self.store.get_session(session_id)? {
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
        let Some(mut session) = self.store.get_session(session_id)? else {
            return Ok(());
        };
        let Some(claim) = self
            .store
            .claim_worker_cleanup_snapshot_recoverable(session_id, session.started_at)?
        else {
            return Ok(());
        };
        self.stop_recoverable_cleanup_claim(&claim).await?;
        let claim = &claim.worker;

        remove_worktree_and_artifacts(
            &self.store,
            session_id,
            session.workspace_path.as_deref(),
            sessions_dir,
            RecoveryMetadata::Retain,
            Some(claim),
        )
        .await;
        session.status = crate::types::SessionStatus::Done;
        session.terminal_at = Some(crate::lifecycle::poller::now_millis());
        let update = self.store.upsert_session(&session);
        if update.is_ok() {
            self.emit(Event::SessionUpdated(
                session,
                SessionFields::STATUS | SessionFields::TERMINAL_AT,
            ));
        }
        let _ = self.store.complete_worker_claim(
            session_id,
            &claim.incarnation_id,
            crate::types::WorkerIncarnationState::CleanupClaimed,
        );
        update?;
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum RecoveryMetadata {
    Retain,
    Remove,
}

/// Remove a session's worktree (if any) and hook artifacts — the shared
/// teardown tail of `remove_orchestrator`, `remove_session`, and
/// `cleanup_session`, so none of them leak a checked-out worktree or
/// per-session hook files.
async fn remove_worktree_and_artifacts(
    store:          &crate::store::Store,
    session_id:     &str,
    workspace_path: Option<&str>,
    sessions_dir:   &std::path::Path,
    recovery_metadata: RecoveryMetadata,
    worker: Option<&crate::types::WorkerIncarnation>,
) {
    let pooled = release_pooled_checkout(store, session_id, worker).await;
    if !pooled {
        if let Some(wp) = workspace_path {
            remove_worker_worktree(wp, session_id, recovery_metadata).await;
        }
    }
    crate::hooks::remove_session_artifacts(sessions_dir, session_id);
}

/// Release only the exact lease currently owned by `session_id`. A dirty or
/// replaced checkout is quarantined and left untouched for manual recovery.
async fn release_pooled_checkout(
    store: &crate::store::Store,
    session_id: &str,
    worker: Option<&crate::types::WorkerIncarnation>,
) -> bool {
    let Ok(Some(record)) = store.pooled_checkout_by_session(session_id) else {
        return false;
    };
    let (Some(owner_incarnation_id), Some(lease_id), Some(branch)) = (
        record.owner_incarnation_id.clone(),
        record.lease_id.clone(),
        record.branch.clone(),
    ) else {
        let _ = store.quarantine_pooled_checkout(
            &record.path,
            "active pooled checkout is missing lease metadata",
        );
        return true;
    };
    if let Some(worker) = worker {
        if owner_incarnation_id != worker.incarnation_id
            || worker.lease_id.as_deref() != Some(lease_id.as_str())
        {
            return true;
        }
    }
    let lease = crate::types::PooledCheckoutLease {
        path: record.path,
        source_repo: record.source_repo,
        common_git_dir: record.common_git_dir,
        slot: record.slot,
        kind: record.kind,
        worktree_git_dir: record.worktree_git_dir,
        worktree_identity: record.worktree_identity,
        session_id: session_id.to_string(),
        owner_incarnation_id,
        lease_id,
        branch,
    };
    let store_result = tokio::task::spawn_blocking({
        let lease = lease.clone();
        move || crate::worktree::PooledWorktree::from_lease(&lease)?.release_clean()
    })
    .await;
    match store_result {
        Ok(Ok(_)) => {
            let _ = store.release_pooled_checkout_for_incarnation(
                &lease.path,
                &lease.session_id,
                &lease.owner_incarnation_id,
                &lease.lease_id,
            );
        }
        Ok(Err(error)) => {
            let _ = store.quarantine_pooled_checkout_lease_for_incarnation(
                &lease.path,
                &lease.session_id,
                &lease.owner_incarnation_id,
                &lease.lease_id,
                &error.to_string(),
            );
        }
        Err(error) => {
            let _ = store.quarantine_pooled_checkout_lease(
                &lease.path,
                &lease.session_id,
                &lease.lease_id,
                &format!("pooled checkout release task failed: {error}"),
            );
        }
    }
    true
}

/// Remove a Ninox-managed git worktree (best-effort, never propagates errors).
///
/// Managed-root worktrees require matching sidecar metadata. Legacy nested
/// worktrees retain their strict path-shape check.
async fn remove_worker_worktree(
    workspace_path: &str,
    session_id: &str,
    recovery_metadata: RecoveryMetadata,
) {
    if let Ok(Some(metadata)) = crate::worktree::ManagedWorktree::load_for_workspace(
        std::path::Path::new(workspace_path),
        session_id,
    ) {
        let remove_metadata = matches!(recovery_metadata, RecoveryMetadata::Remove);
        let _ = tokio::task::spawn_blocking(move || {
            metadata.remove_checkout_if_matches_with_metadata(remove_metadata)
        })
        .await;
        return;
    }
    let suffix = format!("/.claude/worktrees/{session_id}");
    if !workspace_path.ends_with(&suffix) {
        return; // Not a Ninox worktree — leave it alone.
    }
    // Derive the repo root by stripping the worktree suffix.
    let repo_root = &workspace_path[..workspace_path.len() - suffix.len()];
    if repo_root.is_empty() {
        return;
    }
    let _ = tokio::process::Command::new("git")
        .args(["-C", repo_root, "worktree", "remove", "--force", workspace_path])
        .output()
        .await;
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
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None,
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
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: Some(1_000), gate_status: None,
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

    #[tokio::test]
    async fn cleanup_session_sets_done_status() {
        let store = Arc::new(
            Store::open(tempdir().unwrap().keep().join("t.db")).unwrap()
        );
        let session = crate::types::Session {
            id: "s1".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: crate::types::SessionStatus::PrOpen,
            agent_type: "c".into(), cost_usd: 0.0, started_at: 0,
            pr_number: Some(1), pr_id: Some(1),
            workspace_path: None, pid: None,
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None,
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

    #[tokio::test]
    async fn cleanup_session_removes_worktree_and_artifacts() {
        // Real git repo + real worktree so we exercise the same
        // `remove_worker_worktree` path `remove_session` uses, not a mock.
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
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None,
        };
        store.upsert_session(&session).unwrap();
        let engine = Engine::new(Arc::clone(&store));

        engine.cleanup_session_in(session_id, sessions_dir.path()).await.unwrap();

        assert!(!worktree_path.exists(), "cleanup_session must remove the worktree, like remove_session does");
        assert!(!artifact_path.exists(), "cleanup_session must remove session artifacts, like remove_session does");
        let after = store.get_session(session_id).unwrap().unwrap();
        assert!(matches!(after.status, crate::types::SessionStatus::Done));
    }


    #[tokio::test]
    async fn cleanup_session_releases_pooled_checkout_without_removing_warm_cache() {
        let root = tempdir().unwrap();
        let repo = root.path().join("widgets");
        std::fs::create_dir(&repo).unwrap();
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init", "-q"]);
        std::fs::write(repo.join(".gitignore"), "target/\n").unwrap();
        run(&["add", ".gitignore"]);
        run(&[
            "-c",
            "user.email=t@t.com",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "init",
        ]);

        let store = Arc::new(Store::open(root.path().join("ninox.db")).unwrap());
        let identity = crate::worktree::RepositoryIdentity::resolve(&repo).unwrap();
        let lease = store
            .reserve_pooled_checkout(
                &identity.top_level,
                &identity.common_git_dir,
                "pooled-done",
                "pooled-done",
            )
            .unwrap();
        let pooled = crate::worktree::PooledWorktree::create(&lease).unwrap();
        assert!(store
            .finalize_pooled_checkout(
                &lease.path,
                &lease.session_id,
                &lease.lease_id,
                &pooled.worktree_git_dir,
                &pooled.worktree_identity,
            )
            .unwrap());
        let cache = pooled.path.join("target/cache.bin");
        std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
        std::fs::write(&cache, "warm").unwrap();

        store
            .upsert_session(&crate::types::Session {
                id: "pooled-done".into(),
                orchestrator_id: None,
                name: "worker".into(),
                repo: "Owner/widgets".into(),
                status: crate::types::SessionStatus::PrOpen,
                agent_type: "c".into(),
                cost_usd: 0.0,
                started_at: 0,
                pr_number: Some(1),
                pr_id: Some(1),
                workspace_path: Some(pooled.path.to_string_lossy().to_string()),
                pid: None,
                model: None,
                context_tokens: None,
                catalogue_path: None,
                context_used_pct: None,
                context_total_tokens: None,
                context_window_size: None,
                claude_session_id: None,
                summary: None,
                terminal_at: None,
                gate_status: None,
            })
            .unwrap();
        let engine = Engine::new(store.clone());
        let sessions_dir = tempdir().unwrap();

        engine
            .cleanup_session_in("pooled-done", sessions_dir.path())
            .await
            .unwrap();

        assert!(pooled.path.is_dir());
        assert_eq!(std::fs::read_to_string(cache).unwrap(), "warm");
        let record = store
            .pooled_checkout_by_path(&pooled.path)
            .unwrap()
            .unwrap();
        assert_eq!(record.state, crate::types::PooledCheckoutState::Free);
        assert!(record.session_id.is_none());
    }

    // ── Orchestrator removal cleanup claims ───────────────────────────────────

    fn worker(id: &str, orchestrator: &str, status: crate::types::SessionStatus) -> Session {
        Session {
            id: id.into(), orchestrator_id: Some(orchestrator.into()), name: id.into(),
            repo: "r".into(), status, agent_type: "claude-code".into(),
            cost_usd: 0.0, started_at: 0, pr_number: None, pr_id: None,
            workspace_path: None, pid: None, model: None, context_tokens: None,
            catalogue_path: None, context_used_pct: None, context_total_tokens: None,
            context_window_size: None, claude_session_id: None, summary: None,
            terminal_at: None, gate_status: None,
        }
    }

    /// Store + engine seeded with `workers`.
    fn worker_fixture(workers: &[Session]) -> (Arc<Store>, Arc<Engine>) {
        let store = Arc::new(Store::open(tempdir().unwrap().keep().join("t.db")).unwrap());
        for w in workers {
            store.upsert_session(w).unwrap();
        }
        let engine = Engine::new(Arc::clone(&store));
        (store, engine)
    }

    #[tokio::test]
    async fn remove_orchestrator_does_not_delete_workers_before_all_exact_stops_succeed() {
        use crate::types::{Orchestrator, SessionStatus::Terminated, WorkerIncarnationState};

        let root = tempdir().unwrap();
        let safe_id = format!("remove-orch-safe-{}", uuid::Uuid::new_v4());
        let refused_id = format!("remove-orch-refused-{}", uuid::Uuid::new_v4());
        let mut safe = worker(&safe_id, "orch-1", Terminated);
        safe.started_at = 20;
        safe.workspace_path = Some(root.path().to_string_lossy().into_owned());
        let mut refused = worker(&refused_id, "orch-1", Terminated);
        refused.started_at = 10;
        refused.workspace_path = Some(root.path().to_string_lossy().into_owned());
        let (store, engine) = worker_fixture(&[safe, refused]);
        store
            .upsert_orchestrator(&Orchestrator {
                id: "orch-1".into(), name: "orchestrator".into(), created_at: 0,
            })
            .unwrap();
        for (session_id, started_at) in [(&safe_id, 20), (&refused_id, 10)] {
            let worker = store
                .prepare_worker_incarnation(
                    session_id,
                    Some("orch-1"),
                    started_at,
                    root.path().to_str().unwrap(),
                    false,
                    3,
                )
                .unwrap();
            assert!(store
                .bind_worker_incarnation(
                    session_id,
                    &worker.incarnation_id,
                    root.path().to_str().unwrap(),
                    root.path().to_str().unwrap(),
                    None,
                )
                .unwrap());
        }
        crate::tmux::create_session(
            &refused_id,
            root.path().to_str().unwrap(),
            "sleep 30",
            &[("NINOX_WORKER_INCARNATION", "successor")],
        )
        .await
        .unwrap();
        let mut events = engine.subscribe();

        let result = engine.remove_orchestrator("orch-1").await;
        let runtime_survived = crate::tmux::has_session(&refused_id).await;
        let sessions_survived = [safe_id.as_str(), refused_id.as_str()]
            .into_iter()
            .all(|id| store.get_session(id).unwrap().is_some());
        let claims_aborted = [safe_id.as_str(), refused_id.as_str()]
            .into_iter()
            .all(|id| {
                store
                    .current_worker_incarnation(id)
                    .unwrap()
                    .is_some_and(|worker| matches!(worker.state, WorkerIncarnationState::Active))
            });
        let orchestrator_survived =
            store.list_orchestrators().unwrap().iter().any(|orchestrator| orchestrator.id == "orch-1");
        crate::tmux::kill_private_session(&refused_id).await.unwrap();

        assert!(result.is_err());
        assert!(runtime_survived, "a mismatched runtime must never be killed");
        assert!(sessions_survived, "no records may be deleted before all stops succeed");
        assert!(claims_aborted, "every preclaimed worker must remain retryable");
        assert!(orchestrator_survived);
        assert!(matches!(
            events.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn remove_orchestrator_aborts_remaining_claims_after_delete_failure() {
        use crate::types::{Orchestrator, SessionStatus::Terminated, WorkerIncarnationState};

        let root = tempdir().unwrap();
        let db = root.path().join("t.db");
        let store = Arc::new(Store::open(&db).unwrap());
        let removed_id = format!("remove-orch-first-{}", uuid::Uuid::new_v4());
        let failed_id = format!("remove-orch-failed-{}", uuid::Uuid::new_v4());
        for (session_id, started_at) in [(&removed_id, 20), (&failed_id, 10)] {
            let mut session = worker(session_id, "orch-1", Terminated);
            session.started_at = started_at;
            session.workspace_path = Some(root.path().to_string_lossy().into_owned());
            store.upsert_session(&session).unwrap();
            let worker = store
                .prepare_worker_incarnation(
                    session_id,
                    Some("orch-1"),
                    started_at,
                    root.path().to_str().unwrap(),
                    false,
                    3,
                )
                .unwrap();
            assert!(store
                .bind_worker_incarnation(
                    session_id,
                    &worker.incarnation_id,
                    root.path().to_str().unwrap(),
                    root.path().to_str().unwrap(),
                    None,
                )
                .unwrap());
        }
        store
            .upsert_orchestrator(&Orchestrator {
                id: "orch-1".into(), name: "orchestrator".into(), created_at: 0,
            })
            .unwrap();
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(&format!(
            "CREATE TRIGGER refuse_session_delete
             BEFORE DELETE ON sessions WHEN OLD.id='{}'
             BEGIN SELECT RAISE(ABORT, 'refused'); END;",
            failed_id.replace('\'', "''"),
        ))
        .unwrap();
        drop(conn);
        let engine = Engine::new(store.clone());
        let mut events = engine.subscribe();

        let result = engine.remove_orchestrator("orch-1").await;

        assert!(result.is_err());
        assert!(store.get_session(&removed_id).unwrap().is_none());
        assert!(store.get_session(&failed_id).unwrap().is_some());
        assert!(store
            .current_worker_incarnation(&failed_id)
            .unwrap()
            .is_some_and(|worker| matches!(worker.state, WorkerIncarnationState::Released)));
        assert!(store.list_orchestrators().unwrap().iter().any(|orchestrator| orchestrator.id == "orch-1"));
        assert!(matches!(
            events.try_recv(),
            Ok(Event::SessionDone(session_id)) if session_id == removed_id
        ));
        assert!(matches!(
            events.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn remove_worker_worktree_ignores_non_ninox_paths() {
        // Should be a no-op for paths that don't match .claude/worktrees/{id}.
        // We just verify it doesn't panic or error.
        remove_worker_worktree(
            "/some/random/path",
            "s1",
            RecoveryMetadata::Remove,
        )
        .await;
        remove_worker_worktree(
            "/repo/.claude/worktrees/other-id",
            "s1",
            RecoveryMetadata::Remove,
        )
        .await;
        remove_worker_worktree("", "s1", RecoveryMetadata::Remove).await;
    }
}
