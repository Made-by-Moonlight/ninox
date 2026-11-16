//! Delivery bookkeeping for `crate::messaging`: per-attempt outcomes in
//! `message_events`, per-session sent/received timestamps for stall
//! detection, and retry state for messages held because their target was
//! unreachable.

use super::Store;
use crate::store::HeldMessage;
use crate::types::{MessageEvent, MessageKind, MessageOutcome};
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

/// `message_events` rows kept; old ones are pruned on insert. Large enough
/// that `ninox messages --to-me --since` still covers a working day.
const MESSAGE_EVENTS_KEPT: i64 = 2000;

fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names: Vec<String> = stmt.query_map([], |r| r.get(1))?.collect::<rusqlite::Result<_>>()?;
    Ok(names.iter().any(|n| n == column))
}

pub(super) fn migrate(conn: &Connection) -> Result<()> {
    // One transaction: if the column landed without its backfill, the next
    // open would see the column and never backfill.
    let tx = conn.unchecked_transaction()?;
    let conn: &Connection = &tx;
    let stall_clock_is_new = !column_exists(conn, "session_messages", "last_sent_at")?;
    for (table, col, ddl) in [
        ("message_events", "outcome", "ALTER TABLE message_events ADD COLUMN outcome TEXT"),
        ("message_events", "mechanism", "ALTER TABLE message_events ADD COLUMN mechanism TEXT"),
        ("message_events", "detail", "ALTER TABLE message_events ADD COLUMN detail TEXT"),
        ("session_messages", "last_sent_at", "ALTER TABLE session_messages ADD COLUMN last_sent_at INTEGER"),
        ("session_messages", "last_received_at", "ALTER TABLE session_messages ADD COLUMN last_received_at INTEGER"),
        ("session_messages", "stall_nudged_for", "ALTER TABLE session_messages ADD COLUMN stall_nudged_for INTEGER"),
        ("held_messages", "reason", "ALTER TABLE held_messages ADD COLUMN reason TEXT NOT NULL DEFAULT 'queued'"),
        ("held_messages", "attempts", "ALTER TABLE held_messages ADD COLUMN attempts INTEGER NOT NULL DEFAULT 0"),
        ("held_messages", "next_attempt_at", "ALTER TABLE held_messages ADD COLUMN next_attempt_at INTEGER NOT NULL DEFAULT 0"),
        ("held_messages", "last_error", "ALTER TABLE held_messages ADD COLUMN last_error TEXT"),
    ] {
        if !column_exists(conn, table, col)? {
            conn.execute(ddl, [])?;
        }
    }
    if stall_clock_is_new {
        // Older releases never recorded sends, so a NULL `last_sent_at` would
        // read as "owes a reply" and nudge every idle worker on first start.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        conn.execute(
            "INSERT INTO session_messages (session_id, delivered_count, last_sent_at)
             SELECT id, 0, ?1 FROM sessions WHERE true
             ON CONFLICT(session_id) DO UPDATE SET last_sent_at = excluded.last_sent_at",
            params![now],
        )?;
    }
    tx.commit()?;
    Ok(())
}

fn kind_str(kind: MessageKind) -> Result<String> {
    Ok(serde_json::to_string(&kind)?.replace('"', ""))
}

fn parse_kind(s: Option<String>) -> MessageKind {
    s.and_then(|s| serde_json::from_str(&format!("\"{s}\"")).ok()).unwrap_or_default()
}

/// A message held because its target couldn't be reached, due for retry.
#[derive(Debug, Clone)]
pub struct UnreachableHeld {
    pub seq:        i64,
    pub to:         String,
    pub message:    HeldMessage,
    pub attempts:   u32,
    pub created_at: i64,
}

/// Who still owes whom a message: see `Poller::check_message_stalls`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MessageClock {
    pub last_sent_at:         Option<i64>,
    pub last_received_at:     Option<i64>,
    pub stall_nudged_for: Option<i64>,
}

