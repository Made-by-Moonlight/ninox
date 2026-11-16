//! Harness login failures seen anywhere in the fleet (see `harness_auth`).
//! In the store, not in memory, because they're detected in several
//! processes — the daemon's poller, a `ninox plan generate`, a GUI/TUI
//! spawn — and every UI must see all of them.
//!
//! One row per `key`: a session id (from its pane, replaced on every scan)
//! or `<source>:<harness>` for a subprocess/spawn failure.

use super::Store;
use anyhow::Result;
use rusqlite::{params, Connection};

pub(super) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS harness_auth (
            key        TEXT PRIMARY KEY,
            harness    TEXT NOT NULL,
            session_id TEXT,
            detail     TEXT NOT NULL,
            at_ms      INTEGER NOT NULL
        );",
    )?;
    Ok(())
}

/// A subprocess failure stops being news after this long: nothing clears it
/// on success for a run that only happens now and then.
const SUBPROCESS_TTL_MS: i64 = 30 * 60 * 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessAuthRow {
    pub harness:    String,
    pub session_id: Option<String>,
    pub detail:     String,
    pub at_ms:      i64,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl Store {
    pub fn record_harness_auth(&self, key: &str, harness: &str, session_id: Option<&str>, detail: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO harness_auth (key, harness, session_id, detail, at_ms) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(key) DO UPDATE SET harness = ?2, session_id = ?3, detail = ?4, at_ms = ?5",
            params![key, harness, session_id, detail, now_ms()],
        )?;
        Ok(())
    }

    pub fn clear_harness_auth(&self, key: &str) -> Result<()> {
        self.conn.lock().unwrap().execute("DELETE FROM harness_auth WHERE key = ?1", params![key])?;
        Ok(())
    }

    /// Drop every subprocess/spawn failure — the user restarted the server
    /// after logging in, so the next run reports afresh if it still fails.
    pub fn clear_harness_auth_subprocesses(&self) -> Result<()> {
        self.conn.lock().unwrap().execute("DELETE FROM harness_auth WHERE session_id IS NULL", [])?;
        Ok(())
    }

    /// Current failures: every session row, and subprocess rows younger
    /// than [`SUBPROCESS_TTL_MS`].
    pub fn harness_auth_failures(&self) -> Result<Vec<HarnessAuthRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT harness, session_id, detail, at_ms FROM harness_auth
             WHERE session_id IS NOT NULL OR at_ms >= ?1 ORDER BY harness, key",
        )?;
        let rows = stmt
            .query_map(params![now_ms() - SUBPROCESS_TTL_MS], |r| {
                Ok(HarnessAuthRow { harness: r.get(0)?, session_id: r.get(1)?, detail: r.get(2)?, at_ms: r.get(3)? })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path().join("t.db")).unwrap();
        (dir, s)
    }

    #[test]
    fn sessions_and_subprocesses_are_listed_and_cleared_separately() {
        let (_d, s) = store();
        s.record_harness_auth("w1", "claude-code", Some("w1"), "Invalid API key").unwrap();
        s.record_harness_auth("harvest:claude-code", "claude-code", None, "Not logged in").unwrap();
        assert_eq!(s.harness_auth_failures().unwrap().len(), 2);
        s.clear_harness_auth_subprocesses().unwrap();
        let left = s.harness_auth_failures().unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].session_id.as_deref(), Some("w1"));
        s.clear_harness_auth("w1").unwrap();
        assert!(s.harness_auth_failures().unwrap().is_empty());
    }

    #[test]
    fn stale_subprocess_failures_expire() {
        let (_d, s) = store();
        s.record_harness_auth("plan:claude-code", "claude-code", None, "Not logged in").unwrap();
        s.conn.lock().unwrap().execute("UPDATE harness_auth SET at_ms = 0", []).unwrap();
        assert!(s.harness_auth_failures().unwrap().is_empty());
    }
}
