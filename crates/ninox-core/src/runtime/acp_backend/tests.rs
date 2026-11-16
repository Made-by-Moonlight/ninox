//! `AcpBackend`'s own test double: a minimal in-memory agent that speaks
//! just enough ACP JSON-RPC over a duplex pipe to validate this backend's
//! side of the protocol, with no real `opencode` binary involved (per the
//! design doc's "Testing" section) — plus one smoke test against the real
//! binary when it happens to be on `PATH`, covering the handshake only (no
//! `session/prompt`, so it never calls out to a real model/API).

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

use super::*;

async fn write_line(writer: &mut (impl AsyncWrite + Unpin), value: &Value) {
    let mut line = serde_json::to_string(value).unwrap();
    line.push('\n');
    writer.write_all(line.as_bytes()).await.unwrap();
    writer.flush().await.unwrap();
}

async fn reply(writer: &mut (impl AsyncWrite + Unpin), id: Option<Value>, result: Value) {
    let Some(id) = id else { return };
    write_line(writer, &json!({"jsonrpc": "2.0", "id": id, "result": result})).await;
}

async fn notify(writer: &mut (impl AsyncWrite + Unpin), method: &str, params: Value) {
    write_line(writer, &json!({"jsonrpc": "2.0", "method": method, "params": params})).await;
}

/// Replies to `initialize`/`session/new` immediately; on `session/prompt`,
/// emits one `tool_call` update before answering, after an optional delay
/// (so tests can observe a turn that is still in progress).
async fn run_fake_agent(reader: impl AsyncRead + Unpin, mut writer: impl AsyncWrite + Unpin, prompt_delay: Duration) {
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or_default();
        let id = msg.get("id").cloned();
        match method {
            "initialize" => {
                reply(&mut writer, id, json!({"protocolVersion": 1, "agentCapabilities": {}, "authMethods": []})).await
            }
            "session/new" => reply(&mut writer, id, json!({"sessionId": "fake-session-1"})).await,
            "session/prompt" => {
                tokio::time::sleep(prompt_delay).await;
                notify(
                    &mut writer,
                    "session/update",
                    json!({
                        "sessionId": "fake-session-1",
                        "update": {
                            "sessionUpdate": "tool_call",
                            "toolCallId": "call_1",
                            "title": "Running tests",
                            "name": "bash",
                            "status": "completed",
                            "content": [{"type": "content", "content": {"type": "text", "text": "3 passed"}}],
                        },
                    }),
                )
                .await;
                reply(&mut writer, id, json!({"stopReason": "end_turn"})).await;
            }
            "session/cancel" => {}
            _ => {
                if let Some(id) = id {
                    write_line(
                        &mut writer,
                        &json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "method not found"}}),
                    )
                    .await;
                }
            }
        }
    }
}

/// Wires `backend` to a fresh fake-agent session with id `id`, bypassing
/// `create_session`'s real subprocess spawn entirely.
async fn wire_fake_session(backend: &AcpBackend, id: &str, prompt_delay: Duration) {
    let (client_io, agent_io) = tokio::io::duplex(8192);
    let (agent_read, agent_write) = tokio::io::split(agent_io);
    let (client_read, client_write) = tokio::io::split(client_io);

    tokio::spawn(run_fake_agent(agent_read, agent_write, prompt_delay));

    let session =
        AcpSession::spawn_over(0, now_ms(), None, Box::pin(client_write) as DynWriter, Box::pin(client_read) as DynReader)
            .await;
    handshake(&session, "/tmp").await.expect("fake agent handshake");
    backend.sessions.lock().await.insert(id.to_string(), SessionSlot::Ready(session));
}

#[test]
fn kind_is_acp() {
    assert_eq!(AcpBackend::new().kind(), Backend::Acp);
}

