//! [`SessionBackend`] for a session living on an SSH-connected remote
//! machine (`Session::machine_id: Some(_)`) — see
//! `docs/superpowers/specs/2026-10-05-remote-sessions-design.md`, resolved
//! decision #2.
//!
//! Phase 2: the SSH port-forward tunnel (`crate::remote::ensure_port_forward`)
//! now exists, and [`list_sessions`](SessionBackend::list_sessions) is wired
//! to use it — see that method's doc comment for the request/response path.
//! [`attach_args`](SessionBackend::attach_args) is wired too, recursing into
//! the remote machine's own `ninox pane attach` over SSH (resolved decision
//! #1 — see `crate::remote::attach_exec_argv`), and
//! [`RemoteBackend::list_remote_session_rows`] reuses the tunnel for `Poller`
//! remote reconciliation (spec §4), which needs the full `Session` row
//! rather than `list_sessions`'s liveness-only `LiveSession` projection.
//! Every other method is still honest Phase 1 scaffolding: a clear "remote
//! transport not yet implemented" error (for `Result`-returning methods) or
//! the most conservative non-committal answer (for the trait's few
//! infallible methods: never `true`/`Some`/non-empty, since that would be a
//! fabricated success). Wiring those up — the remote ptyd/engine upgrade
//! flow is the only one left — is explicit follow-on work, not attempted
//! here. (Reconnection/backoff for the SSH control connection itself lives
//! in `Poller::poll_remote_machine_health`, not in this backend.)

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;

use super::{Backend, LiveSession, PaneIdentity, SessionBackend, SessionReactions};
use crate::config::MachineProfile;
use crate::events::Engine;
use crate::types::{Session, SessionId};

fn not_yet_implemented() -> anyhow::Error {
    anyhow!(
        "remote session transport is not yet implemented \
         (see docs/superpowers/specs/2026-10-05-remote-sessions-design.md)"
    )
}

/// The machine a resolved [`RemoteBackend`] talks to, plus everything
/// `list_sessions` needs that isn't pure connection identity.
struct RemoteTarget {
    profile: MachineProfile,
    /// The remote `ninox-server`'s port. Today this is a simplifying
    /// assumption — the port our own `[AppConfig]` is configured with,
    /// baked in at resolution time (see `runtime::resolve_remote_backend`)
    /// rather than discovered — since nothing yet lets `machine add` learn
    /// or store the remote's actual configured port. A future increment
    /// that needs a genuinely different remote port should probe for it
    /// (e.g. a quick `ninox config get port` over the control master) and
    /// extend `MachineProfile` then, rather than guessing now at a shape
    /// nothing yet needs.
    server_port: u16,
}

/// A session kind whose methods either proxy over the SSH tunnel to a
/// specific machine ([`Self::for_machine`]) or, with none resolved
/// ([`Self::unresolved`]), answer exactly as Phase 1 always did. See the
/// module docs.
pub struct RemoteBackend {
    target: Option<RemoteTarget>,
}

impl RemoteBackend {
    pub fn unresolved() -> Self {
        Self { target: None }
    }

    pub fn for_machine(profile: MachineProfile, server_port: u16) -> Self {
        Self { target: Some(RemoteTarget { profile, server_port }) }
    }

    /// Ensures this machine's SSH port forward is up (establishing it lazily
    /// — see `crate::remote`'s module docs) and returns the local base URL
    /// its `ninox-server` is reachable at. Shared by
    /// [`SessionBackend::list_sessions`] and [`Self::list_remote_session_rows`].
    async fn tunnel_base_url(&self) -> Result<String> {
        let target = self
            .target
            .as_ref()
            .ok_or_else(|| anyhow!("no configured machine profile for this remote session"))?;
        let local_port = crate::remote::local_forward_port(&target.profile.id);
        let control_path = crate::remote::control_socket_path(&target.profile.id);
        let ssh_target = target.profile.ssh_target.clone();
        let remote_port = target.server_port;
        tokio::task::spawn_blocking(move || {
            crate::remote::ensure_port_forward(&ssh_target, &control_path, local_port, remote_port)
        })
        .await
        .context("port-forward task panicked")??;
        Ok(format!("http://127.0.0.1:{local_port}"))
    }

    /// Full `Session` rows from this machine's own `ninox-server` store,
    /// reached over the SSH tunnel — the counterpart to `list_sessions`'s
    /// `LiveSession` projection, for `Poller` remote reconciliation (see
    /// `docs/superpowers/specs/2026-10-05-remote-sessions-design.md` §4),
    /// which needs the whole row to write into the local store tagged with
    /// `machine_id`. Returns the remote store's raw session list, unfiltered
    /// by `MachineProfile::remote_session` — same scope as `list_sessions`.
    pub async fn list_remote_session_rows(&self) -> Result<Vec<Session>> {
        let base_url = self.tunnel_base_url().await?;
        fetch_remote_session_rows(&base_url).await
    }
}

