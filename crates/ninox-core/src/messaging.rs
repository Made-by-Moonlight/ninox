//! Orchestrator↔worker message delivery across the mechanisms in
//! `config::SendMechanism`. Shared by both callers that inject a message
//! into a session: the `ninox send` CLI and `Engine::send_to_session`
//! (poller reactions).

use crate::{config::SendMechanism, inbox, runtime, session_socket, store::Store};
use anyhow::Result;
use std::path::{Path, PathBuf};

/// The `from` ninox itself uses for messages whose content it generated
/// internally (poller reactions, fleet recovery briefings) rather than
/// relaying on behalf of some other caller. These are trusted — the
/// engine's own code produced the text — so they are attributed to this
/// sentinel rather than recorded as `None` (unverified), which is reserved
/// for a case where the sender SHOULD be a resolvable session but couldn't
/// be (e.g. a `ninox send` invocation outside any known session). Content
/// that originated from an external caller — a spawned session's initial
/// `--prompt` brief, for instance — is attributed to that caller's resolved
/// identity (or left unverified) instead; `SYSTEM_SENDER` must never stand
/// in for "we don't actually know who wrote this."
pub const SYSTEM_SENDER: &str = "ninox";

/// Whether `id` is reserved for ninox's own attribution and can therefore
/// never be claimed as a spawned session's id — currently just
/// [`SYSTEM_SENDER`] itself. Without this, a worker or orchestrator spawned
/// with `--name ninox` would get the literal session id `"ninox"` (`slugify`
/// is a no-op on it), and `inbox::attribute` would then render that
/// session's own `ninox send` messages as `[from ninox]` — byte-identical to
/// the tag this PR uses for ninox's own trusted, internally-generated
/// messages, defeating the distinction entirely. Every spawn path (CLI
/// worker/orchestrator spawn, the GUI spawn modal) must reject this id
/// before it is ever assigned.
pub fn is_reserved_session_id(id: &str) -> bool {
    id == SYSTEM_SENDER
}

/// Deliver `message` to `session_id` by `mechanism`, attributed to `from`
/// (only recorded by the inbox mechanism — see `inbox::InboxMessage::from`;
/// the session-socket and keystroke mechanisms inject literal text with no
/// sender-framing channel of their own).
///
/// Both of the non-keystroke mechanisms need something to be true of the
/// target that ninox does not control, so each is paired with a capability
/// check and falls back to `tmux::send_keys` when it fails. That fallback
/// is not a nicety: handing a message to a transport nobody is reading
/// would be silent, permanent loss, which is never the better outcome.
///
/// - [`SendMechanism::SessionSocket`] (default): if the target advertises a
///   Claude Code messaging socket (see [`session_socket::find_peer`]), the
///   message is written to it as one JSON line and enqueued for the
///   session's next turn. No keystrokes are involved, so nothing can
///   collide with what the human is typing, and an idle session gets it
///   immediately rather than depending on a wake nudge. Falls back to
///   keystrokes when no peer is advertised (non-Claude harness, a build or
///   configuration without cross-session messaging) and, because a registry
///   record can outlive its process, also when the socket turns out to be
///   dead on connect.
/// - [`SendMechanism::Inbox`]: if the target can actually drain a file-based
///   inbox (see [`target_can_drain_inbox`]), the message is written durably
///   to it (`inbox::write_message`) for the Stop/UserPromptSubmit hooks in
///   the worker's worktree settings to drain (see
///   `ninox_app::spawn_util::ensure_statusline_settings`). Keystrokes are
///   then only a best-effort idle-wake nudge (`tmux::wake_idle_session`).
///   Failing to WRITE the inbox file is a real delivery failure and
///   propagates. The nudge is a different story, and the guarantee is
///   weaker than it may look at first: for a session ACTIVELY working, the
///   very next Stop (turn end) or UserPromptSubmit drains it regardless of
///   whether the nudge lands. For a session already IDLE, there is no such
///   next turn boundary coming on its own — the nudge is the ONLY delivery
///   trigger, and it is single-shot and best-effort (see
///   `tmux::wake_idle_session`'s own doc comment for exactly what can make
///   it not fire). If it doesn't land, the message stays durably pending
///   but genuinely undelivered until something else makes the session
///   active again. This residual gap is the reason the session socket, and
///   not the inbox, is the default.
/// - [`SendMechanism::Keystrokes`]: `tmux::send_keys` injects the message
///   directly as verified keyboard input (hardened in PR #69 with a
///   pre-Enter delay and verify/retry). Always available, and what the
///   other two degrade to.
pub async fn deliver_message(
    store:        &Store,
    sessions_dir: &Path,
    session_id:   &str,
    message:      &str,
    mechanism:    SendMechanism,
    from:         Option<&str>,
) -> Result<()> {
    deliver_by_mechanism(store, sessions_dir, session_id, message, mechanism, from).await?;
    // The counter feeds the sidebar's unread badge (`Store::message_delivered_counts`);
    // a miss there must never turn an already-delivered message into an error.
    if let Err(e) = store.record_message_delivered(session_id) {
        tracing::warn!("record delivered message for {session_id}: {e}");
    }
    Ok(())
}

