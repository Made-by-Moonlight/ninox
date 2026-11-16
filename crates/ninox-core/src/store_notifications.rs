//! Persistence for `Event::Notification`: every notification the engine
//! emits is written here so clients that don't host the poller (a GUI or TUI
//! beside a headless daemon) read them from the store, and read/dismiss state
//! is shared across clients.

use super::Store;
use crate::types::{Notification, NotificationKind};
use anyhow::Result;
use rusqlite::{params, Connection};

pub(super) const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS notifications (
        seq          INTEGER PRIMARY KEY AUTOINCREMENT,
        id           TEXT NOT NULL,
        kind         TEXT NOT NULL,
        title        TEXT NOT NULL,
        body         TEXT NOT NULL,
        session_id   TEXT,
        created_at   INTEGER NOT NULL,
        dismissed_at INTEGER
    );
    CREATE UNIQUE INDEX IF NOT EXISTS notifications_live_id ON notifications (id) WHERE dismissed_at IS NULL;
";

pub const MAX_ROWS: i64 = 200;
/// Retention runs every this many inserts, not on each one.
const SWEEP_EVERY: u32 = 20;
static INSERTS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

pub const MAX_AGE_MS: i64 = 7 * 24 * 60 * 60 * 1000;

pub(super) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    Ok(())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A stored notification with its monotonic insertion sequence.
#[derive(Debug, Clone)]
pub struct StoredNotification {
    pub seq:          i64,
    pub notification: Notification,
}

fn kind_to_str(kind: &NotificationKind) -> Result<String> {
    Ok(serde_json::to_value(kind)?.as_str().unwrap_or_default().to_string())
}

fn kind_from_str(s: &str) -> Option<NotificationKind> {
    serde_json::from_value(serde_json::Value::String(s.to_string())).ok()
}

impl Store {
    /// Inserts `n` as a new row (fresh seq, so clients alert once). An
    /// undismissed row with the same id is replaced rather than kept: ids
    /// like `ci-<session>` are reused by each occurrence, and the later one
    /// must alert and show its own body. Every [`SWEEP_EVERY`]th insert also prunes
    /// rows older than [`MAX_AGE_MS`] or beyond the newest [`MAX_ROWS`].
    pub fn insert_notification(&self, n: &Notification) -> Result<()> {
        let kind = kind_to_str(&n.kind)?;
        let now = now_ms();
        let created_at = if n.created_at > 0 { n.created_at } else { now };
        let conn = self.conn.lock().unwrap();
        conn.prepare_cached("DELETE FROM notifications WHERE id = ?1 AND dismissed_at IS NULL")?.execute(params![n.id])?;
        conn.prepare_cached(
            "INSERT INTO notifications (id, kind, title, body, session_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?
        .execute(params![n.id, kind, n.title, n.body, n.session_id, created_at])?;
        if INSERTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed).is_multiple_of(SWEEP_EVERY) {
            Self::prune_notifications_locked(&conn, now)?;
        }
        Ok(())
    }

    fn prune_notifications_locked(conn: &Connection, now: i64) -> Result<()> {
        conn.execute("DELETE FROM notifications WHERE created_at < ?1", params![now - MAX_AGE_MS])?;
        conn.execute(
            "DELETE FROM notifications WHERE seq <= (SELECT MAX(seq) FROM notifications) - ?1",
            params![MAX_ROWS],
        )?;
        Ok(())
    }

    pub fn prune_notifications(&self) -> Result<()> {
        Self::prune_notifications_locked(&self.conn.lock().unwrap(), now_ms())
    }

