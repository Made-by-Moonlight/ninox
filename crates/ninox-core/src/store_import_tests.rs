use super::*;
use crate::store::Store;
use crate::types::{Orchestrator, Session, SessionStatus};
use std::path::Path;

const NOW: i64 = 1_800_000_000_000;

fn session(id: &str, orch: Option<&str>, status: SessionStatus) -> Session {
    Session {
        id: id.into(), orchestrator_id: orch.map(str::to_string), name: format!("name-{id}"),
        repo: "repo".into(), status, agent_type: "claude-code".into(), cost_usd: 1.5,
        started_at: 100, pr_number: None, pr_id: None, workspace_path: Some(format!("/ws/{id}")),
        pid: Some(4242), model: None, observed_model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
        context_used_pct: None, context_total_tokens: None, context_window_size: None,
        claude_session_id: Some(format!("uuid-{id}")), summary: Some(format!("task {id}")),
        terminal_at: None, gate_status: None, merged_at: None,
        activity: Default::default(), activity_note: None, activity_since: None,
        machine_id: None, worker_type: None, success_criteria: Vec::new(),
        completion_outcome: None, self_completed_at: None, cleanup_result: None, cleanup_at: None,
    }
}

fn exec(db: &Path, sql: &str) {
    Connection::open(db).unwrap().execute_batch(sql).unwrap();
}

fn count(db: &Path, sql: &str) -> i64 {
    Connection::open(db).unwrap().query_row(sql, [], |r| r.get(0)).unwrap()
}

fn can_resume(harness: &str, _model: Option<&str>) -> bool {
    harness == "claude-code"
}

fn opts(dry_run: bool, rename_conflicts: bool) -> ImportOptions {
    ImportOptions { dry_run, rename_conflicts, now_ms: NOW }
}

