//! Durable-fleet store tables (spec §5.2): per-session fleet facts that the
//! `sessions` row doesn't carry, outstanding `request-work` items, per-
//! orchestrator recovery bookkeeping, a tiny key/value table for engine
//! flags, and the restore lease. A child module of `store` so it can reach
//! the private connection; every write here is a targeted UPDATE/UPSERT on
//! its own table, so none of it races the full-row `upsert_session`.

use super::Store;
use crate::types::{CIStatus, PrId, SessionStatus};
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::collections::HashMap;

pub(super) const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS fleet_sessions (
        session_id      TEXT PRIMARY KEY,
        task_brief      TEXT,
        branch          TEXT,
        interrupted_at  INTEGER,
        last_status     TEXT,
        interrupt_cause TEXT,
        restored_at     INTEGER,
        restore_mode    TEXT,
        restore_note    TEXT,
        reconciled_terminal_at INTEGER
    );
    CREATE TABLE IF NOT EXISTS work_requests (
        id              TEXT PRIMARY KEY,
        from_session    TEXT NOT NULL,
        orchestrator_id TEXT,
        body            TEXT NOT NULL,
        created_at      INTEGER NOT NULL,
        delivered_at    INTEGER,
        resolved_at     INTEGER
    );
    CREATE TABLE IF NOT EXISTS fleet_recoveries (
        orchestrator_id  TEXT PRIMARY KEY,
        interrupted_at   INTEGER,
        restored_at      INTEGER NOT NULL,
        briefing_sent_at INTEGER,
        acked_at         INTEGER
    );
    CREATE TABLE IF NOT EXISTS fleet_state (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS fleet_lock (
        name        TEXT PRIMARY KEY,
        holder      TEXT NOT NULL,
        acquired_at INTEGER NOT NULL
    );
";

pub(super) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    if !Store::column_exists(conn, "fleet_sessions", "reconciled_terminal_at")? {
        conn.execute("ALTER TABLE fleet_sessions ADD COLUMN reconciled_terminal_at INTEGER", [])?;
    }
    Ok(())
}

/// Fleet facts for one session. Every field is optional: rows exist only
/// for sessions something has recorded a fact about.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct FleetRecord {
    /// The prompt the orchestrator issued at `ninox spawn`, before Ninox's
    /// worker-context footer.
    pub task_brief:      Option<String>,
    /// The worktree's branch as captured right after spawn, then kept in
    /// step with the git wrapper's `checkout -b`/`switch -c` metadata.
    pub branch:          Option<String>,
    /// When the session was found dead by startup reconciliation (for a
    /// reboot: the boot time, the latest moment it can have died).
    pub interrupted_at:  Option<i64>,
    /// The status the session held before reconciliation rewrote it.
    pub last_status:     Option<SessionStatus>,
    /// `fleet::cause::InterruptCause::as_str` at reconciliation time.
    pub interrupt_cause: Option<String>,
    pub restored_at:     Option<i64>,
    /// `fleet::RestoreOutcome::as_str` of the last restore attempt.
    pub restore_mode:    Option<String>,
    pub restore_note:    Option<String>,
    /// The `terminal_at` reconciliation stamped when it wrote `Terminated`
    /// (a harness that can't resume). The session is a fresh-restart
    /// candidate only while its row still carries this exact stamp: any
    /// later relaunch clears `terminal_at` and any later death re-stamps
    /// it, so a session that came back and then ended is never mistaken
    /// for the one reconciliation found dead.
    pub reconciled_terminal_at: Option<i64>,
}

impl FleetRecord {
    /// Interrupted and not restored since. A restore attempt (successful or
    /// not) stamps `restored_at`, so re-running restore never re-launches a
    /// session it already handled for the same interruption.
    pub fn awaiting_restore(&self) -> bool {
        match (self.interrupted_at, self.restored_at) {
            (Some(i), Some(r)) => r < i,
            (Some(_), None)    => true,
            _                  => false,
        }
    }