    /// Undismissed notifications with `seq > after`, oldest first. Rows whose
    /// kind this build doesn't know (a newer writer) are skipped.
    pub fn undismissed_notifications_after(&self, after: i64) -> Result<Vec<StoredNotification>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT seq, id, kind, title, body, session_id, created_at FROM notifications
             WHERE dismissed_at IS NULL AND seq > ?1 ORDER BY seq",
        )?;
        let rows = stmt
            .query_map(params![after], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, Option<String>>(5)?, r.get::<_, i64>(6)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(seq, id, kind, title, body, session_id, created_at)| {
                Some(StoredNotification {
                    seq,
                    notification: Notification { id, kind: kind_from_str(&kind)?, title, body, session_id, created_at },
                })
            })
            .collect())
    }

    pub fn undismissed_notifications(&self) -> Result<Vec<StoredNotification>> {
        self.undismissed_notifications_after(0)
    }

    /// Highest sequence ever assigned (0 when none), dismissed rows included.
    pub fn max_notification_seq(&self) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM notifications", [], |r| r.get(0))?)
    }

    pub fn dismiss_notification(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE notifications SET dismissed_at = ?2 WHERE id = ?1 AND dismissed_at IS NULL",
            params![id, now_ms()],
        )?;
        Ok(())
    }

    /// Dismisses every undismissed row up to `through_seq`: what the client
    /// has shown, not rows that landed since its last read.
    pub fn dismiss_notifications_through(&self, through_seq: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE notifications SET dismissed_at = ?1 WHERE dismissed_at IS NULL AND seq <= ?2",
            params![now_ms(), through_seq],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(id: &str) -> Notification {
        Notification {
            id:         id.into(),
            kind:       NotificationKind::CiFailure,
            title:      format!("t-{id}"),
            body:       "b".into(),
            session_id: Some("s1".into()),
            created_at: now_ms(),
        }
    }

    fn store() -> Store {
        Store::open(":memory:").unwrap()
    }

    #[test]
    fn a_live_id_is_replaced_by_its_next_occurrence_not_duplicated() {
        let s = store();
        s.insert_notification(&note("a")).unwrap();
        s.insert_notification(&note("b")).unwrap();
        s.insert_notification(&note("a")).unwrap();
        let ids: Vec<_> = s.undismissed_notifications().unwrap().into_iter().map(|r| r.notification.id).collect();
        assert_eq!(ids, ["b", "a"], "a live id is replaced, moving to the newest seq");
    }

    #[test]
    fn a_condition_recurring_after_dismissal_appears_again_with_a_new_seq() {
        let s = store();
        s.insert_notification(&note("a")).unwrap();
        let first = s.max_notification_seq().unwrap();
        s.dismiss_notification("a").unwrap();
        s.insert_notification(&note("a")).unwrap();
        let rows = s.undismissed_notifications_after(first).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].seq > first, "new seq so a client alerts once");
    }

    #[test]
    fn dismiss_and_dismiss_all_persist_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        {
            let s = Store::open(&path).unwrap();
            for id in ["a", "b", "c"] {
                s.insert_notification(&note(id)).unwrap();
            }
            s.dismiss_notification("b").unwrap();
        }
        let s = Store::open(&path).unwrap();
        let ids: Vec<_> = s.undismissed_notifications().unwrap().into_iter().map(|r| r.notification.id).collect();
        assert_eq!(ids, ["a", "c"]);
        s.dismiss_notifications_through(i64::MAX).unwrap();
        drop(s);
        assert!(Store::open(&path).unwrap().undismissed_notifications().unwrap().is_empty());
    }

    #[test]
    fn after_cursor_returns_only_newer_rows_and_max_seq_survives_dismissal() {
        let s = store();
        s.insert_notification(&note("a")).unwrap();
        let cursor = s.max_notification_seq().unwrap();
        s.insert_notification(&note("b")).unwrap();
        let newer: Vec<_> = s.undismissed_notifications_after(cursor).unwrap().into_iter().map(|r| r.notification.id).collect();
        assert_eq!(newer, ["b"]);
        s.dismiss_notifications_through(i64::MAX).unwrap();
        assert_eq!(s.max_notification_seq().unwrap(), cursor + 1);
    }

    #[test]
    fn retention_drops_rows_past_the_cap_and_past_seven_days() {
        let s = store();
        let mut old = note("old");
        old.created_at = now_ms() - MAX_AGE_MS - 1000;
        s.insert_notification(&old).unwrap();
        s.insert_notification(&note("fresh")).unwrap();
        s.prune_notifications().unwrap();
        let ids: Vec<_> = s.undismissed_notifications().unwrap().into_iter().map(|r| r.notification.id).collect();
        assert_eq!(ids, ["fresh"], "the stale row went on the next insert");
        for i in 0..(MAX_ROWS + 5) {
            s.insert_notification(&note(&format!("n{i}"))).unwrap();
        }
        s.prune_notifications().unwrap();
        let rows = s.undismissed_notifications().unwrap();
        assert_eq!(rows.len() as i64, MAX_ROWS);
        assert_eq!(rows.last().unwrap().notification.id, format!("n{}", MAX_ROWS + 4));
    }

    #[test]
    fn dismiss_through_spares_rows_newer_than_what_the_client_saw() {
        let s = store();
        s.insert_notification(&note("a")).unwrap();
        let seen = s.max_notification_seq().unwrap();
        s.insert_notification(&note("b")).unwrap();
        s.dismiss_notifications_through(seen).unwrap();
        let ids: Vec<_> = s.undismissed_notifications().unwrap().into_iter().map(|r| r.notification.id).collect();
        assert_eq!(ids, ["b"]);
    }

    #[test]
    fn an_unknown_kind_from_a_newer_writer_is_skipped_not_fatal() {
        let s = store();
        s.insert_notification(&note("a")).unwrap();
        s.conn.lock().unwrap().execute("UPDATE notifications SET kind = 'some_future_kind' WHERE id = 'a'", []).unwrap();
        s.insert_notification(&note("b")).unwrap();
        let ids: Vec<_> = s.undismissed_notifications().unwrap().into_iter().map(|r| r.notification.id).collect();
        assert_eq!(ids, ["b"]);
    }
}
