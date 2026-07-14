//! File-based inbox for orchestrator↔worker messaging (opt-in, see
//! `config::InboxMessagingConfig`). One file per message under
//! `{dir}/{session_id}.inbox/` — same shape as `hooks::append_work_request`'s
//! `{dir}/{session_id}.requests/`, and for the same reason: every writer
//! stays single-owner, so nothing here ever read-modify-writes a file
//! another process could be touching concurrently.
//!
//! Drained by the Stop/UserPromptSubmit hooks installed in a worker's
//! worktree settings (`ninox_app::spawn_util::ensure_statusline_settings`)
//! via [`drain_for_stop`]/[`drain_for_prompt_submit`] — see `crate::messaging`
//! for the write side (`ninox send` / `Engine::send_to_session`).

use anyhow::Result;
use std::path::{Path, PathBuf};

/// `hookSpecificOutput.additionalContext` is capped at ~10k chars by Claude
/// Code — truncate our joined message text to that so the hook payload is
/// never silently rejected.
pub const MAX_ADDITIONAL_CONTEXT_CHARS: usize = 10_000;

#[derive(Debug, Clone, PartialEq)]
pub struct InboxMessage {
    pub id:      String,
    pub text:    String,
    pub sent_at: i64,
}

/// Write `text` as a new pending message for `session_id`. Atomic
/// tmp-then-rename, mirroring `hooks::append_work_request`.
pub fn write_message(dir: &Path, session_id: &str, text: &str) -> Result<InboxMessage> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);

    let sent_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    // pid + per-process sequence make the id unique even when two callers
    // (orchestrator send + a poller reaction) write in the same millisecond.
    let message = InboxMessage {
        id: format!(
            "msg-{sent_at}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
        ),
        text: text.to_string(),
        sent_at,
    };

    let dir = inbox_dir(dir, session_id);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.json", message.id));
    let body = serde_json::json!({
        "id":     message.id,
        "text":   message.text,
        "sentAt": message.sent_at,
    });
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&body)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(message)
}

/// Messages not yet delivered, oldest first.
pub fn read_pending_messages(dir: &Path, session_id: &str) -> Result<Vec<InboxMessage>> {
    let dir = inbox_dir(dir, session_id);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e)  => e,
        Err(_) => return Ok(Vec::new()), // no directory → nothing pending
    };
    let mut messages: Vec<InboxMessage> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|e| {
            let raw = std::fs::read_to_string(e.path()).ok()?;
            let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
            Some(InboxMessage {
                id:      v.get("id")?.as_str()?.to_string(),
                text:    v.get("text")?.as_str()?.to_string(),
                sent_at: v.get("sentAt").and_then(|t| t.as_i64()).unwrap_or(0),
            })
        })
        .collect();
    messages.sort_by(|a, b| a.sent_at.cmp(&b.sent_at).then(a.id.cmp(&b.id)));
    Ok(messages)
}

/// Mark messages delivered by renaming their file out of the pending set
/// (`.json` → `.json.delivered`), kept on disk as an audit trail. Best-effort
/// per id, mirroring `hooks::mark_work_requests_delivered`: one failed
/// rename must not leave later ids pending.
pub fn mark_messages_delivered(dir: &Path, session_id: &str, ids: &[String]) -> Result<()> {
    let dir = inbox_dir(dir, session_id);
    let mut first_err: Option<std::io::Error> = None;
    for id in ids {
        let path = dir.join(format!("{id}.json"));
        if path.exists() {
            let delivered = dir.join(format!("{id}.json.delivered"));
            if let Err(e) = std::fs::rename(&path, &delivered) {
                first_err.get_or_insert(e);
            }
        }
    }
    match first_err {
        Some(e) => Err(e.into()),
        None    => Ok(()),
    }
}

fn inbox_dir(dir: &Path, session_id: &str) -> PathBuf {
    dir.join(format!("{session_id}.inbox"))
}

/// Join pending message bodies into one blob, oldest first, separated by a
/// blank line. Shared by both hook-response builders below.
fn join_pending(pending: &[InboxMessage]) -> String {
    pending.iter().map(|m| m.text.as_str()).collect::<Vec<_>>().join("\n\n")
}

/// Build the Stop hook's response for `session_id`, marking whatever is
/// currently pending as delivered. `None` when nothing is pending — the
/// caller should let Claude Code stop normally rather than print anything.
///
/// Repeat Stop invocations in the same turn (Claude Code sets
/// `stop_hook_active` on the hook's stdin payload when it does) need no
/// special handling here: messages are removed from the pending set the
/// moment they're marked delivered, so a second call only ever sees
/// messages that arrived after the first block — never the same batch
/// re-blocking forever.
pub fn drain_for_stop(dir: &Path, session_id: &str) -> Result<Option<serde_json::Value>> {
    let pending = read_pending_messages(dir, session_id)?;
    if pending.is_empty() {
        return Ok(None);
    }
    let reason = join_pending(&pending);
    let ids: Vec<String> = pending.into_iter().map(|m| m.id).collect();
    // Best-effort: a marking failure must never discard a response whose
    // content is already in hand — that would silently drop a message the
    // model was about to see. Worst case on failure is the same message
    // getting redelivered next turn, which is safe (never lossy).
    if let Err(e) = mark_messages_delivered(dir, session_id, &ids) {
        tracing::warn!("failed to mark inbox messages delivered for {session_id}: {e}");
    }
    Ok(Some(serde_json::json!({ "decision": "block", "reason": reason })))
}

