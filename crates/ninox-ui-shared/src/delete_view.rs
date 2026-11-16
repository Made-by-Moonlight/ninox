//! The delete confirm's wording, shared so the GUI's modal and the TUI's
//! say the same thing about the same `DeletePlan`.

use ninox_core::events::{DeleteOutcome, DeletePlan, DeleteTarget};

pub fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

pub fn root_target(plan: &DeletePlan) -> Option<&DeleteTarget> {
    plan.targets.iter().find(|t| t.id == plan.root)
}

/// The sessions under the root, parent-first as the tree reads.
pub fn under_root(plan: &DeletePlan) -> Vec<&DeleteTarget> {
    plan.tree_order().into_iter().filter(|t| t.id != plan.root).collect()
}

pub fn title(plan: &DeletePlan) -> String {
    let Some(root) = root_target(plan) else { return "Delete session".to_string() };
    let under = plan.targets.len() - 1;
    match (root.is_orchestrator, under) {
        (true, 0) => format!("Delete orchestrator {}", root.name),
        (true, n) => format!("Delete orchestrator {} and {} under it", root.name, plural(n, "session", "sessions")),
        (false, _) => format!("Delete worker {}", root.name),
    }
}

/// The same wording `ninox reap --force` uses for a running session, and
/// what an interrupted one gives up.
pub fn kill_warning(plan: &DeletePlan) -> Option<String> {
    let kills = match plan.running() {
        0 => None,
        n => Some(format!("This kills {}.", plural(n, "running session", "running sessions"))),
    };
    let resumes = match plan.resumable() {
        0 => None,
        n => Some(format!("{} can no longer be resumed.", plural(n, "interrupted session", "interrupted sessions"))),
    };
    match (kills, resumes) {
        (None, None) => None,
        (a, b) => Some([a, b].into_iter().flatten().collect::<Vec<_>>().join(" ")),
    }
}

pub fn override_label(plan: &DeletePlan) -> Option<String> {
    override_label_naming(plan, usize::MAX)
}

/// [`override_label`] naming only the first `shown` worktrees, then how
/// many more, for a screen too small to list them all.
pub fn override_label_naming(plan: &DeletePlan, shown: usize) -> Option<String> {
    let at_risk = plan.at_risk();
    (!at_risk.is_empty()).then(|| {
        let mut names: Vec<String> = at_risk.iter().take(shown).map(|t| t.name.clone()).collect();
        if at_risk.len() > names.len() {
            names.push(format!("and {} more", at_risk.len() - names.len()));
        }
        format!(
            "Also delete {} with uncommitted or unpushed work: {}",
            plural(at_risk.len(), "worktree", "worktrees"),
            names.join(", "),
        )
    })
}

/// How a target's worktree is treated, and whether that's the at-risk case
/// the user should notice: `(text, at_risk)`.
pub fn target_detail(t: &DeleteTarget, force: bool) -> (String, bool) {
    let pane = if t.running { "running pane" } else if t.resumable { "interrupted, resumable" } else { "pane" };
    let mut parts = vec![format!("session, {pane}")];
    let at_risk = t.worktree.as_ref().is_some_and(|wt| wt.at_risk());
    if let Some(wt) = &t.worktree {
        parts.push(match (at_risk, force) {
            (true, false) => format!("worktree kept: {}", wt.describe()),
            _ => format!("worktree: {}", wt.describe()),
        });
    }
    (parts.join(" · "), at_risk)
}

pub fn target_name(t: &DeleteTarget) -> String {
    format!("{} ({})", t.name, if t.is_orchestrator { "orchestrator" } else { "worker" })
}