/// A v1 source: orchestrator `o1` with a live worker `w1` (PR, CI, review
/// comments, a watch, a dep, messages, a held message, requests), a
/// finished worker `w2`, an untyped standalone `s1` on a harness that
/// can't resume, and a queued spawn.
fn seed_v1_source(path: &Path) {
    let st = Store::open(path).unwrap();
    st.upsert_session(&session("o1", None, SessionStatus::Working)).unwrap();
    st.upsert_orchestrator(&Orchestrator { id: "o1".into(), name: "o".into(), created_at: 1 }).unwrap();
    let mut w1 = session("w1", Some("o1"), SessionStatus::PrOpen);
    w1.pr_id = Some(9001);
    w1.pr_number = Some(41);
    w1.worker_type = Some("Researcher".into());
    w1.brain_reads = vec!["/brains/a".into(), "/brains/b".into()];
    w1.brain_write = Some("/brains/a".into());
    st.upsert_session(&w1).unwrap();
    st.upsert_session(&session("w2", Some("o1"), SessionStatus::Done)).unwrap();
    let mut s1 = session("s1", None, SessionStatus::Working);
    s1.agent_type = "no-resume-harness".into();
    st.upsert_session(&s1).unwrap();
    st.record_spawn_facts("w1", "do the thing", Some("feat/w1")).unwrap();
    drop(st);
    exec(path, "
        UPDATE sessions SET success_criteria = '[\"pr_merged\"]' WHERE id = 'w1';
        INSERT INTO prs VALUES (9001, 41, 'title', 'https://x/pr/41', 'body', 'w1');
        INSERT INTO ci_status VALUES (9001, 3, 3, 0, 0);
        INSERT INTO review_comments VALUES (77, 9001, 'rev', 'nit', 'a.rs', 3, 5);
        INSERT INTO pr_watches VALUES ('repo', 41, 'https://x/pr/41', 'w1', 6);
        INSERT INTO orchestrator_plans VALUES ('o1', '/plans/o1.md', 1, 2);
        INSERT INTO session_deps VALUES ('w1', 'w2', 'blocks', NULL, 3);
        INSERT INTO session_messages (session_id, delivered_count) VALUES ('w1', 2);
        INSERT INTO message_events (from_id, to_id, text, delivered_at, kind) VALUES ('o1', 'w1', 'hi', 10, 'status');
        INSERT INTO message_events (from_id, to_id, text, delivered_at, kind) VALUES ('w1', 'o1', 'pr open', 11, 'status');
        INSERT INTO held_messages (to_id, tagged, attributed_from, defanged, kind, created_at) VALUES ('w1', 't', 'o1', 'd', 'status', 12);
        INSERT INTO work_requests VALUES ('wr-1', 'w1', 'o1', 'more work', 13, NULL, NULL);
        INSERT INTO user_requests VALUES ('ur-1', 'w1', 'which?', 'async', 'pending', NULL, NULL, NULL, 14);
        INSERT INTO session_queue (id, location, state, kind, enqueued_at, session, payload)
            VALUES ('q1', '', 'queued', 'spawn', 15, '{}', '{}');
        INSERT INTO orchestrator_runtimes VALUES ('o1', 'rt', 'epoch', 'tmux', '%1', 1, 1, 1);
    ");
}

fn import(target: &Store, source: &Path, scratch: &Path, o: &ImportOptions) -> Result<ImportReport> {
    let copy = snapshot_source(source, scratch)?;
    check_source_schema(&copy, scratch)?;
    drop(Store::open(&copy)?);
    target.import_sessions(&copy, o, &can_resume)
}

fn scratch(iso: &crate::test_isolation::Isolation, name: &str) -> PathBuf {
    let d = iso.root().join(name);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn every_store_table_is_either_imported_or_deliberately_left_behind() {
    let iso = crate::test_isolation::isolate();
    let db = iso.root().join("fresh.db");
    drop(Store::open(&db).unwrap());
    let conn = Connection::open(&db).unwrap();
    let tables: BTreeSet<String> = schema_of(&conn, "main").unwrap().into_keys().collect();
    let listed: BTreeSet<String> = IMPORTED_TABLES.iter().copied()
        .chain(NOT_IMPORTED_TABLES.iter().map(|(t, _)| *t))
        .map(str::to_string)
        .collect();
    assert_eq!(tables, listed);
    assert_eq!(listed.len(), IMPORTED_TABLES.len() + NOT_IMPORTED_TABLES.len(), "a table is in both lists");
}

#[test]
fn imports_everything_into_an_empty_store_as_restorable_sessions() {
    let iso = crate::test_isolation::isolate();
    let src = iso.root().join("trial.db");
    seed_v1_source(&src);
    let target_path = iso.root().join("ninox.db");
    let target = Store::open(&target_path).unwrap();

    let report = import(&target, &src, &scratch(&iso, "s"), &opts(false, false)).unwrap();
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    let by_id: HashMap<_, _> = report.imported.iter().map(|s| (s.id.as_str(), s)).collect();
    assert_eq!(by_id.len(), 4);
    assert_eq!(by_id["o1"].status_after, SessionStatus::Interrupted);
    assert!(by_id["o1"].orchestrator);
    assert_eq!(by_id["w1"].status_after, SessionStatus::Interrupted);
    assert_eq!(by_id["w2"].status_after, SessionStatus::Done);
    assert_eq!(by_id["s1"].status_after, SessionStatus::Terminated);
    assert_eq!(report.queued_not_imported, ["q1"]);
    let copied: Vec<&str> = std::iter::once("sessions").chain(report.rows.iter().map(|(t, ..)| t.as_str())).collect();
    assert_eq!(copied, IMPORTED_TABLES, "IMPORTED_TABLES must list exactly what the import copies");

    let o1 = target.get_session("o1").unwrap().unwrap();
    let w1 = target.get_session("w1").unwrap().unwrap();
    assert_eq!(w1.orchestrator_id.as_deref(), Some("o1"));
    assert_eq!(w1.claude_session_id.as_deref(), Some("uuid-w1"));
    assert_eq!(w1.workspace_path.as_deref(), Some("/ws/w1"));
    assert_eq!(w1.pid, None);
    assert_eq!(w1.cost_usd, 1.5);
    assert_eq!(w1.worker_type.as_deref(), Some("Researcher"), "an explicit type is kept");
    assert_eq!(w1.brain_reads, ["/brains/a", "/brains/b"]);
    assert_eq!(w1.brain_write.as_deref(), Some("/brains/a"));
    assert_eq!(o1.worker_type.as_deref(), Some("Orchestrator"));
    assert_eq!(target.get_session("s1").unwrap().unwrap().worker_type.as_deref(), Some("Implementor"));
    assert_eq!(w1.success_criteria, vec![crate::worker_type::SuccessCriterion::PrMerged]);
    assert!(target.list_orchestrators().unwrap().iter().any(|o| o.id == "o1"));

    let rec = target.fleet_record("w1").unwrap().unwrap();
    assert_eq!(rec.task_brief.as_deref(), Some("do the thing"));
    assert_eq!(rec.branch.as_deref(), Some("feat/w1"));
    assert_eq!(rec.interrupted_at, Some(NOW));
    assert_eq!(rec.last_status, Some(SessionStatus::PrOpen));
    assert_eq!(rec.interrupt_cause.as_deref(), Some("imported"));
    assert!(rec.awaiting_restore());
    let s1 = target.get_session("s1").unwrap().unwrap();
    let s1_rec = target.fleet_record("s1").unwrap().unwrap();
    assert!(crate::fleet::awaits_restore(&s1, &s1_rec), "a non-resumable session is a fresh-restart candidate");

    for (sql, n) in [
        ("SELECT COUNT(*) FROM prs WHERE session_id = 'w1'", 1),
        ("SELECT COUNT(*) FROM ci_status WHERE pr_id = 9001", 1),
        ("SELECT COUNT(*) FROM review_comments WHERE pr_id = 9001", 1),
        ("SELECT COUNT(*) FROM pr_watches WHERE opener_session_id = 'w1'", 1),
        ("SELECT COUNT(*) FROM orchestrator_plans WHERE orchestrator_id = 'o1'", 1),
        ("SELECT COUNT(*) FROM session_deps WHERE session_id = 'w1' AND depends_on = 'w2'", 1),
        ("SELECT delivered_count FROM session_messages WHERE session_id = 'w1'", 2),
        ("SELECT COUNT(*) FROM message_events", 2),
        ("SELECT COUNT(*) FROM held_messages WHERE to_id = 'w1' AND attributed_from = 'o1'", 1),
        ("SELECT COUNT(*) FROM work_requests WHERE from_session = 'w1' AND orchestrator_id = 'o1'", 1),
        ("SELECT COUNT(*) FROM user_requests WHERE requester_session_id = 'w1'", 1),
        ("SELECT COUNT(*) FROM orchestrator_runtimes", 0),
        ("SELECT COUNT(*) FROM session_queue", 0),
    ] {
        assert_eq!(count(&target_path, sql), n, "{sql}");
    }
}

#[test]
fn conflicts_are_skipped_and_reported_and_never_overwritten() {
    let iso = crate::test_isolation::isolate();
    let src = iso.root().join("trial.db");
    seed_v1_source(&src);
    let target_path = iso.root().join("ninox.db");
    let target = Store::open(&target_path).unwrap();
    // A different session that happens to be called `o1`, and the very
    // same `w2` the trial has.
    let mut other = session("o1", None, SessionStatus::Working);
    other.claude_session_id = Some("someone-else".into());
    other.name = "mine".into();
    target.upsert_session(&other).unwrap();
    target.upsert_session(&session("w2", Some("o1"), SessionStatus::Done)).unwrap();

    let report = import(&target, &src, &scratch(&iso, "s"), &opts(false, false)).unwrap();
    let skipped: HashMap<_, _> = report.skipped.iter().map(|s| (s.id.as_str(), &s.reason)).collect();
    assert_eq!(skipped["o1"], &SkipReason::IdTaken { would_be: "o1-trial".into() });
    assert_eq!(skipped["w1"], &SkipReason::OrchestratorSkipped("o1".into()));
    assert_eq!(skipped["w2"], &SkipReason::AlreadyPresent("w2".into()));
    let imported: Vec<_> = report.imported.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(imported, ["s1"]);

    let o1 = target.get_session("o1").unwrap().unwrap();
    assert_eq!((o1.name.as_str(), o1.status), ("mine", SessionStatus::Working), "target row untouched");
    assert_eq!(count(&target_path, "SELECT COUNT(*) FROM message_events"), 0);
    assert_eq!(count(&target_path, "SELECT COUNT(*) FROM orchestrator_plans"), 0, "o1's plan is the trial o1's");
}

#[test]
fn rows_naming_a_skipped_session_are_left_behind_not_pointed_at_the_targets_namesake() {
    let iso = crate::test_isolation::isolate();
    let src = iso.root().join("trial.db");
    seed_v1_source(&src);
    let target_path = iso.root().join("ninox.db");
    let target = Store::open(&target_path).unwrap();
    let mut other = session("w1", None, SessionStatus::Working);
    other.claude_session_id = Some("someone-else".into());
    target.upsert_session(&other).unwrap();

    let report = import(&target, &src, &scratch(&iso, "s"), &opts(false, false)).unwrap();
    assert!(report.imported.iter().any(|s| s.id == "o1"));
    assert!(report.skipped.iter().any(|s| s.id == "w1"));
    let dropped: HashMap<_, _> = report.dropped.iter().map(|(t, n)| (t.as_str(), *n)).collect();
    assert_eq!(dropped.get("message_events"), Some(&2), "{:?}", report.dropped);
    assert_eq!(dropped.get("work_requests"), Some(&1), "{:?}", report.dropped);
    for sql in [
        "SELECT COUNT(*) FROM message_events",
        "SELECT COUNT(*) FROM work_requests",
        "SELECT COUNT(*) FROM held_messages",
        "SELECT COUNT(*) FROM session_deps",
        "SELECT COUNT(*) FROM prs",
    ] {
        assert_eq!(count(&target_path, sql), 0, "{sql}");
    }
}

#[test]
fn re_running_after_a_renaming_import_finds_every_session_already_here() {
    let iso = crate::test_isolation::isolate();
    let src = iso.root().join("trial.db");
    seed_v1_source(&src);
    let target_path = iso.root().join("ninox.db");
    let target = Store::open(&target_path).unwrap();
    let mut other = session("o1", None, SessionStatus::Working);
    other.claude_session_id = Some("someone-else".into());
    target.upsert_session(&other).unwrap();
    import(&target, &src, &scratch(&iso, "a"), &opts(false, true)).unwrap();
    let sessions = target.list_sessions().unwrap().len();
    let events = count(&target_path, "SELECT COUNT(*) FROM message_events");

    for (dir, rename) in [("b", true), ("c", false)] {
        let again = import(&target, &src, &scratch(&iso, dir), &opts(false, rename)).unwrap();
        assert!(again.imported.is_empty(), "{:?}", again.imported);
        let o1 = again.skipped.iter().find(|s| s.id == "o1").unwrap();
        assert_eq!(o1.reason, SkipReason::AlreadyPresent("o1-trial".into()));
    }
    assert_eq!(target.list_sessions().unwrap().len(), sessions);
    assert_eq!(count(&target_path, "SELECT COUNT(*) FROM message_events"), events);
}

#[test]
fn a_pr_number_held_by_another_repos_pr_is_not_merged_into_it() {
    let iso = crate::test_isolation::isolate();
    let src = iso.root().join("trial.db");
    seed_v1_source(&src);
    let target_path = iso.root().join("ninox.db");
    let target = Store::open(&target_path).unwrap();
    exec(&target_path, "INSERT INTO prs VALUES (9001, 9001, 'other', 'https://x/other/pull/9001', '', 'elsewhere');");

    import(&target, &src, &scratch(&iso, "s"), &opts(false, false)).unwrap();
    assert_eq!(target.get_session("w1").unwrap().unwrap().pr_id, None, "link to the other PR cleared");
    assert_eq!(target.get_session("w1").unwrap().unwrap().pr_number, Some(41));
    for (sql, n) in [
        ("SELECT COUNT(*) FROM prs", 1),
        ("SELECT COUNT(*) FROM prs WHERE session_id = 'elsewhere'", 1),
        ("SELECT COUNT(*) FROM review_comments", 0),
        ("SELECT COUNT(*) FROM ci_status", 0),
    ] {
        assert_eq!(count(&target_path, sql), n, "{sql}");
    }
}

#[test]
fn imported_message_events_sort_before_the_targets_own() {
    let iso = crate::test_isolation::isolate();
    let src = iso.root().join("trial.db");
    seed_v1_source(&src);
    let target_path = iso.root().join("ninox.db");
    let target = Store::open(&target_path).unwrap();
    target.record_message_event(Some("me"), "you", "mine", crate::types::MessageKind::Status).unwrap();
    let mine: i64 = count(&target_path, "SELECT id FROM message_events WHERE text = 'mine'");

    import(&target, &src, &scratch(&iso, "s"), &opts(false, false)).unwrap();
    let first: i64 = count(&target_path, "SELECT id FROM message_events WHERE text = 'hi'");
    let second: i64 = count(&target_path, "SELECT id FROM message_events WHERE text = 'pr open'");
    assert!(first < second && second < mine, "{first} {second} {mine}");
    assert!(target.recent_message_events(mine).unwrap().is_empty(), "nothing new to flash");
}

#[test]
fn rename_conflicts_imports_under_trial_ids_with_links_rewritten() {
    let iso = crate::test_isolation::isolate();
    let src = iso.root().join("trial.db");
    seed_v1_source(&src);
    let target_path = iso.root().join("ninox.db");
    let target = Store::open(&target_path).unwrap();
    let mut other = session("o1", None, SessionStatus::Working);
    other.claude_session_id = Some("someone-else".into());
    target.upsert_session(&other).unwrap();
    // `o1-trial` is taken too, so the next free suffix is used.
    let mut taken = session("o1-trial", None, SessionStatus::Done);
    taken.claude_session_id = Some("x".into());
    target.upsert_session(&taken).unwrap();

    let report = import(&target, &src, &scratch(&iso, "s"), &opts(false, true)).unwrap();
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    let o = report.imported.iter().find(|s| s.source_id == "o1").unwrap();
    assert_eq!(o.id, "o1-trial-2");

    let w1 = target.get_session("w1").unwrap().unwrap();
    assert_eq!(w1.orchestrator_id.as_deref(), Some("o1-trial-2"));
    assert_eq!(target.get_session("o1").unwrap().unwrap().claude_session_id.as_deref(), Some("someone-else"));
    assert_eq!(target.get_session("o1-trial-2").unwrap().unwrap().claude_session_id.as_deref(), Some("uuid-o1"));
    for (sql, n) in [
        ("SELECT COUNT(*) FROM orchestrators WHERE id = 'o1-trial-2'", 1),
        ("SELECT COUNT(*) FROM orchestrator_plans WHERE orchestrator_id = 'o1-trial-2'", 1),
        ("SELECT COUNT(*) FROM message_events WHERE from_id = 'o1-trial-2' AND to_id = 'w1'", 1),
        ("SELECT COUNT(*) FROM message_events WHERE from_id = 'w1' AND to_id = 'o1-trial-2'", 1),
        ("SELECT COUNT(*) FROM held_messages WHERE attributed_from = 'o1-trial-2'", 1),
        ("SELECT COUNT(*) FROM work_requests WHERE orchestrator_id = 'o1-trial-2'", 1),
        ("SELECT COUNT(*) FROM fleet_sessions WHERE session_id = 'o1-trial-2' AND interrupted_at IS NOT NULL", 1),
    ] {
        assert_eq!(count(&target_path, sql), n, "{sql}");
    }
}

#[test]
fn a_dry_run_reports_exactly_what_the_import_does_and_writes_nothing() {
    let iso = crate::test_isolation::isolate();
    let src = iso.root().join("trial.db");
    seed_v1_source(&src);
    let target_path = iso.root().join("ninox.db");
    let target = Store::open(&target_path).unwrap();
    target.upsert_session(&session("w2", Some("o1"), SessionStatus::Done)).unwrap();
    let total = "SELECT (SELECT COUNT(*) FROM sessions) + (SELECT COUNT(*) FROM fleet_sessions)
                 + (SELECT COUNT(*) FROM message_events) + (SELECT COUNT(*) FROM prs)";
    let rows_before = count(&target_path, total);

    let dry = import(&target, &src, &scratch(&iso, "a"), &opts(true, false)).unwrap();
    assert_eq!(count(&target_path, total), rows_before);
    assert_eq!(target.list_sessions().unwrap().len(), 1);

    let real = import(&target, &src, &scratch(&iso, "b"), &opts(false, false)).unwrap();
    assert_eq!(ImportReport { dry_run: false, ..dry }, real);
    assert_eq!(target.list_sessions().unwrap().len(), 4);
}

#[test]
fn an_error_mid_import_rolls_the_whole_import_back() {
    let iso = crate::test_isolation::isolate();
    let src = iso.root().join("trial.db");
    seed_v1_source(&src);
    let target_path = iso.root().join("ninox.db");
    let target = Store::open(&target_path).unwrap();
    // Fails after sessions, fleet facts and PRs are already inserted.
    exec(&target_path, "CREATE TRIGGER boom BEFORE INSERT ON message_events BEGIN SELECT RAISE(ABORT, 'boom'); END;");

    let err = import(&target, &src, &scratch(&iso, "s"), &opts(false, false)).unwrap_err();
    assert!(format!("{err:#}").contains("boom"), "{err:#}");
    assert!(target.list_sessions().unwrap().is_empty());
    assert_eq!(count(&target_path, "SELECT COUNT(*) FROM fleet_sessions"), 0);
    assert_eq!(count(&target_path, "SELECT COUNT(*) FROM prs"), 0);
    // The store is usable afterwards: nothing left attached or half-open.
    exec(&target_path, "DROP TRIGGER boom;");
    import(&target, &src, &scratch(&iso, "t"), &opts(false, false)).unwrap();
    assert_eq!(target.list_sessions().unwrap().len(), 4);
}

#[test]
fn a_source_from_a_newer_binary_is_refused_naming_what_is_unknown() {
    let iso = crate::test_isolation::isolate();
    for (name, sql, needle) in [
        ("col", "ALTER TABLE sessions ADD COLUMN future_field TEXT;", "column sessions.future_field"),
        ("table", "CREATE TABLE future_things (x);", "table future_things"),
        ("status", "UPDATE sessions SET status = 'hibernating' WHERE id = 'o1';", "session status \"hibernating\""),
        ("version", "PRAGMA user_version = 99;", "schema version 99"),
    ] {
        let src = iso.root().join(format!("{name}.db"));
        seed_v1_source(&src);
        exec(&src, sql);
        let target = Store::open(iso.root().join(format!("{name}-target.db"))).unwrap();
        let err = import(&target, &src, &scratch(&iso, name), &opts(false, false)).unwrap_err().to_string();
        assert!(err.contains("newer ninox") && err.contains(needle), "{name}: {err}");
        assert!(target.list_sessions().unwrap().is_empty());
    }
}

#[test]
fn the_snapshot_includes_writes_still_in_the_sources_wal() {
    let iso = crate::test_isolation::isolate();
    let src = iso.root().join("trial.db");
    let live = Store::open(&src).unwrap();
    live.upsert_session(&session("w1", None, SessionStatus::Working)).unwrap();
    assert!(src.with_extension("db-wal").metadata().unwrap().len() > 0, "precondition: un-checkpointed");
    let target = Store::open(iso.root().join("ninox.db")).unwrap();
    let report = import(&target, &src, &scratch(&iso, "s"), &opts(false, false)).unwrap();
    assert_eq!(report.imported.len(), 1);
    drop(live);
}

/// A store as v0.37.0 wrote it: pre worker types, success criteria,
/// message events, user requests and the queue.
fn seed_v0_37_source(path: &Path) {
    exec(path, include_str!("testdata/store_v0_37_0.sql"));
    exec(path, "
        PRAGMA journal_mode=WAL;
        INSERT INTO sessions (id, orchestrator_id, name, repo, status, agent_type, cost_usd, started_at,
                              workspace_path, pid, claude_session_id, summary, catalogue_path)
        VALUES ('old-o', NULL, 'orch', 'r', 'working', 'claude-code', 2.0, 50, '/ws/old-o', 11, 'c-o', 's', '/cat'),
               ('old-w', 'old-o', 'worker', 'r', 'ci_failed', 'claude-code', 1.0, 60, '/ws/old-w', 12, 'c-w', 's', NULL),
               ('old-gone', 'old-o', 'gone', 'r', 'interrupted', 'claude-code', 0, 40, '/ws/old-gone', NULL, 'c-g', NULL, NULL),
               ('old-done', NULL, 'done', 'r', 'done', 'claude-code', 0, 30, NULL, NULL, NULL, NULL, NULL);
        INSERT INTO orchestrators VALUES ('old-o', 'orch', 50);
        INSERT INTO orchestrator_plans VALUES ('old-o', '/plans/old-o.md', 1, 2);
        INSERT INTO session_messages VALUES ('old-w', 3);
        INSERT INTO session_deps VALUES ('old-w', 'old-gone', 'blocks', NULL, 3);
        INSERT INTO fleet_sessions (session_id, task_brief, branch, interrupted_at, last_status, interrupt_cause, restored_at, restore_mode)
        VALUES ('old-w', 'fix ci', 'b/old-w', NULL, NULL, NULL, NULL, NULL),
               ('old-gone', 'gone task', 'b/old-gone', 20, 'working', 'reboot', 25, 'failed');
        INSERT INTO work_requests VALUES ('wr', 'old-w', 'old-o', 'more', 5, NULL, NULL);
    ");
}

#[test]
fn a_v0_37_store_imports_into_v1_with_links_plans_and_messages() {
    let iso = crate::test_isolation::isolate();
    let src = iso.root().join("old.db");
    seed_v0_37_source(&src);
    let src_bytes = std::fs::read(&src).unwrap();
    let target_path = iso.root().join("ninox.db");
    let target = Store::open(&target_path).unwrap();
    target.upsert_session(&session("v1-existing", None, SessionStatus::Working)).unwrap();

    let report = import(&target, &src, &scratch(&iso, "s"), &opts(false, false)).unwrap();
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    assert_eq!(report.imported.len(), 4);
    assert_eq!(std::fs::read(&src).unwrap(), src_bytes, "source untouched");
    for side in ["old.db-wal", "old.db-shm"] {
        assert!(!iso.root().join(side).exists(), "{side} created beside the source");
    }
    assert_eq!(Connection::open(&src).unwrap()
        .query_row("SELECT COUNT(*) FROM pragma_table_info('sessions') WHERE name = 'worker_type'", [], |r| r.get::<_, i64>(0))
        .unwrap(), 0, "the source was never migrated in place");

    let o = target.get_session("old-o").unwrap().unwrap();
    let w = target.get_session("old-w").unwrap().unwrap();
    let gone = target.get_session("old-gone").unwrap().unwrap();
    assert_eq!((o.status.clone(), o.worker_type.as_deref()), (SessionStatus::Interrupted, Some("Orchestrator")));
    assert_eq!((w.status.clone(), w.worker_type.as_deref()), (SessionStatus::Interrupted, Some("Implementor")));
    assert_eq!(w.orchestrator_id.as_deref(), Some("old-o"));
    assert_eq!(o.brain_write.as_deref(), Some("/cat"), "0.x catalogue_path becomes the write brain");
    assert!(o.brain_reads.is_empty());
    assert_eq!(count(&target_path, "SELECT COUNT(*) FROM sessions WHERE id = 'old-o' AND brain_write = '/cat'"), 1);
    assert!(w.success_criteria.is_empty(), "pre-v1 rows default to no criteria");
    assert_eq!(gone.status, SessionStatus::Interrupted);
    assert_eq!(target.get_session("old-done").unwrap().unwrap().status, SessionStatus::Done);

    let w_rec = target.fleet_record("old-w").unwrap().unwrap();
    assert_eq!((w_rec.last_status, w_rec.task_brief.as_deref()), (Some(SessionStatus::CiFailed), Some("fix ci")));
    let gone_rec = target.fleet_record("old-gone").unwrap().unwrap();
    assert_eq!(gone_rec.interrupted_at, Some(20), "keeps when it died");
    assert_eq!(gone_rec.interrupt_cause.as_deref(), Some("reboot"));
    assert!(gone_rec.restored_at.is_none() && gone_rec.restore_mode.is_none(), "source restore attempts forgotten");
    assert!(gone_rec.awaiting_restore());

    for (sql, n) in [
        ("SELECT COUNT(*) FROM orchestrators WHERE id = 'old-o'", 1),
        ("SELECT COUNT(*) FROM orchestrator_plans WHERE orchestrator_id = 'old-o'", 1),
        ("SELECT delivered_count FROM session_messages WHERE session_id = 'old-w'", 3),
        ("SELECT COUNT(*) FROM session_deps WHERE session_id = 'old-w'", 1),
        ("SELECT COUNT(*) FROM work_requests WHERE id = 'wr'", 1),
    ] {
        assert_eq!(count(&target_path, sql), n, "{sql}");
    }
}
