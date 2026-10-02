//! `ninox-ptyd`: the only process that holds agent PTY master fds.
//!
//! The engine (`ninox --headless`), the TUI and `ninox pane attach` bridges
//! are all clients of this host over [`protocol`]. Keeping this crate free of
//! engine logic is what lets the engine be upgraded or crash without touching
//! agent processes.

pub mod attach;
pub mod checkpoint;
pub mod client;
pub mod codec;
pub mod engine;
mod filter;
mod handoff;
mod host;
mod pane;
pub mod protocol;
mod render;

pub use client::{PtydClient, Subscription};
pub use protocol::*;

use std::path::PathBuf;

/// Overrides the socket path (tests, isolated fleets).
pub const SOCKET_ENV: &str = "NINOX_PTYD_SOCKET";

/// `$NINOX_PTYD_SOCKET`, else `<data_local_dir>/ninox/ptyd.sock`.
pub fn socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os(SOCKET_ENV) {
        return PathBuf::from(p);
    }
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("ninox")
        .join("ptyd.sock")
}

/// Run the host in the foreground until `Shutdown` or a fatal error.
/// `ninox ptyd` calls this; it binds `socket` (removing a stale socket file
/// only when nothing is listening) and exits non-zero if another host is live.
pub async fn run_host(socket: PathBuf, checkpoint_dir: Option<PathBuf>) -> anyhow::Result<()> {
    host::run(socket, checkpoint_dir, host::Signals::Default).await
}

/// Live upgrade: take over the panes of the host running on `socket` (their
/// PTY master fds arrive over `SCM_RIGHTS`), then serve in its place; the old
/// host exits without killing anything. When no host answers on `socket`
/// this starts a fresh host. Either way the host ignores SIGINT and
/// job-control signals, since it usually runs in a terminal's foreground;
/// only SIGTERM or `Shutdown` stop it. See the `handoff` module docs for the
/// sequence.
pub async fn run_host_takeover(socket: PathBuf, checkpoint_dir: Option<PathBuf>) -> anyhow::Result<()> {
    handoff::run_takeover(socket, checkpoint_dir).await
}

pub use engine::{AlacrittyEngine, TerminalEngine};
