//! The spawn side of the per-location concurrent session limit
//! (`ninox_core::session_limits`): what a queued spawn stores, the
//! admission call every spawn path makes, and the launcher the poller
//! drains the queue through.
//!
//! Each spawn path is split into validation (run at request time, while the
//! caller's identity can still be checked) and a launch half that takes
//! only serialisable inputs, so a queued spawn can be started later by a
//! different process.

use std::sync::Arc;

use ninox_core::brain_set::BrainSet;
use ninox_core::config::{AgentConfig, AppConfig};
use ninox_core::session_limits::{self, QueueLauncher};
use ninox_core::store::{Admission, AdmitError, NewQueueEntry, QueueEntry, QueueKind, Store};
use ninox_core::types::{Session, SessionStatus, SuccessCriterion};
use serde::{Deserialize, Serialize};

/// `ninox spawn` after validation (see `cli::run_spawn`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerLaunch {
    pub id:               String,
    /// The repo the worktree is cut from.
    pub workspace:        String,
    pub display_name:     String,
    pub summary:          Option<String>,
    /// Worker-type-composed prompt, before the worker-context footer.
    pub prompt:           String,
    pub orchestrator_id:  Option<String>,
    pub agent:            AgentConfig,
    pub worker_type:      Option<String>,
    pub success_criteria: Vec<SuccessCriterion>,
    pub started_at:       i64,
    /// The spawner's own `NINOX_BRAIN`/`NINOX_CONFIG`, forwarded to the
    /// worker. Captured at request time: the process that eventually
    /// launches a queued spawn has its own environment.
    pub ninox_brain:      Option<String>,
    pub ninox_config:     Option<String>,
    /// The worker's validated brain set. `ninox_brain` stays its write
    /// path so an older binary draining the queue still lands it right.
    #[serde(default)]
    pub brain:            Option<BrainSet>,
}

/// `cli::spawn_orchestrator_common` after validation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchestratorLaunch {
    pub id:                     String,
    pub name:                   String,
    pub prompt:                 Option<String>,
    pub parent_orchestrator_id: Option<String>,
    pub agent:                  AgentConfig,
    /// The write catalogue; kept for older binaries draining the queue.
    pub catalogue_path:         String,
    #[serde(default)]
    pub brain:                  Option<BrainSet>,
    /// The spawner's `NINOX_ORCHESTRATOR_ID`, which the brief is attributed to.
    pub spawner:                Option<String>,
    pub started_at:             i64,
}

/// `cli::spawn_standalone_worker` after validation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StandaloneLaunch {
    pub id:               String,
    pub name:             String,
    pub workspace_input:  String,
    pub worker_type:      String,
    pub agent:            AgentConfig,
    /// The write catalogue; kept for older binaries draining the queue.
    pub catalogue_path:   String,
    #[serde(default)]
    pub brain:            Option<BrainSet>,
    /// Fully composed initial prompt, if any.
    pub prompt:           Option<String>,
    pub task_brief:       String,
    pub success_criteria: Vec<SuccessCriterion>,
    pub started_at:       i64,
    /// Cut the session its own worktree on top of the resolved workspace
    /// (the desktop Spawn dialog's behaviour).
    #[serde(default)]
    pub isolate_worktree: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum QueuedLaunch {
    CliWorker(WorkerLaunch),
    Orchestrator(OrchestratorLaunch),
    Standalone(StandaloneLaunch),
    /// A fleet-restore relaunch of the existing row with the entry's id.
    Relaunch,
    /// A user Resume (`--resume`) of the existing, not-running row with the
    /// entry's id.
    Resume { is_orchestrator: bool },
}

