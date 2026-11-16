use ninox_core::{
    config::{AppConfig, RemoteMachinesConfig},
    events::Engine,
    lifecycle::brain_harvest,
    remote,
    runtime::SessionBackend,
    types::{Session, SessionKindData},
};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub fn sessions_router(engine: Arc<Engine>) -> Router {
    Router::new()
        .route("/", get(list_sessions))
        .route("/:id", axum::routing::delete(terminate_session))
        .route("/:id/diff", get(session_diff))
        .route("/:id/acp", post(create_acp_session))
        .route("/:id/acp/send", post(send_acp_message))
        .with_state(engine)
}

async fn list_sessions(State(e): State<Arc<Engine>>) -> Result<Json<Vec<Session>>, StatusCode> {
    e.store
        .list_sessions()
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

#[derive(Serialize)]
struct DiffResponse {
    /// `None` when the session has no recorded workspace, or its workspace
    /// has no diff against the default branch yet.
    diff: Option<String>,
}

/// Computes a remote session's diff over SSH. Production code shells to the
/// real `ssh` binary via `ninox_core::remote` ([`SshRemoteDiff`]); tests
/// inject a fake so the routing and error-surfacing behavior below is
/// verified without a real remote host.
trait RemoteDiff: Send + Sync {
    fn diff(&self, machine_id: &str, ssh_target: &str, workspace: &str) -> Result<Option<String>, String>;
}

struct SshRemoteDiff;

impl RemoteDiff for SshRemoteDiff {
    fn diff(&self, machine_id: &str, ssh_target: &str, workspace: &str) -> Result<Option<String>, String> {
        let control_path = remote::control_socket_path(machine_id);
        remote::remote_git_diff(ssh_target, &control_path, workspace).map_err(|e| e.to_string())
    }
}

/// `Store`/`NotFound` keep today's bodyless error responses; `Remote` carries
/// the specific SSH/git failure text so a caller can tell "no changes" apart
/// from "the remote diff actually failed" — see resolved decision #4 of
/// `docs/superpowers/specs/2026-10-05-remote-sessions-design.md`.
enum DiffError {
    Store,
    NotFound,
    Remote(String),
}

impl IntoResponse for DiffError {
    fn into_response(self) -> Response {
        match self {
            DiffError::Store => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            DiffError::NotFound => StatusCode::NOT_FOUND.into_response(),
            DiffError::Remote(error) => {
                (StatusCode::BAD_GATEWAY, Json(serde_json::json!({ "error": error }))).into_response()
            }
        }
    }
}

/// Resolves `session`'s diff: a local session (`machine_id: None`) is
/// unaffected and goes through today's local `git diff` via
/// `brain_harvest::compute_diff`; a session with `machine_id: Some(_)` routes
/// through `remote_diff` instead, over the matching `MachineProfile`'s
/// `ssh_target` — see resolved decision #4 of the remote-sessions design
/// spec. Takes `remote_machines`/`remote_diff` as parameters, rather than
/// loading `AppConfig`/constructing `SshRemoteDiff` itself, so this is
/// testable without a real config file or SSH binary.
async fn diff_for_session(
    session: &Session,
    workspace: &str,
    remote_machines: &RemoteMachinesConfig,
    remote_diff: Arc<dyn RemoteDiff>,
) -> Result<Option<String>, String> {
    let machine_id = match session.kind_data() {
        SessionKindData::Local => return Ok(brain_harvest::compute_diff(std::path::Path::new(workspace)).await),
        SessionKindData::Remote { machine_id } => machine_id,
    };

    let ssh_target = remote_machines
        .machines
        .iter()
        .find(|m| m.id == machine_id)
        .map(|m| m.ssh_target.clone())
        .ok_or_else(|| format!("no configured machine profile for machine_id {machine_id}"))?;

    let workspace = workspace.to_string();
    match tokio::task::spawn_blocking(move || remote_diff.diff(&machine_id, &ssh_target, &workspace)).await {
        Ok(result) => result,
        Err(err) => Err(format!("remote diff task panicked: {err}")),
    }
}

/// The session's current unfiltered diff against its default branch —
/// computed live on every call (no caching), since this is meant to reflect
/// an in-progress session's diff as it changes, not just the one-shot diff
/// snapshotted after a PR opens (see `lifecycle::brain_harvest`).
async fn session_diff(
    State(e): State<Arc<Engine>>,
    Path(id): Path<String>,
) -> Result<Json<DiffResponse>, DiffError> {
    let session = e
        .store
        .get_session(&id)
        .map_err(|_| DiffError::Store)?
        .ok_or(DiffError::NotFound)?;

    let Some(workspace) = session.workspace_path.clone() else {
        return Ok(Json(DiffResponse { diff: None }));
    };

    // Only a remote session needs `AppConfig` at all — a local session's
    // diff path reads no config, same as before this routing existed.
    let remote_machines = match session.kind_data() {
        SessionKindData::Remote { .. } => AppConfig::load()
            .map_err(|err| DiffError::Remote(format!("failed to load config: {err}")))?
            .remote_machines,
        SessionKindData::Local => RemoteMachinesConfig::default(),
    };

    let diff = diff_for_session(&session, &workspace, &remote_machines, Arc::new(SshRemoteDiff))
        .await
        .map_err(DiffError::Remote)?;
    Ok(Json(DiffResponse { diff }))
}

async fn terminate_session(
    State(e): State<Arc<Engine>>,
    Path(id): Path<String>,
) -> StatusCode {
    match e.terminate_session(&id).await {
        Ok(_) => StatusCode::NO_CONTENT,
        Err(err) => {
            tracing::error!("terminate {id}: {err}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

#[derive(Deserialize)]
struct CreateAcpSessionRequest {
    workspace: String,
    /// Which registered ACP agent command to launch — `"opencode"`,
    /// `"codex"`, or `"pi"` today (see
    /// `ninox_core::runtime::AcpBackend::command_for`). Required rather than
    /// defaulting silently: nothing calls this route in production yet (see
    /// below), so there's no legacy caller to stay compatible with, and a
    /// caller that got the harness wrong deserves a clear error over a
    /// quietly-wrong agent.
    harness: String,
}

/// Spawns the registered ACP agent command for `harness` (opencode/codex/pi)
/// against `id` and completes its handshake, holding the connection on
/// `e.acp` — the one `AcpBackend` instance this server process keeps for its
/// whole lifetime — for the session's full life. See the
/// cross-harness-messaging design spec's "Connection ownership" section:
/// nothing else creates one of these yet (per-harness spawn routing is a
/// separate, not-yet-implemented follow-on), so today this is the only path
/// that brings an ACP-backed session into existence.
async fn create_acp_session(
    State(e): State<Arc<Engine>>,
    Path(id): Path<String>,
    Json(req): Json<CreateAcpSessionRequest>,
) -> Result<StatusCode, (StatusCode, String)> {
    e.acp
        .create_session(&id, &req.workspace, &req.harness, &[])
        .await
        .map(|_| StatusCode::CREATED)
        .map_err(|err| (StatusCode::BAD_GATEWAY, err.to_string()))
}

#[derive(Deserialize)]
struct SendAcpMessageRequest {
    text: String,
}

enum SendAcpError {
    /// No ACP connection is held for this id — not necessarily an error on
    /// the caller's part (e.g. this session was never ACP-backed); callers
    /// like `messaging::AcpRouter` treat this as "fall through", not a
    /// failure.
    NotFound,
    /// A connection IS held, but delivery through it failed.
    Delivery(String),
}

impl IntoResponse for SendAcpError {
    fn into_response(self) -> Response {
        match self {
            SendAcpError::NotFound => StatusCode::NOT_FOUND.into_response(),
            SendAcpError::Delivery(err) => (StatusCode::BAD_GATEWAY, err).into_response(),
        }
    }
}

/// How long this route waits for an ACP `session/prompt` turn before
/// detaching it: short enough to stay well under `HttpAcpRouter`'s own
/// client-side request timeout, long enough to still catch (and report as a
/// real 502) the fast failures — a missing handshake, a dead connection —
/// rather than only ever the slow ones.
/// Writes `text` as a `session/prompt` turn over the ACP connection this
/// server already holds for `id` — the counterpart to `terminate_session`'s
/// `StatusCode`-only shape, but distinguishing "no such connection" (404,
/// not an error) from "connection exists but delivery failed" (502, a real
/// error) — see [`SendAcpError`] and `messaging::AcpRouter`'s doc comment
/// for why that distinction matters to callers.
///
/// Delegates the actual turn to [`ninox_core::runtime::AcpBackend::send_keys_detached`]
/// rather than awaiting it inline: it can run for up to `AcpBackend`'s
/// `PROMPT_TIMEOUT` (600s) — an agent thinking, not an HTTP call — so this
/// route only waits up to `SEND_ACK_WINDOW` for an early result before
/// reporting "accepted" and letting the turn keep running in the
/// background, the same way every other delivery mechanism here (tmux
/// keystrokes, the session socket) never waits for the agent's reply
/// either.
async fn send_acp_message(
    State(e): State<Arc<Engine>>,
    Path(id): Path<String>,
    Json(req): Json<SendAcpMessageRequest>,
) -> Result<StatusCode, SendAcpError> {
    if !e.acp.knows(&id).await {
        return Err(SendAcpError::NotFound);
    }
    use ninox_core::runtime::DetachedSendOutcome;
    match e.acp.clone().send_keys_detached(id, req.text, ninox_core::runtime::SEND_ACK_WINDOW).await {
        DetachedSendOutcome::Delivered => Ok(StatusCode::OK),
        DetachedSendOutcome::Failed(err) => Err(SendAcpError::Delivery(err.to_string())),
        DetachedSendOutcome::StillRunning => Ok(StatusCode::ACCEPTED),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::{events::Engine, runtime::AcpBackend, store::Store, types::*};
    use axum::body::Body;
    use http::{Request, StatusCode};
    use std::sync::Arc;
    use tempfile::tempdir;
    use tower::ServiceExt;

    fn json_post(uri: &str, body: serde_json::Value) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    }

    /// A fake `opencode acp` agent (same technique as `AcpBackend`'s own
    /// unit tests — see `runtime::acp_backend::tests`): completes the
    /// handshake, then answers every `session/prompt` turn with
    /// `end_turn`, echoing back whatever request id it was sent. Appends
    /// its own pid to `pid_log` exactly once, on start — the probe this
    /// test uses to prove the server spawned the agent once and reused it
    /// across multiple sends, rather than respawning per message.
    fn fake_agent_command(pid_log: &std::path::Path) -> Vec<String> {
        let script = format!(
            r#"
echo $$ >> {pid_log}
read -r _init
printf '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":1,"agentCapabilities":{{}},"authMethods":[]}}}}\n'
read -r _new
printf '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"fake-session-1"}}}}\n'
while read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  printf '{{"jsonrpc":"2.0","id":%s,"result":{{"stopReason":"end_turn"}}}}\n' "$id"
done
"#,
            pid_log = pid_log.display(),
        );
        vec!["sh".to_string(), "-c".to_string(), script]
    }

    #[tokio::test]
    async fn the_server_holds_one_acp_connection_across_multiple_sends_without_respawning() {
        let store = Arc::new(Store::open(tempdir().unwrap().keep().join("t.db")).unwrap());
        let pid_log = tempdir().unwrap().keep().join("pids");
        let acp = Arc::new(AcpBackend::with_agent_command(fake_agent_command(&pid_log)));
        let engine = Engine::new_with_acp(store, acp);
        let workspace = tempdir().unwrap().keep();

        let create = sessions_router(engine.clone())
            .oneshot(json_post(
                "/s1/acp",
                serde_json::json!({ "workspace": workspace.to_str().unwrap(), "harness": "opencode" }),
            ))
            .await
            .unwrap();
        assert_eq!(create.status(), StatusCode::CREATED);

        for text in ["hello", "a second message"] {
            let send = sessions_router(engine.clone())
                .oneshot(json_post("/s1/acp/send", serde_json::json!({ "text": text })))
                .await
                .unwrap();
            assert_eq!(send.status(), StatusCode::OK, "send of {text:?} must succeed");
        }

        let pids = std::fs::read_to_string(&pid_log).unwrap();
        assert_eq!(
            pids.lines().count(),
            1,
            "the agent subprocess must be spawned exactly once across create + two sends: {pids:?}"
        );
    }

    /// Same handshake as [`fake_agent_command`], but every `session/prompt`
    /// reply is delayed by `delay_secs` — long enough to outlast
    /// `SEND_ACK_WINDOW`, so a test can observe `send_acp_message`'s
    /// detach-and-report-202 path instead of always hitting its fast,
    /// instant-reply sibling above.
    fn fake_agent_command_with_prompt_delay(pid_log: &std::path::Path, delay_secs: u64) -> Vec<String> {
        let script = format!(
            r#"
echo $$ >> {pid_log}
read -r _init
printf '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":1,"agentCapabilities":{{}},"authMethods":[]}}}}\n'
read -r _new
printf '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"fake-session-1"}}}}\n'
while read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  sleep {delay_secs}
  printf '{{"jsonrpc":"2.0","id":%s,"result":{{"stopReason":"end_turn"}}}}\n' "$id"
done
"#,
            pid_log = pid_log.display(),
        );
        vec!["sh".to_string(), "-c".to_string(), script]
    }

    #[tokio::test]
    async fn a_turn_still_running_past_the_ack_window_returns_202_and_still_completes_in_the_background() {
        let store = Arc::new(Store::open(tempdir().unwrap().keep().join("t.db")).unwrap());
        let pid_log = tempdir().unwrap().keep().join("pids");
        let acp = Arc::new(AcpBackend::with_agent_command(fake_agent_command_with_prompt_delay(&pid_log, 4)));
        let engine = Engine::new_with_acp(store, acp);
        let workspace = tempdir().unwrap().keep();

        let create = sessions_router(engine.clone())
            .oneshot(json_post(
                "/s1/acp",
                serde_json::json!({ "workspace": workspace.to_str().unwrap(), "harness": "opencode" }),
            ))
            .await
            .unwrap();
        assert_eq!(create.status(), StatusCode::CREATED);

        let send = sessions_router(engine.clone())
            .oneshot(json_post("/s1/acp/send", serde_json::json!({ "text": "hello" })))
            .await
            .unwrap();
        assert_eq!(
            send.status(),
            StatusCode::ACCEPTED,
            "a turn outlasting SEND_ACK_WINDOW must detach, not block the caller for the full 4s"
        );

        // The fake agent still has ~1s left on its 4s delay; give it margin.
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        let transcript = engine.acp.transcript_history("s1").await;
        assert!(
            transcript.iter().any(|e| matches!(e, ninox_core::runtime::TranscriptEntry::TurnEnded)),
            "the detached turn must still run to completion in the background: {transcript:?}"
        );
    }

    #[tokio::test]
    async fn sending_to_a_session_with_no_held_acp_connection_is_404_not_a_server_error() {
        let engine = Engine::new(Arc::new(Store::open(tempdir().unwrap().keep().join("t.db")).unwrap()));

        let response = sessions_router(engine)
            .oneshot(json_post("/never-created/acp/send", serde_json::json!({ "text": "hi" })))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn creating_an_acp_session_twice_for_the_same_id_is_a_bad_gateway_not_a_silent_success() {
        let store = Arc::new(Store::open(tempdir().unwrap().keep().join("t.db")).unwrap());
        let pid_log = tempdir().unwrap().keep().join("pids");
        let acp = Arc::new(AcpBackend::with_agent_command(fake_agent_command(&pid_log)));
        let engine = Engine::new_with_acp(store, acp);
        let workspace = tempdir().unwrap().keep();
        let body = serde_json::json!({ "workspace": workspace.to_str().unwrap(), "harness": "opencode" });

        let first = sessions_router(engine.clone()).oneshot(json_post("/s1/acp", body.clone())).await.unwrap();
        assert_eq!(first.status(), StatusCode::CREATED);

        let second = sessions_router(engine.clone()).oneshot(json_post("/s1/acp", body)).await.unwrap();
        assert_eq!(second.status(), StatusCode::BAD_GATEWAY);
    }

    /// Regression test for `CreateAcpSessionRequest::harness` actually being
    /// threaded through to `AcpBackend::create_session`'s per-harness
    /// dispatch (not just ignored, as it effectively was before this field
    /// existed): two sessions created against the same server with
    /// different `harness` values must each spawn *their own* registered
    /// fake agent, distinguishable by the pid each one logs.
    #[tokio::test]
    async fn create_acp_session_dispatches_the_agent_command_by_harness() {
        let store = Arc::new(Store::open(tempdir().unwrap().keep().join("t.db")).unwrap());
        let codex_pid_log = tempdir().unwrap().keep().join("codex-pids");
        let pi_pid_log = tempdir().unwrap().keep().join("pi-pids");
        let mut commands = std::collections::HashMap::new();
        commands.insert("codex".to_string(), fake_agent_command(&codex_pid_log));
        commands.insert("pi".to_string(), fake_agent_command(&pi_pid_log));
        let acp = Arc::new(AcpBackend::with_agent_commands(commands));
        let engine = Engine::new_with_acp(store, acp);
        let workspace = tempdir().unwrap().keep();

        for (id, harness, pid_log) in [("s-codex", "codex", &codex_pid_log), ("s-pi", "pi", &pi_pid_log)] {
            let create = sessions_router(engine.clone())
                .oneshot(json_post(
                    &format!("/{id}/acp"),
                    serde_json::json!({ "workspace": workspace.to_str().unwrap(), "harness": harness }),
                ))
                .await
                .unwrap();
            assert_eq!(create.status(), StatusCode::CREATED, "{harness} session must spawn successfully");
            assert_eq!(
                std::fs::read_to_string(pid_log).unwrap().lines().count(),
                1,
                "{harness}'s own registered agent command must have been the one spawned"
            );
        }
    }

    fn test_engine() -> Arc<Engine> {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.db");
        let store = Arc::new(Store::open(&path).unwrap());
        // keep dir alive so the temp directory isn't removed before the test ends
        std::mem::forget(dir);
        Engine::new(store)
    }

    #[tokio::test]
    async fn list_empty() {
        let app = sessions_router(test_engine());
        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let sessions: Vec<Session> = serde_json::from_slice(&body).unwrap();
        assert!(sessions.is_empty());
    }

    #[tokio::test]
    async fn list_returns_stored() {
        let engine = test_engine();
        engine
            .store
            .upsert_session(&Session {
                id: "s1".into(),
                orchestrator_id: None,
                name: "w".into(),
                repo: "r".into(),
                status: SessionStatus::Working,
                agent_type: "c".into(),
                cost_usd: 0.0,
                started_at: 0,
                pr_number: None,
                pr_id: None,
                workspace_path: None,
                pid: None,
                model: None,
                context_tokens: None,
                brain_reads: Vec::new(), brain_write: None,
                context_used_pct: None,
                context_total_tokens: None,
                context_window_size: None,
                claude_session_id: None,
                summary: None,
                terminal_at: None, gate_status: None, merged_at: None,
                activity: Default::default(), activity_note: None, activity_since: None,
                machine_id: None,
                worker_type: None,
                success_criteria: Vec::new(),
                completion_outcome: None,
                self_completed_at: None,
                cleanup_result: None, cleanup_at: None,
            })
            .unwrap();
        let app = sessions_router(engine);
        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let sessions: Vec<Session> = serde_json::from_slice(&body).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "s1");
    }

    #[tokio::test]
    async fn diff_returns_404_for_unknown_session() {
        let response = sessions_router(test_engine())
            .oneshot(Request::builder().uri("/nope/diff").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn diff_is_none_without_a_recorded_workspace() {
        let engine = test_engine();
        engine
            .store
            .upsert_session(&Session {
                id: "s1".into(),
                orchestrator_id: None,
                name: "w".into(),
                repo: "r".into(),
                status: SessionStatus::Working,
                agent_type: "c".into(),
                cost_usd: 0.0,
                started_at: 0,
                pr_number: None,
                pr_id: None,
                workspace_path: None,
                pid: None,
                model: None,
                context_tokens: None,
                brain_reads: Vec::new(), brain_write: None,
                context_used_pct: None,
                context_total_tokens: None,
                context_window_size: None,
                claude_session_id: None,
                summary: None,
                terminal_at: None, gate_status: None, merged_at: None,
                activity: Default::default(), activity_note: None, activity_since: None,
                machine_id: None,
                worker_type: None,
                success_criteria: Vec::new(),
                completion_outcome: None,
                self_completed_at: None,
                cleanup_result: None, cleanup_at: None,
            })
            .unwrap();
        let response = sessions_router(engine)
            .oneshot(Request::builder().uri("/s1/diff").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["diff"].is_null());
    }

    #[tokio::test]
    async fn diff_returns_workspace_diff_including_lockfile_only_changes() {
        // Real git repo — this route must not apply brain-harvest's
        // lockfile-only filtering, unlike `compute_nontrivial_diff`.
        let repo = tempdir().unwrap().keep();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", repo.to_str().unwrap()])
                .args(args)
                .output()
                .unwrap()
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        std::fs::write(repo.join("README.md"), "x").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);
        run(&["checkout", "-q", "-b", "feature"]);
        std::fs::write(repo.join("Cargo.lock"), "version = 3\n").unwrap();
        run(&["add", "Cargo.lock"]);
        run(&["commit", "-q", "-m", "bump lockfile"]);

        let engine = test_engine();
        engine
            .store
            .upsert_session(&Session {
                id: "s1".into(),
                orchestrator_id: None,
                name: "w".into(),
                repo: "r".into(),
                status: SessionStatus::Working,
                agent_type: "c".into(),
                cost_usd: 0.0,
                started_at: 0,
                pr_number: None,
                pr_id: None,
                workspace_path: Some(repo.to_str().unwrap().to_string()),
                pid: None,
                model: None,
                context_tokens: None,
                brain_reads: Vec::new(), brain_write: None,
                context_used_pct: None,
                context_total_tokens: None,
                context_window_size: None,
                claude_session_id: None,
                summary: None,
                terminal_at: None, gate_status: None, merged_at: None,
                activity: Default::default(), activity_note: None, activity_since: None,
                machine_id: None,
                worker_type: None,
                success_criteria: Vec::new(),
                completion_outcome: None,
                self_completed_at: None,
                cleanup_result: None, cleanup_at: None,
            })
            .unwrap();
        let response = sessions_router(engine)
            .oneshot(Request::builder().uri("/s1/diff").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["diff"].as_str().unwrap().contains("Cargo.lock"));
    }

    fn remote_session(id: &str, workspace: &str, machine_id: &str) -> Session {
        Session {
            id: id.into(),
            orchestrator_id: None,
            name: "w".into(),
            repo: "r".into(),
            status: SessionStatus::Working,
            agent_type: "c".into(),
            cost_usd: 0.0,
            started_at: 0,
            pr_number: None,
            pr_id: None,
            workspace_path: Some(workspace.into()),
            pid: None,
            model: None,
            context_tokens: None,
            brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None,
            context_total_tokens: None,
            context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: Some(machine_id.into()),
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        }
    }

    fn machine_profile(id: &str, ssh_target: &str) -> ninox_core::config::MachineProfile {
        ninox_core::config::MachineProfile {
            id: id.into(),
            label: id.into(),
            ssh_target: ssh_target.into(),
            remote_session: "default".into(),
            enabled: true,
        }
    }

    /// Records every call it receives instead of touching a real `ssh`
    /// binary — this is the mock referenced by the routing/error tests
    /// below, so they never require a reachable remote host in CI.
    struct FakeRemoteDiff {
        calls: std::sync::Mutex<Vec<(String, String, String)>>,
        result: Result<Option<String>, String>,
    }

    impl FakeRemoteDiff {
        fn returning(result: Result<Option<String>, String>) -> Self {
            Self { calls: std::sync::Mutex::new(Vec::new()), result }
        }
    }

    impl RemoteDiff for FakeRemoteDiff {
        fn diff(&self, machine_id: &str, ssh_target: &str, workspace: &str) -> Result<Option<String>, String> {
            self.calls.lock().unwrap().push((machine_id.into(), ssh_target.into(), workspace.into()));
            self.result.clone()
        }
    }

    /// A `RemoteDiff` that panics if called — used to prove the local
    /// (`machine_id: None`) path never touches the remote transport at all.
    struct PanicsIfCalledRemoteDiff;

    impl RemoteDiff for PanicsIfCalledRemoteDiff {
        fn diff(&self, _machine_id: &str, _ssh_target: &str, _workspace: &str) -> Result<Option<String>, String> {
            panic!("a local session's diff must never call the remote transport");
        }
    }

    #[tokio::test]
    async fn diff_for_a_remote_session_routes_through_ssh_via_the_matching_machine_profile() {
        let session = remote_session("s1", "/remote/ws", "m1");
        let remote_machines = RemoteMachinesConfig {
            enabled: true,
            machines: vec![machine_profile("m1", "user@remote-host")],
        };
        let fake = Arc::new(FakeRemoteDiff::returning(Ok(Some("diff text".into()))));

        let diff = diff_for_session(&session, "/remote/ws", &remote_machines, fake.clone())
            .await
            .expect("remote diff must succeed");

        assert_eq!(diff.as_deref(), Some("diff text"));
        let calls = fake.calls.lock().unwrap();
        assert_eq!(*calls, vec![("m1".to_string(), "user@remote-host".to_string(), "/remote/ws".to_string())]);
    }

    #[tokio::test]
    async fn diff_for_a_local_session_is_unaffected_and_never_touches_remote_transport() {
        // Real git repo, mirroring `diff_returns_workspace_diff_including_lockfile_only_changes`
        // above — proves the local path's actual diff output, byte-for-byte,
        // is unchanged by the remote-routing addition.
        let repo = tempdir().unwrap().keep();
        let run = |args: &[&str]| {
            std::process::Command::new("git").args(["-C", repo.to_str().unwrap()]).args(args).output().unwrap()
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        std::fs::write(repo.join("README.md"), "x").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);
        run(&["checkout", "-q", "-b", "feature"]);
        std::fs::write(repo.join("README.md"), "y").unwrap();
        run(&["commit", "-q", "-am", "edit"]);

        let workspace = repo.to_str().unwrap().to_string();
        let mut session = remote_session("s1", &workspace, "unused");
        session.machine_id = None;

        let direct = brain_harvest::compute_diff(&repo).await;
        let via_route = diff_for_session(
            &session,
            &workspace,
            &RemoteMachinesConfig::default(),
            Arc::new(PanicsIfCalledRemoteDiff),
        )
        .await
        .expect("local diff must succeed");

        assert_eq!(via_route, direct, "local diff must be byte-for-byte identical to compute_diff");
        assert!(via_route.as_deref().unwrap().contains("README.md"));
    }

    #[tokio::test]
    async fn diff_for_a_remote_session_surfaces_a_failure_as_a_specific_error_not_an_empty_diff() {
        let session = remote_session("s1", "/remote/ws", "m1");
        let remote_machines = RemoteMachinesConfig {
            enabled: true,
            machines: vec![machine_profile("m1", "user@remote-host")],
        };
        let fake = Arc::new(FakeRemoteDiff::returning(Err(
            "remote git diff failed on user@remote-host: bash: git: command not found".into(),
        )));

        let err = diff_for_session(&session, "/remote/ws", &remote_machines, fake)
            .await
            .expect_err("a remote failure must be a distinct error, never Ok(None)");

        assert!(err.contains("git: command not found"), "error must carry the specific remote failure: {err}");
    }

    #[tokio::test]
    async fn diff_for_a_remote_session_with_no_matching_machine_profile_is_a_clear_error() {
        let session = remote_session("s1", "/remote/ws", "missing-machine");
        let fake = Arc::new(FakeRemoteDiff::returning(Ok(Some("unused".into()))));

        let err = diff_for_session(&session, "/remote/ws", &RemoteMachinesConfig::default(), fake)
            .await
            .expect_err("an unconfigured machine_id must error rather than silently diffing nothing");

        assert!(err.contains("missing-machine"));
    }

    #[tokio::test]
    async fn delete_returns_no_content() {
        let engine = test_engine();
        engine
            .store
            .upsert_session(&Session {
                id: "s1".into(),
                orchestrator_id: None,
                name: "w".into(),
                repo: "r".into(),
                status: SessionStatus::Working,
                agent_type: "c".into(),
                cost_usd: 0.0,
                started_at: 0,
                pr_number: None,
                pr_id: None,
                workspace_path: None,
                pid: None,
                model: None,
                context_tokens: None,
                brain_reads: Vec::new(), brain_write: None,
                context_used_pct: None,
                context_total_tokens: None,
                context_window_size: None,
                claude_session_id: None,
                summary: None,
                terminal_at: None, gate_status: None, merged_at: None,
                activity: Default::default(), activity_note: None, activity_since: None,
                machine_id: None,
                worker_type: None,
                success_criteria: Vec::new(),
                completion_outcome: None,
                self_completed_at: None,
                cleanup_result: None, cleanup_at: None,
            })
            .unwrap();
        let response = sessions_router(engine)
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/s1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }
}
