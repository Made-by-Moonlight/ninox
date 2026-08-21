use crate::types::*;
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};

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
            CREATE TABLE IF NOT EXISTS pooled_checkouts (
                path TEXT PRIMARY KEY,
                source_repo TEXT NOT NULL,
                common_git_dir TEXT NOT NULL,
                slot INTEGER NOT NULL CHECK(slot >= 0),
                worktree_git_dir TEXT,
                worktree_identity TEXT,
                state TEXT NOT NULL CHECK(state IN ('provisioning','leased','free','quarantined')),
                session_id TEXT,
                lease_id TEXT,
                branch TEXT,
                quarantine_reason TEXT,
                UNIQUE(common_git_dir, slot),
                CHECK((session_id IS NULL) = (lease_id IS NULL)),
                CHECK(state NOT IN ('provisioning','leased') OR
                      (session_id IS NOT NULL AND lease_id IS NOT NULL AND branch IS NOT NULL))
            );
            CREATE UNIQUE INDEX IF NOT EXISTS pooled_checkouts_active_session
                ON pooled_checkouts(session_id)
                WHERE session_id IS NOT NULL AND state IN ('provisioning','leased');
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
        ] {
            if !Self::column_exists(&conn, "sessions", col)? {
                conn.execute(ddl, [])?;
            }
        }
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let exists = stmt
            .query_map([], |r| r.get::<_, String>(1))?
            .filter_map(|r| r.ok())
            .any(|c| c == column);
        Ok(exists)
    }

    pub fn upsert_session(&self, s: &Session) -> Result<()> {
        let status = serde_json::to_string(&s.status)?.replace('"', "");
        let gate_status = s.gate_status.as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO sessions (id,orchestrator_id,name,repo,status,agent_type,
             cost_usd,started_at,pr_number,pr_id,workspace_path,pid,model,context_tokens,
             catalogue_path,context_used_pct,context_total_tokens,context_window_size,
             claude_session_id,summary,terminal_at,gate_status)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22)
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
             gate_status=excluded.gate_status",
            params![
                s.id, s.orchestrator_id, s.name, s.repo, status, s.agent_type,
                s.cost_usd, s.started_at, s.pr_number, s.pr_id,
                s.workspace_path, s.pid, s.model, s.context_tokens,
                s.catalogue_path, s.context_used_pct, s.context_total_tokens,
                s.context_window_size, s.claude_session_id, s.summary, s.terminal_at,
                gate_status
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
             claude_session_id,summary,terminal_at,gate_status
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
            ))
        })?;
        rows.map(|r| {
            let (id, orchestrator_id, name, repo, status_str, agent_type,
                 cost_usd, started_at, pr_number, pr_id, workspace_path, pid,
                 model, context_tokens, catalogue_path, context_used_pct,
                 context_total_tokens, context_window_size, claude_session_id,
                 summary, terminal_at, gate_status_str) = r?;
            let status = serde_json::from_str(&format!("\"{status_str}\""))
                .unwrap_or(SessionStatus::Working);
            let gate_status = gate_status_str
                .and_then(|s| serde_json::from_str(&s).ok());
            Ok(Session {
                id, orchestrator_id, name, repo, status, agent_type,
                cost_usd, started_at, pr_number, pr_id, workspace_path, pid,
                model, context_tokens: context_tokens.map(|v| v.max(0) as u64),
                catalogue_path,
                context_used_pct,
                context_total_tokens: context_total_tokens.map(|v| v.max(0) as u64),
                context_window_size: context_window_size.map(|v| v.max(0) as u64),
                claude_session_id,
                summary,
                terminal_at,
                gate_status,
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
             claude_session_id,summary,terminal_at,gate_status
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
            ))
        })?;
        match rows.next() {
            None => Ok(None),
            Some(r) => {
                let (id, orchestrator_id, name, repo, status_str, agent_type,
                     cost_usd, started_at, pr_number, pr_id, workspace_path, pid,
                     model, context_tokens, catalogue_path, context_used_pct,
                     context_total_tokens, context_window_size, claude_session_id,
                     summary, terminal_at, gate_status_str) = r?;
                let status = serde_json::from_str(&format!("\"{status_str}\""))
                    .unwrap_or(SessionStatus::Working);
                let gate_status = gate_status_str
                    .and_then(|s| serde_json::from_str(&s).ok());
                Ok(Some(Session {
                    id, orchestrator_id, name, repo, status, agent_type,
                    cost_usd, started_at, pr_number, pr_id, workspace_path, pid,
                    model, context_tokens: context_tokens.map(|v| v.max(0) as u64),
                    catalogue_path,
                    context_used_pct,
                    context_total_tokens: context_total_tokens.map(|v| v.max(0) as u64),
                    context_window_size: context_window_size.map(|v| v.max(0) as u64),
                    claude_session_id,
                    summary,
                    terminal_at,
                    gate_status,
                }))
            }
        }
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

    pub fn sessions_by_orchestrator(&self, orchestrator_id: &str) -> Result<Vec<Session>> {
        let sessions = self.list_sessions()?;
        Ok(sessions.into_iter().filter(|s| s.orchestrator_id.as_deref() == Some(orchestrator_id)).collect())
    }

    pub fn delete_session(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM sessions WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn delete_orchestrator(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM sessions WHERE orchestrator_id = ?1", [id])?;
        conn.execute("DELETE FROM sessions WHERE id = ?1", [id])?;
        conn.execute("DELETE FROM orchestrators WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn upsert_pr(&self, pr: &PR) -> Result<()> {
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

    pub fn upsert_ci_status(&self, ci: &CIStatus) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO ci_status
             (pr_id, total, passing, failing, pending)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![ci.pr_id, ci.total, ci.passing, ci.failing, ci.pending],
        )?;
        Ok(())
    }

    pub fn upsert_comment(&self, c: &Comment) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO review_comments
             (id, pr_id, author, body, path, line, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![c.id, c.pr_id, c.author, c.body, c.path, c.line, c.created_at],
        )?;
        Ok(())
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

    /// Atomically reserves the lowest-numbered free checkout for a session.
    ///
    /// The checkout remains `provisioning` until its Git branch is prepared
    /// and `finalize_pooled_checkout` records the verified worktree identity.
    pub fn claim_lowest_free_pooled_checkout(
        &self,
        common_git_dir: &Path,
        session_id: &str,
        branch: &str,
    ) -> Result<Option<PooledCheckoutLease>> {
        validate_lease_inputs(session_id, branch)?;
        let common_git_dir = canonical_db_path(common_git_dir)?;
        let lease_id = uuid::Uuid::new_v4().to_string();
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let selected = tx
            .query_row(
                &format!(
                    "{POOLED_CHECKOUT_COLUMNS}
                     WHERE common_git_dir = ?1 AND state = 'free'
                     ORDER BY slot ASC LIMIT 1"
                ),
                [&common_git_dir],
                pooled_checkout_row,
            )
            .optional()?;
        let Some(mut record) = selected.map(raw_pooled_checkout).transpose()? else {
            tx.commit()?;
            return Ok(None);
        };
        let changed = tx.execute(
            "UPDATE pooled_checkouts
             SET state='provisioning', session_id=?2, lease_id=?3, branch=?4,
                 quarantine_reason=NULL
             WHERE path=?1 AND state='free'",
            params![path_text(&record.path)?, session_id, lease_id, branch],
        )?;
        anyhow::ensure!(changed == 1, "free pooled checkout changed during claim");
        tx.commit()?;

        record.state = PooledCheckoutState::Provisioning;
        record.session_id = Some(session_id.to_string());
        record.lease_id = Some(lease_id);
        record.branch = Some(branch.to_string());
        Ok(Some(record_into_lease(record)?))
    }

    /// Atomically allocates the next slot and reserves its deterministic path.
    pub fn reserve_pooled_checkout(
        &self,
        source_repo: &Path,
        common_git_dir: &Path,
        repositories_root: &Path,
        session_id: &str,
        branch: &str,
    ) -> Result<PooledCheckoutLease> {
        validate_lease_inputs(session_id, branch)?;
        let source_repo = canonical_db_path(source_repo)?;
        let common_git_dir = canonical_db_path(common_git_dir)?;
        let repositories_root = canonical_db_path(repositories_root)?;
        let repository_name = Path::new(&source_repo)
            .file_name()
            .and_then(|name| name.to_str())
            .context("source repository has no UTF-8 directory name")?;
        let lease_id = uuid::Uuid::new_v4().to_string();
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let slot: u32 = tx.query_row(
            "SELECT COALESCE(MAX(slot), -1) + 1
             FROM pooled_checkouts WHERE common_git_dir = ?1",
            [&common_git_dir],
            |row| row.get(0),
        )?;
        let path = PathBuf::from(&repositories_root)
            .join(format!("{repository_name}-w{}", slot + 1));
        let path_str = path_text(&path)?;
        tx.execute(
            "INSERT INTO pooled_checkouts(
                path,source_repo,common_git_dir,slot,state,session_id,lease_id,branch
             ) VALUES(?1,?2,?3,?4,'provisioning',?5,?6,?7)",
            params![
                path_str,
                source_repo,
                common_git_dir,
                slot,
                session_id,
                lease_id,
                branch,
            ],
        )?;
        tx.commit()?;
        Ok(PooledCheckoutLease {
            path,
            source_repo: PathBuf::from(source_repo),
            common_git_dir: PathBuf::from(common_git_dir),
            slot,
            worktree_git_dir: None,
            worktree_identity: None,
            session_id: session_id.to_string(),
            lease_id,
            branch: branch.to_string(),
        })
    }

    /// Completes a matching reservation after Git identity was established.
    pub fn finalize_pooled_checkout(
        &self,
        path: &Path,
        session_id: &str,
        lease_id: &str,
        worktree_git_dir: &Path,
        worktree_identity: &str,
    ) -> Result<bool> {
        anyhow::ensure!(!worktree_identity.is_empty(), "worktree identity cannot be empty");
        let path = absolute_db_path(path)?;
        let worktree_git_dir = canonical_db_path(worktree_git_dir)?;
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE pooled_checkouts
             SET state='leased', worktree_git_dir=?4, worktree_identity=?5,
                 quarantine_reason=NULL
             WHERE path=?1 AND state='provisioning'
               AND session_id=?2 AND lease_id=?3",
            params![path, session_id, lease_id, worktree_git_dir, worktree_identity],
        )?;
        Ok(changed == 1)
    }

    pub fn pooled_checkout_by_session(
        &self,
        session_id: &str,
    ) -> Result<Option<PooledCheckoutRecord>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            &format!(
                "{POOLED_CHECKOUT_COLUMNS}
                 WHERE session_id=?1 AND state IN ('provisioning','leased')"
            ),
            [session_id],
            pooled_checkout_row,
        )
        .optional()?
        .map(raw_pooled_checkout)
        .transpose()
    }

    pub fn pooled_checkout_by_path(&self, path: &Path) -> Result<Option<PooledCheckoutRecord>> {
        let path = absolute_db_path(path)?;
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            &format!("{POOLED_CHECKOUT_COLUMNS} WHERE path=?1"),
            [path],
            pooled_checkout_row,
        )
        .optional()?
        .map(raw_pooled_checkout)
        .transpose()
    }

    /// All records for one canonical source checkout, ordered by slot.
    pub fn pooled_checkouts_by_repo(
        &self,
        source_repo: &Path,
    ) -> Result<Vec<PooledCheckoutRecord>> {
        let source_repo = canonical_db_path(source_repo)?;
        self.query_pooled_checkouts(
            &format!("{POOLED_CHECKOUT_COLUMNS} WHERE source_repo=?1 ORDER BY slot ASC"),
            &source_repo,
        )
    }

    /// All records sharing a Git object database, ordered for reconciliation.
    pub fn pooled_checkouts_by_common_git_dir(
        &self,
        common_git_dir: &Path,
    ) -> Result<Vec<PooledCheckoutRecord>> {
        let common_git_dir = canonical_db_path(common_git_dir)?;
        self.query_pooled_checkouts(
            &format!("{POOLED_CHECKOUT_COLUMNS} WHERE common_git_dir=?1 ORDER BY slot ASC"),
            &common_git_dir,
        )
    }

    pub fn list_pooled_checkouts(&self) -> Result<Vec<PooledCheckoutRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "{POOLED_CHECKOUT_COLUMNS} ORDER BY common_git_dir ASC, slot ASC"
        ))?;
        let rows = stmt.query_map([], pooled_checkout_row)?;
        rows.map(|row| raw_pooled_checkout(row?)).collect()
    }

    /// Releases only the exact active capability. The old branch is retained
    /// in the record for reconciliation and is never deleted by the registry.
    pub fn release_pooled_checkout(
        &self,
        path: &Path,
        session_id: &str,
        lease_id: &str,
    ) -> Result<bool> {
        let path = absolute_db_path(path)?;
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE pooled_checkouts
             SET state='free', session_id=NULL, lease_id=NULL,
                 quarantine_reason=NULL
             WHERE path=?1 AND state='leased' AND session_id=?2 AND lease_id=?3",
            params![path, session_id, lease_id],
        )?;
        Ok(changed == 1)
    }

    /// Quarantines the current reservation only if its capability still
    /// matches, preventing stale cleanup from affecting a later lease.
    pub fn quarantine_pooled_checkout_lease(
        &self,
        path: &Path,
        session_id: &str,
        lease_id: &str,
        reason: &str,
    ) -> Result<bool> {
        anyhow::ensure!(!reason.trim().is_empty(), "quarantine reason cannot be empty");
        let path = absolute_db_path(path)?;
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE pooled_checkouts
             SET state='quarantined', session_id=NULL, lease_id=NULL,
                 quarantine_reason=?4
             WHERE path=?1 AND state IN ('provisioning','leased')
               AND session_id=?2 AND lease_id=?3",
            params![path, session_id, lease_id, reason],
        )?;
        Ok(changed == 1)
    }

    /// Reconciliation-only quarantine for a record not controlled by a live
    /// lease holder.
    pub fn quarantine_pooled_checkout(&self, path: &Path, reason: &str) -> Result<bool> {
        anyhow::ensure!(!reason.trim().is_empty(), "quarantine reason cannot be empty");
        let path = absolute_db_path(path)?;
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE pooled_checkouts
             SET state='quarantined', session_id=NULL, lease_id=NULL,
                 quarantine_reason=?2
             WHERE path=?1",
            params![path, reason],
        )?;
        Ok(changed == 1)
    }

    /// Returns a user-cleaned, identity-verified quarantined slot to the pool.
    /// Callers must perform the Git ownership and cleanliness checks first.
    pub fn restore_quarantined_pooled_checkout(&self, path: &Path) -> Result<bool> {
        let path = absolute_db_path(path)?;
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE pooled_checkouts
             SET state='free', quarantine_reason=NULL
             WHERE path=?1 AND state='quarantined'
               AND worktree_git_dir IS NOT NULL AND worktree_identity IS NOT NULL",
            [path],
        )?;
        Ok(changed == 1)
    }

    pub fn remove_failed_pooled_checkout(
        &self,
        path: &Path,
        session_id: &str,
        lease_id: &str,
    ) -> Result<bool> {
        let path = absolute_db_path(path)?;
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "DELETE FROM pooled_checkouts
             WHERE path=?1 AND state='provisioning'
               AND session_id=?2 AND lease_id=?3",
            params![path, session_id, lease_id],
        )?;
        Ok(changed == 1)
    }

    fn query_pooled_checkouts(
        &self,
        sql: &str,
        parameter: &str,
    ) -> Result<Vec<PooledCheckoutRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map([parameter], pooled_checkout_row)?;
        rows.map(|row| raw_pooled_checkout(row?)).collect()
    }
}