/// A session row with only identity fields set; the launch halves fill in
/// the rest. Also what a queued spawn displays as.
pub fn base_session(id: &str, name: &str, agent: &AgentConfig, started_at: i64) -> Session {
    Session {
        id: id.to_string(), orchestrator_id: None, name: name.to_string(), repo: String::new(),
        status: SessionStatus::Working, agent_type: agent.harness.clone(), cost_usd: 0.0,
        started_at, pr_number: None, pr_id: None, workspace_path: None, pid: None,
        model: agent.model.clone(), context_tokens: None, brain_reads: Vec::new(), brain_write: None,
        context_used_pct: None, context_total_tokens: None, context_window_size: None,
        claude_session_id: None, summary: None, terminal_at: None, gate_status: None,
        merged_at: None, activity: Default::default(), activity_note: None, activity_since: None,
        machine_id: None, worker_type: None, success_criteria: Vec::new(),
        completion_outcome: None, self_completed_at: None, cleanup_result: None, cleanup_at: None,
    }
}

impl WorkerLaunch {
    /// A payload queued by a binary that predates brain sets carries only
    /// `ninox_brain`: it becomes a single-catalogue set.
    pub fn brain_set(&self) -> Option<BrainSet> {
        self.brain.clone().or_else(|| self.ninox_brain.as_ref().map(BrainSet::single))
    }

    pub fn skeleton(&self) -> Session {
        let mut s = base_session(&self.id, &self.display_name, &self.agent, self.started_at);
        s.orchestrator_id = self.orchestrator_id.clone();
        s.workspace_path = Some(self.workspace.clone());
        if let Some(b) = self.brain_set() {
            b.apply_to(&mut s);
        }
        s.summary = self.summary.clone();
        s.worker_type = self.worker_type.clone();
        s.success_criteria = self.success_criteria.clone();
        s
    }
}

impl OrchestratorLaunch {
    pub fn brain_set(&self) -> BrainSet {
        self.brain.clone().unwrap_or_else(|| BrainSet::single(&self.catalogue_path))
    }

    pub fn skeleton(&self) -> Session {
        let mut s = base_session(&self.id, &self.name, &self.agent, self.started_at);
        s.orchestrator_id = self.parent_orchestrator_id.clone();
        self.brain_set().apply_to(&mut s);
        s.worker_type = Some("Orchestrator".to_string());
        s
    }
}

impl StandaloneLaunch {
    pub fn brain_set(&self) -> BrainSet {
        self.brain.clone().unwrap_or_else(|| BrainSet::single(&self.catalogue_path))
    }