#[test]
fn default_agent_commands_cover_every_tier2_harness_per_the_spec() {
    let commands = default_agent_commands();
    assert_eq!(commands.get("opencode"), Some(&vec!["opencode".to_string(), "acp".to_string()]));
    assert_eq!(
        commands.get("codex"),
        Some(&vec!["npx".to_string(), "-y".to_string(), "@agentclientprotocol/codex-acp".to_string()])
    );
    assert_eq!(
        commands.get("pi"),
        Some(&vec!["npx".to_string(), "-y".to_string(), "pi-acp@0.0.34".to_string()]),
        "pi's adapter must stay pinned at the exact version the design spec resolved on"
    );
    assert!(!commands.contains_key("antigravity"), "antigravity is permanently deferred, not Tier 2");
}

#[test]
fn command_for_defaults_empty_harness_to_opencode() {
    let backend = AcpBackend::new();
    assert_eq!(
        backend.command_for("").unwrap(),
        vec!["opencode".to_string(), "acp".to_string()],
        "an empty harness name must preserve every pre-existing call site's exact behavior"
    );
}

#[test]
fn command_for_an_unregistered_harness_errors_cleanly() {
    let backend = AcpBackend::new();
    let err = backend.command_for("antigravity").unwrap_err();
    assert!(err.to_string().contains("antigravity"), "the error should name the harness it couldn't find: {err}");
}

#[test]
fn command_for_an_override_backend_ignores_the_harness_argument() {
    let backend = AcpBackend::with_agent_command(vec!["fake-agent".to_string()]);
    assert_eq!(backend.command_for("codex").unwrap(), vec!["fake-agent".to_string()]);
    assert_eq!(backend.command_for("whatever-unregistered").unwrap(), vec!["fake-agent".to_string()]);
}

/// Regression test for `create_session` actually dispatching by harness
/// name rather than merely having a lookup table that's never consulted:
/// two harnesses registered via `with_agent_commands` must each spawn
/// *their own* real (if fake) subprocess, distinguishable by pid.
#[tokio::test]
async fn create_session_spawns_the_registered_command_for_the_given_harness() {
    fn tagging_script(pid_log: &std::path::Path) -> Vec<String> {
        let script = format!(
            r#"
echo $$ >> {pid_log}
read -r _init
printf '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":1,"agentCapabilities":{{}},"authMethods":[]}}}}\n'
read -r _new
printf '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"fake-session-1"}}}}\n'
sleep 9999
"#,
            pid_log = pid_log.display(),
        );
        vec!["sh".to_string(), "-c".to_string(), script]
    }

    let codex_pid_log = tempfile::tempdir().unwrap().keep().join("codex-pids");
    let pi_pid_log = tempfile::tempdir().unwrap().keep().join("pi-pids");
    let mut commands = HashMap::new();
    commands.insert("codex".to_string(), tagging_script(&codex_pid_log));
    commands.insert("pi".to_string(), tagging_script(&pi_pid_log));
    let backend = AcpBackend::with_agent_commands(commands);
    let workspace = tempfile::tempdir().unwrap();

    backend.create_session("s-codex", workspace.path().to_str().unwrap(), "codex", &[]).await.unwrap();
    backend.create_session("s-pi", workspace.path().to_str().unwrap(), "pi", &[]).await.unwrap();

    assert_eq!(std::fs::read_to_string(&codex_pid_log).unwrap().lines().count(), 1);
    assert_eq!(std::fs::read_to_string(&pi_pid_log).unwrap().lines().count(), 1);

    backend.kill_session("s-codex").await.unwrap();
    backend.kill_session("s-pi").await.unwrap();
}

#[tokio::test]
async fn create_session_for_an_unregistered_harness_never_spawns_a_process() {
    let backend = AcpBackend::new();
    let workspace = tempfile::tempdir().unwrap();
    let err = backend
        .create_session("s1", workspace.path().to_str().unwrap(), "antigravity", &[])
        .await
        .expect_err("antigravity has no registered ACP agent command");
    assert!(err.to_string().contains("antigravity"));
    // The failed reservation must not leave the id claimed behind.
    assert!(!backend.knows("s1").await);
}

