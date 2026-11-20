//! `ninox fleet import --from <db>`: move sessions from another store (a
//! trial store, or one an older release wrote) into this one. The store
//! work lives in `ninox_core::store::import`; this is the CLI around it:
//! the daemon guard, a scratch dir for the source copy, pending inbox
//! files, and the report.

use crate::server_restart::{self, Target};
use ninox_core::{
    config::{AgentConfig, AppConfig},
    server_control::{self, ServerInfo},
    store::{
        import::{self, ImportOptions, ImportReport},
        Store,
    },
};
use std::path::{Path, PathBuf};

#[derive(clap::Args, Debug, Clone)]
pub struct ImportArgs {
    /// The store to import from, e.g.
    /// "~/Library/Application Support/ninox-v1-trial/data/ninox.db". Only
    /// read; never migrated or written.
    #[arg(long)]
    pub from: PathBuf,
    /// Print exactly what would be imported and skipped; write nothing
    #[arg(long)]
    pub dry_run: bool,
    /// Import a session whose id this store already uses for a different
    /// session as `<id>-trial` instead of skipping it
    #[arg(long)]
    pub rename_conflicts: bool,
    /// The source's sessions directory, to carry over undelivered inbox
    /// messages (`<id>.inbox/*.json`); without it they stay behind
    #[arg(long)]
    pub from_sessions_dir: Option<PathBuf>,
}

pub async fn run_cli(args: ImportArgs, db_path: PathBuf, port: u16) -> anyhow::Result<()> {
    if same_file(&args.from, &db_path) {
        anyhow::bail!("--from is this store ({}); nothing to import", db_path.display());
    }
    if let Err(refusal) = daemon_guard(&probe_daemon(port, &db_path).await, port, &db_path) {
        if !args.dry_run {
            anyhow::bail!(refusal);
        }
        eprintln!("note: the real import would be refused: {refusal}");
    }
    // A dry run works on a copy, so it doesn't even migrate (or create) the
    // target.
    let target_copy = Scratch::new()?;
    let store = if args.dry_run {
        let copy = target_copy.0.join("target.db");
        if db_path.is_file() {
            import::snapshot(&db_path, &copy)?;
        }
        Store::open(&copy)?
    } else {
        Store::open(&db_path)?
    };
    if let Some(since) = crate::fleet::restore_running(&store) {
        anyhow::bail!("a fleet restore is running ({since}); import once it has finished");
    }
    let config = AppConfig::load_or_default();
    let sessions_dir = AppConfig::sessions_dir();
    let (report, inbox) = import_store(&store, &config, &args, &sessions_dir, now_ms())?;
    print!("{}", render(&report, &inbox, &args.from));
    Ok(())
}

/// Snapshot, schema check, migrate the copy, import; then carry pending
/// inbox files over.
pub fn import_store(
    store:        &Store,
    config:       &AppConfig,
    args:         &ImportArgs,
    sessions_dir: &Path,
    now:          i64,
) -> anyhow::Result<(ImportReport, InboxCopy)> {
    let scratch = Scratch::new()?;
    let copy = import::snapshot_source(&args.from, &scratch.0)?;
    import::check_source_schema(&copy, &scratch.0)?;
    drop(Store::open(&copy)?);
    let registry = config.registry();
    let can_resume = |harness: &str, model: Option<&str>| {
        let agent = AgentConfig { harness: harness.to_string(), model: model.map(str::to_string) };
        registry.can_resume(&agent)
    };
    let opts = ImportOptions { dry_run: args.dry_run, rename_conflicts: args.rename_conflicts, now_ms: now };
    let report = store.import_sessions(&copy, &opts, &can_resume)?;
    let inbox = match &args.from_sessions_dir {
        Some(from) => copy_pending_inbox(from, sessions_dir, &report.id_map(), args.dry_run),
        None => InboxCopy::default(),
    };
    Ok((report, inbox))
}

