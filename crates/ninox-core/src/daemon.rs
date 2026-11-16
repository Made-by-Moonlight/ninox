//! Auto-start for the background services. The process running
//! `ninox --headless` (poller + ninox-server) is the daemon; the TCP bind
//! on the configured port is both the liveness probe and the lock — the
//! GUI holds it too, so "something is listening" always means "the
//! services are running" regardless of which frontend hosts them.

use std::path::Path;
use std::time::Duration;

pub enum DaemonStatus {
    AlreadyRunning,
    Started,
    /// The child was spawned but hadn't bound the port within the wait
    /// window — a cold start (wrapper install, orchestrator-root seeding,
    /// the blocking embedding-model load) can take longer. Not a failure:
    /// callers re-probe `port_in_use` later instead of declaring it down.
    Starting,
    /// Spawning itself failed; callers warn and continue (store reads
    /// still work, statuses may go stale).
    Failed(String),
}

pub async fn port_in_use(port: u16) -> bool {
    matches!(
        tokio::time::timeout(
            Duration::from_millis(500),
            tokio::net::TcpStream::connect(("127.0.0.1", port)),
        )
        .await,
        Ok(Ok(_))
    )
}

pub async fn ensure_daemon(port: u16, ninox_bin: &Path, db_path: &Path) -> DaemonStatus {
    // Before the engine, so its first ptyd operation never races the host's
    // startup. Not fatal: tmux sessions still work, and creating a ptyd
    // session spawns the host itself.
    if crate::runtime::configured_backend() == crate::runtime::Backend::Ptyd {
        if let Err(e) = crate::runtime::ensure_ptyd_host(ninox_bin).await {
            tracing::warn!("ptyd host not running: {e}");
        }
    }
    if port_in_use(port).await {
        return DaemonStatus::AlreadyRunning;
    }
    if let Err(e) = spawn_headless(port, ninox_bin, db_path) {
        return DaemonStatus::Failed(e);
    }
    // ninox --headless installs wrappers and seeds the orchestrator root
    // before binding, so give it a generous window.
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if port_in_use(port).await {
            return DaemonStatus::Started;
        }
    }
    DaemonStatus::Starting
}

/// Environment the daemon keeps from whoever launches it; everything else
/// is dropped — above all the launcher's session identity (`NINOX_SESSION`,
/// `NINOX_CALLER_TYPE`, `NINOX_PANE_ID`, `TMUX`, …): a daemon started from
/// inside a session must not run as that session. Credentials the user set
/// explicitly (`GITHUB_TOKEN`, an API key, `AWS_*`) are kept: the daemon's
/// own auto-start goes through here too, and they're its only credentials
/// for some users.
const DAEMON_ENV_KEEP: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "TMPDIR", "SSH_AUTH_SOCK", "RUST_LOG",
    "CLAUDE_CONFIG_DIR", "GITHUB_TOKEN", "GH_TOKEN", "ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN",
    // The overrides that point an instance at its config, data, brain,
    // tmux server and ptyd host — a scratch instance stays scratch.
    "NINOX_CONFIG", "NINOX_DATA_DIR", "NINOX_BRAIN", "NINOX_TMUX_SOCKET", "NINOX_PTYD_SOCKET",
    "NINOX_CLAUDE_PROJECTS_DIR", "TMUX_TMPDIR",
];
const DAEMON_ENV_KEEP_PREFIXES: &[&str] = &["LC_", "XDG_", "AWS_"];

fn daemon_keeps(key: &str) -> bool {
    DAEMON_ENV_KEEP.contains(&key) || DAEMON_ENV_KEEP_PREFIXES.iter().any(|p| key.starts_with(p))
}

