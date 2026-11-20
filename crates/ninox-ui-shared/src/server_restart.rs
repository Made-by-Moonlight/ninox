//! Restart the background daemon (`ninox --headless`) on the configured
//! port, shared by `ninox server restart`, the GUI and the TUI.
//!
//! The daemon reads credentials and its own binary once, at startup, so a
//! login (`aws sso login`, `gh auth login`, a Claude login) or a new build
//! only takes effect after a restart. Sessions survive it: their panes are
//! hosted by the ptyd host (its own `setsid` session) or the tmux server
//! (self-daemonizing), never by the daemon, which only runs the poller and
//! the HTTP server.
//!
//! Stopping goes through the daemon's control socket first, which only
//! accepts the user (see `ninox_core::server_control`). SIGTERM is the
//! fallback for a daemon too old to have that socket. There is no SIGKILL:
//! a daemon that won't stop holds the store, and killing it mid-write is
//! worse than a clear error.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ninox_core::server_control::{self, ServerInfo, ShutdownReply};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub port:      u16,
    pub db_path:   PathBuf,
    /// What the new daemon runs: the self-shim's target, so a freshly
    /// built or installed binary takes effect.
    pub ninox_bin: PathBuf,
}

impl Target {
    pub fn resolve(port: u16, db_path: PathBuf) -> Self {
        let ninox_bin = ninox_core::hooks::read_shim(&ninox_core::hooks::self_shim_path())
            .map(|s| s.target)
            .filter(|p| p.is_file())
            .or_else(|| ninox_core::hooks::canonical_exe().ok())
            .unwrap_or_else(|| PathBuf::from("ninox"));
        Self { port, db_path, ninox_bin }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Stopping,
    Starting,
    WaitingHealthy,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Phase::Stopping => "Stopping server…",
            Phase::Starting => "Starting server…",
            Phase::WaitingHealthy => "Waiting for server…",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// `None` when nothing was running, or it predates `/api/server/info`.
    pub old: Option<ServerInfo>,
    pub new: ServerInfo,
}

impl Outcome {
    pub fn summary(&self) -> String {
        match &self.old {
            Some(old) => format!("server restarted: {} → {}", old.label(), self.new.label()),
            None => format!("server started: {}", self.new.label()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartError {
    /// The caller is an agent session, or the daemon refused it.
    Refused(String),
    /// This very process serves the port (the GUI hosting in-process).
    HostedInProcess,
    /// The daemon on the port serves a different database.
    DbMismatch { serving: String, configured: String },
    /// Something that isn't an identifiable ninox holds the port.
    PortHeldByOther { port: u16 },
    StopTimeout { pid: Option<u32>, waited: Duration },
    StartFailed(String),
    NotHealthy { waited: Duration },
}

impl std::fmt::Display for RestartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(e) => write!(f, "{e}"),
            Self::HostedInProcess => write!(
                f,
                "this app is hosting the server itself — quit and reopen it to restart the server"
            ),
            Self::DbMismatch { serving, configured } => write!(
                f,
                "the server on this port serves {serving}, not the configured {configured} — not touching it"
            ),
            Self::PortHeldByOther { port } => write!(
                f,
                "port {port} is held by a process that isn't a recognisable ninox server — not touching it"
            ),
            Self::StopTimeout { pid, waited } => write!(
                f,
                "the server{} did not stop within {}s; left running rather than force-killed (it holds the store)",
                pid.map(|p| format!(" (pid {p})")).unwrap_or_default(),
                waited.as_secs()
            ),
            Self::StartFailed(e) => write!(f, "could not start the server: {e}"),
            Self::NotHealthy { waited } => write!(
                f,
                "the new server did not report healthy within {}s — see daemon.log",
                waited.as_secs()
            ),
        }
    }
}

impl std::error::Error for RestartError {}

#[derive(Debug, Clone, Copy)]
pub struct Timings {
    pub stop:  Duration,
    /// A cold start loads the embedding model and seeds the orchestrator
    /// root before binding.
    pub start: Duration,
    pub poll:  Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Self { stop: Duration::from_secs(15), start: Duration::from_secs(60), poll: Duration::from_millis(200) }
    }
}

/// Everything the routine does to the outside world, so the state machine
/// is testable without a daemon.
#[allow(async_fn_in_trait)]
pub trait Ops {
    /// The live managed session the caller runs inside, if any; `Err`
    /// when that can't be determined, which refuses.
    async fn caller_session(&self) -> Result<Option<String>, String>;
    async fn info(&self, port: u16) -> Option<ServerInfo>;
    async fn port_in_use(&self, port: u16) -> bool;
    async fn request_shutdown(&self, target: &Target) -> ShutdownReply;
    /// Last resort for a daemon without `/api/server/info`.
    async fn listener_pid(&self, port: u16) -> Option<u32>;
    /// `pid`'s command line, to confirm a `listener_pid` is our daemon.
    async fn command_line(&self, pid: u32) -> Option<String>;
    fn terminate(&self, pid: u32) -> std::io::Result<()>;
    fn pid_alive(&self, pid: u32) -> bool;
    fn spawn(&self, target: &Target) -> Result<(), String>;
    async fn sleep(&self, d: Duration);
    fn own_pid(&self) -> u32;
    /// The new server is up; the user restarted it after logging in.
    fn after_restart(&self) {}
}

pub async fn restart(
    target:   &Target,
    ops:      &impl Ops,
    timings:  Timings,
    mut on_phase: impl FnMut(Phase),
) -> Result<Outcome, RestartError> {
    match ops.caller_session().await {
        Ok(None) => {}
        Ok(Some(session)) => {
            return Err(RestartError::Refused(format!(
                "refused: only the user can restart the server, and this runs inside managed session `{session}`"
            )))
        }
        Err(e) => return Err(RestartError::Refused(format!("refused: could not verify the caller ({e})"))),
    }
    on_phase(Phase::Stopping);
    let old = ops.info(target.port).await;
    if let Some(old) = &old {
        if old.in_process || old.pid == ops.own_pid() {
            return Err(RestartError::HostedInProcess);
        }
        if !same_path(Path::new(&old.db_path), &target.db_path) {
            return Err(RestartError::DbMismatch {
                serving:    old.db_path.clone(),
                configured: target.db_path.display().to_string(),
            });
        }
    }
    if ops.port_in_use(target.port).await {
        stop(target, ops, timings, old.as_ref().map(|o| o.pid)).await?;
    }

    on_phase(Phase::Starting);
    ops.spawn(target).map_err(RestartError::StartFailed)?;

    on_phase(Phase::WaitingHealthy);
    let mut waited = Duration::ZERO;
    loop {
        if let Some(new) = ops.info(target.port).await {
            if Some(new.pid) != old.as_ref().map(|o| o.pid) {
                ops.after_restart();
                return Ok(Outcome { old, new });
            }
        }
        if waited >= timings.start {
            return Err(RestartError::NotHealthy { waited });
        }
        ops.sleep(timings.poll).await;
        waited += timings.poll;
    }
}

async fn stop(target: &Target, ops: &impl Ops, timings: Timings, known_pid: Option<u32>) -> Result<(), RestartError> {
    tracing::warn!(
        "stopping the daemon on :{} (pid {known_pid:?}) for a restart; requested by pid {}",
        target.port,
        ops.own_pid()
    );
    match ops.request_shutdown(target).await {
        ShutdownReply::Refused(e) => return Err(RestartError::Refused(e)),
        ShutdownReply::Accepted => {
            if wait_stopped(target, ops, timings, known_pid).await {
                return Ok(());
            }
            // Accepted but still up (a stuck shutdown): one SIGTERM, which
            // the daemon handles the same graceful way.
            let Some(pid) = known_pid else {
                return Err(RestartError::StopTimeout { pid: None, waited: timings.stop });
            };
            tracing::warn!("daemon pid {pid} accepted the stop but is still up: sending SIGTERM from pid {}", ops.own_pid());
            let _ = ops.terminate(pid);
        }
        ShutdownReply::Unreachable(_) => {
            let pid = match known_pid {
                Some(pid) => pid,
                None => {
                    // No `/api/server/info`: a daemon from before it existed,
                    // or not ours at all. Signal it only if its command line
                    // says it's a headless ninox on this port and db.
                    let pid = ops.listener_pid(target.port).await
                        .ok_or(RestartError::PortHeldByOther { port: target.port })?;
                    let args = ops.command_line(pid).await.unwrap_or_default();
                    if !is_our_daemon(&args, target) {
                        return Err(RestartError::PortHeldByOther { port: target.port });
                    }
                    pid
                }
            };
            if pid == ops.own_pid() {
                return Err(RestartError::HostedInProcess);
            }
            tracing::warn!("no control socket answered: sending SIGTERM to daemon pid {pid} from pid {}", ops.own_pid());
            ops.terminate(pid).map_err(|e| RestartError::Refused(format!("could not signal pid {pid}: {e}")))?;
            if wait_stopped(target, ops, timings, Some(pid)).await {
                return Ok(());
            }
            return Err(RestartError::StopTimeout { pid: Some(pid), waited: timings.stop });
        }
    }
    if wait_stopped(target, ops, timings, known_pid).await {
        Ok(())
    } else {
        Err(RestartError::StopTimeout { pid: known_pid, waited: timings.stop * 2 })
    }
}

async fn wait_stopped(target: &Target, ops: &impl Ops, timings: Timings, pid: Option<u32>) -> bool {
    let mut waited = Duration::ZERO;
    loop {
        let alive = pid.is_some_and(|p| ops.pid_alive(p));
        if !alive && !ops.port_in_use(target.port).await {
            return true;
        }
        if waited >= timings.stop {
            return false;
        }
        ops.sleep(timings.poll).await;
        waited += timings.poll;
    }
}

/// Whether `args` (a `ps -o args=` line) is `ninox --headless` serving
/// `target`'s port and db. A daemon started without `--db` serves the
/// default db.
pub fn is_our_daemon(args: &str, target: &Target) -> bool {
    let words: Vec<&str> = args.split_whitespace().collect();
    let Some(exe) = words.first() else { return false };
    let exe_name = Path::new(exe).file_name().and_then(|n| n.to_str()).unwrap_or("");
    let value = |flag: &str| words.windows(2).find(|w| w[0] == flag).map(|w| w[1]);
    exe_name.starts_with("ninox")
        && words.contains(&"--headless")
        && value("--port").and_then(|p| p.parse::<u16>().ok()) == Some(target.port)
        && match value("--db") {
            Some(db) => same_path(Path::new(db), &target.db_path),
            None => same_path(&ninox_core::AppConfig::default_db_path(), &target.db_path),
        }
}

fn same_path(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).or_else(|_| std::path::absolute(p)).unwrap_or_else(|_| p.to_path_buf());
    canon(a) == canon(b)
}