#[tokio::test]
async fn unknown_session_reports_honestly_empty() {
    let backend = AcpBackend::new();
    assert!(!backend.knows("nope").await);
    assert!(!backend.has_session("nope").await);
    assert_eq!(backend.session_pid("nope").await, None);
    assert!(backend.attach_args("nope").await.is_empty());
    assert_eq!(backend.history_size("nope").await, 0);
    assert!(backend.capture_history("nope", 0, 10).await.is_empty());
    assert!(backend.transcript_history("nope").await.is_empty());
    assert!(!backend.wait_for_input_prompt("nope", Duration::from_millis(1)).await);
}

/// Regression test for the `create_session` TOCTOU race: a concurrent
/// second call for the same id — represented here directly by inserting the
/// `Reserved` slot a real `create_session` would hold during its spawn +
/// handshake — must fail fast instead of racing to spawn a second agent
/// process whose `AcpSession` (and live subprocess) would then have nothing
/// left to tear it down once the winner's insert replaced the map entry.
#[tokio::test]
async fn create_session_fails_fast_against_a_reservation_in_flight() {
    let backend = AcpBackend::new();
    backend.sessions.lock().await.insert("s1".to_string(), SessionSlot::Reserved);

    let err = backend
        .create_session("s1", "/tmp", "", &[])
        .await
        .expect_err("a reserved id must refuse a concurrent create, not spawn a second agent");
    assert!(err.to_string().contains("already exists"));

    // A reservation in flight is "known" (claims the id) but has nothing
    // ready yet — every session-level accessor reports the same honest
    // empty/absent answer as a genuinely unknown id.
    assert!(backend.knows("s1").await);
    assert!(!backend.has_session("s1").await);
    assert_eq!(backend.session_pid("s1").await, None);
    assert!(backend.transcript_history("s1").await.is_empty());
    assert!(backend.list_sessions().await.unwrap().is_empty());
}

#[tokio::test]
async fn handshake_and_prompt_round_trip_records_an_ordered_transcript() {
    let backend = AcpBackend::new();
    wire_fake_session(&backend, "s1", Duration::ZERO).await;

    assert!(backend.knows("s1").await);
    assert!(backend.has_session("s1").await);

    backend.send_keys("s1", "please run the tests").await.expect("prompt turn should succeed");

    let transcript = backend.transcript_history("s1").await;
    assert_eq!(
        transcript,
        vec![
            TranscriptEntry::UserMessage("please run the tests".to_string()),
            TranscriptEntry::TurnStarted,
            TranscriptEntry::ToolCall {
                id:     "call_1".to_string(),
                name:   "bash".to_string(),
                status: ToolCallStatus::Completed,
                detail: "3 passed".to_string(),
            },
            TranscriptEntry::TurnEnded,
        ]
    );

    let screen = backend.read_screen("s1", 0, false).await.unwrap();
    assert!(screen.contains("[tool:bash]"), "rendered transcript should mention the tool call: {screen}");
}

/// Regression test for the `kill_session` vs. in-flight `create_session`
/// race: unlike `create_session_fails_fast_against_a_reservation_in_flight`
/// above (which simulates the `Reserved` state by hand), this drives a real
/// `create_session` call through a real (if fake) subprocess via
/// `with_agent_command`, so the reservation window is genuine — the spawn
/// has actually happened and the handshake is actually in flight — and
/// races a real `kill_session` call against it on separate tasks under a
/// multi-threaded runtime, rather than simulating the interleaving.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kill_session_racing_an_in_flight_create_session_does_not_resurrect_it() {
    // A real `sh` subprocess that only answers `initialize` after a
    // deliberate delay, giving this test a real window where the session is
    // `Reserved` (subprocess already spawned) but not yet `Ready`.
    let script = r#"
