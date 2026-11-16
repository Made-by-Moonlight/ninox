//! Spawn queue for the per-location concurrent session limit (see
//! `crate::session_limits`).
//!
//! Queued sessions live here and never in `sessions.status`: binaries that
//! predate this table read an unknown status as `Working`, so a `queued`
//! value there would hand their poller a pid-less ghost. Older binaries
//! never read these tables at all.
//!
//! Every admission and claim runs inside one `BEGIN IMMEDIATE` transaction,
//! so the count and the insert/claim are atomic across processes sharing
//! the database: two spawns can't both take the last slot.

use super::Store;
use crate::types::{MessageKind, Session};
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

pub(super) const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS session_queue (
        seq            INTEGER PRIMARY KEY AUTOINCREMENT,
        id             TEXT NOT NULL UNIQUE,
        location       TEXT NOT NULL,
        state          TEXT NOT NULL,
        kind           TEXT NOT NULL,
        enqueued_at    INTEGER NOT NULL,
        claimed_at     INTEGER,
        claimed_by_pid INTEGER,
        was_queued     INTEGER NOT NULL DEFAULT 0,
        session        TEXT NOT NULL,
        payload        TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS held_messages (
        seq             INTEGER PRIMARY KEY AUTOINCREMENT,
        to_id           TEXT NOT NULL,
        tagged          TEXT NOT NULL,
        attributed_from TEXT,
        defanged        TEXT NOT NULL,
        kind            TEXT NOT NULL,
        created_at      INTEGER NOT NULL
    );
";

pub(super) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueState {
    /// Waiting for a slot.
    Queued,
    /// Holds a slot while its spawn runs; released when the spawn finishes.
    Launching,
}

impl QueueState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Launching => "launching",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueKind {
    /// A new session; no `sessions` row exists until it launches.
    Spawn,
    /// A fleet restore of an existing (`Interrupted`) `sessions` row.
    Relaunch,
}

impl QueueKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Spawn => "spawn",
            Self::Relaunch => "relaunch",
        }
    }
}

#[derive(Debug, Clone)]
pub struct QueueEntry {
    pub seq:            i64,
    pub id:             String,
    /// `None` = local, else a `MachineProfile::id`.
    pub location:       Option<String>,
    pub state:          QueueState,
    pub kind:           QueueKind,
    pub enqueued_at:    i64,
    pub claimed_by_pid: Option<u32>,
    /// What the session will look like once launched; display only.
    pub session:        Session,
    /// Opaque launcher payload (owned by whoever installs the launcher).
    pub payload:        String,
}

pub struct NewQueueEntry<'a> {
    pub kind:     QueueKind,
    pub session:  &'a Session,
    pub payload:  &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    /// A slot is reserved (a `launching` entry); release it with
    /// [`Store::release_queue_entry`] once the spawn has finished.
    Admitted,
    Queued { ahead: usize },
}

#[derive(Debug)]
pub enum AdmitError {
    LimitReached { running: usize, limit: u32 },
    AlreadyQueued,
    IdTaken,
    Store(anyhow::Error),
}

impl std::fmt::Display for AdmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LimitReached { running, limit } => write!(
                f, "session limit reached ({running}/{limit} running) and --no-queue was given"
            ),
            Self::AlreadyQueued => f.write_str("already queued"),
            Self::IdTaken => f.write_str("a session with that id already exists"),
            Self::Store(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for AdmitError {}

impl From<rusqlite::Error> for AdmitError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Store(e.into())
    }
}

#[derive(Debug, Clone)]
pub struct HeldMessage {
    pub tagged:          String,
    pub attributed_from: Option<String>,
    pub defanged:        String,
    pub kind:            MessageKind,
}

fn loc_key(location: Option<&str>) -> &str {
    location.unwrap_or("")
}

/// Queue-table predicate for an orchestrator's entry. A new orchestrator has
/// no `orchestrators` row until it starts, so the spawn path's own payload
/// kind identifies it; the session's `worker_type` is never consulted, as a
/// worker could be given any type name.
const ORCH_QUEUE_ENTRY: &str =
    "(id IN (SELECT id FROM orchestrators) OR COALESCE(CASE WHEN json_valid(payload) THEN json_extract(payload, '$.kind') END, '') = 'orchestrator')";