async fn deliver_by_mechanism(
    store:        &Store,
    sessions_dir: &Path,
    session_id:   &str,
    message:      &str,
    mechanism:    SendMechanism,
    from:         Option<&str>,
) -> Result<()> {
    match mechanism {
        SendMechanism::SessionSocket => {
            let pane_pid = runtime::ptyd_pane_pid(session_id).await;
            deliver_via_session_socket(session_socket::find_peer(session_id, pane_pid), session_id, message).await
        }
        SendMechanism::Inbox if target_can_drain_inbox(store, session_id) => {
            inbox::write_message(sessions_dir, session_id, message, from)?;
            if let Err(e) = runtime::wake_idle_session(session_id).await {
                tracing::warn!(
                    "idle-wake nudge failed for {session_id} (message already delivered via inbox): {e}"
                );
            }
            Ok(())
        }
        SendMechanism::Inbox | SendMechanism::Keystrokes => {
            runtime::send_keys(session_id, message).await
        }
    }
}

/// The [`SendMechanism::SessionSocket`] path, with its two fallbacks.
///
/// The second one is the subtle one: `find_peer` reads a file, and the file
/// outlives the process that wrote it, so `Some(peer)` is a claim about the
/// registry rather than about anything listening. Connecting is the only
/// real liveness test, which means the dead-socket case can only be
/// discovered after we have already committed to this mechanism — hence
/// falling back on the write failing rather than probing up front.
///
/// Takes the lookup result rather than performing it so the delivery
/// behavior can be tested against a real socket without redirecting the
/// process-global registry path; [`session_socket::find_peer_in`] covers
/// the lookup itself.
async fn deliver_via_session_socket(
    peer:       Option<session_socket::PeerSession>,
    session_id: &str,
    message:    &str,
) -> Result<()> {
    let Some(peer) = peer else {
        tracing::debug!(
            "{session_id} advertises no Claude Code messaging socket; sending as keystrokes"
        );
        return runtime::send_keys(session_id, message).await;
    };
    match session_socket::send(&peer, message).await {
        Ok(()) => Ok(()),
        Err(e) => {
            tracing::warn!(
                "session socket for {session_id} (pid {}) did not accept the message, \
                 falling back to keystrokes: {e}",
                peer.pid
            );
            runtime::send_keys(session_id, message).await
        }
    }
}

/// Whether `session_id`'s recorded workspace can actually drain a
/// file-based inbox: a `claude-code` harness (the only harness with a
/// Claude Code hook mechanism at all — codex/aider/opencode have no
/// equivalent) whose workspace's `.claude/settings.json` has the inbox
/// drain hooks installed.
///
/// Deliberately does NOT trust the global toggle alone — it only reflects
/// "was this on when the worktree was created", not "will draining actually
/// happen for THIS specific session". Covers every gap that would otherwise
/// silently swallow a message:
/// - orchestrators (hooks are only ever installed in worker worktrees, by
///   scope — the orchestrator's own root settings never get them),
/// - a worktree created before the toggle was turned on (or one whose
///   `.claude/settings.json` was already checked into the branch —
///   `ensure_statusline_settings` never touches an existing file),
/// - the shared-workspace fallback `run_spawn`/`SpawnFormConfirm` use when
///   worktree creation itself fails,
/// - non-`claude-code` harnesses, which never get `.claude/settings.json`
///   hooks processed by anything.
fn target_can_drain_inbox(store: &Store, session_id: &str) -> bool {
    let Ok(Some(session)) = store.get_session(session_id) else {
        return false;
    };
    if session.agent_type != "claude-code" {
        return false;
    }
    let Some(workspace) = session.workspace_path else {
        return false;
    };
    inbox_hooks_installed(Path::new(&workspace))
}

