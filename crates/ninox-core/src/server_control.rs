//! The daemon's control plane: its identity ([`ServerInfo`], also served at
//! `GET /api/server/info`) and a Unix control socket that accepts shutdown.
//!
//! Shutdown is deliberately not an HTTP route. The HTTP API is loopback TCP
//! behind a permissive CORS layer, so it can't tell which process (or web
//! page) is calling. A Unix socket hands the daemon the kernel-reported
//! peer pid, and that pid goes through the same live-caller rule `ninox
//! spawn` uses ([`orchestrator_auth::resolve_live_session_id_for_pid`]):
//! only a caller with no live managed session in its ancestry — the user's
//! own terminal, the GUI, the TUI — may stop the daemon. Agents get a
//! refusal.
//!
//! This keeps a cooperating agent from restarting the server by accident
//! or habit; it is not a boundary against a hostile one. Any same-uid
//! process can `kill` the daemon outright, and a process an agent detaches
//! from its own tree (`tmux run-shell`, a double fork) has no managed
//! ancestor to find.

use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::sync::CancellationToken;

use crate::{orchestrator_auth, store::Store};

pub const BUILD_HASH: &str = env!("NINOX_BUILD_HASH");

const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub version: String,
    pub build:   String,
    pub pid:     u32,
    pub port:    u16,
    pub db_path: String,
    /// The server runs inside an app (the GUI) rather than as a headless
    /// daemon: it has no control socket, and restarting it means
    /// relaunching that app, never signalling it.
    #[serde(default)]
    pub in_process: bool,
    /// Background failures still current at the time of the request.
    #[serde(default)]
    pub issues:  Vec<ServiceIssue>,
}

/// A failing background check (`source`: "update_check", "github"),
/// carrying the error text the UIs classify into "what to fix, then
/// restart the server". Replaced on each failure, cleared on success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceIssue {
    pub source:   String,
    pub message:  String,
    /// For a harness login failure: the sessions whose panes show it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<String>,
}

/// Source of the issue [`harness_auth_issues`] builds for `harness`.
pub fn harness_auth_source(harness: &str) -> String {
    format!("harness_auth:{harness}")
}

/// The store's harness login failures, one issue per harness.
pub fn harness_auth_issues(store: &Store) -> Vec<ServiceIssue> {
    let mut out: Vec<ServiceIssue> = Vec::new();
    for row in store.harness_auth_failures().unwrap_or_default() {
        let source = harness_auth_source(&row.harness);
        let issue = match out.iter_mut().find(|i| i.source == source) {
            Some(issue) => issue,
            None => {
                out.push(ServiceIssue { source, message: row.detail.clone(), sessions: Vec::new() });
                out.last_mut().expect("just pushed")
            }
        };
        if let Some(id) = row.session_id {
            issue.sessions.push(id);
        }
    }
    out
}

static ISSUES: std::sync::Mutex<std::collections::BTreeMap<&'static str, String>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

pub fn report_issue(source: &'static str, message: impl Into<String>) {
    ISSUES.lock().unwrap_or_else(|e| e.into_inner()).insert(source, message.into());
}

pub fn clear_issue(source: &'static str) {
    ISSUES.lock().unwrap_or_else(|e| e.into_inner()).remove(source);
}

pub fn current_issues() -> Vec<ServiceIssue> {
    ISSUES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(source, message)| ServiceIssue { source: (*source).to_string(), message: message.clone(), sessions: Vec::new() })
        .collect()
}

impl ServerInfo {
    pub fn current(port: u16, db_path: &Path, in_process: bool) -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            build:   BUILD_HASH.to_string(),
            pid:     std::process::id(),
            port,
            db_path: db_path.to_string_lossy().into_owned(),
            in_process,
            issues:  Vec::new(),
        }
    }

    pub fn label(&self) -> String {
        format!("{} ({})", self.version, self.build)
    }
}

