//! Delivery over Claude Code's own cross-session messaging socket —
//! [`crate::config::SendMechanism::SessionSocket`], the transport its
//! `SendMessage` tool uses to talk between sessions.
//!
//! Every Claude Code session (2.1.x and later, when the cross-session
//! messaging feature is active for it) writes a registry record to
//! `$CLAUDE_CONFIG_DIR/sessions/<pid>.json` advertising a Unix socket it
//! listens on:
//!
//! ```json
//! { "pid": 17902, "sessionId": "ab88458b-…", "peerProtocol": 1,
//!   "tmux": "my-worker:@209.%209",
//!   "messagingSocketPath": "/tmp/cc-socks/17902.sock",
//!   "name": "my-worker-bb", "status": "idle" }
//! ```
//!
//! The socket speaks newline-delimited JSON; one [`USER_MESSAGE_TYPE`]
//! frame enqueues a user message for the session's next turn. Claude Code
//! documents this framing itself — a session started with `--debug` prints
//! `[uds-messaging] Inject messages: echo '{"type":"user","message":
//! {"role":"user","content":"hello"}}' | socat - UNIX-CONNECT:<path>`.
//!
//! Compared to the other two mechanisms this is both simpler and safer:
//! there are no keystrokes to collide with whatever the human is typing
//! (see `tmux::send_keys`'s verify/retry dance and the failure mode that
//! forced it), and no hooks that must have been installed into the
//! worktree ahead of time (see `messaging::target_can_drain_inbox`). An
//! idle session receives the message immediately rather than depending on
//! a best-effort wake nudge landing.
//!
//! What it does NOT do is guarantee reachability: the feature may be off
//! for a given session, the harness may not be Claude Code at all, and a
//! record may outlive the process that wrote it. Everything here is a
//! capability probe reporting "reachable or not" so
//! [`crate::messaging::deliver_message`] can fall back to keystrokes
//! rather than dropping a message into a socket nobody is listening on.
//!
//! One reachability failure this cannot detect: a receiver configured to
//! require per-connection authentication drops an unauthenticated frame and
//! destroys the connection, which looks identical to success from the
//! sending side. That default is Windows-only (elsewhere the socket's own
//! file permissions are the access control), and ninox does not support
//! Windows, so the case is out of reach rather than handled. It would
//! become live if a future Claude Code turned authentication on by default,
//! which is worth knowing when a message silently fails to arrive.

use crate::lifecycle::probe::is_pid_alive;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use tokio::{io::AsyncWriteExt, net::UnixStream};

/// The `type` field of the frame that enqueues a user message. The socket
/// also carries `control` frames (renames, delivery receipts) that we have
/// no reason to send.
const USER_MESSAGE_TYPE: &str = "user";

/// Largest frame the receiver will accept, in bytes.
///
/// It appends each chunk to a buffer and destroys the connection the moment
/// that buffer exceeds 1 MiB — the check runs BEFORE it scans for a
/// newline, so being a single well-formed line does not save an oversized
/// frame. Nothing is reported back to us: the write has already succeeded
/// into the kernel buffer by then, so without this guard an over-large
/// message would be reported delivered and silently dropped.
///
/// Compared in bytes against the receiver's UTF-16 length, which is
/// conservative in the right direction: a UTF-8 encoding is never shorter
/// than the UTF-16 unit count it decodes to.
const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Environment variable that forces Claude Code to bind its cross-session
/// messaging socket.
///
/// The feature is gated behind a remote flag that defaults to OFF; a
/// session started without it logs `[uds-messaging] Skipped: cross-session
/// messaging gate off` and advertises no socket, so [`find_peer`] finds
/// nothing and delivery degrades to keystrokes. This variable is the gate's
/// own first branch — any non-empty value turns it on regardless of the
/// flag — which is what lets ninox guarantee the mechanism it was told to
/// use is the one that actually runs.
///
/// Set on every session ninox spawns (see
/// `ninox_app::spawn_util::interactive_env_vars` and
/// `ninox_app::worker_env_vars`). It has no effect on sessions ninox did
/// not start, and none on already-running ones.
pub const CLAUDE_MESSAGING_GATE_ENV: &str = "CLAUDE_CODE_HARBOR_KITE";