/// The real side effects.
pub struct LiveOps {
    pub store: Option<std::sync::Arc<ninox_core::store::Store>>,
}

impl Ops for LiveOps {
    async fn caller_session(&self) -> Result<Option<String>, String> {
        let Some(store) = self.store.as_ref() else { return Ok(None) };
        let pane = ninox_core::runtime::current_private_pane_identity().await.ok().flatten();
        if let Some(pane) = pane {
            return Ok(Some(pane.physical_tmux_name));
        }
        ninox_core::orchestrator_auth::resolve_live_session_id_for_pid(store, std::process::id()).map_err(|e| e.to_string())
    }

    async fn info(&self, port: u16) -> Option<ServerInfo> {
        fetch_info(port).await
    }

    async fn port_in_use(&self, port: u16) -> bool {
        ninox_core::daemon::port_in_use(port).await
    }

    async fn request_shutdown(&self, target: &Target) -> ShutdownReply {
        server_control::request_shutdown(&server_control::control_socket_path(target.port, &target.db_path)).await
    }

    async fn listener_pid(&self, port: u16) -> Option<u32> {
        let lsof = tokio::process::Command::new("lsof")
            .args(["-nP", "-t", &format!("-iTCP:{port}"), "-sTCP:LISTEN"])
            .kill_on_drop(true)
            .output();
        let out = tokio::time::timeout(Duration::from_secs(2), lsof).await.ok()?.ok()?;
        String::from_utf8_lossy(&out.stdout).lines().next()?.trim().parse().ok()
    }