/// Build the UserPromptSubmit hook's response for `session_id`, marking
/// whatever is currently pending as delivered. `None` when nothing is
/// pending — the human's submitted prompt must pass through completely
/// untouched in that case.
pub fn drain_for_prompt_submit(dir: &Path, session_id: &str) -> Result<Option<serde_json::Value>> {
    let pending = read_pending_messages(dir, session_id)?;
    if pending.is_empty() {
        return Ok(None);
    }
    let mut context = join_pending(&pending);
    if context.chars().count() > MAX_ADDITIONAL_CONTEXT_CHARS {
        context = context.chars().take(MAX_ADDITIONAL_CONTEXT_CHARS).collect();
    }
    let ids: Vec<String> = pending.into_iter().map(|m| m.id).collect();
    // Best-effort, same reasoning as drain_for_stop: never drop an
    // already-built response over a marking failure.
    if let Err(e) = mark_messages_delivered(dir, session_id, &ids) {
        tracing::warn!("failed to mark inbox messages delivered for {session_id}: {e}");
    }
    Ok(Some(serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": context,
        }
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn write_then_read_returns_the_message() {
        let dir = tempdir().unwrap();
        let written = write_message(dir.path(), "sess-1", "hello worker").unwrap();
        let pending = read_pending_messages(dir.path(), "sess-1").unwrap();
        assert_eq!(pending, vec![written]);
    }

    #[test]
    fn read_pending_on_missing_dir_returns_empty() {
        let dir = tempdir().unwrap();
        assert!(read_pending_messages(dir.path(), "no-such-session").unwrap().is_empty());
    }

    #[test]
    fn messages_are_returned_oldest_first() {
        let dir = tempdir().unwrap();
        let a = write_message(dir.path(), "sess-1", "first").unwrap();
        let b = write_message(dir.path(), "sess-1", "second").unwrap();
        let pending = read_pending_messages(dir.path(), "sess-1").unwrap();
        assert_eq!(pending, vec![a, b]);
    }

    #[test]
    fn delivered_messages_are_no_longer_pending() {
        let dir = tempdir().unwrap();
        let msg = write_message(dir.path(), "sess-1", "hello").unwrap();
        mark_messages_delivered(dir.path(), "sess-1", std::slice::from_ref(&msg.id)).unwrap();
        assert!(read_pending_messages(dir.path(), "sess-1").unwrap().is_empty());
        // Kept on disk as an audit trail, just renamed out of the pending set.
        let dir_path = inbox_dir(dir.path(), "sess-1");
        assert!(dir_path.join(format!("{}.json.delivered", msg.id)).exists());
    }

    #[test]
    fn messages_for_different_sessions_do_not_collide() {
        let dir = tempdir().unwrap();
        write_message(dir.path(), "sess-1", "for one").unwrap();
        write_message(dir.path(), "sess-2", "for two").unwrap();
        assert_eq!(read_pending_messages(dir.path(), "sess-1").unwrap().len(), 1);
        assert_eq!(read_pending_messages(dir.path(), "sess-2").unwrap().len(), 1);
    }

    #[test]
    fn drain_for_stop_returns_none_when_nothing_pending() {
        let dir = tempdir().unwrap();
        assert!(drain_for_stop(dir.path(), "sess-1").unwrap().is_none());
    }

    #[test]
    fn drain_for_stop_blocks_with_joined_reason_and_delivers() {
        let dir = tempdir().unwrap();
        write_message(dir.path(), "sess-1", "first").unwrap();
        write_message(dir.path(), "sess-1", "second").unwrap();

        let response = drain_for_stop(dir.path(), "sess-1").unwrap().unwrap();
        assert_eq!(response["decision"], "block");
        assert_eq!(response["reason"], "first\n\nsecond");
        assert!(read_pending_messages(dir.path(), "sess-1").unwrap().is_empty());
    }

    #[test]
    fn drain_for_stop_is_self_limiting_across_repeat_invocations() {
        // Simulates Claude Code calling Stop again with stop_hook_active —
        // the second call must not re-block on the same already-delivered
        // batch.
        let dir = tempdir().unwrap();
        write_message(dir.path(), "sess-1", "only message").unwrap();
        assert!(drain_for_stop(dir.path(), "sess-1").unwrap().is_some());
        assert!(drain_for_stop(dir.path(), "sess-1").unwrap().is_none());
    }

    #[test]
    fn drain_for_prompt_submit_returns_none_when_nothing_pending() {
        let dir = tempdir().unwrap();
        assert!(drain_for_prompt_submit(dir.path(), "sess-1").unwrap().is_none());
    }

    #[test]
    fn drain_for_prompt_submit_shapes_hook_specific_output_and_delivers() {
        let dir = tempdir().unwrap();
        write_message(dir.path(), "sess-1", "context for the next turn").unwrap();

        let response = drain_for_prompt_submit(dir.path(), "sess-1").unwrap().unwrap();
        assert_eq!(response["hookSpecificOutput"]["hookEventName"], "UserPromptSubmit");
        assert_eq!(
            response["hookSpecificOutput"]["additionalContext"],
            "context for the next turn"
        );
        assert!(read_pending_messages(dir.path(), "sess-1").unwrap().is_empty());
    }

    #[test]
    fn drain_for_prompt_submit_truncates_to_the_additional_context_cap() {
        let dir = tempdir().unwrap();
        let long = "x".repeat(MAX_ADDITIONAL_CONTEXT_CHARS + 500);
        write_message(dir.path(), "sess-1", &long).unwrap();

        let response = drain_for_prompt_submit(dir.path(), "sess-1").unwrap().unwrap();
        let context = response["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
        assert_eq!(context.chars().count(), MAX_ADDITIONAL_CONTEXT_CHARS);
    }
}
