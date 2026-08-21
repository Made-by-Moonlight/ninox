use crate::types::*;
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::{
    collections::HashMap,
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

fn allocation_lock_dir(database_path: &Path) -> Result<PathBuf> {
    let absolute = if database_path.is_absolute() {
        database_path.to_path_buf()
    } else {
        std::env::current_dir()?.join(database_path)
    };
    let parent = absolute
        .parent()
        .context("database path has no parent directory")?;
    let name = absolute
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("ninox.db");
    Ok(parent.join(format!(".{name}.worker-allocations")))
}

fn allocation_lock_path(lock_dir: &Path, incarnation_id: &str) -> PathBuf {
    lock_dir.join(format!("{incarnation_id}.lock"))
}

fn allocation_lock_is_reclaimable(
    lock_dir: &Path,
    incarnation_id: &str,
    expected_token: Option<&str>,
) -> Result<bool> {
    let Some(expected_token) = expected_token else {
        return Ok(false);
    };
    let path = allocation_lock_path(lock_dir, incarnation_id);
    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error.into()),
    };
    let mut token = String::new();
    file.read_to_string(&mut token)?;
    if token != expected_token {
        return Ok(true);
    }
    match file.try_lock() {
        Ok(()) => {
            file.unlock()?;
            Ok(true)
        }
        Err(std::fs::TryLockError::WouldBlock) => Ok(false),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

struct PendingAllocationLock {
    path: PathBuf,
    file: Option<File>,
}

impl PendingAllocationLock {
    fn persist(mut self) -> Option<File> {
        self.file.take()
    }
}

impl Drop for PendingAllocationLock {
    fn drop(&mut self) {
        if self.file.is_some() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

const CURRENT_WORKER_INCARNATION_COLUMNS: &[&str] = &[
    "session_id",
    "incarnation_id",
    "orchestrator_id",
    "started_at",
    "source_workspace",
    "workspace_path",
    "lease_id",
    "allocator_pid",
    "allocator_token",
    "checkout_backed",
    "state",
];

const LEGACY_WORKER_INCARNATION_COLUMNS: &[&str] = &[
    "session_id",
    "incarnation_id",
    "phase",
    "ui_outcome",
    "physical_tmux_name",
    "pane_id",
    "pane_pid",
    "workspace_path",
    "pool_path",
    "lease_id",
    "worktree_identity",
    "artifact_dir",
    "started_at",
    "terminal_at",
    "migration_hold",
    "allocator_pid",
    "allocator_token",
];

fn table_columns(conn: &Connection, table: &str) -> Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt.query_map([], |row| Ok((row.get(1)?, row.get(5)?)))?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

fn create_legacy_runtime_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS legacy_worker_runtimes (
            session_id TEXT PRIMARY KEY,
            incarnation_id TEXT NOT NULL UNIQUE,
            physical_tmux_name TEXT NOT NULL UNIQUE,
            pane_id TEXT NOT NULL,
            pane_pid INTEGER NOT NULL
        );
        ",
    )?;
    Ok(())
}

fn migrate_legacy_worker_incarnations(conn: &mut Connection) -> Result<()> {
    let columns = table_columns(conn, "worker_incarnations")?;
    let names = columns.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>();
    if names == CURRENT_WORKER_INCARNATION_COLUMNS
        && columns.first().is_some_and(|(_, pk)| *pk == 1)
        && columns.get(1).is_some_and(|(_, pk)| *pk == 0)
    {
        create_legacy_runtime_table(conn)?;
        return Ok(());
    }

    let legacy_columns_with_additions = LEGACY_WORKER_INCARNATION_COLUMNS
        .iter()
        .copied()
        .chain([
            "orchestrator_id",
            "source_workspace",
            "checkout_backed",
            "state",
        ])
        .collect::<Vec<_>>();
    let legacy_shape =
        names == LEGACY_WORKER_INCARNATION_COLUMNS || names == legacy_columns_with_additions;
    anyhow::ensure!(
        legacy_shape
            && columns.first().is_some_and(|(_, pk)| *pk == 1)
            && columns.get(1).is_some_and(|(_, pk)| *pk == 2),
        "unsupported worker_incarnations schema; refusing ambiguous migration"
    );
    for column in ["current_incarnation_id", "incarnation"] {
        anyhow::ensure!(
            Store::column_exists(conn, "sessions", column)?,
            "legacy worker migration requires sessions.{column}"
        );
    }
    anyhow::ensure!(
        Store::column_exists(conn, "pooled_checkouts", "owner_incarnation_id")?,
        "legacy worker migration requires pooled checkout ownership capabilities"
    );

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "
        CREATE TEMP TABLE worker_migration_candidates (
            session_id TEXT NOT NULL,
            incarnation_id TEXT NOT NULL,
            priority INTEGER NOT NULL,
            PRIMARY KEY(session_id, incarnation_id)
        );

        INSERT INTO worker_migration_candidates(session_id,incarnation_id,priority)
        SELECT
            w.session_id,
            w.incarnation_id,
            CASE
                WHEN EXISTS (
                    SELECT 1 FROM pooled_checkouts p
                    WHERE p.session_id=w.session_id
                      AND p.owner_incarnation_id=w.incarnation_id
                      AND p.lease_id IS w.lease_id
                      AND p.state IN ('provisioning','leased')
                ) THEN 10
                WHEN EXISTS (
                    SELECT 1 FROM sessions s
                    WHERE s.id=w.session_id
                      AND COALESCE(
                          NULLIF(s.current_incarnation_id,''),
                          NULLIF(s.incarnation,'')
                      )=w.incarnation_id
                      AND s.status NOT IN ('done','terminated','interrupted')
                ) THEN 20
                WHEN EXISTS (
                    SELECT 1 FROM worker_retention r
                    WHERE r.session_id=w.session_id
                      AND r.incarnation=w.incarnation_id
                      AND r.finalized_at IS NULL
                ) THEN 30
                WHEN EXISTS (
                    SELECT 1 FROM worker_retention r
                    WHERE r.session_id=w.session_id
                      AND r.incarnation=w.incarnation_id
                ) THEN 31
                ELSE 40
            END
        FROM worker_incarnations w
        WHERE EXISTS (
                SELECT 1 FROM pooled_checkouts p
                WHERE p.session_id=w.session_id
                  AND p.owner_incarnation_id=w.incarnation_id
                  AND p.lease_id IS w.lease_id
                  AND p.state IN ('provisioning','leased')
            )
            OR EXISTS (
                SELECT 1 FROM sessions s
                WHERE s.id=w.session_id
                  AND COALESCE(
                      NULLIF(s.current_incarnation_id,''),
                      NULLIF(s.incarnation,'')
                  )=w.incarnation_id
                  AND s.status NOT IN ('done','terminated','interrupted')
            )
            OR EXISTS (
                SELECT 1 FROM worker_retention r
                WHERE r.session_id=w.session_id
                  AND r.incarnation=w.incarnation_id
            )
            OR (
                SELECT COUNT(*) FROM worker_incarnations siblings
                WHERE siblings.session_id=w.session_id
            )=1;
        ",
    )?;

    let unresolved: i64 = tx.query_row(
        "SELECT
            (SELECT COUNT(DISTINCT session_id) FROM worker_incarnations)
            - (SELECT COUNT(DISTINCT session_id) FROM worker_migration_candidates)",
        [],
        |row| row.get(0),
    )?;
    let tied: i64 = tx.query_row(
        "SELECT COUNT(*) FROM (
            SELECT c.session_id
            FROM worker_migration_candidates c
            JOIN (
                SELECT session_id,MIN(priority) priority
                FROM worker_migration_candidates GROUP BY session_id
            ) strongest
              ON strongest.session_id=c.session_id
             AND strongest.priority=c.priority
            GROUP BY c.session_id
            HAVING COUNT(*)<>1
        )",
        [],
        |row| row.get(0),
    )?;
    anyhow::ensure!(
        unresolved == 0 && tied == 0,
        "ambiguous legacy worker incarnation authority"
    );

    tx.execute_batch(
        "
        CREATE TEMP TABLE worker_migration_survivors AS
        SELECT
            w.session_id,
            w.incarnation_id,
            COALESCE(
                s.orchestrator_id,
                (
                    SELECT r.orchestrator_id FROM worker_retention r
                    WHERE r.session_id=w.session_id
                      AND r.incarnation=w.incarnation_id
                    LIMIT 1
                )
            ) AS orchestrator_id,
            w.started_at,
            COALESCE(
                (
                    SELECT p.source_repo FROM pooled_checkouts p
                    WHERE p.path=COALESCE(w.pool_path,w.workspace_path)
                    LIMIT 1
                ),
                w.workspace_path
            ) AS source_workspace,
            COALESCE(
                (
                    SELECT p.path FROM pooled_checkouts p
                    WHERE p.session_id=w.session_id
                      AND p.owner_incarnation_id=w.incarnation_id
                      AND p.lease_id IS w.lease_id
                      AND p.state IN ('provisioning','leased')
                    LIMIT 1
                ),
                w.workspace_path,
                w.pool_path
            ) AS workspace_path,
            w.lease_id,
            w.allocator_pid,
            w.allocator_token,
            CASE WHEN EXISTS (
                SELECT 1 FROM pooled_checkouts p
                WHERE p.path=COALESCE(w.pool_path,w.workspace_path)
            ) THEN 1 ELSE 0 END AS checkout_backed,
            CASE
                WHEN EXISTS (
                    SELECT 1 FROM pooled_checkouts p
                    WHERE p.session_id=w.session_id
                      AND p.owner_incarnation_id=w.incarnation_id
                      AND p.lease_id IS w.lease_id
                      AND p.state IN ('provisioning','leased')
                ) THEN CASE
                    WHEN w.phase IN ('preparing','starting') THEN 'allocating'
                    ELSE 'active'
                END
                WHEN EXISTS (
                    SELECT 1 FROM sessions active
                    WHERE active.id=w.session_id
                      AND COALESCE(
                          NULLIF(active.current_incarnation_id,''),
                          NULLIF(active.incarnation,'')
                      )=w.incarnation_id
                      AND active.status NOT IN ('done','terminated','interrupted')
                ) THEN CASE w.phase
                    WHEN 'preparing' THEN 'allocating'
                    WHEN 'starting' THEN 'allocating'
                    WHEN 'retained' THEN 'retained'
                    WHEN 'cleanup_claimed' THEN 'cleanup_claimed'
                    ELSE 'active'
                END
                WHEN EXISTS (
                    SELECT 1 FROM worker_retention r
                    WHERE r.session_id=w.session_id
                      AND r.incarnation=w.incarnation_id
                      AND r.finalized_at IS NULL
                ) THEN 'retained'
                WHEN EXISTS (
                    SELECT 1 FROM worker_retention r
                    WHERE r.session_id=w.session_id
                      AND r.incarnation=w.incarnation_id
                ) THEN 'released'
                WHEN w.phase IN ('preparing','starting') THEN 'allocating'
                WHEN w.phase='running' THEN 'active'
                WHEN w.phase='retained' THEN 'retained'
                WHEN w.phase='cleanup_claimed' THEN 'cleanup_claimed'
                ELSE 'released'
            END AS state,
            w.phase,
            w.physical_tmux_name,
            w.pane_id,
            w.pane_pid,
            s.status AS session_status,
            s.pid AS session_pid,
            COALESCE(
                NULLIF(s.current_incarnation_id,''),
                NULLIF(s.incarnation,'')
            ) AS session_incarnation
        FROM worker_migration_candidates c
        JOIN (
            SELECT session_id,MIN(priority) priority
            FROM worker_migration_candidates GROUP BY session_id
        ) strongest
          ON strongest.session_id=c.session_id
         AND strongest.priority=c.priority
        JOIN worker_incarnations w
          ON w.session_id=c.session_id
         AND w.incarnation_id=c.incarnation_id
        LEFT JOIN sessions s ON s.id=w.session_id;

        ALTER TABLE worker_incarnations RENAME TO worker_incarnations_legacy;

        CREATE TABLE worker_incarnations (
            session_id TEXT PRIMARY KEY,
            incarnation_id TEXT NOT NULL UNIQUE,
            orchestrator_id TEXT,
            started_at INTEGER NOT NULL,
            source_workspace TEXT NOT NULL,
            workspace_path TEXT NOT NULL,
            lease_id TEXT,
            allocator_pid INTEGER,
            allocator_token TEXT,
            checkout_backed INTEGER NOT NULL CHECK(checkout_backed IN (0,1)),
            state TEXT NOT NULL CHECK(state IN (
                'allocating','active','retained','cleanup_claimed',
                'release_claimed','released'
            ))
        );

        INSERT INTO worker_incarnations(
            session_id,incarnation_id,orchestrator_id,started_at,
            source_workspace,workspace_path,lease_id,allocator_pid,
            allocator_token,checkout_backed,state
        )
        SELECT
            session_id,incarnation_id,orchestrator_id,started_at,
            source_workspace,workspace_path,lease_id,allocator_pid,
            allocator_token,checkout_backed,state
        FROM worker_migration_survivors;

        CREATE TABLE legacy_worker_runtimes (
            session_id TEXT PRIMARY KEY,
            incarnation_id TEXT NOT NULL UNIQUE,
            physical_tmux_name TEXT NOT NULL UNIQUE,
            pane_id TEXT NOT NULL,
            pane_pid INTEGER NOT NULL
        );

        INSERT INTO legacy_worker_runtimes(
            session_id,incarnation_id,physical_tmux_name,pane_id,pane_pid
        )
        SELECT
            session_id,incarnation_id,physical_tmux_name,pane_id,pane_pid
        FROM worker_migration_survivors
        WHERE state='active'
          AND phase='running'
          AND session_status='working'
          AND session_incarnation=incarnation_id
          AND session_pid=pane_pid
          AND pane_id IS NOT NULL
          AND pane_id LIKE '!%%' ESCAPE '!'
          AND pane_pid IS NOT NULL
          AND physical_tmux_name<>session_id;

        DROP TABLE worker_incarnations_legacy;
        ",
    )?;

    let migrated: i64 =
        tx.query_row("SELECT COUNT(*) FROM worker_incarnations", [], |row| row.get(0))?;
    let expected: i64 = tx.query_row(
        "SELECT COUNT(*) FROM worker_migration_survivors",
        [],
        |row| row.get(0),
    )?;
    anyhow::ensure!(
        migrated == expected,
        "legacy worker migration lost authoritative rows"
    );
    tx.commit()?;
    Ok(())
}