/// One line per kept or failed session after the deleted count, and
/// whether anything failed.
pub fn outcome_summary(results: &[(String, DeleteOutcome)]) -> (String, bool) {
    let deleted = results.iter().filter(|(_, o)| *o == DeleteOutcome::Deleted).count();
    let mut lines = vec![format!("Deleted {}.", plural(deleted, "session", "sessions"))];
    for (id, outcome) in results {
        match outcome {
            DeleteOutcome::Deleted => {}
            DeleteOutcome::KeptWorktree(why) => lines.push(format!("Kept {id} and its worktree ({why}).")),
            DeleteOutcome::Failed(e) => lines.push(format!("Failed {id}: {e}")),
        }
    }
    let failed = results.iter().any(|(_, o)| matches!(o, DeleteOutcome::Failed(_)));
    (lines.join("\n"), failed)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ninox_core::events::WorktreeState;

    pub(crate) fn target(id: &str, orch: bool, running: bool, wt: Option<(bool, bool)>) -> DeleteTarget {
        DeleteTarget {
            id: id.into(),
            name: format!("{id}-name"),
            is_orchestrator: orch,
            running,
            resumable: false,
            depth: if orch { 0 } else { 1 },
            tree_index: 0,
            worktree: wt.map(|(uncommitted, unpushed)| WorktreeState { path: format!("/r/.claude/worktrees/{id}"), uncommitted, stash: false, unpushed }),
        }
    }

    fn orch_plan() -> DeletePlan {
        DeletePlan {
            root: "o".into(),
            targets: vec![
                target("a", false, true, Some((true, false))),
                target("b", false, false, Some((false, false))),
                target("c", false, true, Some((false, true))),
                target("o", true, true, None),
            ],
        }
    }

    #[test]
    fn the_wording_names_the_tree_the_kills_and_the_risky_worktrees() {
        let plan = orch_plan();
        assert_eq!(title(&plan), "Delete orchestrator o-name and 3 sessions under it");
        assert_eq!(kill_warning(&plan).as_deref(), Some("This kills 3 running sessions."));
        assert_eq!(
            override_label(&plan).as_deref(),
            Some("Also delete 2 worktrees with uncommitted or unpushed work: a-name, c-name"),
        );
    }

    #[test]
    fn the_override_can_name_just_the_first_few() {
        assert_eq!(
            override_label_naming(&orch_plan(), 1).as_deref(),
            Some("Also delete 2 worktrees with uncommitted or unpushed work: a-name, and 1 more"),
        );
    }

    #[test]
    fn a_clean_finished_worker_has_no_warning_or_override() {
        let plan = DeletePlan { root: "w".into(), targets: vec![target("w", false, false, Some((false, false)))] };
        assert_eq!(title(&plan), "Delete worker w-name");
        assert_eq!(kill_warning(&plan), None);
        assert_eq!(override_label(&plan), None);
    }

    #[test]
    fn an_interrupted_session_warns_that_it_loses_its_resume() {
        let mut t = target("w", false, false, None);
        t.resumable = true;
        let plan = DeletePlan { root: "w".into(), targets: vec![t] };
        assert_eq!(kill_warning(&plan).as_deref(), Some("1 interrupted session can no longer be resumed."));
    }

    #[test]
    fn a_risky_worktree_reads_kept_until_the_override_is_on() {
        let t = target("a", false, true, Some((true, false)));
        assert_eq!(target_detail(&t, false), ("session, running pane · worktree kept: uncommitted changes".to_string(), true));
        assert_eq!(target_detail(&t, true), ("session, running pane · worktree: uncommitted changes".to_string(), true));
    }

    #[test]
    fn the_summary_names_what_was_kept_and_what_failed() {
        let (text, failed) = outcome_summary(&[
            ("a".into(), DeleteOutcome::KeptWorktree("skipped: dirty".into())),
            ("b".into(), DeleteOutcome::Deleted),
            ("o".into(), DeleteOutcome::Failed("store locked".into())),
        ]);
        assert!(failed);
        assert_eq!(text, "Deleted 1 session.\nKept a and its worktree (skipped: dirty).\nFailed o: store locked");
    }
}
