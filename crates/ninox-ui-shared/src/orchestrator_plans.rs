//! Registered orchestrator plan docs (`ninox plan register`) as the Plans
//! tab lists them: one entry per orchestrator, newest edit first, with
//! whether the orchestrator is still live and whether its file is readable.

use std::io::ErrorKind;
use std::time::UNIX_EPOCH;

use ninox_core::store::Store;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileHealth {
    Ok,
    Missing,
    Unreadable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanEntry {
    pub orchestrator_id: String,
    pub name: String,
    pub file_path: String,
    /// The file's mtime in unix millis; the registration row's `updated_at`
    /// when the file can't be read.
    pub updated_at: i64,
    pub file: FileHealth,
    pub live: bool,
}

/// Every registered plan, most recently updated first. An orchestrator is
/// live while its own `sessions` row is non-terminal — the same signal the
/// Fleet board uses — and ended when that row is terminal or gone.
pub fn load(store: &Store) -> Vec<PlanEntry> {
    let names: std::collections::HashMap<String, String> =
        store.list_orchestrators().unwrap_or_default().into_iter().map(|o| (o.id, o.name)).collect();
    let mut entries: Vec<PlanEntry> = store
        .list_orchestrator_plans()
        .unwrap_or_default()
        .into_iter()
        .map(|reg| {
            let (file, mtime) = probe(&reg.file_path);
            let live = store.get_session(&reg.orchestrator_id).ok().flatten().is_some_and(|s| !s.status.is_terminal());
            PlanEntry {
                name: names.get(&reg.orchestrator_id).cloned().unwrap_or_else(|| reg.orchestrator_id.clone()),
                orchestrator_id: reg.orchestrator_id,
                file_path: reg.file_path,
                updated_at: mtime.unwrap_or(reg.updated_at),
                file,
                live,
            }
        })
        .collect();
    entries.sort_by_key(|e| std::cmp::Reverse(e.updated_at));
    entries
}

/// The entries to list: ended orchestrators only when `show_ended`.
pub fn visible(entries: &[PlanEntry], show_ended: bool) -> Vec<&PlanEntry> {
    entries.iter().filter(|e| show_ended || e.live).collect()
}

/// The plan doc's last-updated time in unix millis: the file's mtime, else
/// the registration row's `updated_at`. `None` when neither is a real time.
pub fn updated_ms(path: &str, row_updated_at: i64) -> Option<i64> {
    let (_, mtime) = probe(path);
    mtime.or(Some(row_updated_at)).filter(|ms| *ms > 0)
}

fn probe(path: &str) -> (FileHealth, Option<i64>) {
    match std::fs::File::open(path).and_then(|f| f.metadata()) {
        Ok(meta) => {
            let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_millis() as i64);
            (FileHealth::Ok, mtime)
        }
        Err(e) if e.kind() == ErrorKind::NotFound => (FileHealth::Missing, None),
        Err(_) => (FileHealth::Unreadable, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::types::{Orchestrator, Session, SessionStatus};

    fn store() -> Store {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        std::mem::forget(dir);
        Store::open(path).unwrap()
    }

    fn session(id: &str) -> Session {
        use ninox_core::types::ActivityState;
        Session {
            id: id.into(), orchestrator_id: None, name: "w".into(),
            repo: "r1".into(), status: SessionStatus::Working,
            agent_type: "claude-code".into(), cost_usd: 1.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: Some("/ws".into()),
            pid: Some(111), model: Some("m1".into()), context_tokens: Some(10),
            brain_reads: Vec::new(), brain_write: None, context_used_pct: Some(1.0),
            context_total_tokens: Some(10), context_window_size: Some(200_000),
            claude_session_id: None, summary: None, terminal_at: None,
            gate_status: None, merged_at: None,
            activity: ActivityState::Unknown,
            activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        }
    }

    fn orch(store: &Store, id: &str, status: SessionStatus) {
        store.upsert_orchestrator(&Orchestrator { id: id.into(), name: format!("{id}-name"), created_at: 0 }).unwrap();
        let mut s = session(id);
        s.status = status;
        store.upsert_session(&s).unwrap();
    }

    #[test]
    fn lists_newest_first_and_hides_ended_by_default() {
        let _iso = ninox_core::test_isolation::isolate();
        let store = store();
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old.md");
        let new = dir.path().join("new.md");
        std::fs::write(&old, "# old").unwrap();
        std::fs::write(&new, "# new").unwrap();
        let t = std::fs::File::options().write(true).open(&old).unwrap();
        t.set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000)).unwrap();
        orch(&store, "a", SessionStatus::Working);
        orch(&store, "b", SessionStatus::Working);
        orch(&store, "c", SessionStatus::Terminated);
        store.register_orchestrator_plan("a", old.to_str().unwrap(), 1).unwrap();
        store.register_orchestrator_plan("b", new.to_str().unwrap(), 2).unwrap();
        store.register_orchestrator_plan("c", new.to_str().unwrap(), 3).unwrap();

        let all = load(&store);
        assert_eq!(all.len(), 3);
        assert_eq!(all.last().unwrap().orchestrator_id, "a");
        let ids: Vec<_> = visible(&all, false).iter().map(|e| e.orchestrator_id.as_str()).collect();
        assert_eq!(ids.len(), 2);
        assert!(!ids.contains(&"c"));
        assert_eq!(visible(&all, true).len(), 3);
        assert!(all.iter().find(|e| e.orchestrator_id == "c").is_some_and(|e| !e.live));
        assert!(all[0].name.ends_with("-name"));
    }

    #[test]
    fn missing_file_is_flagged_and_falls_back_to_the_row_timestamp() {
        let _iso = ninox_core::test_isolation::isolate();
        let store = store();
        orch(&store, "a", SessionStatus::Working);
        store.register_orchestrator_plan("a", "/definitely/not/here.md", 42).unwrap();
        let e = &load(&store)[0];
        assert_eq!(e.file, FileHealth::Missing);
        assert_eq!(e.updated_at, 42);
        assert!(e.live);
    }

    #[test]
    fn updated_ms_prefers_mtime_and_never_returns_epoch_zero() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("p.md");
        std::fs::write(&f, "x").unwrap();
        assert!(updated_ms(f.to_str().unwrap(), 0).is_some_and(|ms| ms > 1_000_000));
        assert_eq!(updated_ms("/definitely/not/here.md", 42), Some(42));
        assert_eq!(updated_ms("/definitely/not/here.md", 0), None);
    }

    #[test]
    fn orchestrator_without_a_session_row_is_ended() {
        let _iso = ninox_core::test_isolation::isolate();
        let store = store();
        store.upsert_orchestrator(&Orchestrator { id: "a".into(), name: "a".into(), created_at: 0 }).unwrap();
        store.register_orchestrator_plan("a", "/x.md", 1).unwrap();
        assert!(!load(&store)[0].live);
    }
}