/// Shared across calls (one connection pool, reused keep-alive connections
/// to the local tunnel port) rather than built fresh per call — `list_sessions`
/// is the kind of method a future poller calls on a recurring interval.
fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder().timeout(Duration::from_secs(10)).build().unwrap_or_else(|_| reqwest::Client::new())
    })
}

/// `GET /api/v1/sessions/`'s raw response, straight off `base_url` — split
/// out from [`fetch_remote_sessions`] so [`RemoteBackend::list_remote_session_rows`]
/// (full rows, for `Poller` remote reconciliation) and
/// [`SessionBackend::list_sessions`] (the `LiveSession` projection) share one
/// HTTP/JSON path, each testable against a plain local HTTP stand-in without
/// a real tunnel.
async fn fetch_remote_session_rows(base_url: &str) -> Result<Vec<Session>> {
    http_client()
        .get(format!("{base_url}/api/v1/sessions/"))
        .send()
        .await
        .context("remote ninox-server unreachable over the SSH tunnel")?
        .error_for_status()
        .context("remote ninox-server returned an error")?
        .json()
        .await
        .context("couldn't parse the remote ninox-server's session list")
}

/// `LiveSession` projection of [`fetch_remote_session_rows`] — what
/// [`SessionBackend::list_sessions`] needs.
async fn fetch_remote_sessions(base_url: &str) -> Result<Vec<LiveSession>> {
    Ok(fetch_remote_session_rows(base_url).await?.into_iter().map(session_to_live).collect())
}

/// `pid: None` always: a remote pid is meaningless to a local `kill(pid, 0)`
/// check — same reasoning as resolved decision #3 of the remote-sessions
/// design spec, which is why [`SessionBackend::session_pid`] below is `None`
/// too.
fn session_to_live(s: Session) -> LiveSession {
    LiveSession { id: s.id, created_ms: s.started_at, pid: None, backend: Backend::Remote }
}

#[async_trait]
impl SessionBackend for RemoteBackend {
    fn kind(&self) -> Backend {
        Backend::Remote
    }

    async fn knows(&self, _id: &str) -> bool {
        false
    }

    async fn create_session(&self, _id: &str, _workspace: &str, _cmd: &str, _env: &[(&str, &str)]) -> Result<()> {
        Err(not_yet_implemented())
    }

    async fn kill_session(&self, _id: &str) -> Result<()> {
        Err(not_yet_implemented())
    }

    async fn has_session(&self, _id: &str) -> bool {
        false
    }

    /// Lists this backend's one machine's sessions by reaching its
    /// `ninox-server` over the SSH tunnel — the end-to-end demonstration
    /// this phase set out to wire up. With no resolved profile (an
    /// unconfigured or stale `machine_id`), this stays the same honest error
    /// as every other not-yet-implemented method, since there is no machine
    /// to reach. Returns the remote store's raw session list, unfiltered by
    /// `MachineProfile::remote_session` — reconciling that against the
    /// local store (and deciding what "unreachable" should do to those
    /// rows) is `Poller` remote reconciliation, explicit follow-on work.
    async fn list_sessions(&self) -> Result<Vec<LiveSession>> {
        let base_url = self.tunnel_base_url().await?;
        fetch_remote_sessions(&base_url).await
    }

    async fn session_pid(&self, _id: &str) -> Option<u32> {
        None
    }

    /// Recurses into the remote machine's own `ninox pane attach <id>` over
    /// SSH — resolved decision #1 of the remote-sessions design spec. See
    /// [`crate::remote::attach_exec_argv`]. Unresolved (no configured
    /// profile) stays the same honest empty answer as before.
    async fn attach_args(&self, id: &str) -> Vec<String> {
        match &self.target {
            Some(target) => crate::remote::attach_exec_argv(&target.profile, id),
            None => Vec::new(),
        }
    }

    async fn history_size(&self, _id: &str) -> i64 {
        0
    }

    async fn capture_history(&self, _id: &str, _start: i64, _end: i64) -> Vec<u8> {
        Vec::new()
    }

    async fn read_screen(&self, _id: &str, _scrollback: usize, _ansi: bool) -> Result<String> {
        Err(not_yet_implemented())
    }

    async fn send_keys(&self, _id: &str, _text: &str) -> Result<()> {
        Err(not_yet_implemented())
    }

    async fn wake_idle_session(&self, _id: &str) -> Result<()> {
        Err(not_yet_implemented())
    }

    async fn wait_for_input_prompt(&self, _id: &str, _timeout: Duration) -> bool {
        false
    }

    async fn write_input(&self, _id: &str, _bytes: &[u8]) -> Result<()> {
        Err(not_yet_implemented())
    }