    /// Whether the last restore attempt belongs to the current interruption
    /// (as opposed to an older one).
    pub fn restored_for_current_interruption(&self) -> bool {
        matches!((self.interrupted_at, self.restored_at), (Some(i), Some(r)) if r >= i)
    }
}

/// One `ninox request-work` item. Mirrors the `{id}.requests/` file the
/// poller delivers from; the file stays the delivery queue, this row is the
/// queryable history.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WorkRequestRow {
    pub id:              String,
    pub from_session:    String,
    pub orchestrator_id: Option<String>,
    pub body:            String,
    pub created_at:      i64,
    pub delivered_at:    Option<i64>,
    pub resolved_at:     Option<i64>,
}

/// Per-orchestrator recovery bookkeeping: one row per orchestrator, rewritten
/// by each restore that touches its fleet.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecoveryRecord {
    pub orchestrator_id:  String,
    pub interrupted_at:   Option<i64>,
    pub restored_at:      i64,
    pub briefing_sent_at: Option<i64>,
    pub acked_at:         Option<i64>,
}

fn status_to_str(s: &SessionStatus) -> String {
    serde_json::to_string(s).unwrap_or_default().replace('"', "")
}

fn status_from_str(s: &str) -> Option<SessionStatus> {
    serde_json::from_str(&format!("\"{s}\"")).ok()
}