/// A reachable-looking Claude Code session: one registry record that
/// advertises a messaging socket. "Looking" because the record is a file
/// that outlives its process — only [`send`] connecting proves liveness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerSession {
    /// The session's process id, which is also its record's file stem.
    pub pid:         u32,
    /// The Unix socket it advertised (`messagingSocketPath`).
    pub socket_path: PathBuf,
    /// Unix epoch milliseconds the session started, used to break ties
    /// between records that claim the same tmux session — see
    /// [`find_peer_in`].
    pub started_at:  i64,
}

/// The directory Claude Code writes session registry records to.
///
/// Honors `CLAUDE_CONFIG_DIR` the same way Claude Code itself does, so a
/// non-default config home (or a test redirecting away from the developer's
/// real `~/.claude`) resolves to the same place the sessions actually
/// register.
pub fn registry_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("sessions");
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("sessions")
}

/// Find the session advertising a messaging socket for ninox's
/// `session_id`, or `None` if there isn't one.
///
/// Ninox names each tmux session after the ninox session id
/// (`tmux::spawn`'s `new-session -s <id>`), and Claude Code records the
/// tmux pane it runs in as `"<session-name>:@<window>.<pane>"` — so the
/// part before the first `:` is the join key between the two systems.
///
/// A ptyd pane has no tmux name, so Claude Code records no `"tmux"` field
/// there. For those, `pane_pid` (the pane's root process) is the join key
/// instead: a record without a `"tmux"` field matches when its pid is the
/// pane's root or descends from it. The shallowest such record wins (the
/// pane root itself, then its direct child, ...), so a nested Claude Code
/// the worker started inside its pane never shadows the worker's own.
///
/// Any of these mean "no peer", none of them an error: the harness isn't
/// Claude Code, the version predates the feature, the feature is off for
/// that session, or the registry directory doesn't exist at all.
pub fn find_peer(session_id: &str, pane_pid: Option<u32>) -> Option<PeerSession> {
    find_peer_in(&registry_dir(), session_id, pane_pid)
}

/// [`find_peer`] against an explicit registry directory, for tests.
pub fn find_peer_in(registry_dir: &Path, session_id: &str, pane_pid: Option<u32>) -> Option<PeerSession> {
    let parents = std::cell::OnceCell::new();
    find_peer_joined(registry_dir, session_id, pane_pid, is_pid_alive, |pid, root| {
        depth_below(parents.get_or_init(crate::runtime::process::parent_map), pid, root)
    })
}

/// How many generations `pid` is below `root` (0 = `root` itself), or
/// `None` when it does not descend from it.
fn depth_below(parents: &std::collections::HashMap<u32, u32>, pid: u32, root: u32) -> Option<usize> {
    let mut current = pid;
    // Bounded so a pid-reuse cycle in a stale snapshot can't spin forever.
    for depth in 0..256 {
        if current == root {
            return Some(depth);
        }
        match parents.get(&current) {
            Some(&parent) if parent != current && current > 1 => current = parent,
            _ => return None,
        }
    }
    None
}

/// [`find_peer_in`] with the liveness probe injected, so tests can describe
/// dead processes without having to arrange real ones.
///
/// Records whose process is gone are skipped rather than merely losing the
/// tie-break. Claude Code only reaps records it can prove are stale, so a
/// session killed and respawned under the same ninox id leaves the old one
/// behind; among what survives, the newest by `startedAt` wins.
///
/// A live pid is not proof the record still describes that process — pids
/// are recycled. That mostly self-corrects, because a new Claude Code
/// session at a recycled pid overwrites the record (both are keyed by pid)
/// and so stops matching this tmux session. The exception is the window
/// where it has bound its socket but not yet written its record, which is
/// real but narrow, and would need `procStart` matching to close.
#[cfg(test)]
fn find_peer_where(
    registry_dir: &Path,
    session_id:   &str,
    is_alive:     impl Fn(u32) -> bool,
) -> Option<PeerSession> {
    find_peer_joined(registry_dir, session_id, None, is_alive, |_, _| None)
}