    pub fn skeleton(&self) -> Session {
        let mut s = base_session(&self.id, &self.name, &self.agent, self.started_at);
        s.workspace_path = Some(self.workspace_input.clone());
        self.brain_set().apply_to(&mut s);
        s.worker_type = Some(self.worker_type.clone());
        s.success_criteria = self.success_criteria.clone();
        s
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admit {
    /// Start now; call [`release`] once the spawn has finished either way.
    Admitted,
    Queued { ahead: usize, message: String },
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// The one admission call every spawn path makes before its first side
/// effect. Reads the limit from the config on disk, so a limit changed in
/// another process applies immediately.
pub fn admit(store: &Store, kind: QueueKind, session: &Session, payload: &QueuedLaunch, no_queue: bool) -> anyhow::Result<Admit> {
    let config = AppConfig::load_or_default();
    let location = session.machine_id.as_deref();
    let limit = config.limits.limit_for(location);
    let payload = serde_json::to_string(payload)?;
    let label = session_limits::location_label(&config, location);
    match store.admit_session(NewQueueEntry { kind, session, payload: &payload }, limit, no_queue, now_ms()) {
        Ok(Admission::Admitted) => Ok(Admit::Admitted),
        Ok(Admission::Queued { ahead }) => Ok(Admit::Queued {
            ahead,
            message: session_limits::queued_message(&session.id, ahead, &label),
        }),
        Err(AdmitError::LimitReached { running, limit }) => anyhow::bail!(
            "session limit reached on {label} ({running}/{limit} running) — not queued because \
             --no-queue was given. Retry later, or raise the limit with `ninox limits set {} <n>`",
            if location.is_none() { "local" } else { label.as_str() },
        ),
        Err(AdmitError::AlreadyQueued) => anyhow::bail!(
            "'{}' is already queued — cancel it with `ninox reap {}` or wait for it to start",
            session.id, session.id,
        ),
        Err(AdmitError::IdTaken) => anyhow::bail!("a session named {} already exists — pick another name", session.id),
        Err(AdmitError::Store(e)) => Err(e),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeGate {
    /// The session is running (a restart in place): it replaces itself, so
    /// the limit doesn't apply and nothing is reserved.
    NotGated,
    /// Start now; call [`release`] once the resume has finished.
    Admitted,
    Queued { message: String },
}

/// The limit for a user Resume: only resuming a session that isn't running
/// (Interrupted, Done, Terminated) adds one, so only that is gated.
/// `no_queue` fails fast instead of queueing.
pub fn admit_resume(store: &Store, session: &Session, is_orchestrator: bool, no_queue: bool) -> anyhow::Result<ResumeGate> {
    if !session.status.is_terminal() {
        return Ok(ResumeGate::NotGated);
    }
    let payload = QueuedLaunch::Resume { is_orchestrator };
    Ok(match admit(store, QueueKind::Relaunch, session, &payload, no_queue)? {
        Admit::Admitted => ResumeGate::Admitted,
        Admit::Queued { message, .. } => ResumeGate::Queued { message },
    })
}

/// Give back an admitted spawn's slot reservation. Its sessions row (if the
/// spawn got that far) is what counts from here on.
pub fn release(store: &Store, id: &str) {
    if let Err(e) = store.release_queue_entry(id) {
        tracing::warn!("session queue: release {id}: {e}");
    }
}

/// Arbitrary; how long a queued session's harness gets to draw its input
/// prompt before held messages are sent anyway.
const HELD_MESSAGE_PROMPT_WAIT: std::time::Duration = std::time::Duration::from_secs(90);

/// What `Poller::with_queue_launcher` runs for each claimed entry.
pub fn queue_launcher(store: Arc<Store>) -> QueueLauncher {
    Arc::new(move |entry: QueueEntry| {
        let store = store.clone();
        Box::pin(async move { launch_entry(store, entry).await })
    })
}

/// Drops a claimed entry (and anything held for it) if its launch never
/// reaches a normal finish — including a panic inside the spawn code, which
/// would otherwise leave the slot reserved for as long as this process
/// lives.
struct ClaimGuard {
    store: Arc<Store>,
    id:    String,
    armed: bool,
}

impl Drop for ClaimGuard {
    fn drop(&mut self) {
        if self.armed {
            if let Err(e) = self.store.drop_queue_entry(&self.id) {
                tracing::warn!("session queue: drop {}: {e}", self.id);
            }
        }
    }
}

async fn launch_entry(store: Arc<Store>, entry: QueueEntry) {
    let id = entry.id.clone();
    let mut guard = ClaimGuard { store: store.clone(), id: id.clone(), armed: true };
    let config = AppConfig::load_or_default();
    let payload: QueuedLaunch = match serde_json::from_str(&entry.payload) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("session queue: {id} has an unreadable launch request, dropping it: {e}");
            return;
        }
    };
    let result = match payload {
        QueuedLaunch::CliWorker(p) => crate::cli::launch_cli_worker(&store, &config, p).await,
        QueuedLaunch::Orchestrator(p) => crate::cli::launch_orchestrator(&store, &config, p).await.map(|_| ()),
        QueuedLaunch::Standalone(p) => crate::cli::launch_standalone_worker(&store, &config, p).await.map(|_| ()),
        QueuedLaunch::Resume { is_orchestrator } => match store.get_session(&id) {
            Ok(Some(session)) if session.status.is_terminal() => {
                let engine = ninox_core::events::Engine::new(store.clone());
                crate::restart::resume_session(engine, session, is_orchestrator, &config, false)
                    .await
                    .map(|_| ())
                    .map_err(|e| anyhow::anyhow!(e))
            }
            // Removed, or already running again (resumed some other way):
            // nothing left to start.
            Ok(_) => Ok(()),
            Err(e) => Err(e),
        },
        QueuedLaunch::Relaunch => match crate::fleet::restore_admitted(store.clone(), &config, &id).await {
            Err(e) if crate::fleet::is_restore_busy(&e) => {
                // Another restore holds the lease; keep the place in line.
                guard.armed = false;
                if let Err(e) = store.requeue_entry(&id) {
                    tracing::warn!("session queue: requeue {id}: {e}");
                }
                return;
            }
            other => other,
        },
    };
    if let Err(e) = result {
        let dropped = store.held_message_count(&id).unwrap_or(0);
        tracing::error!("session queue: starting {id} failed ({dropped} held message(s) dropped): {e:#}");
        return;
    }
    tracing::info!("session queue: started {id}");
    // Messages keep being held while the entry is launching, so the ones
    // sent during startup queue up behind the earlier ones instead of
    // overtaking them; the entry is released only after this flush.
    if store.held_message_count(&id).unwrap_or(0) > 0
        && !ninox_core::runtime::wait_for_input_prompt(&id, HELD_MESSAGE_PROMPT_WAIT).await
    {
        tracing::warn!("session queue: {id} showed no input prompt; delivering held messages anyway");
    }
    flush_held(&store, &config, &id).await;
    guard.armed = false;
    release(&store, &id);
    // Anything held between that flush and the release.
    flush_held(&store, &config, &id).await;
}

async fn flush_held(store: &Store, config: &AppConfig, id: &str) {
    match ninox_core::messaging::deliver_held_messages(store, &AppConfig::sessions_dir(), id, config.send_mechanism()).await {
        Ok(0) => {}
        Ok(n) => tracing::info!("session queue: delivered {n} held message(s) to {id}"),
        Err(e) => tracing::warn!("session queue: delivering held messages to {id}: {e}"),
    }
}

/// Sessions, orchestrators, queue positions and per-location counts as the
/// UIs and `ninox list` show them: the queue folded into the session list
/// (see `session_limits::overlay_queue`), and a group for each queued
/// orchestrator, which has no orchestrators row until it starts.
pub struct QueueView {
    pub sessions:      Vec<Session>,
    pub orchestrators: Vec<ninox_core::types::Orchestrator>,
    pub positions:     std::collections::HashMap<String, usize>,
    pub limits:        Vec<session_limits::LocationStatus>,
}

pub fn queue_view(store: &Store, config: &AppConfig) -> anyhow::Result<QueueView> {
    let mut sessions = store.list_sessions()?;
    let mut orchestrators = store.list_orchestrators()?;
    let queue = store.list_queue()?;
    session_limits::overlay_queue(&mut sessions, &queue);
    for s in sessions.iter().filter(|s| matches!(s.status, SessionStatus::Queued | SessionStatus::Spawning)) {
        if s.worker_type.as_deref() == Some("Orchestrator") && !orchestrators.iter().any(|o| o.id == s.id) {
            orchestrators.push(ninox_core::types::Orchestrator { id: s.id.clone(), name: s.name.clone(), created_at: s.started_at });
        }
    }
    Ok(QueueView {
        sessions,
        orchestrators,
        positions: session_limits::queue_positions(&queue),
        limits: session_limits::summarize(store, config)?,
    })
}

/// The header counter: local always, remotes only when they have activity.
pub fn limits_headline(limits: &[session_limits::LocationStatus]) -> String {
    limits
        .iter()
        .filter(|l| l.machine_id.is_none() || l.running > 0 || l.orchestrators > 0 || l.queued > 0)
        .map(|l| l.headline())
        .collect::<Vec<_>>()
        .join("  ·  ")
}

/// The badge for a queued session, e.g. "queued · #2".
pub fn queued_badge(position: Option<usize>) -> String {
    match position {
        Some(n) => format!("queued · #{n}"),
        None => "queued".to_string(),
    }
}

#[cfg(test)]
mod headline_tests {
    use super::*;

    #[test]
    fn a_remote_with_only_orchestrators_stays_in_the_header() {
        let local = session_limits::LocationStatus {
            machine_id: None, label: "local".into(), running: 0, orchestrators: 0, limit: Some(6), queued: 0,
        };
        let remote = session_limits::LocationStatus {
            machine_id: Some("m1".into()), label: "gpu-box".into(), running: 0, orchestrators: 2, limit: None, queued: 0,
        };
        assert_eq!(
            limits_headline(&[local, remote]),
            "0/6 workers running locally  ·  0 workers running on gpu-box · 2 orchestrators",
        );
    }
}