read -r _init
sleep 2
printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{},"authMethods":[]}}\n'
read -r _new
printf '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"fake-session-1"}}\n'
sleep 2
"#;
    let backend =
        Arc::new(AcpBackend::with_agent_command(vec!["sh".to_string(), "-c".to_string(), script.to_string()]));
    let workspace = tempfile::tempdir().unwrap();
    let workspace_path = workspace.path().to_str().unwrap().to_string();

    let b2 = backend.clone();
    let create = tokio::spawn(async move { b2.create_session("s1", &workspace_path, "", &[]).await });

    // Let create_session reserve the id and spawn the (now-sleeping)
    // subprocess before racing a kill against it.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(backend.knows("s1").await, "reservation should be visible while the handshake is in flight");

    backend.kill_session("s1").await.unwrap();

    let result = create.await.unwrap();
    assert!(result.is_err(), "a kill racing a reservation must make create_session fail, not resurrect the session");

    assert!(!backend.knows("s1").await, "a killed-while-starting session must not be left behind");
    assert!(!backend.has_session("s1").await);
}

#[tokio::test]
async fn live_streaming_emits_session_transcript_events() {
    let store = Arc::new(crate::store::Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
    let engine = Engine::new(store);
    let mut rx = engine.subscribe();

    let backend = AcpBackend::new();
    wire_fake_session(&backend, "s1", Duration::ZERO).await;
    backend.start_streaming(engine.clone(), "s1".to_string(), "s1").await.unwrap();

    backend.send_keys("s1", "go").await.unwrap();

    let mut saw_tool_call = false;
    while let Ok(event) = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.unwrap() {
        if let Event::SessionTranscript { session_id, entry: TranscriptEntry::ToolCall { name, .. } } = event {
            assert_eq!(session_id, "s1");
            assert_eq!(name, "bash");
            saw_tool_call = true;
            break;
        }
    }
    assert!(saw_tool_call, "expected a live Event::SessionTranscript for the tool call");
}

/// Regression test for the `start_streaming` generation race: a second
/// `start_streaming` call for the same session id cancels the first
/// registration's `cancel_rx` (`Engine::register_stream`'s own contract),
/// which deterministically triggers the first call's now-stale cleanup
/// task. Without the generation guard that task would unconditionally null
/// out the second call's `live_emit`, silently killing live delivery for
/// whoever made the second (e.g. reconnecting) call.
#[tokio::test]
async fn start_streaming_generation_survives_a_superseded_cleanup_race() {
    let store = Arc::new(crate::store::Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
    let engine = Engine::new(store);
    let mut rx = engine.subscribe();

    let backend = AcpBackend::new();
    wire_fake_session(&backend, "s1", Duration::ZERO).await;

    backend.start_streaming(engine.clone(), "s1".to_string(), "s1").await.unwrap();
    backend.start_streaming(engine.clone(), "s1".to_string(), "s1").await.unwrap();

    // Give the first call's now-superseded cleanup task a chance to run.
    tokio::time::sleep(Duration::from_millis(20)).await;

    backend.send_keys("s1", "go").await.unwrap();
    let event = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.unwrap().unwrap();
    assert!(
        matches!(event, Event::SessionTranscript { .. }),
        "live_emit must survive the superseded cleanup race, got {event:?}"
    );
}

/// Regression test for the *other* half of the `start_streaming` race: the
/// test above calls the two `start_streaming` invocations strictly
/// sequentially (`.await`ed one at a time), which can only catch a stale
/// cleanup task racing a *later* write. It can't catch the reverse
/// interleaving — two calls whose own `register_stream` + generation-stamp +
/// write sequences interleave with each other — because sequential awaiting
/// forces a total order. This drives both calls as separate tasks on a
/// multi-threaded runtime with no ordering between them, so the two
/// sequences can genuinely interleave.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn start_streaming_survives_two_genuinely_concurrent_calls() {
    let store = Arc::new(crate::store::Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
    let engine = Engine::new(store);

    let backend = Arc::new(AcpBackend::new());
    wire_fake_session(&backend, "s1", Duration::ZERO).await;

    let (b1, b2) = (backend.clone(), backend.clone());
    let (e1, e2) = (engine.clone(), engine.clone());
    let (r1, r2) = tokio::join!(
        tokio::spawn(async move { b1.start_streaming(e1, "s1".to_string(), "s1").await }),
        tokio::spawn(async move { b2.start_streaming(e2, "s1".to_string(), "s1").await }),
    );
    r1.unwrap().unwrap();
    r2.unwrap().unwrap();

    // Whichever call "won", live streaming must still be active — the race
    // this guards against can null out *both* subscribers' entries when it
    // fires, not just the loser's.
    let mut rx = engine.subscribe();
    backend.send_keys("s1", "go").await.unwrap();
    let event = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.unwrap().unwrap();
    assert!(
        matches!(event, Event::SessionTranscript { .. }),
        "live streaming must survive two genuinely concurrent start_streaming calls, got {event:?}"
    );
}

#[tokio::test]
async fn ctrl_c_byte_sends_session_cancel_instead_of_a_prompt_turn() {
    let backend = AcpBackend::new();
    wire_fake_session(&backend, "s1", Duration::ZERO).await;

    backend.write_input("s1", &[0x03]).await.expect("cancel notification should send cleanly");

    // A cancel is a notification, not a turn — nothing should land in the
    // transcript the way a real prompt would.
    assert!(backend.transcript_history("s1").await.is_empty());
}

#[tokio::test]
async fn wait_for_input_prompt_blocks_until_the_turn_ends() {
    let backend = Arc::new(AcpBackend::new());
    wire_fake_session(&backend, "s1", Duration::from_millis(150)).await;

    assert!(
        backend.wait_for_input_prompt("s1", Duration::from_millis(10)).await,
        "idle before any turn starts"
    );

    let b2 = backend.clone();
    let prompt = tokio::spawn(async move { b2.send_keys("s1", "hi").await });
    tokio::time::sleep(Duration::from_millis(30)).await; // let the turn actually start

    assert!(
        !backend.wait_for_input_prompt("s1", Duration::from_millis(10)).await,
        "should still be mid-turn"
    );
    assert!(
        backend.wait_for_input_prompt("s1", Duration::from_secs(2)).await,
        "should become idle once the turn ends"
    );
    prompt.await.unwrap().unwrap();
}

/// Regression test for a bug introduced alongside `ninox-server`'s
/// `send_acp_message` route no longer blocking its HTTP response for a
/// whole turn: once a caller can issue a second send while the first's
/// turn is still running (an ordinary pattern, not a misuse), ACP has no
/// notion of two concurrent `session/prompt` turns on one session —
/// `prompt_turn`'s `turn_lock` must serialize entry so the second send
/// queues behind the first rather than racing it over the wire. Without
/// that lock this test's transcript would show the second turn's
/// `TurnStarted` landing before the first's `TurnEnded`.
#[tokio::test]
async fn two_sends_issued_while_the_first_turn_is_in_flight_are_serialized_not_raced() {
    let backend = Arc::new(AcpBackend::new());
    wire_fake_session(&backend, "s1", Duration::from_millis(150)).await;

    let b2 = backend.clone();
    let first = tokio::spawn(async move { b2.send_keys("s1", "first").await });
    tokio::time::sleep(Duration::from_millis(30)).await; // let the first turn actually start

    // Issued while the first turn is still mid-flight — must queue behind
    // it, not run concurrently against the same connection.
    backend.send_keys("s1", "second").await.expect("second turn should succeed once queued");
    first.await.unwrap().expect("first turn should succeed");

    let transcript = backend.transcript_history("s1").await;
    let starts: Vec<usize> = transcript
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e, TranscriptEntry::TurnStarted))
        .map(|(i, _)| i)
        .collect();
    let ends: Vec<usize> = transcript
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e, TranscriptEntry::TurnEnded))
        .map(|(i, _)| i)
        .collect();
    assert_eq!((starts.len(), ends.len()), (2, 2), "both turns must leave a complete record: {transcript:?}");
    assert!(starts[0] < ends[0] && starts[1] < ends[1], "each turn's start must precede its own end: {transcript:?}");
    assert!(ends[0] < starts[1], "the second turn must not start until the first ends: {transcript:?}");
}