impl Store {
    /// Records one delivery attempt and returns its row id (the receipt's
    /// message id).
    #[allow(clippy::too_many_arguments)]
    pub fn record_message_attempt(
        &self,
        from: Option<&str>,
        to: &str,
        text: &str,
        kind: MessageKind,
        outcome: MessageOutcome,
        mechanism: Option<&str>,
        detail: Option<&str>,
        at: i64,
    ) -> Result<i64> {
        let kind = kind_str(kind)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO message_events (from_id, to_id, text, delivered_at, kind, outcome, mechanism, detail)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![from, to, text, at, kind, outcome.as_str(), mechanism, detail],
        )?;
        let id = conn.last_insert_rowid();
        conn.execute(
            "DELETE FROM message_events WHERE id <= (SELECT MAX(id) FROM message_events) - ?1",
            params![MESSAGE_EVENTS_KEPT],
        )?;
        Ok(id)
    }

    /// Every attempt addressed to `to` at or after `since_ms`, oldest first.
    pub fn message_events_to(&self, to: &str, since_ms: i64) -> Result<Vec<MessageEvent>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, from_id, to_id, text, delivered_at, kind, outcome FROM message_events
             WHERE to_id = ?1 AND delivered_at >= ?2 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![to, since_ms], |r| {
            Ok(MessageEvent {
                id:           r.get(0)?,
                from_id:      r.get(1)?,
                to_id:        r.get(2)?,
                text:         r.get(3)?,
                delivered_at: r.get(4)?,
                kind:         parse_kind(r.get(5)?),
                outcome:      MessageOutcome::parse(r.get::<_, Option<String>>(6)?.as_deref()),
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn note_message_sent(&self, session_id: &str, at: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO session_messages (session_id, delivered_count, last_sent_at) VALUES (?1, 0, ?2)
             ON CONFLICT(session_id) DO UPDATE SET last_sent_at = MAX(COALESCE(last_sent_at, 0), excluded.last_sent_at)",
            params![session_id, at],
        )?;
        Ok(())
    }

    pub fn note_message_received(&self, session_id: &str, at: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO session_messages (session_id, delivered_count, last_received_at) VALUES (?1, 0, ?2)
             ON CONFLICT(session_id) DO UPDATE SET last_received_at = MAX(COALESCE(last_received_at, 0), excluded.last_received_at)",
            params![session_id, at],
        )?;
        Ok(())
    }

    pub fn message_clock(&self, session_id: &str) -> Result<MessageClock> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT last_sent_at, last_received_at, stall_nudged_for FROM session_messages WHERE session_id = ?1",
                params![session_id],
                |r| Ok(MessageClock { last_sent_at: r.get(0)?, last_received_at: r.get(1)?, stall_nudged_for: r.get(2)? }),
            )
            .optional()?
            .unwrap_or_default())
    }

    /// Marks the owed message (by its `last_received_at`) as nudged about.
    /// True only for the caller that made the change, so two pollers never
    /// both nudge for the same message.
    pub fn claim_stall_nudge(&self, session_id: &str, received_at: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "INSERT INTO session_messages (session_id, delivered_count, stall_nudged_for) VALUES (?1, 0, ?2)
             ON CONFLICT(session_id) DO UPDATE SET stall_nudged_for = excluded.stall_nudged_for
             WHERE stall_nudged_for IS NULL OR stall_nudged_for <> excluded.stall_nudged_for",
            params![session_id, received_at],
        )?;
        Ok(changed == 1)
    }

    /// Holds a message whose target couldn't be reached, for
    /// `Poller::retry_unreachable_messages`.
    pub fn hold_unreachable(&self, to: &str, msg: &HeldMessage, error: &str, now: i64, first_retry_at: i64) -> Result<()> {
        let kind = kind_str(msg.kind)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO held_messages (to_id, tagged, attributed_from, defanged, kind, created_at, reason, attempts, next_attempt_at, last_error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'unreachable', 1, ?7, ?8)",
            params![to, msg.tagged, msg.attributed_from, msg.defanged, kind, now, first_retry_at, error],
        )?;
        Ok(())
    }

    /// Every unreachable-held message of each target whose *oldest* one is
    /// due, in send order per target, so a retry never overtakes an older
    /// message that is still backing off.
    pub fn due_unreachable_messages(&self, now: i64) -> Result<Vec<UnreachableHeld>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT h.seq, h.to_id, h.tagged, h.attributed_from, h.defanged, h.kind, h.attempts, h.created_at
             FROM held_messages AS h
             JOIN (SELECT to_id, MIN(seq) AS first FROM held_messages WHERE reason = 'unreachable' GROUP BY to_id) AS o
               ON o.to_id = h.to_id
             JOIN held_messages AS f ON f.seq = o.first
             WHERE h.reason = 'unreachable' AND f.next_attempt_at <= ?1
             ORDER BY h.to_id, h.seq",
        )?;
        let rows = stmt.query_map(params![now], |r| {
            Ok(UnreachableHeld {
                seq:        r.get(0)?,
                to:         r.get(1)?,
                message:    HeldMessage {
                    tagged:          r.get(2)?,
                    attributed_from: r.get(3)?,
                    defanged:        r.get(4)?,
                    kind:            parse_kind(r.get(5)?),
                },
                attempts:   r.get::<_, i64>(6)?.max(0) as u32,
                created_at: r.get(7)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Takes a held message for one delivery attempt: true only for the one
    /// process whose update moved it from due to leased.
    pub fn claim_held_message(&self, seq: i64, now: i64, lease_until: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE held_messages SET next_attempt_at = ?3 WHERE seq = ?1 AND next_attempt_at <= ?2",
            params![seq, now, lease_until],
        )?;
        Ok(changed == 1)
    }

    pub fn has_unreachable_held(&self, to: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row("SELECT 1 FROM held_messages WHERE to_id = ?1 AND reason = 'unreachable' LIMIT 1", params![to], |_| Ok(()))
            .optional()?
            .is_some())
    }

    pub fn delete_held_message(&self, seq: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM held_messages WHERE seq = ?1", params![seq])?;
        Ok(())
    }

    pub fn reschedule_held_message(&self, seq: i64, next_attempt_at: i64, error: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE held_messages SET attempts = attempts + 1, next_attempt_at = ?2, last_error = ?3 WHERE seq = ?1",
            params![seq, next_attempt_at, error],
        )?;
        Ok(())
    }

    /// Held-message counts per target, split into (waiting for a queued
    /// session to start, waiting for an unreachable target), for the UI.
    pub fn held_message_counts(&self) -> Result<std::collections::HashMap<String, (usize, usize)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT to_id, SUM(reason <> 'unreachable'), SUM(reason = 'unreachable') FROM held_messages GROUP BY to_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, (r.get::<_, i64>(1)?.max(0) as usize, r.get::<_, i64>(2)?.max(0) as usize)))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Messages each sender has waiting on an unreachable target, so the
    /// sender's card can say its message is queued.
    pub fn unreachable_held_by_sender(&self) -> Result<std::collections::HashMap<String, usize>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT attributed_from, COUNT(*) FROM held_messages
             WHERE reason = 'unreachable' AND attributed_from IS NOT NULL GROUP BY attributed_from",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?.max(0) as usize)))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// The text of messages still held for `to`, oldest first, for
    /// `ninox messages --to-me`.
    pub fn held_messages_for(&self, to: &str) -> Result<Vec<(i64, String, Option<String>)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT created_at, defanged, attributed_from FROM held_messages WHERE to_id = ?1 ORDER BY seq",
        )?;
        let rows = stmt.query_map(params![to], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap()
    }

    fn held(text: &str) -> HeldMessage {
        HeldMessage { tagged: format!("[freeform] {text}"), attributed_from: Some("w1".into()), defanged: text.into(), kind: MessageKind::Freeform }
    }

    #[test]
    fn attempts_record_their_outcome_and_return_an_id() {
        let _iso = crate::test_isolation::isolate();
        let s = store();
        let a = s.record_message_attempt(Some("w1"), "orch", "hi", MessageKind::Status, MessageOutcome::Delivered, Some("session_socket"), None, 10).unwrap();
        let b = s.record_message_attempt(Some("w1"), "orch", "again", MessageKind::Status, MessageOutcome::Queued, None, Some("no pane"), 20).unwrap();
        assert!(b > a);
        let events = s.message_events_to("orch", 0).unwrap();
        assert_eq!(events.iter().map(|e| e.outcome).collect::<Vec<_>>(), vec![MessageOutcome::Delivered, MessageOutcome::Queued]);
        assert_eq!(s.message_events_to("orch", 15).unwrap().len(), 1, "since filters by time");
        assert!(s.message_events_to("w1", 0).unwrap().is_empty());
    }

    #[test]
    fn message_clock_keeps_the_latest_times() {
        let _iso = crate::test_isolation::isolate();
        let s = store();
        assert_eq!(s.message_clock("w1").unwrap(), MessageClock::default());
        s.note_message_received("w1", 50).unwrap();
        s.note_message_sent("w1", 40).unwrap();
        s.note_message_sent("w1", 30).unwrap();
        assert!(s.claim_stall_nudge("w1", 7).unwrap());
        assert!(!s.claim_stall_nudge("w1", 7).unwrap(), "a second poller loses the claim");
        assert_eq!(
            s.message_clock("w1").unwrap(),
            MessageClock { last_sent_at: Some(40), last_received_at: Some(50), stall_nudged_for: Some(7) },
        );
    }

    #[test]
    fn unreachable_messages_are_retried_on_schedule() {
        let _iso = crate::test_isolation::isolate();
        let s = store();
        s.hold_unreachable("orch", &held("report"), "no pane", 100, 200).unwrap();
        assert!(s.due_unreachable_messages(150).unwrap().is_empty(), "not due yet");
        let due = s.due_unreachable_messages(200).unwrap();
        assert_eq!(due.len(), 1);
        assert!(s.claim_held_message(due[0].seq, 200, 1_000).unwrap());
        assert!(!s.claim_held_message(due[0].seq, 200, 1_000).unwrap(), "claimed once");
        assert_eq!((due[0].to.as_str(), due[0].attempts, due[0].message.defanged.as_str()), ("orch", 1, "report"));
        s.reschedule_held_message(due[0].seq, 500, "still no pane").unwrap();
        // A newer message for the same target waits behind the backing-off one.
        s.hold_unreachable("orch", &held("second"), "behind", 210, 210).unwrap();
        assert!(s.due_unreachable_messages(400).unwrap().is_empty(), "the newer one never overtakes");
        assert_eq!(s.due_unreachable_messages(500).unwrap().len(), 2);
        s.delete_held_message(s.due_unreachable_messages(500).unwrap()[1].seq).unwrap();
        assert!(s.due_unreachable_messages(400).unwrap().is_empty());
        assert_eq!(s.due_unreachable_messages(500).unwrap()[0].attempts, 2);
        assert_eq!(s.held_message_counts().unwrap()["orch"], (0, 1));
        assert_eq!(s.unreachable_held_by_sender().unwrap()["w1"], 1);
        assert_eq!(s.held_messages_for("orch").unwrap()[0].1, "report");
        s.delete_held_message(due[0].seq).unwrap();
        assert!(s.held_message_counts().unwrap().is_empty());
    }
}
