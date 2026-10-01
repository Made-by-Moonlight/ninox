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
    let log_dir = dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("ninox");
    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        return DaemonStatus::Failed(format!("create log dir: {e}"));
    }
    let log = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("daemon.log"))
    {
        Ok(f) => f,
        Err(e) => return DaemonStatus::Failed(format!("open daemon.log: {e}")),
    };
    let mut cmd = std::process::Command::new(ninox_bin);
    cmd.arg("--headless")
        .arg("--port").arg(port.to_string())
        // The caller's resolved db path, not the default — otherwise a CLI
        // run with a custom `--db` spawns a daemon servicing the wrong
        // database, and the port probe then always reports AlreadyRunning.
        .arg("--db").arg(db_path)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone().map(std::process::Stdio::from).unwrap_or_else(|_| std::process::Stdio::null()))
        .stderr(std::process::Stdio::from(log));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0); // survive the parent's terminal/session
    }
    if let Err(e) = cmd.spawn() {
        return DaemonStatus::Failed(format!("spawn ninox --headless: {e}"));
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

#[cfg(test)]
mod tests {
    use super::port_in_use;

    #[tokio::test]
    async fn detects_listener() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(port_in_use(port).await);
    }

    #[tokio::test]
    async fn detects_free_port() {
        // Bind then drop to get a port that was just proven free.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        assert!(!port_in_use(port).await);
    }
}