fn fleet_record_from_row(r: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<FleetRecord> {
    Ok(FleetRecord {
        task_brief:      r.get(offset)?,
        branch:          r.get(offset + 1)?,
        interrupted_at:  r.get(offset + 2)?,
        last_status:     r.get::<_, Option<String>>(offset + 3)?.as_deref().and_then(status_from_str),
        interrupt_cause: r.get(offset + 4)?,
        restored_at:     r.get(offset + 5)?,
        restore_mode:    r.get(offset + 6)?,
        restore_note:    r.get(offset + 7)?,
        reconciled_terminal_at: r.get(offset + 8)?,
    })
}

const FLEET_COLS: &str = "task_brief, branch, interrupted_at, last_status, interrupt_cause,
                          restored_at, restore_mode, restore_note, reconciled_terminal_at";

fn work_request_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<WorkRequestRow> {
    Ok(WorkRequestRow {
        id:              r.get(0)?,
        from_session:    r.get(1)?,
        orchestrator_id: r.get(2)?,
        body:            r.get(3)?,
        created_at:      r.get(4)?,
        delivered_at:    r.get(5)?,
        resolved_at:     r.get(6)?,
    })
}

const WORK_REQUEST_COLS: &str =
    "id, from_session, orchestrator_id, body, created_at, delivered_at, resolved_at";

fn recovery_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RecoveryRecord> {
    Ok(RecoveryRecord {
        orchestrator_id:  r.get(0)?,
        interrupted_at:   r.get(1)?,
        restored_at:      r.get(2)?,
        briefing_sent_at: r.get(3)?,
        acked_at:         r.get(4)?,
    })
}

const RECOVERY_COLS: &str =
    "orchestrator_id, interrupted_at, restored_at, briefing_sent_at, acked_at";

impl Store {
    // ── fleet_sessions ─────────────────────────────────────────────────────

    /// Record a worker's task brief and (when known) its branch at spawn.
    /// Clears any interruption/restore facts: a session id being spawned is
    /// a fresh incarnation.
    pub fn record_spawn_facts(&self, session_id: &str, task_brief: &str, branch: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO fleet_sessions (session_id, task_brief, branch) VALUES (?1, ?2, ?3)
             ON CONFLICT(session_id) DO UPDATE SET
               task_brief=excluded.task_brief, branch=excluded.branch,
               interrupted_at=NULL, last_status=NULL, interrupt_cause=NULL,
               restored_at=NULL, restore_mode=NULL, restore_note=NULL,
               reconciled_terminal_at=NULL",
            params![session_id, task_brief, branch],
        )?;
        Ok(())
    }

    pub fn set_session_branch(&self, session_id: &str, branch: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO fleet_sessions (session_id, branch) VALUES (?1, ?2)
             ON CONFLICT(session_id) DO UPDATE SET branch=excluded.branch",
            params![session_id, branch],
        )?;
        Ok(())
    }

    /// Stamp a dead session's interruption, keeping the status it held
    /// before reconciliation overwrote it. Clears any earlier
    /// [`FleetRecord::reconciled_terminal_at`]; see
    /// [`mark_reconciled_terminal`](Self::mark_reconciled_terminal).
    pub fn record_interruption(
        &self,
        session_id:  &str,
        at:          i64,
        last_status: &SessionStatus,
        cause:       Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO fleet_sessions (session_id, interrupted_at, last_status, interrupt_cause)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(session_id) DO UPDATE SET
               interrupted_at=excluded.interrupted_at,
               last_status=excluded.last_status,
               interrupt_cause=excluded.interrupt_cause,
               reconciled_terminal_at=NULL",
            params![session_id, at, status_to_str(last_status), cause],
        )?;
        Ok(())
    }

    /// Reconciliation wrote `Terminated` with `terminal_at = at`.
    pub fn mark_reconciled_terminal(&self, session_id: &str, at: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE fleet_sessions SET reconciled_terminal_at=?2 WHERE session_id=?1",
            params![session_id, at],
        )?;
        Ok(())
    }

    /// Forget a session's interruption and restore attempts (it was reaped
    /// or killed on purpose), so no later restore resurrects it. The task
    /// brief and branch stay.
    pub fn clear_interruption(&self, session_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE fleet_sessions SET
               interrupted_at=NULL, last_status=NULL, interrupt_cause=NULL,
               restored_at=NULL, restore_mode=NULL, restore_note=NULL,
               reconciled_terminal_at=NULL
             WHERE session_id=?1",
            [session_id],
        )?;
        Ok(())
    }

    pub fn record_restore(&self, session_id: &str, at: i64, mode: &str, note: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO fleet_sessions (session_id, restored_at, restore_mode, restore_note)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(session_id) DO UPDATE SET
               restored_at=excluded.restored_at,
               restore_mode=excluded.restore_mode,
               restore_note=excluded.restore_note",
            params![session_id, at, mode, note],
        )?;
        Ok(())
    }

    pub fn fleet_record(&self, session_id: &str) -> Result<Option<FleetRecord>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            &format!("SELECT {FLEET_COLS} FROM fleet_sessions WHERE session_id=?1"),
            [session_id],
            |r| fleet_record_from_row(r, 0),
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn fleet_records(&self) -> Result<HashMap<String, FleetRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!("SELECT session_id, {FLEET_COLS} FROM fleet_sessions"))?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, fleet_record_from_row(r, 1)?)))?;
        rows.collect::<rusqlite::Result<HashMap<_, _>>>().map_err(Into::into)
    }

    // ── work_requests ──────────────────────────────────────────────────────

    pub fn insert_work_request(&self, w: &WorkRequestRow) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO work_requests
             (id, from_session, orchestrator_id, body, created_at, delivered_at, resolved_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![w.id, w.from_session, w.orchestrator_id, w.body, w.created_at, w.delivered_at, w.resolved_at],
        )?;
        Ok(())
    }

    /// Only the first delivery is stamped.
    pub fn mark_work_request_delivered(&self, id: &str, at: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE work_requests SET delivered_at=?2 WHERE id=?1 AND delivered_at IS NULL",
            params![id, at],
        )?;
        Ok(())
    }

    /// A recovery briefing listing this orchestrator's open requests was
    /// delivered: the undelivered ones created at or before `up_to` (the
    /// briefing's snapshot time) have now reached it.
    pub fn mark_listed_work_requests_delivered(&self, orchestrator_id: &str, up_to: i64, at: i64) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE work_requests SET delivered_at=?3
             WHERE orchestrator_id=?1 AND resolved_at IS NULL
               AND delivered_at IS NULL AND created_at<=?2",
            params![orchestrator_id, up_to, at],
        )?;
        Ok(n)
    }

    /// Resolve this orchestrator's delivered-but-open requests created at or
    /// before `up_to`. Returns how many were resolved.
    pub fn resolve_delivered_work_requests(&self, orchestrator_id: &str, up_to: i64, at: i64) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE work_requests SET resolved_at=?3
             WHERE orchestrator_id=?1 AND resolved_at IS NULL
               AND delivered_at IS NOT NULL AND created_at<=?2",
            params![orchestrator_id, up_to, at],
        )?;
        Ok(n)
    }

    /// Unresolved requests, oldest first; all orchestrators when `None`.
    pub fn open_work_requests(&self, orchestrator_id: Option<&str>) -> Result<Vec<WorkRequestRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {WORK_REQUEST_COLS} FROM work_requests
             WHERE resolved_at IS NULL AND (?1 IS NULL OR orchestrator_id=?1)
             ORDER BY created_at, id"
        ))?;
        let rows = stmt.query_map([orchestrator_id], work_request_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    // ── fleet_recoveries ───────────────────────────────────────────────────

    /// Open (or reopen) a recovery for `orchestrator_id`; resets the
    /// briefing/ack stamps so the new briefing is delivered and acked anew.
    pub fn begin_recovery(&self, orchestrator_id: &str, interrupted_at: Option<i64>, restored_at: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO fleet_recoveries (orchestrator_id, interrupted_at, restored_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(orchestrator_id) DO UPDATE SET
               interrupted_at=excluded.interrupted_at, restored_at=excluded.restored_at,
               briefing_sent_at=NULL, acked_at=NULL",
            params![orchestrator_id, interrupted_at, restored_at],
        )?;
        Ok(())
    }

    pub fn mark_briefing_sent(&self, orchestrator_id: &str, at: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE fleet_recoveries SET briefing_sent_at=?2 WHERE orchestrator_id=?1",
            params![orchestrator_id, at],
        )?;
        Ok(())
    }

    /// Returns `false` when there is no recovery row to acknowledge.
    pub fn ack_recovery(&self, orchestrator_id: &str, at: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE fleet_recoveries SET acked_at=?2 WHERE orchestrator_id=?1",
            params![orchestrator_id, at],
        )?;
        Ok(n > 0)
    }

    pub fn recovery(&self, orchestrator_id: &str) -> Result<Option<RecoveryRecord>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            &format!("SELECT {RECOVERY_COLS} FROM fleet_recoveries WHERE orchestrator_id=?1"),
            [orchestrator_id],
            recovery_from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn list_recoveries(&self) -> Result<Vec<RecoveryRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {RECOVERY_COLS} FROM fleet_recoveries ORDER BY restored_at"
        ))?;
        let rows = stmt.query_map([], recovery_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    // ── fleet_state (engine flags) ─────────────────────────────────────────

    pub fn fleet_state_get(&self, key: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT value FROM fleet_state WHERE key=?1", [key], |r| r.get(0))
            .optional()
            .map_err(Into::into)
    }

    pub fn fleet_state_set(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO fleet_state (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn fleet_state_delete(&self, key: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM fleet_state WHERE key=?1", [key])?;
        Ok(())
    }

    // ── fleet_lock (restore lease) ─────────────────────────────────────────

    /// Take the named lease for `holder` unless someone else holds a lease
    /// newer than `stale_before`. Atomic in one statement, so two processes
    /// racing to restore can't both win.
    pub fn try_acquire_fleet_lock(&self, name: &str, holder: &str, now: i64, stale_before: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "INSERT INTO fleet_lock (name, holder, acquired_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(name) DO UPDATE SET holder=excluded.holder, acquired_at=excluded.acquired_at
             WHERE fleet_lock.acquired_at < ?4 OR fleet_lock.holder = excluded.holder",
            params![name, holder, now, stale_before],
        )?;
        Ok(n > 0)
    }

    pub fn release_fleet_lock(&self, name: &str, holder: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM fleet_lock WHERE name=?1 AND holder=?2", params![name, holder])?;
        Ok(())
    }

    /// `(holder, acquired_at)` of the current lease, if any.
    pub fn fleet_lock_holder(&self, name: &str) -> Result<Option<(String, i64)>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT holder, acquired_at FROM fleet_lock WHERE name=?1",
            [name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(Into::into)
    }

    // ── read helpers the briefing needs ────────────────────────────────────

    pub fn get_ci_status(&self, pr_id: PrId) -> Result<Option<CIStatus>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT pr_id, total, passing, failing, pending FROM ci_status WHERE pr_id=?1",
            [pr_id],
            |r| Ok(CIStatus {
                pr_id:   r.get(0)?,
                total:   r.get(1)?,
                passing: r.get(2)?,
                failing: r.get(3)?,
                pending: r.get(4)?,
            }),
        )
        .optional()
        .map_err(Into::into)
    }

    /// Fleet rows owned by a deleted session; called from `delete_session`
    /// and `delete_orchestrator` with the connection already locked.
    pub(super) fn purge_fleet_rows(conn: &Connection, session_id: &str) -> Result<()> {
        conn.execute("DELETE FROM fleet_sessions WHERE session_id=?1", [session_id])?;
        conn.execute("DELETE FROM fleet_recoveries WHERE orchestrator_id=?1", [session_id])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn store() -> Store {
        let dir = tempdir().unwrap();
        Store::open(dir.keep().join("t.db")).unwrap()
    }

    #[test]
    fn spawn_facts_round_trip_and_reset_interruption() {
        let s = store();
        s.record_interruption("w1", 100, &SessionStatus::PrOpen, Some("reboot")).unwrap();
        s.record_restore("w1", 200, "resumed", None).unwrap();
        s.record_spawn_facts("w1", "do the thing", Some("w1")).unwrap();
        let r = s.fleet_record("w1").unwrap().unwrap();
        assert_eq!(r.task_brief.as_deref(), Some("do the thing"));
        assert_eq!(r.branch.as_deref(), Some("w1"));
        assert_eq!(r.interrupted_at, None);
        assert_eq!(r.restored_at, None);
    }

    #[test]
    fn interruption_keeps_brief_and_tracks_last_status() {
        let s = store();
        s.record_spawn_facts("w1", "brief", Some("b")).unwrap();
        s.record_interruption("w1", 100, &SessionStatus::CiFailed, Some("reboot")).unwrap();
        let r = s.fleet_record("w1").unwrap().unwrap();
        assert_eq!(r.task_brief.as_deref(), Some("brief"));
        assert_eq!(r.last_status, Some(SessionStatus::CiFailed));
        assert_eq!(r.interrupt_cause.as_deref(), Some("reboot"));
        assert!(r.awaiting_restore());
    }

    #[test]
    fn restore_after_interruption_clears_awaiting_until_next_interruption() {
        let s = store();
        s.record_interruption("w1", 100, &SessionStatus::Working, None).unwrap();
        s.record_restore("w1", 150, "resumed", None).unwrap();
        let r = s.fleet_record("w1").unwrap().unwrap();
        assert!(!r.awaiting_restore());
        assert!(r.restored_for_current_interruption());
        s.record_interruption("w1", 300, &SessionStatus::Working, None).unwrap();
        let r = s.fleet_record("w1").unwrap().unwrap();
        assert!(r.awaiting_restore());
        assert!(!r.restored_for_current_interruption());
    }

    #[test]
    fn work_requests_lifecycle() {
        let s = store();
        let mk = |id: &str, orch: &str, at: i64| WorkRequestRow {
            id: id.into(), from_session: "w1".into(), orchestrator_id: Some(orch.into()),
            body: format!("body {id}"), created_at: at, delivered_at: None, resolved_at: None,
        };
        s.insert_work_request(&mk("a", "o1", 10)).unwrap();
        s.insert_work_request(&mk("b", "o1", 20)).unwrap();
        s.insert_work_request(&mk("c", "o2", 30)).unwrap();
        // Re-insert is ignored, not an error.
        s.insert_work_request(&mk("a", "o1", 10)).unwrap();
        assert_eq!(s.open_work_requests(None).unwrap().len(), 3);
        assert_eq!(s.open_work_requests(Some("o1")).unwrap().len(), 2);

        s.mark_work_request_delivered("a", 15).unwrap();
        s.mark_work_request_delivered("a", 99).unwrap();
        let a = &s.open_work_requests(Some("o1")).unwrap()[0];
        assert_eq!(a.delivered_at, Some(15));

        // Only delivered ones resolve; "b" is undelivered and stays open.
        assert_eq!(s.resolve_delivered_work_requests("o1", 100, 200).unwrap(), 1);
        let open: Vec<_> = s.open_work_requests(Some("o1")).unwrap().into_iter().map(|w| w.id).collect();
        assert_eq!(open, vec!["b".to_string()]);

        // A briefing listing "b" delivers it; one created later is not.
        s.insert_work_request(&mk("late", "o1", 400)).unwrap();
        assert_eq!(s.mark_listed_work_requests_delivered("o1", 300, 310).unwrap(), 1);
        let open = s.open_work_requests(Some("o1")).unwrap();
        assert_eq!(open[0].delivered_at, Some(310));
        assert_eq!(open[1].delivered_at, None);
    }

    #[test]
    fn recovery_begin_send_ack() {
        let s = store();
        assert!(!s.ack_recovery("o1", 1).unwrap());
        s.begin_recovery("o1", Some(5), 10).unwrap();
        s.mark_briefing_sent("o1", 11).unwrap();
        assert!(s.ack_recovery("o1", 12).unwrap());
        let r = s.recovery("o1").unwrap().unwrap();
        assert_eq!((r.briefing_sent_at, r.acked_at), (Some(11), Some(12)));
        s.begin_recovery("o1", Some(50), 60).unwrap();
        let r = s.recovery("o1").unwrap().unwrap();
        assert_eq!((r.briefing_sent_at, r.acked_at, r.restored_at), (None, None, 60));
        assert_eq!(s.list_recoveries().unwrap().len(), 1);
    }

    #[test]
    fn fleet_lock_excludes_other_holders_until_stale() {
        let s = store();
        assert!(s.try_acquire_fleet_lock("restore", "a", 100, 0).unwrap());
        assert!(!s.try_acquire_fleet_lock("restore", "b", 110, 50).unwrap());
        // Same holder may re-enter.
        assert!(s.try_acquire_fleet_lock("restore", "a", 120, 50).unwrap());
        // Stale lease (acquired 120 < stale_before 500) is taken over.
        assert!(s.try_acquire_fleet_lock("restore", "b", 600, 500).unwrap());
        assert_eq!(s.fleet_lock_holder("restore").unwrap().unwrap().0, "b");
        s.release_fleet_lock("restore", "a").unwrap();
        assert!(s.fleet_lock_holder("restore").unwrap().is_some(), "non-holder release is a no-op");
        s.release_fleet_lock("restore", "b").unwrap();
        assert!(s.fleet_lock_holder("restore").unwrap().is_none());
    }

    #[test]
    fn fleet_state_kv() {
        let s = store();
        assert_eq!(s.fleet_state_get("k").unwrap(), None);
        s.fleet_state_set("k", "1").unwrap();
        s.fleet_state_set("k", "2").unwrap();
        assert_eq!(s.fleet_state_get("k").unwrap().as_deref(), Some("2"));
        s.fleet_state_delete("k").unwrap();
        assert_eq!(s.fleet_state_get("k").unwrap(), None);
    }

    #[test]
    fn ci_status_read_back() {
        let s = store();
        assert!(s.get_ci_status(7).unwrap().is_none());
        s.upsert_ci_status(&CIStatus { pr_id: 7, total: 3, passing: 3, failing: 0, pending: 0 }).unwrap();
        assert_eq!(s.get_ci_status(7).unwrap().unwrap().passing, 3);
    }

    #[test]
    fn deleting_a_session_purges_its_fleet_rows() {
        let s = store();
        s.record_spawn_facts("w1", "brief", None).unwrap();
        s.begin_recovery("w1", None, 1).unwrap();
        s.delete_session("w1").unwrap();
        assert!(s.fleet_record("w1").unwrap().is_none());
        assert!(s.recovery("w1").unwrap().is_none());
    }
}