/// Running workers on `location`: non-terminal rows (orchestrators and the
/// prompt agent excluded) plus worker `launching` reservations, without
/// counting a reservation twice once its spawn has written the row.
fn running_in(conn: &Connection, location: Option<&str>) -> rusqlite::Result<usize> {
    let n: i64 = conn.query_row(
        &format!("SELECT
           (SELECT COUNT(*) FROM sessions
             WHERE status NOT IN ('done','terminated','interrupted')
               AND COALESCE(machine_id,'') = ?1
               AND NOT (id = ?2 AND COALESCE(worker_type,'') = ?3)
               AND id NOT IN (SELECT id FROM orchestrators)
               AND id NOT IN (SELECT id FROM session_queue WHERE state = 'launching'))
         + (SELECT COUNT(*) FROM session_queue
             WHERE state = 'launching' AND location = ?1 AND NOT {ORCH_QUEUE_ENTRY})"),
        params![
            loc_key(location),
            crate::worker_type::PROMPT_SESSION_ID,
            crate::worker_type::PROMPT_WORKER_TYPE,
        ],
        |r| r.get(0),
    )?;
    Ok(n.max(0) as usize)
}

fn queued_in(conn: &Connection, location: Option<&str>) -> rusqlite::Result<usize> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM session_queue WHERE state = 'queued' AND location = ?1",
        params![loc_key(location)],
        |r| r.get(0),
    )?;
    Ok(n.max(0) as usize)
}

/// Drops `launching` reservations whose owning process has died, so a
/// crashed spawn can't hold a slot forever. Not requeued: the spawn may have
/// partly run, and the user already saw it fail.
fn release_dead_reservations(conn: &Connection) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare(
        "SELECT id, claimed_by_pid FROM session_queue WHERE state = 'launching'",
    )?;
    let dead: Vec<String> = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<u32>>(1)?)))?
        .filter_map(|r| r.ok())
        .filter(|(_, pid)| pid.is_none_or(|p| !crate::lifecycle::probe::is_pid_alive(p)))
        .map(|(id, _)| id)
        .collect();
    for id in dead {
        tracing::warn!("session queue: releasing {id}'s slot; its launching process died");
        conn.execute("DELETE FROM session_queue WHERE id = ?1", params![id])?;
        conn.execute("DELETE FROM held_messages WHERE to_id = ?1 AND reason <> 'unreachable'", params![id])?;
    }
    Ok(())
}

/// Held while waiting, and also while a claimed (formerly waiting) entry is
/// launching: its launcher flushes them once the pane is up. An admitted
/// spawn's own launch window keeps today's direct delivery.
const HOLDS_MESSAGES: &str = "state = 'queued' OR (state = 'launching' AND was_queued = 1)";

const ENTRY_COLS: &str =
    "seq, id, location, state, kind, enqueued_at, claimed_by_pid, session, payload";