/// Substring identifying OUR Stop hook's command, as written by
/// `ensure_statusline_settings` (`"<ninox_bin> inbox drain-stop"`).
const STOP_HOOK_MARKER: &str = "inbox drain-stop";

/// Whether `workspace`'s `.claude/settings.json` has a `Stop` hook whose
/// command is specifically ninox's own inbox drain (contains
/// [`STOP_HOOK_MARKER`]) — NOT merely "some Stop hook exists". A worktree
/// whose checked-in settings.json happens to carry an unrelated Stop hook
/// (e.g. a lint-on-stop check) would otherwise pass a bare
/// presence/non-empty check while nothing there actually drains our
/// inbox — the exact same silent-loss shape this whole gate exists to
/// close. Missing file, unparsable JSON, or no matching command all mean
/// "cannot drain" rather than erroring — this is a best-effort capability
/// probe, not a delivery path of its own.
fn inbox_hooks_installed(workspace: &Path) -> bool {
    let settings_path: PathBuf = workspace.join(".claude").join("settings.json");
    let Ok(raw) = std::fs::read_to_string(&settings_path) else {
        return false;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    let Some(stop_groups) = json.get("hooks").and_then(|h| h.get("Stop")).and_then(|s| s.as_array()) else {
        return false;
    };
    stop_groups.iter().any(|group| {
        group
            .get("hooks")
            .and_then(|h| h.as_array())
            .is_some_and(|hooks| {
                hooks.iter().any(|hook| {
                    hook.get("command")
                        .and_then(|c| c.as_str())
                        .is_some_and(|c| c.contains(STOP_HOOK_MARKER))
                })
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::AgentConfig, types::{Session, SessionStatus}};
    use tempfile::tempdir;

    #[test]
    fn is_reserved_session_id_matches_only_the_system_sender() {
        assert!(is_reserved_session_id(SYSTEM_SENDER));
        assert!(!is_reserved_session_id("worker-1"));
        assert!(!is_reserved_session_id("ninox-2"), "must be an exact match, not a prefix");
    }

    fn session_with(id: &str, agent_type: &str, workspace_path: Option<String>) -> Session {
        Session {
            id: id.to_string(), orchestrator_id: None, name: id.to_string(),
            repo: String::new(), status: SessionStatus::Working,
            agent_type: agent_type.to_string(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path, pid: None,
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
        }
    }

    fn write_installed_hooks(workspace: &Path) {
        std::fs::create_dir_all(workspace.join(".claude")).unwrap();
        std::fs::write(
            workspace.join(".claude").join("settings.json"),
            serde_json::json!({
                "hooks": {
                    "Stop": [{"hooks": [{"type": "command", "command": "ninox inbox drain-stop"}]}],
                    "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "ninox inbox drain-prompt"}]}]
                }
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn inbox_hooks_installed_true_when_stop_hooks_present() {
        let dir = tempdir().unwrap();
        write_installed_hooks(dir.path());
        assert!(inbox_hooks_installed(dir.path()));
    }

    #[test]
    fn inbox_hooks_installed_false_without_a_settings_file() {
        let dir = tempdir().unwrap();
        assert!(!inbox_hooks_installed(dir.path()));
    }

    #[test]
    fn inbox_hooks_installed_false_when_settings_has_no_hooks_table() {
        // e.g. a worktree created before the toggle was ever turned on —
        // ensure_statusline_settings wrote only statusLine.
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
        std::fs::write(
            dir.path().join(".claude").join("settings.json"),
            r#"{"statusLine": {"type": "command", "command": "ninox statusline"}}"#,
        )
        .unwrap();
        assert!(!inbox_hooks_installed(dir.path()));
    }

    #[test]
    fn inbox_hooks_installed_false_when_stop_array_is_empty() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
        std::fs::write(
            dir.path().join(".claude").join("settings.json"),
            r#"{"hooks": {"Stop": []}}"#,
        )
        .unwrap();
        assert!(!inbox_hooks_installed(dir.path()));
    }

    #[test]
    fn inbox_hooks_installed_false_for_a_foreign_stop_hook() {
        // A worktree whose checked-in settings.json has SOME Stop hook —
        // just not ours (e.g. a lint-on-stop check) — must not be treated
        // as drainable: nothing there actually drains our inbox.
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
        std::fs::write(
            dir.path().join(".claude").join("settings.json"),
            serde_json::json!({
                "hooks": {
                    "Stop": [{"hooks": [{"type": "command", "command": "node .claude/lint-on-stop.cjs"}]}]
                }
            })
            .to_string(),
        )
        .unwrap();
        assert!(!inbox_hooks_installed(dir.path()));
    }

    #[test]
    fn target_can_drain_inbox_false_for_unknown_session() {
        let store = Store::open(tempdir().unwrap().keep().join("t.db")).unwrap();
        assert!(!target_can_drain_inbox(&store, "no-such-session"));
    }

    #[test]
    fn target_can_drain_inbox_false_for_non_claude_harness() {
        let store = Store::open(tempdir().unwrap().keep().join("t.db")).unwrap();
        let ws = tempdir().unwrap().keep();
        write_installed_hooks(&ws);
        store.upsert_session(&session_with("codex-worker", "codex", Some(ws.to_string_lossy().to_string()))).unwrap();
        assert!(!target_can_drain_inbox(&store, "codex-worker"));
    }

    #[test]
    fn target_can_drain_inbox_false_for_orchestrator_with_no_workspace() {
        // Orchestrators never get inbox hooks by scope; the common
        // real-world shape is also just "no workspace_path recorded".
        let store = Store::open(tempdir().unwrap().keep().join("t.db")).unwrap();
        store.upsert_session(&session_with("orch-1", "claude-code", None)).unwrap();
        assert!(!target_can_drain_inbox(&store, "orch-1"));
    }

    #[test]
    fn target_can_drain_inbox_false_for_workspace_without_installed_hooks() {
        // e.g. a worktree created before the toggle was turned on.
        let store = Store::open(tempdir().unwrap().keep().join("t.db")).unwrap();
        let ws = tempdir().unwrap().keep();
        store.upsert_session(&session_with("worker-1", "claude-code", Some(ws.to_string_lossy().to_string()))).unwrap();
        assert!(!target_can_drain_inbox(&store, "worker-1"));
    }

    #[test]
    fn target_can_drain_inbox_true_for_claude_code_worker_with_installed_hooks() {
        let store = Store::open(tempdir().unwrap().keep().join("t.db")).unwrap();
        let ws = tempdir().unwrap().keep();
        write_installed_hooks(&ws);
        store.upsert_session(&session_with("worker-1", "claude-code", Some(ws.to_string_lossy().to_string()))).unwrap();
        assert!(target_can_drain_inbox(&store, "worker-1"));
    }

    #[tokio::test]
    async fn the_inbox_mechanism_writes_the_message_when_target_can_drain() {
        let store = Store::open(tempdir().unwrap().keep().join("t.db")).unwrap();
        let ws = tempdir().unwrap().keep();
        write_installed_hooks(&ws);
        store.upsert_session(&session_with("worker-1", "claude-code", Some(ws.to_string_lossy().to_string()))).unwrap();

        let sessions_dir = tempdir().unwrap();
        // No real tmux session named this exists — the best-effort idle-wake
        // nudge must degrade to a no-op rather than surfacing as an error,
        // since the message is already durably written by this point.
        deliver_message(&store, sessions_dir.path(), "worker-1", "hello worker", SendMechanism::Inbox, Some("orch-1"))
            .await
            .unwrap();

        let pending = inbox::read_pending_messages(sessions_dir.path(), "worker-1").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].text, "hello worker");
        assert_eq!(pending[0].from, Some("orch-1".to_string()));
    }

    #[tokio::test]
    async fn a_successful_delivery_is_counted_against_the_target_session() {
        let store = Store::open(tempdir().unwrap().keep().join("t.db")).unwrap();
        let ws = tempdir().unwrap().keep();
        write_installed_hooks(&ws);
        store.upsert_session(&session_with("worker-1", "claude-code", Some(ws.to_string_lossy().to_string()))).unwrap();
        let sessions_dir = tempdir().unwrap();

        deliver_message(&store, sessions_dir.path(), "worker-1", "one", SendMechanism::Inbox, None).await.unwrap();
        deliver_message(&store, sessions_dir.path(), "worker-1", "two", SendMechanism::Inbox, None).await.unwrap();

        assert_eq!(store.message_delivered_counts().unwrap().get("worker-1"), Some(&2));
    }

    #[tokio::test]
    async fn a_failed_delivery_is_not_counted() {
        let store = Store::open(tempdir().unwrap().keep().join("t.db")).unwrap();
        let sessions_dir = tempdir().unwrap();

        let result =
            deliver_message(&store, sessions_dir.path(), "orch-1", "hello", SendMechanism::Keystrokes, None).await;

        assert!(result.is_err(), "no tmux session named orch-1 exists, so keystrokes must fail");
        assert!(store.message_delivered_counts().unwrap().is_empty());
    }

    #[tokio::test]
    async fn falls_back_to_keystrokes_when_target_cannot_drain_an_inbox() {
        // Regression test for the orchestrator-target silent-loss gap:
        // inbox selected, but the target (here: no recorded session at all,
        // the same shape as an orchestrator/unknown target) has nowhere to
        // drain a written inbox message. The message must NOT be written
        // to the inbox — falling back to send_keys, which then errors
        // against a nonexistent tmux session, proves the fallback path
        // was taken rather than a silent, undrainable write.
        let store = Store::open(tempdir().unwrap().keep().join("t.db")).unwrap();
        let sessions_dir = tempdir().unwrap();

        let result =
            deliver_message(&store, sessions_dir.path(), "orch-1", "hello", SendMechanism::Inbox, None).await;

        assert!(result.is_err(), "must fall back to (and surface failures from) send_keys");
        assert!(
            inbox::read_pending_messages(sessions_dir.path(), "orch-1").unwrap().is_empty(),
            "message must never be silently written to an inbox nobody can drain"
        );
    }

    #[tokio::test]
    async fn the_keystrokes_mechanism_never_touches_the_inbox() {
        let store = Store::open(tempdir().unwrap().keep().join("t.db")).unwrap();
        let ws = tempdir().unwrap().keep();
        write_installed_hooks(&ws);
        store.upsert_session(&session_with("worker-1", "claude-code", Some(ws.to_string_lossy().to_string()))).unwrap();
        let sessions_dir = tempdir().unwrap();

        // send_keys against a nonexistent tmux session errors —
        // deliver_message must propagate that, not swallow it, when
        // keystrokes are selected, even though this target COULD drain an
        // inbox.
        let result =
            deliver_message(&store, sessions_dir.path(), "worker-1", "hello", SendMechanism::Keystrokes, None).await;
        assert!(result.is_err());
        assert!(inbox::read_pending_messages(sessions_dir.path(), "worker-1").unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_session_socket_mechanism_delivers_to_a_listening_peer() {
        let dir = tempdir().unwrap();
        let socket_path = dir.path().join("peer.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
        let accept = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut received = Vec::new();
            tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut received).await.unwrap();
            received
        });

        let peer = session_socket::PeerSession { pid: 1, socket_path, started_at: 0 };
        deliver_via_session_socket(Some(peer), "worker-1", "hello worker").await.unwrap();

        let frame: serde_json::Value =
            serde_json::from_slice(String::from_utf8(accept.await.unwrap()).unwrap().trim_end().as_bytes())
                .unwrap();
        assert_eq!(frame["message"]["content"], "hello worker");
    }

    #[tokio::test]
    async fn falls_back_to_keystrokes_when_no_peer_advertises_a_socket() {
        // A non-Claude harness, or a Claude Code build/configuration without
        // cross-session messaging. send_keys erroring against a nonexistent
        // tmux session is what proves the fallback ran.
        assert!(
            deliver_via_session_socket(None, "worker-1", "hello").await.is_err(),
            "must fall back to (and surface failures from) send_keys"
        );
    }

    #[tokio::test]
    async fn falls_back_to_keystrokes_when_the_advertised_socket_is_dead() {
        // The registry record outlived the process that wrote it. Nothing is
        // listening, so the message would vanish if we treated "a peer was
        // found" as "a peer will receive it".
        let dir = tempdir().unwrap();
        let peer = session_socket::PeerSession {
            pid:         1,
            socket_path: dir.path().join("stale.sock"),
            started_at:  0,
        };
        assert!(
            deliver_via_session_socket(Some(peer), "worker-1", "hello").await.is_err(),
            "a dead socket must fall back to send_keys, not swallow the message"
        );
    }

    #[test]
    fn agent_config_default_harness_is_claude_code() {
        // Sanity check the literal "claude-code" comparison in
        // target_can_drain_inbox actually matches the harness default.
        assert_eq!(AgentConfig::default().harness, "claude-code");
    }
}
