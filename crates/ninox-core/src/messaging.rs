//! Orchestrator↔worker message delivery, gated by the opt-in file-based
//! inbox toggle (`config::InboxMessagingConfig`, default off). Shared by
//! both callers that inject a message into a session: the `ninox send` CLI
//! and `Engine::send_to_session` (poller reactions).

use crate::{inbox, store::Store, tmux};
use anyhow::Result;
use std::path::{Path, PathBuf};

/// Deliver `message` to `session_id`.
///
/// - `inbox_enabled = false` (default): unchanged pre-existing behavior —
///   `tmux::send_keys` injects the message directly as verified keyboard
///   input (hardened in PR #69 with a pre-Enter delay and verify/retry).
/// - `inbox_enabled = true` AND the target can actually drain a file-based
///   inbox (see [`target_can_drain_inbox`]): the message is written durably
///   to the target session's file-based inbox (`inbox::write_message`),
///   which the Stop/UserPromptSubmit hooks installed in the worker's
///   worktree settings drain (see
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
///   active again (a human interacting with it, or a later message
///   triggering another nudge attempt) — this is a known residual gap, not
///   a "will always eventually drain" guarantee. Given this whole feature
///   is opt-in and default-off, closing that gap (e.g. a poller loop that
///   keeps re-nudging a session with pending mail) is left as a follow-up
///   rather than built into this change.
/// - `inbox_enabled = true` but the target CANNOT drain a file-based inbox
///   (an orchestrator — hooks are only ever installed in worker worktrees;
///   a pre-existing worktree whose settings.json predates the toggle being
///   turned on; a non-`claude-code` harness with no Claude Code hook
///   mechanism at all): falls back to the same verified keystroke path as
///   the toggle-off case. Writing to an inbox nobody drains would be
///   silent, permanent message loss — never acceptable regardless of the
///   toggle.
pub async fn deliver_message(
    store:         &Store,
    sessions_dir:  &Path,
    session_id:    &str,
    message:       &str,
    inbox_enabled: bool,
) -> Result<()> {
    if !inbox_enabled || !target_can_drain_inbox(store, session_id) {
        return tmux::send_keys(session_id, message).await;
    }
    inbox::write_message(sessions_dir, session_id, message)?;
    if let Err(e) = tmux::wake_idle_session(session_id).await {
        tracing::warn!(
            "idle-wake nudge failed for {session_id} (message already delivered via inbox): {e}"
        );
    }
    Ok(())
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

    fn session_with(id: &str, agent_type: &str, workspace_path: Option<String>) -> Session {
        Session {
            id: id.to_string(), orchestrator_id: None, name: id.to_string(),
            repo: String::new(), status: SessionStatus::Working,
            agent_type: agent_type.to_string(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path, pid: None,
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None, gate_status: None,
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
    async fn inbox_enabled_writes_the_message_when_target_can_drain() {
        let store = Store::open(tempdir().unwrap().keep().join("t.db")).unwrap();
        let ws = tempdir().unwrap().keep();
        write_installed_hooks(&ws);
        store.upsert_session(&session_with("worker-1", "claude-code", Some(ws.to_string_lossy().to_string()))).unwrap();

        let sessions_dir = tempdir().unwrap();
        // No real tmux session named this exists — the best-effort idle-wake
        // nudge must degrade to a no-op rather than surfacing as an error,
        // since the message is already durably written by this point.
        deliver_message(&store, sessions_dir.path(), "worker-1", "hello worker", true).await.unwrap();

        let pending = inbox::read_pending_messages(sessions_dir.path(), "worker-1").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].text, "hello worker");
    }

    #[tokio::test]
    async fn falls_back_to_keystrokes_when_target_cannot_drain_an_inbox() {
        // Regression test for the orchestrator-target silent-loss gap:
        // toggle on, but the target (here: no recorded session at all,
        // the same shape as an orchestrator/unknown target) has nowhere to
        // drain a written inbox message. The message must NOT be written
        // to the inbox — falling back to send_keys, which then errors
        // against a nonexistent tmux session, proves the fallback path
        // was taken rather than a silent, undrainable write.
        let store = Store::open(tempdir().unwrap().keep().join("t.db")).unwrap();
        let sessions_dir = tempdir().unwrap();

        let result = deliver_message(&store, sessions_dir.path(), "orch-1", "hello", true).await;

        assert!(result.is_err(), "must fall back to (and surface failures from) send_keys");
        assert!(
            inbox::read_pending_messages(sessions_dir.path(), "orch-1").unwrap().is_empty(),
            "message must never be silently written to an inbox nobody can drain"
        );
    }

    #[tokio::test]
    async fn inbox_disabled_never_touches_the_inbox() {
        let store = Store::open(tempdir().unwrap().keep().join("t.db")).unwrap();
        let ws = tempdir().unwrap().keep();
        write_installed_hooks(&ws);
        store.upsert_session(&session_with("worker-1", "claude-code", Some(ws.to_string_lossy().to_string()))).unwrap();
        let sessions_dir = tempdir().unwrap();

        // send_keys against a nonexistent tmux session errors —
        // deliver_message must propagate that, not swallow it, when the
        // toggle is off, even though this target COULD drain an inbox.
        let result = deliver_message(&store, sessions_dir.path(), "worker-1", "hello", false).await;
        assert!(result.is_err());
        assert!(inbox::read_pending_messages(sessions_dir.path(), "worker-1").unwrap().is_empty());
    }

    #[test]
    fn agent_config_default_harness_is_claude_code() {
        // Sanity check the literal "claude-code" comparison in
        // target_can_drain_inbox actually matches the harness default.
        assert_eq!(AgentConfig::default().harness, "claude-code");
    }
}