const POOLED_CHECKOUT_COLUMNS: &str =
    "SELECT path,source_repo,common_git_dir,slot,worktree_git_dir,
            worktree_identity,state,session_id,lease_id,branch,quarantine_reason
     FROM pooled_checkouts";

type RawPooledCheckout = (
    String,
    String,
    String,
    u32,
    Option<String>,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn pooled_checkout_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawPooledCheckout> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
    ))
}

fn raw_pooled_checkout(raw: RawPooledCheckout) -> Result<PooledCheckoutRecord> {
    let (
        path,
        source_repo,
        common_git_dir,
        slot,
        worktree_git_dir,
        worktree_identity,
        state,
        session_id,
        lease_id,
        branch,
        quarantine_reason,
    ) = raw;
    let state = match state.as_str() {
        "provisioning" => PooledCheckoutState::Provisioning,
        "leased" => PooledCheckoutState::Leased,
        "free" => PooledCheckoutState::Free,
        "quarantined" => PooledCheckoutState::Quarantined,
        other => anyhow::bail!("invalid pooled checkout state {other:?}"),
    };
    Ok(PooledCheckoutRecord {
        path: PathBuf::from(path),
        source_repo: PathBuf::from(source_repo),
        common_git_dir: PathBuf::from(common_git_dir),
        slot,
        worktree_git_dir: worktree_git_dir.map(PathBuf::from),
        worktree_identity,
        state,
        session_id,
        lease_id,
        branch,
        quarantine_reason,
    })
}