/// Where the daemon serving `port` against `db_path` listens for control
/// requests. Keyed on both so a scratch instance never shares a socket with
/// the real daemon, and kept under the temp dir because a data-dir path can
/// exceed the 104-byte `sun_path` limit. The directory is per-uid: on Linux
/// the temp dir is the shared `/tmp`.
pub fn control_socket_path(port: u16, db_path: &Path) -> PathBuf {
    let db = std::fs::canonicalize(db_path)
        .or_else(|_| std::path::absolute(db_path))
        .unwrap_or_else(|_| db_path.to_path_buf());
    std::env::temp_dir()
        .join(format!("ninox-ctl-{}", own_uid()))
        .join(format!("{port}-{:016x}.sock", fnv1a(db.as_os_str().as_encoded_bytes())))
}

fn own_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// The socket's directory must be ours and private: anyone else who could
/// create it could also stand in for the daemon.
fn check_socket_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(dir).with_context(|| format!("stat {}", dir.display()))?;
    anyhow::ensure!(meta.is_dir(), "{} is not a directory", dir.display());
    anyhow::ensure!(meta.uid() == own_uid(), "{} is owned by uid {}, not us", dir.display(), meta.uid());
    anyhow::ensure!(meta.mode() & 0o077 == 0, "{} is accessible to other users", dir.display());
    Ok(())
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3))
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Request {
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
struct Reply {
    ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Who may stop the daemon: a same-uid peer whose process tree holds no
/// live managed session. Everything uncertain — an unknown pid, a store
/// error — refuses.
pub fn authorize_shutdown(
    peer_uid: u32,
    own_uid:  u32,
    peer_pid: Option<u32>,
    resolve:  impl FnOnce(u32) -> Result<Option<String>>,
) -> Result<(), String> {
    if peer_uid != own_uid {
        return Err("refused: caller runs as a different user".to_string());
    }
    let Some(pid) = peer_pid else {
        return Err("refused: the caller's pid could not be determined".to_string());
    };
    match resolve(pid) {
        Ok(None) => Ok(()),
        Ok(Some(session)) => Err(format!(
            "refused: only the user can restart the server, and this caller runs inside managed session `{session}`"
        )),
        Err(e) => Err(format!("refused: could not verify the caller ({e})")),
    }
}

/// Serve the control socket at `path` until `shutdown` is cancelled — by
/// an accepted shutdown request or by the daemon stopping for any other
/// reason. The socket file is removed on the way out.
pub async fn serve_control(path: PathBuf, store: Arc<Store>, shutdown: CancellationToken) -> Result<()> {
    let dir = path.parent().context("control socket path has no parent")?;
    if !dir.exists() {
        std::fs::DirBuilder::new().mode(0o700).create(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    check_socket_dir(dir)?;
    if path.exists() {
        if UnixStream::connect(&path).await.is_ok() {
            bail!("control socket {} is already served by another daemon", path.display());
        }
        std::fs::remove_file(&path)?;
    }
    let listener = UnixListener::bind(&path).with_context(|| format!("bind {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let _cleanup = RemoveOnDrop(path);
    let own_uid = own_uid();
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return Ok(()),
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(pair) => pair,
                    Err(e) => {
                        tracing::warn!("control socket accept: {e}");
                        continue;
                    }
                };
                let store = Arc::clone(&store);
                let shutdown = shutdown.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle(stream, store, own_uid, shutdown).await {
                        tracing::warn!("control socket request: {e}");
                    }
                });
            }
        }
    }
}

struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