pub struct Store {
    conn: Mutex<Connection>,
    allocator_lock_dir: PathBuf,
    allocator_locks: Mutex<HashMap<String, File>>,
    runtime_claim_locks: Mutex<HashMap<String, File>>,
}


impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let database_path = path.as_ref().to_path_buf();
        let allocator_lock_dir = allocation_lock_dir(&database_path)?;
        std::fs::create_dir_all(&allocator_lock_dir)?;
        let mut conn = Connection::open(&database_path)?;
        conn.execute_batch(
            "
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
                path_kind TEXT NOT NULL DEFAULT 'sibling'
                    CHECK(path_kind IN ('sibling','managed','explicit','unsafe_legacy')),
                worktree_git_dir TEXT,
                worktree_identity TEXT,
                state TEXT NOT NULL CHECK(state IN ('provisioning','leased','free','quarantined')),
                session_id TEXT,
                owner_incarnation_id TEXT,
                lease_id TEXT,
                branch TEXT,
                quarantine_reason TEXT,
                UNIQUE(common_git_dir, slot),
                CHECK((session_id IS NULL) = (lease_id IS NULL)),
                CHECK(state NOT IN ('provisioning','leased') OR
                      (session_id IS NOT NULL AND owner_incarnation_id IS NOT NULL
                       AND lease_id IS NOT NULL AND branch IS NOT NULL))
            );
            CREATE UNIQUE INDEX IF NOT EXISTS pooled_checkouts_active_session
                ON pooled_checkouts(session_id)
                WHERE session_id IS NOT NULL AND state IN ('provisioning','leased');
            CREATE TABLE IF NOT EXISTS worker_incarnations (
                session_id TEXT PRIMARY KEY,
                incarnation_id TEXT NOT NULL UNIQUE,
                orchestrator_id TEXT,
                started_at INTEGER NOT NULL,
                source_workspace TEXT NOT NULL,
                workspace_path TEXT NOT NULL,
                lease_id TEXT,
                allocator_pid INTEGER,
                allocator_token TEXT,
                checkout_backed INTEGER NOT NULL CHECK(checkout_backed IN (0,1)),
                state TEXT NOT NULL CHECK(state IN (
                    'allocating','active','retained','cleanup_claimed',
                    'release_claimed','released'
                ))
            );
            CREATE TABLE IF NOT EXISTS worker_runtime_claims (
                session_id TEXT PRIMARY KEY,
                incarnation_id TEXT NOT NULL,
                claim_id TEXT NOT NULL UNIQUE,
                claim_token TEXT NOT NULL,
                prior_state TEXT NOT NULL CHECK(prior_state IN ('active','retained'))
            );
        ")?;
        migrate_legacy_worker_incarnations(&mut conn)?;
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
        if !Self::column_exists(&conn, "pooled_checkouts", "owner_incarnation_id")? {
            conn.execute(
                "ALTER TABLE pooled_checkouts ADD COLUMN owner_incarnation_id TEXT",
                [],
            )?;
        }
        if !Self::column_exists(&conn, "pooled_checkouts", "path_kind")? {
            conn.execute("ALTER TABLE pooled_checkouts ADD COLUMN path_kind TEXT", [])?;
        }
        if !Self::column_exists(&conn, "worker_incarnations", "allocator_pid")? {
            conn.execute(
                "ALTER TABLE worker_incarnations ADD COLUMN allocator_pid INTEGER",
                [],
            )?;
        }
        if !Self::column_exists(&conn, "worker_incarnations", "allocator_token")? {
            conn.execute(
                "ALTER TABLE worker_incarnations ADD COLUMN allocator_token TEXT",
                [],
            )?;
        }
        conn.execute(
            "UPDATE pooled_checkouts
             SET owner_incarnation_id=session_id
             WHERE owner_incarnation_id IS NULL
               AND session_id IS NOT NULL
               AND state IN ('provisioning','leased')",
            [],
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO worker_incarnations(
                session_id,incarnation_id,orchestrator_id,started_at,
                source_workspace,workspace_path,lease_id,checkout_backed,state
             )
             SELECT s.id,p.owner_incarnation_id,s.orchestrator_id,s.started_at,
                    p.source_repo,p.path,p.lease_id,1,
                    CASE WHEN s.status='done' THEN 'retained' ELSE 'active' END
             FROM pooled_checkouts p
             JOIN sessions s ON s.id=p.session_id
             WHERE p.state IN ('provisioning','leased')
               AND p.owner_incarnation_id IS NOT NULL
               AND p.lease_id IS NOT NULL",
            [],
        )?;
        Self::reconcile_pooled_checkout_paths(&mut conn)?;
        Self::reclaim_dead_allocations(&mut conn, &allocator_lock_dir)?;
        Self::reclaim_dead_runtime_claims(&mut conn, &allocator_lock_dir)?;
        Self::backfill_legacy_managed_workers(&conn)?;
        Self::remove_orphan_spawning_sessions(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            allocator_lock_dir,
            allocator_locks: Mutex::new(HashMap::new()),
            runtime_claim_locks: Mutex::new(HashMap::new()),
        })
    }

    fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let exists = stmt
            .query_map([], |r| r.get::<_, String>(1))?
            .filter_map(|r| r.ok())
            .any(|c| c == column);
        Ok(exists)
    }

    fn reconcile_pooled_checkout_paths(conn: &mut Connection) -> Result<()> {
        let records = {
            let mut stmt = conn.prepare(
                "SELECT path,source_repo,slot,path_kind,session_id,
                        owner_incarnation_id,lease_id
                 FROM pooled_checkouts",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, u32>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for (path, source_repo, slot, kind, session_id, incarnation_id, lease_id) in records {
            if kind.as_deref().is_some_and(|kind| kind != "sibling") {
                continue;
            }
            let source = Path::new(&source_repo);
            let expected = source.parent().and_then(|parent| {
                source
                    .file_name()
                    .map(|name| parent.join(format!("{}-w{}", name.to_string_lossy(), slot + 1)))
            });
            if expected
                .as_ref()
                .is_some_and(|expected| expected == Path::new(&path))
            {
                tx.execute(
                    "UPDATE pooled_checkouts SET path_kind='sibling' WHERE path=?1",
                    [&path],
                )?;
                continue;
            }
            if let (Some(session_id), Some(incarnation_id), Some(lease_id)) =
                (&session_id, &incarnation_id, &lease_id)
            {
                tx.execute(
                    "UPDATE worker_incarnations
                     SET state=CASE
                           WHEN state='allocating' THEN state
                           ELSE 'retained'
                         END,
                         lease_id=NULL
                     WHERE session_id=?1 AND incarnation_id=?2 AND lease_id=?3",
                    params![session_id, incarnation_id, lease_id],
                )?;
            }
            tx.execute(
                "UPDATE pooled_checkouts
                 SET path_kind='unsafe_legacy',state='quarantined',
                     session_id=NULL,owner_incarnation_id=NULL,lease_id=NULL,
                     quarantine_reason='legacy checkout path is not a valid source sibling'
                 WHERE path=?1",
                [&path],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn reclaim_dead_allocations(conn: &mut Connection, lock_dir: &Path) -> Result<()> {
        let stale = {
            let mut stmt = conn.prepare(
                "SELECT session_id,incarnation_id,allocator_token
                 FROM worker_incarnations WHERE state='allocating'",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
                .into_iter()
                .filter(|(_, incarnation_id, token)| {
                    allocation_lock_is_reclaimable(lock_dir, incarnation_id, token.as_deref())
                        .unwrap_or(false)
                })
                .collect::<Vec<_>>()
        };
        if stale.is_empty() {
            return Ok(());
        }
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for (session_id, incarnation_id, _) in stale {
            tx.execute(
                "UPDATE pooled_checkouts
                 SET state='quarantined',session_id=NULL,owner_incarnation_id=NULL,
                     lease_id=NULL,quarantine_reason='allocator process exited before bind'
                 WHERE session_id=?1 AND owner_incarnation_id=?2
                   AND state IN ('provisioning','leased')",
                params![session_id, incarnation_id],
            )?;
            tx.execute(
                "UPDATE worker_incarnations
                 SET state='released',allocator_pid=NULL,allocator_token=NULL
                 WHERE session_id=?1 AND incarnation_id=?2 AND state='allocating'",
                params![session_id, incarnation_id],
            )?;
            tx.execute(
                "UPDATE sessions SET status='terminated'
                 WHERE id=?1 AND started_at=(
                    SELECT started_at FROM worker_incarnations
                    WHERE session_id=?1 AND incarnation_id=?2
                 ) AND status='spawning'",
                params![session_id, incarnation_id],
            )?;
            let _ = std::fs::remove_file(allocation_lock_path(lock_dir, &incarnation_id));
        }
        tx.commit()?;
        Ok(())
    }

    fn reclaim_dead_runtime_claims(conn: &mut Connection, lock_dir: &Path) -> Result<()> {
        let stale = {
            let mut stmt = conn.prepare(
                "SELECT session_id,incarnation_id,claim_id,claim_token
                 FROM worker_runtime_claims",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
                .into_iter()
                .filter(|(_, _, claim_id, token)| {
                    allocation_lock_is_reclaimable(
                        lock_dir,
                        &format!("runtime-{claim_id}"),
                        Some(token),
                    )
                    .unwrap_or(false)
                })
                .collect::<Vec<_>>()
        };
        for (session_id, incarnation_id, claim_id, claim_token) in stale {
            conn.execute(
                "DELETE FROM worker_runtime_claims
                 WHERE session_id=?1 AND incarnation_id=?2
                   AND claim_id=?3 AND claim_token=?4",
                params![session_id, incarnation_id, claim_id, claim_token],
            )?;
            let _ = std::fs::remove_file(allocation_lock_path(
                lock_dir,
                &format!("runtime-{claim_id}"),
            ));
        }
        Ok(())
    }

    fn backfill_legacy_managed_workers(conn: &Connection) -> Result<()> {
        let candidates = {
            let mut stmt = conn.prepare(
                "SELECT s.id,s.orchestrator_id,s.started_at,s.status,s.workspace_path
                 FROM sessions s
                 LEFT JOIN worker_incarnations w ON w.session_id=s.id
                 WHERE w.session_id IS NULL AND s.workspace_path IS NOT NULL",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        for (session_id, orchestrator_id, started_at, status, workspace) in candidates {
            let workspace_path = Path::new(&workspace);
            let metadata =
                match crate::worktree::ManagedWorktree::load_for_workspace(
                    workspace_path,
                    &session_id,
                ) {
                    Ok(Some(metadata)) => metadata,
                    Ok(None) => continue,
                    Err(error) => {
                        tracing::warn!(
                            "ignore invalid legacy managed-worktree metadata for {session_id}: {error}"
                        );
                        continue;
                    }
                };
            let state = if status == "done" { "retained" } else { "active" };
            conn.execute(
                "INSERT OR IGNORE INTO worker_incarnations(
                    session_id,incarnation_id,orchestrator_id,started_at,
                    source_workspace,workspace_path,lease_id,allocator_pid,
                    checkout_backed,state
                 ) VALUES(?1,?2,?3,?4,?5,?6,NULL,NULL,1,?7)",
                params![
                    session_id,
                    uuid::Uuid::new_v4().to_string(),
                    orchestrator_id,
                    started_at,
                    metadata.source_repo.to_string_lossy(),
                    workspace,
                    state,
                ],
            )?;
        }
        Ok(())
    }

    fn remove_orphan_spawning_sessions(conn: &mut Connection) -> Result<()> {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE sessions SET status='terminated'
             WHERE status='working' AND pid IS NULL AND EXISTS(
                SELECT 1 FROM worker_incarnations w
                WHERE w.session_id=sessions.id
                  AND w.started_at=sessions.started_at
                  AND w.state='released'
             )",
            [],
        )?;
        tx.execute(
            "DELETE FROM sessions
             WHERE status='spawning' AND NOT EXISTS(
                SELECT 1 FROM worker_incarnations w
                WHERE w.session_id=sessions.id AND w.state<>'released'
             ) AND NOT EXISTS(
                SELECT 1 FROM orchestrators o WHERE o.id=sessions.id
             )",
            [],
        )?;
        tx.commit()?;
        Ok(())
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

    pub fn insert_spawning_session(&self, s: &Session) -> Result<bool> {
        anyhow::ensure!(
            matches!(s.status, SessionStatus::Spawning),
            "provisional session must be Spawning"
        );
        let status = serde_json::to_string(&s.status)?.replace('"', "");
        let gate_status = s
            .gate_status
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "INSERT INTO sessions (id,orchestrator_id,name,repo,status,agent_type,
             cost_usd,started_at,pr_number,pr_id,workspace_path,pid,model,context_tokens,
             catalogue_path,context_used_pct,context_total_tokens,context_window_size,
             claude_session_id,summary,terminal_at,gate_status)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22)
             ON CONFLICT(id) DO NOTHING",
            params![
                s.id,
                s.orchestrator_id,
                s.name,
                s.repo,
                status,
                s.agent_type,
                s.cost_usd,
                s.started_at,
                s.pr_number,
                s.pr_id,
                s.workspace_path,
                s.pid,
                s.model,
                s.context_tokens,
                s.catalogue_path,
                s.context_used_pct,
                s.context_total_tokens,
                s.context_window_size,
                s.claude_session_id,
                s.summary,
                s.terminal_at,
                gate_status
            ],
        )?;
        Ok(changed == 1)
    }

    pub fn terminalize_spawning_session_snapshot(
        &self,
        session_id: &str,
        started_at: i64,
        workspace_path: &str,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE sessions SET status='terminated',workspace_path=?3
             WHERE id=?1 AND started_at=?2 AND status='spawning'",
            params![session_id, started_at, workspace_path],
        )?;
        Ok(changed == 1)
    }

    pub fn update_spawning_session_workspace_snapshot(
        &self,
        session_id: &str,
        started_at: i64,
        workspace_path: &str,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE sessions SET workspace_path=?3
             WHERE id=?1 AND started_at=?2 AND status='spawning'",
            params![session_id, started_at, workspace_path],
        )?;
        Ok(changed == 1)
    }

    pub fn update_session_status_snapshot(
        &self,
        session_id: &str,
        started_at: i64,
        status: SessionStatus,
    ) -> Result<bool> {
        let status = serde_json::to_string(&status)?.replace('"', "");
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE sessions SET status=?3 WHERE id=?1 AND started_at=?2",
            params![session_id, started_at, status],
        )?;
        Ok(changed == 1)
    }

    pub fn delete_spawning_session_snapshot(
        &self,
        session_id: &str,
        started_at: i64,
        incarnation_id: Option<&str>,
    ) -> Result<bool> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(incarnation_id) = incarnation_id {
            let releasable: bool = tx.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM worker_incarnations
                    WHERE session_id=?1 AND incarnation_id=?2
                      AND started_at=?3 AND state='released'
                )",
                params![session_id, incarnation_id, started_at],
                |row| row.get(0),
            )?;
            if !releasable {
                tx.commit()?;
                return Ok(false);
            }
        } else {
            let active: bool = tx.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM worker_incarnations
                    WHERE session_id=?1 AND state<>'released'
                )",
                [session_id],
                |row| row.get(0),
            )?;
            if active {
                tx.commit()?;
                return Ok(false);
            }
        }
        let deleted = tx.execute(
            "DELETE FROM sessions
             WHERE id=?1 AND started_at=?2 AND status IN ('spawning','terminated')",
            params![session_id, started_at],
        )?;
        if deleted == 1 {
            if let Some(incarnation_id) = incarnation_id {
                tx.execute(
                    "DELETE FROM worker_incarnations
                     WHERE session_id=?1 AND incarnation_id=?2
                       AND started_at=?3 AND state='released'",
                    params![session_id, incarnation_id, started_at],
                )?;
            }
        }
        tx.commit()?;
        Ok(deleted == 1)
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
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM sessions WHERE id = ?1", [id])?;
        tx.execute(
            "DELETE FROM worker_incarnations
             WHERE session_id=?1 AND state IN ('cleanup_claimed','release_claimed','released')",
            [id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn delete_orchestrator(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
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

    pub fn prepare_worker_incarnation(
        &self,
        session_id: &str,
        orchestrator_id: Option<&str>,
        started_at: i64,
        source_workspace: &str,
        checkout_backed: bool,
        checkout_cap: usize,
    ) -> Result<WorkerIncarnation> {
        anyhow::ensure!(!session_id.is_empty(), "session id cannot be empty");
        anyhow::ensure!(
            (1..=3).contains(&checkout_cap),
            "worker checkout cap must be between 1 and 3"
        );
        let incarnation_id = uuid::Uuid::new_v4().to_string();
        let allocator_token = uuid::Uuid::new_v4().to_string();
        let allocator_path = allocation_lock_path(&self.allocator_lock_dir, &incarnation_id);
        let mut allocator_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&allocator_path)?;
        allocator_file.write_all(allocator_token.as_bytes())?;
        allocator_file.sync_all()?;
        allocator_file.lock()?;
        let allocator_lock = PendingAllocationLock {
            path: allocator_path,
            file: Some(allocator_file),
        };
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let runtime_claimed: bool = tx.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM worker_runtime_claims WHERE session_id=?1
            )",
            [session_id],
            |row| row.get(0),
        )?;
        anyhow::ensure!(
            !runtime_claimed,
            "worker {session_id} runtime start is already in progress"
        );
        let previous = tx
            .query_row(
                &format!("{WORKER_INCARNATION_COLUMNS} WHERE session_id=?1"),
                [session_id],
                worker_incarnation_row,
            )
            .optional()?
            .map(raw_worker_incarnation)
            .transpose()?;
        if let Some(previous) = &previous {
            anyhow::ensure!(
                !matches!(
                    previous.state,
                    WorkerIncarnationState::Allocating
                        | WorkerIncarnationState::CleanupClaimed
                        | WorkerIncarnationState::ReleaseClaimed
                ),
                "worker {session_id} has an in-progress resource claim"
            );
        }
        if checkout_backed {
            let used: usize = tx.query_row(
                "SELECT COUNT(*) FROM worker_incarnations
                 WHERE session_id<>?1 AND checkout_backed=1 AND state<>'released'",
                [session_id],
                |row| row.get(0),
            )?;
            anyhow::ensure!(
                used < checkout_cap,
                "checkout-backed worker cap reached ({used}/{checkout_cap})"
            );
        }
        let workspace_path = previous
            .as_ref()
            .map_or(source_workspace, |worker| worker.workspace_path.as_str());
        let pooled_lease_id = if let Some(previous) = &previous {
            tx.query_row(
                "SELECT lease_id FROM pooled_checkouts
                 WHERE session_id=?1 AND owner_incarnation_id=?2
                   AND state IN ('provisioning','leased')",
                params![session_id, previous.incarnation_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        } else {
            None
        };
        let lease_id = previous
            .as_ref()
            .and_then(|worker| worker.lease_id.as_deref())
            .or(pooled_lease_id.as_deref());
        if let Some(previous) = &previous {
            if let Some(lease_id) = lease_id {
                let changed = tx.execute(
                    "UPDATE pooled_checkouts SET owner_incarnation_id=?4
                     WHERE session_id=?1 AND owner_incarnation_id=?2
                       AND lease_id=?3 AND state IN ('provisioning','leased')",
                    params![
                        session_id,
                        previous.incarnation_id,
                        lease_id,
                        incarnation_id
                    ],
                )?;
                anyhow::ensure!(
                    changed == 1,
                    "worker {session_id} pooled capability changed before Re-file"
                );
            }
        }
        tx.execute(
            "INSERT INTO worker_incarnations(
                session_id,incarnation_id,orchestrator_id,started_at,
                source_workspace,workspace_path,lease_id,allocator_pid,allocator_token,
                checkout_backed,state
             ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,'allocating')
             ON CONFLICT(session_id) DO UPDATE SET
                incarnation_id=excluded.incarnation_id,
                orchestrator_id=excluded.orchestrator_id,
                started_at=excluded.started_at,
                source_workspace=excluded.source_workspace,
                workspace_path=excluded.workspace_path,
                lease_id=excluded.lease_id,
                allocator_pid=excluded.allocator_pid,
                allocator_token=excluded.allocator_token,
                checkout_backed=excluded.checkout_backed,
                state='allocating'",
            params![
                session_id,
                incarnation_id,
                orchestrator_id,
                started_at,
                source_workspace,
                workspace_path,
                lease_id,
                std::process::id(),
                allocator_token,
                checkout_backed as i64,
            ],
        )?;
        tx.execute(
            "UPDATE sessions
             SET started_at=?2,pr_number=NULL,pr_id=NULL,gate_status=NULL
             WHERE id=?1",
            params![session_id, started_at],
        )?;
        tx.commit()?;
        let allocator_lock = allocator_lock
            .persist()
            .context("pending allocation lock disappeared")?;
        self.allocator_locks
            .lock()
            .unwrap()
            .insert(incarnation_id.clone(), allocator_lock);
        Ok(WorkerIncarnation {
            session_id: session_id.to_string(),
            incarnation_id,
            orchestrator_id: orchestrator_id.map(str::to_string),
            started_at,
            source_workspace: source_workspace.to_string(),
            workspace_path: workspace_path.to_string(),
            lease_id: lease_id.map(str::to_string),
            checkout_backed,
            state: WorkerIncarnationState::Allocating,
        })
    }

    pub fn bind_worker_incarnation(
        &self,
        session_id: &str,
        incarnation_id: &str,
        source_workspace: &str,
        workspace_path: &str,
        lease_id: Option<&str>,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE worker_incarnations
             SET source_workspace=?3,workspace_path=?4,lease_id=?5,
                 allocator_pid=NULL,allocator_token=NULL,state='active'
             WHERE session_id=?1 AND incarnation_id=?2 AND state='allocating'",
            params![session_id, incarnation_id, source_workspace, workspace_path, lease_id],
        )?;
        if changed == 1 {
            self.release_allocator_lock(incarnation_id);
        }
        Ok(changed == 1)
    }

    fn release_allocator_lock(&self, incarnation_id: &str) {
        if let Some(file) = self.allocator_locks.lock().unwrap().remove(incarnation_id) {
            let _ = file.unlock();
        }
        let _ = std::fs::remove_file(allocation_lock_path(
            &self.allocator_lock_dir,
            incarnation_id,
        ));
    }

    pub fn worker_incarnation_for_snapshot(
        &self,
        session_id: &str,
        started_at: i64,
    ) -> Result<Option<WorkerIncarnation>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            &format!(
                "{WORKER_INCARNATION_COLUMNS} WHERE session_id=?1 AND started_at=?2"
            ),
            params![session_id, started_at],
            worker_incarnation_row,
        )
        .optional()?
        .map(raw_worker_incarnation)
        .transpose()
    }

    pub fn current_worker_incarnation(
        &self,
        session_id: &str,
    ) -> Result<Option<WorkerIncarnation>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            &format!("{WORKER_INCARNATION_COLUMNS} WHERE session_id=?1"),
            [session_id],
            worker_incarnation_row,
        )
        .optional()?
        .map(raw_worker_incarnation)
        .transpose()
    }

    pub fn legacy_worker_runtime(
        &self,
        session_id: &str,
    ) -> Result<Option<LegacyWorkerRuntimeCapability>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT r.session_id,r.incarnation_id,r.physical_tmux_name,
                    r.pane_id,r.pane_pid
             FROM legacy_worker_runtimes r
             JOIN worker_incarnations w
               ON w.session_id=r.session_id
              AND w.incarnation_id=r.incarnation_id
              AND w.state='active'
             WHERE r.session_id=?1",
            [session_id],
            |row| {
                let pane_pid = row.get::<_, i64>(4)?;
                let pane_pid = u32::try_from(pane_pid).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        4,
                        rusqlite::types::Type::Integer,
                        Box::new(error),
                    )
                })?;
                Ok(LegacyWorkerRuntimeCapability {
                    session_id: row.get(0)?,
                    incarnation_id: row.get(1)?,
                    physical_tmux_name: row.get(2)?,
                    pane_id: row.get(3)?,
                    pane_pid,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn worker_runtime_claimed(&self, session_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM worker_runtime_claims WHERE session_id=?1
            )",
            [session_id],
            |row| row.get(0),
        )
        .map_err(Into::into)
    }

    pub fn claim_worker_runtime_start(
        &self,
        session_id: &str,
        incarnation_id: &str,
    ) -> Result<Option<WorkerRuntimeClaim>> {
        let claim_id = uuid::Uuid::new_v4().to_string();
        let claim_token = uuid::Uuid::new_v4().to_string();
        let lock_key = format!("runtime-{claim_id}");
        let lock_path = allocation_lock_path(&self.allocator_lock_dir, &lock_key);
        let mut lock_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&lock_path)?;
        lock_file.write_all(claim_token.as_bytes())?;
        lock_file.sync_all()?;
        lock_file.lock()?;
        let pending_lock = PendingAllocationLock {
            path: lock_path,
            file: Some(lock_file),
        };

        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(raw) = tx
            .query_row(
                &format!(
                    "{WORKER_INCARNATION_COLUMNS}
                     WHERE session_id=?1 AND incarnation_id=?2"
                ),
                params![session_id, incarnation_id],
                worker_incarnation_row,
            )
            .optional()?
        else {
            tx.commit()?;
            return Ok(None);
        };
        let worker = raw_worker_incarnation(raw)?;
        if !matches!(
            worker.state,
            WorkerIncarnationState::Active | WorkerIncarnationState::Retained
        ) {
            tx.commit()?;
            return Ok(None);
        }
        if let Some(lease_id) = worker.lease_id.as_deref() {
            let exact: bool = tx.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM pooled_checkouts
                    WHERE session_id=?1 AND owner_incarnation_id=?2
                      AND lease_id=?3 AND state IN ('provisioning','leased')
                )",
                params![session_id, incarnation_id, lease_id],
                |row| row.get(0),
            )?;
            if !exact {
                tx.commit()?;
                return Ok(None);
            }
        }
        let inserted = tx.execute(
            "INSERT INTO worker_runtime_claims(
                session_id,incarnation_id,claim_id,claim_token,prior_state
             ) VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(session_id) DO NOTHING",
            params![
                session_id,
                incarnation_id,
                claim_id,
                claim_token,
                worker_state_name(worker.state),
            ],
        )?;
        if inserted != 1 {
            tx.commit()?;
            return Ok(None);
        }
        tx.commit()?;
        let lock_file = pending_lock
            .persist()
            .context("pending runtime claim lock disappeared")?;
        self.runtime_claim_locks
            .lock()
            .unwrap()
            .insert(claim_id.clone(), lock_file);
        Ok(Some(WorkerRuntimeClaim { worker, claim_id }))
    }

    pub fn complete_worker_runtime_start(
        &self,
        session_id: &str,
        incarnation_id: &str,
        claim_id: &str,
    ) -> Result<bool> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let prior_state = tx
            .query_row(
                "SELECT prior_state FROM worker_runtime_claims
                 WHERE session_id=?1 AND incarnation_id=?2 AND claim_id=?3",
                params![session_id, incarnation_id, claim_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(prior_state) = prior_state else {
            tx.commit()?;
            return Ok(false);
        };
        let changed = tx.execute(
            "UPDATE worker_incarnations SET state='active'
             WHERE session_id=?1 AND incarnation_id=?2 AND state=?3",
            params![session_id, incarnation_id, prior_state],
        )?;
        if changed != 1 {
            tx.commit()?;
            return Ok(false);
        }
        tx.execute(
            "DELETE FROM worker_runtime_claims
             WHERE session_id=?1 AND incarnation_id=?2 AND claim_id=?3",
            params![session_id, incarnation_id, claim_id],
        )?;
        tx.commit()?;
        self.release_runtime_claim_lock(claim_id);
        Ok(true)
    }

    pub fn abort_worker_runtime_start(
        &self,
        session_id: &str,
        incarnation_id: &str,
        claim_id: &str,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "DELETE FROM worker_runtime_claims
             WHERE session_id=?1 AND incarnation_id=?2 AND claim_id=?3",
            params![session_id, incarnation_id, claim_id],
        )?;
        drop(conn);
        if changed == 1 {
            self.release_runtime_claim_lock(claim_id);
        }
        Ok(changed == 1)
    }

    fn release_runtime_claim_lock(&self, claim_id: &str) {
        if let Some(file) = self.runtime_claim_locks.lock().unwrap().remove(claim_id) {
            let _ = file.unlock();
        }
        let _ = std::fs::remove_file(allocation_lock_path(
            &self.allocator_lock_dir,
            &format!("runtime-{claim_id}"),
        ));
    }

    pub fn claim_worker_cleanup(
        &self,
        session_id: &str,
        incarnation_id: &str,
    ) -> Result<Option<WorkerIncarnation>> {
        self.claim_worker_state(
            session_id,
            incarnation_id,
            &["allocating", "active", "retained"],
            "cleanup_claimed",
        )
    }

    pub fn claim_worker_cleanup_with_lease(
        &self,
        session_id: &str,
        incarnation_id: &str,
        lease_id: &str,
    ) -> Result<Option<WorkerIncarnation>> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let exact: bool = tx.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM pooled_checkouts
                WHERE session_id=?1 AND owner_incarnation_id=?2 AND lease_id=?3
                  AND state IN ('provisioning','leased')
            )",
            params![session_id, incarnation_id, lease_id],
            |row| row.get(0),
        )?;
        if !exact {
            tx.commit()?;
            return Ok(None);
        }
        let changed = tx.execute(
            "UPDATE worker_incarnations
             SET lease_id=?3,state='cleanup_claimed'
             WHERE session_id=?1 AND incarnation_id=?2
               AND state IN ('allocating','active','retained')
               AND (lease_id IS NULL OR lease_id=?3)
               AND NOT EXISTS(
                   SELECT 1 FROM worker_runtime_claims r
                   WHERE r.session_id=worker_incarnations.session_id
                     AND r.incarnation_id=worker_incarnations.incarnation_id
               )",
            params![session_id, incarnation_id, lease_id],
        )?;
        if changed != 1 {
            tx.commit()?;
            return Ok(None);
        }
        let raw = tx.query_row(
            &format!(
                "{WORKER_INCARNATION_COLUMNS}
                 WHERE session_id=?1 AND incarnation_id=?2"
            ),
            params![session_id, incarnation_id],
            worker_incarnation_row,
        )?;
        tx.commit()?;
        Ok(Some(raw_worker_incarnation(raw)?))
    }

    pub fn claim_worker_cleanup_snapshot(
        &self,
        session_id: &str,
        started_at: i64,
    ) -> Result<Option<WorkerIncarnation>> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let runtime_claimed: bool = tx.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM worker_runtime_claims WHERE session_id=?1
            )",
            [session_id],
            |row| row.get(0),
        )?;
        if runtime_claimed {
            tx.commit()?;
            return Ok(None);
        }
        if let Some(raw) = tx
            .query_row(
                &format!(
                    "{WORKER_INCARNATION_COLUMNS}
                     WHERE session_id=?1 AND started_at=?2"
                ),
                params![session_id, started_at],
                worker_incarnation_row,
            )
            .optional()?
        {
            let mut worker = raw_worker_incarnation(raw)?;
            if !matches!(
                worker.state,
                WorkerIncarnationState::Allocating
                    | WorkerIncarnationState::Active
                    | WorkerIncarnationState::Retained
            ) {
                tx.commit()?;
                return Ok(None);
            }
            if let Some(lease_id) = worker.lease_id.as_deref() {
                let exact: bool = tx.query_row(
                    "SELECT EXISTS(
                        SELECT 1 FROM pooled_checkouts
                        WHERE session_id=?1 AND owner_incarnation_id=?2
                          AND lease_id=?3 AND state IN ('provisioning','leased')
                    )",
                    params![session_id, worker.incarnation_id, lease_id],
                    |row| row.get(0),
                )?;
                if !exact {
                    tx.commit()?;
                    return Ok(None);
                }
            }
            let changed = tx.execute(
                "UPDATE worker_incarnations SET state='cleanup_claimed'
                 WHERE session_id=?1 AND incarnation_id=?2 AND started_at=?3
                   AND state=?4",
                params![
                    session_id,
                    worker.incarnation_id,
                    started_at,
                    worker_state_name(worker.state),
                ],
            )?;
            if changed != 1 {
                tx.commit()?;
                return Ok(None);
            }
            tx.commit()?;
            worker.state = WorkerIncarnationState::CleanupClaimed;
            return Ok(Some(worker));
        }

        let legacy = tx
            .query_row(
                "SELECT orchestrator_id,workspace_path
                 FROM sessions WHERE id=?1 AND started_at=?2",
                params![session_id, started_at],
                |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()?;
        let Some((orchestrator_id, workspace_path)) = legacy else {
            tx.commit()?;
            return Ok(None);
        };
        let pool = tx
            .query_row(
                "SELECT owner_incarnation_id,lease_id,source_repo,path
                 FROM pooled_checkouts
                 WHERE session_id=?1 AND state IN ('provisioning','leased')",
                [session_id],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?;
        let incarnation_id = pool
            .as_ref()
            .and_then(|pool| pool.0.clone())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let lease_id = pool.as_ref().and_then(|pool| pool.1.clone());
        let source_workspace = pool
            .as_ref()
            .map_or_else(|| workspace_path.clone().unwrap_or_default(), |pool| pool.2.clone());
        let workspace_path = pool
            .as_ref()
            .map_or_else(|| workspace_path.unwrap_or_default(), |pool| pool.3.clone());
        tx.execute(
            "INSERT INTO worker_incarnations(
                session_id,incarnation_id,orchestrator_id,started_at,
                source_workspace,workspace_path,lease_id,checkout_backed,state
             ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'cleanup_claimed')",
            params![
                session_id,
                incarnation_id,
                orchestrator_id,
                started_at,
                source_workspace,
                workspace_path,
                lease_id,
                pool.is_some() as i64,
            ],
        )?;
        tx.commit()?;
        Ok(Some(WorkerIncarnation {
            session_id: session_id.to_string(),
            incarnation_id,
            orchestrator_id,
            started_at,
            source_workspace,
            workspace_path,
            lease_id,
            checkout_backed: pool.is_some(),
            state: WorkerIncarnationState::CleanupClaimed,
        }))
    }

    pub fn claim_worker_release(
        &self,
        session_id: &str,
        incarnation_id: &str,
    ) -> Result<Option<WorkerIncarnation>> {
        self.claim_worker_state(
            session_id,
            incarnation_id,
            &["retained"],
            "release_claimed",
        )
    }

    fn claim_worker_state(
        &self,
        session_id: &str,
        incarnation_id: &str,
        from_states: &[&str],
        target_state: &str,
    ) -> Result<Option<WorkerIncarnation>> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let runtime_claimed: bool = tx.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM worker_runtime_claims
                WHERE session_id=?1 AND incarnation_id=?2
            )",
            params![session_id, incarnation_id],
            |row| row.get(0),
        )?;
        if runtime_claimed {
            tx.commit()?;
            return Ok(None);
        }
        let Some(raw) = tx
            .query_row(
                &format!(
                    "{WORKER_INCARNATION_COLUMNS}
                     WHERE session_id=?1 AND incarnation_id=?2"
                ),
                params![session_id, incarnation_id],
                worker_incarnation_row,
            )
            .optional()?
        else {
            tx.commit()?;
            return Ok(None);
        };
        let mut worker = raw_worker_incarnation(raw)?;
        let current = worker_state_name(worker.state);
        if !from_states.contains(&current) {
            tx.commit()?;
            return Ok(None);
        }
        if let Some(lease_id) = worker.lease_id.as_deref() {
            let exact: bool = tx.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM pooled_checkouts
                    WHERE session_id=?1 AND owner_incarnation_id=?2
                      AND lease_id=?3 AND state IN ('provisioning','leased')
                )",
                params![session_id, incarnation_id, lease_id],
                |row| row.get(0),
            )?;
            if !exact {
                tx.commit()?;
                return Ok(None);
            }
        }
        let changed = tx.execute(
            "UPDATE worker_incarnations SET state=?3
             WHERE session_id=?1 AND incarnation_id=?2 AND state=?4",
            params![session_id, incarnation_id, target_state, current],
        )?;
        if changed != 1 {
            tx.commit()?;
            return Ok(None);
        }
        tx.commit()?;
        worker.state = parse_worker_state(target_state)?;
        Ok(Some(worker))
    }

    pub fn abort_worker_claim(
        &self,
        session_id: &str,
        incarnation_id: &str,
        claimed_state: WorkerIncarnationState,
        restored_state: WorkerIncarnationState,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE worker_incarnations SET state=?3
             WHERE session_id=?1 AND incarnation_id=?2 AND state=?4",
            params![
                session_id,
                incarnation_id,
                worker_state_name(restored_state),
                worker_state_name(claimed_state),
            ],
        )?;
        Ok(changed == 1)
    }

    pub fn complete_worker_claim(
        &self,
        session_id: &str,
        incarnation_id: &str,
        claimed_state: WorkerIncarnationState,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE worker_incarnations
             SET state='released',allocator_pid=NULL,allocator_token=NULL
             WHERE session_id=?1 AND incarnation_id=?2 AND state=?3",
            params![session_id, incarnation_id, worker_state_name(claimed_state)],
        )?;
        drop(conn);
        if changed == 1 {
            self.release_allocator_lock(incarnation_id);
        }
        Ok(changed == 1)
    }

    pub fn retain_worker_after_merge(
        &self,
        session_id: &str,
        incarnation_id: &str,
        started_at: i64,
        pr_number: u64,
        terminal_at: i64,
    ) -> Result<Option<Session>> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE worker_incarnations SET state='retained'
             WHERE session_id=?1 AND incarnation_id=?2 AND started_at=?3
               AND state='active'
               AND NOT EXISTS(
                   SELECT 1 FROM worker_runtime_claims r
                   WHERE r.session_id=worker_incarnations.session_id
                     AND r.incarnation_id=worker_incarnations.incarnation_id
               )",
            params![session_id, incarnation_id, started_at],
        )?;
        if changed != 1 {
            tx.commit()?;
            return Ok(None);
        }
        let status = "done";
        let changed = tx.execute(
            "UPDATE sessions SET status=?4,terminal_at=?5
             WHERE id=?1 AND started_at=?2 AND pr_number=?3",
            params![session_id, started_at, pr_number, status, terminal_at],
        )?;
        if changed != 1 {
            tx.execute(
                "UPDATE worker_incarnations SET state='active'
                 WHERE session_id=?1 AND incarnation_id=?2 AND state='retained'",
                params![session_id, incarnation_id],
            )?;
            tx.commit()?;
            return Ok(None);
        }
        tx.commit()?;
        drop(conn);
        self.get_session(session_id)
    }

    pub fn is_worker_retained(&self, session_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM worker_incarnations
                WHERE session_id=?1 AND state='retained'
            )",
            [session_id],
            |row| row.get(0),
        )
        .map_err(Into::into)
    }

    pub fn retained_worker_candidates(
        &self,
        orchestrator_id: Option<&str>,
    ) -> Result<Vec<WorkerIncarnation>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "{WORKER_INCARNATION_COLUMNS}
             WHERE orchestrator_id IS ?1 AND state='retained'
             ORDER BY started_at ASC"
        ))?;
        let rows = stmt.query_map([orchestrator_id], worker_incarnation_row)?;
        rows.map(|row| raw_worker_incarnation(row?)).collect()
    }

    pub fn checkout_worker_candidates(
        &self,
        _orchestrator_id: Option<&str>,
    ) -> Result<Vec<WorkerIncarnation>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "{WORKER_INCARNATION_COLUMNS}
             WHERE checkout_backed=1 AND state<>'released'
             ORDER BY CASE state WHEN 'retained' THEN 0 ELSE 1 END, started_at ASC"
        ))?;
        let rows = stmt.query_map([], worker_incarnation_row)?;
        rows.map(|row| raw_worker_incarnation(row?)).collect()
    }

    /// Atomically reserves the lowest-numbered free checkout for a session.
    ///
    /// The checkout remains `provisioning` until its Git branch is prepared
    /// and `finalize_pooled_checkout` records the verified worktree identity.
    pub fn claim_lowest_free_pooled_checkout_for_incarnation(
        &self,
        common_git_dir: &Path,
        session_id: &str,
        owner_incarnation_id: &str,
        branch: &str,
    ) -> Result<Option<PooledCheckoutLease>> {
        self.claim_lowest_free_pooled_checkout_of_kind_for_incarnation(
            common_git_dir,
            PooledCheckoutKind::Sibling,
            session_id,
            owner_incarnation_id,
            branch,
        )
    }

    pub fn claim_lowest_free_pooled_checkout_of_kind_for_incarnation(
        &self,
        common_git_dir: &Path,
        kind: PooledCheckoutKind,
        session_id: &str,
        owner_incarnation_id: &str,
        branch: &str,
    ) -> Result<Option<PooledCheckoutLease>> {
        anyhow::ensure!(
            !matches!(
                kind,
                PooledCheckoutKind::Explicit | PooledCheckoutKind::UnsafeLegacy
            ),
            "lowest-slot claim requires sibling or managed checkout kind"
        );
        validate_lease_inputs(session_id, branch)?;
        let common_git_dir = canonical_db_path(common_git_dir)?;
        let kind_name = pooled_checkout_kind_name(kind);
        let lease_id = uuid::Uuid::new_v4().to_string();
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let selected = tx
            .query_row(
                &format!(
                    "{POOLED_CHECKOUT_COLUMNS}
                     WHERE common_git_dir = ?1 AND path_kind=?2 AND state = 'free'
                     ORDER BY slot ASC LIMIT 1"
                ),
                params![common_git_dir, kind_name],
                pooled_checkout_row,
            )
            .optional()?;
        let Some(mut record) = selected.map(raw_pooled_checkout).transpose()? else {
            tx.commit()?;
            return Ok(None);
        };
        let changed = tx.execute(
            "UPDATE pooled_checkouts
             SET state='provisioning', session_id=?2, owner_incarnation_id=?3,
                 lease_id=?4, branch=?5,
                 quarantine_reason=NULL
             WHERE path=?1 AND path_kind=?6 AND state='free'",
            params![
                path_text(&record.path)?,
                session_id,
                owner_incarnation_id,
                lease_id,
                branch,
                kind_name
            ],
        )?;
        anyhow::ensure!(changed == 1, "free pooled checkout changed during claim");
        tx.commit()?;

        record.state = PooledCheckoutState::Provisioning;
        record.session_id = Some(session_id.to_string());
        record.owner_incarnation_id = Some(owner_incarnation_id.to_string());
        record.lease_id = Some(lease_id);
        record.branch = Some(branch.to_string());
        Ok(Some(record_into_lease(record)?))
    }

    pub fn claim_free_pooled_checkout_at_for_incarnation(
        &self,
        path: &Path,
        common_git_dir: &Path,
        session_id: &str,
        owner_incarnation_id: &str,
        branch: &str,
    ) -> Result<Option<PooledCheckoutLease>> {
        validate_lease_inputs(session_id, branch)?;
        let path = absolute_db_path(path)?;
        let common_git_dir = canonical_db_path(common_git_dir)?;
        let lease_id = uuid::Uuid::new_v4().to_string();
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let selected = tx
            .query_row(
                &format!(
                    "{POOLED_CHECKOUT_COLUMNS}
                     WHERE path=?1 AND common_git_dir=?2 AND state='free'
                       AND path_kind<>'unsafe_legacy'"
                ),
                params![path, common_git_dir],
                pooled_checkout_row,
            )
            .optional()?;
        let Some(mut record) = selected.map(raw_pooled_checkout).transpose()? else {
            tx.commit()?;
            return Ok(None);
        };
        let changed = tx.execute(
            "UPDATE pooled_checkouts
             SET state='provisioning',session_id=?2,owner_incarnation_id=?3,
                 lease_id=?4,branch=?5,quarantine_reason=NULL
             WHERE path=?1 AND common_git_dir=?6 AND state='free'
               AND path_kind<>'unsafe_legacy'",
            params![
                path,
                session_id,
                owner_incarnation_id,
                lease_id,
                branch,
                common_git_dir,
            ],
        )?;
        anyhow::ensure!(changed == 1, "requested pooled checkout changed during claim");
        tx.commit()?;
        record.state = PooledCheckoutState::Provisioning;
        record.session_id = Some(session_id.to_string());
        record.owner_incarnation_id = Some(owner_incarnation_id.to_string());
        record.lease_id = Some(lease_id);
        record.branch = Some(branch.to_string());
        Ok(Some(record_into_lease(record)?))
    }

    /// Atomically allocates the next slot and reserves its deterministic path.
    pub fn reserve_pooled_checkout_for_incarnation(
        &self,
        source_repo: &Path,
        common_git_dir: &Path,
        session_id: &str,
        owner_incarnation_id: &str,
        branch: &str,
    ) -> Result<PooledCheckoutLease> {
        validate_lease_inputs(session_id, branch)?;
        let source_repo = canonical_db_path(source_repo)?;
        let common_git_dir = canonical_db_path(common_git_dir)?;
        let repository_name = Path::new(&source_repo)
            .file_name()
            .and_then(|name| name.to_str())
            .context("source repository has no UTF-8 directory name")?;
        let lease_id = uuid::Uuid::new_v4().to_string();
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let slot: u32 = {
            let mut stmt = tx.prepare(
                "SELECT slot FROM pooled_checkouts
                 WHERE common_git_dir=?1 ORDER BY slot ASC",
            )?;
            let mut rows = stmt.query([&common_git_dir])?;
            let mut candidate = 0_u32;
            while let Some(row) = rows.next()? {
                let existing: u32 = row.get(0)?;
                if existing != candidate {
                    break;
                }
                candidate += 1;
            }
            candidate
        };
        let source_parent = Path::new(&source_repo)
            .parent()
            .context("source repository has no parent directory")?;
        let path = source_parent
            .join(format!("{repository_name}-w{}", slot + 1));
        let path_str = path_text(&path)?;
        tx.execute(
            "INSERT INTO pooled_checkouts(
                path,source_repo,common_git_dir,slot,path_kind,state,session_id,
                owner_incarnation_id,lease_id,branch
             ) VALUES(?1,?2,?3,?4,'sibling','provisioning',?5,?6,?7,?8)",
            params![
                path_str,
                source_repo,
                common_git_dir,
                slot,
                session_id,
                owner_incarnation_id,
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
            kind: PooledCheckoutKind::Sibling,
            worktree_git_dir: None,
            worktree_identity: None,
            session_id: session_id.to_string(),
            owner_incarnation_id: owner_incarnation_id.to_string(),
            lease_id,
            branch: branch.to_string(),
        })
    }

    pub fn reserve_managed_pooled_checkout_for_incarnation(
        &self,
        source_repo: &Path,
        common_git_dir: &Path,
        managed_pool_root: &Path,
        session_id: &str,
        owner_incarnation_id: &str,
        branch: &str,
    ) -> Result<PooledCheckoutLease> {
        validate_lease_inputs(session_id, branch)?;
        let source_repo = canonical_db_path(source_repo)?;
        let common_git_dir = canonical_db_path(common_git_dir)?;
        let managed_pool_root = absolute_db_path(managed_pool_root)?;
        let lease_id = uuid::Uuid::new_v4().to_string();
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let slot: u32 = {
            let mut stmt = tx.prepare(
                "SELECT slot FROM pooled_checkouts
                 WHERE common_git_dir=?1 ORDER BY slot ASC",
            )?;
            let mut rows = stmt.query([&common_git_dir])?;
            let mut candidate = 0_u32;
            while let Some(row) = rows.next()? {
                let existing: u32 = row.get(0)?;
                if existing != candidate {
                    break;
                }
                candidate += 1;
            }
            candidate
        };
        let path = PathBuf::from(&managed_pool_root).join(format!("worker-w{}", slot + 1));
        tx.execute(
            "INSERT INTO pooled_checkouts(
                path,source_repo,common_git_dir,slot,path_kind,state,session_id,
                owner_incarnation_id,lease_id,branch
             ) VALUES(?1,?2,?3,?4,'managed','provisioning',?5,?6,?7,?8)",
            params![
                path_text(&path)?,
                source_repo,
                common_git_dir,
                slot,
                session_id,
                owner_incarnation_id,
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
            kind: PooledCheckoutKind::Managed,
            worktree_git_dir: None,
            worktree_identity: None,
            session_id: session_id.to_string(),
            owner_incarnation_id: owner_incarnation_id.to_string(),
            lease_id,
            branch: branch.to_string(),
        })
    }

    pub fn reserve_pooled_checkout_at_for_incarnation(
        &self,
        source_repo: &Path,
        common_git_dir: &Path,
        path: &Path,
        session_id: &str,
        owner_incarnation_id: &str,
        branch: &str,
    ) -> Result<PooledCheckoutLease> {
        validate_lease_inputs(session_id, branch)?;
        let source_repo = canonical_db_path(source_repo)?;
        let common_git_dir = canonical_db_path(common_git_dir)?;
        let path = absolute_db_path(path)?;
        let lease_id = uuid::Uuid::new_v4().to_string();
        let kind = PooledCheckoutKind::Explicit;
        let kind_name = pooled_checkout_kind_name(kind);
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let slot: u32 = {
            let mut stmt = tx.prepare(
                "SELECT slot FROM pooled_checkouts
                 WHERE common_git_dir=?1 ORDER BY slot ASC",
            )?;
            let mut rows = stmt.query([&common_git_dir])?;
            let mut candidate = 0_u32;
            while let Some(row) = rows.next()? {
                let existing: u32 = row.get(0)?;
                if existing != candidate {
                    break;
                }
                candidate += 1;
            }
            candidate
        };
        tx.execute(
            "INSERT INTO pooled_checkouts(
                path,source_repo,common_git_dir,slot,path_kind,state,session_id,
                owner_incarnation_id,lease_id,branch
             ) VALUES(?1,?2,?3,?4,?5,'provisioning',?6,?7,?8,?9)",
            params![
                path,
                source_repo,
                common_git_dir,
                slot,
                kind_name,
                session_id,
                owner_incarnation_id,
                lease_id,
                branch,
            ],
        )?;
        tx.commit()?;
        Ok(PooledCheckoutLease {
            path: PathBuf::from(path),
            source_repo: PathBuf::from(source_repo),
            common_git_dir: PathBuf::from(common_git_dir),
            slot,
            kind,
            worktree_git_dir: None,
            worktree_identity: None,
            session_id: session_id.to_string(),
            owner_incarnation_id: owner_incarnation_id.to_string(),
            lease_id,
            branch: branch.to_string(),
        })
    }

    /// Completes a matching reservation after Git identity was established.
    pub fn finalize_pooled_checkout_for_incarnation(
        &self,
        path: &Path,
        session_id: &str,
        owner_incarnation_id: &str,
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
             SET state='leased', worktree_git_dir=?5, worktree_identity=?6,
                 quarantine_reason=NULL
             WHERE path=?1 AND state='provisioning'
               AND session_id=?2 AND owner_incarnation_id=?3 AND lease_id=?4",
            params![
                path,
                session_id,
                owner_incarnation_id,
                lease_id,
                worktree_git_dir,
                worktree_identity
            ],
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
    pub fn release_pooled_checkout_for_incarnation(
        &self,
        path: &Path,
        session_id: &str,
        owner_incarnation_id: &str,
        lease_id: &str,
    ) -> Result<bool> {
        let path = absolute_db_path(path)?;
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE pooled_checkouts
             SET state='free', session_id=NULL, owner_incarnation_id=NULL, lease_id=NULL,
                 quarantine_reason=NULL
             WHERE path=?1 AND state='leased' AND session_id=?2
               AND owner_incarnation_id=?3 AND lease_id=?4",
            params![path, session_id, owner_incarnation_id, lease_id],
        )?;
        Ok(changed == 1)
    }

    /// Quarantines the current reservation only if its capability still
    /// matches, preventing stale cleanup from affecting a later lease.
    pub fn quarantine_pooled_checkout_lease_for_incarnation(
        &self,
        path: &Path,
        session_id: &str,
        owner_incarnation_id: &str,
        lease_id: &str,
        reason: &str,
    ) -> Result<bool> {
        anyhow::ensure!(!reason.trim().is_empty(), "quarantine reason cannot be empty");
        let path = absolute_db_path(path)?;
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE pooled_checkouts
             SET state='quarantined', session_id=NULL, owner_incarnation_id=NULL,
                 lease_id=NULL, quarantine_reason=?5
             WHERE path=?1 AND state IN ('provisioning','leased')
               AND session_id=?2 AND owner_incarnation_id=?3 AND lease_id=?4",
            params![path, session_id, owner_incarnation_id, lease_id, reason],
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
             SET state='quarantined', session_id=NULL, owner_incarnation_id=NULL,
                 lease_id=NULL,
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

    pub fn recover_quarantined_pooled_checkout(
        &self,
        path: &Path,
        worktree_git_dir: &Path,
        worktree_identity: &str,
    ) -> Result<bool> {
        let path = absolute_db_path(path)?;
        let worktree_git_dir = canonical_db_path(worktree_git_dir)?;
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE pooled_checkouts
             SET state='free',worktree_git_dir=?2,worktree_identity=?3,
                 quarantine_reason=NULL
             WHERE path=?1 AND state='quarantined'
               AND session_id IS NULL AND owner_incarnation_id IS NULL
               AND lease_id IS NULL",
            params![path, worktree_git_dir, worktree_identity],
        )?;
        Ok(changed == 1)
    }

    pub fn remove_missing_quarantined_pooled_checkout(&self, path: &Path) -> Result<bool> {
        let path = absolute_db_path(path)?;
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "DELETE FROM pooled_checkouts
             WHERE path=?1 AND state='quarantined'
               AND session_id IS NULL AND owner_incarnation_id IS NULL
               AND lease_id IS NULL",
            [path],
        )?;
        Ok(changed == 1)
    }

    pub fn remove_missing_pooled_checkout_for_incarnation(
        &self,
        path: &Path,
        session_id: &str,
        owner_incarnation_id: &str,
        lease_id: &str,
    ) -> Result<bool> {
        let path = absolute_db_path(path)?;
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "DELETE FROM pooled_checkouts
             WHERE path=?1 AND state IN ('provisioning','leased')
               AND session_id=?2 AND owner_incarnation_id=?3 AND lease_id=?4",
            params![path, session_id, owner_incarnation_id, lease_id],
        )?;
        Ok(changed == 1)
    }

    pub fn remove_failed_pooled_checkout_for_incarnation(
        &self,
        path: &Path,
        session_id: &str,
        owner_incarnation_id: &str,
        lease_id: &str,
    ) -> Result<bool> {
        let path = absolute_db_path(path)?;
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "DELETE FROM pooled_checkouts
             WHERE path=?1 AND state='provisioning'
               AND session_id=?2 AND owner_incarnation_id=?3 AND lease_id=?4",
            params![path, session_id, owner_incarnation_id, lease_id],
        )?;
        Ok(changed == 1)
    }

    pub fn claim_lowest_free_pooled_checkout(
        &self,
        common_git_dir: &Path,
        session_id: &str,
        branch: &str,
    ) -> Result<Option<PooledCheckoutLease>> {
        self.claim_lowest_free_pooled_checkout_for_incarnation(
            common_git_dir,
            session_id,
            session_id,
            branch,
        )
    }

    pub fn reserve_pooled_checkout(
        &self,
        source_repo: &Path,
        common_git_dir: &Path,
        session_id: &str,
        branch: &str,
    ) -> Result<PooledCheckoutLease> {
        self.reserve_pooled_checkout_for_incarnation(
            source_repo,
            common_git_dir,
            session_id,
            session_id,
            branch,
        )
    }

    pub fn finalize_pooled_checkout(
        &self,
        path: &Path,
        session_id: &str,
        lease_id: &str,
        worktree_git_dir: &Path,
        worktree_identity: &str,
    ) -> Result<bool> {
        let owner = self
            .pooled_checkout_by_path(path)?
            .and_then(|record| record.owner_incarnation_id)
            .context("pooled checkout has no incarnation owner")?;
        self.finalize_pooled_checkout_for_incarnation(
            path,
            session_id,
            &owner,
            lease_id,
            worktree_git_dir,
            worktree_identity,
        )
    }

    pub fn release_pooled_checkout(
        &self,
        path: &Path,
        session_id: &str,
        lease_id: &str,
    ) -> Result<bool> {
        let Some(owner) = self
            .pooled_checkout_by_path(path)?
            .and_then(|record| record.owner_incarnation_id)
        else {
            return Ok(false);
        };
        self.release_pooled_checkout_for_incarnation(path, session_id, &owner, lease_id)
    }

    pub fn quarantine_pooled_checkout_lease(
        &self,
        path: &Path,
        session_id: &str,
        lease_id: &str,
        reason: &str,
    ) -> Result<bool> {
        let Some(owner) = self
            .pooled_checkout_by_path(path)?
            .and_then(|record| record.owner_incarnation_id)
        else {
            return Ok(false);
        };
        self.quarantine_pooled_checkout_lease_for_incarnation(
            path, session_id, &owner, lease_id, reason,
        )
    }

    pub fn remove_failed_pooled_checkout(
        &self,
        path: &Path,
        session_id: &str,
        lease_id: &str,
    ) -> Result<bool> {
        let Some(owner) = self
            .pooled_checkout_by_path(path)?
            .and_then(|record| record.owner_incarnation_id)
        else {
            return Ok(false);
        };
        self.remove_failed_pooled_checkout_for_incarnation(path, session_id, &owner, lease_id)
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

const WORKER_INCARNATION_COLUMNS: &str =
    "SELECT session_id,incarnation_id,orchestrator_id,started_at,
            source_workspace,workspace_path,lease_id,checkout_backed,state
     FROM worker_incarnations";

type RawWorkerIncarnation = (
    String,
    String,
    Option<String>,
    i64,
    String,
    String,
    Option<String>,
    bool,
    String,
);

fn worker_incarnation_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawWorkerIncarnation> {
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
    ))
}

