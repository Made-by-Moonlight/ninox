//! `ninox fleet import`: copy sessions and their dependent rows from another
//! store (e.g. a trial store, or one an older release wrote) into this one.
//!
//! The source is never opened for writing: [`snapshot_source`] copies it
//! through SQLite's backup API (so un-checkpointed WAL content comes along),
//! [`check_source_schema`] refuses a store written by a newer binary, and
//! [`Store::open`] on the copy runs this binary's normal migrations. The
//! import itself is one `BEGIN IMMEDIATE` transaction against the target;
//! a dry run runs the identical code and rolls back.

use super::Store;
use crate::types::SessionStatus;
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Tables whose rows travel with their sessions, in insertion order.
pub const IMPORTED_TABLES: &[&str] = &[
    "sessions", "fleet_sessions", "orchestrators", "prs", "ci_status", "review_comments",
    "pr_watches", "orchestrator_plans", "session_deps", "session_messages", "message_events",
    "held_messages", "work_requests", "user_requests",
];

/// Tables deliberately left behind, and why. Every table a fresh store has
/// must be in exactly one of these two lists (enforced by a test), so a
/// table added later can't be silently dropped by an import.
pub const NOT_IMPORTED_TABLES: &[(&str, &str)] = &[
    ("orchestrator_runtimes",    "runtime identity of the source's live panes; re-registered on relaunch"),
    ("human_console_identities", "process identity of the source's consoles"),
    ("fleet_recoveries",         "per-restore bookkeeping; this store's restore starts its own"),
    ("fleet_state",              "the source engine's own flags"),
    ("fleet_lock",               "the source engine's leases"),
    ("harness_auth",             "transient login failures"),
    ("machine_health",           "probe cache"),
    ("session_queue",            "queued spawns carry the source's NINOX_CONFIG/NINOX_BRAIN; reported, re-spawn them"),
    ("ghost_plans",              "proposals name the source's sessions and accepting one spawns here; propose again"),
    ("ghost_nodes",              "nodes of ghost_plans, which are left behind"),
    ("pr_event_cursors",         "this store's poller re-baselines each imported subscription on its first sweep"),
    ("pr_status_cache",          "a cache of GitHub state, refilled by the next sweep"),
    ("merge_finish_nudges",      "one-shot reminder bookkeeping; an imported merged worker may be reminded once more"),
    ("github_budget",            "the last GitHub rate-limit reading; the next sweep re-reads it"),
    ("notifications",            "transient alerts of the old install, not worth carrying over"),
    ("workspace_disk_cache",     "measurements of the source's worktrees, re-measured by the next disk tick"),
    ("idle_prompts",             "the source's orchestrator prompt log; its cooldown names the source's sessions"),
];

/// The worker type an untyped (pre-worker-types) session gets.
const ORCHESTRATOR_TYPE: &str = "Orchestrator";
const IMPLEMENTOR_TYPE: &str = "Implementor";