    async fn start_streaming(&self, _engine: Arc<Engine>, _session_id: SessionId, _id: &str) -> Result<()> {
        Err(not_yet_implemented())
    }

    /// `Ok(None)`, not an error: this answers "is the calling process one of
    /// *this backend's* panes", and the local caller is, by construction,
    /// never a pane on a remote machine — that is a real, honest answer, not
    /// a stand-in for missing transport.
    async fn current_pane_identity(&self) -> Result<Option<PaneIdentity>> {
        Ok(None)
    }

    /// Never "answering": without a tunnel there is no way to confirm a
    /// remote pane's absence, so this must not claim the host can.
    async fn answering(&self) -> bool {
        false
    }

    async fn ensure_answering(&self, _configured: bool) -> Result<()> {
        Err(not_yet_implemented())
    }
}

/// Remote sessions react like every other kind for PR-lifecycle wording — no
/// overrides there yet (revisit once a remote-tagged session's PR lifecycle
/// needs different wording, e.g. mentioning the machine it ran on). They do
/// differ on pid liveness: a remote session's pid lives on another machine,
/// so `Poller::poll_pids`'s local `kill(pid, 0)` check must never apply to it
/// (resolved decision #3 of the remote-sessions design spec).
impl SessionReactions for RemoteBackend {
    fn pid_is_local(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_not_yet_implemented<T: std::fmt::Debug>(result: Result<T>) {
        let err = result.expect_err("must be an explicit error, never fabricated success");
        assert!(
            err.to_string().contains("not yet implemented"),
            "unexpected error message: {err}"
        );
    }

    #[test]
    fn kind_is_remote() {
        assert_eq!(RemoteBackend::unresolved().kind(), Backend::Remote);
    }

    #[tokio::test]
    async fn every_fallible_method_returns_the_explicit_not_yet_implemented_error() {
        let b = RemoteBackend::unresolved();
        assert_not_yet_implemented(b.create_session("id", "/ws", "cmd", &[]).await);
        assert_not_yet_implemented(b.kill_session("id").await);
        assert_not_yet_implemented(b.read_screen("id", 0, false).await);
        assert_not_yet_implemented(b.send_keys("id", "text").await);
        assert_not_yet_implemented(b.wake_idle_session("id").await);
        assert_not_yet_implemented(b.write_input("id", b"bytes").await);
        let store = Arc::new(crate::store::Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        assert_not_yet_implemented(b.start_streaming(Engine::new(store), "s1".into(), "id").await);
        assert_not_yet_implemented(b.ensure_answering(false).await);
    }

    /// `list_sessions` is no longer uniformly NYI (see
    /// [`fetch_remote_sessions_maps_the_remote_response`] /
    /// [`list_sessions_surfaces_a_clear_error_when_the_tunnel_cant_reach_it`]
    /// for the resolved cases) — an unresolved backend still gets a clear,
    /// explicit error, just not the same wording as the other stubs.
    #[tokio::test]
    async fn list_sessions_without_a_resolved_profile_is_a_clear_explicit_error() {
        let err = RemoteBackend::unresolved().list_sessions().await.expect_err("must error, never fabricate success");
        assert!(err.to_string().contains("no configured machine profile"), "unexpected error message: {err}");
    }

    #[tokio::test]
    async fn infallible_methods_never_fabricate_success() {
        let b = RemoteBackend::unresolved();
        assert!(!b.knows("id").await);
        assert!(!b.has_session("id").await);
        assert_eq!(b.session_pid("id").await, None);
        assert!(b.attach_args("id").await.is_empty());
        assert_eq!(b.history_size("id").await, 0);
        assert!(b.capture_history("id", 0, 10).await.is_empty());
        assert!(!b.wait_for_input_prompt("id", Duration::from_millis(1)).await);
        assert!(!b.answering().await);
    }

    #[tokio::test]
    async fn current_pane_identity_is_honestly_none_not_an_error() {
        assert_eq!(RemoteBackend::unresolved().current_pane_identity().await.unwrap(), None);
    }

    fn fake_session(id: &str, started_at: i64) -> Session {
        Session {
            id: id.to_string(),
            orchestrator_id: None,
            name: id.to_string(),
            repo: "repo".to_string(),
            status: crate::types::SessionStatus::Working,
            agent_type: "claude".to_string(),
            cost_usd: 0.0,
            started_at,
            observed_model: None,
            pr_number: None,
            pr_id: None,
            workspace_path: None,
            pid: Some(12345),
            model: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            context_tokens: None,
            brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None,
            context_total_tokens: None,
            context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None,
            merged_at: None,
            gate_status: None,
            activity: Default::default(),
            activity_note: None,
            activity_since: None,
            machine_id: None,
            cleanup_result: None,
            cleanup_at: None,
        }
    }

    #[tokio::test]
    async fn attach_args_builds_the_ssh_recursion_argv_for_a_resolved_machine() {
        let profile = MachineProfile {
            id: "prof1".to_string(),
            label: "box".to_string(),
            ssh_target: "user@box.example".to_string(),
            remote_session: "default".to_string(),
            enabled: true,
        };
        let backend = RemoteBackend::for_machine(profile.clone(), 7777);
        let argv = backend.attach_args("sess-1").await;
        assert_eq!(argv, crate::remote::attach_exec_argv(&profile, "sess-1"));
    }

    #[test]
    fn session_to_live_drops_the_remote_pid_and_maps_the_backend() {
        let live = session_to_live(fake_session("s1", 42));
        assert_eq!(live, LiveSession { id: "s1".to_string(), created_ms: 42, pid: None, backend: Backend::Remote });
    }

    /// A one-shot raw HTTP/1.1 responder standing in for a remote
    /// `ninox-server`'s `GET /api/v1/sessions/` — bound to an ephemeral
    /// local port, serving exactly one request on a background thread, no
    /// SSH or tunnel involved. Proves [`fetch_remote_sessions`]'s HTTP/JSON
    /// handling independent of the tunnel mechanism (covered separately,
    /// with a real SSH round trip, in `remote::tests`).
    fn spawn_one_shot_json_server(body: &'static str) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body,
            );
            let _ = stream.write_all(response.as_bytes());
        });
        port
    }

    #[tokio::test]
    async fn fetch_remote_sessions_maps_the_remote_response() {
        let port = spawn_one_shot_json_server(r#"[{"id":"s1","orchestrator_id":null,"name":"s1","repo":"r","status":"working","agent_type":"claude","cost_usd":0.0,"started_at":100,"pr_number":null,"pr_id":null,"workspace_path":null,"pid":999,"model":null,"worker_type":null,"context_tokens":null,"gate_status":null,"activity":"idle","activity_note":null,"activity_since":null,"machine_id":null}]"#);
        let got = fetch_remote_sessions(&format!("http://127.0.0.1:{port}")).await.unwrap();
        assert_eq!(got, vec![LiveSession { id: "s1".to_string(), created_ms: 100, pid: None, backend: Backend::Remote }]);
    }

    /// The full-row counterpart to `fetch_remote_sessions_maps_the_remote_response`
    /// — `Poller` remote reconciliation needs the whole row, not just the
    /// `LiveSession` projection.
    #[tokio::test]
    async fn fetch_remote_session_rows_returns_the_full_row() {
        let port = spawn_one_shot_json_server(r#"[{"id":"s1","orchestrator_id":null,"name":"s1","repo":"r","status":"working","agent_type":"claude","cost_usd":1.5,"started_at":100,"pr_number":null,"pr_id":null,"workspace_path":null,"pid":999,"model":null,"worker_type":null,"context_tokens":null,"gate_status":null,"activity":"idle","activity_note":null,"activity_since":null,"machine_id":null}]"#);
        let got = fetch_remote_session_rows(&format!("http://127.0.0.1:{port}")).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "s1");
        assert_eq!(got[0].cost_usd, 1.5, "full row, not the LiveSession projection, must come through");
        assert_eq!(got[0].pid, Some(999), "the remote's own pid must not be dropped here (unlike `session_to_live`)");
    }

    #[tokio::test]
    async fn list_sessions_surfaces_a_clear_error_when_the_tunnel_cant_reach_it() {
        // No control master was ever established for this profile id, so
        // `ensure_port_forward` finds no live `ControlMaster` and errors
        // rather than hanging or fabricating an empty session list.
        let profile = MachineProfile {
            id: "unreachable-profile".to_string(),
            label: "test".to_string(),
            ssh_target: "nonexistent.invalid".to_string(),
            remote_session: "default".to_string(),
            enabled: true,
        };
        let backend = RemoteBackend::for_machine(profile, 7777);
        let err = backend.list_sessions().await.expect_err("must error, never fabricate success");
        assert!(err.to_string().contains("no active SSH connection"), "unexpected error message: {err}");
    }

    #[tokio::test]
    async fn list_remote_session_rows_surfaces_a_clear_error_when_the_tunnel_cant_reach_it() {
        let profile = MachineProfile {
            id: "unreachable-profile-2".to_string(),
            label: "test".to_string(),
            ssh_target: "nonexistent.invalid".to_string(),
            remote_session: "default".to_string(),
            enabled: true,
        };
        let backend = RemoteBackend::for_machine(profile, 7777);
        let err = backend.list_remote_session_rows().await.expect_err("must error, never fabricate success");
        assert!(err.to_string().contains("no active SSH connection"), "unexpected error message: {err}");
    }
}