/// The `ninox --headless` command for `port`/`db_path`, its environment
/// built from `vars` (the launcher's) by [`DAEMON_ENV_KEEP`].
pub fn headless_command(
    port:      u16,
    ninox_bin: &Path,
    db_path:   &Path,
    vars:      impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> std::process::Command {
    let mut cmd = std::process::Command::new(ninox_bin);
    cmd.arg("--headless")
        .arg("--port").arg(port.to_string())
        // The caller's resolved db path, not the default — otherwise a CLI
        // run with a custom `--db` spawns a daemon servicing the wrong
        // database, and the port probe then always reports AlreadyRunning.
        .arg("--db").arg(db_path)
        .env_clear()
        .envs(vars.into_iter().filter(|(k, _)| k.to_str().is_some_and(daemon_keeps)));
    cmd
}

/// Launch `ninox --headless` serving `port` against `db_path`, detached
/// into its own process group and logging to `daemon.log` beside the db.
/// Returns once spawned; it binds the port some seconds later.
pub fn spawn_headless(port: u16, ninox_bin: &Path, db_path: &Path) -> Result<(), String> {
    let log_path = crate::daemon_log::log_path(db_path);
    if let Some(log_dir) = log_path.parent() {
        if let Err(e) = std::fs::create_dir_all(log_dir) {
            return Err(format!("create log dir: {e}"));
        }
    }
    let log = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(f) => f,
        Err(e) => return Err(format!("open daemon.log: {e}")),
    };
    let mut cmd = headless_command(port, ninox_bin, db_path, std::env::vars_os());
    cmd
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone().map(std::process::Stdio::from).unwrap_or_else(|_| std::process::Stdio::null()))
        .stderr(std::process::Stdio::from(log));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0); // survive the parent's terminal/session
    }
    let mut child = cmd.spawn().map_err(|e| format!("spawn ninox --headless: {e}"))?;
    // Reap it whenever it exits: an unreaped daemon lingers as a zombie of
    // a long-lived GUI/TUI, and a zombie still answers `kill(pid, 0)`, so a
    // later restart would wait for it forever.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod env_tests {
    use super::headless_command;
    use std::ffi::OsString;
    use std::path::Path;

    #[test]
    fn the_daemon_never_inherits_the_launcher_s_session_identity() {
        let launcher: Vec<(OsString, OsString)> = [
            "NINOX_SESSION", "NINOX_SESSION_ID", "NINOX_CALLER_TYPE", "NINOX_CALLER_ID", "NINOX_PANE_ID",
            "NINOX_ACP_PANE_ID", "NINOX_ORCHESTRATOR_ID", "NINOX_WORKER_TYPE", "TMUX", "TMUX_PANE",
            "SOME_OTHER_TOOL_VAR", "GITHUB_TOKEN", "AWS_ACCESS_KEY_ID",
            "PATH", "HOME", "USER", "LANG", "LC_ALL", "AWS_PROFILE", "AWS_CONFIG_FILE",
            "NINOX_CONFIG", "NINOX_DATA_DIR", "NINOX_BRAIN", "NINOX_PTYD_SOCKET", "TMUX_TMPDIR",
        ]
        .iter()
        .map(|k| (OsString::from(k), OsString::from("v")))
        .collect();
        let cmd = headless_command(45678, Path::new("/bin/ninox"), Path::new("/scratch/t.db"), launcher);
        let mut env: Vec<String> = cmd.get_envs()
            .filter(|(_, v)| v.is_some())
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        env.sort();
        assert_eq!(env, [
            "AWS_ACCESS_KEY_ID", "AWS_CONFIG_FILE", "AWS_PROFILE", "GITHUB_TOKEN", "HOME", "LANG", "LC_ALL",
            "NINOX_BRAIN", "NINOX_CONFIG", "NINOX_DATA_DIR", "NINOX_PTYD_SOCKET", "PATH", "TMUX_TMPDIR", "USER",
        ]);
        let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args, ["--headless", "--port", "45678", "--db", "/scratch/t.db"]);
    }
}

#[cfg(test)]
mod tests {
    use super::port_in_use;
    use std::time::Duration;

    /// Poll instead of asserting on a single call: `port_in_use`'s connect
    /// has a fixed 500ms timeout, which a `cargo test --workspace` run's
    /// CPU/socket contention can occasionally exceed even though the port's
    /// actual state already matches `expected`.
    async fn wait_until_port_in_use(port: u16, expected: bool, what: &str) {
        for attempt in 0..20 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if port_in_use(port).await == expected {
                return;
            }
        }
        panic!("{what} never reached {expected}");
    }

    #[tokio::test]
    async fn detects_listener() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        wait_until_port_in_use(port, true, "port_in_use while the port is held").await;
    }

    #[tokio::test]
    async fn detects_free_port() {
        // Bind then drop to get a port that was just proven free.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        wait_until_port_in_use(port, false, "port_in_use after the port was dropped").await;
    }
}
