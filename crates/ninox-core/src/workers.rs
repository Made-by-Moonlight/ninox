use crate::{
    store::Store,
    tmux,
    types::{
        OrchestratorRuntimeIdentity, PooledCheckoutState, Session, SessionStatus,
        WorkerFinalizationIntent, WorkerIncarnation, WorkerIncarnationState,
    },
    worktree::ManagedWorktree,
};
use anyhow::{Context, Result};
use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};

pub const WORKER_INSPECTION_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HarnessState {
    Running,
    Missing,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct HarnessInspection {
    pub state: HarnessState,
    pub pid: Option<u32>,
    pub exit_status: Option<i32>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InspectionPhase {
    Preparing,
    Starting,
    Running,
    Finalizing,
    Retained,
    CleanupClaimed,
    Cleaned,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceKind {
    PooledWorktree,
    ManagedWorktree,
    GitWorktree,
    GitWorkspace,
    NonGit,
    Missing,
    Unavailable,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct GitInspection {
    pub branch: Option<String>,
    pub head: Option<String>,
    pub dirty: Option<bool>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WorkspaceInspection {
    pub path: Option<PathBuf>,
    pub kind: WorkspaceKind,
    pub exists: bool,
    pub git: Option<GitInspection>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PoolInspection {
    pub state: PooledCheckoutState,
    pub owned_by_worker: bool,
    pub branch: Option<String>,
    pub quarantine_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RetentionState {
    Automatic,
    Retained,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RetentionInspection {
    pub state: RetentionState,
    pub retained_at: Option<i64>,
    pub finalized_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkerInspection {
    pub schema_version: u32,
    pub session_id: String,
    pub incarnation_id: String,
    pub phase: InspectionPhase,
    pub physical_tmux_name: String,
    pub orchestrator_id: String,
    pub name: String,
    pub session_status: SessionStatus,
    pub harness: String,
    pub harness_session_id: Option<String>,
    pub runtime: HarnessInspection,
    pub workspace: WorkspaceInspection,
    pub pool: Option<PoolInspection>,
    pub retention: RetentionInspection,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FinalizeOutcome {
    Finalized,
    AlreadyFinalized,
}

#[derive(Debug, Clone, Serialize)]
pub struct FinalizeResult {
    pub outcome: FinalizeOutcome,
    pub worker: WorkerInspection,
}

pub async fn register_live_orchestrator_runtime(
    store: &Store,
    orchestrator_id: &str,
) -> Result<()> {
    let pane = tmux::private_pane_identity(orchestrator_id)
        .await?
        .context("new orchestrator runtime is missing")?;
    anyhow::ensure!(
        pane.physical_tmux_name == orchestrator_id,
        "new orchestrator runtime identity is malformed"
    );
    let registered_at = crate::lifecycle::poller::now_millis();
    store.register_orchestrator_runtime(&OrchestratorRuntimeIdentity {
        orchestrator_id: orchestrator_id.to_string(),
        runtime_id: uuid::Uuid::new_v4().to_string(),
        server_epoch: pane.server_epoch,
        physical_tmux_name: pane.physical_tmux_name,
        pane_id: pane.pane_id,
        root_pid: pane.pane_pid,
        root_created_at: pane.pane_created_at,
        registered_at,
    })
}

fn require_execution_role(
    execution_role: Option<&str>,
    legacy_caller_type: Option<&str>,
    required_role: &str,
    denial: &str,
) -> Result<()> {
    let role = execution_role.or(legacy_caller_type);
    if let Some(role) = role {
        anyhow::ensure!(
            matches!(role, "worker" | "orchestrator"),
            "unrecognized NINOX_EXECUTION_ROLE={role:?}"
        );
    }
    anyhow::ensure!(role == Some(required_role), "{denial}");
    Ok(())
}

/// Environment values identify the claimed orchestrator; immutable tmux and
/// OS process identities prove the caller actually runs under that runtime.
pub fn authorize_orchestrator(
    store: &Store,
    orchestrator_id: Option<&str>,
    execution_role: Option<&str>,
    legacy_caller_type: Option<&str>,
    runtime: Option<&tmux::TmuxPaneIdentity>,
) -> Result<String> {
    let orchestrator_id = orchestrator_id
        .filter(|id| !id.is_empty())
        .context("NINOX_ORCHESTRATOR_ID is not set")?;
    require_execution_role(
        execution_role,
        legacy_caller_type,
        "orchestrator",
        "worker inspection is only available inside the owning orchestrator session",
    )?;
    let runtime = runtime.context("caller is not running inside a private Ninox pane")?;
    anyhow::ensure!(
        store.is_orchestrator(orchestrator_id)?,
        "caller session is not a persisted orchestrator"
    );
    let mut persisted = store.orchestrator_runtime_identity(orchestrator_id)?;
    if persisted.is_none()
        && runtime.physical_tmux_name == orchestrator_id
        && tmux::caller_descends_from(runtime.pane_pid)
    {
        let registered_at = crate::lifecycle::poller::now_millis();
        let migrated = OrchestratorRuntimeIdentity {
            orchestrator_id: orchestrator_id.to_string(),
            runtime_id: format!(
                "migrated:{}:{}:{registered_at}",
                runtime.server_epoch, runtime.pane_id
            ),
            server_epoch: runtime.server_epoch.clone(),
            physical_tmux_name: runtime.physical_tmux_name.clone(),
            pane_id: runtime.pane_id.clone(),
            root_pid: runtime.pane_pid,
            root_created_at: runtime.pane_created_at,
            registered_at,
        };
        if store.register_migrated_orchestrator_runtime(&migrated)? {
            persisted = Some(migrated);
        }
    }
    let persisted = persisted.context("orchestrator has no persisted runtime identity")?;
    anyhow::ensure!(
        persisted.server_epoch == runtime.server_epoch
            && persisted.physical_tmux_name == runtime.physical_tmux_name
            && persisted.pane_id == runtime.pane_id
            && persisted.root_pid == runtime.pane_pid
            && (persisted.root_created_at - runtime.pane_created_at).abs() <= 2_000
            && tmux::caller_descends_from(persisted.root_pid),
        "caller is not running under the immutable orchestrator runtime"
    );
    Ok(orchestrator_id.to_string())
}

pub fn authorize_worker_completion(
    store: &Store,
    session_id: Option<&str>,
    incarnation_id: Option<&str>,
    orchestrator_id: Option<&str>,
    caller_roles: (Option<&str>, Option<&str>),
    runtime: Option<&tmux::TmuxPaneIdentity>,
    runtime_incarnation: Option<&str>,
) -> Result<WorkerIncarnation> {
    require_execution_role(
        caller_roles.0,
        caller_roles.1,
        "worker",
        "completion is only available inside the current worker runtime",
    )?;
    let session_id = session_id
        .filter(|value| !value.is_empty())
        .context("NINOX_SESSION is not set")?;
    let incarnation_id = incarnation_id
        .filter(|value| !value.is_empty())
        .context("NINOX_WORKER_INCARNATION is not set")?;
    let orchestrator_id = orchestrator_id
        .filter(|value| !value.is_empty())
        .context("NINOX_ORCHESTRATOR_ID is not set")?;
    anyhow::ensure!(
        session_id != orchestrator_id && !store.is_orchestrator(session_id)?,
        "orchestrators cannot complete themselves as workers"
    );
    anyhow::ensure!(
        store.is_orchestrator(orchestrator_id)?,
        "worker owner is not a persisted orchestrator"
    );
    let session = store
        .get_session(session_id)?
        .with_context(|| format!("worker {session_id:?} not found"))?;
    anyhow::ensure!(
        session.orchestrator_id.as_deref() == Some(orchestrator_id),
        "worker completion owner does not match its session owner"
    );
    let worker = store
        .current_worker_incarnation(session_id)?
        .context("worker has no current incarnation")?;
    anyhow::ensure!(
        worker.incarnation_id == incarnation_id,
        "worker completion is stale; the current incarnation changed"
    );
    anyhow::ensure!(
        worker.orchestrator_id.as_deref() == Some(orchestrator_id),
        "worker completion owner does not match its exact incarnation"
    );
    let runtime = runtime.context("caller is not running inside a private Ninox pane")?;
    anyhow::ensure!(
        tmux::caller_descends_from(runtime.pane_pid),
        "caller is not running under the current worker runtime"
    );
    if let Some(legacy) = store.legacy_worker_runtime(session_id)? {
        anyhow::ensure!(
            legacy.incarnation_id == incarnation_id
                && legacy.physical_tmux_name == runtime.physical_tmux_name
                && legacy.pane_id == runtime.pane_id
                && legacy.pane_pid == runtime.pane_pid,
            "worker runtime was replaced; refusing stale completion"
        );
    } else {
        anyhow::ensure!(
            runtime.physical_tmux_name == session_id && runtime_incarnation == Some(incarnation_id),
            "worker runtime was replaced; refusing stale completion"
        );
    }
    Ok(worker)
}

pub async fn list_owned_workers(
    store: &Store,
    orchestrator_id: &str,
) -> Result<Vec<WorkerInspection>> {
    let mut workers = Vec::new();
    for session in store.sessions_by_orchestrator(orchestrator_id)? {
        workers.push(inspect_session(store, session).await?);
    }
    Ok(workers)
}

pub async fn inspect_owned_worker(
    store: &Store,
    orchestrator_id: &str,
    session_id: &str,
) -> Result<WorkerInspection> {
    let session = store
        .get_owned_session(orchestrator_id, session_id)?
        .with_context(|| format!("worker {session_id:?} not found"))?;
    inspect_session(store, session).await
}

pub async fn finalize_owned_worker(
    store: Arc<Store>,
    orchestrator_id: &str,
    session_id: &str,
) -> Result<FinalizeResult> {
    let intent = store.begin_worker_finalization(orchestrator_id, session_id)?;
    let (outcome, worker) = match intent {
        WorkerFinalizationIntent::Apply(worker) => (FinalizeOutcome::Finalized, worker),
        WorkerFinalizationIntent::AlreadyFinalized(worker) => {
            (FinalizeOutcome::AlreadyFinalized, worker)
        }
    };
    if outcome == FinalizeOutcome::Finalized {
        stop_exact_runtime(&store, &worker)
            .await
            .with_context(|| format!("stop worker {session_id:?}"))?;
        anyhow::ensure!(
            store.complete_worker_finalization(session_id, &worker.incarnation_id)?,
            "worker operation became stale"
        );
    }
    Ok(FinalizeResult {
        outcome,
        worker: inspect_owned_worker(&store, orchestrator_id, session_id).await?,
    })
}

pub(crate) async fn stop_exact_runtime(store: &Store, worker: &WorkerIncarnation) -> Result<()> {
    if let Some(legacy) = store.legacy_worker_runtime(&worker.session_id)? {
        anyhow::ensure!(
            legacy.incarnation_id == worker.incarnation_id,
            "legacy runtime belongs to another worker incarnation"
        );
        if let Some(runtime) = tmux::exact_private_session(&legacy.physical_tmux_name).await? {
            anyhow::ensure!(
                runtime.pane_id == legacy.pane_id && runtime.pane_pid == legacy.pane_pid,
                "legacy runtime identity changed; refusing to stop a successor"
            );
            tmux::kill_private_session(&legacy.physical_tmux_name).await?;
        }
        anyhow::ensure!(
            tmux::exact_private_session(&legacy.physical_tmux_name)
                .await?
                .is_none(),
            "worker runtime absence could not be confirmed after stop"
        );
        return Ok(());
    }

    if tmux::exact_private_session(&worker.session_id)
        .await?
        .is_none()
    {
        return Ok(());
    }
    let incarnation =
        tmux::private_session_env(&worker.session_id, "NINOX_WORKER_INCARNATION").await?;
    anyhow::ensure!(
        incarnation.as_deref() == Some(worker.incarnation_id.as_str()),
        "worker runtime changed; refusing to stop a successor"
    );
    tmux::kill_private_session(&worker.session_id).await?;
    anyhow::ensure!(
        tmux::exact_private_session(&worker.session_id)
            .await?
            .is_none(),
        "worker runtime absence could not be confirmed after stop"
    );
    Ok(())
}

async fn inspect_session(store: &Store, session: Session) -> Result<WorkerInspection> {
    let worker = store
        .current_worker_incarnation(&session.id)?
        .context("worker has no current incarnation")?;
    let legacy = store.legacy_worker_runtime(&session.id)?;
    let physical_tmux_name = legacy.as_ref().map_or_else(
        || session.id.clone(),
        |runtime| runtime.physical_tmux_name.clone(),
    );
    let exact_runtime = tmux::exact_private_session(&physical_tmux_name).await?;
    let runtime_matches = match (&legacy, &exact_runtime) {
        (Some(expected), Some(actual)) => {
            expected.incarnation_id == worker.incarnation_id
                && expected.pane_id == actual.pane_id
                && expected.pane_pid == actual.pane_pid
        }
        (None, Some(_)) => {
            tmux::private_session_env(&physical_tmux_name, "NINOX_WORKER_INCARNATION")
                .await?
                .as_deref()
                == Some(worker.incarnation_id.as_str())
        }
        (_, None) => false,
    };
    let runtime = if runtime_matches {
        HarnessInspection {
            state: HarnessState::Running,
            pid: exact_runtime.as_ref().map(|runtime| runtime.pane_pid),
            exit_status: None,
        }
    } else {
        HarnessInspection {
            state: HarnessState::Missing,
            pid: None,
            exit_status: None,
        }
    };

    let pool_record = store.pooled_checkout_by_session(&session.id)?.or_else(|| {
        session
            .workspace_path
            .as_deref()
            .and_then(|workspace| store.pooled_checkout_by_path(Path::new(workspace)).ok())
            .flatten()
    });
    let pool = pool_record.as_ref().map(|record| {
        let owned_by_worker = record.session_id.as_deref() == Some(session.id.as_str())
            && record.owner_incarnation_id.as_deref() == Some(worker.incarnation_id.as_str());
        PoolInspection {
            state: record.state,
            owned_by_worker,
            branch: owned_by_worker.then(|| record.branch.clone()).flatten(),
            quarantine_reason: (owned_by_worker
                && matches!(record.state, PooledCheckoutState::Quarantined))
            .then(|| record.quarantine_reason.clone())
            .flatten(),
        }
    });
    let workspace = inspect_workspace(
        session.workspace_path.as_deref(),
        &session.id,
        pool_record.is_some(),
    );
    let finalization = store.worker_finalization(&session.id)?;
    let retention = RetentionInspection {
        state: if finalization.is_some() {
            RetentionState::Retained
        } else {
            RetentionState::Automatic
        },
        retained_at: finalization
            .as_ref()
            .map(|finalization| finalization.claimed_at)
            .or(session.terminal_at),
        finalized_at: finalization.and_then(|finalization| finalization.finalized_at),
    };
    let phase = if store
        .worker_finalization(&session.id)?
        .is_some_and(|finalization| finalization.finalized_at.is_none())
    {
        InspectionPhase::Finalizing
    } else {
        inspection_phase(worker.state, &session.status, runtime_matches)
    };

    Ok(WorkerInspection {
        schema_version: WORKER_INSPECTION_SCHEMA_VERSION,
        session_id: session.id,
        incarnation_id: worker.incarnation_id,
        phase,
        physical_tmux_name,
        orchestrator_id: session
            .orchestrator_id
            .expect("owned session must have an orchestrator"),
        name: session.name,
        session_status: session.status,
        harness: session.agent_type,
        harness_session_id: session.claude_session_id,
        runtime,
        workspace,
        pool,
        retention,
    })
}

fn inspection_phase(
    state: WorkerIncarnationState,
    status: &SessionStatus,
    runtime_matches: bool,
) -> InspectionPhase {
    let phase = match state {
        WorkerIncarnationState::Allocating if runtime_matches => InspectionPhase::Starting,
        WorkerIncarnationState::Allocating => InspectionPhase::Preparing,
        WorkerIncarnationState::Active => InspectionPhase::Running,
        WorkerIncarnationState::Retained => InspectionPhase::Retained,
        WorkerIncarnationState::CleanupClaimed | WorkerIncarnationState::ReleaseClaimed => {
            InspectionPhase::CleanupClaimed
        }
        WorkerIncarnationState::Released => InspectionPhase::Cleaned,
    };
    if matches!(status, SessionStatus::Spawning)
        && runtime_matches
        && matches!(phase, InspectionPhase::Running)
    {
        InspectionPhase::Starting
    } else {
        phase
    }
}

fn inspect_workspace(
    workspace: Option<&str>,
    session_id: &str,
    pooled: bool,
) -> WorkspaceInspection {
    let Some(workspace) = workspace else {
        return WorkspaceInspection {
            path: None,
            kind: WorkspaceKind::Unavailable,
            exists: false,
            git: None,
        };
    };
    let path = PathBuf::from(workspace);
    if !path.exists() {
        return WorkspaceInspection {
            path: Some(path),
            kind: WorkspaceKind::Missing,
            exists: false,
            git: None,
        };
    }
    let git = inspect_git(&path);
    let kind = if pooled {
        WorkspaceKind::PooledWorktree
    } else if ManagedWorktree::load_for_workspace(&path, session_id)
        .ok()
        .flatten()
        .is_some()
    {
        WorkspaceKind::ManagedWorktree
    } else if git.is_some() {
        let git_dir = git_output(&path, &["rev-parse", "--git-dir"]);
        let common_dir = git_output(&path, &["rev-parse", "--git-common-dir"]);
        if git_dir.is_some() && git_dir != common_dir {
            WorkspaceKind::GitWorktree
        } else {
            WorkspaceKind::GitWorkspace
        }
    } else {
        WorkspaceKind::NonGit
    };
    WorkspaceInspection {
        path: Some(path),
        kind,
        exists: true,
        git,
    }
}

fn inspect_git(path: &Path) -> Option<GitInspection> {
    (git_output(path, &["rev-parse", "--is-inside-work-tree"]).as_deref() == Some("true")).then(
        || GitInspection {
            branch: git_output(path, &["symbolic-ref", "--quiet", "--short", "HEAD"]),
            head: git_output(path, &["rev-parse", "HEAD"]),
            dirty: Command::new("git")
                .arg("-C")
                .arg(path)
                .args(["status", "--porcelain"])
                .output()
                .ok()
                .filter(|output| output.status.success())
                .map(|output| !output.stdout.is_empty()),
        },
    )
}

fn git_output(path: &Path, args: &[&str]) -> Option<String> {
    Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|output| output.trim().to_string())
        .filter(|output| !output.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Orchestrator;
    use tempfile::tempdir;

    fn session(id: &str, orchestrator_id: &str, workspace: &Path) -> Session {
        Session {
            id: id.into(),
            orchestrator_id: Some(orchestrator_id.into()),
            name: id.into(),
            repo: String::new(),
            status: SessionStatus::Working,
            agent_type: "cursor-agent".into(),
            cost_usd: 0.0,
            started_at: 1,
            pr_number: None,
            pr_id: None,
            workspace_path: Some(workspace.to_string_lossy().into_owned()),
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
        }
    }

    fn store(path: &Path) -> Arc<Store> {
        let store = Arc::new(Store::open(path).unwrap());
        store
            .upsert_orchestrator(&Orchestrator {
                id: "orch".into(),
                name: "orch".into(),
                created_at: 0,
            })
            .unwrap();
        store
    }

    #[test]
    fn worker_completion_authorization_rejects_wrong_role_and_replaced_runtime() {
        let root = tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let store = store(&root.path().join("ninox.db"));
        store
            .upsert_session(&session("worker", "orch", &workspace))
            .unwrap();
        let worker = store
            .prepare_worker_incarnation(
                "worker",
                Some("orch"),
                1,
                workspace.to_str().unwrap(),
                false,
                3,
            )
            .unwrap();
        assert!(store
            .bind_worker_incarnation(
                "worker",
                &worker.incarnation_id,
                workspace.to_str().unwrap(),
                workspace.to_str().unwrap(),
                None,
            )
            .unwrap());
        let runtime = tmux::TmuxPaneIdentity {
            physical_tmux_name: "worker".into(),
            pane_id: "%1".into(),
            pane_pid: std::process::id(),
            pane_created_at: 1,
            server_epoch: "test".into(),
        };

        assert!(authorize_worker_completion(
            &store,
            Some("worker"),
            Some(&worker.incarnation_id),
            Some("orch"),
            (Some("orchestrator"), Some("worker")),
            Some(&runtime),
            Some(&worker.incarnation_id),
        )
        .unwrap_err()
        .to_string()
        .contains("only available"));
        assert!(authorize_worker_completion(
            &store,
            Some("worker"),
            Some(&worker.incarnation_id),
            Some("orch"),
            (Some("worker"), Some("orchestrator")),
            Some(&runtime),
            Some("replacement"),
        )
        .unwrap_err()
        .to_string()
        .contains("replaced"));
        assert_eq!(
            authorize_worker_completion(
                &store,
                Some("worker"),
                Some(&worker.incarnation_id),
                Some("orch"),
                (None, Some("worker")),
                Some(&runtime),
                Some(&worker.incarnation_id),
            )
            .unwrap()
            .incarnation_id,
            worker.incarnation_id,
        );
    }

    #[tokio::test]
    async fn finalization_is_idempotent_and_preserves_workspace() {
        let root = tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let artifact = workspace.join("result.txt");
        std::fs::write(&artifact, "keep").unwrap();
        let store = store(&root.path().join("ninox.db"));
        store
            .upsert_session(&session("worker", "orch", &workspace))
            .unwrap();

        let first = finalize_owned_worker(store.clone(), "orch", "worker")
            .await
            .unwrap();
        let second = finalize_owned_worker(store.clone(), "orch", "worker")
            .await
            .unwrap();

        assert_eq!(first.outcome, FinalizeOutcome::Finalized);
        assert_eq!(second.outcome, FinalizeOutcome::AlreadyFinalized);
        assert_eq!(std::fs::read_to_string(artifact).unwrap(), "keep");
        assert_eq!(second.worker.phase, InspectionPhase::Retained);
        assert!(store.is_worker_retained("worker").unwrap());
    }

    #[tokio::test]
    async fn finalization_is_strictly_orchestrator_scoped() {
        let root = tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let store = store(&root.path().join("ninox.db"));
        store
            .upsert_session(&session("worker", "other", &workspace))
            .unwrap();

        let error = finalize_owned_worker(store, "orch", "worker")
            .await
            .unwrap_err();

        assert!(error.to_string().contains("not found"));
    }

    #[test]
    fn pending_finalization_blocks_spawn_and_cleanup_claims() {
        let root = tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let store = store(&root.path().join("ninox.db"));
        let mut spawning = session("worker", "orch", &workspace);
        spawning.status = SessionStatus::Spawning;
        store.upsert_session(&spawning).unwrap();
        let intent = store.begin_worker_finalization("orch", "worker").unwrap();
        let WorkerFinalizationIntent::Apply(worker) = intent else {
            panic!("first finalization must apply");
        };

        assert!(!store
            .bind_worker_incarnation(
                "worker",
                &worker.incarnation_id,
                workspace.to_str().unwrap(),
                workspace.to_str().unwrap(),
                None,
            )
            .unwrap());
        assert!(store
            .claim_worker_cleanup("worker", &worker.incarnation_id)
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn finalization_refuses_a_same_name_successor_runtime() {
        let root = tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let store = store(&root.path().join("ninox.db"));
        let id = format!("worker-{}", uuid::Uuid::new_v4());
        store
            .upsert_session(&session(&id, "orch", &workspace))
            .unwrap();
        let worker = store
            .prepare_worker_incarnation(&id, Some("orch"), 1, workspace.to_str().unwrap(), false, 3)
            .unwrap();
        assert!(store
            .bind_worker_incarnation(
                &id,
                &worker.incarnation_id,
                workspace.to_str().unwrap(),
                workspace.to_str().unwrap(),
                None,
            )
            .unwrap());
        tmux::create_session(
            &id,
            workspace.to_str().unwrap(),
            "sleep 30",
            &[("NINOX_WORKER_INCARNATION", "successor")],
        )
        .await
        .unwrap();

        let error = finalize_owned_worker(store, "orch", &id).await.unwrap_err();

        assert!(error.to_string().contains("stop worker"));
        assert!(tmux::exact_private_session(&id).await.unwrap().is_some());
        tmux::kill_private_session(&id).await.unwrap();
    }
}
