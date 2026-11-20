//! Deleting a session on an explicit user request: a worker, or an
//! orchestrator with everything under it. A plan is read first so the user
//! confirms exactly what goes; executing it reuses the reap path
//! (`remove_session`, `run_cleanup`) one session at a time, children first.

use std::collections::HashSet;

use super::{is_finished, Engine, Event, SessionFields};
use crate::runtime::SessionBackend;
use crate::types::{Session, SessionId, SessionStatus};

/// What a worktree would lose. Any of these keeps it unless the user
/// overrides, the same rule `reap` applies without `--force`.
#[derive(Debug, Clone, PartialEq)]
pub struct WorktreeState {
    pub path:        String,
    pub uncommitted: bool,
    pub stash:       bool,
    pub unpushed:    bool,
}

impl WorktreeState {
    pub fn at_risk(&self) -> bool {
        self.uncommitted || self.stash || self.unpushed
    }

    pub fn describe(&self) -> String {
        let parts: Vec<&str> = [
            (self.uncommitted, "uncommitted changes"),
            (self.unpushed, "unpushed commits"),
            (self.stash, "stash"),
        ]
        .into_iter()
        .filter_map(|(on, label)| on.then_some(label))
        .collect();
        if parts.is_empty() { "clean and pushed".to_string() } else { parts.join(", ") }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeleteTarget {
    pub id:              SessionId,
    pub name:            String,
    pub is_orchestrator: bool,
    /// Has a process to kill.
    pub running:         bool,
    /// `Interrupted`: no process, but resumable until it's deleted.
    pub resumable:       bool,
    /// Levels below the plan's root.
    pub depth:           usize,
    /// Position in a parent-first walk of the tree, for showing it as one.
    pub tree_index:      usize,
    /// `None` when the session has no worktree Ninox created on disk.
    pub worktree:        Option<WorktreeState>,
}

/// Everything one delete removes, descendants before their parent and the
/// root last — the order [`Engine::execute_delete`] works in.
#[derive(Debug, Clone, PartialEq)]
pub struct DeletePlan {
    pub root:    SessionId,
    pub targets: Vec<DeleteTarget>,
}

impl DeletePlan {
    /// The targets parent-first, as the tree reads, rather than in the
    /// children-first order they're deleted in.
    pub fn tree_order(&self) -> Vec<&DeleteTarget> {
        let mut out: Vec<&DeleteTarget> = self.targets.iter().collect();
        out.sort_by_key(|t| t.tree_index);
        out
    }

    pub fn running(&self) -> usize {
        self.targets.iter().filter(|t| t.running).count()
    }

    pub fn resumable(&self) -> usize {
        self.targets.iter().filter(|t| t.resumable).count()
    }

    pub fn at_risk(&self) -> Vec<&DeleteTarget> {
        self.targets.iter().filter(|t| t.worktree.as_ref().is_some_and(WorktreeState::at_risk)).collect()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum DeleteOutcome {
    Deleted,
    /// The session ended but its worktree had work on it, so the worktree
    /// and the row stay, detached from any orchestrator that was deleted.
    KeptWorktree(String),
    Failed(String),
}

/// Why a session can't be deleted at all.
pub fn undeletable_reason(session_id: &str) -> Option<&'static str> {
    (session_id == crate::worker_type::PROMPT_SESSION_ID)
        .then_some("the Ninox built-in session can only be restarted")
}

impl Engine {
    /// What deleting `root` would remove. Reads git state for every
    /// worktree, so callers on a UI thread run it off that thread.
    pub fn plan_delete(&self, root: &str) -> anyhow::Result<DeletePlan> {
        if let Some(reason) = undeletable_reason(root) {
            anyhow::bail!("{reason}");
        }
        if self.store.get_session(root)?.is_none() && !self.store.is_orchestrator(root)? {
            anyhow::bail!("no session {root}");
        }
        let mut order = Vec::new();
        self.collect_post_order(root, 0, &mut HashSet::new(), &mut order)?;
        let targets = order
            .into_iter()
            .map(|(id, depth, tree_index)| Ok(DeleteTarget { depth, tree_index, ..self.delete_target(&id)? }))
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(DeletePlan { root: root.to_string(), targets })
    }

    /// `(id, depth, parent-first index)`, children before their parent.
    fn collect_post_order(
        &self, id: &str, depth: usize, seen: &mut HashSet<String>, out: &mut Vec<(String, usize, usize)>,
    ) -> anyhow::Result<()> {
        if !seen.insert(id.to_string()) {
            return Ok(());
        }
        let tree_index = seen.len() - 1;
        let mut children = self.store.sessions_by_orchestrator(id)?;
        children.sort_by(|a, b| a.name.cmp(&b.name));
        for child in children {
            self.collect_post_order(&child.id, depth + 1, seen, out)?;
        }
        out.push((id.to_string(), depth, tree_index));
        Ok(())
    }

    fn delete_target(&self, id: &str) -> anyhow::Result<DeleteTarget> {
        let is_orchestrator = self.store.is_orchestrator(id)?;
        let Some(session) = self.store.get_session(id)? else {
            return Ok(DeleteTarget { id: id.to_string(), name: id.to_string(), is_orchestrator, running: false, resumable: false, depth: 0, tree_index: 0, worktree: None });
        };
        // Its pane and worktree are on that machine, out of reach of the
        // local kill and cleanup, and the remote poller would mirror the
        // row straight back.
        if let Some(machine) = &session.machine_id {
            anyhow::bail!("{} runs on {machine}; delete it there", session.name);
        }
        Ok(DeleteTarget {
            id: id.to_string(),
            name: session.name.clone(),
            is_orchestrator,
            running: !is_finished(&session.status) && session.status != SessionStatus::Interrupted,
            resumable: session.status == SessionStatus::Interrupted,
            depth: 0,
            tree_index: 0,
            worktree: self.worktree_state(&session),
        })
    }

    fn worktree_state(&self, session: &Session) -> Option<WorktreeState> {
        let path = session.workspace_path.as_deref()?;
        if matches!(self.store.custom_workspace(&session.id), Ok(Some(_))) {
            return None;
        }
        crate::workspace::ninox_repo_root(path, &session.id)?;
        let dir = std::path::Path::new(path);
        if !dir.is_dir() {
            return None;
        }
        let (mut facts, _) = self.cleanup_facts(session, session.merged_at.is_some());
        facts.dirty = false;
        let unpushed = matches!(
            crate::workspace::decide(&facts, crate::workspace::CleanupPolicy { force: false }),
            crate::workspace::CleanupDecision::Skip(crate::workspace::SkipReason::UnpushedCommits),
        );
        Some(WorktreeState {
            path: path.to_string(),
            uncommitted: crate::workspace::has_uncommitted_changes(dir),
            stash: crate::workspace::has_stash_from(dir),
            unpushed,
        })
    }

    /// Deletes exactly `plan`'s sessions, in its order. `force_worktrees`
    /// is the user's override for the worktrees the plan showed with work
    /// on them, the same one `reap --force` applies; a worktree the plan
    /// showed clean is never forced, so work that lands on it while the
    /// confirm is open is kept. A session that fails, or keeps its
    /// worktree, is detached from its parent before the parent goes, so
    /// nothing is left pointing at a deleted orchestrator; a session
    /// spawned after the plan was read is detached the same way.
    pub async fn execute_delete(&self, plan: &DeletePlan, force_worktrees: bool) -> Vec<(SessionId, DeleteOutcome)> {
        let mut results = Vec::with_capacity(plan.targets.len());
        for target in &plan.targets {
            let force = force_worktrees && target.worktree.as_ref().is_some_and(WorktreeState::at_risk);
            let outcome = match self.delete_one(target, force).await {
                Ok(outcome) => outcome,
                Err(e) => DeleteOutcome::Failed(e.to_string()),
            };
            results.push((target.id.clone(), outcome));
        }
        results
    }

    async fn delete_one(&self, target: &DeleteTarget, force_worktrees: bool) -> anyhow::Result<DeleteOutcome> {
        if let Some(reason) = undeletable_reason(&target.id) {
            return Ok(DeleteOutcome::Failed(reason.to_string()));
        }
        // Before the detach below, so an orchestrator can't spawn or queue
        // a child after it.
        let _ = crate::runtime::kill_session(&target.id).await;
        let _ = self.acp.kill_session(&target.id).await;
        if target.is_orchestrator {
            self.cancel_queued_under(&target.id);
            for child in self.store.sessions_by_orchestrator(&target.id)? {
                self.store.detach_from_orchestrator(&child.id)?;
                if let Some(row) = self.store.get_session(&child.id)? {
                    self.emit(Event::SessionUpdated(row, SessionFields::ALL));
                }
            }
        }
        if let Some(row) = self.store.get_session(&target.id)? {
            self.mark_reaped_terminal(&row)?;
            self.store.clear_interruption(&target.id)?;
        }
        if let super::RemoveOutcome::Kept(reason) = self.remove_session(&target.id, force_worktrees).await? {
            return Ok(DeleteOutcome::KeptWorktree(reason));
        }
        if target.is_orchestrator {
            self.release_custom_workspace(&target.id);
            self.store.delete_orchestrator(&target.id)?;
            self.emit(Event::OrchestratorRemoved(target.id.clone()));
        }
        Ok(DeleteOutcome::Deleted)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::store::Store;
    use crate::types::{Orchestrator, SessionStatus::*};

    fn session(id: &str, parent: Option<&str>, status: SessionStatus) -> Session {
        let mut s = super::super::tests::bare_session(id, parent);
        s.status = status;
        s
    }

    fn fixture(sessions: &[Session], orchestrators: &[&str]) -> (Arc<Store>, Arc<Engine>) {
        let store = Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        for s in sessions {
            store.upsert_session(s).unwrap();
        }
        for id in orchestrators {
            store.upsert_orchestrator(&Orchestrator { id: (*id).into(), name: (*id).into(), created_at: 0 }).unwrap();
        }
        let engine = Engine::new(store.clone());
        (store, engine)
    }

    fn worktree(session_id: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        super::super::tests::repo_with_worker_worktree(session_id)
    }

    fn with_ws(mut s: Session, wt: &std::path::Path) -> Session {
        s.workspace_path = Some(wt.to_string_lossy().into_owned());
        s
    }

    #[tokio::test]
    async fn an_orchestrator_goes_after_everything_under_it() {
        let _iso = crate::test_isolation::isolate();
        let (store, engine) = fixture(
            &[
                session("root", None, Working),
                session("a-worker", Some("root"), Working),
                session("b-sub", Some("root"), Working),
                session("c-grandchild", Some("b-sub"), Done),
                session("elsewhere", None, Working),
            ],
            &["root", "b-sub"],
        );

        let plan = engine.plan_delete("root").unwrap();
        let order: Vec<&str> = plan.targets.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(order, ["a-worker", "c-grandchild", "b-sub", "root"]);
        let tree: Vec<(&str, usize)> = plan.tree_order().iter().map(|t| (t.id.as_str(), t.depth)).collect();
        assert_eq!(tree, [("root", 0), ("a-worker", 1), ("b-sub", 1), ("c-grandchild", 2)]);
        assert_eq!(plan.running(), 3, "the Done grandchild has nothing to kill");
        assert_eq!(plan.resumable(), 0);

        let results = engine.execute_delete(&plan, false).await;
        let done: Vec<&str> = results.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(done, order, "deleted in plan order");
        assert!(results.iter().all(|(_, o)| *o == DeleteOutcome::Deleted), "{results:?}");
        for id in order {
            assert!(store.get_session(id).unwrap().is_none(), "{id} must be gone");
            assert!(!store.is_orchestrator(id).unwrap(), "{id} must not stay registered");
        }
        assert!(store.get_session("elsewhere").unwrap().is_some(), "an unrelated session stays");
    }

    #[tokio::test]
    async fn a_dirty_worktree_is_kept_unless_overridden() {
        let _iso = crate::test_isolation::isolate();
        for force in [false, true] {
            let (_r1, dirty_wt) = worktree("dirty-1");
            std::fs::write(dirty_wt.join("wip.rs"), "uncommitted").unwrap();
            let (_r2, clean_wt) = worktree("clean-1");
            let (store, engine) = fixture(
                &[
                    session("orch", None, Working),
                    with_ws(session("dirty-1", Some("orch"), Working), &dirty_wt),
                    with_ws(session("clean-1", Some("orch"), Working), &clean_wt),
                ],
                &["orch"],
            );

            let plan = engine.plan_delete("orch").unwrap();
            let at_risk: Vec<&str> = plan.at_risk().iter().map(|t| t.id.as_str()).collect();
            assert_eq!(at_risk, ["dirty-1"]);
            let dirty = plan.targets.iter().find(|t| t.id == "dirty-1").unwrap();
            assert_eq!(dirty.worktree.as_ref().unwrap().describe(), "uncommitted changes");
            let clean = plan.targets.iter().find(|t| t.id == "clean-1").unwrap();
            assert_eq!(clean.worktree.as_ref().unwrap().describe(), "clean and pushed");

            let results = engine.execute_delete(&plan, force).await;
            let outcome = |id: &str| results.iter().find(|(r, _)| r == id).unwrap().1.clone();
            assert_eq!(outcome("clean-1"), DeleteOutcome::Deleted);
            assert_eq!(outcome("orch"), DeleteOutcome::Deleted);
            assert!(!clean_wt.exists());
            assert!(!store.is_orchestrator("orch").unwrap());
            if force {
                assert_eq!(outcome("dirty-1"), DeleteOutcome::Deleted);
                assert!(!dirty_wt.exists(), "the override removes it");
                assert!(store.get_session("dirty-1").unwrap().is_none());
            } else {
                assert!(matches!(outcome("dirty-1"), DeleteOutcome::KeptWorktree(_)), "{results:?}");
                assert!(dirty_wt.join("wip.rs").exists(), "the dirty worktree must survive");
                let row = store.get_session("dirty-1").unwrap().expect("its row stays with it");
                assert_eq!(row.orchestrator_id, None, "detached from the deleted orchestrator");
                assert_eq!(row.status, Terminated, "it was killed");
            }
        }
    }

    #[tokio::test]
    async fn the_override_never_forces_a_worktree_the_plan_showed_clean() {
        let _iso = crate::test_isolation::isolate();
        let (_repo, wt) = worktree("w1");
        let (store, engine) = fixture(&[with_ws(session("w1", None, Working), &wt)], &[]);
        let plan = engine.plan_delete("w1").unwrap();
        assert!(plan.at_risk().is_empty());
        std::fs::write(wt.join("late.rs"), "written while the confirm was open").unwrap();

        let results = engine.execute_delete(&plan, true).await;

        assert!(matches!(results[0].1, DeleteOutcome::KeptWorktree(_)), "{results:?}");
        assert!(wt.join("late.rs").exists());
        assert!(store.get_session("w1").unwrap().is_some());
    }

    #[test]
    fn an_interrupted_session_is_planned_as_resumable_not_running() {
        let _iso = crate::test_isolation::isolate();
        let (_, engine) = fixture(&[session("w1", None, Interrupted)], &[]);
        let plan = engine.plan_delete("w1").unwrap();
        assert_eq!((plan.running(), plan.resumable()), (0, 1));
    }

    #[test]
    fn a_tree_with_a_remote_session_is_refused() {
        let _iso = crate::test_isolation::isolate();
        let mut remote = session("r1", Some("orch"), Working);
        remote.machine_id = Some("mac-studio".into());
        let (_, engine) = fixture(&[session("orch", None, Working), remote], &["orch"]);
        let err = engine.plan_delete("orch").unwrap_err().to_string();
        assert!(err.contains("mac-studio"), "{err}");
    }

    #[tokio::test]
    async fn a_session_spawned_after_the_plan_is_detached_not_deleted() {
        let _iso = crate::test_isolation::isolate();
        let (store, engine) = fixture(&[session("orch", None, Working), session("w1", Some("orch"), Working)], &["orch"]);
        let plan = engine.plan_delete("orch").unwrap();
        store.upsert_session(&session("late", Some("orch"), Working)).unwrap();

        engine.execute_delete(&plan, false).await;

        let late = store.get_session("late").unwrap().expect("not in the plan the user confirmed");
        assert_eq!(late.orchestrator_id, None);
        assert!(store.get_session("w1").unwrap().is_none());
    }

    /// Kill (the GUI session view's button) is `terminate_session`: it must
    /// never take the worktree or the row with it. Only a delete does that.
    #[tokio::test]
    async fn kill_leaves_a_dirty_worktree_and_its_row() {
        let _iso = crate::test_isolation::isolate();
        let (_repo, wt) = worktree("dirty-1");
        std::fs::write(wt.join("wip.rs"), "uncommitted").unwrap();
        let (store, engine) = fixture(&[with_ws(session("dirty-1", None, Working), &wt)], &[]);

        engine.terminate_session("dirty-1").await.unwrap();

        assert!(wt.join("wip.rs").exists(), "the dirty worktree must survive a kill");
        let row = store.get_session("dirty-1").unwrap().expect("the row must survive a kill");
        assert_eq!(row.status, Terminated);
    }

    #[test]
    fn the_built_in_session_cannot_be_deleted() {
        let _iso = crate::test_isolation::isolate();
        let id = crate::worker_type::PROMPT_SESSION_ID;
        let (_, engine) = fixture(&[session(id, None, Working)], &[]);
        assert!(engine.plan_delete(id).is_err());
    }
}
