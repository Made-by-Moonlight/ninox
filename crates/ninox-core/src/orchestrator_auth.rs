//! Anti-spoofing authorization for orchestrator-facing CLI commands (e.g.
//! `ninox plan register`).
//!
//! `NINOX_ORCHESTRATOR_ID` alone is just an env var — trivially forgeable by
//! anything running in the same shell. `authorize_orchestrator` additionally
//! cross-checks the caller's live tmux pane identity against a persisted
//! `OrchestratorRuntimeIdentity` row, self-registering that row on first use
//! (the "migration" path) so no separate registration step is needed at
//! spawn time.

use crate::{runtime, store::Store, types::OrchestratorRuntimeIdentity};
use anyhow::{Context, Result};

pub fn authorize_orchestrator(
    store: &Store,
    orchestrator_id: Option<&str>,
    caller_type: Option<&str>,
    runtime: Option<&runtime::PaneIdentity>,
) -> Result<String> {
    let orchestrator_id = orchestrator_id
        .filter(|id| !id.is_empty())
        .context("NINOX_ORCHESTRATOR_ID is not set")?;
    anyhow::ensure!(
        caller_type == Some("orchestrator"),
        "this command is only available inside the owning orchestrator session"
    );
    let runtime = runtime.context("caller is not running inside a private Ninox pane")?;
    anyhow::ensure!(
        store.is_orchestrator(orchestrator_id)?,
        "caller session is not a persisted orchestrator"
    );
    let mut persisted = store.orchestrator_runtime_identity(orchestrator_id)?;
    if persisted.is_none()
        && runtime.physical_tmux_name == orchestrator_id
        && runtime::caller_descends_from(runtime.pane_pid)
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
            && runtime::caller_descends_from(persisted.root_pid),
        "caller is not running under the immutable orchestrator runtime"
    );
    Ok(orchestrator_id.to_string())
}