/// Regression test for `teardown` aborting the reader task before it can
/// reach its own EOF-driven pending-request drain: without draining
/// `pending` directly in `teardown`, a `kill_session` racing an in-flight
/// turn would strand that turn's `call()` — and the `turn_lock` it holds —
/// until its own `PROMPT_TIMEOUT` (600s) fired, with no reply and no abort
/// signal ever reaching it. The fake agent here never answers the prompt at
/// all, so the only way this can resolve quickly is via that direct drain;
/// bounding the wait at 5s (not 600s) makes a regression fail fast rather
/// than hang the test suite.
#[tokio::test]
async fn killing_a_session_resolves_an_in_flight_turn_promptly_not_after_prompt_timeout() {
    let backend = Arc::new(AcpBackend::new());
    wire_fake_session(&backend, "s1", Duration::from_secs(9999)).await;

    let b2 = backend.clone();
    let prompt = tokio::spawn(async move { b2.send_keys("s1", "hi").await });
    tokio::time::sleep(Duration::from_millis(50)).await; // let the turn actually start and take turn_lock

    backend.kill_session("s1").await.unwrap();

    let result = tokio::time::timeout(Duration::from_secs(5), prompt)
        .await
        .expect("killing the session must resolve the in-flight turn promptly, not strand it for PROMPT_TIMEOUT")
        .unwrap();
    assert!(result.is_err(), "a turn whose connection was killed mid-flight must surface as an error, not success");
}