    async fn command_line(&self, pid: u32) -> Option<String> {
        let ps = tokio::process::Command::new("/bin/ps")
            .args(["-o", "args=", "-p", &pid.to_string()])
            .kill_on_drop(true)
            .output();
        let out = tokio::time::timeout(Duration::from_secs(2), ps).await.ok()?.ok()?;
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|s| !s.is_empty())
    }

    fn terminate(&self, pid: u32) -> std::io::Result<()> {
        let pid = libc::pid_t::try_from(pid).map_err(std::io::Error::other)?;
        // SAFETY: kill(2) with a positive pid only signals that process.
        if unsafe { libc::kill(pid, libc::SIGTERM) } == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
    }

    fn pid_alive(&self, pid: u32) -> bool {
        // Reaps it if it's an exited child of ours (a zombie still answers
        // `kill(pid, 0)`); a no-op ECHILD for anyone else's process.
        if let Ok(raw) = libc::pid_t::try_from(pid) {
            let mut status = 0;
            // SAFETY: WNOHANG never blocks; `status` is a valid out-pointer.
            unsafe { libc::waitpid(raw, &mut status, libc::WNOHANG) };
        }
        ninox_core::lifecycle::probe::is_pid_alive(pid)
    }

    fn spawn(&self, target: &Target) -> Result<(), String> {
        if target.port == ninox_core::AppConfig::load_or_default().port && start_managed_service() {
            return Ok(());
        }
        ninox_core::daemon::spawn_headless(target.port, &target.ninox_bin, &target.db_path)
    }

    async fn sleep(&self, d: Duration) {
        tokio::time::sleep(d).await;
    }

    fn own_pid(&self) -> u32 {
        std::process::id()
    }

    /// Subprocess/spawn login failures predate the login; they'll be
    /// reported again if they recur. Session rows stay until relaunched.
    fn after_restart(&self) {
        if let Some(store) = &self.store {
            let _ = store.clear_harness_auth_subprocesses();
        }
    }
}