async fn handle(stream: UnixStream, store: Arc<Store>, own_uid: u32, shutdown: CancellationToken) -> Result<()> {
    let cred = stream.peer_cred()?;
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    tokio::time::timeout(REPLY_TIMEOUT, BufReader::new(read).read_line(&mut line)).await??;
    let reply = match serde_json::from_str::<Request>(line.trim()) {
        Err(e) => Reply { ok: false, error: Some(format!("bad request: {e}")) },
        Ok(Request::Shutdown) => {
            let peer_pid = cred.pid().and_then(|p| u32::try_from(p).ok());
            let verdict = authorize_shutdown(cred.uid(), own_uid, peer_pid, |pid| {
                orchestrator_auth::resolve_live_session_id_for_pid(&store, pid)
            });
            match verdict {
                Ok(()) => {
                    tracing::warn!("shutdown requested over the control socket by {}", describe_peer(peer_pid).await);
                    Reply { ok: true, error: None }
                }
                Err(e) => {
                    tracing::warn!("control socket shutdown from {} {e}", describe_peer(peer_pid).await);
                    Reply { ok: false, error: Some(e) }
                }
            }
        }
    };
    let mut out = serde_json::to_vec(&reply)?;
    out.push(b'\n');
    write.write_all(&out).await?;
    write.shutdown().await?;
    if reply.ok {
        shutdown.cancel();
    }
    Ok(())
}

/// `pid 123 (ninox server)`: the peer's executable name and, for ninox
/// binaries, an allow-listed subcommand, so a stop in the log says who asked.
/// Never the rest of argv: it can carry tokens, and the log is a file.
async fn describe_peer(pid: Option<u32>) -> String {
    let Some(pid) = pid else { return "an unknown pid".to_string() };
    let ps = tokio::process::Command::new("/bin/ps")
        .args(["-o", "args=", "-p", &pid.to_string()])
        .kill_on_drop(true)
        .output();
    let args = match tokio::time::timeout(Duration::from_secs(1), ps).await {
        Ok(Ok(out)) => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => String::new(),
    };
    describe_pid(pid, &args)
}

const PEER_SUBCOMMANDS: &[&str] = &["server", "service", "tui", "gui", "connect", "orchestrate", "fleet", "update"];

