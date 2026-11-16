use crate::types::*;
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use std::{collections::HashMap, path::Path, sync::Mutex};

#[path = "store_fleet.rs"]
mod fleet;
#[path = "store_harness_auth.rs"]
mod harness_auth;
pub use harness_auth::HarnessAuthRow;
pub use fleet::{FleetRecord, RecoveryRecord, UserRequestRow, WorkRequestRow};

#[path = "store_import.rs"]
pub mod import;

#[path = "store_queue.rs"]
mod queue;

#[path = "store_ghost.rs"]
mod ghost;
#[path = "store_pr_events.rs"]
mod pr_events;
#[path = "store_idle.rs"]
mod idle;
pub use idle::{DiskMeasurement, IdlePromptRecord};
pub use pr_events::CursorKey;
#[path = "store_messages.rs"]
mod messages;
#[path = "store_notifications.rs"]
mod notifications;
pub use notifications::StoredNotification;
pub use messages::{MessageClock, UnreachableHeld};
pub use queue::{Admission, AdmitError, HeldMessage, NewQueueEntry, QueueEntry, QueueKind, QueueState};

pub struct Store {
    conn: Mutex<Connection>,
}


impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch("
            PRAGMA journal_mode=WAL;
            PRAGMA busy_timeout=5000;
            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY, orchestrator_id TEXT,
                name TEXT NOT NULL, repo TEXT NOT NULL,
                status TEXT NOT NULL, agent_type TEXT NOT NULL,
                cost_usd REAL NOT NULL DEFAULT 0, started_at INTEGER NOT NULL,
                pr_number INTEGER, pr_id INTEGER,
                workspace_path TEXT, pid INTEGER,
                model TEXT, context_tokens INTEGER
            );
            CREATE TABLE IF NOT EXISTS orchestrators (
                id TEXT PRIMARY KEY, name TEXT NOT NULL, created_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS prs (
                id INTEGER PRIMARY KEY, number INTEGER NOT NULL,
                title TEXT NOT NULL, url TEXT NOT NULL,
                body TEXT NOT NULL, session_id TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS ci_status (
                pr_id INTEGER PRIMARY KEY, total INTEGER NOT NULL,
                passing INTEGER NOT NULL, failing INTEGER NOT NULL,
                pending INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS review_comments (
                id INTEGER PRIMARY KEY, pr_id INTEGER NOT NULL,
                author TEXT NOT NULL, body TEXT NOT NULL,
                path TEXT, line INTEGER, created_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS pr_watches (
                repo TEXT NOT NULL, pr_number INTEGER NOT NULL,
                pr_url TEXT NOT NULL,
                opener_session_id TEXT NOT NULL DEFAULT '',
                created_at INTEGER NOT NULL,
                PRIMARY KEY (repo, pr_number, opener_session_id)
            );
            CREATE TABLE IF NOT EXISTS orchestrator_runtimes (
                orchestrator_id TEXT PRIMARY KEY,
                runtime_id TEXT NOT NULL UNIQUE,
                server_epoch TEXT NOT NULL,
                physical_tmux_name TEXT NOT NULL,
                pane_id TEXT NOT NULL,
                root_pid INTEGER NOT NULL,
                root_created_at INTEGER NOT NULL,
                registered_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS human_console_identities (
                id TEXT PRIMARY KEY,
                root_pid INTEGER NOT NULL,
                root_created_at INTEGER NOT NULL,
                process_name TEXT NOT NULL,
                registered_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS orchestrator_plans (
                orchestrator_id TEXT PRIMARY KEY,
                file_path TEXT NOT NULL,
                registered_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS session_deps (
                session_id TEXT NOT NULL,
                depends_on TEXT NOT NULL,
                kind TEXT NOT NULL,
                note TEXT,
                created_at INTEGER NOT NULL,
                PRIMARY KEY (session_id, depends_on, kind)
            );
            CREATE TABLE IF NOT EXISTS session_messages (
                session_id TEXT PRIMARY KEY,
                delivered_count INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS message_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                from_id TEXT,
                to_id TEXT NOT NULL,
                text TEXT NOT NULL,
                delivered_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS merge_finish_nudges (
                session_id TEXT PRIMARY KEY,
                sent_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS machine_health (
                machine_id TEXT PRIMARY KEY,
                status TEXT NOT NULL
            );
        ")?;
        // Migrations for columns added after initial release — idempotent so
        // both fresh and pre-existing databases end up with the same schema.
        for (col, ddl) in [
            ("model",                "ALTER TABLE sessions ADD COLUMN model TEXT"),
            ("context_tokens",       "ALTER TABLE sessions ADD COLUMN context_tokens INTEGER"),
            ("catalogue_path",       "ALTER TABLE sessions ADD COLUMN catalogue_path TEXT"),
            ("context_used_pct",     "ALTER TABLE sessions ADD COLUMN context_used_pct REAL"),
            ("context_total_tokens", "ALTER TABLE sessions ADD COLUMN context_total_tokens INTEGER"),
            ("context_window_size",  "ALTER TABLE sessions ADD COLUMN context_window_size INTEGER"),
            ("claude_session_id",    "ALTER TABLE sessions ADD COLUMN claude_session_id TEXT"),
            ("summary",              "ALTER TABLE sessions ADD COLUMN summary TEXT"),
            ("terminal_at",          "ALTER TABLE sessions ADD COLUMN terminal_at INTEGER"),
            ("gate_status",          "ALTER TABLE sessions ADD COLUMN gate_status TEXT"),
            ("merged_at",            "ALTER TABLE sessions ADD COLUMN merged_at INTEGER"),
            ("activity",             "ALTER TABLE sessions ADD COLUMN activity TEXT"),
            ("activity_note",        "ALTER TABLE sessions ADD COLUMN activity_note TEXT"),
            ("activity_since",       "ALTER TABLE sessions ADD COLUMN activity_since INTEGER"),
            ("machine_id",           "ALTER TABLE sessions ADD COLUMN machine_id TEXT"),
            ("worker_type",          "ALTER TABLE sessions ADD COLUMN worker_type TEXT"),
            ("success_criteria",     "ALTER TABLE sessions ADD COLUMN success_criteria TEXT"),
            ("completion_outcome",   "ALTER TABLE sessions ADD COLUMN completion_outcome TEXT"),
            ("self_completed_at",    "ALTER TABLE sessions ADD COLUMN self_completed_at INTEGER"),
            ("cleanup_result",       "ALTER TABLE sessions ADD COLUMN cleanup_result TEXT"),
            ("cleanup_at",           "ALTER TABLE sessions ADD COLUMN cleanup_at INTEGER"),
            ("brain_reads",          "ALTER TABLE sessions ADD COLUMN brain_reads TEXT"),
            ("brain_write",          "ALTER TABLE sessions ADD COLUMN brain_write TEXT"),
            ("permission_mode",      "ALTER TABLE sessions ADD COLUMN permission_mode TEXT"),
        ] {
            if !Self::column_exists(&conn, "sessions", col)? {
                conn.execute(ddl, [])?;
            }
        }
        if !Self::column_exists(&conn, "message_events", "kind")? {
            conn.execute("ALTER TABLE message_events ADD COLUMN kind TEXT", [])?;
        }
        fleet::migrate(&conn)?;
        harness_auth::migrate(&conn)?;
        queue::migrate(&conn)?;
        ghost::migrate(&conn)?;
        messages::migrate(&conn)?;
        Self::migrate_legacy_system_sender(&conn)?;
        pr_events::migrate(&conn)?;
        notifications::migrate(&conn)?;
        idle::migrate(&conn)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// `user_version` 1: rows recorded before the system sender gained its
    /// `:` (`messaging::LEGACY_SYSTEM_SENDER`) move to `SYSTEM_SENDER`.
    /// Exactly once: from then on `ninox` is an ordinary session id whose
    /// rows must keep it.
    fn migrate_legacy_system_sender(conn: &Connection) -> Result<()> {
        use crate::messaging::{LEGACY_SYSTEM_SENDER, SYSTEM_SENDER};
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version >= 1 {
            return Ok(());
        }
        let tx = conn.unchecked_transaction()?;
        tx.execute("UPDATE message_events SET from_id = ?1 WHERE from_id = ?2", params![SYSTEM_SENDER, LEGACY_SYSTEM_SENDER])?;
        tx.execute("UPDATE held_messages SET attributed_from = ?1 WHERE attributed_from = ?2", params![SYSTEM_SENDER, LEGACY_SYSTEM_SENDER])?;
        tx.execute_batch("PRAGMA user_version = 1")?;
        tx.commit()?;
        Ok(())
    }

    fn parse_brain_reads(raw: Option<String>) -> Vec<String> {
        raw.and_then(|r| serde_json::from_str(&r).ok()).unwrap_or_default()
    }

    fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let exists = stmt
            .query_map([], |r| r.get::<_, String>(1))?
            .filter_map(|r| r.ok())
            .any(|c| c == column);
        Ok(exists)
    }

    /// Atomically stamp `merged_at`, only if it is still NULL and the session
    /// isn't `Done`. Returns `true` for the one caller whose write moved it
    /// from NULL — the winner of a race between pollers (daemon and GUI), in
    /// this process or another. The `Done` check keeps a reaped row (which
    /// carries no stamp) from being claimed again by a stale snapshot.
    pub fn claim_merged_at(&self, session_id: &str, at: i64) -> Result<bool> {
        let done = serde_json::to_string(&SessionStatus::Done)?.replace('"', "");
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE sessions SET merged_at=?1 WHERE id=?2 AND merged_at IS NULL AND status != ?3",
            params![at, session_id, done],
        )?;
        Ok(changed == 1)
    }

    /// Atomically record that the one-shot "your PR merged, finish up"
    /// reminder is being sent to `session_id`. `true` only for the single
    /// caller that inserted the row, across pollers and processes. Lives in
    /// its own table so older builds, which never read it, still open the
    /// store.
    pub fn claim_finish_nudge(&self, session_id: &str, at: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "INSERT OR IGNORE INTO merge_finish_nudges (session_id, sent_at) VALUES (?1, ?2)",
            params![session_id, at],
        )?;
        Ok(changed == 1)
    }

    /// Drop a `merged_at` claim without touching any other column. With
    /// `only_if_done`, only a `Done` row is cleared (a reaped row carries no
    /// stamp); otherwise the claim is released unconditionally.
    pub fn clear_merged_at(&self, session_id: &str, only_if_done: bool) -> Result<()> {
        let done = serde_json::to_string(&SessionStatus::Done)?.replace('"', "");
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET merged_at=NULL WHERE id=?1 AND (?2 = 0 OR status = ?3)",
            params![session_id, only_if_done as i64, done],
        )?;
        Ok(())
    }

    pub fn upsert_session(&self, s: &Session) -> Result<()> {
        // Older binaries read an unknown status as `Working`; queued sessions
        // live only in `session_queue` (see `store_queue.rs`).
        anyhow::ensure!(
            s.status != SessionStatus::Queued,
            "refusing to write queued session {} to the sessions table", s.id
        );
        let status = serde_json::to_string(&s.status)?.replace('"', "");
        let gate_status = s.gate_status.as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let activity = serde_json::to_string(&s.activity)?.replace('"', "");
        let success_criteria = serde_json::to_string(&s.success_criteria)?;
        let completion_outcome = s.completion_outcome.as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let brain_reads = serde_json::to_string(&s.brain_reads)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO sessions (id,orchestrator_id,name,repo,status,agent_type,
             cost_usd,started_at,pr_number,pr_id,workspace_path,pid,model,context_tokens,
             catalogue_path,context_used_pct,context_total_tokens,context_window_size,
             claude_session_id,summary,terminal_at,gate_status,merged_at,
             activity,activity_note,activity_since,machine_id,worker_type,
             success_criteria,completion_outcome,self_completed_at,cleanup_result,cleanup_at,
             brain_reads,brain_write)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27,?28,?29,?30,?31,?32,?33,?34,?15)
             ON CONFLICT(id) DO UPDATE SET
             repo=excluded.repo,
             status=excluded.status,cost_usd=excluded.cost_usd,
             started_at=excluded.started_at,
             pr_number=excluded.pr_number,pr_id=excluded.pr_id,
             workspace_path=excluded.workspace_path,pid=excluded.pid,
             model=excluded.model,context_tokens=excluded.context_tokens,
             catalogue_path=excluded.catalogue_path,
             context_used_pct=excluded.context_used_pct,
             context_total_tokens=excluded.context_total_tokens,
             context_window_size=excluded.context_window_size,
             claude_session_id=excluded.claude_session_id,
             summary=excluded.summary,
             terminal_at=excluded.terminal_at,
             gate_status=excluded.gate_status,
             merged_at=excluded.merged_at,
             activity=excluded.activity,
             activity_note=excluded.activity_note,
             activity_since=excluded.activity_since,
             machine_id=excluded.machine_id,
             worker_type=excluded.worker_type,
             success_criteria=excluded.success_criteria,
             completion_outcome=excluded.completion_outcome,
             self_completed_at=excluded.self_completed_at,
             cleanup_result=excluded.cleanup_result,
             cleanup_at=excluded.cleanup_at,
             brain_reads=excluded.brain_reads,
             brain_write=excluded.brain_write",
            params![
                s.id, s.orchestrator_id, s.name, s.repo, status, s.agent_type,
                s.cost_usd, s.started_at, s.pr_number, s.pr_id,
                s.workspace_path, s.pid, s.model, s.context_tokens,
                s.brain_write, s.context_used_pct, s.context_total_tokens,
                s.context_window_size, s.claude_session_id, s.summary, s.terminal_at,
                gate_status, s.merged_at, activity, s.activity_note, s.activity_since,
                s.machine_id, s.worker_type,
                success_criteria, completion_outcome, s.self_completed_at,
                s.cleanup_result, s.cleanup_at, brain_reads
            ],
        )?;
        Ok(())
    }

    pub fn list_sessions(&self) -> Result<Vec<Session>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id,orchestrator_id,name,repo,status,agent_type,cost_usd,
             started_at,pr_number,pr_id,workspace_path,pid,model,context_tokens,
             catalogue_path,context_used_pct,context_total_tokens,context_window_size,
             claude_session_id,summary,terminal_at,gate_status,merged_at,
             activity,activity_note,activity_since,machine_id,worker_type,
             success_criteria,completion_outcome,self_completed_at,cleanup_result,cleanup_at,
             brain_reads,brain_write
             FROM sessions ORDER BY started_at DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, f64>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, Option<u64>>(8)?,
                r.get::<_, Option<i64>>(9)?,
                r.get::<_, Option<String>>(10)?,
                r.get::<_, Option<u32>>(11)?,
                r.get::<_, Option<String>>(12)?,
                r.get::<_, Option<i64>>(13)?,
                r.get::<_, Option<String>>(14)?,
                r.get::<_, Option<f64>>(15)?,
                r.get::<_, Option<i64>>(16)?,
                r.get::<_, Option<i64>>(17)?,
                r.get::<_, Option<String>>(18)?,
                r.get::<_, Option<String>>(19)?,
                r.get::<_, Option<i64>>(20)?,
                r.get::<_, Option<String>>(21)?,
                r.get::<_, Option<i64>>(22)?,
                r.get::<_, Option<String>>(23)?,
                r.get::<_, Option<String>>(24)?,
                r.get::<_, Option<i64>>(25)?,
                r.get::<_, Option<String>>(26)?,
                r.get::<_, Option<String>>(27)?,
                r.get::<_, Option<String>>(28)?,
                r.get::<_, Option<String>>(29)?,
                r.get::<_, Option<i64>>(30)?,
                r.get::<_, Option<String>>(31)?,
                r.get::<_, Option<i64>>(32)?,
                r.get::<_, Option<String>>(33)?,
                r.get::<_, Option<String>>(34)?,
            ))
        })?;
        rows.map(|r| {
            let (id, orchestrator_id, name, repo, status_str, agent_type,
                 cost_usd, started_at, pr_number, pr_id, workspace_path, pid,
                 model, context_tokens, catalogue_path, context_used_pct,
                 context_total_tokens, context_window_size, claude_session_id,
                 summary, terminal_at, gate_status_str, merged_at,
                 activity_str, activity_note, activity_since, machine_id, worker_type,
                 success_criteria_str, completion_outcome_str, self_completed_at,
                 cleanup_result, cleanup_at, brain_reads_str, brain_write) = r?;
            let status = serde_json::from_str(&format!("\"{status_str}\""))
                .unwrap_or(SessionStatus::Working);
            let gate_status = gate_status_str
                .and_then(|s| serde_json::from_str(&s).ok());
            Ok(Session {
                id, orchestrator_id, name, repo, status, agent_type,
                cost_usd, started_at, pr_number, pr_id, workspace_path, pid,
                model, context_tokens: context_tokens.map(|v| v.max(0) as u64),
                brain_reads: Self::parse_brain_reads(brain_reads_str),
                brain_write: brain_write.or(catalogue_path),
                context_used_pct,
                context_total_tokens: context_total_tokens.map(|v| v.max(0) as u64),
                context_window_size: context_window_size.map(|v| v.max(0) as u64),
                claude_session_id,
                summary,
                terminal_at,
                gate_status,
                merged_at,
                activity: Self::parse_activity(activity_str),
                activity_note,
                activity_since,
                machine_id,
                worker_type,
                success_criteria: Self::parse_success_criteria(success_criteria_str),
                completion_outcome: completion_outcome_str.and_then(|s| serde_json::from_str(&s).ok()),
                self_completed_at,
                cleanup_result,
                cleanup_at,
            })
        })
        .collect()
    }

    pub fn get_session(&self, id: &str) -> Result<Option<Session>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id,orchestrator_id,name,repo,status,agent_type,cost_usd,
             started_at,pr_number,pr_id,workspace_path,pid,model,context_tokens,
             catalogue_path,context_used_pct,context_total_tokens,context_window_size,
             claude_session_id,summary,terminal_at,gate_status,merged_at,
             activity,activity_note,activity_since,machine_id,worker_type,
             success_criteria,completion_outcome,self_completed_at,cleanup_result,cleanup_at,
             brain_reads,brain_write
             FROM sessions WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map([id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, f64>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, Option<u64>>(8)?,
                r.get::<_, Option<i64>>(9)?,
                r.get::<_, Option<String>>(10)?,
                r.get::<_, Option<u32>>(11)?,
                r.get::<_, Option<String>>(12)?,
                r.get::<_, Option<i64>>(13)?,
                r.get::<_, Option<String>>(14)?,
                r.get::<_, Option<f64>>(15)?,
                r.get::<_, Option<i64>>(16)?,
                r.get::<_, Option<i64>>(17)?,
                r.get::<_, Option<String>>(18)?,
                r.get::<_, Option<String>>(19)?,
                r.get::<_, Option<i64>>(20)?,
                r.get::<_, Option<String>>(21)?,
                r.get::<_, Option<i64>>(22)?,
                r.get::<_, Option<String>>(23)?,
                r.get::<_, Option<String>>(24)?,
                r.get::<_, Option<i64>>(25)?,
                r.get::<_, Option<String>>(26)?,
                r.get::<_, Option<String>>(27)?,
                r.get::<_, Option<String>>(28)?,
                r.get::<_, Option<String>>(29)?,
                r.get::<_, Option<i64>>(30)?,
                r.get::<_, Option<String>>(31)?,
                r.get::<_, Option<i64>>(32)?,
                r.get::<_, Option<String>>(33)?,
                r.get::<_, Option<String>>(34)?,
            ))
        })?;
        match rows.next() {
            None => Ok(None),
            Some(r) => {
                let (id, orchestrator_id, name, repo, status_str, agent_type,
                     cost_usd, started_at, pr_number, pr_id, workspace_path, pid,
                     model, context_tokens, catalogue_path, context_used_pct,
                     context_total_tokens, context_window_size, claude_session_id,
                     summary, terminal_at, gate_status_str, merged_at,
                     activity_str, activity_note, activity_since, machine_id, worker_type,
                     success_criteria_str, completion_outcome_str, self_completed_at,
                     cleanup_result, cleanup_at, brain_reads_str, brain_write) = r?;
                let status = serde_json::from_str(&format!("\"{status_str}\""))
                    .unwrap_or(SessionStatus::Working);
                let gate_status = gate_status_str
                    .and_then(|s| serde_json::from_str(&s).ok());
                Ok(Some(Session {
                    id, orchestrator_id, name, repo, status, agent_type,
                    cost_usd, started_at, pr_number, pr_id, workspace_path, pid,
                    model, context_tokens: context_tokens.map(|v| v.max(0) as u64),
                    brain_reads: Self::parse_brain_reads(brain_reads_str),
                    brain_write: brain_write.or(catalogue_path),
                    context_used_pct,
                    context_total_tokens: context_total_tokens.map(|v| v.max(0) as u64),
                    context_window_size: context_window_size.map(|v| v.max(0) as u64),
                    claude_session_id,
                    summary,
                    terminal_at,
                    gate_status,
                    merged_at,
                    activity: Self::parse_activity(activity_str),
                    activity_note,
                    activity_since,
                    machine_id,
                    worker_type,
                    success_criteria: Self::parse_success_criteria(success_criteria_str),
                    completion_outcome: completion_outcome_str.and_then(|s| serde_json::from_str(&s).ok()),
                    self_completed_at,
                    cleanup_result,
                    cleanup_at,
                }))
            }
        }
    }

    /// NULL (legacy row) and unrecognized values both collapse to `Unknown` —
    /// same defensive posture as `status`'s `unwrap_or(Working)` above.
    fn parse_activity(raw: Option<String>) -> ActivityState {
        raw.and_then(|s| serde_json::from_str(&format!("\"{s}\"")).ok())
            .unwrap_or_default()
    }

    /// NULL (legacy row) or a JSON parse failure both collapse to no
    /// criteria — `ninox complete` treats that the same as a session
    /// spawned with no type and no `--success` override (defaults to
    /// `[ReportSent]`).
    fn parse_success_criteria(raw: Option<String>) -> Vec<SuccessCriterion> {
        raw.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    /// Targeted write of only the activity columns. The `worker-status`
    /// hook process runs concurrently with the poller's and GUI's full-row
    /// upserts — a read→full-row-write from that process could revert their
    /// fresher fields (status, terminal_at, cost) or re-INSERT a
    /// just-deleted row; an UPDATE can do neither. The status guard repeats
    /// `apply_activity`'s terminal check *at write time*, closing the
    /// read-then-reap window. Returns whether a live row was written.
    pub(crate) fn update_session_activity(
        &self,
        id: &str,
        activity: ActivityState,
        note: Option<&str>,
        since: Option<i64>,
    ) -> Result<bool> {
        let activity_str = serde_json::to_string(&activity)?.replace('"', "");
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE sessions SET activity=?2, activity_note=?3, activity_since=?4
             WHERE id=?1 AND status NOT IN ('done','terminated','interrupted')",
            params![id, activity_str, note, since],
        )?;
        Ok(n > 0)
    }

    /// Targeted write of only the cleanup-result columns — never a full-row
    /// upsert, so it can't race a concurrent full-row writer's stale
    /// snapshot the way `update_session_activity`'s doc comment describes.
    pub fn set_cleanup_result(&self, session_id: &str, outcome: &crate::workspace::CleanupOutcome) -> Result<()> {
        let result = outcome.describe();
        let at = crate::lifecycle::poller::now_millis();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET cleanup_result=?2, cleanup_at=?3 WHERE id=?1",
            params![session_id, result, at],
        )?;
        Ok(())
    }

    pub fn add_session_dep(&self, dep: &SessionDep) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO session_deps (session_id, depends_on, kind, note, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(session_id, depends_on, kind) DO UPDATE SET
             note=excluded.note",
            params![dep.session_id, dep.depends_on, dep.kind.as_str(), dep.note, dep.created_at],
        )?;
        Ok(())
    }

    /// Returns whether an edge actually existed and was removed.
    pub fn remove_session_dep(&self, session_id: &str, depends_on: &str, kind: DepKind) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM session_deps WHERE session_id=?1 AND depends_on=?2 AND kind=?3",
            params![session_id, depends_on, kind.as_str()],
        )?;
        Ok(n > 0)
    }

    pub fn list_session_deps(&self) -> Result<Vec<SessionDep>> {
        self.query_deps("SELECT session_id, depends_on, kind, note, created_at
                         FROM session_deps ORDER BY created_at", &[])
    }

    /// Edges *from* `session_id` (what it depends on), both kinds.
    pub fn deps_for_session(&self, session_id: &str) -> Result<Vec<SessionDep>> {
        self.query_deps("SELECT session_id, depends_on, kind, note, created_at
                         FROM session_deps WHERE session_id=?1 ORDER BY created_at",
                        &[&session_id])
    }

    fn query_deps(&self, sql: &str, args: &[&dyn rusqlite::ToSql]) -> Result<Vec<SessionDep>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(args, |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })?;
        rows.map(|r| {
            let (session_id, depends_on, kind_str, note, created_at) = r?;
            Ok(SessionDep {
                session_id, depends_on,
                // An unrecognized kind (from a future version's row) is
                // surfaced as Declared rather than dropped: better a
                // mislabeled edge than a silently missing one.
                kind: DepKind::parse(&kind_str).unwrap_or(DepKind::Declared),
                note, created_at,
            })
        })
        .collect()
    }

    /// Reconcile the *stacked* edge set for `session_id` against a fresh
    /// derivation from PR branch topology: insert what's missing, delete
    /// what no longer holds, leave `Declared` edges alone. Returns whether
    /// anything changed — no event carries dep changes today (the Workers
    /// view re-reads on its poll tick), but callers that want to react to
    /// churn have the signal.
    pub(crate) fn set_stacked_deps(&self, session_id: &str, depends_on: &[SessionId], now: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT depends_on FROM session_deps WHERE session_id=?1 AND kind='stacked'",
        )?;
        let current: Vec<String> = stmt
            .query_map([session_id], |r| r.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);

        let mut changed = false;
        for stale in current.iter().filter(|c| !depends_on.contains(c)) {
            conn.execute(
                "DELETE FROM session_deps WHERE session_id=?1 AND depends_on=?2 AND kind='stacked'",
                params![session_id, stale],
            )?;
            changed = true;
        }
        for fresh in depends_on.iter().filter(|d| !current.contains(d)) {
            conn.execute(
                "INSERT INTO session_deps (session_id, depends_on, kind, note, created_at)
                 VALUES (?1, ?2, 'stacked', NULL, ?3)",
                params![session_id, fresh, now],
            )?;
            changed = true;
        }
        Ok(changed)
    }


    /// Column-scoped write of the fields the external `ninox statusline`
    /// process owns (cost + context readout, and `model` only when not
    /// already set). Deliberately NOT a `upsert_session` full-row write: the
    /// statusline hook fires continuously and from a *separate process*, so
    /// a read-modify-write there would revert any field the in-app poller
    /// stamped between the read and the write — most visibly `merged_at` on
    /// a kept-alive merged worker (whose statusline keeps firing during
    /// post-merge validation), re-triggering its one-shot merge
    /// notification. A single UPDATE touching only these columns can't
    /// stomp `status`/`pr_number`/`gate_status`/`merged_at`. Each `None`
    /// argument leaves its column untouched. Returns whether a row matched.
    pub(crate) fn update_statusline_metrics(
        &self,
        session_id:           &str,
        cost_usd:             Option<f64>,
        context_used_pct:     Option<f64>,
        context_total_tokens: Option<u64>,
        context_window_size:  Option<u64>,
        model:                Option<&str>,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE sessions SET
                cost_usd             = COALESCE(?2, cost_usd),
                context_used_pct     = COALESCE(?3, context_used_pct),
                context_total_tokens = COALESCE(?4, context_total_tokens),
                context_window_size  = COALESCE(?5, context_window_size),
                model                = COALESCE(model, ?6)
             WHERE id = ?1",
            params![
                session_id,
                cost_usd,
                context_used_pct,
                context_total_tokens,
                context_window_size,
                model,
            ],
        )?;
        Ok(changed > 0)
    }

    /// Non-zero `cost_usd` samples recorded for sessions matching the given
    /// agent harness (`agent_type`) and model — used to compute a
    /// data-driven spawn-modal cost estimate once enough history exists for
    /// a given preset. Read-only; built on `list_sessions` like
    /// `sessions_by_orchestrator`.
    pub fn cost_samples(&self, agent_type: &str, model: Option<&str>) -> Result<Vec<f64>> {
        let sessions = self.list_sessions()?;
        Ok(sessions
            .into_iter()
            .filter(|s| {
                s.agent_type == agent_type
                    && s.model.as_deref() == model
                    && s.cost_usd > 0.0
            })
            .map(|s| s.cost_usd)
            .collect())
    }

    pub fn upsert_orchestrator(&self, o: &Orchestrator) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO orchestrators(id,name,created_at) VALUES(?1,?2,?3)
             ON CONFLICT(id) DO UPDATE SET name=excluded.name",
            params![o.id, o.name, o.created_at],
        )?;
        Ok(())
    }

    pub fn is_orchestrator(&self, id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM orchestrators WHERE id=?1)",
            [id],
            |row| row.get(0),
        )
        .map_err(Into::into)
    }

    pub fn register_orchestrator_runtime(&self, runtime: &OrchestratorRuntimeIdentity) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        anyhow::ensure!(
            conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM orchestrators WHERE id=?1)",
                [&runtime.orchestrator_id],
                |row| row.get::<_, bool>(0),
            )?,
            "orchestrator is not persisted"
        );
        conn.execute(
            "INSERT INTO orchestrator_runtimes(
                orchestrator_id,runtime_id,server_epoch,physical_tmux_name,
                pane_id,root_pid,root_created_at,registered_at
             ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
             ON CONFLICT(orchestrator_id) DO UPDATE SET
                runtime_id=excluded.runtime_id,
                server_epoch=excluded.server_epoch,
                physical_tmux_name=excluded.physical_tmux_name,
                pane_id=excluded.pane_id,
                root_pid=excluded.root_pid,
                root_created_at=excluded.root_created_at,
                registered_at=excluded.registered_at",
            params![
                runtime.orchestrator_id,
                runtime.runtime_id,
                runtime.server_epoch,
                runtime.physical_tmux_name,
                runtime.pane_id,
                runtime.root_pid,
                runtime.root_created_at,
                runtime.registered_at,
            ],
        )?;
        Ok(())
    }

    /// Self-registers an orchestrator's runtime identity the first time it
    /// authorizes from a pane whose physical name matches its own id — the
    /// lazy bootstrap path `authorize_orchestrator` falls back to when no
    /// runtime has been persisted yet. Requires the caller's session row to
    /// already record `pid` as this exact pane's root pid (set at spawn
    /// time), and only fires once per orchestrator (`ON CONFLICT DO NOTHING`).
    /// Returns whether a row was actually created.
    pub(crate) fn register_migrated_orchestrator_runtime(&self, runtime: &OrchestratorRuntimeIdentity) -> Result<bool> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let eligible: bool = tx.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM orchestrators o
                JOIN sessions s ON s.id=o.id
                WHERE o.id=?1 AND s.pid=?2
                  AND NOT EXISTS(
                    SELECT 1 FROM orchestrator_runtimes r
                    WHERE r.orchestrator_id=o.id
                  )
            )",
            params![runtime.orchestrator_id, runtime.root_pid],
            |row| row.get(0),
        )?;
        if !eligible || runtime.physical_tmux_name != runtime.orchestrator_id {
            tx.commit()?;
            return Ok(false);
        }
        let changed = tx.execute(
            "INSERT INTO orchestrator_runtimes(
                orchestrator_id,runtime_id,server_epoch,physical_tmux_name,
                pane_id,root_pid,root_created_at,registered_at
             ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
             ON CONFLICT(orchestrator_id) DO NOTHING",
            params![
                runtime.orchestrator_id,
                runtime.runtime_id,
                runtime.server_epoch,
                runtime.physical_tmux_name,
                runtime.pane_id,
                runtime.root_pid,
                runtime.root_created_at,
                runtime.registered_at,
            ],
        )?;
        tx.commit()?;
        Ok(changed == 1)
    }

    /// Replaces an orchestrator's runtime identity whose root pid the caller
    /// already confirmed dead with `runtime`, iff the stored row still has
    /// `dead_root_pid`, the session row records `runtime.root_pid` as its
    /// pid, and the pane is named after the orchestrator. Compare-and-swap:
    /// a row another caller already replaced is left alone. Returns whether
    /// the row was replaced.
    pub(crate) fn replace_dead_orchestrator_runtime_with(
        &self,
        runtime: &OrchestratorRuntimeIdentity,
        dead_root_pid: u32,
    ) -> Result<bool> {
        if runtime.physical_tmux_name != runtime.orchestrator_id || runtime.root_pid == dead_root_pid {
            return Ok(false);
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let changed = tx.execute(
            "UPDATE orchestrator_runtimes SET
                runtime_id=?3, server_epoch=?4, physical_tmux_name=?5,
                pane_id=?6, root_pid=?7, root_created_at=?8, registered_at=?9
             WHERE orchestrator_id=?1 AND root_pid=?2
               AND EXISTS(SELECT 1 FROM sessions s WHERE s.id=?1 AND s.pid=?7)",
            params![
                runtime.orchestrator_id,
                dead_root_pid,
                runtime.runtime_id,
                runtime.server_epoch,
                runtime.physical_tmux_name,
                runtime.pane_id,
                runtime.root_pid,
                runtime.root_created_at,
                runtime.registered_at,
            ],
        )?;
        tx.commit()?;
        Ok(changed == 1)
    }

    /// Records `pid` as `id`'s pane root iff the row has none yet — never
    /// overwrites a recorded pid. Returns whether it was filled.
    pub fn fill_session_pid_if_null(&self, id: &str, pid: u32) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE sessions SET pid=?2 WHERE id=?1 AND pid IS NULL",
            params![id, pid],
        )?;
        Ok(changed == 1)
    }

    /// Clears `orchestrator_id`'s persisted runtime identity iff a row
    /// exists and its `root_pid` is exactly `expected_root_pid` — never a
    /// wildcard delete. Returns whether a row was actually cleared.
    ///
    /// The `expected_root_pid` guard prevents a TOCTOU race: if some other
    /// migration already replaced the row between the caller reading it and
    /// calling this method, a delete keyed only on `orchestrator_id` could
    /// blow away a different, already-valid row. Keying the delete on the
    /// exact pid the caller already confirmed dead makes it a no-op instead.
    ///
    /// Only ever called from inside `spawn_util::relaunch_in_place` after a
    /// confirmed-dead liveness probe — see
    /// `docs/superpowers/specs/2026-10-22-orchestrator-identity-restart-design.md`.
    pub fn delete_dead_orchestrator_runtime(&self, orchestrator_id: &str, expected_root_pid: u32) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "DELETE FROM orchestrator_runtimes WHERE orchestrator_id=?1 AND root_pid=?2",
            params![orchestrator_id, expected_root_pid],
        )?;
        Ok(changed == 1)
    }

    pub fn orchestrator_runtime_identity(&self, orchestrator_id: &str) -> Result<Option<OrchestratorRuntimeIdentity>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT orchestrator_id,runtime_id,server_epoch,physical_tmux_name,
                    pane_id,root_pid,root_created_at,registered_at
             FROM orchestrator_runtimes WHERE orchestrator_id=?1",
            [orchestrator_id],
            |row| {
                let root_pid = u32::try_from(row.get::<_, i64>(5)?).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        5,
                        rusqlite::types::Type::Integer,
                        Box::new(error),
                    )
                })?;
                Ok(OrchestratorRuntimeIdentity {
                    orchestrator_id: row.get(0)?,
                    runtime_id: row.get(1)?,
                    server_epoch: row.get(2)?,
                    physical_tmux_name: row.get(3)?,
                    pane_id: row.get(4)?,
                    root_pid,
                    root_created_at: row.get(6)?,
                    registered_at: row.get(7)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn list_orchestrator_runtime_identities(&self) -> Result<Vec<OrchestratorRuntimeIdentity>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT orchestrator_id,runtime_id,server_epoch,physical_tmux_name,
                    pane_id,root_pid,root_created_at,registered_at
             FROM orchestrator_runtimes ORDER BY orchestrator_id",
        )?;
        let rows = stmt.query_map([], |row| {
            let root_pid = u32::try_from(row.get::<_, i64>(5)?).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Integer, Box::new(error))
            })?;
            Ok(OrchestratorRuntimeIdentity {
                orchestrator_id: row.get(0)?,
                runtime_id: row.get(1)?,
                server_epoch: row.get(2)?,
                physical_tmux_name: row.get(3)?,
                pane_id: row.get(4)?,
                root_pid,
                root_created_at: row.get(6)?,
                registered_at: row.get(7)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    /// Upsert keyed on `id`: each boot refreshes `registered_at` for the
    /// same process identity.
    pub fn register_human_console_identity(&self, identity: &HumanConsoleIdentity) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO human_console_identities(id,root_pid,root_created_at,process_name,registered_at)
             VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(id) DO UPDATE SET registered_at=excluded.registered_at",
            params![
                identity.id,
                identity.root_pid,
                identity.root_created_at,
                identity.process_name,
                identity.registered_at,
            ],
        )?;
        Ok(())
    }

    pub fn list_human_console_identities(&self) -> Result<Vec<HumanConsoleIdentity>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id,root_pid,root_created_at,process_name,registered_at
             FROM human_console_identities ORDER BY id",
        )?;
        let rows = stmt.query_map([], |row| {
            let root_pid = u32::try_from(row.get::<_, i64>(1)?).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Integer, Box::new(error))
            })?;
            Ok(HumanConsoleIdentity {
                id: row.get(0)?,
                root_pid,
                root_created_at: row.get(2)?,
                process_name: row.get(3)?,
                registered_at: row.get(4)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    /// The one function that can ever mark a pending `user_requests` row
    /// answered: gated on `human_console_auth::verify_caller_is_registered_console`,
    /// checked before anything else so a failed check never touches the row.
    /// Idempotent once the row has left `Pending` — a second answer (or a
    /// cancellation racing a genuine answer) is a no-op, never an overwrite
    /// of the first answer or its timestamp.
    pub fn answer_user_request(&self, id: &str, answer: &str) -> Result<()> {
        crate::human_console_auth::verify_caller_is_registered_console(self)?;
        let current = self
            .get_user_request(id)?
            .ok_or_else(|| anyhow::anyhow!("user request {id} not found"))?;
        if current.status != UserRequestStatus::Pending {
            return Ok(());
        }
        let answered_at = crate::lifecycle::poller::now_millis();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE user_requests SET status=?2, answer=?3, answered_at=?4
             WHERE id=?1 AND status=?5",
            params![
                id,
                UserRequestStatus::Answered.as_str(),
                answer,
                answered_at,
                UserRequestStatus::Pending.as_str(),
            ],
        )?;
        Ok(())
    }

    /// Register (or re-register) `orchestrator_id`'s goals/plan doc. Upsert:
    /// re-registering bumps `updated_at` but leaves the original
    /// `registered_at` untouched.
    pub fn register_orchestrator_plan(&self, orchestrator_id: &str, file_path: &str, now: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO orchestrator_plans(orchestrator_id,file_path,registered_at,updated_at)
             VALUES(?1,?2,?3,?3)
             ON CONFLICT(orchestrator_id) DO UPDATE SET
                file_path=excluded.file_path,
                updated_at=excluded.updated_at",
            params![orchestrator_id, file_path, now],
        )?;
        Ok(())
    }

    pub fn get_orchestrator_plan(&self, orchestrator_id: &str) -> Result<Option<OrchestratorPlan>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT orchestrator_id,file_path,registered_at,updated_at
             FROM orchestrator_plans WHERE orchestrator_id=?1",
            [orchestrator_id],
            |row| {
                Ok(OrchestratorPlan {
                    orchestrator_id: row.get(0)?,
                    file_path: row.get(1)?,
                    registered_at: row.get(2)?,
                    updated_at: row.get(3)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn list_orchestrator_plans(&self) -> Result<Vec<OrchestratorPlan>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT orchestrator_id,file_path,registered_at,updated_at
             FROM orchestrator_plans ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(OrchestratorPlan {
                orchestrator_id: row.get(0)?,
                file_path: row.get(1)?,
                registered_at: row.get(2)?,
                updated_at: row.get(3)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    /// Returns whether a row was actually removed.
    pub fn unregister_orchestrator_plan(&self, orchestrator_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "DELETE FROM orchestrator_plans WHERE orchestrator_id=?1",
            [orchestrator_id],
        )?;
        Ok(changed > 0)
    }

    /// Count one more message delivered to `session_id` (see
    /// `messaging::deliver_message`). Lives in its own table rather than on
    /// the `sessions` row because `upsert_session` is a full-row write and
    /// several processes hold stale snapshots of that row.
    pub(crate) fn record_message_delivered(&self, session_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO session_messages (session_id, delivered_count) VALUES (?1, 1)
             ON CONFLICT(session_id) DO UPDATE SET delivered_count = delivered_count + 1",
            [session_id],
        )?;
        Ok(())
    }

    pub fn message_delivered_counts(&self) -> Result<HashMap<String, u64>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT session_id, delivered_count FROM session_messages")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Records `machine_id`'s last-observed connection health — see
    /// `MachineHealth` and `lifecycle::poller::Poller::poll_remote_machine_health`.
    pub fn set_machine_health(&self, machine_id: &str, health: MachineHealth) -> Result<()> {
        let status = serde_json::to_string(&health)?.replace('"', "");
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO machine_health (machine_id, status) VALUES (?1, ?2)
             ON CONFLICT(machine_id) DO UPDATE SET status = excluded.status",
            params![machine_id, status],
        )?;
        Ok(())
    }

    /// Every machine's last-observed connection health, keyed by
    /// `machine_id` — consumed by `ninox-tui`'s `build_rows` to populate
    /// `Row::machine_health`.
    pub fn machine_health(&self) -> Result<HashMap<String, MachineHealth>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT machine_id, status FROM machine_health")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        Ok(rows
            .filter_map(|r| r.ok())
            .filter_map(|(id, status)| {
                serde_json::from_str::<MachineHealth>(&format!("\"{status}\"")).ok().map(|h| (id, h))
            })
            .collect())
    }

    /// Drops `machine_id`'s recorded health — called once its
    /// `MachineProfile` is no longer configured, mirroring
    /// `Poller::remote_machine_health`'s own in-memory pruning.
    pub fn remove_machine_health(&self, machine_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM machine_health WHERE machine_id = ?1", [machine_id])?;
        Ok(())
    }

    /// Records one successful delivery; see `record_message_attempt` for
    /// the general form that also records queued and failed attempts.
    pub fn record_message_event(&self, from: Option<&str>, to: &str, text: &str, kind: MessageKind) -> Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        self.record_message_attempt(from, to, text, kind, MessageOutcome::Delivered, None, None, now)?;
        Ok(())
    }

    /// Deliveries with `id` after `since_id` (the caller's last-seen
    /// cursor), ordered oldest first — the same "poll what's new" shape as
    /// `list_session_deps`/`deps_for_session`. Queued and failed attempts
    /// are left out: the Graph tab flashes only what actually arrived.
    pub fn recent_message_events(&self, since_id: i64) -> Result<Vec<MessageEvent>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, from_id, to_id, text, delivered_at, kind FROM message_events
             WHERE id > ?1 AND COALESCE(outcome, 'delivered') = 'delivered' ORDER BY id",
        )?;
        let rows = stmt.query_map(params![since_id], |r| {
            let kind_str: Option<String> = r.get(5)?;
            Ok(MessageEvent {
                id: r.get(0)?,
                from_id: r.get(1)?,
                to_id: r.get(2)?,
                text: r.get(3)?,
                delivered_at: r.get(4)?,
                kind: kind_str
                    .and_then(|s| serde_json::from_str(&format!("\"{s}\"")).ok())
                    .unwrap_or_default(),
                outcome: MessageOutcome::Delivered,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// `true` iff `from` has already delivered a `MessageKind::Completion`
    /// to `to` at or after `since_ms` — used to suppress a redundant
    /// Ninox-generated done/retired notice once the worker's own `ninox
    /// complete` already reported in. `since_ms` is the session's
    /// `started_at`: one worker tracks exactly one PR/task for its whole
    /// lifetime, so "since this session started" stands in for "for this
    /// unit of work" without a separate per-PR key.
    pub fn has_completion_since(&self, from: &str, to: &str, since_ms: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let kind_str = serde_json::to_string(&MessageKind::Completion)?.replace('"', "");
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM message_events
             WHERE from_id = ?1 AND to_id = ?2 AND kind = ?3 AND delivered_at >= ?4
               AND COALESCE(outcome, 'delivered') <> 'failed'",
            params![from, to, kind_str, since_ms],
            |r| r.get(0),
        )?;
        Ok(count > 0)
    }

    /// Record the permission mode `id`'s current incarnation was launched
    /// with. Kept off `Session` and out of `upsert_session`, so no row
    /// write can silently reset it; only a fresh launch sets it.
    pub fn set_permission_mode(&self, id: &str, mode: crate::harness::PermissionMode) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("UPDATE sessions SET permission_mode = ?2 WHERE id = ?1", params![id, mode.as_str()])?;
        Ok(())
    }

    /// `None` for rows launched before this was recorded.
    pub fn permission_mode(&self, id: &str) -> Result<Option<crate::harness::PermissionMode>> {
        let conn = self.conn.lock().unwrap();
        let raw: Option<Option<String>> = conn
            .query_row("SELECT permission_mode FROM sessions WHERE id = ?1", [id], |r| r.get(0))
            .optional()?;
        Ok(raw.flatten().as_deref().and_then(crate::harness::PermissionMode::parse))
    }

    pub(crate) fn sessions_by_orchestrator(&self, orchestrator_id: &str) -> Result<Vec<Session>> {
        let sessions = self.list_sessions()?;
        Ok(sessions.into_iter().filter(|s| s.orchestrator_id.as_deref() == Some(orchestrator_id)).collect())
    }

    pub fn delete_session(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM sessions WHERE id = ?1", [id])?;
        // Edges in either direction would otherwise dangle forever — no
        // poller pass re-derives *declared* edges.
        conn.execute("DELETE FROM session_deps WHERE session_id=?1 OR depends_on=?1", [id])?;
        conn.execute("DELETE FROM session_messages WHERE session_id = ?1", [id])?;
        conn.execute("DELETE FROM merge_finish_nudges WHERE session_id = ?1", [id])?;
        conn.execute("DELETE FROM workspace_disk_cache WHERE session_id = ?1", [id])?;
        Self::purge_fleet_rows(&conn, id)?;
        Ok(())
    }

    /// Make `session_id` a standalone session, so deleting its orchestrator
    /// leaves its row (and its messages and dependency edges) alone.
    pub(crate) fn detach_from_orchestrator(&self, session_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("UPDATE sessions SET orchestrator_id = NULL WHERE id = ?1", [session_id])?;
        Ok(())
    }

    /// Deletes an orchestrator and its plain workers' rows (sessions,
    /// deps, messages, fleet rows, runtime identity, plan). Deliberately
    /// never touches a **sub-orchestrator** it finds via `orchestrator_id =
    /// id` — a nested orchestrator is a real, independently-tracked
    /// orchestrator (see the nested-orchestration design spec), not a
    /// disposable worker, and tearing one down is a separate, not-yet-
    /// specced operation. Every `orchestrator_id=?1` subquery below is
    /// guarded with `AND id NOT IN (SELECT id FROM orchestrators)` so a
    /// sub-orchestrator's session, messages and fleet row all survive a
    /// parent's deletion untouched; only its lineage pointer now dangles,
    /// the same already-accepted state a sub-orchestrator reaches whenever
    /// its own `authorize_nested_spawn` root happens to go away.
    pub fn delete_orchestrator(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        // Edge purge must run while the session rows still exist — it keys
        // off them to find the orchestrator's workers.
        conn.execute(
            "DELETE FROM session_deps WHERE session_id IN
               (SELECT id FROM sessions WHERE (orchestrator_id=?1 AND id NOT IN (SELECT id FROM orchestrators)) OR id=?1)
             OR depends_on IN
               (SELECT id FROM sessions WHERE (orchestrator_id=?1 AND id NOT IN (SELECT id FROM orchestrators)) OR id=?1)",
            [id],
        )?;
        conn.execute(
            "DELETE FROM session_messages WHERE session_id = ?1
             OR session_id IN (SELECT id FROM sessions WHERE orchestrator_id = ?1 AND id NOT IN (SELECT id FROM orchestrators))",
            [id],
        )?;
        conn.execute(
            "DELETE FROM merge_finish_nudges WHERE session_id = ?1
             OR session_id IN (SELECT id FROM sessions WHERE orchestrator_id = ?1 AND id NOT IN (SELECT id FROM orchestrators))",
            [id],
        )?;
        conn.execute(
            "DELETE FROM workspace_disk_cache WHERE session_id = ?1
             OR session_id IN (SELECT id FROM sessions WHERE orchestrator_id = ?1 AND id NOT IN (SELECT id FROM orchestrators))",
            [id],
        )?;
        conn.execute("DELETE FROM idle_prompts WHERE orchestrator_id = ?1", [id])?;
        conn.execute(
            "DELETE FROM fleet_sessions WHERE session_id IN
               (SELECT id FROM sessions WHERE orchestrator_id=?1 AND id NOT IN (SELECT id FROM orchestrators))",
            [id],
        )?;
        conn.execute(
            "DELETE FROM user_requests WHERE requester_session_id IN
               (SELECT id FROM sessions WHERE orchestrator_id=?1)",
            [id],
        )?;
        Self::purge_fleet_rows(&conn, id)?;
        conn.execute("DELETE FROM work_requests WHERE orchestrator_id=?1", [id])?;
        conn.execute(
            "DELETE FROM sessions WHERE orchestrator_id = ?1 AND id NOT IN (SELECT id FROM orchestrators)",
            [id],
        )?;
        conn.execute("DELETE FROM sessions WHERE id = ?1", [id])?;
        conn.execute("DELETE FROM orchestrator_runtimes WHERE orchestrator_id=?1", [id])?;
        conn.execute("DELETE FROM orchestrator_plans WHERE orchestrator_id=?1", [id])?;
        conn.execute("DELETE FROM orchestrators WHERE id = ?1", [id])?;
        Ok(())
    }

    pub(crate) fn upsert_pr(&self, pr: &PR) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO prs
             (id, number, title, url, body, session_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![pr.id, pr.number, pr.title, pr.url, pr.body, pr.session_id],
        )?;
        Ok(())
    }

    pub fn get_pr(&self, id: PrId) -> Result<Option<PR>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, number, title, url, body, session_id FROM prs WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map([id], |r| {
            Ok(PR {
                id:         r.get(0)?,
                number:     r.get(1)?,
                title:      r.get(2)?,
                url:        r.get(3)?,
                body:       r.get(4)?,
                session_id: r.get(5)?,
            })
        })?;
        rows.next().transpose().map_err(Into::into)
    }

    pub(crate) fn upsert_ci_status(&self, ci: &CIStatus) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO ci_status
             (pr_id, total, passing, failing, pending)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![ci.pr_id, ci.total, ci.passing, ci.failing, ci.pending],
        )?;
        Ok(())
    }

    pub(crate) fn upsert_comment(&self, c: &Comment) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO review_comments
             (id, pr_id, author, body, path, line, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![c.id, c.pr_id, c.author, c.body, c.path, c.line, c.created_at],
        )?;
        Ok(())
    }

    /// Gated on the `test-support` feature as well as `cfg(test)`: `cfg(test)`
    /// is per-crate, but sibling crates (ninox-gui) need to seed comments for
    /// their own tests of `list_comments` consumers, and `upsert_comment`
    /// itself is `pub(crate)` (see `refactor(core): tighten Store visibility`,
    /// PR #214) since it otherwise has no external callers.
    #[cfg(any(test, feature = "test-support"))]
    pub fn upsert_comment_for_test(&self, c: &Comment) -> Result<()> {
        self.upsert_comment(c)
    }

    /// All persisted review/issue comments, ordered by `created_at` — used to
    /// hydrate `App`'s in-memory comment feed on startup so a restart doesn't
    /// lose comments a previous run already fetched.
    pub fn list_comments(&self) -> Result<Vec<Comment>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, pr_id, author, body, path, line, created_at
             FROM review_comments ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Comment {
                id:         r.get(0)?,
                pr_id:      r.get(1)?,
                author:     r.get(2)?,
                body:       r.get(3)?,
                path:       r.get(4)?,
                line:       r.get(5)?,
                created_at: r.get(6)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    /// One PR's comments, ordered by `created_at` — indexed by `pr_id`, for
    /// callers (e.g. the TUI/GUI detail panes) that only ever want a single
    /// PR's comments and would otherwise re-scan/clone the whole
    /// `review_comments` table every refresh just to filter it down client
    /// side.
    pub fn list_comments_for_pr(&self, pr_id: PrId) -> Result<Vec<Comment>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, pr_id, author, body, path, line, created_at
             FROM review_comments WHERE pr_id = ?1 ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([pr_id], |r| {
            Ok(Comment {
                id:         r.get(0)?,
                pr_id:      r.get(1)?,
                author:     r.get(2)?,
                body:       r.get(3)?,
                path:       r.get(4)?,
                line:       r.get(5)?,
                created_at: r.get(6)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    pub fn upsert_pr_watch(&self, w: &PrWatch) -> Result<()> {
        let opener = w.opener_session_id.as_deref().unwrap_or("");
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO pr_watches (repo, pr_number, pr_url, opener_session_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(repo, pr_number, opener_session_id) DO UPDATE SET
             pr_url = excluded.pr_url",
            params![w.repo, w.pr_number, w.pr_url, opener, w.created_at],
        )?;
        Ok(())
    }

    pub fn delete_pr_watch(&self, repo: &str, pr_number: u64, opener: Option<&str>) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM pr_watches WHERE repo = ?1 AND pr_number = ?2 AND opener_session_id = ?3",
            params![repo, pr_number, opener.unwrap_or("")],
        )?;
        Ok(n)
    }

    pub(crate) fn delete_pr_watches_for_pr(&self, repo: &str, pr_number: u64) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM pr_watches WHERE repo = ?1 AND pr_number = ?2",
            params![repo, pr_number],
        )?;
        Ok(n)
    }

    pub fn list_pr_watches(&self) -> Result<Vec<PrWatch>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT repo, pr_number, pr_url, opener_session_id, created_at
             FROM pr_watches ORDER BY repo, pr_number, opener_session_id",
        )?;
        let rows = stmt.query_map([], |r| {
            let opener: String = r.get(3)?;
            Ok(PrWatch {
                repo:              r.get(0)?,
                pr_number:         r.get(1)?,
                pr_url:            r.get(2)?,
                opener_session_id: if opener.is_empty() { None } else { Some(opener) },
                created_at:        r.get(4)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn list_orchestrators(&self) -> Result<Vec<Orchestrator>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id,name,created_at FROM orchestrators ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Orchestrator {
                id: r.get(0)?,
                name: r.get(1)?,
                created_at: r.get(2)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn the_legacy_system_sender_migrates_once_and_leaves_a_later_ninox_session_alone() {
        use crate::messaging::{LEGACY_SYSTEM_SENDER, SYSTEM_SENDER};
        let _iso = crate::test_isolation::isolate();
        let path = tempdir().unwrap().keep().join("t.db");
        {
            let store = Store::open(&path).unwrap();
            let conn = store.conn.lock().unwrap();
            conn.execute_batch("PRAGMA user_version = 0").unwrap();
            conn.execute(
                "INSERT INTO message_events (from_id, to_id, text, delivered_at, kind) VALUES (?1, 'w1', 'ci failing', 1, 'system')",
                params![LEGACY_SYSTEM_SENDER],
            ).unwrap();
            conn.execute(
                "INSERT INTO held_messages (to_id, tagged, attributed_from, defanged, kind, created_at) VALUES ('w1', 't', ?1, 'd', 'system', 1)",
                params![LEGACY_SYSTEM_SENDER],
            ).unwrap();
        }

        let store = Store::open(&path).unwrap();
        let senders = |store: &Store| -> (Vec<Option<String>>, Vec<Option<String>>) {
            let conn = store.conn.lock().unwrap();
            let col = |sql: &str| conn.prepare(sql).unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
            (col("SELECT from_id FROM message_events ORDER BY id"), col("SELECT attributed_from FROM held_messages ORDER BY seq"))
        };
        assert_eq!(senders(&store), (vec![Some(SYSTEM_SENDER.into())], vec![Some(SYSTEM_SENDER.into())]));

        store.record_message_event(Some("ninox"), "w1", "from the ninox orchestrator", MessageKind::Freeform).unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(
            senders(&store).0,
            [Some(SYSTEM_SENDER.into()), Some("ninox".into())],
            "a session named ninox recorded after the migration keeps its own id",
        );
    }

    fn test_store() -> Store {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.db");
        // keep dir alive for the lifetime of the test by leaking it
        std::mem::forget(dir);
        Store::open(path).unwrap()
    }

    #[test]
    fn upsert_and_list_session() {
        let store = test_store();
        let session = Session {
            id: "s1".into(), orchestrator_id: None, name: "worker-1".into(),
            repo: "slievr/Athene".into(), status: SessionStatus::Working,
            agent_type: "claude-code".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        store.upsert_session(&session).unwrap();
        let list = store.list_sessions().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "s1");
    }

    #[test]
    fn upsert_updates_status() {
        let store = test_store();
        let mut s = Session {
            id: "s1".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: SessionStatus::Working,
            agent_type: "c".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        store.upsert_session(&s).unwrap();
        s.status = SessionStatus::Done;
        store.upsert_session(&s).unwrap();
        let list = store.list_sessions().unwrap();
        assert_eq!(list.len(), 1);
        assert!(matches!(list[0].status, SessionStatus::Done));
    }

    #[test]
    fn set_cleanup_result_is_a_narrow_update() {
        let store = test_store();
        let s = Session {
            id: "s1".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: SessionStatus::Working,
            agent_type: "c".into(), cost_usd: 1.5, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None, worker_type: None,
            success_criteria: Vec::new(), completion_outcome: None, self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        store.upsert_session(&s).unwrap();

        let outcome = crate::workspace::CleanupOutcome {
            worktree_removed: true,
            worktree_removal_failed: false,
            branch_deleted: Some("feat".into()),
            artifacts_removed: true,
            skipped: None,
        };
        store.set_cleanup_result("s1", &outcome).unwrap();

        let after = store.get_session("s1").unwrap().unwrap();
        assert_eq!(after.cleanup_result.as_deref(), Some("worktree removed, branch feat deleted, artifacts removed"));
        assert!(after.cleanup_at.is_some());
        assert!(matches!(after.status, SessionStatus::Working), "status must be untouched by a narrow update");
        assert_eq!(after.cost_usd, 1.5, "cost_usd must be untouched by a narrow update");
    }

    /// The poller's dual-remote self-heal (`session.repo` corrected once a
    /// PR is found against a different remote than the one on record)
    /// depends on `repo` being an updatable column, not just a write-once
    /// field set at insert time.
    #[test]
    fn upsert_updates_repo() {
        let store = test_store();
        let mut s = Session {
            id: "s1".into(), orchestrator_id: None, name: "w".into(),
            repo: "OwnerA/repoA".into(), status: SessionStatus::Working,
            agent_type: "c".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        store.upsert_session(&s).unwrap();
        s.repo = "OwnerB/repoB".into();
        store.upsert_session(&s).unwrap();
        let updated = store.get_session("s1").unwrap().unwrap();
        assert_eq!(updated.repo, "OwnerB/repoB");
    }

    #[test]
    fn get_session_by_id() {
        let store = test_store();
        let s = Session {
            id: "s1".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: SessionStatus::Working,
            agent_type: "c".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        store.upsert_session(&s).unwrap();
        let found = store.get_session("s1").unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "w");
        assert!(store.get_session("missing").unwrap().is_none());
    }

    #[test]
    fn model_and_context_tokens_round_trip() {
        let store = test_store();
        let s = Session {
            id: "s1".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: SessionStatus::Working,
            agent_type: "claude-code".into(), cost_usd: 1.5, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: Some("claude-fable-5".into()), context_tokens: Some(214_000), brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        store.upsert_session(&s).unwrap();
        let found = store.get_session("s1").unwrap().unwrap();
        assert_eq!(found.model.as_deref(), Some("claude-fable-5"));
        assert_eq!(found.context_tokens, Some(214_000));
    }

    fn brain_session(id: &str, reads: &[&str], write: Option<&str>) -> Session {
        let mut v = serde_json::json!({
            "id": id, "orchestrator_id": null, "name": "w", "repo": "r", "status": "working",
            "agent_type": "claude-code", "cost_usd": 0.0, "started_at": 0, "pr_number": null,
            "pr_id": null, "workspace_path": null, "pid": null,
        });
        v["brain_reads"] = serde_json::json!(reads);
        v["brain_write"] = serde_json::json!(write);
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn brain_set_round_trips() {
        let store = test_store();
        store.upsert_session(&brain_session("s2", &["/brains/a", "/brains/b:wiki"], Some("/brains/a"))).unwrap();
        for found in [store.get_session("s2").unwrap().unwrap(), store.list_sessions().unwrap().remove(0)] {
            assert_eq!(found.brain_reads, vec!["/brains/a", "/brains/b:wiki"]);
            assert_eq!(found.brain_write.as_deref(), Some("/brains/a"));
        }
    }

    /// Older binaries sharing this store read only `catalogue_path`; it
    /// must keep tracking `brain_write` or they'd harvest new sessions into
    /// the default brain.
    #[test]
    fn new_rows_dual_write_legacy_catalogue_path() {
        let store = test_store();
        store.upsert_session(&brain_session("s3", &["/brains/a", "/brains/b"], Some("/brains/b"))).unwrap();
        let legacy = |store: &Store| -> Option<String> {
            store.conn.lock().unwrap()
                .query_row("SELECT catalogue_path FROM sessions WHERE id='s3'", [], |r| r.get(0)).unwrap()
        };
        assert_eq!(legacy(&store).as_deref(), Some("/brains/b"));
        store.upsert_session(&brain_session("s3", &["/brains/a"], Some("/brains/a"))).unwrap();
        assert_eq!(legacy(&store).as_deref(), Some("/brains/a"), "updates dual-write too");
    }

    /// A row written before brain sets (or by an older binary) carries only
    /// `catalogue_path`: it reads back as a single-catalogue set.
    #[test]
    fn legacy_catalogue_path_row_migrates_to_brain_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("CREATE TABLE sessions (id TEXT PRIMARY KEY, orchestrator_id TEXT, name TEXT NOT NULL,
                repo TEXT NOT NULL, status TEXT NOT NULL, agent_type TEXT NOT NULL, cost_usd REAL NOT NULL,
                started_at INTEGER NOT NULL, pr_number INTEGER, pr_id INTEGER, workspace_path TEXT, pid INTEGER,
                catalogue_path TEXT);
                INSERT INTO sessions (id,name,repo,status,agent_type,cost_usd,started_at,catalogue_path)
                VALUES ('old','w','r','working','claude-code',0,0,'/brains/legacy');").unwrap();
        }
        let store = Store::open(&path).unwrap();
        let s = store.get_session("old").unwrap().unwrap();
        assert_eq!(s.brain_write.as_deref(), Some("/brains/legacy"));
        assert!(s.brain_reads.is_empty(), "read set resolves to just brain_write (see BrainSet::from_session)");
    }

    #[test]
    fn summary_round_trips() {
        let store = test_store();
        let s = Session {
            id: "s2b".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: SessionStatus::Working,
            agent_type: "claude-code".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: Some("Fix flaky CI on the auth suite".into()),
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        store.upsert_session(&s).unwrap();
        let found = store.get_session("s2b").unwrap().unwrap();
        assert_eq!(found.summary.as_deref(), Some("Fix flaky CI on the auth suite"));
        // list path decodes it too
        assert_eq!(
            store.list_sessions().unwrap().iter().find(|x| x.id == "s2b").unwrap().summary.as_deref(),
            Some("Fix flaky CI on the auth suite"),
        );

        // None round-trips as None, not "" or "none"
        let mut s2 = s.clone();
        s2.id = "s2c".into();
        s2.summary = None;
        store.upsert_session(&s2).unwrap();
        assert_eq!(store.get_session("s2c").unwrap().unwrap().summary, None);
    }

    #[test]
    fn claude_session_id_round_trips() {
        let store = test_store();
        let s = Session {
            id: "s3".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: SessionStatus::Working,
            agent_type: "claude-code".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: Some("b7e0b3a0-0000-4000-8000-000000000001".into()),
            summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        store.upsert_session(&s).unwrap();
        let found = store.get_session("s3").unwrap().unwrap();
        assert_eq!(found.claude_session_id.as_deref(), Some("b7e0b3a0-0000-4000-8000-000000000001"));
        // list path decodes it too
        assert_eq!(
            store.list_sessions().unwrap().iter().find(|x| x.id == "s3").unwrap().claude_session_id.as_deref(),
            Some("b7e0b3a0-0000-4000-8000-000000000001"),
        );

        // None round-trips as None, not "" or "none"
        let mut s2 = s.clone();
        s2.id = "s4".into();
        s2.claude_session_id = None;
        store.upsert_session(&s2).unwrap();
        assert_eq!(store.get_session("s4").unwrap().unwrap().claude_session_id, None);
    }

    #[test]
    fn terminal_at_round_trips() {
        let store = test_store();
        let s = Session {
            id: "s5".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: SessionStatus::Done,
            agent_type: "claude-code".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: Some(1_720_000_000_000), gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        store.upsert_session(&s).unwrap();
        let found = store.get_session("s5").unwrap().unwrap();
        assert_eq!(found.terminal_at, Some(1_720_000_000_000));
        // list path decodes it too
        assert_eq!(
            store.list_sessions().unwrap().iter().find(|x| x.id == "s5").unwrap().terminal_at,
            Some(1_720_000_000_000),
        );

        // None round-trips as None — non-terminal / not-yet-retired sessions.
        let mut s2 = s.clone();
        s2.id = "s6".into();
        s2.terminal_at = None;
        store.upsert_session(&s2).unwrap();
        assert_eq!(store.get_session("s6").unwrap().unwrap().terminal_at, None);
    }

    /// A Re-file respawns the same session id with a fresh `started_at` —
    /// the conflict-update path must persist it, or the in-memory time
    /// silently reverts to the original spawn time on app restart.
    #[test]
    fn upsert_conflict_updates_started_at() {
        let store = test_store();
        let mut s = Session {
            id: "s3".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: SessionStatus::Working,
            agent_type: "claude-code".into(), cost_usd: 0.0, started_at: 100,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        store.upsert_session(&s).unwrap();
        s.started_at = 200;
        store.upsert_session(&s).unwrap();
        assert_eq!(store.get_session("s3").unwrap().unwrap().started_at, 200);
    }

    #[test]
    fn get_pr_round_trips_and_misses_cleanly() {
        let store = test_store();
        assert!(store.get_pr(9).unwrap().is_none());
        let pr = PR {
            id: 9, number: 9, title: "t".into(),
            url: "https://github.com/org/repo/pull/9".into(),
            body: String::new(), session_id: "s1".into(),
        };
        store.upsert_pr(&pr).unwrap();
        let found = store.get_pr(9).unwrap().unwrap();
        assert_eq!(found.number, 9);
        assert_eq!(found.session_id, "s1");
        assert_eq!(found.url, pr.url);
    }

    /// `list_comments` is what hydrates `App::review_threads` on restart —
    /// it must come back ordered by `created_at` so the UI feed doesn't need
    /// to re-sort, and `upsert_comment`'s INSERT OR REPLACE must not produce
    /// duplicate rows for a comment seen twice.
    #[test]
    fn list_comments_orders_by_created_at_and_upsert_dedupes() {
        let store = test_store();
        let later = Comment {
            id: 2, pr_id: 1, author: "bob".into(), body: "second".into(),
            path: None, line: None, created_at: 2_000,
        };
        let earlier = Comment {
            id: 1, pr_id: 1, author: "alice".into(), body: "first".into(),
            path: Some("src/lib.rs".into()), line: Some(10), created_at: 1_000,
        };
        store.upsert_comment(&later).unwrap();
        store.upsert_comment(&earlier).unwrap();

        let comments = store.list_comments().unwrap();
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0].id, 1, "earlier created_at sorts first");
        assert_eq!(comments[1].id, 2);

        // Re-upserting the same id (e.g. a repeated poll) must replace, not duplicate.
        store.upsert_comment(&earlier).unwrap();
        assert_eq!(store.list_comments().unwrap().len(), 2);
    }

    #[test]
    fn list_comments_for_pr_only_returns_that_prs_comments() {
        let store = test_store();
        store.upsert_comment(&Comment { id: 1, pr_id: 1, author: "alice".into(), body: "on pr 1".into(), path: None, line: None, created_at: 1_000 }).unwrap();
        store.upsert_comment(&Comment { id: 2, pr_id: 2, author: "bob".into(), body: "on pr 2".into(), path: None, line: None, created_at: 1_000 }).unwrap();

        let comments = store.list_comments_for_pr(1).unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].author, "alice");
    }

    #[test]
    fn context_fields_round_trip() {
        let store = test_store();
        let s = Session {
            id: "s1".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: SessionStatus::Working,
            agent_type: "claude-code".into(), cost_usd: 2.6, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: Some(62.0),
            context_total_tokens: Some(124_000),
            context_window_size: Some(200_000),
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        store.upsert_session(&s).unwrap();
        let found = store.get_session("s1").unwrap().unwrap();
        assert_eq!(found.context_used_pct, Some(62.0));
        assert_eq!(found.context_total_tokens, Some(124_000));
        assert_eq!(found.context_window_size, Some(200_000));
        // list path decodes it too
        assert_eq!(store.list_sessions().unwrap()[0].context_used_pct, Some(62.0));
    }

    #[test]
    fn context_fields_default_to_none() {
        let store = test_store();
        let s = Session {
            id: "s2".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: SessionStatus::Working,
            agent_type: "claude-code".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        store.upsert_session(&s).unwrap();
        let found = store.get_session("s2").unwrap().unwrap();
        assert_eq!(found.context_used_pct, None);
        assert_eq!(found.context_total_tokens, None);
        assert_eq!(found.context_window_size, None);
    }

    #[test]
    fn cost_samples_filters_by_agent_and_model_and_excludes_zero() {
        let store = test_store();
        for (id, agent_type, model, cost) in [
            ("a", "claude-code", Some("claude-fable-5"), 3.0),
            ("b", "claude-code", Some("claude-fable-5"), 5.0),
            ("c", "claude-code", Some("claude-fable-5"), 0.0), // excluded: zero cost
            ("d", "claude-code", Some("claude-opus-4-8"), 2.0), // excluded: different model
            ("e", "codex",       Some("claude-fable-5"), 4.0),  // excluded: different harness
        ] {
            store.upsert_session(&Session {
                id: id.into(), orchestrator_id: None, name: id.into(),
                repo: "r".into(), status: SessionStatus::Working,
                agent_type: agent_type.into(), cost_usd: cost, started_at: 0,
                pr_number: None, pr_id: None, workspace_path: None, pid: None,
                model: model.map(String::from), context_tokens: None, brain_reads: Vec::new(), brain_write: None,
                context_used_pct: None, context_total_tokens: None, context_window_size: None,
                claude_session_id: None, summary: None,
                terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        }).unwrap();
        }
        let samples = store.cost_samples("claude-code", Some("claude-fable-5")).unwrap();
        assert_eq!(samples.len(), 2);
        assert!((samples.iter().sum::<f64>() - 8.0).abs() < f64::EPSILON);
    }

    #[test]
    fn upsert_and_fetch_session_round_trips_gate_status() {
        let store = Store::open(tempdir().unwrap().keep().join("t.db")).unwrap();
        let mut session = crate::types::Session {
            id: "s1".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: crate::types::SessionStatus::PrOpen,
            agent_type: "c".into(), cost_usd: 0.0, started_at: 0,
            pr_number: Some(1), pr_id: Some(1), workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None,
            gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        session.gate_status = Some(crate::types::GateStatus {
            ci: crate::types::GateCheck::Failing,
            review: crate::types::GateCheck::Passing,
            mergeable: crate::types::GateCheck::Unknown,
            since: 12_345,
        });
        store.upsert_session(&session).unwrap();

        let fetched = store.get_session("s1").unwrap().unwrap();
        assert_eq!(fetched.gate_status, session.gate_status);

        let listed = store.list_sessions().unwrap();
        assert_eq!(listed[0].gate_status, session.gate_status);
    }

    #[test]
    fn legacy_row_without_gate_status_column_defaults_to_none() {
        // A row written before this migration has no gate_status column
        // value — column_exists-gated ALTER TABLE means the column is added
        // but existing rows get SQL NULL, which must deserialize to `None`.
        let store = Store::open(tempdir().unwrap().keep().join("t.db")).unwrap();
        let session = crate::types::Session {
            id: "s2".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: crate::types::SessionStatus::Working,
            agent_type: "c".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None,
            gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        store.upsert_session(&session).unwrap();
        let fetched = store.get_session("s2").unwrap().unwrap();
        assert_eq!(fetched.gate_status, None);
    }

    #[test]
    fn worker_type_column_migration_is_idempotent_across_reopens() {
        // Store::open runs the column_exists-gated ALTER TABLE on every
        // call — reopening the same DB file must not fail with "duplicate
        // column name" the second time around.
        let path = tempdir().unwrap().keep().join("t.db");
        Store::open(&path).unwrap();
        let store = Store::open(&path).unwrap();
        let session = crate::types::Session {
            id: "s3".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: crate::types::SessionStatus::Working,
            agent_type: "c".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None,
            gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: Some("Implementation".into()),
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        store.upsert_session(&session).unwrap();
        let fetched = store.get_session("s3").unwrap().unwrap();
        assert_eq!(fetched.worker_type.as_deref(), Some("Implementation"));

        let listed = store.list_sessions().unwrap();
        assert_eq!(listed[0].worker_type.as_deref(), Some("Implementation"));
    }

    #[test]
    fn machine_id_column_migration_is_idempotent_across_reopens() {
        // Same idempotent column_exists-gated ALTER TABLE as worker_type's
        // (`Store::open` runs it on every call) — reopening an existing DB
        // file must not fail with "duplicate column name".
        let path = tempdir().unwrap().keep().join("t.db");
        Store::open(&path).unwrap();
        let store = Store::open(&path).unwrap();
        let session = crate::types::Session {
            id: "s5".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: crate::types::SessionStatus::Working,
            agent_type: "c".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None,
            gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: Some("m1".into()),
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        store.upsert_session(&session).unwrap();
        let fetched = store.get_session("s5").unwrap().unwrap();
        assert_eq!(fetched.machine_id.as_deref(), Some("m1"));

        let listed = store.list_sessions().unwrap();
        assert_eq!(listed[0].machine_id.as_deref(), Some("m1"));
    }

    #[test]
    fn legacy_row_without_worker_type_column_defaults_to_none() {
        let store = Store::open(tempdir().unwrap().keep().join("t.db")).unwrap();
        let session = crate::types::Session {
            id: "s4".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: crate::types::SessionStatus::Working,
            agent_type: "c".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None,
            gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        store.upsert_session(&session).unwrap();
        let fetched = store.get_session("s4").unwrap().unwrap();
        assert_eq!(fetched.worker_type, None);
    }

    #[test]
    fn upsert_and_fetch_session_round_trips_activity() {
        let store = test_store();
        let mut session = Session {
            id: "s1".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: SessionStatus::Working,
            agent_type: "c".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None,
            gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        session.activity = ActivityState::Blocked;
        session.activity_note = Some("waiting on schema migration".into());
        session.activity_since = Some(12_345);
        store.upsert_session(&session).unwrap();

        let fetched = store.get_session("s1").unwrap().unwrap();
        assert_eq!(fetched.activity, ActivityState::Blocked);
        assert_eq!(fetched.activity_note.as_deref(), Some("waiting on schema migration"));
        assert_eq!(fetched.activity_since, Some(12_345));

        let listed = store.list_sessions().unwrap();
        assert_eq!(listed[0].activity, ActivityState::Blocked);
        assert_eq!(listed[0].activity_note, session.activity_note);
        assert_eq!(listed[0].activity_since, Some(12_345));
    }

    #[test]
    fn session_dep_add_list_and_dedup_on_same_edge() {
        let store = test_store();
        let dep = SessionDep {
            session_id: "b".into(), depends_on: "a".into(),
            kind: DepKind::Declared, note: Some("needs a's schema".into()),
            created_at: 100,
        };
        store.add_session_dep(&dep).unwrap();
        // Same (session, depends_on, kind) again — must not duplicate; the
        // fresher note wins.
        store.add_session_dep(&SessionDep { note: Some("updated".into()), ..dep.clone() }).unwrap();

        let all = store.list_session_deps().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].note.as_deref(), Some("updated"));

        let for_b = store.deps_for_session("b").unwrap();
        assert_eq!(for_b.len(), 1);
        assert_eq!(for_b[0].depends_on, "a");
        assert!(store.deps_for_session("a").unwrap().is_empty());
    }

    #[test]
    fn remove_session_dep_reports_whether_an_edge_was_removed() {
        let store = test_store();
        let dep = SessionDep {
            session_id: "b".into(), depends_on: "a".into(),
            kind: DepKind::Declared, note: None, created_at: 100,
        };
        store.add_session_dep(&dep).unwrap();
        assert!(store.remove_session_dep("b", "a", DepKind::Declared).unwrap());
        assert!(!store.remove_session_dep("b", "a", DepKind::Declared).unwrap(), "second removal finds nothing");
        assert!(store.list_session_deps().unwrap().is_empty());
    }

    #[test]
    fn set_stacked_deps_replaces_stacked_but_preserves_declared() {
        let store = test_store();
        store.add_session_dep(&SessionDep {
            session_id: "b".into(), depends_on: "a".into(),
            kind: DepKind::Declared, note: None, created_at: 1,
        }).unwrap();
        store.add_session_dep(&SessionDep {
            session_id: "b".into(), depends_on: "old".into(),
            kind: DepKind::Stacked, note: None, created_at: 1,
        }).unwrap();

        // Re-derivation: b now stacks on "a" only — "old" must go, the
        // declared edge must survive, and the same stacked target as a
        // declared one is a distinct row.
        let changed = store.set_stacked_deps("b", &["a".to_string()], 2).unwrap();
        assert!(changed);

        let mut kinds: Vec<(String, DepKind)> = store.deps_for_session("b").unwrap()
            .into_iter().map(|d| (d.depends_on, d.kind)).collect();
        kinds.sort();
        assert_eq!(kinds, vec![
            ("a".to_string(), DepKind::Declared),
            ("a".to_string(), DepKind::Stacked),
        ]);

        // Unchanged re-derivation reports no change.
        assert!(!store.set_stacked_deps("b", &["a".to_string()], 3).unwrap());
        // Empty re-derivation drops all stacked edges, keeps declared.
        assert!(store.set_stacked_deps("b", &[], 4).unwrap());
        let remaining = store.deps_for_session("b").unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].kind, DepKind::Declared);
    }

    #[test]
    fn update_session_activity_targets_columns_and_never_inserts_or_touches_terminal_rows() {
        let store = test_store();
        // Missing row: no write, and crucially no INSERT — a full-row upsert
        // here is how a concurrently-deleted session got resurrected.
        assert!(!store.update_session_activity("ghost", ActivityState::Working, None, Some(1)).unwrap());
        assert!(store.get_session("ghost").unwrap().is_none());

        let mut s = Session {
            id: "s1".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: SessionStatus::PrOpen,
            agent_type: "c".into(), cost_usd: 3.5, started_at: 0,
            pr_number: Some(9), pr_id: Some(9), workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None,
            gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        store.upsert_session(&s).unwrap();
        assert!(store.update_session_activity("s1", ActivityState::Blocked, Some("stuck"), Some(42)).unwrap());
        let fetched = store.get_session("s1").unwrap().unwrap();
        assert_eq!(fetched.activity, ActivityState::Blocked);
        assert_eq!(fetched.activity_note.as_deref(), Some("stuck"));
        assert_eq!(fetched.activity_since, Some(42));
        assert!(matches!(fetched.status, SessionStatus::PrOpen), "non-activity columns untouched");
        assert_eq!(fetched.cost_usd, 3.5);

        // Terminal row: guarded at write time, not just at the read that
        // preceded it in another process.
        s.status = SessionStatus::Terminated;
        s.activity = ActivityState::Blocked;
        s.activity_note = Some("stuck".into());
        s.activity_since = Some(42);
        store.upsert_session(&s).unwrap();
        assert!(!store.update_session_activity("s1", ActivityState::Working, None, Some(99)).unwrap());
        assert_eq!(store.get_session("s1").unwrap().unwrap().activity, ActivityState::Blocked);
    }

    #[test]
    fn deleting_an_orchestrator_purges_its_workers_edges() {
        let store = test_store();
        let mut w = Session {
            id: "w1".into(), orchestrator_id: Some("orch".into()), name: "w1".into(),
            repo: "r".into(), status: SessionStatus::Working,
            agent_type: "c".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None,
            gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        store.upsert_session(&w).unwrap();
        w.id = "w2".into();
        w.name = "w2".into();
        store.upsert_session(&w).unwrap();
        store.add_session_dep(&SessionDep {
            session_id: "w2".into(), depends_on: "w1".into(),
            kind: DepKind::Declared, note: None, created_at: 1,
        }).unwrap();

        store.delete_orchestrator("orch").unwrap();
        assert!(store.list_session_deps().unwrap().is_empty());
    }

    /// `Session.orchestrator_id` now also points at a sub-orchestrator's
    /// parent (see the nested-orchestration design spec), not only a plain
    /// worker's. Deleting a root orchestrator must not sweep up a
    /// sub-orchestrator it happens to find via `orchestrator_id = root` —
    /// that would silently delete a live, still-running sub-orchestrator's
    /// session row (and its messages/fleet row) while leaving its
    /// `orchestrators`-table row orphaned forever, with no session behind
    /// it.
    #[test]
    fn deleting_an_orchestrator_does_not_touch_a_sub_orchestrators_session_or_row() {
        let store = test_store();
        let worker = Session {
            id: "worker-a".into(), orchestrator_id: Some("root".into()), name: "worker-a".into(),
            repo: "r".into(), status: SessionStatus::Working,
            agent_type: "c".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None,
            gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        store.upsert_session(&worker).unwrap();
        let mut sub = worker.clone();
        sub.id = "sub1".into();
        sub.name = "sub1".into();
        store.upsert_orchestrator(&Orchestrator { id: "sub1".into(), name: "sub1".into(), created_at: 0 }).unwrap();
        store.upsert_session(&sub).unwrap();

        store.delete_orchestrator("root").unwrap();

        assert!(store.get_session("worker-a").unwrap().is_none(), "a plain worker must still be deleted");
        assert!(store.get_session("sub1").unwrap().is_some(), "a sub-orchestrator's session row must survive");
        assert!(store.is_orchestrator("sub1").unwrap(), "the sub-orchestrator's own row must be untouched");
    }

    #[test]
    fn deleting_a_session_purges_edges_in_both_directions() {
        let store = test_store();
        for (from, to) in [("b", "a"), ("a", "c")] {
            store.add_session_dep(&SessionDep {
                session_id: from.into(), depends_on: to.into(),
                kind: DepKind::Declared, note: None, created_at: 1,
            }).unwrap();
        }
        store.delete_session("a").unwrap();
        assert!(store.list_session_deps().unwrap().is_empty(),
                "edges from AND to the deleted session must go");
    }

    #[test]
    fn pr_watch_upsert_is_idempotent_and_lists() {
        let store = test_store();
        let w = PrWatch {
            repo: "o/r".into(), pr_number: 7,
            pr_url: "https://github.com/o/r/pull/7".into(),
            opener_session_id: Some("sess-a".into()), created_at: 1000,
        };
        store.upsert_pr_watch(&w).unwrap();
        store.upsert_pr_watch(&w).unwrap(); // same key — must not duplicate
        assert_eq!(store.list_pr_watches().unwrap(), vec![w]);
    }

    #[test]
    fn pr_watch_same_pr_different_openers_are_distinct_rows() {
        let store = test_store();
        let a = PrWatch { repo: "o/r".into(), pr_number: 7, pr_url: "u".into(), opener_session_id: Some("sess-a".into()), created_at: 1 };
        let b = PrWatch { opener_session_id: Some("sess-b".into()), ..a.clone() };
        let unowned = PrWatch { opener_session_id: None, ..a.clone() };
        store.upsert_pr_watch(&a).unwrap();
        store.upsert_pr_watch(&b).unwrap();
        store.upsert_pr_watch(&unowned).unwrap();
        store.upsert_pr_watch(&unowned).unwrap(); // unowned must dedup too
        assert_eq!(store.list_pr_watches().unwrap().len(), 3);
    }

    #[test]
    fn pr_watch_delete_removes_only_the_callers_row() {
        let store = test_store();
        let a = PrWatch { repo: "o/r".into(), pr_number: 7, pr_url: "u".into(), opener_session_id: Some("sess-a".into()), created_at: 1 };
        let b = PrWatch { opener_session_id: Some("sess-b".into()), ..a.clone() };
        store.upsert_pr_watch(&a).unwrap();
        store.upsert_pr_watch(&b).unwrap();
        assert_eq!(store.delete_pr_watch("o/r", 7, Some("sess-a")).unwrap(), 1);
        assert_eq!(store.list_pr_watches().unwrap(), vec![b]);
    }

    #[test]
    fn pr_watch_delete_for_pr_removes_all_openers() {
        let store = test_store();
        let a = PrWatch { repo: "o/r".into(), pr_number: 7, pr_url: "u".into(), opener_session_id: Some("sess-a".into()), created_at: 1 };
        let b = PrWatch { opener_session_id: None, ..a.clone() };
        let other = PrWatch { pr_number: 8, ..a.clone() };
        store.upsert_pr_watch(&a).unwrap();
        store.upsert_pr_watch(&b).unwrap();
        store.upsert_pr_watch(&other).unwrap();
        assert_eq!(store.delete_pr_watches_for_pr("o/r", 7).unwrap(), 2);
        assert_eq!(store.list_pr_watches().unwrap(), vec![other]);
    }

    #[test]
    fn orchestrator_plan_round_trips_and_upsert_preserves_registered_at() {
        let store = test_store();
        assert!(store.get_orchestrator_plan("orch").unwrap().is_none());

        store.register_orchestrator_plan("orch", "/plan.md", 100).unwrap();
        let plan = store.get_orchestrator_plan("orch").unwrap().unwrap();
        assert_eq!(plan.file_path, "/plan.md");
        assert_eq!(plan.registered_at, 100);
        assert_eq!(plan.updated_at, 100);

        store.register_orchestrator_plan("orch", "/other-plan.md", 200).unwrap();
        let plan = store.get_orchestrator_plan("orch").unwrap().unwrap();
        assert_eq!(plan.file_path, "/other-plan.md");
        assert_eq!(plan.registered_at, 100, "registered_at must not move on re-register");
        assert_eq!(plan.updated_at, 200);

        assert!(store.unregister_orchestrator_plan("orch").unwrap());
        assert!(store.get_orchestrator_plan("orch").unwrap().is_none());
        assert!(!store.unregister_orchestrator_plan("orch").unwrap());
    }

    #[test]
    fn register_orchestrator_runtime_requires_persisted_orchestrator() {
        let store = test_store();
        let runtime = OrchestratorRuntimeIdentity {
            orchestrator_id: "orch".into(),
            runtime_id: "r1".into(),
            server_epoch: "1:1".into(),
            physical_tmux_name: "orch".into(),
            pane_id: "%1".into(),
            root_pid: 100,
            root_created_at: 0,
            registered_at: 0,
        };
        assert!(store.register_orchestrator_runtime(&runtime).is_err());

        store.upsert_orchestrator(&Orchestrator { id: "orch".into(), name: "orch".into(), created_at: 0 }).unwrap();
        store.register_orchestrator_runtime(&runtime).unwrap();
        let persisted = store.orchestrator_runtime_identity("orch").unwrap().unwrap();
        assert_eq!(persisted, runtime);
    }

    #[test]
    fn delete_dead_orchestrator_runtime_clears_matching_row() {
        let store = test_store();
        store.upsert_orchestrator(&Orchestrator { id: "orch".into(), name: "orch".into(), created_at: 0 }).unwrap();
        let runtime = OrchestratorRuntimeIdentity {
            orchestrator_id: "orch".into(), runtime_id: "r1".into(), server_epoch: "1:1".into(),
            physical_tmux_name: "orch".into(), pane_id: "%1".into(), root_pid: 100,
            root_created_at: 0, registered_at: 0,
        };
        store.register_orchestrator_runtime(&runtime).unwrap();

        assert!(store.delete_dead_orchestrator_runtime("orch", 100).unwrap());
        assert!(store.orchestrator_runtime_identity("orch").unwrap().is_none());
    }

    #[test]
    fn delete_dead_orchestrator_runtime_is_noop_on_pid_mismatch() {
        let store = test_store();
        store.upsert_orchestrator(&Orchestrator { id: "orch".into(), name: "orch".into(), created_at: 0 }).unwrap();
        let runtime = OrchestratorRuntimeIdentity {
            orchestrator_id: "orch".into(), runtime_id: "r1".into(), server_epoch: "1:1".into(),
            physical_tmux_name: "orch".into(), pane_id: "%1".into(), root_pid: 100,
            root_created_at: 0, registered_at: 0,
        };
        store.register_orchestrator_runtime(&runtime).unwrap();

        assert!(!store.delete_dead_orchestrator_runtime("orch", 999).unwrap());
        assert_eq!(store.orchestrator_runtime_identity("orch").unwrap().unwrap(), runtime);
    }

    #[test]
    fn delete_dead_orchestrator_runtime_is_noop_when_no_row_exists() {
        let store = test_store();
        assert!(!store.delete_dead_orchestrator_runtime("ghost", 100).unwrap());
    }

    #[test]
    fn upsert_session_round_trips_success_criteria_and_outcome() {
        let store = test_store();
        let s = Session {
            id: "s1".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: SessionStatus::Working,
            agent_type: "claude-code".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: vec![SuccessCriterion::PrMerged, SuccessCriterion::Check("x".into())],
            completion_outcome: Some(CompletionOutcome {
                summary: "done".into(), evidence: vec!["https://x".into()], completed_at: 123,
            }),
            self_completed_at: Some(456),
            cleanup_result: None, cleanup_at: None,
        };
        store.upsert_session(&s).unwrap();
        let found = store.get_session("s1").unwrap().unwrap();
        assert_eq!(found.success_criteria, s.success_criteria);
        assert_eq!(found.completion_outcome, s.completion_outcome);
        assert_eq!(found.self_completed_at, s.self_completed_at);
    }

    #[test]
    fn pre_existing_row_with_null_new_columns_reads_back_with_defaults() {
        let store = test_store();
        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO sessions (id, name, repo, status, agent_type, started_at) \
                 VALUES ('s1', 'w', 'r', 'working', 'claude-code', 0)",
                [],
            ).unwrap();
        }
        let found = store.get_session("s1").unwrap().unwrap();
        assert_eq!(found.success_criteria, Vec::<SuccessCriterion>::new());
        assert_eq!(found.completion_outcome, None);
        assert_eq!(found.self_completed_at, None);
    }

    #[test]
    fn record_message_delivered_counts_per_session() {
        let store = test_store();
        store.record_message_delivered("orch-1").unwrap();
        store.record_message_delivered("orch-1").unwrap();
        store.record_message_delivered("worker-1").unwrap();

        let counts = store.message_delivered_counts().unwrap();
        assert_eq!(counts.get("orch-1"), Some(&2));
        assert_eq!(counts.get("worker-1"), Some(&1));
        assert_eq!(counts.get("never-sent"), None);
    }

    #[test]
    fn machine_health_round_trips_and_can_be_overwritten() {
        let store = test_store();
        store.set_machine_health("m1", MachineHealth::Reconnecting).unwrap();
        store.set_machine_health("m2", MachineHealth::Unreachable).unwrap();
        let health = store.machine_health().unwrap();
        assert_eq!(health.get("m1"), Some(&MachineHealth::Reconnecting));
        assert_eq!(health.get("m2"), Some(&MachineHealth::Unreachable));

        store.set_machine_health("m1", MachineHealth::Connected).unwrap();
        assert_eq!(store.machine_health().unwrap().get("m1"), Some(&MachineHealth::Connected));
    }

    #[test]
    fn remove_machine_health_drops_the_row() {
        let store = test_store();
        store.set_machine_health("m1", MachineHealth::Unreachable).unwrap();
        store.remove_machine_health("m1").unwrap();
        assert!(!store.machine_health().unwrap().contains_key("m1"));
    }

    #[test]
    fn deleting_a_session_drops_its_message_counter() {
        let store = test_store();
        store.record_message_delivered("worker-1").unwrap();
        store.delete_session("worker-1").unwrap();
        assert!(store.message_delivered_counts().unwrap().is_empty());
    }

    #[test]
    fn deleting_an_orchestrator_drops_its_own_and_its_workers_message_counters() {
        let store = test_store();
        let mut worker = crate::types::Session {
            id: "w1".into(), orchestrator_id: Some("orch".into()), name: "w1".into(),
            repo: String::new(), status: crate::types::SessionStatus::Working,
            agent_type: "claude-code".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        store.upsert_session(&worker).unwrap();
        worker.id = "unrelated".into();
        worker.orchestrator_id = None;
        store.upsert_session(&worker).unwrap();
        store.record_message_delivered("orch").unwrap();
        store.record_message_delivered("w1").unwrap();
        store.record_message_delivered("unrelated").unwrap();

        store.delete_orchestrator("orch").unwrap();

        let counts = store.message_delivered_counts().unwrap();
        assert_eq!(counts.len(), 1);
        assert_eq!(counts.get("unrelated"), Some(&1));
    }

    /// The invariant `orchestrator_auth::authorize_orchestrator`'s
    /// race-recovery branch depends on: when two concurrent callers both see
    /// no persisted runtime identity and race to self-register, the loser's
    /// `ON CONFLICT DO NOTHING` insert must leave the WINNER's row readable
    /// (not silently wipe or merge it) — so re-reading after losing recovers
    /// real data instead of `None`.
    #[test]
    fn a_second_migration_registration_loses_but_the_winners_row_stays_readable() {
        use crate::types::{Orchestrator, OrchestratorRuntimeIdentity, Session, SessionStatus};

        let store = test_store();
        store.upsert_orchestrator(&Orchestrator { id: "orch".into(), name: "orch".into(), created_at: 0 }).unwrap();
        store.upsert_session(&Session {
            id: "orch".into(), orchestrator_id: None, name: "orch".into(),
            repo: String::new(), status: SessionStatus::Working,
            agent_type: "claude-code".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: Some(4242),
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        }).unwrap();

        let winner = OrchestratorRuntimeIdentity {
            orchestrator_id: "orch".into(), runtime_id: "migrated:winner".into(),
            server_epoch: "e".into(), physical_tmux_name: "orch".into(),
            pane_id: "%1".into(), root_pid: 4242, root_created_at: 0, registered_at: 1,
        };
        let loser = OrchestratorRuntimeIdentity {
            orchestrator_id: "orch".into(), runtime_id: "migrated:loser".into(),
            server_epoch: "e".into(), physical_tmux_name: "orch".into(),
            pane_id: "%1".into(), root_pid: 4242, root_created_at: 0, registered_at: 2,
        };
        assert!(store.register_migrated_orchestrator_runtime(&winner).unwrap(), "first registration should win");
        assert!(!store.register_migrated_orchestrator_runtime(&loser).unwrap(), "second registration should lose, not overwrite");

        let persisted = store.orchestrator_runtime_identity("orch").unwrap()
            .expect("the winner's row must still be readable after the loser's attempt");
        assert_eq!(persisted.runtime_id, "migrated:winner", "the loser must not have clobbered the winner's row");
    }

    #[test]
    fn recent_message_events_round_trips_and_orders_by_id() {
        let store = test_store();
        store.record_message_event(Some("orch"), "worker-1", "first", MessageKind::Freeform).unwrap();
        store.record_message_event(None, "worker-1", "second", MessageKind::Freeform).unwrap();
        store.record_message_event(Some("worker-1"), "orch", "third", MessageKind::Freeform).unwrap();

        let all = store.recent_message_events(0).unwrap();
        assert_eq!(all.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(), ["first", "second", "third"]);
        assert_eq!(all[1].from_id, None);
        assert_eq!(all[2].to_id, "orch");

        let since = store.recent_message_events(all[0].id).unwrap();
        assert_eq!(since.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(), ["second", "third"]);
    }

    #[test]
    fn message_events_are_pruned_to_the_most_recent_2000() {
        let store = test_store();
        for i in 0..2050 {
            store.record_message_event(None, "worker-1", &format!("msg {i}"), MessageKind::Freeform).unwrap();
        }
        let all = store.recent_message_events(0).unwrap();
        assert_eq!(all.len(), 2000, "table must be pruned, not grow unbounded");
        assert_eq!(all.last().unwrap().text, "msg 2049", "pruning must drop the oldest rows, not the newest");
    }

    #[test]
    fn record_and_read_back_a_message_events_kind() {
        let store = test_store();
        store.record_message_event(Some("orch"), "worker-1", "done", MessageKind::Completion).unwrap();
        store.record_message_event(None, "worker-1", "fyi", MessageKind::RequestForWork).unwrap();

        let all = store.recent_message_events(0).unwrap();
        assert_eq!(all[0].kind, MessageKind::Completion);
        assert_eq!(all[1].kind, MessageKind::RequestForWork);
    }

    #[test]
    fn pre_existing_message_event_row_with_null_kind_reads_as_freeform() {
        let store = test_store();
        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO message_events (from_id, to_id, text, delivered_at) VALUES (?1, ?2, ?3, ?4)",
                params![Option::<&str>::None, "worker-1", "legacy row", 0i64],
            ).unwrap();
        }
        let all = store.recent_message_events(0).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].kind, MessageKind::Freeform);
    }

    /// Inserts a `message_events` row with an exact `delivered_at`, bypassing
    /// `record_message_event`'s `SystemTime::now()` stamp — `has_completion_since`'s
    /// own tests need precise control over the timestamp to exercise the
    /// `since_ms` boundary.
    fn insert_message_event_at(store: &Store, from: Option<&str>, to: &str, kind: MessageKind, delivered_at: i64) {
        let conn = store.conn.lock().unwrap();
        let kind_str = serde_json::to_string(&kind).unwrap().replace('"', "");
        conn.execute(
            "INSERT INTO message_events (from_id, to_id, text, delivered_at, kind) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![from, to, "msg", delivered_at, kind_str],
        ).unwrap();
    }

    #[test]
    fn has_completion_since_finds_a_matching_recent_completion() {
        let store = test_store();
        insert_message_event_at(&store, Some("w1"), "orch", MessageKind::Completion, 1000);
        assert!(store.has_completion_since("w1", "orch", 500).unwrap());
    }

    #[test]
    fn has_completion_since_ignores_a_completion_before_the_since_bound() {
        let store = test_store();
        insert_message_event_at(&store, Some("w1"), "orch", MessageKind::Completion, 1000);
        assert!(!store.has_completion_since("w1", "orch", 2000).unwrap());
    }

    #[test]
    fn has_completion_since_ignores_a_different_recipient() {
        let store = test_store();
        insert_message_event_at(&store, Some("w1"), "orch-a", MessageKind::Completion, 1000);
        assert!(!store.has_completion_since("w1", "orch-b", 500).unwrap());
    }

    #[test]
    fn has_completion_since_ignores_a_non_completion_kind() {
        let store = test_store();
        insert_message_event_at(&store, Some("w1"), "orch", MessageKind::Status, 1000);
        assert!(!store.has_completion_since("w1", "orch", 500).unwrap());
    }

    #[test]
    fn register_and_list_human_console_identity_round_trips() {
        let store = test_store();
        let identity = HumanConsoleIdentity {
            id: "hci-100-5".into(),
            root_pid: 100,
            root_created_at: 5,
            process_name: "ninox-gui".into(),
            registered_at: 7,
        };
        store.register_human_console_identity(&identity).unwrap();
        assert_eq!(store.list_human_console_identities().unwrap(), vec![identity]);
    }

    fn pending_user_request(id: &str) -> fleet::UserRequestRow {
        fleet::UserRequestRow {
            id: id.to_string(),
            requester_session_id: "worker-a".to_string(),
            prompt: "need a human".to_string(),
            mode: UserRequestMode::Sync,
            status: UserRequestStatus::Pending,
            answer: None,
            answered_at: None,
            answer_delivered_at: None,
            created_at: 0,
        }
    }

    fn user_request_from(id: &str, requester: &str) -> fleet::UserRequestRow {
        fleet::UserRequestRow { requester_session_id: requester.to_string(), ..pending_user_request(id) }
    }

    #[test]
    fn deleting_a_session_purges_its_user_requests() {
        let store = test_store();
        store.insert_user_request(&user_request_from("ur-gone", "worker-a")).unwrap();
        store.insert_user_request(&user_request_from("ur-kept", "worker-b")).unwrap();

        store.delete_session("worker-a").unwrap();

        assert!(store.get_user_request("ur-gone").unwrap().is_none());
        assert!(store.get_user_request("ur-kept").unwrap().is_some());
    }

    #[test]
    fn deleting_an_orchestrator_purges_its_own_and_its_workers_user_requests() {
        let store = test_store();
        let mut worker = crate::types::Session {
            id: "w1".into(), orchestrator_id: Some("orch".into()), name: "w1".into(),
            repo: String::new(), status: crate::types::SessionStatus::Working,
            agent_type: "claude-code".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        store.upsert_session(&worker).unwrap();
        worker.id = "unrelated".into();
        worker.orchestrator_id = None;
        store.upsert_session(&worker).unwrap();
        store.insert_user_request(&user_request_from("ur-orch", "orch")).unwrap();
        store.insert_user_request(&user_request_from("ur-w1", "w1")).unwrap();
        store.insert_user_request(&user_request_from("ur-unrelated", "unrelated")).unwrap();

        store.delete_orchestrator("orch").unwrap();

        let left: Vec<_> = store.open_user_requests(None).unwrap().into_iter().map(|r| r.id).collect();
        assert_eq!(left, vec!["ur-unrelated"]);
    }

    #[test]
    fn answer_user_request_rejected_without_a_registered_console() {
        let store = test_store();
        store.insert_user_request(&pending_user_request("ur-1")).unwrap();

        let result = store.answer_user_request("ur-1", "go ahead");

        assert!(result.is_err(), "an unregistered caller must never be able to answer a request");
        let row = store.get_user_request("ur-1").unwrap().unwrap();
        assert_eq!(row.status, UserRequestStatus::Pending, "a rejected answer attempt must leave the row untouched");
        assert!(row.answer.is_none());
        assert!(row.answered_at.is_none());
    }

    #[test]
    fn answering_an_already_answered_row_does_not_overwrite_it() {
        let store = test_store();
        crate::human_console_auth::register_human_console(&store, "ninox-gui").unwrap();
        store.insert_user_request(&pending_user_request("ur-2")).unwrap();

        store.answer_user_request("ur-2", "first").unwrap();
        let first = store.get_user_request("ur-2").unwrap().unwrap();
        assert_eq!(first.status, UserRequestStatus::Answered);
        assert_eq!(first.answer.as_deref(), Some("first"));

        store.answer_user_request("ur-2", "second").unwrap();
        let second = store.get_user_request("ur-2").unwrap().unwrap();

        assert_eq!(second.answer.as_deref(), Some("first"), "a second answer must never overwrite the first");
        assert_eq!(second.answered_at, first.answered_at, "a second answer must never overwrite the original timestamp");
    }

    #[test]
    fn concurrent_wait_poll_never_observes_a_torn_answered_row() {
        let store = std::sync::Arc::new(test_store());
        crate::human_console_auth::register_human_console(&store, "ninox-gui").unwrap();
        store.insert_user_request(&pending_user_request("ur-3")).unwrap();

        let writer_store = store.clone();
        let writer = std::thread::spawn(move || {
            writer_store.answer_user_request("ur-3", "the answer").unwrap();
        });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut observed_answered = false;
        while std::time::Instant::now() < deadline {
            let row = store.get_user_request("ur-3").unwrap().unwrap();
            if row.status != UserRequestStatus::Pending {
                assert_eq!(row.status, UserRequestStatus::Answered);
                assert!(row.answer.is_some(), "a reader must never observe Answered with answer == None");
                observed_answered = true;
                break;
            }
        }
        writer.join().unwrap();
        assert!(observed_answered, "polling thread never observed the answer land within the deadline");
    }

    #[test]
    fn finish_nudge_claim_is_once_only_and_cleared_with_the_session() {
        let store = test_store();
        assert!(store.claim_finish_nudge("w1", 1).unwrap());
        assert!(!store.claim_finish_nudge("w1", 2).unwrap());
        store.delete_session("w1").unwrap();
        assert!(store.claim_finish_nudge("w1", 3).unwrap(), "deleting the session clears its claim");
        store.upsert_orchestrator(&Orchestrator { id: "o1".into(), name: "o1".into(), created_at: 0 }).unwrap();
        store.delete_orchestrator("o1").unwrap();
        assert!(store.claim_finish_nudge("o1", 4).unwrap());
        store.delete_orchestrator("o1").unwrap();
        assert!(store.claim_finish_nudge("o1", 5).unwrap(), "deleting the orchestrator clears its claim");
    }

    #[test]
    fn list_orchestrator_runtime_identities_returns_all_registered_rows() {
        let store = test_store();
        let mut expected = Vec::new();
        for (id, pid) in [("orch-a", 100), ("orch-b", 200)] {
            store.upsert_orchestrator(&Orchestrator { id: id.into(), name: id.into(), created_at: 0 }).unwrap();
            let runtime = OrchestratorRuntimeIdentity {
                orchestrator_id: id.into(), runtime_id: format!("r-{id}"), server_epoch: "1:1".into(),
                physical_tmux_name: id.into(), pane_id: "%1".into(), root_pid: pid,
                root_created_at: 0, registered_at: 0,
            };
            store.register_orchestrator_runtime(&runtime).unwrap();
            expected.push(runtime);
        }
        let mut listed = store.list_orchestrator_runtime_identities().unwrap();
        listed.sort_by(|a, b| a.orchestrator_id.cmp(&b.orchestrator_id));
        assert_eq!(listed, expected);
    }

    /// `answer_user_request` above is the only function allowed to mark a
    /// `user_requests` row answered — gated on a live, ancestry-verified
    /// console process. The two file-based relay paths that carry arbitrary
    /// third-party bytes (`messaging.rs`'s `ninox send`/poller delivery, and
    /// `inbox.rs`'s worktree drain) must never call it directly, or a
    /// relayed message could forge an answer. Lives here, not inside either
    /// of those files, so reading their source doesn't trip over this very
    /// test naming the function it's checking for.
    mod relay_boundary_tests {
        #[test]
        fn messaging_and_inbox_never_reference_answer_user_request() {
            for path in ["src/messaging.rs", "src/inbox.rs"] {
                let full = concat!(env!("CARGO_MANIFEST_DIR"), "/").to_string() + path;
                let body = std::fs::read_to_string(&full).unwrap();
                assert!(!body.contains("answer_user_request"), "{path} must never call answer_user_request");
            }
        }
    }
}