/// Starts the daemon through launchd/systemd when this user installed the
/// service (`ninox service install`) and it is loaded, so the supervisor
/// stays the only thing that spawns it. Looked up under the real `$HOME`,
/// so a scratch HOME never reaches the user's service.
fn start_managed_service() -> bool {
    use ninox_core::fleet::service::{managed_command, start_commands, unit_path, ServicePlatform};
    let Some(platform) = ServicePlatform::current() else { return false };
    let Some(home) = dirs::home_dir() else { return false };
    if !unit_path(platform, &home).is_file() {
        return false;
    }
    let uid = {
        use std::os::unix::fs::MetadataExt;
        match std::fs::metadata(&home) {
            Ok(m) => m.uid(),
            Err(_) => return false,
        }
    };
    let run = |argv: &[String]| {
        let Some((bin, args)) = argv.split_first() else { return false };
        std::process::Command::new(bin)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    run(&managed_command(platform, uid)) && start_commands(platform, uid).iter().all(|c| run(c))
}

/// Something the user should fix and then restart the server for. Never
/// runs the fix itself — login flows stay the user's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub title:    String,
    pub hint:     String,
    /// Sessions that hit a harness login failure: relaunching them (after
    /// logging in) is what fixes them, not the server restart.
    pub sessions: Vec<String>,
}

impl Alert {
    fn new(title: impl Into<String>, hint: impl Into<String>) -> Self {
        Self { title: title.into(), hint: hint.into(), sessions: Vec::new() }
    }

    pub fn line(&self) -> String {
        format!("{}: {}", self.title, self.hint)
    }
}

/// A harness login failure (the primary case), or a secondary credential
/// failure of the server's own (AWS, GitHub).
pub fn classify(issue: &server_control::ServiceIssue) -> Option<Alert> {
    if let Some(harness) = issue.source.strip_prefix("harness_auth:") {
        let auth = ninox_core::harness_auth::for_harness(harness)?;
        return Some(Alert {
            title:    format!("{} login expired", auth.display),
            hint:     format!("log in again (`{}`), then Restart server", auth.login),
            sessions: issue.sessions.clone(),
        });
    }
    if let Some(fix) = issue.message.strip_prefix("update check can't run: ").filter(|_| issue.source == "update_check") {
        return Some(Alert::new("Update check can't run", fix));
    }
    classify_issue(&issue.message)
}

/// Auth failures worth a restart; anything else (a network blip, a rate
/// limit) isn't surfaced, since restarting wouldn't help.
pub fn classify_issue(message: &str) -> Option<Alert> {
    let lower = message.to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| lower.contains(n));
    if has(&["credential-provider", "aws sso", "sso session", "token has expired", "expiredtoken"]) {
        Some(Alert::new("AWS login expired", "run `aws sso login`, then Restart server"))
    } else if has(&["bad credentials", "requires authentication", "401", "gh auth login"]) {
        Some(Alert::new("GitHub login failed", "run `gh auth login`, then Restart server"))
    } else if has(&["not logged in", "please run /login", "invalid api key"]) {
        Some(Alert::new("Claude login expired", "log in again (`claude /login`), then Restart server"))
    } else {
        None
    }
}

/// Everything a banner should show for the server at `info`, as seen by a
/// client built at `client_version`/`client_build`.
pub fn alerts(info: &ServerInfo, client_version: &str, client_build: &str) -> Vec<Alert> {
    let mut out: Vec<Alert> = Vec::new();
    let mut issues: Vec<&server_control::ServiceIssue> = info.issues.iter().collect();
    // Harness logins first: they're what the banner leads with.
    issues.sort_by_key(|i| !i.source.starts_with("harness_auth:"));
    for issue in issues {
        if let Some(alert) = classify(issue) {
            if !out.contains(&alert) {
                out.push(alert);
            }
        }
    }
    if build_differs(info, client_version, client_build) {
        out.push(Alert::new(
            format!("Server is running {}, this app is {client_version} ({client_build})", info.label()),
            "Restart server to run the current build",
        ));
    }
    out
}

/// A build hash of `unknown` (built outside git) never counts as a mismatch.
fn build_differs(info: &ServerInfo, client_version: &str, client_build: &str) -> bool {
    let known = |b: &str| !b.is_empty() && b != "unknown";
    info.version != client_version || (known(&info.build) && known(client_build) && info.build != client_build)
}

/// The CLI's stderr line and the log line for a daemon running a different
/// build than `client_version`/`client_build`. `None` when they match.
pub fn stale_daemon_warning(info: &ServerInfo, client_version: &str, client_build: &str) -> Option<String> {
    let age = if version_key(&info.version) > version_key(client_version) { "a newer" } else { "an older" };
    build_differs(info, client_version, client_build).then(|| format!(
        "daemon is running {age} build: restart it (`ninox server restart`, or Settings › Restart server) — \
         daemon {}, this binary {client_version} ({client_build})",
        info.label(),
    ))
}

/// Dotted numeric parts of a version, for ordering; non-numeric parts are 0.
fn version_key(v: &str) -> Vec<u64> {
    v.split(['.', '-', '+']).map(|p| p.parse().unwrap_or(0)).collect()
}