fn describe_pid(pid: u32, args: &str) -> String {
    let mut words = args.split_whitespace();
    let Some(exe) = words.next().and_then(|w| Path::new(w).file_name()).and_then(|n| n.to_str()) else {
        return format!("pid {pid}");
    };
    let is_ninox = matches!(exe, "ninox" | "nx" | "ninox-gui" | "ninox-tui");
    match words.next().filter(|w| is_ninox && PEER_SUBCOMMANDS.contains(w)) {
        Some(sub) => format!("pid {pid} ({exe} {sub})"),
        None => format!("pid {pid} ({exe})"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShutdownReply {
    Accepted,
    /// The daemon answered and said no — never retried with a signal.
    Refused(String),
    /// Nothing answered: no daemon, or one predating the control socket.
    Unreachable(String),
}

pub async fn request_shutdown(path: &Path) -> ShutdownReply {
    if let Some(dir) = path.parent() {
        if let Err(e) = check_socket_dir(dir) {
            return ShutdownReply::Unreachable(e.to_string());
        }
    }
    let exchange = async {
        let mut stream = UnixStream::connect(path).await?;
        let mut req = serde_json::to_vec(&Request::Shutdown)?;
        req.push(b'\n');
        stream.write_all(&req).await?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).await?;
        anyhow::Ok(serde_json::from_str::<Reply>(line.trim())?)
    };
    match tokio::time::timeout(REPLY_TIMEOUT, exchange).await {
        Ok(Ok(Reply { ok: true, .. })) => ShutdownReply::Accepted,
        Ok(Ok(Reply { error, .. })) => ShutdownReply::Refused(error.unwrap_or_else(|| "refused".to_string())),
        Ok(Err(e)) => ShutdownReply::Unreachable(e.to_string()),
        Err(_) => ShutdownReply::Unreachable("timed out".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Session, SessionStatus};

    fn ok_user(_: u32) -> Result<Option<String>> {
        Ok(None)
    }

    #[test]
    fn a_peer_is_described_without_its_arguments() {
        let line = describe_pid(7, "/Users/x/.cargo/bin/ninox server restart --token SECRET");
        assert_eq!(line, "pid 7 (ninox server)");
        let line = describe_pid(8, "/usr/bin/curl --token SECRET http://x");
        assert_eq!(line, "pid 8 (curl)");
        let line = describe_pid(9, "ninox --token SECRET server");
        assert_eq!(line, "pid 9 (ninox)");
        assert_eq!(describe_pid(10, ""), "pid 10");
        for l in [describe_pid(7, "ninox server --token SECRET"), describe_pid(8, "sh -c SECRET")] {
            assert!(!l.contains("SECRET"), "{l}");
        }
    }

    #[test]
    fn the_user_may_shut_down() {
        assert_eq!(authorize_shutdown(501, 501, Some(42), ok_user), Ok(()));
    }

    #[test]
    fn a_managed_session_is_refused() {
        let err = authorize_shutdown(501, 501, Some(42), |_| Ok(Some("v1-worker".to_string()))).unwrap_err();
        assert!(err.contains("v1-worker"), "{err}");
    }

    #[test]
    fn another_uid_is_refused_before_any_lookup() {
        let refused = authorize_shutdown(502, 501, Some(42), |_| panic!("must not resolve"));
        assert!(refused.is_err());
    }

    #[test]
    fn an_unknown_pid_or_store_error_is_refused() {
        assert!(authorize_shutdown(501, 501, None, ok_user).is_err());
        assert!(authorize_shutdown(501, 501, Some(42), |_| bail!("db locked")).is_err());
    }

    #[test]
    fn socket_path_is_keyed_on_port_and_db() {
        let a = control_socket_path(40001, Path::new("/x/one.db"));
        assert_ne!(a, control_socket_path(40002, Path::new("/x/one.db")));
        assert_ne!(a, control_socket_path(40001, Path::new("/x/two.db")));
        assert_eq!(a, control_socket_path(40001, Path::new("/x/one.db")));
        assert!(a.as_os_str().len() < 104, "{}", a.display());
    }

    fn live_session_rooted_at_this_process() -> Session {
        Session {
            id: "v1-worker".into(), orchestrator_id: Some("ninox".into()), name: "v1-worker".into(),
            repo: String::new(), status: SessionStatus::Working, agent_type: "claude-code".into(),
            cost_usd: 0.0, started_at: 0, pr_number: None, pr_id: None, workspace_path: None,
            pid: Some(std::process::id()), model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None, machine_id: None,
            worker_type: None, success_criteria: Vec::new(), completion_outcome: None,
            self_completed_at: None, cleanup_result: None, cleanup_at: None,
        }
    }

    /// End to end over a real socket: the kernel reports this test process
    /// as the peer, so recording it as a live worker's root pid makes the
    /// caller a worker.
    #[tokio::test]
    async fn the_socket_refuses_a_worker_and_accepts_the_user() {
        let _iso = crate::test_isolation::isolate();
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("ctl.sock");
        let store = Arc::new(Store::open(dir.path().join("t.db")).unwrap());
        store.upsert_session(&live_session_rooted_at_this_process()).unwrap();
        let token = CancellationToken::new();
        let server = tokio::spawn(serve_control(path.clone(), Arc::clone(&store), token.clone()));
        for _ in 0..50 {
            if path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let reply = request_shutdown(&path).await;
        assert!(matches!(&reply, ShutdownReply::Refused(e) if e.contains("v1-worker")), "{reply:?}");
        assert!(!token.is_cancelled());

        let mut done = live_session_rooted_at_this_process();
        done.status = SessionStatus::Done;
        store.upsert_session(&done).unwrap();
        assert_eq!(request_shutdown(&path).await, ShutdownReply::Accepted);
        tokio::time::timeout(Duration::from_secs(5), server).await.unwrap().unwrap().unwrap();
        assert!(token.is_cancelled());
        assert!(!path.exists(), "socket file is removed on shutdown");
    }

    #[test]
    fn a_socket_dir_others_can_reach_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(check_socket_dir(&shared).is_err());
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(check_socket_dir(&shared).is_ok());
    }

    #[tokio::test]
    async fn nothing_listening_is_unreachable() {
        let dir = tempfile::tempdir().unwrap();
        let reply = request_shutdown(&dir.path().join("none.sock")).await;
        assert!(matches!(reply, ShutdownReply::Unreachable(_)), "{reply:?}");
    }
}