fn record_into_lease(record: PooledCheckoutRecord) -> Result<PooledCheckoutLease> {
    Ok(PooledCheckoutLease {
        path: record.path,
        source_repo: record.source_repo,
        common_git_dir: record.common_git_dir,
        slot: record.slot,
        worktree_git_dir: record.worktree_git_dir,
        worktree_identity: record.worktree_identity,
        session_id: record.session_id.context("active checkout has no session")?,
        lease_id: record.lease_id.context("active checkout has no lease")?,
        branch: record.branch.context("active checkout has no branch")?,
    })
}

fn validate_lease_inputs(session_id: &str, branch: &str) -> Result<()> {
    anyhow::ensure!(!session_id.is_empty(), "session id cannot be empty");
    anyhow::ensure!(!branch.is_empty(), "checkout branch cannot be empty");
    Ok(())
}

fn canonical_db_path(path: &Path) -> Result<String> {
    path.canonicalize()
        .with_context(|| format!("canonicalize {}", path.display()))
        .and_then(|path| path_text(&path))
}

fn absolute_db_path(path: &Path) -> Result<String> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    path_text(&path)
}

fn path_text(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .with_context(|| format!("path is not valid UTF-8: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

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
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None,
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
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None,
        };
        store.upsert_session(&s).unwrap();
        s.status = SessionStatus::Done;
        store.upsert_session(&s).unwrap();
        let list = store.list_sessions().unwrap();
        assert_eq!(list.len(), 1);
        assert!(matches!(list[0].status, SessionStatus::Done));
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
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None,
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
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None,
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
            model: Some("claude-fable-5".into()), context_tokens: Some(214_000), catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None,
        };
        store.upsert_session(&s).unwrap();
        let found = store.get_session("s1").unwrap().unwrap();
        assert_eq!(found.model.as_deref(), Some("claude-fable-5"));
        assert_eq!(found.context_tokens, Some(214_000));
    }

    #[test]
    fn catalogue_path_round_trips() {
        let store = test_store();
        let s = Session {
            id: "s2".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: SessionStatus::Working,
            agent_type: "claude-code".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None,
            catalogue_path: Some("/brains/x".into()),
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None,
        };
        store.upsert_session(&s).unwrap();
        let found = store.get_session("s2").unwrap().unwrap();
        assert_eq!(found.catalogue_path.as_deref(), Some("/brains/x"));
        // list path decodes it too
        assert_eq!(store.list_sessions().unwrap()[0].catalogue_path.as_deref(), Some("/brains/x"));
    }

    #[test]
    fn summary_round_trips() {
        let store = test_store();
        let s = Session {
            id: "s2b".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: SessionStatus::Working,
            agent_type: "claude-code".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: Some("Fix flaky CI on the auth suite".into()),
            terminal_at: None, gate_status: None,
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
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: Some("b7e0b3a0-0000-4000-8000-000000000001".into()),
            summary: None,
            terminal_at: None, gate_status: None,
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
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: Some(1_720_000_000_000), gate_status: None,
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
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None,
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
    fn context_fields_round_trip() {
        let store = test_store();
        let s = Session {
            id: "s1".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: SessionStatus::Working,
            agent_type: "claude-code".into(), cost_usd: 2.6, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: Some(62.0),
            context_total_tokens: Some(124_000),
            context_window_size: Some(200_000),
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None,
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
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None,
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
                model: model.map(String::from), context_tokens: None, catalogue_path: None,
                context_used_pct: None, context_total_tokens: None, context_window_size: None,
                claude_session_id: None, summary: None,
                terminal_at: None, gate_status: None,
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
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None,
            gate_status: None,
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
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None,
            gate_status: None,
        };
        store.upsert_session(&session).unwrap();
        let fetched = store.get_session("s2").unwrap().unwrap();
        assert_eq!(fetched.gate_status, None);
    }

    fn pool_fixture() -> (Store, std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        let root = tempdir().unwrap().keep();
        let source = root.join("source");
        let common = root.join("common.git");
        let slots = root.join("slots");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&common).unwrap();
        std::fs::create_dir_all(&slots).unwrap();
        (
            Store::open(root.join("pool.db")).unwrap(),
            source.canonicalize().unwrap(),
            common.canonicalize().unwrap(),
            slots.canonicalize().unwrap(),
        )
    }

    fn make_free(
        store: &Store,
        source: &Path,
        common: &Path,
        slots: &Path,
        session: &str,
    ) -> PooledCheckoutLease {
        let lease = store
            .reserve_pooled_checkout(source, common, slots, session, &format!("branch-{session}"))
            .unwrap();
        let admin = slots.join(format!("admin-{session}"));
        std::fs::create_dir_all(&admin).unwrap();
        assert!(store
            .finalize_pooled_checkout(
                &lease.path,
                &lease.session_id,
                &lease.lease_id,
                &admin,
                &format!("identity-{session}"),
            )
            .unwrap());
        assert!(store
            .release_pooled_checkout(&lease.path, &lease.session_id, &lease.lease_id)
            .unwrap());
        lease
    }

    #[test]
    fn pooled_checkout_claims_lowest_free_slot_atomically() {
        let (store, source, common, slots) = pool_fixture();
        let low = make_free(&store, &source, &common, &slots, "first");
        let high = make_free(&store, &source, &common, &slots, "second");
        assert_eq!((low.slot, high.slot), (0, 1));

        let claimed = store
            .claim_lowest_free_pooled_checkout(&common, "next", "branch-next")
            .unwrap()
            .unwrap();
        assert_eq!(claimed.slot, 0);
        assert_eq!(claimed.path, low.path);
        assert_eq!(
            store
                .pooled_checkout_by_session("next")
                .unwrap()
                .unwrap()
                .state,
            PooledCheckoutState::Provisioning,
        );
    }

    #[test]
    fn concurrent_pooled_claims_cannot_select_the_same_lowest_slot() {
        let root = tempdir().unwrap().keep();
        let db = root.join("pool.db");
        let source = root.join("source");
        let common = root.join("common.git");
        let slots = root.join("slots");
        for path in [&source, &common, &slots] {
            std::fs::create_dir_all(path).unwrap();
        }
        let first = Store::open(&db).unwrap();
        make_free(&first, &source, &common, &slots, "free-zero");
        make_free(&first, &source, &common, &slots, "free-one");
        let second = Store::open(&db).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

        let spawn =
            |store: Store, session: &'static str, barrier: std::sync::Arc<std::sync::Barrier>| {
                let common = common.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    store
                        .claim_lowest_free_pooled_checkout(
                            &common,
                            session,
                            &format!("branch-{session}"),
                        )
                        .unwrap()
                        .unwrap()
                })
            };
        let a = spawn(first, "claim-a", barrier.clone());
        let b = spawn(second, "claim-b", barrier);
        let mut claimed = [a.join().unwrap().slot, b.join().unwrap().slot];
        claimed.sort();
        assert_eq!(claimed, [0, 1]);
    }

    #[test]
    fn concurrent_pooled_reservations_get_unique_slots() {
        let root = tempdir().unwrap().keep();
        let db = root.join("pool.db");
        let source = root.join("source");
        let common = root.join("common.git");
        let slots = root.join("slots");
        for path in [&source, &common, &slots] {
            std::fs::create_dir_all(path).unwrap();
        }
        let first = Store::open(&db).unwrap();
        let second = Store::open(&db).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

        let spawn =
            |store: Store, session: &'static str, barrier: std::sync::Arc<std::sync::Barrier>| {
                let source = source.clone();
                let common = common.clone();
                let slots = slots.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    store
                        .reserve_pooled_checkout(
                            &source,
                            &common,
                            &slots,
                            session,
                            &format!("branch-{session}"),
                        )
                        .unwrap()
                })
            };
        let a = spawn(first, "a", barrier.clone());
        let b = spawn(second, "b", barrier);
        let mut slots = [a.join().unwrap().slot, b.join().unwrap().slot];
        slots.sort();
        assert_eq!(slots, [0, 1]);
    }

    #[test]
    fn pooled_release_requires_matching_session_and_lease() {
        let (store, source, common, slots) = pool_fixture();
        let lease = store
            .reserve_pooled_checkout(&source, &common, &slots, "owner", "branch-owner")
            .unwrap();
        let admin = slots.join("admin");
        std::fs::create_dir(&admin).unwrap();
        assert!(store
            .finalize_pooled_checkout(&lease.path, "owner", &lease.lease_id, &admin, "identity")
            .unwrap());

        assert!(!store
            .release_pooled_checkout(&lease.path, "other", &lease.lease_id)
            .unwrap());
        assert!(!store
            .release_pooled_checkout(&lease.path, "owner", "stale-lease")
            .unwrap());
        assert_eq!(
            store
                .pooled_checkout_by_path(&lease.path)
                .unwrap()
                .unwrap()
                .state,
            PooledCheckoutState::Leased,
        );
        assert!(store
            .release_pooled_checkout(&lease.path, "owner", &lease.lease_id)
            .unwrap());
        let free = store.pooled_checkout_by_path(&lease.path).unwrap().unwrap();
        assert_eq!(free.state, PooledCheckoutState::Free);
        assert_eq!(free.branch.as_deref(), Some("branch-owner"));
        assert!(free.session_id.is_none());
        assert!(free.lease_id.is_none());
    }
}
