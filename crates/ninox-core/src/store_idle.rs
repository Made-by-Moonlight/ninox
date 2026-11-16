//! Persistence for `crate::idle_workers`: the measured worktree sizes the
//! Idle view reads instead of walking disks, and the per-orchestrator prompt
//! log that rate-limits "prompt orchestrators" across the GUI, TUI and CLI.

use super::Store;
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

pub(super) const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS workspace_disk_cache (
        session_id  TEXT PRIMARY KEY,
        total_bytes INTEGER NOT NULL,
        build_bytes INTEGER NOT NULL,
        measured_at INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS idle_prompts (
        orchestrator_id TEXT PRIMARY KEY,
        worker_set        TEXT NOT NULL,
        prompted_at     INTEGER NOT NULL
    );
";

pub(super) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskMeasurement {
    pub total_bytes: u64,
    pub build_bytes: u64,
    pub measured_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdlePromptRecord {
    pub worker_set:    String,
    pub prompted_at: i64,
}

impl Store {
    pub fn put_disk_measurement(&self, session_id: &str, m: DiskMeasurement) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO workspace_disk_cache (session_id, total_bytes, build_bytes, measured_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(session_id) DO UPDATE SET
                total_bytes = excluded.total_bytes, build_bytes = excluded.build_bytes, measured_at = excluded.measured_at",
            params![session_id, m.total_bytes as i64, m.build_bytes as i64, m.measured_at],
        )?;
        Ok(())
    }

    pub fn disk_measurements(&self) -> Result<std::collections::HashMap<String, DiskMeasurement>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT session_id, total_bytes, build_bytes, measured_at FROM workspace_disk_cache")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                DiskMeasurement {
                    total_bytes: r.get::<_, i64>(1)?.max(0) as u64,
                    build_bytes: r.get::<_, i64>(2)?.max(0) as u64,
                    measured_at: r.get(3)?,
                },
            ))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn prune_disk_measurements(&self, keep: &[String]) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT session_id FROM workspace_disk_cache")?;
        let stale: Vec<String> =
            stmt.query_map([], |r| r.get::<_, String>(0))?.filter_map(|r| r.ok()).filter(|id| !keep.contains(id)).collect();
        for id in &stale {
            conn.execute("DELETE FROM workspace_disk_cache WHERE session_id = ?1", params![id])?;
        }
        Ok(stale.len())
    }

    pub fn idle_prompt_records(&self) -> Result<std::collections::HashMap<String, IdlePromptRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT orchestrator_id, worker_set, prompted_at FROM idle_prompts")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, IdlePromptRecord { worker_set: r.get(1)?, prompted_at: r.get(2)? }))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Atomically claims the right to prompt `orchestrator_id` about
    /// `worker_set`: false if the same set was prompted within `cooldown_ms`.
    /// The claim is recorded before sending, so concurrent GUI/TUI/CLI
    /// callers can't both send.
    pub fn claim_idle_prompt(&self, orchestrator_id: &str, worker_set: &str, now: i64, cooldown_ms: i64) -> Result<bool> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let prev: Option<(String, i64)> = tx
            .query_row(
                "SELECT worker_set, prompted_at FROM idle_prompts WHERE orchestrator_id = ?1",
                params![orchestrator_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if prev.is_some_and(|(h, t)| h == worker_set && now.saturating_sub(t) < cooldown_ms) {
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO idle_prompts (orchestrator_id, worker_set, prompted_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(orchestrator_id) DO UPDATE SET worker_set = excluded.worker_set, prompted_at = excluded.prompted_at",
            params![orchestrator_id, worker_set, now],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Undoes a claim whose send failed outright, so a retry isn't blocked.
    pub fn release_idle_prompt(&self, orchestrator_id: &str, worker_set: &str, prompted_at: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM idle_prompts WHERE orchestrator_id = ?1 AND worker_set = ?2 AND prompted_at = ?3",
            params![orchestrator_id, worker_set, prompted_at],
        )?;
        Ok(())
    }
}