/// Asks the daemon on `port` for its build and logs [`stale_daemon_warning`]
/// for this binary; returns it so a CLI can also print it. Never restarts.
pub async fn check_daemon_build(port: u16) -> Option<String> {
    let info = fetch_info(port).await?;
    let warning = stale_daemon_warning(&info, env!("CARGO_PKG_VERSION"), server_control::BUILD_HASH)?;
    tracing::warn!("{warning}");
    Some(warning)
}

/// [`alerts`] for this binary.
pub fn alerts_for_this_build(info: &ServerInfo) -> Vec<Alert> {
    alerts(info, env!("CARGO_PKG_VERSION"), server_control::BUILD_HASH)
}

/// Record a spawn error that reports a harness login failure, so the
/// banner shows it in every UI.
pub fn note_spawn_failure(store: &ninox_core::store::Store, error: &str) {
    if let Some((auth, line)) = ninox_core::harness_auth::detect_any(error) {
        let _ = store.record_harness_auth(&format!("spawn:{}", auth.harness), auth.harness, None, &line);
    }
}

/// "Restart affected sessions": relaunch in place (conversation resumed
/// where the harness supports it) each session that hit a login failure.
pub async fn restart_sessions(
    store:   std::sync::Arc<ninox_core::store::Store>,
    db_path: PathBuf,
    ids:     Vec<String>,
) -> Result<String, String> {
    let config = ninox_core::AppConfig::load_or_default();
    let args = crate::restart::RestartArgs { session_ids: ids.clone(), all: false, exec_detached: false };
    let outcomes = crate::restart::execute(args, std::sync::Arc::clone(&store), config, db_path)
        .await
        .map_err(|e| format!("restart sessions failed: {e}"))?;
    let summary = crate::restart::summarize(&outcomes);
    if crate::restart::had_trouble(&outcomes) {
        return Err(summary);
    }
    for id in &ids {
        let _ = store.clear_harness_auth(id);
    }
    Ok(summary)
}

#[derive(clap::Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum ServerAction {
    /// Restart the server in place, keeping running sessions — e.g. after
    /// `aws sso login`, `gh auth login`, a Claude login, or a new build
    Restart,
}

/// Non-terminal sessions whose pane is still alive in its runtime.
pub async fn live_session_count(store: &ninox_core::store::Store) -> usize {
    let Ok(sessions) = store.list_sessions() else { return 0 };
    let mut n = 0;
    for s in sessions.iter().filter(|s| !s.status.is_terminal()) {
        if ninox_core::runtime::has_session(&s.id).await {
            n += 1;
        }
    }
    n
}

pub async fn run_cli(port_arg: Option<u16>, db_path: PathBuf) -> anyhow::Result<()> {
    let port = port_arg.unwrap_or_else(|| ninox_core::AppConfig::load_or_default().port);
    let store = std::sync::Arc::new(ninox_core::store::Store::open(&db_path)?);
    let target = Target::resolve(port, db_path);
    let before = live_session_count(&store).await;
    let ops = LiveOps { store: Some(std::sync::Arc::clone(&store)) };
    let outcome = restart(&target, &ops, Timings::default(), |p| eprintln!("{}", p.label())).await?;
    let after = live_session_count(&store).await;
    println!("{}", outcome.summary());
    println!("live sessions: {before} before → {after} after");
    if after < before {
        anyhow::bail!("{} session(s) were not found after the restart — check `ninox list`", before - after);
    }
    Ok(())
}