/// [`find_peer_where`] plus the ptyd pid join, with the process-tree probe
/// injected (`depth(pid, root)`, see [`depth_below`]).
fn find_peer_joined(
    registry_dir: &Path,
    session_id:   &str,
    pane_pid:     Option<u32>,
    is_alive:     impl Fn(u32) -> bool,
    depth:        impl Fn(u32, u32) -> Option<usize>,
) -> Option<PeerSession> {
    let entries = std::fs::read_dir(registry_dir).ok()?;
    entries
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let pid: u32 = path.file_stem()?.to_str()?.parse().ok()?;
            if !is_alive(pid) {
                return None;
            }
            let raw = std::fs::read_to_string(&path).ok()?;
            let record: serde_json::Value = serde_json::from_str(&raw).ok()?;

            let depth = match record.get("tmux").and_then(|v| v.as_str()).filter(|t| !t.is_empty()) {
                Some(tmux) => (tmux.split(':').next() == Some(session_id)).then_some(0),
                None => pane_pid.and_then(|root| if pid == root { Some(0) } else { depth(pid, root) }),
            }?;
            let socket_path = record.get("messagingSocketPath")?.as_str()?;
            if socket_path.is_empty() {
                return None;
            }
            let peer = PeerSession {
                pid,
                socket_path: PathBuf::from(socket_path),
                started_at: record.get("startedAt").and_then(|v| v.as_i64()).unwrap_or(0),
            };
            Some((depth, peer))
        })
        .max_by_key(|(depth, peer)| (std::cmp::Reverse(*depth), peer.started_at, peer.pid))
        .map(|(_, peer)| peer)
}

