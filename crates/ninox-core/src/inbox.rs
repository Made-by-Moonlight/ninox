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

/// Shared cap for both hook payloads: `hookSpecificOutput.additionalContext`
/// is capped at ~10k chars by Claude Code, and `drain_for_stop`'s `reason`
/// uses the same budget for consistency (no equivalent published limit for
/// Stop, but an unbounded `reason` string is exactly the same risk).
pub const MAX_HOOK_PAYLOAD_CHARS: usize = 10_000;

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
    // The sequence is zero-padded: `read_pending_messages` breaks a
    // same-`sent_at` tie with a plain string compare on the id, and
    // unpadded `...-10` sorts before `...-9` lexicographically — padding
    // keeps that tiebreak in actual write order.
    let message = InboxMessage {
        id: format!(
            "msg-{sent_at}-{}-{:010}",
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

/// `session_id` is trusted, ninox-controlled input interpolated directly
/// into a path component — same trust boundary as
/// `hooks::work_requests_dir`'s `{session_id}.requests`. Every caller here
/// is fed a session id ninox itself generated (`slugify`) or already has on
/// record in the store; nothing here parses an id from outside that.
fn inbox_dir(dir: &Path, session_id: &str) -> PathBuf {
    dir.join(format!("{session_id}.inbox"))
}

/// Greedily join `pending` message bodies (oldest first, blank-line
/// separated) up to `max_chars`, returning the joined text and exactly the
/// ids of the messages that made it in. A message that alone exceeds
/// `max_chars` is still included, truncated, rather than left permanently
/// stuck — but nothing after it is added. Anything left out stays pending
/// for the next drain, rather than being silently discarded.
fn drain_batch(pending: Vec<InboxMessage>, max_chars: usize) -> (String, Vec<String>) {
    let mut joined = String::new();
    let mut ids = Vec::new();
    for msg in pending {
        let mut candidate = joined.clone();
        if !candidate.is_empty() {
            candidate.push_str("\n\n");
        }
        candidate.push_str(&msg.text);
        if candidate.chars().count() > max_chars {
            if ids.is_empty() {
                joined = candidate.chars().take(max_chars).collect();
                ids.push(msg.id);
            }
            break;
        }
        joined = candidate;
        ids.push(msg.id);
    }
    (joined, ids)
}

/// Build the Stop hook's response for `session_id`, marking whichever
/// messages made it into the response as delivered. `None` when nothing is
/// pending — the caller should let Claude Code stop normally rather than
/// print anything.
///
/// Repeat Stop invocations in the same turn (Claude Code sets
/// `stop_hook_active` on the hook's stdin payload when it does) need no
/// special handling here: messages are removed from the pending set the
/// moment they're marked delivered, so a second call only ever sees
/// messages that arrived after the first block — never the same batch
/// re-blocking forever.
///
/// Marking happens before this JSON is ever printed by the CLI wrapper
/// (`ninox inbox drain-stop`) — a crash or kill between the two would lose
/// the message despite it being marked delivered here. Delivery is
/// at-most-once, not "never lossy" in the face of a process crash mid-hook;
/// the property this DOES guarantee is that a marking-*failure* (the
/// common case — a concurrent rename losing a race, disk hiccup) never
/// discards a response whose content is already in hand.
pub fn drain_for_stop(dir: &Path, session_id: &str) -> Result<Option<serde_json::Value>> {
    let pending = read_pending_messages(dir, session_id)?;
    if pending.is_empty() {
        return Ok(None);
    }
    let (reason, ids) = drain_batch(pending, MAX_HOOK_PAYLOAD_CHARS);
    // Best-effort: see the "at-most-once" note above — a marking failure
    // must never discard a response whose content is already in hand.
    if let Err(e) = mark_messages_delivered(dir, session_id, &ids) {
        tracing::warn!("failed to mark inbox messages delivered for {session_id}: {e}");
    }
    Ok(Some(serde_json::json!({ "decision": "block", "reason": reason })))
}

/// Build the UserPromptSubmit hook's response for `session_id`, marking
/// whichever messages made it into the response as delivered. `None` when
/// nothing is pending — the human's submitted prompt must pass through
/// completely untouched in that case. Same at-most-once caveat as
/// `drain_for_stop` regarding a process crash between marking and printing.
pub fn drain_for_prompt_submit(dir: &Path, session_id: &str) -> Result<Option<serde_json::Value>> {
    let pending = read_pending_messages(dir, session_id)?;
    if pending.is_empty() {
        return Ok(None);
    }
    let (context, ids) = drain_batch(pending, MAX_HOOK_PAYLOAD_CHARS);
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
    fn drain_for_prompt_submit_truncates_a_single_oversized_message_and_delivers_it() {
        let dir = tempdir().unwrap();
        let long = "x".repeat(MAX_HOOK_PAYLOAD_CHARS + 500);
        write_message(dir.path(), "sess-1", &long).unwrap();

        let response = drain_for_prompt_submit(dir.path(), "sess-1").unwrap().unwrap();
        let context = response["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
        assert_eq!(context.chars().count(), MAX_HOOK_PAYLOAD_CHARS);
        // A message too big to ever fit must still be delivered (truncated)
        // rather than left stuck pending forever.
        assert!(read_pending_messages(dir.path(), "sess-1").unwrap().is_empty());
    }

    #[test]
    fn drain_for_prompt_submit_leaves_overflow_messages_pending_instead_of_discarding_them() {
        let dir = tempdir().unwrap();
        let big = "x".repeat(MAX_HOOK_PAYLOAD_CHARS);
        write_message(dir.path(), "sess-1", &big).unwrap();
        write_message(dir.path(), "sess-1", "second message").unwrap();

        let response = drain_for_prompt_submit(dir.path(), "sess-1").unwrap().unwrap();
        let context = response["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
        assert_eq!(context, big, "only the first message should be in this batch");

        let still_pending = read_pending_messages(dir.path(), "sess-1").unwrap();
        assert_eq!(still_pending.len(), 1, "the overflow message must stay pending, not be lost");
        assert_eq!(still_pending[0].text, "second message");
    }

    #[test]
    fn drain_for_stop_leaves_overflow_messages_pending_instead_of_discarding_them() {
        let dir = tempdir().unwrap();
        let big = "x".repeat(MAX_HOOK_PAYLOAD_CHARS);
        write_message(dir.path(), "sess-1", &big).unwrap();
        write_message(dir.path(), "sess-1", "second message").unwrap();

        let response = drain_for_stop(dir.path(), "sess-1").unwrap().unwrap();
        assert_eq!(response["reason"], big);

        let still_pending = read_pending_messages(dir.path(), "sess-1").unwrap();
        assert_eq!(still_pending.len(), 1, "the overflow message must stay pending, not be lost");
        assert_eq!(still_pending[0].text, "second message");
    }

    #[test]
    fn drain_batch_includes_everything_that_fits() {
        let pending = vec![
            InboxMessage { id: "a".into(), text: "first".into(), sent_at: 1 },
            InboxMessage { id: "b".into(), text: "second".into(), sent_at: 2 },
        ];
        let (joined, ids) = drain_batch(pending, 100);
        assert_eq!(joined, "first\n\nsecond");
        assert_eq!(ids, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn drain_batch_stops_before_a_message_that_would_overflow() {
        let pending = vec![
            InboxMessage { id: "a".into(), text: "12345".into(), sent_at: 1 },
            InboxMessage { id: "b".into(), text: "67890".into(), sent_at: 2 },
        ];
        // "12345" fits; adding "\n\n67890" would exceed the cap.
        let (joined, ids) = drain_batch(pending, 5);
        assert_eq!(joined, "12345");
        assert_eq!(ids, vec!["a".to_string()]);
    }

    #[test]
    fn drain_batch_truncates_a_single_message_that_alone_exceeds_the_cap() {
        let pending = vec![InboxMessage { id: "a".into(), text: "1234567890".into(), sent_at: 1 }];
        let (joined, ids) = drain_batch(pending, 5);
        assert_eq!(joined, "12345");
        assert_eq!(ids, vec!["a".to_string()]);
    }
}