/// A temp dir removed on drop, for the source copy.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> std::io::Result<Self> {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("ninox-fleet-import-{}-{}-{seq}", std::process::id(), now_ms()));
        std::fs::create_dir(&dir)?;
        Ok(Self(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct InboxCopy {
    pub copied: usize,
    /// `path: error` per file that couldn't be copied. The database import
    /// has committed by then, so these are reported, not fatal.
    pub failed: Vec<String>,
}

/// Copy each imported session's undelivered inbox messages into this
/// store's sessions dir (under its new id, with a renamed sender's `from`
/// rewritten), never replacing a file. Delivered messages
/// (`.json.delivered`) are an audit trail and stay.
fn copy_pending_inbox(from: &Path, to: &Path, ids: &[(String, String)], dry_run: bool) -> InboxCopy {
    let mut out = InboxCopy::default();
    for (old, new) in ids {
        let Ok(entries) = std::fs::read_dir(from.join(format!("{old}.inbox"))) else { continue };
        let dest = to.join(format!("{new}.inbox"));
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let target = dest.join(entry.file_name());
            if target.exists() {
                continue;
            }
            if dry_run {
                out.copied += 1;
                continue;
            }
            match copy_message(&path, &dest, &target, ids) {
                Ok(()) => out.copied += 1,
                Err(e) => out.failed.push(format!("{}: {e}", path.display())),
            }
        }
    }
    out
}

fn copy_message(path: &Path, dest: &Path, target: &Path, ids: &[(String, String)]) -> anyhow::Result<()> {
    let raw = std::fs::read_to_string(path)?;
    let mut msg: serde_json::Value = serde_json::from_str(&raw)?;
    let renamed = msg.get("from").and_then(|f| f.as_str())
        .and_then(|f| ids.iter().find(|(old, new)| old == f && old != new))
        .map(|(_, new)| new.clone());
    std::fs::create_dir_all(dest)?;
    match renamed {
        Some(new) => {
            msg["from"] = serde_json::Value::String(new);
            std::fs::write(target, serde_json::to_string_pretty(&msg)?)?;
        }
        None => {
            std::fs::copy(path, target)?;
        }
    }
    Ok(())
}

/// What answers on the configured port.
#[derive(Debug)]
pub enum Daemon {
    None,
    Info(ServerInfo),
    /// A daemon from before `/api/server/info` existed: its command line.
    Legacy(String),
}

async fn probe_daemon(port: u16, db_path: &Path) -> Daemon {
    if let Some(info) = server_restart::fetch_info(port).await {
        return Daemon::Info(info);
    }
    if !ninox_core::daemon::port_in_use(port).await {
        return Daemon::None;
    }
    use server_restart::Ops as _;
    let ops = server_restart::LiveOps { store: None };
    let Some(pid) = ops.listener_pid(port).await else {
        eprintln!("warning: port {port} is in use but its process couldn't be found; not checking whether it serves {}", db_path.display());
        return Daemon::None;
    };
    match ops.command_line(pid).await {
        Some(args) => Daemon::Legacy(args),
        None => {
            eprintln!("warning: port {port} is held by pid {pid}, which couldn't be identified; not checking whether it serves {}", db_path.display());
            Daemon::None
        }
    }
}

/// Refuse while a daemon of a different build serves the target store: it
/// would keep writing rows in its own (older) shape around the import. The
/// same build is fine; the import's IMMEDIATE transaction serializes with it.
pub fn daemon_guard(daemon: &Daemon, port: u16, db_path: &Path) -> Result<(), String> {
    let refuse = |what: String| Err(format!(
        "{what} is serving {}; stop or restart the daemon on {port} first (ninox server restart)",
        db_path.display(),
    ));
    match daemon {
        Daemon::None => Ok(()),
        Daemon::Info(info) => {
            if !same_file(Path::new(&info.db_path), db_path) {
                return Ok(());
            }
            let known = |b: &str| !b.is_empty() && b != "unknown";
            let ours = server_control::BUILD_HASH;
            let differs = info.version != env!("CARGO_PKG_VERSION")
                || (known(&info.build) && known(ours) && info.build != ours);
            if differs {
                refuse(format!("a ninox daemon of a different build ({}, this binary is {} ({ours}))", info.label(), env!("CARGO_PKG_VERSION")))
            } else {
                Ok(())
            }
        }
        Daemon::Legacy(args) => {
            let target = Target { port, db_path: db_path.to_path_buf(), ninox_bin: PathBuf::new() };
            if server_restart::is_our_daemon(args, &target) {
                refuse("an older ninox daemon (no /api/server/info)".into())
            } else {
                Ok(())
            }
        }
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).or_else(|_| std::path::absolute(p)).unwrap_or_else(|_| p.to_path_buf());
    canon(a) == canon(b)
}

fn now_ms() -> i64 {
    crate::fleet::now_ms()
}

fn status(s: &ninox_core::types::SessionStatus) -> String {
    serde_json::to_string(s).unwrap_or_default().replace('"', "")
}

pub fn render(r: &ImportReport, inbox: &InboxCopy, from: &Path) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let verb = if r.dry_run { "would import" } else { "imported" };
    let _ = writeln!(out, "{verb} {} session(s) from {}", r.imported.len(), from.display());
    for s in &r.imported {
        let role = if s.orchestrator { "orchestrator" } else if s.orchestrator_id.is_some() { "worker" } else { "standalone" };
        let renamed = if s.id != s.source_id { format!(" (was {})", s.source_id) } else { String::new() };
        let parent = s.orchestrator_id.as_ref().map(|o| format!(" under {o}")).unwrap_or_default();
        let change = if s.status_before == s.status_after {
            status(&s.status_after)
        } else {
            format!("{} → {}", status(&s.status_before), status(&s.status_after))
        };
        let _ = writeln!(out, "  + {}{renamed} [{role}{parent}] {change} — {}", s.id, s.name);
    }
    if !r.skipped.is_empty() {
        let _ = writeln!(out, "skipped {}:", r.skipped.len());
        for s in &r.skipped {
            let _ = writeln!(out, "  - {} — {}: {}", s.id, s.name, s.reason.describe());
        }
    }
    let rows: Vec<String> = r.rows.iter()
        .filter(|(_, n, present)| *n > 0 || *present > 0)
        .map(|(t, n, present)| if *present > 0 { format!("{t} {n} ({present} already here)") } else { format!("{t} {n}") })
        .collect();
    if !rows.is_empty() {
        let _ = writeln!(out, "with rows: {}", rows.join(", "));
    }
    if !r.dropped.is_empty() {
        let d: Vec<String> = r.dropped.iter().map(|(t, n)| format!("{t} {n}")).collect();
        let _ = writeln!(out, "left behind (they involve a skipped session): {}", d.join(", "));
    }
    if inbox.copied > 0 {
        let _ = writeln!(out, "{} {} undelivered inbox message(s)", if r.dry_run { "would copy" } else { "copied" }, inbox.copied);
    }
    for f in &inbox.failed {
        let _ = writeln!(out, "inbox message NOT copied: {f}");
    }
    if !r.queued_not_imported.is_empty() {
        let _ = writeln!(
            out,
            "not imported: {} queued spawn(s) that never started ({}) — re-spawn them here",
            r.queued_not_imported.len(),
            r.queued_not_imported.join(", "),
        );
    }
    if r.dry_run {
        out.push_str("dry run: nothing was written\n");
    } else if r.imported.iter().any(|s| s.status_before != s.status_after) {
        out.push_str("next: `ninox fleet restore --dry-run` to review, then `ninox fleet restore` to resume them\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::types::{Orchestrator, Session, SessionStatus};

    fn session(id: &str, orch: Option<&str>, status: SessionStatus, ws: &str) -> Session {
        Session {
            id: id.into(), orchestrator_id: orch.map(str::to_string), name: id.into(),
            repo: String::new(), status, agent_type: "claude-code".into(), cost_usd: 0.0,
            started_at: 1, pr_number: None, pr_id: None, workspace_path: Some(ws.into()),
            pid: Some(1), model: None, observed_model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: Some(format!("uuid-{id}")), summary: Some(format!("task {id}")),
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None, worker_type: None, success_criteria: Vec::new(),
            completion_outcome: None, self_completed_at: None, cleanup_result: None, cleanup_at: None,
        }
    }

    fn args(from: &Path, dry_run: bool) -> ImportArgs {
        ImportArgs { from: from.to_path_buf(), dry_run, rename_conflicts: false, from_sessions_dir: None }
    }

    #[test]
    fn imported_live_sessions_appear_in_the_restore_plan_workers_first() {
        let iso = ninox_core::test_isolation::isolate();
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().to_str().unwrap();
        let src = iso.root().join("trial.db");
        {
            let st = Store::open(&src).unwrap();
            st.upsert_session(&session("imp-o", None, SessionStatus::Working, ws)).unwrap();
            st.upsert_orchestrator(&Orchestrator { id: "imp-o".into(), name: "o".into(), created_at: 0 }).unwrap();
            st.upsert_session(&session("imp-w", Some("imp-o"), SessionStatus::PrOpen, ws)).unwrap();
        }
        let target = Store::open(iso.root().join("ninox.db")).unwrap();
        let config = AppConfig::default();

        let (dry, _) = import_store(&target, &config, &args(&src, true), &iso.root().join("sessions"), 10).unwrap();
        assert_eq!(dry.imported.len(), 2);
        assert!(crate::fleet::restore_plan(&target, &config).unwrap().is_empty(), "dry run wrote nothing");

        import_store(&target, &config, &args(&src, false), &iso.root().join("sessions"), 10).unwrap();
        let plan = crate::fleet::restore_plan(&target, &config).unwrap();
        let order: Vec<_> = plan.steps().map(|s| (s.session_id.as_str(), s.mode)).collect();
        use ninox_core::fleet::RestoreMode::Resume;
        assert_eq!(order, [("imp-w", Resume), ("imp-o", Resume)]);
        assert_eq!(target.get_session("imp-w").unwrap().unwrap().status, SessionStatus::Interrupted);
        let brief = crate::fleet::brief(&target, &config, "imp-o").unwrap();
        assert!(brief.contains("moved here from another Ninox store"), "{brief}");
    }

    #[tokio::test]
    async fn a_dry_run_neither_creates_nor_migrates_the_target() {
        let iso = ninox_core::test_isolation::isolate();
        let src = iso.root().join("trial.db");
        {
            let st = Store::open(&src).unwrap();
            st.upsert_session(&session("dry-w", None, SessionStatus::Working, "/ws")).unwrap();
        }
        let port = { std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port() };
        let missing = iso.root().join("missing.db");
        run_cli(args(&src, true), missing.clone(), port).await.unwrap();
        assert!(!missing.exists());

        let old = iso.root().join("old.db");
        rusqlite::Connection::open(&old).unwrap()
            .execute_batch("CREATE TABLE sessions (id TEXT PRIMARY KEY, orchestrator_id TEXT, name TEXT NOT NULL, repo TEXT NOT NULL, status TEXT NOT NULL, agent_type TEXT NOT NULL, cost_usd REAL NOT NULL DEFAULT 0, started_at INTEGER NOT NULL, pr_number INTEGER, pr_id INTEGER, workspace_path TEXT, pid INTEGER, model TEXT, context_tokens INTEGER);")
            .unwrap();
        let before = std::fs::read(&old).unwrap();
        run_cli(args(&src, true), old.clone(), port).await.unwrap();
        assert_eq!(std::fs::read(&old).unwrap(), before);
    }

    #[test]
    fn pending_inbox_messages_follow_their_session_and_never_replace_files() {
        let iso = ninox_core::test_isolation::isolate();
        let from = iso.root().join("from");
        let to = iso.root().join("to");
        std::fs::create_dir_all(from.join("a.inbox")).unwrap();
        std::fs::write(from.join("a.inbox/m1.json"), r#"{"id":"m1","text":"hi","sentAt":1,"from":"a"}"#).unwrap();
        std::fs::write(from.join("a.inbox/m2.json"), "new").unwrap();
        std::fs::write(from.join("a.inbox/m0.json.delivered"), "old").unwrap();
        std::fs::create_dir_all(to.join("a-trial.inbox")).unwrap();
        std::fs::write(to.join("a-trial.inbox/m2.json"), "kept").unwrap();
        let ids = [("a".to_string(), "a-trial".to_string())];

        assert_eq!(copy_pending_inbox(&from, &to, &ids, true).copied, 1);
        assert!(!to.join("a-trial.inbox/m1.json").exists(), "dry run copies nothing");
        assert_eq!(copy_pending_inbox(&from, &to, &ids, false), InboxCopy { copied: 1, failed: vec![] });
        let m1: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(to.join("a-trial.inbox/m1.json")).unwrap()).unwrap();
        assert_eq!((m1["text"].as_str(), m1["from"].as_str()), (Some("hi"), Some("a-trial")), "renamed sender rewritten");
        assert_eq!(std::fs::read_to_string(to.join("a-trial.inbox/m2.json")).unwrap(), "kept");
        assert!(!to.join("a-trial.inbox/m0.json.delivered").exists());
    }

    fn info(version: &str, build: &str, db: &Path) -> ServerInfo {
        ServerInfo {
            version: version.into(), build: build.into(), pid: 1, port: 45678,
            db_path: db.display().to_string(), in_process: false, issues: Vec::new(),
        }
    }

    #[test]
    fn the_daemon_guard_refuses_only_another_build_serving_this_store() {
        let iso = ninox_core::test_isolation::isolate();
        let db = iso.root().join("ninox.db");
        let other_db = iso.root().join("other.db");
        let this = (env!("CARGO_PKG_VERSION"), server_control::BUILD_HASH);

        assert!(daemon_guard(&Daemon::None, 45678, &db).is_ok());
        assert!(daemon_guard(&Daemon::Info(info(this.0, this.1, &db)), 45678, &db).is_ok(), "same build");
        assert!(daemon_guard(&Daemon::Info(info("0.1.0", "abc", &other_db)), 45678, &db).is_ok(), "another store");

        let err = daemon_guard(&Daemon::Info(info("0.1.0", "abc", &db)), 45678, &db).unwrap_err();
        assert!(err.contains("stop or restart the daemon on 45678 first (ninox server restart)"), "{err}");
        assert!(err.contains("0.1.0 (abc)"), "{err}");

        let legacy = format!("/opt/bin/ninox --headless --port 45678 --db {}", db.display());
        let err = daemon_guard(&Daemon::Legacy(legacy), 45678, &db).unwrap_err();
        assert!(err.contains("older ninox daemon"), "{err}");
        let legacy_other = format!("/opt/bin/ninox --headless --port 45678 --db {}", other_db.display());
        assert!(daemon_guard(&Daemon::Legacy(legacy_other), 45678, &db).is_ok());
        assert!(daemon_guard(&Daemon::Legacy("python -m http.server".into()), 45678, &db).is_ok());
    }

    #[test]
    fn render_lists_imports_skips_and_the_next_step() {
        use ninox_core::store::import::{ImportedSession, SkipReason, SkippedSession};
        let r = ImportReport {
            dry_run: true,
            imported: vec![ImportedSession {
                source_id: "w".into(), id: "w-trial".into(), name: "fix ci".into(), orchestrator: false,
                orchestrator_id: Some("o".into()), status_before: SessionStatus::PrOpen,
                status_after: SessionStatus::Interrupted,
            }],
            skipped: vec![SkippedSession { id: "o2".into(), name: "x".into(), reason: SkipReason::AlreadyPresent("o2".into()) }],
            rows: vec![("prs".into(), 1, 0), ("message_events".into(), 0, 0), ("work_requests".into(), 1, 1)],
            dropped: vec![("message_events".into(), 3)],
            queued_not_imported: vec!["q".into()],
        };
        let out = render(&r, &InboxCopy { copied: 2, failed: vec!["/x/m.json: denied".into()] }, Path::new("/t/ninox.db"));
        assert_eq!(out, "\
would import 1 session(s) from /t/ninox.db
  + w-trial (was w) [worker under o] pr_open → interrupted — fix ci
skipped 1:
  - o2 — x: already in this store as o2
with rows: prs 1, work_requests 1 (1 already here)
left behind (they involve a skipped session): message_events 3
would copy 2 undelivered inbox message(s)
inbox message NOT copied: /x/m.json: denied
not imported: 1 queued spawn(s) that never started (q) — re-spawn them here
dry run: nothing was written
");
    }
}
