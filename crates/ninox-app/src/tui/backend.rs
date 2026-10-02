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
    ninox_ptyd::socket_path()
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

#[cfg(test)]
mod host_ownership_tests {
    #[test]
    fn only_a_host_born_after_our_spawn_is_ours() {
        assert!(super::host_is_ours(1_000, 1_000));
        assert!(super::host_is_ours(1_250, 1_000));
        assert!(!super::host_is_ours(999, 1_000), "a host that was already up is someone else's");
    }
}

pub(crate) async fn legacy_connect(store: &Store, id: &str) -> anyhow::Result<crate::connect::ConnectPlan> {
    crate::connect::connect_preflight(store, id).await
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

/// `ninox fleet restore --yes` against this TUI's database. A child process
/// rather than a task in the TUI: a restore waits minutes on agent prompts
/// and must outlive quitting the TUI, and its output must not land on the
/// TUI's screen.
fn restore_command(exe: &Path, db_path: &Path) -> std::process::Command {
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["fleet", "restore", "--yes", "--db"]).arg(db_path);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0); // survive the TUI's terminal/session
    }
    cmd
}

/// Where a detached restore writes: the daemon log next to the database.
fn restore_log(db_path: &Path) -> PathBuf {
    db_path.parent().unwrap_or_else(|| Path::new(".")).join("daemon.log")
}

/// Start a detached restore; returns the notice to show.
pub fn restore_fleet(db_path: &Path) -> Result<String, String> {
    let exe = ninox_core::hooks::canonical_exe().map_err(|e| format!("could not resolve ninox binary: {e}"))?;
    let log_path = restore_log(db_path);
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("open {}: {e}", log_path.display()))?;
    let stderr = log.try_clone().map_err(|e| format!("open {}: {e}", log_path.display()))?;
    restore_command(&exe, db_path)
        .stdin(std::process::Stdio::null())
        .stdout(log)
        .stderr(stderr)
        .spawn()
        .map_err(|e| format!("spawn ninox fleet restore: {e}"))?;
    Ok(format!(
        "restoring fleet in the background — workers first, then orchestrators (log: {})",
        log_path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_runs_as_a_detached_cli_against_the_tuis_database() {
        let db = Path::new("/tmp/sandbox/ninox.db");
        let cmd = restore_command(Path::new("/bin/ninox"), db);
        let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args, ["fleet", "restore", "--yes", "--db", "/tmp/sandbox/ninox.db"]);
        assert_eq!(restore_log(db), Path::new("/tmp/sandbox/daemon.log"));
    }
}