/// Regression test for the window `torn_down` closes: once `teardown`'s
/// forced pending-drain fails the in-flight turn (A) and releases
/// `turn_lock`, a second turn (B) that was queued behind it can otherwise
/// acquire the lock before `teardown` finishes killing the child — writing
/// to the still-open connection would succeed while the reader task
/// (already aborted) will never read a reply, hanging B for the full
/// `PROMPT_TIMEOUT` and continuing to hold `turn_lock`. `torn_down` is set
/// as `teardown`'s very first action, so `prompt_turn` can observe it right
/// after acquiring the lock and fail fast instead. Bounding both waits at 5s
/// makes a regression fail fast rather than hang the test suite.
#[tokio::test]
async fn a_turn_queued_behind_one_a_kill_interrupts_fails_fast_not_after_prompt_timeout() {
    let backend = Arc::new(AcpBackend::new());
    // Neither turn ever gets a real reply — the only way either resolves is
    // via `teardown`'s own forced-failure paths.
    wire_fake_session(&backend, "s1", Duration::from_secs(9999)).await;

    let b1 = backend.clone();
    let first = tokio::spawn(async move { b1.send_keys("s1", "first").await });
    tokio::time::sleep(Duration::from_millis(50)).await; // let the first turn take turn_lock

    let b2 = backend.clone();
    let second = tokio::spawn(async move { b2.send_keys("s1", "second").await });
    tokio::time::sleep(Duration::from_millis(50)).await; // let the second turn queue on turn_lock

    backend.kill_session("s1").await.unwrap();

    let first_result = tokio::time::timeout(Duration::from_secs(5), first)
        .await
        .expect("the in-flight turn must resolve promptly on kill")
        .unwrap();
    assert!(first_result.is_err());

    let second_result = tokio::time::timeout(Duration::from_secs(5), second)
        .await
        .expect("a turn queued behind a killed session must fail fast, not hang for PROMPT_TIMEOUT")
        .unwrap();
    assert!(second_result.is_err(), "a queued turn against a killed session must surface as an error, not hang or succeed");
}

#[tokio::test]
async fn kill_session_is_idempotent_and_frees_the_id() {
    let backend = AcpBackend::new();
    wire_fake_session(&backend, "s1", Duration::ZERO).await;

    backend.kill_session("s1").await.unwrap();
    assert!(!backend.knows("s1").await);
    // Succeeds again on an id that no longer exists.
    backend.kill_session("s1").await.unwrap();
}