/// Enqueue `message` as a user message on `peer`'s socket.
///
/// Connecting is the liveness check: a record left behind by a dead session
/// points at a socket that refuses the connection (or no longer exists), and
/// that surfaces here as an error for the caller to fall back on.
///
/// An error means the frame did NOT reach the peer, so the caller is free to
/// re-send by another route. That is a contract this function has to keep
/// carefully, because the receiver processes each frame the moment its
/// newline arrives rather than waiting for end-of-stream: past a successful
/// `write_all` the message is committed, and anything that goes wrong
/// afterwards must not be reported as failure or the caller will deliver it
/// a second time.
///
/// Success means the session accepted the frame and will pick it up at its
/// next turn — not that the model has read it. Delivery is at-most-once: a
/// peer that dies between accepting the frame and acting on it drops the
/// message, the same caveat the file-based inbox carries.
pub async fn send(peer: &PeerSession, message: &str) -> Result<()> {
    let frame = serde_json::json!({
        "type": USER_MESSAGE_TYPE,
        "message": { "role": "user", "content": message },
    });
    // Newline-delimited: the receiver reads up to the newline, so a message
    // containing one must not be allowed to split into two frames. serde_json
    // escapes them into the string literal, which is what keeps that safe.
    let mut line = serde_json::to_vec(&frame)?;
    line.push(b'\n');

    // Checked before connecting: the receiver would take this frame and drop
    // it on the floor, and we would have no way to tell. Erroring here sends
    // the caller to a transport that can carry it.
    if line.len() > MAX_FRAME_BYTES {
        bail!(
            "message is too large for the session socket ({} bytes, limit {MAX_FRAME_BYTES}); \
             the receiver would drop the connection without delivering it",
            line.len(),
        );
    }

    let mut stream = UnixStream::connect(&peer.socket_path)
        .await
        .with_context(|| format!("connecting to session socket {}", peer.socket_path.display()))?;
    stream
        .write_all(&line)
        .await
        .with_context(|| format!("writing to session socket {}", peer.socket_path.display()))?;

    // Delivered as of the line above. Shutting down is a courtesy that lets
    // the peer see a clean EOF, and it routinely loses a benign race with a
    // receiver that has already processed the frame and closed first — so it
    // is logged, never propagated. Returning an error here would send an
    // already-delivered message down the keystroke fallback as well.
    if let Err(e) = stream.shutdown().await {
        tracing::debug!(
            "session socket {} was already closed when shutting down after a successful write \
             (message delivered): {e}",
            peer.socket_path.display(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use tokio::{io::AsyncReadExt, net::UnixListener};

    fn write_record(dir: &Path, pid: u32, record: serde_json::Value) {
        std::fs::write(dir.join(format!("{pid}.json")), record.to_string()).unwrap();
    }

    fn record_for(tmux: &str, socket: &str, started_at: i64) -> serde_json::Value {
        serde_json::json!({
            "tmux": tmux,
            "messagingSocketPath": socket,
            "startedAt": started_at,
            "peerProtocol": 1,
        })
    }

    #[test]
    fn finds_the_peer_whose_tmux_session_matches() {
        let dir = tempdir().unwrap();
        write_record(dir.path(), 100, record_for("other-worker:@1.%1", "/tmp/cc-socks/100.sock", 10));
        write_record(dir.path(), 200, record_for("my-worker:@2.%2", "/tmp/cc-socks/200.sock", 20));

        let peer = find_peer_where(dir.path(), "my-worker", |_| true).unwrap();
        assert_eq!(peer.pid, 200);
        assert_eq!(peer.socket_path, PathBuf::from("/tmp/cc-socks/200.sock"));
    }

    fn ptyd_record(socket: &str, started_at: i64) -> serde_json::Value {
        serde_json::json!({ "messagingSocketPath": socket, "startedAt": started_at })
    }

    #[test]
    fn a_ptyd_pane_joins_on_a_record_descending_from_its_root_pid() {
        let dir = tempdir().unwrap();
        write_record(dir.path(), 300, ptyd_record("/tmp/cc-socks/300.sock", 10));
        write_record(dir.path(), 400, ptyd_record("/tmp/cc-socks/400.sock", 20));
        // 300 runs under pane root 50; 400 belongs to some other pane.
        let descends = |pid: u32, root: u32| ((pid, root) == (300, 50)).then_some(2);

        let peer = find_peer_joined(dir.path(), "my-worker", Some(50), |_| true, descends).unwrap();
        assert_eq!(peer.pid, 300);
        assert_eq!(find_peer_joined(dir.path(), "my-worker", Some(400), |_| true, |_, _| None).unwrap().pid, 400,
            "the pane root itself counts");
        assert_eq!(find_peer_joined(dir.path(), "my-worker", Some(51), |_| true, descends), None);
    }

    #[test]
    fn a_ptyd_pane_prefers_the_shallowest_descendant_over_a_newer_nested_one() {
        let dir = tempdir().unwrap();
        // Pane root 50 runs a shell (60) running the worker's claude (300);
        // that claude later started a nested claude (400, via 350).
        write_record(dir.path(), 300, ptyd_record("/tmp/cc-socks/300.sock", 10));
        write_record(dir.path(), 400, ptyd_record("/tmp/cc-socks/400.sock", 99));
        write_record(dir.path(), 310, ptyd_record("/tmp/cc-socks/310.sock", 20));
        let parents: std::collections::HashMap<u32, u32> =
            [(60, 50), (300, 60), (310, 60), (350, 300), (400, 350), (50, 1)].into();
        let depth = |pid, root| depth_below(&parents, pid, root);

        let peer = find_peer_joined(dir.path(), "my-worker", Some(50), |_| true, depth).unwrap();
        assert_eq!(peer.pid, 310, "depth first, then the newest among equals");
        assert_eq!(depth_below(&parents, 50, 50), Some(0));
        assert_eq!(depth_below(&parents, 400, 50), Some(4));
        assert_eq!(depth_below(&parents, 400, 60), Some(3));
        assert_eq!(depth_below(&parents, 60, 300), None);
    }

    #[test]
    fn the_pid_join_never_applies_to_a_record_naming_a_tmux_session() {
        let dir = tempdir().unwrap();
        write_record(dir.path(), 300, record_for("someone-else:@1.%1", "/tmp/cc-socks/300.sock", 10));
        assert_eq!(find_peer_joined(dir.path(), "my-worker", Some(50), |_| true, |_, _| Some(1)), None);
    }

    #[test]
    fn records_without_a_tmux_field_never_match_a_tmux_session() {
        let dir = tempdir().unwrap();
        write_record(dir.path(), 300, ptyd_record("/tmp/cc-socks/300.sock", 10));
        assert_eq!(find_peer_joined(dir.path(), "my-worker", None, |_| true, |_, _| Some(1)), None);
    }

    #[test]
    fn matches_the_whole_tmux_session_name_not_a_prefix() {
        // "my-worker-2" starts with "my-worker" — splitting on ':' rather
        // than comparing prefixes is what keeps a message addressed to one
        // session from landing in a differently-named one.
        let dir = tempdir().unwrap();
        write_record(dir.path(), 100, record_for("my-worker-2:@1.%1", "/tmp/cc-socks/100.sock", 10));
        assert_eq!(find_peer_where(dir.path(), "my-worker", |_| true), None);
    }

    #[test]
    fn no_peer_when_the_record_advertises_no_socket() {
        // A session on a Claude Code build without cross-session messaging,
        // or one where the feature is off: the record exists, the socket
        // field does not.
        let dir = tempdir().unwrap();
        write_record(
            dir.path(),
            100,
            serde_json::json!({ "tmux": "my-worker:@1.%1", "startedAt": 10 }),
        );
        assert_eq!(find_peer_where(dir.path(), "my-worker", |_| true), None);
    }

    #[test]
    fn no_peer_when_the_advertised_socket_is_empty() {
        let dir = tempdir().unwrap();
        write_record(dir.path(), 100, record_for("my-worker:@1.%1", "", 10));
        assert_eq!(find_peer_where(dir.path(), "my-worker", |_| true), None);
    }

    #[test]
    fn no_peer_for_a_session_with_no_tmux_pane() {
        // Claude Code sessions started outside tmux have no join key to
        // ninox at all, and must never be matched by accident.
        let dir = tempdir().unwrap();
        write_record(
            dir.path(),
            100,
            serde_json::json!({ "messagingSocketPath": "/tmp/cc-socks/100.sock", "startedAt": 10 }),
        );
        assert_eq!(find_peer_where(dir.path(), "my-worker", |_| true), None);
    }

    #[test]
    fn prefers_the_newest_record_for_a_respawned_session() {
        // Kill a worker and respawn it under the same ninox id and both
        // records name the same tmux session; the stale one would send us
        // to a dead socket and force a keystroke fallback on every message.
        let dir = tempdir().unwrap();
        write_record(dir.path(), 100, record_for("my-worker:@1.%1", "/tmp/cc-socks/100.sock", 10));
        write_record(dir.path(), 300, record_for("my-worker:@3.%3", "/tmp/cc-socks/300.sock", 30));
        write_record(dir.path(), 200, record_for("my-worker:@2.%2", "/tmp/cc-socks/200.sock", 20));

        assert_eq!(find_peer_where(dir.path(), "my-worker", |_| true).unwrap().pid, 300);
    }

    #[test]
    fn ignores_unreadable_and_malformed_records() {
        // One junk file in the registry must not hide a perfectly good peer.
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("not-a-pid.json"), "{}").unwrap();
        std::fs::write(dir.path().join("400.json"), "{ this is not json").unwrap();
        write_record(dir.path(), 500, record_for("my-worker:@5.%5", "/tmp/cc-socks/500.sock", 50));

        assert_eq!(find_peer_where(dir.path(), "my-worker", |_| true).unwrap().pid, 500);
    }

    #[test]
    fn skips_a_record_whose_process_is_gone() {
        // The common stale-record shape: the session died, nothing reaped
        // its record. Taking it would mean dialing a dead socket and falling
        // back on every send while a live peer sits right next to it.
        let dir = tempdir().unwrap();
        write_record(dir.path(), 100, record_for("my-worker:@1.%1", "/tmp/cc-socks/100.sock", 90));
        write_record(dir.path(), 200, record_for("my-worker:@2.%2", "/tmp/cc-socks/200.sock", 20));

        let peer = find_peer_where(dir.path(), "my-worker", |pid| pid != 100).unwrap();
        assert_eq!(peer.pid, 200, "the dead record outranked the live one on startedAt");
    }

    #[test]
    fn no_peer_when_every_matching_record_is_dead() {
        let dir = tempdir().unwrap();
        write_record(dir.path(), 100, record_for("my-worker:@1.%1", "/tmp/cc-socks/100.sock", 10));
        assert_eq!(find_peer_where(dir.path(), "my-worker", |_| false), None);
    }

    #[test]
    fn the_real_lookup_probes_liveness() {
        // find_peer_in must wire in the actual probe, not default to
        // accepting everything — the injected-probe tests above would all
        // still pass if it did.
        let dir = tempdir().unwrap();
        // Pid 1 is init/launchd: always alive, never a Claude Code session.
        write_record(dir.path(), 1, record_for("my-worker:@1.%1", "/tmp/cc-socks/1.sock", 10));
        assert!(find_peer_in(dir.path(), "my-worker", None).is_some());

        // Spawn and reap a child to name a pid that is definitely dead.
        // Picking an arbitrarily large number instead is not portable: pids
        // that look impossible on macOS are ordinary on Linux, where the
        // default pid_max is 4194304.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead_pid = child.id();
        child.wait().unwrap();

        let dead = tempdir().unwrap();
        write_record(dead.path(), dead_pid, record_for("my-worker:@1.%1", "/tmp/cc-socks/x.sock", 10));
        assert_eq!(find_peer_in(dead.path(), "my-worker", None), None);
    }

    #[test]
    fn no_peer_when_the_registry_directory_is_missing() {
        // Claude Code has never run on this machine — not an error.
        assert_eq!(find_peer_where(Path::new("/nonexistent/sessions"), "my-worker", |_| true), None);
    }

    #[test]
    fn registry_dir_honors_the_claude_config_dir_env() {
        let _guard = crate::config::ENV_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let original = std::env::var("CLAUDE_CONFIG_DIR").ok();
        std::env::set_var("CLAUDE_CONFIG_DIR", "/custom/claude");
        assert_eq!(registry_dir(), PathBuf::from("/custom/claude/sessions"));
        match original {
            Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }
    }

    /// The wire format is the whole contract with Claude Code, so assert the
    /// exact bytes rather than just "a send succeeded": one newline-terminated
    /// JSON line, `type: "user"`, with the message under `message.content`.
    #[tokio::test]
    async fn send_writes_one_newline_terminated_user_frame() {
        let dir = tempdir().unwrap();
        let socket_path = dir.path().join("peer.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();

        let accept = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut received = Vec::new();
            stream.read_to_end(&mut received).await.unwrap();
            received
        });

        let peer = PeerSession { pid: 1, socket_path, started_at: 0 };
        send(&peer, "hello worker").await.unwrap();

        let received = String::from_utf8(accept.await.unwrap()).unwrap();
        assert!(received.ends_with('\n'), "frame must be newline-terminated: {received:?}");
        assert_eq!(received.matches('\n').count(), 1, "exactly one frame: {received:?}");

        let frame: serde_json::Value = serde_json::from_str(received.trim_end()).unwrap();
        assert_eq!(frame["type"], "user");
        assert_eq!(frame["message"]["role"], "user");
        assert_eq!(frame["message"]["content"], "hello worker");
    }

    #[tokio::test]
    async fn a_multiline_message_stays_a_single_frame() {
        // Newlines in the message must be escaped into the JSON string, not
        // split the frame — a second "frame" would be unparsable garbage and
        // the tail of the message would be silently lost.
        let dir = tempdir().unwrap();
        let socket_path = dir.path().join("peer.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();

        let accept = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut received = Vec::new();
            stream.read_to_end(&mut received).await.unwrap();
            received
        });

        let peer = PeerSession { pid: 1, socket_path, started_at: 0 };
        send(&peer, "first line\nsecond line").await.unwrap();

        let received = String::from_utf8(accept.await.unwrap()).unwrap();
        assert_eq!(received.matches('\n').count(), 1, "exactly one frame: {received:?}");
        let frame: serde_json::Value = serde_json::from_str(received.trim_end()).unwrap();
        assert_eq!(frame["message"]["content"], "first line\nsecond line");
    }

    /// The peer processing a frame and closing immediately — its actual
    /// behaviour once a complete line arrives — must read as success, since
    /// the caller re-sends by keystrokes on any error and the message has
    /// already been delivered by then.
    ///
    /// Note this does NOT cover the `shutdown`-returns-an-error branch that
    /// motivated making its result non-fatal: an early peer close leaves
    /// `shutdown` returning `Ok` on macOS, and there is no portable way to
    /// force it to fail in-process. That branch rests on the shape of the
    /// code — the result is logged, never propagated — rather than on this
    /// test, which passes either way.
    #[tokio::test]
    async fn send_succeeds_when_the_peer_closes_before_shutdown() {
        let dir = tempdir().unwrap();
        let socket_path = dir.path().join("peer.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();

        let accept = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            // Read the frame, then drop the connection immediately — the
            // receiver's real behaviour once it has a complete line, and the
            // race that makes our shutdown fail.
            let mut buf = vec![0u8; 4096];
            let n = stream.read(&mut buf).await.unwrap();
            drop(stream);
            buf.truncate(n);
            buf
        });

        let peer = PeerSession { pid: 1, socket_path, started_at: 0 };
        let result = send(&peer, "hello worker").await;

        let received = String::from_utf8(accept.await.unwrap()).unwrap();
        let frame: serde_json::Value = serde_json::from_str(received.trim_end()).unwrap();
        assert_eq!(frame["message"]["content"], "hello worker", "the peer did receive it");
        assert!(
            result.is_ok(),
            "a delivered message must not be reported as failed, or it gets sent twice: {result:?}"
        );
    }

    #[tokio::test]
    async fn send_refuses_a_frame_over_the_receivers_buffer_limit() {
        // The receiver destroys the connection once its buffer passes 1 MiB
        // without telling us, so this has to be caught before the write —
        // otherwise it reports success and the message is gone.
        let dir = tempdir().unwrap();
        let socket_path = dir.path().join("peer.sock");
        let _listener = UnixListener::bind(&socket_path).unwrap();

        let peer = PeerSession { pid: 1, socket_path, started_at: 0 };
        let oversized = "x".repeat(MAX_FRAME_BYTES + 1);
        let err = send(&peer, &oversized).await.unwrap_err().to_string();
        assert!(err.contains("too large"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn send_accepts_a_frame_just_under_the_limit() {
        // Guards the boundary from the other side: the cap must not be so
        // conservative that ordinary large messages stop using the socket.
        let dir = tempdir().unwrap();
        let socket_path = dir.path().join("peer.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let accept = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut received = Vec::new();
            stream.read_to_end(&mut received).await.unwrap();
            received.len()
        });

        let peer = PeerSession { pid: 1, socket_path, started_at: 0 };
        // Leave room for the JSON envelope around the content.
        send(&peer, &"x".repeat(MAX_FRAME_BYTES - 1024)).await.unwrap();
        assert!(accept.await.unwrap() <= MAX_FRAME_BYTES);
    }

    #[tokio::test]
    async fn send_errors_when_nothing_is_listening() {
        // The record outlived its process. deliver_message relies on this
        // being an error so it can fall back rather than lose the message.
        let dir = tempdir().unwrap();
        let peer = PeerSession {
            pid:         1,
            socket_path: dir.path().join("stale.sock"),
            started_at:  0,
        };
        assert!(send(&peer, "hello").await.is_err());
    }
}