fn raw_worker_incarnation(raw: RawWorkerIncarnation) -> Result<WorkerIncarnation> {
    Ok(WorkerIncarnation {
        session_id: raw.0,
        incarnation_id: raw.1,
        orchestrator_id: raw.2,
        started_at: raw.3,
        source_workspace: raw.4,
        workspace_path: raw.5,
        lease_id: raw.6,
        checkout_backed: raw.7,
        state: parse_worker_state(&raw.8)?,
    })
}

fn parse_worker_state(state: &str) -> Result<WorkerIncarnationState> {
    match state {
        "allocating" => Ok(WorkerIncarnationState::Allocating),
        "active" => Ok(WorkerIncarnationState::Active),
        "retained" => Ok(WorkerIncarnationState::Retained),
        "cleanup_claimed" => Ok(WorkerIncarnationState::CleanupClaimed),
        "release_claimed" => Ok(WorkerIncarnationState::ReleaseClaimed),
        "released" => Ok(WorkerIncarnationState::Released),
        other => anyhow::bail!("invalid worker incarnation state {other:?}"),
    }
}

fn worker_state_name(state: WorkerIncarnationState) -> &'static str {
    match state {
        WorkerIncarnationState::Allocating => "allocating",
        WorkerIncarnationState::Active => "active",
        WorkerIncarnationState::Retained => "retained",
        WorkerIncarnationState::CleanupClaimed => "cleanup_claimed",
        WorkerIncarnationState::ReleaseClaimed => "release_claimed",
        WorkerIncarnationState::Released => "released",
    }
}