#[tokio::test]
async fn current_pane_identity_is_honestly_none_without_the_env_var() {
    // SAFETY: test-only env mutation, no other test in this process reads
    // or sets this specific var concurrently.
    unsafe { std::env::remove_var(PANE_ID_ENV) };
    assert_eq!(AcpBackend::new().current_pane_identity().await.unwrap(), None);
}

#[tokio::test]
async fn unrecognized_agent_request_gets_a_method_not_found_reply_not_a_hang() {
    let (client_io, agent_io) = tokio::io::duplex(8192);
    let (agent_read, agent_write) = tokio::io::split(agent_io);
    let (client_read, client_write) = tokio::io::split(client_io);
    let mut agent_lines = BufReader::new(agent_read).lines();
    let mut agent_writer = agent_write;

    let session =
        AcpSession::spawn_over(0, now_ms(), None, Box::pin(client_write) as DynWriter, Box::pin(client_read) as DynReader)
            .await;

    let call_task = tokio::spawn(async move { session.call("initialize", json!({}), Duration::from_secs(5)).await });

    let initialize_line = agent_lines.next_line().await.unwrap().unwrap();
    let initialize_request: Value = serde_json::from_str(&initialize_line).unwrap();
    assert_eq!(initialize_request["id"], 1);

    // A method this backend never implements, sent *instead of* a reply —
    // it must be answered immediately, or `call_task` would hang forever.
    write_line(&mut agent_writer, &json!({"jsonrpc": "2.0", "id": 999, "method": "fs/read_text_file", "params": {}})).await;

    let error_line = tokio::time::timeout(Duration::from_secs(5), agent_lines.next_line())
        .await
        .expect("client must answer the unrecognized request instead of hanging")
        .unwrap()
        .unwrap();
    let error_reply: Value = serde_json::from_str(&error_line).unwrap();
    assert_eq!(error_reply["id"], 999);
    assert_eq!(error_reply["error"]["code"], -32601);

    write_line(&mut agent_writer, &json!({"jsonrpc": "2.0", "id": 1, "result": {"protocolVersion": 1}})).await;
    let result = call_task.await.unwrap().unwrap();
    assert_eq!(result["protocolVersion"], 1);
}

/// Real-binary smoke test: skips (rather than fails) when `opencode` isn't
/// on `PATH`, so CI without it still passes — every protocol-correctness
/// assertion above already runs against the fake agent instead. Covers the
/// handshake only, deliberately never calling `session/prompt`, so this
/// never depends on a configured model/API key or makes a real network call.
/// Also skips (rather than hangs) if the real binary doesn't finish the
/// handshake within a short bound — a shared/loaded machine can make a real
/// subprocess arbitrarily slow to start, and this test exists for extra
/// confidence, not as a required signal (that's the fake-agent tests above).
///
/// `#[ignore]`d so a plain `cargo test` stays fast and deterministic even on
/// a machine that happens to have `opencode` installed — run explicitly with
/// `cargo test -- --ignored real_opencode_binary` when you want this signal.
#[tokio::test]
#[ignore = "spawns the real opencode binary if present; run explicitly with `cargo test -- --ignored`"]
async fn real_opencode_binary_completes_the_acp_handshake_if_available() {
    const NAME: &str = "real_opencode_binary_completes_the_acp_handshake_if_available";

    if std::process::Command::new("opencode").arg("--version").output().is_err() {
        eprintln!("skipping {NAME}: opencode not on PATH");
        return;
    }

    let outcome = tokio::time::timeout(Duration::from_secs(20), async {
        let backend = AcpBackend::new();
        let workspace = tempfile::tempdir().unwrap();
        let id = "acp-real-smoke-test";

        backend.create_session(id, workspace.path().to_str().unwrap(), "", &[]).await?;
        assert!(backend.has_session(id).await);
        assert!(backend.session_pid(id).await.is_some());

        backend.kill_session(id).await?;
        assert!(!backend.has_session(id).await);
        anyhow::Ok(())
    })
    .await;

    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("real opencode acp handshake failed: {e}"),
        Err(_) => eprintln!("skipping {NAME}: real opencode binary did not complete the handshake within 20s"),
    }
}

