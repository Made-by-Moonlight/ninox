use std::collections::{BTreeMap, BTreeSet};

/// The per-path actions a sync must take, computed by [`plan_sync`].
/// `pulls`/`delete_local` are safe on the read path (`pull_if_stale`):
/// they only touch files whose local content still matches base.
/// `resurrect_pulls`/`conflicts` overwrite local divergence and run only
/// in a full `sync()` (spec §3).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SyncPlan {
    pub pulls: Vec<String>,
    pub resurrect_pulls: Vec<String>,
    pub pushes: Vec<String>,
    pub delete_local: Vec<String>,
    pub delete_remote: Vec<String>,
    pub conflicts: Vec<String>,
    pub base_updates: Vec<String>,
}

/// Three-way diff of content hashes: `base` (last agreement with the
/// remote), `local` (files on disk), `remote` (manifest). Pure function —
/// all I/O happens in the engine that applies the plan.
pub fn plan_sync(
    base: &BTreeMap<String, String>,
    local: &BTreeMap<String, String>,
    remote: &BTreeMap<String, String>,
) -> SyncPlan {
    let mut plan = SyncPlan::default();
    let paths: BTreeSet<&String> = base.keys().chain(local.keys()).chain(remote.keys()).collect();
    for path in paths {
        let b = base.get(path);
        let l = local.get(path);
        let r = remote.get(path);
        if l == r {
            // Content agrees (or both absent); only the base may be stale.
            if b != l {
                plan.base_updates.push(path.clone());
            }
        } else if l == b {
            // Local untouched since last sync; remote moved.
            match r {
                Some(_) => plan.pulls.push(path.clone()),
                None => plan.delete_local.push(path.clone()),
            }
        } else if r == b {
            // Remote untouched since last sync; local moved.
            match l {
                Some(_) => plan.pushes.push(path.clone()),
                None => plan.delete_remote.push(path.clone()),
            }
        } else {
            // Both sides diverged from base.
            match (l, r) {
                (None, Some(_)) => plan.resurrect_pulls.push(path.clone()),
                (Some(_), None) => plan.pushes.push(path.clone()), // edit beats delete
                (Some(_), Some(_)) => plan.conflicts.push(path.clone()),
                (None, None) => unreachable!("l == r handled above"),
            }
        }
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn all_equal_is_a_noop() {
        let m = map(&[("a.md", "h1")]);
        assert_eq!(plan_sync(&m, &m, &m), SyncPlan::default());
    }

    #[test]
    fn remote_changed_local_untouched_pulls() {
        let plan = plan_sync(&map(&[("a.md", "h1")]), &map(&[("a.md", "h1")]), &map(&[("a.md", "h2")]));
        assert_eq!(plan.pulls, vec!["a.md"]);
        assert_eq!(plan.pushes, Vec::<String>::new());
    }

    #[test]
    fn local_changed_remote_untouched_pushes() {
        let plan = plan_sync(&map(&[("a.md", "h1")]), &map(&[("a.md", "h2")]), &map(&[("a.md", "h1")]));
        assert_eq!(plan.pushes, vec!["a.md"]);
    }

    #[test]
    fn both_changed_same_content_updates_base_only() {
        let plan = plan_sync(&map(&[("a.md", "h1")]), &map(&[("a.md", "h2")]), &map(&[("a.md", "h2")]));
        assert_eq!(plan.base_updates, vec!["a.md"]);
        assert!(plan.pulls.is_empty() && plan.pushes.is_empty() && plan.conflicts.is_empty());
    }

    #[test]
    fn both_changed_differently_conflicts() {
        let plan = plan_sync(&map(&[("a.md", "h1")]), &map(&[("a.md", "h2")]), &map(&[("a.md", "h3")]));
        assert_eq!(plan.conflicts, vec!["a.md"]);
    }

    #[test]
    fn local_delete_remote_untouched_deletes_remote() {
        let plan = plan_sync(&map(&[("a.md", "h1")]), &map(&[]), &map(&[("a.md", "h1")]));
        assert_eq!(plan.delete_remote, vec!["a.md"]);
    }

    #[test]
    fn remote_delete_local_untouched_deletes_local() {
        let plan = plan_sync(&map(&[("a.md", "h1")]), &map(&[("a.md", "h1")]), &map(&[]));
        assert_eq!(plan.delete_local, vec!["a.md"]);
    }

    #[test]
    fn local_delete_vs_remote_edit_resurrects() {
        let plan = plan_sync(&map(&[("a.md", "h1")]), &map(&[]), &map(&[("a.md", "h2")]));
        assert_eq!(plan.resurrect_pulls, vec!["a.md"]);
        assert!(plan.pulls.is_empty(), "resurrection must not run on the read path");
    }

    #[test]
    fn remote_delete_vs_local_edit_pushes_the_edit() {
        let plan = plan_sync(&map(&[("a.md", "h1")]), &map(&[("a.md", "h2")]), &map(&[]));
        assert_eq!(plan.pushes, vec!["a.md"]);
    }

    #[test]
    fn brand_new_local_pushes() {
        let plan = plan_sync(&map(&[]), &map(&[("a.md", "h1")]), &map(&[]));
        assert_eq!(plan.pushes, vec!["a.md"]);
    }

    #[test]
    fn brand_new_remote_pulls() {
        let plan = plan_sync(&map(&[]), &map(&[]), &map(&[("a.md", "h1")]));
        assert_eq!(plan.pulls, vec!["a.md"]);
    }

    #[test]
    fn new_on_both_sides_same_content_updates_base() {
        let plan = plan_sync(&map(&[]), &map(&[("a.md", "h1")]), &map(&[("a.md", "h1")]));
        assert_eq!(plan.base_updates, vec!["a.md"]);
    }

    #[test]
    fn new_on_both_sides_different_content_conflicts() {
        let plan = plan_sync(&map(&[]), &map(&[("a.md", "h1")]), &map(&[("a.md", "h2")]));
        assert_eq!(plan.conflicts, vec!["a.md"]);
    }

    #[test]
    fn deleted_everywhere_updates_base() {
        // In base, gone from both local and remote: only the stale base
        // record remains to clean up.
        let plan = plan_sync(&map(&[("a.md", "h1")]), &map(&[]), &map(&[]));
        assert_eq!(plan.base_updates, vec!["a.md"]);
    }

    #[test]
    fn independent_paths_get_independent_actions() {
        let plan = plan_sync(
            &map(&[("pull.md", "h1"), ("push.md", "h1")]),
            &map(&[("pull.md", "h1"), ("push.md", "h2")]),
            &map(&[("pull.md", "h9"), ("push.md", "h1")]),
        );
        assert_eq!(plan.pulls, vec!["pull.md"]);
        assert_eq!(plan.pushes, vec!["push.md"]);
    }

    /// The spec's scale guarantee: the manifest diff must stay fast at the
    /// brain sizes ninox already tests for (500 entries) — generous ceiling
    /// to catch a catastrophic regression, not to pin performance.
    #[test]
    fn plan_sync_scales_to_500_paths_within_ceiling() {
        let mut base = BTreeMap::new();
        let mut local = BTreeMap::new();
        let mut remote = BTreeMap::new();
        for i in 0..500 {
            base.insert(format!("notes/note{i}.md"), format!("h{i}"));
            // A third each: unchanged, locally edited, remotely edited.
            local.insert(format!("notes/note{i}.md"), if i % 3 == 1 { format!("l{i}") } else { format!("h{i}") });
            remote.insert(format!("notes/note{i}.md"), if i % 3 == 2 { format!("r{i}") } else { format!("h{i}") });
        }
        let start = std::time::Instant::now();
        let plan = plan_sync(&base, &local, &remote);
        let elapsed = start.elapsed();
        assert_eq!(plan.pushes.len() + plan.pulls.len(), 333);
        assert!(elapsed.as_millis() < 500, "diff of 500 paths took too long: {elapsed:?}");
    }
}