/// `GET /api/server/info` on loopback, with a short timeout. A plain
/// HTTP/1.0 request over a TCP stream — not worth an HTTP client.
pub async fn fetch_info(port: u16) -> Option<ServerInfo> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let exchange = async {
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.ok()?;
        s.write_all(b"GET /api/server/info HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n").await.ok()?;
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).await.ok()?;
        let text = String::from_utf8_lossy(&buf);
        let (head, body) = text.split_once("\r\n\r\n")?;
        if !head.starts_with("HTTP/1.1 200") && !head.starts_with("HTTP/1.0 200") {
            return None;
        }
        serde_json::from_str(body).ok()
    };
    tokio::time::timeout(Duration::from_secs(2), exchange).await.ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    /// A scripted daemon: `running` is the pid holding the port, if any.
    struct Fake {
        caller:        Option<String>,
        running:       Cell<Option<u32>>,
        has_info:      bool,
        db:            String,
        reply:         ShutdownReply,
        /// Whether a shutdown request / SIGTERM actually stops it.
        stops:         bool,
        /// Port held by something that isn't ninox.
        foreign:       bool,
        lsof:          Option<u32>,
        args:          String,
        comes_up:      bool,
        in_process:    bool,
        next_pid:      u32,
        log:           RefCell<Vec<String>>,
    }

    impl Fake {
        fn running(pid: u32) -> Self {
            Self {
                caller: None, running: Cell::new(Some(pid)), has_info: true, db: "/scratch/t.db".into(),
                reply: ShutdownReply::Accepted, stops: true, foreign: false, lsof: None,
                args: "/bin/ninox --headless --port 45678 --db /scratch/t.db".into(),
                comes_up: true, in_process: false, next_pid: 200, log: RefCell::default(),
            }
        }
        fn log(&self) -> Vec<String> {
            self.log.borrow().clone()
        }
    }

    impl Ops for Fake {
        async fn caller_session(&self) -> Result<Option<String>, String> {
            match self.caller.as_deref() {
                Some("<unverifiable>") => Err("ps failed".into()),
                other => Ok(other.map(str::to_string)),
            }
        }
        async fn info(&self, port: u16) -> Option<ServerInfo> {
            let pid = self.running.get()?;
            (self.has_info || pid == self.next_pid).then(|| ServerInfo {
                version: if pid == self.next_pid { "0.38.0" } else { "0.37.0" }.into(),
                build: "abc".into(), pid, port, db_path: self.db.clone(), in_process: self.in_process, issues: Vec::new(),
            })
        }
        async fn port_in_use(&self, _: u16) -> bool {
            self.foreign || self.running.get().is_some()
        }
        async fn request_shutdown(&self, _: &Target) -> ShutdownReply {
            self.log.borrow_mut().push("shutdown".into());
            if self.reply == ShutdownReply::Accepted && self.stops {
                self.running.set(None);
            }
            self.reply.clone()
        }
        async fn listener_pid(&self, _: u16) -> Option<u32> {
            self.log.borrow_mut().push("lsof".into());
            self.lsof
        }
        async fn command_line(&self, _: u32) -> Option<String> {
            Some(self.args.clone())
        }
        fn terminate(&self, pid: u32) -> std::io::Result<()> {
            self.log.borrow_mut().push(format!("term {pid}"));
            if self.stops && self.running.get() == Some(pid) {
                self.running.set(None);
            }
            Ok(())
        }
        fn pid_alive(&self, pid: u32) -> bool {
            self.running.get() == Some(pid)
        }
        fn spawn(&self, t: &Target) -> Result<(), String> {
            self.log.borrow_mut().push(format!("spawn {} {}", t.port, t.db_path.display()));
            if self.comes_up {
                self.running.set(Some(self.next_pid));
            }
            Ok(())
        }
        async fn sleep(&self, _: Duration) {}
        fn own_pid(&self) -> u32 {
            1
        }
    }

    fn info_with(issues: &[(&str, &str)], version: &str, build: &str) -> ServerInfo {
        ServerInfo {
            version: version.into(), build: build.into(), pid: 9, port: 1, db_path: String::new(), in_process: false,
            issues: issues.iter().map(|(s, m)| ninox_core::server_control::ServiceIssue {
                source: (*s).into(), message: (*m).into(), sessions: Vec::new(),
            }).collect(),
        }
    }

    #[test]
    fn update_check_issue_shows_the_fix_command_not_a_generic_aws_alert() {
        let issue = ninox_core::server_control::ServiceIssue {
            source: "update_check".into(),
            message: "update check can't run: the AWS SSO session expired — run `aws sso login --profile p`".into(),
            sessions: Vec::new(),
        };
        let alert = classify(&issue).expect("surfaced");
        assert_eq!(alert.line(), "Update check can't run: the AWS SSO session expired — run `aws sso login --profile p`");
        let other = ninox_core::server_control::ServiceIssue { message: "update check failed: network".into(), ..issue };
        assert!(classify(&other).is_none(), "transient failures stay in the log");
    }

    #[test]
    fn the_stale_daemon_warning_fires_only_on_a_version_or_build_mismatch() {
        let warn = |v: &str, b: &str| stale_daemon_warning(&info_with(&[], v, b), "1.2.0", "abc1234");
        let older = warn("1.1.0", "abc1234").expect("a version mismatch warns");
        assert!(older.starts_with("daemon is running an older build: restart it"), "{older}");
        assert!(older.contains("1.1.0 (abc1234)") && older.contains("this binary 1.2.0 (abc1234)"), "{older}");
        assert!(warn("1.2.0", "0ld0ld0").unwrap().starts_with("daemon is running an older build"), "same version, other hash");
        assert!(warn("1.3.0", "abc1234").unwrap().starts_with("daemon is running a newer build: restart it"));
        assert_eq!(warn("1.2.0", "abc1234"), None);
        assert_eq!(warn("1.2.0", "unknown"), None, "an unknown build can't be compared");
    }

    /// End to end over the real `/api/server/info` request, against a
    /// one-shot fake daemon on a loopback port the OS picks (never 8080).
    #[tokio::test]
    async fn check_daemon_build_warns_about_a_daemon_on_another_build() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        assert_ne!(port, 8080);
        let body = serde_json::to_string(&info_with(&[], env!("CARGO_PKG_VERSION"), "0ld0ld0")).unwrap();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 512];
            let _ = s.read(&mut buf).await;
            let reply = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{body}");
            s.write_all(reply.as_bytes()).await.unwrap();
        });

        let warning = check_daemon_build(port).await;

        if server_control::BUILD_HASH == "unknown" || server_control::BUILD_HASH.is_empty() {
            assert_eq!(warning, None, "nothing to compare a build-less binary against");
        } else {
            let warning = warning.expect("the mismatch must warn");
            assert!(warning.contains("daemon is running an older build: restart it"), "{warning}");
            assert!(warning.contains("0ld0ld0"), "{warning}");
        }
    }

    #[test]
    fn credential_failures_say_what_to_run() {
        let aws = classify_issue("update check failed: credential-provider exited with exit status: 254").unwrap();
        assert_eq!(aws.line(), "AWS login expired: run `aws sso login`, then Restart server");
        let gh = classify_issue("GitHub fetch failed: HTTP 401: Bad credentials").unwrap();
        assert!(gh.hint.contains("gh auth login"));
        let claude = classify_issue("claude: Not logged in · Please run /login").unwrap();
        assert!(claude.title.contains("Claude"));
    }

    #[test]
    fn a_harness_login_failure_names_the_login_and_the_affected_sessions() {
        let issue = ninox_core::server_control::ServiceIssue {
            source: "harness_auth:claude-code".into(),
            message: "Invalid API key · Please run /login".into(),
            sessions: vec!["w1".into(), "w2".into()],
        };
        let alert = classify(&issue).unwrap();
        assert_eq!(alert.line(), "Claude login expired: log in again (`claude /login`), then Restart server");
        assert_eq!(alert.sessions, ["w1", "w2"]);
        let unknown = ninox_core::server_control::ServiceIssue { source: "harness_auth:mystery".into(), ..issue };
        assert_eq!(classify(&unknown), None);
    }

    #[test]
    fn transient_failures_are_not_surfaced() {
        assert_eq!(classify_issue("GitHub fetch failed: connection reset by peer"), None);
        assert_eq!(classify_issue("github batch fetch rate limited"), None);
    }

    #[test]
    fn a_matching_healthy_server_has_no_alerts() {
        assert!(alerts(&info_with(&[], "0.37.0", "abc"), "0.37.0", "abc").is_empty());
    }

    #[test]
    fn version_or_build_mismatch_offers_a_restart() {
        assert_eq!(alerts(&info_with(&[], "0.36.0", "abc"), "0.37.0", "abc").len(), 1);
        let stale = alerts(&info_with(&[], "0.37.0", "old"), "0.37.0", "new");
        assert!(stale[0].title.contains("0.37.0 (old)"), "{stale:?}");
        assert!(alerts(&info_with(&[], "0.37.0", "unknown"), "0.37.0", "new").is_empty(), "unknown builds don't compare");
    }

    #[test]
    fn repeated_auth_issues_collapse_into_one_alert() {
        let info = info_with(
            &[("update_check", "credential-provider exited"), ("github", "401 Bad credentials"), ("harness_auth:claude-code", "Invalid API key")],
            "0.37.0",
            "abc",
        );
        let a = alerts(&info, "0.37.0", "abc");
        assert_eq!(a.len(), 3);
        assert!(a[0].title.starts_with("Claude"), "harness login leads: {a:?}");
    }

    fn target() -> Target {
        Target { port: 45678, db_path: "/scratch/t.db".into(), ninox_bin: "/bin/ninox".into() }
    }

    async fn run(fake: &Fake) -> (Result<Outcome, RestartError>, Vec<Phase>) {
        let mut phases = Vec::new();
        let r = restart(&target(), fake, Timings::default(), |p| phases.push(p)).await;
        (r, phases)
    }

    #[tokio::test]
    async fn stop_start_healthy_reports_old_and_new() {
        let fake = Fake::running(100);
        let (r, phases) = run(&fake).await;
        let out = r.unwrap();
        assert_eq!(out.old.unwrap().pid, 100);
        assert_eq!((out.new.pid, out.new.version.as_str()), (200, "0.38.0"));
        assert_eq!(phases, [Phase::Stopping, Phase::Starting, Phase::WaitingHealthy]);
        assert_eq!(fake.log(), ["shutdown", "spawn 45678 /scratch/t.db"], "targets only the configured port and db");
    }

    #[tokio::test]
    async fn nothing_running_just_starts() {
        let fake = Fake::running(100);
        fake.running.set(None);
        let out = run(&fake).await.0.unwrap();
        assert!(out.old.is_none());
        assert_eq!(fake.log(), ["spawn 45678 /scratch/t.db"]);
    }

    #[tokio::test]
    async fn an_agent_caller_is_refused_before_touching_anything() {
        let fake = Fake { caller: Some("v1-worker".into()), ..Fake::running(100) };
        assert!(matches!(run(&fake).await.0, Err(RestartError::Refused(e)) if e.contains("v1-worker")));
        assert!(fake.log().is_empty());
    }

    #[tokio::test]
    async fn a_daemon_refusal_never_falls_back_to_a_signal() {
        let fake = Fake { reply: ShutdownReply::Refused("refused: worker".into()), ..Fake::running(100) };
        assert!(matches!(run(&fake).await.0, Err(RestartError::Refused(_))));
        assert_eq!(fake.log(), ["shutdown"]);
    }

    #[tokio::test]
    async fn an_old_daemon_without_the_socket_gets_sigterm_by_its_info_pid() {
        let fake = Fake { reply: ShutdownReply::Unreachable("no socket".into()), ..Fake::running(100) };
        run(&fake).await.0.unwrap();
        assert_eq!(fake.log(), ["shutdown", "term 100", "spawn 45678 /scratch/t.db"], "no lsof when info gave the pid");
    }

    #[tokio::test]
    async fn without_info_lsof_is_the_last_resort() {
        let fake = Fake {
            reply: ShutdownReply::Unreachable("no socket".into()), has_info: false, lsof: Some(100),
            ..Fake::running(100)
        };
        assert!(run(&fake).await.0.unwrap().old.is_none());
        assert_eq!(fake.log(), ["shutdown", "lsof", "term 100", "spawn 45678 /scratch/t.db"]);
    }

    #[tokio::test]
    async fn an_unverifiable_caller_is_refused() {
        let fake = Fake { caller: Some("<unverifiable>".into()), ..Fake::running(100) };
        assert!(matches!(run(&fake).await.0, Err(RestartError::Refused(e)) if e.contains("could not verify")));
        assert!(fake.log().is_empty());
    }

    #[tokio::test]
    async fn a_lsof_listener_that_isnt_our_daemon_is_never_signalled() {
        for args in [
            "/usr/local/bin/node server.js --port 45678",
            "/bin/ninox --headless --port 45678 --db /real/ninox.db",
            "/bin/ninox --headless --port 9999 --db /scratch/t.db",
        ] {
            let fake = Fake {
                reply: ShutdownReply::Unreachable("no socket".into()), has_info: false, lsof: Some(100),
                args: args.into(), ..Fake::running(100)
            };
            assert_eq!(run(&fake).await.0, Err(RestartError::PortHeldByOther { port: 45678 }), "{args}");
            assert!(!fake.log().iter().any(|l| l.starts_with("term")), "{args}");
        }
    }

    /// The GUI hosting the server answers info but has no control socket;
    /// another process must not fall back to signalling it.
    #[tokio::test]
    async fn a_server_hosted_inside_an_app_is_never_signalled() {
        let fake = Fake { in_process: true, reply: ShutdownReply::Unreachable("no socket".into()), ..Fake::running(100) };
        assert_eq!(run(&fake).await.0, Err(RestartError::HostedInProcess));
        assert!(fake.log().is_empty());
    }

    #[test]
    fn our_daemon_is_recognised_by_its_command_line() {
        let t = target();
        assert!(is_our_daemon("/Users/x/.cargo/bin/ninox --headless --port 45678 --db /scratch/t.db", &t));
        assert!(is_our_daemon("/Applications/Ninox.app/Contents/MacOS/ninox-gui --headless --port 45678 --db /scratch/t.db", &t));
        assert!(!is_our_daemon("/Users/x/.cargo/bin/ninox --port 45678 --db /scratch/t.db", &t), "not headless");
        assert!(!is_our_daemon("", &t));
    }

    #[tokio::test]
    async fn port_held_by_a_stranger_is_left_alone() {
        let fake = Fake { reply: ShutdownReply::Unreachable("no socket".into()), foreign: true, ..Fake::running(100) };
        fake.running.set(None);
        assert_eq!(run(&fake).await.0, Err(RestartError::PortHeldByOther { port: 45678 }));
        assert_eq!(fake.log(), ["shutdown", "lsof"]);
    }

    #[tokio::test]
    async fn a_daemon_that_wont_stop_times_out_without_sigkill() {
        let fake = Fake { stops: false, ..Fake::running(100) };
        let r = run(&fake).await.0;
        assert!(matches!(r, Err(RestartError::StopTimeout { pid: Some(100), .. })), "{r:?}");
        assert_eq!(fake.log(), ["shutdown", "term 100"], "one graceful SIGTERM, then give up");
    }

    #[tokio::test]
    async fn a_new_daemon_that_never_answers_is_not_healthy() {
        let fake = Fake { comes_up: false, ..Fake::running(100) };
        assert!(matches!(run(&fake).await.0, Err(RestartError::NotHealthy { .. })));
    }

    #[tokio::test]
    async fn the_gui_hosting_the_port_itself_is_refused() {
        let fake = Fake::running(1);
        assert_eq!(run(&fake).await.0, Err(RestartError::HostedInProcess));
        assert!(fake.log().is_empty());
    }

    #[tokio::test]
    async fn a_daemon_serving_another_db_is_left_alone() {
        let fake = Fake { db: "/real/ninox.db".into(), ..Fake::running(100) };
        assert!(matches!(run(&fake).await.0, Err(RestartError::DbMismatch { .. })));
        assert!(fake.log().is_empty());
    }
}
