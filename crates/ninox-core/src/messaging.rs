//! Orchestrator↔worker message delivery, gated by the opt-in file-based
//! inbox toggle (`config::InboxMessagingConfig`, default off). Shared by
//! both callers that inject a message into a session: the `ninox send` CLI
//! and `Engine::send_to_session` (poller reactions).

use crate::{inbox, tmux};
use anyhow::Result;
use std::path::Path;

/// Deliver `message` to `session_id`.
///
/// - `inbox_enabled = false` (default): unchanged pre-existing behavior —
///   `tmux::send_keys` injects the message directly as verified keyboard
///   input (hardened in PR #69 with a pre-Enter delay and verify/retry).
/// - `inbox_enabled = true`: the message is written durably to the target
///   session's file-based inbox (`inbox::write_message`), which the Stop/
///   UserPromptSubmit hooks installed in the worker's worktree settings
///   drain (see `ninox_app::spawn_util::ensure_statusline_settings`).
///   Keystrokes are then only a best-effort idle-wake nudge
///   (`tmux::wake_idle_session`) — losing that race is not a delivery
///   failure (the inbox file already durably holds the message and will be
///   drained on the next natural Stop/UserPromptSubmit); failing to write
///   the inbox file is.
pub async fn deliver_message(
    sessions_dir:  &Path,
    session_id:    &str,
    message:       &str,
    inbox_enabled: bool,
) -> Result<()> {
    if !inbox_enabled {
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn inbox_enabled_writes_the_message_and_never_errors_without_a_live_session() {
        let dir = tempdir().unwrap();
        // No real tmux session named this exists — the best-effort idle-wake
        // nudge must degrade to a no-op rather than surfacing as an error,
        // since the message is already durably written by this point.
        deliver_message(dir.path(), "no-such-session", "hello worker", true).await.unwrap();

        let pending = inbox::read_pending_messages(dir.path(), "no-such-session").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].text, "hello worker");
    }

    #[tokio::test]
    async fn inbox_disabled_never_touches_the_inbox() {
        let dir = tempdir().unwrap();
        // send_keys against a nonexistent session errors — deliver_message
        // must propagate that, not swallow it, when the toggle is off.
        let result = deliver_message(dir.path(), "no-such-session", "hello", false).await;
        assert!(result.is_err());
        assert!(inbox::read_pending_messages(dir.path(), "no-such-session").unwrap().is_empty());
    }
}