#[derive(Debug, Clone, Default)]
pub struct ImportOptions {
    pub dry_run:          bool,
    /// Import a session whose id is taken by a *different* session as
    /// `<id>-trial` instead of skipping it.
    pub rename_conflicts: bool,
    pub now_ms:           i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportedSession {
    pub source_id:       String,
    /// Differs from `source_id` only under `--rename-conflicts`.
    pub id:              String,
    pub name:            String,
    pub orchestrator:    bool,
    pub orchestrator_id: Option<String>,
    pub status_before:   SessionStatus,
    pub status_after:    SessionStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// The target already holds this very session (same conversation),
    /// under the given id: its own, or the one an earlier import gave it.
    AlreadyPresent(String),
    /// The id is taken by a different session; `--rename-conflicts` imports
    /// it under the given id instead.
    IdTaken { would_be: String },
    /// Its orchestrator was skipped, and importing it would link it to the
    /// target's unrelated session of that id.
    OrchestratorSkipped(String),
    /// The per-machine "Ask ninox" singleton; the target keeps its own.
    PromptSingleton,
}

impl SkipReason {
    pub fn describe(&self) -> String {
        match self {
            Self::AlreadyPresent(id) => format!("already in this store as {id}"),
            Self::IdTaken { would_be } => format!(
                "id is taken here by a different session (--rename-conflicts imports it as {would_be})"
            ),
            Self::OrchestratorSkipped(o) => format!("its orchestrator {o} was skipped"),
            Self::PromptSingleton => "per-machine Ask-ninox session; this store keeps its own".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SkippedSession {
    pub id:     String,
    pub name:   String,
    pub reason: SkipReason,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImportReport {
    pub dry_run:  bool,
    pub imported: Vec<ImportedSession>,
    pub skipped:  Vec<SkippedSession>,
    /// `(table, rows inserted, matching rows the target already had)` for
    /// every imported table but `sessions`.
    pub rows:     Vec<(String, usize, usize)>,
    /// `(table, rows)` left behind because they involve a skipped session,
    /// which would otherwise point at the target's unrelated one.
    pub dropped:  Vec<(String, usize)>,
    /// Ids of queued spawns in the source that never launched.
    pub queued_not_imported: Vec<String>,
}

impl ImportReport {
    /// `(source id, id here)` of every imported session.
    pub fn id_map(&self) -> Vec<(String, String)> {
        self.imported.iter().map(|s| (s.source_id.clone(), s.id.clone())).collect()
    }
}

/// Copy `source` into `dir/source.db` without writing to it.
pub fn snapshot_source(source: &Path, dir: &Path) -> Result<PathBuf> {
    let copy = dir.join("source.db");
    snapshot(source, &copy)?;
    Ok(copy)
}

/// Copy the store at `source` to `dest` through the backup API. With a
/// `-wal` present the copy reads through it, so writes never checkpointed
/// come along. Without one the store is opened immutable: a plain read-only
/// open of a WAL-mode database creates `-wal`/`-shm` files beside it.
pub fn snapshot(source: &Path, dest: &Path) -> Result<()> {
    if !source.is_file() {
        bail!("no store at {}", source.display());
    }
    let mut wal = source.as_os_str().to_owned();
    wal.push("-wal");
    let immutable = !Path::new(&wal).exists();
    let uri = format!(
        "file:{}?mode=ro{}",
        uri_path(&std::path::absolute(source)?),
        if immutable { "&immutable=1" } else { "" },
    );
    let src = Connection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ).with_context(|| format!("open {} read-only", source.display()))?;
    src.busy_timeout(std::time::Duration::from_secs(5))?;
    let mut dst = Connection::open(dest)?;
    rusqlite::backup::Backup::new(&src, &mut dst)?
        .run_to_completion(256, std::time::Duration::from_millis(0), None)
        .with_context(|| format!("copy {}", source.display()))?;
    Ok(())
}

/// Percent-encode a path for a `file:` URI (e.g. the space in
/// "Application Support").
fn uri_path(path: &Path) -> String {
    let mut out = String::new();
    for b in path.to_string_lossy().bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn schema_of(conn: &Connection, db: &str) -> Result<BTreeMap<String, BTreeSet<String>>> {
    let tables: Vec<String> = conn
        .prepare(&format!(
            "SELECT name FROM {db}.sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'"
        ))?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut out = BTreeMap::new();
    for t in tables {
        let cols: BTreeSet<String> = conn
            .prepare(&format!("PRAGMA {db}.table_info(\"{t}\")"))?
            .query_map([], |r| r.get(1))?
            .collect::<rusqlite::Result<_>>()?;
        out.insert(t, cols);
    }
    Ok(out)
}

/// Refuse a copy written by a newer binary: any table, column or session
/// status this binary's own schema doesn't have. An older source is fine —
/// [`Store::open`] migrates it. Run on the unmigrated copy; `dir` gets a
/// scratch store holding this binary's schema.
pub fn check_source_schema(copy: &Path, dir: &Path) -> Result<()> {
    let known_path = dir.join("known.db");
    drop(Store::open(&known_path)?);
    let known_conn = Connection::open(&known_path)?;
    let known = schema_of(&known_conn, "main")?;
    let known_version: i64 = known_conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;

    let conn = Connection::open(copy)?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let mut unknown: Vec<String> = Vec::new();
    if version > known_version {
        unknown.push(format!("schema version {version} (this binary knows {known_version})"));
    }
    for (table, cols) in schema_of(&conn, "main")? {
        match known.get(&table) {
            None => unknown.push(format!("table {table}")),
            Some(k) => unknown.extend(cols.difference(k).map(|c| format!("column {table}.{c}"))),
        }
    }
    if has_table(&conn, "sessions")? {
        let statuses: Vec<String> = conn
            .prepare("SELECT DISTINCT status FROM sessions")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for s in statuses {
            if parse_status(&s).is_none() {
                unknown.push(format!("session status {s:?}"));
            }
        }
    }
    if !unknown.is_empty() {
        bail!(
            "the source store was written by a newer ninox than this one ({} {}): it has {}. \
             Upgrade this ninox, then import again.",
            env!("CARGO_PKG_VERSION"),
            crate::server_control::BUILD_HASH,
            unknown.join(", "),
        );
    }
    Ok(())
}

fn has_table(conn: &Connection, table: &str) -> Result<bool> {
    Ok(conn
        .query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1", [table], |_| Ok(()))
        .optional()?
        .is_some())
}

fn parse_status(s: &str) -> Option<SessionStatus> {
    serde_json::from_str(&format!("\"{s}\"")).ok()
}

fn status_str(s: &SessionStatus) -> String {
    serde_json::to_string(s).unwrap_or_default().replace('"', "")
}

struct SourceSession {
    id:                String,
    name:              String,
    orchestrator_id:   Option<String>,
    status:            SessionStatus,
    agent_type:        String,
    model:             Option<String>,
    claude_session_id: Option<String>,
    workspace_path:    Option<String>,
    started_at:        i64,
    worker_type:       Option<String>,
}

/// The parts of a target row that tell "the same session" from "a
/// different session that happens to share the id".
struct Fingerprint {
    claude_session_id: Option<String>,
    workspace_path:    Option<String>,
    started_at:        i64,
}

fn same_session(src: &SourceSession, t: &Fingerprint) -> bool {
    match (&src.claude_session_id, &t.claude_session_id) {
        (Some(a), Some(b)) => a == b,
        _ => src.workspace_path == t.workspace_path && src.started_at == t.started_at,
    }
}

enum Decision {
    Insert(String),
    /// Linked to the target's identical session (this id); nothing inserted.
    Present(String),
    Skip(SkipReason),
}

impl Store {
    /// Import every session in `copy` (a [`snapshot_source`] copy that
    /// [`Store::open`] has migrated) with its dependent rows, in one
    /// `BEGIN IMMEDIATE` transaction: all or nothing. A dry run rolls back.
    /// `can_resume(harness, model)` is the harness registry's `--resume`
    /// capability, which decides Interrupted vs. Terminated exactly as
    /// startup reconciliation does for a session whose pane is gone.
    pub fn import_sessions(
        &self,
        copy:       &Path,
        opts:       &ImportOptions,
        can_resume: &dyn Fn(&str, Option<&str>) -> bool,
    ) -> Result<ImportReport> {
        let mut conn = self.conn.lock().unwrap();
        let copy = copy.to_str().context("source copy path is not UTF-8")?;
        conn.execute("ATTACH DATABASE ?1 AS src", [copy])?;
        let result = (|| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let report = import_in_tx(&tx, opts, can_resume)?;
            if opts.dry_run {
                tx.rollback()?;
            } else {
                tx.commit()?;
            }
            Ok(report)
        })();
        // After a commit the import has happened: a cleanup failure must not
        // turn it into a reported failure (and invite a re-run).
        if let Err(e) = conn.execute_batch("DROP TABLE IF EXISTS temp.import_map; DROP TABLE IF EXISTS temp.import_prs; DETACH DATABASE src") {
            tracing::warn!("fleet import: cleanup after import: {e}");
        }
        result
    }
}

fn import_in_tx(
    tx:         &Connection,
    opts:       &ImportOptions,
    can_resume: &dyn Fn(&str, Option<&str>) -> bool,
) -> Result<ImportReport> {
    let sources: Vec<SourceSession> = tx
        .prepare(
            "SELECT id, name, orchestrator_id, status, agent_type, model, claude_session_id,
                    workspace_path, started_at, worker_type
             FROM src.sessions ORDER BY started_at, id",
        )?
        .query_map([], |r| {
            let status: String = r.get(3)?;
            Ok(SourceSession {
                id:                r.get(0)?,
                name:              r.get(1)?,
                orchestrator_id:   r.get(2)?,
                // `check_source_schema` already refused unknown statuses.
                status:            parse_status(&status).unwrap_or(SessionStatus::Working),
                agent_type:        r.get(4)?,
                model:             r.get(5)?,
                claude_session_id: r.get(6)?,
                workspace_path:    r.get(7)?,
                started_at:        r.get(8)?,
                worker_type:       r.get(9)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let src_orchestrators: HashSet<String> = ids(tx, "SELECT id FROM src.orchestrators")?;

    let target: HashMap<String, Fingerprint> = tx
        .prepare("SELECT id, claude_session_id, workspace_path, started_at FROM main.sessions")?
        .query_map([], |r| {
            Ok((r.get::<_, String>(0)?, Fingerprint {
                claude_session_id: r.get(1)?,
                workspace_path:    r.get(2)?,
                started_at:        r.get(3)?,
            }))
        })?
        .collect::<rusqlite::Result<_>>()?;
    // Under any id: a re-run after `--rename-conflicts` finds `<id>-trial`.
    let by_conversation: HashMap<&str, &str> = target.iter()
        .filter_map(|(id, f)| Some((f.claude_session_id.as_deref()?, id.as_str())))
        .collect();
    // A queued spawn, or a session-less orchestrators/fleet row, still owns
    // its id.
    let mut claimed: HashSet<String> = target.keys().cloned().collect();
    claimed.extend(ids(tx, "SELECT id FROM main.session_queue")?);
    claimed.extend(ids(tx, "SELECT id FROM main.orchestrators")?);
    claimed.extend(ids(tx, "SELECT session_id FROM main.fleet_sessions")?);
    let mut taken: HashSet<String> = claimed.clone();
    taken.extend(sources.iter().map(|s| s.id.clone()));

    let mut decisions: Vec<Decision> = sources.iter().map(|s| {
        if let Some(here) = s.claude_session_id.as_deref().and_then(|c| by_conversation.get(c)) {
            return Decision::Present(here.to_string());
        }
        if !claimed.contains(&s.id) {
            return Decision::Insert(s.id.clone());
        }
        if target.get(&s.id).is_some_and(|t| same_session(s, t)) {
            return Decision::Present(s.id.clone());
        }
        if s.id == crate::worker_type::PROMPT_SESSION_ID {
            return Decision::Skip(SkipReason::PromptSingleton);
        }
        let would_be = fresh_id(&s.id, &taken);
        if opts.rename_conflicts {
            taken.insert(would_be.clone());
            Decision::Insert(would_be)
        } else {
            Decision::Skip(SkipReason::IdTaken { would_be })
        }
    }).collect();

    // A child of a skipped orchestrator would attach to the target's
    // unrelated session of that id; skip it too, to a fixpoint (an
    // orchestrator can itself have a parent orchestrator).
    let index: HashMap<&str, usize> = sources.iter().enumerate().map(|(i, s)| (s.id.as_str(), i)).collect();
    loop {
        let mut changed = false;
        for i in 0..sources.len() {
            if !matches!(decisions[i], Decision::Insert(_)) {
                continue;
            }
            let Some(parent) = sources[i].orchestrator_id.as_deref() else { continue };
            if let Some(&p) = index.get(parent) {
                if matches!(decisions[p], Decision::Skip(_)) {
                    decisions[i] = Decision::Skip(SkipReason::OrchestratorSkipped(parent.to_string()));
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }

    tx.execute_batch(
        "DROP TABLE IF EXISTS temp.import_map;
         CREATE TEMP TABLE import_map (old TEXT PRIMARY KEY, new TEXT, inserted INTEGER NOT NULL);",
    )?;
    let mut report = ImportReport { dry_run: opts.dry_run, ..Default::default() };
    let mut map: HashMap<String, String> = HashMap::new();
    for (s, d) in sources.iter().zip(&decisions) {
        match d {
            Decision::Insert(new) => {
                tx.execute("INSERT INTO temp.import_map VALUES (?1, ?2, 1)", params![s.id, new])?;
                map.insert(s.id.clone(), new.clone());
            }
            Decision::Present(here) => {
                tx.execute("INSERT INTO temp.import_map VALUES (?1, ?2, 0)", params![s.id, here])?;
                map.insert(s.id.clone(), here.clone());
                report.skipped.push(SkippedSession {
                    id: s.id.clone(), name: s.name.clone(), reason: SkipReason::AlreadyPresent(here.clone()),
                });
            }
            Decision::Skip(reason) => {
                tx.execute("INSERT INTO temp.import_map VALUES (?1, NULL, 0)", params![s.id])?;
                report.skipped.push(SkippedSession { id: s.id.clone(), name: s.name.clone(), reason: reason.clone() });
            }
        }
    }

    // The source's pids are other processes' now.
    copy_rows(tx, "sessions", &[], &[("id", M), ("orchestrator_id", MC), ("pid", Null)], &ins("id"), false)?;
    let fleet_rows = copy_rows(tx, "fleet_sessions", &[], &[("session_id", M)], &ins("session_id"), true)?;
    report.rows.push(("fleet_sessions".into(), fleet_rows.inserted, fleet_rows.present));
    for (s, d) in sources.iter().zip(&decisions) {
        let Decision::Insert(id) = d else { continue };
        let orchestrator = src_orchestrators.contains(&s.id);
        let after = settle_session(tx, s, id, orchestrator, opts.now_ms, can_resume)?;
        report.imported.push(ImportedSession {
            source_id:       s.id.clone(),
            id:              id.clone(),
            name:            s.name.clone(),
            orchestrator,
            orchestrator_id: s.orchestrator_id.as_ref().map(|o| map.get(o).cloned().unwrap_or_else(|| o.clone())),
            status_before:   s.status.clone(),
            status_after:    after,
        });
    }

    // `prs.id` is the bare PR number, so another repo's PR here can hold it:
    // only PRs whose number is free come over, with their CI and comments,
    // and a session whose number is taken by a different PR loses the link
    // (the poller re-finds its PR).
    tx.execute_batch(&format!(
        "DROP TABLE IF EXISTS temp.import_prs;
         CREATE TEMP TABLE import_prs AS
           SELECT s.id FROM src.prs AS s WHERE {} AND s.id NOT IN (SELECT id FROM main.prs);
         UPDATE main.sessions SET pr_id = NULL
           WHERE id IN (SELECT new FROM temp.import_map WHERE inserted = 1)
             AND pr_id IN (SELECT m.id FROM main.prs AS m JOIN src.prs AS p ON p.id = m.id WHERE m.url <> p.url);",
        ins("session_id"),
    ))?;
    let pr_filter = "s.pr_id IN (SELECT id FROM temp.import_prs)".to_string();
    let tables: Vec<TableCopy> = vec![
        ("orchestrators",      &[],      vec![("id", M)], ins("id")),
        ("prs",                &[],      vec![("session_id", M)], "s.id IN (SELECT id FROM temp.import_prs)".into()),
        ("ci_status",          &[],      vec![], pr_filter.clone()),
        ("review_comments",    &[],      vec![], pr_filter),
        ("pr_watches",         &[],      vec![("opener_session_id", M)], ins("opener_session_id")),
        ("orchestrator_plans", &[],      vec![("orchestrator_id", M)], ins("orchestrator_id")),
        ("session_deps",       &[],      vec![("session_id", M), ("depends_on", MC)], ins("session_id")),
        ("session_messages",   &[],      vec![("session_id", M)], ins("session_id")),
        ("message_events",     &[],      vec![("id", OlderThanTarget), ("from_id", MC), ("to_id", MC)], format!("({} OR {})", ins("from_id"), ins("to_id"))),
        ("held_messages",      &["seq"], vec![("to_id", M), ("attributed_from", MC)], ins("to_id")),
        ("work_requests",      &[],      vec![("from_session", MC), ("orchestrator_id", MC)], format!("({} OR {})", ins("from_session"), ins("orchestrator_id"))),
        ("user_requests",      &[],      vec![("requester_session_id", M)], ins("requester_session_id")),
    ];
    for (table, omit, maps, filter) in tables {
        let c = copy_rows(tx, table, omit, &maps, &filter, true)?;
        report.rows.push((table.to_string(), c.inserted, c.present));
        if c.dropped > 0 {
            report.dropped.push((table.to_string(), c.dropped));
        }
    }

    report.queued_not_imported = ids_vec(tx, "SELECT id FROM src.session_queue WHERE kind = 'spawn' ORDER BY seq")?;
    Ok(report)
}

fn fresh_id(id: &str, taken: &HashSet<String>) -> String {
    let base = format!("{id}-trial");
    if !taken.contains(&base) {
        return base;
    }
    (2..).map(|n| format!("{base}-{n}")).find(|c| !taken.contains(c)).expect("unbounded")
}

/// Rewrite the copied row into the state startup reconciliation would leave
/// a session in whose pane is gone, and give an untyped session its type.
fn settle_session(
    tx:           &Connection,
    s:            &SourceSession,
    id:           &str,
    orchestrator: bool,
    now:          i64,
    can_resume:   &dyn Fn(&str, Option<&str>) -> bool,
) -> Result<SessionStatus> {
    let live = !s.status.is_terminal();
    let after = if live {
        crate::lifecycle::poller::reconciled_status_for_dead_session(
            &s.claude_session_id,
            can_resume(&s.agent_type, s.model.as_deref()),
        )
    } else {
        s.status.clone()
    };
    let worker_type = s.worker_type.clone()
        .unwrap_or_else(|| if orchestrator { ORCHESTRATOR_TYPE } else { IMPLEMENTOR_TYPE }.to_string());
    let terminal_at: Option<i64> = (live && after == SessionStatus::Terminated).then_some(now);
    tx.execute(
        // A 0.x row only has `catalogue_path`; v1 reads it as the write brain.
        "UPDATE main.sessions SET status = ?2, worker_type = ?3, terminal_at = COALESCE(?4, terminal_at),
                brain_write = COALESCE(brain_write, catalogue_path)
         WHERE id = ?1",
        params![id, status_str(&after), worker_type, terminal_at],
    )?;
    if live {
        tx.execute(
            "INSERT INTO main.fleet_sessions (session_id, interrupted_at, last_status, interrupt_cause, reconciled_terminal_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(session_id) DO UPDATE SET
               interrupted_at = excluded.interrupted_at, last_status = excluded.last_status,
               interrupt_cause = excluded.interrupt_cause, reconciled_terminal_at = excluded.reconciled_terminal_at,
               restored_at = NULL, restore_mode = NULL, restore_note = NULL",
            params![id, now, status_str(&s.status), crate::fleet::cause::InterruptCause::Imported.as_str(), terminal_at],
        )?;
    } else if s.status == SessionStatus::Interrupted {
        // Interrupted in the source and never restored here: keep when and
        // why it died, forget the source's restore attempts.
        tx.execute(
            "INSERT INTO main.fleet_sessions (session_id, interrupted_at, last_status, interrupt_cause)
             VALUES (?1, ?2, 'working', ?3)
             ON CONFLICT(session_id) DO UPDATE SET
               interrupted_at = COALESCE(interrupted_at, excluded.interrupted_at),
               last_status = COALESCE(last_status, excluded.last_status),
               interrupt_cause = COALESCE(interrupt_cause, excluded.interrupt_cause),
               restored_at = NULL, restore_mode = NULL, restore_note = NULL",
            params![id, now, crate::fleet::cause::InterruptCause::Imported.as_str()],
        )?;
    }
    Ok(after)
}

/// `(table, omitted autoincrement keys, id columns to rewrite, filter)`.
type TableCopy<'a> = (&'a str, &'a [&'a str], Vec<(&'a str, Map)>, String);

#[derive(Clone, Copy)]
enum Map {
    /// The imported id (the row is only selected when it has one).
    Required,
    /// The imported id if the referenced session was imported, else as is
    /// (the human, an orchestrator this store already has, …). A row whose
    /// column names a skipped session is left behind.
    OrKeep,
    Null,
    /// An autoincrement key renumbered below the target's lowest, in source
    /// order: imported history must neither flash as new in the Graph tab
    /// (`id > since_id`) nor push the target's own events out of
    /// `record_message_event`'s newest-200-by-id pruning.
    OlderThanTarget,
}
use Map::{Null, OlderThanTarget, OrKeep as MC, Required as M};

struct Copied {
    inserted: usize,
    present:  usize,
    dropped:  usize,
}

fn ins(col: &str) -> String {
    ins_on("s", col)
}

fn ins_on(alias: &str, col: &str) -> String {
    format!("{alias}.\"{col}\" IN (SELECT old FROM temp.import_map WHERE inserted = 1)")
}

fn columns(tx: &Connection, db: &str, table: &str) -> Result<Vec<String>> {
    Ok(tx
        .prepare(&format!("PRAGMA {db}.table_info(\"{table}\")"))?
        .query_map([], |r| r.get(1))?
        .collect::<rusqlite::Result<_>>()?)
}

/// `INSERT [OR IGNORE] INTO main.<table> SELECT … FROM src.<table> AS s
/// WHERE <filter>`, every shared column copied except `omit` (autoincrement
/// keys), id columns rewritten through `temp.import_map`.
fn copy_rows(
    tx:        &Connection,
    table:     &str,
    omit:      &[&str],
    maps:      &[(&str, Map)],
    filter:    &str,
    or_ignore: bool,
) -> Result<Copied> {
    let src_cols: HashSet<String> = columns(tx, "src", table)?.into_iter().collect();
    let cols: Vec<String> = columns(tx, "main", table)?
        .into_iter()
        .filter(|c| src_cols.contains(c) && !omit.contains(&c.as_str()))
        .collect();
    let exprs: Vec<String> = cols.iter().map(|c| {
        let mapped = format!("(SELECT new FROM temp.import_map WHERE old = s.\"{c}\")");
        match maps.iter().find(|(m, _)| m == c).map(|(_, kind)| *kind) {
            Some(Map::Required) => mapped,
            Some(Map::OrKeep) => format!("COALESCE({mapped}, s.\"{c}\")"),
            Some(Map::Null) => "NULL".into(),
            Some(Map::OlderThanTarget) => format!(
                "(SELECT COALESCE(MIN(\"{c}\"), 1) FROM main.\"{table}\") - ROW_NUMBER() OVER (ORDER BY s.\"{c}\" DESC)"
            ),
            None => format!("s.\"{c}\""),
        }
    }).collect();
    let quoted: Vec<String> = cols.iter().map(|c| format!("\"{c}\"")).collect();
    let count = |f: &str| -> Result<usize> {
        let n: i64 = tx.query_row(&format!("SELECT COUNT(*) FROM src.\"{table}\" AS s WHERE {f}"), [], |r| r.get(0))?;
        Ok(n.max(0) as usize)
    };
    let all = count(filter)?;
    let mut filter = format!("({filter})");
    for (c, kind) in maps {
        if matches!(kind, Map::OrKeep) {
            filter.push_str(&format!(
                " AND (s.\"{c}\" IS NULL OR s.\"{c}\" NOT IN (SELECT old FROM temp.import_map WHERE new IS NULL))"
            ));
        }
    }
    let selected = count(&filter)?;
    let inserted = tx.execute(
        &format!(
            "INSERT {} INTO main.\"{table}\" ({}) SELECT {} FROM src.\"{table}\" AS s WHERE {filter}",
            if or_ignore { "OR IGNORE" } else { "" },
            quoted.join(", "),
            exprs.join(", "),
        ),
        [],
    )?;
    Ok(Copied { inserted, present: selected.saturating_sub(inserted), dropped: all - selected })
}

fn ids(tx: &Connection, sql: &str) -> Result<HashSet<String>> {
    Ok(ids_vec(tx, sql)?.into_iter().collect())
}

fn ids_vec(tx: &Connection, sql: &str) -> Result<Vec<String>> {
    Ok(tx.prepare(sql)?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
}

#[cfg(test)]
#[path = "store_import_tests.rs"]
mod tests;
