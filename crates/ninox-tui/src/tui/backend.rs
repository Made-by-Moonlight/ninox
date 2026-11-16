//! The TUI's only touch points with runtime/fleet APIs that other
//! terminal-native-runtime workstreams own. Each fn is the single call site
//! to rewire when those land:
//!
//! The TUI's touch points with the runtime seam and durable fleets.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ninox_core::{config::AppConfig, events::Engine, store::Store};

pub fn ptyd_socket() -> PathBuf {
    ninox_core::runtime::ptyd_socket_path()
}

/// Whether the TUI should start a host at launch: only when ptyd is the
/// configured backend. With `[runtime] backend = "tmux"` one already running
/// is used (it may still hold panes from before the switch), and one is
/// started later only if a tmux session needs a viewer pane.
pub fn ptyd_starts_at_launch() -> bool {
    ninox_core::runtime::configured_backend() == ninox_core::runtime::Backend::Ptyd
}

/// Make sure a ptyd host is listening; with `start`, spawn `ninox ptyd`
/// detached if not. `true` when no host answered and one was spawned.
pub async fn ensure_ptyd(start: bool) -> anyhow::Result<bool> {
    let socket = ptyd_socket();
    if !ninox_core::runtime::ptyd_allowed() {
        anyhow::bail!("ptyd is disabled in test binaries");
    }
    match ninox_ptyd::PtydClient::connect(&socket, "ninox-tui").await {
        Ok(_) => return Ok(false),
        Err(e) if !start => return Err(e),
        Err(_) => {}
    }
    let exe = ninox_core::hooks::canonical_exe()?;
    let argv = vec![exe.to_string_lossy().into_owned(), "ptyd".to_string()];
    let log = socket.parent().map(|d| d.join("ptyd.log"));
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let spawned_at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
    let client = ninox_ptyd::PtydClient::connect_or_spawn(&socket, "ninox-tui", &argv, log, Duration::from_secs(5)).await?;
    // Another process may have started a host between our probe and the
    // spawn (ours then exits on the lock): only a host born after we asked
    // is ours to stop later.
    Ok(host_is_ours(client.host_identity().1, spawned_at))
}

/// A host whose epoch predates our spawn request was started by someone
/// else (or adopted panes in a live upgrade, which keeps the old epoch).
fn host_is_ours(host_epoch_ms: u64, spawned_at_ms: u64) -> bool {
    host_epoch_ms >= spawned_at_ms
}

pub(crate) async fn legacy_connect(store: &Store, id: &str) -> anyhow::Result<ninox_ui_shared::connect::ConnectPlan> {
    ninox_ui_shared::connect::connect_preflight(store, id).await
}

pub async fn kill_session(engine: &Arc<Engine>, id: &str) -> anyhow::Result<()> {
    engine.terminate_session(id).await
}

pub use ninox_core::config::RestorePolicy;
pub use ninox_core::fleet::startup::RestoreSummary;

pub fn restore_policy(config: &AppConfig) -> RestorePolicy {
    config.fleet.restore_policy
}

pub fn pending_restore(store: &Store) -> Option<RestoreSummary> {
    ninox_core::fleet::startup::pending_restore(store)
}

pub fn dismiss_pending_restore(store: &Store) {
    ninox_core::fleet::startup::dismiss_pending_restore(store)
}

/// Start a detached restore of the whole fleet; returns the notice to show.
pub fn restore_fleet(db_path: &Path) -> Result<String, String> {
    ninox_ui_shared::fleet::spawn_detached_restore(db_path, &AppConfig::config_path(), &[])
}

#[cfg(test)]
mod host_ownership_tests {
    #[test]
    fn only_a_host_born_after_our_spawn_is_ours() {
        assert!(super::host_is_ours(1_000, 1_000));
        assert!(super::host_is_ours(1_250, 1_000));
        assert!(!super::host_is_ours(999, 1_000), "a host that was already up is someone else's");
    }
}