/// Whether `npx` already has `package` cached locally, checked with
/// `--no-install` so this never triggers a network fetch during test
/// collection — the same "skip, don't hang or silently depend on network
/// access" contract as the opencode smoke test above, just adapted for an
/// adapter that isn't a standalone installed binary.
fn npx_package_cached(package: &str) -> bool {
    std::process::Command::new("npx")
        .args(["--no-install", package, "--version"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Real-adapter smoke test for Codex's `@agentclientprotocol/codex-acp`
/// (the design spec's resolved decision 2). Skips (rather than fails) when
/// the package isn't already cached for `npx` — this never triggers an
/// install, so CI without it still passes, same contract as the opencode
/// smoke test above. Handshake only, same reasoning (no model/API call).
///
/// `#[ignore]`d for the same reason as the opencode smoke test: a plain
/// `cargo test` stays fast and deterministic regardless of this machine's
/// npm cache.
#[tokio::test]
#[ignore = "spawns the real codex-acp adapter if cached; run explicitly with `cargo test -- --ignored`"]
async fn real_codex_acp_adapter_completes_the_acp_handshake_if_available() {
    const NAME: &str = "real_codex_acp_adapter_completes_the_acp_handshake_if_available";
    const PACKAGE: &str = "@agentclientprotocol/codex-acp";

    if !npx_package_cached(PACKAGE) {
        eprintln!("skipping {NAME}: {PACKAGE} not cached for npx");
        return;
    }

    let outcome = tokio::time::timeout(Duration::from_secs(20), async {
        let backend = AcpBackend::new();
        let workspace = tempfile::tempdir().unwrap();
        let id = "acp-real-smoke-test-codex";

        backend.create_session(id, workspace.path().to_str().unwrap(), "codex", &[]).await?;
        assert!(backend.has_session(id).await);
        assert!(backend.session_pid(id).await.is_some());

        backend.kill_session(id).await?;
        assert!(!backend.has_session(id).await);
        anyhow::Ok(())
    })
    .await;

    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("real codex-acp handshake failed: {e}"),
        Err(_) => eprintln!("skipping {NAME}: real codex-acp adapter did not complete the handshake within 20s"),
    }
}

/// Real-adapter smoke test for Pi's `pi-acp@0.0.34` (pinned per the design
/// spec's resolved decision 2). Same skip/cache/ignore contract as the
/// codex-acp smoke test above.
#[tokio::test]
#[ignore = "spawns the real pi-acp adapter if cached; run explicitly with `cargo test -- --ignored`"]
async fn real_pi_acp_adapter_completes_the_acp_handshake_if_available() {
    const NAME: &str = "real_pi_acp_adapter_completes_the_acp_handshake_if_available";
    const PACKAGE: &str = "pi-acp@0.0.34";

    if !npx_package_cached(PACKAGE) {
        eprintln!("skipping {NAME}: {PACKAGE} not cached for npx");
        return;
    }

    let outcome = tokio::time::timeout(Duration::from_secs(20), async {
        let backend = AcpBackend::new();
        let workspace = tempfile::tempdir().unwrap();
        let id = "acp-real-smoke-test-pi";

        backend.create_session(id, workspace.path().to_str().unwrap(), "pi", &[]).await?;
        assert!(backend.has_session(id).await);
        assert!(backend.session_pid(id).await.is_some());

        backend.kill_session(id).await?;
        assert!(!backend.has_session(id).await);
        anyhow::Ok(())
    })
    .await;

    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("real pi-acp handshake failed: {e}"),
        Err(_) => eprintln!("skipping {NAME}: real pi-acp adapter did not complete the handshake within 20s"),
    }
}