fn row_to_entry(r: &rusqlite::Row<'_>) -> rusqlite::Result<QueueEntry> {
    let location: String = r.get(2)?;
    let state: String = r.get(3)?;
    let kind: String = r.get(4)?;
    let session_json: String = r.get(7)?;
    let session: Session = serde_json::from_str(&session_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(QueueEntry {
        seq: r.get(0)?,
        id: r.get(1)?,
        location: (!location.is_empty()).then_some(location),
        state: if state == "launching" { QueueState::Launching } else { QueueState::Queued },
        kind: if kind == "relaunch" { QueueKind::Relaunch } else { QueueKind::Spawn },
        enqueued_at: r.get(5)?,
        claimed_by_pid: r.get(6)?,
        session,
        payload: r.get(8)?,
    })
}

impl Store {
    /// Atomically decide whether `entry` starts now or waits: count the
    /// location's running sessions and either reserve a slot or queue, in
    /// one write-locked transaction. `limit` `None` = unlimited. A free
    /// slot is only taken when nobody is queued ahead, so the queue stays
    /// first-in first-out.
    pub fn admit_session(
        &self,
        entry: NewQueueEntry<'_>,
        limit: Option<u32>,
        no_queue: bool,
        now: i64,
    ) -> Result<Admission, AdmitError> {
        let location = entry.session.machine_id.as_deref();
        let session_json = serde_json::to_string(entry.session).map_err(|e| AdmitError::Store(e.into()))?;
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        release_dead_reservations(&tx)?;
        let id = &entry.session.id;
        let queued: bool = tx
            .query_row("SELECT 1 FROM session_queue WHERE id = ?1", params![id], |_| Ok(true))
            .optional()?
            .unwrap_or(false);
        if queued {
            return Err(AdmitError::AlreadyQueued);
        }
        if entry.kind == QueueKind::Spawn {
            let taken: bool = tx
                .query_row("SELECT 1 FROM sessions WHERE id = ?1", params![id], |_| Ok(true))
                .optional()?
                .unwrap_or(false);
            if taken {
                return Err(AdmitError::IdTaken);
            }
        }
        let running = running_in(&tx, location)?;
        let ahead = queued_in(&tx, location)?;
        let orchestrator = serde_json::from_str::<serde_json::Value>(entry.payload)
            .is_ok_and(|v| v["kind"] == "orchestrator")
            || tx
                .query_row("SELECT 1 FROM orchestrators WHERE id = ?1", params![id], |_| Ok(true))
                .optional()?
                .unwrap_or(false);
        let admit = orchestrator
            || match limit {
                None => true,
                Some(l) => running < l as usize && ahead == 0,
            };
        let state = if admit {
            QueueState::Launching
        } else if no_queue {
            return Err(AdmitError::LimitReached { running, limit: limit.unwrap_or(0) });
        } else {
            QueueState::Queued
        };
        let (claimed_at, pid) = if admit { (Some(now), Some(std::process::id())) } else { (None, None) };
        if entry.kind == QueueKind::Spawn {
            tx.execute("DELETE FROM held_messages WHERE to_id = ?1 AND reason <> 'unreachable'", params![id])?;
        }
        tx.execute(
            "INSERT INTO session_queue (id, location, state, kind, enqueued_at, claimed_at, claimed_by_pid, session, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                id, loc_key(location), state.as_str(), entry.kind.as_str(), now,
                claimed_at, pid, session_json, entry.payload,
            ],
        )?;
        tx.commit()?;
        Ok(if admit { Admission::Admitted } else { Admission::Queued { ahead } })
    }

    /// Atomically move the oldest queued entries on `location` into
    /// `launching`, as many as there are free slots under `limit`. The
    /// caller launches each and then calls [`Self::release_queue_entry`].
    pub fn claim_queued(&self, location: Option<&str>, limit: Option<u32>, now: i64) -> Result<Vec<QueueEntry>> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        release_dead_reservations(&tx)?;
        let free = match limit {
            None => usize::MAX,
            Some(l) => (l as usize).saturating_sub(running_in(&tx, location)?),
        };
        let ids: Vec<String> = {
            let mut stmt = tx.prepare(&format!(
                "SELECT id FROM session_queue WHERE state = 'queued' AND location = ?1
                   AND ({ORCH_QUEUE_ENTRY} OR seq IN (
                     SELECT seq FROM session_queue
                      WHERE state = 'queued' AND location = ?1 AND NOT {ORCH_QUEUE_ENTRY}
                      ORDER BY seq LIMIT ?2))
                 ORDER BY seq"
            ))?;
            let limit_rows = i64::try_from(free).unwrap_or(i64::MAX);
            let rows = stmt.query_map(params![loc_key(location), limit_rows], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut claimed = Vec::with_capacity(ids.len());
        for id in ids {
            tx.execute(
                "UPDATE session_queue SET state = 'launching', claimed_at = ?2, claimed_by_pid = ?3, was_queued = 1 WHERE id = ?1",
                params![id, now, std::process::id()],
            )?;
            let entry = tx.query_row(
                &format!("SELECT {ENTRY_COLS} FROM session_queue WHERE id = ?1"),
                params![id],
                row_to_entry,
            )?;
            claimed.push(entry);
        }
        tx.commit()?;
        Ok(claimed)
    }

    /// Drop a queue entry in any state: a finished launch releasing its
    /// slot, or a failed one giving it up.
    pub fn release_queue_entry(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM session_queue WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Put a claimed entry back in line at its original position (e.g. a
    /// relaunch that couldn't take the restore lease this tick).
    pub fn requeue_entry(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE session_queue SET state = 'queued', claimed_at = NULL, claimed_by_pid = NULL WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// Remove a still-waiting entry and any messages held for it. Returns
    /// whether one was removed; a `launching` entry is left alone because
    /// its process is already being started.
    pub fn cancel_queued(&self, id: &str) -> Result<bool> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let n = tx.execute("DELETE FROM session_queue WHERE id = ?1 AND state = 'queued'", params![id])?;
        if n > 0 {
            tx.execute("DELETE FROM held_messages WHERE to_id = ?1 AND reason <> 'unreachable'", params![id])?;
        }
        tx.commit()?;
        Ok(n > 0)
    }

    /// Take a waiting entry out of line without dropping its held messages
    /// (a manual Resume starting it outside the queue delivers them).
    /// Returns whether one was removed.
    pub fn dequeue_waiting(&self, id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute("DELETE FROM session_queue WHERE id = ?1 AND state = 'queued'", params![id])? > 0)
    }

    /// Drop an entry in any state together with its held messages: a launch
    /// that failed or could not be read.
    pub fn drop_queue_entry(&self, id: &str) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM session_queue WHERE id = ?1", params![id])?;
        tx.execute("DELETE FROM held_messages WHERE to_id = ?1 AND reason <> 'unreachable'", params![id])?;
        tx.commit()?;
        Ok(())
    }

    /// Every entry, oldest first.
    pub fn list_queue(&self) -> Result<Vec<QueueEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!("SELECT {ENTRY_COLS} FROM session_queue ORDER BY seq"))?;
        let rows = stmt.query_map([], row_to_entry)?;
        let mut out = Vec::new();
        for r in rows {
            match r {
                Ok(e) => out.push(e),
                Err(e) => tracing::warn!("session queue: skipping unreadable entry: {e}"),
            }
        }
        Ok(out)
    }

    pub fn queue_entry(&self, id: &str) -> Result<Option<QueueEntry>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                &format!("SELECT {ENTRY_COLS} FROM session_queue WHERE id = ?1"),
                params![id],
                row_to_entry,
            )
            .optional()?)
    }

    /// Whether `id` is waiting for a slot (not yet launching).
    pub fn is_waiting_in_queue(&self, id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT 1 FROM session_queue WHERE id = ?1 AND state = 'queued'",
                params![id],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false))
    }

    /// Live orchestrators on `location`; they don't count toward the limit.
    pub fn running_orchestrator_count(&self, location: Option<&str>) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sessions
              WHERE status NOT IN ('done','terminated','interrupted')
                AND COALESCE(machine_id,'') = ?1
                AND id IN (SELECT id FROM orchestrators)",
            params![loc_key(location)],
            |r| r.get(0),
        )?;
        Ok(n.max(0) as usize)
    }

    pub fn running_session_count(&self, location: Option<&str>) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        Ok(running_in(&conn, location)?)
    }

    /// Stores a fully prepared message for a session that is still queued.
    /// Returns false (nothing stored) when `to` is no longer waiting, so the
    /// caller delivers it directly instead. Checked and inserted in one
    /// transaction, so a message can't be held after the flush that would
    /// deliver it.
    pub fn hold_message_if_queued(&self, to: &str, msg: &HeldMessage, now: i64) -> Result<bool> {
        let kind = serde_json::to_string(&msg.kind)?.replace('"', "");
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let waiting = tx
            .query_row(
                &format!("SELECT 1 FROM session_queue WHERE id = ?1 AND ({HOLDS_MESSAGES})"),
                params![to],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        if waiting {
            tx.execute(
                "INSERT INTO held_messages (to_id, tagged, attributed_from, defanged, kind, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![to, msg.tagged, msg.attributed_from, msg.defanged, kind, now],
            )?;
        }
        tx.commit()?;
        Ok(waiting)
    }

    /// Remove and return `to`'s held messages, oldest first.
    pub fn take_held_messages(&self, to: &str) -> Result<Vec<HeldMessage>> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let msgs = {
            let mut stmt = tx.prepare(
                "SELECT tagged, attributed_from, defanged, kind FROM held_messages WHERE to_id = ?1 AND reason <> 'unreachable' ORDER BY seq",
            )?;
            let rows = stmt.query_map(params![to], |r| {
                let kind: String = r.get(3)?;
                Ok(HeldMessage {
                    tagged: r.get(0)?,
                    attributed_from: r.get(1)?,
                    defanged: r.get(2)?,
                    kind: serde_json::from_str(&format!("\"{kind}\"")).unwrap_or_default(),
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        tx.execute("DELETE FROM held_messages WHERE to_id = ?1 AND reason <> 'unreachable'", params![to])?;
        tx.commit()?;
        Ok(msgs)
    }

    pub fn held_message_count(&self, to: &str) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM held_messages WHERE to_id = ?1 AND reason <> 'unreachable'", params![to], |r| r.get(0))?;
        Ok(n.max(0) as usize)
    }
}