fn pooled_checkout_kind_name(kind: PooledCheckoutKind) -> &'static str {
    match kind {
        PooledCheckoutKind::Sibling => "sibling",
        PooledCheckoutKind::Managed => "managed",
        PooledCheckoutKind::Explicit => "explicit",
        PooledCheckoutKind::UnsafeLegacy => "unsafe_legacy",
    }
}

const POOLED_CHECKOUT_COLUMNS: &str =
    "SELECT path,source_repo,common_git_dir,slot,path_kind,worktree_git_dir,
            worktree_identity,state,session_id,owner_incarnation_id,lease_id,
            branch,quarantine_reason
     FROM pooled_checkouts";

type RawPooledCheckout = (
    String,
    String,
    String,
    u32,
    String,
    Option<String>,
    Option<String>,
    String,
    Option<String>,
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
        row.get(11)?,
        row.get(12)?,
    ))
}

fn raw_pooled_checkout(raw: RawPooledCheckout) -> Result<PooledCheckoutRecord> {
    let (
        path,
        source_repo,
        common_git_dir,
        slot,
        kind,
        worktree_git_dir,
        worktree_identity,
        state,
        session_id,
        owner_incarnation_id,
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
    let kind = match kind.as_str() {
        "sibling" => PooledCheckoutKind::Sibling,
        "managed" => PooledCheckoutKind::Managed,
        "explicit" => PooledCheckoutKind::Explicit,
        "unsafe_legacy" => PooledCheckoutKind::UnsafeLegacy,
        other => anyhow::bail!("invalid pooled checkout kind {other:?}"),
    };
    Ok(PooledCheckoutRecord {
        path: PathBuf::from(path),
        source_repo: PathBuf::from(source_repo),
        common_git_dir: PathBuf::from(common_git_dir),
        slot,
        kind,
        worktree_git_dir: worktree_git_dir.map(PathBuf::from),
        worktree_identity,
        state,
        session_id,
        owner_incarnation_id,
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
        kind: record.kind,
        worktree_git_dir: record.worktree_git_dir,
        worktree_identity: record.worktree_identity,
        session_id: record.session_id.context("active checkout has no session")?,
        owner_incarnation_id: record
            .owner_incarnation_id
            .context("active checkout has no worker incarnation")?,
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

    fn spawning_session(id: &str, started_at: i64) -> Session {
        Session {
            id: id.into(),
            orchestrator_id: None,
            name: id.into(),
            repo: String::new(),
            status: SessionStatus::Spawning,
            agent_type: "claude-code".into(),
            cost_usd: 0.0,
            started_at,
            pr_number: None,
            pr_id: None,
            workspace_path: Some("/repo".into()),
            pid: None,
            model: None,
            context_tokens: None,
            catalogue_path: None,
            context_used_pct: None,
            context_total_tokens: None,
            context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None,
            gate_status: None,
        }
    }

    fn production_legacy_worker_fixture(path: &Path, ambiguous: bool) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "
            CREATE TABLE sessions (
                id TEXT PRIMARY KEY, orchestrator_id TEXT,
                name TEXT NOT NULL, repo TEXT NOT NULL,
                status TEXT NOT NULL, agent_type TEXT NOT NULL,
                cost_usd REAL NOT NULL DEFAULT 0, started_at INTEGER NOT NULL,
                pr_number INTEGER, pr_id INTEGER, workspace_path TEXT, pid INTEGER,
                model TEXT, context_tokens INTEGER, current_incarnation_id TEXT,
                incarnation TEXT NOT NULL DEFAULT ''
            );
            CREATE TABLE pooled_checkouts (
                path TEXT PRIMARY KEY, source_repo TEXT NOT NULL,
                common_git_dir TEXT NOT NULL, slot INTEGER NOT NULL,
                path_kind TEXT NOT NULL, worktree_git_dir TEXT,
                worktree_identity TEXT, state TEXT NOT NULL, session_id TEXT,
                owner_incarnation_id TEXT, lease_id TEXT, branch TEXT,
                quarantine_reason TEXT
            );
            CREATE TABLE worker_retention (
                session_id TEXT NOT NULL, orchestrator_id TEXT NOT NULL,
                incarnation TEXT NOT NULL, retained_at INTEGER NOT NULL,
                finalized_at INTEGER,
                PRIMARY KEY(session_id, incarnation)
            );
            CREATE TABLE worker_incarnations (
                session_id TEXT NOT NULL,
                incarnation_id TEXT NOT NULL,
                phase TEXT NOT NULL,
                ui_outcome TEXT,
                physical_tmux_name TEXT NOT NULL,
                pane_id TEXT,
                pane_pid INTEGER,
                workspace_path TEXT,
                pool_path TEXT,
                lease_id TEXT,
                worktree_identity TEXT,
                artifact_dir TEXT NOT NULL,
                started_at INTEGER NOT NULL,
                terminal_at INTEGER,
                migration_hold INTEGER NOT NULL DEFAULT 0,
                allocator_pid INTEGER,
                allocator_token TEXT,
                orchestrator_id TEXT,
                source_workspace TEXT,
                checkout_backed INTEGER,
                state TEXT,
                PRIMARY KEY(session_id, incarnation_id),
                UNIQUE(physical_tmux_name)
            );
            CREATE INDEX worker_incarnations_phase ON worker_incarnations(phase);

            INSERT INTO sessions(
                id,orchestrator_id,name,repo,status,agent_type,started_at,
                workspace_path,pid,current_incarnation_id,incarnation
            ) VALUES
                ('live-pooled','orch','live-pooled','org/repo','working','cursor-agent',
                 200,'/repo-w1',22001,'inc-live','inc-live'),
                ('unrelated-live','other','unrelated-live','org/other','working','cursor-agent',
                 400,'/other-w1',44001,'inc-other','inc-other'),
                ('released-w8','orch','released-w8','org/ninox','terminated','cursor-agent',
                 600,'/ninox-w8',NULL,'inc-w8','inc-w8');

            INSERT INTO pooled_checkouts(
                path,source_repo,common_git_dir,slot,path_kind,state,session_id,
                owner_incarnation_id,lease_id,branch
            ) VALUES
                ('/repo-w1','/repo','/repo/.git',0,'explicit','leased',
                 'live-pooled','inc-live','lease-live','live-branch'),
                ('/ninox-w8','/ninox','/ninox/.git',7,'explicit','free',
                 NULL,NULL,NULL,'released-branch');

            INSERT INTO worker_retention(
                session_id,orchestrator_id,incarnation,retained_at,finalized_at
            ) VALUES ('released-w8','orch','inc-w8',650,700);

            INSERT INTO worker_incarnations VALUES
                ('live-pooled','inc-old','superseded',NULL,'live-pooled',NULL,NULL,
                 '/repo',NULL,NULL,NULL,'/artifacts/inc-old',100,NULL,0,NULL,NULL,
                 'orch','/repo',0,'released'),
                ('live-pooled','inc-live','running',NULL,'nxw-live-inc', '%11',22001,
                 '/repo-w1','/repo-w1','lease-live','identity-live','/sessions/inc-live',
                 200,NULL,0,NULL,NULL,'orch','/repo',1,'active'),
                ('unrelated-live','inc-other-old','superseded',NULL,'unrelated-live',
                 NULL,NULL,'/other',NULL,NULL,NULL,'/artifacts/inc-other-old',
                 300,NULL,0,NULL,NULL,'other','/other',0,'released'),
                ('unrelated-live','inc-other','running',NULL,'nxw-other-inc','%12',44001,
                 '/other-w1',NULL,NULL,NULL,'/sessions/inc-other',
                 400,NULL,0,NULL,NULL,'other','/other',0,'active'),
                ('released-w8','inc-w8-old','superseded',NULL,'released-w8',NULL,NULL,
                 '/ninox',NULL,NULL,NULL,'/artifacts/inc-w8-old',500,NULL,0,NULL,NULL,
                 'orch','/ninox',0,'released'),
                ('released-w8','inc-w8','retained','terminated','nxw-w8-inc','%13',88001,
                 '/ninox-w8','/ninox-w8','lease-w8','identity-w8','/sessions/inc-w8',
                 600,700,0,NULL,NULL,'orch','/ninox',1,'released');
            ",
        )
        .unwrap();
        if ambiguous {
            conn.execute_batch(
                "
                INSERT INTO sessions(
                    id,orchestrator_id,name,repo,status,agent_type,started_at,
                    workspace_path,pid,current_incarnation_id,incarnation
                ) VALUES (
                    'ambiguous','orch','ambiguous','org/repo','working','cursor-agent',
                    800,'/ambiguous',NULL,NULL,''
                );
                INSERT INTO worker_incarnations VALUES
                    ('ambiguous','amb-a','running',NULL,'nxw-amb-a','%21',21001,
                     '/ambiguous',NULL,NULL,NULL,'/sessions/amb-a',800,NULL,0,NULL,NULL,
                     'orch','/ambiguous',0,'active'),
                    ('ambiguous','amb-b','running',NULL,'nxw-amb-b','%22',22002,
                     '/ambiguous',NULL,NULL,NULL,'/sessions/amb-b',800,NULL,0,NULL,NULL,
                     'orch','/ambiguous',0,'active');
                ",
            )
            .unwrap();
        }
    }

    #[test]
    fn opens_authentic_composite_worker_schema_and_keeps_authoritative_rows() {
        let root = tempdir().unwrap();
        let db = root.path().join("production.db");
        production_legacy_worker_fixture(&db, false);

        let store = Store::open(&db).unwrap();

        for (session, incarnation, state) in [
            ("live-pooled", "inc-live", WorkerIncarnationState::Active),
            ("unrelated-live", "inc-other", WorkerIncarnationState::Active),
            ("released-w8", "inc-w8", WorkerIncarnationState::Released),
        ] {
            let worker = store.current_worker_incarnation(session).unwrap().unwrap();
            assert_eq!(worker.incarnation_id, incarnation);
            assert_eq!(worker.state, state);
        }
        drop(store);

        let conn = Connection::open(&db).unwrap();
        let columns = conn
            .prepare("PRAGMA table_info(worker_incarnations)")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(1)?, row.get::<_, i64>(5)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            columns,
            vec![
                ("session_id".into(), 1),
                ("incarnation_id".into(), 0),
                ("orchestrator_id".into(), 0),
                ("started_at".into(), 0),
                ("source_workspace".into(), 0),
                ("workspace_path".into(), 0),
                ("lease_id".into(), 0),
                ("allocator_pid".into(), 0),
                ("allocator_token".into(), 0),
                ("checkout_backed".into(), 0),
                ("state".into(), 0),
            ]
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM worker_incarnations", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            3
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM legacy_worker_runtimes",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            2
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM pooled_checkouts
                 WHERE path='/ninox-w8' AND state='free'
                   AND session_id IS NULL AND owner_incarnation_id IS NULL
                   AND lease_id IS NULL",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            1
        );
        conn.execute(
            "INSERT INTO worker_incarnations(
                session_id,incarnation_id,started_at,source_workspace,
                workspace_path,checkout_backed,state
             ) VALUES('live-pooled','replacement',900,'/repo','/repo',0,'active')
             ON CONFLICT(session_id) DO UPDATE SET incarnation_id=excluded.incarnation_id",
            [],
        )
        .unwrap();
    }

    #[test]
    fn legacy_worker_migration_fails_closed_on_ambiguous_current_row() {
        let root = tempdir().unwrap();
        let db = root.path().join("ambiguous.db");
        production_legacy_worker_fixture(&db, true);

        let error = match Store::open(&db) {
            Ok(_) => panic!("ambiguous legacy rows must fail closed"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("ambiguous"));
        let conn = Connection::open(&db).unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM pragma_table_info('worker_incarnations')",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            21
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM worker_incarnations WHERE session_id='ambiguous'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            2
        );
    }

    #[test]
    fn opens_pre_additive_legacy_worker_schema() {
        let root = tempdir().unwrap();
        let db = root.path().join("pre-additive.db");
        production_legacy_worker_fixture(&db, false);
        let conn = Connection::open(&db).unwrap();
        for column in [
            "state",
            "checkout_backed",
            "source_workspace",
            "orchestrator_id",
        ] {
            conn.execute(&format!("ALTER TABLE worker_incarnations DROP COLUMN {column}"), [])
                .unwrap();
        }
        drop(conn);

        let store = Store::open(&db).unwrap();

        assert_eq!(
            store
                .current_worker_incarnation("live-pooled")
                .unwrap()
                .unwrap()
                .incarnation_id,
            "inc-live"
        );
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
            .reserve_pooled_checkout(source, common, session, &format!("branch-{session}"))
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
                std::thread::spawn(move || {
                    barrier.wait();
                    store
                        .reserve_pooled_checkout(
                            &source,
                            &common,
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
            .reserve_pooled_checkout(&source, &common, "owner", "branch-owner")
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

    #[test]
    fn concurrent_worker_preparation_hard_caps_checkout_backed_slots() {
        let root = tempdir().unwrap().keep();
        let db = root.join("cap.db");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let stores = (0..8)
            .map(|_| Store::open(&db).unwrap())
            .collect::<Vec<_>>();
        let mut threads = Vec::new();
        for (index, store) in stores.into_iter().enumerate() {
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                store.prepare_worker_incarnation(
                    &format!("worker-{index}"),
                    Some("orch"),
                    index as i64,
                    "/repo",
                    true,
                    3,
                )
            }));
        }
        let successes = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .filter(Result::is_ok)
            .count();
        assert_eq!(successes, 3);
        assert_eq!(
            Store::open(db)
                .unwrap()
                .retained_worker_candidates(Some("orch"))
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn checkout_cap_is_local_across_orchestrators() {
        let store = test_store();
        for index in 0..3 {
            store
                .prepare_worker_incarnation(
                    &format!("worker-{index}"),
                    Some(&format!("orch-{index}")),
                    index,
                    "/repo",
                    true,
                    3,
                )
                .unwrap();
        }
        assert!(store
            .prepare_worker_incarnation("worker-3", Some("orch-3"), 3, "/repo", true, 3)
            .is_err());
    }

    #[test]
    fn restart_reclaims_dead_prebind_allocation_from_capacity() {
        let root = tempdir().unwrap().keep();
        let db = root.join("restart.db");
        let store = Store::open(&db).unwrap();
        let stale = store
            .prepare_worker_incarnation("stale", None, 1, "/repo", true, 3)
            .unwrap();
        let source = root.join("repo");
        let common = source.join(".git");
        let admin = common.join("worktrees").join("stale");
        std::fs::create_dir_all(&admin).unwrap();
        let lease = store
            .reserve_pooled_checkout_for_incarnation(
                &source,
                &common,
                "stale",
                &stale.incarnation_id,
                "stale",
            )
            .unwrap();
        assert!(store
            .finalize_pooled_checkout_for_incarnation(
                &lease.path,
                "stale",
                &stale.incarnation_id,
                &lease.lease_id,
                &admin,
                "identity",
            )
            .unwrap());
        let premature_refile = store.prepare_worker_incarnation("stale", None, 2, "/repo", true, 3);
        assert!(premature_refile
            .unwrap_err()
            .to_string()
            .contains("in-progress resource claim"));
        assert_eq!(
            std::fs::read_dir(&store.allocator_lock_dir).unwrap().count(),
            1,
            "failed preparation must not leak allocator capabilities"
        );
        let concurrent = Store::open(&db).unwrap();
        assert!(matches!(
            concurrent
                .current_worker_incarnation("stale")
                .unwrap()
                .unwrap()
                .state,
            WorkerIncarnationState::Allocating
        ));
        drop(concurrent);
        drop(store);

        let reopened = Store::open(db).unwrap();
        assert!(matches!(
            reopened
                .current_worker_incarnation("stale")
                .unwrap()
                .unwrap()
                .state,
            WorkerIncarnationState::Released
        ));
        let record = reopened.pooled_checkout_by_path(&lease.path).unwrap().unwrap();
        assert!(matches!(record.state, PooledCheckoutState::Quarantined));
        assert!(record.session_id.is_none());
        for index in 0..3 {
            reopened
                .prepare_worker_incarnation(
                    &format!("replacement-{index}"),
                    None,
                    index,
                    "/repo",
                    true,
                    3,
                )
                .unwrap();
        }
    }

    #[test]
    fn restart_preserves_mixed_version_live_tokenless_allocation() {
        let root = tempdir().unwrap().keep();
        let db = root.join("mixed-version.db");
        let store = Store::open(&db).unwrap();
        let worker = store
            .prepare_worker_incarnation("legacy", None, 1, "/repo", true, 1)
            .unwrap();
        store
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE worker_incarnations
                 SET allocator_token=NULL,allocator_pid=?3
                 WHERE session_id=?1 AND incarnation_id=?2",
                params!["legacy", worker.incarnation_id, std::process::id()],
            )
            .unwrap();
        drop(store);

        let reopened = Store::open(&db).unwrap();
        assert!(matches!(
            reopened
                .current_worker_incarnation("legacy")
                .unwrap()
                .unwrap()
                .state,
            WorkerIncarnationState::Allocating
        ));
        assert!(reopened
            .prepare_worker_incarnation("replacement", None, 2, "/repo", true, 1)
            .is_err());
    }

    #[test]
    fn startup_removes_orphan_spawning_session_and_name_is_reusable() {
        let root = tempdir().unwrap().keep();
        let db = root.join("orphan-spawn.db");
        let store = Store::open(&db).unwrap();
        store
            .upsert_session(&spawning_session("worker", 1))
            .unwrap();
        drop(store);

        let reopened = Store::open(&db).unwrap();
        assert!(reopened.get_session("worker").unwrap().is_none());
        reopened
            .upsert_session(&spawning_session("worker", 2))
            .unwrap();
        let worker = reopened
            .prepare_worker_incarnation("worker", None, 2, "/repo", true, 3)
            .unwrap();
        assert_eq!(worker.started_at, 2);
    }

    #[test]
    fn startup_terminalizes_legacy_working_ghost_after_failed_launch() {
        let root = tempdir().unwrap().keep();
        let db = root.join("working-ghost.db");
        let store = Store::open(&db).unwrap();
        let mut session = spawning_session("worker", 1);
        session.status = SessionStatus::Working;
        store.upsert_session(&session).unwrap();
        let worker = store
            .prepare_worker_incarnation("worker", None, 1, "/repo", true, 3)
            .unwrap();
        store
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE worker_incarnations
                 SET state='released',allocator_token=NULL,allocator_pid=NULL
                 WHERE session_id=?1 AND incarnation_id=?2",
                params!["worker", worker.incarnation_id],
            )
            .unwrap();
        drop(store);

        let reopened = Store::open(db).unwrap();
        assert!(matches!(
            reopened.get_session("worker").unwrap().unwrap().status,
            SessionStatus::Terminated
        ));
    }

    #[test]
    fn stale_prompt_failure_cannot_terminalize_refile_successor() {
        let store = test_store();
        store
            .upsert_session(&spawning_session("worker", 1))
            .unwrap();
        let old = store
            .prepare_worker_incarnation("worker", None, 1, "/repo", true, 3)
            .unwrap();
        assert!(store
            .bind_worker_incarnation("worker", &old.incarnation_id, "/repo", "/repo-w1", None,)
            .unwrap());
        let successor = store
            .prepare_worker_incarnation("worker", None, 2, "/repo", true, 3)
            .unwrap();
        store
            .upsert_session(&spawning_session("worker", 2))
            .unwrap();

        assert!(!store
            .terminalize_spawning_session_snapshot("worker", 1, "/repo")
            .unwrap());
        assert!(!store
            .update_session_status_snapshot("worker", 1, SessionStatus::Terminated)
            .unwrap());
        let session = store.get_session("worker").unwrap().unwrap();
        assert_eq!(session.started_at, 2);
        assert_eq!(session.status, SessionStatus::Spawning);
        assert_eq!(
            store
                .current_worker_incarnation("worker")
                .unwrap()
                .unwrap()
                .incarnation_id,
            successor.incarnation_id
        );
        assert!(!store
            .insert_spawning_session(&spawning_session("worker", 3))
            .unwrap());
        assert_eq!(store.get_session("worker").unwrap().unwrap().started_at, 2);
    }

    #[test]
    fn startup_backfills_legacy_managed_worktree_into_capacity() {
        let root = tempdir().unwrap().keep();
        let repo = root.join("repo");
        std::fs::create_dir(&repo).unwrap();
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success());
        };
        run(&["init", "-q"]);
        run(&[
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=Test",
            "commit",
            "--allow-empty",
            "-q",
            "-m",
            "init",
        ]);
        let target = root.join("legacy-worker");
        let mut managed =
            crate::worktree::ManagedWorktree::new_at(&repo, &target, "legacy", "legacy").unwrap();
        managed.add_checkout().unwrap();
        managed.persist().unwrap();
        let db = root.join("migration.db");
        let store = Store::open(&db).unwrap();
        store
            .conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO sessions(
                    id,name,repo,status,agent_type,cost_usd,started_at,workspace_path
                 ) VALUES('legacy','legacy','','done','claude-code',0,1,?1)",
                [target.to_string_lossy().as_ref()],
            )
            .unwrap();
        drop(store);

        let reopened = Store::open(db).unwrap();
        let legacy = reopened
            .current_worker_incarnation("legacy")
            .unwrap()
            .expect("legacy managed worker must count after migration");
        assert!(legacy.checkout_backed);
        for index in 0..2 {
            reopened
                .prepare_worker_incarnation(
                    &format!("new-{index}"),
                    None,
                    index + 2,
                    "/repo",
                    true,
                    3,
                )
                .unwrap();
        }
        assert!(reopened
            .prepare_worker_incarnation("over-cap", None, 4, "/repo", true, 3)
            .is_err());
    }

    #[test]
    fn stale_incarnation_cannot_claim_transferred_refile_lease() {
        let (store, source, common, _slots) = pool_fixture();
        let old = store
            .prepare_worker_incarnation("worker", Some("orch"), 1, "/repo", true, 3)
            .unwrap();
        let lease = store
            .reserve_pooled_checkout_for_incarnation(
                &source,
                &common,
                "worker",
                &old.incarnation_id,
                "worker",
            )
            .unwrap();
        assert!(store
            .bind_worker_incarnation(
                "worker",
                &old.incarnation_id,
                "/repo",
                lease.path.to_str().unwrap(),
                Some(&lease.lease_id),
            )
            .unwrap());

        let successor = store
            .prepare_worker_incarnation("worker", Some("orch"), 2, "/repo", true, 3)
            .unwrap();
        let record = store.pooled_checkout_by_path(&lease.path).unwrap().unwrap();
        assert_eq!(
            record.owner_incarnation_id.as_deref(),
            Some(successor.incarnation_id.as_str())
        );
        assert!(store
            .claim_worker_cleanup("worker", &old.incarnation_id)
            .unwrap()
            .is_none());
        assert!(!store
            .release_pooled_checkout_for_incarnation(
                &lease.path,
                "worker",
                &old.incarnation_id,
                &lease.lease_id,
            )
            .unwrap());
    }

    #[test]
    fn failed_bind_rollback_cannot_claim_or_detach_refile_successor_lease() {
        let (store, source, common, _slots) = pool_fixture();
        let old = store
            .prepare_worker_incarnation("worker", Some("orch"), 1, "/repo", true, 3)
            .unwrap();
        let lease = store
            .reserve_pooled_checkout_for_incarnation(
                &source,
                &common,
                "worker",
                &old.incarnation_id,
                "worker",
            )
            .unwrap();
        assert!(store
            .bind_worker_incarnation(
                "worker",
                &old.incarnation_id,
                "/repo",
                lease.path.to_string_lossy().as_ref(),
                Some(&lease.lease_id),
            )
            .unwrap());
        let successor = store
            .prepare_worker_incarnation("worker", Some("orch"), 2, "/repo", true, 3)
            .unwrap();

        assert!(store
            .claim_worker_cleanup_with_lease("worker", &old.incarnation_id, &lease.lease_id)
            .unwrap()
            .is_none());
        let record = store.pooled_checkout_by_path(&lease.path).unwrap().unwrap();
        assert_eq!(
            record.owner_incarnation_id.as_deref(),
            Some(successor.incarnation_id.as_str())
        );
        assert!(matches!(
            store.current_worker_incarnation("worker").unwrap().unwrap().state,
            WorkerIncarnationState::Allocating
        ));
    }

    #[test]
    fn release_claim_has_one_concurrent_winner_and_blocks_refile() {
        let root = tempdir().unwrap().keep();
        let db = root.join("release.db");
        let store = Store::open(&db).unwrap();
        let worker = store
            .prepare_worker_incarnation("worker", Some("orch"), 1, "/repo", true, 3)
            .unwrap();
        store
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE worker_incarnations SET state='retained'
                 WHERE session_id='worker'",
                [],
            )
            .unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let spawn = |barrier: std::sync::Arc<std::sync::Barrier>| {
            let db = db.clone();
            let incarnation_id = worker.incarnation_id.clone();
            std::thread::spawn(move || {
                let store = Store::open(db).unwrap();
                barrier.wait();
                store.claim_worker_release("worker", &incarnation_id).unwrap()
            })
        };
        let first = spawn(barrier.clone());
        let second = spawn(barrier);
        let winners = [first.join().unwrap(), second.join().unwrap()]
            .into_iter()
            .filter(Option::is_some)
            .count();
        assert_eq!(winners, 1);
        assert!(Store::open(db)
            .unwrap()
            .prepare_worker_incarnation("worker", Some("orch"), 2, "/repo", true, 3)
            .is_err());
    }

    #[test]
    fn runtime_start_claim_blocks_release_cleanup_and_refile_through_launch() {
        let store = test_store();
        let worker = store
            .prepare_worker_incarnation("worker", Some("orch"), 1, "/repo", true, 3)
            .unwrap();
        assert!(store
            .bind_worker_incarnation(
                "worker",
                &worker.incarnation_id,
                "/repo",
                "/repo-w1",
                None,
            )
            .unwrap());
        store
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE worker_incarnations SET state='retained'
                 WHERE session_id='worker'",
                [],
            )
            .unwrap();

        let claim = store
            .claim_worker_runtime_start("worker", &worker.incarnation_id)
            .unwrap()
            .unwrap();
        assert!(store.worker_runtime_claimed("worker").unwrap());
        assert!(store
            .claim_worker_release("worker", &worker.incarnation_id)
            .unwrap()
            .is_none());
        assert!(store
            .claim_worker_cleanup("worker", &worker.incarnation_id)
            .unwrap()
            .is_none());
        assert!(store
            .prepare_worker_incarnation("worker", Some("orch"), 2, "/repo", true, 3)
            .is_err());

        assert!(store
            .complete_worker_runtime_start(
                "worker",
                &worker.incarnation_id,
                &claim.claim_id,
            )
            .unwrap());
        assert!(!store.worker_runtime_claimed("worker").unwrap());
        assert!(matches!(
            store.current_worker_incarnation("worker").unwrap().unwrap().state,
            WorkerIncarnationState::Active
        ));
        assert!(store
            .claim_worker_release("worker", &worker.incarnation_id)
            .unwrap()
            .is_none());
    }

    #[test]
    fn release_claim_blocks_runtime_start() {
        let store = test_store();
        let worker = store
            .prepare_worker_incarnation("worker", Some("orch"), 1, "/repo", true, 3)
            .unwrap();
        assert!(store
            .bind_worker_incarnation(
                "worker",
                &worker.incarnation_id,
                "/repo",
                "/repo-w1",
                None,
            )
            .unwrap());
        store
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE worker_incarnations SET state='retained'
                 WHERE session_id='worker'",
                [],
            )
            .unwrap();

        assert!(store
            .claim_worker_release("worker", &worker.incarnation_id)
            .unwrap()
            .is_some());
        assert!(store
            .claim_worker_runtime_start("worker", &worker.incarnation_id)
            .unwrap()
            .is_none());
    }

    #[test]
    fn release_and_runtime_start_have_one_concurrent_winner() {
        let root = tempdir().unwrap().keep();
        let db = root.join("release-resume-race.db");
        let store = Store::open(&db).unwrap();
        let worker = store
            .prepare_worker_incarnation("worker", Some("orch"), 1, "/repo", true, 3)
            .unwrap();
        assert!(store
            .bind_worker_incarnation(
                "worker",
                &worker.incarnation_id,
                "/repo",
                "/repo-w1",
                None,
            )
            .unwrap());
        store
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE worker_incarnations SET state='retained'
                 WHERE session_id='worker'",
                [],
            )
            .unwrap();
        let resume_store = Store::open(&db).unwrap();
        let release_store = Store::open(&db).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let resume_barrier = barrier.clone();
        let resume_incarnation = worker.incarnation_id.clone();
        let resume = std::thread::spawn(move || {
            resume_barrier.wait();
            resume_store
                .claim_worker_runtime_start("worker", &resume_incarnation)
                .unwrap()
                .is_some()
        });
        let release_incarnation = worker.incarnation_id.clone();
        let release = std::thread::spawn(move || {
            barrier.wait();
            release_store
                .claim_worker_release("worker", &release_incarnation)
                .unwrap()
                .is_some()
        });

        let resume_won = resume.join().unwrap();
        let release_won = release.join().unwrap();
        assert_ne!(resume_won, release_won);
        assert_eq!(store.worker_runtime_claimed("worker").unwrap(), resume_won);
        assert_eq!(
            matches!(
                store.current_worker_incarnation("worker").unwrap().unwrap().state,
                WorkerIncarnationState::ReleaseClaimed
            ),
            release_won
        );
    }

    #[test]
    fn restart_reclaims_only_dead_runtime_start_claim() {
        let root = tempdir().unwrap().keep();
        let db = root.join("runtime-claim-restart.db");
        let store = Store::open(&db).unwrap();
        let worker = store
            .prepare_worker_incarnation("worker", None, 1, "/repo", false, 3)
            .unwrap();
        assert!(store
            .bind_worker_incarnation(
                "worker",
                &worker.incarnation_id,
                "/repo",
                "/repo",
                None,
            )
            .unwrap());
        store
            .claim_worker_runtime_start("worker", &worker.incarnation_id)
            .unwrap()
            .unwrap();

        let concurrent = Store::open(&db).unwrap();
        assert!(concurrent.worker_runtime_claimed("worker").unwrap());
        drop(concurrent);
        drop(store);

        let reopened = Store::open(db).unwrap();
        assert!(!reopened.worker_runtime_claimed("worker").unwrap());
        assert!(matches!(
            reopened.current_worker_incarnation("worker").unwrap().unwrap().state,
            WorkerIncarnationState::Active
        ));
    }

    #[test]
    fn removed_low_slot_is_reused_without_suffix_growth() {
        let (store, source, common, _slots) = pool_fixture();
        let first = store
            .reserve_pooled_checkout(&source, &common, "first", "first")
            .unwrap();
        let second = store
            .reserve_pooled_checkout(&source, &common, "second", "second")
            .unwrap();
        assert_eq!((first.slot, second.slot), (0, 1));
        assert!(store
            .remove_failed_pooled_checkout(&first.path, "first", &first.lease_id)
            .unwrap());
        let reused = store
            .reserve_pooled_checkout(&source, &common, "reused", "reused")
            .unwrap();
        assert_eq!(reused.slot, 0);
        assert!(reused.path.ends_with("source-w1"));
    }

    #[test]
    fn pooled_slot_is_derived_from_source_parent() {
        let root = tempdir().unwrap();
        let source = root.path().join("repo");
        let common = source.join(".git");
        std::fs::create_dir_all(&common).unwrap();
        let store = test_store();

        let lease = store
            .reserve_pooled_checkout(&source, &common, "worker", "worker")
            .unwrap();

        assert_eq!(lease.path, root.path().canonicalize().unwrap().join("repo-w1"));
        assert!(!lease.path.starts_with(source.canonicalize().unwrap()));
    }

    #[test]
    fn same_named_nested_repositories_get_distinct_sibling_slots() {
        let root = tempdir().unwrap();
        let outer = root.path().join("repo");
        let nested = outer.join("nested").join("repo");
        let outer_common = outer.join(".git");
        let nested_common = nested.join(".git");
        std::fs::create_dir_all(&outer_common).unwrap();
        std::fs::create_dir_all(&nested_common).unwrap();
        let store = test_store();

        let outer_lease = store
            .reserve_pooled_checkout(&outer, &outer_common, "outer", "outer")
            .unwrap();
        let nested_lease = store
            .reserve_pooled_checkout(&nested, &nested_common, "nested", "nested")
            .unwrap();

        let canonical_root = root.path().canonicalize().unwrap();
        assert_eq!(outer_lease.path, canonical_root.join("repo-w1"));
        assert_eq!(
            nested_lease.path,
            canonical_root.join("repo").join("nested").join("repo-w1")
        );
        assert_ne!(outer_lease.path, nested_lease.path);
        assert!(!nested_lease.path.starts_with(nested.canonicalize().unwrap()));
    }

    #[test]
    fn startup_quarantines_unsafe_legacy_checkout_paths_permanently() {
        let root = tempdir().unwrap().keep();
        let db = root.join("legacy-path.db");
        let source = root.join("repo");
        let common = source.join(".git");
        std::fs::create_dir_all(&common).unwrap();
        let unsafe_path = source.join("worker-w1");
        let store = Store::open(&db).unwrap();
        let worker = store
            .prepare_worker_incarnation("legacy", None, 1, "/repo", true, 3)
            .unwrap();
        assert!(store
            .bind_worker_incarnation(
                "legacy",
                &worker.incarnation_id,
                "/repo",
                unsafe_path.to_string_lossy().as_ref(),
                Some("legacy-lease"),
            )
            .unwrap());
        store
            .conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO pooled_checkouts(
                    path,source_repo,common_git_dir,slot,path_kind,state,
                    session_id,owner_incarnation_id,lease_id,branch
                 ) VALUES(?1,?2,?3,0,'sibling','leased',?4,?5,'legacy-lease','legacy')",
                params![
                    path_text(&unsafe_path).unwrap(),
                    canonical_db_path(&source).unwrap(),
                    canonical_db_path(&common).unwrap(),
                    "legacy",
                    worker.incarnation_id,
                ],
            )
            .unwrap();
        drop(store);

        let reopened = Store::open(&db).unwrap();
        let record = reopened
            .pooled_checkout_by_path(&unsafe_path)
            .unwrap()
            .unwrap();
        assert_eq!(record.kind, PooledCheckoutKind::UnsafeLegacy);
        assert_eq!(record.state, PooledCheckoutState::Quarantined);
        assert!(matches!(
            reopened
                .current_worker_incarnation("legacy")
                .unwrap()
                .unwrap()
                .state,
            WorkerIncarnationState::Retained
        ));
        assert!(reopened
            .claim_lowest_free_pooled_checkout_for_incarnation(
                &common,
                "worker",
                "incarnation",
                "worker",
            )
            .unwrap()
            .is_none());
        assert!(reopened
            .claim_free_pooled_checkout_at_for_incarnation(
                &unsafe_path,
                &common,
                "worker",
                "incarnation",
                "worker",
            )
            .unwrap()
            .is_none());
        reopened
            .prepare_worker_incarnation("replacement-1", None, 2, "/repo", true, 3)
            .unwrap();
        reopened
            .prepare_worker_incarnation("replacement-2", None, 3, "/repo", true, 3)
            .unwrap();
        assert!(reopened
            .prepare_worker_incarnation("replacement-3", None, 4, "/repo", true, 3)
            .is_err());
    }

    #[test]
    fn direct_upgrade_backfills_unsafe_pool_owner_before_quarantine() {
        let root = tempdir().unwrap().keep();
        let db = root.join("early-pr-upgrade.db");
        let source = root.join("repo");
        let common = source.join(".git");
        let unsafe_path = source.join("worker-w1");
        std::fs::create_dir_all(&common).unwrap();
        let store = Store::open(&db).unwrap();
        let mut session = spawning_session("legacy", 1);
        session.status = SessionStatus::Working;
        session.workspace_path = Some(unsafe_path.to_string_lossy().to_string());
        store.upsert_session(&session).unwrap();
        store
            .conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO pooled_checkouts(
                    path,source_repo,common_git_dir,slot,path_kind,state,
                    session_id,owner_incarnation_id,lease_id,branch
                 ) VALUES(?1,?2,?3,0,'sibling','leased',
                          'legacy','legacy-inc','legacy-lease','legacy')",
                params![
                    path_text(&unsafe_path).unwrap(),
                    canonical_db_path(&source).unwrap(),
                    canonical_db_path(&common).unwrap(),
                ],
            )
            .unwrap();
        drop(store);

        let reopened = Store::open(&db).unwrap();
        let worker = reopened
            .current_worker_incarnation("legacy")
            .unwrap()
            .unwrap();
        assert_eq!(worker.incarnation_id, "legacy-inc");
        assert!(matches!(worker.state, WorkerIncarnationState::Retained));
        let pool = reopened
            .pooled_checkout_by_path(&unsafe_path)
            .unwrap()
            .unwrap();
        assert_eq!(pool.state, PooledCheckoutState::Quarantined);
        assert_eq!(pool.kind, PooledCheckoutKind::UnsafeLegacy);
        assert!(reopened
            .prepare_worker_incarnation("replacement-1", None, 2, "/repo", true, 1)
            .is_err());
    }

    #[test]
    fn legacy_cleanup_snapshot_claim_blocks_refile_before_filesystem_effects() {
        let store = test_store();
        let session = Session {
            id: "legacy".into(),
            orchestrator_id: Some("orch".into()),
            name: "legacy".into(),
            repo: "o/r".into(),
            status: SessionStatus::Done,
            agent_type: "claude-code".into(),
            cost_usd: 0.0,
            started_at: 1,
            pr_number: Some(1),
            pr_id: Some(1),
            workspace_path: Some("/workspace".into()),
            pid: None,
            model: None,
            context_tokens: None,
            catalogue_path: None,
            context_used_pct: None,
            context_total_tokens: None,
            context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: Some(1),
            gate_status: None,
        };
        store.upsert_session(&session).unwrap();

        let claim = store
            .claim_worker_cleanup_snapshot("legacy", session.started_at)
            .unwrap()
            .unwrap();
        assert!(matches!(claim.state, WorkerIncarnationState::CleanupClaimed));
        assert!(store
            .prepare_worker_incarnation("legacy", Some("orch"), 2, "/workspace", true, 3)
            .is_err());
    }

    #[test]
    fn stale_legacy_cleanup_snapshot_cannot_claim_refiled_session() {
        let store = test_store();
        let mut session = Session {
            id: "legacy".into(),
            orchestrator_id: Some("orch".into()),
            name: "legacy".into(),
            repo: "o/r".into(),
            status: SessionStatus::Done,
            agent_type: "claude-code".into(),
            cost_usd: 0.0,
            started_at: 1,
            pr_number: Some(1),
            pr_id: Some(1),
            workspace_path: Some("/workspace".into()),
            pid: None,
            model: None,
            context_tokens: None,
            catalogue_path: None,
            context_used_pct: None,
            context_total_tokens: None,
            context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: Some(1),
            gate_status: None,
        };
        store.upsert_session(&session).unwrap();
        session.started_at = 2;
        session.status = SessionStatus::Working;
        session.pr_number = None;
        session.pr_id = None;
        store.upsert_session(&session).unwrap();

        assert!(store
            .claim_worker_cleanup_snapshot("legacy", 1)
            .unwrap()
            .is_none());
        assert!(matches!(
            store.get_session("legacy").unwrap().unwrap().status,
            SessionStatus::Working
        ));
    }
}
